# Article staging — claudebase

> Staging directory for the upcoming Medium article. Source materials live in `docs/architecture/` and `docs/benchmarks/`; this directory holds the article-specific cuts and any supplementary figures.

## Source materials

- **Foundation walkthrough**: [`docs/architecture/technical-decisions.md`](../architecture/technical-decisions.md), section "How vector search works end-to-end" — the 5-step flow from ingest-time encoding through K-NN to RRF fusion. This is the load-bearing pedagogical content.
- **Architectural rationale**: same file, sections "Why hybrid retrieval", "Why L2 distance with cosine-equivalent ranking", "Why fastembed-rs (not raw ort)", "Why ocr-rs (MNN) instead of paddle-ocr-rs (ONNX)". Stack-decision narrative.
- **Concrete benchmarks**: [`docs/benchmarks/2026-05-10-baseline.md`](../benchmarks/2026-05-10-baseline.md) — +75% Recall@5 over lexical baseline, +94% MRR, p95 latency 85ms over 75 895 chunks. Headline numbers + per-mode side-by-side samples.

## Article outline (proposed)

1. **Hook**: a Russian query that BM25 misses but hybrid finds — concrete demo.
2. **Setup**: the corpus (39 PDFs, RU+EN, ML/data-eng/SRE/system-design domains).
3. **The three retrieval modes**:
   - Lexical (FTS5 BM25) — what it gets right, what it misses.
   - Dense (sqlite-vec, e5-multilingual-small, 384-dim, L2-normalized) — semantic + cross-lingual recall.
   - Hybrid (RRF k=60) — why fusion beats either pure mode without score normalization.
4. **The math nobody tells you**: L2 vs cosine on unit-norm vectors are the same ranking. Skip the conversion.
5. **The asymmetric trick**: e5's `passage:` / `query:` prefix discipline.
6. **Multimodal honestly**: PaddleOCR PP-OCRv4 via MNN (not ONNX, because of dependency conflicts), figure → text → embed in same 384-dim space.
7. **What broke and how we fixed it**: the post-merge schema-v3 page-column rename, the GHA `ort-sys` prebuilt-binary gap on darwin-x64 / linux-arm64.
8. **Numbers**: the benchmark table.
9. **What's next**: cross-page chunkers, ANN index for >1M chunks.

## Stub status

This file is a placeholder. Once the article is drafted, replace this overview with the actual Medium-ready Markdown.
