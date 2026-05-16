# Use Cases — Agent Insights Base

PRD reference: `docs/PRD.md` §16. Design source-of-truth: `docs/design/agent-insights-base.md`.

## Actors

| Actor | Description |
|-------|-------------|
| **SDLC Agent** | One of the 16 in-scope thinking agents (the 13 cognitive-self-check thinking agents + `reflection` / `consolidator` / `red-team`). Calls `claudebase insight create / search` from its own prompt-driven workflow. Always invokes the binary via a non-TTY pipe (stdin or `--body` literal). Never a human at a keyboard. |
| **Pipeline Operator (Vlad)** | Human running `/develop-feature` / `/qa-cycle` / `/consolidate`. Interacts with the insights corpus indirectly via the agents; may run admin commands (`list / random / get / gc / delete`) interactively. |
| **claudebase CLI binary** | Local Rust binary at `~/.claude/tools/claudebase/claudebase` (also `claudebase` via PATH alias). Single-process per invocation; the `Insight` subcommand tree opens `<project>/.claude/knowledge/insights.db` exclusively. |
| **SQLite + sqlite-vec + e5 encoder** | Storage + retrieval engine. The encoder is optional at write time (best-effort dense-vector population); the FTS5 BM25 path is always present. |

## Use Case Coverage

| Use Case | PRD Functional Requirements Covered |
|----------|--------------------------------------|
| UC-AIB-1: Agent emits a cognitive insight via `insight create` (happy path) | FR-AIB-3.1, 3.2 (positional body), 3.5, 3.6 |
| UC-AIB-2: Agent emits the same body twice in one session — exact-sha dedup | FR-AIB-3.3 |
| UC-AIB-3: Two different agents emit the same body — both rows kept | FR-AIB-3.4 |
| UC-AIB-4: Agent retrieves prior insights via `insight search` at task-receipt | FR-AIB-4.1, 4.2 |
| UC-AIB-5: Operator lists insights with pagination, newest-first | FR-AIB-5.1, 5.2 |
| UC-AIB-6: Operator picks a random insight (e.g. for spot-checking corpus quality) | FR-AIB-5.3 |
| UC-AIB-7: Operator fetches one insight by id or sha prefix | FR-AIB-5.4, 5.5, 5.6 |
| UC-AIB-8: Books-corpus untouched by insight writes | NFR-AIB-4 |
| UC-AIB-9 (PLANNED — Slice 5): Semantic-dedup near-duplicate skipped | FR-AIB-6 |
| UC-AIB-10 (PLANNED — Slice 6): Cross-corpus search via `--corpus all` | FR-AIB-7 |
| UC-AIB-11 (PLANNED — Slice 7): TTL gc purges expired insights | FR-AIB-8 |
| UC-AIB-12 (PLANNED — Slice 8): SDLC agent retrieves insights at task receipt | FR-AIB-9 |
| UC-AIB-EC-1: Empty body rejected | FR-AIB-3.2 (boundary) |
| UC-AIB-EC-2: TTY without body refused | FR-AIB-3.2 (boundary) |
| UC-AIB-EC-3: Encoder missing — lexical-only retrieval still works | FR-AIB-4.2 |
| UC-AIB-EC-4: Empty insights corpus — `random` exits 1 | FR-AIB-5.3 (boundary) |
| UC-AIB-EC-5: `get` with too-short prefix — exit 2 | FR-AIB-5.5 |

## UC-AIB-1: Agent Emits a Cognitive Insight (Happy Path)

**Actor:** SDLC Agent (e.g., `reflection`)
**Preconditions:**
- Branch `<project>/.claude/knowledge/` exists (created on first `claudebase` invocation if absent).
- The body is a real cognitive insight matching one of the three axes (self-learning / peer-bias / prediction-reality).

**Main flow:**
1. Agent constructs the body text in its workflow.
2. Agent invokes: `claudebase insight create "kafka exactly-once breaks on rebalance during transaction commit" --type agent-learned --agent reflection --feature payments-v2 --salience high`.
3. CLI reads body from positional arg (or stdin if positional is `-` / absent).
4. CLI computes `sha256(body)`, synthesizes `source_path = "agent:reflection:-:payments-v2:5171dd7a4cd2d4f3"`.
5. CLI probes for an existing `(agent, sha256)` row within the last 30 days — none found.
6. CLI begins an `IMMEDIATE` transaction, calls `upsert_insight_document` populating all six v4 metadata columns, runs `chunk()` on the body, writes chunks via `replace_chunks`, commits.
7. CLI best-effort populates `chunks_vec` via the e5 encoder (silent no-op if encoder unavailable).
8. CLI emits human-readable line: `remembered: doc_id=1 chunks=1 sha=5171dd7a4cd2 agent=reflection type=agent-learned salience=high` and exits 0.

**Postconditions:**
- `documents` table in `insights.db` has exactly one new row with `source_type='agent-learned'`, `agent_name='reflection'`, `salience='high'`, `feature_slug='payments-v2'`.
- `chunks` table has ≥1 row keyed to that document.
- `chunks_fts` index reflects the new chunks (FTS5 trigger fires).
- `index.db` (books corpus) is untouched.

## UC-AIB-2: Same Body Emitted Twice — Exact-Sha Dedup

**Actor:** SDLC Agent
**Preconditions:** A row with the same `(agent_name, sha256)` was ingested within the last 30 days.

**Main flow:**
1. Agent invokes `claudebase insight create "duplicate body" --type agent-learned --agent x --json`.
2. CLI computes `sha256("duplicate body")`, runs `find_recent_insight_by_sha(sha, "x", now-30d)` — returns `Some(existing_id)`.
3. CLI skips the write, emits JSON `{"status": "deduped", "doc_id": <N>, "source_path": "...", "sha256": "...", "agent": "x", "type": "agent-learned"}`, exits 0.

**Postconditions:**
- `documents` row count unchanged.
- The existing row's `ingested_at` is NOT updated (the dedup is read-only).

## UC-AIB-3: Cross-Agent Same Body — Both Rows Kept

**Actor:** Two distinct SDLC Agents (`planner` and `verifier`).
**Preconditions:** No prior row.

**Main flow:**
1. `planner` writes body B with `--agent planner`. Row 1 created.
2. `verifier` writes body B (identical text) with `--agent verifier`. No dedup match (the probe is `agent_name`-keyed). Row 2 created.

**Postconditions:**
- Two rows exist: `(agent_name=planner, sha256=H)` and `(agent_name=verifier, sha256=H)`. Cross-agent agreement on an observation is preserved as load-bearing signal.

## UC-AIB-4: Agent Retrieves Prior Insights at Task-Receipt

**Actor:** SDLC Agent
**Preconditions:** ≥1 prior insight exists in `insights.db`.

**Main flow:**
1. At task start, agent invokes `claudebase insight search "feature keywords from current task" --top-k 5 --json`.
2. CLI opens `insights.db`, encodes the query via e5 (or falls back to lexical with stderr warning if encoder missing).
3. CLI runs hybrid search (BM25 ⊕ dense via RRF k=60) over `chunks_fts` + `chunks_vec`.
4. CLI returns up to 5 `SearchHit` JSON objects with `chunk_id`, `score`, `snippet`, `source` (the synthetic `agent:...` path).
5. Agent surfaces the hits in its `## Facts → Verified facts` block citing `knowledge-base: agent:reflection:-:payments-v2:5171dd... — query: "..." — BM25: ...`.

**Postconditions:**
- Agent's output references prior-session insights with concrete citations.

## UC-AIB-5: Operator Lists Insights with Pagination

**Actor:** Pipeline Operator
**Preconditions:** ≥1 insight in the corpus.

**Main flow:**
1. Operator runs `claudebase insight list --offset 0 --json`.
2. CLI calls `count_insights(filters)` to get the total, `list_insights(filters, limit=10, offset=0)` to get the page.
3. CLI emits `{"total": N, "offset": 0, "page_size": 10, "returned": min(10, N), "rows": [{id, sha256_short, ingested_at, source_type, agent_name, salience, feature_slug, snippet}, ...]}`.
4. Operator runs `claudebase insight list --offset 1 --json` to see the next 10.

**Postconditions:**
- Pagination is newest-first by `ingested_at DESC`.
- Filters: `--type / --agent / --salience / --feature` reduce both `total` and `returned`.

## UC-AIB-6: Operator Picks a Random Insight

**Actor:** Pipeline Operator
**Preconditions:** ≥1 insight matching the filters.

**Main flow:**
1. Operator runs `claudebase insight random --salience high`.
2. CLI runs `SELECT id FROM documents WHERE source_type IS NOT NULL AND salience='high' ORDER BY RANDOM() LIMIT 1`.
3. CLI loads the full record (metadata + reconstructed body) and emits human-readable or JSON output.

**Postconditions:**
- A single insight printed.
- On empty corpus / no matches: exit 1 with `error: no insights match the filters`.

## UC-AIB-7: Operator Fetches One Insight by Identifier

**Actor:** Pipeline Operator
**Preconditions:** The target insight exists.

**Main flow (by id):**
1. Operator runs `claudebase insight get 42 --json`. CLI parses `42` as `i64`, calls `get_insight_by_id(42)`, emits the record or exit 1 if not found.

**Main flow (by sha prefix):**
1. Operator runs `claudebase insight get 5171dd7a`. CLI validates `≥4` hex chars, calls `get_insight_by_sha_prefix("5171dd7a")` → `sha256 LIKE '5171dd7a%'`, returns the most recent match.

**Postconditions:**
- Record emitted carries the reconstructed body with the 100-char chunker overlap collapsed.

## UC-AIB-8: Books-Corpus Untouched

**Actor:** SDLC Agent
**Preconditions:** Fresh project with no `index.db` yet.

**Main flow:**
1. Agent invokes `claudebase insight create "..." --type ... --agent ...`.
2. CLI opens `<project>/.claude/knowledge/insights.db`, creates it if absent.

**Postconditions:**
- `<project>/.claude/knowledge/insights.db` exists.
- `<project>/.claude/knowledge/index.db` does NOT exist — the insight write path never touches it.

## UC-AIB-EC-1: Empty Body Rejected

**Actor:** Either
**Preconditions:** Body is empty or whitespace-only.

**Main flow:**
1. Invocation: `claudebase insight create "   \n\t  " --type ... --agent ...`.
2. CLI trims, checks `body.is_empty()`, emits `error: insight body is empty`, exits 2.

## UC-AIB-EC-2: TTY Without Body Refused

**Actor:** Pipeline Operator (accidental interactive invocation)
**Preconditions:** stdin IS a TTY, no positional body.

**Main flow:**
1. Operator runs `claudebase insight create --type ... --agent ...` at a terminal prompt.
2. CLI detects TTY via `IsTerminal::is_terminal(&stdin())`, emits `error: body required (positional <body> or pipe input to stdin); refusing to block on TTY`, exits 2.

## UC-AIB-EC-3: Encoder Missing — Lexical-Only Works

**Actor:** SDLC Agent
**Preconditions:** e5-multilingual-small model not installed.

**Main flow:**
1. `insight create` proceeds normally — `try_populate_insight_chunks_vec` returns `Err(())` silently and the write commits without dense vectors.
2. `insight search "query" --mode hybrid` falls back to lexical with stderr warning `warning: encoder unavailable (...); falling back to lexical mode.`
3. `insight search "query" --mode lexical` works straight through.

## UC-AIB-EC-4: Empty Corpus — `random` Exits 1

**Actor:** Pipeline Operator
**Preconditions:** `insights.db` exists but has zero `documents` rows.

**Main flow:**
1. `claudebase insight random` → exit 1 with `error: no insights match the filters`.

## UC-AIB-EC-5: `get` with Too-Short Prefix — Exit 2

**Actor:** Pipeline Operator
**Preconditions:** Identifier is non-numeric and `<4` chars (e.g., `abc`).

**Main flow:**
1. `claudebase insight get abc` → exit 2 with `error: sha prefix must be ≥4 hex chars (got 'abc')`.
2. `claudebase insight get zzzzzz` (`≥4` chars but non-hex) → exit 2 with `error: identifier must be an integer id or a hex sha prefix (got 'zzzzzz')`.

## Facts

### Verified facts

- The eight DONE scenarios (UC-AIB-1..8) and EC-1..5 are exercised by 19 E2E tests in `tests/cli_insight_e2e_test.rs`, all passing at commit `e7bcc1c` — salience: high.
- The cross-agent non-dedup contract (UC-AIB-3) is verified by `create_cross_agent_same_body_is_not_deduped` — salience: high.
- Books-corpus isolation (UC-AIB-8) is verified by `create_does_not_create_index_db` — salience: high.

### External contracts

- **SQLite + FTS5 + sqlite-vec** — symbol: existing v2/v3 retrieval stack from §15 — verified: yes — salience: medium.
- **`std::io::IsTerminal::is_terminal`** — symbol: stable since Rust 1.70 — source: `src/main.rs::run_insight_create` — verified: yes — salience: low.

### Assumptions

- The three-axis cognitive taxonomy (self-learning / peer-bias / prediction-reality) is the right scope filter. Risk: agents may write factual findings classified as `agent-learned` and pollute the corpus. How to verify: monitor write rate per `source_type`; if `agent-learned` dominates and bodies look factual rather than cognitive, tighten agent prompts (Slice 8). Salience: medium.

### Open questions

(none)

## Decisions

### Inbound validation

(none) — use cases were derived directly from the design doc, PRD §16, and the implemented test suite. No upstream contradictions.

### Decisions made

- Scenarios are split into DONE (UC-AIB-1..8) and PLANNED (UC-AIB-9..12) so the test plan tracks coverage state without inventing tests for unimplemented paths. Salience: medium.

### Hacks acknowledged

(none)

### Symptom-only patches (with root-cause links)

(none)
