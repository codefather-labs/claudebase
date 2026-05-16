//! Slice 3 (agent-insights-base) end-to-end CLI `insight` tests.
//!
//! Coverage:
//! - `insight create` happy path: positional body writes one doc + ≥1 chunk
//!   to insights.db with v4 metadata populated
//! - `insight create` reads stdin when body arg omitted
//! - `insight create` exact-sha dedup for same (agent, sha256) within 30 days
//! - `insight create` cross-agent same body is NOT deduped (intentional)
//! - `insight create` rejects empty body (exit 2)
//! - `insight create` does NOT touch index.db (books corpus isolation)
//! - `insight create` source_path encodes (agent, session, feature, sha-prefix)
//! - `insight search` retrieves via lexical mode on insights.db
//! - `insight list` paginates 10-per-page newest-first; `--offset` advances
//! - `insight list` filters by type / agent / salience
//! - `insight random` returns one row when corpus non-empty
//! - `insight random` exits 1 on empty corpus
//! - `insight get <id>` returns the full record
//! - `insight get <sha-prefix>` matches via LIKE
//! - `insight get <unknown>` exits 1
//! - `insight get` rejects too-short / non-hex identifiers (exit 2)

use assert_cmd::Command;
use rusqlite::params;
use std::fs;
use std::path::{Path, PathBuf};

fn bin() -> Command {
    Command::cargo_bin("claudebase").expect("binary built")
}

fn fresh_project() -> tempfile::TempDir {
    let tmp = tempfile::tempdir().expect("tempdir");
    fs::create_dir_all(tmp.path().join(".claude/knowledge")).expect("mkdir .claude/knowledge");
    tmp
}

fn insights_db(root: &Path) -> PathBuf {
    root.join(".claude/knowledge/insights.db")
}

fn open_db(db_path: &Path) -> rusqlite::Connection {
    rusqlite::Connection::open(db_path).expect("open db")
}

fn create_insight(
    project: &Path,
    body: &str,
    kind: &str,
    agent: &str,
    extra: &[&str],
) -> assert_cmd::assert::Assert {
    let mut cmd = bin();
    cmd.current_dir(project).args(["insight", "create", body, "--type", kind, "--agent", agent]);
    for e in extra {
        cmd.arg(e);
    }
    cmd.assert()
}

// ---------------------------------------------------------------------------
// `insight create` — happy path + metadata + isolation.
// ---------------------------------------------------------------------------

#[test]
fn create_writes_one_doc_with_v4_metadata() {
    let tmp = fresh_project();
    create_insight(
        tmp.path(),
        "kafka exactly-once semantics break on rebalance during transaction commit",
        "agent-learned",
        "reflection",
        &["--feature", "agent-insights-base", "--salience", "high"],
    )
    .success();

    let db = insights_db(tmp.path());
    assert!(db.exists(), "insights.db should exist at {}", db.display());
    let conn = open_db(&db);
    let (docs, chunks): (i64, i64) = (
        conn.query_row("SELECT COUNT(*) FROM documents", [], |r| r.get(0)).unwrap(),
        conn.query_row("SELECT COUNT(*) FROM chunks", [], |r| r.get(0)).unwrap(),
    );
    assert_eq!(docs, 1);
    assert!(chunks >= 1);
    let (st, ag, sal): (String, String, String) = conn
        .query_row(
            "SELECT source_type, agent_name, salience FROM documents LIMIT 1",
            [],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )
        .unwrap();
    assert_eq!(st, "agent-learned");
    assert_eq!(ag, "reflection");
    assert_eq!(sal, "high");
}

#[test]
fn create_reads_stdin_when_body_arg_omitted() {
    let tmp = fresh_project();
    bin()
        .current_dir(tmp.path())
        .args(["insight", "create", "--type", "reflection-observation", "--agent", "reflection"])
        .write_stdin("piped body from stdin")
        .assert()
        .success();
    let conn = open_db(&insights_db(tmp.path()));
    let n: i64 = conn.query_row("SELECT COUNT(*) FROM documents", [], |r| r.get(0)).unwrap();
    assert_eq!(n, 1);
}

#[test]
fn create_exact_sha_dedup_keeps_doc_count_at_one() {
    let tmp = fresh_project();
    for _ in 0..2 {
        create_insight(
            tmp.path(),
            "PRD §3.2 says X but plan slice 4 says not-X",
            "consolidator-drift",
            "consolidator",
            &[],
        )
        .success();
    }
    let conn = open_db(&insights_db(tmp.path()));
    let n: i64 = conn.query_row("SELECT COUNT(*) FROM documents", [], |r| r.get(0)).unwrap();
    assert_eq!(n, 1);
}

#[test]
fn create_emits_deduped_status_on_second_call() {
    let tmp = fresh_project();
    create_insight(tmp.path(), "duplicate body", "agent-learned", "x", &["--json"]).success();
    let assert = create_insight(
        tmp.path(),
        "duplicate body",
        "agent-learned",
        "x",
        &["--json"],
    )
    .success();
    let stdout = String::from_utf8_lossy(&assert.get_output().stdout).to_string();
    assert!(
        stdout.contains("\"status\": \"deduped\"") || stdout.contains("\"status\":\"deduped\""),
        "expected deduped status; got:\n{stdout}"
    );
}

#[test]
fn create_cross_agent_same_body_is_not_deduped() {
    let tmp = fresh_project();
    for agent in ["planner", "verifier"] {
        create_insight(
            tmp.path(),
            "RRF k=60 outperforms BM25-alone on cross-lingual retrieval",
            "agent-learned",
            agent,
            &[],
        )
        .success();
    }
    let conn = open_db(&insights_db(tmp.path()));
    let n: i64 = conn.query_row("SELECT COUNT(*) FROM documents", [], |r| r.get(0)).unwrap();
    assert_eq!(n, 2, "different agents should produce separate docs");
}

#[test]
fn create_rejects_empty_body_with_exit_2() {
    let tmp = fresh_project();
    bin()
        .current_dir(tmp.path())
        .args([
            "insight", "create", "   \n\t  ",
            "--type", "agent-learned", "--agent", "x",
        ])
        .assert()
        .failure()
        .code(2);
}

#[test]
fn create_does_not_create_index_db() {
    let tmp = fresh_project();
    create_insight(tmp.path(), "isolation test", "agent-learned", "x", &[]).success();
    let books = tmp.path().join(".claude/knowledge/index.db");
    assert!(!books.exists(), "create must not touch index.db");
}

#[test]
fn create_source_path_encodes_metadata_segments() {
    let tmp = fresh_project();
    create_insight(
        tmp.path(),
        "source_path must encode (agent, session, feature, sha-prefix)",
        "agent-learned",
        "planner",
        &["--session", "sess-abc", "--feature", "agent-insights-base"],
    )
    .success();
    let conn = open_db(&insights_db(tmp.path()));
    let src: String = conn
        .query_row(
            "SELECT source_path FROM documents WHERE agent_name = ?1",
            params!["planner"],
            |r| r.get(0),
        )
        .unwrap();
    assert!(
        src.starts_with("agent:planner:sess-abc:agent-insights-base:"),
        "expected synthetic prefix; got `{src}`"
    );
}

// ---------------------------------------------------------------------------
// `insight search` — round-trip via lexical mode.
// ---------------------------------------------------------------------------

#[test]
fn search_returns_lexical_hit_for_written_insight() {
    let tmp = fresh_project();
    create_insight(
        tmp.path(),
        "claudebase preserves cross session memory for SDLC agents",
        "agent-learned",
        "planner",
        &[],
    )
    .success();
    // FTS5 unicode61 tokenizer treats `-` as a negation prefix; we use
    // hyphen-free terms in the query string so the test exercises lexical
    // retrieval rather than FTS5 syntax handling (which is upstream search
    // territory, not insight-specific).
    let assert = bin()
        .current_dir(tmp.path())
        .args(["insight", "search", "memory agents", "--mode", "lexical", "--json"])
        .assert()
        .success();
    let stdout = String::from_utf8_lossy(&assert.get_output().stdout).to_string();
    assert!(
        stdout.contains("memory") || stdout.contains("agents"),
        "expected lexical hit; got:\n{stdout}"
    );
}

// ---------------------------------------------------------------------------
// `insight list` — pagination, defaults, filters.
// ---------------------------------------------------------------------------

#[test]
fn list_default_page_size_is_ten_newest_first() {
    let tmp = fresh_project();
    // 12 distinct bodies (cross-agent so each survives dedup as a separate doc).
    for i in 0..12 {
        let agent = if i % 2 == 0 { "planner" } else { "verifier" };
        create_insight(
            tmp.path(),
            &format!("insight body number {i:02}"),
            "agent-learned",
            agent,
            &[],
        )
        .success();
    }
    let assert = bin()
        .current_dir(tmp.path())
        .args(["insight", "list", "--offset", "0", "--json"])
        .assert()
        .success();
    let stdout = String::from_utf8_lossy(&assert.get_output().stdout).to_string();
    let v: serde_json::Value = serde_json::from_str(&stdout).expect("valid json");
    let returned = v["returned"].as_i64().unwrap();
    let total = v["total"].as_i64().unwrap();
    let page_size = v["page_size"].as_i64().unwrap();
    assert_eq!(returned, 10, "first page should be 10");
    assert_eq!(total, 12);
    assert_eq!(page_size, 10);
    let rows = v["rows"].as_array().unwrap();
    // Newest-first means body 11 then 10 then ... 02
    let first_snippet = rows[0]["snippet"].as_str().unwrap_or_default();
    assert!(
        first_snippet.contains("number 11"),
        "page 0 row 0 should be the newest insight; got snippet=`{first_snippet}`"
    );
}

#[test]
fn list_offset_one_returns_remaining_two() {
    let tmp = fresh_project();
    for i in 0..12 {
        let agent = if i % 2 == 0 { "planner" } else { "verifier" };
        create_insight(
            tmp.path(),
            &format!("insight body number {i:02}"),
            "agent-learned",
            agent,
            &[],
        )
        .success();
    }
    let assert = bin()
        .current_dir(tmp.path())
        .args(["insight", "list", "--offset", "1", "--json"])
        .assert()
        .success();
    let stdout = String::from_utf8_lossy(&assert.get_output().stdout).to_string();
    let v: serde_json::Value = serde_json::from_str(&stdout).expect("valid json");
    assert_eq!(v["returned"].as_i64().unwrap(), 2, "page 1 should hold the remaining 2");
}

#[test]
fn list_filter_by_agent_only_returns_matches() {
    let tmp = fresh_project();
    create_insight(tmp.path(), "alpha body", "agent-learned", "planner", &[]).success();
    create_insight(tmp.path(), "beta body", "agent-learned", "verifier", &[]).success();
    let assert = bin()
        .current_dir(tmp.path())
        .args(["insight", "list", "--agent", "planner", "--json"])
        .assert()
        .success();
    let stdout = String::from_utf8_lossy(&assert.get_output().stdout).to_string();
    let v: serde_json::Value = serde_json::from_str(&stdout).expect("valid json");
    assert_eq!(v["total"].as_i64().unwrap(), 1);
    assert!(stdout.contains("alpha"));
    assert!(!stdout.contains("beta"));
}

// ---------------------------------------------------------------------------
// `insight random` — happy + empty corpus.
// ---------------------------------------------------------------------------

#[test]
fn random_returns_one_row_when_corpus_non_empty() {
    let tmp = fresh_project();
    create_insight(tmp.path(), "alpha", "agent-learned", "planner", &[]).success();
    create_insight(tmp.path(), "beta", "agent-learned", "verifier", &[]).success();
    let assert = bin()
        .current_dir(tmp.path())
        .args(["insight", "random", "--json"])
        .assert()
        .success();
    let stdout = String::from_utf8_lossy(&assert.get_output().stdout).to_string();
    let v: serde_json::Value = serde_json::from_str(&stdout).expect("valid json");
    assert!(v["id"].as_i64().is_some());
    assert!(v["body"].as_str().is_some());
}

#[test]
fn random_exits_1_on_empty_corpus() {
    let tmp = fresh_project();
    // Touch the db so open_or_init succeeds (no doc rows inside).
    fs::create_dir_all(tmp.path().join(".claude/knowledge")).unwrap();
    bin()
        .current_dir(tmp.path())
        .args(["insight", "random"])
        .assert()
        .failure()
        .code(1);
}

// ---------------------------------------------------------------------------
// `insight get` — by id, by sha prefix, errors.
// ---------------------------------------------------------------------------

#[test]
fn get_by_integer_id_returns_full_record() {
    let tmp = fresh_project();
    create_insight(tmp.path(), "the unique body", "agent-learned", "planner", &[]).success();
    let conn = open_db(&insights_db(tmp.path()));
    let id: i64 = conn.query_row("SELECT id FROM documents LIMIT 1", [], |r| r.get(0)).unwrap();
    let assert = bin()
        .current_dir(tmp.path())
        .args(["insight", "get", &id.to_string(), "--json"])
        .assert()
        .success();
    let stdout = String::from_utf8_lossy(&assert.get_output().stdout).to_string();
    let v: serde_json::Value = serde_json::from_str(&stdout).expect("valid json");
    assert_eq!(v["id"].as_i64().unwrap(), id);
    assert!(v["body"].as_str().unwrap().contains("the unique body"));
}

#[test]
fn get_by_sha_prefix_matches_via_like() {
    let tmp = fresh_project();
    create_insight(tmp.path(), "sha-prefix lookup target", "agent-learned", "planner", &[]).success();
    let conn = open_db(&insights_db(tmp.path()));
    let sha: String = conn.query_row("SELECT sha256 FROM documents LIMIT 1", [], |r| r.get(0)).unwrap();
    let prefix: String = sha.chars().take(8).collect();
    bin()
        .current_dir(tmp.path())
        .args(["insight", "get", &prefix])
        .assert()
        .success();
}

#[test]
fn get_unknown_id_exits_1() {
    let tmp = fresh_project();
    create_insight(tmp.path(), "anything", "agent-learned", "planner", &[]).success();
    bin()
        .current_dir(tmp.path())
        .args(["insight", "get", "99999"])
        .assert()
        .failure()
        .code(1);
}

#[test]
fn get_short_non_numeric_ident_exits_2() {
    let tmp = fresh_project();
    bin()
        .current_dir(tmp.path())
        .args(["insight", "get", "abc"])
        .assert()
        .failure()
        .code(2);
}

#[test]
fn get_non_hex_ident_exits_2() {
    let tmp = fresh_project();
    bin()
        .current_dir(tmp.path())
        .args(["insight", "get", "zzzzzz"])
        .assert()
        .failure()
        .code(2);
}
