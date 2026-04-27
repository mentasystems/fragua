# pcb

AI-native PCB design tool. The agent does the work, the human watches and steers.

- See [VISION.md](VISION.md) for what we are building and why.
- See [ARCHITECTURE.md](ARCHITECTURE.md) for the stack, layout, and phases.

## Status

Phase 1 MVP — agent and human share a live `Project`:

- `pcb-core`: project model (footprints, pads, layers), nm fixed-point
  geometry, tokio broadcast event bus.
- `pcb-render`: `Board` → SVG (Y-up coordinate system, dark theme).
- `pcb-mcp`: JSON-RPC 2.0 server with `project.status`, `placement.add`,
  `view.snapshot`. Both stdio and TCP transports, plus a stdio-to-TCP
  bridge so Claude Code can connect to a running Tauri host.
- `src-tauri` + `frontend`: Tauri 2 desktop shell. Owns the canonical
  `Project`, runs the MCP TCP server on `127.0.0.1:7878`, re-emits
  project events to the webview. Frontend renders the live SVG and an
  activity log.

`cargo test` and the e2e MCP TCP smoke test pass.

## Run it

```sh
# Build the frontend bundle once (release build embeds it).
npm --prefix frontend install
npm --prefix frontend run build

# Run the desktop app (release uses the built frontend).
cargo run --release -p pcb-app --bin pcb-app
```

The app opens a window and starts an MCP server on `127.0.0.1:7878`.

## Connect Claude Code

```sh
# After `cargo build --release` produced the bridge binary:
claude mcp add pcb -- ./target/release/pcb-mcp-bridge
```

Claude launches the bridge, the bridge proxies stdio ↔ TCP to the running
app, and the agent and the UI now share one project.

## Standalone (no GUI)

For headless use — CI, scripts, or when the agent is the only client:

```sh
./target/release/pcb-mcp-stdio
```
