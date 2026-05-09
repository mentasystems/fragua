//! `pcb-placer` — simulated-annealing footprint placer.
//!
//! Given a board, a set of "movable" footprint references, and an
//! options bundle, the placer searches for new positions that minimise
//! total HPWL (sum of half-perimeter wire length over every net) while
//! keeping all footprints inside the outline, non-overlapping, and (for
//! `edge_mounted = true` parts) still touching the outline.
//!
//! Movability is per-call, not per-footprint state: callers pass the
//! list of refs they want the placer to touch. Everything else stays
//! pinned. This keeps the on-disk footprint schema unchanged and lets
//! the agent run a focused "auto-place these three parts" without
//! committing the whole board to a move.
//!
//! The objective is HPWL — same metric the DRC and router report — so
//! a successful placement run translates directly into shorter wire and
//! lower detour ratios when the router is rerun afterwards.

use std::collections::HashMap;

use pcb_core::{Board, Footprint, Id, Length, Point};

/// Tunables for the SA search. Defaults are calibrated on the
/// door-controller test project: ~3 s wall-clock, reliably converges
/// to within a few percent of HPWL minimum for ≤20 footprints.
#[derive(Debug, Clone)]
pub struct PlaceOptions {
    /// How many SA candidate moves to try in total.
    pub max_iterations: usize,
    /// Starting temperature. Tuned in the same units as HPWL (mm) so a
    /// move that worsens HPWL by ~T at iteration 0 has ~37 % accept
    /// probability — wide exploration up front.
    pub initial_temp: f64,
    /// Final temperature; the schedule cools geometrically from
    /// `initial_temp` to `final_temp` over `max_iterations` steps.
    pub final_temp: f64,
    /// Largest random translation, in mm, attempted at the start of
    /// the run. Shrinks linearly to `min_step_mm` as we cool.
    pub max_step_mm: f64,
    pub min_step_mm: f64,
    /// PRNG seed: same seed → same placement, so a script run is
    /// reproducible. `0` lets the placer pick its own seed from the
    /// system clock.
    pub seed: u64,
    /// Minimum body-to-body gap (mm) the placer *prefers*. Soft
    /// constraint: pairs of footprints whose gap drops below this
    /// contribute a quadratic penalty to the SA score, so the placer
    /// pushes them apart over time instead of hard-rejecting moves
    /// that don't already satisfy it. Pad-against-pad collisions
    /// (gap <= 0) ARE hard-rejected — we never produce an electrical
    /// short. Default 1.5 mm gives the router about 5 cells of free
    /// corridor at a 0.25 mm pitch.
    pub min_gap_mm: f64,
    /// Score weight on the soft-gap penalty term. Penalty per pair is
    /// `(min_gap_mm - actual_gap)^2` (mm²) when below the threshold;
    /// the score adds `gap_penalty_factor * Σ penalty` to HPWL. Default
    /// 4.0 makes a 0.5 mm shortfall on one pair cost ~1 mm-equivalent
    /// of HPWL, large enough to be felt without dominating routing.
    pub gap_penalty_factor: f64,
    /// Resolution of the congestion-proxy grid (cells per side) over
    /// the board outline. The placer rasterises every net's pad bbox
    /// onto this grid and sums per-cell overflow (= count - 1) as a
    /// proxy for "how many nets fight over the same routing channel".
    /// 32 → ~3 mm cells on an 80 mm board; coarse enough to be cheap,
    /// fine enough to distinguish channels. 0 disables congestion.
    pub congestion_resolution: u32,
    /// Score weight on the congestion proxy. Each unit of overflow
    /// (= one extra net sharing a cell with another) costs this many
    /// mm-equivalent of HPWL. Default 1.0 makes a placement where 50
    /// cells are doubly-claimed cost 50 mm — comparable to the wire
    /// reductions HPWL captures. Tune up if the placer keeps producing
    /// "tight HPWL but unroutable" layouts; tune down if it spreads
    /// parts so far apart wire goes back up.
    pub congestion_penalty_factor: f64,
}

impl Default for PlaceOptions {
    fn default() -> Self {
        Self {
            max_iterations: 8000,
            initial_temp: 50.0,
            final_temp: 0.05,
            max_step_mm: 20.0,
            min_step_mm: 0.5,
            seed: 0,
            // 2 mm body-to-body keeps the router corridor wide enough
            // that even a star net can slip through between any two
            // footprints without DRC pad-pad clearance violations on
            // the typical 0.2 mm clearance.
            min_gap_mm: 2.0,
            // Steep enough that SA reliably enforces `min_gap_mm` on
            // small nets — a 1 mm shortfall on one pair costs
            // 16 mm-equivalent of HPWL, well above the noise floor.
            gap_penalty_factor: 16.0,
            // 32×32 grid maps cleanly to typical SMD boards (50–100 mm
            // wide → 1.5–3 mm cells, finer than a footprint body).
            congestion_resolution: 32,
            // Each shared cell costs ~1 mm of equivalent HPWL —
            // empirically enough to discourage piling nets in one
            // corridor without dominating the score.
            congestion_penalty_factor: 1.0,
        }
    }
}

#[derive(Debug, Clone)]
pub struct PlaceReport {
    /// HPWL at the start of the run, mm.
    pub initial_hpwl_mm: f64,
    /// HPWL of the best placement found, mm.
    pub final_hpwl_mm: f64,
    /// Congestion overflow at the start, summed over the rasterised
    /// grid. 0 = every cell is touched by at most one net's pad bbox.
    pub initial_congestion: f64,
    /// Congestion overflow of the best placement.
    pub final_congestion: f64,
    /// Number of SA candidate moves tried.
    pub iterations: usize,
    /// Of those, how many were applied (improving moves + accepted-uphill).
    pub accepted: usize,
    /// Refs of footprints whose position changed by ≥ 0.05 mm.
    pub moved: Vec<String>,
    /// Refs the caller listed but the placer couldn't touch (unknown,
    /// or no `bounds()` because they have no pads).
    pub skipped: Vec<String>,
}

/// Run the SA placer in-place on `board`. Only footprints whose
/// reference is in `movable` are candidates for movement; everything
/// else is pinned. Returns a report with HPWL before/after and the
/// list of refs whose position changed.
///
/// On error (no outline, no movables found, no nets touching movables),
/// returns a report with `iterations = 0` and `final_hpwl == initial_hpwl`.
pub fn place(
    board: &mut Board,
    movable: &[String],
    opts: &PlaceOptions,
) -> Result<PlaceReport, String> {
    let outline = board
        .outline
        .ok_or_else(|| "auto-place needs a board outline; set one with `outline W H`".to_string())?;

    // Resolve movable refs to ids, skipping unknowns. Capturing ids
    // up front means `movable` order doesn't matter and we don't
    // re-walk the footprint map per move.
    let mut movable_ids: Vec<Id> = Vec::new();
    let mut starting_positions: HashMap<Id, Point> = HashMap::new();
    let mut skipped: Vec<String> = Vec::new();
    for r in movable {
        let found = board
            .footprints_in_order()
            .find(|fp| fp.reference == *r);
        match found {
            Some(fp) if fp.bounds().is_some() => {
                movable_ids.push(fp.id);
                starting_positions.insert(fp.id, fp.position);
            }
            _ => skipped.push(r.clone()),
        }
    }
    if movable_ids.is_empty() {
        return Ok(PlaceReport {
            initial_hpwl_mm: total_hpwl(board),
            final_hpwl_mm: total_hpwl(board),
            initial_congestion: 0.0,
            final_congestion: 0.0,
            iterations: 0,
            accepted: 0,
            moved: Vec::new(),
            skipped,
        });
    }

    // Net membership: for each movable footprint, which nets does it
    // contribute pads to? Used to compute incremental HPWL deltas
    // after a move (HPWL per net depends on min/max pad coords).
    let mut nets_of_id: HashMap<Id, Vec<String>> = HashMap::new();
    for id in &movable_ids {
        let Some(fp) = board.footprints.get(id) else { continue };
        let mut nets: Vec<String> = fp
            .pads
            .iter()
            .filter_map(|p| p.net.clone())
            .collect();
        nets.sort();
        nets.dedup();
        nets_of_id.insert(*id, nets);
    }

    let initial_hpwl = total_hpwl(board);
    let initial_congestion = if opts.congestion_resolution > 0 {
        congestion_overflow(board, outline, opts.congestion_resolution)
    } else {
        0.0
    };
    let initial_score = initial_hpwl
        + opts.gap_penalty_factor * total_gap_penalty(board, opts.min_gap_mm)
        + opts.congestion_penalty_factor * initial_congestion;
    let mut current_score = initial_score;
    let mut best_score = initial_score;
    let mut best_hpwl = initial_hpwl;
    let mut best_congestion = initial_congestion;
    let mut best_positions: HashMap<Id, Point> = movable_ids
        .iter()
        .map(|id| (*id, board.footprints[id].position))
        .collect();
    let mut best_rotations: HashMap<Id, f32> = movable_ids
        .iter()
        .map(|id| (*id, board.footprints[id].rotation))
        .collect();

    let mut rng = Rng::new(if opts.seed == 0 {
        seed_from_clock()
    } else {
        opts.seed
    });

    let cooling = (opts.final_temp / opts.initial_temp).powf(1.0 / opts.max_iterations.max(1) as f64);
    let mut accepted = 0usize;

    for iter in 0..opts.max_iterations {
        // Linear progression of temperature & step size with iteration.
        let temp = opts.initial_temp * cooling.powi(iter as i32);
        let progress = iter as f64 / opts.max_iterations.max(1) as f64;
        let step_mm = opts.max_step_mm * (1.0 - progress) + opts.min_step_mm * progress;

        let id = movable_ids[rng.next_usize() % movable_ids.len()];
        let Some(fp) = board.footprints.get(&id) else { continue };
        let move_kind = if rng.next_u32() % 8 == 0 {
            // 1/8 of the moves try a 90° rotation in place — cheap way
            // to fix orientation without burning translation moves.
            MoveKind::Rotate90 { footprint: id }
        } else {
            // Random translation within `step_mm` mm in either axis.
            let dx_mm = (rng.next_f64() - 0.5) * 2.0 * step_mm;
            let dy_mm = (rng.next_f64() - 0.5) * 2.0 * step_mm;
            MoveKind::Translate {
                footprint: id,
                new_pos: Point::new(
                    fp.position.x + Length::from_mm(dx_mm),
                    fp.position.y + Length::from_mm(dy_mm),
                ),
            }
        };

        // Evaluate the move on a probe clone of the footprint and
        // check constraints. Reject hard violations outright.
        let (probe, original_position, original_rotation) = make_probe(board, &move_kind);
        let Some(probe) = probe else { continue };
        if !inside_outline(&probe, outline) {
            continue;
        }
        // Hard reject: actual pad-on-pad overlap is an electrical
        // short — we never accept that, no matter the temperature.
        // The soft `min_gap_mm` preference is folded into the score
        // delta below so SA can climb out of a tight starting state
        // instead of being stuck.
        if would_overlap(board, &probe, Some(probe.id), 0.0) {
            continue;
        }
        if board.edge_mount_violation(&probe).is_some() {
            continue;
        }

        // Score delta: HPWL is local to the nets this footprint
        // touches; the gap penalty is local to the pairs that touch
        // this footprint; the congestion proxy depends on every net's
        // pad bbox overlapping. Recompute the relevant pieces before
        // and after applying the move.
        let nets = nets_of_id.get(&probe.id).cloned().unwrap_or_default();
        let before_hpwl: f64 = nets.iter().map(|n| net_hpwl(board, n)).sum();
        let before_pen = footprint_gap_penalty(board, probe.id, opts.min_gap_mm);
        let before_cong = if opts.congestion_resolution > 0 {
            congestion_overflow(board, outline, opts.congestion_resolution)
        } else {
            0.0
        };
        // Apply the move temporarily to compute the new HPWL on the
        // affected nets.
        apply_move_in_place(board, &move_kind);
        let after_hpwl: f64 = nets.iter().map(|n| net_hpwl(board, n)).sum();
        let after_pen = footprint_gap_penalty(board, probe.id, opts.min_gap_mm);
        let after_cong = if opts.congestion_resolution > 0 {
            congestion_overflow(board, outline, opts.congestion_resolution)
        } else {
            0.0
        };
        let delta = (after_hpwl - before_hpwl)
            + opts.gap_penalty_factor * (after_pen - before_pen)
            + opts.congestion_penalty_factor * (after_cong - before_cong);

        let accept = if delta <= 0.0 {
            true
        } else {
            // Metropolis: accept uphill with probability exp(-Δ/T).
            let p = (-delta / temp).exp();
            rng.next_f64() < p
        };

        if accept {
            current_score += delta;
            accepted += 1;
            if current_score < best_score {
                best_score = current_score;
                best_hpwl = total_hpwl(board);
                best_congestion = if opts.congestion_resolution > 0 {
                    congestion_overflow(board, outline, opts.congestion_resolution)
                } else {
                    0.0
                };
                for id in &movable_ids {
                    if let Some(fp) = board.footprints.get(id) {
                        best_positions.insert(*id, fp.position);
                        best_rotations.insert(*id, fp.rotation);
                    }
                }
            }
        } else {
            // Roll back.
            revert_move_in_place(board, &move_kind, original_position, original_rotation);
        }
    }

    // Restore the best placement we saw.
    for (id, pos) in &best_positions {
        if let Some(fp) = board.footprints.get_mut(id) {
            fp.position = *pos;
        }
    }
    for (id, rot) in &best_rotations {
        if let Some(fp) = board.footprints.get_mut(id) {
            fp.rotation = *rot;
        }
    }

    let mut moved: Vec<String> = Vec::new();
    for id in &movable_ids {
        let Some(fp) = board.footprints.get(id) else { continue };
        let start = starting_positions[id];
        let dx_mm = (fp.position.x.to_mm() - start.x.to_mm()).abs();
        let dy_mm = (fp.position.y.to_mm() - start.y.to_mm()).abs();
        if dx_mm + dy_mm >= 0.05 {
            moved.push(fp.reference.clone());
        }
    }

    Ok(PlaceReport {
        initial_hpwl_mm: initial_hpwl,
        final_hpwl_mm: best_hpwl,
        initial_congestion,
        final_congestion: best_congestion,
        iterations: opts.max_iterations,
        accepted,
        moved,
        skipped,
    })
}

/// Sum of HPWL across every multi-pad net on the board, mm.
fn total_hpwl(board: &Board) -> f64 {
    let mut nets: HashMap<&str, [f64; 4]> = HashMap::new();
    for fp in board.footprints.values() {
        for pad in &fp.pads {
            let Some(net) = pad.net.as_deref() else { continue };
            let center = fp.pad_world_center(pad);
            let x = center.x.to_mm();
            let y = center.y.to_mm();
            let entry = nets.entry(net).or_insert([f64::INFINITY, f64::INFINITY, f64::NEG_INFINITY, f64::NEG_INFINITY]);
            entry[0] = entry[0].min(x);
            entry[1] = entry[1].min(y);
            entry[2] = entry[2].max(x);
            entry[3] = entry[3].max(y);
        }
    }
    nets.values()
        .filter(|b| b[2] >= b[0] && b[3] >= b[1])
        .map(|b| (b[2] - b[0]) + (b[3] - b[1]))
        .sum()
}

/// HPWL of a single net, mm. Returns 0 if the net has 0 or 1 pads.
fn net_hpwl(board: &Board, net: &str) -> f64 {
    let mut min_x = f64::INFINITY;
    let mut min_y = f64::INFINITY;
    let mut max_x = f64::NEG_INFINITY;
    let mut max_y = f64::NEG_INFINITY;
    let mut count = 0usize;
    for fp in board.footprints.values() {
        for pad in &fp.pads {
            if pad.net.as_deref() != Some(net) {
                continue;
            }
            let c = fp.pad_world_center(pad);
            let x = c.x.to_mm();
            let y = c.y.to_mm();
            min_x = min_x.min(x);
            min_y = min_y.min(y);
            max_x = max_x.max(x);
            max_y = max_y.max(y);
            count += 1;
        }
    }
    if count < 2 {
        return 0.0;
    }
    (max_x - min_x) + (max_y - min_y)
}

/// Routing-congestion proxy: rasterise every net's pad bounding box
/// onto a `res × res` grid spanning `outline`, count nets per cell,
/// and sum `max(0, count - 1)` — the "overflow" of nets sharing a
/// cell. Higher number = more nets fighting over the same routing
/// channel = harder for the router to lay clean copper.
///
/// This is a coarse proxy, not a real router cost: it doesn't know
/// about pad rotation, individual trace widths, or the layered grid.
/// What it captures is the basic "did the placer cluster too many
/// signals through one bottleneck" failure mode that pure HPWL
/// minimisation produces. Cheap to compute (O(N_nets × cells)).
fn congestion_overflow(board: &Board, outline: pcb_core::Rect, res: u32) -> f64 {
    if res == 0 {
        return 0.0;
    }
    let res_i = res as i32;
    let ox = outline.min.x.to_mm();
    let oy = outline.min.y.to_mm();
    let w = outline.width().to_mm();
    let h = outline.height().to_mm();
    if w <= 0.0 || h <= 0.0 {
        return 0.0;
    }
    let cell_w = w / res as f64;
    let cell_h = h / res as f64;

    // Per-net pad-bbox in mm.
    let mut net_bbox: HashMap<&str, [f64; 4]> = HashMap::new();
    for fp in board.footprints.values() {
        for pad in &fp.pads {
            let Some(net) = pad.net.as_deref() else { continue };
            let c = fp.pad_world_center(pad);
            let x = c.x.to_mm();
            let y = c.y.to_mm();
            let entry = net_bbox.entry(net).or_insert([
                f64::INFINITY,
                f64::INFINITY,
                f64::NEG_INFINITY,
                f64::NEG_INFINITY,
            ]);
            entry[0] = entry[0].min(x);
            entry[1] = entry[1].min(y);
            entry[2] = entry[2].max(x);
            entry[3] = entry[3].max(y);
        }
    }

    let mut counts = vec![0u32; (res * res) as usize];
    for [x0, y0, x1, y1] in net_bbox.values() {
        // Single-pad nets contribute nothing to congestion.
        if x1 - x0 < 1e-6 && y1 - y0 < 1e-6 {
            continue;
        }
        let c0 = (((x0 - ox) / cell_w).floor() as i32).clamp(0, res_i - 1);
        let r0 = (((y0 - oy) / cell_h).floor() as i32).clamp(0, res_i - 1);
        let c1 = (((x1 - ox) / cell_w).floor() as i32).clamp(0, res_i - 1);
        let r1 = (((y1 - oy) / cell_h).floor() as i32).clamp(0, res_i - 1);
        for r in r0..=r1 {
            for c in c0..=c1 {
                counts[(r * res_i + c) as usize] += 1;
            }
        }
    }

    counts
        .iter()
        .map(|&n| if n > 1 { (n - 1) as f64 } else { 0.0 })
        .sum()
}

/// AABB gap in mm: positive = clear separation, negative = overlap
/// depth. Used both for the hard-reject pad-overlap check (gap ≤ 0)
/// and the soft `min_gap_mm` penalty term.
fn aabb_gap_mm(a: pcb_core::Rect, b: pcb_core::Rect) -> f64 {
    let dx = if a.max.x.0 < b.min.x.0 {
        (b.min.x.0 - a.max.x.0) as f64 / 1_000_000.0
    } else if b.max.x.0 < a.min.x.0 {
        (a.min.x.0 - b.max.x.0) as f64 / 1_000_000.0
    } else {
        // Overlap on x — measure penetration depth.
        -((a.max.x.0.min(b.max.x.0) - a.min.x.0.max(b.min.x.0)) as f64) / 1_000_000.0
    };
    let dy = if a.max.y.0 < b.min.y.0 {
        (b.min.y.0 - a.max.y.0) as f64 / 1_000_000.0
    } else if b.max.y.0 < a.min.y.0 {
        (a.min.y.0 - b.max.y.0) as f64 / 1_000_000.0
    } else {
        -((a.max.y.0.min(b.max.y.0) - a.min.y.0.max(b.min.y.0)) as f64) / 1_000_000.0
    };
    if dx >= 0.0 && dy >= 0.0 {
        // Clear: shortest separation along either axis.
        dx.min(dy)
    } else if dx >= 0.0 {
        dx
    } else if dy >= 0.0 {
        dy
    } else {
        // Both axes overlap: ACTUAL collision. Return the larger of
        // the two penetration depths (more "negative") so callers
        // testing `gap <= 0` see this as worse than a touch.
        dx.max(dy)
    }
}

/// Quadratic shortfall against `min_gap_mm` for a single pair, mm².
/// 0 if the pair is clear by at least `min_gap_mm`.
fn pair_gap_penalty(a: &Footprint, b: &Footprint, min_gap_mm: f64) -> f64 {
    let Some(ab) = a.bounds() else { return 0.0 };
    let Some(bb) = b.bounds() else { return 0.0 };
    let gap = aabb_gap_mm(ab, bb);
    if gap >= min_gap_mm {
        0.0
    } else {
        // Clip negative gaps (actual overlap) to 0 here — the hard
        // reject already prevents accepting overlapping moves, but if
        // the starting layout is already overlapping we don't want
        // an exploding penalty that drowns out HPWL.
        let s = min_gap_mm - gap.max(0.0);
        s * s
    }
}

/// Sum of `pair_gap_penalty` over every pair `(fp_id, other)`. Cheap
/// when called in the SA loop because only one footprint moved per
/// iteration; everything else is reused.
fn footprint_gap_penalty(board: &Board, fp_id: Id, min_gap_mm: f64) -> f64 {
    let Some(fp) = board.footprints.get(&fp_id) else { return 0.0 };
    board
        .footprints_in_order()
        .filter(|other| other.id != fp_id)
        .map(|other| pair_gap_penalty(fp, other, min_gap_mm))
        .sum()
}

/// Sum of `pair_gap_penalty` over every unordered pair on the board.
fn total_gap_penalty(board: &Board, min_gap_mm: f64) -> f64 {
    let fps: Vec<&Footprint> = board.footprints_in_order().collect();
    let mut sum = 0.0;
    for i in 0..fps.len() {
        for j in (i + 1)..fps.len() {
            sum += pair_gap_penalty(fps[i], fps[j], min_gap_mm);
        }
    }
    sum
}

/// True if `probe` (after inflating its bbox by `gap_mm` on every side)
/// would intersect any other footprint's inflated bbox. The placer
/// uses this only for the hard pad-overlap check (`gap_mm = 0`); the
/// soft min-gap preference is enforced via the SA score, not by reject.
fn would_overlap(board: &Board, probe: &Footprint, ignore_id: Option<Id>, gap_mm: f64) -> bool {
    let extra = Length::from_mm(gap_mm);
    let Some(probe_bounds) = probe.bounds() else { return false };
    let probe_bounds = probe_bounds.expand(extra);
    for fp in board.footprints_in_order() {
        if Some(fp.id) == ignore_id {
            continue;
        }
        if let Some(b) = fp.bounds() {
            if probe_bounds.intersects(&b.expand(extra)) {
                return true;
            }
        }
    }
    false
}

/// True if every corner of `probe`'s bbox sits inside `outline`.
fn inside_outline(probe: &Footprint, outline: pcb_core::Rect) -> bool {
    let Some(b) = probe.bounds() else { return false };
    b.min.x.0 >= outline.min.x.0
        && b.min.y.0 >= outline.min.y.0
        && b.max.x.0 <= outline.max.x.0
        && b.max.y.0 <= outline.max.y.0
}

#[derive(Debug, Clone)]
enum MoveKind {
    Translate { footprint: Id, new_pos: Point },
    Rotate90 { footprint: Id },
}

fn make_probe(board: &Board, m: &MoveKind) -> (Option<Footprint>, Point, f32) {
    match m {
        MoveKind::Translate { footprint, new_pos } => {
            let Some(fp) = board.footprints.get(footprint) else {
                return (None, Point::ORIGIN, 0.0);
            };
            let mut probe = fp.clone();
            probe.position = *new_pos;
            (Some(probe), fp.position, fp.rotation)
        }
        MoveKind::Rotate90 { footprint } => {
            let Some(fp) = board.footprints.get(footprint) else {
                return (None, Point::ORIGIN, 0.0);
            };
            let mut probe = fp.clone();
            probe.rotation = (probe.rotation + 90.0).rem_euclid(360.0);
            (Some(probe), fp.position, fp.rotation)
        }
    }
}

fn apply_move_in_place(board: &mut Board, m: &MoveKind) {
    match m {
        MoveKind::Translate { footprint, new_pos } => {
            if let Some(fp) = board.footprints.get_mut(footprint) {
                fp.position = *new_pos;
            }
        }
        MoveKind::Rotate90 { footprint } => {
            if let Some(fp) = board.footprints.get_mut(footprint) {
                fp.rotation = (fp.rotation + 90.0).rem_euclid(360.0);
            }
        }
    }
}

fn revert_move_in_place(board: &mut Board, m: &MoveKind, original_pos: Point, original_rot: f32) {
    match m {
        MoveKind::Translate { footprint, .. } => {
            if let Some(fp) = board.footprints.get_mut(footprint) {
                fp.position = original_pos;
            }
        }
        MoveKind::Rotate90 { footprint } => {
            if let Some(fp) = board.footprints.get_mut(footprint) {
                fp.rotation = original_rot;
            }
        }
    }
}

/// Tiny xorshift64* RNG. Self-contained so the placer doesn't pull in
/// `rand` (and we get deterministic results from a u64 seed). Uniform
/// enough for SA's accept/reject — we don't need crypto-grade output.
struct Rng {
    state: u64,
}

impl Rng {
    fn new(seed: u64) -> Self {
        Self { state: seed.max(1) }
    }
    fn next_u64(&mut self) -> u64 {
        let mut x = self.state;
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        self.state = x;
        x.wrapping_mul(2685821657736338717)
    }
    fn next_u32(&mut self) -> u32 {
        (self.next_u64() >> 32) as u32
    }
    fn next_usize(&mut self) -> usize {
        self.next_u64() as usize
    }
    /// Uniform [0, 1).
    fn next_f64(&mut self) -> f64 {
        // 53 bits of mantissa; standard recipe.
        (self.next_u64() >> 11) as f64 * (1.0 / (1u64 << 53) as f64)
    }
}

fn seed_from_clock() -> u64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0xfeed_face_dead_beef)
}
