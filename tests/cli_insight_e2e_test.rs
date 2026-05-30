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

/// Pin `$HOME` / `USERPROFILE` to a per-project sandbox dir so the GLOBAL
/// insights db (`$HOME/.claude/knowledge/insights.db`) resolves UNDER the test
/// tempdir, never the operator's real home. Slice 5 made the default insight
/// read path merge the cwd-local db with the global db; without this pin, a
/// test that asserts an exact local-only count would non-deterministically
/// pick up the operator's real global insights. The home dir lives inside the
/// project tempdir (`<project>/.testhome`) so it is auto-cleaned and shared
/// consistently across the create + read commands of one test.
fn pin_home(cmd: &mut Command, project: &Path) {
    let home = project.join(".testhome");
    let _ = fs::create_dir_all(&home);
    cmd.env("HOME", &home).env("USERPROFILE", &home);
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
    pin_home(&mut cmd, project);
    cmd.current_dir(project).args(["insight", "create", body, "--type", kind, "--agent", agent]);
    // Slice 3 made --category + --tags mandatory. Tests that don't exercise
    // routing default to a project-scoped insight with a seed tag so they
    // stay hermetic (cwd-local db only, never the operator's global db).
    let caller_sets_category = extra.iter().any(|e| *e == "--category");
    let caller_sets_tags = extra.iter().any(|e| *e == "--tags");
    if !caller_sets_category {
        cmd.args(["--category", "project"]);
    }
    if !caller_sets_tags {
        cmd.args(["--tags", "seedtag"]);
    }
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
    let mut pinned1 = bin();
    pin_home(&mut pinned1, tmp.path());
    pinned1
        .current_dir(tmp.path())
        .args([
            "insight", "create", "--type", "reflection-observation", "--agent", "reflection",
            "--category", "project", "--tags", "seedtag",
        ])
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
    let mut pinned2 = bin();
    pin_home(&mut pinned2, tmp.path());
    pinned2
        .current_dir(tmp.path())
        .args([
            "insight", "create", "   \n\t  ",
            "--type", "agent-learned", "--agent", "x",
            "--category", "project", "--tags", "seedtag",
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
    let mut pinned3 = bin();
    pin_home(&mut pinned3, tmp.path());
    let assert = pinned3
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
// `insight gc / delete` — Slice 7 TTL purge + single delete.
// ---------------------------------------------------------------------------

#[test]
fn gc_purges_low_salience_past_90_days() {
    let tmp = fresh_project();
    // Three insights at distinct salience tiers, different agents to bypass
    // semantic dedup.
    create_insight(tmp.path(), "high-tier insight body alpha", "agent-learned", "a",
                   &["--salience", "high"]).success();
    create_insight(tmp.path(), "medium-tier insight body beta", "agent-learned", "b",
                   &["--salience", "medium"]).success();
    create_insight(tmp.path(), "low-tier insight body gamma", "agent-learned", "c",
                   &["--salience", "low"]).success();
    // Backdate the LOW one by 100 days.
    let conn = open_db(&insights_db(tmp.path()));
    let now: i64 = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH).unwrap().as_secs() as i64;
    let old_ts = now - 100 * 86_400;
    conn.execute(
        "UPDATE documents SET ingested_at = ?1 WHERE salience = 'low'",
        params![old_ts],
    ).unwrap();
    drop(conn);
    // Run gc.
    let mut pinned4 = bin();
    pin_home(&mut pinned4, tmp.path());
    let assert = pinned4
        .current_dir(tmp.path())
        .args(["insight", "gc", "--json"])
        .assert()
        .success();
    let stdout = String::from_utf8_lossy(&assert.get_output().stdout).to_string();
    let v: serde_json::Value = serde_json::from_str(&stdout).expect("valid json");
    assert_eq!(v["low_deleted"].as_u64().unwrap(), 1);
    assert_eq!(v["medium_deleted"].as_u64().unwrap(), 0);
    // Verify the right ones survive.
    let conn = open_db(&insights_db(tmp.path()));
    let n: i64 = conn.query_row("SELECT COUNT(*) FROM documents", [], |r| r.get(0)).unwrap();
    assert_eq!(n, 2, "high and medium must survive; low must be purged");
    let salience_left: Vec<String> = conn
        .prepare("SELECT salience FROM documents ORDER BY salience").unwrap()
        .query_map([], |r| r.get::<_, String>(0)).unwrap()
        .filter_map(Result::ok).collect();
    assert_eq!(salience_left, vec!["high".to_string(), "medium".to_string()]);
}

#[test]
fn gc_purges_medium_salience_past_365_days() {
    let tmp = fresh_project();
    create_insight(tmp.path(), "medium-tier insight body delta", "agent-learned", "a",
                   &["--salience", "medium"]).success();
    let conn = open_db(&insights_db(tmp.path()));
    let now: i64 = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH).unwrap().as_secs() as i64;
    // 400 days back — past the 365-day TTL.
    let old_ts = now - 400 * 86_400;
    conn.execute("UPDATE documents SET ingested_at = ?1", params![old_ts]).unwrap();
    drop(conn);
    let mut gc_cmd = bin();
    pin_home(&mut gc_cmd, tmp.path());
    let assert = gc_cmd.current_dir(tmp.path()).args(["insight", "gc", "--json"]).assert().success();
    let stdout = String::from_utf8_lossy(&assert.get_output().stdout).to_string();
    let v: serde_json::Value = serde_json::from_str(&stdout).expect("valid json");
    assert_eq!(v["medium_deleted"].as_u64().unwrap(), 1);
}

#[test]
fn gc_dry_run_reports_without_deleting() {
    let tmp = fresh_project();
    create_insight(tmp.path(), "low body would be deleted soon", "agent-learned", "a",
                   &["--salience", "low"]).success();
    let conn = open_db(&insights_db(tmp.path()));
    let now: i64 = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH).unwrap().as_secs() as i64;
    conn.execute("UPDATE documents SET ingested_at = ?1", params![now - 100*86_400]).unwrap();
    drop(conn);
    let mut gc_cmd = bin();
    pin_home(&mut gc_cmd, tmp.path());
    let assert = gc_cmd.current_dir(tmp.path()).args(["insight", "gc", "--dry-run", "--json"]).assert().success();
    let stdout = String::from_utf8_lossy(&assert.get_output().stdout).to_string();
    let v: serde_json::Value = serde_json::from_str(&stdout).expect("valid json");
    assert_eq!(v["dry_run"].as_bool().unwrap(), true);
    assert_eq!(v["would_delete_low"].as_u64().unwrap(), 1);
    // Row still present.
    let conn = open_db(&insights_db(tmp.path()));
    let n: i64 = conn.query_row("SELECT COUNT(*) FROM documents", [], |r| r.get(0)).unwrap();
    assert_eq!(n, 1, "dry-run must NOT delete rows");
}

#[test]
fn delete_removes_one_insight_by_id() {
    let tmp = fresh_project();
    create_insight(tmp.path(), "redis pipelining halves the latency", "agent-learned", "x", &[]).success();
    let conn = open_db(&insights_db(tmp.path()));
    let id: i64 = conn.query_row("SELECT id FROM documents LIMIT 1", [], |r| r.get(0)).unwrap();
    drop(conn);
    bin().current_dir(tmp.path()).args(["insight", "delete", &id.to_string()]).assert().success();
    let conn = open_db(&insights_db(tmp.path()));
    let n: i64 = conn.query_row("SELECT COUNT(*) FROM documents", [], |r| r.get(0)).unwrap();
    assert_eq!(n, 0);
    let chunks: i64 = conn.query_row("SELECT COUNT(*) FROM chunks", [], |r| r.get(0)).unwrap();
    assert_eq!(chunks, 0, "chunks must cascade-delete");
}

#[test]
fn delete_unknown_id_exits_1() {
    let tmp = fresh_project();
    create_insight(tmp.path(), "anything", "agent-learned", "x", &[]).success();
    bin().current_dir(tmp.path()).args(["insight", "delete", "99999"]).assert().failure().code(1);
}

// ---------------------------------------------------------------------------
// `insight create` — Slice 5 semantic dedup (cosine > 0.92, same agent, 30d).
// ---------------------------------------------------------------------------

/// Probe whether the e5 encoder is functional in this test environment.
/// Returns true iff a write triggered chunks_vec population (encoder
/// available + chunks_vec virtual table working).
fn encoder_is_functional(project: &Path) -> bool {
    let conn = open_db(&insights_db(project));
    let n: i64 = conn
        .query_row("SELECT COUNT(*) FROM chunks_vec", [], |r| r.get(0))
        .unwrap_or(0);
    n > 0
}

#[test]
fn semantic_dedup_skips_near_duplicate_when_encoder_available() {
    let tmp = fresh_project();
    create_insight(
        tmp.path(),
        "the agent learned that kafka rebalance during transaction commit breaks exactly-once delivery",
        "agent-learned",
        "reflection",
        &[],
    )
    .success();
    if !encoder_is_functional(tmp.path()) {
        eprintln!(
            "encoder not functional in this test env (chunks_vec empty); \
             skipping semantic-dedup assertion. install via `bash install.sh --yes`."
        );
        return;
    }
    // Paraphrased body — same concept, different word order. cosine should
    // be well above 0.92 for these two e5-encoded passages.
    let assert = create_insight(
        tmp.path(),
        "kafka exactly-once delivery breaks when rebalance happens during the transaction commit, as the agent learned",
        "agent-learned",
        "reflection", // SAME agent — load-bearing for the dedup window
        &["--json"],
    )
    .success();
    let stdout = String::from_utf8_lossy(&assert.get_output().stdout).to_string();
    assert!(
        stdout.contains("\"status\": \"near-duplicate\"")
            || stdout.contains("\"status\":\"near-duplicate\""),
        "expected status=near-duplicate on paraphrased re-emit; got:\n{stdout}"
    );
    let conn = open_db(&insights_db(tmp.path()));
    let n: i64 = conn
        .query_row("SELECT COUNT(*) FROM documents", [], |r| r.get(0))
        .unwrap();
    assert_eq!(n, 1, "near-duplicate must NOT increment doc count");
}

#[test]
fn semantic_dedup_does_not_block_unrelated_body() {
    let tmp = fresh_project();
    create_insight(
        tmp.path(),
        "the agent learned that kafka rebalance during transaction commit breaks exactly-once delivery",
        "agent-learned",
        "reflection",
        &[],
    )
    .success();
    if !encoder_is_functional(tmp.path()) {
        eprintln!("encoder not functional; skipping unrelated-body assertion.");
        return;
    }
    // A semantically unrelated body — about pdfium PDF extraction.
    let assert = create_insight(
        tmp.path(),
        "pdfium correctly handles CID fonts in calibre-converted PDF documents",
        "agent-learned",
        "reflection",
        &["--json"],
    )
    .success();
    let stdout = String::from_utf8_lossy(&assert.get_output().stdout).to_string();
    assert!(
        stdout.contains("\"status\": \"written\"")
            || stdout.contains("\"status\":\"written\""),
        "unrelated body must NOT trigger semantic dedup; got:\n{stdout}"
    );
    let conn = open_db(&insights_db(tmp.path()));
    let n: i64 = conn
        .query_row("SELECT COUNT(*) FROM documents", [], |r| r.get(0))
        .unwrap();
    assert_eq!(n, 2, "unrelated body must create a second doc");
}

#[test]
fn semantic_dedup_does_not_block_other_agent() {
    let tmp = fresh_project();
    create_insight(
        tmp.path(),
        "the agent learned that kafka rebalance during transaction commit breaks exactly-once delivery",
        "agent-learned",
        "reflection",
        &[],
    )
    .success();
    if !encoder_is_functional(tmp.path()) {
        eprintln!("encoder not functional; skipping cross-agent assertion.");
        return;
    }
    // Paraphrase BUT different agent — must NOT dedup (cross-agent agreement
    // is load-bearing signal).
    let assert = create_insight(
        tmp.path(),
        "kafka exactly-once delivery breaks when rebalance happens during the transaction commit",
        "agent-learned",
        "verifier", // DIFFERENT agent
        &["--json"],
    )
    .success();
    let stdout = String::from_utf8_lossy(&assert.get_output().stdout).to_string();
    assert!(
        stdout.contains("\"status\": \"written\"")
            || stdout.contains("\"status\":\"written\""),
        "cross-agent paraphrase must NOT dedup; got:\n{stdout}"
    );
    let conn = open_db(&insights_db(tmp.path()));
    let n: i64 = conn
        .query_row("SELECT COUNT(*) FROM documents", [], |r| r.get(0))
        .unwrap();
    assert_eq!(n, 2, "cross-agent paraphrase must create a second doc");
}

// ---------------------------------------------------------------------------
// `insight search` — Slice 4 metadata filters.
// ---------------------------------------------------------------------------

#[test]
fn search_filter_by_agent_drops_other_agents_hits() {
    let tmp = fresh_project();
    create_insight(tmp.path(), "alpha rebalance commit", "agent-learned", "planner", &[]).success();
    create_insight(tmp.path(), "beta rebalance commit", "agent-learned", "verifier", &[]).success();
    let mut pinned5 = bin();
    pin_home(&mut pinned5, tmp.path());
    let assert = pinned5
        .current_dir(tmp.path())
        .args([
            "insight", "search", "rebalance",
            "--mode", "lexical",
            "--agent", "planner",
            "--top-k", "5",
            "--json",
        ])
        .assert()
        .success();
    let stdout = String::from_utf8_lossy(&assert.get_output().stdout).to_string();
    assert!(stdout.contains("alpha"), "expected alpha (planner) hit; got:\n{stdout}");
    assert!(!stdout.contains("beta"), "verifier's hit should be filtered out; got:\n{stdout}");
}

#[test]
fn search_filter_by_salience_keeps_only_matching_tier() {
    let tmp = fresh_project();
    create_insight(tmp.path(), "highsal rebalance commit", "agent-learned", "a", &["--salience", "high"]).success();
    create_insight(tmp.path(), "medsal rebalance commit", "agent-learned", "b", &["--salience", "medium"]).success();
    let mut pinned6 = bin();
    pin_home(&mut pinned6, tmp.path());
    let assert = pinned6
        .current_dir(tmp.path())
        .args([
            "insight", "search", "rebalance",
            "--mode", "lexical",
            "--salience", "high",
            "--top-k", "5",
            "--json",
        ])
        .assert()
        .success();
    let stdout = String::from_utf8_lossy(&assert.get_output().stdout).to_string();
    assert!(stdout.contains("highsal"));
    assert!(!stdout.contains("medsal"));
}

#[test]
fn search_filter_by_type_and_feature_combine() {
    let tmp = fresh_project();
    create_insight(
        tmp.path(),
        "kafka rebalance commit window",
        "agent-learned",
        "a",
        &["--feature", "payments-v2"],
    ).success();
    create_insight(
        tmp.path(),
        "kafka rebalance commit batches",
        "consolidator-drift",
        "b",
        &["--feature", "payments-v2"],
    ).success();
    create_insight(
        tmp.path(),
        "kafka rebalance commit elsewhere",
        "agent-learned",
        "c",
        &["--feature", "checkout-v1"],
    ).success();
    let mut pinned7 = bin();
    pin_home(&mut pinned7, tmp.path());
    let assert = pinned7
        .current_dir(tmp.path())
        .args([
            "insight", "search", "rebalance",
            "--mode", "lexical",
            "--type", "agent-learned",
            "--feature", "payments-v2",
            "--top-k", "5",
            "--json",
        ])
        .assert()
        .success();
    let stdout = String::from_utf8_lossy(&assert.get_output().stdout).to_string();
    assert!(stdout.contains("window"), "expected agent-learned + payments-v2 hit");
    assert!(!stdout.contains("batches"), "consolidator-drift type filtered out");
    assert!(!stdout.contains("elsewhere"), "checkout-v1 feature filtered out");
}

#[test]
fn search_since_filter_rejects_malformed_value() {
    let tmp = fresh_project();
    create_insight(tmp.path(), "anything", "agent-learned", "x", &[]).success();
    // No unit suffix.
    let mut pinned8 = bin();
    pin_home(&mut pinned8, tmp.path());
    pinned8
        .current_dir(tmp.path())
        .args(["insight", "search", "anything", "--since", "30", "--mode", "lexical"])
        .assert()
        .failure()
        .code(2);
    // Unknown unit.
    let mut pinned9 = bin();
    pin_home(&mut pinned9, tmp.path());
    pinned9
        .current_dir(tmp.path())
        .args(["insight", "search", "anything", "--since", "30x", "--mode", "lexical"])
        .assert()
        .failure()
        .code(2);
}

#[test]
fn search_since_filter_keeps_recent_drops_old() {
    let tmp = fresh_project();
    create_insight(tmp.path(), "fresh rebalance commit text", "agent-learned", "x", &[]).success();
    let db_path = insights_db(tmp.path());
    // Backdate the row by 100 days.
    let conn = open_db(&db_path);
    let now: i64 = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs() as i64;
    let old_ts = now - 100 * 86_400;
    conn.execute("UPDATE documents SET ingested_at = ?1 WHERE id = 1", params![old_ts])
        .unwrap();
    drop(conn);
    // --since 7d → should drop the 100-day-old insight.
    let mut pinned10 = bin();
    pin_home(&mut pinned10, tmp.path());
    let assert = pinned10
        .current_dir(tmp.path())
        .args([
            "insight", "search", "rebalance",
            "--mode", "lexical",
            "--since", "7d",
            "--top-k", "5",
            "--json",
        ])
        .assert()
        .success();
    let stdout = String::from_utf8_lossy(&assert.get_output().stdout).to_string();
    assert!(
        !stdout.contains("fresh rebalance commit"),
        "100-day-old insight should be filtered by --since 7d; got:\n{stdout}"
    );
}

// ---------------------------------------------------------------------------
// `search --corpus` — Slice 6 cross-corpus flag on the standalone `search`.
// ---------------------------------------------------------------------------

#[test]
fn search_corpus_insights_opens_insights_db() {
    let tmp = fresh_project();
    create_insight(
        tmp.path(),
        "redis pipelining reduces round-trip cost dramatically",
        "agent-learned",
        "planner",
        &[],
    )
    .success();
    let mut pinned11 = bin();
    pin_home(&mut pinned11, tmp.path());
    let assert = pinned11
        .current_dir(tmp.path())
        .args([
            "search",
            "pipelining",
            "--corpus",
            "insights",
            "--mode",
            "lexical",
            "--json",
        ])
        .assert()
        .success();
    let stdout = String::from_utf8_lossy(&assert.get_output().stdout).to_string();
    assert!(
        stdout.contains("redis") || stdout.contains("pipelining"),
        "--corpus insights should hit the insights.db; got:\n{stdout}"
    );
}

#[test]
fn search_corpus_books_does_not_see_insights() {
    let tmp = fresh_project();
    // Write only to insights — books index.db doesn't exist yet (will be
    // created empty when `--corpus books` opens it).
    create_insight(
        tmp.path(),
        "kafka exactly-once semantics break on rebalance",
        "agent-learned",
        "planner",
        &[],
    )
    .success();
    // `--corpus books` opens (or creates) index.db; since the insight was
    // written to insights.db, the books-corpus search must return ZERO
    // results — no cross-corpus bleed-through.
    let mut pinned12 = bin();
    pin_home(&mut pinned12, tmp.path());
    let assert = pinned12
        .current_dir(tmp.path())
        .args([
            "search",
            "kafka",
            "--corpus",
            "books",
            "--mode",
            "lexical",
            "--json",
        ])
        .assert()
        .success();
    let stdout = String::from_utf8_lossy(&assert.get_output().stdout).to_string();
    let v: serde_json::Value = serde_json::from_str(&stdout).expect("valid json");
    let hits = v.as_array().expect("search returns array");
    assert!(
        hits.is_empty(),
        "books corpus must NOT see insight content; got hits:\n{stdout}"
    );
}

#[test]
fn search_corpus_all_with_only_insights_present_returns_hits_with_source_corpus() {
    let tmp = fresh_project();
    create_insight(
        tmp.path(),
        "kafka exactly-once semantics break on rebalance during commit",
        "agent-learned",
        "planner",
        &[],
    )
    .success();
    // index.db does NOT exist — Slice-6 contract: silently treat missing
    // corpus as empty, return hits from the other corpus.
    let mut pinned13 = bin();
    pin_home(&mut pinned13, tmp.path());
    let assert = pinned13
        .current_dir(tmp.path())
        .args([
            "search",
            "kafka",
            "--corpus",
            "all",
            "--mode",
            "lexical",
            "--json",
        ])
        .assert()
        .success();
    let stdout = String::from_utf8_lossy(&assert.get_output().stdout).to_string();
    let v: serde_json::Value = serde_json::from_str(&stdout).expect("valid json");
    let hits = v.as_array().expect("search returns array");
    assert!(!hits.is_empty(), "expected ≥1 hit from insights corpus");
    // Every hit must be labeled `source_corpus=insights` since books.db is absent.
    for h in hits {
        let label = h["source_corpus"].as_str().unwrap_or("");
        assert_eq!(
            label, "insights",
            "hit should be labeled `insights` when only insights corpus has data; got:\n{h}"
        );
    }
}

// ---------------------------------------------------------------------------
// `insight list` — pagination, defaults, filters.
// ---------------------------------------------------------------------------

/// Semantically distinct fixture bodies for pagination tests. Adjacent
/// bodies must differ in EMBEDDING space, not just by a number, because
/// the Slice 5 semantic-dedup probe correctly catches near-paraphrases
/// (`insight body number 00` and `insight body number 01` map to nearly
/// identical e5 vectors and would dedup). Each line below is intentionally
/// about a different concept.
const PAGINATION_FIXTURE_BODIES: &[&str] = &[
    "kafka exactly-once breaks on rebalance during transaction commit",
    "redis pipelining reduces round-trip cost for bulk SET operations",
    "postgres index-only scans require all columns in the index",
    "rust borrow checker rejects mutable aliasing of the same reference",
    "vector quantization shrinks ANN index size at recall cost",
    "FTS5 unicode61 tokenizer treats hyphens as token separators",
    "RRF k=60 fuses BM25 and dense rankers without score normalization",
    "sqlite WAL mode allows concurrent reader during a writer commit",
    "ONNX runtime CPU provider works without GPU dependency chain",
    "e5-multilingual-small outputs L2-normalized 384-dimensional vectors",
    "claude-code agents pipe stdin into claudebase to bypass TTY guards",
    "github actions matrix builds parallelize across darwin and linux",
];

#[test]
fn list_default_page_size_is_ten_newest_first() {
    let tmp = fresh_project();
    // 12 semantically distinct bodies — see PAGINATION_FIXTURE_BODIES doc.
    for (i, body) in PAGINATION_FIXTURE_BODIES.iter().enumerate() {
        let agent = if i % 2 == 0 { "planner" } else { "verifier" };
        create_insight(tmp.path(), body, "agent-learned", agent, &[]).success();
    }
    let mut list_cmd = bin();
    pin_home(&mut list_cmd, tmp.path());
    let assert = list_cmd
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
    // Newest-first: row[0] should be the LAST body in the fixture array.
    let first_snippet = rows[0]["snippet"].as_str().unwrap_or_default();
    let last_body = PAGINATION_FIXTURE_BODIES.last().unwrap();
    assert!(
        first_snippet.starts_with(&last_body[..40]),
        "page 0 row 0 should be the newest insight (`{last_body}`); got snippet=`{first_snippet}`"
    );
}

#[test]
fn list_offset_one_returns_remaining_two() {
    let tmp = fresh_project();
    for (i, body) in PAGINATION_FIXTURE_BODIES.iter().enumerate() {
        let agent = if i % 2 == 0 { "planner" } else { "verifier" };
        create_insight(tmp.path(), body, "agent-learned", agent, &[]).success();
    }
    let mut list_cmd = bin();
    pin_home(&mut list_cmd, tmp.path());
    let assert = list_cmd
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
    let mut pinned14 = bin();
    pin_home(&mut pinned14, tmp.path());
    let assert = pinned14
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
    let mut pinned15 = bin();
    pin_home(&mut pinned15, tmp.path());
    let assert = pinned15
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
    let mut rand_cmd = bin();
    pin_home(&mut rand_cmd, tmp.path());
    rand_cmd
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
    let mut pinned16 = bin();
    pin_home(&mut pinned16, tmp.path());
    let assert = pinned16
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
    let mut pinned17 = bin();
    pin_home(&mut pinned17, tmp.path());
    pinned17
        .current_dir(tmp.path())
        .args(["insight", "get", &prefix])
        .assert()
        .success();
}

#[test]
fn get_unknown_id_exits_1() {
    let tmp = fresh_project();
    create_insight(tmp.path(), "anything", "agent-learned", "planner", &[]).success();
    let mut pinned18 = bin();
    pin_home(&mut pinned18, tmp.path());
    pinned18
        .current_dir(tmp.path())
        .args(["insight", "get", "99999"])
        .assert()
        .failure()
        .code(1);
}

#[test]
fn get_short_non_numeric_ident_exits_2() {
    let tmp = fresh_project();
    let mut pinned19 = bin();
    pin_home(&mut pinned19, tmp.path());
    pinned19
        .current_dir(tmp.path())
        .args(["insight", "get", "abc"])
        .assert()
        .failure()
        .code(2);
}

#[test]
fn get_non_hex_ident_exits_2() {
    let tmp = fresh_project();
    let mut pinned20 = bin();
    pin_home(&mut pinned20, tmp.path());
    pinned20
        .current_dir(tmp.path())
        .args(["insight", "get", "zzzzzz"])
        .assert()
        .failure()
        .code(2);
}
