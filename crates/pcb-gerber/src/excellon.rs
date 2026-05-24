//! Excellon drill writer.
//!
//! Format: METRIC, leading-zero suppression, 3.3 fixed-point in mm.
//! One tool definition per unique drill size, then `Tn` selects it and
//! `XnnnYnnn` punches a hole.

use std::collections::BTreeMap;
use std::io::{self, Write};

use pcb_core::{Board, Length};

/// Plated holes come from two sources: vias and perforated footprint
/// pads (`Pad::drill = Some(d)`). NPTH stays empty until we model
/// mounting holes.
pub fn write(board: &Board, plated: bool, w: &mut impl Write) -> io::Result<()> {
    let kind = if plated { "PTH" } else { "NPTH" };
    writeln!(w, "M48")?;
    writeln!(w, "; pcb {kind} drills")?;
    writeln!(w, "FMAT,2")?;
    writeln!(w, "METRIC,LZ,000.000")?;

    if plated {
        // Group every plated hole by drill diameter so each tool is
        // defined once. Orphan vias (no surviving same-net trace
        // approaches them) are dropped — drilling a hole the fab
        // would never use. Pad drills are always kept because the
        // pad copper itself is the landing.
        let orphan_vias = board.orphan_via_ids();
        let mut groups: BTreeMap<i64, Vec<(f64, f64)>> = BTreeMap::new();
        for via in &board.vias {
            if orphan_vias.contains(&via.id) {
                continue;
            }
            groups
                .entry(via.drill.0)
                .or_default()
                .push((via.position.x.to_mm(), via.position.y.to_mm()));
        }
        for fp in board.footprints_in_order() {
            for pad in &fp.pads {
                let Some(drill) = pad.drill else {
                    continue;
                };
                let c = fp.pad_world_center(pad);
                groups
                    .entry(drill.0)
                    .or_default()
                    .push((c.x.to_mm(), c.y.to_mm()));
            }
        }
        for (i, drill_nm) in groups.keys().enumerate() {
            let tool_id = i + 1;
            let drill_mm = Length(*drill_nm).to_mm();
            writeln!(w, "T{tool_id}C{drill_mm:.3}")?;
        }
        writeln!(w, "%")?;
        writeln!(w, "G90")?; // absolute coordinates
        for (i, (_drill, points)) in groups.iter().enumerate() {
            let tool_id = i + 1;
            writeln!(w, "T{tool_id}")?;
            for (x, y) in points {
                writeln!(w, "X{x:.3}Y{y:.3}")?;
            }
        }
    } else {
        writeln!(w, "%")?;
    }

    writeln!(w, "M30")?;
    Ok(())
}
