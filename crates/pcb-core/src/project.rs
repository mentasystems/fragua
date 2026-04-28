//! `Project` — the live, mutable state every other component reads
//! from and writes to.
//!
//! All mutating methods publish an `Event` so subscribers (UI, MCP, the
//! router) see changes regardless of where the change originated.

use std::sync::{Arc, RwLock, RwLockReadGuard};

use crate::board::{Board, Footprint, Id, Trace, Via};
use crate::event::{ActivityLevel, Event, EventBus};
use crate::geometry::{Point, Rect};
use crate::schematic::{Net, Schematic, Symbol};

/// Cheap-to-clone handle around the shared project state.
///
/// Cloning a `Project` clones the `Arc`s — every clone reads and writes
/// the same underlying board and the same event bus.
#[derive(Debug, Clone)]
pub struct Project {
    inner: Arc<RwLock<ProjectInner>>,
    bus: EventBus,
}

#[derive(Debug, Default)]
struct ProjectInner {
    name: String,
    board: Board,
    schematic: Schematic,
    /// Footprints declared but not yet placed on the board. The UI
    /// shows these in the palette strip; drag-and-drop or
    /// `placement.auto` move them into `board.footprints`.
    palette: Vec<Footprint>,
}

impl Project {
    #[must_use]
    pub fn new(name: impl Into<String>) -> Self {
        let proj = Self {
            inner: Arc::new(RwLock::new(ProjectInner {
                name: name.into(),
                board: Board::new(),
                schematic: Schematic::new(),
                palette: Vec::new(),
            })),
            bus: EventBus::new(),
        };
        proj.bus.publish(Event::ProjectChanged);
        proj
    }

    #[must_use]
    pub fn events(&self) -> &EventBus {
        &self.bus
    }

    /// Activity log helper used everywhere we want the UI's activity
    /// panel to show what happened, with a severity tag.
    pub fn log(&self, level: ActivityLevel, message: impl Into<String>) {
        self.bus.publish(Event::Activity {
            level,
            message: message.into(),
        });
    }

    pub fn read(&self) -> ProjectSnapshot<'_> {
        ProjectSnapshot {
            guard: self.inner.read().expect("project lock poisoned"),
        }
    }

    pub fn add_footprint(&self, footprint: Footprint) -> Id {
        let reference = footprint.reference.clone();
        let id = {
            let mut inner = self.inner.write().expect("project lock poisoned");
            inner.board.add_footprint(footprint)
        };
        self.bus.publish(Event::FootprintAdded { id, reference });
        id
    }

    pub fn move_footprint(&self, id: Id, position: Point) -> bool {
        let moved = {
            let mut inner = self.inner.write().expect("project lock poisoned");
            inner.board.move_footprint(id, position)
        };
        if moved {
            self.bus.publish(Event::FootprintMoved { id, position });
        }
        moved
    }

    pub fn remove_footprint(&self, id: Id) -> bool {
        let removed = {
            let mut inner = self.inner.write().expect("project lock poisoned");
            inner.board.remove_footprint(id).is_some()
        };
        if removed {
            self.bus.publish(Event::FootprintRemoved { id });
        }
        removed
    }

    pub fn set_outline(&self, outline: Rect) {
        {
            let mut inner = self.inner.write().expect("project lock poisoned");
            inner.board.outline = Some(outline);
        }
        self.bus.publish(Event::OutlineChanged);
    }

    pub fn add_trace(&self, trace: Trace) -> Id {
        let id = {
            let mut inner = self.inner.write().expect("project lock poisoned");
            inner.board.add_trace(trace)
        };
        self.publish_routing_counts();
        id
    }

    pub fn add_via(&self, via: Via) -> Id {
        let id = {
            let mut inner = self.inner.write().expect("project lock poisoned");
            inner.board.add_via(via)
        };
        self.publish_routing_counts();
        id
    }

    /// Drop everything: schematic, palette, footprints, traces, vias.
    /// Useful between demo runs so state doesn't accumulate across calls.
    pub fn reset(&self) {
        {
            let mut inner = self.inner.write().expect("project lock poisoned");
            inner.board = Board::new();
            inner.schematic = Schematic::new();
            inner.palette.clear();
        }
        self.bus.publish(Event::ProjectChanged);
    }

    /// Append a footprint to the palette. References must be unique
    /// across palette + board.
    pub fn palette_add(&self, footprint: Footprint) -> Result<(), String> {
        let mut inner = self.inner.write().expect("project lock poisoned");
        let already = inner
            .board
            .footprints
            .values()
            .any(|f| f.reference == footprint.reference)
            || inner.palette.iter().any(|f| f.reference == footprint.reference);
        if already {
            return Err(format!("reference {} already in palette or board", footprint.reference));
        }
        inner.palette.push(footprint);
        let count = inner.palette.len();
        drop(inner);
        self.bus.publish(Event::PaletteChanged { count });
        Ok(())
    }

    pub fn palette_clear(&self) {
        let mut inner = self.inner.write().expect("project lock poisoned");
        inner.palette.clear();
        drop(inner);
        self.bus.publish(Event::PaletteChanged { count: 0 });
    }

    /// Send any board footprint whose body bounding-box pokes outside
    /// the outline back to the palette. Uses the full bbox (not just
    /// the centre) so a component dragged half-way off the board edge
    /// gets reclaimed too.
    pub fn unplace_out_of_bounds(&self) -> Vec<String> {
        let mut moved = Vec::new();
        let mut inner = self.inner.write().expect("project lock poisoned");
        let Some(outline) = inner.board.outline else {
            return moved;
        };
        let ids: Vec<Id> = inner
            .board
            .footprints
            .iter()
            .filter(|(_, fp)| {
                let Some(bbox) = fp.bounds() else { return false; };
                bbox.min.x.0 < outline.min.x.0
                    || bbox.max.x.0 > outline.max.x.0
                    || bbox.min.y.0 < outline.min.y.0
                    || bbox.max.y.0 > outline.max.y.0
            })
            .map(|(id, _)| *id)
            .collect();
        for id in ids {
            if let Some(fp) = inner.board.remove_footprint(id) {
                moved.push(fp.reference.clone());
                inner.palette.push(fp);
            }
        }
        let palette_count = inner.palette.len();
        drop(inner);
        if !moved.is_empty() {
            self.bus.publish(Event::PaletteChanged { count: palette_count });
            for r in &moved {
                // Loose change event so the UI repaints.
                let _ = r;
            }
            self.bus.publish(Event::ProjectChanged);
        }
        moved
    }

    /// Move a palette item onto the board at `position`. The footprint
    /// disappears from the palette. Returns the new board id, or an
    /// error if no palette item with that reference exists.
    pub fn place_from_palette(&self, reference: &str, position: Point) -> Result<Id, String> {
        let mut inner = self.inner.write().expect("project lock poisoned");
        let idx = inner
            .palette
            .iter()
            .position(|f| f.reference == reference)
            .ok_or_else(|| format!("no palette item named {reference}"))?;
        let mut fp = inner.palette.remove(idx);
        fp.position = position;
        let id = inner.board.add_footprint(fp);
        let palette_count = inner.palette.len();
        let reference_owned = reference.to_string();
        drop(inner);
        self.bus.publish(Event::PaletteChanged { count: palette_count });
        self.bus.publish(Event::FootprintAdded {
            id,
            reference: reference_owned,
        });
        Ok(id)
    }

    /// Set the rotation (in degrees, CCW) of a footprint already on
    /// the board, identified by reference.
    pub fn rotate_footprint(&self, reference: &str, rotation_deg: f32) -> Result<Id, String> {
        let mut inner = self.inner.write().expect("project lock poisoned");
        let id = inner
            .board
            .footprints
            .iter_mut()
            .find(|(_, f)| f.reference == reference)
            .map(|(id, fp)| {
                fp.rotation = rotation_deg;
                *id
            })
            .ok_or_else(|| format!("no board footprint named {reference}"))?;
        let position = inner.board.footprints[&id].position;
        drop(inner);
        // Re-emit a "moved" event so the UI re-renders; rotation
        // doesn't have its own event variant yet and adding one would
        // be cosmetic noise.
        self.bus.publish(Event::FootprintMoved { id, position });
        Ok(id)
    }

    /// Move a footprint already on the board to a new position.
    pub fn move_footprint_to(&self, reference: &str, position: Point) -> Result<Id, String> {
        let mut inner = self.inner.write().expect("project lock poisoned");
        let id = inner
            .board
            .footprints
            .iter()
            .find(|(_, f)| f.reference == reference)
            .map(|(id, _)| *id)
            .ok_or_else(|| format!("no board footprint named {reference}"))?;
        if !inner.board.move_footprint(id, position) {
            return Err(format!("move_footprint failed for {reference}"));
        }
        drop(inner);
        self.bus.publish(Event::FootprintMoved { id, position });
        Ok(id)
    }

    /// Drop every trace and via belonging to one net. Returns the
    /// number of items removed.
    pub fn clear_net_routing(&self, net: &str) -> usize {
        let removed = {
            let mut inner = self.inner.write().expect("project lock poisoned");
            let before = inner.board.traces.len() + inner.board.vias.len();
            inner.board.traces.retain(|t| t.net != net);
            inner.board.vias.retain(|v| v.net != net);
            before - (inner.board.traces.len() + inner.board.vias.len())
        };
        if removed > 0 {
            self.publish_routing_counts();
        }
        removed
    }

    /// Remove a single trace by id. Returns whether anything was removed.
    pub fn delete_trace(&self, id: Id) -> bool {
        let removed = {
            let mut inner = self.inner.write().expect("project lock poisoned");
            let before = inner.board.traces.len();
            inner.board.traces.retain(|t| t.id != id);
            inner.board.traces.len() != before
        };
        if removed {
            self.publish_routing_counts();
        }
        removed
    }

    pub fn delete_via(&self, id: Id) -> bool {
        let removed = {
            let mut inner = self.inner.write().expect("project lock poisoned");
            let before = inner.board.vias.len();
            inner.board.vias.retain(|v| v.id != id);
            inner.board.vias.len() != before
        };
        if removed {
            self.publish_routing_counts();
        }
        removed
    }

    /// Drop every trace and via on the board. Used by the router before
    /// re-laying routing on a fresh canvas.
    pub fn clear_routing(&self) {
        {
            let mut inner = self.inner.write().expect("project lock poisoned");
            inner.board.clear_routing();
        }
        self.publish_routing_counts();
    }

    fn publish_routing_counts(&self) {
        let inner = self.inner.read().expect("project lock poisoned");
        self.bus.publish(Event::RoutingChanged {
            trace_count: inner.board.traces.len(),
            via_count: inner.board.vias.len(),
        });
    }

    pub fn add_symbol(&self, symbol: Symbol) -> Id {
        let reference = symbol.reference.clone();
        let id = {
            let mut inner = self.inner.write().expect("project lock poisoned");
            inner.schematic.add_symbol(symbol)
        };
        self.bus.publish(Event::SymbolAdded { id, reference });
        id
    }

    /// Replace the connections on a named net. Returns an error if any
    /// referenced symbol or pin does not exist — keeps the netlist
    /// consistent so downstream tools (router, BOM) never see dangling
    /// references.
    pub fn set_net(&self, net: Net) -> Result<(), String> {
        let mut inner = self.inner.write().expect("project lock poisoned");
        for c in &net.connections {
            let symbol = inner
                .schematic
                .symbols
                .get(&c.symbol_id)
                .ok_or_else(|| format!("unknown symbol {}", c.symbol_id.0))?;
            let valid = symbol
                .kind
                .pins()
                .into_iter()
                .any(|p| p.number == c.pin_number);
            if !valid {
                return Err(format!(
                    "symbol {} has no pin {}",
                    symbol.reference, c.pin_number
                ));
            }
        }
        let count = net.connections.len();
        let name = net.name.clone();
        inner.schematic.set_net(net);
        drop(inner);
        self.bus.publish(Event::NetChanged {
            name,
            connection_count: count,
        });
        Ok(())
    }
}

/// Read-only view of the project, held while the caller is reading.
pub struct ProjectSnapshot<'a> {
    guard: RwLockReadGuard<'a, ProjectInner>,
}

impl ProjectSnapshot<'_> {
    #[must_use]
    pub fn name(&self) -> &str {
        &self.guard.name
    }

    #[must_use]
    pub fn board(&self) -> &Board {
        &self.guard.board
    }

    #[must_use]
    pub fn schematic(&self) -> &Schematic {
        &self.guard.schematic
    }

    #[must_use]
    pub fn palette(&self) -> &[Footprint] {
        &self.guard.palette
    }
}
