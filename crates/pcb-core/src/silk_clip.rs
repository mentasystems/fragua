//! Silk-vs-pad segment clipping.
//!
//! Fab houses mask silkscreen ink that lands on solder pads — leaving
//! it through would smudge the legend onto the copper and ruin the
//! solder joint. The renderer and the Gerber writer both need to drop
//! silk strokes that cross a pad, but they should KEEP the part of
//! the stroke that stays clear.
//!
//! V2 implementation: proper segment-vs-rect clipping using the
//! Liang-Barsky parametric form. For each segment we either
//!  * emit it whole (no pad on the path),
//!  * skip it entirely (segment lies inside a pad), or
//!  * emit one or two trimmed pieces that bracket the pad.
//!
//! Pad rectangles are taken in WORLD space and assumed axis-aligned
//! — `Footprint::pad_world_size` already swaps width↔height for the
//! 90°/270° rotations we support, so the world bbox of a pad is
//! axis-aligned even when the host footprint is rotated.
//!
//! The output of `clip_segment` is a `Vec<(Point, Point)>` of zero,
//! one, or two pieces. Callers iterate the result and emit each piece
//! the same way they'd emit the original segment.

use crate::geometry::{Point, Rect};
use crate::units::Length;

/// Clip a segment against a list of axis-aligned pad rectangles,
/// returning the pieces that lie OUTSIDE every rect. If the segment
/// never enters any rect the result is `[(start, end)]`. If it lies
/// entirely inside some rect the result is empty.
///
/// The implementation is a simple loop: for each rect we replace the
/// running list of pieces with the bits of each piece that survive
/// after subtracting the rect. Up to two surviving pieces per rect
/// (entry stub + exit stub) — the typical case is at most one rect
/// in play because pads on a footprint don't overlap each other.
#[must_use]
pub fn clip_segment(start: Point, end: Point, rects: &[Rect]) -> Vec<(Point, Point)> {
    let mut pieces: Vec<(Point, Point)> = vec![(start, end)];
    if rects.is_empty() {
        return pieces;
    }
    for rect in rects {
        let mut next: Vec<(Point, Point)> = Vec::with_capacity(pieces.len() * 2);
        for (a, b) in pieces.drain(..) {
            subtract_rect(a, b, *rect, &mut next);
        }
        pieces = next;
        if pieces.is_empty() {
            return pieces;
        }
    }
    pieces
}

/// Append the parts of segment `a..b` that lie outside `rect` to
/// `out`. Up to two pieces (entry stub before the rect, exit stub
/// after the rect).
fn subtract_rect(a: Point, b: Point, rect: Rect, out: &mut Vec<(Point, Point)>) {
    let ax = a.x.to_mm();
    let ay = a.y.to_mm();
    let bx = b.x.to_mm();
    let by = b.y.to_mm();
    let rmin_x = rect.min.x.to_mm();
    let rmin_y = rect.min.y.to_mm();
    let rmax_x = rect.max.x.to_mm();
    let rmax_y = rect.max.y.to_mm();

    // Liang-Barsky against [rmin..rmax] gives the interval [t0, t1]
    // of `a..b` parameter that lies INSIDE the rect (clamped to
    // [0, 1]). Outside-the-rect pieces are [0, t0] and [t1, 1].
    let dx = bx - ax;
    let dy = by - ay;
    let mut t0: f64 = 0.0;
    let mut t1: f64 = 1.0;
    // Each edge of the rect contributes a (p, q) clip pair.
    let edges: [(f64, f64); 4] = [
        (-dx, ax - rmin_x), // left
        (dx, rmax_x - ax),  // right
        (-dy, ay - rmin_y), // bottom
        (dy, rmax_y - ay),  // top
    ];
    for (p, q) in edges {
        if p.abs() < 1e-12 {
            // Segment is parallel to this edge; if outside, the rect
            // never bites this segment at all.
            if q < 0.0 {
                out.push((a, b));
                return;
            }
            continue;
        }
        let r = q / p;
        if p < 0.0 {
            if r > t1 {
                // Inside-interval is empty → segment never enters.
                out.push((a, b));
                return;
            }
            if r > t0 {
                t0 = r;
            }
        } else {
            if r < t0 {
                out.push((a, b));
                return;
            }
            if r < t1 {
                t1 = r;
            }
        }
    }
    // Now [t0, t1] is the inside-rect interval, clamped to [0, 1].
    if t0 >= t1 {
        // Degenerate intersection — treat as no-bite.
        out.push((a, b));
        return;
    }
    // Outside pieces: [0, t0] and [t1, 1] in parameter space.
    let entry_len = t0;
    let exit_len = 1.0 - t1;
    let lerp = |t: f64| -> Point {
        Point::new(
            Length::from_mm(ax + dx * t),
            Length::from_mm(ay + dy * t),
        )
    };
    if entry_len > 1e-9 {
        out.push((a, lerp(t0)));
    }
    if exit_len > 1e-9 {
        out.push((lerp(t1), b));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn p(x: f64, y: f64) -> Point {
        Point::new(Length::from_mm(x), Length::from_mm(y))
    }

    fn r(x0: f64, y0: f64, x1: f64, y1: f64) -> Rect {
        Rect::from_corners(p(x0, y0), p(x1, y1))
    }

    #[test]
    fn no_rects_passes_through() {
        let pieces = clip_segment(p(0.0, 0.0), p(10.0, 0.0), &[]);
        assert_eq!(pieces.len(), 1);
    }

    #[test]
    fn segment_outside_unchanged() {
        let pieces = clip_segment(p(0.0, 0.0), p(10.0, 0.0), &[r(20.0, -5.0, 30.0, 5.0)]);
        assert_eq!(pieces.len(), 1);
    }

    #[test]
    fn segment_through_pad_splits_in_two() {
        // Horizontal segment crossing a pad centred at x=5.
        let pieces = clip_segment(p(0.0, 0.0), p(10.0, 0.0), &[r(4.0, -1.0, 6.0, 1.0)]);
        assert_eq!(pieces.len(), 2);
        // Entry stub ends at x=4, exit stub starts at x=6.
        let (a0, b0) = pieces[0];
        let (a1, b1) = pieces[1];
        assert!((a0.x.to_mm() - 0.0).abs() < 1e-3);
        assert!((b0.x.to_mm() - 4.0).abs() < 1e-3);
        assert!((a1.x.to_mm() - 6.0).abs() < 1e-3);
        assert!((b1.x.to_mm() - 10.0).abs() < 1e-3);
    }

    #[test]
    fn segment_inside_dropped() {
        let pieces = clip_segment(p(4.5, 0.0), p(5.5, 0.0), &[r(4.0, -1.0, 6.0, 1.0)]);
        assert!(pieces.is_empty());
    }

    #[test]
    fn segment_starts_inside_emits_only_exit() {
        let pieces = clip_segment(p(5.0, 0.0), p(10.0, 0.0), &[r(4.0, -1.0, 6.0, 1.0)]);
        assert_eq!(pieces.len(), 1);
        let (a, b) = pieces[0];
        assert!((a.x.to_mm() - 6.0).abs() < 1e-3);
        assert!((b.x.to_mm() - 10.0).abs() < 1e-3);
    }
}
