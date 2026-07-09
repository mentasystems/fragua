//! Headless fab-pack check: load a project JSON and run the JLCPCB pack.
//!
//! Usage: cargo run -p pcb-fab --example fabcheck -- <project.json> <out_dir>

use std::path::Path;

fn main() {
    let mut args = std::env::args().skip(1);
    let (Some(src), Some(out)) = (args.next(), args.next()) else {
        eprintln!("usage: fabcheck <project.json> <out_dir>");
        std::process::exit(2);
    };
    let Some(project) = pcb_core::Project::load_from_path(Path::new(&src)) else {
        eprintln!("failed to load {src}");
        std::process::exit(1);
    };
    match pcb_fab::pack(&project, pcb_fab::Provider::Jlcpcb, Path::new(&out)) {
        Ok(report) => {
            println!("blocking: {}", report.blocking);
            for r in &report.blocking_reasons {
                println!("  reason: {r}");
            }
            if report.blocking {
                std::process::exit(1);
            }
        }
        Err(e) => {
            eprintln!("pack failed: {e}");
            std::process::exit(1);
        }
    }
}
