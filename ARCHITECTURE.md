# pcb — Architecture

This document maps VISION.md onto a concrete Rust workspace, an
in-process data flow, and a phased implementation order.

## Process model

One process. One Tauri app. Inside it:

- A **shared in-memory project** (`pcb-core::Project`) — single source of
  truth for the schematic, board, nets, design rules, and routing state.
- An **MCP server** task — speaks MCP over stdio and/or SSE, exposes tools
  that read and mutate the project.
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
/Users/jairo/pcb/
├── Cargo.toml             workspace manifest
├── rust-toolchain.toml    pinned toolchain
├── crates/
│   ├── pcb-core/          project model, geometry, units, ids, change events
│   ├── pcb-router/        autorouting (grid + geometric, ours)
│   ├── pcb-drc/           design rule check (geometry-based, ours)
│   ├── pcb-gerber/        RS-274X + Excellon writer (ours)
│   ├── pcb-render/        SVG/PNG render of the board (ours)
│   └── pcb-mcp/           MCP server, tool definitions
├── src-tauri/             Tauri binary crate (host)
├── frontend/              Vite + TypeScript UI
├── VISION.md
├── ARCHITECTURE.md
└── README.md
```

## Crate responsibilities

### `pcb-core`
The model. Owns:
- Units (mils/mm, fixed-point internal representation).
- Geometry primitives (point, segment, polygon, arc).
- `Project { schematic, board, rules }`.
- `Schematic { symbols, nets, wires }`.
- `Board { layer_stack, footprints, traces, vias, zones, outline }`.
- `Component library` — we ship our own; KiCad libraries can be a *reference*
  for shapes but we re-author what we need internally.
- A change-event bus so `pcb-mcp`, the UI, and `pcb-router` can subscribe.

No I/O on the critical path. File import/export lives in submodules but
is optional — the canonical project lives in memory.

### `pcb-router`
Auto-routing. Receives a `Board` snapshot + ratsnest, produces traces and
vias, streams progress events. Phase 1 is grid-based A*/Lee on two layers
with via cost; later we evolve toward a geometric/rip-up-and-retry router.

### `pcb-drc`
Design rule check. Pure geometry over a `Board`: clearance, track width,
drill sizes, via annular ring, edge clearance, unconnected nets. Emits
violations with positions so the UI can highlight them.

### `pcb-gerber`
Manufacturing output. Writes one Gerber file per copper/mask/silk/paste
layer (RS-274X), Excellon drill files (plated and non-plated), CSV BOM,
and pick-and-place CSV. Pure writer, no parser needed.

### `pcb-render`
Board rendering. Produces SVG (preferred — the frontend can style and
animate it) and optional PNG. Used by MCP tools that need to attach a
visual, and by the UI as a fallback.

### `pcb-mcp`
The MCP server. Tools are thin: each one validates inputs, mutates
`Project` through `pcb-core` APIs, and returns a result. The agent does
the reasoning; tools are mechanical.

Initial tool set (will grow):
- `project.new` / `project.open` / `project.save`
- `schematic.add_symbol`, `schematic.add_wire`, `schematic.delete`
- `board.set_outline`, `board.set_layer_stack`, `board.set_rules`
- `placement.add`, `placement.move`, `placement.lock`
- `route.run`, `route.stop`
- `drc.run`
- `output.gerber`, `output.bom`, `output.pick_place`
- `view.snapshot` — returns SVG of current board state for the agent

### `src-tauri`
Tauri binary. Owns the `Project`, spawns the MCP server, registers Tauri
commands for the frontend, forwards change events.

### `frontend`
TypeScript + Vite. The board canvas is the centerpiece — likely SVG for
v0 (easy to style, accessible, easy to animate), with a path to WebGL if
we hit perf limits on large boards. Side panels: agent activity log,
component tree, DRC violations, design rule editor. Pen-tool overlay for
annotations is a separate canvas layer that captures strokes and feeds
them back to the agent as image attachments via MCP.

## Data flow: a typical agent action

1. Agent calls MCP tool `placement.add` with a footprint and position.
2. `pcb-mcp` validates and calls `pcb-core::Project::add_placement`.
3. `Project` emits `Event::PlacementAdded`.
4. Tauri's event bridge forwards the event to the frontend.
5. Frontend updates the canvas; the human sees the new component appear.
6. If the human drags it, the frontend calls a Tauri command, which calls
   `Project::move_placement`, which emits `Event::PlacementMoved` — and
   the agent, watching the project state through MCP, sees the override.

## Implementation phases

**Phase 0 — skeleton (where we are now)**
Workspace, empty crates, docs. `cargo check` passes.

**Phase 1 — minimum viable loop**
- `pcb-core` skeleton (Project, basic geometry, change events).
- `pcb-mcp` with `project.new`, `placement.add`, `view.snapshot`.
- Tauri shell that hosts the MCP server and renders the project.
- Frontend with an SVG canvas that listens to change events.
- Agent can: create a project, drop a few components, see them rendered.

**Phase 2 — schematic + nets**
- Schematic data model + a starter symbol library (we author ours).
- MCP tools to build a schematic.
- Frontend schematic view.

**Phase 3 — gerbers + BOM**
- `pcb-gerber` writes a complete fab pack from a placed-only board.
- Lets us validate the pipeline end-to-end before tackling the router.

**Phase 4 — DRC**
- `pcb-drc` with the core geometric checks.
- Frontend overlay for violations.

**Phase 5 — autorouter v0**
- Grid-based A*/Lee, two layers, basic vias.
- Streamed progress to the UI.

**Phase 6 — interaction tools**
- Drag-to-move with router re-plan.
- Pen-tool annotation surface.
- Pin/lock components.

**Phase 7+ — autorouter evolution, multi-layer, advanced DRC, polish.**

We pick this order because each phase produces something the human can
*see working* end-to-end, even if narrow. We never have a 6-month
construction phase with nothing to demo.
