//! `claudebase journal …` — a thin client: every subcommand is one call to the
//! daemon's `journal_*` tools (`daemon::journal_tools`), which own the files.
//!
//! The command sends what identifies the caller and nothing that picks a file:
//! the session pair `claudebase run` exported (`CLAUDEBASE_AGENT_ID` /
//! `CLAUDEBASE_SESSION_TOKEN`) when there is one, and the working directory for
//! reads from a plain terminal. The daemon turns that into a project.

use anyhow::{bail, Result};
use serde_json::{json, Value};

use crate::agent_cli::{AGENT_ID_ENV, SESSION_TOKEN_ENV};
use crate::chat_transfer::SESSION_ID_ENV;
use crate::cli::{JournalFilterArgs, JournalSubcommand};
use crate::daemon::client::DaemonClient;

pub async fn run(sub: JournalSubcommand) -> Result<String> {
    if let JournalSubcommand::Import(a) = sub {
        let entries = crate::chat_transfer::read_journal(&a.archive)?;
        let cwd = std::env::current_dir()?;
        // Without the conversation this has no id to map, but the archive
        // still says where the project was.
        let source = crate::chat_transfer::archive_source_project(&a.archive)?;
        let moved = Moved { session: None, source_project: source.as_deref() };
        let r = import_entries(&cwd, entries, moved).await?;
        return Ok(if a.json { serde_json::to_string_pretty(&r.to_json())? } else { r.describe() });
    }
    let (tool, args, json_out) = request(sub)?;
    let mut client = DaemonClient::connect().await?;
    let payload = client.call_tool(tool, with_caller(args)).await?;
    if json_out {
        return Ok(serde_json::to_string_pretty(&payload)?);
    }
    Ok(render(tool, &payload))
}

/// The tool to call, its arguments, and whether to print JSON.
fn request(sub: JournalSubcommand) -> Result<(&'static str, Value, bool)> {
    Ok(match sub {
        JournalSubcommand::Add(a) => {
            let body = read_body(a.body.as_deref())?;
            let args = json!({
                "body": body,
                "category": a.category,
                "tags": a.tags,
                "salience": a.salience.as_str(),
                "agent": a.agent,
                "feature": a.feature,
                "source_artifact": a.source_artifact,
                // Claude Code exports the conversation id into every tool
                // shell; it links the entry back to the transcript.
                "session_id": std::env::var(SESSION_ID_ENV).ok().filter(|s| !s.is_empty()),
            });
            ("journal_add", args, a.json)
        }
        JournalSubcommand::Search(a) => {
            let mut args = filter_json(&a.filter)?;
            args["query"] = json!(a.query);
            args["top_k"] = json!(a.top_k);
            args["mode"] = json!(format!("{:?}", a.mode).to_lowercase());
            ("journal_search", args, a.json)
        }
        JournalSubcommand::List(a) => {
            let size = a.page_size.clamp(1, crate::journal::MAX_PAGE_SIZE);
            let mut args = filter_json(&a.filter)?;
            args["offset"] = json!(a.page * size);
            args["limit"] = json!(size);
            ("journal_list", args, a.json)
        }
        JournalSubcommand::Get(a) => ("journal_get", json!({ "id": a.id }), a.json),
        JournalSubcommand::Delete(a) => ("journal_delete", json!({ "id": a.id }), a.json),
        JournalSubcommand::Tags(a) => ("journal_tags", json!({ "category": a.category }), a.json),
        JournalSubcommand::Categories(a) => ("journal_categories", json!({}), a.json),
        JournalSubcommand::Gc(a) => ("journal_gc", json!({ "dry_run": a.dry_run }), a.json),
        JournalSubcommand::Projects(a) => ("journal_projects", json!({}), a.json),
        JournalSubcommand::Import(_) => unreachable!("handled in run"),
    })
}

/// Every entry of the journal that `project` reads from, oldest first — what
/// `chat dump` packs. `None` when the project has no journal. Asked as a
/// directory read, not as the session: the conversation being dumped may not
/// be this session's.
pub async fn export_entries(project: &std::path::Path) -> Result<Option<Vec<Value>>> {
    let mut client = DaemonClient::connect().await?;
    let size = crate::journal::MAX_PAGE_SIZE;
    let mut out = Vec::new();
    for page in 0.. {
        let p = client
            .call_tool(
                "journal_list",
                json!({
                    "cwd": project.to_string_lossy(),
                    "project_id": crate::project_id::resolve_project_id(project),
                    "offset": page * size,
                    "limit": size,
                }),
            )
            .await?;
        if p.get("project").is_none() || p["project"].is_null() {
            return Ok(None);
        }
        let entries = p["entries"].as_array().cloned().unwrap_or_default();
        let total = p["total"].as_u64().unwrap_or(0) as usize;
        let last = entries.len() < size;
        out.extend(entries);
        if last || out.len() >= total {
            break;
        }
    }
    if out.is_empty() {
        return Ok(None);
    }
    // `list` is newest first; replaying oldest first keeps the ids in order.
    out.reverse();
    Ok(Some(out))
}

/// `path` re-rooted from `from` to `to` when it is `from` or under it — on a
/// path boundary, so `/w/proj2` is not under `/w/proj`.
fn move_path(path: &str, from: &str, to: &str) -> Option<String> {
    let from = from.trim_end_matches(['/', '\\']);
    if from.is_empty() || from == to {
        return None;
    }
    let rest = path.strip_prefix(from)?;
    if !(rest.is_empty() || rest.starts_with('/') || rest.starts_with('\\') || rest.starts_with('#')) {
        return None;
    }
    Some(format!("{to}{rest}"))
}

/// What [`import_entries`] did.
#[derive(Debug, Default)]
pub struct ImportReport {
    pub project: String,
    pub written: u64,
    pub merged: u64,
    pub refused: Vec<Value>,
}

impl ImportReport {
    pub fn to_json(&self) -> Value {
        json!({ "project": self.project, "written": self.written, "merged": self.merged, "refused": self.refused })
    }

    pub fn describe(&self) -> String {
        let mut s = format!(
            "journal: {} entries written, {} already there ({})",
            self.written, self.merged, self.project
        );
        if !self.refused.is_empty() {
            s.push_str(&format!("\n{} refused, e.g. {}", self.refused.len(), self.refused[0]["error"]));
        }
        s
    }
}

/// How the entries' references change on the way to another machine.
#[derive(Debug, Default, Clone, Copy)]
pub struct Moved<'a> {
    /// The source conversation id and the loaded one (`chat load --new-id`).
    pub session: Option<(&'a str, &'a str)>,
    /// The project directory on the source machine, replaced by `project`.
    /// The two rarely match: another user, another drive, another name.
    pub source_project: Option<&'a str>,
}

/// Write `entries` into the journal of directory `project`, in batches the
/// daemon accepts. The journal goes to `project` whatever the source directory
/// was called: the journal is keyed by where the conversation now lives.
/// Structured references move with it — the conversation id, and a
/// `source_artifact` path under the source project — while the entry text is
/// history and stays as written, as in the transcript.
pub async fn import_entries(
    project: &std::path::Path,
    mut entries: Vec<Value>,
    moved: Moved<'_>,
) -> Result<ImportReport> {
    let target = project.to_string_lossy().into_owned();
    for e in &mut entries {
        if let Some((from, to)) = moved.session {
            if e.get("session_id").and_then(Value::as_str) == Some(from) {
                e["session_id"] = json!(to);
            }
        }
        if let (Some(src), Some(a)) = (moved.source_project, e.get("source_artifact").and_then(Value::as_str)) {
            if let Some(moved_path) = move_path(a, src, &target) {
                e["source_artifact"] = json!(moved_path);
            }
        }
    }
    let project_id = crate::project_id::resolve_project_id(project);
    let mut client = DaemonClient::connect().await?;
    let mut report = ImportReport { project: project.to_string_lossy().into_owned(), ..Default::default() };
    for (n, batch) in entries.chunks(crate::daemon::journal_tools::MAX_IMPORT_BATCH).enumerate() {
        let r = client
            .call_tool(
                "journal_import",
                json!({ "cwd": project.to_string_lossy(), "project_id": project_id, "entries": batch }),
            )
            .await?;
        report.written += r["written"].as_u64().unwrap_or(0);
        report.merged += r["merged"].as_u64().unwrap_or(0);
        if let Some(p) = r["project"].as_str() {
            report.project = p.to_owned();
        }
        for mut f in r["refused"].as_array().cloned().unwrap_or_default() {
            if let Some(i) = f["index"].as_u64() {
                f["index"] = json!(n * crate::daemon::journal_tools::MAX_IMPORT_BATCH + i as usize);
            }
            report.refused.push(f);
        }
    }
    Ok(report)
}

fn with_caller(mut args: Value) -> Value {
    let env = |k| std::env::var(k).ok().filter(|s: &String| !s.is_empty());
    if let (Some(id), Some(token)) = (env(AGENT_ID_ENV), env(SESSION_TOKEN_ENV)) {
        args["agent_id"] = json!(id);
        args["session_token"] = json!(token);
    }
    if let Ok(cwd) = std::env::current_dir() {
        args["cwd"] = json!(cwd.to_string_lossy());
        // Which repository this is, resolved here where the checkout is: the
        // daemon may be on another machine (docs/design/remote-daemon.md).
        args["project_id"] = json!(crate::project_id::resolve_project_id(&cwd));
    }
    args
}

fn filter_json(f: &JournalFilterArgs) -> Result<Value> {
    let since = match f.since.as_deref() {
        Some(s) => Some(crate::cli::parse_since(s).map_err(anyhow::Error::msg)?),
        None => None,
    };
    Ok(json!({
        "tags": f.tags,
        "category": f.category,
        "agent": f.agent,
        "salience": f.salience.map(|s| s.as_str()),
        "feature": f.feature,
        "since_secs": since,
    }))
}

fn read_body(arg: Option<&str>) -> Result<String> {
    match arg {
        Some("-") | None => {
            if std::io::IsTerminal::is_terminal(&std::io::stdin()) {
                bail!("the entry text is missing: pass it as an argument, or pipe it on stdin");
            }
            crate::input::read_stdin()
        }
        Some(text) => Ok(text.to_string()),
    }
}

fn render(tool: &str, p: &Value) -> String {
    let project = p["project"].as_str().unwrap_or("?");
    match tool {
        "journal_add" => {
            let verb = if p["status"] == "deduped" { "already in the journal as" } else { "journaled" };
            format!("{verb} #{} ({project})", p["id"])
        }
        "journal_search" => {
            let hits = p["hits"].as_array().cloned().unwrap_or_default();
            if hits.is_empty() {
                return format!("no matching entries in the journal of {project}");
            }
            let mut out = format!("journal of {project} — {} hit(s), {}\n", hits.len(), p["mode"].as_str().unwrap_or(""));
            for h in &hits {
                out.push_str(&format!("\n{}\n  {}\n", headline(h), h["snippet"].as_str().unwrap_or("")));
            }
            out
        }
        "journal_list" => {
            let entries = p["entries"].as_array().cloned().unwrap_or_default();
            let total = p["total"].as_i64().unwrap_or(0);
            if entries.is_empty() {
                return format!("no entries in the journal of {project} (total matching: {total})");
            }
            let first = p["offset"].as_i64().unwrap_or(0) + 1;
            let mut out = format!(
                "journal of {project} — {first}-{} of {total}\n",
                first + entries.len() as i64 - 1
            );
            for e in &entries {
                out.push_str(&format!("\n{}\n  {}\n", headline(e), one_line(e["body"].as_str().unwrap_or(""), 200)));
            }
            out
        }
        "journal_get" => {
            let e = &p["entry"];
            let mut out = format!("{}\n", headline(e));
            for (label, key) in [("agent", "agent"), ("session", "session_id"), ("feature", "feature"), ("source", "source_artifact")] {
                if let Some(v) = e[key].as_str() {
                    out.push_str(&format!("{label}: {v}\n"));
                }
            }
            out.push('\n');
            out.push_str(e["body"].as_str().unwrap_or(""));
            out
        }
        "journal_delete" => format!("deleted #{} from the journal of {project}", p["deleted"]),
        "journal_tags" | "journal_categories" => {
            let key = if tool == "journal_tags" { "tags" } else { "categories" };
            let items = p[key].as_array().cloned().unwrap_or_default();
            if items.is_empty() {
                return format!("no {key} yet in the journal of {project}");
            }
            let mut out = format!("{key} in the journal of {project}:\n");
            for i in &items {
                out.push_str(&format!("  {:<32} {}\n", i["name"].as_str().unwrap_or(""), i["count"]));
            }
            out
        }
        "journal_gc" => {
            let verb = if p["dry_run"] == true { "would delete" } else { "deleted" };
            format!("{verb} {} medium + {} low entries past retention ({project})", p["medium"], p["low"])
        }
        "journal_projects" => {
            let items = p["journals"].as_array().cloned().unwrap_or_default();
            if items.is_empty() {
                return "no journals yet".into();
            }
            let mut out = String::from("journals held by the daemon:\n");
            for j in &items {
                let last = j["last_entry_at"].as_i64().map(date).unwrap_or_else(|| "-".into());
                out.push_str(&format!("  {:>5}  {:<10}  {}\n", j["entries"], last, j["project"].as_str().unwrap_or("")));
            }
            out
        }
        _ => p.to_string(),
    }
}

/// `#12 2026-10-24 14:05Z decision [sqlite, locking] high`
fn headline(e: &Value) -> String {
    let tags: Vec<&str> = e["tags"].as_array().map(|t| t.iter().filter_map(Value::as_str).collect()).unwrap_or_default();
    format!(
        "#{} {} {} [{}] {}",
        e["id"],
        e["created_at"].as_i64().map(date).unwrap_or_default(),
        e["category"].as_str().unwrap_or(""),
        tags.join(", "),
        e["salience"].as_str().unwrap_or("")
    )
}

/// Minutes matter: several updates land on the same day, and the journal is
/// read as a changelog.
fn date(secs: i64) -> String {
    chrono::DateTime::<chrono::Utc>::from_timestamp(secs, 0).map(|d| d.format("%Y-%m-%d %H:%MZ").to_string()).unwrap_or_default()
}

fn one_line(text: &str, max: usize) -> String {
    let flat = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if flat.chars().count() > max {
        format!("{}…", flat.chars().take(max).collect::<String>())
    } else {
        flat
    }
}

#[cfg(test)]
mod tests {
    use super::move_path;

    #[test]
    fn artifact_paths_move_with_the_project_whatever_it_is_called() {
        let (from, to) = ("/home/a/tacticstrade.ai", r"C:\Users\b\tt");
        assert_eq!(move_path("/home/a/tacticstrade.ai/src/x.rs#L3", from, to).as_deref(), Some(r"C:\Users\b\tt/src/x.rs#L3"));
        assert_eq!(move_path("/home/a/tacticstrade.ai", from, to).as_deref(), Some(r"C:\Users\b\tt"));
        assert_eq!(move_path("/home/a/tacticstrade.ai2/x", from, to), None);
        assert_eq!(move_path("docs/PRD.md#s16", from, to), None);
        assert_eq!(move_path("cd3f1a1", from, to), None);
    }
}
