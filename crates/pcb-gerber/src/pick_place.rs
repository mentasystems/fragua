//! Pick-and-place position file (CSV).

use std::io::{self, Write};

use pcb_core::Board;

pub fn write(board: &Board, w: &mut impl Write) -> io::Result<()> {
    writeln!(w, "Reference,Value,Footprint,X,Y,Rotation,Side")?;
    for fp in board.footprints_in_order() {
        let side = if fp.layer.is_top() { "top" } else { "bottom" };
        writeln!(
            w,
            "{},{},{},{:.4},{:.4},{:.2},{}",
            csv(&fp.reference),
            csv(&fp.value),
            csv(&fp.library),
            fp.position.x.to_mm(),
            fp.position.y.to_mm(),
            fp.rotation,
            side,
        )?;
    }
    Ok(())
}

fn csv(s: &str) -> String {
    if s.contains(',') || s.contains('"') || s.contains('\n') {
        let escaped = s.replace('"', "\"\"");
        format!("\"{escaped}\"")
    } else {
        s.to_string()
    }
}
