//! `pcb-mcp` — MCP server.
//!
//! Speaks MCP (stdio and/or SSE) and exposes the tool surface that AI
//! agents — Claude Code first — use to drive a `Project`. Tools are
//! intentionally thin: validate input, mutate the project through
//! `pcb-core` APIs, return a result. The agent does the reasoning.
//!
//! Tool families (see `ARCHITECTURE.md` for the canonical list):
//! `project.*`, `schematic.*`, `board.*`, `placement.*`, `route.*`,
//! `drc.*`, `output.*`, `view.*`.
