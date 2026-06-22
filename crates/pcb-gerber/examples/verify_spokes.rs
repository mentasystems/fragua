//! Ad-hoc verification: load a real Fragua project and audit its
//! thermal-relief spokes under the current `select_spokes` logic.
//!
//! Reports, per same-net pad, how many spokes survive the foreign-net
//! collision guard, whether the fallback dropped to 45° diagonals, and —
//! most importantly — whether any pad is left ISOLATED (zero spokes), i.e.
//! electrically floating off its plane. A clean board reports 0 isolated.
//!
//!   cargo run -p pcb-gerber --example verify_spokes -- <file.fragua>

use std::path::Path;

use pcb_core::thermal::{select_spokes, POUR_CLEARANCE};
use pcb_core::{Length, Project, ThermalRelief};

fn main() {
    let path = std::env::args().nth(1).expect("usage: verify_spokes <file>");
    let proj = Project::load_from_path(Path::new(&path)).expect("load project");
    let snap = proj.read();
    let board = snap.board();

    let orphan_traces = board.orphan_trace_ids();
    let orphan_vias = board.orphan_via_ids();

    // Audit under both clearances: the Gerber writer guards spokes with
    // the fab clearance (0.2 mm); the renderer guards them with the
    // larger visual void (`POUR_CLEARANCE_MM`, 0.6 mm). Report each so
    // both screen and fab can be checked for isolated pads.
    const RENDER_CLEARANCE: Length = Length(600_000); // 0.6 mm
    for (label, clearance) in [("fab/gerber (0.2 mm)", POUR_CLEARANCE), ("render (0.6 mm)", RENDER_CLEARANCE)] {
        let mut pads = 0usize;
        let mut spokes = 0usize;
        let mut diag_fallback: Vec<String> = Vec::new();
        let mut isolated: Vec<String> = Vec::new();

        for pour in &board.pours {
            let ThermalRelief::Spokes4 {
                spoke_width_mm,
                gap_mm,
            } = pour.thermal_relief
            else {
                continue;
            };
            let spoke_half = Length::from_mm(spoke_width_mm) / 2;
            let reach = Length::from_mm(gap_mm + 0.1);
            let layer = pour.layer;
            for fp in board.footprints_in_order() {
                for pad in &fp.pads {
                    if !pad.occupies_layer(layer) || pad.net.as_deref() != Some(pour.net.as_str()) {
                        continue;
                    }
                    let c = fp.pad_world_center(pad);
                    let (pw, ph) = fp.pad_world_size(pad);
                    let kept = select_spokes(
                        c,
                        pw,
                        ph,
                        spoke_half,
                        clearance,
                        reach,
                        pour.net.as_str(),
                        layer,
                        board,
                        &orphan_traces,
                        &orphan_vias,
                    );
                    pads += 1;
                    spokes += kept.len();
                    let id = format!("{}.{} ({})", fp.reference, pad.number, pour.net);
                    if kept.is_empty() {
                        isolated.push(id);
                    } else {
                        // A spoke whose endpoints differ on both axes is a
                        // diagonal — only emitted when all orthogonals fail.
                        let is_diag = kept
                            .iter()
                            .any(|(a, b)| a.x.0 != b.x.0 && a.y.0 != b.y.0);
                        if is_diag {
                            diag_fallback.push(id);
                        }
                    }
                }
            }
        }

        println!("=== {label} ===");
        println!("  same-net pads with Spokes4: {pads}");
        println!("  spokes emitted: {spokes}");
        println!("  pads using diagonal fallback: {}", diag_fallback.len());
        for p in &diag_fallback {
            println!("    diagonal at pad {p}");
        }
        println!("  ISOLATED pads (no spoke — floating off plane): {}", isolated.len());
        for p in &isolated {
            println!("    ISOLATED pad {p}");
        }
    }
}
