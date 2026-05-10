# claudebase

> Local hybrid retrieval CLI for LLM agents — BM25 + dense embeddings + Reciprocal Rank Fusion, with multimodal OCR for figures and per-page navigation.

`claudebase` indexes a directory of PDF / Markdown / plain-text documents into a single SQLite file (`<project>/.claude/knowledge/index.db`) and serves three retrieval modes over the same chunks:

- **lexical** — SQLite FTS5 BM25; fast and exact-keyword-friendly
- **dense** — 384-dim e5-multilingual-small embeddings via `sqlite-vec`; semantic + cross-lingual recall
- **hybrid** (default) — BM25 ⊕ dense fused via Reciprocal Rank Fusion (k=60); the best of both

Designed to be invoked by Claude Code agents — every search hit carries the source path, chunk position, BM25 / dense / RRF scores, and (for PDFs) the 1-indexed page number, so the LLM can cite verifiable evidence and navigate the source book by page.

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

All subcommands accept `--project-root <dir>` (defaults to current working directory) and `--json` for structured output.

## Architecture quick-tour

| Concern | Implementation |
|---|---|
| Lexical retrieval | SQLite FTS5 BM25 with `unicode61` tokenizer |
| Dense retrieval | `sqlite-vec` v0.1.9 vec0 virtual table (L2 over 384-dim unit-norm vectors → cosine-equivalent ranking) |
| Encoder | `intfloat/multilingual-e5-small` ONNX via `fastembed-rs` v5; `passage:` / `query:` prefix discipline enforced |
| Fusion | Reciprocal Rank Fusion with k=60 (Cormack/Clarke/Buttcher 2009) |
| PDF extraction | `pdfium-render` v0.9 (CID fonts, calibre-converted PDFs, multi-column layouts handled) |
| OCR (image chunks) | `ocr-rs` v2 / PaddleOCR PP-OCRv4 via MNN runtime |
| Storage invariant | Single `index.db` SQLite file per project — no co-located figure files; image bytes as BLOB |

For the deep-dive — including the L2/cosine equivalence math, why hybrid beats either pure mode, the e5 prefix asymmetry contract, and the full RRF derivation — see [`docs/architecture/technical-decisions.md`](docs/architecture/technical-decisions.md).

For the headline benchmark numbers (+75% Recall@5 vs lexical baseline on the 12-query golden set), see [`docs/benchmarks/2026-05-10-baseline.md`](docs/benchmarks/2026-05-10-baseline.md).

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

`claudebase` was extracted from the [`claude-code-sdlc`](https://github.com/codefather-labs/claude-code-sdlc) monorepo's `tools/sdlc-knowledge/` crate on 2026-05-10. Versioning continues from the last sdlc-knowledge release: **claudebase v0.4.0** is the direct successor to **sdlc-knowledge v0.4.0**. Pre-extraction history is preserved in the SDLC monorepo's git log up to commit [`ca3ecb5`](https://github.com/codefather-labs/claude-code-sdlc/commit/ca3ecb5).

The CLI was renamed from `claudeknows` to `claudebase`. Existing installations that ran `claude-code-sdlc/install.sh` before this date have an auto-migration path — the installer detects the old `~/.claude/tools/sdlc-knowledge/` directory and `claudeknows` symlink and removes them on next run.

## License

MIT — see [LICENSE](LICENSE).
