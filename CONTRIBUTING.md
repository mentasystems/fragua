# Contributing to fragua

Thanks for your interest. Fragua is a small project but we want it to grow
in the open. The bar for contributions is "would I want this in my own copy"
— short, focused, with a test for anything that breaks if regressed.

## Quick start

```sh
# Build and run the tests.
go test ./...

# Run the app (HTTP API + browser UI).
go build -o fragua ./cmd/fragua
./fragua run
# or: ./fragua run /path/to/board.fragua

# Bare `fragua` (no `run`) prints usage + the full script reference and exits.
./fragua

# Drive the agent script API (once `fragua run` is up).
curl -s http://127.0.0.1:7878/                    # usage + full reference
curl -s http://127.0.0.1:7878/help                # same
curl -s http://127.0.0.1:7878/health              # ok
curl -s 'http://127.0.0.1:7878/screenshot' -o board.svg
```

Override the listen address with `FRAGUA_API_ADDR` if needed.
Set `FRAGUA_NO_BROWSER=1` to skip opening the browser.

## What's in scope

- Improvements to the router, placer, DRC, ERC, or fab pipeline.
- New fab providers (PCBWay variants, OSHPark, Aisler, Eurocircuits…)
  — add a preset next to the existing JLCPCB profiles.
- Library entries — open a PR adding the part to the agent's component
  catalogue.
- Bug fixes, especially anything found by trying a real-world design.
- Documentation. Yes please.

## What's out of scope (for now)

- A general-purpose schematic/PCB editor that competes with KiCad on
  features. The human edits to *correct* the agent, not to design from
  scratch by hand.
- External CAD tool integrations (`kicad-cli`, FreeRouting, Altium import
  / export). The non-negotiable rule is "no shell-out".
- SPICE, and a mechanical CAD kernel (STEP bodies, mates). Static 3D
  product shots are in scope: `fragua render`.

## How to propose a change

1. **Discuss first if it's big.** Open an issue describing the
   intent — saves you re-doing work if the maintainers want a
   different angle.
2. **One change per PR.** A bug fix and a refactor in the same PR
   doubles the review time.
3. **Add a test for the regression.** Packages under `internal/` have
   `_test.go` files; pick the closest existing pattern.
4. **Run `go test ./...`.** New warnings from `go vet` should be fixed.
5. **Keep the script reference accurate.** If you add or change a
   verb, update `internal/script/usage.go` and the dispatcher in
   `internal/script/dispatch.go` so the agent and the human see the
   new surface at startup, at `GET /`, and in "did you mean"
   suggestions.

## Style

- **Comments explain the *why*, not the *what*.** Names should already
  cover the *what*.
- **No half-finished implementations.** Either it works for the
  declared scope or it shouldn't be in the PR.
- **No dead branches "for the future".** Add the branch when the
  future arrives.
- **Match the existing package's conventions.** If the surrounding code
  uses early returns, use early returns.
- **Code and docs in English.** Commit messages follow
  `feat|fix|docs(scope): …`.
