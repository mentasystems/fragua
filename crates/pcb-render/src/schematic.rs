//! Render a `Schematic` to SVG.
//!
//! Symbol bodies are simple boxes with reference + value inside; pins
//! are 2.54 mm stubs poking out of each side, with the connected net
//! name (or pin name, if unconnected) as a label at the stub's tip.
//!
//! The renderer never moves symbols — it draws what the agent placed.
//! Auto-placement (when the agent omits a position) happens in
//! `pcb-script` before the symbol enters the model.

use std::fmt::Write;

use pcb_core::{
    schematic::{PinSide, SchPin, Symbol, SymbolKind},
    Schematic,
};

const PIN_LEN_MM: f64 = 2.54;
const PIN_PITCH_MM: f64 = 2.54;
const DISCRETE_BODY_W_MM: f64 = 7.62; // 3 × 2.54
const DISCRETE_BODY_H_MM: f64 = 2.54;
const IC_BODY_W_MM: f64 = 12.7; // 5 × 2.54

/// Render `schematic` as an SVG document. Coordinates use SVG-default Y
/// going down (schematic editors traditionally do; matches KiCad's eeschema).
#[must_use]
pub fn render_schematic_svg(schematic: &Schematic) -> String {
    let view = view_box(schematic);
    let mut svg = String::with_capacity(2048);
    let _ = write!(
        svg,
        r##"<svg xmlns="http://www.w3.org/2000/svg" viewBox="{:.2} {:.2} {:.2} {:.2}" width="100%" height="100%">"##,
        view.0, view.1, view.2, view.3,
    );
    let _ = write!(
        svg,
        r##"<rect x="{:.2}" y="{:.2}" width="{:.2}" height="{:.2}" fill="#0e1116"/>"##,
        view.0, view.1, view.2, view.3,
    );
    // Light dot grid so the human can read pitch at a glance.
    write_grid(&mut svg, view);

    for sym in schematic.symbols_in_order() {
        write_symbol(&mut svg, schematic, sym);
    }
    svg.push_str("</svg>");
    svg
}

/// Returns (x, y, w, h) in mm. Falls back to a 100 × 70 mm sheet when
/// the schematic is empty.
fn view_box(schematic: &Schematic) -> (f64, f64, f64, f64) {
    if schematic.symbol_order.is_empty() {
        return (0.0, 0.0, 100.0, 70.0);
    }
    let mut min_x = f64::INFINITY;
    let mut min_y = f64::INFINITY;
    let mut max_x = f64::NEG_INFINITY;
    let mut max_y = f64::NEG_INFINITY;
    for sym in schematic.symbols_in_order() {
        let (bw, bh) = body_size(&sym.kind);
        let cx = sym.position.x.to_mm();
        let cy = sym.position.y.to_mm();
        // Include the pin stubs in the bbox so they don't get cropped.
        min_x = min_x.min(cx - bw / 2.0 - PIN_LEN_MM - 5.0);
        max_x = max_x.max(cx + bw / 2.0 + PIN_LEN_MM + 5.0);
        min_y = min_y.min(cy - bh / 2.0 - PIN_LEN_MM - 3.0);
        max_y = max_y.max(cy + bh / 2.0 + PIN_LEN_MM + 3.0);
    }
    (min_x, min_y, max_x - min_x, max_y - min_y)
}

fn body_size(kind: &SymbolKind) -> (f64, f64) {
    match kind {
        SymbolKind::Resistor
        | SymbolKind::Capacitor
        | SymbolKind::Inductor
        | SymbolKind::Led
        | SymbolKind::Diode => (DISCRETE_BODY_W_MM, DISCRETE_BODY_H_MM),
        SymbolKind::GenericIc { pins } => {
            let left = pins.iter().filter(|p| p.side == PinSide::Left).count();
            let right = pins.iter().filter(|p| p.side == PinSide::Right).count();
            let top = pins.iter().filter(|p| p.side == PinSide::Top).count();
            let bottom = pins.iter().filter(|p| p.side == PinSide::Bottom).count();
            #[allow(clippy::cast_precision_loss)]
            let h_pins = left.max(right) as f64;
            #[allow(clippy::cast_precision_loss)]
            let w_pins = top.max(bottom) as f64;
            let h = (h_pins.max(2.0)) * PIN_PITCH_MM + PIN_PITCH_MM;
            let w = (w_pins.max(2.0)) * PIN_PITCH_MM + IC_BODY_W_MM;
            (w, h)
        }
    }
}

fn write_grid(svg: &mut String, view: (f64, f64, f64, f64)) {
    let (vx, vy, vw, vh) = view;
    let step = PIN_PITCH_MM;
    let _ = write!(
        svg,
        r##"<g stroke="#1a1f27" stroke-width="0.05" fill="none">"##
    );
    let mut x = (vx / step).floor() * step;
    while x <= vx + vw {
        let _ = write!(
            svg,
            r##"<line x1="{:.2}" y1="{:.2}" x2="{:.2}" y2="{:.2}"/>"##,
            x,
            vy,
            x,
            vy + vh
        );
        x += step;
    }
    let mut y = (vy / step).floor() * step;
    while y <= vy + vh {
        let _ = write!(
            svg,
            r##"<line x1="{:.2}" y1="{:.2}" x2="{:.2}" y2="{:.2}"/>"##,
            vx,
            y,
            vx + vw,
            y
        );
        y += step;
    }
    svg.push_str("</g>");
}

fn write_symbol(svg: &mut String, schematic: &Schematic, sym: &Symbol) {
    let cx = sym.position.x.to_mm();
    let cy = sym.position.y.to_mm();
    let (bw, bh) = body_size(&sym.kind);
    let bx = cx - bw / 2.0;
    let by = cy - bh / 2.0;

    // Body box.
    let _ = write!(
        svg,
        r##"<rect x="{:.2}" y="{:.2}" width="{:.2}" height="{:.2}" fill="#161b22" stroke="#7d8590" stroke-width="0.15"/>"##,
        bx, by, bw, bh
    );

    // Reference designator above the symbol.
    let _ = write!(
        svg,
        r##"<text x="{:.2}" y="{:.2}" text-anchor="middle" font-family="ui-monospace, monospace" font-size="1.4" fill="#e6edf3">{}</text>"##,
        cx,
        by - 0.6,
        escape(&sym.reference)
    );
    // Value below the symbol.
    let _ = write!(
        svg,
        r##"<text x="{:.2}" y="{:.2}" text-anchor="middle" font-family="ui-monospace, monospace" font-size="1.2" fill="#8b949e">{}</text>"##,
        cx,
        by + bh + 1.6,
        escape(&sym.value)
    );

    let pins = sym.kind.pins();
    let mut idx_per_side = SideCounter::default();
    for pin in &pins {
        write_pin(
            svg,
            schematic,
            sym,
            pin,
            (bx, by, bw, bh),
            &mut idx_per_side,
        );
    }
}

#[derive(Default)]
struct SideCounter {
    left: usize,
    right: usize,
    top: usize,
    bottom: usize,
}

impl SideCounter {
    fn next(&mut self, side: PinSide) -> usize {
        let slot = match side {
            PinSide::Left => &mut self.left,
            PinSide::Right => &mut self.right,
            PinSide::Top => &mut self.top,
            PinSide::Bottom => &mut self.bottom,
        };
        let i = *slot;
        *slot += 1;
        i
    }
}

fn write_pin(
    svg: &mut String,
    schematic: &Schematic,
    sym: &Symbol,
    pin: &SchPin,
    body: (f64, f64, f64, f64),
    counts: &mut SideCounter,
) {
    let (bx, by, bw, bh) = body;
    let i = counts.next(pin.side);
    #[allow(clippy::cast_precision_loss)]
    let i_f = i as f64;
    let (start_x, start_y, end_x, end_y, label_x, label_y, label_anchor) = match pin.side {
        PinSide::Left => {
            let y = by + PIN_PITCH_MM * (i_f + 1.0);
            (
                bx,
                y,
                bx - PIN_LEN_MM,
                y,
                bx - PIN_LEN_MM - 0.4,
                y + 0.4,
                "end",
            )
        }
        PinSide::Right => {
            let y = by + PIN_PITCH_MM * (i_f + 1.0);
            (
                bx + bw,
                y,
                bx + bw + PIN_LEN_MM,
                y,
                bx + bw + PIN_LEN_MM + 0.4,
                y + 0.4,
                "start",
            )
        }
        PinSide::Top => {
            let x = bx + PIN_PITCH_MM * (i_f + 1.0);
            (
                x,
                by,
                x,
                by - PIN_LEN_MM,
                x,
                by - PIN_LEN_MM - 0.4,
                "middle",
            )
        }
        PinSide::Bottom => {
            let x = bx + PIN_PITCH_MM * (i_f + 1.0);
            (
                x,
                by + bh,
                x,
                by + bh + PIN_LEN_MM,
                x,
                by + bh + PIN_LEN_MM + 1.2,
                "middle",
            )
        }
    };

    // Pin stub line.
    let _ = write!(
        svg,
        r##"<line x1="{:.2}" y1="{:.2}" x2="{:.2}" y2="{:.2}" stroke="#c97a2b" stroke-width="0.2"/>"##,
        start_x, start_y, end_x, end_y
    );
    // Pin number, just inside the body next to the stub root.
    let (num_x, num_y, num_anchor) = match pin.side {
        PinSide::Left => (bx + 0.4, start_y - 0.2, "start"),
        PinSide::Right => (bx + bw - 0.4, start_y - 0.2, "end"),
        PinSide::Top => (start_x + 0.3, by + 1.0, "start"),
        PinSide::Bottom => (start_x + 0.3, by + bh - 0.4, "start"),
    };
    let _ = write!(
        svg,
        r##"<text x="{:.2}" y="{:.2}" text-anchor="{}" font-family="ui-monospace, monospace" font-size="0.7" fill="#8b949e">{}</text>"##,
        num_x,
        num_y,
        num_anchor,
        escape(&pin.number)
    );

    // Label at the tip: the net name if connected, otherwise the
    // human-readable pin name. Empty pin name + no net = nothing drawn.
    let label = schematic
        .net_for_pin(sym.id, &pin.number)
        .map_or_else(|| pin.name.clone(), str::to_string);
    if !label.is_empty() {
        let fill = if schematic.net_for_pin(sym.id, &pin.number).is_some() {
            "#3fb950"
        } else {
            "#8b949e"
        };
        let _ = write!(
            svg,
            r##"<text x="{:.2}" y="{:.2}" text-anchor="{}" font-family="ui-monospace, monospace" font-size="1.0" fill="{}">{}</text>"##,
            label_x,
            label_y,
            label_anchor,
            fill,
            escape(&label)
        );
    }
}

fn escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}
