//! Tauri host. Owns the canonical `Project`, serves a tiny stateless
//! HTTP API on `127.0.0.1:7878` so an external agent can drive the
//! board, exposes commands to the webview, and re-emits project events
//! into the frontend's event bus.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use pcb_core::Project;
use serde::Serialize;
use tauri::{Emitter, State};

const API_DEFAULT_ADDR: &str = "127.0.0.1:7878";

/// Printed to stdout on launch and served at `GET /` so a fresh agent
/// can curl one URL and learn how to drive Fragua.
const USAGE: &str = "\
Fragua — AI-native PCB design tool.

USAGE
  fragua                 print this help + the full script reference, then exit
                         (so you read the surface before driving it)
  fragua run             launch the API server, empty in-memory project
  fragua run <file.fragua>
                         launch + load that file; autosave to it on every edit
                         (legacy `.json` files are also accepted)
  fragua help            same as bare `fragua`

LOCAL API
  Stateless HTTP on http://127.0.0.1:7878 (override: FRAGUA_API_ADDR).

  Replies are plain text (text/plain) — agent-friendly, no JSON parsing
  needed on the client side. Request bodies are JSON.

  GET  /                 usage + the full script reference
  GET  /help             same as `GET /` (usage + full script reference)
  POST /script           run a multi-line script
                         body:  {\"script\": \"...\"}
                         reply: per-line outcomes,
                                `[L<line> ok|FAIL <tool>] <text>`,
                                + an unsaved-session warning if any
  POST /save             write the current project to disk (atomic)
                         and bind autosave to that path
                         body:  {\"path\": \"/abs/or/rel/file.fragua\"}
                         reply: `Saved to <path>`
  GET  /screenshot       PNG render of the current project state, for
                         headless agent verification. No OS permissions
                         needed — rasterises the same SVG the webview
                         shows. Query params:
                           view=board|schematic  (default: board)
                           width=<px>            (default: 1600, max 8192)
                         reply: binary PNG (Content-Type: image/png)
  GET  /health           `ok`

  Examples:
    curl -s http://127.0.0.1:7878/script \\
      -H 'content-type: application/json' \\
      -d '{\"script\": \"outline 80 30\\nstatus\"}'

    curl -s http://127.0.0.1:7878/save \\
      -H 'content-type: application/json' \\
      -d '{\"path\": \"/tmp/board.fragua\"}'

  The script language is the surface for every design action (lib, sym,
  net, palette, place, route, drc, export, ...). `POST /save` is the
  one operational endpoint outside the script — useful when fragua was
  launched without a file argument (no autosave) and you still want to
  persist. Send `GET /` for the full script reference.
";

/// Wrapper kept in Tauri state so commands can read project + addr.
struct AppState {
    project: Project,
    api_addr: String,
    /// Set while an `Auto Routing` GA search is running so concurrent
    /// clicks from the UI bail out instead of stacking searches.
    autoroute_running: Arc<AtomicBool>,
    /// Tripped by the `stop_autoroute` command to ask the GA loop to
    /// finish gracefully and keep the best found so far. Reset to false
    /// at the start of every search.
    autoroute_stop: Arc<AtomicBool>,
}

#[derive(Serialize)]
struct ProjectStatePayload {
    name: String,
    footprint_count: usize,
    symbol_count: usize,
    net_count: usize,
    palette_count: usize,
    palette: Vec<PalettePayload>,
    api_addr: String,
    board_svg: String,
    schematic_svg: String,
    outline: Option<OutlinePayload>,
}

#[derive(Serialize)]
struct PalettePayload {
    reference: String,
    value: String,
    library: String,
    pad_count: usize,
}

#[derive(Serialize)]
struct OutlinePayload {
    x_mm: f64,
    y_mm: f64,
    w_mm: f64,
    h_mm: f64,
}

#[tauri::command]
fn project_state(state: State<'_, AppState>) -> ProjectStatePayload {
    let margins = collect_placement_margins(&state.project);
    let snap = state.project.read();
    let palette: Vec<PalettePayload> = snap
        .palette()
        .iter()
        .map(|fp| PalettePayload {
            reference: fp.reference.clone(),
            value: fp.value.clone(),
            library: fp.library.clone(),
            pad_count: fp.pads.len(),
        })
        .collect();
    let outline = snap.board().outline.map(|r| OutlinePayload {
        x_mm: r.min.x.to_mm(),
        y_mm: r.min.y.to_mm(),
        w_mm: r.width().to_mm(),
        h_mm: r.height().to_mm(),
    });
    ProjectStatePayload {
        name: snap.name().to_string(),
        footprint_count: snap.board().footprints.len(),
        symbol_count: snap.schematic().symbols.len(),
        net_count: snap.schematic().nets.len(),
        palette_count: palette.len(),
        palette,
        api_addr: state.api_addr.clone(),
        board_svg: pcb_render::render_svg_with_margins(snap.board(), &margins),
        schematic_svg: pcb_render::render_schematic_svg(snap.schematic()),
        outline,
    }
}

/// Snapshot the library-key → placement-margin lookup the renderer
/// consumes for body outlines. Mirrors the script-API helper of the
/// same name — keeping a tiny copy here avoids pulling `pcb_script` as
/// a runtime dep on the Tauri side.
fn collect_placement_margins(project: &pcb_core::Project) -> pcb_render::PlacementMarginMap {
    let mut out = pcb_render::PlacementMarginMap::default();
    for entry in project.library().list() {
        if entry.placement_margin.is_zero() {
            continue;
        }
        out.insert(entry.key, entry.placement_margin);
    }
    out
}

/// Wipe schematic + palette + board.
#[tauri::command]
fn reset_project(state: State<'_, AppState>) {
    state.project.reset();
    state
        .project
        .log(pcb_core::ActivityLevel::Info, "project.reset (UI button)");
}

/// Send every placed footprint back to the palette and drop routing.
/// Schematic — symbols and nets — survives so the next auto-place can
/// rebuild the layout from the same component set.
#[tauri::command]
fn reset_placement(state: State<'_, AppState>) {
    state.project.reset_board();
    state
        .project
        .log(pcb_core::ActivityLevel::Info, "placement reset (UI button)");
}

/// Drop every trace and via on the board. Footprints stay where they are.
#[tauri::command]
fn reset_route(state: State<'_, AppState>) {
    state.project.clear_routing();
    state
        .project
        .log(pcb_core::ActivityLevel::Info, "route reset (UI button)");
}

/// List every entry in the user's component library — same shape as the
/// Library entries USED by the current project — every key referenced
/// either by a placed footprint or by something still in the palette.
/// The disk-backed library is shared across projects but the UI panel
/// is scoped to "what this project actually uses". A future "global
/// catalog" pane will expose the full library for browsing.
#[tauri::command]
fn library_state(state: State<'_, AppState>) -> serde_json::Value {
    use std::collections::HashSet;
    let snap = state.project.read();
    let mut used: HashSet<String> = HashSet::new();
    for fp in snap.board().footprints.values() {
        if !fp.key.is_empty() {
            used.insert(fp.key.clone());
        }
    }
    for fp in snap.palette() {
        if !fp.key.is_empty() {
            used.insert(fp.key.clone());
        }
    }
    drop(snap);
    let entries = state.project.library().list();
    let items: Vec<serde_json::Value> = entries
        .iter()
        .filter(|e| used.contains(&e.key))
        .map(|e| {
            serde_json::json!({
                "key": e.key,
                "description": e.description,
                "default_value": e.default_value,
                "default_rotation_deg": e.default_rotation_deg,
                "edge_mounted": e.edge_mounted,
                "pad_count": e.pads.len(),
                "attachments": e.attachments.iter().map(|a| serde_json::json!({
                    "id": a.id,
                    "kind": a.kind,
                    "filename": a.filename,
                    "mime": a.mime,
                    "added_at": a.added_at,
                })).collect::<Vec<_>>(),
                "created_at": e.created_at,
            })
        })
        .collect();
    serde_json::json!({ "entries": items })
}

/// All the info the board info-modal needs about one placed
/// footprint: schematic-side identity (reference, value, description,
/// position, rotation, edge_mounted) plus the linked library entry
/// (key, description, pads, attachments). The frontend then fetches
/// each photo attachment separately via `library_attachment_data_uri`.
#[tauri::command]
fn component_info(
    state: State<'_, AppState>,
    reference: String,
) -> Result<serde_json::Value, String> {
    let snap = state.project.read();
    let fp = snap
        .board()
        .footprints
        .values()
        .find(|f| f.reference == reference)
        .ok_or_else(|| format!("no footprint named {reference}"))?
        .clone();
    drop(snap);

    let lib_entry = if fp.key.is_empty() {
        None
    } else {
        state.project.library().find(&fp.key)
    };

    let pads: Vec<serde_json::Value> = fp
        .pads
        .iter()
        .map(|p| {
            serde_json::json!({
                "number": p.number,
                "name": p.name,
                "net": p.net,
                "layer": if p.layer.is_top() { "top" } else { "bottom" },
            })
        })
        .collect();

    let library = lib_entry.map(|e| {
        serde_json::json!({
            "key": e.key,
            "description": e.description,
            "default_value": e.default_value,
            "edge_mounted": e.edge_mounted,
            "pad_count": e.pads.len(),
            "attachments": e.attachments.iter().map(|a| serde_json::json!({
                "id": a.id,
                "kind": a.kind,
                "filename": a.filename,
                "mime": a.mime,
            })).collect::<Vec<_>>(),
        })
    });

    Ok(serde_json::json!({
        "reference": fp.reference,
        "key": fp.key,
        "value": fp.value,
        "description": fp.description,
        "rotation_deg": fp.rotation,
        "edge_mounted": fp.edge_mounted,
        "x_mm": fp.position.x.to_mm(),
        "y_mm": fp.position.y.to_mm(),
        "pads": pads,
        "library": library,
    }))
}

/// Full library (every entry on disk, NOT filtered to "used by this
/// project"). Powers the library review pane: the user walks every
/// component, compares the rendered footprint against the photo, and
/// flags anything mirrored. Returns the inline review SVG for each
/// entry so the frontend can paint without a follow-up call.
#[tauri::command]
fn library_review_state(state: State<'_, AppState>) -> serde_json::Value {
    let entries = state.project.library().list();
    let items: Vec<serde_json::Value> = entries
        .iter()
        .map(|e| {
            serde_json::json!({
                "key": e.key,
                "description": e.description,
                "default_value": e.default_value,
                "edge_mounted": e.edge_mounted,
                "pad_count": e.pads.len(),
                "ground_pad_count": e.pads.iter().filter(|p| {
                    pcb_render::is_ground_pad_label(&p.number, &p.name)
                }).count(),
                "lcsc_id": e.lcsc_id,
                "mpn": e.mpn,
                "attachments": e.attachments.iter().map(|a| serde_json::json!({
                    "id": a.id,
                    "kind": a.kind,
                    "filename": a.filename,
                    "mime": a.mime,
                    "added_at": a.added_at,
                    "view_transform": {
                        "rotation_deg": a.view_transform.rotation_deg,
                        "flip_h": a.view_transform.flip_h,
                        "flip_v": a.view_transform.flip_v,
                    },
                })).collect::<Vec<_>>(),
                "created_at": e.created_at,
                "review_svg": pcb_render::render_library_entry_svg(e),
                "footprint_view_transform": {
                    "rotation_deg": e.footprint_view_transform.rotation_deg,
                    "flip_h": e.footprint_view_transform.flip_h,
                    "flip_v": e.footprint_view_transform.flip_v,
                },
                "placement_margin": {
                    "top_mm": e.placement_margin.top_mm,
                    "right_mm": e.placement_margin.right_mm,
                    "bottom_mm": e.placement_margin.bottom_mm,
                    "left_mm": e.placement_margin.left_mm,
                },
            })
        })
        .collect();
    serde_json::json!({ "entries": items })
}

/// Persist a visual-only transform on one library attachment (the
/// photo cell in the review card). Touches only `~/.pcb-library/`; the
/// project file is untouched.
#[tauri::command]
fn library_set_attachment_view_transform(
    state: State<'_, AppState>,
    key: String,
    attachment_id: String,
    rotation_deg: u16,
    flip_h: bool,
    flip_v: bool,
) -> Result<(), String> {
    let transform = pcb_core::ViewTransform {
        rotation_deg: rotation_deg % 360,
        flip_h,
        flip_v,
    };
    state
        .project
        .library()
        .set_attachment_view_transform(&key, &attachment_id, transform)?;
    state.project.notify_library_changed();
    Ok(())
}

/// Persist a visual-only transform on the rendered-footprint cell of
/// a library entry's review card.
#[tauri::command]
fn library_set_footprint_view_transform(
    state: State<'_, AppState>,
    key: String,
    rotation_deg: u16,
    flip_h: bool,
    flip_v: bool,
) -> Result<(), String> {
    let transform = pcb_core::ViewTransform {
        rotation_deg: rotation_deg % 360,
        flip_h,
        flip_v,
    };
    state
        .project
        .library()
        .set_footprint_view_transform(&key, transform)?;
    state.project.notify_library_changed();
    Ok(())
}

/// Persist the per-side placement margin on a library entry. Picked up
/// by the next auto-place run via the script tool.
#[tauri::command]
fn library_set_placement_margin(
    state: State<'_, AppState>,
    key: String,
    top_mm: f64,
    right_mm: f64,
    bottom_mm: f64,
    left_mm: f64,
) -> Result<(), String> {
    let margin = pcb_core::PlacementMargin {
        top_mm: top_mm.max(0.0),
        right_mm: right_mm.max(0.0),
        bottom_mm: bottom_mm.max(0.0),
        left_mm: left_mm.max(0.0),
    };
    state
        .project
        .library()
        .set_placement_margin(&key, margin)?;
    state.project.notify_library_changed();
    Ok(())
}

/// Drop an entry (and all its attachment files) from the global
/// library. The review pane refetches via the `LibraryChanged` event.
#[tauri::command]
fn library_delete_entry(state: State<'_, AppState>, key: String) -> Result<bool, String> {
    let removed = state.project.library().delete(&key)?;
    if removed {
        state.project.log(
            pcb_core::ActivityLevel::Warn,
            format!("library.delete: {key}"),
        );
        state.project.notify_library_changed();
    }
    Ok(removed)
}

/// Snapshot of every library entry the agent has queued but a human
/// has not confirmed yet. Each item carries the inline review SVG and
/// the staged attachments (photo + datasheet) as base64 data URIs —
/// the frontend renders the confirmation modal from this alone, no
/// follow-up fetches needed.
#[tauri::command]
fn pending_library_entries(state: State<'_, AppState>) -> serde_json::Value {
    use base64::Engine;
    let pending = state.project.pending_library_entries();
    let items: Vec<serde_json::Value> = pending
        .iter()
        .map(|p| {
            let attachments: Vec<serde_json::Value> = p
                .attachments
                .iter()
                .map(|a| {
                    let b64 = base64::engine::general_purpose::STANDARD.encode(&a.data);
                    serde_json::json!({
                        "kind": a.kind,
                        "filename": a.filename,
                        "mime": a.mime,
                        "data_uri": format!("data:{};base64,{}", a.mime, b64),
                        "bytes": a.data.len(),
                    })
                })
                .collect();
            serde_json::json!({
                "key": p.entry.key,
                "description": p.entry.description,
                "default_value": p.entry.default_value,
                "default_rotation_deg": p.entry.default_rotation_deg,
                "edge_mounted": p.entry.edge_mounted,
                "pad_count": p.entry.pads.len(),
                "ground_pad_count": p.entry.pads.iter().filter(|pad| {
                    pcb_render::is_ground_pad_label(&pad.number, &pad.name)
                }).count(),
                "lcsc_id": p.entry.lcsc_id,
                "mpn": p.entry.mpn,
                "attachments": attachments,
                "review_svg": pcb_render::render_library_entry_svg(&p.entry),
                "pads": p.entry.pads.iter().map(|pad| serde_json::json!({
                    "number": pad.number,
                    "name": pad.name,
                    "x_mm": pad.x_mm,
                    "y_mm": pad.y_mm,
                    "w_mm": pad.w_mm,
                    "h_mm": pad.h_mm,
                    "drill_mm": pad.drill_mm,
                    "is_ground": pcb_render::is_ground_pad_label(&pad.number, &pad.name),
                })).collect::<Vec<_>>(),
            })
        })
        .collect();
    serde_json::json!({ "entries": items })
}

/// Confirm a pending library entry: promote it to the on-disk library
/// (plus any staged attachments). The script-side agent does NOT have
/// a verb for this — confirmation is human-only, by design.
#[tauri::command]
fn confirm_pending_library_entry(state: State<'_, AppState>, key: String) -> Result<bool, String> {
    let ok = state.project.confirm_pending_library_entry(&key)?;
    state.project.log(
        pcb_core::ActivityLevel::Info,
        format!(
            "library.confirm: {} ({})",
            key,
            if ok { "saved" } else { "no pending match" }
        ),
    );
    Ok(ok)
}

/// Discard a pending library entry. Drops the staged attachments too.
#[tauri::command]
fn discard_pending_library_entry(state: State<'_, AppState>, key: String) -> bool {
    let ok = state.project.discard_pending_library_entry(&key);
    state.project.log(
        pcb_core::ActivityLevel::Warn,
        format!(
            "library.discard: {} ({})",
            key,
            if ok { "dropped" } else { "no pending match" }
        ),
    );
    ok
}

/// Read one library attachment as a base64-encoded data URI so the
/// webview's <img> can render it directly. Files large enough that
/// base64-ing 33% blows past the IPC limit get rejected; in practice
/// component photos are <2 MB.
#[tauri::command]
fn library_attachment_data_uri(
    state: State<'_, AppState>,
    key: String,
    attachment_id: String,
) -> Result<String, String> {
    use base64::Engine;
    let entry = state
        .project
        .library()
        .find(&key)
        .ok_or_else(|| format!("no library entry with key {key}"))?;
    let att = entry
        .attachments
        .iter()
        .find(|a| a.id == attachment_id)
        .ok_or_else(|| format!("no attachment {attachment_id} on {key}"))?;
    let bytes = state
        .project
        .library()
        .read_attachment(att)
        .map_err(|e| e.to_string())?;
    let b64 = base64::engine::general_purpose::STANDARD.encode(&bytes);
    Ok(format!("data:{};base64,{}", att.mime, b64))
}

/// Set the rectangular Edge.Cuts outline of the board, plus an
/// optional uniform corner radius (mm). The webview's set-board-size
/// control uses this; the script verb `outline W H [radius=R]` goes
/// through the script tool path instead.
#[tauri::command]
fn set_board_outline(
    state: State<'_, AppState>,
    w_mm: f64,
    h_mm: f64,
    corner_radius_mm: Option<f64>,
) -> Result<(), String> {
    if w_mm < 1.0 || h_mm < 1.0 {
        return Err("dimensions must be at least 1 mm".to_string());
    }
    let outline = pcb_core::Rect::from_corners(
        pcb_core::Point::new(
            pcb_core::Length::from_mm(0.0),
            pcb_core::Length::from_mm(0.0),
        ),
        pcb_core::Point::new(
            pcb_core::Length::from_mm(w_mm),
            pcb_core::Length::from_mm(h_mm),
        ),
    );
    let radius = pcb_core::Length::from_mm(corner_radius_mm.unwrap_or(0.0).max(0.0));
    state.project.set_outline_with_radius(outline, radius);
    Ok(())
}

/// Drop the named palette footprint at the given board coordinates.
/// Used by the UI's drag-from-palette gesture.
#[tauri::command]
fn place_from_palette(
    state: State<'_, AppState>,
    reference: String,
    x_mm: f64,
    y_mm: f64,
) -> Result<(), String> {
    state
        .project
        .place_from_palette(
            &reference,
            pcb_core::Point::new(
                pcb_core::Length::from_mm(x_mm),
                pcb_core::Length::from_mm(y_mm),
            ),
        )
        .map(|_| ())
}

/// Rotate a footprint already on the board by `degrees_delta` (CCW).
/// Wraps modulo 360. Used by the UI's "R" keybinding.
#[tauri::command]
fn rotate_footprint(
    state: State<'_, AppState>,
    reference: String,
    degrees_delta: f32,
) -> Result<(), String> {
    // Read current rotation, add delta, write back.
    let current = {
        let snap = state.project.read();
        snap.board()
            .footprints
            .values()
            .find(|f| f.reference == reference)
            .map(|f| f.rotation)
            .ok_or_else(|| format!("no footprint named {reference}"))?
    };
    let next = (current + degrees_delta).rem_euclid(360.0);
    state.project.rotate_footprint(&reference, next).map(|_| ())
}

/// Move a footprint already on the board. Used by the UI's
/// drag-within-canvas gesture.
#[tauri::command]
fn move_footprint(
    state: State<'_, AppState>,
    reference: String,
    x_mm: f64,
    y_mm: f64,
) -> Result<(), String> {
    state
        .project
        .move_footprint_to(
            &reference,
            pcb_core::Point::new(
                pcb_core::Length::from_mm(x_mm),
                pcb_core::Length::from_mm(y_mm),
            ),
        )
        .map(|_| ())
}

/// Run the native DRC and return the report. The frontend reads
/// `violations` to paint markers on the board.
#[tauri::command]
fn run_drc(state: State<'_, AppState>) -> Result<DrcReportPayload, String> {
    let mut opts = pcb_drc::DrcOptions::default();
    for entry in state.project.library().list() {
        if entry.placement_margin.is_zero() {
            continue;
        }
        opts.placement_margins
            .insert(entry.key, entry.placement_margin);
    }
    let snap = state.project.read();
    let report = pcb_drc::run(snap.board(), &opts);
    drop(snap);
    state.project.log(
        pcb_core::ActivityLevel::Info,
        format!(
            "drc: {} error(s), {} warning(s)",
            report.error_count, report.warning_count
        ),
    );
    Ok(DrcReportPayload {
        error_count: report.error_count,
        warning_count: report.warning_count,
        violations: report
            .violations
            .iter()
            .map(|v| DrcViolationPayload {
                kind: format!("{:?}", v.kind),
                severity: format!("{:?}", v.severity).to_lowercase(),
                message: v.message.clone(),
                x_mm: v.x_mm,
                y_mm: v.y_mm,
                involved: v.involved.clone(),
            })
            .collect(),
    })
}

#[derive(Serialize)]
struct DrcReportPayload {
    error_count: usize,
    warning_count: usize,
    violations: Vec<DrcViolationPayload>,
}

#[derive(Serialize)]
struct DrcViolationPayload {
    kind: String,
    severity: String,
    message: String,
    x_mm: f64,
    y_mm: f64,
    involved: Vec<String>,
}

/// Run the native autorouter with default options.
#[tauri::command]
fn run_router(state: State<'_, AppState>) -> Result<String, String> {
    let mut work = state.project.read().board().clone();
    let report = pcb_router::route(&mut work, &pcb_router::RouteOptions::default());

    state.project.clear_routing();
    for trace in &work.traces {
        state.project.add_trace(trace.clone());
    }
    for via in &work.vias {
        state.project.add_via(via.clone());
    }

    let failed: Vec<&str> = report
        .per_net
        .iter()
        .filter_map(|(n, o)| matches!(o, pcb_router::Outcome::Failed { .. }).then_some(n.as_str()))
        .collect();
    let summary = if failed.is_empty() {
        format!(
            "Routed {} traces, {} vias",
            report.trace_count, report.via_count
        )
    } else {
        format!(
            "Routed {} traces, {} vias ({} failed: {})",
            report.trace_count,
            report.via_count,
            failed.len(),
            failed.join(", ")
        )
    };
    state.project.log(
        pcb_core::ActivityLevel::Info,
        format!("route.run: {summary}"),
    );
    Ok(summary)
}

/// Payload mirroring `pcb_router_tune::GaProgress` for the
/// `autoroute:progress` event the frontend subscribes to. snake_case is
/// kept on the wire to match the rest of the Fragua command surface.
#[derive(Serialize, Clone)]
struct AutorouteProgressPayload {
    generation: usize,
    evaluations: usize,
    cache_hits: usize,
    elapsed_secs: f64,
    best_score: f64,
    best_drc_errors: usize,
    best_failed_nets: usize,
    best_length_mm: f64,
    best_vias: usize,
    best_cell_mm: f64,
    best_via_cost: u32,
    best_clearance_mm: f64,
    best_net_order: Vec<String>,
    improved: bool,
}

impl From<&pcb_router_tune::GaProgress> for AutorouteProgressPayload {
    fn from(p: &pcb_router_tune::GaProgress) -> Self {
        Self {
            generation: p.generation,
            evaluations: p.evaluations,
            cache_hits: p.cache_hits,
            elapsed_secs: p.elapsed_secs,
            best_score: p.best_score,
            best_drc_errors: p.best_drc_errors,
            best_failed_nets: p.best_failed_nets,
            best_length_mm: p.best_length_mm,
            best_vias: p.best_vias,
            best_cell_mm: p.best_cell_mm,
            best_via_cost: p.best_via_cost,
            best_clearance_mm: p.best_clearance_mm,
            best_net_order: p.best_net_order.clone(),
            improved: p.improved,
        }
    }
}

#[derive(Serialize, Clone)]
struct AutorouteOutcomePayload {
    generations: usize,
    total_evaluations: usize,
    cache_hits: usize,
    elapsed_secs: f64,
    best: Option<AutorouteProgressPayload>,
}

/// Spawn an in-process GA search against the live project's current
/// board. Returns immediately; progress streams via the
/// `autoroute:progress` event, completion via `autoroute:done` (and
/// `autoroute:error` on failure).
#[tauri::command]
fn start_autoroute(
    app: tauri::AppHandle,
    state: State<'_, AppState>,
    budget_secs: u64,
) -> Result<String, String> {
    // Atomic CAS: only one autoroute thread at a time. The frontend
    // also disables its button while the run is live, but we belt-and-
    // braces here in case a script or a stray IPC triggers a second.
    if state
        .autoroute_running
        .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
        .is_err()
    {
        return Err("already running".to_string());
    }

    // Clear any stale stop request from the previous run.
    state.autoroute_stop.store(false, Ordering::SeqCst);

    let board = state.project.read().board().clone();
    let project = state.project.clone();
    let running = state.autoroute_running.clone();
    let stop_flag = state.autoroute_stop.clone();
    let app_handle = app.clone();

    project.log(
        pcb_core::ActivityLevel::Info,
        format!("autoroute.start: budget={budget_secs}s"),
    );

    // GA is CPU-bound — plain OS thread so the Tauri async runtime
    // stays free for IPC and the HTTP server.
    std::thread::spawn(move || {
        let config = pcb_router_tune::GaConfig {
            algorithm: pcb_router_tune::Algorithm::Ga,
            budget_secs,
            population: 16,
            mutation_rate: 0.30,
            patience: 8,
            max_generations: 50,
            trials: 0,
            seed: 0,
        };
        let drc_opts = pcb_drc::DrcOptions::default();
        let progress_app = app_handle.clone();
        let progress_project = project.clone();
        // Board re-renders are expensive; only commit when this trial
        // sets a new best OR ~500 ms have passed since the last commit.
        // Progress text always fires so the user sees evaluation count
        // tick up between commits.
        let mut last_commit = std::time::Instant::now();
        let result = pcb_router_tune::run_search(&board, &config, &drc_opts, &stop_flag, |p, trial_board| {
            let now = std::time::Instant::now();
            if p.improved || now.duration_since(last_commit).as_millis() >= 500 {
                // Sync rotations first so the SVG re-render the
                // RoutingChanged event triggers picks up the GA's
                // current orientation pick alongside the new traces.
                progress_project.sync_footprint_rotations(trial_board);
                progress_project.replace_routing(
                    trial_board.traces.clone(),
                    trial_board.vias.clone(),
                );
                last_commit = now;
            }
            let payload: AutorouteProgressPayload = p.into();
            let _ = progress_app.emit("autoroute:progress", &payload);
        });

        match result {
            Ok((best_board, outcome)) => {
                project.sync_footprint_rotations(&best_board);
                project.replace_routing(best_board.traces.clone(), best_board.vias.clone());
                let best_payload: Option<AutorouteProgressPayload> =
                    outcome.best.as_ref().map(Into::into);
                if let Some(best) = best_payload.as_ref() {
                    project.log(
                        pcb_core::ActivityLevel::Info,
                        format!(
                            "autoroute.done: {gens} gens, {eval} evals (+{ch} cached) in {secs:.1}s; best {len:.1}mm, {vias} vias, {err} DRC err",
                            gens = outcome.generations,
                            eval = outcome.total_evaluations,
                            ch = outcome.cache_hits,
                            secs = outcome.elapsed_secs,
                            len = best.best_length_mm,
                            vias = best.best_vias,
                            err = best.best_drc_errors,
                        ),
                    );
                } else {
                    project.log(
                        pcb_core::ActivityLevel::Warn,
                        "autoroute.done: no trials completed".to_string(),
                    );
                }
                let outcome_payload = AutorouteOutcomePayload {
                    generations: outcome.generations,
                    total_evaluations: outcome.total_evaluations,
                    cache_hits: outcome.cache_hits,
                    elapsed_secs: outcome.elapsed_secs,
                    best: best_payload,
                };
                // Force an explicit save so the user doesn't depend on
                // the 500 ms autosave debounce — if they close the window
                // immediately the best is already on disk.
                if let Some(target) = project.save_path() {
                    if let Err(e) = project.save_to_path(&target) {
                        project.log(
                            pcb_core::ActivityLevel::Error,
                            format!("autoroute.save: {e}"),
                        );
                    }
                }
                let _ = app_handle.emit("autoroute:done", &outcome_payload);
            }
            Err(e) => {
                project.log(
                    pcb_core::ActivityLevel::Error,
                    format!("autoroute.error: {e}"),
                );
                let _ = app_handle.emit("autoroute:error", &e);
            }
        }
        running.store(false, Ordering::SeqCst);
    });

    Ok("started".to_string())
}

/// Run the full JLCPCB fab pack: ERC + DRC + manufacturing-DRC,
/// generates Gerbers/drill/BOM/CPL, zips it, and drops the file in
/// `~/Downloads/`. Returns the zip path plus a short summary so the
/// UI can show "ready / NOT READY" + the path.
#[tauri::command]
fn export_jlcpcb_pack(state: State<'_, AppState>) -> Result<JlcpcbPackResult, String> {
    let out_dir = std::env::var_os("HOME").map_or_else(
        || std::path::PathBuf::from("/tmp"),
        |h| std::path::PathBuf::from(h).join("Downloads"),
    );
    let report = pcb_fab::pack(&state.project, pcb_fab::Provider::Jlcpcb, &out_dir)
        .map_err(|e| format!("pack: {e}"))?;
    let zip_path = report.zip_path.to_string_lossy().into_owned();
    state.project.log(
        pcb_core::ActivityLevel::Info,
        format!(
            "fab.pack: jlcpcb → {} ({} blocking)",
            zip_path,
            report.blocking_reasons.len()
        ),
    );
    Ok(JlcpcbPackResult {
        ready: !report.blocking,
        zip_path,
        file_count: report.files.len(),
        blocking_reasons: report.blocking_reasons,
    })
}

#[derive(Serialize)]
struct JlcpcbPackResult {
    ready: bool,
    zip_path: String,
    file_count: usize,
    blocking_reasons: Vec<String>,
}

#[derive(Serialize)]
struct OdbPackResult {
    tgz_path: String,
    file_count: usize,
}

/// ODB++ export: builds the standard ODB++ tree and writes it as a
/// `<project>.tgz` in `~/Downloads`. Mirrors `export_jlcpcb_pack`'s
/// shape so the frontend can show a similar "ready" / path UI.
#[tauri::command]
fn export_odb_pack(state: State<'_, AppState>) -> Result<OdbPackResult, String> {
    let snap = state.project.read();
    let name = snap.name().to_string();
    let board = snap.board().clone();
    drop(snap);

    let stem = pcb_odb::sanitize_name(&name);
    let tgz_bytes = pcb_odb::write_odb_tgz(&board, &stem).map_err(|e| format!("odb: {e}"))?;
    let out_dir = std::env::var_os("HOME").map_or_else(
        || std::path::PathBuf::from("/tmp"),
        |h| std::path::PathBuf::from(h).join("Downloads"),
    );
    std::fs::create_dir_all(&out_dir).map_err(|e| e.to_string())?;
    let path = out_dir.join(format!("{stem}.tgz"));
    std::fs::write(&path, &tgz_bytes).map_err(|e| e.to_string())?;

    let file_count = pcb_odb::build_tree(&board, &stem).len();
    state.project.log(
        pcb_core::ActivityLevel::Info,
        format!(
            "odb.export: wrote {} ({} files)",
            path.display(),
            file_count,
        ),
    );
    Ok(OdbPackResult {
        tgz_path: path.to_string_lossy().into_owned(),
        file_count,
    })
}

/// Ask the running autoroute search to finish on the next trial
/// boundary and commit the best genome found so far. No-op if no
/// search is running.
#[tauri::command]
fn stop_autoroute(state: State<'_, AppState>) -> Result<String, String> {
    if !state.autoroute_running.load(Ordering::SeqCst) {
        return Ok("not running".to_string());
    }
    state.autoroute_stop.store(true, Ordering::SeqCst);
    Ok("stop requested".to_string())
}

/// Write the full fab pack (Gerbers + Excellon + BOM + pos.csv) into
/// `~/Downloads/pcb-{name}-{timestamp}/` and return the directory.
#[tauri::command]
fn export_fab_pack(state: State<'_, AppState>) -> Result<String, String> {
    let snap = state.project.read();
    let name = snap.name().to_string();
    let board = snap.board().clone();
    drop(snap);

    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_err(|e| e.to_string())?
        .as_secs();
    let home = std::env::var("HOME").map_err(|_| "no HOME env var".to_string())?;
    let dir = std::path::PathBuf::from(home)
        .join("Downloads")
        .join(format!("pcb-{name}-{stamp}"));

    let paths = pcb_gerber::write_fab_pack(&board, &name, &dir).map_err(|e| e.to_string())?;
    state.project.log(
        pcb_core::ActivityLevel::Info,
        format!(
            "output.fab_pack: wrote {} files to {}",
            paths.len(),
            dir.display()
        ),
    );
    Ok(dir.to_string_lossy().into_owned())
}

#[tauri::command]
fn add_demo_resistor(state: State<'_, AppState>) {
    use pcb_core::{CopperLayer, Footprint, Id, Length, Pad, Point};
    let count = state.project.read().board().footprints.len();
    let x_mm = 5.0 + (count as f64) * 4.0;
    let footprint = Footprint {
        id: Id::new(),
        reference: format!("R{}", count + 1),
        value: "10k".into(),
        library: "Resistor_SMD:R_0805".into(),
        position: Point::new(Length::from_mm(x_mm), Length::from_mm(15.0)),
        rotation: 0.0,
        layer: CopperLayer::Top,
        pads: vec![
            Pad {
                number: "1".into(),
                name: String::new(),
                offset: Point::new(Length::from_mm(-1.0), Length::ZERO),
                size: (Length::from_mm(1.0), Length::from_mm(1.2)),
                layer: CopperLayer::Top,
                net: None,
                drill: None,
            },
            Pad {
                number: "2".into(),
                name: String::new(),
                offset: Point::new(Length::from_mm(1.0), Length::ZERO),
                size: (Length::from_mm(1.0), Length::from_mm(1.2)),
                layer: CopperLayer::Top,
                net: None,
                drill: None,
            },
        ],
        key: String::new(),
        description: String::new(),
        edge_mounted: false,
        silk: Vec::new(),
    };
    state.project.add_footprint(footprint);
}

/// Auto-stitch every isolated plane pad: drop a same-net via (beside the
/// pad with a stub, or via-in-pad when fully boxed) that ties it to the
/// pour on another copper layer. Returns how many were stitched and which
/// pads remain unreachable (a manual reroute is required for those).
#[tauri::command]
fn stitch_isolated_pads(state: State<'_, AppState>) -> serde_json::Value {
    let plan = {
        let snap = state.project.read();
        pcb_core::stitch::plan_stitches(snap.board(), pcb_core::stitch::StitchParams::default())
    };
    let stitched = plan.proposals.len();
    for s in &plan.proposals {
        state.project.add_via(s.via.clone());
        if let Some(stub) = &s.stub {
            state.project.add_trace(stub.clone());
        }
    }
    serde_json::json!({ "stitched": stitched, "unreachable": plan.unreachable })
}

/// Entry point used by the binary in `main.rs`.
pub fn run() {
    // Launch is gated behind a `run` subcommand on purpose: `fragua run
    // [file.fragua]` starts the API server (+ optional file, autosaved);
    // ANY other invocation — bare `fragua`, `fragua help`, `fragua
    // <file>` — prints the usage + the FULL script reference and exits
    // WITHOUT launching. The point is to force whoever just wants to
    // "open it" (a human, or an agent that backgrounds the process and
    // never reads its stdout) to actually see the surface — the verbs,
    // the manual `trace`/`via`, `GET /help` — instead of flying blind.
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.first().map(String::as_str) != Some("run") {
        print!("{USAGE}");
        println!("\n--- SCRIPT REFERENCE ---\n");
        print!("{}", pcb_script::tools::script_reference());
        let _ = std::io::Write::flush(&mut std::io::stdout());
        return;
    }

    // `fragua run [file.fragua]` — the file, if any, is the arg after
    // `run`. Legacy `.json` loads too (on-disk format is JSON regardless
    // of extension); a missing/unreadable file starts empty. Autosave
    // target lives on the project, so a later `POST /save` rebinds it.
    let cli_path = args.get(1).map(std::path::PathBuf::from);
    let project = match cli_path {
        Some(path) => Project::load_from_path(&path).unwrap_or_else(|| {
            let p = Project::new(name_from_path(&path));
            p.set_save_path(Some(path.clone()));
            p
        }),
        None => Project::new(""),
    };
    let api_addr =
        std::env::var("FRAGUA_API_ADDR").unwrap_or_else(|_| API_DEFAULT_ADDR.to_string());

    // Concise startup line — the full usage + script reference is at
    // `GET /` and `GET /help`, and bare `fragua` (no `run`) prints it.
    println!("Fragua — API on http://{api_addr}  ·  GET /help for the full script reference");
    let _ = std::io::Write::flush(&mut std::io::stdout());

    let state = AppState {
        project: project.clone(),
        api_addr: api_addr.clone(),
        autoroute_running: Arc::new(AtomicBool::new(false)),
        autoroute_stop: Arc::new(AtomicBool::new(false)),
    };

    tauri::Builder::default()
        .manage(state)
        .setup(move |app| {
            let handle = app.handle().clone();
            spawn_event_pump(handle, project.clone());
            spawn_autosave(project.clone());
            spawn_http_api(project, api_addr.clone());
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            project_state,
            add_demo_resistor,
            stitch_isolated_pads,
            reset_project,
            reset_placement,
            reset_route,
            set_board_outline,
            place_from_palette,
            move_footprint,
            rotate_footprint,
            run_router,
            start_autoroute,
            stop_autoroute,
            export_jlcpcb_pack,
            export_odb_pack,
            run_drc,
            export_fab_pack,
            library_state,
            library_review_state,
            library_attachment_data_uri,
            library_set_attachment_view_transform,
            library_set_footprint_view_transform,
            library_set_placement_margin,
            library_delete_entry,
            component_info,
            pending_library_entries,
            confirm_pending_library_entry,
            discard_pending_library_entry
        ])
        .run(tauri::generate_context!())
        .expect("tauri runtime");
}

/// Subscribe to the project event bus and forward every event into the
/// webview as `pcb://event`. Errors (lagged subscriber, send failure)
/// are non-fatal — the next event will catch up.
fn spawn_event_pump(handle: tauri::AppHandle, project: Project) {
    let mut rx = project.events().subscribe();
    tauri::async_runtime::spawn(async move {
        loop {
            match rx.recv().await {
                Ok(event) => {
                    let _ = handle.emit("pcb://event", &event);
                }
                Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
            }
        }
    });
}

/// Subscribe to the project event bus, debounce mutations, and write
/// `~/.pcb-projects/<name>/current.json` after each idle window. Every
/// `HISTORY_EVERY_N_SAVES` saves we also drop a copy into the history
/// dir so the user can roll back if a session goes wrong.
///
/// Activity / PlacementProgress / LibraryChanged are NOT mutations of
/// the project file (activity is logging; library is its own store), so
/// they don't trigger a save.
fn name_from_path(path: &std::path::Path) -> String {
    path.file_stem()
        .and_then(|s| s.to_str())
        .map(str::to_owned)
        .unwrap_or_else(|| "untitled".to_string())
}

fn spawn_autosave(project: Project) {
    // The autosave target lives on `project.save_path()`. We always run
    // the loop, but only write when the project has a path bound:
    // this way a later `/save` call (or `save PATH` script verb) on a
    // memory-only session promotes it to autosaving without a restart.

    use std::time::Duration;

    const DEBOUNCE: Duration = Duration::from_millis(500);

    let mut rx = project.events().subscribe();
    tauri::async_runtime::spawn(async move {
        loop {
            // Block until the first mutation in a burst arrives.
            let first = match rx.recv().await {
                Ok(ev) => ev,
                Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
            };
            if !is_persistable(&first) {
                continue;
            }
            // Drain the rest of the burst with a debounce timer; each
            // new persistable event resets the timer.
            loop {
                match tokio::time::timeout(DEBOUNCE, rx.recv()).await {
                    Ok(Ok(ev)) => {
                        if is_persistable(&ev) {
                            // keep waiting — burst still arriving
                            continue;
                        }
                    }
                    Ok(Err(tokio::sync::broadcast::error::RecvError::Lagged(_))) => continue,
                    Ok(Err(tokio::sync::broadcast::error::RecvError::Closed)) => return,
                    Err(_) => break, // debounce window expired — flush
                }
            }
            // Quiet period reached: write the user's file (if any).
            let Some(target) = project.save_path() else {
                continue;
            };
            if let Err(e) = project.save_to_path(&target) {
                project.log(pcb_core::ActivityLevel::Error, format!("autosave: {e}"));
            }
        }
    });
}

fn is_persistable(ev: &pcb_core::Event) -> bool {
    use pcb_core::Event;
    !matches!(
        ev,
        Event::Activity { .. }
            | Event::PlacementProgress { .. }
            | Event::LibraryChanged { .. }
            | Event::PendingLibraryChanged { .. }
    )
}

/// Local HTTP API. Stateless — every request is independent, so the
/// agent never has to re-establish a session. Three endpoints:
///   GET  /         → `USAGE` + the script reference (text/plain)
///   GET  /health   → `{"ok": true}`
///   POST /script   → run a script; body `{"script": "..."}`, reply
///                    is whatever `tools::dispatch` returned.
///
/// Implementation is a hand-rolled HTTP/1.1 reader rather than pulling
/// in axum/hyper — the surface is three routes and we already have
/// tokio::net.
fn spawn_http_api(project: Project, addr: String) {
    use tokio::net::TcpListener;
    let project_for_log = project.clone();
    tauri::async_runtime::spawn(async move {
        let listener = match TcpListener::bind(&addr).await {
            Ok(l) => l,
            Err(e) => {
                project_for_log.log(
                    pcb_core::ActivityLevel::Error,
                    format!("api: bind {addr}: {e}"),
                );
                return;
            }
        };
        loop {
            let (sock, _peer) = match listener.accept().await {
                Ok(t) => t,
                Err(e) => {
                    project_for_log.log(pcb_core::ActivityLevel::Warn, format!("api: accept: {e}"));
                    continue;
                }
            };
            let project_for_conn = project.clone();
            tokio::spawn(async move {
                let _ = http::serve_one(sock, project_for_conn).await;
            });
        }
    });
}

mod http {
    //! Minimal HTTP/1.1 handler for the local API. Single connection =
    //! single request (Connection: close); good enough for `curl` from
    //! the agent and the local UI's debug panel. Not a general-purpose
    //! server — don't expose it past loopback.

    use pcb_core::Project;
    use serde_json::Value;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpStream;

    use crate::{collect_placement_margins, USAGE};

    /// Intermediate result from `handle_screenshot`'s synchronous
    /// render block. Kept outside the function so clippy doesn't
    /// complain about items declared after statements.
    enum Rendered {
        Bytes(Result<Vec<u8>, String>),
        UnknownView,
    }

    pub async fn serve_one(mut sock: TcpStream, project: Project) -> std::io::Result<()> {
        let (head, body_start) = match read_head(&mut sock).await? {
            Some(parts) => parts,
            None => {
                return write_status(&mut sock, 400, "Bad Request", "text/plain", b"bad request")
                    .await
            }
        };
        let mut lines = head.split("\r\n");
        let request_line = lines.next().unwrap_or_default();
        let mut parts = request_line.split_ascii_whitespace();
        let method = parts.next().unwrap_or_default();
        let path = parts.next().unwrap_or_default();

        let mut content_length: usize = 0;
        for h in lines {
            if let Some(rest) = h
                .strip_prefix("Content-Length:")
                .or_else(|| h.strip_prefix("content-length:"))
            {
                if let Ok(n) = rest.trim().parse::<usize>() {
                    content_length = n;
                }
            }
        }

        // Drain the body up to Content-Length.
        let mut body = body_start;
        if body.len() < content_length {
            let mut rest = vec![0u8; content_length - body.len()];
            sock.read_exact(&mut rest).await?;
            body.extend_from_slice(&rest);
        }
        body.truncate(content_length);

        // Split path from query string for routes that take params
        // (`/screenshot?view=board&width=2000`).
        let (route, query) = match path.split_once('?') {
            Some((r, q)) => (r, q),
            None => (path, ""),
        };

        match (method, route) {
            ("GET", "/") => {
                let reference = pcb_script::tools::script_reference();
                let mut out = String::new();
                out.push_str(USAGE);
                out.push_str("\n--- SCRIPT REFERENCE ---\n");
                out.push_str(reference);
                write_text(&mut sock, 200, "OK", &out).await
            }
            ("GET", "/help") => {
                let mut out = String::new();
                out.push_str(USAGE);
                out.push_str("\n--- SCRIPT REFERENCE ---\n");
                out.push_str(pcb_script::tools::script_reference());
                write_text(&mut sock, 200, "OK", &out).await
            }
            ("GET", "/health") => write_text(&mut sock, 200, "OK", "ok\n").await,
            ("GET", "/screenshot") => handle_screenshot(&mut sock, &project, query).await,
            ("POST", "/script") => handle_script(&mut sock, &project, &body).await,
            ("POST", "/save") => handle_save(&mut sock, &project, &body).await,
            _ => write_text(&mut sock, 404, "Not Found", "unknown route\n").await,
        }
    }

    /// `GET /screenshot[?view=board|schematic][&width=<px>]` — rasterise
    /// the project's current SVG to a PNG and return it inline. The
    /// SVG is regenerated from the live `Project`, so the response is
    /// always up to date with whatever the script last mutated.
    ///
    /// We deliberately re-render the model rather than capturing the
    /// webview's actual pixels: on macOS, screen-capture APIs require
    /// the user to grant Accessibility permission to the `fragua`
    /// binary, which breaks the "agent boots a fresh build and verifies
    /// its work" loop. Re-rendering the same SVG is what the user sees
    /// anyway (minus any human pan/zoom state).
    async fn handle_screenshot(
        sock: &mut TcpStream,
        project: &Project,
        query: &str,
    ) -> std::io::Result<()> {
        let params = parse_query(query);
        let view = params
            .iter()
            .find(|(k, _)| k == "view")
            .map_or("board", |(_, v)| v.as_str());
        let width: u32 = params
            .iter()
            .find(|(k, _)| k == "width")
            .and_then(|(_, v)| v.parse().ok())
            .unwrap_or(pcb_render::DEFAULT_PNG_WIDTH);

        // Render the PNG synchronously in a scoped block so the
        // `ProjectSnapshot` (an `RwLockReadGuard`) is dropped before any
        // `.await` — the guard is `!Send`, which would otherwise make
        // this future un-spawnable on the multi-threaded runtime.
        let rendered = {
            let margins = collect_placement_margins(project);
            let snap = project.read();
            match view {
                "board" => Rendered::Bytes(pcb_render::render_board_png_with_margins(
                    snap.board(),
                    &margins,
                    width,
                )),
                "schematic" | "sch" => {
                    Rendered::Bytes(pcb_render::render_schematic_png(snap.schematic(), width))
                }
                _ => Rendered::UnknownView,
            }
        };
        let png_result = match rendered {
            Rendered::Bytes(r) => r,
            Rendered::UnknownView => {
                return write_text(
                    sock,
                    400,
                    "Bad Request",
                    &format!("unknown view `{view}`; expected `board` or `schematic`\n"),
                )
                .await;
            }
        };

        match png_result {
            Ok(bytes) => {
                project.log(
                    pcb_core::ActivityLevel::Info,
                    format!("api.screenshot: {view} {} bytes", bytes.len()),
                );
                write_status(sock, 200, "OK", "image/png", &bytes).await
            }
            Err(e) => write_text(sock, 500, "Internal Server Error", &format!("render: {e}\n"))
                .await,
        }
    }

    /// Parse `k1=v1&k2=v2` into pairs. No percent-decoding — our keys
    /// and values are ASCII identifiers / integers. Unknown shapes (no
    /// `=`, trailing `&`) are silently skipped.
    fn parse_query(q: &str) -> Vec<(String, String)> {
        if q.is_empty() {
            return Vec::new();
        }
        q.split('&')
            .filter_map(|pair| {
                let (k, v) = pair.split_once('=')?;
                if k.is_empty() {
                    return None;
                }
                Some((k.to_string(), v.to_string()))
            })
            .collect()
    }

    async fn handle_save(
        sock: &mut TcpStream,
        project: &Project,
        body: &[u8],
    ) -> std::io::Result<()> {
        let parsed: Value = match serde_json::from_slice(body) {
            Ok(v) => v,
            Err(e) => {
                return write_text(
                    sock,
                    400,
                    "Bad Request",
                    &format!("invalid json body: {e}\n"),
                )
                .await;
            }
        };
        let path = match parsed.get("path").and_then(Value::as_str) {
            Some(p) if !p.trim().is_empty() => p.to_string(),
            _ => {
                return write_text(sock, 400, "Bad Request", "missing or empty `path`\n").await;
            }
        };
        match project.save_to_path(std::path::Path::new(&path)) {
            Ok(written) => {
                project.log(
                    pcb_core::ActivityLevel::Info,
                    format!("api.save: wrote {}", written.display()),
                );
                let body = format!(
                    "Saved to {p}\nAutosave is now bound to this path; subsequent edits write here.\n",
                    p = written.display(),
                );
                write_text(sock, 200, "OK", &body).await
            }
            Err(e) => write_text(sock, 400, "Bad Request", &format!("save failed: {e}\n")).await,
        }
    }

    async fn handle_script(
        sock: &mut TcpStream,
        project: &Project,
        body: &[u8],
    ) -> std::io::Result<()> {
        let args: Value = match serde_json::from_slice(body) {
            Ok(v) => v,
            Err(e) => {
                return write_text(
                    sock,
                    400,
                    "Bad Request",
                    &format!("invalid json body: {e}\n"),
                )
                .await;
            }
        };
        match pcb_script::tools::dispatch(project, "script", &args).await {
            Ok(value) => {
                let mut text = format_script_result(&value);
                if project.save_path().is_none() {
                    text.push_str(unsaved_warning());
                }
                write_text(sock, 200, "OK", &text).await
            }
            Err(err) => {
                let mut text = format!(
                    "script error ({code}): {msg}\n",
                    code = err.code,
                    msg = err.message
                );
                if project.save_path().is_none() {
                    text.push_str(unsaved_warning());
                }
                write_text(sock, 400, "Bad Request", &text).await
            }
        }
    }

    /// Render the script tool's structured reply into the text shape an
    /// AI agent reads naturally:
    ///   <summary>
    ///   [L<line> ok|FAIL <tool>] <inner text or error>
    ///   ...
    fn format_script_result(value: &Value) -> String {
        let mut out = String::new();
        if let Some(summary) = value.pointer("/content/0/text").and_then(Value::as_str) {
            out.push_str(summary);
            out.push('\n');
        }
        if let Some(results) = value
            .pointer("/structuredContent/results")
            .and_then(Value::as_array)
        {
            for r in results {
                let line = r.get("line").and_then(Value::as_u64).unwrap_or(0);
                let tool = r.get("tool").and_then(Value::as_str).unwrap_or("?");
                let ok = r.get("ok").and_then(Value::as_bool).unwrap_or(false);
                let body = if ok {
                    r.pointer("/result/content/0/text")
                        .and_then(Value::as_str)
                        .map(str::to_owned)
                        .unwrap_or_else(|| {
                            // Tool returned no text content: fall back to the
                            // structured result so the agent sees something.
                            r.get("result").map(|v| v.to_string()).unwrap_or_default()
                        })
                } else {
                    r.get("error")
                        .and_then(Value::as_str)
                        .map(str::to_owned)
                        .unwrap_or_else(|| "(no error message)".into())
                };
                let status = if ok { "ok" } else { "FAIL" };
                out.push_str(&format!("[L{line} {status} {tool}] {body}\n"));
            }
        }
        out
    }

    fn unsaved_warning() -> &'static str {
        "\nWARNING: this Fragua session is memory-only — no autosave is configured. \
The current project will be lost on exit unless you persist it with \
`POST /save {\"path\": \"...\"}` or include a `save PATH` line in the next \
script. After the first save, autosave rebinds to that path automatically.\n"
    }

    async fn read_head(sock: &mut TcpStream) -> std::io::Result<Option<(String, Vec<u8>)>> {
        // Read until "\r\n\r\n" — cap at 16 KB so a stuck client can't
        // exhaust memory. Body bytes that arrived in the same read get
        // returned alongside the head.
        const MAX_HEAD: usize = 16 * 1024;
        let mut buf = Vec::with_capacity(1024);
        let mut tmp = [0u8; 1024];
        loop {
            let n = sock.read(&mut tmp).await?;
            if n == 0 {
                return Ok(None);
            }
            buf.extend_from_slice(&tmp[..n]);
            if buf.len() > MAX_HEAD {
                return Ok(None);
            }
            if let Some(idx) = find_double_crlf(&buf) {
                let head = String::from_utf8_lossy(&buf[..idx]).to_string();
                let body_start = buf[idx + 4..].to_vec();
                return Ok(Some((head, body_start)));
            }
        }
    }

    fn find_double_crlf(buf: &[u8]) -> Option<usize> {
        buf.windows(4).position(|w| w == b"\r\n\r\n")
    }

    async fn write_text(
        sock: &mut TcpStream,
        code: u16,
        reason: &str,
        body: &str,
    ) -> std::io::Result<()> {
        write_status(
            sock,
            code,
            reason,
            "text/plain; charset=utf-8",
            body.as_bytes(),
        )
        .await
    }

    async fn write_status(
        sock: &mut TcpStream,
        code: u16,
        reason: &str,
        content_type: &str,
        body: &[u8],
    ) -> std::io::Result<()> {
        let header = format!(
            "HTTP/1.1 {code} {reason}\r\nContent-Type: {ct}\r\nContent-Length: {len}\r\nConnection: close\r\n\r\n",
            ct = content_type,
            len = body.len(),
        );
        sock.write_all(header.as_bytes()).await?;
        sock.write_all(body).await?;
        sock.flush().await
    }
}
