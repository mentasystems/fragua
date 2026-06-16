//! Thermal-relief spoke collision geometry.
//!
//! A `Spokes4` thermal relief connects a same-net pad to the
//! surrounding pour with four narrow copper bars (N/S/E/W). Those bars
//! are *dark* copper laid down after the pour's clearance voids, so a
//! spoke that reaches across a foreign net's keepout would re-deposit
//! copper over it and short the pad/pour to that net.
//!
//! [`spoke_clear`] is the shared predicate both the Gerber writer and
//! the renderer use to decide whether a candidate spoke is safe to
//! emit: it is kept only if it stays at least `clearance` away from
//! every foreign-net pad, trace, and via on the layer. All math is in
//! `i128` over nanometre coordinates so it never overflows and matches
//! exactly between the two pipelines.

use std::collections::HashSet;

use crate::board::{Board, Id, Layer};
use crate::geometry::{Point, Rect};
use crate::units::Length;

/// Per-side copper clearance used as the short threshold when deciding
/// whether a thermal spoke may be emitted. This is the *fab* clearance
/// (matches the Gerber writer's pour clearance and the DRC's
/// `min_clearance`), NOT the renderer's larger visual void — both the
/// Gerber writer and the renderer pass this so they agree on exactly
/// which spokes survive.
pub const POUR_CLEARANCE: Length = Length(200_000); // 0.2 mm

/// Squared distance (nm²) from point `p` to segment `a`–`b`.
#[must_use]
pub fn seg_pt_dist2(a: Point, b: Point, p: Point) -> i128 {
    let abx = i128::from(b.x.0 - a.x.0);
    let aby = i128::from(b.y.0 - a.y.0);
    let apx = i128::from(p.x.0 - a.x.0);
    let apy = i128::from(p.y.0 - a.y.0);
    let denom = abx * abx + aby * aby;
    let (cx, cy) = if denom == 0 {
        (i128::from(a.x.0), i128::from(a.y.0))
    } else {
        let t = apx * abx + apy * aby;
        if t <= 0 {
            (i128::from(a.x.0), i128::from(a.y.0))
        } else if t >= denom {
            (i128::from(b.x.0), i128::from(b.y.0))
        } else {
            (
                i128::from(a.x.0) + t * abx / denom,
                i128::from(a.y.0) + t * aby / denom,
            )
        }
    };
    let dx = i128::from(p.x.0) - cx;
    let dy = i128::from(p.y.0) - cy;
    dx * dx + dy * dy
}

/// Cross product of `a→b` and `a→c` (twice the signed triangle area).
fn orient(a: Point, b: Point, c: Point) -> i128 {
    let abx = i128::from(b.x.0 - a.x.0);
    let aby = i128::from(b.y.0 - a.y.0);
    let acx = i128::from(c.x.0 - a.x.0);
    let acy = i128::from(c.y.0 - a.y.0);
    abx * acy - aby * acx
}

/// True if `p` lies within the bounding box of segment `a`–`b` (the
/// colinear case of [`segs_intersect`]).
fn on_seg(a: Point, b: Point, p: Point) -> bool {
    p.x.0 >= a.x.0.min(b.x.0)
        && p.x.0 <= a.x.0.max(b.x.0)
        && p.y.0 >= a.y.0.min(b.y.0)
        && p.y.0 <= a.y.0.max(b.y.0)
}

/// True if segments `a`–`b` and `c`–`d` cross or touch.
fn segs_intersect(a: Point, b: Point, c: Point, d: Point) -> bool {
    let d1 = orient(c, d, a);
    let d2 = orient(c, d, b);
    let d3 = orient(a, b, c);
    let d4 = orient(a, b, d);
    if d1 != 0
        && d2 != 0
        && d3 != 0
        && d4 != 0
        && (d1 > 0) != (d2 > 0)
        && (d3 > 0) != (d4 > 0)
    {
        return true;
    }
    (d1 == 0 && on_seg(c, d, a))
        || (d2 == 0 && on_seg(c, d, b))
        || (d3 == 0 && on_seg(a, b, c))
        || (d4 == 0 && on_seg(a, b, d))
}

/// Squared distance (nm²) between segments `a`–`b` and `c`–`d`.
#[must_use]
pub fn seg_seg_dist2(a: Point, b: Point, c: Point, d: Point) -> i128 {
    if segs_intersect(a, b, c, d) {
        return 0;
    }
    seg_pt_dist2(a, b, c)
        .min(seg_pt_dist2(a, b, d))
        .min(seg_pt_dist2(c, d, a))
        .min(seg_pt_dist2(c, d, b))
}

/// True if `p` is inside (or on) axis-aligned rectangle `r`.
fn pt_in_rect(p: Point, r: Rect) -> bool {
    p.x.0 >= r.min.x.0 && p.x.0 <= r.max.x.0 && p.y.0 >= r.min.y.0 && p.y.0 <= r.max.y.0
}

/// Squared distance (nm²) between segment `a`–`b` and rectangle `r`.
#[must_use]
pub fn seg_rect_dist2(a: Point, b: Point, r: Rect) -> i128 {
    if pt_in_rect(a, r) || pt_in_rect(b, r) {
        return 0;
    }
    let c0 = r.min;
    let c1 = Point::new(r.max.x, r.min.y);
    let c2 = r.max;
    let c3 = Point::new(r.min.x, r.max.y);
    seg_seg_dist2(a, b, c0, c1)
        .min(seg_seg_dist2(a, b, c1, c2))
        .min(seg_seg_dist2(a, b, c2, c3))
        .min(seg_seg_dist2(a, b, c3, c0))
}

/// True if a thermal-relief spoke segment `a`–`b` of half-width
/// `spoke_half` (belonging to `pad_net`) keeps at least `clearance`
/// from every foreign-net copper item on `layer`. A spoke that fails
/// this would re-deposit copper across a foreign net's clearance void
/// and short the pad/pour to that net, so the caller drops it.
///
/// `orphan_traces` / `orphan_vias` are the ids the exporters already
/// skip (dangling stubs that carry no real copper); they are ignored
/// here too.
#[must_use]
#[allow(clippy::too_many_arguments)]
pub fn spoke_clear<S: std::hash::BuildHasher>(
    a: Point,
    b: Point,
    spoke_half: Length,
    clearance: Length,
    pad_net: &str,
    layer: Layer,
    board: &Board,
    orphan_traces: &HashSet<Id, S>,
    orphan_vias: &HashSet<Id, S>,
) -> bool {
    let base = spoke_half.0 + clearance.0;
    // Foreign-net pads (full rectangular copper extent).
    for fp in board.footprints_in_order() {
        for pad in &fp.pads {
            if !pad.occupies_layer(layer) || pad.net.as_deref() == Some(pad_net) {
                continue;
            }
            let center = fp.pad_world_center(pad);
            let (pw, ph) = fp.pad_world_size(pad);
            let rect = Rect::from_center(center, pw, ph);
            let margin = i128::from(base);
            if seg_rect_dist2(a, b, rect) < margin * margin {
                return false;
            }
        }
    }
    // Foreign-net traces on this layer.
    for t in &board.traces {
        if t.layer != layer || t.net == pad_net || orphan_traces.contains(&t.id) {
            continue;
        }
        let margin = i128::from(base + t.width.0 / 2);
        if seg_seg_dist2(a, b, t.start, t.end) < margin * margin {
            return false;
        }
    }
    // Foreign-net vias (a via punches every copper layer).
    for v in &board.vias {
        if v.net == pad_net || orphan_vias.contains(&v.id) {
            continue;
        }
        let margin = i128::from(base + v.diameter.0 / 2);
        if seg_pt_dist2(a, b, v.position) < margin * margin {
            return false;
        }
    }
    true
}
