//! `pcb-drc` — design rule check.
//!
//! Geometric checks over a `Board`: clearance, track width, drill sizes,
//! via annular ring, edge clearance, unconnected nets. Emits violations
//! with positions so the UI can highlight them and the agent can react.
//!
//! Pure geometry, no shell-out to `kicad-cli`.
