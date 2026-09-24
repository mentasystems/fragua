# fragua — Architecture

This document maps [VISION.md](VISION.md) onto the Go tree and the
in-process data flow (v1.2+).

## Process model

One process. One static binary (`cmd/fragua`). Inside it:

- A **shared in-memory project** (`internal/core.Project`) — single source of
  truth for the schematic, board, nets, design rules, stackup, and routing
  state.
- A **local HTTP API** on `127.0.0.1:7878` (override: `FRAGUA_API_ADDR`) —
  agents and humans run script verbs through `POST /script`. Also: `GET /`
  and `GET /help` (usage + script reference), `GET /health`, `GET /state`,
  `GET /events`, `GET /screenshot`, `GET /schematic`, `GET /summary`,
  `GET /drc`, `GET /erc`, `GET /part`, `GET /ui/`, `POST /save`,
  `POST /cancel`.
- An **embedded browser UI** (`internal/host/ui`) — live board and schematic
  as inline SVG, layer panel, net inspector, DRC/ERC list, progress with
  cancel, script console. The agent and the human edit the same `Project`.
  Every mutation emits a change event consumed by the UI; long operations
  stream progress events of their own.

CLI: **`fragua run [file.fragua]`** starts the server and opens the browser.
Bare `fragua` (or `fragua help`) prints usage + the full script reference
and exits so agents can discover the surface without starting the server.

## Layout

```
pcb/   (repo: mentasystems/fragua)
├── go.mod
├── cmd/
│   ├── fragua/            HTTP host + embedded UI
│   ├── board-regen/       keep-place, Go-route, JLCPCB pack
│   ├── drc-print/
│   ├── fab-dump/
│   └── fecha-regen/
├── internal/
│   ├── core/              project, board, schematic, library, geometry (nm)
│   ├── script/            DSL parse + tool dispatch + help text
│   ├── router/            Theta*, fanout/slots, RR&R, negotiate, organic, stitch; opt-in topo
│   ├── placer/            ePlace global + SA legalisation + decoupling ring + edge snap
│   ├── drc/  erc/  si/
│   ├── fab/  gerber/  odb/
│   ├── render/            board + schematic SVG, static 3D PNG
│   └── host/              HTTP API + UI
│       └── ui/            index.html, app.css, js/ (go:embed, no build step)
├── scripts/install.sh
├── stress/                RP2040 open-hardware stress campaign
├── docs/                  GitHub Pages landing
├── VISION.md
├── ARCHITECTURE.md
└── README.md
```

## Package responsibilities

### `internal/core`
The model. Units are `Length(i64)` nanometres. Owns `Project`, `Board`,
`Schematic`, library, rule areas, fab floors (`FabRules` — JLCPCB 2L/4L),
stackup (`Default2Layer` / `Default4Layer`: F / GND / +3V3 / B), pours,
and the event bus. File I/O is JSON (`.fragua` / legacy `.json`).

### `internal/router`
Auto-routing. Default (`engine=grid` or unset) stages: fab ceiling → QFN
escape-slot matching + leftover far-ring → Prim/Theta* tree → RR&R
(both-or-neither) → negotiate leftovers → organic string-pull → pour stitch.
`engine=topo` is an opt-in Delaunay homotopy / rubber-band engine (ported
from rust `topo.rs`); it re-routes the whole board and is not the agent
default. Via-in-pad is an explicit exception
(`escape via-in-pad REF.PAD`), not the default. A net whose class width does
not fit its escape retries with a short neck near its own pads (nominal width
everywhere else), then one width tier down; both are counted in `Summary()`,
never silent.

A multi-terminal net is one Prim tree, and the shape of that tree — which
terminal seeds it and which one it reaches for next — decides what copper the
last branch has to get past. The first pass builds one shape (all seeds for a
net of six terminals or fewer, as it always has); the two last-chance passes
let a net still open try up to `maxTreeAttempts` of them (`treeAttempts`),
including trunk-first growth, and give a branch that found no path one more
look once the rest of the tree is down. The shapes are deliberately *not* spent
inside RR&R or the negotiator: those try a leftover once per rip set, and a
fruitless A* is the most expensive search there is.

Two things keep the greedy pass honest. Pads are filed in a millimetre-tile
index (`padIndex`), so the clearance query A* runs on every step and every
Theta* chord costs a handful of tests instead of a scan of every pad on the
board — one cross-board hop went from seconds to tens of milliseconds. And a
cell inside the escape corridor of a pad whose net is still unrouted carries a
toll (`pendingPenalty`), so a rail crossing the board does not take the
shortcut straight over an unrouted header's pin face and strand the connector.

Three invariants the engine owes its callers:

- **Every run is bounded.** `ClampBudget` normalises `max_seconds`: absent,
  zero, negative and non-finite all mean the 600 s default, which is also the
  ceiling for any single call. The search is anytime (per-net caps, deadline
  checks inside A*), so a run that hits the clock returns the tree it has. A
  net's first-pass cap is a share of the clock still left (`netBudget`), never
  a fixed number of seconds — and a net that ran out of clock is reported as
  `budget`, never as `unreachable`. The repair passes slice the same way
  (`repairBudget`), per leftover in RR&R and per rip set in the negotiator, so
  one hard net cannot spend the budget the rest of the queue is waiting on.
- **A net that does not finish leaves no copper.** The partial passes
  (`routeDirect`, `routeClearHops`) commit their hops as they go; `routeNetAt`
  rolls the board back when the net dies later, so a failed net never ships
  dangling stubs and a retry never lays a second copy of them.
- **Committed copper is legal copper.** Nothing is kept that DRC would
  reject: `copperClearanceFrom` checks new traces against traces, pads *and*
  vias; `viaClearanceFrom` checks new barrels against all three; `viaSiteOK`
  gates every layer change on the fab hole-to-hole gap and on room for the
  annulus. A through-hole pad is copper on every layer, so the router lands
  on it from either side and owes it no via.

### `internal/placer`
ePlace global stage (Poisson/DCT electrostatics + Nesterov, ported from
rust `global.rs`) then simulated-annealing legalisation, decoupling-ring
seating for passives, edge-mount snap. The SA-only path (`global=0`) is
deterministic for a fixed seed; ePlace itself has no RNG.

### `internal/drc` / `internal/erc`
Geometric DRC (clearance, drill, edge, net split, unconnected pads) and
schematic ERC (floating pins, drivers, power rails) plus heuristics.

### `internal/si`
Signal-integrity audit behind the `si-check` verb: impedance deviation per
routed segment (closed form, via `internal/impedance`), return-path gaps
against the nearest plane layer, diff-pair skew and via budget. Read-only,
same `Report`/`Violation` shape as DRC/ERC. A deviating width covering only a
sliver of the net is an escape neck: warning, not error.

### `internal/fab` + `internal/gerber` + `internal/odb`
`pack fab=jlcpcb` writes Gerber + Excellon + BOM/CPL zip. DRC for the pack
uses the board's `FabRules` (JLCPCB mins). ODB++ is a subset writer.

### `internal/script`
One verb per line. `internal/script/usage.go` is the agent-facing
reference (also `GET /` and `fragua help`).

### `internal/host`
`cmd/fragua` embeds `ui/` and serves it at `/ui/`. Loopback only. Beyond
`/script`, `/state`, `/events`, `/screenshot` and `/save`, the observer UI
is fed by `GET /drc`, `GET /erc` (violations as JSON, each with a stable
`id` and, where the check knows one, a location), `GET /summary` (the whole
status bar in one cheap read), `GET /schematic`, `GET /part?key=` (the
disk library entry behind a footprint — description, datasheet, LCSC id)
and `POST /cancel`.

`/summary` and `/drc` take the project read lock, so they queue behind a
long op the way everything else does; `/cancel` deliberately does not —
`Project.Ops()` is lock-free, or a cancel could never reach the route it is
trying to stop.

## The observer UI

`internal/host/ui/` is one static bundle: `index.html`, `app.css` and
dependency-free ES modules under `js/` (`api.js` HTTP + SSE, `canvas.js`
the SVG board, `panels.js` inspector/checks/layers/log, `app.js` wiring).
No framework, no build step, no CDN — it must work offline, and
`go:embed ui/*` ships it inside the binary. A `internal/host/ui` directory
on disk wins over the embedded copy, so editing the UI needs no rebuild.

The page is 80% observation, 20% steering, per [VISION.md](VISION.md):

- **Canvas.** The SVG from `/screenshot` is *injected*, not an `<img>`, so
  every element is addressable. Pan and zoom are a transform on a `g.vp`
  wrapper the UI inserts around the render's root group — never a refetch.
- **Steering.** Dragging a footprint rewrites its group transform live and,
  on drop, issues `move REF X Y` snapped to 0.1 mm through `/script`; `R`
  issues `rotate`. Every line the UI sends is echoed in the log, so the
  gesture teaches the verb.
- **Long ops.** `op_start` / `progress` / `op_end` events drive the bar. The
  canvas does *not* refresh while an op is in flight: the op holds the write
  lock, so a refresh would only queue. It catches up on `op_end`.
- **Checks.** A board change invalidates the last DRC/ERC run — the findings
  describe a board that no longer exists.

### SVG data-attribute contract

`internal/render` is what makes the canvas addressable. The board document
guarantees:

| Attribute | On | Meaning |
|-----------|----|---------|
| `data-root="1"` | one group | the Y-flip group; pan/zoom wraps it |
| `data-view-mm` | `<svg>` | `x y w h` of the view in board mm |
| `data-copper-layers` | `<svg>` | copper layer count |
| `data-layer` | groups **and** pads/pours | `F.Cu`, `In1.Cu`, `B.Cu`, `pours`, `vias`, `drills`, `mask`, `silk`, `pad-names`, `edge`, `ratsnest`, `drc`, `footprints`, `substrate`, `background` |
| `data-kind="copper"` + `data-index` | copper groups | index into the stackup, so the UI can list it top-down |
| `data-ref`, `data-value`, `data-key`, `data-side` | footprint group | the part |
| `data-pad`, `data-pad-name`, `data-net`, `data-layer`, `data-through` | pad rect | `data-through` marks a drilled pad: copper on every layer, so layer toggles must not hide it |
| `data-id`, `data-net` | trace lines, via circles | selection and net highlight |
| `data-net` | pour paths, ratsnest lines, mask openings | net highlight |
| `data-marker`, `data-severity` | DRC marker group | matches the `id` in `/drc` |

Layer visibility is "hide everything with this `data-layer`", which is why
pads carry their own — hiding `F.Cu` has to take the top pads with it.

The `drc` group ships empty from the renderer and the UI fills it from
`/drc` (markers are counter-scaled so they stay the same size on screen).
`GET /screenshot?drc=1` bakes them in server-side instead, for a
screenshot that has to stand alone.

### 3D product shot

`fragua render` (alias `fragua render3d`) writes a PNG from the same board
model: extruded outline with soldermask and an FR-4 edge, top copper, silk,
drills, and box bodies guessed from the footprint when no CAD model exists.
It is a software z-buffer in `internal/render` — no browser, no GPU, no
external binary. The live canvas stays the 2D SVG.

The schematic (`/schematic`) is the same idea: `data-sym`, `data-kind`,
`data-pin`, `data-pin-name`, `data-role`, `data-net`. It is a deterministic
shelf-packed sheet of symbol bodies with a net *flag* per pin rather than
drawn wiring — a label next to every pin always reads, where auto-routed
wires on a generated layout do not.

## Data flow

```
agent  --POST /script-->  script.RunScript  -->  core.Project
human  --browser /ui/-->  same
                              |
                              +--> router / placer / drc / fab
                              +--> events --> UI refresh
                              +--> gerber.WriteFabPack --> zip
```

One writer process per board file. No database.
