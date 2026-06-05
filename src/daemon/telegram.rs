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

/// Outbound channel from MCP `chat_reply` (server.rs::handle_chat_post)
/// to the telegram long-poll task. Set ONCE at spawn_long_poll time;
/// reads happen in run_long_poll's select! loop.
///
/// Tuple shape: `(chat_id, thread_id, text)` — chat_id is the integer
/// parsed from the `telegram:<N>` thread prefix used by chat_reply tool
/// callers; thread_id (Slice 3 of multi-agent-telegram-on-v0.6) is the
/// optional Telegram forum-topic id from the inbound notification's
/// `meta.thread_id` echoed back by the CLI's `chat_reply` tool call so
/// the outbound lands in the same forum topic the inbound came from
/// (KP2/KP3 round-trip).
/// Guaranteed-delivery extension 2026-06-05 (operator directive):
/// the 4th tuple field is the chat_messages.id for the persisted row.
/// On successful TG send, run_long_poll UPDATE's chat_messages.delivered_at
/// for that row. On daemon startup, `drain_pending_outbound_tg` re-enqueues
/// any chat_messages with thread `telegram:%`, from_agent != inbound-sender,
/// delivered_at IS NULL, created_at > now - 24h — so a daemon crash between
/// enqueue and send does NOT silently lose the message. None = legacy/skip-tracking.
static OUTBOUND_TG: OnceLock<
    mpsc::UnboundedSender<(i64, Option<i64>, String, Option<String>)>,
> = OnceLock::new();

/// Slice 8 of multi-agent-telegram-on-v0.6 — outbound channel for
/// inline-keyboard messages spawned by the `chat_ask` MCP tool. Carries
/// the (chat_id, thread_id, text, options, ack) tuple. Per architect
/// AR-1: a parallel channel preserves the single-`Bot`-owner discipline
/// — the keyboard send is dispatched in the same drain loop, against the
/// same teloxide `Bot` instance — without coupling every plain
/// `chat_reply` path to teloxide keyboard types. The `ack` oneshot
/// returns the captured Telegram `message_id` (or a redacted send-error)
/// back to the chat_ask handler so it can INSERT `pending_asks` with
/// the real message_id (send-then-insert ordering per AR-1).
pub struct KeyboardOutbound {
    pub chat_id: i64,
    pub thread_id: Option<i64>,
    pub text: String,
    /// `(button_label, callback_data)` pairs — one button per option,
    /// one row each. The `chat_ask` handler validates that every
    /// `callback_data` fits the Telegram 64-byte budget at request time
    /// (FR-MAT-11.9).
    pub options: Vec<(String, String)>,
    pub ack: tokio::sync::oneshot::Sender<Result<i64>>,
}

static OUTBOUND_TG_KEYBOARD: OnceLock<mpsc::UnboundedSender<KeyboardOutbound>> = OnceLock::new();

/// Push an outbound Telegram message from any task. Returns Ok(()) on
/// successful enqueue (does NOT wait for HTTP send completion). Returns
/// Err if telegram long-poll is not running OR the channel is closed.
///
/// Slice 3 of multi-agent-telegram-on-v0.6: when `thread_id` is `Some`,
/// the outbound `sendMessage` call carries the Telegram forum-topic id
/// so the response lands in the correct topic. When `None`, the
/// outbound is a plain DM / topic-less reply (Slice 0 baseline shape).
pub fn enqueue_outbound_tg(chat_id: i64, thread_id: Option<i64>, text: String) -> Result<()> {
    enqueue_outbound_tg_tracked(chat_id, thread_id, text, None)
}

/// Same as `enqueue_outbound_tg` but tracks the chat_messages row for
/// guaranteed-delivery (operator directive 2026-06-05). When `message_id`
/// is `Some`, the run_long_poll send loop UPDATE's chat_messages.delivered_at
/// on successful send. On daemon startup, undelivered TG messages are
/// re-enqueued from chat.db so a daemon crash between enqueue and send
/// does NOT lose the message.
pub fn enqueue_outbound_tg_tracked(
    chat_id: i64,
    thread_id: Option<i64>,
    text: String,
    message_id: Option<String>,
) -> Result<()> {
    let tx = OUTBOUND_TG
        .get()
        .ok_or_else(|| anyhow::anyhow!("telegram outbound channel not initialised (long-poll task not spawned)"))?;
    tx.send((chat_id, thread_id, text, message_id))
        .map_err(|e| anyhow::anyhow!("outbound channel closed: {e}"))?;
    Ok(())
}

/// Guaranteed-delivery 2026-06-05 (operator directive): on daemon startup,
/// scan chat.db for agent-sent TG messages whose `delivered_at` is NULL
/// and re-enqueue them through OUTBOUND_TG. This recovers messages that
/// were enqueued by `chat_reply` but never reached Telegram because the
/// daemon crashed / was bounced before the long-poll send loop drained
/// them. The 24h cutoff prevents replay of ancient stale outbound on a
/// fresh install. Routing-key state (which agent is currently bound via
/// /switch) is intentionally NOT considered — outbound is unconditional
/// per operator directive.
///
/// Returns the count of re-enqueued messages on success. Errors are
/// logged-and-swallowed at the caller (long-poll spawn) — a chat.db
/// access failure here should NOT block the daemon from starting up;
/// new outbound continues to work.
pub fn drain_pending_outbound_tg() -> Result<usize> {
    use rusqlite::params;
    let conn = chat::open_chat_db()?;
    let cutoff_ms = chat::now_millis() - 24 * 60 * 60 * 1000; // 24h ago
    let mut stmt = conn.prepare(
        "SELECT id, thread_id, content FROM chat_messages \
         WHERE thread_id LIKE 'telegram:%' \
           AND delivered_at IS NULL \
           AND created_at > ?1 \
           AND from_agent != 'tg' \
         ORDER BY created_at ASC",
    )?;
    let rows = stmt.query_map(params![cutoff_ms], |row| {
        Ok((
            row.get::<_, String>(0)?,
            row.get::<_, String>(1)?,
            row.get::<_, String>(2)?,
        ))
    })?;
    let mut count = 0usize;
    for r in rows.flatten() {
        let (msg_id, thread, content) = r;
        let Some(chat_id_str) = thread.strip_prefix("telegram:") else {
            continue;
        };
        let Ok(chat_id) = chat_id_str.parse::<i64>() else {
            continue;
        };
        match enqueue_outbound_tg_tracked(chat_id, None, content, Some(msg_id.clone())) {
            Ok(_) => {
                count += 1;
                tracing::info!(
                    msg_id = %msg_id,
                    chat_id,
                    "re-enqueued pending TG outbound from chat.db on daemon startup"
                );
            }
            Err(e) => tracing::warn!(
                msg_id = %msg_id,
                error = %e,
                "failed to re-enqueue pending TG outbound — message will retry on next daemon start"
            ),
        }
    }
    Ok(count)
}

/// Slice 8 — enqueue a `chat_ask` keyboard outbound. Returns the
/// oneshot::Receiver the caller awaits to obtain the captured
/// `message_id` AFTER the daemon successfully sends the inline-keyboard
/// message via teloxide. The receiver yields `Err(_)` if the send
/// failed (network error, 401, etc.) — in that case the chat_ask
/// handler MUST NOT INSERT a pending_asks row (send-then-insert
/// ordering per architect AR-1).
pub fn enqueue_outbound_tg_keyboard(
    chat_id: i64,
    thread_id: Option<i64>,
    text: String,
    options: Vec<(String, String)>,
) -> Result<tokio::sync::oneshot::Receiver<Result<i64>>> {
    let tx = OUTBOUND_TG_KEYBOARD.get().ok_or_else(|| {
        anyhow::anyhow!("telegram keyboard outbound channel not initialised (long-poll task not spawned)")
    })?;
    let (ack_tx, ack_rx) = tokio::sync::oneshot::channel::<Result<i64>>();
    let payload = KeyboardOutbound {
        chat_id,
        thread_id,
        text,
        options,
        ack: ack_tx,
    };
    tx.send(payload)
        .map_err(|e| anyhow::anyhow!("keyboard outbound channel closed: {e}"))?;
    Ok(ack_rx)
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
    /// Slice 8 of multi-agent-telegram-on-v0.6 — Telegram CallbackQuery,
    /// raised when the operator taps an `InlineKeyboardButton` rendered
    /// by `chat_ask`. Architect AR-2: additive `Option<>` preserves the
    /// Slice 0 baseline wire shape bit-for-bit when callback_query is
    /// absent OR `null` — `#[serde(default)] Option<>` yields `None` in
    /// both cases. Telegram getUpdates dispatches Message vs CallbackQuery
    /// in MUTUALLY EXCLUSIVE update objects in practice, but we accept
    /// any combination.
    #[serde(default)]
    pub callback_query: Option<CallbackQuery>,
}

/// Slice 8 — Telegram CallbackQuery surface. The minimal field set per
/// architect AR-2 (and Telegram Bot API
/// https://core.telegram.org/bots/api#callbackquery): `chat_instance` is
/// REQUIRED (Telegram uses it for cache scoping); `data` is OPTIONAL
/// per the official spec (a game button has no data) but our daemon
/// requires it to be present to dispatch the ask_id — undefined data
/// callbacks are logged and silently dropped.
#[derive(Debug, Deserialize)]
pub struct CallbackQuery {
    pub id: String,
    pub from: User,
    pub chat_instance: String,
    #[serde(default)]
    pub data: Option<String>,
    /// The original message that carried the inline keyboard. `message_id`
    /// is what the daemon passes to `Bot::edit_message_reply_markup` for
    /// the Slice 8b multi-select state-machine. Telegram occasionally
    /// returns `None` here for inline-bot callbacks; our daemon does not
    /// participate in inline bot mode (we only render keyboards under
    /// regular `sendMessage`), so this should always be `Some` in practice.
    #[serde(default)]
    pub message: Option<MessageRef>,
}

/// Slice 8 — minimal reference to the message that carried the inline
/// keyboard. Telegram returns the FULL Message JSON; we deserialize only
/// the (message_id, chat) pair the daemon needs for edit_message_reply_markup.
#[derive(Debug, Deserialize)]
pub struct MessageRef {
    pub message_id: i64,
    pub chat: Chat,
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
    /// Slice 2 of multi-agent-telegram-on-v0.6 — Telegram forum-topic id.
    /// Present only when the bot is in a supergroup with forum topics
    /// enabled and the user posted into a specific topic. Telegram API
    /// types this as i32; we widen losslessly to i64 to match the
    /// `routing_thread_id INTEGER` column type in agent_registry
    /// (Slice 1 schema migration). When `None`, the routing key resolves
    /// to `(chat_id, NULL)` — the DM / topic-less group route.
    #[serde(default)]
    pub message_thread_id: Option<i64>,
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

/// Slice 4b of multi-agent-telegram-on-v0.6 — bot commands handled
/// server-side by `process_batch_with_pairing` BEFORE the
/// routing-key-extraction block (Slice 2). When a paired inbound's
/// text parses as a `BotCommand`, the dispatch produces a reply text
/// pushed onto the `pair_replies` queue (topic-aware via the
/// `(chat_id, thread_id, text)` tuple per Slice 4b widening) and the
/// iteration skips emit-channel-notification — operators never see
/// `/whoami` echoed into their CLI session.
///
/// Slice 4b ships only the READ-ONLY variants: `/whoami` (shows
/// current binding) and `/agents` (lists CLIs on this routing key).
/// Mutating commands `/switch` (rebind) and `/here` (host/cwd/pid
/// reveal) land in Slices 4c and 4d together with their security gate
/// + concurrency tests.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum BotCommand {
    Whoami,
    Agents,
    /// Slice 4c — `/switch <agent_name>` rebinds the routing key
    /// `(chat_id, message_thread_id)` to the named CLI. String arg is
    /// the operator-typed CLI name; resolved case-insensitively against
    /// `agent_registry.agent_name` of alive rows (Slice 7 tiebreak: when
    /// multiple alive rows share a name, the most-recently-spawned wins).
    Switch(String),
    /// Slice 12 — `/start` emits a two-button inline keyboard `[agents, switch]`.
    /// State-free: callback_data is `start:agents` / `start:switch` (no
    /// `pending_asks` row needed). The `switch` tap emits a SECOND keyboard
    /// with one button per alive CLI (callback `startswitch:<agent_name>`),
    /// rebuilt at-tap-time from `agent_registry::list_alive`.
    Start,
}

/// Parse `text` as a Telegram bot command. Returns Some when `text`
/// matches `/whoami`, `/agents`, `/whoami@botname`, or `/agents@botname`
/// (group-mention suffix per Telegram convention). Lowercase only —
/// `/WhoAmI` is intentionally NOT a hit, matches the official plugin's
/// case-sensitive dispatch.
pub(crate) fn parse_bot_command(text: &str) -> Option<BotCommand> {
    let trimmed = text.trim();
    let after_slash = trimmed.strip_prefix('/')?;
    let mut parts = after_slash.split_whitespace();
    let cmd_raw = parts.next()?;
    // Telegram permits `/cmd@botname` in groups to disambiguate when
    // multiple bots share a chat. Strip the suffix BEFORE matching.
    let cmd = cmd_raw.split('@').next().unwrap_or(cmd_raw);
    match cmd {
        "whoami" => Some(BotCommand::Whoami),
        "agents" => Some(BotCommand::Agents),
        "switch" => {
            // /switch requires exactly one positional arg — the target
            // CLI name. Missing arg returns None so the operator gets
            // no reply (the user-facing help text /agents lists options).
            let arg = parts.next()?;
            Some(BotCommand::Switch(arg.to_string()))
        }
        // Slice 12 — /start emits an inline-keyboard menu. Any positional
        // args are ignored (operators may type /start@botname in groups).
        "start" => Some(BotCommand::Start),
        _ => None,
    }
}

/// Slice 4b — `/whoami` handler. Returns the user-facing reply text
/// describing the CLI bound to `(chat_id, thread_id)`, or a helpful
/// "no binding" hint when none is registered.
pub(crate) fn handle_whoami(
    conn: &rusqlite::Connection,
    chat_id: i64,
    thread_id: Option<i64>,
) -> anyhow::Result<String> {
    let rows = crate::daemon::agent_registry::list_routings_for(conn, chat_id, thread_id)?;
    if rows.is_empty() {
        Ok("No CLI is bound to this chat/topic. Use /switch <name> to bind one.".to_string())
    } else {
        // 1-CLI-per-key invariant — rows always has exactly one entry
        // when non-empty (the Slice 1 partial-UNIQUE index guarantees it).
        let agent = &rows[0];
        Ok(format!(
            "Bound CLI: {} (agent_id={}, last_pinged_at={})",
            agent.agent_name, agent.agent_id, agent.last_pinged_at
        ))
    }
}

/// Slice 4c — `/switch <name>` handler. Rebinds the routing key
/// `(chat_id, thread_id)` to the named CLI subject to the FR-MAT-8.6
/// security gate.
///
/// **Security gate (FR-MAT-8.6, partial — chat-admin fallback deferred):**
///   - If NO binding currently exists on (chat_id, thread_id): ALLOWED
///     (first claim of an unowned routing key).
///   - If a binding exists AND its `last_user_id` equals the requesting
///     user_id: ALLOWED (the operator who last used the bound CLI may
///     rebind).
///   - Otherwise: DENIED. (The chat-admin fallback via
///     `bot.get_chat_administrators(chat_id)` requires async context and
///     is deferred to a follow-up sub-slice; for now group operators
///     without prior `last_user_id` will hit the deny branch.)
///
/// Bind itself runs via `agent_registry::bind_routing_key_in_tx`
/// inside the caller's transaction — within a single Update batch,
/// /switch taps serialize naturally via sequential processing, so the
/// nested BEGIN IMMEDIATE pattern of bind_routing_key is not needed
/// (and would deadlock against the parent tx anyway).
pub(crate) fn handle_switch(
    tx: &rusqlite::Transaction,
    chat_id: i64,
    thread_id: Option<i64>,
    sender_user_id: i64,
    target_name: &str,
) -> anyhow::Result<String> {
    use crate::daemon::agent_registry;

    // 1. Find the target CLI by name (case-insensitive). Multiple alive
    //    rows with the same name → tiebreak by spawned_at DESC per the
    //    Slice 7 @-mention convention (most recently spawned wins).
    let alive = agent_registry::list_alive(tx, None)?;
    let target_name_lower = target_name.to_ascii_lowercase();
    let target = alive
        .into_iter()
        .filter(|r| r.agent_name.to_ascii_lowercase() == target_name_lower)
        .max_by_key(|r| r.spawned_at);
    let target = match target {
        Some(t) => t,
        None => {
            return Ok(format!(
                "CLI '{target_name}' not found among alive CLIs. Use /agents to list bound CLIs in this chat/topic."
            ));
        }
    };

    // 2. Security gate.
    let existing = agent_registry::list_routings_for(tx, chat_id, thread_id)?;
    if !existing.is_empty() {
        let current_agent_id = &existing[0].agent_id;
        let current_agent_name = &existing[0].agent_name;
        // Same-agent rebind is a no-op — allowed regardless of who
        // requested it. Closes the surprising "operator typed /switch
        // own-agent-name and got denied" case.
        if current_agent_id == &target.agent_id {
            return Ok(format!(
                "Already bound to {} — no change.",
                target.agent_name
            ));
        }
        let last_user_id: Option<i64> = tx
            .query_row(
                "SELECT last_user_id FROM agent_registry WHERE agent_id = ?1",
                params![current_agent_id],
                |r| r.get(0),
            )
            .ok()
            .flatten();
        match last_user_id {
            Some(lid) if lid == sender_user_id => { /* authorized */ }
            _ => {
                return Ok(format!(
                    "Denied: only the user who last messaged this chat/topic via the bound CLI ({current_agent_name}) may /switch. Have them rebind, or wait for chat-admin fallback (deferred sub-slice)."
                ));
            }
        }
    }

    // 3. Atomic rebind inside the caller's tx.
    agent_registry::bind_routing_key_in_tx(tx, &target.agent_id, chat_id, thread_id)?;
    Ok(format!(
        "Switched: this chat/topic is now bound to {} (agent_id={}). Subsequent inbound messages route to that CLI.",
        target.agent_name, target.agent_id
    ))
}

/// `/agents` handler. Lists ALL alive CLIs registered in the daemon,
/// regardless of routing-key binding to this chat (operator clarification
/// 2026-06-04: "claudebase run должен регистрировать всех CLI в демоне,
/// /agents должна показывать список всех зарегистрированных в демоне
/// агентов; /switch остаётся выбором адресата сообщений").
///
/// Each row is annotated with `(current)` when its agent_id matches the
/// routing-key binding for the requesting `(chat_id, thread_id)` so the
/// operator sees at a glance who is currently selected as the addressee
/// for THIS chat. The binding-lookup is a single `list_routings_for`
/// call against the same routing key the `/switch` handler uses, so the
/// marker stays in lock-step with whatever `/switch` last persisted.
pub(crate) fn handle_agents(
    conn: &rusqlite::Connection,
    chat_id: i64,
    thread_id: Option<i64>,
) -> anyhow::Result<String> {
    let alive = crate::daemon::agent_registry::list_alive(conn, None)?;
    if alive.is_empty() {
        return Ok("No CLIs registered in the daemon.".to_string());
    }
    let bound_agent_id: Option<String> =
        crate::daemon::agent_registry::list_routings_for(conn, chat_id, thread_id)?
            .into_iter()
            .next()
            .map(|r| r.agent_id);
    let mut lines = Vec::with_capacity(alive.len() + 1);
    lines.push("Registered CLIs (alive):".to_string());
    for r in alive {
        let marker = match bound_agent_id.as_deref() {
            Some(b) if b == r.agent_id => " (current)",
            _ => "",
        };
        lines.push(format!(
            "- {} (agent_id={}, last_pinged_at={}){}",
            r.agent_name, r.agent_id, r.last_pinged_at, marker
        ));
    }
    Ok(lines.join("\n"))
}

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
    /// Slice 4b of multi-agent-telegram-on-v0.6 widened the tuple to
    /// `(chat_id, thread_id, text)` so bot-command replies (`/whoami`,
    /// `/agents`, etc.) land in the same forum topic the operator
    /// queried from. The original pair-code replies (server.ts:910-915
    /// origin) push `None` for thread_id — pairing happens before any
    /// topic UX is established.
    pub pair_replies: Vec<(i64, Option<i64>, String)>,
    /// True when the gate code mutated `channel_state::Access.pending`
    /// (a new code was issued OR a `replies` counter incremented). The
    /// async caller MUST save access.json when set; otherwise the next
    /// inbound DM from the same sender re-issues a different code.
    pub access_dirty: bool,
    /// Slice 8 — list of CallbackQuery ids to acknowledge via
    /// `Bot::answer_callback_query`. The async long-poll caller fires
    /// these AFTER the batch transaction commits — within Telegram's
    /// ~15s deadline (architect AR-1). Acknowledgement carries no body
    /// (the response to the operator's tap is the separate `<channel>`
    /// event built by `build_channel_notification_callback_response`).
    pub callback_acks: Vec<String>,
    /// Slice 8b — inline-keyboard redraw requests pending
    /// `Bot::edit_message_reply_markup`. The async caller iterates,
    /// builds the new `InlineKeyboardMarkup` with ✓ markers on
    /// `selected_values` + a final "Done" button, and dispatches.
    /// 429 retry-once with `retry_after`; second failure logged-and-
    /// swallowed (SQLite state stays correct, UI lags until next tap).
    pub keyboard_edits: Vec<KeyboardEdit>,
}

/// Slice 8b — one pending inline-keyboard redraw. Built inside the
/// batch transaction so the multi-select state machine reads
/// `selected_values_json` atomically via `update_selected_values
/// RETURNING`; the async caller dispatches `Bot::edit_message_reply_
/// markup` outside the transaction.
#[derive(Debug)]
pub struct KeyboardEdit {
    pub chat_id: i64,
    pub message_id: i64,
    /// `(label, callback_data, is_selected)` triples. The async caller
    /// renders selected entries as `"✓ <label>"`; unselected as
    /// `"<label>"`. callback_data is the EXACT toggle string the
    /// `:toggle:` parser expects — so the round-trip stays
    /// idempotent.
    pub buttons: Vec<(String, String, bool)>,
    /// Final "Done" button — its callback_data is `<ask_id>:done`.
    pub done_callback_data: String,
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

/// Process one batch with full official-telegram-plugin gating semantics
/// (channel_state::Access — DmPolicy{Pairing,Allowlist,Disabled}, pending
/// codes, replies counter, format_pair_reply). Mirrors server.ts:900-916
/// for the per-update gate decision. All chat-message inserts AND the
/// offset-advance UPDATE are wrapped in ONE rusqlite transaction so
/// either every row makes it OR none of them do (SEC-13).
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
            callback_acks: Vec::new(),
            keyboard_edits: Vec::new(),
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
    let mut pair_replies: Vec<(i64, Option<i64>, String)> = Vec::new();
    let mut callback_acks: Vec<String> = Vec::new();
    let mut keyboard_edits: Vec<KeyboardEdit> = Vec::new();

    for update in batch {
        if update.update_id > max_id {
            max_id = update.update_id;
        }

        // Slice 8 — CallbackQuery branch. Telegram delivers CallbackQuery
        // and Message in mutually-exclusive Updates; we handle the former
        // FIRST so the message-flow below stays untouched on plain DMs.
        if let Some(cb) = &update.callback_query {
            let sender_id_str = cb.from.id.to_string();
            if !channel_state::gate_callback(access, &sender_id_str) {
                // AR-3: drop silently. No `answerCallbackQuery` (don't
                // acknowledge unknown senders), no pairing code.
                tracing::info!(
                    user_id = cb.from.id,
                    "callback from non-allowed user_id; dropping"
                );
                continue;
            }
            // AR-1: schedule answerCallbackQuery (cheap, no body).
            // Fired AFTER tx.commit() by the async caller so the
            // operator's spinner clears within Telegram's ~15s deadline.
            callback_acks.push(cb.id.clone());

            let data = match cb.data.as_deref() {
                Some(s) if !s.is_empty() => s,
                _ => {
                    tracing::warn!(callback_id = %cb.id, "callback missing data; dropping");
                    continue;
                }
            };
            // Slice 12 — `/start` menu callback prefixes are state-free
            // (no pending_asks row). They are handled inline here BEFORE
            // the chat_ask `<ask_id>:<value>` discriminator.
            //
            //   `start:agents`           → emit list_alive bullet text via
            //                              pair_replies
            //   `start:switch`           → emit a SECOND keyboard with one
            //                              option per alive CLI (callback
            //                              `startswitch:<agent_name>`)
            //   `startswitch:<name>`     → call handle_switch with the chosen
            //                              CLI name (same security gate as
            //                              typed /switch)
            //
            // No pending_asks insert / lookup / delete — the state lives
            // entirely in callback_data strings rebuilt at tap-time.
            if let Some(start_suffix) = data.strip_prefix("start:") {
                let cb_chat_id = match cb.message.as_ref().map(|m| m.chat.id) {
                    Some(c) => c,
                    None => {
                        tracing::warn!(callback_id = %cb.id, "callback missing message.chat.id; dropping");
                        continue;
                    }
                };
                match start_suffix {
                    "agents" => {
                        match handle_agents(&tx, cb_chat_id, None) {
                            Ok(text) => pair_replies.push((cb_chat_id, None, text)),
                            Err(e) => tracing::warn!(
                                error = %e,
                                "/start:agents handler failed"
                            ),
                        }
                    }
                    "switch" => {
                        // Build a fresh keyboard from list_alive at tap-time
                        // (operator spec 2026-06-04: "AT-TAP-TIME, not cached").
                        let alive = match crate::daemon::agent_registry::list_alive(&tx, None) {
                            Ok(rows) => rows,
                            Err(e) => {
                                tracing::warn!(error = %e, "/start:switch list_alive failed");
                                continue;
                            }
                        };
                        if alive.is_empty() {
                            pair_replies.push((
                                cb_chat_id,
                                None,
                                "No CLIs alive — try /agents later.".to_string(),
                            ));
                        } else {
                            let options: Vec<(String, String)> = alive
                                .iter()
                                .map(|row| {
                                    (
                                        row.agent_name.clone(),
                                        format!("startswitch:{}", row.agent_name),
                                    )
                                })
                                .collect();
                            match enqueue_outbound_tg_keyboard(
                                cb_chat_id,
                                None,
                                "Switch to:".to_string(),
                                options,
                            ) {
                                Ok(_ack_rx) => tracing::debug!(
                                    chat_id = cb_chat_id,
                                    "/start:switch keyboard enqueued"
                                ),
                                Err(e) => tracing::warn!(
                                    error = %e,
                                    "/start:switch enqueue failed"
                                ),
                            }
                        }
                    }
                    other => tracing::warn!(
                        data = %data,
                        suffix = %other,
                        "unrecognised /start suffix; dropping"
                    ),
                }
                callback_acks.push(cb.id.clone());
                continue;
            }
            if let Some(agent_name) = data.strip_prefix("startswitch:") {
                let cb_chat_id = match cb.message.as_ref().map(|m| m.chat.id) {
                    Some(c) => c,
                    None => {
                        tracing::warn!(callback_id = %cb.id, "callback missing message.chat.id; dropping");
                        continue;
                    }
                };
                // Use existing FR-MAT-8.6 security gate via handle_switch.
                // user_id comes from cb.from.id (tapping operator).
                match handle_switch(&tx, cb_chat_id, None, cb.from.id, agent_name) {
                    Ok(text) => pair_replies.push((cb_chat_id, None, text)),
                    Err(e) => tracing::warn!(
                        error = %e,
                        agent_name = %agent_name,
                        "/startswitch handler failed"
                    ),
                }
                callback_acks.push(cb.id.clone());
                continue;
            }

            // Slice 8 callback_data discriminator:
            //   single-select: `<ask_id>:<value>`
            //   multi-select : `<ask_id>:toggle:<value>` OR `<ask_id>:done`
            // We split AT MOST ONCE so the value (single) / suffix (multi)
            // can contain its own `:` if it ever needs to.
            let Some((ask_id, suffix)) = data.split_once(':') else {
                tracing::warn!(
                    callback_id = %cb.id,
                    data = %data,
                    "callback data has no ':' separator; dropping"
                );
                continue;
            };

            let ask_row = crate::daemon::pending_asks::get_pending(&tx, ask_id)?;
            let Some(ask) = ask_row else {
                tracing::warn!(
                    ask_id = %ask_id,
                    "callback for unknown ask_id (expired, GC'd, or response-injection attempt); dropping"
                );
                continue;
            };

            // AR-4 alive-check is shared by single + multi resolution.
            let alive_of = |agent_id: &str| -> Result<bool> {
                use rusqlite::OptionalExtension;
                Ok(tx
                    .query_row(
                        "SELECT 1 FROM agent_registry WHERE agent_id = ?1 AND state = 'alive' LIMIT 1",
                        rusqlite::params![agent_id],
                        |_| Ok(true),
                    )
                    .optional()?
                    .unwrap_or(false))
            };

            // Slice 8b live-fix: CC's channel-surface renderer requires the
            // v0.6 frozen-contract meta keys (chat_id/message_id/user/user_id/
            // ts as strings) — without them the frame is silently dropped
            // before reaching the LLM. Construct once; reuse across single
            // and multi-Done branches. message_id prefers cb.message (the
            // tap originated on a specific bot message), falls back to
            // ask.message_id (the original keyboard message we stored at
            // insert time). ts is server-side now() — CallbackQuery payloads
            // do not carry a date field.
            let cb_user_id = cb.from.id;
            let cb_user_display = cb
                .from
                .username
                .clone()
                .unwrap_or_else(|| cb_user_id.to_string());
            let cb_message_id = cb
                .message
                .as_ref()
                .map(|m| m.message_id)
                .unwrap_or(ask.message_id);
            let ts_iso_now = {
                use chrono::{SecondsFormat, Utc};
                Utc::now().to_rfc3339_opts(SecondsFormat::Millis, true)
            };
            let cb_tg_meta = chat::TelegramMessageMeta {
                chat_id: ask.chat_id,
                message_id_str: cb_message_id.to_string(),
                user: cb_user_display,
                user_id: cb_user_id.to_string(),
                ts_iso8601: ts_iso_now,
                thread_id: ask.message_thread_id,
            };

            if !ask.multi {
                // Slice 8a — single-select: any tap finalizes.
                let value = suffix;
                let is_alive = alive_of(&ask.requesting_agent_id)?;
                let frame = chat::build_channel_notification_callback_response(
                    &ask.ask_id,
                    chat::CallbackAnswer::Single(value),
                    &ask.requesting_agent_id,
                    is_alive,
                    &ask.question,
                    &ask.options_json,
                    false,
                    &cb_tg_meta,
                );
                let thread = format!("telegram:{}", ask.chat_id);
                notifications.push((thread, frame));
                crate::daemon::pending_asks::delete_pending(&tx, ask_id)?;
                continue;
            }

            // Slice 8b — multi-select state machine.
            if suffix == "done" {
                // Finalize: read current selected_values_json (None = empty),
                // emit channel-response with values array, delete row.
                let values: Vec<String> = match &ask.selected_values_json {
                    None => Vec::new(),
                    Some(s) => serde_json::from_str(s).unwrap_or_default(),
                };
                let is_alive = alive_of(&ask.requesting_agent_id)?;
                let frame = chat::build_channel_notification_callback_response(
                    &ask.ask_id,
                    chat::CallbackAnswer::Multi(&values),
                    &ask.requesting_agent_id,
                    is_alive,
                    &ask.question,
                    &ask.options_json,
                    true,
                    &cb_tg_meta,
                );
                let thread = format!("telegram:{}", ask.chat_id);
                notifications.push((thread, frame));
                crate::daemon::pending_asks::delete_pending(&tx, ask_id)?;
                continue;
            }

            // suffix must be `toggle:<value>`; anything else is malformed.
            let Some(toggled_value) = suffix.strip_prefix("toggle:") else {
                tracing::warn!(
                    callback_id = %cb.id,
                    data = %data,
                    "multi-select callback suffix not recognized; dropping"
                );
                continue;
            };

            // Mutate selected_values: toggle the tapped value on/off.
            let mut current: Vec<String> = match &ask.selected_values_json {
                None => Vec::new(),
                Some(s) => serde_json::from_str(s).unwrap_or_default(),
            };
            if let Some(pos) = current.iter().position(|v| v == toggled_value) {
                current.remove(pos);
            } else {
                current.push(toggled_value.to_string());
            }
            let new_json = serde_json::to_string(&current)
                .unwrap_or_else(|_| "[]".to_string());

            // AR-7 atomic RETURNING — value the row holds AFTER the write.
            let returned = crate::daemon::pending_asks::update_selected_values(
                &tx,
                ask_id,
                &new_json,
            )?;
            // If RETURNING reported NULL the row vanished mid-flight (rare:
            // concurrent /switch tap raced GC) — give up on this callback.
            let Some(_post_state) = returned else {
                tracing::warn!(
                    ask_id = %ask_id,
                    "update_selected_values returned None — row missing; dropping"
                );
                continue;
            };

            // Build keyboard-edit payload for the async edit_message_reply_markup
            // call. We rebuild the WHOLE keyboard so the ✓ marker on
            // every option mirrors the post-write selected_values set.
            let options: Vec<serde_json::Value> =
                serde_json::from_str(&ask.options_json).unwrap_or_default();
            let mut buttons: Vec<(String, String, bool)> =
                Vec::with_capacity(options.len());
            for opt in &options {
                let label = opt
                    .get("label")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string();
                let value = opt
                    .get("value")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string();
                let is_selected = current.iter().any(|v| v == &value);
                let cb_data = format!("{}:toggle:{}", ask.ask_id, value);
                buttons.push((label, cb_data, is_selected));
            }
            keyboard_edits.push(KeyboardEdit {
                chat_id: ask.chat_id,
                message_id: ask.message_id,
                buttons,
                done_callback_data: format!("{}:done", ask.ask_id),
            });
            continue;
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
                // Pair reply has no topic context — operator's first
                // contact pre-binding. Always None for thread_id.
                pair_replies.push((chat_id, None, text));
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

        // Slice 4b of multi-agent-telegram-on-v0.6 — bot-command dispatch
        // BEFORE the Slice 2 routing-extraction block. Commands handled
        // server-side: reply goes onto pair_replies (sent via bot
        // send_message in run_long_poll); iteration `continue`s so the
        // command never emits a channel notification to the bridge.
        //
        // Commands DO get persisted in chat.db (the INSERT above already
        // ran) — operators can still inspect what bot commands were sent
        // via `chat_list_threads`. Only the bridge-broadcast side is
        // skipped.
        if let Some(cmd) = parse_bot_command(&content) {
            // Slice 12 — `/start` is dispatched separately from the
            // text-reply commands because it sends an inline keyboard via
            // OUTBOUND_TG_KEYBOARD (not a text reply via pair_replies).
            // Fire-and-forget: we ignore the oneshot receiver since the
            // /start menu is state-free (callback_data prefix encodes the
            // choice; no pending_asks row needed).
            if let BotCommand::Start = cmd {
                let options = vec![
                    ("agents".to_string(), "start:agents".to_string()),
                    ("switch".to_string(), "start:switch".to_string()),
                ];
                match enqueue_outbound_tg_keyboard(
                    chat_id,
                    msg.message_thread_id,
                    "Choose:".to_string(),
                    options,
                ) {
                    Ok(_ack_rx) => {
                        // Drop the receiver — we don't track message_id for
                        // state-free /start (no pending_asks insert).
                        tracing::debug!(
                            chat_id,
                            thread_id = ?msg.message_thread_id,
                            "/start menu keyboard enqueued"
                        );
                    }
                    Err(e) => tracing::warn!(
                        chat_id,
                        error = %e,
                        "/start keyboard enqueue failed"
                    ),
                }
                continue;
            }
            let reply = match cmd {
                BotCommand::Whoami => handle_whoami(&tx, chat_id, msg.message_thread_id),
                BotCommand::Agents => handle_agents(&tx, chat_id, msg.message_thread_id),
                BotCommand::Switch(name) => {
                    handle_switch(&tx, chat_id, msg.message_thread_id, user_id, &name)
                }
                BotCommand::Start => unreachable!("handled above"),
            };
            match reply {
                Ok(text) => {
                    // pair_replies tuple Slice 4b shape: (chat_id, thread_id, text).
                    // thread_id from the INBOUND so the bot reply lands in
                    // the same forum topic the operator queried from.
                    pair_replies.push((chat_id, msg.message_thread_id, text));
                }
                Err(e) => tracing::warn!(
                    chat_id,
                    thread_id = ?msg.message_thread_id,
                    error = %e,
                    "bot-command handler failed; no reply sent"
                ),
            }
            continue;
        }

        // Slice 2 of multi-agent-telegram-on-v0.6 — routing-key binding
        // lookup. Resolves (chat_id, message_thread_id) against the
        // agent_registry partial-UNIQUE index added in Slice 1. Wins over
        // @-mention so explicit operator bindings (Slice 5 `/switch`)
        // take precedence over text-based @mention parsing.
        let routed_target: Option<String> = crate::daemon::agent_registry::resolve_routing(
            &tx,
            chat_id,
            msg.message_thread_id,
        )
        .unwrap_or(None);

        // Slice 4b — `last_user_id` stamping. When the inbound routes to
        // a registered CLI, refresh that binding's last_user_id so the
        // Slice 4c `/switch` security gate (FR-MAT-8.6) has the right
        // authorization signal. Failure is logged but non-fatal — the
        // notification still goes out; only authorization data becomes
        // stale.
        if let Some(ref agent_id) = routed_target {
            if let Err(e) =
                crate::daemon::agent_registry::stamp_last_user_id(&tx, agent_id, user_id)
            {
                tracing::warn!(
                    agent_id,
                    user_id,
                    error = %e,
                    "stamp_last_user_id failed; /switch security gate will fall back to chat-admin check"
                );
            }
        }

        // Slice 7 — @-mention routing fallback (preserved bit-for-bit
        // from the v0.6 baseline; runs only when no explicit routing
        // binding exists).
        let mention_target: Option<String> = if let Some(mention) = extract_first_mention(&content)
        {
            let alive = crate::daemon::agent_registry::list_alive(&tx, Some(&thread_id))
                .unwrap_or_default();
            let mention_lower = mention.to_ascii_lowercase();
            let target = alive
                .into_iter()
                .filter(|r| r.agent_name.to_ascii_lowercase() == mention_lower)
                .max_by_key(|r| r.spawned_at);
            target.map(|row| row.agent_id)
        } else {
            None
        };

        // Routing-key binding wins; @-mention is the fallback; absence
        // of both yields None (the inbound becomes broadcast-to-all
        // subscribers — Slice 0 baseline behavior).
        let target_agent_id: Option<String> = routed_target.or(mention_target);

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
            // Slice 2 additive: when None, build_channel_notification_telegram
            // OMITS the meta.thread_id field so DM / topic-less group inbound
            // preserves Slice 0 baseline meta shape bit-for-bit.
            thread_id: msg.message_thread_id,
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
        callback_acks,
        keyboard_edits,
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
    bus: SharedBus,
    asr: Option<Arc<dyn Asr>>,
) -> tokio::task::JoinHandle<()> {
    // Initialise the outbound bridge BEFORE spawning so server.rs's MCP
    // chat_reply handler can enqueue immediately (race-free: any push
    // before the spawn is queued; the receiver picks it up on the first
    // select! tick).
    let (outbound_tx, outbound_rx) =
        mpsc::unbounded_channel::<(i64, Option<i64>, String, Option<String>)>();
    if OUTBOUND_TG.set(outbound_tx).is_err() {
        tracing::warn!(
            "OUTBOUND_TG already initialised — second spawn_long_poll call ignored (daemon should spawn only once per process)"
        );
    }
    // Slice 8 — parallel channel for `chat_ask` keyboard outbounds.
    let (kb_tx, kb_rx) = mpsc::unbounded_channel::<KeyboardOutbound>();
    if OUTBOUND_TG_KEYBOARD.set(kb_tx).is_err() {
        tracing::warn!(
            "OUTBOUND_TG_KEYBOARD already initialised — second spawn_long_poll call ignored"
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

    // Guaranteed-delivery 2026-06-05: drain chat.db for undelivered TG
    // outbound from the last 24h and re-enqueue. Runs BEFORE the send-loop
    // spawn so the first poll iteration already picks up the recovered
    // messages. Errors logged-and-swallowed — chat.db access failure must
    // NOT prevent daemon startup.
    match drain_pending_outbound_tg() {
        Ok(count) if count > 0 => tracing::info!(
            count,
            "drained pending TG outbound from chat.db on daemon startup"
        ),
        Ok(_) => {}
        Err(e) => tracing::warn!(
            error = %e,
            "drain_pending_outbound_tg failed — startup proceeds; new outbound still works"
        ),
    }

    tokio::spawn(async move {
        // ASYNC_INVARIANTS Rule 3 — wrap the long-poll body so any
        // unhandled error logs structured (without leaking the token) and
        // the daemon's other tasks keep running.
        let token_str = token.as_str().to_string();
        if let Err(e) = run_long_poll(token, bus, asr, outbound_rx, kb_rx).await {
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
    bus: SharedBus,
    asr: Option<Arc<dyn Asr>>,
    mut outbound_rx: mpsc::UnboundedReceiver<(i64, Option<i64>, String, Option<String>)>,
    mut kb_rx: mpsc::UnboundedReceiver<KeyboardOutbound>,
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

    // 1:1 port of the official Anthropic telegram plugin. The
    // skill-managed channel state lives at the path documented in
    // `src/daemon/channel_state.rs` (`~/.claude/channels/claudebase/`).
    // The legacy `~/.config/claudebase/` permissions module and the
    // `claudebase daemon access pair/list` CLI shims that wrote to it
    // were removed in Slice 5 (commits b507434 + this commit) — see
    // architect verdict at `.claude/scratchpad.md`.
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
                    callback_acks: Vec::new(),
                    keyboard_edits: Vec::new(),
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
                Ok((chat_id, thread_id, text, msg_db_id)) => {
                    // Slice 3 of multi-agent-telegram-on-v0.6: when
                    // thread_id is Some, apply teloxide's
                    // SendMessageSetters::message_thread_id setter so the
                    // outbound lands in the inbound's forum topic
                    // (KP2/KP3). When None, the message goes to the main
                    // chat / DM thread (KP1 / Slice 0 baseline).
                    //
                    // teloxide-core's ThreadId wraps MessageId which wraps
                    // i32 — Telegram thread_ids are positive (derived
                    // from positive message_ids) so the i64->i32 narrowing
                    // is safe in practice; we use `as i32` per architect
                    // A5 (CHECK constraint in agent_registry already
                    // rejects non-positive thread_ids at INSERT time).
                    use teloxide::payloads::SendMessageSetters;
                    use teloxide::requests::Requester;
                    use teloxide::types::{ChatId, MessageId, ThreadId};

                    let send_payload = bot.send_message(ChatId(chat_id), &text);
                    let send_result = match thread_id {
                        Some(tid) => send_payload
                            .message_thread_id(ThreadId(MessageId(tid as i32)))
                            .await,
                        None => send_payload.await,
                    };
                    match send_result {
                        Ok(_) => {
                            tracing::info!(
                                chat_id,
                                thread_id = ?thread_id,
                                bytes = text.len(),
                                msg_db_id = ?msg_db_id,
                                "telegram outbound sent"
                            );
                            // Guaranteed-delivery 2026-06-05: mark the chat.db
                            // row delivered so the daemon-restart spool drain
                            // does NOT re-enqueue an already-sent message.
                            if let Some(id) = msg_db_id {
                                let now_ms = chat::now_millis();
                                let _ = tokio::task::spawn_blocking(move || -> anyhow::Result<()> {
                                    let conn = chat::open_chat_db()?;
                                    conn.execute(
                                        "UPDATE chat_messages SET delivered_at = ?1 WHERE id = ?2",
                                        rusqlite::params![now_ms, id],
                                    )?;
                                    Ok(())
                                }).await;
                            }
                        }
                        Err(e) => tracing::warn!(
                            chat_id,
                            thread_id = ?thread_id,
                            error = %redact_error_string(&format!("{e}"), &token_for_error_redaction),
                            "telegram outbound send failed (chat.db row stays delivered_at=NULL — startup spool will re-enqueue)"
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

        // Slice 8 — drain `OUTBOUND_TG_KEYBOARD` parallel to OUTBOUND_TG.
        // Same 16-per-iteration budget so neither path starves the other.
        // Per AR-1: SAME teloxide `bot` instance owns the HTTP client
        // for both plain `chat_reply` and `chat_ask` keyboard outbounds.
        for _ in 0..16 {
            match kb_rx.try_recv() {
                Ok(payload) => {
                    use teloxide::payloads::SendMessageSetters;
                    use teloxide::requests::Requester;
                    use teloxide::types::{
                        ChatId, InlineKeyboardButton, InlineKeyboardMarkup, MessageId, ThreadId,
                    };
                    let KeyboardOutbound {
                        chat_id,
                        thread_id,
                        text,
                        options,
                        ack,
                    } = payload;
                    // One row per option (Slice 8a single-select). Slice 8b
                    // multi-select uses the same build helper — toggle/done
                    // buttons composed by the chat_ask handler beforehand.
                    let rows: Vec<Vec<InlineKeyboardButton>> = options
                        .iter()
                        .map(|(label, data)| {
                            vec![InlineKeyboardButton::callback(label.clone(), data.clone())]
                        })
                        .collect();
                    let kb = InlineKeyboardMarkup::new(rows);
                    let send_payload = bot.send_message(ChatId(chat_id), &text).reply_markup(kb);
                    let send_result = match thread_id {
                        Some(tid) => send_payload
                            .message_thread_id(ThreadId(MessageId(tid as i32)))
                            .await,
                        None => send_payload.await,
                    };
                    match send_result {
                        Ok(msg) => {
                            let mid: i64 = msg.id.0 as i64;
                            tracing::info!(
                                chat_id,
                                thread_id = ?thread_id,
                                message_id = mid,
                                "telegram chat_ask keyboard sent"
                            );
                            // Best-effort ack: if the receiver was dropped
                            // (e.g. chat_ask handler exited mid-flight),
                            // the row will not be INSERTed — the keyboard
                            // is operator-visible but un-dispatchable.
                            // That's acceptable for Slice 8a; future work
                            // can GC orphaned-keyboards by message_id but
                            // it's not on the critical path.
                            let _ = ack.send(Ok(mid));
                        }
                        Err(e) => {
                            let redacted = redact_error_string(
                                &format!("{e}"),
                                &token_for_error_redaction,
                            );
                            tracing::warn!(
                                chat_id,
                                thread_id = ?thread_id,
                                error = %redacted,
                                "telegram chat_ask keyboard send failed"
                            );
                            // Per AR-1: send failure → ack with Err so
                            // the chat_ask handler can return `-32603`
                            // to the bridge WITHOUT inserting an orphan.
                            let _ = ack.send(Err(anyhow::anyhow!("telegram send failed: {redacted}")));
                        }
                    }
                }
                Err(mpsc::error::TryRecvError::Empty) => break,
                Err(mpsc::error::TryRecvError::Disconnected) => {
                    tracing::warn!("keyboard outbound channel disconnected (no more chat_ask traffic)");
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

                // Send pair-action / bot-command replies via teloxide
                // (server.ts:910-915). Slice 4b: when thread_id is Some
                // the reply targets the forum topic the inbound came
                // from (KP2/KP3 round-trip for /whoami /agents in
                // topics). When None, reply to the main chat thread / DM.
                use teloxide::payloads::SendMessageSetters;
                use teloxide::types::{MessageId, ThreadId};
                for (chat_id, thread_id, text) in outcome.pair_replies {
                    let send_payload = bot
                        .send_message(teloxide::types::ChatId(chat_id), &text);
                    let send_result = match thread_id {
                        Some(tid) => {
                            send_payload
                                .message_thread_id(ThreadId(MessageId(tid as i32)))
                                .await
                        }
                        None => send_payload.await,
                    };
                    match send_result {
                        Ok(_) => tracing::info!(
                            chat_id,
                            thread_id = ?thread_id,
                            "telegram pair/bot reply sent"
                        ),
                        Err(e) => tracing::warn!(
                            chat_id,
                            thread_id = ?thread_id,
                            error = %redact_error_string(&format!("{e}"), &token_for_error_redaction),
                            "telegram pair/bot reply send failed"
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

                // Slice 8 — fire `answerCallbackQuery` for every
                // CallbackQuery the gate accepted (architect AR-1).
                // Cheap call, no body — clears the operator's spinner.
                // Failures here are logged-and-swallowed: the response
                // notification has already been broadcast, so the
                // semantic round-trip is complete; the spinner just
                // lingers until the operator dismisses it manually.
                for cb_id in outcome.callback_acks {
                    use teloxide::requests::Requester;
                    use teloxide::types::CallbackQueryId;
                    match bot.answer_callback_query(CallbackQueryId(cb_id.clone())).await {
                        Ok(_) => tracing::debug!(
                            callback_id = %cb_id,
                            "answerCallbackQuery sent"
                        ),
                        Err(e) => tracing::warn!(
                            callback_id = %cb_id,
                            error = %redact_error_string(&format!("{e}"), &token_for_error_redaction),
                            "answerCallbackQuery failed (spinner may linger)"
                        ),
                    }
                }

                // Slice 8b — drain keyboard_edits. The multi-select state
                // machine already wrote `selected_values_json` to SQLite
                // inside the batch transaction; here we redraw the inline
                // keyboard so the operator sees ✓ markers on selected
                // options. 429 → retry-once with 5s back-off (same shape
                // as getUpdates 429 handling); second failure logged and
                // swallowed because the persistent state is already
                // correct — UI is the only lag.
                for edit in outcome.keyboard_edits {
                    use teloxide::payloads::EditMessageReplyMarkupSetters;
                    use teloxide::requests::Requester;
                    use teloxide::types::{
                        ChatId, InlineKeyboardButton, InlineKeyboardMarkup, MessageId,
                    };
                    let mut rows: Vec<Vec<InlineKeyboardButton>> = edit
                        .buttons
                        .iter()
                        .map(|(label, data, is_selected)| {
                            let display = if *is_selected {
                                format!("✓ {}", label)
                            } else {
                                label.clone()
                            };
                            vec![InlineKeyboardButton::callback(display, data.clone())]
                        })
                        .collect();
                    rows.push(vec![InlineKeyboardButton::callback(
                        "Done".to_string(),
                        edit.done_callback_data.clone(),
                    )]);
                    let kb = InlineKeyboardMarkup::new(rows);
                    let chat = ChatId(edit.chat_id);
                    let mid = MessageId(edit.message_id as i32);
                    let mut attempt = 0;
                    loop {
                        attempt += 1;
                        let res = bot
                            .edit_message_reply_markup(chat, mid)
                            .reply_markup(kb.clone())
                            .await;
                        match res {
                            Ok(_) => {
                                tracing::debug!(
                                    chat_id = edit.chat_id,
                                    message_id = edit.message_id,
                                    "keyboard redraw sent"
                                );
                                break;
                            }
                            Err(e) => {
                                let err_str = format!("{e}");
                                let redacted = redact_error_string(
                                    &err_str,
                                    &token_for_error_redaction,
                                );
                                let is_429 = err_str.contains("429")
                                    || err_str.contains("RetryAfter");
                                if is_429 && attempt == 1 {
                                    tracing::warn!(
                                        chat_id = edit.chat_id,
                                        message_id = edit.message_id,
                                        error = %redacted,
                                        "keyboard redraw 429 — retrying once after 5s"
                                    );
                                    tokio::time::sleep(Duration::from_secs(5)).await;
                                    continue;
                                }
                                tracing::warn!(
                                    chat_id = edit.chat_id,
                                    message_id = edit.message_id,
                                    error = %redacted,
                                    "keyboard redraw failed (state is correct; UI may lag)"
                                );
                                break;
                            }
                        }
                    }
                }

                // Slice 8 — GC expired pending_asks at every batch tail
                // (AR-6 — once per long-poll cycle is cheap; the
                // expires_at index makes the DELETE O(log n)).
                let now_gc = crate::daemon::chat::now_millis();
                let gc_res = tokio::task::spawn_blocking(move || -> anyhow::Result<usize> {
                    let conn = crate::daemon::chat::open_chat_db()?;
                    crate::daemon::pending_asks::gc_expired(&conn, now_gc)
                })
                .await;
                match gc_res {
                    Ok(Ok(n)) if n > 0 => {
                        tracing::info!(removed = n, "pending_asks GC")
                    }
                    Ok(Ok(_)) => {}
                    Ok(Err(e)) => {
                        tracing::warn!(error = %e, "pending_asks GC failed")
                    }
                    Err(e) => {
                        tracing::warn!(error = %e, "pending_asks GC spawn_blocking panicked")
                    }
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
    fn set_bot_state_persists_into_daemon_state() {
        let conn = rusqlite::Connection::open_in_memory().unwrap();
        chat::ensure_chat_db_schema(&conn).unwrap();
        set_bot_state(&conn, "disconnected").unwrap();
        let v: String = conn
            .query_row("SELECT value FROM daemon_state WHERE key='tg_bot_state'", [], |r| r.get(0))
            .unwrap();
        assert_eq!(v, "disconnected");
    }

    // ---------------------------------------------------------------
    // Slice 2 of multi-agent-telegram-on-v0.6 — Message struct
    // deserialization of message_thread_id from real Telegram getUpdates
    // JSON shape. v0.6 baseline did NOT carry this field; Slice 2 adds
    // it (telegram.rs:Message). The test asserts both presence and
    // absence paths parse cleanly into Option<i64>.
    // ---------------------------------------------------------------

    #[test]
    fn slice2_message_deserializes_thread_id_from_forum_topic_inbound() {
        // Trimmed-to-essentials shape of a Telegram supergroup forum-topic
        // message. Real getUpdates payloads carry many more fields; serde
        // tolerates them via the absence of #[serde(deny_unknown_fields)].
        let json = serde_json::json!({
            "message_id": 100,
            "from": {"id": 42, "is_bot": false, "username": "alice"},
            "chat": {"id": 500, "type": "supergroup"},
            "date": 1780500000,
            "text": "hello topic α",
            "message_thread_id": 7,
        });
        let msg: Message = serde_json::from_value(json).expect("Message deserialization");
        assert_eq!(msg.message_thread_id, Some(7));
        assert_eq!(msg.message_id, 100);
        assert_eq!(msg.chat.id, 500);
    }

    #[test]
    fn slice2_message_thread_id_defaults_to_none_on_dm_inbound() {
        // A DM message has no message_thread_id field. serde(default)
        // gives us Option<i64>::None — the KP1 routing key.
        let json = serde_json::json!({
            "message_id": 200,
            "from": {"id": 42, "is_bot": false, "username": "alice"},
            "chat": {"id": 8791871989_i64, "type": "private"},
            "date": 1780500000,
            "text": "hi in DM",
        });
        let msg: Message = serde_json::from_value(json).expect("Message deserialization");
        assert_eq!(msg.message_thread_id, None);
    }

    // ---------------------------------------------------------------
    // Slice 4b of multi-agent-telegram-on-v0.6 —
    // parse_bot_command + handle_whoami + handle_agents tests.
    // ---------------------------------------------------------------

    #[test]
    fn slice4b_parse_whoami_plain() {
        assert_eq!(parse_bot_command("/whoami"), Some(BotCommand::Whoami));
    }

    #[test]
    fn slice4b_parse_whoami_with_bot_suffix() {
        // Telegram group `/cmd@botname` form.
        assert_eq!(
            parse_bot_command("/whoami@heymytechcclaude_bot"),
            Some(BotCommand::Whoami)
        );
    }

    #[test]
    fn slice4b_parse_agents_plain() {
        assert_eq!(parse_bot_command("/agents"), Some(BotCommand::Agents));
    }

    #[test]
    fn slice4b_parse_agents_with_trailing_args_ignored() {
        // Extra args after the command are not used by /agents (it
        // takes no args); parser still returns Agents.
        assert_eq!(parse_bot_command("/agents extra ignored"), Some(BotCommand::Agents));
    }

    #[test]
    fn slice4b_parse_rejects_non_slash() {
        assert_eq!(parse_bot_command("whoami"), None);
        assert_eq!(parse_bot_command(" /whoami"), Some(BotCommand::Whoami)); // trim leading WS
        assert_eq!(parse_bot_command(""), None);
    }

    #[test]
    fn slice4b_parse_is_case_sensitive() {
        // Telegram bot commands are case-sensitive by convention;
        // /WhoAmI does not match /whoami.
        assert_eq!(parse_bot_command("/WhoAmI"), None);
        assert_eq!(parse_bot_command("/AGENTS"), None);
    }

    #[test]
    fn slice4b_parse_rejects_unknown() {
        assert_eq!(parse_bot_command("/foo"), None);
        // Slice 12 — /start is now daemon-owned (emits inline keyboard menu).
        // v0.6 baseline left it in the plugin; v0.9 takes it over.
        assert_eq!(parse_bot_command("/start"), Some(BotCommand::Start));
        assert_eq!(parse_bot_command("/start@botname"), Some(BotCommand::Start));
        assert_eq!(parse_bot_command("/start ignored args"), Some(BotCommand::Start));
    }

    #[test]
    fn slice4b_whoami_no_binding_returns_hint() {
        let conn = rusqlite::Connection::open_in_memory().expect("conn");
        chat::ensure_chat_db_schema(&conn).expect("schema");
        let reply = handle_whoami(&conn, 8791871989, None).expect("whoami no-binding");
        assert!(reply.contains("No CLI"), "got: {reply}");
        assert!(reply.contains("/switch"), "should suggest /switch, got: {reply}");
    }

    #[test]
    fn slice4b_whoami_with_binding_names_agent() {
        let mut conn = rusqlite::Connection::open_in_memory().expect("conn");
        chat::ensure_chat_db_schema(&conn).expect("schema");
        // Manually seed an alive CLI bound to chat 100 / no thread (DM).
        conn.execute(
            "INSERT INTO agent_registry \
             (agent_id, agent_name, connection_id, chat_thread_id, spawned_at, last_pinged_at, state, routing_chat_id, routing_thread_id) \
             VALUES ('a-uuid', 'alice', 'conn-a', NULL, 1, 1, 'alive', 100, NULL)",
            [],
        )
        .unwrap();
        let _ = &mut conn; // silence unused-mut warning when we don't bind
        let reply = handle_whoami(&conn, 100, None).expect("whoami with-binding");
        assert!(reply.contains("alice"), "should name the agent, got: {reply}");
        assert!(reply.contains("a-uuid"), "should include agent_id, got: {reply}");
    }

    #[test]
    fn agents_lists_all_alive_clis_regardless_of_binding() {
        // Operator clarification 2026-06-04: /agents shows the full
        // alive-CLI roster in the daemon, not just CLIs bound to the
        // requesting chat. Two alive CLIs (bob bound to chat=500/topic=7,
        // carol bound to chat=500/topic=8); /agents in topic α (=7) must
        // list BOTH, marking bob as `(current)` because his binding
        // matches the requested routing key.
        let conn = rusqlite::Connection::open_in_memory().expect("conn");
        chat::ensure_chat_db_schema(&conn).expect("schema");
        conn.execute(
            "INSERT INTO agent_registry \
             (agent_id, agent_name, connection_id, chat_thread_id, spawned_at, last_pinged_at, state, routing_chat_id, routing_thread_id) \
             VALUES ('uuid-b', 'bob', 'conn-b', NULL, 1, 1, 'alive', 500, 7)",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO agent_registry \
             (agent_id, agent_name, connection_id, chat_thread_id, spawned_at, last_pinged_at, state, routing_chat_id, routing_thread_id) \
             VALUES ('uuid-c', 'carol', 'conn-c', NULL, 1, 1, 'alive', 500, 8)",
            [],
        )
        .unwrap();
        let reply_alpha = handle_agents(&conn, 500, Some(7)).expect("agents topic α");
        assert!(reply_alpha.contains("bob"), "must name bob, got: {reply_alpha}");
        assert!(reply_alpha.contains("carol"), "must also name carol, got: {reply_alpha}");
        // (current) marker only on bob (bound to this chat/topic).
        let bob_line = reply_alpha
            .lines()
            .find(|l| l.contains("bob"))
            .expect("bob line");
        assert!(
            bob_line.contains("(current)"),
            "bob line must carry (current) marker for topic α, got: {bob_line}"
        );
        let carol_line = reply_alpha
            .lines()
            .find(|l| l.contains("carol"))
            .expect("carol line");
        assert!(
            !carol_line.contains("(current)"),
            "carol line must NOT carry (current) marker in topic α, got: {carol_line}"
        );
        // Same call in topic β flips which row is marked current.
        let reply_beta = handle_agents(&conn, 500, Some(8)).expect("agents topic β");
        assert!(reply_beta.contains("bob"));
        assert!(reply_beta.contains("carol"));
        let carol_line_beta = reply_beta
            .lines()
            .find(|l| l.contains("carol"))
            .expect("carol line in β");
        assert!(carol_line_beta.contains("(current)"));
        let bob_line_beta = reply_beta
            .lines()
            .find(|l| l.contains("bob"))
            .expect("bob line in β");
        assert!(!bob_line_beta.contains("(current)"));
    }

    #[test]
    fn agents_empty_when_no_alive_clis_registered() {
        let conn = rusqlite::Connection::open_in_memory().expect("conn");
        chat::ensure_chat_db_schema(&conn).expect("schema");
        let reply = handle_agents(&conn, 999, None).expect("agents empty");
        assert!(reply.contains("No CLIs registered"), "got: {reply}");
    }

    #[test]
    fn agents_lists_unbound_alive_cli_without_current_marker() {
        // A registered CLI that no /switch has bound — must still appear
        // in /agents, just without the (current) marker for any chat.
        let conn = rusqlite::Connection::open_in_memory().expect("conn");
        chat::ensure_chat_db_schema(&conn).expect("schema");
        conn.execute(
            "INSERT INTO agent_registry \
             (agent_id, agent_name, connection_id, chat_thread_id, spawned_at, last_pinged_at, state, routing_chat_id, routing_thread_id) \
             VALUES ('uuid-mira', 'Mira', 'conn-mira', NULL, 1, 1, 'alive', NULL, NULL)",
            [],
        )
        .unwrap();
        let reply = handle_agents(&conn, 8791871989, None).expect("agents");
        assert!(reply.contains("Mira"), "must list Mira even with no binding, got: {reply}");
        assert!(
            !reply.lines().any(|l| l.contains("Mira") && l.contains("(current)")),
            "Mira must NOT carry (current) marker — she's unbound. Got: {reply}"
        );
    }

    // ---------------------------------------------------------------
    // Slice 4c of multi-agent-telegram-on-v0.6 —
    // parse `/switch` + handle_switch (security gate + bind).
    // ---------------------------------------------------------------

    #[test]
    fn slice4c_parse_switch_with_name() {
        assert_eq!(
            parse_bot_command("/switch alice"),
            Some(BotCommand::Switch("alice".to_string()))
        );
    }

    #[test]
    fn slice4c_parse_switch_extra_args_ignored() {
        assert_eq!(
            parse_bot_command("/switch alice extra args"),
            Some(BotCommand::Switch("alice".to_string()))
        );
    }

    #[test]
    fn slice4c_parse_switch_without_arg_returns_none() {
        // /switch with no positional arg returns None so the dispatcher
        // emits no reply — operator sees /agents help instead.
        assert_eq!(parse_bot_command("/switch"), None);
    }

    #[test]
    fn slice4c_parse_switch_with_bot_suffix() {
        assert_eq!(
            parse_bot_command("/switch@heymytechcclaude_bot alice"),
            Some(BotCommand::Switch("alice".to_string()))
        );
    }

    fn slice4c_seed_alive(conn: &rusqlite::Connection, agent_id: &str, agent_name: &str) {
        conn.execute(
            "INSERT INTO agent_registry \
             (agent_id, agent_name, connection_id, chat_thread_id, spawned_at, last_pinged_at, state) \
             VALUES (?1, ?2, ?3, NULL, ?4, ?4, 'alive')",
            rusqlite::params![
                agent_id,
                agent_name,
                format!("c-{agent_id}"),
                chrono_millis()
            ],
        )
        .expect("seed insert");
    }

    #[test]
    fn slice4c_switch_target_not_found() {
        let mut conn = rusqlite::Connection::open_in_memory().expect("conn");
        chat::ensure_chat_db_schema(&conn).expect("schema");
        let tx = conn.transaction().unwrap();
        let reply = handle_switch(&tx, 100, None, 8791871989, "ghost").unwrap();
        assert!(reply.contains("not found"), "got: {reply}");
        assert!(reply.contains("/agents"), "should suggest /agents, got: {reply}");
    }

    #[test]
    fn slice4c_switch_first_claim_allowed() {
        // KP1 first-time setup — no prior binding on (chat=100, None).
        // Any user may claim it.
        let mut conn = rusqlite::Connection::open_in_memory().expect("conn");
        chat::ensure_chat_db_schema(&conn).expect("schema");
        slice4c_seed_alive(&conn, "uuid-a", "alice");
        let tx = conn.transaction().unwrap();
        let reply = handle_switch(&tx, 100, None, 8791871989, "alice").unwrap();
        assert!(reply.contains("Switched"), "got: {reply}");
        assert!(reply.contains("alice"), "got: {reply}");
        // Binding actually applied
        let (cid, tid): (Option<i64>, Option<i64>) = tx
            .query_row(
                "SELECT routing_chat_id, routing_thread_id FROM agent_registry WHERE agent_id='uuid-a'",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert_eq!(cid, Some(100));
        assert_eq!(tid, None);
    }

    #[test]
    fn slice4c_switch_matching_last_user_id_allowed() {
        // Existing binding, sender's user_id matches last_user_id → allowed.
        let mut conn = rusqlite::Connection::open_in_memory().expect("conn");
        chat::ensure_chat_db_schema(&conn).expect("schema");
        slice4c_seed_alive(&conn, "uuid-old", "olivia");
        slice4c_seed_alive(&conn, "uuid-new", "natalie");
        // Bind olivia to (100, None) with last_user_id=42.
        conn.execute(
            "UPDATE agent_registry SET routing_chat_id=100, routing_thread_id=NULL, last_user_id=42 WHERE agent_id='uuid-old'",
            [],
        )
        .unwrap();
        let tx = conn.transaction().unwrap();
        let reply = handle_switch(&tx, 100, None, 42, "natalie").unwrap();
        assert!(reply.contains("Switched"), "got: {reply}");
        // Old cleared, new bound.
        let (cid_old, _): (Option<i64>, Option<i64>) = tx
            .query_row(
                "SELECT routing_chat_id, routing_thread_id FROM agent_registry WHERE agent_id='uuid-old'",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert_eq!(cid_old, None);
        let (cid_new, _): (Option<i64>, Option<i64>) = tx
            .query_row(
                "SELECT routing_chat_id, routing_thread_id FROM agent_registry WHERE agent_id='uuid-new'",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert_eq!(cid_new, Some(100));
    }

    #[test]
    fn slice4c_switch_mismatched_user_id_denied() {
        // Different user_id from last_user_id → denied (FR-MAT-8.6).
        let mut conn = rusqlite::Connection::open_in_memory().expect("conn");
        chat::ensure_chat_db_schema(&conn).expect("schema");
        slice4c_seed_alive(&conn, "uuid-old", "olivia");
        slice4c_seed_alive(&conn, "uuid-new", "natalie");
        conn.execute(
            "UPDATE agent_registry SET routing_chat_id=100, routing_thread_id=NULL, last_user_id=42 WHERE agent_id='uuid-old'",
            [],
        )
        .unwrap();
        let tx = conn.transaction().unwrap();
        // Mallory (user_id=999) tries to switch — must be denied.
        let reply = handle_switch(&tx, 100, None, 999, "natalie").unwrap();
        assert!(reply.contains("Denied"), "got: {reply}");
        assert!(reply.contains("olivia"), "should name the binder, got: {reply}");
        // Binding unchanged: olivia still holds (100, None), natalie nothing.
        let (cid_old, _): (Option<i64>, Option<i64>) = tx
            .query_row(
                "SELECT routing_chat_id, routing_thread_id FROM agent_registry WHERE agent_id='uuid-old'",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert_eq!(cid_old, Some(100));
        let (cid_new, _): (Option<i64>, Option<i64>) = tx
            .query_row(
                "SELECT routing_chat_id, routing_thread_id FROM agent_registry WHERE agent_id='uuid-new'",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert_eq!(cid_new, None);
    }

    #[test]
    fn slice4c_switch_same_agent_no_op_success() {
        // /switch <currently-bound-name> succeeds with a "no change" hint
        // regardless of sender — closes the surprising case where the
        // operator typed the name of the CLI they already use.
        let mut conn = rusqlite::Connection::open_in_memory().expect("conn");
        chat::ensure_chat_db_schema(&conn).expect("schema");
        slice4c_seed_alive(&conn, "uuid-a", "alice");
        conn.execute(
            "UPDATE agent_registry SET routing_chat_id=100, routing_thread_id=NULL, last_user_id=42 WHERE agent_id='uuid-a'",
            [],
        )
        .unwrap();
        let tx = conn.transaction().unwrap();
        // ANY user_id — same-name idempotency check runs before the
        // last_user_id gate.
        let reply = handle_switch(&tx, 100, None, 999, "alice").unwrap();
        assert!(reply.contains("Already bound"), "got: {reply}");
    }

    // -----------------------------------------------------------------
    // Slice 8 — Update / CallbackQuery JSON parse round-trip.
    // The Slice 0 baseline wire shape MUST stay byte-for-byte parseable
    // when callback_query is absent OR present-and-null.
    // -----------------------------------------------------------------

    #[test]
    fn slice8_update_parses_without_callback_query() {
        let raw = r#"{
            "update_id": 17616300,
            "message": {
                "message_id": 73,
                "from": {"id": 8791871989, "username": "g"},
                "chat": {"id": 8791871989},
                "text": "ping"
            }
        }"#;
        let u: Update = serde_json::from_str(raw).unwrap();
        assert_eq!(u.update_id, 17616300);
        assert!(u.message.is_some(), "message present");
        assert!(
            u.callback_query.is_none(),
            "absent callback_query must yield None (additive contract)"
        );
    }

    #[test]
    fn slice8_update_parses_with_explicit_null_callback_query() {
        let raw = r#"{
            "update_id": 17616301,
            "message": {"message_id": 1, "from": {"id": 1}, "chat": {"id": 1}, "text": "hi"},
            "callback_query": null
        }"#;
        let u: Update = serde_json::from_str(raw).unwrap();
        assert!(u.callback_query.is_none(), "null also yields None");
    }

    #[test]
    fn slice8_update_parses_a_real_callback_query_shape() {
        // Approximates a real Telegram CallbackQuery: id + from + chat_instance
        // + data + message subset. The daemon's MessageRef extracts only
        // (message_id, chat).
        let raw = r#"{
            "update_id": 17616302,
            "callback_query": {
                "id": "12345",
                "from": {"id": 8791871989, "username": "g"},
                "chat_instance": "-987654321",
                "data": "ask-uuid:option_2",
                "message": {
                    "message_id": 99,
                    "chat": {"id": 8791871989}
                }
            }
        }"#;
        let u: Update = serde_json::from_str(raw).unwrap();
        let cb = u.callback_query.expect("callback_query present");
        assert_eq!(cb.id, "12345");
        assert_eq!(cb.from.id, 8791871989);
        assert_eq!(cb.chat_instance, "-987654321");
        assert_eq!(cb.data.as_deref(), Some("ask-uuid:option_2"));
        let mref = cb.message.expect("message ref present");
        assert_eq!(mref.message_id, 99);
        assert_eq!(mref.chat.id, 8791871989);
    }

    #[test]
    fn slice8_callback_query_parses_without_data_or_message() {
        // Per Telegram Bot API, both `data` and `message` are OPTIONAL.
        // Our daemon does not accept these in the runtime path (no data
        // means we can't dispatch to an ask_id), but parse MUST not fail.
        let raw = r#"{
            "update_id": 17616303,
            "callback_query": {
                "id": "67890",
                "from": {"id": 1},
                "chat_instance": "abc"
            }
        }"#;
        let u: Update = serde_json::from_str(raw).unwrap();
        let cb = u.callback_query.unwrap();
        assert!(cb.data.is_none());
        assert!(cb.message.is_none());
    }

    // ---- Slice 8b multi-select state-machine tests ----

    fn slice8b_setup() -> (rusqlite::Connection, channel_state::Access) {
        let conn = rusqlite::Connection::open_in_memory().expect("conn");
        chat::ensure_chat_db_schema(&conn).expect("chat schema");
        crate::daemon::pending_asks::apply_pending_asks_migration(&conn)
            .expect("pending_asks migration");
        let access = channel_state::Access {
            dm_policy: channel_state::DmPolicy::Allowlist,
            allow_from: vec!["8791871989".to_string()],
            ..channel_state::Access::default()
        };
        (conn, access)
    }

    fn slice8b_seed_ask(
        conn: &rusqlite::Connection,
        ask_id: &str,
        multi: bool,
        selected_values_json: Option<&str>,
    ) {
        let now = chrono_millis();
        let ask = crate::daemon::pending_asks::PendingAsk {
            ask_id: ask_id.to_string(),
            chat_id: 8791871989,
            message_thread_id: None,
            message_id: 99,
            requesting_agent_id: "agent-a".to_string(),
            question: "pick".to_string(),
            options_json: r#"[{"label":"Apple","value":"a"},{"label":"Berry","value":"b"},{"label":"Cherry","value":"c"}]"#.to_string(),
            multi,
            selected_values_json: selected_values_json.map(|s| s.to_string()),
            created_at: now,
            expires_at: now + 600_000,
        };
        crate::daemon::pending_asks::insert_pending(conn, &ask).expect("seed insert");
    }

    fn slice8b_cb_update(ask_id: &str, suffix: &str) -> Update {
        let raw = format!(
            r#"{{
                "update_id": 1,
                "callback_query": {{
                    "id": "cb-1",
                    "from": {{"id": 8791871989, "username": "g"}},
                    "chat_instance": "ci-1",
                    "data": "{ask_id}:{suffix}",
                    "message": {{
                        "message_id": 99,
                        "chat": {{"id": 8791871989}}
                    }}
                }}
            }}"#
        );
        serde_json::from_str(&raw).expect("synthetic CallbackQuery Update")
    }

    #[test]
    fn slice8b_toggle_adds_value_when_absent() {
        let (mut conn, mut access) = slice8b_setup();
        slice8b_seed_ask(&conn, "askA", true, None);
        let batch = vec![slice8b_cb_update("askA", "toggle:b")];
        let outcome = process_batch_with_pairing(&mut conn, &mut access, &batch).unwrap();

        assert_eq!(outcome.callback_acks, vec!["cb-1".to_string()]);
        assert!(outcome.notifications.is_empty(), "toggle does not finalize");
        assert_eq!(outcome.keyboard_edits.len(), 1);
        let edit = &outcome.keyboard_edits[0];
        assert_eq!(edit.chat_id, 8791871989);
        assert_eq!(edit.message_id, 99);
        assert_eq!(edit.done_callback_data, "askA:done");
        // Only "b" should be selected after the toggle.
        let selected: Vec<&str> = edit
            .buttons
            .iter()
            .filter(|(_, _, sel)| *sel)
            .map(|(label, _, _)| label.as_str())
            .collect();
        assert_eq!(selected, vec!["Berry"]);

        // Persisted state on the row matches.
        let row = crate::daemon::pending_asks::get_pending(&conn, "askA")
            .unwrap()
            .expect("row still present");
        let values: Vec<String> =
            serde_json::from_str(row.selected_values_json.as_deref().unwrap()).unwrap();
        assert_eq!(values, vec!["b"]);
    }

    #[test]
    fn slice8b_toggle_removes_value_when_present() {
        let (mut conn, mut access) = slice8b_setup();
        slice8b_seed_ask(&conn, "askR", true, Some(r#"["a","b"]"#));
        let batch = vec![slice8b_cb_update("askR", "toggle:a")];
        let outcome = process_batch_with_pairing(&mut conn, &mut access, &batch).unwrap();

        assert!(outcome.notifications.is_empty());
        assert_eq!(outcome.keyboard_edits.len(), 1);
        let edit = &outcome.keyboard_edits[0];
        let selected: Vec<&str> = edit
            .buttons
            .iter()
            .filter(|(_, _, sel)| *sel)
            .map(|(label, _, _)| label.as_str())
            .collect();
        assert_eq!(selected, vec!["Berry"]);

        let row = crate::daemon::pending_asks::get_pending(&conn, "askR")
            .unwrap()
            .unwrap();
        let values: Vec<String> =
            serde_json::from_str(row.selected_values_json.as_deref().unwrap()).unwrap();
        assert_eq!(values, vec!["b"]);
    }

    #[test]
    fn slice8b_done_emits_values_and_deletes_row() {
        let (mut conn, mut access) = slice8b_setup();
        slice8b_seed_ask(&conn, "askD", true, Some(r#"["a","c"]"#));
        // Seed alive agent so the alive-check returns true.
        slice4c_seed_alive(&conn, "agent-a", "alice");

        let batch = vec![slice8b_cb_update("askD", "done")];
        let outcome = process_batch_with_pairing(&mut conn, &mut access, &batch).unwrap();

        assert_eq!(outcome.callback_acks, vec!["cb-1".to_string()]);
        assert!(outcome.keyboard_edits.is_empty(), "done does not redraw");
        assert_eq!(outcome.notifications.len(), 1);
        let (thread, frame) = &outcome.notifications[0];
        assert_eq!(thread, "telegram:8791871989");
        // Iteration 2 — Slice 8 round-trip data lives in `content`
        // preamble, NOT in meta.values. Mira parses the bracketed
        // prefix at the start of the channel body.
        let content = frame["params"]["content"].as_str().expect("content present");
        assert_eq!(content, "[chat_ask kind=multi ask_id=askD values=a,c]");

        // Row is gone.
        let row = crate::daemon::pending_asks::get_pending(&conn, "askD").unwrap();
        assert!(row.is_none(), "done deletes the pending_asks row");
    }

    #[test]
    fn slice8b_done_with_no_selections_emits_empty_array() {
        let (mut conn, mut access) = slice8b_setup();
        slice8b_seed_ask(&conn, "askE", true, None);
        let batch = vec![slice8b_cb_update("askE", "done")];
        let outcome = process_batch_with_pairing(&mut conn, &mut access, &batch).unwrap();

        assert_eq!(outcome.notifications.len(), 1);
        let content = outcome.notifications[0].1["params"]["content"]
            .as_str()
            .expect("content present");
        assert_eq!(content, "[chat_ask kind=multi ask_id=askE values=]");
    }

    #[test]
    fn slice8b_unknown_ask_id_dropped() {
        let (mut conn, mut access) = slice8b_setup();
        // No seed.
        let batch = vec![slice8b_cb_update("phantom", "toggle:x")];
        let outcome = process_batch_with_pairing(&mut conn, &mut access, &batch).unwrap();

        // Callback was still ACK'd (clears the spinner) but no state
        // mutation, no redraw, no notification.
        assert_eq!(outcome.callback_acks, vec!["cb-1".to_string()]);
        assert!(outcome.notifications.is_empty());
        assert!(outcome.keyboard_edits.is_empty());
    }

    #[test]
    fn slice8b_malformed_suffix_dropped() {
        let (mut conn, mut access) = slice8b_setup();
        slice8b_seed_ask(&conn, "askM", true, None);
        // suffix is neither "done" nor "toggle:<value>" — malformed.
        let batch = vec![slice8b_cb_update("askM", "garbage")];
        let outcome = process_batch_with_pairing(&mut conn, &mut access, &batch).unwrap();

        assert!(outcome.notifications.is_empty());
        assert!(outcome.keyboard_edits.is_empty());
        // Row untouched.
        let row = crate::daemon::pending_asks::get_pending(&conn, "askM")
            .unwrap()
            .unwrap();
        assert!(row.selected_values_json.is_none());
    }
}
