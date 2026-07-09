//! `compact` — headless board compaction harness.
//!
//! Loads a Fragua project JSON, runs the exact compaction routine the
//! `compact` script verb uses (`pcb_script::compact::compact`), prints a
//! one-screen report, and writes the compacted project to an output path.
//! This is how the fecha-gateway-v3 board is validated without the Tauri
//! app.
//!
//! Usage:
//!   cargo run -q --release --example compact -- <in.json> <out.json> [seed=N] [aspect=keep|free] [step=MM]
//!
//! Placement/DRC body margins are pulled from the on-disk default library
//! (same source the verb uses); if the library can't be opened the run
//! falls back to empty margins.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Instant;

use pcb_core::{Board, Footprint, PlacementMargin, Schematic};
use pcb_placer::MarginMap;
use pcb_script::compact::{compact, CompactOptions};
use serde::{Deserialize, Serialize};

#[derive(Deserialize, Serialize)]
struct ProjectFile {
    name: String,
    board: Board,
    schematic: Schematic,
    #[serde(default)]
    palette: Vec<Footprint>,
}

fn parse_kv(args: &[String], key: &str) -> Option<String> {
    let prefix = format!("{key}=");
    args.iter()
        .find_map(|a| a.strip_prefix(&prefix).map(str::to_string))
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let in_path = args.get(1).cloned().unwrap_or_else(|| {
        eprintln!(
            "usage: compact <in.json> <out.json> [seed=N] [aspect=keep|free] [step=MM] [iters=N] [min_w=MM] [min_h=MM]"
        );
        std::process::exit(2);
    });
    let out_path = args.get(2).cloned().unwrap_or_else(|| {
        eprintln!("error: missing <out.json> output path");
        std::process::exit(2);
    });
    let kv = &args[3.min(args.len())..];

    let bytes = std::fs::read(&in_path).expect("read project");
    let pf: ProjectFile = serde_json::from_slice(&bytes).expect("parse project");

    let mut opts = CompactOptions::default();
    if let Some(v) = parse_kv(kv, "seed").and_then(|s| s.parse().ok()) {
        opts.seed = v;
    }
    if let Some(v) = parse_kv(kv, "step").and_then(|s| s.parse().ok()) {
        opts.step_mm = v;
    }
    if let Some(v) = parse_kv(kv, "iters").and_then(|s| s.parse().ok()) {
        opts.place_iters = v;
    }
    opts.min_w_mm = parse_kv(kv, "min_w").and_then(|s| s.parse().ok());
    opts.min_h_mm = parse_kv(kv, "min_h").and_then(|s| s.parse().ok());
    opts.aspect_free = matches!(parse_kv(kv, "aspect").as_deref(), Some("free"));

    // Margins from the default library, mirroring the verb. Empty when
    // the library can't be opened (still runs, just no body keep-outs).
    let (place_margins, drc_margins) = load_margins(&pf.board);

    let schematic = Arc::new(pf.schematic.clone());
    let t0 = Instant::now();
    let outcome = compact(
        &pf.board,
        &schematic,
        &place_margins,
        &drc_margins,
        None,
        &opts,
    )
    .expect("compact");
    let elapsed = t0.elapsed();

    let m = &outcome.metrics;
    println!("=== FRAGUA COMPACT: {} ===", pf.name);
    println!(
        "seed={} aspect={} step={} iters={}  wall={:.1?}",
        opts.seed,
        if opts.aspect_free { "free" } else { "keep" },
        opts.step_mm,
        opts.place_iters,
        elapsed,
    );
    println!(
        "outline: {:.1} x {:.1} mm  ->  {:.1} x {:.1} mm",
        m.old_w_mm, m.old_h_mm, m.new_w_mm, m.new_h_mm
    );
    println!(
        "area:    {:.0} mm^2  ->  {:.0} mm^2  ({:+.1}%)",
        m.old_area_mm2, m.new_area_mm2, -m.area_reduction_pct
    );
    println!(
        "floor:   {:.1} x {:.1} mm   checks={}",
        m.lower_bound_w_mm, m.lower_bound_h_mm, m.checks
    );
    println!(
        "route:   {} traces, {} vias, {:.1} mm wire   failed_nets={}   drc_errors={}",
        m.trace_count, m.via_count, m.total_length_mm, m.failed_nets, m.drc_errors
    );
    println!(
        "RESULT:  {}",
        if outcome.shrunk {
            "COMPACTED"
        } else {
            "NO SHRINK FEASIBLE (board unchanged)"
        }
    );

    // Persist the (possibly shrunk) project in the same on-disk shape.
    let doc = ProjectFile {
        name: pf.name,
        board: outcome.board,
        schematic: pf.schematic,
        palette: pf.palette,
    };
    std::fs::write(
        &out_path,
        serde_json::to_vec_pretty(&doc).expect("serialize"),
    )
    .expect("write output");
    eprintln!("(wrote compacted project -> {out_path})");
}

/// Build the placer + DRC margin maps from the default on-disk library,
/// keyed the same way the `compact` verb builds them. Returns empty maps
/// if the library can't be opened.
fn load_margins(board: &Board) -> (MarginMap, HashMap<String, PlacementMargin>) {
    let Ok(lib) = pcb_core::Library::open_default() else {
        return (MarginMap::new(), HashMap::new());
    };
    let mut place = MarginMap::new();
    for fp in board.footprints_in_order() {
        if fp.key.is_empty() {
            continue;
        }
        let Some(entry) = lib.find(&fp.key) else {
            continue;
        };
        let m = entry.placement_margin;
        if m.is_zero() {
            continue;
        }
        place.insert(fp.id, [m.top_mm, m.right_mm, m.bottom_mm, m.left_mm]);
    }
    let mut drc = HashMap::new();
    for entry in lib.list() {
        if entry.placement_margin.is_zero() {
            continue;
        }
        drc.insert(entry.key, entry.placement_margin);
    }
    (place, drc)
}
