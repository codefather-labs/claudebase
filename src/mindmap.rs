//! Mind maps: an agent's working memory for a long task or a piece of
//! research — a tree of short nodes around one topic, cross-links between
//! branches, and the journal entries each node, link or line of reasoning
//! rests on.
//!
//! Operator decisions journal#92, journal#93, journal#94:
//!
//! - A map lives in its project's journal file, so it persists and travels
//!   with `journal share` like the entries it points at.
//! - A map belongs to the session that made it: its nick and host, which a
//!   resumed conversation keeps. Only the owner writes; another session of the
//!   project may read a map by id and nothing more, and is never shown one
//!   it did not ask for.
//! - Every node and every link can point at journal entries; a chain is a
//!   named ordered sequence of entries, unique within its map.
//!
//! This module is storage only. The daemon decides who the caller is
//! (`daemon::mindmap_tools`); `mindmap_svg` draws.

use std::collections::HashMap;

use anyhow::{bail, Context, Result};
use rusqlite::{params, Connection, OptionalExtension, TransactionBehavior};
use serde::Serialize;

/// Applied with the journal schema on every open (`journal::open_file`).
pub const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS mindmaps (
    id          INTEGER PRIMARY KEY,
    title       TEXT NOT NULL,
    owner_nick  TEXT NOT NULL,
    owner_host  TEXT,
    state       TEXT NOT NULL DEFAULT 'open',
    view_token  TEXT NOT NULL,
    created_at  INTEGER NOT NULL,
    updated_at  INTEGER NOT NULL
);
CREATE INDEX IF NOT EXISTS mindmaps_owner ON mindmaps(owner_nick, state);
CREATE TABLE IF NOT EXISTS mindmap_nodes (
    id         INTEGER PRIMARY KEY,
    map_id     INTEGER NOT NULL REFERENCES mindmaps(id) ON DELETE CASCADE,
    parent_id  INTEGER REFERENCES mindmap_nodes(id) ON DELETE CASCADE,
    kind       TEXT NOT NULL,
    text       TEXT NOT NULL,
    state      TEXT NOT NULL DEFAULT 'open',
    ord        INTEGER NOT NULL,
    created_at INTEGER NOT NULL
);
CREATE INDEX IF NOT EXISTS mindmap_nodes_map ON mindmap_nodes(map_id, parent_id);
CREATE TABLE IF NOT EXISTS mindmap_edges (
    id        INTEGER PRIMARY KEY,
    map_id    INTEGER NOT NULL REFERENCES mindmaps(id) ON DELETE CASCADE,
    from_node INTEGER NOT NULL REFERENCES mindmap_nodes(id) ON DELETE CASCADE,
    to_node   INTEGER NOT NULL REFERENCES mindmap_nodes(id) ON DELETE CASCADE,
    label     TEXT NOT NULL DEFAULT '',
    UNIQUE(from_node, to_node, label)
);
CREATE TABLE IF NOT EXISTS mindmap_node_refs (
    node_id  INTEGER NOT NULL REFERENCES mindmap_nodes(id) ON DELETE CASCADE,
    entry_id INTEGER NOT NULL REFERENCES entries(id) ON DELETE CASCADE,
    UNIQUE(node_id, entry_id)
);
CREATE TABLE IF NOT EXISTS mindmap_edge_refs (
    edge_id  INTEGER NOT NULL REFERENCES mindmap_edges(id) ON DELETE CASCADE,
    entry_id INTEGER NOT NULL REFERENCES entries(id) ON DELETE CASCADE,
    UNIQUE(edge_id, entry_id)
);
CREATE TABLE IF NOT EXISTS mindmap_chains (
    id         INTEGER PRIMARY KEY,
    map_id     INTEGER NOT NULL REFERENCES mindmaps(id) ON DELETE CASCADE,
    name       TEXT NOT NULL,
    signature  TEXT NOT NULL,
    created_at INTEGER NOT NULL,
    UNIQUE(map_id, name),
    UNIQUE(map_id, signature)
);
CREATE TABLE IF NOT EXISTS mindmap_chain_steps (
    chain_id INTEGER NOT NULL REFERENCES mindmap_chains(id) ON DELETE CASCADE,
    pos      INTEGER NOT NULL,
    entry_id INTEGER NOT NULL REFERENCES entries(id) ON DELETE CASCADE,
    PRIMARY KEY(chain_id, pos)
);
";

/// What a node is. The root of every map is its `topic`.
pub const NODE_KINDS: [&str; 10] =
    ["topic", "goal", "fact", "hypothesis", "question", "decision", "idea", "task", "dead-end", "note"];
pub const NODE_STATES: [&str; 3] = ["open", "done", "dropped"];
pub const MAX_TEXT_CHARS: usize = 2_000;
pub const MAX_TITLE_CHARS: usize = 200;
pub const MAX_NODES: i64 = 2_000;
pub const MAX_CHAIN_STEPS: usize = 200;

/// The session a map belongs to, or that asks to write to one.
#[derive(Debug, Clone)]
pub struct Owner {
    pub nick: String,
    pub host: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct MapSummary {
    pub id: i64,
    pub title: String,
    pub owner_nick: String,
    pub owner_host: Option<String>,
    pub state: String,
    pub nodes: i64,
    pub open_nodes: i64,
    pub created_at: i64,
    pub updated_at: i64,
}

#[derive(Debug, Clone, Serialize)]
pub struct Node {
    pub id: i64,
    pub parent: Option<i64>,
    pub kind: String,
    pub text: String,
    pub state: String,
    pub refs: Vec<i64>,
}

#[derive(Debug, Clone, Serialize)]
pub struct Edge {
    pub id: i64,
    pub from: i64,
    pub to: i64,
    pub label: String,
    pub refs: Vec<i64>,
}

#[derive(Debug, Clone, Serialize)]
pub struct Chain {
    pub id: i64,
    pub name: String,
    pub entries: Vec<i64>,
}

/// A whole map, nodes in tree order (parent before child, siblings by `ord`).
#[derive(Debug, Clone, Serialize)]
pub struct Map {
    #[serde(flatten)]
    pub summary: MapSummary,
    pub root: i64,
    pub nodes: Vec<Node>,
    pub edges: Vec<Edge>,
    pub chains: Vec<Chain>,
}

impl Map {
    pub fn node(&self, id: i64) -> Option<&Node> {
        self.nodes.iter().find(|n| n.id == id)
    }
    pub fn children(&self, id: i64) -> impl Iterator<Item = &Node> {
        self.nodes.iter().filter(move |n| n.parent == Some(id))
    }
}

/// What a set of journal references hangs on.
#[derive(Debug, Clone, Copy)]
pub enum Target {
    Node(i64),
    Edge(i64),
}

fn tx(conn: &mut Connection) -> Result<rusqlite::Transaction<'_>> {
    Ok(conn.transaction_with_behavior(TransactionBehavior::Immediate)?)
}

fn clean_text(raw: &str, max: usize, what: &str) -> Result<String> {
    let t = raw.trim();
    if t.is_empty() {
        bail!("{what} is empty");
    }
    if t.chars().count() > max {
        bail!("{what} is longer than {max} characters — a mind-map node is a note, keep the detail in the journal");
    }
    Ok(t.to_string())
}

pub fn normalize_kind(raw: &str) -> Result<String> {
    let k = raw.trim().to_ascii_lowercase().replace(['_', ' '], "-");
    let k = if k == "deadend" { "dead-end".to_string() } else { k };
    if NODE_KINDS.contains(&k.as_str()) {
        Ok(k)
    } else {
        bail!("unknown node kind `{raw}` — one of: {}", NODE_KINDS.join(", "))
    }
}

fn view_token() -> String {
    uuid::Uuid::new_v4().simple().to_string()
}

fn same_owner(nick: &str, host: Option<&str>, who: &Owner) -> bool {
    nick == who.nick
        && match (host, who.host.as_deref()) {
            (Some(a), Some(b)) => a == b,
            _ => true,
        }
}

/// A new map, with its topic as the root node.
pub fn create(conn: &mut Connection, title: &str, owner: &Owner, now: i64) -> Result<(i64, i64)> {
    let title = clean_text(title, MAX_TITLE_CHARS, "the title")?;
    if owner.nick.trim().is_empty() {
        bail!("a mind map belongs to a named session");
    }
    let t = tx(conn)?;
    t.execute(
        "INSERT INTO mindmaps(title, owner_nick, owner_host, view_token, created_at, updated_at) VALUES (?1, ?2, ?3, ?4, ?5, ?5)",
        params![title, owner.nick, owner.host, view_token(), now],
    )?;
    let map = t.last_insert_rowid();
    t.execute(
        "INSERT INTO mindmap_nodes(map_id, parent_id, kind, text, ord, created_at) VALUES (?1, NULL, 'topic', ?2, 0, ?3)",
        params![map, title, now],
    )?;
    let root = t.last_insert_rowid();
    t.commit()?;
    Ok((map, root))
}

fn summary_row(r: &rusqlite::Row<'_>) -> rusqlite::Result<MapSummary> {
    Ok(MapSummary {
        id: r.get(0)?,
        title: r.get(1)?,
        owner_nick: r.get(2)?,
        owner_host: r.get(3)?,
        state: r.get(4)?,
        created_at: r.get(5)?,
        updated_at: r.get(6)?,
        nodes: r.get(7)?,
        open_nodes: r.get(8)?,
    })
}

const SUMMARY_SQL: &str = "SELECT m.id, m.title, m.owner_nick, m.owner_host, m.state, m.created_at, m.updated_at, \
     (SELECT COUNT(*) FROM mindmap_nodes n WHERE n.map_id = m.id), \
     (SELECT COUNT(*) FROM mindmap_nodes n WHERE n.map_id = m.id AND n.state = 'open' AND n.parent_id IS NOT NULL) \
     FROM mindmaps m";

/// The maps of `owner` (every map when `None`: the operator's terminal),
/// open ones only unless `closed_too`, most recently changed first.
pub fn list(conn: &Connection, owner: Option<&Owner>, closed_too: bool) -> Result<Vec<MapSummary>> {
    let mut stmt = conn.prepare(&format!(
        "{SUMMARY_SQL} WHERE (?1 IS NULL OR m.owner_nick = ?1) AND (?2 OR m.state = 'open') ORDER BY m.updated_at DESC, m.id DESC"
    ))?;
    let rows = stmt.query_map(params![owner.map(|o| o.nick.as_str()), closed_too], summary_row)?;
    let all: Vec<MapSummary> = rows.collect::<rusqlite::Result<_>>()?;
    Ok(match owner {
        Some(o) => all.into_iter().filter(|m| same_owner(&m.owner_nick, m.owner_host.as_deref(), o)).collect(),
        None => all,
    })
}

pub fn summary(conn: &Connection, map: i64) -> Result<Option<MapSummary>> {
    Ok(conn.query_row(&format!("{SUMMARY_SQL} WHERE m.id = ?1"), params![map], summary_row).optional()?)
}

pub fn view_token_of(conn: &Connection, map: i64) -> Result<Option<String>> {
    Ok(conn.query_row("SELECT view_token FROM mindmaps WHERE id = ?1", params![map], |r| r.get(0)).optional()?)
}

/// Refuse a write by anyone but the map's owner (journal#94).
pub fn check_owner(conn: &Connection, map: i64, who: &Owner) -> Result<()> {
    let row: Option<(String, Option<String>)> = conn
        .query_row("SELECT owner_nick, owner_host FROM mindmaps WHERE id = ?1", params![map], |r| Ok((r.get(0)?, r.get(1)?)))
        .optional()?;
    let Some((nick, host)) = row else { bail!("no mind map #{map} in this project") };
    if !same_owner(&nick, host.as_deref(), who) {
        bail!("mind map #{map} belongs to session `{nick}` — another session may read it (`mindmap show {map}`), only its owner writes");
    }
    Ok(())
}

fn refs_of(conn: &Connection, sql: &str, id: i64) -> Result<Vec<i64>> {
    let mut stmt = conn.prepare_cached(sql)?;
    let rows = stmt.query_map(params![id], |r| r.get(0))?;
    Ok(rows.collect::<rusqlite::Result<_>>()?)
}

pub fn load(conn: &Connection, map: i64) -> Result<Option<Map>> {
    let Some(summary) = summary(conn, map)? else { return Ok(None) };
    let mut raw: Vec<(Node, i64)> = {
        let mut stmt = conn.prepare("SELECT id, parent_id, kind, text, state, ord FROM mindmap_nodes WHERE map_id = ?1 ORDER BY ord, id")?;
        let rows = stmt.query_map(params![map], |r| {
            Ok((
                Node { id: r.get(0)?, parent: r.get(1)?, kind: r.get(2)?, text: r.get(3)?, state: r.get(4)?, refs: vec![] },
                r.get::<_, i64>(5)?,
            ))
        })?;
        rows.collect::<rusqlite::Result<_>>()?
    };
    for (n, _) in raw.iter_mut() {
        n.refs = refs_of(conn, "SELECT entry_id FROM mindmap_node_refs WHERE node_id = ?1 ORDER BY entry_id", n.id)?;
    }
    let root = raw.iter().find(|(n, _)| n.parent.is_none()).map(|(n, _)| n.id).context("mind map has no root")?;
    // Tree order: depth-first from the root, siblings by ord.
    let mut by_parent: HashMap<Option<i64>, Vec<Node>> = HashMap::new();
    for (n, _) in raw {
        by_parent.entry(n.parent).or_default().push(n);
    }
    let mut nodes = Vec::new();
    let mut stack = vec![by_parent.remove(&None).unwrap_or_default()];
    while let Some(level) = stack.last_mut() {
        if level.is_empty() {
            stack.pop();
            continue;
        }
        let n = level.remove(0);
        let kids = by_parent.remove(&Some(n.id)).unwrap_or_default();
        nodes.push(n);
        stack.push(kids);
    }
    let edges = {
        let mut stmt = conn.prepare("SELECT id, from_node, to_node, label FROM mindmap_edges WHERE map_id = ?1 ORDER BY id")?;
        let rows = stmt.query_map(params![map], |r| {
            Ok(Edge { id: r.get(0)?, from: r.get(1)?, to: r.get(2)?, label: r.get(3)?, refs: vec![] })
        })?;
        let mut v: Vec<Edge> = rows.collect::<rusqlite::Result<_>>()?;
        for e in v.iter_mut() {
            e.refs = refs_of(conn, "SELECT entry_id FROM mindmap_edge_refs WHERE edge_id = ?1 ORDER BY entry_id", e.id)?;
        }
        v
    };
    let chains = {
        let mut stmt = conn.prepare("SELECT id, name FROM mindmap_chains WHERE map_id = ?1 ORDER BY id")?;
        let rows = stmt.query_map(params![map], |r| Ok(Chain { id: r.get(0)?, name: r.get(1)?, entries: vec![] }))?;
        let mut v: Vec<Chain> = rows.collect::<rusqlite::Result<_>>()?;
        for c in v.iter_mut() {
            c.entries = refs_of(conn, "SELECT entry_id FROM mindmap_chain_steps WHERE chain_id = ?1 ORDER BY pos", c.id)?;
        }
        v
    };
    Ok(Some(Map { summary, root, nodes, edges, chains }))
}

fn touch(conn: &Connection, map: i64, now: i64) -> Result<()> {
    conn.execute("UPDATE mindmaps SET updated_at = ?2 WHERE id = ?1", params![map, now])?;
    Ok(())
}

fn root_of(conn: &Connection, map: i64) -> Result<i64> {
    conn.query_row("SELECT id FROM mindmap_nodes WHERE map_id = ?1 AND parent_id IS NULL", params![map], |r| r.get(0))
        .context("mind map has no root")
}

fn node_in(conn: &Connection, map: i64, node: i64) -> Result<()> {
    let found: Option<i64> =
        conn.query_row("SELECT map_id FROM mindmap_nodes WHERE id = ?1", params![node], |r| r.get(0)).optional()?;
    match found {
        Some(m) if m == map => Ok(()),
        _ => bail!("node {node} is not in mind map #{map}"),
    }
}

fn edge_in(conn: &Connection, map: i64, edge: i64) -> Result<()> {
    let found: Option<i64> =
        conn.query_row("SELECT map_id FROM mindmap_edges WHERE id = ?1", params![edge], |r| r.get(0)).optional()?;
    match found {
        Some(m) if m == map => Ok(()),
        _ => bail!("link {edge} is not in mind map #{map}"),
    }
}

fn entries_exist(conn: &Connection, entries: &[i64]) -> Result<()> {
    for e in entries {
        let n: i64 = conn.query_row("SELECT COUNT(*) FROM entries WHERE id = ?1", params![e], |r| r.get(0))?;
        if n == 0 {
            bail!("journal#{e} does not exist in this project's journal");
        }
    }
    Ok(())
}

fn insert_refs(conn: &Connection, target: Target, entries: &[i64]) -> Result<usize> {
    let mut n = 0;
    for e in entries {
        n += match target {
            Target::Node(id) => conn.execute("INSERT OR IGNORE INTO mindmap_node_refs(node_id, entry_id) VALUES (?1, ?2)", params![id, e])?,
            Target::Edge(id) => conn.execute("INSERT OR IGNORE INTO mindmap_edge_refs(edge_id, entry_id) VALUES (?1, ?2)", params![id, e])?,
        };
    }
    Ok(n)
}

fn is_open(conn: &Connection, map: i64) -> Result<()> {
    let state: String = conn.query_row("SELECT state FROM mindmaps WHERE id = ?1", params![map], |r| r.get(0))?;
    if state != "open" {
        bail!("mind map #{map} is closed — `mindmap reopen {map}` to change it again");
    }
    Ok(())
}

/// Add a node under `parent` (the root when `None`).
pub fn add_node(
    conn: &mut Connection,
    map: i64,
    parent: Option<i64>,
    kind: &str,
    text: &str,
    refs: &[i64],
    now: i64,
) -> Result<i64> {
    let kind = normalize_kind(kind)?;
    if kind == "topic" {
        bail!("`topic` is the root's kind; pick another for a node under it");
    }
    let text = clean_text(text, MAX_TEXT_CHARS, "the node text")?;
    let t = tx(conn)?;
    is_open(&t, map)?;
    let parent = match parent {
        Some(p) => {
            node_in(&t, map, p)?;
            p
        }
        None => root_of(&t, map)?,
    };
    let count: i64 = t.query_row("SELECT COUNT(*) FROM mindmap_nodes WHERE map_id = ?1", params![map], |r| r.get(0))?;
    if count >= MAX_NODES {
        bail!("mind map #{map} already has {MAX_NODES} nodes — close it and start another, or prune it");
    }
    entries_exist(&t, refs)?;
    let ord: i64 = t.query_row(
        "SELECT COALESCE(MAX(ord), -1) + 1 FROM mindmap_nodes WHERE parent_id = ?1",
        params![parent],
        |r| r.get(0),
    )?;
    t.execute(
        "INSERT INTO mindmap_nodes(map_id, parent_id, kind, text, ord, created_at) VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
        params![map, parent, kind, text, ord, now],
    )?;
    let id = t.last_insert_rowid();
    insert_refs(&t, Target::Node(id), refs)?;
    touch(&t, map, now)?;
    t.commit()?;
    Ok(id)
}

/// Change a node's text and/or kind. The root's text is the map's title.
pub fn edit_node(conn: &mut Connection, map: i64, node: i64, text: Option<&str>, kind: Option<&str>, now: i64) -> Result<()> {
    let t = tx(conn)?;
    is_open(&t, map)?;
    node_in(&t, map, node)?;
    let root = root_of(&t, map)?;
    if let Some(k) = kind {
        let k = normalize_kind(k)?;
        if (node == root) != (k == "topic") {
            bail!("only the root is a `topic`, and the root stays one");
        }
        t.execute("UPDATE mindmap_nodes SET kind = ?2 WHERE id = ?1", params![node, k])?;
    }
    if let Some(text) = text {
        let max = if node == root { MAX_TITLE_CHARS } else { MAX_TEXT_CHARS };
        let text = clean_text(text, max, "the node text")?;
        t.execute("UPDATE mindmap_nodes SET text = ?2 WHERE id = ?1", params![node, text])?;
        if node == root {
            t.execute("UPDATE mindmaps SET title = ?2 WHERE id = ?1", params![map, text])?;
        }
    }
    touch(&t, map, now)?;
    t.commit()?;
    Ok(())
}

/// Move a node (and its subtree) under another parent.
pub fn move_node(conn: &mut Connection, map: i64, node: i64, parent: i64, now: i64) -> Result<()> {
    let t = tx(conn)?;
    is_open(&t, map)?;
    node_in(&t, map, node)?;
    node_in(&t, map, parent)?;
    if node == root_of(&t, map)? {
        bail!("the root does not move");
    }
    // The new parent must not sit inside the subtree being moved.
    let mut cur = Some(parent);
    while let Some(c) = cur {
        if c == node {
            bail!("node {parent} is inside node {node}'s subtree — that would make a cycle");
        }
        cur = t.query_row("SELECT parent_id FROM mindmap_nodes WHERE id = ?1", params![c], |r| r.get(0))?;
    }
    let ord: i64 = t.query_row(
        "SELECT COALESCE(MAX(ord), -1) + 1 FROM mindmap_nodes WHERE parent_id = ?1",
        params![parent],
        |r| r.get(0),
    )?;
    t.execute("UPDATE mindmap_nodes SET parent_id = ?2, ord = ?3 WHERE id = ?1", params![node, parent, ord])?;
    touch(&t, map, now)?;
    t.commit()?;
    Ok(())
}

pub fn set_node_state(conn: &mut Connection, map: i64, node: i64, state: &str, now: i64) -> Result<()> {
    if !NODE_STATES.contains(&state) {
        bail!("unknown node state `{state}`");
    }
    let t = tx(conn)?;
    is_open(&t, map)?;
    node_in(&t, map, node)?;
    if node == root_of(&t, map)? {
        bail!("the root has no state of its own — close the map instead (`mindmap close {map}`)");
    }
    t.execute("UPDATE mindmap_nodes SET state = ?2 WHERE id = ?1", params![node, state])?;
    touch(&t, map, now)?;
    t.commit()?;
    Ok(())
}

/// Remove a node and everything under it. Returns how many nodes went.
pub fn remove_node(conn: &mut Connection, map: i64, node: i64, now: i64) -> Result<usize> {
    let t = tx(conn)?;
    is_open(&t, map)?;
    node_in(&t, map, node)?;
    if node == root_of(&t, map)? {
        bail!("the root cannot be removed — delete the map instead (`mindmap delete {map}`)");
    }
    let before: i64 = t.query_row("SELECT COUNT(*) FROM mindmap_nodes WHERE map_id = ?1", params![map], |r| r.get(0))?;
    t.execute("DELETE FROM mindmap_nodes WHERE id = ?1", params![node])?;
    let after: i64 = t.query_row("SELECT COUNT(*) FROM mindmap_nodes WHERE map_id = ?1", params![map], |r| r.get(0))?;
    touch(&t, map, now)?;
    t.commit()?;
    Ok((before - after) as usize)
}

/// A cross-link between two nodes, optionally labelled ("contradicts",
/// "depends on") and backed by journal entries.
pub fn add_edge(conn: &mut Connection, map: i64, from: i64, to: i64, label: &str, refs: &[i64], now: i64) -> Result<i64> {
    if from == to {
        bail!("a link joins two different nodes");
    }
    let label = label.trim();
    if label.chars().count() > MAX_TITLE_CHARS {
        bail!("a link label is longer than {MAX_TITLE_CHARS} characters");
    }
    let t = tx(conn)?;
    is_open(&t, map)?;
    node_in(&t, map, from)?;
    node_in(&t, map, to)?;
    entries_exist(&t, refs)?;
    t.execute(
        "INSERT OR IGNORE INTO mindmap_edges(map_id, from_node, to_node, label) VALUES (?1, ?2, ?3, ?4)",
        params![map, from, to, label],
    )?;
    let id: i64 = t.query_row(
        "SELECT id FROM mindmap_edges WHERE from_node = ?1 AND to_node = ?2 AND label = ?3",
        params![from, to, label],
        |r| r.get(0),
    )?;
    insert_refs(&t, Target::Edge(id), refs)?;
    touch(&t, map, now)?;
    t.commit()?;
    Ok(id)
}

pub fn remove_edge(conn: &mut Connection, map: i64, edge: i64, now: i64) -> Result<()> {
    let t = tx(conn)?;
    is_open(&t, map)?;
    edge_in(&t, map, edge)?;
    t.execute("DELETE FROM mindmap_edges WHERE id = ?1", params![edge])?;
    touch(&t, map, now)?;
    t.commit()?;
    Ok(())
}

/// Point a node or a link at journal entries. Returns how many were new.
pub fn add_refs(conn: &mut Connection, map: i64, target: Target, entries: &[i64], now: i64) -> Result<usize> {
    if entries.is_empty() {
        bail!("name at least one journal entry");
    }
    let t = tx(conn)?;
    is_open(&t, map)?;
    match target {
        Target::Node(n) => node_in(&t, map, n)?,
        Target::Edge(e) => edge_in(&t, map, e)?,
    }
    entries_exist(&t, entries)?;
    let n = insert_refs(&t, target, entries)?;
    touch(&t, map, now)?;
    t.commit()?;
    Ok(n)
}

pub fn remove_refs(conn: &mut Connection, map: i64, target: Target, entries: &[i64], now: i64) -> Result<usize> {
    let t = tx(conn)?;
    is_open(&t, map)?;
    let mut n = 0;
    match target {
        Target::Node(id) => {
            node_in(&t, map, id)?;
            for e in entries {
                n += t.execute("DELETE FROM mindmap_node_refs WHERE node_id = ?1 AND entry_id = ?2", params![id, e])?;
            }
        }
        Target::Edge(id) => {
            edge_in(&t, map, id)?;
            for e in entries {
                n += t.execute("DELETE FROM mindmap_edge_refs WHERE edge_id = ?1 AND entry_id = ?2", params![id, e])?;
            }
        }
    }
    touch(&t, map, now)?;
    t.commit()?;
    Ok(n)
}

fn signature(entries: &[i64]) -> String {
    entries.iter().map(i64::to_string).collect::<Vec<_>>().join(",")
}

/// A named, ordered sequence of journal entries — a line of reasoning, the
/// history of a fix. Its name and its sequence are both unique in the map.
pub fn add_chain(conn: &mut Connection, map: i64, name: &str, entries: &[i64], now: i64) -> Result<i64> {
    let name = clean_text(name, MAX_TITLE_CHARS, "the chain name")?;
    if entries.len() < 2 {
        bail!("a chain links at least two entries");
    }
    if entries.len() > MAX_CHAIN_STEPS {
        bail!("a chain has at most {MAX_CHAIN_STEPS} entries");
    }
    let mut seen = std::collections::HashSet::new();
    if let Some(dup) = entries.iter().find(|e| !seen.insert(**e)) {
        bail!("journal#{dup} appears twice in the chain");
    }
    let sig = signature(entries);
    let t = tx(conn)?;
    is_open(&t, map)?;
    entries_exist(&t, entries)?;
    let by_name: Option<i64> =
        t.query_row("SELECT id FROM mindmap_chains WHERE map_id = ?1 AND name = ?2", params![map, name], |r| r.get(0)).optional()?;
    if by_name.is_some() {
        bail!("mind map #{map} already has a chain named `{name}`");
    }
    let by_sig: Option<String> = t
        .query_row("SELECT name FROM mindmap_chains WHERE map_id = ?1 AND signature = ?2", params![map, sig], |r| r.get(0))
        .optional()?;
    if let Some(other) = by_sig {
        bail!("chain `{other}` already is this exact sequence of entries");
    }
    t.execute(
        "INSERT INTO mindmap_chains(map_id, name, signature, created_at) VALUES (?1, ?2, ?3, ?4)",
        params![map, name, sig, now],
    )?;
    let id = t.last_insert_rowid();
    for (pos, e) in entries.iter().enumerate() {
        t.execute("INSERT INTO mindmap_chain_steps(chain_id, pos, entry_id) VALUES (?1, ?2, ?3)", params![id, pos as i64, e])?;
    }
    touch(&t, map, now)?;
    t.commit()?;
    Ok(id)
}

pub fn remove_chain(conn: &mut Connection, map: i64, name: &str, now: i64) -> Result<()> {
    let t = tx(conn)?;
    is_open(&t, map)?;
    let n = t.execute("DELETE FROM mindmap_chains WHERE map_id = ?1 AND name = ?2", params![map, name.trim()])?;
    if n == 0 {
        bail!("mind map #{map} has no chain named `{}`", name.trim());
    }
    touch(&t, map, now)?;
    t.commit()?;
    Ok(())
}

/// Close (`open = false`) or reopen a map. A closed map is kept and readable,
/// but no longer named in the session's reminder.
pub fn set_state(conn: &Connection, map: i64, open: bool, now: i64) -> Result<()> {
    let state = if open { "open" } else { "closed" };
    let n = conn.execute("UPDATE mindmaps SET state = ?2, updated_at = ?3 WHERE id = ?1", params![map, state, now])?;
    if n == 0 {
        bail!("no mind map #{map} in this project");
    }
    Ok(())
}

pub fn delete(conn: &Connection, map: i64) -> Result<()> {
    let n = conn.execute("DELETE FROM mindmaps WHERE id = ?1", params![map])?;
    if n == 0 {
        bail!("no mind map #{map} in this project");
    }
    Ok(())
}

/// Copy every map of `src` into `dst` (journal share), journal references
/// renumbered through `entries` (src id -> dst id). A reference whose entry
/// did not come along is dropped; a chain that loses a step is skipped.
pub fn merge(dst: &mut Connection, src: &Connection, entries: &HashMap<i64, i64>) -> Result<usize> {
    let ids: Vec<i64> = {
        let mut stmt = src.prepare("SELECT id FROM mindmaps ORDER BY id")?;
        let rows = stmt.query_map([], |r| r.get(0))?;
        rows.collect::<rusqlite::Result<_>>()?
    };
    let t = tx(dst)?;
    for old in &ids {
        let (title, nick, host, state, created, updated): (String, String, Option<String>, String, i64, i64) = src.query_row(
            "SELECT title, owner_nick, owner_host, state, created_at, updated_at FROM mindmaps WHERE id = ?1",
            params![old],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?, r.get(5)?)),
        )?;
        t.execute(
            "INSERT INTO mindmaps(title, owner_nick, owner_host, state, view_token, created_at, updated_at) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            params![title, nick, host, state, view_token(), created, updated],
        )?;
        let map = t.last_insert_rowid();
        let map_of = |e: &i64| entries.get(e).copied();
        let mut nodes: HashMap<i64, i64> = HashMap::new();
        {
            // Parents first: walk by depth.
            let mut stmt = src.prepare("SELECT id, parent_id, kind, text, state, ord, created_at FROM mindmap_nodes WHERE map_id = ?1")?;
            let rows = stmt.query_map(params![old], |r| {
                Ok((r.get::<_, i64>(0)?, r.get::<_, Option<i64>>(1)?, r.get::<_, String>(2)?, r.get::<_, String>(3)?, r.get::<_, String>(4)?, r.get::<_, i64>(5)?, r.get::<_, i64>(6)?))
            })?;
            let mut pending: Vec<_> = rows.collect::<rusqlite::Result<_>>()?;
            while !pending.is_empty() {
                let before = pending.len();
                pending.retain(|(id, parent, kind, text, st, ord, created)| {
                    let new_parent = match parent {
                        None => None,
                        Some(p) => match nodes.get(p) {
                            Some(np) => Some(*np),
                            None => return true,
                        },
                    };
                    let ok = t.execute(
                        "INSERT INTO mindmap_nodes(map_id, parent_id, kind, text, state, ord, created_at) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
                        params![map, new_parent, kind, text, st, ord, created],
                    );
                    if ok.is_ok() {
                        nodes.insert(*id, t.last_insert_rowid());
                    }
                    false
                });
                if pending.len() == before {
                    break;
                }
            }
        }
        let node_refs: Vec<(i64, i64)> = {
            let mut stmt = src.prepare(
                "SELECT r.node_id, r.entry_id FROM mindmap_node_refs r JOIN mindmap_nodes n ON n.id = r.node_id WHERE n.map_id = ?1",
            )?;
            let rows = stmt.query_map(params![old], |r| Ok((r.get(0)?, r.get(1)?)))?;
            rows.collect::<rusqlite::Result<_>>()?
        };
        for (n, e) in node_refs {
            if let (Some(n), Some(e)) = (nodes.get(&n), map_of(&e)) {
                t.execute("INSERT OR IGNORE INTO mindmap_node_refs(node_id, entry_id) VALUES (?1, ?2)", params![n, e])?;
            }
        }
        let edges: Vec<(i64, i64, i64, String)> = {
            let mut stmt = src.prepare("SELECT id, from_node, to_node, label FROM mindmap_edges WHERE map_id = ?1")?;
            let rows = stmt.query_map(params![old], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))?;
            rows.collect::<rusqlite::Result<_>>()?
        };
        for (old_edge, from, to, label) in edges {
            let (Some(f), Some(to)) = (nodes.get(&from), nodes.get(&to)) else { continue };
            t.execute(
                "INSERT OR IGNORE INTO mindmap_edges(map_id, from_node, to_node, label) VALUES (?1, ?2, ?3, ?4)",
                params![map, f, to, label],
            )?;
            let new_edge = t.last_insert_rowid();
            let refs = refs_of(src, "SELECT entry_id FROM mindmap_edge_refs WHERE edge_id = ?1", old_edge)?;
            for e in refs.iter().filter_map(map_of) {
                t.execute("INSERT OR IGNORE INTO mindmap_edge_refs(edge_id, entry_id) VALUES (?1, ?2)", params![new_edge, e])?;
            }
        }
        let chains: Vec<(i64, String, i64)> = {
            let mut stmt = src.prepare("SELECT id, name, created_at FROM mindmap_chains WHERE map_id = ?1 ORDER BY id")?;
            let rows = stmt.query_map(params![old], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?;
            rows.collect::<rusqlite::Result<_>>()?
        };
        for (old_chain, name, created) in chains {
            let steps = refs_of(src, "SELECT entry_id FROM mindmap_chain_steps WHERE chain_id = ?1 ORDER BY pos", old_chain)?;
            let mapped: Vec<i64> = steps.iter().filter_map(map_of).collect();
            if mapped.len() != steps.len() {
                continue;
            }
            t.execute(
                "INSERT OR IGNORE INTO mindmap_chains(map_id, name, signature, created_at) VALUES (?1, ?2, ?3, ?4)",
                params![map, name, signature(&mapped), created],
            )?;
            let chain = t.last_insert_rowid();
            for (pos, e) in mapped.iter().enumerate() {
                t.execute("INSERT OR IGNORE INTO mindmap_chain_steps(chain_id, pos, entry_id) VALUES (?1, ?2, ?3)", params![chain, pos as i64, e])?;
            }
        }
    }
    t.commit()?;
    Ok(ids.len())
}

/// The map as an indented outline — what `mindmap show` prints and an agent
/// reads back into its context.
pub fn outline(map: &Map) -> String {
    let s = &map.summary;
    let mut out = format!(
        "mind map #{} \"{}\" — {}, owner {}, {} nodes ({} open)\n",
        s.id, s.title, s.state, s.owner_nick, s.nodes, s.open_nodes
    );
    let mut depth: HashMap<i64, usize> = HashMap::new();
    for n in &map.nodes {
        let d = n.parent.and_then(|p| depth.get(&p).copied()).map(|d| d + 1).unwrap_or(0);
        depth.insert(n.id, d);
        let mark = match n.state.as_str() {
            "done" => " [done]",
            "dropped" => " [dropped]",
            _ => "",
        };
        let refs = if n.refs.is_empty() {
            String::new()
        } else {
            format!("  → {}", n.refs.iter().map(|r| format!("journal#{r}")).collect::<Vec<_>>().join(" "))
        };
        let text = n.text.replace('\n', " ");
        out.push_str(&format!("{}- n{} ({}) {text}{mark}{refs}\n", "  ".repeat(d), n.id, n.kind));
    }
    if !map.edges.is_empty() {
        out.push_str("links:\n");
        for e in &map.edges {
            let label = if e.label.is_empty() { String::new() } else { format!(" \"{}\"", e.label) };
            let refs = if e.refs.is_empty() {
                String::new()
            } else {
                format!("  → {}", e.refs.iter().map(|r| format!("journal#{r}")).collect::<Vec<_>>().join(" "))
            };
            out.push_str(&format!("- l{}: n{} → n{}{label}{refs}\n", e.id, e.from, e.to));
        }
    }
    if !map.chains.is_empty() {
        out.push_str("chains:\n");
        for c in &map.chains {
            let steps = c.entries.iter().map(|e| format!("journal#{e}")).collect::<Vec<_>>().join(" → ");
            out.push_str(&format!("- {}: {steps}\n", c.name));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::journal::{self, NewEntry};
    use std::path::Path;

    fn journal() -> (tempfile::TempDir, Connection) {
        let dir = tempfile::tempdir().unwrap();
        let conn = journal::open(dir.path(), Path::new("/work/proj"), true).unwrap().unwrap();
        (dir, conn)
    }

    fn entry(conn: &mut Connection, body: &str) -> i64 {
        let e = NewEntry {
            body: body.into(),
            category: "finding".into(),
            tags: vec!["x".into()],
            salience: "medium".into(),
            ..Default::default()
        };
        journal::add(conn, &e, 10).unwrap().1
    }

    fn mira() -> Owner {
        Owner { nick: "mira".into(), host: Some("box".into()) }
    }

    #[test]
    fn a_map_grows_moves_and_prunes_as_a_tree() {
        let (_d, mut c) = journal();
        let (m, root) = create(&mut c, "why is the pool slow", &mira(), 1).unwrap();
        let a = add_node(&mut c, m, None, "hypothesis", "lock contention", &[], 2).unwrap();
        let b = add_node(&mut c, m, Some(a), "question", "which lock?", &[], 3).unwrap();
        let x = add_node(&mut c, m, None, "Dead End", "GC pauses", &[], 4).unwrap();
        assert!(add_node(&mut c, m, None, "topic", "second root", &[], 5).is_err());
        assert!(add_node(&mut c, m, None, "vibe", "?", &[], 5).is_err());
        // No cycles, no moving the root.
        assert!(move_node(&mut c, m, a, b, 6).is_err());
        assert!(move_node(&mut c, m, root, a, 6).is_err());
        move_node(&mut c, m, x, a, 6).unwrap();
        set_node_state(&mut c, m, x, "dropped", 7).unwrap();
        let map = load(&c, m).unwrap().unwrap();
        assert_eq!(map.nodes.iter().map(|n| n.id).collect::<Vec<_>>(), vec![root, a, b, x], "tree order");
        assert_eq!(map.node(x).unwrap().state, "dropped");
        assert_eq!(map.node(x).unwrap().kind, "dead-end");
        // Removing a node takes its subtree.
        assert_eq!(remove_node(&mut c, m, a, 8).unwrap(), 3);
        assert!(remove_node(&mut c, m, root, 8).is_err());
        assert_eq!(load(&c, m).unwrap().unwrap().nodes.len(), 1);
        // The root's text is the title.
        edit_node(&mut c, m, root, Some("why is the pool slow under load"), None, 9).unwrap();
        assert_eq!(summary(&c, m).unwrap().unwrap().title, "why is the pool slow under load");
    }

    #[test]
    fn nodes_and_links_point_at_journal_entries_and_chains_are_unique() {
        let (_d, mut c) = journal();
        let e1 = entry(&mut c, "the pool queue grows when the bus is saturated");
        let e2 = entry(&mut c, "the bus has a single writer task");
        let e3 = entry(&mut c, "fix: shard the writer");
        let (m, _) = create(&mut c, "pool", &mira(), 1).unwrap();
        let a = add_node(&mut c, m, None, "fact", "queue grows", &[e1], 2).unwrap();
        let b = add_node(&mut c, m, None, "fact", "single writer", &[], 3).unwrap();
        assert!(add_node(&mut c, m, None, "fact", "ghost", &[9999], 3).is_err(), "a missing entry is refused");
        let l = add_edge(&mut c, m, a, b, "caused by", &[e2], 4).unwrap();
        assert!(add_edge(&mut c, m, a, a, "", &[], 4).is_err());
        add_refs(&mut c, m, Target::Node(b), &[e2, e3], 5).unwrap();
        add_chain(&mut c, m, "the fix", &[e1, e2, e3], 6).unwrap();
        assert!(add_chain(&mut c, m, "the fix", &[e3, e1], 7).is_err(), "same name");
        assert!(add_chain(&mut c, m, "again", &[e1, e2, e3], 7).is_err(), "same sequence");
        assert!(add_chain(&mut c, m, "loop", &[e1, e2, e1], 7).is_err(), "repeated entry");
        add_chain(&mut c, m, "reverse", &[e3, e2, e1], 7).unwrap();
        let map = load(&c, m).unwrap().unwrap();
        assert_eq!(map.node(a).unwrap().refs, vec![e1]);
        assert_eq!(map.node(b).unwrap().refs, vec![e2, e3]);
        assert_eq!(map.edges[0].id, l);
        assert_eq!(map.edges[0].refs, vec![e2]);
        assert_eq!(map.chains[0].entries, vec![e1, e2, e3]);
        let text = outline(&map);
        assert!(text.contains(&format!("→ journal#{e1}")), "{text}");
        assert!(text.contains("the fix: journal#"), "{text}");
        // An entry deleted from the journal drops out of the map.
        journal::delete(&c, e2).unwrap();
        let map = load(&c, m).unwrap().unwrap();
        assert_eq!(map.node(b).unwrap().refs, vec![e3]);
    }

    #[test]
    fn only_the_owner_writes_and_lists_show_own_open_maps() {
        let (_d, mut c) = journal();
        let (m, _) = create(&mut c, "mine", &mira(), 1).unwrap();
        let (other, _) = create(&mut c, "atlas's", &Owner { nick: "atlas".into(), host: Some("box".into()) }, 2).unwrap();
        check_owner(&c, m, &mira()).unwrap();
        assert!(check_owner(&c, other, &mira()).unwrap_err().to_string().contains("belongs to session `atlas`"));
        // Same nick on another machine is someone else.
        assert!(check_owner(&c, m, &Owner { nick: "mira".into(), host: Some("other".into()) }).is_err());
        let mine: Vec<i64> = list(&c, Some(&mira()), false).unwrap().iter().map(|s| s.id).collect();
        assert_eq!(mine, vec![m]);
        set_state(&c, m, false, 3).unwrap();
        assert!(list(&c, Some(&mira()), false).unwrap().is_empty(), "closed maps are not listed");
        assert!(add_node(&mut c, m, None, "idea", "x", &[], 4).is_err(), "a closed map is read-only");
        assert_eq!(list(&c, Some(&mira()), true).unwrap().len(), 1);
        assert_eq!(list(&c, None, true).unwrap().len(), 2, "the operator sees every map");
        delete(&c, m).unwrap();
        assert!(load(&c, m).unwrap().is_none());
    }

    #[test]
    fn merge_carries_maps_with_renumbered_entries() {
        let (_d, mut src) = journal();
        let e1 = entry(&mut src, "first finding about the cache");
        let e2 = entry(&mut src, "second finding about the cache");
        let (m, _) = create(&mut src, "cache", &mira(), 1).unwrap();
        let a = add_node(&mut src, m, None, "fact", "a", &[e1], 2).unwrap();
        let b = add_node(&mut src, m, Some(a), "idea", "b", &[], 3).unwrap();
        add_edge(&mut src, m, b, a, "refines", &[e2], 4).unwrap();
        add_chain(&mut src, m, "story", &[e1, e2], 5).unwrap();

        let dir = tempfile::tempdir().unwrap();
        let mut dst = journal::open(dir.path(), Path::new("/work/other"), true).unwrap().unwrap();
        let pad = entry(&mut dst, "an entry the destination already had");
        journal::merge_into(&mut dst, &src).unwrap();
        let maps = list(&dst, None, true).unwrap();
        assert_eq!(maps.len(), 1);
        let map = load(&dst, maps[0].id).unwrap().unwrap();
        assert_eq!(map.nodes.len(), 3);
        let n1 = map.nodes.iter().find(|n| n.text == "a").unwrap();
        assert_eq!(n1.refs.len(), 1);
        assert_ne!(n1.refs[0], pad);
        let body = journal::get(&dst, n1.refs[0]).unwrap().unwrap().body;
        assert_eq!(body, "first finding about the cache");
        assert_eq!(map.edges.len(), 1);
        assert_eq!(map.edges[0].label, "refines");
        assert_eq!(map.chains[0].entries.len(), 2);
        assert_eq!(map.summary.owner_nick, "mira");
    }
}
