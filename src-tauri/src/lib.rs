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

/// Run the simulated-annealing placer on every palette item plus any
/// footprint currently outside the board outline (so dragging a
/// footprint past the edge and pressing auto-place repositions it).
/// Footprints fully inside the board are treated as locked obstacles.
#[tauri::command]
async fn run_auto_placement(state: State<'_, AppState>) -> Result<(), String> {
    // Pull stragglers back into the palette before the simulation.
    let _ = state.project.unplace_out_of_bounds();
    use pcb_core::Footprint;

    let project = state.project.clone();
    let bounds = project
        .read()
        .board()
        .outline
        .ok_or_else(|| "set the board outline first".to_string())?;

    // Build placer input from the live state. Same shape as the MCP
    // tool — code is intentionally duplicated here so the UI doesn't
    // round-trip through MCP for its own button.
    struct Item {
        reference: String,
        bbox_w: pcb_core::Length,
        bbox_h: pcb_core::Length,
        position: pcb_core::Point,
        locked: bool,
        footprint: Footprint,
        is_palette: bool,
    }
    let mut items: Vec<Item> = Vec::new();
    {
        let snap = project.read();
        for fp in snap.board().footprints_in_order() {
            let r = fp.bounds().unwrap_or_else(|| {
                pcb_core::Rect::from_corners(fp.position, fp.position)
            });
            items.push(Item {
                reference: fp.reference.clone(),
                bbox_w: r.width(),
                bbox_h: r.height(),
                position: fp.position,
                locked: true,
                footprint: fp.clone(),
                is_palette: false,
            });
        }
        for fp in snap.palette() {
            let r = fp.bounds().unwrap_or_else(|| {
                pcb_core::Rect::from_corners(fp.position, fp.position)
            });
            items.push(Item {
                reference: fp.reference.clone(),
                bbox_w: r.width(),
                bbox_h: r.height(),
                position: fp.position,
                locked: false,
                footprint: fp.clone(),
                is_palette: true,
            });
        }
    }
    let palette_count = items.iter().filter(|i| i.is_palette).count();
    if palette_count == 0 {
        return Ok(());
    }

    // Sprinkle palette items inside the bounds.
    {
        let n = palette_count as f64;
        let cols = (n.sqrt().ceil()).max(1.0);
        let bx = bounds.min.x.to_mm();
        let by = bounds.min.y.to_mm();
        let bw = (bounds.max.x - bounds.min.x).to_mm();
        let bh = (bounds.max.y - bounds.min.y).to_mm();
        let dx = bw / (cols + 1.0);
        let dy = bh / (cols + 1.0);
        let mut pi = 0_f64;
        for item in items.iter_mut().filter(|i| i.is_palette) {
            let row = (pi / cols).floor();
            let col = pi - row * cols;
            item.position = pcb_core::Point::new(
                pcb_core::Length::from_mm(bx + dx * (col + 1.0)),
                pcb_core::Length::from_mm(by + dy * (row + 1.0)),
            );
            item.footprint.position = item.position;
            pi += 1.0;
        }
    }

    let placeable: Vec<pcb_placer::PlaceableFootprint> = items
        .iter()
        .map(|i| pcb_placer::PlaceableFootprint {
            reference: i.reference.clone(),
            bbox_w: i.bbox_w,
            bbox_h: i.bbox_h,
            position: i.position,
            rotation: i.footprint.rotation,
            locked: i.locked,
            footprint: i.footprint.clone(),
        })
        .collect();
    let palette_refs: std::collections::HashSet<String> = items
        .iter()
        .filter(|i| i.is_palette)
        .map(|i| i.reference.clone())
        .collect();

    let nets: Vec<Vec<String>> = {
        let snap = project.read();
        let sch = snap.schematic();
        sch.nets
            .values()
            .map(|n| {
                let mut refs: Vec<String> = n
                    .connections
                    .iter()
                    .filter_map(|c| sch.symbols.get(&c.symbol_id).map(|s| s.reference.clone()))
                    .collect();
                refs.sort();
                refs.dedup();
                refs
            })
            .filter(|v| v.len() >= 2)
            .collect()
    };

    let mut placer = pcb_placer::Placer::new(
        pcb_placer::PlacementInput {
            footprints: placeable,
            nets,
            bounds: Some(bounds),
        },
        pcb_placer::PlacerOptions {
            total_steps: 200,
            ..Default::default()
        },
    );

    for reference in &palette_refs {
        let item = items
            .iter()
            .find(|i| &i.reference == reference)
            .expect("palette ref present");
        let _ = project.place_from_palette(reference, item.position);
    }
    project.clear_routing();

    const ITERATIONS: u32 = 200;
    const FRAME_EVERY: u32 = 4;
    const FRAME_DELAY_MS: u64 = 100;
    for i in 0..ITERATIONS {
        let frame = placer.step();
        if i % FRAME_EVERY == 0 || i == ITERATIONS - 1 {
            for (reference, position) in &frame.positions {
                if !palette_refs.contains(reference) {
                    continue;
                }
                let _ = project.move_footprint_to(reference, *position);
            }
            project
                .events()
                .publish(pcb_core::Event::PlacementProgress {
                    iteration: frame.iteration,
                });
            tokio::time::sleep(std::time::Duration::from_millis(FRAME_DELAY_MS)).await;
        }
    }
    placer.finalise();
    for fp in placer.current() {
        if palette_refs.contains(&fp.reference) {
            let _ = project.move_footprint_to(&fp.reference, fp.position);
            let _ = project.rotate_footprint(&fp.reference, fp.rotation);
        }
    }
    project.log(
        pcb_core::ActivityLevel::Info,
        format!("placement.auto: settled after {ITERATIONS} iterations (UI button)"),
    );
    Ok(())
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
        .invoke_handler(tauri::generate_handler![
            project_state,
            add_demo_resistor,
            reset_project,
            set_board_outline,
            place_from_palette,
            move_footprint,
            rotate_footprint,
            run_auto_placement,
            run_router,
            export_fab_pack
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
