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

/// Columns added after v0.23.1, whose journals already hold the tables:
/// `CREATE TABLE IF NOT EXISTS` leaves an existing table as it is, so each is
/// added here when missing (journal#108).
const COLUMNS: [(&str, &str, &str); 4] = [
    ("mindmaps", "shared", "INTEGER NOT NULL DEFAULT 0"),
    ("mindmaps", "layout", "TEXT NOT NULL DEFAULT 'mindmap'"),
    ("mindmaps", "direction", "TEXT NOT NULL DEFAULT 'down'"),
    ("mindmap_nodes", "author", "TEXT"),
];

/// Bring the mind-map tables of an opened journal up to date.
pub fn migrate(conn: &Connection) -> Result<()> {
    for (table, column, decl) in COLUMNS {
        let has: bool = conn
            .prepare(&format!("SELECT 1 FROM pragma_table_info('{table}') WHERE name = ?1"))?
            .exists(params![column])?;
        if !has {
            conn.execute_batch(&format!("ALTER TABLE {table} ADD COLUMN {column} {decl}"))
                .with_context(|| format!("add {table}.{column}"))?;
        }
    }
    Ok(())
}

/// The two kinds of map (journal#107): a two-sided tree of notes around a
/// topic, and a block diagram of directed arrows between ISO 5807 symbols and
/// logic gates.
pub const LAYOUTS: [&str; 2] = ["mindmap", "flowchart"];
pub const DIRECTIONS: [&str; 2] = ["down", "right"];

/// What a node of a `mindmap` is. The root of every map is its `topic`.
pub const NODE_KINDS: [&str; 10] =
    ["topic", "goal", "fact", "hypothesis", "question", "decision", "idea", "task", "dead-end", "note"];

/// What a node of a `flowchart` is: ISO 5807 symbols, then logic gates.
pub const FLOW_KINDS: [&str; 20] = [
    "start", "end", "process", "decision", "io", "subprocess", "document", "data", "manual", "prep",
    "connector", "delay", "note", "and", "or", "not", "nand", "nor", "xor", "xnor",
];
pub const GATES: [&str; 7] = ["and", "or", "not", "nand", "nor", "xor", "xnor"];
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
    pub shared: bool,
    pub layout: String,
    pub direction: String,
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
    /// The session that added it — on a shared map, not always the owner.
    pub author: Option<String>,
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

/// A node kind as stored, checked against the kinds of the map's layout.
pub fn normalize_kind(raw: &str, layout: &str) -> Result<String> {
    let k = raw.trim().to_ascii_lowercase().replace(['_', ' '], "-");
    let k = match k.as_str() {
        "deadend" => "dead-end".to_string(),
        "terminator" | "begin" => "start".into(),
        "stop" | "finish" => "end".into(),
        "input" | "output" | "input-output" | "i/o" => "io".into(),
        "predefined" | "predefined-process" => "subprocess".into(),
        "database" | "storage" | "stored-data" => "data".into(),
        "manual-input" => "manual".into(),
        "preparation" => "prep".into(),
        _ => k,
    };
    let kinds: &[&str] = if layout == "flowchart" { &FLOW_KINDS } else { &NODE_KINDS };
    if kinds.contains(&k.as_str()) {
        Ok(k)
    } else {
        bail!("unknown {layout} node kind `{raw}` — one of: {}", kinds.join(", "))
    }
}

pub fn normalize_layout(raw: &str) -> Result<String> {
    let l = raw.trim().to_ascii_lowercase();
    let l = match l.as_str() {
        "mind-map" | "tree" | "classic" => "mindmap".to_string(),
        "flow" | "flow-chart" | "block" | "diagram" => "flowchart".to_string(),
        _ => l,
    };
    if LAYOUTS.contains(&l.as_str()) {
        Ok(l)
    } else {
        bail!("unknown map type `{raw}` — mindmap or flowchart")
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

/// How a new map is set up.
#[derive(Debug, Clone)]
pub struct NewMap<'a> {
    pub title: &'a str,
    /// `mindmap` or `flowchart`.
    pub layout: &'a str,
    /// For a flowchart: `down` (top to bottom) or `right` (left to right).
    pub direction: &'a str,
    pub shared: bool,
}

impl<'a> NewMap<'a> {
    pub fn mindmap(title: &'a str) -> Self {
        NewMap { title, layout: "mindmap", direction: "down", shared: false }
    }
}

/// A new map of the classic kind, with its topic as the root node.
pub fn create(conn: &mut Connection, title: &str, owner: &Owner, now: i64) -> Result<(i64, i64)> {
    create_map(conn, &NewMap::mindmap(title), owner, now)
}

/// A new map. Its root holds the title: the topic of a mind map, the caption
/// of a flowchart (which draws no root).
pub fn create_map(conn: &mut Connection, spec: &NewMap<'_>, owner: &Owner, now: i64) -> Result<(i64, i64)> {
    let title = clean_text(spec.title, MAX_TITLE_CHARS, "the title")?;
    if owner.nick.trim().is_empty() {
        bail!("a mind map belongs to a named session");
    }
    let layout = normalize_layout(spec.layout)?;
    let direction = spec.direction.trim().to_ascii_lowercase();
    if !DIRECTIONS.contains(&direction.as_str()) {
        bail!("unknown direction `{}` — down or right", spec.direction);
    }
    let t = tx(conn)?;
    t.execute(
        "INSERT INTO mindmaps(title, owner_nick, owner_host, view_token, created_at, updated_at, shared, layout, direction) \
         VALUES (?1, ?2, ?3, ?4, ?5, ?5, ?6, ?7, ?8)",
        params![title, owner.nick, owner.host, view_token(), now, spec.shared, layout, direction],
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
        shared: r.get(9)?,
        layout: r.get(10)?,
        direction: r.get(11)?,
    })
}

const SUMMARY_SQL: &str = "SELECT m.id, m.title, m.owner_nick, m.owner_host, m.state, m.created_at, m.updated_at, \
     (SELECT COUNT(*) FROM mindmap_nodes n WHERE n.map_id = m.id), \
     (SELECT COUNT(*) FROM mindmap_nodes n WHERE n.map_id = m.id AND n.state = 'open' AND n.parent_id IS NOT NULL), \
     m.shared, m.layout, m.direction \
     FROM mindmaps m";

impl MapSummary {
    pub fn owned_by(&self, who: &Owner) -> bool {
        same_owner(&self.owner_nick, self.owner_host.as_deref(), who)
    }
}

/// The maps `viewer` may see listed: its own and the project's shared ones.
/// Every map when `None` (the operator's terminal). Open ones only unless
/// `closed_too`; most recently changed first.
pub fn list(conn: &Connection, viewer: Option<&Owner>, closed_too: bool) -> Result<Vec<MapSummary>> {
    let mut stmt = conn.prepare(&format!(
        "{SUMMARY_SQL} WHERE (?1 OR m.state = 'open') ORDER BY m.updated_at DESC, m.id DESC"
    ))?;
    let rows = stmt.query_map(params![closed_too], summary_row)?;
    let all: Vec<MapSummary> = rows.collect::<rusqlite::Result<_>>()?;
    Ok(match viewer {
        Some(v) => all.into_iter().filter(|m| m.shared || m.owned_by(v)).collect(),
        None => all,
    })
}

pub fn summary(conn: &Connection, map: i64) -> Result<Option<MapSummary>> {
    Ok(conn.query_row(&format!("{SUMMARY_SQL} WHERE m.id = ?1"), params![map], summary_row).optional()?)
}

pub fn view_token_of(conn: &Connection, map: i64) -> Result<Option<String>> {
    Ok(conn.query_row("SELECT view_token FROM mindmaps WHERE id = ?1", params![map], |r| r.get(0)).optional()?)
}

fn owner_row(conn: &Connection, map: i64) -> Result<(String, Option<String>, bool)> {
    let row = conn
        .query_row("SELECT owner_nick, owner_host, shared FROM mindmaps WHERE id = ?1", params![map], |r| {
            Ok((r.get(0)?, r.get(1)?, r.get(2)?))
        })
        .optional()?;
    row.ok_or_else(|| anyhow::anyhow!("no mind map #{map} in this project"))
}

/// Refuse anyone but the map's owner — making a map shared is the owner's
/// call (journal#107).
pub fn check_owner(conn: &Connection, map: i64, who: &Owner) -> Result<()> {
    let (nick, host, _) = owner_row(conn, map)?;
    if !same_owner(&nick, host.as_deref(), who) {
        bail!("mind map #{map} belongs to session `{nick}` — only its owner can do that");
    }
    Ok(())
}

/// Refuse a write to a private map by anyone but its owner (journal#94); a
/// shared map is everyone's in the project, delete and unshare included
/// (journal#107).
pub fn check_write(conn: &Connection, map: i64, who: &Owner) -> Result<()> {
    let (nick, host, shared) = owner_row(conn, map)?;
    if !shared && !same_owner(&nick, host.as_deref(), who) {
        bail!(
            "mind map #{map} belongs to session `{nick}` and is not shared — another session may read it \
             (`mindmap show {map}`), only its owner writes or shares it"
        );
    }
    Ok(())
}

pub fn set_shared(conn: &Connection, map: i64, shared: bool, now: i64) -> Result<()> {
    let n = conn.execute("UPDATE mindmaps SET shared = ?2, updated_at = ?3 WHERE id = ?1", params![map, shared, now])?;
    if n == 0 {
        bail!("no mind map #{map} in this project");
    }
    Ok(())
}

fn layout_of(conn: &Connection, map: i64) -> Result<String> {
    conn.query_row("SELECT layout FROM mindmaps WHERE id = ?1", params![map], |r| r.get(0))
        .optional()?
        .ok_or_else(|| anyhow::anyhow!("no mind map #{map} in this project"))
}

fn refs_of(conn: &Connection, sql: &str, id: i64) -> Result<Vec<i64>> {
    let mut stmt = conn.prepare_cached(sql)?;
    let rows = stmt.query_map(params![id], |r| r.get(0))?;
    Ok(rows.collect::<rusqlite::Result<_>>()?)
}

pub fn load(conn: &Connection, map: i64) -> Result<Option<Map>> {
    let Some(summary) = summary(conn, map)? else { return Ok(None) };
    let mut raw: Vec<(Node, i64)> = {
        let mut stmt =
            conn.prepare("SELECT id, parent_id, kind, text, state, ord, author FROM mindmap_nodes WHERE map_id = ?1 ORDER BY ord, id")?;
        let rows = stmt.query_map(params![map], |r| {
            Ok((
                Node {
                    id: r.get(0)?,
                    parent: r.get(1)?,
                    kind: r.get(2)?,
                    text: r.get(3)?,
                    state: r.get(4)?,
                    refs: vec![],
                    author: r.get(6)?,
                },
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

/// A node to add. `parent` places it in a mind map's tree; `after` (a
/// flowchart's) draws the arrow from that node to the new one.
#[derive(Debug, Clone, Default)]
pub struct NewNode<'a> {
    pub parent: Option<i64>,
    pub after: Option<i64>,
    /// The label of the `after` arrow ("yes", "no").
    pub label: &'a str,
    pub kind: &'a str,
    pub text: &'a str,
    pub refs: &'a [i64],
    pub author: Option<&'a str>,
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
    add(conn, map, &NewNode { parent, kind, text, refs, ..Default::default() }, now)
}

pub fn add(conn: &mut Connection, map: i64, spec: &NewNode<'_>, now: i64) -> Result<i64> {
    let layout = layout_of(conn, map)?;
    // No kind given: the plainest one of the map's layout.
    let kind = match spec.kind.trim() {
        "" if layout == "flowchart" => "process".to_string(),
        "" => "note".to_string(),
        k => normalize_kind(k, &layout)?,
    };
    if kind == "topic" {
        bail!("`topic` is the root's kind; pick another for a node under it");
    }
    let text = clean_text(spec.text, MAX_TEXT_CHARS, "the node text")?;
    let refs = spec.refs;
    let flow = layout == "flowchart";
    if flow && spec.parent.is_some() {
        bail!("a flowchart is not a tree — place the node with --after n<id> (draws the arrow) or link it");
    }
    if !flow && spec.after.is_some() {
        bail!("--after draws a flowchart arrow — in a mind map, place the node with --parent n<id>");
    }
    let t = tx(conn)?;
    is_open(&t, map)?;
    let parent = match spec.parent {
        Some(p) => {
            node_in(&t, map, p)?;
            p
        }
        None => root_of(&t, map)?,
    };
    if let Some(a) = spec.after {
        node_in(&t, map, a)?;
    }
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
        "INSERT INTO mindmap_nodes(map_id, parent_id, kind, text, ord, created_at, author) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
        params![map, parent, kind, text, ord, now, spec.author],
    )?;
    let id = t.last_insert_rowid();
    insert_refs(&t, Target::Node(id), refs)?;
    if let Some(a) = spec.after {
        t.execute(
            "INSERT INTO mindmap_edges(map_id, from_node, to_node, label) VALUES (?1, ?2, ?3, ?4)",
            params![map, a, id, spec.label.trim()],
        )?;
    }
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
        let k = if node == root { k.trim().to_string() } else { normalize_kind(k, &layout_of(&t, map)?)? };
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
    if layout_of(&t, map)? == "flowchart" {
        bail!("a flowchart is not a tree — change its arrows with link / unlink");
    }
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
        let (shared, layout, direction): (bool, String, String) = src.query_row(
            "SELECT shared, layout, direction FROM mindmaps WHERE id = ?1",
            params![old],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )?;
        t.execute(
            "INSERT INTO mindmaps(title, owner_nick, owner_host, state, view_token, created_at, updated_at, shared, layout, direction) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
            params![title, nick, host, state, view_token(), created, updated, shared, layout, direction],
        )?;
        let map = t.last_insert_rowid();
        let map_of = |e: &i64| entries.get(e).copied();
        let mut nodes: HashMap<i64, i64> = HashMap::new();
        {
            // Parents first: walk by depth.
            let mut stmt =
                src.prepare("SELECT id, parent_id, kind, text, state, ord, created_at, author FROM mindmap_nodes WHERE map_id = ?1")?;
            let rows = stmt.query_map(params![old], |r| {
                Ok((
                    r.get::<_, i64>(0)?,
                    r.get::<_, Option<i64>>(1)?,
                    r.get::<_, String>(2)?,
                    r.get::<_, String>(3)?,
                    r.get::<_, String>(4)?,
                    r.get::<_, i64>(5)?,
                    r.get::<_, i64>(6)?,
                    r.get::<_, Option<String>>(7)?,
                ))
            })?;
            let mut pending: Vec<_> = rows.collect::<rusqlite::Result<_>>()?;
            while !pending.is_empty() {
                let before = pending.len();
                pending.retain(|(id, parent, kind, text, st, ord, created, author)| {
                    let new_parent = match parent {
                        None => None,
                        Some(p) => match nodes.get(p) {
                            Some(np) => Some(*np),
                            None => return true,
                        },
                    };
                    let ok = t.execute(
                        "INSERT INTO mindmap_nodes(map_id, parent_id, kind, text, state, ord, created_at, author) \
                         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
                        params![map, new_parent, kind, text, st, ord, created, author],
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
    let flow = s.layout == "flowchart";
    let mut out = format!(
        "{} #{} \"{}\" — {}{}, owner {}, {} nodes ({} open)\n",
        if flow { "flowchart" } else { "mind map" },
        s.id,
        s.title,
        s.state,
        if s.shared { ", shared with every session of the project" } else { "" },
        s.owner_nick,
        s.nodes.saturating_sub(1),
        s.open_nodes
    );
    let mut depth: HashMap<i64, usize> = HashMap::new();
    for n in &map.nodes {
        if flow && n.id == map.root {
            continue;
        }
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
        let by = match &n.author {
            Some(a) if *a != s.owner_nick => format!("  (by {a})"),
            _ => String::new(),
        };
        let indent = if flow { String::new() } else { "  ".repeat(d) };
        out.push_str(&format!("{indent}- n{} ({}) {text}{mark}{refs}{by}\n", n.id, n.kind));
    }
    if !map.edges.is_empty() {
        out.push_str(if flow { "arrows:\n" } else { "links:\n" });
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
    fn a_shared_map_is_everyones_a_private_one_its_owners() {
        let (_d, mut c) = journal();
        let atlas = Owner { nick: "atlas".into(), host: Some("box".into()) };
        let spec = NewMap { title: "architecture", layout: "mindmap", direction: "down", shared: true };
        let (shared, _) = create_map(&mut c, &spec, &mira(), 1).unwrap();
        let (private, _) = create(&mut c, "mira's research", &mira(), 2).unwrap();
        // Anyone writes to the shared one, nobody else to the private one.
        check_write(&c, shared, &atlas).unwrap();
        assert!(check_write(&c, private, &atlas).unwrap_err().to_string().contains("not shared"));
        let n = add(&mut c, shared, &NewNode { kind: "fact", text: "the daemon owns the journals", author: Some("atlas"), ..Default::default() }, 3).unwrap();
        assert_eq!(load(&c, shared).unwrap().unwrap().node(n).unwrap().author.as_deref(), Some("atlas"));
        assert!(outline(&load(&c, shared).unwrap().unwrap()).contains("(by atlas)"));
        // Sharing is the owner's call; unsharing a shared map is anyone's.
        assert!(check_owner(&c, private, &atlas).is_err());
        check_write(&c, shared, &atlas).unwrap();
        set_shared(&c, shared, false, 4).unwrap();
        assert!(check_write(&c, shared, &atlas).is_err(), "private again");
        set_shared(&c, shared, true, 5).unwrap();
        // A session lists its own maps and the shared ones.
        let seen: Vec<i64> = list(&c, Some(&atlas), false).unwrap().iter().map(|m| m.id).collect();
        assert_eq!(seen, vec![shared]);
        let mine: Vec<i64> = list(&c, Some(&mira()), false).unwrap().iter().map(|m| m.id).collect();
        assert_eq!(mine.len(), 2);
    }

    #[test]
    fn a_flowchart_takes_symbols_and_gates_and_arrows_not_a_tree() {
        let (_d, mut c) = journal();
        let e = entry(&mut c, "the checkout spec, rev 3");
        let spec = NewMap { title: "checkout", layout: "flow", direction: "right", shared: false };
        let (f, _) = create_map(&mut c, &spec, &mira(), 1).unwrap();
        let s = summary(&c, f).unwrap().unwrap();
        assert_eq!((s.layout.as_str(), s.direction.as_str()), ("flowchart", "right"));
        let start = add(&mut c, f, &NewNode { kind: "terminator", text: "start", ..Default::default() }, 2).unwrap();
        let q = add(&mut c, f, &NewNode { kind: "decision", text: "paid?", after: Some(start), refs: &[e], ..Default::default() }, 3).unwrap();
        let gate = add(&mut c, f, &NewNode { kind: "AND", text: "paid and in stock", after: Some(q), label: "yes", ..Default::default() }, 4).unwrap();
        let plain = add(&mut c, f, &NewNode { text: "ship", after: Some(gate), ..Default::default() }, 5).unwrap();
        assert!(add(&mut c, f, &NewNode { kind: "fact", text: "x", ..Default::default() }, 6).is_err(), "mind-map kind in a flowchart");
        assert!(add(&mut c, f, &NewNode { kind: "process", text: "x", parent: Some(q), ..Default::default() }, 6).is_err(), "no tree");
        assert!(move_node(&mut c, f, plain, q, 6).is_err());
        let l = add_edge(&mut c, f, q, plain, "no", &[e], 7).unwrap();
        let map = load(&c, f).unwrap().unwrap();
        assert_eq!(map.node(gate).unwrap().kind, "and");
        assert_eq!(map.node(plain).unwrap().kind, "process", "a flowchart's default kind");
        assert_eq!(map.node(q).unwrap().refs, vec![e]);
        let arrows: Vec<(i64, i64, &str)> = map.edges.iter().map(|x| (x.from, x.to, x.label.as_str())).collect();
        assert!(arrows.contains(&(q, gate, "yes")) && arrows.contains(&(start, q, "")));
        assert_eq!(map.edges.iter().find(|x| x.id == l).unwrap().refs, vec![e]);
        assert!(outline(&map).starts_with("flowchart #"));
        // And the other way: no gates, no --after in a mind map.
        let (m, _) = create(&mut c, "notes", &mira(), 8).unwrap();
        assert!(add(&mut c, m, &NewNode { kind: "xor", text: "x", ..Default::default() }, 9).is_err());
        assert!(add(&mut c, m, &NewNode { kind: "fact", text: "x", after: Some(start), ..Default::default() }, 9).is_err());
        assert!(create_map(&mut c, &NewMap { title: "t", layout: "gantt", direction: "down", shared: false }, &mira(), 9).is_err());
    }

    #[test]
    fn a_journal_from_v0_23_gets_the_new_columns() {
        // The mind-map tables as v0.23.1 created them, with a map in them.
        let dir = tempfile::tempdir().unwrap();
        let project = Path::new("/work/old");
        let path = journal::journal_path(dir.path(), project);
        {
            let old = Connection::open(&path).unwrap();
            old.execute_batch(
                "CREATE TABLE mindmaps (id INTEGER PRIMARY KEY, title TEXT NOT NULL, owner_nick TEXT NOT NULL, owner_host TEXT, \
                 state TEXT NOT NULL DEFAULT 'open', view_token TEXT NOT NULL, created_at INTEGER NOT NULL, updated_at INTEGER NOT NULL);
                 CREATE TABLE mindmap_nodes (id INTEGER PRIMARY KEY, map_id INTEGER NOT NULL REFERENCES mindmaps(id) ON DELETE CASCADE, \
                 parent_id INTEGER REFERENCES mindmap_nodes(id) ON DELETE CASCADE, kind TEXT NOT NULL, text TEXT NOT NULL, \
                 state TEXT NOT NULL DEFAULT 'open', ord INTEGER NOT NULL, created_at INTEGER NOT NULL);
                 INSERT INTO mindmaps VALUES (1, 'old map', 'mira', 'box', 'open', 'tok', 1, 1);
                 INSERT INTO mindmap_nodes VALUES (1, 1, NULL, 'topic', 'old map', 'open', 0, 1);
                 INSERT INTO mindmap_nodes VALUES (2, 1, 1, 'fact', 'kept', 'open', 0, 1);",
            )
            .unwrap();
        }
        let c = journal::open(dir.path(), project, false).unwrap().expect("the old file opens");
        let map = load(&c, 1).unwrap().unwrap();
        assert!(!map.summary.shared);
        assert_eq!(map.summary.layout, "mindmap");
        assert_eq!(map.node(2).unwrap().author, None);
        // Opening again is a no-op.
        drop(c);
        journal::open(dir.path(), project, false).unwrap().unwrap();
    }

    #[test]
    fn merge_carries_maps_with_renumbered_entries() {
        let (_d, mut src) = journal();
        let e1 = entry(&mut src, "first finding about the cache");
        let e2 = entry(&mut src, "second finding about the cache");
        let (m, _) = create(&mut src, "cache", &mira(), 1).unwrap();
        let spec = NewMap { title: "flow", layout: "flowchart", direction: "right", shared: true };
        let (f, _) = create_map(&mut src, &spec, &mira(), 1).unwrap();
        add(&mut src, f, &NewNode { kind: "or", text: "either", author: Some("atlas"), ..Default::default() }, 2).unwrap();
        let a = add_node(&mut src, m, None, "fact", "a", &[e1], 2).unwrap();
        let b = add_node(&mut src, m, Some(a), "idea", "b", &[], 3).unwrap();
        add_edge(&mut src, m, b, a, "refines", &[e2], 4).unwrap();
        add_chain(&mut src, m, "story", &[e1, e2], 5).unwrap();

        let dir = tempfile::tempdir().unwrap();
        let mut dst = journal::open(dir.path(), Path::new("/work/other"), true).unwrap().unwrap();
        let pad = entry(&mut dst, "an entry the destination already had");
        journal::merge_into(&mut dst, &src).unwrap();
        let maps = list(&dst, None, true).unwrap();
        assert_eq!(maps.len(), 2);
        let flow = maps.iter().find(|m| m.layout == "flowchart").expect("the flowchart came along");
        assert!(flow.shared && flow.direction == "right");
        let fm = load(&dst, flow.id).unwrap().unwrap();
        assert!(fm.nodes.iter().any(|n| n.kind == "or" && n.author.as_deref() == Some("atlas")));
        let map = load(&dst, maps.iter().find(|m| m.layout == "mindmap").unwrap().id).unwrap().unwrap();
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
