//! Storage layer: schema initialization, WAL pragma, FTS5 trigger wiring,
//! and `validate_schema` corruption probe.
//!
//! SQL discipline: ONLY ?N parameterized statements; never format!/+ for user data.
//!
//! Phase 1.5 Security MUSTs implemented here:
//!   #4  All SQL is either a static `&str` literal (CREATE/PRAGMA) or a parameterized
//!       statement using `rusqlite::params!`. Never `format!`/`write!`/`+` to build SQL.
//!
//! `open_or_init` opens the SQLite file (creating its parent dirs as needed),
//! flips `journal_mode` to WAL (NFR-1.6 / FR-2.7), and runs the v1 schema.
//! `validate_schema` confirms the four-table shape and `schema_version=1`.

use std::path::Path;
use std::sync::Once;

use rusqlite::Connection;
use thiserror::Error;

/// Process-wide once-flag for sqlite-vec extension registration. The crate
/// exposes a C entrypoint `sqlite3_vec_init` and we register it as a SQLite
/// auto-extension via rusqlite's FFI. After registration EVERY new Connection
/// opened in this process automatically loads the vec0 virtual table builtin.
/// This must run BEFORE the first Connection::open in the process.
static SQLITE_VEC_INIT: Once = Once::new();

fn ensure_sqlite_vec_registered() {
    SQLITE_VEC_INIT.call_once(|| {
        // SAFETY: sqlite_vec::sqlite3_vec_init is the C entrypoint exported
        // by libsqlite_vec0. Transmuting to the auto-extension function
        // pointer signature is the documented usage pattern from the
        // sqlite-vec crate's own integration tests (sqlite-vec 0.1.9).
        unsafe {
            rusqlite::ffi::sqlite3_auto_extension(Some(std::mem::transmute(
                sqlite_vec::sqlite3_vec_init as *const (),
            )));
        }
    });
}

use crate::output::{DocumentSummary, StatusInfo};

#[derive(Debug, Error)]
pub enum StoreError {
    #[error("database error: {0}")]
    Sqlite(#[from] rusqlite::Error),
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
}

#[derive(Debug, Error)]
pub enum IndexError {
    #[error("index database invalid; re-ingest required")]
    Corrupt,
    #[error("database error: {0}")]
    Sqlite(#[from] rusqlite::Error),
}

/// V1 schema — kept as a static `&str` literal; no user data interpolated.
///
/// V2 additions (page tracking) are applied via `migrations::run_migrations`:
///   - `chunks.page_start` / `chunks.page_end` (nullable INTEGER) — first and
///     last 1-indexed PDF page covered by the chunk text. NULL for non-PDF
///     sources. For PDFs (per-page chunking) `page_start = page_end`.
///   - new `pages` table — one row per (doc_id, page_no) holding the full
///     extracted text of that page. Powers the `page` subcommand which
///     returns the raw page text without re-running PDFium.
const SCHEMA_V1: &str = r#"
CREATE TABLE IF NOT EXISTS documents (
  id INTEGER PRIMARY KEY,
  source_path TEXT UNIQUE NOT NULL,
  mtime INTEGER NOT NULL,
  sha256 TEXT NOT NULL,
  ingested_at INTEGER NOT NULL
);

CREATE TABLE IF NOT EXISTS chunks (
  id INTEGER PRIMARY KEY,
  doc_id INTEGER NOT NULL REFERENCES documents(id) ON DELETE CASCADE,
  ord INTEGER NOT NULL,
  text TEXT NOT NULL
);

CREATE VIRTUAL TABLE IF NOT EXISTS chunks_fts USING fts5(
  text,
  content='chunks',
  content_rowid='id'
);

CREATE TABLE IF NOT EXISTS schema_version (version INTEGER NOT NULL);

CREATE TRIGGER IF NOT EXISTS chunks_ai AFTER INSERT ON chunks BEGIN
  INSERT INTO chunks_fts(rowid, text) VALUES (new.id, new.text);
END;

CREATE TRIGGER IF NOT EXISTS chunks_ad AFTER DELETE ON chunks BEGIN
  INSERT INTO chunks_fts(chunks_fts, rowid, text) VALUES('delete', old.id, old.text);
END;

CREATE TRIGGER IF NOT EXISTS chunks_au AFTER UPDATE ON chunks BEGIN
  INSERT INTO chunks_fts(chunks_fts, rowid, text) VALUES('delete', old.id, old.text);
  INSERT INTO chunks_fts(rowid, text) VALUES (new.id, new.text);
END;
"#;

/// Open (or create) the SQLite database at `db_path`, ensure parent directories exist,
/// flip journal_mode to WAL, and apply the v1 schema. Idempotent — safe to call on
/// an already-initialized database.
pub fn open_or_init(db_path: &Path) -> Result<Connection, StoreError> {
    if let Some(parent) = db_path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    // Register sqlite-vec auto-extension here too (Slice 7 CLI-wiring fix):
    // the extension is process-global once registered, and registering on the
    // v1 path means hybrid search on a v2 DB opened via this entry point still
    // sees vec0. v1 DBs simply won't have chunks_vec — vec0 SQL fails cleanly
    // and the search fallback to lexical fires per design.
    ensure_sqlite_vec_registered();
    let conn = Connection::open(db_path)?;
    // WAL is per-database persistent so this only matters first-run, but the call is
    // idempotent and very cheap.
    conn.pragma_update(None, "journal_mode", "WAL")?;
    conn.pragma_update(None, "foreign_keys", "ON")?;
    conn.execute_batch(SCHEMA_V1)?;
    Ok(conn)
}

/// V2 schema delta (Slice 2 of vector-retrieval-backend). Applied on top of
/// `SCHEMA_V1` for fresh DBs. Existing v1 DBs go through
/// `migrations::migrate_v1_to_v2` which is destructive (drop+recreate) per
/// architect OQ-2 resolution.
///
/// Adds two columns to `chunks`:
///   - `type` — 'text' | 'table' | 'image'; defaults to 'text' for legacy rows
///   - `image_bytes` — PNG bytes BLOB for figure chunks (NULL for text)
///
/// Adds `chunks_vec` virtual table backed by sqlite-vec — vec0 with
/// `embedding float[384]` for e5-multilingual-small (Slice 5 populates it).
///
/// SQL discipline: static `&str` literal, no user data interpolation.
const SCHEMA_V2_DELTA: &str = r#"
ALTER TABLE chunks ADD COLUMN type TEXT NOT NULL DEFAULT 'text';
ALTER TABLE chunks ADD COLUMN image_bytes BLOB;
CREATE VIRTUAL TABLE IF NOT EXISTS chunks_vec USING vec0(embedding float[384]);
"#;

/// V3 schema delta — page-level addressing. Additive and non-destructive:
///   - `chunks.page_start` / `page_end` — 1-indexed PDF page each chunk's
///     text was sourced from. NULL for legacy v2 chunks; freshly-ingested
///     PDFs populate them via `chunk_pages` in `ingest.rs`.
///   - `pages(doc_id, page_no, text)` table — raw per-page text exposed
///     to the LLM via `claudebase page <doc> <page>` so it can navigate
///     the source book the same way a human flips pages.
///
/// Page numbering is **pdfium 1-indexed** — independent of any "printed"
/// page numbering the document might use (Roman for preface, Arabic for
/// body). Out-of-range page lookups exit 1 with the literal stderr line
/// `error: page number out of range`.
///
/// SQL discipline: static `&str` literal, no user-data interpolation.
const SCHEMA_V3_DELTA: &str = r#"
ALTER TABLE chunks ADD COLUMN page_start INTEGER;
ALTER TABLE chunks ADD COLUMN page_end INTEGER;
CREATE TABLE IF NOT EXISTS pages (
  id INTEGER PRIMARY KEY,
  doc_id INTEGER NOT NULL REFERENCES documents(id) ON DELETE CASCADE,
  page_no INTEGER NOT NULL,
  text TEXT NOT NULL,
  UNIQUE(doc_id, page_no)
);
CREATE INDEX IF NOT EXISTS pages_doc_page_idx ON pages(doc_id, page_no);
"#;

/// V4 schema delta — agent-insights metadata. Additive and non-destructive.
///
/// Adds six nullable columns to `documents` so the same SQLite shape can host
/// either the user-curated books corpus (all metadata NULL, today's behavior)
/// or the new agent-written insights corpus (metadata populated by
/// `claudebase remember`). Books-corpus rows remain unaffected; back-compat
/// preserved via the NULL default on every new column.
///
/// Columns:
///   - `source_type`     — enum of the insight kind (e.g. reflection-observation,
///                         consolidator-drift, red-team-objection, decision-record,
///                         assumption-log, hack-acknowledged). NULL for book docs.
///   - `agent_name`      — emitting SDLC agent (planner, reflection, etc.).
///   - `session_id`      — Claude Code session UUID for trace linking.
///   - `feature_slug`    — feature this insight belongs to (matches `.claude/plan.md` feature).
///   - `salience`        — `high` | `medium` | `low` per cognitive-self-check rule;
///                         drives retention (high=∞, medium=1y, low=90d).
///   - `parent_artifact` — file path of the artifact the insight was extracted from.
///
/// Indexes on the four filter columns most likely to appear in `claudebase recall`
/// WHERE clauses (source_type / agent_name / feature_slug / salience).
///
/// SQL discipline: static `&str` literal, no user-data interpolation.
const SCHEMA_V4_DELTA: &str = r#"
ALTER TABLE documents ADD COLUMN source_type     TEXT;
ALTER TABLE documents ADD COLUMN agent_name      TEXT;
ALTER TABLE documents ADD COLUMN session_id      TEXT;
ALTER TABLE documents ADD COLUMN feature_slug    TEXT;
ALTER TABLE documents ADD COLUMN salience        TEXT;
ALTER TABLE documents ADD COLUMN parent_artifact TEXT;
CREATE INDEX IF NOT EXISTS idx_documents_source_type ON documents(source_type);
CREATE INDEX IF NOT EXISTS idx_documents_agent_name  ON documents(agent_name);
CREATE INDEX IF NOT EXISTS idx_documents_feature     ON documents(feature_slug);
CREATE INDEX IF NOT EXISTS idx_documents_salience    ON documents(salience);
"#;

/// V5 schema delta — insights-hybrid-corpus categorization + normalized tags.
/// Additive and non-destructive (FR-IHC-1.1..1.3).
///
/// Adds two nullable columns to `documents` so an insight row can record which
/// corpus it belongs to and which project it came from:
///   - `category`     — `general` | `project` for insight rows; NULL for books
///                      rows (the books corpus never carries a category).
///   - `project_slug` — basename of the per-project db path the insight was
///                      written against; NULL for general insights + books rows.
///
/// Adds a normalized many-to-one tags table so a single insight can carry
/// several tags without a delimited blob in the documents row:
///   - `insight_tags(doc_id, tag)` — UNIQUE(doc_id, tag) dedups repeated tags;
///     ON DELETE CASCADE so deleting an insight removes its tags.
///
/// Two indexes back the most common WHERE/JOIN filters:
///   - `idx_documents_category` on documents(category)
///   - `idx_insight_tags_tag`   on insight_tags(tag)
///
/// Books-corpus rows (`source_path NOT LIKE 'agent:%'`) are untouched by this
/// delta — `category` defaults to NULL and no `insight_tags` rows are created
/// for them. See `open_or_init_v2`'s v4→5 branch for the backfill that only
/// targets insight rows.
///
/// SQL discipline: static `&str` literal, no user-data interpolation.
const SCHEMA_V5_DELTA: &str = r#"
ALTER TABLE documents ADD COLUMN category     TEXT;
ALTER TABLE documents ADD COLUMN project_slug TEXT;
CREATE INDEX IF NOT EXISTS idx_documents_category ON documents(category);
CREATE TABLE IF NOT EXISTS insight_tags (
  doc_id INTEGER NOT NULL REFERENCES documents(id) ON DELETE CASCADE,
  tag    TEXT NOT NULL,
  UNIQUE(doc_id, tag)
);
CREATE INDEX IF NOT EXISTS idx_insight_tags_tag ON insight_tags(tag);
"#;

/// Open (or create) the SQLite database at `db_path` with v2 schema enabled.
/// Loads the sqlite-vec extension at connection-open time (architect OQ-2
/// resolution: `sqlite_vec::load(&conn)` registers vec0 without enabling
/// rusqlite's `load_extension` feature, preserving the security posture).
///
/// Migration semantics for existing DBs:
///   - Fresh DB (schema_version absent): apply SCHEMA_V1 + SCHEMA_V2_DELTA, stamp version=2
///   - schema_version=1: caller MUST run `migrations::migrate_v1_to_v2` (destructive re-ingest)
///   - schema_version=2: idempotent no-op (CREATE ... IF NOT EXISTS clauses)
///
/// Returns the connection on success. Caller is responsible for invoking
/// migration if the DB is at v1 and needs upgrading.
pub fn open_or_init_v2(db_path: &Path) -> Result<Connection, StoreError> {
    if let Some(parent) = db_path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    // Register sqlite-vec auto-extension once per process BEFORE Connection::open
    // so the new connection picks up vec0 virtual table builtin + vec_distance_cosine
    // SQL function. Per architect OQ-2 this uses sqlite3_auto_extension (NOT
    // rusqlite's `load_extension` feature, which stays OFF — security posture).
    ensure_sqlite_vec_registered();
    let mut conn = Connection::open(db_path)?;
    conn.pragma_update(None, "journal_mode", "WAL")?;
    conn.pragma_update(None, "foreign_keys", "ON")?;
    conn.execute_batch(SCHEMA_V1)?;
    // Apply v2 delta only on fresh DBs (no schema_version row) OR when
    // schema_version=2 (idempotent CREATE IF NOT EXISTS for chunks_vec; the
    // ALTER TABLE statements would error on re-run for v2-already DBs, so we
    // gate them via current_version).
    let v: i64 = conn
        .query_row("SELECT version FROM schema_version", [], |r| r.get(0))
        .unwrap_or(0);
    if v == 0 {
        // Fresh DB — apply v2 + v3 + v4 + v5 deltas and stamp version=5.
        conn.execute_batch(SCHEMA_V2_DELTA)?;
        conn.execute_batch(SCHEMA_V3_DELTA)?;
        conn.execute_batch(SCHEMA_V4_DELTA)?;
        conn.execute_batch(SCHEMA_V5_DELTA)?;
        conn.execute(
            "INSERT INTO schema_version(version) VALUES (?1)",
            rusqlite::params![5i64],
        )?;
    } else if v == 2 {
        // v2 → v4 progression. Additive + non-destructive: page columns +
        // pages table from v3, then the six agent-insights metadata columns
        // from v4. Existing chunks keep NULL page_start/page_end + NULL
        // insights metadata (pages table is empty until `claudebase
        // reindex-pages` runs; insights metadata stays NULL on books-corpus
        // rows). Then carry through v4→v5 (category/project_slug columns +
        // insight_tags table + backfill) so a v2 db converges on v5 in one
        // open. Wrap in a transaction so a partially-failed v2→v5 rolls back.
        let tx = conn.transaction()?;
        tx.execute_batch(SCHEMA_V3_DELTA)?;
        tx.execute_batch(SCHEMA_V4_DELTA)?;
        apply_v5_delta_and_backfill(&tx, db_path)?;
        tx.commit()?;
    } else if v == 3 {
        // Already at v3 — ensure forward-compat objects exist (CREATE IF NOT
        // EXISTS for both vec0 and pages so a corruption-free re-open is
        // idempotent).
        //
        // Legacy v3 shape (Slice 12 first iteration before merge with main):
        // pages had `page_num` column instead of `page_no`. Detect that shape
        // and rename the column in-place — SQLite 3.25+ supports
        // `ALTER TABLE ... RENAME COLUMN`. The data (doc_id, page_text) is
        // schema-equivalent so the rename is a no-data-loss operation.
        let has_page_num: bool = conn
            .query_row(
                "SELECT 1 FROM pragma_table_info('pages') WHERE name = 'page_num'",
                [],
                |_| Ok(true),
            )
            .unwrap_or(false);
        if has_page_num {
            conn.execute_batch(
                "ALTER TABLE pages RENAME COLUMN page_num TO page_no; \
                 DROP INDEX IF EXISTS idx_pages_doc;",
            )?;
        }
        conn.execute_batch(
            "CREATE VIRTUAL TABLE IF NOT EXISTS chunks_vec USING vec0(embedding float[384]); \
             CREATE TABLE IF NOT EXISTS pages ( \
               id INTEGER PRIMARY KEY, \
               doc_id INTEGER NOT NULL REFERENCES documents(id) ON DELETE CASCADE, \
               page_no INTEGER NOT NULL, \
               text TEXT NOT NULL, \
               UNIQUE(doc_id, page_no) \
             ); \
             CREATE INDEX IF NOT EXISTS pages_doc_page_idx ON pages(doc_id, page_no);",
        )?;
        // v3 → v5 progression. Apply the agent-insights metadata columns +
        // indexes (v4), then category/project_slug + insight_tags + backfill
        // (v5) so a v3 db converges on v5 in one open. Additive + non-
        // destructive on the books corpus: rows that existed at v3 stay valid;
        // the v4/v5 columns are NULL on all books rows.
        let tx = conn.transaction()?;
        tx.execute_batch(SCHEMA_V4_DELTA)?;
        apply_v5_delta_and_backfill(&tx, db_path)?;
        tx.commit()?;
    } else if v == 4 {
        // v4 → v5 upgrade. Before applying the v5 delta we defensively ensure
        // every v4 column exists (a partially-failed prior v4 migration could
        // have left version=4 stamped without all six columns; the v5 backfill
        // references `feature_slug`, so it must exist first). `ALTER TABLE ...
        // ADD COLUMN` is not idempotent natively — probe pragma and add only
        // the missing ones.
        let v4_cols = [
            "source_type", "agent_name", "session_id",
            "feature_slug", "salience", "parent_artifact",
        ];
        for col in v4_cols {
            let exists: bool = conn
                .query_row(
                    "SELECT 1 FROM pragma_table_info('documents') WHERE name = ?1",
                    rusqlite::params![col],
                    |_| Ok(true),
                )
                .unwrap_or(false);
            if !exists {
                let stmt = format!("ALTER TABLE documents ADD COLUMN {col} TEXT");
                conn.execute_batch(&stmt)?;
            }
        }
        conn.execute_batch(
            "CREATE INDEX IF NOT EXISTS idx_documents_source_type ON documents(source_type); \
             CREATE INDEX IF NOT EXISTS idx_documents_agent_name  ON documents(agent_name); \
             CREATE INDEX IF NOT EXISTS idx_documents_feature     ON documents(feature_slug); \
             CREATE INDEX IF NOT EXISTS idx_documents_salience    ON documents(salience);",
        )?;
        // Apply the v5 delta + insight-row backfill transactionally so a
        // partially-failed v4→v5 rolls back cleanly.
        let tx = conn.transaction()?;
        apply_v5_delta_and_backfill(&tx, db_path)?;
        tx.commit()?;
    } else if v == 5 {
        // Already at v5 — idempotent re-open. A partially-failed prior v4→v5
        // migration could have left version=5 stamped without the two columns
        // or the insight_tags table; probe each and add the missing ones.
        // `ALTER TABLE ... ADD COLUMN` is not idempotent natively, so we probe
        // pragma first (mirrors the v4 probe loop above).
        let v5_cols = ["category", "project_slug"];
        for col in v5_cols {
            let exists: bool = conn
                .query_row(
                    "SELECT 1 FROM pragma_table_info('documents') WHERE name = ?1",
                    rusqlite::params![col],
                    |_| Ok(true),
                )
                .unwrap_or(false);
            if !exists {
                let stmt = format!("ALTER TABLE documents ADD COLUMN {col} TEXT");
                conn.execute_batch(&stmt)?;
            }
        }
        // insight_tags table + both indexes are CREATE ... IF NOT EXISTS, so
        // re-creating them is a safe idempotent converge for a partial prior
        // migration that added columns but skipped the table/indexes.
        conn.execute_batch(
            "CREATE INDEX IF NOT EXISTS idx_documents_category ON documents(category); \
             CREATE TABLE IF NOT EXISTS insight_tags ( \
               doc_id INTEGER NOT NULL REFERENCES documents(id) ON DELETE CASCADE, \
               tag    TEXT NOT NULL, \
               UNIQUE(doc_id, tag) \
             ); \
             CREATE INDEX IF NOT EXISTS idx_insight_tags_tag ON insight_tags(tag);",
        )?;
    }
    // v == 1: caller runs migrate_v1_to_v2 explicitly. We don't auto-migrate
    // here because migration is destructive (architect-resolved).
    Ok(conn)
}

/// Derive the `project_slug` value backfilled into v5 insight rows from the
/// db path. We use the basename of the db file's grand-parent directory —
/// i.e. the project root name — falling back to the file stem, then to the
/// literal `"unknown"` so the column is never NULL for an `agent:%` row.
///
/// Example: `/Users/x/proj/.claude/knowledge/insights.db` → `"proj"`
/// (`.claude/knowledge/insights.db` ⇒ walk up three components to `proj`).
/// A bare `insights.db` with no project ancestry → `"insights"` (file stem).
fn derive_project_slug(db_path: &Path) -> String {
    // .../<proj>/.claude/knowledge/insights.db
    //                    ^knowledge ^.claude  ^<proj>
    db_path
        .parent() // knowledge
        .and_then(|p| p.parent()) // .claude
        .and_then(|p| p.parent()) // <proj>
        .and_then(|p| p.file_name())
        .map(|s| s.to_string_lossy().into_owned())
        .or_else(|| {
            db_path
                .file_stem()
                .map(|s| s.to_string_lossy().into_owned())
        })
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "unknown".to_string())
}

/// Apply the V5 schema delta + insight-row backfill within an open transaction,
/// then stamp `schema_version = 5`. Shared by the v2/v3/v4 upgrade branches so
/// any pre-v5 db converges on v5 in a single open — the backfill SQL lives in
/// exactly one place.
///
/// Backfill semantics (FR-IHC-1.5, FR-IHC-1.6):
///   - Only `documents` rows whose `source_path LIKE 'agent:%'` (the insight
///     rows) are touched. Books-corpus rows are left with `category IS NULL`
///     and zero `insight_tags` rows.
///   - `category` ⇒ `'project'` (existing local insights are project-scoped).
///   - `project_slug` ⇒ db-path-derived basename (see `derive_project_slug`).
///     Guarded by `category IS NULL` so a re-run is a no-op on already-tagged
///     rows.
///   - One default tag per insight ⇒ `COALESCE(NULLIF(feature_slug,''),'untagged')`.
///     `INSERT OR IGNORE` so the `UNIQUE(doc_id, tag)` constraint silently
///     dedups on re-run / partial-prior-migration.
///
/// SQL discipline: `project_slug` value is parameterized (`?1`); every other
/// statement is a static literal with no user-data interpolation.
fn apply_v5_delta_and_backfill(
    tx: &rusqlite::Transaction<'_>,
    db_path: &Path,
) -> Result<(), rusqlite::Error> {
    tx.execute_batch(SCHEMA_V5_DELTA)?;
    let slug = derive_project_slug(db_path);
    tx.execute(
        "UPDATE documents SET category = 'project', project_slug = ?1 \
         WHERE source_path LIKE 'agent:%' AND category IS NULL",
        rusqlite::params![slug],
    )?;
    tx.execute_batch(
        "INSERT OR IGNORE INTO insight_tags(doc_id, tag) \
         SELECT id, COALESCE(NULLIF(feature_slug, ''), 'untagged') \
         FROM documents WHERE source_path LIKE 'agent:%';",
    )?;
    tx.execute(
        "UPDATE schema_version SET version = ?1",
        rusqlite::params![5i64],
    )?;
    Ok(())
}

/// Confirm the four expected objects exist, `schema_version` row is in `1..=2`
/// (forward-compat for iter-2), and `chunks_fts` is an FTS5 virtual table.
///
/// Returns `IndexError::Corrupt` on ANY structural mismatch — including raw
/// rusqlite errors raised during the probe (a truncated database file, a file
/// that isn't a SQLite database at all, schema-master corruption, etc.).
/// Mapping all failure modes to a single variant prevents information leak
/// and lets the caller print the literal user-facing message
/// `error: index database invalid; re-ingest required` per FR-1.6 / AC-7.
pub fn validate_schema(conn: &Connection) -> Result<(), IndexError> {
    validate_schema_inner(conn).map_err(|_| IndexError::Corrupt)
}

/// Internal helper: any error here flips to `IndexError::Corrupt` in the public
/// wrapper. Using `anyhow::Error` would pull a runtime dep — instead, we use
/// `rusqlite::Error` plus a sentinel `Corrupt` short-circuit via `?`-on-`Result`.
fn validate_schema_inner(conn: &Connection) -> Result<(), rusqlite::Error> {
    // Required objects (table or virtual-table).
    let required = ["documents", "chunks", "chunks_fts", "schema_version"];

    // A single sqlite_master scan: collect (name, type, sql) triples so we can
    // additionally verify chunks_fts is FTS5 (the CREATE VIRTUAL TABLE sql
    // contains the literal `fts5` token).
    let mut stmt = conn.prepare(
        "SELECT name, type, COALESCE(sql, '') FROM sqlite_master \
         WHERE name IN ('documents','chunks','chunks_fts','schema_version')",
    )?;
    let mut found: std::collections::HashMap<String, (String, String)> =
        std::collections::HashMap::new();
    let rows = stmt.query_map([], |r| {
        Ok((
            r.get::<_, String>(0)?,
            r.get::<_, String>(1)?,
            r.get::<_, String>(2)?,
        ))
    })?;
    for row in rows {
        let (name, ty, sql) = row?;
        found.insert(name, (ty, sql));
    }
    for n in required {
        if !found.contains_key(n) {
            return Err(rusqlite::Error::QueryReturnedNoRows);
        }
    }

    // chunks_fts must be a virtual table backed by FTS5.
    let (fts_type, fts_sql) = found
        .get("chunks_fts")
        .ok_or(rusqlite::Error::QueryReturnedNoRows)?;
    if fts_type != "table" {
        return Err(rusqlite::Error::QueryReturnedNoRows);
    }
    if !fts_sql.to_lowercase().contains("fts5") {
        return Err(rusqlite::Error::QueryReturnedNoRows);
    }

    // schema_version row exists and is in 1..=5 (forward-compat through v5
    // insights-hybrid-corpus categorization — documents.category /
    // project_slug + the insight_tags table). v4 added the agent-insights
    // metadata columns (source_type / agent_name / session_id / feature_slug /
    // salience / parent_artifact).
    let v: i64 = conn.query_row("SELECT version FROM schema_version", [], |r| r.get(0))?;
    if !(1..=5).contains(&v) {
        return Err(rusqlite::Error::QueryReturnedNoRows);
    }

    Ok(())
}

/// Insert or update a documents row; returns the row id.
///
/// SQL discipline: parameterized via `?1..?4`. The literal SQL is a static `&str`.
pub fn upsert_document(
    conn: &Connection,
    source_path: &str,
    mtime: i64,
    sha256: &str,
    ingested_at: i64,
) -> Result<i64, rusqlite::Error> {
    conn.execute(
        "INSERT INTO documents(source_path, mtime, sha256, ingested_at) \
         VALUES (?1, ?2, ?3, ?4) \
         ON CONFLICT(source_path) DO UPDATE SET \
           mtime = excluded.mtime, \
           sha256 = excluded.sha256, \
           ingested_at = excluded.ingested_at",
        rusqlite::params![source_path, mtime, sha256, ingested_at],
    )?;
    let id: i64 = conn.query_row(
        "SELECT id FROM documents WHERE source_path = ?1",
        rusqlite::params![source_path],
        |r| r.get(0),
    )?;
    Ok(id)
}

/// Insert or update a documents row carrying the v4 agent-insights metadata.
///
/// This is the parallel of `upsert_document` for the insights corpus
/// (`insights.db`): same `INSERT ... ON CONFLICT(source_path) DO UPDATE`
/// shape so an in-session re-write of the same synthetic source_path
/// produces exactly one row, but extended with the six nullable columns
/// added by `SCHEMA_V4_DELTA`. Books-corpus rows continue to use
/// `upsert_document` (which leaves the v4 columns NULL).
///
/// SQL discipline: parameterized via `?1..?10`; static `&str` literal SQL.
#[allow(clippy::too_many_arguments)]
pub fn upsert_insight_document(
    conn: &Connection,
    source_path: &str,
    mtime: i64,
    sha256: &str,
    ingested_at: i64,
    source_type: &str,
    agent_name: &str,
    session_id: Option<&str>,
    feature_slug: Option<&str>,
    salience: &str,
    parent_artifact: Option<&str>,
) -> Result<i64, rusqlite::Error> {
    conn.execute(
        "INSERT INTO documents( \
             source_path, mtime, sha256, ingested_at, \
             source_type, agent_name, session_id, \
             feature_slug, salience, parent_artifact) \
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10) \
         ON CONFLICT(source_path) DO UPDATE SET \
           mtime           = excluded.mtime, \
           sha256          = excluded.sha256, \
           ingested_at     = excluded.ingested_at, \
           source_type     = excluded.source_type, \
           agent_name      = excluded.agent_name, \
           session_id      = excluded.session_id, \
           feature_slug    = excluded.feature_slug, \
           salience        = excluded.salience, \
           parent_artifact = excluded.parent_artifact",
        rusqlite::params![
            source_path,
            mtime,
            sha256,
            ingested_at,
            source_type,
            agent_name,
            session_id,
            feature_slug,
            salience,
            parent_artifact,
        ],
    )?;
    let id: i64 = conn.query_row(
        "SELECT id FROM documents WHERE source_path = ?1",
        rusqlite::params![source_path],
        |r| r.get(0),
    )?;
    Ok(id)
}

/// One row returned by the `insight list / random / get` family. Carries
/// the v4 metadata columns plus the reconstructed body text (chunks joined
/// with 100-char overlap collapsed, matching the ingest::chunk window).
#[derive(Debug, Clone, serde::Serialize)]
pub struct InsightRecord {
    pub id: i64,
    pub source_path: String,
    pub sha256: String,
    pub ingested_at: i64,
    pub source_type: Option<String>,
    pub agent_name: Option<String>,
    pub session_id: Option<String>,
    pub feature_slug: Option<String>,
    pub salience: Option<String>,
    pub parent_artifact: Option<String>,
    pub body: String,
}

/// Compact summary for the `list` page — same identifying fields as the
/// full record but with a snippet instead of the full body.
#[derive(Debug, Clone, serde::Serialize)]
pub struct InsightSummary {
    pub id: i64,
    pub sha256_short: String,
    pub ingested_at: i64,
    pub source_type: Option<String>,
    pub agent_name: Option<String>,
    pub salience: Option<String>,
    pub feature_slug: Option<String>,
    pub snippet: String,
}

/// Reconstruct a flat body from `chunks` rows ordered by `ord`. Insights
/// are written by `ingest::chunk` (flat 500/100 sliding window with
/// 100-char overlap), so when chunks > 1 the adjacent chunks share the
/// trailing/leading 100 chars; the helper drops the overlap from chunks
/// 1..N when stitching. For single-chunk insights the chunk text is
/// returned verbatim.
fn reconstruct_body_from_chunks(chunks: &[String]) -> String {
    if chunks.is_empty() {
        return String::new();
    }
    let mut out = chunks[0].clone();
    for chunk in &chunks[1..] {
        let chars: Vec<char> = chunk.chars().collect();
        // The chunker uses CHUNK_OVERLAP = 100 so the first 100 chars of
        // every chunk after #0 duplicate the previous chunk's tail.
        const OVERLAP: usize = 100;
        if chars.len() > OVERLAP {
            let suffix: String = chars[OVERLAP..].iter().collect();
            out.push_str(&suffix);
        } else {
            // Chunk shorter than the overlap window — happens only when the
            // body is exactly window-aligned. Append as-is.
            out.push_str(chunk);
        }
    }
    out
}

/// Internal: load the full `InsightRecord` for one `documents.id`. Caller
/// has already verified the row is an insight (source_type IS NOT NULL).
fn load_insight_record(
    conn: &Connection,
    id: i64,
) -> Result<Option<InsightRecord>, rusqlite::Error> {
    use rusqlite::OptionalExtension;
    let row: Option<(
        i64,
        String,
        String,
        i64,
        Option<String>,
        Option<String>,
        Option<String>,
        Option<String>,
        Option<String>,
        Option<String>,
    )> = conn
        .query_row(
            "SELECT id, source_path, sha256, ingested_at, source_type, \
                    agent_name, session_id, feature_slug, salience, parent_artifact \
             FROM documents WHERE id = ?1",
            rusqlite::params![id],
            |r| {
                Ok((
                    r.get(0)?,
                    r.get(1)?,
                    r.get(2)?,
                    r.get(3)?,
                    r.get(4)?,
                    r.get(5)?,
                    r.get(6)?,
                    r.get(7)?,
                    r.get(8)?,
                    r.get(9)?,
                ))
            },
        )
        .optional()?;
    let Some((
        id,
        source_path,
        sha256,
        ingested_at,
        source_type,
        agent_name,
        session_id,
        feature_slug,
        salience,
        parent_artifact,
    )) = row
    else {
        return Ok(None);
    };
    // Stitch chunks back into the full body. `stmt` must outlive the
    // MappedRows iterator returned by `query_map`, so we collect inside
    // the same scope with the stmt binding still alive.
    let chunk_texts: Vec<String> = {
        let mut stmt = conn.prepare(
            "SELECT text FROM chunks WHERE doc_id = ?1 ORDER BY ord",
        )?;
        let rows = stmt.query_map(rusqlite::params![id], |r| r.get::<_, String>(0))?;
        let collected: Vec<String> = rows.filter_map(Result::ok).collect();
        collected
    };
    let body = reconstruct_body_from_chunks(&chunk_texts);
    Ok(Some(InsightRecord {
        id,
        source_path,
        sha256,
        ingested_at,
        source_type,
        agent_name,
        session_id,
        feature_slug,
        salience,
        parent_artifact,
        body,
    }))
}

/// Count insights (rows where `source_type IS NOT NULL`) optionally
/// filtered by source_type / agent / salience / feature.
#[allow(clippy::too_many_arguments)]
pub fn count_insights(
    conn: &Connection,
    kind: Option<&str>,
    agent: Option<&str>,
    salience: Option<&str>,
    feature: Option<&str>,
) -> Result<i64, rusqlite::Error> {
    let (sql, params) = build_filter_sql(
        "SELECT COUNT(*) FROM documents",
        kind,
        agent,
        salience,
        feature,
        None,
        None,
    );
    let params_refs: Vec<&dyn rusqlite::ToSql> =
        params.iter().map(|s| s as &dyn rusqlite::ToSql).collect();
    conn.query_row(&sql, params_refs.as_slice(), |r| r.get::<_, i64>(0))
}

/// List insights newest-first with metadata filters and OFFSET/LIMIT
/// pagination. Returns compact summaries — call `get_insight_by_id` to
/// fetch a full record with the reconstructed body.
#[allow(clippy::too_many_arguments)]
pub fn list_insights(
    conn: &Connection,
    kind: Option<&str>,
    agent: Option<&str>,
    salience: Option<&str>,
    feature: Option<&str>,
    limit: i64,
    offset: i64,
) -> Result<Vec<InsightSummary>, rusqlite::Error> {
    let (sql, params) = build_filter_sql(
        "SELECT id, sha256, ingested_at, source_type, agent_name, salience, feature_slug \
         FROM documents",
        kind,
        agent,
        salience,
        feature,
        Some(limit),
        Some(offset),
    );
    let params_refs: Vec<&dyn rusqlite::ToSql> =
        params.iter().map(|s| s as &dyn rusqlite::ToSql).collect();
    let mut stmt = conn.prepare(&sql)?;
    let rows = stmt.query_map(params_refs.as_slice(), |r| {
        let id: i64 = r.get(0)?;
        let sha: String = r.get(1)?;
        Ok((
            id,
            sha,
            r.get::<_, i64>(2)?,
            r.get::<_, Option<String>>(3)?,
            r.get::<_, Option<String>>(4)?,
            r.get::<_, Option<String>>(5)?,
            r.get::<_, Option<String>>(6)?,
        ))
    })?;
    let mut summaries: Vec<(i64, String, i64, Option<String>, Option<String>, Option<String>, Option<String>)> = Vec::new();
    for row in rows {
        summaries.push(row?);
    }
    // Render snippets — pull chunk 0's text up to 200 chars per insight.
    let mut out = Vec::with_capacity(summaries.len());
    for (id, sha, ingested_at, st, an, sal, feat) in summaries {
        let snippet = {
            use rusqlite::OptionalExtension;
            let row: Option<String> = conn
                .query_row(
                    "SELECT text FROM chunks WHERE doc_id = ?1 ORDER BY ord LIMIT 1",
                    rusqlite::params![id],
                    |r| r.get(0),
                )
                .optional()?;
            row.unwrap_or_default()
                .chars()
                .take(200)
                .collect::<String>()
        };
        out.push(InsightSummary {
            id,
            sha256_short: sha.chars().take(16).collect(),
            ingested_at,
            source_type: st,
            agent_name: an,
            salience: sal,
            feature_slug: feat,
            snippet,
        });
    }
    Ok(out)
}

/// Return one random insight (uniform sample) optionally filtered by the
/// same dimensions as `list_insights`. Returns `Ok(None)` when no row
/// matches the filters (empty corpus or restrictive filter combination).
///
/// Cannot reuse `build_filter_sql` because the random path needs
/// `ORDER BY RANDOM() LIMIT 1`, not `ORDER BY ingested_at DESC LIMIT ?`.
pub fn random_insight(
    conn: &Connection,
    kind: Option<&str>,
    agent: Option<&str>,
    salience: Option<&str>,
    feature: Option<&str>,
) -> Result<Option<InsightRecord>, rusqlite::Error> {
    use rusqlite::OptionalExtension;
    let mut sql = String::from("SELECT id FROM documents WHERE source_type IS NOT NULL");
    let mut params: Vec<String> = Vec::new();
    let mut next_idx = 1usize;
    for (col, val) in [
        ("source_type", kind),
        ("agent_name", agent),
        ("salience", salience),
        ("feature_slug", feature),
    ] {
        if let Some(v) = val {
            sql.push_str(&format!(" AND {col} = ?{next_idx}"));
            params.push(v.to_string());
            next_idx += 1;
        }
    }
    sql.push_str(" ORDER BY RANDOM() LIMIT 1");
    let params_refs: Vec<&dyn rusqlite::ToSql> =
        params.iter().map(|s| s as &dyn rusqlite::ToSql).collect();
    let id: Option<i64> = conn
        .query_row(&sql, params_refs.as_slice(), |r| r.get(0))
        .optional()?;
    match id {
        Some(id) => load_insight_record(conn, id),
        None => Ok(None),
    }
}

/// Fetch one insight by integer `documents.id`. Returns `Ok(None)` when no
/// row exists OR when the row exists but is a books-corpus doc
/// (source_type IS NULL) — the caller treats the latter as "not an insight".
pub fn get_insight_by_id(
    conn: &Connection,
    id: i64,
) -> Result<Option<InsightRecord>, rusqlite::Error> {
    let rec = load_insight_record(conn, id)?;
    Ok(rec.filter(|r| r.source_type.is_some()))
}

/// Fetch one insight by sha256 prefix (≥4 hex chars, matched as
/// `sha256 LIKE 'prefix%'`). Returns `Err(rusqlite::Error)` mapped from
/// QueryReturnedNoRows when no match; returns the most recently ingested
/// match when the prefix matches multiple rows (rare; means the user gave
/// too short a prefix).
pub fn get_insight_by_sha_prefix(
    conn: &Connection,
    prefix: &str,
) -> Result<Option<InsightRecord>, rusqlite::Error> {
    use rusqlite::OptionalExtension;
    let pattern = format!("{prefix}%");
    let id: Option<i64> = conn
        .query_row(
            "SELECT id FROM documents \
             WHERE sha256 LIKE ?1 AND source_type IS NOT NULL \
             ORDER BY ingested_at DESC LIMIT 1",
            rusqlite::params![pattern],
            |r| r.get(0),
        )
        .optional()?;
    match id {
        Some(id) => load_insight_record(conn, id),
        None => Ok(None),
    }
}

/// Build a parameterized SELECT with optional WHERE filters and
/// LIMIT/OFFSET clauses for the insight-list / random / count family.
///
/// The base SELECT is passed in by the caller (so the same builder works
/// for `SELECT id, ...` and `SELECT COUNT(*)`). Filters are pushed onto a
/// `WHERE source_type IS NOT NULL AND ...` chain so books-corpus rows are
/// always excluded — this is the "insight" semantic boundary.
fn build_filter_sql(
    base_select: &str,
    kind: Option<&str>,
    agent: Option<&str>,
    salience: Option<&str>,
    feature: Option<&str>,
    limit: Option<i64>,
    offset: Option<i64>,
) -> (String, Vec<String>) {
    let mut sql = format!("{base_select} WHERE source_type IS NOT NULL");
    let mut params: Vec<String> = Vec::new();
    let mut next_idx = 1usize;
    let push_eq = |col: &str, val: &str, sql: &mut String, params: &mut Vec<String>, idx: &mut usize| {
        sql.push_str(&format!(" AND {col} = ?{idx}"));
        params.push(val.to_string());
        *idx += 1;
    };
    if let Some(v) = kind {
        push_eq("source_type", v, &mut sql, &mut params, &mut next_idx);
    }
    if let Some(v) = agent {
        push_eq("agent_name", v, &mut sql, &mut params, &mut next_idx);
    }
    if let Some(v) = salience {
        push_eq("salience", v, &mut sql, &mut params, &mut next_idx);
    }
    if let Some(v) = feature {
        push_eq("feature_slug", v, &mut sql, &mut params, &mut next_idx);
    }
    // ORDER BY ingested_at DESC for stable newest-first pagination. The
    // `random_insight` caller rewrites this clause via string-replace
    // (see `random_insight`); for list/count this is the canonical order.
    sql.push_str(" ORDER BY ingested_at DESC");
    if let Some(l) = limit {
        sql.push_str(&format!(" LIMIT ?{next_idx}"));
        params.push(l.to_string());
        next_idx += 1;
        if let Some(o) = offset {
            sql.push_str(&format!(" OFFSET ?{next_idx}"));
            params.push(o.to_string());
        }
    }
    let _ = next_idx; // suppress unused-mut lint when both Optional are None
    (sql, params)
}

/// TTL-driven garbage-collection summary returned by
/// `gc_insights_by_salience` / `count_insights_past_ttl`.
#[derive(Debug, Clone, serde::Serialize)]
pub struct GcSummary {
    /// Number of medium-salience insights past 365 days.
    pub medium_deleted: u64,
    /// Number of low-salience insights past 90 days.
    pub low_deleted: u64,
    /// Total chunks_vec rows cleared as a result.
    pub chunks_vec_orphans_cleared: u64,
}

/// Compute (without deleting) how many insights would be purged at `now`.
/// Mirrors `gc_insights_by_salience` but uses SELECT COUNT(*) — used by
/// the `--dry-run` flag.
pub fn count_insights_past_ttl(
    conn: &Connection,
    now: i64,
) -> Result<GcSummary, rusqlite::Error> {
    let medium_cutoff = now - 365 * 86_400;
    let low_cutoff = now - 90 * 86_400;
    let medium: i64 = conn.query_row(
        "SELECT COUNT(*) FROM documents \
         WHERE source_type IS NOT NULL AND salience = 'medium' AND ingested_at < ?1",
        rusqlite::params![medium_cutoff],
        |r| r.get(0),
    )?;
    let low: i64 = conn.query_row(
        "SELECT COUNT(*) FROM documents \
         WHERE source_type IS NOT NULL AND salience = 'low' AND ingested_at < ?1",
        rusqlite::params![low_cutoff],
        |r| r.get(0),
    )?;
    Ok(GcSummary {
        medium_deleted: medium.max(0) as u64,
        low_deleted: low.max(0) as u64,
        chunks_vec_orphans_cleared: 0,
    })
}

/// TTL purge: delete insights past their salience-driven retention window.
///
/// Retention rules (FR-AIB-8.1):
///   - salience = 'high'   → retained indefinitely (never purged)
///   - salience = 'medium' → retained 365 days
///   - salience = 'low'    → retained 90 days
///   - salience IS NULL    → ignored (defensive — should not occur for insights)
///
/// Books-corpus rows (source_type IS NULL) are NEVER touched even when this
/// helper runs against the books DB by mistake — the WHERE clause guards on
/// `source_type IS NOT NULL`.
///
/// `chunks` rows cascade-delete via `chunks(doc_id) REFERENCES documents(id)
/// ON DELETE CASCADE` in SCHEMA_V1. `chunks_fts` stays in sync via the
/// `chunks_ad` trigger. `chunks_vec` rows are NOT cascade-deleted by SQLite
/// (it's a virtual table — no FK relationship), so we explicitly clear
/// orphans after the document delete: `DELETE FROM chunks_vec WHERE rowid
/// NOT IN (SELECT id FROM chunks)`.
pub fn gc_insights_by_salience(
    conn: &mut Connection,
    now: i64,
) -> Result<GcSummary, rusqlite::Error> {
    let medium_cutoff = now - 365 * 86_400;
    let low_cutoff = now - 90 * 86_400;
    let tx = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
    let n_medium = tx.execute(
        "DELETE FROM documents \
         WHERE source_type IS NOT NULL AND salience = 'medium' AND ingested_at < ?1",
        rusqlite::params![medium_cutoff],
    )?;
    let n_low = tx.execute(
        "DELETE FROM documents \
         WHERE source_type IS NOT NULL AND salience = 'low' AND ingested_at < ?1",
        rusqlite::params![low_cutoff],
    )?;
    // Orphaned chunks_vec cleanup — skip silently if the virtual table is
    // absent (v1 DB or chunks_vec never created).
    let has_vec: bool = tx
        .query_row(
            "SELECT 1 FROM sqlite_master WHERE name='chunks_vec'",
            [],
            |_| Ok(true),
        )
        .unwrap_or(false);
    let n_vec_orphans = if has_vec {
        tx.execute(
            "DELETE FROM chunks_vec WHERE rowid NOT IN (SELECT id FROM chunks)",
            [],
        )?
    } else {
        0
    };
    tx.commit()?;
    Ok(GcSummary {
        medium_deleted: n_medium as u64,
        low_deleted: n_low as u64,
        chunks_vec_orphans_cleared: n_vec_orphans as u64,
    })
}

/// Delete a single insight by integer id with chunks + chunks_vec cascade.
///
/// Guard: refuses to delete rows where `source_type IS NULL` (books-corpus
/// rows) — returns `Ok(None)` in that case so the CLI can emit a friendly
/// error message instead of silently truncating the books corpus.
pub fn insight_delete_with_summary(
    conn: &mut Connection,
    id: i64,
) -> Result<Option<DeleteByIdSummary>, rusqlite::Error> {
    use rusqlite::OptionalExtension;
    let tx = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
    // Probe: row exists AND is an insight.
    let row: Option<(String, Option<String>)> = tx
        .query_row(
            "SELECT source_path, source_type FROM documents WHERE id = ?1",
            rusqlite::params![id],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .optional()?;
    let (source_path, source_type) = match row {
        Some(t) => t,
        None => return Ok(None),
    };
    if source_type.is_none() {
        // Books-corpus row — refuse via the same Ok(None) signal so the
        // CLI can surface a different message ("not an insight").
        return Ok(None);
    }
    let chunks_removed: u64 = tx.query_row(
        "SELECT COUNT(*) FROM chunks WHERE doc_id = ?1",
        rusqlite::params![id],
        |row| row.get::<_, i64>(0).map(|n| n as u64),
    )?;
    // Clear chunks_vec rows for this doc's chunks BEFORE the cascade fires
    // — chunks_vec has no FK relation to chunks. Skip silently when the
    // virtual table is absent (v1 DB).
    let has_vec: bool = tx
        .query_row(
            "SELECT 1 FROM sqlite_master WHERE name='chunks_vec'",
            [],
            |_| Ok(true),
        )
        .unwrap_or(false);
    if has_vec {
        tx.execute(
            "DELETE FROM chunks_vec WHERE rowid IN (SELECT id FROM chunks WHERE doc_id = ?1)",
            rusqlite::params![id],
        )?;
    }
    tx.execute(
        "DELETE FROM documents WHERE id = ?1",
        rusqlite::params![id],
    )?;
    tx.commit()?;
    Ok(Some(DeleteByIdSummary {
        deleted_id: id,
        source_path,
        chunks_removed,
    }))
}

/// Document metadata snapshot used by `run_insight_search` to post-filter
/// ranked hits. Single lookup per unique `doc_id` per call (the caller
/// caches across hits since multiple chunks share a doc_id).
#[derive(Debug, Clone)]
pub struct DocMetadata {
    pub source_type: Option<String>,
    pub agent_name: Option<String>,
    pub salience: Option<String>,
    pub feature_slug: Option<String>,
    pub ingested_at: i64,
}

/// Fetch the filter-relevant metadata columns for a single document. Used
/// by the `insight search` post-filter pass — the canonical retrieval
/// engine (search.rs) is corpus-agnostic and doesn't know about insights
/// metadata, so we filter after ranking.
pub fn get_doc_metadata(
    conn: &Connection,
    doc_id: i64,
) -> Result<Option<DocMetadata>, rusqlite::Error> {
    use rusqlite::OptionalExtension;
    conn.query_row(
        "SELECT source_type, agent_name, salience, feature_slug, ingested_at \
         FROM documents WHERE id = ?1",
        rusqlite::params![doc_id],
        |r| {
            Ok(DocMetadata {
                source_type: r.get(0)?,
                agent_name: r.get(1)?,
                salience: r.get(2)?,
                feature_slug: r.get(3)?,
                ingested_at: r.get(4)?,
            })
        },
    )
    .optional()
}

/// Exact-sha dedup probe for the insights corpus.
///
/// Returns the existing `documents.id` when a row with the same `sha256`
/// AND `agent_name` was ingested at or after `cutoff_ingested_at` (a
/// unix-seconds timestamp; callers pass `now - 30 * 86400` for the
/// design-doc-specified 30-day window). `None` means no recent duplicate
/// — caller proceeds to upsert.
///
/// Intentionally narrower than a generic "is this body in the corpus?"
/// query: cross-agent collisions are NOT deduplicated (two agents
/// independently surfacing the same observation IS load-bearing signal).
pub fn find_recent_insight_by_sha(
    conn: &Connection,
    sha256: &str,
    agent_name: &str,
    cutoff_ingested_at: i64,
) -> Result<Option<i64>, rusqlite::Error> {
    use rusqlite::OptionalExtension;
    conn.query_row(
        "SELECT id FROM documents \
         WHERE sha256 = ?1 AND agent_name = ?2 AND ingested_at >= ?3 \
         ORDER BY ingested_at DESC LIMIT 1",
        rusqlite::params![sha256, agent_name, cutoff_ingested_at],
        |r| r.get(0),
    )
    .optional()
}

/// Replace all chunks for a document: delete prior rows then insert the new set.
/// FTS5 triggers fire for each row, so the FTS5 index stays in sync.
///
/// Each chunk carries optional `page_start`/`page_end` (1-indexed PDF page
/// numbers). For non-PDF sources callers pass `None` for both — these columns
/// were added in schema v2 and stay NULL for markdown/txt where pagination is
/// undefined. For PDFs the chunker emits one chunk per page, so
/// `page_start == page_end == page_no`.
pub fn replace_chunks(
    conn: &Connection,
    doc_id: i64,
    chunks: &[(usize, &str, Option<i64>, Option<i64>)],
) -> Result<(), rusqlite::Error> {
    conn.execute(
        "DELETE FROM chunks WHERE doc_id = ?1",
        rusqlite::params![doc_id],
    )?;
    let mut stmt = conn.prepare(
        "INSERT INTO chunks(doc_id, ord, text, page_start, page_end) \
         VALUES (?1, ?2, ?3, ?4, ?5)",
    )?;
    for (ord, text, page_start, page_end) in chunks {
        stmt.execute(rusqlite::params![
            doc_id,
            *ord as i64,
            *text,
            *page_start,
            *page_end
        ])?;
    }
    Ok(())
}

/// Replace all per-page text rows for a document. PDFs only — markdown/txt
/// callers MUST NOT invoke this (the chunker for those formats emits chunks
/// without page tracking and the `pages` table stays empty for them).
///
/// The unique `(doc_id, page_no)` constraint declared in `SCHEMA_V2_PAGES_TABLE`
/// prevents accidental dupes when re-ingesting; we DELETE first to keep the
/// "replace = atomic refresh" semantics that `replace_chunks` already follows.
pub fn replace_pages(
    conn: &Connection,
    doc_id: i64,
    pages: &[(i64, &str)],
) -> Result<(), rusqlite::Error> {
    conn.execute(
        "DELETE FROM pages WHERE doc_id = ?1",
        rusqlite::params![doc_id],
    )?;
    let mut stmt = conn.prepare(
        "INSERT INTO pages(doc_id, page_no, text) VALUES (?1, ?2, ?3)",
    )?;
    for (page_no, text) in pages {
        stmt.execute(rusqlite::params![doc_id, *page_no, *text])?;
    }
    Ok(())
}

/// Fetch the full extracted text of a single page by `(source_path, page_no)`.
/// Returns `Ok(None)` when no row matches — caller decides whether that means
/// "document not found", "page out of range", or "non-PDF source has no
/// pages" and renders the appropriate user-facing error.
pub fn get_page_by_source(
    conn: &Connection,
    source_path: &str,
    page_no: i64,
) -> Result<Option<PageRecord>, rusqlite::Error> {
    use rusqlite::OptionalExtension;
    conn.query_row(
        "SELECT d.id, d.source_path, p.page_no, p.text \
         FROM pages p JOIN documents d ON d.id = p.doc_id \
         WHERE d.source_path = ?1 AND p.page_no = ?2",
        rusqlite::params![source_path, page_no],
        |r| {
            Ok(PageRecord {
                doc_id: r.get(0)?,
                source_path: r.get(1)?,
                page_no: r.get(2)?,
                text: r.get(3)?,
            })
        },
    )
    .optional()
}

/// Fetch the full extracted text of a single page by `(doc_id, page_no)`.
pub fn get_page_by_id(
    conn: &Connection,
    doc_id: i64,
    page_no: i64,
) -> Result<Option<PageRecord>, rusqlite::Error> {
    use rusqlite::OptionalExtension;
    conn.query_row(
        "SELECT d.id, d.source_path, p.page_no, p.text \
         FROM pages p JOIN documents d ON d.id = p.doc_id \
         WHERE d.id = ?1 AND p.page_no = ?2",
        rusqlite::params![doc_id, page_no],
        |r| {
            Ok(PageRecord {
                doc_id: r.get(0)?,
                source_path: r.get(1)?,
                page_no: r.get(2)?,
                text: r.get(3)?,
            })
        },
    )
    .optional()
}

/// Returned by `get_page_by_source` / `get_page_by_id` — the full text of one
/// extracted PDF page plus identifying metadata, JSON-serializable for the
/// `page --json` output shape.
#[derive(Debug, Clone, serde::Serialize)]
pub struct PageRecord {
    pub doc_id: i64,
    pub source_path: String,
    pub page_no: i64,
    pub text: String,
}

/// Look up a document id by source_path. Used by the `page` subcommand to
/// disambiguate "document not found" from "page out of range" so the user
/// sees the more helpful of the two error messages.
pub fn lookup_doc_id(
    conn: &Connection,
    source_path: &str,
) -> Result<Option<i64>, rusqlite::Error> {
    use rusqlite::OptionalExtension;
    conn.query_row(
        "SELECT id FROM documents WHERE source_path = ?1",
        rusqlite::params![source_path],
        |r| r.get(0),
    )
    .optional()
}

/// Reverse of `lookup_doc_id`: id → source_path. The `page --by-id` path
/// uses this to render the source path in error messages without an extra
/// JOIN inside `get_page_by_id`.
pub fn lookup_document_by_id(
    conn: &Connection,
    id: i64,
) -> Result<Option<String>, rusqlite::Error> {
    use rusqlite::OptionalExtension;
    conn.query_row(
        "SELECT source_path FROM documents WHERE id = ?1",
        rusqlite::params![id],
        |r| r.get(0),
    )
    .optional()
}

/// Count how many `pages` rows exist for a doc — used to render
/// "page X of Y" errors. Returns 0 for non-PDF docs (they store no pages).
pub fn page_count(conn: &Connection, doc_id: i64) -> Result<i64, rusqlite::Error> {
    conn.query_row(
        "SELECT COUNT(*) FROM pages WHERE doc_id = ?1",
        rusqlite::params![doc_id],
        |r| r.get(0),
    )
}

/// Look up the prior `(mtime, sha256)` for a source path, if any.
pub fn lookup_document(
    conn: &Connection,
    source_path: &str,
) -> Result<Option<(i64, String)>, rusqlite::Error> {
    let row: Result<(i64, String), rusqlite::Error> = conn.query_row(
        "SELECT mtime, sha256 FROM documents WHERE source_path = ?1",
        rusqlite::params![source_path],
        |r| Ok((r.get(0)?, r.get(1)?)),
    );
    match row {
        Ok(t) => Ok(Some(t)),
        Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
        Err(e) => Err(e),
    }
}

/// List every ingested document with its chunk count, ordered by `ingested_at DESC`.
/// Used by `list` subcommand. SQL is a static literal.
pub fn list_documents(conn: &Connection) -> Result<Vec<DocumentSummary>, rusqlite::Error> {
    let mut stmt = conn.prepare(
        "SELECT d.source_path, \
                COUNT(c.id) AS chunk_count, \
                d.ingested_at \
         FROM documents d \
         LEFT JOIN chunks c ON c.doc_id = d.id \
         GROUP BY d.id \
         ORDER BY d.ingested_at DESC",
    )?;
    let rows = stmt.query_map([], |r| {
        Ok(DocumentSummary {
            source_path: r.get(0)?,
            chunk_count: r.get(1)?,
            ingested_at: r.get(2)?,
        })
    })?;
    let mut out = Vec::new();
    for r in rows {
        out.push(r?);
    }
    Ok(out)
}

/// Aggregate counts + schema_version + db_path for `status` subcommand.
pub fn status_summary(
    conn: &Connection,
    db_path: &Path,
) -> Result<StatusInfo, rusqlite::Error> {
    let schema_version: i64 =
        conn.query_row("SELECT version FROM schema_version", [], |r| r.get(0))?;
    let doc_count: i64 = conn.query_row("SELECT COUNT(*) FROM documents", [], |r| r.get(0))?;
    let chunk_count: i64 = conn.query_row("SELECT COUNT(*) FROM chunks", [], |r| r.get(0))?;
    Ok(StatusInfo {
        schema_version,
        doc_count,
        chunk_count,
        db_path: db_path.display().to_string(),
    })
}

/// FR-4.5 result shape for `delete --by-id`. Serialized to JSON in `output.rs`
/// as `{"deleted_id": N, "source_path": "...", "chunks_removed": M}`.
#[derive(Debug, Clone, serde::Serialize)]
pub struct DeleteByIdSummary {
    pub deleted_id: i64,
    pub source_path: String,
    pub chunks_removed: u64,
}

/// Delete a documents row by integer primary key, returning a summary of what
/// was removed (id + source_path + chunks_removed) per FR-4.5.
///
/// Wraps the multi-statement cascade in a `BEGIN IMMEDIATE` transaction per
/// FR-4.4 so the SELECT-source_path / SELECT-COUNT-chunks / DELETE-documents
/// triple is atomic against concurrent writers. The chunks rows cascade-delete
/// via the `chunks(doc_id) REFERENCES documents(id) ON DELETE CASCADE`
/// foreign-key constraint declared in `SCHEMA_V1`; FTS5 cleanup happens via
/// the `chunks_ad` AFTER-DELETE trigger on each chunk row removed.
///
/// Returns:
///   - `Ok(Some(summary))` — document existed and was deleted.
///   - `Ok(None)` — no documents row with that id; transaction rolls back
///     (implicit on drop without commit).
///   - `Err(...)` — SQL error during the probe or delete; transaction rolls
///     back.
pub fn delete_by_id_with_summary(
    conn: &mut Connection,
    id: i64,
) -> Result<Option<DeleteByIdSummary>, rusqlite::Error> {
    use rusqlite::OptionalExtension;

    // BEGIN IMMEDIATE per FR-4.4 — same transaction discipline as ingest.
    let tx = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;

    let source_path: Option<String> = tx
        .query_row(
            "SELECT source_path FROM documents WHERE id = ?1",
            rusqlite::params![id],
            |row| row.get(0),
        )
        .optional()?;
    let source_path = match source_path {
        Some(s) => s,
        None => {
            // No row to delete; rollback is implicit on drop without commit.
            return Ok(None);
        }
    };

    let chunks_removed: u64 = tx.query_row(
        "SELECT COUNT(*) FROM chunks WHERE doc_id = ?1",
        rusqlite::params![id],
        |row| row.get::<_, i64>(0).map(|n| n as u64),
    )?;

    tx.execute(
        "DELETE FROM documents WHERE id = ?1",
        rusqlite::params![id],
    )?;
    // chunks rows cascade-delete via FOREIGN KEY ... ON DELETE CASCADE on
    // chunks.doc_id (declared in SCHEMA_V1); FTS5 stays in sync via the
    // chunks_ad AFTER DELETE trigger on each chunk row removed.

    tx.commit()?;
    Ok(Some(DeleteByIdSummary {
        deleted_id: id,
        source_path,
        chunks_removed,
    }))
}

// ===========================================================================
// Schema v3 — page-range fetch helpers (Slice 12 of vector-retrieval-backend).
// Built on top of the `pages` table populated by `replace_pages` (above) which
// is the canonical insert path; these helpers only READ.
// ===========================================================================

/// One page of raw extracted text from a source document. `page_no` is
/// 1-indexed per the pdfium convention (pages numbered 1..N where N is the
/// PDF's reported page count, independent of any "printed" numbering like
/// Roman numerals for preface).
#[derive(Debug, Clone)]
pub struct PageRow {
    pub page_no: i64,
    pub text: String,
}

/// Resolve a user-facing doc identifier to `(documents.id, source_path,
/// total_pages)`. Accepts either an integer id (parsed as `documents.id`
/// directly) or a basename string matched against `documents.source_path` so
/// the LLM can request pages from a specific book by its printed filename.
/// `total_pages` is derived via `MAX(page_no) FROM pages` since the schema
/// keeps it on the pages table rather than denormalizing onto `documents`.
pub fn resolve_doc_id(
    conn: &Connection,
    identifier: &str,
) -> Result<Option<(i64, String, Option<i64>)>, rusqlite::Error> {
    use rusqlite::OptionalExtension;
    let row: Option<(i64, String)> = if let Ok(id) = identifier.parse::<i64>() {
        conn.query_row(
            "SELECT id, source_path FROM documents WHERE id = ?1",
            rusqlite::params![id],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .optional()?
    } else {
        conn.query_row(
            "SELECT id, source_path FROM documents \
             WHERE source_path = ?1 OR source_path LIKE ?2 \
             ORDER BY ingested_at DESC LIMIT 1",
            rusqlite::params![identifier, format!("%/{identifier}")],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .optional()?
    };
    if let Some((doc_id, source_path)) = row {
        let total: Option<i64> = conn
            .query_row(
                "SELECT MAX(page_no) FROM pages WHERE doc_id = ?1",
                rusqlite::params![doc_id],
                |r| r.get::<_, Option<i64>>(0),
            )
            .optional()?
            .flatten();
        Ok(Some((doc_id, source_path, total)))
    } else {
        Ok(None)
    }
}

/// Fetch a page range `[lo..=hi]` (1-indexed, inclusive). Empty result means
/// no page in that range has been populated for the document (either the
/// range is out of bounds OR the document has no `pages` rows yet — caller
/// disambiguates via `resolve_doc_id`'s `total_pages`).
pub fn fetch_page_range(
    conn: &Connection,
    doc_id: i64,
    lo: i64,
    hi: i64,
) -> Result<Vec<PageRow>, rusqlite::Error> {
    let mut stmt = conn.prepare(
        "SELECT page_no, text FROM pages \
         WHERE doc_id = ?1 AND page_no BETWEEN ?2 AND ?3 \
         ORDER BY page_no",
    )?;
    let rows = stmt.query_map(rusqlite::params![doc_id, lo, hi], |r| {
        Ok(PageRow {
            page_no: r.get(0)?,
            text: r.get(1)?,
        })
    })?;
    let mut out = Vec::new();
    for r in rows {
        out.push(r?);
    }
    Ok(out)
}

/// User-level chat.db path: `$HOME/.claude/knowledge/chat.db`.
///
/// Slice 3 (agent-chat-daemon) — chat is global to the user, NOT per project,
/// per architect OQ-ACD-4. The daemon owns this file; the CLI `chat list` /
/// `chat threads` subcommands and tests open it directly via rusqlite.
///
/// On HOME-unset (extremely unusual), falls back to USERPROFILE (Windows)
/// then `/tmp` — the same fallback chain `reap_on_boot_stub` already uses
/// so behavior is consistent across the daemon and the CLI.
pub fn user_level_chat_db_path() -> std::path::PathBuf {
    let home = std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .unwrap_or_else(|| std::ffi::OsString::from("/tmp"));
    std::path::PathBuf::from(home)
        .join(".claude")
        .join("knowledge")
        .join("chat.db")
}

/// Delete a documents row by exact `source_path` string. Returns rows deleted.
///
/// SECURITY: callers MUST canonicalize-and-prefix-check the `source_path`
/// argument against the project root BEFORE invoking this function — see the
/// Slice 1 cross-slice flag in `.claude/scratchpad.md`. This function does
/// NOT perform that check itself; it is purely a parameterized DELETE.
pub fn delete_by_source_path(
    conn: &Connection,
    source_path: &str,
) -> Result<u64, rusqlite::Error> {
    let n = conn.execute(
        "DELETE FROM documents WHERE source_path = ?1",
        rusqlite::params![source_path],
    )?;
    Ok(n as u64)
}
