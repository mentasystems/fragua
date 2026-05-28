//! Gerber RS-274X writer.
//!
//! Implements the subset of RS-274X needed to produce a manufacturable
//! board for the layers we currently model: copper (per side), solder
//! mask (per side), and edge cuts. Coordinates are emitted in 4.6 mm
//! format with leading-zero suppression — i.e. our internal nanometre
//! `Length` is *exactly* the integer encoding Gerber expects.

use std::collections::{HashMap, HashSet};
use std::io::{self, Write};

use pcb_core::{
    hershey, silk_clip, Board, CopperLayer, FootprintSilk, Layer, Length, Point, Rect, SilkAnchor,
    SilkLayer,
};

/// Mask clearance applied per side when expanding pad apertures into
/// solder-mask openings. 0.05 mm is the JLC/KiCad default.
const MASK_CLEARANCE: Length = Length(50_000); // 0.05 mm

/// Edge.Cuts stroke width.
const EDGE_STROKE: Length = Length(50_000); // 0.05 mm

/// Per-side clearance the pour leaves around foreign-net pads, traces,
/// and vias. Matches the DRC's `min_clearance` so a clean route + a
/// pour produce a fab-correct file.
const POUR_CLEARANCE: Length = Length(200_000); // 0.2 mm

/// Inset of the pour polygon from the board outline. Matches the
/// DRC's `edge_clearance` so the fab does not slot into the pour.
const POUR_EDGE_CLEARANCE: Length = Length(300_000); // 0.3 mm

#[derive(Clone, Copy)]
pub enum Side {
    Top,
    Bottom,
}

impl Side {
    fn copper_label(self) -> &'static str {
        match self {
            Self::Top => "F.Cu",
            Self::Bottom => "B.Cu",
        }
    }
    fn mask_label(self) -> &'static str {
        match self {
            Self::Top => "F.Mask",
            Self::Bottom => "B.Mask",
        }
    }
    fn silk_label(self) -> &'static str {
        match self {
            Self::Top => "F.SilkS",
            Self::Bottom => "B.SilkS",
        }
    }
    fn copper_layer(self) -> CopperLayer {
        match self {
            Self::Top => CopperLayer::Top,
            Self::Bottom => CopperLayer::Bottom,
        }
    }
    fn silk_layer(self) -> SilkLayer {
        match self {
            Self::Top => SilkLayer::Top,
            Self::Bottom => SilkLayer::Bottom,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
enum Aperture {
    Rect { w: Length, h: Length },
    Round { d: Length },
}

#[derive(Default)]
struct Table {
    map: HashMap<Aperture, u32>,
    list: Vec<Aperture>,
}

impl Table {
    /// Aperture IDs start at D10 per RS-274X convention (D01-D03 are
    /// reserved for draw/move/flash operations).
    fn intern(&mut self, ap: Aperture) -> u32 {
        if let Some(&id) = self.map.get(&ap) {
            return id;
        }
        #[allow(clippy::cast_possible_truncation)]
        let id = 10 + self.list.len() as u32;
        self.map.insert(ap, id);
        self.list.push(ap);
        id
    }
}

fn write_header(w: &mut impl Write, label: &str) -> io::Result<()> {
    // Modern Gerber X2 file-function attributes so fab-house DFM
    // viewers (JLCPCB, PCBWay, OSH Park) auto-detect each layer
    // without relying on filename suffixes alone. The mapping
    // between our `label` and the X2 file-function string follows
    // the KiCad convention.
    let function = match label {
        "F.Cu" => Some("Copper,L1,Top"),
        "B.Cu" => Some("Copper,L2,Bot"),
        "F.Mask" => Some("Soldermask,Top"),
        "B.Mask" => Some("Soldermask,Bot"),
        "F.SilkS" => Some("Legend,Top"),
        "B.SilkS" => Some("Legend,Bot"),
        "Edge.Cuts" => Some("Profile,NP"),
        _ => None,
    };
    writeln!(w, "G04 pcb {label}*")?;
    writeln!(
        w,
        "%TF.GenerationSoftware,pcb,pcb-gerber,{}*%",
        env!("CARGO_PKG_VERSION")
    )?;
    if let Some(func) = function {
        writeln!(w, "%TF.FileFunction,{func}*%")?;
        writeln!(w, "%TF.FilePolarity,Positive*%")?;
    }
    writeln!(w, "%FSLAX46Y46*%")?;
    writeln!(w, "%MOMM*%")?;
    writeln!(w, "%LPD*%")?;
    Ok(())
}

fn write_apertures(w: &mut impl Write, table: &Table) -> io::Result<()> {
    for (idx, ap) in table.list.iter().enumerate() {
        #[allow(clippy::cast_possible_truncation)]
        let id = 10 + idx as u32;
        match *ap {
            Aperture::Rect { w: aw, h: ah } => {
                writeln!(w, "%ADD{id}R,{:.6}X{:.6}*%", aw.to_mm(), ah.to_mm())?;
            }
            Aperture::Round { d } => {
                writeln!(w, "%ADD{id}C,{:.6}*%", d.to_mm())?;
            }
        }
    }
    Ok(())
}

fn coord(l: Length) -> i64 {
    l.0
}

fn flash(w: &mut impl Write, p: Point) -> io::Result<()> {
    writeln!(w, "X{}Y{}D03*", coord(p.x), coord(p.y))
}

fn move_to(w: &mut impl Write, p: Point) -> io::Result<()> {
    writeln!(w, "X{}Y{}D02*", coord(p.x), coord(p.y))
}

fn line_to(w: &mut impl Write, p: Point) -> io::Result<()> {
    writeln!(w, "X{}Y{}D01*", coord(p.x), coord(p.y))
}

/// Counter-clockwise circular arc (Gerber `G03`) from the current
/// pen position to `end`, with the arc's centre offset from the
/// start by `(i_off, j_off)` relative coords. The caller must have
/// emitted `G75*` already (multi-quadrant mode) and a `move_to` to
/// the arc's start point.
fn arc_to_ccw(w: &mut impl Write, end: Point, i_off: Length, j_off: Length) -> io::Result<()> {
    writeln!(
        w,
        "G03X{}Y{}I{}J{}D01*",
        coord(end.x),
        coord(end.y),
        coord(i_off),
        coord(j_off),
    )
}

fn select(w: &mut impl Write, id: u32) -> io::Result<()> {
    writeln!(w, "D{id}*")
}

fn footer(w: &mut impl Write) -> io::Result<()> {
    writeln!(w, "M02*")
}

/// Write the copper layer for the given side. Includes pad flashes,
/// trace polylines (drawn with circular line apertures), via copper
/// pads (flashed on every layer the via punches through), and any
/// `Pour` declared for this layer (rendered as a dark G36/G37 region
/// inset from the outline, with foreign-net items punched out via
/// negative polarity).
///
/// Orphan traces and orphan vias (leftover stubs from a half-finished
/// route) are filtered out before flashing — those would otherwise
/// appear in the fab files as dangling copper that the fab house
/// would manufacture for no reason.
/// Multi-layer entry point: write the copper layer at `index` in the
/// board's stackup. For 2-layer boards this is a 1:1 wrapper over
/// `write_copper(Side::Top|Bottom)`; for 4/6/8-layer boards each call
/// emits one of the inner signal/plane layers using its stackup name.
pub fn write_copper_layer(board: &Board, layer: Layer, w: &mut impl Write) -> io::Result<()> {
    if layer.is_top() {
        return write_copper(board, Side::Top, w);
    }
    let bottom = board.stackup.layer_count().saturating_sub(1);
    if layer.index == bottom {
        return write_copper(board, Side::Bottom, w);
    }
    // Inner layer: emit a copper file scoped to that layer only.
    // Reuses the same machinery as `write_copper` but with a custom
    // header label sourced from the stackup so the fab portal can
    // pick it up.
    let label = board
        .stackup
        .layers
        .get(layer.index as usize)
        .map_or_else(|| format!("In{}.Cu", layer.index), |l| l.name.clone());
    write_copper_inner_layer(board, layer, &label, w)
}

fn write_copper_inner_layer(
    board: &Board,
    target_layer: Layer,
    label: &str,
    w: &mut impl Write,
) -> io::Result<()> {
    write_header(w, label)?;
    // Pre-collect what we need.
    let mut table = Table::default();
    // Pads on this layer. PTH pads (drill.is_some) have a copper ring
    // on every copper layer, so `occupies_layer` folds that in.
    for fp in board.footprints_in_order() {
        for pad in &fp.pads {
            if !pad.occupies_layer(target_layer) {
                continue;
            }
            let (pw, ph) = fp.pad_world_size(pad);
            table.intern(Aperture::Rect { w: pw, h: ph });
        }
    }
    // Traces on this layer.
    let orphans = board.orphan_trace_ids();
    for t in &board.traces {
        if t.layer != target_layer || orphans.contains(&t.id) {
            continue;
        }
        table.intern(Aperture::Round { d: t.width });
    }
    // Vias punch through, so include their copper pad on every layer.
    let orphan_vias = board.orphan_via_ids();
    for v in &board.vias {
        if orphan_vias.contains(&v.id) {
            continue;
        }
        table.intern(Aperture::Round { d: v.diameter });
    }
    write_apertures(w, &table)?;
    // Pads.
    for fp in board.footprints_in_order() {
        for pad in &fp.pads {
            if !pad.occupies_layer(target_layer) {
                continue;
            }
            let (pw, ph) = fp.pad_world_size(pad);
            let id = table.intern(Aperture::Rect { w: pw, h: ph });
            select(w, id)?;
            flash(w, fp.pad_world_center(pad))?;
        }
    }
    // Traces.
    for t in &board.traces {
        if t.layer != target_layer || orphans.contains(&t.id) {
            continue;
        }
        let id = table.intern(Aperture::Round { d: t.width });
        select(w, id)?;
        move_to(w, t.start)?;
        line_to(w, t.end)?;
    }
    // Vias copper.
    for v in &board.vias {
        if orphan_vias.contains(&v.id) {
            continue;
        }
        let id = table.intern(Aperture::Round { d: v.diameter });
        select(w, id)?;
        flash(w, v.position)?;
    }
    footer(w)
}

pub fn write_copper(board: &Board, side: Side, w: &mut impl Write) -> io::Result<()> {
    write_header(w, side.copper_label())?;
    let layer = side.copper_layer();
    let mut table = Table::default();
    let orphan_traces = board.orphan_trace_ids();
    let orphan_vias = board.orphan_via_ids();

    // Same-net set for this layer: a pad/trace/via on one of these
    // nets is electrically continuous with the pour and does NOT get
    // a clearance void.
    let pour_nets: HashSet<&str> = board
        .pours
        .iter()
        .filter(|p| p.layer == layer)
        .map(|p| p.net.as_str())
        .collect();
    let has_pour = !pour_nets.is_empty();

    // Clearance-void apertures (foreign-net items, expanded by the
    // pour clearance on every side). Empty when `has_pour` is false.
    let mut void_flashes: Vec<(u32, Point)> = Vec::new();
    let mut void_draws: Vec<(u32, Point, Point)> = Vec::new();
    // Thermal-relief spokes: 4 narrow copper bars per same-net pad
    // when the pour requests `Spokes4`. Emitted as dark draws AFTER
    // the LPC void pass so the pad ↔ pour bridge survives the ring.
    let mut spoke_draws: Vec<(u32, Point, Point)> = Vec::new();
    if has_pour {
        let cl = POUR_CLEARANCE;
        // Build a quick {net -> ThermalRelief} for the pours on this
        // layer so we can decide whether to add thermal voids+spokes
        // around same-net pads.
        let pour_relief: HashMap<&str, pcb_core::ThermalRelief> = board
            .pours
            .iter()
            .filter(|p| p.layer == layer)
            .map(|p| (p.net.as_str(), p.thermal_relief))
            .collect();
        for fp in board.footprints_in_order() {
            for pad in &fp.pads {
                if !pad.occupies_layer(layer) {
                    continue;
                }
                let pad_net = pad.net.as_deref();
                let same_net_pour = pad_net.and_then(|n| pour_relief.get(n).copied());
                if let Some(relief) = same_net_pour {
                    if let pcb_core::ThermalRelief::Spokes4 {
                        spoke_width_mm,
                        gap_mm,
                    } = relief
                    {
                        // Thermal ring: a rectangle the same shape as
                        // the pad but inflated by `gap_mm` is voided
                        // in clear polarity, leaving a halo around the
                        // pad copper. The four spoke bars (dark) are
                        // emitted after to bridge pad→pour.
                        let center = fp.pad_world_center(pad);
                        let (pw, ph) = fp.pad_world_size(pad);
                        let gap = Length::from_mm(gap_mm);
                        let ring_id = table.intern(Aperture::Rect {
                            w: pw + gap + gap,
                            h: ph + gap + gap,
                        });
                        void_flashes.push((ring_id, center));
                        // Spoke aperture: circular trace, width =
                        // spoke_width_mm. Each spoke runs from the pad
                        // edge to just past the gap.
                        let spoke_id = table.intern(Aperture::Round {
                            d: Length::from_mm(spoke_width_mm),
                        });
                        let half_w = pw.0 / 2;
                        let half_h = ph.0 / 2;
                        let len = gap.0 + 100_000; // gap + 0.1 mm overshoot
                        // East / west
                        spoke_draws.push((
                            spoke_id,
                            Point::new(Length(center.x.0 - half_w - len), center.y),
                            Point::new(Length(center.x.0 - half_w + 50_000), center.y),
                        ));
                        spoke_draws.push((
                            spoke_id,
                            Point::new(Length(center.x.0 + half_w - 50_000), center.y),
                            Point::new(Length(center.x.0 + half_w + len), center.y),
                        ));
                        // North / south
                        spoke_draws.push((
                            spoke_id,
                            Point::new(center.x, Length(center.y.0 - half_h - len)),
                            Point::new(center.x, Length(center.y.0 - half_h + 50_000)),
                        ));
                        spoke_draws.push((
                            spoke_id,
                            Point::new(center.x, Length(center.y.0 + half_h - 50_000)),
                            Point::new(center.x, Length(center.y.0 + half_h + len)),
                        ));
                    }
                    // Solid (or non-Spokes4): no void, no spoke — pad
                    // melts into the pour, legacy behaviour.
                    continue;
                }
                let center = fp.pad_world_center(pad);
                let (pw, ph) = fp.pad_world_size(pad);
                let id = table.intern(Aperture::Rect {
                    w: pw + cl + cl,
                    h: ph + cl + cl,
                });
                void_flashes.push((id, center));
            }
        }
        for trace in board.traces.iter().filter(|t| t.layer == layer) {
            if pour_nets.contains(trace.net.as_str()) {
                continue;
            }
            if orphan_traces.contains(&trace.id) {
                continue;
            }
            let id = table.intern(Aperture::Round {
                d: trace.width + cl + cl,
            });
            void_draws.push((id, trace.start, trace.end));
        }
        for via in &board.vias {
            // Vias punch every layer, so a via on a foreign net always
            // gets a void on the pour layer regardless of which layer
            // the trace approaching it lives on.
            if pour_nets.contains(via.net.as_str()) {
                continue;
            }
            if orphan_vias.contains(&via.id) {
                continue;
            }
            let id = table.intern(Aperture::Round {
                d: via.diameter + cl + cl,
            });
            void_flashes.push((id, via.position));
        }
    }

    // Regular dark-polarity flashes: every pad, trace, and via on this
    // layer in its true geometry. Same-net pads sit ON the pour and
    // merge seamlessly; foreign-net pads sit INSIDE their clearance
    // void leaving the keepout ring intact.
    let mut flashes: Vec<(u32, Point)> = Vec::new();
    let mut draws: Vec<(u32, Point, Point)> = Vec::new();
    for fp in board.footprints_in_order() {
        for pad in &fp.pads {
            if !pad.occupies_layer(layer) {
                continue;
            }
            let center = fp.pad_world_center(pad);
            let (pw, ph) = fp.pad_world_size(pad);
            let id = table.intern(Aperture::Rect { w: pw, h: ph });
            flashes.push((id, center));
        }
    }
    for trace in board.traces.iter().filter(|t| t.layer == layer) {
        if orphan_traces.contains(&trace.id) {
            continue;
        }
        let id = table.intern(Aperture::Round { d: trace.width });
        draws.push((id, trace.start, trace.end));
    }
    for via in &board.vias {
        if orphan_vias.contains(&via.id) {
            continue;
        }
        let id = table.intern(Aperture::Round { d: via.diameter });
        flashes.push((id, via.position));
    }

    write_apertures(w, &table)?;

    // 1. Lay down the pour polygon (dark) — outline inset by the
    //    pour edge clearance.
    if has_pour {
        if let Some(outline) = board.outline {
            let inset = POUR_EDGE_CLEARANCE;
            let x0 = outline.min.x + inset;
            let y0 = outline.min.y + inset;
            let x1 = outline.max.x - inset;
            let y1 = outline.max.y - inset;
            writeln!(w, "G36*")?;
            writeln!(w, "X{}Y{}D02*", coord(x0), coord(y0))?;
            writeln!(w, "X{}Y{}D01*", coord(x1), coord(y0))?;
            writeln!(w, "X{}Y{}D01*", coord(x1), coord(y1))?;
            writeln!(w, "X{}Y{}D01*", coord(x0), coord(y1))?;
            writeln!(w, "X{}Y{}D01*", coord(x0), coord(y0))?;
            writeln!(w, "G37*")?;
        }
        // 2. Switch to clear polarity and punch keepouts around every
        //    foreign-net pad / trace / via.
        writeln!(w, "%LPC*%")?;
        let mut current = 0u32;
        for (id, p) in &void_flashes {
            if *id != current {
                select(w, *id)?;
                current = *id;
            }
            flash(w, *p)?;
        }
        for (id, a, b) in &void_draws {
            if *id != current {
                select(w, *id)?;
                current = *id;
            }
            move_to(w, *a)?;
            line_to(w, *b)?;
        }
        // 3. Back to dark for the regular pad/trace/via flashes that
        //    follow. Thermal-relief spokes are dark draws bridging
        //    pad → pour through the gap ring punched above.
        writeln!(w, "%LPD*%")?;
        let mut current_spoke = 0u32;
        for (id, a, b) in &spoke_draws {
            if *id != current_spoke {
                select(w, *id)?;
                current_spoke = *id;
            }
            move_to(w, *a)?;
            line_to(w, *b)?;
        }
    }

    let mut current = 0u32;
    for (id, p) in flashes {
        if id != current {
            select(w, id)?;
            current = id;
        }
        flash(w, p)?;
    }
    for (id, a, b) in draws {
        if id != current {
            select(w, id)?;
            current = id;
        }
        move_to(w, a)?;
        line_to(w, b)?;
    }
    footer(w)
}

/// Write the solder-mask opening layer for the given side.
pub fn write_mask(board: &Board, side: Side, w: &mut impl Write) -> io::Result<()> {
    write_header(w, side.mask_label())?;
    let mut table = Table::default();
    let mut flashes = Vec::<(u32, Point)>::new();
    let layer = side.copper_layer();
    for fp in board.footprints_in_order() {
        for pad in &fp.pads {
            if !pad.occupies_layer(layer) {
                continue;
            }
            let center = fp.pad_world_center(pad);
            let (pw, ph) = fp.pad_world_size(pad);
            let id = table.intern(Aperture::Rect {
                w: pw + MASK_CLEARANCE + MASK_CLEARANCE,
                h: ph + MASK_CLEARANCE + MASK_CLEARANCE,
            });
            flashes.push((id, center));
        }
    }
    write_apertures(w, &table)?;
    let mut current = 0u32;
    for (id, p) in flashes {
        if id != current {
            select(w, id)?;
            current = id;
        }
        flash(w, p)?;
    }
    footer(w)
}

/// Write the Edge.Cuts layer (board outline). If the board has no
/// explicit outline we fall back to the content bounding box plus a
/// 2 mm margin so the fab still has *something* to cut.
pub fn write_edge_cuts(board: &Board, w: &mut impl Write) -> io::Result<()> {
    write_header(w, "Edge.Cuts")?;
    let outline = board.outline.or_else(|| {
        board
            .content_bounds()
            .map(|r| r.expand(Length::from_mm(2.0)))
    });
    let Some(rect) = outline else {
        footer(w)?;
        return Ok(());
    };
    let mut table = Table::default();
    let id = table.intern(Aperture::Round { d: EDGE_STROKE });
    write_apertures(w, &table)?;
    select(w, id)?;

    // Sharp corners — single rectangle of straight segments.
    let radius = board.outline_corner_radius;
    if radius.0 == 0 {
        let p00 = Point::new(rect.min.x, rect.min.y);
        let p10 = Point::new(rect.max.x, rect.min.y);
        let p11 = Point::new(rect.max.x, rect.max.y);
        let p01 = Point::new(rect.min.x, rect.max.y);
        move_to(w, p00)?;
        line_to(w, p10)?;
        line_to(w, p11)?;
        line_to(w, p01)?;
        line_to(w, p00)?;
        return footer(w);
    }

    // Rounded corners: 4 straight edges + 4 CCW quarter-arcs.
    // `G75*` enables multi-quadrant arc mode (the only sane choice for
    // arbitrary arc spans, including 90°). Without it, fab CAM tools
    // can read I/J offsets as single-quadrant only and corrupt the
    // outline.
    writeln!(w, "G75*")?;
    let r = radius;
    let xmin = rect.min.x;
    let ymin = rect.min.y;
    let xmax = rect.max.x;
    let ymax = rect.max.y;
    // Path traversed CCW (looking at the board from the top): start
    // on the bottom edge just after the bottom-left arc and walk
    // counter-clockwise around the perimeter.
    let p_bottom_start = Point::new(xmin + r, ymin);
    let p_bottom_end = Point::new(xmax - r, ymin);
    let p_right_start = Point::new(xmax, ymin + r);
    let p_right_end = Point::new(xmax, ymax - r);
    let p_top_start = Point::new(xmax - r, ymax);
    let p_top_end = Point::new(xmin + r, ymax);
    let p_left_start = Point::new(xmin, ymax - r);
    let p_left_end = Point::new(xmin, ymin + r);

    move_to(w, p_bottom_start)?;
    line_to(w, p_bottom_end)?;
    // Bottom-right arc: centre (xmax - r, ymin + r). Offset from
    // start = (0, +r).
    arc_to_ccw(w, p_right_start, Length(0), r)?;
    line_to(w, p_right_end)?;
    // Top-right arc: centre (xmax - r, ymax - r). Offset = (-r, 0).
    arc_to_ccw(w, p_top_start, Length(-r.0), Length(0))?;
    line_to(w, p_top_end)?;
    // Top-left arc: centre (xmin + r, ymax - r). Offset = (0, -r).
    arc_to_ccw(w, p_left_start, Length(0), Length(-r.0))?;
    line_to(w, p_left_end)?;
    // Bottom-left arc: centre (xmin + r, ymin + r). Offset = (+r, 0).
    arc_to_ccw(w, p_bottom_start, r, Length(0))?;
    footer(w)
}

/// Default silk text stroke width when none is provided. Roughly
/// matches the `KiCad` default of size/8.
fn default_silk_stroke(size: Length) -> Length {
    Length(size.0 / 8)
}

/// Write the silkscreen layer for `side`. Every line — board-level
/// strokes, board-level text (vectorised via Hershey), and every
/// footprint's silk transformed to world coords — is emitted as a
/// `D01` interpolation under a circular aperture matching the
/// stroke width. Multiple stroke widths are coalesced through the
/// regular aperture-table machinery so the file stays compact.
// Each emitted item is one stroke under an aperture. A polyline
// groups two or more points so the writer can fold them into a
// single `D02 ... D01 ...; D01 ...; ...` run, instead of re-issuing
// `D02` between every pair. Bare segments stay in single-line form
// for clarity.
enum Stroke {
    Seg(Point, Point),
    Poly(Vec<Point>),
}

pub fn write_silk(board: &Board, side: Side, w: &mut impl Write) -> io::Result<()> {
    write_header(w, side.silk_label())?;
    let layer = side.silk_layer();
    let mut table = Table::default();
    let mut draws: Vec<(u32, Stroke)> = Vec::new();

    // Board-level lines.
    for line in board.silk_lines.iter().filter(|l| l.layer == layer) {
        let id = table.intern(Aperture::Round { d: line.width });
        draws.push((id, Stroke::Seg(line.start, line.end)));
    }
    // Board-level text → polyline strokes (one polyline per glyph
    // stroke). Board-level text never overlaps a pad — pads belong to
    // footprints — so no clipping is needed.
    for txt in board.silk_texts.iter().filter(|t| t.layer == layer) {
        let polys =
            hershey::text_polylines(&txt.text, txt.position, txt.size, txt.rotation, txt.anchor);
        let stroke_w = if txt.width.0 > 0 {
            txt.width
        } else {
            default_silk_stroke(txt.size)
        };
        let id = table.intern(Aperture::Round { d: stroke_w });
        for poly in polys {
            draws.push((id, Stroke::Poly(poly)));
        }
    }
    // Footprint-attached silk (or the synthesised default).
    for fp in board.footprints_in_order() {
        // World-space pad rects for clipping. Same approach as the
        // renderer — pads on the same footprint mask out silk that
        // would land on copper.
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
            // Default `{REF}` label, matching the renderer.
            let default_layer = if fp.layer.is_top() {
                SilkLayer::Top
            } else {
                SilkLayer::Bottom
            };
            if default_layer != layer {
                continue;
            }
            let primary = if fp.key.is_empty() {
                fp.reference.as_str()
            } else {
                fp.key.as_str()
            };
            if primary.is_empty() {
                continue;
            }
            // Anchor 0.6 mm above the body bbox (same as renderer).
            let Some(body) = footprint_body_local(fp) else {
                continue;
            };
            let anchor_local = Point::new(Length::ZERO, body.1 + Length::from_mm(0.6));
            let pos = fp.local_to_world(anchor_local);
            let size = Length::from_mm(0.9);
            let stroke_w = default_silk_stroke(size);
            let id = table.intern(Aperture::Round { d: stroke_w });
            // Default label sits above the body, so it never crosses
            // a pad — emit polylines straight through.
            let polys =
                hershey::text_polylines(primary, pos, size, fp.rotation, SilkAnchor::Middle);
            for poly in polys {
                draws.push((id, Stroke::Poly(poly)));
            }
        } else {
            for item in &fp.silk {
                match *item {
                    FootprintSilk::Line {
                        layer: l,
                        start,
                        end,
                        width,
                    } => {
                        if l != layer {
                            continue;
                        }
                        let s = fp.local_to_world(start);
                        let e = fp.local_to_world(end);
                        let id = table.intern(Aperture::Round { d: width });
                        for (a, b) in silk_clip::clip_segment(s, e, &pad_rects) {
                            draws.push((id, Stroke::Seg(a, b)));
                        }
                    }
                    FootprintSilk::Text {
                        layer: l,
                        position,
                        ref text,
                        size,
                        rotation,
                        anchor,
                        width,
                    } => {
                        if l != layer {
                            continue;
                        }
                        let pos = fp.local_to_world(position);
                        let resolved = fp.resolve_silk_text(text);
                        let stroke_w = if width.0 > 0 {
                            width
                        } else {
                            default_silk_stroke(size)
                        };
                        let id = table.intern(Aperture::Round { d: stroke_w });
                        let polys = hershey::text_polylines(
                            &resolved,
                            pos,
                            size,
                            rotation + fp.rotation,
                            anchor,
                        );
                        for poly in polys {
                            // Clip each segment of the polyline; the
                            // result may break the polyline into
                            // shorter runs. We re-emit the original
                            // polyline if every segment survived
                            // (the common case — most glyph strokes
                            // miss every pad), otherwise fall back to
                            // per-segment emission. This keeps the
                            // optimisation while staying correct.
                            if pad_rects.is_empty() || polyline_misses_all(&poly, &pad_rects) {
                                draws.push((id, Stroke::Poly(poly)));
                            } else {
                                for pair in poly.windows(2) {
                                    for (a, b) in
                                        silk_clip::clip_segment(pair[0], pair[1], &pad_rects)
                                    {
                                        draws.push((id, Stroke::Seg(a, b)));
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }
    }

    write_apertures(w, &table)?;
    let mut current = 0u32;
    for (id, stroke) in draws {
        if id != current {
            select(w, id)?;
            current = id;
        }
        match stroke {
            Stroke::Seg(a, b) => {
                move_to(w, a)?;
                line_to(w, b)?;
            }
            Stroke::Poly(points) => {
                let mut iter = points.into_iter();
                let Some(first) = iter.next() else { continue };
                move_to(w, first)?;
                for p in iter {
                    line_to(w, p)?;
                }
            }
        }
    }
    footer(w)
}

/// Cheap pre-check: every vertex of the polyline lies outside every
/// rect, AND no segment endpoint pair brackets any rect. We use a
/// quick "all vertices outside + no bbox overlap" test which is
/// conservative — when it returns true the polyline is guaranteed
/// not to cross any pad and the writer can keep the polyline as one
/// run. False means "fall back to per-segment clip".
fn polyline_misses_all(poly: &[Point], rects: &[Rect]) -> bool {
    if poly.is_empty() {
        return true;
    }
    // 1. No vertex inside any rect.
    for p in poly {
        for r in rects {
            if p.x >= r.min.x && p.x <= r.max.x && p.y >= r.min.y && p.y <= r.max.y {
                return false;
            }
        }
    }
    // 2. No segment bbox overlaps any rect (would-be crossing
    //    even though both endpoints sit outside).
    for pair in poly.windows(2) {
        let (a, b) = (pair[0], pair[1]);
        let xmin = a.x.min(b.x);
        let xmax = a.x.max(b.x);
        let ymin = a.y.min(b.y);
        let ymax = a.y.max(b.y);
        for r in rects {
            if xmax >= r.min.x && xmin <= r.max.x && ymax >= r.min.y && ymin <= r.max.y {
                return false;
            }
        }
    }
    true
}

/// Local-coord (`max_y`, ...) of the bounding box of `fp`'s pads —
/// in footprint-local frame, ignoring rotation. Used by the silk
/// writer to anchor the default `{REF}` label without round-tripping
/// through `Footprint::bounds` (which gives world-space coords).
/// Returns `(min_y, max_y)`.
fn footprint_body_local(fp: &pcb_core::Footprint) -> Option<(Length, Length)> {
    let mut iter = fp.pads.iter().map(|pad| {
        let half_h = pad.size.1 / 2;
        (pad.offset.y - half_h, pad.offset.y + half_h)
    });
    let (mut lo, mut hi) = iter.next()?;
    for (a, b) in iter {
        if a < lo {
            lo = a;
        }
        if b > hi {
            hi = b;
        }
    }
    // Mimic the 0.4 mm body expand the renderer uses.
    Some((lo - Length::from_mm(0.4), hi + Length::from_mm(0.4)))
}

#[cfg(test)]
mod thermal_relief_tests {
    use super::*;
    use pcb_core::{Board, CopperLayer, Footprint, Id, Pad, Pour, Rect, ThermalRelief};

    fn board_with_pour(relief: ThermalRelief) -> Board {
        let mut b = Board::new();
        b.outline = Some(Rect::from_corners(
            Point::new(Length::from_mm(0.0), Length::from_mm(0.0)),
            Point::new(Length::from_mm(40.0), Length::from_mm(20.0)),
        ));
        b.add_footprint(Footprint {
            id: Id::new(),
            reference: "R1".into(),
            value: String::new(),
            library: "test".into(),
            position: Point::new(Length::from_mm(10.0), Length::from_mm(10.0)),
            rotation: 0.0,
            layer: CopperLayer::Top,
            pads: vec![Pad {
                number: "1".into(),
                name: String::new(),
                offset: Point::new(Length::from_mm(0.0), Length::from_mm(0.0)),
                size: (Length::from_mm(1.5), Length::from_mm(1.5)),
                layer: CopperLayer::Top,
                net: Some("GND".into()),
                drill: None,
            }],
            key: String::new(),
            description: String::new(),
            edge_mounted: false,
            silk: Vec::new(),
        });
        b.add_pour(Pour {
            net: "GND".into(),
            layer: CopperLayer::Top,
            thermal_relief: relief,
            stitching: pcb_core::StitchPolicy::None,
        });
        b
    }

    #[test]
    fn pour_with_spokes_generates_4_bridges_per_pad() {
        let b = board_with_pour(ThermalRelief::Spokes4 {
            spoke_width_mm: 0.4,
            gap_mm: 0.4,
        });
        let mut out = Vec::new();
        write_copper(&b, Side::Top, &mut out).unwrap();
        let text = String::from_utf8(out).unwrap();
        // 4 spoke draws → 4 "D01" move/draw pairs in the dark
        // section after LPD. They sit AFTER the LPC void pass, and
        // each pair is 1 move_to (D02) + 1 line_to (D01).
        let lpd_idx = text.rfind("%LPD*%").expect("missing LPD section");
        let tail = &text[lpd_idx..];
        let d01_count = tail.matches("D01*").count();
        assert!(
            d01_count >= 4,
            "expected >=4 D01 ops post-LPD for 4 thermal spokes, got {d01_count}\n{tail}"
        );
    }

    #[test]
    fn pour_solid_unchanged() {
        // Solid pour: no thermal voids or spokes around the same-net
        // pad; the LPC section after the pour region only contains
        // foreign-net keepouts (none here). So the D01 count after
        // LPD should reflect just the pad/trace/via flashes, no
        // thermal spokes.
        let b = board_with_pour(ThermalRelief::Solid);
        let mut out = Vec::new();
        write_copper(&b, Side::Top, &mut out).unwrap();
        let text = String::from_utf8(out).unwrap();
        // Solid relief means no spoke "D02 ... D01 ..." line pairs
        // were emitted. The only D01 after %LPD*% should be the
        // explicit dark-mode flashes (pads/vias use D03, not D01;
        // traces use D01 but there are none here).
        let lpd_idx = text.rfind("%LPD*%").expect("missing LPD section");
        let tail = &text[lpd_idx..];
        let d01_count = tail.matches("D01*").count();
        assert_eq!(
            d01_count, 0,
            "solid relief should not emit any D01 line draws after LPD, got {d01_count}\n{tail}"
        );
    }
}
