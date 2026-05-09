//! `pcb-router` — autorouting.
//!
//! Phase 5 implementation: a 2-layer grid router using A* on a uniform
//! cell grid. Each net is routed as a star from its first pad to every
//! other pad in turn — the simplest correct multi-pad strategy. Layer
//! transitions are paid for with a via-cost penalty; the router will
//! switch sides when a same-layer detour is more expensive than punching
//! through.
//!
//! No external router binaries, no FreeRouting wrappers — everything
//! here is ours. KiCad and FreeRouting are reference material only.
//!
//! Phase 5 limitations (intentional, will lift in later phases):
//! - Single net routed at a time, no rip-up-and-retry. A net that fails
//!   is logged and skipped.
//! - Pad obstacles are inflated by the trace clearance; obstacles from
//!   already-routed traces are honoured cell-by-cell.
//! - Only orthogonal moves on the grid (Manhattan paths).
//! - One global trace width and one via geometry, supplied by the caller.

mod astar;
mod grid;
mod router;

pub use router::{route, NetOverride, Outcome, RouteOptions, RouteReport};
