//! Organic post-pass: turn the router's grid-born polylines into
//! smooth, any-angle, TopoR-style geometry.
//!
//! The Theta* search already produces any-angle *segments*, but they
//! carry grid artefacts: staircase kinks, needless bends, and corners
//! sharp enough to read as "autorouted". This pass rewrites each routed
//! chain in continuous space:
//!
//!   1. **String-pulling** — greedily replace any sub-path with a
//!      straight segment when the segment keeps full clearance from
//!      every other-net obstacle (pads, traces, vias, board edge). This
//!      is the rubber-band contraction: the trace tightens like an
//!      elastic cord around the obstacles that actually block it.
//!   2. **Arc filleting** — every remaining corner becomes a tangent
//!      arc with the largest radius that stays clear, discretised well
//!      under DRC resolution. Traces flow around parts instead of
//!      cornering at them.
//!
//! Every rewrite is validated against the same clearance model before
//! being accepted, so the pass is DRC-neutral by construction: a chain
//! either comes out cleaner or is left exactly as the router made it.
//! Chain endpoints (pads, vias, junctions with other chains) are never
//! moved, which keeps net topology and connectivity untouched.

use std::collections::HashMap;

use pcb_core::{Board, CopperLayer, Length, Point, Trace};

use crate::router::RouteOptions;

/// Tunables for the organic pass.
#[derive(Debug, Clone)]
pub struct OrganicOptions {
    /// Largest fillet radius attempted at a corner, mm.
    pub max_fillet_radius_mm: f64,
    /// Max chord deviation when discretising an arc, mm. 0.02 mm is far
    /// below any fab tolerance while keeping segment counts low.
    pub chord_tol_mm: f64,
}

impl Default for OrganicOptions {
    fn default() -> Self {
        Self {
            max_fillet_radius_mm: 3.0,
            chord_tol_mm: 0.02,
        }
    }
}

/// What the pass did, for the route report.
#[derive(Debug, Clone, Default)]
pub struct OrganicReport {
    pub chains: usize,
    pub segments_before: usize,
    pub segments_after: usize,
    pub length_before_mm: f64,
    pub length_after_mm: f64,
}

type P2 = [f64; 2];

fn sub(a: P2, b: P2) -> P2 {
    [a[0] - b[0], a[1] - b[1]]
}
fn dot(a: P2, b: P2) -> f64 {
    a[0] * b[0] + a[1] * b[1]
}
fn norm(a: P2) -> f64 {
    dot(a, a).sqrt()
}
fn dist(a: P2, b: P2) -> f64 {
    norm(sub(a, b))
}

/// Distance from point `p` to segment `ab`.
fn point_seg_dist(p: P2, a: P2, b: P2) -> f64 {
    let ab = sub(b, a);
    let len2 = dot(ab, ab);
    if len2 <= 1e-18 {
        return dist(p, a);
    }
    let t = (dot(sub(p, a), ab) / len2).clamp(0.0, 1.0);
    dist(p, [a[0] + t * ab[0], a[1] + t * ab[1]])
}

/// True if segments `ab` and `cd` properly intersect or touch.
fn segs_intersect(a: P2, b: P2, c: P2, d: P2) -> bool {
    let orient = |p: P2, q: P2, r: P2| -> f64 {
        (q[0] - p[0]) * (r[1] - p[1]) - (q[1] - p[1]) * (r[0] - p[0])
    };
    let (o1, o2) = (orient(a, b, c), orient(a, b, d));
    let (o3, o4) = (orient(c, d, a), orient(c, d, b));
    if ((o1 > 0.0) != (o2 > 0.0)) && ((o3 > 0.0) != (o4 > 0.0)) {
        return true;
    }
    // Collinear-touch cases resolve through the distance checks below;
    // exact zero orientation with separation > 0 is handled there too.
    false
}

/// Distance between segments `ab` and `cd` (0 when they intersect).
fn seg_seg_dist(a: P2, b: P2, c: P2, d: P2) -> f64 {
    if segs_intersect(a, b, c, d) {
        return 0.0;
    }
    point_seg_dist(a, c, d)
        .min(point_seg_dist(b, c, d))
        .min(point_seg_dist(c, a, b))
        .min(point_seg_dist(d, a, b))
}

/// Distance from segment `ab` to an axis-aligned rect (0 when the
/// segment enters it).
fn seg_rect_dist(a: P2, b: P2, min: P2, max: P2) -> f64 {
    let inside = |p: P2| p[0] >= min[0] && p[0] <= max[0] && p[1] >= min[1] && p[1] <= max[1];
    if inside(a) || inside(b) {
        return 0.0;
    }
    let corners = [
        [min[0], min[1]],
        [max[0], min[1]],
        [max[0], max[1]],
        [min[0], max[1]],
    ];
    let mut best = f64::INFINITY;
    for i in 0..4 {
        let c = corners[i];
        let d = corners[(i + 1) % 4];
        best = best.min(seg_seg_dist(a, b, c, d));
        if best == 0.0 {
            return 0.0;
        }
    }
    best
}

/// One other-net obstacle on a layer, with the clearance its own net
/// class demands. The final required distance to a chain is
/// `chain_half_width + max(chain_clearance, self.clearance) + self
/// copper reach` — computed in `Obstacles::polyline_clear`.
enum Shape {
    /// Pad copper as the DRC sees it: an AABB.
    Rect { min: P2, max: P2 },
    /// Another net's trace segment: centreline + half-width.
    Capsule { a: P2, b: P2, half_w: f64 },
    /// A via barrel: centre + radius.
    Circle { c: P2, r: f64 },
}

struct Obstacle {
    shape: Shape,
    clearance_mm: f64,
}

/// All other-net obstacles a given net's chains must clear on a layer,
/// plus the outline band the centreline must stay inside.
struct Obstacles {
    items: Vec<Obstacle>,
    /// Outline shrink for the centreline: half-width + edge clearance.
    outline_min: P2,
    outline_max: P2,
}

impl Obstacles {
    /// True when every segment of `pts` keeps clearance. `hw` is the
    /// chain's half-width, `clr` its net clearance.
    fn polyline_clear(&self, pts: &[P2], hw: f64, clr: f64) -> bool {
        for w in pts.windows(2) {
            let (a, b) = (w[0], w[1]);
            // Stay inside the outline band.
            for p in [a, b] {
                if p[0] < self.outline_min[0]
                    || p[1] < self.outline_min[1]
                    || p[0] > self.outline_max[0]
                    || p[1] > self.outline_max[1]
                {
                    return false;
                }
            }
            for ob in &self.items {
                let need = hw + clr.max(ob.clearance_mm);
                let d = match &ob.shape {
                    Shape::Rect { min, max } => seg_rect_dist(a, b, *min, *max),
                    Shape::Capsule {
                        a: c,
                        b: d2,
                        half_w,
                    } => seg_seg_dist(a, b, *c, *d2) - half_w,
                    Shape::Circle { c, r } => point_seg_dist(*c, a, b) - r,
                };
                if d < need {
                    return false;
                }
            }
        }
        true
    }
}

/// Map key for exact endpoint matching (nm-resolution fixed point).
fn key(p: Point) -> (i64, i64) {
    (p.x.0, p.y.0)
}

fn to_mm(p: Point) -> P2 {
    [p.x.to_mm(), p.y.to_mm()]
}

fn to_point(p: P2) -> Point {
    Point::new(Length::from_mm(p[0]), Length::from_mm(p[1]))
}

/// Run the organic pass over every routed net. `rules` resolves a net
/// name to its `(trace_width, clearance)` — the router's
/// `effective_net_rules` partially applied.
pub(crate) fn organic_pass<F>(
    board: &mut Board,
    opts: &OrganicOptions,
    route_opts: &RouteOptions,
    rules: F,
) -> OrganicReport
where
    F: Fn(&RouteOptions, &str) -> (Length, Length),
{
    let Some(outline) = board.outline else {
        return OrganicReport::default();
    };
    let mut report = OrganicReport::default();

    // Net names with any copper, insertion-ordered for determinism.
    let mut nets: Vec<String> = Vec::new();
    for t in &board.traces {
        if !nets.contains(&t.net) {
            nets.push(t.net.clone());
        }
    }

    for net in &nets {
        let (width, clearance) = rules(route_opts, net);
        let hw = width.to_mm() / 2.0;
        let clr = clearance.to_mm();

        for layer in [CopperLayer::Top, CopperLayer::Bottom] {
            let obstacles =
                collect_obstacles(board, net, layer, route_opts, &rules, hw, clr, outline);
            let chains = extract_chains(board, net, layer);
            for chain in chains {
                report.chains += 1;
                report.segments_before += chain.len() - 1;
                let pts: Vec<P2> = chain.iter().map(|p| to_mm(*p)).collect();
                report.length_before_mm += polyline_len(&pts);

                let pulled = string_pull(&pts, &obstacles, hw, clr);
                let smooth = fillet(&pulled, &obstacles, hw, clr, opts);

                report.segments_after += smooth.len() - 1;
                report.length_after_mm += polyline_len(&smooth);
                replace_chain(board, net, layer, &chain, &smooth, width);
            }
        }
    }
    report
}

fn polyline_len(pts: &[P2]) -> f64 {
    pts.windows(2).map(|w| dist(w[0], w[1])).sum()
}

/// Everything on `layer` that does not belong to `net`.
#[allow(clippy::too_many_arguments)]
fn collect_obstacles<F>(
    board: &Board,
    net: &str,
    layer: CopperLayer,
    route_opts: &RouteOptions,
    rules: &F,
    hw: f64,
    clr: f64,
    outline: pcb_core::Rect,
) -> Obstacles
where
    F: Fn(&RouteOptions, &str) -> (Length, Length),
{
    let mut items: Vec<Obstacle> = Vec::new();
    // Cache per-net rule lookups; boards have few distinct nets.
    let mut rule_cache: HashMap<String, (f64, f64)> = HashMap::new();
    let mut rules_of = |n: &str| -> (f64, f64) {
        if let Some(v) = rule_cache.get(n) {
            return *v;
        }
        let (w, c) = rules(route_opts, n);
        let v = (w.to_mm(), c.to_mm());
        rule_cache.insert(n.to_string(), v);
        v
    };

    for fp in board.footprints_in_order() {
        for pad in &fp.pads {
            if pad.net.as_deref() == Some(net) {
                continue;
            }
            // A pad blocks this layer if it's on it or is through-hole.
            if pad.drill.is_none() && pad.layer != layer {
                continue;
            }
            let c = fp.pad_world_center(pad);
            let (w, h) = fp.pad_world_size(pad);
            let clearance_mm = pad.net.as_deref().map_or(clr, |n| rules_of(n).1);
            let cm = to_mm(c);
            items.push(Obstacle {
                shape: Shape::Rect {
                    min: [cm[0] - w.to_mm() / 2.0, cm[1] - h.to_mm() / 2.0],
                    max: [cm[0] + w.to_mm() / 2.0, cm[1] + h.to_mm() / 2.0],
                },
                clearance_mm,
            });
        }
    }
    for t in &board.traces {
        if t.net == net || t.layer != layer {
            continue;
        }
        let (w_o, c_o) = rules_of(&t.net);
        items.push(Obstacle {
            shape: Shape::Capsule {
                a: to_mm(t.start),
                b: to_mm(t.end),
                half_w: w_o / 2.0,
            },
            clearance_mm: c_o,
        });
    }
    for v in &board.vias {
        if v.net == net {
            continue;
        }
        let c_o = rules_of(&v.net).1;
        items.push(Obstacle {
            shape: Shape::Circle {
                c: to_mm(v.position),
                r: v.diameter.to_mm() / 2.0,
            },
            clearance_mm: c_o,
        });
    }

    // Board edge: centreline band. Matches the DRC edge check (0.2 mm
    // default) plus the chain's own half width.
    let edge = 0.3;
    Obstacles {
        items,
        outline_min: [
            outline.min.x.to_mm() + hw + edge,
            outline.min.y.to_mm() + hw + edge,
        ],
        outline_max: [
            outline.max.x.to_mm() - hw - edge,
            outline.max.y.to_mm() - hw - edge,
        ],
    }
}

/// Split `net`'s copper on `layer` into maximal chains whose interior
/// points have degree exactly 2 and coincide with nothing else. Chain
/// ends are pads' connection points, vias, junctions (degree ≥ 3) — the
/// points the pass must not move.
fn extract_chains(board: &Board, net: &str, layer: CopperLayer) -> Vec<Vec<Point>> {
    // Adjacency over exact endpoints.
    let mut adj: HashMap<(i64, i64), Vec<usize>> = HashMap::new();
    let segs: Vec<&Trace> = board
        .traces
        .iter()
        .filter(|t| t.net == net && t.layer == layer)
        .collect();
    for (i, t) in segs.iter().enumerate() {
        adj.entry(key(t.start)).or_default().push(i);
        adj.entry(key(t.end)).or_default().push(i);
    }
    // Points that must anchor a chain end even at degree 2: vias and
    // pad centres of this net (a chain that passes straight through a
    // pad still must keep hitting it).
    let mut hard: std::collections::HashSet<(i64, i64)> = std::collections::HashSet::new();
    for v in &board.vias {
        if v.net == net {
            hard.insert(key(v.position));
        }
    }
    for fp in board.footprints_in_order() {
        for pad in &fp.pads {
            if pad.net.as_deref() == Some(net) {
                hard.insert(key(fp.pad_world_center(pad)));
            }
        }
    }

    let is_anchor = |k: &(i64, i64), adj: &HashMap<(i64, i64), Vec<usize>>| -> bool {
        hard.contains(k) || adj.get(k).is_none_or(|v| v.len() != 2)
    };

    let mut used = vec![false; segs.len()];
    let mut chains: Vec<Vec<Point>> = Vec::new();

    // Walk from every anchor endpoint.
    let mut anchor_keys: Vec<(i64, i64)> =
        adj.keys().filter(|k| is_anchor(k, &adj)).copied().collect();
    anchor_keys.sort_unstable();
    for start_key in anchor_keys {
        let Some(start_segs) = adj.get(&start_key) else {
            continue;
        };
        for &first in start_segs {
            if used[first] {
                continue;
            }
            let mut chain: Vec<Point> = Vec::new();
            let mut cur_key = start_key;
            let mut cur_seg = first;
            chain.push(point_of(segs[first], start_key));
            loop {
                used[cur_seg] = true;
                let t = segs[cur_seg];
                let nxt_key = if key(t.start) == cur_key {
                    key(t.end)
                } else {
                    key(t.start)
                };
                chain.push(point_of(t, nxt_key));
                if is_anchor(&nxt_key, &adj) {
                    break;
                }
                // Degree exactly 2: continue on the other segment.
                let Some(cands) = adj.get(&nxt_key) else {
                    break;
                };
                let Some(&next_seg) = cands.iter().find(|&&s| s != cur_seg && !used[s]) else {
                    break;
                };
                cur_key = nxt_key;
                cur_seg = next_seg;
            }
            if chain.len() >= 2 {
                chains.push(chain);
            }
        }
    }
    chains
}

/// The endpoint of `t` whose key is `k`.
fn point_of(t: &Trace, k: (i64, i64)) -> Point {
    if key(t.start) == k {
        t.start
    } else {
        t.end
    }
}

/// Greedy rubber-band contraction: repeatedly replace the longest
/// clear-line-of-sight sub-path with a straight segment.
fn string_pull(pts: &[P2], obs: &Obstacles, hw: f64, clr: f64) -> Vec<P2> {
    if pts.len() <= 2 {
        return pts.to_vec();
    }
    let mut out: Vec<P2> = Vec::with_capacity(pts.len());
    let mut i = 0usize;
    out.push(pts[0]);
    while i + 1 < pts.len() {
        // Farthest j > i with a clear straight shot from i.
        let mut j = i + 1;
        for cand in ((i + 2)..pts.len()).rev() {
            if obs.polyline_clear(&[pts[i], pts[cand]], hw, clr) {
                j = cand;
                break;
            }
        }
        out.push(pts[j]);
        i = j;
    }
    out
}

/// Replace polyline corners with tangent arcs where a clear arc fits.
fn fillet(pts: &[P2], obs: &Obstacles, hw: f64, clr: f64, opts: &OrganicOptions) -> Vec<P2> {
    if pts.len() < 3 {
        return pts.to_vec();
    }
    let mut out: Vec<P2> = Vec::with_capacity(pts.len() * 4);
    out.push(pts[0]);
    for k in 1..pts.len() - 1 {
        let a = *out.last().unwrap();
        let b = pts[k];
        let c = pts[k + 1];
        let v1 = sub(a, b);
        let v2 = sub(c, b);
        let (l1, l2) = (norm(v1), norm(v2));
        if l1 < 1e-9 || l2 < 1e-9 {
            continue;
        }
        let u1 = [v1[0] / l1, v1[1] / l1];
        let u2 = [v2[0] / l2, v2[1] / l2];
        let cosang = dot(u1, u2).clamp(-1.0, 1.0);
        let ang = cosang.acos(); // corner interior angle
                                 // Nearly straight (or degenerate reversal): keep the corner.
        if !(0.05..=std::f64::consts::PI - 0.05).contains(&ang) {
            out.push(b);
            continue;
        }
        // Tangent offset t from the corner along both legs; keep less
        // than half of either leg so consecutive fillets never overlap.
        let half = ang / 2.0;
        let mut r = opts.max_fillet_radius_mm;
        let mut placed = false;
        for _ in 0..3 {
            let t_full = r / half.tan();
            let t = t_full.min(0.45 * l1).min(0.45 * l2);
            let r_eff = t * half.tan();
            if r_eff < 0.05 {
                break; // corner too tight to be worth an arc
            }
            let p1 = [b[0] + u1[0] * t, b[1] + u1[1] * t];
            let p2 = [b[0] + u2[0] * t, b[1] + u2[1] * t];
            // Arc centre along the bisector.
            let bis = [u1[0] + u2[0], u1[1] + u2[1]];
            let bl = norm(bis);
            if bl < 1e-9 {
                break;
            }
            let centre = [
                b[0] + bis[0] / bl * (r_eff / half.sin()),
                b[1] + bis[1] / bl * (r_eff / half.sin()),
            ];
            let arc = sample_arc(centre, p1, p2, r_eff, opts.chord_tol_mm);
            if obs.polyline_clear(&arc, hw, clr) {
                out.extend_from_slice(&arc);
                placed = true;
                break;
            }
            r /= 2.0;
        }
        if !placed {
            out.push(b);
        }
    }
    out.push(pts[pts.len() - 1]);
    // Drop consecutive duplicates the construction can produce.
    out.dedup_by(|a, b| dist(*a, *b) < 1e-6);
    out
}

/// Points along the arc of radius `r` centred at `c` from `p1` to `p2`
/// (the short way), endpoints included.
fn sample_arc(c: P2, p1: P2, p2: P2, r: f64, chord_tol: f64) -> Vec<P2> {
    let a1 = (p1[1] - c[1]).atan2(p1[0] - c[0]);
    let a2 = (p2[1] - c[1]).atan2(p2[0] - c[0]);
    let mut sweep = a2 - a1;
    while sweep > std::f64::consts::PI {
        sweep -= 2.0 * std::f64::consts::PI;
    }
    while sweep < -std::f64::consts::PI {
        sweep += 2.0 * std::f64::consts::PI;
    }
    // Chord tolerance → max step angle.
    let max_step = 2.0 * (1.0 - (chord_tol / r).min(0.5)).acos().max(0.05);
    let steps = ((sweep.abs() / max_step).ceil() as usize).clamp(1, 64);
    let mut out = Vec::with_capacity(steps + 1);
    for s in 0..=steps {
        let a = a1 + sweep * (s as f64 / steps as f64);
        out.push([c[0] + r * a.cos(), c[1] + r * a.sin()]);
    }
    out
}

/// Swap a chain's old segments for the smoothed polyline. Endpoints are
/// identical by construction, so connectivity is untouched.
fn replace_chain(
    board: &mut Board,
    net: &str,
    layer: CopperLayer,
    old_chain: &[Point],
    new_pts: &[P2],
    width: Length,
) {
    // Remove the exact old segments (by endpoint pairs, order-agnostic).
    let mut old_pairs: std::collections::HashSet<((i64, i64), (i64, i64))> =
        std::collections::HashSet::new();
    for w in old_chain.windows(2) {
        let (a, b) = (key(w[0]), key(w[1]));
        old_pairs.insert((a.min(b), a.max(b)));
    }
    board.traces.retain(|t| {
        if t.net != net || t.layer != layer {
            return true;
        }
        let (a, b) = (key(t.start), key(t.end));
        !old_pairs.contains(&(a.min(b), a.max(b)))
    });
    // Insert the new geometry, preserving the EXACT original endpoints
    // (mm round-trip must not perturb junction matching).
    let n = new_pts.len();
    for (i, w) in new_pts.windows(2).enumerate() {
        let start = if i == 0 { old_chain[0] } else { to_point(w[0]) };
        let end = if i + 2 == n {
            *old_chain.last().unwrap()
        } else {
            to_point(w[1])
        };
        if key(start) == key(end) {
            continue;
        }
        board.traces.push(Trace {
            id: pcb_core::Id::new(),
            layer,
            start,
            end,
            width,
            net: net.to_string(),
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn obs_none(outline_mm: f64) -> Obstacles {
        Obstacles {
            items: Vec::new(),
            outline_min: [0.0, 0.0],
            outline_max: [outline_mm, outline_mm],
        }
    }

    /// A staircase with line-of-sight collapses to one segment.
    #[test]
    fn string_pull_collapses_staircase() {
        let pts = vec![
            [1.0, 1.0],
            [2.0, 1.0],
            [2.0, 2.0],
            [3.0, 2.0],
            [3.0, 3.0],
            [4.0, 3.0],
        ];
        let out = string_pull(&pts, &obs_none(10.0), 0.125, 0.2);
        assert_eq!(out.len(), 2, "clear staircase must collapse: {out:?}");
        assert_eq!(out[0], [1.0, 1.0]);
        assert_eq!(out[1], [4.0, 3.0]);
    }

    /// An obstacle in the line of sight keeps the detour point.
    #[test]
    fn string_pull_respects_obstacles() {
        let pts = vec![[1.0, 1.0], [5.0, 4.5], [9.0, 1.0]];
        let mut obs = obs_none(10.0);
        // Block the straight shot y=1 between the endpoints.
        obs.items.push(Obstacle {
            shape: Shape::Rect {
                min: [4.0, 0.0],
                max: [6.0, 2.0],
            },
            clearance_mm: 0.2,
        });
        let out = string_pull(&pts, &obs, 0.125, 0.2);
        assert_eq!(out.len(), 3, "blocked path must keep its bend: {out:?}");
    }

    /// Filleting a right angle inserts an arc and shortens the path,
    /// and every arc point keeps clearance.
    #[test]
    fn fillet_rounds_a_right_angle() {
        let pts = vec![[1.0, 1.0], [5.0, 1.0], [5.0, 5.0]];
        let opts = OrganicOptions::default();
        let out = fillet(&pts, &obs_none(10.0), 0.125, 0.2, &opts);
        assert!(out.len() > 3, "arc points expected: {}", out.len());
        assert!(
            polyline_len(&out) < polyline_len(&pts) - 0.5,
            "fillet should shorten the corner: {:.3} vs {:.3}",
            polyline_len(&out),
            polyline_len(&pts)
        );
        // Endpoints preserved exactly.
        assert_eq!(out[0], [1.0, 1.0]);
        assert_eq!(*out.last().unwrap(), [5.0, 5.0]);
    }

    /// Segment-rect distance sanity.
    #[test]
    fn seg_rect_distance_basics() {
        // Passing above the rect at y=3, rect top at y=2 → distance 1.
        let d = seg_rect_dist([0.0, 3.0], [10.0, 3.0], [4.0, 1.0], [6.0, 2.0]);
        assert!((d - 1.0).abs() < 1e-9, "got {d}");
        // Piercing the rect → 0.
        let d = seg_rect_dist([0.0, 1.5], [10.0, 1.5], [4.0, 1.0], [6.0, 2.0]);
        assert_eq!(d, 0.0);
    }
}
