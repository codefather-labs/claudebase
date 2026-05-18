//! Hand-rolled MCP / JSON-RPC 2.0 envelope helpers for the plugin
//! bridge (Slice 1b). The MCP spec is wire-compatible with JSON-RPC 2.0
//! plus a thin layer of method conventions (`initialize`, `tools/list`,
//! `tools/call`, `notifications/initialized`, `notifications/tools/list_changed`,
//! `notifications/claude/channel`).
//!
//! We intentionally do NOT depend on the `rmcp` crate (OQ-ACD-3 resolved
//! in plan): hand-rolled JSON values keep the dependency surface flat
//! and the wire-format auditing direct. The downside (boilerplate) is
//! manageable for the 4-5 methods we touch.
//!
//! ## Frame sizing
//!
//! The MCP leg (stdin/stdout) is capped at 1 MiB per frame
//! (`MAX_MCP_FRAME_SIZE`) — significantly stricter than the daemon's
//! 16 MiB cap because MCP frames are short JSON-RPC envelopes. The cap
//! is enforced on stdin line reads AND on daemon response bodies before
//! deserialization (SEC-1 from Vault pre-review).

use serde_json::{json, Value};

/// Maximum size of a single MCP-leg frame (stdin line OR daemon response
/// body before deserialization). 1 MiB is generous for JSON-RPC
/// envelopes — Claude Code's prompt frames rarely exceed 64 KiB. The
/// daemon's UDS leg keeps its 16 MiB cap (different threat model:
/// production traffic includes embeddings).
pub const MAX_MCP_FRAME_SIZE: usize = 1 * 1024 * 1024;

/// Single supported MCP protocol version. Mismatches raise `-32602
/// Invalid params` per JSON-RPC 2.0. We pin one version explicitly so
/// the architect soft-concern (silent client-version drift) surfaces as
/// an explicit error instead of an opaque handshake failure.
pub const SUPPORTED_PROTOCOL_VERSION: &str = "2024-11-05";

// JSON-RPC 2.0 error codes (from the spec).
pub const ERROR_PARSE: i64 = -32700;
pub const ERROR_INVALID_REQUEST: i64 = -32600;
pub const ERROR_METHOD_NOT_FOUND: i64 = -32601;
pub const ERROR_INVALID_PARAMS: i64 = -32602;
pub const ERROR_INTERNAL: i64 = -32603;

/// Whitelist of tool names the plugin will accept on `tools/call`. Per
/// SEC-7 from Vault — unknown names short-circuit to `-32601` BEFORE
/// any UDS frame is sent. Slice 3 adds the real handlers for the
/// chat_* tools; Slice 1b just whitelists the names.
pub const TOOL_WHITELIST: &[&str] = &[
    "chat_post",
    "chat_subscribe",
    "chat_reply",
    "chat_list",
    "claudebase_daemon_status",
    // Slice 5 — agent_registry tools (SEC-7 whitelist parity with daemon dispatch)
    "agent_register",
    "agent_unregister",
    "agent_list_alive",
    "agent_reap",
];

/// Build the Parse Error response per JSON-RPC 2.0 (SEC-3).
///
/// MUST be byte-for-byte the same shape on both the plugin and the
/// daemon. The `id: null` is mandatory — the client didn't pass an `id`
/// because we couldn't even parse the frame.
pub fn parse_error_response() -> Value {
    json!({
        "jsonrpc": "2.0",
        "id": serde_json::Value::Null,
        "error": {
            "code": ERROR_PARSE,
            "message": "Parse error"
        }
    })
}

/// Build a generic error response with `code` + `message` echoing the
/// inbound `id` (which may be int, string, or null per JSON-RPC 2.0).
pub fn error_response(id: Value, code: i64, message: &str) -> Value {
    json!({
        "jsonrpc": "2.0",
        "id": id,
        "error": {
            "code": code,
            "message": message,
        }
    })
}

/// Build the `initialize` response with capabilities matching the
/// claudebase plugin's surface. Includes
/// `capabilities.tools.listChanged: true` per mcp-protocol-expert
/// invariant #2 (load-bearing for FR-ACD-3.7 — daemon-up notifies
/// Claude Code that the tool list grew).
pub fn initialize_response(id: Value) -> Value {
    json!({
        "jsonrpc": "2.0",
        "id": id,
        "result": {
            "protocolVersion": SUPPORTED_PROTOCOL_VERSION,
            "serverInfo": {
                "name": "claudebase",
                "version": env!("CARGO_PKG_VERSION")
            },
            "capabilities": {
                "tools": {
                    "listChanged": true
                }
            }
        }
    })
}

/// Build the daemon-down `tools/list` sentinel response per FR-ACD-10.1.
/// The sentinel surface is exactly one tool — `claudebase_daemon_status`
/// — with an empty `inputSchema` (`{}`) so Claude Code can render the
/// "daemon down" status without parameter prompts. Schema drift from
/// this literal is flagged as MAJOR by the mcp-protocol-expert
/// invariant #8.
pub fn tools_list_daemon_down_response(id: Value) -> Value {
    json!({
        "jsonrpc": "2.0",
        "id": id,
        "result": {
            "tools": [{
                "name": "claudebase_daemon_status",
                "description": "Report claudebase daemon status when the daemon is not running.",
                "inputSchema": {}
            }]
        }
    })
}

/// Build the daemon-down `claudebase_daemon_status` response with the
/// verbatim FR-ACD-10.1 message — byte-exact, NO paths, NO env vars,
/// NO PID file locations, NO Rust error chains. Vault SEC-8.
pub fn daemon_status_down_call_response(id: Value) -> Value {
    json!({
        "jsonrpc": "2.0",
        "id": id,
        "result": {
            "content": [{
                "type": "text",
                "text": "{\"status\":\"down\",\"message\":\"claudebase daemon is not running — start it with 'claudebase daemon start'\"}"
            }]
        }
    })
}

/// Validate an inbound `tools/call` `params.name` against the whitelist.
/// Returns `None` when valid (caller forwards to daemon); returns
/// `Some(Value)` with a pre-built error response on mismatch (SEC-7).
pub fn validate_tool_name(name: &str) -> bool {
    TOOL_WHITELIST.iter().any(|t| *t == name)
}

/// Build the `notifications/tools/list_changed` notification — sent
/// (per mcp-protocol-expert invariant #5) when the daemon transitions
/// from down → up so Claude Code re-fetches `tools/list`. No `id`
/// field per JSON-RPC 2.0 (notifications are fire-and-forget).
pub fn tools_list_changed_notification() -> Value {
    json!({
        "jsonrpc": "2.0",
        "method": "notifications/tools/list_changed"
    })
}

/// Build a `notifications/claude/channel` notification. The plugin
/// relays these from the daemon to Claude Code — Slice 3 is the
/// producer; Slice 1b wires the emitter path so the plumbing is in
/// place. Per mcp-protocol-expert invariant #6.
#[allow(dead_code)] // Slice 3 wires the call site; Slice 1b ships the helper.
pub fn channel_notification(params: Value) -> Value {
    json!({
        "jsonrpc": "2.0",
        "method": "notifications/claude/channel",
        "params": params
    })
}

/// Classify an inbound MCP frame after JSON parsing. Plugin's bridge
/// uses the verdict to decide handler vs forward vs error vs notification.
pub enum Inbound {
    /// A request with an `id` and a `method` — caller dispatches by method.
    Request {
        id: Value,
        method: String,
        params: Value,
    },
    /// A notification — `method` present, `id` absent. Caller handles
    /// or discards.
    Notification {
        method: String,
        #[allow(dead_code)] // Slice 3 will consume notification params
        params: Value,
    },
    /// Frame parsed as JSON but did not match request/notification
    /// shape (e.g., missing `method`). Caller MUST respond with
    /// `-32600 Invalid request`.
    Invalid,
}

/// Classify a parsed JSON Value as a JSON-RPC request, notification, or
/// invalid frame. Field-level details (method name dispatch, params
/// validation) are caller's responsibility — this helper just untangles
/// the request-vs-notification dichotomy.
pub fn classify(value: &Value) -> Inbound {
    let method = match value.get("method").and_then(|m| m.as_str()) {
        Some(m) => m.to_string(),
        None => return Inbound::Invalid,
    };
    let params = value.get("params").cloned().unwrap_or(Value::Null);
    match value.get("id") {
        Some(id) => Inbound::Request {
            id: id.clone(),
            method,
            params,
        },
        None => Inbound::Notification { method, params },
    }
}
