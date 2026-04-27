//! `pcb-mcp` — MCP server.
//!
//! Speaks MCP / JSON-RPC 2.0 (stdio for now; SSE later) and exposes the
//! tool surface that AI agents — Claude Code first — use to drive a
//! `Project`. Tools are intentionally thin: validate input, mutate the
//! project through `pcb-core` APIs, return a result. The agent does the
//! reasoning.
//!
//! Tool families: `project.*`, `placement.*`, `view.*` (more added per
//! ARCHITECTURE.md phases).

pub mod protocol;
pub mod server;
pub mod tools;

pub use server::McpServer;
