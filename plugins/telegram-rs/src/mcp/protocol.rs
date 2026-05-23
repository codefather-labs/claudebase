//! JSON-RPC 2.0 + MCP message constructors. Plain `serde_json::Value` so
//! the wire format is auditable byte-for-byte against the TSX reference.

use serde_json::{json, Value};

/// MCP protocol version advertised on `initialize`. Per MCP spec the
/// server responds with its supported version regardless of what the
/// client asks for; Claude Code 2.1.144 sends `"2025-11-25"` and accepts
/// the negotiation outcome without error.
pub const SUPPORTED_PROTOCOL_VERSION: &str = "2025-11-25";

/// Maximum stdin line size we accept (1 MiB). MCP envelopes are short
/// JSON-RPC frames — anything bigger is a protocol violation or attack.
pub const MAX_MCP_FRAME_SIZE: usize = 1024 * 1024;

// JSON-RPC 2.0 error codes from the spec.
pub const ERROR_PARSE: i64 = -32700;
pub const ERROR_INVALID_REQUEST: i64 = -32600;
pub const ERROR_METHOD_NOT_FOUND: i64 = -32601;
pub const ERROR_INVALID_PARAMS: i64 = -32602;
pub const ERROR_INTERNAL: i64 = -32603;

/// Build a Parse Error response. `id: null` per JSON-RPC 2.0 spec —
/// the client didn't pass an id because we couldn't even parse the frame.
pub fn parse_error_response() -> Value {
    json!({
        "jsonrpc": "2.0",
        "id": Value::Null,
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

/// Build the response to `initialize`. Capabilities mirror what TSX
/// `server.ts` declares: tools only, no prompts/resources/sampling.
/// `experimental.claude/channel*` is the channel-surface opt-in.
pub fn initialize_response(id: Value) -> Value {
    json!({
        "jsonrpc": "2.0",
        "id": id,
        "result": {
            "protocolVersion": SUPPORTED_PROTOCOL_VERSION,
            "capabilities": {
                "tools": { "listChanged": true },
                "experimental": {
                    "claude/channel": {},
                    "claude/channel/permission": {}
                }
            },
            "serverInfo": {
                "name": "telegram-plugin-rs",
                "version": env!("CARGO_PKG_VERSION")
            }
        }
    })
}

/// Build a `tools/list` response. Tools is the array passed in (already
/// formatted with `name`, `description`, `inputSchema`).
pub fn tools_list_response(id: Value, tools: Value) -> Value {
    json!({
        "jsonrpc": "2.0",
        "id": id,
        "result": { "tools": tools }
    })
}

/// Build a `tools/call` success response. `content` is the array of
/// content items (typically one `{type: "text", text: "..."}`).
pub fn tool_call_response(id: Value, content: Value) -> Value {
    json!({
        "jsonrpc": "2.0",
        "id": id,
        "result": { "content": content }
    })
}

/// Build a custom notification (no `id` field per JSON-RPC 2.0). This is
/// how we emit Claude Code channel notifications:
/// `notifications/claude/channel/message`, `notifications/claude/channel/permission`,
/// etc.
pub fn notification(method: &str, params: Value) -> Value {
    json!({
        "jsonrpc": "2.0",
        "method": method,
        "params": params
    })
}

/// Classify a parsed JSON value as a JSON-RPC request, notification, or
/// invalid frame. Request has `method` + `id`; notification has `method`
/// but no `id`; everything else is invalid.
#[derive(Debug)]
pub enum Inbound {
    Request {
        id: Value,
        method: String,
        params: Value,
    },
    Notification {
        method: String,
        #[allow(dead_code)]
        params: Value,
    },
    Invalid,
}

pub fn classify(value: &Value) -> Inbound {
    let method = match value.get("method").and_then(|v| v.as_str()) {
        Some(m) => m.to_string(),
        None => return Inbound::Invalid,
    };
    let params = value.get("params").cloned().unwrap_or(Value::Null);
    match value.get("id").cloned() {
        Some(id) if !id.is_null() => Inbound::Request { id, method, params },
        _ => Inbound::Notification { method, params },
    }
}
