//! A* search on the routing grid.
//!
//! Nodes are `GridPoint`s. Moves are 4-connected on the same layer
//! (cost = 1 per cell) plus a "punch via" move that flips the layer
//! at the same (col,row) for `via_cost`. Goal test: cell value is
//! `NetPad(net)` and we are on either layer (the search reaches *any*
//! valid pad cell of the target net).

use std::cmp::Ordering;
use std::collections::{BinaryHeap, HashMap};

use crate::grid::{Cell, Grid, GridPoint};

#[derive(Copy, Clone, Eq, PartialEq)]
struct Node {
    f: u32,
    g: u32,
    p: GridPoint,
}

impl Ord for Node {
    fn cmp(&self, other: &Self) -> Ordering {
        // Min-heap by f; tie-break on g so deeper nodes are preferred
        // (helps when many cells share the same f value).
        other
            .f
            .cmp(&self.f)
            .then_with(|| other.g.cmp(&self.g))
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

/// Run A* from `start` to any cell whose value is `NetPad(target_net)`
/// and is reachable. Returns the cells along the path, including both
/// endpoints. Returns `None` if no path exists.
pub fn search(
    grid: &Grid,
    start: GridPoint,
    target_net: u32,
    via_cost: u32,
    target: GridPoint,
) -> Option<AStarResult> {
    let h = |p: GridPoint| -> u32 {
        let dc = (p.col - target.col).unsigned_abs();
        let dr = (p.row - target.row).unsigned_abs();
        let dl = if p.layer == target.layer { 0 } else { via_cost };
        dc + dr + dl
    };

    let mut open = BinaryHeap::new();
    let mut g_score: HashMap<GridPoint, u32> = HashMap::new();
    let mut came_from: HashMap<GridPoint, GridPoint> = HashMap::new();
    g_score.insert(start, 0);
    open.push(Node { f: h(start), g: 0, p: start });

    while let Some(Node { p, g, .. }) = open.pop() {
        // Goal: same cell as target, on the right layer, and the cell
        // belongs to the target net.
        if p == target && matches!(grid.get(p), Cell::NetPad(n) if n == target_net) {
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

        for next in neighbours(p) {
            let step_cost = if next.layer != p.layer { via_cost } else { 1 };
            if !grid.in_bounds(next) {
                continue;
            }
            // Treat the destination cell:
            // - Free: walk through.
            // - NetPad/Trace of *target* net: walk through.
            // - Anything else: blocked.
            let walkable = match grid.get(next) {
                Cell::Free => true,
                Cell::NetPad(n) | Cell::Trace(n) => n == target_net,
                Cell::Obstacle => false,
            };
            if !walkable {
                continue;
            }
            let tentative = g + step_cost;
            if tentative < *g_score.get(&next).unwrap_or(&u32::MAX) {
                g_score.insert(next, tentative);
                came_from.insert(next, p);
                open.push(Node {
                    f: tentative + h(next),
                    g: tentative,
                    p: next,
                });
            }
        }
    }
    None
}

fn neighbours(p: GridPoint) -> [GridPoint; 5] {
    [
        GridPoint { layer: p.layer, col: p.col + 1, row: p.row },
        GridPoint { layer: p.layer, col: p.col - 1, row: p.row },
        GridPoint { layer: p.layer, col: p.col,     row: p.row + 1 },
        GridPoint { layer: p.layer, col: p.col,     row: p.row - 1 },
        // Layer flip at the same (col,row) — a via.
        GridPoint { layer: 1 - p.layer, col: p.col, row: p.row },
    ]
}
