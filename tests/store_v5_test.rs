//! insights-hybrid-corpus Slice 1 — schema v5 migration tests.
//!
//! Coverage (QA cases TC-IHC-13.1..13.8):
//! - TC-IHC-13.6: fresh db → schema_version=5; category/project_slug columns;
//!   insight_tags table + both indexes
//! - TC-IHC-13.1: v4 fixture db opened by v5 binary → version=5; columns + table
//! - TC-IHC-13.2: v4→v5 backfill — all agent:% insight rows get
//!   category='project', non-null project_slug, >=1 insight_tags row
//! - TC-IHC-13.3: books-corpus rows (source_path NOT LIKE 'agent:%') untouched —
//!   category IS NULL, zero insight_tags rows
//! - TC-IHC-13.4: both indexes (idx_insight_tags_tag, idx_documents_category) exist
//! - TC-IHC-13.5: idempotent re-open at v5 — version still 5, no duplicate column
//! - TC-IHC-13.7: validate_schema accepts version=5, rejects version=6 (store-level;
//!   see test doc note re: the CLI-level stderr message, owned by a main.rs slice)
//! - TC-IHC-13.8: v4 insight rows with feature_slug NULL → tag='untagged'
//!
//! Fixtures are hermetic — built in-test, never depending on any external db file.

use claudebase::store::{open_or_init, open_or_init_v2, validate_schema};
use rusqlite::Connection;
use tempfile::TempDir;

fn fresh_db_path() -> (TempDir, std::path::PathBuf) {
    let tmp = TempDir::new().expect("tempdir");
    let path = tmp.path().join("insights.db");
    (tmp, path)
}

/// Build a hermetic v4 `insights.db` fixture under a project-named directory so
/// the v5 backfill can derive a non-empty `project_slug` from the path.
///
/// Layout: `<tmp>/<proj>/.claude/knowledge/insights.db` so `derive_project_slug`
/// resolves the slug to `<proj>`.
///
/// We start from `open_or_init` (SCHEMA_V1 — documents/chunks/chunks_fts/
/// schema_version + FTS5 triggers), then ALTER `documents` to add the six v4
/// agent-insights metadata columns, then stamp version=4. We do NOT create
/// chunks_vec / pages — the v5 migration only touches `documents` +
/// `insight_tags`, and `validate_schema` only requires the four SCHEMA_V1
/// objects. Insight rows use `source_path LIKE 'agent:%'`; one books row uses a
/// filesystem-style path.
///
/// Returns (TempDir guard, db path, count of inserted agent:% insight rows).
fn build_v4_fixture(proj_name: &str) -> (TempDir, std::path::PathBuf, usize) {
    let tmp = TempDir::new().expect("tempdir");
    let db_path = tmp
        .path()
        .join(proj_name)
        .join(".claude")
        .join("knowledge")
        .join("insights.db");
    // open_or_init creates parent dirs + SCHEMA_V1 and flips WAL.
    let conn = open_or_init(&db_path).expect("v1 init for v4 fixture");
    // Add the six v4 agent-insights metadata columns to documents.
    conn.execute_batch(
        "ALTER TABLE documents ADD COLUMN source_type     TEXT; \
         ALTER TABLE documents ADD COLUMN agent_name      TEXT; \
         ALTER TABLE documents ADD COLUMN session_id      TEXT; \
         ALTER TABLE documents ADD COLUMN feature_slug    TEXT; \
         ALTER TABLE documents ADD COLUMN salience        TEXT; \
         ALTER TABLE documents ADD COLUMN parent_artifact TEXT; \
         CREATE INDEX IF NOT EXISTS idx_documents_source_type ON documents(source_type); \
         CREATE INDEX IF NOT EXISTS idx_documents_agent_name  ON documents(agent_name); \
         CREATE INDEX IF NOT EXISTS idx_documents_feature     ON documents(feature_slug); \
         CREATE INDEX IF NOT EXISTS idx_documents_salience    ON documents(salience);",
    )
    .expect("apply v4 columns");
    // Stamp schema_version=4 (open_or_init does NOT stamp the version row).
    conn.execute(
        "INSERT INTO schema_version(version) VALUES (?1)",
        rusqlite::params![4i64],
    )
    .expect("stamp v4");

    // Insert 4 insight rows (source_path LIKE 'agent:%'):
    //   - 3 with a non-empty feature_slug
    //   - 1 with feature_slug = NULL (exercises the 'untagged' COALESCE branch)
    // plus 1 books-corpus row (source_path = a filesystem path).
    let insight_rows = [
        ("agent:planner:s1:feat-alpha:aaaa", Some("feat-alpha")),
        ("agent:architect:s1:feat-beta:bbbb", Some("feat-beta")),
        ("agent:reflection:s1:feat-alpha:cccc", Some("feat-alpha")),
        ("agent:red-team:s1::dddd", None), // feature_slug NULL → 'untagged'
    ];
    for (sp, feat) in insight_rows {
        conn.execute(
            "INSERT INTO documents(source_path, mtime, sha256, ingested_at, source_type, feature_slug) \
             VALUES (?1, 0, 'sha', 0, 'agent-learned', ?2)",
            rusqlite::params![sp, feat],
        )
        .expect("insert insight row");
    }
    // Books-corpus row — must stay untouched (category NULL, no tags).
    conn.execute(
        "INSERT INTO documents(source_path, mtime, sha256, ingested_at) \
         VALUES ('/proj/.claude/knowledge/sources/book.pdf', 0, 'sha', 0)",
        [],
    )
    .expect("insert books row");
    drop(conn);
    (tmp, db_path, insight_rows.len())
}

fn col_exists(conn: &Connection, table: &str, col: &str) -> bool {
    conn.query_row(
        "SELECT 1 FROM pragma_table_info(?1) WHERE name = ?2",
        rusqlite::params![table, col],
        |_| Ok(true),
    )
    .unwrap_or(false)
}

fn schema_version(conn: &Connection) -> i64 {
    conn.query_row("SELECT version FROM schema_version", [], |r| r.get(0))
        .expect("schema_version row")
}

fn table_exists(conn: &Connection, name: &str) -> bool {
    conn.query_row(
        "SELECT 1 FROM sqlite_master WHERE type='table' AND name=?1",
        rusqlite::params![name],
        |_| Ok(true),
    )
    .unwrap_or(false)
}

fn index_exists(conn: &Connection, name: &str) -> bool {
    conn.query_row(
        "SELECT 1 FROM sqlite_master WHERE type='index' AND name=?1",
        rusqlite::params![name],
        |_| Ok(true),
    )
    .unwrap_or(false)
}

// ----------------------------------------------------------------------------
// TC-IHC-13.6 — fresh db → v5 stamp + columns + table + indexes
// ----------------------------------------------------------------------------
#[test]
fn tc_13_6_fresh_db_stamps_v5_with_columns_table_indexes() {
    let (_tmp, path) = fresh_db_path();
    let conn = open_or_init_v2(&path).expect("open_or_init_v2 fresh");
    assert_eq!(schema_version(&conn), 5, "fresh db should be at v5");
    assert!(col_exists(&conn, "documents", "category"), "category column");
    assert!(
        col_exists(&conn, "documents", "project_slug"),
        "project_slug column"
    );
    assert!(table_exists(&conn, "insight_tags"), "insight_tags table");
    assert!(
        index_exists(&conn, "idx_documents_category"),
        "idx_documents_category"
    );
    assert!(
        index_exists(&conn, "idx_insight_tags_tag"),
        "idx_insight_tags_tag"
    );
}

// ----------------------------------------------------------------------------
// TC-IHC-13.1 — v4 fixture opened by v5 binary → version=5; columns + table
// ----------------------------------------------------------------------------
#[test]
fn tc_13_1_v4_fixture_migrates_to_v5() {
    let (_tmp, path, _) = build_v4_fixture("tc131proj");
    let conn = open_or_init_v2(&path).expect("open v4 fixture → migrate to v5");
    assert_eq!(schema_version(&conn), 5, "post-open version should be 5");
    assert!(col_exists(&conn, "documents", "category"));
    assert!(col_exists(&conn, "documents", "project_slug"));
    assert!(table_exists(&conn, "insight_tags"));
}

// ----------------------------------------------------------------------------
// TC-IHC-13.2 — backfill: all agent:% insight rows get category='project',
// non-null project_slug, >=1 insight_tags row
// ----------------------------------------------------------------------------
#[test]
fn tc_13_2_backfill_all_insight_rows() {
    let (_tmp, path, n) = build_v4_fixture("tc132proj");
    let conn = open_or_init_v2(&path).expect("migrate to v5");

    let category_project: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM documents WHERE source_path LIKE 'agent:%' AND category='project'",
            [],
            |r| r.get(0),
        )
        .expect("count category=project");
    assert_eq!(category_project, n as i64, "all insight rows category='project'");

    let non_null_slug: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM documents WHERE source_path LIKE 'agent:%' AND project_slug IS NOT NULL",
            [],
            |r| r.get(0),
        )
        .expect("count non-null slug");
    assert_eq!(non_null_slug, n as i64, "all insight rows have project_slug");

    // project_slug derived from the path basename (the project dir name).
    let slug: String = conn
        .query_row(
            "SELECT project_slug FROM documents WHERE source_path LIKE 'agent:%' LIMIT 1",
            [],
            |r| r.get(0),
        )
        .expect("read slug");
    assert_eq!(slug, "tc132proj", "project_slug == project dir basename");

    let docs_with_tag: i64 = conn
        .query_row(
            "SELECT COUNT(DISTINCT doc_id) FROM insight_tags \
             WHERE doc_id IN (SELECT id FROM documents WHERE source_path LIKE 'agent:%')",
            [],
            |r| r.get(0),
        )
        .expect("count tagged insight docs");
    assert_eq!(docs_with_tag, n as i64, "each insight row has >=1 tag");
}

// ----------------------------------------------------------------------------
// TC-IHC-13.3 — books-corpus rows untouched (category NULL, zero tags)
// ----------------------------------------------------------------------------
#[test]
fn tc_13_3_books_rows_untouched() {
    let (_tmp, path, _) = build_v4_fixture("tc133proj");
    let conn = open_or_init_v2(&path).expect("migrate to v5");

    let books_with_category: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM documents WHERE source_path NOT LIKE 'agent:%' AND category IS NOT NULL",
            [],
            |r| r.get(0),
        )
        .expect("count books with category");
    assert_eq!(books_with_category, 0, "books rows keep category=NULL");

    let books_tags: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM insight_tags t JOIN documents d ON t.doc_id=d.id \
             WHERE d.source_path NOT LIKE 'agent:%'",
            [],
            |r| r.get(0),
        )
        .expect("count books tags");
    assert_eq!(books_tags, 0, "books rows get zero insight_tags");
}

// ----------------------------------------------------------------------------
// TC-IHC-13.4 — both indexes exist after migration
// ----------------------------------------------------------------------------
#[test]
fn tc_13_4_both_indexes_exist() {
    let (_tmp, path, _) = build_v4_fixture("tc134proj");
    let conn = open_or_init_v2(&path).expect("migrate to v5");
    let count: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM sqlite_master WHERE type='index' \
             AND name IN ('idx_insight_tags_tag','idx_documents_category')",
            [],
            |r| r.get(0),
        )
        .expect("count indexes");
    assert_eq!(count, 2, "both v5 indexes present");
}

// ----------------------------------------------------------------------------
// TC-IHC-13.5 — idempotent re-open at v5 (no duplicate column, version still 5)
// ----------------------------------------------------------------------------
#[test]
fn tc_13_5_idempotent_reopen_at_v5() {
    let (_tmp, path) = fresh_db_path();
    {
        let _c = open_or_init_v2(&path).expect("first open → v5");
    }
    let conn = open_or_init_v2(&path).expect("second open idempotent");
    assert_eq!(schema_version(&conn), 5, "version still 5 after re-open");
    let category_cols: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM pragma_table_info('documents') WHERE name='category'",
            [],
            |r| r.get(0),
        )
        .expect("count category cols");
    assert_eq!(category_cols, 1, "no duplicate category column");
}

// Idempotent re-open of a *migrated* v4→v5 fixture (a second open after the
// migration must not double-apply the backfill or error on ALTER re-run).
#[test]
fn tc_13_5b_idempotent_reopen_after_v4_migration() {
    let (_tmp, path, n) = build_v4_fixture("tc135bproj");
    {
        let _c = open_or_init_v2(&path).expect("v4 → v5 migrate");
    }
    let conn = open_or_init_v2(&path).expect("re-open migrated db");
    assert_eq!(schema_version(&conn), 5);
    // Backfill is not double-applied: still exactly one tag-per-doc baseline.
    let tagged_docs: i64 = conn
        .query_row(
            "SELECT COUNT(DISTINCT doc_id) FROM insight_tags",
            [],
            |r| r.get(0),
        )
        .expect("count tagged docs");
    assert_eq!(tagged_docs, n as i64, "tag set stable across re-open");
}

// ----------------------------------------------------------------------------
// TC-IHC-13.7 — validate_schema accepts v5, rejects v6 (store-level).
//
// NOTE (Protocol-3 inbound contradiction surfaced in the report): the QA case
// TC-IHC-13.7 expects the *CLI* stderr literal `error: unsupported schema
// version` for a v6 db. No such message exists in the codebase — main.rs maps
// any validate_schema failure to `error: index database invalid; re-ingest
// required` (main.rs:2320). Emitting the QA-specified message is a main.rs
// change owned by a later slice (Slice 1 is store.rs-only). This test asserts
// the store-level contract that IS in Slice 1's scope: validate_schema accepts
// v5 and rejects v6.
// ----------------------------------------------------------------------------
#[test]
fn tc_13_7_validate_schema_accepts_v5_rejects_v6() {
    let (_tmp, path) = fresh_db_path();
    let conn = open_or_init_v2(&path).expect("fresh v5 db");
    assert!(
        validate_schema(&conn).is_ok(),
        "validate_schema accepts version=5"
    );

    // Stamp version=6 and confirm validate_schema now rejects it.
    conn.execute("UPDATE schema_version SET version = 6", [])
        .expect("stamp v6");
    assert!(
        validate_schema(&conn).is_err(),
        "validate_schema rejects unknown version=6"
    );
}

// ----------------------------------------------------------------------------
// TC-IHC-13.8 — v4 insight rows with feature_slug NULL → tag='untagged'
// ----------------------------------------------------------------------------
#[test]
fn tc_13_8_null_feature_slug_gets_untagged() {
    let (_tmp, path, _) = build_v4_fixture("tc138proj");
    let conn = open_or_init_v2(&path).expect("migrate to v5");
    let tags: Vec<String> = {
        let mut stmt = conn
            .prepare(
                "SELECT t.tag FROM insight_tags t JOIN documents d ON t.doc_id=d.id \
                 WHERE d.feature_slug IS NULL",
            )
            .expect("prepare");
        let rows = stmt
            .query_map([], |r| r.get::<_, String>(0))
            .expect("query")
            .filter_map(Result::ok)
            .collect::<Vec<_>>();
        rows
    };
    assert!(!tags.is_empty(), "null-feature_slug doc has a tag");
    assert!(
        tags.iter().all(|t| t == "untagged"),
        "all null-feature_slug tags are 'untagged', got {tags:?}"
    );

    // And a non-null feature_slug row gets that slug verbatim as its tag.
    let alpha_tag: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM insight_tags WHERE tag='feat-alpha'",
            [],
            |r| r.get(0),
        )
        .expect("count feat-alpha tags");
    assert!(alpha_tag >= 1, "feature_slug used verbatim as default tag");
}
