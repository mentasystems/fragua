//! Localized fine-grid escape (Level 2).
//!
//! The plain fanout pass drops a via-in-pad on every fine-pitch pad and
//! lets the *coarse* router pick the net up on an inner layer. That works
//! until a row packs three or more independent 0.5 mm-pitch signals into
//! one connector (USB-C CC/DP/DM): the via barrels short every layer, so a
//! barrel at a neighbour's column boxes the middle pin's approach on all
//! layers, and the coarse grid's cell-quantised clearance disk over-rejects
//! the genuinely-valid (≈0.225 mm) gap between adjacent pads.
//!
//! This module fixes both. For each fine-pitch footprint it builds a small
//! ~0.05 mm grid over the pad field and, for every pad that must escape,
//! routes a short surface STUB from the pad out through the tight gap
//! between its neighbours into the OPEN channel (past the pad ends, where
//! all of the row's pads have stopped), then fans the stubs apart and drops
//! a breakout via where a 0.30 mm via fits with room to spare. The coarse
//! router then targets the spread-out breakout — never the boxed-in pad.
//!
//! The fine grid only proposes the stub PATH; every laid segment is then
//! validated against the true (un-quantised) geometry at honest clearance,
//! so a stub is accepted only when it really clears 0.20 mm. Pads for which
//! no clean stub is found fall back to the staggered via-in-pad, so this
//! pass is never worse than the fanout it replaces.

use std::collections::{HashMap, HashSet};

use pcb_core::{Board, CopperLayer, Id, Length, Point, Rect, Trace, Via};

use crate::astar::search;
use crate::fanout::{
    self, FanoutPlan, PadRect, EDGE_CLEARANCE_MM, FANOUT_VIA_DIAMETER_MM, FANOUT_VIA_DRILL_MM,
};
use crate::grid::{Cell, Grid};
use crate::router::{is_power_net, RouteOptions};

/// Fine routing pitch for the local escape tiles.
const FINE_CELL_MM: f64 = 0.05;
/// Margin (mm) added around a footprint's pad bbox for the local tile.
const TILE_MARGIN_MM: f64 = 1.4;
/// Minimum perp spacing (mm) the breakouts of one fanned row spread to.
/// The actual spread is the larger of this and a value derived from the
/// COARSE cell (`breakout_spread`) so a middle pin is never re-boxed at a
/// coarser grid pitch.
const BREAKOUT_SPREAD_MIN_MM: f64 = 0.8;

/// Perp spacing the coarse router needs between two breakout vias so the
/// approach lane to one clears the other's barrel. Scales with the coarse
/// cell: at a coarser pitch the clearance disk is wider, so the breakouts
/// must spread further or the middle pin gets boxed again. Derived from the
/// coarse clearance disk radius (`ceil((clr+w/2)/cell)+guard`) plus the via
/// radius plus a half-cell margin.
fn breakout_spread(opts: &RouteOptions) -> f64 {
    let cell = opts.cell.to_mm();
    let clr = opts.clearance.to_mm();
    let w = opts.trace_width.to_mm();
    let disk_cells = ((clr + w / 2.0) / cell).ceil() + 1.0; // +1 = clearance guard
    let disk_mm = disk_cells * cell;
    (disk_mm + FANOUT_VIA_DIAMETER_MM / 2.0 + 0.5 * cell).max(BREAKOUT_SPREAD_MIN_MM)
}
/// How far (mm) past the pad end the breakout sits — just enough to clear
/// the row's pad field so the fan happens in open copper, no further.
const BREAKOUT_DEPTH_MM: f64 = 0.85;
/// Extra depths/perp nudges tried when the first breakout spot is taken.
const DEPTH_STEPS: [f64; 4] = [0.0, 0.25, 0.5, 0.75];

/// Result of the escape pass. Carries a `FanoutPlan` shaped exactly like
/// the legacy fanout (so the coarse router's via-disk stamping and
/// via-targeted landing work unchanged — `via_positions` now points at the
/// breakout), plus the pre-laid escape stubs.
pub struct EscapePlan {
    pub fanout: FanoutPlan,
    /// Pre-laid escape stubs (real copper, pad → breakout) to add to the
    /// board and stamp on the coarse grid as obstacles.
    pub stubs: Vec<Trace>,
}

/// An accepted escape: the stub polyline (world points), the breakout via,
/// and the pad it serves.
struct Escape {
    pad_ref: String,
    net: String,
    layer: CopperLayer,
    points: Vec<Point>,
    breakout: Point,
}

/// The stub escape spreads breakout vias just far enough that the COARSE
/// router un-boxes the middle pin of a fanned row. Above this coarse-cell
/// pitch the breakouts would have to spread so wide they overrun the
/// channel (and the coarse pickup of them regresses), so the pass falls
/// back to the plain via-in-pad fanout instead — never worse than fanout.
const FINE_ESCAPE_MAX_CELL_MM: f64 = 0.22;

/// A power/ground pin with a foreign pad this close (mm) must escape: its
/// ~0.50 mm rail terminating on the raw pad would graze the neighbour even
/// when the pin could "escape" a thin trace outward.
const POWER_NEIGHBOUR_MM: f64 = 0.62;

pub fn plan_escapes(board: &Board, opts: &RouteOptions) -> EscapePlan {
    let mut plan = EscapePlan {
        fanout: FanoutPlan::default(),
        stubs: Vec::new(),
    };
    if board.stackup.layer_count() < 3 {
        return plan;
    }
    // Too coarse a coarse-grid for the localized escape to pay off — use
    // the plain fanout (which the coarse router handles fine at this pitch).
    if opts.cell.to_mm() > FINE_ESCAPE_MAX_CELL_MM {
        plan.fanout = fanout::plan_fanout(board, opts);
        return plan;
    }
    let rects = pad_rects_owned(board);
    let foreign: Vec<&PadRect> = rects.iter().collect();
    let tw = opts.trace_width.to_mm();
    let clearance = opts.clearance.to_mm();
    let via_r = FANOUT_VIA_DIAMETER_MM / 2.0;

    // Local net-id map: every net gets a stable id so the fine grid's
    // clearance model treats foreign copper as foreign. No-net pads stamp
    // the FOREIGN sentinel inside `stamp_pads`.
    let net_ids = net_id_map(board);
    let net_id_of = |n: &str| net_ids.get(n).copied();

    // Accumulates placed breakout vias + stubs so later escapes (and the
    // via-fit test) respect them.
    let mut work = board.clone();
    let mut accepted: Vec<Escape> = Vec::new();

    for fp in board.footprints_in_order() {
        let (fp_cx, fp_cy) = footprint_centroid(fp);

        // Which of this footprint's pads need to escape (same rule as the
        // fanout pass)?
        let mut targets: Vec<EscapePad> = Vec::new();
        for pad in &fp.pads {
            let Some(net) = pad.net.as_deref() else {
                continue;
            };
            if pad.drill.is_some() {
                continue;
            }
            let c = fp.pad_world_center(pad);
            let (w, h) = fp.pad_world_size(pad);
            let (cx, cy) = (c.x.to_mm(), c.y.to_mm());
            let (hw, hh) = (w.to_mm() / 2.0, h.to_mm() / 2.0);
            let neighbours = foreign
                .iter()
                .filter(|r| r.net.as_deref() != Some(net))
                .filter(|r| fanout::point_rect_dist(cx, cy, r) < fanout::CLUSTER_DIST_MM)
                .count();
            let in_cluster = neighbours >= fanout::CLUSTER_NEIGHBOURS;
            // A power rail is laid at the power floor (~0.50 mm). Even a
            // corner pin that can escape *outward* into open copper still
            // grazes the pad 0.5 mm to its side, because the fat trace
            // TERMINATING on the pad is wider than the pitch — a 0.25 mm
            // signal would clear, the 0.50 mm rail does not. So a power pin
            // with any foreign pad within `POWER_NEIGHBOUR_MM` must escape:
            // the rail then ends at a necked breakout, not the raw pad.
            let power_crowded = is_power_net(net)
                && foreign
                    .iter()
                    .filter(|r| r.net.as_deref() != Some(net))
                    .any(|r| fanout::point_rect_dist(cx, cy, r) < POWER_NEIGHBOUR_MM);
            if !in_cluster
                && !power_crowded
                && fanout::can_escape_surface(cx, cy, hw, hh, net, &foreign, tw, clearance)
            {
                continue;
            }
            targets.push(EscapePad {
                pad_ref: format!("{}.{}", fp.reference, pad.number),
                net: net.to_string(),
                center: c,
                layer: pad.layer,
                cx,
                cy,
                hw,
                hh,
            });
        }
        if targets.is_empty() {
            continue;
        }

        // Build the local fine grid over the pad field + margin and stamp
        // every pad bare (no body/keepout: the connector body bbox covers
        // the very channel the stubs escape into).
        let region = footprint_tile(fp, TILE_MARGIN_MM);
        let mut grid = Grid::with_layers(region, Length::from_mm(FINE_CELL_MM), 2);
        grid.stamp_pads(board, &net_id_of, Length(0));

        // Long axis / escape direction per pad, and fan grouping.
        let mut infos: Vec<PadInfo> = targets
            .iter()
            .enumerate()
            .map(|(i, t)| {
                let long_x = t.hw >= t.hh;
                let (ax, ay) = if long_x { (1.0, 0.0) } else { (0.0, 1.0) };
                // dir = along the long axis toward the footprint interior.
                let along = ax * (fp_cx - t.cx) + ay * (fp_cy - t.cy);
                let (dx, dy) = if along >= 0.0 { (ax, ay) } else { (-ax, -ay) };
                // perp axis.
                let (px, py) = (-dy, dx);
                // side key: which end of the long axis (groups the two
                // rows of a connector / the four edges of a QFN).
                let side = if long_x {
                    (t.cx - fp_cx).signum()
                } else {
                    (t.cy - fp_cy).signum()
                };
                let group = (long_x, side as i64 as i32);
                PadInfo {
                    idx: i,
                    dir: (dx, dy),
                    perp: (px, py),
                    perp_coord: px * t.cx + py * t.cy,
                    half_len: if long_x { t.hw } else { t.hh },
                    group,
                }
            })
            .collect();

        // Per-group fan: spread breakouts along perp so the coarse router
        // sees them on distinct lanes.
        let mut groups: HashMap<(bool, i32), Vec<usize>> = HashMap::new();
        for (k, info) in infos.iter().enumerate() {
            groups.entry(info.group).or_default().push(k);
        }
        let spread = breakout_spread(opts);
        let mut fan_target: HashMap<usize, f64> = HashMap::new();
        for members in groups.values() {
            let mut ms = members.clone();
            ms.sort_by(|&a, &b| {
                infos[a]
                    .perp_coord
                    .partial_cmp(&infos[b].perp_coord)
                    .unwrap()
            });
            let n = ms.len();
            let center: f64 = ms.iter().map(|&k| infos[k].perp_coord).sum::<f64>() / n as f64;
            for (rank, &k) in ms.iter().enumerate() {
                let off = (rank as f64 - (n as f64 - 1.0) / 2.0) * spread;
                fan_target.insert(k, center + off);
            }
        }

        // Route each pad's stub (process in perp order within the tile so
        // the fan grows monotonically and stubs don't cross).
        infos.sort_by(|a, b| a.perp_coord.partial_cmp(&b.perp_coord).unwrap());
        for info in &infos {
            let t = &targets[info.idx];
            let want_perp = fan_target[&info.idx];
            // Every flagged pin (signal AND power) escapes via the stub:
            // excluding power leaves its via-in-pad boxed in by the
            // neighbouring signal escapes (a power pin surrounded by escaped
            // signals can no longer be reached), so the whole row escapes
            // together.
            let stub = route_one(
                &mut grid, t, info, want_perp, via_r, clearance, &foreign, &work, &net_ids,
                &accepted, opts,
            );
            if let Some(esc) = stub {
                // Record the breakout via in the working board so the next
                // pad's via-fit / validation respects it.
                let via = Via {
                    id: Id::new(),
                    position: esc.breakout,
                    drill: Length::from_mm(FANOUT_VIA_DRILL_MM),
                    diameter: Length::from_mm(FANOUT_VIA_DIAMETER_MM),
                    net: esc.net.clone(),
                };
                work.vias.push(via.clone());
                plan.fanout.vias.push(via);
                plan.fanout.through_pads.insert(esc.pad_ref.clone());
                plan.fanout
                    .via_positions
                    .insert(esc.pad_ref.clone(), esc.breakout);
                // Stamp the laid stub on the local grid as own copper so a
                // later same-tile stub treats it as an obstacle.
                if let Some(&id) = net_ids.get(&esc.net) {
                    stamp_polyline(&mut grid, &esc.points, esc.layer, id, opts);
                }
                accepted.push(esc);
            } else {
                // Fall back to the staggered via-in-pad (legacy fanout
                // behaviour) — never worse than before.
                if let Some((vx, vy)) = fanout::pick_via_position(
                    t.cx, t.cy, t.hw, t.hh, fp_cx, fp_cy, &t.net, via_r, clearance, &foreign, &work,
                ) {
                    let pos = Point::new(Length::from_mm(vx), Length::from_mm(vy));
                    let via = Via {
                        id: Id::new(),
                        position: pos,
                        drill: Length::from_mm(FANOUT_VIA_DRILL_MM),
                        diameter: Length::from_mm(FANOUT_VIA_DIAMETER_MM),
                        net: t.net.clone(),
                    };
                    work.vias.push(via.clone());
                    plan.fanout.vias.push(via);
                    plan.fanout.through_pads.insert(t.pad_ref.clone());
                    plan.fanout.via_positions.insert(t.pad_ref.clone(), pos);
                }
            }
        }
    }

    // Emit the accepted stubs as real traces.
    for esc in &accepted {
        for w in esc.points.windows(2) {
            plan.stubs.push(Trace {
                id: Id::new(),
                layer: esc.layer,
                start: w[0],
                end: w[1],
                width: opts.trace_width,
                net: esc.net.clone(),
            });
        }
    }
    plan
}

struct EscapePad {
    pad_ref: String,
    net: String,
    center: Point,
    layer: CopperLayer,
    cx: f64,
    cy: f64,
    hw: f64,
    hh: f64,
}

struct PadInfo {
    idx: usize,
    dir: (f64, f64),
    perp: (f64, f64),
    perp_coord: f64,
    half_len: f64,
    group: (bool, i32),
}

/// Try to route one pad's escape stub to a fanned-out breakout. Returns
/// the accepted escape, or `None` if no clean stub was found.
#[allow(clippy::too_many_arguments)]
fn route_one(
    grid: &mut Grid,
    t: &EscapePad,
    info: &PadInfo,
    want_perp: f64,
    via_r: f64,
    clearance: f64,
    foreign: &[&PadRect],
    work: &Board,
    net_ids: &HashMap<String, u32>,
    accepted: &[Escape],
    opts: &RouteOptions,
) -> Option<Escape> {
    let target_id = *net_ids.get(&t.net)?;
    let (px, py) = info.perp;
    let width = opts.trace_width.to_mm();
    // Permissive fine-grid clearance: floor (so the truly-valid tight gap
    // isn't quantised away). Honest 0.20 mm is enforced afterwards by the
    // exact-geometry validator.
    let cell = FINE_CELL_MM;
    let clr_cells = (((clearance + width / 2.0) / cell).floor() as i32).max(1);
    let via_safe = 1;
    let cost_map = grid.new_cost_map();
    let huge_via_cost = 1_000_000u32; // keep the stub planar

    // Candidate breakouts: at the fanned perp, several depths; nudge perp
    // if the via won't fit. First clean one wins.
    let base_depth = info.half_len + BREAKOUT_DEPTH_MM;
    let perp_nudges = [0.0, 0.15, -0.15, 0.3, -0.3];
    // Interior direction first (inline connectors: interior IS the open
    // inter-row channel — USB-C byte-for-byte unchanged), then exterior
    // (centre-thermal QFNs whose interior is blocked by the exposed pad).
    // First fitting + routable + validated breakout wins; if neither
    // direction yields one, the caller falls back to via-in-pad.
    let (idx, idy) = info.dir;
    for (dx, dy) in [(idx, idy), (-idx, -idy)] {
        for ddepth in DEPTH_STEPS {
            let depth = base_depth + ddepth;
            for dn in perp_nudges {
                let target_perp = want_perp + dn;
                // Breakout = pad centre + dir*depth shifted to the fanned perp.
                let shift = target_perp - (px * t.cx + py * t.cy);
                let bx = t.cx + dx * depth + px * shift;
                let by = t.cy + dy * depth + py * shift;
                // Via must fit (exact) against foreign pads, existing vias, edge.
                if !fanout::fanout_via_fits(bx, by, &t.net, via_r, clearance, foreign, work) {
                    continue;
                }
                let breakout = Point::new(Length::from_mm(bx), Length::from_mm(by));
                // Stamp the breakout as a landing pad on the fine grid so the
                // search can terminate on it, then route pad → breakout.
                let via_copper = ((via_r / cell).round() as i32).max(1);
                grid.stamp_drilled_disk(breakout, via_copper, target_id);
                let start = grid.snap(t.center, t.layer);
                let target = grid.snap(breakout, t.layer);
                let res = search(
                    grid,
                    start,
                    target_id,
                    huge_via_cost,
                    target,
                    via_safe,
                    clr_cells,
                    &cost_map,
                    &[],
                    1.0,
                );
                // Un-stamp the trial landing (set back to free) regardless of
                // outcome; an accepted stub re-stamps its own copper.
                unstamp_disk(grid, breakout, via_copper);
                let Some(res) = res else { continue };
                // Build the world polyline, exact endpoints at pad centre and
                // breakout.
                let mut pts: Vec<Point> = res.path.iter().map(|gp| grid.unsnap(*gp)).collect();
                if pts.len() < 2 {
                    continue;
                }
                *pts.first_mut().unwrap() = t.center;
                *pts.last_mut().unwrap() = breakout;
                simplify_collinear(&mut pts);
                if !validate_polyline(
                    &pts, t.layer, &t.net, width, foreign, work, accepted, clearance,
                ) {
                    continue;
                }
                return Some(Escape {
                    pad_ref: t.pad_ref.clone(),
                    net: t.net.clone(),
                    layer: t.layer,
                    points: pts,
                    breakout,
                });
            }
        }
    }
    None
}

/// Exact-geometry clearance check (un-quantised) for a laid stub polyline:
/// every segment must keep `clearance` to all foreign pads, board vias,
/// already-accepted stubs/breakouts, and `EDGE_CLEARANCE_MM` to the board
/// edge.
#[allow(clippy::too_many_arguments)]
fn validate_polyline(
    pts: &[Point],
    layer: CopperLayer,
    net: &str,
    width: f64,
    foreign: &[&PadRect],
    work: &Board,
    accepted: &[Escape],
    clearance: f64,
) -> bool {
    let half = width / 2.0;
    for w in pts.windows(2) {
        let (ax, ay) = (w[0].x.to_mm(), w[0].y.to_mm());
        let (bx, by) = (w[1].x.to_mm(), w[1].y.to_mm());
        let len = ((bx - ax).powi(2) + (by - ay).powi(2)).sqrt();
        let n = (len / 0.02).ceil().max(1.0) as i32;
        for i in 0..=n {
            let f = f64::from(i) / f64::from(n);
            let sx = ax + f * (bx - ax);
            let sy = ay + f * (by - ay);
            // Foreign pads.
            for r in foreign {
                if r.net.as_deref() == Some(net) {
                    continue;
                }
                if fanout::point_rect_dist(sx, sy, r) < half + clearance - 1e-9 {
                    return false;
                }
            }
            // Board vias + accumulated breakout vias.
            for v in &work.vias {
                if v.net == net {
                    continue;
                }
                let d = ((sx - v.position.x.to_mm()).powi(2) + (sy - v.position.y.to_mm()).powi(2))
                    .sqrt();
                if d < half + v.diameter.to_mm() / 2.0 + clearance - 1e-9 {
                    return false;
                }
            }
            // Already-accepted stubs of other nets.
            for esc in accepted {
                if esc.net == net || esc.layer.index != layer.index {
                    continue;
                }
                for sw in esc.points.windows(2) {
                    let d = point_seg_dist(
                        sx,
                        sy,
                        sw[0].x.to_mm(),
                        sw[0].y.to_mm(),
                        sw[1].x.to_mm(),
                        sw[1].y.to_mm(),
                    );
                    if d < half + width / 2.0 + clearance - 1e-9 {
                        return false;
                    }
                }
            }
            // Board edge.
            if let Some(o) = work.outline {
                let edge = (sx - o.min.x.to_mm())
                    .min(o.max.x.to_mm() - sx)
                    .min(sy - o.min.y.to_mm())
                    .min(o.max.y.to_mm() - sy);
                if edge < half + EDGE_CLEARANCE_MM - 1e-9 {
                    return false;
                }
            }
        }
    }
    true
}

fn point_seg_dist(px: f64, py: f64, ax: f64, ay: f64, bx: f64, by: f64) -> f64 {
    let dx = bx - ax;
    let dy = by - ay;
    let l2 = dx * dx + dy * dy;
    if l2 < 1e-12 {
        return ((px - ax).powi(2) + (py - ay).powi(2)).sqrt();
    }
    let mut t = ((px - ax) * dx + (py - ay) * dy) / l2;
    t = t.clamp(0.0, 1.0);
    let cx = ax + t * dx;
    let cy = ay + t * dy;
    ((px - cx).powi(2) + (py - cy).powi(2)).sqrt()
}

/// Drop consecutive collinear points so the emitted polyline is minimal.
fn simplify_collinear(pts: &mut Vec<Point>) {
    if pts.len() < 3 {
        return;
    }
    let mut out = vec![pts[0]];
    for i in 1..pts.len() - 1 {
        let a = out.last().unwrap();
        let b = pts[i];
        let c = pts[i + 1];
        let cross = (b.x.to_mm() - a.x.to_mm()) * (c.y.to_mm() - a.y.to_mm())
            - (b.y.to_mm() - a.y.to_mm()) * (c.x.to_mm() - a.x.to_mm());
        if cross.abs() > 1e-6 {
            out.push(b);
        }
    }
    out.push(*pts.last().unwrap());
    *pts = out;
}

fn stamp_polyline(
    grid: &mut Grid,
    pts: &[Point],
    layer: CopperLayer,
    id: u32,
    opts: &RouteOptions,
) {
    let copper = (((opts.trace_width.to_mm() / 2.0) / FINE_CELL_MM).round() as i32).max(0);
    for w in pts.windows(2) {
        let a = grid.snap(w[0], layer);
        let b = grid.snap(w[1], layer);
        grid.stamp_trace(a, b, id, copper);
    }
}

/// Reset a trial landing disk back to `Free` (only cells we set to a
/// DrilledPad of the trial net — `stamp_drilled_disk` overwrites
/// unconditionally, so we just clear the disk footprint on every layer).
fn unstamp_disk(grid: &mut Grid, center: Point, copper: i32) {
    let gp = grid.snap(center, CopperLayer::Top);
    let copper = copper.max(0);
    let r2 = copper * copper;
    for layer in 0..grid.layer_count {
        for dr in -copper..=copper {
            for dc in -copper..=copper {
                if dc * dc + dr * dr > r2 {
                    continue;
                }
                let p = crate::grid::GridPoint {
                    layer,
                    col: gp.col + dc,
                    row: gp.row + dr,
                };
                if grid.in_bounds(p) && matches!(grid.get(p), Cell::DrilledPad(_)) {
                    grid.set(p, Cell::Free);
                }
            }
        }
    }
}

fn pad_rects_owned(board: &Board) -> Vec<PadRect> {
    fanout::pad_rects(board)
}

fn net_id_map(board: &Board) -> HashMap<String, u32> {
    let mut names: HashSet<String> = HashSet::new();
    for fp in board.footprints_in_order() {
        for pad in &fp.pads {
            if let Some(n) = &pad.net {
                names.insert(n.clone());
            }
        }
    }
    let mut v: Vec<String> = names.into_iter().collect();
    v.sort();
    v.into_iter()
        .enumerate()
        .map(|(i, n)| (n, i as u32))
        .collect()
}

fn footprint_centroid(fp: &pcb_core::Footprint) -> (f64, f64) {
    let (mut sx, mut sy, mut n) = (0.0, 0.0, 0.0);
    for pad in &fp.pads {
        let c = fp.pad_world_center(pad);
        sx += c.x.to_mm();
        sy += c.y.to_mm();
        n += 1.0;
    }
    if n > 0.0 {
        (sx / n, sy / n)
    } else {
        (0.0, 0.0)
    }
}

fn footprint_tile(fp: &pcb_core::Footprint, margin: f64) -> Rect {
    let mut min_x = f64::INFINITY;
    let mut min_y = f64::INFINITY;
    let mut max_x = f64::NEG_INFINITY;
    let mut max_y = f64::NEG_INFINITY;
    for pad in &fp.pads {
        let c = fp.pad_world_center(pad);
        let (w, h) = fp.pad_world_size(pad);
        let (cx, cy) = (c.x.to_mm(), c.y.to_mm());
        let (hw, hh) = (w.to_mm() / 2.0, h.to_mm() / 2.0);
        min_x = min_x.min(cx - hw);
        min_y = min_y.min(cy - hh);
        max_x = max_x.max(cx + hw);
        max_y = max_y.max(cy + hh);
    }
    Rect::from_corners(
        Point::new(
            Length::from_mm(min_x - margin),
            Length::from_mm(min_y - margin),
        ),
        Point::new(
            Length::from_mm(max_x + margin),
            Length::from_mm(max_y + margin),
        ),
    )
}
