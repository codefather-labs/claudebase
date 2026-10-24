//! The project journal: what agents decided and found, kept per project so the
//! next session can look it up by category and tag instead of starting cold.
//!
//! ## Ownership
//!
//! The daemon owns every journal. `claudebase journal …` never opens these
//! files itself: it asks the daemon, which works out which project the caller
//! belongs to and is the only writer. That is what makes "the journals of every
//! project" a single place something can later orchestrate from, and it is why
//! nothing is written into the project directory.
//!
//! One SQLite file per project under `~/.claude/claudebase/journals/`, named
//! `<folder name>-<12 hex of sha256(project path)>.db`: readable in a listing,
//! unique per path. The project path itself is recorded inside the file
//! (`journal_meta.project_path`), so the daemon can enumerate projects without
//! a separate index that could drift from the files.
//!
//! ## What a project is
//!
//! The directory `claudebase run` was started in — the `cwd` the supervisor
//! registers with the daemon. A command run from `src/` inside that session
//! still lands in the same journal. Resolution lives in the daemon
//! (`daemon::journal_tools`); this module only takes a path.
//!
//! ## Search
//!
//! BM25 over `entries_fts` plus, when the e5 model is on disk, K-NN over
//! `entries_vec`, fused by RRF (k=60, as everywhere else in claudebase).
//! Metadata filters are SQL on the lexical leg; the vec0 K-NN cannot take a
//! WHERE clause, so the dense leg over-fetches and filters by id.
//!
//! SQL discipline as in `store.rs`: every value is a bound parameter. The only
//! formatted SQL is the count of `?` placeholders for an IN list.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use rusqlite::{params, params_from_iter, Connection, OptionalExtension};
use serde::Serialize;
use sha2::{Digest, Sha256};

/// Salience → retention. `high` is kept forever.
pub const MEDIUM_TTL_SECS: i64 = 365 * 86_400;
pub const LOW_TTL_SECS: i64 = 90 * 86_400;
/// The same body from the same project within this window is not stored twice.
pub const DEDUP_WINDOW_SECS: i64 = 30 * 86_400;
/// Longest accepted entry. A journal entry is a note, not a document.
pub const MAX_BODY_CHARS: usize = 20_000;
pub const MAX_TOP_K: usize = 50;
pub const MAX_PAGE_SIZE: usize = 100;
const RRF_K: f64 = 60.0;
const SNIPPET_CHARS: usize = 200;

const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS journal_meta (
    key   TEXT PRIMARY KEY,
    value TEXT NOT NULL
);
CREATE TABLE IF NOT EXISTS entries (
    id              INTEGER PRIMARY KEY,
    created_at      INTEGER NOT NULL,
    category        TEXT NOT NULL,
    body            TEXT NOT NULL,
    sha256          TEXT NOT NULL,
    salience        TEXT NOT NULL DEFAULT 'medium',
    agent           TEXT,
    agent_id        TEXT,
    session_id      TEXT,
    feature         TEXT,
    source_artifact TEXT
);
CREATE INDEX IF NOT EXISTS entries_created  ON entries(created_at);
CREATE INDEX IF NOT EXISTS entries_category ON entries(category);
CREATE INDEX IF NOT EXISTS entries_sha      ON entries(sha256);
CREATE TABLE IF NOT EXISTS entry_tags (
    entry_id INTEGER NOT NULL REFERENCES entries(id) ON DELETE CASCADE,
    tag      TEXT NOT NULL,
    UNIQUE(entry_id, tag)
);
CREATE INDEX IF NOT EXISTS entry_tags_tag ON entry_tags(tag);
CREATE VIRTUAL TABLE IF NOT EXISTS entries_fts USING fts5(
    body, content='entries', content_rowid='id'
);
CREATE TRIGGER IF NOT EXISTS entries_ai AFTER INSERT ON entries BEGIN
    INSERT INTO entries_fts(rowid, body) VALUES (new.id, new.body);
END;
CREATE TRIGGER IF NOT EXISTS entries_ad AFTER DELETE ON entries BEGIN
    INSERT INTO entries_fts(entries_fts, rowid, body) VALUES ('delete', old.id, old.body);
    DELETE FROM entries_vec WHERE rowid = old.id;
END;
CREATE VIRTUAL TABLE IF NOT EXISTS entries_vec USING vec0(embedding float[384]);
";

/// `~/.claude/claudebase/journals`. HOME-rooted, never joined with input.
pub fn journals_dir() -> PathBuf {
    let home = std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .unwrap_or_else(|| std::ffi::OsString::from("/tmp"));
    PathBuf::from(home).join(".claude").join("claudebase").join("journals")
}

/// The file name for a project path. The folder name is only decoration — the
/// hash is what makes it unique — so it is reduced to a safe alphabet.
pub fn journal_file_name(project: &Path) -> String {
    let mut name: String = project
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default()
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() || c == '.' || c == '_' || c == '-' { c } else { '-' })
        .take(40)
        .collect();
    if name.trim_matches(|c| c == '.' || c == '-').is_empty() {
        name = "root".into();
    }
    let digest = Sha256::digest(project.to_string_lossy().as_bytes());
    format!("{name}-{}.db", hex(&digest)[..12].to_owned())
}

pub fn journal_path(dir: &Path, project: &Path) -> PathBuf {
    dir.join(journal_file_name(project))
}

/// Open a project's journal. `create = false` returns `None` when it has never
/// been written, so a read from an unrelated directory does not leave an empty
/// file behind.
pub fn open(dir: &Path, project: &Path, create: bool) -> Result<Option<Connection>> {
    let path = journal_path(dir, project);
    if !create && !path.exists() {
        return Ok(None);
    }
    create_private_dir(dir)?;
    let conn = open_file(&path)?;
    let recorded: Option<String> = conn
        .query_row("SELECT value FROM journal_meta WHERE key = 'project_path'", [], |r| r.get(0))
        .optional()?;
    match recorded {
        None => {
            conn.execute(
                "INSERT INTO journal_meta(key, value) VALUES ('project_path', ?1)",
                params![project.to_string_lossy()],
            )?;
            conn.execute(
                "INSERT OR IGNORE INTO journal_meta(key, value) VALUES ('created_at', ?1)",
                params![now_secs().to_string()],
            )?;
        }
        // Twelve hex digits make a collision a curiosity, but a journal that
        // silently served another project would be exactly the leak this
        // design exists to prevent.
        Some(p) if p != project.to_string_lossy() => {
            bail!("journal {} belongs to {p}, not {}", path.display(), project.display())
        }
        Some(_) => {}
    }
    Ok(Some(conn))
}

fn open_file(path: &Path) -> Result<Connection> {
    crate::store::ensure_sqlite_vec_registered();
    let fresh = !path.exists();
    let conn = Connection::open(path).with_context(|| format!("open {}", path.display()))?;
    if fresh {
        restrict(path);
    }
    conn.busy_timeout(std::time::Duration::from_secs(5))?;
    conn.execute_batch("PRAGMA journal_mode = WAL; PRAGMA foreign_keys = ON;")?;
    conn.execute_batch(SCHEMA).context("apply journal schema")?;
    Ok(conn)
}

fn create_private_dir(dir: &Path) -> Result<()> {
    std::fs::create_dir_all(dir).with_context(|| format!("create {}", dir.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700));
    }
    Ok(())
}

/// Journals hold whatever agents wrote down, which is not for other users.
fn restrict(_path: &Path) {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(_path, std::fs::Permissions::from_mode(0o600));
    }
}

/// Tags and categories share one normal form: a leading `#` dropped, trimmed,
/// lowercased, inner whitespace turned into `-`. Empty means "none".
pub fn normalize_label(raw: &str) -> Option<String> {
    let s = raw.trim();
    let s = s.strip_prefix('#').unwrap_or(s).trim().to_lowercase();
    let s: String = s.split_whitespace().collect::<Vec<_>>().join("-");
    (!s.is_empty()).then_some(s)
}

/// Normalize, drop empties, de-duplicate, keep first-seen order.
pub fn normalize_tags<S: AsRef<str>>(raw: &[S]) -> Vec<String> {
    let mut seen = HashSet::new();
    raw.iter()
        .filter_map(|t| normalize_label(t.as_ref()))
        .filter(|t| seen.insert(t.clone()))
        .collect()
}

pub fn normalize_salience(raw: Option<&str>) -> Result<String> {
    match raw.map(|s| s.trim().to_lowercase()).as_deref() {
        None | Some("") => Ok("medium".into()),
        Some(s @ ("high" | "medium" | "low")) => Ok(s.into()),
        Some(other) => bail!("salience must be high, medium or low, not `{other}`"),
    }
}

#[derive(Debug, Clone, Default)]
pub struct NewEntry {
    pub body: String,
    pub category: String,
    pub tags: Vec<String>,
    pub salience: String,
    pub agent: Option<String>,
    pub agent_id: Option<String>,
    pub session_id: Option<String>,
    pub feature: Option<String>,
    pub source_artifact: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum AddStatus {
    Written,
    Deduped,
}

/// Validate and store one entry. The caller has resolved the project; this
/// checks the entry itself.
pub fn add(conn: &mut Connection, entry: &NewEntry, now: i64) -> Result<(AddStatus, i64)> {
    let body = entry.body.trim();
    if body.is_empty() {
        bail!("the entry is empty");
    }
    if body.chars().count() > MAX_BODY_CHARS {
        bail!("the entry is longer than {MAX_BODY_CHARS} characters; a journal entry is a note — link the document instead (--source-artifact)");
    }
    let Some(category) = normalize_label(&entry.category) else {
        bail!("a category is required (--category), e.g. decision, finding, bug");
    };
    let tags = normalize_tags(&entry.tags);
    if tags.is_empty() {
        bail!("at least one --tag is required: tags are how the entry is found again");
    }
    let salience = normalize_salience(Some(&entry.salience))?;
    let sha = hex(&Sha256::digest(body.as_bytes()));

    let existing: Option<i64> = conn
        .query_row(
            "SELECT id FROM entries WHERE sha256 = ?1 AND category = ?2 AND created_at >= ?3 \
             ORDER BY id DESC LIMIT 1",
            params![sha, category, now - DEDUP_WINDOW_SECS],
            |r| r.get(0),
        )
        .optional()?;
    if let Some(id) = existing {
        // The same note again still teaches us its tags.
        let tx = conn.transaction()?;
        insert_tags(&tx, id, &tags)?;
        tx.commit()?;
        return Ok((AddStatus::Deduped, id));
    }

    let tx = conn.transaction()?;
    tx.execute(
        "INSERT INTO entries (created_at, category, body, sha256, salience, agent, agent_id, \
                              session_id, feature, source_artifact) \
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
        params![
            now,
            category,
            body,
            sha,
            salience,
            clean(&entry.agent),
            clean(&entry.agent_id),
            clean(&entry.session_id),
            clean(&entry.feature),
            clean(&entry.source_artifact),
        ],
    )?;
    let id = tx.last_insert_rowid();
    insert_tags(&tx, id, &tags)?;
    tx.commit()?;
    Ok((AddStatus::Written, id))
}

fn clean(v: &Option<String>) -> Option<&str> {
    v.as_deref().map(str::trim).filter(|s| !s.is_empty())
}

fn insert_tags(conn: &Connection, id: i64, tags: &[String]) -> Result<()> {
    let mut stmt = conn.prepare_cached("INSERT OR IGNORE INTO entry_tags(entry_id, tag) VALUES (?1, ?2)")?;
    for tag in tags {
        stmt.execute(params![id, tag])?;
    }
    Ok(())
}

#[derive(Debug, Clone, Default)]
pub struct Filter {
    pub category: Option<String>,
    /// Any of these (OR): `--tag nginx --tag docker` finds either.
    pub tags: Vec<String>,
    pub agent: Option<String>,
    pub salience: Option<String>,
    pub feature: Option<String>,
    /// Only entries created at or after this unix second.
    pub since: Option<i64>,
}

impl Filter {
    /// The filter as SQL over `entries e`, with its parameters.
    fn sql(&self) -> (String, Vec<rusqlite::types::Value>) {
        use rusqlite::types::Value;
        let mut clauses = Vec::new();
        let mut values = Vec::new();
        let eq = |col: &str, v: &Option<String>, clauses: &mut Vec<String>, values: &mut Vec<Value>| {
            if let Some(v) = v {
                clauses.push(format!("e.{col} = ?"));
                values.push(Value::Text(v.clone()));
            }
        };
        eq("category", &self.category.as_deref().and_then(normalize_label), &mut clauses, &mut values);
        eq("agent", &self.agent, &mut clauses, &mut values);
        eq("salience", &self.salience, &mut clauses, &mut values);
        eq("feature", &self.feature, &mut clauses, &mut values);
        if let Some(since) = self.since {
            clauses.push("e.created_at >= ?".into());
            values.push(Value::Integer(since));
        }
        let tags = normalize_tags(&self.tags);
        if !tags.is_empty() {
            clauses.push(format!(
                "e.id IN (SELECT entry_id FROM entry_tags WHERE tag IN ({}))",
                vec!["?"; tags.len()].join(",")
            ));
            values.extend(tags.into_iter().map(Value::Text));
        }
        if clauses.is_empty() {
            ("1".into(), values)
        } else {
            (clauses.join(" AND "), values)
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct Entry {
    pub id: i64,
    /// When the entry was written, unix seconds; for an imported insight,
    /// when the insight was.
    pub created_at: i64,
    /// `created_at` as RFC 3339 UTC, for readers that should not convert.
    pub created: String,
    pub category: String,
    pub tags: Vec<String>,
    pub salience: String,
    pub body: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub agent: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub agent_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub feature: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source_artifact: Option<String>,
}

const ENTRY_COLUMNS: &str = "e.id, e.created_at, e.category, e.salience, e.body, e.agent, \
                             e.agent_id, e.session_id, e.feature, e.source_artifact";

fn entry_from_row(r: &rusqlite::Row<'_>) -> rusqlite::Result<Entry> {
    Ok(Entry {
        id: r.get(0)?,
        created_at: r.get(1)?,
        created: rfc3339(r.get(1)?),
        category: r.get(2)?,
        salience: r.get(3)?,
        body: r.get(4)?,
        agent: r.get(5)?,
        agent_id: r.get(6)?,
        session_id: r.get(7)?,
        feature: r.get(8)?,
        source_artifact: r.get(9)?,
        tags: Vec::new(),
    })
}

pub fn rfc3339(secs: i64) -> String {
    chrono::DateTime::<chrono::Utc>::from_timestamp(secs, 0)
        .map(|d| d.format("%Y-%m-%dT%H:%M:%SZ").to_string())
        .unwrap_or_default()
}

fn attach_tags(conn: &Connection, entries: &mut [Entry]) -> Result<()> {
    let mut stmt = conn.prepare_cached("SELECT tag FROM entry_tags WHERE entry_id = ?1 ORDER BY rowid")?;
    for e in entries.iter_mut() {
        e.tags = stmt.query_map(params![e.id], |r| r.get(0))?.collect::<rusqlite::Result<_>>()?;
    }
    Ok(())
}

pub fn get(conn: &Connection, id: i64) -> Result<Option<Entry>> {
    let mut entry = conn
        .query_row(&format!("SELECT {ENTRY_COLUMNS} FROM entries e WHERE e.id = ?1"), params![id], entry_from_row)
        .optional()?;
    if let Some(e) = entry.as_mut() {
        attach_tags(conn, std::slice::from_mut(e))?;
    }
    Ok(entry)
}

/// Newest first. Returns the number of matching entries and one page of them.
pub fn list(conn: &Connection, filter: &Filter, offset: usize, limit: usize) -> Result<(i64, Vec<Entry>)> {
    let (where_sql, values) = filter.sql();
    let total: i64 = conn.query_row(
        &format!("SELECT COUNT(*) FROM entries e WHERE {where_sql}"),
        params_from_iter(values.iter()),
        |r| r.get(0),
    )?;
    let mut page_values = values;
    page_values.push(rusqlite::types::Value::Integer(limit.clamp(1, MAX_PAGE_SIZE) as i64));
    page_values.push(rusqlite::types::Value::Integer(offset as i64));
    let mut stmt = conn.prepare(&format!(
        "SELECT {ENTRY_COLUMNS} FROM entries e WHERE {where_sql} ORDER BY e.created_at DESC, e.id DESC LIMIT ? OFFSET ?"
    ))?;
    let mut entries: Vec<Entry> =
        stmt.query_map(params_from_iter(page_values.iter()), entry_from_row)?.collect::<rusqlite::Result<_>>()?;
    attach_tags(conn, &mut entries)?;
    Ok((total, entries))
}

pub fn delete(conn: &Connection, id: i64) -> Result<bool> {
    Ok(conn.execute("DELETE FROM entries WHERE id = ?1", params![id])? > 0)
}

#[derive(Debug, Clone, Serialize)]
pub struct LabelCount {
    pub name: String,
    pub count: i64,
}

/// Tag frequencies, most used first; optionally only within one category.
pub fn tags(conn: &Connection, category: Option<&str>) -> Result<Vec<LabelCount>> {
    let category = category.and_then(normalize_label);
    let mut stmt = conn.prepare(
        "SELECT t.tag, COUNT(*) AS n FROM entry_tags t JOIN entries e ON e.id = t.entry_id \
         WHERE ?1 IS NULL OR e.category = ?1 GROUP BY t.tag ORDER BY n DESC, t.tag",
    )?;
    let rows = stmt.query_map(params![category], |r| Ok(LabelCount { name: r.get(0)?, count: r.get(1)? }))?;
    Ok(rows.collect::<rusqlite::Result<_>>()?)
}

pub fn categories(conn: &Connection) -> Result<Vec<LabelCount>> {
    let mut stmt = conn.prepare(
        "SELECT category, COUNT(*) AS n FROM entries GROUP BY category ORDER BY n DESC, category",
    )?;
    let rows = stmt.query_map([], |r| Ok(LabelCount { name: r.get(0)?, count: r.get(1)? }))?;
    Ok(rows.collect::<rusqlite::Result<_>>()?)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum SearchMode {
    Lexical,
    Dense,
    Hybrid,
}

impl std::str::FromStr for SearchMode {
    type Err = anyhow::Error;
    fn from_str(s: &str) -> Result<Self> {
        match s {
            "lexical" => Ok(Self::Lexical),
            "dense" => Ok(Self::Dense),
            "hybrid" => Ok(Self::Hybrid),
            other => bail!("mode must be lexical, dense or hybrid, not `{other}`"),
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct Hit {
    pub score: f64,
    pub snippet: String,
    #[serde(flatten)]
    pub entry: Entry,
}

/// An FTS5 query from free text: every word quoted, ORed. Quoting is what
/// keeps `codefather.dev` or `v0.13.0` from being parsed as FTS5 syntax
/// (issue 001); OR keeps a long question from matching nothing.
pub fn fts_query(text: &str) -> Option<String> {
    let words: Vec<String> = text
        .split(|c: char| !c.is_alphanumeric())
        .filter(|w| !w.is_empty())
        .map(|w| format!("\"{w}\""))
        .collect();
    (!words.is_empty()).then(|| words.join(" OR "))
}

/// Ranked entries for `query`. `embedding` is the query vector when the
/// caller could encode it; without one, dense and hybrid quietly become
/// lexical and the returned mode says so.
pub fn search(
    conn: &Connection,
    query: &str,
    embedding: Option<&[f32]>,
    mode: SearchMode,
    filter: &Filter,
    top_k: usize,
) -> Result<(SearchMode, Vec<Hit>)> {
    let top_k = top_k.clamp(1, MAX_TOP_K);
    let candidates = (top_k * 4).min(200);
    let effective = match (mode, embedding) {
        (SearchMode::Lexical, _) | (_, None) => SearchMode::Lexical,
        (m, Some(_)) => m,
    };
    let lexical = if effective == SearchMode::Dense { Vec::new() } else { lexical_ids(conn, query, filter, candidates)? };
    let dense = match (effective, embedding) {
        (SearchMode::Lexical, _) | (_, None) => Vec::new(),
        (_, Some(e)) => dense_ids(conn, e, filter, candidates)?,
    };
    let ranked: Vec<(i64, f64)> = match effective {
        SearchMode::Lexical => lexical,
        SearchMode::Dense => dense,
        SearchMode::Hybrid => rrf(&lexical, &dense),
    };
    let mut hits = Vec::new();
    for (id, score) in ranked.into_iter().take(top_k) {
        if let Some(entry) = get(conn, id)? {
            hits.push(Hit { score, snippet: snippet(&entry.body, query), entry });
        }
    }
    Ok((effective, hits))
}

fn lexical_ids(conn: &Connection, query: &str, filter: &Filter, k: usize) -> Result<Vec<(i64, f64)>> {
    let Some(fts) = fts_query(query) else { return Ok(Vec::new()) };
    let (where_sql, mut values) = filter.sql();
    values.insert(0, rusqlite::types::Value::Text(fts));
    values.push(rusqlite::types::Value::Integer(k as i64));
    let mut stmt = conn.prepare(&format!(
        "SELECT e.id, -bm25(entries_fts) AS score FROM entries_fts JOIN entries e ON e.id = entries_fts.rowid \
         WHERE entries_fts MATCH ? AND {where_sql} ORDER BY score DESC LIMIT ?"
    ))?;
    let rows = stmt.query_map(params_from_iter(values.iter()), |r| Ok((r.get(0)?, r.get(1)?)))?;
    Ok(rows.collect::<rusqlite::Result<_>>()?)
}

fn dense_ids(conn: &Connection, embedding: &[f32], filter: &Filter, k: usize) -> Result<Vec<(i64, f64)>> {
    let bytes: Vec<u8> = embedding.iter().flat_map(|f| f.to_le_bytes()).collect();
    let mut stmt = conn.prepare("SELECT rowid, distance FROM entries_vec WHERE embedding MATCH ?1 AND k = ?2 ORDER BY distance")?;
    let near: Vec<(i64, f64)> = stmt
        .query_map(params![bytes, k as i64], |r| Ok((r.get::<_, i64>(0)?, -r.get::<_, f64>(1)?)))?
        .collect::<rusqlite::Result<_>>()?;
    if near.is_empty() {
        return Ok(near);
    }
    // vec0 cannot filter, so keep only ids the filter admits.
    let (where_sql, mut values) = filter.sql();
    values.extend(near.iter().map(|(id, _)| rusqlite::types::Value::Integer(*id)));
    let mut stmt = conn.prepare(&format!(
        "SELECT e.id FROM entries e WHERE {where_sql} AND e.id IN ({})",
        vec!["?"; near.len()].join(",")
    ))?;
    let allowed: HashSet<i64> =
        stmt.query_map(params_from_iter(values.iter()), |r| r.get(0))?.collect::<rusqlite::Result<_>>()?;
    Ok(near.into_iter().filter(|(id, _)| allowed.contains(id)).collect())
}

fn rrf(a: &[(i64, f64)], b: &[(i64, f64)]) -> Vec<(i64, f64)> {
    let mut scores: HashMap<i64, f64> = HashMap::new();
    for list in [a, b] {
        for (rank, (id, _)) in list.iter().enumerate() {
            *scores.entry(*id).or_default() += 1.0 / (RRF_K + rank as f64 + 1.0);
        }
    }
    let mut out: Vec<(i64, f64)> = scores.into_iter().collect();
    out.sort_by(|x, y| y.1.partial_cmp(&x.1).unwrap_or(std::cmp::Ordering::Equal).then(y.0.cmp(&x.0)));
    out
}

/// Up to 200 characters around the first query word found, else the start.
fn snippet(body: &str, query: &str) -> String {
    let lower = body.to_lowercase();
    let start = query
        .split(|c: char| !c.is_alphanumeric())
        .filter(|w| w.chars().count() > 2)
        .filter_map(|w| lower.find(&w.to_lowercase()))
        .min()
        .unwrap_or(0);
    // `find` on the lowercased copy gives a byte offset that may not exist in
    // the original when lowercasing changed lengths; fall back to a char count.
    let chars_before = lower[..start].chars().count();
    let from = chars_before.saturating_sub(SNIPPET_CHARS / 4);
    let total = body.chars().count();
    let text: String = body.chars().skip(from).take(SNIPPET_CHARS).collect();
    let text = text.split_whitespace().collect::<Vec<_>>().join(" ");
    format!("{}{text}{}", if from > 0 { "…" } else { "" }, if from + SNIPPET_CHARS < total { "…" } else { "" })
}

/// Entries that have no vector yet, oldest first.
pub fn missing_vectors(conn: &Connection, limit: usize) -> Result<Vec<(i64, String)>> {
    let mut stmt = conn.prepare(
        "SELECT e.id, e.body FROM entries e WHERE NOT EXISTS (SELECT 1 FROM entries_vec v WHERE v.rowid = e.id) \
         ORDER BY e.id LIMIT ?1",
    )?;
    let rows = stmt.query_map(params![limit as i64], |r| Ok((r.get(0)?, r.get(1)?)))?;
    Ok(rows.collect::<rusqlite::Result<_>>()?)
}

pub fn store_vector(conn: &Connection, id: i64, embedding: &[f32]) -> Result<()> {
    let bytes: Vec<u8> = embedding.iter().flat_map(|f| f.to_le_bytes()).collect();
    // vec0 has no upsert; the entry may have been deleted in between, in which
    // case there is nothing to attach the vector to.
    if conn.query_row("SELECT 1 FROM entries WHERE id = ?1", params![id], |_| Ok(())).optional()?.is_none() {
        return Ok(());
    }
    conn.execute("DELETE FROM entries_vec WHERE rowid = ?1", params![id])?;
    conn.execute("INSERT INTO entries_vec(rowid, embedding) VALUES (?1, ?2)", params![id, bytes])?;
    Ok(())
}

#[derive(Debug, Clone, Copy, Default, Serialize)]
pub struct GcReport {
    pub medium: i64,
    pub low: i64,
}

/// Drop entries past their salience's retention.
pub fn gc(conn: &Connection, now: i64, dry_run: bool) -> Result<GcReport> {
    let count = |salience: &str, ttl: i64| -> rusqlite::Result<i64> {
        conn.query_row(
            "SELECT COUNT(*) FROM entries WHERE salience = ?1 AND created_at < ?2",
            params![salience, now - ttl],
            |r| r.get(0),
        )
    };
    let report = GcReport { medium: count("medium", MEDIUM_TTL_SECS)?, low: count("low", LOW_TTL_SECS)? };
    if !dry_run && report.medium + report.low > 0 {
        conn.execute(
            "DELETE FROM entries WHERE (salience = 'medium' AND created_at < ?1) OR (salience = 'low' AND created_at < ?2)",
            params![now - MEDIUM_TTL_SECS, now - LOW_TTL_SECS],
        )?;
    }
    Ok(report)
}

#[derive(Debug, Clone, Serialize)]
pub struct JournalInfo {
    pub project: String,
    pub file: String,
    pub entries: i64,
    pub last_entry_at: Option<i64>,
}

/// Every journal the daemon holds. A file that cannot be read is skipped, not
/// fatal: one damaged journal must not hide the others.
pub fn list_journals(dir: &Path) -> Result<Vec<JournalInfo>> {
    let Ok(read) = std::fs::read_dir(dir) else { return Ok(Vec::new()) };
    let mut out = Vec::new();
    for entry in read.flatten() {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("db") {
            continue;
        }
        let Ok(conn) = open_file(&path) else { continue };
        let project: Option<String> = conn
            .query_row("SELECT value FROM journal_meta WHERE key = 'project_path'", [], |r| r.get(0))
            .optional()
            .ok()
            .flatten();
        let Some(project) = project else { continue };
        let (entries, last): (i64, Option<i64>) = conn
            .query_row("SELECT COUNT(*), MAX(created_at) FROM entries", [], |r| Ok((r.get(0)?, r.get(1)?)))
            .unwrap_or((0, None));
        out.push(JournalInfo {
            project,
            file: path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default(),
            entries,
            last_entry_at: last,
        });
    }
    out.sort_by(|a, b| b.last_entry_at.cmp(&a.last_entry_at).then(a.project.cmp(&b.project)));
    Ok(out)
}

/// Where the pre-journal insights of a project lived.
pub fn legacy_insights_db(project: &Path) -> PathBuf {
    project.join(".claude").join("knowledge").join("insights.db")
}

/// Bring a project's old `insights.db` into its journal, once. The insight
/// `type` becomes the category, its tags and salience carry over, and the
/// original file is left exactly as it was. Returns the number imported, 0 when
/// there was nothing to import or it was imported before.
pub fn import_legacy_insights(conn: &mut Connection, project: &Path) -> Result<usize> {
    let done: Option<String> = conn
        .query_row("SELECT value FROM journal_meta WHERE key = 'insights_imported'", [], |r| r.get(0))
        .optional()?;
    if done.is_some() {
        return Ok(0);
    }
    let legacy = legacy_insights_db(project);
    let imported = if legacy.is_file() { copy_insights(conn, &legacy)? } else { 0 };
    conn.execute(
        "INSERT OR REPLACE INTO journal_meta(key, value) VALUES ('insights_imported', ?1)",
        params![format!("{imported} from {}", legacy.display())],
    )?;
    Ok(imported)
}

fn copy_insights(conn: &mut Connection, legacy: &Path) -> Result<usize> {
    let old = Connection::open_with_flags(legacy, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)
        .with_context(|| format!("open {}", legacy.display()))?;
    let has = |what: &str, name: &str| -> bool {
        old.query_row(
            "SELECT 1 FROM sqlite_master WHERE type = ?1 AND name = ?2",
            params![what, name],
            |_| Ok(()),
        )
        .optional()
        .ok()
        .flatten()
        .is_some()
    };
    let has_column = |col: &str| -> bool {
        old.query_row(
            "SELECT 1 FROM pragma_table_info('documents') WHERE name = ?1",
            params![col],
            |_| Ok(()),
        )
        .optional()
        .ok()
        .flatten()
        .is_some()
    };
    if !has("table", "documents") || !has_column("source_type") {
        return Ok(0);
    }
    let with_tags = has("table", "insight_tags");

    struct Old {
        id: i64,
        at: i64,
        kind: String,
        agent: Option<String>,
        session: Option<String>,
        feature: Option<String>,
        salience: Option<String>,
        artifact: Option<String>,
    }
    let rows: Vec<Old> = {
        let mut stmt = old.prepare(
            "SELECT id, ingested_at, source_type, agent_name, session_id, feature_slug, salience, parent_artifact \
             FROM documents WHERE source_type IS NOT NULL ORDER BY id",
        )?;
        let mapped = stmt.query_map([], |r| {
            Ok(Old {
                id: r.get(0)?,
                at: r.get(1)?,
                kind: r.get(2)?,
                agent: r.get(3)?,
                session: r.get(4)?,
                feature: r.get(5)?,
                salience: r.get(6)?,
                artifact: r.get(7)?,
            })
        })?;
        mapped.collect::<rusqlite::Result<_>>()?
    };
    let mut count = 0;
    for row in rows {
        let chunks: Vec<String> = {
            let mut stmt = old.prepare("SELECT text FROM chunks WHERE doc_id = ?1 ORDER BY ord")?;
            let mapped = stmt.query_map(params![row.id], |r| r.get(0))?;
            mapped.collect::<rusqlite::Result<_>>()?
        };
        let body = stitch_chunks(&chunks);
        let mut tags: Vec<String> = if with_tags {
            let mut stmt = old.prepare("SELECT tag FROM insight_tags WHERE doc_id = ?1 ORDER BY rowid")?;
            let mapped = stmt.query_map(params![row.id], |r| r.get(0))?;
            mapped.collect::<rusqlite::Result<_>>()?
        } else {
            Vec::new()
        };
        if tags.is_empty() {
            tags.push(row.feature.clone().unwrap_or_else(|| "imported".into()));
        }
        let entry = NewEntry {
            body,
            category: row.kind,
            tags,
            salience: normalize_salience(row.salience.as_deref()).unwrap_or_else(|_| "medium".into()),
            agent: row.agent,
            agent_id: None,
            session_id: row.session,
            feature: row.feature,
            source_artifact: row.artifact,
        };
        // An unusable old row (empty, oversized) is skipped rather than
        // aborting the import of everything after it.
        if let Ok((AddStatus::Written, _)) = add(conn, &entry, row.at) {
            count += 1;
        }
    }
    Ok(count)
}

/// Insights were stored as a 500-character window sliding by 400, so every
/// chunk after the first repeats the previous one's last 100 characters.
fn stitch_chunks(chunks: &[String]) -> String {
    const OVERLAP: usize = 100;
    let mut out = String::new();
    for (i, chunk) in chunks.iter().enumerate() {
        if i == 0 {
            out.push_str(chunk);
        } else {
            out.extend(chunk.chars().skip(OVERLAP));
        }
    }
    out
}

pub fn now_secs() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn journal() -> (tempfile::TempDir, Connection) {
        let dir = tempfile::tempdir().unwrap();
        let conn = open(dir.path(), Path::new("/work/proj"), true).unwrap().unwrap();
        (dir, conn)
    }

    fn entry(body: &str, category: &str, tags: &[&str]) -> NewEntry {
        NewEntry {
            body: body.into(),
            category: category.into(),
            tags: tags.iter().map(|t| t.to_string()).collect(),
            salience: "medium".into(),
            ..Default::default()
        }
    }

    #[test]
    fn file_names_are_readable_unique_and_safe() {
        let a = journal_file_name(Path::new("/home/me/my project"));
        let b = journal_file_name(Path::new("/srv/my project"));
        assert!(a.starts_with("my-project-") && a.ends_with(".db"), "{a}");
        assert_ne!(a, b);
        assert!(journal_file_name(Path::new("/")).starts_with("root-"));
        assert!(!journal_file_name(Path::new("/x/../..")).contains('/'));
    }

    #[test]
    fn a_journal_refuses_to_serve_another_project() {
        let dir = tempfile::tempdir().unwrap();
        open(dir.path(), Path::new("/a"), true).unwrap();
        let path = journal_path(dir.path(), Path::new("/a"));
        std::fs::copy(&path, journal_path(dir.path(), Path::new("/b"))).unwrap();
        let err = open(dir.path(), Path::new("/b"), true).unwrap_err().to_string();
        assert!(err.contains("belongs to /a"), "{err}");
        assert!(open(dir.path(), Path::new("/c"), false).unwrap().is_none());
    }

    #[test]
    fn labels_normalize_one_way() {
        assert_eq!(normalize_label("  #Design Decision "), Some("design-decision".into()));
        assert_eq!(normalize_label("#"), None);
        assert_eq!(normalize_tags(&["Nginx", "#nginx", "", "docker"]), vec!["nginx", "docker"]);
    }

    #[test]
    fn add_validates_and_dedups() {
        let (_d, mut c) = journal();
        assert!(add(&mut c, &entry("x", "", &["a"]), 10).is_err());
        assert!(add(&mut c, &entry("x", "decision", &[]), 10).is_err());
        assert!(add(&mut c, &entry("  ", "decision", &["a"]), 10).is_err());
        let (s, id) = add(&mut c, &entry("use WAL", "Decision", &["sqlite"]), 10).unwrap();
        assert_eq!(s, AddStatus::Written);
        let (s, again) = add(&mut c, &entry("use WAL", "decision", &["locking"]), 20).unwrap();
        assert_eq!((s, again), (AddStatus::Deduped, id));
        assert_eq!(get(&c, id).unwrap().unwrap().tags, vec!["sqlite", "locking"]);
        // Outside the window it is a new entry.
        let (s, _) = add(&mut c, &entry("use WAL", "decision", &["sqlite"]), 20 + DEDUP_WINDOW_SECS + 11).unwrap();
        assert_eq!(s, AddStatus::Written);
    }

    #[test]
    fn categories_tags_list_and_filters() {
        let (_d, mut c) = journal();
        add(&mut c, &entry("chose tokio", "decision", &["async", "runtime"]), 1).unwrap();
        add(&mut c, &entry("found a race in the bus", "finding", &["async", "bus"]), 2).unwrap();
        add(&mut c, &entry("vec0 cannot filter", "finding", &["sqlite"]), 3).unwrap();

        let cats = categories(&c).unwrap();
        assert_eq!((cats[0].name.as_str(), cats[0].count), ("finding", 2));
        let t = tags(&c, None).unwrap();
        assert_eq!((t[0].name.as_str(), t[0].count), ("async", 2));
        let t = tags(&c, Some("decision")).unwrap();
        assert_eq!(t.iter().map(|l| l.name.as_str()).collect::<Vec<_>>(), vec!["async", "runtime"]);

        let (total, page) = list(&c, &Filter::default(), 0, 2).unwrap();
        assert_eq!(total, 3);
        assert_eq!(page[0].body, "vec0 cannot filter");
        let f = Filter { tags: vec!["bus".into(), "sqlite".into()], ..Default::default() };
        assert_eq!(list(&c, &f, 0, 10).unwrap().0, 2);
        let f = Filter { category: Some("Finding".into()), since: Some(3), ..Default::default() };
        assert_eq!(list(&c, &f, 0, 10).unwrap().0, 1);
    }

    #[test]
    fn every_entry_carries_its_creation_time() {
        let (_d, mut c) = journal();
        add(&mut c, &entry("bumped rusqlite to 0.32", "update", &["deps"]), 1_761_300_300).unwrap();
        let (_, page) = list(&c, &Filter::default(), 0, 10).unwrap();
        assert_eq!((page[0].created_at, page[0].created.as_str()), (1_761_300_300, "2025-10-24T10:05:00Z"));
        let json = serde_json::to_value(&page[0]).unwrap();
        assert_eq!((json["created_at"].as_i64(), json["created"].as_str()), (Some(1_761_300_300), Some("2025-10-24T10:05:00Z")));
    }

    #[test]
    fn lexical_search_ranks_filters_and_survives_punctuation() {
        let (_d, mut c) = journal();
        add(&mut c, &entry("deploy to codefather.dev failed on v0.13.0", "finding", &["deploy"]), 1).unwrap();
        add(&mut c, &entry("deploy pipeline uses release.yml", "decision", &["ci"]), 2).unwrap();
        let (mode, hits) = search(&c, "codefather.dev v0.13.0", None, SearchMode::Hybrid, &Filter::default(), 5).unwrap();
        assert_eq!(mode, SearchMode::Lexical);
        assert_eq!(hits.len(), 1);
        assert!(hits[0].snippet.contains("codefather"));
        let f = Filter { category: Some("decision".into()), ..Default::default() };
        let (_, hits) = search(&c, "deploy", None, SearchMode::Lexical, &f, 5).unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].entry.category, "decision");
        assert!(search(&c, "...", None, SearchMode::Lexical, &Filter::default(), 5).unwrap().1.is_empty());
    }

    #[test]
    fn dense_leg_uses_stored_vectors_and_respects_filters() {
        let (_d, mut c) = journal();
        let (_, a) = add(&mut c, &entry("alpha", "finding", &["x"]), 1).unwrap();
        let (_, b) = add(&mut c, &entry("beta", "decision", &["y"]), 2).unwrap();
        assert_eq!(missing_vectors(&c, 10).unwrap().len(), 2);
        let mut va = vec![0.0f32; 384];
        va[0] = 1.0;
        let mut vb = vec![0.0f32; 384];
        vb[1] = 1.0;
        store_vector(&c, a, &va).unwrap();
        store_vector(&c, b, &vb).unwrap();
        assert!(missing_vectors(&c, 10).unwrap().is_empty());
        let (mode, hits) = search(&c, "zzz", Some(&vb), SearchMode::Dense, &Filter::default(), 1).unwrap();
        assert_eq!(mode, SearchMode::Dense);
        assert_eq!(hits[0].entry.id, b);
        let f = Filter { category: Some("finding".into()), ..Default::default() };
        let (_, hits) = search(&c, "zzz", Some(&vb), SearchMode::Dense, &f, 5).unwrap();
        assert_eq!(hits.iter().map(|h| h.entry.id).collect::<Vec<_>>(), vec![a]);
        // Deleting the entry takes its vector with it.
        assert!(delete(&c, b).unwrap());
        let n: i64 = c.query_row("SELECT COUNT(*) FROM entries_vec", [], |r| r.get(0)).unwrap();
        assert_eq!(n, 1);
    }

    #[test]
    fn gc_honours_salience() {
        let (_d, mut c) = journal();
        let now = 1_000_000_000;
        for (s, age) in [("high", 10_000 * 86_400), ("medium", 400 * 86_400), ("low", 100 * 86_400), ("low", 10 * 86_400)] {
            let mut e = entry(&format!("{s} {age}"), "finding", &["t"]);
            e.salience = s.into();
            add(&mut c, &e, now - age).unwrap();
        }
        let r = gc(&c, now, true).unwrap();
        assert_eq!((r.medium, r.low), (1, 1));
        assert_eq!(list(&c, &Filter::default(), 0, 10).unwrap().0, 4);
        gc(&c, now, false).unwrap();
        assert_eq!(list(&c, &Filter::default(), 0, 10).unwrap().0, 2);
    }

    #[test]
    fn old_insights_come_across_once() {
        let dir = tempfile::tempdir().unwrap();
        let project = dir.path().join("proj");
        let legacy = legacy_insights_db(&project);
        std::fs::create_dir_all(legacy.parent().unwrap()).unwrap();
        let old = Connection::open(&legacy).unwrap();
        old.execute_batch(
            "CREATE TABLE documents (id INTEGER PRIMARY KEY, source_path TEXT, ingested_at INTEGER, \
               source_type TEXT, agent_name TEXT, session_id TEXT, feature_slug TEXT, salience TEXT, parent_artifact TEXT);
             CREATE TABLE chunks (id INTEGER PRIMARY KEY, doc_id INTEGER, ord INTEGER, text TEXT);
             CREATE TABLE insight_tags (doc_id INTEGER, tag TEXT);
             INSERT INTO documents VALUES (1, 'book.pdf', 5, NULL, NULL, NULL, NULL, NULL, NULL);
             INSERT INTO documents VALUES (2, 'agent:x', 7, 'agent-learned', 'planner', 's1', 'feat', 'high', 'plan.md');
             INSERT INTO chunks VALUES (1, 2, 0, 'lesson body');
             INSERT INTO insight_tags VALUES (2, 'sqlite');",
        )
        .unwrap();
        drop(old);
        let journals = dir.path().join("journals");
        let mut c = open(&journals, &project, true).unwrap().unwrap();
        assert_eq!(import_legacy_insights(&mut c, &project).unwrap(), 1);
        assert_eq!(import_legacy_insights(&mut c, &project).unwrap(), 0);
        let (_, page) = list(&c, &Filter::default(), 0, 10).unwrap();
        assert_eq!(page.len(), 1);
        let e = &page[0];
        assert_eq!((e.category.as_str(), e.body.as_str(), e.salience.as_str()), ("agent-learned", "lesson body", "high"));
        assert_eq!((e.tags.clone(), e.created_at, e.agent.as_deref()), (vec!["sqlite".to_string()], 7, Some("planner")));
        assert!(legacy.exists(), "the original is left in place");
    }

    #[test]
    fn stitching_drops_the_window_overlap() {
        let body: String = (0..1200).map(|i| char::from(b'a' + (i % 26) as u8)).collect();
        let chunks: Vec<String> = [0usize, 400, 800]
            .iter()
            .map(|s| body.chars().skip(*s).take(500).collect())
            .collect();
        assert_eq!(stitch_chunks(&chunks), body);
    }

    #[test]
    fn journals_are_enumerable() {
        let dir = tempfile::tempdir().unwrap();
        let mut a = open(dir.path(), Path::new("/p/a"), true).unwrap().unwrap();
        add(&mut a, &entry("one", "finding", &["t"]), 50).unwrap();
        open(dir.path(), Path::new("/p/b"), true).unwrap();
        let all = list_journals(dir.path()).unwrap();
        assert_eq!(all.len(), 2);
        assert_eq!((all[0].project.as_str(), all[0].entries), ("/p/a", 1));
    }
}
