//! `Project` — the live, mutable state every other component reads
//! from and writes to.
//!
//! All mutating methods publish an `Event` so subscribers (UI, MCP, the
//! router) see changes regardless of where the change originated.

use std::sync::{Arc, RwLock, RwLockReadGuard};

use crate::board::{Board, Footprint, Id, Trace, Via};
use crate::event::{ActivityLevel, Event, EventBus};
use crate::geometry::Point;
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
}

impl Project {
    #[must_use]
    pub fn new(name: impl Into<String>) -> Self {
        let proj = Self {
            inner: Arc::new(RwLock::new(ProjectInner {
                name: name.into(),
                board: Board::new(),
                schematic: Schematic::new(),
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
}
