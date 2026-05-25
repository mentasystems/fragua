//! `pcb-core` — the in-memory project model.
//!
//! Owns the canonical `Project` (board + design rules + future schematic),
//! the geometry primitives every other crate operates on, and the change
//! event bus that lets the HTTP script API, the UI, and the router all
//! observe and react to mutations.
//!
//! See `ARCHITECTURE.md` for the full responsibility map.

pub mod board;
pub mod event;
pub mod geometry;
pub mod hershey;
pub mod library;
pub mod project;
pub mod schematic;
pub mod silk_clip;
pub mod units;

pub use board::{
    rotate_margin_trbl, Board, CopperLayer, Footprint, FootprintSilk, Id, Pad, Pour, SilkAnchor,
    SilkLayer, SilkLine, SilkText, Trace, Via,
};
pub use event::{ActivityLevel, Event, EventBus};
pub use geometry::{Point, Rect};
pub use library::{
    Attachment, Library, LibraryEntry, LibraryPad, LibrarySilk, PlacementMargin, ViewTransform,
};
pub use project::{
    DeletedFootprint, PendingAttachment, PendingLibraryEntry, Project, ProjectSnapshot,
};
pub use schematic::{
    Net, NetClass, NetConnection, PinRole, PinSide, SchPin, Schematic, Symbol, SymbolKind,
};
pub use units::{Length, MIL, MM, NM, UM};
