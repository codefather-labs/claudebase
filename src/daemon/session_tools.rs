//! `session_*` tools — what a session asks the daemon's registry about itself.
//!
//! A session whose daemon is on this machine reads these answers straight from
//! `chat.db`. A session on a remote daemon (docs/design/remote-daemon.md) has
//! no such file, so it asks here; every tool calls the same
//! `agent_registry` function the local path does, so the two cannot disagree.
//! Read-only, and no more than `agent_list_alive` already reveals.

use anyhow::{anyhow, bail, Result};
use serde_json::{json, Value};

use crate::daemon::agent_registry as reg;

pub async fn handle(tool: &str, args: Value) -> Result<Value> {
    let tool = tool.to_owned();
    tokio::task::spawn_blocking(move || dispatch(&tool, &args))
        .await
        .map_err(|e| anyhow!("session task failed: {e}"))?
}

fn dispatch(tool: &str, args: &Value) -> Result<Value> {
    let conn = crate::daemon::chat::open_chat_db_readonly()?;
    match tool {
        "session_bound_threads" => Ok(json!({ "threads": reg::bound_telegram_threads(&conn, &s(args, "agent_id")?)? })),
        "session_topic_owner" => {
            let topic = args.get("topic").and_then(Value::as_i64);
            Ok(json!({ "owner": reg::topic_owner(&conn, i(args, "chat_id")?, topic)? }))
        }
        "session_chat_bound" => Ok(json!({ "owner": reg::chat_bound_to(&conn, i(args, "chat_id")?, &s(args, "agent_id")?)? })),
        "session_nick_of" => Ok(json!({ "nick": reg::nick_of(&conn, &s(args, "agent_id")?)? })),
        "session_startup_nick" => {
            let (nick, chosen) = reg::startup_nick(
                &conn,
                &s(args, "host")?,
                &s(args, "cwd")?,
                args.get("terminal").and_then(Value::as_str).unwrap_or(""),
                &s(args, "base")?,
            );
            Ok(json!({ "nick": nick, "chosen": chosen }))
        }
        "session_resolve_agent" => Ok(json!({ "agent_id": reg::resolve_target(&conn, &s(args, "target")?)? })),
        "session_list" => Ok(json!({ "text": crate::agent_cli::list_with(
            &conn,
            args.get("include_offline").and_then(Value::as_bool).unwrap_or(false),
            args.get("json").and_then(Value::as_bool).unwrap_or(false),
        )? })),
        "session_whoami" => Ok(json!({ "text": crate::agent_cli::whoami_with(
            &conn,
            &s(args, "agent_id")?,
            &s(args, "host")?,
            args.get("cwd").and_then(Value::as_str).unwrap_or(""),
        )? })),
        "session_telegram_target" => {
            let explicit = args.get("explicit").and_then(Value::as_str);
            let agent_id = args.get("agent_id").and_then(Value::as_str);
            let (thread, topic, source) = crate::telegram_cli::resolve_thread(&conn, explicit, agent_id)?;
            Ok(json!({ "thread": thread, "topic": topic, "source": format!("{source:?}") }))
        }
        "session_telegram_status" => Ok(json!({ "text": crate::telegram_cli::status_with(
            &conn,
            args.get("agent_id").and_then(Value::as_str),
        )? })),
        "session_conversation_nick" => Ok(json!({ "nick": reg::recall_conversation_nick(&conn, &s(args, "session_id")?)? })),
        other => bail!("unknown session tool `{other}`"),
    }
}

fn s(args: &Value, key: &str) -> Result<String> {
    args.get(key)
        .and_then(Value::as_str)
        .filter(|v| !v.is_empty())
        .map(str::to_owned)
        .ok_or_else(|| anyhow!("{key} required"))
}

fn i(args: &Value, key: &str) -> Result<i64> {
    args.get(key).and_then(Value::as_i64).ok_or_else(|| anyhow!("{key} required (integer)"))
}
