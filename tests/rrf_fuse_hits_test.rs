//! Slice 5 (insights-hybrid-corpus) — `rrf_fuse_hits` correctness tests.
//!
//! `rrf_fuse_hits(local, general, top_k)` is the arg-type-agnostic Reciprocal
//! Rank Fusion helper that drives the dual-DB insight read path. Unlike
//! `rrf_fuse` (keyed on `chunk_id` alone), this helper keys hit identity on
//! `(source_corpus, chunk_id)` so a LOCAL chunk and a GENERAL chunk that
//! happen to share the SAME integer `chunk_id` survive fusion as TWO distinct
//! hits. chunk_ids are scoped per-DB, so an unkeyed fusion would collapse them
//! — the bug the architect (insights-base doc#15) and red-team (doc#18) flagged.

use claudebase::search::{rrf_fuse_hits, SearchHit};

/// Build a tagged hit. `corpus` populates `source_corpus` so fusion can key on it.
fn tagged_hit(chunk_id: i64, corpus: &str, score: f64) -> SearchHit {
    SearchHit {
        chunk_id,
        doc_id: chunk_id, // arbitrary; identity is (corpus, chunk_id)
        source: format!("/tmp/{corpus}.{chunk_id}.md"),
        ord: 0,
        score,
        snippet: String::new(),
        page_start: None,
        page_end: None,
        context: None,
        mode_used: Some("hybrid".to_string()),
        bm25_score: None,
        dense_score: None,
        rrf_score: None,
        source_corpus: Some(corpus.to_string()),
    }
}

#[test]
fn chunk_id_collision_local_and_general_survive_as_two_hits() {
    // TC-IHC-7.5 — a local chunk_id=42 and a general chunk_id=42 are DISTINCT.
    let local = vec![tagged_hit(42, "local", 5.0)];
    let general = vec![tagged_hit(42, "general", 4.0)];
    let fused = rrf_fuse_hits(local, general, 10);
    assert_eq!(
        fused.len(),
        2,
        "local chunk_id=42 and general chunk_id=42 must survive as 2 distinct hits"
    );
    // Both corpora must be represented.
    let corpora: std::collections::HashSet<String> = fused
        .iter()
        .map(|h| h.source_corpus.clone().unwrap_or_default())
        .collect();
    assert!(corpora.contains("local"), "local hit present");
    assert!(corpora.contains("general"), "general hit present");
}

#[test]
fn fuse_uses_default_local_label_when_source_corpus_none() {
    // A hit with no source_corpus is treated as "local" for keying. Two such
    // hits with the same chunk_id collapse (correct — both are local-default).
    let mut a = tagged_hit(7, "local", 5.0);
    a.source_corpus = None;
    let mut b = tagged_hit(7, "local", 4.0);
    b.source_corpus = None;
    let fused = rrf_fuse_hits(vec![a], vec![b], 10);
    assert_eq!(
        fused.len(),
        1,
        "two None-corpus hits with same chunk_id key identically and collapse"
    );
}

#[test]
fn fuse_ranks_both_corpora_hits_higher() {
    // A chunk appearing in BOTH legs (same corpus label) outranks single-leg
    // hits — proves the RRF accumulation still works across legs.
    let local = vec![
        tagged_hit(1, "local", 5.0),
        tagged_hit(2, "local", 4.0),
    ];
    // chunk 1 also surfaces in the general leg — but as a DISTINCT (general,1).
    // To test same-key accumulation, re-feed (local,1) via the general arg.
    let general = vec![tagged_hit(1, "local", 9.9)];
    let fused = rrf_fuse_hits(local, general, 10);
    // (local,1) seen twice → highest RRF; (local,2) once; total 2 distinct keys.
    assert_eq!(fused.len(), 2, "(local,1) accumulates; (local,2) single");
    assert_eq!(fused[0].chunk_id, 1, "(local,1) ranks first (two contributions)");
    assert!(fused[0].rrf_score.is_some(), "rrf_score populated");
}

#[test]
fn fuse_empty_inputs_yield_empty() {
    let fused = rrf_fuse_hits(vec![], vec![], 10);
    assert!(fused.is_empty());
}

#[test]
fn fuse_top_k_truncates() {
    let local = vec![
        tagged_hit(1, "local", 5.0),
        tagged_hit(2, "local", 4.0),
        tagged_hit(3, "local", 3.0),
    ];
    let fused = rrf_fuse_hits(local, vec![], 2);
    assert_eq!(fused.len(), 2, "top_k=2 truncates the 3-hit local leg");
}
