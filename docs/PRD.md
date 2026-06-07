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

## §18. Multi-Agent Telegram Routing on v0.6 Foundation

**Status:** [PLANNED]
**Date:** 2026-06-02
**Priority:** High
**Related:** §17 (Agent Chat Daemon + Telegram Bridge — this feature is the additive, topic-aware routing layer on top of the v0.6 baseline shipped under §17; the daemon UDS framing, `chat.db` location, `agent_registry` table and the `notifications/claude/channel` wire shape from §17 are all preserved here). §16 (Insights Base — the `chat.db` sibling-file pattern continues). Plan source: `.claude/plan.md` (v2, 321 lines, read in full this session) — same body persisted at `docs/plans/multi-agent-telegram-on-v0.6.md` (untracked, 320 lines).

Changelog: Adds topic-aware Telegram routing so one bot serves three Claude Code CLI instances bound to DM + forum topic α + forum topic β in the same group, rebuilt from the `claudebase-v0.6.0` tag after v0.7/v0.8 multi-CLI work was rolled back.

### 18.1 Feature Description

The v0.7 (2026-05-30) and v0.8 (2026-05-31) attempts at multi-CLI Telegram routing were declared empirically broken by the operator after extended debugging — root cause was not isolated and fix-forward cost exceeded rebuild cost. This feature **rolls the Telegram subsystem back to the `claudebase-v0.6.0` tag** and **additively extends** the daemon so a single Telegram bot can route inbound messages to three different Claude Code CLI instances based on a routing key `(chat_id, Option<message_thread_id>)`: one CLI bound to a DM, one bound to forum topic α inside a group, one bound to forum topic β inside the same group. (Plan §"Context — why", lines 9–22; plan §"Architecture", lines 17–21.)

The architecture is the operator-decided C1/C2/C3 triple (plan lines 17–21): **C1** — the mixed library stack (teloxide 0.17 in the daemon, frankenstein 0.49 in the plugin) is kept; **C2** — the daemon owns the Telegram connection via teloxide's high-level `Dispatcher` API and the plugin's frankenstein polling loop is disabled, making the daemon the sole `getUpdates` consumer and resolving v0.6's latent dual-poller 409 vulnerability; **C3** — the `reply` MCP tool gains one OPTIONAL `message_thread_id: Option<String>` param and the inbound `notifications/claude/channel` meta gains one OPTIONAL `thread_id: Option<String>` field. Both are additive per JSON-RPC convention so old v0.6 consumers ignore unknown fields and remain backward-compatible.

The CLI-observable surface — MCP tool names, notification method names, IPC framing, installer behavior, `claudebase run` argv — is frozen bit-for-bit to v0.6 except for the two optional-additive fields per C3. (Plan §"Architectural Constraints → Frozen", lines 78–91.)

### 18.2 User Story

As the SDLC pipeline operator, I want to run three independent Claude Code CLI sessions on the same machine, each bound to a different conversation surface in the same Telegram bot (one DM and two forum topics in a group), so that I can hold three parallel multi-agent conversations through a single bot token without per-CLI bots and without colliding routing keys.

### 18.3 Functional Requirements

#### FR-MAT-1: Routing Key Shape — `(chat_id, Option<message_thread_id>)` (Slice 3, plan lines 126–137)

1. **FR-MAT-1.1:** The daemon's TG routing key MUST be the tuple `(chat_id: i64, message_thread_id: Option<i64>)`. Direct-message inbound MUST resolve to `(dm_chat_id, None)`. Forum-topic inbound MUST resolve to `(group_chat_id, Some(thread_id))`. The same bot token MUST serve all three routing keys in KP1, KP2 and KP3 (plan §"Success Criteria", lines 37–48).
2. **FR-MAT-1.2:** When extracting the routing key from each teloxide `Update::Message`, the daemon MUST read `Message.message_thread_id` directly. If the field is absent on the inbound `Message` (DM case), the routing key MUST be `(chat_id, None)`. (Plan §"Open for modification", lines 95–98; OQ — Slice 1 architect verification of teloxide-0.17 `message_thread_id` field-existence per R4.)
3. **FR-MAT-1.3:** Outbound replies MUST translate the optional `message_thread_id` from the CLI's `reply` call into teloxide's `SendMessageSetters::message_thread_id` so the reply lands in the correct topic. When the CLI omits the field (legacy reply), the reply lands in the main chat / DM (plan §"Architecture", line 21).

#### FR-MAT-2: `agent_registry` Schema — Additive Migration (Slice 2, plan lines 119–125)

1. **FR-MAT-2.1:** The v0.6 `agent_registry` table (existing columns `agent_id, agent_name, connection_id, chat_thread_id, state, spawned_at, last_pinged_at` per §17.7) MUST be extended with two new nullable columns: `routing_chat_id INTEGER` and `routing_thread_id INTEGER NULL`. The migration MUST use `ALTER TABLE ... ADD COLUMN ... DEFAULT NULL` so existing rows survive untouched (plan Slice 2 done-when, line 124).
2. **FR-MAT-2.2:** A new partial-unique index `agent_registry_routing_alive_uniq_idx ON agent_registry(routing_chat_id, routing_thread_id) WHERE state='alive'` MUST be created. This index enforces the at-most-one-alive-CLI-per-routing-key invariant required for deterministic routing in KP1–KP3 (plan §"Open for modification → agent_registry", line 96).
3. **FR-MAT-2.3:** A new daemon-internal API `register_routing(cli_id: AgentId, chat_id: i64, thread_id: Option<i64>) -> Result<()>` MUST be exposed on `src/daemon/agent_registry.rs`. Calling it twice with the same `(chat_id, thread_id)` while both rows are `state='alive'` MUST fail with a UNIQUE-constraint violation surfaced as a clear application error (plan Slice 2 done-when subpoints a-b, line 124).
4. **FR-MAT-2.4:** The migration MUST be idempotent — re-running on an already-migrated `chat.db` MUST be a no-op (exit 0) — AND MUST tolerate pre-existing v0.7/v0.8 leftover columns on dev boxes by leaving them in place rather than dropping them (plan Slice 2 migration discipline, lines 122–123; plan R7, line 209).

#### FR-MAT-3: Daemon Owns the Telegram Connection via teloxide `Dispatcher` (Slice 3, plan lines 126–137)

1. **FR-MAT-3.1:** `src/daemon/telegram.rs` MUST own the Telegram connection via teloxide 0.17's high-level `Dispatcher::builder` API wired to a `handler_tree!` routing every inbound `Message`. The daemon MUST NOT contain a hand-rolled `loop { get_updates }` long-poll — the library handles long-polling internally and dispatches events as callbacks (plan §"Architecture" C2, line 20; plan Slice 3, line 129).
2. **FR-MAT-3.2:** For every inbound `Message`, the Dispatcher handler MUST extract the routing key per FR-MAT-1.2, look up the bound CLI in `agent_registry` via the partial-unique index from FR-MAT-2.2, and (on match) emit a `notifications/claude/channel` event with the optional `thread_id` meta field (per FR-MAT-7) scoped to that CLI's subscription. On no-match the orphan-inbound fallback policy per FR-MAT-9.5 applies (plan Slice 3 done-when, line 137).
3. **FR-MAT-3.3:** The daemon MUST be the sole `getUpdates` consumer for the configured `TELEGRAM_BOT_TOKEN`. The plugin's frankenstein polling loop MUST be disabled per FR-MAT-4 so v0.6's latent dual-poller 409 vulnerability cannot fire (plan R6, line 208).

#### FR-MAT-4: Plugin Is a Thin MCP Bridge — Frankenstein Polling Disabled (Slice 4, plan lines 139–144)

1. **FR-MAT-4.1:** `plugins/telegram-rs/src/telegram/bot.rs`'s `getUpdates` long-poll loop MUST be disabled by default. The loop MAY remain compiled-in behind `cfg(feature = "legacy-direct-poll")` as an emergency revert escape-hatch but MUST NOT run when the daemon-ownership build is active (plan §"Open for modification", line 99; plan R5, line 207; Decisions → Hacks acknowledged below).
2. **FR-MAT-4.2:** The plugin MUST continue to host the access-gate and pairing UI from v0.6 unchanged (plan §"Open for modification → bot.rs", line 99). The §17 pairing model and the v0.6 `permissions.rs` + `channel_state.rs` modules MUST both be preserved as-is — Slice 6 does NOT migrate or consolidate them (plan Slice 6, lines 153–155).
3. **FR-MAT-4.3:** The frankenstein 0.49 crate pin in `plugins/telegram-rs/Cargo.toml` MUST be preserved — no migration of the plugin to teloxide and no migration of the daemon to frankenstein (operator decision C1, plan line 19).

#### FR-MAT-5: Plugin's Outbound Tools Forward to Daemon over UDS (Slice 4, plan lines 139–144)

1. **FR-MAT-5.1:** The plugin's four outbound MCP tools (`reply, react, edit_message, download_attachment`) MUST forward to the daemon over the existing v0.6 UDS framing (length-prefixed 4-byte big-endian header + UTF-8 JSON body) using a daemon-internal envelope method name (e.g. `internal.send_message`). The daemon-internal envelope is NOT a new CLI-facing MCP method — it is part of the plugin↔daemon UDS dispatch table only (plan §"Open for modification → handle_reply/handle_react/handle_edit_message/handle_download_attachment", line 100).
2. **FR-MAT-5.2:** The CLI-facing tool names and required-parameter shapes of `reply, react, edit_message, download_attachment` MUST be unchanged from v0.6. The ONLY contract delta is the additive optional `message_thread_id` param on `reply` per FR-MAT-6 (plan §"Architectural Constraints → Frozen", points 2 and 4, lines 82–87).

#### FR-MAT-6: `reply` Gains Optional `message_thread_id` Param (Slice 4, plan lines 143–144)

1. **FR-MAT-6.1:** The CLI-facing `reply` MCP tool params MUST gain a NEW OPTIONAL field `message_thread_id: Option<String>` (string per the v0.6 ID-as-string discipline). Old CLI clients omitting the field MUST continue to work — their replies land in the main chat / DM (plan §"Architecture" C3, line 21; plan §"Architectural Constraints → Frozen", point 4, lines 84–87).
2. **FR-MAT-6.2:** When the plugin forwards the `reply` tool call to the daemon over the UDS envelope (per FR-MAT-5.1), the plugin MUST propagate the optional `message_thread_id` into the envelope unchanged. The daemon MUST parse the field and translate it to teloxide's `SendMessageSetters::message_thread_id` on the outbound `sendMessage` call (plan Slice 4 done-when, line 144).
3. **FR-MAT-6.3:** When the `message_thread_id` field is absent on the inbound CLI tool call, the daemon MUST NOT call `SendMessageSetters::message_thread_id`. The outbound reply MUST land in the main chat / DM — preserving the v0.6 reply-without-thread behavior (plan §"Architecture" C3, line 21).

#### FR-MAT-7: `notifications/claude/channel` Meta Gains Optional `thread_id` Field (Slice 3, plan line 130)

1. **FR-MAT-7.1:** The inbound `notifications/claude/channel` meta JSON object MUST gain a NEW OPTIONAL field `thread_id: Option<String>` (string-typed for symmetry with FR-MAT-6.1). The field MUST be present when the inbound message originated from a forum topic and MUST be absent (or `null`) when the inbound originated from a DM (plan §"Architecture" C3, line 21; plan §"Architectural Constraints → Frozen", point 4, lines 84–87).
2. **FR-MAT-7.2:** No existing field on the `notifications/claude/channel` meta MUST be renamed, retyped, or removed. The notification method name `notifications/claude/channel` MUST remain bit-for-bit identical to v0.6 (plan §"Architectural Constraints → Frozen", point 1, line 81). The contract delta is purely additive per JSON-RPC backward-evolution convention.
3. **FR-MAT-7.3:** v0.6 CLI clients (Claude Code with a v0.6 plugin) MUST continue to operate against a daemon emitting the `thread_id` field — unknown fields MUST be ignored per JSON-RPC convention (plan §"Architectural Constraints → Frozen", point 4 closing line, line 87).

#### FR-MAT-8: Bot Commands `/agents`, `/switch`, `/whoami`, `/here` — Topic-Aware (Slice 5, plan lines 146–151)

1. **FR-MAT-8.1:** The daemon's Dispatcher handler tree MUST dispatch the four bot commands `/agents`, `/switch <cli>`, `/whoami`, `/here` BEFORE the routing-key lookup runs. The commands MUST be handled server-side in the daemon — they are NOT CLI-facing MCP tools (plan Slice 5, line 147).
2. **FR-MAT-8.2:** `/agents` MUST be topic-aware: when issued from forum topic α the command MUST list CLIs whose binding is `(chat_id, Some(α))`; when issued from forum topic β the command MUST list CLIs bound to `(chat_id, Some(β))`; when issued in a DM the command MUST list CLIs bound to `(chat_id, None)`. The Dispatcher handler MUST read `message.message_thread_id` from the COMMAND message itself when filtering (plan R8, line 210).
3. **FR-MAT-8.3:** `/switch <cli>` MUST rebind the routing-key row in `agent_registry` to the named CLI. Two concurrent `/switch` taps targeting the same routing key MUST be serialized by SQLite's per-write transaction; the partial-unique index from FR-MAT-2.2 ensures only one binding persists. Default conflict-resolution policy: second tap wins (overwrites) with a TG confirmation reply (plan Slice 5 race-condition note, line 149).
4. **FR-MAT-8.4:** `/whoami` MUST return the caller's current `(routing_chat_id, routing_thread_id)` binding and the bound CLI's `agent_id` / `agent_name` (plan Slice 5, line 148).
5. **FR-MAT-8.5:** `/here` MUST return the bound CLI's host / cwd / process metadata from the registry (plan Slice 5, line 148).
6. **FR-MAT-8.6 (Group-`/switch` security — OQ3 default policy, refined by red-team at bootstrap):** In a group, only the user whose `user_id` matches the existing binding's last user_id, OR a chat admin, MAY invoke `/switch`. Unauthorized callers MUST receive a TG denial reply. This is the default; the red-team agent MAY refine at bootstrap (plan OQ3, line 225; plan R3, line 205).

#### FR-MAT-9: Error Handling — teloxide / Token / Orphan Inbound (Slice 3, plan lines 131–136)

1. **FR-MAT-9.1:** When `TELEGRAM_BOT_TOKEN` is unset at daemon startup, the daemon MUST start WITHOUT Telegram support, log a WARN message and continue serving non-TG subsystems (chat, registry, plugin bridge). The daemon MUST NOT crash (plan Slice 3 error handling subpoint 1, line 132).
2. **FR-MAT-9.2:** When teloxide returns HTTP 401 (invalid token) on bootstrap or mid-stream, the daemon MUST log an ERROR, gracefully terminate the Dispatcher task, and keep the rest of the daemon alive (plan Slice 3 error handling subpoint 2, line 133).
3. **FR-MAT-9.3:** When teloxide returns HTTP 409 (another consumer holds the token), the daemon MUST log a WARN with an operator-facing instruction («kill any other claudebase daemon / TG plugin holding this token; the daemon is the sole owner in this build») and apply exponential backoff before retrying (plan Slice 3 error handling subpoint 3, line 134).
4. **FR-MAT-9.4:** When an inbound message arrives without `message_thread_id` (DM case), the routing key MUST resolve to `(chat_id, None)` and routing MUST proceed normally (plan Slice 3 error handling subpoint 4, line 135).
5. **FR-MAT-9.5 (Orphan inbound):** When an inbound message routing-key has no `state='alive'` binding in `agent_registry`, the daemon MUST log the orphan event AND apply the fallback policy decided at bootstrap (plan OQ3-related; plan Slice 3 error handling subpoint 5, line 136). Default plan: log + emit a TG reply «no CLI bound to this routing key» — architect refines the precise fallback at bootstrap (plan §"Verification" step 4, line 235).

#### FR-MAT-10: Daemon-Restart Resilience (Slice 7, plan lines 157–162)

1. **FR-MAT-10.1:** All `agent_registry` rows including the new `routing_chat_id` / `routing_thread_id` columns MUST persist in `chat.db` across daemon restart — using the existing v0.6 SQLite persistence layer (plan Slice 7, line 158).
2. **FR-MAT-10.2:** On daemon restart, all `state='alive'` bindings MUST be restored to in-memory routing state without operator intervention. A subsequent inbound message MUST route to the correct CLI per the persisted binding (plan Slice 7 done-when, line 162).
3. **FR-MAT-10.3:** A CLI that started BEFORE the daemon came up MUST continue to operate against the v0.6 `claudebase_daemon_status: { status: "down" }` sentinel and MUST automatically reconnect via the v0.6 `notifications/tools/list_changed` mechanism once the daemon is up. NO v0.8 session-cache, NO reconnect-replay, NO `ensure_daemon_running` is introduced (plan Slice 7 explicit-out, line 161; plan §"Open for modification → bridge.rs", line 101).

### 18.4 Non-Functional Requirements

1. **NFR-MAT-1 (Mixed library stack preserved):** The daemon MUST keep teloxide 0.17 as the TG client (`src/daemon/telegram.rs`) and the plugin MUST keep frankenstein 0.49 (`plugins/telegram-rs/`). No migration in either direction (operator decision C1, plan line 19; plan §"Architectural Constraints → Frozen", point 8, line 91).
2. **NFR-MAT-2 (Zero CLI-observable contract renames):** The plugin's four outbound MCP tools (`reply, react, edit_message, download_attachment`), the root-crate daemon-bridge's ten MCP tools (`chat_post, chat_subscribe, chat_reply, chat_list, chat_list_threads, claudebase_daemon_status, agent_register, agent_unregister, agent_list_alive, agent_reap`), and the inbound `notifications/claude/channel` method name MUST remain bit-for-bit identical to v0.6. The only contract delta is the additive optional fields per FR-MAT-6 and FR-MAT-7 (plan §"Architectural Constraints → Frozen", points 1–3, lines 80–83).
3. **NFR-MAT-3 (Installer behavior unchanged):** `install.sh` MUST patch the **official** `telegram@claude-plugins-official` plugin's `.mcp.json` per the v0.6 implementation at `install.sh:551-702` (Plan-Critic VERIFIED line range, plan §"Architectural Constraints → Frozen", point 6, line 89). No installer changes are required for this feature.
4. **NFR-MAT-4 (`claudebase run` argv unchanged):** `claudebase run` MUST continue to exec `claude --channels plugin:telegram@claude-plugins-official` per the v0.6 implementation at `src/main.rs:177` (plan §"Architectural Constraints → Frozen", point 7, line 90).
5. **NFR-MAT-5 (Single bot serves all three routing keys):** A single `TELEGRAM_BOT_TOKEN` MUST be sufficient to satisfy KP1, KP2 and KP3 — no per-CLI bot, no per-topic bot. The `agent_registry` partial-unique index from FR-MAT-2.2 is the load-bearing mechanism (plan §"Success Criteria" closing bullet, line 48).
6. **NFR-MAT-6 (`src/plugin/bridge.rs` preserved bit-for-bit):** `src/plugin/bridge.rs` MUST remain the v0.6 692-line baseline. The v0.8 additions — session-cache, reconnect-replay, `ensure_daemon_running` — MUST NOT be ported (plan §"Open for modification → bridge.rs", line 101; plan §"Files Likely Affected → Preserved", line 190).

### 18.5 Acceptance Criteria

The QA-cycle MUST verify these three load-bearing criteria with concrete evidence on a live Telegram bot — they are copied verbatim from `.claude/plan.md` §"Success Criteria" (lines 37–48) and they are the only AC items for this section. KP1, KP2 and KP3 are non-negotiable; a build that fails any one is NOT shippable.

1. **AC-MAT-KP1 (DM routing):** Operator messages bot `@X` in **DM**. Routing key resolves to `(dm_chat_id, None)`. The inbound surfaces in **CLI A** as a `<channel source="plugin:telegram:telegram" chat_id="..." thread_id="" user="...">` event. CLI A's `reply` (no `message_thread_id`) lands in the same DM. Daemon log MUST contain the literal line `routed (chat_id=<N>, thread_id=None) -> cli_id=A`. SQL `SELECT cli_id, chat_id, thread_id FROM agent_registry WHERE state='alive'` MUST contain the row `(A, <dm_chat_id>, NULL)`. Evidence: OS-level screenshot `tc-kp1-dm-after.png` + terminal screenshot `tc-kp1-cli-a-channel.png` + daemon log line + SQL row output (plan §"Success Criteria" KP1 + evidence table, lines 39, 54).
2. **AC-MAT-KP2 (Forum-topic α routing):** Operator messages the **same bot `@X`** in a **group with forum topics enabled**, topic α. Routing key resolves to `(group_chat_id, Some(thread_id_α))`. The inbound surfaces in **CLI B** (B ≠ A) as a `<channel>` event with `thread_id` set. CLI B's `reply` echoing back `message_thread_id` lands in topic α. Daemon log MUST contain `routed (chat_id=<G>, thread_id=Some(α_id)) -> cli_id=B`. SQL row `(B, <G>, α_id)` MUST exist. Evidence: screenshot `tc-kp2-topicα-after.png` + terminal screenshot + daemon log + SQL row (plan §"Success Criteria" KP2 + evidence table, lines 40, 55).
3. **AC-MAT-KP3 (Forum-topic β routing):** Operator messages the **same bot `@X`** in the **same group**, topic β (a different topic from KP2). Routing key resolves to `(group_chat_id, Some(thread_id_β))`. The inbound surfaces in **CLI C** (C ≠ A, C ≠ B). CLI C's `reply` lands in topic β. Daemon log MUST contain `routed (chat_id=<G>, thread_id=Some(β_id)) -> cli_id=C`. SQL row `(C, <G>, β_id)` MUST exist. Three distinct CLIs MUST be visible side-by-side in `agent_registry`. Evidence: screenshot `tc-kp3-topicβ-after.png` + terminal screenshot + daemon log + SQL row (plan §"Success Criteria" KP3 + evidence table, lines 41, 56).

All three cases are **Mixed** Verification Class (UI/UX + CLI + DB) per the plan's evidence-required table (line 52). Screenshots are OS-level (PowerShell `Get-Screenshot` / Snipping Tool on Windows; `screencapture` on macOS; `gnome-screenshot` on Linux) because Playwright MCP cannot capture native Telegram Desktop (Plan-Critic finding #6, plan line 58). Telegram Web in a Playwright-driven browser is an acceptable alternate path.

### 18.6 Affected Components

**Modified (daemon + plugin internals — open for change per plan §"Architectural Constraints → Open for modification", lines 95–101):**

- `src/daemon/telegram.rs` — teloxide `Dispatcher` rewrite of v0.6 `run_long_poll`, routing-key extraction, error handling (Slice 3, 5)
- `src/daemon/agent_registry.rs` — additive schema columns (`routing_chat_id`, `routing_thread_id`) + partial-unique index + new `register_routing` API (Slice 2)
- `src/daemon/server.rs` — UDS dispatch table grows daemon-internal envelopes for plugin-forwarded outbound (Slice 4)
- `src/daemon/chat.rs` — broadcast bus filter by `(chat_id, thread_id)` routing key (Slice 3)
- `src/daemon/mod.rs` — Dispatcher wiring
- `src/daemon/migrations.rs` (or wherever v0.6 daemon migrations live) — additive schema migration (Slice 2)
- `plugins/telegram-rs/src/telegram/bot.rs` — disable `getUpdates` poller; keep access-gate + pairing UI (Slice 4)
- `plugins/telegram-rs/src/mcp/server.rs` — `handle_reply` / `handle_react` / `handle_edit_message` / `handle_download_attachment` forward to daemon over UDS (Slice 4)
- `plugins/telegram-rs/src/mcp/notification.rs` — emit `thread_id` optional meta on inbound (Slice 3)
- `plugins/telegram-rs/src/mcp/tools.rs` — `reply` tool schema gains optional `message_thread_id` (Slice 4)
- `Cargo.lock` — regen

**Preserved bit-for-bit (frozen contract per plan §"Architectural Constraints → Frozen", lines 78–91):**

- `plugins/telegram-rs/src/mcp/protocol.rs`
- `src/plugin/bridge.rs` — the v0.6 692-line version, NOT the v0.8 1066-line version
- `src/plugin/mcp.rs` — v0.6 `TOOL_WHITELIST` exactly
- `install.sh` Telegram plugin patching block at v0.6 lines 551–702

**Created:**

- `docs/use-cases/multi-agent-telegram-on-v0.6_use_cases.md`
- `docs/qa/multi-agent-telegram-on-v0.6_test_cases.md`
- `docs/qa/multi-agent-telegram-on-v0.6_smoke_runbook.md`
- This PRD section (§18) appended to `docs/PRD.md`

### 18.7 Schema Changes

The §17 `chat.db` (schema v6 with the `agent_registry` table) is extended additively in a new migration (schema v7 in `src/migrations.rs`, registered after the existing `apply_v6` per plan Slice 2 line 122):

```sql
-- additive migration on top of the §17 agent_registry table.
-- Two nullable columns + one partial-unique index. No rows altered, no columns dropped.
ALTER TABLE agent_registry ADD COLUMN routing_chat_id   INTEGER;
ALTER TABLE agent_registry ADD COLUMN routing_thread_id INTEGER;

-- One-CLI-per-routing-key invariant: at most one state='alive' row for any
-- given (routing_chat_id, routing_thread_id) tuple. NULL thread_id is the DM
-- case and is treated as a distinct routing key from any non-NULL thread_id
-- per SQLite's NULL-as-distinct semantics in UNIQUE indexes.
CREATE UNIQUE INDEX IF NOT EXISTS agent_registry_routing_alive_uniq_idx
    ON agent_registry(routing_chat_id, routing_thread_id)
    WHERE state = 'alive';
```

Migration discipline (plan Slice 2 lines 122–123, plan R7 line 209):

- Idempotent — re-running on an already-migrated `chat.db` MUST be a no-op.
- Tolerant of pre-existing v0.7/v0.8 leftover columns on operator dev boxes — leftover columns are NOT dropped, they become unused.
- Forward-compatible rollback — both new columns are nullable so old INSERTs continue to be valid if a future revert removes only the new code.

The §17 v6 `agent_registry_thread_name_alive_idx` index (FR-ACD-5.7 agent-name uniqueness within thread) remains in place and is orthogonal to the new routing-key index.

### 18.8 UI Changes

**Telegram bot UX (server-handled, no CLI UI changes).** The bot's command surface gains four commands handled in the daemon's Dispatcher handler tree per FR-MAT-8:

- `/agents` — list CLIs bound to the current routing key (topic-aware: filters by the originating chat_id and message_thread_id of the command message itself per FR-MAT-8.2).
- `/switch <cli>` — rebind the current routing key to the named CLI; conflict resolution = second tap wins (FR-MAT-8.3).
- `/whoami` — show current binding `(routing_chat_id, routing_thread_id) -> agent_id, agent_name` (FR-MAT-8.4).
- `/here` — show the bound CLI's host / cwd / process metadata (FR-MAT-8.5).

In a group, `/switch` is restricted to the user whose `user_id` matches the binding's last user_id OR a chat admin (FR-MAT-8.6 default policy, refined by red-team at bootstrap per plan OQ3 line 225).

No Claude Code CLI-side UI changes are introduced. The inbound `<channel>` event surface gains an optional `thread_id` attribute when the source is a forum topic (per FR-MAT-7.1) but old CLIs ignore unknown fields per JSON-RPC convention so they continue to operate unchanged.

### 18.9 Risks and Dependencies

1. **R-MAT-1 (Full v0.6 baseline = loss of v0.7 quality-of-life).** Branching from `claudebase-v0.6.0` cleanly drops v0.7 amenities (insights tag-filter, hooks, `/update-claudebase`, `prompts/` reorg). Selective port-forward of v0.7 Bucket-A items is deferred to a follow-up feature on top of the merged branch (plan R1 line 203; out-of-scope line 219).
2. **R-MAT-2 (`reply` becoming an optional-additive contract change).** Strictly an additive contract change. Operator approved as C3 option (a). Backward-compat guaranteed by the field being optional + old consumers ignoring unknown fields (plan R2 line 204).
3. **R-MAT-3 (`/switch` security in groups).** Default policy: per-tap user-check (only last user_id or chat admin). Red-team at bootstrap may refine (plan R3 line 205; plan OQ3 line 225).
4. **R-MAT-4 (teloxide 0.17 `Message.message_thread_id` field existence).** Slice 1 architect call verifies the field exists in teloxide 0.17. If absent: fallback options (a) bump teloxide to 0.18+, (b) custom deser bypass, (c) downgrade to "topic routing unsupported, only DM works" with explicit operator approval to cut scope (plan R4 line 206; plan Slice 1 line 114).
5. **R-MAT-5 (Plugin frankenstein polling disabled = no fallback if daemon dies).** v0.6 plugins could poll standalone. Mitigation: explicitly documented as deliberate architectural choice (operator-stated C2); v0.6 standalone mode preserved via `cfg(feature = "legacy-direct-poll")` emergency-revert escape-hatch (plan R5 line 207).
6. **R-MAT-6 (v0.6's latent dual-poller 409 vulnerability).** Two pollers (daemon teloxide + plugin frankenstein) both reading the same `TELEGRAM_BOT_TOKEN` in v0.6 baseline is a known latent race. Resolved by FR-MAT-4.1 / Slice 4 disabling the plugin's poller — daemon becomes sole owner (plan R6 line 208).
7. **R-MAT-7 (`chat.db` migration on a dev box that has v0.7/v0.8 leftovers).** Slice 2 migration is `ALTER TABLE ADD COLUMN` only; pre-existing leftover columns survive. New code reads only the new columns (plan R7 line 209).
8. **R-MAT-8 (Topic-aware `/agents` in a group main thread).** Command sent in main group thread without a topic context — list all CLIs in that group. Filtering logic reads `message.message_thread_id` from the command message itself (plan R8 line 210).
9. **R-MAT-9 (Symptom-only rebuild rather than v0.8 root-cause isolation).** The operator's decision to rollback rather than fix-forward v0.8 IS a symptom-only patch at the meta-level — root cause of v0.8 brokenness is NOT pursued because forward-debug cost exceeds rebuild cost. Tracked in Decisions → Symptom-only patches below. Operator-acknowledged trade-off (plan Symptom-only patches line 294).

## Facts

### Verified facts

- Plan at `.claude/plan.md` read in full this session (321 lines). Plan version v2, post Plan-Critic + post-operator C1/C2/C3 decisions (plan line 7). Salience: high.
- KP1–KP3 acceptance criteria copied verbatim from `.claude/plan.md` §"Success Criteria" lines 37–48; routing key tuple `(chat_id, Option<message_thread_id>)` mandated by plan line 21. Salience: high.
- v0.6 tag `claudebase-v0.6.0` is the branch-point; `Cargo.toml` workspace version `0.6.0`; members `[".", "plugins/telegram-rs"]`. Source: plan Facts line 242 (verified via `git show claudebase-v0.6.0:Cargo.toml`). Salience: high.
- v0.6 ROOT `Cargo.toml` pins `teloxide = "0.17"` for the daemon's TG client; v0.6 plugin `Cargo.toml` pins `frankenstein = "0.49"` with `features = ["client-reqwest"]`. Source: plan Facts lines 243–244 (verified via `git show claudebase-v0.6.0:Cargo.toml:90` and `…:plugins/telegram-rs/Cargo.toml:23`). Salience: high.
- v0.6 contains BOTH `src/daemon/telegram.rs` (teloxide) AND `plugins/telegram-rs/src/telegram/bot.rs` (frankenstein) — both with `getUpdates` polling reading `TELEGRAM_BOT_TOKEN` from env. Latent dual-poller vulnerability. Source: plan Facts line 245. Salience: high.
- v0.6 already contains both `src/daemon/permissions.rs` AND `src/daemon/channel_state.rs` — Plan-Critic finding #11 correction. Source: plan Facts line 246. Salience: high.
- v0.6 MCP protocol version `"2025-11-25"`; max frame size 1 MiB. v0.6 IPC framing: length-prefixed 4-byte big-endian + UTF-8 JSON, 16 MiB cap on the daemon UDS. Source: plan Facts lines 247, 250. Salience: high.
- v0.6 notification literal `notifications/claude/channel` lives at `plugins/telegram-rs/src/mcp/notification.rs:59`. Source: plan Facts line 248 (Plan-Critic finding #12 correction). Salience: medium.
- v0.6 root-crate `src/plugin/mcp.rs` `TOOL_WHITELIST` contains exactly the 10 chat/agent/status tools (`chat_post, chat_subscribe, chat_reply, chat_list, chat_list_threads, claudebase_daemon_status, agent_register, agent_unregister, agent_list_alive, agent_reap`); the 4 plugin tools (`reply, react, edit_message, download_attachment`) are exposed by `plugins/telegram-rs/src/mcp/server.rs:201-204` + `tools.rs:15,41,54,72`. Source: plan Facts line 249. Salience: high.
- v0.6 `claudebase run` execs `claude --channels plugin:telegram@claude-plugins-official` per `src/main.rs:177`. Source: plan Facts line 251 (Plan Critic verified). Salience: high.
- v0.6 installer patches the official `telegram@claude-plugins-official` plugin's `.mcp.json` at `install.sh:551-702`. Source: plan Facts line 252 (Plan Critic VERIFIED). Salience: high.
- v0.6 `agent_registry` schema: `(agent_id, agent_name, connection_id, chat_thread_id: Option<String>, state, spawned_at, last_pinged_at)` with partial-unique index `(chat_thread_id, agent_name) WHERE state='alive'`. Source: plan Facts line 253 (`src/daemon/agent_registry.rs:92-138`). Salience: high.
- v0.6 PRD §17 does NOT mention forum-topic support in scope. Source: plan Facts line 254 + this session's read of `docs/PRD.md` §17.7 schema block (lines 583–662). Salience: medium.
- Existing `docs/PRD.md` ends at line 781 with §17 `## Decisions → Symptom-only patches` block. §18 appends after. Source: `wc -l` this session = 781 lines + `grep "^## "` showing §17 last. Salience: medium.
- The companion design doc `docs/plans/multi-agent-telegram-on-v0.6.md` is 320 lines, byte-equivalent to `.claude/plan.md` body. Source: `wc -l` this session. Salience: low.
- Knowledge-base / insights corpus NOT activated for this project — `<project>/.claude/knowledge/index.db` and `<project>/.claude/knowledge/insights.db` absent on this v0.6-branched workspace per Mira's spawn-prompt instruction. Corpus protocol silently skipped per `~/.claude/rules/knowledge-base.md` `## Activation sentinel`. Salience: low.

### External contracts

- **teloxide 0.17 `Dispatcher` API** — symbol: `teloxide::dispatching::Dispatcher::builder`, `handler_tree!`, `teloxide::types::Message`, `Message.message_thread_id` (expected `Option<i32>` per Telegram Bot API) — source: `git show claudebase-v0.6.0:Cargo.toml:90` (PIN verified this session via plan Facts) — verified: **PIN yes; `message_thread_id` field-existence DEFERRED to Slice 1 architect verification (R-MAT-4)**. Salience: **high** (Slice 1 BLOCKING check).
- **teloxide 0.17 `SendMessageSetters::message_thread_id`** — symbol: outbound builder method that targets a forum-topic on `sendMessage` — source: crates.io / docs.rs `teloxide` v0.17 (NOT opened this session) — verified: **no — assumption**. Risk: builder method may have a different name (e.g. `with_message_thread_id`, `set_message_thread_id`) or may require an HTTP-call-builder vs setter pattern. How to verify: Slice 1 / Slice 4 architect pre-review. Salience: high.
- **frankenstein 0.49** — symbol: `Bot`, `Message`, `SendMessageParams`, `GetUpdatesParams`, `AsyncTelegramApi` — source: `git show claudebase-v0.6.0:plugins/telegram-rs/Cargo.toml:23` (verified this session via plan Facts line 243) — verified: yes (pinned + Slice 4 only DISABLES the polling loop, does not require new symbols). Salience: high.
- **Telegram Bot API `message_thread_id` field** — symbol: optional integer on `Message` object; used in `sendMessage` outbound to target a specific forum topic — source: `https://core.telegram.org/bots/api#message` (NOT opened this session) — verified: **no — assumption**. Salience: high (Slice 1 architect verifies field-existence in teloxide deser).
- **MCP `notifications/claude/channel` notification method** — symbol: notification with `meta` JSON object carrying source / content / target_agent_id and (new in this feature) optional `thread_id` — source: `plugins/telegram-rs/src/mcp/notification.rs:59` per plan Facts line 248 (NOT opened this session by me; cited via plan) — verified: **partial** (literal method name verified by plan Facts; meta JSON shape inherited from §17.3 FR-ACD-3.5 + FR-ACD-11.2). Salience: high.
- **JSON-RPC 2.0 additive-evolution convention** — symbol: «consumers MUST ignore unknown fields» — source: JSON-RPC 2.0 spec § "Notification" + § "Extensions" (NOT opened this session) — verified: **no — assumption** (industry convention, broadly applied; v0.6 Claude Code client is the only consumer here and operator confirms its tolerance per plan Assumptions line 264). Salience: medium.

### Assumptions

- v0.6 worked «stably» for the operator because they ran ONE polling owner at a time in practice — either the daemon's teloxide loop OR the plugin's frankenstein loop, not both. The dual-poller is a latent vulnerability that v0.6 never hit in single-CLI use. Risk: «v0.6 was working» is operator shorthand; the architecture has a known race. Mitigation: FR-MAT-4.1 / Slice 4 explicitly resolves it by making daemon the sole owner. Source: plan Assumptions line 263. Salience: high.
- Adding optional fields to existing JSON-RPC notification meta is non-breaking per JSON-RPC convention (consumers MUST ignore unknown fields). Risk: a strict consumer might reject. Mitigation: documented; v0.6 Claude Code client is the only consumer and is known-tolerant per plan. Source: plan Assumptions line 264. Salience: medium.
- `chat.db` schema migration tolerates pre-existing v0.7/v0.8 columns left over on a dev box. Risk: SQLite `ALTER TABLE ADD COLUMN` is additive and does not conflict with existing columns; SQLite UNIQUE indexes on nullable columns treat NULL as distinct so DM-routed (`thread_id = NULL`) bindings do not collide with each other within the partial index. How to verify: Slice 2 unit tests exercise both fresh-DB and pre-populated-DB cases AND multiple-NULL-thread_id-DM rows. Source: plan Assumptions line 265 + FR-MAT-2.4 done-when. Salience: medium.
- The operator's «one bot serves three CLIs» model presumes the Telegram Bot API permits a single bot to receive Updates from a DM and a group (with topics) simultaneously. Standard bot capability per Telegram docs; not session-verified. Risk: if the bot lacks Forum-Topics permission in the group, KP2/KP3 silently fail. How to verify: Slice 3 daemon doctor probe; QA runbook step «invite bot as admin, enable Forum Topics on the group». Salience: high.

### Open questions

- **OQ-MAT-1 (Selective port-forward of v0.7 Bucket-A items).** Default: NO — strict v0.6 baseline + KP1–KP3 routing only. Operator may green-light Bucket-A as follow-up. Source: plan OQ1 line 223. Needs: operator decision. Salience: medium.
- **OQ-MAT-2 (teloxide 0.17 `Message.message_thread_id` field existence).** Slice 1 architect call. If absent → fallback options per R-MAT-4. Source: plan OQ list + plan R4 line 206. Needs: architect call at Slice 1. Salience: **high** (Slice 1 BLOCKING).
- **OQ-MAT-3 (`/switch` security in groups).** Default: per-tap user check (last binding's user_id OR chat admin). Red-team at bootstrap may refine. Source: plan OQ3 line 225. Needs: red-team refinement at bootstrap. Salience: medium.
- **OQ-MAT-4 (Release version after merge — v0.7.0-rebuild vs v0.10.0 vs v0.6.1).** Plan recommendation: v0.10.0 (skip v0.9 entirely; v0.9 reserved for the fleet plan). Source: plan OQ4 line 226. Needs: operator decision at `/release` time. Salience: medium.
- **OQ-MAT-5 (Orphan-inbound fallback policy precise shape).** Default: log + TG reply «no CLI bound to this routing key». Architect refines exact UX at bootstrap. Source: plan §"Verification" step 4 line 235 + FR-MAT-9.5. Needs: architect call at bootstrap. Salience: medium.
- **OQ-MAT-6 (Slice 6 cooperation of `permissions.rs` and `channel_state.rs`).** Do the two modules cooperate or contend on access-grant state in v0.6? Architect inspects at Slice 6. Source: plan Open questions line 270. Needs: architect call at Slice 6 pre-review. Salience: medium.

## Decisions

### Inbound validation

- Inbound task: «append §N PRD section for `multi-agent-telegram-on-v0.6` based on `.claude/plan.md`», emit Facts + Decisions blocks, cite teloxide / frankenstein / Telegram API external contracts. Challenged: yes — verified that `.claude/plan.md` exists and is v2 (post-Plan-Critic), that `docs/PRD.md` last section is §17 (not §18 or higher), that the design doc `docs/plans/multi-agent-telegram-on-v0.6.md` is byte-equivalent (untracked, 320 lines vs `.claude/plan.md`'s 321 — diff is the trailing newline). No contradictions detected between the plan body and the spawn-prompt's FR-MAT enumeration. Outcome: proceeded as instructed. Salience: high.
- Spawn-prompt instruction «KP1–KP3 are the ONLY AC items» challenged briefly against the §17 precedent of 15 AC items (AC-ACD-1 through AC-ACD-15). Outcome: kept to KP1–KP3 as instructed — the plan explicitly states «KP1-KP3 are the load-bearing acceptance criteria» (plan line 60) and the spawn-prompt confirms «AC count (should be 3 — KP1/KP2/KP3)». Additional verification depth lives in the QA test cases file (TC-1 through TC-9 per plan Deliverables Checklist line 71), not in the PRD AC list. Salience: medium.
- Spawn-prompt date `2026-06-02` matches the session reminder `currentDate: 2026-06-02` and matches the plan's `Date drafted: 2026-06-02`. No drift. Salience: low.

### Decisions made

- Section numbered §18 because `grep "^## §" docs/PRD.md` shows the last existing section is §17 (line 407). Q1 hack? no | Q2 sane? yes | Q3 alternatives? skipping to §19 considered (rejected — wasteful, no §18 placeholder exists) | Q4 symptom-or-cause? cause | Q5 root-cause-tracked? n/a. Salience: medium.
- 10 FR-MAT groups (FR-MAT-1 through FR-MAT-10) — one per directive from the spawn prompt's enumerated criteria (FR-MAT-1 routing key, FR-MAT-2 schema additive, FR-MAT-3 daemon-owns-TG, FR-MAT-4 plugin thin-bridge, FR-MAT-5 plugin outbound forward, FR-MAT-6 reply `message_thread_id`, FR-MAT-7 channel meta `thread_id`, FR-MAT-8 bot commands, FR-MAT-9 error handling, FR-MAT-10 daemon-restart resilience). Q1–Q5: passes all five cleanly — direct mapping from spawn-prompt requirements. Salience: medium.
- 3 AC items (AC-MAT-KP1 / KP2 / KP3) only — matching the plan's stated load-bearing trio (plan line 60) and the spawn-prompt's explicit «AC count (should be 3)» instruction. Q1 hack? no | Q2 sane? yes (KP1-KP3 are the load-bearing criterion; deeper test coverage lives in QA test cases) | Q3 alternatives? add KP4 «`/agents` works» / KP5 «daemon restart resilience» — rejected per spawn-prompt instruction; deeper coverage in QA. Salience: high.
- Optional `message_thread_id` / `thread_id` typed as `Option<String>` (not `Option<i64>`) per v0.6's «ID-as-string discipline» (plan §"Architectural Constraints → Frozen", point 4, line 86 — explicit «string per v0.6 ID-as-string discipline»). Q1 hack? no | Q2 sane? yes | Q3 alternatives? `Option<i64>` to match Telegram's native integer type — rejected per the documented v0.6 discipline; daemon parses/converts at the boundary. Salience: high.
- All teloxide / Telegram Bot API contracts marked `verified: no — assumption` because no teloxide source / docs / API was opened in this session. The PIN itself (teloxide 0.17) is verified via the plan's Facts citation of `Cargo.toml:90`, but field-level symbols (`Dispatcher::builder`, `handler_tree!`, `Message.message_thread_id`, `SendMessageSetters::message_thread_id`) are explicitly deferred to Slice 1 architect verification per OQ-MAT-2 and R-MAT-4. This conservative labeling is correct per Protocol 1 and matches the spawn-prompt's directive («`verified: no — assumption` is the correct label for symbols you have NOT opened in this session»). Salience: high.
- Q1-Q5 verdict on the operator's «rollback rather than fix-forward» decision (load-bearing meta-decision): Q1 hack? **yes — meta-level symptom-only patch** | Q2 sane? yes (operator-stated cost-benefit) | Q3 alternatives? fix-forward considered + rejected by operator | Q4 symptom-or-cause? **symptom** (root cause of v0.8 brokenness NOT isolated) | Q5 root-cause-tracked? **partial** — tracked under «Symptom-only patches» here AND in the plan's `### Symptom-only patches` block (plan line 294). Operator-acknowledged trade-off; flagged below for downstream visibility. Salience: high.

### Hacks acknowledged

- The `cfg(feature = "legacy-direct-poll")` feature-flag on the plugin's frankenstein poller (FR-MAT-4.1) is a deliberate fallback escape-hatch for «daemon unavailable but operator still wants TG in single-CLI mode». It is a workaround, not a long-term path. **Removal path:** drop the cfg gate once daemon-ownership has been proven stable across 2+ releases. Tracked in this PRD section's FR-MAT-4.1 + plan §"Decisions → Hacks acknowledged" line 291. Salience: medium.

### Symptom-only patches (with root-cause links)

- The operator's decision to **rollback v0.7/v0.8 entirely** rather than isolate the v0.7/v0.8 root cause IS a symptom-only patch at the meta-level — the empirical brokenness was the symptom, the root cause was not pursued because forward-debug cost exceeded rebuild cost. Symptom: «v0.7/v0.8 multi-CLI work was broken, root cause not isolated despite extended debugging». Root cause that remains: unknown — explicitly NOT pursued by operator decision. Tracked at: plan §"Symptom-only patches" line 294 AND an implicit obligation to log any specific v0.8 failure modes uncovered during the v0.6+ build into `docs/issues/`. Operator-acknowledged trade-off. Salience: high.

---

### §18.10 Slice 8 — `chat_ask` MCP Tool, Telegram Inline Keyboard, CallbackQuery Handling

**Status:** SHIPPED 2026-06-04 (with AR-9 post-implementation amendment — see §18.10.9)
**Date:** 2026-06-04
**Implemented:** commits `5dfcf8d` (8a single-select), `86ab5ff` (8b multi-select), `4a65819` (live-fix AR-9 frame shape)
**Priority:** High
**Related:** §18.3 FR-MAT-1 through FR-MAT-10 (base routing layer this slice builds on); §18.7 Schema Changes (additive `pending_asks` migration runs after `apply_routing_migration`); §18.5 Acceptance Criteria (Slice 8 adds AC-MAT-CHA-1 through AC-MAT-CHA-3 below). Plan source: `.claude/plan.md` Slice 8 block (lines 312–399) + AR-1 through AR-8 blocks (lines 335–378) + Slice 8c block (lines 386–398) + operator OQ resolutions (lines 380–384), all read this session.

> **⚠️ AR-9 Live-fix amendment (2026-06-04):** Initial Slice 8a/8b deploy did NOT surface chat_ask responses in the requesting CC's session despite daemon delivering frames to bridge UDS. Root cause discovered live: CC's `<channel>` surface renderer **silently drops** any frame whose `params.meta` contains keys outside the inbound-Telegram schema (`chat_id`/`message_id`/`user`/`user_id`/`ts`/`thread_id`/`target_agent_id`). The Slice 8 extras (`is_callback_response`, `ask_id`, `value`/`values`, `multi`, `question`, `options`, `originating_agent_id`) triggered the drop. **All meta-shape requirements in FR-MAT-11.5 and FR-MAT-11.6 below are SUPERSEDED by AR-9 (§18.10.11)** — Slice 8 round-trip data now lives in `params.content` as a parseable preamble. Original text below is retained for historical context.

Changelog: Agents can now ask the operator a multi-option question that appears as native Telegram inline buttons; tapping a button routes the response back to the requesting agent automatically.

#### §18.10.1 Feature Description

Slice 8 adds the `chat_ask` MCP tool so that Claude Code CLI instances (e.g. Mira in plan-mode) can surface structured multi-option decisions to the operator as native Telegram inline keyboard buttons, and receive the operator's tap as a `<channel>` event routed back to the originating CC.

The motivation is the operator's direct request: «опции которые cli предлагает в план моде не приходят в виде кнопок в тг» (the options Mira proposes in plan mode do not arrive as buttons in Telegram). Slice 8 closes this gap by making inline keyboard rendering a first-class daemon capability.

The feature is split into three atomic sub-slices:

- **8a (single-select MVP):** `chat_ask` tool, `pending_asks` SQLite table, `InlineKeyboardMarkup` send via new `OUTBOUND_TG_KEYBOARD` channel, CallbackQuery parsing + single-select response emit.
- **8b (multi-select stateful toggle):** multi-select keyboard with toggle-tap ✓ marker updates via `Bot::edit_message_reply_markup`, Done-button finalization with `meta.values=[...]` array.
- **8c (`chat_list_pending_asks` debug tool):** read-only MCP tool listing open pending asks for debugging.

No user-facing changes to the operator's existing KP1–KP3 routing flows. The `OUTBOUND_TG_KEYBOARD` mpsc channel preserves the single-`Bot`-owner discipline from `telegram.rs:936–944` — no parallel `Bot::new` constructor is introduced.

#### §18.10.2 User Stories

- **UC-MAT-16 (single-select):** As Mira entering plan-mode, I want to call `chat_ask` with a question and 2–8 options so that the operator receives an inline keyboard in Telegram and tapping a button routes the response back to me as a `<channel>` event with `meta.value` set.
- **UC-MAT-17 (multi-select):** As Mira, I want to call `chat_ask` with `multi=true` so that the operator can tap multiple options (each tap toggles a ✓ marker on the button) before tapping Done to finalize, delivering `meta.values=[...]` to me.
- **UC-MAT-18 (debug list):** As Mira or the operator, I want to call `chat_list_pending_asks` to see which asks are still open, for debugging "why didn't my chat_ask response come through?".

#### §18.10.3 Functional Requirements

##### FR-MAT-11.1 — `chat_ask` added to `TOOL_WHITELIST` (Slices 8a + 8c)

1. The string `"chat_ask"` MUST be added to `TOOL_WHITELIST` in both `src/plugin/mcp.rs` (SEC-7 whitelist gate) AND in `src/daemon/server.rs` `tools/list` enumeration and dispatch table. Adding the tool name to only one location is insufficient — both gates must be satisfied for Claude Code to discover and invoke the tool.
2. The string `"chat_list_pending_asks"` MUST likewise be added to both `TOOL_WHITELIST` locations (Slice 8c).
3. The v0.6 `TOOL_WHITELIST` of 10 existing chat/agent tools MUST remain unchanged — additions are strictly additive.

##### FR-MAT-11.2 — `InlineKeyboardMarkup` via `OUTBOUND_TG_KEYBOARD` mpsc (Slice 8a)

1. A new `OnceLock<mpsc::UnboundedSender<(i64, Option<i64>, String, InlineKeyboardMarkup, oneshot::Sender<Result<i64>>)>>` named `OUTBOUND_TG_KEYBOARD` MUST be declared in `src/daemon/telegram.rs` parallel to the existing `OUTBOUND_TG` channel (`telegram.rs:79`).
2. The channel receiver MUST be drained inside `run_long_poll`'s existing outbound-send loop (`telegram.rs:1040–1088`) so that inline-keyboard messages are issued by the SAME teloxide `Bot` instance that processes inbound `Update`s. Constructing a parallel `Bot::new(token)` in `server.rs` is PROHIBITED — it would break the single-owner discipline and diverge `TELOXIDE_API_URL` in test mode.
3. The `chat_ask` handler in `server.rs` MUST enqueue a send request to `OUTBOUND_TG_KEYBOARD` and await the oneshot receiver for the returned `message_id`. INSERT of the `pending_asks` row MUST happen ONLY after the send succeeds (send-then-insert ordering — if send fails no orphan row is created).
4. One keyboard button MUST be rendered per option. Maximum 8 options per ask. Requests with more than 8 options MUST be rejected with JSON-RPC error `-32602`.
5. Single-select callback data format: `<ask_id>:<value>` (ask_id = UUID v4 = 36 chars; value ≤ 27 bytes — see FR-MAT-11.9 budget enforcement).
6. Multi-select callback data format per option: `<ask_id>:toggle:<option_id>`. Done-button callback data: `<ask_id>:done`.

##### FR-MAT-11.3 — `Update` struct extended with `callback_query` (Slice 8a, AR-2)

1. The daemon's `Update` struct in `src/daemon/telegram.rs` (line 108 in the v0.6 baseline) MUST be extended additively with the field `#[serde(default)] callback_query: Option<CallbackQuery>`. When `callback_query` is absent from the wire JSON, `serde(default)` yields `None` — the Slice 0 baseline wire shape is preserved bit-for-bit.
2. A new `CallbackQuery` struct MUST be defined with the following fields, matching the Telegram Bot API `callbackQuery` object minimal required set as validated by the architect at AR-2: `id: String`, `from: User`, `chat_instance: String` (REQUIRED per Telegram Bot API — used for cache scoping), `#[serde(default)] data: Option<String>` (absent on game buttons), `#[serde(default)] message: Option<MessageRef>`.
3. A new `MessageRef` struct MUST be defined: `{ message_id: i64, chat: Chat }`. This struct exists solely to allow the daemon to call `edit_message_reply_markup(chat_id, message_id)` without deserializing the full `Message` shape.
4. The implementer MUST verify the field names `id`, `from`, `chat_instance`, `data`, `message` against the live Telegram Bot API docs at `https://core.telegram.org/bots/api#callbackquery` at implementation time and cite the result under `### External contracts` in the implementing agent's `## Facts` block.
5. `User` and `Chat` structs reuse the existing definitions in `telegram.rs` — no new struct duplication.

##### FR-MAT-11.4 — CallbackQuery access gate (`gate_callback`) (Slice 8a, AR-3)

1. A new helper function `gate_callback(access: &Access, sender_id: &str) -> bool` MUST be defined in `src/daemon/telegram.rs` (or `channel_state.rs`, per implementer). It returns `true` if and only if `access.allow_from.contains(sender_id)`.
2. The existing `gate_dm` function (at `telegram.rs:534`) MUST NOT be reused for CallbackQuery dispatch. `gate_dm` issues pairing codes to non-allowed senders; applying it to callbacks would send a pairing code when an unknown user taps an inline button — incorrect UX and incorrect security posture.
3. In `process_batch_with_pairing`, after extracting a `CallbackQuery` update, the daemon MUST call `gate_callback(access, &callback.from.id.to_string())`. On `false` (non-allowed sender): DROP the callback silently. No pairing code reply. No `answerCallbackQuery` call. No log at WARN or above (log at DEBUG only). Unit test `callback_from_disallowed_user_is_silently_dropped` MUST cover this path.

##### FR-MAT-11.5 — Single-select response emit (Slice 8a, AR-1 + AR-4 + AR-5)

1. `Bot::answer_callback_query(callback_id)` MUST be called as the FIRST action in the CallbackQuery dispatch branch, before any SQLite read, before any `editMessageReplyMarkup`, before any channel-notification emit. This clears the Telegram "loading" spinner on the tapped button within the required ~15-second window (Telegram Bot API contract per AR-1).
2. On a valid single-select tap, after `answerCallbackQuery` succeeds, the daemon MUST emit a `notifications/claude/channel` event with the following `meta` fields:
   - `meta.is_callback_response = true`
   - `meta.ask_id` — the UUID v4 from the `pending_asks` row
   - `meta.value` — the option value from `callback_data` (single-select only)
   - `meta.question` — the original question text (AR-5: CC-compaction resilience)
   - `meta.options` — the full `[{label, value}]` JSON array (AR-5)
   - `meta.multi = false`
3. `target_agent_id` routing MUST follow the AR-4 dead-originating-agent fallback:
   - If `requesting_agent_id` (from the `pending_asks` row) names an agent currently in `state='alive'` in `agent_registry`: set `meta.target_agent_id = requesting_agent_id`. The existing `bridge.rs:823–837` filter routes the response to that CC only.
   - If `requesting_agent_id` is NOT alive: OMIT `meta.target_agent_id` AND ADD `meta.originating_agent_id = requesting_agent_id` (informational). A notification without `target_agent_id` is treated as an unaddressed broadcast by the bridge filter — all active CCs receive it. A compacted Mira reconstructs semantic context from `meta.question`, `meta.options`, `meta.ask_id`.
4. Unit test `callback_response_unaddressed_when_originating_agent_dead` MUST cover the dead-agent broadcast path.

##### FR-MAT-11.6 — Multi-select state-machine (Slice 8b, AR-7)

1. Each option-tap in a multi-select ask MUST execute an atomic SQLite operation `UPDATE pending_asks SET selected_values_json = ? WHERE ask_id = ? RETURNING selected_values_json` to read post-state atomically. The write-lock is released before the network call to Telegram.
2. After the atomic UPDATE, the daemon MUST call `Bot::edit_message_reply_markup` to redraw the inline keyboard with ✓ markers on the currently-selected options and a "Done" button at the bottom. The ✓ rendering appends `✓ ` to the option label in the `InlineKeyboardButton.text` field.
3. The `answerCallbackQuery` call MUST precede `editMessageReplyMarkup` so the spinner clears immediately even if the subsequent edit lags or 429s.
4. On HTTP 429 from `editMessageReplyMarkup`: retry ONCE after `retry_after` seconds (mirroring the inbound 429 handling at `telegram.rs:25–28`). On a second 429: give up silently. The SQLite row remains correct; the keyboard display is stale but data is not lost.
5. A Done-button tap MUST finalize the ask: emit a `notifications/claude/channel` event with `meta.multi=true`, `meta.values=[...]` (JSON array of all selected option values), `meta.question`, `meta.options`, `meta.ask_id`, and the AR-4 `target_agent_id` / `originating_agent_id` routing logic from FR-MAT-11.5. Then DELETE the `pending_asks` row.
6. Unit tests `multi_select_toggle_updates_pending_row`, `multi_select_done_emits_values_array_and_clears_pending`, `multi_select_concurrent_taps_serialize_via_sqlite`, `edit_message_reply_markup_called_with_correct_chat_message_pair` MUST all pass.

##### FR-MAT-11.7 — `pending_asks` table in `chat.db` (Slices 8a + 8b, AR-6)

1. A new helper `apply_pending_asks_migration(conn)` MUST be defined in `src/daemon/chat.rs` (or `src/daemon/migrations.rs`) and called from `chat::ensure_chat_db_schema` AFTER the existing `apply_routing_migration(conn)?;` call (chat.rs:343). The migration is additive — it MUST NOT modify existing tables.
2. The `pending_asks` table schema is:
   ```sql
   CREATE TABLE IF NOT EXISTS pending_asks (
     ask_id              TEXT PRIMARY KEY,
     chat_id             INTEGER NOT NULL,
     message_thread_id   INTEGER NULL CHECK (message_thread_id IS NULL OR message_thread_id > 0),
     message_id          INTEGER NOT NULL,
     requesting_agent_id TEXT NOT NULL,
     question            TEXT NOT NULL,
     options_json        TEXT NOT NULL,
     multi               INTEGER NOT NULL DEFAULT 0,
     selected_values_json TEXT NULL,
     created_at          INTEGER NOT NULL,
     expires_at          INTEGER NOT NULL
   );
   CREATE INDEX IF NOT EXISTS pending_asks_expires_idx ON pending_asks(expires_at);
   ```
3. The `question` column is mandatory (NOT NULL) for AR-5 CC-compaction resilience. The `message_id` column captures the Telegram message_id returned by `sendMessage` so `editMessageReplyMarkup` can target the correct message.
4. INSERT of a row MUST occur ONLY after `Bot::send_message` returns successfully and the `message_id` has been captured from the response. If `send_message` fails, no row is inserted and `chat_ask` returns an error to the caller (send-then-insert ordering).
5. TTL is 24 hours. `expires_at = created_at + 24 * 60 * 60 * 1000` (milliseconds). The GC predicate is `expires_at < now()` and applies unconditionally — abandoned multi-select asks also expire after 24h regardless of `selected_values_json` state.
6. GC MUST run from the existing long-poll loop's per-batch tail hook via a single `DELETE FROM pending_asks WHERE expires_at < ?` SQL call (low overhead — one SQL per batch cycle).
7. A new module `src/daemon/pending_asks.rs` MUST be created exposing these public helpers: `insert_pending`, `get_pending(ask_id)`, `update_selected_values`, `delete_pending`, `gc_expired`, `list_open(conn, optional_agent_id, optional_chat_id)`. This module parallels `agent_registry.rs` in structure.

##### FR-MAT-11.8 — `chat_list_pending_asks` debug MCP tool (Slice 8c)

1. A new handler `handle_chat_list_pending_asks` MUST be added to `src/daemon/server.rs`. It calls `pending_asks::list_open(conn, optional_agent_id, optional_chat_id)` and returns a JSON object `{asks: [{ask_id, chat_id, message_thread_id, question, requesting_agent_id, multi, options, created_at, expires_at}]}`. The `selected_values_json` field MUST NOT be returned — it is internal toggle state, not a debug-relevant field.
2. Two optional filter parameters are supported: `agent_id` (return only asks whose `requesting_agent_id` equals the supplied value), `thread` (return only asks whose Telegram thread matches the supplied thread string — parsed as `<chat_id>` or `telegram:<chat_id>`).
3. The tool returns only OPEN asks: rows that have not been answered (no finalized channel notification emitted for them yet) AND have not expired (`expires_at >= now()`). Answered rows (deleted after response emit) and expired rows (deleted by GC) are excluded.
4. This tool is read-only — calling it has NO side effects on Telegram state, on `pending_asks` rows, or on any outbound message.
5. Unit test `list_open_returns_only_unanswered_and_unexpired` MUST pass.

##### FR-MAT-11.9 — `callback_data` 64-byte budget enforcement (Slice 8a, AR-1)

1. Telegram limits `callback_data` to 1–64 bytes (UTF-8). The `chat_ask` handler in `server.rs` MUST validate that each option's callback data fits within this budget at request time — before sending any message to Telegram.
2. Single-select format `<ask_id>:<value>`: ask_id is UUID v4 = 36 bytes; separator `:` = 1 byte; value ≤ 27 bytes. Requests with a single-select option value exceeding 27 bytes MUST be rejected with JSON-RPC error `-32602` and a descriptive message explaining the 27-byte limit.
3. Multi-select toggle format `<ask_id>:toggle:<option_id>`: 36 + 7 (`:toggle:`) = 43 bytes overhead; option_id ≤ 20 bytes. Done format `<ask_id>:done` = 41 bytes (always within budget). Requests with a multi-select option_id exceeding 20 bytes MUST be rejected with JSON-RPC error `-32602`.
4. The validation function `validate_options_callback_data_budget(ask_id_len, format_overhead, max_option_id_len)` MUST be a standalone, unit-testable helper in `src/daemon/pending_asks.rs`.
5. The `chat_ask` tool's JSON schema description MUST document the per-option byte budgets so callers (Mira, other agents) see the constraint in the tool's `inputSchema.description` field.

#### §18.10.4 Non-Functional Requirements

1. **NFR-MAT-11.1 (Single-Bot-owner invariant preserved):** The `OUTBOUND_TG_KEYBOARD` channel MUST route all keyboard send requests through the existing teloxide `Bot` instance in `run_long_poll`. No additional `Bot::new` construction is permitted anywhere in the call path for `chat_ask`.
2. **NFR-MAT-11.2 (KP1–KP3 routing unchanged):** The addition of CallbackQuery dispatch in `process_batch_with_pairing` MUST NOT alter the routing logic for regular `Message` updates. The Slice 0 baseline wire shape for message inbound MUST be preserved bit-for-bit when `callback_query` is absent.
3. **NFR-MAT-11.3 (Daemon-restart resilience for pending asks):** Open `pending_asks` rows MUST survive a daemon restart. After restart, a tap on a keyboard sent before the restart MUST still produce a valid `<channel>` response — the daemon re-reads the `pending_asks` row from SQLite on receipt of the CallbackQuery without requiring in-memory state.
4. **NFR-MAT-11.4 (Security — ask_id unguessable):** `ask_id` MUST be a UUID v4 generated via a cryptographically-random source (not sequential, not predictable). An attacker who obtains a `chat_ask` keyboard URL cannot forge a response by guessing another `ask_id`. The debug tool `chat_list_pending_asks` lists ask_ids but this is behind the same SEC-7 whitelist as all other MCP tools.
5. **NFR-MAT-11.5 (Multi-select concurrency via SQLite serializability):** Concurrent taps on multi-select buttons from the same operator (network lag) MUST serialize correctly via SQLite's write-lock on the `pending_asks` row. No two tap handlers must read-modify-write `selected_values_json` outside a transaction.

#### §18.10.5 Acceptance Criteria

The QA-cycle MUST verify these three criteria with concrete live evidence (TC-CHA-1, TC-CHA-2, TC-CHA-3 from `docs/qa/multi-agent-telegram-on-v0.6_test_cases.md`):

1. **AC-MAT-CHA-1 (Single-select round-trip):** Mira calls `chat_ask` with `question="Which plan?"`, `options=[{label:"Plan A", value:"plan_a"}, {label:"Plan B", value:"plan_b"}, {label:"Plan C", value:"plan_c"}]`, `multi=false`. Operator's Telegram client shows an inline keyboard with 3 buttons. Operator taps "Plan B". The `<channel>` event arrives in Mira's CC with all of the following verified by evidence: `meta.is_callback_response=true`, `meta.value="plan_b"`, `meta.ask_id` (a UUID v4 string), `meta.question="Which plan?"`, `meta.options` (JSON array with 3 entries), `meta.multi=false`. Evidence: TC-CHA-1 screenshot `tc-cha-1-tg-buttons.png` (showing inline keyboard in TG before tap) + `tc-cha-1-channel-event.png` (showing terminal with channel event meta fields).
2. **AC-MAT-CHA-2 (Multi-select toggle round-trip):** Mira calls `chat_ask` with `multi=true` and 3 options (labels: "Option 1", "Option 2", "Option 3"; values: "opt_1", "opt_2", "opt_3"). Operator taps "Option 1" (✓ marker appears on button 1 in TG). Operator taps "Option 3" (✓ marker appears on button 3). Operator taps "Done". The `<channel>` event arrives in Mira's CC with `meta.values=["opt_1","opt_3"]`, `meta.multi=true`, `meta.question`, `meta.options`. Evidence: TC-CHA-3 screenshots `tc-cha-3-tg-toggle-after-tap1.png` (✓ on Option 1), `tc-cha-3-tg-toggle-after-tap3.png` (✓ on Option 1 and Option 3), `tc-cha-3-channel-event.png` (meta.values in terminal).
3. **AC-MAT-CHA-3 (Daemon-restart resilience):** Mira calls `chat_ask`. Daemon is restarted before operator taps. Operator taps a button after restart. The `<channel>` event arrives in Mira's CC with the correct `meta.value` / `meta.values`. Evidence: TC-CHA-7 daemon-log showing restart timestamp, then callback receipt timestamp after restart, then channel event in terminal.

All three cases are **Mixed** Verification Class (UI/UX + CLI + DB). Screenshots are OS-level (PowerShell `Get-Screenshot` / Snipping Tool on Windows; `screencapture` on macOS) because Playwright MCP cannot capture native Telegram Desktop. Telegram Web in a Playwright-driven browser is an acceptable alternate path.

#### §18.10.6 Affected Components (Slice 8 additions)

**New files:**

- `src/daemon/pending_asks.rs` — `pending_asks` table schema, CRUD helpers (`insert_pending`, `get_pending`, `update_selected_values`, `delete_pending`, `gc_expired`, `list_open`), `validate_options_callback_data_budget`.

**Modified files:**

- `src/daemon/telegram.rs` — `Update` struct extended with `#[serde(default)] callback_query: Option<CallbackQuery>`; new `CallbackQuery` and `MessageRef` structs; new `gate_callback` helper; `OUTBOUND_TG_KEYBOARD` `OnceLock<mpsc::UnboundedSender<...>>`; CallbackQuery dispatch branch in `process_batch_with_pairing`; `answerCallbackQuery` call; `editMessageReplyMarkup` for multi-select.
- `src/daemon/server.rs` — `"chat_ask"` and `"chat_list_pending_asks"` added to `tools/list` + dispatch; `handle_chat_ask` handler (validates options, generates ask_id, enqueues to `OUTBOUND_TG_KEYBOARD`, awaits `message_id` oneshot, inserts `pending_asks` row, returns `{ask_id, status:"pending"}`); `handle_chat_list_pending_asks` handler.
- `src/daemon/chat.rs` (or `src/daemon/migrations.rs`) — `apply_pending_asks_migration` called after `apply_routing_migration`.
- `src/plugin/mcp.rs` — `"chat_ask"` and `"chat_list_pending_asks"` added to `TOOL_WHITELIST` (SEC-7 gate).

**Unchanged (NFR-MAT-6):**

- `src/plugin/bridge.rs` — v0.6 692-line version preserved. The `target_agent_id` filter added in commit `0ba2c41` is already present and handles the AR-4 routing semantics without further modification.
- `plugins/telegram-rs/` — all plugin files unchanged.
- `src/plugin/mcp.rs` existing whitelist entries — additive only.

#### §18.10.7 Schema Changes (Slice 8 additions)

The `pending_asks` table is a new additive migration in `chat.db` (the existing `chat.db` file at the v0.6 path — same security perimeter as `agent_registry`). The migration is registered as `apply_pending_asks_migration` called from `chat::ensure_chat_db_schema` AFTER `apply_routing_migration` (sequential migration ordering). Full schema reproduced in FR-MAT-11.7.

The migration is idempotent — `CREATE TABLE IF NOT EXISTS` and `CREATE INDEX IF NOT EXISTS` guarantee re-running on an already-migrated `chat.db` is a no-op.

No existing tables or columns are modified. No existing indexes are dropped. The `agent_registry` schema added in §18.7 is unaffected.

#### §18.10.8 Risks and Dependencies

1. **R-MAT-10 (teloxide `Bot::answer_callback_query` symbol existence in v0.17).** The plan's architect pre-review (AR-1, AR-2) validated the Telegram Bot API surface conceptually. The implementer MUST verify that `teloxide::Bot::answer_callback_query(id)` and `teloxide::Bot::edit_message_reply_markup(chat_id, message_id, reply_markup)` exist with these exact signatures in teloxide 0.17 before coding. If either symbol is absent or has a different API shape: (a) check the `teloxide::requests::Requester` trait which may expose the method differently, (b) fall back to raw API calls via `teloxide::net::request_json`, (c) escalate to architect. Salience: high.
2. **R-MAT-11 (Telegram 15-second `answerCallbackQuery` deadline).** If the daemon is under load and CallbackQuery processing is delayed beyond ~15 seconds, the operator sees a permanent spinner on the tapped button even if the response eventually routes correctly. Mitigation: `answerCallbackQuery` is the FIRST action in the dispatch branch (FR-MAT-11.5) with no blocking work before it. The risk is daemon startup lag after a restart, not steady-state. Salience: medium.
3. **R-MAT-12 (Multi-select `editMessageReplyMarkup` 429 rate limit).** Rapid successive taps on a multi-select keyboard can trigger Telegram's rate limiter on `editMessageReplyMarkup`. Mitigation: AR-7 specifies retry-once after `retry_after`, then silent give-up. The SQLite row remains correct; only the keyboard display is stale. Salience: low.
4. **R-MAT-13 (CC compaction between `chat_ask` and callback response).** If Mira's context window compacts after calling `chat_ask`, the in-session memory of the ask is lost. Mitigation: AR-5 embeds `meta.question`, `meta.options`, `meta.ask_id` in the callback response so a compacted Mira can reconstruct context from the channel event alone. Salience: medium.
5. **R-MAT-14 (Dead originating agent at response time).** If the CC that called `chat_ask` has exited before the operator taps, the bridge filter would silently drop a `target_agent_id`-addressed notification. Mitigation: AR-4 fallback — when `requesting_agent_id` is not alive, omit `target_agent_id` and broadcast with `originating_agent_id` (informational). Salience: medium.

#### §18.10.9 AR-9 — Post-implementation live-fix amendment (2026-06-04)

**Status:** SHIPPED in commit `4a65819`. Supersedes FR-MAT-11.5 ¶2-3 and FR-MAT-11.6 ¶5 (meta-shape requirements only — Slice 8a/8b code paths, state machine semantics, send-then-insert ordering, AR-4 dead-agent fallback, and AR-7 atomic UPDATE...RETURNING are all unchanged).

**Discovery:** After Slice 8b deploy on 2026-06-04, operator's first multi-select probe (ask_id `1a2de8a1-...`) completed end-to-end on the daemon side — state machine ran, `pending_asks` row deleted, notification frame pushed to the broadcast bus, frame written to bridge UDS (684 bytes per daemon log at 22:40:18 UTC). But the `<channel>` event never surfaced in the requesting CC's session. Inbound TG messages on the same thread DID surface (304-byte frames), proving bus + UDS + bridge filter all worked. Difference between the two frames: meta payload shape.

**Root cause:** Claude Code's `<channel>` surface renderer **silently drops** any `notifications/claude/channel` frame whose `params.meta` carries keys outside the inbound-Telegram schema. The schema CC accepts is exactly:
- `chat_id` (string)
- `message_id` (string)
- `user` (string)
- `user_id` (string)
- `ts` (ISO 8601 string)
- `thread_id` (string, optional)
- `target_agent_id` (string, optional)

Adding extra keys (Slice 8a/8b's original `is_callback_response`, `ask_id`, `value`/`values`, `multi`, `question`, `options[]`, `originating_agent_id`) caused CC to drop the entire frame before reaching the LLM input stream. This matches the long-standing comment at `src/daemon/chat.rs:244-250` describing the strict-shape behavior of the inbound builder — the live-fix work confirmed the strictness also rejects extra keys, not just wrong types.

**Decision:** Move all Slice 8 round-trip semantic data from `params.meta` into `params.content` as a single-line parseable preamble at the start of the channel body. Mira (and any other downstream consumer) parses the bracketed prefix:

- Single-select: `[chat_ask kind=single ask_id=<uuid> value=<v>]`
- Multi-select Done: `[chat_ask kind=multi ask_id=<uuid> values=v1,v2,...]`
- Multi-select Done with zero selections: `[chat_ask kind=multi ask_id=<uuid> values=]` (trailing `=`, NOT `values=,`)

**FR-MAT-11.5 amendment (supersedes ¶2 and ¶3 meta keys):**

After `answerCallbackQuery`, the daemon emits a `notifications/claude/channel` event with:

- `params.content` = the preamble string above (single-line, kept first so it parses cleanly even when followed by other content).
- `params.meta.chat_id` = `pending_asks.chat_id` as STRING (v0.6 string discipline).
- `params.meta.message_id` = the CallbackQuery's `cb.message.message_id` if present, else `pending_asks.message_id`, as STRING.
- `params.meta.user` = `cb.from.username` if present, else `cb.from.id.to_string()`.
- `params.meta.user_id` = `cb.from.id.to_string()`.
- `params.meta.ts` = server-side `chrono::Utc::now().to_rfc3339_opts(SecondsFormat::Millis, true)` (CallbackQuery payloads do not carry a date field).
- `params.meta.thread_id` = `pending_asks.message_thread_id` as STRING when Some, OMITTED when None.
- `params.meta.target_agent_id` = AR-4 routing logic unchanged: present (string) iff `requesting_agent_id` is alive; OMITTED otherwise (frame becomes unaddressed broadcast).

`params.meta.originating_agent_id` is **removed** — the AR-4 dead-agent fallback no longer carries it as an informational breadcrumb in meta. The ask_id in the content preamble is the breadcrumb a fresh / compacted Mira uses to call `chat_list_pending_asks` for older context. Removing the meta key is what makes the frame surface; preserving it kept reproducing the silent drop.

**FR-MAT-11.6 amendment (supersedes ¶5 emit shape):**

Done-button finalization emits the SAME frame shape as FR-MAT-11.5 amended above, with the `kind=multi values=...` preamble form. No `meta.multi`, no `meta.values` array, no `meta.question`, no `meta.options` keys.

**AR-5 compaction-resilience revised:** Originally Slice 8 spec embedded `meta.question`, `meta.options`, `meta.ask_id` in the notification frame so a compacted Mira could reconstruct semantic context from the meta payload alone. AR-9 narrows this: only `ask_id` is preserved (in the content preamble). A compacted Mira reconstructs the question + options by calling `chat_list_pending_asks` filtered by `ask_id` — `pending_asks` row has already been deleted by Done, so this lookup returns empty; instead Mira must re-scan recent message history OR the operator re-states the question. Trade-off acknowledged.

**AC-MAT-CHA-1 / AC-MAT-CHA-2 / AC-MAT-CHA-3 amendment:** Evidence assertions on `meta.is_callback_response`, `meta.value`, `meta.values`, `meta.multi`, `meta.question`, `meta.options` are SUPERSEDED. New assertion: the `<channel>` body MUST contain the literal preamble substring matching the kind/ask_id/value(s) of the resolution. QA test cases `TC-CHA-1`, `TC-CHA-3`, `TC-CHA-7`, `TC-CHA-8` updated in sync — see `docs/qa/multi-agent-telegram-on-v0.6_test_cases.md` for new expected-result columns.

**Live-confirmed pass:** ask_id `3561a606-ae2f-46f2-aade-85a9887b25da` (multi-select) at 10:27 UTC + ask_id `7903663c-a65a-48f9-8858-9dfff662344c` (single-select) at 10:30 UTC, both surfacing in this Mira's session with the new content preamble.

**Maintenance constraint (for future slices):** ANY future feature that wants to embed structured semantic context in a `notifications/claude/channel` frame MUST use `params.content` as the channel — adding meta keys outside the inbound-Telegram schema is a silent drop. This is a CC-side constraint claudebase cannot relax without modifying CC's plugin code.

## Facts

### Verified facts

- `.claude/plan.md` Slice 8 block (lines 312–334), AR-1 through AR-8 blocks (lines 335–378), Slice 8 OQ resolutions (lines 380–384), Slice 8c block (lines 386–398) all read in full this session. Plan version v2, 2026-06-04 operator additions. Salience: high.
- `OUTBOUND_TG` OnceLock parallel pattern cited from plan AR-1 block (`.claude/plan.md:341`). New `OUTBOUND_TG_KEYBOARD` is the same pattern. Salience: high.
- `telegram.rs:936–944` cited as source of single-Bot-owner discipline — not read directly this session; cited via plan AR-1 block line 341. Salience: high (DEFERRED to Slice 8a architect pre-review for exact line verification).
- `bridge.rs:823–837` cited as the existing target_agent_id filter — commit `0ba2c41` (verified in git log this session). Salience: high.
- `gate_dm` at `telegram.rs:534` issues pairing codes — cited from plan AR-3 block line 349. Not verified by direct Read this session; cited via plan. Salience: medium.
- `process_batch_with_pairing` is the dispatch function name — cited from plan Slice 8 files-affected block line 321. Not verified by direct Read this session. Salience: medium.
- `chat::ensure_chat_db_schema` call at `chat.rs:343` — cited from plan AR-6 block line 374. Not verified by direct Read this session. Salience: medium.
- `apply_routing_migration` ordering constraint — cited from plan AR-6 block line 374. Not verified by direct Read this session. Salience: medium.
- Operator OQ resolution: urgency parameter = NO, `chat_list_pending_asks` = YES (Slice 8c). Source: `.claude/plan.md` lines 382–383. Salience: high.
- Insights corpus corrupt per spawn-prompt instruction — skipped silently per `~/.claude/rules/knowledge-base.md` § Fallback behavior. Salience: low.
- Corpus scope relevance: No overlap — task is pure software-feature specification; `<project>/.claude/knowledge/index.db` absent (this claudebase workspace). Corpus protocol silently bypassed. Salience: low.

### External contracts

- **Telegram Bot API `CallbackQuery` object** — symbol: required fields `id: String`, `from: User`, `chat_instance: String`; optional fields `data: Option<String>`, `message: Option<Message>`, `inline_message_id`, `game_short_name` — source: `https://core.telegram.org/bots/api#callbackquery` (AR-2 architect citation, `.claude/plan.md:347`) — verified: **no — assumption (cited by architect at AR-2; implementer MUST re-verify at impl time)** — salience: high.
- **Telegram Bot API `callback_data` 64-byte budget** — symbol: `InlineKeyboardButton.callback_data` field max 1–64 bytes — source: `https://core.telegram.org/bots/api#inlinekeyboardbutton` (AR-1 architect citation, `.claude/plan.md:345`) — verified: **no — assumption (architect-cited; implementer MUST verify at impl time)** — salience: high.
- **Telegram Bot API `answerCallbackQuery` ~15s deadline** — symbol: `answerCallbackQuery` must be called within ~15 seconds of CallbackQuery receipt — source: Telegram Bot API docs (AR-1 architect citation, `.claude/plan.md:343`) — verified: **no — assumption** — salience: high.
- **teloxide 0.17 `Bot::answer_callback_query`** — symbol: method name and signature — source: `teloxide` crate docs.rs v0.17 (NOT opened this session) — verified: **no — assumption (R-MAT-10; implementer MUST verify before coding)** — salience: high.
- **teloxide 0.17 `Bot::edit_message_reply_markup`** — symbol: method accepting `(chat_id, message_id, reply_markup)` — source: `teloxide` crate docs.rs v0.17 (NOT opened this session) — verified: **no — assumption (R-MAT-10)** — salience: high.

---

## §19. claudebase v0.9 cut — port-forward v0.7 insights surface

**Status:** [PLANNED]
**Date:** 2026-06-04
**Priority:** High
**Related:** §16 (Agent Insights Base — this section extends the insights corpus with schema v5, dual-DB routing, and tag vocabulary; the fundamental `insight create / search` CLI surface from §16 is extended, not replaced). §18 (Multi-Agent Telegram Routing on v0.6 Foundation — the 38 commits on `feat/multi-agent-on-v0.6` are the v0.9 baseline; this section documents the Wave A + Wave D additions on top of that baseline). Implementation plan: `.claude/plan.md` (v2, 265 lines, read in full this session). Product plan: `docs/plans/claudebase-v0.9-product-plan.md` (committed `15b9460`).

Changelog: Cross-session insights now work again — the insights corpus tracks categories and tags, survives migration from any prior schema version, and two new Claude Code hooks remind agents to query the corpus on every new context and every prompt.

### 19.1 Feature Description

v0.7 and v0.8 were declared broken by the operator after extended debugging (`docs/plans/claudebase-v0.9-product-plan.md` §1 — root cause not isolated; forward-debug cost exceeded rebuild cost). The `feat/multi-agent-on-v0.6` branch rebuilt the Telegram product surface from scratch on the v0.6 baseline, live-verifying each delta. That branch is now the v0.9 release vehicle. This PRD section covers the two waves of additional work required before v0.9 ships: **Wave A** (port-forward v0.7's insights corpus and operational tooling) and **Wave D** (release infrastructure).

Wave A is a code-reuse exercise, not a re-architecture. The v0.7 source code (commits `1161570`, `ff30d9f`, `c0eebca`, `2719e25`, `afddf71`, `cccef44`, `4bc9a9c`, `385efff`, `cb45b4d`, `0b92384`, `e43ca12`) is cherry-picked onto the v0.6+ baseline. The architect's pre-review focuses on compatibility with the 38 existing branch commits — not on re-deriving the v0.7 architecture which was already shipped upstream. Two BLOCKER files (`src/store.rs` wholesale rewrite and `src/cli.rs` breaking-change) require deliberate reconciliation; both are gated on architect pre-review before implementation.

The load-bearing constraint for this entire section is the **backward-compatibility MANDATE** (operator directive 2026-06-04): any `insights.db` on the operator's box that was created by a prior v0.6 installation MUST continue to function after v0.9's schema v5 migration — without data loss, without `error: index database invalid` lockouts, and without requiring `claudebase ingest --reset`. Silent data loss is rejected. The migration MUST either repair the corrupt state in place, or exit cleanly with the documented `repair-required` message and a recovery path.

Wave B (Telegram polish, KP2/KP3 live-evidence, bug #2 bridge reconnect, bug #8 daemon hang) and Wave C (multi-bot fleet, multi-bot long-poll, `/start` inline menu, `startproject`, `daemon setup`) are **deferred to v0.10** by operator decision 2026-06-04. The v0.6+ branch's KP1-verified Telegram work ships as-is in v0.9. The `CHANGELOG.md` entry MUST state that KP2/KP3 forum-topic routing is architecturally complete but live-evidence is pending v0.10.

### 19.2 User Stories

1. As the SDLC pipeline operator, I want `claudebase insight create --category project --tags v9-cut-smoke "Mira test"` to succeed so that agent insights persist across sessions without the `error: index database invalid` lockout that blocks cross-session learning today.
2. As the SDLC pipeline operator, I want `claudebase insight search "Mira test" --tag v9-cut-smoke --salience high --top-k 3 --json` to return the insight I just created so that agents can retrieve prior-session observations by tag.
3. As the SDLC pipeline operator, I want `claudebase insight tags --json` to return a merged tag vocabulary from both the local project DB and the global DB so that I can browse and filter the corpus across projects.
4. As the SDLC pipeline operator, opening a new Claude Code session should automatically remind me to query the insights corpus so that I do not start sessions cold when prior-session observations are available.
5. As the SDLC pipeline operator, submitting a prompt to Claude Code should inject a short cognitive self-check reminder so that agents habitually run the three protocols without me prompting them.
6. As the SDLC pipeline operator, I want `claudebase run` to register the current project in `~/.claude/knowledge/projects.json` so that `insight search --project <slug>` can locate the project's local insights DB without me specifying a path.
7. As the SDLC pipeline operator, I want `/update-claudebase` to update the installed binary and restart the daemon in one step so that I never have to manually stop the daemon, replace the binary, and restart it.
8. As the SDLC pipeline operator with an existing `insights.db` from an older claudebase install, I want v0.9's schema v5 migration to preserve all my existing rows with correct `category` and `project_slug` backfill so that no historical observations are lost.
9. As the SDLC pipeline operator, if my `insights.db` is in a corrupt state, I want v0.9 to exit with a clear `repair-required` message and a documented recovery path rather than silently corrupting data.

### 19.3 Functional Requirements

#### FR-V9-1: Schema v5 Migration + Global Resolver (Slice 1)

1. **FR-V9-1.1 (schema v5 delta):** `src/store.rs` MUST apply the following additive schema delta when `open_or_init` is called against any insights DB at schema versions v1 through v4:
   - `ALTER TABLE documents ADD COLUMN category TEXT NOT NULL DEFAULT 'project';`
   - `ALTER TABLE documents ADD COLUMN project_slug TEXT;`
   - `CREATE TABLE insight_tags(doc_id INTEGER NOT NULL, tag TEXT NOT NULL, PRIMARY KEY(doc_id, tag), FOREIGN KEY(doc_id) REFERENCES documents(id) ON DELETE CASCADE);`
   - `CREATE INDEX insight_tags_tag_idx ON insight_tags(tag);`
2. **FR-V9-1.2 (4-path migration semantics):** The migration MUST converge all four entry paths to schema v5: v1→v5, v2→v5, v3→v5, v4→v5. Each path applies the delta via `apply_v5_delta_and_backfill(tx, db_path)`. Schema version is updated to 5 after successful migration. Four synthetic fixture DBs (`tests/fixtures/synthetic-v{1,2,3,4}.db`, 3 rows each with known payload) MUST all pass migration with row-count preservation + `category`/`project_slug` backfill correct.
3. **FR-V9-1.3 (v4→v5 backfill for agent rows):** After the schema DDL is applied, existing agent rows MUST be backfilled: `category = 'project'`, `project_slug` derived from `feature_slug` column (if present), a default tag inserted in `insight_tags` (one row per agent document using `feature_slug` as the tag value, or `'untagged'` when `feature_slug` is NULL). Books-corpus rows MUST remain untouched.
4. **FR-V9-1.4 (backward-compat repair-required exit):** When the migration cannot repair a DB (schema version unrecognised, or structural corruption detected by `validate_schema_inner()` on versions `1..=5`), the binary MUST exit 1 with the literal stderr line `error: index database invalid; run \`claudebase ingest --reset\` to recover`. MUST NOT silently corrupt data. MUST NOT proceed with a partially-migrated DB.
5. **FR-V9-1.5 (global resolver):** A new public function `resolve_global_insights_path() -> PathBuf` MUST return `~/.claude/knowledge/insights.db`. A corresponding `open_global_insights_db() -> Option<Connection>` MUST attempt to open that path; when the global DB is missing or unopenable, it MUST return `None` without error (the caller falls back to local-only operation).

#### FR-V9-2: `insight create` Breaking-Change CLI Contract (Slices 2a + 2b)

1. **FR-V9-2.1 (required flags):** `InsightCreateArgs` MUST add two REQUIRED flags: `--category <general|project>` (enum; exits 2 if absent) and `--tags <comma-separated>` (one or more tags; exits 2 if absent).
2. **FR-V9-2.2 (optional project flag):** An optional `--project <slug>` flag MUST be added to allow explicit project scoping when the caller is not in the project's cwd.
3. **FR-V9-2.3 (tag normalisation):** Tag values MUST be normalised before storage: strip leading `#`, lowercase, trim whitespace, deduplicate within the call. Example: `"#v9-cut-smoke, V9-cut-smoke, v9-cut-smoke "` normalises to a single tag `"v9-cut-smoke"`.
4. **FR-V9-2.4 (dual-DB write routing):** `--category general` MUST write the insight to `~/.claude/knowledge/insights.db` (global DB). `--category project` MUST write to `<cwd>/.claude/knowledge/insights.db` (local DB). Existing exact-sha and semantic dedup MUST be preserved per-DB independently.
5. **FR-V9-2.5 (tag row writes):** After inserting the document row, the handler MUST insert one row per normalised tag into `insight_tags(doc_id, tag)`. The insert MUST use `INSERT OR IGNORE` to be idempotent.

#### FR-V9-3: `insight search` Dual-DB Read + Tag/Category/Project Filters (Slices 3a + 3b)

1. **FR-V9-3.1 (`rrf_fuse_hits` function):** A new `pub fn rrf_fuse_hits(local: Vec<SearchHit>, general: Vec<SearchHit>, top_k: usize) -> Vec<SearchHit>` MUST be added to `src/search.rs`. The function fuses hits keyed on `(source_corpus, chunk_id)` using Reciprocal Rank Fusion (k=60). It MUST handle the case where the same `chunk_id` appears in both lists without panicking.
2. **FR-V9-3.2 (dual-DB read):** `insight search` MUST query both the local DB and the global DB (if present), fuse results via `rrf_fuse_hits`, and return the top-K fused list. When the global DB is absent or corrupt, the search MUST fall back to local-only with a stderr warning — it MUST NOT exit 1.
3. **FR-V9-3.3 (tag filter — parameterised SQL only):** The `--tag <t>` filter (repeatable, OR-semantics) MUST be implemented as a parameterised SQL `WHERE chunk_id IN (SELECT doc_id FROM insight_tags WHERE tag IN (?,?,...))` with bound parameters. Building the filter via `format!()` string interpolation is PROHIBITED — any such pattern is a SQL injection vector and a security violation.
4. **FR-V9-3.4 (additional filters):** The following optional filters MUST be added to `insight search`, `insight list`, `insight random`: `--tag <repeatable>`, `--category <general|project>`, `--project <slug>`. The `insight gc` and `insight delete` subcommands MUST also gain `--category`.
5. **FR-V9-3.5 (existing regression safety):** All 11 existing `cli_search_e2e` tests MUST continue to pass after the dual-DB changes.

#### FR-V9-4: `insight tags` Subcommand (Slice 4)

1. **FR-V9-4.1 (new subcommand):** A new `claudebase insight tags` subcommand MUST be added with `InsightTagsArgs` parsed by `src/cli.rs` and dispatched to `run_insight_tags()` in `src/main.rs`.
2. **FR-V9-4.2 (merged semantics):** "Merged" output MUST mean: union of `(tag, count)` pairs from local DB and global DB, deduplicated by exact tag string, with `count` = sum of per-DB counts for that tag. Sort order MUST be `count DESC, tag ASC` (deterministic tie-breaking on alphabetical tag).
3. **FR-V9-4.3 (missing global DB — no error):** When the global DB path does not exist, the global contribution is zero across all tags. The subcommand MUST NOT materialise an empty DB file, MUST NOT exit 1, and MUST NOT emit a warning — it silently omits the global contribution.
4. **FR-V9-4.4 (JSON output shape):** `--json` MUST output a JSON array of `{"tag": string, "count": integer}` objects in the specified sort order.

#### FR-V9-5: Project Registry (Slice 5)

1. **FR-V9-5.1 (new file):** `src/registry.rs` MUST be added to the crate, declared via `pub mod registry;` in `src/lib.rs`.
2. **FR-V9-5.2 (`upsert_project` semantics):** `pub fn upsert_project(root: &Path) -> Result<(), String>` MUST write or update a `ProjectEntry` for the given path in `~/.claude/knowledge/projects.json`. The `project_slug` MUST be the canonicalised basename of `root` — never derived from user-supplied input. Writes MUST be atomic: write to a temp file in the same directory, then rename (avoids partial-write corruption).
3. **FR-V9-5.3 (`resolve_project_path` API):** `pub fn resolve_project_path(slug: &str) -> Option<PathBuf>` MUST read `projects.json` and return the `path` for the entry whose slug matches, or `None` if not found.
4. **FR-V9-5.4 (`claudebase run` integration):** The `run_claude_with_preset` function in `src/main.rs` MUST call `upsert_project(&cwd)` as its first action, before spawning the child process. The call MUST be non-fatal — a registry write failure MUST log a warning and continue; it MUST NOT prevent `claudebase run` from launching the CLI.
5. **FR-V9-5.5 (reconciliation with per-project config):** The project registry is the cross-cutting index of all projects. The per-project `.claudebase/config.json` (added in commit `25189bc`) is the per-project source of truth for `session_id` and `name`. These two files MUST NOT conflict; `upsert_project` reads neither `.claudebase/config.json` nor the global registry — it derives the slug from the filesystem path only.

#### FR-V9-6: UserPromptSubmit Self-Check Hook (Slices 6a + 6b + 6c)

1. **FR-V9-6.1 (hook scripts):** Two new hook scripts MUST be created: `hooks/claudebase-selfcheck-reminder.sh` and `hooks/claudebase-selfcheck-reminder.ps1`. Both MUST emit valid JSON `{"hookSpecificOutput": {"additionalContext": "..."}}` on stdout when invoked.
2. **FR-V9-6.2 (ASCII-only PowerShell constraint):** All `.ps1` hook files shipped by this feature MUST contain ONLY ASCII bytes (codepoint ≤ 127). Non-ASCII glyphs (emoji, em-dashes, curly quotes, bullets) are PROHIBITED in `.ps1` files. Verification: `(Get-Content $f -Encoding Byte | Where-Object { $_ -gt 127 }).Count -eq 0` MUST return `True` for every `.ps1` hook file. This constraint exists because Windows PowerShell 5.1 parses no-BOM scripts in the local code page (not UTF-8); multi-byte UTF-8 sequences corrupt string literals.
3. **FR-V9-6.3 (installer wiring — idempotent):** `install.sh` and `install.ps1` MUST wire both `.sh` and `.ps1` hooks into `~/.claude/settings.json` under `hooks.UserPromptSubmit`. Wiring MUST use dedup-by-command-string equality: the installer reads the existing array, extracts the `command` field from each entry, and skips appending if a matching `command` string is already present. Running the installer twice MUST produce zero new entries in the second run.
4. **FR-V9-6.4 (`prompts/` repo-shipped directory):** The `prompts/rules/` directory IS the repo-shipped source for rule files. The installer MUST deploy these files to `~/.claude/rules/` using `cp -n` (shell) or `Copy-Item -ErrorAction SilentlyContinue` (PowerShell) so that operator-customised files in `~/.claude/rules/` are NOT overwritten.
5. **FR-V9-6.5 (modification to `claudebase-insight-capture` hooks):** If `hooks/claudebase-insight-capture.{sh,ps1}` already exist on the branch, MUST apply the compact-reason update from commit `0b92384` and the ASCII-only fix from `e43ca12`. If they do not exist, MUST create them as new files ported from v0.7.

#### FR-V9-7: SessionStart Read-Insights-on-New-Context Hook (Slice 7)

1. **FR-V9-7.1 (hook scripts):** Two new hook scripts MUST be created: `hooks/claudebase-read-insights-reminder.sh` and `hooks/claudebase-read-insights-reminder.ps1`. Both MUST be ASCII-only (same constraint as FR-V9-6.2). Both MUST emit JSON with `additionalContext` containing the literal substring `claudebase insight tags`.
2. **FR-V9-7.2 (installer wiring — idempotent):** `install.sh` and `install.ps1` MUST wire both hooks into `~/.claude/settings.json` under `hooks.SessionStart` using the same dedup-by-command-string pattern as FR-V9-6.3. Running the installer twice MUST produce zero new entries in the second run.
3. **FR-V9-7.3 (firing condition):** The hook fires on session `start`, `resume`, and post-compact events. This is the standard `SessionStart` event type; no special-casing is required by the hook scripts themselves.

#### FR-V9-8: `/update-claudebase` Skill + Daemon-State Preservation (Slice 8)

1. **FR-V9-8.1 (skill file):** `prompts/commands/update-claudebase.md` MUST be created, containing a slash-command specification that instructs the agent to update the claudebase binary by reading the project's README first and running the appropriate install path.
2. **FR-V9-8.2 (daemon-state preservation — 6-step contract):** The skill MUST instruct the agent to follow this exact sequence:
   - Step 1: Run `claudebase daemon status --json` and capture the pre-update PID.
   - Step 2: Verify that the fresh binary's version is GREATER than the running version (refuse downgrade).
   - Step 3: Stop the daemon via `claudebase daemon stop`.
   - Step 4: Replace the binary atomically (write to a temp path, then rename to final path).
   - Step 5: Restart the daemon via `claudebase daemon start`.
   - Step 6: Verify the new PID is different from the pre-update PID AND `claudebase daemon status` returns `running`.
3. **FR-V9-8.3 (reads-README-first discipline):** The skill MUST instruct the agent to fetch and read the project README before executing any install command, so the install path used by the skill matches the README's documented install one-liner and never drifts from it.
4. **FR-V9-8.4 (no downgrade):** The skill MUST abort with an error message if the fresh binary version is ≤ the currently-running version. It MUST NOT replace the binary in that case.

#### FR-V9-9: `prompts/` Repo-Shipped Directory + Installer Deploy (Slice 6a)

1. **FR-V9-9.1 (repo structure):** The `prompts/` directory MUST exist at the repo root after this feature lands. Subdirectory `prompts/rules/` MUST contain at minimum: `cognitive-self-check.md`, `knowledge-base.md`, `knowledge-base-tool.md`, `tool-limitations.md` (ported from v0.7 commit `cb45b4d`).
2. **FR-V9-9.2 (content verification):** Content of these files MUST be byte-identical to their v0.7 counterparts (verified via `git show cb45b4d:<path>` diff at implementation time).
3. **FR-V9-9.3 (installer deploy — no-clobber):** The installer MUST deploy `prompts/rules/*` → `~/.claude/rules/` and `prompts/commands/*` → `~/.claude/commands/` using no-clobber semantics (`cp -n` on shell; `Copy-Item -ErrorAction SilentlyContinue` on PowerShell). Existing operator customisations MUST NOT be overwritten.

### 19.4 Non-Functional Requirements

1. **NFR-V9-1 (backward-compat MANDATE — no silent data loss):** The schema v5 migration MUST NOT silently discard or corrupt any row that exists in an insights DB at any prior schema version (v1 through v4). This is an absolute constraint. A migration that silently loses data is a CRITICAL defect regardless of test coverage. The only permitted data-loss outcomes are (a) the explicit `claudebase ingest --reset` path chosen by the operator, or (b) GC of expired low/medium-salience rows. — salience: high.
2. **NFR-V9-2 (ASCII-only `.ps1` hooks for Windows PowerShell 5.1):** Every `.ps1` file shipped by this feature MUST contain only ASCII bytes. Windows PowerShell 5.1 on operator's box (Windows 11 Home 10.0.26200) parses no-BOM scripts in the local ANSI code page, not UTF-8. Multi-byte UTF-8 sequences corrupt string literals — this caused a production breakage in v0.7 (commit `e43ca12` was the fix). — salience: high.
3. **NFR-V9-3 (installer idempotency):** Running `install.sh --yes` or `install.ps1` more than once MUST be a no-op with respect to `~/.claude/settings.json` hook entries and `~/.claude/rules/` file state. No duplicate hook entries. No overwritten operator customisations. — salience: medium.
4. **NFR-V9-4 (parameterised SQL — no injection vector):** Any SQL filter that incorporates user-supplied tag strings MUST use bound parameters (`?, ?, ...` placeholders). Building a SQL string via `format!()` or string concatenation with user input is PROHIBITED. This applies specifically to the `WHERE tag IN (?,?,...)` pattern in `insight search` and `insight tags`. — salience: high.
5. **NFR-V9-5 (no new Cargo dependencies):** Wave A MUST introduce zero new entries in `Cargo.toml`. The implementation uses only existing dependencies (`rusqlite`, `serde_json`, `std::fs`). — salience: medium.

### 19.5 Acceptance Criteria

These four criteria are the v0.9 release gate. All must pass before `/merge-ready` is invoked.

| ID | Criterion | Evidence Required |
|---|---|---|
| **AC-V9-1** | `claudebase insight create --category project --tags v9-cut-smoke --salience high "Mira test"` succeeds on operator's box AND writes a row to the cwd-local `insights.db`. | Shell stdout (exit 0) + SQL `SELECT count(*) FROM documents WHERE source_type LIKE 'agent-%'` returns ≥1 row after the call. Captured in `docs/qa/evidence/claudebase-v0.9/AC-V9-1-create-stdout.txt` and `AC-V9-1-sql-count.txt`. |
| **AC-V9-2** | After AC-V9-1's row is inserted, `claudebase insight search "Mira test" --tag v9-cut-smoke --salience high --top-k 3 --json` exits 0 AND the JSON array contains the row inserted by AC-V9-1. | Shell stdout MUST be a non-empty JSON array; one element MUST contain the body substring `Mira test` AND `salience: "high"`; exit code MUST be 0; stderr MUST NOT contain `error: index database invalid`. Captured in `docs/qa/evidence/claudebase-v0.9/AC-V9-2-search-stdout.json`. |
| **AC-V9-3** | `claudebase insight tags --json` returns the merged tag vocabulary from cwd-local + global DBs, where merged means: union by tag, count = sum across DBs, sorted by `count DESC, tag ASC`. | JSON array of `{"tag": string, "count": integer}` objects; MUST contain `{"tag": "v9-cut-smoke", "count": 1}` after AC-V9-1's row is inserted; sort order verified via jq assertion. Captured in `docs/qa/evidence/claudebase-v0.9/AC-V9-3-tags-stdout.json` and `AC-V9-3-merged-semantics-assertion.txt`. |
| **AC-V9-4** | `claudebase --version` reports `0.9.0` after running `/update-claudebase` skill AND the `claudebase-v0.9.0` git tag exists with a GitHub release attached. | `git tag --list 'claudebase-v0.9.0'` returns the tag; `gh release view claudebase-v0.9.0` shows a non-draft release with Linux + macOS + Windows binary assets. |

**Backward-compat MANDATE:** AC-V9-1 and AC-V9-2 MUST also pass against operator's existing `insights.db` at `C:\Users\madwh\.claude\knowledge\insights.db` in its current (corrupt) state. The migration MUST either (a) repair the DB in place with rows preserved, OR (b) exit with the literal stderr `error: index database invalid; run \`claudebase ingest --reset\` to recover`. Evidence captured in `docs/qa/evidence/claudebase-v0.9/TC-V9-5-pre-migration-rows.txt` and `TC-V9-5-post-migration-rows.txt`.

### 19.6 Affected Components

**Modified (port-forward — v0.7 source taken wholesale or near-wholesale):**

- `src/store.rs` — BLOCKER: full rewrite of `open_or_init_v2` to the 4-path v1→v5 / v2→v5 / v3→v5 / v4→v5 converging migration (sourced from v0.7 commit `1161570`).
- `src/cli.rs` — BLOCKER: `InsightCreateArgs` gains required `--category {general|project}` enum and required `--tags <comma-separated>` Vec<String>; new `InsightTagsArgs` + `Tags` enum variant added to `InsightSubcommand`; additional filter flags on `InsightSearchArgs`, `InsightListArgs`, `InsightRandomArgs` (sourced from v0.7 commits `c0eebca` + `afddf71` + `2719e25`).
- `src/main.rs` — `upsert_project(&cwd)` call at top of `run_claude_with_preset`; new match arm `InsightSubcommand::Tags(a) => run_insight_tags(&a)`; dual-DB call sites for search/list/random/gc/delete.
- `src/lib.rs` — `pub mod registry;` alphabetic insert.
- `src/search.rs` — `pub fn rrf_fuse_hits(...)` extraction; `rrf_fuse_corpora()` refactored to 7-line wrapper; dual-DB search call sites (sourced from v0.7 commit `afddf71`).
- `install.sh` + `install.ps1` — hook-wiring additions for UserPromptSubmit + SessionStart hooks; `prompts/` → `~/.claude/` deploy step.
- `README.md` — `/update-claudebase` skill banner update.
- `CHANGELOG.md` — NEW at repo root; `[0.9.0]` block with v0.6+ Telegram work summary + Wave A port-forward summary + KP2/KP3 deferred-evidence note.
- `Cargo.toml` — version bump `0.6.0` → `0.9.0`.
- `Cargo.lock` — regen after version bump.

**Created:**

- `src/registry.rs` — project registry (new file, sourced from v0.7 commit `cccef44`).
- `hooks/claudebase-selfcheck-reminder.sh` + `hooks/claudebase-selfcheck-reminder.ps1`
- `hooks/claudebase-read-insights-reminder.sh` + `hooks/claudebase-read-insights-reminder.ps1`
- `hooks/claudebase-insight-capture.sh` + `hooks/claudebase-insight-capture.ps1` (if not already on branch; else modified)
- `prompts/rules/cognitive-self-check.md`
- `prompts/rules/knowledge-base.md`
- `prompts/rules/knowledge-base-tool.md`
- `prompts/rules/tool-limitations.md`
- `prompts/commands/update-claudebase.md`
- `tests/store_v5_test.rs` + `tests/store_global_resolver_test.rs` + `tests/fixtures/synthetic-v{1,2,3,4}.db`
- `tests/cli_insight_create_args_test.rs` + `tests/cli_insight_create_routing_test.rs`
- `tests/rrf_fuse_hits_test.rs` + `tests/cli_insight_dual_db_test.rs` + `tests/cli_insight_tags_test.rs`
- `tests/registry_test.rs`
- `docs/use-cases/claudebase-v0.9-cut_use_cases.md`
- `docs/qa/claudebase-v0.9-cut_test_cases.md`
- `docs/qa/evidence/claudebase-v0.9/` (evidence directory populated by Slice 10)
- `.github/workflows/release.yml` (if missing)

**Preserved bit-for-bit (frozen — operator directive 2026-06-04):**

- All `src/daemon/*` files modified by the 38 v0.6+ branch commits.
- `src/plugin/bridge.rs` (target_agent_id filter from commit `0ba2c41`).
- `src/plugin/mcp.rs` (TOOL_WHITELIST with `chat_ask` + `chat_list_pending_asks`).
- `src/project_config.rs` (`.claudebase/config.json` from commit `25189bc`).
- `docs/PRD.md` §18 + §18.10 (multi-agent TG + Slice 8 AR-9 amendment).

### 19.7 Schema Changes

The following SQL DDL constitutes the complete v5 schema delta applied by `apply_v5_delta_and_backfill(tx, db_path)` in `src/store.rs`:

```sql
-- Column additions to documents table (applied via ALTER TABLE — additive)
ALTER TABLE documents ADD COLUMN category TEXT NOT NULL DEFAULT 'project';
ALTER TABLE documents ADD COLUMN project_slug TEXT;

-- New tags table (ON DELETE CASCADE from documents.id)
CREATE TABLE IF NOT EXISTS insight_tags (
    doc_id  INTEGER NOT NULL,
    tag     TEXT    NOT NULL,
    PRIMARY KEY (doc_id, tag),
    FOREIGN KEY (doc_id) REFERENCES documents(id) ON DELETE CASCADE
);

-- Index on tag column for tag-filter queries
CREATE INDEX IF NOT EXISTS insight_tags_tag_idx ON insight_tags(tag);
```

Backfill SQL semantics (executed within the same transaction as the DDL):

```sql
-- Backfill category for all existing rows (agent rows inherit 'project')
UPDATE documents SET category = 'project' WHERE category IS NULL OR category = '';

-- Backfill project_slug from feature_slug where available
UPDATE documents SET project_slug = feature_slug
WHERE feature_slug IS NOT NULL AND feature_slug != '';

-- Insert one default tag per agent document using feature_slug as tag
-- (rows whose feature_slug IS NULL receive tag 'untagged')
INSERT OR IGNORE INTO insight_tags (doc_id, tag)
SELECT id, COALESCE(feature_slug, 'untagged')
FROM documents
WHERE source_type LIKE 'agent-%';
```

These migrations are idempotent: the `ALTER TABLE` statements are guarded by `PRAGMA table_info` column-existence checks before execution; `CREATE TABLE IF NOT EXISTS` and `CREATE INDEX IF NOT EXISTS` are unconditionally idempotent; the `INSERT OR IGNORE` is idempotent by the primary key constraint.

This schema change applies to `insights.db` files only (both local `<project>/.claude/knowledge/insights.db` and global `~/.claude/knowledge/insights.db`). The `index.db` books corpus, `chat.db`, and `claudebase.db` are unaffected.

### 19.8 Risks and Dependencies

**R-V9-CUT-1 (BLOCKER #1: `src/store.rs` open_or_init_v2 wholesale rewrite).** v0.7's migration logic replaces the v4-only init code with a 4-path v1→v5 converging migration. A line-by-line cherry-pick will not apply cleanly against the 38 v0.6+ branch commits that touch `store.rs`. The mitigation is to take v0.7's `store.rs` wholesale at Slice 1, immediately verify against a copy of operator's actual `insights.db` (NOT a synthetic test DB) as part of the Slice 1 done-condition. The backward-compat MANDATE is satisfied by this verification, not by code-reading the migration body. Salience: high.

**R-V9-CUT-2 (BLOCKER #2: `src/cli.rs` InsightCreateArgs breaking-change).** v0.7 makes `--category` and `--tags` REQUIRED flags. Operator's SDLC config already mandates these flags via UserPromptSubmit hook reminders every prompt turn — the deployed binary is behind the contract agents already expect. The mitigation is that Slice 2 ships in the same release as Slices 6-7 (the hook scripts), so the binary and the hooks align simultaneously. No ecosystem-wide breakage window exists. Salience: high.

**R-V9-CUT-3 (Operator's current `insights.db` is corrupt).** The current state returns `error: index database invalid; re-ingest required` on every `insight create` call (followup #7 from `docs/plans/claudebase-v0.9-product-plan.md` §2). The schema version on disk is unknown without a binary that handles the invalid state. The mitigation is Slice 1's done-condition: the implementer snapshots operator's actual DB to `tests/fixtures/operator-db-snapshot.db` (gitignored, local-only), runs the v0.9 migration against the snapshot, and verifies either (a) rows are preserved + tag/category backfill is correct, OR (b) the literal repair-required stderr message is emitted + exit code is 1. Silent data loss fails the done-condition. Salience: high.

**R-V9-CUT-4 (install.ps1 hook-wiring reconciliation with our `a615d9c` Start-Process daemon block).** The Explore agent #2 verdict on this risk was ADDITIVE: v0.7's hook-wiring additions land in a separate section of `install.ps1` BEFORE the daemon-spawn block; there is no overlap. The mitigation is the Slice 6c implementer applying v0.7's hook-wiring patch on top of the current `install.ps1` and verifying the post-install daemon-spawn block still runs end-to-end. Salience: medium.

**R-V9-CUT-5 (Schema-version assumption — operator's DB might be at v1, not v4).** The v0.6 baseline initialises fresh DBs at schema v4. If the operator's DB pre-dates that install, it could be at v1, v2, or v3. The v0.7 migration handles all four paths to v5, so this is not a real risk — only a verification step in Slice 1. The done-condition's synthetic-fixture pass (which exercises all four entry paths) covers this. Salience: low.

**R-V9-CUT-6 (Wave A code-REUSE directive must not be interpreted as blind cherry-pick).** The operator's directive says "code REUSE, NOT a re-architecture." This means using v0.7's source code as the implementation, not re-deriving the design. However, R-V9-CUT-1 and R-V9-CUT-2 BOTH require deliberate reconciliation — the v0.7 commits cannot be applied mechanically without considering the 38 v0.6+ branch commits. The mitigation is that both BLOCKER slices (1 and 2a) spawn `architect` pre-review gates, so reconciliation is supervised. Salience: medium.

**R-V9-CUT-7 (KP2/KP3 live evidence is pending and ships in v0.9 unverified).** Operator accepted this scope trade-off in the 2026-06-04 scope-cut decision: KP1 LIVE-verified is good enough for v0.9; KP2/KP3 require operator to set up a group with forum topics and 3 CC sessions, which is deferred to v0.10. The mitigation is that the `CHANGELOG.md` `[0.9.0]` entry MUST explicitly state "KP2/KP3 forum-topic routing architecturally complete; live-evidence capture deferred to v0.10." Salience: medium.

## Facts

### Verified facts

- `.claude/plan.md` lines 1–265 read in full this session — source: Read tool call this session — salience: high.
- `docs/plans/claudebase-v0.9-product-plan.md` lines 1–493 read in full this session — source: Read tool call this session — salience: high.
- `docs/PRD.md` last top-level section is §18 (confirmed via Grep on `^## §` pattern this session); §19 is the next available section number — source: Grep output `## §18. Multi-Agent Telegram Routing...` at line 783 this session — salience: high.
- PRD file ends at line 1299 (last content line) per Read offset=1290 limit=10 call this session — source: Read tool call this session — salience: medium.
- v5 schema delta identifiers (column names, table name, FK constraint, index name) copied verbatim from `.claude/plan.md` §External contracts entry — source: plan.md lines 192–197 this session — salience: high.
- 4 success criteria (AC-V9-1 through AC-V9-4) copied verbatim from `.claude/plan.md` §Success Criteria table — source: plan.md lines 22–31 this session — salience: high.
- 7 risks (R-V9-CUT-1 through R-V9-CUT-7) ported from `.claude/plan.md` §Risks & Dependencies — source: plan.md lines 141–149 this session — salience: high.
- Files Likely Affected list (Modified / Created / Preserved-bit-for-bit) ported from `.claude/plan.md` lines 108–139 this session — salience: medium.
- The `--with-resources` and role-planner steps are NOT in scope for this PRD-writer invocation (prd-writer only) — source: task prompt from orchestrator this session — salience: low.
- Knowledge base `index.db` exists at `<project>/.claude/knowledge/index.db` but contains 0 documents (verified via `claudebase status --json --project-root` call this session). Corpus scope relevance: No overlap — task is software-feature specification; no domain books are indexed. Topical queries silently skipped per `~/.claude/rules/knowledge-base-tool.md` § Corpus scope relevance protocol. — salience: low.
- Insights corpus (`insights.db`) absent in this project workspace; `insight search` queries skipped silently per activation-sentinel rule — salience: low.

### External contracts

- **v5 schema identifiers (commit `1161570` via Explore agent #1 cited in `.claude/plan.md:192–197`):**
  - column `documents.category TEXT NOT NULL DEFAULT 'project'` — source: `.claude/plan.md:197` — verified: yes (plan.md read this session) — salience: high.
  - column `documents.project_slug TEXT` — source: `.claude/plan.md:197` — verified: yes — salience: high.
  - table `insight_tags(doc_id INTEGER NOT NULL, tag TEXT NOT NULL, PRIMARY KEY(doc_id, tag), FOREIGN KEY(doc_id) REFERENCES documents(id) ON DELETE CASCADE)` — source: `.claude/plan.md:197` — verified: yes — salience: high.
  - index `insight_tags_tag_idx ON insight_tags(tag)` — source: `.claude/plan.md:197` — verified: yes — salience: high.
- **v0.7 `apply_v5_delta_and_backfill(tx, db_path)` symbol** — source: `.claude/plan.md:192` (Explore agent #1 citation of commit `1161570`) — verified: yes (plan.md read this session) — salience: high.
- **v0.7 `validate_schema_inner()` extended to `1..=5`** — source: `.claude/plan.md:192` — verified: yes — salience: high.
- **v0.7 `InsightCreateArgs` REQUIRED `--category <general|project>` + REQUIRED `--tags <comma-separated>` + optional `--project <slug>`** — source: `.clone/plan.md:193` (Explore agent #2 citation of commit `c0eebca`) — verified: yes — salience: high.
- **v0.7 `src/registry.rs` API symbols:** `pub struct ProjectEntry`, `pub fn upsert_project(root: &Path) -> Result<(), String>`, `pub fn resolve_project_path(slug: &str) -> Option<PathBuf>` — source: `.claude/plan.md:194` (Explore agent #1 citation of commit `cccef44`) — verified: yes — salience: high.
- **v0.7 hook script paths:** `hooks/claudebase-selfcheck-reminder.{sh,ps1}`, `hooks/claudebase-read-insights-reminder.{sh,ps1}`, `hooks/claudebase-insight-capture.{sh,ps1}` — source: `.claude/plan.md:195` (commits `cb45b4d` + `385efff` + `0b92384` + `e43ca12`) — verified: yes — salience: medium.
- **v0.7 `/update-claudebase` skill spec** — symbol: skill name `update-claudebase`, protocol "read README → execute install → report version delta" — source: `.claude/plan.md:196` (commit `4bc9a9c`) — verified: yes — salience: medium.
- **SQLite `INSERT OR IGNORE`** — symbol: `INSERT OR IGNORE INTO insight_tags(doc_id, tag) ...` — standard SQLite conflict-resolution clause — source: standard SQLite docs; idempotency via PRIMARY KEY constraint — verified: yes (standard SQL, no version dependency) — salience: medium.
- **`rrf_fuse_hits` keyed on `(source_corpus, chunk_id)`** — source: `.claude/plan.md:80` (Slice 3a done-condition) — verified: yes — salience: high.

### Assumptions

- The backfill SQL `COALESCE(feature_slug, 'untagged')` correctly handles the case where `feature_slug` column exists in the v1–v4 schema but some rows have NULL values. Risk: if `feature_slug` column does not exist in v1 schema, the backfill SELECT fails. How to verify: Slice 1 implementer inspects v1 schema via `PRAGMA table_info(documents)` against the synthetic-v1 fixture before running backfill. Salience: medium.
- The six-step daemon-state preservation contract in FR-V9-8.2 is sufficient for the skill to reliably update the binary. Risk: the daemon `stop` → binary-replace → daemon `start` dance assumes `claudebase daemon stop` blocks until the old process terminates. How to verify: Slice 8 implementer adds a post-stop existence check (poll `daemon status` until it returns `stopped` before binary replace). Salience: medium.
- The `cp -n` / `Copy-Item -ErrorAction SilentlyContinue` no-clobber semantics for the `prompts/` → `~/.claude/` deploy step will not silently fail on directories vs files. Risk: `cp -n` on a path that is a directory (not a file) may have different behaviour across BSD vs GNU coreutils. How to verify: Slice 6c implementer tests the deploy on both macOS (BSD cp) and Linux (GNU cp) to confirm `-n` works uniformly. Salience: low.

### Open questions

- OQ-V9-CUT-1 (resolved in plan): Slice 1 implementer copies operator's actual DB to `tests/fixtures/operator-db-snapshot.db` (gitignored) for local backward-compat verification. The resolution is recorded in `.claude/plan.md:248`. Salience: medium.
- OQ-V9-CUT-2: Whether the SessionStart onboarding hook AND the subagent-onboarding hook are already deployed on operator's box. Slice 6 + 7 implementers verify before patching installers. Salience: low.
- OQ-V9-CUT-3: Operator explicit sign-off that `v0.9.0` is the correct semver label (not `v0.7.0-rebuild` or `v1.0.0`). Recommendation in product plan §7 R-V9-1 is to keep `v0.9.0` and explain via CHANGELOG. Salience: low.

## Decisions

### Inbound validation

- Task received: write PRD §19 from `.claude/plan.md` as source of truth. Challenged: no — the plan is coherent, approved by operator 2026-06-04, and the inbound task description maps 1:1 onto the plan's deliverables checklist entry for the PRD section. No contradiction with §18 found (§19 extends the insights corpus; §18 extends the Telegram routing; different subsystems). Outcome: proceeded. Salience: high.
- The plan's `### Symptom-only patches` entry acknowledges that the v0.7/v0.8 root cause was not isolated. Protocol 3 check: does executing this PRD section amplify that unresolved root cause? No — the PRD documents the port-forward approach explicitly (FR-V9-1 through FR-V9-9 are all additive, not cover-ups). The risk is documented in R-V9-CUT-1 through R-V9-CUT-7 and in the `### Symptom-only patches` entry of the plan. Outcome: proceeded. Salience: high.

### Decisions made

- Section number §19 chosen (next available after §18). Q1 hack? no | Q2 sane? yes | Q3 alternatives? — no other section numbers available — Q4 cause | Q5 n/a. Salience: low.
- AC-V9-1 through AC-V9-4 copied verbatim from the implementation plan's `## Success Criteria` table, including the deterministic AC-V9-2 wording with `Mira test` + `v9-cut-smoke` tag. Decision: do not paraphrase ACs — verbatim copy is the only way to ensure the qa-engineer's `/qa-cycle` execution targets the same criterion the planner designed. Q1 hack? no | Q2 sane? yes | Q3 alternatives? paraphrase — rejected (drift risk) | Q4 cause | Q5 n/a. Salience: high.
- Schema DDL in §19.7 presented as a full SQL block rather than prose description. Decision: implementers and reviewers need the exact DDL; a prose description of column types invites misreading. Q1 hack? no | Q2 sane? yes | Q3 alternatives? prose only — rejected (less precise) | Q4 cause | Q5 n/a. Salience: medium.
- `COALESCE(feature_slug, 'untagged')` chosen as the backfill tag default for rows with NULL `feature_slug`. Decision: `'untagged'` is a safe explicit sentinel that survives in the tag vocabulary without misleading operators. Alternatives: `''` (empty tag — would contaminate tag vocabulary), `NULL` (impossible — `tag TEXT NOT NULL`), `project_slug` (circular — `project_slug` is also being backfilled in the same transaction). Q1 hack? no | Q2 sane? yes | Q3 alternatives? listed above, all rejected | Q4 cause | Q5 n/a. Salience: medium.
- Waves B and C deferred to v0.10 per operator directive 2026-06-04. This is an operator scope decision, not a technical decision by prd-writer. Recorded here to make the deferral explicit and reviewable. Salience: high.

### Hacks / workarounds acknowledged

(none — this PRD section documents a principled port-forward of already-shipped v0.7 architecture; no shortcuts taken in the requirements)

### Symptom-only patches (with root-cause links)

- The v0.7/v0.8 root cause of brokenness was not isolated (operator's `docs/plans/claudebase-v0.9-product-plan.md` §1). v0.9 ships from the rebuild branch with v0.7's insights surface ported on top. Symptom: operator's box returns `error: index database invalid; re-ingest required` on every `insight create`. Root cause that remains: unknown (possibly a migration interaction between v0.6 schema and a v0.7 partial-apply, or a pre-existing DB corruption). Tracked at: `.claude/plan.md` §Symptom-only patches + `docs/plans/claudebase-v0.9-product-plan.md` §2 open bug #7. The Slice 1 done-condition (test against operator's actual DB snapshot) is the only verification that the migration correctly handles this specific real-world state. Salience: high.

---

## §20. CLI-to-CLI Routing — Agent-to-Agent Communication via Daemon

**Status:** [PLANNED]
**Date:** 2026-06-06
**Priority:** High
**Related:** §18 (Multi-Agent Telegram Routing — `agent_registry` table schema v4/v5 originated there; this section adds v5→v6 additive columns and reuses the existing `chat_messages` + `notifications/claude/channel` transport unchanged). §19 (claudebase v0.9 cut — `chat_messages.delivered_at` outbound spool from commit `ccdf538` is reused bit-for-bit; `agent_registry` table is the same table extended here). Plan source: `.claude/plan.md` (233 lines, read in full this session; HEAD `ccdf538` on branch `feat/multi-agent-on-v0.6`).

Changelog: Claude Code instances running in different terminals or clones of the same repo can now discover each other, share what they are working on, and send direct messages — without leaving the editor.

### 20.1 Feature Description

The operator runs multiple Claude Code windows simultaneously across different folders, branches, or git worktrees of the same logical project. Today these instances are completely siloed: they do not know about each other, cannot share findings, and produce overlapping or conflicting work without warning.

Existing infrastructure in the claudebase daemon already handles 80% of the problem. The `agent_registry` table (added in §18) tracks live agents by `agent_id` and `routing_key`. The `chat_messages` table with the guaranteed-delivery outbound spool (commit `ccdf538`) already provides at-least-once delivery semantics. The `notifications/claude/channel` MCP notification path already routes messages to the right Claude Code session. This feature is primarily wiring: extend the `agent_registry` schema (v5→v6), add three thin MCP tools (`agent_describe`, `agent_send`, `agent_set_dnd`), wire a `PostToolUse:ExitPlanMode` hook to mandate feature-description updates, add bridge auto-subscribe to an agent-specific inbox thread, and implement a DND (Do Not Disturb) state machine with background drain.

The trust model is single-box single-user. No prompt-injection guards between agents in MVP — the operator has confirmed that the use case is a local single-machine context. Inbound agent messages are wrapped in an `<agent-message>` XML tag to provide provenance context to the receiving model, but no escape-sequence sanitization or rate-limit guards are applied.

### 20.2 User Story

As the SDLC pipeline operator running multiple Claude Code windows on the same machine, I want each CC instance to register itself with the daemon and publish what feature it is working on, so that other CC instances can discover it, see its current task, send it a direct message, and avoid working on overlapping files — all without leaving the terminal.

### 20.3 Functional Requirements

#### FR-C2C-1: Schema Migration v5→v6 (Slice 1)

1. **FR-C2C-1.1:** The migration function `apply_agent_registry_c2c_migration` in `src/daemon/chat.rs` — invoked from `ensure_chat_db_schema` after `apply_routing_migration` and `apply_pending_asks_migration` — MUST apply an additive v5→v6 migration to `agent_registry` that adds the following columns when they are absent: `project_id TEXT`, `branch TEXT`, `working_dir TEXT`, `feature_description TEXT NULL`, `dnd_until_ts INTEGER NULL`. The migration MUST be idempotent: re-running against an already-migrated v6 schema MUST exit cleanly without error. This follows the same probe-before-ADD pattern as `apply_routing_migration` (see `src/daemon/chat.rs:508-540`).
2. **FR-C2C-1.2:** An index `agent_registry_project_id_idx ON agent_registry(project_id)` MUST be created as part of the migration to support efficient `list-alive --project` scope filtering.
3. **FR-C2C-1.3:** All existing `agent_registry` rows MUST survive the migration with their original column values unchanged. New columns MUST be backfilled with `NULL` (for `feature_description` and `dnd_until_ts`) and with `NULL` for `project_id`, `branch`, `working_dir` (values cannot be derived retroactively). Legacy `agent_registry` rows where `cwd IS NULL` (those predating the `apply_routing_migration` `cwd` column added 2026-06-04) will have `project_id = NULL` after backfill. The `--project current` filter MUST use `WHERE project_id = ? AND project_id IS NOT NULL` to exclude these unscopable rows. Rows with `project_id IS NULL` appear ONLY when the user passes `--project all`.
4. **FR-C2C-1.4:** The schema version sentinel in `src/daemon/chat.rs` MUST be bumped after the migration completes. `claudebase status --json` MUST reflect the new schema version.
5. **FR-C2C-1.5:** The existing columns on `agent_registry` (per `src/daemon/chat.rs:443-459` and the `apply_routing_migration` additions at lines 508-518) MUST remain structurally unchanged — no renames, no type changes, no constraint additions. Preserved columns are:
   - `agent_id TEXT PRIMARY KEY`
   - `agent_name TEXT NOT NULL`
   - `connection_id TEXT NOT NULL`
   - `chat_thread_id TEXT`
   - `permission_relayer TEXT`
   - `spawned_at INTEGER NOT NULL`
   - `last_pinged_at INTEGER NOT NULL`
   - `state TEXT NOT NULL CHECK (state IN ('alive','orphaned','dead'))`
   - `metadata TEXT`
   - `routing_chat_id INTEGER` (added by `apply_routing_migration`)
   - `routing_thread_id INTEGER CHECK (routing_thread_id IS NULL OR routing_thread_id > 0)` (added by `apply_routing_migration`)
   - `last_user_id INTEGER` (added by `apply_routing_migration`)
   - `host TEXT` (added by `apply_routing_migration`)
   - `cwd TEXT` (added by `apply_routing_migration`)
   - `pid INTEGER` (added by `apply_routing_migration`)

#### FR-C2C-2: `project_id` Resolver (Slice 2)

1. **FR-C2C-2.1:** A new module `src/project_id.rs` MUST be added to the crate (declared via `pub mod project_id;` in `src/lib.rs`). It MUST export one public function: `resolve_project_id(cwd: &Path) -> String`.
2. **FR-C2C-2.2:** `resolve_project_id` MUST implement a three-step fallback chain in order:
   - Step 1: Run `git -C <cwd> config --get remote.origin.url`. On success, normalize the URL to a `host/owner/repo` string: strip the protocol prefix (`https://`, `git@`, `ssh://`), strip the trailing `.git` suffix if present, replace `:` host-path separators (SSH format) with `/`, and lowercase the entire result.
   - Step 2: If Step 1 fails (non-zero exit or no `origin` remote), read `<cwd>/.claudebase/config.json` and return the `project_id` field if present and non-empty.
   - Step 3: If both Steps 1 and 2 fail, return `sha256(canonical_absolute_path(cwd))[..16]` as a hex string prefixed with `local:`.
3. **FR-C2C-2.3:** The resolver MUST be covered by at least 8 unit tests covering: HTTPS URL normalization, SSH URL normalization (colon-separator form), `.git` suffix stripping, case normalization, no-git-repo fallback (Step 3), `.claudebase/config.json` override (Step 2 priority over Step 3), git worktree with different origin, and fork-with-different-origin (two different repos in same directory structure).
4. **FR-C2C-2.4:** An integration test MUST create a real git repository in a temp directory, run `resolve_project_id` against it, and assert the result matches the expected normalized form of the temp repo's origin URL.

#### FR-C2C-3: `agent_describe` MCP Tool + Register-Time Identity Capture (Slice 3)

1. **FR-C2C-3.1:** The existing `agent_register` MCP handler MUST be extended to call `resolve_project_id(cwd)` and `git rev-parse --abbrev-ref HEAD` at register time and persist the resulting `project_id`, `branch`, and `working_dir` values into the `agent_registry` row.
2. **FR-C2C-3.2:** A new MCP tool `agent_describe` MUST be added to `src/daemon/server.rs` with the following input schema: `{ feature_id: string, branch: string, description: string }`. The handler MUST UPDATE the existing `agent_registry` row for the calling `agent_id`, writing `feature_description = description` and `branch = branch`.
3. **FR-C2C-3.3:** `agent_describe` MUST be registered in `src/plugin/mcp.rs`'s TOOL_WHITELIST with a JSON schema matching the input shape in FR-C2C-3.2.
4. **FR-C2C-3.4:** A round-trip test MUST verify: agent registers → row contains non-NULL `project_id` + `branch` + `working_dir` → `agent_describe` is called → row's `feature_description` is updated → `agent_list_alive` returns the updated description.

#### FR-C2C-4: `agent_send` MCP Tool + Bridge Auto-Subscribe (Slice 4)

1. **FR-C2C-4.1:** A new MCP tool `agent_send` MUST be added with input schema: `{ to_agent_id: string, content: string, urgent?: boolean }`. The handler MUST:
   a. Verify `to_agent_id` exists in `agent_registry` (fail with a structured error if not found — see FR-C2C-4.2).
   b. Post the message to thread `agent:<to_agent_id>` using the existing `chat_post` machinery (the same `chat_messages` table used by Telegram, inserting with `thread_id = 'agent:<to_agent_id>'`, reusing `delivered_at` tracking from commit `ccdf538`).
   c. If the receiver's `dnd_until_ts` is NULL or in the past: emit a `notifications/claude/channel` notification with `target_agent_id=<to_agent_id>` and return `{ delivered: true }`.
   d. If the receiver IS in DND (`dnd_until_ts > now`): persist the message with `delivered_at = NULL` and return `{ queued: true, delivered_when: dnd_until_ts }`. No notification is emitted.
2. **FR-C2C-4.2:** When `to_agent_id` is not found in `agent_registry`, `agent_send` MUST return a structured error (exit 1 / error response body) with a message indicating the agent does not exist. It MUST NOT silently enqueue for an unknown agent.
3. **FR-C2C-4.3:** The outbound `notifications/claude/channel` notification for agent-to-agent messages MUST use the same wire format as Telegram inbound (shape frozen in §18 §Frozen — bit-for-bit existing surface): `source="claudebase"`, `chat_id="<from_agent_id>"`, `thread="agent:<to_agent_id>"`, `target_agent_id=<to_agent_id>`, with an additional optional `meta.kind="agent-to-agent"` field.
4. **FR-C2C-4.4:** On initial bridge connection, `src/plugin/bridge.rs` MUST auto-subscribe to the thread `agent:<my-agent-id>` so that inbound agent-to-agent messages are delivered as channel notifications without requiring an explicit `chat_subscribe` call from the agent.
5. **FR-C2C-4.5:** The bridge's existing `should_relay_channel_notification(target_agent_id)` filter MUST remain unchanged. It already routes notifications to the correct CC session; agent-to-agent notifications reuse it without modification.
6. **FR-C2C-4.6 (Sender Identity Binding — SECURITY):** `handle_agent_send` MUST resolve `from_agent_id` from the connection's registered identity by querying `agent_registry WHERE connection_id = <current_connection_id>`, NOT from caller-supplied arguments. This prevents any local process with UDS access from impersonating arbitrary agents. The implementation MUST mirror the existing `handle_agent_register` pattern at `src/daemon/server.rs` which binds the caller identity to the connection's `connection_id: Uuid` from connection state — the UDS connection state already carries this. Slice 4 REQUIRES a **security pre-review** from `security-auditor` before implementation begins.

#### FR-C2C-5: `agent_set_dnd` MCP Tool + DND Drain Background Task (Slice 5)

1. **FR-C2C-5.1:** A new MCP tool `agent_set_dnd` MUST be added with input schema: `{ state: string }` where `state` accepts the following values: `"on"` (DND active indefinitely), `"off"` (clear DND), `"<N>m"` (DND for N minutes), `"<N>h"` (DND for N hours), `"until HH:MM"` (DND until a wall-clock time in the local timezone). The handler MUST write the computed `dnd_until_ts` (Unix epoch integer, or NULL for "off") to `agent_registry` for the calling agent_id. **Indefinite DND encoding:** `agent_set_dnd("on")` without a duration MUST write `dnd_until_ts = i64::MAX (9223372036854775807)`. `NULL` means no DND is active. The drain task's `WHERE dnd_until_ts < now()` predicate naturally excludes `i64::MAX` rows without special-casing. An explicit test in `tests/agent_dnd_test.rs` MUST cover the indefinite path.
2. **FR-C2C-5.2:** The daemon MUST run a **new recurring `tokio::spawn` background task** in the daemon main loop (NOT an extension of `drain_pending_outbound_tg`, which is a startup-one-shot). This task polls `agent_registry` every 30 seconds for rows where `dnd_until_ts IS NOT NULL AND dnd_until_ts < now()`. For each expired DND row, the task MUST: (a) clear `dnd_until_ts` to NULL, (b) query `chat_messages WHERE thread_id = 'agent:<id>' AND delivered_at IS NULL`, (c) emit `notifications/claude/channel` for each queued message using the existing notification path, and (d) set `delivered_at = now()` on each drained message row. The SQL drain pattern reuses the approach from `src/daemon/telegram.rs:160-205` applied to `thread_id LIKE 'agent:%'` rows for agents whose `dnd_until_ts < now()` was just cleared.
3. **FR-C2C-5.3:** When DND is active and `agent_send` is called, the caller MUST receive the response `{ queued: true, delivered_when: <dnd_until_ts as ISO-8601 string> }` within 2 seconds of the call.
4. **FR-C2C-5.4:** DND drain latency MUST be at most 30 seconds from DND expiry (enforced by the polling interval). This is an explicit operator-acknowledged trade-off: event-driven drain is deferred as an optimization (see §20.8 R-C2C-4).
5. **FR-C2C-5.5 (DND Drain Rate Limit):** The drain task MUST emit at most 10 `notifications/claude/channel` notifications per agent per 30-second tick. If an agent has more than 10 queued messages, the task processes the oldest 10 (ordered by `created_at ASC`) and leaves the remainder with `delivered_at = NULL` — they surface on the next tick. This protects the CC channel surface from bombardment when many messages drain simultaneously on DND-off. The `delivered_at` timestamp on drained rows MUST be set to the actual emission time, not the original `created_at`, so drain order is preserved across ticks.

#### FR-C2C-6: `claudebase agent list-alive` CLI Subcommand (Slice 6)

1. **FR-C2C-6.1:** A new CLI subcommand `claudebase agent list-alive` MUST be added. It MUST accept a `--project` flag with values: `current` (filter by the `project_id` resolved from the invoking process's cwd), `all` (return all alive agents across all projects), or `<slug>` (a literal normalized project_id string). Default is `current`.
2. **FR-C2C-6.2:** The output MUST include the following fields for each alive agent: `agent_id`, `branch`, `working_dir`, `feature_description` (NULL displayed as `null`), `last_seen_at`, `dnd_until_ts` (NULL displayed as `null`).
3. **FR-C2C-6.3:** With `--json`, output MUST be a JSON array of objects with the fields listed in FR-C2C-6.2. Without `--json`, a human-readable table format MUST be used.
4. **FR-C2C-6.4:** The `--project current` filter MUST call `resolve_project_id(cwd())` to determine the current project's normalized identifier, then SELECT only rows from `agent_registry` where `project_id = <resolved> AND project_id IS NOT NULL` (the `IS NOT NULL` guard excludes legacy rows that predate the `cwd` column and received `project_id = NULL` during the v5→v6 backfill; see FR-C2C-1.3).
5. **FR-C2C-6.5:** Agents from other projects MUST NOT appear in `--project current` output. This is the load-bearing isolation property (AC-C2C-1).

#### FR-C2C-7: `PostToolUse:ExitPlanMode` Hook for Feature-Description Discipline (Slice 7)

1. **FR-C2C-7.1:** Two new hook scripts MUST be created: `hooks/claudebase-feature-describe.sh` and `hooks/claudebase-feature-describe.ps1`. Both MUST contain only ASCII bytes (codepoint ≤ 127); the same ASCII-only constraint applies as in §19 FR-V9-6.2 — Windows PowerShell 5.1 parses no-BOM scripts in the local ANSI code page. Hook scripts MUST read `.claude/plan.md` first heading via **literal-text extraction only** (e.g., `grep` / `awk` text extraction). Scripts MUST NOT execute heading content in any form (`bash -c "<heading>"` or equivalent shell-expansion patterns are forbidden). Low risk given single-box trust model but defense-in-depth applies.
2. **FR-C2C-7.2:** The hook MUST fire on the Claude Code `PostToolUse` event when the tool name matches `ExitPlanMode`. It MUST read the first heading from `.claude/plan.md` to extract the feature title, then inject `additionalContext` that mandates the receiving agent to: (a) call `agent_describe(feature_id, branch, description)` via MCP, and (b) update the `.claude/scratchpad.md` `## Feature:` line to match. Both writes MUST happen in the same turn. **Pre-flight hook event verification (Slice 7 requirement):** The implementer MUST verify the hook event name and matcher field against the actual `~/.claude/settings.json` schema BEFORE writing the scripts. Primary target: `PostToolUse` event + `matchers: ["ExitPlanMode"]`. Fallback A (if primary is not supported): `UserPromptSubmit` event + detect-previous-turn-ExitPlanMode heuristic. Fallback B: `Stop` hook + content-marker detection. Fallback C (operator-driven): Mira calls `agent_describe` manually post-bootstrap. The implementer MUST document which strategy was verified and used in the slice commit message.
3. **FR-C2C-7.3:** The installer (`install.sh` and `install.ps1`) MUST wire both hook scripts into `~/.claude/settings.json` under `hooks.PostToolUse` using dedup-by-command-string equality (the same idempotent wiring pattern as §19 FR-V9-6.3). Re-running the installer MUST produce zero new entries.
4. **FR-C2C-7.4:** The hook MUST be idempotent: if `.claude/plan.md` does not exist or has no heading on the first non-blank line, the hook MUST emit an empty `additionalContext` (or a minimal note) rather than failing.

#### FR-C2C-7b: `PreToolUse:EnterPlanMode` Peer-Awareness Hook (Slice 7b — operator-requested 2026-06-06)

Complementary read-side hook to FR-C2C-7 — fires BEFORE the agent enters plan mode so plans drafted in isolation can be coordinated against parallel work in other CC sessions. Together with FR-C2C-7 (the write-side, post-exit) they form the read-write boundary of cli-to-cli routing: read peers before planning, publish your plan after exiting.

1. **FR-C2C-7b.1:** Two new hook scripts MUST be created: `hooks/claudebase-agent-routing-reminder.sh` and `hooks/claudebase-agent-routing-reminder.ps1`. Same ASCII-only constraint as FR-C2C-7.1 (`.ps1` codepoints ≤ 127). Scripts MUST gracefully skip emit (empty `additionalContext`) when the `claudebase` binary is absent on the operator's box so non-claudebase sessions are not noisy.

2. **FR-C2C-7b.2:** The hook MUST fire on the Claude Code `PreToolUse` event when the tool name matches `EnterPlanMode`. It MUST inject `additionalContext` teaching the receiving agent (a) the WHY of cli-to-cli routing — that parallel CC sessions often collide on overlapping work and the channel exists to detect-and-coordinate BEFORE the plan commits; (b) the DISCOVER primitives `claudebase agent list-alive --project current` and `claudebase agent inspect <agent_id>`; (c) the MCP communication tool surface `agent_send` / `agent_describe` / `agent_set_dnd` with explicit FR-C2C-4.6 daemon-side identity-binding callout; (d) the INBOUND peer-message wire format (TG-shape `<channel>` meta + JSON `agent_to_agent` preamble at the start of the content body per FR-C2C-8 hotfix #2 + blank line + verbatim sender text); (e) the single-box single-user trust model breadcrumb.

3. **FR-C2C-7b.3:** Installer wiring MUST add this hook to `~/.claude/settings.json` under `hooks.PreToolUse` with matcher `EnterPlanMode` using the SAME idempotent dedup-by-command-string pattern as FR-C2C-7.3. Re-running the installer MUST produce zero new entries.

4. **FR-C2C-7b.4:** The hook script SHOULD NOT spawn a subprocess that runs `claudebase agent list-alive` for the agent — the script's job is to TEACH the agent what to do, not to do it. The agent invokes the discovery primitives itself when the planning context calls for it. Keeps the hook fast (single file existence check + JSON envelope emit) and avoids consuming agent-context budget with output the agent might not need.

#### FR-C2C-8: Bridge Inbound `<agent-message>` Rendering Convention (Slice 8)

1. **FR-C2C-8.1:** `src/plugin/bridge.rs` MUST branch on the `meta.kind` field of an inbound `notifications/claude/channel` event. When `meta.kind = "agent-to-agent"`, the notification content MUST be rendered as `<agent-message from="<chat_id>" thread="<thread>" ts="<timestamp>">CONTENT</agent-message>` rather than the Telegram `<channel>` shape.
2. **FR-C2C-8.2:** The Telegram inbound rendering (`<channel>` shape) MUST remain unchanged for all notifications where `meta.kind` is absent or is not `"agent-to-agent"`. This is a non-negotiable regression-safety requirement.
3. **FR-C2C-8.3:** The `from`, `thread`, and `ts` attributes on the `<agent-message>` tag MUST be populated from the corresponding fields of the notification's meta object (`chat_id`, `thread`, and the message timestamp).
4. **FR-C2C-8.4:** At least 4 unit tests MUST cover: (a) Telegram inbound still renders `<channel>` (regression), (b) agent-to-agent inbound renders `<agent-message>`, (c) `<agent-message>` contains the correct `from` attribute, (d) missing `meta.kind` defaults to `<channel>` rendering. **Discriminator fallthrough rule:** ANY value of `meta.kind` other than the literal string `"agent-to-agent"` falls through to the existing `<channel>` rendering path. Implementers MUST NOT treat unknown `meta.kind` values as a discriminated union to error on — they are future-extension values that must render as `<channel>` for backward compatibility.

### 20.4 Non-Functional Requirements

1. **NFR-C2C-1 (end-to-end delivery latency):** `agent_send` MUST return within 2 seconds on a local box (daemon and both CC instances on the same machine, LAN-isolated). The round-trip includes the daemon handler + SQLite write + channel notification emit. — salience: high.
2. **NFR-C2C-2 (DND drain latency):** After DND expiry, queued messages MUST be drained and delivered as channel notifications within 30 seconds. This is determined by the background task polling interval (FR-C2C-5.2). — salience: medium.
3. **NFR-C2C-3 (schema migration backward compatibility):** The v5→v6 migration MUST be idempotent and additive. No existing column may be dropped or renamed. No data in existing rows may be lost. — salience: high.
4. **NFR-C2C-4 (single-box single-user trust model):** No network surface is exposed beyond the existing UDS/named-pipe socket. Agent-to-agent messages never leave the local machine. No authentication layer is added between agents in MVP. This trade-off is operator-confirmed (plan.md §Trust model, 2026-06-05). — salience: high.
5. **NFR-C2C-5 (regression safety for Telegram surface):** All existing Telegram functionality (`<channel>` rendering, `should_relay_channel_notification` filter, `chat_post` / `chat_subscribe` / `chat_ask` / `chat_reply` tools) MUST remain fully functional after this feature lands. The existing 178+ tests MUST continue to pass. — salience: high.
6. **NFR-C2C-6 (ASCII-only hook scripts):** Both `.ps1` hook files MUST contain only ASCII bytes (codepoint ≤ 127). — salience: high.
7. **NFR-C2C-7 (test coverage target):** The feature MUST add approximately 45 new unit and integration tests across 8 new test files (breakdown: ~6 v6 schema, ~8 project_id resolver, ~6 agent_describe, ~8 agent_send, ~7 agent_set_dnd, ~6 CLI list-alive, ~4 bridge render). — salience: medium.

8. **NFR-C2C-8 (agent-to-agent rendering convention):** Daemon-emitted agent-to-agent notifications MUST use the SAME `notifications/claude/channel` JSON-RPC method as the Telegram inbound path, with two distinguishing meta fields: `meta.source = "claudebase:agent"` (distinct from `"claudebase"` / `"plugin:telegram:telegram"` used for TG) AND `meta.kind = "agent-to-agent"`. Claude Code's channel surface renders the meta into a `<channel source="claudebase:agent" kind="agent-to-agent" from_agent_id="<sender>" thread="agent:<receiver>" target_agent_id="<receiver>" message_id="<uuid>" drained_from_dnd="<bool>">CONTENT</channel>` tag in the receiving model's prompt context. Downstream consumers (future bridge versions, CC versions, alternative MCP clients) MUST treat any frame where `meta.kind != "agent-to-agent"` (or `meta.kind` is absent) as TG inbound and fall through to the existing channel rendering (UC-C2C-15-EC1 fallthrough rule — implementers MUST NOT treat unknown `meta.kind` values as a discriminated-union error). The frame builder `crate::daemon::chat::build_channel_notification_agent_to_agent` is the single source of truth for the wire shape; both the direct `agent_send` path AND the Slice 5 DND drain path MUST call it (no inline JSON literals duplicating the shape). — salience: high.

### 20.5 Acceptance Criteria

All five criteria must pass before `/merge-ready` is invoked for this feature.

| ID | Criterion | Evidence Required |
|---|---|---|
| **AC-C2C-1** | Operator runs `claudebase agent list-alive --project current` from one CC. Output is a JSON array listing all currently-alive agents in the same `project_id` (normalized from `git remote origin URL`), excluding agents from other projects. Each entry includes `agent_id`, `branch`, `working_dir`, `feature_description`, `last_seen_at`, `dnd_until_ts`. | Shell stdout JSON array; at least 2 agents present (operator opens 2 CC windows in 2 different clones of the same repo); both appear in the list; cwd hashes for unrelated projects do NOT appear; `feature_description` field is non-NULL for at least one entry. Captured in `docs/qa/evidence/cli-to-cli-routing/AC-C2C-1-list-alive-stdout.json`. |
| **AC-C2C-2** | Operator publishes a feature description from one CC via MCP tool `agent_describe(feature_id, branch, description)`. Within 5 seconds, the other CC running `agent list-alive` sees the updated description in the daemon. | Daemon-side SQL `SELECT feature_description FROM agent_registry WHERE agent_id=?` returns the published string; second CC's `agent list-alive --project current` shows the updated field. Captured in `docs/qa/evidence/cli-to-cli-routing/AC-C2C-2-describe-roundtrip.txt`. |
| **AC-C2C-3** | Agent A in CC #1 calls MCP tool `agent_send(to_agent_id, content)`. Within 2 seconds, CC #2 receives the message as a `notifications/claude/channel` notification with `target_agent_id=B`. CC #2's bridge filter passes it through. | CC #2 transcript shows the inbound `<agent-message from="A" thread="agent:B">...</agent-message>` block; `chat_messages` row exists with `from_agent='A'`, `thread='agent:B'`, `delivered_at` non-NULL. Captured in `docs/qa/evidence/cli-to-cli-routing/AC-C2C-3-send-receive-transcript.md`. |
| **AC-C2C-4** | Agent B sets DND via `agent_set_dnd("30m")`. Agent A's subsequent `agent_send` to B is enqueued but produces no `notifications/claude/channel` emit during the DND window. On `agent_set_dnd("off")`, the daemon drains and delivers the queued messages to CC #2. | CC #2 transcript shows NO notification during DND window; `agent_send` returns `{queued: true, delivered_when: <ts>}`; after DND off, CC #2 transcript shows queued messages drained as channel notifications within `30s × ceil(count/10)` seconds (rate limit: 10 per 30s tick per FR-C2C-5.5); `chat_messages.delivered_at` is set on all drained rows. Captured in `docs/qa/evidence/cli-to-cli-routing/AC-C2C-4-dnd-queued-then-drained.md`. |
| **AC-C2C-5** | The `PostToolUse` hook matching `ExitPlanMode` fires after an operator exits plan mode. The injected context mandates calling `agent_describe` AND updating `.claude/scratchpad.md` `## Feature:`. Both writes succeed in the same agent turn. | Hook output captured in session transcript; daemon-side row updated with non-NULL `feature_description`; scratchpad's `## Feature:` line matches the daemon's `feature_description`. Captured in `docs/qa/evidence/cli-to-cli-routing/AC-C2C-5-hook-fired-stdout.txt`. |

### 20.6 Affected Components

**New files:**
- `src/project_id.rs` — `resolve_project_id` function and fallback chain
- `hooks/claudebase-feature-describe.sh`
- `hooks/claudebase-feature-describe.ps1`
- `tests/store_v6_test.rs` — schema migration tests
- `tests/agent_registry_v6_test.rs` — extended struct and query tests
- `tests/project_id_test.rs` — resolver unit tests (≥8)
- `tests/agent_describe_test.rs` — round-trip tests (≥6)
- `tests/agent_send_test.rs` — send/DND/queue path tests (≥8)
- `tests/agent_dnd_test.rs` — DND state machine tests (≥7)
- `tests/cli_agent_list_alive_test.rs` — CLI output and scope filter tests (≥6)
- `tests/bridge_agent_message_render_test.rs` — rendering regression tests (≥4)
- `docs/use-cases/cli-to-cli-routing_use_cases.md` — (produced by ba-analyst, Step 2)
- `docs/qa/cli-to-cli-routing_test_cases.md` — (produced by qa-planner, Step 4)
- `docs/qa/evidence/cli-to-cli-routing/` — (populated by qa-engineer in Slice 9)

**Modified files:**
- `src/daemon/chat.rs` — `apply_agent_registry_c2c_migration` function (new, invoked from `ensure_chat_db_schema` after `apply_routing_migration` and `apply_pending_asks_migration`) — v5→v6 migration logic
- `src/daemon/agent_registry.rs` — struct extension (`project_id`, `branch`, `working_dir`, `feature_description`, `dnd_until_ts`) + query functions for list-alive with project filter
- `src/daemon/server.rs` — `agent_register` extension + `agent_describe` handler + `agent_send` handler (with sender identity binding per FR-C2C-4.6) + `agent_set_dnd` handler + DND-expiry background task (new `tokio::spawn` recurring task)
- `src/plugin/bridge.rs` — auto-subscribe to `agent:<my-id>` on connect + `<agent-message>` rendering branch
- `src/plugin/mcp.rs` — TOOL_WHITELIST additions for `agent_describe`, `agent_send`, `agent_set_dnd` + tool specs
- `src/cli.rs` — `AgentSubcommand::ListAlive` with `--project` flag
- `src/main.rs` — `run_agent_list_alive` handler
- `src/lib.rs` — `pub mod project_id;`
- `install.sh` — PostToolUse hook wiring (additive, idempotent)
- `install.ps1` — PostToolUse hook wiring (additive, idempotent)

**Preserved bit-for-bit (frozen):**
- `chat_messages` schema — including `delivered_at` column from `ccdf538`; agent-to-agent messages reuse the same table with `thread = 'agent:<id>'` convention
- `notifications/claude/channel` wire format meta shape — `chat_id` as string, `target_agent_id`, etc. (§18 contract)
- `should_relay_channel_notification(target_agent_id)` bridge filter — UNCHANGED; reused for agent-to-agent routing
- Telegram `<channel>` inbound rendering — UNCHANGED; agent-to-agent uses the new `<agent-message>` shape only when `meta.kind = "agent-to-agent"`

### 20.7 Schema Changes

The following additive SQL DDL constitutes the complete v5→v6 migration applied to `agent_registry`. The migration is implemented as `apply_agent_registry_c2c_migration(conn)` in `src/daemon/chat.rs`, following the same probe-before-ADD pattern as `apply_routing_migration` (lines 508-540 of the same file):

```sql
-- Additive columns on agent_registry (guarded by PRAGMA table_info probe before each ALTER)
-- Follows the same probe-before-ADD pattern as apply_routing_migration in src/daemon/chat.rs:508-540
ALTER TABLE agent_registry ADD COLUMN project_id TEXT;
ALTER TABLE agent_registry ADD COLUMN branch TEXT;
ALTER TABLE agent_registry ADD COLUMN working_dir TEXT;
ALTER TABLE agent_registry ADD COLUMN feature_description TEXT;
ALTER TABLE agent_registry ADD COLUMN dnd_until_ts INTEGER;

-- Index for efficient project-scoped list-alive queries
CREATE INDEX IF NOT EXISTS agent_registry_project_id_idx ON agent_registry(project_id);
```

Backfill: existing rows receive `NULL` for all five new columns. Legacy rows where `cwd IS NULL` (predating `apply_routing_migration`) receive `project_id = NULL` and are excluded from `--project current` queries (see FR-C2C-1.3, FR-C2C-6.4). `project_id` is populated at next `agent_register` call for each agent (no retroactive derivation).

The `chat_messages` table is unchanged by this migration. The column name is `thread_id` (verified at `src/daemon/chat.rs:431` — `thread_id TEXT NOT NULL`); agent-to-agent messages insert with `thread_id = 'agent:<to_agent_id>'`.

This schema change applies to `chat.db` (the daemon's operational database). `index.db` (books corpus), `insights.db` (agent insights), and `claudebase.db` are unaffected.

### 20.8 Out of Scope (this feature)

- **Prompt-injection guard between agents** — operator-confirmed deferred; single-box single-user trust is sufficient for MVP.
- **Broadcast/topic-style threads** (e.g., a `project:claudebase` shared channel all agents subscribe to) — MVP is DM-style only.
- **Cross-host communication** (agent on machine A talks to agent on machine B) — daemon is local-only.
- **Urgent-override of DND** (`agent_send --urgent`) — proposed, deferred; MVP DND is a hard block.
- **Event-driven DND drain** (tokio one-shot timer per DND expiry instead of 30s polling) — deferred optimization; 30s latency is acceptable per operator acknowledgment.

### 20.9 Risks and Dependencies

**R-C2C-1 (`project_id` ambiguity for forks and monorepo splits):** Two clones of forks with different `origin` URLs produce different `project_id` values and will NOT discover each other. Mitigation: the `.claudebase/config.json::project_id` manual override path provides an escape hatch. Acceptable for MVP. Salience: medium.

**R-C2C-2 (Hook fires only on plan-mode exit; mid-session feature switches are undetected):** If the operator says "now work on slice 9" without entering plan mode, no ExitPlanMode fires → no hook → `feature_description` in the daemon is stale. Mitigation: extending the existing `UserPromptSubmit` hook to detect scratchpad-vs-daemon drift is documented in §20.3 FR-C2C-7.2 as a low-priority deferred follow-up. Salience: medium.

**R-C2C-3 (Race condition on concurrent `agent_describe` for the same `agent_id`):** Two CC windows in the same cwd both register as the same agent_id and both call `agent_describe` concurrently. Last-write-wins; no merge. Acceptable for MVP; documented. Salience: low.

**R-C2C-4 (DND drain background task adds ~30s polling cost):** Could be replaced with an event-driven one-shot timer on DND change. MVP ships with polling. Deferred optimization. Salience: low.

**R-C2C-5 (`<agent-message>` tag is a new model-facing convention):** Claude Code's channel surface may not render it specially. Mitigation: the receiving model treats the tag as quoted provenance context. Integration test with a real CC verifies the tag is visible to the model. Salience: medium.

**R-C2C-6 (Single-branch interleave with v0.9-cut Wave 3):** cli-to-cli-routing commits share the `feat/multi-agent-on-v0.6` branch with parked v0.9-cut Wave 3 (Slices 9-11). Mitigation: distinct commit-message prefixes per feature; merge-prep Slice 10 triages commits. Operator-accepted. Salience: medium.

**R-C2C-7 (Bridge auto-subscribe may require additive logic for `agent:*` threads):** The existing self-bootstrap at bridge init is hardcoded for `telegram:*` threads. Adding `agent:<my-id>` subscribe may require reading the `agent_id` at connect time. Mitigation: implementer reads `src/plugin/bridge.rs` before Slice 4 to confirm the bootstrap pattern. Salience: high.

**R-C2C-8 (`chat_messages.thread_id` column constraints):** The `chat_messages.thread_id` column (verified at `src/daemon/chat.rs:431`) has no CHECK constraint restricting values to `telegram:%` prefixes — it is declared as `TEXT NOT NULL` with no enum guard. Agent-to-agent messages using `thread_id = 'agent:<id>'` are therefore structurally valid. This risk is RESOLVED by verification: no constraint conflict exists. Salience: low (downgraded from high; constraint verified this session).

## Facts

### Verified facts

- `.claude/plan.md` lines 1–233 read in full this session — source: Read tool call, offset 0 limit 233 — salience: high.
- Current branch `feat/multi-agent-on-v0.6` HEAD `ccdf538` — source: `.claude/plan.md` line 5 (plan.md §Verified facts: "verified `git branch --show-current` + `git log -1 --oneline` this session") — salience: high.
- **CORRECTED (architect amendment 2026-06-06):** `agent_registry` actual columns verified at `src/daemon/chat.rs:443-459` (base schema) and `src/daemon/chat.rs:508-518` (routing migration additions): `agent_id PRIMARY KEY`, `agent_name NOT NULL`, `connection_id NOT NULL`, `chat_thread_id`, `permission_relayer`, `spawned_at NOT NULL`, `last_pinged_at NOT NULL`, `state CHECK('alive'|'orphaned'|'dead')`, `metadata`; plus routing migration columns `routing_chat_id`, `routing_thread_id CHECK(>0)`, `last_user_id`, `host`, `cwd`, `pid`. The original PRD §20 stated `agent_id, routing_key, last_seen_at, registered_at` — this was factually incorrect and has been corrected in FR-C2C-1.5 and §20.7 — source: Read tool call `src/daemon/chat.rs:443-552` this session — salience: high.
- **CORRECTED (architect amendment 2026-06-06):** Migration function is `apply_routing_migration` (and the new `apply_agent_registry_c2c_migration` to be created) in `src/daemon/chat.rs`, invoked from `ensure_chat_db_schema` (line 418) — NOT in `src/store.rs` as originally stated — source: Read tool call `src/daemon/chat.rs:418-472` this session — salience: high.
- **CORRECTED (architect amendment 2026-06-06):** `chat_messages` column is `thread_id` (not `thread`) — verified at `src/daemon/chat.rs:431` — salience: high.
- **CORRECTED (architect amendment 2026-06-06):** `chat_messages.thread_id` has no CHECK constraint restricting values to `telegram:%` — verified at `src/daemon/chat.rs:426-434` (base schema declaration) — R-C2C-8 risk resolved — source: Read tool call this session — salience: high.
- `ensure_chat_db_schema` calls `apply_routing_migration(conn)` then `apply_pending_asks_migration(conn)` before returning — source: `src/daemon/chat.rs:467-471` Read this session — salience: high.
- `chat_messages.delivered_at` column exists and is populated by the outbound spool (commit `ccdf538`) — source: `src/daemon/chat.rs:433` Read this session — salience: high.
- Existing MCP tools `agent_register`, `agent_list_alive`, `agent_unregister`, `chat_post`, `chat_subscribe`, `chat_ask`, `chat_list_pending_asks`, `chat_list_threads`, `chat_reply`, `chat_list` verified as live — source: `.claude/plan.md` line 179 — salience: high.
- `should_relay_channel_notification(target_agent_id)` bridge filter exists (Slice 6 multi-agent-tg) — source: `.claude/plan.md` line 180 — salience: high.
- `.claudebase/config.json` per-project persistence exists (commit `25189bc`) — source: `.claude/plan.md` line 182 — salience: medium.
- Acceptance criteria AC-C2C-1 through AC-C2C-5 copied verbatim from `.claude/plan.md` lines 38–42 (§Success Criteria table) — source: plan.md Read this session — salience: high.
- PRD §19 is the prior section (claudebase v0.9 cut); §20 is the correct next section — source: Grep on `^## §\d+` against `docs/PRD.md` this session returning `§19` as last match at line 1303 — salience: high.
- `docs/PRD.md` line count is 1600 (verified via PowerShell `Get-Content | Count` this session at original authoring) — source: Bash tool call this session — salience: medium.
- Insights corpus hit: doc_id=5 (`agent:mira:-:-:080737317032621b`) contains "Operator correction on cli-to-cli routing design: feature_description for cross-agent discovery MUST be agent-published via MCP (PostToolUse:ExitPlanMode hook mandates the call) and mirrored in both scratchpad and daemon" — source: `claudebase insight search` call this session — salience: high.
- `notifications/claude/channel` wire format — meta MUST match TG inbound contract shape (§18 contract, frozen) — source: `.claude/plan.md` lines 48–52 §Frozen section — salience: high.
- Corpus scope relevance check: `claudebase list --json` returned 0 documents for this project workspace; corpus is absent. No topical queries executed — salience: low.

### External contracts

- **`git config --get remote.origin.url`** — symbol: returns remote URL string or exits non-zero if remote `origin` is not configured — source: git documentation (not opened this session) — verified: no — assumption (well-established git CLI contract; risk: non-standard remote name or bare repo setup may not have `origin`; mitigated by fallback chain) — salience: medium.
- **`git rev-parse --abbrev-ref HEAD`** — symbol: returns current branch name or `HEAD` if in detached HEAD state — source: git documentation (not opened this session) — verified: no — assumption — salience: low.
- **`PostToolUse` hook event with `ExitPlanMode` matcher** — symbol: Claude Code hook system fires `PostToolUse` after every tool call; `matchers: ["ExitPlanMode"]` in settings.json scopes to that one tool. Fallback strategy documented in FR-C2C-7.2: Primary → PostToolUse+matchers:[ExitPlanMode]; Fallback A → UserPromptSubmit+detect-prev-turn-ExitPlanMode; Fallback B → Stop hook+content-marker; Fallback C → operator-driven. Implementer MUST verify before scripting — source: Claude Code hook documentation (not opened this session) — verified: no — assumption. Risk: hook event semantics, field name, or `matchers` key may differ; Slice 7 pre-flight verification is mandatory (FR-C2C-7.2) — salience: high.
- **`notifications/claude/channel` wire format** — symbol: meta object fields `chat_id` (string), `target_agent_id` (string), `thread` (string), optional `meta.kind` (string) — source: §18 PRD contract (read this session at docs/PRD.md line 783+) + commit `ccdf538` live-tested in plan.md §Verified facts — verified: yes — salience: high.
- **`chat_messages` table columns** — symbol: `thread_id TEXT NOT NULL` (CORRECTED from earlier `thread TEXT`), `delivered_at INTEGER` (nullable) — source: `src/daemon/chat.rs:431,433` Read this session — verified: yes — salience: high.
- **`src/daemon/server.rs` `handle_agent_register` pattern** — symbol: uses `connection_id: Uuid` from connection state to bind caller identity (per FR-C2C-4.6 sender identity binding) — source: architect finding citing `src/daemon/server.rs:1284`; file NOT read this session — verified: no — assumption. Risk: if the pattern differs, FR-C2C-4.6 implementer must adapt the identity-binding approach. Verify by reading `src/daemon/server.rs:1280-1290` before Slice 4 — salience: high.

### Assumptions

- Bridge auto-subscribe to `agent:<my-id>` can reuse the existing self-bootstrap pattern at bridge init; the current pattern is hardcoded for `telegram:*` threads and may require additive logic. Risk: if `agent_id` is not available at bridge init time (e.g., because registration happens after init), the auto-subscribe cannot fire. How to verify: implementer reads `src/plugin/bridge.rs` before Slice 4. Tracked in R-C2C-7. Salience: high.
- `chat_messages.thread_id` has no CHECK constraint restricting values to `telegram:%` prefixes — RESOLVED by architect verification (`src/daemon/chat.rs:431` confirms `thread_id TEXT NOT NULL` with no enum guard); R-C2C-8 risk downgraded to low. Salience: low.
- The `<agent-message>` rendering convention will be visible to the receiving model as provenance context even if Claude Code's channel surface has no special treatment for it. Risk: CC may strip unknown XML-tag shapes. Mitigation: integration test in Slice 8 live-verifies with real CC. Salience: medium.
- Approximately 45 new tests will be added across 8 new test files. Risk: actual count may differ during implementation; this is a planning estimate. How to verify: `cargo test --workspace` count before and after each slice. Salience: low.
- **PRD §20 pre-amendment factual errors acknowledged (2026-06-06):** The original PRD §20 (authored 2026-06-05) contained factual errors about the codebase: (1) wrong agent_registry column names (`routing_key, last_seen_at, registered_at` vs actual `agent_name, connection_id, spawned_at, last_pinged_at, state, metadata, routing_chat_id, ...`); (2) migration in `src/store.rs` (actual: `src/daemon/chat.rs`); (3) phantom path `src/agent_registry.rs` (actual: `src/daemon/agent_registry.rs`); (4) wrong chat_messages column `thread` (actual: `thread_id`). All four corrected based on architect-grounded reading of `src/daemon/chat.rs:418-552`. Risk: any plan or test files authored from the pre-amendment PRD may carry the same errors; ba-analyst and qa-planner running in parallel MUST be notified. Salience: high.

### Open questions

- **OQ-C2C-1:** Does bridge auto-subscribe to `agent:<my-id>` need to be project-scoped (subscribe only when in matching `project_id`)? MVP proposal: NO — subscribe always; rely on `target_agent_id` filter. Confirm at Slice 4 implementation — needs: architect call at Slice 4 start. Salience: low.
- **OQ-C2C-2:** Should `agent_send` to a non-existent `agent_id` fail loudly (error response) or enqueue with `delivered_when: null` and wait for the agent to register? MVP proposal: fail loudly (FR-C2C-4.2). Confirm at bootstrap — needs: operator decision or architect call. Salience: low.
- **OQ-C2C-3:** Should the `PostToolUse:ExitPlanMode` hook ALSO fire on `UserPromptSubmit` to catch mid-session feature drift (R-C2C-2)? MVP proposal: defer. Confirm at bootstrap — needs: operator decision. Salience: medium.

## Decisions

### Inbound validation

- Task received: write PRD §20 (cli-to-cli-routing) from `.claude/plan.md` as source of truth. Challenged: yes — §19 already existed (v0.9-cut) and had been listed in the task prompt as "§18 multi-agent-telegram"; the correct section number is §20, not §19 as naively stated. Protocol 3 Q1: task is coherent. Q2: no upstream error — the plan is approved. Q4: no amplification. Outcome: proceeded with §20 as correct section number, sourcing all FRs from plan.md lines 72–103. Salience: high.
- Insights corpus hit (doc_id=5) confirmed that the design choice of MCP-mandated `feature_description` via PostToolUse hook was an operator correction already applied in this session. Inbound task is consistent with that correction. No contradiction. Salience: high.
- **Amendment task received 2026-06-06:** Architect PASS-WITH-CONDITIONS verdict identified 4 CRITICAL and 7 MAJOR factual errors in PRD §20. Protocol 3 Q1: amendment task is coherent — the errors are concrete and evidence-grounded. Q2: the original PRD §20 contained upstream errors (wrong column names, wrong file paths, wrong column name) that would propagate to every downstream agent (planner, test-writer, implementer). Q4: executing amendment is correct; NOT executing would amplify the upstream errors. Outcome: applied all 11 amendments (CRITICAL-1 through MAJOR-11 + OQ-UC-C2C-1 resolved). Salience: high.

### Decisions made

- Section number §20 chosen (§19 was already taken by the v0.9-cut PRD section). Q1 hack? no | Q2 sane? yes — sequential numbering is the only coherent choice | Q3 alternatives? n/a | Q4 cause | Q5 n/a. Salience: low.
- AC-C2C-1 through AC-C2C-5 copied verbatim from `.claude/plan.md` §Success Criteria table (lines 38–42). Decision: no paraphrase — exact copy ensures qa-engineer's `/qa-cycle` targets the same criterion the planner designed. Q1 hack? no | Q2 sane? yes | Q3 alternatives? paraphrase rejected (drift risk) | Q4 cause | Q5 n/a. Salience: high.
- FR numbering scheme: FR-C2C-1 through FR-C2C-8 (mirroring slice 1–8 structure from plan.md). Q1 hack? no | Q2 sane? yes — 1:1 with implementation slices aids traceability | Q3 alternatives? topic-based numbering rejected (harder to trace to plan) | Q4 cause | Q5 n/a. Salience: medium.
- Schema DDL for v5→v6 migration presented as a SQL block. Decision: exact DDL is more useful than prose for the implementer and less ambiguous for reviewers. Q1 hack? no | Q2 sane? yes | Q3 alternatives? prose only rejected | Q4 cause | Q5 n/a. Salience: medium.
- Trust model (single-box single-user, no prompt-injection guard) documented in §20.1 and NFR-C2C-4, not hidden. Decision: explicit scope decision, not a shortcut. Q1 hack? no (operator-confirmed 2026-06-05) | Q2 sane? yes | Q3 alternatives? sanitize-and-escape considered, rejected for MVP per operator | Q4 cause | Q5 future hardening tracked in §20.8 Out of Scope. Salience: high.
- **Architect action A-1 (migration file location):** All FR references to `src/store.rs` replaced with `src/daemon/chat.rs::apply_agent_registry_c2c_migration`. Decision basis: verified at `src/daemon/chat.rs:418-472` this session; no `store.rs` migration function exists. Q1 hack? no | Q2 sane? yes | Q3 alternatives? creating it in `store.rs` rejected (breaks existing pattern; `store.rs` does not handle `chat.db` schema) | Q4 cause (fixing wrong file reference) | Q5 n/a. Salience: high.
- **Architect action A-2 (column names):** FR-C2C-1.5 rewritten with actual columns from `src/daemon/chat.rs:443-459` and `508-518`. Decision: verbatim from source; no interpretation. Q1 hack? no | Q2 sane? yes | Q3 n/a | Q4 cause | Q5 n/a. Salience: high.
- **Architect action A-3 (phantom path):** `src/agent_registry.rs` → `src/daemon/agent_registry.rs` across Modified files list. Decision: correct path to match project structure (`src/daemon/` submodule pattern). Q1 hack? no | Q2 sane? yes | Q3 n/a | Q4 cause | Q5 n/a. Salience: medium.
- **Architect action A-4 (chat_messages column):** `chat_messages.thread` → `chat_messages.thread_id` in all FR references, drain SQL, and Schema Changes section. Decision: verbatim from `src/daemon/chat.rs:431`. Q1 hack? no | Q2 sane? yes | Q3 n/a | Q4 cause | Q5 n/a. Salience: high.
- **Architect action A-5 (sender identity binding FR-C2C-4.6):** Added as new security requirement rather than guidance. Decision: binding `from_agent_id` to connection state is the load-bearing security property that prevents impersonation over UDS. Q1 hack? no | Q2 sane? yes — mirrors an existing pattern | Q3 alternatives? caller-supplied identity rejected (trivially bypassable) | Q4 cause (prevents impersonation) | Q5 n/a. Salience: high.
- **FR-C2C-5.5 (DND drain rate limit):** 10 messages per agent per 30s tick. Decision: protects CC channel surface from N-message bombardment on DND-off. Q1 hack? no | Q2 sane? yes — 10/30s is an operationally reasonable limit | Q3 alternatives? unlimited drain (rejected: bombardment risk), 1/tick (too slow for typical case), configurable (premature complexity) | Q4 cause | Q5 n/a. Salience: medium.
- **Indefinite DND as i64::MAX:** Encoding `"on"` without duration as `i64::MAX` rather than a sentinel NULL. Decision: NULL is already "no DND"; using MAX avoids a three-state (on/off/indefinite) enum in the drain WHERE clause. Q1 hack? no | Q2 sane? yes — `WHERE dnd_until_ts < now()` naturally excludes MAX | Q3 alternatives? separate `dnd_indefinite BOOLEAN` column (rejected: adds schema complexity for no benefit) | Q4 cause | Q5 n/a. Salience: medium.

### Hacks / workarounds acknowledged

(none — this feature is principled wiring of existing infrastructure; no shortcuts taken)

### Symptom-only patches (with root-cause links)

- Mid-session feature switch (operator switches task without entering plan mode) leaves `feature_description` stale in the daemon. Symptom treated: hook mandates update only on ExitPlanMode. Root cause that remains: no reliable CC lifecycle event fires on task change outside plan mode. Tracked at: R-C2C-2 in §20.9 + OQ-C2C-3. Salience: medium.
