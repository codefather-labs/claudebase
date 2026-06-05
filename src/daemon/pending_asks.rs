//! `pending_asks` — Slice 8 of multi-agent-telegram-on-v0.6.
//!
//! Persistent state for in-flight `chat_ask` MCP tool requests. A row is
//! INSERTed AFTER the daemon successfully sends the inline-keyboard
//! Telegram message (send-then-insert ordering per AR-1; no orphan rows
//! if `sendMessage` fails). The row is DELETEd when the operator's
//! callback finalizes the ask:
//!   - single-select: any button tap.
//!   - multi-select: tap on the "Done" button.
//! Abandoned asks (no tap within TTL=24h) are GC'd by `gc_expired`,
//! which the long-poll calls at every batch tail. The architect-finalized
//! schema lives at the SQL string constant `SCHEMA_V8_PENDING_ASKS` below.
//!
//! ## Where this lives
//!
//! `chat.db` (alongside `chat_threads`, `chat_messages`, `agent_registry`,
//! `daemon_state`) — same 0o600 security perimeter, same single-connection
//! discipline, single-file backup story. Cross-table JOIN with
//! `agent_registry` for the AR-4 dead-originating-agent alive-check is
//! a same-connection same-database SELECT.
//!
//! ## Open-ask definition
//!
//! A row is OPEN iff it is present in the table AND `expires_at > now()`.
//! Multi-select rows where the operator has toggled options but not yet
//! tapped Done are STILL OPEN (their `selected_values_json` is non-NULL
//! but the row is not deleted) — see `list_open` predicate. The qa-planner
//! v1 draft of TC-CHA-9 used `selected_values_json IS NULL` which would
//! have excluded partially-toggled multi-selects; the predicate here is
//! `expires_at > now()` only, per ba-analyst OQ-MAT-UC-5 resolution.

use anyhow::{Context, Result};
use rusqlite::{params, Connection, OptionalExtension};

/// Schema version constant for the pending_asks table. Single statement
/// `CREATE TABLE IF NOT EXISTS` keeps the migration idempotent. The
/// trailing index supports the GC predicate.
const SCHEMA_V8_PENDING_ASKS: &str = r#"
CREATE TABLE IF NOT EXISTS pending_asks (
    ask_id              TEXT PRIMARY KEY,
    chat_id             INTEGER NOT NULL,
    message_thread_id   INTEGER NULL CHECK (
        message_thread_id IS NULL OR message_thread_id > 0
    ),
    message_id          INTEGER NOT NULL,
    requesting_agent_id TEXT NOT NULL,
    question            TEXT NOT NULL,
    options_json        TEXT NOT NULL,
    multi               INTEGER NOT NULL DEFAULT 0 CHECK (multi IN (0, 1)),
    selected_values_json TEXT NULL,
    created_at          INTEGER NOT NULL,
    expires_at          INTEGER NOT NULL
);
CREATE INDEX IF NOT EXISTS pending_asks_expires_idx
    ON pending_asks(expires_at);
"#;

/// AR-6 TTL — 24 hours in milliseconds. A `chat_ask` whose tap does
/// not arrive within this window is GC'd. Hardcoded per operator
/// decision 2026-06-04 (no `urgency` parameter for Slice 8 MVP).
pub const DEFAULT_TTL_MS: i64 = 24 * 60 * 60 * 1000;

/// Apply the Slice 8 schema additive migration. Idempotent — re-runs
/// are no-ops via `IF NOT EXISTS`. Called from
/// `chat::ensure_chat_db_schema` AFTER `apply_routing_migration` per
/// architect AR-6.
pub fn apply_pending_asks_migration(conn: &Connection) -> rusqlite::Result<()> {
    conn.execute_batch(SCHEMA_V8_PENDING_ASKS)?;
    Ok(())
}

/// One row in `pending_asks`. The `selected_values_json` is `None` for
/// freshly-INSERTed asks (single-select) and remains `None` for
/// multi-select asks UNTIL the operator's first toggle, after which it
/// holds the JSON-encoded array of selected option values.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PendingAsk {
    pub ask_id: String,
    pub chat_id: i64,
    pub message_thread_id: Option<i64>,
    pub message_id: i64,
    pub requesting_agent_id: String,
    pub question: String,
    pub options_json: String,
    pub multi: bool,
    pub selected_values_json: Option<String>,
    pub created_at: i64,
    pub expires_at: i64,
}

/// INSERT a fresh ask row. The `message_id` MUST come from the daemon's
/// successful `sendMessage` round-trip (send-then-insert per AR-1) so
/// orphans never enter the table. Same-`ask_id` collision is a SQL
/// PRIMARY-KEY violation; callers MUST use a freshly-generated UUID v4
/// (architect SEC requirement: ask_id is unguessable to prevent
/// callback response-injection).
pub fn insert_pending(conn: &Connection, ask: &PendingAsk) -> Result<()> {
    conn.execute(
        "INSERT INTO pending_asks \
         (ask_id, chat_id, message_thread_id, message_id, requesting_agent_id, \
          question, options_json, multi, selected_values_json, created_at, expires_at) \
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)",
        params![
            ask.ask_id,
            ask.chat_id,
            ask.message_thread_id,
            ask.message_id,
            ask.requesting_agent_id,
            ask.question,
            ask.options_json,
            i64::from(ask.multi),
            ask.selected_values_json,
            ask.created_at,
            ask.expires_at,
        ],
    )
    .context("insert pending_asks row")?;
    Ok(())
}

/// Fetch a single ask by id. Returns `None` when the row is absent
/// (most callers tolerate this — an unknown ask_id from a callback
/// data string is treated as "silently dropped" per FR-MAT-11.5).
pub fn get_pending(conn: &Connection, ask_id: &str) -> Result<Option<PendingAsk>> {
    let mut stmt = conn.prepare(
        "SELECT ask_id, chat_id, message_thread_id, message_id, requesting_agent_id, \
                question, options_json, multi, selected_values_json, created_at, expires_at \
         FROM pending_asks WHERE ask_id = ?1",
    )?;
    let row_opt = stmt
        .query_row(params![ask_id], |row| {
            let multi_int: i64 = row.get(7)?;
            Ok(PendingAsk {
                ask_id: row.get(0)?,
                chat_id: row.get(1)?,
                message_thread_id: row.get(2)?,
                message_id: row.get(3)?,
                requesting_agent_id: row.get(4)?,
                question: row.get(5)?,
                options_json: row.get(6)?,
                multi: multi_int != 0,
                selected_values_json: row.get(8)?,
                created_at: row.get(9)?,
                expires_at: row.get(10)?,
            })
        })
        .optional()
        .context("query pending_asks row by id")?;
    Ok(row_opt)
}

/// DELETE the row that finalized a callback flow (any tap on a
/// single-select; Done tap on a multi-select). Returns the number of
/// rows removed (0 when the row was already gone — concurrent callback,
/// or GC won the race). Idempotent.
pub fn delete_pending(conn: &Connection, ask_id: &str) -> Result<usize> {
    let n = conn
        .execute(
            "DELETE FROM pending_asks WHERE ask_id = ?1",
            params![ask_id],
        )
        .context("delete pending_asks row")?;
    Ok(n)
}

/// AR-7 atomic toggle for Slice 8b (multi-select). Rewrites
/// `selected_values_json` and returns the post-write state in a single
/// SQLite statement so the caller's `editMessageReplyMarkup` redraw uses
/// the value the row actually holds — no read-then-write race.
/// Returns `None` when the row is absent (unknown ask_id).
pub fn update_selected_values(
    conn: &Connection,
    ask_id: &str,
    new_values_json: &str,
) -> Result<Option<String>> {
    let updated = conn
        .query_row(
            "UPDATE pending_asks SET selected_values_json = ?2 \
             WHERE ask_id = ?1 \
             RETURNING selected_values_json",
            params![ask_id, new_values_json],
            |row| row.get::<_, String>(0),
        )
        .optional()
        .context("update_selected_values RETURNING clause")?;
    Ok(updated)
}

/// Slice 8c (`chat_list_pending_asks` MCP tool) — list open asks
/// for the debugging surface. Open = `expires_at > now()` REGARDLESS
/// of `selected_values_json`; a partially-toggled multi-select is
/// still open until Done or expiry.
pub fn list_open(
    conn: &Connection,
    now_ms: i64,
    agent_id_filter: Option<&str>,
    chat_id_filter: Option<i64>,
) -> Result<Vec<PendingAsk>> {
    let mut sql = String::from(
        "SELECT ask_id, chat_id, message_thread_id, message_id, requesting_agent_id, \
                question, options_json, multi, selected_values_json, created_at, expires_at \
         FROM pending_asks WHERE expires_at > ?1",
    );
    if agent_id_filter.is_some() {
        sql.push_str(" AND requesting_agent_id = ?2");
    }
    if chat_id_filter.is_some() {
        let pos = if agent_id_filter.is_some() { 3 } else { 2 };
        sql.push_str(&format!(" AND chat_id = ?{}", pos));
    }
    sql.push_str(" ORDER BY created_at ASC");

    let mut stmt = conn.prepare(&sql)?;
    let mapper = |row: &rusqlite::Row| -> rusqlite::Result<PendingAsk> {
        let multi_int: i64 = row.get(7)?;
        Ok(PendingAsk {
            ask_id: row.get(0)?,
            chat_id: row.get(1)?,
            message_thread_id: row.get(2)?,
            message_id: row.get(3)?,
            requesting_agent_id: row.get(4)?,
            question: row.get(5)?,
            options_json: row.get(6)?,
            multi: multi_int != 0,
            selected_values_json: row.get(8)?,
            created_at: row.get(9)?,
            expires_at: row.get(10)?,
        })
    };
    let rows: Vec<PendingAsk> = match (agent_id_filter, chat_id_filter) {
        (None, None) => stmt
            .query_map(params![now_ms], mapper)?
            .collect::<rusqlite::Result<Vec<_>>>()?,
        (Some(aid), None) => stmt
            .query_map(params![now_ms, aid], mapper)?
            .collect::<rusqlite::Result<Vec<_>>>()?,
        (None, Some(cid)) => stmt
            .query_map(params![now_ms, cid], mapper)?
            .collect::<rusqlite::Result<Vec<_>>>()?,
        (Some(aid), Some(cid)) => stmt
            .query_map(params![now_ms, aid, cid], mapper)?
            .collect::<rusqlite::Result<Vec<_>>>()?,
    };
    Ok(rows)
}

/// GC predicate runs at every long-poll batch tail per AR-6. Removes
/// abandoned asks (any row whose `expires_at < now_ms` regardless of
/// whether it has selected values — operators sometimes toggle a few
/// options then walk away).
pub fn gc_expired(conn: &Connection, now_ms: i64) -> Result<usize> {
    let n = conn
        .execute(
            "DELETE FROM pending_asks WHERE expires_at < ?1",
            params![now_ms],
        )
        .context("gc_expired DELETE")?;
    Ok(n)
}

/// FR-MAT-11.9 — the `callback_data` budget the chat_ask handler enforces
/// at REQUEST time. Telegram limits `callback_data` to 1-64 bytes UTF-8
/// (per https://core.telegram.org/bots/api#inlinekeyboardbutton). The
/// daemon's data formats are:
///   single-select: `<ask_id>:<value>`            (overhead 1 byte)
///   multi-select:  `<ask_id>:toggle:<option_id>` (overhead 9 bytes)
///                  `<ask_id>:done`               (overhead 6 bytes)
/// With a 36-byte UUID ask_id, the option-value budget is 27 bytes
/// (single-select) or 20 bytes (multi-select). Returns Err with a
/// human-actionable message that the caller surfaces as MCP `-32602`.
pub fn validate_callback_data_budget(
    ask_id_len: usize,
    multi: bool,
    options: &[(String, String)],
) -> Result<()> {
    let overhead = if multi {
        // <ask_id>:toggle:<option_id>
        ask_id_len + ":toggle:".len()
    } else {
        // <ask_id>:<value>
        ask_id_len + ":".len()
    };
    for (label, value) in options {
        let total = overhead + value.as_bytes().len();
        if total > 64 {
            anyhow::bail!(
                "callback_data budget exceeded for option \"{}\" (value={:?}, \
                 total={} > 64 bytes); shorten value to <= {} bytes",
                label,
                value,
                total,
                64usize.saturating_sub(overhead),
            );
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use rusqlite::Connection;

    fn fresh_conn() -> Connection {
        let conn = Connection::open_in_memory().unwrap();
        apply_pending_asks_migration(&conn).unwrap();
        conn
    }

    fn sample_ask(id: &str, agent: &str, now: i64) -> PendingAsk {
        PendingAsk {
            ask_id: id.to_string(),
            chat_id: 8791871989,
            message_thread_id: None,
            message_id: 123,
            requesting_agent_id: agent.to_string(),
            question: "Approve plan?".to_string(),
            options_json: r#"[{"label":"Yes","value":"yes"},{"label":"No","value":"no"}]"#
                .to_string(),
            multi: false,
            selected_values_json: None,
            created_at: now,
            expires_at: now + DEFAULT_TTL_MS,
        }
    }

    #[test]
    fn migration_is_idempotent() {
        let conn = Connection::open_in_memory().unwrap();
        apply_pending_asks_migration(&conn).unwrap();
        apply_pending_asks_migration(&conn).unwrap(); // re-run = no-op
        apply_pending_asks_migration(&conn).unwrap();
        let cnt: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM sqlite_master \
                 WHERE name = 'pending_asks' AND type = 'table'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(cnt, 1);
    }

    #[test]
    fn insert_then_get_round_trip() {
        let conn = fresh_conn();
        let ask = sample_ask("ask-1", "mira", 1_000_000);
        insert_pending(&conn, &ask).unwrap();
        let fetched = get_pending(&conn, "ask-1").unwrap().expect("must be found");
        assert_eq!(fetched, ask);
    }

    #[test]
    fn get_pending_returns_none_for_unknown_id() {
        let conn = fresh_conn();
        assert!(get_pending(&conn, "nope").unwrap().is_none());
    }

    #[test]
    fn delete_pending_is_idempotent() {
        let conn = fresh_conn();
        let ask = sample_ask("ask-2", "mira", 1_000_000);
        insert_pending(&conn, &ask).unwrap();
        assert_eq!(delete_pending(&conn, "ask-2").unwrap(), 1);
        assert_eq!(delete_pending(&conn, "ask-2").unwrap(), 0); // already gone
        assert!(get_pending(&conn, "ask-2").unwrap().is_none());
    }

    #[test]
    fn update_selected_values_returns_post_state() {
        let conn = fresh_conn();
        let mut ask = sample_ask("ask-3", "mira", 1_000_000);
        ask.multi = true;
        insert_pending(&conn, &ask).unwrap();
        let post = update_selected_values(&conn, "ask-3", r#"["yes"]"#)
            .unwrap()
            .expect("must return the row");
        assert_eq!(post, r#"["yes"]"#);
        let post2 = update_selected_values(&conn, "ask-3", r#"["yes","maybe"]"#)
            .unwrap()
            .expect("must return the row");
        assert_eq!(post2, r#"["yes","maybe"]"#);
    }

    #[test]
    fn update_selected_values_returns_none_for_unknown_id() {
        let conn = fresh_conn();
        assert!(update_selected_values(&conn, "ghost", r#"["x"]"#)
            .unwrap()
            .is_none());
    }

    #[test]
    fn gc_expired_drops_only_past_due_rows() {
        let conn = fresh_conn();
        let now = 5_000_000_000_i64;
        let fresh = sample_ask("fresh", "mira", now);
        let mut stale = sample_ask("stale", "mira", now - 2 * DEFAULT_TTL_MS);
        stale.expires_at = now - 1; // explicitly past due
        insert_pending(&conn, &fresh).unwrap();
        insert_pending(&conn, &stale).unwrap();
        let removed = gc_expired(&conn, now).unwrap();
        assert_eq!(removed, 1);
        assert!(get_pending(&conn, "fresh").unwrap().is_some());
        assert!(get_pending(&conn, "stale").unwrap().is_none());
    }

    #[test]
    fn list_open_returns_only_unexpired_rows_regardless_of_toggle_state() {
        // OQ-MAT-UC-5 resolution: partially-toggled multi-select rows
        // are STILL OPEN. list_open uses `expires_at > now` only.
        let conn = fresh_conn();
        let now = 1_000_000;
        // a: single-select, untoggled, open
        insert_pending(&conn, &sample_ask("a", "mira", now)).unwrap();
        // b: multi-select, PARTIALLY TOGGLED, still open
        let mut b = sample_ask("b", "mira", now);
        b.multi = true;
        b.selected_values_json = Some(r#"["yes"]"#.to_string());
        insert_pending(&conn, &b).unwrap();
        // c: expired
        let mut c = sample_ask("c", "mira", now);
        c.expires_at = now - 1;
        insert_pending(&conn, &c).unwrap();
        let open = list_open(&conn, now, None, None).unwrap();
        let ids: Vec<&str> = open.iter().map(|p| p.ask_id.as_str()).collect();
        assert!(ids.contains(&"a"), "single-select untoggled must be open");
        assert!(
            ids.contains(&"b"),
            "multi-select PARTIALLY toggled must still be open"
        );
        assert!(!ids.contains(&"c"), "expired row must NOT be open");
    }

    #[test]
    fn list_open_filter_by_agent_id() {
        let conn = fresh_conn();
        let now = 1_000_000;
        insert_pending(&conn, &sample_ask("a", "mira", now)).unwrap();
        insert_pending(&conn, &sample_ask("b", "fbscout", now)).unwrap();
        let only_mira = list_open(&conn, now, Some("mira"), None).unwrap();
        assert_eq!(only_mira.len(), 1);
        assert_eq!(only_mira[0].requesting_agent_id, "mira");
    }

    #[test]
    fn list_open_filter_by_chat_id() {
        let conn = fresh_conn();
        let now = 1_000_000;
        let mut a = sample_ask("a", "mira", now);
        a.chat_id = 100;
        let mut b = sample_ask("b", "mira", now);
        b.chat_id = 200;
        insert_pending(&conn, &a).unwrap();
        insert_pending(&conn, &b).unwrap();
        let only_100 = list_open(&conn, now, None, Some(100)).unwrap();
        assert_eq!(only_100.len(), 1);
        assert_eq!(only_100[0].chat_id, 100);
    }

    #[test]
    fn validate_callback_data_budget_single_select_passes_short_value() {
        let opts = vec![
            ("Yes".to_string(), "yes".to_string()),
            ("No".to_string(), "no".to_string()),
        ];
        validate_callback_data_budget(36, false, &opts).expect("short single values OK");
    }

    #[test]
    fn validate_callback_data_budget_single_select_rejects_overflow() {
        // 36-byte uuid + ":" + 28-byte value = 65 bytes > 64 → reject
        let oversized: String = "a".repeat(28);
        let opts = vec![("Long".to_string(), oversized)];
        let err = validate_callback_data_budget(36, false, &opts).unwrap_err();
        assert!(
            err.to_string().contains("callback_data budget exceeded"),
            "got: {err}"
        );
    }

    #[test]
    fn validate_callback_data_budget_multi_select_tight_path() {
        // 36-byte uuid + ":toggle:" (8) + 20-byte value = 64 bytes, exactly fits.
        let twenty: String = "x".repeat(20);
        let opts = vec![("ok".to_string(), twenty)];
        validate_callback_data_budget(36, true, &opts).expect("exactly 64 bytes is OK");
    }

    #[test]
    fn validate_callback_data_budget_multi_select_rejects_overflow() {
        // 36 + 8 + 21 = 65 > 64
        let twentyone: String = "x".repeat(21);
        let opts = vec![("nope".to_string(), twentyone)];
        let err = validate_callback_data_budget(36, true, &opts).unwrap_err();
        assert!(err.to_string().contains("budget exceeded"));
    }
}
