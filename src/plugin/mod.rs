//! # Plugin — Claude Code MCP STDIO bridge to claudebase daemon
//!
//! INVARIANT (load-bearing): this module runs TWO wire protocols
//! simultaneously and they MUST NOT be cross-wired:
//!
//! - **STDIO side (Claude Code ↔ plugin):** newline-delimited UTF-8 JSON.
//!   One JSON object per `\n`-terminated line. NO length prefix.
//!   Read with `tokio::io::BufReader::new(stdin).lines()`. Write with
//!   `stdout.write_all(serialized + "\n")` then `flush()`.
//!
//! - **UDS side (plugin ↔ daemon):** length-prefixed JSON via
//!   `crate::daemon::ipc::{read_frame, write_frame}` — 4-byte big-endian
//!   length header + JSON body, 16 MiB cap.
//!
//! Calling `ipc::read_frame` on stdin OR doing newline-framing on the UDS
//! socket is a wire-format violation. The dispatcher in `bridge.rs` is
//! the ONLY place these two protocols meet.

pub mod bridge;
pub mod mcp;

use crate::cli::PluginServeArgs;

/// Entry point for `claudebase plugin serve`. Runs the STDIO/MCP bridge
/// that connects to the daemon over UDS and proxies JSON-RPC traffic.
/// Returns Ok(()) on stdin EOF (clean shutdown per SEC-6).
pub async fn serve(_args: &PluginServeArgs) -> anyhow::Result<()> {
    bridge::run().await
}
