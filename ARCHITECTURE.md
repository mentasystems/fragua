# pcb — Architecture

This document maps VISION.md onto a concrete Rust workspace, an
in-process data flow, and the implementation as it stands.

## Process model

One process. One Tauri app. Inside it:

- A **shared in-memory project** (`pcb-core::Project`) — single source of
  truth for the schematic, board, nets, design rules, and routing state.
- A **local HTTP API task** (`POST /script` on `127.0.0.1:7878`,
  `text/plain` responses) — agents and humans run script verbs through
  this single endpoint; the server is stateless beyond the live project.
- A **Tauri command surface** — JS-callable handlers that read the same
  project and stream change events to the frontend.
- A **frontend** (Vite + TS) — renders the project, listens for change
  events, and sends user actions (drag, annotate, override) back through
  Tauri commands, which in turn mutate the project and notify any waiting
  agent.

The agent and the human edit the same `Project`. Every mutation, regardless
of source, emits a change event consumed by the UI.

## Workspace layout

```
pcb/
├── Cargo.toml             workspace manifest
├── rust-toolchain.toml    pinned toolchain
├── crates/
│   ├── pcb-core/          project model, geometry, units, ids, change events
│   ├── pcb-router/        autorouting (A* + RR&R + negotiated congestion)
│   ├── pcb-placer/        simulated-annealing footprint placer
│   ├── pcb-drc/           design rule check (geometry-based)
│   ├── pcb-erc/           electrical rule check (schematic-side)
│   ├── pcb-fab/           fab provider abstraction (JLCPCB / PCBWay / Generic)
│   ├── pcb-gerber/        RS-274X + Excellon writer + BOM/CPL CSV
│   ├── pcb-render/        SVG render of the board (substrate, copper, silk)
│   └── pcb-script/        line-oriented DSL + tool dispatch + reference docs
├── src-tauri/             Tauri binary crate (host + HTTP API)
├── frontend/              Vite + TypeScript UI
├── VISION.md
├── ARCHITECTURE.md
└── README.md
```

## Crate responsibilities

### `pcb-core`
The model. Owns:
- Units (mm, fixed-point internal representation, `Length(i64)` in nm).
- Geometry primitives (point, segment, rect).
- `Project { schematic, board, library, save_path }` plus an event bus.
- `Schematic { symbols, nets, net_classes }` — symbols carry pins with
  electrical roles (Passive / Input / Output / Bidir / PowerOut / PowerIn).
- `Board { footprints, traces, vias, pours, silk, outline,
  outline_corner_radius }`.
- `Library` — disk-backed component catalogue with attachments
  (photos / datasheets), `lcsc_id` and `mpn` fields for fab BOMs.
- `Hershey` stroke font for silkscreen text.

No I/O on the critical path. File save/load lives behind `Project::load_from_path`
/ `save_to_path` and is content-based (JSON), so legacy `.json` and the
canonical `.fragua` extension both load.

### `pcb-router`
Auto-routing. Receives a `Board`, produces traces and vias.
- Multi-source A* with bend penalty and via cost.
- Rip-up-and-reroute driver: re-orders failed/inefficient nets to the
  front, accumulates per-cell congestion bias across iterations.
- Steiner-style construction: same-net `Trace` cells are sources at g=0
  so later spokes branch off the existing trunk.
- Per-net `NetOverride` for `trace_width` / `clearance` from net classes.

### `pcb-placer`
Simulated-annealing footprint placer. Score = HPWL + soft body-to-body
gap penalty + congestion-overflow proxy (rasterised pad-bbox grid).
Caller passes the list of refs that may move; everything else stays
pinned. Edge-mounted parts are constrained to the outline.

### `pcb-drc`
Geometric design rule check over a `Board`: clearance (per-net via
`NetOverride`), track width, drill sizes, via annular ring, edge
clearance, unconnected pads, routing efficiency (`actual / HPWL`).
Emits violations with positions so the UI can highlight them.

### `pcb-erc`
Schematic-side validation. Strict checks: floating pin/net, duplicate
pin, empty net, orphan symbol, phantom net (board pad on a net the
schematic doesn't declare). Role-based: multiple drivers, unpowered
power net, undriven input. Heuristic (opt-in): missing decoupling cap
near a PowerIn pin, missing pull-up on I²C nets.

### `pcb-fab`
Fab-house provider abstraction. `Provider { Jlcpcb, Pcbway, Generic }`
with per-house `FabRules` (min trace, drill, annular, board size),
BOM and CPL formatters, and a `pack(project, provider, out_dir)` entry
point that runs ERC + DRC + manufacturing-DRC and ships a single
`.zip` ready to upload.

### `pcb-gerber`
Manufacturing output. Writes one Gerber file per copper/mask/silk/edge
layer (RS-274X), Excellon drill files (plated and non-plated), generic
BOM and pick-and-place CSV. Pure writer, no parser. Rounded outlines
emit straight segments + CCW quarter-arcs (`G75*` multi-quadrant).

### `pcb-render`
Board rendering. Produces SVG (the frontend can pan/zoom and
attach interactive handlers). Renders substrate (with rounded corners),
copper layers, vias, pads with labels, silk strokes (footprint-attached
and board-level), DRC marker overlay. Silk text whose bbox would clip
the outline is auto-relocated to a body-relative fallback.

### `pcb-script`
The agent surface. Single line-oriented DSL: `verb args [kv=val]`,
indented sub-lines for blocks (`lib`/`sym`). The parser produces
`Cmd { tool, args }` records; `dispatch` routes them through the rest
of the workspace. The `script_reference()` string printed at startup
and served at `GET /` documents every verb, the example flow, and the
recommended pipeline (ERC → power planes → auto-place → route → pack).

### `src-tauri`
Tauri binary. Owns the `Project`, hosts the HTTP API on
`127.0.0.1:7878` (3 endpoints: `GET /` for the script reference,
`POST /script` for the agent's tool calls, `POST /save` to bind autosave),
registers Tauri commands for the frontend, forwards events.

### `frontend`
TypeScript + Vite. SVG canvas as the centrepiece (pan/zoom, click to
inspect a component). Side panels: activity log, library, palette,
DRC violation list. Default panes are hidden; the topbar tabs flip
them open.

## Data flow: a typical agent action

1. Agent sends `POST /script` with a multi-line script body.
2. The HTTP handler in `src-tauri` dispatches each line via
   `pcb_script::tools::dispatch`.
3. Each dispatched tool validates inputs and calls a `Project` mutator.
4. `Project` emits the matching `Event` (e.g. `FootprintAdded`).
5. The Tauri event pump forwards each event to the webview as `pcb://event`.
6. The frontend re-fetches `project_state` via a Tauri command and repaints.
7. If the human drags a footprint, the frontend calls
   `move_footprint(reference, x, y)` via Tauri, which goes through the
   same `Project::move_footprint_to` API the script tool uses; the agent
   sees the new position on the next `view`/`snap`/`status` call.

## Where we ended up vs the original plan

The plan documented an MCP server as the primary surface. We ran it that
way for a while, then dropped it: the agent the user runs (Claude Code)
already has tool-call + slash-command primitives, and a stateless local
HTTP endpoint replying in `text/plain` was easier for it to use than the
MCP framing. The endpoint lives on the same `127.0.0.1:7878` port the
MCP server used, just speaks plain HTTP now.

The script DSL exists for the same reason — small surface area, one verb
per concept, deterministic parsing. The agent reasons about the design;
the script just commits each step.

## Implementation phases (historical, for context)

The work landed roughly in this order:

1. Skeleton + pcb-core data model.
2. Stateless HTTP API replacing the original MCP server.
3. Pcb-gerber → first end-to-end fab pack from a placed-only board.
4. Pcb-drc with the core geometric checks.
5. Pcb-router (initial: per-net A*; today: RR&R + negotiated congestion +
   Steiner-ish multi-source).
6. Footprint silk + library attachments + photos.
7. Net classes + per-net trace_width / clearance.
8. Pin roles + role-based ERC checks (multiple drivers, unpowered nets).
9. Pcb-placer (simulated annealing on HPWL + gap penalty + congestion).
10. Pcb-fab provider abstraction + manufacturing-DRC + zip pack flow
    (JLCPCB / PCBWay / Generic).
11. Rounded board outlines + silk-text relocation.
12. Heuristic ERC checks (decoupling caps, I²C pull-ups).

Each step kept the human-visible end-to-end demo working — no long
construction phase with nothing to show.
</content>
</invoke>