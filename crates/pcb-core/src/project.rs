//! `Project` — the live, mutable state every other component reads
//! from and writes to.
//!
//! All mutating methods publish an `Event` so subscribers (UI, MCP, the
//! router) see changes regardless of where the change originated.

use std::sync::{Arc, RwLock, RwLockReadGuard};

use crate::board::{Board, Footprint, Id};
use crate::event::{ActivityLevel, Event, EventBus};
use crate::geometry::Point;

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
}

impl Project {
    #[must_use]
    pub fn new(name: impl Into<String>) -> Self {
        let proj = Self {
            inner: Arc::new(RwLock::new(ProjectInner {
                name: name.into(),
                board: Board::new(),
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
}
