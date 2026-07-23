//! `pcb-placer` — simulated-annealing footprint placer.
//!
//! Given a board, a set of "movable" footprint references, and an
//! options bundle, the placer searches for new positions that minimise
//! total HPWL (sum of half-perimeter wire length over every net) while
//! keeping all footprints inside the outline, non-overlapping, and (for
//! `edge_mounted = true` parts) still touching the outline.
//!
//! Movability is per-call, not per-footprint state: callers pass the
//! list of refs they want the placer to touch. Everything else stays
//! pinned. This keeps the on-disk footprint schema unchanged and lets
//! the agent run a focused "auto-place these three parts" without
//! committing the whole board to a move.
//!
//! The objective is HPWL — same metric the DRC and router report — so
//! a successful placement run translates directly into shorter wire and
//! lower detour ratios when the router is rerun afterwards.

use std::collections::HashMap;

use pcb_core::{Board, Footprint, Id, Length, Point, Rect};

/// Per-footprint placement margin in mm, in the footprint's LOCAL
/// frame: `[top, right, bottom, left]`. Stored on `LibraryEntry` and
/// resolved by the caller into a map keyed by footprint id.
pub type MarginMap = HashMap<Id, [f64; 4]>;

/// Rotate a `[top, right, bottom, left]` local-frame margin into the
/// world-aligned `[top, right, bottom, left]` AABB inflation, given a
/// footprint rotation in degrees CCW. We only handle 90° increments
/// (matching the placer's `Rotate90` move); anything off-axis snaps to
/// the nearest quadrant — for placement keep-out this rounding error
/// is irrelevant compared to the user-set margins (usually >= 0.5 mm).
fn rotated_margin(local: [f64; 4], rotation_deg: f32) -> [f64; 4] {
    let r = f64::from(rotation_deg).rem_euclid(360.0);
    // local order: [top, right, bottom, left]
    let [t, r2, b, l] = local;
    if (45.0..135.0).contains(&r) {
        // +90° CCW: local +Y (top) maps to world -X (left); local +X
        // (right) maps to world +Y (top).
        [r2, b, l, t]
    } else if (135.0..225.0).contains(&r) {
        // 180°: flip both axes.
        [b, l, t, r2]
    } else if (225.0..315.0).contains(&r) {
        // +270° CCW.
        [l, t, r2, b]
    } else {
        local
    }
}

/// Inflate `bounds` by per-side world-aligned margins (`[top, right,
/// bottom, left]`, mm).
fn inflate_rect(bounds: Rect, sides: [f64; 4]) -> Rect {
    let [t, r, b, l] = sides;
    Rect {
        min: Point::new(
            bounds.min.x - Length::from_mm(l),
            bounds.min.y - Length::from_mm(b),
        ),
        max: Point::new(
            bounds.max.x + Length::from_mm(r),
            bounds.max.y + Length::from_mm(t),
        ),
    }
}

/// World-frame bounding box of `fp` already inflated by the placement
/// margin recorded for it in `margins` (if any). Used everywhere the
/// placer wants "where is this part for component-to-component
/// purposes" — pad clearance is still pad-level in DRC, so the margin
/// only matters for body-to-body separation here.
fn fp_bounds_with_margin(fp: &Footprint, margins: &MarginMap) -> Option<Rect> {
    let b = fp.bounds()?;
    let Some(local) = margins.get(&fp.id) else {
        return Some(b);
    };
    if local.iter().all(|v| *v <= 0.0) {
        return Some(b);
    }
    let world = rotated_margin(*local, fp.rotation);
    Some(inflate_rect(b, world))
}

/// Tunables for the SA search. Defaults are calibrated on the
/// door-controller test project: ~3 s wall-clock, reliably converges
/// to within a few percent of HPWL minimum for ≤20 footprints.
#[derive(Debug, Clone)]
pub struct PlaceOptions {
    /// How many SA candidate moves to try in total.
    pub max_iterations: usize,
    /// Starting temperature. Tuned in the same units as HPWL (mm) so a
    /// move that worsens HPWL by ~T at iteration 0 has ~37 % accept
    /// probability — wide exploration up front.
    pub initial_temp: f64,
    /// Final temperature; the schedule cools geometrically from
    /// `initial_temp` to `final_temp` over `max_iterations` steps.
    pub final_temp: f64,
    /// Largest random translation, in mm, attempted at the start of
    /// the run. Shrinks linearly to `min_step_mm` as we cool.
    pub max_step_mm: f64,
    pub min_step_mm: f64,
    /// PRNG seed: same seed → same placement, so a script run is
    /// reproducible. `0` lets the placer pick its own seed from the
    /// system clock.
    pub seed: u64,
    /// Minimum body-to-body gap (mm) the placer *prefers*. Soft
    /// constraint: pairs of footprints whose gap drops below this
    /// contribute a quadratic penalty to the SA score, so the placer
    /// pushes them apart over time instead of hard-rejecting moves
    /// that don't already satisfy it. Pad-against-pad collisions
    /// (gap <= 0) ARE hard-rejected — we never produce an electrical
    /// short. Default 1.5 mm gives the router about 5 cells of free
    /// corridor at a 0.25 mm pitch.
    pub min_gap_mm: f64,
    /// **Hard** minimum body-to-body clearance (mm). The SA never accepts
    /// a move that leaves the moved footprint closer than this to any
    /// other body — so the final placement ALWAYS has at least this much
    /// margin between component edges (no overlaps, no touching). Moves
    /// that *increase* the gap of an already-too-close pair are still
    /// allowed, so the placer can separate an overlapping starting layout.
    /// Distinct from `min_gap_mm` (a soft *preference*, typically larger).
    pub min_clearance_mm: f64,
    /// **Hard** solder-access floor (mm). The effective hard clearance the
    /// SA enforces is `max(min_clearance_mm, solder_gap_mm)`, so the final
    /// placement never leaves two component bodies closer than this — the
    /// user hand-solders and needs iron-tip access between parts, so parts
    /// must NEVER end up nearly touching after auto-place / compact.
    /// Default 1.0 mm. Set to 0 to degrade to the old behaviour where
    /// `min_clearance_mm` (0.5 mm) is the only hard floor.
    pub solder_gap_mm: f64,
    /// Score weight on the soft-gap penalty term. Penalty per pair is
    /// `(min_gap_mm - actual_gap)^2` (mm²) when below the threshold;
    /// the score adds `gap_penalty_factor * Σ penalty` to HPWL. Default
    /// 4.0 makes a 0.5 mm shortfall on one pair cost ~1 mm-equivalent
    /// of HPWL, large enough to be felt without dominating routing.
    pub gap_penalty_factor: f64,
    /// Resolution of the congestion-proxy grid (cells per side) over
    /// the board outline. The placer rasterises every net's pad bbox
    /// onto this grid and sums per-cell overflow (= count - 1) as a
    /// proxy for "how many nets fight over the same routing channel".
    /// 32 → ~3 mm cells on an 80 mm board; coarse enough to be cheap,
    /// fine enough to distinguish channels. 0 disables congestion.
    pub congestion_resolution: u32,
    /// Score weight on the congestion proxy. Each unit of overflow
    /// (= one extra net sharing a cell with another) costs this many
    /// mm-equivalent of HPWL. Default 1.0 makes a placement where 50
    /// cells are doubly-claimed cost 50 mm — comparable to the wire
    /// reductions HPWL captures. Tune up if the placer keeps producing
    /// "tight HPWL but unroutable" layouts; tune down if it spreads
    /// parts so far apart wire goes back up.
    pub congestion_penalty_factor: f64,
}

impl Default for PlaceOptions {
    fn default() -> Self {
        Self {
            max_iterations: 8000,
            initial_temp: 50.0,
            final_temp: 0.05,
            max_step_mm: 20.0,
            min_step_mm: 0.5,
            seed: 0,
            // 2 mm body-to-body keeps the router corridor wide enough
            // that even a star net can slip through between any two
            // footprints without DRC pad-pad clearance violations on
            // the typical 0.2 mm clearance.
            min_gap_mm: 2.0,
            // 0.5 mm fab-style hard margin between any two component
            // bodies. JLCPCB's component-to-component recommendation is
            // ~0.5 mm; smaller risks placement/assembly. Overridden
            // upward by `solder_gap_mm` for hand-soldering.
            min_clearance_mm: 0.5,
            // Hand-soldering access floor — must match
            // `pcb_core::MIN_FOOTPRINT_GAP_MM` so place/move/rotate and
            // auto-place/compact agree. Parts must never end up nearly
            // touching: the user needs iron-tip room between bodies.
            solder_gap_mm: pcb_core::MIN_FOOTPRINT_GAP_MM,
            // Steep enough that SA reliably enforces `min_gap_mm` on
            // small nets — a 1 mm shortfall on one pair costs
            // 16 mm-equivalent of HPWL, well above the noise floor.
            gap_penalty_factor: 16.0,
            // 32×32 grid maps cleanly to typical SMD boards (50–100 mm
            // wide → 1.5–3 mm cells, finer than a footprint body).
            congestion_resolution: 32,
            // Each shared cell costs ~1 mm of equivalent HPWL —
            // empirically enough to discourage piling nets in one
            // corridor without dominating the score.
            congestion_penalty_factor: 1.0,
        }
    }
}

#[derive(Debug, Clone)]
pub struct PlaceReport {
    /// HPWL at the start of the run, mm.
    pub initial_hpwl_mm: f64,
    /// HPWL of the best placement found, mm.
    pub final_hpwl_mm: f64,
    /// Congestion overflow at the start, summed over the rasterised
    /// grid. 0 = every cell is touched by at most one net's pad bbox.
    pub initial_congestion: f64,
    /// Congestion overflow of the best placement.
    pub final_congestion: f64,
    /// Number of SA candidate moves tried.
    pub iterations: usize,
    /// Of those, how many were applied (improving moves + accepted-uphill).
    pub accepted: usize,
    /// Refs of footprints whose position changed by ≥ 0.05 mm.
    pub moved: Vec<String>,
    /// Refs the caller listed but the placer couldn't touch (unknown,
    /// or no `bounds()` because they have no pads).
    pub skipped: Vec<String>,
}

/// Run the SA placer in-place on `board`. Only footprints whose
/// reference is in `movable` are candidates for movement; everything
/// else is pinned. Returns a report with HPWL before/after and the
/// list of refs whose position changed.
///
/// On error (no outline, no movables found, no nets touching movables),
/// returns a report with `iterations = 0` and `final_hpwl == initial_hpwl`.
pub fn place(
    board: &mut Board,
    movable: &[String],
    opts: &PlaceOptions,
    margins: &MarginMap,
) -> Result<PlaceReport, String> {
    let outline = board.outline.ok_or_else(|| {
        "auto-place needs a board outline; set one with `outline W H`".to_string()
    })?;

    // Resolve movable refs to ids, skipping unknowns. Capturing ids
    // up front means `movable` order doesn't matter and we don't
    // re-walk the footprint map per move.
    let mut movable_ids: Vec<Id> = Vec::new();
    let mut starting_positions: HashMap<Id, Point> = HashMap::new();
    let mut skipped: Vec<String> = Vec::new();
    for r in movable {
        let found = board.footprints_in_order().find(|fp| fp.reference == *r);
        match found {
            Some(fp) if fp.bounds().is_some() => {
                movable_ids.push(fp.id);
                starting_positions.insert(fp.id, fp.position);
            }
            _ => skipped.push(r.clone()),
        }
    }
    if movable_ids.is_empty() {
        return Ok(PlaceReport {
            initial_hpwl_mm: total_hpwl(board),
            final_hpwl_mm: total_hpwl(board),
            initial_congestion: 0.0,
            final_congestion: 0.0,
            iterations: 0,
            accepted: 0,
            moved: Vec::new(),
            skipped,
        });
    }

    // Edge-mounted connectors (screw terminals, USB modules, headers…)
    // must sit on the outline. If they start floating in the middle
    // (spawned before the library flag was set, or after a bad manual
    // place), every SA move that stays interior is hard-rejected and
    // they freeze off-edge forever. Snap them to the nearest edge
    // first so the search starts in a feasible region. Refresh
    // `starting_positions` after the snap so "moved" is relative to
    // the post-snap pose (the real SA baseline).
    for id in &movable_ids {
        let Some(fp) = board.footprints.get(id).cloned() else {
            continue;
        };
        if !fp.edge_mounted {
            continue;
        }
        if board.edge_mount_violation(&fp).is_none() {
            continue;
        }
        if let Some((dx, dy)) = snap_delta_to_nearest_edge(&fp, outline) {
            if let Some(fp) = board.footprints.get_mut(id) {
                fp.position = Point::new(fp.position.x + dx, fp.position.y + dy);
            }
        }
    }
    for id in &movable_ids {
        if let Some(fp) = board.footprints.get(id) {
            starting_positions.insert(*id, fp.position);
        }
    }

    // Net membership: for each movable footprint, which nets does it
    // contribute pads to? Used to compute incremental HPWL deltas
    // after a move (HPWL per net depends on min/max pad coords).
    let mut nets_of_id: HashMap<Id, Vec<String>> = HashMap::new();
    for id in &movable_ids {
        let Some(fp) = board.footprints.get(id) else {
            continue;
        };
        let mut nets: Vec<String> = fp.pads.iter().filter_map(|p| p.net.clone()).collect();
        nets.sort();
        nets.dedup();
        nets_of_id.insert(*id, nets);
    }

    let initial_hpwl = total_hpwl(board);
    let initial_congestion = if opts.congestion_resolution > 0 {
        congestion_overflow(board, outline, opts.congestion_resolution)
    } else {
        0.0
    };
    let initial_score = initial_hpwl
        + opts.gap_penalty_factor * total_gap_penalty(board, opts.min_gap_mm, margins)
        + opts.congestion_penalty_factor * initial_congestion;
    let mut current_score = initial_score;
    let mut best_score = initial_score;
    let mut best_hpwl = initial_hpwl;
    let mut best_congestion = initial_congestion;
    let mut best_positions: HashMap<Id, Point> = movable_ids
        .iter()
        .map(|id| (*id, board.footprints[id].position))
        .collect();
    let mut best_rotations: HashMap<Id, f32> = movable_ids
        .iter()
        .map(|id| (*id, board.footprints[id].rotation))
        .collect();

    let mut rng = Rng::new(if opts.seed == 0 {
        seed_from_clock()
    } else {
        opts.seed
    });

    let cooling =
        (opts.final_temp / opts.initial_temp).powf(1.0 / opts.max_iterations.max(1) as f64);
    let mut accepted = 0usize;

    for iter in 0..opts.max_iterations {
        // Linear progression of temperature & step size with iteration.
        let temp = opts.initial_temp * cooling.powi(iter as i32);
        let progress = iter as f64 / opts.max_iterations.max(1) as f64;
        let step_mm = opts.max_step_mm * (1.0 - progress) + opts.min_step_mm * progress;

        let id = movable_ids[rng.next_usize() % movable_ids.len()];
        let Some(fp) = board.footprints.get(&id) else {
            continue;
        };
        let move_kind = {
            let roll = rng.next_u32() % 16;
            if roll == 0 {
                // 1/16: 90° rotation in place — cheap orientation fix.
                MoveKind::Rotate90 { footprint: id }
            } else if roll == 1 {
                // 1/16: long-range teleport to a random point inside the
                // outline. Local steps can't hop a large obstacle (OLED
                // body, dense module clusters): every intermediate cell
                // that collides is hard-rejected, so a part trapped in a
                // pocket freezes there forever. Teleports let SA escape.
                let ow = outline.width().to_mm();
                let oh = outline.height().to_mm();
                let margin = 2.0; // keep away from the absolute edge
                let x = outline.min.x.to_mm()
                    + margin
                    + rng.next_f64() * (ow - 2.0 * margin).max(0.0);
                let y = outline.min.y.to_mm()
                    + margin
                    + rng.next_f64() * (oh - 2.0 * margin).max(0.0);
                MoveKind::Translate {
                    footprint: id,
                    new_pos: Point::new(Length::from_mm(x), Length::from_mm(y)),
                }
            } else {
                // Local translation within `step_mm` mm in either axis.
                let dx_mm = (rng.next_f64() - 0.5) * 2.0 * step_mm;
                let dy_mm = (rng.next_f64() - 0.5) * 2.0 * step_mm;
                MoveKind::Translate {
                    footprint: id,
                    new_pos: Point::new(
                        fp.position.x + Length::from_mm(dx_mm),
                        fp.position.y + Length::from_mm(dy_mm),
                    ),
                }
            }
        };

        // Evaluate the move on a probe clone of the footprint and
        // check constraints. Reject hard violations outright.
        let (probe, original_position, original_rotation) = make_probe(board, &move_kind);
        let Some(probe) = probe else { continue };
        // Pads must stay on copper (inside the outline). The plastic
        // body may hang off an edge whose pads already touch that
        // edge — same rule as place/move/DRC (`body_outline_violation`)
        // so an OLED / breakout can sit header-on-board, body off-edge.
        if !pads_inside_outline(&probe, outline) {
            continue;
        }
        {
            let margin = margin_for_fp(&probe, margins);
            // Synthesize a temporary board view: body_outline_violation
            // only needs `outline` + the probe footprint's geometry.
            if board.body_outline_violation(&probe, margin).is_some() {
                continue;
            }
        }
        // Hard clearance: never accept a move that leaves the moved
        // footprint closer than the hard floor to any other body — so the
        // result always has a real margin between component edges (no
        // overlap, no touching). The floor is `max(min_clearance_mm,
        // solder_gap_mm)`: the solder-access gap (default 1.0 mm) so the
        // user can get an iron tip between parts. EXCEPTION: if the part is
        // already inside the margin (e.g. an overlapping starting layout),
        // allow a move that *increases* its worst gap, so SA can separate
        // things out instead of being frozen.
        let hard_clearance = opts.min_clearance_mm.max(opts.solder_gap_mm);
        let after_gap = probe_min_gap(board, &probe, margins);
        if after_gap < hard_clearance {
            let before_gap = footprint_min_gap(board, probe.id, margins);
            if after_gap <= before_gap {
                continue; // would create or worsen a sub-margin clearance
            }
        }
        if board.edge_mount_violation(&probe).is_some() {
            continue;
        }

        // Score delta: HPWL is local to the nets this footprint
        // touches; the gap penalty is local to the pairs that touch
        // this footprint; the congestion proxy depends on every net's
        // pad bbox overlapping. Recompute the relevant pieces before
        // and after applying the move.
        let nets = nets_of_id.get(&probe.id).cloned().unwrap_or_default();
        let before_hpwl: f64 = nets.iter().map(|n| net_hpwl(board, n)).sum();
        let before_pen = footprint_gap_penalty(board, probe.id, opts.min_gap_mm, margins);
        let before_cong = if opts.congestion_resolution > 0 {
            congestion_overflow(board, outline, opts.congestion_resolution)
        } else {
            0.0
        };
        // Apply the move temporarily to compute the new HPWL on the
        // affected nets.
        apply_move_in_place(board, &move_kind);
        let after_hpwl: f64 = nets.iter().map(|n| net_hpwl(board, n)).sum();
        let after_pen = footprint_gap_penalty(board, probe.id, opts.min_gap_mm, margins);
        let after_cong = if opts.congestion_resolution > 0 {
            congestion_overflow(board, outline, opts.congestion_resolution)
        } else {
            0.0
        };
        let delta = (after_hpwl - before_hpwl)
            + opts.gap_penalty_factor * (after_pen - before_pen)
            + opts.congestion_penalty_factor * (after_cong - before_cong);

        let accept = if delta <= 0.0 {
            true
        } else {
            // Metropolis: accept uphill with probability exp(-Δ/T).
            let p = (-delta / temp).exp();
            rng.next_f64() < p
        };

        if accept {
            current_score += delta;
            accepted += 1;
            if current_score < best_score {
                best_score = current_score;
                best_hpwl = total_hpwl(board);
                best_congestion = if opts.congestion_resolution > 0 {
                    congestion_overflow(board, outline, opts.congestion_resolution)
                } else {
                    0.0
                };
                for id in &movable_ids {
                    if let Some(fp) = board.footprints.get(id) {
                        best_positions.insert(*id, fp.position);
                        best_rotations.insert(*id, fp.rotation);
                    }
                }
            }
        } else {
            // Roll back.
            revert_move_in_place(board, &move_kind, original_position, original_rotation);
        }
    }

    // Restore the best placement we saw.
    for (id, pos) in &best_positions {
        if let Some(fp) = board.footprints.get_mut(id) {
            fp.position = *pos;
        }
    }
    for (id, rot) in &best_rotations {
        if let Some(fp) = board.footprints.get_mut(id) {
            fp.rotation = *rot;
        }
    }

    let mut moved: Vec<String> = Vec::new();
    for id in &movable_ids {
        let Some(fp) = board.footprints.get(id) else {
            continue;
        };
        let start = starting_positions[id];
        let dx_mm = (fp.position.x.to_mm() - start.x.to_mm()).abs();
        let dy_mm = (fp.position.y.to_mm() - start.y.to_mm()).abs();
        if dx_mm + dy_mm >= 0.05 {
            moved.push(fp.reference.clone());
        }
    }

    Ok(PlaceReport {
        initial_hpwl_mm: initial_hpwl,
        final_hpwl_mm: best_hpwl,
        initial_congestion,
        final_congestion: best_congestion,
        iterations: opts.max_iterations,
        accepted,
        moved,
        skipped,
    })
}

/// Sum of **weighted** HPWL across every multi-pad net on the board, mm.
///
/// Weight is `1 / max(1, n_pads - 1)` (classic clique model). Without
/// this, fat power nets (`+3V3`, `GND` with 7–15 pads) dominate the SA
/// score and a 2-pin series resistor like an SSR LED series R can be
/// left 40 mm from both of its neighbours while the placer chases
/// millimetres on the power plane. With the weight, a 2-pin net at
/// 50 mm costs the same as a 20-pin net at ~950 mm — short nets pull
/// their components into clusters.
fn total_hpwl(board: &Board) -> f64 {
    let mut nets: HashMap<&str, ([f64; 4], usize)> = HashMap::new();
    for fp in board.footprints.values() {
        for pad in &fp.pads {
            let Some(net) = pad.net.as_deref() else {
                continue;
            };
            let center = fp.pad_world_center(pad);
            let x = center.x.to_mm();
            let y = center.y.to_mm();
            let entry = nets.entry(net).or_insert((
                [
                    f64::INFINITY,
                    f64::INFINITY,
                    f64::NEG_INFINITY,
                    f64::NEG_INFINITY,
                ],
                0,
            ));
            entry.0[0] = entry.0[0].min(x);
            entry.0[1] = entry.0[1].min(y);
            entry.0[2] = entry.0[2].max(x);
            entry.0[3] = entry.0[3].max(y);
            entry.1 += 1;
        }
    }
    nets.values()
        .filter(|(b, count)| *count >= 2 && b[2] >= b[0] && b[3] >= b[1])
        .map(|(b, count)| {
            let raw = (b[2] - b[0]) + (b[3] - b[1]);
            raw * net_weight(*count)
        })
        .sum()
}

/// Weight for a net with `n_pads` pads. See `total_hpwl`.
///
/// Base clique weight is `1/(n-1)`. Multiplied by 4 so two-pin nets
/// (series R, LED, SSR LED drive…) pull their ends together hard
/// enough to beat residual fat-net / congestion noise on typical
/// IoT boards.
fn net_weight(n_pads: usize) -> f64 {
    4.0 / (n_pads.saturating_sub(1).max(1) as f64)
}

/// Weighted HPWL of a single net, mm. Returns 0 if the net has 0 or 1 pads.
fn net_hpwl(board: &Board, net: &str) -> f64 {
    let mut min_x = f64::INFINITY;
    let mut min_y = f64::INFINITY;
    let mut max_x = f64::NEG_INFINITY;
    let mut max_y = f64::NEG_INFINITY;
    let mut count = 0usize;
    for fp in board.footprints.values() {
        for pad in &fp.pads {
            if pad.net.as_deref() != Some(net) {
                continue;
            }
            let c = fp.pad_world_center(pad);
            let x = c.x.to_mm();
            let y = c.y.to_mm();
            min_x = min_x.min(x);
            min_y = min_y.min(y);
            max_x = max_x.max(x);
            max_y = max_y.max(y);
            count += 1;
        }
    }
    if count < 2 {
        return 0.0;
    }
    ((max_x - min_x) + (max_y - min_y)) * net_weight(count)
}

/// Translation that moves `fp` so its pad bbox touches the nearest side
/// of `outline`. Used to un-stick edge-mounted parts that spawned
/// interior before the SA loop starts.
fn snap_delta_to_nearest_edge(
    fp: &Footprint,
    outline: pcb_core::Rect,
) -> Option<(Length, Length)> {
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

/// Routing-congestion proxy: rasterise every net's pad bounding box
/// onto a `res × res` grid spanning `outline`, count nets per cell,
/// and sum `max(0, count - 1)` — the "overflow" of nets sharing a
/// cell. Higher number = more nets fighting over the same routing
/// channel = harder for the router to lay clean copper.
///
/// This is a coarse proxy, not a real router cost: it doesn't know
/// about pad rotation, individual trace widths, or the layered grid.
/// What it captures is the basic "did the placer cluster too many
/// signals through one bottleneck" failure mode that pure HPWL
/// minimisation produces. Cheap to compute (`O(N_nets` × cells)).
fn congestion_overflow(board: &Board, outline: pcb_core::Rect, res: u32) -> f64 {
    if res == 0 {
        return 0.0;
    }
    let res_i = res as i32;
    let ox = outline.min.x.to_mm();
    let oy = outline.min.y.to_mm();
    let w = outline.width().to_mm();
    let h = outline.height().to_mm();
    if w <= 0.0 || h <= 0.0 {
        return 0.0;
    }
    let cell_w = w / f64::from(res);
    let cell_h = h / f64::from(res);

    // Per-net pad-bbox in mm.
    let mut net_bbox: HashMap<&str, [f64; 4]> = HashMap::new();
    for fp in board.footprints.values() {
        for pad in &fp.pads {
            let Some(net) = pad.net.as_deref() else {
                continue;
            };
            let c = fp.pad_world_center(pad);
            let x = c.x.to_mm();
            let y = c.y.to_mm();
            let entry = net_bbox.entry(net).or_insert([
                f64::INFINITY,
                f64::INFINITY,
                f64::NEG_INFINITY,
                f64::NEG_INFINITY,
            ]);
            entry[0] = entry[0].min(x);
            entry[1] = entry[1].min(y);
            entry[2] = entry[2].max(x);
            entry[3] = entry[3].max(y);
        }
    }

    let mut counts = vec![0u32; (res * res) as usize];
    for [x0, y0, x1, y1] in net_bbox.values() {
        // Single-pad nets contribute nothing to congestion.
        if x1 - x0 < 1e-6 && y1 - y0 < 1e-6 {
            continue;
        }
        let c0 = (((x0 - ox) / cell_w).floor() as i32).clamp(0, res_i - 1);
        let r0 = (((y0 - oy) / cell_h).floor() as i32).clamp(0, res_i - 1);
        let c1 = (((x1 - ox) / cell_w).floor() as i32).clamp(0, res_i - 1);
        let r1 = (((y1 - oy) / cell_h).floor() as i32).clamp(0, res_i - 1);
        for r in r0..=r1 {
            for c in c0..=c1 {
                counts[(r * res_i + c) as usize] += 1;
            }
        }
    }

    counts
        .iter()
        .map(|&n| if n > 1 { f64::from(n - 1) } else { 0.0 })
        .sum()
}

/// AABB gap in mm: positive = clear separation, negative = overlap
/// depth. Used both for the hard-reject pad-overlap check (gap ≤ 0)
/// and the soft `min_gap_mm` penalty term.
fn aabb_gap_mm(a: pcb_core::Rect, b: pcb_core::Rect) -> f64 {
    let dx = if a.max.x.0 < b.min.x.0 {
        (b.min.x.0 - a.max.x.0) as f64 / 1_000_000.0
    } else if b.max.x.0 < a.min.x.0 {
        (a.min.x.0 - b.max.x.0) as f64 / 1_000_000.0
    } else {
        // Overlap on x — measure penetration depth.
        -((a.max.x.0.min(b.max.x.0) - a.min.x.0.max(b.min.x.0)) as f64) / 1_000_000.0
    };
    let dy = if a.max.y.0 < b.min.y.0 {
        (b.min.y.0 - a.max.y.0) as f64 / 1_000_000.0
    } else if b.max.y.0 < a.min.y.0 {
        (a.min.y.0 - b.max.y.0) as f64 / 1_000_000.0
    } else {
        -((a.max.y.0.min(b.max.y.0) - a.min.y.0.max(b.min.y.0)) as f64) / 1_000_000.0
    };
    if dx >= 0.0 && dy >= 0.0 {
        // Clear: shortest separation along either axis.
        dx.min(dy)
    } else if dx >= 0.0 {
        dx
    } else if dy >= 0.0 {
        dy
    } else {
        // Both axes overlap: ACTUAL collision. Return the larger of
        // the two penetration depths (more "negative") so callers
        // testing `gap <= 0` see this as worse than a touch.
        dx.max(dy)
    }
}

/// Smallest body-to-body gap (mm, margins folded in) between the
/// footprint with `fp_id` and every other footprint on the board.
/// `+INFINITY` when it has no neighbours / no bounds.
fn footprint_min_gap(board: &Board, fp_id: Id, margins: &MarginMap) -> f64 {
    let Some(fp) = board.footprints.get(&fp_id) else {
        return f64::INFINITY;
    };
    let Some(fb) = fp_bounds_with_margin(fp, margins) else {
        return f64::INFINITY;
    };
    board
        .footprints_in_order()
        .filter(|o| o.id != fp_id)
        .filter_map(|o| fp_bounds_with_margin(o, margins).map(|ob| aabb_gap_mm(fb, ob)))
        .fold(f64::INFINITY, f64::min)
}

/// Same as `footprint_min_gap` but for a probe footprint (a candidate
/// at a new position), measured against every board footprint except its
/// own id.
fn probe_min_gap(board: &Board, probe: &Footprint, margins: &MarginMap) -> f64 {
    let Some(pb) = fp_bounds_with_margin(probe, margins) else {
        return f64::INFINITY;
    };
    board
        .footprints_in_order()
        .filter(|o| o.id != probe.id)
        .filter_map(|o| fp_bounds_with_margin(o, margins).map(|ob| aabb_gap_mm(pb, ob)))
        .fold(f64::INFINITY, f64::min)
}

/// Smallest body-to-body gap across EVERY pair on the board (mm). Used
/// to verify a finished placement honours the hard clearance.
pub fn min_pairwise_gap(board: &Board, margins: &MarginMap) -> f64 {
    let fps: Vec<&Footprint> = board.footprints_in_order().collect();
    let mut m = f64::INFINITY;
    for i in 0..fps.len() {
        let Some(a) = fp_bounds_with_margin(fps[i], margins) else {
            continue;
        };
        for b in fps.iter().skip(i + 1) {
            if let Some(bb) = fp_bounds_with_margin(b, margins) {
                m = m.min(aabb_gap_mm(a, bb));
            }
        }
    }
    m
}

/// Quadratic shortfall against `min_gap_mm` for a single pair, mm².
/// 0 if the pair is clear by at least `min_gap_mm`. Margins from
/// `LibraryEntry::placement_margin` are folded in by inflating each
/// footprint's bbox before measuring the gap — so a part with a
/// 1 mm top margin reads "1 mm closer" to anything north of it.
fn pair_gap_penalty(a: &Footprint, b: &Footprint, min_gap_mm: f64, margins: &MarginMap) -> f64 {
    let Some(ab) = fp_bounds_with_margin(a, margins) else {
        return 0.0;
    };
    let Some(bb) = fp_bounds_with_margin(b, margins) else {
        return 0.0;
    };
    let gap = aabb_gap_mm(ab, bb);
    if gap >= min_gap_mm {
        0.0
    } else {
        // Clip negative gaps (actual overlap) to 0 here — the hard
        // reject already prevents accepting overlapping moves, but if
        // the starting layout is already overlapping we don't want
        // an exploding penalty that drowns out HPWL.
        let s = min_gap_mm - gap.max(0.0);
        s * s
    }
}

/// Sum of `pair_gap_penalty` over every pair `(fp_id, other)`. Cheap
/// when called in the SA loop because only one footprint moved per
/// iteration; everything else is reused.
fn footprint_gap_penalty(board: &Board, fp_id: Id, min_gap_mm: f64, margins: &MarginMap) -> f64 {
    let Some(fp) = board.footprints.get(&fp_id) else {
        return 0.0;
    };
    board
        .footprints_in_order()
        .filter(|other| other.id != fp_id)
        .map(|other| pair_gap_penalty(fp, other, min_gap_mm, margins))
        .sum()
}

/// Sum of `pair_gap_penalty` over every unordered pair on the board.
fn total_gap_penalty(board: &Board, min_gap_mm: f64, margins: &MarginMap) -> f64 {
    let fps: Vec<&Footprint> = board.footprints_in_order().collect();
    let mut sum = 0.0;
    for i in 0..fps.len() {
        for j in (i + 1)..fps.len() {
            sum += pair_gap_penalty(fps[i], fps[j], min_gap_mm, margins);
        }
    }
    sum
}

/// True if `probe` (after inflating its bbox by `gap_mm` on every side,
/// plus its library-authored placement margin) would intersect any
/// other footprint's inflated bbox. The placer uses this only for the
/// hard pad-overlap check (`gap_mm = 0`); the soft min-gap preference
/// is enforced via the SA score, not by reject. Folding the margin in
/// here means "do not let the bodies of two parts overlap their
/// keep-outs" is a hard reject — exactly what AI-authored pad-only
/// footprints need when the real part body is wider than the pads.
#[allow(dead_code)]
fn would_overlap(
    board: &Board,
    probe: &Footprint,
    ignore_id: Option<Id>,
    gap_mm: f64,
    margins: &MarginMap,
) -> bool {
    let extra = Length::from_mm(gap_mm);
    let Some(probe_bounds) = fp_bounds_with_margin(probe, margins) else {
        return false;
    };
    let probe_bounds = probe_bounds.expand(extra);
    for fp in board.footprints_in_order() {
        if Some(fp.id) == ignore_id {
            continue;
        }
        if let Some(b) = fp_bounds_with_margin(fp, margins) {
            if probe_bounds.intersects(&b.expand(extra)) {
                return true;
            }
        }
    }
    false
}

/// True if every corner of `probe`'s bbox (margins included) sits
/// inside `outline`. The margin pushes a part off the edge if it has
/// `top_mm`/etc. set — useful for connectors that need clearance from
/// the cut line.
/// Pads fully on-board (no pad copper past the outline).
fn pads_inside_outline(probe: &Footprint, outline: pcb_core::Rect) -> bool {
    let Some(b) = probe.bounds() else {
        return false;
    };
    b.min.x.0 >= outline.min.x.0
        && b.min.y.0 >= outline.min.y.0
        && b.max.x.0 <= outline.max.x.0
        && b.max.y.0 <= outline.max.y.0
}

/// Library placement margin for a probe footprint, if any.
fn margin_for_fp(
    probe: &Footprint,
    margins: &MarginMap,
) -> pcb_core::PlacementMargin {
    match margins.get(&probe.id) {
        Some([t, r, b, l]) => pcb_core::PlacementMargin {
            top_mm: *t,
            right_mm: *r,
            bottom_mm: *b,
            left_mm: *l,
        },
        None => pcb_core::PlacementMargin::default(),
    }
}

#[derive(Debug, Clone)]
enum MoveKind {
    Translate { footprint: Id, new_pos: Point },
    Rotate90 { footprint: Id },
}

fn make_probe(board: &Board, m: &MoveKind) -> (Option<Footprint>, Point, f32) {
    match m {
        MoveKind::Translate { footprint, new_pos } => {
            let Some(fp) = board.footprints.get(footprint) else {
                return (None, Point::ORIGIN, 0.0);
            };
            let mut probe = fp.clone();
            probe.position = *new_pos;
            (Some(probe), fp.position, fp.rotation)
        }
        MoveKind::Rotate90 { footprint } => {
            let Some(fp) = board.footprints.get(footprint) else {
                return (None, Point::ORIGIN, 0.0);
            };
            let mut probe = fp.clone();
            probe.rotation = (probe.rotation + 90.0).rem_euclid(360.0);
            (Some(probe), fp.position, fp.rotation)
        }
    }
}

fn apply_move_in_place(board: &mut Board, m: &MoveKind) {
    match m {
        MoveKind::Translate { footprint, new_pos } => {
            if let Some(fp) = board.footprints.get_mut(footprint) {
                fp.position = *new_pos;
            }
        }
        MoveKind::Rotate90 { footprint } => {
            if let Some(fp) = board.footprints.get_mut(footprint) {
                fp.rotation = (fp.rotation + 90.0).rem_euclid(360.0);
            }
        }
    }
}

fn revert_move_in_place(board: &mut Board, m: &MoveKind, original_pos: Point, original_rot: f32) {
    match m {
        MoveKind::Translate { footprint, .. } => {
            if let Some(fp) = board.footprints.get_mut(footprint) {
                fp.position = original_pos;
            }
        }
        MoveKind::Rotate90 { footprint } => {
            if let Some(fp) = board.footprints.get_mut(footprint) {
                fp.rotation = original_rot;
            }
        }
    }
}

/// Tiny xorshift64* RNG. Self-contained so the placer doesn't pull in
/// `rand` (and we get deterministic results from a u64 seed). Uniform
/// enough for SA's accept/reject — we don't need crypto-grade output.
struct Rng {
    state: u64,
}

impl Rng {
    fn new(seed: u64) -> Self {
        Self { state: seed.max(1) }
    }
    fn next_u64(&mut self) -> u64 {
        let mut x = self.state;
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        self.state = x;
        x.wrapping_mul(2_685_821_657_736_338_717)
    }
    fn next_u32(&mut self) -> u32 {
        (self.next_u64() >> 32) as u32
    }
    fn next_usize(&mut self) -> usize {
        self.next_u64() as usize
    }
    /// Uniform [0, 1).
    fn next_f64(&mut self) -> f64 {
        // 53 bits of mantissa; standard recipe.
        (self.next_u64() >> 11) as f64 * (1.0 / (1u64 << 53) as f64)
    }
}

fn seed_from_clock() -> u64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0xfeed_face_dead_beef)
}
