//! Theta* (lazy variant) any-angle search on the routing grid.
//!
//! Each open node is a `GridPoint`; cost is fixed-point Euclidean (cell
//! distance × 1000). 8-connected on a layer plus a via flip; on
//! relaxation, if the parent of the current node has line-of-sight to
//! the neighbour on the same layer, we shortcut and set parent
//! directly — that is what produces any-angle paths instead of
//! grid-aligned staircases.

use std::cmp::Ordering;
use std::collections::{BinaryHeap, HashMap};

use crate::grid::{Cell, CostMap, Grid, GridPoint};

#[derive(Copy, Clone, Eq, PartialEq)]
struct Node {
    f: u32,
    g: u32,
    p: GridPoint,
}

impl Ord for Node {
    fn cmp(&self, other: &Self) -> Ordering {
        other.f.cmp(&self.f).then_with(|| other.g.cmp(&self.g))
    }
}
impl PartialOrd for Node {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

#[derive(Debug, Clone)]
pub struct AStarResult {
    pub path: Vec<GridPoint>,
}

/// One unit cell = `SCALE` cost units. Lets us keep `u32` arithmetic
/// while representing Euclidean distance to ~1e-3 cell precision.
const SCALE: u32 = 1000;
/// `sqrt(2) * SCALE`, rounded — cost of a diagonal step.
const DIAG: u32 = 1414;
/// Surcharge for placing a via on top of a same-net pad — fab houses
/// charge extra for via-in-pad fill, so we'd rather offset.
const VIA_IN_PAD_PENALTY: u32 = 40 * SCALE;

// `via_safe_radius`: in cells, ceil((via_diameter/2 + clearance) / cell).
// A via flip is rejected if any foreign-net cell sits within this radius
// on either layer. Pass 0 to disable the check.
//
// `halo`: same value `route_pass` uses when stamping traces. Forwarded
// into `line_of_sight` so the Theta* shortcut rejects any-angle segments
// whose swept body would clip a foreign-net cell, not just whose centre
// line does.
//
// `cost_map` adds per-cell extra bias for negotiated congestion. Raw
// `cost_map` values are promoted into the scaled cost domain on use.
// For an any-angle straight segment Theta* charges the sum along the
// Bresenham line via `grid.cost_along`; for step-wise relaxation it
// charges the destination cell only.
//
// Multi-source for Prim-style Steiner construction. Two kinds of
// source, prioritised by g-penalty:
//
//   - Every `Trace(target_net)` cell at g=0. Those are the existing
//     tree; later spokes branch off the closest cell to the new target.
//   - The explicit `start` (seed pad) at g equal to the smallest h() over
//     all trace cells, floored by `SEED_FALLBACK_PENALTY`. This keeps the
//     best trace cell at least tied with the seed on f-score under
//     Theta*'s Euclidean metric, so Steiner branching survives the move
//     from Manhattan A* (the old fixed 12-cell penalty was overwhelmed by
//     direct diagonals from the seed pad).
//
// First spoke (no traces yet) sees the seed at g=0 — the penalty only
// kicks in once a tree exists.
pub fn search(
    grid: &Grid,
    start: GridPoint,
    target_net: u32,
    via_cost: u32,
    target: GridPoint,
    via_safe_radius: i32,
    halo: i32,
    cost_map: &CostMap,
) -> Option<AStarResult> {
    /// Floor for the seed pad's g-penalty when at least one trace cell
    /// exists. ~6 mm at default 0.25 mm pitch — kicks in only when every
    /// trace cell is closer to the target than this; in practice the
    /// max() with `best_trace_h` dominates whenever a trace cell sits
    /// between the seed and the target.
    const SEED_FALLBACK_PENALTY: u32 = 24 * SCALE;

    let via_scaled = via_cost.saturating_mul(SCALE);

    // A drilled (through-hole) target accepts either layer for free:
    // the existing PTH already connects top and bottom, so reaching it
    // from the opposite side doesn't actually need a router via. Skip
    // the layer-mismatch via_cost in the heuristic when the target is
    // a DrilledPad on either layer.
    let target_is_th = matches!(grid.get(target), Cell::DrilledPad(_))
        || matches!(
            grid.get(GridPoint { layer: 1 - target.layer, col: target.col, row: target.row }),
            Cell::DrilledPad(_)
        );
    let h = |p: GridPoint| -> u32 {
        let dc = f64::from(p.col - target.col);
        let dr = f64::from(p.row - target.row);
        let dist = (dc * dc + dr * dr).sqrt();
        let dl = if p.layer == target.layer || target_is_th {
            0
        } else {
            via_scaled
        };
        (dist * f64::from(SCALE)).round() as u32 + dl
    };

    let euclid = |a: GridPoint, b: GridPoint| -> u32 {
        let dc = f64::from(a.col - b.col);
        let dr = f64::from(a.row - b.row);
        ((dc * dc + dr * dr).sqrt() * f64::from(SCALE)).round() as u32
    };

    let mut open = BinaryHeap::new();
    let mut g_score: HashMap<GridPoint, u32> = HashMap::new();
    let mut came_from: HashMap<GridPoint, GridPoint> = HashMap::new();

    let mut had_trace_source = false;
    let mut best_trace_h: u32 = u32::MAX;
    for layer in 0..2u8 {
        for row in 0..grid.rows {
            for col in 0..grid.cols {
                let p = GridPoint { layer, col, row };
                if matches!(grid.get(p), Cell::Trace(n) if n == target_net) {
                    let hp = h(p);
                    g_score.insert(p, 0);
                    open.push(Node { f: hp, g: 0, p });
                    had_trace_source = true;
                    if hp < best_trace_h {
                        best_trace_h = hp;
                    }
                }
            }
        }
    }
    let seed_g = if had_trace_source {
        best_trace_h.max(SEED_FALLBACK_PENALTY)
    } else {
        0
    };
    g_score.insert(start, seed_g);
    open.push(Node {
        f: seed_g + h(start),
        g: seed_g,
        p: start,
    });

    while let Some(Node { p, g, .. }) = open.pop() {
        // Termination: same column/row as target, AND either same
        // layer OR the cell is a DrilledPad of the target net (the
        // TH connects both layers, so entering from either side
        // counts as landing).
        let at_target = p.col == target.col
            && p.row == target.row
            && (p.layer == target.layer
                || (target_is_th
                    && matches!(
                        grid.get(p),
                        Cell::DrilledPad(n) if n == target_net
                    )));
        if at_target
            && matches!(grid.get(p), Cell::NetPad(n) | Cell::DrilledPad(n) if n == target_net)
        {
            let mut path = vec![p];
            let mut cur = p;
            while let Some(&prev) = came_from.get(&cur) {
                path.push(prev);
                cur = prev;
            }
            path.reverse();
            return Some(AStarResult { path });
        }
        if g > *g_score.get(&p).unwrap_or(&u32::MAX) {
            continue;
        }

        for (next_p, kind) in neighbours(p) {
            if !grid.in_bounds(next_p) {
                continue;
            }
            let walkable = match grid.get(next_p) {
                Cell::Free => true,
                Cell::NetPad(n) | Cell::DrilledPad(n) | Cell::Trace(n) => n == target_net,
                Cell::Obstacle => false,
            };
            if !walkable {
                continue;
            }
            let is_via = matches!(kind, Move::Via);
            // Hard reject: a via landing inside a through-hole pad
            // collides with the existing PTH drill. No score penalty
            // is high enough to make that legal, so refuse outright.
            if is_via
                && (matches!(
                    grid.get(GridPoint { layer: 0, col: next_p.col, row: next_p.row }),
                    Cell::DrilledPad(_)
                ) || matches!(
                    grid.get(GridPoint { layer: 1, col: next_p.col, row: next_p.row }),
                    Cell::DrilledPad(_)
                ))
            {
                continue;
            }
            if is_via
                && via_safe_radius > 0
                && !via_safe(grid, next_p, target_net, via_safe_radius)
            {
                continue;
            }

            // Step-wise A* relaxation as the default proposal.
            let base_step = match kind {
                Move::Ortho => SCALE,
                Move::Diag => DIAG,
                Move::Via => via_scaled,
            };
            let mut best_parent = p;
            let mut best_g = g
                .saturating_add(base_step)
                .saturating_add(cost_map.at(next_p).saturating_mul(SCALE));

            // Theta* lazy shortcut: if our parent has line-of-sight to
            // the neighbour on the same layer, route p_parent → next
            // directly. Vias never shortcut (they cross layers).
            if !is_via {
                if let Some(&parent) = came_from.get(&p) {
                    if parent.layer == next_p.layer
                        && grid.line_of_sight(parent, next_p, target_net, halo)
                    {
                        let g_parent = *g_score.get(&parent).unwrap_or(&u32::MAX);
                        if g_parent != u32::MAX {
                            let shortcut = g_parent
                                .saturating_add(euclid(parent, next_p))
                                .saturating_add(
                                    grid.cost_along(cost_map, parent, next_p)
                                        .saturating_mul(SCALE),
                                );
                            if shortcut < best_g {
                                best_g = shortcut;
                                best_parent = parent;
                            }
                        }
                    }
                }
            }

            if is_via {
                // Soft penalty for landing on an SMD pad (legal but
                // requires via-in-pad fill at the fab — extra cost).
                // DrilledPad cases are already hard-rejected above.
                let on_pad = matches!(
                    grid.get(GridPoint { layer: 0, col: next_p.col, row: next_p.row }),
                    Cell::NetPad(_)
                ) || matches!(
                    grid.get(GridPoint { layer: 1, col: next_p.col, row: next_p.row }),
                    Cell::NetPad(_)
                );
                if on_pad {
                    best_g = best_g.saturating_add(VIA_IN_PAD_PENALTY);
                }
            }

            if best_g < *g_score.get(&next_p).unwrap_or(&u32::MAX) {
                g_score.insert(next_p, best_g);
                came_from.insert(next_p, best_parent);
                open.push(Node {
                    f: best_g.saturating_add(h(next_p)),
                    g: best_g,
                    p: next_p,
                });
            }
        }
    }
    None
}

/// True if a via at `p` would have foreign-net copper within `radius`
/// cells on either layer.
fn via_safe(grid: &Grid, p: GridPoint, target_net: u32, radius: i32) -> bool {
    let r2 = radius * radius;
    for layer in 0..2u8 {
        for dr in -radius..=radius {
            for dc in -radius..=radius {
                if dr * dr + dc * dc > r2 {
                    continue;
                }
                let np = GridPoint { layer, col: p.col + dc, row: p.row + dr };
                match grid.get(np) {
                    Cell::Obstacle => return false,
                    Cell::NetPad(n) | Cell::DrilledPad(n) | Cell::Trace(n)
                        if n != target_net =>
                    {
                        return false;
                    }
                    _ => {}
                }
            }
        }
    }
    true
}

#[derive(Copy, Clone)]
enum Move {
    Ortho,
    Diag,
    Via,
}

fn neighbours(p: GridPoint) -> [(GridPoint, Move); 9] {
    let l = p.layer;
    let c = p.col;
    let r = p.row;
    [
        (GridPoint { layer: l, col: c + 1, row: r }, Move::Ortho),
        (GridPoint { layer: l, col: c - 1, row: r }, Move::Ortho),
        (GridPoint { layer: l, col: c, row: r + 1 }, Move::Ortho),
        (GridPoint { layer: l, col: c, row: r - 1 }, Move::Ortho),
        (GridPoint { layer: l, col: c + 1, row: r + 1 }, Move::Diag),
        (GridPoint { layer: l, col: c + 1, row: r - 1 }, Move::Diag),
        (GridPoint { layer: l, col: c - 1, row: r + 1 }, Move::Diag),
        (GridPoint { layer: l, col: c - 1, row: r - 1 }, Move::Diag),
        (GridPoint { layer: 1 - l, col: c, row: r }, Move::Via),
    ]
}
