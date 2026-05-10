# Security

Fragua is a desktop app that runs locally. It opens an HTTP server on
`127.0.0.1:7878` for the agent to drive the project. The server is
**loopback only** and stateless — it never authenticates, because it
never accepts non-loopback connections. If you reverse-proxy or
expose it past the local machine, you become the threat model.

## Reporting a vulnerability

If you think you've found something exploitable, please **don't**
open a public issue. Instead, email `kidandcat@gmail.com` with:

- A description of the issue.
- Steps to reproduce.
- The git rev (`git rev-parse HEAD`) and platform.

We'll acknowledge within a few days. For non-critical issues a
public issue is fine.

## What we worry about

- Crafted `.fragua` project files producing arbitrary file writes
  outside the chosen path.
- The script API accepting commands from a network attacker who
  somehow gets a request onto the loopback interface (e.g. via a
  CSRF on a website the user visits while fragua is running).
- The Tauri command surface being abused by a webview extension or
  a malicious page.

## What we don't worry about

- Anyone with shell access to the machine running fragua. They could
  have edited the binary directly.
- Compute amplification (DoS) on the local API. Fragua is a single
  user's tool; locking the loop with a long script is the user's
  problem.
