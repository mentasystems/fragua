//! Driver that ties the grid and A* together.

use std::collections::{BTreeMap, HashMap};

use pcb_core::{Board, CopperLayer, Length, Point, Rect, Trace, Via};

use crate::astar::search;
use crate::grid::{Grid, GridPoint};

#[derive(Debug, Clone)]
pub struct RouteOptions {
    /// Cell pitch on the routing grid. 0.25 mm is the default sweet
    /// spot for SMD-only boards: fine enough for 0.5 mm pin pitch,
    /// coarse enough for grids of ~250 × 250 cells per layer to stay
    /// fast.
    pub cell: Length,
    /// Trace width laid down by the router.
    pub trace_width: Length,
    /// Clearance added on every side of pad obstacles.
    pub clearance: Length,
    /// Cost (in cells) of punching a via vs. routing one cell on the
    /// same layer. Higher = router prefers single-layer detours.
    pub via_cost: u32,
    /// Via geometry produced when the path flips layers.
    pub via_drill: Length,
    pub via_diameter: Length,
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
        }
    }
}

#[derive(Debug, Clone)]
pub enum Outcome {
    Ok { trace_segments: usize, vias: usize },
    Failed { reason: String },
}

#[derive(Debug, Clone)]
pub struct RouteReport {
    pub per_net: Vec<(String, Outcome)>,
    pub trace_count: usize,
    pub via_count: usize,
}

/// Route every net found in the board's pad assignments. Mutates
/// `board` in place: existing routing is cleared, new routing is laid.
pub fn route(board: &mut Board, opts: &RouteOptions) -> RouteReport {
    board.clear_routing();

    // Collect nets and their pad locations.
    let nets = collect_nets(board);
    if nets.is_empty() {
        return RouteReport {
            per_net: Vec::new(),
            trace_count: 0,
            via_count: 0,
        };
    }
    let net_names: Vec<String> = nets.keys().cloned().collect();
    let net_id_of_owned = net_names.clone();
    let net_id_of: HashMap<String, u32> = net_id_of_owned
        .iter()
        .enumerate()
        .map(|(i, n)| (n.clone(), i as u32))
        .collect();

    // Routing region. If the board has an outline, the router stays
    // *inside* it with an inset that keeps the centre of the widest
    // copper feature (a via) far enough from Edge.Cuts to satisfy the
    // DRC's edge clearance check (default 0.3 mm). Without an outline
    // we fall back to the content bbox expanded by 5 mm so the router
    // still has slack to find paths.
    let edge_clearance = Length::from_mm(0.3);
    let half_widest = Length(opts.trace_width.0.max(opts.via_diameter.0) / 2);
    let outline_inset = edge_clearance + half_widest;
    let region = match board.outline {
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
    };

    let mut grid = Grid::new(region, opts.cell);
    let net_id_lookup = |n: &str| net_id_of.get(n).copied();
    grid.stamp_pads(board, &net_id_lookup, opts.clearance);
    // Halo around freshly-laid traces, in cells: clearance / cell.
    // Round up so 0.20 mm clearance on a 0.25 mm grid still gives one
    // cell of breathing room.
    let halo_cells = {
        let raw = (opts.clearance.0 + opts.cell.0 - 1) / opts.cell.0;
        i32::try_from(raw).unwrap_or(1).max(1)
    };
    // Via-safety radius: a via's copper extends `via_diameter/2` from
    // its centre and must keep `clearance` to every other net's copper
    // on both layers. The A* check rejects via flips landing inside
    // this radius of foreign cells.
    let via_safe_radius = {
        let raw = (opts.via_diameter.0 / 2 + opts.clearance.0 + opts.cell.0 - 1) / opts.cell.0;
        i32::try_from(raw).unwrap_or(1).max(1)
    };

    let mut per_net = Vec::with_capacity(nets.len());
    let mut total_traces = 0;
    let mut total_vias = 0;

    // Route nets in order of increasing pad count so easy ones lay
    // down their tracks before harder ones contend for space. This is
    // a cheap heuristic; full rip-up-and-retry comes in a later phase.
    let mut ordered: Vec<_> = nets.into_iter().collect();
    ordered.sort_by_key(|(_, pads)| pads.len());

    for (net_name, pad_points) in ordered {
        let net_id = net_id_of[&net_name];
        if pad_points.len() < 2 {
            per_net.push((
                net_name,
                Outcome::Ok { trace_segments: 0, vias: 0 },
            ));
            continue;
        }
        // Pick the geographically central pad as hub: minimum sum of
        // Manhattan distances to every other pad on the net. Short
        // spokes are easier to route, and since the FIRST failed spoke
        // takes the whole net down, a central hub keeps the failure
        // probability low. Star routing is still tutorial-grade — full
        // Steiner improvement is future work.
        let hub_idx = (0..pad_points.len())
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
        let hub = pad_points[hub_idx].clone();
        let hub_grid = grid.snap(hub.center, hub.layer);

        let mut net_segments = 0usize;
        let mut net_vias = 0usize;
        let mut failed = false;
        // Spokes ordered by distance to hub (closest first). Once the
        // first spokes lay copper near the hub, subsequent (farther)
        // spokes can join the existing trace anywhere along its run
        // instead of having to fight back to the hub pad itself.
        let mut spokes_sorted: Vec<NetPadInfo> = pad_points
            .iter()
            .enumerate()
            .filter(|(i, _)| *i != hub_idx)
            .map(|(_, p)| p.clone())
            .collect();
        spokes_sorted.sort_by_key(|q| {
            (hub.center.x.0 - q.center.x.0).unsigned_abs()
                + (hub.center.y.0 - q.center.y.0).unsigned_abs()
        });
        for spoke in spokes_sorted {
            let spoke_grid = grid.snap(spoke.center, spoke.layer);
            let Some(result) = search(
                &grid,
                hub_grid,
                net_id,
                opts.via_cost,
                spoke_grid,
                via_safe_radius,
            ) else {
                per_net.push((
                    net_name.clone(),
                    Outcome::Failed {
                        reason: format!(
                            "no path from hub {} to pad {} at ({:.2}, {:.2}) mm",
                            hub.pad_ref,
                            spoke.pad_ref,
                            spoke.center.x.to_mm(),
                            spoke.center.y.to_mm(),
                        ),
                    },
                ));
                failed = true;
                break;
            };
            let (segs, vias) =
                lay_path(board, &mut grid, &result.path, &net_name, net_id, opts, halo_cells);
            net_segments += segs;
            net_vias += vias;
        }
        if !failed {
            total_traces += net_segments;
            total_vias += net_vias;
            per_net.push((
                net_name,
                Outcome::Ok {
                    trace_segments: net_segments,
                    vias: net_vias,
                },
            ));
        }
    }

    RouteReport {
        per_net,
        trace_count: total_traces,
        via_count: total_vias,
    }
}

/// Collapse the path's grid cells into trace segments + via flips and
/// add them to the board. Stamps the new traces onto the grid so
/// subsequent nets honour them as obstacles.
fn lay_path(
    board: &mut Board,
    grid: &mut Grid,
    path: &[GridPoint],
    net: &str,
    net_id: u32,
    opts: &RouteOptions,
    halo_cells: i32,
) -> (usize, usize) {
    if path.len() < 2 {
        return (0, 0);
    }
    let mut segments = 0;
    let mut vias = 0;
    let mut seg_start_idx = 0;
    for i in 1..path.len() {
        let prev = path[i - 1];
        let cur = path[i];
        if cur.layer != prev.layer {
            if seg_start_idx < i - 1 {
                emit_trace(
                    board,
                    grid,
                    &path[seg_start_idx..i],
                    net,
                    net_id,
                    opts,
                    halo_cells,
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
        emit_trace(
            board,
            grid,
            &path[seg_start_idx..],
            net,
            net_id,
            opts,
            halo_cells,
        );
        segments += 1;
    }
    (segments, vias)
}

fn emit_trace(
    board: &mut Board,
    grid: &mut Grid,
    path: &[GridPoint],
    net: &str,
    net_id: u32,
    opts: &RouteOptions,
    halo_cells: i32,
) {
    if path.len() < 2 {
        return;
    }
    let layer = path[0].copper_layer();
    let mut start_idx = 0;
    for i in 1..path.len() {
        let a = path[i - 1];
        let b = path[i];
        let s = path[start_idx];
        let going_horizontal = a.row == b.row;
        let started_horizontal = a.row == s.row;
        let direction_change = i > 1 && going_horizontal != started_horizontal;
        if direction_change {
            let trace = Trace {
                id: pcb_core::Id::new(),
                layer,
                start: grid.unsnap(s),
                end: grid.unsnap(a),
                width: opts.trace_width,
                net: net.to_string(),
            };
            grid.stamp_trace(s, a, net_id, halo_cells);
            board.add_trace(trace);
            start_idx = i - 1;
        }
    }
    let s = path[start_idx];
    let e = path[path.len() - 1];
    let trace = Trace {
        id: pcb_core::Id::new(),
        layer,
        start: grid.unsnap(s),
        end: grid.unsnap(e),
        width: opts.trace_width,
        net: net.to_string(),
    };
    grid.stamp_trace(s, e, net_id, halo_cells);
    board.add_trace(trace);
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
