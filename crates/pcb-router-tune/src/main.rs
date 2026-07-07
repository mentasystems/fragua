//! `pcb-router-tune` — hyperparameter sweep over `pcb_router::RouteOptions`.
//!
//! Two execution modes:
//!   * **In-process** (default) — load a Fragua project file, drive
//!     `pcb_router_tune::run_search` against the cloned board, write
//!     the best routed board back to disk.
//!   * **HTTP** (`--via-http URL`) — drive a running Fragua server so
//!     each trial re-renders in the UI. Implemented entirely in this
//!     binary; the library knows nothing about HTTP.

use std::collections::HashSet;
use std::fs;
use std::path::PathBuf;
use std::time::Instant;

use clap::Parser;
use rand::rngs::StdRng;
use rand::seq::SliceRandom;
use rand::SeedableRng;
use serde::{Deserialize, Serialize};

use pcb_core::{Board, Footprint, Schematic};
use pcb_drc::DrcOptions;
use pcb_router::RouteOptions;
use pcb_router_tune::{
    collect_net_names, run_search, Algorithm, GaConfig, GaOutcome, GaProgress, CELL_CHOICES_MM,
    CLEARANCE_CHOICES_MM, VIA_COST_CHOICES,
};

#[derive(Debug, Serialize, Deserialize)]
struct ProjectFile {
    name: String,
    board: Board,
    schematic: Schematic,
    #[serde(default)]
    palette: Vec<Footprint>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
enum AlgoArg {
    Ga,
    Random,
}

impl From<AlgoArg> for Algorithm {
    fn from(a: AlgoArg) -> Self {
        match a {
            AlgoArg::Ga => Algorithm::Ga,
            AlgoArg::Random => Algorithm::Random,
        }
    }
}

#[derive(Parser, Debug)]
#[command(
    name = "pcb-router-tune",
    about = "Hyperparameter tuner for pcb-router (GA + random search)."
)]
struct Cli {
    input: PathBuf,
    #[arg(long)]
    out: PathBuf,
    #[arg(long, value_enum, default_value_t = AlgoArg::Ga)]
    algorithm: AlgoArg,
    #[arg(long, default_value_t = 120)]
    budget_secs: u64,
    /// GA only: population per generation.
    #[arg(long, default_value_t = 24)]
    population: usize,
    /// GA only: per-gene mutation rate.
    #[arg(long, default_value_t = 0.30)]
    mutation_rate: f64,
    /// GA only: generations with no improvement before early stop.
    #[arg(long, default_value_t = 8)]
    patience: usize,
    /// GA only: hard cap on generations.
    #[arg(long, default_value_t = 50)]
    max_generations: usize,
    /// Random only: hard cap on trials.
    #[arg(long, default_value_t = 500)]
    trials: usize,
    #[arg(long, default_value_t = 0)]
    seed: u64,
    #[arg(long, default_value_t = false)]
    quiet: bool,
    /// Drive a running Fragua HTTP server instead of evaluating in-process.
    /// When set, the GA POSTs `route ...` script lines to <URL>/script and
    /// parses the plain-text response. Lets the live UI re-render each trial.
    #[arg(long)]
    via_http: Option<String>,
    /// Sleep this many milliseconds after each HTTP trial. Gives the Tauri
    /// webview time to drain the per-trial render event so a tight GA loop
    /// doesn't starve the UI thread. No effect on in-process mode.
    #[arg(long, default_value_t = 150)]
    inter_trial_ms: u64,
}

fn main() {
    let cli = Cli::parse();
    if let Err(e) = run(&cli) {
        eprintln!("pcb-router-tune: {e}");
        std::process::exit(1);
    }
}

fn run(cli: &Cli) -> Result<(), String> {
    let bytes = fs::read(&cli.input).map_err(|e| format!("read {}: {e}", cli.input.display()))?;
    let project: ProjectFile = serde_json::from_slice(&bytes)
        .map_err(|e| format!("parse {}: {e}", cli.input.display()))?;

    if let Some(url) = &cli.via_http {
        return run_http(cli, url, &project);
    }

    let drc_opts = DrcOptions::default();
    let config = GaConfig {
        algorithm: cli.algorithm.into(),
        budget_secs: cli.budget_secs,
        population: cli.population,
        mutation_rate: cli.mutation_rate,
        patience: cli.patience,
        max_generations: cli.max_generations,
        trials: cli.trials,
        seed: cli.seed,
    };
    let quiet = cli.quiet;
    let on_progress = |p: &GaProgress, _b: &Board| {
        if !quiet {
            // One line per tick keeps the CLI feel close to before.
            println!(
                "gen {gen:02} [+{secs:.1}s] eval={ev} cached={ca} best={bs:.1} (err={err} vias={vias} cell={cell:.2} via_cost={vc} clear={clr:.2}){tag}",
                gen = p.generation,
                secs = p.elapsed_secs,
                ev = p.evaluations,
                ca = p.cache_hits,
                bs = p.best_score,
                err = p.best_drc_errors,
                vias = p.best_vias,
                cell = p.best_cell_mm,
                vc = p.best_via_cost,
                clr = p.best_clearance_mm,
                tag = if p.improved { " IMPROVED" } else { "" },
            );
        }
    };

    let never_stop = std::sync::atomic::AtomicBool::new(false);
    let (best_board, outcome) =
        run_search(&project.board, &config, &drc_opts, &never_stop, on_progress)?;

    let out_project = ProjectFile {
        name: project.name,
        board: best_board,
        schematic: project.schematic,
        palette: project.palette,
    };
    let out_bytes =
        serde_json::to_vec_pretty(&out_project).map_err(|e| format!("serialise: {e}"))?;
    fs::write(&cli.out, &out_bytes).map_err(|e| format!("write {}: {e}", cli.out.display()))?;

    print_summary(cli, &outcome);
    Ok(())
}

fn print_summary(cli: &Cli, outcome: &GaOutcome) {
    println!();
    match cli.algorithm {
        AlgoArg::Ga => {
            println!(
                "GA finished after {} generations ({}s, {} unique evaluations, {} cache hits).",
                outcome.generations,
                outcome.elapsed_secs.round() as u64,
                outcome.total_evaluations,
                outcome.cache_hits,
            );
        }
        AlgoArg::Random => {
            println!(
                "Random search finished after {} trials in {}s.",
                outcome.total_evaluations,
                outcome.elapsed_secs.round() as u64,
            );
        }
    }
    let Some(best) = outcome.best.as_ref() else {
        println!("(no trials completed)");
        return;
    };
    println!("Best:");
    println!(
        "  params: cell={cell:.2}mm via_cost={vc} clearance={clr:.2}mm",
        cell = best.best_cell_mm,
        vc = best.best_via_cost,
        clr = best.best_clearance_mm,
    );
    if !best.best_net_order.is_empty() {
        let head: Vec<&str> = best
            .best_net_order
            .iter()
            .take(8)
            .map(String::as_str)
            .collect();
        println!("  net_order: {} ...  (first 8 names)", head.join(", "));
    }
    println!(
        "  routing: {err} DRC errors, {fail} failed nets, {len:.1}mm wire, {vias} vias",
        err = best.best_drc_errors,
        fail = best.best_failed_nets,
        len = best.best_length_mm,
        vias = best.best_vias,
    );
    println!("  score: {:.1}", best.best_score);
    println!("  saved to: {}", cli.out.display());
}

// -------------- HTTP mode --------------
//
// Local-only feature kept in the CLI because Fragua-over-HTTP doesn't
// belong in the library. Mirrors the previous behaviour: drive the
// server's `/script` endpoint, parse the text reply, score each trial.

#[derive(Debug, Clone)]
struct HttpMetrics {
    drc_errors: usize,
    failed_nets: usize,
    total_length_mm: f64,
    via_count: usize,
}

#[derive(Debug, Clone)]
struct HttpGenome {
    cell_mm: f64,
    via_cost: u32,
    clearance_mm: f64,
    net_order: Vec<String>,
}

fn run_http(cli: &Cli, url: &str, project: &ProjectFile) -> Result<(), String> {
    let client = reqwest::blocking::Client::builder()
        .timeout(std::time::Duration::from_secs(600))
        .build()
        .map_err(|e| format!("build http client: {e}"))?;
    let base_url = url.trim_end_matches('/').to_string();

    let net_names = collect_net_names(&project.board);

    let baseline_opts = RouteOptions::default();
    let mut rng: StdRng = if cli.seed == 0 {
        StdRng::from_entropy()
    } else {
        StdRng::seed_from_u64(cli.seed)
    };
    let start = Instant::now();
    let mut best: Option<(HttpGenome, HttpMetrics, f64)> = None;
    let mut trial = 0usize;

    while start.elapsed().as_secs() < cli.budget_secs && trial < cli.trials.max(1) {
        let genome = if trial == 0 {
            HttpGenome {
                cell_mm: baseline_opts.cell.to_mm(),
                via_cost: baseline_opts.via_cost,
                clearance_mm: baseline_opts.clearance.to_mm(),
                net_order: net_names.clone(),
            }
        } else {
            let mut net_order = net_names.clone();
            net_order.shuffle(&mut rng);
            HttpGenome {
                cell_mm: *CELL_CHOICES_MM.choose(&mut rng).expect("non-empty"),
                via_cost: *VIA_COST_CHOICES.choose(&mut rng).expect("non-empty"),
                clearance_mm: *CLEARANCE_CHOICES_MM.choose(&mut rng).expect("non-empty"),
                net_order,
            }
        };
        let metrics = http_trial(&client, &base_url, &genome)?;
        if cli.inter_trial_ms > 0 {
            std::thread::sleep(std::time::Duration::from_millis(cli.inter_trial_ms));
        }
        let score = metrics.drc_errors as f64 * 10_000.0
            + metrics.total_length_mm
            + metrics.via_count as f64 * 5.0
            + metrics.failed_nets as f64 * 100_000.0;
        let is_best = best.as_ref().is_none_or(|(_, _, s)| score < *s);
        if !cli.quiet {
            println!(
                "[{:03} +{:.1}s] cell={:.2} via_cost={} clear={:.2} -> err={} fail={} len={:.1} vias={} score={:.1}{}",
                trial + 1,
                start.elapsed().as_secs_f64(),
                genome.cell_mm,
                genome.via_cost,
                genome.clearance_mm,
                metrics.drc_errors,
                metrics.failed_nets,
                metrics.total_length_mm,
                metrics.via_count,
                score,
                if is_best { " (BEST)" } else { "" },
            );
        }
        if is_best {
            best = Some((genome, metrics, score));
        }
        trial += 1;
    }

    let (best_genome, _, _) = best.ok_or("no trials completed")?;
    // Replay the winning trial so the live board lands on it, then save.
    let _ = http_trial(&client, &base_url, &best_genome)?;
    http_save(&client, &base_url, &cli.out)?;
    println!("\nHTTP mode finished. Best params committed to running fragua and saved.");
    Ok(())
}

fn http_trial(
    client: &reqwest::blocking::Client,
    base_url: &str,
    g: &HttpGenome,
) -> Result<HttpMetrics, String> {
    // Sanity-guard the net_order: drop duplicates while preserving first
    // occurrence so the server doesn't reject the script line.
    let mut seen: HashSet<&str> = HashSet::new();
    let order: Vec<&str> = g
        .net_order
        .iter()
        .filter(|n| seen.insert(n.as_str()))
        .map(String::as_str)
        .collect();
    let script = format!(
        "route cell={cell:.4} via_cost={vc} clearance={clr:.4} order={order}",
        cell = g.cell_mm,
        vc = g.via_cost,
        clr = g.clearance_mm,
        order = order.join(","),
    );
    let body = serde_json::json!({ "script": script });
    let resp = client
        .post(format!("{base_url}/script"))
        .json(&body)
        .send()
        .map_err(|e| format!("http POST /script: {e}"))?;
    let status = resp.status();
    let text = resp.text().map_err(|e| format!("http read body: {e}"))?;
    if !status.is_success() {
        return Err(format!("fragua /script {status}: {text}"));
    }
    parse_route_response(&text)
}

fn http_save(
    client: &reqwest::blocking::Client,
    base_url: &str,
    out: &std::path::Path,
) -> Result<(), String> {
    let body = serde_json::json!({ "path": out.to_string_lossy() });
    let resp = client
        .post(format!("{base_url}/save"))
        .json(&body)
        .send()
        .map_err(|e| format!("http POST /save: {e}"))?;
    let status = resp.status();
    let text = resp
        .text()
        .map_err(|e| format!("http read save body: {e}"))?;
    if !status.is_success() {
        return Err(format!("fragua /save {status}: {text}"));
    }
    Ok(())
}

fn parse_route_response(text: &str) -> Result<HttpMetrics, String> {
    let line = text
        .lines()
        .find(|l| l.contains("] Routed:"))
        .ok_or_else(|| format!("no 'Routed:' line in response:\n{text}"))?;
    let (_, after) = line.split_once("Routed:").ok_or("malformed Routed: line")?;
    let _traces = extract_usize_before(after, "traces")?;
    let vias = extract_usize_before(after, "vias")?;
    let length = extract_f64_before(after, "mm wire")?;
    let failed = extract_usize_before(after, "failed")?;
    let drc_errors = extract_usize_before(after, "error(s)")?;
    Ok(HttpMetrics {
        drc_errors,
        failed_nets: failed,
        total_length_mm: length,
        via_count: vias,
    })
}

fn extract_usize_before(s: &str, marker: &str) -> Result<usize, String> {
    let idx = s
        .find(marker)
        .ok_or_else(|| format!("marker '{marker}' missing in: {s}"))?;
    let head = &s[..idx];
    let tok = head
        .split(|c: char| !c.is_ascii_digit())
        .rfind(|t| !t.is_empty())
        .ok_or_else(|| format!("no digits before '{marker}' in: {s}"))?;
    tok.parse::<usize>()
        .map_err(|e| format!("parse usize before '{marker}': {e}"))
}

fn extract_f64_before(s: &str, marker: &str) -> Result<f64, String> {
    let idx = s
        .find(marker)
        .ok_or_else(|| format!("marker '{marker}' missing in: {s}"))?;
    let head = &s[..idx];
    let tok = head
        .split(|c: char| !(c.is_ascii_digit() || c == '.' || c == '-'))
        .rfind(|t| !t.is_empty() && t.chars().any(|c| c.is_ascii_digit()))
        .ok_or_else(|| format!("no number before '{marker}' in: {s}"))?;
    tok.parse::<f64>()
        .map_err(|e| format!("parse f64 before '{marker}': {e}"))
}
