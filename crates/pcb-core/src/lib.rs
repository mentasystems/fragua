//! `pcb-core` — the in-memory project model.
//!
//! Owns the canonical `Project` (board + design rules + future schematic),
//! the geometry primitives every other crate operates on, and the change
//! event bus that lets the MCP server, the UI, and the router all
//! observe and react to mutations.
//!
//! See `ARCHITECTURE.md` for the full responsibility map.

pub mod board;
pub mod event;
pub mod geometry;
pub mod project;
pub mod schematic;
pub mod units;

pub use board::{Board, CopperLayer, Footprint, Id, Pad};
pub use event::{ActivityLevel, Event, EventBus};
pub use geometry::{Point, Rect};
pub use project::{Project, ProjectSnapshot};
pub use schematic::{Net, NetConnection, PinSide, SchPin, Schematic, Symbol, SymbolKind};
pub use units::{Length, MIL, MM, NM, UM};
