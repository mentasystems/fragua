//! Library form of the GA tuner. The CLI in `main.rs` and the Fragua
//! Tauri backend both call into [`run_search`]; the algorithm code (GA
//! population loop, OX1, mutation operators, scoring) lives here so it
//! stays a single source of truth.

use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Instant;

use rand::rngs::StdRng;
use rand::seq::SliceRandom;
use rand::{Rng, SeedableRng};

use pcb_core::{Board, Length};
use pcb_drc::DrcOptions;
use pcb_router::{NetOverride, Outcome, RouteOptions, RouteReport};

/// Choices for the cell pitch (mm) gene.
pub const CELL_CHOICES_MM: &[f64] = &[0.20, 0.25, 0.30, 0.40];
/// Choices for the via-cost gene.
pub const VIA_COST_CHOICES: &[u32] = &[4, 6, 8, 10, 12, 16];
/// Choices for the clearance (mm) gene.
pub const CLEARANCE_CHOICES_MM: &[f64] = &[0.20, 0.25, 0.30, 0.35, 0.40];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Algorithm {
    Ga,
    Random,
}

/// Tuning knobs the GA driver consumes. Defaults mirror the CLI's
/// historical defaults so behaviour is stable across the two entry points.
#[derive(Debug, Clone)]
pub struct GaConfig {
    pub algorithm: Algorithm,
    pub budget_secs: u64,
    pub population: usize,
    pub mutation_rate: f64,
    pub patience: usize,
    pub max_generations: usize,
    /// Random-search only: hard cap on trials.
    pub trials: usize,
    /// 0 = seed from entropy.
    pub seed: u64,
}

impl Default for GaConfig {
    fn default() -> Self {
        Self {
            algorithm: Algorithm::Ga,
            budget_secs: 120,
            population: 24,
            mutation_rate: 0.30,
            patience: 8,
            max_generations: 50,
            trials: 500,
            seed: 0,
        }
    }
}

/// Streamed progress snapshot. Emitted at every generation summary AND
/// at the end of each trial inside a generation; the `generation` field
/// is 0 while the initial population is being scored.
#[derive(Debug, Clone)]
pub struct GaProgress {
    pub generation: usize,
    pub evaluations: usize,
    pub cache_hits: usize,
    pub elapsed_secs: f64,
    pub best_score: f64,
    pub best_drc_errors: usize,
    pub best_failed_nets: usize,
    pub best_length_mm: f64,
    pub best_vias: usize,
    pub best_cell_mm: f64,
    pub best_via_cost: u32,
    pub best_clearance_mm: f64,
    pub best_net_order: Vec<String>,
    pub improved: bool,
}

/// Final outcome of a search. `best` is `None` only if the budget
/// expired before a single trial completed; otherwise it carries the
/// final state of the winning genome (same fields as the per-tick
/// `GaProgress`).
#[derive(Debug, Clone)]
pub struct GaOutcome {
    pub generations: usize,
    pub total_evaluations: usize,
    pub cache_hits: usize,
    pub elapsed_secs: f64,
    pub best: Option<GaProgress>,
    pub best_options: RouteOptions,
}

/// Per-trial scoring inputs.
#[derive(Debug, Clone)]
struct TrialMetrics {
    drc_errors: usize,
    failed_nets: usize,
    total_length_mm: f64,
    via_count: usize,
}

#[derive(Debug, Clone)]
struct Genome {
    cell_mm: f64,
    via_cost: u32,
    clearance_mm: f64,
    net_order: Vec<String>,
}

impl Genome {
    fn to_options(&self, baseline: &RouteOptions) -> RouteOptions {
        RouteOptions {
            cell: Length::from_mm(self.cell_mm),
            trace_width: baseline.trace_width,
            clearance: Length::from_mm(self.clearance_mm),
            via_cost: self.via_cost,
            via_drill: baseline.via_drill,
            via_diameter: baseline.via_diameter,
            net_overrides: HashMap::<String, NetOverride>::new(),
            initial_net_order: Some(self.net_order.clone()),
        }
    }

    fn cache_key(&self) -> String {
        format!(
            "{:.4}|{}|{:.4}|{}",
            self.cell_mm,
            self.via_cost,
            self.clearance_mm,
            self.net_order.join(",")
        )
    }
}

/// Collect all net names referenced by board pads, deduplicated, in a
/// stable order (sorted) so the initial heuristic genome is deterministic.
pub fn collect_net_names(board: &Board) -> Vec<String> {
    let mut set: HashSet<String> = HashSet::new();
    for fp in board.footprints_in_order() {
        for pad in &fp.pads {
            if let Some(n) = &pad.net {
                set.insert(n.clone());
            }
        }
    }
    let mut v: Vec<String> = set.into_iter().collect();
    v.sort();
    v
}

fn count_failed_nets(report: &RouteReport) -> usize {
    report
        .per_net
        .iter()
        .filter(|(_, o)| matches!(o, Outcome::Failed { .. }))
        .count()
}

fn compute_score_from_metrics(m: &TrialMetrics) -> f64 {
    m.drc_errors as f64 * 10_000.0
        + m.total_length_mm
        + m.via_count as f64 * 5.0
        + m.failed_nets as f64 * 100_000.0
}

fn random_genome(rng: &mut StdRng, nets: &[String]) -> Genome {
    let mut net_order = nets.to_vec();
    net_order.shuffle(rng);
    Genome {
        cell_mm: *CELL_CHOICES_MM.choose(rng).expect("non-empty"),
        via_cost: *VIA_COST_CHOICES.choose(rng).expect("non-empty"),
        clearance_mm: *CLEARANCE_CHOICES_MM.choose(rng).expect("non-empty"),
        net_order,
    }
}

fn baseline_genome(baseline: &RouteOptions, nets: &[String]) -> Genome {
    Genome {
        cell_mm: baseline.cell.to_mm(),
        via_cost: baseline.via_cost,
        clearance_mm: baseline.clearance.to_mm(),
        net_order: nets.to_vec(),
    }
}

fn tournament<'a>(scored: &'a [(Genome, f64)], k: usize, rng: &mut StdRng) -> &'a Genome {
    let mut best_idx = rng.gen_range(0..scored.len());
    for _ in 1..k {
        let i = rng.gen_range(0..scored.len());
        if scored[i].1 < scored[best_idx].1 {
            best_idx = i;
        }
    }
    &scored[best_idx].0
}

fn crossover(a: &Genome, b: &Genome, rng: &mut StdRng) -> Genome {
    let cell_mm = if rng.gen_bool(0.5) { a.cell_mm } else { b.cell_mm };
    let via_cost = if rng.gen_bool(0.5) { a.via_cost } else { b.via_cost };
    let clearance_mm = if rng.gen_bool(0.5) {
        a.clearance_mm
    } else {
        b.clearance_mm
    };
    let net_order = ox1(&a.net_order, &b.net_order, rng);
    Genome {
        cell_mm,
        via_cost,
        clearance_mm,
        net_order,
    }
}

/// Order Crossover (OX1). Picks two cut points i ≤ j, copies
/// parent_a[i..=j] into the child at the same positions, then fills
/// remaining slots by walking parent_b starting from position (j+1)
/// (wrapping), skipping anything already in the child.
fn ox1(a: &[String], b: &[String], rng: &mut StdRng) -> Vec<String> {
    let n = a.len();
    if n == 0 {
        return Vec::new();
    }
    if n == 1 {
        return a.to_vec();
    }
    let mut i = rng.gen_range(0..n);
    let mut j = rng.gen_range(0..n);
    if i > j {
        std::mem::swap(&mut i, &mut j);
    }
    ox1_with_cuts(a, b, i, j)
}

fn ox1_with_cuts(a: &[String], b: &[String], i: usize, j: usize) -> Vec<String> {
    let n = a.len();
    debug_assert_eq!(b.len(), n);
    let mut child: Vec<Option<String>> = vec![None; n];
    let mut taken: HashSet<String> = HashSet::new();
    for k in i..=j {
        child[k] = Some(a[k].clone());
        taken.insert(a[k].clone());
    }
    let mut write = (j + 1) % n;
    let mut read = (j + 1) % n;
    let mut placed = j - i + 1;
    while placed < n {
        let candidate = &b[read];
        if !taken.contains(candidate) {
            child[write] = Some(candidate.clone());
            taken.insert(candidate.clone());
            write = (write + 1) % n;
            placed += 1;
        }
        read = (read + 1) % n;
    }
    child.into_iter().map(|c| c.expect("filled")).collect()
}

fn mutate(g: &mut Genome, rate: f64, rng: &mut StdRng) {
    if rng.gen_bool(rate) {
        g.cell_mm = *CELL_CHOICES_MM.choose(rng).expect("non-empty");
    }
    if rng.gen_bool(rate) {
        g.via_cost = *VIA_COST_CHOICES.choose(rng).expect("non-empty");
    }
    if rng.gen_bool(rate) {
        g.clearance_mm = *CLEARANCE_CHOICES_MM.choose(rng).expect("non-empty");
    }
    if rng.gen_bool(rate) && g.net_order.len() >= 2 {
        let op = rng.gen_range(0..3);
        match op {
            0 => {
                let i = rng.gen_range(0..g.net_order.len());
                let mut j = rng.gen_range(0..g.net_order.len());
                while j == i {
                    j = rng.gen_range(0..g.net_order.len());
                }
                g.net_order.swap(i, j);
            }
            1 => {
                let max_len = 4.min(g.net_order.len());
                let len = rng.gen_range(2..=max_len);
                let start = rng.gen_range(0..=g.net_order.len() - len);
                g.net_order[start..start + len].reverse();
            }
            _ => {
                let from = rng.gen_range(0..g.net_order.len());
                let item = g.net_order.remove(from);
                let to = rng.gen_range(0..=g.net_order.len());
                g.net_order.insert(to, item);
            }
        }
    }
}

/// Run one routing trial on a fresh clone of `original_board` and
/// return both the metrics and the routed board.
fn evaluate_in_process(
    original_board: &Board,
    drc_opts: &DrcOptions,
    options: &RouteOptions,
) -> (TrialMetrics, Board) {
    let mut work = original_board.clone();
    let report = pcb_router::route(&mut work, options);
    let drc = pcb_drc::run(&work, drc_opts);
    let failed_nets = count_failed_nets(&report);
    (
        TrialMetrics {
            drc_errors: drc.error_count,
            failed_nets,
            total_length_mm: report.total_length_mm,
            via_count: report.via_count,
        },
        work,
    )
}

fn progress_from(
    generation: usize,
    evaluations: usize,
    cache_hits: usize,
    elapsed_secs: f64,
    best_score: f64,
    metrics: &TrialMetrics,
    genome: &Genome,
    improved: bool,
) -> GaProgress {
    GaProgress {
        generation,
        evaluations,
        cache_hits,
        elapsed_secs,
        best_score,
        best_drc_errors: metrics.drc_errors,
        best_failed_nets: metrics.failed_nets,
        best_length_mm: metrics.total_length_mm,
        best_vias: metrics.via_count,
        best_cell_mm: genome.cell_mm,
        best_via_cost: genome.via_cost,
        best_clearance_mm: genome.clearance_mm,
        best_net_order: genome.net_order.clone(),
        improved,
    }
}

/// Drive the configured search algorithm to completion (or to the time
/// budget) against an existing `Board`. Each completed trial AND each
/// generation summary calls `on_progress`. Returns the winning board
/// (routing applied) plus a summary outcome.
pub fn run_search(
    board: &Board,
    config: &GaConfig,
    drc_opts: &DrcOptions,
    should_stop: &AtomicBool,
    mut on_progress: impl FnMut(&GaProgress, &Board),
) -> Result<(Board, GaOutcome), String> {
    let baseline_opts = RouteOptions::default();
    let net_names = collect_net_names(board);

    let mut rng: StdRng = if config.seed == 0 {
        StdRng::from_entropy()
    } else {
        StdRng::seed_from_u64(config.seed)
    };

    match config.algorithm {
        Algorithm::Ga => run_ga(
            board,
            config,
            drc_opts,
            &baseline_opts,
            &net_names,
            &mut rng,
            should_stop,
            &mut on_progress,
        ),
        Algorithm::Random => run_random(
            board,
            config,
            drc_opts,
            &baseline_opts,
            &mut rng,
            should_stop,
            &mut on_progress,
        ),
    }
}

#[allow(clippy::too_many_arguments)]
fn run_ga(
    board: &Board,
    config: &GaConfig,
    drc_opts: &DrcOptions,
    baseline_opts: &RouteOptions,
    net_names: &[String],
    rng: &mut StdRng,
    should_stop: &AtomicBool,
    on_progress: &mut dyn FnMut(&GaProgress, &Board),
) -> Result<(Board, GaOutcome), String> {
    let start = Instant::now();
    let mut cache: HashMap<String, f64> = HashMap::new();
    let mut best_board: Option<Board> = None;
    let mut best_genome: Option<Genome> = None;
    let mut best_metrics: Option<TrialMetrics> = None;
    let mut best_score = f64::INFINITY;
    let mut cache_hits = 0usize;
    let mut unique_evaluations = 0usize;
    let mut last_improved_gen = 0usize;

    let mut population: Vec<Genome> = Vec::with_capacity(config.population);
    population.push(baseline_genome(baseline_opts, net_names));
    while population.len() < config.population {
        population.push(random_genome(rng, net_names));
    }

    let mut generation = 0usize;
    while generation < config.max_generations {
        if should_stop.load(Ordering::SeqCst) {
            break;
        }
        generation += 1;
        if start.elapsed().as_secs() >= config.budget_secs {
            generation -= 1;
            break;
        }

        // Phase 1 (sequential): split population by cache hit/miss.
        let mut cached_scored: Vec<(Genome, f64)> = Vec::new();
        let mut to_eval: Vec<Genome> = Vec::with_capacity(population.len());
        for genome in population.drain(..) {
            let key = genome.cache_key();
            if let Some(s) = cache.get(&key) {
                cache_hits += 1;
                cached_scored.push((genome, *s));
            } else {
                to_eval.push(genome);
            }
        }

        // Phase 2 (parallel): score the uncached genomes across cores.
        // Each trial clones the input board so there's no shared mutable
        // state; only the routing/DRC computation runs concurrently.
        use rayon::prelude::*;
        let evaluated: Vec<(Genome, TrialMetrics, Board, f64)> = to_eval
            .into_par_iter()
            .map(|genome| {
                let options = genome.to_options(baseline_opts);
                let (metrics, work) = evaluate_in_process(board, drc_opts, &options);
                let score = compute_score_from_metrics(&metrics);
                (genome, metrics, work, score)
            })
            .collect();

        // Phase 3 (sequential): integrate results in deterministic order,
        // update cache, fire callbacks, track best.
        let mut scored: Vec<(Genome, f64)> = Vec::with_capacity(population.len());
        for (genome, metrics, work, score) in evaluated {
            cache.insert(genome.cache_key(), score);
            unique_evaluations += 1;

            let is_new_best = score < best_score;
            if is_new_best {
                best_score = score;
                best_genome = Some(genome.clone());
                best_metrics = Some(metrics.clone());
                last_improved_gen = generation;
            }

            if let (Some(g), Some(m)) = (best_genome.as_ref(), best_metrics.as_ref()) {
                let snap = progress_from(
                    generation,
                    unique_evaluations,
                    cache_hits,
                    start.elapsed().as_secs_f64(),
                    best_score,
                    m,
                    g,
                    is_new_best,
                );
                on_progress(&snap, &work);
            }

            if is_new_best {
                best_board = Some(work);
            }

            scored.push((genome, score));
        }
        scored.extend(cached_scored);
        scored.sort_by(|a, b| a.1.partial_cmp(&b.1).unwrap_or(std::cmp::Ordering::Equal));

        // Generation-summary tick (improved flag carries the "did this
        // generation set a new best" signal). Pass the best board so the
        // UI lands on it between generations.
        if let (Some(g), Some(m), Some(b)) = (
            best_genome.as_ref(),
            best_metrics.as_ref(),
            best_board.as_ref(),
        ) {
            let snap = progress_from(
                generation,
                unique_evaluations,
                cache_hits,
                start.elapsed().as_secs_f64(),
                best_score,
                m,
                g,
                last_improved_gen == generation,
            );
            on_progress(&snap, b);
        }

        if generation - last_improved_gen >= config.patience {
            break;
        }
        if start.elapsed().as_secs() >= config.budget_secs {
            break;
        }
        if generation >= config.max_generations {
            break;
        }

        let mut next: Vec<Genome> = Vec::with_capacity(config.population);
        let elite_count = 2.min(scored.len());
        for entry in scored.iter().take(elite_count) {
            next.push(entry.0.clone());
        }
        while next.len() < config.population {
            let p1 = tournament(&scored, 3, rng);
            let p2 = tournament(&scored, 3, rng);
            let mut child = crossover(p1, p2, rng);
            mutate(&mut child, config.mutation_rate, rng);
            next.push(child);
        }
        population = next;
    }

    let elapsed = start.elapsed().as_secs_f64();
    let (Some(best_board), Some(best_genome), Some(best_metrics)) =
        (best_board, best_genome, best_metrics)
    else {
        return Ok((
            board.clone(),
            GaOutcome {
                generations: generation,
                total_evaluations: unique_evaluations,
                cache_hits,
                elapsed_secs: elapsed,
                best: None,
                best_options: baseline_opts.clone(),
            },
        ));
    };

    let best_options = best_genome.to_options(baseline_opts);
    let best = progress_from(
        generation,
        unique_evaluations,
        cache_hits,
        elapsed,
        best_score,
        &best_metrics,
        &best_genome,
        true,
    );
    Ok((
        best_board,
        GaOutcome {
            generations: generation,
            total_evaluations: unique_evaluations,
            cache_hits,
            elapsed_secs: elapsed,
            best: Some(best),
            best_options,
        },
    ))
}

#[allow(clippy::too_many_arguments)]
fn run_random(
    board: &Board,
    config: &GaConfig,
    drc_opts: &DrcOptions,
    baseline_opts: &RouteOptions,
    rng: &mut StdRng,
    should_stop: &AtomicBool,
    on_progress: &mut dyn FnMut(&GaProgress, &Board),
) -> Result<(Board, GaOutcome), String> {
    let start = Instant::now();
    let mut best_board: Option<Board> = None;
    let mut best_genome: Option<Genome> = None;
    let mut best_metrics: Option<TrialMetrics> = None;
    let mut best_score = f64::INFINITY;
    let mut trial_idx = 0usize;

    while trial_idx < config.trials {
        if start.elapsed().as_secs() >= config.budget_secs || should_stop.load(Ordering::SeqCst) {
            break;
        }

        let genome = if trial_idx == 0 {
            Genome {
                cell_mm: baseline_opts.cell.to_mm(),
                via_cost: baseline_opts.via_cost,
                clearance_mm: baseline_opts.clearance.to_mm(),
                net_order: Vec::new(),
            }
        } else {
            Genome {
                cell_mm: *CELL_CHOICES_MM.choose(rng).expect("non-empty"),
                via_cost: *VIA_COST_CHOICES.choose(rng).expect("non-empty"),
                clearance_mm: *CLEARANCE_CHOICES_MM.choose(rng).expect("non-empty"),
                net_order: Vec::new(),
            }
        };

        let options = genome.to_options(baseline_opts);
        let (metrics, work) = evaluate_in_process(board, drc_opts, &options);
        let score = compute_score_from_metrics(&metrics);
        let is_best = score < best_score;
        if is_best {
            best_score = score;
            best_genome = Some(genome.clone());
            best_metrics = Some(metrics.clone());
        }

        trial_idx += 1;

        if let (Some(g), Some(m)) = (best_genome.as_ref(), best_metrics.as_ref()) {
            let snap = progress_from(
                0,
                trial_idx,
                0,
                start.elapsed().as_secs_f64(),
                best_score,
                m,
                g,
                is_best,
            );
            on_progress(&snap, &work);
        }

        if is_best {
            best_board = Some(work);
        }
    }

    let elapsed = start.elapsed().as_secs_f64();
    let (Some(best_board), Some(best_genome), Some(best_metrics)) =
        (best_board, best_genome, best_metrics)
    else {
        return Ok((
            board.clone(),
            GaOutcome {
                generations: 0,
                total_evaluations: trial_idx,
                cache_hits: 0,
                elapsed_secs: elapsed,
                best: None,
                best_options: baseline_opts.clone(),
            },
        ));
    };
    let best_options = best_genome.to_options(baseline_opts);
    let best = progress_from(
        0,
        trial_idx,
        0,
        elapsed,
        best_score,
        &best_metrics,
        &best_genome,
        true,
    );
    Ok((
        best_board,
        GaOutcome {
            generations: 0,
            total_evaluations: trial_idx,
            cache_hits: 0,
            elapsed_secs: elapsed,
            best: Some(best),
            best_options,
        },
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Canonical OX1 example. Parents on 9-element permutations, cut
    /// window [3..=6] from A. Result must contain A's [3..=6] in place
    /// and fill the remaining slots by walking B from index 7 wrapping,
    /// skipping anything already in the child.
    ///
    /// A = 1 2 3 | 4 5 6 7 | 8 9
    /// B = 5 7 4 | 9 1 3 6 | 2 8
    /// Cut points i=3, j=6.
    /// Final child = 9 1 3 4 5 6 7 2 8
    #[test]
    fn ox1_canonical_example() {
        let a: Vec<String> = ["1", "2", "3", "4", "5", "6", "7", "8", "9"]
            .iter()
            .map(ToString::to_string)
            .collect();
        let b: Vec<String> = ["5", "7", "4", "9", "1", "3", "6", "2", "8"]
            .iter()
            .map(ToString::to_string)
            .collect();
        let child = ox1_with_cuts(&a, &b, 3, 6);
        let expected: Vec<String> = ["9", "1", "3", "4", "5", "6", "7", "2", "8"]
            .iter()
            .map(ToString::to_string)
            .collect();
        assert_eq!(child, expected);
    }
}
