//! Gerber RS-274X writer.
//!
//! Implements the subset of RS-274X needed to produce a manufacturable
//! board for the layers we currently model: copper (per side), solder
//! mask (per side), and edge cuts. Coordinates are emitted in 4.6 mm
//! format with leading-zero suppression — i.e. our internal nanometre
//! `Length` is *exactly* the integer encoding Gerber expects.

use std::collections::HashMap;
use std::io::{self, Write};

use pcb_core::{Board, CopperLayer, Length, Point};

/// Mask clearance applied per side when expanding pad apertures into
/// solder-mask openings. 0.05 mm is the JLC/KiCad default.
const MASK_CLEARANCE: Length = Length(50_000); // 0.05 mm

/// Edge.Cuts stroke width.
const EDGE_STROKE: Length = Length(50_000); // 0.05 mm

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
    fn copper_layer(self) -> CopperLayer {
        match self {
            Self::Top => CopperLayer::Top,
            Self::Bottom => CopperLayer::Bottom,
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
    writeln!(w, "G04 pcb {label}*")?;
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

fn select(w: &mut impl Write, id: u32) -> io::Result<()> {
    writeln!(w, "D{id}*")
}

fn footer(w: &mut impl Write) -> io::Result<()> {
    writeln!(w, "M02*")
}

/// Write the copper layer for the given side. Includes pad flashes,
/// trace polylines (drawn with circular line apertures), and via copper
/// pads (flashed on every layer the via punches through).
pub fn write_copper(board: &Board, side: Side, w: &mut impl Write) -> io::Result<()> {
    write_header(w, side.copper_label())?;
    let mut table = Table::default();
    let mut flashes = Vec::<(u32, Point)>::new();
    let mut draws = Vec::<(u32, Point, Point)>::new();
    let layer = side.copper_layer();

    for fp in board.footprints_in_order() {
        for pad in &fp.pads {
            if pad.layer != layer {
                continue;
            }
            let center = fp.pad_world_center(pad);
            let (pw, ph) = fp.pad_world_size(pad);
            let id = table.intern(Aperture::Rect { w: pw, h: ph });
            flashes.push((id, center));
        }
    }
    for trace in board.traces.iter().filter(|t| t.layer == layer) {
        let id = table.intern(Aperture::Round { d: trace.width });
        draws.push((id, trace.start, trace.end));
    }
    for via in &board.vias {
        let id = table.intern(Aperture::Round { d: via.diameter });
        flashes.push((id, via.position));
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
            if pad.layer != layer {
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
    let p00 = Point::new(rect.min.x, rect.min.y);
    let p10 = Point::new(rect.max.x, rect.min.y);
    let p11 = Point::new(rect.max.x, rect.max.y);
    let p01 = Point::new(rect.min.x, rect.max.y);
    move_to(w, p00)?;
    line_to(w, p10)?;
    line_to(w, p11)?;
    line_to(w, p01)?;
    line_to(w, p00)?;
    footer(w)
}
