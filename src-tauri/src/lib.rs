//! Tauri host. Owns the canonical `Project`, serves a tiny stateless
//! HTTP API on `127.0.0.1:7878` so an external agent can drive the
//! board, exposes commands to the webview, and re-emits project events
//! into the frontend's event bus.

use pcb_core::Project;
use serde::Serialize;
use tauri::{Emitter, State};

const API_DEFAULT_ADDR: &str = "127.0.0.1:7878";

/// Printed to stdout on launch and served at `GET /` so a fresh agent
/// can curl one URL and learn how to drive Fragua.
const USAGE: &str = "\
Fragua — AI-native PCB design tool.

USAGE
  fragua                 open with no project loaded (in-memory only)
  fragua <file.json>     load that file; autosave to it on every edit

LOCAL API
  Stateless HTTP on http://127.0.0.1:7878 (override: FRAGUA_API_ADDR).

  Replies are plain text (text/plain) — agent-friendly, no JSON parsing
  needed on the client side. Request bodies are JSON.

  GET  /                 usage + the full script reference
  POST /script           run a multi-line script
                         body:  {\"script\": \"...\"}
                         reply: per-line outcomes,
                                `[L<line> ok|FAIL <tool>] <text>`,
                                + an unsaved-session warning if any
  POST /save             write the current project to disk (atomic)
                         and bind autosave to that path
                         body:  {\"path\": \"/abs/or/rel/file.json\"}
                         reply: `Saved to <path>`
  GET  /health           `ok`

  Examples:
    curl -s http://127.0.0.1:7878/script \\
      -H 'content-type: application/json' \\
      -d '{\"script\": \"outline 80 30\\nstatus\"}'

    curl -s http://127.0.0.1:7878/save \\
      -H 'content-type: application/json' \\
      -d '{\"path\": \"/tmp/board.json\"}'

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
        api_addr: state.api_addr.clone(),
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
    // First thing on launch — print the usage AND the full script
    // language reference so the operator (or the agent that just
    // spawned the process) sees the entire surface without grepping the
    // source or hitting `GET /` first.
    print!("{USAGE}");
    println!("\n--- SCRIPT REFERENCE ---\n");
    print!("{}", pcb_script::tools::script_reference());
    let _ = std::io::Write::flush(&mut std::io::stdout());

    // CLI: `fragua` (no args) → empty in-memory project, no autosave.
    // `fragua <file.json>` → load that file (or start empty if missing
    // / unreadable) and autosave back to it. The autosave target lives
    // on the project itself, so a later `POST /save` (or `save PATH`
    // verb) rebinds it without restart.
    let cli_path = std::env::args_os().nth(1).map(std::path::PathBuf::from);
    let project = match cli_path {
        Some(path) => Project::load_from_path(&path).unwrap_or_else(|| {
            let p = Project::new(name_from_path(&path));
            p.set_save_path(Some(path.clone()));
            p
        }),
        None => Project::new(""),
    };
    let api_addr = std::env::var("FRAGUA_API_ADDR").unwrap_or_else(|_| API_DEFAULT_ADDR.to_string());

    let state = AppState {
        project: project.clone(),
        api_addr: api_addr.clone(),
    };

    tauri::Builder::default()
        .manage(state)
        .setup(move |app| {
            let handle = app.handle().clone();
            spawn_event_pump(handle, project.clone());
            spawn_animation_pump(project.clone());
            spawn_autosave(project.clone());
            spawn_http_api(project, api_addr.clone());
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
            let Some(target) = project.save_path() else { continue };
            if let Err(e) = project.save_to_path(&target) {
                project.log(
                    pcb_core::ActivityLevel::Error,
                    format!("autosave: {e}"),
                );
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
                    project_for_log.log(
                        pcb_core::ActivityLevel::Warn,
                        format!("api: accept: {e}"),
                    );
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

    use crate::USAGE;

    pub async fn serve_one(mut sock: TcpStream, project: Project) -> std::io::Result<()> {
        let (head, body_start) = match read_head(&mut sock).await? {
            Some(parts) => parts,
            None => return write_status(&mut sock, 400, "Bad Request", "text/plain", b"bad request").await,
        };
        let mut lines = head.split("\r\n");
        let request_line = lines.next().unwrap_or_default();
        let mut parts = request_line.split_ascii_whitespace();
        let method = parts.next().unwrap_or_default();
        let path = parts.next().unwrap_or_default();

        let mut content_length: usize = 0;
        for h in lines {
            if let Some(rest) = h.strip_prefix("Content-Length:").or_else(|| h.strip_prefix("content-length:")) {
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

        match (method, path) {
            ("GET", "/") => {
                let reference = pcb_script::tools::script_reference();
                let mut out = String::new();
                out.push_str(USAGE);
                out.push_str("\n--- SCRIPT REFERENCE ---\n");
                out.push_str(reference);
                write_text(&mut sock, 200, "OK", &out).await
            }
            ("GET", "/health") => write_text(&mut sock, 200, "OK", "ok\n").await,
            ("POST", "/script") => handle_script(&mut sock, &project, &body).await,
            ("POST", "/save") => handle_save(&mut sock, &project, &body).await,
            _ => write_text(&mut sock, 404, "Not Found", "unknown route\n").await,
        }
    }

    async fn handle_save(sock: &mut TcpStream, project: &Project, body: &[u8]) -> std::io::Result<()> {
        let parsed: Value = match serde_json::from_slice(body) {
            Ok(v) => v,
            Err(e) => {
                return write_text(sock, 400, "Bad Request", &format!("invalid json body: {e}\n")).await;
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

    async fn handle_script(sock: &mut TcpStream, project: &Project, body: &[u8]) -> std::io::Result<()> {
        let args: Value = match serde_json::from_slice(body) {
            Ok(v) => v,
            Err(e) => {
                return write_text(sock, 400, "Bad Request", &format!("invalid json body: {e}\n")).await;
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
                let mut text = format!("script error ({code}): {msg}\n",
                    code = err.code, msg = err.message);
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
        if let Some(summary) = value
            .pointer("/content/0/text")
            .and_then(Value::as_str)
        {
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
                            r.get("result")
                                .map(|v| v.to_string())
                                .unwrap_or_default()
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
        write_status(sock, code, reason, "text/plain; charset=utf-8", body.as_bytes()).await
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
