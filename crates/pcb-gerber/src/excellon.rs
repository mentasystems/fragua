//! Excellon drill writer.
//!
//! Phase 3 keeps this as a stub: the board model has no through-holes
//! or vias yet. We still emit a syntactically valid file (header + end
//! marker) so the fab pack is structurally complete and a CAM viewer
//! does not choke on a missing drill file.

use std::io::{self, Write};

use pcb_core::Board;

/// `plated` is encoded into the header comment so PTH and NPTH outputs
/// are distinguishable even when both are empty.
pub fn write(_board: &Board, plated: bool, w: &mut impl Write) -> io::Result<()> {
    let kind = if plated { "PTH" } else { "NPTH" };
    writeln!(w, "M48")?;
    writeln!(w, "; pcb {kind} drills")?;
    writeln!(w, "FMAT,2")?;
    writeln!(w, "METRIC,LZ,000.000")?;
    writeln!(w, "%")?;
    writeln!(w, "M30")?;
    Ok(())
}
