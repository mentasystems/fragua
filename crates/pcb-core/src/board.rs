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
}

impl Footprint {
    /// Bounding box of the footprint in board coordinates, derived from
    /// its pads. Rotation is ignored for Phase 1 — we are bbox'ing the
    /// nominal shape; precise rotated bounds come with the routing work.
    #[must_use]
    pub fn bounds(&self) -> Option<Rect> {
        let mut iter = self.pads.iter().map(|pad| {
            let center = self.position.translate(pad.offset.x, pad.offset.y);
            Rect::from_center(center, pad.size.0, pad.size.1)
        });
        let first = iter.next()?;
        Some(iter.fold(first, Rect::union))
    }
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
}
