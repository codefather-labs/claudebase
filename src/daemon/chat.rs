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
    let mut params = serde_json::Map::new();
    params.insert("thread".into(), Value::String(msg.thread_id.clone()));
    params.insert(
        "message".into(),
        serde_json::json!({
            "id": msg.id,
            "thread_id": msg.thread_id,
            "from_agent": msg.from_agent,
            "content": msg.content,
            "reply_to": msg.reply_to,
            "created_at": msg.created_at,
        }),
    );
    if let Some(id) = target_agent_id {
        params.insert(
            "meta".into(),
            serde_json::json!({ "target_agent_id": id }),
        );
    }
    serde_json::json!({
        "jsonrpc": "2.0",
        "method": "notifications/claude/channel",
        "params": Value::Object(params),
    })
}

/// Apply schema v5 + v6 to a chat.db connection. Idempotent — all
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
COMMIT;
"#,
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
pub fn build_channel_notification(message: &ChatMessage) -> Value {
    json!({
        "jsonrpc": "2.0",
        "method": "notifications/claude/channel",
        "params": {
            "thread": message.thread_id,
            "message": message.to_json(),
        }
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
}
