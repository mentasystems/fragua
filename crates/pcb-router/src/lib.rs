//! `pcb-router` — autorouting.
//!
//! Takes a `Board` snapshot plus a ratsnest, produces traces and vias,
//! streams progress events back to subscribers so the UI can show routing
//! happening live.
//!
//! Phase 1 target: grid-based A*/Lee on two layers with via cost.
//! Long-term: geometric routing with rip-up-and-retry.
//!
//! No external router binaries, no FreeRouting wrappers — everything here
//! is ours. KiCad and FreeRouting are reference material only.
