//! `Project` — the live, mutable state every other component reads
//! from and writes to.
//!
//! All mutating methods publish an `Event` so subscribers (UI, the
//! HTTP script API, the router) see changes regardless of where the
//! change originated.

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Arc, RwLock, RwLockReadGuard};

use serde::{Deserialize, Serialize};

use crate::board::{
    Board, CopperLayer, Footprint, Id, Keepout, Pour, SilkLine, SilkText, Trace, Via,
};
use crate::event::{ActivityLevel, Event, EventBus};
use crate::geometry::{Point, Rect};
use crate::schematic::{Net, Schematic, Symbol};
use crate::units::Length;

/// Cheap-to-clone handle around the shared project state. Mutations
/// land synchronously on `inner` and the matching `Event` is published
/// on the bus immediately — UI subscribers see whatever the agent did
/// at the speed at which the agent did it.
///
/// Cloning a `Project` clones the `Arc`s — every clone reads and writes
/// the same underlying state and event bus.
#[derive(Debug, Clone)]
pub struct Project {
    inner: Arc<RwLock<ProjectInner>>,
    bus: EventBus,
    /// User-driven component library, persisted to `~/.pcb-library/`.
    /// Shared across every `Project` clone so the same in-memory state
    /// (and disk file) backs them all.
    library: Arc<crate::library::Library>,
    /// Where the autosave loop writes. `None` means "no autosave"
    /// (memory-only session). `save_to_path` updates this so a manual
    /// save also rebinds the autosave target.
    save_path: Arc<RwLock<Option<PathBuf>>>,
    /// Library entries the script API created but a human hasn't
    /// confirmed yet. Lives in memory only — the agent populates this
    /// via `library.create` and the UI either promotes the entry into
    /// the disk-backed `library` (confirm) or drops it (discard).
    /// Shared across every `Project` clone so the confirm modal sees
    /// the same buffer the script just pushed into.
    pending_library: Arc<RwLock<Vec<PendingLibraryEntry>>>,
    /// Currently adopted fab capability profile, if any. DRC honors
    /// this via `DrcOptions::fab_profile`. Lives in memory only —
    /// not persisted to the project JSON (the agent re-adopts it
    /// each session via `fab profile <name>`).
    fab_profile: Arc<RwLock<Option<FabProfileHandle>>>,
}

/// Cheap handle around a fab profile so the project can share it with
/// any number of DRC runs without re-cloning the inner struct. Boxed
/// to avoid pulling `pcb-drc` types into the `Project` struct's public
/// signature beyond this opaque-ish handle.
#[derive(Debug, Clone)]
pub struct FabProfileHandle {
    pub name: String,
    pub min_trace_width_mm: f64,
    pub min_clearance_mm: f64,
    pub min_drill_mm: f64,
    pub min_annular_ring_mm: f64,
    pub min_via_diameter_mm: f64,
    pub min_edge_clearance_mm: f64,
    pub max_board_size_mm: (f64, f64),
}

/// An in-flight library entry waiting for human review. Carries the
/// fully-formed `LibraryEntry` plus any binary attachments the agent
/// uploaded alongside it (typically a single component photo). The
/// attachments live in memory until confirmation; on confirm they are
/// written through `Library::attach` so the on-disk store ends up in
/// the same shape it would have had if `library.create` saved
/// directly.
#[derive(Debug, Clone)]
pub struct PendingLibraryEntry {
    pub entry: crate::library::LibraryEntry,
    pub attachments: Vec<PendingAttachment>,
}

#[derive(Debug, Clone)]
pub struct PendingAttachment {
    pub kind: String,
    pub filename: String,
    pub mime: String,
    pub data: Vec<u8>,
}

/// Summary returned by `Project::delete_footprint_by_ref`. The script
/// layer formats this into the human-facing reply; UI consumers can
/// pipe it straight into a toast.
#[derive(Debug, Clone)]
pub struct DeletedFootprint {
    pub id: Id,
    pub reference: String,
    pub library: String,
    pub key: String,
    pub pad_count: usize,
    pub traces_removed: usize,
    pub vias_removed: usize,
    /// Total trace count remaining on the board after the delete.
    pub trace_count: usize,
    /// Total via count remaining on the board after the delete.
    pub via_count: usize,
    /// Nets whose only pads were on the removed footprint — surfaced as
    /// a warning so the caller knows the netlist is now incomplete.
    pub orphaned_nets: Vec<String>,
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
        let proj = Self {
            inner: Arc::new(RwLock::new(ProjectInner {
                name: name.into(),
                board: Board::new(),
                schematic: Schematic::new(),
                palette: Vec::new(),
            })),
            bus: EventBus::new(),
            library: Arc::new(library),
            save_path: Arc::new(RwLock::new(None)),
            pending_library: Arc::new(RwLock::new(Vec::new())),
            fab_profile: Arc::new(RwLock::new(None)),
        };
        proj.bus.publish(Event::ProjectChanged);
        proj
    }

    /// Snapshot of the pending-library buffer. Clones the entries; the
    /// UI uses this to populate the confirmation modal.
    #[must_use]
    pub fn pending_library_entries(&self) -> Vec<PendingLibraryEntry> {
        self.pending_library
            .read()
            .expect("pending_library lock poisoned")
            .clone()
    }

    #[must_use]
    pub fn find_pending_library_entry(&self, key: &str) -> Option<PendingLibraryEntry> {
        self.pending_library
            .read()
            .expect("pending_library lock poisoned")
            .iter()
            .find(|p| p.entry.key == key)
            .cloned()
    }

    /// Queue a library entry for human review. The agent calls this
    /// instead of writing straight to the disk-backed `Library` so a
    /// mirrored / mis-pinned footprint never reaches fab. Returns the
    /// number of pending entries after insertion. If an entry with the
    /// same key is already pending, it is replaced — the agent can
    /// iterate without leaking ghosts.
    pub fn queue_pending_library_entry(&self, pending: PendingLibraryEntry) -> usize {
        let key = pending.entry.key.clone();
        let count = {
            let mut buf = self
                .pending_library
                .write()
                .expect("pending_library lock poisoned");
            if let Some(idx) = buf.iter().position(|p| p.entry.key == key) {
                buf[idx] = pending;
            } else {
                buf.push(pending);
            }
            buf.len()
        };
        self.bus.publish(Event::PendingLibraryChanged { count });
        count
    }

    /// Promote a pending entry to the on-disk library. Writes any
    /// staged attachments through `Library::attach`. Returns `true` if
    /// the key was found and persisted, `false` if it was no longer in
    /// the buffer (e.g. discarded racing with confirm).
    pub fn confirm_pending_library_entry(&self, key: &str) -> Result<bool, String> {
        let pending = {
            let mut buf = self
                .pending_library
                .write()
                .expect("pending_library lock poisoned");
            let Some(idx) = buf.iter().position(|p| p.entry.key == key) else {
                return Ok(false);
            };
            buf.remove(idx)
        };
        let stored = self.library.upsert(pending.entry)?;
        for att in pending.attachments {
            // Attachment failures are surfaced — the entry is already
            // saved by upsert; the caller can re-attach if needed.
            self.library
                .attach(&stored.key, att.kind, att.filename, att.mime, &att.data)?;
        }
        let lib_count = self.library.list().len();
        self.bus.publish(Event::LibraryChanged { count: lib_count });
        let pending_count = self
            .pending_library
            .read()
            .expect("pending_library lock poisoned")
            .len();
        self.bus.publish(Event::PendingLibraryChanged {
            count: pending_count,
        });
        Ok(true)
    }

    /// Drop a pending entry without persisting. Returns `true` if the
    /// key was found, `false` if it was already gone.
    pub fn discard_pending_library_entry(&self, key: &str) -> bool {
        let removed = {
            let mut buf = self
                .pending_library
                .write()
                .expect("pending_library lock poisoned");
            let Some(idx) = buf.iter().position(|p| p.entry.key == key) else {
                return false;
            };
            buf.remove(idx);
            true
        };
        if removed {
            let pending_count = self
                .pending_library
                .read()
                .expect("pending_library lock poisoned")
                .len();
            self.bus.publish(Event::PendingLibraryChanged {
                count: pending_count,
            });
        }
        removed
    }

    /// Read-only access to the user's component library. Mutations go
    /// through the library's own methods; they're internally locked.
    #[must_use]
    pub fn library(&self) -> &crate::library::Library {
        &self.library
    }

    /// Publish a `LibraryChanged` event with the current entry count.
    /// Called by hosts (Tauri commands, etc.) after they mutate the
    /// library directly so the frontend refetches its review pane.
    pub fn notify_library_changed(&self) {
        let count = self.library.list().len();
        self.bus.publish(Event::LibraryChanged { count });
    }

    /// Project name (used as the on-disk directory under
    /// `~/.pcb-projects/`). Borrowing-friendly accessor; the lock is
    /// only held for the time it takes to clone the string.
    #[must_use]
    pub fn name_owned(&self) -> String {
        self.inner
            .read()
            .expect("project lock poisoned")
            .name
            .clone()
    }

    #[must_use]
    pub fn events(&self) -> &EventBus {
        &self.bus
    }

    /// Activity log helper used everywhere we want the UI's activity
    /// panel to show what happened, with a severity tag.
    pub fn log(&self, level: ActivityLevel, message: impl Into<String>) {
        self.bus.publish(Event::Activity {
            level,
            message: message.into(),
        });
    }

    /// Add (or replace by reference) a sub-sheet under the top-level
    /// schematic. Returns `Err` if a sheet with the same reference
    /// already exists with conflicting content; idempotent otherwise.
    pub fn add_sub_sheet(&self, sheet: crate::Sheet) -> Result<(), String> {
        let mut inner = self.inner.write().expect("project lock poisoned");
        if inner
            .schematic
            .sub_sheets
            .iter()
            .any(|s| s.reference == sheet.reference)
        {
            return Err(format!(
                "sheet `{}` already exists at the top level",
                sheet.reference
            ));
        }
        inner.schematic.add_sub_sheet(sheet);
        Ok(())
    }

    /// Declare or replace a port on the named sub-sheet (or the
    /// top-level schematic when `sheet_ref` is empty).
    pub fn set_sheet_port(&self, sheet_ref: &str, port: crate::Port) -> Result<(), String> {
        let mut inner = self.inner.write().expect("project lock poisoned");
        if sheet_ref.is_empty() {
            inner.schematic.set_port(port);
            return Ok(());
        }
        let sheet = inner
            .schematic
            .sub_sheets
            .iter_mut()
            .find(|s| s.reference == sheet_ref)
            .ok_or_else(|| format!("no sub-sheet `{sheet_ref}` at the top level"))?;
        sheet.schematic.set_port(port);
        Ok(())
    }

    /// Bind a port on a top-level sub-sheet to a parent net.
    pub fn bind_sheet_port(
        &self,
        sheet_ref: &str,
        port_name: &str,
        parent_net: &str,
    ) -> Result<(), String> {
        let mut inner = self.inner.write().expect("project lock poisoned");
        let sheet = inner
            .schematic
            .sub_sheets
            .iter_mut()
            .find(|s| s.reference == sheet_ref)
            .ok_or_else(|| format!("no sub-sheet `{sheet_ref}` at the top level"))?;
        sheet
            .port_bindings
            .insert(port_name.to_string(), parent_net.to_string());
        Ok(())
    }

    /// Update one or more stackup fields. Empty `updater` is a no-op.
    pub fn update_stackup(&self, mut updater: impl FnMut(&mut crate::LayerStackup)) {
        let mut inner = self.inner.write().expect("project lock poisoned");
        updater(&mut inner.board.stackup);
    }

    /// Currently adopted fab capability profile (cloned). `None` if
    /// no profile is set — the agent runs `fab profile <name>` to
    /// adopt one. DRC honors the profile on every subsequent run.
    #[must_use]
    pub fn fab_profile(&self) -> Option<FabProfileHandle> {
        self.fab_profile
            .read()
            .expect("fab_profile lock poisoned")
            .clone()
    }

    /// Adopt a fab profile. `None` clears it.
    pub fn set_fab_profile(&self, profile: Option<FabProfileHandle>) {
        *self.fab_profile.write().expect("fab_profile lock poisoned") = profile;
    }

    /// Flatten the schematic's sheet tree into a single namespace.
    /// Convenience wrapper that snapshots the schematic and calls
    /// [`Schematic::flatten`]. The router and DRC consume the FLAT
    /// schematic — they don't see the hierarchy.
    #[must_use]
    pub fn flatten_schematic(&self) -> crate::FlatSchematic {
        self.inner
            .read()
            .expect("project lock poisoned")
            .schematic
            .flatten()
    }

    /// Read the project state.
    pub fn read(&self) -> ProjectSnapshot<'_> {
        ProjectSnapshot {
            guard: self.inner.read().expect("project lock poisoned"),
        }
    }

    pub fn add_footprint(&self, footprint: Footprint) -> Id {
        let id = footprint.id;
        let reference = footprint.reference.clone();
        {
            let mut inner = self.inner.write().expect("project lock poisoned");
            inner.board.add_footprint(footprint);
        }
        self.bus.publish(Event::FootprintAdded { id, reference });
        id
    }

    pub fn move_footprint(&self, id: Id, position: Point) -> bool {
        let moved = {
            let mut inner = self.inner.write().expect("project lock poisoned");
            inner.board.move_footprint(id, position)
        };
        if moved {
            self.bus.publish(Event::FootprintMoved { id, position });
        }
        moved
    }

    /// Set a footprint's rotation by id, bypassing the overlap and
    /// edge-mount probe that `rotate_footprint` runs. Used by trusted
    /// callers (the auto-placer) that have already validated the full
    /// final placement: a ref-by-ref re-check against the LIVE state
    /// would falsely reject intermediate steps where two parts haven't
    /// landed yet.
    pub fn set_footprint_rotation(&self, id: Id, rotation_deg: f32) -> bool {
        let mut inner = self.inner.write().expect("project lock poisoned");
        let position = if let Some(fp) = inner.board.footprints.get_mut(&id) {
            fp.rotation = rotation_deg;
            fp.position
        } else {
            return false;
        };
        drop(inner);
        self.bus.publish(Event::FootprintMoved { id, position });
        true
    }

    pub fn remove_footprint(&self, id: Id) -> bool {
        let removed = {
            let mut inner = self.inner.write().expect("project lock poisoned");
            inner.board.remove_footprint(id).is_some()
        };
        if removed {
            self.bus.publish(Event::FootprintRemoved { id });
        }
        removed
    }

    /// Outcome of `delete_footprint_by_ref`. Carries enough detail for the
    /// script handler to produce a useful reply (number of pads, traces
    /// and vias cleared) without re-walking the board.
    ///
    /// Lives on `Project` rather than `tools.rs` so other consumers (UI,
    /// future scripting layers) can reuse the same structure.
    #[allow(dead_code)]
    pub fn delete_footprint_by_ref(&self, reference: &str) -> Result<DeletedFootprint, String> {
        let outcome = {
            let mut inner = self.inner.write().expect("project lock poisoned");
            let id = inner
                .board
                .footprints
                .iter()
                .find(|(_, f)| f.reference == reference)
                .map(|(id, _)| *id)
                .ok_or_else(|| format!("no footprint with ref '{reference}'"))?;
            inner
                .board
                .remove_footprint_and_routing(id)
                .map(|(fp, traces, vias, orphans)| DeletedFootprint {
                    id,
                    reference: fp.reference.clone(),
                    library: fp.library.clone(),
                    key: fp.key.clone(),
                    pad_count: fp.pads.len(),
                    traces_removed: traces,
                    vias_removed: vias,
                    trace_count: inner.board.traces.len(),
                    via_count: inner.board.vias.len(),
                    orphaned_nets: orphans,
                })
                .ok_or_else(|| format!("footprint with ref '{reference}' vanished mid-delete"))?
        };
        self.bus.publish(Event::FootprintRemoved { id: outcome.id });
        if outcome.traces_removed > 0 || outcome.vias_removed > 0 {
            self.bus.publish(Event::RoutingChanged {
                trace_count: outcome.trace_count,
                via_count: outcome.via_count,
            });
        }
        Ok(outcome)
    }

    /// Drop every placed footprint AND all routing. Outline / silk /
    /// schematic / library are preserved. Returns the references of the
    /// footprints that were removed (in board insertion order) so the
    /// caller can echo them back.
    pub fn clear_board_placements(&self) -> Vec<String> {
        let (removed_refs, removed_ids) = {
            let mut inner = self.inner.write().expect("project lock poisoned");
            let ordered: Vec<Id> = inner.board.footprint_order.clone();
            let mut refs = Vec::with_capacity(ordered.len());
            let mut ids = Vec::with_capacity(ordered.len());
            for id in &ordered {
                if let Some(fp) = inner.board.remove_footprint(*id) {
                    refs.push(fp.reference);
                    ids.push(*id);
                }
            }
            inner.board.clear_routing();
            (refs, ids)
        };
        for id in removed_ids {
            self.bus.publish(Event::FootprintRemoved { id });
        }
        if !removed_refs.is_empty() {
            self.bus.publish(Event::RoutingChanged {
                trace_count: 0,
                via_count: 0,
            });
        }
        removed_refs
    }

    pub fn set_outline(&self, outline: Rect) {
        self.set_outline_with_radius(outline, crate::Length::ZERO);
    }

    /// Set the rectangular outline plus a corner radius. Radius is
    /// clamped to half the shorter side so the resulting shape is
    /// always a valid closed rounded rectangle (a radius wider than
    /// half the board would degenerate the geometry).
    pub fn set_outline_with_radius(&self, outline: Rect, corner_radius: crate::Length) {
        let cap = (outline.width().0.min(outline.height().0)) / 2;
        let radius_clamped = crate::Length(corner_radius.0.max(0).min(cap));
        {
            let mut inner = self.inner.write().expect("project lock poisoned");
            inner.board.outline = Some(outline);
            inner.board.outline_corner_radius = radius_clamped;
        }
        self.bus.publish(Event::OutlineChanged);
    }

    pub fn add_trace(&self, trace: Trace) -> Id {
        let id = trace.id;
        let (trace_count, via_count) = {
            let mut inner = self.inner.write().expect("project lock poisoned");
            inner.board.add_trace(trace);
            (inner.board.traces.len(), inner.board.vias.len())
        };
        self.bus.publish(Event::RoutingChanged {
            trace_count,
            via_count,
        });
        id
    }

    pub fn add_via(&self, via: Via) -> Id {
        let id = via.id;
        let (trace_count, via_count) = {
            let mut inner = self.inner.write().expect("project lock poisoned");
            inner.board.add_via(via);
            (inner.board.traces.len(), inner.board.vias.len())
        };
        self.bus.publish(Event::RoutingChanged {
            trace_count,
            via_count,
        });
        id
    }

    /// Add a copper pour. Replaces any existing pour for the same
    /// `(net, layer)` so a second call is idempotent.
    pub fn add_pour(&self, pour: Pour) {
        let count = {
            let mut inner = self.inner.write().expect("project lock poisoned");
            inner.board.add_pour(pour);
            inner.board.pours.len()
        };
        self.bus.publish(Event::PoursChanged { count });
    }

    /// Append a silk line to the board.
    pub fn add_silk_line(&self, line: SilkLine) {
        let (line_count, text_count) = {
            let mut inner = self.inner.write().expect("project lock poisoned");
            inner.board.add_silk_line(line);
            (inner.board.silk_lines.len(), inner.board.silk_texts.len())
        };
        self.bus.publish(Event::SilkChanged {
            line_count,
            text_count,
        });
    }

    /// Append a silk text item to the board.
    pub fn add_silk_text(&self, text: SilkText) {
        let (line_count, text_count) = {
            let mut inner = self.inner.write().expect("project lock poisoned");
            inner.board.add_silk_text(text);
            (inner.board.silk_lines.len(), inner.board.silk_texts.len())
        };
        self.bus.publish(Event::SilkChanged {
            line_count,
            text_count,
        });
    }

    /// Replace every board-level silk text wholesale. Used by the
    /// compaction pass to commit labels that were pulled inside a
    /// shrunk outline. Publishes a single `SilkChanged` event.
    pub fn set_silk_texts(&self, texts: Vec<SilkText>) {
        let (line_count, text_count) = {
            let mut inner = self.inner.write().expect("project lock poisoned");
            inner.board.silk_texts = texts;
            (inner.board.silk_lines.len(), inner.board.silk_texts.len())
        };
        self.bus.publish(Event::SilkChanged {
            line_count,
            text_count,
        });
    }

    /// Update the thermal relief style of every existing pour on
    /// `net`. Returns the number of pours updated. No-op if the net
    /// has no pours.
    pub fn set_pour_relief(&self, net: &str, relief: crate::board::ThermalRelief) -> usize {
        let (changed, count) = {
            let mut inner = self.inner.write().expect("project lock poisoned");
            let mut changed = 0_usize;
            for p in &mut inner.board.pours {
                if p.net == net {
                    p.thermal_relief = relief;
                    changed += 1;
                }
            }
            (changed, inner.board.pours.len())
        };
        if changed > 0 {
            self.bus.publish(Event::PoursChanged { count });
        }
        changed
    }

    /// Update the stitching policy of every existing pour on `net`.
    /// Returns the number of pours updated.
    pub fn set_pour_stitching(&self, net: &str, policy: crate::board::StitchPolicy) -> usize {
        let (changed, count) = {
            let mut inner = self.inner.write().expect("project lock poisoned");
            let mut changed = 0_usize;
            for p in &mut inner.board.pours {
                if p.net == net {
                    p.stitching = policy;
                    changed += 1;
                }
            }
            (changed, inner.board.pours.len())
        };
        if changed > 0 {
            self.bus.publish(Event::PoursChanged { count });
        }
        changed
    }

    /// Add a polygonal keepout. Returns its id and publishes
    /// `KeepoutsChanged`.
    pub fn add_keepout(&self, keepout: Keepout) -> Id {
        let (id, count) = {
            let mut inner = self.inner.write().expect("project lock poisoned");
            let id = inner.board.add_keepout(keepout);
            (id, inner.board.keepouts.len())
        };
        self.bus.publish(Event::KeepoutsChanged { count });
        id
    }

    /// Remove a keepout by id. Returns whether anything was removed.
    pub fn remove_keepout(&self, id: Id) -> bool {
        let (removed, count) = {
            let mut inner = self.inner.write().expect("project lock poisoned");
            let removed = inner.board.remove_keepout(id);
            (removed, inner.board.keepouts.len())
        };
        if removed {
            self.bus.publish(Event::KeepoutsChanged { count });
        }
        removed
    }

    /// Remove a pour by `(net, layer)`. Returns true if one was removed.
    pub fn remove_pour(&self, net: &str, layer: CopperLayer) -> bool {
        let (removed, count) = {
            let mut inner = self.inner.write().expect("project lock poisoned");
            let removed = inner.board.remove_pour(net, layer);
            (removed, inner.board.pours.len())
        };
        if removed {
            self.bus.publish(Event::PoursChanged { count });
        }
        removed
    }

    /// Drop everything: schematic, palette, footprints, traces, vias.
    /// Useful between demo runs so state doesn't accumulate across calls.
    pub fn reset(&self) {
        {
            let mut inner = self.inner.write().expect("project lock poisoned");
            inner.board = Board::new();
            inner.schematic = Schematic::new();
            inner.palette.clear();
        }
        self.bus.publish(Event::ProjectChanged);
    }

    /// Send every board footprint back into the palette and drop all
    /// routing. The component set is preserved (board ∪ palette stays
    /// constant) — only the *positions* are cleared.
    pub fn reset_board(&self) {
        {
            let mut inner = self.inner.write().expect("project lock poisoned");
            let outline = inner.board.outline;
            let mut salvaged: Vec<Footprint> = inner.board.footprints_in_order().cloned().collect();
            for fp in &mut salvaged {
                fp.position = Point::new(Length::from_mm(-100.0), Length::from_mm(-100.0));
            }
            inner.board = Board::new();
            inner.board.outline = outline;
            inner.palette.extend(salvaged);
        }
        self.bus.publish(Event::ProjectChanged);
    }

    /// Append a footprint to the palette. References must be unique
    /// across palette + board.
    pub fn palette_add(&self, footprint: Footprint) -> Result<(), String> {
        let count = {
            let mut inner = self.inner.write().expect("project lock poisoned");
            let already = inner
                .board
                .footprints
                .values()
                .any(|f| f.reference == footprint.reference)
                || inner
                    .palette
                    .iter()
                    .any(|f| f.reference == footprint.reference);
            if already {
                return Err(format!(
                    "reference {} already in palette or board",
                    footprint.reference
                ));
            }
            inner.palette.push(footprint);
            inner.palette.len()
        };
        self.bus.publish(Event::PaletteChanged { count });
        Ok(())
    }

    pub fn palette_clear(&self) {
        {
            let mut inner = self.inner.write().expect("project lock poisoned");
            inner.palette.clear();
        }
        self.bus.publish(Event::PaletteChanged { count: 0 });
    }

    /// Send any board footprint whose body bounding-box pokes outside
    /// the outline back to the palette. Uses the full bbox (not just
    /// the centre) so a component dragged half-way off the board edge
    /// gets reclaimed too.
    pub fn unplace_out_of_bounds(&self) -> Vec<String> {
        let mut moved_refs = Vec::new();
        let mut moved_ids: Vec<Id> = Vec::new();
        let palette_count;
        {
            let mut inner = self.inner.write().expect("project lock poisoned");
            let Some(outline) = inner.board.outline else {
                return moved_refs;
            };
            let ids: Vec<Id> = inner
                .board
                .footprints
                .iter()
                .filter(|(_, fp)| {
                    let Some(bbox) = fp.bounds() else {
                        return false;
                    };
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
                    inner.palette.push(fp);
                    moved_ids.push(id);
                }
            }
            palette_count = inner.palette.len();
        }
        for id in moved_ids {
            self.bus.publish(Event::FootprintRemoved { id });
        }
        if !moved_refs.is_empty() {
            self.bus.publish(Event::PaletteChanged {
                count: palette_count,
            });
        }
        moved_refs
    }

    /// Move a palette item onto the board at `position`. The footprint
    /// disappears from the palette. Returns the new board id, or an
    /// error if no palette item with that reference exists or if the
    /// proposed bbox would intersect an existing footprint's bbox.
    pub fn place_from_palette(&self, reference: &str, position: Point) -> Result<Id, String> {
        let library = Arc::clone(&self.library);
        let margin_for = |fp: &Footprint| {
            if fp.key.is_empty() {
                crate::library::PlacementMargin::default()
            } else {
                library
                    .find(&fp.key)
                    .map(|e| e.placement_margin)
                    .unwrap_or_default()
            }
        };
        let mut inner = self.inner.write().expect("project lock poisoned");
        let idx = inner
            .palette
            .iter()
            .position(|f| f.reference == reference)
            .ok_or_else(|| format!("no palette item named {reference}"))?;
        let mut fp = inner.palette[idx].clone();
        fp.position = position;
        if let Some(other) = inner.board.first_overlapper(&fp, None) {
            return Err(format!(
                "{reference} at ({:.2}, {:.2}) mm would overlap {} — pick another position",
                position.x.to_mm(),
                position.y.to_mm(),
                other,
            ));
        }
        if let Some(other) = inner.board.first_body_overlapper(&fp, None, &margin_for) {
            return Err(format!(
                "{reference} body at ({:.2}, {:.2}) mm would overlap {} body — pick another position",
                position.x.to_mm(),
                position.y.to_mm(),
                other,
            ));
        }
        // Body-off-board is ALWAYS a hard rejection — even for
        // edge-mounted parts. The pads of an edge-mounted connector
        // legitimately touch the outline, but the plastic body of the
        // part can never extend past it.
        if let Some(reason) = inner.board.body_outline_violation(&fp, margin_for(&fp)) {
            return Err(format!("{reference} {reason}"));
        }
        if let Some(reason) = inner.board.edge_mount_violation(&fp) {
            return Err(format!(
                "{reference} is edge-mounted but {reason} — pick a position whose bbox touches the board outline",
            ));
        }
        let mut fp = inner.palette.remove(idx);
        fp.position = position;
        let id = fp.id;
        let reference_string = fp.reference.clone();
        let _ = inner.board.add_footprint(fp);
        let palette_count = inner.palette.len();
        drop(inner);
        self.bus.publish(Event::PaletteChanged {
            count: palette_count,
        });
        self.bus.publish(Event::FootprintAdded {
            id,
            reference: reference_string,
        });
        Ok(id)
    }

    /// Set the rotation (in degrees, CCW) of a footprint already on
    /// the board, identified by reference. Rejected if the rotated
    /// bbox would intersect another footprint's bbox.
    pub fn rotate_footprint(&self, reference: &str, rotation_deg: f32) -> Result<Id, String> {
        let library = Arc::clone(&self.library);
        let margin_for = |fp: &Footprint| {
            if fp.key.is_empty() {
                crate::library::PlacementMargin::default()
            } else {
                library
                    .find(&fp.key)
                    .map(|e| e.placement_margin)
                    .unwrap_or_default()
            }
        };
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
        if let Some(other) = inner.board.first_overlapper(&probe, Some(id)) {
            return Err(format!(
                "{reference} rotated to {rotation_deg:.0}° would overlap {other}"
            ));
        }
        if let Some(other) = inner
            .board
            .first_body_overlapper(&probe, Some(id), &margin_for)
        {
            return Err(format!(
                "{reference} body rotated to {rotation_deg:.0}° would overlap {other} body"
            ));
        }
        // See comment in `place_from_palette`: body-off-board is a
        // hard reject regardless of `edge_mounted`.
        if let Some(reason) = inner
            .board
            .body_outline_violation(&probe, margin_for(&probe))
        {
            return Err(format!(
                "{reference} rotated to {rotation_deg:.0}°: {reason}"
            ));
        }
        if let Some(reason) = inner.board.edge_mount_violation(&probe) {
            return Err(format!(
                "{reference} is edge-mounted but after rotation {reason}",
            ));
        }
        let position = if let Some(fp) = inner.board.footprints.get_mut(&id) {
            fp.rotation = rotation_deg;
            fp.position
        } else {
            return Err(format!("no board footprint named {reference}"));
        };
        drop(inner);
        self.bus.publish(Event::FootprintMoved { id, position });
        Ok(id)
    }

    /// Move a footprint already on the board to a new position.
    /// Rejected if the new bbox would intersect another footprint's bbox.
    pub fn move_footprint_to(&self, reference: &str, position: Point) -> Result<Id, String> {
        let library = Arc::clone(&self.library);
        let margin_for = |fp: &Footprint| {
            if fp.key.is_empty() {
                crate::library::PlacementMargin::default()
            } else {
                library
                    .find(&fp.key)
                    .map(|e| e.placement_margin)
                    .unwrap_or_default()
            }
        };
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
        if let Some(other) = inner.board.first_overlapper(&probe, Some(id)) {
            return Err(format!(
                "moving {reference} to ({:.2}, {:.2}) mm would overlap {other}",
                position.x.to_mm(),
                position.y.to_mm(),
            ));
        }
        if let Some(other) = inner
            .board
            .first_body_overlapper(&probe, Some(id), &margin_for)
        {
            return Err(format!(
                "moving {reference} to ({:.2}, {:.2}) mm: body would overlap {other} body",
                position.x.to_mm(),
                position.y.to_mm(),
            ));
        }
        // See comment in `place_from_palette`: body-off-board is a
        // hard reject regardless of `edge_mounted`.
        if let Some(reason) = inner
            .board
            .body_outline_violation(&probe, margin_for(&probe))
        {
            return Err(format!(
                "moving {reference} to ({:.2}, {:.2}) mm: {reason}",
                position.x.to_mm(),
                position.y.to_mm(),
            ));
        }
        if let Some(reason) = inner.board.edge_mount_violation(&probe) {
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
        self.bus.publish(Event::FootprintMoved { id, position });
        Ok(id)
    }

    /// Drop every trace and via belonging to one net. Returns the
    /// number of items removed.
    pub fn clear_net_routing(&self, net: &str) -> usize {
        let (removed, trace_count, via_count) = {
            let mut inner = self.inner.write().expect("project lock poisoned");
            let before = inner.board.traces.len() + inner.board.vias.len();
            inner.board.traces.retain(|t| t.net != net);
            inner.board.vias.retain(|v| v.net != net);
            let trace_count = inner.board.traces.len();
            let via_count = inner.board.vias.len();
            (before - (trace_count + via_count), trace_count, via_count)
        };
        if removed > 0 {
            self.bus.publish(Event::RoutingChanged {
                trace_count,
                via_count,
            });
        }
        removed
    }

    /// Remove a single trace by id. Returns whether anything was removed.
    pub fn delete_trace(&self, id: Id) -> bool {
        let (removed, trace_count, via_count) = {
            let mut inner = self.inner.write().expect("project lock poisoned");
            let before = inner.board.traces.len();
            inner.board.traces.retain(|t| t.id != id);
            (
                inner.board.traces.len() != before,
                inner.board.traces.len(),
                inner.board.vias.len(),
            )
        };
        if removed {
            self.bus.publish(Event::RoutingChanged {
                trace_count,
                via_count,
            });
        }
        removed
    }

    pub fn delete_via(&self, id: Id) -> bool {
        let (removed, trace_count, via_count) = {
            let mut inner = self.inner.write().expect("project lock poisoned");
            let before = inner.board.vias.len();
            inner.board.vias.retain(|v| v.id != id);
            (
                inner.board.vias.len() != before,
                inner.board.traces.len(),
                inner.board.vias.len(),
            )
        };
        if removed {
            self.bus.publish(Event::RoutingChanged {
                trace_count,
                via_count,
            });
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
        self.bus.publish(Event::RoutingChanged {
            trace_count: 0,
            via_count: 0,
        });
    }

    /// Copy footprint rotations from `from` into this project's
    /// matching-id footprints. No validation, no event — the caller
    /// should follow up with `replace_routing` (or another bus event)
    /// so the UI's project_state refetch picks up the changes.
    /// Footprints absent in `from` are left untouched; ids in `from`
    /// that don't exist here are silently skipped. Used by the
    /// auto-router so every trial's UI commit reflects the GA's
    /// rotation choices, not just the traces.
    pub fn sync_footprint_rotations(&self, from: &Board) {
        let mut inner = self.inner.write().expect("project lock poisoned");
        for (id, src) in &from.footprints {
            if let Some(dst) = inner.board.footprints.get_mut(id) {
                dst.rotation = src.rotation;
            }
        }
    }

    /// Atomically replace all routing with the given traces and vias and
    /// publish a single `RoutingChanged` event. Used by the auto-router
    /// to commit a trial board without firing one event per trace, which
    /// would saturate the UI render loop.
    pub fn replace_routing(&self, traces: Vec<Trace>, vias: Vec<Via>) {
        let (trace_count, via_count) = {
            let mut inner = self.inner.write().expect("project lock poisoned");
            inner.board.clear_routing();
            for trace in traces {
                inner.board.add_trace(trace);
            }
            for via in vias {
                inner.board.add_via(via);
            }
            (inner.board.traces.len(), inner.board.vias.len())
        };
        self.bus.publish(Event::RoutingChanged {
            trace_count,
            via_count,
        });
    }

    pub fn add_symbol(&self, symbol: Symbol) -> Id {
        let id = symbol.id;
        let reference = symbol.reference.clone();
        {
            let mut inner = self.inner.write().expect("project lock poisoned");
            inner.schematic.add_symbol(symbol);
        }
        self.bus.publish(Event::SymbolAdded { id, reference });
        id
    }

    /// Replace the connections on a named net. Returns an error if any
    /// referenced symbol or pin does not exist — keeps the netlist
    /// Add or replace a named `NetClass`. Reused by every net that
    /// references the class by name. Mutates the schematic; the
    /// router and DRC pick the new rules up on their next call.
    pub fn set_net_class(&self, class: crate::schematic::NetClass) {
        let name = class.name.clone();
        {
            let mut inner = self.inner.write().expect("project lock poisoned");
            inner.schematic.set_net_class(class);
        }
        self.log(
            crate::ActivityLevel::Info,
            format!("schematic.set_class: {name}"),
        );
    }

    /// Bind `net_name` to a previously-declared `NetClass`. Returns
    /// `Err` if the class does not exist (so the agent gets fast
    /// feedback rather than silently routing with default rules).
    pub fn assign_net_to_class(
        &self,
        net_name: impl Into<String>,
        class_name: impl Into<String>,
    ) -> Result<(), String> {
        let net_name = net_name.into();
        let class_name = class_name.into();
        {
            let mut inner = self.inner.write().expect("project lock poisoned");
            if !inner.schematic.net_classes.contains_key(&class_name) {
                return Err(format!(
                    "net class `{class_name}` not declared — define it with `class {class_name} ...` first"
                ));
            }
            inner
                .schematic
                .assign_net_to_class(net_name.clone(), class_name.clone());
        }
        self.log(
            crate::ActivityLevel::Info,
            format!("schematic.assign_net_class: {net_name} -> {class_name}"),
        );
        Ok(())
    }

    /// consistent so downstream tools (router, BOM) never see dangling
    /// references.
    #[allow(clippy::needless_pass_by_value)]
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
        // Push the net assignment down to any already-placed footprint
        // whose schematic symbol is part of this net. Without this,
        // wiring nets AFTER `place` left footprint pads with `net =
        // None`, even though the schematic was correct — the router
        // and the DRC then both saw those pads as unrouted.
        Self::propagate_net_to_pads(&mut inner, &net);
        let name = net.name.clone();
        let connection_count = net.connections.len();
        drop(inner);
        self.bus.publish(Event::NetChanged {
            name,
            connection_count,
        });
        Ok(())
    }

    /// Walk every footprint in `inner.board` and assign `pad.net` for
    /// any pad addressed (by pad NUMBER, or by pad NAME when number
    /// fails) by `net.connections`. Idempotent — overwrites previous
    /// assignments for the same pad on the same net.
    fn propagate_net_to_pads(inner: &mut ProjectInner, net: &Net) {
        // Resolve each connection's target pad number on the
        // matching footprint. Pad NUMBER is the canonical address;
        // pad NAME is the fallback (LED's "A"/"K", header pin
        // labels). Built once per call so we don't re-scan.
        let resolved: Vec<(String, String)> = net
            .connections
            .iter()
            .filter_map(|c| {
                let sym = inner.schematic.symbols.get(&c.symbol_id)?;
                Some((sym.reference.clone(), c.pin_number.clone()))
            })
            .collect();
        for fp in inner.board.footprints.values_mut() {
            for (sym_ref, pin_addr) in &resolved {
                if &fp.reference != sym_ref {
                    continue;
                }
                if let Some(pad) = fp
                    .pads
                    .iter_mut()
                    .find(|p| &p.number == pin_addr || &p.name == pin_addr)
                {
                    pad.net = Some(net.name.clone());
                }
            }
        }
    }
}

// ─── Persistence ──────────────────────────────────────────────────────
//
// JSON files: tiny (a 10-component design fits in a few KB),
// human-inspectable, and forgiving when we add new fields (everything
// is `#[serde(default)]`-friendly). The path is whatever the user
// passed to `fragua` (or to `save_to_path`); no implicit per-project
// directory.

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

impl Project {
    /// Load a project from an arbitrary JSON file (the path the user
    /// passed on the CLI). Returns `None` if the file is missing or
    /// can't be parsed. The autosave target is set to `path` so any
    /// subsequent edit writes back to it.
    #[must_use]
    pub fn load_from_path(path: &Path) -> Option<Self> {
        let bytes = fs::read(path).ok()?;
        let file: ProjectFile = serde_json::from_slice(&bytes).ok()?;
        let library = match crate::library::Library::open_default() {
            Ok(lib) => lib,
            Err(_) => {
                crate::library::Library::open_at(std::env::temp_dir().join("pcb-library")).ok()?
            }
        };
        let proj = Self {
            inner: Arc::new(RwLock::new(ProjectInner {
                name: file.name,
                board: file.board,
                schematic: file.schematic,
                palette: file.palette,
            })),
            bus: EventBus::new(),
            library: Arc::new(library),
            save_path: Arc::new(RwLock::new(Some(path.to_path_buf()))),
            pending_library: Arc::new(RwLock::new(Vec::new())),
            fab_profile: Arc::new(RwLock::new(None)),
        };
        proj.bus.publish(Event::ProjectChanged);
        Some(proj)
    }

    /// Write the current state to `path`, atomically via tmp+rename.
    /// Also rebinds the autosave target to `path` — so calling this
    /// once on a memory-only session "promotes" it to autosaving there.
    pub fn save_to_path(&self, path: &Path) -> Result<PathBuf, String> {
        let inner = self.inner.read().expect("project lock poisoned");
        let file = ProjectFile {
            name: inner.name.clone(),
            board: inner.board.clone(),
            schematic: inner.schematic.clone(),
            palette: inner.palette.clone(),
        };
        drop(inner);
        if let Some(parent) = path.parent() {
            if !parent.as_os_str().is_empty() {
                fs::create_dir_all(parent)
                    .map_err(|e| format!("project: mkdir {}: {e}", parent.display()))?;
            }
        }
        // Append `.tmp` to the full filename so the temp file lives next to
        // the target regardless of the user-chosen extension (`.fragua`,
        // `.json`, …) and never collides with another sibling.
        let tmp: PathBuf = {
            let mut s = path.as_os_str().to_owned();
            s.push(".tmp");
            PathBuf::from(s)
        };
        let bytes =
            serde_json::to_vec_pretty(&file).map_err(|e| format!("project: serialise: {e}"))?;
        fs::write(&tmp, &bytes).map_err(|e| format!("project: write {}: {e}", tmp.display()))?;
        fs::rename(&tmp, path).map_err(|e| format!("project: rename {}: {e}", path.display()))?;
        let final_path = path.to_path_buf();
        *self.save_path.write().expect("save_path lock poisoned") = Some(final_path.clone());
        Ok(final_path)
    }

    /// Current autosave target. `None` means memory-only — no autosave
    /// is happening; the caller must `save_to_path` to persist.
    #[must_use]
    pub fn save_path(&self) -> Option<PathBuf> {
        self.save_path
            .read()
            .expect("save_path lock poisoned")
            .clone()
    }

    /// Set (or clear) the autosave target without writing now.
    pub fn set_save_path(&self, path: Option<PathBuf>) {
        *self.save_path.write().expect("save_path lock poisoned") = path;
    }
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
