# Contributing to fragua

Thanks for your interest. Fragua is a small project but we want it to grow
in the open. The bar for contributions is "would I want this in my own copy"
— short, focused, with a test for anything that breaks if regressed.

## Quick start

```sh
# Build and run the tests.
cargo test --workspace

# Run the desktop app.
npm --prefix frontend install
npm --prefix frontend run build
cargo run --release --bin fragua

# Run the agent script API on its own (no GUI needed for most flows).
curl -s http://127.0.0.1:7878/                    # full reference
curl -s http://127.0.0.1:7878/health              # ok
```

## What's in scope

- Improvements to the router, placer, DRC, ERC, or fab pipeline.
- New fab providers (PCBWay variants, OSHPark, Aisler, Eurocircuits…)
  — add a variant to `pcb_fab::Provider` and the matching `match` arms.
- Library entries — open a PR adding the part to the agent's component
  catalogue.
- Bug fixes, especially anything found by trying a real-world design.
- Documentation. Yes please.

## What's out of scope (for now)

- A general-purpose schematic/PCB editor that competes with KiCad on
  features. The human edits to *correct* the agent, not to design from
  scratch by hand.
- External CAD tool integrations (`kicad-cli`, FreeRouting, Altium import
  / export). The non-negotiable rule is "no shell-out, no wrapper crates".
- 3D rendering / SPICE / signal integrity. These belong in adjacent
  tools, not in the core loop.

## How to propose a change

1. **Discuss first if it's big.** Open an issue describing the
   intent — saves you re-doing work if the maintainers want a
   different angle.
2. **One change per PR.** A bug fix and a refactor in the same PR
   doubles the review time.
3. **Add a test for the regression.** Almost every crate has a `tests/`
   directory or inline `#[test]`s; pick the closest existing pattern.
4. **Run `cargo test --workspace` and `cargo clippy --workspace`.**
   The warnings list is intentionally short; new warnings should be
   addressed or explicitly silenced with a comment explaining why.
5. **Keep the script reference accurate.** If you add or change a
   verb, update the `script_reference()` string in
   `crates/pcb-script/src/tools.rs` so the agent and the human see
   the new surface at startup and at `GET /`.

## Style

- **Comments explain the *why*, not the *what*.** Names should already
  cover the *what*.
- **No half-finished implementations.** Either it works for the
  declared scope or it shouldn't be in the PR.
- **No dead branches "for the future".** Add the branch when the
  future arrives.
- **Match the existing crate's conventions.** If the surrounding code
  uses early returns, use early returns; if it uses exhaustive matches,
  use exhaustive matches.

## Commit messages

Subject line in imperative mood, ≤ 72 characters; describe *what*
the change does, not the journey of getting there. Body explains the
reasoning, the trade-offs, and any behaviour the user/agent will
notice. We use the body as the changelog — please write it as if a
maintainer six months from now is reading it cold.

## Reporting bugs

Open an issue with:

- A minimal script that reproduces the problem.
- The expected behaviour and what you saw instead.
- The board file (`.fragua`) if the bug depends on geometry — feel
  free to anonymise net names.
- The git rev of the build (`git rev-parse HEAD`).

## License

By contributing you agree your work is published under the MIT
license that covers the rest of the repository.
