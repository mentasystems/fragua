//! Auto-stitch isolated plane pads.
//!
//! A same-net pad (e.g. GND) normally bonds to its copper pour through a
//! thermal-relief spoke. When the pad is boxed in by foreign copper on
//! its own layer, [`crate::thermal::select_spokes`] can't find a safe
//! spoke and the pad is left floating off the plane (see `verify_spokes`).
//!
//! For a board that carries the same net as a pour on *another* copper
//! layer (e.g. a GND plane on both Top and Bottom), the fix is a
//! stitching via: drop a same-net via at the pad so it punches down to
//! the other layer's plane, which floods solid up to the via. The pad
//! then ties to its net through the via even though its own layer is
//! blocked. Where the pad has room beside it, the via sits next to the
//! pad with a short stub trace; where it is fully boxed in, the via lands
//! inside the pad (via-in-pad), which is the only opening left.
//!
//! [`plan_stitches`] is pure — it reads the board and returns proposals.
//! [`apply_stitches`] mutates the board. A pad that cannot reach any
//! plane even with a via (no same-net pour has room anywhere reachable)
//! is reported in [`StitchPlan::unreachable`] so the caller can surface a
//! "reroute needed" instead of fabricating nothing.

use std::collections::HashSet;

use crate::board::{Board, Id, Layer, Trace, Via};
use crate::geometry::Point;
use crate::thermal::{seg_pt_dist2, seg_rect_dist2, select_spokes, POUR_CLEARANCE};
use crate::units::Length;

/// Inset from the board outline within which a via may tie to a pour —
/// matches the Gerber writer's pour edge clearance so a stitch never
/// lands where the pour itself is pulled back from the edge.
const EDGE_CLEARANCE: Length = Length(300_000); // 0.3 mm

/// One proposed stitch: a via plus an optional stub trace from the pad
/// to the via (absent when the via lands inside the pad).
#[derive(Debug, Clone)]
pub struct StitchProposal {
    /// `"U2.10"` — for reporting / logging.
    pub pad_ref: String,
    pub via: Via,
    pub stub: Option<Trace>,
    /// True when the via had to land inside the pad (fully boxed in).
    pub via_in_pad: bool,
}

/// Outcome of planning: the stitches to apply plus the pads that remain
/// genuinely unreachable (no plane within reach — a real reroute).
#[derive(Debug, Clone, Default)]
pub struct StitchPlan {
    pub proposals: Vec<StitchProposal>,
    /// `"U3.1"` of pads no via can connect — reroute needed.
    pub unreachable: Vec<String>,
}

/// Knobs for the stitch. Defaults mirror the router: 0.6 mm via, 0.3 mm
/// drill, 0.25 mm stub, and the fab pour clearance (0.2 mm).
#[derive(Debug, Clone, Copy)]
pub struct StitchParams {
    pub via_diameter: Length,
    pub via_drill: Length,
    pub clearance: Length,
    pub stub_width: Length,
}

impl Default for StitchParams {
    fn default() -> Self {
        Self {
            via_diameter: Length::from_mm(0.6),
            via_drill: Length::from_mm(0.3),
            clearance: POUR_CLEARANCE,
            stub_width: Length::from_mm(0.25),
        }
    }
}

/// Squared distance (nm²) from point `p` to the nearest different-net
/// copper item on `layer`, ignoring orphan (dangling) stubs. Same-net
/// copper is not an obstacle — the via/stub bonds to it.
fn min_dist2_to_foreign<S: std::hash::BuildHasher>(
    board: &Board,
    p: Point,
    layer: Layer,
    net: &str,
    orphan_traces: &HashSet<Id, S>,
    orphan_vias: &HashSet<Id, S>,
) -> i128 {
    let mut best = i128::MAX;
    for fp in board.footprints_in_order() {
        for pad in &fp.pads {
            if !pad.occupies_layer(layer) || pad.net.as_deref() == Some(net) {
                continue;
            }
            let center = fp.pad_world_center(pad);
            let (pw, ph) = fp.pad_world_size(pad);
            let rect = crate::geometry::Rect::from_center(center, pw, ph);
            best = best.min(seg_rect_dist2(p, p, rect));
        }
    }
    for t in &board.traces {
        if t.layer != layer || t.net == net || orphan_traces.contains(&t.id) {
            continue;
        }
        // Distance to the trace centre-line, then subtract half-width
        // (in squared space we compare against the inflated margin at the
        // call site, so return centre-line distance here and let callers
        // fold the half-width into their margin).
        let d2 = seg_pt_dist2(t.start, t.end, p);
        // Fold the trace half-width in immediately: clamp to >= 0.
        let edge = (d2.isqrt() - i128::from(t.width.0 / 2)).max(0);
        best = best.min(edge * edge);
    }
    for v in &board.vias {
        if v.net == net || orphan_vias.contains(&v.id) {
            continue;
        }
        let d2 = seg_pt_dist2(v.position, v.position, p);
        let edge = (d2.isqrt() - i128::from(v.diameter.0 / 2)).max(0);
        best = best.min(edge * edge);
    }
    best
}

/// True if a via of `radius` (copper) on `net` at `p` keeps `clearance`
/// from every foreign copper item on every layer it punches.
fn via_clears<S: std::hash::BuildHasher>(
    board: &Board,
    p: Point,
    radius: Length,
    clearance: Length,
    net: &str,
    orphan_traces: &HashSet<Id, S>,
    orphan_vias: &HashSet<Id, S>,
) -> bool {
    let margin = i128::from(radius.0 + clearance.0);
    let margin2 = margin * margin;
    let layers = board.stackup.layers.len().max(2);
    for idx in 0..layers {
        let layer = Layer { index: idx as u8 };
        if min_dist2_to_foreign(board, p, layer, net, orphan_traces, orphan_vias) < margin2 {
            return false;
        }
    }
    true
}

/// True if a same-net pour floods up to `p` on some layer — i.e. `p` is
/// inside the board outline (inset by the edge clearance) and clears all
/// foreign copper on that pour's layer by the pour clearance. That means
/// solid pour copper reaches the via, tying it to the plane.
fn ties_to_a_pour<S: std::hash::BuildHasher>(
    board: &Board,
    p: Point,
    net: &str,
    clearance: Length,
    orphan_traces: &HashSet<Id, S>,
    orphan_vias: &HashSet<Id, S>,
) -> bool {
    let Some(outline) = board.outline else {
        return false;
    };
    let inset = EDGE_CLEARANCE.0;
    if p.x.0 <= outline.min.x.0 + inset
        || p.x.0 >= outline.max.x.0 - inset
        || p.y.0 <= outline.min.y.0 + inset
        || p.y.0 >= outline.max.y.0 - inset
    {
        return false;
    }
    let margin = i128::from(clearance.0);
    let margin2 = margin * margin;
    for pour in &board.pours {
        if pour.net != net {
            continue;
        }
        if min_dist2_to_foreign(board, p, pour.layer, net, orphan_traces, orphan_vias) >= margin2 {
            return true;
        }
    }
    false
}

/// Decide whether `pad` (same-net as some pour) is left isolated from its
/// plane — every same-net pour on a layer the pad occupies fails to bond
/// it (a `Spokes4` pour with no surviving spoke), and no same-net trace
/// or via already touches it.
fn pad_is_isolated<S: std::hash::BuildHasher>(
    board: &Board,
    fp: &crate::board::Footprint,
    pad: &crate::board::Pad,
    net: &str,
    reach: Length,
    orphan_traces: &HashSet<Id, S>,
    orphan_vias: &HashSet<Id, S>,
) -> bool {
    let center = fp.pad_world_center(pad);
    let (pw, ph) = fp.pad_world_size(pad);
    let mut has_pour = false;
    for pour in &board.pours {
        if pour.net != net || !pad.occupies_layer(pour.layer) {
            continue;
        }
        has_pour = true;
        match pour.thermal_relief {
            crate::board::ThermalRelief::Solid => return false, // pour floods the pad
            crate::board::ThermalRelief::Spokes4 {
                spoke_width_mm, ..
            } => {
                let spoke_half = Length::from_mm(spoke_width_mm) / 2;
                let spokes = select_spokes(
                    center,
                    pw,
                    ph,
                    spoke_half,
                    POUR_CLEARANCE,
                    reach,
                    net,
                    pour.layer,
                    board,
                    orphan_traces,
                    orphan_vias,
                );
                if !spokes.is_empty() {
                    return false; // a spoke bonds it on this layer
                }
            }
        }
    }
    if !has_pour {
        return false;
    }
    // Already wired by real copper? Then it isn't floating.
    if same_net_trace_or_via_touches(board, center, pw, ph, net) {
        return false;
    }
    true
}

/// True if a same-net trace endpoint or via sits on the pad's copper.
fn same_net_trace_or_via_touches(
    board: &Board,
    center: Point,
    pw: Length,
    ph: Length,
    net: &str,
) -> bool {
    let hx = pw.0 / 2;
    let hy = ph.0 / 2;
    let inside = |p: Point| {
        (p.x.0 - center.x.0).abs() <= hx && (p.y.0 - center.y.0).abs() <= hy
    };
    board
        .traces
        .iter()
        .any(|t| t.net == net && (inside(t.start) || inside(t.end)))
        || board.vias.iter().any(|v| v.net == net && inside(v.position))
}

/// Plan stitching vias for every isolated plane pad on the board.
#[must_use]
pub fn plan_stitches(board: &Board, params: StitchParams) -> StitchPlan {
    let orphan_traces = board.orphan_trace_ids();
    let orphan_vias = board.orphan_via_ids();
    let via_radius = params.via_diameter / 2;
    // Spoke reach used to probe isolation: the largest pour gap + 0.1 mm.
    let reach = board
        .pours
        .iter()
        .filter_map(|p| match p.thermal_relief {
            crate::board::ThermalRelief::Spokes4 { gap_mm, .. } => Some(gap_mm),
            crate::board::ThermalRelief::Solid => None,
        })
        .fold(0.0_f64, f64::max);
    let reach = Length::from_mm(reach + 0.1);

    let mut plan = StitchPlan::default();
    for fp in board.footprints_in_order() {
        for pad in &fp.pads {
            let Some(net) = pad.net.as_deref() else {
                continue;
            };
            if !pad_is_isolated(board, fp, pad, net, reach, &orphan_traces, &orphan_vias) {
                continue;
            }
            let pad_ref = format!("{}.{}", fp.reference, pad.number);
            let center = fp.pad_world_center(pad);
            let (pw, ph) = fp.pad_world_size(pad);
            match find_via_spot(
                board,
                center,
                pw,
                ph,
                net,
                &params,
                via_radius,
                &orphan_traces,
                &orphan_vias,
            ) {
                Some((pos, via_in_pad)) => {
                    let via = Via {
                        id: Id::new(),
                        position: pos,
                        drill: params.via_drill,
                        diameter: params.via_diameter,
                        net: net.to_string(),
                    };
                    let stub = if via_in_pad {
                        None
                    } else {
                        Some(Trace {
                            id: Id::new(),
                            layer: pad.layer,
                            start: center,
                            end: pos,
                            width: params.stub_width,
                            net: net.to_string(),
                        })
                    };
                    plan.proposals.push(StitchProposal {
                        pad_ref,
                        via,
                        stub,
                        via_in_pad,
                    });
                }
                None => plan.unreachable.push(pad_ref),
            }
        }
    }
    plan
}

/// Find a via position for an isolated pad. Prefers a via *beside* the
/// pad reachable by a clear stub (the via never drills the pad); falls
/// back to a via inside the pad (its centre) when the pad is fully boxed
/// in. Returns `(position, via_in_pad)` or `None` if nothing connects.
#[allow(clippy::too_many_arguments)]
fn find_via_spot<S: std::hash::BuildHasher>(
    board: &Board,
    center: Point,
    pad_w: Length,
    pad_h: Length,
    net: &str,
    params: &StitchParams,
    via_radius: Length,
    orphan_traces: &HashSet<Id, S>,
    orphan_vias: &HashSet<Id, S>,
) -> Option<(Point, bool)> {
    let stub_half = params.stub_width / 2;
    // A position counts as "beside the pad" only if the via copper clears
    // the pad outline — otherwise it is really a via-in-pad and belongs to
    // phase 2 (no degenerate stub inside the pad).
    let hx = pad_w.0 / 2 + via_radius.0;
    let hy = pad_h.0 / 2 + via_radius.0;
    let outside_pad =
        |p: Point| (p.x.0 - center.x.0).abs() > hx || (p.y.0 - center.y.0).abs() > hy;
    // Phase 1: a via offset from the pad, reachable by a stub clear of
    // foreign copper on the pad's layer. Scan rings outward; nearest wins.
    let step = 100_000_i64; // 0.1 mm
    let max_r = 3_000_000_i64; // 3 mm search window
    let mut r = step;
    while r <= max_r {
        // 24 directions per ring.
        let mut best: Option<Point> = None;
        for k in 0..24 {
            let ang = std::f64::consts::TAU * f64::from(k) / 24.0;
            let dx = (f64::from(r as i32) * ang.cos()) as i64;
            let dy = (f64::from(r as i32) * ang.sin()) as i64;
            let pos = Point::new(Length(center.x.0 + dx), Length(center.y.0 + dy));
            if !outside_pad(pos) {
                continue;
            }
            if !via_clears(
                board,
                pos,
                via_radius,
                params.clearance,
                net,
                orphan_traces,
                orphan_vias,
            ) {
                continue;
            }
            if !ties_to_a_pour(board, pos, net, params.clearance, orphan_traces, orphan_vias) {
                continue;
            }
            // The stub from the pad to the via must clear foreign copper
            // on the pad's layer (checked per pour layer the net pours on
            // — use the via clearance against the stub centre-line).
            if !stub_clears(
                board,
                center,
                pos,
                stub_half,
                params.clearance,
                net,
                orphan_traces,
                orphan_vias,
            ) {
                continue;
            }
            best = Some(pos);
            break;
        }
        if let Some(pos) = best {
            return Some((pos, false));
        }
        r += step;
    }
    // Phase 2: via-in-pad — the pad's own copper is the only opening. The
    // via at the pad centre needs to clear foreign copper on every layer
    // and tie to a same-net pour (on the other plane).
    if via_clears(
        board,
        center,
        via_radius,
        params.clearance,
        net,
        orphan_traces,
        orphan_vias,
    ) && ties_to_a_pour(board, center, net, params.clearance, orphan_traces, orphan_vias)
    {
        return Some((center, true));
    }
    None
}

/// True if the stub segment `a`–`b` of half-width `stub_half` clears
/// every foreign copper item on every copper layer by `clearance`.
fn stub_clears<S: std::hash::BuildHasher>(
    board: &Board,
    a: Point,
    b: Point,
    stub_half: Length,
    clearance: Length,
    net: &str,
    orphan_traces: &HashSet<Id, S>,
    orphan_vias: &HashSet<Id, S>,
) -> bool {
    // The stub lives on one layer, but reuse the spoke clearance predicate
    // on the pad's layers. Conservatively check it on every layer that
    // carries a same-net pour (where it will be drawn / matters).
    let margin = i128::from(stub_half.0 + clearance.0);
    let margin2 = margin * margin;
    for pour in &board.pours {
        if pour.net != net {
            continue;
        }
        let layer = pour.layer;
        // foreign pads
        for fp in board.footprints_in_order() {
            for pad in &fp.pads {
                if !pad.occupies_layer(layer) || pad.net.as_deref() == Some(net) {
                    continue;
                }
                let rect = crate::geometry::Rect::from_center(
                    fp.pad_world_center(pad),
                    fp.pad_world_size(pad).0,
                    fp.pad_world_size(pad).1,
                );
                if crate::thermal::seg_rect_dist2(a, b, rect) < margin2 {
                    return false;
                }
            }
        }
        for t in &board.traces {
            if t.layer != layer || t.net == net || orphan_traces.contains(&t.id) {
                continue;
            }
            let m = i128::from(stub_half.0 + clearance.0 + t.width.0 / 2);
            if crate::thermal::seg_seg_dist2(a, b, t.start, t.end) < m * m {
                return false;
            }
        }
        for v in &board.vias {
            if v.net == net || orphan_vias.contains(&v.id) {
                continue;
            }
            let m = i128::from(stub_half.0 + clearance.0 + v.diameter.0 / 2);
            if crate::thermal::seg_pt_dist2(a, b, v.position) < m * m {
                return false;
            }
        }
    }
    true
}

/// Apply a plan to the board, pushing the vias and stub traces. Returns
/// the number of stitches added.
pub fn apply_stitches(board: &mut Board, plan: &StitchPlan) -> usize {
    for s in &plan.proposals {
        board.add_via(s.via.clone());
        if let Some(stub) = &s.stub {
            board.add_trace(stub.clone());
        }
    }
    plan.proposals.len()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::board::{Footprint, Pad, Pour, StitchPolicy, ThermalRelief};
    use crate::geometry::Rect;

    fn pad(num: &str, ox: f64, oy: f64, w: f64, h: f64, net: &str) -> Pad {
        Pad {
            number: num.into(),
            name: String::new(),
            offset: Point::new(Length::from_mm(ox), Length::from_mm(oy)),
            size: (Length::from_mm(w), Length::from_mm(h)),
            layer: Layer::TOP,
            net: Some(net.into()),
            drill: None,
        }
    }

    fn fp(reference: &str, x: f64, y: f64, pads: Vec<Pad>) -> Footprint {
        Footprint {
            id: Id::new(),
            reference: reference.into(),
            value: String::new(),
            library: "test".into(),
            position: Point::new(Length::from_mm(x), Length::from_mm(y)),
            rotation: 0.0,
            layer: Layer::TOP,
            pads,
            key: String::new(),
            description: String::new(),
            edge_mounted: false,
            silk: Vec::new(),
        }
    }

    /// A GND pad fully boxed in on Top by four large foreign pads (which
    /// also close the diagonals) gets a via-in-pad tying it to the Bottom
    /// GND plane.
    #[test]
    fn boxed_in_gnd_pad_gets_via_in_pad() {
        let mut b = Board::new();
        b.outline = Some(Rect::from_corners(
            Point::new(Length::from_mm(0.0), Length::from_mm(0.0)),
            Point::new(Length::from_mm(40.0), Length::from_mm(20.0)),
        ));
        // Isolated GND pad at board centre.
        b.add_footprint(fp("U1", 20.0, 10.0, vec![pad("10", 0.0, 0.0, 1.5, 1.5, "GND")]));
        // Four 3 mm foreign pads at 2.4 mm — they overlap at the corners,
        // leaving only a ~0.15 mm ring around the GND pad (no spoke,
        // diagonal or fine-sweep direction escapes).
        for (i, (dx, dy)) in [(2.4, 0.0), (-2.4, 0.0), (0.0, 2.4), (0.0, -2.4)]
            .into_iter()
            .enumerate()
        {
            b.add_footprint(fp(
                &format!("B{i}"),
                20.0 + dx,
                10.0 + dy,
                vec![pad("1", 0.0, 0.0, 3.0, 3.0, "SIG")],
            ));
        }
        // GND pours on BOTH layers — Top can't bond the boxed pad, Bottom
        // has room directly under it.
        b.add_pour(Pour {
            net: "GND".into(),
            layer: Layer::TOP,
            thermal_relief: ThermalRelief::Spokes4 {
                spoke_width_mm: 0.4,
                gap_mm: 0.4,
            },
            stitching: StitchPolicy::None,
        });
        b.add_pour(Pour {
            net: "GND".into(),
            layer: Layer::Bottom,
            thermal_relief: ThermalRelief::Solid,
            stitching: StitchPolicy::None,
        });

        let plan = plan_stitches(&b, StitchParams::default());
        assert_eq!(plan.proposals.len(), 1, "expected one stitch");
        assert!(plan.unreachable.is_empty(), "should be reachable via Bottom");
        let s = &plan.proposals[0];
        assert_eq!(s.pad_ref, "U1.10");
        assert!(s.via_in_pad, "fully boxed → via-in-pad");
        assert!(s.stub.is_none());
        assert_eq!(s.via.net, "GND");

        // After applying, nothing is isolated any more.
        let mut b2 = b.clone();
        apply_stitches(&mut b2, &plan);
        let after = plan_stitches(&b2, StitchParams::default());
        assert!(after.proposals.is_empty());
        assert!(after.unreachable.is_empty());
    }

    /// A pad already bonded by a surviving spoke (open surroundings) is
    /// left alone.
    #[test]
    fn connected_pad_is_not_stitched() {
        let mut b = Board::new();
        b.outline = Some(Rect::from_corners(
            Point::new(Length::from_mm(0.0), Length::from_mm(0.0)),
            Point::new(Length::from_mm(40.0), Length::from_mm(20.0)),
        ));
        b.add_footprint(fp("U1", 20.0, 10.0, vec![pad("1", 0.0, 0.0, 1.5, 1.5, "GND")]));
        b.add_pour(Pour {
            net: "GND".into(),
            layer: Layer::TOP,
            thermal_relief: ThermalRelief::Spokes4 {
                spoke_width_mm: 0.4,
                gap_mm: 0.4,
            },
            stitching: StitchPolicy::None,
        });
        let plan = plan_stitches(&b, StitchParams::default());
        assert!(plan.proposals.is_empty());
        assert!(plan.unreachable.is_empty());
    }
}
