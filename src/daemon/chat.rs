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
/// Slice 8 of cli-to-cli-routing — build the `notifications/claude/channel`
/// frame for an agent-to-agent message (direct path or DND drain).
///
/// **Meta convention (NFR-C2C-8 / red team F-8).** The frame uses the
/// SAME `notifications/claude/channel` method as the Telegram inbound
/// path, but with two distinguishing meta keys:
///
///   * `meta.source = "claudebase:agent"` (TG inbound uses
///     `"claudebase"` / `"plugin:telegram:telegram"` depending on layer)
///   * `meta.kind = "agent-to-agent"` (TG inbound omits this)
///
/// Claude Code's channel surface renders the meta into a `<channel
/// source="..." kind="..." from_agent_id="..." thread="agent:..."
/// target_agent_id="..." message_id="...">CONTENT</channel>` tag in
/// the receiving model's prompt context. The `source` and `kind`
/// attributes are the load-bearing distinguishers — downstream
/// consumers MUST treat any frame with `meta.kind != "agent-to-agent"`
/// (or no `meta.kind`) as TG inbound and fall through to the existing
/// channel rendering (UC-C2C-15-EC1 fallthrough rule).
///
/// `drained_from_dnd` is `true` when the frame originates from
/// Slice 5's recurring drain task (i.e., the message was queued under
/// DND and is being re-emitted after expiry); `false` for the direct
/// `agent_send` path.
pub fn build_channel_notification_agent_to_agent(
    content: &str,
    from_agent_id: &str,
    target_agent_id: &str,
    message_id: &str,
    drained_from_dnd: bool,
) -> Value {
    let thread = format!("agent:{target_agent_id}");
    // Slice 8 hotfix #2 (Wave 5 live QA, doc_id #13 in insights corpus):
    // Round 1 added the 5 TG-shape keys but kept extra distinguishers
    // (kind / target_agent_id / from_agent_id / thread / drained_from_dnd /
    // source) in meta — frames STILL dropped because CC's channel
    // renderer rejects unknown meta keys. This is the SAME pattern
    // caught in v0.9-cut Slice 8 AR-9 amendment: load agent-to-agent
    // distinguishers into `params.content` as a parseable preamble
    // and keep meta bit-for-bit identical to the TG inbound shape.
    //
    // Meta MUST contain ONLY the 5 TG-known keys (plus the optional
    // `target_agent_id` which TG inbound also uses since Slice 6 of
    // multi-agent-on-v0.6):
    //   chat_id, message_id, user, user_id, ts, target_agent_id
    //
    // Content preamble carries the rest as a one-line JSON object
    // followed by a blank line then the verbatim user content, so a
    // receiver model parses the metadata then reads the message.
    let now_iso = chrono::Utc::now().format("%Y-%m-%dT%H:%M:%S%.3fZ").to_string();
    let preamble = serde_json::json!({
        "agent_to_agent": {
            "from_agent_id": from_agent_id,
            "target_agent_id": target_agent_id,
            "thread": thread,
            "drained_from_dnd": drained_from_dnd,
            "message_id": message_id,
        }
    });
    let preamble_line = serde_json::to_string(&preamble)
        .unwrap_or_else(|_| "{\"agent_to_agent\":{}}".to_string());
    let body = format!("{preamble_line}\n\n{content}");
    serde_json::json!({
        "jsonrpc": "2.0",
        "method": "notifications/claude/channel",
        "params": {
            "meta": {
                "chat_id": thread,
                "message_id": message_id,
                "user": from_agent_id,
                "user_id": from_agent_id,
                "ts": now_iso,
                "target_agent_id": target_agent_id,
            },
            "content": body,
        }
    })
}

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
    /// `msg.chat.id` — i64 number, serialised as **JSON string** in the
    /// channel meta to match the Slice 0 baseline plugin-emit shape
    /// (verified against `docs/qa/evidence/slice-0-baseline-v0.6/
    /// plugin-stdout.jsonl` on 2026-06-03). The official Anthropic
    /// telegram plugin's server.ts emits `String(msg.chat.id)`; CC's
    /// channel-surface renderer is strict and silently drops frames
    /// whose `meta.chat_id` is a JSON Number — the live-confirmed root
    /// cause of the multi-agent-telegram-on-v0.6 KP1 regression.
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
    /// Slice 2 of multi-agent-telegram-on-v0.6 — Telegram forum-topic id.
    /// When `Some`, `build_channel_notification_telegram` emits it as
    /// `meta.thread_id` (string, per v0.6 ID-as-string discipline).
    /// When `None`, the field is OMITTED from the meta object so the
    /// downstream `<channel>` surface stays bit-for-bit identical for
    /// DM / topic-less group inbound (the Slice 0 baseline shape).
    pub thread_id: Option<i64>,
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
    // PRD A18 frozen-contract: chat_id is a STRING in the channel meta
    // (matches Slice 0 baseline plugin-emit shape — verified against
    // docs/qa/evidence/slice-0-baseline-v0.6/plugin-stdout.jsonl 2026-06-03).
    // Emitting as Number causes CC's channel-surface renderer to silently
    // drop the frame so `<channel ...>` events never reach the session —
    // a regression introduced in the daemon-emit path that this branch
    // depends on. Keep both forms in lockstep: string everywhere.
    meta.insert(
        "chat_id".into(),
        Value::String(tg_meta.chat_id.to_string()),
    );
    meta.insert(
        "message_id".into(),
        Value::String(tg_meta.message_id_str.clone()),
    );
    meta.insert("user".into(), Value::String(tg_meta.user.clone()));
    meta.insert("user_id".into(), Value::String(tg_meta.user_id.clone()));
    meta.insert("ts".into(), Value::String(tg_meta.ts_iso8601.clone()));

    // Slice 2 additive optional field per PRD §18 FR-MAT-7 (C3 wire
    // contract). Forum-topic-id emitted as string to match the v0.6
    // chat_id/message_id/user_id-as-string discipline. When None, the
    // key is OMITTED so DM / topic-less inbound preserves Slice 0
    // baseline meta shape bit-for-bit.
    if let Some(tid) = tg_meta.thread_id {
        meta.insert("thread_id".into(), Value::String(tid.to_string()));
    }
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

/// Slice 8 of multi-agent-telegram-on-v0.6 — build the `<channel>`
/// notification frame the daemon emits when a Telegram CallbackQuery
/// resolves a pending `chat_ask` (single-select on any tap; multi-select
/// on Done). Per architect AR-4 + AR-5:
///
/// - `meta.target_agent_id` is set to `originating_agent_id` ONLY when
///   the caller's `is_originator_alive` returns `true`. When the
///   originating CC has exited, the field is OMITTED — the bridge filter
///   in `src/plugin/bridge.rs::should_relay_channel_notification`
///   treats absent `target_agent_id` as unaddressed broadcast and
///   relays to every active CC. The new `meta.originating_agent_id`
///   field (informational, NOT gated) carries the breadcrumb so any
///   receiving CC can see "this was originally for agent X".
///
/// - The frame also carries `meta.question`, `meta.options`, and
///   `meta.multi` so a CC that has been compacted between the original
///   `chat_ask` and this response can reconstruct semantic context
///   without an in-session memory lookup (compaction-resilience).
///
/// Caller passes either `Single(value)` for one-shot single-select or
/// `Multi(values)` for the array form. The frame encodes them under
/// `meta.value` (string) or `meta.values` (JSON array of strings)
/// respectively. The split-enum keeps the type signature self-
/// documenting at the call site.
pub fn build_channel_notification_callback_response(
    ask_id: &str,
    answer: CallbackAnswer<'_>,
    originating_agent_id: &str,
    is_originator_alive: bool,
    _question: &str,
    _options_json: &str,
    multi: bool,
    tg_meta: &TelegramMessageMeta,
) -> Value {
    // Slice 8b live-fix iteration 2 (2026-06-04): CC's channel-surface
    // renderer drops the frame when `params.meta` carries keys outside
    // the inbound-Telegram schema (chat_id / message_id / user / user_id
    // / ts / optional thread_id / optional target_agent_id). Iteration 1
    // added the missing v0.6 string-shape keys but kept Slice 8 extras
    // (is_callback_response, ask_id, value/values, multi, question,
    // options[], originating_agent_id) — daemon log on 22:40:18 showed
    // the frame written to the bridge's UDS (684 bytes) but it never
    // surfaced in the requesting CC. The inbound pizza message (304
    // bytes, plain meta) DID surface in the same session via the same
    // path — confirming the size/shape difference is the gate, not
    // transport. Iteration 2: keep `meta` BIT-FOR-BIT identical to the
    // inbound Telegram shape; encode Slice 8 round-trip data inside
    // `content` as a parseable single-line preamble Mira reads at the
    // start of the `<channel>` body. Trade-off: `meta.is_callback_
    // response` / structured options[] no longer accessible to other
    // CC consumers — but the chat_ask round-trip is THE load-bearing
    // path and operator-visibility wins over machine-cleanness.
    let mut params = serde_json::Map::new();

    let answers_str = match answer {
        CallbackAnswer::Single(v) => v.to_string(),
        CallbackAnswer::Multi(values) => values.join(","),
    };
    // Preamble shape — single line so a glance at the channel surface
    // tells Mira (or any other consumer reading the rendered XML body)
    // what ask resolved, with what answer(s), and the multi/single
    // discriminator. Keep keys lowercase + `=` separated for cheap
    // regex/split parsing. Mira parses by looking for
    // `[chat_ask kind=multi ask_id=<uuid> values=v1,v2,...]` /
    // `[chat_ask kind=single ask_id=<uuid> value=<v>]` on line 1 of
    // the channel body.
    let preamble = if multi {
        format!(
            "[chat_ask kind=multi ask_id={} values={}]",
            ask_id, answers_str
        )
    } else {
        format!(
            "[chat_ask kind=single ask_id={} value={}]",
            ask_id, answers_str
        )
    };
    params.insert("content".into(), Value::String(preamble));

    let mut meta = serde_json::Map::new();
    // BIT-FOR-BIT inbound Telegram meta shape (see
    // build_channel_notification_telegram lines 244-273). chat_id /
    // message_id / user / user_id / ts are MANDATORY; thread_id and
    // target_agent_id are present-or-absent (no `null`).
    meta.insert(
        "chat_id".into(),
        Value::String(tg_meta.chat_id.to_string()),
    );
    meta.insert(
        "message_id".into(),
        Value::String(tg_meta.message_id_str.clone()),
    );
    meta.insert("user".into(), Value::String(tg_meta.user.clone()));
    meta.insert("user_id".into(), Value::String(tg_meta.user_id.clone()));
    meta.insert("ts".into(), Value::String(tg_meta.ts_iso8601.clone()));
    if let Some(tid) = tg_meta.thread_id {
        meta.insert("thread_id".into(), Value::String(tid.to_string()));
    }
    // AR-4 — dead-agent fallback. target_agent_id present iff alive
    // (drives src/plugin/bridge.rs::should_relay_channel_notification).
    if is_originator_alive {
        meta.insert(
            "target_agent_id".into(),
            Value::String(originating_agent_id.to_string()),
        );
    }

    params.insert("meta".into(), Value::Object(meta));
    json!({
        "jsonrpc": "2.0",
        "method": "notifications/claude/channel",
        "params": Value::Object(params),
    })
}

/// Slice 8 helper type — the answer payload the callback handler
/// passes to `build_channel_notification_callback_response`. Split-enum
/// distinguishes single-select (one value) from multi-select (array)
/// at the type level.
pub enum CallbackAnswer<'a> {
    Single(&'a str),
    Multi(&'a [String]),
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
    // Slice 1 of multi-agent-telegram-on-v0.6: additive routing-key
    // migration. Idempotent — re-runs are no-ops via pragma_table_info
    // probe + `IF NOT EXISTS` on the index. Tolerates pre-existing
    // v0.7/v0.8 leftover columns (additive-only, never drops).
    apply_routing_migration(conn)?;
    // Slice 8 — `pending_asks` table for `chat_ask` MCP tool. Additive,
    // idempotent. Architect AR-6: chat.db single-database discipline.
    crate::daemon::pending_asks::apply_pending_asks_migration(conn)?;
    // Slice 1 of cli-to-cli-routing — additive v5→v6 migration adding
    // the 5 cross-agent discovery / DND columns to `agent_registry`
    // plus a `project_id` lookup index. Idempotent via probe-before-ADD.
    apply_agent_registry_c2c_migration(conn)?;
    Ok(())
}

/// Slice 1 of multi-agent-telegram-on-v0.6 — additive migration adding
/// the per-CLI Telegram routing key columns and the partial-UNIQUE
/// expression-index that enforces the one-CLI-per-`(chat_id, thread_id)`
/// invariant (FR-MAT-2 of PRD §18 + KP1-KP3 acceptance).
///
/// The expression-index uses `COALESCE(routing_thread_id, -1)` so that
/// two DM rows with `routing_thread_id IS NULL` collide on the index
/// key (`-1`) — closes the red-team C2 SQLite-NULL-distinct bug.
///
/// The `routing_thread_id > 0` CHECK constraint reflects the
/// architect/security defense-in-depth recommendation: Telegram forum
/// `message_thread_id` is always positive in practice, so rejecting
/// non-positive values prevents the (theoretical) collision between
/// the `-1` index sentinel and a malformed Update.
///
/// Architect placed this in `chat.rs` (the v0.6 schema locus) rather
/// than a new `migrations.rs` module — disproportionate to introduce a
/// module pattern the project does not have for ~30 LOC of additive
/// ALTERs.
fn apply_routing_migration(conn: &Connection) -> rusqlite::Result<()> {
    // Each (column, type+constraints) pair. `DEFAULT NULL` explicit per
    // architect MINOR for clarity-of-intent even though it's the SQLite
    // ALTER TABLE ADD COLUMN default.
    //
    // routing_thread_id carries an inline CHECK constraint so any
    // future caller (e.g. a buggy Slice 2 implementer or a sqlite3 CLI
    // edit) cannot insert a non-positive value that would collide with
    // the COALESCE(-1) sentinel inside the index.
    //
    // chat.db is dev-machine local with no migrated users — see plan v4
    // R7. The probe-before-ADD pattern below still safely tolerates a
    // pre-existing v0.7/v0.8 leftover column with the same name (in
    // practice none of these names existed before this feature, but
    // the probe is the principled idempotency primitive either way).
    let columns: &[(&str, &str)] = &[
        ("routing_chat_id", "INTEGER DEFAULT NULL"),
        (
            "routing_thread_id",
            "INTEGER DEFAULT NULL CHECK (routing_thread_id IS NULL OR routing_thread_id > 0)",
        ),
        ("last_user_id", "INTEGER DEFAULT NULL"),
        ("host", "TEXT DEFAULT NULL"),
        ("cwd", "TEXT DEFAULT NULL"),
        ("pid", "INTEGER DEFAULT NULL"),
    ];

    // Wrap migration in its own transaction so a daemon crash mid-ALTER
    // leaves the schema either fully pre-migration or fully post — no
    // partial-state recovery path needed.
    conn.execute_batch("BEGIN")?;
    for (col, decl) in columns {
        let exists: bool = conn.query_row(
            "SELECT EXISTS(SELECT 1 FROM pragma_table_info('agent_registry') WHERE name = ?1)",
            [col],
            |row| row.get(0),
        )?;
        if !exists {
            // Column declarations are source literals (the slice above)
            // and contain NO user input, so the format!() is safe from
            // SQL injection. Wrapping the table+column names in a
            // parameterised query is not possible (SQLite parameters
            // bind values, never identifiers).
            conn.execute_batch(&format!(
                "ALTER TABLE agent_registry ADD COLUMN {col} {decl}"
            ))?;
        }
    }
    // Expression-index. `WHERE state='alive' AND routing_chat_id IS NOT NULL`
    // is load-bearing (security T8): without the IS NOT NULL clause every
    // legacy row (routing_chat_id default = NULL) would enter the index
    // at key `(NULL, COALESCE(NULL, -1)) = (NULL, -1)` and collide.
    conn.execute_batch(
        "CREATE UNIQUE INDEX IF NOT EXISTS agent_registry_routing_alive_uniq_idx \
         ON agent_registry(routing_chat_id, COALESCE(routing_thread_id, -1)) \
         WHERE state = 'alive' AND routing_chat_id IS NOT NULL",
    )?;
    conn.execute_batch("COMMIT")?;
    Ok(())
}

/// Slice 1 of cli-to-cli-routing — additive migration adding cross-agent
/// discovery and DND state columns to `agent_registry`, plus a `project_id`
/// lookup index. v5→v6 transition; idempotent via the same
/// `pragma_table_info` probe pattern used by `apply_routing_migration`.
///
/// New columns:
///   - `project_id TEXT` — normalized git remote URL (host/owner/repo) or
///     fallback per `src/project_id.rs` (Slice 2). Populated by Slice 3's
///     extended `agent_register` handler. Indexed for `--project current`
///     filter in Slice 6's `claudebase agent list-alive` CLI.
///   - `branch TEXT` — `git rev-parse --abbrev-ref HEAD` captured at
///     register time. Populated by Slice 3.
///   - `working_dir TEXT` — absolute cwd captured at register time.
///     Distinguishes per-clone agents that share a `project_id`.
///   - `feature_description TEXT` — operator-facing label set by Slice 3's
///     new `agent_describe` MCP tool. Mandated post-ExitPlanMode by the
///     Slice 7 hook.
///   - `dnd_until_ts INTEGER` — UNIX millis until which the agent is in
///     Do-Not-Disturb mode. NULL = no DND; `i64::MAX` = indefinite
///     (architect A-3 + OQ-UC-C2C-1 resolution). The Slice 5 drain task
///     scans for rows with `dnd_until_ts < now() AND dnd_until_ts IS NOT NULL`
///     so the `i64::MAX` sentinel is naturally excluded without special-case
///     code.
///
/// Index `agent_registry_project_id_idx ON agent_registry(project_id)`
/// supports the `WHERE project_id = ? AND project_id IS NOT NULL` filter
/// used by Slice 6's `list-alive --project current`. Legacy rows where the
/// `project_id` could not be derived (cwd-less pre-Slice-3 inserts) have
/// `project_id IS NULL` and surface only under `--project all` per
/// UC-C2C-1-EC3 backfill semantics.
fn apply_agent_registry_c2c_migration(conn: &Connection) -> rusqlite::Result<()> {
    let columns: &[(&str, &str)] = &[
        ("project_id", "TEXT DEFAULT NULL"),
        ("branch", "TEXT DEFAULT NULL"),
        ("working_dir", "TEXT DEFAULT NULL"),
        ("feature_description", "TEXT DEFAULT NULL"),
        ("dnd_until_ts", "INTEGER DEFAULT NULL"),
    ];

    conn.execute_batch("BEGIN")?;
    for (col, decl) in columns {
        let exists: bool = conn.query_row(
            "SELECT EXISTS(SELECT 1 FROM pragma_table_info('agent_registry') WHERE name = ?1)",
            [col],
            |row| row.get(0),
        )?;
        if !exists {
            // Column declarations are source literals (no user input),
            // safe to format!() into the ALTER TABLE statement — SQLite
            // parameters bind values, never identifiers.
            conn.execute_batch(&format!(
                "ALTER TABLE agent_registry ADD COLUMN {col} {decl}"
            ))?;
        }
    }
    conn.execute_batch(
        "CREATE INDEX IF NOT EXISTS agent_registry_project_id_idx \
         ON agent_registry(project_id)",
    )?;
    conn.execute_batch("COMMIT")?;
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
    // Slice 1 T5 hardening: restrict chat.db to user-only (0o600). The
    // file holds the bot token (in `daemon_state`) plus the new
    // `host`/`cwd`/`pid` process-metadata columns; on multi-user Linux
    // boxes the umask-default 0o644 would leak both. Parent dir is
    // already 0o700 from `server.rs::ensure_runtime_dir`. Windows ACLs
    // are user-owned by default; no equivalent chmod primitive — the
    // gate is `#[cfg(unix)]` per the security-auditor verdict.
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let perms = std::fs::Permissions::from_mode(0o600);
        std::fs::set_permissions(&path, perms)?;
    }
    ensure_chat_db_schema(&conn)?;
    Ok(conn)
}

/// Current wall-clock milliseconds since the UNIX epoch.
pub fn now_millis() -> i64 {
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

    // ---------------------------------------------------------------
    // Slice 1 of multi-agent-telegram-on-v0.6 — apply_routing_migration
    // tests. The 11 cases below cover:
    //
    //   (a) fresh DB has all 6 new columns + the new expression-index
    //   (b) idempotency: 2nd ensure_chat_db_schema call is a no-op
    //   (c) insert with a routing key triple succeeds and round-trips
    //   (d) two distinct routing keys coexist (different chat OR different topic)
    //   (e) duplicate (chat_id, non-NULL thread_id) raises UNIQUE
    //   (f) two topics in the SAME group both succeed (KP2/KP3 scenario)
    //   (g) C2 fix proof: duplicate (dm_chat_id, NULL) raises UNIQUE
    //   (h) i64::MIN / i64::MAX routing_chat_id round-trip cleanly
    //   (i) v0.6 register() API still works post-migration (architect i)
    //   (j) legacy partial-unique index agent_registry_thread_name_alive_idx
    //       survived the migration intact (architect j)
    //   (k) CHECK constraint rejects routing_thread_id=0 and routing_thread_id=-1
    //
    // All tests use raw INSERT against the migrated table — Slice 1
    // does NOT add a new Rust API (per architect: register() stays
    // unchanged, the routing-binding setter is Slice 2 work).
    // ---------------------------------------------------------------

    /// Convenience: insert a row directly into agent_registry with the
    /// fields the new-columns tests care about. Returns the rusqlite
    /// Result so error-path tests can introspect the SQLite error.
    fn raw_insert_routing(
        conn: &Connection,
        agent_id: &str,
        agent_name: &str,
        connection_id: &str,
        state: &str,
        routing_chat_id: Option<i64>,
        routing_thread_id: Option<i64>,
    ) -> rusqlite::Result<usize> {
        conn.execute(
            "INSERT INTO agent_registry \
             (agent_id, agent_name, connection_id, chat_thread_id, spawned_at, last_pinged_at, state, routing_chat_id, routing_thread_id) \
             VALUES (?1, ?2, ?3, NULL, 1, 1, ?4, ?5, ?6)",
            rusqlite::params![
                agent_id,
                agent_name,
                connection_id,
                state,
                routing_chat_id,
                routing_thread_id,
            ],
        )
    }

    #[test]
    fn slice1_a_fresh_db_has_new_columns_and_index() {
        let conn = fresh_db();
        // 6 new columns
        for col in [
            "routing_chat_id",
            "routing_thread_id",
            "last_user_id",
            "host",
            "cwd",
            "pid",
        ] {
            let exists: bool = conn
                .query_row(
                    "SELECT EXISTS(SELECT 1 FROM pragma_table_info('agent_registry') WHERE name = ?1)",
                    [col],
                    |r| r.get(0),
                )
                .unwrap();
            assert!(exists, "expected new column `{col}` to exist post-migration");
        }
        // Expression-index present and contains COALESCE(routing_thread_id, -1)
        let sql: String = conn
            .query_row(
                "SELECT sql FROM sqlite_master WHERE name='agent_registry_routing_alive_uniq_idx'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert!(
            sql.contains("COALESCE(routing_thread_id, -1)"),
            "expected COALESCE sentinel in index sql, got: {sql}"
        );
        assert!(
            sql.contains("routing_chat_id IS NOT NULL"),
            "expected IS NOT NULL filter in index sql (security T8), got: {sql}"
        );
    }

    #[test]
    fn slice1_b_migration_is_idempotent() {
        let conn = fresh_db();
        // The 1st ensure already ran via fresh_db(). Run a 2nd one and
        // confirm it returns Ok without ADD-COLUMN duplicate errors.
        ensure_chat_db_schema(&conn).expect("2nd ensure should be no-op");
        // And a 3rd, just to be sure.
        ensure_chat_db_schema(&conn).expect("3rd ensure should be no-op");
        // Column count for agent_registry hasn't grown beyond expected.
        let n: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM pragma_table_info('agent_registry')",
                [],
                |r| r.get(0),
            )
            .unwrap();
        // Base v5 schema: 9 columns. apply_routing_migration adds 6.
        // Slice 1 of cli-to-cli-routing's apply_agent_registry_c2c_migration
        // adds 5 more (project_id / branch / working_dir /
        // feature_description / dnd_until_ts). Total = 9 + 6 + 5 = 20.
        assert_eq!(n, 20, "agent_registry should have exactly 20 columns post-migration");
    }

    #[test]
    fn slice1_c_insert_with_routing_key_round_trips() {
        let conn = fresh_db();
        raw_insert_routing(&conn, "a1", "alice", "c1", "alive", Some(100), Some(7))
            .expect("insert with routing key");
        let (cid, tid): (i64, i64) = conn
            .query_row(
                "SELECT routing_chat_id, routing_thread_id FROM agent_registry WHERE agent_id='a1'",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert_eq!(cid, 100);
        assert_eq!(tid, 7);
    }

    #[test]
    fn slice1_d_two_distinct_routing_keys_coexist() {
        let conn = fresh_db();
        // Different chat_id → distinct rows
        raw_insert_routing(&conn, "a1", "alice", "c1", "alive", Some(100), None).unwrap();
        raw_insert_routing(&conn, "a2", "bob", "c2", "alive", Some(200), None)
            .expect("different chat_id should not collide");
        let n: i64 = conn
            .query_row("SELECT COUNT(*) FROM agent_registry WHERE state='alive'", [], |r| r.get(0))
            .unwrap();
        assert_eq!(n, 2);
    }

    #[test]
    fn slice1_e_duplicate_non_null_thread_raises_unique() {
        let conn = fresh_db();
        raw_insert_routing(&conn, "a1", "alice", "c1", "alive", Some(100), Some(7)).unwrap();
        let err = raw_insert_routing(&conn, "a2", "bob", "c2", "alive", Some(100), Some(7))
            .expect_err("duplicate (chat, thread) must violate UNIQUE");
        assert!(
            err.to_string().contains("UNIQUE"),
            "expected UNIQUE-constraint violation, got: {err}"
        );
    }

    #[test]
    fn slice1_f_same_group_two_topics_both_succeed() {
        let conn = fresh_db();
        // KP2/KP3 scenario: same group chat_id, topic α (7) and topic β (8).
        raw_insert_routing(&conn, "a-b", "bob", "c1", "alive", Some(500), Some(7))
            .expect("topic α insert");
        raw_insert_routing(&conn, "a-c", "carol", "c2", "alive", Some(500), Some(8))
            .expect("topic β insert (different thread)");
        let n: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM agent_registry WHERE routing_chat_id=500 AND state='alive'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(n, 2);
    }

    #[test]
    fn slice1_g_c2_fix_null_null_dm_collision_raises_unique() {
        let conn = fresh_db();
        // KP1 DM-collision: two CLIs both try to bind to the same DM
        // chat with no topic. Pre-C2-fix this would silently succeed
        // because SQLite treats NULL as DISTINCT in UNIQUE constraints.
        // The COALESCE(routing_thread_id, -1) expression-index closes
        // that hole — second insert MUST raise UNIQUE.
        raw_insert_routing(&conn, "a1", "alice", "c1", "alive", Some(42), None).unwrap();
        let err = raw_insert_routing(&conn, "a2", "bob", "c2", "alive", Some(42), None)
            .expect_err("C2 fix proof: duplicate (chat, NULL) must violate UNIQUE");
        assert!(
            err.to_string().contains("UNIQUE"),
            "expected UNIQUE violation for NULL-NULL DM collision, got: {err}"
        );
    }

    #[test]
    fn slice1_h_i64_extremes_round_trip() {
        let conn = fresh_db();
        raw_insert_routing(&conn, "a-min", "x", "c1", "alive", Some(i64::MIN), Some(1)).unwrap();
        raw_insert_routing(&conn, "a-max", "y", "c2", "alive", Some(i64::MAX), Some(1)).unwrap();
        let (min_back, max_back): (i64, i64) = conn
            .query_row(
                "SELECT \
                 (SELECT routing_chat_id FROM agent_registry WHERE agent_id='a-min'), \
                 (SELECT routing_chat_id FROM agent_registry WHERE agent_id='a-max')",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert_eq!(min_back, i64::MIN);
        assert_eq!(max_back, i64::MAX);
    }

    #[test]
    fn slice1_i_v06_register_api_still_works_post_migration() {
        // Architect's added test (i): the legacy register() function
        // signature is untouched by Slice 1; verify a register round-trip
        // still works against the migrated schema. The new columns stay
        // at their NULL default for the rows the v0.6 API inserts.
        let conn = fresh_db();
        crate::daemon::agent_registry::register(
            &conn,
            "legacy-a",
            "legacy",
            "conn-x",
            Some("telegram:thread-1"),
            None,
        )
        .expect("legacy v0.6 register() round-trip must still work");
        // New columns are NULL on a row inserted via the legacy API.
        let (rcid, rtid): (Option<i64>, Option<i64>) = conn
            .query_row(
                "SELECT routing_chat_id, routing_thread_id FROM agent_registry WHERE agent_id='legacy-a'",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert!(rcid.is_none(), "legacy insert should leave routing_chat_id NULL");
        assert!(rtid.is_none(), "legacy insert should leave routing_thread_id NULL");
    }

    #[test]
    fn slice1_j_legacy_index_survives_migration() {
        // Architect's added test (j): the migration does not drop the
        // legacy partial-unique index agent_registry_thread_name_alive_idx.
        let conn = fresh_db();
        let n: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM sqlite_master \
                 WHERE type='index' AND name='agent_registry_thread_name_alive_idx'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(n, 1, "legacy partial-unique index must survive the migration");
        // And the other legacy index too.
        let n: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM sqlite_master \
                 WHERE type='index' AND name='agent_registry_thread_alive_idx'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(n, 1, "legacy routing-lookup index must survive the migration");
    }

    #[test]
    fn slice1_k_check_constraint_rejects_non_positive_thread_id() {
        // Defense-in-depth: routing_thread_id = 0 or -1 must be rejected
        // at the DB layer so a malformed Update (or buggy caller) cannot
        // collide with the COALESCE(-1) sentinel inside the expression
        // index.
        let conn = fresh_db();
        let err_zero = raw_insert_routing(&conn, "a-0", "x", "c1", "alive", Some(7), Some(0))
            .expect_err("routing_thread_id=0 must fail CHECK");
        assert!(
            err_zero.to_string().contains("CHECK"),
            "expected CHECK-constraint violation for thread_id=0, got: {err_zero}"
        );
        let err_neg = raw_insert_routing(&conn, "a-neg", "y", "c2", "alive", Some(7), Some(-1))
            .expect_err("routing_thread_id=-1 must fail CHECK");
        assert!(
            err_neg.to_string().contains("CHECK"),
            "expected CHECK-constraint violation for thread_id=-1, got: {err_neg}"
        );
        // Sanity: positive thread_id still works.
        raw_insert_routing(&conn, "a-pos", "z", "c3", "alive", Some(7), Some(1))
            .expect("routing_thread_id=1 should succeed");
    }

    // ---------------------------------------------------------------
    // Slice 2 of multi-agent-telegram-on-v0.6 —
    // build_channel_notification_telegram thread_id additive-field
    // emission tests.
    //
    // Per PRD §18 FR-MAT-7 (C3 wire contract): when the inbound message
    // carries a forum-topic id, the channel notification meta object
    // gains a `thread_id` string field. When None (DM / topic-less),
    // the field is OMITTED so DM inbound stays bit-for-bit identical
    // to the Slice 0 baseline meta shape.
    // ---------------------------------------------------------------

    fn slice2_sample_tg_meta(thread_id: Option<i64>) -> TelegramMessageMeta {
        TelegramMessageMeta {
            chat_id: 42,
            message_id_str: "100".to_string(),
            user: "alice".to_string(),
            user_id: "8791871989".to_string(),
            ts_iso8601: "2026-06-03T00:00:00.000Z".to_string(),
            thread_id,
        }
    }

    #[test]
    fn slice2_meta_emits_thread_id_when_some() {
        let tg_meta = slice2_sample_tg_meta(Some(7));
        let frame = build_channel_notification_telegram("hi", &tg_meta, None);
        let meta = frame
            .pointer("/params/meta")
            .and_then(|v| v.as_object())
            .expect("meta object");
        let thread_id_value = meta
            .get("thread_id")
            .expect("meta.thread_id present when Some");
        assert_eq!(
            thread_id_value.as_str(),
            Some("7"),
            "thread_id must be emitted as string per v0.6 ID-as-string discipline"
        );
    }

    #[test]
    fn slice2_meta_omits_thread_id_when_none() {
        let tg_meta = slice2_sample_tg_meta(None);
        let frame = build_channel_notification_telegram("hi", &tg_meta, None);
        let meta = frame
            .pointer("/params/meta")
            .and_then(|v| v.as_object())
            .expect("meta object");
        assert!(
            !meta.contains_key("thread_id"),
            "meta.thread_id MUST be omitted (not set to null) when None — \
             preserves Slice 0 baseline shape for DM / topic-less inbound"
        );
    }

    #[test]
    fn slice2_meta_thread_id_independent_of_target_agent_id() {
        // Both routing-key (thread_id) and @-mention (target_agent_id)
        // can coexist in the same meta. This proves additive layering.
        let tg_meta = slice2_sample_tg_meta(Some(99));
        let frame = build_channel_notification_telegram("@bob hi", &tg_meta, Some("bob-uuid"));
        let meta = frame.pointer("/params/meta").unwrap().as_object().unwrap();
        assert_eq!(meta.get("thread_id").unwrap().as_str(), Some("99"));
        assert_eq!(
            meta.get("target_agent_id").unwrap().as_str(),
            Some("bob-uuid")
        );
        // Existing fields still there
        assert_eq!(meta.get("user").unwrap().as_str(), Some("alice"));
        assert_eq!(meta.get("user_id").unwrap().as_str(), Some("8791871989"));
    }

    // ----------------------------------------------------------------
    // Slice 8 — build_channel_notification_callback_response
    // ----------------------------------------------------------------

    fn cb_tg_meta_stub() -> TelegramMessageMeta {
        TelegramMessageMeta {
            chat_id: 100,
            message_id_str: "200".to_string(),
            user: "alice".to_string(),
            user_id: "300".to_string(),
            ts_iso8601: "2026-06-04T00:00:00.000Z".to_string(),
            thread_id: None,
        }
    }

    #[test]
    fn slice8_callback_single_alive_originator_emits_target_agent_id() {
        let frame = build_channel_notification_callback_response(
            "ask-uuid-1",
            CallbackAnswer::Single("yes"),
            "mira",
            true, // alive
            "Approve plan?",
            r#"[{"label":"Yes","value":"yes"},{"label":"No","value":"no"}]"#,
            false,
            &cb_tg_meta_stub(),
        );
        let meta = frame.pointer("/params/meta").unwrap().as_object().unwrap();
        // Iteration 2 — meta is BIT-FOR-BIT inbound shape; Slice 8 data
        // lives in `content` preamble, NOT in meta. Asserting on the
        // absence of the extras catches accidental regression to the
        // verbose meta shape that CC's renderer dropped.
        assert!(!meta.contains_key("is_callback_response"));
        assert!(!meta.contains_key("ask_id"));
        assert!(!meta.contains_key("value"));
        assert!(!meta.contains_key("values"));
        assert!(!meta.contains_key("question"));
        assert!(!meta.contains_key("options"));
        assert!(!meta.contains_key("originating_agent_id"));
        // Required inbound-shape meta keys — strings, no nulls.
        assert_eq!(meta.get("chat_id").unwrap().as_str(), Some("100"));
        assert_eq!(meta.get("message_id").unwrap().as_str(), Some("200"));
        assert_eq!(meta.get("user").unwrap().as_str(), Some("alice"));
        assert_eq!(meta.get("user_id").unwrap().as_str(), Some("300"));
        assert_eq!(meta.get("ts").unwrap().as_str(), Some("2026-06-04T00:00:00.000Z"));
        // AR-4 alive → target_agent_id present (bridge filter gate).
        assert_eq!(meta.get("target_agent_id").unwrap().as_str(), Some("mira"));
        // content preamble carries Slice 8 round-trip data.
        let content = frame.pointer("/params/content").unwrap().as_str().unwrap();
        assert_eq!(content, "[chat_ask kind=single ask_id=ask-uuid-1 value=yes]");
    }

    #[test]
    fn slice8_callback_single_dead_originator_omits_target_agent_id() {
        let frame = build_channel_notification_callback_response(
            "ask-uuid-2",
            CallbackAnswer::Single("no"),
            "ghost",
            false, // NOT alive — AR-4 fallback
            "?",
            "[]",
            false,
            &cb_tg_meta_stub(),
        );
        let meta = frame.pointer("/params/meta").unwrap().as_object().unwrap();
        assert!(
            !meta.contains_key("target_agent_id"),
            "AR-4: target_agent_id MUST be omitted when originator is not alive"
        );
        // Dead originator does NOT leak via originating_agent_id either —
        // iteration 2 stripped it from meta. Round-trip context lives
        // exclusively in content; the ask_id is the breadcrumb a fresh
        // Mira can use to chat_list_pending_asks for older context.
        assert!(!meta.contains_key("originating_agent_id"));
        let content = frame.pointer("/params/content").unwrap().as_str().unwrap();
        assert_eq!(content, "[chat_ask kind=single ask_id=ask-uuid-2 value=no]");
    }

    #[test]
    fn slice8_callback_multi_emits_values_in_content_preamble() {
        let values = vec!["a".to_string(), "c".to_string()];
        let frame = build_channel_notification_callback_response(
            "ask-uuid-3",
            CallbackAnswer::Multi(&values),
            "mira",
            true,
            "Pick:",
            r#"[{"label":"A","value":"a"},{"label":"B","value":"b"},{"label":"C","value":"c"}]"#,
            true,
            &cb_tg_meta_stub(),
        );
        // values gone from meta — now CSV in content preamble.
        let meta = frame.pointer("/params/meta").unwrap().as_object().unwrap();
        assert!(!meta.contains_key("values"));
        assert!(!meta.contains_key("multi"));
        let content = frame.pointer("/params/content").unwrap().as_str().unwrap();
        assert_eq!(content, "[chat_ask kind=multi ask_id=ask-uuid-3 values=a,c]");
    }

    #[test]
    fn slice8_callback_multi_empty_values_renders_as_empty_csv() {
        // Done with no selections — empty list path. Preamble shape MUST
        // stay parseable (trailing `=`, not `values=,`).
        let empty: Vec<String> = Vec::new();
        let frame = build_channel_notification_callback_response(
            "ask-uuid-7",
            CallbackAnswer::Multi(&empty),
            "mira",
            true,
            "?",
            "[]",
            true,
            &cb_tg_meta_stub(),
        );
        let content = frame.pointer("/params/content").unwrap().as_str().unwrap();
        assert_eq!(content, "[chat_ask kind=multi ask_id=ask-uuid-7 values=]");
    }

    #[test]
    fn slice8_callback_frame_method_matches_baseline_contract() {
        // Slice 0 baseline preservation: the notification method name is
        // `notifications/claude/channel` (same as regular TG messages).
        let frame = build_channel_notification_callback_response(
            "ask-uuid-5",
            CallbackAnswer::Single("x"),
            "mira",
            true,
            "?",
            "[]",
            false,
            &cb_tg_meta_stub(),
        );
        assert_eq!(
            frame.get("method").unwrap().as_str(),
            Some("notifications/claude/channel")
        );
    }

    #[test]
    fn slice8b_callback_meta_includes_thread_id_when_present() {
        // Slice 2 thread_id additive — same shape as inbound TG meta.
        let mut tg_meta = cb_tg_meta_stub();
        tg_meta.thread_id = Some(42);
        let frame = build_channel_notification_callback_response(
            "ask-uuid-6",
            CallbackAnswer::Single("x"),
            "mira",
            true,
            "?",
            "[]",
            false,
            &tg_meta,
        );
        let meta = frame.pointer("/params/meta").unwrap().as_object().unwrap();
        assert_eq!(meta.get("thread_id").unwrap().as_str(), Some("42"));
    }
}
