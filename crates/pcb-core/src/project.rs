//! `Project` — the live, mutable state every other component reads
//! from and writes to.
//!
//! All mutating methods publish an `Event` so subscribers (UI, MCP, the
//! router) see changes regardless of where the change originated.

use std::collections::VecDeque;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, RwLock, RwLockReadGuard};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

use crate::board::{Board, Footprint, Id, Trace, Via};
use crate::event::{ActivityLevel, Event, EventBus};
use crate::geometry::{Point, Rect};
use crate::units::Length;
use crate::schematic::{Net, Schematic, Symbol};

/// One mutation captured by the project's deferred queue. Each entry
/// carries the FULL data needed to re-apply the change to the
/// `visible` state mirror at animation cadence — so the canvas
/// advances frame-by-frame even though the agent's mutation already
/// landed in `live`. The pump (`tick`) pops one of these per frame,
/// applies it to `visible`, and broadcasts the corresponding `Event`
/// for subscribers (UI, autosave). The agent never observes this
/// queue — it sees `live` immediately.
#[derive(Debug, Clone)]
enum Mutation {
    AddFootprint(Footprint),
    MoveFootprint { id: Id, position: Point },
    RotateFootprint { id: Id, rotation: f32 },
    RemoveFootprint(Id),
    SetOutline(Option<Rect>),
    AddTrace(Trace),
    RemoveTrace(Id),
    AddVia(Via),
    RemoveVia(Id),
    ClearRouting,
    ClearNetRouting { net: String },
    AddSymbol(Symbol),
    SetNet(Net),
    PaletteAdd(Footprint),
    PaletteTake { reference: String },
    PaletteClear,
    ResetBoard,
    Activity { level: ActivityLevel, message: String },
}

/// Cheap-to-clone handle around the shared project state.
///
/// Two parallel states:
///   - `live`: the truth — every mutation lands here immediately, the
///     agent sees it on its next read, no animation lag.
///   - `visible`: mirror that the UI renders. Lags `live` by the
///     animation cadence; advanced frame-by-frame by `tick()` which
///     pops one Mutation and applies it.
///
/// Cloning a `Project` clones the `Arc`s — every clone reads and writes
/// the same underlying state and event bus.
#[derive(Debug, Clone)]
pub struct Project {
    /// Latest committed state. Mutating methods update this
    /// synchronously and queue a `Mutation` for the visible mirror.
    inner: Arc<RwLock<ProjectInner>>,
    /// Animation mirror. Advanced by `tick()`. UI renders from here.
    visible: Arc<RwLock<ProjectInner>>,
    /// FIFO queue of mutations pending replay onto `visible`. The
    /// animation pump drains one per cadence tick.
    pending: Arc<Mutex<VecDeque<Mutation>>>,
    bus: EventBus,
    /// User-driven component library, persisted to `~/.pcb-library/`.
    /// Shared across every `Project` clone so the same in-memory state
    /// (and disk file) backs them all.
    library: Arc<crate::library::Library>,
}

#[derive(Debug, Default)]
struct ProjectInner {
    name: String,
    board: Board,
    schematic: Schematic,
    /// Footprints declared but not yet placed on the board. The UI
    /// shows these in the palette strip; the agent or the human moves
    /// them onto the board.
    palette: Vec<Footprint>,
}

impl Project {
    #[must_use]
    pub fn new(name: impl Into<String>) -> Self {
        // Library failures are non-fatal: if the disk is read-only or
        // the index is corrupt, fall back to a temp-rooted library so
        // the rest of the app still boots. The error is logged on the
        // first activity-level call after construction.
        let library = match crate::library::Library::open_default() {
            Ok(lib) => lib,
            Err(_) => crate::library::Library::open_at(std::env::temp_dir().join("pcb-library"))
                .expect("library: temp fallback also failed"),
        };
        let name: String = name.into();
        // Both live and visible start identical and empty; mutations
        // queue Mutation entries that the pump replays onto visible.
        let make_inner = || ProjectInner {
            name: name.clone(),
            board: Board::new(),
            schematic: Schematic::new(),
            palette: Vec::new(),
        };
        let proj = Self {
            inner: Arc::new(RwLock::new(make_inner())),
            visible: Arc::new(RwLock::new(make_inner())),
            pending: Arc::new(Mutex::new(VecDeque::new())),
            bus: EventBus::new(),
            library: Arc::new(library),
        };
        proj.bus.publish(Event::ProjectChanged);
        proj
    }

    /// Push a mutation onto the deferred queue. Mutations are cheap
    /// (an enum variant + cloned data); the queue is unbounded — if
    /// the agent fires faster than the pump drains, the queue grows
    /// transiently and animation runs longer to catch up.
    fn queue(&self, m: Mutation) {
        self.pending.lock().expect("pending lock poisoned").push_back(m);
    }

    /// Advance the animation mirror by one mutation, if any pending.
    /// Returns `true` if a mutation was applied. The Tauri host calls
    /// this from a background task at the configured cadence (default
    /// 500 ms).
    pub fn tick(&self) -> bool {
        let m = match self.pending.lock().expect("pending lock poisoned").pop_front() {
            Some(m) => m,
            None => return false,
        };
        let mut visible = self.visible.write().expect("visible lock poisoned");
        match m {
            Mutation::AddFootprint(fp) => {
                let id = fp.id;
                let reference = fp.reference.clone();
                visible.board.add_footprint(fp);
                drop(visible);
                self.bus.publish(Event::FootprintAdded { id, reference });
            }
            Mutation::MoveFootprint { id, position } => {
                visible.board.move_footprint(id, position);
                drop(visible);
                self.bus.publish(Event::FootprintMoved { id, position });
            }
            Mutation::RotateFootprint { id, rotation } => {
                if let Some(fp) = visible.board.footprints.get_mut(&id) {
                    fp.rotation = rotation;
                }
                let position = visible.board.footprints.get(&id).map(|fp| fp.position);
                drop(visible);
                if let Some(position) = position {
                    self.bus.publish(Event::FootprintMoved { id, position });
                }
            }
            Mutation::RemoveFootprint(id) => {
                visible.board.remove_footprint(id);
                drop(visible);
                self.bus.publish(Event::FootprintRemoved { id });
            }
            Mutation::SetOutline(outline) => {
                visible.board.outline = outline;
                drop(visible);
                self.bus.publish(Event::OutlineChanged);
            }
            Mutation::AddTrace(trace) => {
                visible.board.add_trace(trace);
                let trace_count = visible.board.traces.len();
                let via_count = visible.board.vias.len();
                drop(visible);
                self.bus.publish(Event::RoutingChanged { trace_count, via_count });
            }
            Mutation::RemoveTrace(id) => {
                visible.board.traces.retain(|t| t.id != id);
                let trace_count = visible.board.traces.len();
                let via_count = visible.board.vias.len();
                drop(visible);
                self.bus.publish(Event::RoutingChanged { trace_count, via_count });
            }
            Mutation::AddVia(via) => {
                visible.board.add_via(via);
                let trace_count = visible.board.traces.len();
                let via_count = visible.board.vias.len();
                drop(visible);
                self.bus.publish(Event::RoutingChanged { trace_count, via_count });
            }
            Mutation::RemoveVia(id) => {
                visible.board.vias.retain(|v| v.id != id);
                let trace_count = visible.board.traces.len();
                let via_count = visible.board.vias.len();
                drop(visible);
                self.bus.publish(Event::RoutingChanged { trace_count, via_count });
            }
            Mutation::ClearRouting => {
                visible.board.clear_routing();
                drop(visible);
                self.bus.publish(Event::RoutingChanged { trace_count: 0, via_count: 0 });
            }
            Mutation::ClearNetRouting { net, .. } => {
                visible.board.traces.retain(|t| t.net != net);
                visible.board.vias.retain(|v| v.net != net);
                let trace_count = visible.board.traces.len();
                let via_count = visible.board.vias.len();
                drop(visible);
                self.bus.publish(Event::RoutingChanged { trace_count, via_count });
            }
            Mutation::AddSymbol(symbol) => {
                let id = symbol.id;
                let reference = symbol.reference.clone();
                visible.schematic.add_symbol(symbol);
                drop(visible);
                self.bus.publish(Event::SymbolAdded { id, reference });
            }
            Mutation::SetNet(net) => {
                let name = net.name.clone();
                let connection_count = net.connections.len();
                let _ = visible.schematic.set_net(net);
                drop(visible);
                self.bus.publish(Event::NetChanged { name, connection_count });
            }
            Mutation::PaletteAdd(fp) => {
                visible.palette.push(fp);
                let count = visible.palette.len();
                drop(visible);
                self.bus.publish(Event::PaletteChanged { count });
            }
            Mutation::PaletteTake { reference } => {
                visible.palette.retain(|f| f.reference != reference);
                let count = visible.palette.len();
                drop(visible);
                self.bus.publish(Event::PaletteChanged { count });
            }
            Mutation::PaletteClear => {
                visible.palette.clear();
                drop(visible);
                self.bus.publish(Event::PaletteChanged { count: 0 });
            }
            Mutation::ResetBoard => {
                let outline = visible.board.outline;
                let salvaged: Vec<Footprint> = visible.board
                    .footprints_in_order()
                    .cloned()
                    .map(|mut fp| {
                        fp.position = Point::new(Length::from_mm(-100.0), Length::from_mm(-100.0));
                        fp
                    })
                    .collect();
                visible.board = Board::new();
                visible.board.outline = outline;
                visible.palette.extend(salvaged);
                drop(visible);
                self.bus.publish(Event::ProjectChanged);
            }
            Mutation::Activity { level, message } => {
                drop(visible);
                self.bus.publish(Event::Activity { level, message });
            }
        }
        true
    }

    /// How many mutations are still queued waiting to animate. UIs can
    /// show this as a "lag" indicator.
    #[must_use]
    pub fn pending_count(&self) -> usize {
        self.pending.lock().expect("pending lock poisoned").len()
    }

    /// Read-only access to the user's component library. Mutations go
    /// through the library's own methods; they're internally locked.
    #[must_use]
    pub fn library(&self) -> &crate::library::Library {
        &self.library
    }

    /// Project name (used as the on-disk directory under
    /// `~/.pcb-projects/`). Borrowing-friendly accessor; the lock is
    /// only held for the time it takes to clone the string.
    #[must_use]
    pub fn name_owned(&self) -> String {
        self.inner.read().expect("project lock poisoned").name.clone()
    }

    #[must_use]
    pub fn events(&self) -> &EventBus {
        &self.bus
    }

    /// Activity log helper used everywhere we want the UI's activity
    /// panel to show what happened, with a severity tag.
    pub fn log(&self, level: ActivityLevel, message: impl Into<String>) {
        self.queue(Mutation::Activity { level, message: message.into() });
    }

    /// Read the LIVE state — the truth, no animation lag. Use for the
    /// agent's read-only tools so it sees the most-recent commit, not
    /// the lagging visible mirror.
    pub fn read(&self) -> ProjectSnapshot<'_> {
        ProjectSnapshot {
            guard: self.inner.read().expect("project lock poisoned"),
        }
    }

    /// Read the VISIBLE mirror — what the UI should render. Lags the
    /// live state by the animation cadence; tracks the order in which
    /// mutations were committed.
    pub fn read_visible(&self) -> ProjectSnapshot<'_> {
        ProjectSnapshot {
            guard: self.visible.read().expect("visible lock poisoned"),
        }
    }

    pub fn add_footprint(&self, footprint: Footprint) -> Id {
        let id = footprint.id;
        {
            let mut inner = self.inner.write().expect("project lock poisoned");
            inner.board.add_footprint(footprint.clone());
        }
        self.queue(Mutation::AddFootprint(footprint));
        id
    }

    pub fn move_footprint(&self, id: Id, position: Point) -> bool {
        let moved = {
            let mut inner = self.inner.write().expect("project lock poisoned");
            inner.board.move_footprint(id, position)
        };
        if moved {
            self.queue(Mutation::MoveFootprint { id, position });
        }
        moved
    }

    pub fn remove_footprint(&self, id: Id) -> bool {
        let removed = {
            let mut inner = self.inner.write().expect("project lock poisoned");
            inner.board.remove_footprint(id).is_some()
        };
        if removed {
            self.queue(Mutation::RemoveFootprint(id));
        }
        removed
    }

    pub fn set_outline(&self, outline: Rect) {
        {
            let mut inner = self.inner.write().expect("project lock poisoned");
            inner.board.outline = Some(outline);
        }
        self.queue(Mutation::SetOutline(Some(outline)));
    }

    pub fn add_trace(&self, trace: Trace) -> Id {
        let id = trace.id;
        {
            let mut inner = self.inner.write().expect("project lock poisoned");
            inner.board.add_trace(trace.clone());
        }
        self.queue(Mutation::AddTrace(trace));
        id
    }

    pub fn add_via(&self, via: Via) -> Id {
        let id = via.id;
        {
            let mut inner = self.inner.write().expect("project lock poisoned");
            inner.board.add_via(via.clone());
        }
        self.queue(Mutation::AddVia(via));
        id
    }

    /// Drop everything: schematic, palette, footprints, traces, vias.
    /// Useful between demo runs so state doesn't accumulate across calls.
    /// Reset is INSTANT in both states (also drains the pending queue) —
    /// otherwise the user's "wipe and start over" would be followed by a
    /// long animation of every previous component being un-added.
    pub fn reset(&self) {
        {
            let mut inner = self.inner.write().expect("project lock poisoned");
            inner.board = Board::new();
            inner.schematic = Schematic::new();
            inner.palette.clear();
        }
        {
            let mut visible = self.visible.write().expect("visible lock poisoned");
            visible.board = Board::new();
            visible.schematic = Schematic::new();
            visible.palette.clear();
        }
        self.pending.lock().expect("pending lock poisoned").clear();
        self.bus.publish(Event::ProjectChanged);
    }

    /// Send every board footprint back into the palette and drop all
    /// routing. The component set is preserved (board ∪ palette stays
    /// constant) — only the *positions* are cleared.
    pub fn reset_board(&self) {
        {
            let mut inner = self.inner.write().expect("project lock poisoned");
            let outline = inner.board.outline;
            let mut salvaged: Vec<Footprint> = inner
                .board
                .footprints_in_order()
                .cloned()
                .collect();
            for fp in salvaged.iter_mut() {
                fp.position = Point::new(Length::from_mm(-100.0), Length::from_mm(-100.0));
            }
            inner.board = Board::new();
            inner.board.outline = outline;
            inner.palette.extend(salvaged);
        }
        self.queue(Mutation::ResetBoard);
    }

    /// Append a footprint to the palette. References must be unique
    /// across palette + board.
    pub fn palette_add(&self, footprint: Footprint) -> Result<(), String> {
        let mut inner = self.inner.write().expect("project lock poisoned");
        let already = inner
            .board
            .footprints
            .values()
            .any(|f| f.reference == footprint.reference)
            || inner.palette.iter().any(|f| f.reference == footprint.reference);
        if already {
            return Err(format!("reference {} already in palette or board", footprint.reference));
        }
        inner.palette.push(footprint.clone());
        drop(inner);
        self.queue(Mutation::PaletteAdd(footprint));
        Ok(())
    }

    pub fn palette_clear(&self) {
        let mut inner = self.inner.write().expect("project lock poisoned");
        inner.palette.clear();
        drop(inner);
        self.queue(Mutation::PaletteClear);
    }

    /// Send any board footprint whose body bounding-box pokes outside
    /// the outline back to the palette. Uses the full bbox (not just
    /// the centre) so a component dragged half-way off the board edge
    /// gets reclaimed too.
    pub fn unplace_out_of_bounds(&self) -> Vec<String> {
        let mut moved_refs = Vec::new();
        let mut moved_pairs: Vec<(Id, Footprint)> = Vec::new();
        let mut inner = self.inner.write().expect("project lock poisoned");
        let Some(outline) = inner.board.outline else {
            return moved_refs;
        };
        let ids: Vec<Id> = inner
            .board
            .footprints
            .iter()
            .filter(|(_, fp)| {
                let Some(bbox) = fp.bounds() else { return false; };
                bbox.min.x.0 < outline.min.x.0
                    || bbox.max.x.0 > outline.max.x.0
                    || bbox.min.y.0 < outline.min.y.0
                    || bbox.max.y.0 > outline.max.y.0
            })
            .map(|(id, _)| *id)
            .collect();
        for id in ids {
            if let Some(fp) = inner.board.remove_footprint(id) {
                moved_refs.push(fp.reference.clone());
                inner.palette.push(fp.clone());
                moved_pairs.push((id, fp));
            }
        }
        drop(inner);
        for (id, fp) in moved_pairs {
            self.queue(Mutation::RemoveFootprint(id));
            self.queue(Mutation::PaletteAdd(fp));
        }
        moved_refs
    }

    /// Move a palette item onto the board at `position`. The footprint
    /// disappears from the palette. Returns the new board id, or an
    /// error if no palette item with that reference exists or if the
    /// proposed bbox would intersect an existing footprint's bbox.
    pub fn place_from_palette(&self, reference: &str, position: Point) -> Result<Id, String> {
        let mut inner = self.inner.write().expect("project lock poisoned");
        let idx = inner
            .palette
            .iter()
            .position(|f| f.reference == reference)
            .ok_or_else(|| format!("no palette item named {reference}"))?;
        let mut fp = inner.palette[idx].clone();
        fp.position = position;
        if let Some(other) = first_overlapper(&inner.board, &fp, None) {
            return Err(format!(
                "{reference} at ({:.2}, {:.2}) mm would overlap {} — pick another position",
                position.x.to_mm(),
                position.y.to_mm(),
                other,
            ));
        }
        if let Some(reason) = edge_violation(&inner.board, &fp) {
            return Err(format!(
                "{reference} is edge-mounted but {reason} — pick a position whose bbox touches the board outline",
            ));
        }
        // Commit: remove from palette + add to board on the live state.
        // Queue the two mutations in the same order so the visible
        // mirror sees palette-take then board-add.
        let mut fp = inner.palette.remove(idx);
        fp.position = position;
        let id = fp.id;
        let added = fp.clone();
        let _ = inner.board.add_footprint(fp);
        drop(inner);
        self.queue(Mutation::PaletteTake { reference: reference.to_string() });
        self.queue(Mutation::AddFootprint(added));
        Ok(id)
    }

    /// Set the rotation (in degrees, CCW) of a footprint already on
    /// the board, identified by reference. Rejected if the rotated
    /// bbox would intersect another footprint's bbox.
    pub fn rotate_footprint(&self, reference: &str, rotation_deg: f32) -> Result<Id, String> {
        let mut inner = self.inner.write().expect("project lock poisoned");
        let id = inner
            .board
            .footprints
            .iter()
            .find(|(_, f)| f.reference == reference)
            .map(|(id, _)| *id)
            .ok_or_else(|| format!("no board footprint named {reference}"))?;
        let mut probe = inner.board.footprints[&id].clone();
        probe.rotation = rotation_deg;
        if let Some(other) = first_overlapper(&inner.board, &probe, Some(id)) {
            return Err(format!(
                "{reference} rotated to {rotation_deg:.0}° would overlap {other}"
            ));
        }
        if let Some(reason) = edge_violation(&inner.board, &probe) {
            return Err(format!(
                "{reference} is edge-mounted but after rotation {reason}",
            ));
        }
        if let Some(fp) = inner.board.footprints.get_mut(&id) {
            fp.rotation = rotation_deg;
        }
        drop(inner);
        self.queue(Mutation::RotateFootprint { id, rotation: rotation_deg });
        Ok(id)
    }

    /// Move a footprint already on the board to a new position.
    /// Rejected if the new bbox would intersect another footprint's bbox.
    pub fn move_footprint_to(&self, reference: &str, position: Point) -> Result<Id, String> {
        let mut inner = self.inner.write().expect("project lock poisoned");
        let id = inner
            .board
            .footprints
            .iter()
            .find(|(_, f)| f.reference == reference)
            .map(|(id, _)| *id)
            .ok_or_else(|| format!("no board footprint named {reference}"))?;
        let mut probe = inner.board.footprints[&id].clone();
        probe.position = position;
        if let Some(other) = first_overlapper(&inner.board, &probe, Some(id)) {
            return Err(format!(
                "moving {reference} to ({:.2}, {:.2}) mm would overlap {other}",
                position.x.to_mm(),
                position.y.to_mm(),
            ));
        }
        if let Some(reason) = edge_violation(&inner.board, &probe) {
            return Err(format!(
                "{reference} is edge-mounted but moving to ({:.2}, {:.2}) mm {reason}",
                position.x.to_mm(),
                position.y.to_mm(),
            ));
        }
        if !inner.board.move_footprint(id, position) {
            return Err(format!("move_footprint failed for {reference}"));
        }
        drop(inner);
        self.queue(Mutation::MoveFootprint { id, position });
        Ok(id)
    }

    /// Drop every trace and via belonging to one net. Returns the
    /// number of items removed.
    pub fn clear_net_routing(&self, net: &str) -> usize {
        let removed = {
            let mut inner = self.inner.write().expect("project lock poisoned");
            let before = inner.board.traces.len() + inner.board.vias.len();
            inner.board.traces.retain(|t| t.net != net);
            inner.board.vias.retain(|v| v.net != net);
            before - (inner.board.traces.len() + inner.board.vias.len())
        };
        if removed > 0 {
            self.queue(Mutation::ClearNetRouting { net: net.to_string() });
        }
        removed
    }

    /// Remove a single trace by id. Returns whether anything was removed.
    pub fn delete_trace(&self, id: Id) -> bool {
        let removed = {
            let mut inner = self.inner.write().expect("project lock poisoned");
            let before = inner.board.traces.len();
            inner.board.traces.retain(|t| t.id != id);
            inner.board.traces.len() != before
        };
        if removed {
            self.queue(Mutation::RemoveTrace(id));
        }
        removed
    }

    pub fn delete_via(&self, id: Id) -> bool {
        let removed = {
            let mut inner = self.inner.write().expect("project lock poisoned");
            let before = inner.board.vias.len();
            inner.board.vias.retain(|v| v.id != id);
            inner.board.vias.len() != before
        };
        if removed {
            self.queue(Mutation::RemoveVia(id));
        }
        removed
    }

    /// Drop every trace and via on the board. Used by the router before
    /// re-laying routing on a fresh canvas.
    pub fn clear_routing(&self) {
        {
            let mut inner = self.inner.write().expect("project lock poisoned");
            inner.board.clear_routing();
        }
        self.queue(Mutation::ClearRouting);
    }

    pub fn add_symbol(&self, symbol: Symbol) -> Id {
        let id = symbol.id;
        {
            let mut inner = self.inner.write().expect("project lock poisoned");
            inner.schematic.add_symbol(symbol.clone());
        }
        self.queue(Mutation::AddSymbol(symbol));
        id
    }

    /// Replace the connections on a named net. Returns an error if any
    /// referenced symbol or pin does not exist — keeps the netlist
    /// consistent so downstream tools (router, BOM) never see dangling
    /// references.
    pub fn set_net(&self, net: Net) -> Result<(), String> {
        let mut inner = self.inner.write().expect("project lock poisoned");
        for c in &net.connections {
            let symbol = inner
                .schematic
                .symbols
                .get(&c.symbol_id)
                .ok_or_else(|| format!("unknown symbol {}", c.symbol_id.0))?;
            let valid = symbol
                .kind
                .pins()
                .into_iter()
                .any(|p| p.number == c.pin_number);
            if !valid {
                return Err(format!(
                    "symbol {} has no pin {}",
                    symbol.reference, c.pin_number
                ));
            }
        }
        inner.schematic.set_net(net.clone());
        drop(inner);
        self.queue(Mutation::SetNet(net));
        Ok(())
    }
}

// ─── Persistence ──────────────────────────────────────────────────────
//
// Auto-save layout (under `~/.pcb-projects/<name>/`):
//   current.json                    ← live state, rewritten after each
//                                     mutation (debounced by the caller)
//   history/<unix_secs>.json        ← rolling snapshot, written every
//                                     N saves; pruned to the latest
//                                     `HISTORY_MAX` entries
//
// JSON is intentional: tiny (a 10-component design fits in a few KB),
// human-inspectable, and forgiving when we add new fields (everything
// is `#[serde(default)]`-friendly).

/// How many snapshots to keep in the history dir.
const HISTORY_MAX: usize = 50;

/// Serialisable mirror of `ProjectInner`. Lives only on disk; the
/// in-memory state is always `ProjectInner`.
#[derive(Debug, Serialize, Deserialize)]
struct ProjectFile {
    name: String,
    board: Board,
    schematic: Schematic,
    #[serde(default)]
    palette: Vec<Footprint>,
}

fn projects_root() -> PathBuf {
    let home = std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."));
    home.join(".pcb-projects")
}

fn now_unix_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

impl Project {
    /// Try to load the named project from `~/.pcb-projects/<name>/current.json`.
    /// Returns `None` if the file doesn't exist OR fails to parse — the
    /// caller falls back to a fresh project. Parse errors are silenced
    /// so a corrupt save doesn't brick the app on boot.
    #[must_use]
    pub fn load_default(name: &str) -> Option<Self> {
        let path = projects_root().join(name).join("current.json");
        let bytes = fs::read(&path).ok()?;
        let file: ProjectFile = serde_json::from_slice(&bytes).ok()?;
        let library = match crate::library::Library::open_default() {
            Ok(lib) => lib,
            Err(_) => crate::library::Library::open_at(std::env::temp_dir().join("pcb-library"))
                .ok()?,
        };
        // On load, both live AND visible start with the saved state —
        // there's no animation of "playing back" the project's history.
        let make_inner = || ProjectInner {
            name: file.name.clone(),
            board: file.board.clone(),
            schematic: file.schematic.clone(),
            palette: file.palette.clone(),
        };
        let proj = Self {
            inner: Arc::new(RwLock::new(make_inner())),
            visible: Arc::new(RwLock::new(make_inner())),
            pending: Arc::new(Mutex::new(VecDeque::new())),
            bus: EventBus::new(),
            library: Arc::new(library),
        };
        proj.bus.publish(Event::ProjectChanged);
        Some(proj)
    }

    /// Write the current state to `~/.pcb-projects/<name>/current.json`.
    /// Atomic via tmp+rename so a crash mid-write can't corrupt the file.
    pub fn save_to_default(&self) -> Result<PathBuf, String> {
        let inner = self.inner.read().expect("project lock poisoned");
        let file = ProjectFile {
            name: inner.name.clone(),
            board: inner.board.clone(),
            schematic: inner.schematic.clone(),
            palette: inner.palette.clone(),
        };
        let dir = projects_root().join(&file.name);
        drop(inner);
        fs::create_dir_all(&dir).map_err(|e| format!("project: mkdir {}: {e}", dir.display()))?;
        let path = dir.join("current.json");
        let tmp = dir.join("current.json.tmp");
        let bytes = serde_json::to_vec_pretty(&file).map_err(|e| format!("project: serialise: {e}"))?;
        fs::write(&tmp, &bytes).map_err(|e| format!("project: write {}: {e}", tmp.display()))?;
        fs::rename(&tmp, &path).map_err(|e| format!("project: rename {}: {e}", path.display()))?;
        Ok(path)
    }

    /// Copy `current.json` into `history/<unix_secs>.json`, then prune
    /// the history dir down to `HISTORY_MAX` newest files. Caller decides
    /// the cadence (typically every Nth save, not every save, so the
    /// history doesn't churn on tiny edits).
    pub fn snapshot_history(&self) -> Result<PathBuf, String> {
        let name = self.name_owned();
        let dir = projects_root().join(&name);
        let history = dir.join("history");
        fs::create_dir_all(&history)
            .map_err(|e| format!("project: mkdir {}: {e}", history.display()))?;
        let src = dir.join("current.json");
        let dst = history.join(format!("{}.json", now_unix_secs()));
        // If `current.json` doesn't exist yet, write the live state
        // directly into the history slot — useful when the very first
        // history snapshot fires before save_to_default has run.
        if let Err(_) = fs::copy(&src, &dst) {
            let inner = self.inner.read().expect("project lock poisoned");
            let file = ProjectFile {
                name: inner.name.clone(),
                board: inner.board.clone(),
                schematic: inner.schematic.clone(),
                palette: inner.palette.clone(),
            };
            drop(inner);
            let bytes = serde_json::to_vec_pretty(&file)
                .map_err(|e| format!("project: serialise: {e}"))?;
            fs::write(&dst, &bytes)
                .map_err(|e| format!("project: write {}: {e}", dst.display()))?;
        }
        prune_history_dir(&history, HISTORY_MAX);
        Ok(dst)
    }

    /// List historical snapshot file paths, newest first.
    #[must_use]
    pub fn history_files(&self) -> Vec<PathBuf> {
        let history = projects_root().join(self.name_owned()).join("history");
        let Ok(entries) = fs::read_dir(&history) else { return Vec::new(); };
        let mut paths: Vec<PathBuf> = entries
            .filter_map(|e| e.ok())
            .filter(|e| e.path().extension().and_then(|s| s.to_str()) == Some("json"))
            .map(|e| e.path())
            .collect();
        paths.sort_by(|a, b| b.file_name().cmp(&a.file_name()));
        paths
    }
}

/// Keep only the newest `keep` files in `dir`, deleting the rest.
fn prune_history_dir(dir: &Path, keep: usize) {
    let Ok(entries) = fs::read_dir(dir) else { return; };
    let mut files: Vec<(PathBuf, std::time::SystemTime)> = entries
        .filter_map(|e| e.ok())
        .filter(|e| e.path().extension().and_then(|s| s.to_str()) == Some("json"))
        .filter_map(|e| {
            let modified = e.metadata().ok()?.modified().ok()?;
            Some((e.path(), modified))
        })
        .collect();
    if files.len() <= keep {
        return;
    }
    files.sort_by(|a, b| b.1.cmp(&a.1));
    for (path, _) in files.iter().skip(keep) {
        let _ = fs::remove_file(path);
    }
}

/// Tolerance in mm: how close a bbox edge must be to a board outline
/// edge for the footprint to count as "touching the edge". Bigger than
/// the trace clearance default so rounding doesn't reject borderline
/// placements.
const EDGE_TOUCH_TOLERANCE_MM: f64 = 0.5;

/// If `probe.edge_mounted` is true, return a human-readable reason
/// when its bbox does NOT touch any side of the board outline. Returns
/// `None` if either edge_mounted is false (no constraint), the board
/// has no outline yet, or at least one bbox side is within tolerance
/// of the matching outline side.
fn edge_violation(board: &Board, probe: &Footprint) -> Option<String> {
    if !probe.edge_mounted {
        return None;
    }
    let outline = board.outline?;
    let bbox = probe.bounds()?;
    let tol_nm = (EDGE_TOUCH_TOLERANCE_MM * 1_000_000.0) as i64;
    let touches_left   = (bbox.min.x.0 - outline.min.x.0).abs() <= tol_nm;
    let touches_right  = (outline.max.x.0 - bbox.max.x.0).abs() <= tol_nm;
    let touches_top    = (bbox.min.y.0 - outline.min.y.0).abs() <= tol_nm;
    let touches_bottom = (outline.max.y.0 - bbox.max.y.0).abs() <= tol_nm;
    if touches_left || touches_right || touches_top || touches_bottom {
        return None;
    }
    let dx_left = (bbox.min.x.0 - outline.min.x.0).abs() as f64 / 1_000_000.0;
    let dx_right = (outline.max.x.0 - bbox.max.x.0).abs() as f64 / 1_000_000.0;
    let dy_top = (bbox.min.y.0 - outline.min.y.0).abs() as f64 / 1_000_000.0;
    let dy_bottom = (outline.max.y.0 - bbox.max.y.0).abs() as f64 / 1_000_000.0;
    let nearest = dx_left.min(dx_right).min(dy_top).min(dy_bottom);
    Some(format!(
        "the bbox is {nearest:.2} mm from the nearest outline edge"
    ))
}

/// Return the reference of the first existing board footprint whose
/// bbox intersects `probe`'s bbox, or `None` if `probe` is clear.
/// `ignore_id` skips a single footprint (useful for move/rotate where
/// the probe is the same footprint at a new pose).
/// Minimum body-to-body clearance between two footprints (mm). Anything
/// closer than this can't be hand-soldered or reworked without disturbing
/// the neighbour, so the placement API rejects it.
const MIN_FOOTPRINT_GAP_MM: f64 = 0.5;

fn first_overlapper(board: &Board, probe: &Footprint, ignore_id: Option<Id>) -> Option<String> {
    let probe_bounds = probe.bounds()?.expand(Length::from_mm(MIN_FOOTPRINT_GAP_MM / 2.0));
    for fp in board.footprints_in_order() {
        if Some(fp.id) == ignore_id {
            continue;
        }
        if let Some(b) = fp.bounds() {
            let inflated = b.expand(Length::from_mm(MIN_FOOTPRINT_GAP_MM / 2.0));
            if probe_bounds.intersects(&inflated) {
                return Some(fp.reference.clone());
            }
        }
    }
    None
}

/// Read-only view of the project, held while the caller is reading.
pub struct ProjectSnapshot<'a> {
    guard: RwLockReadGuard<'a, ProjectInner>,
}

impl ProjectSnapshot<'_> {
    #[must_use]
    pub fn name(&self) -> &str {
        &self.guard.name
    }

    #[must_use]
    pub fn board(&self) -> &Board {
        &self.guard.board
    }

    #[must_use]
    pub fn schematic(&self) -> &Schematic {
        &self.guard.schematic
    }

    #[must_use]
    pub fn palette(&self) -> &[Footprint] {
        &self.guard.palette
    }
}
