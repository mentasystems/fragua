//! Tiny line-oriented DSL for `script` — the only tool the MCP catalog
//! advertises. The agent writes a multi-line program; the parser turns
//! each line into an existing internal tool call which `dispatch`
//! executes in order.
//!
//! Why a DSL: a 30-line JSON `ops` array bloats context and burns
//! tokens. An equivalent script is closer to 30 short lines of plain
//! text, no bracket noise. The catalog stays at one tool, so the agent
//! only loads ONE schema.
//!
//! Grammar in one paragraph: each non-empty, non-comment line is a
//! command — `verb arg1 arg2 ... key=value ...`. Strings with spaces
//! are double-quoted. Indented lines (2 spaces or a tab) extend the
//! previous block-opening command (`sym` / `lib`) with `pin` or `pad`
//! sub-entries. `#` starts a line comment.

use serde_json::{json, Value};

/// One parsed command + the line number it came from. The compiler
/// turns each `Cmd` into a `(tool_name, args_json)` pair routed through
/// the regular `dispatch` function.
#[derive(Debug)]
pub struct Cmd {
    pub line: usize,
    pub tool: String,
    pub args: Value,
}

#[derive(Debug)]
pub struct ParseError {
    pub line: usize,
    pub message: String,
}

impl ParseError {
    fn at(line: usize, msg: impl Into<String>) -> Self {
        Self { line, message: msg.into() }
    }
}

/// Parse a multi-line script into a flat list of dispatch-ready commands.
/// Indented pin/pad sub-lines are folded into their parent block as
/// nested arrays.
pub fn parse(script: &str) -> Result<Vec<Cmd>, ParseError> {
    let mut out = Vec::new();
    // The currently open block (`sym ...` or `lib ...`) — pending
    // pin/pad continuations land here. `None` means the previous line
    // was a standalone command.
    let mut block: Option<Block> = None;

    for (idx, raw) in script.lines().enumerate() {
        let line_no = idx + 1;
        // Strip trailing whitespace; preserve leading so we can detect indent.
        let line = strip_trailing(raw);
        if line.trim().is_empty() {
            continue;
        }
        if line.trim_start().starts_with('#') {
            continue;
        }
        let indented = line.starts_with(' ') || line.starts_with('\t');
        let body = line.trim();
        let tokens = tokenise(body, line_no)?;

        if indented {
            let Some(b) = block.as_mut() else {
                return Err(ParseError::at(
                    line_no,
                    "indented line has no open block — start with `sym` or `lib` first",
                ));
            };
            b.absorb_continuation(line_no, &tokens)?;
            continue;
        }

        // Non-indented: any open block is finished.
        if let Some(b) = block.take() {
            out.push(b.finish()?);
        }
        let verb = tokens[0].clone();
        if opens_block(&verb) {
            block = Some(Block::open(line_no, verb, tokens)?);
        } else {
            out.push(compile_command(line_no, &tokens)?);
        }
    }
    if let Some(b) = block {
        out.push(b.finish()?);
    }
    Ok(out)
}

// ─── Tokeniser ────────────────────────────────────────────────────────

fn tokenise(body: &str, line_no: usize) -> Result<Vec<String>, ParseError> {
    let mut out = Vec::new();
    let mut chars = body.chars().peekable();
    while let Some(&c) = chars.peek() {
        if c.is_whitespace() {
            chars.next();
            continue;
        }
        let mut tok = String::new();
        if c == '"' {
            chars.next(); // consume opening quote
            // Quoted strings allow escaped \" and \\.
            loop {
                match chars.next() {
                    Some('"') => break,
                    Some('\\') => match chars.next() {
                        Some(esc) => tok.push(esc),
                        None => {
                            return Err(ParseError::at(line_no, "unterminated escape inside \"..\""))
                        }
                    },
                    Some(c) => tok.push(c),
                    None => return Err(ParseError::at(line_no, "unterminated quoted string")),
                }
            }
            out.push(tok);
        } else {
            // Bare token — allow `key=value` where value may itself be quoted.
            while let Some(&c) = chars.peek() {
                if c.is_whitespace() {
                    break;
                }
                if c == '=' {
                    tok.push(c);
                    chars.next();
                    // If the next char is `"`, slurp a quoted string into the token.
                    if matches!(chars.peek(), Some('"')) {
                        chars.next(); // opening quote
                        loop {
                            match chars.next() {
                                Some('"') => break,
                                Some('\\') => match chars.next() {
                                    Some(esc) => tok.push(esc),
                                    None => {
                                        return Err(ParseError::at(
                                            line_no,
                                            "unterminated escape inside key=\"..\"",
                                        ))
                                    }
                                },
                                Some(c) => tok.push(c),
                                None => {
                                    return Err(ParseError::at(
                                        line_no,
                                        "unterminated quoted value after `=`",
                                    ))
                                }
                            }
                        }
                        break;
                    }
                    continue;
                }
                tok.push(c);
                chars.next();
            }
            out.push(tok);
        }
    }
    if out.is_empty() {
        return Err(ParseError::at(line_no, "empty command"));
    }
    Ok(out)
}

fn strip_trailing(s: &str) -> &str {
    s.trim_end_matches(|c: char| c == ' ' || c == '\t' || c == '\r')
}

// ─── Block (sym / lib) accumulator ────────────────────────────────────

fn opens_block(verb: &str) -> bool {
    matches!(verb, "sym" | "lib")
}

struct Block {
    verb: String,
    line: usize,
    tokens: Vec<String>,
    pins: Vec<Value>,
    pads: Vec<Value>,
}

impl Block {
    fn open(line: usize, verb: String, tokens: Vec<String>) -> Result<Self, ParseError> {
        Ok(Self {
            verb,
            line,
            tokens,
            pins: Vec::new(),
            pads: Vec::new(),
        })
    }

    fn absorb_continuation(&mut self, line: usize, tokens: &[String]) -> Result<(), ParseError> {
        match (self.verb.as_str(), tokens[0].as_str()) {
            ("sym", "pin") => {
                // `pin NUMBER SIDE [name=NAME]` or `pin NUMBER SIDE NAME`
                if tokens.len() < 3 {
                    return Err(ParseError::at(
                        line,
                        "pin needs at least: pin NUMBER SIDE [NAME]",
                    ));
                }
                let number = tokens[1].clone();
                let side = expand_side(&tokens[2], line)?;
                let mut name = String::new();
                for t in &tokens[3..] {
                    if let Some(rest) = t.strip_prefix("name=") {
                        name = rest.to_string();
                    } else if !t.contains('=') {
                        // Bare third token is the pin name (e.g. `pin 1 L V5`).
                        name = t.clone();
                    }
                }
                let mut pin = json!({"number": number, "side": side});
                if !name.is_empty() {
                    pin.as_object_mut().unwrap().insert("name".into(), Value::String(name));
                }
                self.pins.push(pin);
                Ok(())
            }
            ("lib", "pad") => {
                // `pad NUMBER X Y W H [name=NAME]`
                if tokens.len() < 6 {
                    return Err(ParseError::at(
                        line,
                        "pad needs: pad NUMBER X Y W H [name=NAME]",
                    ));
                }
                let number = tokens[1].clone();
                let x = parse_num(&tokens[2], line, "x")?;
                let y = parse_num(&tokens[3], line, "y")?;
                let w = parse_num(&tokens[4], line, "w")?;
                let h = parse_num(&tokens[5], line, "h")?;
                let mut name = String::new();
                for t in &tokens[6..] {
                    if let Some(rest) = t.strip_prefix("name=") {
                        name = rest.to_string();
                    }
                }
                let mut pad = json!({
                    "number": number,
                    "x_mm": x, "y_mm": y, "w_mm": w, "h_mm": h,
                });
                if !name.is_empty() {
                    pad.as_object_mut().unwrap().insert("name".into(), Value::String(name));
                }
                self.pads.push(pad);
                Ok(())
            }
            (verb, other) => Err(ParseError::at(
                line,
                format!("`{verb}` block can't contain `{other}`"),
            )),
        }
    }

    fn finish(self) -> Result<Cmd, ParseError> {
        let tokens = &self.tokens;
        let line = self.line;
        match self.verb.as_str() {
            "sym" => {
                // sym REF KIND [key=K] [value=V] [rot=DEG] [x=N] [y=N] [desc="..."]
                if tokens.len() < 3 {
                    return Err(ParseError::at(
                        line,
                        "sym needs: sym REF KIND [...key=value...]",
                    ));
                }
                let reference = tokens[1].clone();
                let kind = expand_kind(&tokens[2], line)?;
                let mut args = json!({
                    "reference": reference,
                    "kind": kind,
                });
                apply_kv(&mut args, &tokens[3..], line, &[
                    ("key", AttrType::Str), ("value", AttrType::Str),
                    ("rot", AttrType::NumInto("rotation")),
                    ("rotation", AttrType::Num),
                    ("x", AttrType::NumInto("x_mm")),
                    ("y", AttrType::NumInto("y_mm")),
                    ("desc", AttrType::StrInto("description")),
                    ("description", AttrType::Str),
                ])?;
                if kind == "generic_ic" {
                    args.as_object_mut().unwrap().insert("pins".into(), Value::Array(self.pins));
                }
                Ok(Cmd { line, tool: "schematic.add_symbol".into(), args })
            }
            "lib" => {
                // lib KEY [value=V] [rot=DEG] [edge=true|false] [desc="..."]
                if tokens.len() < 2 {
                    return Err(ParseError::at(line, "lib needs: lib KEY [...]"));
                }
                let key = tokens[1].clone();
                if self.pads.is_empty() {
                    return Err(ParseError::at(
                        line,
                        format!("lib {key} needs at least one indented `pad ...` line"),
                    ));
                }
                let mut args = json!({"key": key, "pads": self.pads});
                apply_kv(&mut args, &tokens[2..], line, &[
                    ("value", AttrType::StrInto("default_value")),
                    ("default_value", AttrType::Str),
                    ("rot", AttrType::NumInto("default_rotation_deg")),
                    ("default_rotation_deg", AttrType::Num),
                    ("edge", AttrType::BoolInto("edge_mounted")),
                    ("edge_mounted", AttrType::Bool),
                    ("desc", AttrType::StrInto("description")),
                    ("description", AttrType::Str),
                ])?;
                // `description` is required by library.create — synthesise an empty
                // string if the agent didn't provide one (the tool will reject empty
                // descriptions only if the schema declares them required).
                if !args.as_object().unwrap().contains_key("description") {
                    args.as_object_mut().unwrap().insert("description".into(), json!(""));
                }
                Ok(Cmd { line, tool: "library.create".into(), args })
            }
            _ => unreachable!(),
        }
    }
}

// ─── Single-line command compiler ─────────────────────────────────────

fn compile_command(line: usize, tokens: &[String]) -> Result<Cmd, ParseError> {
    let verb = tokens[0].as_str();
    match verb {
        "reset"        => Ok(Cmd { line, tool: "project.reset".into(), args: json!({}) }),
        "status"       => Ok(Cmd { line, tool: "project.status".into(), args: json!({}) }),
        "view"         => Ok(Cmd { line, tool: "view.summary".into(), args: json!({}) }),
        "snap"         => Ok(Cmd { line, tool: "view.snapshot".into(), args: json!({}) }),
        "sch"          => Ok(Cmd { line, tool: "schematic.snapshot".into(), args: json!({}) }),
        "sch-status"   => Ok(Cmd { line, tool: "schematic.status".into(), args: json!({}) }),
        "nets"         => Ok(Cmd { line, tool: "net.status".into(), args: json!({}) }),
        "list-lib"     => Ok(Cmd { line, tool: "library.list".into(), args: json!({}) }),
        "list-palette" => Ok(Cmd { line, tool: "palette.list".into(), args: json!({}) }),
        "clear-palette" => Ok(Cmd { line, tool: "palette.clear".into(), args: json!({}) }),
        "clear-route"  => Ok(Cmd { line, tool: "route.clear".into(), args: json!({}) }),
        "save" => {
            // save PATH  — write the project JSON to PATH (atomic).
            need_args(line, tokens, 1, "save PATH")?;
            let path = tokens[1..].join(" ");
            Ok(Cmd { line, tool: "project.save".into(), args: json!({"path": path}) })
        }
        "pour" => {
            // pour NET LAYER  (LAYER = top|bottom)
            need_args(line, tokens, 2, "pour NET LAYER")?;
            Ok(Cmd { line, tool: "pour.add".into(),
                args: json!({"net": tokens[1], "layer": tokens[2]}) })
        }
        "clear-pour" => {
            // clear-pour NET LAYER
            need_args(line, tokens, 2, "clear-pour NET LAYER")?;
            Ok(Cmd { line, tool: "pour.remove".into(),
                args: json!({"net": tokens[1], "layer": tokens[2]}) })
        }

        "outline" => {
            need_args(line, tokens, 2, "outline W H")?;
            let w = parse_num(&tokens[1], line, "W")?;
            let h = parse_num(&tokens[2], line, "H")?;
            Ok(Cmd { line, tool: "board.set_outline".into(),
                     args: json!({"w_mm": w, "h_mm": h}) })
        }

        "net" => {
            need_args(line, tokens, 2, "net NAME PIN1 [PIN2 ...]")?;
            let name = tokens[1].clone();
            let pins: Vec<Value> = tokens[2..].iter().map(|s| Value::String(s.clone())).collect();
            Ok(Cmd { line, tool: "schematic.connect".into(),
                     args: json!({"net": name, "pins": pins}) })
        }

        "find-lib" => {
            need_args(line, tokens, 1, "find-lib KEY")?;
            Ok(Cmd { line, tool: "library.find".into(),
                     args: json!({"key": tokens[1]}) })
        }
        "delete-lib" => {
            need_args(line, tokens, 1, "delete-lib KEY")?;
            Ok(Cmd { line, tool: "library.delete".into(),
                     args: json!({"key": tokens[1]}) })
        }
        "detach" => {
            need_args(line, tokens, 2, "detach KEY ATTACHMENT_ID")?;
            Ok(Cmd { line, tool: "library.delete_attachment".into(),
                     args: json!({"key": tokens[1], "attachment_id": tokens[2]}) })
        }
        "attach" => {
            // attach KEY KIND PATH [mime=MIME] [filename=NAME]
            need_args(line, tokens, 3, "attach KEY KIND PATH [mime=...] [filename=...]")?;
            let key = tokens[1].clone();
            let kind = tokens[2].clone();
            let path = tokens[3].clone();
            // Read + base64-encode the file. Mime auto-detected from the
            // path extension unless overridden.
            let bytes = std::fs::read(&path).map_err(|e| ParseError::at(
                line, format!("attach: cannot read {path}: {e}"),
            ))?;
            use base64::Engine;
            let data_base64 = base64::engine::general_purpose::STANDARD.encode(&bytes);
            let filename = std::path::Path::new(&path)
                .file_name()
                .and_then(|s| s.to_str())
                .unwrap_or(&path).to_string();
            let mime = guess_mime(&path);
            let mut args = json!({
                "key": key,
                "kind": kind,
                "filename": filename,
                "mime": mime,
                "data_base64": data_base64,
            });
            apply_kv(&mut args, &tokens[4..], line, &[
                ("mime", AttrType::Str),
                ("filename", AttrType::Str),
            ])?;
            Ok(Cmd { line, tool: "library.attach".into(), args })
        }

        "palette" => {
            // palette REF KEY [rot=DEG] [value=V] [layer=top|bottom]
            need_args(line, tokens, 2, "palette REF KEY [rot=...] [value=...]")?;
            let mut args = json!({"reference": tokens[1], "key": tokens[2]});
            apply_kv(&mut args, &tokens[3..], line, &[
                ("rot", AttrType::NumInto("rotation")),
                ("rotation", AttrType::Num),
                ("value", AttrType::Str),
                ("layer", AttrType::Str),
            ])?;
            Ok(Cmd { line, tool: "palette.add_from_library".into(), args })
        }

        "place" => {
            // place REF X Y [ROT_DEG]
            need_args(line, tokens, 3, "place REF X Y [ROT_DEG]")?;
            let reference = tokens[1].clone();
            let x = parse_num(&tokens[2], line, "X")?;
            let y = parse_num(&tokens[3], line, "Y")?;
            // If a rotation token was passed, emit a placement.batch with
            // the rotation included; otherwise plain place_from_palette.
            if tokens.len() > 4 {
                let rot = parse_num(&tokens[4], line, "ROT")? as f32;
                Ok(Cmd { line, tool: "placement.batch".into(),
                    args: json!({"items": [{
                        "reference": reference, "x_mm": x, "y_mm": y, "rotation": rot
                    }]}) })
            } else {
                Ok(Cmd { line, tool: "placement.place_from_palette".into(),
                    args: json!({"reference": reference, "x_mm": x, "y_mm": y}) })
            }
        }

        "move" => {
            need_args(line, tokens, 3, "move REF X Y")?;
            Ok(Cmd { line, tool: "placement.move".into(), args: json!({
                "reference": tokens[1],
                "x_mm": parse_num(&tokens[2], line, "X")?,
                "y_mm": parse_num(&tokens[3], line, "Y")?,
            }) })
        }
        "rotate" => {
            need_args(line, tokens, 2, "rotate REF DEG")?;
            Ok(Cmd { line, tool: "placement.rotate".into(), args: json!({
                "reference": tokens[1],
                "degrees": parse_num(&tokens[2], line, "DEG")?,
            }) })
        }

        "route" => {
            // optional kv: trace_width, clearance, via_drill, via_diameter, via_cost, cell
            let mut args = json!({});
            apply_kv(&mut args, &tokens[1..], line, &[
                ("trace_width", AttrType::NumInto("trace_width_mm")),
                ("clearance",   AttrType::NumInto("clearance_mm")),
                ("via_drill",   AttrType::NumInto("via_drill_mm")),
                ("via_diameter",AttrType::NumInto("via_diameter_mm")),
                ("via_cost",    AttrType::Num),
                ("cell",        AttrType::NumInto("cell_mm")),
            ])?;
            Ok(Cmd { line, tool: "route.run".into(), args })
        }
        "clear-net" => {
            need_args(line, tokens, 1, "clear-net NET")?;
            Ok(Cmd { line, tool: "route.clear_net".into(), args: json!({"net": tokens[1]}) })
        }
        "trace" => {
            // trace top|bottom NET X1 Y1 X2 Y2 [width=N]
            need_args(line, tokens, 6, "trace LAYER NET X1 Y1 X2 Y2 [width=...]")?;
            let mut args = json!({
                "layer": tokens[1], "net": tokens[2],
                "x1_mm": parse_num(&tokens[3], line, "X1")?,
                "y1_mm": parse_num(&tokens[4], line, "Y1")?,
                "x2_mm": parse_num(&tokens[5], line, "X2")?,
                "y2_mm": parse_num(&tokens[6], line, "Y2")?,
                "width_mm": 0.25,
            });
            apply_kv(&mut args, &tokens[7..], line, &[
                ("width", AttrType::NumInto("width_mm")),
            ])?;
            Ok(Cmd { line, tool: "route.add_trace".into(), args })
        }
        "via" => {
            // via NET X Y [drill=N] [diameter=N]
            need_args(line, tokens, 3, "via NET X Y [drill=...] [diameter=...]")?;
            let mut args = json!({
                "net": tokens[1],
                "x_mm": parse_num(&tokens[2], line, "X")?,
                "y_mm": parse_num(&tokens[3], line, "Y")?,
                "drill_mm": 0.30,
                "diameter_mm": 0.60,
            });
            apply_kv(&mut args, &tokens[4..], line, &[
                ("drill",    AttrType::NumInto("drill_mm")),
                ("diameter", AttrType::NumInto("diameter_mm")),
            ])?;
            Ok(Cmd { line, tool: "route.add_via".into(), args })
        }
        "delete-trace" => {
            need_args(line, tokens, 1, "delete-trace ID")?;
            Ok(Cmd { line, tool: "route.delete_trace".into(), args: json!({"id": tokens[1]}) })
        }
        "delete-via" => {
            need_args(line, tokens, 1, "delete-via ID")?;
            Ok(Cmd { line, tool: "route.delete_via".into(), args: json!({"id": tokens[1]}) })
        }

        "drc" => {
            let mut args = json!({});
            apply_kv(&mut args, &tokens[1..], line, &[
                ("clearance", AttrType::NumInto("min_clearance_mm")),
                ("edge",      AttrType::NumInto("edge_clearance_mm")),
                ("trace_width", AttrType::NumInto("min_trace_width_mm")),
                ("drill",     AttrType::NumInto("min_drill_mm")),
            ])?;
            Ok(Cmd { line, tool: "drc.run".into(), args })
        }
        "export" => {
            need_args(line, tokens, 1, "export DIR [name=STEM]")?;
            let mut args = json!({"out_dir": tokens[1]});
            apply_kv(&mut args, &tokens[2..], line, &[
                ("name", AttrType::Str),
            ])?;
            Ok(Cmd { line, tool: "output.fab_pack".into(), args })
        }

        other => Err(ParseError::at(line, format!("unknown verb `{other}`"))),
    }
}

// ─── Helpers ──────────────────────────────────────────────────────────

fn need_args(line: usize, tokens: &[String], min_after_verb: usize, usage: &str)
    -> Result<(), ParseError>
{
    if tokens.len() <= min_after_verb {
        Err(ParseError::at(line, format!("expected `{usage}`")))
    } else {
        Ok(())
    }
}

fn parse_num(s: &str, line: usize, label: &str) -> Result<f64, ParseError> {
    s.parse::<f64>()
        .map_err(|_| ParseError::at(line, format!("{label}: expected a number, got `{s}`")))
}

fn expand_side(s: &str, line: usize) -> Result<String, ParseError> {
    Ok(match s.to_ascii_lowercase().as_str() {
        "l" | "left"   => "left".into(),
        "r" | "right"  => "right".into(),
        "t" | "top"    => "top".into(),
        "b" | "bottom" => "bottom".into(),
        other => return Err(ParseError::at(line, format!("side: expected L/R/T/B (or full names), got `{other}`"))),
    })
}

fn expand_kind(s: &str, line: usize) -> Result<String, ParseError> {
    Ok(match s {
        "ic" | "generic_ic"   => "generic_ic".into(),
        "r"  | "resistor"     => "resistor".into(),
        "c"  | "capacitor"    => "capacitor".into(),
        "l"  | "inductor"     => "inductor".into(),
        "led"                 => "led".into(),
        "d"  | "diode"        => "diode".into(),
        other => return Err(ParseError::at(
            line,
            format!("kind: expected ic/r/c/l/led/d (or full names), got `{other}`"),
        )),
    })
}

fn guess_mime(path: &str) -> String {
    let lower = path.to_ascii_lowercase();
    if lower.ends_with(".jpg") || lower.ends_with(".jpeg") { "image/jpeg".into() }
    else if lower.ends_with(".png") { "image/png".into() }
    else if lower.ends_with(".gif") { "image/gif".into() }
    else if lower.ends_with(".webp") { "image/webp".into() }
    else if lower.ends_with(".pdf") { "application/pdf".into() }
    else if lower.ends_with(".txt") { "text/plain".into() }
    else if lower.ends_with(".md") { "text/markdown".into() }
    else { "application/octet-stream".into() }
}

#[derive(Clone, Copy)]
enum AttrType {
    /// Plain string-typed key, stored in args under the same name.
    Str,
    /// String-typed but renamed to a different field in args.
    StrInto(&'static str),
    /// Number-typed key, stored in args under the same name.
    Num,
    /// Number-typed but renamed.
    NumInto(&'static str),
    /// Boolean-typed but renamed.
    BoolInto(&'static str),
    /// Boolean-typed.
    Bool,
}

/// Apply `key=value` tokens to a JSON object. The `allowed` list maps
/// each allowed key to its destination name + type. Unknown keys are
/// rejected so the agent gets feedback rather than silent drops.
fn apply_kv(
    args: &mut Value,
    tokens: &[String],
    line: usize,
    allowed: &[(&str, AttrType)],
) -> Result<(), ParseError> {
    let obj = args.as_object_mut().expect("args must be a JSON object");
    for tok in tokens {
        let Some((k, v)) = tok.split_once('=') else {
            return Err(ParseError::at(line, format!(
                "expected key=value for trailing args, got `{tok}`"
            )));
        };
        let Some((_, ty)) = allowed.iter().find(|(name, _)| *name == k) else {
            let names: Vec<&str> = allowed.iter().map(|(n, _)| *n).collect();
            return Err(ParseError::at(line, format!(
                "unknown attribute `{k}`; allowed: {}",
                names.join(", "),
            )));
        };
        match ty {
            AttrType::Str => { obj.insert(k.into(), Value::String(v.into())); }
            AttrType::StrInto(target) => { obj.insert((*target).into(), Value::String(v.into())); }
            AttrType::Num => {
                let n = parse_num(v, line, k)?;
                obj.insert(k.into(), json!(n));
            }
            AttrType::NumInto(target) => {
                let n = parse_num(v, line, k)?;
                obj.insert((*target).into(), json!(n));
            }
            AttrType::Bool => {
                let b = parse_bool(v, line, k)?;
                obj.insert(k.into(), Value::Bool(b));
            }
            AttrType::BoolInto(target) => {
                let b = parse_bool(v, line, k)?;
                obj.insert((*target).into(), Value::Bool(b));
            }
        }
    }
    Ok(())
}

fn parse_bool(s: &str, line: usize, k: &str) -> Result<bool, ParseError> {
    Ok(match s.to_ascii_lowercase().as_str() {
        "true"  | "1" | "yes" => true,
        "false" | "0" | "no"  => false,
        other => return Err(ParseError::at(line, format!("{k}: expected true/false, got `{other}`"))),
    })
}
