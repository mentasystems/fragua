//! A* search on the routing grid.
//!
//! Nodes are `(GridPoint, last_direction)`. Tracking the direction lets
//! us add a *bend penalty* — moving in the same direction is cheap,
//! turning costs extra, and punching a via flips the layer. Without
//! the bend term A* happily emits stair-step paths because zigzag has
//! the same cost as an L-shape; with it, the router prefers long
//! straight runs and clean orthogonal corners.

use std::cmp::Ordering;
use std::collections::{BinaryHeap, HashMap};

use crate::grid::{Cell, Grid, GridPoint};

/// Direction of the last move. `Start` is the entry node before any
/// move has happened; `Via` lets us model "I just punched through" so
/// the next same-layer move on either axis isn't penalised as a bend.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
enum Dir {
    Start,
    Via,
    Right,
    Left,
    Up,
    Down,
}

#[derive(Copy, Clone, Eq, PartialEq, Hash)]
struct State {
    p: GridPoint,
    dir: Dir,
}

#[derive(Copy, Clone, Eq, PartialEq)]
struct Node {
    f: u32,
    g: u32,
    s: State,
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

/// Cost added every time a same-layer move turns 90°. Tuned to be
/// significantly larger than the per-cell move cost so the router will
/// route around obstacles in straight lines whenever possible, but
/// small enough that an unnecessary detour is still cheaper than a
/// terrible-looking zigzag.
const BEND_COST: u32 = 6;

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
    let mut g_score: HashMap<State, u32> = HashMap::new();
    let mut came_from: HashMap<State, State> = HashMap::new();

    let start_state = State { p: start, dir: Dir::Start };
    g_score.insert(start_state, 0);
    open.push(Node { f: h(start), g: 0, s: start_state });

    while let Some(Node { s, g, .. }) = open.pop() {
        if s.p == target && matches!(grid.get(s.p), Cell::NetPad(n) if n == target_net) {
            // Reconstruct path of grid points.
            let mut path = vec![s.p];
            let mut cur = s;
            while let Some(&prev) = came_from.get(&cur) {
                path.push(prev.p);
                cur = prev;
            }
            path.reverse();
            return Some(AStarResult { path });
        }
        if g > *g_score.get(&s).unwrap_or(&u32::MAX) {
            continue;
        }

        for (next_p, move_dir) in neighbours(s.p) {
            if !grid.in_bounds(next_p) {
                continue;
            }
            let walkable = match grid.get(next_p) {
                Cell::Free => true,
                Cell::NetPad(n) | Cell::Trace(n) => n == target_net,
                Cell::Obstacle => false,
            };
            if !walkable {
                continue;
            }
            let mut step_cost = if move_dir == Dir::Via { via_cost } else { 1 };
            // Bend penalty: same-layer turn that doesn't extend the
            // current run. After a via or from the start node we don't
            // count it (the next move always counts as "new" then).
            if move_dir != Dir::Via
                && s.dir != Dir::Start
                && s.dir != Dir::Via
                && s.dir != move_dir
            {
                step_cost += BEND_COST;
            }
            let next = State { p: next_p, dir: move_dir };
            let tentative = g + step_cost;
            if tentative < *g_score.get(&next).unwrap_or(&u32::MAX) {
                g_score.insert(next, tentative);
                came_from.insert(next, s);
                open.push(Node {
                    f: tentative + h(next_p),
                    g: tentative,
                    s: next,
                });
            }
        }
    }
    None
}

fn neighbours(p: GridPoint) -> [(GridPoint, Dir); 5] {
    [
        (GridPoint { layer: p.layer, col: p.col + 1, row: p.row }, Dir::Right),
        (GridPoint { layer: p.layer, col: p.col - 1, row: p.row }, Dir::Left),
        (GridPoint { layer: p.layer, col: p.col,     row: p.row + 1 }, Dir::Down),
        (GridPoint { layer: p.layer, col: p.col,     row: p.row - 1 }, Dir::Up),
        (GridPoint { layer: 1 - p.layer, col: p.col, row: p.row }, Dir::Via),
    ]
}
