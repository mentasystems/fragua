//! Driver that ties the grid and A* together.

use std::collections::{BTreeMap, HashMap};

use pcb_core::{Board, CopperLayer, Length, Point, Rect, Trace, Via};

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
    /// Cost (in cells) of punching a via vs. routing one cell on the
    /// same layer. Higher = router prefers single-layer detours.
    pub via_cost: u32,
    /// Via geometry produced when the path flips layers.
    pub via_drill: Length,
    pub via_diameter: Length,
    /// Per-net rule overrides keyed by net name. Built by the caller
    /// from the schematic's `NetClass` definitions; the router stays
    /// schematic-agnostic and just consults this map.
    pub net_overrides: HashMap<String, NetOverride>,
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
        }
    }
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
/// outright. 1.8 means "the actual wire is ≥80 % longer than the
/// hub-to-pads optimum"; below that, reordering rarely helps.
const BAD_DETOUR_RATIO: f64 = 1.8;

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

    // First-pass order: easy nets (fewest pads) first. Same heuristic
    // as before — gets the unconstrained nets to lay copper before the
    // hairy ones contend for space.
    let mut order: Vec<String> = nets.keys().cloned().collect();
    order.sort_by_key(|n| nets.get(n).map(Vec::len).unwrap_or(0));

    // Cost map shared across iterations: starts at 0, accumulates bias
    // around the corridors of failed/inefficient nets so the next pass
    // detours easy nets out of those corridors. Built from a one-shot
    // grid only for its dims; the actual obstacle grid is built fresh
    // per pass inside `route_pass`.
    let region = compute_region(board, opts);
    let mut cost_map = Grid::new(region, opts.cell).new_cost_map();

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
        let report = route_pass(&mut work, &nets, &order, opts, &cost_map);

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
        let snap_grid = Grid::new(region, opts.cell);
        let bump_factor = iterations_run as u32; // 1, 2, 3...
        for name in &failed {
            bump_corridor(
                &snap_grid,
                &mut cost_map,
                nets.get(name).map(Vec::as_slice).unwrap_or(&[]),
                CONGESTION_BUMP_FAILED * bump_factor,
            );
        }
        for name in &inefficient {
            bump_corridor(
                &snap_grid,
                &mut cost_map,
                nets.get(name).map(Vec::as_slice).unwrap_or(&[]),
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
) -> RouteReport {
    let net_id_of: HashMap<String, u32> = order
        .iter()
        .enumerate()
        .map(|(i, n)| (n.clone(), i as u32))
        .collect();

    let region = compute_region(board, opts);
    let mut grid = Grid::new(region, opts.cell);
    // Effective clearance for grid setup = max across the global
    // default and every per-net override. The grid is built once, so
    // we have to be conservative — using the strictest clearance
    // means a class with 0.30 mm clearance never gets less halo than
    // a default of 0.20 mm.
    let max_clearance: Length = {
        let mut c = opts.clearance;
        for o in opts.net_overrides.values() {
            if let Some(over) = o.clearance {
                if over.0 > c.0 {
                    c = over;
                }
            }
        }
        c
    };
    let net_id_lookup = |n: &str| net_id_of.get(n).copied();
    grid.stamp_pads(board, &net_id_lookup, max_clearance);
    // Halo around freshly-laid traces, in cells: max_clearance / cell.
    // Round up so 0.20 mm clearance on a 0.25 mm grid still gives one
    // cell of breathing room.
    let halo_cells = {
        let raw = (max_clearance.0 + opts.cell.0 - 1) / opts.cell.0;
        i32::try_from(raw).unwrap_or(1).max(1)
    };
    // Via-safety radius: a via's copper extends `via_diameter/2` from
    // its centre and must keep `clearance` to every other net's copper
    // on both layers. The A* check rejects via flips landing inside
    // this radius of foreign cells.
    let via_safe_radius = {
        let raw = (opts.via_diameter.0 / 2 + max_clearance.0 + opts.cell.0 - 1) / opts.cell.0;
        i32::try_from(raw).unwrap_or(1).max(1)
    };

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
        let seed_idx = (0..pad_points.len())
            .min_by_key(|&i| {
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
            .unwrap_or(0);
        let seed = pad_points[seed_idx].clone();
        let seed_grid = grid.snap(seed.center, seed.layer);

        // Resolve this net's trace width: per-net override wins,
        // otherwise the global default.
        let net_trace_width = opts
            .net_overrides
            .get(net_name)
            .and_then(|o| o.trace_width)
            .unwrap_or(opts.trace_width);

        let mut net_segments = 0usize;
        let mut net_vias = 0usize;
        let mut net_length_mm = 0.0_f64;
        let mut failed = false;
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
            let spoke_grid = grid.snap(spoke.center, spoke.layer);
            let Some(result) = search(
                &grid,
                seed_grid,
                net_id,
                opts.via_cost,
                spoke_grid,
                via_safe_radius,
                cost_map,
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
                halo_cells,
                net_trace_width,
            );
            net_segments += segs;
            net_vias += vias;
            net_length_mm += length_mm;
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
fn lay_path(
    board: &mut Board,
    grid: &mut Grid,
    path: &[GridPoint],
    net: &str,
    net_id: u32,
    opts: &RouteOptions,
    halo_cells: i32,
    trace_width: Length,
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
                    halo_cells,
                    trace_width,
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
            grid.stamp_via(prev, net_id, halo_cells);
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
            halo_cells,
            trace_width,
        );
        segments += 1;
    }
    (segments, vias, length_mm)
}

/// Emit all the straight segments contained in `path` (one per turn)
/// and return the total length, in mm, of the segments laid.
fn emit_trace(
    board: &mut Board,
    grid: &mut Grid,
    path: &[GridPoint],
    net: &str,
    net_id: u32,
    _opts: &RouteOptions,
    halo_cells: i32,
    trace_width: Length,
) -> f64 {
    if path.len() < 2 {
        return 0.0;
    }
    let layer = path[0].copper_layer();
    let mut total_mm = 0.0_f64;
    let mut start_idx = 0;
    let push_trace = |board: &mut Board, grid: &mut Grid, s: GridPoint, e: GridPoint| -> f64 {
        let start = grid.unsnap(s);
        let end = grid.unsnap(e);
        let len_mm =
            (start.x.to_mm() - end.x.to_mm()).abs() + (start.y.to_mm() - end.y.to_mm()).abs();
        let trace = Trace {
            id: pcb_core::Id::new(),
            layer,
            start,
            end,
            width: trace_width,
            net: net.to_string(),
        };
        grid.stamp_trace(s, e, net_id, halo_cells);
        board.add_trace(trace);
        len_mm
    };
    for i in 1..path.len() {
        let a = path[i - 1];
        let b = path[i];
        let s = path[start_idx];
        let going_horizontal = a.row == b.row;
        let started_horizontal = a.row == s.row;
        let direction_change = i > 1 && going_horizontal != started_horizontal;
        if direction_change {
            total_mm += push_trace(board, grid, s, a);
            start_idx = i - 1;
        }
    }
    let s = path[start_idx];
    let e = path[path.len() - 1];
    total_mm += push_trace(board, grid, s, e);
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
