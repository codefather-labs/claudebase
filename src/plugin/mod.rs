//! Claude Code MCP plugin bridge (STDIO ↔ daemon UDS).
//!
//! Slice 1a ships a stub that errors out — the real implementation
//! lands in Slice 1b. We expose the entry point now so the CLI dispatch
//! in `src/main.rs` is wired and the `Plugin` subcommand parses
//! end-to-end.

use crate::cli::PluginServeArgs;

/// Stub entry point for `claudebase plugin serve`. Slice 1b replaces
/// the body with the actual STDIO/MCP bridge that connects to the
/// daemon over UDS and proxies JSON-RPC traffic.
pub async fn serve(_args: &PluginServeArgs) -> anyhow::Result<()> {
    anyhow::bail!("Slice 1b implements MCP plugin bridge")
}
