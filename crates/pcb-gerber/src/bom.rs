//! Bill-of-materials writer (CSV).
//!
//! Footprints are grouped by `(value, library)`; references for each
//! group are concatenated. This is the layout JLC, MacroFab and most
//! house assembly tools accept.

use std::collections::BTreeMap;
use std::io::{self, Write};

use pcb_core::Board;

pub fn write(board: &Board, w: &mut impl Write) -> io::Result<()> {
    writeln!(w, "Reference,Value,Footprint,Quantity")?;
    let mut groups: BTreeMap<(String, String), Vec<String>> = BTreeMap::new();
    for fp in board.footprints_in_order() {
        groups
            .entry((fp.value.clone(), fp.library.clone()))
            .or_default()
            .push(fp.reference.clone());
    }
    for ((value, library), mut refs) in groups {
        refs.sort();
        let refs_joined = refs.join(" ");
        writeln!(
            w,
            "{},{},{},{}",
            csv_field(&refs_joined),
            csv_field(&value),
            csv_field(&library),
            refs.len()
        )?;
    }
    Ok(())
}

fn csv_field(s: &str) -> String {
    let needs_quoting = s.contains(',') || s.contains('"') || s.contains('\n');
    if !needs_quoting {
        return s.to_string();
    }
    let escaped = s.replace('"', "\"\"");
    format!("\"{escaped}\"")
}
