//! Load a Fragua project and write its fab pack (gerbers + drill + BOM)
//! to a directory, using the current code.
//!
//!   cargo run -p pcb-gerber --example export_fab -- <file.fragua> <out_dir> [stem]

use std::path::{Path, PathBuf};

use pcb_core::Project;

fn main() {
    let mut args = std::env::args().skip(1);
    let file = args.next().expect("usage: export_fab <file> <out_dir> [stem]");
    let out_dir = args.next().expect("usage: export_fab <file> <out_dir> [stem]");
    let proj = Project::load_from_path(Path::new(&file)).expect("load project");
    let snap = proj.read();
    let stem = args.next().unwrap_or_else(|| snap.name().to_string());
    let paths = pcb_gerber::write_fab_pack(snap.board(), &stem, &PathBuf::from(&out_dir))
        .expect("write_fab_pack");
    println!("wrote {} files to {out_dir}:", paths.len());
    for p in &paths {
        println!("  {}", p.display());
    }
}
