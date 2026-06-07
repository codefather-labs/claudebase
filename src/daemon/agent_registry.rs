//! Slice 5 — agent_registry table + state transitions.
//!
//! Schema v6 (defined in `chat.rs::ensure_chat_db_schema`) carries the
//! `agent_registry` table + 2 indexes (1 partial UNIQUE on
//! `(chat_thread_id, agent_name) WHERE state='alive' AND chat_thread_id IS NOT NULL`
//! per F-5.1 red-team finding, 1 routing index for Slice 7
//! `target_agent_id` resolution).
//!
//! State machine (STRUCTURAL-5-2):
//!   alive    → orphaned   (on connection EOF — bulk-UPDATE by connection_id)
//!   alive    → alive      (idempotent re-register from SAME connection_id)
//!   orphaned → alive      (re-register from NEW connection_id, same agent_id)
//!   alive    → dead       (agent_unregister)
//!   orphaned → dead       (agent_unregister OR agent_reap)
//!
//! Reconciliation of UC-5-EC-2 (idempotent same agent_id) and UC-5-EC-3
//! (different agent_id same name in alive thread) is a single
//! `INSERT...ON CONFLICT(agent_id) DO UPDATE` statement (STRUCTURAL-5-3):
//! ON CONFLICT on the agent_id primary key handles UC-5-EC-2;
//! the partial UNIQUE index on `(thread, name)` fires SQLITE_CONSTRAINT_UNIQUE
//! on UC-5-EC-3 BEFORE the ON CONFLICT clause runs, surfacing the
//! literal "UNIQUE constraint failed" string the implementer catches and
//! maps to the friendly error TC-5.9 expects.
//!
//! `connection_id` rendering invariant (STRUCTURAL-5-8): every INSERT
//! uses `connection_id.to_string()` (where `connection_id` is the
//! `Uuid::new_v4()` value generated per-connection in
//! `src/daemon/server.rs`). Every EOF UPDATE uses the SAME
//! `to_string()` render so the WHERE clause matches.

use anyhow::Context;
use chrono::{Local, NaiveTime, TimeZone, Timelike};
use rusqlite::{params, Connection, OptionalExtension};
use serde_json::Value;

use crate::daemon::chat::now_millis;

/// Slice 5 of cli-to-cli-routing — sentinel value for indefinite DND.
/// Architect A-3 + OQ-UC-C2C-1 resolution: store `i64::MAX` in the
/// `dnd_until_ts` column so the drain query `dnd_until_ts < now()`
/// naturally excludes the indefinite-DND row without a special-case
/// branch.
pub const INDEFINITE_DND: i64 = i64::MAX;

/// The three states an agent can occupy. The DB CHECK constraint on
/// `state` enforces the same vocabulary; the Rust enum is the authority
/// for transition legality (transitions other than the ones listed in
/// the module docstring are forbidden — the caller MUST NOT bypass
/// these helpers).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AgentState {
    Alive,
    Orphaned,
    Dead,
}

impl AgentState {
    pub fn as_str(&self) -> &'static str {
        match self {
            AgentState::Alive => "alive",
            AgentState::Orphaned => "orphaned",
            AgentState::Dead => "dead",
        }
    }
    pub fn parse(s: &str) -> anyhow::Result<Self> {
        match s {
            "alive" => Ok(AgentState::Alive),
            "orphaned" => Ok(AgentState::Orphaned),
            "dead" => Ok(AgentState::Dead),
            other => anyhow::bail!("unknown agent state: {other}"),
        }
    }
}

/// Outcome of a successful `agent_register` call.
#[derive(Debug, Clone)]
pub struct RegisterOutcome {
    pub spawned_at: i64,
}

/// Outcome of a successful `agent_unregister` call.
#[derive(Debug, Clone)]
pub struct UnregisterOutcome {
    pub previous_state: String,
}

/// One row of the agent_registry table returned by `list_alive`.
///
/// Slice 1 of cli-to-cli-routing — schema v6 introduces 5 new optional
/// columns (project_id / branch / working_dir / feature_description /
/// dnd_until_ts). They are carried through the struct here so Slice 3's
/// `agent_describe` handler and Slice 5's `agent_set_dnd` handler can
/// construct/inspect rows without a second struct. `list_alive`'s SELECT
/// does NOT yet populate these — Slice 6 (the `claudebase agent list-alive`
/// CLI surface) extends the SELECT. Until then `list_alive` returns rows
/// with `None` in the 5 new fields.
#[derive(Debug, Clone)]
pub struct AgentRow {
    pub agent_id: String,
    pub agent_name: String,
    pub chat_thread_id: Option<String>,
    pub spawned_at: i64,
    pub last_pinged_at: i64,
    /// Normalized project identity (e.g. `github.com/owner/repo`).
    /// Populated by Slice 3 via `src/project_id.rs` resolver.
    pub project_id: Option<String>,
    /// Branch name captured at register time (`git rev-parse --abbrev-ref HEAD`).
    pub branch: Option<String>,
    /// Absolute cwd captured at register time — distinguishes per-clone
    /// agents that share a `project_id`.
    pub working_dir: Option<String>,
    /// Operator-facing label set by `agent_describe`. Mandated post-
    /// ExitPlanMode by the Slice 7 hook.
    pub feature_description: Option<String>,
    /// Do-Not-Disturb expiry in UNIX millis. `None` = no DND;
    /// `Some(i64::MAX)` = indefinite (architect A-3 / OQ-UC-C2C-1).
    pub dnd_until_ts: Option<i64>,
}

/// Outcome of a successful `agent_reap` call. The wire shape exposed
/// by the MCP tool is `{"reaped_count": N, "remaining_orphaned": N}` —
/// `reaped_count` (NOT `reaped`) per the TC-5.4 jq path
/// `.result.reaped_count`.
#[derive(Debug, Clone)]
pub struct ReapOutcome {
    pub reaped_count: usize,
    pub remaining_orphaned: usize,
}

/// Validate `agent_name` against the 1-64-char `[A-Za-z0-9_-]` charset
/// per STRUCTURAL-5-9. Prevents Unicode / control-char injection into
/// the unique-index lookup and PII leakage in tracing output.
pub fn validate_agent_name(name: &str) -> anyhow::Result<()> {
    if name.is_empty() || name.len() > 64 {
        anyhow::bail!("agent_name must be 1-64 chars of [a-zA-Z0-9_-]");
    }
    if !name
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
    {
        anyhow::bail!("agent_name must be 1-64 chars of [a-zA-Z0-9_-]");
    }
    Ok(())
}

/// Register an agent OR re-register an existing agent_id (idempotent
/// per UC-5-EC-2). On `(chat_thread_id, agent_name)` collision with a
/// DIFFERENT agent_id the partial UNIQUE index fires
/// SQLITE_CONSTRAINT_UNIQUE; we map the rusqlite error string to the
/// friendly TC-5.9 error.
///
/// **Rename-as-cleanup (operator vision 2026-06-03/04):** when this call
/// arrives on a `connection_id` that already has an alive row under a
/// DIFFERENT `agent_id`, the old row is marked `state='dead'` in the
/// SAME transaction as the new INSERT. This is the daemon-side half of
/// the "register as mira" rename UX — without it, the auto-registered
/// UUID/cwd-basename row stays alive alongside the new `mira` row and
/// `/agents` displays both as available, which surprised operator on
/// 2026-06-03. The bridge-side half (rewriting `.claudebase/config.json`
/// on success) lives in `src/plugin/bridge.rs::persist_rename_if_changed`.
///
/// Same-id re-call is a no-op for the cleanup (predicate excludes
/// `agent_id = ?2`); cross-connection_id rows are untouched (other CLIs'
/// alive rows are not the caller's to bury).
///
/// The transaction uses `unchecked_transaction` because the rusqlite
/// `transaction()` API requires `&mut Connection` and all callers pass
/// `&Connection` (open_chat_db owns the conn locally). Daemon
/// serializes writes via the WAL boundary so the "unchecked" caveat
/// (re-entry on the same connection) cannot fire here.
///
/// The `metadata` JSON value (if any) is serialised to TEXT for storage;
/// SQLite's JSON1 hint type is intentionally not used (TEXT + serde
/// round-trip is the canonical pattern).
pub fn register(
    conn: &Connection,
    agent_id: &str,
    agent_name: &str,
    connection_id: &str,
    chat_thread_id: Option<&str>,
    metadata: Option<&Value>,
) -> anyhow::Result<RegisterOutcome> {
    validate_agent_name(agent_name).context("agent_register: validate name")?;
    let now = now_millis();
    let metadata_text = metadata.map(|v| v.to_string());

    let tx = conn
        .unchecked_transaction()
        .context("agent_register: begin tx")?;

    // Rename-as-cleanup sweep: any alive row on THIS connection_id with
    // a DIFFERENT agent_id AND THE SAME chat_thread_id is a prior
    // register on the same identity slot (typically the bridge's auto-
    // register UUID before the user renamed via Mira). Bury it so
    // `/agents` shows only the current intended id.
    //
    // The chat_thread_id match uses SQLite's NULL-safe `IS` operator
    // so `(thread=NULL)` matches `(thread=NULL)` (the bridge auto-
    // register case) without burying a SIBLING agent that the same
    // connection registered into a DIFFERENT thread (e.g. a
    // permission_relayer pattern where one connection manages multiple
    // per-thread sub-agents — list_alive_filters_by_thread covers it).
    let stale_dead = tx
        .execute(
            "UPDATE agent_registry SET state='dead' \
             WHERE connection_id = ?1 \
               AND agent_id != ?2 \
               AND state = 'alive' \
               AND chat_thread_id IS ?3",
            params![connection_id, agent_id, chat_thread_id],
        )
        .context("agent_register: rename-cleanup UPDATE")?;

    let result = tx.execute(
        "INSERT INTO agent_registry \
         (agent_id, agent_name, connection_id, chat_thread_id, \
          permission_relayer, spawned_at, last_pinged_at, state, metadata) \
         VALUES (?1, ?2, ?3, ?4, NULL, ?5, ?5, 'alive', ?6) \
         ON CONFLICT(agent_id) DO UPDATE SET \
           agent_name = excluded.agent_name, \
           connection_id = excluded.connection_id, \
           chat_thread_id = excluded.chat_thread_id, \
           last_pinged_at = excluded.last_pinged_at, \
           state = 'alive', \
           metadata = excluded.metadata",
        params![agent_id, agent_name, connection_id, chat_thread_id, now, metadata_text],
    );
    match result {
        Ok(_) => {
            tx.commit().context("agent_register: commit tx")?;
            if stale_dead > 0 {
                tracing::info!(
                    %connection_id,
                    new_agent_id = %agent_id,
                    dropped_rows = stale_dead,
                    "agent_register rename-cleanup marked old rows dead"
                );
            }
            Ok(RegisterOutcome { spawned_at: now })
        }
        Err(e) => {
            // tx auto-rolls back on drop, so the rename-cleanup UPDATE is
            // also reverted — the file and daemon stay consistent under
            // failure: if the INSERT fails we leave the old row alive.
            let msg = e.to_string();
            if msg.contains("UNIQUE constraint failed") {
                anyhow::bail!(
                    "agent_name already alive in thread; unregister first or use different name"
                )
            }
            Err(e.into())
        }
    }
}

/// Mark an agent as `dead` (terminal state). Returns the previous
/// state so the caller can report "was alive" vs "was orphaned" vs
/// "absent" (idempotent — no error when the row never existed, per
/// UC-5-A).
///
/// Race: a concurrent unregister between our SELECT and UPDATE would
/// land both transitions to 'dead'; the result is the same end state.
/// `previous_state` may lie under that race (both callers report the
/// pre-pre-state) — acceptable for v1 since the end-state is correct
/// and SQLite serialises writes at the WAL boundary.
pub fn unregister(conn: &Connection, agent_id: &str) -> anyhow::Result<UnregisterOutcome> {
    let previous: Option<String> = conn
        .query_row(
            "SELECT state FROM agent_registry WHERE agent_id=?1",
            params![agent_id],
            |row| row.get(0),
        )
        .optional()?;
    match previous {
        Some(prev_state) => {
            conn.execute(
                "UPDATE agent_registry SET state='dead' WHERE agent_id=?1",
                params![agent_id],
            )?;
            Ok(UnregisterOutcome {
                previous_state: prev_state,
            })
        }
        None => Ok(UnregisterOutcome {
            previous_state: "absent".to_string(),
        }),
    }
}

/// Slice 3 of cli-to-cli-routing — capture project identity columns
/// (project_id / branch / working_dir) on an alive `agent_registry`
/// row. Called from `handle_agent_register` after the canonical UPSERT
/// once the caller's `cwd` has been resolved.
///
/// COALESCE semantics: passing `None` for a field leaves the existing
/// value untouched, so partial re-captures on rename or mid-session
/// agent_describe don't accidentally clobber identity that's already
/// good. Passing `Some("")` IS still a write — caller is expected to
/// filter empties before calling (the resolver in `src/project_id.rs`
/// never returns an empty string).
///
/// Returns the number of rows updated (0 if `agent_id` is not alive
/// in the registry, 1 otherwise).
pub fn capture_identity(
    conn: &Connection,
    agent_id: &str,
    project_id: Option<&str>,
    branch: Option<&str>,
    working_dir: Option<&str>,
) -> anyhow::Result<usize> {
    let n = conn.execute(
        "UPDATE agent_registry SET \
           project_id  = COALESCE(?1, project_id), \
           branch      = COALESCE(?2, branch), \
           working_dir = COALESCE(?3, working_dir) \
         WHERE agent_id = ?4 AND state = 'alive'",
        params![project_id, branch, working_dir, agent_id],
    )?;
    Ok(n)
}

/// Slice 3 of cli-to-cli-routing — set `feature_description` (and
/// optionally `branch`) on an existing `agent_registry` row. The
/// `agent_describe` MCP tool surface calls this after resolving the
/// caller's `agent_id` from the connection_id (the FR-C2C-4.6 sender
/// identity binding primitive — see `lookup_agent_id_by_connection`).
///
/// Returns the number of rows updated. Caller maps `0` to a JSON-RPC
/// error response surface ("no agent registered on this connection").
pub fn describe(
    conn: &Connection,
    agent_id: &str,
    feature_description: &str,
    branch: Option<&str>,
) -> anyhow::Result<usize> {
    let n = conn.execute(
        "UPDATE agent_registry SET \
           feature_description = ?1, \
           branch              = COALESCE(?2, branch) \
         WHERE agent_id = ?3",
        params![feature_description, branch, agent_id],
    )?;
    Ok(n)
}

/// Slice 3 of cli-to-cli-routing — resolve the `agent_id` of the alive
/// row bound to a specific `connection_id`. This is the security
/// primitive Slice 4 FR-C2C-4.6 builds on: instead of trusting a
/// caller-supplied `from_agent_id`, the daemon looks up the identity
/// the connection ALREADY registered. Local processes that can write
/// to the UDS but never called `agent_register` (or registered as a
/// different id) cannot impersonate.
///
/// Returns `None` when no alive row matches the connection. Multiple
/// alive rows on one connection_id is technically possible if the
/// caller registered several agents in different threads; we return
/// the most-recently-pinged to break the tie.
pub fn lookup_agent_id_by_connection(
    conn: &Connection,
    connection_id: &str,
) -> anyhow::Result<Option<String>> {
    let result = conn
        .query_row(
            "SELECT agent_id FROM agent_registry \
             WHERE connection_id = ?1 AND state = 'alive' \
             ORDER BY last_pinged_at DESC \
             LIMIT 1",
            params![connection_id],
            |row| row.get::<_, String>(0),
        )
        .optional()?;
    Ok(result)
}

/// Decision returned by [`send_message`] — whether the handler should
/// proceed with bus publication or skip it because the recipient is
/// in DND. The `message_id` is the row id of the persisted
/// `chat_messages` entry — useful for the [`mark_delivered`] follow-
/// up call on the deliver path.
#[derive(Debug, Clone)]
pub enum SendDecision {
    /// Recipient is not in DND. Handler should call `bus.publish(...)`
    /// and, if subscriber count >= 1, follow up with `mark_delivered`.
    Deliver,
    /// Recipient is in DND until the given UNIX-millis timestamp.
    /// `i64::MAX` denotes indefinite DND (architect A-3 /
    /// OQ-UC-C2C-1). Handler returns `{queued, delivered_when}` to
    /// the caller and lets Slice 5's drain task fire when DND clears.
    Queue { dnd_until_ts: i64 },
}

/// Outcome of [`send_message`] — the inserted row id plus the decision
/// the handler should act on.
#[derive(Debug, Clone)]
pub struct SendOutcome {
    pub message_id: String,
    pub decision: SendDecision,
}

/// Slice 4 of cli-to-cli-routing — DB-side primitive backing the
/// `agent_send` MCP tool handler.
///
/// Wraps target-alive verification + chat_messages INSERT in a single
/// SQLite transaction. The handler does the bus.publish + delivered_at
/// UPDATE async-side because `ChatBus` is tokio-backed.
///
/// **Security pre-review SEC-2 (PASS-WITH-CONDITIONS).** The row is
/// inserted with `delivered_at = NULL`; the handler bumps it to
/// `now_ms` via [`mark_delivered`] only after `bus.publish` returns a
/// non-zero subscriber count. If the publish reports zero subscribers
/// (target's bridge raced disconnect, no subscription on this thread,
/// etc.) the row stays drainable for Slice 5's DND-expiry drain task.
///
/// **FR-C2C-4.6 sender identity binding.** `from_agent_id` MUST be
/// the value the handler resolved from `connection_id` via
/// [`lookup_agent_id_by_connection`]. Callers passing in their own
/// claimed identity here defeat the binding; the handler layer is
/// the trust boundary, this DB primitive trusts its inputs.
///
/// Returns `Err` when the target row is absent, dead, or orphaned —
/// closes architect finding F-3 (no silent drop to orphaned targets).
pub fn send_message(
    conn: &Connection,
    from_agent_id: &str,
    to_agent_id: &str,
    content: &str,
    now_ms: i64,
) -> anyhow::Result<SendOutcome> {
    let tx = conn
        .unchecked_transaction()
        .context("send_message: begin tx")?;
    let (state, dnd_until_ts): (String, Option<i64>) = tx
        .query_row(
            "SELECT state, dnd_until_ts FROM agent_registry WHERE agent_id = ?1",
            params![to_agent_id],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .optional()
        .context("send_message: query target")?
        .ok_or_else(|| anyhow::anyhow!("agent not found or not alive: {to_agent_id}"))?;
    if state != "alive" {
        anyhow::bail!("agent not found or not alive: {to_agent_id}");
    }
    let decision = match dnd_until_ts {
        Some(ts) if ts > now_ms => SendDecision::Queue { dnd_until_ts: ts },
        _ => SendDecision::Deliver,
    };
    let message_id = uuid::Uuid::new_v4().to_string();
    let thread = format!("agent:{to_agent_id}");
    tx.execute(
        "INSERT INTO chat_messages \
         (id, thread_id, from_agent, content, reply_to, created_at, delivered_at) \
         VALUES (?1, ?2, ?3, ?4, NULL, ?5, NULL)",
        params![&message_id, &thread, from_agent_id, content, now_ms],
    )
    .context("send_message: insert chat_messages")?;
    tx.commit().context("send_message: commit tx")?;
    Ok(SendOutcome {
        message_id,
        decision,
    })
}

/// Slice 4 of cli-to-cli-routing — stamp `delivered_at` on the row id
/// returned by [`send_message`]. Called by the handler ONLY after
/// `bus.publish` confirmed >= 1 subscriber on the deliver path.
/// Returns the number of rows updated (0 if the id was already
/// stamped or removed — both treated as no-ops by the caller).
pub fn mark_delivered(
    conn: &Connection,
    message_id: &str,
    now_ms: i64,
) -> anyhow::Result<usize> {
    let n = conn.execute(
        "UPDATE chat_messages SET delivered_at = ?1 \
         WHERE id = ?2 AND delivered_at IS NULL",
        params![now_ms, message_id],
    )?;
    Ok(n)
}

/// Slice 5 of cli-to-cli-routing — parse a DND state string into a
/// `dnd_until_ts` value the daemon stores in `agent_registry`.
///
/// Grammar:
///   * `"on"`          → `Some(INDEFINITE_DND)` (i64::MAX sentinel)
///   * `"off"`         → `None`
///   * `"<N>m"`        → `Some(now_ms + N*60_000)` — minutes
///   * `"<N>h"`        → `Some(now_ms + N*3_600_000)` — hours
///   * `"until HH:MM"` → `Some(<next local HH:MM in millis>)`
///
/// Whitespace is trimmed; matching is case-insensitive on the keyword
/// stems (`On`, `OFF`, etc. all work) but the numeric-unit suffixes
/// `m`/`h` are kept lowercase per common DND-UI convention.
///
/// HH:MM is parsed in the operator's local timezone via `chrono::Local`.
/// If today's HH:MM has already passed (e.g., now is 19:00 and the
/// caller asks `until 18:00`), the timestamp rolls over to tomorrow.
/// DST transitions are handled by chrono — the parser never panics on
/// well-formed HH:MM where 0..=23 and 0..=59.
pub fn parse_dnd_state(s: &str, now_ms: i64) -> anyhow::Result<Option<i64>> {
    let trimmed = s.trim();
    if trimmed.is_empty() {
        anyhow::bail!("empty DND state");
    }
    let lower = trimmed.to_lowercase();

    if lower == "on" {
        return Ok(Some(INDEFINITE_DND));
    }
    if lower == "off" {
        return Ok(None);
    }
    if let Some(stripped) = lower.strip_prefix("until ") {
        let t = NaiveTime::parse_from_str(stripped.trim(), "%H:%M")
            .context("parse `until HH:MM`")?;
        // Anchor against the operator's local timezone today; if
        // today's target is in the past, roll to tomorrow.
        let now = Local
            .timestamp_millis_opt(now_ms)
            .single()
            .ok_or_else(|| anyhow::anyhow!("now_ms out of chrono range"))?;
        let today_target = now
            .date_naive()
            .and_hms_opt(t.hour(), t.minute(), 0)
            .ok_or_else(|| anyhow::anyhow!("invalid HH:MM"))?;
        let today_target_local = Local
            .from_local_datetime(&today_target)
            .single()
            .or_else(|| Local.from_local_datetime(&today_target).earliest())
            .ok_or_else(|| anyhow::anyhow!("ambiguous local time"))?;
        let mut target = today_target_local.timestamp_millis();
        if target <= now_ms {
            // Roll forward one day. chrono is happy with naive +
            // Days; we use a simple millis-add (no DST drift for a
            // 24h step since we re-anchor in the local zone).
            target += 24 * 3_600_000;
        }
        return Ok(Some(target));
    }
    // Number + unit suffix path.
    let (num_part, unit) = if let Some(p) = lower.strip_suffix('m') {
        (p, "m")
    } else if let Some(p) = lower.strip_suffix('h') {
        (p, "h")
    } else {
        anyhow::bail!("DND state must end with `m`, `h`, or be `on`/`off`/`until HH:MM`");
    };
    let n: u64 = num_part
        .trim()
        .parse()
        .context("DND numeric prefix must parse as u64")?;
    let delta_ms: i64 = match unit {
        "m" => (n as i64) * 60_000,
        "h" => (n as i64) * 3_600_000,
        _ => anyhow::bail!("unreachable: unit was already matched"),
    };
    Ok(Some(now_ms + delta_ms))
}

/// Slice 5 of cli-to-cli-routing — UPDATE `dnd_until_ts` on an alive
/// row. Passing `None` clears DND (sets the column to NULL); passing
/// `Some(i64::MAX)` (`INDEFINITE_DND`) sets indefinite DND; any other
/// `Some(ts)` is a future expiry.
///
/// Returns the rows updated (0 if `agent_id` isn't alive in the
/// registry, 1 otherwise). Caller surfaces 0 as a "no agent on this
/// connection" JSON-RPC error.
pub fn set_dnd(
    conn: &Connection,
    agent_id: &str,
    dnd_until_ts: Option<i64>,
) -> anyhow::Result<usize> {
    let n = conn.execute(
        "UPDATE agent_registry SET dnd_until_ts = ?1 \
         WHERE agent_id = ?2 AND state = 'alive'",
        params![dnd_until_ts, agent_id],
    )?;
    Ok(n)
}

/// A single drainable message returned by [`drain_dnd_tick`]. The
/// daemon-side handler turns each into a `notifications/claude/channel`
/// frame + a follow-up `mark_delivered` UPDATE.
#[derive(Debug, Clone)]
pub struct DrainableMessage {
    pub id: String,
    pub thread_id: String,
    pub from_agent: String,
    pub content: String,
    pub created_at: i64,
}

/// Stats returned by [`drain_dnd_tick`] for the tracing heartbeat.
#[derive(Debug, Clone, Default)]
pub struct DrainStats {
    /// Number of `agent_registry` rows whose expired DND was cleared
    /// this tick.
    pub cleared_dnd: usize,
    /// Messages the caller should emit on the bus + mark_delivered.
    pub drainable: Vec<DrainableMessage>,
}

/// Slice 5 of cli-to-cli-routing — single-tick worker for the DND
/// drain background task.
///
/// Steps (all inside one SQLite transaction):
///   1. SELECT agent_id FROM agent_registry WHERE dnd_until_ts IS NOT
///      NULL AND dnd_until_ts < now_ms — these are the agents whose
///      DND just expired this tick.
///   2. For each, clear `dnd_until_ts = NULL`.
///   3. SELECT chat_messages WHERE thread_id = 'agent:<id>' AND
///      delivered_at IS NULL ORDER BY created_at ASC LIMIT rate_limit
///      (FR-C2C-5.5: rate_limit defaults to 10 per agent per tick).
///   4. Return the batch to the caller; caller emits notifications +
///      marks delivered.
///
/// `i64::MAX` (`INDEFINITE_DND`) is naturally excluded by the
/// `dnd_until_ts < now_ms` predicate since no plausible `now_ms`
/// reaches `i64::MAX`. Architect A-3 confirmed.
///
/// F-2 phantom-sender semantics: rows are drained with their
/// ORIGINAL `from_agent` regardless of whether that agent is still
/// alive. Operator-accepted symptom — receiver can detect
/// dead-sender via `agent list-alive`.
pub fn drain_dnd_tick(
    conn: &Connection,
    now_ms: i64,
    rate_limit: usize,
) -> anyhow::Result<DrainStats> {
    let tx = conn
        .unchecked_transaction()
        .context("drain_dnd_tick: begin tx")?;
    // Step 1 — collect expired-DND agents.
    let expired: Vec<String> = {
        let mut stmt = tx.prepare(
            "SELECT agent_id FROM agent_registry \
             WHERE dnd_until_ts IS NOT NULL AND dnd_until_ts < ?1",
        )?;
        let rows: rusqlite::Result<Vec<String>> = stmt
            .query_map(params![now_ms], |r| r.get(0))?
            .collect();
        rows?
    };

    let mut stats = DrainStats::default();
    for agent_id in &expired {
        // Step 2 — clear DND.
        tx.execute(
            "UPDATE agent_registry SET dnd_until_ts = NULL WHERE agent_id = ?1",
            params![agent_id],
        )?;
        stats.cleared_dnd += 1;

        // Step 3 — batch up to rate_limit drainable messages.
        let thread = format!("agent:{agent_id}");
        let mut q = tx.prepare(
            "SELECT id, thread_id, from_agent, content, created_at \
             FROM chat_messages \
             WHERE thread_id = ?1 AND delivered_at IS NULL \
             ORDER BY created_at ASC \
             LIMIT ?2",
        )?;
        let rows: rusqlite::Result<Vec<DrainableMessage>> = q
            .query_map(params![&thread, rate_limit as i64], |r| {
                Ok(DrainableMessage {
                    id: r.get(0)?,
                    thread_id: r.get(1)?,
                    from_agent: r.get(2)?,
                    content: r.get(3)?,
                    created_at: r.get(4)?,
                })
            })?
            .collect();
        stats.drainable.extend(rows?);
    }
    tx.commit().context("drain_dnd_tick: commit")?;
    Ok(stats)
}

/// Slice 5 hotfix of cli-to-cli-routing — drain the queued inbox of a
/// SPECIFIC agent, regardless of DND state. Called from the
/// `agent_set_dnd("off")` handler when the operator explicitly clears
/// DND, because the recurring `drain_dnd_tick` task only picks up
/// rows whose `dnd_until_ts < now()` AND IS NOT NULL — clearing DND
/// to NULL would otherwise leave queued messages permanently
/// undelivered (regression caught by live Wave 5 QA, doc_id #10 in
/// the insights corpus).
///
/// Returns the same `DrainableMessage` shape as `drain_dnd_tick` so
/// the handler can reuse the bus.publish + mark_delivered loop. The
/// caller is responsible for emitting and marking; this primitive
/// only SELECTs.
pub fn drain_agent_inbox(
    conn: &Connection,
    agent_id: &str,
    rate_limit: usize,
) -> anyhow::Result<Vec<DrainableMessage>> {
    let thread = format!("agent:{agent_id}");
    let mut stmt = conn.prepare(
        "SELECT id, thread_id, from_agent, content, created_at \
         FROM chat_messages \
         WHERE thread_id = ?1 AND delivered_at IS NULL \
         ORDER BY created_at ASC \
         LIMIT ?2",
    )?;
    let rows: rusqlite::Result<Vec<DrainableMessage>> = stmt
        .query_map(params![&thread, rate_limit as i64], |r| {
            Ok(DrainableMessage {
                id: r.get(0)?,
                thread_id: r.get(1)?,
                from_agent: r.get(2)?,
                content: r.get(3)?,
                created_at: r.get(4)?,
            })
        })?
        .collect();
    Ok(rows?)
}

/// List all rows where state='alive', optionally filtered by thread.
/// Ordered newest-pinged first so Slice 7's routing can prefer
/// recently-active agents on race.
pub fn list_alive(conn: &Connection, thread: Option<&str>) -> anyhow::Result<Vec<AgentRow>> {
    let mut stmt = conn.prepare(
        "SELECT agent_id, agent_name, chat_thread_id, spawned_at, last_pinged_at \
         FROM agent_registry \
         WHERE state='alive' \
           AND (?1 IS NULL OR chat_thread_id = ?1) \
         ORDER BY last_pinged_at DESC \
         LIMIT 1000",
    )?;
    let rows: rusqlite::Result<Vec<AgentRow>> = stmt
        .query_map(params![thread], |row| {
            Ok(AgentRow {
                agent_id: row.get(0)?,
                agent_name: row.get(1)?,
                chat_thread_id: row.get(2)?,
                spawned_at: row.get(3)?,
                last_pinged_at: row.get(4)?,
                // Slice 1 of cli-to-cli-routing — v6 columns not yet in
                // this SELECT. Slice 6 extends the projection list when
                // the `claudebase agent list-alive` CLI surface lands.
                project_id: None,
                branch: None,
                working_dir: None,
                feature_description: None,
                dnd_until_ts: None,
            })
        })?
        .collect();
    Ok(rows?)
}

/// Slice 2 of multi-agent-telegram-on-v0.6 — resolve the CLI bound to
/// a specific Telegram routing key `(routing_chat_id, routing_thread_id)`.
/// Returns the bound CLI's `agent_id` when an alive binding exists,
/// `Ok(None)` otherwise.
///
/// The COALESCE(-1) sentinel pattern is symmetric with the expression
/// index `agent_registry_routing_alive_uniq_idx` created in Slice 1 so
/// the SQLite optimiser can use the index for the lookup. The
/// `state = 'alive'` predicate excludes orphaned and dead rows; the
/// implicit `routing_chat_id IS NOT NULL` (via the equality match on
/// the parameter) excludes legacy chat-bus rows that never bound a
/// Telegram routing key.
///
/// Called from `daemon::telegram::process_batch_with_pairing` BEFORE
/// the @-mention fallback so explicit `(chat_id, thread_id)` bindings
/// win over text-based @mention resolution.
pub fn resolve_routing(
    conn: &Connection,
    routing_chat_id: i64,
    routing_thread_id: Option<i64>,
) -> rusqlite::Result<Option<String>> {
    let result: rusqlite::Result<String> = conn.query_row(
        "SELECT agent_id FROM agent_registry \
         WHERE routing_chat_id = ?1 \
           AND COALESCE(routing_thread_id, -1) = COALESCE(?2, -1) \
           AND state = 'alive'",
        params![routing_chat_id, routing_thread_id],
        |row| row.get(0),
    );
    match result {
        Ok(agent_id) => Ok(Some(agent_id)),
        Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
        Err(e) => Err(e),
    }
}

/// Reap orphaned rows older than `older_than_secs` seconds. Default
/// 86400 (24 hours). Returns `reaped_count` (the number of rows
/// transitioned orphaned→dead) and `remaining_orphaned` (rows still in
/// state='orphaned' after the reap).
///
/// **Unit note (insight #12)**: the wire param is in SECONDS per
/// FR-ACD-5.4 PRD spec; `last_pinged_at` is stored in MILLISECONDS
/// (`now_millis()` convention). The cutoff arithmetic multiplies by
/// 1000 here so the WHERE clause is unit-coherent.
pub fn reap(conn: &Connection, older_than_secs: Option<i64>) -> anyhow::Result<ReapOutcome> {
    let secs = older_than_secs.unwrap_or(86_400);
    let cutoff_ms = now_millis().saturating_sub(secs.saturating_mul(1000));
    let reaped = conn.execute(
        "UPDATE agent_registry SET state='dead' \
         WHERE state='orphaned' AND last_pinged_at < ?1",
        params![cutoff_ms],
    )?;
    let remaining: i64 = conn.query_row(
        "SELECT COUNT(*) FROM agent_registry WHERE state='orphaned'",
        [],
        |row| row.get(0),
    )?;
    Ok(ReapOutcome {
        reaped_count: reaped,
        remaining_orphaned: remaining as usize,
    })
}

/// Slice 4a of multi-agent-telegram-on-v0.6 — bind a CLI to a Telegram
/// routing key `(routing_chat_id, routing_thread_id)`. If another ALIVE
/// CLI is already bound to this routing key, that binding is FIRST
/// cleared (its `routing_*` columns set to NULL) so the partial-UNIQUE
/// expression-index `(chat_id, COALESCE(thread_id, -1)) WHERE
/// state='alive'` invariant is preserved.
///
/// The clear + set runs inside a single `BEGIN IMMEDIATE` transaction
/// so concurrent `/switch` calls serialize at the SQLite layer (the
/// IMMEDIATE behavior takes a write lock on entry instead of upgrading
/// from DEFERRED on the first write, removing the SQLITE_BUSY retry
/// window). Second-tap-wins is the deterministic outcome — exactly the
/// FR-MAT-8.3 / TC-14 contract.
///
/// Orphaned/dead rows are not touched (their routing_* columns may
/// remain populated from before they went orphaned — they're excluded
/// from the partial-UNIQUE index by the `WHERE state='alive'` clause).
///
/// Idempotent: re-binding the same agent to the same routing key
/// produces no observable change.
pub fn bind_routing_key(
    conn: &mut Connection,
    agent_id: &str,
    routing_chat_id: i64,
    routing_thread_id: Option<i64>,
) -> rusqlite::Result<()> {
    let tx = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
    bind_routing_key_in_tx(&tx, agent_id, routing_chat_id, routing_thread_id)?;
    tx.commit()?;
    Ok(())
}

/// Slice 4c — internal-tx variant of `bind_routing_key`. Used by
/// `/switch` handler dispatched inside `process_batch_with_pairing`
/// which already holds a rusqlite Transaction. Within a single Update
/// batch, multiple `/switch` taps serialize naturally via sequential
/// processing; cross-batch concurrency is guarded by run_long_poll's
/// sequential batch loop. The caller is responsible for committing
/// the parent transaction.
pub fn bind_routing_key_in_tx(
    tx: &rusqlite::Transaction,
    agent_id: &str,
    routing_chat_id: i64,
    routing_thread_id: Option<i64>,
) -> rusqlite::Result<()> {
    // Clear any OTHER alive CLI's binding on this routing key (the
    // `agent_id != ?3` clause keeps a same-agent rebind idempotent).
    tx.execute(
        "UPDATE agent_registry \
         SET routing_chat_id = NULL, routing_thread_id = NULL \
         WHERE state = 'alive' \
           AND routing_chat_id = ?1 \
           AND COALESCE(routing_thread_id, -1) = COALESCE(?2, -1) \
           AND agent_id != ?3",
        params![routing_chat_id, routing_thread_id, agent_id],
    )?;
    // Bind THIS agent. UPDATE 0 rows when the agent doesn't exist or is
    // not alive — the caller (Slice 4c `/switch` handler) is responsible
    // for surfacing that case to the user with a helpful error.
    tx.execute(
        "UPDATE agent_registry \
         SET routing_chat_id = ?1, routing_thread_id = ?2 \
         WHERE agent_id = ?3 AND state = 'alive'",
        params![routing_chat_id, routing_thread_id, agent_id],
    )?;
    Ok(())
}

/// Slice 4a — clear a CLI's routing-key binding without touching other
/// columns (state, host, cwd, pid, last_user_id stay). Used by `/switch`
/// to unbind the old CLI when the operator names a different one, and
/// by agent shutdown paths. Returns Ok(()) even when the agent has no
/// binding (no-op UPDATE 0 rows).
pub fn unbind_routing_key(conn: &Connection, agent_id: &str) -> rusqlite::Result<()> {
    conn.execute(
        "UPDATE agent_registry \
         SET routing_chat_id = NULL, routing_thread_id = NULL \
         WHERE agent_id = ?1",
        params![agent_id],
    )?;
    Ok(())
}

/// Slice 4a — stamp `last_user_id` on a CLI's binding row. Called from
/// the inbound routing path (Slice 4b wire-up) on every successful
/// `resolve_routing` hit so `/switch` has the FR-MAT-8.6 authorization
/// signal: only the last_user_id OR a chat admin may rebind the
/// routing key. The stamp targets ALIVE rows only (orphaned bindings
/// don't have an in-flight operator and shouldn't grant `/switch`
/// authority retroactively).
///
/// Idempotent: stamping the same user_id is a no-op write.
pub fn stamp_last_user_id(
    conn: &Connection,
    agent_id: &str,
    last_user_id: i64,
) -> rusqlite::Result<()> {
    conn.execute(
        "UPDATE agent_registry SET last_user_id = ?1 \
         WHERE agent_id = ?2 AND state = 'alive'",
        params![last_user_id, agent_id],
    )?;
    Ok(())
}

/// Slice 4a — list all alive CLIs bound to a specific routing key
/// `(chat_id, thread_id)`. Under the current 1-CLI-per-key invariant
/// (Slice 1 partial-UNIQUE index) the Vec contains 0 or 1 entries; the
/// signature returns a Vec for future extensibility (e.g., if multiple
/// CLIs can fan out from the same key).
///
/// Used by Slice 4b's `/agents` command to enumerate the active CLI on
/// the requesting (chat, topic) tuple, and by Slice 4c's `/whoami` /
/// `/here` commands to look up the binding's metadata.
pub fn list_routings_for(
    conn: &Connection,
    routing_chat_id: i64,
    routing_thread_id: Option<i64>,
) -> rusqlite::Result<Vec<AgentRow>> {
    let mut stmt = conn.prepare(
        "SELECT agent_id, agent_name, chat_thread_id, spawned_at, last_pinged_at \
         FROM agent_registry \
         WHERE state = 'alive' \
           AND routing_chat_id = ?1 \
           AND COALESCE(routing_thread_id, -1) = COALESCE(?2, -1)",
    )?;
    let rows: rusqlite::Result<Vec<AgentRow>> = stmt
        .query_map(params![routing_chat_id, routing_thread_id], |row| {
            Ok(AgentRow {
                agent_id: row.get(0)?,
                agent_name: row.get(1)?,
                chat_thread_id: row.get(2)?,
                spawned_at: row.get(3)?,
                last_pinged_at: row.get(4)?,
                // v6 columns not in this SELECT — Slice 6 extends.
                project_id: None,
                branch: None,
                working_dir: None,
                feature_description: None,
                dnd_until_ts: None,
            })
        })?
        .collect();
    Ok(rows?)
}

/// Bulk-UPDATE all rows where connection_id matches and state='alive'
/// to state='orphaned'. Called from the connection-EOF hook in
/// `server.rs::handle_connection`. Single SQL statement so SQLite's
/// auto-commit provides atomicity.
pub fn mark_connection_orphaned(conn: &Connection, connection_id: &str) -> anyhow::Result<usize> {
    let updated = conn.execute(
        "UPDATE agent_registry SET state='orphaned' \
         WHERE connection_id=?1 AND state='alive'",
        params![connection_id],
    )?;
    Ok(updated)
}

/// Reap-on-boot: mark every state='alive' row as 'orphaned'. Run at
/// daemon startup because all connections that existed before restart
/// are gone. Slice 1b's `reap_on_boot_stub` body becomes this real path
/// now that schema v6 ensures the table exists.
pub fn reap_on_boot(conn: &Connection) -> anyhow::Result<usize> {
    let updated = conn.execute(
        "UPDATE agent_registry SET state='orphaned' WHERE state='alive'",
        [],
    )?;
    Ok(updated)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn open_test_conn() -> Connection {
        let conn = Connection::open_in_memory().expect("in-memory db");
        crate::daemon::chat::ensure_chat_db_schema(&conn).expect("schema applied");
        conn
    }

    #[test]
    fn agent_state_round_trip() {
        for s in [AgentState::Alive, AgentState::Orphaned, AgentState::Dead] {
            assert_eq!(AgentState::parse(s.as_str()).unwrap(), s);
        }
        assert!(AgentState::parse("bogus").is_err());
    }

    #[test]
    fn validate_agent_name_accepts_valid() {
        for n in ["planner", "PLANNER", "x", "_a", "-z", "a-b_c-1"] {
            assert!(validate_agent_name(n).is_ok(), "should accept '{n}'");
        }
        let long_ok = "a".repeat(64);
        assert!(validate_agent_name(&long_ok).is_ok());
    }

    #[test]
    fn validate_agent_name_rejects_invalid() {
        for n in ["", "with space", "слово", "name!", "n.dot"] {
            assert!(validate_agent_name(n).is_err(), "should reject '{n}'");
        }
        let too_long = "a".repeat(65);
        assert!(validate_agent_name(&too_long).is_err());
    }

    #[test]
    fn register_creates_alive_row() {
        let conn = open_test_conn();
        let out = register(
            &conn,
            "planner-abc123",
            "planner",
            "conn-xyz",
            Some("telegram:1001"),
            Some(&json!({"role": "tactical"})),
        )
        .unwrap();
        assert!(out.spawned_at > 0);
        let state: String = conn
            .query_row(
                "SELECT state FROM agent_registry WHERE agent_id='planner-abc123'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(state, "alive");
    }

    #[test]
    fn register_idempotent_same_agent_id() {
        let conn = open_test_conn();
        register(&conn, "a-1", "planner", "conn-x", Some("t-1"), None).unwrap();
        // Re-register from a DIFFERENT connection — UC-5-EC-2 expects
        // a successful re-bind (orphaned → alive flow if state was
        // orphaned; alive → alive otherwise).
        register(&conn, "a-1", "planner", "conn-y", Some("t-1"), None).unwrap();
        let cid: String = conn
            .query_row(
                "SELECT connection_id FROM agent_registry WHERE agent_id='a-1'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(cid, "conn-y", "re-register should update connection_id");
    }

    #[test]
    fn register_rejects_conflict_different_agent_id_same_name_same_thread() {
        let conn = open_test_conn();
        register(&conn, "a-1", "planner", "conn-x", Some("t-1"), None).unwrap();
        // Different agent_id, same (thread, name) — partial UNIQUE index
        // fires; UC-5-EC-3, TC-5.9.
        let err = register(&conn, "a-2", "planner", "conn-y", Some("t-1"), None).unwrap_err();
        assert!(
            err.to_string()
                .contains("agent_name already alive in thread"),
            "expected friendly TC-5.9 error, got: {err}"
        );
    }

    #[test]
    fn register_allows_same_name_different_thread() {
        let conn = open_test_conn();
        register(&conn, "a-1", "planner", "conn-x", Some("t-1"), None).unwrap();
        // Different thread — UNIQUE INDEX scope is per-thread.
        register(&conn, "a-2", "planner", "conn-x", Some("t-2"), None).unwrap();
    }

    #[test]
    fn rename_cleanup_marks_old_row_dead_when_same_connection_id_renames() {
        // Bridge auto-registers as UUID; user later asks Mira "register
        // as mira" which calls agent_register on the SAME connection_id.
        // The UUID row must transition to dead so /agents shows only
        // "mira" — the operator-vision rename UX from 2026-06-03.
        let conn = open_test_conn();
        register(&conn, "uuid-abc", "uuid-abc", "conn-shared", None, None).unwrap();
        register(&conn, "mira", "mira", "conn-shared", None, None).unwrap();
        // old row: dead
        let old_state: String = conn
            .query_row(
                "SELECT state FROM agent_registry WHERE agent_id='uuid-abc'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(old_state, "dead", "old agent_id row must be dead after rename");
        // new row: alive
        let new_state: String = conn
            .query_row(
                "SELECT state FROM agent_registry WHERE agent_id='mira'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(new_state, "alive");
        // only ONE alive row on this connection_id
        let alive_count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM agent_registry \
                 WHERE connection_id='conn-shared' AND state='alive'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(alive_count, 1);
    }

    #[test]
    fn rename_cleanup_does_not_touch_same_id_reregister() {
        // Same connection_id, same agent_id, second register call
        // (e.g. bridge auto-register after a daemon bounce + reconnect).
        // The cleanup predicate excludes agent_id = ?2 so no rows are
        // marked dead — pure idempotent re-bind.
        let conn = open_test_conn();
        register(&conn, "uuid-abc", "uuid-abc", "conn-x", None, None).unwrap();
        register(&conn, "uuid-abc", "uuid-abc", "conn-x", None, None).unwrap();
        let alive_count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM agent_registry \
                 WHERE agent_id='uuid-abc' AND state='alive'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(alive_count, 1);
        let dead_count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM agent_registry \
                 WHERE agent_id='uuid-abc' AND state='dead'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(dead_count, 0, "no rows should be marked dead on same-id re-register");
    }

    #[test]
    fn rename_cleanup_does_not_bury_sibling_agent_in_different_thread() {
        // Same connection_id manages multiple sub-agents, each scoped
        // to its own chat_thread_id (permission_relayer pattern). When
        // one of them renames, the cleanup must use NULL-safe
        // `chat_thread_id IS ?` matching so it ONLY buries the row in
        // the SAME thread, not the sibling in a different thread.
        let conn = open_test_conn();
        register(&conn, "uuid-1", "uuid-1", "conn-shared", Some("t-1"), None).unwrap();
        register(&conn, "uuid-2", "uuid-2", "conn-shared", Some("t-2"), None).unwrap();
        // Rename the t-1 occupant
        register(&conn, "mira", "mira", "conn-shared", Some("t-1"), None).unwrap();
        // t-1 occupant: dead (was uuid-1)
        let s1: String = conn
            .query_row(
                "SELECT state FROM agent_registry WHERE agent_id='uuid-1'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(s1, "dead", "uuid-1 must be dead (rename swept it)");
        // t-2 occupant: alive (uuid-2 must NOT have been touched)
        let s2: String = conn
            .query_row(
                "SELECT state FROM agent_registry WHERE agent_id='uuid-2'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(
            s2, "alive",
            "uuid-2 in t-2 must stay alive — cleanup is per-thread"
        );
        // mira: alive
        let sm: String = conn
            .query_row(
                "SELECT state FROM agent_registry WHERE agent_id='mira'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(sm, "alive");
    }

    #[test]
    fn rename_cleanup_does_not_bury_other_connections_alive_rows() {
        // Two CLIs each auto-register on their own connection_ids. One
        // CLI renames; the OTHER CLI's row must stay alive — the cleanup
        // is scoped by connection_id and must not touch siblings.
        let conn = open_test_conn();
        register(&conn, "uuid-1", "uuid-1", "conn-1", None, None).unwrap();
        register(&conn, "uuid-2", "uuid-2", "conn-2", None, None).unwrap();
        // conn-1 renames to "mira"
        register(&conn, "mira", "mira", "conn-1", None, None).unwrap();
        let state_other: String = conn
            .query_row(
                "SELECT state FROM agent_registry WHERE agent_id='uuid-2'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(
            state_other, "alive",
            "rename on conn-1 must not bury conn-2's alive row"
        );
    }

    #[test]
    fn unregister_marks_dead_and_releases_index_slot() {
        let conn = open_test_conn();
        register(&conn, "a-1", "planner", "conn-x", Some("t-1"), None).unwrap();
        unregister(&conn, "a-1").unwrap();
        let state: String = conn
            .query_row(
                "SELECT state FROM agent_registry WHERE agent_id='a-1'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(state, "dead");
        // After dead, (thread, name) slot is released — a fresh
        // register with a different agent_id succeeds.
        register(&conn, "a-2", "planner", "conn-y", Some("t-1"), None).unwrap();
    }

    #[test]
    fn list_alive_filters_by_thread() {
        let conn = open_test_conn();
        register(&conn, "a-1", "planner", "c-x", Some("t-1"), None).unwrap();
        register(&conn, "a-2", "reflection", "c-x", Some("t-2"), None).unwrap();
        let t1 = list_alive(&conn, Some("t-1")).unwrap();
        assert_eq!(t1.len(), 1);
        assert_eq!(t1[0].agent_id, "a-1");
        let all = list_alive(&conn, None).unwrap();
        assert_eq!(all.len(), 2);
    }

    #[test]
    fn reap_drops_orphaned_older_than_cutoff() {
        let conn = open_test_conn();
        register(&conn, "a-1", "planner", "c-x", Some("t-1"), None).unwrap();
        // Backdate last_pinged_at to 25 hours ago AND set state to orphaned.
        let two_days_ago_ms = now_millis() - 25 * 60 * 60 * 1000;
        conn.execute(
            "UPDATE agent_registry SET state='orphaned', last_pinged_at=?1 WHERE agent_id='a-1'",
            params![two_days_ago_ms],
        )
        .unwrap();
        // Reap with default 24h cutoff.
        let out = reap(&conn, None).unwrap();
        assert_eq!(out.reaped_count, 1);
        assert_eq!(out.remaining_orphaned, 0);
    }

    #[test]
    fn reap_preserves_fresh_orphans() {
        let conn = open_test_conn();
        register(&conn, "a-1", "planner", "c-x", Some("t-1"), None).unwrap();
        // Mark orphaned but keep last_pinged_at recent.
        conn.execute(
            "UPDATE agent_registry SET state='orphaned' WHERE agent_id='a-1'",
            [],
        )
        .unwrap();
        let out = reap(&conn, Some(86400)).unwrap();
        assert_eq!(out.reaped_count, 0);
        assert_eq!(out.remaining_orphaned, 1);
    }

    #[test]
    fn mark_connection_orphaned_only_touches_matching_connection() {
        let conn = open_test_conn();
        register(&conn, "a-1", "planner", "conn-a", Some("t-1"), None).unwrap();
        register(&conn, "a-2", "reflection", "conn-b", Some("t-2"), None).unwrap();
        let updated = mark_connection_orphaned(&conn, "conn-a").unwrap();
        assert_eq!(updated, 1);
        let s1: String = conn
            .query_row(
                "SELECT state FROM agent_registry WHERE agent_id='a-1'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        let s2: String = conn
            .query_row(
                "SELECT state FROM agent_registry WHERE agent_id='a-2'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(s1, "orphaned");
        assert_eq!(s2, "alive");
    }

    #[test]
    fn reap_on_boot_marks_all_alive_as_orphaned() {
        let conn = open_test_conn();
        register(&conn, "a-1", "planner", "c-x", Some("t-1"), None).unwrap();
        register(&conn, "a-2", "reflection", "c-y", Some("t-2"), None).unwrap();
        let updated = reap_on_boot(&conn).unwrap();
        assert_eq!(updated, 2);
        for id in ["a-1", "a-2"] {
            let s: String = conn
                .query_row(
                    "SELECT state FROM agent_registry WHERE agent_id=?1",
                    params![id],
                    |row| row.get(0),
                )
                .unwrap();
            assert_eq!(s, "orphaned");
        }
    }

    #[test]
    fn state_check_rejects_unknown_value() {
        let conn = open_test_conn();
        let err = conn
            .execute(
                "INSERT INTO agent_registry \
                 (agent_id, agent_name, connection_id, chat_thread_id, \
                  permission_relayer, spawned_at, last_pinged_at, state, metadata) \
                 VALUES ('x', 'planner', 'c', NULL, NULL, 0, 0, 'bogus', NULL)",
                [],
            )
            .unwrap_err();
        assert!(
            err.to_string().contains("CHECK constraint failed"),
            "expected DB-layer CHECK rejection, got: {err}"
        );
    }

    // ---------------------------------------------------------------
    // Slice 2 of multi-agent-telegram-on-v0.6 — resolve_routing tests.
    // Verifies the routing-key lookup against the Slice 1 partial-UNIQUE
    // expression-index in 4 cases:
    //   (a) DM hit: (chat, None) returns the bound agent_id
    //   (b) topic hit: (chat, Some(thread)) returns the bound agent_id
    //   (c) orphan miss: unbound routing key returns None
    //   (d) state filter: an `orphaned` row is NOT returned even when
    //       its routing key matches
    // ---------------------------------------------------------------

    fn slice2_seed_routing(
        conn: &Connection,
        agent_id: &str,
        connection_id: &str,
        routing_chat_id: i64,
        routing_thread_id: Option<i64>,
        state: &str,
    ) {
        conn.execute(
            "INSERT INTO agent_registry \
             (agent_id, agent_name, connection_id, chat_thread_id, spawned_at, last_pinged_at, state, routing_chat_id, routing_thread_id) \
             VALUES (?1, ?2, ?3, NULL, 1, 1, ?4, ?5, ?6)",
            params![
                agent_id,
                "x",
                connection_id,
                state,
                routing_chat_id,
                routing_thread_id,
            ],
        )
        .expect("seed insert");
    }

    #[test]
    fn slice2_resolve_routing_dm_hit() {
        let conn = open_test_conn();
        slice2_seed_routing(&conn, "a-dm", "c1", 100, None, "alive");
        let resolved = resolve_routing(&conn, 100, None).unwrap();
        assert_eq!(resolved, Some("a-dm".to_string()));
    }

    #[test]
    fn slice2_resolve_routing_topic_hit() {
        let conn = open_test_conn();
        slice2_seed_routing(&conn, "a-b", "c1", 500, Some(7), "alive");
        let resolved = resolve_routing(&conn, 500, Some(7)).unwrap();
        assert_eq!(resolved, Some("a-b".to_string()));
    }

    #[test]
    fn slice2_resolve_routing_orphan_miss_returns_none() {
        let conn = open_test_conn();
        // Bind one CLI to chat=500/topic=7; query for chat=500/topic=8 (no binding)
        slice2_seed_routing(&conn, "a-b", "c1", 500, Some(7), "alive");
        let resolved = resolve_routing(&conn, 500, Some(8)).unwrap();
        assert_eq!(resolved, None);
        // And query for a chat that has no binding at all.
        let resolved = resolve_routing(&conn, 999, None).unwrap();
        assert_eq!(resolved, None);
    }

    #[test]
    fn slice2_resolve_routing_excludes_orphaned_rows() {
        let conn = open_test_conn();
        // The row with routing key (100, NULL) is `orphaned` — the
        // resolve query must NOT return it. (KP1 binding can be
        // recreated by a new `claudebase run` from another cwd.)
        slice2_seed_routing(&conn, "a-dead", "c1", 100, None, "orphaned");
        let resolved = resolve_routing(&conn, 100, None).unwrap();
        assert_eq!(
            resolved, None,
            "orphaned row must not be returned by resolve_routing"
        );
    }

    #[test]
    fn slice2_resolve_routing_dm_and_topic_distinct() {
        // KP1 + KP2/KP3 simultaneously: same chat_id, one DM (None) and
        // two topics (7 + 8). resolve_routing returns the correct CLI
        // for each routing key.
        let conn = open_test_conn();
        slice2_seed_routing(&conn, "cli-a", "c1", 1000, None, "alive");
        slice2_seed_routing(&conn, "cli-b", "c2", 1000, Some(7), "alive");
        slice2_seed_routing(&conn, "cli-c", "c3", 1000, Some(8), "alive");
        assert_eq!(
            resolve_routing(&conn, 1000, None).unwrap(),
            Some("cli-a".to_string())
        );
        assert_eq!(
            resolve_routing(&conn, 1000, Some(7)).unwrap(),
            Some("cli-b".to_string())
        );
        assert_eq!(
            resolve_routing(&conn, 1000, Some(8)).unwrap(),
            Some("cli-c".to_string())
        );
    }

    // ---------------------------------------------------------------
    // Slice 4a of multi-agent-telegram-on-v0.6 — bind/unbind/stamp +
    // list_routings_for tests. Covers the binding mutators that
    // Slice 4b/4c command handlers will call into.
    // ---------------------------------------------------------------

    fn slice4_register_alive(conn: &Connection, agent_id: &str, agent_name: &str) {
        conn.execute(
            "INSERT INTO agent_registry \
             (agent_id, agent_name, connection_id, chat_thread_id, spawned_at, last_pinged_at, state) \
             VALUES (?1, ?2, ?3, NULL, 1, 1, 'alive')",
            params![agent_id, agent_name, format!("conn-{agent_id}")],
        )
        .expect("register insert");
    }

    fn slice4_routing_of(conn: &Connection, agent_id: &str) -> (Option<i64>, Option<i64>) {
        conn.query_row(
            "SELECT routing_chat_id, routing_thread_id FROM agent_registry WHERE agent_id = ?1",
            params![agent_id],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .expect("query routing")
    }

    #[test]
    fn slice4a_bind_routing_initial_succeeds() {
        let mut conn = open_test_conn();
        slice4_register_alive(&conn, "a1", "alice");
        bind_routing_key(&mut conn, "a1", 500, Some(7)).expect("initial bind");
        let (cid, tid) = slice4_routing_of(&conn, "a1");
        assert_eq!(cid, Some(500));
        assert_eq!(tid, Some(7));
    }

    #[test]
    fn slice4a_bind_routing_dm_with_none_thread() {
        let mut conn = open_test_conn();
        slice4_register_alive(&conn, "a-dm", "alice");
        bind_routing_key(&mut conn, "a-dm", 42, None).expect("DM bind");
        let (cid, tid) = slice4_routing_of(&conn, "a-dm");
        assert_eq!(cid, Some(42));
        assert_eq!(tid, None);
    }

    #[test]
    fn slice4a_bind_routing_displaces_existing_binding() {
        // /switch semantic: re-binding chat=500/thread=7 to a different
        // CLI clears the old CLI's routing_* columns and sets the new
        // CLI's columns. partial-UNIQUE index never violated.
        let mut conn = open_test_conn();
        slice4_register_alive(&conn, "old", "olivia");
        slice4_register_alive(&conn, "new", "natalie");
        bind_routing_key(&mut conn, "old", 500, Some(7)).expect("old bound");
        bind_routing_key(&mut conn, "new", 500, Some(7)).expect("new rebound");
        // Old must be cleared
        assert_eq!(slice4_routing_of(&conn, "old"), (None, None));
        // New must hold the binding
        assert_eq!(slice4_routing_of(&conn, "new"), (Some(500), Some(7)));
    }

    #[test]
    fn slice4a_bind_routing_idempotent_same_agent() {
        let mut conn = open_test_conn();
        slice4_register_alive(&conn, "a1", "alice");
        bind_routing_key(&mut conn, "a1", 100, None).unwrap();
        bind_routing_key(&mut conn, "a1", 100, None).expect("re-bind same agent");
        assert_eq!(slice4_routing_of(&conn, "a1"), (Some(100), None));
    }

    #[test]
    fn slice4a_bind_routing_preserves_orphaned_rows() {
        // An orphaned CLI's routing_* columns are NOT cleared when a new
        // alive CLI rebinds the same routing key — the orphaned record
        // is excluded from the partial-UNIQUE index anyway, and we want
        // to preserve the audit trail of "this CLI was bound here at
        // shutdown".
        let mut conn = open_test_conn();
        conn.execute(
            "INSERT INTO agent_registry \
             (agent_id, agent_name, connection_id, chat_thread_id, spawned_at, last_pinged_at, state, routing_chat_id, routing_thread_id) \
             VALUES ('ghost', 'g', 'cg', NULL, 1, 1, 'orphaned', 500, 7)",
            [],
        )
        .unwrap();
        slice4_register_alive(&conn, "new", "n");
        bind_routing_key(&mut conn, "new", 500, Some(7)).unwrap();
        // Ghost (orphaned) row's routing still populated
        assert_eq!(slice4_routing_of(&conn, "ghost"), (Some(500), Some(7)));
        // New (alive) row also bound
        assert_eq!(slice4_routing_of(&conn, "new"), (Some(500), Some(7)));
    }

    #[test]
    fn slice4a_unbind_routing_clears_columns() {
        let mut conn = open_test_conn();
        slice4_register_alive(&conn, "a1", "alice");
        bind_routing_key(&mut conn, "a1", 100, Some(3)).unwrap();
        unbind_routing_key(&conn, "a1").expect("unbind");
        assert_eq!(slice4_routing_of(&conn, "a1"), (None, None));
    }

    #[test]
    fn slice4a_unbind_routing_noop_on_unknown_agent() {
        let conn = open_test_conn();
        unbind_routing_key(&conn, "never-registered").expect("noop unbind");
    }

    #[test]
    fn slice4a_stamp_last_user_id_sets_column() {
        let mut conn = open_test_conn();
        slice4_register_alive(&conn, "a1", "alice");
        bind_routing_key(&mut conn, "a1", 100, None).unwrap();
        stamp_last_user_id(&conn, "a1", 8791871989).expect("stamp");
        let last: Option<i64> = conn
            .query_row(
                "SELECT last_user_id FROM agent_registry WHERE agent_id='a1'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(last, Some(8791871989));
    }

    #[test]
    fn slice4a_list_routings_empty_when_no_binding() {
        let conn = open_test_conn();
        let rows = list_routings_for(&conn, 999, None).unwrap();
        assert!(rows.is_empty());
    }

    #[test]
    fn slice4a_list_routings_returns_dm_binding() {
        let mut conn = open_test_conn();
        slice4_register_alive(&conn, "a-dm", "alice");
        bind_routing_key(&mut conn, "a-dm", 42, None).unwrap();
        let rows = list_routings_for(&conn, 42, None).unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].agent_id, "a-dm");
        assert_eq!(rows[0].agent_name, "alice");
    }

    #[test]
    fn slice4a_list_routings_topic_aware() {
        // KP2/KP3 scenario: same group, two topics, different CLIs.
        // list_routings_for(chat=500, thread=Some(7)) returns ONLY the
        // CLI bound to topic α; it must NOT return the CLI bound to
        // topic β (8).
        let mut conn = open_test_conn();
        slice4_register_alive(&conn, "cli-b", "bob");
        slice4_register_alive(&conn, "cli-c", "carol");
        bind_routing_key(&mut conn, "cli-b", 500, Some(7)).unwrap();
        bind_routing_key(&mut conn, "cli-c", 500, Some(8)).unwrap();
        let rows_alpha = list_routings_for(&conn, 500, Some(7)).unwrap();
        assert_eq!(rows_alpha.len(), 1);
        assert_eq!(rows_alpha[0].agent_id, "cli-b");
        let rows_beta = list_routings_for(&conn, 500, Some(8)).unwrap();
        assert_eq!(rows_beta.len(), 1);
        assert_eq!(rows_beta[0].agent_id, "cli-c");
    }
}
