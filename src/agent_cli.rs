//! `claudebase agent send` / `agent list` — agent-to-agent messaging over the CLI.
//!
//! Slice 1 of the pty-transport feature, per the operator's decision: the
//! cli-to-cli routing shipped in v0.8 reached peers through the `agent_send`
//! MCP tool, which dies with the plugin bridge. Moving it to the CLI is what
//! lets the bridge be deleted.
//!
//! ## Sender identity
//!
//! `agent_send` resolves the sender server-side from the connection's
//! registered identity (FR-C2C-4.6) so no agent can claim to be another. A
//! short-lived CLI process has no registered connection, so it presents the
//! `(agent_id, session_token)` pair the PTY supervisor exported into its
//! environment; the daemon validates the token against the registry row. A
//! bare `--from` is deliberately NOT accepted — that would turn an enforced
//! property into an honour system.

use anyhow::{bail, Context, Result};
use rusqlite::OptionalExtension;
use serde_json::json;

use crate::daemon::client::DaemonClient;
use crate::daemon::{agent_registry, chat};

pub const AGENT_ID_ENV: &str = "CLAUDEBASE_AGENT_ID";
pub const SESSION_TOKEN_ENV: &str = "CLAUDEBASE_SESSION_TOKEN";

/// Send a direct message to another Claude session on this daemon.
pub async fn send(text: &str, target: &str, target_is_id: bool) -> Result<String> {
    if text.trim().is_empty() {
        bail!("refusing to send an empty message");
    }

    let from_agent_id = std::env::var(AGENT_ID_ENV).ok().filter(|s| !s.is_empty());
    let session_token = std::env::var(SESSION_TOKEN_ENV)
        .ok()
        .filter(|s| !s.is_empty());

    let to_agent_id = {
        let conn = chat::open_chat_db().context("open chat.db")?;
        if target_is_id {
            target.to_string()
        } else {
            agent_registry::resolve_target(&conn, target)?
        }
    };


    if Some(&to_agent_id) == from_agent_id.as_ref() {
        bail!("refusing to send a message to this session itself");
    }

    let mut args = json!({ "to_agent_id": to_agent_id, "content": text });
    match (from_agent_id.as_deref(), session_token.as_deref()) {
        (Some(id), Some(token)) => {
            args["from_agent_id"] = json!(id);
            args["session_token"] = json!(token);
        }
        _ => bail!(
            "this session has no claudebase identity ({AGENT_ID_ENV} / {SESSION_TOKEN_ENV} unset).\n\
             Agent-to-agent messages are only available in sessions launched with `claudebase run`."
        ),
    }

    let mut client = DaemonClient::connect().await?;
    let payload = client.call_tool("agent_send", args).await?;

    // The daemon answers `{queued: true, delivered_when}` when the recipient is
    // in DND — that is a success, not a failure, and the caller should see the
    // difference.
    if payload.get("queued").and_then(|v| v.as_bool()) == Some(true) {
        let when = payload
            .get("delivered_when")
            .and_then(|v| v.as_i64())
            .unwrap_or_default();
        return Ok(format!(
            "queued for {to_agent_id} — recipient is in DND until {when}"
        ));
    }
    Ok(format!("delivered to {to_agent_id}"))
}

/// `claudebase agent describe "<what this session is working on>"`.
///
/// Publishes into this session's registry row so peers see it in
/// `claudebase agent list`. Replaces the `agent_describe` MCP tool; identity is
/// proven with the same session token as `send`, so a stray process cannot
/// rewrite another session's label.
pub async fn describe(description: &str, branch: Option<&str>) -> Result<String> {
    if description.trim().is_empty() {
        bail!("describe what this session is doing, e.g. `claudebase agent describe \"pty transport\"`");
    }
    let from_agent_id = std::env::var(AGENT_ID_ENV).ok().filter(|s| !s.is_empty());
    let session_token = std::env::var(SESSION_TOKEN_ENV)
        .ok()
        .filter(|s| !s.is_empty());

    let (id, token) = match (from_agent_id, session_token) {
        (Some(i), Some(t)) => (i, t),
        _ => bail!(
            "this session has no claudebase identity ({AGENT_ID_ENV} / {SESSION_TOKEN_ENV} unset).\n\
             Start the session with `claudebase run` to make it visible to peers."
        ),
    };

    let mut args = json!({
        "description": description,
        "from_agent_id": id,
        "session_token": token,
    });
    if let Some(b) = branch {
        args["branch"] = json!(b);
    }

    let mut client = DaemonClient::connect().await?;
    client.call_tool("agent_describe", args).await?;
    Ok(format!("published: {description}"))
}

/// `claudebase agent rename <nick>` — rename THIS session.
///
/// Only this session: identity comes from the token the supervisor exported, so
/// there is no way to rename a neighbour by accident. Renaming someone else is
/// a different operation with a different trust rule and is not offered here.
pub async fn rename(new_nick: &str) -> Result<String> {
    let nick = new_nick.trim();
    if nick.is_empty() {
        bail!("pass the new nick: claudebase agent rename <nick>");
    }
    let id = std::env::var(AGENT_ID_ENV).ok().filter(|s| !s.is_empty());
    let token = std::env::var(SESSION_TOKEN_ENV).ok().filter(|s| !s.is_empty());
    let (id, token) = match (id, token) {
        (Some(i), Some(t)) => (i, t),
        _ => bail!(
            "this session has no claudebase identity ({AGENT_ID_ENV} / {SESSION_TOKEN_ENV} unset).\n\
             Only sessions started with `claudebase run` have a nick to change."
        ),
    };

    let mut client = DaemonClient::connect().await?;
    client
        .call_tool(
            "agent_rename",
            json!({ "name": nick, "from_agent_id": id, "session_token": token }),
        )
        .await?;
    Ok(format!(
        "this session is now `{nick}` — it appears under that name in `claudebase agent list` \
         and in the Telegram /switch menu"
    ))
}

/// One row of `claudebase agent list`.
#[derive(Debug, serde::Serialize)]
pub struct SessionRow {
    /// Host and pid recorded at register, used to verify the row is not stale.
    pub host: Option<String>,
    pub pid: Option<i64>,
    /// Human-addressable name — this is what `--agent_nick` takes.
    pub nick: String,
    /// Stable id for this session; the disambiguator when nicks collide.
    pub agent_id: String,
    /// `online` while the session holds a live daemon connection.
    pub status: &'static str,
    pub project: Option<String>,
    pub branch: Option<String>,
    pub working_dir: Option<String>,
    pub feature: Option<String>,
    pub last_seen_ms: i64,
}

/// `claudebase agent list [--all] [--json]`.
///
/// Status comes from the registry `state` column: a session is `online` while
/// its supervisor holds the daemon connection, and flips to `offline` when the
/// connection drops (the daemon marks rows orphaned on EOF). That makes the
/// list an accurate answer to "who can receive a message right now", which is
/// the only question it needs to answer.
pub fn list(include_offline: bool, as_json: bool) -> Result<String> {
    let conn = chat::open_chat_db().context("open chat.db")?;
    let mut stmt = conn
        .prepare(
            "SELECT agent_id, agent_name, state, project_id, branch, working_dir, \
                    feature_description, last_pinged_at, host, pid \
             FROM agent_registry \
             WHERE state != 'dead' OR ?1 \
             ORDER BY (state = 'alive') DESC, last_pinged_at DESC",
        )
        .context("prepare session list")?;
    let rows: Vec<SessionRow> = stmt
        .query_map([include_offline], |row| {
            let state: String = row.get(2)?;
            Ok(SessionRow {
                agent_id: row.get(0)?,
                nick: row.get(1)?,
                status: if state == "alive" { "online" } else { "offline" },
                project: row.get(3)?,
                branch: row.get(4)?,
                working_dir: row.get(5)?,
                feature: row.get(6)?,
                last_seen_ms: row.get(7)?,
                host: row.get(8)?,
                pid: row.get(9)?,
            })
        })
        .context("query sessions")?
        .collect::<rusqlite::Result<Vec<_>>>()
        .context("read session rows")?;

    // Cross-check `state` against the process table: a row stays `alive` when a
    // session dies while the daemon is down, so `state` alone over-reports.
    let host = crate::daemon::agent_registry::this_host();
    let verified: Vec<SessionRow> = rows
        .into_iter()
        .map(|mut r| {
            if r.status == "online"
                && !crate::daemon::agent_registry::process_is_live(
                    r.host.as_deref(),
                    r.pid,
                    &host,
                )
            {
                r.status = "offline";
            }
            r
        })
        .collect();

    let rows: Vec<SessionRow> = if include_offline {
        verified
    } else {
        verified.into_iter().filter(|r| r.status == "online").collect()
    };

    if as_json {
        return serde_json::to_string_pretty(&rows).context("serialise session list");
    }
    if rows.is_empty() {
        return Ok(if include_offline {
            "no sessions known — start one with `claudebase run`\n".to_string()
        } else {
            "no sessions online — start one with `claudebase run`, or pass --all\n".to_string()
        });
    }

    let nick_w = rows.iter().map(|r| r.nick.len()).max().unwrap_or(4).max(4);
    let mut out = format!(
        "{:<nick_w$}  {:<36}  {:<7}  {}\n",
        "NICK", "AGENT_ID", "STATUS", "WORKING ON"
    );
    for r in &rows {
        let what = r
            .feature
            .clone()
            .or_else(|| r.working_dir.clone())
            .unwrap_or_else(|| "—".to_string());
        out.push_str(&format!(
            "{:<nick_w$}  {:<36}  {:<7}  {}\n",
            r.nick, r.agent_id, r.status, what
        ));
    }
    out.push_str("\nsend with: claudebase agent send \"текст\" --agent_nick <NICK>\n");
    Ok(out)
}

#[cfg(test)]
mod tests {
    use crate::daemon::{agent_registry, chat};

    fn fixture() -> rusqlite::Connection {
        let conn = rusqlite::Connection::open_in_memory().expect("memory db");
        chat::ensure_chat_db_schema(&conn).expect("schema");
        conn
    }

    fn add_agent(conn: &rusqlite::Connection, id: &str, name: &str, seen: i64, state: &str) {
        conn.execute(
            "INSERT INTO agent_registry \
             (agent_id, agent_name, connection_id, spawned_at, last_pinged_at, state) \
             VALUES (?1, ?2, 'conn', 0, ?3, ?4)",
            rusqlite::params![id, name, seen, state],
        )
        .expect("insert agent");
    }

    #[test]
    fn resolves_exact_id() {
        let c = fixture();
        add_agent(&c, "id-1", "alpha", 10, "alive");
        assert_eq!(agent_registry::resolve_target(&c, "id-1").unwrap(), "id-1");
    }

    #[test]
    fn resolves_unique_name() {
        let c = fixture();
        add_agent(&c, "id-1", "alpha", 10, "alive");
        assert_eq!(agent_registry::resolve_target(&c, "alpha").unwrap(), "id-1");
    }

    #[test]
    fn ambiguous_name_refuses_and_lists_candidates() {
        let c = fixture();
        add_agent(&c, "id-1", "alpha", 10, "alive");
        add_agent(&c, "id-2", "alpha", 20, "alive");
        let err = agent_registry::resolve_target(&c, "alpha").expect_err("must not guess");
        let msg = err.to_string();
        assert!(msg.contains("id-1") && msg.contains("id-2"));
    }

    #[test]
    fn dead_agents_are_not_targets() {
        let c = fixture();
        add_agent(&c, "id-1", "alpha", 10, "dead");
        assert!(agent_registry::resolve_target(&c, "alpha").is_err());
    }

    #[test]
    fn session_token_must_match_an_alive_row() {
        let c = fixture();
        add_agent(&c, "id-1", "alpha", 10, "alive");
        agent_registry::set_session_token(&c, "id-1", "secret").unwrap();

        assert!(agent_registry::resolve_session_token(&c, "id-1", "secret").unwrap());
        assert!(!agent_registry::resolve_session_token(&c, "id-1", "wrong").unwrap());
        assert!(
            !agent_registry::resolve_session_token(&c, "id-1", "").unwrap(),
            "an empty token must never authenticate"
        );
        assert!(
            !agent_registry::resolve_session_token(&c, "id-unknown", "secret").unwrap(),
            "token of one agent must not authenticate another"
        );
    }

    #[test]
    fn token_of_a_dead_agent_stops_working() {
        let c = fixture();
        add_agent(&c, "id-1", "alpha", 10, "alive");
        agent_registry::set_session_token(&c, "id-1", "secret").unwrap();
        c.execute("UPDATE agent_registry SET state='dead' WHERE agent_id='id-1'", [])
            .unwrap();
        assert!(!agent_registry::resolve_session_token(&c, "id-1", "secret").unwrap());
    }
}

/// `claudebase agent whoami` — this session's own name, and where it came from.
///
/// The `origin` line is the load-bearing part, and it exists for the
/// SessionStart hook rather than for a human. The hook asks a fresh session to
/// give itself a distinctive name, but it must ask ONCE: a session that already
/// carries a chosen name has a Telegram `/switch` binding pointing at that name
/// (`chat_bindings` is keyed by nick), so renaming it again on every start
/// would break delivery — the very bug the nick memory closes.
///
/// * `chosen` — a name was set with `agent rename` in this directory and is
///   remembered across restarts.
/// * `auto` — the name fell out of the project directory, so every window
///   opened here shares it.
///
/// Read-only and daemon-free on purpose: a hook must not hang on a busy or
/// absent daemon at session start.
pub fn whoami() -> Result<String> {
    let id = std::env::var(AGENT_ID_ENV).ok().filter(|s| !s.is_empty());
    let Some(id) = id else {
        bail!(
            "this session has no claudebase identity ({AGENT_ID_ENV} unset).\n\
             Only sessions started with `claudebase run` have a nick."
        );
    };

    let conn = crate::daemon::chat::open_chat_db_readonly()
        .context("open chat.db (read-only) for whoami")?;

    let nick: Option<String> = conn
        .query_row(
            "SELECT agent_name FROM agent_registry WHERE agent_id = ?1",
            [&id],
            |row| row.get(0),
        )
        .optional()
        .context("look up this session's registry row")?;
    let Some(nick) = nick else {
        bail!("no registry row for {id} — is the daemon running?");
    };

    let cwd = std::env::current_dir()
        .map(|c| c.to_string_lossy().into_owned())
        .unwrap_or_default();
    let host = crate::daemon::agent_registry::this_host();
    let remembered =
        crate::daemon::agent_registry::recall_nick(&conn, &host, &cwd).unwrap_or(None);

    // Chosen only if the memory names THIS nick: a stale memory pointing at a
    // name the session no longer carries must not silence the hook.
    let origin = match remembered.as_deref() {
        Some(r) if r == nick => "chosen",
        _ => "auto",
    };

    Ok(format!(
        "nick: {nick}\nagent_id: {id}\norigin: {origin}\ncwd: {cwd}"
    ))
}
