//! Ad-hoc verification: load a real Fragua project and report how many
//! thermal-relief spokes the collision guard drops (i.e. how many would
//! have shorted pad/pour to a foreign net under the old code).
//!
//!   cargo run -p pcb-gerber --example verify_spokes -- <file.fragua>

use std::path::Path;

use pcb_core::thermal::{spoke_clear, POUR_CLEARANCE};
use pcb_core::{Length, Point, Project, ThermalRelief};

fn main() {
    let path = std::env::args().nth(1).expect("usage: verify_spokes <file>");
    let proj = Project::load_from_path(Path::new(&path)).expect("load project");
    let snap = proj.read();
    let board = snap.board();

    let orphan_traces = board.orphan_trace_ids();
    let orphan_vias = board.orphan_via_ids();

    let mut total = 0usize;
    let mut dropped = 0usize;
    let mut drop_pads: Vec<String> = Vec::new();

    for pour in &board.pours {
        let ThermalRelief::Spokes4 {
            spoke_width_mm,
            gap_mm,
        } = pour.thermal_relief
        else {
            continue;
        };
        let spoke_half = Length::from_mm(spoke_width_mm) / 2;
        let layer = pour.layer;
        for fp in board.footprints_in_order() {
            for pad in &fp.pads {
                if !pad.occupies_layer(layer) || pad.net.as_deref() != Some(pour.net.as_str()) {
                    continue;
                }
                let c = fp.pad_world_center(pad);
                let (pw, ph) = fp.pad_world_size(pad);
                let half_w = pw / 2;
                let half_h = ph / 2;
                let len = Length::from_mm(gap_mm + 0.1);
                let nudge = Length::from_mm(0.05);
                let candidates = [
                    (
                        Point::new(c.x - half_w - len, c.y),
                        Point::new(c.x - half_w + nudge, c.y),
                    ),
                    (
                        Point::new(c.x + half_w - nudge, c.y),
                        Point::new(c.x + half_w + len, c.y),
                    ),
                    (
                        Point::new(c.x, c.y - half_h - len),
                        Point::new(c.x, c.y - half_h + nudge),
                    ),
                    (
                        Point::new(c.x, c.y + half_h - nudge),
                        Point::new(c.x, c.y + half_h + len),
                    ),
                ];
                for (a, b) in candidates {
                    total += 1;
                    if !spoke_clear(
                        a,
                        b,
                        spoke_half,
                        POUR_CLEARANCE,
                        pour.net.as_str(),
                        layer,
                        board,
                        &orphan_traces,
                        &orphan_vias,
                    ) {
                        dropped += 1;
                        drop_pads.push(format!("{}.{} ({})", fp.reference, pad.number, pour.net));
                    }
                }
            }
        }
    }

    println!("candidate spokes: {total}");
    println!("dropped (would short under old code): {dropped}");
    for p in &drop_pads {
        println!("  drop spoke at pad {p}");
    }
}
