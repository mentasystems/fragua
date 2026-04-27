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
    mcp_addr: String,
    svg: String,
}

#[tauri::command]
fn project_state(state: State<'_, AppState>) -> ProjectStatePayload {
    let snap = state.project.read();
    ProjectStatePayload {
        name: snap.name().to_string(),
        footprint_count: snap.board().footprints.len(),
        mcp_addr: state.mcp_addr.clone(),
        svg: pcb_render::render_svg(snap.board()),
    }
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
                offset: Point::new(Length::from_mm(-1.0), Length::ZERO),
                size: (Length::from_mm(1.0), Length::from_mm(1.2)),
                layer: CopperLayer::Top,
                net: None,
            },
            Pad {
                number: "2".into(),
                offset: Point::new(Length::from_mm(1.0), Length::ZERO),
                size: (Length::from_mm(1.0), Length::from_mm(1.2)),
                layer: CopperLayer::Top,
                net: None,
            },
        ],
    };
    state.project.add_footprint(footprint);
}

/// Entry point used by the binary in `main.rs`.
pub fn run() {
    let project = Project::new("untitled");
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
            spawn_mcp_server(project, mcp_addr.clone());
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![project_state, add_demo_resistor])
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
