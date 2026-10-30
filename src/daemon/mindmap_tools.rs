//! `journal_mindmap_*` tools: the daemon side of `claudebase mindmap`.
//!
//! A map is filed in its project's journal and resolved the way every journal
//! call is (`journal_tools::resolve`). On top of that, journal#93/#94:
//!
//! - writing needs a session, and that session must be the map's owner
//!   (nick + host); anyone else is refused, whatever the project;
//! - `list` shows a session its own maps only — the reminder hook reads it,
//!   so no session is ever told about another's map;
//! - any session of the project, or the operator's terminal, reads a map by
//!   its id;
//! - the operator's terminal (no session) lists every map and may delete one.

use std::path::Path;

use anyhow::{anyhow, bail, Context, Result};
use serde_json::{json, Value};

use super::journal_tools::{open, project_json, resolve, str_arg, Caller};
use crate::journal;
use crate::mindmap::{self, Owner, Target};
use crate::mindmap_svg::{self, Theme};

pub(super) fn dispatch(tool: &str, dir: &Path, args: &Value) -> Result<Value> {
    match tool {
        "journal_mindmap_new" => new(dir, args),
        "journal_mindmap_list" => list(dir, args),
        "journal_mindmap_show" => show(dir, args),
        "journal_mindmap_svg" => svg(dir, args),
        "journal_mindmap_send" => send(dir, args),
        "journal_mindmap_url" => url(dir, args),
        "journal_mindmap_delete" => delete(dir, args),
        _ => write(tool, dir, args),
    }
}

fn owner_of(caller: &Caller) -> Result<Owner> {
    let nick = caller
        .nick
        .clone()
        .filter(|n| !n.is_empty())
        .ok_or_else(|| anyhow!("this session has no nick yet — mind maps belong to a named session"))?;
    Ok(Owner { nick, host: caller.host.clone() })
}

fn i64_arg(args: &Value, key: &str) -> Result<i64> {
    args.get(key).and_then(Value::as_i64).ok_or_else(|| anyhow!("{key} required (integer)"))
}

fn opt_i64(args: &Value, key: &str) -> Option<i64> {
    args.get(key).and_then(Value::as_i64)
}

fn ids(args: &Value, key: &str) -> Vec<i64> {
    args.get(key).and_then(Value::as_array).map(|a| a.iter().filter_map(Value::as_i64).collect()).unwrap_or_default()
}

fn theme(args: &Value) -> Result<Theme> {
    Theme::parse(str_arg(args, "theme").as_deref().unwrap_or("auto"))
}

fn new(dir: &Path, args: &Value) -> Result<Value> {
    let caller = resolve(args, true)?;
    let owner = owner_of(&caller)?;
    let mut conn = open(dir, &caller)?.expect("a session journal is always created");
    let title = str_arg(args, "title").ok_or_else(|| anyhow!("title required"))?;
    let (map, root) = mindmap::create(&mut conn, &title, &owner, journal::now_secs())?;
    Ok(json!({ "project": project_json(&caller), "map": map, "root": root, "title": title }))
}

fn list(dir: &Path, args: &Value) -> Result<Value> {
    let caller = resolve(args, false)?;
    let closed_too = args.get("closed").and_then(Value::as_bool).unwrap_or(false);
    let Some(conn) = open(dir, &caller)? else {
        return Ok(json!({ "project": project_json(&caller), "maps": [], "scope": "none" }));
    };
    let (maps, scope) = if caller.agent_id.is_some() {
        (mindmap::list(&conn, Some(&owner_of(&caller)?), closed_too)?, "session")
    } else {
        (mindmap::list(&conn, None, closed_too)?, "project")
    };
    Ok(json!({ "project": project_json(&caller), "maps": maps, "scope": scope }))
}

fn load(dir: &Path, args: &Value) -> Result<(Caller, mindmap::Map)> {
    let caller = resolve(args, false)?;
    let id = i64_arg(args, "map")?;
    let conn = open(dir, &caller)?.ok_or_else(|| anyhow!("this project has no journal yet, so no mind map #{id}"))?;
    let map = mindmap::load(&conn, id)?.ok_or_else(|| anyhow!("no mind map #{id} in this project"))?;
    Ok((caller, map))
}

fn show(dir: &Path, args: &Value) -> Result<Value> {
    let (caller, map) = load(dir, args)?;
    let mine = caller.agent_id.is_none() || owner_of(&caller).map(|o| o.nick == map.summary.owner_nick).unwrap_or(false);
    Ok(json!({
        "project": project_json(&caller),
        "outline": mindmap::outline(&map),
        "read_only": !mine,
        "map": map,
    }))
}

fn svg(dir: &Path, args: &Value) -> Result<Value> {
    let (caller, map) = load(dir, args)?;
    Ok(json!({ "project": project_json(&caller), "map": map.summary.id, "svg": mindmap_svg::render_svg(&map, theme(args)?) }))
}

/// Draw the map and send it to the operator in Telegram — to the chat this
/// session answers in, by the same rules as `claudebase telegram send`.
fn send(dir: &Path, args: &Value) -> Result<Value> {
    let (caller, map) = load(dir, args)?;
    let theme = match theme(args)? {
        Theme::Auto => Theme::Light,
        t => t,
    };
    let chat = crate::daemon::chat::open_chat_db().context("open chat.db")?;
    let (thread, topic, _) = crate::telegram_cli::resolve_thread(&chat, str_arg(args, "thread").as_deref(), caller.agent_id.as_deref())?;
    let chat_id: i64 = thread
        .strip_prefix("telegram:")
        .and_then(|c| c.parse().ok())
        .ok_or_else(|| anyhow!("`{thread}` is not a Telegram chat"))?;
    let png = mindmap_svg::render_png(&map, theme)?;
    let as_document = !mindmap_svg::fits_telegram_photo(&png);
    let caption = str_arg(args, "caption").unwrap_or_else(|| {
        format!("Mind map #{}: {} — {} nodes, {} open", map.summary.id, map.summary.title, map.summary.nodes, map.summary.open_nodes)
    });
    let caption: String = caption.chars().take(1000).collect();
    let file_name = format!("mindmap-{}.png", map.summary.id);
    let bytes = png.len();
    let rx = crate::daemon::telegram::enqueue_outbound_tg_file(chat_id, topic, png, file_name, caption, as_document)?;
    // A tool runs on a blocking thread; wait for the upload's answer here.
    let handle = tokio::runtime::Handle::current();
    let sent = handle
        .block_on(async { tokio::time::timeout(std::time::Duration::from_secs(60), rx).await })
        .map_err(|_| anyhow!("Telegram did not answer within 60 s; the image may still arrive"))?
        .map_err(|_| anyhow!("the Telegram task dropped the upload"))??;
    Ok(json!({
        "project": project_json(&caller),
        "map": map.summary.id,
        "thread": thread,
        "topic": topic,
        "message_id": sent,
        "as_document": as_document,
        "bytes": bytes,
    }))
}

/// The live page of a map, on the daemon's HTTP listener.
fn url(dir: &Path, args: &Value) -> Result<Value> {
    let (caller, map) = load(dir, args)?;
    let conn = open(dir, &caller)?.expect("the map was just read from it");
    let token = mindmap::view_token_of(&conn, map.summary.id)?.context("map has no view token")?;
    let chat = crate::daemon::chat::open_chat_db().context("open chat.db")?;
    let Some(bind) = crate::daemon::callback::configured_bind(&chat)? else {
        bail!(
            "the daemon's HTTP listener is off — the live page needs it: `claudebase daemon callback enable`, \
             then restart the daemon. Meanwhile: `claudebase mindmap svg {} -o map.svg`",
            map.summary.id
        );
    };
    let host = match bind.rsplit_once(':') {
        Some(("0.0.0.0", port)) | Some(("[::]", port)) => format!("127.0.0.1:{port}"),
        _ => bind.clone(),
    };
    Ok(json!({
        "project": project_json(&caller),
        "map": map.summary.id,
        "url": format!("http://{host}/mindmap/{token}"),
    }))
}

fn delete(dir: &Path, args: &Value) -> Result<Value> {
    let caller = resolve(args, false)?;
    let id = i64_arg(args, "map")?;
    let conn = open(dir, &caller)?.ok_or_else(|| anyhow!("no mind map #{id} in this project"))?;
    if caller.agent_id.is_some() {
        mindmap::check_owner(&conn, id, &owner_of(&caller)?)?;
    }
    mindmap::delete(&conn, id)?;
    Ok(json!({ "project": project_json(&caller), "map": id, "deleted": true }))
}

/// Every tool that changes a map: a session, and the map's owner.
fn write(tool: &str, dir: &Path, args: &Value) -> Result<Value> {
    let caller = resolve(args, true)?;
    let owner = owner_of(&caller)?;
    let mut conn = open(dir, &caller)?.expect("a session journal is always created");
    let map = i64_arg(args, "map")?;
    mindmap::check_owner(&conn, map, &owner)?;
    let now = journal::now_secs();
    let c = &mut conn;
    let out = match tool {
        "journal_mindmap_add" => {
            let text = str_arg(args, "text").ok_or_else(|| anyhow!("text required"))?;
            let kind = str_arg(args, "kind").unwrap_or_else(|| "note".into());
            let id = mindmap::add_node(c, map, opt_i64(args, "parent"), &kind, &text, &ids(args, "refs"), now)?;
            json!({ "node": id })
        }
        "journal_mindmap_edit" => {
            let node = i64_arg(args, "node")?;
            mindmap::edit_node(c, map, node, str_arg(args, "text").as_deref(), str_arg(args, "kind").as_deref(), now)?;
            json!({ "node": node })
        }
        "journal_mindmap_move" => {
            let node = i64_arg(args, "node")?;
            mindmap::move_node(c, map, node, i64_arg(args, "parent")?, now)?;
            json!({ "node": node })
        }
        "journal_mindmap_state" => {
            let node = i64_arg(args, "node")?;
            let state = str_arg(args, "state").ok_or_else(|| anyhow!("state required"))?;
            mindmap::set_node_state(c, map, node, &state, now)?;
            json!({ "node": node, "state": state })
        }
        "journal_mindmap_remove" => {
            let node = i64_arg(args, "node")?;
            json!({ "node": node, "removed": mindmap::remove_node(c, map, node, now)? })
        }
        "journal_mindmap_link" => {
            let label = str_arg(args, "label").unwrap_or_default();
            let id = mindmap::add_edge(c, map, i64_arg(args, "from")?, i64_arg(args, "to")?, &label, &ids(args, "refs"), now)?;
            json!({ "link": id })
        }
        "journal_mindmap_unlink" => {
            let link = i64_arg(args, "link")?;
            mindmap::remove_edge(c, map, link, now)?;
            json!({ "link": link, "removed": true })
        }
        "journal_mindmap_ref" => {
            let target = match (opt_i64(args, "node"), opt_i64(args, "link")) {
                (Some(n), None) => Target::Node(n),
                (None, Some(l)) => Target::Edge(l),
                _ => bail!("name exactly one of node or link"),
            };
            let entries = ids(args, "entries");
            let n = if args.get("remove").and_then(Value::as_bool).unwrap_or(false) {
                mindmap::remove_refs(c, map, target, &entries, now)?
            } else {
                mindmap::add_refs(c, map, target, &entries, now)?
            };
            json!({ "changed": n })
        }
        "journal_mindmap_chain" => {
            let name = str_arg(args, "name").ok_or_else(|| anyhow!("name required"))?;
            if args.get("remove").and_then(Value::as_bool).unwrap_or(false) {
                mindmap::remove_chain(c, map, &name, now)?;
                json!({ "chain": name, "removed": true })
            } else {
                json!({ "chain": name, "id": mindmap::add_chain(c, map, &name, &ids(args, "entries"), now)? })
            }
        }
        "journal_mindmap_close" => {
            let open = args.get("open").and_then(Value::as_bool).unwrap_or(false);
            mindmap::set_state(c, map, open, now)?;
            json!({ "state": if open { "open" } else { "closed" } })
        }
        other => bail!("unknown mind-map tool `{other}`"),
    };
    let mut out = out;
    out["project"] = project_json(&caller);
    out["map"] = json!(map);
    Ok(out)
}

/// The live page's data, for the HTTP listener: the map whose view token is
/// `token`, in whichever project's journal it is.
pub fn find_by_token(token: &str) -> Result<Option<mindmap::Map>> {
    if token.len() != 32 || !token.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Ok(None);
    }
    let dir = journal::journals_dir();
    for info in journal::list_journals(&dir)? {
        let Some(conn) = journal::open(&dir, Path::new(&info.project), false)? else { continue };
        let id: Option<i64> = rusqlite::OptionalExtension::optional(conn.query_row(
            "SELECT id FROM mindmaps WHERE view_token = ?1",
            rusqlite::params![token],
            |r| r.get(0),
        ))?;
        if let Some(id) = id {
            return mindmap::load(&conn, id);
        }
    }
    Ok(None)
}
