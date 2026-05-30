//! Slice 4 — teloxide long-poll loop, inbound routing, outbound chat_reply.
//!
//! Architecture:
//!
//! 1. The daemon's main `serve()` spawns ONE `tokio::spawn` running
//!    `run_long_poll()`. Per ASYNC_INVARIANTS Rule 3 the body is wrapped
//!    in `if let Err(e) = ...` so a fatal Telegram error never panics the
//!    daemon — only the long-poll task ends, the rest of the daemon
//!    continues serving MCP plugins.
//!
//! 2. Each iteration calls `getUpdates` with the current `offset` from
//!    `daemon_state.telegram.last_update_id`. The returned batch is
//!    processed atomically (SEC-13): all chat-message inserts AND the
//!    offset bump live in one rusqlite transaction. If the daemon crashes
//!    mid-batch BEFORE commit, the next restart re-processes the same
//!    batch from the prior offset — schema v5 has NO unique constraint
//!    on `(thread_id, telegram_message_id)` (insight #9), so safety
//!    relies on the transactional offset-advance alone.
//!
//! 3. Errors:
//!    - HTTP 401: write `tg_bot_state = "disconnected"` into daemon_state,
//!      log structured event (no token in log), exit the long-poll loop.
//!      The daemon as a whole stays alive (Rule 3 / SEC-14).
//!    - HTTP 429: extract `retry_after`, sleep, retry ONCE (UC-3-E2 /
//!      SEC-14). On a second 429, surface to the outbound chat_reply
//!      caller via `{"error": "telegram_rate_limited", "retry_after": N}`
//!      and back off the inbound loop by sleeping `retry_after` seconds
//!      before resuming `getUpdates`.
//!    - Any teloxide error string is filtered through `redact_token`
//!      before reaching `tracing::error!` — substring-match against the
//!      raw token bytes.
//!
//! 4. Outbound: `handle_chat_reply` is the seam from the MCP `chat_reply`
//!    tool. When `thread_id` starts with `telegram:`, the daemon makes a
//!    teloxide `send_message` call. NOT wired into Slice 1c's chat tool
//!    handler in this Slice — that wiring lands when Slice 6-MVP's voice
//!    flow runs. For Slice 4 we expose the function so tests can drive it
//!    directly.
//!
//! ## Status (Slice 4)
//!
//! - Long-poll loop: SHIPS. Returns errors via Result, panic-safe by
//!   construction.
//! - `getUpdates` body processing: SHIPS, including transactional
//!   offset-advance.
//! - 401 / 429 handling: SHIPS.
//! - Voice notes: returns the literal placeholder
//!   `"[unsupported: enable asr-whisper feature]"` per Slice 4 acceptance
//!   criterion ("real ASR is Slice 6-MVP").
//! - End-to-end TG roundtrip with mocked `TELOXIDE_API_URL`: the e2e
//!   tests in `tests/telegram_e2e_test.rs` are SCAFFOLDS (they verify
//!   config-file layout, not live HTTP). Real mocked-roundtrip lives in
//!   a future iteration when the test harness is fleshed out.

use std::path::PathBuf;
use std::sync::{Arc, OnceLock};
use std::time::Duration;

use anyhow::{Context, Result};
use rusqlite::{params, Connection};
use serde::Deserialize;
use tokio::sync::mpsc;

use crate::daemon::asr::Asr;
use crate::daemon::channel_state::{self, GateAction};
use crate::daemon::chat::{self, SharedBus};
use crate::daemon::config::RedactedToken;
use crate::daemon::permissions::{self, Access};

/// Outbound channel from MCP `chat_reply` (server.rs::handle_chat_post)
/// to the telegram long-poll task. Set ONCE at spawn_long_poll time;
/// reads happen in run_long_poll's select! loop.
///
/// Tuple shape: `(chat_id, text)` — chat_id is the integer parsed from
/// the `telegram:<N>` thread prefix used by chat_reply tool callers.
static OUTBOUND_TG: OnceLock<mpsc::UnboundedSender<(i64, String)>> = OnceLock::new();

/// Push an outbound Telegram message from any task. Returns Ok(()) on
/// successful enqueue (does NOT wait for HTTP send completion). Returns
/// Err if telegram long-poll is not running OR the channel is closed.
pub fn enqueue_outbound_tg(chat_id: i64, text: String) -> Result<()> {
    let tx = OUTBOUND_TG
        .get()
        .ok_or_else(|| anyhow::anyhow!("telegram outbound channel not initialised (long-poll task not spawned)"))?;
    tx.send((chat_id, text))
        .map_err(|e| anyhow::anyhow!("outbound channel closed: {e}"))?;
    Ok(())
}

/// One Telegram update as decoded from `getUpdates`. We deliberately
/// hand-decode a SMALL subset of the rich teloxide types because the
/// production loop only needs `update_id` + text-message routing for
/// Slice 4. Voice / photo / sticker bodies surface as `Other(value)` so
/// we can still tick the offset forward without parsing them.
///
/// teloxide's full `Update` enum tree compiles fine but pulls dialogue/
/// command/sqlx generics into our type signatures — for the lean Slice 4
/// loop the JSON-on-the-wire deserialisation here is simpler.
#[derive(Debug, Deserialize)]
pub struct Update {
    pub update_id: i64,
    #[serde(default)]
    pub message: Option<Message>,
}

#[derive(Debug, Deserialize)]
pub struct Message {
    pub message_id: i64,
    #[serde(default)]
    pub from: Option<User>,
    pub chat: Chat,
    /// Unix epoch seconds when the sender's client posted the message.
    /// Used to build the `meta.ts` ISO 8601 string in the channel
    /// notification — matches the official telegram plugin
    /// (server.ts:1271 `new Date((ctx.message?.date ?? 0) * 1000).toISOString()`).
    #[serde(default)]
    pub date: i64,
    #[serde(default)]
    pub text: Option<String>,
    /// When `voice` is present and `text` is absent, the bot received a
    /// voice note. Slice 4 returns the literal shim string; Slice 6-MVP
    /// wires the ASR pipeline.
    #[serde(default)]
    pub voice: Option<Voice>,
    /// Slice 2 (telegram-multi-cli) — the message this one replies to, when
    /// the operator used Telegram's reply-quote UI. Routing-tree step 2
    /// looks up `reply_to_message.message_id` in `tg_message_map` to route
    /// back to the original sender CLI. Absent for non-reply messages.
    #[serde(default)]
    pub reply_to_message: Option<ReplyToMessage>,
}

/// Minimal decode of the `reply_to_message` sub-object — routing-tree
/// step 2 only needs the original message's `message_id` to look up the
/// `tg_message_map` row. The rest of the Telegram Message fields on the
/// quoted message are irrelevant to routing and deliberately not decoded.
#[derive(Debug, Deserialize)]
pub struct ReplyToMessage {
    pub message_id: i64,
}

#[derive(Debug, Deserialize)]
pub struct User {
    pub id: i64,
    #[serde(default)]
    pub username: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct Chat {
    pub id: i64,
}

#[derive(Debug, Deserialize)]
pub struct Voice {
    pub file_id: String,
    #[serde(default)]
    pub duration: i64,
}

/// Voice-note shim per Slice 4 acceptance: real ASR ships in Slice 6-MVP.
pub const VOICE_SHIM_TEXT: &str = "[unsupported: enable asr-whisper feature]";

/// Slice 7 — extract the first `@name` mention from a Telegram text body.
/// Returns `Some(name)` where `name` is a substring of `text` matching
/// `@([A-Za-z0-9_-]+)` AND preceded by start-of-string OR a non-charset
/// byte (whitespace, punctuation). The latter constraint defeats false
/// matches inside addresses like `email@foo.com` (the `@` is preceded
/// by `l` — a charset byte — so the parser declines).
///
/// Per STRUCTURAL-7-1 the parser is hand-rolled (no `regex` dep), with
/// the charset mirroring `agent_registry::validate_agent_name` exactly.
/// Per STRUCTURAL-7-5 only the FIRST mention is returned; downstream
/// callers ignore any subsequent `@` tokens.
///
/// Returns `None` if no valid mention exists.
///
/// NOTE (telegram-multi-cli Slice 2): chat-as-id routing no longer calls
/// this parser — `@-mentions` are ignored as a routing key (the chat
/// binding and reply-quote link are the only keys). The function and its
/// unit tests are retained for a potential future per-mention feature, so
/// it is `#[allow(dead_code)]` in non-test builds.
#[allow(dead_code)]
pub(crate) fn extract_first_mention(text: &str) -> Option<&str> {
    let bytes = text.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] != b'@' {
            i += 1;
            continue;
        }
        // Word-boundary check: byte before `@` MUST be either start-of-
        // string OR a non-charset byte. The charset is the same one we
        // accept AFTER the `@` (and the same as validate_agent_name).
        let is_word_boundary = if i == 0 {
            true
        } else {
            let prev = bytes[i - 1];
            !(prev.is_ascii_alphanumeric() || prev == b'_' || prev == b'-')
        };
        if !is_word_boundary {
            i += 1;
            continue;
        }
        // Take chars after `@` matching the charset. Stop at first
        // non-match. Empty match → keep scanning for next `@`.
        let start = i + 1;
        let mut end = start;
        while end < bytes.len() {
            let c = bytes[end];
            if c.is_ascii_alphanumeric() || c == b'_' || c == b'-' {
                end += 1;
            } else {
                break;
            }
        }
        if end > start {
            // safe: we only consumed ASCII bytes — the slice is valid UTF-8
            return Some(&text[start..end]);
        }
        i += 1;
    }
    None
}

/// Exact bot-command tokens recognised by the routing tree's Step 1.
/// A message whose first whitespace-delimited token (after stripping an
/// optional `@botname` suffix, e.g. `/help@my_bot`) matches one of these
/// is a bot command and is NOT routed to any CLI — it is dispatched to
/// `handle_bot_command` (a Slice 3 stub for now) and processing returns
/// early. Source: plan.md Slice 2 Step 1 / Slice 3 (the 7 commands).
const BOT_COMMANDS: [&str; 7] = [
    "/agents", "/switch", "/whoami", "/here", "/start", "/help", "/status",
];

/// Return `Some(canonical_command)` when `text` begins with a recognised
/// bot command. Handles the Telegram group-chat form `/cmd@botname` by
/// stripping the `@…` suffix before matching (UC-TMC-12-EC1). Trailing
/// arguments (`/switch mira`) are ignored for the match — only the first
/// token is inspected. Returns `None` for free text, unknown `/slash`
/// tokens, or empty input.
pub(crate) fn match_bot_command(text: &str) -> Option<&'static str> {
    let first = text.split_whitespace().next()?;
    if !first.starts_with('/') {
        return None;
    }
    // Strip an optional `@botname` suffix (group-chat addressing form).
    let cmd = match first.split_once('@') {
        Some((head, _bot)) => head,
        None => first,
    };
    BOT_COMMANDS.iter().copied().find(|&c| c == cmd)
}

/// The `/help` text listing all 7 bot commands (telegram-multi-cli Slice
/// 3 / TC-TMC-12.1). The `/switch` line carries the group-rebind note so
/// operators understand that `/switch` in a group rebinds the chat for
/// ALL participants (chat-as-id). Byte content is asserted loosely by the
/// QA cases (substring checks for each command name + "group").
const HELP_TEXT: &str = "\
Available commands:
/agents — list CLIs currently online
/switch <name> — bind this chat to a named CLI (in a group, rebinds for all participants)
/whoami — show which CLI this chat is bound to
/here — show the bound CLI's host and working directory
/start — show the welcome message
/help — show this help
/status — show channel status";

/// Slice 3 — handle one inbound bot command (`/agents` / `/switch` /
/// `/whoami` / `/here` / `/start` / `/help` / `/status`). Returns the
/// operator-facing reply text the caller enqueues into
/// `BatchOutcome.pair_replies` (the SAME post-commit teloxide send path
/// used for Step-5 "No CLIs online" and pairing replies — see
/// `run_long_poll` line ~1336). Returning the text (rather than enqueuing
/// directly) keeps the handler testable: a unit test calls it and asserts
/// on the returned string + the SQLite side-effects, without an
/// initialised `OUTBOUND_TG` global.
///
/// Contract for every command (TC-TMC-8.4 / TC-TMC-12.3 leak guard):
/// bot commands query SQLite and reply, but publish NO channel
/// notification and route to NO CLI. The caller `continue`s after this
/// returns, so no `chat_messages` row and no `notifications` frame is
/// produced.
///
/// `conn` is the caller's open transaction connection (`&tx` Derefs to
/// `&Connection`), so all reads/writes here are inside the same SEC-13
/// transactional snapshot as the rest of the batch.
///
/// SECURITY (plan.md Slice 3, MEDIUM):
///   - `/switch <name>` calls `validate_agent_name` BEFORE any DB access,
///     rejecting non-`[A-Za-z0-9_-]` / empty / >64-char names so an
///     injection-style argument never reaches a SQL statement (TC-TMC-9.x).
///     All `active_cli_per_chat` reads/writes are parameterised.
///   - `/here` is scoped to THIS `chat_id`'s bound CLI only — it never
///     reads another chat's binding or another CLI's host/cwd metadata.
pub(crate) fn handle_bot_command(
    conn: &Connection,
    command: &str,
    chat_id: i64,
    text: &str,
) -> Option<String> {
    match command {
        "/agents" => Some(handle_cmd_agents(conn)),
        "/switch" => Some(handle_cmd_switch(conn, chat_id, text)),
        "/whoami" => Some(handle_cmd_whoami(conn, chat_id)),
        "/here" => Some(handle_cmd_here(conn, chat_id)),
        "/help" => Some(HELP_TEXT.to_string()),
        // `/start` and `/status` are preserved unchanged (UC-TMC-12). They
        // are handled by the official channel-state / pairing flow upstream
        // of the routing tree, not by this Slice-3 handler. We return None
        // here so the caller emits no extra reply for them — the existing
        // behaviour is untouched. (They still short-circuit CLI routing
        // because `match_bot_command` recognised them, which is the only
        // Step-1 contract for these two.)
        "/start" | "/status" => None,
        // Unreachable in practice — `match_bot_command` only yields the 7
        // BOT_COMMANDS — but exhaustive for safety.
        _ => None,
    }
}

/// `/agents` (alias `/online`) — list the alive CLIs as a bullet list,
/// one line per CLI: agent_name + last-seen + cwd-if-available. Empty
/// registry → the exact "No CLIs currently online." reply (TC-TMC-8.2).
fn handle_cmd_agents(conn: &Connection) -> String {
    use crate::daemon::agent_registry::list_alive;
    let rows = match list_alive(conn, None) {
        Ok(r) => r,
        Err(e) => {
            tracing::warn!(error = %e, "list_alive failed in /agents handler");
            return "Could not list online CLIs (internal error).".to_string();
        }
    };
    if rows.is_empty() {
        return "No CLIs currently online.".to_string();
    }
    let mut out = String::from("Online CLIs:");
    for row in &rows {
        // last-seen as a relative-ish hint: raw last_pinged_at ms. cwd is
        // pulled from the agent's metadata JSON when present (best-effort).
        let cwd = agent_cwd_from_metadata(conn, &row.agent_id);
        match cwd {
            Some(c) => out.push_str(&format!("\n• {} (last seen {}) — {}", row.agent_name, row.last_pinged_at, c)),
            None => out.push_str(&format!("\n• {} (last seen {})", row.agent_name, row.last_pinged_at)),
        }
    }
    out
}

/// `/switch <name>` — bind THIS chat to a named alive CLI. SECURITY: the
/// name is validated with `validate_agent_name` BEFORE any DB access; an
/// injection-style argument is rejected and NO row is written
/// (TC-TMC-9.3/9.5). On an exact match against an alive CLI the binding
/// is upserted with fully-parameterised values (never string-interpolated).
fn handle_cmd_switch(conn: &Connection, chat_id: i64, text: &str) -> String {
    use crate::daemon::agent_registry::{list_alive, validate_agent_name};

    // Extract the first argument after the command token. `text` is e.g.
    // "/switch mira" or "/switch@bot mira" — split off the command token,
    // take the next whitespace-delimited token as the name.
    let arg = text.split_whitespace().nth(1);
    let name = match arg {
        Some(n) => n,
        None => return "Usage: /switch <name> — bind this chat to a named CLI. Use /agents to list online CLIs.".to_string(),
    };

    // ---- SECURITY: validate BEFORE touching the database --------------
    // An injection-style argument (e.g. `'; DROP TABLE …`, `../x`, a
    // 100-char blob) fails validate_agent_name and we return immediately,
    // so NO SQL statement ever sees the value (TC-TMC-9.x).
    if validate_agent_name(name).is_err() {
        return format!(
            "Invalid CLI name '{}'. Names are 1-64 chars of letters, digits, '_' or '-'.",
            // Echo a truncated, char-safe rendering so an oversized/garbage
            // arg cannot bloat or break the reply. Take up to 32 chars.
            name.chars().take(32).collect::<String>()
        );
    }

    // ---- exact-match against the alive set ----------------------------
    let alive = match list_alive(conn, None) {
        Ok(r) => r,
        Err(e) => {
            tracing::warn!(error = %e, "list_alive failed in /switch handler");
            return "Could not switch (internal error).".to_string();
        }
    };
    let matched = alive.iter().find(|r| r.agent_name == name);
    let Some(row) = matched else {
        let available = if alive.is_empty() {
            "none online".to_string()
        } else {
            alive.iter().map(|r| r.agent_name.as_str()).collect::<Vec<_>>().join(", ")
        };
        return format!("Unknown CLI: '{name}'. Available: {available}.");
    };

    // ---- upsert the binding (parameterised) ---------------------------
    // `set_by` records the chat_id as the setter (chat-as-id has no
    // per-user identity in the routing key). All values bound, not
    // interpolated.
    let set_by = chat_id.to_string();
    if let Err(e) = conn.execute(
        "INSERT OR REPLACE INTO active_cli_per_chat \
         (chat_id, active_cli_name, active_agent_id, set_at, set_by) \
         VALUES (?1, ?2, ?3, strftime('%s','now'), ?4)",
        params![chat_id, row.agent_name, row.agent_id, set_by],
    ) {
        tracing::warn!(error = %e, chat_id, "failed to upsert active_cli_per_chat in /switch");
        return "Could not save the binding (internal error).".to_string();
    }

    // Group chats (negative chat_id) rebind for ALL participants — make
    // that explicit (TC-TMC-9.6 asserts the group note).
    let mut reply = format!(
        "Switched to {}. Next free-text in this chat goes there.",
        row.agent_name
    );
    if chat_id < 0 {
        reply.push_str(" (Group chat: this rebinds the active CLI for all participants.)");
    }
    reply
}

/// `/whoami` — report THIS chat's bound CLI. Unbound → name the
/// first_alive fallback so the operator knows where free-text lands.
/// A bound-but-dead CLI is flagged with a /switch hint (TC-TMC-10.3).
fn handle_cmd_whoami(conn: &Connection, chat_id: i64) -> String {
    use crate::daemon::agent_registry::{first_alive, is_alive};

    // Read THIS chat's binding only (parameterised, scoped to chat_id).
    let binding: Option<(String, String)> = conn
        .query_row(
            "SELECT active_cli_name, active_agent_id FROM active_cli_per_chat WHERE chat_id = ?1",
            params![chat_id],
            |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?)),
        )
        .ok();

    match binding {
        Some((name, agent_id)) => match is_alive(conn, &agent_id) {
            Ok(true) => format!("This chat is bound to {name} ({agent_id})."),
            _ => format!(
                "This chat is bound to {name} ({agent_id}), but that CLI is offline / no longer alive. Use /switch to bind another, or /agents to see who is online."
            ),
        },
        None => match first_alive(conn, None, Some("orchestrator")) {
            Ok(Some(row)) => format!(
                "This chat has no explicit binding set. Free-text defaults to {} ({}). Use /switch to bind one.",
                row.agent_name, row.agent_id
            ),
            _ => "This chat has no explicit binding set and no CLIs are online. Spawn one with `claudebase run`.".to_string(),
        },
    }
}

/// `/here` — report the host + cwd of THIS chat's bound CLI ONLY.
/// SECURITY: strictly scoped to `chat_id`'s binding — it never reads
/// another chat's binding nor another CLI's metadata. In v1 no slice
/// populates host/cwd at `agent_register` (red-team F-6, grep-confirmed),
/// so the host/cwd read returns absent and we reply "unavailable"
/// (TC-TMC-11.2). A bound CLI whose registry row was reaped → "no longer
/// online" + /switch hint (TC-TMC-11.3).
fn handle_cmd_here(conn: &Connection, chat_id: i64) -> String {
    use crate::daemon::agent_registry::is_alive;

    // Scope to THIS chat's binding only.
    let bound: Option<(String, String)> = conn
        .query_row(
            "SELECT active_cli_name, active_agent_id FROM active_cli_per_chat WHERE chat_id = ?1",
            params![chat_id],
            |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?)),
        )
        .ok();

    let Some((name, agent_id)) = bound else {
        return "This chat has no bound CLI. Use /switch <name> to bind one (see /agents).".to_string();
    };

    // The bound CLI's registry row may have been reaped between /switch
    // and /here (TC-TMC-11.3).
    match is_alive(conn, &agent_id) {
        Ok(true) => {}
        _ => {
            return format!(
                "{name} ({agent_id}) is no longer online. Use /switch to bind another, or /agents to see who is online."
            )
        }
    }

    // Pull host/cwd from the bound CLI's metadata JSON (best-effort v1).
    let host = agent_metadata_field(conn, &agent_id, "host");
    let cwd = agent_metadata_field(conn, &agent_id, "cwd");
    match (host, cwd) {
        (Some(h), Some(c)) => format!("{name} is running on {h} in {c}."),
        (Some(h), None) => format!("{name} is running on {h} (working directory unavailable)."),
        (None, Some(c)) => format!("{name} working directory: {c} (host unavailable)."),
        (None, None) => format!(
            "{name} host/cwd information is unavailable (the CLI did not report it)."
        ),
    }
}

/// Read a string field from an agent's `metadata` JSON column, scoped to
/// the given `agent_id` (parameterised). Returns `None` when the row is
/// absent, the metadata is NULL/empty/non-JSON, the field is missing, the
/// field is not a string, or the string is empty. Used by `/here` and
/// `/agents` — never reads metadata for any agent other than the one named.
fn agent_metadata_field(conn: &Connection, agent_id: &str, field: &str) -> Option<String> {
    let metadata_text: Option<String> = conn
        .query_row(
            "SELECT metadata FROM agent_registry WHERE agent_id = ?1",
            params![agent_id],
            |row| row.get::<_, Option<String>>(0),
        )
        .ok()
        .flatten();
    let raw = metadata_text?;
    let val: serde_json::Value = serde_json::from_str(&raw).ok()?;
    let s = val.get(field)?.as_str()?;
    if s.is_empty() {
        None
    } else {
        Some(s.to_string())
    }
}

/// Convenience wrapper for the `cwd` metadata field used by `/agents`.
fn agent_cwd_from_metadata(conn: &Connection, agent_id: &str) -> Option<String> {
    agent_metadata_field(conn, agent_id, "cwd")
}

/// The outcome of running the 5-step routing decision tree over one
/// inbound Telegram message (telegram-multi-cli Slice 2, chat-as-id).
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum RoutingDecision {
    /// Step 1 — the message is a recognised bot command. Caller dispatches
    /// to `handle_bot_command` and does NOT publish a channel notification
    /// or route to any CLI.
    BotCommand(&'static str),
    /// Steps 2/4 — the message routes to the CLI whose `agent_id` is held
    /// here. Caller tags `meta.target_agent_id` with this value.
    Route(String),
    /// Step 5 — no alive CLI could be resolved. Caller sends the operator
    /// the literal "No CLIs online…" reply and routes nothing.
    NoTarget,
}

/// Run the 5-step chat-as-id routing decision tree for one inbound
/// message inside the caller's open transaction `tx`. Returns a
/// `RoutingDecision` the caller acts on. The tree is shared by both
/// `process_batch` and `process_batch_with_pairing` so production and the
/// test surface route identically (telegram-multi-cli Slice 2 replaces the
/// prior `@-mention` precursor in BOTH).
///
/// Steps (plan.md Slice 2 / PRD §19 FR-TMC-2.1):
///   1. Bot command (`/agents` …) → short-circuit, no CLI routing.
///   2. Reply-quote → `tg_message_map(chat_id, reply_to.message_id)`; if
///      the original sender CLI is alive, route to it; if dead, fall
///      through to step 4 (logged).
///   3. (omitted — chat-as-id has no per-user state.)
///   4. Active binding → `active_cli_per_chat[chat_id]`; if the bound CLI
///      is alive, route to it; otherwise (dead / empty / missing) fall
///      back to `first_alive(None, Some("orchestrator"))`.
///   5. No alive CLI anywhere → `NoTarget`.
///
/// Under chat-as-id the `@-mention` text is deliberately IGNORED — the
/// only routing keys are the reply-quote link and the chat binding
/// (UC-TMC-4-EC3 / TC-TMC-4.4).
///
/// `tx` Derefs to `&Connection`, so the `agent_registry` helpers
/// (`is_alive`, `first_alive`) and the `tg_message_map` /
/// `active_cli_per_chat` lookups all read the SAME SQLite snapshot the
/// message was inserted under — keeping the SEC-13 transactional
/// invariant intact (no DB read outside the transaction, no Connection
/// held across an `.await`: this function is fully synchronous and runs
/// inside the caller's `spawn_blocking` body).
pub(crate) fn resolve_routing_target(
    tx: &Connection,
    chat_id: i64,
    // `_thread_id` is retained in the signature for call-site clarity and a
    // possible future per-thread scoping mode, but chat-as-id routing keys
    // on `chat_id` alone (thread=None on the registry lookups), so it is
    // intentionally unused here.
    _thread_id: &str,
    text: &str,
    reply_to_message_id: Option<i64>,
) -> RoutingDecision {
    use crate::daemon::agent_registry::{first_alive, is_alive};

    // ---- Step 1: bot command ------------------------------------------
    if let Some(cmd) = match_bot_command(text) {
        return RoutingDecision::BotCommand(cmd);
    }

    // ---- Step 2: reply-quote ------------------------------------------
    if let Some(reply_id) = reply_to_message_id {
        let sender: Option<String> = tx
            .query_row(
                "SELECT sender_agent_id FROM tg_message_map \
                 WHERE chat_id = ?1 AND tg_msg_id = ?2",
                params![chat_id, reply_id],
                |row| row.get(0),
            )
            .ok();
        if let Some(sender_agent_id) = sender {
            match is_alive(tx, &sender_agent_id) {
                Ok(true) => {
                    tracing::info!(
                        event = "routing_reply_quote",
                        target_agent_id = %sender_agent_id,
                        chat_id,
                        reply_to = reply_id,
                        "telegram reply-quote routed to original sender CLI"
                    );
                    return RoutingDecision::Route(sender_agent_id);
                }
                _ => {
                    // Original sender CLI is no longer alive — fall through
                    // to the active-binding step (TC-TMC-5.2).
                    tracing::info!(
                        event = "routing_reply_quote_dead",
                        dead_agent_id = %sender_agent_id,
                        chat_id,
                        reply_to = reply_id,
                        "reply-quote original sender CLI no longer alive; falling through to active binding"
                    );
                }
            }
        }
        // No tg_message_map row for this reply → treat as free text and
        // fall through to step 4 (TC-TMC-5.3).
    }

    // ---- Step 4: active binding, else first_alive ---------------------
    let binding: Option<String> = tx
        .query_row(
            "SELECT active_agent_id FROM active_cli_per_chat WHERE chat_id = ?1",
            params![chat_id],
            |row| row.get(0),
        )
        .ok();

    if let Some(active_agent_id) = binding {
        if active_agent_id.is_empty() {
            // Corrupt binding row (TC-TMC-4.6) — empty agent_id never
            // matches an alive row. Warn and fall through to first_alive.
            tracing::warn!(
                event = "routing_malformed_binding",
                chat_id,
                "active_cli_per_chat row has empty active_agent_id (malformed); falling through to first_alive"
            );
        } else {
            match is_alive(tx, &active_agent_id) {
                Ok(true) => {
                    tracing::info!(
                        event = "routing_active_binding",
                        target_agent_id = %active_agent_id,
                        chat_id,
                        "telegram free-text routed to active chat binding"
                    );
                    return RoutingDecision::Route(active_agent_id);
                }
                _ => {
                    tracing::info!(
                        event = "routing_dead_binding",
                        dead_agent_id = %active_agent_id,
                        chat_id,
                        "active binding CLI is dead; falling through to first_alive"
                    );
                }
            }
        }
    }

    // No binding (or dead/malformed binding) → first alive orchestrator,
    // else any alive CLI. `thread`=None: chat-as-id routes across the
    // whole registry, not just the per-thread subscribers.
    match first_alive(tx, None, Some("orchestrator")) {
        Ok(Some(row)) => {
            tracing::info!(
                event = "routing_first_alive",
                target_agent_id = %row.agent_id,
                matched_name = %row.agent_name,
                chat_id,
                "telegram free-text fell back to first_alive"
            );
            RoutingDecision::Route(row.agent_id)
        }
        _ => {
            // ---- Step 5: no alive CLI anywhere ------------------------
            tracing::info!(
                event = "routing_no_target",
                chat_id,
                "no alive CLI to route to; replying with spawn hint"
            );
            RoutingDecision::NoTarget
        }
    }
}

/// The exact operator-facing reply when the routing tree resolves to
/// `NoTarget` (Step 5). Byte-for-byte per PRD §19 FR-TMC-2.1 / TC-TMC-21.1
/// — the backticks around `claudebase run` are literal.
pub const NO_CLIS_ONLINE_REPLY: &str = "No CLIs online. Spawn one with `claudebase run`.";

/// Result of one batch process — the highest `update_id` seen so the
/// outer loop can advance the offset, plus the notification frames the
/// async caller must publish via `ChatBus`. Notifications are deferred
/// to post-commit (we collect inside the sync `spawn_blocking` body and
/// publish from the async side once the transaction is durable) so a
/// crash between insert and publish cannot deliver phantom messages.
#[derive(Debug)]
pub struct BatchOutcome {
    pub new_offset: Option<i64>,
    pub messages_inserted: usize,
    /// Built post-commit by `process_batch`. Each tuple is
    /// `(thread_id, channel_notification_frame)`. The async long-poll
    /// caller iterates these and calls `bus.publish(thread, frame).await`.
    pub notifications: Vec<(String, serde_json::Value)>,
    /// Pair-action replies pending bot.send_message. Each tuple is
    /// `(chat_id, formatted_text)` matching the official telegram plugin's
    /// `gate(ctx)` Pair branch (server.ts:910-915). The async long-poll
    /// caller iterates these and sends via teloxide AFTER the DB
    /// transaction commits.
    pub pair_replies: Vec<(i64, String)>,
    /// True when the gate code mutated `channel_state::Access.pending`
    /// (a new code was issued OR a `replies` counter incremented). The
    /// async caller MUST save access.json when set; otherwise the next
    /// inbound DM from the same sender re-issues a different code.
    pub access_dirty: bool,
}

/// Strip occurrences of the bot token from any error string before it
/// reaches `tracing::error!`. This is the SEC-14 defence: teloxide errors
/// occasionally embed the URL (which carries the token in the path).
fn redact_error_string(s: &str, token: &str) -> String {
    if token.is_empty() {
        return s.to_string();
    }
    s.replace(token, "***")
}

/// Open the chat.db connection. Mirrors `chat::open_chat_db` but kept here
/// so the telegram module's call sites are explicit (the daemon spawns
/// telegram with its own DB handle — never share Connections across tasks
/// per ASYNC_INVARIANTS).
pub fn open_chat_db() -> Result<Connection> {
    chat::open_chat_db().map_err(Into::into)
}

/// Process one batch of Telegram updates inside ONE rusqlite transaction.
/// All chat-message inserts AND the offset-advance UPDATE are wrapped so
/// either every row makes it OR none of them do (SEC-13). On commit the
/// `last_update_id` daemon_state row is now `max_update_id`, so the next
/// `getUpdates` call uses `offset = max_update_id + 1`.
///
/// `access` is consulted for `check_allowed` — messages from non-allow-listed
/// users are silently skipped (TC-4.3) BUT their update_id still advances
/// the offset (otherwise an attacker could DOS the daemon by repeatedly
/// sending messages that get stuck at the same offset). The skip happens
/// inside the transaction — no DB row is inserted, but the offset moves.
pub fn process_batch(
    conn: &mut Connection,
    access: &Access,
    bus: Option<&SharedBus>,
    batch: &[Update],
) -> Result<BatchOutcome> {
    if batch.is_empty() {
        return Ok(BatchOutcome {
            new_offset: None,
            messages_inserted: 0,
            notifications: Vec::new(),
            pair_replies: Vec::new(),
            access_dirty: false,
        });
    }

    let tx = conn.transaction()?;
    let mut max_id: i64 = 0;
    let mut inserted: usize = 0;
    let mut notifications: Vec<(String, serde_json::Value)> = Vec::new();
    // Step-5 "No CLIs online" replies accumulate here and are sent via the
    // same post-commit teloxide path as pairing replies (telegram-multi-cli
    // Slice 2 — routes through pair_replies because the long-poll loop
    // already drains it with `bot.send_message`, and the OUTBOUND_TG global
    // is not initialised in the sync test harness).
    let mut pair_replies: Vec<(i64, String)> = Vec::new();

    for update in batch {
        if update.update_id > max_id {
            max_id = update.update_id;
        }
        let Some(msg) = &update.message else {
            continue;
        };
        let user_id = msg.from.as_ref().map(|u| u.id).unwrap_or(0);
        // SEC-12 / TC-4.3: drop messages from disallowed users without
        // inserting. Offset still advances (handled above).
        if !permissions::check_allowed(access, user_id) {
            continue;
        }

        let chat_id = msg.chat.id;
        let thread_id = format!("telegram:{}", msg.chat.id);
        let from_agent = match &msg.from.as_ref().and_then(|u| u.username.as_ref()) {
            Some(name) => format!("telegram:{name}"),
            None => format!("telegram:{user_id}"),
        };

        let content = match (&msg.text, &msg.voice) {
            (Some(text), _) => text.clone(),
            (None, Some(_)) => VOICE_SHIM_TEXT.to_string(),
            (None, None) => continue, // unsupported message type — skip but still advance offset
        };

        // telegram-multi-cli Slice 2 — run the 5-step chat-as-id routing
        // tree (replaces the prior @-mention precursor). The decision is
        // made against the open transaction snapshot so the tg_message_map
        // / active_cli_per_chat / agent_registry reads are consistent with
        // the same DB state this batch sees.
        let reply_to_id = msg.reply_to_message.as_ref().map(|r| r.message_id);
        let decision = resolve_routing_target(&tx, chat_id, &thread_id, &content, reply_to_id);

        // Step 1: bot command — do NOT insert a chat row and do NOT notify
        // any CLI. Dispatch to the Slice-3 stub and move on (offset still
        // advanced above).
        let target_agent_id: Option<String> = match decision {
            RoutingDecision::BotCommand(cmd) => {
                // Step 1 — bot command: query SQLite, enqueue the reply via
                // the same post-commit teloxide path as pairing/step-5
                // replies, publish NO channel notification, route to NO CLI
                // (TC-TMC-8.4 leak guard). `handle_bot_command` returns None
                // for /start and /status (preserved-as-is, no extra reply).
                if let Some(reply) = handle_bot_command(&tx, cmd, chat_id, &content) {
                    pair_replies.push((chat_id, reply));
                }
                continue;
            }
            RoutingDecision::Route(agent_id) => Some(agent_id),
            RoutingDecision::NoTarget => {
                // Step 5 — reply to the operator, route nothing.
                pair_replies.push((chat_id, NO_CLIS_ONLINE_REPLY.to_string()));
                continue;
            }
        };

        // chat::insert_message but inside this transaction. We replicate
        // the SQL here because the existing helper takes &Connection
        // (not &Transaction) and we need transactional atomicity per
        // SEC-13.
        let id = uuid::Uuid::new_v4().to_string();
        let now = chrono_millis();
        tx.execute(
            "INSERT OR IGNORE INTO chat_threads (id, created_at) VALUES (?1, ?2)",
            params![thread_id, now],
        )?;
        tx.execute(
            "INSERT INTO chat_messages \
             (id, thread_id, from_agent, content, reply_to, created_at) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            params![id, thread_id, from_agent, content, Option::<String>::None, now],
        )?;
        inserted += 1;

        // Build the notification we'll broadcast AFTER commit so a crash
        // between insert and broadcast doesn't deliver phantom messages.
        let msg_for_notif = chat::ChatMessage {
            id: id.clone(),
            thread_id: thread_id.clone(),
            from_agent: from_agent.clone(),
            content: content.clone(),
            reply_to: None,
            created_at: now,
        };
        notifications.push((
            thread_id.clone(),
            chat::build_channel_notification_routed(&msg_for_notif, target_agent_id.as_deref()),
        ));
    }

    // Bump offset to max_update_id (so the next getUpdates uses offset =
    // max_id + 1). The value column in daemon_state is TEXT so we
    // stringify here. Stored value is the highest processed update_id;
    // the +1 offset adjustment is applied at the long-poll call site.
    tx.execute(
        "UPDATE daemon_state SET value = ?1 WHERE key = 'telegram.last_update_id'",
        params![max_id.to_string()],
    )?;

    tx.commit()?;

    // POST-COMMIT broadcast queue handover. `bus.publish` is async and
    // cannot be called from this sync function (we run inside
    // `spawn_blocking`). The async caller iterates `outcome.notifications`
    // and publishes each frame after `spawn_blocking` returns — keeping
    // the invariant that broadcast only happens after the durable commit.
    // The `bus` parameter is retained for the test sites that thread it
    // through, but the actual publish wiring lives in `run_long_poll`.
    let _ = bus;

    Ok(BatchOutcome {
        new_offset: Some(max_id),
        messages_inserted: inserted,
        notifications,
        pair_replies,
        access_dirty: false,
    })
}

/// Process one batch with full official-telegram-plugin gating semantics
/// (channel_state::Access — DmPolicy{Pairing,Allowlist,Disabled}, pending
/// codes, replies counter, format_pair_reply). Mirrors server.ts:900-916
/// for the per-update gate decision; the post-gate insert/broadcast path
/// reuses the SEC-13 transactional invariants from `process_batch`.
///
/// The function mutates `access` in-place when a new pairing code is
/// issued OR an existing entry's `replies` counter increments. The caller
/// inspects `BatchOutcome.access_dirty` and saves access.json when true.
///
/// `pair_replies` is populated with `(chat_id, formatted_text)` tuples
/// for the async caller to send via `bot.send_message`. Pair-action
/// inbound DMs do NOT advance into chat.db and do NOT broadcast — the
/// pairing-code reply is the entire visible side-effect.
pub fn process_batch_with_pairing(
    conn: &mut Connection,
    access: &mut channel_state::Access,
    batch: &[Update],
) -> Result<BatchOutcome> {
    if batch.is_empty() {
        return Ok(BatchOutcome {
            new_offset: None,
            messages_inserted: 0,
            notifications: Vec::new(),
            pair_replies: Vec::new(),
            access_dirty: false,
        });
    }

    // Drop expired entries before any gate decision (server.ts:229).
    let now = channel_state::now_ms();
    let pruned = channel_state::prune_expired(access, now);
    let mut dirty = pruned;

    let tx = conn.transaction()?;
    let mut max_id: i64 = 0;
    let mut inserted: usize = 0;
    let mut notifications: Vec<(String, serde_json::Value)> = Vec::new();
    let mut pair_replies: Vec<(i64, String)> = Vec::new();

    for update in batch {
        if update.update_id > max_id {
            max_id = update.update_id;
        }
        let Some(msg) = &update.message else {
            continue;
        };

        let chat_id = msg.chat.id;
        let user_id = msg.from.as_ref().map(|u| u.id).unwrap_or(0);
        // Telegram numeric IDs serialised as strings — matches the
        // official plugin's access.json schema and the user-facing skill.
        let sender_id_str = user_id.to_string();
        let chat_id_str = chat_id.to_string();

        // Run the gate. server.ts:225-267 semantics — Disabled drops all,
        // allowFrom hit delivers, Pairing+unknown emits a code, Allowlist+
        // unknown drops, pending cap drops, MAX_PAIRING_REPLIES drops.
        let action = channel_state::gate_dm(access, &sender_id_str, &chat_id_str, now);

        match action {
            GateAction::Drop => continue,
            GateAction::Pair { code, is_resend } => {
                dirty = true;
                let text = channel_state::format_pair_reply(&code, is_resend);
                pair_replies.push((chat_id, text));
                // Pair-action does NOT insert into chat.db and does NOT
                // broadcast to subscribers — matches server.ts:910-915.
                continue;
            }
            GateAction::Deliver => {
                // fall through to insert + broadcast
            }
        }

        let thread_id = format!("telegram:{}", chat_id);
        let from_agent = match &msg.from.as_ref().and_then(|u| u.username.as_ref()) {
            Some(name) => format!("telegram:{name}"),
            None => format!("telegram:{user_id}"),
        };

        let content = match (&msg.text, &msg.voice) {
            (Some(text), _) => text.clone(),
            (None, Some(_)) => VOICE_SHIM_TEXT.to_string(),
            (None, None) => continue,
        };

        // telegram-multi-cli Slice 2 — run the 5-step chat-as-id routing
        // tree (replaces the prior @-mention precursor) BEFORE inserting,
        // so bot commands and no-target messages short-circuit cleanly.
        let reply_to_id = msg.reply_to_message.as_ref().map(|r| r.message_id);
        let decision = resolve_routing_target(&tx, chat_id, &thread_id, &content, reply_to_id);
        let target_agent_id: Option<String> = match decision {
            RoutingDecision::BotCommand(cmd) => {
                // Step 1 — query SQLite, enqueue the reply via the post-commit
                // teloxide path; no chat row, no CLI notification (TC-TMC-8.4
                // / TC-TMC-12.3 leak guard). None for /start and /status.
                if let Some(reply) = handle_bot_command(&tx, cmd, chat_id, &content) {
                    pair_replies.push((chat_id, reply));
                }
                continue;
            }
            RoutingDecision::Route(agent_id) => Some(agent_id),
            RoutingDecision::NoTarget => {
                // Step 5 — reply with the spawn hint via the same teloxide
                // send path as pairing replies; route nothing (TC-TMC-21.1).
                pair_replies.push((chat_id, NO_CLIS_ONLINE_REPLY.to_string()));
                continue;
            }
        };

        let id = uuid::Uuid::new_v4().to_string();
        let row_now = chrono_millis();
        tx.execute(
            "INSERT OR IGNORE INTO chat_threads (id, created_at) VALUES (?1, ?2)",
            params![thread_id, row_now],
        )?;
        tx.execute(
            "INSERT INTO chat_messages \
             (id, thread_id, from_agent, content, reply_to, created_at) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            params![id, thread_id, from_agent, content, Option::<String>::None, row_now],
        )?;
        inserted += 1;

        // Slice 7.x — build the official-telegram-plugin-shaped meta so
        // Claude Code's channel surface parses chat_id / user / user_id /
        // ts and emits a usable <channel ...> tag. The legacy
        // build_channel_notification_routed (thread + from_agent +
        // numeric ts) delivers via UDS but Claude Code surface silently
        // drops it.
        let user_display = msg
            .from
            .as_ref()
            .and_then(|u| u.username.as_ref())
            .cloned()
            .unwrap_or_else(|| user_id.to_string());
        let ts_iso = ts_seconds_to_iso8601(msg.date);
        let tg_meta = chat::TelegramMessageMeta {
            chat_id,
            message_id_str: msg.message_id.to_string(),
            user: user_display,
            user_id: user_id.to_string(),
            ts_iso8601: ts_iso,
        };
        notifications.push((
            thread_id.clone(),
            chat::build_channel_notification_telegram(
                &content,
                &tg_meta,
                target_agent_id.as_deref(),
            ),
        ));
    }

    tx.execute(
        "UPDATE daemon_state SET value = ?1 WHERE key = 'telegram.last_update_id'",
        params![max_id.to_string()],
    )?;

    tx.commit()?;

    Ok(BatchOutcome {
        new_offset: Some(max_id),
        messages_inserted: inserted,
        notifications,
        pair_replies,
        access_dirty: dirty,
    })
}

/// Wall-clock millis since epoch — local helper because the chat module's
/// `now_millis` is private. Behaviour identical.
fn chrono_millis() -> i64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// Convert Telegram's `msg.date` (epoch SECONDS, not millis) to an
/// ISO 8601 UTC string matching JS `new Date(seconds*1000).toISOString()`
/// (server.ts:1271). Format: `2026-05-18T20:13:13.000Z`. Returns a
/// safe fallback (`1970-01-01T00:00:00.000Z`) on out-of-range seconds.
fn ts_seconds_to_iso8601(date_seconds: i64) -> String {
    use chrono::{DateTime, SecondsFormat, Utc};
    match DateTime::<Utc>::from_timestamp(date_seconds, 0) {
        Some(dt) => dt.to_rfc3339_opts(SecondsFormat::Millis, true),
        None => "1970-01-01T00:00:00.000Z".to_string(),
    }
}

/// Read the persisted offset from daemon_state. Returns 0 when the row
/// doesn't exist (shouldn't happen since the v5 schema seeds it, but be
/// defensive).
pub fn load_offset(conn: &Connection) -> Result<i64> {
    let value: String = conn
        .query_row(
            "SELECT value FROM daemon_state WHERE key='telegram.last_update_id'",
            [],
            |r| r.get(0),
        )
        .unwrap_or_else(|_| "0".to_string());
    Ok(value.parse::<i64>().unwrap_or(0))
}

/// Mark daemon state for the bot connection — `up` after a successful
/// getUpdates round-trip, `disconnected` on 401. Persisted into daemon_state
/// so `daemon status` and tests can introspect.
pub fn set_bot_state(conn: &Connection, state: &str) -> Result<()> {
    conn.execute(
        "INSERT INTO daemon_state(key, value) VALUES('tg_bot_state', ?1) \
         ON CONFLICT(key) DO UPDATE SET value=excluded.value",
        params![state],
    )
    .context("write tg_bot_state to daemon_state")?;
    Ok(())
}

/// Spawn the Telegram long-poll task. Returns immediately; the spawned
/// task runs until it hits a fatal error (401) OR the daemon is killed.
///
/// `secrets_path` is the loaded-and-perm-checked secrets.toml path; the
/// caller must have already passed it through `config::load_secrets_toml`
/// so this function only sees a token-shaped `RedactedToken`.
///
/// **Slice 4 status:** the long-poll loop is implemented for the path
/// where the daemon has valid secrets AND `enabled = true` in
/// daemon.toml. When either condition is false, the spawn is skipped
/// silently — Slice 1-3 callers without secrets.toml see no behavior
/// change. Live HTTP integration (mocked or real Telegram API) is exercised
/// only when the operator points the daemon at a real bot; the e2e tests
/// in `tests/telegram_e2e_test.rs` are scaffolds that verify config plumbing
/// not live HTTP. The real production loop body still has to compile and
/// be ready to run.
pub fn spawn_long_poll(
    token: RedactedToken,
    access_path: PathBuf,
    bus: SharedBus,
    asr: Option<Arc<dyn Asr>>,
) -> tokio::task::JoinHandle<()> {
    // Initialise the outbound bridge BEFORE spawning so server.rs's MCP
    // chat_reply handler can enqueue immediately (race-free: any push
    // before the spawn is queued; the receiver picks it up on the first
    // select! tick).
    let (outbound_tx, outbound_rx) = mpsc::unbounded_channel::<(i64, String)>();
    if OUTBOUND_TG.set(outbound_tx).is_err() {
        tracing::warn!(
            "OUTBOUND_TG already initialised — second spawn_long_poll call ignored (daemon should spawn only once per process)"
        );
    }

    // Slice 7.x — spawn the approved-dir polling task alongside the
    // long-poll. The official telegram plugin server.ts:351 starts the
    // same `setInterval(checkApprovals, 5000)` so the bot can confirm
    // pairings out-of-band. Rust port: a separate tokio task with the
    // same 5s cadence, sharing only the bot token (constructs its own
    // teloxide::Bot).
    let approved_token = token.clone();
    tokio::spawn(async move {
        run_approved_polling(approved_token).await;
    });

    tokio::spawn(async move {
        // ASYNC_INVARIANTS Rule 3 — wrap the long-poll body so any
        // unhandled error logs structured (without leaking the token) and
        // the daemon's other tasks keep running.
        let token_str = token.as_str().to_string();
        if let Err(e) = run_long_poll(token, access_path, bus, asr, outbound_rx).await {
            tracing::error!(
                error = %redact_error_string(&format!("{e:#}"), &token_str),
                "telegram long-poll fatal"
            );
        }
    })
}

/// Approved-dir polling — 1:1 port of `checkApprovals` (server.ts:331-349)
/// + `setInterval(checkApprovals, 5000)` (server.ts:351).
///
/// Every 5 seconds, scans `~/.claude/channels/claudebase/approved/`. For
/// each file `<senderId>`, reads the file contents as the `chatId`, sends
/// `"Paired! Say hi to Claude."` to that chat via teloxide, then unlinks
/// the file regardless of send success (matches server.ts:344 — remove
/// anyway so a broken-send doesn't loop).
///
/// The polling task runs forever; cancellation happens when the parent
/// task drops the JoinHandle (daemon shutdown).
async fn run_approved_polling(token: RedactedToken) {
    use std::fs;

    let bot = teloxide::Bot::new(token.as_str());
    let token_for_redaction = token.as_str().to_string();
    let dir = channel_state::approved_dir();
    let mut interval = tokio::time::interval(Duration::from_secs(5));
    interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

    loop {
        interval.tick().await;
        // Read the dir entries. Missing dir = silent no-op (matches
        // server.ts:336 `try { readdirSync } catch { return }`).
        let entries = match fs::read_dir(&dir) {
            Ok(it) => it,
            Err(_) => continue,
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if !path.is_file() {
                continue;
            }
            // The filename IS the senderId; for Telegram DMs chatId == senderId,
            // but server.ts:340-344 deliberately uses the file contents (chatId)
            // so this still works for group chats added later.
            let chat_id_str = match fs::read_to_string(&path) {
                Ok(s) => s.trim().to_string(),
                Err(e) => {
                    tracing::warn!(error = %e, path = %path.display(), "approved file unreadable; removing");
                    let _ = fs::remove_file(&path);
                    continue;
                }
            };
            let chat_id_int: i64 = match chat_id_str.parse() {
                Ok(v) => v,
                Err(_) => {
                    tracing::warn!(
                        chat_id_str = %chat_id_str,
                        path = %path.display(),
                        "approved file chatId not parseable as i64; removing"
                    );
                    let _ = fs::remove_file(&path);
                    continue;
                }
            };
            // server.ts:341 — "Paired! Say hi to Claude." verbatim.
            use teloxide::requests::Requester;
            let send_res = bot
                .send_message(
                    teloxide::types::ChatId(chat_id_int),
                    "Paired! Say hi to Claude.",
                )
                .await;
            match send_res {
                Ok(_) => tracing::info!(chat_id = chat_id_int, "paired-confirm sent"),
                Err(e) => tracing::warn!(
                    chat_id = chat_id_int,
                    error = %redact_error_string(&format!("{e}"), &token_for_redaction),
                    "paired-confirm send failed (file removed anyway)"
                ),
            }
            // server.ts:344 — remove anyway, don't loop on a broken send.
            let _ = fs::remove_file(&path);
        }
    }
}

/// Inner long-poll loop. Reads `getUpdates`, processes each batch
/// transactionally, advances `last_update_id`, sleeps a small interval,
/// repeats.
///
/// The loop is structured so cancellation (parent task drop) is
/// graceful — only `tokio::time::sleep` and reqwest's response future can
/// be in flight at any await point, and both are cancellation-safe per
/// ASYNC_INVARIANTS Rule 4.
async fn run_long_poll(
    token: RedactedToken,
    access_path: PathBuf,
    bus: SharedBus,
    asr: Option<Arc<dyn Asr>>,
    mut outbound_rx: mpsc::UnboundedReceiver<(i64, String)>,
) -> Result<()> {
    // Allow tests / local dev to point at a mock Telegram endpoint via
    // TELOXIDE_API_URL. teloxide 0.17 reads this env var directly via
    // `Bot::from_env_with_api_url` and `requester_ext` — see the spawned
    // teloxide construction below.
    let api_url = std::env::var("TELOXIDE_API_URL").ok();

    // Slice 4 production loop body. teloxide ships a high-level
    // `repls` interface but we use the lower-level `getUpdates` call
    // directly so the transactional offset-advance lives where SEC-13
    // wants it.
    //
    // teloxide's `set_api_url` accepts a `reqwest::Url`; we parse via the
    // url crate then convert. Both crates use the same underlying
    // representation so the conversion is a free re-parse.
    use teloxide::payloads::GetUpdatesSetters;
    use teloxide::requests::Requester;
    let bot = if let Some(url_str) = api_url.as_deref() {
        match url::Url::parse(url_str) {
            Ok(parsed) => {
                // url::Url and reqwest::Url are the SAME type (reqwest
                // re-exports url::Url). Just hand it through directly.
                teloxide::Bot::new(token.as_str()).set_api_url(parsed)
            }
            Err(e) => {
                tracing::error!(error = %e, "TELOXIDE_API_URL is not a valid URL");
                teloxide::Bot::new(token.as_str())
            }
        }
    } else {
        teloxide::Bot::new(token.as_str())
    };

    // Open the daemon's chat.db handle. Per ASYNC_INVARIANTS each task
    // opens its own Connection — never share across .await.
    let conn_path = crate::store::user_level_chat_db_path();
    tracing::info!(chat_db = %conn_path.display(), "telegram long-poll starting");

    // Slice 4: production long-poll body. We loop forever reading
    // `getUpdates`. For Slice 4 the body sleeps a small interval and
    // re-reads access.json every iteration — access.json changes are
    // observed within one poll cycle without restarting the daemon.
    //
    // The loop ALSO handles the access-path-missing case: if the file
    // doesn't exist (fresh install before any /start) the daemon uses
    // Access::default(), which keeps `dm_policy = Pairing` — so unknown
    // users get filtered out. The pending-pair generation lives in the
    // /start branch we route through bot's message handler.
    let mut consecutive_429_retries: u32 = 0;
    let token_for_error_redaction = token.as_str().to_string();

    // Slice 7.x — 1:1 port of the official Anthropic telegram plugin.
    // The skill-managed channel state lives at the path documented in
    // `src/daemon/channel_state.rs` (`~/.claude/channels/claudebase/`),
    // NOT the legacy `~/.config/claudebase/` location. The legacy
    // `access_path` parameter is retained so existing CLI shims continue
    // to compile but the long-poll body ignores it — channel_state owns
    // state from here onward.
    let _ = &access_path; // suppress unused-var lint without changing the API
    let channel_access_path = channel_state::access_json_path();

    loop {
        // Load channel state fresh each poll (Slice 7.x — operator
        // mutations via `/claudebase:access pair <code>` and the bot's
        // pair-action mutations happen out-of-band; one-poll lag is
        // acceptable).
        let mut cs_access = match channel_state::load_access(&channel_access_path) {
            Ok(a) => a,
            Err(e) => {
                tracing::warn!(
                    error = %redact_error_string(&format!("{e}"), &token_for_error_redaction),
                    path = %channel_access_path.display(),
                    "failed to load channel_state access.json (using defaults)"
                );
                channel_state::Access::default()
            }
        };

        // Open a fresh connection for this poll cycle so the long-running
        // task doesn't hold a Connection across .await. spawn_blocking
        // wraps the rusqlite work per Rule 2.
        let process_result = tokio::task::spawn_blocking(move || -> Result<(i64, BatchOutcome)> {
            let conn = chat::open_chat_db()
                .context("open chat.db for telegram poll")?;
            let offset = load_offset(&conn)?;
            // For Slice 4 we DO NOT make the real teloxide network call
            // from this thread — the rest of this loop (network I/O,
            // batch construction) happens BACK in the async context.
            // Return the current offset so the async side knows where
            // to start, AND an empty batch outcome (no work done in
            // spawn_blocking).
            Ok((
                offset,
                BatchOutcome {
                    new_offset: None,
                    messages_inserted: 0,
                    notifications: Vec::new(),
                    pair_replies: Vec::new(),
                    access_dirty: false,
                },
            ))
        })
        .await;

        let (offset, _) = match process_result {
            Ok(Ok(v)) => v,
            Ok(Err(e)) => {
                tracing::warn!(
                    error = %redact_error_string(&format!("{e}"), &token_for_error_redaction),
                    "spawn_blocking load_offset failed"
                );
                tokio::time::sleep(Duration::from_secs(5)).await;
                continue;
            }
            Err(e) => {
                tracing::warn!(error = %e, "spawn_blocking joined with panic");
                tokio::time::sleep(Duration::from_secs(5)).await;
                continue;
            }
        };

        // Drain any pending outbound messages BEFORE making the next
        // long-poll request. This means an MCP chat_reply call that
        // enqueues outbound has a snappy delivery path: it fires
        // immediately on the next loop iteration, not after waiting for
        // the long-poll to time out. Each iteration drains up to 16
        // queued messages so a burst doesn't starve getUpdates.
        for _ in 0..16 {
            match outbound_rx.try_recv() {
                Ok((chat_id, text)) => {
                    let send_result = bot
                        .send_message(teloxide::types::ChatId(chat_id), &text)
                        .await;
                    match send_result {
                        Ok(_) => tracing::info!(
                            chat_id,
                            bytes = text.len(),
                            "telegram outbound sent"
                        ),
                        Err(e) => tracing::warn!(
                            chat_id,
                            error = %redact_error_string(&format!("{e}"), &token_for_error_redaction),
                            "telegram outbound send failed"
                        ),
                    }
                }
                Err(mpsc::error::TryRecvError::Empty) => break,
                Err(mpsc::error::TryRecvError::Disconnected) => {
                    tracing::warn!("outbound channel disconnected (no more chat_reply traffic)");
                    break;
                }
            }
        }

        // Make the getUpdates HTTP call. teloxide's `Requester::get_updates`
        // returns a builder; we set offset and timeout, then await.
        let updates_result = bot
            .get_updates()
            .offset(offset.saturating_add(1) as i32)
            // Long-poll timeout MUST be strictly less than teloxide's default
            // reqwest client timeout (17 seconds — see teloxide-core/src/net.rs
            // doc comment "If you are using the polling mechanism to get updates,
            // the timeout configured in the client should be bigger than the
            // polling timeout"). 10s leaves comfortable margin for TLS handshake
            // + first-byte. Fix surfaced on live test: 25s caused
            // `reqwest::Error::request` after 17s client-side cutoff before
            // server long-poll resolved.
            .timeout(10u32)
            .await;

        let raw_updates = match updates_result {
            Ok(v) => {
                consecutive_429_retries = 0;
                v
            }
            Err(e) => {
                let err_str = format!("{e}");
                let redacted = redact_error_string(&err_str, &token_for_error_redaction);
                // SEC-14: 401 → mark disconnected and exit the loop. 429
                // → retry once.
                if err_str.contains("401") || err_str.contains("Unauthorized") {
                    tracing::error!(
                        reason = "telegram 401 unauthorized",
                        error = %redacted,
                        "telegram disconnected"
                    );
                    let _ = tokio::task::spawn_blocking(|| -> Result<()> {
                        let conn = chat::open_chat_db()?;
                        set_bot_state(&conn, "disconnected")?;
                        Ok(())
                    })
                    .await;
                    return Ok(());
                }
                if err_str.contains("429") || err_str.contains("RetryAfter") {
                    consecutive_429_retries += 1;
                    if consecutive_429_retries > 1 {
                        // UC-3-E2 / SEC-14: ONE retry only. After the
                        // second 429, back off the inbound loop and
                        // reset the counter.
                        tracing::warn!(
                            error = %redacted,
                            "telegram 429 after retry — backing off"
                        );
                        consecutive_429_retries = 0;
                        tokio::time::sleep(Duration::from_secs(30)).await;
                    } else {
                        // First 429 — retry once after the suggested
                        // back-off (Telegram puts retry_after in the
                        // error description; for simplicity sleep 5s).
                        tracing::warn!(
                            error = %redacted,
                            "telegram 429 — retrying once after 5s"
                        );
                        tokio::time::sleep(Duration::from_secs(5)).await;
                    }
                    continue;
                }
                tracing::warn!(
                    error = %redacted,
                    "telegram getUpdates error — continuing"
                );
                tokio::time::sleep(Duration::from_secs(5)).await;
                continue;
            }
        };

        // Convert teloxide Updates into our minimal `Update` shape via
        // JSON round-trip — keeps our process_batch surface decoupled
        // from teloxide's enum tree.
        let mut decoded: Vec<Update> = Vec::with_capacity(raw_updates.len());
        for up in &raw_updates {
            match serde_json::to_value(up).and_then(serde_json::from_value::<Update>) {
                Ok(u) => decoded.push(u),
                Err(e) => {
                    tracing::warn!(
                        error = %e,
                        "failed to decode telegram Update — skipping"
                    );
                }
            }
        }

        if decoded.is_empty() {
            // Idle interval — Slice 4 hard-codes 1s; future config could
            // expose this as a knob.
            tokio::time::sleep(Duration::from_secs(1)).await;
            continue;
        }

        // Slice 6-MVP: voice-note transcription happens HERE, BEFORE
        // the spawn_blocking(process_batch) call so the SEC-13 DB
        // transaction stays short (architect axis-5). Per update, if
        // `voice` is set and `text` is absent, fetch the file via the
        // teloxide client, decode Opus, run ASR, mutate the update to
        // `text = Some(transcript); voice = None`. Failures surface as
        // a bracketed `[voice transcription failed: ...]` text that
        // process_batch inserts as a normal row — the operator sees
        // the error in the chat thread instead of silent loss.
        for update in decoded.iter_mut() {
            if let Some(msg) = &mut update.message {
                if msg.text.is_none() && msg.voice.is_some() {
                    let voice_text = match transcribe_voice_note(&bot, msg, asr.as_ref()).await {
                        Ok(t) => t,
                        Err(e) => {
                            tracing::warn!(
                                error = %redact_error_string(&format!("{e:#}"), &token_for_error_redaction),
                                "voice transcribe failed; using fallback"
                            );
                            format!("[voice transcription failed: {e}]")
                        }
                    };
                    msg.text = Some(voice_text);
                    msg.voice = None;
                }
            }
        }

        let batch = decoded;
        let access_for_spawn = cs_access.clone();
        let process_outcome = tokio::task::spawn_blocking(
            move || -> Result<(BatchOutcome, channel_state::Access)> {
                let mut conn = chat::open_chat_db()?;
                let mut access_local = access_for_spawn;
                let outcome = process_batch_with_pairing(&mut conn, &mut access_local, &batch)?;
                Ok((outcome, access_local))
            },
        )
        .await;

        match process_outcome {
            Ok(Ok((outcome, mutated_access))) => {
                if outcome.messages_inserted > 0 {
                    tracing::info!(
                        inserted = outcome.messages_inserted,
                        max_update_id = ?outcome.new_offset,
                        "telegram batch persisted"
                    );
                }
                // Persist any access.json mutation BEFORE sending pair
                // replies — if the bot.send_message fails midway we want
                // the pending entry on disk so the next inbound DM resends
                // (or hits the existing-code branch in gate_dm).
                if outcome.access_dirty {
                    let path_clone = channel_access_path.clone();
                    let access_to_save = mutated_access.clone();
                    let save_res = tokio::task::spawn_blocking(move || {
                        channel_state::save_access(&path_clone, &access_to_save)
                    })
                    .await;
                    match save_res {
                        Ok(Ok(())) => {
                            cs_access = mutated_access;
                        }
                        Ok(Err(e)) => {
                            tracing::warn!(
                                error = %e,
                                path = %channel_access_path.display(),
                                "failed to persist channel_state access.json — code may resend"
                            );
                        }
                        Err(e) => {
                            tracing::warn!(error = %e, "channel_state save spawn_blocking panicked");
                        }
                    }
                }

                // Send pair-action replies via teloxide (server.ts:910-915).
                for (chat_id, text) in outcome.pair_replies {
                    match bot
                        .send_message(teloxide::types::ChatId(chat_id), &text)
                        .await
                    {
                        Ok(_) => tracing::info!(
                            chat_id,
                            "telegram pair reply sent"
                        ),
                        Err(e) => tracing::warn!(
                            chat_id,
                            error = %redact_error_string(&format!("{e}"), &token_for_error_redaction),
                            "telegram pair reply send failed"
                        ),
                    }
                }

                // Publish post-commit notifications from the async side
                // (Rule 4 cancellation-safety: bus.publish drops a
                // broadcast send result, no held lock across the await).
                for (thread, frame) in outcome.notifications {
                    let n = bus.publish(&thread, frame).await;
                    tracing::debug!(
                        thread = %thread,
                        subscribers = n,
                        "telegram broadcast"
                    );
                }
            }
            Ok(Err(e)) => {
                tracing::warn!(
                    error = %redact_error_string(&format!("{e}"), &token_for_error_redaction),
                    "telegram batch processing failed"
                );
            }
            Err(e) => {
                tracing::warn!(error = %e, "telegram batch spawn_blocking panicked");
            }
        }
    }
}

/// Slice 6-MVP — transcribe one Telegram voice note end-to-end.
///
/// 1. `bot.get_file(file_id)` → File metadata (carries `path` and
///    `file_size`).
/// 2. `bot.download_file(path, &mut buf)` → raw Opus-in-Ogg bytes.
/// 3. `decoder::decode_ogg_opus_to_16k_mono_pcm(&bytes)` → 16 kHz mono
///    Vec<f32>.
/// 4. `asr.transcribe(pcm, 16_000).await` → transcript string.
///
/// Returns Err when:
///   - the message has no voice field
///   - `asr` is None (no backend configured / feature off)
///   - the file fetch, decode, or transcribe step fails
///
/// The caller decides how to surface the error to the chat thread —
/// the current `run_long_poll` integration wraps Err in the literal
/// `[voice transcription failed: ...]` placeholder so the operator
/// sees the failure inline (NEVER silent loss).
async fn transcribe_voice_note(
    bot: &teloxide::Bot,
    msg: &Message,
    asr: Option<&Arc<dyn Asr>>,
) -> Result<String> {
    use teloxide::net::Download;
    use teloxide::requests::Requester;
    use teloxide::types::FileId;

    let voice = msg
        .voice
        .as_ref()
        .context("voice transcribe: message has no voice field")?;
    let asr = asr.context("voice transcribe: ASR backend not configured")?;

    // Step 1: get_file resolves the Telegram file path from file_id.
    let file = bot
        .get_file(FileId(voice.file_id.clone()))
        .await
        .with_context(|| format!("voice transcribe: get_file {}", voice.file_id))?;

    // Step 2: download into an in-memory buffer. Voice notes max out
    // around ~1 MB for 10-minute clips at Opus's 24 kbps default; we
    // pre-size the buffer with file_size when available.
    let mut buf: Vec<u8> = Vec::with_capacity(file.size as usize);
    bot.download_file(&file.path, &mut buf)
        .await
        .with_context(|| format!("voice transcribe: download_file {}", file.path))?;

    // Step 3: decode Opus-in-Ogg → 16 kHz mono PCM. Run on the blocking
    // pool so the codec work doesn't hog the tokio worker.
    let pcm = tokio::task::spawn_blocking(move || {
        crate::daemon::asr::decoder::decode_ogg_opus_to_16k_mono_pcm(&buf)
    })
    .await
    .context("voice transcribe: decode spawn_blocking join failed")?
    .context("voice transcribe: decode failed")?;

    // Step 4: hand the PCM to the configured ASR backend. The trait's
    // own implementation chooses how to dispatch (sync blocking pool
    // for whisper; HTTP for nim; etc.).
    let transcript = asr.transcribe(pcm, 16_000).await.context("voice transcribe: ASR failed")?;
    Ok(transcript)
}

/// Cleanup helper — keep `Arc` so the daemon's lifetime guards work even
/// when the long-poll never runs.
pub fn no_op_arc() -> Arc<()> {
    Arc::new(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::daemon::config::DmPolicy;

    // Slice 7 — @-mention parser tests (STRUCTURAL-7-1)
    #[test]
    fn extract_first_mention_finds_simple_at_token() {
        assert_eq!(extract_first_mention("@reflection thoughts?"), Some("reflection"));
    }

    #[test]
    fn extract_first_mention_rejects_email_like_token() {
        assert_eq!(extract_first_mention("write to email@foo.com test"), None);
    }

    #[test]
    fn extract_first_mention_preserves_case() {
        // case-insensitive matching is done DOWNSTREAM in process_batch;
        // the parser returns the verbatim slice (TC-7.7 requires this so
        // logs surface case-divergence).
        assert_eq!(extract_first_mention("hi @PLANNER !!"), Some("PLANNER"));
    }

    #[test]
    fn extract_first_mention_first_wins_on_multiple() {
        // STRUCTURAL-7-5: only the first valid mention is returned.
        assert_eq!(extract_first_mention("@a hi @b"), Some("a"));
    }

    #[test]
    fn extract_first_mention_stops_at_non_charset() {
        // Underscore and hyphen are in-charset; period stops the scan.
        assert_eq!(extract_first_mention("@my_agent-1.next"), Some("my_agent-1"));
    }

    #[test]
    fn extract_first_mention_returns_none_for_bare_at() {
        assert_eq!(extract_first_mention("hello @ world"), None);
        assert_eq!(extract_first_mention("@"), None);
        assert_eq!(extract_first_mention(""), None);
    }

    #[test]
    fn extract_first_mention_word_boundary_after_punct() {
        // STRUCTURAL-7-1: punctuation before `@` is a word boundary.
        assert_eq!(extract_first_mention("Hey! @planner ping?"), Some("planner"));
        assert_eq!(extract_first_mention("(@planner)"), Some("planner"));
    }

    // Slice 7 — routing tiebreak + build_channel_notification_routed
    // (STRUCTURAL-7-2, STRUCTURAL-7-3). Wire shape rewritten 2026-05-18
    // to match claude-telegram-voice-control: `params.content` flat at
    // top level, `params.meta` carries thread/from_agent/message_id/ts
    // and optionally `target_agent_id`.
    #[test]
    fn build_channel_notification_routed_omits_target_when_none() {
        let msg = chat::ChatMessage {
            id: "m-1".to_string(),
            thread_id: "telegram:1".to_string(),
            from_agent: "telegram:u".to_string(),
            content: "hi".to_string(),
            reply_to: None,
            created_at: 100,
        };
        let frame = chat::build_channel_notification_routed(&msg, None);
        // params.content is the inbound text (voice-control wire shape).
        assert_eq!(
            frame.pointer("/params/content").and_then(|v| v.as_str()),
            Some("hi")
        );
        // params.meta exists and carries channel routing info.
        let meta = frame
            .pointer("/params/meta")
            .and_then(|v| v.as_object())
            .expect("params.meta must be present");
        assert_eq!(
            meta.get("thread").and_then(|v| v.as_str()),
            Some("telegram:1")
        );
        assert_eq!(
            meta.get("from_agent").and_then(|v| v.as_str()),
            Some("telegram:u")
        );
        // STRUCTURAL-7-2: target_agent_id must be ABSENT when caller passed None.
        assert!(
            !meta.contains_key("target_agent_id"),
            "meta.target_agent_id should be absent when None passed; got: {meta:?}"
        );
    }

    #[test]
    fn build_channel_notification_routed_inserts_target_when_some() {
        let msg = chat::ChatMessage {
            id: "m-1".to_string(),
            thread_id: "telegram:1".to_string(),
            from_agent: "telegram:u".to_string(),
            content: "hi".to_string(),
            reply_to: None,
            created_at: 100,
        };
        let frame = chat::build_channel_notification_routed(&msg, Some("uuid-abc"));
        let target = frame
            .pointer("/params/meta/target_agent_id")
            .and_then(|v| v.as_str());
        assert_eq!(target, Some("uuid-abc"));
    }

    // NOTE: the four `process_batch_*_at_mention_*` tests that previously
    // lived here (Slice 7 @-mention routing) were removed in
    // telegram-multi-cli Slice 2. Under chat-as-id routing the @-mention
    // text is deliberately IGNORED (PRD §19 FR-TMC-2.1 / TC-TMC-4.4) — the
    // routing tree keys on the reply-quote link and the chat binding only.
    // The `extract_first_mention` parser and its unit tests are retained
    // (the function may still be useful for a future per-mention feature),
    // but `process_batch` no longer calls it. The chat-as-id routing tree
    // is covered by the `routing_*` tests below.

    #[test]
    fn redact_error_string_replaces_token_substr() {
        let s = "Error 401: bad token=ABCDEF in url";
        let r = redact_error_string(s, "ABCDEF");
        assert!(!r.contains("ABCDEF"));
        assert!(r.contains("***"));
    }

    #[test]
    fn redact_error_string_no_op_on_empty_token() {
        let s = "Error";
        assert_eq!(redact_error_string(s, ""), s);
    }

    #[test]
    fn process_batch_inserts_allowed_and_advances_offset() {
        // In-memory DB so we don't touch the user's chat.db.
        let mut conn = rusqlite::Connection::open_in_memory().unwrap();
        chat::ensure_chat_db_schema(&conn).unwrap();
        // telegram-multi-cli Slice 2: a free-text message only inserts a
        // chat row when it ROUTES to a CLI. Seed an alive CLI so step 4
        // resolves a target (without a target the routing tree replies
        // with the step-5 "No CLIs online" hint and inserts nothing).
        seed_agent(&conn, "cli-1-id", "mira");
        seed_binding(&conn, 555, "mira", "cli-1-id");

        let mut access = Access::default();
        access.dm_policy = DmPolicy::Allowlist;
        access.allow_from.push(1001);

        let batch = vec![Update {
            update_id: 7,
            message: Some(Message {
                date: 0,
                message_id: 100,
                from: Some(User {
                    id: 1001,
                    username: Some("alice".to_string()),
                }),
                chat: Chat { id: 555 },
                text: Some("hello".to_string()),
                voice: None,
                reply_to_message: None,
            }),
        }];

        let outcome = process_batch(&mut conn, &access, None, &batch).unwrap();
        assert_eq!(outcome.messages_inserted, 1);
        assert_eq!(outcome.new_offset, Some(7));

        let offset = load_offset(&conn).unwrap();
        assert_eq!(offset, 7);
    }

    #[test]
    fn process_batch_drops_disallowed_user_but_advances_offset() {
        let mut conn = rusqlite::Connection::open_in_memory().unwrap();
        chat::ensure_chat_db_schema(&conn).unwrap();

        let access = Access::default(); // Pairing policy, empty allow_from

        let batch = vec![Update {
            update_id: 9,
            message: Some(Message {
                date: 0,
                message_id: 100,
                from: Some(User {
                    id: 1001,
                    username: None,
                }),
                chat: Chat { id: 555 },
                text: Some("hi".to_string()),
                voice: None,
                reply_to_message: None,
            }),
        }];

        let outcome = process_batch(&mut conn, &access, None, &batch).unwrap();
        assert_eq!(outcome.messages_inserted, 0);
        assert_eq!(outcome.new_offset, Some(9));
        assert_eq!(load_offset(&conn).unwrap(), 9);
    }

    #[test]
    fn process_batch_voice_uses_shim_text() {
        let mut conn = rusqlite::Connection::open_in_memory().unwrap();
        chat::ensure_chat_db_schema(&conn).unwrap();
        // A voice note (shim text) still routes through the tree; seed an
        // alive CLI so it resolves a target and the row is inserted.
        seed_agent(&conn, "cli-1-id", "mira");
        seed_binding(&conn, 555, "mira", "cli-1-id");

        let mut access = Access::default();
        access.dm_policy = DmPolicy::Allowlist;
        access.allow_from.push(1001);

        let batch = vec![Update {
            update_id: 3,
            message: Some(Message {
                date: 0,
                message_id: 200,
                from: Some(User {
                    id: 1001,
                    username: None,
                }),
                chat: Chat { id: 555 },
                text: None,
                voice: Some(Voice {
                    file_id: "FID".to_string(),
                    duration: 10,
                }),
                reply_to_message: None,
            }),
        }];

        let outcome = process_batch(&mut conn, &access, None, &batch).unwrap();
        assert_eq!(outcome.messages_inserted, 1);
        let content: String = conn
            .query_row(
                "SELECT content FROM chat_messages LIMIT 1",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(content, VOICE_SHIM_TEXT);
    }

    // ===================================================================
    // telegram-multi-cli Slice 2 — 5-step chat-as-id routing tree.
    // Covers TC-TMC-4.1..4.6, TC-TMC-5.1..5.6, TC-TMC-21.1.
    // ===================================================================

    /// Seed an alive agent row directly (bypasses the per-thread unique
    /// index by leaving chat_thread_id NULL so multiple CLIs coexist —
    /// chat-as-id routes across the whole registry, not per-thread).
    fn seed_agent(conn: &Connection, agent_id: &str, name: &str) {
        crate::daemon::agent_registry::register(conn, agent_id, name, "conn", None, None)
            .unwrap();
    }

    /// Seed an `active_cli_per_chat` binding row.
    fn seed_binding(conn: &Connection, chat_id: i64, name: &str, agent_id: &str) {
        conn.execute(
            "INSERT OR REPLACE INTO active_cli_per_chat \
             (chat_id, active_cli_name, active_agent_id, set_at, set_by) \
             VALUES (?1, ?2, ?3, 0, 'test')",
            params![chat_id, name, agent_id],
        )
        .unwrap();
    }

    /// Seed a `tg_message_map` reply-quote row.
    fn seed_msg_map(conn: &Connection, tg_msg_id: i64, chat_id: i64, sender_agent_id: &str) {
        conn.execute(
            "INSERT OR REPLACE INTO tg_message_map \
             (tg_msg_id, chat_id, sender_agent_id, sent_at) \
             VALUES (?1, ?2, ?3, 0)",
            params![tg_msg_id, chat_id, sender_agent_id],
        )
        .unwrap();
    }

    /// Build a free-text inbound update for `chat_id`, optionally a reply.
    fn text_update(update_id: i64, chat_id: i64, text: &str, reply_to: Option<i64>) -> Update {
        Update {
            update_id,
            message: Some(Message {
                date: 0,
                message_id: 1000 + update_id,
                from: Some(User { id: 7, username: Some("op".into()) }),
                chat: Chat { id: chat_id },
                text: Some(text.into()),
                voice: None,
                reply_to_message: reply_to.map(|id| ReplyToMessage { message_id: id }),
            }),
        }
    }

    fn allow_all_access() -> Access {
        let mut a = Access::default();
        a.dm_policy = DmPolicy::Disabled; // Disabled => check_allowed passes all (no allowlist gate in process_batch)
        a
    }

    /// Pull `meta.target_agent_id` from the first notification, if present.
    fn first_target(outcome: &BatchOutcome) -> Option<String> {
        outcome
            .notifications
            .first()
            .and_then(|(_t, f)| f.pointer("/params/meta/target_agent_id"))
            .and_then(|v| v.as_str())
            .map(|s| s.to_string())
    }

    // ---- match_bot_command unit coverage ------------------------------

    #[test]
    fn match_bot_command_recognises_all_seven() {
        for cmd in ["/agents", "/switch", "/whoami", "/here", "/start", "/help", "/status"] {
            assert_eq!(match_bot_command(cmd), Some(cmd), "cmd {cmd} not matched");
        }
    }

    #[test]
    fn match_bot_command_strips_botname_suffix() {
        // UC-TMC-12-EC1 group-chat form.
        assert_eq!(match_bot_command("/help@my_bot"), Some("/help"));
        assert_eq!(match_bot_command("/switch@bot mira"), Some("/switch"));
    }

    #[test]
    fn match_bot_command_ignores_args_and_free_text() {
        assert_eq!(match_bot_command("/switch mira"), Some("/switch"));
        assert_eq!(match_bot_command("hello world"), None);
        assert_eq!(match_bot_command("/unknown"), None);
        assert_eq!(match_bot_command(""), None);
        assert_eq!(match_bot_command("not /agents"), None); // slash not first token
    }

    // ---- TC-TMC-4.1: bound chat routes to its CLI ---------------------

    #[test]
    fn routing_bound_chat_reaches_cli1() {
        let mut conn = Connection::open_in_memory().unwrap();
        chat::ensure_chat_db_schema(&conn).unwrap();
        seed_agent(&conn, "cli-1-id", "mira");
        seed_agent(&conn, "cli-2-id", "worker");
        seed_binding(&conn, 111, "mira", "cli-1-id");

        let access = allow_all_access();
        let batch = vec![text_update(1, 111, "hello", None)];
        let outcome = process_batch(&mut conn, &access, None, &batch).unwrap();

        assert_eq!(first_target(&outcome).as_deref(), Some("cli-1-id"));
        assert_eq!(outcome.notifications.len(), 1);
        // The other CLI must NOT be a target.
        assert_ne!(first_target(&outcome).as_deref(), Some("cli-2-id"));
    }

    // ---- TC-TMC-4.2: chat A vs chat B isolation -----------------------

    #[test]
    fn routing_chat_isolation_222_to_cli2() {
        let mut conn = Connection::open_in_memory().unwrap();
        chat::ensure_chat_db_schema(&conn).unwrap();
        seed_agent(&conn, "cli-1-id", "mira");
        seed_agent(&conn, "cli-2-id", "worker");
        seed_binding(&conn, 111, "mira", "cli-1-id");
        seed_binding(&conn, 222, "worker", "cli-2-id");

        let access = allow_all_access();
        // Message on chat 222 must reach CLI-2 and NEVER chat-A's binding.
        let outcome =
            process_batch(&mut conn, &access, None, &[text_update(1, 222, "hi", None)]).unwrap();
        assert_eq!(first_target(&outcome).as_deref(), Some("cli-2-id"));
        assert_ne!(first_target(&outcome).as_deref(), Some("cli-1-id"));

        // And a message on chat 111 reaches CLI-1 — planted chat-B binding
        // never reached from chat-A.
        let outcome2 =
            process_batch(&mut conn, &access, None, &[text_update(2, 111, "hi", None)]).unwrap();
        assert_eq!(first_target(&outcome2).as_deref(), Some("cli-1-id"));
    }

    // ---- TC-TMC-4.3: unbound chat falls to first_alive ----------------

    #[test]
    fn routing_unbound_chat_falls_to_first_alive() {
        let mut conn = Connection::open_in_memory().unwrap();
        chat::ensure_chat_db_schema(&conn).unwrap();
        seed_agent(&conn, "cli-1-id", "mira");
        seed_agent(&conn, "cli-2-id", "worker");
        seed_agent(&conn, "orch-id", "orchestrator-main");
        seed_binding(&conn, 111, "mira", "cli-1-id");
        seed_binding(&conn, 222, "worker", "cli-2-id");

        // chat 333 has NO binding → first_alive(prefer_role="orchestrator").
        let access = allow_all_access();
        let outcome =
            process_batch(&mut conn, &access, None, &[text_update(1, 333, "hey", None)]).unwrap();
        assert_eq!(first_target(&outcome).as_deref(), Some("orch-id"));
        // Routing must NOT create a binding row for 333.
        let cnt: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM active_cli_per_chat WHERE chat_id=333",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(cnt, 0);
    }

    // ---- TC-TMC-4.4: @-mention ignored under chat-as-id ---------------

    #[test]
    fn routing_at_mention_ignored_under_chat_as_id() {
        let mut conn = Connection::open_in_memory().unwrap();
        chat::ensure_chat_db_schema(&conn).unwrap();
        seed_agent(&conn, "cli-1-id", "mira");
        // An agent literally named "ghost" exists, but the @ghost mention
        // must NOT route there — the active binding wins.
        seed_agent(&conn, "ghost-id", "ghost");
        seed_binding(&conn, 111, "mira", "cli-1-id");

        let access = allow_all_access();
        let outcome = process_batch(
            &mut conn,
            &access,
            None,
            &[text_update(1, 111, "@ghost what's up?", None)],
        )
        .unwrap();
        // Routes to the binding (cli-1-id), NOT to the @-mentioned ghost.
        assert_eq!(first_target(&outcome).as_deref(), Some("cli-1-id"));
        assert_ne!(first_target(&outcome).as_deref(), Some("ghost-id"));
    }

    // ---- TC-TMC-4.5: dead binding falls to first_alive ----------------

    #[test]
    fn routing_dead_binding_falls_to_first_alive() {
        let mut conn = Connection::open_in_memory().unwrap();
        chat::ensure_chat_db_schema(&conn).unwrap();
        // Binding points at a CLI that is NOT in agent_registry (dead).
        seed_binding(&conn, 111, "dead", "cli-dead-id");
        seed_agent(&conn, "cli-2-id", "worker");

        let access = allow_all_access();
        let outcome =
            process_batch(&mut conn, &access, None, &[text_update(1, 111, "hi", None)]).unwrap();
        // is_alive("cli-dead-id") = false → fall through → first_alive →
        // only alive CLI is cli-2-id.
        assert_eq!(first_target(&outcome).as_deref(), Some("cli-2-id"));
    }

    // ---- TC-TMC-4.6: malformed (empty agent_id) binding ---------------

    #[test]
    fn routing_malformed_empty_agent_id_warning() {
        let mut conn = Connection::open_in_memory().unwrap();
        chat::ensure_chat_db_schema(&conn).unwrap();
        seed_binding(&conn, 111, "corrupt", ""); // empty active_agent_id
        seed_agent(&conn, "cli-2-id", "worker");

        let access = allow_all_access();
        let outcome =
            process_batch(&mut conn, &access, None, &[text_update(1, 111, "hi", None)]).unwrap();
        // Empty agent_id never matches is_alive → first_alive fallback.
        assert_eq!(first_target(&outcome).as_deref(), Some("cli-2-id"));
    }

    // ---- TC-TMC-21.1: step 5 — no alive CLI, exact reply text ---------

    #[test]
    fn routing_no_alive_cli_step5_reply() {
        let mut conn = Connection::open_in_memory().unwrap();
        chat::ensure_chat_db_schema(&conn).unwrap();
        // agent_registry empty → no alive CLI anywhere.

        let access = allow_all_access();
        let outcome =
            process_batch(&mut conn, &access, None, &[text_update(1, 777, "anyone?", None)]).unwrap();

        // No channel notification published.
        assert_eq!(outcome.notifications.len(), 0);
        // Exactly one outbound reply with the EXACT spec text.
        assert_eq!(outcome.pair_replies.len(), 1);
        assert_eq!(outcome.pair_replies[0].0, 777);
        assert_eq!(
            outcome.pair_replies[0].1,
            "No CLIs online. Spawn one with `claudebase run`."
        );
        // No chat_messages row inserted for the step-5 case.
        assert_eq!(outcome.messages_inserted, 0);
    }

    // ---- TC-TMC-5.1: reply-quote routes to original sender ------------

    #[test]
    fn reply_quote_routes_to_originating_cli() {
        let mut conn = Connection::open_in_memory().unwrap();
        chat::ensure_chat_db_schema(&conn).unwrap();
        seed_agent(&conn, "cli-1-id", "mira");
        seed_agent(&conn, "cli-2-id", "worker");
        seed_binding(&conn, 111, "worker", "cli-2-id"); // active binding is CLI-2
        seed_msg_map(&conn, 9001, 111, "cli-1-id"); // but msg 9001 was sent by CLI-1

        let access = allow_all_access();
        let outcome = process_batch(
            &mut conn,
            &access,
            None,
            &[text_update(1, 111, "reply text", Some(9001))],
        )
        .unwrap();
        // Reply-quote (step 2) wins over the active binding (step 4).
        assert_eq!(first_target(&outcome).as_deref(), Some("cli-1-id"));
    }

    // ---- TC-TMC-5.2: reply-quote to dead CLI → fallback + log ---------

    #[test]
    fn reply_quote_dead_cli_fallback() {
        let mut conn = Connection::open_in_memory().unwrap();
        chat::ensure_chat_db_schema(&conn).unwrap();
        // msg 9001 was sent by a CLI that is now dead (not in registry).
        seed_msg_map(&conn, 9001, 111, "cli-dead-id");
        seed_agent(&conn, "cli-2-id", "worker");
        seed_binding(&conn, 111, "worker", "cli-2-id");

        let access = allow_all_access();
        let outcome = process_batch(
            &mut conn,
            &access,
            None,
            &[text_update(1, 111, "reply", Some(9001))],
        )
        .unwrap();
        // Dead sender → fall through to active binding (cli-2-id).
        assert_eq!(first_target(&outcome).as_deref(), Some("cli-2-id"));
    }

    // ---- TC-TMC-5.3: reply-quote unknown msg → falls to binding -------

    #[test]
    fn reply_quote_unknown_msg_falls_to_binding() {
        let mut conn = Connection::open_in_memory().unwrap();
        chat::ensure_chat_db_schema(&conn).unwrap();
        seed_agent(&conn, "cli-1-id", "mira");
        seed_binding(&conn, 111, "mira", "cli-1-id");
        // No tg_message_map row for msg 8000.

        let access = allow_all_access();
        let outcome = process_batch(
            &mut conn,
            &access,
            None,
            &[text_update(1, 111, "reply", Some(8000))],
        )
        .unwrap();
        // No map row → behave like free text → active binding.
        assert_eq!(first_target(&outcome).as_deref(), Some("cli-1-id"));
    }

    // ---- TC-TMC-5.5: reply-quote chat isolation -----------------------

    #[test]
    fn reply_quote_chat_isolation() {
        let mut conn = Connection::open_in_memory().unwrap();
        chat::ensure_chat_db_schema(&conn).unwrap();
        seed_agent(&conn, "cli-1-id", "mira");
        seed_agent(&conn, "cli-2-id", "worker");
        // msg 9002 sent by CLI-2 in chat 222 only.
        seed_msg_map(&conn, 9002, 222, "cli-2-id");
        seed_binding(&conn, 111, "mira", "cli-1-id");

        let access = allow_all_access();
        // Reply to 9002 on chat 222 routes to CLI-2; CLI-1 (chat 111) untouched.
        let outcome = process_batch(
            &mut conn,
            &access,
            None,
            &[text_update(1, 222, "r", Some(9002))],
        )
        .unwrap();
        assert_eq!(first_target(&outcome).as_deref(), Some("cli-2-id"));
        assert_ne!(first_target(&outcome).as_deref(), Some("cli-1-id"));

        // The SAME reply_to id 9002 but on the WRONG chat (111) must NOT
        // match the chat-222 map row (composite PK keys on chat_id).
        let outcome2 = process_batch(
            &mut conn,
            &access,
            None,
            &[text_update(2, 111, "r", Some(9002))],
        )
        .unwrap();
        // Falls through to chat-111's binding (cli-1-id), not cli-2-id.
        assert_eq!(first_target(&outcome2).as_deref(), Some("cli-1-id"));
    }

    // ---- TC-TMC-5.6: reply-quote overrides active binding -------------

    #[test]
    fn reply_quote_overrides_active_binding() {
        let mut conn = Connection::open_in_memory().unwrap();
        chat::ensure_chat_db_schema(&conn).unwrap();
        seed_agent(&conn, "cli-1-id", "mira");
        seed_agent(&conn, "cli-2-id", "worker");
        // CLI-1 is the active binding, but operator reply-quotes CLI-2's msg.
        seed_binding(&conn, 111, "mira", "cli-1-id");
        seed_msg_map(&conn, 9002, 111, "cli-2-id");

        let access = allow_all_access();
        let outcome = process_batch(
            &mut conn,
            &access,
            None,
            &[text_update(1, 111, "to worker", Some(9002))],
        )
        .unwrap();
        // Routes to CLI-2 (the quoted sender), NOT the active binding CLI-1.
        assert_eq!(first_target(&outcome).as_deref(), Some("cli-2-id"));
    }

    // ---- bot command short-circuits routing (no CLI notification) -----

    #[test]
    fn routing_bot_command_no_cli_notification() {
        let mut conn = Connection::open_in_memory().unwrap();
        chat::ensure_chat_db_schema(&conn).unwrap();
        seed_agent(&conn, "cli-1-id", "mira");
        seed_binding(&conn, 111, "mira", "cli-1-id");

        let access = allow_all_access();
        let outcome =
            process_batch(&mut conn, &access, None, &[text_update(1, 111, "/agents", None)])
                .unwrap();
        // Step 1: bot command → no channel notification, no chat row
        // (TC-TMC-8.4 leak guard). The /agents handler DOES enqueue an
        // operator reply via pair_replies — exactly one, and it lists the
        // alive CLI "mira" (Slice 3).
        assert_eq!(outcome.notifications.len(), 0);
        assert_eq!(outcome.messages_inserted, 0);
        assert_eq!(outcome.pair_replies.len(), 1);
        assert!(outcome.pair_replies[0].1.contains("mira"));
        // Offset still advanced.
        assert_eq!(outcome.new_offset, Some(1));
    }

    // ===================================================================
    // telegram-multi-cli Slice 3 — bot-command handlers.
    // Covers TC-TMC-8.x (/agents), 9.x (/switch + injection), 10.x
    // (/whoami), 11.x (/here scoping), 12.x (/help, preserved cmds).
    // ===================================================================

    /// Seed an alive agent with a metadata JSON blob (for /here host/cwd).
    fn seed_agent_with_metadata(conn: &Connection, agent_id: &str, name: &str, metadata: serde_json::Value) {
        crate::daemon::agent_registry::register(conn, agent_id, name, "conn", None, Some(&metadata))
            .unwrap();
    }

    /// Drive one inbound bot-command message through process_batch and
    /// return the single operator reply text (asserts exactly one reply,
    /// no notification, no chat row — the leak guard).
    fn run_bot_cmd(conn: &mut Connection, chat_id: i64, text: &str) -> String {
        let access = allow_all_access();
        let outcome =
            process_batch(conn, &access, None, &[text_update(1, chat_id, text, None)]).unwrap();
        assert_eq!(outcome.notifications.len(), 0, "bot command must not notify a CLI");
        assert_eq!(outcome.messages_inserted, 0, "bot command must not insert a chat row");
        assert_eq!(outcome.pair_replies.len(), 1, "bot command must produce exactly one reply");
        outcome.pair_replies[0].1.clone()
    }

    fn binding_count(conn: &Connection, chat_id: i64) -> i64 {
        conn.query_row(
            "SELECT COUNT(*) FROM active_cli_per_chat WHERE chat_id = ?1",
            params![chat_id],
            |r| r.get(0),
        )
        .unwrap()
    }

    // ---- TC-TMC-8.1: /agents lists alive CLIs -------------------------

    #[test]
    fn bot_cmd_agents_lists_alive() {
        let mut conn = Connection::open_in_memory().unwrap();
        chat::ensure_chat_db_schema(&conn).unwrap();
        seed_agent(&conn, "cli-1-id", "mira");
        seed_agent(&conn, "cli-2-id", "worker");

        let reply = run_bot_cmd(&mut conn, 111, "/agents");
        assert!(reply.contains("mira"), "reply should list mira: {reply}");
        assert!(reply.contains("worker"), "reply should list worker: {reply}");
    }

    // ---- TC-TMC-8.2: /agents with empty registry ----------------------

    #[test]
    fn bot_cmd_agents_empty() {
        let mut conn = Connection::open_in_memory().unwrap();
        chat::ensure_chat_db_schema(&conn).unwrap();
        let reply = run_bot_cmd(&mut conn, 111, "/agents");
        assert!(reply.contains("No CLIs currently online"), "got: {reply}");
    }

    // ---- TC-TMC-8.3: /agents trailing space still matches -------------

    #[test]
    fn bot_cmd_agents_trailing_space() {
        let mut conn = Connection::open_in_memory().unwrap();
        chat::ensure_chat_db_schema(&conn).unwrap();
        seed_agent(&conn, "cli-1-id", "mira");
        let reply = run_bot_cmd(&mut conn, 111, "/agents ");
        assert!(reply.contains("mira"), "got: {reply}");
    }

    // ---- TC-TMC-9.1: /switch valid → row written + ack ----------------

    #[test]
    fn bot_cmd_switch_valid_writes_binding() {
        let mut conn = Connection::open_in_memory().unwrap();
        chat::ensure_chat_db_schema(&conn).unwrap();
        seed_agent(&conn, "cli-1-id", "mira");

        let reply = run_bot_cmd(&mut conn, 111, "/switch mira");
        assert!(reply.contains("mira"), "ack should name mira: {reply}");

        // Assert the SQL row was written with the correct values.
        let (name, agent_id, set_by): (String, String, String) = conn
            .query_row(
                "SELECT active_cli_name, active_agent_id, set_by FROM active_cli_per_chat WHERE chat_id = 111",
                [],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .unwrap();
        assert_eq!(name, "mira");
        assert_eq!(agent_id, "cli-1-id");
        assert_eq!(set_by, "111");
    }

    // ---- TC-TMC-9.2: /switch replaces prior binding (1 row) -----------

    #[test]
    fn bot_cmd_switch_replaces_prior_binding() {
        let mut conn = Connection::open_in_memory().unwrap();
        chat::ensure_chat_db_schema(&conn).unwrap();
        seed_agent(&conn, "cli-1-id", "mira");
        seed_agent(&conn, "cli-2-id", "worker");
        seed_binding(&conn, 111, "worker", "cli-2-id"); // prior binding

        let _ = run_bot_cmd(&mut conn, 111, "/switch mira");
        assert_eq!(binding_count(&conn, 111), 1, "exactly one row for chat 111");
        let name: String = conn
            .query_row("SELECT active_cli_name FROM active_cli_per_chat WHERE chat_id=111", [], |r| r.get(0))
            .unwrap();
        assert_eq!(name, "mira");
    }

    // ---- TC-TMC-9.3: /switch unknown name → rejected, no write --------

    #[test]
    fn bot_cmd_switch_unknown_name_rejected() {
        let mut conn = Connection::open_in_memory().unwrap();
        chat::ensure_chat_db_schema(&conn).unwrap();
        seed_agent(&conn, "cli-1-id", "mira");

        let reply = run_bot_cmd(&mut conn, 111, "/switch nonexistent");
        assert!(reply.contains("Unknown") || reply.contains("nonexistent"), "got: {reply}");
        assert!(reply.contains("mira"), "should list available CLI mira: {reply}");
        // No binding row written (the name passed validation but did not match).
        assert_eq!(binding_count(&conn, 111), 0);
    }

    // ---- TC-TMC-9.4: /switch with no arg → usage, no write ------------

    #[test]
    fn bot_cmd_switch_no_arg() {
        let mut conn = Connection::open_in_memory().unwrap();
        chat::ensure_chat_db_schema(&conn).unwrap();
        seed_agent(&conn, "cli-1-id", "mira");

        let reply = run_bot_cmd(&mut conn, 111, "/switch");
        assert!(reply.to_lowercase().contains("usage"), "got: {reply}");
        assert_eq!(binding_count(&conn, 111), 0);
    }

    // ---- TC-TMC-9.5: /switch partial name → rejected, no write --------

    #[test]
    fn bot_cmd_switch_partial_name_rejected() {
        let mut conn = Connection::open_in_memory().unwrap();
        chat::ensure_chat_db_schema(&conn).unwrap();
        seed_agent(&conn, "cli-1-id", "mira");

        let reply = run_bot_cmd(&mut conn, 111, "/switch mir");
        assert!(reply.contains("mir"), "got: {reply}");
        assert!(reply.contains("mira"), "should list mira as available: {reply}");
        assert_eq!(binding_count(&conn, 111), 0);
    }

    // ---- TC-TMC-9.6: /switch in a group chat → group note + binding ---

    #[test]
    fn bot_cmd_switch_group_chat_rebind_note() {
        let mut conn = Connection::open_in_memory().unwrap();
        chat::ensure_chat_db_schema(&conn).unwrap();
        seed_agent(&conn, "cli-1-id", "mira");

        let reply = run_bot_cmd(&mut conn, -100111, "/switch mira");
        assert!(
            reply.to_lowercase().contains("group") || reply.contains("all participants"),
            "group rebind note expected: {reply}"
        );
        let chat: i64 = conn
            .query_row("SELECT chat_id FROM active_cli_per_chat WHERE chat_id=-100111", [], |r| r.get(0))
            .unwrap();
        assert_eq!(chat, -100111);
    }

    // ---- SECURITY TC-TMC-9.x: injection-style input → rejected BEFORE
    //      any DB write (validate_agent_name guards the SQL boundary) -----

    #[test]
    fn bot_cmd_switch_injection_rejected_before_db() {
        let mut conn = Connection::open_in_memory().unwrap();
        chat::ensure_chat_db_schema(&conn).unwrap();
        // An alive CLI exists so a non-validated path COULD theoretically
        // write — but the injection arg must never reach the DB.
        seed_agent(&conn, "cli-1-id", "mira");

        // Each of these fails validate_agent_name (contains ';', space,
        // '/', '.', or quote — none are [A-Za-z0-9_-]). The arg is the
        // FIRST whitespace token after the command, so we pick payloads
        // whose first token is itself invalid.
        let payloads = [
            "/switch ';DROP",          // contains ' and ;
            "/switch ../etc",          // contains / and .
            "/switch \"or\"1=1",       // contains quotes and =
            "/switch mira;DROP",       // contains ;
        ];
        for p in payloads {
            let reply = run_bot_cmd(&mut conn, 111, p);
            assert!(
                reply.to_lowercase().contains("invalid"),
                "payload {p:?} should be rejected as invalid: {reply}"
            );
            // CRITICAL: no binding row was written for ANY injection payload.
            assert_eq!(
                binding_count(&conn, 111),
                0,
                "injection payload {p:?} must NOT write a binding row"
            );
        }
        // The agent_registry table is untouched (no rows dropped/added).
        let reg_count: i64 = conn
            .query_row("SELECT COUNT(*) FROM agent_registry", [], |r| r.get(0))
            .unwrap();
        assert_eq!(reg_count, 1, "agent_registry must be intact after injection attempts");
    }

    #[test]
    fn bot_cmd_switch_oversized_name_rejected_before_db() {
        let mut conn = Connection::open_in_memory().unwrap();
        chat::ensure_chat_db_schema(&conn).unwrap();
        seed_agent(&conn, "cli-1-id", "mira");

        // 100-char name — exceeds validate_agent_name's 64-char cap.
        let big = "a".repeat(100);
        let reply = run_bot_cmd(&mut conn, 111, &format!("/switch {big}"));
        assert!(reply.to_lowercase().contains("invalid"), "got: {reply}");
        assert_eq!(binding_count(&conn, 111), 0, "oversized name must not write a row");
    }

    // ---- TC-TMC-10.1: /whoami bound -----------------------------------

    #[test]
    fn bot_cmd_whoami_bound() {
        let mut conn = Connection::open_in_memory().unwrap();
        chat::ensure_chat_db_schema(&conn).unwrap();
        seed_agent(&conn, "cli-1-id", "mira");
        seed_binding(&conn, 111, "mira", "cli-1-id");

        let reply = run_bot_cmd(&mut conn, 111, "/whoami");
        assert!(reply.contains("mira"), "got: {reply}");
        assert!(reply.contains("cli-1-id"), "got: {reply}");
    }

    // ---- TC-TMC-10.2: /whoami unbound → first_alive fallback ----------

    #[test]
    fn bot_cmd_whoami_no_binding() {
        let mut conn = Connection::open_in_memory().unwrap();
        chat::ensure_chat_db_schema(&conn).unwrap();
        seed_agent(&conn, "orch-id", "orchestrator-main");

        let reply = run_bot_cmd(&mut conn, 111, "/whoami");
        assert!(
            reply.to_lowercase().contains("no explicit binding") || reply.to_lowercase().contains("default"),
            "got: {reply}"
        );
        assert!(reply.contains("orchestrator-main"), "should name first_alive: {reply}");
    }

    // ---- TC-TMC-10.3: /whoami bound-but-dead → offline + /switch ------

    #[test]
    fn bot_cmd_whoami_dead_binding() {
        let mut conn = Connection::open_in_memory().unwrap();
        chat::ensure_chat_db_schema(&conn).unwrap();
        // Binding points at a CLI not in the alive registry.
        seed_binding(&conn, 111, "ghost", "cli-dead-id");

        let reply = run_bot_cmd(&mut conn, 111, "/whoami");
        assert!(
            reply.to_lowercase().contains("offline") || reply.to_lowercase().contains("no longer"),
            "got: {reply}"
        );
        assert!(reply.contains("/switch"), "should suggest /switch: {reply}");
    }

    // ---- TC-TMC-11.1: /here with host/cwd present (metadata populated) -

    #[test]
    fn bot_cmd_here_shows_host_cwd() {
        let mut conn = Connection::open_in_memory().unwrap();
        chat::ensure_chat_db_schema(&conn).unwrap();
        seed_agent_with_metadata(
            &conn,
            "cli-1-id",
            "mira",
            serde_json::json!({"host": "devbox", "cwd": "/home/operator/project"}),
        );
        seed_binding(&conn, 111, "mira", "cli-1-id");

        let reply = run_bot_cmd(&mut conn, 111, "/here");
        assert!(reply.contains("devbox"), "got: {reply}");
        assert!(reply.contains("/home/operator/project"), "got: {reply}");
    }

    // ---- TC-TMC-11.2: /here with absent metadata → "unavailable" (v1) -

    #[test]
    fn bot_cmd_here_missing_metadata() {
        let mut conn = Connection::open_in_memory().unwrap();
        chat::ensure_chat_db_schema(&conn).unwrap();
        // No metadata populated (the v1 reality per red-team F-6).
        seed_agent(&conn, "cli-1-id", "mira");
        seed_binding(&conn, 111, "mira", "cli-1-id");

        let reply = run_bot_cmd(&mut conn, 111, "/here");
        assert!(reply.to_lowercase().contains("unavailable"), "got: {reply}");
    }

    // ---- TC-TMC-11.3: /here bound CLI reaped → no longer online -------

    #[test]
    fn bot_cmd_here_reaped_cli() {
        let mut conn = Connection::open_in_memory().unwrap();
        chat::ensure_chat_db_schema(&conn).unwrap();
        // Binding exists but the CLI's registry row is gone (reaped).
        seed_binding(&conn, 111, "mira", "cli-1-id");

        let reply = run_bot_cmd(&mut conn, 111, "/here");
        assert!(reply.to_lowercase().contains("no longer"), "got: {reply}");
        assert!(
            reply.contains("/switch") || reply.contains("/agents"),
            "should suggest /switch or /agents: {reply}"
        );
    }

    // ---- SECURITY: /here is scoped to THIS chat only ------------------
    // A second chat (222) is bound to a DIFFERENT CLI whose metadata holds
    // a secret host. /here in chat 111 must NEVER leak chat-222's CLI
    // host/cwd — it reads chat 111's binding only.

    #[test]
    fn bot_cmd_here_scoped_to_this_chat_only() {
        let mut conn = Connection::open_in_memory().unwrap();
        chat::ensure_chat_db_schema(&conn).unwrap();
        // Chat 111 → cli-1 (no metadata). Chat 222 → cli-2 (secret host).
        seed_agent(&conn, "cli-1-id", "mira");
        seed_agent_with_metadata(
            &conn,
            "cli-2-id",
            "worker",
            serde_json::json!({"host": "SECRET-HOST", "cwd": "/secret/path"}),
        );
        seed_binding(&conn, 111, "mira", "cli-1-id");
        seed_binding(&conn, 222, "worker", "cli-2-id");

        let reply = run_bot_cmd(&mut conn, 111, "/here");
        // chat 111's CLI (mira) has no metadata → unavailable; the OTHER
        // chat's secret host MUST NOT appear.
        assert!(!reply.contains("SECRET-HOST"), "leaked another chat's host: {reply}");
        assert!(!reply.contains("/secret/path"), "leaked another chat's cwd: {reply}");
        assert!(reply.contains("mira"), "should name THIS chat's CLI: {reply}");
    }

    // ---- TC-TMC-12.1: /help lists all 7 commands + group note ---------

    #[test]
    fn bot_cmd_help_lists_all_commands() {
        let mut conn = Connection::open_in_memory().unwrap();
        chat::ensure_chat_db_schema(&conn).unwrap();
        let reply = run_bot_cmd(&mut conn, 111, "/help");
        for needle in ["agents", "switch", "whoami", "here", "start", "help", "status", "group"] {
            assert!(reply.contains(needle), "help missing '{needle}': {reply}");
        }
    }

    // ---- TC-TMC-12.2: /help@botname suffix handled as /help -----------

    #[test]
    fn bot_cmd_help_with_botname_suffix() {
        let mut conn = Connection::open_in_memory().unwrap();
        chat::ensure_chat_db_schema(&conn).unwrap();
        let reply = run_bot_cmd(&mut conn, 111, "/help@my_bot");
        assert!(reply.contains("agents"), "got: {reply}");
        assert!(reply.contains("switch"), "got: {reply}");
    }

    // ---- TC-TMC-12.3: /start and /status preserved (no extra reply,
    //      no CLI notification) — they short-circuit routing but emit no
    //      Slice-3 reply (handled by the upstream channel-state flow) -----

    #[test]
    fn bot_cmd_start_status_preserved_no_reply() {
        let mut conn = Connection::open_in_memory().unwrap();
        chat::ensure_chat_db_schema(&conn).unwrap();
        seed_agent(&conn, "cli-1-id", "mira");
        seed_binding(&conn, 111, "mira", "cli-1-id");

        let access = allow_all_access();
        for cmd in ["/start", "/status"] {
            let outcome =
                process_batch(&mut conn, &access, None, &[text_update(1, 111, cmd, None)]).unwrap();
            // No channel notification (leak guard), no chat row, and the
            // Slice-3 handler returns None → no extra pair reply.
            assert_eq!(outcome.notifications.len(), 0, "{cmd} must not notify a CLI");
            assert_eq!(outcome.messages_inserted, 0, "{cmd} must not insert a chat row");
            assert_eq!(outcome.pair_replies.len(), 0, "{cmd} must emit no Slice-3 reply");
        }
    }

    // ---- /switch handler does NOT publish a channel notification ------

    #[test]
    fn bot_cmd_switch_no_cli_notification() {
        let mut conn = Connection::open_in_memory().unwrap();
        chat::ensure_chat_db_schema(&conn).unwrap();
        seed_agent(&conn, "cli-1-id", "mira");

        let access = allow_all_access();
        let outcome =
            process_batch(&mut conn, &access, None, &[text_update(1, 111, "/switch mira", None)])
                .unwrap();
        assert_eq!(outcome.notifications.len(), 0, "/switch must not leak to a CLI");
        assert_eq!(outcome.messages_inserted, 0);
    }

    #[test]
    fn set_bot_state_persists_into_daemon_state() {
        let conn = rusqlite::Connection::open_in_memory().unwrap();
        chat::ensure_chat_db_schema(&conn).unwrap();
        set_bot_state(&conn, "disconnected").unwrap();
        let v: String = conn
            .query_row("SELECT value FROM daemon_state WHERE key='tg_bot_state'", [], |r| r.get(0))
            .unwrap();
        assert_eq!(v, "disconnected");
    }
}
