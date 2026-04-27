//! Tool surface — what the MCP client (the AI agent) can call.
//!
//! Each tool is intentionally thin: parse the input, call into
//! `pcb-core` to mutate the project, return the result. The agent owns
//! all the design reasoning; tools are pure data primitives.

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
