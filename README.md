# fragua

[![CI](https://github.com/kidandcat/fragua/actions/workflows/ci.yml/badge.svg)](https://github.com/kidandcat/fragua/actions/workflows/ci.yml)
[![License: MIT](https://img.shields.io/badge/license-MIT-d6905b.svg)](LICENSE)
[![Landing](https://img.shields.io/badge/landing-kidandcat.github.io%2Ffragua-d6905b)](https://kidandcat.github.io/fragua/)

AI-native PCB design tool. The agent does the work, the human watches and steers.

- 🌐 Landing: <https://kidandcat.github.io/fragua/>
- 🧭 [VISION.md](VISION.md) — what we are building and why
- 🏗️ [ARCHITECTURE.md](ARCHITECTURE.md) — the stack and crate layout
- 🤝 [CONTRIBUTING.md](CONTRIBUTING.md) — how to help

## Status

End-to-end agent loop, schematic → board → fab-ready zip:

- `pcb-core`: project model (schematic, board, library, pours), nm
  fixed-point geometry, tokio broadcast event bus, JSON persistence
  (`.fragua` files; legacy `.json` still loads).
- `pcb-script`: line-oriented agent DSL — `lib`, `sym`, `net`, `class`,
  `palette`, `place`, `auto-place`, `route`, `erc`, `drc`, `auto-pour`,
  `pack`. The full reference is printed at app launch and served at
  `GET /`.
- `pcb-router`: A* on a 2-layer grid + rip-up-and-reroute + negotiated
  congestion + Steiner-ish multi-source. Honours per-net `NetClass`
  for trace width / clearance.
- `pcb-placer`: simulated annealing on HPWL + soft gap penalty +
  rasterised pad-bbox congestion proxy.
- `pcb-drc`: pad/trace clearance, drill, edge clearance, narrow trace,
  routing efficiency. Per-net class overrides supported.
- `pcb-erc`: floating pin/net, duplicate pin, orphan symbol, phantom
  net; role-based: multiple drivers, unpowered power net, undriven
  input. Heuristic: missing decoupling cap, missing I²C pull-up.
- `pcb-fab`: `Provider { Jlcpcb, Pcbway, Generic }` + manufacturing-DRC
  (min trace, drill, annular ring, board size) + per-provider BOM and
  CPL formats + `pack(...)` that ships a single ready-to-upload `.zip`.
- `pcb-gerber`: RS-274X writer (rounded outlines emit arcs), Excellon
  drill files, BOM + pick-and-place CSV.
- `pcb-render`: Board → SVG. Substrate (with rounded corners), copper,
  silkscreen (Hershey strokes; auto-relocation when a label would
  spill off the outline), DRC marker overlay.
- `src-tauri` + `frontend`: Tauri 2 shell. Hosts a stateless local HTTP
  API on `127.0.0.1:7878` (`POST /script`, `POST /save`, `GET /` for
  the script reference). Frontend pans/zooms an SVG of the live state
  and surfaces the activity log.

`cargo test --workspace` is green.

## Install

One-liner (macOS arm64/x64, Linux x64):

```sh
curl -fsSL https://raw.githubusercontent.com/kidandcat/fragua/master/scripts/install.sh | sh
```

Drops the `fragua` binary in `/usr/local/bin` (or `~/.local/bin` if it
can't write there). Windows users: grab `fragua-<ver>-windows-x64.zip`
from the [releases page](https://github.com/kidandcat/fragua/releases/latest).

Then just tell your AI to design the hardware using the `fragua` CLI —
it launches the window, exposes the HTTP script API on
`127.0.0.1:7878`, and the agent drives the rest.

## Run it

```sh
# Build the frontend bundle once (release build embeds it).
npm --prefix frontend install
npm --prefix frontend run build

# Run the desktop app.
cargo run --release --bin fragua

# …or open an existing project:
cargo run --release --bin fragua /path/to/project.fragua
```

The window opens and the local HTTP API starts on `127.0.0.1:7878`.

## Drive it from an agent

Stateless HTTP — every request is independent. From any tool that can
make HTTP calls (Claude Code, GPT, a shell loop):

```sh
# Discover the full action surface.
curl -s http://127.0.0.1:7878/

# Run a multi-line script.
curl -s http://127.0.0.1:7878/script \
  -H 'content-type: application/json' \
  -d '{"script": "outline 80 30 radius=2\nstatus"}'

# Persist when launched without a file argument.
curl -s http://127.0.0.1:7878/save \
  -H 'content-type: application/json' \
  -d '{"path": "/tmp/board.fragua"}'
```

Replies are `text/plain`: per-line outcomes in the form
`[L<n> ok|FAIL <tool>] <text>`, plus a warning when the session is
memory-only.

## End-to-end recipe

```text
class ground pour=both
class power width=0.4

sym U1 ic key=esp32_s3_zero
  pin 1 L 3V3 role=power_in
  pin 2 L GND role=power_in
  ...
sym C1 capacitor key=c_0603 lcsc=C14663
sym R1 resistor key=r_0603 lcsc=C25804

net GND  U1.GND C1.2 R1.2 class=ground
net +3V3 U1.3V3 C1.1 class=power

erc

palette U1 esp32_s3_zero
palette C1 c_0603 value=100nF
palette R1 r_0603 value=10k
place U1 25 15
place C1 35 15
place R1 35 25

auto-place R1 C1 seed=42
route
pack fab=jlcpcb out=/tmp
```

The final line writes `/tmp/<project>-jlcpcb.zip` ready to upload.
</content>
</invoke>