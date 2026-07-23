//! Headless E2E: load a `.fragua`/`.json` project, run a DSL script
//! against it (default: two-stage auto-place of every footprint + drc),
//! and write before/after SVG renders next to the given output stem.
//!
//!   cargo run -p pcb-script --example place_project --release -- \
//!       <project.json> <out-stem> [script-file]

use std::path::Path;

use pcb_core::Project;
use serde_json::json;

#[tokio::main(flavor = "current_thread")]
async fn main() {
    let mut args = std::env::args().skip(1);
    let project_path = args
        .next()
        .expect("usage: place_project <project> <out-stem> [script]");
    let out_stem = args
        .next()
        .expect("usage: place_project <project> <out-stem> [script]");
    let script_file = args.next();

    let project = Project::load_from_path(Path::new(&project_path)).expect("project loads");
    let board = project.read().board().clone();
    std::fs::write(
        format!("{out_stem}-before.svg"),
        pcb_render::render_svg(&board),
    )
    .unwrap();

    let script = match script_file {
        Some(f) => std::fs::read_to_string(f).unwrap(),
        None => {
            let refs: Vec<String> = board
                .footprints_in_order()
                .map(|fp| fp.reference.clone())
                .collect();
            format!("auto-place {} seed=42\ndrc", refs.join(" "))
        }
    };
    eprintln!("--- script ---\n{script}\n--------------");

    let out =
        match pcb_script::tools::dispatch(&project, "script", &json!({ "script": script })).await {
            Ok(v) => v,
            Err(e) => panic!("script dispatch failed: [{}] {}", e.code, e.message),
        };
    println!("{}", serde_json::to_string_pretty(&out).unwrap());

    let board = project.read().board().clone();
    std::fs::write(
        format!("{out_stem}-after.svg"),
        pcb_render::render_svg(&board),
    )
    .unwrap();
    eprintln!("wrote {out_stem}-before.svg / {out_stem}-after.svg");
}
