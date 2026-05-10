<!--
Thanks for the PR. Couple of quick checks:

- One change per PR. Bundle a refactor with the bug fix it enables.
- `cargo test --workspace` and `cargo clippy --workspace` are green.
- If you added or changed a script verb, the `script_reference()`
  string in `crates/pcb-script/src/tools.rs` is updated.

Drop the comment markers and fill in the sections below.
-->

## Summary

What changed and why.

## How to verify

- [ ] Tests added or existing tests still pass.
- [ ] End-to-end check on a real board (`route` / `pack` / etc.).

## Notes for reviewers

Anything tricky, anything you want a second opinion on, anything you
deferred.
