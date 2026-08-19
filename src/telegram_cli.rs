//! `claudebase telegram …` — the agent-facing outbound surface.
//!
//! Slice 1 of the pty-transport feature. This is the half of the new
//! transport that replaces the `mcp__…__chat_reply` tool: an agent sitting in
//! a Claude Code session reaches its operator by shelling out to
//!
//! ```text
//! claudebase telegram send --text "…"
//! ```
//!
//! and nothing about that call depends on Claude Code's plugin machinery,
//! channel allowlists, or MCP protocol versions.
//!
//! ## Addressing: the agent must not need to know a chat_id
//!
//! Requiring `--thread telegram:434566766` would be a usability failure —
//! the agent has no reason to know the operator's Telegram id, and an id
//! pasted into a prompt is an id that gets hallucinated later. Resolution
//! order, first match wins:
//!
//! 1. explicit `--thread`;
//! 2. the routing key bound to THIS session's agent (`/switch` in Telegram
//!    sets it — `agent_registry.routing_chat_id`), read via
//!    `CLAUDEBASE_AGENT_ID` which the supervisor exports into the session;
//! 3. the only known `telegram:*` thread, when there is exactly one;
//! 4. otherwise: an error that LISTS the candidates instead of guessing.
//!
//! Step 4 is deliberate. Sending an operator's message to the wrong chat is
//! not a recoverable mistake, so ambiguity fails loudly.

use anyhow::{bail, Context, Result};
use serde_json::json;

use crate::daemon::client::DaemonClient;
use crate::daemon::{agent_registry, chat};

/// Env var the PTY supervisor exports into the `claude` child so every Bash
/// call made from that session can attribute itself without arguments.
pub const AGENT_ID_ENV: &str = "CLAUDEBASE_AGENT_ID";

/// What `resolve_thread` concluded, kept for the operator-facing log line so
/// a surprising destination is explainable after the fact.
#[derive(Debug, PartialEq, Eq)]
pub enum ThreadSource {
    Explicit,
    RoutingKey,
    OnlyThread,
}

/// Decide which Telegram thread this send targets.
///
/// Split from the send path so it is unit-testable against a fixture DB —
/// the resolution rules, not the socket, are where the risk is.
pub fn resolve_thread(
    conn: &rusqlite::Connection,
    explicit: Option<&str>,
    agent_id: Option<&str>,
) -> Result<(String, Option<i64>, ThreadSource)> {
    if let Some(thread) = explicit {
        if !thread.starts_with("telegram:") {
            bail!("--thread must look like `telegram:<chat_id>`, got `{thread}`");
        }
        return Ok((thread.to_string(), None, ThreadSource::Explicit));
    }

    // 2 — routing key of this session's agent.
    //
    // The topic travels with the chat id. It used to be discarded here — the
    // binding was read as `(chat_id, _thread_id)` and only the chat survived —
    // so a session bound to a forum TOPIC answered into the group's General
    // instead. Inbound resolved the topic, the binding stored the topic, the
    // registry kept the topic, and the reply threw it away one line before it
    // would have been used.
    if let Some(agent_id) = agent_id {
        if let Some((chat_id, topic)) = routing_key_for(conn, agent_id)? {
            return Ok((format!("telegram:{chat_id}"), topic, ThreadSource::RoutingKey));
        }
    }

    // 3 — exactly one known telegram thread.
    let telegram_threads: Vec<String> = chat::list_threads(conn)
        .context("list chat threads")?
        .into_iter()
        .map(|t| t.id)
        .filter(|id| id.starts_with("telegram:"))
        .collect();

    match telegram_threads.len() {
        1 => Ok((telegram_threads[0].clone(), None, ThreadSource::OnlyThread)),
        0 => bail!(
            "no Telegram thread is known yet — the bot has never received a message.\n\
             Send anything to the bot once, then retry."
        ),
        _ => bail!(
            "several Telegram threads are known and this session is not bound to one:\n  {}\n\
             Pick one explicitly:  claudebase telegram send --thread <id> --text \"…\"\n\
             or bind this session from Telegram with /switch.",
            telegram_threads.join("\n  ")
        ),
    }
}

/// `(routing_chat_id, routing_thread_id)` bound to `agent_id`, if any.
///
/// Reads the registry directly rather than asking the daemon: resolution has
/// to work even while the daemon is mid-restart, and the row is authoritative
/// either way.
fn routing_key_for(
    conn: &rusqlite::Connection,
    agent_id: &str,
) -> Result<Option<(i64, Option<i64>)>> {
    let mut stmt = conn
        .prepare(
            "SELECT routing_chat_id, routing_thread_id \
             FROM agent_registry \
             WHERE agent_id = ?1 AND routing_chat_id IS NOT NULL",
        )
        .context("prepare routing-key lookup")?;
    let mut rows = stmt.query([agent_id]).context("query routing key")?;
    if let Some(row) = rows.next().context("read routing-key row")? {
        let chat_id: i64 = row.get(0).context("routing_chat_id")?;
        let thread_id: Option<i64> = row.get(1).context("routing_thread_id")?;
        return Ok(Some((chat_id, thread_id)));
    }
    Ok(None)
}

/// Human-facing sender label persisted in `chat_messages.from_agent`.
///
/// This is a DISPLAY field, not an authorization claim — the daemon does not
/// grant anything based on it. Sender identity that actually gates behaviour
/// (agent-to-agent routing) is resolved server-side; see the token design in
/// the plan's Slice 1 notes.
fn sender_label(agent_id: Option<&str>) -> String {
    agent_id
        .map(|s| s.to_string())
        .unwrap_or_else(|| "cli".to_string())
}

/// Run `claudebase telegram send`.
pub async fn send(text: &str, explicit_thread: Option<&str>) -> Result<String> {
    if text.trim().is_empty() {
        bail!("refusing to send an empty message");
    }

    let agent_id = std::env::var(AGENT_ID_ENV).ok().filter(|s| !s.is_empty());
    let conn = chat::open_chat_db().context("open chat.db")?;
    let (thread, topic, source) = resolve_thread(&conn, explicit_thread, agent_id.as_deref())?;
    drop(conn);

    let mut client = DaemonClient::connect().await?;
    // `message_thread_id` is a string on the wire because that is what
    // `chat_reply` parses; omitted entirely when there is no topic, so a DM
    // send is byte-for-byte what it always was.
    let mut args = json!({
        "thread": thread,
        "content": text,
        "from": sender_label(agent_id.as_deref()),
    });
    if let Some(topic) = topic {
        args["message_thread_id"] = json!(topic.to_string());
    }
    let payload = client.call_tool("chat_reply", args).await?;

    let message_id = payload
        .get("id")
        .and_then(|v| v.as_str())
        .unwrap_or("<unknown>")
        .to_string();
    tracing::info!(%thread, ?topic, ?source, %message_id, "telegram send queued");
    match topic {
        Some(t) => Ok(format!("sent to {thread} topic {t} (message {message_id})")),
        None => Ok(format!("sent to {thread} (message {message_id})")),
    }
}

/// Run `claudebase telegram status` — what this session would send to, and why.
pub fn status() -> Result<String> {
    let agent_id = std::env::var(AGENT_ID_ENV).ok().filter(|s| !s.is_empty());
    let conn = chat::open_chat_db().context("open chat.db")?;

    let mut out = String::new();
    out.push_str(&format!(
        "session agent: {}\n",
        agent_id.as_deref().unwrap_or("<not set — session not launched via `claudebase run`>")
    ));

    match resolve_thread(&conn, None, agent_id.as_deref()) {
        Ok((thread, topic, source)) => {
            // The topic belongs in the answer to "where would this go", or the
            // status line says a forum-bound session targets the whole group.
            match topic {
                Some(t) => out.push_str(&format!(
                    "default target: {thread} topic {t} (resolved via {source:?})\n"
                )),
                None => out.push_str(&format!(
                    "default target: {thread} (resolved via {source:?})\n"
                )),
            }
        }
        Err(e) => out.push_str(&format!("default target: unresolved — {e}\n")),
    }

    let threads = chat::list_threads(&conn).context("list chat threads")?;
    let alive = agent_registry::list_alive(&conn, None).unwrap_or_default();
    out.push_str(&format!(
        "known telegram threads: {}\nalive agents: {}\n",
        threads
            .iter()
            .filter(|t| t.id.starts_with("telegram:"))
            .map(|t| t.id.as_str())
            .collect::<Vec<_>>()
            .join(", "),
        alive.len()
    ));
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// In-memory chat.db with just enough schema for the resolver.
    /// `ensure_chat_db_schema` creates `agent_registry` too (chat.rs:522),
    /// so one call covers both tables the resolver touches.
    fn fixture() -> rusqlite::Connection {
        let conn = rusqlite::Connection::open_in_memory().expect("open memory db");
        chat::ensure_chat_db_schema(&conn).expect("chat schema");
        conn
    }

    fn add_thread(conn: &rusqlite::Connection, id: &str) {
        conn.execute(
            "INSERT OR IGNORE INTO chat_threads (id, created_at) VALUES (?1, 0)",
            [id],
        )
        .expect("insert thread");
    }


    /// A session bound to a forum TOPIC must answer into that topic.
    ///
    /// The binding always knew the topic; this resolver read it as
    /// `(chat_id, _thread_id)` and dropped it, so a reply landed in the group's
    /// General while inbound, the binding and the registry all agreed on the
    /// topic. Verified live in a real forum before this test was written: the
    /// send logged `thread_id: None` and the operator confirmed the message
    /// appeared in General.
    #[test]
    fn a_session_bound_to_a_topic_answers_into_that_topic() {
        let conn = fixture();
        conn.execute(
            "INSERT INTO agent_registry \
             (agent_id, agent_name, connection_id, chat_thread_id, spawned_at, last_pinged_at, \
              state, routing_chat_id, routing_thread_id) \
             VALUES ('a-1', 'transport', 'c-1', NULL, 1, 1, 'alive', -1004451125152, 3)",
            [],
        )
        .expect("seed a topic-bound session");

        let (thread, topic, src) = resolve_thread(&conn, None, Some("a-1")).expect("resolve");
        assert_eq!(thread, "telegram:-1004451125152");
        assert_eq!(topic, Some(3), "the reply would land in General without this");
        assert_eq!(src, ThreadSource::RoutingKey);
    }

    /// A DM-bound session has no topic, and must stay byte-for-byte as before.
    #[test]
    fn a_dm_bound_session_resolves_no_topic() {
        let conn = fixture();
        conn.execute(
            "INSERT INTO agent_registry \
             (agent_id, agent_name, connection_id, chat_thread_id, spawned_at, last_pinged_at, \
              state, routing_chat_id, routing_thread_id) \
             VALUES ('a-2', 'mira', 'c-2', NULL, 1, 1, 'alive', 434566766, NULL)",
            [],
        )
        .expect("seed a DM-bound session");

        let (thread, topic, src) = resolve_thread(&conn, None, Some("a-2")).expect("resolve");
        assert_eq!(thread, "telegram:434566766");
        assert_eq!(topic, None);
        assert_eq!(src, ThreadSource::RoutingKey);
    }

    #[test]
    fn explicit_thread_wins_and_is_validated() {
        let conn = fixture();
        let (t, _topic, src) = resolve_thread(&conn, Some("telegram:42"), None).expect("resolve");
        assert_eq!(t, "telegram:42");
        assert_eq!(src, ThreadSource::Explicit);

        let err = resolve_thread(&conn, Some("agent:foo"), None).expect_err("must reject");
        assert!(err.to_string().contains("telegram:<chat_id>"));
    }

    #[test]
    fn single_known_thread_is_used_when_unbound() {
        let conn = fixture();
        add_thread(&conn, "telegram:99");
        let (t, _topic, src) = resolve_thread(&conn, None, None).expect("resolve");
        assert_eq!(t, "telegram:99");
        assert_eq!(src, ThreadSource::OnlyThread);
    }

    #[test]
    fn ambiguity_fails_loudly_and_lists_candidates() {
        let conn = fixture();
        add_thread(&conn, "telegram:1");
        add_thread(&conn, "telegram:2");
        let err = resolve_thread(&conn, None, None).expect_err("must not guess");
        let msg = err.to_string();
        assert!(msg.contains("telegram:1") && msg.contains("telegram:2"));
    }

    #[test]
    fn no_threads_at_all_explains_itself() {
        let conn = fixture();
        let err = resolve_thread(&conn, None, None).expect_err("must fail");
        assert!(err.to_string().contains("never received a message"));
    }
}

// ---------------------------------------------------------------------------
// Slice 8 — bot registry surface
// ---------------------------------------------------------------------------

/// `claudebase telegram addbot <token>`.
///
/// The token is VERIFIED against Telegram before it is stored: `getMe` both
/// proves the token works and returns the bot's id and username, which is what
/// makes re-adding the same bot a rotation rather than a duplicate. Storing
/// first and discovering later that the operator pasted a dead token is the
/// failure mode this ordering removes.
pub async fn addbot(token: &str, label: Option<&str>) -> Result<String> {
    use crate::daemon::bots;

    let token = token.trim();
    let claimed_id = bots::validate_token_shape(token)?;

    let (bot_id, username) = get_me(token).await?;
    if bot_id != claimed_id {
        // Telegram disagreeing with the id embedded in the token means the
        // paste is mangled; refuse rather than store something inconsistent.
        bail!("token id {claimed_id} does not match the bot Telegram reports ({bot_id})");
    }

    let conn = chat::open_chat_db().context("open chat.db")?;
    let is_default = bots::upsert(
        &conn,
        bot_id,
        &username,
        token,
        label,
        chat::now_millis(),
    )?;

    Ok(format!(
        "stored @{username} (id {bot_id}, token {}){}\n\
         The daemon reads the registry at startup — restart it to pick this up:\n  \
         claudebase daemon restart",
        bots::mask(token),
        if is_default { " — default bot" } else { "" }
    ))
}

/// `claudebase telegram bots` — list registered bots, never revealing secrets.
pub fn list_bots() -> Result<String> {
    use crate::daemon::bots;

    let conn = chat::open_chat_db().context("open chat.db")?;
    let rows = bots::list(&conn)?;
    if rows.is_empty() {
        return Ok("no bots registered — add one with `claudebase telegram addbot <token>`\n"
            .to_string());
    }
    let mut out = String::new();
    for b in rows {
        out.push_str(&format!(
            "{} @{} (id {}){}\n",
            if b.is_default { "*" } else { " " },
            b.username,
            b.bot_id,
            b.label.map(|l| format!(" — {l}")).unwrap_or_default()
        ));
    }
    out.push_str("\n* = default bot used by the daemon\n");
    Ok(out)
}

/// Call Telegram's `getMe` and return the **raw** response body.
///
/// Raw rather than parsed because that is what an operator debugging a channel
/// wants: `can_join_groups`, `can_read_all_group_messages`,
/// `supports_inline_queries` and whatever Telegram adds next all matter when
/// the bot behaves oddly, and a summary would hide exactly the field that
/// explains it. The body carries no secret — the token travels in the URL, not
/// the response.
///
/// Errors are scrubbed: reqwest embeds the request URL — which carries the
/// token — in its Display output, and an error message is exactly the kind of
/// string that ends up pasted into an issue.
pub async fn get_me_raw(token: &str) -> Result<serde_json::Value> {
    let url = format!("https://api.telegram.org/bot{token}/getMe");
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(15))
        .build()
        .context("build http client")?;

    let resp = client
        .get(&url)
        .send()
        .await
        .map_err(|e| anyhow::anyhow!("Telegram getMe failed: {}", scrub(&e.to_string(), token)))?;

    let status = resp.status();
    let text = resp
        .text()
        .await
        .map_err(|e| anyhow::anyhow!("reading getMe body failed: {}", scrub(&e.to_string(), token)))?;

    serde_json::from_str::<serde_json::Value>(&text).map_err(|e| {
        anyhow::anyhow!(
            "Telegram returned non-JSON ({status}): {} — body: {}",
            e,
            scrub(&text, token)
        )
    })
}

/// Parsed `(bot_id, username)`, for callers that need identity rather than the
/// full body (`addbot`).
pub async fn get_me(token: &str) -> Result<(i64, String)> {
    let body = get_me_raw(token).await?;
    if body.get("ok").and_then(|v| v.as_bool()) != Some(true) {
        let desc = body
            .get("description")
            .and_then(|d| d.as_str())
            .unwrap_or("no description");
        bail!("Telegram rejected the token: {desc}");
    }
    let result = body.get("result").context("getMe: no result object")?;
    let bot_id = result
        .get("id")
        .and_then(|v| v.as_i64())
        .context("getMe: no bot id")?;
    let username = result
        .get("username")
        .and_then(|v| v.as_str())
        .unwrap_or("unknown")
        .to_string();
    Ok((bot_id, username))
}

fn scrub(text: &str, token: &str) -> String {
    if token.is_empty() {
        return text.to_string();
    }
    text.replace(token, "***")
}

/// `claudebase telegram get_me [--token <token>]` — print Telegram's raw
/// `getMe` response.
///
/// With no argument it checks the REGISTERED default bot, which is the
/// question an operator actually has: "is the thing my daemon uses still
/// alive?" A revoked token fails here in a second instead of showing up as
/// silence in the channel. Exit code follows `ok`, so it is usable in scripts
/// even though the output is the verbatim API body.
pub async fn get_me_command(explicit_token: Option<&str>) -> Result<String> {
    use crate::daemon::bots;

    let token = match explicit_token {
        Some(t) => t.trim().to_string(),
        None => {
            let conn = chat::open_chat_db().context("open chat.db")?;
            match bots::default_token(&conn)? {
                Some(t) => t,
                None => bail!(
                    "no bot registered — add one with `claudebase telegram addbot <token>`, \
                     or pass --token to check a token before registering it"
                ),
            }
        }
    };

    let body = get_me_raw(&token).await?;
    let rendered = serde_json::to_string_pretty(&body).unwrap_or_else(|_| body.to_string());
    if body.get("ok").and_then(|v| v.as_bool()) == Some(true) {
        Ok(rendered)
    } else {
        // Still show the body — the `description` field is the whole answer —
        // but fail so scripts and humans both notice.
        bail!("{rendered}")
    }
}
