//! Slice 10 (agent-insights-base) — full lifecycle E2E.
//!
//! Exercises every subcommand in `insight` plus `search --corpus all`
//! against the same project tempdir, in the order an agent + operator
//! would use them:
//!
//!   1. create two insights (high + low salience)
//!   2. list — both visible
//!   3. random — returns one of them
//!   4. get <id> — full record round-trips
//!   5. insight search — finds them by keyword
//!   6. search --corpus all — labels hits with source_corpus
//!   7. backdate the low-salience insight by 100 days
//!   8. gc --dry-run — reports low_deleted=1, no DB changes
//!   9. gc — actually purges; high survives
//!  10. delete — final cleanup; corpus is empty

use assert_cmd::Command;
use rusqlite::params;
use std::fs;
use std::path::{Path, PathBuf};

fn bin() -> Command {
    Command::cargo_bin("claudebase").expect("binary built")
}

fn fresh_project() -> tempfile::TempDir {
    let tmp = tempfile::tempdir().expect("tempdir");
    fs::create_dir_all(tmp.path().join(".claude/knowledge")).expect("mkdir");
    tmp
}

fn insights_db(root: &Path) -> PathBuf {
    root.join(".claude/knowledge/insights.db")
}

fn open_db(p: &Path) -> rusqlite::Connection {
    rusqlite::Connection::open(p).expect("open db")
}

#[test]
fn full_lifecycle_from_create_to_gc_to_delete() {
    let tmp = fresh_project();
    let proj = tmp.path();

    // ─────────────────────────────────────────────────────────────────
    // Step 1: create two insights, different salience tiers, different
    // agents (cross-agent so semantic dedup never collides).
    // ─────────────────────────────────────────────────────────────────
    bin()
        .current_dir(proj)
        .args([
            "insight", "create",
            "kafka exactly-once delivery requires careful rebalance handling during transaction commit",
            "--type", "agent-learned",
            "--agent", "reflection",
            "--feature", "payments-v2",
            "--salience", "high",
            "--category", "project",
            "--tags", "kafka",
            "--json",
        ])
        .assert()
        .success();
    bin()
        .current_dir(proj)
        .args([
            "insight", "create",
            "polygon CTF balanceOf returns wei units not human-readable amounts",
            "--type", "agent-learned",
            "--agent", "verifier",
            "--feature", "payments-v2",
            "--salience", "low",
            "--category", "project",
            "--tags", "polygon",
            "--json",
        ])
        .assert()
        .success();

    // ─────────────────────────────────────────────────────────────────
    // Step 2: list — both visible, page_size default 10.
    // ─────────────────────────────────────────────────────────────────
    let assert = bin()
        .current_dir(proj)
        .args(["insight", "list", "--json"])
        .assert()
        .success();
    let v: serde_json::Value =
        serde_json::from_slice(&assert.get_output().stdout).expect("valid json");
    assert_eq!(v["total"].as_i64().unwrap(), 2);
    assert_eq!(v["returned"].as_i64().unwrap(), 2);

    // ─────────────────────────────────────────────────────────────────
    // Step 3: random — returns one row from the corpus.
    // ─────────────────────────────────────────────────────────────────
    let assert = bin()
        .current_dir(proj)
        .args(["insight", "random", "--json"])
        .assert()
        .success();
    let v: serde_json::Value =
        serde_json::from_slice(&assert.get_output().stdout).expect("valid json");
    assert!(v["id"].as_i64().is_some());
    assert!(v["body"].as_str().is_some());

    // ─────────────────────────────────────────────────────────────────
    // Step 4: get <id> — the high-salience insight by integer id.
    // ─────────────────────────────────────────────────────────────────
    let conn = open_db(&insights_db(proj));
    let high_id: i64 = conn
        .query_row(
            "SELECT id FROM documents WHERE salience='high'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    let low_id: i64 = conn
        .query_row(
            "SELECT id FROM documents WHERE salience='low'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    drop(conn);
    let assert = bin()
        .current_dir(proj)
        .args(["insight", "get", &high_id.to_string(), "--json"])
        .assert()
        .success();
    let v: serde_json::Value =
        serde_json::from_slice(&assert.get_output().stdout).expect("valid json");
    assert_eq!(v["id"].as_i64().unwrap(), high_id);
    assert_eq!(v["salience"].as_str().unwrap(), "high");
    assert!(v["body"]
        .as_str()
        .unwrap()
        .contains("kafka exactly-once delivery"));

    // ─────────────────────────────────────────────────────────────────
    // Step 5: insight search — lexical mode, find by keyword.
    // ─────────────────────────────────────────────────────────────────
    let assert = bin()
        .current_dir(proj)
        .args([
            "insight", "search", "kafka",
            "--mode", "lexical",
            "--top-k", "5",
            "--json",
        ])
        .assert()
        .success();
    let stdout = String::from_utf8_lossy(&assert.get_output().stdout).to_string();
    assert!(
        stdout.contains("kafka") || stdout.contains("rebalance"),
        "insight search should hit the kafka insight; got:\n{stdout}"
    );

    // ─────────────────────────────────────────────────────────────────
    // Step 6: search --corpus all — hits labeled source_corpus.
    // books index.db doesn't exist; only insights row should appear.
    // ─────────────────────────────────────────────────────────────────
    let assert = bin()
        .current_dir(proj)
        .args([
            "search", "kafka",
            "--corpus", "all",
            "--mode", "lexical",
            "--top-k", "5",
            "--json",
        ])
        .assert()
        .success();
    let v: serde_json::Value =
        serde_json::from_slice(&assert.get_output().stdout).expect("valid json");
    let hits = v.as_array().expect("array");
    assert!(!hits.is_empty(), "expected ≥1 hit from insights corpus");
    for h in hits {
        assert_eq!(h["source_corpus"].as_str().unwrap_or(""), "insights");
    }

    // ─────────────────────────────────────────────────────────────────
    // Step 7: backdate the low-salience insight by 100 days.
    // ─────────────────────────────────────────────────────────────────
    let conn = open_db(&insights_db(proj));
    let now: i64 = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs() as i64;
    let old_ts = now - 100 * 86_400;
    conn.execute(
        "UPDATE documents SET ingested_at = ?1 WHERE id = ?2",
        params![old_ts, low_id],
    )
    .unwrap();
    drop(conn);

    // ─────────────────────────────────────────────────────────────────
    // Step 8: gc --dry-run — reports low_deleted=1 but DOES NOT delete.
    // ─────────────────────────────────────────────────────────────────
    let assert = bin()
        .current_dir(proj)
        .args(["insight", "gc", "--dry-run", "--json"])
        .assert()
        .success();
    let v: serde_json::Value =
        serde_json::from_slice(&assert.get_output().stdout).expect("valid json");
    assert_eq!(v["dry_run"].as_bool().unwrap(), true);
    assert_eq!(v["would_delete_low"].as_u64().unwrap(), 1);
    let conn = open_db(&insights_db(proj));
    let n: i64 = conn
        .query_row("SELECT COUNT(*) FROM documents", [], |r| r.get(0))
        .unwrap();
    assert_eq!(n, 2, "dry-run must NOT delete");
    drop(conn);

    // ─────────────────────────────────────────────────────────────────
    // Step 9: gc — actually purges. High survives, low is gone.
    // ─────────────────────────────────────────────────────────────────
    let assert = bin()
        .current_dir(proj)
        .args(["insight", "gc", "--json"])
        .assert()
        .success();
    let v: serde_json::Value =
        serde_json::from_slice(&assert.get_output().stdout).expect("valid json");
    assert_eq!(v["low_deleted"].as_u64().unwrap(), 1);
    assert_eq!(v["medium_deleted"].as_u64().unwrap(), 0);
    let conn = open_db(&insights_db(proj));
    let n: i64 = conn
        .query_row("SELECT COUNT(*) FROM documents", [], |r| r.get(0))
        .unwrap();
    assert_eq!(n, 1, "only high-salience insight should survive gc");
    let surviving_salience: String = conn
        .query_row("SELECT salience FROM documents LIMIT 1", [], |r| r.get(0))
        .unwrap();
    assert_eq!(surviving_salience, "high");
    drop(conn);

    // ─────────────────────────────────────────────────────────────────
    // Step 10: delete — explicit cleanup. Corpus now empty.
    // ─────────────────────────────────────────────────────────────────
    bin()
        .current_dir(proj)
        .args(["insight", "delete", &high_id.to_string()])
        .assert()
        .success();
    let conn = open_db(&insights_db(proj));
    let n: i64 = conn
        .query_row("SELECT COUNT(*) FROM documents", [], |r| r.get(0))
        .unwrap();
    assert_eq!(n, 0, "delete should drop the last document");
    let chunks: i64 = conn
        .query_row("SELECT COUNT(*) FROM chunks", [], |r| r.get(0))
        .unwrap();
    assert_eq!(chunks, 0, "chunks cascade-deleted on document delete");
}
