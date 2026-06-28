//! Fanout pre-pass: escape dense fine-pitch pads to the inner layers
//! with a via-in-pad, so the rest of the router can pick the net up on a
//! layer that actually has room.
//!
//! Why this exists: a 0.5 mm-pitch part (USB-C, QFN) leaves a ~0.2 mm gap
//! between adjacent pads. A routed trace plus honest clearance needs far
//! more than that, so parallel surface escape is geometrically
//! impossible. Real boards solve it the same way this pass does — drop a
//! small via *inside* the pad (a "via-in-pad" / POFV) down to an inner
//! layer where the pins can spread out. A 0.30 mm / 0.15 mm via (JLCPCB's
//! minimum) sits centred on a 0.5 mm-pitch pad and still keeps a full
//! 0.20 mm to the neighbouring pad, so the result passes DRC.
//!
//! The pass only fans a pad out when it genuinely cannot escape on its
//! own layer — ordinary 2-pin passives keep routing on the surface and
//! never grow a needless via.

use std::collections::HashSet;

use pcb_core::{Board, Id, Length, Point, Via};

use crate::router::RouteOptions;

/// JLCPCB minimum via — 0.30 mm pad, 0.15 mm drill. Small enough to sit
/// centred in a 0.30 mm-wide, 0.5 mm-pitch pad and still clear the
/// neighbour by the default 0.20 mm.
const FANOUT_VIA_DIAMETER_MM: f64 = 0.30;
const FANOUT_VIA_DRILL_MM: f64 = 0.15;

/// How far (mm) a trace must be able to run away from a pad edge, in some
/// direction, for the pad to count as "able to escape on the surface".
/// About two trace pitches — enough to clear the neighbouring pads.
const ESCAPE_LEN_MM: f64 = 0.9;

/// A pad flanked by at least this many foreign-net pads within
/// `CLUSTER_DIST_MM` is in a fine-pitch cluster (USB-C row, QFN edge).
/// Even though *one* trace can slip out along its long axis, the whole
/// row can't escape in parallel at sub-0.65 mm pitch, so every clustered
/// pad gets a fanout via and routes on an inner layer instead.
const CLUSTER_NEIGHBOURS: usize = 2;
const CLUSTER_DIST_MM: f64 = 0.55;

/// Result of the fanout pass.
#[derive(Debug, Default, Clone)]
pub struct FanoutPlan {
    /// Vias to add to the board (one per fanned-out pad).
    pub vias: Vec<Via>,
    /// `"ref.num"` of every pad that was fanned out (matches
    /// `NetPadInfo::pad_ref`). The grid stamps these as through-hole so
    /// the search can land on them from any layer (the via already ties
    /// the surface pad to the inner copper).
    pub through_pads: HashSet<String>,
}

/// Axis-aligned pad rectangle in world mm.
struct PadRect {
    cx: f64,
    cy: f64,
    hw: f64,
    hh: f64,
    net: Option<String>,
}

fn pad_rects(board: &Board) -> Vec<PadRect> {
    let mut out = Vec::new();
    for fp in board.footprints_in_order() {
        for pad in &fp.pads {
            let c = fp.pad_world_center(pad);
            let (w, h) = fp.pad_world_size(pad);
            out.push(PadRect {
                cx: c.x.to_mm(),
                cy: c.y.to_mm(),
                hw: w.to_mm() / 2.0,
                hh: h.to_mm() / 2.0,
                net: pad.net.clone(),
            });
        }
    }
    out
}

/// Distance (mm) from point to an axis-aligned rectangle (0 inside).
fn point_rect_dist(px: f64, py: f64, r: &PadRect) -> f64 {
    let dx = (px - r.cx).abs() - r.hw;
    let dy = (py - r.cy).abs() - r.hh;
    let dx = dx.max(0.0);
    let dy = dy.max(0.0);
    (dx * dx + dy * dy).sqrt()
}

/// Can a trace of width `tw` leave this pad on its own layer in *some*
/// direction without coming within `clearance` of a foreign-net pad for
/// `ESCAPE_LEN_MM`? We probe the four cardinal and four diagonal
/// directions from the pad centre.
fn can_escape_surface(
    cx: f64,
    cy: f64,
    hw: f64,
    hh: f64,
    net: &str,
    foreign: &[&PadRect],
    tw: f64,
    clearance: f64,
) -> bool {
    let need = tw / 2.0 + clearance;
    let dirs = [
        (1.0, 0.0),
        (-1.0, 0.0),
        (0.0, 1.0),
        (0.0, -1.0),
        (0.707, 0.707),
        (0.707, -0.707),
        (-0.707, 0.707),
        (-0.707, -0.707),
    ];
    let start = hw.max(hh); // step out past the pad body
    for (dx, dy) in dirs {
        let mut blocked = false;
        // Sample along the escape ray.
        let mut d = start;
        let end = start + ESCAPE_LEN_MM;
        while d <= end {
            let px = cx + dx * d;
            let py = cy + dy * d;
            for r in foreign {
                if r.net.as_deref() == Some(net) {
                    continue;
                }
                if point_rect_dist(px, py, r) < need {
                    blocked = true;
                    break;
                }
            }
            if blocked {
                break;
            }
            d += 0.1;
        }
        if !blocked {
            return true;
        }
    }
    false
}

/// Would a fanout via at `(cx,cy)` on `net` keep `clearance` to every
/// foreign-net pad and to every existing via? (Same-net pad is the pad
/// we sit in — that's the point.)
fn fanout_via_fits(
    cx: f64,
    cy: f64,
    net: &str,
    via_r: f64,
    clearance: f64,
    foreign: &[&PadRect],
    board: &Board,
) -> bool {
    let need = via_r + clearance;
    for r in foreign {
        if r.net.as_deref() == Some(net) {
            continue;
        }
        if point_rect_dist(cx, cy, r) < need - 1e-9 {
            return false;
        }
    }
    for v in &board.vias {
        let dx = cx - v.position.x.to_mm();
        let dy = cy - v.position.y.to_mm();
        let other_r = v.diameter.to_mm() / 2.0;
        if (dx * dx + dy * dy).sqrt() < via_r + other_r + clearance - 1e-9 {
            return false;
        }
    }
    // Keep the via inside the board, clear of the edge.
    if let Some(o) = board.outline {
        let edge = (cx - o.min.x.to_mm())
            .min(o.max.x.to_mm() - cx)
            .min(cy - o.min.y.to_mm())
            .min(o.max.y.to_mm() - cy);
        if edge < via_r + clearance {
            return false;
        }
    }
    true
}

/// Plan the fanout: for every pad that can't escape on the surface, drop
/// a via-in-pad if it fits. A 2-layer board has nowhere to fan out to, so
/// the pass is a no-op there.
pub fn plan_fanout(board: &Board, opts: &RouteOptions) -> FanoutPlan {
    let mut plan = FanoutPlan::default();
    if board.stackup.layer_count() < 3 {
        return plan;
    }
    let rects = pad_rects(board);
    let foreign: Vec<&PadRect> = rects.iter().collect();
    let tw = opts.trace_width.to_mm();
    let clearance = opts.clearance.to_mm();
    let via_r = FANOUT_VIA_DIAMETER_MM / 2.0;

    // Mutable copy of board vias so successively-placed fanout vias also
    // respect each other's spacing.
    let mut work = board.clone();

    for fp in board.footprints_in_order() {
        for pad in &fp.pads {
            let Some(net) = pad.net.as_deref() else {
                continue;
            };
            // Through-hole pads already reach every layer — no fanout.
            if pad.drill.is_some() {
                continue;
            }
            let c = fp.pad_world_center(pad);
            let (w, h) = fp.pad_world_size(pad);
            let (cx, cy) = (c.x.to_mm(), c.y.to_mm());
            let (hw, hh) = (w.to_mm() / 2.0, h.to_mm() / 2.0);
            // Count foreign-net pads crowding this one.
            let neighbours = foreign
                .iter()
                .filter(|r| r.net.as_deref() != Some(net))
                .filter(|r| point_rect_dist(cx, cy, r) < CLUSTER_DIST_MM)
                .count();
            let in_cluster = neighbours >= CLUSTER_NEIGHBOURS;
            // Fan out if the pad is in a fine-pitch cluster (parallel
            // escape impossible) OR simply can't escape in any direction.
            if !in_cluster && can_escape_surface(cx, cy, hw, hh, net, &foreign, tw, clearance) {
                continue;
            }
            if !fanout_via_fits(cx, cy, net, via_r, clearance, &foreign, &work) {
                // Too tight even for the minimum via — leave it for the
                // router to attempt on the surface (it will likely fail,
                // and the report will flag it).
                continue;
            }
            let via = Via {
                id: Id::new(),
                position: Point::new(c.x, c.y),
                drill: Length::from_mm(FANOUT_VIA_DRILL_MM),
                diameter: Length::from_mm(FANOUT_VIA_DIAMETER_MM),
                net: net.to_string(),
            };
            work.vias.push(via.clone());
            plan.vias.push(via);
            plan.through_pads
                .insert(format!("{}.{}", fp.reference, pad.number));
        }
    }
    plan
}
