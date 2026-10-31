//! `claudebase mindmap …` — a thin client of the daemon's `journal_mindmap_*`
//! tools (`daemon::mindmap_tools`), identified the way `claudebase journal` is.

use anyhow::{Context, Result};
use serde_json::{json, Value};

use crate::cli::MindmapSubcommand as S;
use crate::daemon::client::DaemonClient;
use crate::journal_cli::with_caller;

pub async fn run(sub: S) -> Result<String> {
    // `svg -o` writes here, on the caller's machine, whatever daemon drew it.
    let output = match &sub {
        S::Svg { output, .. } => output.clone(),
        _ => None,
    };
    let (tool, args, json_out, brief) = request(sub);
    let mut client = DaemonClient::connect().await?;
    let payload = client.call_tool(tool, with_caller(args)).await?;
    if let Some(path) = output {
        let svg = payload["svg"].as_str().context("the daemon returned no SVG")?;
        std::fs::write(&path, svg).with_context(|| format!("write {}", path.display()))?;
        return Ok(format!("mind map #{} drawn to {}", payload["map"], path.display()));
    }
    if json_out {
        return Ok(serde_json::to_string_pretty(&payload)?);
    }
    Ok(render(tool, &payload, brief))
}

fn request(sub: S) -> (&'static str, Value, bool, bool) {
    match sub {
        S::New { title, layout, direction, shared, json } => (
            "journal_mindmap_new",
            json!({ "title": title, "layout": layout, "direction": direction, "shared": shared }),
            json,
            false,
        ),
        S::Share { map } => ("journal_mindmap_share", json!({ "map": map, "shared": true }), false, false),
        S::Unshare { map } => ("journal_mindmap_share", json!({ "map": map, "shared": false }), false, false),
        S::List { closed, brief, json } => ("journal_mindmap_list", json!({ "closed": closed }), json, brief),
        S::Show { map, json } => ("journal_mindmap_show", json!({ "map": map }), json, false),
        S::Add { map, text, parent, after, label, kind, refs, json } => (
            "journal_mindmap_add",
            json!({ "map": map, "text": text, "parent": parent, "after": after, "label": label, "kind": kind, "refs": refs }),
            json,
            false,
        ),
        S::Edit { map, node, text, kind } => {
            ("journal_mindmap_edit", json!({ "map": map, "node": node, "text": text, "kind": kind }), false, false)
        }
        S::Mv { map, node, parent } => ("journal_mindmap_move", json!({ "map": map, "node": node, "parent": parent }), false, false),
        S::Done { map, node } => ("journal_mindmap_state", json!({ "map": map, "node": node, "state": "done" }), false, false),
        S::Drop { map, node } => ("journal_mindmap_state", json!({ "map": map, "node": node, "state": "dropped" }), false, false),
        S::Undo { map, node } => ("journal_mindmap_state", json!({ "map": map, "node": node, "state": "open" }), false, false),
        S::Rm { map, node } => ("journal_mindmap_remove", json!({ "map": map, "node": node }), false, false),
        S::Link { map, from, to, label, refs } => (
            "journal_mindmap_link",
            json!({ "map": map, "from": from, "to": to, "label": label, "refs": refs }),
            false,
            false,
        ),
        S::Unlink { map, link } => ("journal_mindmap_unlink", json!({ "map": map, "link": link }), false, false),
        S::Ref { map, node, link, entries, remove } => (
            "journal_mindmap_ref",
            json!({ "map": map, "node": node, "link": link, "entries": entries, "remove": remove }),
            false,
            false,
        ),
        S::Chain { map, name, entries, remove } => (
            "journal_mindmap_chain",
            json!({ "map": map, "name": name, "entries": entries, "remove": remove }),
            false,
            false,
        ),
        S::Close { map } => ("journal_mindmap_close", json!({ "map": map, "open": false }), false, false),
        S::Reopen { map } => ("journal_mindmap_close", json!({ "map": map, "open": true }), false, false),
        S::Delete { map } => ("journal_mindmap_delete", json!({ "map": map }), false, false),
        S::Svg { map, theme, .. } => ("journal_mindmap_svg", json!({ "map": map, "theme": theme }), false, false),
        S::Send { map, theme, caption, thread } => (
            "journal_mindmap_send",
            json!({ "map": map, "theme": theme, "caption": caption, "thread": thread }),
            false,
            false,
        ),
        S::Url { map } => ("journal_mindmap_url", json!({ "map": map }), false, false),
    }
}

fn ago(secs: i64) -> String {
    let d = (crate::journal::now_secs() - secs).max(0);
    match d {
        0..=59 => "just now".into(),
        60..=3599 => format!("{} min ago", d / 60),
        3600..=86_399 => format!("{} h ago", d / 3600),
        _ => format!("{} d ago", d / 86_400),
    }
}

fn render(tool: &str, p: &Value, brief: bool) -> String {
    let map = &p["map"];
    match tool {
        "journal_mindmap_new" if p["layout"].as_str() == Some("flowchart") => format!(
            "flowchart #{map} \"{}\"{} — first symbol: claudebase mindmap add {map} \"<text>\" --kind start; \
             then --after n<id> [--label yes] for each next one",
            p["title"].as_str().unwrap_or_default(),
            if p["shared"].as_bool() == Some(true) { ", shared" } else { "" },
        ),
        "journal_mindmap_new" => format!(
            "mind map #{map} \"{}\" (root n{}){} — add nodes: claudebase mindmap add {map} \"<text>\" --kind fact",
            p["title"].as_str().unwrap_or_default(),
            p["root"],
            if p["shared"].as_bool() == Some(true) { ", shared" } else { "" },
        ),
        "journal_mindmap_share" if p["shared"].as_bool() == Some(true) => {
            format!("mind map #{map} is shared: every session of the project may read and change it")
        }
        "journal_mindmap_share" => format!("mind map #{map} is no longer shared"),
        "journal_mindmap_list" => {
            let maps = p["maps"].as_array().cloned().unwrap_or_default();
            if maps.is_empty() {
                return if brief { String::new() } else { "no mind maps".into() };
            }
            maps.iter()
                .map(|m| {
                    let owner = m["owner_nick"].as_str().unwrap_or_default();
                    let flow = m["layout"].as_str() == Some("flowchart");
                    let line = format!(
                        "#{} {}\"{}\" — {} nodes, {} open, updated {}",
                        m["id"],
                        if flow { "flowchart " } else { "" },
                        m["title"].as_str().unwrap_or_default(),
                        m["nodes"].as_i64().unwrap_or(1).saturating_sub(1),
                        m["open_nodes"],
                        ago(m["updated_at"].as_i64().unwrap_or(0))
                    );
                    let shared = m["shared"].as_bool() == Some(true);
                    let mine = m["mine"].as_bool() == Some(true);
                    if brief {
                        // The session-start hook splits on this prefix: another
                        // session's shared map goes in its own list.
                        match (mine, shared) {
                            (false, true) => format!("shared: {line}, owner {owner}"),
                            (true, true) => format!("{line} (shared)"),
                            _ => line,
                        }
                    } else {
                        let state = m["state"].as_str().unwrap_or_default();
                        format!("{line} · {state}{} · owner {owner}", if shared { " · shared" } else { "" })
                    }
                })
                .collect::<Vec<_>>()
                .join("\n")
        }
        "journal_mindmap_show" => {
            let mut out = p["outline"].as_str().unwrap_or_default().trim_end().to_string();
            if p["read_only"].as_bool().unwrap_or(false) {
                out.push_str("\n(read-only: this map belongs to another session)");
            }
            out
        }
        "journal_mindmap_add" => format!("added n{} to mind map #{map}", p["node"]),
        "journal_mindmap_link" => format!("linked (l{}) in mind map #{map}", p["link"]),
        "journal_mindmap_remove" => format!("removed n{} ({} node(s)) from mind map #{map}", p["node"], p["removed"]),
        "journal_mindmap_state" => format!("n{} is {}", p["node"], p["state"].as_str().unwrap_or_default()),
        "journal_mindmap_ref" => format!("{} reference(s) changed in mind map #{map}", p["changed"]),
        "journal_mindmap_chain" if p["removed"].as_bool() == Some(true) => {
            format!("chain `{}` removed from mind map #{map}", p["chain"].as_str().unwrap_or_default())
        }
        "journal_mindmap_chain" => format!("chain `{}` added to mind map #{map}", p["chain"].as_str().unwrap_or_default()),
        "journal_mindmap_close" => format!("mind map #{map} is {}", p["state"].as_str().unwrap_or_default()),
        "journal_mindmap_delete" => format!("mind map #{map} deleted"),
        "journal_mindmap_svg" => p["svg"].as_str().unwrap_or_default().to_string(),
        "journal_mindmap_send" => format!(
            "mind map #{map} sent to {}{} as {} (message {})",
            p["thread"].as_str().unwrap_or_default(),
            p["topic"].as_i64().map(|t| format!(" topic {t}")).unwrap_or_default(),
            if p["as_document"].as_bool() == Some(true) { "a file" } else { "an image" },
            p["message_id"]
        ),
        "journal_mindmap_url" => p["url"].as_str().unwrap_or_default().to_string(),
        _ => format!("mind map #{map}: done"),
    }
}
