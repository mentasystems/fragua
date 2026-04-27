//! `pcb-gerber` — manufacturing output.
//!
//! Writes the full fab pack from a `Board`:
//! - Gerber RS-274X per copper / mask / edge layer.
//! - Excellon drill files (plated and non-plated; stub until we model
//!   through-holes and vias).
//! - BOM (CSV).
//! - Pick-and-place (CSV).
//!
//! Pure writer. We never parse third-party Gerbers; we only produce them.

pub mod bom;
pub mod bundle;
pub mod excellon;
pub mod gerber;
pub mod pick_place;

pub use bundle::write_fab_pack;
pub use gerber::Side;
