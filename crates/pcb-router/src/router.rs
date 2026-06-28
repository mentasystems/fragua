//! Driver that ties the grid and A* together.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::sync::Arc;

use pcb_core::{Board, CopperLayer, Length, Point, Rect, Schematic, Trace, Via};

use crate::astar::search;
use crate::grid::{CostMap, Grid, GridPoint};

#[derive(Debug, Clone)]
pub struct RouteOptions {
    /// Cell pitch on the routing grid. 0.25 mm is the default sweet
    /// spot for SMD-only boards: fine enough for 0.5 mm pin pitch,
    /// coarse enough for grids of ~250 × 250 cells per layer to stay
    /// fast.
    pub cell: Length,
    /// Default trace width laid down by the router. Per-net entries in
    /// `net_overrides` win when set.
    pub trace_width: Length,
    /// Default clearance added on every side of pad obstacles. Per-net
    /// entries in `net_overrides` win when set; the grid is stamped at
    /// the *max* clearance across all overrides + this default so a
    /// stricter net's clearance is never undersold.
    pub clearance: Length,
    /// Cost of punching a via, expressed as a multiplier on the
    /// per-cell base step. Higher = router prefers single-layer
    /// detours. Internally scaled to the search's fixed-point
    /// Euclidean cost domain.
    pub via_cost: u32,
    /// Via geometry produced when the path flips layers.
    pub via_drill: Length,
    pub via_diameter: Length,
    /// Per-net rule overrides keyed by net name. Built by the caller
    /// from the schematic's `NetClass` definitions; the router stays
    /// schematic-agnostic and just consults this map.
    ///
    /// **Deprecated** in favour of `schematic` — kept for one release
    /// so existing callers (router-tune, tests) compile unchanged.
    #[doc(hidden)]
    pub net_overrides: HashMap<String, NetOverride>,
    /// Optional schematic reference. When set, the router consults
    /// `schematic.resolved_for_net(net)` for per-net trace width and
    /// clearance — superseding `net_overrides`. The arc is cheap to
    /// clone and keeps the router lock-free with respect to the
    /// schematic.
    pub schematic: Option<Arc<Schematic>>,
    /// If `Some`, use this exact net order as the first-pass ordering instead
    /// of the built-in "fewest pads first" heuristic. Net names not present
    /// in the board are silently dropped; nets in the board but missing from
    /// the override are appended at the end in default order. The rip-up-and-
    /// reroute loop is unaffected — it still reorders on subsequent passes.
    pub initial_net_order: Option<Vec<String>>,
    /// Greedy-search weight `W` applied to the A* heuristic (`f = g + W·h`).
    /// `1.0` = admissible/optimal A* (default — byte-identical to the
    /// historical router). Values in `1.25..=1.5` collapse the near-tied-f
    /// frontier on long, board-spanning nets 5–30× at a few-percent
    /// path-length cost. The weighting is **size-gated inside the search**:
    /// short connections (every tight-detour test, fanout/diff-pair
    /// end-cap) stay at `W=1.0` and provably optimal, so only the long
    /// searches — exactly where the frontier explosion lives — pay the
    /// small detour for the big speed win. Orthogonal to clearance
    /// stamping, so it never changes the DRC/CLEAN outcome, only latency.
    pub heuristic_weight: f64,
}

/// Per-net rule overrides — fields default to "use the global
/// `RouteOptions` value" when `None`.
#[derive(Debug, Clone, Default)]
pub struct NetOverride {
    pub trace_width: Option<Length>,
    pub clearance: Option<Length>,
}

impl Default for RouteOptions {
    fn default() -> Self {
        Self {
            cell: Length::from_mm(0.25),
            trace_width: Length::from_mm(0.25),
            // 0.4 mm gives a 2-cell halo around traces and pads on a
            // 0.25 mm grid: even at the closest legal spacing, two
            // foreign-net traces have at least one empty cell of gap
            // between them, so they never appear visually pegged.
            clearance: Length::from_mm(0.40),
            via_cost: 8,
            via_drill: Length::from_mm(0.3),
            via_diameter: Length::from_mm(0.6),
            net_overrides: HashMap::new(),
            schematic: None,
            initial_net_order: None,
            heuristic_weight: 1.0,
        }
    }
}

/// Minimum trace width (mm) the router lays on a *power* net when the
/// net has no explicit class/override width. Power distribution carries
/// real current and wants low impedance, so a backbone thinner than this
/// is almost never what the designer intends. A net that explicitly sets
/// a narrower width via a class still wins — this is only a floor for the
/// "router picked the default" case.
const POWER_MIN_TRACE_WIDTH_MM: f64 = 0.50;

/// Classify a net as power/ground by name. Keyed on the conventional
/// rail names so the router can widen them automatically without the
/// designer having to declare a `class power` for every board. Matches
/// the exact rail or a name that starts with it (e.g. `+3V3`, `3V3_MCU`,
/// `VBUS_IN`, `VCC_IO`).
pub fn is_power_net(net: &str) -> bool {
    let u = net.to_ascii_uppercase();
    const RAILS: &[&str] = &[
        "GND", "VBUS", "+3V3", "3V3", "+5V", "5V", "+1V", "1V", "VCC", "VDD", "VDDA", "VIN", "VSYS",
        "VBAT", "PWR", "+12V", "12V",
    ];
    RAILS.iter().any(|p| u == *p || u.starts_with(p))
}

/// Helper: resolve `(trace_width, clearance)` for `net` honouring
/// `opts.schematic` first, then `opts.net_overrides`, then the global
/// defaults on `opts`. Centralises the precedence so the grid stamp,
/// per-net layout, and `compute_region` stay in sync.
///
/// Power nets get a width *floor* (`POWER_MIN_TRACE_WIDTH_MM`) applied
/// last: a power rail that resolved to the bare global default is widened
/// to the floor, while a net that explicitly asked for a specific width
/// (via a class or override) keeps it.
fn effective_net_rules(opts: &RouteOptions, net: &str) -> (Length, Length) {
    // `explicit` tracks whether the width came from a real class/override
    // (respect it) or fell through to the global default (floor it).
    let (mut w, c, explicit_width) = {
        if let Some(sch) = opts.schematic.as_ref() {
            // Only consult the schematic when the net actually has a class —
            // otherwise we'd shadow the override map below for every net.
            if sch.class_for_net(net).is_some() {
                let res = sch.resolved_for_net(
                    net,
                    opts.trace_width,
                    opts.clearance,
                    opts.via_diameter,
                    opts.via_drill,
                );
                // A class whose width differs from the default is an
                // explicit choice; one that equals the default is just
                // inheriting it.
                let explicit = res.trace_width.0 != opts.trace_width.0;
                (res.trace_width, res.clearance, explicit)
            } else {
                resolve_from_overrides(opts, net)
            }
        } else {
            resolve_from_overrides(opts, net)
        }
    };

    if !explicit_width && is_power_net(net) {
        let floor = Length::from_mm(POWER_MIN_TRACE_WIDTH_MM);
        if floor.0 > w.0 {
            w = floor;
        }
    }
    (w, c)
}

/// Quantization guard (in cells) folded into every search-time clearance
/// radius. A Theta* any-angle segment can pass up to ~0.5 cell off its
/// Bresenham raster, and a bare pad/trace cell represents copper up to
/// ~0.5 cell from the cell point, so the true edge-to-edge distance can
/// fall up to ~1 cell short of the disk-measured distance. Adding one
/// cell to the clearance radius absorbs that, keeping the DRC's true-
/// geometry clearance honest at a coarse grid. (Costs ~1 cell of extra
/// separation — at cell 0.20 that's 0.20 mm — traded for zero collisions.)
const CLEARANCE_GUARD_CELLS: i32 = 1;

/// Ceil-divide `num_nm` by `cell_nm` on the underlying nm integers,
/// returning a cell count. Used to size per-net clearance / copper /
/// via radii so the discrete grid never undersells a distance (always
/// rounds the radius up). Callers apply `.max(1)` (clearance / via-safe)
/// or `.max(0)` (copper) as appropriate.
fn ceil_cells(num_nm: i64, cell_nm: i64) -> i32 {
    let raw = (num_nm + cell_nm.max(1) - 1) / cell_nm.max(1);
    i32::try_from(raw).unwrap_or(1)
}

/// `(trace_width, clearance, explicit_width)` from the per-net override
/// map, falling back to the global defaults.
fn resolve_from_overrides(opts: &RouteOptions, net: &str) -> (Length, Length, bool) {
    let ov = opts.net_overrides.get(net);
    let w = ov.and_then(|o| o.trace_width);
    let explicit = w.is_some();
    let c = ov
        .and_then(|o| o.clearance)
        .unwrap_or(opts.clearance);
    (w.unwrap_or(opts.trace_width), c, explicit)
}

#[derive(Debug, Clone)]
pub enum Outcome {
    Ok {
        trace_segments: usize,
        vias: usize,
        /// Sum of straight-segment lengths laid down for this net, mm.
        length_mm: f64,
        /// Sum of Manhattan distances from the chosen hub pad to every
        /// other pad in the net, mm. This is the lower bound a perfect
        /// orthogonal star tree could hit. `length_mm / lower_bound_mm`
        /// is the "detour ratio" — 1.0 = optimal, > 1.5 = the router
        /// (or the placement) made the net work harder than it should.
        lower_bound_mm: f64,
    },
    Failed {
        reason: String,
    },
}

#[derive(Debug, Clone)]
pub struct RouteReport {
    pub per_net: Vec<(String, Outcome)>,
    pub trace_count: usize,
    pub via_count: usize,
    /// Sum of `length_mm` over every successfully-routed net.
    pub total_length_mm: f64,
    /// Sum of `lower_bound_mm` over the same set.
    pub total_lower_bound_mm: f64,
    /// How many full rip-up-and-reroute passes the driver performed
    /// before settling on this report. 1 = single pass (no RR&R needed
    /// or RR&R didn't help); 2..=`MAX_RR_ITERATIONS` = RR&R kicked in.
    pub iterations: usize,
    /// Plain-text suggestions for the agent: which footprints to move
    /// to fix the still-failing nets. Generated post-hoc from the best
    /// report — empty when every net routed.
    pub hints: Vec<String>,
}

/// Hard cap on rip-up-and-reroute passes. Each pass clears all routing
/// and re-runs the per-net A* loop with a different ordering, so the
/// cost is roughly linear in this constant. 3 is empirically enough to
/// recover most fixable failures without exploding wall-clock time.
const MAX_RR_ITERATIONS: usize = 3;

/// A net is "bad" — and pulled to the front of the next iteration's
/// order — if its detour ratio exceeds this threshold or it failed
/// outright. The lower bound is HPWL (Manhattan), but Theta* lays
/// Euclidean traces that can be shorter than Manhattan even on a
/// detour; so the ratio is looser than the number suggests. 2.2 catches
/// real failures without flagging healthy diagonal runs.
const BAD_DETOUR_RATIO: f64 = 2.2;

/// Negotiated congestion: per-cell bias added to the corridor around a
/// failed net's pads on the next iteration. Compared to a base step
/// cost of 1 per cell, 4 makes the corridor 5× more expensive — strong
/// enough to push easy nets to detour, weak enough that the bad net
/// itself (which routes first under RR&R) still uses the corridor.
const CONGESTION_BUMP_FAILED: u32 = 4;
/// Lighter bump for a net that succeeded but took a long detour: the
/// "blame" is fuzzier so the bias is too.
const CONGESTION_BUMP_INEFFICIENT: u32 = 2;
/// Cells around a bad net's pad bbox to mark as congested. ~1.5 mm at
/// the default 0.25 mm cell pitch — about a trace width plus clearance,
/// so other nets see the whole "corridor" as expensive, not just the
/// pads themselves.
const CONGESTION_RADIUS_CELLS: i32 = 6;
/// Hard cap on accumulated bias per cell. Beyond this the bias would
/// dominate the heuristic and A* would refuse the cell even when it's
/// the only path; keep it bounded.
const CONGESTION_MAX: u32 = 32;

/// Route every net found in the board's pad assignments. Mutates
/// `board` in place: existing routing is cleared, new routing is laid.
///
/// The driver runs up to `MAX_RR_ITERATIONS` rip-up-and-reroute passes.
/// After each pass, any net that failed or whose detour ratio exceeds
/// `BAD_DETOUR_RATIO` is pulled to the front of the order for the next
/// pass — those bad nets get pristine corridors before the easy nets
/// claim the obvious paths. The "best" report (fewest failures, then
/// shortest total wire) wins; if no iteration improves on the first,
/// the first wins and the board is laid back to its first-pass state.
pub fn route(board: &mut Board, opts: &RouteOptions) -> RouteReport {
    let nets = collect_nets(board);
    // Fanout pre-pass: drop a via-in-pad on any fine-pitch pad that can't
    // escape on its own layer, so the router can reach it from an inner
    // layer. No-op on 2-layer boards (nowhere to fan out to).
    let fanout = crate::fanout::plan_fanout(board, opts);
    if nets.is_empty() {
        board.clear_routing();
        return RouteReport {
            per_net: Vec::new(),
            trace_count: 0,
            via_count: 0,
            total_length_mm: 0.0,
            total_lower_bound_mm: 0.0,
            iterations: 0,
            hints: Vec::new(),
        };
    }

    // First-pass order: caller override (e.g. the GA tuner) wins;
    // otherwise easy nets (fewest pads) first. Same heuristic as before
    // when no override is supplied — gets the unconstrained nets to lay
    // copper before the hairy ones contend for space.
    let mut order: Vec<String> = if let Some(custom) = opts.initial_net_order.as_ref() {
        let valid: HashSet<&str> = nets.keys().map(String::as_str).collect();
        let mut seen: HashSet<String> = HashSet::new();
        let mut out: Vec<String> = Vec::with_capacity(nets.len());
        for n in custom {
            if valid.contains(n.as_str()) && !seen.contains(n.as_str()) {
                seen.insert(n.clone());
                out.push(n.clone());
            }
        }
        let mut leftover: Vec<String> = nets
            .keys()
            .filter(|n| !seen.contains(n.as_str()))
            .cloned()
            .collect();
        leftover.sort_by_key(|n| nets.get(n).map_or(0, Vec::len));
        out.extend(leftover);
        out
    } else {
        let mut o: Vec<String> = nets.keys().cloned().collect();
        o.sort_by_key(|n| nets.get(n).map_or(0, Vec::len));
        o
    };

    // Fine-pitch escape nets first. A net with a fanned-out pad has to
    // thread the congested escape channel of a fine-pitch part (USB-C row,
    // QFN edge); if the easy 2-pin nets route first they claim the channel
    // lanes and box the escapes out. Pull every net that owns a fanned pad
    // to the front (preserving relative order) so the hard escapes get a
    // clear channel before the easy nets fill in around them.
    if !fanout.through_pads.is_empty() {
        let fanned_nets: HashSet<String> = nets
            .iter()
            .filter(|(_, pads)| pads.iter().any(|p| fanout.through_pads.contains(&p.pad_ref)))
            .map(|(n, _)| n.clone())
            .collect();
        if !fanned_nets.is_empty() {
            let (mut hard, easy): (Vec<String>, Vec<String>) =
                order.into_iter().partition(|n| fanned_nets.contains(n));
            hard.extend(easy);
            order = hard;
        }
    }

    // Diff-pair adjacency: any net whose class declares `diff_pair_with = X`
    // must route immediately AFTER X so the "follow" mode can read X's
    // already-laid geometry. Stable in the rest of the ordering.
    order = reorder_for_diff_pairs(order, opts);

    // Cost map shared across iterations: starts at 0, accumulates bias
    // around the corridors of failed/inefficient nets so the next pass
    // detours easy nets out of those corridors. Built from a one-shot
    // grid only for its dims; the actual obstacle grid is built fresh
    // per pass inside `route_pass`.
    let region = compute_region(board, opts);
    let layer_count = board.stackup.layer_count();
    let mut cost_map = Grid::with_layers(region, opts.cell, layer_count).new_cost_map();

    let mut best: Option<(Board, RouteReport)> = None;
    let mut last_order: Option<Vec<String>> = None;
    let mut iterations_run = 0;
    for _ in 1..=MAX_RR_ITERATIONS {
        // Stop early if reordering produced nothing new — no point
        // re-running the exact same sequence twice.
        if last_order.as_ref() == Some(&order) {
            break;
        }
        last_order = Some(order.clone());
        iterations_run += 1;

        let mut work = board.clone();
        work.clear_routing();
        let report = route_pass(&mut work, &nets, &order, opts, &cost_map, &fanout);

        let take_it = match &best {
            None => true,
            Some((_, prev)) => report_is_better(&report, prev),
        };
        if take_it {
            best = Some((work, report.clone()));
        }

        // Identify bad nets for next iteration. Failed nets always go
        // to the front; inefficient ones follow. Everything else keeps
        // its relative position so we don't rotate the easy nets too.
        let mut failed: Vec<String> = Vec::new();
        let mut inefficient: Vec<String> = Vec::new();
        for (name, outcome) in &report.per_net {
            match outcome {
                Outcome::Failed { .. } => failed.push(name.clone()),
                Outcome::Ok {
                    length_mm,
                    lower_bound_mm,
                    ..
                } if *lower_bound_mm > 0.0 && length_mm / lower_bound_mm > BAD_DETOUR_RATIO => {
                    inefficient.push(name.clone());
                }
                _ => {}
            }
        }
        if failed.is_empty() && inefficient.is_empty() {
            break;
        }

        // Negotiated congestion: bump the corridor around each bad
        // net's pads so easy nets in the NEXT pass detour around it
        // and leave the bad net a clear shot. Bias scales with
        // iteration index — if a net survives its first bump, the
        // next iteration applies a stronger one (capped at
        // `CONGESTION_MAX`) until A* finds a way through.
        let snap_grid = Grid::with_layers(region, opts.cell, layer_count);
        let bump_factor = iterations_run as u32; // 1, 2, 3...
        for name in &failed {
            bump_corridor(
                &snap_grid,
                &mut cost_map,
                nets.get(name).map_or(&[], Vec::as_slice),
                CONGESTION_BUMP_FAILED * bump_factor,
            );
        }
        for name in &inefficient {
            bump_corridor(
                &snap_grid,
                &mut cost_map,
                nets.get(name).map_or(&[], Vec::as_slice),
                CONGESTION_BUMP_INEFFICIENT * bump_factor,
            );
        }

        let bad: std::collections::HashSet<String> =
            failed.iter().chain(inefficient.iter()).cloned().collect();
        let rest: Vec<String> = order
            .iter()
            .filter(|n| !bad.contains(*n))
            .cloned()
            .collect();
        order = failed.into_iter().chain(inefficient).chain(rest).collect();
    }

    let (best_work, mut best_report) = best.expect("at least one iteration ran");
    best_report.iterations = iterations_run;
    best_report.hints = generate_hints(&best_report, &nets);
    // Stamp the winning routing onto the caller's board.
    board.clear_routing();
    for trace in best_work.traces {
        board.add_trace(trace);
    }
    for via in best_work.vias {
        board.add_via(via);
    }
    // Fanout vias are fixed geometry, not part of the rip-up/reroute
    // search, so they're added once to the final board.
    for via in &fanout.vias {
        board.add_via(via.clone());
    }
    // Post-passes: auto-stitching vias for pours with a Grid policy,
    // then length matching for nets that ask for it. Length matching
    // requires a schematic (it's the source of target lengths).
    crate::stitching::add_stitching_vias(board, opts);
    if let Some(sch) = opts.schematic.as_ref() {
        let _ = crate::length_match::length_match_pass(board, sch.as_ref());
    }
    best_report
}

/// Look at the report and emit human-readable suggestions for the
/// agent. For every net that is still failing or whose detour ratio is
/// pathological (>2× HPWL), pick the *outlier* pad — the one whose
/// removal would shrink the bbox the most — and suggest moving its
/// footprint closer to the rest of the net. This is heuristic but
/// usually right: the failing/detoured nets are the ones with one or
/// two pads geographically far from the cluster, and moving those is
/// the lowest-effort fix.
fn generate_hints(report: &RouteReport, nets: &BTreeMap<String, Vec<NetPadInfo>>) -> Vec<String> {
    let mut hints = Vec::new();
    for (net_name, outcome) in &report.per_net {
        let troubled = match outcome {
            Outcome::Failed { .. } => true,
            Outcome::Ok {
                length_mm,
                lower_bound_mm,
                ..
            } if *lower_bound_mm > 0.0 && length_mm / lower_bound_mm > 2.0 => true,
            _ => false,
        };
        if !troubled {
            continue;
        }
        let Some(pads) = nets.get(net_name) else {
            continue;
        };
        if pads.len() < 2 {
            continue;
        }
        // Outlier = pad with max sum-Manhattan distance to all other
        // pads on the net. A central cluster has tight pairwise sums;
        // the outlier sticks out and dominates the bbox.
        let outlier = pads.iter().max_by_key(|p| {
            pads.iter()
                .map(|q| {
                    (p.center.x.0 - q.center.x.0).unsigned_abs()
                        + (p.center.y.0 - q.center.y.0).unsigned_abs()
                })
                .sum::<u64>()
        });
        if let Some(o) = outlier {
            // Reference is "REF.PIN"; the footprint reference is the
            // bit before the dot. Tell the agent that piece.
            let fp_ref = o
                .pad_ref
                .split_once('.')
                .map_or(o.pad_ref.as_str(), |(r, _)| r);
            let kind = if matches!(outcome, Outcome::Failed { .. }) {
                "failed"
            } else {
                "detoured"
            };
            hints.push(format!(
                "net {net_name} {kind}: {fp_ref} (pad at {:.1},{:.1} mm) is the outlier — moving it closer to the rest of the net usually frees the corridor",
                o.center.x.to_mm(),
                o.center.y.to_mm(),
            ));
        }
    }
    hints
}

/// Heuristic: fewer failed nets > shorter total wire > fewer vias.
fn report_is_better(a: &RouteReport, b: &RouteReport) -> bool {
    let fails_a = count_failed(a);
    let fails_b = count_failed(b);
    if fails_a != fails_b {
        return fails_a < fails_b;
    }
    if (a.total_length_mm - b.total_length_mm).abs() > 1e-6 {
        return a.total_length_mm < b.total_length_mm;
    }
    a.via_count < b.via_count
}

fn count_failed(r: &RouteReport) -> usize {
    r.per_net
        .iter()
        .filter(|(_, o)| matches!(o, Outcome::Failed { .. }))
        .count()
}

/// Routing region. If the board has an outline, the router stays
/// *inside* it with an inset that keeps the centre of the widest
/// copper feature (a via) far enough from Edge.Cuts to satisfy the
/// DRC's edge clearance check (default 0.3 mm). Without an outline we
/// fall back to the content bbox expanded by 5 mm so the router still
/// has slack to find paths. Pulled out so `route()` can size the
/// negotiated-congestion cost map before the first pass.
fn compute_region(board: &Board, opts: &RouteOptions) -> Rect {
    let edge_clearance = Length::from_mm(0.3);
    // Widest copper feature across the *effective* trace widths (max
    // of default and any class override) and the via diameter. Used
    // to inset the routing region so even the fattest power trace
    // sits clear of the board edge.
    let mut widest = opts.trace_width.0.max(opts.via_diameter.0);
    for o in opts.net_overrides.values() {
        if let Some(w) = o.trace_width {
            widest = widest.max(w.0);
        }
    }
    if let Some(sch) = opts.schematic.as_ref() {
        for class in sch.net_classes.values() {
            if let Some(w_mm) = class.trace_width_mm {
                widest = widest.max(Length::from_mm(w_mm).0);
            }
        }
    }
    let half_widest = Length(widest / 2);
    let mut outline_inset = edge_clearance + half_widest;
    // Rounded outline cuts inward at each corner by `r × (1 − 1/√2)`
    // (~0.293 r). The straight sides of the rounded outline still
    // line up with the rect, so only the corners would otherwise
    // intrude into the router's region. Easiest fix: shrink the
    // entire region by that worst-case extra inset — costs a few mm
    // of routable area near the sides, but guarantees no copper
    // crosses the rounded edge.
    if board.outline_corner_radius.0 > 0 {
        let corner_extra_nm = (board.outline_corner_radius.0 as f64 * 0.293).ceil() as i64;
        outline_inset = outline_inset + Length(corner_extra_nm);
    }
    match board.outline {
        Some(r) => Rect::from_corners(
            Point::new(r.min.x + outline_inset, r.min.y + outline_inset),
            Point::new(r.max.x - outline_inset, r.max.y - outline_inset),
        ),
        None => match board.content_bounds() {
            Some(r) => r.expand(Length::from_mm(5.0)),
            None => Rect::from_corners(
                Point::new(Length::from_mm(0.0), Length::from_mm(0.0)),
                Point::new(Length::from_mm(50.0), Length::from_mm(50.0)),
            ),
        },
    }
}

/// One full routing pass: lay every net (in `order`) onto a freshly
/// cleared `board` and return the per-net outcomes. The board's
/// routing must already be cleared by the caller.
fn route_pass(
    board: &mut Board,
    nets: &BTreeMap<String, Vec<NetPadInfo>>,
    order: &[String],
    opts: &RouteOptions,
    cost_map: &CostMap,
    fanout: &crate::fanout::FanoutPlan,
) -> RouteReport {
    let net_id_of: HashMap<String, u32> = order
        .iter()
        .enumerate()
        .map(|(i, n)| (n.clone(), i as u32))
        .collect();

    let region = compute_region(board, opts);
    let mut grid = Grid::with_layers(region, opts.cell, board.stackup.layer_count());

    let net_id_lookup = |n: &str| net_id_of.get(n).copied();
    // Layered stamping, broad-to-narrow: bodies block the area each
    // footprint occupies, keepouts block any user-marked region, pads
    // overwrite the cells they actually own so they stay reachable.
    grid.stamp_bodies(board);
    grid.stamp_keepouts(board);
    // Stamp pads BARE (no clearance inflation): a pad cell holds its true
    // copper extent only. Edge-to-edge clearance to a pad is enforced at
    // search time by each net's own clearance disk — exact at any grid
    // pitch and never over-inflating thin signals (which is what used to
    // box fine-pitch pins in). No-net pads stamp the FOREIGN_NET sentinel
    // (see `stamp_pads`) so they still demand clearance.
    grid.stamp_pads(board, &net_id_lookup, Length(0));
    // Fanned-out pads become through-hole landing zones at their VIA: the
    // via-in-pad ties every layer together, so stamp a DrilledPad disk
    // (the via barrel's footprint) on all layers at the via position. The
    // router can then reach it from an inner layer where there's room,
    // instead of being trapped on the congested surface between fine-pitch
    // neighbours. Crucially we stamp only the via disk, NOT the whole SMD
    // pad rect on every layer: on the inner layers the SMD pad does not
    // exist (only the barrel does), so walling off the full rect there
    // would block the very approach lanes the inner-layer escape needs.
    if !fanout.through_pads.is_empty() {
        // The fanout via is the JLCPCB-minimum 0.30 mm; its copper radius
        // in cells, floored at one cell so the landing is always at least
        // a single reachable DrilledPad cell.
        let fanout_via_copper_cells =
            ceil_cells(Length::from_mm(0.15).0, opts.cell.0).max(1);
        for fp in board.footprints_in_order() {
            for pad in &fp.pads {
                let key = format!("{}.{}", fp.reference, pad.number);
                if !fanout.through_pads.contains(&key) {
                    continue;
                }
                let Some(id) = pad.net.as_deref().and_then(&net_id_lookup) else {
                    continue;
                };
                let via_pos = fanout
                    .via_positions
                    .get(&key)
                    .copied()
                    .unwrap_or_else(|| fp.pad_world_center(pad));
                grid.stamp_drilled_disk(via_pos, fanout_via_copper_cells, id);
            }
        }
    }
    // Via copper radius (in cells): the via's own half-diameter, stamped
    // bare on every layer. Independent of trace width, so computed once.
    let via_copper_cells = ceil_cells(opts.via_diameter.0 / 2, opts.cell.0).max(0);

    let mut per_net = Vec::with_capacity(nets.len());
    let mut total_traces = 0;
    let mut total_vias = 0;
    let mut total_length_mm = 0.0_f64;
    let mut total_lower_bound_mm = 0.0_f64;

    // Nets that already have a copper pour on at least one layer
    // skip the router entirely — the pour itself is the electrical
    // connection, so adding traces is redundant copper that just
    // clutters the board.
    let pour_nets: std::collections::HashSet<String> =
        board.pours.iter().map(|p| p.net.clone()).collect();

    for net_name in order {
        let Some(pad_points) = nets.get(net_name) else {
            continue;
        };
        let net_id = net_id_of[net_name];
        if pour_nets.contains(net_name) {
            per_net.push((
                net_name.clone(),
                Outcome::Ok {
                    trace_segments: 0,
                    vias: 0,
                    length_mm: 0.0,
                    lower_bound_mm: 0.0,
                },
            ));
            continue;
        }
        if pad_points.len() < 2 {
            per_net.push((
                net_name.clone(),
                Outcome::Ok {
                    trace_segments: 0,
                    vias: 0,
                    length_mm: 0.0,
                    lower_bound_mm: 0.0,
                },
            ));
            continue;
        }

        // Differential-pair follow attempt. If this net's class names
        // a partner that already has traces, try to lay parallel
        // geometry first. On success we skip the normal Theta* loop
        // for this net; on failure we log and fall through.
        let (net_trace_width_early, _) = effective_net_rules(opts, net_name);
        if let Some(partner) = diff_pair_partner(opts, net_name) {
            let partner_traces: Vec<Trace> = board
                .traces
                .iter()
                .filter(|t| t.net == partner)
                .cloned()
                .collect();
            if !partner_traces.is_empty() {
                let gap_mm = opts
                    .schematic
                    .as_ref()
                    .and_then(|s| s.class_for(net_name).diff_gap_mm)
                    .unwrap_or(0.2);
                match try_diff_pair_follow(
                    board,
                    &mut grid,
                    pad_points,
                    &partner_traces,
                    net_name,
                    net_id,
                    net_trace_width_early,
                    gap_mm,
                    opts,
                    via_copper_cells,
                    cost_map,
                ) {
                    Ok((segs, vias, length_mm)) => {
                        total_traces += segs;
                        total_vias += vias;
                        total_length_mm += length_mm;
                        total_lower_bound_mm += hpwl_mm(pad_points);
                        per_net.push((
                            net_name.clone(),
                            Outcome::Ok {
                                trace_segments: segs,
                                vias,
                                length_mm,
                                lower_bound_mm: hpwl_mm(pad_points),
                            },
                        ));
                        continue;
                    }
                    Err(reason) => {
                        eprintln!(
                            "diff_pair.fallback: net={net_name} reason={reason}"
                        );
                    }
                }
            }
        }

        // Lower bound = HPWL: half-perimeter of the pad bounding box,
        // mm. The minimum wire length any tree connecting these pads
        // can use, regardless of topology. Same metric the DRC reports
        // so router and DRC agree on what "optimal" means.
        let net_lower_bound_mm = hpwl_mm(pad_points);

        // Pick the geographically central pad as the seed: minimum sum
        // of Manhattan distances to every other pad. With multi-source
        // A* the hub is no longer mandatory — any same-net cell is a
        // search source — but the spoke ordering "closest to seed
        // first" still helps build a tight Prim-style tree.
        // Prefer a NON-fanned-out pad as the seed when one exists: the
        // seed anchors the trunk, and a wide trunk emanating from a
        // fine-pitch fanout pad would short its neighbours. Among the
        // eligible pads pick the geographically central one.
        let eligible: Vec<usize> = {
            let non_fanout: Vec<usize> = (0..pad_points.len())
                .filter(|&i| !fanout.through_pads.contains(&pad_points[i].pad_ref))
                .collect();
            if non_fanout.is_empty() {
                (0..pad_points.len()).collect()
            } else {
                non_fanout
            }
        };
        let seed_idx = *eligible
            .iter()
            .min_by_key(|&&i| {
                pad_points
                    .iter()
                    .enumerate()
                    .filter(|(j, _)| *j != i)
                    .map(|(_, q)| {
                        let p = pad_points[i].center;
                        let q = q.center;
                        (p.x.0 - q.x.0).unsigned_abs() + (p.y.0 - q.y.0).unsigned_abs()
                    })
                    .sum::<u64>()
            })
            .unwrap_or(&0);
        let seed = pad_points[seed_idx].clone();
        // For a fanned-out pad, aim at the via-in-pad, not the pad centre:
        // the via (possibly slid along the pad) is the only point where the
        // inner-layer copper exists, so it is where the search must land.
        let route_point = |p: &NetPadInfo| -> Point {
            fanout
                .via_positions
                .get(&p.pad_ref)
                .copied()
                .unwrap_or(p.center)
        };
        let seed_grid = grid.snap(route_point(&seed), seed.layer);
        let seed_is_fanout = fanout.through_pads.contains(&seed.pad_ref);

        // Resolve this net's trace width and clearance: schematic class
        // first, then per-net override, then the global default.
        let (net_trace_width, net_clearance) = effective_net_rules(opts, net_name);
        // Via-safety radius is per net (via geometry is fixed; only the
        // net's clearance varies). A via's copper extends `via_diameter/2`
        // and must keep `clearance` to foreign copper — whose own
        // half-width is already baked into its bare stamp, so no extra
        // term is needed here.
        let via_safe_radius =
            ceil_cells(opts.via_diameter.0 / 2 + net_clearance.0, opts.cell.0).max(1);

        let mut net_segments = 0usize;
        let mut net_vias = 0usize;
        let mut net_length_mm = 0.0_f64;
        let mut failed = false;
        // The net's already-laid trace cells, accumulated as each spoke
        // is routed. Seeds the multi-source search (Prim/Steiner growth)
        // without rescanning the whole grid — the key to fine-grid speed.
        let mut net_trace_cells: Vec<GridPoint> = Vec::new();
        // Spokes ordered by distance to seed (closest first). After
        // each spoke is laid, multi-source A* will pick whichever
        // existing same-net cell is closest to the next spoke — so the
        // tree grows Prim-style, not star.
        let mut spokes_sorted: Vec<NetPadInfo> = pad_points
            .iter()
            .enumerate()
            .filter(|(i, _)| *i != seed_idx)
            .map(|(_, p)| p.clone())
            .collect();
        spokes_sorted.sort_by_key(|q| {
            (seed.center.x.0 - q.center.x.0).unsigned_abs()
                + (seed.center.y.0 - q.center.y.0).unsigned_abs()
        });
        for spoke in spokes_sorted {
            let spoke_grid = grid.snap(route_point(&spoke), spoke.layer);
            // Neck a spoke down to the default (signal) width when either
            // end is a fanned-out fine-pitch pad. A 0.5 mm power trace
            // can't physically enter a 0.30 mm connector pin without
            // shorting the 0.5 mm-pitch neighbour, so the entry necks —
            // exactly what a hand layout does. The trunk between regular
            // pads keeps the full power width.
            let spoke_is_fanout = fanout.through_pads.contains(&spoke.pad_ref);
            let spoke_width = if seed_is_fanout || spoke_is_fanout {
                Length(net_trace_width.0.min(opts.trace_width.0))
            } else {
                net_trace_width
            };
            // Per-trace clearance + copper radii, from this spoke's
            // (possibly necked) width. `clr_cells` drives the search-time
            // clearance disk; `copper_cells` the bare-copper stamp.
            let clr_cells = (ceil_cells(net_clearance.0 + spoke_width.0 / 2, opts.cell.0)
                + CLEARANCE_GUARD_CELLS)
                .max(1);
            let copper_cells = ceil_cells(spoke_width.0 / 2, opts.cell.0).max(0);
            let Some(result) = search(
                &grid,
                seed_grid,
                net_id,
                opts.via_cost,
                spoke_grid,
                via_safe_radius,
                clr_cells,
                cost_map,
                &net_trace_cells,
                opts.heuristic_weight,
            ) else {
                per_net.push((
                    net_name.clone(),
                    Outcome::Failed {
                        reason: format!(
                            "no path to pad {} at ({:.2}, {:.2}) mm",
                            spoke.pad_ref,
                            spoke.center.x.to_mm(),
                            spoke.center.y.to_mm(),
                        ),
                    },
                ));
                failed = true;
                break;
            };
            let (segs, vias, length_mm) = lay_path(
                board,
                &mut grid,
                &result.path,
                net_name,
                net_id,
                opts,
                copper_cells,
                via_copper_cells,
                spoke_width,
                Some(route_point(&spoke)),
            );
            net_segments += segs;
            net_vias += vias;
            net_length_mm += length_mm;
            // Record this spoke's path cells as future search sources so
            // the next spoke branches off the nearest point of the tree.
            for w in result.path.windows(2) {
                if w[0].layer == w[1].layer {
                    net_trace_cells.extend(grid.line_cells(w[0], w[1]));
                }
            }
            // The spoke's own pad cell joins the tree too.
            net_trace_cells.push(spoke_grid);
        }
        if !failed {
            total_traces += net_segments;
            total_vias += net_vias;
            total_length_mm += net_length_mm;
            total_lower_bound_mm += net_lower_bound_mm;
            per_net.push((
                net_name.clone(),
                Outcome::Ok {
                    trace_segments: net_segments,
                    vias: net_vias,
                    length_mm: net_length_mm,
                    lower_bound_mm: net_lower_bound_mm,
                },
            ));
        }
    }

    RouteReport {
        per_net,
        trace_count: total_traces,
        via_count: total_vias,
        total_length_mm,
        total_lower_bound_mm,
        iterations: 0,
        hints: Vec::new(),
    }
}

/// HPWL (half-perimeter wire length) of the net's pad bounding box, in
/// mm. The minimum wire any tree connecting these pads can use; matches
/// the DRC's `RoutingInefficient` lower bound so the two layers report
/// the same "optimum we're measuring against".
fn hpwl_mm(pads: &[NetPadInfo]) -> f64 {
    if pads.len() < 2 {
        return 0.0;
    }
    let mut min_x = f64::INFINITY;
    let mut min_y = f64::INFINITY;
    let mut max_x = f64::NEG_INFINITY;
    let mut max_y = f64::NEG_INFINITY;
    for pad in pads {
        let x = pad.center.x.to_mm();
        let y = pad.center.y.to_mm();
        min_x = min_x.min(x);
        min_y = min_y.min(y);
        max_x = max_x.max(x);
        max_y = max_y.max(y);
    }
    (max_x - min_x) + (max_y - min_y)
}

/// Snap every pad of a bad net to the grid, take the bbox, expand by
/// `CONGESTION_RADIUS_CELLS`, and bump the cost map there. We bump the
/// whole bbox (not just the pad cells) so the corridor that any star
/// route from these pads would naturally take becomes expensive — easy
/// nets routed in the next pass detour around it, leaving a clear lane
/// when the bad net itself runs (it's now first in `order`).
fn bump_corridor(snap_grid: &Grid, cost_map: &mut CostMap, pads: &[NetPadInfo], amount: u32) {
    if pads.is_empty() {
        return;
    }
    let mut min_c = i32::MAX;
    let mut min_r = i32::MAX;
    let mut max_c = i32::MIN;
    let mut max_r = i32::MIN;
    for pad in pads {
        let gp = snap_grid.snap(pad.center, pad.layer);
        min_c = min_c.min(gp.col);
        min_r = min_r.min(gp.row);
        max_c = max_c.max(gp.col);
        max_r = max_r.max(gp.row);
    }
    cost_map.bump_box(
        min_c - CONGESTION_RADIUS_CELLS,
        min_r - CONGESTION_RADIUS_CELLS,
        max_c + CONGESTION_RADIUS_CELLS,
        max_r + CONGESTION_RADIUS_CELLS,
        amount,
        CONGESTION_MAX,
    );
}

/// Collapse the path's grid cells into trace segments + via flips and
/// add them to the board. Stamps the new traces onto the grid so
/// subsequent nets honour them as obstacles. Returns
/// `(segments, vias, length_mm)` where `length_mm` is the sum of all
/// straight segments laid (vias themselves contribute zero length).
#[allow(clippy::too_many_arguments)]
fn lay_path(
    board: &mut Board,
    grid: &mut Grid,
    path: &[GridPoint],
    net: &str,
    net_id: u32,
    opts: &RouteOptions,
    copper_cells: i32,
    via_copper_cells: i32,
    trace_width: Length,
    target_world: Option<Point>,
) -> (usize, usize, f64) {
    if path.len() < 2 {
        return (0, 0, 0.0);
    }
    let mut segments = 0;
    let mut vias = 0;
    let mut length_mm = 0.0_f64;
    let mut seg_start_idx = 0;
    for i in 1..path.len() {
        let prev = path[i - 1];
        let cur = path[i];
        if cur.layer != prev.layer {
            if seg_start_idx < i - 1 {
                length_mm += emit_trace(
                    board,
                    grid,
                    &path[seg_start_idx..i],
                    net,
                    net_id,
                    opts,
                    copper_cells,
                    trace_width,
                    None,
                );
                segments += 1;
            }
            board.add_via(Via {
                id: pcb_core::Id::new(),
                position: grid.unsnap(prev),
                drill: opts.via_drill,
                diameter: opts.via_diameter,
                net: net.to_string(),
            });
            grid.stamp_via(prev, net_id, via_copper_cells);
            vias += 1;
            seg_start_idx = i;
        }
    }
    if seg_start_idx < path.len() - 1 {
        length_mm += emit_trace(
            board,
            grid,
            &path[seg_start_idx..],
            net,
            net_id,
            opts,
            copper_cells,
            trace_width,
            target_world,
        );
        segments += 1;
    }
    (segments, vias, length_mm)
}

/// Emit one straight segment per consecutive same-layer pair in `path`.
/// Theta* hands us explicit corners (any-angle), so each window is
/// already a straight LOS run — no need to detect direction changes.
/// Returns total Euclidean length in mm. If `target_world` is `Some`,
/// the LAST segment's end is overridden with that exact world point
/// (typically the spoke pad's true centre, not the snapped grid cell).
/// Cleans up the visual gap a grid-rounded endpoint leaves between
/// trace and pad copper.
#[allow(clippy::too_many_arguments)]
fn emit_trace(
    board: &mut Board,
    grid: &mut Grid,
    path: &[GridPoint],
    net: &str,
    net_id: u32,
    _opts: &RouteOptions,
    copper_cells: i32,
    trace_width: Length,
    target_world: Option<Point>,
) -> f64 {
    if path.len() < 2 {
        return 0.0;
    }
    let layer = path[0].copper_layer();
    let mut total_mm = 0.0_f64;
    let last_idx = path.len() - 2;
    for (i, w) in path.windows(2).enumerate() {
        let s = w[0];
        let e = w[1];
        if s == e {
            continue;
        }
        let start = grid.unsnap(s);
        let end = if i == last_idx {
            target_world.unwrap_or_else(|| grid.unsnap(e))
        } else {
            grid.unsnap(e)
        };
        let dx = start.x.to_mm() - end.x.to_mm();
        let dy = start.y.to_mm() - end.y.to_mm();
        let len_mm = (dx * dx + dy * dy).sqrt();
        let trace = Trace {
            id: pcb_core::Id::new(),
            layer,
            start,
            end,
            width: trace_width,
            net: net.to_string(),
        };
        grid.stamp_trace(s, e, net_id, copper_cells);
        board.add_trace(trace);
        total_mm += len_mm;
    }
    total_mm
}

/// One pad to be routed: its world-coord centre, copper layer, and
/// the human-friendly reference (e.g. "U3.2") so failures can name
/// the offender instead of dumping nm coordinates.
#[derive(Debug, Clone)]
pub struct NetPadInfo {
    pub center: Point,
    pub layer: CopperLayer,
    pub pad_ref: String,
}

/// Board-coord corridor check for diff-pair follow. We reject a
/// proposed B segment when it intersects a foreign-net trace, a pad of
/// any other net, or a keepout polygon — but ALLOW running close to
/// the partner trace (which is the whole point of diff-pair routing).
fn check_diff_corridor_clear(
    board: &Board,
    t: &Trace,
    self_net: &str,
    partner_net: &str,
    clearance_mm: f64,
) -> Result<(), String> {
    let half_w = t.width.to_mm() / 2.0;
    // Foreign-net traces — except the partner, which we deliberately
    // want to run alongside. Require full edge-to-edge clearance: the
    // diff-pair offset is only loosened toward the PARTNER, never toward
    // unrelated copper (otherwise the parallel run hugs foreign traces
    // and the DRC flags TraceTraceClearance).
    for other in &board.traces {
        if other.net == self_net || other.net == partner_net {
            continue;
        }
        if other.layer != t.layer {
            continue;
        }
        let half_other = other.width.to_mm() / 2.0;
        let min_dist = half_w + half_other + clearance_mm;
        let d = segment_to_segment_distance_mm(t, other);
        if d < min_dist {
            return Err(format!("crosses foreign net `{}`", other.net));
        }
    }
    // Keepouts.
    for kp in &board.keepouts {
        if kp.polygon.len() < 3 {
            continue;
        }
        // Sample a few points along the segment.
        for k in 0..=10 {
            let f = f64::from(k) / 10.0;
            let x = t.start.x.to_mm() + f * (t.end.x.to_mm() - t.start.x.to_mm());
            let y = t.start.y.to_mm() + f * (t.end.y.to_mm() - t.start.y.to_mm());
            if simple_point_in_polygon(&kp.polygon, x, y) {
                return Err("enters keepout".into());
            }
        }
    }
    // Foreign pads — but NOT the partner's: a diff pair runs deliberately
    // close to its partner, including the partner's breakout pads at the
    // connector. Only unrelated copper demands full clearance.
    for fp in board.footprints_in_order() {
        for pad in &fp.pads {
            if pad.net.as_deref() == Some(self_net)
                || pad.net.as_deref() == Some(partner_net)
            {
                continue;
            }
            if pad.layer != t.layer {
                continue;
            }
            let c = fp.pad_world_center(pad);
            let (pw, ph) = fp.pad_world_size(pad);
            // Distance from pad center to segment.
            let d = point_to_segment_mm(
                c.x.to_mm(),
                c.y.to_mm(),
                t.start.x.to_mm(),
                t.start.y.to_mm(),
                t.end.x.to_mm(),
                t.end.y.to_mm(),
            );
            let pad_r = pw.to_mm().max(ph.to_mm()) / 2.0;
            if d < pad_r + half_w + clearance_mm {
                return Err(format!(
                    "crosses pad of net `{}`",
                    pad.net.as_deref().unwrap_or("(no-net)")
                ));
            }
        }
    }
    Ok(())
}

fn point_to_segment_mm(px: f64, py: f64, ax: f64, ay: f64, bx: f64, by: f64) -> f64 {
    let dx = bx - ax;
    let dy = by - ay;
    let len2 = dx * dx + dy * dy;
    if len2 < 1e-12 {
        let ex = px - ax;
        let ey = py - ay;
        return (ex * ex + ey * ey).sqrt();
    }
    let t = ((px - ax) * dx + (py - ay) * dy) / len2;
    let t = t.clamp(0.0, 1.0);
    let cx = ax + t * dx;
    let cy = ay + t * dy;
    let ex = px - cx;
    let ey = py - cy;
    (ex * ex + ey * ey).sqrt()
}

fn segment_to_segment_distance_mm(a: &Trace, b: &Trace) -> f64 {
    // Approximate: minimum of the four endpoint→segment distances.
    let ax1 = a.start.x.to_mm();
    let ay1 = a.start.y.to_mm();
    let ax2 = a.end.x.to_mm();
    let ay2 = a.end.y.to_mm();
    let bx1 = b.start.x.to_mm();
    let by1 = b.start.y.to_mm();
    let bx2 = b.end.x.to_mm();
    let by2 = b.end.y.to_mm();
    let mut d = point_to_segment_mm(ax1, ay1, bx1, by1, bx2, by2);
    d = d.min(point_to_segment_mm(ax2, ay2, bx1, by1, bx2, by2));
    d = d.min(point_to_segment_mm(bx1, by1, ax1, ay1, ax2, ay2));
    d = d.min(point_to_segment_mm(bx2, by2, ax1, ay1, ax2, ay2));
    d
}

fn simple_point_in_polygon(poly: &[Point], x: f64, y: f64) -> bool {
    let n = poly.len();
    if n < 3 {
        return false;
    }
    let mut inside = false;
    let mut j = n - 1;
    for i in 0..n {
        let pix = poly[i].x.to_mm();
        let piy = poly[i].y.to_mm();
        let pjx = poly[j].x.to_mm();
        let pjy = poly[j].y.to_mm();
        if (piy > y) != (pjy > y) {
            let denom = pjy - piy;
            if denom.abs() > 1e-12 {
                let xi = pix + (y - piy) * (pjx - pix) / denom;
                if x < xi {
                    inside = !inside;
                }
            }
        }
        j = i;
    }
    inside
}

/// Resolve the diff-pair partner net for `net_name` from the schematic.
/// Returns the partner only when the schematic declares one and it is a
/// different net.
fn diff_pair_partner(opts: &RouteOptions, net_name: &str) -> Option<String> {
    let sch = opts.schematic.as_ref()?;
    let partner = sch.class_for(net_name).diff_pair_with.as_ref()?.clone();
    if partner == net_name {
        return None;
    }
    Some(partner)
}

/// Attempt to lay net `B`'s traces as parallel offsets of partner A's
/// existing traces, then short stub paths to B's pads. Returns
/// `(segments, vias, length_mm)` on success or an error string on
/// failure (caller falls back to plain Theta*).
#[allow(clippy::too_many_arguments)]
fn try_diff_pair_follow(
    board: &mut Board,
    grid: &mut crate::grid::Grid,
    b_pads: &[NetPadInfo],
    partner_traces: &[Trace],
    net_b: &str,
    net_id_b: u32,
    width_b: Length,
    gap_mm: f64,
    opts: &RouteOptions,
    via_copper_cells: i32,
    cost_map: &crate::grid::CostMap,
) -> Result<(usize, usize, f64), String> {
    use crate::astar::search;
    if b_pads.len() < 2 {
        return Err("less than 2 pads on follower".into());
    }
    // Per-trace clearance / copper / via-safe radii for net B, from its
    // own width and clearance — mirrors the spoke loop in `route_pass`.
    let (_, net_b_clearance) = effective_net_rules(opts, net_b);
    let clr_cells_b = (ceil_cells(net_b_clearance.0 + width_b.0 / 2, opts.cell.0)
        + CLEARANCE_GUARD_CELLS)
        .max(1);
    let copper_cells_b = ceil_cells(width_b.0 / 2, opts.cell.0).max(0);
    let via_safe_radius =
        ceil_cells(opts.via_diameter.0 / 2 + net_b_clearance.0, opts.cell.0).max(1);
    // Pick a layer that the partner actually uses (same layer for both).
    let layer = partner_traces[0].layer;
    if !partner_traces.iter().all(|t| t.layer == layer) {
        return Err("partner uses multiple layers".into());
    }
    // Width of partner traces (assume all the same — first one wins).
    let width_a_mm = partner_traces[0].width.to_mm();
    let width_b_mm = width_b.to_mm();
    let offset_mm = width_a_mm / 2.0 + gap_mm + width_b_mm / 2.0;

    // Choose offset side: pick the side that puts the parallel run
    // closer to B's pad cluster centroid.
    let centroid_x_mm: f64 =
        b_pads.iter().map(|p| p.center.x.to_mm()).sum::<f64>() / b_pads.len() as f64;
    let centroid_y_mm: f64 =
        b_pads.iter().map(|p| p.center.y.to_mm()).sum::<f64>() / b_pads.len() as f64;

    let mut emitted: Vec<Trace> = Vec::with_capacity(partner_traces.len());
    let mut total_len_mm = 0.0_f64;
    for t in partner_traces {
        let sx = t.start.x.to_mm();
        let sy = t.start.y.to_mm();
        let ex = t.end.x.to_mm();
        let ey = t.end.y.to_mm();
        let dx = ex - sx;
        let dy = ey - sy;
        let len = (dx * dx + dy * dy).sqrt();
        if len < 1e-9 {
            continue;
        }
        let nx = -dy / len;
        let ny = dx / len;
        // Side selection per segment: pick the side whose midpoint is
        // closer to B's centroid.
        let mid_x = f64::midpoint(sx, ex);
        let mid_y = f64::midpoint(sy, ey);
        let pos_d = (mid_x + offset_mm * nx - centroid_x_mm).powi(2)
            + (mid_y + offset_mm * ny - centroid_y_mm).powi(2);
        let neg_d = (mid_x - offset_mm * nx - centroid_x_mm).powi(2)
            + (mid_y - offset_mm * ny - centroid_y_mm).powi(2);
        let sign = if pos_d <= neg_d { 1.0 } else { -1.0 };
        let ox = sign * offset_mm * nx;
        let oy = sign * offset_mm * ny;
        let p_start = Point::new(
            Length::from_mm(sx + ox),
            Length::from_mm(sy + oy),
        );
        let p_end = Point::new(
            Length::from_mm(ex + ox),
            Length::from_mm(ey + oy),
        );
        emitted.push(Trace {
            id: pcb_core::Id::new(),
            layer,
            start: p_start,
            end: p_end,
            width: width_b,
            net: net_b.to_string(),
        });
        total_len_mm += len;
    }
    if emitted.is_empty() {
        return Err("no usable partner segments".into());
    }

    // Check the parallel corridor in board coords (not via the grid):
    // the grid's halo around the partner trace would falsely reject
    // the very-close diff-pair offset because gap < default clearance
    // by design. We check foreign-net traces (excluding the partner),
    // foreign pads, and keepouts directly.
    let partner_net_str = diff_pair_partner(opts, net_b).unwrap_or_default();
    let clearance_mm = net_b_clearance.to_mm();
    for t in &emitted {
        check_diff_corridor_clear(board, t, net_b, &partner_net_str, clearance_mm)?;
    }
    // Commit traces and stamp them on the grid.
    let mut segments = 0usize;
    for t in &emitted {
        let a = grid.snap(t.start, t.layer);
        let b = grid.snap(t.end, t.layer);
        grid.stamp_trace(a, b, net_id_b, copper_cells_b);
        board.add_trace(t.clone());
        segments += 1;
    }

    // Now do short Theta* end-cap searches from the closest emitted
    // endpoint to each pad of B. We attempt to land each pad onto the
    // existing parallel net. Multi-source over Trace(net_id_b) covers
    // that for free.
    let mut vias = 0usize;
    let mut total_segs = segments;
    for pad in b_pads {
        let spoke_grid = grid.snap(pad.center, pad.layer);
        // If pad already lands on a same-net trace, skip.
        if matches!(grid.get(spoke_grid), crate::grid::Cell::Trace(n) if n == net_id_b) {
            continue;
        }
        // If the pad is very close to one of the emitted parallel
        // endpoints (within a couple of grid cells), emit a direct
        // stub trace instead of running A* — A* sometimes refuses to
        // start from cells already stamped as `NetPad(self)` because
        // the pad cell is the search start AND target.
        let mut nearest: Option<(Point, f64)> = None;
        for t in &emitted {
            for ep in [t.start, t.end] {
                let dx = ep.x.to_mm() - pad.center.x.to_mm();
                let dy = ep.y.to_mm() - pad.center.y.to_mm();
                let d = (dx * dx + dy * dy).sqrt();
                if nearest.is_none_or(|(_, nd)| d < nd) {
                    nearest = Some((ep, d));
                }
            }
        }
        if let Some((closest, d)) = nearest {
            // Two grid cells worth of stub is the threshold for a
            // direct connection — A* would just emit the same line.
            if d <= 2.0 * grid.cell_nm as f64 / 1_000_000.0 {
                let stub = Trace {
                    id: pcb_core::Id::new(),
                    layer,
                    start: closest,
                    end: pad.center,
                    width: width_b,
                    net: net_b.to_string(),
                };
                let a = grid.snap(stub.start, stub.layer);
                let b = grid.snap(stub.end, stub.layer);
                grid.stamp_trace(a, b, net_id_b, copper_cells_b);
                board.add_trace(stub);
                total_segs += 1;
                total_len_mm += d;
                continue;
            }
        }
        // Synthesise a "seed" — pick the closest emitted endpoint as
        // the start (multi-source A* will also see the Trace cells).
        let mut best_seed = grid.snap(emitted[0].start, layer);
        let mut best_d = u64::MAX;
        for t in &emitted {
            for ep in [t.start, t.end] {
                let gp = grid.snap(ep, layer);
                let dc = u64::from((gp.col - spoke_grid.col).unsigned_abs());
                let dr = u64::from((gp.row - spoke_grid.row).unsigned_abs());
                let d = dc + dr;
                if d < best_d {
                    best_d = d;
                    best_seed = gp;
                }
            }
        }
        // Multi-source set = every cell of net_b's already-emitted
        // traces, so the search branches off the partner-parallel run
        // just as it did when it rescanned the grid for Trace cells.
        let mut db_sources: Vec<GridPoint> = Vec::new();
        for t in &emitted {
            let a = grid.snap(t.start, t.layer);
            let b = grid.snap(t.end, t.layer);
            if a.layer == b.layer {
                db_sources.extend(grid.line_cells(a, b));
            }
        }
        let Some(result) = search(
            grid,
            best_seed,
            net_id_b,
            opts.via_cost,
            spoke_grid,
            via_safe_radius,
            clr_cells_b,
            cost_map,
            &db_sources,
            opts.heuristic_weight,
        ) else {
            return Err(format!("no end-cap to pad {}", pad.pad_ref));
        };
        let (segs, vs, len) = lay_path(
            board,
            grid,
            &result.path,
            net_b,
            net_id_b,
            opts,
            copper_cells_b,
            via_copper_cells,
            width_b,
            Some(pad.center),
        );
        total_segs += segs;
        vias += vs;
        total_len_mm += len;
    }
    Ok((total_segs, vias, total_len_mm))
}

/// Reorder so any net whose class declares `diff_pair_with = X` is
/// scheduled IMMEDIATELY after X. The partner has to be in the board's
/// net set too — otherwise we can't follow what isn't there.
/// Preserves the relative order of every other net.
fn reorder_for_diff_pairs(order: Vec<String>, opts: &RouteOptions) -> Vec<String> {
    let Some(sch) = opts.schematic.as_ref() else {
        return order;
    };
    let present: HashSet<&str> = order.iter().map(String::as_str).collect();
    // For each net, the partner it depends on (if any).
    let mut depends_on: HashMap<String, String> = HashMap::new();
    for n in &order {
        if let Some(p) = sch.class_for(n).diff_pair_with.as_ref() {
            if p != n && present.contains(p.as_str()) {
                depends_on.insert(n.clone(), p.clone());
            }
        }
    }
    if depends_on.is_empty() {
        return order;
    }
    // Pair declarations are symmetric (A pair=B and B pair=A). To avoid
    // both halves being treated as followers, break ties by picking
    // whichever appears FIRST in `order` as the leader, then the other
    // becomes the follower. Net names that aren't part of any cycle
    // keep their leader/follower roles as declared.
    let mut leader_of_follower: HashMap<String, String> = HashMap::new();
    let order_idx: HashMap<&str, usize> = order
        .iter()
        .enumerate()
        .map(|(i, n)| (n.as_str(), i))
        .collect();
    for (b, a) in &depends_on {
        // If `a` also depends on `b`, this is a symmetric pair → the
        // earlier one wins as leader.
        if depends_on.get(a).is_some_and(|x| x == b) {
            let bi = order_idx.get(b.as_str()).copied().unwrap_or(usize::MAX);
            let ai = order_idx.get(a.as_str()).copied().unwrap_or(usize::MAX);
            if ai < bi {
                // a leads, b follows.
                leader_of_follower.insert(b.clone(), a.clone());
            }
            // else: b is earlier or equal → b leads; skip recording this
            // (the partner direction `a depends on b` will handle b's
            // follower role from its own loop iteration).
        } else {
            // Asymmetric — `b` depends on `a`, `a` doesn't depend on
            // `b`. `b` is the follower.
            leader_of_follower.insert(b.clone(), a.clone());
        }
    }
    if leader_of_follower.is_empty() {
        return order;
    }
    let followers: HashSet<&str> = leader_of_follower.keys().map(String::as_str).collect();
    let mut followers_of: HashMap<String, Vec<String>> = HashMap::new();
    for (b, a) in &leader_of_follower {
        followers_of.entry(a.clone()).or_default().push(b.clone());
    }
    let mut out: Vec<String> = Vec::with_capacity(order.len());
    for n in &order {
        if followers.contains(n.as_str()) {
            continue;
        }
        out.push(n.clone());
        if let Some(fs) = followers_of.get(n) {
            let mut sorted: Vec<&String> = fs.iter().collect();
            sorted.sort_by_key(|f| order.iter().position(|x| x == *f).unwrap_or(usize::MAX));
            for f in sorted {
                out.push(f.clone());
            }
        }
    }
    out
}

/// For every footprint pad with a net assignment, record the pad's
/// world-coord center (rotation-aware), copper layer, and "Ref.Pin"
/// label under that net's name.
fn collect_nets(board: &Board) -> BTreeMap<String, Vec<NetPadInfo>> {
    let mut nets: BTreeMap<String, Vec<NetPadInfo>> = BTreeMap::new();
    for fp in board.footprints_in_order() {
        for pad in &fp.pads {
            if let Some(net) = &pad.net {
                let center = fp.pad_world_center(pad);
                nets.entry(net.clone()).or_default().push(NetPadInfo {
                    center,
                    layer: pad.layer,
                    pad_ref: format!("{}.{}", fp.reference, pad.number),
                });
            }
        }
    }
    nets
}
