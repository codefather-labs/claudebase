//! Hand-rolled MCP / JSON-RPC 2.0 wire layer.
//!
//! Modeled after `claudebase/src/plugin/mcp.rs` (own code, freely reusable
//! within this ecosystem). We intentionally do NOT depend on the `rmcp`
//! crate — its high-level Peer API does not expose arbitrary-method
//! notifications, which is a blocker for the Claude Code channel surface
//! (`notifications/claude/channel/...`).
//!
//! The MCP spec is wire-compatible with JSON-RPC 2.0 plus a thin layer of
//! method conventions: `initialize`, `tools/list`, `tools/call`,
//! `notifications/initialized`, `notifications/claude/channel/...`.

pub mod notification;
pub mod permission;
pub mod protocol;
pub mod server;
pub mod tools;
