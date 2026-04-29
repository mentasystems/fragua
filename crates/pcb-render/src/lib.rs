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

    // Millimetre grid: drawn under everything so the agent can read
    // coordinates straight off the board.
    write_mm_grid(&mut svg, view);

    if let Some(outline) = board.outline {
        write_rect_stroke(&mut svg, outline, "#d6905b", 0.4);
        write_outline_dimensions(&mut svg, outline);
        write_outline_handles(&mut svg, outline);
    }

    // Origin marker so (0,0) is always identifiable.
    write_origin_marker(&mut svg);

    // Bottom traces first so top traces visually win at crossings.
    for trace in board.traces.iter().filter(|t| t.layer == CopperLayer::Bottom) {
        write_trace(&mut svg, trace);
    }
    // Ratsnest BELOW footprints so the labels stay readable.
    write_ratsnest(&mut svg, board);
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

/// Subtle millimetre grid: a faint line every mm, a brighter line every
/// 5 mm with a numeric label. The labels live in flipped-Y space so the
/// outer scale(1,-1) doesn't mirror them.
fn write_mm_grid(svg: &mut String, view: Rect) {
    let vx = view.min.x.to_mm();
    let vy = view.min.y.to_mm();
    let vw = view.width().to_mm();
    let vh = view.height().to_mm();
    // Skip the grid for very wide views so we don't blow up the SVG.
    if vw > 400.0 || vh > 400.0 {
        return;
    }
    let _ = write!(
        svg,
        r##"<g pointer-events="none" stroke="#1a1f27" stroke-width="0.03" fill="none">"##
    );
    let mut x = (vx).floor();
    while x <= vx + vw {
        let major = (x.round() as i32) % 5 == 0;
        let stroke = if major { "#222a35" } else { "#161b22" };
        let _ = write!(
            svg,
            r##"<line x1="{x:.3}" y1="{y1:.3}" x2="{x:.3}" y2="{y2:.3}" stroke="{stroke}"/>"##,
            y1 = vy,
            y2 = vy + vh,
        );
        x += 1.0;
    }
    let mut y = (vy).floor();
    while y <= vy + vh {
        let major = (y.round() as i32) % 5 == 0;
        let stroke = if major { "#222a35" } else { "#161b22" };
        let _ = write!(
            svg,
            r##"<line x1="{x1:.3}" y1="{y:.3}" x2="{x2:.3}" y2="{y:.3}" stroke="{stroke}"/>"##,
            x1 = vx,
            x2 = vx + vw,
        );
        y += 1.0;
    }
    svg.push_str("</g>");
    // Major gridline labels along the bottom and left edges.
    let mut x = (vx / 5.0).ceil() * 5.0;
    while x <= vx + vw {
        let _ = write!(
            svg,
            r##"<g transform="translate({x:.3},{y:.3}) scale(1,-1)"><text x="0" y="0" font-family="ui-monospace, monospace" font-size="0.9" fill="#3a4452" pointer-events="none">{lab}</text></g>"##,
            y = vy + 0.4,
            lab = x as i32,
        );
        x += 5.0;
    }
    let mut y = (vy / 5.0).ceil() * 5.0;
    while y <= vy + vh {
        let _ = write!(
            svg,
            r##"<g transform="translate({x:.3},{y:.3}) scale(1,-1)"><text x="0" y="0" font-family="ui-monospace, monospace" font-size="0.9" fill="#3a4452" pointer-events="none">{lab}</text></g>"##,
            x = vx + 0.3,
            lab = y as i32,
        );
        y += 5.0;
    }
}

/// Crosshair + "0,0" label at the world origin so the agent can
/// reorient quickly.
fn write_origin_marker(svg: &mut String) {
    let _ = write!(
        svg,
        r##"<g pointer-events="none" stroke="#d6905b" stroke-width="0.08" opacity="0.6"><line x1="-1.5" y1="0" x2="1.5" y2="0"/><line x1="0" y1="-1.5" x2="0" y2="1.5"/></g><g transform="translate(0.4,0.4) scale(1,-1)"><text x="0" y="0" font-family="ui-monospace, monospace" font-size="0.9" fill="#d6905b" opacity="0.7" pointer-events="none">0,0</text></g>"##,
    );
}

/// Width × height labels around the board outline so the agent doesn't
/// have to compute it from min/max corners.
fn write_outline_dimensions(svg: &mut String, outline: Rect) {
    let w = outline.width().to_mm();
    let h = outline.height().to_mm();
    let cx = (outline.min.x.to_mm() + outline.max.x.to_mm()) / 2.0;
    let cy = (outline.min.y.to_mm() + outline.max.y.to_mm()) / 2.0;
    // Width label hovering above the top edge.
    let _ = write!(
        svg,
        r##"<g transform="translate({cx:.3},{y:.3}) scale(1,-1)"><text x="0" y="0" text-anchor="middle" font-family="ui-monospace, monospace" font-size="1.4" fill="#d6905b" pointer-events="none">{w:.1} mm</text></g>"##,
        y = outline.max.y.to_mm() + 1.8,
    );
    // Height label to the left, rotated 90°.
    let _ = write!(
        svg,
        r##"<g transform="translate({x:.3},{cy:.3}) scale(1,-1) rotate(-90)"><text x="0" y="0" text-anchor="middle" font-family="ui-monospace, monospace" font-size="1.4" fill="#d6905b" pointer-events="none">{h:.1} mm</text></g>"##,
        x = outline.min.x.to_mm() - 1.8,
    );
}

fn view_rect(board: &Board) -> Rect {
    if let Some(outline) = board.outline {
        // Pad the viewBox by 10% so the outline stroke doesn't get
        // clipped at the canvas edge and the human has breathing room
        // around the board.
        let pad_x = (outline.max.x - outline.min.x) / 10;
        let pad_y = (outline.max.y - outline.min.y) / 10;
        return Rect {
            min: outline.min.translate(-pad_x, -pad_y),
            max: outline.max.translate(pad_x, pad_y),
        };
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
    // Wrap the whole footprint in a transform group; pads, body, and
    // label live in footprint-local coordinates so the rotation is
    // applied uniformly. The drag hitbox is a transparent body rect
    // sitting under the pads — click anywhere inside the body and you
    // can drag it.
    let fx = fp.position.x.to_mm();
    let fy = fp.position.y.to_mm();
    let _ = write!(
        svg,
        r##"<g data-board-ref="{r}" transform="translate({x:.3},{y:.3}) rotate({deg:.2})" style="cursor:grab">"##,
        r = escape(&fp.reference),
        x = fx,
        y = fy,
        deg = fp.rotation,
    );

    // Body rect: bbox of pads expanded by a small margin. Stroke is
    // the silkscreen-ish boundary; fill is a transparent hitbox so the
    // pointer hits anywhere inside the body.
    if let Some(body) = body_rect(fp) {
        let bx = body.min.x.to_mm();
        let by = body.min.y.to_mm();
        let bw = (body.max.x - body.min.x).to_mm();
        let bh = (body.max.y - body.min.y).to_mm();
        let _ = write!(
            svg,
            r##"<rect x="{bx:.3}" y="{by:.3}" width="{bw:.3}" height="{bh:.3}" fill="rgba(255,255,255,0.02)" stroke="#8b949e" stroke-width="0.1"/>"##,
        );
    }
    for pad in &fp.pads {
        write_pad(svg, pad);
    }
    // Reference + value label, plus pad numbers when the pad is large
    // enough to fit one. All labels live inside an inner scale(1,-1) so
    // the outer Y-flip doesn't mirror them, and use pointer-events:none
    // so clicks fall through to the body hitbox.
    let body = body_rect(fp);
    let label_y = body
        .map(|r| r.min.y.to_mm() - 0.6)
        .unwrap_or(-0.6);
    let _ = write!(
        svg,
        r##"<g transform="scale(1,-1)" pointer-events="none">"##,
    );
    // REF on top of the body.
    let _ = write!(
        svg,
        r##"<text x="0" y="{y:.3}" text-anchor="middle" font-family="ui-monospace, monospace" font-size="0.9" fill="#e6edf3">{r}</text>"##,
        y = -label_y,
        r = escape(&fp.reference),
    );
    // Value below the body, slimmer.
    if !fp.value.is_empty() {
        let val_y = body
            .map(|r| r.max.y.to_mm() + 1.2)
            .unwrap_or(1.2);
        let _ = write!(
            svg,
            r##"<text x="0" y="{y:.3}" text-anchor="middle" font-family="ui-monospace, monospace" font-size="0.7" fill="#8b949e">{v}</text>"##,
            y = -val_y,
            v = escape(&fp.value),
        );
    }
    // Pad numbers — only when the pad is at least 0.8 mm in both axes
    // so the digits don't bleed outside the copper.
    for pad in &fp.pads {
        let pw = pad.size.0.to_mm();
        let ph = pad.size.1.to_mm();
        if pw < 0.8 || ph < 0.8 {
            continue;
        }
        let size = (pw.min(ph) * 0.55).clamp(0.35, 1.2);
        let _ = write!(
            svg,
            r##"<text x="{x:.3}" y="{y:.3}" text-anchor="middle" dominant-baseline="middle" font-family="ui-monospace, monospace" font-size="{sz:.2}" fill="#0e1116">{n}</text>"##,
            x = pad.offset.x.to_mm(),
            y = -pad.offset.y.to_mm(),
            sz = size,
            n = escape(&pad.number),
        );
    }
    svg.push_str("</g></g>");
}

fn body_rect(fp: &Footprint) -> Option<Rect> {
    let mut iter = fp.pads.iter().map(|pad| {
        Rect::from_center(
            pcb_core::Point::new(pad.offset.x, pad.offset.y),
            pad.size.0,
            pad.size.1,
        )
    });
    let first = iter.next()?;
    Some(iter.fold(first, Rect::union).expand(pcb_core::Length::from_mm(0.4)))
}

fn write_pad(svg: &mut String, pad: &Pad) {
    let cx = pad.offset.x.to_mm();
    let cy = pad.offset.y.to_mm();
    let w = pad.size.0.to_mm();
    let h = pad.size.1.to_mm();
    // Pads use saturated copper tones; traces use a clearly different
    // hue (gold for top, cyan for bottom) so the agent and the human
    // can tell at a glance whether a piece of copper is a landing pad
    // or a routed segment.
    let fill = match pad.layer {
        CopperLayer::Top => "#c97a2b",
        CopperLayer::Bottom => "#2b6cc9",
    };
    let _ = write!(
        svg,
        r##"<rect x="{x:.3}" y="{y:.3}" width="{w:.3}" height="{h:.3}" fill="{fill}" pointer-events="none"/>"##,
        x = cx - w / 2.0,
        y = cy - h / 2.0,
    );
}

/// Draw the ratsnest: thin lines between every pair of pads on the
/// same net, where no trace already routes that pair. Suppressed for
/// nets that have at least one trace (assume the router is on it).
fn write_ratsnest(svg: &mut String, board: &Board) {
    use std::collections::HashMap;
    // Collect pads-per-net with their absolute board positions, and
    // remember which nets already have at least one trace.
    let mut net_pads: HashMap<&str, Vec<(f64, f64)>> = HashMap::new();
    for fp in board.footprints_in_order() {
        for pad in &fp.pads {
            if let Some(net) = pad.net.as_deref() {
                let center = fp.pad_world_center(pad);
                net_pads
                    .entry(net)
                    .or_default()
                    .push((center.x.to_mm(), center.y.to_mm()));
            }
        }
    }
    let mut routed: std::collections::HashSet<&str> = std::collections::HashSet::new();
    for trace in &board.traces {
        routed.insert(trace.net.as_str());
    }
    for via in &board.vias {
        routed.insert(via.net.as_str());
    }
    for (net, pads) in &net_pads {
        if routed.contains(net) || pads.len() < 2 {
            continue;
        }
        for i in 0..pads.len() {
            for j in (i + 1)..pads.len() {
                let (x1, y1) = pads[i];
                let (x2, y2) = pads[j];
                let _ = write!(
                    svg,
                    r##"<line x1="{x1:.3}" y1="{y1:.3}" x2="{x2:.3}" y2="{y2:.3}" stroke="#3fb950" stroke-width="0.05" stroke-opacity="0.6" pointer-events="none"/>"##,
                );
            }
        }
    }
}

fn write_trace(svg: &mut String, trace: &Trace) {
    // Traces use a different hue than pads on the same layer so the
    // eye can separate "where copper lands on a component" from "where
    // copper carries a signal". Top: gold against the orange pads;
    // bottom: cyan against the blue pads.
    let stroke = match trace.layer {
        CopperLayer::Top => "#ffd166",
        CopperLayer::Bottom => "#4ec9ff",
    };
    let layer_label = match trace.layer {
        CopperLayer::Top => "top",
        CopperLayer::Bottom => "bottom",
    };
    let _ = write!(
        svg,
        r##"<line data-trace-id="{id}" pathLength="1" x1="{x1:.3}" y1="{y1:.3}" x2="{x2:.3}" y2="{y2:.3}" stroke="{stroke}" stroke-width="{w:.3}" stroke-linecap="round"><title>{net} ({layer_label})</title></line>"##,
        id = trace.id.0,
        x1 = trace.start.x.to_mm(),
        y1 = trace.start.y.to_mm(),
        x2 = trace.end.x.to_mm(),
        y2 = trace.end.y.to_mm(),
        w = trace.width.to_mm(),
        net = escape(&trace.net),
    );
}

fn write_via(svg: &mut String, via: &Via) {
    let cx = via.position.x.to_mm();
    let cy = via.position.y.to_mm();
    let outer = via.diameter.to_mm() / 2.0;
    let inner = via.drill.to_mm() / 2.0;
    let _ = write!(
        svg,
        r##"<g data-via-id="{id}"><title>{net} (via)</title><circle cx="{cx:.3}" cy="{cy:.3}" r="{outer:.3}" fill="#7d8590"/><circle cx="{cx:.3}" cy="{cy:.3}" r="{inner:.3}" fill="#0e1116"/></g>"##,
        id = via.id.0,
        net = escape(&via.net),
    );
}

/// Draw four resize handles on the outline, one per side. Each handle
/// is tagged with `data-resize-edge` so the frontend can hit-test it
/// in pointerdown.
fn write_outline_handles(svg: &mut String, outline: Rect) {
    let cx = (outline.min.x.to_mm() + outline.max.x.to_mm()) / 2.0;
    let cy = (outline.min.y.to_mm() + outline.max.y.to_mm()) / 2.0;
    let w = (outline.max.x - outline.min.x).to_mm();
    let h = (outline.max.y - outline.min.y).to_mm();
    // Handle is a small square sitting on the edge midpoint.
    let s = (w.min(h) * 0.04).clamp(0.8, 3.0);
    let handles = [
        ("top",    cx, outline.max.y.to_mm()),
        ("bottom", cx, outline.min.y.to_mm()),
        ("right",  outline.max.x.to_mm(), cy),
        ("left",   outline.min.x.to_mm(), cy),
    ];
    for (edge, hx, hy) in handles {
        let cursor = match edge {
            "top" | "bottom" => "ns-resize",
            _ => "ew-resize",
        };
        let _ = write!(
            svg,
            r##"<rect data-resize-edge="{edge}" x="{x:.3}" y="{y:.3}" width="{s:.3}" height="{s:.3}" fill="#d6905b" stroke="#0e1116" stroke-width="0.1" style="cursor:{cursor}"/>"##,
            x = hx - s / 2.0,
            y = hy - s / 2.0,
        );
    }
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
