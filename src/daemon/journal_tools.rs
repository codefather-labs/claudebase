//! `journal_*` tools: the daemon side of `claudebase journal`.
//!
//! The daemon is the only process that opens a journal (see `crate::journal`).
//! Every call names its project the same way, and the project is never taken
//! from the caller's word for it:
//!
//! - **A session** — `agent_id` + `session_token`, the pair `claudebase run`
//!   exports into the session. The project is the directory that session was
//!   started in, as its supervisor registered it. Writing requires this: an
//!   entry always belongs to a session that can be named, and a command typed
//!   in `src/` still lands in the project's journal.
//! - **A directory** — `cwd`, for reading from a plain terminal. It resolves to
//!   the nearest enclosing directory that has a journal, so reading from a
//!   subdirectory finds the project's. A directory read never creates a file.
//!
//! Nothing here ever reaches another project's journal: the file is chosen by
//! the resolved path alone, and each file checks it belongs to that path.

use std::path::{Path, PathBuf};

use anyhow::{anyhow, bail, Context, Result};
use serde_json::{json, Value};

use crate::journal::{self, Filter, NewEntry, SearchMode};

/// Who is asking, and therefore which journal.
struct Caller {
    /// What the journal is filed under (`journal::project_key`).
    project: PathBuf,
    /// The directory the caller works in — on ITS machine, which for a
    /// session joined from elsewhere is not this one.
    path: PathBuf,
    /// Present for a session; `None` for a directory read.
    agent_id: Option<String>,
    nick: Option<String>,
}

/// Run one `journal_*` tool. Errors are operator-facing messages.
pub async fn handle(tool: &str, args: Value) -> Result<Value> {
    let tool = tool.to_string();
    let result = tokio::task::spawn_blocking(move || dispatch(&tool, &args).map(|v| (tool, v)))
        .await
        .map_err(|e| anyhow!("journal task failed: {e}"))?;
    let (tool, value) = result?;
    // Vectors are computed after the answer, so a write never waits for the
    // encoder; a search that just ran also tops up anything still missing
    // (entries imported from old insights, or written before the model).
    if matches!(tool.as_str(), "journal_add" | "journal_search" | "journal_import") {
        if let Some(project) = value.get("project").and_then(Value::as_str) {
            spawn_embedding(PathBuf::from(project));
        }
    }
    Ok(value)
}

/// JSON-RPC code for a failed tool call. A refused request (no session, bad
/// arguments, unknown entry) is the caller's to fix: -32602. A database that
/// stayed locked past the busy timeout, or any other storage failure, is the
/// daemon's: -32603, so the caller can tell "retry" from "change the call".
pub fn error_code(e: &anyhow::Error) -> i64 {
    let storage = e.chain().any(|c| c.downcast_ref::<rusqlite::Error>().is_some());
    if storage { -32603 } else { -32602 }
}

fn dispatch(tool: &str, args: &Value) -> Result<Value> {
    let dir = journal::journals_dir();
    match tool {
        "journal_add" => add(&dir, args),
        "journal_search" => search(&dir, args),
        "journal_list" => list(&dir, args),
        "journal_get" => get(&dir, args),
        "journal_delete" => delete(&dir, args),
        "journal_tags" => tags(&dir, args),
        "journal_categories" => categories(&dir, args),
        "journal_gc" => gc(&dir, args),
        "journal_projects" => Ok(json!({ "journals": journal::list_journals(&dir)? })),
        "journal_import" => import(&dir, args),
        "journal_mark" => mark(&dir, args),
        "journal_unmark" => unmark(&dir, args),
        "journal_close" => close(&dir, args),
        "journal_reopen" => reopen(&dir, args),
        "journal_agenda" => agenda(&dir, args),
        "journal_link" => link(&dir, args),
        "journal_unlink" => unlink(&dir, args),
        other => bail!("unknown journal tool `{other}`"),
    }
}

fn resolve(args: &Value, need_session: bool) -> Result<Caller> {
    let agent_id = str_arg(args, "agent_id");
    let token = str_arg(args, "session_token");
    if let (Some(agent_id), Some(token)) = (agent_id.as_deref(), token.as_deref()) {
        let conn = crate::daemon::chat::open_chat_db().context("open chat.db")?;
        if !crate::daemon::agent_registry::resolve_session_token(&conn, agent_id, token)? {
            bail!(
                "this session is not registered with the daemon (session_token does not match an alive agent) — \
                 if the daemon was restarted, restart the session with `claudebase run`"
            );
        }
        let (cwd, nick, project_id): (Option<String>, Option<String>, Option<String>) = conn.query_row(
            "SELECT cwd, agent_name, project_id FROM agent_registry WHERE agent_id = ?1",
            rusqlite::params![agent_id],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )?;
        let cwd = cwd.filter(|c| !c.is_empty()).ok_or_else(|| {
            anyhow!("the daemon does not know which directory this session was started in; restart it with `claudebase run`")
        })?;
        let path = PathBuf::from(cwd);
        let project = journal::project_key(project_id.as_deref(), &path);
        return Ok(Caller { project, path, agent_id: Some(agent_id.into()), nick });
    }
    if need_session {
        bail!(
            "writing to the journal needs a session started with `claudebase run` \
             (CLAUDEBASE_AGENT_ID / CLAUDEBASE_SESSION_TOKEN are not set) — the journal belongs to the directory \
             that session was started in"
        );
    }
    let cwd = str_arg(args, "cwd").ok_or_else(|| anyhow!("cwd required when there is no session"))?;
    let cwd = PathBuf::from(cwd);
    if !cwd.is_absolute() {
        bail!("cwd must be an absolute path");
    }
    // A repository's journal when it has one; else the closest directory
    // journal, as before (including one not yet adopted under the repository).
    let dir = journal::journals_dir();
    let key = journal::project_key(str_arg(args, "project_id").as_deref(), &cwd);
    let project = if key != cwd && journal::journal_path(&dir, &key).exists() { key } else { nearest_journal(&dir, &cwd) };
    Ok(Caller { project, path: cwd, agent_id: None, nick: None })
}

/// The closest directory at or above `cwd` that has a journal; `cwd` itself
/// when none does.
fn nearest_journal(dir: &Path, cwd: &Path) -> PathBuf {
    cwd.ancestors()
        .find(|p| journal::journal_path(dir, p).exists())
        .unwrap_or(cwd)
        .to_path_buf()
}

/// Open the caller's journal. A session's journal is created on first use, and
/// that is when its old `insights.db` is brought in.
fn open(dir: &Path, caller: &Caller) -> Result<Option<rusqlite::Connection>> {
    let create = caller.agent_id.is_some();
    if create {
        adopt(dir, caller);
    }
    let Some(mut conn) = journal::open(dir, &caller.project, create)? else { return Ok(None) };
    if create {
        // `path` is the caller's own directory; for a session joined from
        // another machine it does not exist here, and there is nothing to import.
        match journal::import_legacy_insights(&mut conn, &caller.path) {
            Ok(n) if n > 0 => tracing::info!(project = %caller.project.display(), imported = n, "journal: imported insights.db"),
            Ok(_) => {}
            Err(e) => tracing::warn!(project = %caller.project.display(), error = %e, "journal: insights.db import failed"),
        }
    }
    Ok(Some(conn))
}

/// File the directory's journal under the repository, the first time a
/// session of that repository writes. Best effort: a failure leaves both
/// journals readable and is logged.
fn adopt(dir: &Path, caller: &Caller) {
    match journal::adopt_path_journal(dir, &caller.project, &caller.path) {
        Ok(true) => tracing::info!(
            from = %caller.path.display(),
            to = %caller.project.display(),
            "journal: filed the directory's journal under its repository"
        ),
        Ok(false) => {}
        Err(e) => tracing::warn!(error = %format!("{e:#}"), "journal: could not adopt the directory journal"),
    }
}

fn project_json(caller: &Caller) -> Value {
    json!(caller.project.to_string_lossy())
}

fn add(dir: &Path, args: &Value) -> Result<Value> {
    let caller = resolve(args, true)?;
    let mut conn = open(dir, &caller)?.expect("a session journal is always created");
    let entry = NewEntry {
        body: str_arg(args, "body").unwrap_or_default(),
        category: str_arg(args, "category").unwrap_or_default(),
        tags: str_list(args, "tags"),
        salience: str_arg(args, "salience").unwrap_or_else(|| "medium".into()),
        // The emitting agent defaults to the session's nick: someone wrote it.
        agent: str_arg(args, "agent").or(caller.nick.clone()),
        agent_id: caller.agent_id.clone(),
        session_id: str_arg(args, "session_id"),
        feature: str_arg(args, "feature"),
        source_artifact: str_arg(args, "source_artifact"),
        flags: str_list(args, "flags"),
        foreign: false,
    };
    let (status, id) = journal::add(&mut conn, &entry, journal::now_secs())?;
    Ok(json!({ "status": status, "id": id, "project": project_json(&caller) }))
}

/// Most entries one `journal_import` call takes; the client sends batches.
pub const MAX_IMPORT_BATCH: usize = 100;

/// Entries carried over from another machine (`chat load`, `journal import`),
/// written into the journal of the directory the command ran in.
///
/// The one write that needs no session: the target is not "the project this
/// session belongs to" but the directory the operator loaded a conversation
/// into — the one it will be resumed from. Each entry still goes through
/// `journal::add`, so the archive (untrusted, from elsewhere) gets the same
/// validation as an agent's `add`, and its own creation time is kept. Loading
/// the same archive twice writes nothing the second time: `add` folds an
/// identical note in the same category into the existing entry.
fn import(dir: &Path, args: &Value) -> Result<Value> {
    let cwd = str_arg(args, "cwd").ok_or_else(|| anyhow!("cwd required: the directory to import into"))?;
    let path = PathBuf::from(cwd);
    if !path.is_absolute() {
        bail!("cwd must be an absolute path");
    }
    let project = journal::project_key(str_arg(args, "project_id").as_deref(), &path);
    let entries = args.get("entries").and_then(Value::as_array).ok_or_else(|| anyhow!("entries required (array)"))?;
    if entries.len() > MAX_IMPORT_BATCH {
        bail!("at most {MAX_IMPORT_BATCH} entries per call, got {}", entries.len());
    }
    let caller = Caller { project, path, agent_id: None, nick: None };
    adopt(dir, &caller);
    let mut conn = journal::open(dir, &caller.project, true)?.expect("created on demand");
    let now = journal::now_secs();
    let (mut written, mut merged, mut refused) = (0, 0, Vec::new());
    for (i, e) in entries.iter().enumerate() {
        // Flags travel as `journal_list` prints them: `[{flag, open, ...}]`.
        let flags: Vec<&Value> = e.get("flags").and_then(Value::as_array).map(|a| a.iter().collect()).unwrap_or_default();
        let entry = NewEntry {
            body: str_arg(e, "body").unwrap_or_default(),
            category: str_arg(e, "category").unwrap_or_default(),
            tags: str_list(e, "tags"),
            salience: str_arg(e, "salience").unwrap_or_else(|| "medium".into()),
            agent: str_arg(e, "agent"),
            // The source machine's session ids mean nothing here.
            agent_id: None,
            session_id: str_arg(e, "session_id"),
            feature: str_arg(e, "feature"),
            source_artifact: str_arg(e, "source_artifact"),
            flags: flags.iter().filter_map(|f| str_arg(f, "flag")).collect(),
            // Its `journal#N` numbers entries of the source journal.
            foreign: true,
        };
        // A creation time from the future would outlive its retention.
        let created = e.get("created_at").and_then(Value::as_i64).filter(|t| *t > 0).unwrap_or(now).min(now);
        match journal::add(&mut conn, &entry, created) {
            Ok((journal::AddStatus::Written, id)) => {
                written += 1;
                for f in flags.iter().filter(|f| f.get("open").and_then(Value::as_bool) == Some(false)) {
                    let (Some(flag), at) = (str_arg(f, "flag"), f.get("closed_at").and_then(Value::as_i64)) else { continue };
                    let at = at.unwrap_or(now).min(now);
                    let _ = journal::close(&mut conn, id, &[flag], str_arg(f, "closed_by").as_deref(), str_arg(f, "resolution").as_deref(), at);
                }
            }
            Ok((_, _)) => merged += 1,
            Err(err) => refused.push(json!({ "index": i, "error": format!("{err:#}") })),
        }
    }
    Ok(json!({ "written": written, "merged": merged, "refused": refused, "project": project_json(&caller) }))
}

fn filter(args: &Value) -> Result<Filter> {
    let salience = match str_arg(args, "salience") {
        Some(s) => Some(journal::normalize_salience(Some(&s))?),
        None => None,
    };
    let since = match args.get("since_secs").and_then(Value::as_i64) {
        Some(secs) if secs >= 0 => Some(journal::now_secs() - secs),
        Some(_) => bail!("since_secs must not be negative"),
        None => None,
    };
    let flags = str_list(args, "flags");
    journal::normalize_flags(&flags)?;
    let open = match str_arg(args, "state").as_deref() {
        None => None,
        Some("open") => Some(true),
        Some("closed") => Some(false),
        Some(other) => bail!("state must be open or closed, not `{other}`"),
    };
    Ok(Filter {
        category: str_arg(args, "category"),
        tags: str_list(args, "tags"),
        agent: str_arg(args, "agent"),
        salience,
        feature: str_arg(args, "feature"),
        since,
        flags,
        open,
    })
}

fn search(dir: &Path, args: &Value) -> Result<Value> {
    let caller = resolve(args, false)?;
    let query = str_arg(args, "query").ok_or_else(|| anyhow!("query required"))?;
    let mode: SearchMode = str_arg(args, "mode").as_deref().unwrap_or("hybrid").parse()?;
    let top_k = args.get("top_k").and_then(Value::as_u64).unwrap_or(5) as usize;
    let filter = filter(args)?;
    let Some(conn) = open(dir, &caller)? else {
        return Ok(json!({ "project": project_json(&caller), "mode": "lexical", "hits": [] }));
    };
    let embedding = if mode != SearchMode::Lexical && crate::encoder::model_cached() {
        match crate::encoder::encode_query(&query) {
            Ok(e) => Some(e),
            Err(e) => {
                tracing::warn!(error = %e, "journal search: encoder unavailable, lexical only");
                None
            }
        }
    } else {
        None
    };
    let (used, hits) = journal::search(&conn, &query, embedding.as_deref(), mode, &filter, top_k)?;
    Ok(json!({ "project": project_json(&caller), "mode": used, "hits": hits }))
}

fn list(dir: &Path, args: &Value) -> Result<Value> {
    let caller = resolve(args, false)?;
    let offset = args.get("offset").and_then(Value::as_u64).unwrap_or(0) as usize;
    let limit = args.get("limit").and_then(Value::as_u64).unwrap_or(10) as usize;
    let filter = filter(args)?;
    let (total, entries) = match open(dir, &caller)? {
        Some(conn) => journal::list(&conn, &filter, offset, limit)?,
        None => (0, Vec::new()),
    };
    Ok(json!({ "project": project_json(&caller), "total": total, "offset": offset, "entries": entries }))
}

fn id_arg(args: &Value) -> Result<i64> {
    args.get("id").and_then(Value::as_i64).ok_or_else(|| anyhow!("id required (integer)"))
}

fn get(dir: &Path, args: &Value) -> Result<Value> {
    let caller = resolve(args, false)?;
    let id = id_arg(args)?;
    let similar = args.get("similar").and_then(Value::as_u64).unwrap_or(5).min(20) as usize;
    let found = match open(dir, &caller)? {
        Some(conn) => match journal::get(&conn, id)? {
            Some(entry) => Some((entry, journal::related(&conn, id, similar)?)),
            None => None,
        },
        None => None,
    };
    let (entry, related) = found.ok_or_else(|| anyhow!("no entry {id} in the journal of {}", caller.project.display()))?;
    Ok(json!({ "project": project_json(&caller), "entry": entry, "related": related }))
}

fn target_arg(args: &Value) -> Result<i64> {
    args.get("to").and_then(Value::as_i64).ok_or_else(|| anyhow!("to required (integer)"))
}

fn link(dir: &Path, args: &Value) -> Result<Value> {
    let caller = resolve(args, true)?;
    let (from, to) = (id_arg(args)?, target_arg(args)?);
    let kind = str_arg(args, "kind").unwrap_or_else(|| "cites".into());
    let mut conn = open(dir, &caller)?.expect("a session journal is always created");
    let added = journal::link(&mut conn, from, to, &kind)?;
    Ok(json!({ "project": project_json(&caller), "id": from, "to": to, "kind": kind, "added": added }))
}

fn unlink(dir: &Path, args: &Value) -> Result<Value> {
    let caller = resolve(args, true)?;
    let (from, to) = (id_arg(args)?, target_arg(args)?);
    let kind = str_arg(args, "kind");
    let conn = open(dir, &caller)?.expect("a session journal is always created");
    let removed = journal::unlink(&conn, from, to, kind.as_deref())?;
    if removed == 0 {
        bail!("entry {from} has no such link to {to}");
    }
    Ok(json!({ "project": project_json(&caller), "id": from, "to": to, "removed": removed }))
}

fn delete(dir: &Path, args: &Value) -> Result<Value> {
    let caller = resolve(args, true)?;
    let id = id_arg(args)?;
    let conn = open(dir, &caller)?.expect("a session journal is always created");
    if !journal::delete(&conn, id)? {
        bail!("no entry {id} in the journal of {}", caller.project.display());
    }
    Ok(json!({ "project": project_json(&caller), "deleted": id }))
}

/// The entry after a flag change, so the caller sees where it now stands.
fn flagged(conn: &rusqlite::Connection, caller: &Caller, id: i64, changed: Vec<String>) -> Result<Value> {
    Ok(json!({ "project": project_json(caller), "id": id, "changed": changed, "entry": journal::get(conn, id)? }))
}

fn mark(dir: &Path, args: &Value) -> Result<Value> {
    let caller = resolve(args, true)?;
    let id = id_arg(args)?;
    let mut conn = open(dir, &caller)?.expect("a session journal is always created");
    let changed = journal::mark(&mut conn, id, &str_list(args, "flags"), journal::now_secs())?;
    flagged(&conn, &caller, id, changed)
}

fn unmark(dir: &Path, args: &Value) -> Result<Value> {
    let caller = resolve(args, true)?;
    let id = id_arg(args)?;
    let mut conn = open(dir, &caller)?.expect("a session journal is always created");
    let changed = journal::unmark(&mut conn, id, &str_list(args, "flags"))?;
    flagged(&conn, &caller, id, changed)
}

fn close(dir: &Path, args: &Value) -> Result<Value> {
    let caller = resolve(args, true)?;
    let id = id_arg(args)?;
    let mut conn = open(dir, &caller)?.expect("a session journal is always created");
    let changed = journal::close(
        &mut conn,
        id,
        &str_list(args, "flags"),
        caller.nick.as_deref(),
        str_arg(args, "resolution").as_deref(),
        journal::now_secs(),
    )?;
    flagged(&conn, &caller, id, changed)
}

fn reopen(dir: &Path, args: &Value) -> Result<Value> {
    let caller = resolve(args, true)?;
    let id = id_arg(args)?;
    let mut conn = open(dir, &caller)?.expect("a session journal is always created");
    let changed = journal::reopen(&mut conn, id, &str_list(args, "flags"))?;
    flagged(&conn, &caller, id, changed)
}

fn agenda(dir: &Path, args: &Value) -> Result<Value> {
    let caller = resolve(args, false)?;
    let limit = args.get("limit").and_then(Value::as_u64).unwrap_or(20).clamp(1, journal::MAX_PAGE_SIZE as u64) as usize;
    let groups = match open(dir, &caller)? {
        Some(conn) => journal::agenda(&conn, limit)?,
        None => Vec::new(),
    };
    let groups: Vec<Value> = groups
        .into_iter()
        .map(|(flag, total, entries)| json!({ "flag": flag, "open": total, "entries": entries }))
        .collect();
    Ok(json!({ "project": project_json(&caller), "agenda": groups }))
}

fn tags(dir: &Path, args: &Value) -> Result<Value> {
    let caller = resolve(args, false)?;
    let category = str_arg(args, "category");
    let tags = match open(dir, &caller)? {
        Some(conn) => journal::tags(&conn, category.as_deref())?,
        None => Vec::new(),
    };
    Ok(json!({ "project": project_json(&caller), "tags": tags }))
}

fn categories(dir: &Path, args: &Value) -> Result<Value> {
    let caller = resolve(args, false)?;
    let categories = match open(dir, &caller)? {
        Some(conn) => journal::categories(&conn)?,
        None => Vec::new(),
    };
    Ok(json!({ "project": project_json(&caller), "categories": categories }))
}

fn gc(dir: &Path, args: &Value) -> Result<Value> {
    let caller = resolve(args, false)?;
    let dry_run = args.get("dry_run").and_then(Value::as_bool).unwrap_or(false);
    let report = match open(dir, &caller)? {
        Some(conn) => journal::gc(&conn, journal::now_secs(), dry_run)?,
        None => journal::GcReport::default(),
    };
    Ok(json!({ "project": project_json(&caller), "dry_run": dry_run, "medium": report.medium, "low": report.low }))
}

/// Embed whatever in this project's journal has no vector yet. Only when the
/// model is already on disk — the daemon never triggers its download.
fn spawn_embedding(project: PathBuf) {
    if !crate::encoder::model_cached() {
        return;
    }
    tokio::task::spawn_blocking(move || {
        if let Err(e) = embed_missing(&journal::journals_dir(), &project) {
            tracing::warn!(project = %project.display(), error = %e, "journal: embedding failed");
        }
    });
}

fn embed_missing(dir: &Path, project: &Path) -> Result<()> {
    let Some(mut conn) = journal::open(dir, project, false)? else { return Ok(()) };
    loop {
        let batch = journal::missing_vectors(&conn, 32)?;
        if batch.is_empty() {
            return Ok(());
        }
        let texts: Vec<&str> = batch.iter().map(|(_, body)| body.as_str()).collect();
        let vectors = crate::encoder::encode_passages(&texts)?;
        for ((id, _), v) in batch.iter().zip(vectors) {
            journal::store_vector(&mut conn, *id, &v)?;
        }
        if batch.len() < 32 {
            return Ok(());
        }
    }
}

/// Once a day, apply retention to every journal. Salience decides how long an
/// entry lives; somebody has to actually enforce it, and the owner of the
/// files is the natural one.
pub fn spawn_maintenance() {
    tokio::spawn(async {
        let mut tick = tokio::time::interval(std::time::Duration::from_secs(24 * 3600));
        tick.tick().await; // the first tick is immediate; give startup room
        tokio::time::sleep(std::time::Duration::from_secs(300)).await;
        loop {
            let swept = tokio::task::spawn_blocking(|| -> Result<(i64, i64)> {
                let dir = journal::journals_dir();
                let mut total = (0, 0);
                for info in journal::list_journals(&dir)? {
                    if let Some(conn) = journal::open(&dir, Path::new(&info.project), false)? {
                        let r = journal::gc(&conn, journal::now_secs(), false)?;
                        total = (total.0 + r.medium, total.1 + r.low);
                    }
                }
                Ok(total)
            })
            .await;
            match swept {
                Ok(Ok((m, l))) if m + l > 0 => tracing::info!(medium = m, low = l, "journal gc"),
                Ok(Ok(_)) => {}
                Ok(Err(e)) => tracing::warn!(error = %e, "journal gc failed"),
                Err(e) => tracing::warn!(error = %e, "journal gc task panicked"),
            }
            tick.tick().await;
        }
    });
}

fn str_arg(args: &Value, key: &str) -> Option<String> {
    args.get(key).and_then(Value::as_str).map(str::trim).filter(|s| !s.is_empty()).map(String::from)
}

fn str_list(args: &Value, key: &str) -> Vec<String> {
    args.get(key)
        .and_then(Value::as_array)
        .map(|a| a.iter().filter_map(Value::as_str).map(String::from).collect())
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_directory_read_finds_the_enclosing_project() {
        let dir = tempfile::tempdir().unwrap();
        journal::open(dir.path(), Path::new("/w/proj"), true).unwrap();
        assert_eq!(nearest_journal(dir.path(), Path::new("/w/proj/src/deep")), PathBuf::from("/w/proj"));
        assert_eq!(nearest_journal(dir.path(), Path::new("/w/other")), PathBuf::from("/w/other"));
        // A sibling whose name merely starts the same is a different project.
        assert_eq!(nearest_journal(dir.path(), Path::new("/w/proj2")), PathBuf::from("/w/proj2"));
    }

    #[test]
    fn a_lock_is_the_daemons_error_a_bad_call_is_the_callers() {
        let busy = rusqlite::Error::SqliteFailure(rusqlite::ffi::Error::new(rusqlite::ffi::SQLITE_BUSY), None);
        assert_eq!(error_code(&anyhow::Error::new(busy).context("journal add")), -32603);
        assert_eq!(error_code(&anyhow!("at least one --tag is required")), -32602);
    }

    #[test]
    fn writing_without_a_session_is_refused() {
        let err = resolve(&json!({ "cwd": "/w/proj" }), true).err().unwrap().to_string();
        assert!(err.contains("claudebase run"), "{err}");
        let err = resolve(&json!({ "cwd": "relative" }), false).err().unwrap().to_string();
        assert!(err.contains("absolute"), "{err}");
    }
}
