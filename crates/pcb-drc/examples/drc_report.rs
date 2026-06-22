//! Headless DRC over a saved project file. Mirrors `src-tauri`'s
//! `run_drc` (native DRC, default options) so the report matches what
//! the app paints. Highlights the net-continuity checks.
//!
//! Usage: `cargo run -p pcb-drc --example drc_report -- <project.json> [...]`

use std::collections::BTreeMap;
use std::path::Path;

fn main() {
    let paths: Vec<String> = std::env::args().skip(1).collect();
    if paths.is_empty() {
        eprintln!("usage: drc_report <project.json> [more.json ...]");
        std::process::exit(2);
    }

    for path in &paths {
        let Some(project) = pcb_core::Project::load_from_path(Path::new(path)) else {
            println!("== {path} ==\n  FAILED TO LOAD\n");
            continue;
        };
        let snap = project.read();
        let board = snap.board();
        let opts = pcb_drc::DrcOptions::default();
        let report = pcb_drc::run(board, &opts);

        let mut counts: BTreeMap<String, usize> = BTreeMap::new();
        for v in &report.violations {
            *counts.entry(format!("{:?}", v.kind)).or_default() += 1;
        }

        println!("== {path} ==");
        println!(
            "  geometry: {} footprints, {} traces, {} vias, {} pours",
            board.footprints.len(),
            board.traces.len(),
            board.vias.len(),
            board.pours.len(),
        );
        println!(
            "  DRC: {} error(s), {} warning(s)",
            report.error_count, report.warning_count,
        );
        for (kind, n) in &counts {
            println!("    {kind}: {n}");
        }

        // The headline: opens & shorts — the multimeter checks.
        let continuity: Vec<&pcb_drc::Violation> = report
            .violations
            .iter()
            .filter(|v| {
                matches!(
                    v.kind,
                    pcb_drc::ViolationKind::NetSplit | pcb_drc::ViolationKind::NetShort
                )
            })
            .collect();
        if continuity.is_empty() {
            println!("  continuity: no opens/shorts ✔");
        } else {
            println!("  continuity findings:");
            for v in continuity {
                println!(
                    "    [{:?}] {} @ ({:.2}, {:.2})",
                    v.kind, v.message, v.x_mm, v.y_mm,
                );
            }
        }
        println!();
    }
}
