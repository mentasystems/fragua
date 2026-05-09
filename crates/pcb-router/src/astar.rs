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

use crate::grid::{Cell, CostMap, Grid, GridPoint};

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

// `via_safe_radius`: in cells, ceil((via_diameter/2 + clearance) / cell).
// A via flip is rejected if any foreign-net cell sits within this radius
// on either layer. Pass 0 to disable the check.
//
// `cost_map` adds per-cell extra cost for negotiated congestion: A*
// charges `cost_map.at(next)` on top of the base step cost when crossing
// `next`. Bias is added before the bend penalty so a costly cell makes
// the router prefer cheaper alternatives even when no bend is involved.
//
// Multi-source for Prim-style Steiner construction. Two kinds of
// source, prioritised by g-penalty:
//
//   - Every `Trace(target_net)` cell at g=0. Those are the existing
//     tree; later spokes branch off the closest cell to the new target.
//   - The explicit `start` (seed pad) at g=`SEED_FALLBACK_PENALTY`.
//     Falls back when no trace cell is within ~3 mm of being as close
//     to the target as the seed is, e.g. when the existing trunk took
//     a detour that doesn't help reach the next pad. Without this
//     penalty the seed wins ties on h alone and lays copper parallel
//     to the trunk; with it disabled entirely (seed not in queue at
//     all), a bad trunk traps the search and the spoke fails outright.
//
// First spoke (no traces yet) sees the seed at g=0 — the penalty only
// kicks in once a tree exists.
//
// Other-net `NetPad` cells are NOT sources; the path must reach `target`
// by routing, not by snapping through neighbouring pads of the same net.
pub fn search(
    grid: &Grid,
    start: GridPoint,
    target_net: u32,
    via_cost: u32,
    target: GridPoint,
    via_safe_radius: i32,
    cost_map: &CostMap,
) -> Option<AStarResult> {
    /// Cells, at the default 0.25 mm pitch ≈ 3 mm of free-cell walking.
    /// A* picks the seed over a trace cell only if the seed is more
    /// than this many cells closer to the target — strong enough to
    /// suppress the parallel-trunk artifact but small enough that a
    /// trace-cell path that's much further away still loses to the seed.
    const SEED_FALLBACK_PENALTY: u32 = 12;

    let h = |p: GridPoint| -> u32 {
        let dc = (p.col - target.col).unsigned_abs();
        let dr = (p.row - target.row).unsigned_abs();
        let dl = if p.layer == target.layer { 0 } else { via_cost };
        dc + dr + dl
    };

    let mut open = BinaryHeap::new();
    let mut g_score: HashMap<State, u32> = HashMap::new();
    let mut came_from: HashMap<State, State> = HashMap::new();

    let mut had_trace_source = false;
    for layer in 0..2u8 {
        for row in 0..grid.rows {
            for col in 0..grid.cols {
                let p = GridPoint { layer, col, row };
                if matches!(grid.get(p), Cell::Trace(n) if n == target_net) {
                    let state = State { p, dir: Dir::Start };
                    g_score.insert(state, 0);
                    open.push(Node { f: h(p), g: 0, s: state });
                    had_trace_source = true;
                }
            }
        }
    }
    let seed_g = if had_trace_source { SEED_FALLBACK_PENALTY } else { 0 };
    let start_state = State { p: start, dir: Dir::Start };
    g_score.insert(start_state, seed_g);
    open.push(Node { f: seed_g + h(start), g: seed_g, s: start_state });

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
            // Vias have a finite copper diameter and need clearance to
            // every other net's copper on *both* layers (since the via
            // punches through). A via at the edge of our own pad's
            // expanded clearance box can otherwise sit too close to the
            // adjacent foreign pad. Reject via flips that would land
            // within `via_safe_radius` of any foreign-net cell.
            if move_dir == Dir::Via
                && via_safe_radius > 0
                && !via_safe(grid, next_p, target_net, via_safe_radius)
            {
                continue;
            }
            let mut step_cost = if move_dir == Dir::Via { via_cost } else { 1 };
            // Negotiated-congestion bias on the destination cell.
            step_cost = step_cost.saturating_add(cost_map.at(next_p));
            // "Via in pad" penalty: discourage but don't forbid via
            // flips that land on a same-net pad cell. Fab houses
            // (JLCPCB) require a more expensive via-in-pad-fill
            // process for those, so we'd rather offset the via by a
            // cell when an alternative exists.
            if move_dir == Dir::Via {
                let on_pad = matches!(
                    grid.get(GridPoint { layer: 0, col: next_p.col, row: next_p.row }),
                    Cell::NetPad(_)
                ) || matches!(
                    grid.get(GridPoint { layer: 1, col: next_p.col, row: next_p.row }),
                    Cell::NetPad(_)
                );
                if on_pad {
                    step_cost = step_cost.saturating_add(40);
                }
            }
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

/// True if a via at `p` (which is on one layer) would have foreign-net
/// copper within `radius` cells on either layer. Foreign-net = anything
/// that is not Free, not Trace/NetPad of `target_net`. The check looks
/// at both layers because a via punches through both.
fn via_safe(grid: &Grid, p: GridPoint, target_net: u32, radius: i32) -> bool {
    let r2 = radius * radius;
    for layer in 0..2u8 {
        for dr in -radius..=radius {
            for dc in -radius..=radius {
                if dr * dr + dc * dc > r2 {
                    continue;
                }
                let np = GridPoint {
                    layer,
                    col: p.col + dc,
                    row: p.row + dr,
                };
                match grid.get(np) {
                    Cell::Obstacle => return false,
                    Cell::NetPad(n) | Cell::Trace(n) if n != target_net => return false,
                    _ => {}
                }
            }
        }
    }
    true
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
