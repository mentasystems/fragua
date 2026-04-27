//! `pcb-core` — the in-memory project model.
//!
//! Owns the canonical `Project` (schematic + board + design rules), the
//! geometry primitives every other crate operates on, and the change-event
//! bus that lets the MCP server, the UI, and the router all observe and
//! react to mutations.
//!
//! This crate is the source of truth. Everything else (router, DRC, gerber,
//! render, MCP) is a function over a `Project` snapshot or a subscriber
//! to its events.
//!
//! See `ARCHITECTURE.md` for the full responsibility map.
