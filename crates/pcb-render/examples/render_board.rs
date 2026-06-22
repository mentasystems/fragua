//! Load a Fragua project and write a high-resolution PNG of the board
//! (top view, with pours) so a change can be eyeballed headless.
//!
//!   cargo run -p pcb-render --example render_board -- <file.fragua> <out.png> [width_px]

use std::path::Path;

use pcb_core::Project;

fn main() {
    let mut args = std::env::args().skip(1);
    let file = args.next().expect("usage: render_board <file> <out.png> [width_px]");
    let out = args.next().expect("usage: render_board <file> <out.png> [width_px]");
    let width: u32 = args.next().map_or(2400, |w| w.parse().expect("width_px"));

    let proj = Project::load_from_path(Path::new(&file)).expect("load project");
    let snap = proj.read();
    if out.ends_with(".svg") {
        let svg = pcb_render::render_svg(snap.board());
        std::fs::write(&out, &svg).expect("write svg");
        println!("wrote {} ({} bytes)", out, svg.len());
    } else {
        let png = pcb_render::render_board_png(snap.board(), width).expect("render png");
        std::fs::write(&out, &png).expect("write png");
        println!("wrote {} ({} bytes, {width}px wide)", out, png.len());
    }
}
