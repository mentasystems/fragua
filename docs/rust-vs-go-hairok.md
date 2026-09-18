# Fragua: last Rust tree vs Go master

**For:** Hairok — decide whether recovering Rust is worth it.
**Verdict:** **NO recover.** Stay on Go. Cherry-pick at most 2–3 algorithms if a board hits a wall.
**Date:** 2026-09-18. Compared `4e20ae0` (last Rust-only tip) vs `master` @ `1bf5c4c`.

This is a decision memo, not a product change.

---

## 2-minute verdict

Recovering Rust would spend **8–16 person-weeks** to re-port a month of live Go work, then a permanent dual-maintain tax, for algorithms the live product (fragua.cloud, Fecha door, MAX17220 boost benches) no longer needs. Go already claimed DRC/ERC/Gerber parity in the Aug 12–13 rewrite window and then **pulled ahead** on the agent surface (MCP, LCSC/EasyEDA, KiCad export, SI, WLP autoroute). Rust still wins on a few unused engines (full ePlace, topo, photo-calibrate, serpentine length-match). Those are cherry-picks, not a rewrite.

**Do not resurrect crates. Do not dual-maintain. Do not bring Tauri back.**

---

## Anchors (verified in git)

| Claim | Verified | Notes |
|-------|----------|-------|
| Last Rust-era tip `~2dd5411` (2026-08-09) | **Close, not exact** | `2dd5411` still has `Cargo.toml` / `crates/`. Four more Rust-only commits follow it. |
| **True last Rust-only tree** | **`4e20ae0`** (2026-08-09) `fix(placer): make alignment/ring tests CI-stable` | Parent of the Go rewrite. This is the last commit whose tree is Rust+Tauri only. |
| Go rewrite | **`97a46b9`** (2026-08-12) `feat: Go rewrite of Fragua` | Added `cmd/` + `internal/` **beside** crates. Rust kept as oracle. See `PORT_GO.md` (deleted at drop). |
| Rust/Tauri dropped | **`84c070a`** (2026-08-13) | `-75,822` lines. Last tree **with** crates is `625cfa6` (same day, Pico 39/39). |
| Current | `master` @ `1bf5c4c` (2026-09-07) | Pure Go + embedded JS UI. 107 commits after the drop. |

`2dd5411..4e20ae0` is placer/test CI only (`decouple.rs`, solder-floor tests). No missing product feature in that sliver.

---

## 1. Architecture

Same product shape both sides: one in-memory `Project`, script DSL, HTTP on `127.0.0.1:7878`, observer UI. The rewrite swapped the **shell**, not the pipeline.

| Concern | Rust tip (`4e20ae0`) | Go master | Winner |
|---------|----------------------|-----------|--------|
| Core model | `crates/pcb-core` (~11.6k LOC) | `internal/core` (~4.0k) | Tie (same nm/`Project` idea; Rust had photo/hierarchy extras) |
| Script / DSL | `crates/pcb-script` (~13.5k, 77 verbs) | `internal/script` (~5.7k, 46 documented + hidden aliases) | **Go** for agent surface; Rust had more review/photo verbs |
| Host HTTP | `src-tauri` hand-rolled HTTP (~2.3k) **plus** Tauri invoke | `internal/host` + `cmd/fragua` | **Go** (one surface) |
| Desktop | Tauri 2 webview + Vite/`frontend/` (~3.7k TS) | **Dropped** — browser at `/ui/` | **Go** (matches VISION: no extra binary) |
| Placer | `pcb-placer`: real ePlace (`global.rs` 1150 LOC Poisson/DCT/Nesterov) + SA + decouple + edge | `internal/placer`: **cheap force pre-pass** + SA + decouple + edge + power-island | **Rust** algorithm; **Go** production seating |
| Router | `pcb-router` (~16.2k): Theta*, fanout, slots, `reach.rs`, RR&R, negotiate, organic, **topo**, stitch, **diff-follow**, **length-match** | `internal/router` (~9.2k): Theta*, fanout, slots, RR&R, negotiate, organic string-pull, stitch, **impedance necking** | **Go** on shipped boards; Rust deeper unused engines |
| Router tune | `pcb-router-tune` GA (~1.9k) | `fragua bench` (fixed corpus, not a GA) | Different jobs |
| DRC / ERC | `pcb-drc` / `pcb-erc` | `internal/drc` / `internal/erc` (comments still say “Rust oracle”) | Tie at rewrite; Go reports locations now |
| SI / impedance | `impedance` script verb only | `internal/si` + `internal/impedance` + `si-check` | **Go** |
| Fab / Gerber | `pcb-gerber` + `pcb-fab` | `internal/gerber` + `internal/fab` + IPC-D-356A + teardrops | **Go** (byte-identical packs claimed `407f752`) |
| ODB++ | `pcb-odb` JLC-usable subset (~0.7k) | `internal/odb` unused skeleton | **Rust** |
| Parts / library | Disk `~/.pcb-library` + photo calibrate/rectify + pending-confirm | `internal/parts`: EasyEDA/LCSC, KiCad `.kicad_mod/.kicad_sym`, IPC-7351 `lib-gen` | **Go** for agents; Rust for photo review |
| KiCad board | — | `internal/kicad` export `.kicad_pcb` | **Go** |
| MCP | Already gone in Rust | `internal/mcp` stdio (`fragua mcp`) | **Go** |
| Render / UI | `pcb-render` SVG + **PNG/resvg**; Vite UI | `internal/render` SVG + `host/ui` (~1.8k, no build) | **Go** for agent loop; Rust had PNG |
| Size | ~62.7k `.rs` + Tauri/Vite | ~40.5k `.go` + 1.8k JS | Go is smaller and current |
| Tests | 34 `*_test.rs` / `tests/` | 50 `*_test.go` (~10.8k LOC) | **Go** |
| CI | cargo + Tauri | `go test ./...` + `gofmt` + CGO-off release | **Go** |

Workspace members at `4e20ae0`: `pcb-core`, `pcb-router`, `pcb-router-tune`, `pcb-placer`, `pcb-drc`, `pcb-erc`, `pcb-fab`, `pcb-gerber`, `pcb-odb`, `pcb-render`, `pcb-script`, `src-tauri`.

---

## 2. Feature parity at the rewrite (Aug 12–13)

`PORT_GO.md` (in-tree `97a46b9`–`625cfa6`) set cut-over gates A–D: ERC/DRC counts, net connectivity, outline/stackup/positions, normalized Gerber. Byte-identical copper was **explicitly not required**. Topo engine, PNG vs resvg, and MCP were **v1 non-goals**.

What the Go rewrite **claimed and landed** before dropping Rust:

| Commit | Claim |
|--------|--------|
| `d16656c` | DRC/ERC algorithmic parity with Rust on real boards |
| `407f752` | **Byte-identical fab packs** vs Rust on real boards |
| `9aacdf7` | 1-pad and pour nets treated `Ok` like Rust |
| `2063cdb` | Seed-fixed SA **nm-identical** on `stress/two-resistors.fragua`; lazy Theta* |
| `6121b06` | fecha-gateway keep-place **19/19 + DRC 0** |
| `625cfa6` | Pico **39/39**; fecha-door **15/15 DRC 0**; JLCPCB ceiling; 4-layer planes |

**Intentionally dropped with `84c070a`:**

- Tauri desktop + Vite/`frontend/` (VISION already said HTTP + browser).
- Rust oracle (`crates/`, `pcb-oracle`, `scripts/*parity*`).
- Photo-calibration / pending-library review UI (verbs existed; no Go port).
- `engine=topo`, `fine_escape` behavior, GA “Auto Routing” button.

Honest caveat from `PORT_GO.md` the *same afternoon* as the drop: “Algorithmic parity **not achieved** — skeleton ≠ oracle” for copper hashes. They cut over on **process + DRC 0 + net counts**, not geometry identity. That was the right product call; it is not “Go == Rust copper.”

---

## 3. What Go has that Rust never had (post-`84c070a`)

~3.5 weeks, **107 commits**, new packages: `internal/{parts,kicad,si,impedance,mcp,bench,llms}` + `host/ui`.

High-value deltas (shipped, used):

1. **Real parts pipeline** — `part C2040` / `LCSC:…` EasyEDA fetch (`internal/parts/easyeda.go`); `part kicad:…` + `lib-import`; **IPC-7351B `lib-gen`** including WLP (`internal/parts/ipc.go`). Rust only stored an `lcsc=` string on a hand-authored `lib`.
2. **KiCad 9 export** — `pack fab=kicad` / zip includes `.kicad_pcb` (`internal/kicad`). Rust had no board writer.
3. **MCP** — `fragua mcp` stdio over the same host (`internal/mcp`). Rust had dropped MCP before the rewrite.
4. **SI + impedance-aware routing** — `si-check`, closed-form Z (`internal/impedance`), pour-as-return on 2L, **escape necking** (`internal/router/neck.go`, `0df65ac`). Rust had an `impedance` verb, not this loop.
5. **Agent productization** — `fragua init`, `docs/llms.txt`, structured `Verbs` help, JSON `/drc` `/erc` `/summary`, `/cancel`, streamed progress.
6. **Observer/steer UI** — inline SVG, layers, inspector, drag=`move`, no Node (`482b94f`).
7. **Teardrops + IPC-D-356A** (`eed0840`).
8. **Production router/placer after Fecha/MAX17220** — power-island seating, pad-local stitch, WLP-6 fanout ≥6 pads, VIP-fits-land, same-layer short nets. Fair bench: **6/6 nets, DRC 0, 17 ms** (`bench/boost-max17220-fair/NOTES.md`).
9. **Host API tests** that do not depend on `~/.pcb-library` (`bc2aafb`).
10. **`fragua bench`** reference corpus.

Rust LCSC was a BOM field. Go LCSC is an importer. That alone is why agents can design without hand-typing pads.

---

## 4. Quality / performance / determinism

**Still open in Go (code + `TODO.md`):**

| Gap | Where | Severity |
|-----|-------|----------|
| `FineEscape` flag **ignored** | `internal/router/router.go:35` | Low — slots/fanout replaced it |
| Full ePlace not ported | `placer.go` header + `PORT_GO.md`: force-field stand-in | Medium if auto-place quality walls |
| No `reach.rs` flood-proof escape legalisation | Rust `pcb-router/src/reach.rs`; Go reports `unreachable` after search | Medium on dense QFN |
| No coupled diff-pair **routing** / serpentine length-match | Rust `diff_pair` + `length_match.rs`; Go only `diff` + SI skew | Medium on USB/HiS |
| ODB++ stub, not wired to `pack` | `internal/odb/odb.go` | Low — JLC takes Gerber |
| Photo verbs absent (model fields remain) | `core/library.go` `PhotoCalibration` | Low — no Tauri review UI |
| Hierarchical schematic | Rust `sheet` / `sub_sheets`; Go flat | Low |
| Impedance-width unroutable through fine pitch; inner-layer Z refuses asymmetric 4L | `TODO.md` | Known; necking shipped as the fix |
| Organic **fillets** reserved | `OrganicFilletMM` unused; string-pull only | Cosmetic |
| Parity harness deleted | `84c070a` removed `scripts/algo-parity.sh` | Cannot re-diff Rust without a checkout |
| Fecha compact campaign still open | `TODO.md` (same text as Rust) | Product, not language |

**Go strengths vs Rust (measured on this repo’s own boards):**

- Pico 39/39 and fecha-door 15/15 at drop (`625cfa6`).
- MAX17220 WLP-6 fair island 6/6 DRC 0 after Sept placer/router fixes — Rust never saw this package.
- Bounded anytime search: `max_seconds` default 600, failed net rolls back copper, budget ≠ “unreachable”.
- Determinism: seed-fixed SA, sorted maps; host tests no longer depend on `$HOME` library.

**Rust strengths that did not transfer:**

- Real ePlace (literature formulation, 1150 LOC).
- Reachability-legalised escapes (stress campaign O1).
- Experimental topo (2-layer, `PORT_GO` non-goal).
- GA tuner (UI “Auto Routing”).
- PNG screenshots.

There is **no remaining evidence** that the Go router is systematically worse on the boards Hairok ships. The Rust router is larger because it still contains engines Go chose not to port.

---

## 5. Maintainability for AI-driven development

Hairok writes almost no app code; agents drive Fragua **and** the Fragua repo.

| Factor | Rust+Tauri | Go master |
|--------|------------|-----------|
| Toolchains an agent must keep green | rustc + cargo + Tauri 2 + Node/Vite | `go test ./...` |
| Incremental test | `cargo test -p pcb-router` (slow; they added `bench-dev` to dodge LTO) | `go test ./internal/router` (seconds) |
| Cross-compile / release | Tauri per-OS; `unsafe_code = forbid` but heavy | `CGO_ENABLED=0 go build` — current CI/release |
| Surface area | 63k Rust + 2k Tauri + 3.7k TS | 40k Go + 1.8k JS |
| Agent fluency | Fewer training examples; borrow-checker loops | Agents already land PRs #32–#41 on this tree |
| Dual-maintain | Every verb/router fix twice | — |
| Product docs / `fragua init` / MCP | Would need a second port | Already the front door |

VISION.md and AGENTS.md are written for the Go binary. Recovering Rust means rewriting the agent front door again.

---

## 6. Effort to recover Rust

| Option | Work | Risk |
|--------|------|------|
| **A. Resurrect crates at `4e20ae0` and replace Go** | Hours to compile. Then **port ~107 commits / ~15k LOC of Go-only packages** (parts, kicad, si, mcp, host/ui, WLP/island fixes) back into Rust. **8–16 person-weeks.** Fecha + MAX17220 benches go dark until the port is done. | **High.** Live product regresses. Agents slower. Tauri/Vite tax returns. |
| **B. Dual-maintain** | Every script/router/placer change in two languages. Oracle harness would have to be rebuilt (`84c070a` deleted it). | **Unacceptable** for a one-human + agents shop. |
| **C. Stay on Go; cherry-pick algorithms** | Port a file when a board proves the gap. ePlace ~1–2 pw; length-match ~0.5–1 pw; `reach.rs` ~1–2 pw. | **Low.** Matches how Go already absorbed Rust process. |

There is no cheap “just keep the crates.” The crates at `625cfa6` do **not** include EasyEDA, KiCad export, MCP, SI, or the WLP boost work.

---

## Capability table

| Capability | Rust tip `4e20ae0` | Go master | Winner |
|------------|--------------------|-----------|--------|
| Agent HTTP script API | Yes | Yes | Tie |
| MCP stdio | No | Yes | **Go** |
| Tauri desktop | Yes | Dropped | Go (intentional) |
| Observer UI (no build) | Vite+Tauri | Embedded JS | **Go** |
| Photo calibrate / pending lib | Yes | Model only | Rust |
| LCSC/EasyEDA import | Field only | Full importer | **Go** |
| KiCad footprint/symbol import | No | Yes | **Go** |
| IPC-7351 / WLP `lib-gen` | No | Yes | **Go** |
| KiCad board export | No | Yes | **Go** |
| ePlace (Poisson/DCT) | Yes | Cheap force | Rust |
| SA + decouple + edge | Yes | Yes + power-island | **Go** |
| Theta* + fanout + RR&R + organic + stitch | Yes | Yes | Tie |
| Fine-grid escape | Yes | Flag stub | Rust |
| Reachability-legalised escapes | Yes | No | Rust |
| Topo engine | Yes (2L, experimental) | No | Rust (unused) |
| Negotiate leftovers | Optional | Always on leftovers | **Go** |
| Impedance-aware width + necking | No | Yes | **Go** |
| Diff-pair **route** + serpentine | Yes | Declare + SI skew only | Rust |
| SI audit (`si-check`) | No | Yes | **Go** |
| Hierarchical schematic | Yes | No | Rust (unused) |
| DRC/ERC | Yes | Yes + locations + host JSON | **Go** |
| Gerber / JLC pack | Yes | Byte-identical claim + IPC-356 + teardrops | **Go** |
| ODB++ | JLC subset | Stub | Rust |
| PNG screenshot | Yes (resvg) | SVG only | Rust |
| GA router tuner | Yes | `fragua bench` instead | Different |
| Pico / Fecha / MAX17220 live boards | Pico/Fecha era | Those **plus** WLP boost 6/6 | **Go** |
| Agent onboarding (`init`, llms.txt) | No | Yes | **Go** |
| Compile / CI for agents | Cargo+Tauri+Node | `go test ./...` | **Go** |

---

## Top 5 reasons for NO recover

1. **Go is the live product.** fragua.cloud, Fecha door, MAX17220 benches, CI, install.sh, `fragua init` — all Go. Rust last shipped as a product on 2026-08-09.
2. **The rewrite already took the algorithms that mattered** (DRC/ERC, Gerber identity, SA nm, Theta*, fanout/slots, organic, compact) and then **kept going** for a month. Recovering Rust means re-doing that month.
3. **Rust leftovers are unused engines**, not missing product. Topo was a non-goal. Photo-calibrate died with Tauri. GA tuner is not how agents route. ODB is unused because JLC eats Gerber.
4. **Agent velocity.** Hairok does not write the app. `go test ./internal/router` vs cargo+Tauri+Vite is the actual development loop. Dual-maintain would cut that in half.
5. **Risk is one-way.** A failed Rust recovery breaks shipping boards. A failed ePlace cherry-pick is a placer PR.

---

## Hybrid: what (if anything) to cherry-pick **from Rust into Go**

Only if a **named board** fails for that reason. Do not port “because Rust had it.”

| Cherry-pick | From | When |
|-------------|------|------|
| Real ePlace (`global.rs`) | `crates/pcb-placer/src/global.rs` @ `4e20ae0` | Auto-place HPWL/overlaps on a dense module board stay bad after SA |
| Escape reachability (`reach.rs`) | `crates/pcb-router/src/reach.rs` | QFN pads entombed; Go `unreachable` after a full budget |
| Diff-follow + serpentine | `pcb-router` `try_diff_pair_follow` + `length_match.rs` | USB/HiS pair fails SI skew **and** visual pairing on a 4L board |
| **Do not** port | Tauri, Vite photo UI, topo, GA tuner, hierarchical sheets, ODB, PNG | No live demand; Go already replaced the workflow |

---

## Recommendation

**Stay on Go. Do not recover Rust. Do not dual-maintain.**

If placement or a high-speed pair becomes the wall on Fecha or a future USB board, open a **narrow** Go PR that ports one Rust file, with a bench that fails first. That is the only hybrid that pays.
