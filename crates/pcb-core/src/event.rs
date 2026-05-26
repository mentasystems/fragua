//! Project change events.
//!
//! The HTTP script API, the Tauri frontend bridge, and any future router/DRC
//! background tasks all subscribe here. A `tokio::sync::broadcast`
//! channel gives us cheap fan-out with backpressure: slow subscribers
//! get `Lagged` errors instead of blocking publishers.

use serde::{Deserialize, Serialize};
use tokio::sync::broadcast;

use crate::board::Id;
use crate::geometry::Point;

/// Anything observable that changes the project.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind")]
pub enum Event {
    /// A new project has been opened or created.
    ProjectChanged,
    /// A footprint was added.
    FootprintAdded { id: Id, reference: String },
    /// A footprint was moved (drag from UI, agent move, router relayout).
    FootprintMoved { id: Id, position: Point },
    /// A footprint was removed.
    FootprintRemoved { id: Id },
    /// The board outline was set or replaced.
    OutlineChanged,
    /// A schematic symbol was added.
    SymbolAdded { id: Id, reference: String },
    /// The connections of a net were set or replaced.
    NetChanged {
        name: String,
        connection_count: usize,
    },
    /// Routing (traces + vias) changed in bulk — typically emitted
    /// after a router pass or a manual clear.
    RoutingChanged {
        trace_count: usize,
        via_count: usize,
    },
    /// A copper pour was added or removed. Carries the resulting pour
    /// count so the UI can re-fetch and re-render.
    PoursChanged { count: usize },
    /// A keep-out polygon was added or removed. Carries the resulting
    /// count so the UI can refresh its overlay layer.
    KeepoutsChanged { count: usize },
    /// Silkscreen content was added or removed. Carries totals so the
    /// UI can refresh without a follow-up fetch.
    SilkChanged {
        line_count: usize,
        text_count: usize,
    },
    /// One frame of an in-progress auto-placement. Streamed several
    /// times per second so the UI can animate components settling.
    PlacementProgress { iteration: u32 },
    /// The palette (footprints declared but not yet placed) was
    /// modified. Includes the current count so the UI can show a
    /// "N components remaining" hint.
    PaletteChanged { count: usize },
    /// The component library (user-driven, persisted to disk) was
    /// modified. Carries the new entry count so the UI can refresh
    /// without an extra fetch.
    LibraryChanged { count: usize },
    /// The pending-library buffer changed (a `library.create` script
    /// call queued a new entry for human review, or a confirm/discard
    /// drained one out). Carries the new pending count so the UI's
    /// confirmation modal can open or close.
    PendingLibraryChanged { count: usize },
    /// Free-form activity log line for the UI's activity panel.
    Activity {
        level: ActivityLevel,
        message: String,
    },
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ActivityLevel {
    Info,
    Warn,
    Error,
}

const CHANNEL_CAPACITY: usize = 256;

/// Multi-producer, multi-consumer event bus.
#[derive(Debug, Clone)]
pub struct EventBus {
    sender: broadcast::Sender<Event>,
}

impl EventBus {
    #[must_use]
    pub fn new() -> Self {
        let (sender, _) = broadcast::channel(CHANNEL_CAPACITY);
        Self { sender }
    }

    /// Publish an event. Errors are silently ignored: an event with no
    /// active subscribers is fine.
    pub fn publish(&self, event: Event) {
        let _ = self.sender.send(event);
    }

    #[must_use]
    pub fn subscribe(&self) -> broadcast::Receiver<Event> {
        self.sender.subscribe()
    }
}

impl Default for EventBus {
    fn default() -> Self {
        Self::new()
    }
}
