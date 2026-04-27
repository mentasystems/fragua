//! Tool surface — what the MCP client (the AI agent) can call.
//!
//! Each tool is intentionally thin: parse the input, call into
//! `pcb-core` to mutate the project, return the result. The agent owns
//! all the design reasoning; tools are pure data primitives.

use pcb_core::schematic::{Net, NetConnection, PinSide, SchPin, Symbol, SymbolKind};
use pcb_core::{ActivityLevel, CopperLayer, Footprint, Length, Pad, Point, Project};
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
pub fn dispatch(project: &Project, name: &str, args: &Value) -> Result<Value, ToolError> {
    match name {
        "project.status" => tool_project_status(project),
        "placement.add" => tool_placement_add(project, args),
        "view.snapshot" => tool_view_snapshot(project),
        "schematic.add_symbol" => tool_schematic_add_symbol(project, args),
        "schematic.connect" => tool_schematic_connect(project, args),
        "schematic.status" => tool_schematic_status(project),
        "schematic.snapshot" => tool_schematic_snapshot(project),
        "output.fab_pack" => tool_output_fab_pack(project, args),
        _ => Err(ToolError {
            code: crate::protocol::error_code::METHOD_NOT_FOUND,
            message: format!("unknown tool: {name}"),
        }),
    }
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
    let svg = pcb_render::render_svg(snap.board());
    Ok(text_result(svg).into())
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
