//! `pcb-render` — board / schematic → SVG.
//!
//! SVG is the primary output: the frontend can style and animate it, and
//! it is trivial for the agent to attach as visual context. PNG comes
//! later if/when we hit perf or fidelity needs.

pub mod schematic;
pub use schematic::render_schematic_svg;

use std::fmt::Write;

use pcb_core::{Board, CopperLayer, Footprint, Pad, Rect, Trace, Via};

/// Margin (in board nm) added around the content bounding box when no
/// explicit outline is set, so footprints aren't flush against the edge.
const FALLBACK_MARGIN_NM: i64 = 5_000_000; // 5 mm

/// Render `board` as an SVG document string.
///
/// The viewBox uses millimetres (the natural human-facing unit), with the
/// Y axis flipped so positive Y goes up — matching how PCB tools display
/// boards rather than how SVG defaults work.
#[must_use]
pub fn render_svg(board: &Board) -> String {
    let view = view_rect(board);
    let view_w_mm = view.width().to_mm();
    let view_h_mm = view.height().to_mm();
    let view_x_mm = view.min.x.to_mm();
    let view_y_mm = view.min.y.to_mm();

    let mut svg = String::with_capacity(2048);
    let _ = write!(
        svg,
        r##"<svg xmlns="http://www.w3.org/2000/svg" viewBox="{x:.3} {y:.3} {w:.3} {h:.3}" width="100%" height="100%">"##,
        x = view_x_mm,
        y = -(view_y_mm + view_h_mm),
        w = view_w_mm,
        h = view_h_mm,
    );
    svg.push_str(r##"<g transform="scale(1,-1)">"##);

    // Background.
    let _ = write!(
        svg,
        r##"<rect x="{x:.3}" y="{y:.3}" width="{w:.3}" height="{h:.3}" fill="#0e1116"/>"##,
        x = view_x_mm,
        y = view_y_mm,
        w = view_w_mm,
        h = view_h_mm,
    );

    if let Some(outline) = board.outline {
        write_rect_stroke(&mut svg, outline, "#7d8590", 0.15);
    }

    // Bottom traces first so top traces visually win at crossings.
    for trace in board.traces.iter().filter(|t| t.layer == CopperLayer::Bottom) {
        write_trace(&mut svg, trace);
    }
    for fp in board.footprints_in_order() {
        write_footprint(&mut svg, fp);
    }
    for trace in board.traces.iter().filter(|t| t.layer == CopperLayer::Top) {
        write_trace(&mut svg, trace);
    }
    for via in &board.vias {
        write_via(&mut svg, via);
    }

    svg.push_str("</g></svg>");
    svg
}

fn view_rect(board: &Board) -> Rect {
    if let Some(outline) = board.outline {
        return outline;
    }
    if let Some(content) = board.content_bounds() {
        return content.expand(pcb_core::Length(FALLBACK_MARGIN_NM));
    }
    // Empty board: a 50 × 50 mm placeholder so the canvas has something
    // to show.
    Rect::from_corners(
        pcb_core::Point::new(pcb_core::Length(0), pcb_core::Length(0)),
        pcb_core::Point::new(
            pcb_core::Length(50_000_000),
            pcb_core::Length(50_000_000),
        ),
    )
}

fn write_footprint(svg: &mut String, fp: &Footprint) {
    for pad in &fp.pads {
        write_pad(svg, fp, pad);
    }
    let label_x = fp.position.x.to_mm();
    let label_y = fp.position.y.to_mm();
    let _ = write!(
        svg,
        r##"<g transform="translate({x:.3},{y:.3}) scale(1,-1)"><text x="0" y="0" text-anchor="middle" dominant-baseline="middle" font-family="ui-monospace, monospace" font-size="0.8" fill="#e6edf3">{r}</text></g>"##,
        x = label_x,
        y = label_y,
        r = escape(&fp.reference),
    );
}

fn write_pad(svg: &mut String, fp: &Footprint, pad: &Pad) {
    let cx = (fp.position.x + pad.offset.x).to_mm();
    let cy = (fp.position.y + pad.offset.y).to_mm();
    let w = pad.size.0.to_mm();
    let h = pad.size.1.to_mm();
    let fill = match pad.layer {
        CopperLayer::Top => "#c97a2b",
        CopperLayer::Bottom => "#2b6cc9",
    };
    let _ = write!(
        svg,
        r##"<rect x="{x:.3}" y="{y:.3}" width="{w:.3}" height="{h:.3}" fill="{fill}"/>"##,
        x = cx - w / 2.0,
        y = cy - h / 2.0,
    );
}

fn write_trace(svg: &mut String, trace: &Trace) {
    let stroke = match trace.layer {
        CopperLayer::Top => "#c97a2b",
        CopperLayer::Bottom => "#2b6cc9",
    };
    let _ = write!(
        svg,
        r##"<line x1="{x1:.3}" y1="{y1:.3}" x2="{x2:.3}" y2="{y2:.3}" stroke="{stroke}" stroke-width="{w:.3}" stroke-linecap="round"/>"##,
        x1 = trace.start.x.to_mm(),
        y1 = trace.start.y.to_mm(),
        x2 = trace.end.x.to_mm(),
        y2 = trace.end.y.to_mm(),
        w = trace.width.to_mm(),
    );
}

fn write_via(svg: &mut String, via: &Via) {
    let cx = via.position.x.to_mm();
    let cy = via.position.y.to_mm();
    let outer = via.diameter.to_mm() / 2.0;
    let inner = via.drill.to_mm() / 2.0;
    let _ = write!(
        svg,
        r##"<circle cx="{cx:.3}" cy="{cy:.3}" r="{outer:.3}" fill="#7d8590"/><circle cx="{cx:.3}" cy="{cy:.3}" r="{inner:.3}" fill="#0e1116"/>"##,
    );
}

fn write_rect_stroke(svg: &mut String, rect: Rect, stroke: &str, width_mm: f64) {
    let _ = write!(
        svg,
        r##"<rect x="{x:.3}" y="{y:.3}" width="{w:.3}" height="{h:.3}" fill="none" stroke="{stroke}" stroke-width="{sw:.3}"/>"##,
        x = rect.min.x.to_mm(),
        y = rect.min.y.to_mm(),
        w = rect.width().to_mm(),
        h = rect.height().to_mm(),
        sw = width_mm,
    );
}

fn escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}
