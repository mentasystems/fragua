//! Tool surface — what the MCP client (the AI agent) can call.
//!
//! Each tool is intentionally thin: parse the input, call into
//! `pcb-core` to mutate the project, return the result. The agent owns
//! all the design reasoning; tools are pure data primitives.

use pcb_core::schematic::{Net, NetConnection, PinSide, SchPin, Symbol, SymbolKind};
use pcb_core::{ActivityLevel, CopperLayer, Footprint, Length, Pad, Point, Project, Trace, Via};
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

/// Static catalog returned by `tools/list`.
#[must_use]
pub fn catalog() -> Value {
    json!([
        {
            "name": "project.status",
            "description": "Returns a summary of the current project: name, footprint count, content bounding box.",
            "inputSchema": {
                "type": "object",
                "properties": {},
                "additionalProperties": false
            }
        },
        {
            "name": "placement.add",
            "description": "Add a footprint to the board at a given (x, y) position in millimetres. The footprint is described by its reference designator, value, library id, and a list of pads with sizes and copper layer assignment.",
            "inputSchema": {
                "type": "object",
                "required": ["reference", "library", "x_mm", "y_mm", "pads"],
                "properties": {
                    "reference":  { "type": "string", "description": "e.g. R1, U3" },
                    "value":      { "type": "string", "description": "component value, e.g. 10k" },
                    "library":    { "type": "string", "description": "library id, e.g. Resistor_SMD:R_0805" },
                    "x_mm":       { "type": "number" },
                    "y_mm":       { "type": "number" },
                    "rotation":   { "type": "number", "description": "degrees CCW", "default": 0 },
                    "layer":      { "type": "string", "enum": ["top", "bottom"], "default": "top" },
                    "pads": {
                        "type": "array",
                        "items": {
                            "type": "object",
                            "required": ["number", "x_mm", "y_mm", "w_mm", "h_mm"],
                            "properties": {
                                "number": { "type": "string" },
                                "x_mm":   { "type": "number" },
                                "y_mm":   { "type": "number" },
                                "w_mm":   { "type": "number" },
                                "h_mm":   { "type": "number" },
                                "layer":  { "type": "string", "enum": ["top", "bottom"], "default": "top" },
                                "net":    { "type": "string" }
                            }
                        }
                    }
                },
                "additionalProperties": false
            }
        },
        {
            "name": "view.snapshot",
            "description": "Returns the current board rendered as SVG. Useful for the agent to attach a visual snapshot of the work in progress.",
            "inputSchema": {
                "type": "object",
                "properties": {},
                "additionalProperties": false
            }
        },
        {
            "name": "board.set_outline",
            "description": "Set the rectangular outline (Edge.Cuts) of the board, in millimetres. Required before placement.auto so the placer has a workspace and the gerbers have a cut shape. Origin is (0,0) at the top-left corner of the rectangle.",
            "inputSchema": {
                "type": "object",
                "required": ["w_mm","h_mm"],
                "properties": {
                    "w_mm": { "type": "number", "minimum": 1 },
                    "h_mm": { "type": "number", "minimum": 1 }
                },
                "additionalProperties": false
            }
        },
        {
            "name": "project.reset",
            "description": "Drop every symbol, net, footprint, trace, and via. Returns the project to an empty state — useful between demos or when starting a new design from scratch.",
            "inputSchema": {
                "type": "object",
                "properties": {},
                "additionalProperties": false
            }
        },
        {
            "name": "schematic.add_symbol",
            "description": "Add a symbol to the schematic. For discrete primitives (resistor, capacitor, inductor, led, diode), pins are implicit. For generic ICs, declare every pin with its number, optional name, and side (left/right/top/bottom). Position is in millimetres on the schematic page; if omitted the symbol is placed on the next free 5×3 grid slot.",
            "inputSchema": {
                "type": "object",
                "required": ["reference", "kind"],
                "properties": {
                    "reference": { "type": "string" },
                    "value":     { "type": "string" },
                    "kind":      {
                        "type": "string",
                        "enum": ["resistor", "capacitor", "inductor", "led", "diode", "generic_ic"]
                    },
                    "pins": {
                        "type": "array",
                        "description": "required when kind=generic_ic; ignored otherwise",
                        "items": {
                            "type": "object",
                            "required": ["number", "side"],
                            "properties": {
                                "number": { "type": "string" },
                                "name":   { "type": "string" },
                                "side":   { "type": "string", "enum": ["left","right","top","bottom"] }
                            }
                        }
                    },
                    "x_mm":      { "type": "number" },
                    "y_mm":      { "type": "number" },
                    "rotation":  { "type": "number" }
                },
                "additionalProperties": false
            }
        },
        {
            "name": "schematic.connect",
            "description": "Set or replace the connections of a named net. The pin reference uses 'REF.PIN' notation (e.g. 'R1.1', 'U1.VBAT'). Replacing on every call makes the tool idempotent — the agent can re-state the full net without accumulating duplicates.",
            "inputSchema": {
                "type": "object",
                "required": ["net", "pins"],
                "properties": {
                    "net":  { "type": "string", "description": "net name, e.g. VCC, GND, SDA" },
                    "pins": {
                        "type": "array",
                        "items": { "type": "string", "description": "REF.PIN, e.g. R1.1" },
                        "minItems": 1
                    }
                },
                "additionalProperties": false
            }
        },
        {
            "name": "schematic.status",
            "description": "Returns counts of symbols and nets, and the list of unconnected pins (potential design errors).",
            "inputSchema": {
                "type": "object",
                "properties": {},
                "additionalProperties": false
            }
        },
        {
            "name": "schematic.snapshot",
            "description": "Returns the current schematic rendered as SVG.",
            "inputSchema": {
                "type": "object",
                "properties": {},
                "additionalProperties": false
            }
        },
        {
            "name": "palette.add",
            "description": "Register footprints in the project's palette — the strip of components shown above the board canvas. Items in the palette are not yet on the board; the human drags them in (locking them where dropped) or the agent calls placement.auto to settle the remaining ones automatically. Pads are tagged with their schematic net so once they hit the board the routing is ready to run.",
            "inputSchema": {
                "type": "object",
                "required": ["footprints"],
                "properties": {
                    "footprints": {
                        "type": "array",
                        "items": {
                            "type": "object",
                            "required": ["reference","library","pads"],
                            "properties": {
                                "reference": { "type": "string" },
                                "library":   { "type": "string" },
                                "rotation":  { "type": "number", "default": 0 },
                                "layer":     { "type": "string", "enum": ["top","bottom"], "default": "top" },
                                "pads": {
                                    "type": "array",
                                    "items": {
                                        "type": "object",
                                        "required": ["number","x_mm","y_mm","w_mm","h_mm"],
                                        "properties": {
                                            "number": {"type":"string"},
                                            "x_mm":   {"type":"number"},
                                            "y_mm":   {"type":"number"},
                                            "w_mm":   {"type":"number"},
                                            "h_mm":   {"type":"number"},
                                            "layer":  {"type":"string","enum":["top","bottom"],"default":"top"}
                                        }
                                    }
                                }
                            }
                        },
                        "minItems": 1
                    }
                },
                "additionalProperties": false
            }
        },
        {
            "name": "palette.clear",
            "description": "Drop every footprint currently in the palette. Footprints already on the board are untouched.",
            "inputSchema": { "type": "object", "properties": {}, "additionalProperties": false }
        },
        {
            "name": "placement.place_from_palette",
            "description": "Take the named footprint out of the palette and place it on the board at (x_mm, y_mm). The footprint is then locked at that position from the auto-placer's point of view (it's a board obstacle). Used by the human's drag-from-palette gesture.",
            "inputSchema": {
                "type": "object",
                "required": ["reference","x_mm","y_mm"],
                "properties": {
                    "reference": {"type":"string"},
                    "x_mm":      {"type":"number"},
                    "y_mm":      {"type":"number"}
                },
                "additionalProperties": false
            }
        },
        {
            "name": "placement.rotate",
            "description": "Rotate a footprint already on the board. `degrees` is the new absolute rotation (CCW); use 0, 90, 180, or 270 for clean orthogonal layouts.",
            "inputSchema": {
                "type": "object",
                "required": ["reference","degrees"],
                "properties": {
                    "reference": {"type":"string"},
                    "degrees":   {"type":"number"}
                },
                "additionalProperties": false
            }
        },
        {
            "name": "route.clear_net",
            "description": "Drop every trace and via belonging to one net. Use this before re-routing only that net (route.run reroutes everything).",
            "inputSchema": {
                "type": "object",
                "required": ["net"],
                "properties": { "net": {"type":"string"} },
                "additionalProperties": false
            }
        },
        {
            "name": "route.delete_trace",
            "description": "Remove a single trace segment by id. Use the id returned by route.run or read from view.snapshot's structured content.",
            "inputSchema": {
                "type": "object",
                "required": ["id"],
                "properties": { "id": {"type":"string"} },
                "additionalProperties": false
            }
        },
        {
            "name": "route.delete_via",
            "description": "Remove a via by id.",
            "inputSchema": {
                "type": "object",
                "required": ["id"],
                "properties": { "id": {"type":"string"} },
                "additionalProperties": false
            }
        },
        {
            "name": "placement.move",
            "description": "Move a footprint already on the board to a new position. Used by the human's drag-within-canvas gesture.",
            "inputSchema": {
                "type": "object",
                "required": ["reference","x_mm","y_mm"],
                "properties": {
                    "reference": {"type":"string"},
                    "x_mm":      {"type":"number"},
                    "y_mm":      {"type":"number"}
                },
                "additionalProperties": false
            }
        },
        {
            "name": "placement.auto",
            "description": "Auto-place every footprint currently in the palette using a force-directed simulation. Footprints already on the board are treated as locked obstacles — they don't move. The simulation streams frames to the UI so the human watches components settle in real time. Idempotent: repeated calls just re-settle the same palette items. Requires the board outline to be set first (board.set_outline).",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "iterations":   { "type": "integer", "default": 200 },
                    "frame_every":  { "type": "integer", "default": 4 },
                    "frame_delay_ms":{ "type": "integer", "default": 100 }
                },
                "additionalProperties": false
            }
        },
        {
            "name": "placement.from_schematic",
            "description": "Bridge schematic → board. For each entry, find the same-reference symbol in the schematic, build a Footprint with the given physical pad geometry, and copy the schematic's net assignments onto each pad. Pads are looked up by number; use pin_map={schematic_pin: board_pad} if a footprint's pad numbers do not match the schematic's pin numbers (e.g. an MCU package with pin names instead of numbers). Returns the list of placed footprints and the ratsnest (pads grouped by net) so a future router has a starting point.",
            "inputSchema": {
                "type": "object",
                "required": ["footprints"],
                "properties": {
                    "footprints": {
                        "type": "array",
                        "items": {
                            "type": "object",
                            "required": ["reference", "library", "x_mm", "y_mm", "pads"],
                            "properties": {
                                "reference": { "type": "string" },
                                "library":   { "type": "string", "description": "e.g. Resistor_SMD:R_0805" },
                                "x_mm":      { "type": "number" },
                                "y_mm":      { "type": "number" },
                                "rotation":  { "type": "number", "default": 0 },
                                "layer":     { "type": "string", "enum": ["top","bottom"], "default": "top" },
                                "pin_map":   {
                                    "type": "object",
                                    "description": "schematic_pin → board_pad mapping; identity if omitted",
                                    "additionalProperties": { "type": "string" }
                                },
                                "pads": {
                                    "type": "array",
                                    "items": {
                                        "type": "object",
                                        "required": ["number","x_mm","y_mm","w_mm","h_mm"],
                                        "properties": {
                                            "number": { "type": "string" },
                                            "x_mm":   { "type": "number" },
                                            "y_mm":   { "type": "number" },
                                            "w_mm":   { "type": "number" },
                                            "h_mm":   { "type": "number" },
                                            "layer":  { "type": "string", "enum": ["top","bottom"], "default": "top" }
                                        }
                                    }
                                }
                            }
                        },
                        "minItems": 1
                    }
                },
                "additionalProperties": false
            }
        },
        {
            "name": "route.add_trace",
            "description": "Add a single straight copper trace segment between (x1,y1) and (x2,y2) on a copper layer. Used by the agent for hand-routing or by future routers laying paths segment-by-segment.",
            "inputSchema": {
                "type": "object",
                "required": ["net","layer","x1_mm","y1_mm","x2_mm","y2_mm","width_mm"],
                "properties": {
                    "net":     { "type": "string" },
                    "layer":   { "type": "string", "enum": ["top","bottom"] },
                    "x1_mm":   { "type": "number" },
                    "y1_mm":   { "type": "number" },
                    "x2_mm":   { "type": "number" },
                    "y2_mm":   { "type": "number" },
                    "width_mm":{ "type": "number" }
                },
                "additionalProperties": false
            }
        },
        {
            "name": "route.add_via",
            "description": "Add a through-hole via at (x,y). Joins both copper layers and produces an entry in the PTH drill file.",
            "inputSchema": {
                "type": "object",
                "required": ["net","x_mm","y_mm","drill_mm","diameter_mm"],
                "properties": {
                    "net":         { "type": "string" },
                    "x_mm":        { "type": "number" },
                    "y_mm":        { "type": "number" },
                    "drill_mm":    { "type": "number", "description": "hole diameter" },
                    "diameter_mm": { "type": "number", "description": "copper pad diameter (drill + 2 × annular ring)" }
                },
                "additionalProperties": false
            }
        },
        {
            "name": "route.run",
            "description": "Auto-route every net on the board using the native grid A* router on two copper layers. Existing routing is cleared before the pass. Returns per-net outcome (success with trace/via counts, or failure reason) plus aggregate totals. Net order is set by the router (smallest pad-count first); the agent does not need to specify it.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "cell_mm":         { "type": "number", "default": 0.25, "description": "grid cell pitch" },
                    "trace_width_mm":  { "type": "number", "default": 0.25 },
                    "clearance_mm":    { "type": "number", "default": 0.20 },
                    "via_cost":        { "type": "integer", "default": 8, "description": "cells of penalty per layer flip" },
                    "via_drill_mm":    { "type": "number", "default": 0.30 },
                    "via_diameter_mm": { "type": "number", "default": 0.60 }
                },
                "additionalProperties": false
            }
        },
        {
            "name": "route.clear",
            "description": "Drop every trace and via on the board. Footprints and the schematic are untouched.",
            "inputSchema": {
                "type": "object",
                "properties": {},
                "additionalProperties": false
            }
        },
        {
            "name": "drc.run",
            "description": "Run the native design-rule checker on the current board: pad↔pad / trace↔trace / trace↔pad clearance, edge clearance, unconnected pads, narrow traces, small drills. Returns a structured report with the list of violations (each has a position, severity, and the items involved).",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "min_clearance_mm": { "type": "number", "default": 0.2 },
                    "edge_clearance_mm": { "type": "number", "default": 0.3 },
                    "min_trace_width_mm": { "type": "number", "default": 0.1 },
                    "min_drill_mm": { "type": "number", "default": 0.2 }
                },
                "additionalProperties": false
            }
        },
        {
            "name": "output.fab_pack",
            "description": "Write the manufacturing fab pack — Gerber RS-274X (copper, mask, edge cuts), Excellon drill files, BOM CSV, and pick-and-place CSV — to a directory on disk. Returns the absolute paths of every file written.",
            "inputSchema": {
                "type": "object",
                "required": ["out_dir"],
                "properties": {
                    "out_dir": { "type": "string", "description": "absolute path to the output directory (created if missing)" },
                    "name":    { "type": "string", "description": "optional filename stem; defaults to the project name" }
                },
                "additionalProperties": false
            }
        }
    ])
}

/// Dispatch a `tools/call` to the right handler.
pub async fn dispatch(project: &Project, name: &str, args: &Value) -> Result<Value, ToolError> {
    match name {
        "project.status" => tool_project_status(project),
        "project.reset" => tool_project_reset(project),
        "board.set_outline" => tool_board_set_outline(project, args),
        "placement.add" => tool_placement_add(project, args),
        "view.snapshot" => tool_view_snapshot(project),
        "schematic.add_symbol" => tool_schematic_add_symbol(project, args),
        "schematic.connect" => tool_schematic_connect(project, args),
        "schematic.status" => tool_schematic_status(project),
        "schematic.snapshot" => tool_schematic_snapshot(project),
        "placement.from_schematic" => tool_placement_from_schematic(project, args),
        "placement.auto" => tool_placement_auto(project, args).await,
        "palette.add" => tool_palette_add(project, args),
        "palette.clear" => tool_palette_clear(project),
        "placement.place_from_palette" => tool_place_from_palette(project, args),
        "placement.move" => tool_placement_move(project, args),
        "placement.rotate" => tool_placement_rotate(project, args),
        "route.clear_net" => tool_route_clear_net(project, args),
        "route.delete_trace" => tool_route_delete_trace(project, args),
        "route.delete_via" => tool_route_delete_via(project, args),
        "route.add_trace" => tool_route_add_trace(project, args),
        "route.add_via" => tool_route_add_via(project, args),
        "route.clear" => tool_route_clear(project),
        "route.run" => tool_route_run(project, args),
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
            json!({
                "id": fp.id.0.to_string(),
                "reference": fp.reference,
                "value": fp.value,
                "library": fp.library,
                "x_mm": fp.position.x.to_mm(),
                "y_mm": fp.position.y.to_mm(),
                "rotation": fp.rotation,
                "pads": fp.pads.iter().map(|p| json!({
                    "number": p.number,
                    "net": p.net,
                })).collect::<Vec<_>>(),
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
            let (sym_ref, pin_num) = pin_ref.split_once('.').ok_or_else(|| {
                ToolError::invalid_params(format!(
                    "expected REF.PIN format, got {pin_ref:?}"
                ))
            })?;
            let symbol = sch.find_by_reference(sym_ref).ok_or_else(|| {
                ToolError::invalid_params(format!("unknown symbol {sym_ref}"))
            })?;
            connections.push(NetConnection {
                symbol_id: symbol.id,
                pin_number: pin_num.to_string(),
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
}

fn tool_palette_add(project: &Project, args: &Value) -> Result<Value, ToolError> {
    let input: PaletteAddInput = serde_json::from_value(args.clone())
        .map_err(|e| ToolError::invalid_params(format!("palette.add: {e}")))?;

    let mut added = Vec::with_capacity(input.footprints.len());
    for plan in input.footprints {
        // Pull the value + net assignments from the schematic so the
        // palette item carries everything the bridge would have given it.
        let (value, pads) = {
            let snap = project.read();
            let sch = snap.schematic();
            let symbol = sch.find_by_reference(&plan.reference).ok_or_else(|| {
                ToolError::invalid_params(format!(
                    "palette.add: no schematic symbol named {}",
                    plan.reference
                ))
            })?;
            let value = symbol.value.clone();
            let pads: Vec<Pad> = plan
                .pads
                .iter()
                .map(|pad_plan| {
                    let net = sch.net_for_pin(symbol.id, &pad_plan.number).map(str::to_string);
                    Pad {
                        number: pad_plan.number.clone(),
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
            (value, pads)
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

#[derive(Debug, Deserialize)]
struct PlaceFromPaletteInput {
    reference: String,
    x_mm: f64,
    y_mm: f64,
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
struct AutoPlaceInput {
    #[serde(default = "default_iterations")]
    iterations: u32,
    #[serde(default = "default_frame_every")]
    frame_every: u32,
    #[serde(default = "default_frame_delay_ms")]
    frame_delay_ms: u64,
}

fn default_iterations() -> u32 { 200 }
fn default_frame_every() -> u32 { 4 }
fn default_frame_delay_ms() -> u64 { 100 }

async fn tool_placement_auto(project: &Project, args: &Value) -> Result<Value, ToolError> {
    let input: AutoPlaceInput = serde_json::from_value(args.clone())
        .map_err(|e| ToolError::invalid_params(format!("placement.auto: {e}")))?;

    // Reclaim any board footprint that drifted outside the outline.
    let _ = project.unplace_out_of_bounds();

    let bounds = project.read().board().outline.ok_or_else(|| {
        ToolError::invalid_params(
            "no board outline; call board.set_outline { w_mm, h_mm } first".to_string(),
        )
    })?;

    // Snapshot the inputs: palette items become the unlocked particles,
    // footprints already on the board are anchors. Both contribute to
    // the spring + repulsion model.
    struct Item {
        reference: String,
        bbox_w: Length,
        bbox_h: Length,
        position: Point,
        locked: bool,
        footprint: Footprint,
        is_palette: bool,
    }
    let mut items: Vec<Item> = Vec::new();
    {
        let snap = project.read();
        for fp in snap.board().footprints_in_order() {
            let bounds_fp = fp.bounds().unwrap_or_else(|| {
                pcb_core::Rect::from_corners(fp.position, fp.position)
            });
            items.push(Item {
                reference: fp.reference.clone(),
                bbox_w: bounds_fp.width(),
                bbox_h: bounds_fp.height(),
                position: fp.position,
                locked: true,
                footprint: fp.clone(),
                is_palette: false,
            });
        }
        for fp in snap.palette() {
            let bounds_fp = fp.bounds().unwrap_or_else(|| {
                pcb_core::Rect::from_corners(fp.position, fp.position)
            });
            items.push(Item {
                reference: fp.reference.clone(),
                bbox_w: bounds_fp.width(),
                bbox_h: bounds_fp.height(),
                position: fp.position, // off-canvas placeholder
                locked: false,
                footprint: fp.clone(),
                is_palette: true,
            });
        }
    }
    let palette_count = items.iter().filter(|i| i.is_palette).count();
    if palette_count == 0 {
        return Ok(text_result("Palette is empty; nothing to auto-place").into());
    }

    // Sprinkle palette particles inside the board so they have a
    // sensible starting point. Locked items keep their current
    // position.
    {
        #[allow(clippy::cast_precision_loss)]
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
            item.position = Point::new(
                Length::from_mm(bx + dx * (col + 1.0)),
                Length::from_mm(by + dy * (row + 1.0)),
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
    let footprints_for_board: Vec<Footprint> =
        items.iter().map(|i| i.footprint.clone()).collect();
    let palette_refs: std::collections::HashSet<String> = items
        .iter()
        .filter(|i| i.is_palette)
        .map(|i| i.reference.clone())
        .collect();

    // Nets for the placer come from the schematic — group references
    // touching each net and let the placer pull them together.
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

    let placer_input = pcb_placer::PlacementInput {
        footprints: placeable,
        nets,
        bounds: Some(bounds),
    };
    let mut placer = pcb_placer::Placer::new(
        placer_input,
        pcb_placer::PlacerOptions {
            total_steps: input.iterations,
            ..Default::default()
        },
    );

    // Move every palette item onto the board at its initial sprinkled
    // position. From this point on they are regular board footprints
    // that we update with placement.move on each frame; the placer's
    // locked flag determines who actually gets moved.
    for reference in &palette_refs {
        let item = items
            .iter()
            .find(|i| &i.reference == reference)
            .expect("palette ref present");
        if let Err(e) = project.place_from_palette(reference, item.position) {
            return Err(ToolError {
                code: crate::protocol::error_code::INTERNAL_ERROR,
                message: format!("place_from_palette({reference}): {e}"),
            });
        }
    }
    project.clear_routing();

    // Animate. Each frame: write the new positions of the palette
    // items (locked items stay where they are) and emit progress.
    let _ = footprints_for_board; // kept for future per-frame templating
    for i in 0..input.iterations {
        let frame = placer.step();
        if i % input.frame_every == 0 || i == input.iterations - 1 {
            for (reference, position) in &frame.positions {
                if !palette_refs.contains(reference) {
                    continue; // locked, don't move
                }
                let _ = project.move_footprint_to(reference, *position);
            }
            project
                .events()
                .publish(pcb_core::Event::PlacementProgress { iteration: frame.iteration });
            tokio::time::sleep(std::time::Duration::from_millis(input.frame_delay_ms)).await;
        }
    }

    placer.finalise();
    // Apply the best-cost positions and rotations to the project.
    for fp in placer.current() {
        let _ = project.move_footprint_to(&fp.reference, fp.position);
        let _ = project.rotate_footprint(&fp.reference, fp.rotation);
    }
    let final_state = placer.current();
    let final_positions: Vec<Value> = final_state
        .iter()
        .map(|fp| {
            json!({
                "reference": fp.reference,
                "x_mm": fp.position.x.to_mm(),
                "y_mm": fp.position.y.to_mm(),
            })
        })
        .collect();
    project.log(
        ActivityLevel::Info,
        format!("placement.auto: settled after {} iterations", input.iterations),
    );
    Ok(text_result(format!(
        "Placed {} footprint(s) over {} iterations",
        final_state.len(),
        input.iterations
    ))
    .with_data(json!({
        "iterations": input.iterations,
        "positions": final_positions,
    })))
}

#[derive(Debug, Deserialize)]
struct PlaceFromSchInput {
    footprints: Vec<FootprintPlan>,
}

#[derive(Debug, Deserialize)]
struct FootprintPlan {
    reference: String,
    library: String,
    x_mm: f64,
    y_mm: f64,
    #[serde(default)]
    rotation: f32,
    #[serde(default = "default_layer")]
    layer: LayerInput,
    #[serde(default)]
    pin_map: std::collections::HashMap<String, String>,
    pads: Vec<PadPlan>,
}

#[derive(Debug, Deserialize)]
struct PadPlan {
    number: String,
    x_mm: f64,
    y_mm: f64,
    w_mm: f64,
    h_mm: f64,
    #[serde(default = "default_layer")]
    layer: LayerInput,
}

fn tool_placement_from_schematic(project: &Project, args: &Value) -> Result<Value, ToolError> {
    let input: PlaceFromSchInput = serde_json::from_value(args.clone())
        .map_err(|e| ToolError::invalid_params(format!("placement.from_schematic: {e}")))?;

    // Resolve every plan against the current schematic before mutating
    // the board: if any reference is missing, fail fast and leave the
    // project untouched.
    let mut placed: Vec<(Footprint, Vec<(String, Option<String>)>)> = Vec::new();
    {
        let snap = project.read();
        let sch = snap.schematic();
        for plan in &input.footprints {
            let symbol = sch.find_by_reference(&plan.reference).ok_or_else(|| {
                ToolError::invalid_params(format!(
                    "no schematic symbol named {}",
                    plan.reference
                ))
            })?;
            let mut pads = Vec::with_capacity(plan.pads.len());
            let mut net_summary = Vec::with_capacity(plan.pads.len());
            for pad_plan in &plan.pads {
                // Identity by default; override via pin_map.
                let schematic_pin = plan
                    .pin_map
                    .iter()
                    .find_map(|(sch_p, board_p)| {
                        (board_p == &pad_plan.number).then(|| sch_p.clone())
                    })
                    .unwrap_or_else(|| pad_plan.number.clone());
                let net = sch
                    .net_for_pin(symbol.id, &schematic_pin)
                    .map(str::to_string);
                net_summary.push((pad_plan.number.clone(), net.clone()));
                pads.push(Pad {
                    number: pad_plan.number.clone(),
                    offset: Point::new(
                        Length::from_mm(pad_plan.x_mm),
                        Length::from_mm(pad_plan.y_mm),
                    ),
                    size: (
                        Length::from_mm(pad_plan.w_mm),
                        Length::from_mm(pad_plan.h_mm),
                    ),
                    layer: pad_plan.layer.into(),
                    net,
                });
            }
            let footprint = Footprint {
                id: pcb_core::Id::new(),
                reference: plan.reference.clone(),
                value: symbol.value.clone(),
                library: plan.library.clone(),
                position: Point::new(
                    Length::from_mm(plan.x_mm),
                    Length::from_mm(plan.y_mm),
                ),
                rotation: plan.rotation,
                layer: plan.layer.into(),
                pads,
            };
            placed.push((footprint, net_summary));
        }
    }

    let mut placed_summaries = Vec::with_capacity(placed.len());
    for (footprint, net_summary) in placed {
        let reference = footprint.reference.clone();
        let id = project.add_footprint(footprint);
        placed_summaries.push(json!({
            "id": id.0.to_string(),
            "reference": reference,
            "pads": net_summary.iter().map(|(num, net)| {
                json!({"number": num, "net": net})
            }).collect::<Vec<_>>(),
        }));
    }

    // Ratsnest: pads grouped by net, derived from the freshly-placed
    // footprints so the agent (and a future router) has the connectivity
    // graph in hand without re-querying.
    let mut ratsnest: std::collections::BTreeMap<String, Vec<String>> = std::collections::BTreeMap::new();
    {
        let snap = project.read();
        for fp in snap.board().footprints_in_order() {
            for pad in &fp.pads {
                if let Some(net) = &pad.net {
                    ratsnest
                        .entry(net.clone())
                        .or_default()
                        .push(format!("{}.{}", fp.reference, pad.number));
                }
            }
        }
    }
    let ratsnest_json: Vec<Value> = ratsnest
        .into_iter()
        .map(|(net, pads)| json!({"net": net, "pads": pads}))
        .collect();

    project.log(
        ActivityLevel::Info,
        format!(
            "placement.from_schematic: placed {} footprint(s)",
            placed_summaries.len()
        ),
    );
    Ok(text_result(format!(
        "Placed {} footprint(s) from the schematic",
        placed_summaries.len()
    ))
    .with_data(json!({
        "placed": placed_summaries,
        "ratsnest": ratsnest_json,
    })))
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
