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

use crate::grid::{self, Cell, CostMap, Grid, GridPoint};

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
// `clr_cells`: the searching net's per-trace clearance radius in cells,
// `ceil((clearance + trace_width/2) / cell)`. A precomputed Euclidean
// disk of this radius is scanned at every non-via expansion (and along
// the Theta* LOS shortcut): if any foreign copper sits inside the disk,
// the move is rejected. Because foreign copper is stamped bare (its own
// half-width baked into the stamp) and this radius adds clearance plus
// the searching net's own half-width, centreline-to-centreline ends up at
// `w_a/2 + w_b/2 + clearance` — exact at any grid pitch.
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
#[allow(clippy::too_many_arguments)]
pub fn search(
    grid: &Grid,
    start: GridPoint,
    target_net: u32,
    via_cost: u32,
    target: GridPoint,
    via_safe_radius: i32,
    clr_cells: i32,
    cost_map: &CostMap,
    // Existing trace cells of `target_net` laid so far (the partial
    // tree). Seeded at g=0 so a later spoke branches off the closest
    // one — Prim/Steiner growth. Empty for the first spoke. Passing
    // this in (instead of rescanning the whole grid for `Trace(net)`
    // cells every call) is what makes fine grids tractable.
    sources: &[GridPoint],
    // Greedy heuristic weight `W` for `f = g + W·h`. `1.0` keeps A*
    // admissible/optimal; `>1.0` inflates `h` to collapse the near-tied-f
    // frontier on long searches. Applied only when the INITIAL
    // start→target straight-line distance is `>= WEIGHT_MIN_DIST_CELLS`
    // (see below) so short, tight searches stay optimal regardless of `W`.
    weight: f64,
) -> Option<AStarResult> {
    /// Floor for the seed pad's g-penalty when at least one trace cell
    /// exists. ~6 mm at default 0.25 mm pitch — kicks in only when every
    /// trace cell is closer to the target than this; in practice the
    /// max() with `best_trace_h` dominates whenever a trace cell sits
    /// between the seed and the target.
    const SEED_FALLBACK_PENALTY: u32 = 24 * SCALE;

    let via_scaled = via_cost.saturating_mul(SCALE);

    // Per-trace clearance disk, computed once: the set of cell offsets
    // within `clr_cells` of any candidate centreline cell that must be
    // free of foreign copper. Reused for every expansion and LOS check.
    let disk = grid::disk_offsets(clr_cells);

    // Bound the search to the bounding box of {sources, start, target}
    // inflated by a generous margin. A purely local connection (e.g.
    // fine-pitch fanout) then explores a few hundred cells instead of
    // the whole board, the dominant cost at fine pitch. The margin is
    // wide enough that any reasonable detour around an obstacle stays
    // inside the window; a connection genuinely needing more is rare and
    // simply fails this attempt (RR&R will retry with a clearer board).
    const SEARCH_MARGIN_CELLS: i32 = 120;
    let (mut min_c, mut min_r, mut max_c, mut max_r) = (
        start.col.min(target.col),
        start.row.min(target.row),
        start.col.max(target.col),
        start.row.max(target.row),
    );
    for s in sources {
        min_c = min_c.min(s.col);
        min_r = min_r.min(s.row);
        max_c = max_c.max(s.col);
        max_r = max_r.max(s.row);
    }
    let bound_min_c = min_c - SEARCH_MARGIN_CELLS;
    let bound_min_r = min_r - SEARCH_MARGIN_CELLS;
    let bound_max_c = max_c + SEARCH_MARGIN_CELLS;
    let bound_max_r = max_r + SEARCH_MARGIN_CELLS;
    let in_window = |p: GridPoint| -> bool {
        p.col >= bound_min_c && p.col <= bound_max_c && p.row >= bound_min_r && p.row <= bound_max_r
    };

    // A drilled (through-hole) target accepts any copper layer for
    // free: the existing PTH already connects every copper layer it
    // straddles, so reaching it from any side doesn't need a router
    // via. Scan every layer of the stackup for a DrilledPad cell at
    // the target column/row.
    let target_is_th = (0..grid.layer_count).any(|l| {
        matches!(
            grid.get(GridPoint { layer: l, col: target.col, row: target.row }),
            Cell::DrilledPad(_)
        )
    });
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

    // Greedy weighting, decided ONCE at search entry on a single scalar
    // (the initial start→target straight-line distance). Gating on a
    // per-search constant — not per-node — keeps the weight stable for
    // the whole search (so the open-heap priority stays consistent and
    // the result deterministic) and forces W=1.0 on every SHORT search:
    // the tight-detour tests (≤1.10 / ≤1.30 / ≤1.5×HPWL) and every
    // fanout/diff-pair end-cap are tens of cells, far under the gate, so
    // they remain provably optimal. Only long, board-spanning nets —
    // where the near-open frontier explodes — get the weight, trading a
    // few-percent detour for a 5–30× smaller frontier.
    const WEIGHT_MIN_DIST_CELLS: f64 = 64.0;
    let use_weight = weight > 1.0 && {
        let dc = f64::from(start.col - target.col);
        let dr = f64::from(start.row - target.row);
        (dc * dc + dr * dr).sqrt() >= WEIGHT_MIN_DIST_CELLS
    };
    // Inflate a heuristic estimate by `W`. Applied ONLY at f-score
    // construction (heap priority); g_score, came_from, the Theta* LOS
    // shortcut, via penalties, termination and reconstruction all stay in
    // the exact g-domain, so weighting can only reprioritise expansion —
    // never corrupt the path or the clearance it was found under.
    let wh = |hv: u32| -> u32 {
        if use_weight {
            (f64::from(hv) * weight).round() as u32
        } else {
            hv
        }
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
    for &p in sources {
        // Seed only from actual TRACE cells of this net — exactly the
        // set the old full-grid scan produced. Pad (`NetPad`) cells are
        // deliberately excluded: seeding them at g=0 lets the lazy-Theta*
        // shortcut weave a `came_from` cycle (pad cells sit adjacent to
        // many tied-cost cells), which corrupts path reconstruction.
        // The caller's `sources` list may contain pad cells (path
        // endpoints); we filter them here so the search stays acyclic
        // while still avoiding the whole-grid rescan.
        if matches!(grid.get(p), Cell::Trace(n) if n == target_net) {
            let hp = h(p);
            if !g_score.contains_key(&p) {
                g_score.insert(p, 0);
                open.push(Node { f: wh(hp), g: 0, p });
            }
            had_trace_source = true;
            if hp < best_trace_h {
                best_trace_h = hp;
            }
        }
    }
    let seed_g = if had_trace_source {
        best_trace_h.max(SEED_FALLBACK_PENALTY)
    } else {
        0
    };
    // Seed the start. If the start pad is through-hole (a real PTH pad or
    // a fanned-out SMD pad with a via-in-pad), its barrel shorts every
    // layer at this (col,row), so the search may begin on any of them —
    // crucial for a fanned-out seed, whose whole point is to route on an
    // inner layer rather than escape on the congested surface. For an
    // ordinary SMD pad only its own layer is walkable, so this seeds just
    // `start`, exactly as before.
    for layer in 0..grid.layer_count {
        let p = GridPoint { layer, col: start.col, row: start.row };
        let walkable = p == start
            || matches!(grid.get(p), Cell::NetPad(n) | Cell::DrilledPad(n) | Cell::Trace(n) if n == target_net);
        if walkable && !g_score.contains_key(&p) {
            g_score.insert(p, seed_g);
            open.push(Node { f: seed_g.saturating_add(wh(h(p))), g: seed_g, p });
        }
    }

    let mut _pop_guard: u64 = 0;
    while let Some(Node { p, g, .. }) = open.pop() {
        _pop_guard += 1;
        if _pop_guard > 50_000_000 {
            eprintln!("ASTAR_GUARD: 50M pops, bailing (sources={})", sources.len());
            return None;
        }
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
            let mut recon_guard = 0u64;
            while let Some(&prev) = came_from.get(&cur) {
                path.push(prev);
                cur = prev;
                recon_guard += 1;
                if recon_guard > 10_000_000 {
                    eprintln!("ASTAR_GUARD: reconstruction cycle detected");
                    return None;
                }
            }
            path.reverse();
            return Some(AStarResult { path });
        }
        if g > *g_score.get(&p).unwrap_or(&u32::MAX) {
            continue;
        }

        for (next_p, kind) in neighbours(p, grid.layer_count) {
            if !grid.in_bounds(next_p) || !in_window(next_p) {
                continue;
            }
            let cell = grid.get(next_p);
            if !grid::walkable(cell, target_net) {
                continue;
            }
            let is_via = matches!(kind, Move::Via);
            // Hard reject: a via landing inside a through-hole pad
            // collides with the existing PTH drill. No score penalty
            // is high enough to make that legal, so refuse outright.
            if is_via
                && (0..grid.layer_count).any(|l| {
                    matches!(
                        grid.get(GridPoint { layer: l, col: next_p.col, row: next_p.row }),
                        Cell::DrilledPad(_)
                    )
                })
            {
                continue;
            }
            if is_via
                && via_safe_radius > 0
                && !via_safe(grid, next_p, target_net, via_safe_radius)
            {
                continue;
            }
            // Per-trace clearance for a planar move: the candidate
            // centreline cell must keep its clearance disk free of foreign
            // copper. Landing on our OWN pad is always allowed — the pad's
            // clearance to its neighbours is fixed placement/fanout
            // geometry, not the router's to enforce; this is what lets a
            // net reach a fine-pitch pad whose foreign neighbour sits
            // inside the disk. (Vias use `via_safe`, not the disk.)
            if !is_via {
                let own_pad =
                    matches!(cell, Cell::NetPad(n) | Cell::DrilledPad(n) if n == target_net);
                if !own_pad && !grid.clearance_ok_disk(next_p, target_net, &disk) {
                    continue;
                }
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
                        && grid.line_of_sight(parent, next_p, target_net, &disk)
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
                let on_pad = (0..grid.layer_count).any(|l| {
                    matches!(
                        grid.get(GridPoint { layer: l, col: next_p.col, row: next_p.row }),
                        Cell::NetPad(_)
                    )
                });
                if on_pad {
                    best_g = best_g.saturating_add(VIA_IN_PAD_PENALTY);
                }
            }

            if best_g < *g_score.get(&next_p).unwrap_or(&u32::MAX) {
                g_score.insert(next_p, best_g);
                came_from.insert(next_p, best_parent);
                open.push(Node {
                    f: best_g.saturating_add(wh(h(next_p))),
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
    for layer in 0..grid.layer_count {
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

fn neighbours(p: GridPoint, layer_count: u8) -> Vec<(GridPoint, Move)> {
    let l = p.layer;
    let c = p.col;
    let r = p.row;
    // 8 in-plane moves plus one via move to *every other* copper
    // layer. A through-hole via's drilled barrel shorts every copper
    // layer it passes through, so from any layer a single via punch
    // reaches any other layer at the same (col,row). Modelling each
    // reachable layer as its own Move::Via lets the router treat inner
    // layers as first-class routing space (top↔inner↔bottom), not just
    // the two outer layers it used pre-Phase-4. On a 2-layer board this
    // degenerates to the old single top↔bottom flip.
    let mut out = Vec::with_capacity(8 + layer_count.saturating_sub(1) as usize);
    out.push((GridPoint { layer: l, col: c + 1, row: r }, Move::Ortho));
    out.push((GridPoint { layer: l, col: c - 1, row: r }, Move::Ortho));
    out.push((GridPoint { layer: l, col: c, row: r + 1 }, Move::Ortho));
    out.push((GridPoint { layer: l, col: c, row: r - 1 }, Move::Ortho));
    out.push((GridPoint { layer: l, col: c + 1, row: r + 1 }, Move::Diag));
    out.push((GridPoint { layer: l, col: c + 1, row: r - 1 }, Move::Diag));
    out.push((GridPoint { layer: l, col: c - 1, row: r + 1 }, Move::Diag));
    out.push((GridPoint { layer: l, col: c - 1, row: r - 1 }, Move::Diag));
    for tl in 0..layer_count {
        if tl != l {
            out.push((GridPoint { layer: tl, col: c, row: r }, Move::Via));
        }
    }
    out
}
