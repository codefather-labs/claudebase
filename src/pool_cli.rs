//! `claudebase pool …` (docs/design/pool-dispatch.md).
//!
//! Worker commands (`job`, `reply`, `fail`, `ready`) go to the daemon the
//! session belongs to — local or remote — as that session (agent id + token):
//! only the session holding a job may read or answer it.
//!
//! `token …` and `workers` administer THIS machine's daemon and read or write
//! its `chat.db` directly, like `daemon callback …` and `daemon remote issue`;
//! they are never reachable over a connection.

use std::path::PathBuf;

use anyhow::{bail, Context, Result};
use base64::Engine;
use serde_json::{json, Value};

use crate::agent_cli::{AGENT_ID_ENV, SESSION_TOKEN_ENV};
use crate::cli::{PoolSubcommand, PoolTokenSubcommand};
use crate::daemon::client::DaemonClient;
use crate::daemon::remote::{self, TokenKind};

pub async fn run(sub: PoolSubcommand) -> Result<String> {
    match sub {
        PoolSubcommand::Job(a) => job(&a.req_id, a.dir).await,
        PoolSubcommand::Reply(a) => {
            let result = match (a.content, a.file, a.result.as_deref()) {
                (Some(text), _, _) => json!({ "content": text }),
                (None, Some(path), _) => parse_result(&crate::input::read_file(&path)?)?,
                (None, None, Some(r)) if r != "-" => parse_result(r)?,
                (None, None, _) => parse_result(&crate::input::read_stdin()?)?,
            };
            if !a.raw {
                check_content(&result)?;
            }
            let v = call("pool_reply", json!({ "req_id": a.req_id, "result": result })).await?;
            Ok(if v["accepted"].as_bool() == Some(true) {
                format!("pool job {} answered", a.req_id)
            } else {
                format!("pool job {} not accepted ({}): {}", a.req_id, v["state"].as_str().unwrap_or("?"), v["note"].as_str().unwrap_or(""))
            })
        }
        PoolSubcommand::Fail(a) => {
            let v = call("pool_fail", json!({ "req_id": a.req_id, "reason": a.reason })).await?;
            Ok(format!("pool job {}: {}", a.req_id, v["state"].as_str().unwrap_or("?")))
        }
        PoolSubcommand::Ready => {
            call("pool_ready", json!({})).await?;
            Ok("this worker takes jobs again".into())
        }
        PoolSubcommand::Token(t) => token(t.sub),
        PoolSubcommand::Workers(a) => workers(&a.pool),
    }
}

/// The caller parses `content` strictly, so a `content` that is meant to be
/// JSON — it opens with `{` or `[` — and is not, is refused here, where the
/// worker can still fix it, instead of failing downstream. Workers produced
/// `{color:red}` on the first LAN run (2026-10-26).
fn check_content(result: &Value) -> Result<()> {
    let Some(content) = result.get("content").and_then(Value::as_str) else {
        return Ok(());
    };
    let t = content.trim();
    if (t.starts_with('{') || t.starts_with('[')) && serde_json::from_str::<Value>(t).is_err() {
        let err = serde_json::from_str::<Value>(t).unwrap_err();
        bail!(
            "\"content\" looks like JSON but is not valid JSON ({err}) — the caller parses it strictly. \
             Quote every key and string: {{\"color\": \"red\"}}, not {{color: red}}. \
             If it really is meant as text, add --raw."
        );
    }
    Ok(())
}

/// A result is any JSON value — `{content}`, `{tool_calls: […]}`, whatever the
/// gateway's caller expects; the daemon relays it untouched.
fn parse_result(text: &str) -> Result<Value> {
    serde_json::from_str(crate::input::strip_bom(text).trim()).with_context(|| {
        let start: String = text.chars().take(24).flat_map(char::escape_debug).collect();
        format!(
            "the result is not JSON (it starts with \"{start}\"); use --file <path> from PowerShell, \
             or --content \"<text>\" for plain text"
        )
    })
}

async fn call(tool: &str, mut args: Value) -> Result<Value> {
    let env = |k| std::env::var(k).ok().filter(|s: &String| !s.is_empty());
    let (Some(id), Some(token)) = (env(AGENT_ID_ENV), env(SESSION_TOKEN_ENV)) else {
        bail!("pool commands run inside the worker session that holds the job (claudebase run --pool <name>)");
    };
    args["agent_id"] = json!(id);
    args["session_token"] = json!(token);
    let mut client = DaemonClient::connect().await?;
    client.call_tool(tool, args).await
}

/// Fetch the job and lay it out on THIS machine — the worker may be joined to
/// a daemon elsewhere, so the files come through the daemon, not a shared disk.
async fn job(req_id: &str, dir: Option<PathBuf>) -> Result<String> {
    crate::daemon::pool::check_req_id(req_id)?;
    let v = call("pool_job", json!({ "req_id": req_id })).await?;
    let dir = dir.unwrap_or_else(|| remote::claudebase_dir().join("pool-jobs").join(req_id));
    std::fs::create_dir_all(&dir).with_context(|| format!("create {}", dir.display()))?;
    let payload = dir.join("payload.json");
    std::fs::write(&payload, serde_json::to_vec_pretty(&v["payload"])?)?;
    let mut lines = vec![format!("payload: {}", payload.display())];
    for img in v["images"].as_array().into_iter().flatten() {
        let file = img["file"].as_str().unwrap_or("image.bin");
        // Only a plain file name lands in the directory.
        if file.contains(['/', '\\']) || file.starts_with('.') {
            bail!("the daemon named an image `{file}`");
        }
        let bytes = base64::engine::general_purpose::STANDARD.decode(img["base64"].as_str().unwrap_or_default())?;
        let path = dir.join(file);
        std::fs::write(&path, bytes)?;
        lines.push(format!("image: {} ({})", path.display(), img["mime"].as_str().unwrap_or("?")));
    }
    Ok(format!(
        "pool job {req_id}{}\n{}\n\nAnswer with the JSON the caller expects — {{\"content\": …}} or {{\"tool_calls\": […]}} — \
         as the payload's messages ask:\n  claudebase pool reply {req_id} <<'EOF'\n  {{…}}\n  EOF\nor give it back: claudebase pool fail {req_id} \"<reason>\"",
        v["call_type"].as_str().map(|c| format!(" ({c})")).unwrap_or_default(),
        lines.join("\n")
    ))
}

fn token(sub: PoolTokenSubcommand) -> Result<String> {
    let conn = crate::daemon::chat::open_chat_db().context("open chat.db")?;
    match sub {
        PoolTokenSubcommand::Issue(a) => {
            let t = remote::issue_token(&conn, TokenKind::Pool, &a.client)?;
            let bind = crate::daemon::callback::configured_bind(&conn).ok().flatten();
            Ok(format!(
                "pool token for gateway `{}` (shown once; re-issuing retires it):\n\n{t}\n\n\
                 send it as the X-Api-Token header to http://{}/pool/…\n{}",
                a.client,
                bind.clone().unwrap_or_else(|| "<host>:8585".into()),
                if bind.is_none() {
                    "the HTTP listener is off: claudebase daemon callback enable [--bind 0.0.0.0:8585 --i-know-this-is-remote], then claudebase daemon restart"
                } else {
                    ""
                }
            ))
        }
        PoolTokenSubcommand::Revoke(a) => {
            if remote::revoke_token(&conn, TokenKind::Pool, &a.client)? {
                Ok(format!("pool token of `{}` revoked", a.client))
            } else {
                bail!("no pool token for `{}`", a.client)
            }
        }
        PoolTokenSubcommand::List => {
            let list = remote::list_tokens(&conn, TokenKind::Pool)?;
            if list.is_empty() {
                return Ok("no pool tokens (claudebase pool token issue <gateway>)".into());
            }
            Ok(list
                .iter()
                .map(|t| format!("{:<20} issued {}  last used {}", t.name, stamp(t.created_at), t.last_used_at.map(stamp).unwrap_or_else(|| "never".into())))
                .collect::<Vec<_>>()
                .join("\n"))
        }
    }
}

fn workers(pool: &str) -> Result<String> {
    let conn = crate::daemon::chat::open_chat_db().context("open chat.db")?;
    let Some(v) = crate::daemon::pool::workers_json(&conn, pool)? else {
        return Ok(format!("no session has joined pool `{pool}` (claudebase run --pool {pool})"));
    };
    let mut out = format!("pool {pool}: {} free\n", v["free"]);
    for w in v["workers"].as_array().into_iter().flatten() {
        out.push_str(&format!(
            "  {:<20} {:<8} {}/{}  {}\n",
            w["nick"].as_str().unwrap_or("?"),
            w["state"].as_str().unwrap_or("?"),
            w["in_flight"],
            w["capacity"],
            w["req_ids"].as_array().map(|a| a.iter().filter_map(Value::as_str).collect::<Vec<_>>().join(" ")).unwrap_or_default()
        ));
    }
    Ok(out)
}

fn stamp(secs: i64) -> String {
    chrono::DateTime::<chrono::Utc>::from_timestamp(secs, 0)
        .map(|d| d.format("%Y-%m-%d %H:%MZ").to_string())
        .unwrap_or_default()
}
