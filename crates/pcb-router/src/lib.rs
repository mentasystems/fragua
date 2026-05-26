//! `pcb-router` — autorouting.
//!
//! 2-layer grid router using Theta* (any-angle) on a uniform cell grid.
//! Nets are grown Prim-style from a seed pad; once a trace exists,
//! every cell on it is a valid source for the next spoke, so the tree
//! shares trunks instead of fanning out as a strict star. Layer
//! transitions are paid for with a via-cost penalty; the router will
//! switch sides when a same-layer detour is more expensive than
//! punching through.
//!
//! No external router binaries, no `FreeRouting` wrappers — everything
//! here is ours. `KiCad` and `FreeRouting` are reference material only.

mod astar;
mod grid;
mod router;

pub use router::{route, NetOverride, Outcome, RouteOptions, RouteReport};
