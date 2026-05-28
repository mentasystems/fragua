//! ODB++ "features" file emission.
//!
//! Implements just the subset of the symbol-level syntax that JLCPCB
//! cares about: `L` lines (traces), `P` pads (positive flashes), and
//! `S ... SR ... SE` surfaces (regions). Apertures are declared
//! inline above the feature lines via `$<id> standard <shape>` (we
//! follow the ODB++ "Mentor format spec" convention — newer revisions
//! use a separate `apertures` file, which we skip).

use pcb_core::{Board, CopperLayer, Footprint, Pad};

use crate::LayerKind;

/// Build the text of one layer's `features` file.
#[must_use]
pub fn build_layer(board: &Board, kind: LayerKind) -> String {
    let mut out = String::new();
    out.push_str("UNITS=MM\n");
    // ODB++ features files declare apertures inline. We keep one
    // aperture (id 1) — a 0.25 mm round line used for traces and a
    // generic rectangular flash for pads is enough at this fidelity.
    out.push_str("$1 standard r0.25\n");
    out.push_str("LP=P\n");

    match kind {
        LayerKind::CopperTop | LayerKind::CopperBottom => {
            let layer = match kind {
                LayerKind::CopperTop => CopperLayer::Top,
                _ => CopperLayer::Bottom,
            };
            emit_copper_layer(&mut out, board, layer);
        }
        LayerKind::Drill => emit_drill_layer(&mut out, board),
        LayerKind::Outline => emit_outline_layer(&mut out, board),
        LayerKind::SilkTop | LayerKind::SilkBottom => {
            // Silk emission is intentionally empty in this iteration —
            // the renderer / Gerber writer keep that information; ODB++
            // export doesn't need it for JLC's basic ingestion.
        }
        LayerKind::SoldermaskTop | LayerKind::SoldermaskBottom => {
            let layer = match kind {
                LayerKind::SoldermaskTop => CopperLayer::Top,
                _ => CopperLayer::Bottom,
            };
            // Mask follows the pad rectangles + clearance — we keep
            // the rectangle outline only, no clearance bloat (the fab
            // applies a default mask expand of ~0.05 mm on import).
            emit_mask_layer(&mut out, board, layer);
        }
    }
    out
}

fn emit_copper_layer(out: &mut String, board: &Board, layer: CopperLayer) {
    // Pads: `P x y aperture_id symbol_index polarity dcode net_index`
    // We emit the simplified positional form.
    let mut net_idx = build_net_index(board);
    let mut pad_idx = 0_usize;
    for fp in board.footprints_in_order() {
        for pad in &fp.pads {
            if !pad.occupies_layer(layer) {
                continue;
            }
            let (cx, cy) = pad_world_center(fp, pad);
            let (pw, ph) = pad_world_size(fp, pad);
            let net = pad
                .net
                .as_deref()
                .and_then(|n| net_idx.get(n).copied())
                .unwrap_or(usize::MAX);
            // Use rectangular surface fill for the pad so the
            // aperture id (round 0.25 mm) doesn't have to be
            // re-declared per pad. `R` symbol = rectangle.
            out.push_str(&format!(
                "S P {sym}\nOB {x1:.4} {y1:.4} I\nOS {x2:.4} {y1:.4}\nOS {x2:.4} {y2:.4}\nOS {x1:.4} {y2:.4}\nOS {x1:.4} {y1:.4}\nOE\nSE NET={net} REF={r} PIN={p} PAD={pad_idx}\n",
                sym = pad_idx,
                x1 = cx - pw / 2.0,
                y1 = cy - ph / 2.0,
                x2 = cx + pw / 2.0,
                y2 = cy + ph / 2.0,
                net = net,
                r = fp.reference,
                p = pad.number,
            ));
            pad_idx += 1;
        }
    }
    // Traces — emitted as `L x1 y1 x2 y2 aperture_id polarity NET=...`.
    for trace in &board.traces {
        if trace.layer != layer {
            continue;
        }
        let net_id = net_idx
            .get(trace.net.as_str())
            .copied()
            .unwrap_or_else(|| {
                let n = net_idx.len();
                net_idx.insert(trace.net.clone(), n);
                n
            });
        out.push_str(&format!(
            "L {x1:.4} {y1:.4} {x2:.4} {y2:.4} 1 P NET={net} ID={id}\n",
            x1 = trace.start.x.to_mm(),
            y1 = trace.start.y.to_mm(),
            x2 = trace.end.x.to_mm(),
            y2 = trace.end.y.to_mm(),
            net = trace.net,
            id = net_id,
        ));
    }
    // Vias — `P x y aperture_id with drill annotation`. We emit a
    // dedicated symbol per via id.
    for via in &board.vias {
        out.push_str(&format!(
            "P {x:.4} {y:.4} 1 P NET={net} DIA={d:.4} DRILL={drill:.4}\n",
            x = via.position.x.to_mm(),
            y = via.position.y.to_mm(),
            net = via.net,
            d = via.diameter.to_mm(),
            drill = via.drill.to_mm(),
        ));
    }
}

fn emit_drill_layer(out: &mut String, board: &Board) {
    // Drill layer: vias + through-hole pads, one `P` per hole.
    for via in &board.vias {
        out.push_str(&format!(
            "P {x:.4} {y:.4} 1 P DRILL={d:.4}\n",
            x = via.position.x.to_mm(),
            y = via.position.y.to_mm(),
            d = via.drill.to_mm(),
        ));
    }
    for fp in board.footprints_in_order() {
        for pad in &fp.pads {
            let Some(drill) = pad.drill else { continue };
            let (cx, cy) = pad_world_center(fp, pad);
            out.push_str(&format!(
                "P {cx:.4} {cy:.4} 1 P DRILL={d:.4}\n",
                d = drill.to_mm(),
            ));
        }
    }
}

fn emit_outline_layer(out: &mut String, board: &Board) {
    if let Some(rect) = board.outline {
        let x1 = rect.min.x.to_mm();
        let y1 = rect.min.y.to_mm();
        let x2 = rect.max.x.to_mm();
        let y2 = rect.max.y.to_mm();
        for (sx, sy, ex, ey) in [
            (x1, y1, x2, y1),
            (x2, y1, x2, y2),
            (x2, y2, x1, y2),
            (x1, y2, x1, y1),
        ] {
            out.push_str(&format!("L {sx:.4} {sy:.4} {ex:.4} {ey:.4} 1 P\n"));
        }
    }
}

fn emit_mask_layer(out: &mut String, board: &Board, layer: CopperLayer) {
    for fp in board.footprints_in_order() {
        for pad in &fp.pads {
            if !pad.occupies_layer(layer) {
                continue;
            }
            let (cx, cy) = pad_world_center(fp, pad);
            let (pw, ph) = pad_world_size(fp, pad);
            out.push_str(&format!(
                "S P {sym}\nOB {x1:.4} {y1:.4} I\nOS {x2:.4} {y1:.4}\nOS {x2:.4} {y2:.4}\nOS {x1:.4} {y2:.4}\nOS {x1:.4} {y1:.4}\nOE\nSE REF={r} PIN={p}\n",
                sym = 0,
                x1 = cx - pw / 2.0,
                y1 = cy - ph / 2.0,
                x2 = cx + pw / 2.0,
                y2 = cy + ph / 2.0,
                r = fp.reference,
                p = pad.number,
            ));
        }
    }
}

fn build_net_index(board: &Board) -> std::collections::HashMap<String, usize> {
    let mut idx = std::collections::HashMap::new();
    for fp in board.footprints_in_order() {
        for pad in &fp.pads {
            if let Some(n) = pad.net.as_deref() {
                if !idx.contains_key(n) {
                    let next = idx.len();
                    idx.insert(n.to_string(), next);
                }
            }
        }
    }
    idx
}

fn pad_world_center(fp: &Footprint, pad: &Pad) -> (f64, f64) {
    let c = fp.pad_world_center(pad);
    (c.x.to_mm(), c.y.to_mm())
}

fn pad_world_size(fp: &Footprint, pad: &Pad) -> (f64, f64) {
    let (w, h) = fp.pad_world_size(pad);
    (w.to_mm(), h.to_mm())
}

/// Parsed form of an `L` feature line. Used by the round-trip test —
/// the writer emits, the parser reads back, the test confirms
/// fidelity.
#[derive(Debug, Clone)]
pub struct ParsedLine {
    pub x1: f64,
    pub y1: f64,
    pub x2: f64,
    pub y2: f64,
    pub aperture: u32,
    pub polarity: char,
    pub net: String,
}

/// Parse one `L x1 y1 x2 y2 ap P NET=...` line. Returns `None` if
/// the line doesn't match the expected shape.
#[must_use]
pub fn parse_l_line(line: &str) -> Option<ParsedLine> {
    if !line.starts_with("L ") {
        return None;
    }
    let parts: Vec<&str> = line.split_whitespace().collect();
    if parts.len() < 7 {
        return None;
    }
    let x1 = parts[1].parse::<f64>().ok()?;
    let y1 = parts[2].parse::<f64>().ok()?;
    let x2 = parts[3].parse::<f64>().ok()?;
    let y2 = parts[4].parse::<f64>().ok()?;
    let aperture = parts[5].parse::<u32>().ok()?;
    let polarity = parts[6].chars().next()?;
    let mut net = String::new();
    for tok in &parts[7..] {
        if let Some(rest) = tok.strip_prefix("NET=") {
            net = rest.to_string();
            break;
        }
    }
    Some(ParsedLine {
        x1,
        y1,
        x2,
        y2,
        aperture,
        polarity,
        net,
    })
}
