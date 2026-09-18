# Rust algorithms now living in Go

The rust workspace (and Tauri) is gone. The last rust-only tree is
`4e20ae0` — parent of the Go rewrite `97a46b9`. Two engines that the Go
tree had only as stand-ins are now ports of that tip:

| Engine | Rust source at `4e20ae0` | Go | How to use |
|--------|--------------------------|----|------------|
| **ePlace** | `crates/pcb-placer/src/global.rs` (~1150 LOC: Poisson/DCT, WA wirelength, Nesterov + BB, rotation probe, density legalisation) | `internal/placer/eplace.go` | Default `auto-place` / `Place` with `GlobalStage=true` (the default). `global=0` is SA-only, still rust-parity. |
| **Topo router** | `crates/pcb-router/src/topo.rs` (CDT dual, homotopy A*, rubber-band realise, rip-up rounds) | `internal/router/topo.go` + `delaunay.go` | Opt-in: `route engine=topo` or `Options.Engine = "topo"`. Default remains Theta* / RR&R. |

## What was replaced

- **ePlace.** Go used to run a cheap force pre-pass (`globalForce`: net-centroid
  attraction + pairwise push) whenever `GlobalStage=true`, then retune SA.
  That function is still in `placer.go` so tests can compare it to the
  literature solve; it is no longer on the agent path.
- **Topo.** There was no Go engine. `route` always used the grid / Theta*
  path. It still does unless `engine=topo` is set. Topo *clears existing
  copper and re-routes the board* (same as rust `route_all`).

## What stayed

SA legalisation, decoupling / power-island seating, edge snap, MCP,
KiCad export, and the Fecha / MAX17220-fair default `route` path are
unchanged. ePlace is deterministic (no RNG). Topo is deterministic
(sites in board order, A* ties on indices, no RNG).

Delaunay is a small Bowyer–Watson in `delaunay.go` rather than rust's
`spade` crate. Generic Go only — no CAD wrapper.

## Enabling topo

```text
route engine=topo max_seconds=60
```

MCP: `fragua_route` accepts `"engine": "topo"`. Leave it off for the
stable grid engine.
