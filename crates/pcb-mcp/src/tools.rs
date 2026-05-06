//! Tool surface — what the MCP client (the AI agent) can call.
//!
//! Each tool is intentionally thin: parse the input, call into
//! `pcb-core` to mutate the project, return the result. The agent owns
//! all the design reasoning; tools are pure data primitives.

use pcb_core::schematic::{Net, NetConnection, PinSide, SchPin, Symbol, SymbolKind};
use pcb_core::{ActivityLevel, CopperLayer, Footprint, Length, Pad, Point, Pour, Project, Trace, Via};
use serde::Deserialize;
use serde_json::{json, Value};

use crate::protocol::error_code::INVALID_PARAMS;

pub struct ToolError {
    pub code: i64,
    pub message: String,
}

impl ToolError {
    pub fn invalid_params(msg: impl Into<String>) -> Self {
        Self {
            code: INVALID_PARAMS,
            message: msg.into(),
        }
    }
}

/// Static catalog returned by `tools/list` — exposes ONLY the `script`
/// tool. Every internal action stays callable via `dispatch` (the
/// script runner uses it), but the agent only loads one schema and one
/// description, keeping context lean.
#[must_use]
pub fn catalog() -> Value {
    json!([
        {
            "name": "script",
            "description": "Run a multi-line PCB design script — the ONLY tool you need. \
The script is plain text, one action per line; multi-line blocks (`sym`, `lib`) \
take indented sub-lines (`pin`, `pad`). Strings with spaces use double quotes; \
trailing key=value pairs override defaults; `#` starts a comment.\n\
\n\
=== EXAMPLE ===\n\
reset\n\
outline 90 30\n\
\n\
sym U1 ic key=esp32_s3_zero desc=\"ESP32 main MCU; USB-C edge\"\n\
  pin 1 L V5\n\
  pin 2 L GND\n\
  pin 3 L 3V3\n\
sym C1 capacitor key=c_0603 value=100nF desc=\"HF decoupling near U2.VCC\"\n\
\n\
net GND U1.GND U2.GND_1 C1.2\n\
net +3V3 U1.3V3 U2.VCC C1.1\n\
\n\
palette U1 esp32_s3_zero rot=90\n\
palette C1 c_0603 value=100nF\n\
place U1 11.5 15 90\n\
place C1 48 14\n\
route\n\
drc\n\
=== END EXAMPLE ===\n\
\n\
=== ACTIONS (verb args) ===\n\
\n\
PROJECT / READS:\n\
  reset                                        — wipe schematic, board, palette\n\
  status                                       — terse project summary (footprint count, name)\n\
  view                                         — full board summary: outline + nets + DRC counts (NO svg)\n\
  snap                                         — full SVG + structured pad/trace data (heavy)\n\
  sch                                          — schematic SVG\n\
  sch-status                                   — schematic counts + unconnected pins\n\
  nets                                         — per-net pad-by-pad connection report\n\
  list-lib                                     — every library entry (key, desc, pad count, attachments)\n\
  list-palette                                 — items waiting in the palette\n\
\n\
BOARD:\n\
  outline W H                                  — set Edge.Cuts rectangle in mm\n\
\n\
LIBRARY (build first, reuse forever):\n\
  lib KEY [value=V] [rot=N] [edge=true|false] [desc=\"...\"]\n\
    pad NUMBER X Y W H [name=NAME]             — repeat for every pad\n\
  attach KEY KIND PATH                         — file from disk; mime auto-detected\n\
                                                 KIND is free text: photo / datasheet / note / ...\n\
  detach KEY ATTACHMENT_ID\n\
  delete-lib KEY\n\
  find-lib KEY                                 — full record + pads\n\
\n\
SCHEMATIC:\n\
  sym REF KIND [key=K] [value=V] [rot=N] [x=N] [y=N] [desc=\"...\"]\n\
    pin NUMBER SIDE [NAME]                     — only for KIND=ic; SIDE = L|R|T|B (or full names)\n\
                                                 KIND aliases: ic, r, c, l, led, d\n\
  net NAME PIN1 PIN2 ...                       — PIN = REF.PIN_NUMBER or REF.PIN_NAME (case-insensitive)\n\
\n\
PALETTE / PLACEMENT:\n\
  palette REF KEY [rot=N] [value=V] [layer=top|bottom]\n\
                                               — spawn a palette item from a library entry; the\n\
                                                 schematic must already have a symbol with REF\n\
  clear-palette\n\
  place REF X Y [ROT_DEG]                      — drop palette item at (x, y) mm; rejects if it\n\
                                                 overlaps another footprint or violates the\n\
                                                 edge_mounted constraint\n\
  move REF X Y\n\
  rotate REF DEG                               — absolute rotation, multiples of 90 recommended\n\
\n\
ROUTING:\n\
  route [trace_width=N] [clearance=N] [via_drill=N] [via_diameter=N] [via_cost=N] [cell=N]\n\
                                               — auto-route every net (A* on 2 layers); defaults\n\
                                                 trace_width=0.25, clearance=0.20, via_drill=0.30,\n\
                                                 via_diameter=0.60, cell=0.25, via_cost=8\n\
  clear-route                                  — drop all traces and vias\n\
  clear-net NET                                — clear one net's traces/vias\n\
  trace top|bottom NET X1 Y1 X2 Y2 [width=N]   — manual trace segment\n\
  via NET X Y [drill=N] [diameter=N]           — manual via\n\
  delete-trace ID                              — id from `snap` structured data\n\
  delete-via ID\n\
  pour NET top|bottom                          — declare a copper pour (ground/power plane); pads\n\
                                                 of NET on that layer count as connected without\n\
                                                 a routed trace. Cross-layer pads still need a via.\n\
                                                 Drop a `pour GND bottom` early on dense boards so\n\
                                                 the router does not have to thread GND everywhere.\n\
  clear-pour NET top|bottom                    — remove a pour\n\
\n\
DRC / EXPORT:\n\
  drc [clearance=N] [edge=N] [trace_width=N] [drill=N]\n\
                                               — design-rule check; defaults clearance=0.20,\n\
                                                 edge=0.30, trace_width=0.10, drill=0.20\n\
  export DIR [name=STEM]                       — write Gerbers + drill + BOM + pick-and-place\n\
\n\
=== RULES ===\n\
- One action per line. Indent (2 spaces / tab) = sub-line of previous block.\n\
- Strings with spaces: double-quote them. Comments: `#` at line start.\n\
- Numbers are decimals (mm or degrees). Booleans: true/false (or 1/0, yes/no).\n\
- The result is per-line; if line N fails, lines N+1..end still run.\n\
- For a single action, send a one-line script.\n\
- Order matters: lib before palette, sym before net, palette before place, place before route.\n\
- Footprints need ≥0.5 mm body-to-body gap (enforced); edge-mounted parts must touch the outline.\n\
\n\
=== DESIGN STRATEGY (how to fix things) ===\n\
The router is A* per-net with no copper fill. When `route` reports `not all\n\
nets routed` or `drc` reports `unconnected_pad` warnings on a star net (GND,\n\
+3V3), the placement is trapping the hub: existing signal traces saturate the\n\
corridor the hub needs. Resolve it BY MOVING COMPONENTS, not by hand-routing\n\
through the conflict.\n\
\n\
Workflow when warnings appear:\n\
  1. `nets`                    — see which pads are stranded.\n\
  2. Move 1-2 components       — `move REF X Y` (or `rotate REF DEG`) to open\n\
                                 a clear horizontal/vertical corridor along an\n\
                                 unused band of the board (typically y near\n\
                                 the outline edge or between component rows).\n\
  3. `clear-route` then `route` — re-route everything from the new placement.\n\
  4. `drc`                     — verify; loop if needed.\n\
\n\
Hand-routing (`trace`, `via`) only works for short bridges in known-empty\n\
zones; on a populated board you will almost always hit `trace_trace_clearance`\n\
errors. Re-place first, route second."
,
            "inputSchema": {
                "type": "object",
                "required": ["script"],
                "properties": {
                    "script": {
                        "type": "string",
                        "description": "multi-line PCB design script (see tool description for syntax + every action)"
                    }
                },
                "additionalProperties": false
            }
        }
    ])
}

/// Dispatch a `tools/call` to the right handler.
pub async fn dispatch(project: &Project, name: &str, args: &Value) -> Result<Value, ToolError> {
    match name {
        "script" => tool_script(project, args).await,
        "batch" => tool_batch(project, args).await,
        "project.status" => tool_project_status(project),
        "project.reset" => tool_project_reset(project),
        "board.set_outline" => tool_board_set_outline(project, args),
        "placement.add" => tool_placement_add(project, args),
        "view.snapshot" => tool_view_snapshot(project),
        "view.summary" => tool_view_summary(project),
        "net.status" => tool_net_status(project),
        "schematic.add_symbol" => tool_schematic_add_symbol(project, args),
        "schematic.connect" => tool_schematic_connect(project, args),
        "schematic.status" => tool_schematic_status(project),
        "schematic.snapshot" => tool_schematic_snapshot(project),
        "palette.add" => tool_palette_add(project, args),
        "palette.list" => tool_palette_list(project),
        "palette.clear" => tool_palette_clear(project),
        "palette.add_from_library" => tool_palette_add_from_library(project, args),
        "library.list" => tool_library_list(project),
        "library.find" => tool_library_find(project, args),
        "library.create" => tool_library_create(project, args),
        "library.attach" => tool_library_attach(project, args),
        "library.delete_attachment" => tool_library_delete_attachment(project, args),
        "library.delete" => tool_library_delete(project, args),
        "placement.place_from_palette" => tool_place_from_palette(project, args),
        "placement.batch" => tool_placement_batch(project, args),
        "placement.move" => tool_placement_move(project, args),
        "placement.rotate" => tool_placement_rotate(project, args),
        "route.clear_net" => tool_route_clear_net(project, args),
        "route.delete_trace" => tool_route_delete_trace(project, args),
        "route.delete_via" => tool_route_delete_via(project, args),
        "route.add_trace" => tool_route_add_trace(project, args),
        "route.add_via" => tool_route_add_via(project, args),
        "route.clear" => tool_route_clear(project),
        "route.run" => tool_route_run(project, args),
        "pour.add" => tool_pour_add(project, args),
        "pour.remove" => tool_pour_remove(project, args),
        "drc.run" => tool_drc_run(project, args),
        "output.fab_pack" => tool_output_fab_pack(project, args),
        _ => Err(ToolError {
            code: crate::protocol::error_code::METHOD_NOT_FOUND,
            message: format!("unknown tool: {name}"),
        }),
    }
}

fn tool_project_reset(project: &Project) -> Result<Value, ToolError> {
    project.reset();
    project.log(ActivityLevel::Info, "project.reset");
    Ok(text_result("Project reset").into())
}

#[derive(Debug, Deserialize)]
struct BatchInput {
    ops: Vec<BatchOp>,
}

#[derive(Debug, Deserialize)]
struct BatchOp {
    tool: String,
    #[serde(default)]
    args: Value,
}

/// Run many tool calls sequentially in one MCP round-trip. Each op is
/// `{tool, args}`; the result mirrors the per-op outcome so the agent
/// can react granularly. `batch` itself is rejected as an op (no
/// nesting).
#[derive(Debug, Deserialize)]
struct ScriptInput {
    script: String,
}

/// Parse a multi-line DSL script and dispatch each line as a tool call.
/// On parse error, no commands run — we surface the line + message so
/// the agent can fix the script. On dispatch errors, later lines still
/// run; per-line outcomes come back in the result list.
async fn tool_script(project: &Project, args: &Value) -> Result<Value, ToolError> {
    let input: ScriptInput = serde_json::from_value(args.clone())
        .map_err(|e| ToolError::invalid_params(format!("script: {e}")))?;

    let cmds = match crate::script::parse(&input.script) {
        Ok(cmds) => cmds,
        Err(e) => {
            let msg = format!("script: parse error at line {}: {}", e.line, e.message);
            project.log(ActivityLevel::Error, msg.clone());
            return Err(ToolError::invalid_params(msg));
        }
    };

    let mut results = Vec::with_capacity(cmds.len());
    let mut ok_count = 0_usize;
    let mut fail_count = 0_usize;
    for cmd in cmds {
        match Box::pin(dispatch(project, &cmd.tool, &cmd.args)).await {
            Ok(v) => {
                ok_count += 1;
                results.push(json!({
                    "line": cmd.line,
                    "tool": cmd.tool,
                    "ok": true,
                    "result": v,
                }));
            }
            Err(e) => {
                fail_count += 1;
                results.push(json!({
                    "line": cmd.line,
                    "tool": cmd.tool,
                    "ok": false,
                    "error": e.message,
                    "code": e.code,
                }));
            }
        }
    }
    project.log(
        ActivityLevel::Info,
        format!("script: {ok_count} ok, {fail_count} failed"),
    );
    Ok(text_result(format!("script: {ok_count} ok, {fail_count} failed")).with_data(json!({
        "ok_count": ok_count,
        "fail_count": fail_count,
        "results": results,
    })))
}

async fn tool_batch(project: &Project, args: &Value) -> Result<Value, ToolError> {
    let input: BatchInput = serde_json::from_value(args.clone())
        .map_err(|e| ToolError::invalid_params(format!("batch: {e}")))?;
    let mut results = Vec::with_capacity(input.ops.len());
    let mut ok_count = 0_usize;
    let mut fail_count = 0_usize;
    for op in input.ops {
        if op.tool == "batch" {
            fail_count += 1;
            results.push(json!({
                "tool": "batch",
                "ok": false,
                "error": "batch cannot call itself",
                "code": INVALID_PARAMS,
            }));
            continue;
        }
        // `Box::pin` lets dispatch recurse into itself — async recursion
        // requires a heap-allocated future to give the type a known size.
        match Box::pin(dispatch(project, &op.tool, &op.args)).await {
            Ok(v) => {
                ok_count += 1;
                results.push(json!({
                    "tool": op.tool,
                    "ok": true,
                    "result": v,
                }));
            }
            Err(e) => {
                fail_count += 1;
                results.push(json!({
                    "tool": op.tool,
                    "ok": false,
                    "error": e.message,
                    "code": e.code,
                }));
            }
        }
    }
    project.log(
        ActivityLevel::Info,
        format!("batch: {ok_count} ok, {fail_count} failed"),
    );
    Ok(text_result(format!("batch: {ok_count} ok, {fail_count} failed")).with_data(json!({
        "ok_count": ok_count,
        "fail_count": fail_count,
        "results": results,
    })))
}

#[derive(Debug, Deserialize)]
struct SetOutlineInput {
    w_mm: f64,
    h_mm: f64,
}

fn tool_board_set_outline(project: &Project, args: &Value) -> Result<Value, ToolError> {
    let input: SetOutlineInput = serde_json::from_value(args.clone())
        .map_err(|e| ToolError::invalid_params(format!("board.set_outline: {e}")))?;
    let outline = pcb_core::Rect::from_corners(
        Point::new(Length::from_mm(0.0), Length::from_mm(0.0)),
        Point::new(Length::from_mm(input.w_mm), Length::from_mm(input.h_mm)),
    );
    project.set_outline(outline);
    project.log(
        ActivityLevel::Info,
        format!("board.set_outline: {:.1} × {:.1} mm", input.w_mm, input.h_mm),
    );
    Ok(text_result(format!(
        "Board outline set to {:.1} × {:.1} mm",
        input.w_mm, input.h_mm
    ))
    .with_data(json!({"w_mm": input.w_mm, "h_mm": input.h_mm})))
}

fn tool_project_status(project: &Project) -> Result<Value, ToolError> {
    let snap = project.read();
    let board = snap.board();
    let bounds = board.content_bounds().map(|r| {
        json!({
            "x_mm": r.min.x.to_mm(),
            "y_mm": r.min.y.to_mm(),
            "w_mm": r.width().to_mm(),
            "h_mm": r.height().to_mm(),
        })
    });
    Ok(text_result(format!(
        "project {name}: {n} footprint(s)",
        name = snap.name(),
        n = board.footprints.len(),
    ))
    .with_data(json!({
        "name": snap.name(),
        "footprint_count": board.footprints.len(),
        "content_bounds_mm": bounds,
    })))
}

#[derive(Debug, Deserialize)]
struct PlacementInput {
    reference: String,
    #[serde(default)]
    value: String,
    library: String,
    x_mm: f64,
    y_mm: f64,
    #[serde(default)]
    rotation: f32,
    #[serde(default = "default_layer")]
    layer: LayerInput,
    pads: Vec<PadInput>,
}

#[derive(Debug, Deserialize)]
struct PadInput {
    number: String,
    #[serde(default)]
    name: String,
    x_mm: f64,
    y_mm: f64,
    w_mm: f64,
    h_mm: f64,
    #[serde(default = "default_layer")]
    layer: LayerInput,
    #[serde(default)]
    net: Option<String>,
}

#[derive(Debug, Deserialize, Clone, Copy)]
#[serde(rename_all = "lowercase")]
enum LayerInput {
    Top,
    Bottom,
}

impl From<LayerInput> for CopperLayer {
    fn from(value: LayerInput) -> Self {
        match value {
            LayerInput::Top => Self::Top,
            LayerInput::Bottom => Self::Bottom,
        }
    }
}

fn default_layer() -> LayerInput {
    LayerInput::Top
}

fn tool_placement_add(project: &Project, args: &Value) -> Result<Value, ToolError> {
    let input: PlacementInput = serde_json::from_value(args.clone())
        .map_err(|e| ToolError::invalid_params(format!("placement.add: {e}")))?;

    let pads = input
        .pads
        .into_iter()
        .map(|p| Pad {
            number: p.number,
            name: p.name,
            offset: Point::new(Length::from_mm(p.x_mm), Length::from_mm(p.y_mm)),
            size: (Length::from_mm(p.w_mm), Length::from_mm(p.h_mm)),
            layer: p.layer.into(),
            net: p.net,
        })
        .collect();

    let footprint = Footprint {
        id: pcb_core::Id::new(),
        reference: input.reference.clone(),
        value: input.value,
        library: input.library,
        position: Point::new(Length::from_mm(input.x_mm), Length::from_mm(input.y_mm)),
        rotation: input.rotation,
        layer: input.layer.into(),
        pads,
        key: String::new(),
        description: String::new(),
        edge_mounted: false,
    };

    let id = project.add_footprint(footprint);
    project.log(
        ActivityLevel::Info,
        format!("placement.add: {} at ({:.2}, {:.2}) mm", input.reference, input.x_mm, input.y_mm),
    );
    Ok(text_result(format!("Placed {} ({})", input.reference, id.0))
        .with_data(json!({ "id": id.0.to_string(), "reference": input.reference })))
}

fn tool_view_snapshot(project: &Project) -> Result<Value, ToolError> {
    let snap = project.read();
    let board = snap.board();
    let svg = pcb_render::render_svg(board);

    // Structured introspection so the agent can act on the board
    // without parsing SVG: every footprint, trace, via with id, world
    // position, net.
    let outline = board.outline.map(|r| {
        json!({
            "x_mm": r.min.x.to_mm(),
            "y_mm": r.min.y.to_mm(),
            "w_mm": r.width().to_mm(),
            "h_mm": r.height().to_mm(),
        })
    });
    let footprints: Vec<Value> = board
        .footprints_in_order()
        .map(|fp| {
            // bbox in world coords gives the agent the rectangle the
            // footprint actually occupies after rotation, so it can
            // place neighbours without recomputing it.
            let bounds = fp.bounds();
            let bbox = bounds.map(|r| {
                json!({
                    "x_mm": r.min.x.to_mm(),
                    "y_mm": r.min.y.to_mm(),
                    "w_mm": r.width().to_mm(),
                    "h_mm": r.height().to_mm(),
                })
            });
            json!({
                "id": fp.id.0.to_string(),
                "reference": fp.reference,
                "value": fp.value,
                "library": fp.library,
                "key": fp.key,
                "description": fp.description,
                "edge_mounted": fp.edge_mounted,
                "x_mm": fp.position.x.to_mm(),
                "y_mm": fp.position.y.to_mm(),
                "rotation": fp.rotation,
                "bbox": bbox,
                "pads": fp.pads.iter().map(|p| {
                    let world = fp.pad_world_center(p);
                    let (pw, ph) = fp.pad_world_size(p);
                    json!({
                        "number": p.number,
                        "net": p.net,
                        "x_mm": world.x.to_mm(),
                        "y_mm": world.y.to_mm(),
                        "w_mm": pw.to_mm(),
                        "h_mm": ph.to_mm(),
                    })
                }).collect::<Vec<_>>(),
            })
        })
        .collect();
    let traces: Vec<Value> = board
        .traces
        .iter()
        .map(|t| json!({
            "id": t.id.0.to_string(),
            "net": t.net,
            "layer": match t.layer { pcb_core::CopperLayer::Top => "top", pcb_core::CopperLayer::Bottom => "bottom" },
            "x1_mm": t.start.x.to_mm(), "y1_mm": t.start.y.to_mm(),
            "x2_mm": t.end.x.to_mm(),   "y2_mm": t.end.y.to_mm(),
            "width_mm": t.width.to_mm(),
        }))
        .collect();
    let vias: Vec<Value> = board
        .vias
        .iter()
        .map(|v| json!({
            "id": v.id.0.to_string(),
            "net": v.net,
            "x_mm": v.position.x.to_mm(),
            "y_mm": v.position.y.to_mm(),
            "drill_mm": v.drill.to_mm(),
            "diameter_mm": v.diameter.to_mm(),
        }))
        .collect();

    Ok(text_result(svg).with_data(json!({
        "outline": outline,
        "footprints": footprints,
        "traces": traces,
        "vias": vias,
    })))
}

/// Compact "where are we" digest — outline, schematic counts, palette
/// items, footprint count, per-net connection status, DRC counts. No
/// SVG, no per-trace coords. Designed for the agent to call between
/// every action without burning tokens.
fn tool_view_summary(project: &Project) -> Result<Value, ToolError> {
    let snap = project.read();
    let board = snap.board();
    let sch = snap.schematic();

    let outline = board.outline.map(|r| {
        json!({
            "x_mm": r.min.x.to_mm(),
            "y_mm": r.min.y.to_mm(),
            "w_mm": r.width().to_mm(),
            "h_mm": r.height().to_mm(),
        })
    });

    let palette: Vec<Value> = snap
        .palette()
        .iter()
        .map(|fp| {
            let bounds = fp.bounds();
            let (bw, bh) = bounds
                .map(|r| (r.width().to_mm(), r.height().to_mm()))
                .unwrap_or((0.0, 0.0));
            json!({
                "reference": fp.reference,
                "key": fp.key,
                "edge_mounted": fp.edge_mounted,
                "bbox_w_mm": bw,
                "bbox_h_mm": bh,
            })
        })
        .collect();

    let footprints: Vec<Value> = board
        .footprints_in_order()
        .map(|fp| {
            json!({
                "reference": fp.reference,
                "key": fp.key,
                "x_mm": fp.position.x.to_mm(),
                "y_mm": fp.position.y.to_mm(),
                "rotation": fp.rotation,
            })
        })
        .collect();

    let nets = collect_net_status(board, sch);

    // DRC summary only — no per-violation list. Use drc.run for details.
    let drc = pcb_drc::run(board, &pcb_drc::DrcOptions::default());

    let total_nets = nets.len();
    let unconnected_nets: usize = nets
        .iter()
        .filter(|n| n["unconnected_pads"].as_array().map_or(false, |a| !a.is_empty()))
        .count();

    Ok(text_result(format!(
        "{} symbols, {} nets ({} fully connected), {} placed, {} in palette; DRC {}E {}W",
        sch.symbols.len(),
        total_nets,
        total_nets - unconnected_nets,
        board.footprints.len(),
        snap.palette().len(),
        drc.error_count,
        drc.warning_count,
    ))
    .with_data(json!({
        "outline": outline,
        "schematic": {
            "symbol_count": sch.symbols.len(),
            "net_count": sch.nets.len(),
        },
        "palette": palette,
        "footprints": footprints,
        "nets": nets,
        "drc": {
            "error_count": drc.error_count,
            "warning_count": drc.warning_count,
        },
    })))
}

/// Per-net "what's expected, what landed, what's missing". Built once
/// and reused by `view.summary` and the dedicated `net.status` tool.
fn collect_net_status(board: &pcb_core::Board, sch: &pcb_core::schematic::Schematic) -> Vec<Value> {
    use std::collections::HashSet;
    // First: which nets have any copper laid down? Pads that participate
    // in laid traces are counted as connected via that copper.
    let mut net_with_copper: HashSet<&str> = HashSet::new();
    for t in &board.traces {
        net_with_copper.insert(t.net.as_str());
    }
    for v in &board.vias {
        net_with_copper.insert(v.net.as_str());
    }
    // Map (footprint reference, pad number) → does this pad sit on
    // copper of its declared net? We approximate: a pad is "connected"
    // if its net has any copper at all AND the pad itself overlaps a
    // trace endpoint or via on the same net. The looser version (net
    // has any copper) is the cheaper signal we already report; the
    // tighter check exists in DRC's unconnected_pad warning, so we
    // mirror the DRC view by re-running the relevant geometry.
    let drc_report = pcb_drc::run(board, &pcb_drc::DrcOptions::default());
    let mut unconnected_pads: HashSet<(String, String)> = HashSet::new();
    for v in &drc_report.violations {
        if v.kind == pcb_drc::ViolationKind::UnconnectedPad {
            // involved is ["Ref.Pin"] for unconnected_pad
            if let Some(rp) = v.involved.first() {
                if let Some((r, p)) = rp.split_once('.') {
                    unconnected_pads.insert((r.to_string(), p.to_string()));
                }
            }
        }
    }

    let mut out = Vec::with_capacity(sch.nets.len());
    for (net_name, net) in &sch.nets {
        let mut pads_expected: Vec<Value> = Vec::with_capacity(net.connections.len());
        let mut connected_count = 0_usize;
        let mut unconnected = Vec::new();
        for conn in &net.connections {
            let Some(symbol) = sch.symbols.get(&conn.symbol_id) else { continue; };
            let pad_ref = format!("{}.{}", symbol.reference, conn.pin_number);
            let pin_name = symbol
                .kind
                .pins()
                .iter()
                .find(|p| p.number == conn.pin_number)
                .map(|p| p.name.clone())
                .unwrap_or_default();
            let is_unconnected = unconnected_pads
                .contains(&(symbol.reference.clone(), conn.pin_number.clone()));
            if !is_unconnected {
                connected_count += 1;
            } else {
                unconnected.push(pad_ref.clone());
            }
            pads_expected.push(json!({
                "ref": pad_ref,
                "pin_name": pin_name,
                "connected": !is_unconnected,
            }));
        }
        out.push(json!({
            "net": net_name,
            "pad_count": net.connections.len(),
            "connected_count": connected_count,
            "has_copper": net_with_copper.contains(net_name.as_str()),
            "pads": pads_expected,
            "unconnected_pads": unconnected,
        }));
    }
    out
}

fn tool_net_status(project: &Project) -> Result<Value, ToolError> {
    let snap = project.read();
    let nets = collect_net_status(snap.board(), snap.schematic());
    let unconnected: Vec<&str> = nets
        .iter()
        .filter(|n| n["unconnected_pads"].as_array().map_or(false, |a| !a.is_empty()))
        .filter_map(|n| n["net"].as_str())
        .collect();
    Ok(text_result(format!(
        "{} nets total, {} with unconnected pads ({})",
        nets.len(),
        unconnected.len(),
        if unconnected.is_empty() { "all clean".to_string() } else { unconnected.join(", ") },
    ))
    .with_data(json!({ "nets": nets })))
}

#[derive(Debug, Deserialize)]
struct SymbolInput {
    reference: String,
    #[serde(default)]
    value: String,
    kind: SymbolKindInput,
    #[serde(default)]
    pins: Vec<PinInput>,
    #[serde(default)]
    x_mm: Option<f64>,
    #[serde(default)]
    y_mm: Option<f64>,
    #[serde(default)]
    rotation: f32,
    /// Library key the agent picked (snake_case, e.g.
    /// "esp32_s3_zero"). Empty string means "no library entry".
    #[serde(default)]
    key: String,
    /// Free-form intent / role / orientation notes.
    #[serde(default)]
    description: String,
}

#[derive(Debug, Deserialize, Clone, Copy)]
#[serde(rename_all = "snake_case")]
enum SymbolKindInput {
    Resistor,
    Capacitor,
    Inductor,
    Led,
    Diode,
    GenericIc,
}

#[derive(Debug, Deserialize)]
struct PinInput {
    number: String,
    #[serde(default)]
    name: String,
    side: PinSideInput,
}

#[derive(Debug, Deserialize, Clone, Copy)]
#[serde(rename_all = "lowercase")]
enum PinSideInput {
    Left,
    Right,
    Top,
    Bottom,
}

impl From<PinSideInput> for PinSide {
    fn from(v: PinSideInput) -> Self {
        match v {
            PinSideInput::Left => Self::Left,
            PinSideInput::Right => Self::Right,
            PinSideInput::Top => Self::Top,
            PinSideInput::Bottom => Self::Bottom,
        }
    }
}

fn tool_schematic_add_symbol(project: &Project, args: &Value) -> Result<Value, ToolError> {
    let input: SymbolInput = serde_json::from_value(args.clone())
        .map_err(|e| ToolError::invalid_params(format!("schematic.add_symbol: {e}")))?;

    let kind = match input.kind {
        SymbolKindInput::Resistor => SymbolKind::Resistor,
        SymbolKindInput::Capacitor => SymbolKind::Capacitor,
        SymbolKindInput::Inductor => SymbolKind::Inductor,
        SymbolKindInput::Led => SymbolKind::Led,
        SymbolKindInput::Diode => SymbolKind::Diode,
        SymbolKindInput::GenericIc => {
            if input.pins.is_empty() {
                return Err(ToolError::invalid_params(
                    "schematic.add_symbol: kind=generic_ic requires a non-empty pins array",
                ));
            }
            let pins = input
                .pins
                .iter()
                .map(|p| SchPin {
                    number: p.number.clone(),
                    name: p.name.clone(),
                    side: p.side.into(),
                })
                .collect();
            SymbolKind::GenericIc { pins }
        }
    };

    let position = match (input.x_mm, input.y_mm) {
        (Some(x), Some(y)) => Point::new(Length::from_mm(x), Length::from_mm(y)),
        _ => auto_place(project),
    };

    let symbol = Symbol {
        id: pcb_core::Id::new(),
        reference: input.reference.clone(),
        value: input.value,
        kind,
        position,
        rotation: input.rotation,
        key: input.key.clone(),
        description: input.description.clone(),
    };
    let id = project.add_symbol(symbol);
    project.log(
        ActivityLevel::Info,
        format!(
            "schematic.add_symbol: {} at ({:.2}, {:.2}) mm",
            input.reference,
            position.x.to_mm(),
            position.y.to_mm()
        ),
    );
    Ok(text_result(format!("Added {} ({})", input.reference, id.0))
        .with_data(json!({ "id": id.0.to_string(), "reference": input.reference })))
}

/// Default placement: lay symbols out in rows of 6, 25 mm apart
/// horizontally and 20 mm vertically. The agent can always pass
/// explicit positions; this is just a "don't crash if you forget".
fn auto_place(project: &Project) -> Point {
    let snap = project.read();
    #[allow(clippy::cast_precision_loss)]
    let n = snap.schematic().symbol_order.len() as f64;
    let row = (n / 6.0).floor();
    let col = n - row * 6.0;
    Point::new(
        Length::from_mm(15.0 + col * 25.0),
        Length::from_mm(15.0 + row * 20.0),
    )
}

#[derive(Debug, Deserialize)]
struct ConnectInput {
    net: String,
    pins: Vec<String>,
}

fn tool_schematic_connect(project: &Project, args: &Value) -> Result<Value, ToolError> {
    let input: ConnectInput = serde_json::from_value(args.clone())
        .map_err(|e| ToolError::invalid_params(format!("schematic.connect: {e}")))?;

    let mut connections = Vec::with_capacity(input.pins.len());
    {
        let snap = project.read();
        let sch = snap.schematic();
        for pin_ref in &input.pins {
            let (sym_ref, pin_token) = pin_ref.split_once('.').ok_or_else(|| {
                ToolError::invalid_params(format!(
                    "expected REF.PIN format, got {pin_ref:?}"
                ))
            })?;
            let symbol = sch.find_by_reference(sym_ref).ok_or_else(|| {
                ToolError::invalid_params(format!("unknown symbol {sym_ref}"))
            })?;
            // Accept either the pin number (e.g. "U1.16") or the pin
            // name (e.g. "U1.GPIO13"). Names are matched
            // case-insensitively to be forgiving with how the agent
            // typed them. If neither matches a declared pin, fall back
            // to using the token verbatim as a pin number — discrete
            // primitives have implicit pins ("1"/"2") that aren't in
            // the SchPin list.
            let pin_number = symbol
                .kind
                .pins()
                .iter()
                .find(|p| {
                    p.number == pin_token
                        || p.name.eq_ignore_ascii_case(pin_token)
                })
                .map(|p| p.number.clone())
                .unwrap_or_else(|| pin_token.to_string());
            connections.push(NetConnection {
                symbol_id: symbol.id,
                pin_number,
            });
        }
    }
    let count = connections.len();
    project
        .set_net(Net {
            name: input.net.clone(),
            connections,
        })
        .map_err(ToolError::invalid_params)?;
    project.log(
        ActivityLevel::Info,
        format!("schematic.connect: {} ({} pin(s))", input.net, count),
    );
    Ok(text_result(format!(
        "Net {} now has {} connection(s)",
        input.net, count
    ))
    .with_data(json!({ "net": input.net, "connection_count": count })))
}

fn tool_schematic_status(project: &Project) -> Result<Value, ToolError> {
    let snap = project.read();
    let sch = snap.schematic();
    let symbol_count = sch.symbols.len();
    let net_count = sch.nets.len();
    let mut unconnected = Vec::new();
    for sym in sch.symbols_in_order() {
        for pin in sym.kind.pins() {
            if sch.net_for_pin(sym.id, &pin.number).is_none() {
                unconnected.push(format!("{}.{}", sym.reference, pin.number));
            }
        }
    }
    Ok(text_result(format!(
        "schematic: {symbol_count} symbol(s), {net_count} net(s), {} unconnected pin(s)",
        unconnected.len()
    ))
    .with_data(json!({
        "symbol_count": symbol_count,
        "net_count": net_count,
        "unconnected": unconnected,
    })))
}

fn tool_schematic_snapshot(project: &Project) -> Result<Value, ToolError> {
    let snap = project.read();
    let svg = pcb_render::render_schematic_svg(snap.schematic());
    Ok(text_result(svg).into())
}

#[derive(Debug, Deserialize)]
struct PaletteAddInput {
    footprints: Vec<PaletteFootprint>,
}

#[derive(Debug, Deserialize)]
struct PaletteFootprint {
    reference: String,
    library: String,
    #[serde(default)]
    rotation: f32,
    #[serde(default = "default_layer")]
    layer: LayerInput,
    pads: Vec<PadPlan>,
    /// Override edge_mounted from the schematic side. Useful when the
    /// agent decides at placement time that this instance must hug a
    /// specific edge (e.g. the on-module USB).
    #[serde(default)]
    edge_mounted: Option<bool>,
}

fn tool_palette_add(project: &Project, args: &Value) -> Result<Value, ToolError> {
    let input: PaletteAddInput = serde_json::from_value(args.clone())
        .map_err(|e| ToolError::invalid_params(format!("palette.add: {e}")))?;

    let mut added = Vec::with_capacity(input.footprints.len());
    for plan in input.footprints {
        // Pull the value + net assignments + key/description from the
        // schematic so the palette item carries the full intent.
        let (value, pads, key, description) = {
            let snap = project.read();
            let sch = snap.schematic();
            let symbol = sch.find_by_reference(&plan.reference).ok_or_else(|| {
                ToolError::invalid_params(format!(
                    "palette.add: no schematic symbol named {}",
                    plan.reference
                ))
            })?;
            let value = symbol.value.clone();
            let key = symbol.key.clone();
            let description = symbol.description.clone();
            let pads: Vec<Pad> = plan
                .pads
                .iter()
                .map(|pad_plan| {
                    let net = sch
                        .net_for_pin(symbol.id, &pad_plan.number)
                        .or_else(|| {
                            if pad_plan.name.is_empty() {
                                None
                            } else {
                                sch.net_for_pin(symbol.id, &pad_plan.name)
                            }
                        })
                        .map(str::to_string);
                    Pad {
                        number: pad_plan.number.clone(),
                        name: pad_plan.name.clone(),
                        offset: Point::new(
                            Length::from_mm(pad_plan.x_mm),
                            Length::from_mm(pad_plan.y_mm),
                        ),
                        size: (Length::from_mm(pad_plan.w_mm), Length::from_mm(pad_plan.h_mm)),
                        layer: pad_plan.layer.into(),
                        net,
                    }
                })
                .collect();
            (value, pads, key, description)
        };
        let footprint = Footprint {
            id: pcb_core::Id::new(),
            reference: plan.reference.clone(),
            value,
            library: plan.library,
            // Initial position will be overridden by the UI strip
            // (laid out left-to-right above the board) so any value is
            // fine; we put it off-canvas to avoid a flash of bad layout.
            position: Point::new(Length::from_mm(-100.0), Length::from_mm(-100.0)),
            rotation: plan.rotation,
            layer: plan.layer.into(),
            pads,
            key,
            description,
            edge_mounted: plan.edge_mounted.unwrap_or(false),
        };
        project
            .palette_add(footprint)
            .map_err(ToolError::invalid_params)?;
        added.push(plan.reference);
    }
    project.log(
        ActivityLevel::Info,
        format!("palette.add: {} component(s)", added.len()),
    );
    Ok(text_result(format!("Added {} item(s) to palette", added.len()))
        .with_data(json!({ "added": added })))
}

fn tool_palette_clear(project: &Project) -> Result<Value, ToolError> {
    project.palette_clear();
    project.log(ActivityLevel::Info, "palette.clear");
    Ok(text_result("Palette cleared").into())
}

fn tool_palette_list(project: &Project) -> Result<Value, ToolError> {
    let snap = project.read();
    let entries: Vec<Value> = snap
        .palette()
        .iter()
        .map(|fp| {
            let bounds = fp.bounds();
            let (bw, bh) = bounds
                .map(|r| (r.width().to_mm(), r.height().to_mm()))
                .unwrap_or((0.0, 0.0));
            let mut nets: Vec<&str> = fp
                .pads
                .iter()
                .filter_map(|p| p.net.as_deref())
                .collect();
            nets.sort();
            nets.dedup();
            json!({
                "reference": fp.reference,
                "value": fp.value,
                "library": fp.library,
                "rotation": fp.rotation,
                "bbox_w_mm": bw,
                "bbox_h_mm": bh,
                "pad_count": fp.pads.len(),
                "nets": nets,
            })
        })
        .collect();
    Ok(text_result(format!(
        "{} item(s) waiting in the palette",
        entries.len()
    ))
    .with_data(json!({ "items": entries })))
}

#[derive(Debug, Deserialize)]
struct PlaceFromPaletteInput {
    reference: String,
    x_mm: f64,
    y_mm: f64,
}

// ─── Library tools ─────────────────────────────────────────────────────

fn library_entry_summary(e: &pcb_core::LibraryEntry) -> Value {
    json!({
        "key": e.key,
        "description": e.description,
        "default_value": e.default_value,
        "default_rotation_deg": e.default_rotation_deg,
        "edge_mounted": e.edge_mounted,
        "pad_count": e.pads.len(),
        "attachment_count": e.attachments.len(),
        "attachments": e.attachments.iter().map(|a| json!({
            "id": a.id,
            "kind": a.kind,
            "filename": a.filename,
            "mime": a.mime,
            "added_at": a.added_at,
        })).collect::<Vec<_>>(),
        "created_at": e.created_at,
    })
}

fn library_entry_full(e: &pcb_core::LibraryEntry) -> Value {
    let mut v = library_entry_summary(e);
    let pads: Vec<Value> = e.pads.iter().map(|p| json!({
        "number": p.number,
        "name": p.name,
        "x_mm": p.x_mm,
        "y_mm": p.y_mm,
        "w_mm": p.w_mm,
        "h_mm": p.h_mm,
    })).collect();
    if let Some(obj) = v.as_object_mut() {
        obj.insert("pads".into(), Value::Array(pads));
    }
    v
}

fn tool_library_list(project: &Project) -> Result<Value, ToolError> {
    let entries = project.library().list();
    let items: Vec<Value> = entries.iter().map(library_entry_summary).collect();
    Ok(text_result(format!("{} entries in library", items.len()))
        .with_data(json!({ "entries": items })))
}

#[derive(Debug, Deserialize)]
struct LibraryFindInput { key: String }

fn tool_library_find(project: &Project, args: &Value) -> Result<Value, ToolError> {
    let input: LibraryFindInput = serde_json::from_value(args.clone())
        .map_err(|e| ToolError::invalid_params(format!("library.find: {e}")))?;
    match project.library().find(&input.key) {
        Some(e) => Ok(text_result(format!("Found {}", e.key)).with_data(library_entry_full(&e))),
        None => Err(ToolError::invalid_params(format!(
            "library.find: no entry with key {}",
            input.key
        ))),
    }
}

#[derive(Debug, Deserialize)]
struct LibraryCreatePadInput {
    number: String,
    #[serde(default)]
    name: String,
    x_mm: f64,
    y_mm: f64,
    w_mm: f64,
    h_mm: f64,
}

#[derive(Debug, Deserialize)]
struct LibraryCreateInput {
    key: String,
    description: String,
    #[serde(default)]
    default_value: String,
    #[serde(default)]
    default_rotation_deg: f32,
    #[serde(default)]
    edge_mounted: bool,
    pads: Vec<LibraryCreatePadInput>,
}

fn tool_library_create(project: &Project, args: &Value) -> Result<Value, ToolError> {
    let input: LibraryCreateInput = serde_json::from_value(args.clone())
        .map_err(|e| ToolError::invalid_params(format!("library.create: {e}")))?;
    let pads = input.pads.into_iter().map(|p| pcb_core::LibraryPad {
        number: p.number,
        name: p.name,
        x_mm: p.x_mm,
        y_mm: p.y_mm,
        w_mm: p.w_mm,
        h_mm: p.h_mm,
    }).collect();
    let entry = pcb_core::LibraryEntry {
        key: input.key.clone(),
        description: input.description,
        default_value: input.default_value,
        default_rotation_deg: input.default_rotation_deg,
        edge_mounted: input.edge_mounted,
        pads,
        attachments: Vec::new(),
        created_at: 0,
    };
    let stored = project
        .library()
        .upsert(entry)
        .map_err(ToolError::invalid_params)?;
    let count = project.library().list().len();
    project.events().publish(pcb_core::Event::LibraryChanged { count });
    project.log(
        ActivityLevel::Info,
        format!("library.create: {} ({} pads)", stored.key, stored.pads.len()),
    );
    Ok(text_result(format!("Saved {}", stored.key)).with_data(library_entry_full(&stored)))
}

#[derive(Debug, Deserialize)]
struct LibraryAttachInput {
    key: String,
    kind: String,
    filename: String,
    mime: String,
    data_base64: String,
}

fn tool_library_attach(project: &Project, args: &Value) -> Result<Value, ToolError> {
    use base64::Engine;
    let input: LibraryAttachInput = serde_json::from_value(args.clone())
        .map_err(|e| ToolError::invalid_params(format!("library.attach: {e}")))?;
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(input.data_base64.as_bytes())
        .map_err(|e| ToolError::invalid_params(format!("library.attach: invalid base64: {e}")))?;
    let att = project
        .library()
        .attach(&input.key, input.kind, input.filename, input.mime, &bytes)
        .map_err(ToolError::invalid_params)?;
    let count = project.library().list().len();
    project.events().publish(pcb_core::Event::LibraryChanged { count });
    project.log(
        ActivityLevel::Info,
        format!("library.attach: {} ← {} ({} bytes)", input.key, att.filename, bytes.len()),
    );
    Ok(text_result(format!("Attached {}", att.filename)).with_data(json!({
        "id": att.id,
        "kind": att.kind,
        "filename": att.filename,
        "mime": att.mime,
        "added_at": att.added_at,
    })))
}

#[derive(Debug, Deserialize)]
struct LibraryDeleteAttachmentInput { key: String, attachment_id: String }

fn tool_library_delete_attachment(project: &Project, args: &Value) -> Result<Value, ToolError> {
    let input: LibraryDeleteAttachmentInput = serde_json::from_value(args.clone())
        .map_err(|e| ToolError::invalid_params(format!("library.delete_attachment: {e}")))?;
    let removed = project
        .library()
        .delete_attachment(&input.key, &input.attachment_id)
        .map_err(ToolError::invalid_params)?;
    if removed {
        let count = project.library().list().len();
        project.events().publish(pcb_core::Event::LibraryChanged { count });
    }
    Ok(text_result(if removed { "Attachment removed" } else { "No matching attachment" }.to_string())
        .with_data(json!({ "removed": removed })))
}

#[derive(Debug, Deserialize)]
struct LibraryDeleteInput { key: String }

fn tool_library_delete(project: &Project, args: &Value) -> Result<Value, ToolError> {
    let input: LibraryDeleteInput = serde_json::from_value(args.clone())
        .map_err(|e| ToolError::invalid_params(format!("library.delete: {e}")))?;
    let removed = project
        .library()
        .delete(&input.key)
        .map_err(ToolError::invalid_params)?;
    if removed {
        let count = project.library().list().len();
        project.events().publish(pcb_core::Event::LibraryChanged { count });
        project.log(ActivityLevel::Info, format!("library.delete: {}", input.key));
    }
    Ok(text_result(if removed { "Entry removed" } else { "No matching entry" }.to_string())
        .with_data(json!({ "removed": removed })))
}

#[derive(Debug, Deserialize)]
struct PaletteAddFromLibraryInput {
    reference: String,
    key: String,
    #[serde(default)]
    value: Option<String>,
    #[serde(default)]
    rotation: Option<f32>,
    #[serde(default = "default_layer")]
    layer: LayerInput,
}

fn tool_palette_add_from_library(project: &Project, args: &Value) -> Result<Value, ToolError> {
    let input: PaletteAddFromLibraryInput = serde_json::from_value(args.clone())
        .map_err(|e| ToolError::invalid_params(format!("palette.add_from_library: {e}")))?;
    let entry = project
        .library()
        .find(&input.key)
        .ok_or_else(|| ToolError::invalid_params(format!(
            "palette.add_from_library: no library entry with key {}",
            input.key
        )))?;

    // Pull value/key/description/edge from the schematic symbol if it
    // exists, falling back to the library entry's defaults. The
    // schematic also carries the per-pad net assignment.
    let (resolved_value, key_field, description_field, pads, edge_from_schematic) = {
        let snap = project.read();
        let sch = snap.schematic();
        let symbol = sch.find_by_reference(&input.reference).ok_or_else(|| {
            ToolError::invalid_params(format!(
                "palette.add_from_library: no schematic symbol named {}",
                input.reference
            ))
        })?;
        let value = input.value
            .clone()
            .filter(|s| !s.is_empty())
            .or_else(|| (!symbol.value.is_empty()).then(|| symbol.value.clone()))
            .unwrap_or_else(|| entry.default_value.clone());
        let key_field = if symbol.key.is_empty() { input.key.clone() } else { symbol.key.clone() };
        let description_field = if symbol.description.is_empty() {
            entry.description.clone()
        } else {
            symbol.description.clone()
        };
        let pads: Vec<Pad> = entry.pads.iter().map(|p| {
            // Library pads are numbered ("1", "2", ...) but the
            // schematic side may use names ("A"/"K" for LEDs, "VBAT"
            // for power pins). Look up by number first, then by the
            // pad's name so net wiring survives across either
            // convention.
            let net = sch
                .net_for_pin(symbol.id, &p.number)
                .or_else(|| {
                    if p.name.is_empty() {
                        None
                    } else {
                        sch.net_for_pin(symbol.id, &p.name)
                    }
                })
                .map(str::to_string);
            Pad {
                number: p.number.clone(),
                name: p.name.clone(),
                offset: Point::new(Length::from_mm(p.x_mm), Length::from_mm(p.y_mm)),
                size: (Length::from_mm(p.w_mm), Length::from_mm(p.h_mm)),
                layer: input.layer.into(),
                net,
            }
        }).collect();
        // edge_mounted: schematic doesn't have this yet; just inherit
        // from library.
        (value, key_field, description_field, pads, entry.edge_mounted)
    };

    let footprint = Footprint {
        id: pcb_core::Id::new(),
        reference: input.reference.clone(),
        value: resolved_value,
        library: format!("library:{}", input.key),
        position: Point::new(Length::from_mm(-100.0), Length::from_mm(-100.0)),
        rotation: input.rotation.unwrap_or(entry.default_rotation_deg),
        layer: input.layer.into(),
        pads,
        key: key_field,
        description: description_field,
        edge_mounted: edge_from_schematic,
    };
    project.palette_add(footprint).map_err(ToolError::invalid_params)?;
    project.log(
        ActivityLevel::Info,
        format!("palette.add_from_library: {} ← {}", input.reference, input.key),
    );
    Ok(text_result(format!("Spawned {} from {}", input.reference, input.key))
        .with_data(json!({ "reference": input.reference, "key": input.key })))
}

fn tool_place_from_palette(project: &Project, args: &Value) -> Result<Value, ToolError> {
    let input: PlaceFromPaletteInput = serde_json::from_value(args.clone())
        .map_err(|e| ToolError::invalid_params(format!("placement.place_from_palette: {e}")))?;
    let id = project
        .place_from_palette(
            &input.reference,
            Point::new(Length::from_mm(input.x_mm), Length::from_mm(input.y_mm)),
        )
        .map_err(ToolError::invalid_params)?;
    project.log(
        ActivityLevel::Info,
        format!(
            "placement.place_from_palette: {} at ({:.2}, {:.2}) mm",
            input.reference, input.x_mm, input.y_mm
        ),
    );
    Ok(text_result(format!("Placed {}", input.reference))
        .with_data(json!({"id": id.0.to_string()})))
}

#[derive(Debug, Deserialize)]
struct PlacementBatchItem {
    reference: String,
    x_mm: f64,
    y_mm: f64,
    #[serde(default)]
    rotation: Option<f32>,
}

#[derive(Debug, Deserialize)]
struct PlacementBatchInput {
    items: Vec<PlacementBatchItem>,
}

fn tool_placement_batch(project: &Project, args: &Value) -> Result<Value, ToolError> {
    let input: PlacementBatchInput = serde_json::from_value(args.clone())
        .map_err(|e| ToolError::invalid_params(format!("placement.batch: {e}")))?;
    let mut results = Vec::with_capacity(input.items.len());
    let mut ok_count = 0_usize;
    let mut fail_count = 0_usize;
    for item in input.items {
        let pos = Point::new(Length::from_mm(item.x_mm), Length::from_mm(item.y_mm));
        let placed = project.place_from_palette(&item.reference, pos);
        match placed {
            Ok(id) => {
                // Apply rotation after placement so it shares the same
                // overlap-vs-edge gates as a manual call would.
                if let Some(deg) = item.rotation {
                    if let Err(rot_err) = project.rotate_footprint(&item.reference, deg) {
                        fail_count += 1;
                        results.push(json!({
                            "reference": item.reference,
                            "ok": false,
                            "stage": "rotate",
                            "error": rot_err,
                            "id": id.0.to_string(),
                        }));
                        continue;
                    }
                }
                ok_count += 1;
                results.push(json!({
                    "reference": item.reference,
                    "ok": true,
                    "id": id.0.to_string(),
                }));
            }
            Err(e) => {
                fail_count += 1;
                results.push(json!({
                    "reference": item.reference,
                    "ok": false,
                    "stage": "place",
                    "error": e,
                }));
            }
        }
    }
    project.log(
        ActivityLevel::Info,
        format!("placement.batch: {ok_count} placed, {fail_count} failed"),
    );
    Ok(text_result(format!(
        "{ok_count} placed, {fail_count} failed"
    ))
    .with_data(json!({
        "ok_count": ok_count,
        "fail_count": fail_count,
        "results": results,
    })))
}

#[derive(Debug, Deserialize)]
struct PlacementMoveInput {
    reference: String,
    x_mm: f64,
    y_mm: f64,
}

fn tool_placement_move(project: &Project, args: &Value) -> Result<Value, ToolError> {
    let input: PlacementMoveInput = serde_json::from_value(args.clone())
        .map_err(|e| ToolError::invalid_params(format!("placement.move: {e}")))?;
    project
        .move_footprint_to(
            &input.reference,
            Point::new(Length::from_mm(input.x_mm), Length::from_mm(input.y_mm)),
        )
        .map_err(ToolError::invalid_params)?;
    Ok(text_result(format!(
        "Moved {} to ({:.2}, {:.2}) mm",
        input.reference, input.x_mm, input.y_mm
    ))
    .into())
}

#[derive(Debug, Deserialize)]
struct PlacementRotateInput {
    reference: String,
    degrees: f32,
}

fn tool_placement_rotate(project: &Project, args: &Value) -> Result<Value, ToolError> {
    let input: PlacementRotateInput = serde_json::from_value(args.clone())
        .map_err(|e| ToolError::invalid_params(format!("placement.rotate: {e}")))?;
    let normalised = input.degrees.rem_euclid(360.0);
    project
        .rotate_footprint(&input.reference, normalised)
        .map_err(ToolError::invalid_params)?;
    project.log(
        ActivityLevel::Info,
        format!("placement.rotate: {} → {normalised:.0}°", input.reference),
    );
    Ok(text_result(format!(
        "Rotated {} to {normalised:.0}°",
        input.reference
    ))
    .into())
}

#[derive(Debug, Deserialize)]
struct ClearNetInput { net: String }

fn tool_route_clear_net(project: &Project, args: &Value) -> Result<Value, ToolError> {
    let input: ClearNetInput = serde_json::from_value(args.clone())
        .map_err(|e| ToolError::invalid_params(format!("route.clear_net: {e}")))?;
    let removed = project.clear_net_routing(&input.net);
    project.log(
        ActivityLevel::Info,
        format!("route.clear_net: {} ({} item(s))", input.net, removed),
    );
    Ok(text_result(format!("Cleared {removed} item(s) from net {}", input.net))
        .with_data(json!({"removed": removed})))
}

#[derive(Debug, Deserialize)]
struct DeleteByIdInput { id: String }

fn parse_id(s: &str) -> Result<pcb_core::Id, ToolError> {
    pcb_core::Id::parse(s)
        .map_err(|e| ToolError::invalid_params(format!("invalid id {s}: {e}")))
}

fn tool_route_delete_trace(project: &Project, args: &Value) -> Result<Value, ToolError> {
    let input: DeleteByIdInput = serde_json::from_value(args.clone())
        .map_err(|e| ToolError::invalid_params(format!("route.delete_trace: {e}")))?;
    let id = parse_id(&input.id)?;
    let ok = project.delete_trace(id);
    Ok(text_result(if ok { "Trace removed" } else { "Trace not found" }.to_string())
        .with_data(json!({"removed": ok})))
}

fn tool_route_delete_via(project: &Project, args: &Value) -> Result<Value, ToolError> {
    let input: DeleteByIdInput = serde_json::from_value(args.clone())
        .map_err(|e| ToolError::invalid_params(format!("route.delete_via: {e}")))?;
    let id = parse_id(&input.id)?;
    let ok = project.delete_via(id);
    Ok(text_result(if ok { "Via removed" } else { "Via not found" }.to_string())
        .with_data(json!({"removed": ok})))
}


#[derive(Debug, Deserialize)]
struct PadPlan {
    number: String,
    #[serde(default)]
    name: String,
    x_mm: f64,
    y_mm: f64,
    w_mm: f64,
    h_mm: f64,
    #[serde(default = "default_layer")]
    layer: LayerInput,
}

#[derive(Debug, Deserialize)]
struct AddTraceInput {
    net: String,
    layer: LayerInput,
    x1_mm: f64,
    y1_mm: f64,
    x2_mm: f64,
    y2_mm: f64,
    width_mm: f64,
}

fn tool_route_add_trace(project: &Project, args: &Value) -> Result<Value, ToolError> {
    let input: AddTraceInput = serde_json::from_value(args.clone())
        .map_err(|e| ToolError::invalid_params(format!("route.add_trace: {e}")))?;
    let id = project.add_trace(Trace {
        id: pcb_core::Id::new(),
        layer: input.layer.into(),
        start: Point::new(Length::from_mm(input.x1_mm), Length::from_mm(input.y1_mm)),
        end: Point::new(Length::from_mm(input.x2_mm), Length::from_mm(input.y2_mm)),
        width: Length::from_mm(input.width_mm),
        net: input.net.clone(),
    });
    Ok(text_result(format!(
        "trace {} on {:?} ({})",
        id.0, input.layer, input.net
    ))
    .with_data(json!({"id": id.0.to_string()})))
}

#[derive(Debug, Deserialize)]
struct AddViaInput {
    net: String,
    x_mm: f64,
    y_mm: f64,
    drill_mm: f64,
    diameter_mm: f64,
}

fn tool_route_add_via(project: &Project, args: &Value) -> Result<Value, ToolError> {
    let input: AddViaInput = serde_json::from_value(args.clone())
        .map_err(|e| ToolError::invalid_params(format!("route.add_via: {e}")))?;
    let id = project.add_via(Via {
        id: pcb_core::Id::new(),
        position: Point::new(Length::from_mm(input.x_mm), Length::from_mm(input.y_mm)),
        drill: Length::from_mm(input.drill_mm),
        diameter: Length::from_mm(input.diameter_mm),
        net: input.net.clone(),
    });
    Ok(text_result(format!("via {} ({})", id.0, input.net))
        .with_data(json!({"id": id.0.to_string()})))
}

fn tool_route_clear(project: &Project) -> Result<Value, ToolError> {
    project.clear_routing();
    project.log(ActivityLevel::Info, "route.clear");
    Ok(text_result("Cleared all traces and vias").into())
}

#[derive(Debug, Deserialize)]
struct PourInput {
    net: String,
    layer: LayerInput,
}

fn tool_pour_add(project: &Project, args: &Value) -> Result<Value, ToolError> {
    let input: PourInput = serde_json::from_value(args.clone())
        .map_err(|e| ToolError::invalid_params(format!("pour.add: {e}")))?;
    let layer: CopperLayer = input.layer.into();
    project.add_pour(Pour {
        net: input.net.clone(),
        layer,
    });
    project.log(
        ActivityLevel::Info,
        format!("pour.add {} on {:?}", input.net, layer),
    );
    Ok(text_result(format!(
        "Pour added: net={} layer={:?}",
        input.net, layer
    ))
    .with_data(json!({"net": input.net, "layer": layer_to_str(layer)})))
}

fn tool_pour_remove(project: &Project, args: &Value) -> Result<Value, ToolError> {
    let input: PourInput = serde_json::from_value(args.clone())
        .map_err(|e| ToolError::invalid_params(format!("pour.remove: {e}")))?;
    let layer: CopperLayer = input.layer.into();
    let removed = project.remove_pour(&input.net, layer);
    Ok(text_result(if removed {
        format!("Pour removed: net={} layer={:?}", input.net, layer)
    } else {
        format!("No pour for net={} layer={:?}", input.net, layer)
    })
    .with_data(json!({"removed": removed})))
}

fn layer_to_str(layer: CopperLayer) -> &'static str {
    match layer {
        CopperLayer::Top => "top",
        CopperLayer::Bottom => "bottom",
    }
}

#[derive(Debug, Deserialize)]
struct RouteRunInput {
    #[serde(default = "default_cell")]
    cell_mm: f64,
    #[serde(default = "default_trace_w")]
    trace_width_mm: f64,
    #[serde(default = "default_clearance")]
    clearance_mm: f64,
    #[serde(default = "default_via_cost")]
    via_cost: u32,
    #[serde(default = "default_via_drill")]
    via_drill_mm: f64,
    #[serde(default = "default_via_diameter")]
    via_diameter_mm: f64,
}

fn default_cell() -> f64 { 0.25 }
fn default_trace_w() -> f64 { 0.25 }
fn default_clearance() -> f64 { 0.20 }
fn default_via_cost() -> u32 { 8 }
fn default_via_drill() -> f64 { 0.30 }
fn default_via_diameter() -> f64 { 0.60 }

fn tool_route_run(project: &Project, args: &Value) -> Result<Value, ToolError> {
    let input: RouteRunInput = serde_json::from_value(args.clone())
        .map_err(|e| ToolError::invalid_params(format!("route.run: {e}")))?;

    let opts = pcb_router::RouteOptions {
        cell: Length::from_mm(input.cell_mm),
        trace_width: Length::from_mm(input.trace_width_mm),
        clearance: Length::from_mm(input.clearance_mm),
        via_cost: input.via_cost,
        via_drill: Length::from_mm(input.via_drill_mm),
        via_diameter: Length::from_mm(input.via_diameter_mm),
    };

    // Route on a clone so the lock is released quickly; then push the
    // result back into the live Project via the regular APIs (which
    // emit RoutingChanged events for the UI).
    let mut work = project.read().board().clone();
    let report = pcb_router::route(&mut work, &opts);

    project.clear_routing();
    for trace in &work.traces {
        project.add_trace(trace.clone());
    }
    for via in &work.vias {
        project.add_via(via.clone());
    }

    let per_net: Vec<Value> = report
        .per_net
        .iter()
        .map(|(name, outcome)| match outcome {
            pcb_router::Outcome::Ok { trace_segments, vias } => json!({
                "net": name, "ok": true,
                "trace_segments": trace_segments, "vias": vias,
            }),
            pcb_router::Outcome::Failed { reason } => json!({
                "net": name, "ok": false, "reason": reason,
            }),
        })
        .collect();
    let failed: Vec<&str> = report
        .per_net
        .iter()
        .filter_map(|(n, o)| matches!(o, pcb_router::Outcome::Failed { .. }).then_some(n.as_str()))
        .collect();

    // Run DRC right after the route so the agent gets the verdict
    // in a single round-trip and can iterate without a second call.
    let drc_report = {
        let snap = project.read();
        pcb_drc::run(snap.board(), &pcb_drc::DrcOptions::default())
    };
    project.log(
        ActivityLevel::Info,
        format!(
            "route.run: {} traces, {} vias, {} net(s) failed; DRC {}E {}W",
            report.trace_count,
            report.via_count,
            failed.len(),
            drc_report.error_count,
            drc_report.warning_count,
        ),
    );
    Ok(text_result(format!(
        "Routed: {} traces, {} vias{}; DRC: {} error(s), {} warning(s)",
        report.trace_count,
        report.via_count,
        if failed.is_empty() {
            String::new()
        } else {
            format!(" ({} failed: {})", failed.len(), failed.join(", "))
        },
        drc_report.error_count,
        drc_report.warning_count,
    ))
    .with_data(json!({
        "trace_count": report.trace_count,
        "via_count": report.via_count,
        "per_net": per_net,
        "drc": serde_json::to_value(&drc_report).unwrap_or(json!({})),
    })))
}

#[derive(Debug, Deserialize)]
struct DrcInput {
    #[serde(default)]
    min_clearance_mm: Option<f64>,
    #[serde(default)]
    edge_clearance_mm: Option<f64>,
    #[serde(default)]
    min_trace_width_mm: Option<f64>,
    #[serde(default)]
    min_drill_mm: Option<f64>,
}

fn tool_drc_run(project: &Project, args: &Value) -> Result<Value, ToolError> {
    let input: DrcInput = serde_json::from_value(args.clone())
        .map_err(|e| ToolError::invalid_params(format!("drc.run: {e}")))?;

    let mut opts = pcb_drc::DrcOptions::default();
    if let Some(v) = input.min_clearance_mm { opts.min_clearance = Length::from_mm(v); }
    if let Some(v) = input.edge_clearance_mm { opts.edge_clearance = Length::from_mm(v); }
    if let Some(v) = input.min_trace_width_mm { opts.min_trace_width = Length::from_mm(v); }
    if let Some(v) = input.min_drill_mm { opts.min_drill = Length::from_mm(v); }

    let snap = project.read();
    let report = pcb_drc::run(snap.board(), &opts);
    drop(snap);

    project.log(
        ActivityLevel::Info,
        format!(
            "drc.run: {} error(s), {} warning(s)",
            report.error_count, report.warning_count
        ),
    );
    let summary = format!(
        "DRC: {} error(s), {} warning(s)",
        report.error_count, report.warning_count
    );
    Ok(text_result(summary).with_data(serde_json::to_value(&report).unwrap_or(json!({}))))
}

#[derive(Debug, Deserialize)]
struct FabPackInput {
    out_dir: String,
    #[serde(default)]
    name: Option<String>,
}

fn tool_output_fab_pack(project: &Project, args: &Value) -> Result<Value, ToolError> {
    let input: FabPackInput = serde_json::from_value(args.clone())
        .map_err(|e| ToolError::invalid_params(format!("output.fab_pack: {e}")))?;

    let snap = project.read();
    let stem = input
        .name
        .unwrap_or_else(|| snap.name().to_string());
    let out_dir = std::path::PathBuf::from(&input.out_dir);

    let paths = pcb_gerber::write_fab_pack(snap.board(), &stem, &out_dir).map_err(|e| ToolError {
        code: crate::protocol::error_code::INTERNAL_ERROR,
        message: format!("write_fab_pack: {e}"),
    })?;

    let path_strings: Vec<String> = paths
        .iter()
        .map(|p| p.display().to_string())
        .collect();
    project.log(
        ActivityLevel::Info,
        format!(
            "output.fab_pack: wrote {} files to {}",
            path_strings.len(),
            out_dir.display()
        ),
    );

    Ok(text_result(format!(
        "Wrote {} files:\n{}",
        path_strings.len(),
        path_strings.join("\n")
    ))
    .with_data(json!({
        "out_dir": out_dir.display().to_string(),
        "files": path_strings,
    })))
}

/// Builds the MCP tool result envelope. The text content is what the
/// agent sees; `with_data` attaches structured metadata for the UI bridge.
struct ToolResult {
    text: String,
    data: Option<Value>,
}

fn text_result(text: impl Into<String>) -> ToolResult {
    ToolResult {
        text: text.into(),
        data: None,
    }
}

impl ToolResult {
    fn with_data(mut self, data: Value) -> Value {
        self.data = Some(data);
        self.into_value()
    }

    fn into_value(self) -> Value {
        let mut obj = json!({
            "content": [{ "type": "text", "text": self.text }],
        });
        if let Some(data) = self.data {
            obj.as_object_mut()
                .expect("ToolResult shape")
                .insert("structuredContent".into(), data);
        }
        obj
    }
}

impl From<ToolResult> for Value {
    fn from(value: ToolResult) -> Self {
        value.into_value()
    }
}
