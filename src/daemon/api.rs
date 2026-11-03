//! The public API of the journal and mind maps, for programs such as an
//! orchestrator: `/v1/*` on the callback listener (docs/design/public-api.md).
//!
//! A request names its project and author, so it needs no session and survives
//! a restart of either side. The token (`claudebase daemon api issue`) reaches
//! every project on this daemon: who may see what is the client's business.

use anyhow::Result;
use serde_json::{json, Value};

use super::journal_tools;

/// Bumped only by a change an existing client would notice.
pub const API_VERSION: u32 = 1;
pub const MAX_BODY: usize = 4 * 1024 * 1024;

const JOURNAL_OPS: [&str; 15] = [
    "add", "search", "list", "get", "tags", "categories", "mark", "unmark", "close", "reopen", "agenda", "link",
    "unlink", "branch", "policy",
];
/// These reach outside the journal (Telegram, the listener's address) or move whole maps.
const MINDMAP_DENIED: [&str; 4] = ["send", "url", "export", "import"];
/// Set by a session's own daemon connection; a program has no session to send a reminder to.
const FIELDS_DENIED: [&str; 4] = ["agent_id", "session_token", "cwd", "every_secs"];

type Reply = (&'static str, Value);

fn err(status: &'static str, code: &str, message: impl Into<String>) -> Reply {
    (status, json!({ "error": code, "message": message.into() }))
}

/// What starts a journal: a first entry or a first map. Everything else on an unknown project is told so.
const CREATES: [&str; 2] = ["journal_add", "journal_mindmap_new"];

/// The refusal for a token that is missing or unknown. Answered from the head, before any body is read.
pub async fn refuse_token(token: Option<&str>) -> Option<Reply> {
    let token = token.unwrap_or("").to_owned();
    let authed = tokio::task::spawn_blocking(move || -> Result<Option<String>> {
        let conn = crate::daemon::chat::open_chat_db()?;
        crate::daemon::remote::authenticate(&conn, crate::daemon::remote::TokenKind::Api, &token)
    })
    .await;
    match authed {
        Ok(Ok(Some(_))) => None,
        Ok(Ok(None)) => Some(err("401 Unauthorized", "unauthorized", "missing or unknown X-Api-Token (claudebase daemon api issue <client>)")),
        _ => Some(err("500 Internal Server Error", "internal", "token check failed")),
    }
}

/// `/v1/*`, for a token already checked. Returns the HTTP status line and the JSON body.
pub async fn http(method: &str, target: &str, body: &[u8]) -> Reply {
    let path = target.split_once('?').map_or(target, |(p, _)| p);
    let parts: Vec<&str> = path.trim_matches('/').split('/').collect();
    match (method, parts.as_slice()) {
        ("GET", ["v1", "version"]) => {
            ("200 OK", json!({ "api": API_VERSION, "claudebase": env!("CARGO_PKG_VERSION") }))
        }
        ("GET", ["v1", "projects"]) => reply(journal_tools::handle("journal_projects", json!({})).await),
        ("POST", ["v1", "journal", op]) if JOURNAL_OPS.contains(op) => call(format!("journal_{op}"), body).await,
        ("POST", ["v1", "mindmap", op]) if !MINDMAP_DENIED.contains(op) => call(format!("journal_mindmap_{op}"), body).await,
        _ => err("404 Not Found", "not_found", format!("no route {method} {path}")),
    }
}

async fn call(tool: String, body: &[u8]) -> Reply {
    let mut args: Value = match serde_json::from_slice(body) {
        Ok(v @ Value::Object(_)) => v,
        _ => return err("400 Bad Request", "bad_request", "the body must be a JSON object"),
    };
    let text = |k: &str| args.get(k).and_then(Value::as_str).unwrap_or("").trim().to_owned();
    let (project, author) = (canonical(&text("project_id")), text("author"));
    let Some(project) = project else {
        return err("400 Bad Request", "bad_request", "project_id is required: the repository, as host/owner/repo");
    };
    if author.is_empty() {
        return err("400 Bad Request", "bad_request", "author is required");
    }
    if !CREATES.contains(&tool.as_str()) && !journal_exists(&project) {
        return err("404 Not Found", "unknown_project", format!("no journal for {project}: the first journal add or mindmap new starts one"));
    }
    if let Some(fields) = args.as_object_mut() {
        for field in FIELDS_DENIED.iter().chain(["project_id", "author"].iter()) {
            fields.remove(*field);
        }
        // On a write the entry is its author's, whatever the body says. On a read `agent` stays what it is for a
        // session, a filter the client may send: the author must not narrow what it reads to its own entries.
        if tool == "journal_add" {
            fields.insert("agent".into(), json!(author));
        }
    }
    reply(journal_tools::handle_api(&tool, &project, &author, args).await)
}

/// A repository as sessions name it (`project_id::normalize_remote_url`), so that every spelling reaches their
/// journal; anything that is not a repository, none.
fn canonical(raw: &str) -> Option<String> {
    if raw.starts_with("local:") {
        return None;
    }
    let id = crate::project_id::normalize_remote_url(raw)?;
    let parts: Vec<&str> = id.split('/').collect();
    (parts.len() >= 3 && parts.iter().all(|p| !p.is_empty() && *p != "." && *p != "..")).then_some(id)
}

fn journal_exists(project: &str) -> bool {
    let key = crate::journal::project_key(Some(project), std::path::Path::new(""));
    crate::journal::journal_path(&crate::journal::journals_dir(), &key).exists()
}

fn reply(result: Result<Value>) -> Reply {
    match result {
        Ok(v) => ("200 OK", v),
        Err(e) => {
            let message = format!("{e:#}");
            if journal_tools::error_code(&e) == -32603 {
                err("500 Internal Server Error", "storage", message)
            } else {
                err("400 Bad Request", "refused", message)
            }
        }
    }
}
