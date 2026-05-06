#![recursion_limit = "256"]
//! `pcb-script` — interpreter that drives a `pcb_core::Project`.
//!
//! One entry point: `tools::dispatch(&project, "script", &args)`. The
//! script is plain text; the language reference lives in
//! `tools::catalog()`. The Tauri host exposes this via a tiny local
//! HTTP API; this crate itself is transport-agnostic.

pub mod script;
pub mod tools;
