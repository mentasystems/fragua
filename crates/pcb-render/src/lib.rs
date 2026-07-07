//! `pcb-render` — board / schematic → SVG.
//!
//! SVG is the primary output: the frontend can style and animate it, and
//! it is trivial for the agent to attach as visual context. PNG comes
//! later if/when we hit perf or fidelity needs.

pub mod png;
pub mod schematic;
pub use png::{
    render_board_png, render_board_png_with_margins, render_library_entry_png,
    render_schematic_png, svg_to_png, DEFAULT_PNG_WIDTH, MAX_PNG_DIMENSION,
};
pub use schematic::render_schematic_svg;

use std::fmt::Write;

use std::collections::HashMap;

use pcb_core::{
    hershey, silk_clip, Board, Footprint, FootprintSilk, Layer, Length, Pad, PlacementMargin,
    Point, Rect, SilkAnchor, SilkLayer, SilkText, Trace, Via,
};

/// Library-key → per-side placement margin. The renderer takes this
/// instead of a full `Library` so callers without library access (tests,
/// the schematic preview pipeline) can pass an empty map and still get a
/// pad-only render, while the Tauri host and the script API pass a real
/// lookup so the user's review-pane margins appear as a body outline on
/// the board.
pub type PlacementMarginMap = HashMap<String, PlacementMargin>;

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
    render_svg_with_margins(board, &PlacementMarginMap::default())
}

/// Same as `render_svg` but consults `margins` (library-key → per-side
/// placement margin in mm) for every footprint and draws a thin grey
/// body-outline rectangle around the inflated pad bbox. The outline
/// reflects the user-configured physical body extent recorded in the
/// component library — a screw terminal whose plastic shroud overhangs
/// the pads by 2 mm shows that overhang on the board even though the
/// pads themselves are smaller. Margin-less or unknown-key footprints
/// render identically to `render_svg`.
#[must_use]
pub fn render_svg_with_margins(board: &Board, margins: &PlacementMarginMap) -> String {
    let view = view_rect(board);
    let view_w_mm = view.width().to_mm();
    let view_h_mm = view.height().to_mm();
    let view_x_mm = view.min.x.to_mm();
    let view_y_mm = view.min.y.to_mm();

    let mut svg = String::with_capacity(2048);
    let _ = write!(
        svg,
        r#"<svg xmlns="http://www.w3.org/2000/svg" viewBox="{x:.3} {y:.3} {w:.3} {h:.3}" width="100%" height="100%">"#,
        x = view_x_mm,
        y = -(view_y_mm + view_h_mm),
        w = view_w_mm,
        h = view_h_mm,
    );
    svg.push_str(r#"<g transform="scale(1,-1)">"#);

    // Background.
    let _ = write!(
        svg,
        r##"<rect x="{view_x_mm:.3}" y="{view_y_mm:.3}" width="{view_w_mm:.3}" height="{view_h_mm:.3}" fill="#0e1116"/>"##,
    );

    // Millimetre grid: drawn under everything so the agent can read
    // coordinates straight off the board.
    write_mm_grid(&mut svg, view);

    if let Some(outline) = board.outline {
        // Substrate first: paint the FR4-coloured board area inside
        // the outline. Anywhere the pour does NOT cover (cutouts,
        // edge clearance) shows this brown so the eye reads it as
        // "bare substrate, no copper".
        let radius_mm = board.outline_corner_radius.to_mm();
        write_substrate_fill(&mut svg, outline, radius_mm);
        // Pours sit on the substrate. Each pour is the outline
        // (inset by the edge clearance) MINUS the clearance keepouts
        // around every foreign-net pad / trace / via — same geometry
        // the Gerber writer emits.
        for pour in &board.pours {
            write_pour_polygon(&mut svg, board, pour, outline);
        }
        write_rect_stroke(&mut svg, outline, "#d6905b", 0.4, radius_mm);
        write_outline_dimensions(&mut svg, outline);
    }

    // Origin marker so (0,0) is always identifiable.
    write_origin_marker(&mut svg);

    // Bottom traces first so top traces visually win at crossings.
    // Orphan stubs (neither endpoint touches a pad/via/other-trace
    // of the same net) are skipped so a half-finished route does
    // not pollute the visual.
    let orphans = board.orphan_trace_ids();
    let orphan_vias = board.orphan_via_ids();
    // Multi-layer rendering: walk the stackup BOTTOM → TOP so traces
    // on higher (smaller-index) layers visually win at crossings. On
    // a 2-layer board this collapses to "bottom then top" — the
    // pre-Phase-4 behaviour.
    let stackup_count = board.stackup.layer_count();
    for idx in (1..stackup_count).rev() {
        for trace in board
            .traces
            .iter()
            .filter(|t| t.layer.index == idx && !orphans.contains(&t.id))
        {
            write_trace(&mut svg, trace);
        }
    }
    // Ratsnest BELOW footprints so the labels stay readable.
    write_ratsnest(&mut svg, board);
    for fp in board.footprints_in_order() {
        let margin = footprint_margin(fp, margins);
        write_footprint(&mut svg, fp, &board.pours, margin);
    }
    // Top traces drawn last so they sit visually on top.
    for trace in board
        .traces
        .iter()
        .filter(|t| t.layer.is_top() && !orphans.contains(&t.id))
    {
        write_trace(&mut svg, trace);
    }
    for via in board.vias.iter().filter(|v| !orphan_vias.contains(&v.id)) {
        write_via(&mut svg, via);
    }

    // Silkscreen sits on top of copper / pads (the fab applies it last
    // in the stackup) but BELOW any DRC overlay the frontend may add
    // on its own. Top silk is full opacity; bottom silk is dimmed —
    // mirroring how bottom copper is dimmed cyan vs orange top.
    write_silk_layer(&mut svg, board, SilkLayer::Top);
    write_silk_layer(&mut svg, board, SilkLayer::Bottom);

    // Keep-outs sit on top so they're always visible — magenta
    // hatched fill with a solid outline. The frontend may toggle
    // visibility, but the default render shows them.
    write_keepouts(&mut svg, board);

    svg.push_str("</g></svg>");
    svg
}

/// Silkscreen colour. Off-white because silkscreen on a real PCB is a
/// thin epoxy ink — pure white reads as too aggressive against the
/// dark substrate fill we use for the canvas.
const SILK_COLOR: &str = "#e6edf3";
/// Bottom-side dim factor — same idea as the bottom-copper cyan being
/// dimmer than the top-copper orange.
const SILK_BOTTOM_OPACITY: f64 = 0.55;

/// Emit every silk stroke for the given side: board-level lines/texts
/// followed by every footprint's silk (transformed to world coords).
/// Footprints with no explicit silk get a default `{REF}` label
/// synthesised here — keeping the hand-tuned visual that previously
/// lived as an SVG `<text>` inside `write_footprint`, but now via the
/// Hershey font so it ships into the silk Gerber as well.
fn write_silk_layer(svg: &mut String, board: &Board, side: SilkLayer) {
    let opacity = match side {
        SilkLayer::Top => 1.0,
        SilkLayer::Bottom => SILK_BOTTOM_OPACITY,
    };
    let _ = write!(
        svg,
        r#"<g pointer-events="none" stroke="{SILK_COLOR}" stroke-linecap="round" fill="none" opacity="{opacity}">"#,
    );
    for line in board.silk_lines.iter().filter(|l| l.layer == side) {
        write_silk_segment(svg, line.start, line.end, line.width.to_mm());
    }
    for txt in board.silk_texts.iter().filter(|t| t.layer == side) {
        write_silk_text(svg, txt, /*owner_pads=*/ &[]);
    }
    for fp in board.footprints_in_order() {
        write_footprint_silk(svg, fp, side, board.outline);
    }
    svg.push_str("</g>");
}

/// Walk a footprint's silk (or the synthesised default) and draw
/// every stroke that targets `side`. Silk segments whose midpoint
/// falls inside any same-footprint pad bbox are skipped — fab houses
/// mask silk over solder pads, and the rendering should reflect that.
/// `outline` (when set) is used to relocate silk text whose nominal
/// position would land outside the board, so labels stay on copper.
fn write_footprint_silk(svg: &mut String, fp: &Footprint, side: SilkLayer, outline: Option<Rect>) {
    // World-space pad rects for the pad-overlap suppression check.
    let pad_rects: Vec<Rect> = fp
        .pads
        .iter()
        .map(|pad| {
            let c = fp.pad_world_center(pad);
            let (pw, ph) = fp.pad_world_size(pad);
            Rect::from_center(c, pw, ph)
        })
        .collect();

    if fp.silk.is_empty() {
        // Default: a single `{REF}` label above the footprint body
        // bbox on TOP silk. Bottom-mounted footprints get the label on
        // bottom silk so it stays readable from the right side.
        let default_side = if fp.layer.is_top() {
            SilkLayer::Top
        } else {
            SilkLayer::Bottom
        };
        if default_side != side {
            return;
        }
        let primary = if fp.key.is_empty() {
            fp.reference.as_str()
        } else {
            fp.key.as_str()
        };
        if primary.is_empty() {
            return;
        }
        if let Some(body) = body_rect(fp) {
            // Anchor sits 0.6 mm above the body bbox — same offset the
            // pre-silk SVG `<text>` used, so the visual change is small.
            let local_anchor = Point::new(
                pcb_core::Length::ZERO,
                body.max.y + pcb_core::Length::from_mm(0.6),
            );
            let world = fp.local_to_world(local_anchor);
            let size = pcb_core::Length::from_mm(0.9);
            // Body in world coords for the relocate fallback.
            let world_body = world_body_rect(fp);
            let safe = safe_silk_text_pos(
                world,
                primary,
                size,
                fp.rotation,
                SilkAnchor::Middle,
                world_body,
                outline,
            );
            let text = SilkText {
                layer: default_side,
                position: safe,
                text: primary.to_string(),
                size,
                rotation: fp.rotation,
                anchor: SilkAnchor::Middle,
                width: SilkText::default_stroke(size),
            };
            write_silk_text(svg, &text, &pad_rects);
        }
        return;
    }

    for item in &fp.silk {
        match *item {
            FootprintSilk::Line {
                layer,
                start,
                end,
                width,
            } => {
                if layer != side {
                    continue;
                }
                let s = fp.local_to_world(start);
                let e = fp.local_to_world(end);
                for (a, b) in silk_clip::clip_segment(s, e, &pad_rects) {
                    write_silk_segment(svg, a, b, width.to_mm());
                }
            }
            FootprintSilk::Text {
                layer,
                position,
                ref text,
                size,
                rotation,
                anchor,
                width,
            } => {
                if layer != side {
                    continue;
                }
                let world = fp.local_to_world(position);
                let resolved = fp.resolve_silk_text(text);
                let total_rotation = rotation + fp.rotation;
                // Library-authored silk text can spill off the board
                // when a footprint sits near the edge — relocate to a
                // body-relative fallback if so.
                let world_body = world_body_rect(fp);
                let safe = safe_silk_text_pos(
                    world,
                    &resolved,
                    size,
                    total_rotation,
                    anchor,
                    world_body,
                    outline,
                );
                let st = SilkText {
                    layer,
                    position: safe,
                    text: resolved,
                    size,
                    rotation: total_rotation,
                    anchor,
                    width,
                };
                write_silk_text(svg, &st, &pad_rects);
            }
        }
    }
}

fn write_silk_segment(svg: &mut String, a: Point, b: Point, width_mm: f64) {
    let _ = write!(
        svg,
        r#"<line x1="{x1:.3}" y1="{y1:.3}" x2="{x2:.3}" y2="{y2:.3}" stroke-width="{w:.3}"/>"#,
        x1 = a.x.to_mm(),
        y1 = a.y.to_mm(),
        x2 = b.x.to_mm(),
        y2 = b.y.to_mm(),
        w = width_mm,
    );
}

/// Approximate AABB the rendered silk text would occupy in world
/// coords. Used to detect labels that would spill off the board and
/// to find a safer anchor for them. The Hershey font is fixed-pitch:
/// every character takes `ADVANCE_UNITS / CAP_HEIGHT_UNITS` ≈ 0.75
/// of `size_mm` in horizontal advance.
fn silk_text_bbox(
    text: &str,
    pos: Point,
    size: Length,
    rotation_deg: f32,
    anchor: SilkAnchor,
) -> Option<Rect> {
    if text.is_empty() {
        return None;
    }
    let chars = text.chars().count() as f64;
    let w_mm = chars * 0.75 * size.to_mm();
    let h_mm = size.to_mm();
    let (x0, x1) = match anchor {
        SilkAnchor::Start => (0.0, w_mm),
        SilkAnchor::Middle => (-w_mm / 2.0, w_mm / 2.0),
        SilkAnchor::End => (-w_mm, 0.0),
    };
    // Baseline at y=0; cap top at y=h_mm.
    let local = [(x0, 0.0), (x1, 0.0), (x1, h_mm), (x0, h_mm)];
    let theta = f64::from(rotation_deg).to_radians();
    let cos = theta.cos();
    let sin = theta.sin();
    let px = pos.x.to_mm();
    let py = pos.y.to_mm();
    let mut min_x = f64::INFINITY;
    let mut min_y = f64::INFINITY;
    let mut max_x = f64::NEG_INFINITY;
    let mut max_y = f64::NEG_INFINITY;
    for (lx, ly) in local {
        let wx = px + lx * cos - ly * sin;
        let wy = py + lx * sin + ly * cos;
        min_x = min_x.min(wx);
        min_y = min_y.min(wy);
        max_x = max_x.max(wx);
        max_y = max_y.max(wy);
    }
    Some(Rect::from_corners(
        Point::new(
            pcb_core::Length::from_mm(min_x),
            pcb_core::Length::from_mm(min_y),
        ),
        Point::new(
            pcb_core::Length::from_mm(max_x),
            pcb_core::Length::from_mm(max_y),
        ),
    ))
}

/// True if `inner` sits fully inside `outer` (touching counts).
fn rect_inside(inner: Rect, outer: Rect) -> bool {
    inner.min.x.0 >= outer.min.x.0
        && inner.min.y.0 >= outer.min.y.0
        && inner.max.x.0 <= outer.max.x.0
        && inner.max.y.0 <= outer.max.y.0
}

/// Pick the best position for a silk label so its bbox stays inside
/// the board outline. Tries the requested `pos` first; if that bbox
/// would clip the outline, walks a small set of alternatives (above
/// the body, below it) and returns the first one that fits. Falls
/// back to the original `pos` when nothing fits — the renderer
/// always emits the text, even if part of it ends up off-board.
fn safe_silk_text_pos(
    pos: Point,
    text: &str,
    size: Length,
    rotation_deg: f32,
    anchor: SilkAnchor,
    body: Option<Rect>,
    outline: Option<Rect>,
) -> Point {
    let Some(outline) = outline else { return pos };
    let Some(body) = body else { return pos };
    let pad = pcb_core::Length::from_mm(0.6);
    if let Some(bb) = silk_text_bbox(text, pos, size, rotation_deg, anchor) {
        if rect_inside(bb, outline) {
            return pos;
        }
    }
    // Candidate anchor points (in world coords) ordered by visual
    // preference: above body, below body, then horizontally-offset
    // alternatives. The first whose rendered bbox fits wins.
    let bx_mid = pcb_core::Length(i64::midpoint(body.min.x.0, body.max.x.0));
    let by_mid = pcb_core::Length(i64::midpoint(body.min.y.0, body.max.y.0));
    let candidates = [
        Point::new(bx_mid, body.max.y + pad),
        Point::new(bx_mid, body.min.y - pad - size),
        Point::new(body.max.x + pad, by_mid),
        Point::new(body.min.x - pad, by_mid),
    ];
    for cand in candidates {
        let Some(bb) = silk_text_bbox(text, cand, size, rotation_deg, anchor) else {
            continue;
        };
        if rect_inside(bb, outline) {
            return cand;
        }
    }
    pos
}

fn write_silk_text(svg: &mut String, txt: &SilkText, suppress_in: &[Rect]) {
    let segments =
        hershey::text_segments(&txt.text, txt.position, txt.size, txt.rotation, txt.anchor);
    let w = txt.width.to_mm();
    for (a, b) in segments {
        for (s, e) in silk_clip::clip_segment(a, b, suppress_in) {
            write_silk_segment(svg, s, e, w);
        }
    }
}

/// Hatched magenta polygons for every keepout, drawn on top of
/// copper but under DRC overlays. Each keepout uses a single SVG
/// `<pattern>` for its hatching; the polygon outline is solid.
fn write_keepouts(svg: &mut String, board: &Board) {
    if board.keepouts.is_empty() {
        return;
    }
    // One shared hatching pattern — same stroke for every keepout
    // (the polygon itself communicates the boundary; the hatch is
    // visual texture).
    svg.push_str(
        r##"<defs><pattern id="keepout-hatch" patternUnits="userSpaceOnUse" width="1.2" height="1.2" patternTransform="rotate(45)"><line x1="0" y1="0" x2="0" y2="1.2" stroke="#ff2bd6" stroke-width="0.25"/></pattern></defs>"##,
    );
    for kp in &board.keepouts {
        if kp.polygon.len() < 3 {
            continue;
        }
        let pts: Vec<String> = kp
            .polygon
            .iter()
            .map(|p| format!("{:.3},{:.3}", p.x.to_mm(), p.y.to_mm()))
            .collect();
        let _ = write!(
            svg,
            r##"<polygon data-keepout="{label}" points="{pts}" fill="url(#keepout-hatch)" fill-opacity="0.5" stroke="#ff2bd6" stroke-width="0.18" pointer-events="none"/>"##,
            label = escape(&kp.label),
            pts = pts.join(" "),
        );
    }
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
            r#"<line x1="{x:.3}" y1="{y1:.3}" x2="{x:.3}" y2="{y2:.3}" stroke="{stroke}"/>"#,
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
            r#"<line x1="{x1:.3}" y1="{y:.3}" x2="{x2:.3}" y2="{y:.3}" stroke="{stroke}"/>"#,
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
    let cx = f64::midpoint(outline.min.x.to_mm(), outline.max.x.to_mm());
    let cy = f64::midpoint(outline.min.y.to_mm(), outline.max.y.to_mm());
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
        pcb_core::Point::new(pcb_core::Length(50_000_000), pcb_core::Length(50_000_000)),
    )
}

/// Look up the placement margin for `fp` in `margins`, keyed by the
/// footprint's library key. Returns `PlacementMargin::default()` for
/// keyless footprints or unknown keys so callers can always pass a
/// margin into render helpers without branching.
fn footprint_margin(fp: &Footprint, margins: &PlacementMarginMap) -> PlacementMargin {
    if fp.key.is_empty() {
        return PlacementMargin::default();
    }
    margins.get(&fp.key).copied().unwrap_or_default()
}

fn write_footprint(
    svg: &mut String,
    fp: &Footprint,
    pours: &[pcb_core::Pour],
    margin: PlacementMargin,
) {
    // Wrap the whole footprint in a transform group so pads, body,
    // and label rotate together with the footprint. Read-only render:
    // no drag handle, no per-component cursor, no hit-tracking attribute.
    let fx = fp.position.x.to_mm();
    let fy = fp.position.y.to_mm();
    // `data-board-ref` (the schematic reference) and `data-library-key`
    // (the library entry it was spawned from) are both kept so the
    // frontend can spawn-flash, hit-test for the info modal, and
    // cross-reference the library panel.
    let _ = write!(
        svg,
        r#"<g data-board-ref="{r}" data-library-key="{k}" transform="translate({x:.3},{y:.3}) rotate({deg:.2})">"#,
        r = escape(&fp.reference),
        k = escape(&fp.key),
        x = fx,
        y = fy,
        deg = fp.rotation,
    );

    // Body rect: bbox of pads expanded by a small margin. Fill is an
    // almost-fully-transparent white so the whole body acts as a
    // click hit-target for the info modal — but visually it stays
    // empty so the pads + traces beneath are not tinted.
    if let Some(body) = body_rect(fp) {
        let bx = body.min.x.to_mm();
        let by = body.min.y.to_mm();
        let bw = (body.max.x - body.min.x).to_mm();
        let bh = (body.max.y - body.min.y).to_mm();
        let _ = write!(
            svg,
            r##"<rect x="{bx:.3}" y="{by:.3}" width="{bw:.3}" height="{bh:.3}" fill="rgba(255,255,255,0.01)" stroke="#8b949e" stroke-width="0.1" style="cursor:pointer"/>"##,
        );
    }
    // Library-authored body outline: pad bbox inflated by the per-side
    // placement margin in footprint-LOCAL coords (we're already inside
    // the translate+rotate group, so the local margin maps directly to
    // the visual top/right/bottom/left edges the user dialled in via
    // the review pane). Drawn even when only one side is non-zero so
    // an asymmetric overhang (e.g. a screw terminal whose plastic
    // sticks out 2 mm to the right of the pads) is visible. Skipped
    // for zero/negative margins so an unannotated footprint looks
    // identical to the legacy render.
    if !margin.is_zero() {
        if let Some(pad_bbox) = local_pad_bbox(fp) {
            let bx = pad_bbox.min.x.to_mm() - margin.left_mm;
            let by = pad_bbox.min.y.to_mm() - margin.bottom_mm;
            let bw = (pad_bbox.max.x - pad_bbox.min.x).to_mm() + margin.left_mm + margin.right_mm;
            let bh = (pad_bbox.max.y - pad_bbox.min.y).to_mm() + margin.top_mm + margin.bottom_mm;
            let _ = write!(
                svg,
                r##"<rect data-body-outline="{r}" x="{bx:.3}" y="{by:.3}" width="{bw:.3}" height="{bh:.3}" fill="none" stroke="#6e7681" stroke-width="0.08" stroke-dasharray="0.4 0.3" pointer-events="none"/>"##,
                r = escape(&fp.reference),
            );
        }
    }
    for pad in &fp.pads {
        write_pad(svg, pad, pours);
    }
    // Pad number labels and the secondary `ref · value` line. The
    // PRIMARY footprint label moved to silkscreen — see
    // `write_footprint_silk` — so the same string ends up in both the
    // SVG render and the F.SilkS Gerber. The secondary line stays as
    // SVG `<text>` because it is metadata (run-time only) and the
    // Hershey vectorisation hurts readability at the small font size
    // we use for it.
    let _ = write!(svg, r#"<g transform="scale(1,-1)" pointer-events="none">"#,);
    // The "REF · VALUE" caption used to live here as a plain SVG
    // <text> below the body. It overlapped the silk-text labels
    // emitted by the silkscreen pipeline (`{REF}`/`{KEY}` templates)
    // and showed redundant information; the silk pipeline now owns
    // the visible identifier on the board.
    // Pad labels — prefer the human pin NAME (e.g. "VBAT", "MOSI",
    // "GND") and fall back to the bare pad number when no name is
    // set. Skipped on tiny pads so the text does not bleed outside
    // the copper. Dark text on the copper fill — readable on the
    // orange/blue background. The font shrinks for long names so
    // they still fit in the pad bbox.
    let _ = pours;
    for pad in &fp.pads {
        let pw = pad.size.0.to_mm();
        let ph = pad.size.1.to_mm();
        if pw < 0.8 || ph < 0.8 {
            continue;
        }
        let label_text = if pad.name.is_empty() {
            pad.number.as_str()
        } else {
            pad.name.as_str()
        };
        // Heuristic font size: cap at ~half the pad's short side, then
        // shrink linearly with the label length so 4+ chars still fit.
        let chars = label_text.chars().count().max(1) as f64;
        let cap = (pw.min(ph) * 0.55).clamp(0.30, 1.0);
        let by_width = (pw / chars * 1.4).clamp(0.30, 1.0);
        let size = cap.min(by_width);
        let _ = write!(
            svg,
            r##"<text x="{x:.3}" y="{y:.3}" text-anchor="middle" dominant-baseline="middle" font-family="ui-monospace, monospace" font-size="{sz:.2}" fill="#0e1116">{n}</text>"##,
            x = pad.offset.x.to_mm(),
            y = -pad.offset.y.to_mm(),
            sz = size,
            n = escape(label_text),
        );
    }
    svg.push_str("</g></g>");
}

/// Local (pre-rotation, pre-translate) pad bounding box — the raw
/// envelope the placement margin inflates. Returns `None` for a padless
/// footprint.
fn local_pad_bbox(fp: &Footprint) -> Option<Rect> {
    let mut iter = fp.pads.iter().map(|pad| {
        Rect::from_center(
            Point::new(pad.offset.x, pad.offset.y),
            pad.size.0,
            pad.size.1,
        )
    });
    let first = iter.next()?;
    Some(iter.fold(first, Rect::union))
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
    Some(
        iter.fold(first, Rect::union)
            .expand(pcb_core::Length::from_mm(0.4)),
    )
}

/// World-coord bounding box of the footprint body (pad bbox + 0.4 mm
/// margin), suitable for relocating silk labels relative to the
/// placed part. Returns `None` when the footprint has no pads.
fn world_body_rect(fp: &Footprint) -> Option<Rect> {
    let mut iter = fp.pads.iter().map(|pad| {
        let c = fp.pad_world_center(pad);
        let (w, h) = fp.pad_world_size(pad);
        Rect::from_center(c, w, h)
    });
    let first = iter.next()?;
    Some(
        iter.fold(first, Rect::union)
            .expand(pcb_core::Length::from_mm(0.4)),
    )
}

fn write_pad(svg: &mut String, pad: &Pad, pours: &[pcb_core::Pour]) {
    let cx = pad.offset.x.to_mm();
    let cy = pad.offset.y.to_mm();
    let w = pad.size.0.to_mm();
    let h = pad.size.1.to_mm();
    // Pads use saturated copper tones; traces use a clearly different
    // hue (gold for top, cyan for bottom) so the agent and the human
    // can tell at a glance whether a piece of copper is a landing pad
    // or a routed segment.
    // All pads share the layer's copper colour. Whether a pad is
    // electrically continuous with a pour reads off the surrounding
    // shape: pads on the pour's net merge into the orange/olive
    // wash; pads on other nets get a dark clearance ring around
    // them (drawn by `write_pour_polygon` as evenodd cutouts).
    let _ = pours;
    let fill = layer_pad_fill(pad.layer);
    let _ = write!(
        svg,
        r#"<rect x="{x:.3}" y="{y:.3}" width="{w:.3}" height="{h:.3}" fill="{fill}" pointer-events="none"/>"#,
        x = cx - w / 2.0,
        y = cy - h / 2.0,
    );
    // GND highlight: a magenta hatched overlay + bold border so any
    // ground pad on the board reads as such at a glance. The component
    // library review view uses the same convention — it is the single
    // most expensive footprint mistake to ship (a power pin landing on
    // the wrong pad shorts the supply on power-up), so it gets its own
    // colour everywhere.
    if pad.net.as_deref().is_some_and(is_ground_net) {
        let _ = write!(
            svg,
            r##"<rect x="{x:.3}" y="{y:.3}" width="{w:.3}" height="{h:.3}" fill="none" stroke="#ff2bd6" stroke-width="0.15" pointer-events="none"/>"##,
            x = cx - w / 2.0,
            y = cy - h / 2.0,
        );
    }
    if let Some(drill) = pad.drill {
        let r = drill.to_mm() / 2.0;
        let _ = write!(
            svg,
            r##"<circle cx="{cx:.3}" cy="{cy:.3}" r="{r:.3}" fill="#0e1116" pointer-events="none"/>"##,
        );
    }
}

/// True if `name` looks like the universal "this is the ground rail"
/// label. We accept the common spellings (`GND`, `GROUND`, `VSS`, `0V`)
/// case-insensitively because schematic capture across the industry is
/// not consistent. The library review view and the board pad renderer
/// both light up these pads in a distinct colour so a footprint with a
/// mirrored / mis-numbered pinout is obvious before fab.
#[must_use]
pub fn is_ground_net(name: &str) -> bool {
    let n = name.trim();
    n.eq_ignore_ascii_case("GND")
        || n.eq_ignore_ascii_case("GROUND")
        || n.eq_ignore_ascii_case("VSS")
        || n.eq_ignore_ascii_case("0V")
        || n.eq_ignore_ascii_case("AGND")
        || n.eq_ignore_ascii_case("DGND")
        || n.eq_ignore_ascii_case("PGND")
}

/// True if a `LibraryPad`'s number / name reads as a ground pad. Used
/// by the library review view: library entries are not bound to a net
/// yet (those bindings live on placed footprints), so we look at the
/// pad number and the optional pad name instead. Falls back to false
/// for the common numeric pad labels.
#[must_use]
pub fn is_ground_pad_label(number: &str, name: &str) -> bool {
    is_ground_net(number) || is_ground_net(name)
}

/// Render a single library entry (pads + library-authored silk) into
/// a standalone SVG, looking straight down on the part from the TOP.
/// Used by the library review pane and by the create-time confirmation
/// modal: the user can compare the rendered footprint to the photo
/// attachment side by side and catch mirrored pinouts before they
/// reach fab. Ground pads (`GND` / `VSS` / `0V` / …, matched on the
/// pad number or name) are highlighted in magenta so a swap stands
/// out.
///
/// `view_size_mm` defines the side length of the square viewBox; pass
/// a value that comfortably contains the part with a small margin.
#[must_use]
pub fn render_library_entry_svg(entry: &pcb_core::LibraryEntry) -> String {
    // Compute bounding box across pads (in library-local mm, no rotation).
    let (mut min_x, mut min_y) = (f64::INFINITY, f64::INFINITY);
    let (mut max_x, mut max_y) = (f64::NEG_INFINITY, f64::NEG_INFINITY);
    for pad in &entry.pads {
        let hw = pad.w_mm / 2.0;
        let hh = pad.h_mm / 2.0;
        min_x = min_x.min(pad.x_mm - hw);
        min_y = min_y.min(pad.y_mm - hh);
        max_x = max_x.max(pad.x_mm + hw);
        max_y = max_y.max(pad.y_mm + hh);
    }
    if !min_x.is_finite() {
        // No pads — degenerate footprint, render an empty 10 × 10 mm view.
        min_x = -5.0;
        min_y = -5.0;
        max_x = 5.0;
        max_y = 5.0;
    }
    let margin = ((max_x - min_x).max(max_y - min_y) * 0.15).max(1.0);
    min_x -= margin;
    min_y -= margin;
    max_x += margin;
    max_y += margin;
    let w = max_x - min_x;
    let h = max_y - min_y;

    let mut svg = String::with_capacity(2048);
    let _ = write!(
        svg,
        r#"<svg xmlns="http://www.w3.org/2000/svg" viewBox="{x:.3} {y:.3} {w:.3} {h:.3}" width="100%" height="100%">"#,
        // Flip Y so positive Y is up — TOP view, matching how the rest
        // of the board canvas behaves.
        x = min_x,
        y = -(min_y + h),
        w = w,
        h = h,
    );
    // Substrate.
    let _ = write!(
        svg,
        r##"<rect x="{x:.3}" y="{y:.3}" width="{w:.3}" height="{h:.3}" fill="#0e1116"/>"##,
        x = min_x,
        y = -(min_y + h),
        w = w,
        h = h,
    );
    svg.push_str(r#"<g transform="scale(1,-1)">"#);

    // Light millimetre grid for scale.
    let grid_step = 1.0;
    let gx0 = (min_x / grid_step).floor() * grid_step;
    let gx1 = (max_x / grid_step).ceil() * grid_step;
    let gy0 = (min_y / grid_step).floor() * grid_step;
    let gy1 = (max_y / grid_step).ceil() * grid_step;
    svg.push_str(r##"<g stroke="#1a1f27" stroke-width="0.02" fill="none">"##);
    let mut gx = gx0;
    while gx <= gx1 + 1e-9 {
        let _ = write!(
            svg,
            r#"<line x1="{gx:.3}" y1="{min_y:.3}" x2="{gx:.3}" y2="{max_y:.3}"/>"#,
        );
        gx += grid_step;
    }
    let mut gy = gy0;
    while gy <= gy1 + 1e-9 {
        let _ = write!(
            svg,
            r#"<line x1="{min_x:.3}" y1="{gy:.3}" x2="{max_x:.3}" y2="{gy:.3}"/>"#,
        );
        gy += grid_step;
    }
    svg.push_str("</g>");

    // Pads. Top layer colour (this is a library-side render, no layer
    // info beyond the SMD/through-hole drill flag — assume top.)
    for pad in &entry.pads {
        let x = pad.x_mm - pad.w_mm / 2.0;
        let y = pad.y_mm - pad.h_mm / 2.0;
        let is_gnd = is_ground_pad_label(&pad.number, &pad.name);
        let fill = "#c97a2b";
        let _ = write!(
            svg,
            r#"<rect x="{x:.3}" y="{y:.3}" width="{w:.3}" height="{h:.3}" fill="{fill}"/>"#,
            x = x,
            y = y,
            w = pad.w_mm,
            h = pad.h_mm,
        );
        if is_gnd {
            let _ = write!(
                svg,
                r##"<rect x="{x:.3}" y="{y:.3}" width="{w:.3}" height="{h:.3}" fill="none" stroke="#ff2bd6" stroke-width="0.18"/>"##,
                x = x,
                y = y,
                w = pad.w_mm,
                h = pad.h_mm,
            );
        }
        if let Some(d) = pad.drill_mm {
            let _ = write!(
                svg,
                r##"<circle cx="{cx:.3}" cy="{cy:.3}" r="{r:.3}" fill="#0e1116"/>"##,
                cx = pad.x_mm,
                cy = pad.y_mm,
                r = d / 2.0,
            );
        }
        // Pad labels — show the PIN NUMBER (always) plus the net name
        // when present. Jairo needs to cross-reference the datasheet by
        // pin index, so the number must be visible even when a net is
        // assigned. Two stacked text lines: number on top, net below.
        // Y-flip the text group because the outer `scale(1,-1)` would
        // otherwise mirror the glyphs.
        let pin_number = pad.number.as_str();
        let net_label = if pad.name.is_empty() {
            None
        } else {
            Some(pad.name.as_str())
        };
        let base_sz = (pad.w_mm.min(pad.h_mm) * 0.45).clamp(0.3, 1.2);
        let label_color = if is_gnd { "#ff2bd6" } else { "#0e1116" };
        // When there are two lines, shrink slightly and offset.
        let (num_sz, net_sz) = if net_label.is_some() {
            (base_sz * 0.85, base_sz * 0.7)
        } else {
            (base_sz, base_sz)
        };
        // Offsets in mm relative to the pad center, in PRE-flip coords
        // (we wrap each text in its own scale(1,-1) so positive dy is
        // visually downward).
        let (num_dy, net_dy) = if net_label.is_some() {
            (-num_sz * 0.55, net_sz * 0.85)
        } else {
            (0.0, 0.0)
        };
        let _ = write!(
            svg,
            r#"<g transform="translate({cx:.3},{cy:.3}) scale(1,-1)"><text x="0" y="{dy:.3}" text-anchor="middle" dominant-baseline="middle" font-family="ui-monospace, monospace" font-size="{sz:.2}" fill="{label_color}" font-weight="bold">{lab}</text></g>"#,
            cx = pad.x_mm,
            cy = -pad.y_mm,
            dy = num_dy,
            sz = num_sz,
            label_color = label_color,
            lab = escape(pin_number),
        );
        if let Some(net) = net_label {
            let _ = write!(
                svg,
                r#"<g transform="translate({cx:.3},{cy:.3}) scale(1,-1)"><text x="0" y="{dy:.3}" text-anchor="middle" dominant-baseline="middle" font-family="ui-monospace, monospace" font-size="{sz:.2}" fill="{label_color}" font-weight="normal">{lab}</text></g>"#,
                cx = pad.x_mm,
                cy = -pad.y_mm,
                dy = net_dy,
                sz = net_sz,
                label_color = label_color,
                lab = escape(net),
            );
        }
    }

    // Pin-1 marker — a small yellow dot off the top-left corner of pad
    // "1" if it exists, plus an explicit "pin 1" label so the reviewer
    // never has to guess what the dot means. Helps verify canonical
    // orientation against the datasheet.
    if let Some(p1) = entry.pads.iter().find(|p| p.number == "1") {
        let cx = p1.x_mm - p1.w_mm / 2.0 - 0.4;
        let cy = p1.y_mm + p1.h_mm / 2.0 + 0.4;
        let _ = write!(
            svg,
            r##"<circle cx="{cx:.3}" cy="{cy:.3}" r="0.3" fill="#ffd166"/>"##,
        );
        // Label sits just above the dot. The outer scale(1,-1) flips
        // glyphs, so wrap in its own unflip.
        let label_sz = 0.7_f64;
        let label_cx = cx;
        let label_cy = cy + 0.55;
        let _ = write!(
            svg,
            r##"<g transform="translate({cx:.3},{cy:.3}) scale(1,-1)"><text x="0" y="0" text-anchor="middle" dominant-baseline="middle" font-family="ui-monospace, monospace" font-size="{sz:.2}" fill="#ffd166" font-weight="bold">pin 1</text></g>"##,
            cx = label_cx,
            cy = -label_cy,
            sz = label_sz,
        );
    }

    // TOP-view tag in a corner so the reviewer cannot confuse the
    // orientation. Plain SVG text (the outer flip mirrors it, so undo
    // that locally).
    let tag_x = min_x + 0.5;
    let tag_y = max_y - 0.5;
    let _ = write!(
        svg,
        r##"<g transform="translate({x:.3},{y:.3}) scale(1,-1)"><text x="0" y="0" font-family="ui-monospace, monospace" font-size="0.9" fill="#3a4452">TOP view</text></g>"##,
        x = tag_x,
        y = -tag_y,
    );

    svg.push_str("</g></svg>");
    svg
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
    // Nets that have a pour ARE connected — through the pour copper,
    // not via traces. Skip the ratsnest for them so a 20-pad GND
    // does not draw the spaghetti star.
    for pour in &board.pours {
        routed.insert(pour.net.as_str());
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
    let stroke = layer_trace_stroke(trace.layer);
    let layer_label = layer_text_label(trace.layer);
    let _ = write!(
        svg,
        r#"<line data-trace-id="{id}" pathLength="1" x1="{x1:.3}" y1="{y1:.3}" x2="{x2:.3}" y2="{y2:.3}" stroke="{stroke}" stroke-width="{w:.3}" stroke-linecap="round"><title>{net} ({layer_label})</title></line>"#,
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
/// in pointerdown. Currently unused — the outline-resize UI path was
/// drafted but not yet wired into the render pipeline.
#[allow(dead_code)]
fn write_outline_handles(svg: &mut String, outline: Rect) {
    let cx = f64::midpoint(outline.min.x.to_mm(), outline.max.x.to_mm());
    let cy = f64::midpoint(outline.min.y.to_mm(), outline.max.y.to_mm());
    let w = (outline.max.x - outline.min.x).to_mm();
    let h = (outline.max.y - outline.min.y).to_mm();
    // Handle is a small square sitting on the edge midpoint.
    let s = (w.min(h) * 0.04).clamp(0.8, 3.0);
    let handles = [
        ("top", cx, outline.max.y.to_mm()),
        ("bottom", cx, outline.min.y.to_mm()),
        ("right", outline.max.x.to_mm(), cy),
        ("left", outline.min.x.to_mm(), cy),
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

/// Visual clearance the pour render leaves around foreign-net items,
/// in mm. INTENTIONALLY larger than the fab `POUR_CLEARANCE` (0.2 mm
/// in `pcb-gerber`) so the cutouts are obvious on a typical zoom —
/// at 0.2 mm the halo is one or two pixels and reads as solid.
const POUR_CLEARANCE_MM: f64 = 0.6;
/// Inset of the pour from the board outline, in mm. Matches the fab
/// value so the visible pour edge is where the copper actually ends.
const POUR_EDGE_CLEARANCE_MM: f64 = 0.3;
/// Cell size of the rasterised pour mask, in mm. Smaller = sharper
/// edges + slower render. 0.125 mm keeps a 100×60 mm board under
/// ~400 k cells, which morphology + RLE chew through in ~20 ms in
/// release.
const POUR_GRID_MM: f64 = 0.125;
/// Minimum continuous-copper strip width the pour will tolerate,
/// in mm. Slivers of pour below this width are removed by a
/// morphological CLOSE on the void grid (dilate-then-erode by half
/// the strip). Roughly matches what `KiCad` calls "min copper width".
const POUR_MIN_STRIP_MM: f64 = 1.2;
/// Smallest connected pour island the renderer will keep, in mm².
/// After identifying the largest pour component as "the main
/// plane", every OTHER pour blob whose total area is under this
/// threshold gets converted into void. Matches `KiCad`'s "min
/// island area".
const POUR_MIN_ISLAND_MM2: f64 = 30.0;

/// FR4 substrate fill across the board outline. Sits between the
/// canvas grid and the pour copper so any spot uncovered by the pour
/// (clearance voids, edge ring) reads as bare PCB.
fn write_substrate_fill(svg: &mut String, outline: Rect, corner_radius_mm: f64) {
    let r = corner_radius_mm.max(0.0);
    let _ = write!(
        svg,
        r##"<rect x="{x:.3}" y="{y:.3}" width="{w:.3}" height="{h:.3}" rx="{r:.3}" ry="{r:.3}" fill="#5a3a1f" fill-opacity="0.55" pointer-events="none"/>"##,
        x = outline.min.x.to_mm(),
        y = outline.min.y.to_mm(),
        w = outline.width().to_mm(),
        h = outline.height().to_mm(),
    );
}

/// Render a copper pour as a black-filled rectangle (the outline
/// inset by the edge clearance) HIDDEN by an SVG `<mask>` wherever a
/// foreign-net pad, trace, or via on the pour's layer needs
/// clearance.
///
/// Why a mask, not a `<path>` with `fill-rule="evenodd"`?
/// Even-odd only flips on RING NESTING. When two cutout rectangles
/// overlap (e.g. a foreign pad and the foreign trace leaving it),
/// even-odd counts that as depth-3 → filled → a black "island"
/// appears exactly where the user expects continuous clearance. SVG
/// masks solve it directly: `black + black` in the mask is still
/// black, so overlapping cutouts naturally form one continuous
/// keepout. Same trick `KiCad` uses internally for its plot output.
///
/// Mask convention: white pixels show the underlying fill, black
/// pixels hide it. So the mask starts as a fully-white rect (pour
/// visible everywhere) and we paint BLACK shapes for every
/// clearance region.
fn write_pour_polygon(svg: &mut String, board: &Board, pour: &pcb_core::Pour, outline: Rect) {
    let inset = POUR_EDGE_CLEARANCE_MM;
    let x0 = outline.min.x.to_mm() + inset;
    let y0 = outline.min.y.to_mm() + inset;
    let x1 = outline.max.x.to_mm() - inset;
    let y1 = outline.max.y.to_mm() - inset;
    if x1 <= x0 || y1 <= y0 {
        return;
    }
    let w = x1 - x0;
    let h = y1 - y0;
    // Pour follows the outline's corner curve, inset by the same
    // edge-clearance amount as everything else. Smaller radius (or
    // zero, sharp corners) = sharper inner pour corners.
    let pour_radius = (board.outline_corner_radius.to_mm() - inset).max(0.0);
    let layer_tag = layer_short_tag(pour.layer);
    let mask_id = format!("pour-mask-{layer_tag}-{}", sanitize_id(&pour.net));
    let cl = POUR_CLEARANCE_MM;

    // 1. Rasterise every keepout (foreign-net pad/trace/via) into a
    //    boolean grid. `void[i]` = true means "pour copper does NOT
    //    appear in this cell".
    let cell = POUR_GRID_MM;
    let cols = ((w / cell).ceil() as usize).max(1);
    let rows = ((h / cell).ceil() as usize).max(1);
    let mut void = vec![false; cols * rows];

    let cell_x = |i: usize| x0 + (i as f64 + 0.5) * cell;
    let cell_y = |j: usize| y0 + (j as f64 + 0.5) * cell;

    let mark_rect = |grid: &mut [bool], rx: f64, ry: f64, rw: f64, rh: f64| {
        let i0 = (((rx - x0) / cell).floor() as i64).max(0) as usize;
        let i1 = (((rx + rw - x0) / cell).ceil() as i64).max(0) as usize;
        let j0 = (((ry - y0) / cell).floor() as i64).max(0) as usize;
        let j1 = (((ry + rh - y0) / cell).ceil() as i64).max(0) as usize;
        for j in j0..j1.min(rows) {
            for i in i0..i1.min(cols) {
                grid[j * cols + i] = true;
            }
        }
    };

    let mark_circle = |grid: &mut [bool], cx: f64, cy: f64, r: f64| {
        let i0 = (((cx - r - x0) / cell).floor() as i64).max(0) as usize;
        let i1 = (((cx + r - x0) / cell).ceil() as i64).max(0) as usize;
        let j0 = (((cy - r - y0) / cell).floor() as i64).max(0) as usize;
        let j1 = (((cy + r - y0) / cell).ceil() as i64).max(0) as usize;
        let r2 = r * r;
        for j in j0..j1.min(rows) {
            for i in i0..i1.min(cols) {
                let dx = cell_x(i) - cx;
                let dy = cell_y(j) - cy;
                if dx * dx + dy * dy <= r2 {
                    grid[j * cols + i] = true;
                }
            }
        }
    };

    let mark_segment = |grid: &mut [bool], sx: f64, sy: f64, ex: f64, ey: f64, half: f64| {
        let xmin = sx.min(ex) - half;
        let xmax = sx.max(ex) + half;
        let ymin = sy.min(ey) - half;
        let ymax = sy.max(ey) + half;
        let i0 = (((xmin - x0) / cell).floor() as i64).max(0) as usize;
        let i1 = (((xmax - x0) / cell).ceil() as i64).max(0) as usize;
        let j0 = (((ymin - y0) / cell).floor() as i64).max(0) as usize;
        let j1 = (((ymax - y0) / cell).ceil() as i64).max(0) as usize;
        let dxs = ex - sx;
        let dys = ey - sy;
        let len2 = dxs * dxs + dys * dys;
        let half2 = half * half;
        for j in j0..j1.min(rows) {
            for i in i0..i1.min(cols) {
                let px = cell_x(i);
                let py = cell_y(j);
                let t = if len2 > 1e-9 {
                    (((px - sx) * dxs + (py - sy) * dys) / len2).clamp(0.0, 1.0)
                } else {
                    0.0
                };
                let qx = sx + t * dxs;
                let qy = sy + t * dys;
                let dx = px - qx;
                let dy = py - qy;
                if dx * dx + dy * dy <= half2 {
                    grid[j * cols + i] = true;
                }
            }
        }
    };

    // Inverse of `mark_segment`: restore pour copper along a capsule of
    // half-width `half`. Used to punch thermal-relief spokes (orthogonal
    // or diagonal) back into the void. Rounded ends match the Gerber
    // writer's round spoke aperture.
    let punch_segment = |grid: &mut [bool], sx: f64, sy: f64, ex: f64, ey: f64, half: f64| {
        let xmin = sx.min(ex) - half;
        let xmax = sx.max(ex) + half;
        let ymin = sy.min(ey) - half;
        let ymax = sy.max(ey) + half;
        let i0 = (((xmin - x0) / cell).floor() as i64).max(0) as usize;
        let i1 = (((xmax - x0) / cell).ceil() as i64).max(0) as usize;
        let j0 = (((ymin - y0) / cell).floor() as i64).max(0) as usize;
        let j1 = (((ymax - y0) / cell).ceil() as i64).max(0) as usize;
        let dxs = ex - sx;
        let dys = ey - sy;
        let len2 = dxs * dxs + dys * dys;
        let half2 = half * half;
        for j in j0..j1.min(rows) {
            for i in i0..i1.min(cols) {
                let px = cell_x(i);
                let py = cell_y(j);
                let t = if len2 > 1e-9 {
                    (((px - sx) * dxs + (py - sy) * dys) / len2).clamp(0.0, 1.0)
                } else {
                    0.0
                };
                let qx = sx + t * dxs;
                let qy = sy + t * dys;
                let dx = px - qx;
                let dy = py - qy;
                if dx * dx + dy * dy <= half2 {
                    grid[j * cols + i] = false;
                }
            }
        }
    };

    // Resolve the thermal relief from the pour. `Solid` keeps the
    // legacy flood — same-net pads merge into the pour without any
    // cutout. `Spokes4` punches a thermal ring around each same-net
    // pad, leaving 4 narrow copper bridges N/S/E/W.
    let (spoke_w_mm, gap_mm) = match pour.thermal_relief {
        pcb_core::ThermalRelief::Solid => (0.0_f64, 0.0_f64),
        pcb_core::ThermalRelief::Spokes4 {
            spoke_width_mm,
            gap_mm,
        } => (spoke_width_mm, gap_mm),
    };
    let use_spokes = spoke_w_mm > 0.0 && gap_mm > 0.0;

    // Dangling stubs carry no real copper; the spoke-collision test
    // skips them just like the exporters do.
    let orphan_traces = board.orphan_trace_ids();
    let orphan_vias = board.orphan_via_ids();

    // Thermal-relief spoke segments (mm) collected during the same-net
    // pass and punched back into copper as the very last step, so no
    // foreign void or morphological pass can erase a real spoke.
    let mut spoke_segments: Vec<(f64, f64, f64, f64)> = Vec::new();

    for fp in board.footprints_in_order() {
        for pad in &fp.pads {
            if !pad.occupies_layer(pour.layer) {
                continue;
            }
            let c = fp.pad_world_center(pad);
            let (pw, ph) = fp.pad_world_size(pad);
            if pad.net.as_deref() == Some(pour.net.as_str()) {
                if !use_spokes {
                    // Solid relief — nothing to do, the pour floods.
                    continue;
                }
                // Spokes4: mark an annular keepout around the pad,
                // then re-clear the 4 spoke arms so copper bridges
                // the pad to the pour through them.
                let cx = c.x.to_mm();
                let cy = c.y.to_mm();
                let half_w = pw.to_mm() / 2.0;
                let half_h = ph.to_mm() / 2.0;
                // Annular gap ring: rect inflated by `gap_mm` voided.
                mark_rect(
                    &mut void,
                    cx - half_w - gap_mm,
                    cy - half_h - gap_mm,
                    pw.to_mm() + gap_mm * 2.0,
                    ph.to_mm() + gap_mm * 2.0,
                );
                // Collect the thermal-relief spokes. `select_spokes`
                // returns exactly the bars the Gerber writer draws
                // (orthogonal where clear, 45° diagonals, then a fine
                // angular sweep as a last resort so a boxed-in pad still
                // bonds to its plane). We punch them in a FINAL pass —
                // after the foreign keepouts, the morphological close and
                // the island prune — so a real spoke is never erased by a
                // foreign void or eaten as a "thin sliver", keeping the
                // displayed pour in lock-step with the manufactured
                // copper (render == fab).
                let bridge_len = gap_mm + 0.1; // overshoot for safety
                let reach = pcb_core::Length::from_mm(bridge_len);
                let spoke_half_l = pcb_core::Length::from_mm(spoke_w_mm) / 2;
                for (a, b) in pcb_core::thermal::select_spokes(
                    c,
                    pw,
                    ph,
                    spoke_half_l,
                    pcb_core::thermal::POUR_CLEARANCE,
                    reach,
                    pour.net.as_str(),
                    pour.layer,
                    board,
                    &orphan_traces,
                    &orphan_vias,
                ) {
                    spoke_segments.push((a.x.to_mm(), a.y.to_mm(), b.x.to_mm(), b.y.to_mm()));
                }
                // Foreign-net pads fall through; they're voided in the
                // second pass below.
            }
        }
    }
    // Second pass: void every foreign-net pad.
    for fp in board.footprints_in_order() {
        for pad in &fp.pads {
            if !pad.occupies_layer(pour.layer) {
                continue;
            }
            if pad.net.as_deref() == Some(pour.net.as_str()) {
                continue;
            }
            let c = fp.pad_world_center(pad);
            let (pw, ph) = fp.pad_world_size(pad);
            mark_rect(
                &mut void,
                c.x.to_mm() - pw.to_mm() / 2.0 - cl,
                c.y.to_mm() - ph.to_mm() / 2.0 - cl,
                pw.to_mm() + cl * 2.0,
                ph.to_mm() + cl * 2.0,
            );
        }
    }
    for trace in board.traces.iter().filter(|t| t.layer == pour.layer) {
        if trace.net == pour.net {
            continue;
        }
        mark_segment(
            &mut void,
            trace.start.x.to_mm(),
            trace.start.y.to_mm(),
            trace.end.x.to_mm(),
            trace.end.y.to_mm(),
            trace.width.to_mm() / 2.0 + cl,
        );
    }
    for via in &board.vias {
        if via.net == pour.net {
            continue;
        }
        mark_circle(
            &mut void,
            via.position.x.to_mm(),
            via.position.y.to_mm(),
            via.diameter.to_mm() / 2.0 + cl,
        );
    }

    // 2. Morphological CLOSE on the void: dilate by R, then erode
    //    by R. Closes every pour-copper sliver narrower than 2*R
    //    cells = `POUR_MIN_STRIP_MM`. CLOSE-on-void is equivalent
    //    to OPEN-on-pour (de Morgan), so this eats unmanufacturable
    //    strips of pour without growing the legitimate keepouts.
    let r_cells = ((POUR_MIN_STRIP_MM * 0.5) / cell).round() as usize;
    if r_cells > 0 {
        let mut tmp = vec![false; cols * rows];
        morph_dilate(&void, &mut tmp, cols, rows, r_cells);
        morph_erode(&tmp, &mut void, cols, rows, r_cells);
    }

    // 2b. Find the LARGEST connected pour component (the "main
    //     plane") and prune any other component whose area is
    //     below `POUR_MIN_ISLAND_MM2` mm². Components that touch a
    //     same-net pad are protected (the pad needs the surrounding
    //     pour copper to read as electrically connected to GND).
    //     This avoids the previous approach's over-aggressive
    //     dilation that fragmented the central copper of densely
    //     populated ICs.
    let mut protect = vec![false; cols * rows];
    for fp in board.footprints_in_order() {
        for pad in &fp.pads {
            if !pad.occupies_layer(pour.layer) {
                continue;
            }
            if pad.net.as_deref() != Some(pour.net.as_str()) {
                continue;
            }
            let c = fp.pad_world_center(pad);
            let (pw, ph) = fp.pad_world_size(pad);
            mark_rect(
                &mut protect,
                c.x.to_mm() - pw.to_mm() / 2.0 - cl,
                c.y.to_mm() - ph.to_mm() / 2.0 - cl,
                pw.to_mm() + cl * 2.0,
                ph.to_mm() + cl * 2.0,
            );
        }
    }

    let min_cells = (POUR_MIN_ISLAND_MM2 / (cell * cell)).round() as usize;
    prune_pour_islands(&mut void, &protect, cols, rows, min_cells);

    // 2c. Punch the thermal-relief spokes back into copper as the final
    //     step. Doing this AFTER the foreign keepouts, the morphological
    //     close and the island prune guarantees a real spoke is always
    //     shown — it can't be erased by a foreign void it threads past
    //     (the bond is genuine copper in fab) nor swallowed as a thin
    //     pour sliver. This is what bonds a boxed-in pad to its plane on
    //     screen exactly as it will be manufactured.
    if use_spokes {
        let spoke_half = spoke_w_mm / 2.0;
        for (ax, ay, bx, by) in &spoke_segments {
            punch_segment(&mut void, *ax, *ay, *bx, *by, spoke_half);
        }
    }

    // 3. Open the SVG mask. Its pixel space is the pour rect.
    //    White = pour visible; black = pour hidden (void).
    let _ = write!(
        svg,
        r#"<defs><mask id="{mask_id}" maskUnits="userSpaceOnUse" x="{x0:.3}" y="{y0:.3}" width="{w:.3}" height="{h:.3}"><rect x="{x0:.3}" y="{y0:.3}" width="{w:.3}" height="{h:.3}" fill="white"/>"#,
    );

    // 4. Run-length encode horizontal void runs into black rects.
    //    Most rows have only a handful of runs; for an N-component
    //    board this is typically a few thousand rects total.
    for j in 0..rows {
        let row_y = y0 + j as f64 * cell;
        let mut i = 0usize;
        while i < cols {
            if !void[j * cols + i] {
                i += 1;
                continue;
            }
            let start = i;
            while i < cols && void[j * cols + i] {
                i += 1;
            }
            let rx = x0 + start as f64 * cell;
            let rw = (i - start) as f64 * cell;
            let _ = write!(
                svg,
                r#"<rect x="{rx:.3}" y="{row_y:.3}" width="{rw:.3}" height="{cell:.3}" fill="black"/>"#,
            );
        }
    }

    let _ = write!(svg, "</mask></defs>");

    // 5. The pour itself: a dark rectangle filling the outline-
    //    inset area, masked by the rasterised void above.
    let _ = write!(
        svg,
        r##"<rect x="{x0:.3}" y="{y0:.3}" width="{w:.3}" height="{h:.3}" rx="{pour_radius:.3}" ry="{pour_radius:.3}" fill="#0e1116" fill-opacity="0.78" mask="url(#{mask_id})" pointer-events="none"/>"##,
    );
}

/// 4-neighbour connected-component analysis on the pour (= non-void
/// cells in `void`). The largest component is treated as the "main
/// pour plane" and kept untouched. Every OTHER component is pruned
/// (turned into void) UNLESS:
///   * it touches a cell in `protect` (a same-net pad needing the
///     pour copper to remain), OR
///   * its area is at least `min_cells` (large enough that it is
///     probably an intentional secondary pour region).
fn prune_pour_islands(
    void: &mut [bool],
    protect: &[bool],
    cols: usize,
    rows: usize,
    min_cells: usize,
) {
    let n = cols * rows;
    // First pass: label every pour cell with a component id and
    // record per-component size + protected-flag.
    let mut comp = vec![u32::MAX; n];
    let mut sizes: Vec<usize> = Vec::new();
    let mut prot: Vec<bool> = Vec::new();
    let mut stack: Vec<usize> = Vec::with_capacity(64);
    for start in 0..n {
        if void[start] || comp[start] != u32::MAX {
            continue;
        }
        let id = sizes.len() as u32;
        sizes.push(0);
        prot.push(false);
        stack.clear();
        stack.push(start);
        comp[start] = id;
        while let Some(idx) = stack.pop() {
            sizes[id as usize] += 1;
            if protect[idx] {
                prot[id as usize] = true;
            }
            let i = idx % cols;
            let j = idx / cols;
            let push = |ni: usize, nj: usize, comp: &mut [u32], stack: &mut Vec<usize>| {
                let nidx = nj * cols + ni;
                if !void[nidx] && comp[nidx] == u32::MAX {
                    comp[nidx] = id;
                    stack.push(nidx);
                }
            };
            if i + 1 < cols {
                push(i + 1, j, &mut comp, &mut stack);
            }
            if i > 0 {
                push(i - 1, j, &mut comp, &mut stack);
            }
            if j + 1 < rows {
                push(i, j + 1, &mut comp, &mut stack);
            }
            if j > 0 {
                push(i, j - 1, &mut comp, &mut stack);
            }
        }
    }

    // Find the biggest component → that's the main plane.
    let main = sizes
        .iter()
        .enumerate()
        .max_by_key(|(_, s)| **s)
        .map(|(i, _)| i as u32);
    let Some(main) = main else { return };

    // Second pass: prune every cell whose component is neither the
    // main plane, nor protected, nor large enough to be intentional.
    for idx in 0..n {
        let id = comp[idx];
        if id == u32::MAX {
            continue;
        }
        if id == main {
            continue;
        }
        let id_u = id as usize;
        if prot[id_u] {
            continue;
        }
        if sizes[id_u] >= min_cells {
            continue;
        }
        void[idx] = true;
    }
}

/// Output[i,j] = true iff any cell within `r` of (i,j) in `input` is
/// true. Separable two-pass implementation: horizontal max into a
/// scratch buffer, then vertical max into `output`.
fn morph_dilate(input: &[bool], output: &mut [bool], cols: usize, rows: usize, r: usize) {
    let mut scratch = vec![false; cols * rows];
    // Horizontal pass.
    for j in 0..rows {
        for i in 0..cols {
            let lo = i.saturating_sub(r);
            let hi = (i + r + 1).min(cols);
            let mut any = false;
            for k in lo..hi {
                if input[j * cols + k] {
                    any = true;
                    break;
                }
            }
            scratch[j * cols + i] = any;
        }
    }
    // Vertical pass.
    for j in 0..rows {
        let lo = j.saturating_sub(r);
        let hi = (j + r + 1).min(rows);
        for i in 0..cols {
            let mut any = false;
            for k in lo..hi {
                if scratch[k * cols + i] {
                    any = true;
                    break;
                }
            }
            output[j * cols + i] = any;
        }
    }
}

/// Output[i,j] = true iff every cell within `r` of (i,j) in `input`
/// is true. Same separable structure as `morph_dilate` with `all`
/// in place of `any`.
fn morph_erode(input: &[bool], output: &mut [bool], cols: usize, rows: usize, r: usize) {
    let mut scratch = vec![true; cols * rows];
    for j in 0..rows {
        for i in 0..cols {
            let lo = i.saturating_sub(r);
            let hi = (i + r + 1).min(cols);
            let mut all = true;
            for k in lo..hi {
                if !input[j * cols + k] {
                    all = false;
                    break;
                }
            }
            scratch[j * cols + i] = all;
        }
    }
    for j in 0..rows {
        let lo = j.saturating_sub(r);
        let hi = (j + r + 1).min(rows);
        for i in 0..cols {
            let mut all = true;
            for k in lo..hi {
                if !scratch[k * cols + i] {
                    all = false;
                    break;
                }
            }
            output[j * cols + i] = all;
        }
    }
}

/// Lowercase ASCII letters/digits/dashes — anything else becomes `_`.
/// Used to slugify the pour's net name for the SVG mask id.
fn sanitize_id(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
            out.push(c.to_ascii_lowercase());
        } else {
            out.push('_');
        }
    }
    if out.is_empty() {
        out.push_str("net");
    }
    out
}

fn write_rect_stroke(
    svg: &mut String,
    rect: Rect,
    stroke: &str,
    width_mm: f64,
    corner_radius_mm: f64,
) {
    let r = corner_radius_mm.max(0.0);
    let _ = write!(
        svg,
        r#"<rect x="{x:.3}" y="{y:.3}" width="{w:.3}" height="{h:.3}" rx="{r:.3}" ry="{r:.3}" fill="none" stroke="{stroke}" stroke-width="{sw:.3}"/>"#,
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

/// Per-layer fill colour for pads. Layer 0 = orange, last = cyan,
/// inner signal layers cycle through a saturated palette so a
/// 4/6/8-layer render stays legible without consulting a legend.
/// Pre-Phase-4 boards (2 layers) get the historical orange/blue pair.
fn layer_pad_fill(layer: Layer) -> &'static str {
    // We can't see the stackup from here so we treat index 1 as the
    // bottom for the 2-layer historical case AND as the first inner
    // for N>2. The renderer only ever materialises items currently
    // declared in `Trace.layer`/`Pad.layer`, which today are at most
    // index 1 (CopperLayer::Bottom). For inner layers we still want
    // a colour mapping so the multi-layer support is in place.
    PAD_PALETTE
        .get(layer.index as usize)
        .copied()
        .unwrap_or("#888")
}

fn layer_trace_stroke(layer: Layer) -> &'static str {
    TRACE_PALETTE
        .get(layer.index as usize)
        .copied()
        .unwrap_or("#aaa")
}

fn layer_text_label(layer: Layer) -> &'static str {
    match layer.index {
        0 => "top",
        1 => "bottom",
        2 => "in1",
        3 => "in2",
        4 => "in3",
        5 => "in4",
        6 => "in5",
        _ => "inner",
    }
}

fn layer_short_tag(layer: Layer) -> &'static str {
    match layer.index {
        0 => "t",
        1 => "b",
        2 => "i1",
        3 => "i2",
        4 => "i3",
        5 => "i4",
        6 => "i5",
        _ => "in",
    }
}

/// Palette for pad fills, one entry per layer index.
const PAD_PALETTE: &[&str] = &[
    "#c97a2b", // 0 - top — historical orange.
    "#2b6cc9", // 1 - bottom (2-layer historical blue) OR inner-1.
    "#3aa66c", // 2 - inner green.
    "#a63a8c", // 3 - inner purple.
    "#d6b500", // 4 - inner yellow.
    "#b0303a", // 5 - inner red.
    "#3aa6a6", // 6 - inner teal.
    "#9c6b3a", // 7 - inner sienna.
];

/// Palette for trace strokes — brighter variants of `PAD_PALETTE` so
/// traces visually float above their pads.
const TRACE_PALETTE: &[&str] = &[
    "#ffd166", // top - gold (historical).
    "#4ec9ff", // bottom - cyan (historical).
    "#84e8b3", // inner green.
    "#e495d2", // inner purple.
    "#ffe89a", // inner yellow.
    "#ff95a0", // inner red.
    "#9ce5e5", // inner teal.
    "#deb887", // inner sienna.
];

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ground_net_matches_common_spellings() {
        for n in [
            "GND", "gnd", "Gnd", "GROUND", "ground", "vss", "VSS", "0V", "0v", "AGND", "DGND",
            "PGND",
        ] {
            assert!(is_ground_net(n), "expected {n} to read as ground");
        }
        for n in [
            "VCC",
            "VDD",
            "3V3",
            "5V",
            "SDA",
            "MISO",
            "GNDB",
            "MY_GND_NET",
        ] {
            assert!(!is_ground_net(n), "expected {n} NOT to read as ground");
        }
    }

    #[test]
    fn ground_pad_label_falls_through_to_pad_name() {
        // Some libraries number GND pins by digits but name them "GND".
        assert!(is_ground_pad_label("12", "GND"));
        assert!(is_ground_pad_label("GND", ""));
        assert!(!is_ground_pad_label("12", ""));
    }

    #[test]
    fn library_entry_svg_marks_ground_pads() {
        let entry = pcb_core::LibraryEntry {
            key: "test_part".into(),
            description: String::new(),
            default_value: String::new(),
            default_rotation_deg: 0.0,
            edge_mounted: false,
            pads: vec![
                pcb_core::LibraryPad {
                    number: "1".into(),
                    name: "VCC".into(),
                    x_mm: -2.0,
                    y_mm: 0.0,
                    w_mm: 1.0,
                    h_mm: 1.0,
                    drill_mm: None,
                },
                pcb_core::LibraryPad {
                    number: "2".into(),
                    name: "GND".into(),
                    x_mm: 2.0,
                    y_mm: 0.0,
                    w_mm: 1.0,
                    h_mm: 1.0,
                    drill_mm: None,
                },
            ],
            silk: Vec::new(),
            lcsc_id: None,
            mpn: None,
            attachments: Vec::new(),
            created_at: 0,
            footprint_view_transform: pcb_core::ViewTransform::default(),
            placement_margin: pcb_core::PlacementMargin::default(),
        };
        let svg = render_library_entry_svg(&entry);
        // GND highlight colour appears.
        assert!(
            svg.contains("#ff2bd6"),
            "expected GND highlight colour in SVG"
        );
        // TOP-view tag is visible.
        assert!(svg.contains("TOP view"));
        // Both pad labels present.
        assert!(svg.contains("VCC"));
        assert!(svg.contains("GND"));
    }

    #[test]
    fn library_entry_svg_with_no_pads_does_not_panic() {
        let entry = pcb_core::LibraryEntry {
            key: "empty".into(),
            description: String::new(),
            default_value: String::new(),
            default_rotation_deg: 0.0,
            edge_mounted: false,
            pads: Vec::new(),
            silk: Vec::new(),
            lcsc_id: None,
            mpn: None,
            attachments: Vec::new(),
            created_at: 0,
            footprint_view_transform: pcb_core::ViewTransform::default(),
            placement_margin: pcb_core::PlacementMargin::default(),
        };
        let svg = render_library_entry_svg(&entry);
        assert!(svg.starts_with("<svg"));
        assert!(svg.ends_with("</svg>"));
    }
}
