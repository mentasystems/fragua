# fragua — web redesign brief

Context pack for redesigning fragua's web UI into something beautiful and
modern. Self-contained: hand this to a design session (or build from it
directly).

## What fragua is
fragua is an **AI-native PCB autorouter and layout tool** (Rust core). A
user describes/loads a board; fragua places components, auto-routes traces
across copper layers, runs DRC/ERC, manages a component library, and
exports manufacturing files (JLCPCB / ODB++). The web UI is the **cockpit**:
it visualizes the board and drives every operation.

It is a *technical tool for makers/EEs*, but it should feel **crafted,
fast, and confident** — closer to Linear / Vercel / Figma polish than to a
dated EDA tool. The current UI (vanilla TS + hand-rolled CSS) works but
looks utilitarian; the owner wants a genuinely nice ("chula") web.

## Core jobs the UI must do (feature map)
1. **Board canvas (the hero).** A pan/zoom SVG viewport rendering the PCB:
   board outline, copper layers (F.Cu / inner planes / B.Cu), traces, pads,
   vias, GND copper pours, keepouts, and **DRC error markers** overlaid on
   the geometry. Smooth pan/zoom, layer visibility toggles, crisp at any
   zoom. This is 70% of the screen and must look stunning.
2. **View switch:** `board` ↔ `schematic` ↔ `review` (library review).
3. **Autoroute:** a primary action button with **live progress**
   (idle → running → done/error) streamed from the backend. Show route
   score (unrouted nets, collisions, wire length).
4. **DRC / ERC results:** a panel listing errors/warnings by kind, each
   clickable to fly the canvas to its marker. Clean severity styling.
5. **Component library:** a pane of **cards** — each a component with a
   **footprint thumbnail**, value/MPN/LCSC badges, GND-pad count, a
   draggable **placement-margin** handle, and **datasheet photo
   attachments**. Plus a **review queue** for pending library entries
   (confirm "Save to library" / discard), gated on having a photo attached.
6. **Component detail modal:** click a part → its pads, datasheet image,
   description.
7. **Palette:** available components to drop onto the board.
8. **Export:** JLCPCB pack + ODB++ buttons → produce a zip; surface
   READY / NOT-READY with blocking DRC/ERC counts.
9. **Activity / event log:** a live feed of backend events.
10. **Command surface:** fragua is driven by a small **script DSL** (verbs:
    `outline, route, pour, pack, drc, erc, place, move, lib, …`). Expose a
    clean command/console — ideally an **AI chat / command bar** befitting
    an "AI-native" tool (natural-language → DSL), with the raw console
    available.

## Backend API the UI talks to (keep — redesign is frontend-only)
The Rust server (default `http://127.0.0.1:7878`) exposes:
- `POST /script` — body `{"script": "<DSL>"}` → runs verbs, returns a text
  result + structured content. This is how every action runs (route, pour,
  pack, drc, erc, place, move, library ops, etc.).
- `GET /screenshot?view=board&width=N` → PNG raster of the current board
  (the existing renderer; the redesign can keep using SVG live render or
  this raster).
- **Server-sent events** stream of `AnyEvent`:
  `ProjectChanged, FootprintAdded{reference}, FootprintMoved,
  FootprintRemoved, OutlineChanged, SymbolAdded{reference}, NetChanged,
  RoutingChanged, PlacementProgress, PaletteChanged, LibraryChanged{count},
  PendingLibraryChanged{count}` + an `Activity{level,message}` log channel.
  The UI re-fetches/repaints on these.
- Library/project state is fetched via `/script` verbs
  (`library_state`, `library_review_state`, `component_info`,
  `pending_library_entries`, etc.) returning JSON.

## Current tech (replace the look, keep the contract)
- Vite + **vanilla TypeScript** (`frontend/src/main.ts` ~60 KB,
  `styles.css` ~23 KB), custom SVG rendering + pan/zoom, optional Tauri
  shell. Free to re-pick the stack (React/Svelte/Solid + Tailwind, etc.)
  as long as it stays a static SPA hitting the same HTTP API and stays
  light (this also ships in a Tauri desktop wrapper).

## Design direction
- **Aesthetic:** dark, high-contrast, technical-elegant. The PCB greens/
  golds of copper against a near-black canvas; a restrained accent (electric
  teal/lime) for primary actions and "routed/clean" status. Monospace for
  net names, DRC kinds, coordinates; a crisp humanist sans for chrome.
- **Layout:** board canvas as a full-bleed hero; a slim left rail (views +
  layers), a right inspector (DRC/ERC, library, export, score), a bottom
  command/chat bar. Panels collapsible, remembered.
- **Feel:** instant, no jank; buttery pan/zoom; live status that reads at a
  glance (unrouted/collisions/score as a always-visible health strip);
  delightful micro-interactions on route-done (e.g. a sweep highlighting
  newly-laid copper).
- **Hero moment:** "Auto Route" → live progress → the board fills with
  copper and a clean ✅ score. Make that moment feel great.
- **Don't:** generic admin-template look, heavy chrome, tiny dense controls,
  modal-soup. Keep it spacious, legible, confident.

## Deliverable
A redesigned static SPA (same backend contract) that an EE/maker would call
beautiful — the board front-and-center, library + DRC + export as clean
inspectors, an AI command bar, and a standout autoroute moment.
