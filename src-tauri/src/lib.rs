//! Tauri host. Owns the canonical `Project`, runs the MCP server on
//! `127.0.0.1:7878`, exposes commands to the webview, and re-emits
//! project events into the frontend's event bus.

use pcb_core::Project;
use pcb_mcp::McpServer;
use serde::Serialize;
use tauri::{Emitter, State};

const MCP_DEFAULT_ADDR: &str = "127.0.0.1:7878";

/// Wrapper kept in Tauri state so commands can read project + addr.
struct AppState {
    project: Project,
    mcp_addr: String,
}

#[derive(Serialize)]
struct ProjectStatePayload {
    name: String,
    footprint_count: usize,
    symbol_count: usize,
    net_count: usize,
    palette_count: usize,
    palette: Vec<PalettePayload>,
    mcp_addr: String,
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
    // Render the VISIBLE mirror (lags `live` by the animation cadence)
    // — that's what the user sees on the canvas. The agent's own
    // read-tools (`view.snapshot`, `view.summary`) read live for an
    // accurate, instant view of state after a script runs.
    let snap = state.project.read_visible();
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
        mcp_addr: state.mcp_addr.clone(),
        board_svg: pcb_render::render_svg(snap.board()),
        schematic_svg: pcb_render::render_schematic_svg(snap.schematic()),
        outline,
    }
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
        .map(|e| serde_json::json!({
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
        }))
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
        .map(|p| serde_json::json!({
            "number": p.number,
            "name": p.name,
            "net": p.net,
            "layer": match p.layer {
                pcb_core::CopperLayer::Top => "top",
                pcb_core::CopperLayer::Bottom => "bottom",
            },
        }))
        .collect();

    let library = lib_entry.map(|e| serde_json::json!({
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
    }));

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

/// Set the rectangular Edge.Cuts outline of the board.
#[tauri::command]
fn set_board_outline(state: State<'_, AppState>, w_mm: f64, h_mm: f64) -> Result<(), String> {
    if w_mm < 1.0 || h_mm < 1.0 {
        return Err("dimensions must be at least 1 mm".to_string());
    }
    state.project.set_outline(pcb_core::Rect::from_corners(
        pcb_core::Point::new(pcb_core::Length::from_mm(0.0), pcb_core::Length::from_mm(0.0)),
        pcb_core::Point::new(pcb_core::Length::from_mm(w_mm), pcb_core::Length::from_mm(h_mm)),
    ));
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
    state
        .project
        .rotate_footprint(&reference, next)
        .map(|_| ())
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
    let snap = state.project.read();
    let report = pcb_drc::run(snap.board(), &pcb_drc::DrcOptions::default());
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
        .filter_map(|(n, o)| {
            matches!(o, pcb_router::Outcome::Failed { .. }).then_some(n.as_str())
        })
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
    state
        .project
        .log(pcb_core::ActivityLevel::Info, format!("route.run: {summary}"));
    Ok(summary)
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
            },
            Pad {
                number: "2".into(),
                name: String::new(),
                offset: Point::new(Length::from_mm(1.0), Length::ZERO),
                size: (Length::from_mm(1.0), Length::from_mm(1.2)),
                layer: CopperLayer::Top,
                net: None,
            },
        ],
        key: String::new(),
        description: String::new(),
        edge_mounted: false,
    };
    state.project.add_footprint(footprint);
}

/// Entry point used by the binary in `main.rs`.
pub fn run() {
    // Restore the most recent state if `~/.pcb-projects/untitled/current.json`
    // exists; otherwise start fresh. The auto-save loop below will keep
    // the file updated as the agent works.
    let project = Project::load_default("untitled").unwrap_or_else(|| Project::new("untitled"));
    let mcp_addr = std::env::var("PCB_MCP_ADDR").unwrap_or_else(|_| MCP_DEFAULT_ADDR.to_string());

    let state = AppState {
        project: project.clone(),
        mcp_addr: mcp_addr.clone(),
    };

    tauri::Builder::default()
        .manage(state)
        .setup(move |app| {
            let handle = app.handle().clone();
            spawn_event_pump(handle, project.clone());
            spawn_animation_pump(project.clone());
            spawn_autosave(project.clone());
            spawn_mcp_server(project, mcp_addr.clone());
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            project_state,
            add_demo_resistor,
            reset_project,
            reset_placement,
            reset_route,
            set_board_outline,
            place_from_palette,
            move_footprint,
            rotate_footprint,
            run_router,
            run_drc,
            export_fab_pack,
            library_state,
            library_attachment_data_uri,
            component_info
        ])
        .run(tauri::generate_context!())
        .expect("tauri runtime");
}

/// Subscribe to the project event bus and forward every event into the
/// webview as `pcb://event`. Errors (lagged subscriber, send failure)
/// are non-fatal — the next event will catch up.
/// Drive the project's animation mirror: every `ANIMATION_TICK_MS`,
/// pop one Mutation from the pending queue, apply it to `visible`,
/// and emit the corresponding Event. The agent never blocks on this
/// — mutations land in `live` instantly; only the UI's view (which
/// reads `visible`) catches up frame-by-frame.
fn spawn_animation_pump(project: Project) {
    use std::time::Duration;
    const TICK: Duration = Duration::from_millis(150);
    tauri::async_runtime::spawn(async move {
        loop {
            tokio::time::sleep(TICK).await;
            // Drain whatever is pending — but only one per tick so the
            // animation paces. If the agent has queued up a long burst,
            // the queue catches up gradually.
            project.tick();
        }
    });
}

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
fn spawn_autosave(project: Project) {
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::time::Duration;

    const DEBOUNCE: Duration = Duration::from_millis(500);
    const HISTORY_EVERY_N_SAVES: u64 = 10;

    let mut rx = project.events().subscribe();
    let save_counter = std::sync::Arc::new(AtomicU64::new(0));
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
            // Quiet period reached: write current.json, possibly snapshot.
            match project.save_to_default() {
                Ok(_) => {
                    let n = save_counter.fetch_add(1, Ordering::Relaxed) + 1;
                    if n % HISTORY_EVERY_N_SAVES == 0 {
                        if let Err(e) = project.snapshot_history() {
                            project.log(
                                pcb_core::ActivityLevel::Warn,
                                format!("autosave: history snapshot failed: {e}"),
                            );
                        }
                    }
                }
                Err(e) => {
                    project.log(
                        pcb_core::ActivityLevel::Error,
                        format!("autosave: {e}"),
                    );
                }
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
    )
}

fn spawn_mcp_server(project: Project, addr: String) {
    let server = McpServer::new(project.clone());
    let project_for_log = project;
    tauri::async_runtime::spawn(async move {
        match server.run_tcp(&addr).await {
            Ok(()) => {}
            Err(e) => {
                project_for_log.log(
                    pcb_core::ActivityLevel::Error,
                    format!("mcp server: {e}"),
                );
            }
        }
    });
}
