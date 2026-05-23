//! MCP server runtime: stdin reader → dispatch → stdout writer.
//!
//! Single producer (this loop) for stdout. Background tasks that want to
//! emit channel notifications send their JSON value through the
//! `notification_tx` channel; the writer drains it from a single point so
//! the protocol stream is never corrupted by concurrent writes.

use frankenstein::client_reqwest::Bot;
use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::sync::mpsc;

use super::permission::{PendingPermissions, PermissionDetails};
use super::protocol::{
    classify, error_response, initialize_response, parse_error_response,
    tool_call_response, tools_list_response, Inbound, ERROR_INTERNAL,
    ERROR_INVALID_PARAMS, ERROR_METHOD_NOT_FOUND, MAX_MCP_FRAME_SIZE,
};
use super::tools::tools_list;
use crate::telegram::api as tg_api;

/// Run the MCP server on stdin/stdout. Returns when stdin EOFs or a fatal
/// IO error occurs. `bot` is optional — None means TG isn't configured;
/// tool calls return method-not-found. `pending_permissions` is shared
/// with the TG polling task for permission-flow round-tripping.
pub async fn run(
    mut notification_rx: mpsc::UnboundedReceiver<Value>,
    bot: Option<Bot>,
    pending_permissions: PendingPermissions,
) -> std::io::Result<()> {
    let stdin = tokio::io::stdin();
    let mut reader = BufReader::new(stdin).lines();
    let mut stdout = tokio::io::stdout();

    loop {
        tokio::select! {
            line = reader.next_line() => {
                let line = match line {
                    Ok(Some(l)) => l,
                    Ok(None) => {
                        tracing::info!("stdin EOF — shutting down");
                        return Ok(());
                    }
                    Err(e) => {
                        tracing::error!(error = %e, "stdin read error");
                        return Err(e);
                    }
                };
                if line.len() > MAX_MCP_FRAME_SIZE {
                    tracing::warn!(size = line.len(), "inbound frame exceeds MAX_MCP_FRAME_SIZE — dropping");
                    write_value(&mut stdout, &parse_error_response()).await?;
                    continue;
                }
                if line.trim().is_empty() {
                    continue;
                }
                let response = handle_frame(&line, bot.as_ref(), &pending_permissions).await;
                if let Some(resp) = response {
                    write_value(&mut stdout, &resp).await?;
                }
            }

            Some(notif) = notification_rx.recv() => {
                write_value(&mut stdout, &notif).await?;
            }
        }
    }
}

/// Parse one inbound JSON-RPC line + dispatch.
async fn handle_frame(
    line: &str,
    bot: Option<&Bot>,
    pending: &PendingPermissions,
) -> Option<Value> {
    let value: Value = match serde_json::from_str(line) {
        Ok(v) => v,
        Err(e) => {
            tracing::warn!(error = %e, line = %line, "parse error");
            return Some(parse_error_response());
        }
    };

    match classify(&value) {
        Inbound::Request { id, method, params } => {
            tracing::debug!(method = %method, "request");
            match method.as_str() {
                "initialize" => Some(initialize_response(id)),
                "tools/list" => Some(tools_list_response(id, tools_list())),
                "tools/call" => Some(dispatch_tool_call(id, params, bot).await),
                _ => Some(error_response(
                    id,
                    ERROR_METHOD_NOT_FOUND,
                    &format!("method not found: {}", method),
                )),
            }
        }
        Inbound::Notification { method, params } => {
            tracing::debug!(method = %method, "notification (no response)");
            if method == "notifications/claude/channel/permission_request" {
                if let Some(bot) = bot {
                    handle_permission_request(params, bot, pending).await;
                } else {
                    tracing::warn!("permission_request received but no bot configured");
                }
            }
            None
        }
        Inbound::Invalid => {
            tracing::warn!(value = %value, "invalid frame (no method)");
            Some(parse_error_response())
        }
    }
}

/// Receive a permission_request notification from CC, store details,
/// fan out to all allowlisted TG chats as a message with inline
/// yes/no/more buttons. Mirrors TSX `server.ts:418-444`.
async fn handle_permission_request(
    params: Value,
    bot: &Bot,
    pending: &PendingPermissions,
) {
    let request_id = match params.get("request_id").and_then(|v| v.as_str()) {
        Some(s) => s.to_string(),
        None => {
            tracing::warn!("permission_request missing request_id — dropping");
            return;
        }
    };
    let tool_name = params
        .get("tool_name")
        .and_then(|v| v.as_str())
        .unwrap_or("(unknown)")
        .to_string();
    let description = params
        .get("description")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    let input_preview = params
        .get("input_preview")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();

    tracing::info!(
        request_id = %request_id,
        tool_name = %tool_name,
        "permission_request received from CC"
    );

    pending.insert(
        request_id.clone(),
        PermissionDetails {
            tool_name: tool_name.clone(),
            description,
            input_preview,
        },
    );

    let access = crate::access::state::load();
    let text = format!("🔐 Permission: {}", tool_name);
    for chat_id_str in &access.allow_from {
        let chat_id: i64 = match chat_id_str.parse() {
            Ok(n) => n,
            Err(_) => {
                tracing::warn!(chat_id = %chat_id_str, "non-numeric chat_id in allowlist — skipping");
                continue;
            }
        };
        if let Err(e) = crate::telegram::api::send_permission_prompt(
            bot, chat_id, &text, &request_id,
        )
        .await
        {
            tracing::warn!(
                chat_id = chat_id,
                error = %e,
                "permission_request send failed for chat"
            );
        }
    }
}

/// Dispatch `tools/call` to the named tool.
async fn dispatch_tool_call(id: Value, params: Value, bot: Option<&Bot>) -> Value {
    let name = params.get("name").and_then(|v| v.as_str()).unwrap_or("");
    let arguments = params.get("arguments").cloned().unwrap_or(Value::Null);
    tracing::debug!(tool = %name, "tools/call dispatch");

    let Some(bot) = bot else {
        return error_response(
            id,
            ERROR_INTERNAL,
            "TG not configured (TELEGRAM_BOT_TOKEN missing)",
        );
    };

    match name {
        "reply" => handle_reply(id, arguments, bot).await,
        "react" => handle_react(id, arguments, bot).await,
        "edit_message" => handle_edit_message(id, arguments, bot).await,
        "download_attachment" => handle_download_attachment(id, arguments, bot).await,
        other => error_response(
            id,
            ERROR_METHOD_NOT_FOUND,
            &format!("unknown tool: {}", other),
        ),
    }
}

fn parse_id_arg(args: &Value, key: &str) -> Result<i64, String> {
    match args.get(key) {
        Some(v) => v
            .as_str()
            .and_then(|s| s.parse::<i64>().ok())
            .or_else(|| v.as_i64())
            .ok_or_else(|| format!("{} must be string or integer", key)),
        None => Err(format!("{} required", key)),
    }
}

/// Handle a `reply` tool call.
async fn handle_reply(id: Value, args: Value, bot: &Bot) -> Value {
    let chat_id = match parse_id_arg(&args, "chat_id") {
        Ok(n) => n,
        Err(e) => return error_response(id, ERROR_INVALID_PARAMS, &e),
    };
    if let Err(e) = crate::access::gate::assert_allowed_chat(&chat_id.to_string()) {
        return error_response(id, ERROR_INVALID_PARAMS, &e);
    }

    let text = match args.get("text").and_then(|v| v.as_str()) {
        Some(s) => s,
        None => return error_response(id, ERROR_INVALID_PARAMS, "text required"),
    };

    let reply_to = args
        .get("reply_to")
        .and_then(|v| v.as_str().and_then(|s| s.parse::<i32>().ok()).or_else(|| v.as_i64().map(|n| n as i32)));

    let files: Vec<String> = args
        .get("files")
        .and_then(|v| v.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|f| f.as_str().map(str::to_string))
                .collect()
        })
        .unwrap_or_default();

    match tg_api::reply(bot, chat_id, text, reply_to, &files).await {
        Ok(result) => {
            let ids_str = result
                .message_ids
                .iter()
                .map(|m| m.to_string())
                .collect::<Vec<_>>()
                .join(",");
            let response_text = if result.message_ids.len() == 1 {
                format!("sent (id: {})", ids_str)
            } else {
                format!("sent {} chunks (ids: {})", result.message_ids.len(), ids_str)
            };
            tool_call_response(id, json!([{ "type": "text", "text": response_text }]))
        }
        Err(e) => {
            tracing::error!(error = %e, "reply failed");
            error_response(id, ERROR_INTERNAL, &format!("reply failed: {}", e))
        }
    }
}

/// Handle a `react` tool call.
async fn handle_react(id: Value, args: Value, bot: &Bot) -> Value {
    let chat_id = match parse_id_arg(&args, "chat_id") {
        Ok(n) => n,
        Err(e) => return error_response(id, ERROR_INVALID_PARAMS, &e),
    };
    if let Err(e) = crate::access::gate::assert_allowed_chat(&chat_id.to_string()) {
        return error_response(id, ERROR_INVALID_PARAMS, &e);
    }
    let message_id = match parse_id_arg(&args, "message_id") {
        Ok(n) => n as i32,
        Err(e) => return error_response(id, ERROR_INVALID_PARAMS, &e),
    };
    let emoji = match args.get("emoji").and_then(|v| v.as_str()) {
        Some(s) => s,
        None => return error_response(id, ERROR_INVALID_PARAMS, "emoji required"),
    };

    match tg_api::react(bot, chat_id, message_id, emoji).await {
        Ok(()) => tool_call_response(id, json!([{ "type": "text", "text": "reacted" }])),
        Err(e) => {
            tracing::error!(error = %e, "react failed");
            error_response(id, ERROR_INTERNAL, &format!("react failed: {}", e))
        }
    }
}

/// Handle an `edit_message` tool call.
async fn handle_edit_message(id: Value, args: Value, bot: &Bot) -> Value {
    let chat_id = match parse_id_arg(&args, "chat_id") {
        Ok(n) => n,
        Err(e) => return error_response(id, ERROR_INVALID_PARAMS, &e),
    };
    if let Err(e) = crate::access::gate::assert_allowed_chat(&chat_id.to_string()) {
        return error_response(id, ERROR_INVALID_PARAMS, &e);
    }
    let message_id = match parse_id_arg(&args, "message_id") {
        Ok(n) => n as i32,
        Err(e) => return error_response(id, ERROR_INVALID_PARAMS, &e),
    };
    let text = match args.get("text").and_then(|v| v.as_str()) {
        Some(s) => s,
        None => return error_response(id, ERROR_INVALID_PARAMS, "text required"),
    };

    match tg_api::edit_message(bot, chat_id, message_id, text).await {
        Ok(mid) => tool_call_response(
            id,
            json!([{ "type": "text", "text": format!("edited (id: {})", mid) }]),
        ),
        Err(e) => {
            tracing::error!(error = %e, "edit_message failed");
            error_response(id, ERROR_INTERNAL, &format!("edit_message failed: {}", e))
        }
    }
}

/// Handle a `download_attachment` tool call. No assert_allowed_chat —
/// file_id is opaque, validated by Telegram on getFile.
async fn handle_download_attachment(id: Value, args: Value, bot: &Bot) -> Value {
    let file_id = match args.get("file_id").and_then(|v| v.as_str()) {
        Some(s) => s,
        None => return error_response(id, ERROR_INVALID_PARAMS, "file_id required"),
    };
    let token = std::env::var("TELEGRAM_BOT_TOKEN").unwrap_or_default();
    if token.is_empty() {
        return error_response(id, ERROR_INTERNAL, "TELEGRAM_BOT_TOKEN missing at download time");
    }
    match tg_api::download_attachment(bot, &token, file_id).await {
        Ok(path) => tool_call_response(id, json!([{ "type": "text", "text": path }])),
        Err(e) => {
            tracing::error!(error = %e, "download_attachment failed");
            error_response(id, ERROR_INTERNAL, &format!("download_attachment failed: {}", e))
        }
    }
}

/// Serialize JSON value as single line + flush to stdout.
async fn write_value<W>(out: &mut W, value: &Value) -> std::io::Result<()>
where
    W: tokio::io::AsyncWrite + Unpin,
{
    let mut s = serde_json::to_string(value).map_err(std::io::Error::other)?;
    s.push('\n');
    out.write_all(s.as_bytes()).await?;
    out.flush().await?;
    Ok(())
}
