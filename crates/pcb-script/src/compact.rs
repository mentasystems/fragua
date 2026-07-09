//! Board compaction — feasibility-gated outline shrink.
//!
//! Given a routed, DRC-clean board, `compact` searches for the smallest
//! rectangular outline that still lets the placer + router produce a
//! layout with **0 failed nets and 0 DRC errors**. The search never
//! trusts a candidate size on geometry alone: every candidate is proven
//! by cloning the board, re-placing every footprint into the smaller
//! outline, re-routing, and re-running DRC with the exact options the
//! `route` / `drc` verbs use. A size is "feasible" only when that whole
//! pipeline comes back clean, so the result is always manufacturable.
//!
//! Two search phases (both share one feasibility oracle):
//!   1. Binary-search a uniform scale factor `s ∈ [s_min, 1]` applied to
//!      both dimensions (aspect = keep). Converges to 0.5 mm.
//!   2. Greedy per-dimension shrink from the binary-search result:
//!      repeatedly try `W-step` / `H-step` while feasible. This is the
//!      whole of aspect = free, and a cheap refinement for aspect = keep.
//!
//! The core takes a `&Board` (+ a schematic `Arc` and margin maps) and
//! returns a `CompactOutcome` with the best routed board and metrics, so
//! the `compact` verb and the headless `examples/compact.rs` binary share
//! one implementation.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use pcb_core::{Board, Length, PlacementMargin, Point, Rect, Schematic, SilkText};
use pcb_placer::{place, MarginMap, PlaceOptions};
use pcb_router::{Outcome, RouteOptions};

/// Tunables for a compaction run. Defaults are calibrated for a ~15
/// footprint board finishing within a couple of minutes.
#[derive(Debug, Clone)]
pub struct CompactOptions {
    /// Lower bound on the compacted width (mm). `None` = derive from
    /// component geometry only.
    pub min_w_mm: Option<f64>,
    /// Lower bound on the compacted height (mm).
    pub min_h_mm: Option<f64>,
    /// Greedy per-dimension shrink step (mm).
    pub step_mm: f64,
    /// PRNG seed for the placer. Per-iteration seeds are derived from
    /// this deterministically, so a fixed seed → a fixed result.
    pub seed: u64,
    /// Placer iterations per feasibility check.
    pub place_iters: usize,
    /// `true` = shrink each dimension independently (aspect = free);
    /// `false` = keep aspect ratio in the binary-search phase, then
    /// still run a bounded greedy refinement.
    pub aspect_free: bool,
    /// Base soft body-to-body gap (mm) at full size. Scaled down as the
    /// outline shrinks, but never below the placer's hard 0.5 mm
    /// clearance. `None` = use the placer default (2.0 mm).
    pub min_gap_mm: Option<f64>,
    /// Binary-search iterations in phase 1.
    pub binary_steps: usize,
    /// Hard cap on total feasibility checks (placer+router+DRC runs)
    /// across both phases, so a pathological board can't run forever.
    pub max_checks: usize,
    /// Wall-clock budget. When exceeded the search stops and returns the
    /// best feasible result found so far.
    pub time_budget: Duration,
    /// Board-edge copper clearance (mm) folded into the per-dimension
    /// lower bound. Matches the DRC `edge_clearance` default.
    pub edge_clearance_mm: f64,
    /// Packing allowance on the summed component area used as an
    /// absolute area floor: `area_min = packing_factor * Σ component
    /// area`. > 1 leaves room for routing channels and imperfect packing.
    pub packing_factor: f64,
}

impl Default for CompactOptions {
    // `from_secs` reads fine here; `from_mins` is not on our MSRV.
    #[allow(clippy::duration_suboptimal_units)]
    fn default() -> Self {
        Self {
            min_w_mm: None,
            min_h_mm: None,
            step_mm: 1.0,
            seed: 1,
            place_iters: 8000,
            aspect_free: false,
            min_gap_mm: None,
            binary_steps: 7,
            max_checks: 40,
            time_budget: Duration::from_secs(240),
            edge_clearance_mm: 0.3,
            packing_factor: 1.3,
        }
    }
}

/// Metrics describing what a compaction run achieved.
#[derive(Debug, Clone)]
pub struct CompactMetrics {
    pub old_w_mm: f64,
    pub old_h_mm: f64,
    pub old_area_mm2: f64,
    pub new_w_mm: f64,
    pub new_h_mm: f64,
    pub new_area_mm2: f64,
    /// Percentage area reduction: `(old - new) / old * 100`.
    pub area_reduction_pct: f64,
    pub trace_count: usize,
    pub via_count: usize,
    pub total_length_mm: f64,
    /// Always 0 on a successful shrink (a size with any failed net is
    /// never accepted); carried for the report.
    pub failed_nets: usize,
    /// Always 0 on a successful shrink.
    pub drc_errors: usize,
    /// Per-dimension geometric lower bound the search was clamped to.
    pub lower_bound_w_mm: f64,
    pub lower_bound_h_mm: f64,
    /// How many feasibility checks (full placer+router+DRC runs) ran.
    pub checks: usize,
}

/// Result of a compaction run.
#[derive(Debug, Clone)]
pub struct CompactOutcome {
    /// The best feasible board (outline shrunk, re-placed, re-routed).
    /// When `shrunk == false` this is a clone of the input, untouched.
    pub board: Board,
    pub metrics: CompactMetrics,
    /// `true` when a smaller feasible outline was found and applied to
    /// `board`; `false` when no shrink was feasible (board untouched).
    pub shrunk: bool,
}

/// A single feasible candidate: the fully re-placed, re-routed board and
/// its headline metrics.
struct Feasible {
    board: Board,
    trace_count: usize,
    via_count: usize,
    total_length_mm: f64,
}

/// Geometric lower bound on the outline. Returns `(w_min, h_min,
/// area_min)` in mm / mm².
///
/// Per-dimension floor: a component always fits if the board's shorter
/// side clears the component's shorter side (it can be rotated), so the
/// floor is `max over parts of min(width, height)` plus twice the edge
/// clearance. `min_w_mm` / `min_h_mm` raise the floor further.
///
/// Area floor: `packing_factor * Σ (width · height)` over every
/// component's inflated (margin-folded) bbox — a hard minimum below
/// which no packing can fit the copper.
#[must_use]
pub fn lower_bound_outline(
    board: &Board,
    margins: &MarginMap,
    opts: &CompactOptions,
) -> (f64, f64, f64) {
    let mut max_min_side = 0.0_f64;
    let mut sum_area = 0.0_f64;
    for fp in board.footprints_in_order() {
        let Some(bb) = inflated_bounds(fp, margins) else {
            continue;
        };
        let w = bb.width().to_mm();
        let h = bb.height().to_mm();
        max_min_side = max_min_side.max(w.min(h));
        sum_area += w * h;
    }
    let dim_floor = max_min_side + 2.0 * opts.edge_clearance_mm;
    let w_min = dim_floor.max(opts.min_w_mm.unwrap_or(0.0));
    let h_min = dim_floor.max(opts.min_h_mm.unwrap_or(0.0));
    let area_min = opts.packing_factor * sum_area;
    (w_min, h_min, area_min)
}

/// World-frame bbox of a footprint inflated by its placement margin (if
/// any). Mirrors the placer's `fp_bounds_with_margin`.
fn inflated_bounds(fp: &pcb_core::Footprint, margins: &MarginMap) -> Option<Rect> {
    let base = fp.bounds()?;
    let Some(local) = margins.get(&fp.id) else {
        return Some(base);
    };
    if local.iter().all(|v| *v <= 0.0) {
        return Some(base);
    }
    let world = pcb_core::rotate_margin_trbl(*local, fp.rotation);
    let [t, r, b, l] = world;
    Some(Rect {
        min: Point::new(
            base.min.x - Length::from_mm(l),
            base.min.y - Length::from_mm(b),
        ),
        max: Point::new(
            base.max.x + Length::from_mm(r),
            base.max.y + Length::from_mm(t),
        ),
    })
}

/// Run board compaction. Returns the best feasible (or untouched) board
/// plus metrics. Errors only when the board has no outline to shrink.
// `drc_margins` always uses the default hasher at every call site; a
// generic `BuildHasher` param would only add noise.
#[allow(clippy::implicit_hasher)]
pub fn compact(
    base: &Board,
    schematic: &Arc<Schematic>,
    place_margins: &MarginMap,
    drc_margins: &HashMap<String, PlacementMargin>,
    fab_profile: Option<&pcb_drc::FabProfile>,
    opts: &CompactOptions,
) -> Result<CompactOutcome, String> {
    let outline = base
        .outline
        .ok_or_else(|| "compact needs a board outline; set one with `outline W H`".to_string())?;
    let base_min = outline.min;
    let w_cur = outline.width().to_mm();
    let h_cur = outline.height().to_mm();
    let old_area = w_cur * h_cur;

    let (w_min, h_min, area_min) = lower_bound_outline(base, place_margins, opts);

    // Baseline metrics, used for the "no shrink" path.
    let untouched_metrics = |checks: usize| CompactMetrics {
        old_w_mm: w_cur,
        old_h_mm: h_cur,
        old_area_mm2: old_area,
        new_w_mm: w_cur,
        new_h_mm: h_cur,
        new_area_mm2: old_area,
        area_reduction_pct: 0.0,
        trace_count: base.traces.len(),
        via_count: base.vias.len(),
        total_length_mm: 0.0,
        failed_nets: 0,
        drc_errors: 0,
        lower_bound_w_mm: w_min,
        lower_bound_h_mm: h_min,
        checks,
    };

    // Scale lower bound: both dims scale by `s`, so `s` must clear every
    // per-dimension floor and the area floor.
    let s_min = (w_min / w_cur)
        .max(h_min / h_cur)
        .max((area_min / old_area).sqrt())
        .clamp(0.0, 1.0);
    // Already at (or below) the geometric floor — nothing to gain.
    if s_min >= 1.0 - 1e-6 {
        return Ok(CompactOutcome {
            board: base.clone(),
            metrics: untouched_metrics(0),
            shrunk: false,
        });
    }

    let route_opts = RouteOptions {
        cell: Length::from_mm(0.20),
        trace_width: Length::from_mm(0.25),
        clearance: Length::from_mm(0.20),
        via_cost: 8,
        via_drill: Length::from_mm(0.30),
        via_diameter: Length::from_mm(0.60),
        net_overrides: HashMap::new(),
        schematic: Some(schematic.clone()),
        initial_net_order: None,
        heuristic_weight: 1.0,
    };
    let base_min_gap = opts
        .min_gap_mm
        .unwrap_or_else(|| PlaceOptions::default().min_gap_mm);

    let base_radius = base.outline_corner_radius;
    let start = Instant::now();
    let mut checks = 0usize;
    let mut best: Option<(f64, f64, Feasible)> = None;

    // One feasibility check: prove a W×H outline routes + passes DRC.
    let feasible = |w_mm: f64, h_mm: f64, checks: &mut usize| -> Option<Feasible> {
        if *checks >= opts.max_checks || start.elapsed() >= opts.time_budget {
            return None;
        }
        *checks += 1;
        let seed = derive_seed(opts.seed, *checks);
        try_feasible(
            base,
            base_min,
            w_mm,
            h_mm,
            base_radius,
            seed,
            base_min_gap,
            old_area,
            opts,
            place_margins,
            &route_opts,
            drc_margins,
            fab_profile,
            schematic,
        )
    };

    // ── Phase 1: binary search a uniform scale factor. ──
    // Invariant: `hi` brackets a feasible-or-larger scale, `lo` a
    // (presumed) infeasible one. When a mid is feasible we record it and
    // pull `hi` down (try smaller); otherwise we push `lo` up.
    let mut lo = s_min;
    let mut hi = 1.0_f64;
    let bigger_dim = w_cur.max(h_cur);
    for _ in 0..opts.binary_steps {
        if (hi - lo) * bigger_dim < 0.5 {
            break;
        }
        let mid = 0.5 * (lo + hi);
        let (w, h) = (w_cur * mid, h_cur * mid);
        if let Some(f) = feasible(w, h, &mut checks) {
            best = Some((w, h, f));
            hi = mid;
        } else {
            lo = mid;
        }
    }

    // ── Phase 2: greedy per-dimension shrink. ──
    // Runs whether aspect is keep (refinement) or free (the main event),
    // starting from the best size we have. If phase 1 found nothing, seed
    // the greedy pass from the full outline so a shape that only shrinks
    // on one axis is still discovered.
    let (mut bw, mut bh) = best.as_ref().map_or((w_cur, h_cur), |(w, h, _)| (*w, *h));
    loop {
        if start.elapsed() >= opts.time_budget || checks >= opts.max_checks {
            break;
        }
        let mut improved = false;
        if bw - opts.step_mm >= w_min {
            if let Some(f) = feasible(bw - opts.step_mm, bh, &mut checks) {
                bw -= opts.step_mm;
                best = Some((bw, bh, f));
                improved = true;
            }
        }
        if bh - opts.step_mm >= h_min {
            if let Some(f) = feasible(bw, bh - opts.step_mm, &mut checks) {
                bh -= opts.step_mm;
                best = Some((bw, bh, f));
                improved = true;
            }
        }
        if !improved {
            break;
        }
    }

    match best {
        Some((w, h, f)) if w * h < old_area - 1e-6 => {
            let new_area = w * h;
            let metrics = CompactMetrics {
                old_w_mm: w_cur,
                old_h_mm: h_cur,
                old_area_mm2: old_area,
                new_w_mm: w,
                new_h_mm: h,
                new_area_mm2: new_area,
                area_reduction_pct: (old_area - new_area) / old_area * 100.0,
                trace_count: f.trace_count,
                via_count: f.via_count,
                total_length_mm: f.total_length_mm,
                failed_nets: 0,
                drc_errors: 0,
                lower_bound_w_mm: w_min,
                lower_bound_h_mm: h_min,
                checks,
            };
            Ok(CompactOutcome {
                board: f.board,
                metrics,
                shrunk: true,
            })
        }
        _ => Ok(CompactOutcome {
            board: base.clone(),
            metrics: untouched_metrics(checks),
            shrunk: false,
        }),
    }
}

/// Deterministically derive a per-check placer seed from the base seed
/// and the check index, so the same base seed → the same search.
fn derive_seed(base: u64, check: usize) -> u64 {
    base.wrapping_mul(0x9E37_79B9_7F4A_7C15)
        .wrapping_add(check as u64)
        .wrapping_add(1)
        .max(1)
}

/// Build one candidate board at `w_mm × h_mm`, re-place, re-route,
/// re-DRC. `Some` iff every net routes and DRC is error-free.
#[allow(clippy::too_many_arguments)]
fn try_feasible(
    base: &Board,
    base_min: Point,
    w_mm: f64,
    h_mm: f64,
    corner_radius: Length,
    seed: u64,
    base_min_gap: f64,
    old_area: f64,
    opts: &CompactOptions,
    place_margins: &MarginMap,
    route_opts: &RouteOptions,
    drc_margins: &HashMap<String, PlacementMargin>,
    fab_profile: Option<&pcb_drc::FabProfile>,
    schematic: &Arc<Schematic>,
) -> Option<Feasible> {
    let mut b = base.clone();
    let new_outline = Rect::from_corners(
        base_min,
        Point::new(
            base_min.x + Length::from_mm(w_mm),
            base_min.y + Length::from_mm(h_mm),
        ),
    );
    // Clamp the corner radius to half the shorter side of the new outline.
    let cap = new_outline.width().0.min(new_outline.height().0) / 2;
    b.outline = Some(new_outline);
    b.outline_corner_radius = Length(corner_radius.0.max(0).min(cap));
    // Traces/vias are re-laid from scratch; pours stay (they are
    // net/layer policies, not geometry, and re-fill downstream).
    b.clear_routing();

    // Snap edge-mounted parts onto the new outline first, then clamp any
    // footprint poking outside back inside — otherwise the placer's hard
    // "edge parts must touch the outline" / "fit inside" constraints can
    // start from an infeasible pose and never recover.
    for id in b.footprint_order.clone() {
        if let Some(fp) = b.footprints.get(&id) {
            if fp.edge_mounted {
                if let Some(delta) = snap_to_nearest_edge(fp, new_outline) {
                    if let Some(fp) = b.footprints.get_mut(&id) {
                        fp.position = fp.position.translate(delta.0, delta.1);
                    }
                }
            }
        }
        if let Some(fp) = b.footprints.get(&id) {
            if let Some(delta) = clamp_inside(fp, new_outline) {
                if let Some(fp) = b.footprints.get_mut(&id) {
                    fp.position = fp.position.translate(delta.0, delta.1);
                }
            }
        }
    }
    // Board-level silk that would now fall outside the outline is pulled
    // back inside with a small margin.
    clamp_silk_texts(&mut b.silk_texts, new_outline);

    // Scale the soft gap preference down with the board, floored at the
    // placer's hard 0.5 mm clearance.
    let scale = (w_mm * h_mm / old_area).sqrt();
    let min_gap = (base_min_gap * scale).max(PlaceOptions::default().min_clearance_mm);

    let place_opts = PlaceOptions {
        seed,
        max_iterations: opts.place_iters,
        min_gap_mm: min_gap,
        ..PlaceOptions::default()
    };
    let movable: Vec<String> = b
        .footprints_in_order()
        .map(|fp| fp.reference.clone())
        .collect();
    place(&mut b, &movable, &place_opts, place_margins).ok()?;

    let report = pcb_router::route(&mut b, route_opts);
    let any_failed = report
        .per_net
        .iter()
        .any(|(_, o)| matches!(o, Outcome::Failed { .. }));
    if any_failed {
        return None;
    }

    let drc_opts = pcb_drc::DrcOptions {
        placement_margins: drc_margins.clone(),
        schematic: Some(schematic.clone()),
        fab_profile: fab_profile.cloned(),
        ..pcb_drc::DrcOptions::default()
    };
    let drc = pcb_drc::run(&b, &drc_opts);
    if drc.error_count > 0 {
        return None;
    }

    Some(Feasible {
        board: b,
        trace_count: report.trace_count,
        via_count: report.via_count,
        total_length_mm: report.total_length_mm,
    })
}

/// Translation (dx, dy) that moves `fp` so its bbox touches the nearest
/// side of `outline`. `None` if the footprint has no bounds.
fn snap_to_nearest_edge(fp: &pcb_core::Footprint, outline: Rect) -> Option<(Length, Length)> {
    let b = fp.bounds()?;
    let d_left = (b.min.x.0 - outline.min.x.0).abs();
    let d_right = (outline.max.x.0 - b.max.x.0).abs();
    let d_bottom = (b.min.y.0 - outline.min.y.0).abs();
    let d_top = (outline.max.y.0 - b.max.y.0).abs();
    let nearest = d_left.min(d_right).min(d_bottom).min(d_top);
    let (mut dx, mut dy) = (Length::ZERO, Length::ZERO);
    if nearest == d_left {
        dx = outline.min.x - b.min.x;
    } else if nearest == d_right {
        dx = outline.max.x - b.max.x;
    } else if nearest == d_bottom {
        dy = outline.min.y - b.min.y;
    } else {
        dy = outline.max.y - b.max.y;
    }
    Some((dx, dy))
}

/// Translation (dx, dy) that pulls `fp`'s bbox fully inside `outline`.
/// `None` when it already fits or has no bounds.
fn clamp_inside(fp: &pcb_core::Footprint, outline: Rect) -> Option<(Length, Length)> {
    let b = fp.bounds()?;
    let mut dx = Length::ZERO;
    let mut dy = Length::ZERO;
    if b.min.x.0 < outline.min.x.0 {
        dx = outline.min.x - b.min.x;
    } else if b.max.x.0 > outline.max.x.0 {
        dx = outline.max.x - b.max.x;
    }
    if b.min.y.0 < outline.min.y.0 {
        dy = outline.min.y - b.min.y;
    } else if b.max.y.0 > outline.max.y.0 {
        dy = outline.max.y - b.max.y;
    }
    if dx.0 == 0 && dy.0 == 0 {
        None
    } else {
        Some((dx, dy))
    }
}

/// Clamp every board-level silk text anchor to sit at least `MARGIN_MM`
/// inside `outline`, so a label doesn't spill past a shrunk edge.
fn clamp_silk_texts(texts: &mut [SilkText], outline: Rect) {
    const MARGIN_MM: f64 = 1.0;
    let m = Length::from_mm(MARGIN_MM);
    let lo_x = outline.min.x + m;
    let hi_x = outline.max.x - m;
    let lo_y = outline.min.y + m;
    let hi_y = outline.max.y - m;
    for t in texts {
        // Guard against a tiny outline where the margins cross over.
        if lo_x.0 <= hi_x.0 {
            t.position.x = Length(t.position.x.0.clamp(lo_x.0, hi_x.0));
        }
        if lo_y.0 <= hi_y.0 {
            t.position.y = Length(t.position.y.0.clamp(lo_y.0, hi_y.0));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pcb_core::{CopperLayer, Footprint, Id, Pad};

    fn pad(num: &str, off_x: f64, off_y: f64, net: Option<&str>) -> Pad {
        Pad {
            number: num.into(),
            name: String::new(),
            offset: Point::new(Length::from_mm(off_x), Length::from_mm(off_y)),
            size: (Length::from_mm(1.0), Length::from_mm(1.2)),
            layer: CopperLayer::Top,
            net: net.map(str::to_string),
            drill: None,
        }
    }

    fn footprint(reference: &str, x_mm: f64, y_mm: f64, pads: Vec<Pad>) -> Footprint {
        Footprint {
            id: Id::new(),
            reference: reference.into(),
            value: String::new(),
            library: "demo".into(),
            position: Point::new(Length::from_mm(x_mm), Length::from_mm(y_mm)),
            rotation: 0.0,
            layer: CopperLayer::Top,
            pads,
            key: String::new(),
            description: String::new(),
            edge_mounted: false,
            silk: Vec::new(),
        }
    }

    fn set_outline(board: &mut Board, w: f64, h: f64) {
        board.outline = Some(Rect::from_corners(
            Point::new(Length::ZERO, Length::ZERO),
            Point::new(Length::from_mm(w), Length::from_mm(h)),
        ));
    }

    /// Two two-pad parts on shared nets, spread across a roomy outline.
    fn two_part_board(w: f64, h: f64) -> Board {
        let mut board = Board::new();
        set_outline(&mut board, w, h);
        board.add_footprint(footprint(
            "R1",
            6.0,
            6.0,
            vec![
                pad("1", -1.0, 0.0, Some("A")),
                pad("2", 1.0, 0.0, Some("N")),
            ],
        ));
        board.add_footprint(footprint(
            "R2",
            w - 6.0,
            h - 6.0,
            vec![
                pad("1", -1.0, 0.0, Some("N")),
                pad("2", 1.0, 0.0, Some("B")),
            ],
        ));
        board
    }

    fn fast_opts() -> CompactOptions {
        CompactOptions {
            place_iters: 1500,
            binary_steps: 6,
            max_checks: 24,
            time_budget: Duration::from_secs(60),
            seed: 7,
            ..CompactOptions::default()
        }
    }

    #[test]
    fn lower_bound_tracks_geometry() {
        // Single 1×1.2 mm pad part → bbox ~1×1.2; min side ~1.0 mm, plus
        // 2 × 0.3 mm edge clearance ⇒ floor ≈ 1.6 mm on each dimension.
        let mut board = Board::new();
        set_outline(&mut board, 20.0, 20.0);
        board.add_footprint(footprint(
            "R1",
            5.0,
            5.0,
            vec![pad("1", 0.0, 0.0, Some("A"))],
        ));
        let (w, h, area) =
            lower_bound_outline(&board, &MarginMap::new(), &CompactOptions::default());
        assert!((w - 1.6).abs() < 0.05, "w_min {w}");
        assert!((h - 1.6).abs() < 0.05, "h_min {h}");
        // Area floor = packing_factor (1.3) × (1.0 × 1.2) = 1.56 mm².
        assert!((area - 1.56).abs() < 0.05, "area_min {area}");
    }

    #[test]
    fn min_w_min_h_raise_the_floor() {
        let mut board = Board::new();
        set_outline(&mut board, 20.0, 20.0);
        board.add_footprint(footprint(
            "R1",
            5.0,
            5.0,
            vec![pad("1", 0.0, 0.0, Some("A"))],
        ));
        let opts = CompactOptions {
            min_w_mm: Some(12.0),
            min_h_mm: Some(8.0),
            ..CompactOptions::default()
        };
        let (w, h, _) = lower_bound_outline(&board, &MarginMap::new(), &opts);
        assert!((w - 12.0).abs() < 1e-6);
        assert!((h - 8.0).abs() < 1e-6);
    }

    #[test]
    fn compacts_an_oversized_board() {
        // A 40×40 board holding two tiny parts should shrink a lot while
        // still routing its one shared net and passing DRC.
        let board = two_part_board(40.0, 40.0);
        let out = compact(
            &board,
            &Arc::new(Schematic::default()),
            &MarginMap::new(),
            &HashMap::new(),
            None,
            &fast_opts(),
        )
        .expect("compact ok");
        assert!(out.shrunk, "expected a shrink on a roomy board");
        assert!(
            out.metrics.new_area_mm2 < out.metrics.old_area_mm2 * 0.9,
            "area {} -> {} not measurably smaller",
            out.metrics.old_area_mm2,
            out.metrics.new_area_mm2,
        );
        assert_eq!(out.metrics.failed_nets, 0);
        assert_eq!(out.metrics.drc_errors, 0);
        // The shrunk board's outline actually matches the reported size.
        let o = out.board.outline.expect("outline");
        assert!((o.width().to_mm() - out.metrics.new_w_mm).abs() < 1e-3);
        assert!((o.height().to_mm() - out.metrics.new_h_mm).abs() < 1e-3);
    }

    #[test]
    fn deterministic_for_a_fixed_seed() {
        let board = two_part_board(40.0, 40.0);
        let run = || {
            compact(
                &board,
                &Arc::new(Schematic::default()),
                &MarginMap::new(),
                &HashMap::new(),
                None,
                &fast_opts(),
            )
            .unwrap()
        };
        let a = run();
        let b = run();
        assert_eq!(a.shrunk, b.shrunk);
        assert!((a.metrics.new_w_mm - b.metrics.new_w_mm).abs() < 1e-9);
        assert!((a.metrics.new_h_mm - b.metrics.new_h_mm).abs() < 1e-9);
        assert_eq!(a.metrics.checks, b.metrics.checks);
    }

    #[test]
    fn board_at_minimum_is_left_untouched() {
        // Outline already at the geometric floor: s_min ≈ 1, no shrink.
        let mut board = two_part_board(40.0, 40.0);
        let (w_min, h_min, _) =
            lower_bound_outline(&board, &MarginMap::new(), &CompactOptions::default());
        set_outline(&mut board, w_min, h_min);
        let out = compact(
            &board,
            &Arc::new(Schematic::default()),
            &MarginMap::new(),
            &HashMap::new(),
            None,
            &fast_opts(),
        )
        .expect("compact ok");
        assert!(!out.shrunk, "a board at its floor must not shrink");
        assert_eq!(out.metrics.new_area_mm2, out.metrics.old_area_mm2);
        // Untouched: same outline as we set.
        let o = out.board.outline.expect("outline");
        assert!((o.width().to_mm() - w_min).abs() < 1e-3);
    }

    #[test]
    fn edge_mounted_part_still_touches_after_compaction() {
        let mut board = Board::new();
        set_outline(&mut board, 40.0, 40.0);
        let mut j1 = footprint(
            "J1",
            2.0,
            20.0,
            vec![
                pad("1", 0.0, -1.0, Some("A")),
                pad("2", 0.0, 1.0, Some("N")),
            ],
        );
        j1.edge_mounted = true;
        board.add_footprint(j1);
        board.add_footprint(footprint(
            "R1",
            30.0,
            20.0,
            vec![
                pad("1", -1.0, 0.0, Some("N")),
                pad("2", 1.0, 0.0, Some("B")),
            ],
        ));
        let out = compact(
            &board,
            &Arc::new(Schematic::default()),
            &MarginMap::new(),
            &HashMap::new(),
            None,
            &fast_opts(),
        )
        .expect("compact ok");
        let o = out.board.outline.expect("outline");
        let j = out
            .board
            .footprints_in_order()
            .find(|f| f.reference == "J1")
            .expect("J1");
        let b = j.bounds().expect("bounds");
        let tol = 0.5; // matches EDGE_TOUCH_TOLERANCE_MM
        let touches = (b.min.x.to_mm() - o.min.x.to_mm()).abs() <= tol
            || (o.max.x.to_mm() - b.max.x.to_mm()).abs() <= tol
            || (b.min.y.to_mm() - o.min.y.to_mm()).abs() <= tol
            || (o.max.y.to_mm() - b.max.y.to_mm()).abs() <= tol;
        assert!(touches, "edge-mounted J1 no longer touches the outline");
    }
}
