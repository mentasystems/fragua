//! Tiny line-oriented DSL for the local HTTP script API. The agent
//! writes a multi-line program; the parser turns each line into an
//! existing internal tool call which `dispatch` executes in order.
//!
//! Why a DSL: a 30-line JSON `ops` array bloats context and burns
//! tokens. An equivalent script is closer to 30 short lines of plain
//! text, no bracket noise. The agent talks to a single endpoint
//! (`POST /script`) and only needs the verb reference at `GET /`.
//!
//! Grammar in one paragraph: each non-empty, non-comment line is a
//! command — `verb arg1 arg2 ... key=value ...`. Strings with spaces
//! are double-quoted. Indented lines (2 spaces or a tab) extend the
//! previous block-opening command (`sym` / `lib`) with `pin` or `pad`
//! sub-entries. `#` starts a line comment.

use base64::Engine;
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
        Self {
            line,
            message: msg.into(),
        }
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
                            return Err(ParseError::at(
                                line_no,
                                "unterminated escape inside \"..\"",
                            ))
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
    s.trim_end_matches([' ', '\t', '\r'])
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
    /// Library silk lines/texts accumulated from `silk-line` /
    /// `silk-text` indented continuations under a `lib` block.
    silk: Vec<Value>,
}

impl Block {
    fn open(line: usize, verb: String, tokens: Vec<String>) -> Result<Self, ParseError> {
        Ok(Self {
            verb,
            line,
            tokens,
            pins: Vec::new(),
            pads: Vec::new(),
            silk: Vec::new(),
        })
    }

    fn absorb_continuation(&mut self, line: usize, tokens: &[String]) -> Result<(), ParseError> {
        match (self.verb.as_str(), tokens[0].as_str()) {
            ("sym", "pin") => {
                // `pin NUMBER SIDE [NAME] [name=NAME] [role=ROLE]`
                if tokens.len() < 3 {
                    return Err(ParseError::at(
                        line,
                        "pin needs at least: pin NUMBER SIDE [NAME] [role=ROLE]",
                    ));
                }
                let number = tokens[1].clone();
                let side = expand_side(&tokens[2], line)?;
                let mut name = String::new();
                let mut role: Option<String> = None;
                for t in &tokens[3..] {
                    if let Some(rest) = t.strip_prefix("name=") {
                        name = rest.to_string();
                    } else if let Some(rest) = t.strip_prefix("role=") {
                        role = Some(canonical_pin_role(rest, line)?);
                    } else if !t.contains('=') {
                        // Bare third token is the pin name (e.g. `pin 1 L V5`).
                        name.clone_from(t);
                    }
                }
                let mut pin = json!({"number": number, "side": side});
                if !name.is_empty() {
                    pin.as_object_mut()
                        .unwrap()
                        .insert("name".into(), Value::String(name));
                }
                if let Some(r) = role {
                    pin.as_object_mut()
                        .unwrap()
                        .insert("role".into(), Value::String(r));
                }
                self.pins.push(pin);
                Ok(())
            }
            ("lib", "silk-line") => {
                // Library-frame silk segment in footprint-local mm.
                // Reuses the same token shape as the top-level
                // verb so the agent doesn't have to learn two
                // syntaxes.
                let args = parse_silk_line(line, tokens)?;
                let layer_str = args
                    .get("layer")
                    .and_then(Value::as_str)
                    .unwrap_or("top")
                    .to_string();
                let mut entry = json!({
                    "kind": "line",
                    "layer": layer_str,
                    "x1_mm": args["x1_mm"],
                    "y1_mm": args["y1_mm"],
                    "x2_mm": args["x2_mm"],
                    "y2_mm": args["y2_mm"],
                    "width_mm": args["width_mm"],
                });
                if let Some(obj) = entry.as_object_mut() {
                    obj.remove("rotation"); // not relevant for lines
                }
                self.silk.push(entry);
                Ok(())
            }
            ("lib", "silk-text") => {
                let args = parse_silk_text(line, tokens)?;
                let layer_str = args
                    .get("layer")
                    .and_then(Value::as_str)
                    .unwrap_or("top")
                    .to_string();
                let anchor_str = args
                    .get("anchor")
                    .and_then(Value::as_str)
                    .unwrap_or("middle")
                    .to_string();
                let entry = json!({
                    "kind": "text",
                    "layer": layer_str,
                    "x_mm": args["x_mm"],
                    "y_mm": args["y_mm"],
                    "text": args["text"],
                    "size_mm": args["size_mm"],
                    "rotation_deg": args["rotation"],
                    "anchor": anchor_str,
                    "width_mm": args.get("width_mm").cloned().unwrap_or(Value::Null),
                });
                self.silk.push(entry);
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
                    pad.as_object_mut()
                        .unwrap()
                        .insert("name".into(), Value::String(name));
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
                apply_kv(
                    &mut args,
                    &tokens[3..],
                    line,
                    &[
                        ("key", AttrType::Str),
                        ("value", AttrType::Str),
                        ("rot", AttrType::NumInto("rotation")),
                        ("rotation", AttrType::Num),
                        ("x", AttrType::NumInto("x_mm")),
                        ("y", AttrType::NumInto("y_mm")),
                        ("desc", AttrType::StrInto("description")),
                        ("description", AttrType::Str),
                    ],
                )?;
                if kind == "generic_ic" {
                    args.as_object_mut()
                        .unwrap()
                        .insert("pins".into(), Value::Array(self.pins));
                }
                Ok(Cmd {
                    line,
                    tool: "schematic.add_symbol".into(),
                    args,
                })
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
                let mut args = json!({"key": key, "pads": self.pads, "silk": self.silk});
                apply_kv(
                    &mut args,
                    &tokens[2..],
                    line,
                    &[
                        ("value", AttrType::StrInto("default_value")),
                        ("default_value", AttrType::Str),
                        ("rot", AttrType::NumInto("default_rotation_deg")),
                        ("default_rotation_deg", AttrType::Num),
                        ("edge", AttrType::BoolInto("edge_mounted")),
                        ("edge_mounted", AttrType::Bool),
                        ("desc", AttrType::StrInto("description")),
                        ("description", AttrType::Str),
                        ("lcsc", AttrType::StrInto("lcsc_id")),
                        ("lcsc_id", AttrType::Str),
                        ("mpn", AttrType::Str),
                    ],
                )?;
                // `description` is required by library.create — synthesise an empty
                // string if the agent didn't provide one (the tool will reject empty
                // descriptions only if the schema declares them required).
                if !args.as_object().unwrap().contains_key("description") {
                    args.as_object_mut()
                        .unwrap()
                        .insert("description".into(), json!(""));
                }
                Ok(Cmd {
                    line,
                    tool: "library.create".into(),
                    args,
                })
            }
            _ => unreachable!(),
        }
    }
}

// ─── Single-line command compiler ─────────────────────────────────────

fn compile_command(line: usize, tokens: &[String]) -> Result<Cmd, ParseError> {
    let verb = tokens[0].as_str();
    match verb {
        "reset" => Ok(Cmd {
            line,
            tool: "project.reset".into(),
            args: json!({}),
        }),
        "status" => Ok(Cmd {
            line,
            tool: "project.status".into(),
            args: json!({}),
        }),
        "view" => Ok(Cmd {
            line,
            tool: "view.summary".into(),
            args: json!({}),
        }),
        "snap" => Ok(Cmd {
            line,
            tool: "view.snapshot".into(),
            args: json!({}),
        }),
        "sch" => Ok(Cmd {
            line,
            tool: "schematic.snapshot".into(),
            args: json!({}),
        }),
        "sch-status" => Ok(Cmd {
            line,
            tool: "schematic.status".into(),
            args: json!({}),
        }),
        "nets" => Ok(Cmd {
            line,
            tool: "net.status".into(),
            args: json!({}),
        }),
        "list-lib" => Ok(Cmd {
            line,
            tool: "library.list".into(),
            args: json!({}),
        }),
        "list-palette" => Ok(Cmd {
            line,
            tool: "palette.list".into(),
            args: json!({}),
        }),
        "clear-palette" => Ok(Cmd {
            line,
            tool: "palette.clear".into(),
            args: json!({}),
        }),
        "clear-route" => Ok(Cmd {
            line,
            tool: "route.clear".into(),
            args: json!({}),
        }),
        "save" => {
            // save PATH  — write the project JSON to PATH (atomic).
            need_args(line, tokens, 1, "save PATH")?;
            let path = tokens[1..].join(" ");
            Ok(Cmd {
                line,
                tool: "project.save".into(),
                args: json!({"path": path}),
            })
        }
        "screenshot" => {
            // screenshot PATH [view=board|schematic] [width=PX]
            // Rasterises the current board (or schematic) to a PNG on
            // disk. The same content is also served over
            // `GET /screenshot` on the HTTP API; this verb is the
            // inline form so a script can mutate-then-snap in one
            // round trip.
            need_args(
                line,
                tokens,
                1,
                "screenshot PATH [view=board|schematic] [width=PX]",
            )?;
            let mut args = json!({"path": tokens[1]});
            apply_kv(
                &mut args,
                &tokens[2..],
                line,
                &[("view", AttrType::Str), ("width", AttrType::Num)],
            )?;
            Ok(Cmd {
                line,
                tool: "project.screenshot".into(),
                args,
            })
        }
        "keepout" => {
            // keepout add x1,y1 x2,y2 ... [layer=top|bottom|both] [label=NAME]
            // keepout list
            // keepout remove ID
            need_args(line, tokens, 1, "keepout add|list|remove ...")?;
            let sub = tokens[1].as_str();
            match sub {
                "add" => {
                    // Collect coordinate pairs ("x,y") until first kv token.
                    let mut points: Vec<(f64, f64)> = Vec::new();
                    let mut kv_start = tokens.len();
                    for (i, t) in tokens.iter().enumerate().skip(2) {
                        if t.contains('=') {
                            kv_start = i;
                            break;
                        }
                        let (xs, ys) = t.split_once(',').ok_or_else(|| {
                            ParseError::at(
                                line,
                                format!("keepout add: expected `x,y`, got `{t}`"),
                            )
                        })?;
                        let x = parse_num(xs, line, "x")?;
                        let y = parse_num(ys, line, "y")?;
                        points.push((x, y));
                    }
                    if points.len() < 3 {
                        return Err(ParseError::at(
                            line,
                            "keepout add: need at least 3 points (x,y triples)",
                        ));
                    }
                    let pts_json: Vec<Value> =
                        points.into_iter().map(|(x, y)| json!([x, y])).collect();
                    let mut args = json!({"points": pts_json});
                    apply_kv(
                        &mut args,
                        &tokens[kv_start..],
                        line,
                        &[("layer", AttrType::Str), ("label", AttrType::Str)],
                    )?;
                    Ok(Cmd {
                        line,
                        tool: "keepout.add".into(),
                        args,
                    })
                }
                "list" => Ok(Cmd {
                    line,
                    tool: "keepout.list".into(),
                    args: json!({}),
                }),
                "remove" => {
                    need_args(line, tokens, 2, "keepout remove ID")?;
                    Ok(Cmd {
                        line,
                        tool: "keepout.remove".into(),
                        args: json!({"id": tokens[2]}),
                    })
                }
                other => Err(ParseError::at(
                    line,
                    format!("keepout: unknown subcommand `{other}` (add|list|remove)"),
                )),
            }
        }
        "pour" => {
            // Syntaxes:
            //   pour NET LAYER                          — add pour
            //   pour relief NET solid                   — set thermal relief style
            //   pour relief NET spokes [width=N] [gap=N]
            //   pour stitch NET none                    — disable stitching
            //   pour stitch NET grid [pitch=N] [clearance=N]
            need_args(line, tokens, 2, "pour NET LAYER | pour relief NET ... | pour stitch NET ...")?;
            if tokens[1] == "stitch" {
                need_args(
                    line,
                    tokens,
                    3,
                    "pour stitch NET (none|grid [pitch=N] [clearance=N])",
                )?;
                let net = tokens[2].clone();
                let policy = tokens[3].as_str();
                match policy {
                    "none" => Ok(Cmd {
                        line,
                        tool: "pour.stitch".into(),
                        args: json!({"net": net, "policy": "none"}),
                    }),
                    "grid" => {
                        let mut args = json!({"net": net, "policy": "grid"});
                        apply_kv(
                            &mut args,
                            &tokens[4..],
                            line,
                            &[
                                ("pitch", AttrType::NumInto("pitch_mm")),
                                ("clearance", AttrType::NumInto("clearance_mm")),
                            ],
                        )?;
                        Ok(Cmd {
                            line,
                            tool: "pour.stitch".into(),
                            args,
                        })
                    }
                    other => Err(ParseError::at(
                        line,
                        format!("pour stitch: expected none|grid, got `{other}`"),
                    )),
                }
            } else if tokens[1] == "relief" {
                need_args(
                    line,
                    tokens,
                    3,
                    "pour relief NET (solid|spokes [width=N] [gap=N])",
                )?;
                let net = tokens[2].clone();
                let style = tokens[3].as_str();
                match style {
                    "solid" => Ok(Cmd {
                        line,
                        tool: "pour.relief".into(),
                        args: json!({"net": net, "style": "solid"}),
                    }),
                    "spokes" => {
                        let mut args = json!({"net": net, "style": "spokes"});
                        apply_kv(
                            &mut args,
                            &tokens[4..],
                            line,
                            &[
                                ("width", AttrType::NumInto("spoke_width_mm")),
                                ("gap", AttrType::NumInto("gap_mm")),
                            ],
                        )?;
                        Ok(Cmd {
                            line,
                            tool: "pour.relief".into(),
                            args,
                        })
                    }
                    other => Err(ParseError::at(
                        line,
                        format!("pour relief: expected solid|spokes, got `{other}`"),
                    )),
                }
            } else {
                Ok(Cmd {
                    line,
                    tool: "pour.add".into(),
                    args: json!({"net": tokens[1], "layer": tokens[2]}),
                })
            }
        }
        "clear-pour" => {
            // clear-pour NET LAYER
            need_args(line, tokens, 2, "clear-pour NET LAYER")?;
            Ok(Cmd {
                line,
                tool: "pour.remove".into(),
                args: json!({"net": tokens[1], "layer": tokens[2]}),
            })
        }

        "silk-line" => {
            let args = parse_silk_line(line, tokens)?;
            Ok(Cmd {
                line,
                tool: "silk.add_line".into(),
                args,
            })
        }
        "silk-text" => {
            let args = parse_silk_text(line, tokens)?;
            Ok(Cmd {
                line,
                tool: "silk.add_text".into(),
                args,
            })
        }

        "outline" => {
            // outline W H [radius=R]
            need_args(line, tokens, 2, "outline W H [radius=R]")?;
            let w = parse_num(&tokens[1], line, "W")?;
            let h = parse_num(&tokens[2], line, "H")?;
            let mut args = json!({"w_mm": w, "h_mm": h});
            apply_kv(
                &mut args,
                &tokens[3..],
                line,
                &[
                    ("radius", AttrType::NumInto("corner_radius_mm")),
                    ("r", AttrType::NumInto("corner_radius_mm")),
                ],
            )?;
            Ok(Cmd {
                line,
                tool: "board.set_outline".into(),
                args,
            })
        }

        "net" => {
            // net NAME PIN1 [PIN2 ...] [class=NAME]
            need_args(line, tokens, 2, "net NAME PIN1 [PIN2 ...] [class=NAME]")?;
            let name = tokens[1].clone();
            // Positional pins end at the first kv token (`class=...`).
            let mut pins: Vec<Value> = Vec::new();
            let mut kv_start = tokens.len();
            for (i, t) in tokens.iter().enumerate().skip(2) {
                if t.contains('=') {
                    kv_start = i;
                    break;
                }
                pins.push(Value::String(t.clone()));
            }
            let mut args = json!({"net": name, "pins": pins});
            apply_kv(
                &mut args,
                &tokens[kv_start..],
                line,
                &[("class", AttrType::Str)],
            )?;
            Ok(Cmd {
                line,
                tool: "schematic.connect".into(),
                args,
            })
        }
        "class" => {
            // class NAME [width=N] [clearance=N] [via=N] [drill=N] [z0=N]
            //            [pair=NET] [gap=N] [pour=top|bottom|both]
            need_args(
                line,
                tokens,
                1,
                "class NAME [width=N] [clearance=N] [via=N] [drill=N] [z0=N] [pair=NET] [gap=N] [pour=top|bottom|both]",
            )?;
            let mut args = json!({"name": tokens[1]});
            apply_kv(
                &mut args,
                &tokens[2..],
                line,
                &[
                    ("width", AttrType::NumInto("trace_width_mm")),
                    ("clearance", AttrType::NumInto("clearance_mm")),
                    ("via", AttrType::NumInto("via_diameter_mm")),
                    ("drill", AttrType::NumInto("via_drill_mm")),
                    ("z0", AttrType::NumInto("target_impedance_ohms")),
                    ("pair", AttrType::StrInto("diff_pair_with")),
                    ("gap", AttrType::NumInto("diff_gap_mm")),
                    ("pour", AttrType::Str),
                ],
            )?;
            Ok(Cmd {
                line,
                tool: "schematic.set_class".into(),
                args,
            })
        }
        "net-class" => {
            // net-class NET CLASS — bind NET to a previously-declared
            // class. Idempotent: re-binding to a different class wins.
            need_args(line, tokens, 2, "net-class NET CLASS")?;
            Ok(Cmd {
                line,
                tool: "schematic.assign_net_class".into(),
                args: json!({"net": tokens[1], "class": tokens[2]}),
            })
        }
        "auto-pour" => {
            // No args: walk the schematic and materialise pours for
            // every net whose class declares a `pour_layer`.
            Ok(Cmd {
                line,
                tool: "pour.auto".into(),
                args: json!({}),
            })
        }

        "find-lib" => {
            need_args(line, tokens, 1, "find-lib KEY")?;
            Ok(Cmd {
                line,
                tool: "library.find".into(),
                args: json!({"key": tokens[1]}),
            })
        }
        "delete-lib" => {
            need_args(line, tokens, 1, "delete-lib KEY")?;
            Ok(Cmd {
                line,
                tool: "library.delete".into(),
                args: json!({"key": tokens[1]}),
            })
        }
        "detach" => {
            need_args(line, tokens, 2, "detach KEY ATTACHMENT_ID")?;
            Ok(Cmd {
                line,
                tool: "library.delete_attachment".into(),
                args: json!({"key": tokens[1], "attachment_id": tokens[2]}),
            })
        }
        "attach" => {
            // attach KEY KIND PATH [mime=MIME] [filename=NAME]
            need_args(
                line,
                tokens,
                3,
                "attach KEY KIND PATH [mime=...] [filename=...]",
            )?;
            let key = tokens[1].clone();
            let kind = tokens[2].clone();
            let path = tokens[3].clone();
            // Read + base64-encode the file. Mime auto-detected from the
            // path extension unless overridden.
            let bytes = std::fs::read(&path)
                .map_err(|e| ParseError::at(line, format!("attach: cannot read {path}: {e}")))?;
            let data_base64 = base64::engine::general_purpose::STANDARD.encode(&bytes);
            let filename = std::path::Path::new(&path)
                .file_name()
                .and_then(|s| s.to_str())
                .unwrap_or(&path)
                .to_string();
            let mime = guess_mime(&path);
            let mut args = json!({
                "key": key,
                "kind": kind,
                "filename": filename,
                "mime": mime,
                "data_base64": data_base64,
            });
            apply_kv(
                &mut args,
                &tokens[4..],
                line,
                &[("mime", AttrType::Str), ("filename", AttrType::Str)],
            )?;
            Ok(Cmd {
                line,
                tool: "library.attach".into(),
                args,
            })
        }

        "palette" => {
            // palette REF KEY [rot=DEG] [value=V] [layer=top|bottom]
            need_args(line, tokens, 2, "palette REF KEY [rot=...] [value=...]")?;
            let mut args = json!({"reference": tokens[1], "key": tokens[2]});
            apply_kv(
                &mut args,
                &tokens[3..],
                line,
                &[
                    ("rot", AttrType::NumInto("rotation")),
                    ("rotation", AttrType::Num),
                    ("value", AttrType::Str),
                    ("layer", AttrType::Str),
                ],
            )?;
            Ok(Cmd {
                line,
                tool: "palette.add_from_library".into(),
                args,
            })
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
                Ok(Cmd {
                    line,
                    tool: "placement.batch".into(),
                    args: json!({"items": [{
                        "reference": reference, "x_mm": x, "y_mm": y, "rotation": rot
                    }]}),
                })
            } else {
                Ok(Cmd {
                    line,
                    tool: "placement.place_from_palette".into(),
                    args: json!({"reference": reference, "x_mm": x, "y_mm": y}),
                })
            }
        }

        "move" => {
            need_args(line, tokens, 3, "move REF X Y")?;
            Ok(Cmd {
                line,
                tool: "placement.move".into(),
                args: json!({
                    "reference": tokens[1],
                    "x_mm": parse_num(&tokens[2], line, "X")?,
                    "y_mm": parse_num(&tokens[3], line, "Y")?,
                }),
            })
        }
        "rotate" => {
            need_args(line, tokens, 2, "rotate REF DEG")?;
            Ok(Cmd {
                line,
                tool: "placement.rotate".into(),
                args: json!({
                    "reference": tokens[1],
                    "degrees": parse_num(&tokens[2], line, "DEG")?,
                }),
            })
        }
        "delete" => {
            need_args(line, tokens, 1, "delete REF [REF ...]")?;
            // Strict ref-only verb — every token after `delete` is a
            // reference designator. Anything carrying `=` would be a
            // typo (no kv flags supported yet) so reject early to give
            // a clear error rather than treating it as a ref.
            let refs: Vec<String> = tokens[1..].to_vec();
            for r in &refs {
                if r.contains('=') {
                    return Err(ParseError::at(
                        line,
                        format!("delete: unexpected `=` in ref `{r}` (no kv flags supported)"),
                    ));
                }
            }
            Ok(Cmd {
                line,
                tool: "placement.delete".into(),
                args: json!({ "refs": refs }),
            })
        }
        "clear-board" => Ok(Cmd {
            line,
            tool: "placement.clear_board".into(),
            args: json!({}),
        }),

        "route" => {
            // optional kv: trace_width, clearance, via_drill, via_diameter, via_cost, cell, order
            let mut args = json!({});
            apply_kv(
                &mut args,
                &tokens[1..],
                line,
                &[
                    ("trace_width", AttrType::NumInto("trace_width_mm")),
                    ("clearance", AttrType::NumInto("clearance_mm")),
                    ("via_drill", AttrType::NumInto("via_drill_mm")),
                    ("via_diameter", AttrType::NumInto("via_diameter_mm")),
                    ("via_cost", AttrType::Num),
                    ("cell", AttrType::NumInto("cell_mm")),
                    ("order", AttrType::Str),
                ],
            )?;
            Ok(Cmd {
                line,
                tool: "route.run".into(),
                args,
            })
        }
        "clear-net" => {
            need_args(line, tokens, 1, "clear-net NET")?;
            Ok(Cmd {
                line,
                tool: "route.clear_net".into(),
                args: json!({"net": tokens[1]}),
            })
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
            apply_kv(
                &mut args,
                &tokens[7..],
                line,
                &[("width", AttrType::NumInto("width_mm"))],
            )?;
            Ok(Cmd {
                line,
                tool: "route.add_trace".into(),
                args,
            })
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
            apply_kv(
                &mut args,
                &tokens[4..],
                line,
                &[
                    ("drill", AttrType::NumInto("drill_mm")),
                    ("diameter", AttrType::NumInto("diameter_mm")),
                ],
            )?;
            Ok(Cmd {
                line,
                tool: "route.add_via".into(),
                args,
            })
        }
        "delete-trace" => {
            need_args(line, tokens, 1, "delete-trace ID")?;
            Ok(Cmd {
                line,
                tool: "route.delete_trace".into(),
                args: json!({"id": tokens[1]}),
            })
        }
        "delete-via" => {
            need_args(line, tokens, 1, "delete-via ID")?;
            Ok(Cmd {
                line,
                tool: "route.delete_via".into(),
                args: json!({"id": tokens[1]}),
            })
        }

        "auto-place" => {
            // auto-place REF [REF...] [iters=N] [seed=N] [max_step=N] [min_step=N]
            need_args(
                line,
                tokens,
                1,
                "auto-place REF [REF...] [iters=N] [seed=N] [max_step=N]",
            )?;
            // Positional refs end at the first token containing `=`. Refs
            // can't contain `=` (alphanumeric + `.` + `_` only), so this
            // split is unambiguous.
            let mut refs: Vec<String> = Vec::new();
            let mut kv_start = tokens.len();
            for (i, t) in tokens.iter().enumerate().skip(1) {
                if t.contains('=') {
                    kv_start = i;
                    break;
                }
                refs.push(t.clone());
            }
            if refs.is_empty() {
                return Err(ParseError::at(
                    line,
                    "auto-place: at least one footprint reference required".to_string(),
                ));
            }
            let mut args = json!({ "refs": refs });
            apply_kv(
                &mut args,
                &tokens[kv_start..],
                line,
                &[
                    ("iters", AttrType::Num),
                    ("seed", AttrType::Num),
                    ("max_step", AttrType::NumInto("max_step_mm")),
                    ("min_step", AttrType::NumInto("min_step_mm")),
                    ("min_gap", AttrType::NumInto("min_gap_mm")),
                    ("gap_penalty", AttrType::NumInto("gap_penalty_factor")),
                    ("congestion", AttrType::NumInto("congestion_penalty_factor")),
                    ("congestion_res", AttrType::NumInto("congestion_resolution")),
                ],
            )?;
            Ok(Cmd {
                line,
                tool: "placement.auto".into(),
                args,
            })
        }

        "drc" => {
            let mut args = json!({});
            apply_kv(
                &mut args,
                &tokens[1..],
                line,
                &[
                    ("clearance", AttrType::NumInto("min_clearance_mm")),
                    ("edge", AttrType::NumInto("edge_clearance_mm")),
                    ("trace_width", AttrType::NumInto("min_trace_width_mm")),
                    ("drill", AttrType::NumInto("min_drill_mm")),
                ],
            )?;
            Ok(Cmd {
                line,
                tool: "drc.run".into(),
                args,
            })
        }
        "erc" => {
            // No options yet; the report is whatever every rule
            // surfaced. Schema-stable so the agent can rely on it.
            Ok(Cmd {
                line,
                tool: "erc.run".into(),
                args: json!({}),
            })
        }
        "export" => {
            need_args(line, tokens, 1, "export DIR [name=STEM]")?;
            let mut args = json!({"out_dir": tokens[1]});
            apply_kv(&mut args, &tokens[2..], line, &[("name", AttrType::Str)])?;
            Ok(Cmd {
                line,
                tool: "output.fab_pack".into(),
                args,
            })
        }
        "pack" => {
            // pack [fab=jlcpcb|pcbway|generic] [out=PATH]
            // Default: fab=jlcpcb, out=~/Downloads.
            let mut args = json!({});
            apply_kv(
                &mut args,
                &tokens[1..],
                line,
                &[
                    ("fab", AttrType::Str),
                    ("out", AttrType::StrInto("out_dir")),
                ],
            )?;
            Ok(Cmd {
                line,
                tool: "fab.pack".into(),
                args,
            })
        }

        other => Err(ParseError::at(line, format!("unknown verb `{other}`"))),
    }
}

// ─── Silk-line / silk-text token parsers ─────────────────────────────
// Shared by both the top-level verbs (board-level silk) and the
// `lib` block continuations (library-authored, footprint-local silk).

fn parse_silk_line(line: usize, tokens: &[String]) -> Result<Value, ParseError> {
    // silk-line LAYER X1 Y1 X2 Y2 [width=0.15]
    need_args(line, tokens, 5, "silk-line LAYER X1 Y1 X2 Y2 [width=...]")?;
    let mut args = json!({
        "layer": tokens[1],
        "x1_mm": parse_num(&tokens[2], line, "X1")?,
        "y1_mm": parse_num(&tokens[3], line, "Y1")?,
        "x2_mm": parse_num(&tokens[4], line, "X2")?,
        "y2_mm": parse_num(&tokens[5], line, "Y2")?,
        "width_mm": 0.15,
    });
    apply_kv(
        &mut args,
        &tokens[6..],
        line,
        &[("width", AttrType::NumInto("width_mm"))],
    )?;
    Ok(args)
}

fn parse_silk_text(line: usize, tokens: &[String]) -> Result<Value, ParseError> {
    // silk-text LAYER X Y "TEXT" [size=1.2] [rot=0] [anchor=middle] [width=...]
    need_args(
        line,
        tokens,
        4,
        "silk-text LAYER X Y TEXT [size=...] [rot=...] [anchor=start|middle|end] [width=...]",
    )?;
    let mut args = json!({
        "layer": tokens[1],
        "x_mm": parse_num(&tokens[2], line, "X")?,
        "y_mm": parse_num(&tokens[3], line, "Y")?,
        "text": tokens[4],
        "size_mm": 1.2,
        "rotation": 0.0,
        "anchor": "middle",
    });
    apply_kv(
        &mut args,
        &tokens[5..],
        line,
        &[
            ("size", AttrType::NumInto("size_mm")),
            ("rot", AttrType::NumInto("rotation")),
            ("anchor", AttrType::Str),
            ("width", AttrType::NumInto("width_mm")),
        ],
    )?;
    Ok(args)
}

// ─── Helpers ──────────────────────────────────────────────────────────

fn need_args(
    line: usize,
    tokens: &[String],
    min_after_verb: usize,
    usage: &str,
) -> Result<(), ParseError> {
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
        "l" | "left" => "left".into(),
        "r" | "right" => "right".into(),
        "t" | "top" => "top".into(),
        "b" | "bottom" => "bottom".into(),
        other => {
            return Err(ParseError::at(
                line,
                format!("side: expected L/R/T/B (or full names), got `{other}`"),
            ))
        }
    })
}

/// Canonical `PinRole` name (`snake_case`) the JSON layer expects, with
/// short aliases ("in", "out", "pwr", "`pwr_in`") so the agent can be
/// terse in long pin lists.
fn canonical_pin_role(s: &str, line: usize) -> Result<String, ParseError> {
    Ok(match s.to_ascii_lowercase().as_str() {
        "passive" | "p" => "passive".into(),
        "input" | "in" => "input".into(),
        "output" | "out" => "output".into(),
        "bidir" | "io" => "bidir".into(),
        "power" | "power_out" | "pwr" | "pwr_out" => "power_out".into(),
        "power_in" | "pwr_in" => "power_in".into(),
        other => {
            return Err(ParseError::at(
                line,
                format!(
                    "role: expected passive/input/output/bidir/power_out/power_in, got `{other}`"
                ),
            ))
        }
    })
}

fn expand_kind(s: &str, line: usize) -> Result<String, ParseError> {
    Ok(match s {
        "ic" | "generic_ic" => "generic_ic".into(),
        "r" | "resistor" => "resistor".into(),
        "c" | "capacitor" => "capacitor".into(),
        "l" | "inductor" => "inductor".into(),
        "led" => "led".into(),
        "d" | "diode" => "diode".into(),
        other => {
            return Err(ParseError::at(
                line,
                format!("kind: expected ic/r/c/l/led/d (or full names), got `{other}`"),
            ))
        }
    })
}

fn guess_mime(path: &str) -> String {
    let lower = path.to_ascii_lowercase();
    if lower.ends_with(".jpg") || lower.ends_with(".jpeg") {
        "image/jpeg".into()
    } else if lower.ends_with(".png") {
        "image/png".into()
    } else if lower.ends_with(".gif") {
        "image/gif".into()
    } else if lower.ends_with(".webp") {
        "image/webp".into()
    } else if lower.ends_with(".pdf") {
        "application/pdf".into()
    } else if lower.ends_with(".txt") {
        "text/plain".into()
    } else if lower.ends_with(".md") {
        "text/markdown".into()
    } else {
        "application/octet-stream".into()
    }
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
            return Err(ParseError::at(
                line,
                format!("expected key=value for trailing args, got `{tok}`"),
            ));
        };
        let Some((_, ty)) = allowed.iter().find(|(name, _)| *name == k) else {
            let names: Vec<&str> = allowed.iter().map(|(n, _)| *n).collect();
            return Err(ParseError::at(
                line,
                format!("unknown attribute `{k}`; allowed: {}", names.join(", "),),
            ));
        };
        match ty {
            AttrType::Str => {
                obj.insert(k.into(), Value::String(v.into()));
            }
            AttrType::StrInto(target) => {
                obj.insert((*target).into(), Value::String(v.into()));
            }
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
        "true" | "1" | "yes" => true,
        "false" | "0" | "no" => false,
        other => {
            return Err(ParseError::at(
                line,
                format!("{k}: expected true/false, got `{other}`"),
            ))
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lib_block_with_silk_lines_and_text_emits_silk_array() {
        let script = "lib test_dip\n  pad 1 -2.5 0 1.0 1.5\n  pad 2 2.5 0 1.0 1.5\n  silk-line top -3 -2 3 -2\n  silk-line top 3 -2 3 2\n  silk-text top 0 0 \"{REF}\" size=1.0 anchor=middle\n";
        let cmds = parse(script).expect("parse");
        assert_eq!(cmds.len(), 1);
        let cmd = &cmds[0];
        assert_eq!(cmd.tool, "library.create");
        let silk = cmd
            .args
            .get("silk")
            .expect("silk array")
            .as_array()
            .expect("array");
        assert_eq!(silk.len(), 3, "expected two lines + one text, got {silk:?}");
        // Two lines first.
        assert_eq!(silk[0]["kind"], "line");
        assert_eq!(silk[0]["layer"], "top");
        assert!((silk[0]["x1_mm"].as_f64().unwrap() - -3.0).abs() < 1e-6);
        // Text last, with the {REF} placeholder preserved.
        assert_eq!(silk[2]["kind"], "text");
        assert_eq!(silk[2]["text"], "{REF}");
        assert_eq!(silk[2]["anchor"], "middle");
        assert!((silk[2]["size_mm"].as_f64().unwrap() - 1.0).abs() < 1e-6);
    }

    #[test]
    fn lib_block_without_silk_keeps_empty_array() {
        let script = "lib plain\n  pad 1 0 0 1 1\n";
        let cmds = parse(script).expect("parse");
        let silk = cmds[0].args.get("silk").unwrap().as_array().unwrap();
        assert!(silk.is_empty());
    }
}
