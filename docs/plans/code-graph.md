# Plan: `claudebase code-graph` — deterministic multi-language call graph

**Owner:** Mira (orchestrator)
**Status:** draft — concept only, NOT scheduled for implementation
**Created:** 2026-05-25

**Relation to other plans:**
- **Independent** of the orchestration quartet (`claudebase-server-foundation.md`, `agent-registry-multi-cli.md`, `telegram-multi-cli-orchestration.md`, `claudebase-project-dir.md`). The code-graph is a *local retrieval feature* — a new retrieval primitive alongside the books corpus and insights corpus, not a networked service.
- Shares infrastructure with the existing corpora: the same per-project SQLite (`<project>/.claude/knowledge/`), the same sha256+mtime incremental-ingest idempotency, the same single-binary / no-Python / no-Node constraint.

## Goal

A deterministic, **LLM-free** call graph of a codebase that the agent can query by function name and instantly get the full call tree — callers and callees — to see how a function intersects with the rest of the system.

> The agent runs one query — `code-graph query <function>` — and receives the
> dependency-ordered tree of what that function calls and what calls it, across
> files, so it understands "what breaks if I touch this" and "where does this
> fit" without reading the whole codebase.

Two load-bearing properties:

- **No LLM in the graph.** The graph is built by deterministic parsing only. LLM summaries (the part that goes stale and lies — see `## Why LLM-free` below) are explicitly out of scope. The graph is a *retrieval primitive*, not generated documentation.
- **Self-rebuilding.** The tool maintains its own graph incrementally — on each run it re-parses only the files whose sha256 changed since last index, reusing the corpus idempotency machinery that already exists.

## Why LLM-free

The reference project that prompted this concept ([Lum1104/Understand-Anything](https://github.com/Lum1104/Understand-Anything)) pairs a deterministic tree-sitter skeleton with an LLM-generated semantic overlay (architectural-layer classification, business-domain mapping, plain-English summaries). The skeleton is trustworthy; the overlay is the weak link:

- **Staleness.** A 200k-LOC graph's deterministic edges can be kept fresh cheaply (re-parse changed files). LLM-generated prose cannot — it is expensive to regenerate and drifts from the code it describes. Documentation that lies is worse than none.
- **Confident wrongness.** LLM layer-classification on a messy real codebase is often wrong-with-confidence.

Decision: **keep the deterministic graph; drop the pre-baked LLM overlay.** If natural-language summaries are ever wanted, generate them *lazily at query time and cache with fingerprint-invalidation* — never pre-bake. (Out of scope for the MVP; noted as a possible later layer.)

## The core technical reality (load-bearing)

The expensive, per-language part of a call graph is **name resolution**, not AST parsing.

- **AST + call-sites are cheap and universal.** [tree-sitter](https://github.com/tree-sitter) has ~305 ready grammars ([`tree-sitter-language-pack`](https://crates.io/crates/tree-sitter-language-pack), Rust, on-demand). Extracting "here is a function definition" and "here is a call-site" is one small query file per language.
- **Binding a call-site to its definition is hard and per-language.** Turning "there is a call to `foo()` here" into "this calls THE `foo` defined in module X" requires scoping, imports, overload resolution, method dispatch — semantics that differ fundamentally across languages (Python duck-typing vs Java static dispatch vs Go interfaces vs JS prototype chain). **This is why no single turnkey "call graph for all languages" Rust library exists.**

The architectural choice is therefore a precision-vs-effort dial, documented below.

## Landscape (existing Rust-native building blocks, verified 2026-05-25)

| Tool | Gives | Languages | Fit for claudebase |
|---|---|---|---|
| [github/stack-graphs](https://github.com/github/stack-graphs) | Precise cross-file name-binding (def↔ref), file-incremental, Rust | Python, TS/TSX, JS, Java mature; new language = author DSL rules (heavy) | Precise upgrade for top languages. NB: it is name-binding, **not** a call graph — call edges are derived on top (call-site ref → resolved def). |
| [tree-sitter/tree-sitter-graph](https://github.com/tree-sitter/tree-sitter-graph) | DSL to build arbitrary graphs from a parse tree, Rust | any tree-sitter language | Building block (stack-graphs is built on it). |
| [sourcegraph/scip](https://github.com/sourcegraph/scip) | Protobuf code-intel format (defs + references) | one indexer **binary per language** (scip-typescript, scip-java, scip-clang, rust-analyzer…) | Precise but a polyglot zoo of binaries — **breaks the single-binary constraint**. Rejected as a core dependency. |
| [mozilla/rust-code-analysis](https://github.com/mozilla/rust-code-analysis) | Metrics + structural analysis, tree-sitter | multi-language | Metrics-oriented, not call-graph. Reference only. |

No drop-in "multi-language call graph as a Rust crate" exists. The pattern (tree-sitter → dependency graph → RAG) has been hand-rolled by others ([CodeRAG write-up](https://medium.com/@shsax/how-i-built-coderag-with-dependency-graph-using-tree-sitter-0a71867059ae)) but not packaged as a library.

## Decision: approximate-first, precise-later

```
                 precision
                    ▲
   stack-graphs ────┤  precise, per-language DSL (heavy)  — Phase 3, top langs only
                    │
   name-match    ───┤  approximate, 1 query file/lang (cheap) — Phase 1 MVP, all langs
   on tree-sitter   │
                    └──────────────────────────────►  language coverage / effort
```

**Phase 1 MVP = approximate call graph on raw tree-sitter.** Per language: one existing grammar + one small query file extracting `(definition name)` and `(call-site callee-name)`. Link a call-site `foo()` to candidate definitions named `foo`, scoped by file/imports where the grammar makes it cheap. This is **over-approximate** — false edges on name collisions, misses dynamic/method dispatch — and that is acceptable for the exploration use-case: the agent disambiguates from context, and candidate edges are exactly "where this *might* intersect." **The approximation MUST be labelled in every query result** (e.g. `confidence: name-match`) so neither the agent nor the operator mistakes it for precise resolution. Shipping an approximate graph as if it were precise would be the named decision-shaped hack this project must avoid.

**Phase 3 upgrade = wrap stack-graphs for the 4 mature languages** (Python/TS/JS/Java) to produce precise edges where rules exist; approximate everywhere else. Query results tag each edge `confidence: resolved | name-match`.

## Architecture

```
 source files ──► tree-sitter parse (per-lang grammar)
                        │
                        ├─► extract defs   (query: function/method definitions)
                        └─► extract calls  (query: call-sites + callee name)
                        │
                   name-match linker (Phase 1)  /  stack-graphs resolver (Phase 3)
                        │
                        ▼
            SQLite graph tables  (same .claude/knowledge/ db family)
                        │
        query ◄─────────┴──────► MCP tool  code_graph_query(function)
   code-graph query <fn>              (rides existing claudebase MCP infra)
```

### SQLite schema (sketch)

```sql
-- nodes: every definition we can name
create table code_nodes (
    id          integer primary key,
    kind        text not null,          -- function | method | class
    name        text not null,
    file_path   text not null,
    start_line  integer, end_line integer,
    lang        text not null,          -- python | typescript | rust | ...
    file_sha    text not null           -- for incremental invalidation
);
create index idx_code_nodes_name on code_nodes(name);

-- edges: call-site -> candidate definition
create table code_edges (
    caller_id   integer references code_nodes(id) on delete cascade,
    callee_id   integer references code_nodes(id) on delete cascade,
    call_line   integer,
    confidence  text not null,          -- resolved | name-match
    primary key (caller_id, callee_id, call_line)
);

-- per-file fingerprint, mirrors the books-corpus idempotency
create table code_files (
    file_path text primary key,
    sha256    text not null,
    lang      text not null,
    mtime     text
);
```

### Per-language query-file format

One file per language, e.g. `queries/python.scm` (tree-sitter S-expression queries):

```scheme
; definitions
(function_definition name: (identifier) @def.name) @def
; call sites
(call function: (identifier) @call.callee) @call
(call function: (attribute attribute: (identifier) @call.method)) @call
```

Adding a language = drop a grammar (already available) + author one `.scm` query file. This is the "minimal updates per language" the operator wants — for the *approximate* tier. (The *precise* tier per language is the heavy stack-graphs DSL, deferred to Phase 3.)

### Incremental rebuild

Reuse the existing sha256+mtime idempotency from the books-corpus ingest: on `code-graph build`, hash each source file; re-parse only changed files; delete + re-insert that file's nodes/edges (FK cascade handles cleanup). Unchanged files are skipped. Self-maintaining with no external trigger beyond running the build (later: a file-watch or pre-commit hook could call it).

### Query shape

```
claudebase code-graph query <function> [--callers] [--callees] [--depth N] [--json]
```

Returns the dependency-ordered call tree (callees by default; `--callers` for the inverse), each edge tagged `confidence`. JSON output for the MCP tool; human tree for the CLI.

### Integration

- **Subcommand:** `claudebase code-graph {build,query,status}` in the same Rust binary (tree-sitter crates as deps). No new process, no new language runtime.
- **MCP tool:** `code_graph_query(function_name, direction, depth)` exposed over the existing claudebase MCP surface (the channel/plugin bridge already speaks MCP), so the agent gets the call tree in one tool call.

## Phases

| Phase | Scope | Done-when |
|---|---|---|
| 1 — MVP | tree-sitter parse + approximate name-match linker + SQLite schema + `code-graph build`/`query` CLI for 3 seed languages (Python, TypeScript, Rust) | `code-graph query <fn>` returns a call tree with `confidence: name-match` edges; incremental rebuild re-parses only changed files |
| 2 — coverage + MCP | +5 languages (one `.scm` query file each) + `code_graph_query` MCP tool + `--callers`/`--depth` | agent retrieves a call tree via one MCP call; 8 languages supported |
| 3 — precise upgrade | wrap stack-graphs for Python/TS/JS/Java; edges tagged `resolved` where available, `name-match` elsewhere | precise edges on the 4 mature languages; query result distinguishes confidence |

## Open questions / risks

- **Approximate-graph noise.** If name-match false-edge rate is too high to be useful for exploration, Phase 3 (stack-graphs) becomes mandatory sooner. Needs a real-codebase eval before committing to approximate-only.
- **Method / attribute calls.** `obj.foo()` name-match across all `foo` methods is very noisy. May need a per-language heuristic (limit to imported/in-file scope) even in the approximate tier.
- **Graph size at scale.** Reference project caps the graph at ~10 MB without git-lfs. SQLite handles larger, but query traversal depth needs a sane default cap to avoid runaway trees on hot functions.
- **Build trigger.** MVP runs on explicit `code-graph build`. Auto-rebuild (file-watch, pre-commit hook, or piggyback on `claudebase ingest`) is a Phase-2+ decision.
- **Relationship to the books corpus search.** Should `claudebase search` ever fuse code-graph hits with text hits (RRF across corpora, like `--corpus all`)? Possible future unification; out of scope here.

## Facts grounding this concept

All tool capabilities below were verified via live web search on 2026-05-25 (not training-data recall):

- tree-sitter ecosystem has ~305 ready grammars via `tree-sitter-language-pack` (Rust). — source: https://crates.io/crates/tree-sitter-language-pack — verified: yes
- `github/stack-graphs` is a Rust name-resolution (def↔ref) library, file-incremental, with mature language definitions for Python, TypeScript/TSX, JavaScript, Java; it is name-binding, not a call graph. — source: https://github.com/github/stack-graphs + https://github.blog/open-source/introducing-stack-graphs/ — verified: yes
- `tree-sitter-graph` is a Rust DSL for constructing graphs from parse trees; stack-graphs is built on it. — source: https://github.com/tree-sitter/tree-sitter-graph — verified: yes
- SCIP (Sourcegraph) is a protobuf code-intel format with a separate indexer binary per language (scip-typescript, scip-java, scip-clang, rust-analyzer emits SCIP). — source: https://github.com/sourcegraph/scip — verified: yes
- No single turnkey "multi-language call graph as a Rust crate" surfaced in search; the tree-sitter→dependency-graph pattern is hand-rolled by practitioners, not packaged. — verified: yes (absence-of-result, 2026-05-25 search)
- The SQLite schema, per-language `.scm` query format, and the approximate-vs-precise phasing are **this concept's design proposals**, not externally verified — they are the author's architecture, to be validated at implementation time. — verified: no — design assumption
- claudebase's existing sha256+mtime incremental-ingest idempotency is reused for the code-graph rebuild. — source: claudebase books-corpus ingest (referenced from `~/.claude/rules/knowledge-base-tool.md` § per-document transactional ingest) — verified: yes (documented behavior; not re-read from source this session — treat the exact reuse surface as an implementation-time check)
