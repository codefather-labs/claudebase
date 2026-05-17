## §15. Vector + Multimodal Retrieval Backend

**Status:** [IN DEVELOPMENT]
**Date:** 2026-05-09
**Priority:** High
**Related:** §11 (pdfium-render PDF extraction — Docling replaces pdfium as primary parser, pdfium kept as fallback). §12 (schema v1 FTS5 store — schema bumped to v2 with `chunks_vec` virtual table, amendments documented in §15.9). §14 (plan auto-persist — the user-approved plan at `.claude/plan.md` is the authoritative source for this section).

Changelog: The knowledge-base search tool now understands your queries semantically — matching concepts and cross-lingual paraphrases rather than exact keywords — and can also find text embedded in figures and diagrams extracted from PDFs.

### 15.1 Feature Description

The `claudebase` retrieval backend shipped in 0.3.x uses BM25-only lexical search via SQLite FTS5 with a naïve 500-character sliding-window chunker and pdfium-text-only PDF extraction. This produces three concrete user-facing limitations on the existing 51 K-chunk multilingual corpus: (1) a Russian query never matches an English chunk that covers the same concept, because the FTS5 `unicode61` tokenizer is purely lexical; (2) tables flatten poorly, headings do not influence chunking, and figures are dropped entirely, so retrieval misses content BM25 can never see; (3) paraphrases ("how do I authenticate" vs. "JWT validation") do not match. This feature replaces the BM25-only backend with a **hybrid lexical + dense retrieval layer** (BM25 ⊕ K-NN dense via Reciprocal Rank Fusion k=60), **structurally-aware document parsing** via Docling (IBM, Apache-2.0) with pdfium fallback, and **OCR-based multimodal embeddings** so figures from PDFs are searchable through unified cosine similarity in the same 384-dimensional `intfloat/multilingual-e5-small` embedding space as text and tables. A benchmark harness ships alongside the new backend and quantifies Recall@K, MRR, NDCG@10, and latency across all three search modes against a 25-query golden set grounded in the user's multilingual corpus.

### 15.2 User Story

As a developer using the SDLC pipeline with a curated multilingual knowledge base, I want hybrid lexical + dense retrieval with multimodal awareness so that cross-lingual queries, paraphrase-style queries, and queries whose answers live inside figures and diagrams all return relevant chunks — not just queries whose exact keywords appear in text.

### 15.3 Functional Requirements

#### FR-VR-1: Structurally-Aware Document Parser (Docling integration — Slice 3)

1. **FR-VR-1.1:** The ingest path MUST attempt Docling as the primary PDF backend when Docling model files are present at `~/.claude/tools/claudebase/models/docling/`. On Docling error or absent models, the path MUST fall back to pdfium and log a warning.
2. **FR-VR-1.2:** Docling output (Markdown + figure list) MUST feed the structural chunker (FR-VR-2) so that section hierarchy is preserved in chunk metadata.
3. **FR-VR-1.3:** The fallback to pdfium MUST produce non-empty output for any PDF that pdfium can extract; the fallback MUST be tested by a fixture `sample-structured.pdf` ingested with Docling models absent.
4. **FR-VR-1.4:** **Pragmatic fallback (OQ-1):** If the architect pre-review (Slice 3) determines Docling is not feasible in a zero-Python Rust binary, Slice 3 de-scopes to "structural chunker over pdfium output" and Docling is deferred to v2. This de-scope is permissible without violating this PRD section — FR-VR-2 (structural chunker) still ships regardless of whether the parser is Docling or pdfium.

#### FR-VR-2: Heading-Aware Structural Chunker (Slice 1)

1. **FR-VR-2.1:** A new `chunker::structural_chunk()` function MUST parse Markdown and plain-text input for `^#{1,6}\s+` heading patterns and "Chapter/Section N" markers, chunking on heading boundaries with a soft cap of 1 500 characters and 200-character overlap.
2. **FR-VR-2.2:** When no headings are detected, `structural_chunk()` MUST fall back to the current 500-character sliding-window output (backward-compatible — existing regression tests pass unchanged).
3. **FR-VR-2.3:** The existing `ingest::chunk()` at `src/ingest.rs:71` MUST be replaced with a thin call to `chunker::structural_chunk()`.
4. **FR-VR-2.4:** A fixture `sample-with-headings.md` containing exactly three headings MUST yield exactly three chunks each starting with its heading line; a fixture `sample-no-headings.md` MUST yield the same chunk count as the iter-1 baseline.

#### FR-VR-3: Schema v2 — Vector Table, Image BLOB Column, Migration UX (Slice 2)

1. **FR-VR-3.1:** The `sqlite-vec` extension MUST be linked at connection-open time. A new virtual table `CREATE VIRTUAL TABLE chunks_vec USING vec0(embedding float[384])` MUST be created in the same `index.db` file as the FTS5 table — preserving the single-file invariant (NFR-VR-4).
2. **FR-VR-3.2:** Two new columns MUST be added to the `chunks` table: `type TEXT NOT NULL DEFAULT 'text'` (allowed values: `'text'`, `'table'`, `'image'`) and `image_bytes BLOB NULL` (populated only for `type='image'` chunks).
3. **FR-VR-3.3:** The schema version MUST be bumped from 1 to 2. `claudebase status --json` on a fresh v2 database MUST return `"schema_version": 2`.
4. **FR-VR-3.4:** When the v2 binary opens a v1 index, it MUST detect the version mismatch and: (a) if a TTY is attached, prompt "Re-ingest required for v2 schema. Proceed? [y/N]"; (b) if `CLAUDEKNOWS_AUTO_REINGEST=1` is set, skip the prompt; (c) on user refusal, exit 0 with a hint message; (d) on acceptance or env-var, drop and recreate the schema, then exit 0 with a hint to re-run `ingest`.
5. **FR-VR-3.5:** A truncated or corrupt v1 database MUST honor the iter-1 AC-7 contract: exit 1 with the literal message `error: index database invalid; re-ingest required`.

#### FR-VR-4: Text Encoder + Ingest-Time Embedding (Slice 5)

1. **FR-VR-4.1:** An `Encoder` singleton (mutex-guarded, lazy-loaded) MUST load the `intfloat/multilingual-e5-small` ONNX model from `~/.claude/tools/claudebase/models/e5-small/`.
2. **FR-VR-4.2:** Two public methods MUST be provided: `encode_passages(&[&str]) -> Vec<Vec<f32>>` which prepends `"passage: "` to each input, and `encode_query(&str) -> Vec<f32>` which prepends `"query: "` to the input. The e5 prefix discipline MUST be enforced and covered by dedicated tests (`encoder_prefix_test.rs`).
3. **FR-VR-4.3:** Ingest MUST batch chunks at `batch_size=32` and write 384-dimensional float vectors to `chunks_vec`. After a complete ingest, the row count in `chunks_vec` MUST equal the row count in `chunks`.
4. **FR-VR-4.4:** When model files are absent, the encoder MUST initialize in degraded mode that returns `Err` on every encode call. Ingest MUST catch this and fall back to BM25-only indexing. `claudebase status --json` MUST report `"degraded": "encoder model missing"` in degraded mode.
5. **FR-VR-4.5:** On a 2024 MacBook M1 (reference machine): encoder cold-start MUST be below 3 seconds; hot-path batch of 32 chunks MUST complete below 50 ms.

#### FR-VR-5: OCR Bridge for Image Chunks (Slice 6)

1. **FR-VR-5.1:** For each `type='image'` chunk containing a non-NULL `image_bytes` BLOB, the ingest pipeline MUST load the PNG bytes, run the OCR model (PaddleOCR det+rec via `ort`, or architect-selected alternative per OQ-3), and set `chunk.text` to the OCR'd text.
2. **FR-VR-5.2:** If OCR returns empty output (non-textual diagram), `chunk.text` MUST be set to the placeholder `[image: figure N from <doc-basename>]`.
3. **FR-VR-5.3:** Each image chunk's text (OCR'd or placeholder) MUST be encoded via the Slice 5 encoder and written to `chunks_vec`, making image chunks part of the unified 384-dim e5 search space.
4. **FR-VR-5.4:** A fixture `diagram-with-text.png` containing the literal text "Authentication Service" MUST yield a cosine similarity above 0.5 between the query `"auth service architecture"` (encoded via `encode_query`) and the corresponding stored embedding.
5. **FR-VR-5.5:** When OCR model files are absent, all `type='image'` chunks MUST receive placeholder text with a warning logged; ingest MUST continue without hard failure.

#### FR-VR-6: Hybrid Search — Three Modes with RRF (Slice 7)

1. **FR-VR-6.1:** A `dense_search(query, top_k)` function MUST encode the query via `encode_query()`, run K-NN over `chunks_vec` using the sqlite-vec distance function, and return the top-K results.
2. **FR-VR-6.2:** A `hybrid_search(query, top_k)` function MUST run BM25 top-(K×4) and dense top-(K×4) in parallel, merge results via Reciprocal Rank Fusion with k=60 per the formula `score(d) = Σ_i 1/(60 + rank_i(d))`, and return the top-K results.
3. **FR-VR-6.3:** The CLI `--mode` flag MUST accept values `lexical`, `dense`, and `hybrid`. The default MUST be `hybrid`.
4. **FR-VR-6.4:** JSON output from all three modes MUST be extended with fields `mode_used`, `bm25_score`, `dense_score`, and `rrf_score`.
5. **FR-VR-6.5:** RRF correctness MUST be covered by a unit test (`rrf_test.rs`) providing three known input rankings and verifying the output matches the expected merged ranking exactly.
6. **FR-VR-6.6:** When dense mode is requested but the encoder model is absent, the CLI MUST exit 1 with the message `"encoder model missing"`. When hybrid mode is requested with no encoder model, the CLI MUST fall back to lexical mode with a warning printed to stderr.
7. **FR-VR-6.7:** On a 2024 MacBook M1 reference machine, hybrid p95 latency over a fixed sequence of 30 queries against the 51 K-chunk corpus MUST be below 500 ms.

#### FR-VR-7: Benchmark Harness + Report (Slices 9 and 10)

1. **FR-VR-7.1:** A standalone binary `claudebase-bench` (declared as `[[bin]]` in `Cargo.toml`) MUST accept `--queries <path>` and `--modes <comma-list>` flags and produce a Markdown benchmark report.
2. **FR-VR-7.2:** The golden query set at `bench/golden/queries.jsonl` MUST contain at least 25 manually-curated queries with fields: `id`, `query`, `lang` (values `ru`, `en`, or `cross`), `relevant_chunk_ids`, `relevant_docs`, and `category` (values `keyword`, `nl`, `cross`, or `paraphrase`).
3. **FR-VR-7.3:** The benchmark MUST compute and report for each mode: Recall@1, Recall@3, Recall@5, Recall@10, Precision@5, MRR (mean reciprocal rank, 1/rank of first relevant result), NDCG@10, per-document recall (fraction of relevant documents hit), and latency p50/p95.
4. **FR-VR-7.4:** The committed benchmark report at `bench/reports/2026-05-09-vector-vs-bm25.md` MUST include: methodology, dataset description, query categorization, metric tables per mode, latency, top-10 qualitative side-by-side samples for 5–10 representative queries, failure-mode taxonomy, and recommendations.
5. **FR-VR-7.5:** Per-language metric stratification is **out of scope** per OQ-4 resolution — the report includes overall metrics plus qualitative side-by-side samples only.

#### FR-VR-8: Install Scripts, Model Bundles, and Rule Updates (Slice 11)

1. **FR-VR-8.1:** `install.sh` and `install.ps1` MUST add `install_e5_model`, `install_paddleocr_models`, and `install_docling_models` functions following the existing `install_pdfium_binary` pattern. Total model footprint at install time is approximately 200 MB.
2. **FR-VR-8.2:** After fresh install, the following directories MUST exist: `~/.claude/tools/claudebase/models/e5-small/`, `~/.claude/tools/claudebase/models/paddleocr/`, and `~/.claude/tools/claudebase/models/docling/`.
3. **FR-VR-8.3:** `src/rules/knowledge-base-tool.md` MUST have the assertion "**NOT a vector database. No embeddings, no semantic similarity.**" removed and replaced with a description of the three search modes and hybrid retrieval.
4. **FR-VR-8.4:** `src/rules/knowledge-base.md` MUST be updated to reference three search modes, hybrid retrieval, image chunks, and schema v2.
5. **FR-VR-8.5:** `README.md` MUST gain a "Vector + Multimodal Retrieval" subsection in the Hardening table referencing the benchmark report.

### 15.4 Non-Functional Requirements

1. **NFR-VR-1:** The `claudebase` binary itself MUST remain below 10 MB. If static-linking `ort` or `sqlite-vec` would breach this limit, those libraries MUST be shipped as dynamic loads following the pdfium pattern. (Risk R9 — architect pre-review at Slice 5 validates.)
2. **NFR-VR-2:** Hybrid search p95 latency MUST be below 500 ms on a 2024 MacBook M1 over the user's 51 K-chunk corpus (same reference machine used for encoder latency in FR-VR-4.5).
3. **NFR-VR-3:** Full re-ingest of approximately 40 PDFs (the user's books corpus) MUST complete within 15 minutes on CPU (M1/M2 MacBook). Wall-clock time is captured and documented in Slice 8.
4. **NFR-VR-4 (Single-file invariant — amends §11 NFR-1.5):** The `index.db` SQLite file remains the sole persistent artifact of the knowledge base. Image PNG bytes are stored as `chunks.image_bytes BLOB` INSIDE `index.db`. The `chunks_vec` virtual table is INSIDE `index.db`. No co-located figure files or vector store files outside the database.
5. **NFR-VR-5:** Zero Python dependencies. All ML inference runs via `ort` (Rust ONNX Runtime). If Docling requires Python orchestration and no feasible Rust integration exists, Docling is deferred to v2 (FR-VR-1.4 fallback) — the zero-Python constraint is non-negotiable.
6. **NFR-VR-6:** Model footprint at install time MUST not exceed approximately 200 MB total (e5-small ~120 MB + PaddleOCR ~30 MB + Docling models ~50 MB). Binary size is excluded from this budget.
7. **NFR-VR-7:** All changes are Rust source files, test fixtures, install scripts, and Markdown documentation. No agent prompt files are modified by this feature.
8. **NFR-VR-8:** Backward compatibility for BM25-only mode: `claudebase search "<query>" --mode lexical` MUST work even when all model files are absent, providing identical behavior to the iter-1 baseline.

### 15.5 Acceptance Criteria

Each criterion is bash-runnable or grep-verifiable by a test runner or human reviewer:

1. **AC-VR-1 (Schema v2):** `claudebase status --json | jq '.schema_version'` returns `2` on a fresh post-install database.
2. **AC-VR-2 (Search modes — lexical):** `claudebase search "authentication architecture" --mode lexical --json | jq '.[0].mode_used'` returns `"lexical"`.
3. **AC-VR-3 (Search modes — dense):** `claudebase search "authentication architecture" --mode dense --json | jq '.[0].mode_used'` returns `"dense"`.
4. **AC-VR-4 (Search modes — hybrid):** `claudebase search "authentication architecture" --mode hybrid --json | jq '.[0].mode_used'` returns `"hybrid"`.
5. **AC-VR-5 (Default mode is hybrid):** `claudebase search "authentication architecture" --json | jq '.[0].mode_used'` returns `"hybrid"` (no `--mode` flag supplied).
6. **AC-VR-6 (RRF correctness):** `cargo test --test rrf_test -p claudebase` exits 0.
7. **AC-VR-7 (Image chunks searchable):** After re-ingesting the books corpus, `claudebase search "figure diagram" --mode dense --json | jq '[.[] | select(.type=="image")] | length'` returns a value greater than 0.
8. **AC-VR-8 (Benchmark report exists):** `test -f bench/reports/2026-05-09-vector-vs-bm25.md && echo EXISTS` prints `EXISTS`.
9. **AC-VR-9 (Rule updated — no stale assertion):** `grep -rF "NOT a vector database" ~/.claude/rules/` returns zero matches after a fresh `bash install.sh --yes`.
10. **AC-VR-10 (Rule updated — hybrid present):** `grep -E "hybrid|RRF|sqlite-vec" ~/.claude/rules/knowledge-base.md | wc -l` returns a count greater than or equal to 1.
11. **AC-VR-11 (Structural chunker — headings):** `cargo test --test chunker_test -p claudebase` exits 0; the fixture with three headings yields exactly three chunks.
12. **AC-VR-12 (Migration UX — corrupt v1 DB):** Placing a truncated v1 fixture `index.db` in a temp dir and running `claudebase status --json --project-root <tmpdir>` exits 1 and stdout/stderr contains the substring `index database invalid`.
13. **AC-VR-13 (Migration UX — headless):** With `CLAUDEKNOWS_AUTO_REINGEST=1` and a v1 fixture DB, running any `claudebase` command exits 0 without prompting.
14. **AC-VR-14 (Model-missing degraded mode):** With model files removed, `claudebase search "anything" --mode dense` exits 1 with the substring `encoder model missing`; `claudebase search "anything" --mode lexical` exits 0.
15. **AC-VR-15 (Image BLOB integrity):** `cargo test --test image_extraction_test -p claudebase` exits 0 and the test asserts that `image_bytes` decodes to a valid PNG via `image::load_from_memory`.
16. **AC-VR-16 (e5 prefix discipline):** `cargo test --test encoder_prefix_test -p claudebase` exits 0; the test asserts every passage input starts with `"passage: "` and every query input starts with `"query: "`.
17. **AC-VR-17 (chunks_vec parity):** After a full ingest, `SELECT COUNT(*) FROM chunks` equals `SELECT COUNT(*) FROM chunks_vec` (verifiable via sqlite3 CLI on `index.db`).

### 15.6 Affected Files

**New files [NEW]:**
- `src/chunker.rs` [NEW]
- `src/docling.rs` [NEW]
- `src/encoder.rs` [NEW]
- `src/ocr.rs` [NEW]
- `tests/chunker_test.rs` [NEW]
- `tests/store_v2_test.rs` [NEW]
- `tests/migration_test.rs` [NEW]
- `tests/docling_test.rs` [NEW]
- `tests/image_extraction_test.rs` [NEW]
- `tests/encoder_test.rs` [NEW]
- `tests/encoder_prefix_test.rs` [NEW]
- `tests/ocr_test.rs` [NEW]
- `tests/search_modes_test.rs` [NEW]
- `tests/rrf_test.rs` [NEW]
- `tests/fixtures/sample-with-headings.md` [NEW]
- `tests/fixtures/sample-no-headings.md` [NEW]
- `tests/fixtures/sample-structured.pdf` [NEW]
- `tests/fixtures/sample-with-figure.pdf` [NEW]
- `tests/fixtures/sample-with-multiple-figures.pdf` [NEW]
- `tests/fixtures/diagram-with-text.png` [NEW]
- `bench/runner.rs` [NEW]
- `bench/metrics.rs` [NEW]
- `bench/golden/queries.jsonl` [NEW]
- `bench/golden/README.md` [NEW]
- `bench/reports/2026-05-09-vector-vs-bm25.md` [NEW]
- `docs/use-cases.md` [NEW]
- `docs/qa.md` [NEW]

**Modified files [MODIFIED]:**
- `claudebase/Cargo.toml` [MODIFIED] — new dependencies (`fastembed`/`ort`, `sqlite-vec`, `image`); new `[[bin]]` for `claudebase-bench`; version bump deferred to `/release`
- `claudebase/Cargo.lock` [MODIFIED]
- `src/ingest.rs` [MODIFIED] — calls structural chunker; Docling/pdfium routing; image chunk queue; encoder batch writes
- `src/store.rs` [MODIFIED] — sqlite-vec extension load; `chunks_vec` table creation; new columns
- `src/migrations.rs` [MODIFIED] — v1→v2 migration logic and UX
- `src/search.rs` [MODIFIED] — dense_search and hybrid_search functions; RRF merge
- `src/cli.rs` [MODIFIED] — `--mode` flag; output field extensions
- `src/output.rs` [MODIFIED] — `mode_used`, `bm25_score`, `dense_score`, `rrf_score` JSON fields
- `install.sh` [MODIFIED] — model download functions for e5, PaddleOCR, Docling
- `install.ps1` [MODIFIED] — Windows equivalents
- `README.md` [MODIFIED] — "Vector + Multimodal Retrieval" subsection
- `src/rules/knowledge-base.md` [MODIFIED] — schema v2, three modes, hybrid retrieval, image chunks
- `src/rules/knowledge-base-tool.md` [MODIFIED] — remove "NOT a vector database" assertion; update description (create file if absent in `src/rules/`)
- `docs/PRD.md` [MODIFIED] — this §15 section
- `CHANGELOG.md` [MODIFIED] — `[Unreleased]` entry added by `changelog-writer` at `/merge-ready`

**Intentionally unchanged:**
- All 17 agent prompt files (`src/agents/*.md`)
- All 7 slash-command files (`src/commands/*.md`)
- `templates/` directory

### 15.7 Out of Scope

The following items are explicitly excluded from this feature:

1. **Pure-vision CLIP-space embeddings.** Embedding images in a CLIP embedding space (separate from the e5 text space) would require a parallel index in a different dimensionality. Deferred to v3 pending benchmark evidence that OCR-as-text bridge is insufficient.
2. **Per-language benchmark stratification.** 25 queries split across multiple languages produces statistically marginal per-language metrics. The benchmark reports overall metrics plus qualitative side-by-side samples only (OQ-4, resolved).
3. **Semantic re-ranking (cross-encoder).** Adding a cross-encoder re-ranking step after hybrid retrieval is an iter-3 enhancement; not included here.
4. **Auto-publish or version bump in this feature.** Version bump 0.3.x → 0.4.0 happens via the user-invoked `/release` command after merge, not in any implementation slice.
5. **Windows native installer for model bundles.** The `install.ps1` changes add model downloads but the Windows-native CI pipeline for testing them is left to a subsequent CI hardening feature.

### 15.8 Risks

1. **R1 — Docling Rust integration (CRITICAL, OQ-1).** Docling is a Python library with no first-class Rust SDK; direct ONNX inference, sidecar binary, or alternative parser are the three options. **Mitigation:** pragmatic fallback — if architect Slice 3 pre-review rules Docling unfeasible, Slice 3 de-scopes to "structural chunker over pdfium output is sufficient for v1"; vector backend (Slices 2/5/7) + OCR multimodal (Slice 6) + benchmark (Slices 9/10) still deliver the primary win.
2. **R2 — sqlite-vec linking strategy (OQ-2).** Static-link via `rusqlite-bundled` vs. runtime `Connection::load_extension`. **Mitigation:** prefer static-link; fall back to runtime-load if static build fails on any target. Architect Slice 2 pre-review decides.
3. **R3 — Model bundle size (+200 MB at install).** Mitigation: install-time download per the pdfium pattern; lazy degraded-mode fallback if models are missing (encoder degraded → BM25-only; OCR degraded → placeholder text). Binary stays below 10 MB.
4. **R4 — v1→v2 migration UX on large corpora.** Re-encoding 51 K chunks takes approximately 10 minutes on CPU. **Mitigation:** `CLAUDEKNOWS_AUTO_REINGEST=1` for headless; clear TTY prompt; corrupt-v1 honors AC-7 contract.
5. **R5 — Benchmark fairness.** BM25 and dense must use the same post-Slice-1 structural-chunker output to isolate retrieval-method differences from chunking effects. Slice 9 enforces this invariant.
6. **R6 — OCR quality on schematic diagrams.** PaddleOCR is optimized for natural text; quality degrades on diagrams with irregular fonts, arrows, and labels. **Mitigation:** benchmark Slice 10 surfaces real numbers; if poor, iter-2 may add layout-aware diagram parsers. Placeholder text ensures image chunks remain searchable even when OCR fails.
7. **R7 — e5 prefix discipline drift.** Forgetting `"passage: "` / `"query: "` prefixes silently degrades retrieval quality by 5–10%. **Mitigation:** `encoder_prefix_test.rs` mocks the ONNX call and asserts prefix discipline at the unit-test level.
8. **R8 — Plan-mode persistence.** The plan body is auto-persisted to `<project>/.claude/plan.md` per the rule shipped in 0.3.1 (§14). This is a built-in precondition, not a risk.
9. **R9 — Binary-size budget breach from static ONNX runtime.** Static-linking `ort` can add 20–40 MB. **Mitigation:** investigate `ort` linkage modes; if static blows the 10 MB budget, ship `ort` as a dynamic load following the pdfium pattern. Architect Slice 5 pre-review validates the linkage strategy.
10. **R10 — Cargo.toml multi-wave serialization.** Six slices touch `Cargo.toml` across five waves. **Mitigation:** each edit ADDS a new dependency entry only, never modifying existing entries; sequential wave merges preserve correctness.

### 15.9 Amendments to Prior PRD Sections

#### Amendment to §11 FR-4.3 (Scope Reduction Detection — Plan Critic finding identifier reuse)

**Original §11 FR-4.3** (pipeline hardening) defined: "The finding MUST identify the specific hedging phrase, the slice where it appears, and the PRD requirement it violates." This is a Plan Critic output format requirement unrelated to the database schema.

**§15 supersession of the iter-1 schema reservation:** §11 reserved an `embedding BLOB` column on the `chunks` table for non-destructive iter-2 migration (documented in the §11 Facts block). PRD §15 **supersedes that reservation** — the `embedding BLOB` column on `chunks` is NOT added. Instead, a separate `chunks_vec` virtual table from the `sqlite-vec` extension is used. **Rationale:** `sqlite-vec` is purpose-built for vector K-NN queries, exposes a native `vec_distance_cosine` function, and operates as an independent virtual table that does not interfere with FTS5 triggers. Storing 384 × 4 = 1 536 bytes inline on every `chunks` row would bloat the FTS5 content table and complicate partial-update migrations. The virtual-table approach cleanly separates lexical and dense storage. The iter-1 `embedding BLOB` reservation is now archival; §15 FR-VR-3.1 is canonical.

#### Clarification of §11 NFR-1.5 (Single-File SQLite Invariant)

**§11 NFR-1.5** mandates that the knowledge base consists of a single SQLite file (`index.db`). **§15 confirms this invariant is preserved:** image PNG bytes are stored as `chunks.image_bytes BLOB` INSIDE the same `index.db` file (FR-VR-3.2). The `chunks_vec` virtual table is also INSIDE `index.db` (FR-VR-3.1). No figure files, no separate vector store files, and no sidecar databases exist outside `index.db`. The single-file invariant holds.

## Facts

### Verified facts

- Current `claudebase` v0.3.1 uses BM25-only FTS5 retrieval, schema v1, ~4 MB binary — verified against `claudebase/Cargo.toml` and `src/store.rs` (plan.md Verified facts, verified via plan Read this session).
- 500-character sliding-window chunker is at `src/ingest.rs:71` — verified via plan.md Verified facts.
- Knowledge-base corpus: 28 documents, 51 542 chunks — verified by `claudebase status --json` run in this session (output: `{"schema_version":1,"doc_count":28,"chunk_count":51542}`).
- Corpus contains English and Russian content: `claudebase list --json` returned filenames including `841031560_Современная_программная_инженерия_2023.pdf`, `Али_Аминиан_и_другие_System_Design_Подготовка_к_сложному_интервью.pdf`, `Хаос_инжиниринг_2021_Кейси_Розенталь,_Нора_Джонс.pdf` (Russian) and `Hands-On Machine Learning with Pytorch.pdf`, `Building_AI_Agents_With_LLMs_RAG_And_Knowledge_Graphs.pdf` (English) — verified this session.
- Corpus scope relevance: **Overlap**. Observed corpus domain: ML/AI, data engineering, RAG, vector search, generative AI, LLM agents, system design, SRE. Task domain: vector retrieval, dense embeddings, hybrid search, multimodal RAG. The domain overlap is direct — all key concepts in this PRD section (embeddings, RAG, BM25, hybrid retrieval, chunking, OCR) appear in the corpus.
- Detected corpus languages: English and Russian — confirmed by language probes in `claudebase list --json` and search hits in both languages this session.
- Plan at `.claude/plan.md` confirmed as authoritative source: read in full this session (lines 1–349). 26 Plan Critic findings (7 CRITICAL, 13 MAJOR, 6 MINOR) all addressed. User approved verbatim.
- `src/migrations.rs` and `src/store.rs` confirmed to exist — stated in plan.md Verified facts.
- e5 prompt-prefix discipline (`"passage: "` for ingest, `"query: "` for search) documented on the `intfloat/multilingual-e5-small` model card — verified: yes (plan.md External contracts entry marked `verified: yes`).
- RRF formula `score(d) = Σ_i 1/(k + rank_i(d))` with k=60 from Cormack et al. 2009 — verified: yes (plan.md External contracts entry marked `verified: yes`).
- `docs/PRD.md` sections §1–§14 exist; §15 is the next available number — confirmed by reading PRD end (lines 3462–3616) and section grep this session.

### External contracts

- knowledge-base: Али_Аминиан_и_другие_System_Design_Подготовка_к_сложному_интервью.pdf:44359 — query: "векторный поиск" — BM25: 19.937681073031765 — verified: yes
- knowledge-base: Али_Аминиан_и_другие_System_Design_Подготовка_к_сложному_интервью.pdf:44368 — query: "векторный поиск" — BM25: 19.44132131158577 — verified: yes
- knowledge-base: Али_Аминиан_и_другие_System_Design_Подготовка_к_сложному_интервью.pdf:44368 — query: "семантический поиск" — BM25: 19.152063060870095 — verified: yes
- knowledge-base: 934216520_Mastering_LangChain_A_Comprehensive_Guide_to_Building.pdf:37926 — query: "dense retrieval semantic similarity" — BM25: 24.120519498482583 — verified: yes
- knowledge-base: 923991015_Generative_AI_With_LangChain_Build_Production_ready_LLM.pdf:26011 — query: "dense retrieval semantic similarity" — BM25: 23.387287506230457 — verified: yes
- knowledge-base: 923991015_Generative_AI_With_LangChain_Build_Production_ready_LLM.pdf:26013 — query: "BM25 ranking" — BM25: 19.714764291301655 — verified: yes
- knowledge-base: 923991015_Generative_AI_With_LangChain_Build_Production_ready_LLM.pdf:26006 — query: "BM25 ranking" — BM25: 13.666352175833016 — verified: yes
- knowledge-base: 947059230_AI_Agents_and_Applications_Roberto_Infante_bibis_ir.pdf:23504 — query: "RAG retrieval" — BM25: 13.320964774336012 — verified: yes
- knowledge-base: 908530342_Building_AI_Agents_With_LLMs_RAG_And_Knowledge_Graphs.pdf:39244 — query: "chunking document structure headings" — BM25: 24.68298434658531 — verified: yes
- knowledge-base: 908530342_Building_AI_Agents_With_LLMs_RAG_And_Knowledge_Graphs.pdf:38743 — query: "multimodal embeddings image text" — BM25: 17.842685437373483 — verified: yes
- **`fastembed-rs` (Qdrant, crates.io `fastembed = "4"`)** — symbol: `TextEmbedding::try_new(InitOptions { model_name: EmbeddingModel::MultilingualE5Small, ... })`, `embed(documents: Vec<&str>, batch_size: Option<usize>) -> Vec<Vec<f32>>` — source: https://github.com/Anush008/fastembed-rs — verified: **no — assumption**. Architect Slice 5 pre-review MUST verify e5-small is in fastembed's supported model list and the API shape matches. Risk: if fastembed does not support e5-small, fall back to raw `ort`.
- **`sqlite-vec` extension** — symbol: `vec0` virtual table; `embedding float[384]` column declaration; `vec_distance_cosine(a, b)` distance function; static or runtime linking via `rusqlite` — source: https://github.com/asg017/sqlite-vec — verified: **no — assumption**. Architect Slice 2 pre-review MUST decide static-vs-runtime linking and verify cross-platform build. Risk: static linking may not be available on all platforms.
- **`ort` Rust ONNX Runtime v2.x** — symbol: `ort::Session::builder().commit_from_file(path)`, `Session::run(inputs) -> Result<Outputs>` — source: https://docs.rs/ort/2 — verified: **no — assumption**. Used transitively by fastembed-rs and directly by PaddleOCR and Docling integrations. Risk: API shape may differ across minor versions.
- **Docling (IBM, Apache-2.0)** — ONNX model artifacts at `https://huggingface.co/ds4sd/docling-models`; outputs structured Markdown + DocLink JSON — source: https://github.com/DS4SD/docling — verified: **no — assumption (CRITICAL)**. Docling has no first-class Rust SDK. Architect Slice 3 pre-review picks the integration strategy (direct ONNX, sidecar CLI, or alternative parser). Pragmatic fallback: if unfeasible, Slice 3 de-scopes and Docling defers to v2.
- **PaddleOCR det+rec ONNX** — symbols: detection model `ch_PP-OCRv4_det_infer.onnx`, recognition model `ch_PP-OCRv4_rec_infer.onnx`, multilingual variant `ml_PP-OCRv4_*_infer.onnx` (~30 MB combined) — source: https://github.com/PaddlePaddle/PaddleOCR — verified: **no — assumption**. Architect Slice 6 picks between PaddleOCR, trocr, and Tesseract. Risk: model filenames and ONNX export format may differ from assumption.
- **`intfloat/multilingual-e5-small` model card** — symbol: `"passage: "` prefix for indexed passages, `"query: "` prefix for search queries; 384-dimensional output; ONNX export available — source: https://huggingface.co/intfloat/multilingual-e5-small — verified: yes (documented on model card; plan.md entry marked `verified: yes`).
- **Reciprocal Rank Fusion k=60** — symbol: `score(d) = Σ_i 1/(60 + rank_i(d))` — source: Cormack, Clarke, and Buettcher, "Reciprocal Rank Fusion outperforms Condorcet and individual Rank Learning Methods," SIGIR 2009 — verified: yes (plan.md entry marked `verified: yes`; canonical value in industry use).

### Assumptions

- ONNX runtime via `ort` works on all target platforms (macOS arm64/x64, Linux x64/arm64, Windows x64). ARM Windows and FreeBSD are not covered. Verify: build matrix in Slice 11 install scripts.
- 51 K chunks at encode batch=32 on CPU (M1/M2 MacBook) takes ≤10 minutes for full re-ingest. Verify: time the actual re-ingest in Slice 8 and document in scratchpad.
- 25 manually-curated queries are sufficient to detect a meaningful retrieval-quality difference between modes. Verify: spot-check qualitative samples; expand to 50 if benchmark results are inconclusive.
- Image bytes as BLOB column add tolerable storage overhead (~4 MB per 50-page PDF with 20 figures; ~112 MB for 28 docs). Verify: measure DB file size growth in Slice 4.
- e5-small embedding quality is sufficient on technical-book content in both English and Russian. Risk: bge-m3 (~2 GB) might be measurably better. Verify: benchmark Slice 10 — if hybrid recall is unimpressive, iter-2 may swap the encoder.
- The `chunks_vec` row count equaling the `chunks` row count after ingest is a sufficient integrity check. Risk: rows could be inserted out of sync if a batch write fails partway. Verify: Slice 5 tests include a mid-batch failure injection.

### Open questions

- **OQ-1 (Docling integration strategy)** — load-bearing for Slice 3 architect pre-review. Three options: direct ONNX, Python sidecar, or alternative parser (Marker, MinerU, heuristic over pdfium). Decision must land before Slice 3 implementation. Pragmatic fallback is FR-VR-1.4.
- **OQ-2 (sqlite-vec linking)** — static-link via rusqlite-bundled vs. runtime `load_extension`. Architect Slice 2 pre-review decides.
- **OQ-3 (OCR model selection)** — PaddleOCR, trocr, or Tesseract. Architect Slice 6 pre-review decides and pins exact ONNX model filenames.
- knowledge-base: searched "hybrid retrieval RRF reciprocal rank fusion" → 0 hits in any language; consider adding an information-retrieval reference (e.g., the Cormack 2009 paper or BEIR benchmark docs) to the knowledge-base corpus for future retrieval-engineering tasks. This is not a corpus gap for this PRD section — the RRF formula is verified directly from the canonical paper citation in plan.md.


---

## §16. Agent Insights Base

**Status:** [IN DEVELOPMENT — Slices 1-3 + admin surface shipped]
**Date:** 2026-05-16
**Priority:** High
**Related:** §15 (vector + multimodal retrieval — agent-insights reuses the v2/v3 hybrid retrieval stack and pdfium parser; schema v4 adds nullable metadata columns on `documents`). §11 (pdfium). §14 (plan auto-persist). Design doc: `docs/design/agent-insights-base.md` is the technical source of truth and supersedes this section on implementation detail.

Changelog: skip — internal (developer-facing tool surface; no end-user behavior change in claudebase. Surfaced to SDLC agents via prompt integration in Slice 8.)

### 16.1 Feature Description

Today every Claude Code session is cognitively isolated. The SDLC pipeline emits new knowledge per session — Reflection observations, Consolidator drift findings, Red-Team adversarial objections, decision rationales — but the knowledge dies at session end because it lives in stdout, scratchpads, and one-shot artifact files that the next session does not read. This feature extends `claudebase` (the local hybrid-retrieval CLI shipped in §15) with a **parallel insights corpus** — a second SQLite database `insights.db` alongside the existing books `index.db` — that agents write to via `claudebase insight create` and read from via `claudebase insight search / list / random / get`. The hippocampal analogue is exact: prior sessions' load-bearing cognitive insights survive into future sessions where they can ground decisions.

The scope is deliberately narrow. The corpus holds **cognitive insights only** along three axes: (1) self-learning (`agent-learned`, `self-bias-caught`), (2) peer-bias detection (`peer-bias-observed`, `red-team-objection`, `consolidator-drift`), and (3) prediction-reality mismatch (`prediction-error`, `assumption-falsified`, `plan-reality-gap`). Factual findings, mechanical narration, restatements of input, and generic best-practice claims do NOT belong in the corpus — they go to PRs, scratchpads, issue trackers, or stay silent.

### 16.2 User Story

As an SDLC pipeline operator, I want each Claude Code session to start with the cognitive insights prior sessions accumulated so that decisions ground in what previous agents actually learned, peer-bias was caught against, and predictions failed against — instead of re-discovering the same lessons every session.

### 16.3 Functional Requirements

#### FR-AIB-1: Schema v4 Migration (Slice 1 — DONE)

1. **FR-AIB-1.1:** A `SCHEMA_V4_DELTA` constant MUST add six nullable columns to `documents`: `source_type`, `agent_name`, `session_id`, `feature_slug`, `salience`, `parent_artifact`.
2. **FR-AIB-1.2:** Four indexes MUST be created: `idx_documents_source_type`, `idx_documents_agent_name`, `idx_documents_feature`, `idx_documents_salience`.
3. **FR-AIB-1.3:** Schema progression MUST support v0→4 (fresh), v2→4 (transactional), v3→4 (transactional), v4 idempotent re-open via pragma probe + safe ADD COLUMN.
4. **FR-AIB-1.4:** Books-corpus rows (existing `documents` entries in `index.db`) MUST be unaffected — all six new columns default to NULL.

#### FR-AIB-2: `--db-name` Parameterization (Slice 2 — DONE)

1. **FR-AIB-2.1:** `SearchArgs`, `ListArgs`, `StatusArgs`, `DeleteArgs` MUST accept `--db-name <name>` (default `index.db`).
2. **FR-AIB-2.2:** A `validate_db_name` helper MUST reject path separators, parent-directory escapes, and hidden-file prefixes (except canonical `index.db` / `insights.db`).
3. **FR-AIB-2.3:** The `resolve_project_root` security backbone MUST remain the only path-from-user-input gate; `db_name` resolves to `<project>/.claude/knowledge/<db_name>`.

#### FR-AIB-3: `claudebase insight create` (Slice 3 — DONE)

1. **FR-AIB-3.1:** `insight create "<body>" --type <kind> --agent <agent>` MUST persist one row to `insights.db` with the v4 metadata columns populated.
2. **FR-AIB-3.2:** Body MUST be readable from (a) positional argument, (b) `-` positional with piped stdin, (c) omitted positional with piped stdin. Interactive TTY without a body MUST exit 2 with usage.
3. **FR-AIB-3.3:** Exact-sha dedup: when `(agent_name, sha256)` matches a row ingested within the last 30 days, the write MUST be skipped and the response MUST report `status: deduped` with the existing `doc_id`.
4. **FR-AIB-3.4:** Cross-agent same-body MUST NOT be deduped — two agents independently surfacing the same observation IS load-bearing signal.
5. **FR-AIB-3.5:** The synthesized `source_path` MUST encode `agent:{agent}:{session}:{feature}:{sha[..16]}` for stable in-session re-write idempotency.
6. **FR-AIB-3.6:** Dense vector population into `chunks_vec` MUST be best-effort — silent no-op when the e5 encoder is unavailable (degraded-mode parity with the books ingest path).

#### FR-AIB-4: `claudebase insight search` (Slice 3 — DONE)

1. **FR-AIB-4.1:** `insight search "<query>"` MUST default to hybrid mode (BM25 ⊕ dense via RRF k=60) against `insights.db`.
2. **FR-AIB-4.2:** Encoder unavailable OR `chunks_vec` missing MUST trigger auto-fallback to lexical with a stderr warning — parity with the standalone `search` subcommand.
3. **FR-AIB-4.3 (PLANNED — Slice 4):** Metadata filters MUST be honored: `--type`, `--agent`, `--salience`, `--feature`, `--since <30d>`. Filters apply as SQL WHERE clauses on the `documents` row before chunk ranking.

#### FR-AIB-5: `claudebase insight list / random / get` (Slice 3 admin surface — DONE)

1. **FR-AIB-5.1:** `insight list [--offset N] [--page-size N]` MUST return newest-first paginated summaries. Default page size 10, capped at 100. `--offset 0` returns the latest page.
2. **FR-AIB-5.2:** `insight list` MUST honor the same metadata filters as `insight search` (FR-AIB-4.3).
3. **FR-AIB-5.3:** `insight random` MUST uniform-sample one insight, optionally filtered; exit 1 on empty corpus.
4. **FR-AIB-5.4:** `insight get <ident>` MUST accept integer `documents.id` OR a hex sha256 prefix of ≥4 characters (matched via `sha256 LIKE 'prefix%'`).
5. **FR-AIB-5.5:** `insight get` MUST reject too-short prefixes (`<4` chars) and non-hex identifiers with exit 2.
6. **FR-AIB-5.6:** Returned records MUST carry the reconstructed body — chunks joined with the 100-char chunker overlap collapsed.

#### FR-AIB-6: Semantic Dedup (Slice 5 — PLANNED)

1. **FR-AIB-6.1:** In addition to exact-sha dedup, `insight create` MUST run new chunks against the dense index with a cosine-similarity threshold of 0.92.
2. **FR-AIB-6.2:** A near-duplicate hit from the same agent within 30 days MUST skip the write and log `cf-dedup: near-duplicate of doc #N`.
3. **FR-AIB-6.3:** For `source_type=consolidator-drift`, dedup key MUST be the pair of cited `file:line` references rather than the body text alone.

#### FR-AIB-7: `--corpus books|insights|all` on Standalone `search` (Slice 6 — PLANNED)

1. **FR-AIB-7.1:** The existing `search` subcommand MUST gain a `--corpus` flag. `books` (default) opens `index.db`; `insights` opens `insights.db`; `all` opens both, runs hybrid search on each, RRF-fuses across, and emits hits with a `source_corpus` field.
2. **FR-AIB-7.2:** Backward compatibility: `--corpus books` MUST be byte-identical to today's `search` output.

#### FR-AIB-8: TTL-Driven Garbage Collection + Manual Delete (Slice 7 — PLANNED)

1. **FR-AIB-8.1:** `insight gc` MUST purge rows whose retention TTL has elapsed: `salience=high` retained indefinitely; `salience=medium` retained 365 days; `salience=low` retained 90 days.
2. **FR-AIB-8.2:** After purge, `gc` MUST run `VACUUM` and FTS5/vec compact.
3. **FR-AIB-8.3:** `insight delete <id>` MUST remove a single insight by integer id with chunks cascade-deletion (same FK shape the books corpus uses).
4. **FR-AIB-8.4:** `gc` and `delete` MUST emit a JSON summary on `--json`: `{deleted: N, freed_bytes: B}` for gc; `{deleted_id: N, source_path: ..., chunks_removed: M}` for delete.

#### FR-AIB-9: SDLC Agent-Prompt Integration (Slice 8 — PLANNED)

1. **FR-AIB-9.1:** Each of the 13 in-scope thinking agents (cognitive-self-check `## Application Scope`) plus `reflection`, `consolidator`, `red-team` MUST receive two new prompt sections: `## Insight retrieval (MANDATORY at task receipt)` and `## Insight surfacing (MANDATORY at task end, when applicable)`.
2. **FR-AIB-9.2:** Retrieval MUST call `claudebase insight search` filtered by feature-slug + salience-high+medium and surface hits in the agent's `## Facts → Verified facts` block as cross-session memory.
3. **FR-AIB-9.3:** Surfacing MUST call `claudebase insight create --type <enum> --agent <self> --salience <tag>` ONLY for insights matching the three-axis cognitive taxonomy (self-learning / peer-bias / prediction-reality) — never for factual findings.

#### FR-AIB-10: Rule Documentation (Slice 9 — PLANNED)

1. **FR-AIB-10.1:** `~/.claude/rules/knowledge-base-tool.md` MUST gain a section distinguishing the two corpora (books = user-curated, insights = agent-written) and the salience-tag retention contract.
2. **FR-AIB-10.2:** `~/.claude/rules/cognitive-self-check.md` MUST tie the salience tag explicitly to retention (high=∞, medium=1y, low=90d) so agents understand the cost of marking insights as high-salience.

#### FR-AIB-11: Cross-Repo E2E Verification (Slice 10 — PLANNED)

1. **FR-AIB-11.1:** A full-flow E2E test MUST exercise: insight write → list non-zero → recall top hit → cross-corpus search → gc backdating semantics.

### 16.4 Non-Functional Requirements

- **NFR-AIB-1: Per-project isolation.** Insights are scoped per `<project>/.claude/knowledge/insights.db`. No cross-project read or write surface in v1.
- **NFR-AIB-2: Schema additivity.** v4 migration MUST NOT touch existing v3 rows or break v3 readers of the books corpus.
- **NFR-AIB-3: Security backbone preserved.** All path-from-user-input continues to funnel through `cli::resolve_project_root` (the single gate established in §15).
- **NFR-AIB-4: Books-corpus zero-touch.** `insight create` MUST NOT create or modify `index.db` — verified by the `create_does_not_create_index_db` test in `tests/cli_insight_e2e_test.rs`.

## Facts

### Verified facts

- The schema-v4 migration applies via `store::open_or_init_v2` with v0/v2/v3→4 progression and v4 idempotent re-open — verified by `tests/store_v2_test.rs` (`schema_version_is_four_on_fresh_v2_db`) and `tests/store_test.rs` (`fresh_db_has_four_tables`) — salience: high.
- The 19 E2E tests in `tests/cli_insight_e2e_test.rs` cover all five `insight` subcommands and all FR-AIB-3 / FR-AIB-4.1/4.2 / FR-AIB-5 requirements — verified by `cargo test --test cli_insight_e2e_test` returning 19/19 pass at commit `e7bcc1c` — salience: high.
- The full claudebase test surface (23 suites) passes alongside the new tests — verified by `cargo test` exiting 0 across all suites at commit `e7bcc1c` — salience: high.
- The synthesized `source_path` shape `agent:{agent}:{session}:{feature}:{sha[..16]}` is exercised by `create_source_path_encodes_metadata_segments` — verified — salience: medium.

### External contracts

- **SQLite FTS5 + `bm25()` function** — symbol: `bm25(chunks_fts)`; ranking via `-bm25(...) AS score ORDER BY score DESC` (positive larger-is-better in JSON) — source: `src/search.rs:75` (existing §15 contract, unchanged in this feature) — verified: yes — salience: medium.
- **`sqlite-vec` `vec0` virtual table** — symbol: `CREATE VIRTUAL TABLE chunks_vec USING vec0(embedding float[384])` — source: §15 FR-VR-3.1 (existing contract, unchanged) — verified: yes — salience: medium.
- **`intfloat/multilingual-e5-small` model** — used via `encoder::encode_passages` / `encode_query` for dense vector population on `insight create` — source: §15 contract — verified: yes (model loads via `fastembed` after `claudebase warmup`) — salience: medium.
- **rusqlite `INSERT ... ON CONFLICT(source_path) DO UPDATE`** — symbol: parameterized via `?1..?10` — source: `store::upsert_insight_document` at `src/store.rs` — verified: yes — salience: high.

### Assumptions

- The 30-day exact-sha dedup window is the right default. Risk: agents in long-running sessions may re-emit identical insights and the dedup masks novelty signal. How to verify: monitor `deduped` vs `written` ratio in real pipeline runs; tune if dedup hit rate exceeds 30%. Salience: medium.
- The 100-char chunker overlap collapse in `reconstruct_body_from_chunks` matches the ingest chunker exactly. Risk: a future chunker change would silently corrupt body reconstruction. How to verify: existing test `get_by_integer_id_returns_full_record` round-trips the body and asserts the literal string survives. Salience: medium.
- The synthetic `source_path` `agent:...` prefix never collides with a real file path in the documents table. Risk: a user file literally named starting with `agent:` could collide. How to verify: filesystems prohibit `:` in path components on most platforms; on macOS/Linux the prefix is unique in practice. Salience: low.

### Open questions

- knowledge-base: corpus is AI / ML / RAG books; task is agent-cognitive-memory infrastructure; partial overlap. The retrieval mechanics (BM25 / RRF / sqlite-vec) are well-covered (verified against `Generative_AI_With_LangChain`, `AI_Agents_and_Applications`, `Building_AI_Agents_With_LLMs_RAG_And_Knowledge_Graphs` in §15 PRD citations). The cognitive-bias / hippocampal-replay framing is NOT in the corpus and was not queried for this PRD — the framing comes from `~/.claude/CLAUDE.md` neuroscience-inspired protocols. Salience: low.
- Should `insight gc` run automatically on a schedule, or only via explicit user invocation? Current plan (FR-AIB-8): manual only. Salience: low.

## Decisions

### Inbound validation

- The user's session-driven refinement (the original plan said `claudebase remember`, mid-session the user asked to restructure under `insight create` plus admin surface). Challenged: yes — Mira pushed back on the apparent scope expansion mid-implementation. Outcome: proceeded as restructured — the unified `insight` namespace is cleaner than two top-level commands (`remember` + `insights status/list/delete`) and the cost of the rename is one search-and-replace in tests. Salience: medium.

### Decisions made

- Unified `insight <subcommand>` tree replacing the planned `remember` + `insights status/list/delete/gc` split. Alternatives considered: (a) keep `remember` as the write verb and `insights` as the admin namespace — rejected because two top-level surfaces fragment agent prompt integration; (b) put everything under `insights` (plural) — rejected because the singular form reads more naturally (`claudebase insight create "..."` vs `claudebase insights create "..."`). Q1-Q5: hack? no. Sane? yes. Alternatives evaluated. Symptom-or-cause? cause. n/a. Salience: high.
- `insight get <ident>` accepts both integer id and hex sha prefix in the same positional, disambiguated by `i64::parse`. Alternatives: separate `--by-id` and `--by-sha` flags — rejected as more verbose for the common interactive case. Q1-Q5: hack? no. Sane? yes. Alternatives evaluated. Salience: medium.
- Exact-sha dedup is `agent_name`-keyed, NOT global. The same body emitted by two agents produces two rows. Reasoning: cross-agent agreement on an observation IS load-bearing signal — deduping it would mask consensus. Q1-Q5: hack? no. Sane? yes. Alternatives evaluated. Symptom-or-cause? cause. Salience: high.

### Hacks acknowledged

(none) — no shipped band-aids. The pre-v4 schema-v3 path still works untouched for the books corpus.

### Symptom-only patches (with root-cause links)

(none) — no symptom-only patches in this feature.

---

## §17. Agent Chat Daemon + Telegram Bridge + ASR Pipeline + Claude Code Plugin

**Status:** [PLANNED]
**Date:** 2026-05-17
**Priority:** High
**Related:** §16 (Agent Insights Base — `chat.db` and `agent_registry` extend the SQLite sibling-file pattern established by `insights.db`). §15 (Vector + Multimodal Retrieval Backend — UDS IPC framing follows the same single-binary, zero-Python constraint). §11 (pdfium — `daemon install` post-install hook extends the pattern from `install_pdfium_binary` in `install.sh`). Plan source: `.claude/plan.md` (374 lines, read in full this session).

Changelog: Adds a persistent claudebase daemon with Telegram bridge, voice transcription via Whisper, Parakeet/NeMo, or NVIDIA NIM, and a chat-based communication channel between Claude Code agents and the user.

### 17.1 Feature Description

Today `claudebase` is a request-response retrieval CLI: each invocation opens a SQLite index, runs a query, prints results, and exits. The 22 SDLC subagents run as parallel monologues — there is no shared communication channel, no way for the user to interject mid-pipeline via voice from a mobile device, and no persistence of agent-to-user dialog across Claude Code sessions.

This feature extends `claudebase` with a **persistent OS-level daemon** (`claudebase daemon serve`) that runs as a system service (systemd on Linux, launchd on macOS, Windows Service on Windows), auto-starting at boot. The daemon hosts three coupled subsystems: (1) a **Telegram bot** for user input (text and voice notes), (2) an **ASR pipeline** (three configurable backends — whisper-rs local, sherpa-onnx local ONNX, NVIDIA NIM cloud) that transcribes voice notes into text, and (3) a **Claude Code MCP plugin bridge** (`claudebase plugin serve`) that exposes the daemon's chat and agent-registry tools to any running Claude Code session via a Unix Domain Socket or named pipe.

The motivation is architectural: a stable daemon backed by SQLite persistence turns today's stateless 22-agent ensemble into a system the user can communicate with in real time, on the go, via Telegram voice. When Claude Code is running, the orchestrator Mira receives Telegram messages as `notifications/claude/channel` events through the plugin and can route replies to specific subagents via `SendMessage`. When Claude Code is not running, messages are persisted to `chat.db` and delivered as a backlog on the next session start. The single-binary Rust constraint — no bundled Python, no Node.js — is non-negotiable. (Plan §"Why", lines 11–22; plan §"Architecture", lines 28–61.)

### 17.2 User Story

As the SDLC pipeline operator, I want a persistent Telegram bot backed by a local Rust daemon so that I can send voice notes from my phone during a pipeline run, have them transcribed automatically, and have the right subagent receive my message and respond in Telegram — without needing to keep a terminal open or write to a file manually.

### 17.3 Functional Requirements

#### FR-ACD-1: Daemon Lifecycle — `claudebase daemon` Subcommand Tree (Slice 2, plan lines 119–128)

1. **FR-ACD-1.1:** `claudebase daemon install [--yes] [--no-start]` MUST generate the per-OS service unit (systemd user unit on Linux, launchd user agent plist on macOS, Windows Service via `sc.exe`), write `~/.claude/plugins/claudebase/.mcp.json` (the user-level Claude Code plugin descriptor), and register the service. The operation MUST be idempotent — re-running on an already-installed system is a no-op with exit 0.
2. **FR-ACD-1.2:** `claudebase daemon uninstall [--keep-data]` MUST remove the service unit and deregister the service. With `--keep-data`, `chat.db`, `insights.db`, `secrets.toml`, and `daemon.toml` MUST be preserved. Without `--keep-data`, those files MUST also be removed.
3. **FR-ACD-1.3:** `claudebase daemon start` MUST activate the service via the OS-native mechanism (`systemctl --user start claudebase`, `launchctl load ...`, Windows `sc start`).
4. **FR-ACD-1.4:** `claudebase daemon stop` MUST deactivate the service gracefully.
5. **FR-ACD-1.5:** `claudebase daemon restart` MUST execute stop then start.
6. **FR-ACD-1.6:** `claudebase daemon status` MUST return JSON with fields: `state` (values: `"running"`, `"stopped"`, `"not-installed"`), `pid` (integer or null), `uptime` (seconds or null), `socket_path` (string or null), `subscriber_count` (integer), `tg_bot_state` (`"connected"`, `"disconnected"`, `"not-configured"`), `asr_backend` (`"whisper"`, `"sherpa-nemo"`, `"nim"`, `"none"`).
7. **FR-ACD-1.7:** `claudebase daemon logs [--lines N] [--follow]` MUST tail the OS-native log stream (`journalctl --user -u claudebase` on Linux, `log show --predicate 'process == "claudebase"'` on macOS, `Get-WinEvent` on Windows).
8. **FR-ACD-1.8:** `claudebase daemon serve` MUST be the actual long-running process entry point. It MUST enforce single-instance via `fslock` PID-file at `$XDG_RUNTIME_DIR/claudebase/daemon.pid` (Unix) or `%LOCALAPPDATA%\claudebase\daemon.pid` (Windows). A second invocation MUST exit 1 with the message `claudebase daemon: already running (pid N)`.

#### FR-ACD-2: IPC Transport — Unix Domain Socket / Named Pipe (Slice 1, plan lines 95–113)

1. **FR-ACD-2.1:** The daemon MUST listen on `$XDG_RUNTIME_DIR/claudebase/daemon.sock` on Unix and `\\.\pipe\claudebase-daemon` on Windows.
2. **FR-ACD-2.2:** The framing protocol MUST be length-prefixed JSON frames (4-byte big-endian length header + UTF-8 JSON body). Message types MUST be `Request`, `Response`, and `Notification`.
3. **FR-ACD-2.3:** Each accepted connection MUST be assigned a unique `connection_id` (UUID v4). The `connection_id` is used by the plugin bridge and `agent_registry` to scope state.
4. **FR-ACD-2.4:** The daemon MUST support concurrent connections (minimum: two simultaneous plugin bridge connections for AC-ACD-8 — two concurrent Claude Code sessions).
5. **FR-ACD-2.5:** On connection EOF, the daemon MUST mark all `agent_registry` rows with that `connection_id` as `state = 'orphaned'` and log the event.

#### FR-ACD-3: MCP Plugin Bridge — `claudebase plugin` Subcommand Tree (Slice 1, plan lines 95–113)

1. **FR-ACD-3.1:** `claudebase plugin serve` MUST implement a minimal MCP server over STDIO — handling `initialize`, `tools/list`, `tools/call`, and `notifications/claude/channel` per the JSON-RPC 2.0 MCP wire format.
2. **FR-ACD-3.2:** The plugin MUST connect to the daemon UDS/named-pipe on startup and remain connected for the lifetime of the Claude Code session.
3. **FR-ACD-3.3:** If the daemon is not running on startup, the plugin MUST retry with 250 ms × 3 exponential backoff, then complete the MCP `initialize` handshake with a synthetic fallback tool `claudebase_daemon_status` returning `{ status: "down" }` and emit a `notifications/tools/list_changed` event when the daemon later comes up.
4. **FR-ACD-3.4:** The plugin MUST forward `tools/call` requests from Claude Code → daemon over the UDS and return the response to Claude Code.
5. **FR-ACD-3.5:** The plugin MUST forward `Notification` frames received from the daemon (e.g., incoming Telegram messages) to Claude Code as `notifications/claude/channel` events.
6. **FR-ACD-3.6:** The `~/.claude/plugins/claudebase/.mcp.json` descriptor written by `daemon install` MUST declare `command: "claudebase"` and `args: ["plugin", "serve"]`. It MUST be a user-level singleton (not per-project), so every Claude Code session in every directory loads it automatically.
7. **FR-ACD-3.7:** If the daemon comes up AFTER Claude Code has started, the plugin MUST send `notifications/tools/list_changed` to Claude Code so the real tool list replaces the fallback `claudebase_daemon_status` sentinel.

#### FR-ACD-4: Chat Backend — `chat_post` / `chat_subscribe` / `chat_reply` / `chat_list` (Slice 3, plan lines 130–143)

1. **FR-ACD-4.1:** `chat_post { thread: string, content: string, from: string }` MUST persist a message row to `chat.db` and broadcast it to all connections subscribed to that thread within 10 ms.
2. **FR-ACD-4.2:** `chat_subscribe { thread: string }` MUST register the calling connection as a subscriber to the named thread. On subscribe, the tool MUST return any undelivered messages since the last `chat_list` call for that connection.
3. **FR-ACD-4.3:** `chat_reply { thread: string, content: string, reply_to: string }` MUST persist a reply row linked to `reply_to` (a message id) and broadcast it to subscribers.
4. **FR-ACD-4.4:** `chat_list { thread: string, since: string?, limit: integer? }` MUST return a paginated list of messages for the thread, ordered by insertion time, filtered by `since` timestamp if provided.
5. **FR-ACD-4.5:** Thread names for Telegram chat threads MUST follow the format `telegram:<chat_id>`.
6. **FR-ACD-4.6:** The broadcast dispatch MUST be in-process (tokio channel) — no additional process or network hop. Broadcast latency to a subscribed connection MUST be ≤ 10 ms under normal load (NFR-ACD-3).

#### FR-ACD-5: Agent Registry — `agent_register` / `agent_unregister` / `agent_list_alive` / `agent_reap` (Slice 5, plan lines 160–188)

1. **FR-ACD-5.1:** `agent_register { agent_id: string, name: string, thread: string?, metadata: JSON? }` MUST insert a row into `agent_registry` with `state = 'alive'` and `connection_id` from the calling plugin's connection.
2. **FR-ACD-5.2:** `agent_unregister { agent_id: string }` MUST update the row to `state = 'dead'`.
3. **FR-ACD-5.3:** `agent_list_alive { thread: string? }` MUST return all rows with `state = 'alive'`, optionally filtered by `chat_thread_id`. The response MUST include `agent_id`, `name`, `chat_thread_id`, `spawned_at`, and `last_pinged_at`.
4. **FR-ACD-5.4:** `agent_reap { older_than: integer }` MUST bulk-update rows where `last_pinged_at < (now - older_than seconds)` and `state = 'alive'` to `state = 'dead'`. Returns count of reaped rows.
5. **FR-ACD-5.5:** On connection EOF, the daemon MUST bulk-UPDATE all rows with `connection_id = <closed_connection_id>` and `state = 'alive'` to `state = 'orphaned'`.
6. **FR-ACD-5.6:** The `agent_registry` table MUST have an index on `(chat_thread_id, state) WHERE state = 'alive'` for efficient alive-agent lookup during routing (plan Slice 5, SQL schema block, lines 169–183).
7. **FR-ACD-5.7 (Agent-name uniqueness within thread):** When `agent_register` is called with `(chat_thread_id, agent_name, state='alive')` and a row already exists with the SAME `chat_thread_id` AND `agent_name` AND `state='alive'`, the second registration MUST fail with a clear error (e.g., `UNIQUE constraint failed: agent_registry.chat_thread_id, agent_registry.agent_name`). This ensures `@<agent_name>` mentions in a Telegram thread route deterministically. Second-session agents of the same name fall through to fresh-spawn with stitched backlog at mention time rather than registry-resolved SendMessage. Enforced via a partial unique index in §17.7. Source: plan directive 5 / F-5.1 (`.claude/plan.md` line 386).

#### FR-ACD-6: Telegram Bot Integration + Permission / Pairing Model (Slice 4, plan lines 145–158)

1. **FR-ACD-6.1:** The daemon MUST start a `tokio::spawn`'d task running a `teloxide` long-polling loop when a bot token is configured.
2. **FR-ACD-6.2:** Inbound Telegram text messages MUST be persisted to `chat.db` thread `telegram:<chat_id>` and broadcast to all subscribed connections as `notifications/claude/channel`.
3. **FR-ACD-6.3:** Inbound Telegram voice notes MUST be queued to the ASR pipeline (FR-ACD-7). The transcribed text MUST then be treated as an inbound text message per FR-ACD-6.2.
4. **FR-ACD-6.4:** The bot token MUST be stored in `~/.config/claudebase/secrets.toml` with file permissions `0600`. The daemon MUST refuse to start if the bot-token file has permissions other than `0600`.
5. **FR-ACD-6.5:** Access control MUST implement a pairing model ported from the voice-control reference repo (`ACCESS.md`, plan line 154): `dmPolicy` values `"pairing"` (default) | `"allowlist"` | `"disabled"`. On first message from an unknown user, the bot MUST reply with a pairing code via Telegram inline keyboard. The user runs `claudebase daemon access pair <code>` from the terminal to authorize. Once authorized, the user is added to the `allowFrom` list in `~/.config/claudebase/access.json`.
6. **FR-ACD-6.6:** `claudebase daemon access pair <code>` MUST accept a pairing code and add the pending Telegram user to the authorized list.
7. **FR-ACD-6.7:** `claudebase daemon access list` MUST print the current access list with user ids, Telegram usernames, and authorization dates.
8. **FR-ACD-6.8:** `claudebase daemon config edit` MUST open `~/.config/claudebase/daemon.toml` in `$EDITOR` (default `vi`).
9. **FR-ACD-6.9:** `claudebase daemon config show` MUST print the parsed effective config as JSON, masking resolved secret values (e.g., the bot token appears as `"***"`).
10. **FR-ACD-6.10 (Restart-window resume):** On daemon start, the Telegram long-poll worker MUST read `telegram.last_update_id` from `daemon_state` and resume polling from that offset. The worker MUST update this row after every successful long-poll batch (atomic INSERT OR REPLACE). This guarantees no Telegram message sent during a daemon-restart window is lost or duplicated. Verified by TC-4.16. Source: plan directive 7 / F-4.3 (`.claude/plan.md` line 388).

#### FR-ACD-7: ASR Pipeline — Three Backends Behind a Single `Asr` Trait (Slice 6, plan lines 190–238)

1. **FR-ACD-7.1:** An `Asr` trait MUST be defined with the signature `async fn transcribe(&self, pcm: Vec<f32>, sample_rate: u32) -> Result<String>` and the bounds `Send + Sync + 'static`. Audio decoding from Telegram-native formats (e.g., `.ogg`/`.oga`) to 16 kHz mono `Vec<f32>` PCM happens BEFORE the trait call, in the symphonia-based decoder. All three backends MUST implement this trait. Backend construction is mediated by a `make_asr(config: &Config) -> Box<dyn Asr>` factory that handles backend-specific state (e.g., whisper-rs holds a loaded model handle; NIM is stateless per-call). This signature supersedes the earlier `audio_bytes: Vec<u8>, format: AudioFormat` shape per architect [STRUCTURAL] #5 resolution at Step 3 of the bootstrap pipeline.
2. **FR-ACD-7.2:** The active backend MUST be selected at runtime from `[asr] backend` in `daemon.toml` without recompilation. Daemon restart is required to switch backends.
3. **FR-ACD-7.3 (Backend A — whisper, default):** The `whisper` backend MUST use the `whisper-rs` Rust binding to whisper.cpp (Cargo feature `asr-whisper`). On first voice note, if the configured model file is absent, the daemon MUST auto-download `ggml-<size>.bin` from `https://huggingface.co/ggerganov/whisper.cpp/resolve/main/` to `~/.claude/tools/claudebase/models/whisper/`. Default model: `medium`. Supported sizes: `tiny`, `base`, `small`, `medium`, `large`.
4. **FR-ACD-7.4 (Backend B — sherpa-nemo, optional):** The `sherpa-nemo` backend MUST use `sherpa-onnx` (Cargo feature `asr-sherpa`) to load ONNX-exported NeMo models. Config MUST point to user-provided `encoder_onnx`, `decoder_onnx`, and `tokens` file paths. The daemon MUST NOT bundle Python, PyTorch, or the `.nemo` native runtime. If the sherpa-rs Rust binding proves unstable during implementation, the fallback is shell-out to a system-installed `sherpa-onnx-offline` binary (plan risk 3a, line 290).
5. **FR-ACD-7.5 (Backend C — nim, optional):** The `nim` backend MUST POST audio to the NVIDIA NIM endpoint (assumed `https://integrate.api.nvidia.com/v1/audio/transcriptions`) using `reqwest` with `rustls-tls` (Cargo feature `asr-nim`). The API key MUST be read from the environment variable named by `api_key_env` in config (default `NVIDIA_API_KEY`). The key MUST NEVER be written to `daemon.toml` or any config file.
6. **FR-ACD-7.6:** Audio decoding from Telegram's Opus-in-Ogg format MUST use the `symphonia` crate (pure Rust, no ffmpeg dependency). The decoded output fed to all three backends MUST be 16 kHz mono PCM `Vec<f32>`.
7. **FR-ACD-7.7:** ASR errors (model load failure, network timeout, decode error) MUST be logged at WARN level without crashing the daemon. The chat thread MUST receive a `[ASR error: <reason>]` placeholder message so the user knows transcription failed.
8. **FR-ACD-7.8:** `claudebase daemon doctor --asr` MUST validate the configured backend: for `whisper`, verify model file exists and loads; for `sherpa-nemo`, verify ONNX files exist; for `nim`, run an HTTP probe against the endpoint. Exit 0 on success, exit 1 with a human-readable error on failure.
9. **FR-ACD-7.9:** `claudebase daemon warmup --asr` MUST pre-load the configured ASR model into memory. For the `whisper` backend, this triggers the auto-download if the model is absent.

#### FR-ACD-8: Cross-Platform Service Installer (Slice 2, plan lines 115–128)

1. **FR-ACD-8.1:** On Linux, `daemon install` MUST generate a systemd user unit at `~/.config/systemd/user/claudebase.service`. The unit MUST include: `ProtectSystem=strict`, `ProtectHome=read-only`, `ReadWritePaths=%h/.claude %h/.config/claudebase`, `NoNewPrivileges=true`, `PrivateTmp=true`. `User=root` MUST NOT appear.
2. **FR-ACD-8.2:** On macOS, `daemon install` MUST generate a launchd plist at `~/Library/LaunchAgents/dev.codefather.claudebase.plist` using SIP-aware sandboxing with equivalent access restrictions.
3. **FR-ACD-8.3:** On Windows, `daemon install` MUST register a Windows Service running as the current user (NOT `LocalSystem`).
4. **FR-ACD-8.4:** `install.sh` and `install.ps1` MUST support an opt-in `CLAUDEBASE_INSTALL_DAEMON=1` environment variable that calls `claudebase daemon install --no-start` as a post-install step. Without this variable, the daemon is NOT installed as a system service during `bash install.sh --yes`.
5. **FR-ACD-8.5:** The `.mcp.json` descriptor written by `daemon install` MUST declare `command: "claudebase"` and `args: ["plugin", "serve"]` at the user-level plugin path `~/.claude/plugins/claudebase/.mcp.json`.

#### FR-ACD-9: Single-Instance Enforcement (Slice 4, plan line 154)

1. **FR-ACD-9.1:** `claudebase daemon serve` MUST acquire an exclusive `fslock` on `$XDG_RUNTIME_DIR/claudebase/daemon.pid` before starting any subsystem.
2. **FR-ACD-9.2:** A second concurrent `claudebase daemon serve` invocation MUST fail immediately with exit 1 and the message `claudebase daemon: already running (pid N)` before starting any subsystem or binding the UDS socket.
3. **FR-ACD-9.3:** On daemon shutdown (SIGTERM/SIGINT or Windows service stop event), the PID file MUST be removed and the UDS socket file MUST be unlinked.

#### FR-ACD-10: Daemon-Down Graceful Degradation (Slice 1, plan lines 109–110)

1. **FR-ACD-10.1:** When the plugin bridge fails to connect to the daemon after the 3-retry backoff, it MUST still complete the MCP `initialize` handshake successfully and expose exactly one tool: `claudebase_daemon_status` with schema `{}` (no parameters) and return `{ status: "down", message: "claudebase daemon is not running — start it with 'claudebase daemon start'" }`.
2. **FR-ACD-10.2:** The plugin MUST NOT crash or exit when the daemon is down. Claude Code MUST be able to call `claudebase_daemon_status` and receive the above response at any time.
3. **FR-ACD-10.3:** If the daemon starts while the plugin is running, the plugin MUST automatically re-connect and send `notifications/tools/list_changed` to replace the sentinel tool with the real tool list.

#### FR-ACD-11: Subagent Routing via `target_agent_id` (Slice 7, plan lines 241–253)

1. **FR-ACD-11.1:** When a Telegram message contains a `@<agent-name>` mention, the daemon MUST look up an alive agent with a matching name in `agent_registry` and set a `target_agent_id` field in the `notifications/claude/channel` payload.
2. **FR-ACD-11.2:** The plugin MUST forward the `target_agent_id` field in the `notifications/claude/channel` notification to Claude Code without modification.
3. **FR-ACD-11.3:** The SDLC orchestrator (Mira, via persona update in `src/claude.md`) MUST call `SendMessage(to=target_agent_id, content=...)` when it receives a `notifications/claude/channel` event with `target_agent_id` set. If the targeted agent is no longer alive, Mira MUST fresh-spawn the agent of that name with backlog from `chat_list` as onboarding context.
4. **FR-ACD-11.4:** Agent name parsing from Telegram mentions MUST be case-insensitive. `@Reflection` and `@reflection` MUST both route to the `reflection` agent.

#### FR-ACD-12: Configuration Schema (Slice 4, plan lines 210–232)

1. **FR-ACD-12.1:** The daemon config MUST be stored at `~/.config/claudebase/daemon.toml`. The TOML schema MUST include the following top-level tables: `[telegram]`, `[asr]`, `[asr.whisper]`, `[asr.sherpa-nemo]`, `[asr.nim]`, `[daemon]`.
2. **FR-ACD-12.2:** `[telegram]` MUST reference the bot-token via a pointer to `secrets.toml` — not inline. The config file itself MUST NOT contain the bot token.
3. **FR-ACD-12.3:** `[asr] backend` MUST accept `"whisper"` (default), `"sherpa-nemo"`, or `"nim"`. An unknown value MUST cause daemon startup to fail with a clear error.
4. **FR-ACD-12.4:** All file paths in config that begin with `~` MUST be expanded to the user's home directory at load time.

#### FR-ACD-13: CLI Chat Introspection Subcommands

1. **FR-ACD-13.1:** `claudebase chat list --thread <id> [--since <ts>] [--limit N]` MUST query `chat.db` directly (no daemon connection required) and print messages in chronological order.
2. **FR-ACD-13.2:** `claudebase chat threads` MUST list all thread ids with message counts and last-message timestamps.

### 17.4 Non-Functional Requirements

1. **NFR-ACD-1 (Text message latency):** A Telegram text message sent to the bot MUST appear in the connected Claude Code session's input stream (via `notifications/claude/channel`) within 1 second on a residential broadband connection. Source: plan acceptance criterion 5, line 71.
2. **NFR-ACD-2 (Voice transcription latency):** A 10-second voice note MUST yield a transcript in the chat thread within 30 seconds using any configured backend on the reference machine (M1/M2 MacBook). Source: plan acceptance criterion 6, line 72.
3. **NFR-ACD-3 (Broadcast latency):** The in-process broadcast from `chat_post` to a subscribed connection MUST complete in ≤ 10 ms. Source: plan Slice 3 predicted outcome, line 143.
4. **NFR-ACD-4 (Single-binary install):** The `claudebase` binary MUST NOT require Python, Node.js, or any managed-language runtime to be installed on the host. All daemon subsystems (bot, ASR, IPC server, chat backend) MUST be compiled into the same binary via Cargo features. Source: plan §"Why", line 22.
5. **NFR-ACD-5 (Binary size — whisper feature):** With `--features asr-whisper`, the binary size delta MUST be ≤ 8 MB. With `--features asr-sherpa`, ≤ 12 MB. With `--features asr-nim`, ≤ 2 MB. Model files are stored separately (not bundled). Source: plan Slice 6 predicted outcome, line 238.
6. **NFR-ACD-6 (Concurrent sessions):** Two concurrent Claude Code sessions on the same machine MUST both receive Telegram incoming message broadcasts from one daemon instance. Source: plan acceptance criterion 8, line 74.
7. **NFR-ACD-7 (Bot-token security):** The bot-token file `~/.config/claudebase/secrets.toml` MUST have permissions `0600`. The daemon startup MUST fail if this file has any group or world-readable bit set. `NVIDIA_API_KEY` MUST be read from the environment and MUST NEVER be written to any config file. Source: plan Slice 4 changes, line 154; plan risk 7, line 298.
8. **NFR-ACD-8 (Service unit hardening):** The systemd unit MUST include `ProtectSystem=strict`, `NoNewPrivileges=true`, `PrivateTmp=true`. On Windows, the service MUST NOT run as `LocalSystem`. Source: plan risk 9, lines 302–303.
9. **NFR-ACD-9 (Cross-platform support):** The daemon subsystem (excluding optional whisper/sherpa features) MUST compile and pass integration tests on macOS arm64, macOS x64, Linux x64, Linux arm64, and Windows x64. Source: plan files affected, line 270.
10. **NFR-ACD-10 (Daemon resilience):** The daemon MUST not crash on ASR errors, network timeouts (Telegram long-poll), or malformed IPC frames. All per-connection errors MUST be isolated and logged; the daemon process MUST remain alive. Source: plan Slice 6 done-when condition, line 237.
11. **NFR-ACD-11 (Whisper model storage):** The whisper model (`ggml-medium.bin` at ~120 MB) MUST be stored under `~/.claude/tools/claudebase/models/whisper/`. The model path MUST be configurable. Model auto-download MUST be interruptible and resumable. Source: plan risk 8, lines 300–301.
12. **NFR-ACD-12 (Per-user isolation):** The daemon is per-OS-user (per HOME directory). Multi-user machines where two users run `claudebase daemon serve` simultaneously MUST have fully isolated UDS sockets, PID files, and SQLite databases. Cross-user daemon sharing is out of scope. Source: plan risk 5, line 294.

### 17.5 Acceptance Criteria

1. **AC-ACD-1 (Install idempotency):** `claudebase daemon install --yes` on a fresh box writes the per-OS service unit and `~/.claude/plugins/claudebase/.mcp.json`. Running it a second time exits 0 with a message indicating no changes. Source: plan AC 1, line 66.
2. **AC-ACD-2 (Boot persistence):** After `daemon install` and a reboot, `claudebase daemon status --json` returns `{ "state": "running", "pid": <N>, "uptime": <N>, "socket_path": "<path>", "subscriber_count": 0, "tg_bot_state": "connected" | "disconnected", "asr_backend": "<name>" }`. Source: plan AC 2, line 67.
3. **AC-ACD-3 (Plugin auto-load):** Starting Claude Code in any directory results in the claudebase plugin auto-loading. Mira's tool list includes `chat_post`, `chat_subscribe`, `chat_reply`, `chat_list`, `agent_register`, `agent_unregister`, `agent_list_alive`, `agent_reap`. Source: plan AC 3, line 68.
4. **AC-ACD-4 (Daemon outlives Claude Code):** Stopping Claude Code does NOT stop the daemon. Telegram messages sent during the Claude Code gap are persisted to `chat.db`. The next Claude Code session sees them as backlog via `chat_subscribe`. Verified by: `sqlite3 ~/.claude/knowledge/chat.db "SELECT count(*) FROM chat_messages WHERE thread='telegram:<id>'"` — count increments during the gap. Source: plan AC 4, line 69.
5. **AC-ACD-5 (Text message delivery < 1s):** Sending a text message to the Telegram bot while a Claude Code session is connected results in a `notifications/claude/channel` event in the plugin within 1 second. Verified by daemon trace log timestamp delta. Source: plan AC 5, line 71.
6. **AC-ACD-6 (Voice transcription < 30s):** Sending a 10-second voice note to the bot produces a transcript in the connected Claude Code session within 30 seconds using the configured ASR backend. Verified by log timestamp delta between `voice note received` and `transcript posted`. Source: plan AC 6, line 72.
7. **AC-ACD-7 (Daemon-down graceful degradation):** Killing the daemon mid-session: `claudebase_daemon_status` tool returns `{ status: "down" }`. Claude Code does not crash. Restarting the daemon causes the plugin to reconnect automatically. Source: plan AC 7, line 73.
8. **AC-ACD-8 (Two concurrent sessions):** Two concurrent Claude Code sessions on the same machine both receive a Telegram incoming message broadcast. Verified by: start two sessions, send one TG message, observe trace logs for both sessions showing the notification. Source: plan AC 8, line 74.
9. **AC-ACD-9 (Backend switch):** `claudebase daemon config edit` opens `daemon.toml` in `$EDITOR`. Switching `[asr] backend = "whisper"` → `"parakeet"` (or `"nim"`) and running `daemon restart` routes subsequent voice notes through the new backend. `daemon status --json` reflects `asr_backend: "parakeet"` (or `"nim"`) after restart. Source: plan AC 9, line 75.
10. **AC-ACD-10 (Subagent routing end-to-end):** Mira spawns a subagent, registers it via `agent_register`. User sends `@<agent-name> <message>` in Telegram. Daemon delivers the message to Mira with `target_agent_id` set. Mira calls `SendMessage(agent_id, ...)`. The subagent receives and responds. The response appears in Telegram. Verified by daemon and SDLC trace logs showing the full chain. Source: plan AC 10, line 76.
11. **AC-ACD-11 (Service unit hardening):** `cat ~/.config/systemd/user/claudebase.service` (Linux) contains `ProtectSystem=strict`, `NoNewPrivileges=true`, `PrivateTmp=true`, and does NOT contain `User=root`. Source: plan risk 9, line 302.
12. **AC-ACD-12 (Single-instance enforcement):** Running `claudebase daemon serve` twice concurrently: the second invocation exits 1 with `claudebase daemon: already running (pid N)` within 1 second. Source: plan Slice 4, line 154.
13. **AC-ACD-13 (Bot-token permission enforcement):** Setting `chmod 0644 ~/.config/claudebase/secrets.toml` then running `claudebase daemon serve`: daemon refuses to start with the message `secrets.toml must have permissions 0600`. Source: FR-ACD-6.4; plan Slice 4, line 154.
14. **AC-ACD-14 (CLI introspection without daemon):** `claudebase chat list --thread telegram:12345` queries `chat.db` directly and returns messages without requiring the daemon to be running. Source: FR-ACD-13.1.
15. **AC-ACD-15 (Whisper auto-download):** On a machine with no whisper model files, sending the first voice note triggers model download to `~/.claude/tools/claudebase/models/whisper/ggml-medium.bin` before transcription begins. `claudebase daemon status --json` shows `asr_backend: "whisper"` throughout. Source: plan risk 8, line 300; FR-ACD-7.3.

### 17.6 Affected Endpoints

N/A. `claudebase` has no HTTP API today. The daemon UDS (`$XDG_RUNTIME_DIR/claudebase/daemon.sock`) and named pipe (`\\.\pipe\claudebase-daemon`) are internal IPC transports, not public HTTP endpoints. The MCP tool surface (exposed to Claude Code via the plugin bridge) is documented in §17.3 FR-ACD-4 (chat tools) and FR-ACD-5 (registry tools). The Telegram long-poll loop connects outbound to Telegram servers — no inbound HTTP port is opened.

### 17.7 Schema Changes

Two new SQLite files are introduced alongside the existing `index.db` and `insights.db`:

#### `chat.db` — Schema v5 (Slice 3, plan lines 130–143)

Introduced in schema migration v5 in `src/migrations.rs`:

```sql
-- chat.db: user-level singleton at ~/.claude/knowledge/chat.db
-- (NOT per-project — the daemon is per-OS-user; chat threads cross project boundaries
-- e.g. one Telegram thread spans whatever project Claude Code is currently in)
-- index.db and insights.db remain per-project at <project>/.claude/knowledge/
-- This pinning resolves OQ-ACD-4 per architect [STRUCTURAL] #1 at bootstrap Step 3.
-- A `user_level_chat_db_path()` helper in src/store.rs bypasses resolve_project_root().
CREATE TABLE IF NOT EXISTS chat_threads (
    id           TEXT PRIMARY KEY,
    created_at   INTEGER NOT NULL DEFAULT (unixepoch()),
    metadata     JSON
);

CREATE TABLE IF NOT EXISTS chat_messages (
    id           TEXT PRIMARY KEY,   -- UUID v4
    thread_id    TEXT NOT NULL REFERENCES chat_threads(id),
    from_agent   TEXT NOT NULL,
    content      TEXT NOT NULL,
    reply_to     TEXT REFERENCES chat_messages(id),
    created_at   INTEGER NOT NULL DEFAULT (unixepoch()),
    delivered_at INTEGER
);

CREATE INDEX chat_messages_thread_time_idx
    ON chat_messages(thread_id, created_at);

-- daemon_state: small KV store for daemon-process metadata that must
-- survive restart. Specifically used by Slice 4 to checkpoint the last
-- Telegram update_id processed, so on daemon restart teloxide's long-poll
-- resumes from the persisted offset without losing or duplicating messages
-- delivered during the restart window. Reserved for future scalar daemon
-- state; NOT a general-purpose KV (use chat.db or a future settings table
-- for user-configurable values).
CREATE TABLE IF NOT EXISTS daemon_state (
    key        TEXT PRIMARY KEY,
    value      TEXT NOT NULL,
    updated_at INTEGER NOT NULL DEFAULT (unixepoch())
);
-- Slice 4 writes: INSERT OR REPLACE INTO daemon_state(key, value)
--                 VALUES ('telegram.last_update_id', <id>)
-- Slice 4 reads at boot: SELECT value FROM daemon_state
--                        WHERE key = 'telegram.last_update_id'
```

#### `agent_registry` table — Schema v6 (Slice 5, plan lines 160–188)

Added to `chat.db` via schema migration v6:

```sql
CREATE TABLE IF NOT EXISTS agent_registry (
    agent_id           TEXT PRIMARY KEY,
    agent_name         TEXT NOT NULL,
    connection_id      TEXT NOT NULL,
    chat_thread_id     TEXT,
    permission_relayer TEXT,
    spawned_at         INTEGER NOT NULL,
    last_pinged_at     INTEGER NOT NULL,
    state              TEXT NOT NULL
                           CHECK (state IN ('alive', 'orphaned', 'dead')),
    metadata           JSON
);

CREATE INDEX agent_registry_thread_alive_idx
    ON agent_registry(chat_thread_id, state)
    WHERE state = 'alive';

-- Per FR-ACD-5.7 (agent-name uniqueness within thread).
-- Partial unique index allows N>1 dead/orphaned rows for the same
-- (chat_thread_id, agent_name) pair while enforcing at-most-one alive
-- row at any moment. Required by Slice 5 + verified by TC-5.9.
CREATE UNIQUE INDEX IF NOT EXISTS agent_registry_thread_name_alive_idx
    ON agent_registry(chat_thread_id, agent_name)
    WHERE state = 'alive' AND chat_thread_id IS NOT NULL;
```

Both migrations are registered via `src/migrations.rs` following the existing `apply_v2`, `apply_v3`, `apply_v4` pattern. Both are additive — they do NOT modify `index.db` or `insights.db`.

### 17.8 UI Changes

#### Telegram Bot UI

The Telegram bot presents the following user-facing surfaces:

1. **Permission pairing flow** — when an unknown user sends `/start`, the bot replies with an inline keyboard containing a pairing code. The user runs `claudebase daemon access pair <code>` in their terminal. On success, the bot confirms via a Telegram message. This flow is ported from the voice-control reference repo's `ACCESS.md` (plan Slice 4, line 154).
2. **ASR error notification** — when voice transcription fails, the bot sends a Telegram message `[ASR error: <reason>]` to the user in the same chat.
3. **Daemon-down notification** — if the plugin detects the daemon is down during a Claude Code session, the `claudebase_daemon_status` sentinel tool response surfaced to Mira MUST include a human-readable message with recovery instructions.

#### CLI Surface (New Subcommand Trees)

Two new top-level subcommand trees are added to the `claudebase` binary:

**`claudebase daemon <subcommand>`** (Slice 1–4):
- `serve` — long-running daemon process
- `install [--yes] [--no-start]` — write service unit + .mcp.json
- `uninstall [--keep-data]` — remove service unit
- `start`, `stop`, `restart` — OS service control
- `status [--json]` — daemon health
- `logs [--lines N] [--follow]` — log streaming
- `config edit`, `config show [--json]` — TOML config management
- `access pair <code>`, `access list` — permission management
- `doctor [--asr]` — validation and health check
- `warmup [--asr]` — model pre-loading

**`claudebase plugin <subcommand>`** (Slice 1):
- `serve` — long-running MCP plugin bridge process (launched by Claude Code via `.mcp.json`)

**`claudebase chat <subcommand>`** (Slice 3):
- `list --thread <id> [--since <ts>] [--limit N]` — direct DB query
- `threads` — list all thread ids

All new subcommands follow the existing `claudebase` CLI conventions: `--json` flag for machine-readable output, `--project-root` for path override, exit codes 0/1/2 per the established contract.

### 17.9 Risks and Dependencies

1. **Risk 1 — whisper-rs build dependencies (CMake + clang) on all platforms.** whisper.cpp requires native build tooling. Mitigation: CI matrix step per platform; documented in install troubleshooting (`brew install cmake llvm` / `apt install cmake clang` / `choco install cmake llvm`). Source: plan risk 1, line 284.
2. **Risk 2 — `claude/channel` and `claude/channel/permission` notification methods are Anthropic-internal spec** inlined from the voice-control reference repo. If the spec drifts, the plugin bridge lags. Mitigation: pin the wire format shipped; `daemon doctor` validates against the live Claude Code MCP handshake monthly. Source: plan risk 2, line 286.
3. **Risk 3 — NVIDIA NIM Parakeet API surface unverified.** The `https://integrate.api.nvidia.com/v1/audio/transcriptions` endpoint could not be verified during planning (docs 404'd). Implementation begins with an HTTP probe; if the shape differs (e.g., gRPC-only), the `nim` backend implementation pivots. The `whisper` backend is unaffected. Source: plan risk 3, line 288.
4. **Risk 3a — NeMo model format (`.nemo`) incompatible with single-binary constraint.** Native `.nemo` requires Python+NeMo+PyTorch+CUDA. The `sherpa-nemo` backend MUST use ONNX-exported variants via `sherpa-onnx`. If sherpa-rs Rust binding proves unstable, fallback is shell-out to system-installed `sherpa-onnx-offline`. Bundling Python is a hard disqualifier, not a trade-off. Source: plan risk 3a, line 290.
5. **Risk 4 — Phased deliverable exit after Slice 4.** If after Slice 4 (Telegram working end-to-end) the UX is less compelling than expected, Slices 5–7 can be deferred without losing Slice 1–4 value. Source: plan risk 4, line 292.
6. **Risk 5 — Multi-user shared machine.** Out of scope. Daemon is per-OS-user. Source: plan risk 5, line 294.
7. **Risk 6 — Plugin lifecycle on Claude Code restart.** When Claude Code is killed and restarted, the plugin process also restarts with a new `connection_id`. State tied to the prior `connection_id` is orphaned per FR-ACD-5.5. Verified by AC-ACD-4 and AC-ACD-10. Source: plan risk 6, line 296.
8. **Risk 7 — `NVIDIA_API_KEY` in config file.** The key is NEVER stored in config — only the env var name. `daemon config show` masks resolved values. Source: plan risk 7, line 298.
9. **Risk 8 — Whisper model storage (~120 MB).** Auto-downloaded on first voice note to `~/.claude/tools/claudebase/models/whisper/`. Configurable model size. Pre-warmed via `claudebase daemon warmup`. Source: plan risk 8, line 300.
10. **Risk 9 — Service unit security.** systemd unit MUST have hardening flags (FR-ACD-8.1). Audited by `security-auditor` in Slice 2. Source: plan risk 9, lines 302–303.
11. **Risk 10 — `tools/list_changed` MCP notification when daemon comes up late.** If the daemon starts after Claude Code, the plugin must proactively send `notifications/tools/list_changed`. This is the documented MCP capability flow (FR-ACD-3.7). Source: plan risk 10, line 304.

## Facts

### Verified facts

- Plan at `.claude/plan.md` read in full this session (374 lines). Architecture choice: Option B (Persistent Daemon + Thin STDIO Plugin Bridge over UDS/Named Pipe). Option A (HTTP transport) and Option C (3-mode pluggable) evaluated and rejected. Source: plan lines 28–58. Salience: high.
- 7 implementation slices, 6 waves (Wave 1: Slice 1 solo; Wave 2: Slices 2+3 parallel; Wave 3: Slice 4; Wave 4: Slice 5; Wave 5: Slice 6; Wave 6: Slice 7). Source: plan `Wave:` fields per slice, lines 98–253. Salience: high.
- 10 acceptance criteria verbatim in plan lines 64–76. All 10 mapped to AC-ACD-1 through AC-ACD-10. Salience: high.
- Schema v5 introduces `chat_threads` + `chat_messages` tables (Slice 3, plan line 135). Schema v6 introduces `agent_registry` (Slice 5, plan lines 168–184). Both go into `chat.db` sibling file, NOT into `index.db`. Source: plan Slice 3 files + Slice 5 files. Salience: high.
- `agent_registry` SQL schema verbatim in plan lines 169–183: `state TEXT NOT NULL CHECK (state IN ('alive','orphaned','dead'))`, index on `(chat_thread_id, state) WHERE state = 'alive'`. Salience: high.
- Single-binary constraint (no Python, no Node.js) stated explicitly in plan §"Why" line 22 and reinforced in risk 3a (line 290): "NEVER bundle Python — that's a hard constraint, not a preference." Salience: high.
- Whisper auto-download target: `https://huggingface.co/ggerganov/whisper.cpp/resolve/main/ggml-<size>.bin` to `~/.claude/tools/claudebase/models/whisper/`. Source: plan Slice 6, plan lines 204–205. Salience: medium.
- NVIDIA NIM endpoint (assumed, not verified during planning): `https://integrate.api.nvidia.com/v1/audio/transcriptions`. Source: plan risk 3, line 288. Salience: high (unverified — see External contracts).
- Bot-token storage: `~/.config/claudebase/secrets.toml` at chmod 0600. Source: plan Slice 4 changes, line 154. Salience: high.
- fslock PID-file single-instance enforcement. Source: plan Slice 4, line 154. Salience: medium.
- Broadcast latency target ≤ 10 ms. Source: plan Slice 3 predicted outcome, line 143. Salience: medium.
- Claudebase project knowledge base: index.db exists but doc_count = 0 (empty corpus). Verified via `claudebase status --json` returning `{"schema_version":4,"doc_count":0,"chunk_count":0}`. Corpus scope verdict: **No overlap** — empty corpus has no indexed domain content. Topical queries skipped per corpus-scope-relevance protocol. Salience: low.
- insights.db exists at `<project>/.claude/knowledge/insights.db`. Queried `claudebase insight search "daemon telegram MCP ASR whisper"` — 0 hits (empty corpus, no prior sessions on this feature). Salience: low.
- Existing `docs/PRD.md` ends at line 404 with §16 `## Decisions → Symptom-only patches` block. §17 is appended after. Source: Read tool output this session. Salience: medium.
- §16 used `chat.db` sibling-file pattern reference in plan as the schema pattern precedent. Source: plan Slice 3 line 135. Salience: medium.

### External contracts

- **`teloxide` Rust crate (Telegram bot framework)** — symbol: `teloxide::dispatching::Dispatcher`, long-poll update loop, `bot.send_message(chat_id, text)` — source: crates.io `teloxide` (version not pinned in plan) — verified: **no — assumption**. Risk: API surface may differ from TypeScript voice-control source. How to verify: Slice 4 implementation begins with `teloxide` API exploration; architect pre-review in Slice 4. Salience: high.
- **`whisper-rs` Rust crate (whisper.cpp binding)** — symbol: `WhisperContext::new(model_path)`, `WhisperContext::create_state()`, `state.full(params, audio_pcm)`, `state.full_n_segments()`, `state.full_get_segment_text(i)` — source: crates.io `whisper-rs` (community binding to ggerganov/whisper.cpp) — verified: **no — assumption**. Risk: API shape may differ; CMake+clang build deps may fail on CI. How to verify: Slice 6 pre-review by architect. Salience: high.
- **`sherpa-rs` / `sherpa-onnx-sys` Rust crate (ONNX runtime for NeMo)** — symbol: `sherpa_rs::OfflineRecognizer::new(config)`, `recognizer.decode(audio)` (assumed) — source: crates.io `sherpa-rs` (community crate, quality uncertain per plan) — verified: **no — assumption**. Risk: crate may be unmaintained or API unstable. Fallback: shell-out to system `sherpa-onnx-offline` binary per plan risk 3a. Salience: high.
- **NVIDIA NIM REST API** — symbol: `POST /v1/audio/transcriptions`, `Authorization: Bearer $NVIDIA_API_KEY`, multipart form body with `file` and `model` fields (assumed OpenAI-compatible shape) — source: assumed `https://integrate.api.nvidia.com/v1/...` — verified: **no — assumption (CRITICAL)**. NIM docs 404'd during planning per plan risk 3. Implementation MUST begin with HTTP probe; if gRPC-only, backend pivots. Salience: high.
- **MCP protocol JSON-RPC 2.0** — symbols: `initialize` request/response, `tools/list` request/response, `tools/call` request/response, `notifications/claude/channel` notification method — source: `https://modelcontextprotocol.io/specification` (assumed) — verified: **no — assumption**. Salience: high.
- **`claude/channel` and `claude/channel/permission` notification methods** — symbol: `notifications/claude/channel` with fields `source`, `content`, `meta.target_agent_id`; `notifications/claude/channel/permission` permission-relayer state machine — source: `anthropics/claude-cli-internal` (private, inlined from voice-control repo) — verified: **no — assumption**. Risk: private spec may drift. Mitigation: pin wire format; `daemon doctor` validates monthly. Salience: high.
- **whisper.cpp ggml model format** — symbol: `ggml-tiny.bin`, `ggml-base.bin`, `ggml-small.bin`, `ggml-medium.bin`, `ggml-large.bin` from `https://huggingface.co/ggerganov/whisper.cpp/resolve/main/` — source: HuggingFace repo `ggerganov/whisper.cpp` (assumed URL structure) — verified: **no — assumption**. Risk: HuggingFace URL format may change; model names may be versioned. Salience: medium.
- **NVIDIA Parakeet / Nemotron ONNX export via sherpa-onnx** — symbol: user pre-exports `.nemo` → ONNX using scripts at `https://k2-fsa.github.io/sherpa/onnx/nemo/index.html`; community-converted ONNX at `https://huggingface.co/csukuangfj/` — source: plan Slice 6, lines 206–207 — verified: **yes — confirmed during planning** (`.nemo` format requires ONNX export; NeMo framework incompatible with single-binary; sherpa-onnx export path exists). Salience: medium.
- **`symphonia` Rust crate (audio decoder)** — symbol: `symphonia::core::io::MediaSourceStream`, Opus/Ogg container demux, 16 kHz resampling — source: crates.io `symphonia` (pure Rust, well-maintained) — verified: **no — assumption**. Risk: Telegram's voice note format (Opus-in-Ogg) may require specific container support. How to verify: Slice 6 includes a fixture `.oga` file transcoding test. Salience: medium.
- **`fslock` Rust crate (PID-file locking)** — symbol: `LockFile::open_and_lock(path)` — source: crates.io `fslock` — verified: **no — assumption**. Salience: low.
- **`interprocess` Rust crate (UDS + named-pipe)** — symbol: `LocalSocketListener::bind(path)`, `LocalSocketStream::connect(path)` — source: crates.io `interprocess` — verified: **no — assumption**. Salience: high.

### Assumptions

- The SDLC orchestrator Mira will be updated (Slice 7, `src/claude.md` change, plan line 248) to handle `notifications/claude/channel` events with `target_agent_id` and call `SendMessage`. This update is in the SDLC repo, not the claudebase repo. Risk: the SDLC update may lag behind the claudebase implementation, causing AC-ACD-10 to fail. How to verify: Slice 7 done-when condition requires end-to-end trace log verification. Salience: high.
- Two concurrent Claude Code sessions on the same machine can both connect to the same daemon UDS socket. Risk: `interprocess` crate's concurrent-accept behavior on macOS/Linux/Windows may have edge cases. How to verify: AC-ACD-8 explicit multi-session test. Salience: high.
- Telegram long-poll rate limits (teloxide built-in backoff) are sufficient to avoid API bans under normal single-user operation. Risk: heavy voice-note usage could trigger Telegram server-side throttling. How to verify: note in QA test cases; monitor in production. Salience: medium.
- The `daemon.toml` TOML parsing via the `toml` Rust crate handles the `[asr.sherpa-nemo]` table name with a hyphen without special quoting. Risk: TOML parsers may require quoted table names for hyphenated keys. How to verify: Slice 4 config-loading unit test with a hyphenated table name. Salience: medium.
- The voice-control reference repo's pairing model (`ACCESS.md`, plan line 154) is transferable to Rust/teloxide with the same user-visible flow. Risk: Telegram bot callback_query IDs and inline keyboard interactions may differ between Bun/TypeScript and Rust/teloxide. How to verify: Slice 4 Telegram integration test. Salience: medium.
- whisper.cpp model auto-download from HuggingFace does not require authentication for the ggml-*.bin files. Risk: HuggingFace may gate the download behind an account. How to verify: AC-ACD-15 tests the download path on a clean machine. Salience: low.

### Open questions

- **OQ-ACD-1 (NVIDIA NIM endpoint shape)** — Needs: external research / HTTP probe at Slice 6 implementation start. If gRPC-only, `nim` backend must use tonic rather than reqwest. Salience: high.
- **OQ-ACD-2 (sherpa-rs Rust binding stability)** — Needs: architect review at Slice 6 pre-review. If sherpa-rs is too community-quality, decide between: (a) shell-out to `sherpa-onnx-offline`, (b) defer `sherpa-nemo` backend, (c) accept instability with integration-test guard. Salience: high.
- **OQ-ACD-3 (Telegram `claude/channel/permission` spec fidelity)** — The permission-relayer state machine is inlined from a private Anthropic repo. Needs: daemon doctor verification at Slice 2 security-auditor review + Slice 7 pre-review. Salience: medium.
- **OQ-ACD-4 (chat.db file location)** — Plan says `<project>/.claude/knowledge/chat.db`. For the daemon (a user-level service, not project-scoped), this location is ambiguous — should `chat.db` be at `~/.claude/knowledge/chat.db` (user-level) or at the project level? Needs: architect decision at Slice 3 pre-review. Salience: high.

## Decisions

### Inbound validation

- Task: append §17 PRD section to `docs/PRD.md` based on the approved plan at `.claude/plan.md`. The plan was read in full (374 lines). No contradictions detected between plan and task description. The plan references `docs/PRD.md` as `[new]` (plan line 81) but it already exists with §15 and §16 — the APPEND instruction in the task description correctly resolves this. Challenged: yes — noted the `[new]` vs. existing file discrepancy. Outcome: proceeded with APPEND as instructed. Salience: medium.

### Decisions made

- Map 10 plan acceptance criteria (plan lines 64–76) directly to AC-ACD-1 through AC-ACD-10, then add 5 additional AC items (AC-ACD-11 through AC-ACD-15) derived from FR gaps (service hardening, single-instance enforcement, token permission enforcement, direct CLI query, whisper auto-download). Q1-Q5: hack? no. Sane? yes. Alternatives: use plan ACs verbatim only — rejected because FR-ACD coverage requires additional testable criteria. Symptom-or-cause? cause. Salience: medium.
- OQ-ACD-4 (chat.db location) is surfaced as an open question rather than assuming project-level placement. The plan states `<project>/.claude/knowledge/chat.db` but a user-level daemon arguably needs user-level storage. Decision delegated to architect Slice 3 pre-review. Q1-Q5: hack? no. Sane? yes. Alternative: assume user-level `~/.claude/knowledge/chat.db` — rejected because changing the plan's stated location without architect input is out of prd-writer scope. Salience: high.
- External contracts for `teloxide`, `whisper-rs`, `sherpa-rs`, MCP protocol, NIM API, `claude/channel` notification methods, and `interprocess` all marked `verified: no — assumption` because none were directly inspected this session. This is conservative and correct per Protocol 1. Salience: high.

### Hacks acknowledged

(none) — PRD authoring only; no implementation hacks introduced.

### Symptom-only patches (with root-cause links)

(none) — no symptom-only patches in this PRD section.
