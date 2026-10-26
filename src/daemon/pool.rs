//! Pool dispatch — the daemon routes model calls to a pool of Claude sessions
//! (docs/design/pool-dispatch.md).
//!
//! A gateway asks `/pool/<pool>/dispatch`; the daemon leases a FREE worker (a
//! session started with `claudebase run --pool <pool>`), injects the job into
//! its input as `[pool-job:<pool> <req_id> <callType>]`, and the worker answers
//! with `claudebase pool reply`. The gateway long-polls `/pool/jobs/<req_id>`.
//!
//! The daemon keeps no queue: a dispatch with no free worker is refused and
//! the request stays in the gateway's queue. Busy/free is the lease the daemon
//! holds, never a guess from a terminal.

use std::path::PathBuf;
use std::sync::LazyLock;

use anyhow::{anyhow, bail, Context, Result};
use base64::Engine;
use rusqlite::{params, Connection, OptionalExtension};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};

use crate::daemon::chat::SharedBus;

/// The brain's own budget per call (`HARNESS_STEP_TIMEOUT_MS`, 8 min): a lease
/// that outlives the caller answers nobody.
pub const DEFAULT_TIMEOUT_MS: i64 = 480_000;
pub const MAX_TIMEOUT_MS: i64 = 900_000;
/// A dispatch body, images included.
pub const MAX_BODY: usize = 12 * 1024 * 1024;
const MAX_WAIT_MS: i64 = 30_000;
/// A final job is kept this long after the gateway first read it.
const KEEP_AFTER_READ_MS: i64 = 600_000;
/// And this long if nobody ever reads it.
const KEEP_UNREAD_MS: i64 = 86_400_000;
/// A worker whose session vanished gets this long to come back (a daemon
/// restart, a reconnect) before its job is failed as `worker_lost`.
const LOST_GRACE_MS: i64 = 15_000;
/// Offline workers drop out of `/workers` after this long.
const OFFLINE_SHOWN_MS: i64 = 3_600_000;
/// A suspect worker (a job expired on it) is trusted again after this long,
/// so a pool that hit the subscription's rate limit recovers by itself: if the
/// limit still holds, the next job expires too and the worker is suspect again
/// — backpressure, not a loop. Operator decision 2026-10-26: 8 minutes.
/// `CLAUDEBASE_POOL_SUSPECT_COOLDOWN_MS` overrides it on the daemon.
const SUSPECT_COOLDOWN_MS: i64 = 480_000;

fn suspect_cooldown_ms() -> i64 {
    std::env::var("CLAUDEBASE_POOL_SUSPECT_COOLDOWN_MS")
        .ok()
        .and_then(|v| v.parse::<i64>().ok())
        .filter(|v| *v >= 0)
        .unwrap_or(SUSPECT_COOLDOWN_MS)
}

/// Wakes long-polls when any job changes. Waiters re-check their own job.
static CHANGED: LazyLock<tokio::sync::Notify> = LazyLock::new(tokio::sync::Notify::new);

fn changed() {
    CHANGED.notify_waiters();
}

fn now_ms() -> i64 {
    crate::daemon::chat::now_millis()
}

pub fn ensure_schema(conn: &Connection) -> Result<()> {
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS pool_members (
           agent_id         TEXT PRIMARY KEY,
           pool             TEXT NOT NULL,
           capacity         INTEGER NOT NULL DEFAULT 1,
           suspect          INTEGER NOT NULL DEFAULT 0,
           joined_at        INTEGER NOT NULL,
           last_assigned_at INTEGER
         );
         CREATE INDEX IF NOT EXISTS pool_members_pool ON pool_members(pool);
         CREATE TABLE IF NOT EXISTS pool_jobs (
           req_id          TEXT PRIMARY KEY,
           pool            TEXT NOT NULL,
           worker          TEXT NOT NULL,
           call_type       TEXT,
           state           TEXT NOT NULL,
           dir             TEXT NOT NULL,
           created_at      INTEGER NOT NULL,
           expires_at      INTEGER NOT NULL,
           finished_at     INTEGER,
           first_read_at   INTEGER,
           worker_gone_at  INTEGER,
           result          TEXT,
           error           TEXT,
           message         TEXT
         );
         CREATE INDEX IF NOT EXISTS pool_jobs_worker ON pool_jobs(worker, state);",
    )
    .context("pool: ensure schema")?;
    // Added with the suspect cooldown; databases from 0.18 lack it.
    let has: bool = conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM pragma_table_info('pool_members') WHERE name = 'suspect_since')",
        [],
        |r| r.get(0),
    )?;
    if !has {
        conn.execute_batch("ALTER TABLE pool_members ADD COLUMN suspect_since INTEGER")?;
    }
    Ok(())
}

fn check_name(what: &str, s: &str, max: usize, extra: &[char]) -> Result<()> {
    if s.is_empty() || s.len() > max || !s.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_' || extra.contains(&c)) {
        bail!("{what} must be 1-{max} characters of [a-zA-Z0-9_-{}]", extra.iter().collect::<String>());
    }
    Ok(())
}

pub fn check_pool(pool: &str) -> Result<()> {
    check_name("a pool name", pool, 64, &[])
}

/// The id goes into a command line the worker runs, so it is one safe word.
pub fn check_req_id(req_id: &str) -> Result<()> {
    check_name("req_id", req_id, 128, &['.', ':'])
}

fn jobs_root() -> PathBuf {
    crate::daemon::remote::claudebase_dir().join("pool")
}

fn job_dir(req_id: &str) -> PathBuf {
    jobs_root().join(&hex::encode(Sha256::digest(req_id.as_bytes()))[..24])
}

// ---------------------------------------------------------------------------
// Membership
// ---------------------------------------------------------------------------

/// `claudebase run --pool <pool>` registered `agent_id`.
pub fn join(conn: &Connection, agent_id: &str, pool: &str, capacity: i64) -> Result<()> {
    check_pool(pool)?;
    ensure_schema(conn)?;
    conn.execute(
        "INSERT INTO pool_members (agent_id, pool, capacity, joined_at) VALUES (?1, ?2, ?3, ?4) \
         ON CONFLICT(agent_id) DO UPDATE SET pool = excluded.pool, capacity = excluded.capacity",
        params![agent_id, pool, capacity.clamp(1, 16), now_ms()],
    )?;
    Ok(())
}

struct Worker {
    agent_id: String,
    nick: String,
    alive: bool,
    capacity: i64,
    suspect: bool,
    last_seen: i64,
    last_assigned: i64,
    leased: Vec<String>,
}

impl Worker {
    fn state(&self) -> &'static str {
        if !self.alive {
            "offline"
        } else if self.suspect {
            "suspect"
        } else if self.leased.len() as i64 >= self.capacity {
            "busy"
        } else {
            "free"
        }
    }
}

fn workers(conn: &Connection, pool: &str) -> Result<Vec<Worker>> {
    let mut stmt = conn.prepare(
        "SELECT m.agent_id, COALESCE(r.agent_name, m.agent_id), COALESCE(r.state, 'dead'), m.capacity, m.suspect, \
                COALESCE(r.last_pinged_at, 0), COALESCE(m.last_assigned_at, 0) \
         FROM pool_members m LEFT JOIN agent_registry r ON r.agent_id = m.agent_id \
         WHERE m.pool = ?1",
    )?;
    let mut out: Vec<Worker> = stmt
        .query_map([pool], |r| {
            Ok(Worker {
                agent_id: r.get(0)?,
                nick: r.get(1)?,
                alive: r.get::<_, String>(2)? == "alive",
                capacity: r.get(3)?,
                suspect: r.get::<_, i64>(4)? != 0,
                last_seen: r.get(5)?,
                last_assigned: r.get(6)?,
                leased: Vec::new(),
            })
        })?
        .collect::<rusqlite::Result<_>>()?;
    let mut leased = conn.prepare("SELECT req_id FROM pool_jobs WHERE worker = ?1 AND state = 'leased' ORDER BY created_at")?;
    for w in &mut out {
        w.leased = leased.query_map([&w.agent_id], |r| r.get(0))?.collect::<rusqlite::Result<_>>()?;
    }
    Ok(out)
}

pub fn workers_json(conn: &Connection, pool: &str) -> Result<Option<Value>> {
    ensure_schema(conn)?;
    let all = workers(conn, pool)?;
    if all.is_empty() {
        return Ok(None);
    }
    let now = now_ms();
    let shown: Vec<&Worker> = all.iter().filter(|w| w.alive || now - w.last_seen < OFFLINE_SHOWN_MS).collect();
    let free = shown.iter().filter(|w| w.state() == "free").count();
    Ok(Some(json!({
        "pool": pool,
        "free": free,
        "workers": shown.iter().map(|w| json!({
            "nick": w.nick,
            "agent_id": w.agent_id,
            "state": w.state(),
            "capacity": w.capacity,
            "in_flight": w.leased.len(),
            "req_ids": w.leased,
            "last_seen_ms": w.last_seen,
        })).collect::<Vec<_>>(),
    })))
}

// ---------------------------------------------------------------------------
// Jobs
// ---------------------------------------------------------------------------

fn job_json(conn: &Connection, req_id: &str) -> Result<Option<Value>> {
    conn.query_row(
        "SELECT j.req_id, j.state, COALESCE(r.agent_name, j.worker), j.created_at, j.expires_at, j.finished_at, \
                j.result, j.error, j.message, j.call_type \
         FROM pool_jobs j LEFT JOIN agent_registry r ON r.agent_id = j.worker WHERE j.req_id = ?1",
        [req_id],
        |r| {
            let state: String = r.get(1)?;
            let created: i64 = r.get(3)?;
            let finished: Option<i64> = r.get(5)?;
            let result: Option<String> = r.get(6)?;
            let mut v = json!({
                "req_id": r.get::<_, String>(0)?,
                "state": state,
                "worker": r.get::<_, String>(2)?,
                "expires_at_ms": r.get::<_, i64>(4)?,
                "call_type": r.get::<_, Option<String>>(9)?,
            });
            if let Some(f) = finished {
                v["duration_ms"] = json!(f - created);
            }
            if let Some(res) = result {
                v["result"] = serde_json::from_str(&res).unwrap_or(Value::String(res));
            }
            if let Some(e) = r.get::<_, Option<String>>(7)? {
                v["error"] = json!(e);
            }
            if let Some(m) = r.get::<_, Option<String>>(8)? {
                v["message"] = json!(m);
            }
            Ok(v)
        },
    )
    .optional()
    .map_err(Into::into)
}

fn is_final(state: &str) -> bool {
    matches!(state, "done" | "failed" | "expired" | "cancelled")
}

/// A decoded image to store with the job.
struct Image {
    ext: &'static str,
    mime: String,
    bytes: Vec<u8>,
}

fn ext_for(mime: &str) -> &'static str {
    match mime {
        "image/jpeg" | "image/jpg" => "jpg",
        "image/png" => "png",
        "image/webp" => "webp",
        "image/gif" => "gif",
        _ => "bin",
    }
}

fn mime_for(path: &str) -> String {
    let lower = path.to_ascii_lowercase();
    let m = if lower.ends_with(".jpg") || lower.ends_with(".jpeg") {
        "image/jpeg"
    } else if lower.ends_with(".png") {
        "image/png"
    } else if lower.ends_with(".webp") {
        "image/webp"
    } else if lower.ends_with(".gif") {
        "image/gif"
    } else {
        "application/octet-stream"
    };
    m.to_string()
}

/// `images[]` of a dispatch: a path on THIS host, or base64 with a mime.
fn read_images(v: &Value) -> Result<Vec<Image>> {
    let mut out = Vec::new();
    for (i, img) in v.as_array().into_iter().flatten().enumerate() {
        let (mime, bytes) = if let Some(path) = img.get("path").and_then(Value::as_str) {
            let bytes = std::fs::read(path).with_context(|| format!("images[{i}]: read {path} on the daemon's host"))?;
            (img.get("mime").and_then(Value::as_str).map(str::to_owned).unwrap_or_else(|| mime_for(path)), bytes)
        } else if let Some(b64) = img.get("base64").and_then(Value::as_str) {
            let bytes = base64::engine::general_purpose::STANDARD
                .decode(b64.trim())
                .with_context(|| format!("images[{i}]: base64 does not decode"))?;
            (img.get("mime").and_then(Value::as_str).unwrap_or("image/jpeg").to_owned(), bytes)
        } else {
            bail!("images[{i}] needs `path` or `base64`");
        };
        if !mime.starts_with("image/") {
            bail!("images[{i}]: `{mime}` is not an image type");
        }
        out.push(Image { ext: ext_for(&mime), mime, bytes });
    }
    Ok(out)
}

pub enum Dispatch {
    /// Leased to a worker: deliver `job_line` to `worker`.
    Leased { job: Value, worker: String, line: String },
    /// `req_id` was dispatched before; its current state.
    Existing(Value),
    NoFreeWorker { free: usize, busy: usize },
    NoSuchPool,
}

/// Lease a free worker of `pool` for this job. Runs under the daemon's single
/// writer, so two dispatches cannot take the same slot.
pub fn dispatch(conn: &Connection, pool: &str, body: &Value) -> Result<Dispatch> {
    check_pool(pool)?;
    ensure_schema(conn)?;
    let req_id = body.get("req_id").and_then(Value::as_str).ok_or_else(|| anyhow!("req_id required"))?;
    check_req_id(req_id)?;
    if let Some(existing) = job_json(conn, req_id)? {
        return Ok(Dispatch::Existing(existing));
    }
    let payload = body.get("payload").cloned().ok_or_else(|| anyhow!("payload required"))?;
    let timeout = body.get("timeout_ms").and_then(Value::as_i64).unwrap_or(DEFAULT_TIMEOUT_MS);
    if !(1_000..=MAX_TIMEOUT_MS).contains(&timeout) {
        bail!("timeout_ms must be 1000..={MAX_TIMEOUT_MS}");
    }
    let call_type = payload
        .pointer("/meta/callType")
        .and_then(Value::as_str)
        .filter(|c| check_name("callType", c, 64, &[]).is_ok())
        .map(str::to_owned);
    let images = read_images(body.get("images").unwrap_or(&Value::Null))?;

    let all = workers(conn, pool)?;
    if all.is_empty() {
        return Ok(Dispatch::NoSuchPool);
    }
    let Some(worker) = all
        .iter()
        .filter(|w| w.state() == "free")
        .min_by_key(|w| (w.leased.len(), w.last_assigned))
    else {
        let busy = all.iter().filter(|w| matches!(w.state(), "busy" | "suspect")).count();
        return Ok(Dispatch::NoFreeWorker { free: 0, busy });
    };

    // The job's files first; the row that makes it visible last.
    let dir = job_dir(req_id);
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).with_context(|| format!("create {}", dir.display()))?;
    let mut manifest = Vec::new();
    for (i, img) in images.iter().enumerate() {
        let name = format!("image-{:02}.{}", i + 1, img.ext);
        std::fs::write(dir.join(&name), &img.bytes)?;
        manifest.push(json!({ "file": name, "mime": img.mime }));
    }
    std::fs::write(dir.join("payload.json"), serde_json::to_vec_pretty(&payload)?)?;
    std::fs::write(dir.join("images.json"), serde_json::to_vec(&manifest)?)?;

    let now = now_ms();
    conn.execute(
        "INSERT INTO pool_jobs (req_id, pool, worker, call_type, state, dir, created_at, expires_at) \
         VALUES (?1, ?2, ?3, ?4, 'leased', ?5, ?6, ?7)",
        params![req_id, pool, worker.agent_id, call_type, dir.to_string_lossy(), now, now + timeout],
    )?;
    conn.execute("UPDATE pool_members SET last_assigned_at = ?1 WHERE agent_id = ?2", params![now, worker.agent_id])?;
    let job = job_json(conn, req_id)?.expect("just inserted");
    changed();
    let line = format!(
        "claudebase pool job {req_id} -- the payload and {} image(s); answer with: claudebase pool reply {req_id} \
         (JSON on stdin), or claudebase pool fail {req_id} \"<reason>\"",
        images.len()
    );
    Ok(Dispatch::Leased { job, worker: worker.agent_id.clone(), line })
}

/// The job as the gateway sees it; a final state read for the first time
/// starts its retention clock.
pub fn read_job(conn: &Connection, req_id: &str) -> Result<Option<Value>> {
    ensure_schema(conn)?;
    let job = job_json(conn, req_id)?;
    if let Some(j) = &job {
        if is_final(j["state"].as_str().unwrap_or("")) {
            conn.execute(
                "UPDATE pool_jobs SET first_read_at = ?1 WHERE req_id = ?2 AND first_read_at IS NULL",
                params![now_ms(), req_id],
            )?;
        }
    }
    Ok(job)
}

/// Returns the worker and pool it was leased to, and the job.
pub fn cancel(conn: &Connection, req_id: &str) -> Result<Option<(String, String, Value)>> {
    ensure_schema(conn)?;
    let row: Option<(String, String)> = conn
        .query_row("SELECT worker, pool FROM pool_jobs WHERE req_id = ?1 AND state = 'leased'", [req_id], |r| {
            Ok((r.get(0)?, r.get(1)?))
        })
        .optional()?;
    let Some((worker, pool)) = row else {
        return Ok(None);
    };
    conn.execute(
        "UPDATE pool_jobs SET state = 'cancelled', finished_at = ?1 WHERE req_id = ?2",
        params![now_ms(), req_id],
    )?;
    changed();
    Ok(Some((worker, pool, job_json(conn, req_id)?.expect("exists"))))
}

// ---------------------------------------------------------------------------
// The worker's side
// ---------------------------------------------------------------------------

fn owned_job(conn: &Connection, agent_id: &str, req_id: &str) -> Result<(String, String)> {
    let row: Option<(String, String, String)> = conn
        .query_row("SELECT worker, state, dir FROM pool_jobs WHERE req_id = ?1", [req_id], |r| {
            Ok((r.get(0)?, r.get(1)?, r.get(2)?))
        })
        .optional()?;
    let Some((worker, state, dir)) = row else {
        bail!("no pool job {req_id}");
    };
    if worker != agent_id {
        bail!("pool job {req_id} was given to another session");
    }
    Ok((state, dir))
}

/// The payload and images, for `claudebase pool job`.
pub fn worker_job(conn: &Connection, agent_id: &str, req_id: &str) -> Result<Value> {
    ensure_schema(conn)?;
    let (state, dir) = owned_job(conn, agent_id, req_id)?;
    if state != "leased" {
        bail!("pool job {req_id} is {state}; there is nothing to do");
    }
    let dir = PathBuf::from(dir);
    let payload: Value = serde_json::from_slice(&std::fs::read(dir.join("payload.json"))?)?;
    let manifest: Vec<Value> = serde_json::from_slice(&std::fs::read(dir.join("images.json"))?)?;
    let mut images = Vec::new();
    for m in manifest {
        let file = m["file"].as_str().unwrap_or_default();
        let bytes = std::fs::read(dir.join(file))?;
        images.push(json!({
            "file": file,
            "mime": m["mime"],
            "base64": base64::engine::general_purpose::STANDARD.encode(bytes),
        }));
    }
    let call_type: Option<String> =
        conn.query_row("SELECT call_type FROM pool_jobs WHERE req_id = ?1", [req_id], |r| r.get(0))?;
    Ok(json!({ "req_id": req_id, "call_type": call_type, "payload": payload, "images": images }))
}

/// `claudebase pool reply`. In time: the job is done and the worker free. Late
/// (the lease expired): the answer is discarded — the gateway has moved on —
/// but the worker is trusted again.
pub fn worker_reply(conn: &Connection, agent_id: &str, req_id: &str, result: &Value) -> Result<Value> {
    ensure_schema(conn)?;
    let (state, _) = owned_job(conn, agent_id, req_id)?;
    conn.execute("UPDATE pool_members SET suspect = 0, suspect_since = NULL WHERE agent_id = ?1", [agent_id])?;
    let out = match state.as_str() {
        "leased" => {
            conn.execute(
                "UPDATE pool_jobs SET state = 'done', result = ?1, finished_at = ?2 WHERE req_id = ?3",
                params![serde_json::to_string(result)?, now_ms(), req_id],
            )?;
            json!({ "accepted": true, "req_id": req_id })
        }
        other => json!({ "accepted": false, "req_id": req_id, "state": other,
                         "note": "too late: the job is no longer yours to answer; you are free for the next one" }),
    };
    changed();
    Ok(out)
}

pub fn worker_fail(conn: &Connection, agent_id: &str, req_id: &str, reason: &str) -> Result<Value> {
    ensure_schema(conn)?;
    let (state, _) = owned_job(conn, agent_id, req_id)?;
    conn.execute("UPDATE pool_members SET suspect = 0, suspect_since = NULL WHERE agent_id = ?1", [agent_id])?;
    if state == "leased" {
        conn.execute(
            "UPDATE pool_jobs SET state = 'failed', error = 'worker_failed', message = ?1, finished_at = ?2 \
             WHERE req_id = ?3",
            params![reason.chars().take(2000).collect::<String>(), now_ms(), req_id],
        )?;
    }
    changed();
    Ok(json!({ "req_id": req_id, "failed": state == "leased", "state": if state == "leased" { "failed" } else { state.as_str() } }))
}

pub fn worker_ready(conn: &Connection, agent_id: &str) -> Result<Value> {
    ensure_schema(conn)?;
    let n = conn.execute("UPDATE pool_members SET suspect = 0, suspect_since = NULL WHERE agent_id = ?1", [agent_id])?;
    if n == 0 {
        bail!("this session is not a pool worker (start it with claudebase run --pool <name>)");
    }
    changed();
    Ok(json!({ "ready": true }))
}

// ---------------------------------------------------------------------------
// Sweep: expiry, lost workers, retention
// ---------------------------------------------------------------------------

/// Returns whether anything changed.
pub fn sweep(conn: &Connection, now: i64) -> Result<bool> {
    ensure_schema(conn)?;
    let mut changed_any = false;
    // Leases that ran out: the job is expired and the worker is not trusted
    // with another until it answers or says it is ready.
    let expired: Vec<(String, String)> = {
        let mut s = conn.prepare("SELECT req_id, worker FROM pool_jobs WHERE state = 'leased' AND expires_at <= ?1")?;
        let rows = s.query_map([now], |r| Ok((r.get(0)?, r.get(1)?)))?;
        rows.collect::<rusqlite::Result<_>>()?
    };
    for (req_id, worker) in &expired {
        conn.execute("UPDATE pool_jobs SET state = 'expired', finished_at = ?1 WHERE req_id = ?2", params![now, req_id])?;
        conn.execute(
            "UPDATE pool_members SET suspect = 1, suspect_since = ?1 WHERE agent_id = ?2",
            params![now, worker],
        )?;
        changed_any = true;
    }
    // Suspects whose cooldown has passed get the next job again.
    let cleared = conn.execute(
        "UPDATE pool_members SET suspect = 0, suspect_since = NULL \
         WHERE suspect = 1 AND COALESCE(suspect_since, 0) <= ?1",
        params![now - suspect_cooldown_ms()],
    )?;
    if cleared > 0 {
        tracing::info!(cleared, "pool: suspect workers trusted again after the cooldown");
        changed_any = true;
    }
    // Workers that vanished: a grace period first, for a reconnect.
    let leased: Vec<(String, bool, Option<i64>)> = {
        let mut s = conn.prepare(
            "SELECT j.req_id, COALESCE(r.state, 'dead') = 'alive', j.worker_gone_at \
             FROM pool_jobs j LEFT JOIN agent_registry r ON r.agent_id = j.worker WHERE j.state = 'leased'",
        )?;
        let rows = s.query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?;
        rows.collect::<rusqlite::Result<_>>()?
    };
    for (req_id, alive, gone) in leased {
        match (alive, gone) {
            (true, Some(_)) => {
                conn.execute("UPDATE pool_jobs SET worker_gone_at = NULL WHERE req_id = ?1", [&req_id])?;
            }
            (false, None) => {
                conn.execute("UPDATE pool_jobs SET worker_gone_at = ?1 WHERE req_id = ?2", params![now, req_id])?;
            }
            (false, Some(t)) if now - t >= LOST_GRACE_MS => {
                conn.execute(
                    "UPDATE pool_jobs SET state = 'failed', error = 'worker_lost', finished_at = ?1 WHERE req_id = ?2",
                    params![now, req_id],
                )?;
                changed_any = true;
            }
            _ => {}
        }
    }
    // Retention.
    let old: Vec<(String, String)> = {
        let mut s = conn.prepare(
            "SELECT req_id, dir FROM pool_jobs WHERE state != 'leased' AND \
             ((first_read_at IS NOT NULL AND first_read_at <= ?1) OR (finished_at IS NOT NULL AND finished_at <= ?2))",
        )?;
        let rows = s.query_map(params![now - KEEP_AFTER_READ_MS, now - KEEP_UNREAD_MS], |r| Ok((r.get(0)?, r.get(1)?)))?;
        rows.collect::<rusqlite::Result<_>>()?
    };
    for (req_id, dir) in old {
        conn.execute("DELETE FROM pool_jobs WHERE req_id = ?1", [&req_id])?;
        let _ = std::fs::remove_dir_all(dir);
    }
    if changed_any {
        changed();
    }
    Ok(changed_any)
}

/// Run the sweep every two seconds for the life of the daemon.
pub fn spawn_sweeper() {
    tokio::spawn(async {
        let mut tick = tokio::time::interval(std::time::Duration::from_secs(2));
        loop {
            tick.tick().await;
            let r = tokio::task::spawn_blocking(|| crate::daemon::chat::with_writer(|c| sweep(c, now_ms()))).await;
            if let Ok(Err(e)) = r {
                tracing::warn!(error = %format!("{e:#}"), "pool: sweep failed");
            }
        }
    });
}

// ---------------------------------------------------------------------------
// Delivery into the worker's input
// ---------------------------------------------------------------------------

/// Store the job line for `worker` and publish it — the same path a callback
/// takes, so it survives a reconnect as backlog.
async fn deliver(bus: &SharedBus, pool: &str, worker: &str, req_id: &str, call_type: Option<&str>, line: &str) -> Result<()> {
    let (from, worker_s, content) = (format!("pool:{pool}"), worker.to_owned(), line.to_owned());
    let message_id = tokio::task::spawn_blocking(move || {
        crate::daemon::chat::with_writer(|conn| {
            Ok(crate::daemon::agent_registry::send_message(conn, &from, &worker_s, &content, now_ms())?.message_id)
        })
    })
    .await
    .map_err(|e| anyhow!("pool deliver task: {e}"))??;
    let frame = json!({
        "jsonrpc": "2.0",
        "method": "notifications/claude/channel",
        "params": {
            "content": line,
            "meta": {
                "source": "pool",
                "pool": pool,
                "req_id": req_id,
                "call_type": call_type,
                "target_agent_id": worker,
                // What a supervisor older than `source: pool` shows instead.
                "from_agent": format!("pool-{pool}"),
                "chat_id": format!("agent:{worker}"),
                "thread": format!("agent:{worker}"),
                "message_id": message_id,
                "ts": chrono::Utc::now().to_rfc3339(),
            }
        }
    });
    bus.publish(&format!("agent:{worker}"), frame).await;
    Ok(())
}

// ---------------------------------------------------------------------------
// HTTP (on the callback listener)
// ---------------------------------------------------------------------------

/// `/pool/*`. Returns the HTTP status line and the JSON body.
pub async fn http(method: &str, target: &str, token: Option<&str>, body: &[u8], bus: &SharedBus) -> (&'static str, Value) {
    let err = |status: &'static str, code: &str, message: String| (status, json!({ "error": code, "message": message }));
    let token = token.unwrap_or("").to_owned();
    let authed = tokio::task::spawn_blocking(move || -> Result<Option<String>> {
        let conn = crate::daemon::chat::open_chat_db()?;
        crate::daemon::remote::authenticate(&conn, crate::daemon::remote::TokenKind::Pool, &token)
    })
    .await;
    let gateway = match authed {
        Ok(Ok(Some(name))) => name,
        Ok(Ok(None)) => return err("401 Unauthorized", "unauthorized", "missing or unknown X-Api-Token (claudebase pool token issue <name>)".into()),
        _ => return err("500 Internal Server Error", "internal", "token check failed".into()),
    };

    let (path, query) = target.split_once('?').unwrap_or((target, ""));
    let wait_ms = query
        .split('&')
        .find_map(|kv| kv.strip_prefix("wait_ms="))
        .and_then(|v| v.parse::<i64>().ok())
        .unwrap_or(0)
        .clamp(0, MAX_WAIT_MS);
    let parts: Vec<&str> = path.trim_matches('/').split('/').collect();
    match (method, parts.as_slice()) {
        ("GET", ["pool", "jobs", req_id]) => wait_for(req_id, wait_ms).await,
        ("POST", ["pool", "jobs", req_id, "cancel"]) => {
            let id = req_id.to_string();
            match tokio::task::spawn_blocking(move || crate::daemon::chat::with_writer(|c| cancel(c, &id))).await {
                Ok(Ok(Some((worker, pool, job)))) => {
                    let _ = deliver(bus, &pool, &worker, req_id, None,
                        &format!("pool job {req_id} was cancelled by the gateway; stop working on it, do not reply")).await;
                    ("200 OK", job)
                }
                Ok(Ok(None)) => err("404 Not Found", "no_such_job", format!("no leased job {req_id}")),
                _ => err("500 Internal Server Error", "internal", "cancel failed".into()),
            }
        }
        ("GET", ["pool", pool, "workers"]) => {
            let p = pool.to_string();
            match tokio::task::spawn_blocking(move || {
                let conn = crate::daemon::chat::open_chat_db()?;
                workers_json(&conn, &p)
            })
            .await
            {
                Ok(Ok(Some(v))) => ("200 OK", v),
                Ok(Ok(None)) => err("404 Not Found", "no_such_pool", format!("no session has joined pool `{pool}`")),
                Ok(Err(e)) => err("400 Bad Request", "bad_request", format!("{e:#}")),
                Err(_) => err("500 Internal Server Error", "internal", "workers failed".into()),
            }
        }
        ("POST", ["pool", pool, "dispatch"]) => {
            if body.len() > MAX_BODY {
                return err("413 Payload Too Large", "too_large", format!("over {MAX_BODY} bytes"));
            }
            let parsed: Value = match serde_json::from_slice(body) {
                Ok(v) => v,
                Err(e) => return err("400 Bad Request", "bad_request", format!("body is not JSON: {e}")),
            };
            let wait = parsed.get("wait_ms").and_then(Value::as_i64).unwrap_or(0).clamp(0, MAX_WAIT_MS);
            let (p, b) = (pool.to_string(), parsed.clone());
            let outcome = tokio::task::spawn_blocking(move || crate::daemon::chat::with_writer(|c| dispatch(c, &p, &b))).await;
            match outcome {
                Ok(Ok(Dispatch::Leased { job, worker, line })) => {
                    let req_id = job["req_id"].as_str().unwrap_or_default().to_owned();
                    let call_type = job["call_type"].as_str().map(str::to_owned);
                    if let Err(e) = deliver(bus, pool, &worker, &req_id, call_type.as_deref(), &line).await {
                        tracing::warn!(%req_id, error = %format!("{e:#}"), "pool: delivery failed; the lease will expire");
                    }
                    tracing::info!(%gateway, pool = %pool, %req_id, worker = %job["worker"], "pool: dispatched");
                    if wait > 0 {
                        let (status, v) = wait_for(&req_id, wait).await;
                        return (if v["state"] == "leased" { "202 Accepted" } else { status }, v);
                    }
                    ("202 Accepted", job)
                }
                Ok(Ok(Dispatch::Existing(job))) => ("200 OK", job),
                Ok(Ok(Dispatch::NoFreeWorker { free, busy })) => (
                    "503 Service Unavailable",
                    json!({ "error": "no_free_worker", "free": free, "busy": busy }),
                ),
                Ok(Ok(Dispatch::NoSuchPool)) => err("404 Not Found", "no_such_pool", format!("no session has joined pool `{pool}`")),
                Ok(Err(e)) => err("400 Bad Request", "bad_request", format!("{e:#}")),
                Err(_) => err("500 Internal Server Error", "internal", "dispatch failed".into()),
            }
        }
        _ => err("404 Not Found", "no_such_route", format!("{method} {path}")),
    }
}

/// Long-poll one job until it is final or `wait_ms` passes.
async fn wait_for(req_id: &str, wait_ms: i64) -> (&'static str, Value) {
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_millis(wait_ms as u64);
    loop {
        // Registered before the check, so a change between the check and the
        // wait is not missed.
        let notified = CHANGED.notified();
        let id = req_id.to_owned();
        let job = tokio::task::spawn_blocking(move || {
            let conn = crate::daemon::chat::open_chat_db()?;
            read_job(&conn, &id)
        })
        .await;
        let job = match job {
            Ok(Ok(Some(j))) => j,
            Ok(Ok(None)) => return ("404 Not Found", json!({ "error": "no_such_job", "message": format!("no job {req_id}") })),
            _ => return ("500 Internal Server Error", json!({ "error": "internal" })),
        };
        if is_final(job["state"].as_str().unwrap_or("")) || tokio::time::Instant::now() >= deadline {
            return ("200 OK", job);
        }
        let _ = tokio::time::timeout_at(deadline, notified).await;
    }
}

// ---------------------------------------------------------------------------
// Daemon tools for the worker CLI (`claudebase pool job|reply|fail|ready`)
// ---------------------------------------------------------------------------

pub async fn handle_tool(tool: &str, args: Value) -> Result<Value> {
    let tool = tool.to_owned();
    tokio::task::spawn_blocking(move || -> Result<Value> {
        let agent_id = args.get("agent_id").and_then(Value::as_str).unwrap_or("").to_owned();
        let token = args.get("session_token").and_then(Value::as_str).unwrap_or("").to_owned();
        {
            let conn = crate::daemon::chat::open_chat_db()?;
            if agent_id.is_empty() || !crate::daemon::agent_registry::resolve_session_token(&conn, &agent_id, &token)? {
                bail!("pool commands need the session that holds the job (started with claudebase run --pool …)");
            }
        }
        let req_id = || -> Result<String> {
            let r = args.get("req_id").and_then(Value::as_str).unwrap_or("").to_owned();
            check_req_id(&r)?;
            Ok(r)
        };
        match tool.as_str() {
            "pool_job" => {
                let conn = crate::daemon::chat::open_chat_db()?;
                worker_job(&conn, &agent_id, &req_id()?)
            }
            "pool_reply" => {
                let r = req_id()?;
                let result = args.get("result").cloned().ok_or_else(|| anyhow!("result required"))?;
                crate::daemon::chat::with_writer(|c| worker_reply(c, &agent_id, &r, &result))
            }
            "pool_fail" => {
                let r = req_id()?;
                let reason = args.get("reason").and_then(Value::as_str).unwrap_or("").to_owned();
                crate::daemon::chat::with_writer(|c| worker_fail(c, &agent_id, &r, &reason))
            }
            "pool_ready" => crate::daemon::chat::with_writer(|c| worker_ready(c, &agent_id)),
            other => bail!("unknown pool tool `{other}`"),
        }
    })
    .await
    .map_err(|e| anyhow!("pool task failed: {e}"))?
}
