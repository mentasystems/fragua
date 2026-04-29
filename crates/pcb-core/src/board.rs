//! Board model.
//!
//! A `Board` holds the physical layout: copper layer stack, footprints,
//! traces, vias, and outline. The schematic side lives in `schematic.rs`
//! (added in Phase 2) — this is enough for the Phase 1 placement loop.

use std::collections::HashMap;

use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::geometry::{Point, Rect};
use crate::units::Length;

/// Stable identifier for any item the human or agent can address by name
/// across MCP calls and UI events.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize,
)]
pub struct Id(pub Uuid);

impl Id {
    #[must_use]
    pub fn new() -> Self {
        Self(Uuid::new_v4())
    }

    /// Parse a UUID string. Used by MCP tool inputs that accept ids
    /// as strings.
    pub fn parse(s: &str) -> Result<Self, String> {
        Uuid::parse_str(s).map(Self).map_err(|e| e.to_string())
    }
}

impl Default for Id {
    fn default() -> Self {
        Self::new()
    }
}

/// Copper layer slot. Phase 1 only models the two outer layers; inner
/// layers are added when we tackle multi-layer routing.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize,
)]
pub enum CopperLayer {
    Top,
    Bottom,
}

/// A pad on a footprint. Phase 1: rectangular SMD pads only — round
/// pads, ovals, and through-hole follow when we start consuming a real
/// component library.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Pad {
    /// Footprint-local pad number (e.g. "1", "2", "GND").
    pub number: String,
    /// Position relative to the footprint origin.
    pub offset: Point,
    pub size: (Length, Length),
    pub layer: CopperLayer,
    /// Net this pad belongs to. `None` until the netlist is synced.
    pub net: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Footprint {
    pub id: Id,
    /// Reference designator, e.g. "R1", "U3". Unique within a board.
    pub reference: String,
    /// Component value, e.g. "10k", "STM32F103".
    pub value: String,
    /// Library identifier, e.g. "Resistor_SMD:R_0805". Free-form for now.
    pub library: String,
    pub position: Point,
    /// Rotation in degrees, counter-clockwise.
    pub rotation: f32,
    pub layer: CopperLayer,
    pub pads: Vec<Pad>,
    /// Library key copied from the schematic symbol when this footprint
    /// was placed (snake_case, e.g. "esp32_s3_zero"). Empty string if
    /// the symbol had no key. Lets `view.snapshot` and the UI cross-
    /// reference back to the library entry.
    #[serde(default)]
    pub key: String,
    /// Free-form description copied from the schematic symbol — kept on
    /// the footprint so the agent's intent survives even after the
    /// schematic side is reset.
    #[serde(default)]
    pub description: String,
    /// Whether this footprint must touch a board edge (USB-C cables,
    /// screw terminals, antennas). Honoured by `placement` checks.
    #[serde(default)]
    pub edge_mounted: bool,
}

impl Footprint {
    /// Bounding box of the footprint in board coordinates, taking the
    /// footprint's rotation into account.
    #[must_use]
    pub fn bounds(&self) -> Option<Rect> {
        let mut iter = self.pads.iter().map(|pad| {
            let center = self.pad_world_center(pad);
            let (w, h) = self.pad_world_size(pad);
            Rect::from_center(center, w, h)
        });
        let first = iter.next()?;
        Some(iter.fold(first, Rect::union))
    }

    /// Absolute board-coord centre of `pad` after applying the
    /// footprint's position and rotation. The pad's offset is treated
    /// as a vector in footprint-local coords and rotated CCW around
    /// the footprint origin.
    #[must_use]
    pub fn pad_world_center(&self, pad: &Pad) -> Point {
        let theta = f64::from(self.rotation).to_radians();
        let (sin, cos) = (theta.sin(), theta.cos());
        let ox = pad.offset.x.to_mm();
        let oy = pad.offset.y.to_mm();
        let rx = ox * cos - oy * sin;
        let ry = ox * sin + oy * cos;
        Point::new(
            crate::units::Length::from_mm(self.position.x.to_mm() + rx),
            crate::units::Length::from_mm(self.position.y.to_mm() + ry),
        )
    }

    /// Pad dimensions on the board after rotation. We only model 90°
    /// increments (the placer and the rotate-by-keypress path both
    /// produce multiples of 90) so this just swaps width ↔ height
    /// when the rotation lands in the 90° / 270° quadrant.
    #[must_use]
    pub fn pad_world_size(&self, pad: &Pad) -> (crate::units::Length, crate::units::Length) {
        let r = f64::from(self.rotation).rem_euclid(360.0);
        if (45.0..135.0).contains(&r) || (225.0..315.0).contains(&r) {
            (pad.size.1, pad.size.0)
        } else {
            (pad.size.0, pad.size.1)
        }
    }
}

/// A copper trace segment. Traces are stored as straight 2-point
/// segments — polylines and arcs become multiple `Trace`s.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Trace {
    pub id: Id,
    pub layer: CopperLayer,
    pub start: Point,
    pub end: Point,
    pub width: Length,
    pub net: String,
}

/// A through-hole via that joins both copper layers. Phase 5 only models
/// pad-on-via vias (no buried/blind layers); `diameter` is the copper
/// pad diameter, `drill` the hole diameter — annular ring is implicit.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Via {
    pub id: Id,
    pub position: Point,
    pub drill: Length,
    pub diameter: Length,
    pub net: String,
}

/// The board itself.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Board {
    /// Optional rectangular outline. `None` means "not set yet"; the
    /// agent or the human assigns one before manufacturing.
    pub outline: Option<Rect>,
    pub footprints: HashMap<Id, Footprint>,
    /// Insertion order for deterministic rendering and serialisation.
    pub footprint_order: Vec<Id>,
    pub traces: Vec<Trace>,
    pub vias: Vec<Via>,
}

impl Board {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    pub fn add_footprint(&mut self, footprint: Footprint) -> Id {
        let id = footprint.id;
        self.footprint_order.push(id);
        self.footprints.insert(id, footprint);
        id
    }

    pub fn move_footprint(&mut self, id: Id, position: Point) -> bool {
        if let Some(fp) = self.footprints.get_mut(&id) {
            fp.position = position;
            true
        } else {
            false
        }
    }

    pub fn remove_footprint(&mut self, id: Id) -> Option<Footprint> {
        self.footprint_order.retain(|i| *i != id);
        self.footprints.remove(&id)
    }

    #[must_use]
    pub fn footprints_in_order(&self) -> impl Iterator<Item = &Footprint> {
        self.footprint_order
            .iter()
            .filter_map(|id| self.footprints.get(id))
    }

    /// Tight bounding box covering every placed footprint, or `None` for
    /// an empty board.
    #[must_use]
    pub fn content_bounds(&self) -> Option<Rect> {
        let mut iter = self.footprints_in_order().filter_map(Footprint::bounds);
        let first = iter.next()?;
        Some(iter.fold(first, Rect::union))
    }

    pub fn add_trace(&mut self, trace: Trace) -> Id {
        let id = trace.id;
        self.traces.push(trace);
        id
    }

    pub fn add_via(&mut self, via: Via) -> Id {
        let id = via.id;
        self.vias.push(via);
        id
    }

    /// Drop every trace and via on the board. The router uses this
    /// before re-laying the routing — keeps re-routes idempotent.
    pub fn clear_routing(&mut self) {
        self.traces.clear();
        self.vias.clear();
    }

    /// Pairs of footprint references whose pad-derived bounding boxes
    /// intersect on the board. Used as a hard postcondition after the
    /// placer settles: any non-empty result means the layout is invalid
    /// and the user must intervene (rotate, resize the board, drop
    /// components). References are returned sorted within each pair so
    /// the caller can format stable error strings.
    #[must_use]
    pub fn footprint_overlaps(&self) -> Vec<(String, String)> {
        let with_bounds: Vec<(&Footprint, Rect)> = self
            .footprints_in_order()
            .filter_map(|fp| fp.bounds().map(|r| (fp, r)))
            .collect();
        let mut out = Vec::new();
        for i in 0..with_bounds.len() {
            for j in (i + 1)..with_bounds.len() {
                let (a, ar) = with_bounds[i];
                let (b, br) = with_bounds[j];
                if ar.intersects(&br) {
                    let (lo, hi) = if a.reference <= b.reference {
                        (a.reference.clone(), b.reference.clone())
                    } else {
                        (b.reference.clone(), a.reference.clone())
                    };
                    out.push((lo, hi));
                }
            }
        }
        out
    }
}
