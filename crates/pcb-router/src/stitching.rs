//! Auto-stitching: sprinkle vias on a grid inside a `Pour` when its
//! `stitching` policy is `Grid` and a same-net pour exists on the
//! opposite copper layer. Vias keep the two planes electrically tied
//! together; a 5 mm pitch is the industry rule-of-thumb for GND
//! stitching on hand-assembled boards.

use pcb_core::{Board, CopperLayer, Length, Point, StitchPolicy, Via};

use crate::router::RouteOptions;

/// Drop auto-generated stitching vias for every pour whose
/// `StitchPolicy` is `Grid`. Returns the number of vias added.
pub fn add_stitching_vias(board: &mut Board, opts: &RouteOptions) -> usize {
    let mut added = 0usize;
    // Snapshot pours upfront — we mutate `board.vias` while iterating.
    let pours: Vec<_> = board.pours.clone();
    for pour in &pours {
        let StitchPolicy::Grid {
            pitch_mm,
            clearance_mm,
        } = pour.stitching
        else {
            continue;
        };
        if pitch_mm <= 0.0 {
            continue;
        }
        // Require a same-net pour on the OPPOSITE outer layer. With
        // multi-layer stackups we'd ideally consult board.stackup;
        // for now we keep the legacy top↔bottom flip since vias still
        // punch top↔bottom only.
        let opposite: CopperLayer = if pour.layer.is_top() {
            CopperLayer::Bottom
        } else {
            CopperLayer::Top
        };
        if !board
            .pours
            .iter()
            .any(|p| p.net == pour.net && p.layer == opposite)
        {
            continue;
        }
        // Polygon = board outline (the pour fills the outline). When
        // no outline is set, skip — we have nothing to enclose.
        let Some(outline) = board.outline else {
            continue;
        };
        let x0 = outline.min.x.to_mm();
        let y0 = outline.min.y.to_mm();
        let x1 = outline.max.x.to_mm();
        let y1 = outline.max.y.to_mm();
        // Inset by clearance so vias don't fall right at the edge.
        let inset = clearance_mm.max(opts.via_diameter.to_mm() / 2.0);
        let mut y = y0 + inset;
        while y <= y1 - inset + 1e-9 {
            let mut x = x0 + inset;
            while x <= x1 - inset + 1e-9 {
                if !cell_is_clear(board, x, y, opts, clearance_mm) {
                    x += pitch_mm;
                    continue;
                }
                let via = Via {
                    id: pcb_core::Id::new(),
                    position: Point::new(Length::from_mm(x), Length::from_mm(y)),
                    drill: opts.via_drill,
                    diameter: opts.via_diameter,
                    net: pour.net.clone(),
                };
                board.vias.push(via);
                added += 1;
                x += pitch_mm;
            }
            y += pitch_mm;
        }
    }
    added
}

/// Does an (x_mm, y_mm) cell clear every conflict? Checks traces,
/// pads, existing vias, and keepout polygons within `clearance_mm`.
/// Foreign-net traces/vias respect the clearance too — same-net stays
/// out of the way because the stitching grid is per-net.
fn cell_is_clear(board: &Board, x: f64, y: f64, opts: &RouteOptions, clearance_mm: f64) -> bool {
    let via_r = opts.via_diameter.to_mm() / 2.0;
    let needed = via_r + clearance_mm;
    // Existing vias of ANY net (including just-added stitching).
    for v in &board.vias {
        let dx = x - v.position.x.to_mm();
        let dy = y - v.position.y.to_mm();
        let other_r = v.diameter.to_mm() / 2.0;
        if (dx * dx + dy * dy).sqrt() < needed + other_r {
            return false;
        }
    }
    // Pads (any net).
    for fp in board.footprints_in_order() {
        for pad in &fp.pads {
            let c = fp.pad_world_center(pad);
            let (pw, ph) = fp.pad_world_size(pad);
            let dx = (x - c.x.to_mm()).abs() - pw.to_mm() / 2.0;
            let dy = (y - c.y.to_mm()).abs() - ph.to_mm() / 2.0;
            // Treat as a fattened rectangle.
            if dx < needed && dy < needed {
                return false;
            }
        }
    }
    // Traces: point-to-segment distance.
    for t in &board.traces {
        let d = point_to_segment_mm(
            x,
            y,
            t.start.x.to_mm(),
            t.start.y.to_mm(),
            t.end.x.to_mm(),
            t.end.y.to_mm(),
        );
        if d < needed + t.width.to_mm() / 2.0 {
            return false;
        }
    }
    // Keepouts (any layer).
    for kp in &board.keepouts {
        if kp.polygon.len() < 3 {
            continue;
        }
        if point_in_polygon(&kp.polygon, x, y) {
            return false;
        }
    }
    true
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

fn point_in_polygon(poly: &[Point], x: f64, y: f64) -> bool {
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
                let x_intersect = pix + (y - piy) * (pjx - pix) / denom;
                if x < x_intersect {
                    inside = !inside;
                }
            }
        }
        j = i;
    }
    inside
}
