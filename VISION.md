# pcb — Vision

An AI-native PCB design tool. The agent does the work; the human watches it happen
in real time, and steps in to redirect, mark up, or correct.

The product target is *pencil.dev for hardware* — a single app you open, that
exposes a local script API for an AI agent, and whose UI exists to make the
agent's reasoning and output visible and steerable.

## What the product is

A desktop app that:

1. Hosts a **local HTTP script API** for AI agents (`POST /script` on
   `127.0.0.1:7878`, `text/plain` responses, agent-friendly).
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
   Visible live in the UI as it grows. ERC validates as it goes.
4. **Recommendations** — the agent surfaces design suggestions (decoupling,
   protection, ESD, test points) and lets the human accept/reject each.
5. **Auxiliary components** — pull-ups, decoupling caps, indicator LEDs,
   anything implied by the chosen ICs.
6. **Board sizing** — derived from component footprints + connectors +
   mechanical constraints, or set by the human (`outline W H [radius=R]`).
7. **Placement** — agent places footprints; `auto-place` runs simulated
   annealing on movable parts. The human can drag any component at any time;
   the agent re-plans around fixed positions.
8. **Auto-routing** — native router (RR&R + negotiated congestion +
   Steiner-style multi-source A*) lays traces and vias. The human watches
   the routing progress live.
9. **Corrections** — DRC + manufacturing-DRC run continuously. Violations
   are highlighted on the canvas; the agent proposes fixes; the human
   approves or overrides.
10. **Output** — `pack [fab=jlcpcb|pcbway|generic]` ships Gerbers + drill +
    BOM + CPL + README in a single zip ready to upload. All produced
    in-process; no external CAD tool involved.

## Non-negotiables

- **No external binaries.** No `kicad-cli`, no `java -jar freerouting.jar`,
  no `pcbnew.so`, no shell-out to anything. Every file format read or
  written is implemented in our own Rust code.
- **No KiCad/FreeRouting wrapper crates.** External projects are reference
  material — we read their docs and source for understanding, not for
  linking. Generic Rust crates (geometry, serialization, UI) are fine.
- **Agent-first.** The script API is the primary surface. The desktop UI is a
  rich observer/editor on top of the same in-memory project that the script
  tools mutate.
- **The human is never blocked.** Long-running operations (routing, DRC)
  stream progress to the UI and can be paused, redirected, or cancelled.

## Stack

- **Rust** for everything: core data model, parsers, router, placer, DRC,
  ERC, Gerber writer, fab provider abstraction, Tauri app host.
- **Tauri 2** as the desktop shell.
- **TypeScript + Vite** for the frontend; SVG for board rendering and the
  pen-tool annotation surface.

## What we are NOT building (now)

- A general-purpose schematic/PCB editor that competes with KiCad on
  features. Human editing exists to *correct* the agent, not to design
  from scratch by hand.
- A SPICE simulator, signal-integrity tool, or thermal analyzer.
- 3D rendering of the board. Top-down 2D is enough for the agent loop.
- Plugin/scripting APIs beyond the script verb language.
</content>
</invoke>