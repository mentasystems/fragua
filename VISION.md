# pcb — Vision

An AI-native PCB design tool. The agent does the work; the human watches it happen
in real time, and steps in to redirect, mark up, or correct.

The product target is *pencil.dev for hardware* — a single app you open, that
exposes an MCP server for an AI agent (Claude Code first), and whose UI exists
to make the agent's reasoning and output visible and steerable.

## What the product is

A desktop app that:

1. Hosts an **MCP server** for AI agents (initial client: Claude Code).
2. Devotes ~80% of the UI to **observation**: the human watches, in real time,
   what the agent is doing — schematic forming, components placing, traces
   routing, DRC running.
3. Devotes the remaining UI to **steering**: the human can drag components,
   draw with a pen tool to annotate the canvas, point at things, and feed
   feedback back to the agent.

The agent drives the workflow end to end. The human supervises.

## End-to-end pipeline

From a natural-language prompt (or code description) all the way to
manufacturing files, without ever launching KiCad, FreeRouting, or any
external CAD tool:

1. **Intake** — prompt or code in. The agent restates the requirements.
2. **Clarification** — the agent asks the human for any missing decisions
   (target voltage, connector type, dimensions, mechanical constraints).
3. **Schematic synthesis** — the agent produces a schematic (symbols + nets).
   Visible live in the UI as it grows.
4. **Recommendations** — the agent surfaces design suggestions (decoupling,
   protection, ESD, test points) and lets the human accept/reject each.
5. **Auxiliary components** — pull-ups, decoupling caps, indicator LEDs,
   anything implied by the chosen ICs.
6. **Board sizing** — derived from component footprints + connectors +
   mechanical constraints, or set by the human.
7. **Placement** — agent places footprints. The human can drag any component
   at any time; the agent re-plans around fixed positions.
8. **Auto-routing** — native router lays traces and vias. The human watches
   the routing progress live, layer by layer, net by net.
9. **Corrections** — DRC runs continuously. Violations are highlighted on
   the canvas; the agent proposes fixes; the human approves or overrides.
10. **Output** — Gerber RS-274X for each layer, Excellon drill files, BOM
    (CSV), pick-and-place file. All produced in-process.

## Non-negotiables

- **No external binaries.** No `kicad-cli`, no `java -jar freerouting.jar`,
  no `pcbnew.so`, no shell-out to anything. Every file format read or
  written is implemented in our own Rust code.
- **No KiCad/FreeRouting wrapper crates.** External projects are reference
  material — we read their docs and source for understanding, not for
  linking. Generic Rust crates (geometry, serialization, UI) are fine.
- **MCP-first.** The MCP surface is the primary API. The desktop UI is a
  rich observer/editor on top of the same in-memory project that MCP tools
  mutate.
- **The human is never blocked.** Long-running operations (routing, DRC)
  stream progress to the UI and can be paused, redirected, or cancelled.

## Stack

- **Rust** for everything: core data model, parsers, router, DRC, Gerber
  writer, MCP server, Tauri app host.
- **Tauri 2** as the desktop shell.
- **TypeScript + Vite** for the frontend; canvas/SVG/WebGL for board
  rendering and the pen-tool annotation surface.

## What we are NOT building (now)

- A general-purpose schematic/PCB editor that competes with KiCad on
  features. Human editing exists to *correct* the agent, not to design
  from scratch by hand.
- A SPICE simulator, signal-integrity tool, or thermal analyzer.
- 3D rendering of the board. Top-down 2D is enough for the agent loop.
- Plugin/scripting APIs beyond MCP.
