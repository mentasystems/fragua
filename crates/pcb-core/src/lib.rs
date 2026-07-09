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
pub mod stitch;
pub mod thermal;
pub mod units;

pub use board::{
    rotate_margin_trbl, Board, CopperLayer, Dielectric, Footprint, FootprintSilk, Id, Keepout,
    Layer, LayerKind, LayerSpec, LayerStackup, Pad, Pour, SilkAnchor, SilkLayer, SilkLine,
    SilkText, StitchPolicy, ThermalRelief, Trace, Via,
};
pub use event::{ActivityLevel, Event, EventBus};
pub use geometry::{Point, Rect};
pub use library::{
    affine_compose, derive_photo_transform, Attachment, BodyRect, Library, LibraryEntry,
    LibraryPad, LibrarySilk, PhotoCalibration, PlacementMargin, SimilarityTransform, ViewTransform,
};
pub use project::{
    DeletedFootprint, FabProfileHandle, PendingAttachment, PendingLibraryEntry, Project,
    ProjectSnapshot,
};
pub use schematic::{
    is_power_named_net, FlatSchematic, Net, NetClass, NetConnection, PinRole, PinSide, Port,
    PortDirection, ResolvedNetRules, SchPin, Schematic, Sheet, Symbol, SymbolKind,
};
pub use units::{Length, MIL, MM, NM, UM};
