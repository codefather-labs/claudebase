# Feature plan — Agent insights base

> Status: **planning**
> Repo: `codefather-labs/claudebase`
> Branch: `feat/agent-insights-base`
> Author: Vlad + Mira (planning pass)

## Context

The SDLC pipeline (the agent zoo at `~/.claude/agents/` and the neuroscience-
inspired protocols in `~/.claude/rules/`) has a class of agents whose **output
IS first-class knowledge** — not code, not tests, but observations about the
project, the codebase, the work, and themselves:

- `reflection` — Default-Mode-Network analogue. Unfocused observations.
- `consolidator` — sleep-replay analogue. Drift detection across artifacts.
- `red-team` — adversarial review. Catches confirmation-bias blind spots.
- The cognitive-self-check `## Facts` / `## Decisions` blocks that *every*
  thinking agent emits per `~/.claude/rules/cognitive-self-check.md`.

Today those outputs go to:
1. stdout (transient — read once, gone)
2. file artifacts the orchestrator persists (`.claude/plan.md`,
   `.claude/scratchpad.md` — but only the per-feature parts)

There is no **cross-session, cross-project corpus** the *next* agent
can search to discover *what previous agents learned about this codebase*.
Every new agent invocation starts cold.

The user's intuition: the same retrieval engine that powers the books
corpus could power a parallel insights corpus — separate index, separate
write protocol, separate retrieval surface, same engine.

The goal of this feature: **let claudebase ingest, store, search, and
re-surface agent cognitive insights** so a session N+1 agent's first
move can be "what did sessions 1..N already figure out about this place?"

## Why now

- The neuroscience-inspired protocols (Reflection, Consolidator, Deliberate
  Mode, Predictive Coding, Salience tagging) are *producing* high-signal
  output per agent run, and that output is being lost or buried.
- The books corpus is locked-down user-curated content. Mixing
  agent-generated text into it would pollute the trusted source — a
  separate corpus is the right boundary.
- claudebase has a mature ingestion/retrieval engine (BM25 + dense
  + RRF, schema v3, page-tagged citations). Reusing the engine is cheap;
  inventing a separate one is wasteful.

## Out of scope (explicitly)

- A new retrieval algorithm — the existing hybrid stack is reused.
- A new vector model — the existing `e5-multilingual-small` 384-dim is reused.
- LLM evaluation of insights — quality control is the agents' job, not
  claudebase's. claudebase is a *store*, not a *judge*.
- A web UI — CLI only, consistent with the rest of claudebase.

## High-level architecture

Two parallel corpora share the engine:

```
<project>/.claude/knowledge/
├── index.db           ← books corpus (existing, user-curated, read-mostly)
└── insights.db        ← agent insights corpus (new, agent-written, append-mostly)
```

Both files use the **same schema v3+** (chunks_fts, chunks_vec, pages,
documents) with the same migration system. The only differences:

1. `insights.db` uses a distinct `documents.source_type` enum to capture
   the *kind* of insight (see "Insight types" below).
2. `insights.db` has a richer `documents` table with cross-session metadata
   (agent name, session id, project root, parent-artifact hash).
3. Writes to `insights.db` are agent-driven via a new `claudebase remember`
   subcommand. Writes to `index.db` remain user-driven via `claudebase
   ingest`.

The retrieval engine is unchanged. Both DBs are searchable independently,
and a `--corpus all` flag fuses results across them.

## Insight types (the `source_type` enum)

| `source_type` | Emitter | Trigger | Typical content |
|---|---|---|---|
| `reflection-observation` | `reflection` agent | `/reflect` invocation | Free-form non-obvious observation about the project state |
| `consolidator-drift` | `consolidator` agent | Auto-chained between waves in `/develop-feature`, or `/consolidate` | Two-point drift finding (file:line A diverged from file:line B) |
| `red-team-objection` | `red-team` agent | Auto-chained Step 5.25 in `/bootstrap-feature` | Adversarial finding (premise/scope/dependency/etc.) |
| `decision-record` | Any agent | A load-bearing decision where Q1-Q5 of cognitive-self-check had real ambiguity (not mechanical) | The decision + outcome summary |
| `assumption-log` | Any agent | A high-salience assumption with non-obvious risk | The assumption + risk + verification path |
| `hack-acknowledged` | Any agent | A shipped workaround with non-trivial removal cost | The hack + removal path |
| `project-observation` | Any agent | A non-obvious structural finding about the codebase or repo state | The observation |
| `agent-handoff-note` | Any agent | A contract drift or hand-off failure with another agent that next agents should know | The drift + which agents involved + reproducer |
| `operator-correction` | Any agent | The operator (Vlad) corrected the agent in a way that should propagate to future agents | The correction + context where it applied |
| `self-observation` | Any agent | The agent noticed something about its own reasoning — blind spot, prompting technique that worked unexpectedly, etc. | The self-observation |

The taxonomy mirrors the cognitive-self-check rule's output structure where
applicable, and adds three new types — `project-observation`,
`agent-handoff-note`, `operator-correction` — for findings that are real but
do not fit the structured decision/assumption/hack vocabulary.

Important: **the agent decides whether a finding is "real" before choosing a
source_type**. The taxonomy is a *classifier* for findings that already pass
the quality gate, not a list of compulsory categories to populate every run.

## CLI surface — new subcommands

### `claudebase remember`

Append one insight. Agents call this at the end of their work.

```
claudebase remember \
    --type reflection-observation \
    --agent reflection \
    --session $CLAUDE_SESSION_ID \
    --feature wave-1-promo \
    --salience high \
    --body @stdin
```

Arguments:
- `--type` (required) — one of the enum above.
- `--agent` (required) — emitting agent name.
- `--session` (optional) — Claude Code session UUID for trace linking.
- `--feature` (optional) — feature slug (matches `<project>/.claude/plan.md` feature).
- `--salience` (one of `high|medium|low`) — surfacing weight.
- `--source-artifact` (optional) — path to the file the insight was extracted from.
- `--body` — the actual insight text (markdown). `@stdin` reads from stdin.

Effect: chunks the body, indexes into `insights.db` with the metadata,
deduplicates against the last 30 days of similar content per `--agent` to
avoid noise.

Exit code 0 on success, 1 on validation failure, 2 on schema mismatch.

### `claudebase recall`

The retrieval surface, conceptually identical to `claudebase search` but
typed for the insights workflow.

```
claudebase recall "<query>" \
    [--type <enum>] \
    [--agent <agent-name>] \
    [--feature <slug>] \
    [--salience high] \
    [--since 30d] \
    [--top-k 10] \
    [--mode hybrid] \
    [--json]
```

Behavior: BM25⊕dense⊕RRF over insights.db chunks_fts + chunks_vec, with
metadata filters (type/agent/feature/salience/age) applied as WHERE
clauses before the rank.

Returns JSON hits identical to `claudebase search` shape plus an
`insight_meta` block with the source-type/agent/session/feature fields.

### `claudebase insights status` / `list` / `delete`

Parallel to existing `status`/`list`/`delete` for the books corpus.
`status` reports doc/chunk counts and storage size for `insights.db`.

### `claudebase search` (existing) — extend with `--corpus` flag

```
claudebase search "<query>" --corpus books     # default — only books (back-compat)
claudebase search "<query>" --corpus insights  # only insights
claudebase search "<query>" --corpus all       # cross-corpus RRF fusion
```

When omitted, defaults to `books` (back-compat with current behavior).

## Schema additions

```sql
-- Same v3 base (chunks, chunks_fts, chunks_vec, pages) +

-- documents table extended with insight metadata
create table if not exists documents (
    id            integer primary key,
    source_path   text not null,
    sha256        text not null,
    mtime         integer not null,
    ingested_at   text not null,
    -- new for insights.db only (nullable in books.db):
    source_type   text,      -- enum above, NULL for book docs
    agent_name    text,
    session_id    text,
    feature_slug  text,
    salience      text,      -- high|medium|low
    parent_artifact text     -- path to .claude/plan.md / scratchpad / etc.
);

create index if not exists idx_documents_source_type on documents(source_type);
create index if not exists idx_documents_agent on documents(agent_name);
create index if not exists idx_documents_feature on documents(feature_slug);
create index if not exists idx_documents_salience on documents(salience);
create index if not exists idx_documents_ingested on documents(ingested_at);
```

Migrations go in `src/migrations.rs` as `apply_v4_insights` (or
`apply_v3.5` since v3 is page-tracking).

## Dedup strategy

Reflection observations and red-team objections can repeat. We dedup to
keep the corpus signal-dense:

1. **Exact match** — sha256(body) hits an existing chunk from the same
   agent in the last 30 days → skip the write, log "deduplicated".
2. **Semantic near-dup** — query the dense index with the new body; if
   top hit has cosine > 0.92 *and* same agent *and* within 30 days,
   skip the write.
3. **Drift findings** — `consolidator-drift` insights are keyed by the
   pair of `file:line` references they cite. Same pair from same
   consolidator run is a dup.

Dedup happens inside `claudebase remember` before chunking.

## Write protocol — every agent contributes, but only when there is a real insight

**Scope (UPDATED 2026-05-16):** Every agent in the SDLC pipeline (thinking AND
executor agents both — including `test-writer`, `build-runner`, `e2e-runner`,
`doc-updater`, `changelog-writer` that were previously exempt from the
cognitive-self-check rule) MAY call `claudebase remember`. The previous
"12 in-scope thinking agents" restriction is lifted.

**Quality bar (NEW, non-negotiable):** Agents call `claudebase remember`
**only when a real intellectual finding exists**. Writing for the sake of
writing — repeating well-known facts, paraphrasing input, narrating mechanical
steps — is forbidden. The dedup layer (Slice 5) will catch some of this; the
agent prompt language must explicitly gate the call.

A "real insight" is any of:

- **About the project** — a non-obvious structural observation about the
  codebase, an emergent constraint, a load-bearing decision rationale, a
  pattern the agent noticed across files, a counter-intuitive interaction.
- **About working with other agents** — a contract that previously held but
  drifted, a hand-off failure mode the agent encountered, a missing affordance
  in an upstream agent's output, a pattern in how Vlad (the operator) corrects
  the pipeline.
- **About the operator** — preferences observed, recurring corrections, signal
  about what Vlad considers important vs noise, working-style cues that should
  inform how future agents respond. *Memory of human-in-the-loop interaction.*
- **About self** — surprising things the agent learned about its own
  reasoning, blind spots it discovered, prompting techniques that worked
  unexpectedly well or poorly.

What is **NOT** an insight (do not write):

- Mechanical execution narration ("I read file X then edited Y").
- Restating PRD requirements or plan slices.
- Generic best-practice claims (e.g. "tests are good").
- Anything already searchable in the books corpus.
- Hedge-language summaries ("this might be useful later").

Per-agent triggers (suggested, not exhaustive):

| Agent | When the gate opens | Sample body |
|---|---|---|
| `reflection` | After each `/reflect` run, per surfaced observation | Verbatim observation report |
| `consolidator` | Per drift finding emitted | Two-point citation + drift type |
| `red-team` | Per objection emitted | The severity-tagged objection block |
| `architect` | When the design has a non-obvious trade-off the next architect should know | Trade-off + rationale |
| `planner` | When a slice ordering decision is load-bearing for future planners | The ordering rationale |
| `qa-engineer` | When a test surfaced an unexpected production behavior | The behavior + reproducible trigger |
| `test-writer` | When a TDD slice revealed a missing requirement | The missing requirement |
| `code-reviewer` | When a review uncovered a class-of-bug worth surfacing | The class + canonical example |
| `verifier` | When goal-backward verification caught wiring drift | The wired-vs-unwired pair |
| `build-runner` | When a build failure pattern matters for future runs | The pattern + reproducer |
| `refactor-cleaner` | When dead code revealed an architectural smell | The smell |
| **Every agent** | When the operator (Vlad) corrects the agent in a way that should propagate | The correction + context |

The call is fire-and-forget per the existing tracing pattern — `claudebase
remember` writes are async-safe (SQLite WAL mode + single-writer).

If `claudebase remember` exits non-zero (DB locked, disk full, schema
mismatch), the agent's primary output is unaffected; the miss is logged at
`warn` level. If the agent has nothing real to write, it does NOT call the
tool — silence is the correct default.

## Privacy + security

The insights corpus contains:
- Project paths
- File:line references
- Decision reasoning that may contain proprietary context
- Possibly secrets if an agent dumps something it shouldn't

Therefore:
- `insights.db` is **per-project**, lives at `<project>/.claude/knowledge/`,
  never crosses project boundaries.
- `claudebase recall` requires the same `--project-root` containment
  as the rest of claudebase — no cross-project access.
- Default git-ignore: `.claude/knowledge/insights.db` should be added
  to a templates `.gitignore` so the corpus is not accidentally
  committed.

## Lifecycle / TTL

Salience tags from cognitive-self-check map to retention:

- `high` salience insights — retained forever
- `medium` — retained 1 year
- `low` — retained 90 days, then auto-purged via `claudebase insights gc`

A weekly cron-style `claudebase insights gc` reads salience + age,
purges low-salience old entries, runs FTS5/vec vacuum.

## Integration with existing SDLC

### When an agent runs
- The agent's prompt template gets one new section:
  `## Insight surfacing (MANDATORY when applicable)` — instructs the
  agent to call `claudebase remember` for each high-signal output.
- The orchestrator (`/develop-feature`, `/bootstrap-feature`,
  `/qa-cycle`, `/reflect`, `/consolidate`) passes the session ID and
  feature slug as env vars (`CLAUDE_SESSION_ID`, `CLAUDE_FEATURE_SLUG`).

### When an agent starts (the retrieval side)
- Each in-scope thinking agent's prompt template gets a *paired* section:
  `## Insight retrieval (MANDATORY at task receipt)` — instructs the agent
  to run `claudebase recall "<feature-keywords>" --feature $FEATURE
  --top-k 5` and surface the top hits in its `## Facts → Verified facts`
  block as cross-session memory.
- This is the analogue of the books-corpus query the cognitive-self-check
  rule already mandates. Same mechanic, different corpus.

### Cross-corpus example

```bash
# Agent at task receipt
claudebase recall "idempotent ledger reconciliation" \
    --feature payments-v2 --salience high --top-k 3 --json

# returns past decisions + drift findings from earlier sessions on the
# same feature, which the agent now incorporates into its plan
```

## Iteration plan

Eight-slice rollout:

1. **Schema v4 migration** — add insights-specific columns + indexes
   to `documents` table. Make them nullable so books-corpus rows unaffected.
2. **`claudebase remember` subcommand** — write-side CLI + dedup.
3. **`claudebase recall` subcommand** — read-side CLI with metadata filters.
4. **`--corpus` flag on existing `search`** — books|insights|all routing.
5. **`claudebase insights status/list/delete/gc`** — admin surface.
6. **Agent-prompt integration in SDLC repo** — add the
   `## Insight surfacing` + `## Insight retrieval` sections to the
   12 in-scope thinking agents.
7. **Rule update in SDLC repo** — extend
   `~/.claude/rules/knowledge-base-tool.md` to document the insights
   workflow + `~/.claude/rules/cognitive-self-check.md` to formally
   tie salience tags to retention.
8. **Test pass** — unit tests for the new subcommands, integration
   test that ends with "agent A wrote an insight; agent B in next
   session retrieved it".

Estimated effort: ~3-5 days of focused work on claudebase + 1 day on
the SDLC agent-prompt + rule updates. Slices 1-5 land in claudebase
repo, slices 6-7 in the SDLC repo, slice 8 spans both.

## Open questions

1. **Should `claudebase remember` accept binary attachments?**
   E.g. a flame-graph from `verifier`. Initial answer: no — text only.
   Binary attachments belong in `<project>/.claude/scratchpad/` or
   similar, with paths cited from the insight body.
2. **Should the corpus be Git-versioned per-project?**
   Initial answer: no — git-ignored. The corpus is a write-mostly
   local cache; reproducing it across machines isn't a goal.
3. **What's the agent's contract if `claudebase` isn't installed?**
   Initial answer: silent no-op (same as the books-corpus rule today —
   agent logs `claudebase: tool not installed; skipping` once and
   proceeds without recall/remember).
4. **Cross-project insight sharing.** Not in scope for v1, but the
   schema supports it (project root is in metadata). Future flag
   `claudebase recall --include-other-projects` could fuse multiple
   `insights.db` files for cross-domain agent memory. Defer.

## Acceptance criteria

The feature is complete when:

1. `claudebase remember --type reflection-observation --agent reflection
   --body "test"` succeeds and the chunk is searchable via
   `claudebase recall "test"`.
2. `claudebase recall "idempotency" --type decision-record --salience high`
   returns only high-salience decision records mentioning idempotency.
3. `claudebase search "ledger" --corpus all` fuses books-corpus and
   insights-corpus hits via RRF and labels the source-corpus in JSON.
4. The 12 in-scope thinking agents successfully write to insights.db
   when their tasks complete (manual smoke test via running
   `/develop-feature` on a toy project and observing
   `claudebase insights list` grow).
5. The 12 in-scope thinking agents successfully retrieve cross-session
   insights at task-receipt and cite them in `## Facts → Verified facts`
   blocks (manual smoke test).
6. `claudebase insights gc` correctly purges low-salience entries past
   their TTL.
7. Existing books-corpus behavior is byte-identical to pre-feature
   (back-compat regression test on a 1k-chunk fixture).

## Risks

- **Noise floor too high.** If every decision becomes an insight, the
  corpus drowns the signal. Mitigation: salience filter on retrieval
  defaults to `high|medium`, and the dedup strategy is aggressive.
- **Schema migration on existing books indexes.** Adding nullable
  columns to `documents` is non-destructive but cargo-cult-applied
  migrations have failed before. Mitigation: explicit migration test
  on a fixture v3 index from a real codefather.dev pre-feature snapshot.
- **Concurrent writes from multiple agents in one session.** SQLite
  WAL mode handles single-writer multi-reader well, but if a
  `/develop-feature` wave spawns 4 implementers and 1 verifier
  simultaneously, the writes serialize at the SQLite level. Mitigation:
  document the contention; in practice 5 writes/second is well below
  SQLite's WAL ceiling.
- **Agent contract creep.** Adding write-recall responsibilities to
  every thinking agent enlarges their prompt and inference cost.
  Mitigation: the `## Insight surfacing` / `## Insight retrieval`
  sections are short (<200 tokens each) and the cognitive-self-check
  rule already mandates the underlying `## Facts` / `## Decisions`
  blocks — `remember` just *persists* what's already being emitted.

## Verification

End-to-end check after slices 1-8 land:

```bash
# 1. Run /develop-feature on a toy feature
cd /tmp/test-project
# (assume claudebase + SDLC agents installed)
claude /develop-feature "add /healthz endpoint"

# 2. Verify insights were written
claudebase insights status --json
# expected: doc_count > 0, chunk_count > 0

# 3. Verify cross-session recall — start a new Claude session in same project
claude /develop-feature "add /version endpoint"
# observe: the new session's planner agent cites prior insights from the
# earlier session in its ## Facts → Verified facts block

# 4. Books-corpus regression check
claudebase search "RAG production" --corpus books --top-k 3 --json
# expected: identical JSON shape and ranking to pre-feature behavior
```

## Why this matters

The current SDLC pipeline treats each Claude session as cognitively
isolated. The neuroscience-inspired protocols make individual sessions
smarter — Reflection catches blind spots, Consolidator detects drift,
Red-Team adversarially reviews — but every session re-discovers the same
things because there is no *cross-session memory*.

This feature adds that memory. The next session's first agent is no
longer cold-starting on a domain it has seen before; it asks the
insights corpus what its predecessors learned and incorporates the
answers into its own facts block.

The cost is small (one new SQLite file, two new CLI subcommands, a
nullable-columns migration). The compounding payoff is *every future
session is built on top of every prior session's load-bearing work*.

That is the closest thing the SDLC pipeline has to a hippocampus, and
the books corpus alone cannot serve as one.
