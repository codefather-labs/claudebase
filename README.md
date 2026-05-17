# claudebase

> Local hybrid retrieval CLI for LLM agents — BM25 + dense embeddings + Reciprocal Rank Fusion, with multimodal OCR for figures, per-page navigation, and a parallel insights corpus that lets agents persist cognitive observations across sessions.

`claudebase` indexes a directory of PDF / Markdown / plain-text documents into a single SQLite file (`<project>/.claude/knowledge/index.db`) and serves three retrieval modes over the same chunks:

- **lexical** — SQLite FTS5 BM25; fast and exact-keyword-friendly
- **dense** — 384-dim e5-multilingual-small embeddings via `sqlite-vec`; semantic + cross-lingual recall
- **hybrid** (default) — BM25 ⊕ dense fused via Reciprocal Rank Fusion (k=60); the best of both

Designed to be invoked by Claude Code agents — every search hit carries the source path, chunk position, BM25 / dense / RRF scores, and (for PDFs) the 1-indexed page number, so the LLM can cite verifiable evidence and navigate the source book by page.

Alongside the read-side **books corpus**, claudebase v0.5.0 ships a write-side **insights corpus** (`<project>/.claude/knowledge/insights.db`) that lets agents persist their own cognitive observations — drift findings, prediction-errors, peer-bias catches, self-corrections — and recall them in future sessions. The hippocampal-replay analogue for cross-session agent memory. See [Two corpora — books and insights](#two-corpora--books-and-insights) below.

## Quick install

The recommended path is via [`claude-code-sdlc`](https://github.com/codefather-labs/claude-code-sdlc), which installs `claudebase` as part of its agent toolkit:

```bash
curl -fsSL https://raw.githubusercontent.com/codefather-labs/claude-code-sdlc/main/install.sh | bash -s -- --yes
```

This downloads the platform-appropriate binary from this repo's releases (`claudebase-darwin-arm64`, `claudebase-linux-x64`, `claudebase-windows-x64.exe`), places it at `~/.claude/tools/claudebase/claudebase`, registers a global `claudebase` alias, and wires up the e5 encoder + pdfium dynamic library.

For a standalone install without the agent SDK:

```bash
# darwin-arm64 example; substitute your platform
curl -fsSL -o ~/.local/bin/claudebase \
  https://github.com/codefather-labs/claudebase/releases/latest/download/claudebase-darwin-arm64
chmod +x ~/.local/bin/claudebase
```

## Subcommands

**Books corpus** (`index.db`) — user-curated PDF/MD/TXT for RAG-style retrieval:

```text
claudebase ingest <path>                 ingest a file or directory (PDF/MD/TXT)
claudebase search <query> [--mode M]     M ∈ {lexical, dense, hybrid}; default hybrid
                          [--top-k N]    top-K hits (default 5)
                          [--context N]  ±N neighbor chunks per hit (~one page at N=2)
                          [--json]
claudebase compare <query>               A/B-test all 3 modes side-by-side
claudebase page <doc> <N> [--range R]    raw text of page N (or [N-R..N+R]); 1-indexed
claudebase reindex-pages [--doc X]       backfill pages table for legacy v2 indexes
claudebase list                          enumerate indexed sources
claudebase status                        schema_version + doc/chunk counts + db_path
claudebase delete <source-path>          remove a source and its chunks
claudebase warmup [--quiet]              pre-load encoder model (~30s first run)
```

**Insights corpus** (`insights.db`) — agent-written cognitive observations, opt-in per project:

```text
claudebase insight create <body>         persist an agent's cognitive observation
                          --type <kind>  agent-learned | self-bias-caught |
                                         peer-bias-observed | red-team-objection |
                                         consolidator-drift | prediction-error |
                                         assumption-falsified | plan-reality-gap |
                                         reflection-observation | operator-correction
                          --agent <name> emitting agent (planner, reflection, ...)
                          [--feature SLUG] [--salience high|medium|low] [--session ID]
                          [--source-artifact REF]
claudebase insight search <query>        hybrid retrieval over the insights corpus
                          [--mode M] [--top-k N] [--type T] [--agent A]
                          [--salience S] [--feature F] [--since <Nd|Nh|Nm|Nw>]
claudebase insight list                  newest-first, 10 per page
                          [--offset N] [--page-size N] [filters]
claudebase insight random [filters]      uniformly-sampled single insight
claudebase insight get <id|sha-prefix>   fetch one by integer id or ≥4-hex sha prefix
claudebase insight gc [--dry-run]        salience-driven TTL purge + VACUUM
claudebase insight delete <id>           single-row delete with chunks + vec cascade
```

**Cross-corpus search:**

```text
claudebase search <query> --corpus all   RRF-fuse hits from both books and insights
                                         (each hit tagged with source_corpus)
```

All subcommands accept `--project-root <dir>` (defaults to current working directory) and `--json` for structured output. Insight bodies can come from positional arg, `-`, or piped stdin (a TTY without a body is rejected — the surface is designed for non-interactive agent use).

## Architecture quick-tour

| Concern | Implementation |
|---|---|
| Lexical retrieval | SQLite FTS5 BM25 with `unicode61` tokenizer |
| Dense retrieval | `sqlite-vec` v0.1.9 vec0 virtual table (L2 over 384-dim unit-norm vectors → cosine-equivalent ranking) |
| Encoder | `intfloat/multilingual-e5-small` ONNX via `fastembed-rs` v5; `passage:` / `query:` prefix discipline enforced |
| Fusion | Reciprocal Rank Fusion with k=60 (Cormack/Clarke/Buttcher 2009) |
| PDF extraction | `pdfium-render` v0.9 (CID fonts, calibre-converted PDFs, multi-column layouts handled) |
| OCR (image chunks) | `ocr-rs` v2 / PaddleOCR PP-OCRv4 via MNN runtime |
| Books-corpus storage | Single `index.db` SQLite file per project — no co-located figure files; image bytes as BLOB |
| Insights-corpus storage | Separate `insights.db` SQLite file per project — same schema shape (chunks_fts + chunks_vec) plus an `insights` metadata table for type/agent/salience/feature/session/source-artifact; cascade-deletes through chunks and chunks_vec |
| Insights dedup | Exact-sha within `(agent, sha256(body))` over 30 days → `status: deduped`; semantic cosine > 0.92 from same agent over 30 days → `status: near-duplicate`. Cross-agent agreement on the same body is intentionally NOT deduped (load-bearing signal) |

For the deep-dive — including the L2/cosine equivalence math, why hybrid beats either pure mode, the e5 prefix asymmetry contract, and the full RRF derivation — see [`docs/architecture/technical-decisions.md`](docs/architecture/technical-decisions.md).

For the headline benchmark numbers (+75% Recall@5 vs lexical baseline on the 12-query golden set), see [`docs/benchmarks/2026-05-10-baseline.md`](docs/benchmarks/2026-05-10-baseline.md).

## Two corpora — books and insights

`claudebase` stores two SQLite databases per project, side by side under `<project>/.claude/knowledge/`. They use the same retrieval engine and the same hybrid scoring, but they serve opposite directions of the agent's information flow:

| | Books corpus (`index.db`) | Insights corpus (`insights.db`) |
|---|---|---|
| **Direction** | Read-side. The user feeds it; agents query it. | Write-side. Agents feed it; agents query it (and the user audits it). |
| **Content** | Curated PDFs, Markdown, plain text — books, regulatory docs, internal style guides, domain references. | Cognitive observations emitted by agents — drift findings, prediction-errors, peer-bias catches, self-corrections, DMN observations. |
| **Lifecycle** | Stable; changes only when the user re-ingests a document. | Dynamic; grows across every session. `gc` prunes by TTL. |
| **Activation** | Present when `index.db` exists (user runs `claudebase ingest …`). | Opt-in; created on the first `insight create` call. A project that never adopts insights stays byte-identical to one that never heard of it. |
| **Why it exists** | Extend agent expertise with project-specific domain content not in training data. | Persist load-bearing cognitive insights across sessions — without it, every Claude Code session re-discovers what previous sessions already learned. |

### Three-axis taxonomy for insights

The `--type` field on `insight create` is a small open enum, organized along three cognitive axes:

| Axis | `--type` values | When to emit |
|---|---|---|
| **Self-learning** | `agent-learned`, `self-bias-caught` | The agent noticed it learned something new, or caught a blind spot in its own prior reasoning. |
| **Peer-bias / drift detection** | `peer-bias-observed`, `red-team-objection`, `consolidator-drift` | The agent observed a cognitive bias or drift in another agent's output or in upstream artifacts. |
| **Prediction-reality mismatch** | `prediction-error`, `assumption-falsified`, `plan-reality-gap` | Planned/expected/predicted did not match what actually happened (Friston-style prediction error). |
| Special | `reflection-observation`, `operator-correction` | DMN observations from the reflection agent; insights from operator corrections worth carrying forward. |

Factual findings, mechanical execution narration, and generic best-practice claims do **not** belong in the corpus — they go to PRs, scratchpads, or stay silent.

### Salience drives retention

Every insight carries a `--salience` tag with three values, which `claudebase insight gc` uses to age out stale rows:

| Salience | Retention | Use for |
|---|---|---|
| `high` | indefinite (never gc'd) | Insights whose loss would degrade the entire pipeline. Use sparingly. |
| `medium` | 365 days | Slice-level or single-decision insights. Default. |
| `low` | 90 days | Ambient / context-setting observations. Cheap to lose. |

Be honest with the tag — marking everything `high` defeats the purge and turns the corpus into a write-only log.

### Recall at the start of every task

The agent SDLC contract (in `claude-code-sdlc/src/rules/knowledge-base-tool.md`) makes recall mandatory at task-receipt for in-scope thinking agents:

```text
claudebase insight search "<feature-keywords>" \
    --feature "$FEATURE_SLUG" --salience high --top-k 5 --json
```

Load-bearing hits are cited verbatim in the agent's `## Facts → ### Verified facts` block under the prefix `insights-base:` — greppable for reviewer audits, byte-parallel to the `knowledge-base:` prefix used for books-corpus citations.

### Books vs insights — which to query for what

| Question | Right corpus |
|---|---|
| "What does the SQL spec say about FTS5?" | books (`claudebase search`) |
| "What did reflection notice last session about the consent flow?" | insights (`claudebase insight search`) |
| "How does Kafka's exactly-once delivery work?" | books |
| "Did a prior planner flag this scope as oversized?" | insights |
| Genuinely spans both | `claudebase search --corpus all` (RRF-fused; each hit tagged with `source_corpus`) |

## Repository layout

```
claudebase/
├─ src/                              Rust source (cli.rs, store.rs, search.rs,
│                                    ingest.rs, encoder.rs, ocr.rs, pdf.rs,
│                                    chunker.rs, migrations.rs, parser.rs, ...)
├─ tests/                            Integration tests + fixtures
├─ bench/                            Benchmark harness (claudebase-bench binary)
│  ├─ runner.rs
│  ├─ golden/queries.jsonl           12-query golden set
│  └─ reports/                       Benchmark output (gitignored)
├─ docs/                             Self-contained product documentation
│  ├─ PRD.md                         Product requirements
│  ├─ design.md                      System design
│  ├─ use-cases.md                   User stories + scenarios
│  ├─ qa.md                          Acceptance criteria + test cases
│  ├─ architecture/
│  │  └─ technical-decisions.md      Stack rationale + L2/cosine math + walkthrough
│  ├─ benchmarks/
│  │  └─ 2026-05-10-baseline.md      Golden-set benchmark numbers
│  └─ article/                       Medium-article staging
├─ .github/workflows/release.yml     5-platform release matrix on claudebase-v* tags
├─ Cargo.toml                        Standalone Rust crate
├─ RELEASING.md                      Release procedure
├─ LICENSE                           MIT
└─ README.md                         (this file)
```

## Versioning + history

`claudebase` was extracted from the [`claude-code-sdlc`](https://github.com/codefather-labs/claude-code-sdlc) monorepo's `tools/sdlc-knowledge/` crate on 2026-05-10. Versioning continues from the last sdlc-knowledge release: **claudebase v0.4.0** is the direct successor to **sdlc-knowledge v0.4.0**. **v0.5.0** (2026-05-15) added the agent-insights corpus and the `insight` subcommand family. Pre-extraction history is preserved in the SDLC monorepo's git log up to commit [`ca3ecb5`](https://github.com/codefather-labs/claude-code-sdlc/commit/ca3ecb5).

The CLI was renamed from `claudeknows` to `claudebase`. Existing installations that ran `claude-code-sdlc/install.sh` before this date have an auto-migration path — the installer detects the old `~/.claude/tools/sdlc-knowledge/` directory and `claudeknows` symlink and removes them on next run.

## License

MIT — see [LICENSE](LICENSE).
