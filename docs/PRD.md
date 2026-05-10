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

