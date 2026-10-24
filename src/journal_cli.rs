//! `claudebase journal …` — a thin client: every subcommand is one call to the
//! daemon's `journal_*` tools (`daemon::journal_tools`), which own the files.
//!
//! The command sends what identifies the caller and nothing that picks a file:
//! the session pair `claudebase run` exported (`CLAUDEBASE_AGENT_ID` /
//! `CLAUDEBASE_SESSION_TOKEN`) when there is one, and the working directory for
//! reads from a plain terminal. The daemon turns that into a project.

use std::io::Read;

use anyhow::{bail, Context, Result};
use serde_json::{json, Value};

use crate::agent_cli::{AGENT_ID_ENV, SESSION_TOKEN_ENV};
use crate::chat_transfer::SESSION_ID_ENV;
use crate::cli::{JournalFilterArgs, JournalSubcommand};
use crate::daemon::client::DaemonClient;

pub async fn run(sub: JournalSubcommand) -> Result<String> {
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
    })
}

fn with_caller(mut args: Value) -> Value {
    let env = |k| std::env::var(k).ok().filter(|s: &String| !s.is_empty());
    if let (Some(id), Some(token)) = (env(AGENT_ID_ENV), env(SESSION_TOKEN_ENV)) {
        args["agent_id"] = json!(id);
        args["session_token"] = json!(token);
    }
    if let Ok(cwd) = std::env::current_dir() {
        args["cwd"] = json!(cwd.to_string_lossy());
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
            let mut buf = String::new();
            std::io::stdin().read_to_string(&mut buf).context("read stdin")?;
            Ok(buf)
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
