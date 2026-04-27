//! `pcb-gerber` — manufacturing output.
//!
//! Writes the full fab pack from a `Board`:
//! - Gerber RS-274X per copper / mask / silk / paste / edge layer.
//! - Excellon drill files (plated and non-plated holes).
//! - BOM (CSV).
//! - Pick-and-place (CSV).
//!
//! Pure writer. We never parse third-party Gerbers; we only produce them.
