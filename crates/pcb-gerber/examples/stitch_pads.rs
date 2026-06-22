//! Plan auto-stitch vias for isolated plane pads on a real Fragua
//! project and report the result.
//!
//!   cargo run -p pcb-gerber --example stitch_pads -- <file.fragua>
//!
//! Prints every isolated pad it would stitch (via position, via-in-pad or
//! beside-pad + stub) and any pad that stays unreachable, then applies the
//! plan to an in-memory copy and re-checks that no isolated pad remains.

use std::path::Path;

use pcb_core::stitch::{apply_stitches, plan_stitches, StitchParams};
use pcb_core::Project;

fn main() {
    let mut args = std::env::args().skip(1);
    let file = args.next().expect("usage: stitch_pads <file> [out.json]");
    let out = args.next();
    let proj = Project::load_from_path(Path::new(&file)).expect("load project");
    let mut board = proj.read().board().clone();

    let plan = plan_stitches(&board, StitchParams::default());
    println!("stitch proposals: {}", plan.proposals.len());
    for s in &plan.proposals {
        let kind = if s.via_in_pad {
            "via-in-pad"
        } else {
            "beside-pad + stub"
        };
        println!(
            "  {} -> via at ({:.3}, {:.3}) mm  [{kind}]",
            s.pad_ref,
            s.via.position.x.to_mm(),
            s.via.position.y.to_mm(),
        );
    }
    println!(
        "unreachable pads (reroute needed): {}",
        plan.unreachable.len()
    );
    for r in &plan.unreachable {
        println!("  {r}");
    }

    let added = apply_stitches(&mut board, &plan);
    let after = plan_stitches(&board, StitchParams::default());
    println!(
        "applied {added} stitches; isolated pads still stitchable: {}, unreachable: {}",
        after.proposals.len(),
        after.unreachable.len()
    );

    if let Some(out) = out {
        for s in &plan.proposals {
            proj.add_via(s.via.clone());
            if let Some(stub) = &s.stub {
                proj.add_trace(stub.clone());
            }
        }
        proj.save_to_path(Path::new(&out)).expect("save");
        println!("wrote stitched board to {out}");
    }
}
