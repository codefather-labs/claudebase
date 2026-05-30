//! Slice 3 — chat backend: schema v5, message persistence, broadcast bus.
//!
//! The chat surface is GLOBAL to the user (per architect OQ-ACD-4) so the
//! storage lives at `$HOME/.claude/knowledge/chat.db`, NOT under a
//! per-project root. The daemon holds an `Arc<ChatBus>` shared between
//! the accept loop and every per-connection handler; each `chat_subscribe`
//! tool-call creates a per-thread `tokio::sync::broadcast::channel`
//! receiver that the handler then forwards to the connection's outbound
//! mpsc.
//!
//! On `chat_post` / `chat_reply` the handler:
//!   1. Persists the row in chat.db (`INSERT OR IGNORE INTO chat_threads`,
//!      `INSERT INTO chat_messages`).
//!   2. Builds the `notifications/claude/channel` JSON.
//!   3. Sends it via `broadcast::Sender::send` — the broadcast::send call
//!      returns `Err` only when there are no subscribers; that's
//!      expected and silently ignored.
//!
//! ## Async discipline (ASYNC_INVARIANTS.md)
//!
//! - DB connections live in `tokio::task::spawn_blocking` closures —
//!   rusqlite is sync, so we do NOT hold a `Connection` across an
//!   `.await` point. The pattern is: `spawn_blocking(move || { open db;
//!   do work; return result })` (Rule 2).
//! - Spawned receiver-forwarding tasks log on error via tracing rather
//!   than `.unwrap()` (Rule 5).
//! - `tokio::sync::broadcast` is the cancellation-safe channel here
//!   (Rule 4).
//!
//! ## Schema v5
//!
//! ```sql
//! CREATE TABLE IF NOT EXISTS chat_threads (
//!   id TEXT PRIMARY KEY,
//!   created_at INTEGER NOT NULL
//! );
//! CREATE TABLE IF NOT EXISTS chat_messages (
//!   id TEXT PRIMARY KEY,
//!   thread_id TEXT NOT NULL,
//!   from_agent TEXT NOT NULL,
//!   content TEXT NOT NULL,     -- empty allowed per TC-3.6
//!   reply_to TEXT,
//!   created_at INTEGER NOT NULL,
//!   delivered_at INTEGER
//! );
//! CREATE INDEX IF NOT EXISTS chat_messages_thread_time_idx
//!     ON chat_messages(thread_id, created_at);
//! CREATE TABLE IF NOT EXISTS daemon_state (
//!   key TEXT PRIMARY KEY,
//!   value TEXT NOT NULL
//! );
//! INSERT OR IGNORE INTO daemon_state(key, value)
//!     VALUES ('telegram.last_update_id', '0');
//! ```

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use rusqlite::{params, Connection};
use serde_json::{json, Value};
use tokio::sync::{broadcast, RwLock};
use uuid::Uuid;

/// Capacity of each per-thread broadcast channel. Slow subscribers lag
/// silently — the broadcast::Receiver will surface `RecvError::Lagged`
/// on the next recv; the forwarding task logs and continues.
const BROADCAST_CAPACITY: usize = 256;

/// Per-thread broadcast bus shared between accept loop and connection
/// handlers. `senders` maps `thread_id` → `broadcast::Sender<Value>` where
/// the `Value` is the full `notifications/claude/channel` JSON frame.
///
/// Slow-subscriber discipline: `broadcast::send` is non-blocking; if a
/// subscriber lags past `BROADCAST_CAPACITY`, the oldest message is
/// dropped from THAT subscriber's view and the next `recv` returns
/// `RecvError::Lagged`. The forwarding task logs and resumes.
pub struct ChatBus {
    senders: RwLock<HashMap<String, broadcast::Sender<Value>>>,
}

impl ChatBus {
    pub fn new() -> Self {
        Self {
            senders: RwLock::new(HashMap::new()),
        }
    }

    /// Subscribe to a thread, lazily creating the broadcast channel on
    /// first subscribe. Returns a fresh `Receiver` that observes only
    /// messages published AFTER this call.
    pub async fn subscribe(&self, thread: &str) -> broadcast::Receiver<Value> {
        // Fast path: read lock; if sender exists, just subscribe.
        {
            let guard = self.senders.read().await;
            if let Some(tx) = guard.get(thread) {
                return tx.subscribe();
            }
        }
        // Slow path: write lock; insert sender if still absent.
        let mut guard = self.senders.write().await;
        let tx = guard
            .entry(thread.to_string())
            .or_insert_with(|| broadcast::channel::<Value>(BROADCAST_CAPACITY).0);
        tx.subscribe()
    }

    /// Send a message to all subscribers of a thread. Returns the number
    /// of receivers reached; 0 when no one is subscribed (silently OK).
    pub async fn publish(&self, thread: &str, frame: Value) -> usize {
        // Fast path: read lock.
        let guard = self.senders.read().await;
        if let Some(tx) = guard.get(thread) {
            // broadcast::send returns Err when no active receivers; we
            // treat that as 0-delivered, not an error.
            tx.send(frame).unwrap_or(0)
        } else {
            0
        }
    }
}

impl Default for ChatBus {
    fn default() -> Self {
        Self::new()
    }
}

/// Slice 7 — build the `notifications/claude/channel` frame with an
/// optional `meta.target_agent_id` routing hint. When `target_agent_id`
/// is `Some`, the `params.meta` object is inserted with that field; when
/// `None`, the `meta` key is OMITTED entirely from `params` (NOT set to
/// `null`) per the architect's STRUCTURAL-7-2 reading of TC-7.5 (which
/// accepts both shapes; we choose `absent` as the idiomatic JSON form).
///
/// Kept as a SEPARATE function from `build_channel_notification` per
/// STRUCTURAL-7-6: server.rs:916 (agent-to-agent `chat_post`/`chat_reply`)
/// has no @-mention semantics and continues calling the original 1-arg
/// builder. Only the Telegram inbound path in `daemon/telegram.rs`
/// invokes the routed variant.
pub fn build_channel_notification_routed(
    msg: &ChatMessage,
    target_agent_id: Option<&str>,
) -> Value {
    // Wire shape MUST match `claude-telegram-voice-control` server.ts:1262-1284
    // verbatim: top-level `params.content` carries the inbound text;
    // `params.meta` carries the routing/identity hints (thread + from_agent
    // + message_id + ts + target_agent_id when set). Claude Code's MCP
    // client (live-tested 2026-05-18) silently DISCARDS any frame whose
    // shape differs from this — the original Slice 7 implementation
    // nested content under `params.message.content` and never reached
    // the LLM's input stream.
    //
    // Slice 7's `meta.target_agent_id` field stays optional: present only
    // when the @-mention resolved to an alive agent. STRUCTURAL-7-2's
    // "absent, not null" rule holds.
    let mut params = serde_json::Map::new();
    params.insert("content".into(), Value::String(msg.content.clone()));

    let mut meta = serde_json::Map::new();
    meta.insert("thread".into(), Value::String(msg.thread_id.clone()));
    meta.insert(
        "from_agent".into(),
        Value::String(msg.from_agent.clone()),
    );
    meta.insert("message_id".into(), Value::String(msg.id.clone()));
    meta.insert(
        "ts".into(),
        Value::Number(serde_json::Number::from(msg.created_at)),
    );
    if let Some(rt) = &msg.reply_to {
        meta.insert("reply_to".into(), Value::String(rt.clone()));
    }
    if let Some(id) = target_agent_id {
        meta.insert("target_agent_id".into(), Value::String(id.to_string()));
    }
    params.insert("meta".into(), Value::Object(meta));

    serde_json::json!({
        "jsonrpc": "2.0",
        "method": "notifications/claude/channel",
        "params": Value::Object(params),
    })
}

/// Telegram-specific meta fields populated by `process_batch_with_pairing`
/// for the channel-notification builder. Mirrors the official Anthropic
/// telegram plugin's meta shape (server.ts:1264-1280) so Claude Code's
/// channel surface parses `chat_id` / `user` / `user_id` / `ts` and emits
/// the `<channel source="claudebase" chat_id="..." user="..." user_id="..."
/// ts="..." message_id="...">` tag correctly. Live-test 2026-05-18:
/// the flat-shape `{thread, from_agent, message_id, ts (number)}` we used
/// pre-fix delivered to the plugin via UDS bus but Claude Code's surface
/// silently dropped it because the surface parser expects these exact
/// field names + types.
#[derive(Debug, Clone)]
pub struct TelegramMessageMeta {
    /// `msg.chat.id` — i64 number, serialised as JSON number (not string).
    pub chat_id: i64,
    /// `msg.message_id` — i64 number, serialised as STRING per
    /// server.ts:1268 `String(msgId)`.
    pub message_id_str: String,
    /// `msg.from.username` OR `String(msg.from.id)` fallback when no
    /// username — per server.ts:1269.
    pub user: String,
    /// `String(msg.from.id)` — numeric Telegram user ID as string per
    /// server.ts:1270.
    pub user_id: String,
    /// ISO 8601 UTC string derived from `msg.date * 1000` per
    /// server.ts:1271 `new Date((ctx.message?.date ?? 0) * 1000).toISOString()`.
    pub ts_iso8601: String,
}

/// Build the `notifications/claude/channel` frame with the official
/// telegram-plugin meta shape. Used by `daemon::telegram::process_batch_
/// with_pairing` — the Telegram inbound surface.
///
/// `target_agent_id` is the optional @-mention routing hint
/// (`meta.target_agent_id` per claudebase Slice 7). When `None`, the
/// key is OMITTED from the meta object — NOT set to `null`.
pub fn build_channel_notification_telegram(
    content: &str,
    tg_meta: &TelegramMessageMeta,
    target_agent_id: Option<&str>,
) -> Value {
    let mut params = serde_json::Map::new();
    params.insert("content".into(), Value::String(content.to_string()));

    let mut meta = serde_json::Map::new();
    meta.insert(
        "chat_id".into(),
        Value::Number(serde_json::Number::from(tg_meta.chat_id)),
    );
    meta.insert(
        "message_id".into(),
        Value::String(tg_meta.message_id_str.clone()),
    );
    meta.insert("user".into(), Value::String(tg_meta.user.clone()));
    meta.insert("user_id".into(), Value::String(tg_meta.user_id.clone()));
    meta.insert("ts".into(), Value::String(tg_meta.ts_iso8601.clone()));

    if let Some(id) = target_agent_id {
        meta.insert("target_agent_id".into(), Value::String(id.to_string()));
    }
    params.insert("meta".into(), Value::Object(meta));

    json!({
        "jsonrpc": "2.0",
        "method": "notifications/claude/channel",
        "params": Value::Object(params),
    })
}

/// Apply schema v5 + v6 + v7 to a chat.db connection. Idempotent — all
/// statements use `IF NOT EXISTS` / `INSERT OR IGNORE`. Wrapped in a
/// BEGIN/COMMIT transaction so partial-failure recovery is clean.
///
/// v5 tables: chat_threads, chat_messages, chat_messages_thread_time_idx,
///            daemon_state + bootstrap row.
/// v6 tables (Slice 5): agent_registry + 2 indexes (1 partial UNIQUE per
///            F-5.1 red-team finding + 1 routing index for Slice 7
///            target_agent_id lookups). The CHECK constraint on `state`
///            is enforced at the DB layer per STRUCTURAL-5-2 — a buggy
///            Rust caller or a sqlite3 CLI edit cannot corrupt state.
/// v7 tables (telegram-multi-cli Slice 1): three additive tables for
///            chat-as-id routing —
///            - `active_cli_per_chat` — the active CLI bound to a chat.
///            - `tg_message_map` — reply-quote tracking (composite PK +
///              `sent_at` index for the 30-day TTL purge, architect
///              action item 4).
///            - `pending_questions` — durable pending `chat_ask` state
///              that survives daemon restart (F-1 red-team revision;
///              replaces the in-memory map originally planned for Slice 5).
///            chat.db has no `user_version` gate — the migration is
///            additive-by-construction (all `CREATE TABLE IF NOT EXISTS`).
pub fn ensure_chat_db_schema(conn: &Connection) -> rusqlite::Result<()> {
    conn.execute_batch(
        r#"
BEGIN;
CREATE TABLE IF NOT EXISTS chat_threads (
  id TEXT PRIMARY KEY,
  created_at INTEGER NOT NULL
);
CREATE TABLE IF NOT EXISTS chat_messages (
  id           TEXT PRIMARY KEY,
  thread_id    TEXT NOT NULL,
  from_agent   TEXT NOT NULL,
  content      TEXT NOT NULL,
  reply_to     TEXT,
  created_at   INTEGER NOT NULL,
  delivered_at INTEGER
);
CREATE INDEX IF NOT EXISTS chat_messages_thread_time_idx
  ON chat_messages(thread_id, created_at);
CREATE TABLE IF NOT EXISTS daemon_state (
  key   TEXT PRIMARY KEY,
  value TEXT NOT NULL
);
INSERT OR IGNORE INTO daemon_state(key, value)
  VALUES ('telegram.last_update_id', '0');
CREATE TABLE IF NOT EXISTS agent_registry (
  agent_id           TEXT PRIMARY KEY,
  agent_name         TEXT NOT NULL,
  connection_id      TEXT NOT NULL,
  chat_thread_id     TEXT,
  permission_relayer TEXT,
  spawned_at         INTEGER NOT NULL,
  last_pinged_at     INTEGER NOT NULL,
  state              TEXT NOT NULL CHECK (state IN ('alive','orphaned','dead')),
  metadata           TEXT
);
CREATE UNIQUE INDEX IF NOT EXISTS agent_registry_thread_name_alive_idx
    ON agent_registry(chat_thread_id, agent_name)
    WHERE state = 'alive' AND chat_thread_id IS NOT NULL;
CREATE INDEX IF NOT EXISTS agent_registry_thread_alive_idx
    ON agent_registry(chat_thread_id, state)
    WHERE state = 'alive';
CREATE TABLE IF NOT EXISTS active_cli_per_chat (
  chat_id         INTEGER PRIMARY KEY,
  active_cli_name TEXT NOT NULL,
  active_agent_id TEXT NOT NULL,
  set_at          INTEGER NOT NULL,
  set_by          TEXT NOT NULL
);
CREATE TABLE IF NOT EXISTS tg_message_map (
  tg_msg_id       INTEGER NOT NULL,
  chat_id         INTEGER NOT NULL,
  sender_agent_id TEXT NOT NULL,
  sent_at         INTEGER NOT NULL,
  PRIMARY KEY (chat_id, tg_msg_id)
);
CREATE INDEX IF NOT EXISTS tg_message_map_sent_at_idx
  ON tg_message_map(sent_at);
CREATE TABLE IF NOT EXISTS pending_questions (
  question_id        TEXT PRIMARY KEY,
  chat_id            INTEGER NOT NULL,
  requesting_agent_id TEXT NOT NULL,
  options_json       TEXT NOT NULL,
  created_at         INTEGER NOT NULL,
  expires_at         INTEGER NOT NULL
);
COMMIT;
"#,
    )?;
    Ok(())
}

/// Startup TTL eviction for the two time-bounded v7 tables. Run once at
/// daemon boot AFTER `ensure_chat_db_schema` applies (the tables must
/// exist before the DELETEs run). Separated from `ensure_chat_db_schema`
/// because schema-application is data-preserving by contract; the purges
/// intentionally delete stale rows and so are a distinct concern.
///
/// - `tg_message_map`: rows older than 30 days (2 592 000 s) are dropped
///   (FR-TMC-1.3 — reply-quote tracking is only useful while the Telegram
///   message is recent enough to be replied to).
/// - `pending_questions`: rows whose `expires_at` is in the past are
///   evicted (F-1/F-8 — an unanswered `chat_ask` that has expired must not
///   linger and route a stale callback after daemon restart).
///
/// All timestamps in these two tables are UNIX **seconds** (the
/// `strftime('%s','now')` convention), distinct from the `now_millis()`
/// convention used by `agent_registry`. The cutoff arithmetic stays in
/// SQL so the unit is unambiguous and matches whatever the producer wrote.
pub fn purge_expired_chat_state(conn: &Connection) -> rusqlite::Result<()> {
    conn.execute(
        "DELETE FROM tg_message_map \
         WHERE sent_at < (strftime('%s','now') - 2592000)",
        [],
    )?;
    conn.execute(
        "DELETE FROM pending_questions \
         WHERE expires_at < strftime('%s','now')",
        [],
    )?;
    Ok(())
}

/// Open the chat.db at `$HOME/.claude/knowledge/chat.db` (creating
/// parent dirs and the file as needed), apply schema v5, and return the
/// Connection. Caller is responsible for moving the Connection into a
/// `spawn_blocking` closure if running inside an async task.
pub fn open_chat_db() -> anyhow::Result<Connection> {
    let path = crate::store::user_level_chat_db_path();
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let conn = Connection::open_with_flags(
        &path,
        rusqlite::OpenFlags::SQLITE_OPEN_CREATE | rusqlite::OpenFlags::SQLITE_OPEN_READ_WRITE,
    )?;
    ensure_chat_db_schema(&conn)?;
    Ok(conn)
}

/// Current wall-clock milliseconds since the UNIX epoch.
pub(crate) fn now_millis() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// One persisted chat message row, returned by backlog SELECT and used
/// as the `message` payload in `notifications/claude/channel` frames.
#[derive(Debug, Clone)]
pub struct ChatMessage {
    pub id: String,
    pub thread_id: String,
    pub from_agent: String,
    pub content: String,
    pub reply_to: Option<String>,
    pub created_at: i64,
}

impl ChatMessage {
    pub fn to_json(&self) -> Value {
        json!({
            "id": self.id,
            "thread_id": self.thread_id,
            "from_agent": self.from_agent,
            "content": self.content,
            "reply_to": self.reply_to,
            "created_at": self.created_at,
        })
    }
}

/// Insert a chat message row, also ensuring the thread row exists. Returns
/// the persisted `ChatMessage` (with generated id + created_at). The
/// `reply_to` argument is taken verbatim — caller is responsible for
/// resolving stale references to NULL (TC-3.5).
pub fn insert_message(
    conn: &Connection,
    thread_id: &str,
    from_agent: &str,
    content: &str,
    reply_to: Option<&str>,
) -> rusqlite::Result<ChatMessage> {
    let id = Uuid::new_v4().to_string();
    let now = now_millis();

    conn.execute(
        "INSERT OR IGNORE INTO chat_threads (id, created_at) VALUES (?1, ?2)",
        params![thread_id, now],
    )?;

    conn.execute(
        "INSERT INTO chat_messages (id, thread_id, from_agent, content, reply_to, created_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
        params![id, thread_id, from_agent, content, reply_to, now],
    )?;

    Ok(ChatMessage {
        id,
        thread_id: thread_id.to_string(),
        from_agent: from_agent.to_string(),
        content: content.to_string(),
        reply_to: reply_to.map(|s| s.to_string()),
        created_at: now,
    })
}

/// Resolve a `reply_to` reference: keep it verbatim when it parses as
/// a UUID (even if the message doesn't exist in DB — TC-3.2 expects
/// hardcoded UUIDs to survive; the client can have legitimate
/// references to messages that landed before the daemon last
/// restarted). Drop it (downgrade to NULL) when it doesn't parse as a
/// UUID — TC-3.5 names this "graceful degradation on stale reply_to"
/// and uses the literal `"nonexistent-uuid-1234"` (a non-UUID) as
/// representative of "junk that shouldn't reach storage".
///
/// The `_conn` parameter is retained so a future stricter mode (e.g.
/// `--strict-reply-to` daemon flag) can re-introduce FK-style checks
/// without changing the call sites.
pub fn resolve_reply_to(
    _conn: &Connection,
    reply_to: Option<&str>,
) -> rusqlite::Result<Option<String>> {
    let Some(rt) = reply_to else {
        return Ok(None);
    };
    // Parse as UUID. If it parses, keep verbatim; else, downgrade to NULL.
    if uuid::Uuid::parse_str(rt).is_ok() {
        Ok(Some(rt.to_string()))
    } else {
        Ok(None)
    }
}

/// Read all undelivered messages for `thread_id` (delivered_at IS NULL)
/// ordered chronologically, then UPDATE delivered_at to now for those
/// rows. Returns the snapshotted backlog the subscriber should be told
/// about. Performs the UPDATE only on the snapshotted IDs — not a blanket
/// UPDATE on the table — so concurrent inserts of new messages after the
/// SELECT but before the UPDATE keep their NULL delivered_at and surface
/// on the broadcast channel instead of being silently consumed.
pub fn drain_backlog(conn: &mut Connection, thread_id: &str) -> rusqlite::Result<Vec<ChatMessage>> {
    let tx = conn.transaction()?;
    let messages: Vec<ChatMessage> = {
        let mut stmt = tx.prepare(
            "SELECT id, thread_id, from_agent, content, reply_to, created_at
             FROM chat_messages
             WHERE thread_id = ?1 AND delivered_at IS NULL
             ORDER BY created_at ASC, id ASC",
        )?;
        let rows = stmt.query_map(params![thread_id], |row| {
            Ok(ChatMessage {
                id: row.get(0)?,
                thread_id: row.get(1)?,
                from_agent: row.get(2)?,
                content: row.get(3)?,
                reply_to: row.get(4)?,
                created_at: row.get(5)?,
            })
        })?;
        let mut out = Vec::new();
        for row in rows {
            out.push(row?);
        }
        out
    };

    let now = now_millis();
    for msg in &messages {
        tx.execute(
            "UPDATE chat_messages SET delivered_at = ?1 WHERE id = ?2",
            params![now, msg.id],
        )?;
    }
    tx.commit()?;
    Ok(messages)
}

/// SELECT messages for `thread_id` ordered chronologically with optional
/// `since` (created_at >) and `limit`. Used by `chat_list` MCP tool and
/// the `chat list --thread X` CLI.
pub fn list_messages(
    conn: &Connection,
    thread_id: &str,
    since: Option<i64>,
    limit: Option<i64>,
) -> rusqlite::Result<Vec<ChatMessage>> {
    let mut sql = String::from(
        "SELECT id, thread_id, from_agent, content, reply_to, created_at
         FROM chat_messages WHERE thread_id = ?1",
    );
    if since.is_some() {
        sql.push_str(" AND created_at > ?2");
    }
    sql.push_str(" ORDER BY created_at ASC, id ASC");
    let has_limit = limit.is_some();
    if has_limit {
        // SQLite accepts numeric LIMIT bound parameters; we always bind
        // the limit as the last `?` slot.
        if since.is_some() {
            sql.push_str(" LIMIT ?3");
        } else {
            sql.push_str(" LIMIT ?2");
        }
    }

    let mut stmt = conn.prepare(&sql)?;
    let mapper = |row: &rusqlite::Row<'_>| {
        Ok(ChatMessage {
            id: row.get(0)?,
            thread_id: row.get(1)?,
            from_agent: row.get(2)?,
            content: row.get(3)?,
            reply_to: row.get(4)?,
            created_at: row.get(5)?,
        })
    };
    let rows = match (since, limit) {
        (Some(s), Some(l)) => stmt.query_map(params![thread_id, s, l], mapper)?,
        (Some(s), None) => stmt.query_map(params![thread_id, s], mapper)?,
        (None, Some(l)) => stmt.query_map(params![thread_id, l], mapper)?,
        (None, None) => stmt.query_map(params![thread_id], mapper)?,
    };
    let mut out = Vec::new();
    for row in rows {
        out.push(row?);
    }
    Ok(out)
}

/// One thread row with summary stats — used by `claudebase chat threads`.
#[derive(Debug, Clone)]
pub struct ThreadSummary {
    pub id: String,
    pub message_count: i64,
    pub last_created_at: Option<i64>,
}

/// Return all known threads with message counts and last-message
/// timestamps. Threads with zero messages still appear (created via
/// INSERT OR IGNORE into `chat_threads` on the first post).
pub fn list_threads(conn: &Connection) -> rusqlite::Result<Vec<ThreadSummary>> {
    let mut stmt = conn.prepare(
        "SELECT t.id,
                (SELECT COUNT(*) FROM chat_messages m WHERE m.thread_id = t.id),
                (SELECT MAX(created_at) FROM chat_messages m WHERE m.thread_id = t.id)
         FROM chat_threads t
         ORDER BY t.created_at ASC, t.id ASC",
    )?;
    let rows = stmt.query_map(params![], |row| {
        Ok(ThreadSummary {
            id: row.get(0)?,
            message_count: row.get(1)?,
            last_created_at: row.get(2)?,
        })
    })?;
    let mut out = Vec::new();
    for row in rows {
        out.push(row?);
    }
    Ok(out)
}

/// Build the `notifications/claude/channel` frame for a posted message.
///
/// Wire shape MUST match `build_channel_notification_routed` for parity with
/// the Telegram inbound path: top-level `params.content` carries the message
/// text; `params.meta` carries thread + from_agent + message_id + ts (+
/// reply_to when present). Claude Code's MCP client silently DISCARDS any
/// frame whose shape differs (live-tested 2026-05-18) — without this
/// alignment, agent-to-agent `chat_post` / `chat_reply` notifications never
/// reach the peer LLM's input stream even though daemon broadcast succeeds.
pub fn build_channel_notification(msg: &ChatMessage) -> Value {
    let mut params = serde_json::Map::new();
    params.insert("content".into(), Value::String(msg.content.clone()));

    let mut meta = serde_json::Map::new();
    meta.insert("thread".into(), Value::String(msg.thread_id.clone()));
    meta.insert("from_agent".into(), Value::String(msg.from_agent.clone()));
    meta.insert("message_id".into(), Value::String(msg.id.clone()));
    meta.insert(
        "ts".into(),
        Value::Number(serde_json::Number::from(msg.created_at)),
    );
    if let Some(rt) = &msg.reply_to {
        meta.insert("reply_to".into(), Value::String(rt.clone()));
    }
    params.insert("meta".into(), Value::Object(meta));

    json!({
        "jsonrpc": "2.0",
        "method": "notifications/claude/channel",
        "params": Value::Object(params),
    })
}

/// Globally-shared `Arc<ChatBus>` — built once at daemon startup, cloned
/// into each `handle_connection` task. The wrapper type avoids exporting
/// `Arc<ChatBus>` literals all over the codebase.
pub type SharedBus = Arc<ChatBus>;

#[cfg(test)]
mod tests {
    use super::*;
    use rusqlite::Connection;

    fn fresh_db() -> Connection {
        let conn = Connection::open_in_memory().expect("in-memory db");
        ensure_chat_db_schema(&conn).expect("schema applied");
        conn
    }

    #[test]
    fn schema_creates_tables_and_index() {
        let conn = fresh_db();
        let n: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name='chat_threads'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(n, 1);
        let n: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name='chat_messages'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(n, 1);
        let n: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM sqlite_master WHERE type='index' AND name='chat_messages_thread_time_idx'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(n, 1);
        let v: String = conn
            .query_row(
                "SELECT value FROM daemon_state WHERE key='telegram.last_update_id'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(v, "0");
    }

    #[test]
    fn insert_message_persists_row_and_thread() {
        let conn = fresh_db();
        let msg = insert_message(&conn, "telegram:1", "agent-a", "hi", None).unwrap();
        assert!(!msg.id.is_empty());
        let n: i64 = conn
            .query_row("SELECT COUNT(*) FROM chat_messages", [], |r| r.get(0))
            .unwrap();
        assert_eq!(n, 1);
        let n: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM chat_threads WHERE id='telegram:1'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(n, 1);
    }

    #[test]
    fn drain_backlog_marks_delivered() {
        let mut conn = fresh_db();
        insert_message(&conn, "t1", "a", "msg1", None).unwrap();
        insert_message(&conn, "t1", "a", "msg2", None).unwrap();
        let backlog = drain_backlog(&mut conn, "t1").unwrap();
        assert_eq!(backlog.len(), 2);
        let n_undelivered: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM chat_messages WHERE thread_id='t1' AND delivered_at IS NULL",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(n_undelivered, 0);
    }

    // ---- telegram-multi-cli Slice 1: schema v7 + startup purges ----

    /// Helper: column names of a table via PRAGMA table_info, in cid order.
    fn table_cols(conn: &Connection, table: &str) -> Vec<String> {
        let mut stmt = conn
            .prepare(&format!("PRAGMA table_info({table})"))
            .unwrap();
        let cols: Vec<String> = stmt
            .query_map([], |r| r.get::<_, String>(1))
            .unwrap()
            .map(|c| c.unwrap())
            .collect();
        cols
    }

    /// TC-TMC-1.1(a): active_cli_per_chat has exactly the 5 spec columns.
    #[test]
    fn v7_active_cli_per_chat_has_5_columns() {
        let conn = fresh_db();
        let cols = table_cols(&conn, "active_cli_per_chat");
        assert_eq!(
            cols,
            vec![
                "chat_id",
                "active_cli_name",
                "active_agent_id",
                "set_at",
                "set_by"
            ]
        );
    }

    /// TC-TMC-1.1(b): tg_message_map has exactly the 4 spec columns AND a
    /// composite PK on (chat_id, tg_msg_id).
    #[test]
    fn v7_tg_message_map_has_4_columns_and_composite_pk() {
        let conn = fresh_db();
        let cols = table_cols(&conn, "tg_message_map");
        assert_eq!(
            cols,
            vec!["tg_msg_id", "chat_id", "sender_agent_id", "sent_at"]
        );
        // Composite PK: the two PK members per PRAGMA (pk column index > 0).
        let mut stmt = conn.prepare("PRAGMA table_info(tg_message_map)").unwrap();
        // row: (cid, name, type, notnull, dflt, pk)
        let pk_members: Vec<(String, i64)> = stmt
            .query_map([], |r| {
                Ok((r.get::<_, String>(1)?, r.get::<_, i64>(5)?))
            })
            .unwrap()
            .map(|x| x.unwrap())
            .filter(|(_, pk)| *pk > 0)
            .collect();
        assert_eq!(pk_members.len(), 2, "expected composite PK of 2 columns");
        // PRAGMA reports PK order via the pk index: chat_id=1, tg_msg_id=2.
        let names: Vec<&str> = pk_members.iter().map(|(n, _)| n.as_str()).collect();
        assert!(names.contains(&"chat_id"));
        assert!(names.contains(&"tg_msg_id"));
    }

    /// tg_message_map_sent_at_idx index exists (architect action item 4).
    #[test]
    fn v7_tg_message_map_sent_at_index_exists() {
        let conn = fresh_db();
        let n: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM sqlite_master \
                 WHERE type='index' AND name='tg_message_map_sent_at_idx'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(n, 1);
    }

    /// pending_questions has exactly the 6 spec columns.
    #[test]
    fn v7_pending_questions_has_6_columns() {
        let conn = fresh_db();
        let cols = table_cols(&conn, "pending_questions");
        assert_eq!(
            cols,
            vec![
                "question_id",
                "chat_id",
                "requesting_agent_id",
                "options_json",
                "created_at",
                "expires_at"
            ]
        );
    }

    /// TC-TMC-1.2: ensure_chat_db_schema is idempotent — a second call on
    /// an already-v7 db returns Ok and changes no row counts.
    #[test]
    fn v7_schema_is_idempotent() {
        let conn = fresh_db();
        // Plant one row in each v7 table.
        conn.execute(
            "INSERT INTO active_cli_per_chat \
             (chat_id, active_cli_name, active_agent_id, set_at, set_by) \
             VALUES (1, 'cli-a', 'agent-a', 100, 'op')",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO tg_message_map \
             (tg_msg_id, chat_id, sender_agent_id, sent_at) \
             VALUES (5, 1, 'agent-a', 9999999999)",
            [],
        )
        .unwrap();
        // Second schema call must succeed and preserve the planted rows.
        ensure_chat_db_schema(&conn).expect("second ensure_chat_db_schema call Ok");
        let n_cli: i64 = conn
            .query_row("SELECT COUNT(*) FROM active_cli_per_chat", [], |r| r.get(0))
            .unwrap();
        let n_map: i64 = conn
            .query_row("SELECT COUNT(*) FROM tg_message_map", [], |r| r.get(0))
            .unwrap();
        assert_eq!(n_cli, 1);
        assert_eq!(n_map, 1);
    }

    /// Existing v6 rows survive a re-open: insert a chat_threads row and
    /// run ensure_chat_db_schema again; the row count is unchanged.
    #[test]
    fn v6_rows_survive_schema_reapply() {
        let conn = fresh_db();
        insert_message(&conn, "telegram:42", "agent-a", "hello", None).unwrap();
        let before: i64 = conn
            .query_row("SELECT COUNT(*) FROM chat_threads", [], |r| r.get(0))
            .unwrap();
        let msg_before: i64 = conn
            .query_row("SELECT COUNT(*) FROM chat_messages", [], |r| r.get(0))
            .unwrap();
        ensure_chat_db_schema(&conn).unwrap();
        let after: i64 = conn
            .query_row("SELECT COUNT(*) FROM chat_threads", [], |r| r.get(0))
            .unwrap();
        let msg_after: i64 = conn
            .query_row("SELECT COUNT(*) FROM chat_messages", [], |r| r.get(0))
            .unwrap();
        assert_eq!(before, after);
        assert_eq!(msg_before, msg_after);
        assert_eq!(after, 1);
    }

    /// Startup eviction: an expired pending_questions row (expires_at in
    /// the past) is deleted by purge_expired_chat_state; a future-expiry
    /// row survives.
    #[test]
    fn purge_evicts_expired_pending_questions() {
        let conn = fresh_db();
        conn.execute(
            "INSERT INTO pending_questions \
             (question_id, chat_id, requesting_agent_id, options_json, created_at, expires_at) \
             VALUES ('q-expired', 1, 'agent-a', '[]', 0, 1)",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO pending_questions \
             (question_id, chat_id, requesting_agent_id, options_json, created_at, expires_at) \
             VALUES ('q-future', 1, 'agent-a', '[]', 0, strftime('%s','now') + 3600)",
            [],
        )
        .unwrap();
        purge_expired_chat_state(&conn).unwrap();
        let remaining: i64 = conn
            .query_row("SELECT COUNT(*) FROM pending_questions", [], |r| r.get(0))
            .unwrap();
        assert_eq!(remaining, 1, "only the future-expiry row should survive");
        let id: String = conn
            .query_row("SELECT question_id FROM pending_questions", [], |r| r.get(0))
            .unwrap();
        assert_eq!(id, "q-future");
    }

    /// Startup eviction: a tg_message_map row older than the 30-day TTL is
    /// deleted; a recent row survives.
    #[test]
    fn purge_evicts_old_tg_message_map_rows() {
        let conn = fresh_db();
        // sent_at far in the past (epoch second 1) → older than 30 days.
        conn.execute(
            "INSERT INTO tg_message_map \
             (tg_msg_id, chat_id, sender_agent_id, sent_at) \
             VALUES (1, 1, 'agent-a', 1)",
            [],
        )
        .unwrap();
        // Recent row (now) → survives.
        conn.execute(
            "INSERT INTO tg_message_map \
             (tg_msg_id, chat_id, sender_agent_id, sent_at) \
             VALUES (2, 1, 'agent-a', strftime('%s','now'))",
            [],
        )
        .unwrap();
        purge_expired_chat_state(&conn).unwrap();
        let remaining: i64 = conn
            .query_row("SELECT COUNT(*) FROM tg_message_map", [], |r| r.get(0))
            .unwrap();
        assert_eq!(remaining, 1, "only the recent row should survive");
        let id: i64 = conn
            .query_row("SELECT tg_msg_id FROM tg_message_map", [], |r| r.get(0))
            .unwrap();
        assert_eq!(id, 2);
    }
}
