//! Tool surface — what the agent calls (over the local HTTP API).
//!
//! Each tool is intentionally thin: parse the input, call into
//! `pcb-core` to mutate the project, return the result. The agent owns
//! all the design reasoning; tools are pure data primitives.

use std::collections::HashMap;

use pcb_core::schematic::{Net, NetConnection, PinSide, SchPin, Symbol, SymbolKind};
use pcb_core::{
    ActivityLevel, CopperLayer, Footprint, FootprintSilk, Length, LibrarySilk, Pad, Point, Pour,
    Project, SilkAnchor, SilkLayer, SilkLine, SilkText, Trace, Via,
};
use serde::Deserialize;
use serde_json::{json, Value};

// Internal error markers (kept compatible with JSON-RPC numeric codes
// so callers that already understood them keep working).
pub mod error_code {
    pub const METHOD_NOT_FOUND: i64 = -32601;
    pub const INVALID_PARAMS: i64 = -32602;
    pub const INTERNAL_ERROR: i64 = -32603;
}

use error_code::INVALID_PARAMS;

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

/// Snapshot the library-key → placement-margin lookup the renderer and
/// DRC consume. Cheap: one pass over the library entries; library
/// reads are RwLock-shared so concurrent tools don't block each other.
/// Keys with all-zero margins are omitted so the renderer can skip the
/// outline draw cheaply via `HashMap::get`.
fn build_placement_margin_map(project: &Project) -> pcb_render::PlacementMarginMap {
    let mut out = pcb_render::PlacementMarginMap::default();
    for entry in project.library().list() {
        if entry.placement_margin.is_zero() {
            continue;
        }
        out.insert(entry.key, entry.placement_margin);
    }
    out
}

/// `HashMap<String, PlacementMargin>` flavour suitable for DRC, which
/// keeps its own copy in `DrcOptions`. Mirrors
/// `build_placement_margin_map` so both consumers see the same set of
/// margins.
fn build_drc_margin_map(
    project: &Project,
) -> std::collections::HashMap<String, pcb_core::PlacementMargin> {
    let mut out = std::collections::HashMap::new();
    for entry in project.library().list() {
        if entry.placement_margin.is_zero() {
            continue;
        }
        out.insert(entry.key, entry.placement_margin);
    }
    out
}

/// Reference for the `script` action language — every verb the agent
/// can put on a line. Served verbatim at `GET /` of the local API.
pub const SCRIPT_REFERENCE: &str = "Run a multi-line PCB design script — the ONLY surface you need. \
The script is plain text, one action per line; multi-line blocks (`sym`, `lib`) \
take indented sub-lines (`pin`, `pad`). Strings with spaces use double quotes; \
trailing key=value pairs override defaults; `#` starts a comment.\n\
\n\
=== EXAMPLE ===\
reset\n\
outline 90 30 radius=2                    # rounded corners\n\
\n\
class ground pour=both                    # GND on top + bottom plane\n\
class power width=0.4                     # +3V3 with wider traces\n\
\n\
sym U1 ic key=esp32_s3_zero desc=\"ESP32 main MCU; USB-C edge\"\n\
  pin 1 L V5     role=power_in\n\
  pin 2 L GND    role=power_in\n\
  pin 3 L 3V3    role=power_in\n\
  pin 4 L TX     role=output\n\
sym C1 capacitor key=c_0603 value=100nF desc=\"HF decoupling near U1.3V3\"\n\
\n\
net GND  U1.GND U2.GND_1 C1.2 class=ground\n\
net +3V3 U1.3V3 U2.VCC   C1.1 class=power\n\
\n\
erc                                       # catch netlist bugs early\n\
\n\
palette U1 esp32_s3_zero rot=90\n\
palette C1 c_0603 value=100nF\n\
place U1 11.5 15 90\n\
place C1 48 14\n\
auto-place C1 seed=42                     # SA on the parts you don't pin\n\
route                                     # auto-pour materialises ground first\n\
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
  save PATH                                    — write the project to PATH (atomic .tmp+rename).\n\
                                                 Use this when fragua was launched with no file\n\
                                                 argument (no autosave); afterwards re-launch with\n\
                                                 `fragua PATH` to keep autosaving.\n\
  screenshot PATH [view=board|schematic]       — rasterise the current project to a PNG on disk.\n\
            [width=PX]                           Same content the webview shows (board SVG or\n\
                                                 schematic SVG), rendered headlessly via resvg —\n\
                                                 no OS permission needed. Default view=board,\n\
                                                 width=1600 (max 8192). Also exposed as\n\
                                                 `GET /screenshot[?view=...&width=...]` returning\n\
                                                 `image/png` for direct curl.\n\
\n\
BOARD:\n\
  outline W H [radius=R]                       — set Edge.Cuts rectangle in mm. Optional uniform\n\
                                                 corner radius (mm) rounds all four corners; default\n\
                                                 0 = sharp. Clamped to min(W, H) / 2.\n\
\n\
LIBRARY (build first, reuse forever):\n\
  lib KEY [value=V] [rot=N] [edge=true|false] [desc=\"...\"] [lcsc=Cxxxx] [mpn=...]\n\
    pad NUMBER X Y W H [name=NAME]             — repeat for every pad\n\
    # `lcsc` = LCSC catalogue ID (e.g. C25804 for 10k 0603). Required\n\
    #   for JLCPCB SMT assembly to know what part to load. Optional\n\
    #   but strongly recommended once the part is real.\n\
    # `mpn` = manufacturer part number (e.g. RC0603FR-0710KL). Carries\n\
    #   into the BOM as a fallback when no `lcsc` is set.\n\
    silk-line LAYER X1 Y1 X2 Y2 [width=N]      — body outline / pin-1 marker, in footprint-local mm.\n\
                                                 Same syntax as the top-level verb, but coords are\n\
                                                 relative to the footprint origin and follow it when\n\
                                                 placed/rotated.\n\
    silk-text LAYER X Y \"TEXT\" [size=N] [rot=N] [anchor=start|middle|end] [width=N]\n\
                                               — footprint-local text. Use \"{REF}\" / \"{VAL}\" to\n\
                                                 emit the placed instance's reference / value\n\
                                                 (e.g. one library entry can ship `silk-text top 0 3 \"{REF}\"`\n\
                                                 and every spawn renders \"U1\", \"U2\", ...).\n\
  attach KEY KIND PATH                         — file from disk; mime auto-detected\n\
                                                 KIND is free text: photo / datasheet / note / ...\n\
  detach KEY ATTACHMENT_ID\n\
  delete-lib KEY\n\
  find-lib KEY                                 — full record + pads + silk\n\
  # Library example with body outline + pin 1 dot + auto-ref label:\n\
  #   lib so8 desc=\"SO-8 IC\"\n\
  #     pad 1 -1.905 1.27 1.55 0.6\n\
  #     pad 2 -1.905 0    1.55 0.6\n\
  #     ...\n\
  #     silk-line top -2.5 -2.5  2.5 -2.5\n\
  #     silk-line top  2.5 -2.5  2.5  2.5\n\
  #     silk-line top  2.5  2.5 -2.5  2.5\n\
  #     silk-line top -2.5  2.5 -2.5 -2.5\n\
  #     silk-text top -2.0  2.0 \"*\" size=0.6   # pin-1 dot\n\
  #     silk-text top  0    3.5 \"{REF}\" size=1.0\n\
\n\
SCHEMATIC:\n\
  sym REF KIND [key=K] [value=V] [rot=N] [x=N] [y=N] [desc=\"...\"]\n\
    pin NUMBER SIDE [NAME] [role=ROLE]         — only for KIND=ic; SIDE = L|R|T|B (or full names).\n\
                                                 ROLE = passive (default) | input (in) | output (out)\n\
                                                 | bidir (io) | power_out (power, pwr) | power_in (pwr_in).\n\
                                                 ERC uses roles to catch shorts the geometry can't: 2+\n\
                                                 outputs on one net = error, PowerIn pins on a net with no\n\
                                                 PowerOut source = warning, Input pin with no driver = warning.\n\
                                                 Discretes (R, C, L, LED, D) are always passive, no need to set.\n\
                                                 KIND aliases: ic, r, c, l, led, d\n\
  net NAME PIN1 PIN2 ... [class=NAME]          — PIN = REF.PIN_NUMBER or REF.PIN_NAME (case-insensitive).\n\
                                                 `class` attaches a net class (see below) so the\n\
                                                 router/DRC use its trace_width / clearance for\n\
                                                 this net.\n\
  class NAME [width=N] [clearance=N] [pour=top|bottom|both]\n\
                                               — declare or replace a net class. Set on a net via\n\
                                                 `net NAME ... class=NAME`. Unset fields fall back\n\
                                                 to the route/drc defaults at the call site. `pour`\n\
                                                 makes every net in the class ride a copper pour on\n\
                                                 the chosen layer(s) instead of routed traces; the\n\
                                                 router skips those nets. `pour=both` is the\n\
                                                 standard GND-on-both-layers pattern that connects\n\
                                                 same-net pads regardless of which side they sit on.\n\
\n\
PALETTE / PLACEMENT:\n\
  palette REF KEY [rot=N] [value=V] [layer=top|bottom]\n\
                                               — spawn a palette item from a library entry; the\n\
                                                 schematic must already have a symbol with REF.\n\
                                                 The entry's `footprint_view_transform` (set via the\n\
                                                 review pane's flip/rotate buttons) is baked into the\n\
                                                 spawned pad geometry and silk: the native library\n\
                                                 data stays untouched in index.json, but the placed\n\
                                                 footprint matches what the user saw in the review.\n\
                                                 The optional `rot=` is then layered on top of that\n\
                                                 view transform, same as `place X Y ROT`.\n\
  clear-palette\n\
  place REF X Y [ROT_DEG]                      — drop palette item at (x, y) mm; rejects if it\n\
                                                 overlaps another footprint or violates the\n\
                                                 edge_mounted constraint\n\
  move REF X Y\n\
  rotate REF DEG                               — absolute rotation, multiples of 90 recommended\n\
  delete REF [REF ...]                         — remove placed footprint(s) by ref; also drops every\n\
                                                 trace / via whose endpoint landed on one of their\n\
                                                 pads. Errors if any REF is not on the board; warns\n\
                                                 (in the reply) for nets that lose their last pad.\n\
  clear-board                                  — drop every placed footprint AND all routing;\n\
                                                 outline / silk / schematic / library are kept.\n\
                                                 Useful after editing a library entry: clear-board\n\
                                                 then re-spawn from the palette to pick up the\n\
                                                 updated geometry.\n\
  auto-place REF [REF...] [iters=N] [seed=N] [max_step=N] [min_step=N] [min_gap=N] [gap_penalty=N] [congestion=N] [congestion_res=N]\n\
                                               — simulated-annealing placer over the listed refs.\n\
                                                 Pinned refs (everything not listed) stay put.\n\
                                                 Optimises HPWL + a soft body-to-body gap penalty\n\
                                                 + a congestion proxy (how many net pad-bboxes\n\
                                                 share the same routing cell). Obeys outline +\n\
                                                 edge_mounted; hard-rejects pad overlap. Defaults:\n\
                                                 iters=8000 (~3 s for ~20 components), seed=clock,\n\
                                                 max_step=20 mm, min_gap=2.0 mm, gap_penalty=16,\n\
                                                 congestion=1, congestion_res=32. Bump congestion\n\
                                                 if SA produces tight HPWL but the router struggles;\n\
                                                 set congestion_res=0 to disable the proxy.\n\
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
  auto-pour                                    — materialise a `Pour` for every net whose class has\n\
                                                 `pour=...` set (run implicitly by `route` too).\n\
  pour NET top|bottom                          — declare a copper pour (ground/power plane); pads\n\
                                                 of NET on that layer count as connected without\n\
                                                 a routed trace. Cross-layer pads still need a via.\n\
                                                 Drop a `pour GND bottom` early on dense boards so\n\
                                                 the router does not have to thread GND everywhere.\n\
  clear-pour NET top|bottom                    — remove a pour\n\
\n\
SILK:\n\
  Two scopes: BOARD-level silk lives directly on the board (frames,\n\
  version markings, logos, fiducial labels) — use the top-level verbs\n\
  below. FOOTPRINT-level silk lives on a library entry and follows the\n\
  spawned footprint when it moves/rotates — author it under a `lib`\n\
  block (see LIBRARY above).\n\
  silk-line LAYER X1 Y1 X2 Y2 [width=N]        — silkscreen segment in mm; LAYER = top|bottom;\n\
                                                 default width 0.15 mm.\n\
  silk-text LAYER X Y \"TEXT\" [size=1.2] [rot=0] [anchor=start|middle|end] [width=...]\n\
                                               — silkscreen text vectorised through the built-in\n\
                                                 stroke font. ASCII printable only; default size\n\
                                                 1.2 mm cap height, default stroke ~size/8.\n\
\n\
VALIDATION / EXPORT:\n\
  erc                                          — electrical rules check (schematic side):\n\
                                                 floating pin/net, duplicate pin assignment,\n\
                                                 empty net, orphan symbol, phantom net (board\n\
                                                 pad on a net the schematic doesn't declare).\n\
                                                 Run before placement to catch netlist bugs early.\n\
  drc [clearance=N] [edge=N] [trace_width=N] [drill=N]\n\
                                               — design rules check (board side); defaults\n\
                                                 clearance=0.20, edge=0.30, trace_width=0.10,\n\
                                                 drill=0.20\n\
  export DIR [name=STEM]                       — write the raw fab outputs (gerbers + drill + BOM +\n\
                                                 CPL) to a directory in KiCad-default format. Use\n\
                                                 `pack` instead when you want a ready-to-upload zip.\n\
  pack [fab=jlcpcb|pcbway|generic] [out=DIR]   — run ERC + DRC + manufacturing-DRC, generate every\n\
                                                 fab artefact, format the BOM and CPL for the chosen\n\
                                                 provider, and zip the lot ready to upload. Defaults\n\
                                                 fab=jlcpcb, out=~/Downloads. The result is a single\n\
                                                 `<project>-<fab>.zip` plus a README.txt inside it\n\
                                                 explaining which file is which and how to order.\n\
                                                 If any check fires errors, the zip is still written\n\
                                                 (so you can see the partial output) but the reply\n\
                                                 says NOT READY and lists the blockers.\n\
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
The pipeline runs ERC → placement → routing → DRC. Each layer catches\n\
a different bug class; running them in order saves cycles vs jumping\n\
straight to `route` and debugging the failures.\n\
\n\
1. `erc` — schematic-side validation (after `sym`/`net`, before `place`).\n\
   Catches floating pin/net, duplicate pin, empty net, orphan symbol,\n\
   and (with `pin ... role=...` set) multiple drivers, unpowered\n\
   power nets, undriven inputs. Fix Errors before continuing.\n\
\n\
2. Power planes — declare BEFORE routing, not after:\n\
     class ground pour=both\n\
     net GND ... class=ground\n\
   `pour=both` lays a GND plane on top + bottom; the router skips\n\
   the net entirely (every GND pad connects through the pour) and\n\
   ERC won't fire UnpoweredPowerNet because the pour counts as a\n\
   source. Drops ~15-25 % of total wire on a typical design.\n\
   `class power width=0.4 clearance=0.3` for +3V3/+5V if you want\n\
   wider rails — the router and DRC honour it per net.\n\
\n\
3. `auto-place REF1 REF2 ...` — when you have a rough placement and\n\
   want SA to optimise. Score = HPWL + gap penalty + congestion\n\
   proxy. The default `min_gap=2` keeps parts far enough apart that\n\
   the router has corridors. Reproducible with `seed=N`.\n\
\n\
4. `route` runs the auto-router (RR&R + negotiated congestion +\n\
   Steiner-style multi-source A*), then runs DRC inline. Its output\n\
   includes per-net detour ratio (actual / HPWL) and `route.hint`\n\
   warnings naming the outlier component on every detoured/failed\n\
   net — that's the part to move next.\n\
\n\
When `route` still reports failures or hint warnings:\n\
  a. Read the hints — each one names the outlier component and its\n\
     coords. Move that part toward the rest of its net.\n\
  b. `auto-place <outlier_ref>` if you can't decide where; SA will\n\
     pull it in.\n\
  c. `clear-route` + `route` again. Loop until 0 hints.\n\
\n\
Hand-routing (`trace`, `via`) only works for short bridges in\n\
known-empty zones; on a populated board you will almost always hit\n\
`trace_trace_clearance` errors. Re-place first, route second.\n\
\n\
Rounded boards: declare `outline W H radius=R` BEFORE placing\n\
components. The router's region inset accounts for the corner\n\
curve; placing parts on sharp corners and then adding `radius`\n\
later will leave them outside the routable region (DRC catches it,\n\
but it's wasted work).
";

#[must_use]
pub fn script_reference() -> &'static str {
    SCRIPT_REFERENCE
}

/// Dispatch a `tools/call` to the right handler.
pub async fn dispatch(project: &Project, name: &str, args: &Value) -> Result<Value, ToolError> {
    match name {
        "script" => tool_script(project, args).await,
        "batch" => tool_batch(project, args).await,
        "project.status" => tool_project_status(project),
        "project.reset" => tool_project_reset(project),
        "project.save" => tool_project_save(project, args),
        "project.screenshot" => tool_screenshot(project, args),
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
        "placement.delete" => tool_placement_delete(project, args),
        "placement.clear_board" => tool_placement_clear_board(project),
        "route.clear_net" => tool_route_clear_net(project, args),
        "route.delete_trace" => tool_route_delete_trace(project, args),
        "route.delete_via" => tool_route_delete_via(project, args),
        "route.add_trace" => tool_route_add_trace(project, args),
        "route.add_via" => tool_route_add_via(project, args),
        "route.clear" => tool_route_clear(project),
        "placement.auto" => tool_placement_auto(project, args),
        "route.run" => tool_route_run(project, args),
        "pour.add" => tool_pour_add(project, args),
        "pour.remove" => tool_pour_remove(project, args),
        "pour.relief" => tool_pour_relief(project, args),
        "pour.stitch" => tool_pour_stitch(project, args),
        "keepout.add" => tool_keepout_add(project, args),
        "keepout.list" => tool_keepout_list(project),
        "keepout.remove" => tool_keepout_remove(project, args),
        "silk.add_line" => tool_silk_add_line(project, args),
        "silk.add_text" => tool_silk_add_text(project, args),
        "drc.run" => tool_drc_run(project, args),
        "erc.run" => tool_erc_run(project, args),
        "fab.pack" => tool_fab_pack(project, args),
        "schematic.set_class" => tool_schematic_set_class(project, args),
        "schematic.assign_net_class" => tool_schematic_assign_net_class(project, args),
        "pour.auto" => tool_auto_pour(project, args),
        "output.fab_pack" => tool_output_fab_pack(project, args),
        _ => Err(ToolError {
            code: error_code::METHOD_NOT_FOUND,
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
struct SaveInput {
    path: String,
}

/// Write the current project to an arbitrary path. Useful when the app
/// was launched without a file argument (no autosave): the agent runs a
/// `save /path/to/board.fragua` line once it has something worth keeping.
fn tool_project_save(project: &Project, args: &Value) -> Result<Value, ToolError> {
    let input: SaveInput = serde_json::from_value(args.clone())
        .map_err(|e| ToolError::invalid_params(format!("save: {e}")))?;
    if input.path.trim().is_empty() {
        return Err(ToolError::invalid_params("save: path is empty"));
    }
    let path = std::path::PathBuf::from(&input.path);
    let written = project
        .save_to_path(&path)
        .map_err(|e| ToolError::invalid_params(format!("save: {e}")))?;
    project.log(
        ActivityLevel::Info,
        format!("project.save: wrote {}", written.display()),
    );
    Ok(text_result(format!("Saved to {}", written.display())).into())
}

#[derive(Debug, Deserialize)]
struct ScreenshotInput {
    /// Where to write the PNG. Created/truncated; parent dirs must
    /// already exist.
    path: String,
    /// Which surface to render: `board` (default) or `schematic`.
    #[serde(default)]
    view: Option<String>,
    /// Image width in pixels (height follows the SVG aspect ratio).
    /// Defaults to `pcb_render::DEFAULT_PNG_WIDTH`. Accepted as a
    /// number so the script-DSL `width=2000` (parsed as f64) round-trips
    /// cleanly without needing an integer-typed `AttrType`.
    #[serde(default)]
    width: Option<f64>,
}

/// Rasterise the current project to a PNG file on disk. This is the
/// script-side counterpart to `GET /screenshot` on the HTTP API — the
/// agent uses it inline (`screenshot path=/tmp/x.png`) so a single
/// script run can mutate the board, screenshot it, then keep going.
fn tool_screenshot(project: &Project, args: &Value) -> Result<Value, ToolError> {
    let input: ScreenshotInput = serde_json::from_value(args.clone())
        .map_err(|e| ToolError::invalid_params(format!("screenshot: {e}")))?;
    if input.path.trim().is_empty() {
        return Err(ToolError::invalid_params("screenshot: path is empty"));
    }
    let view = input.view.as_deref().unwrap_or("board");
    let width = input.width.map_or(pcb_render::DEFAULT_PNG_WIDTH, |w| {
        w.round().clamp(1.0, f64::from(pcb_render::MAX_PNG_DIMENSION)) as u32
    });

    let snap = project.read();
    let margins = build_placement_margin_map(project);
    let png_result = match view {
        "board" => pcb_render::render_board_png_with_margins(snap.board(), &margins, width),
        "schematic" | "sch" => pcb_render::render_schematic_png(snap.schematic(), width),
        other => {
            return Err(ToolError::invalid_params(format!(
                "screenshot: unknown view `{other}` (use `board` or `schematic`)"
            )));
        }
    };
    drop(snap);
    let png = png_result
        .map_err(|e| ToolError::invalid_params(format!("screenshot: render: {e}")))?;

    let path = std::path::PathBuf::from(&input.path);
    std::fs::write(&path, &png).map_err(|e| {
        ToolError::invalid_params(format!("screenshot: write {}: {e}", path.display()))
    })?;
    project.log(
        ActivityLevel::Info,
        format!(
            "screenshot: wrote {} ({} bytes, view={view}, width={width})",
            path.display(),
            png.len()
        ),
    );
    Ok(text_result(format!(
        "Wrote {view} screenshot ({bytes} bytes) to {p}",
        bytes = png.len(),
        p = path.display()
    ))
    .into())
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

/// Run many tool calls sequentially in a single API request. Each op
/// is `{tool, args}`; the result mirrors the per-op outcome so the
/// agent can react granularly. `batch` itself is rejected as an op
/// (no nesting).
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
    Ok(
        text_result(format!("script: {ok_count} ok, {fail_count} failed")).with_data(json!({
            "ok_count": ok_count,
            "fail_count": fail_count,
            "results": results,
        })),
    )
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
    Ok(
        text_result(format!("batch: {ok_count} ok, {fail_count} failed")).with_data(json!({
            "ok_count": ok_count,
            "fail_count": fail_count,
            "results": results,
        })),
    )
}

#[derive(Debug, Deserialize)]
struct SetOutlineInput {
    w_mm: f64,
    h_mm: f64,
    /// Optional uniform corner radius in mm (default 0 = sharp).
    /// Clamped by `Project::set_outline_with_radius` to half the
    /// shorter side, so even a comically-large value produces a
    /// valid outline (a stadium / pill shape at the limit).
    #[serde(default)]
    corner_radius_mm: Option<f64>,
}

fn tool_board_set_outline(project: &Project, args: &Value) -> Result<Value, ToolError> {
    let input: SetOutlineInput = serde_json::from_value(args.clone())
        .map_err(|e| ToolError::invalid_params(format!("board.set_outline: {e}")))?;
    let outline = pcb_core::Rect::from_corners(
        Point::new(Length::from_mm(0.0), Length::from_mm(0.0)),
        Point::new(Length::from_mm(input.w_mm), Length::from_mm(input.h_mm)),
    );
    let radius = Length::from_mm(input.corner_radius_mm.unwrap_or(0.0).max(0.0));
    project.set_outline_with_radius(outline, radius);
    let radius_mm = radius.to_mm();
    project.log(
        ActivityLevel::Info,
        format!(
            "board.set_outline: {:.1} × {:.1} mm{}",
            input.w_mm,
            input.h_mm,
            if radius_mm > 0.0 {
                format!(" (radius {radius_mm:.2} mm)")
            } else {
                String::new()
            },
        ),
    );
    let mut text = format!(
        "Board outline set to {:.1} × {:.1} mm",
        input.w_mm, input.h_mm
    );
    if radius_mm > 0.0 {
        text.push_str(&format!(", corner radius {radius_mm:.2} mm"));
    }
    Ok(text_result(text).with_data(json!({
        "w_mm": input.w_mm,
        "h_mm": input.h_mm,
        "corner_radius_mm": radius_mm,
    })))
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
    /// Plated through-hole drill diameter in mm. Omit for a pure SMD
    /// pad. Set to make a perforated (hybrid SMD + PTH) pad.
    #[serde(default)]
    drill_mm: Option<f64>,
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

impl From<LayerInput> for SilkLayer {
    fn from(value: LayerInput) -> Self {
        match value {
            LayerInput::Top => Self::Top,
            LayerInput::Bottom => Self::Bottom,
        }
    }
}

#[derive(Debug, Deserialize, Clone, Copy)]
#[serde(rename_all = "lowercase")]
enum AnchorInput {
    Start,
    Middle,
    End,
}

impl From<AnchorInput> for SilkAnchor {
    fn from(value: AnchorInput) -> Self {
        match value {
            AnchorInput::Start => Self::Start,
            AnchorInput::Middle => Self::Middle,
            AnchorInput::End => Self::End,
        }
    }
}

fn default_anchor_middle() -> AnchorInput {
    AnchorInput::Middle
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
            drill: p.drill_mm.map(Length::from_mm),
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
        silk: Vec::new(),
    };

    let id = project.add_footprint(footprint);
    project.log(
        ActivityLevel::Info,
        format!(
            "placement.add: {} at ({:.2}, {:.2}) mm",
            input.reference, input.x_mm, input.y_mm
        ),
    );
    Ok(
        text_result(format!("Placed {} ({})", input.reference, id.0))
            .with_data(json!({ "id": id.0.to_string(), "reference": input.reference })),
    )
}

fn tool_view_snapshot(project: &Project) -> Result<Value, ToolError> {
    let margins = build_placement_margin_map(project);
    let snap = project.read();
    let board = snap.board();
    let svg = pcb_render::render_svg_with_margins(board, &margins);

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
        .map(|v| {
            json!({
                "id": v.id.0.to_string(),
                "net": v.net,
                "x_mm": v.position.x.to_mm(),
                "y_mm": v.position.y.to_mm(),
                "drill_mm": v.drill.to_mm(),
                "diameter_mm": v.diameter.to_mm(),
            })
        })
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
            let (bw, bh) = bounds.map_or((0.0, 0.0), |r| (r.width().to_mm(), r.height().to_mm()));
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
    let drc_opts = pcb_drc::DrcOptions {
        placement_margins: build_drc_margin_map(project),
        ..pcb_drc::DrcOptions::default()
    };
    let drc = pcb_drc::run(board, &drc_opts);

    let total_nets = nets.len();
    let unconnected_nets: usize = nets
        .iter()
        .filter(|n| {
            n["unconnected_pads"]
                .as_array()
                .is_some_and(|a| !a.is_empty())
        })
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
            let Some(symbol) = sch.symbols.get(&conn.symbol_id) else {
                continue;
            };
            let pad_ref = format!("{}.{}", symbol.reference, conn.pin_number);
            let pin_name = symbol
                .kind
                .pins()
                .iter()
                .find(|p| p.number == conn.pin_number)
                .map(|p| p.name.clone())
                .unwrap_or_default();
            let is_unconnected =
                unconnected_pads.contains(&(symbol.reference.clone(), conn.pin_number.clone()));
            if is_unconnected {
                unconnected.push(pad_ref.clone());
            } else {
                connected_count += 1;
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
        .filter(|n| {
            n["unconnected_pads"]
                .as_array()
                .is_some_and(|a| !a.is_empty())
        })
        .filter_map(|n| n["net"].as_str())
        .collect();
    Ok(text_result(format!(
        "{} nets total, {} with unconnected pads ({})",
        nets.len(),
        unconnected.len(),
        if unconnected.is_empty() {
            "all clean".to_string()
        } else {
            unconnected.join(", ")
        },
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
    /// Library key the agent picked (`snake_case`, e.g.
    /// "`esp32_s3_zero`"). Empty string means "no library entry".
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
    /// ERC role for the pin. Optional in the JSON; defaults to
    /// `passive` so existing scripts that didn't set a role keep
    /// their semantics.
    #[serde(default)]
    role: PinRoleInput,
}

#[derive(Debug, Deserialize, Clone, Copy, Default)]
#[serde(rename_all = "snake_case")]
enum PinRoleInput {
    #[default]
    Passive,
    Input,
    Output,
    Bidir,
    PowerOut,
    PowerIn,
}

impl From<PinRoleInput> for pcb_core::PinRole {
    fn from(v: PinRoleInput) -> Self {
        match v {
            PinRoleInput::Passive => Self::Passive,
            PinRoleInput::Input => Self::Input,
            PinRoleInput::Output => Self::Output,
            PinRoleInput::Bidir => Self::Bidir,
            PinRoleInput::PowerOut => Self::PowerOut,
            PinRoleInput::PowerIn => Self::PowerIn,
        }
    }
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
                    role: p.role.into(),
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
    /// Optional `NetClass` name to attach to this net. If unset (or
    /// the named class doesn't exist) the router and DRC fall back to
    /// their default `trace_width/clearance`.
    #[serde(default)]
    class: Option<String>,
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
                ToolError::invalid_params(format!("expected REF.PIN format, got {pin_ref:?}"))
            })?;
            let symbol = sch
                .find_by_reference(sym_ref)
                .ok_or_else(|| ToolError::invalid_params(format!("unknown symbol {sym_ref}")))?;
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
                .find(|p| p.number == pin_token || p.name.eq_ignore_ascii_case(pin_token))
                .map_or_else(|| pin_token.to_string(), |p| p.number.clone());
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
            class: input.class.clone(),
        })
        .map_err(ToolError::invalid_params)?;
    project.log(
        ActivityLevel::Info,
        format!("schematic.connect: {} ({} pin(s))", input.net, count),
    );
    Ok(
        text_result(format!("Net {} now has {} connection(s)", input.net, count))
            .with_data(json!({ "net": input.net, "connection_count": count })),
    )
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
    /// Override `edge_mounted` from the schematic side. Useful when the
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
                        size: (
                            Length::from_mm(pad_plan.w_mm),
                            Length::from_mm(pad_plan.h_mm),
                        ),
                        layer: pad_plan.layer.into(),
                        net,
                        drill: pad_plan.drill_mm.map(Length::from_mm),
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
            silk: Vec::new(),
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
    Ok(
        text_result(format!("Added {} item(s) to palette", added.len()))
            .with_data(json!({ "added": added })),
    )
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
            let (bw, bh) = bounds.map_or((0.0, 0.0), |r| (r.width().to_mm(), r.height().to_mm()));
            let mut nets: Vec<&str> = fp.pads.iter().filter_map(|p| p.net.as_deref()).collect();
            nets.sort_unstable();
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
    Ok(
        text_result(format!("{} item(s) waiting in the palette", entries.len()))
            .with_data(json!({ "items": entries })),
    )
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
    let pads: Vec<Value> = e
        .pads
        .iter()
        .map(|p| {
            json!({
                "number": p.number,
                "name": p.name,
                "x_mm": p.x_mm,
                "y_mm": p.y_mm,
                "w_mm": p.w_mm,
                "h_mm": p.h_mm,
            })
        })
        .collect();
    let silk: Vec<Value> = e
        .silk
        .iter()
        .map(|s| match s {
            LibrarySilk::Line {
                layer,
                x1_mm,
                y1_mm,
                x2_mm,
                y2_mm,
                width_mm,
            } => json!({
                "kind": "line",
                "layer": layer_to_str_silk(*layer),
                "x1_mm": x1_mm, "y1_mm": y1_mm, "x2_mm": x2_mm, "y2_mm": y2_mm,
                "width_mm": width_mm,
            }),
            LibrarySilk::Text {
                layer,
                x_mm,
                y_mm,
                text,
                size_mm,
                rotation_deg,
                anchor,
                width_mm,
            } => json!({
                "kind": "text",
                "layer": layer_to_str_silk(*layer),
                "x_mm": x_mm, "y_mm": y_mm,
                "text": text, "size_mm": size_mm,
                "rotation_deg": rotation_deg,
                "anchor": anchor_to_str(*anchor),
                "width_mm": width_mm,
            }),
        })
        .collect();
    if let Some(obj) = v.as_object_mut() {
        obj.insert("pads".into(), Value::Array(pads));
        obj.insert("silk".into(), Value::Array(silk));
    }
    v
}

fn anchor_to_str(a: SilkAnchor) -> &'static str {
    match a {
        SilkAnchor::Start => "start",
        SilkAnchor::Middle => "middle",
        SilkAnchor::End => "end",
    }
}

/// Convert a library-frame silk item into the runtime
/// footprint-local `FootprintSilk` representation. Library coords are
/// already footprint-local mm; we just rebox into nanometre `Length`.
/// Convert a `LibrarySilk` (footprint-local mm) into the runtime
/// `FootprintSilk` representation, applying the library entry's
/// `footprint_view_transform` so the body outline / pin-1 markers
/// track the visual orientation the user picked in the review pane.
/// Pass `ViewTransform::default()` for callers that have no view
/// transform context (currently none — the only call site is the
/// palette spawn).
fn library_silk_to_footprint_with_view(
    s: &LibrarySilk,
    vt: pcb_core::ViewTransform,
) -> FootprintSilk {
    match s {
        LibrarySilk::Line {
            layer,
            x1_mm,
            y1_mm,
            x2_mm,
            y2_mm,
            width_mm,
        } => {
            let (x1, y1) = vt.apply_point_mm(*x1_mm, *y1_mm);
            let (x2, y2) = vt.apply_point_mm(*x2_mm, *y2_mm);
            FootprintSilk::Line {
                layer: *layer,
                start: Point::new(Length::from_mm(x1), Length::from_mm(y1)),
                end: Point::new(Length::from_mm(x2), Length::from_mm(y2)),
                width: Length::from_mm(*width_mm),
            }
        }
        LibrarySilk::Text {
            layer,
            x_mm,
            y_mm,
            text,
            size_mm,
            rotation_deg,
            anchor,
            width_mm,
        } => {
            let (x, y) = vt.apply_point_mm(*x_mm, *y_mm);
            FootprintSilk::Text {
                layer: *layer,
                position: Point::new(Length::from_mm(x), Length::from_mm(y)),
                text: text.clone(),
                size: Length::from_mm(*size_mm),
                rotation: vt.apply_angle_deg(*rotation_deg),
                anchor: *anchor,
                width: Length::from_mm(*width_mm),
            }
        }
    }
}

fn tool_library_list(project: &Project) -> Result<Value, ToolError> {
    let entries = project.library().list();
    let items: Vec<Value> = entries.iter().map(library_entry_summary).collect();
    Ok(text_result(format!("{} entries in library", items.len()))
        .with_data(json!({ "entries": items })))
}

#[derive(Debug, Deserialize)]
struct LibraryFindInput {
    key: String,
}

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
    /// Plated through-hole drill diameter in mm. Omit for SMD.
    #[serde(default)]
    drill_mm: Option<f64>,
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
    /// Library-authored silk strokes. Coordinates are in
    /// footprint-local mm; the spawn step converts them into
    /// world-aware `FootprintSilk` items.
    #[serde(default)]
    silk: Vec<LibrarySilkInput>,
    /// Optional LCSC catalogue ID (e.g. "C25804"). Plumbed straight
    /// to the JLCPCB BOM writer so SMT assembly knows what part to
    /// load. Routing/placement ignore it.
    #[serde(default)]
    lcsc_id: Option<String>,
    /// Optional manufacturer part number (e.g. "RC0603FR-0710KL").
    /// Fab-agnostic identifier used by every assembler.
    #[serde(default)]
    mpn: Option<String>,
}

/// Wire-format mirror of `pcb_core::LibrarySilk` — kept separate from
/// the core type so we can accept the looser `Option<f64>` for an
/// auto-derived stroke width without touching the on-disk schema.
#[derive(Debug, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
enum LibrarySilkInput {
    Line {
        #[serde(default = "default_layer")]
        layer: LayerInput,
        x1_mm: f64,
        y1_mm: f64,
        x2_mm: f64,
        y2_mm: f64,
        #[serde(default = "default_silk_width")]
        width_mm: f64,
    },
    Text {
        #[serde(default = "default_layer")]
        layer: LayerInput,
        x_mm: f64,
        y_mm: f64,
        text: String,
        #[serde(default = "default_silk_size")]
        size_mm: f64,
        #[serde(default)]
        rotation_deg: f32,
        #[serde(default = "default_anchor_middle")]
        anchor: AnchorInput,
        #[serde(default)]
        width_mm: Option<f64>,
    },
}

impl From<LibrarySilkInput> for LibrarySilk {
    fn from(v: LibrarySilkInput) -> Self {
        match v {
            LibrarySilkInput::Line {
                layer,
                x1_mm,
                y1_mm,
                x2_mm,
                y2_mm,
                width_mm,
            } => LibrarySilk::Line {
                layer: layer.into(),
                x1_mm,
                y1_mm,
                x2_mm,
                y2_mm,
                width_mm,
            },
            LibrarySilkInput::Text {
                layer,
                x_mm,
                y_mm,
                text,
                size_mm,
                rotation_deg,
                anchor,
                width_mm,
            } => LibrarySilk::Text {
                layer: layer.into(),
                x_mm,
                y_mm,
                text,
                size_mm,
                rotation_deg,
                anchor: anchor.into(),
                width_mm: width_mm.unwrap_or(size_mm / 8.0),
            },
        }
    }
}

fn tool_library_create(project: &Project, args: &Value) -> Result<Value, ToolError> {
    let input: LibraryCreateInput = serde_json::from_value(args.clone())
        .map_err(|e| ToolError::invalid_params(format!("library.create: {e}")))?;
    let pads = input
        .pads
        .into_iter()
        .map(|p| pcb_core::LibraryPad {
            number: p.number,
            name: p.name,
            x_mm: p.x_mm,
            y_mm: p.y_mm,
            w_mm: p.w_mm,
            h_mm: p.h_mm,
            drill_mm: p.drill_mm,
        })
        .collect();
    let silk: Vec<LibrarySilk> = input.silk.into_iter().map(Into::into).collect();
    let entry = pcb_core::LibraryEntry {
        key: input.key.clone(),
        description: input.description,
        default_value: input.default_value,
        default_rotation_deg: input.default_rotation_deg,
        edge_mounted: input.edge_mounted,
        pads,
        silk,
        lcsc_id: input.lcsc_id,
        mpn: input.mpn,
        attachments: Vec::new(),
        created_at: 0,
        footprint_view_transform: pcb_core::ViewTransform::default(),
        placement_margin: pcb_core::PlacementMargin::default(),
    };
    // HARD GUARD: a library entry the agent generates does NOT land in
    // the on-disk library until a human eyeballs the rendered footprint
    // against the component photo and clicks confirm. Mirrored / mis-
    // numbered footprints have shipped fab orders that ended up in the
    // bin; the cost of one extra click per part is trivial next to
    // ordering PCBs twice.
    let pad_count = entry.pads.len();
    let key = entry.key.clone();
    let pending = pcb_core::PendingLibraryEntry {
        entry: entry.clone(),
        attachments: Vec::new(),
    };
    let pending_count = project.queue_pending_library_entry(pending);
    project.log(
        ActivityLevel::Info,
        format!(
            "library.create: {key} ({pad_count} pads) — pending human confirmation ({pending_count} queued)"
        ),
    );
    Ok(text_result(format!(
        "Queued {key} for review — open the library review pane (or the auto-popup) and confirm before it lands in the on-disk library. Attach the component photo via `library.attach` before confirming so the reviewer can compare pinout vs. reality."
    ))
    .with_data(library_entry_full(&entry)))
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
    // If the target entry is still in the pending-confirmation buffer
    // (the typical case: `library.create` → `library.attach photo` →
    // human reviews), stage the attachment on the pending record so the
    // review modal can display it. Only after `confirm_pending_library_entry`
    // does the file get written to disk via `Library::attach`. If the
    // entry was already confirmed earlier, fall through to the live
    // library so the agent can patch existing parts.
    if let Some(mut pending) = project.find_pending_library_entry(&input.key) {
        let byte_len = bytes.len();
        pending.attachments.push(pcb_core::PendingAttachment {
            kind: input.kind.clone(),
            filename: input.filename.clone(),
            mime: input.mime.clone(),
            data: bytes,
        });
        project.queue_pending_library_entry(pending);
        project.log(
            ActivityLevel::Info,
            format!(
                "library.attach: {} ← {} ({} bytes) [pending review]",
                input.key, input.filename, byte_len
            ),
        );
        return Ok(text_result(format!(
            "Staged {} on pending entry {} — will be persisted when the entry is confirmed",
            input.filename, input.key
        ))
        .with_data(json!({
            "kind": input.kind,
            "filename": input.filename,
            "mime": input.mime,
            "pending": true,
        })));
    }
    let att = project
        .library()
        .attach(&input.key, input.kind, input.filename, input.mime, &bytes)
        .map_err(ToolError::invalid_params)?;
    let count = project.library().list().len();
    project
        .events()
        .publish(pcb_core::Event::LibraryChanged { count });
    project.log(
        ActivityLevel::Info,
        format!(
            "library.attach: {} ← {} ({} bytes)",
            input.key,
            att.filename,
            bytes.len()
        ),
    );
    Ok(
        text_result(format!("Attached {}", att.filename)).with_data(json!({
            "id": att.id,
            "kind": att.kind,
            "filename": att.filename,
            "mime": att.mime,
            "added_at": att.added_at,
        })),
    )
}

#[derive(Debug, Deserialize)]
struct LibraryDeleteAttachmentInput {
    key: String,
    attachment_id: String,
}

fn tool_library_delete_attachment(project: &Project, args: &Value) -> Result<Value, ToolError> {
    let input: LibraryDeleteAttachmentInput = serde_json::from_value(args.clone())
        .map_err(|e| ToolError::invalid_params(format!("library.delete_attachment: {e}")))?;
    let removed = project
        .library()
        .delete_attachment(&input.key, &input.attachment_id)
        .map_err(ToolError::invalid_params)?;
    if removed {
        let count = project.library().list().len();
        project
            .events()
            .publish(pcb_core::Event::LibraryChanged { count });
    }
    Ok(text_result(
        if removed {
            "Attachment removed"
        } else {
            "No matching attachment"
        }
        .to_string(),
    )
    .with_data(json!({ "removed": removed })))
}

#[derive(Debug, Deserialize)]
struct LibraryDeleteInput {
    key: String,
}

fn tool_library_delete(project: &Project, args: &Value) -> Result<Value, ToolError> {
    let input: LibraryDeleteInput = serde_json::from_value(args.clone())
        .map_err(|e| ToolError::invalid_params(format!("library.delete: {e}")))?;
    let removed = project
        .library()
        .delete(&input.key)
        .map_err(ToolError::invalid_params)?;
    if removed {
        let count = project.library().list().len();
        project
            .events()
            .publish(pcb_core::Event::LibraryChanged { count });
        project.log(
            ActivityLevel::Info,
            format!("library.delete: {}", input.key),
        );
    }
    Ok(text_result(
        if removed {
            "Entry removed"
        } else {
            "No matching entry"
        }
        .to_string(),
    )
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
    let entry = project.library().find(&input.key).ok_or_else(|| {
        ToolError::invalid_params(format!(
            "palette.add_from_library: no library entry with key {}",
            input.key
        ))
    })?;

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
        let value = input
            .value
            .clone()
            .filter(|s| !s.is_empty())
            .or_else(|| (!symbol.value.is_empty()).then(|| symbol.value.clone()))
            .unwrap_or_else(|| entry.default_value.clone());
        let key_field = if symbol.key.is_empty() {
            input.key.clone()
        } else {
            symbol.key.clone()
        };
        let description_field = if symbol.description.is_empty() {
            entry.description.clone()
        } else {
            symbol.description.clone()
        };
        let vt = entry.footprint_view_transform;
        let pads: Vec<Pad> = entry
            .pads
            .iter()
            .map(|p| {
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
                // Apply the library entry's footprint_view_transform to
                // the native pad geometry. This is the orientation the
                // user dialled in via the review pane; the placer then
                // layers `place X Y ROT` on top of this. The original
                // `LibraryPad` (and `index.json`) stay untouched so the
                // review pane still drives off the native data.
                let (x_mm, y_mm) = vt.apply_point_mm(p.x_mm, p.y_mm);
                let (w_mm, h_mm) = vt.apply_size_mm(p.w_mm, p.h_mm);
                Pad {
                    number: p.number.clone(),
                    name: p.name.clone(),
                    offset: Point::new(Length::from_mm(x_mm), Length::from_mm(y_mm)),
                    size: (Length::from_mm(w_mm), Length::from_mm(h_mm)),
                    layer: input.layer.into(),
                    net,
                    drill: p.drill_mm.map(Length::from_mm),
                }
            })
            .collect();
        // edge_mounted: schematic doesn't have this yet; just inherit
        // from library.
        (
            value,
            key_field,
            description_field,
            pads,
            entry.edge_mounted,
        )
    };

    // Library silk lives in footprint-local mm just like the pads, so it
    // gets the same view transform — body outlines and pin-1 dots stay
    // visually attached to the pads after a flip / rotate.
    let vt = entry.footprint_view_transform;
    let silk: Vec<FootprintSilk> = entry
        .silk
        .iter()
        .map(|s| library_silk_to_footprint_with_view(s, vt))
        .collect();
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
        silk,
    };
    project
        .palette_add(footprint)
        .map_err(ToolError::invalid_params)?;
    project.log(
        ActivityLevel::Info,
        format!(
            "palette.add_from_library: {} ← {}",
            input.reference, input.key
        ),
    );
    Ok(
        text_result(format!("Spawned {} from {}", input.reference, input.key))
            .with_data(json!({ "reference": input.reference, "key": input.key })),
    )
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
    Ok(
        text_result(format!("{ok_count} placed, {fail_count} failed")).with_data(json!({
            "ok_count": ok_count,
            "fail_count": fail_count,
            "results": results,
        })),
    )
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
    Ok(text_result(format!("Rotated {} to {normalised:.0}°", input.reference)).into())
}

#[derive(Debug, Deserialize)]
struct PlacementDeleteInput {
    refs: Vec<String>,
}

fn tool_placement_delete(project: &Project, args: &Value) -> Result<Value, ToolError> {
    let input: PlacementDeleteInput = serde_json::from_value(args.clone())
        .map_err(|e| ToolError::invalid_params(format!("placement.delete: {e}")))?;
    if input.refs.is_empty() {
        return Err(ToolError::invalid_params(
            "placement.delete: at least one reference required".to_string(),
        ));
    }
    // Stop at the first ref that doesn't exist so the human sees the
    // typo immediately rather than getting a half-applied delete.
    let mut summaries: Vec<pcb_core::DeletedFootprint> = Vec::with_capacity(input.refs.len());
    for r in &input.refs {
        match project.delete_footprint_by_ref(r) {
            Ok(s) => summaries.push(s),
            Err(e) => {
                return Err(ToolError::invalid_params(e));
            }
        }
    }
    let mut total_traces = 0_usize;
    let mut total_vias = 0_usize;
    let mut total_pads = 0_usize;
    let mut orphaned: Vec<String> = Vec::new();
    let mut per_ref = Vec::with_capacity(summaries.len());
    for s in &summaries {
        total_traces += s.traces_removed;
        total_vias += s.vias_removed;
        total_pads += s.pad_count;
        for n in &s.orphaned_nets {
            if !orphaned.contains(n) {
                orphaned.push(n.clone());
            }
        }
        let key_display = if s.key.is_empty() {
            s.library.clone()
        } else {
            s.key.clone()
        };
        per_ref.push(json!({
            "reference": s.reference,
            "id": s.id.0.to_string(),
            "key": key_display,
            "pads": s.pad_count,
            "traces_removed": s.traces_removed,
            "vias_removed": s.vias_removed,
            "orphaned_nets": s.orphaned_nets,
        }));
    }
    let refs_csv = summaries
        .iter()
        .map(|s| s.reference.as_str())
        .collect::<Vec<_>>()
        .join(", ");
    // Use the first summary's key/lib for the single-ref reply shape
    // requested in the spec; for multi-ref we degrade to a roll-up.
    let mut msg = if summaries.len() == 1 {
        let s = &summaries[0];
        let key_display = if s.key.is_empty() {
            s.library.clone()
        } else {
            s.key.clone()
        };
        format!(
            "removed {} ({}, {} pads) + {} traces + {} vias",
            s.reference, key_display, s.pad_count, s.traces_removed, s.vias_removed,
        )
    } else {
        format!(
            "removed {} footprint(s) [{}] + {} traces + {} vias ({} pads total)",
            summaries.len(),
            refs_csv,
            total_traces,
            total_vias,
            total_pads,
        )
    };
    if !orphaned.is_empty() {
        msg.push_str(&format!(
            " — WARNING: net(s) {} now have no pads on the board",
            orphaned.join(", ")
        ));
    }
    project.log(
        ActivityLevel::Info,
        format!("placement.delete: {refs_csv} ({total_traces} traces, {total_vias} vias cleared)"),
    );
    Ok(text_result(msg).with_data(json!({
        "removed": per_ref,
        "total_traces_removed": total_traces,
        "total_vias_removed": total_vias,
        "orphaned_nets": orphaned,
    })))
}

fn tool_placement_clear_board(project: &Project) -> Result<Value, ToolError> {
    let refs = project.clear_board_placements();
    let msg = if refs.is_empty() {
        "board already empty".to_string()
    } else {
        format!("cleared {} footprint(s) and all routing", refs.len())
    };
    project.log(
        ActivityLevel::Info,
        format!("placement.clear_board: {} removed", refs.len()),
    );
    Ok(text_result(msg).with_data(json!({"removed": refs})))
}

#[derive(Debug, Deserialize)]
struct ClearNetInput {
    net: String,
}

fn tool_route_clear_net(project: &Project, args: &Value) -> Result<Value, ToolError> {
    let input: ClearNetInput = serde_json::from_value(args.clone())
        .map_err(|e| ToolError::invalid_params(format!("route.clear_net: {e}")))?;
    let removed = project.clear_net_routing(&input.net);
    project.log(
        ActivityLevel::Info,
        format!("route.clear_net: {} ({} item(s))", input.net, removed),
    );
    Ok(
        text_result(format!("Cleared {removed} item(s) from net {}", input.net))
            .with_data(json!({"removed": removed})),
    )
}

#[derive(Debug, Deserialize)]
struct DeleteByIdInput {
    id: String,
}

fn parse_id(s: &str) -> Result<pcb_core::Id, ToolError> {
    pcb_core::Id::parse(s).map_err(|e| ToolError::invalid_params(format!("invalid id {s}: {e}")))
}

fn tool_route_delete_trace(project: &Project, args: &Value) -> Result<Value, ToolError> {
    let input: DeleteByIdInput = serde_json::from_value(args.clone())
        .map_err(|e| ToolError::invalid_params(format!("route.delete_trace: {e}")))?;
    let id = parse_id(&input.id)?;
    let ok = project.delete_trace(id);
    Ok(text_result(
        if ok {
            "Trace removed"
        } else {
            "Trace not found"
        }
        .to_string(),
    )
    .with_data(json!({"removed": ok})))
}

fn tool_route_delete_via(project: &Project, args: &Value) -> Result<Value, ToolError> {
    let input: DeleteByIdInput = serde_json::from_value(args.clone())
        .map_err(|e| ToolError::invalid_params(format!("route.delete_via: {e}")))?;
    let id = parse_id(&input.id)?;
    let ok = project.delete_via(id);
    Ok(
        text_result(if ok { "Via removed" } else { "Via not found" }.to_string())
            .with_data(json!({"removed": ok})),
    )
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
    /// Plated through-hole drill diameter in mm. Omit for SMD.
    #[serde(default)]
    drill_mm: Option<f64>,
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
        thermal_relief: pcb_core::ThermalRelief::default(),
        stitching: pcb_core::StitchPolicy::None,
    });
    project.log(
        ActivityLevel::Info,
        format!("pour.add {} on {:?}", input.net, layer),
    );
    Ok(
        text_result(format!("Pour added: net={} layer={:?}", input.net, layer))
            .with_data(json!({"net": input.net, "layer": layer_to_str(layer)})),
    )
}

#[derive(Debug, Deserialize)]
struct KeepoutAddInput {
    /// Vertices as `[[x_mm, y_mm], ...]`. Three or more.
    points: Vec<[f64; 2]>,
    #[serde(default)]
    layer: Option<String>,
    #[serde(default)]
    label: Option<String>,
}

fn tool_keepout_add(project: &Project, args: &Value) -> Result<Value, ToolError> {
    let input: KeepoutAddInput = serde_json::from_value(args.clone())
        .map_err(|e| ToolError::invalid_params(format!("keepout.add: {e}")))?;
    if input.points.len() < 3 {
        return Err(ToolError::invalid_params(
            "keepout.add: need at least 3 points",
        ));
    }
    let polygon: Vec<Point> = input
        .points
        .iter()
        .map(|[x, y]| Point::new(Length::from_mm(*x), Length::from_mm(*y)))
        .collect();
    let layers: Vec<CopperLayer> = match input.layer.as_deref().unwrap_or("both") {
        "top" => vec![CopperLayer::Top],
        "bottom" => vec![CopperLayer::Bottom],
        "both" | "" => vec![],
        other => {
            return Err(ToolError::invalid_params(format!(
                "keepout.add: layer must be top|bottom|both, got `{other}`"
            )));
        }
    };
    let kp = pcb_core::Keepout {
        id: pcb_core::Id::new(),
        polygon,
        layers,
        nets_allowed: Vec::new(),
        label: input.label.unwrap_or_default(),
    };
    let id = project.add_keepout(kp);
    project.log(ActivityLevel::Info, format!("keepout.add: {}", id.0));
    Ok(text_result(format!("Keepout added: {}", id.0))
        .with_data(json!({ "id": id.0.to_string() })))
}

fn tool_keepout_list(project: &Project) -> Result<Value, ToolError> {
    let snap = project.read();
    let board = snap.board();
    let items: Vec<Value> = board
        .keepouts
        .iter()
        .map(|kp| {
            json!({
                "id": kp.id.0.to_string(),
                "label": kp.label,
                "points": kp.polygon.iter()
                    .map(|p| json!([p.x.to_mm(), p.y.to_mm()]))
                    .collect::<Vec<_>>(),
                "layers": kp.layers.iter().map(|l| layer_to_str(*l)).collect::<Vec<_>>(),
                "nets_allowed": kp.nets_allowed.clone(),
            })
        })
        .collect();
    Ok(text_result(format!("{} keepout(s)", items.len())).with_data(json!({ "keepouts": items })))
}

#[derive(Debug, Deserialize)]
struct KeepoutRemoveInput {
    id: String,
}

fn tool_keepout_remove(project: &Project, args: &Value) -> Result<Value, ToolError> {
    let input: KeepoutRemoveInput = serde_json::from_value(args.clone())
        .map_err(|e| ToolError::invalid_params(format!("keepout.remove: {e}")))?;
    let id = pcb_core::Id::parse(&input.id).map_err(ToolError::invalid_params)?;
    let removed = project.remove_keepout(id);
    Ok(text_result(if removed {
        format!("Keepout {} removed", input.id)
    } else {
        format!("No keepout with id {}", input.id)
    })
    .with_data(json!({ "removed": removed })))
}

#[derive(Debug, Deserialize)]
struct PourReliefInput {
    net: String,
    style: String,
    #[serde(default)]
    spoke_width_mm: Option<f64>,
    #[serde(default)]
    gap_mm: Option<f64>,
}

fn tool_pour_relief(project: &Project, args: &Value) -> Result<Value, ToolError> {
    let input: PourReliefInput = serde_json::from_value(args.clone())
        .map_err(|e| ToolError::invalid_params(format!("pour.relief: {e}")))?;
    let relief = match input.style.as_str() {
        "solid" => pcb_core::ThermalRelief::Solid,
        "spokes" => pcb_core::ThermalRelief::Spokes4 {
            spoke_width_mm: input.spoke_width_mm.unwrap_or(0.4),
            gap_mm: input.gap_mm.unwrap_or(0.4),
        },
        other => {
            return Err(ToolError::invalid_params(format!(
                "pour.relief: style must be solid|spokes, got `{other}`"
            )));
        }
    };
    let changed = project.set_pour_relief(&input.net, relief);
    project.log(
        ActivityLevel::Info,
        format!("pour.relief: net={} style={}", input.net, input.style),
    );
    Ok(text_result(if changed > 0 {
        format!(
            "Updated {} pour(s) on net `{}` to {}",
            changed, input.net, input.style
        )
    } else {
        format!("No pour found on net `{}`", input.net)
    })
    .with_data(json!({"changed": changed, "net": input.net, "style": input.style})))
}

#[derive(Debug, Deserialize)]
struct PourStitchInput {
    net: String,
    policy: String,
    #[serde(default)]
    pitch_mm: Option<f64>,
    #[serde(default)]
    clearance_mm: Option<f64>,
}

fn tool_pour_stitch(project: &Project, args: &Value) -> Result<Value, ToolError> {
    let input: PourStitchInput = serde_json::from_value(args.clone())
        .map_err(|e| ToolError::invalid_params(format!("pour.stitch: {e}")))?;
    let policy = match input.policy.as_str() {
        "none" => pcb_core::StitchPolicy::None,
        "grid" => pcb_core::StitchPolicy::Grid {
            pitch_mm: input.pitch_mm.unwrap_or(5.0),
            clearance_mm: input.clearance_mm.unwrap_or(0.5),
        },
        other => {
            return Err(ToolError::invalid_params(format!(
                "pour.stitch: policy must be none|grid, got `{other}`"
            )));
        }
    };
    let changed = project.set_pour_stitching(&input.net, policy);
    project.log(
        ActivityLevel::Info,
        format!("pour.stitch: net={} policy={}", input.net, input.policy),
    );
    Ok(text_result(if changed > 0 {
        format!(
            "Updated {} pour(s) on net `{}` to stitching={}",
            changed, input.net, input.policy
        )
    } else {
        format!("No pour found on net `{}`", input.net)
    })
    .with_data(json!({"changed": changed, "net": input.net, "policy": input.policy})))
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

#[derive(Debug, Deserialize)]
struct SilkLineInput {
    layer: LayerInput,
    x1_mm: f64,
    y1_mm: f64,
    x2_mm: f64,
    y2_mm: f64,
    #[serde(default = "default_silk_width")]
    width_mm: f64,
}

#[derive(Debug, Deserialize)]
struct SilkTextInput {
    layer: LayerInput,
    x_mm: f64,
    y_mm: f64,
    text: String,
    #[serde(default = "default_silk_size")]
    size_mm: f64,
    #[serde(default)]
    rotation: f32,
    #[serde(default = "default_anchor_middle")]
    anchor: AnchorInput,
    #[serde(default)]
    width_mm: Option<f64>,
}

fn default_silk_width() -> f64 {
    0.15
}
fn default_silk_size() -> f64 {
    1.2
}

fn tool_silk_add_line(project: &Project, args: &Value) -> Result<Value, ToolError> {
    let input: SilkLineInput = serde_json::from_value(args.clone())
        .map_err(|e| ToolError::invalid_params(format!("silk.add_line: {e}")))?;
    let layer: SilkLayer = input.layer.into();
    let line = SilkLine {
        layer,
        start: Point::new(Length::from_mm(input.x1_mm), Length::from_mm(input.y1_mm)),
        end: Point::new(Length::from_mm(input.x2_mm), Length::from_mm(input.y2_mm)),
        width: Length::from_mm(input.width_mm),
    };
    project.add_silk_line(line);
    project.log(
        ActivityLevel::Info,
        format!(
            "silk.add_line {:?} ({:.2},{:.2})→({:.2},{:.2})",
            layer, input.x1_mm, input.y1_mm, input.x2_mm, input.y2_mm
        ),
    );
    Ok(text_result("Silk line added").with_data(json!({
        "layer": layer_to_str_silk(layer),
    })))
}

fn tool_silk_add_text(project: &Project, args: &Value) -> Result<Value, ToolError> {
    let input: SilkTextInput = serde_json::from_value(args.clone())
        .map_err(|e| ToolError::invalid_params(format!("silk.add_text: {e}")))?;
    let layer: SilkLayer = input.layer.into();
    let size = Length::from_mm(input.size_mm);
    let text = SilkText {
        layer,
        position: Point::new(Length::from_mm(input.x_mm), Length::from_mm(input.y_mm)),
        text: input.text.clone(),
        size,
        rotation: input.rotation,
        anchor: input.anchor.into(),
        width: input
            .width_mm
            .map_or_else(|| SilkText::default_stroke(size), Length::from_mm),
    };
    project.add_silk_text(text);
    project.log(
        ActivityLevel::Info,
        format!("silk.add_text {:?} \"{}\"", layer, input.text),
    );
    Ok(
        text_result(format!("Silk text added: \"{}\"", input.text)).with_data(json!({
            "layer": layer_to_str_silk(layer),
            "text": input.text,
        })),
    )
}

fn layer_to_str_silk(layer: SilkLayer) -> &'static str {
    match layer {
        SilkLayer::Top => "top",
        SilkLayer::Bottom => "bottom",
    }
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
    // Script DSL always emits numbers as floats; accept either and
    // round down so `via_cost=8` works whether typed as 8 or 8.0.
    #[serde(default = "default_via_cost", deserialize_with = "de_u32_lenient")]
    via_cost: u32,
    #[serde(default = "default_via_drill")]
    via_drill_mm: f64,
    #[serde(default = "default_via_diameter")]
    via_diameter_mm: f64,
    /// Comma-separated list of net names. When present, seeds the
    /// router's first-pass ordering — useful for GA-driven tuning.
    #[serde(default)]
    order: Option<String>,
}

fn de_u32_lenient<'de, D>(d: D) -> Result<u32, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let v = serde_json::Value::deserialize(d)?;
    match v {
        serde_json::Value::Number(n) => {
            if let Some(u) = n.as_u64() {
                Ok(u as u32)
            } else if let Some(f) = n.as_f64() {
                if f.is_finite() && f >= 0.0 {
                    Ok(f as u32)
                } else {
                    Err(serde::de::Error::custom(format!("invalid via_cost: {f}")))
                }
            } else {
                Err(serde::de::Error::custom("via_cost: not a number"))
            }
        }
        other => Err(serde::de::Error::custom(format!(
            "via_cost: expected number, got {other}"
        ))),
    }
}

fn default_cell() -> f64 {
    0.25
}
fn default_trace_w() -> f64 {
    0.25
}
fn default_clearance() -> f64 {
    0.20
}
fn default_via_cost() -> u32 {
    8
}
fn default_via_drill() -> f64 {
    0.30
}
fn default_via_diameter() -> f64 {
    0.60
}

#[derive(Debug, Deserialize)]
struct AutoPlaceInput {
    refs: Vec<String>,
    /// Floats so the script parser (which emits `42` as `42.0` for any
    /// numeric kv) can hand them to us; we cast to the integer types
    /// the placer wants below. Negative or NaN values are clamped.
    #[serde(default)]
    iters: Option<f64>,
    #[serde(default)]
    seed: Option<f64>,
    #[serde(default)]
    max_step_mm: Option<f64>,
    #[serde(default)]
    min_step_mm: Option<f64>,
    #[serde(default)]
    min_gap_mm: Option<f64>,
    #[serde(default)]
    gap_penalty_factor: Option<f64>,
    #[serde(default)]
    congestion_penalty_factor: Option<f64>,
    #[serde(default)]
    congestion_resolution: Option<f64>,
}

fn tool_placement_auto(project: &Project, args: &Value) -> Result<Value, ToolError> {
    let input: AutoPlaceInput = serde_json::from_value(args.clone())
        .map_err(|e| ToolError::invalid_params(format!("auto-place: {e}")))?;

    let mut opts = pcb_placer::PlaceOptions::default();
    if let Some(v) = input.iters {
        opts.max_iterations = v.max(0.0) as usize;
    }
    if let Some(v) = input.seed {
        opts.seed = v.max(0.0) as u64;
    }
    if let Some(v) = input.max_step_mm {
        opts.max_step_mm = v;
    }
    if let Some(v) = input.min_step_mm {
        opts.min_step_mm = v;
    }
    if let Some(v) = input.min_gap_mm {
        opts.min_gap_mm = v;
    }
    if let Some(v) = input.gap_penalty_factor {
        opts.gap_penalty_factor = v;
    }
    if let Some(v) = input.congestion_penalty_factor {
        opts.congestion_penalty_factor = v;
    }
    if let Some(v) = input.congestion_resolution {
        opts.congestion_resolution = v.max(0.0) as u32;
    }

    // Place on a clone so the project lock is released quickly. Apply
    // the resulting positions back through the regular `move_footprint_to`
    // / `rotate_footprint` APIs so the UI sees the moves event by event.
    let mut work = project.read().board().clone();
    // Build a per-id margin map from the library so footprints linked
    // to a `LibraryEntry::placement_margin` get extra body-to-body
    // breathing room during the SA search.
    let margins: pcb_placer::MarginMap = work
        .footprints_in_order()
        .filter_map(|fp| {
            if fp.key.is_empty() {
                return None;
            }
            let entry = project.library().find(&fp.key)?;
            let m = entry.placement_margin;
            if m.top_mm <= 0.0 && m.right_mm <= 0.0 && m.bottom_mm <= 0.0 && m.left_mm <= 0.0 {
                return None;
            }
            Some((fp.id, [m.top_mm, m.right_mm, m.bottom_mm, m.left_mm]))
        })
        .collect();
    let report = pcb_placer::place(&mut work, &input.refs, &opts, &margins)
        .map_err(|e| ToolError::invalid_params(format!("auto-place: {e}")))?;

    // Push back any positions / rotations that actually changed. We
    // use the id-based, unchecked Project APIs (`move_footprint` /
    // `set_footprint_rotation`) instead of the ref-based, validated
    // ones: the placer's FINAL state is consistent, but applying move
    // by move re-validates each intermediate state against the LIVE
    // project, which falsely rejects a step when two parts cross paths
    // mid-batch. The id-based path skips the re-check.
    let mut applied_moves = 0usize;
    let mut applied_rotations = 0usize;
    let live_id_of_ref: HashMap<String, pcb_core::Id> = project
        .read()
        .board()
        .footprints_in_order()
        .map(|fp| (fp.reference.clone(), fp.id))
        .collect();
    for r in &report.moved {
        let Some(target) = work
            .footprints_in_order()
            .find(|fp| &fp.reference == r)
            .cloned()
        else {
            continue;
        };
        let Some(&id) = live_id_of_ref.get(r) else {
            continue;
        };
        if project.move_footprint(id, target.position) {
            applied_moves += 1;
        }
        if project.set_footprint_rotation(id, target.rotation) {
            applied_rotations += 1;
        }
    }
    let errors: Vec<String> = Vec::new();

    project.log(
        ActivityLevel::Info,
        format!(
            "auto-place: HPWL {:.1} → {:.1} mm ({:+.1} mm), congestion {:.0} → {:.0} ({:+.0}), {} accepted of {} iters, applied {} moves",
            report.initial_hpwl_mm,
            report.final_hpwl_mm,
            report.final_hpwl_mm - report.initial_hpwl_mm,
            report.initial_congestion,
            report.final_congestion,
            report.final_congestion - report.initial_congestion,
            report.accepted,
            report.iterations,
            applied_moves,
        ),
    );

    let mut text = format!(
        "auto-place: HPWL {:.1} mm → {:.1} mm ({:+.1} mm), congestion {:.0} → {:.0} ({:+.0} cells); moved {} footprint(s)",
        report.initial_hpwl_mm,
        report.final_hpwl_mm,
        report.final_hpwl_mm - report.initial_hpwl_mm,
        report.initial_congestion,
        report.final_congestion,
        report.final_congestion - report.initial_congestion,
        applied_moves,
    );
    if !report.skipped.is_empty() {
        text.push_str(&format!(
            "\n  skipped {} unknown ref(s): {}",
            report.skipped.len(),
            report.skipped.join(", "),
        ));
    }
    if !errors.is_empty() {
        text.push_str("\n  errors:");
        for e in &errors {
            text.push_str("\n    ");
            text.push_str(e);
        }
    }

    Ok(text_result(text).with_data(json!({
        "initial_hpwl_mm": round2(report.initial_hpwl_mm),
        "final_hpwl_mm": round2(report.final_hpwl_mm),
        "delta_mm": round2(report.final_hpwl_mm - report.initial_hpwl_mm),
        "initial_congestion": round2(report.initial_congestion),
        "final_congestion": round2(report.final_congestion),
        "iterations": report.iterations,
        "accepted": report.accepted,
        "moved": report.moved,
        "applied_moves": applied_moves,
        "applied_rotations": applied_rotations,
        "skipped": report.skipped,
        "errors": errors,
    })))
}

fn tool_route_run(project: &Project, args: &Value) -> Result<Value, ToolError> {
    let input: RouteRunInput = serde_json::from_value(args.clone())
        .map_err(|e| ToolError::invalid_params(format!("route.run: {e}")))?;

    // Materialise any class-declared pours BEFORE the router clones
    // the board — otherwise the router lays redundant traces on what
    // should have been a pour-only net. Idempotent.
    let _ = materialize_class_pours(project);

    // Snapshot the schematic so the router can resolve per-net classes
    // itself. This replaces the previous "rebuild a net_overrides map
    // from class fields" path — the router now consults the schematic
    // directly via `RouteOptions::schematic`.
    let schematic_arc = {
        let snap = project.read();
        std::sync::Arc::new(snap.schematic().clone())
    };
    // Legacy: also build the overrides map for any caller code that
    // still expects to see it populated (keeps router-tune working in
    // mixed setups). Empty when no overrides are needed.
    let net_overrides: std::collections::HashMap<String, pcb_router::NetOverride> =
        std::collections::HashMap::new();

    let initial_net_order = input.order.as_ref().map(|s| {
        s.split(',')
            .map(|t| t.trim().to_string())
            .filter(|t| !t.is_empty())
            .collect::<Vec<String>>()
    });

    let opts = pcb_router::RouteOptions {
        cell: Length::from_mm(input.cell_mm),
        trace_width: Length::from_mm(input.trace_width_mm),
        clearance: Length::from_mm(input.clearance_mm),
        via_cost: input.via_cost,
        via_drill: Length::from_mm(input.via_drill_mm),
        via_diameter: Length::from_mm(input.via_diameter_mm),
        net_overrides,
        schematic: Some(schematic_arc),
        initial_net_order,
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
            pcb_router::Outcome::Ok {
                trace_segments,
                vias,
                length_mm,
                lower_bound_mm,
            } => {
                let detour = if *lower_bound_mm > 0.0 {
                    length_mm / lower_bound_mm
                } else {
                    1.0
                };
                json!({
                    "net": name, "ok": true,
                    "trace_segments": trace_segments, "vias": vias,
                    "length_mm": round2(*length_mm),
                    "lower_bound_mm": round2(*lower_bound_mm),
                    "detour_ratio": round2(detour),
                })
            }
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
    let total_detour = if report.total_lower_bound_mm > 0.0 {
        report.total_length_mm / report.total_lower_bound_mm
    } else {
        1.0
    };

    // Run DRC right after the route so the agent gets the verdict
    // in a single round-trip and can iterate without a second call.
    let drc_report = {
        let margins = build_drc_margin_map(project);
        let snap = project.read();
        let opts = pcb_drc::DrcOptions {
            placement_margins: margins,
            ..pcb_drc::DrcOptions::default()
        };
        pcb_drc::run(snap.board(), &opts)
    };
    project.log(
        ActivityLevel::Info,
        format!(
            "route.run: {} traces, {} vias, {:.1} mm wire (detour {:.2}×), {} pass(es), {} net(s) failed; DRC {}E {}W",
            report.trace_count,
            report.via_count,
            report.total_length_mm,
            total_detour,
            report.iterations,
            failed.len(),
            drc_report.error_count,
            drc_report.warning_count,
        ),
    );
    // Surface placement hints in the activity log too: the agent's
    // most-recent action ends with these so failures lead directly to
    // a concrete next move.
    for hint in &report.hints {
        project.log(ActivityLevel::Warn, format!("route.hint: {hint}"));
    }
    let hints_block = if report.hints.is_empty() {
        String::new()
    } else {
        let lines: Vec<String> = report.hints.iter().map(|h| format!("  - {h}")).collect();
        format!("\nhints:\n{}", lines.join("\n"))
    };
    Ok(text_result(format!(
        "Routed: {} traces, {} vias, {:.1} mm wire, {} failed (detour {:.2}× over {:.1} mm lower bound), {} pass(es){}; DRC: {} error(s), {} warning(s){}",
        report.trace_count,
        report.via_count,
        report.total_length_mm,
        failed.len(),
        total_detour,
        report.total_lower_bound_mm,
        report.iterations,
        if failed.is_empty() {
            String::new()
        } else {
            format!(" ({} failed: {})", failed.len(), failed.join(", "))
        },
        drc_report.error_count,
        drc_report.warning_count,
        hints_block,
    ))
    .with_data(json!({
        "trace_count": report.trace_count,
        "via_count": report.via_count,
        "total_length_mm": round2(report.total_length_mm),
        "total_lower_bound_mm": round2(report.total_lower_bound_mm),
        "total_detour_ratio": round2(total_detour),
        "iterations": report.iterations,
        "per_net": per_net,
        "hints": report.hints,
        "drc": serde_json::to_value(&drc_report).unwrap_or(json!({})),
    })))
}

fn round2(v: f64) -> f64 {
    (v * 100.0).round() / 100.0
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
    #[serde(default)]
    routing_inefficient_ratio: Option<f32>,
}

fn tool_drc_run(project: &Project, args: &Value) -> Result<Value, ToolError> {
    let input: DrcInput = serde_json::from_value(args.clone())
        .map_err(|e| ToolError::invalid_params(format!("drc.run: {e}")))?;

    let mut opts = pcb_drc::DrcOptions {
        placement_margins: build_drc_margin_map(project),
        ..pcb_drc::DrcOptions::default()
    };
    if let Some(v) = input.min_clearance_mm {
        opts.min_clearance = Length::from_mm(v);
    }
    if let Some(v) = input.edge_clearance_mm {
        opts.edge_clearance = Length::from_mm(v);
    }
    if let Some(v) = input.min_trace_width_mm {
        opts.min_trace_width = Length::from_mm(v);
    }
    if let Some(v) = input.min_drill_mm {
        opts.min_drill = Length::from_mm(v);
    }
    if let Some(v) = input.routing_inefficient_ratio {
        opts.routing_inefficient_ratio = v;
    }

    let snap = project.read();
    // Hand the schematic to DRC so per-net class clearances are
    // enforced (no per-net override map needed).
    opts.schematic = Some(std::sync::Arc::new(snap.schematic().clone()));
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
struct SetClassInput {
    name: String,
    #[serde(default)]
    trace_width_mm: Option<f64>,
    #[serde(default)]
    clearance_mm: Option<f64>,
    /// Via copper-pad diameter (mm). `None` → use route defaults.
    #[serde(default)]
    via_diameter_mm: Option<f64>,
    /// Via drill diameter (mm). `None` → use route defaults.
    #[serde(default)]
    via_drill_mm: Option<f64>,
    /// Z0 single-ended impedance target. Schema-only for now.
    #[serde(default)]
    target_impedance_ohms: Option<f64>,
    /// Partner net for differential pair routing. Schema-only for now.
    #[serde(default)]
    diff_pair_with: Option<String>,
    /// Diff-pair edge-to-edge gap (mm). Schema-only for now.
    #[serde(default)]
    diff_gap_mm: Option<f64>,
    /// Layer(s) for the auto-pour: "top", "bottom", or "both". When
    /// set, every net assigned to this class gets a `Pour`
    /// materialised on the listed layer(s) by `auto-pour` (and
    /// implicitly by `route`).
    #[serde(default)]
    pour: Option<PourLayersInput>,
    /// Length-match target (mm). When set, the router post-pass
    /// extends nets in this class with a serpentine to reach this
    /// length.
    #[serde(default)]
    target_length_mm: Option<f64>,
    /// Tolerance (mm) for length match. Defaults to the NetClass
    /// default (0.5 mm) when absent.
    #[serde(default)]
    length_tolerance_mm: Option<f64>,
}

#[derive(Debug, Deserialize, Clone, Copy)]
#[serde(rename_all = "lowercase")]
enum PourLayersInput {
    Top,
    Bottom,
    Both,
}

impl PourLayersInput {
    fn to_layers(self) -> Vec<CopperLayer> {
        match self {
            Self::Top => vec![CopperLayer::Top],
            Self::Bottom => vec![CopperLayer::Bottom],
            Self::Both => vec![CopperLayer::Top, CopperLayer::Bottom],
        }
    }
}

#[derive(Debug, Deserialize)]
struct AssignNetClassInput {
    net: String,
    class: String,
}

fn tool_schematic_assign_net_class(project: &Project, args: &Value) -> Result<Value, ToolError> {
    let input: AssignNetClassInput = serde_json::from_value(args.clone())
        .map_err(|e| ToolError::invalid_params(format!("net-class: {e}")))?;
    project
        .assign_net_to_class(input.net.clone(), input.class.clone())
        .map_err(ToolError::invalid_params)?;
    Ok(text_result(format!("net `{}` → class `{}`", input.net, input.class)).with_data(
        json!({ "net": input.net, "class": input.class }),
    ))
}

fn tool_schematic_set_class(project: &Project, args: &Value) -> Result<Value, ToolError> {
    let input: SetClassInput = serde_json::from_value(args.clone())
        .map_err(|e| ToolError::invalid_params(format!("class: {e}")))?;
    if input.name.trim().is_empty() {
        return Err(ToolError::invalid_params("class: name is empty"));
    }
    let class = pcb_core::NetClass {
        name: input.name.clone(),
        trace_width_mm: input.trace_width_mm,
        clearance_mm: input.clearance_mm,
        via_diameter_mm: input.via_diameter_mm,
        via_drill_mm: input.via_drill_mm,
        target_impedance_ohms: input.target_impedance_ohms,
        diff_pair_with: input.diff_pair_with.clone(),
        diff_gap_mm: input.diff_gap_mm,
        pour_layers: input
            .pour
            .map(PourLayersInput::to_layers)
            .unwrap_or_default(),
        target_length_mm: input.target_length_mm,
        length_tolerance_mm: input
            .length_tolerance_mm
            .unwrap_or(pcb_core::NetClass::default().length_tolerance_mm),
    };
    project.set_net_class(class);
    let mut text = format!("class {} set", input.name);
    let mut bits: Vec<String> = Vec::new();
    if let Some(w) = input.trace_width_mm {
        bits.push(format!("width={w} mm"));
    }
    if let Some(c) = input.clearance_mm {
        bits.push(format!("clearance={c} mm"));
    }
    if !bits.is_empty() {
        text.push_str(&format!(" ({})", bits.join(", ")));
    }
    Ok(text_result(text).with_data(json!({
        "name": input.name,
        "trace_width_mm": input.trace_width_mm,
        "clearance_mm": input.clearance_mm,
        "pour_layers": input.pour
            .map(|p| p.to_layers().into_iter().map(layer_to_str).collect::<Vec<_>>())
            .unwrap_or_default(),
    })))
}

/// Look up every net assigned to a `NetClass` whose `pour_layer` is
/// set, and add a `Pour { net, layer }` for each. Idempotent (the
/// project's `add_pour` replaces same-key pours rather than
/// duplicating). Returns the list of nets that newly got pours, the
/// list that already had matching pours, and the list of class refs
/// pointing at undeclared classes (skipped).
fn materialize_class_pours(project: &Project) -> ClassPourSummary {
    use std::collections::HashSet;
    let mut summary = ClassPourSummary::default();
    let snap = project.read();
    let sch = snap.schematic();
    let board = snap.board();
    let existing: HashSet<(String, CopperLayer)> = board
        .pours
        .iter()
        .map(|p| (p.net.clone(), p.layer))
        .collect();
    let mut to_add: Vec<(String, CopperLayer)> = Vec::new();
    for net_name in sch.nets.keys() {
        let Some(class) = sch.class_for_net(net_name) else {
            continue;
        };
        for layer in &class.pour_layers {
            if existing.contains(&(net_name.clone(), *layer)) {
                summary
                    .already_present
                    .push(format!("{net_name}/{}", layer_to_str(*layer)));
            } else {
                to_add.push((net_name.clone(), *layer));
            }
        }
    }
    drop(snap);
    for (net, layer) in to_add {
        project.add_pour(Pour {
            net: net.clone(),
            layer,
            thermal_relief: pcb_core::ThermalRelief::default(),
            stitching: pcb_core::StitchPolicy::None,
        });
        summary.added.push(format!("{net}/{}", layer_to_str(layer)));
    }
    summary
}

#[derive(Debug, Default)]
struct ClassPourSummary {
    added: Vec<String>,
    already_present: Vec<String>,
}

fn tool_auto_pour(project: &Project, _args: &Value) -> Result<Value, ToolError> {
    let summary = materialize_class_pours(project);
    project.log(
        ActivityLevel::Info,
        format!(
            "auto-pour: added {} pour(s), {} already present",
            summary.added.len(),
            summary.already_present.len(),
        ),
    );
    let text = format!(
        "auto-pour: added {} ({}); already present {} ({})",
        summary.added.len(),
        if summary.added.is_empty() {
            "—".to_string()
        } else {
            summary.added.join(", ")
        },
        summary.already_present.len(),
        if summary.already_present.is_empty() {
            "—".to_string()
        } else {
            summary.already_present.join(", ")
        },
    );
    Ok(text_result(text).with_data(json!({
        "added": summary.added,
        "already_present": summary.already_present,
    })))
}

#[derive(Debug, Deserialize)]
struct FabPackArgs {
    /// Provider name: "jlcpcb" / "pcbway" / "generic". Default
    /// jlcpcb because that's what most non-technical users will pick.
    #[serde(default)]
    fab: Option<String>,
    /// Directory to drop the zip in. Default `~/Downloads`.
    #[serde(default)]
    out_dir: Option<String>,
}

fn tool_fab_pack(project: &Project, args: &Value) -> Result<Value, ToolError> {
    let input: FabPackArgs = serde_json::from_value(args.clone())
        .map_err(|e| ToolError::invalid_params(format!("pack: {e}")))?;

    let provider = match input.fab.as_deref() {
        None => pcb_fab::Provider::Jlcpcb,
        Some(s) => pcb_fab::Provider::from_name(s).ok_or_else(|| {
            ToolError::invalid_params(format!(
                "pack: unknown fab `{s}` — supported: jlcpcb, pcbway, generic"
            ))
        })?,
    };

    let out_dir = match input.out_dir {
        Some(p) => std::path::PathBuf::from(p),
        None => std::env::var_os("HOME").map_or_else(
            || std::path::PathBuf::from("/tmp"),
            |h| std::path::PathBuf::from(h).join("Downloads"),
        ),
    };

    let report = pcb_fab::pack(project, provider, &out_dir)
        .map_err(|e| ToolError::invalid_params(format!("pack: {e}")))?;

    let summary = if report.blocking {
        format!(
            "pack: NOT READY — wrote {} ({} files), but blocking issues: {}",
            report.zip_path.display(),
            report.files.len(),
            report.blocking_reasons.join("; "),
        )
    } else {
        format!(
            "pack: ready — wrote {} ({} files); upload to {}",
            report.zip_path.display(),
            report.files.len(),
            report.provider,
        )
    };

    project.log(
        ActivityLevel::Info,
        format!(
            "fab.pack: {} → {} ({} blocking)",
            report.provider,
            report.zip_path.display(),
            report.blocking_reasons.len(),
        ),
    );
    Ok(text_result(summary).with_data(serde_json::to_value(&report).unwrap_or(json!({}))))
}

fn tool_erc_run(project: &Project, _args: &Value) -> Result<Value, ToolError> {
    let snap = project.read();
    let report = pcb_erc::run(
        snap.board(),
        snap.schematic(),
        &pcb_erc::ErcOptions::default(),
    );
    drop(snap);

    project.log(
        ActivityLevel::Info,
        format!(
            "erc.run: {} error(s), {} warning(s)",
            report.error_count, report.warning_count
        ),
    );

    // Surface the first ~6 violation messages inline so a one-shot
    // `erc` call gives the agent something actionable without having
    // to read the structured `violations` array.
    let mut summary = format!(
        "ERC: {} error(s), {} warning(s)",
        report.error_count, report.warning_count,
    );
    for v in report.violations.iter().take(6) {
        summary.push_str(&format!("\n  [{:?}] {}", v.severity, v.message,));
    }
    if report.violations.len() > 6 {
        summary.push_str(&format!(
            "\n  ... and {} more (see structuredContent.violations)",
            report.violations.len() - 6,
        ));
    }
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
    let stem = input.name.unwrap_or_else(|| snap.name().to_string());
    let out_dir = std::path::PathBuf::from(&input.out_dir);

    let paths =
        pcb_gerber::write_fab_pack(snap.board(), &stem, &out_dir).map_err(|e| ToolError {
            code: error_code::INTERNAL_ERROR,
            message: format!("write_fab_pack: {e}"),
        })?;

    let path_strings: Vec<String> = paths.iter().map(|p| p.display().to_string()).collect();
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

/// Builds the tool result envelope returned to the script API caller.
/// The text content is what the agent sees verbatim; `with_data`
/// attaches structured metadata that the UI bridge or follow-up tool
/// calls can read.
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
