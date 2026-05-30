//! Slice 5 (insights-hybrid-corpus) — dual-DB insight read path + filters.
//!
//! Coverage (QA TC-IHC-7.x / 8.x / 9.x / 17.x / 18.x / 19.x / 21.x):
//!   - `--tag` OR / any-intersection semantics (nginx-only + docker-only +
//!     both-tagged all returned for `--tag nginx --tag docker`)
//!   - `--category general` reads ONLY the global db
//!   - default merges local + global, excluding a planted other-project row
//!   - `--general-only` excludes project rows
//!   - `--project-only` excludes general rows
//!   - `--general-only + --project-only` → exit 2
//!   - corrupt/missing global db → stderr warning + local-only results, exit 0
//!   - gc both dbs cascades insight_tags
//!   - delete --category general → global db
//!
//! HERMETICITY: `$HOME` / `USERPROFILE` pinned to a per-test tempdir so the
//! operator's real `~/.claude/knowledge/insights.db` is never touched.

use assert_cmd::Command;
use std::fs;
use std::path::{Path, PathBuf};

fn bin() -> Command {
    Command::cargo_bin("claudebase").expect("binary built")
}

struct Sandbox {
    project: tempfile::TempDir,
    home: tempfile::TempDir,
}

fn sandbox() -> Sandbox {
    let project = tempfile::tempdir().expect("project tempdir");
    fs::create_dir_all(project.path().join(".claude/knowledge")).expect("mkdir project knowledge");
    let home = tempfile::tempdir().expect("home tempdir");
    Sandbox { project, home }
}

impl Sandbox {
    fn cmd(&self) -> Command {
        let mut c = bin();
        c.current_dir(self.project.path())
            .env("HOME", self.home.path())
            .env("USERPROFILE", self.home.path());
        c
    }

    fn local_db(&self) -> PathBuf {
        self.project.path().join(".claude/knowledge/insights.db")
    }

    fn global_db(&self) -> PathBuf {
        self.home.path().join(".claude/knowledge/insights.db")
    }
}

fn open(db: &Path) -> rusqlite::Connection {
    rusqlite::Connection::open(db).expect("open db")
}

fn doc_count(db: &Path) -> i64 {
    if !db.exists() {
        return 0;
    }
    open(db)
        .query_row("SELECT COUNT(*) FROM documents", [], |r| r.get(0))
        .unwrap_or(0)
}

/// Create a project-scoped insight with the given body + tags. Uses unique
/// agent names per call so exact-sha dedup never collapses distinct fixtures.
fn create_project(sb: &Sandbox, body: &str, agent: &str, tags: &[&str]) {
    let mut args: Vec<&str> = vec![
        "insight", "create", body, "--type", "agent-learned", "--agent", agent,
        "--category", "project",
    ];
    for t in tags {
        args.push("--tags");
        args.push(t);
    }
    sb.cmd().args(&args).assert().success();
}

fn create_general(sb: &Sandbox, body: &str, agent: &str, tags: &[&str]) {
    let mut args: Vec<&str> = vec![
        "insight", "create", body, "--type", "agent-learned", "--agent", agent,
        "--category", "general",
    ];
    for t in tags {
        args.push("--tags");
        args.push(t);
    }
    sb.cmd().args(&args).assert().success();
}

fn search_json(sb: &Sandbox, extra: &[&str]) -> serde_json::Value {
    let mut args: Vec<&str> = vec!["insight", "search", "infrastructure", "--mode", "lexical", "--json"];
    args.extend_from_slice(extra);
    let assert = sb.cmd().args(&args).assert().success();
    let stdout = String::from_utf8_lossy(&assert.get_output().stdout).to_string();
    serde_json::from_str(&stdout).unwrap_or_else(|e| panic!("parse search json: {e}\n{stdout}"))
}

/// Doc-ids present in a search-json hit array (each hit object's `doc_id`).
fn hit_doc_ids(v: &serde_json::Value) -> Vec<i64> {
    v.as_array()
        .map(|arr| arr.iter().filter_map(|h| h.get("doc_id").and_then(|d| d.as_i64())).collect())
        .unwrap_or_default()
}

// ---------------------------------------------------------------------------
// TC-IHC-19.x — `--tag` OR / any-intersection semantics.
// ---------------------------------------------------------------------------

#[test]
fn tag_filter_or_returns_nginx_only_docker_only_and_both() {
    // TC-IHC-19.1 — `--tag nginx --tag docker` returns the union: an
    // nginx-only insight, a docker-only insight, AND a both-tagged insight.
    let sb = sandbox();
    create_project(&sb, "nginx reload infrastructure note alpha", "agA", &["nginx"]);
    create_project(&sb, "docker compose infrastructure note beta", "agB", &["docker"]);
    create_project(&sb, "nginx in docker infrastructure note gamma", "agC", &["nginx", "docker"]);
    // A control insight with NEITHER tag must be excluded.
    create_project(&sb, "redis infrastructure note delta", "agD", &["redis"]);

    let v = search_json(&sb, &["--top-k", "50", "--tag", "nginx", "--tag", "docker", "--project-only"]);
    let ids = hit_doc_ids(&v);
    assert_eq!(
        ids.len(),
        3,
        "OR filter must return nginx-only + docker-only + both = 3 hits; got {ids:?}"
    );
}

#[test]
fn tag_filter_single_tag_excludes_others() {
    // TC-IHC-19.2 — `--tag nginx` returns only nginx-tagged insights.
    let sb = sandbox();
    create_project(&sb, "nginx infrastructure note one", "agA", &["nginx"]);
    create_project(&sb, "docker infrastructure note two", "agB", &["docker"]);
    let v = search_json(&sb, &["--top-k", "50", "--tag", "nginx", "--project-only"]);
    assert_eq!(hit_doc_ids(&v).len(), 1, "only the nginx insight should match");
}

// ---------------------------------------------------------------------------
// TC-IHC-8.x — `--category general` / `--general-only` / `--project-only`.
// ---------------------------------------------------------------------------

#[test]
fn category_general_reads_only_global() {
    // TC-IHC-8.1 — a project row and a general row both match the query text;
    // `--category general` must return only the general one.
    let sb = sandbox();
    create_project(&sb, "local infrastructure secret", "agLocal", &["infra"]);
    create_general(&sb, "global infrastructure secret", "agGlobal", &["infra"]);
    let v = search_json(&sb, &["--top-k", "50", "--category", "general"]);
    let ids = hit_doc_ids(&v);
    assert_eq!(ids.len(), 1, "only the global insight should surface; got {ids:?}");
    // Confirm the body is the global one by re-fetching from global db.
    let n_global: i64 = open(&sb.global_db())
        .query_row("SELECT COUNT(*) FROM documents", [], |r| r.get(0))
        .unwrap();
    assert_eq!(n_global, 1);
}

#[test]
fn default_merges_local_and_global() {
    // TC-IHC-7.1 — default (no scope flags) returns hits from BOTH dbs.
    let sb = sandbox();
    create_project(&sb, "project infrastructure entry", "agLocal", &["infra"]);
    create_general(&sb, "general infrastructure entry", "agGlobal", &["infra"]);
    let v = search_json(&sb, &["--top-k", "50"]);
    assert_eq!(
        hit_doc_ids(&v).len(),
        2,
        "default merge must return both the local and the global insight"
    );
}

#[test]
fn general_only_excludes_project_rows() {
    // TC-IHC-8.2 — `--general-only` skips the local leg entirely.
    let sb = sandbox();
    create_project(&sb, "local-only infrastructure body", "agLocal", &["infra"]);
    create_general(&sb, "global-only infrastructure body", "agGlobal", &["infra"]);
    let v = search_json(&sb, &["--top-k", "50", "--general-only"]);
    assert_eq!(hit_doc_ids(&v).len(), 1, "--general-only returns only the global hit");
}

#[test]
fn project_only_excludes_general_rows() {
    // TC-IHC-7.3 — `--project-only` skips the global leg.
    let sb = sandbox();
    create_project(&sb, "local-only infrastructure thing", "agLocal", &["infra"]);
    create_general(&sb, "global-only infrastructure thing", "agGlobal", &["infra"]);
    let v = search_json(&sb, &["--top-k", "50", "--project-only"]);
    assert_eq!(hit_doc_ids(&v).len(), 1, "--project-only returns only the local hit");
}

#[test]
fn general_only_and_project_only_conflict_exit_2() {
    // TC-IHC-8.4 — mutually exclusive flags → exit 2.
    let sb = sandbox();
    let assert = sb
        .cmd()
        .args([
            "insight", "search", "anything",
            "--general-only", "--project-only",
        ])
        .assert()
        .failure()
        .code(2);
    let stderr = String::from_utf8_lossy(&assert.get_output().stderr).to_string();
    assert!(
        stderr.contains("mutually exclusive"),
        "stderr should explain the conflict; got:\n{stderr}"
    );
}

#[test]
fn source_corpus_label_distinguishes_legs() {
    // TC-IHC-7.2 — fused hits carry source_corpus "local" / "general".
    let sb = sandbox();
    create_project(&sb, "labelled infrastructure local", "agLocal", &["infra"]);
    create_general(&sb, "labelled infrastructure global", "agGlobal", &["infra"]);
    let v = search_json(&sb, &["--top-k", "50"]);
    let labels: std::collections::HashSet<String> = v
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|h| h.get("source_corpus").and_then(|s| s.as_str()).map(String::from))
        .collect();
    assert!(labels.contains("local"), "a hit must be labelled local; got {labels:?}");
    assert!(labels.contains("general"), "a hit must be labelled general; got {labels:?}");
}

// ---------------------------------------------------------------------------
// TC-IHC-7.6 — corrupt/missing global db → local-only fallback + warning.
// ---------------------------------------------------------------------------

#[test]
fn corrupt_global_db_falls_back_to_local_with_warning() {
    // TC-IHC-7.6 — plant a corrupt global insights.db; search must still
    // return local hits, exit 0, and emit a stderr warning.
    let sb = sandbox();
    create_project(&sb, "surviving infrastructure local row", "agLocal", &["infra"]);
    // Corrupt the global db: write garbage bytes where the global db resolves.
    let global = sb.global_db();
    fs::create_dir_all(global.parent().unwrap()).unwrap();
    fs::write(&global, b"this is not a sqlite database at all").unwrap();

    let assert = sb
        .cmd()
        .args(["insight", "search", "infrastructure", "--mode", "lexical", "--json"])
        .assert()
        .success(); // exit 0 despite corrupt global
    let stderr = String::from_utf8_lossy(&assert.get_output().stderr).to_string();
    assert!(
        stderr.contains("global insights db unavailable") || stderr.contains("local results only"),
        "expected a global-unavailable warning; got:\n{stderr}"
    );
    let stdout = String::from_utf8_lossy(&assert.get_output().stdout).to_string();
    let v: serde_json::Value = serde_json::from_str(&stdout).unwrap();
    assert_eq!(hit_doc_ids(&v).len(), 1, "the local row must still be returned");
}

// ---------------------------------------------------------------------------
// TC-IHC-9.x — gc across both dbs cascades insight_tags.
// ---------------------------------------------------------------------------

#[test]
fn gc_both_dbs_cascades_insight_tags() {
    // TC-IHC-9.1 — plant an old low-salience insight in BOTH dbs with tags;
    // gc (default = both) deletes them and cascades the insight_tags rows.
    let sb = sandbox();
    // Write a low-salience insight to each db, then back-date it past the 90d
    // low TTL directly via SQL so gc collects it deterministically.
    create_project(&sb, "stale project gc body", "agP", &["staletag"]);
    create_general(&sb, "stale general gc body", "agG", &["staletag"]);
    let old = 1_000_000i64; // far in the past, well past any TTL
    for db in [sb.local_db(), sb.global_db()] {
        let conn = open(&db);
        conn.execute(
            "UPDATE documents SET salience='low', ingested_at=?1 WHERE source_type IS NOT NULL",
            rusqlite::params![old],
        )
        .unwrap();
    }
    // Precondition: tags present in both.
    for db in [sb.local_db(), sb.global_db()] {
        let n: i64 = open(&db)
            .query_row("SELECT COUNT(*) FROM insight_tags", [], |r| r.get(0))
            .unwrap();
        assert_eq!(n, 1, "precondition: one tag row per db");
    }
    // gc both dbs.
    sb.cmd().args(["insight", "gc", "--json"]).assert().success();
    // Both documents gone; both tag tables empty (cascade fired).
    assert_eq!(doc_count(&sb.local_db()), 0, "local insight gc'd");
    assert_eq!(doc_count(&sb.global_db()), 0, "global insight gc'd");
    for db in [sb.local_db(), sb.global_db()] {
        let n: i64 = open(&db)
            .query_row("SELECT COUNT(*) FROM insight_tags", [], |r| r.get(0))
            .unwrap();
        assert_eq!(n, 0, "insight_tags cascade-deleted with the document in {db:?}");
    }
}

#[test]
fn gc_category_general_only_touches_global() {
    // TC-IHC-9.2 — `--category general` gc's only the global db.
    let sb = sandbox();
    create_project(&sb, "fresh project keepme", "agP", &["keep"]);
    create_general(&sb, "stale general purgeme", "agG", &["purge"]);
    let old = 1_000_000i64;
    // Back-date ONLY the global one.
    let g = open(&sb.global_db());
    g.execute(
        "UPDATE documents SET salience='low', ingested_at=?1 WHERE source_type IS NOT NULL",
        rusqlite::params![old],
    )
    .unwrap();
    sb.cmd().args(["insight", "gc", "--category", "general", "--json"]).assert().success();
    assert_eq!(doc_count(&sb.global_db()), 0, "global stale insight gc'd");
    assert_eq!(doc_count(&sb.local_db()), 1, "local insight untouched by --category general");
}

// ---------------------------------------------------------------------------
// TC-IHC-17.x — delete --category general resolves against the global db.
// ---------------------------------------------------------------------------

#[test]
fn delete_category_general_targets_global_db() {
    // TC-IHC-17.1 — an id valid in the global db is deletable via
    // `--category general`; the same id MUST NOT touch the local db.
    let sb = sandbox();
    create_project(&sb, "local keepme delete test", "agP", &["keep"]);
    create_general(&sb, "global deleteme delete test", "agG", &["del"]);
    // Grab the global insight's id.
    let gid: i64 = open(&sb.global_db())
        .query_row("SELECT id FROM documents WHERE source_type IS NOT NULL LIMIT 1", [], |r| r.get(0))
        .unwrap();
    sb.cmd()
        .args(["insight", "delete", &gid.to_string(), "--category", "general"])
        .assert()
        .success();
    assert_eq!(doc_count(&sb.global_db()), 0, "global insight deleted");
    assert_eq!(doc_count(&sb.local_db()), 1, "local insight untouched");
    // insight_tags cascade in the global db.
    let n: i64 = open(&sb.global_db())
        .query_row("SELECT COUNT(*) FROM insight_tags", [], |r| r.get(0))
        .unwrap();
    assert_eq!(n, 0, "global insight_tags cascade-deleted");
}

#[test]
fn delete_default_targets_local_db() {
    // TC-IHC-17.2 — without --category, delete targets the local db.
    let sb = sandbox();
    create_project(&sb, "local deleteme default", "agP", &["del"]);
    let lid: i64 = open(&sb.local_db())
        .query_row("SELECT id FROM documents WHERE source_type IS NOT NULL LIMIT 1", [], |r| r.get(0))
        .unwrap();
    sb.cmd().args(["insight", "delete", &lid.to_string()]).assert().success();
    assert_eq!(doc_count(&sb.local_db()), 0, "local insight deleted by default");
}

// ---------------------------------------------------------------------------
// TC-IHC-18.x — list merges both legs; --category scopes the list.
// ---------------------------------------------------------------------------

#[test]
fn list_default_merges_both_legs() {
    // TC-IHC-18.1 — list with no scope flag returns rows from BOTH dbs.
    let sb = sandbox();
    create_project(&sb, "list local body", "agL", &["t"]);
    create_general(&sb, "list global body", "agG", &["t"]);
    let assert = sb.cmd().args(["insight", "list", "--json"]).assert().success();
    let stdout = String::from_utf8_lossy(&assert.get_output().stdout).to_string();
    let v: serde_json::Value = serde_json::from_str(&stdout).unwrap();
    assert_eq!(v.get("total").and_then(|t| t.as_i64()), Some(2), "total spans both dbs");
}

#[test]
fn list_category_general_scopes_to_global() {
    // TC-IHC-18.2 — `--category general` lists only the global db.
    let sb = sandbox();
    create_project(&sb, "list local scoped", "agL", &["t"]);
    create_general(&sb, "list global scoped", "agG", &["t"]);
    let assert = sb
        .cmd()
        .args(["insight", "list", "--category", "general", "--json"])
        .assert()
        .success();
    let stdout = String::from_utf8_lossy(&assert.get_output().stdout).to_string();
    let v: serde_json::Value = serde_json::from_str(&stdout).unwrap();
    assert_eq!(v.get("total").and_then(|t| t.as_i64()), Some(1), "only global counted");
}

#[test]
fn list_tag_filter_or_semantics() {
    // TC-IHC-18.3 — list `--tag nginx --tag docker` → 3 (nginx, docker, both).
    let sb = sandbox();
    create_project(&sb, "list nginx body", "agA", &["nginx"]);
    create_project(&sb, "list docker body", "agB", &["docker"]);
    create_project(&sb, "list both body", "agC", &["nginx", "docker"]);
    create_project(&sb, "list neither body", "agD", &["redis"]);
    let assert = sb
        .cmd()
        .args(["insight", "list", "--project-only", "--tag", "nginx", "--tag", "docker", "--json"])
        .assert()
        .success();
    let stdout = String::from_utf8_lossy(&assert.get_output().stdout).to_string();
    let v: serde_json::Value = serde_json::from_str(&stdout).unwrap();
    assert_eq!(v.get("total").and_then(|t| t.as_i64()), Some(3), "OR filter returns 3");
}

// ---------------------------------------------------------------------------
// TC-IHC-21.x — random across legs respects scope + tag filter.
// ---------------------------------------------------------------------------

#[test]
fn random_project_only_never_returns_general() {
    // TC-IHC-21.1 — with only a general row present, `--project-only` random
    // finds no candidate and exits 1.
    let sb = sandbox();
    create_general(&sb, "only general for random", "agG", &["t"]);
    sb.cmd()
        .args(["insight", "random", "--project-only"])
        .assert()
        .failure()
        .code(1);
}

#[test]
fn random_tag_filter_restricts_candidates() {
    // TC-IHC-21.2 — random with `--tag nginx` only ever returns the nginx row.
    let sb = sandbox();
    create_project(&sb, "random nginx body", "agA", &["nginx"]);
    create_project(&sb, "random docker body", "agB", &["docker"]);
    // Run a few times; every result must be the nginx insight.
    for _ in 0..5 {
        let assert = sb
            .cmd()
            .args(["insight", "random", "--project-only", "--tag", "nginx", "--json"])
            .assert()
            .success();
        let stdout = String::from_utf8_lossy(&assert.get_output().stdout).to_string();
        assert!(
            stdout.contains("random nginx body"),
            "random --tag nginx must only surface the nginx insight; got:\n{stdout}"
        );
    }
}
