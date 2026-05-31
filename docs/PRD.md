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
5. **FR-ACD-6.5:** Access control MUST implement a pairing model ported from the voice-control reference repo (`ACCESS.md`, plan line 154): `dmPolicy` values `"pairing"` (default) | `"allowlist"` | `"disabled"`. On first message from an unknown user, the bot MUST reply with a pairing code via Telegram inline keyboard. The user runs `claudebase daemon access pair <code>` from the terminal to authorize. Once authorized, the user is added to the `allowFrom` list in `~/.claude/channels/claudebase/access.json`. **(Amended 2026-05-31 — access-json-path-reconciliation:** the canonical access file is `~/.claude/channels/claudebase/access.json` (returned by `channel_state::access_json_path()`), NOT the previously-documented `~/.config/claudebase/access.json`. The daemon's inbound gating, the pairing/`approved/` subsystem, and the `daemon access pair` CLI all operate on this single canonical file; `allowFrom` ids are stored as JSON strings. A one-shot boot-time migration carries any legacy `~/.config/claudebase/access.json` grants into the canonical file. Architect verdict: canonical = the channel-state file because the entire channel subsystem already lives under `~/.claude/channels/claudebase/`, matching the official-plugin convention.)
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

---

## §18. Insights Hybrid Corpus — Global General DB, Project Registry, Mandatory Tags, and Read-on-New-Context Hook

**Status:** [PLANNED]
**Date:** 2026-05-27
**Priority:** High
**Related:** §16 (Agent Insights Base — this section extends the `insight` subcommand tree and `insights.db` schema introduced in §16; schema v5 of `insights.db` is additive on top of §16's v4). §17 (Agent Chat Daemon — §17's `chat.db` uses an independent schema versioning scheme; no conflict). §15 (Vector + Multimodal Retrieval Backend — the `--corpus all` RRF fusion machinery at `main.rs:2548-2604` is reused for the local+general insight merge in FR-IHC-5). Plan source: `.claude/plan.md` (199 lines, read in full this session).

Changelog: Agents can now share knowledge across projects — general lessons go to a global pool every project can read, while project-specific insights stay local. Tags make it fast to pull only what's relevant when a new session starts.

### 18.1 Feature Description

The Agent Insights Base (§16) introduced per-project cognitive memory: `insights.db` at `<project>/.claude/knowledge/insights.db` lets SDLC agents write and read cross-session insights. Three structural gaps remain after §16:

1. **No global collection point.** Insights about general tool-level or domain-level lessons (nginx reload signals, Tokio mutex gotchas, cognitive-bias patterns) are trapped in whichever project happened to discover them, invisible to every other project.
2. **No selective read surface.** Agents filter by `feature_slug` / `agent` / `salience`. There is no topic-tag axis — an agent entering a fresh context either floods its window with all insights or reads none.
3. **No scope discipline.** An agent working on project X has no mechanism to say "give me X's insights plus general ones, but not project Y's."

This feature delivers a **hybrid corpus**:

- **Project insights** stay in their project's LOCAL db (`<project>/.claude/knowledge/insights.db`). Existing v4 data is untouched; zero content migration.
- **General insights** (cross-project, tool-level knowledge) live in ONE GLOBAL db at `~/.claude/knowledge/insights.db`.
- A **project registry** at `~/.claude/knowledge/projects.json` is upserted at every `claudebase run` startup, mapping `project-name → path` so routing resolves the right db.
- Every `insight create` call requires a **mandatory `--category` (`general` | `project`)** — the routing key — and at least one **mandatory free-form `--tag`**. Missing either causes exit 2.
- A new **`insight tags` subcommand** lists the tag vocabulary with counts. `search`, `list`, `random`, `gc`, and `delete` gain `--tag`, `--category`, `--project`, `--general-only`, and `--project-only` filters.
- The default in-project read posture is `merge(local-project + global-general)`. Other projects' insights are walled off unless explicitly named.
- A **SessionStart hook** (`claudebase-read-insights-reminder.sh` / `.ps1`) reminds agents entering a fresh context window to pull relevant insights by tag and category — once per context, not every message.

This is the first feature that makes `--category` and `--tags` required on `insight create`. Every existing caller (~22 SDLC agent prompt files, the knowledge-base rule docs, and the UserPromptSubmit reminder hook) is updated in this release so no caller breaks. This is a **BREAKING CLI change** — `insight create` without `--category` or without `--tags` returns exit 2.

The release target is claudebase core **v0.7.0**.

### 18.2 User Story

As an SDLC pipeline agent starting a fresh context window, I want to pull relevant insights by tag and category — merging this project's local insights with general cross-project ones — so that I ground decisions in what prior sessions actually learned without flooding my context with unrelated knowledge from other projects.

### 18.3 Functional Requirements

#### FR-IHC-1: Schema v5 Migration for `insights.db` (Slice 1) — SCHEMA CHANGE

1. **FR-IHC-1.1:** A `SCHEMA_V5_DELTA` constant MUST add two nullable columns to the `documents` table: `category TEXT` and `project_slug TEXT`. No existing column may be modified or removed.
2. **FR-IHC-1.2:** A new normalized tags table MUST be created: `insight_tags(doc_id INTEGER NOT NULL REFERENCES documents(id) ON DELETE CASCADE, tag TEXT NOT NULL, UNIQUE(doc_id, tag))`.
3. **FR-IHC-1.3:** Two new indexes MUST be created: `CREATE INDEX idx_insight_tags_tag ON insight_tags(tag)` and `CREATE INDEX idx_documents_category ON documents(category)`.
4. **FR-IHC-1.4:** The `open_or_init_v2` function in `src/store.rs` MUST be extended with: (a) a fresh-database branch that stamps schema version 5 and applies the V5 delta; (b) a `v4 → 5` upgrade branch that runs the delta transactionally and is additive — books-corpus rows in `index.db` are unaffected because `category` defaults to NULL; (c) a `v5` idempotent re-open branch that probes the two columns and the `insight_tags` table via `pragma_table_info` before proceeding.
5. **FR-IHC-1.5:** On v4→v5 backfill, ALL existing insight rows (documents rows where `source_path` starts with `agent:`) MUST be updated: `category = 'project'`; `project_slug` derived from the db's project-path basename; one default tag equal to `feature_slug` (or `'untagged'` when `feature_slug` is NULL) inserted into `insight_tags`.
6. **FR-IHC-1.6:** Books-corpus rows (documents rows where `source_path` does NOT start with `agent:`) MUST retain `category = NULL` and MUST NOT receive any `insight_tags` entries. The V5 migration MUST be verifiable with a test asserting zero `insight_tags` rows for books-corpus doc_ids.
7. **FR-IHC-1.7:** `cargo test` MUST remain green after the migration: fresh→v5 stamp; v4→v5 adds columns and table; idempotent re-open; the 4 existing SDLC-repo insight rows backfill to `category='project'` with a non-empty tag.

#### FR-IHC-2: Global Insights Resolver (Slice 2) — SECURITY NOTE

1. **FR-IHC-2.1:** A new function `resolve_global_insights_db() -> PathBuf` in `src/store.rs` MUST return the fixed path `$HOME/.claude/knowledge/insights.db` (resolved via `std::env::var("HOME")` on Unix or `USERPROFILE` on Windows).
2. **FR-IHC-2.2:** The function MUST create the parent directory `~/.claude/knowledge/` if it does not exist, with the same permissions logic used for per-project db directories.
3. **FR-IHC-2.3:** This function DELIBERATELY bypasses the `resolve_project_root` cwd-containment gate (established in §15 FR-7.3). The bypass is safe and MUST be documented in a code comment: the path is a fixed `$HOME`-rooted constant and contains NO user-input-derived component. The security-auditor MUST confirm this during Slice 2 review.
4. **FR-IHC-2.4:** A unit test MUST assert the resolved path equals `$HOME/.claude/knowledge/insights.db` and that the parent directory is created.

#### FR-IHC-3: `insight create` — Mandatory Category and Tags, Dual-DB Routing (Slice 3) — BREAKING CLI CHANGE

1. **FR-IHC-3.1 [BREAKING]:** The `--category <general|project>` flag MUST be added to `InsightCreateArgs` in `src/cli.rs` as a `clap value_enum` required argument. Invocations that omit `--category` MUST exit 2 with a clap usage error. This is a breaking change from v0.6.0 where `--category` did not exist.
2. **FR-IHC-3.2 [BREAKING]:** The `--tags <tag>` flag MUST be added to `InsightCreateArgs` as a repeatable required argument accepting one or more comma-separated or space-separated tag strings. Invocations that supply `--category` but omit `--tags` entirely MUST exit 2 with the literal stderr message `error: insight create requires at least one --tag`. This is a breaking change from v0.6.0 where `--tags` did not exist.
3. **FR-IHC-3.3:** An optional `--project <slug>` flag MAY be supplied when `--category project`. When omitted with `--category project`, the project slug MUST be auto-derived from the cwd project basename. The `--project` flag MUST be silently ignored when `--category general`.
4. **FR-IHC-3.4:** `run_insight_create` MUST route the db open by category: `--category general` opens the global db via `resolve_global_insights_db()`; `--category project` opens the cwd-resolved local db via the existing `resolve_project_root` path.
5. **FR-IHC-3.5:** After the `documents` row insert, `run_insight_create` MUST insert one row per tag into `insight_tags`. Tag strings MUST be lowercased and stripped of leading `#` characters before insertion. Duplicate tags for the same doc MUST be silently dropped (the `UNIQUE(doc_id, tag)` constraint handles this).
6. **FR-IHC-3.6:** The `category` and `project_slug` columns on the inserted `documents` row MUST be populated from the `--category` and `--project` arguments.
7. **FR-IHC-3.7:** Existing exact-sha and semantic dedup logic MUST continue to fire per-db — general-category dedup is checked against the global db; project-category dedup is checked against the local db.
8. **FR-IHC-3.8:** Tests MUST cover: missing `--tags` → exit 2; missing `--category` → clap exit 2; `--category general` writes to global db and NOT to local db (asserted via direct SQL on both files); `--category project` writes to local db; tags are persisted in `insight_tags`; dedup still fires.

#### FR-IHC-4: `insight tags` Subcommand (Slice 4) — NEW COMMAND

1. **FR-IHC-4.1:** A new `InsightCmd::Tags` variant and `InsightTagsArgs` struct MUST be added to `src/cli.rs`. The subcommand is invoked as `claudebase insight tags`.
2. **FR-IHC-4.2:** `run_insight_tags` MUST execute `SELECT tag, COUNT(*) AS count FROM insight_tags GROUP BY tag ORDER BY count DESC` and return a list of `{tag, count}` objects.
3. **FR-IHC-4.3:** The `--category <c>` filter MUST restrict to tags for insights matching the given category (via a JOIN to `documents`).
4. **FR-IHC-4.4:** The `--project <slug>` filter MUST resolve `<slug>` via the project registry (`registry::resolve_project_path` reading `~/.claude/knowledge/projects.json`) and open THAT project's `insights.db` + the global db, merging tag counts across both. A slug not present in the registry MUST exit 1 with the literal stderr `error: project '<slug>' not found in registry`. (Corrected 2026-05-30 from the original "filter by project_slug column" framing to match plan.md:222 + QA TC-IHC-6.3, which are the executable contract.)
5. **FR-IHC-4.5:** The default posture (no filters) MUST merge tags from BOTH the local-project db and the global db — the same local+general merge posture as the read subcommands.
6. **FR-IHC-4.6:** `--json` output shape MUST be `[{"tag": "<string>", "count": <integer>}, ...]`.
7. **FR-IHC-4.7:** Tests MUST cover: returns distinct tags with descending counts; `--category general` lists only global-db tags; merged default includes both; json shape asserted.

#### FR-IHC-5: Dual-DB Reads — `search`, `list`, `random`, `gc`, `delete` (Slice 5)

1. **FR-IHC-5.1:** The following read subcommands MUST gain four new filter flags in `src/cli.rs`: `--tag <tag>` (repeatable; **OR / any-intersection semantics** — an insight carries many tags, and a result is returned if its tag set intersects the requested `--tag` set by AT LEAST ONE; `--tag nginx --tag docker` returns insights carrying nginx OR docker), `--category <c>`, `--project <slug>`, and two narrowing boolean flags `--general-only` / `--project-only`. (Operator decision 2026-05-27: OR, not AND. A future `--all-tags` flag for AND semantics is deferred.)
2. **FR-IHC-5.2:** The default in-project posture for `insight search`, `insight list`, and `insight random` MUST be `merge(local-project + global-general)`, reusing the RRF fusion machinery at `main.rs:2548-2604` adapted to operate over two insight dbs rather than a books db and an insight db. Cross-project reads are walled off unless `--project <other-slug>` is supplied, in which case the project registry resolves the db path.
3. **FR-IHC-5.3:** Tag filtering MUST be implemented as a single membership filter over the UNION of requested tags — `WHERE doc_id IN (SELECT doc_id FROM insight_tags WHERE tag IN (?, ?, ...))`, one parameterized placeholder per `--tag` value — applied after retrieval ranking to preserve RRF score ordering. This realizes the OR / any-intersection semantics of FR-IHC-5.1 (a per-tag intersect-all JOIN chain would wrongly implement AND).
4. **FR-IHC-5.4:** `--general-only` MUST read ONLY the global db and MUST ignore the local-project db entirely.
5. **FR-IHC-5.5:** `--project-only` MUST read ONLY the local-project db and MUST ignore the global db entirely.
6. **FR-IHC-5.6:** `insight gc` with `--category general` MUST run GC against the global db. Without `--category`, GC MUST run against BOTH dbs sequentially and report combined `{deleted, freed_bytes}`.
7. **FR-IHC-5.7:** `insight delete <id>` behavior is unchanged from §16 FR-AIB-8.3 for the local db; with `--category general`, the id is resolved against the global db.
8. **FR-IHC-5.8:** Tests MUST cover: `--tag nginx` returns rows carrying the `nginx` tag (among possibly many); **`--tag nginx --tag docker` returns a nginx-only insight, a docker-only insight, AND a both-tagged insight (OR / any-intersection, NOT AND)**; `--category general` reads only global; default in-project returns both project and general insights and excludes a planted other-project row; `--general-only` excludes project rows; `--project-only` excludes general rows.

#### FR-IHC-6: Project Registry at `~/.claude/knowledge/projects.json` (Slice 6) — NEW FILE

1. **FR-IHC-6.1:** A new `src/registry.rs` module MUST define a `ProjectRegistry` with entries shaped as `{name: String, path: String, last_seen: u64}` (epoch seconds). The registry is persisted as a JSON array at `~/.claude/knowledge/projects.json`.
2. **FR-IHC-6.2:** `upsert_project(root: &Path)` MUST: derive `name` from the canonical path basename; find an existing entry by canonical `path`; update `last_seen` if found; append a new entry if not found. The upsert MUST be keyed on canonical path to prevent duplicate entries for the same project under different relative representations.
3. **FR-IHC-6.3:** The upsert MUST use an atomic write-then-rename pattern: write the updated JSON to a temp file in the same directory, then `rename` (which is atomic on POSIX and near-atomic on Windows) to the final path. This prevents corrupt registry state from concurrent `claudebase run` invocations.
4. **FR-IHC-6.4:** `resolve_project_path(name: &str) -> Option<PathBuf>` MUST look up a project by name and return its path if found.
5. **FR-IHC-6.5:** The `upsert_project` call MUST be placed at the TOP of `run_claude_with_preset` in `src/main.rs` (near line 154), BEFORE the `exec()` call on Unix (near line 199). The registry write MUST NOT be placed after `exec()` — `exec()` replaces the process and any code after it never runs.
6. **FR-IHC-6.6:** Tests MUST cover: running `claudebase run` in a simulated test harness creates/updates the registry with the cwd project; upsert is idempotent on repeated calls with the same path; name→path lookup works; atomic write prevents partial reads.

#### FR-IHC-7: SessionStart "Read Insights on New Context" Hook (Slice 7) — NEW HOOK

1. **FR-IHC-7.1:** Two new hook files MUST be added to the `hooks/` directory: `claudebase-read-insights-reminder.sh` (bash, Unix) and `claudebase-read-insights-reminder.ps1` (PowerShell, Windows). Both are SessionStart hooks that fire on `startup`, `resume`, and `compact` events.
2. **FR-IHC-7.2:** The hook MUST emit `additionalContext` reminding the agent it is entering a fresh context window and SHOULD pull relevant insights: (a) call `claudebase insight tags --project <cwd-project>` to discover the available tag vocabulary; (b) call `claudebase insight search "<kw>" --tag <t>` (one or two representative calls) to load general and project insights by tag. The reminder MUST phrase the pull as conditional ("if entering fresh context") to avoid re-running on every message.
3. **FR-IHC-7.3:** The `.ps1` hook MUST be ASCII-only with no BOM. Non-ASCII characters (including PowerShell help comments with non-ASCII) MUST NOT appear. This constraint is established by the failure mode documented in commit `2d5eb8d` (ASCII-only PowerShell hooks — PS 5.1 parse failure).
4. **FR-IHC-7.4:** `install.sh` MUST wire the `claudebase-read-insights-reminder.sh` hook into `~/.claude/settings.json` under the `hooks.SessionStart` array using `jq`, keyed by the command string. The wiring MUST be idempotent — a second `install.sh` run MUST NOT add a duplicate entry.
5. **FR-IHC-7.5:** `install.ps1` MUST perform the equivalent wiring using `ConvertFrom-Json` / `ConvertTo-Json`, also idempotent.
6. **FR-IHC-7.6:** Tests MUST cover: `bash -n hooks/claudebase-read-insights-reminder.sh` exits 0; PowerShell parse of the `.ps1` exits 0; install wiring is idempotent (re-run = no-op); the hook text emits the reminder on a simulated SessionStart event.

#### FR-IHC-8: Update All Callers — SDLC Blast Radius (Slice 8) — BREAKING CHANGE REMEDIATION

1. **FR-IHC-8.1:** Every `insight create` invocation template or example in the SDLC agent prompt files (the ~22 files under `~/.claude/agents/*.md` or `src/agents/*.md`) MUST be updated to include `--category <value>` and `--tags <value>`. After this update, no bare `insight create` example lacking `--category` and `--tags` MUST remain in any agent prompt or rule file.
2. **FR-IHC-8.2:** `~/.claude/rules/knowledge-base-tool.md` MUST be updated to: document the new `--category` (required) and `--tags` (required, ≥1) flags on `insight create`; document the `insight tags` subcommand and the read-on-new-context flow; document the global/project routing semantics.
3. **FR-IHC-8.3:** `~/.claude/rules/knowledge-base.md` MUST be updated to reflect the new required flags in its CLI invocation contract section.
4. **FR-IHC-8.4:** The UserPromptSubmit reminder hook text (in `claudebase-selfcheck-reminder.sh` / `.ps1`) MUST be updated to include `--category` and `--tags` in any `insight create` examples it shows to agents.
5. **FR-IHC-8.5 (Done-when):** `grep -rE "insight create" ~/.claude/ src/agents/ ~/.claude/rules/` finds zero lines that lack `--category` AND zero lines that lack `--tag`. This grep-check is the acceptance gate for this slice.

#### FR-IHC-9: Docs and Release v0.7.0 (Slice 9)

1. **FR-IHC-9.1:** The claudebase `CHANGELOG.md` `[Unreleased]` section MUST gain entries: `Added` — hybrid corpus (global general-db at `~/.claude/knowledge/insights.db`, project registry at `~/.claude/knowledge/projects.json`, `insight tags` subcommand, tag and category filters on all read subcommands, read-on-new-context SessionStart hook); `Changed / BREAKING` — `insight create` now requires `--category` and `--tags` (missing either causes exit 2).
2. **FR-IHC-9.2:** `README.md` MUST gain a "Hybrid Insights Corpus" subsection documenting the routing model (`general` → global db, `project` → local db), the `insight tags` discovery flow, the project registry, and the `--category`/`--tags` requirement.
3. **FR-IHC-9.3:** The `/release` command MUST produce claudebase core release `v0.7.0` after `/merge-ready` reports MERGE READY.

### 18.4 Non-Functional Requirements

1. **NFR-IHC-1 (Backward compatibility — schema):** The v5 migration MUST be additive. No existing column on `documents` may be dropped or renamed. Books-corpus rows MUST remain valid after the migration. Existing v4 insights MUST be backfilled with non-empty `category` and `project_slug` values and at least one `insight_tags` row.
2. **NFR-IHC-2 (Security backbone preserved):** The `resolve_project_root` cwd-containment gate MUST remain the only path-from-user-input gate for per-project db access. The `resolve_global_insights_db` bypass (FR-IHC-2.3) is explicitly exempt and documented. No other bypass is introduced.
3. **NFR-IHC-3 (Registry concurrency safety):** Concurrent `claudebase run` invocations on the same machine MUST NOT corrupt `projects.json`. The atomic write-then-rename strategy (FR-IHC-6.3) is the required mechanism.
4. **NFR-IHC-4 (Hook nag frequency):** The SessionStart hook fires on every `startup`, `resume`, and `compact` event. The hook text MUST be lightweight `additionalContext` (not a blocking prompt) and MUST NOT exceed 200 words of injected reminder text.
5. **NFR-IHC-5 (Tag storage — normalized):** Tags are stored in a normalized `insight_tags(doc_id, tag)` table rather than a denormalized JSON/CSV column on `documents`. This enables efficient `GROUP BY tag` counts and an indexed `WHERE tag = ?` filter without full-table scans.
6. **NFR-IHC-6 (CLI breaking change is intentional and fully remediated):** The breaking change to `insight create` (adding required `--category` and `--tags`) is operator-approved. All in-repository callers are updated in Slice 8 before v0.7.0 is released. External callers not maintained in this repository are warned via the `CHANGELOG.md` BREAKING entry.
7. **NFR-IHC-7 (Single-binary constraint):** All new functionality (global resolver, registry, dual-db reads, hook scripts) MUST compile into the existing `claudebase` binary with no additional runtime dependencies. No Python, no Node.js, no new external binaries.

### 18.5 Acceptance Criteria

1. **AC-IHC-1 (Schema v5 — fresh stamp):** `claudebase status --json` on a fresh post-install database returns `"schema_version": 5`.
2. **AC-IHC-2 (Schema v5 — v4 migration additive):** On a v4 `insights.db` with existing insight rows and book rows in `index.db`, running any `claudebase insight` subcommand triggers the v4→v5 migration; `pragma_table_info(documents)` shows `category` and `project_slug` columns; the `insight_tags` table exists; book-corpus rows have `category = NULL` and zero `insight_tags` entries.
3. **AC-IHC-3 (Backfill):** The 4 existing SDLC-repo insight rows (inserted at v4) each have `category = 'project'` and at least one row in `insight_tags` after migration.
4. **AC-IHC-4 (Mandatory enforcement — tags):** `claudebase insight create "test" --category project` (no `--tags`) exits 2 and writes to neither db.
5. **AC-IHC-5 (Mandatory enforcement — category):** `claudebase insight create "test" --tags foo` (no `--category`) exits 2 (clap error).
6. **AC-IHC-6 (Routing — general):** `claudebase insight create "nginx lesson" --category general --tags nginx` inserts a row into `~/.claude/knowledge/insights.db` and NOT into the cwd local `insights.db`. Verified via `sqlite3 ~/.claude/knowledge/insights.db "SELECT count(*) FROM documents WHERE source_type='agent-learned'"` returning ≥ 1, and cwd local db count unchanged.
7. **AC-IHC-7 (Routing — project):** `claudebase insight create "local lesson" --category project --tags myfeature` inserts a row into the cwd local `insights.db` and NOT into `~/.claude/knowledge/insights.db`.
8. **AC-IHC-8 (Tags subcommand):** `claudebase insight tags --json` returns a JSON array with at least one element having `tag` and `count` fields after AC-IHC-6 and AC-IHC-7 complete.
9. **AC-IHC-9 (Search — merged default):** In-project `claudebase insight search "lesson" --json` returns hits from BOTH the local db and the global db and excludes a row planted in a second unrelated project's db.
10. **AC-IHC-10 (Search — tag filter):** `claudebase insight search "lesson" --tag nginx --json` returns only rows tagged `nginx`.
11. **AC-IHC-11 (Search — general-only):** `claudebase insight search "lesson" --general-only --json` returns only global-db rows.
12. **AC-IHC-12 (Registry — created on run):** After `claudebase run` executes (or the registry entry-point is triggered in a test harness), `~/.claude/knowledge/projects.json` exists and contains an entry for the cwd project with a non-null `last_seen` epoch.
13. **AC-IHC-13 (Registry — idempotent):** Running the registry upsert twice with the same cwd produces exactly one entry for that project in `projects.json`, not two.
14. **AC-IHC-14 (Hook — parse):** `bash -n hooks/claudebase-read-insights-reminder.sh` exits 0. PowerShell `-Command "& { . 'hooks/claudebase-read-insights-reminder.ps1' }"` exits 0 on a PS 5.1 engine.
15. **AC-IHC-15 (Hook — install idempotent):** Running `bash install.sh --yes` twice does not produce duplicate entries for `claudebase-read-insights-reminder.sh` in `~/.claude/settings.json`.
16. **AC-IHC-16 (Callers updated):** `grep -rE "insight create" ~/.claude/ src/agents/ ~/.claude/rules/ | grep -v "\-\-category" | wc -l` returns 0.
17. **AC-IHC-17 (Release):** After `/merge-ready` reports MERGE READY and `/release` completes, `claudebase --version` returns `0.7.0` and `CHANGELOG.md` contains a versioned `[0.7.0]` section with a `Changed / BREAKING` entry for `insight create`.

### 18.6 Affected CLI Surface

**New flags on `insight create` (BREAKING — required):**
- `--category <general|project>` — routing key; required; clap `value_enum`
- `--tags <tag>` — repeatable; at least one required; empty list → exit 2
- `--project <slug>` — optional; applicable only with `--category project`

**New flags on `insight search`, `list`, `random`, `gc`, `delete`:**
- `--tag <tag>` — repeatable; AND semantics
- `--category <c>` — filter by category
- `--project <slug>` — resolve cross-project db via registry
- `--general-only` — read global db only
- `--project-only` — read local db only

**New subcommand:**
- `claudebase insight tags [--category <c>] [--project <slug>] [--json]`

**Unchanged subcommands (interface):**
- `insight search`, `insight list`, `insight random`, `insight get`, `insight gc`, `insight delete` — interface additive only (new filter flags); existing flags unchanged.

### 18.7 Schema Changes

All schema changes apply to `insights.db` only. `index.db` (books corpus) and `chat.db` (daemon) are unaffected.

**`insights.db` — Schema v5 (additive on top of §16 v4):**

```sql
-- New columns on the existing `documents` table:
ALTER TABLE documents ADD COLUMN category TEXT;       -- 'general' | 'project' | NULL (books rows)
ALTER TABLE documents ADD COLUMN project_slug TEXT;   -- cwd project basename; NULL for general insights

-- New index for category filtering:
CREATE INDEX idx_documents_category ON documents(category);

-- New normalized tags table:
CREATE TABLE IF NOT EXISTS insight_tags (
    doc_id  INTEGER NOT NULL REFERENCES documents(id) ON DELETE CASCADE,
    tag     TEXT NOT NULL,
    UNIQUE(doc_id, tag)
);
CREATE INDEX idx_insight_tags_tag ON insight_tags(tag);
```

**Backfill for existing v4 insight rows (rows where `source_path LIKE 'agent:%'`):**

```sql
UPDATE documents
SET    category     = 'project',
       project_slug = '<derived-from-db-path-basename>'
WHERE  source_path LIKE 'agent:%' AND category IS NULL;

-- One default tag row per existing insight, using feature_slug or 'untagged':
INSERT OR IGNORE INTO insight_tags (doc_id, tag)
SELECT id, COALESCE(NULLIF(feature_slug, ''), 'untagged')
FROM   documents
WHERE  source_path LIKE 'agent:%' AND category = 'project';
```

**New file: `~/.claude/knowledge/projects.json`**

```json
[
  { "name": "<project-basename>", "path": "<canonical-absolute-path>", "last_seen": 1748376015 }
]
```

**New file: `~/.claude/knowledge/insights.db` (global)**

Identical schema to the per-project `insights.db` (v5), but populated only with `category = 'general'` rows.

### 18.8 File and FS Changes

**claudebase Rust binary (new or modified files):**
- `src/store.rs` — `SCHEMA_V5_DELTA` constant + v4→v5 migration branch + v5 idempotent probe + backfill logic + `resolve_global_insights_db()`
- `src/cli.rs` — `InsightCreateArgs` extended with `--category`, `--tags`, `--project`; new `InsightTagsArgs` + `InsightCmd::Tags`; read-subcommand filter flags
- `src/main.rs` — `run_insight_create` dual-db routing (:895); `run_insight_search`, `run_insight_list`, `run_insight_random`, `run_insight_gc`, `run_insight_delete` dual-db + tag/category filters; new `run_insight_tags`; `run_claude_with_preset` registry hook (:154)
- `src/registry.rs` — **NEW** — `ProjectRegistry`, `upsert_project`, `resolve_project_path`
- `hooks/claudebase-read-insights-reminder.sh` — **NEW** — bash SessionStart hook
- `hooks/claudebase-read-insights-reminder.ps1` — **NEW** — PowerShell SessionStart hook (ASCII-only, no BOM)
- `install.sh` — wire new hook into `~/.claude/settings.json` idempotently
- `install.ps1` — wire new hook into `~/.claude/settings.json` idempotently (ConvertFrom-Json / ConvertTo-Json)
- `CHANGELOG.md` — `[Unreleased]` entries (Added + Changed/BREAKING)
- `README.md` — "Hybrid Insights Corpus" subsection

**SDLC repo (caller blast radius — Slice 8):**
- `~/.claude/agents/*.md` — ~22 agent prompt files with `insight create` examples updated
- `~/.claude/rules/knowledge-base-tool.md` — CLI contract + surfacing protocol updated
- `~/.claude/rules/knowledge-base.md` — CLI invocation contract updated
- `hooks/claudebase-selfcheck-reminder.sh` / `.ps1` — UserPromptSubmit reminder hook updated

**New persistent FS artifacts (created at runtime, not committed):**
- `~/.claude/knowledge/insights.db` — global insights database (created on first `--category general` write)
- `~/.claude/knowledge/projects.json` — project registry (created on first `claudebase run`)

### 18.9 Out of Scope

The following items are explicitly excluded from v0.7.0:

1. **Content migration of existing project insights to the global db.** Hybrid storage keeps existing project insights local by design — zero content migration in v0.7.0.
2. **Cross-project global search** (reading ALL project dbs). The registry enables this later; not in v0.7.0.
3. **Category taxonomy beyond `{general, project}`.** The 2-value enum is extensible in a later release.
4. **Tag synonyms, hierarchical tags, or tag-rename tooling.** Free-form tags are sufficient for v0.7.0; taxonomy features are deferred.
5. **Auto-classification of category by content.** Agents supply `--category` explicitly; no ML-based routing in v0.7.0.
6. **`sdlc-knowledge` embedded tag-scheme versioning.** Tag-scheme disambiguation at `/release` (per `auto-release.md`) is a release-engineer concern, not a PRD requirement.

### 18.10 Risks and Dependencies

1. **Shared `documents` table — category/project_slug columns on a books-corpus-shared table.** The v5 delta adds insight-only semantics to a table shared with books-corpus rows. Backfill MUST leave book rows with `category = NULL`. The architect must confirm this column-on-shared-table approach is acceptable vs. a dedicated insight table. Default: columns on shared table (consistent with the v4 metadata pattern). — Risk: medium.
2. **`exec()` replaces the process — registry write must precede it.** `run_claude_with_preset` at main.rs:154 calls `exec()` at line 199 on Unix. A registry write placed after the `exec()` call never executes. Mitigation: FR-IHC-6.5 mandates placement at the TOP of the function, verified by the done-when test. — Risk: high (correctness); mitigation strong.
3. **Breaking CLI change blast radius.** Every `insight create` call site breaks if `--category` and `--tags` are not added. Mitigation: Slice 8 updates all in-repository callers before v0.7.0 release; CHANGELOG BREAKING entry warns external callers. — Risk: medium.
4. **Dual-db RRF fusion generalization.** The existing `--corpus all` machinery at main.rs:2548-2604 was designed for books+insights, not insight+insight. The corpus-label handling may be hardwired. Mitigation: Slice 5 reads `run_one_corpus_for_fusion` fully before reusing; if it does not generalize, a simplified union+re-sort is the fallback. — Risk: medium; must be verified before Slice 5 implementation.
5. **Global-resolver cwd-gate bypass security.** `resolve_global_insights_db()` bypasses `resolve_project_root`. Safe because the path is a fixed `$HOME`-rooted constant with no user-input-derived component. Security-auditor MUST confirm during Slice 2. — Risk: low (path is fixed); confirmatory review required.
6. **Registry concurrency.** Multiple concurrent `claudebase run` invocations race on `projects.json`. Mitigation: atomic write-then-rename (FR-IHC-6.3). — Risk: low with mitigation in place.
7. **SessionStart hook frequency.** Fires on every compact, which may occur frequently during long sessions. Mitigation: hook text is lightweight additionalContext (≤200 words), phrased conditionally. — Risk: low.
8. **Backfill of 4 existing SDLC-repo insights.** They are v4, tag-less; mandatory tags cannot be retroactive. Mitigation: backfill derives a default tag from `feature_slug` (or `'untagged'` when NULL). — Risk: low.

## Facts

### Verified facts

- `open_or_init_v2` at `src/store.rs:222` is the single insights/books DB-open+migrate entry point; v4→5 is the required new branch — source: `.claude/plan.md` lines 154 (Verified facts block, "open_or_init_v2 (store.rs:222) is the single insights/books DB-open+migrate entry point"), confirmed against the plan read this session. — salience: high
- Insight metadata (`source_type`, `agent_name`, `session_id`, `feature_slug`, `salience`, `parent_artifact`) is stored as nullable columns on the SHARED `documents` table (SCHEMA_V4_DELTA, store.rs:197-207); books rows keep them NULL — so adding `category`/`project_slug` the same way is consistent with the existing pattern — source: plan.md Verified facts, lines 155-156, read this session. — salience: high
- There is NO tags table and NO category column in v4; `feature_slug` is the only tag-ish field — source: plan.md Verified facts line 156 ("There is NO tags table and NO category column today"), read this session. — salience: high
- `run_insight_create` is at main.rs:895; insight read handlers are at: `run_insight_search` (:1148), `run_insight_list` (:1315), `run_insight_random` (:1388), `run_insight_get` (:1417), `run_insight_gc` (:1466), `run_insight_delete` (:1557) — source: plan.md Verified facts line 157, read this session. — salience: high
- Cross-corpus RRF fusion exists at `main.rs:2548-2604` (`run_one_corpus_for_fusion`) and is reusable for the local+general insight merge — source: plan.md Verified facts line 158, read this session. — salience: high
- `claudebase run` dispatches to `run_claude_with_preset` at main.rs:154; `exec()` is called at main.rs:199 on Unix — registry write must precede exec — source: plan.md Verified facts line 159, read this session. — salience: high
- `chat.db` already lives at `$HOME/.claude/knowledge/` (store.rs:1483) — precedent for HOME-rooted global db path — source: plan.md Verified facts line 160, read this session. — salience: medium
- The `resolve_project_root` gate rejects targets not under cwd, so `--project-root $HOME` fails from a project subdir — global resolver MUST bypass this gate — source: plan.md Verified facts line 162 ("resolve_project_root rejects targets not under cwd ... this FALSIFIED the earlier 'global via --project-root $HOME' hypothesis"), read this session. — salience: high
- Existing PRD has sections §1 through §17; §18 is the next available number — source: grep output this session showing `§15`, `§16`, `§17` headings; line count = 781. — salience: medium
- Knowledge base corpus: `doc_count = 0` (empty) — confirmed via `claudebase status --json` this session. Corpus scope verdict: **No overlap** — empty corpus, topical queries skipped. — salience: low
- Insights corpus query returned 0 hits for "hybrid corpus insights tags category routing" at `--salience high` — no prior session insights on this feature — confirmed via `claudebase insight search` this session. — salience: low
- Commit `2d5eb8d` ("fix(infra): ASCII-only PowerShell hooks — Windows PS 5.1 parse failure") establishes the ASCII-only `.ps1` constraint — source: git log shown in session context at conversation start. — salience: medium

### External contracts

- **`claudebase` CLI v0.6.0** — symbol: `insight create` flags today: `--type`, `--agent`, `--session`, `--feature`, `--salience`, `--source-artifact`, `--project-root`, `--db-name`; no `--category`, no `--tags` — source: plan.md External contracts block, read this session — verified: yes — salience: high
- **`clap` derive macros** — symbol: `#[arg(long, value_enum)]` (required), `#[arg(long)]` (repeatable via `Vec<String>`) — used by existing `Salience`/`SearchMode` enums; new `--category` follows same `value_enum` pattern — source: plan.md External contracts, "cli.rs:732,767", read this session — verified: yes — salience: medium
- **`rusqlite`** — symbol: `Connection::transaction`, `execute_batch`, `pragma_table_info`, `ALTER TABLE … ADD COLUMN` — the exact primitives the v4 migration uses; V5 reuses them — source: plan.md External contracts, "store.rs:234-341", read this session — verified: yes — salience: high
- **SQLite FTS5 + `sqlite-vec`** — symbol: `chunks_fts`, `chunks_vec USING vec0(embedding float[384])` — both insight dbs share this schema; `insight_tags` is a plain table — source: plan.md External contracts, "store.rs:259-270", read this session — verified: yes — salience: medium

### Assumptions

- Architect will accept `category`/`project_slug` as columns on the shared `documents` table (consistent with v4) rather than mandating a dedicated insight table — risk: a dedicated table would enlarge Slice 1 and ripple into all handlers — how to verify: architect review verdict at bootstrap Step 3. — salience: high
- The `--corpus all` RRF fusion at main.rs:2548-2604 generalizes from books+insights to insight+insight by swapping the second corpus path/label — risk: corpus-label or schema assumption may be hardwired — how to verify: Slice 5 reads `run_one_corpus_for_fusion` fully before reusing; fallback is simplified union+re-sort. — salience: high
- The SessionStart hook is the correct surface for read-on-new-context (vs. folding into the existing UserPromptSubmit reminder) — risk: SessionStart fires on every compact and may inject too frequently — how to verify: ba-analyst use-case + manual hook-fire test in Slice 7; hook text is capped at ≤200 words. — salience: medium
- `main` is in a releasable state to branch from (agent-chat-daemon feature either merged or cleanly independent) — risk: branching off a dirty main — how to verify: `git status` + `git log main` at bootstrap Step 0. — salience: medium
- Tags stored in normalized `insight_tags(doc_id, tag)` (rather than denormalized JSON on `documents`) provides efficient `GROUP BY tag` counts and indexed filter — architect to confirm — risk: if architect objects, denormalized approach is a fallback. — salience: medium

### Open questions

- Backfill default tag for the 4 existing tag-less v4 insights: `feature_slug` vs literal `'untagged'` — if `feature_slug` is NULL the fallback is `'untagged'`; this is the current plan default — needs: planner/architect confirmation in Slice 1 done-when. — salience: low
- Whether v0.7.0 also touches the `sdlc-knowledge` embedded tag-scheme or only the claudebase-core scheme — needs: release-engineer at `/release` (tag-scheme disambiguation per auto-release.md). — salience: low
- knowledge-base: corpus is empty (doc_count=0); task domain is CLI engineering + SQLite schema migration + cross-project insights routing; no overlap. Topical queries skipped per corpus-scope-relevance protocol. Corpus enrichment with Rust/SQLite/CLI engineering reference materials would benefit future similar tasks. — salience: low

## Decisions

### Inbound validation

- Task received: append §18 PRD section to `docs/PRD.md` based on the approved plan at `.claude/plan.md`. Plan read in full (199 lines). The plan's "schema v5" for `insights.db` conflicts in name with §17's "Schema v5" for `chat.db` — investigated: these are independent SQLite files with independent schema version tracking; no actual conflict. Challenged: yes — surfaced the naming overlap; confirmed it is not a semantic conflict. Outcome: proceeded, noting in FR-IHC-1 that this is `insights.db` schema v5 to distinguish from `chat.db`'s own version history. — salience: medium
- The plan's Deliverables checklist refers to "PRD section in claudebase/docs/PRD.md (prd-writer) — each FR with a Changelog: field". Checked: the Changelog field is a section-level field (not per-FR); the plan's phrasing is imprecise. The correct interpretation per the PRD format convention (visible in §15, §16, §17) is one `Changelog:` field at the top of the section. Proceeded with the section-level field. — challenged: yes — outcome: proceeded with correct placement. — salience: low

### Decisions made

- **Hybrid storage routing is the correct model** over single-global-db or per-project-only. Q1 hack? no — strictly more backward-compatible (zero content migration). Q2 sane? yes — matches operator's stated mental model and data locality. Q3 alternatives? single-global (rejected: forces migration); per-project-only (rejected: no cross-project sharing). Q4 symptom/cause? cause — the gap is structural. Q5 n/a. Source: plan.md Decisions block, read this session. — salience: high
- **`category`/`project_slug` as columns on the shared `documents` table** (pending architect). Q1 hack? no — mirrors v4 pattern exactly. Q2 sane? yes — proportional. Q3 alternatives? dedicated insight table (cleaner but heavier; deferred to architect). Q4 cause. Q5 tracked in Open questions. Source: plan.md Decisions block, read this session. — salience: high
- **`insight_tags` as a normalized table** (not denormalized JSON/CSV). Q1 hack? no — normalized enables efficient grouped counts and indexed filter joins. Q2 sane? yes. Q3 alternatives? JSON array on documents (rejected: no efficient GROUP BY, no indexed filter). Q4 cause. Q5 n/a. Architect to confirm. — salience: medium
- **Registry write at the top of `run_claude_with_preset`, before exec.** Q1 hack? no — only correct placement given exec() semantics. Q2 sane? yes. Q3 alternatives? separate `claudebase register` subcommand (rejected: operator wants automatic at `run` startup). Q4 cause. Q5 n/a. Source: plan.md Decisions block, read this session. — salience: high
- **Reuse existing `--corpus all` RRF fusion for local+general merge.** Q1 hack? no — reuses proven machinery. Q2 sane? yes. Q3 alternatives? naive union+re-sort (rejected: loses calibrated cross-corpus ranking). Q4 cause. Q5 generalization assumption tracked in Assumptions. Source: plan.md Decisions block, read this session. — salience: medium

### Hacks acknowledged

(none) — PRD authoring only; no implementation hacks introduced.

### Symptom-only patches (with root-cause links)

(none) — no symptom-only patches in this PRD section.

---

## §19. Telegram Multi-CLI Orchestration — Chat-as-ID Routing, Bot Commands, Inline-Keyboard Questionnaires, and Plugin Cutover

**Status:** [PLANNED]
**Date:** 2026-05-30
**Priority:** High
**Related:** §17 (Agent Chat Daemon + Telegram Bridge — this section extends the daemon's Telegram bridge at `src/daemon/telegram.rs`, the `ChatBus` at `src/daemon/chat.rs`, and the `agent_registry` at `src/daemon/agent_registry.rs` that shipped in §17; the `chat.db` schema v6 introduced in §17 is extended to v7 here). §18 (Insights Hybrid Corpus — no functional overlap; the `chat.db` version namespace is independent of `insights.db`). Plan source: `/Users/aleksandra/Documents/claude-code-sdlc/claudebase/.claude/plan.md` (198 lines, read in full this session, 2026-05-30).

Changelog: The Telegram bot can now route messages from each chat to its own bound Claude Code instance — tap /switch to rebind, tap /agents to see who is alive, and answer agent questions directly from Telegram as inline buttons.

### 19.1 Feature Description

The operator runs multiple Claude Code CLI instances and needs ONE Telegram bot to serve all of them. Today the only Telegram path is the per-CLI plugin (`plugins/telegram-rs/`), which holds the bot token's `getUpdates` slot. Two CLI instances sharing the same token each attempt to poll `getUpdates`; Telegram enforces a single-consumer rule and the second poller receives 409 Conflict — the core pain that makes multi-CLI routing impossible with the current architecture.

The daemon already contains a dormant server-centric Telegram bridge (`src/daemon/telegram.rs`, 68 KB) that defaults to `enabled: true` but has not yet been activated for production use because it lacks three capabilities: (1) a persistent chat-to-CLI binding so each Telegram chat consistently reaches the same CLI; (2) bot commands to inspect and switch that binding; (3) a mechanism for agents to surface multiple-choice questions as tappable inline keyboard buttons.

This feature activates the daemon's Telegram bridge as the authoritative single poller, adds a **chat-as-id routing layer** (one Telegram chat = one CLI binding, keyed by `chat_id` alone — operator decision 2026-05-30, OQ-3 closed), adds bot commands (`/agents`, `/switch`, `/whoami`, `/here`), extends the outbound path with `reply_markup` support for inline keyboards, and introduces a new `chat_ask` MCP tool that renders multiple-choice agent questions as tappable Telegram buttons.

The per-CLI plugin is preserved as a revert path behind the `[telegram] enabled` flag, and a conflict gate makes the 409-Conflict condition loud and non-crashing so the operator can stop the plugin poller cleanly before the daemon takes over.

HTTP/WSS transport and cross-machine fleet routing are explicitly out of scope (deferred to a later feature); this feature operates entirely over the existing UDS daemon on a single machine.

### 19.2 User Story

As the operator running multiple Claude Code CLIs, I want a single Telegram bot that routes each conversation to its designated CLI — and lets me answer agent questions from my phone by tapping buttons — so that I can manage a multi-agent workflow from Telegram without 409 Conflict errors or manual token juggling.

### 19.3 Functional Requirements

#### Schema v7 and Registry Helpers (Slice 1)

**FR-TMC-1.1 (chat.db schema v7 — `active_cli_per_chat` table):** The function `ensure_chat_db_schema` in `src/daemon/chat.rs` MUST be extended to apply a schema v7 migration that adds the following table if it does not already exist:

```sql
CREATE TABLE IF NOT EXISTS active_cli_per_chat (
    chat_id         INTEGER PRIMARY KEY,
    active_cli_name TEXT NOT NULL,
    active_agent_id TEXT NOT NULL,
    set_at          INTEGER NOT NULL,
    set_by          TEXT NOT NULL
);
```

The migration MUST be additive and idempotent; all existing v6 rows MUST survive unchanged.

**FR-TMC-1.2 (chat.db schema v7 — `tg_message_map` table):** The same migration MUST add:

```sql
CREATE TABLE IF NOT EXISTS tg_message_map (
    tg_msg_id       INTEGER NOT NULL,
    chat_id         INTEGER NOT NULL,
    sender_agent_id TEXT NOT NULL,
    sent_at         INTEGER NOT NULL,
    PRIMARY KEY (chat_id, tg_msg_id)
);
```

`tg_message_map` stores the mapping from a daemon-proxied outbound Telegram message ID to the CLI that sent it, enabling reply-quote routing back to the originating CLI.

**FR-TMC-1.3 (TTL purge of `tg_message_map`):** A background task (or purge triggered at startup and periodically thereafter) MUST delete rows from `tg_message_map` where `sent_at < (current_unix_seconds - 2592000)` (30-day TTL, 30 x 86400 = 2592000 seconds). Rows MUST NOT be purged sooner than 30 days after insertion.

**FR-TMC-1.4 (`is_alive` helper in `agent_registry`):** `src/daemon/agent_registry.rs` MUST gain a new public function `is_alive(conn: &Connection, agent_id: &str) -> anyhow::Result<bool>` that returns `true` if and only if a row with the given `agent_id` exists in the `agent_registry` table with a non-orphaned status. Only `list_alive` exists today (verified at `agent_registry.rs:200`); `is_alive` is a new addition.

**FR-TMC-1.5 (`first_alive` helper in `agent_registry`):** `src/daemon/agent_registry.rs` MUST gain a new public function `first_alive(conn: &Connection, thread: Option<&str>, prefer_role: Option<&str>) -> anyhow::Result<Option<AgentRow>>` that returns the first alive agent matching the given thread (if specified) and preferring an agent whose name contains `prefer_role` (e.g. `"orchestrator"`). If no match on `prefer_role`, any alive agent is returned; if no alive agents exist, `None` is returned. This is the default-CLI resolver used by the routing tree.

#### 5-Step Routing Decision Tree (Slice 2)

**FR-TMC-2.1 (routing tree replaces `@-mention` precursor):** The `@-mention`-only routing precursor at `telegram.rs:333` (`extract_first_mention` to `list_alive` to `meta.target_agent_id`) MUST be replaced by the following 5-step decision tree, evaluated in order for every inbound Telegram `message` update:

1. **Bot command step:** If `msg.text` starts with `/` and matches one of `/agents`, `/switch`, `/whoami`, `/here`, `/start`, `/help`, `/status`, handle the command server-side (see FR-TMC-3.x), reply to the originating Telegram chat, and route NO message to any CLI. The routing tree terminates.
2. **Reply-quote step:** If `msg.reply_to_message` is present, look up `(chat_id, msg.reply_to_message.message_id)` in `tg_message_map`. If a row is found, set `target_agent_id` to the `sender_agent_id` from that row. If `is_alive` returns `false` for that agent, log a diagnostic note (e.g., "original sender CLI is no longer alive; falling through to active binding") and fall through to step 4.
3. *(Omitted under chat-as-id — no per-user "last addressed" state is maintained.)*
4. **Active binding step:** Look up `chat_id` in `active_cli_per_chat`. If a row exists and `is_alive` returns `true` for its `active_agent_id`, set `target_agent_id` to that value. If no row exists or the bound agent is dead, call `first_alive(conn, thread=None, prefer_role="orchestrator")` and use the result as the target.
5. **No CLI step:** If all preceding steps yield no alive target, reply to the Telegram chat with the message: "No CLIs online. Spawn one with `claudebase run`." Route no message to any CLI.

**FR-TMC-2.2 (target propagation):** After the routing tree resolves a `target_agent_id`, the daemon MUST tag the channel notification with `meta.target_agent_id` equal to the resolved agent's `agent_id`. This preserves the existing `ChatBus` publish-to-specific-subscriber behavior introduced in §17's `@-mention` precursor.

**FR-TMC-2.3 (chat isolation):** A message arriving on `chat_id` A MUST NOT be routed to the CLI bound under `chat_id` B, even if B's binding is the only one in `active_cli_per_chat`. Each chat's binding is independent.

#### Bot Commands (Slice 3)

**FR-TMC-3.1 (`/agents` command):** When the daemon receives `/agents` in a Telegram message, it MUST call `list_alive(conn, thread=None)` and reply with a formatted list of alive CLI names and their agent IDs. The reply MUST be sent to the originating `chat_id` via the existing Telegram outbound path. If no CLIs are alive, the reply MUST say "No CLIs currently online."

**FR-TMC-3.2 (`/switch <name>` command):** When the daemon receives `/switch <name>`, it MUST:
- Validate that `<name>` matches an alive agent name via `list_alive`; if not, reply with an error message identifying the unknown name and listing available names.
- Write a row to `active_cli_per_chat` with `chat_id`, `active_cli_name = <name>`, `active_agent_id = <resolved agent_id>`, `set_at = current_unix_seconds`, `set_by = <telegram user_id or username>`. Use `INSERT OR REPLACE` so an existing binding is overwritten atomically.
- Reply to the Telegram chat confirming the new binding. The reply MUST include a note that in group chats this rebinds the entire chat for all participants (chat-as-id semantics).

**FR-TMC-3.3 (`/whoami` command):** When the daemon receives `/whoami`, it MUST look up `active_cli_per_chat[chat_id]` and reply with the bound CLI name and agent ID. If no binding exists, the reply MUST name the default (the result of `first_alive(prefer_role="orchestrator")`) and indicate that no explicit binding is set.

**FR-TMC-3.4 (`/here` command):** When the daemon receives `/here`, it MUST look up the bound CLI's `AgentRow` in `agent_registry` and reply with the agent's `host` and `cwd` metadata fields. If the bound CLI is not found or those fields are absent, the reply MUST indicate the information is unavailable.

**FR-TMC-3.5 (existing commands preserved):** The existing `/start`, `/help`, and `/status` handlers in the daemon Telegram bridge MUST remain functional and unmodified in behavior after this feature lands.

**FR-TMC-3.6 (bot-command response latency):** All four new bot commands (`/agents`, `/switch`, `/whoami`, `/here`) MUST respond within approximately 1 second under normal load. They operate on local SQLite state only; no CLI roundtrip is required.

#### Outbound Reply-Quote Tracking (Slice 4)

**FR-TMC-4.1 (outbound message recording):** Every daemon-proxied outbound Telegram message (produced by the `enqueue_outbound_tg` path at `telegram.rs:81` or its successor) MUST, after the `sendMessage` API call succeeds and returns a Telegram `message_id`, insert a row into `tg_message_map`:
- `tg_msg_id` = the `message_id` returned by the Telegram API.
- `chat_id` = the `chat_id` the message was sent to.
- `sender_agent_id` = the `agent_id` of the CLI that produced the message.
- `sent_at` = current Unix seconds at the time of the API call.

**FR-TMC-4.2 (recording survives restart):** Because `tg_message_map` is persisted in `chat.db`, reply-quote routing MUST survive a daemon restart for any message inserted within the 30-day TTL window.

**FR-TMC-4.3 (no double-recording):** A single outbound Telegram message MUST produce exactly one row in `tg_message_map`. Retry logic on API failure MUST NOT produce duplicate rows; the `PRIMARY KEY (chat_id, tg_msg_id)` constraint enforces uniqueness at the SQLite layer (`INSERT OR IGNORE` acceptable for retries).

#### Inline-Keyboard Questionnaire — `chat_ask` MCP Tool (Slice 5)

**FR-TMC-5.1 (`callback_query` update handling):** The `getUpdates` polling loop and `process_batch` function in `src/daemon/telegram.rs` MUST be extended to parse and handle `callback_query` update objects (currently only `message` updates are processed). On receipt of a `callback_query` update the daemon MUST:
- Call `answerCallbackQuery` with the `callback_query_id` to dismiss the loading spinner on the operator's device.
- Decode the `callback_data` field as `<question_id>:<option_idx>` (colon-delimited, both components are ASCII).
- Route the decoded answer to the CLI bound to the originating `chat_id` via `active_cli_per_chat`.

**FR-TMC-5.2 (`callback_data` size discipline):** The `callback_data` string MUST be no greater than 64 bytes as required by the Telegram Bot API. The `question_id` component MUST be a compact opaque identifier (e.g., a short UUID prefix or a monotonic counter rendered as decimal) — NOT the full question text.

**FR-TMC-5.3 (`chat_ask` MCP tool — interface):** A new MCP tool `chat_ask` MUST be added to the daemon's `tools/list` response (dispatch at `src/daemon/server.rs`) with the following interface:

```json
{
  "name": "chat_ask",
  "description": "Send a multiple-choice question to a Telegram chat as an inline keyboard. Returns the index and label of the option the operator taps.",
  "inputSchema": {
    "type": "object",
    "properties": {
      "thread":   { "type": "string", "description": "Thread id, e.g. telegram:<chat_id>" },
      "question": { "type": "string", "description": "Question text displayed above the buttons" },
      "options":  {
        "type": "array",
        "items": {
          "type": "object",
          "properties": {
            "label":       { "type": "string" },
            "description": { "type": "string" }
          },
          "required": ["label"]
        },
        "minItems": 2
      }
    },
    "required": ["thread", "question", "options"]
  }
}
```

**FR-TMC-5.4 (`chat_ask` MCP tool — behavior):** When an agent calls `chat_ask`, the daemon MUST:
- Send a `sendMessage` to the Telegram `chat_id` resolved from `thread` with the `question` text and `reply_markup.inline_keyboard` containing one button per option, with `callback_data = "<question_id>:<option_index>"`.
- Correlate the eventual `callback_query` (FR-TMC-5.1) back to the pending `chat_ask` call and return the tapped option's index and label to the calling agent.
- The sync-vs-async correlation mechanism is an OPEN QUESTION deferred to the architect review at bootstrap Step 3 (see §19.10 Risks and Dependencies, risk #7). The deliverable contract is that the agent receives the answer; the internal wire is architect-determined.

**FR-TMC-5.5 (`chat_ask` honest scope — no native popup mirroring):** The `chat_ask` tool is an explicit agent-initiated call. The native harness `AskUserQuestion` popup CANNOT be automatically intercepted or mirrored to Telegram — there is no harness hook for that. The deliverable mechanism is that the agent (e.g., Mira in plan mode) explicitly calls `chat_ask` to render options as TG buttons when the operator is reachable on Telegram. Auto-mirroring of `AskUserQuestion` is out of scope for this feature.

**FR-TMC-5.6 (`chat_ask` in plugin whitelist):** `chat_ask` MUST be added to `TOOL_WHITELIST` in `src/plugin/mcp.rs` so CLI instances accessing the daemon over the thin-client plugin bridge can call `chat_ask`. Today the whitelist contains 9 tools (verified at `mcp.rs:56-71` this session); `chat_ask` becomes the 10th entry.

**FR-TMC-5.7 (single-select only):** The initial implementation MUST support single-select multiple-choice only — the operator taps exactly one button and the answer is returned. Free-text input and multi-select are out of scope for this feature.

#### Migration Flag and Conflict Gate (Slice 6)

**FR-TMC-6.1 (daemon poller gate):** The daemon Telegram bridge's `getUpdates` long-poller MUST check the `[telegram] enabled` flag in `daemon.toml` before starting. This flag already exists and defaults to `true` (`config.rs:100-123`). When `enabled = false`, the daemon MUST NOT start the long-poller, and the operator retains the per-CLI plugin path as the active Telegram receiver.

**FR-TMC-6.2 (conflict gate — 409 detection):** On the first `getUpdates` call (and on each subsequent call that returns HTTP 409), the daemon MUST log a clear, operator-readable error message — for example: "Telegram daemon poller received 409 Conflict: the legacy telegram-plugin-rs poller is still running. Stop it before enabling the daemon poller." The daemon MUST NOT crash; polling attempts MUST cease gracefully until the operator takes corrective action (e.g., daemon restart after stopping the plugin).

**FR-TMC-6.3 (no silent dual-poll):** The daemon MUST NEVER silently poll alongside the per-CLI plugin. Either the conflict gate surfaces the 409 loudly, or the operator has explicitly stopped the plugin. There is no scenario where both pollers hold the token concurrently without a logged error.

**FR-TMC-6.4 (revert path):** Setting `[telegram] enabled = false` in `daemon.toml` and restarting the daemon MUST restore the per-CLI plugin as the sole Telegram receiver, with no code changes required.

**FR-TMC-6.5 (installer channel-bridge wiring):** **(Amended 2026-05-31 — corrected cutover.)** The `install.sh` and `install.ps1` scripts MUST patch the official Anthropic Telegram plugin's `.mcp.json` so its `telegram` MCP server runs the claudebase **daemon bridge** (`claudebase plugin serve`, the GitHub-downloaded `~/.claude/tools/claudebase/claudebase` binary) — NOT the old direct-poll `server-rs` variant. The bridge only RELAYS the single daemon's channel notifications (it does not poll Telegram itself), so the daemon remains the sole `getUpdates` consumer (NFR-TMC-5 preserved) — there is no dual-poll. Launching `claude --channels plugin:telegram@claude-plugins-official` (the parameter `claudebase run` passes) then makes Claude Code inject the daemon's routed Telegram messages as `<channel>` turns into the live CLI session — this is the ONLY way real-time channel push reaches a live session (a plain `.mcp.json` mcpServers entry does NOT receive channel injection; only the approved `--channels` telegram slot does). The installer MUST back up the upstream `.mcp.json` to `.mcp.json.upstream-backup` and be idempotent. Documentation (README, RELEASING.md) MUST describe the `claudebase run` setup + the manual revert (restore the upstream backup). The earlier "stop auto-patching / server-rs" form of this requirement is superseded — `server-rs` (the per-CLI direct poller) is obsolete in the daemon architecture; the bridge replaces it.

#### Thin-Client Wiring Verification and Docs (Slice 7)

**FR-TMC-7.1 (CLI subscribes to chat-bound thread):** The thin-client bridge (`src/plugin/bridge.rs`) MUST be verified — via an end-to-end integration test — that a CLI subscribes to the `telegram:<chat_id>` thread via `ChatBus` and that inbound messages from the routing tree reach the CLI's message handler.

**FR-TMC-7.2 (Mira persona guidance):** The SDLC repo's Mira persona (`src/agents/` or equivalent) MUST be updated to include guidance that in plan-mode questionnaires, when the operator is reachable on a TG-bound chat, Mira SHOULD call `chat_ask` with the multiple-choice options rather than relying solely on the CLI popup. Mira MUST register with a stable `agent_name` so `/switch` can target the orchestrator by name.

**FR-TMC-7.3 (documentation):** `README.md` and `RELEASING.md` MUST gain a "Telegram Multi-CLI Setup" section documenting: (a) daemon cutover steps (stop the plugin poller, set `[telegram] enabled = true`, restart daemon), (b) `/switch` `/whoami` `/agents` `/here` command reference, (c) `chat_ask` tool usage guide for agent authors, (d) revert steps.

### 19.4 Non-Functional Requirements

1. **NFR-TMC-1 (chat-as-id isolation):** The routing tree MUST ensure that a message arriving in Telegram `chat_id` A is NEVER routed to the CLI bound under `chat_id` B, regardless of the order of evaluation or any concurrent `/switch` in progress. Binding updates use atomic `INSERT OR REPLACE` in SQLite to prevent partial-update races.

2. **NFR-TMC-2 (schema additive):** The v6 to v7 migration MUST be additive. No existing column or table in `chat.db` at v6 may be dropped or renamed. All existing `agent_registry`, `messages`, `chat_sessions`, and other v6 rows MUST survive the migration untouched.

3. **NFR-TMC-3 (conflict gate non-crashing):** The 409-Conflict detection MUST be handled gracefully — the daemon process MUST remain alive and responsive to UDS connections after a 409; only the Telegram poller loop is stopped. Other daemon capabilities (MCP dispatch, `ChatBus`, agent_registry, etc.) MUST remain unaffected.

4. **NFR-TMC-4 (`callback_data` size):** The `callback_data` field MUST NOT exceed 64 bytes. Question IDs MUST be compact (e.g., a base-62 or UUID-prefix representation), not full question text.

5. **NFR-TMC-5 (single `getUpdates` consumer):** After the cutover, only the daemon's long-poller holds the bot token's `getUpdates` slot. The installer (FR-TMC-6.5) and conflict gate (FR-TMC-6.2) together enforce this invariant.

6. **NFR-TMC-6 (no HTTP/WSS dependency):** All routing, bot commands, and `chat_ask` MUST operate entirely over the existing UDS socket (`src/daemon/server.rs`). No HTTP or WebSocket listener is added in this feature.

7. **NFR-TMC-7 (daemon auto-start prerequisite):** The feature presupposes that the daemon auto-starts via the service manager (launchd/systemd/SCM) installed in §17 Slice 2. The daemon MUST be running for routing to work. If the service manager registration is not confirmed active, the operator MUST be instructed to start the daemon manually before the cutover.

### 19.5 Acceptance Criteria

1. **AC-TMC-1 (schema v7 — fresh):** After the migration, `PRAGMA table_info(active_cli_per_chat)` on an upgraded `chat.db` returns columns `chat_id`, `active_cli_name`, `active_agent_id`, `set_at`, `set_by`. `PRAGMA table_info(tg_message_map)` returns columns `tg_msg_id`, `chat_id`, `sender_agent_id`, `sent_at`.

2. **AC-TMC-2 (schema v7 — additive):** On a v6 `chat.db` with existing rows, running the v7 migration leaves all pre-existing rows intact (row count unchanged across all pre-v7 tables).

3. **AC-TMC-3 (registry helpers):** Unit tests confirm `is_alive` returns `true` for a registered, non-orphaned agent and `false` for an unregistered or orphaned agent. `first_alive` returns the agent matching `prefer_role` when one exists and falls back to any alive agent otherwise; returns `None` when no alive agents exist.

4. **AC-TMC-4 (routing — chat isolation):** Two CLIs registered; `chat_id` 111 bound to CLI-1 via `/switch`, `chat_id` 222 bound to CLI-2. A free-text message on `chat_id` 111 reaches only CLI-1; a message on `chat_id` 222 reaches only CLI-2; no cross-chat routing occurs.

5. **AC-TMC-5 (routing — reply-quote):** CLI-1 sends an outbound message; the returned `message_id` is recorded in `tg_message_map`. When the operator sends a reply-to-message referencing that `message_id` in the same chat, the daemon routes the reply to CLI-1. Verified by inspecting `meta.target_agent_id` in the channel notification.

6. **AC-TMC-6 (routing — reply-quote dead CLI fallback):** CLI-1 sends a message and is then unregistered. A subsequent reply-quote to CLI-1's message falls through to the active binding (CLI-2 if bound, or `first_alive` result). A diagnostic log entry is produced.

7. **AC-TMC-7 (routing — no alive CLI):** When no CLIs are registered (or all are dead), a free-text message on any `chat_id` causes the daemon to reply "No CLIs online. Spawn one with `claudebase run`." and route nothing to any agent.

8. **AC-TMC-8 (/agents command):** `/agents` sent in Telegram returns a formatted list naming all alive CLI instances; if none are alive, returns "No CLIs currently online."

9. **AC-TMC-9 (/switch command):** `/switch <name>` with a valid alive CLI name writes a binding to `active_cli_per_chat` and confirms in the Telegram reply. `/switch <deadname>` is rejected with an error listing available names.

10. **AC-TMC-10 (/whoami command):** `/whoami` returns the bound CLI name and agent ID for the sending `chat_id`, or names the default when no explicit binding exists.

11. **AC-TMC-11 (/here command):** `/here` returns the bound CLI's `host` and `cwd` from `agent_registry` metadata, or a clear "information unavailable" message.

12. **AC-TMC-12 (outbound tracking):** After CLI-1 sends a Telegram message via `chat_reply`, `sqlite3 chat.db "SELECT sender_agent_id FROM tg_message_map WHERE chat_id = <id>"` returns CLI-1's `agent_id`.

13. **AC-TMC-13 (TTL purge):** Rows inserted with `sent_at` older than 30 days are absent from `tg_message_map` after a purge run. Rows inserted within 30 days are present.

14. **AC-TMC-14 (`chat_ask` — buttons rendered):** An agent calls `chat_ask("telegram:111", "Pick one", [{label:"A"}, {label:"B"}, {label:"C"}])`; the daemon sends a Telegram message to `chat_id` 111 with an `inline_keyboard` containing 3 buttons labelled A, B, C; verified by inspecting the outbound `sendMessage` payload.

15. **AC-TMC-15 (`chat_ask` — answer routed):** Tapping button B on the inline keyboard triggers a `callback_query` with `callback_data = "<qid>:1"` (index 1 = B); the daemon calls `answerCallbackQuery` to dismiss the spinner; the answer (index=1, label="B") is delivered to the calling agent on the chat-bound CLI.

16. **AC-TMC-16 (`callback_data` size):** `callback_data` for a `chat_ask` with 3 options is no greater than 64 bytes. Verified by inspecting the serialized string length in a unit test.

17. **AC-TMC-17 (conflict gate — 409):** With the per-CLI plugin running (holding `getUpdates`), starting the daemon with `[telegram] enabled = true` causes the daemon to log a clear conflict message containing "409" and the phrase "legacy telegram-plugin-rs poller still running", and NOT crash.

18. **AC-TMC-18 (conflict gate — clean takeover):** After the per-CLI plugin is stopped, restarting the daemon causes it to successfully poll `getUpdates` and route messages.

19. **AC-TMC-19 (revert path):** Setting `[telegram] enabled = false` and restarting the daemon causes the per-CLI plugin to resume receiving messages (verified by a message from Telegram arriving as `source="plugin:telegram:telegram"` rather than `source="claudebase"`).

20. **AC-TMC-20 (plugin whitelist):** `TOOL_WHITELIST` in `src/plugin/mcp.rs` contains `"chat_ask"`.

21. **AC-TMC-21 (bot commands don't leak to CLIs):** A `/switch` command sent in Telegram produces a daemon-side reply and does NOT produce a channel notification routed to any CLI.

### 19.6 Affected CLI and MCP Surface

**New MCP tool (added to daemon `tools/list` dispatch at `src/daemon/server.rs:632-727` and plugin `TOOL_WHITELIST` at `src/plugin/mcp.rs:56`):**
- `chat_ask(thread, question, options)` — renders multiple-choice options as Telegram inline keyboard buttons; returns the tapped option's index and label.

**Modified daemon Telegram path (`src/daemon/telegram.rs`):**
- `process_batch`: extended with 5-step routing tree (FR-TMC-2.1) and `callback_query` handling (FR-TMC-5.1).
- `enqueue_outbound_tg` (`:81`): extended to accept optional `reply_markup` and to record the returned `message_id` into `tg_message_map` (FR-TMC-4.1).
- Bot command handlers: `/agents`, `/switch`, `/whoami`, `/here` added; `/start`, `/help`, `/status` preserved.

**No changes to the public `claudebase` CLI binary interface** — no new subcommands; `chat_ask` is an MCP tool exposed through the daemon UDS socket.

### 19.7 Schema Changes

**`chat.db` — Schema v7 (additive on top of §17 v6):**

The `ensure_chat_db_schema` function at `src/daemon/chat.rs:265` applies the following additions. Both tables are created with `IF NOT EXISTS` for idempotency.

```sql
-- Chat-to-CLI binding (chat-as-id: one chat_id = one CLI)
CREATE TABLE IF NOT EXISTS active_cli_per_chat (
    chat_id         INTEGER PRIMARY KEY,
    active_cli_name TEXT NOT NULL,
    active_agent_id TEXT NOT NULL,
    set_at          INTEGER NOT NULL,  -- Unix seconds
    set_by          TEXT NOT NULL      -- Telegram user_id or username of /switch initiator
);

-- Reply-quote routing map (30-day TTL purge)
CREATE TABLE IF NOT EXISTS tg_message_map (
    tg_msg_id       INTEGER NOT NULL,
    chat_id         INTEGER NOT NULL,
    sender_agent_id TEXT NOT NULL,
    sent_at         INTEGER NOT NULL,  -- Unix seconds
    PRIMARY KEY (chat_id, tg_msg_id)
);
```

No columns are dropped, renamed, or altered on existing v6 tables. The `index.db` (books corpus) and `insights.db` (agent insights) files are unaffected.

### 19.8 UI / Bot-Surface Changes

The Telegram bot gains the following user-visible surface:

**New bot commands:**
- `/agents` — list alive CLI instances by name.
- `/switch <name>` — bind this Telegram chat to the named CLI; rebinds for the whole chat in group chats.
- `/whoami` — show the chat's current CLI binding.
- `/here` — show the bound CLI's host and working directory.

**Updated `/help` text:** MUST document the four new commands and note that `/switch` in a group chat rebinds for all participants.

**Inline keyboard buttons:** Agent-initiated `chat_ask` calls render multiple-choice options as tappable buttons in the Telegram conversation. Tapping a button dismisses the spinner (via `answerCallbackQuery`) and delivers the answer to the bound CLI.

**Outbound source label unchanged:** Daemon-proxied messages continue to carry `source="claudebase"` (distinct from the plugin's `"plugin:telegram:telegram"`) so the operator can identify the active poller.

### 19.9 Out of Scope

The following items are explicitly excluded from this feature:

1. **HTTP/WSS `claudebase-server-foundation` + cross-machine fleet.** Routing over UDS on a single machine is sufficient; cross-machine routing is a separate future feature.
2. **`.claudebase/identity.local` per-project CLI identity.** Routing works off `agent_registry` + `active_cli_per_chat` without a project-level identity file.
3. **Auto-mirroring of the native `AskUserQuestion` popup.** There is no harness hook to intercept the popup. The deliverable is the explicit `chat_ask` MCP tool; agents call it intentionally.
4. **Multi-select and free-text question types.** Single-select inline keyboard buttons only in this feature.
5. **Per-user routing within a group chat.** The routing key is `chat_id` alone; all participants in a group share one CLI binding.
6. **Anonymous or unauthenticated bot access.** Existing `access.json` allowlist and pairing flow (§17) govern who can interact with the bot; no changes to the authorization model in this feature.

### 19.10 Risks and Dependencies

1. **`AskUserQuestion` is not auto-interceptable.** The buttons-in-Telegram UX is delivered by the agent explicitly calling `chat_ask`, not by any automatic CLI popup interception. If the operator expects automatic mirroring of every CLI popup, that expectation must be corrected. Risk: medium (operator expectation gap); mitigated by FR-TMC-5.5 and Mira persona guidance in FR-TMC-7.2.

2. **`callback_query` is a different update type than `message`.** Today `process_batch` only handles `message` updates (confirmed: no `callback_query` symbols in `telegram.rs` this session). Missing handling means inline keyboard buttons silently do nothing. Slice 5 adds it explicitly. Risk: high if Slice 5 is omitted; mitigated by Slice 5 being a required FR.

3. **`callback_data` 64-byte Telegram limit.** The `question_id` must be compact. FR-TMC-5.2 mandates compact IDs; AC-TMC-16 verifies no greater than 64 bytes. Risk: low with discipline enforced.

4. **Single `getUpdates` consumer rule.** The cutover MUST stop the plugin poller or the daemon receives 409. The conflict gate (FR-TMC-6.2) makes this loud. The installer change (FR-TMC-6.5) prevents auto-resurrection. Risk: high if either is omitted; mitigated by both being required FRs.

5. **Daemon must be running and auto-started.** The daemon was not running at the start of this session (verified: `pgrep` empty). Service-install shipped in §17 Slice 2; the operator MUST confirm the service is active before the cutover. Risk: medium; mitigated by FR-TMC-7.1 end-to-end test requiring the daemon.

6. **chat-as-id group-chat social surprise.** Any group participant's `/switch` rebinds the shared chat for everyone. This is intentional (operator decision 2026-05-30). FR-TMC-3.2 and the `/help` update (§19.8) make this visible. Risk: low (documented).

7. **sync vs. async `chat_ask` correlation (open decision).** Sync (blocking tool call until button tap) is simpler for the agent but fragile for slow operators. Async (tool returns `question_id`; answer arrives later as a channel notification) is more robust. This decision shapes the `chat_ask` server-side implementation in `server.rs`. Risk: medium; architect decision required at bootstrap Step 3 before Slice 5 implementation begins.

8. **`active_cli_per_chat` as SQLite table vs. JSON file.** The plan selects SQLite for consistency with `chat.db` + transactional updates. Architect review at bootstrap confirms. Risk: low; SQLite is the default (FR-TMC-1.1); JSON is the fallback if architect requires it.

9. **teloxide v0.17 inline keyboard API.** The `InlineKeyboardMarkup`, `InlineKeyboardButton::callback`, and `CallbackQuery` symbols are present in teloxide's published API but have NOT been verified against the exact version pinned in `Cargo.toml` in this session. The implementer MUST verify symbol availability before coding Slice 5. Risk: medium; mitigation is pre-Slice-5 verification of the Cargo.lock pinned version.

## Facts

### Verified facts

- `agent_registry.rs` at `src/daemon/agent_registry.rs` exposes these `pub fn` symbols: `validate_agent_name` (:102), `register` (:124), `unregister` (:173), `list_alive` (:200), `reap` (:232), `mark_connection_orphaned` (:255), `reap_on_boot` (:268). No `is_alive` or `first_alive` exist — verified by `grep "pub fn"` this session. — salience: high
- `chat.db` is at schema v5+v6; the file-level comment at `src/daemon/chat.rs:1` reads "schema v5, message persistence, broadcast bus"; `chat.rs:254` reads "Apply schema v5 + v6 to a chat.db connection". Neither `active_cli_per_chat` nor `tg_message_map` are present — confirmed by grep returning no output this session. — salience: high
- `TOOL_WHITELIST` in `src/plugin/mcp.rs:56-71` contains exactly 9 tools: `chat_post`, `chat_subscribe`, `chat_reply`, `chat_list`, `chat_list_threads`, `claudebase_daemon_status`, `agent_register`, `agent_unregister`, `agent_list_alive`, `agent_reap`. `chat_ask` is absent — verified by Read this session. — salience: high
- No `callback_query`, `inline_keyboard`, `InlineKeyboardMarkup`, `answerCallbackQuery`, or `answer_callback` symbols exist in `src/daemon/telegram.rs` — confirmed by grep returning no output this session. — salience: high
- The daemon Telegram bridge defaults `[telegram] enabled = true` at `config.rs:100-123` — verified from plan.md `## Facts` block, which sourced this from a direct file read in its own session. — salience: high
- `enqueue_outbound_tg` is at `telegram.rs:81`; `extract_first_mention` routing precursor is at `telegram.rs:162`; `meta.target_agent_id` tagging is at `telegram.rs:333` — source: plan.md `## Facts`, read this session. — salience: high
- `ChatBus` publish/subscribe is at `src/daemon/chat.rs:78-121`; MCP dispatch is at `src/daemon/server.rs:632-727` — source: plan.md `## Facts`, read this session. — salience: high
- Operator decision 2026-05-30: routing key is `chat_id` alone (not `(user_id, chat_id)`); 1 chat = 1 CLI; `/switch` rebinds the whole chat — source: insight doc#30 (`sha=1a7e734c`, `agent=mira`, `type=operator-correction`), retrieved this session via `claudebase insight get 30 --json`. — salience: high
- Books corpus `doc_count = 0` (empty). Corpus scope verdict: **No overlap** — empty corpus; topical queries skipped per corpus-scope-relevance protocol. — salience: low
- PRD has sections §1 through §18; §19 is the next section — confirmed by `grep "^## §"` output this session showing last section is §18 at line 785. — salience: medium
- insights-base: doc#30 sha=1a7e734c agent=mira type=operator-correction — query: "chat-as-id keying operator-correction" — verified: yes — salience: high

### External contracts

- **Telegram Bot API — `getUpdates`** — symbol: single-consumer-per-token rule; a second concurrent caller receives HTTP 409 Conflict — source: Telegram Bot API docs (NOT opened this session) — verified: no — assumption. The implementer MUST verify the exact HTTP status code and response body before coding the conflict gate. — salience: high
- **Telegram Bot API — `sendMessage` with `reply_markup.inline_keyboard`** — symbol: `reply_markup` field; `inline_keyboard` is an array of arrays of `InlineKeyboardButton` objects, each with `text` (string) and `callback_data` (string, max 64 bytes) — source: Telegram Bot API docs (NOT opened this session) — verified: no — assumption. Implementer MUST verify shape before Slice 5. — salience: high
- **Telegram Bot API — `callback_query` update** — symbol: `callback_query` top-level field in an Update object; contains `id` (for `answerCallbackQuery`), `data` (the `callback_data` string), and `message.chat.id` (the originating chat) — source: Telegram Bot API docs (NOT opened this session) — verified: no — assumption. — salience: high
- **Telegram Bot API — `answerCallbackQuery`** — symbol: POST method, required parameter `callback_query_id` (string); dismisses the loading spinner on the Telegram client — source: Telegram Bot API docs (NOT opened this session) — verified: no — assumption. — salience: high
- **Telegram Bot API — `callback_data` max 64 bytes** — symbol: hard limit enforced server-side by Telegram; exceeding it causes the `sendMessage` call to fail — source: Telegram Bot API docs (NOT opened this session) — verified: no — assumption. — salience: high
- **teloxide v0.17** — symbols: `Bot::get_updates` (confirmed in-use); `InlineKeyboardMarkup`, `InlineKeyboardButton::callback`, `CallbackQuery`, `answer_callback_query` (NOT verified against pinned Cargo.lock version this session) — verified: yes for `get_updates`; no — assumption for the inline-keyboard/callback symbols. Implementer MUST confirm symbol availability at the pinned version before Slice 5. — salience: high
- **MCP `tools/list` / `tools/call` dispatch** — symbol: daemon dispatch at `src/daemon/server.rs:632-727`; plugin `TOOL_WHITELIST` at `src/plugin/mcp.rs:56-71` — adding `chat_ask` extends both — verified: yes (Read this session). — salience: medium

### Assumptions

- `active_cli_per_chat` as a SQLite table (not a JSON file) is the correct choice — consistent with `chat.db` and enables atomic `INSERT OR REPLACE`. Risk: architect may prefer JSON. How to verify: architect review at bootstrap Step 3. Pending confirmation. — salience: medium
- The daemon service-install (launchd/systemd/SCM) delivered in §17 Slice 2 is wired and auto-starts the daemon at boot. Risk: if not active, the feature does not work without manual daemon startup. How to verify: `claudebase daemon status` or `pgrep` before cutover. — salience: high
- `chat_ask` sync-vs-async correlation is an open architect decision; the PRD captures the tool interface (FR-TMC-5.3) and behavior contract (FR-TMC-5.4) without prescribing the wire mechanism. Risk: architect may determine async-via-notification requires significant server.rs changes. How to verify: architect review at bootstrap Step 3. — salience: high
- The operator wants single-select multiple-choice questionnaires as the initial deliverable (matches the `AskUserQuestion` shape). Risk: operator may request free-text or multi-select after seeing the buttons. How to verify: confirmed by plan Out of Scope block (plan.md read this session). — salience: low

### Open questions

- **Sync vs. async `chat_ask` answer correlation** — blocking tool call vs. async `question_id` + later channel notification. Needs: architect decision at bootstrap Step 3 before Slice 5 is implemented. — salience: high
- **`[telegram] enabled` cutover default** — remains `true` (daemon-on by default) or flips to `false` (opt-in migration) for the release? Needs: operator/architect decision. — salience: medium
- **Mira persona update: claudebase repo or SDLC repo?** The `chat_ask` usage guidance and stable `agent_name` registration live in the SDLC `src/agents/` persona, not the claudebase binary. Slice 7 is cross-repo. Needs: planner to confirm cross-repo branch strategy. — salience: medium
- knowledge-base: corpus is empty (doc_count=0); task domain is Telegram Bot API + Rust daemon engineering + SQLite schema migration; no overlap. Topical queries skipped per corpus-scope-relevance protocol. — salience: low

## Decisions

### Inbound validation

- Task received: append §19 PRD section to `docs/PRD.md` based on the approved plan at `.claude/plan.md`. Plan read in full (198 lines). The plan explicitly names the "chat-as-id" operator decision (2026-05-30, OQ-3), verified independently via insights-corpus doc#30 (`operator-correction`, `mira`, sha=1a7e734c). No contradictions detected. Challenged: yes — verified operator-decision evidence from two independent sources (plan.md + insight doc#30) before authoring FR-TMC-2.1. Outcome: proceeded with chat-as-id routing as stated. — salience: high
- The plan's Slice 5 frames `chat_ask` as having an honest scope boundary (no native AskUserQuestion mirroring). Protocol-1 check: correct — the harness provides no hook for intercepting the native popup, confirmed by plan.md:101 and absence of contrary harness documentation. Challenged: yes. Outcome: FR-TMC-5.5 captures this constraint explicitly. — salience: high

### Decisions made

- **Chat-as-id routing (`chat_id` alone as the CLI binding key).** Q1 hack? no — operator-decided. Q2 sane? yes — simpler than `(user_id, chat_id)` and matches the operator's stated mental model. Q3 alternatives? per-user routing — rejected by operator on 2026-05-30 (OQ-3, insight doc#30). Q4 cause? yes. Q5 tracked? group-chat social consequence documented in FR-TMC-3.2, §19.9 #5, and §19.10 #6. — salience: high
- **5-step routing tree omits step 3 (per-user "last addressed" state).** Q1 hack? no — structurally inapplicable under chat-as-id. Q2 sane? yes. Q3 alternatives? retain as dead no-op (rejected: confusing dead code). Q4 cause. Q5 n/a. — salience: medium
- **`chat_ask` as an explicit MCP tool.** Q1 hack? no — only feasible mechanism. Q2 sane? yes. Q3 alternatives? harness interception (rejected: no hook exists). Q4 cause. Q5 n/a. — salience: high
- **SQLite for `active_cli_per_chat` and `tg_message_map`** (architect confirmation pending). Q1 hack? no — consistent with all other `chat.db` tables; transactional. Q2 sane? yes. Q3 alternatives? JSON file (rejected: less transactional, consistency risks). Q4 cause. Q5 pending architect confirmation surfaced in Assumptions. — salience: medium
- **Defer sync-vs-async `chat_ask` correlation to architect.** Q1 hack? no — both are valid with different trade-offs. Q2 sane? yes. Q3 alternatives? default to sync (rejected: fragile for slow human taps). Q4 cause. Q5 tracked in Open questions. — salience: high

### Hacks acknowledged

(none) — PRD authoring only; no implementation hacks introduced.

### Symptom-only patches (with root-cause links)

(none) — no symptom-only patches in this PRD section.
