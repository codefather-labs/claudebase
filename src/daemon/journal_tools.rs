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
    project: PathBuf,
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
    if matches!(tool.as_str(), "journal_add" | "journal_search") {
        if let Some(project) = value.get("project").and_then(Value::as_str) {
            spawn_embedding(PathBuf::from(project));
        }
    }
    Ok(value)
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
        let (cwd, nick): (Option<String>, Option<String>) = conn.query_row(
            "SELECT cwd, agent_name FROM agent_registry WHERE agent_id = ?1",
            rusqlite::params![agent_id],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )?;
        let cwd = cwd.filter(|c| !c.is_empty()).ok_or_else(|| {
            anyhow!("the daemon does not know which directory this session was started in; restart it with `claudebase run`")
        })?;
        return Ok(Caller { project: PathBuf::from(cwd), agent_id: Some(agent_id.into()), nick });
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
    Ok(Caller { project: nearest_journal(&journal::journals_dir(), &cwd), agent_id: None, nick: None })
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
    let Some(mut conn) = journal::open(dir, &caller.project, create)? else { return Ok(None) };
    if create {
        match journal::import_legacy_insights(&mut conn, &caller.project) {
            Ok(n) if n > 0 => tracing::info!(project = %caller.project.display(), imported = n, "journal: imported insights.db"),
            Ok(_) => {}
            Err(e) => tracing::warn!(project = %caller.project.display(), error = %e, "journal: insights.db import failed"),
        }
    }
    Ok(Some(conn))
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
    };
    let (status, id) = journal::add(&mut conn, &entry, journal::now_secs())?;
    Ok(json!({ "status": status, "id": id, "project": project_json(&caller) }))
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
    Ok(Filter {
        category: str_arg(args, "category"),
        tags: str_list(args, "tags"),
        agent: str_arg(args, "agent"),
        salience,
        feature: str_arg(args, "feature"),
        since,
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
    let entry = match open(dir, &caller)? {
        Some(conn) => journal::get(&conn, id)?,
        None => None,
    };
    let entry = entry.ok_or_else(|| anyhow!("no entry {id} in the journal of {}", caller.project.display()))?;
    Ok(json!({ "project": project_json(&caller), "entry": entry }))
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
    let Some(conn) = journal::open(dir, project, false)? else { return Ok(()) };
    loop {
        let batch = journal::missing_vectors(&conn, 32)?;
        if batch.is_empty() {
            return Ok(());
        }
        let texts: Vec<&str> = batch.iter().map(|(_, body)| body.as_str()).collect();
        let vectors = crate::encoder::encode_passages(&texts)?;
        for ((id, _), v) in batch.iter().zip(vectors) {
            journal::store_vector(&conn, *id, &v)?;
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
    fn writing_without_a_session_is_refused() {
        let err = resolve(&json!({ "cwd": "/w/proj" }), true).err().unwrap().to_string();
        assert!(err.contains("claudebase run"), "{err}");
        let err = resolve(&json!({ "cwd": "relative" }), false).err().unwrap().to_string();
        assert!(err.contains("absolute"), "{err}");
    }
}
