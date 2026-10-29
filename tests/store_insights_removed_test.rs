//! Schema v6 — the agent-insights objects of v4/v5 leave the knowledge index.
//!
//! Insights became the project journal (`journal.rs`), so an index opened by
//! this binary drops the v4 metadata columns, the v5 `category` /
//! `project_slug` columns and the `insight_tags` table, and keeps every books
//! row with its chunks.

use claudebase::store::{open_or_init_v2, validate_schema, SCHEMA_VERSION};
use rusqlite::Connection;
use tempfile::TempDir;

const INSIGHT_COLUMNS: [&str; 8] = [
    "source_type", "agent_name", "session_id", "feature_slug",
    "salience", "parent_artifact", "category", "project_slug",
];

/// An index as v4 or v5 left it: the v3 shape, the insight columns and
/// indexes, the v5 tag table when `v5`, one books row with a chunk.
fn old_index(v5: bool) -> (TempDir, std::path::PathBuf) {
    let tmp = TempDir::new().expect("tempdir");
    let path = tmp.path().join("index.db");
    let conn = open_or_init_v2(&path).expect("v3 shape");
    conn.execute_batch(
        "ALTER TABLE documents ADD COLUMN source_type     TEXT; \
         ALTER TABLE documents ADD COLUMN agent_name      TEXT; \
         ALTER TABLE documents ADD COLUMN session_id      TEXT; \
         ALTER TABLE documents ADD COLUMN feature_slug    TEXT; \
         ALTER TABLE documents ADD COLUMN salience        TEXT; \
         ALTER TABLE documents ADD COLUMN parent_artifact TEXT; \
         CREATE INDEX idx_documents_source_type ON documents(source_type); \
         CREATE INDEX idx_documents_agent_name  ON documents(agent_name); \
         CREATE INDEX idx_documents_feature     ON documents(feature_slug); \
         CREATE INDEX idx_documents_salience    ON documents(salience);",
    )
    .expect("v4 columns");
    if v5 {
        conn.execute_batch(
            "ALTER TABLE documents ADD COLUMN category     TEXT; \
             ALTER TABLE documents ADD COLUMN project_slug TEXT; \
             CREATE INDEX idx_documents_category ON documents(category); \
             CREATE TABLE insight_tags (doc_id INTEGER NOT NULL REFERENCES documents(id) ON DELETE CASCADE, \
                                        tag TEXT NOT NULL, UNIQUE(doc_id, tag)); \
             CREATE INDEX idx_insight_tags_tag ON insight_tags(tag);",
        )
        .expect("v5 objects");
    }
    conn.execute("UPDATE schema_version SET version = ?1", [if v5 { 5i64 } else { 4 }])
        .expect("stamp");
    conn.execute(
        "INSERT INTO documents(source_path, mtime, sha256, ingested_at) VALUES ('/books/a.pdf', 0, 'sha', 0)",
        [],
    )
    .expect("books row");
    conn.execute("INSERT INTO chunks(doc_id, ord, text) VALUES (1, 0, 'the quick brown fox')", [])
        .expect("chunk");
    drop(conn);
    (tmp, path)
}

fn col_exists(conn: &Connection, col: &str) -> bool {
    conn.query_row("SELECT 1 FROM pragma_table_info('documents') WHERE name = ?1", [col], |_| Ok(true))
        .unwrap_or(false)
}

fn object_exists(conn: &Connection, name: &str) -> bool {
    conn.query_row("SELECT 1 FROM sqlite_master WHERE name = ?1", [name], |_| Ok(true))
        .unwrap_or(false)
}

fn version(conn: &Connection) -> i64 {
    conn.query_row("SELECT version FROM schema_version", [], |r| r.get(0)).expect("version")
}

fn assert_no_insights(conn: &Connection) {
    for col in INSIGHT_COLUMNS {
        assert!(!col_exists(conn, col), "documents.{col} should be gone");
    }
    for name in ["insight_tags", "idx_insight_tags_tag", "idx_documents_category", "idx_documents_salience"] {
        assert!(!object_exists(conn, name), "{name} should be gone");
    }
}

#[test]
fn a_fresh_index_has_no_insight_objects() {
    let tmp = TempDir::new().unwrap();
    let conn = open_or_init_v2(&tmp.path().join("index.db")).expect("open");
    assert_eq!(version(&conn), SCHEMA_VERSION);
    assert_no_insights(&conn);
    assert!(object_exists(&conn, "pages") && object_exists(&conn, "chunks_vec"));
    validate_schema(&conn).expect("valid");
}

#[test]
fn a_v4_or_v5_index_drops_insights_and_keeps_the_books() {
    for v5 in [false, true] {
        let (_tmp, path) = old_index(v5);
        let conn = open_or_init_v2(&path).expect("migrate");
        assert_eq!(version(&conn), SCHEMA_VERSION, "v5={v5}");
        assert_no_insights(&conn);
        assert!(object_exists(&conn, "pages"), "v3 objects converge too");
        let docs: i64 = conn.query_row("SELECT COUNT(*) FROM documents", [], |r| r.get(0)).unwrap();
        let hits: i64 = conn
            .query_row("SELECT COUNT(*) FROM chunks_fts WHERE chunks_fts MATCH 'fox'", [], |r| r.get(0))
            .unwrap();
        assert_eq!((docs, hits), (1, 1), "the books row and its chunk survive (v5={v5})");
        assert_eq!(conn.query_row("PRAGMA integrity_check", [], |r| r.get::<_, String>(0)).unwrap(), "ok");
        validate_schema(&conn).expect("valid");
    }
}

#[test]
fn reopening_a_v6_index_changes_nothing() {
    let (_tmp, path) = old_index(true);
    drop(open_or_init_v2(&path).expect("migrate"));
    let conn = open_or_init_v2(&path).expect("reopen");
    assert_eq!(version(&conn), SCHEMA_VERSION);
    assert_no_insights(&conn);
}
