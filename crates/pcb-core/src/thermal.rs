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
    if d1 != 0 && d2 != 0 && d3 != 0 && d4 != 0 && (d1 > 0) != (d2 > 0) && (d3 > 0) != (d4 > 0) {
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

/// A bite of copper restored just inside the pad edge so a spoke
/// segment overlaps the pad and reads as electrically bonded to it.
const SPOKE_OVERLAP: i64 = 50_000; // 0.05 mm

/// The four orthogonal N/S/E/W spoke candidates for a rectangular pad.
/// Each segment runs from `SPOKE_OVERLAP` inside the pad edge to `reach`
/// beyond it, so it bridges the pad copper to the surrounding pour.
fn orthogonal_candidates(
    center: Point,
    half_w: i64,
    half_h: i64,
    reach: i64,
) -> [(Point, Point); 4] {
    let cx = center.x.0;
    let cy = center.y.0;
    [
        // West.
        (
            Point::new(Length(cx - half_w - reach), center.y),
            Point::new(Length(cx - half_w + SPOKE_OVERLAP), center.y),
        ),
        // East.
        (
            Point::new(Length(cx + half_w - SPOKE_OVERLAP), center.y),
            Point::new(Length(cx + half_w + reach), center.y),
        ),
        // South.
        (
            Point::new(center.x, Length(cy - half_h - reach)),
            Point::new(center.x, Length(cy - half_h + SPOKE_OVERLAP)),
        ),
        // North.
        (
            Point::new(center.x, Length(cy + half_h - SPOKE_OVERLAP)),
            Point::new(center.x, Length(cy + half_h + reach)),
        ),
    ]
}

/// The four 45° diagonal spoke candidates, one per pad corner, used as a
/// fallback when every orthogonal spoke is boxed in by foreign copper. A
/// diagonal frequently threads between traffic that crowds the axes, so
/// the pad still bonds to its plane instead of floating. Each segment
/// starts `SPOKE_OVERLAP` inside the corner and extends `reach` outward
/// on both axes (a 45° ray).
fn diagonal_candidates(center: Point, half_w: i64, half_h: i64, reach: i64) -> [(Point, Point); 4] {
    let cx = center.x.0;
    let cy = center.y.0;
    let o = SPOKE_OVERLAP;
    [
        // South-west.
        (
            Point::new(Length(cx - half_w + o), Length(cy - half_h + o)),
            Point::new(Length(cx - half_w - reach), Length(cy - half_h - reach)),
        ),
        // South-east.
        (
            Point::new(Length(cx + half_w - o), Length(cy - half_h + o)),
            Point::new(Length(cx + half_w + reach), Length(cy - half_h - reach)),
        ),
        // North-west.
        (
            Point::new(Length(cx - half_w + o), Length(cy + half_h - o)),
            Point::new(Length(cx - half_w - reach), Length(cy + half_h + reach)),
        ),
        // North-east.
        (
            Point::new(Length(cx + half_w - o), Length(cy + half_h - o)),
            Point::new(Length(cx + half_w + reach), Length(cy + half_h + reach)),
        ),
    ]
}

/// One candidate spoke fired from the pad boundary at `angle` (radians).
/// The segment starts `SPOKE_OVERLAP` inside the boundary and runs
/// `reach` beyond it. The boundary exit point is where the ray from the
/// pad centre meets the pad's bounding box, so the bite into the pad is
/// correct for any direction, not just the cardinals.
fn angled_candidate(
    center: Point,
    half_w: i64,
    half_h: i64,
    reach: i64,
    angle: f64,
) -> (Point, Point) {
    let (dx, dy) = (angle.cos(), angle.sin());
    // Distance from centre to the bounding-box edge along this ray.
    let tx = if dx.abs() > 1e-9 {
        half_w as f64 / dx.abs()
    } else {
        f64::INFINITY
    };
    let ty = if dy.abs() > 1e-9 {
        half_h as f64 / dy.abs()
    } else {
        f64::INFINITY
    };
    let te = tx.min(ty);
    let o = SPOKE_OVERLAP as f64;
    let r = reach as f64;
    let ax = center.x.0 as f64 + dx * (te - o);
    let ay = center.y.0 as f64 + dy * (te - o);
    let bx = center.x.0 as f64 + dx * (te + r);
    let by = center.y.0 as f64 + dy * (te + r);
    (
        Point::new(Length(ax.round() as i64), Length(ay.round() as i64)),
        Point::new(Length(bx.round() as i64), Length(by.round() as i64)),
    )
}

/// Pick the thermal-relief spokes to emit for a same-net pad, never
/// leaving the pad isolated when a clear direction exists.
///
/// Resolution order, cheapest/cleanest first:
/// 1. Orthogonal N/S/E/W spokes — returned whenever at least one clears
///    every foreign net (the overwhelmingly common case).
/// 2. 45° diagonal spokes from the pad corners — tried when all four
///    axes are boxed in; a diagonal often slips between crowding traffic.
/// 3. A fine angular sweep — when even the diagonals fail, scan every
///    direction and fire a single spoke down the centre of the *widest*
///    clear arc. This rescues a pad hemmed in by traffic whose only gap
///    sits at an odd angle (e.g. a GND pad a foreign trace wraps around),
///    so it still bonds to its plane through the one safe opening.
///
/// The returned segments are exactly the ones the Gerber writer draws
/// and the renderer punches, so screen and fab agree. An empty result
/// means the pad genuinely cannot connect at this `clearance` without a
/// short — the caller should surface that (reroute needed) rather than
/// fabricate a short.
#[must_use]
#[allow(clippy::too_many_arguments)]
pub fn select_spokes<S: std::hash::BuildHasher>(
    center: Point,
    pad_w: Length,
    pad_h: Length,
    spoke_half: Length,
    clearance: Length,
    reach: Length,
    pad_net: &str,
    layer: Layer,
    board: &Board,
    orphan_traces: &HashSet<Id, S>,
    orphan_vias: &HashSet<Id, S>,
) -> Vec<(Point, Point)> {
    // Angular resolution of the last-resort sweep (every 2°).
    const STEPS: usize = 180;
    let half_w = pad_w.0 / 2;
    let half_h = pad_h.0 / 2;
    let r = reach.0;
    let clears = |&(a, b): &(Point, Point)| {
        spoke_clear(
            a,
            b,
            spoke_half,
            clearance,
            pad_net,
            layer,
            board,
            orphan_traces,
            orphan_vias,
        )
    };

    let ortho: Vec<(Point, Point)> = orthogonal_candidates(center, half_w, half_h, r)
        .into_iter()
        .filter(|c| clears(c))
        .collect();
    if !ortho.is_empty() {
        return ortho;
    }

    let diag: Vec<(Point, Point)> = diagonal_candidates(center, half_w, half_h, r)
        .into_iter()
        .filter(|c| clears(c))
        .collect();
    if !diag.is_empty() {
        return diag;
    }

    // Fine sweep: probe every 2° and find the widest run of consecutive
    // clear directions, then fire one spoke down its middle.
    let mut clear_at = [false; STEPS];
    for (i, slot) in clear_at.iter_mut().enumerate() {
        let angle = std::f64::consts::TAU * i as f64 / STEPS as f64;
        *slot = clears(&angled_candidate(center, half_w, half_h, r, angle));
    }
    if !clear_at.iter().any(|&c| c) {
        return Vec::new();
    }
    // Longest circular run of `true`, then take its midpoint index.
    let (mut best_start, mut best_len) = (0usize, 0usize);
    let mut i = 0usize;
    while i < STEPS {
        if !clear_at[i] {
            i += 1;
            continue;
        }
        let start = i;
        let mut len = 0usize;
        while len < STEPS && clear_at[(start + len) % STEPS] {
            len += 1;
        }
        if len > best_len {
            best_len = len;
            best_start = start;
        }
        i = start + len.max(1);
    }
    let mid = (best_start + best_len / 2) % STEPS;
    let angle = std::f64::consts::TAU * mid as f64 / STEPS as f64;
    let cand = angled_candidate(center, half_w, half_h, r, angle);
    if clears(&cand) {
        vec![cand]
    } else {
        Vec::new()
    }
}
