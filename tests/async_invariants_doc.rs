//! Slice 1c — ASYNC_INVARIANTS.md presence + completeness test.
//!
//! The invariants document is the primary deliverable of Slice 1c. It is
//! the tokio-async-specialist's reference at audit time and prevents
//! re-discovery of the five async-discipline rules from code archaeology.
//!
//! This test enforces:
//!   - file exists at the canonical path `src/daemon/ASYNC_INVARIANTS.md`
//!   - file is non-trivial (≥ 30 lines per slice spec "Verify:" field)
//!   - all five named invariants are mentioned by topic-keyword

use std::fs;
use std::path::PathBuf;

fn invariants_path() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("src/daemon/ASYNC_INVARIANTS.md")
}

#[test]
fn invariants_file_exists() {
    let p = invariants_path();
    assert!(
        p.exists(),
        "expected ASYNC_INVARIANTS.md at {} — Slice 1c primary deliverable",
        p.display()
    );
}

#[test]
fn invariants_file_is_at_least_30_lines() {
    let p = invariants_path();
    let body = fs::read_to_string(&p).expect("read ASYNC_INVARIANTS.md");
    let line_count = body.lines().count();
    assert!(
        line_count >= 30,
        "expected ≥30 lines per slice 1c verify clause, got {line_count}"
    );
}

#[test]
fn invariants_file_covers_all_five_rules() {
    let p = invariants_path();
    let body = fs::read_to_string(&p).expect("read ASYNC_INVARIANTS.md");
    // Lowercase comparison so the doc author has freedom over capitalisation.
    let body_lower = body.to_lowercase();

    // Each rule has a discriminating keyword cluster — the OR-set tolerates
    // wording variation while still proving the topic is covered.
    let rules: [(&str, &[&str]); 5] = [
        ("rule 1 — sync fn main", &["sync fn main", "synchronous main", "fn main()"]),
        ("rule 2 — no .await while holding Mutex", &[".await", "mutex"]),
        ("rule 3 — panic-safe tokio::spawn", &["tokio::spawn", "panic"]),
        ("rule 4 — cancellation-safe tokio::select!", &["tokio::select", "cancellation"]),
        ("rule 5 — no .unwrap() in spawned tasks", &[".unwrap", "spawned"]),
    ];

    for (label, needles) in rules.iter() {
        let any_present = needles.iter().any(|n| body_lower.contains(&n.to_lowercase()));
        assert!(
            any_present,
            "ASYNC_INVARIANTS.md missing coverage for {label} — expected at least one of {needles:?}"
        );
    }
}
