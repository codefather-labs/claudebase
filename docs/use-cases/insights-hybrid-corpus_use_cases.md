# Use Cases: Insights Hybrid Corpus

> Based on [PRD §18](../PRD.md) — "Insights Hybrid Corpus — Global General DB, Project Registry, Mandatory Tags, and Read-on-New-Context Hook"
> Plan source: `.claude/plan.md` (199 lines, read this session)

---

## Actors

| Actor | Description |
|-------|-------------|
| **SDLC Writer-Agent** | Any of the 13 in-scope thinking agents invoking `claudebase insight create`. Calls the binary non-interactively (piped stdin or positional body). Supplies `--category` and `--tags` (both now required). |
| **SDLC Reader-Agent (new context)** | Any SDLC agent entering a fresh context window (startup, resume, or compact event). Calls `claudebase insight tags` and/or `claudebase insight search --tag <t>` to pull relevant prior-session insights before producing output. |
| **Pipeline Operator** | Human running admin commands (`insight tags`, `insight list`, `insight get`, `insight gc`, `insight delete`). May inspect `projects.json` and the tag registry directly. |
| **`claudebase run` launcher process** | The `claudebase run` invocation that starts a Claude Code session. Its `run_claude_with_preset` function upserts the cwd project into `~/.claude/knowledge/projects.json` before handing control to `exec()`. |
| **SessionStart hook** | The `claudebase-read-insights-reminder.sh` / `.ps1` script wired into `~/.claude/settings.json`. Fires on `startup`, `resume`, and `compact` events; emits `additionalContext` reminding agents to pull insights by tag. |

---

## Use Case Coverage

| Use Case | PRD FR / AC Covered |
|----------|---------------------|
| UC-IHC-1: Create a PROJECT insight — mandatory routing to local db | FR-IHC-3.1, 3.2, 3.3, 3.4, 3.5, 3.6; AC-IHC-7 |
| UC-IHC-2: Create a GENERAL insight — mandatory routing to global db | FR-IHC-3.1, 3.2, 3.4, 3.5, 3.6; AC-IHC-6 |
| UC-IHC-3: Create without `--tags` → exit 2 | FR-IHC-3.2; AC-IHC-4 |
| UC-IHC-4: Create without `--category` → clap exit 2 | FR-IHC-3.1; AC-IHC-5 |
| UC-IHC-5: `insight tags` lists merged vocabulary (default) | FR-IHC-4.1, 4.2, 4.5, 4.6; AC-IHC-8 |
| UC-IHC-6: `insight tags` with `--category` / `--project` filters | FR-IHC-4.3, 4.4 |
| UC-IHC-7: Reader-agent queries tags then searches by `--tag` (merged default) | FR-IHC-5.1, 5.2, 5.3; AC-IHC-9, AC-IHC-10 |
| UC-IHC-8: Read with `--general-only` narrowing | FR-IHC-5.4; AC-IHC-11 |
| UC-IHC-9: Read with `--project-only` narrowing | FR-IHC-5.5 |
| UC-IHC-10: `claudebase run` startup upserts project into registry | FR-IHC-6.1, 6.2, 6.3, 6.5; AC-IHC-12 |
| UC-IHC-11: Registry upsert is idempotent on repeated `run` invocations | FR-IHC-6.2; AC-IHC-13 |
| UC-IHC-12: Concurrent `claudebase run` invocations race on `projects.json` | FR-IHC-6.3; NFR-IHC-3 |
| UC-IHC-13: Schema v5 migration of an existing v4 `insights.db` | FR-IHC-1.4, 1.5, 1.6, 1.7; AC-IHC-2, AC-IHC-3 |
| UC-IHC-14: Global db absent — first `--category general` write creates it | FR-IHC-2.1, 2.2; AC-IHC-6 (first-run variant) |
| UC-IHC-15: SessionStart hook fires on new context window | FR-IHC-7.1, 7.2, 7.3; AC-IHC-14 |
| UC-IHC-16: SessionStart hook install is idempotent | FR-IHC-7.4, 7.5; AC-IHC-15 |
| UC-IHC-17: Cross-project read attempt walled off (default posture) | FR-IHC-5.2; AC-IHC-9 (exclusion side) |
| UC-IHC-18: Cross-project read via explicit `--project <slug>` (registry lookup) | FR-IHC-5.2, 6.4 |
| UC-IHC-19: `insight gc` runs against both dbs (default) and global-only (`--category general`) | FR-IHC-5.6 |
| UC-IHC-20: Dedup continues to fire per-db after mandatory-flag enforcement | FR-IHC-3.7 |
| UC-IHC-21: `insight delete` with `--category general` resolves against global db | FR-IHC-5.7 |

---

## UC-IHC-1: Create a PROJECT Insight (Happy Path)

**Actor:** SDLC Writer-Agent
**Preconditions:**
- `<cwd-project>/.claude/knowledge/insights.db` exists (or the binary will create it).
- The binary is at schema v5 (or will auto-migrate from v4 on open).
- The agent has a cognitive observation that is project-specific.

**Trigger:** The agent produces a `plan-reality-gap` / `agent-learned` / `peer-bias-observed` insight at task-end and invokes `claudebase insight create`.

### Primary Flow (Happy Path)

1. Agent constructs the body text (e.g., `"Tokio mutex held across await point — caused deadlock in slice 3"`).
2. Agent invokes:
   ```
   claudebase insight create "Tokio mutex held across await point — caused deadlock in slice 3" \
       --type agent-learned \
       --agent planner \
       --feature insights-hybrid-corpus \
       --salience high \
       --category project \
       --tags tokio mutex
   ```
3. CLI validates: `--category project` present, `--tags` non-empty (`tokio`, `mutex` parsed from the two token arguments).
4. CLI routes: `--category project` → calls `resolve_project_root(cwd)` to get the local `insights.db` path.
5. CLI opens the local `insights.db` (creates if absent; triggers v4→v5 migration if needed).
6. CLI computes `sha256(body)` and synthesizes `source_path = "agent:planner:...:<sha-prefix>"`.
7. CLI probes for exact-sha dedup (same agent, same sha, within 30 days) — none found.
8. CLI begins `IMMEDIATE` transaction: inserts into `documents` with `category='project'`, `project_slug=<cwd-basename>`, all existing v4 metadata columns (`source_type`, `agent_name`, `salience`, `feature_slug`, etc.). Writes chunks via `replace_chunks`. Commits.
9. CLI inserts one row per normalized tag into `insight_tags`: `lowercased('tokio') → (doc_id, 'tokio')`, `lowercased('mutex') → (doc_id, 'mutex')`. Duplicate tags silently dropped.
10. CLI best-effort populates `chunks_vec` via e5 encoder (silent no-op if encoder unavailable).
11. CLI emits: `remembered: doc_id=N chunks=1 sha=<prefix> agent=planner type=agent-learned salience=high` and exits 0.

**Postconditions:**
- The local `<cwd>/.claude/knowledge/insights.db` has a new `documents` row with `category='project'`, `project_slug=<project-basename>`.
- `insight_tags` has rows `(doc_id, 'tokio')` and `(doc_id, 'mutex')`.
- `~/.claude/knowledge/insights.db` (global db) is NOT created or written.
- `chunks` and `chunks_fts` reflect the new insight body.

### Alternative Flows

- **UC-IHC-1-A1: `--project <slug>` flag supplied explicitly** — The agent supplies `--project myproject` along with `--category project`. The CLI uses `myproject` as `project_slug` rather than deriving it from the cwd basename. All other steps identical. Useful when the agent's cwd differs from the logical project root.

- **UC-IHC-1-A2: Tags supplied with leading `#` characters** — The agent supplies `--tags "#tokio" "#mutex"`. CLI strips leading `#` before normalization. `insight_tags` receives `('tokio')` and `('mutex')`, not `('#tokio')`. Postconditions identical to the primary flow.

- **UC-IHC-1-A3: Duplicate tags in `--tags` list** — The agent supplies `--tags tokio tokio mutex`. CLI attempts two inserts for `tokio`; the `UNIQUE(doc_id, tag)` constraint silently drops the second. `insight_tags` has two rows: `tokio` and `mutex`. No error emitted. Exits 0.

- **UC-IHC-1-A4: `insights.db` absent — created on first write** — The local db file does not yet exist (fresh project). CLI's `open_or_init_v2` creates the file, stamps schema version 5, applies V5 delta, then proceeds with the insert. Postconditions identical; no prior migration step runs.

### Error Flows

- **UC-IHC-1-E1: `resolve_project_root` fails (cwd not under a project root)** — CLI cannot determine the project root. Exits 1 with `error: could not determine project root from cwd`. The global db is NOT written. No `insight_tags` rows created.

- **UC-IHC-1-E2: SQLite write failure (disk full / permissions)** — CLI opens db, transaction begins, then fails on write. Transaction rolls back atomically. CLI emits `error: failed to write insight: <sqlite error>`, exits 1. `insight_tags` rows are absent (they were inside the same transaction).

### Edge Cases

- **UC-IHC-1-EC1: Tag string that lowercases to empty string** — Input `--tags "#"`. After stripping `#` and lowercasing, the tag is an empty string `""`. Behavior: the CLI MUST reject an empty tag string with `error: insight create requires at least one --tag` and exit 2. An insight with zero persisted tags is not permitted.
- **UC-IHC-1-EC2: Very long tag string (>255 chars)** — The `insight_tags` table schema does not impose a length limit; SQLite TEXT is unlimited. The CLI should store it. This is a degenerate case; no truncation or error is specified — assumption: stored as-is.
- **UC-IHC-1-EC3: Schema v4 db opened for the first time by v5 binary** — Auto-migration fires (UC-IHC-13). After migration, the project insight write proceeds normally in the same open connection.

### Data Requirements

- **Input:** body text (non-empty), `--type`, `--agent`, `--feature`, `--salience`, `--category project`, `--tags <≥1 tag>`.
- **Output:** Human-readable confirmation line + `doc_id`, `sha`, `agent`, `type`, `salience` on exit 0.
- **Side Effects:** `documents` row inserted; `insight_tags` rows inserted; `chunks` + `chunks_fts` updated; `chunks_vec` best-effort updated; global db untouched.

---

## UC-IHC-2: Create a GENERAL Insight (Happy Path)

**Actor:** SDLC Writer-Agent
**Preconditions:**
- The agent has a cross-project, tool-level observation (e.g., nginx reload signals, a Tokio pattern that applies everywhere).
- `~/.claude/knowledge/` directory exists OR the binary will create it.

**Trigger:** Agent at task-end invokes `insight create --category general`.

### Primary Flow (Happy Path)

1. Agent invokes:
   ```
   claudebase insight create "nginx reload sends SIGHUP, not SIGTERM — use reload not restart to avoid downtime" \
       --type agent-learned \
       --agent ba-analyst \
       --feature insights-hybrid-corpus \
       --salience medium \
       --category general \
       --tags nginx infrastructure
   ```
2. CLI validates: `--category general` present, `--tags` non-empty.
3. CLI routes: `--category general` → calls `resolve_global_insights_db()`.
4. `resolve_global_insights_db()` returns `~/.claude/knowledge/insights.db`. Creates `~/.claude/knowledge/` if absent. This function deliberately bypasses `resolve_project_root`'s cwd-containment gate (safe: fixed HOME-rooted path, no user-controlled component).
5. CLI opens `~/.claude/knowledge/insights.db` (creates if absent; applies v5 schema on first open).
6. CLI performs the same sha-dedup check, insert, `insight_tags` population, and chunk indexing as UC-IHC-1 steps 6-10.
7. CLI ignores `--project` flag silently (not applicable for `general` category).
8. CLI emits confirmation and exits 0.

**Postconditions:**
- `~/.claude/knowledge/insights.db` has the new row with `category='general'`, `project_slug=NULL`.
- `insight_tags` in the global db has `('nginx')` and `('infrastructure')` rows for the new doc.
- The cwd-local `<project>/.claude/knowledge/insights.db` is NOT touched.

### Alternative Flows

- **UC-IHC-2-A1: `--project <slug>` supplied with `--category general`** — The flag is silently ignored. `project_slug` remains NULL in the `documents` row. CLI emits no warning. The insight is stored in the global db only.

### Error Flows

- **UC-IHC-2-E1: HOME environment variable unset (Unix)** — `resolve_global_insights_db()` calls `std::env::var("HOME")` and gets `Err`. CLI emits `error: $HOME not set; cannot resolve global insights db path`, exits 1.
- **UC-IHC-2-E2: `~/.claude/knowledge/` cannot be created (permissions)** — `resolve_global_insights_db()` attempts `create_dir_all`. Fails with OS permission error. CLI emits `error: could not create directory ~/.claude/knowledge/: <os error>`, exits 1.

### Edge Cases

- **UC-IHC-2-EC1: Global db absent and cwd-local db also absent** — Both are created independently on their first respective writes. First `--category general` write creates the global db. First `--category project` write creates the local db. They are entirely independent files.
- **UC-IHC-2-EC2: General insight written from a directory that is NOT a git project** — `resolve_global_insights_db()` does not call `resolve_project_root` at all; cwd is irrelevant. The write succeeds regardless of cwd.
- **UC-IHC-2-EC3: Same body written as `general` by two different agents** — Cross-agent non-dedup rule applies per-db (matching UC-AIB-3). Both rows are kept in the global db. The dedup probe is keyed on `(agent_name, sha256)`.

### Data Requirements

- **Input:** body text, `--type`, `--agent`, `--feature`, `--salience`, `--category general`, `--tags <≥1>`.
- **Output:** Confirmation line on exit 0.
- **Side Effects:** `~/.claude/knowledge/insights.db` row + tags inserted; local project db untouched.

---

## UC-IHC-3: Create Without `--tags` → Exit 2

**Actor:** SDLC Writer-Agent or Pipeline Operator (accidental / old-callsite invocation)
**Preconditions:** The caller was compiled against the old v0.6.0 CLI contract (or omitted `--tags` by mistake).

**Trigger:** `claudebase insight create "body" --category project --type agent-learned --agent x` with no `--tags` argument.

### Primary Flow

1. CLI parses arguments. `--category project` found. `--tags` argument: absent.
2. CLI detects the missing required argument AFTER clap parsing (clap would not reject `--tags` as required-positional at this stage because the requirement is business-logic, not clap syntax — the tag list is an empty `Vec<String>` from a repeatable `#[arg(long)]` with no clap-level required constraint).
3. CLI checks `tags.is_empty()` in `run_insight_create`, emits `error: insight create requires at least one --tag` to stderr, exits 2.
4. No `documents` row is inserted. No `insight_tags` rows are inserted. Neither db is opened for writing.

**Postconditions:**
- Exit code is 2 (usage error).
- stderr contains the literal message `error: insight create requires at least one --tag`.
- Both the local and global `insights.db` are unchanged.

### Alternative Flows

- **UC-IHC-3-A1: `--tags` supplied but all values reduce to empty after stripping** — See UC-IHC-1-EC1. Treated as zero tags; same exit-2 path.

### Error Flows

(none beyond the primary exit-2 path)

### Edge Cases

- **UC-IHC-3-EC1: Pipe body via stdin but still no `--tags`** — Even with a valid piped body, the `--tags` absence check fires before the write. Exit 2, no write.
- **UC-IHC-3-EC2: `--json` flag present** — Output mode is JSON. The error is still emitted to stderr as the literal string; stdout may be empty or a JSON error object depending on implementation. Exit code remains 2.

### Data Requirements

- **Input:** any valid combination of args except `--tags`.
- **Output:** exit 2 + stderr literal `error: insight create requires at least one --tag`.
- **Side Effects:** none (no db write).

---

## UC-IHC-4: Create Without `--category` → Clap Exit 2

**Actor:** SDLC Writer-Agent or Pipeline Operator
**Preconditions:** Caller omits `--category` (old v0.6.0 callsite or mistake).

**Trigger:** `claudebase insight create "body" --type agent-learned --agent x --tags foo` with no `--category`.

### Primary Flow

1. CLI invokes clap argument parsing. `--category` is a `required` `value_enum` argument in `InsightCreateArgs`.
2. Clap detects the missing required argument before any application logic runs.
3. Clap emits its standard usage-error output (the exact format is clap-generated, not a literal string controlled by application code) to stderr, including the argument name and available values (`general`, `project`).
4. Process exits 2.

**Postconditions:**
- Exit code is 2.
- stderr contains a clap-generated error referencing `--category`.
- No db file is opened; no write occurs.

### Alternative Flows

- **UC-IHC-4-A1: `--category` supplied with an invalid value (e.g., `--category team`)** — Clap rejects it as a `value_enum` parse failure. Same exit-2 behavior with an error naming the invalid value.

### Edge Cases

- **UC-IHC-4-EC1: `--category` supplied as empty string `""`** — Clap `value_enum` parsing rejects an empty string (no matching variant). Exit 2.

### Data Requirements

- **Input:** any `insight create` invocation omitting `--category`.
- **Output:** clap-generated usage error on stderr; exit 2.
- **Side Effects:** none.

---

## UC-IHC-5: `insight tags` Lists Merged Tag Vocabulary (Default)

**Actor:** Pipeline Operator or SDLC Reader-Agent
**Preconditions:**
- At least one insight exists in the local `insights.db` with tags.
- At least one insight exists in the global `~/.claude/knowledge/insights.db` with tags.

**Trigger:** `claudebase insight tags --json`

### Primary Flow (Happy Path)

1. Actor invokes `claudebase insight tags --json` from within a project directory.
2. CLI opens both the cwd-local `insights.db` and the global `insights.db`.
3. CLI executes `SELECT tag, COUNT(*) AS count FROM insight_tags GROUP BY tag ORDER BY count DESC` against each db independently.
4. CLI merges the two result sets: for tags present in both dbs, the counts are summed. For tags in only one db, the count from that db is used.
5. CLI emits a JSON array sorted by descending count:
   ```json
   [
     {"tag": "nginx", "count": 3},
     {"tag": "tokio", "count": 2},
     {"tag": "mutex", "count": 1}
   ]
   ```
6. Exits 0.

**Postconditions:**
- Output reflects merged vocabulary from both dbs.
- Neither db is modified.

### Alternative Flows

- **UC-IHC-5-A1: No filters, global db absent** — If `~/.claude/knowledge/insights.db` does not exist, CLI queries only the local db and returns its tags. No error for the absent global db. Exits 0 with local-only results.
- **UC-IHC-5-A2: Local db absent but global db present** — CLI returns global db tags only. No error.
- **UC-IHC-5-A3: Both dbs empty (`insight_tags` tables have zero rows)** — CLI returns an empty JSON array `[]`. Exits 0.
- **UC-IHC-5-A4: Human-readable output (no `--json`)** — CLI emits a formatted table: `<tag>    <count>` lines, sorted by count descending. Exits 0.

### Error Flows

- **UC-IHC-5-E1: Local db exists but is corrupt** — SQLite open fails with schema-validation error. CLI emits `error: index database invalid; re-ingest required`, exits 1. Global db results are NOT returned in partial form — atomicity of the merge operation.

### Edge Cases

- **UC-IHC-5-EC1: Same tag in both dbs with equal counts** — Tie-break ordering is unspecified (SQLite's `ORDER BY count DESC` picks either). This is a cosmetic ordering non-issue; test must allow either ordering for equal-count tags.
- **UC-IHC-5-EC2: Tag exists only in general db, not in local** — Tag appears in the merged output with count from the global db only.

### Data Requirements

- **Input:** optional `--json`.
- **Output:** `[{"tag": string, "count": integer}, ...]` sorted by count descending.
- **Side Effects:** none (read-only).

---

## UC-IHC-6: `insight tags` with `--category` / `--project` Filters

**Actor:** Pipeline Operator
**Preconditions:** Insights with various categories and project slugs exist across both dbs.

**Trigger:** `claudebase insight tags --category general --json`

### Primary Flow

1. Operator invokes `claudebase insight tags --category general --json`.
2. CLI opens only the global db (because `--category general` implies global-only posture).
3. CLI executes: `SELECT tag, COUNT(*) AS count FROM insight_tags JOIN documents ON insight_tags.doc_id = documents.id WHERE documents.category = 'general' GROUP BY tag ORDER BY count DESC`.
4. CLI emits JSON with only tags from insights where `category='general'`.
5. Exits 0.

**Postconditions:**
- Output contains no tags from project-category insights.

### Alternative Flows

- **UC-IHC-6-A1: `--project <slug>` filter** — Operator invokes `claudebase insight tags --project claudebase --json`. CLI opens the local db (and/or global db if the named project is not the cwd), filters: `WHERE documents.project_slug = 'claudebase'`. Returns only tags from insights associated with that project slug.
- **UC-IHC-6-A2: Combined `--category project --project <slug>`** — CLI applies both filters. Returns tags for insights with `category='project'` AND `project_slug='<slug>'`. Useful to distinguish a project's own tags from general tags.
- **UC-IHC-6-A3: `--category project` with no `--project` filter** — Returns tags from all project-category insights visible in the local db (any project slug present).

### Error Flows

- **UC-IHC-6-E1: `--category` value not in the enum** — Clap rejects at parse time. Exit 2 with usage error.

### Edge Cases

- **UC-IHC-6-EC1: `--project <slug>` names a project not in the registry** — CLI cannot resolve the db path. Emits `error: project '<slug>' not found in registry`, exits 1.
- **UC-IHC-6-EC2: `--project <slug>` names the cwd project** — Identical to omitting `--project`; uses the already-known local db path.

### Data Requirements

- **Input:** `--category <c>` and/or `--project <slug>`, optional `--json`.
- **Output:** filtered `[{tag, count}]` array.
- **Side Effects:** none (read-only).

---

## UC-IHC-7: Reader-Agent Queries Tags Then Searches by `--tag` (Merged Default)

**Actor:** SDLC Reader-Agent on new context
**Preconditions:**
- The agent is entering a fresh context window (session start, resume, or compact).
- The local `insights.db` has project insights; the global db has general insights.
- At least one prior insight is tagged with a tag relevant to the current task.

**Trigger:** Agent receives a task with feature keywords. Per the read-on-new-context protocol, the agent queries tags first to discover the vocabulary, then searches by tag.

### Primary Flow (Happy Path)

1. Agent identifies the current feature keywords: `"insights hybrid corpus"`.
2. **Phase 1 — Tag discovery:** Agent invokes `claudebase insight tags --project <cwd-project> --json`.
3. CLI returns merged tag vocabulary: `[{"tag":"tokio","count":3},{"tag":"nginx","count":2},{"tag":"insights-hybrid-corpus","count":1}]`.
4. Agent selects the relevant tag: `insights-hybrid-corpus`.
5. **Phase 2 — Filtered search:** Agent invokes:
   ```
   claudebase insight search "hybrid corpus global db routing" --tag insights-hybrid-corpus --top-k 5 --json
   ```
6. CLI opens both the local `insights.db` and the global `~/.claude/knowledge/insights.db`.
7. CLI performs hybrid (BM25 + dense RRF) search over both dbs. Candidates are collected from each db separately, then RRF-fused.
8. CLI applies tag filter post-ranking: for each candidate, JOIN `insight_tags` WHERE `tag = 'insights-hybrid-corpus'` must return at least one row. Candidates not carrying the tag are excluded.
9. CLI returns up to 5 `SearchHit` JSON objects. The `source` field of each hit identifies which db it came from.
10. The result set includes both project insights (from local db) and general insights (from global db) that match the query and carry the tag.
11. The result set DOES NOT include insights from any other project's db — cross-project rows are walled off.
12. Agent cites the hits in `## Facts → Verified facts` as `insights-base: doc#<id> sha=<prefix> agent=<author> type=<kind> — query: "..." — verified: yes`.

**Postconditions:**
- Agent's `## Facts` block contains citations from both the project and general pools.
- No insight from an unrelated project's local db appears in the results.
- Neither db is modified.

### Alternative Flows

- **UC-IHC-7-A1: `--tag` with multiple values (OR / any-intersection semantics)** — Agent invokes `--tag tokio --tag mutex`. CLI filters: the candidate is returned if its tag set intersects the requested tags by AT LEAST ONE. An insight carries many tags; a result tagged only `tokio` IS included, a result tagged only `mutex` IS included, a result tagged both IS included. (Operator decision 2026-05-27: OR, not AND — when a tag query intersects an insight's tags, the insight is surfaced.)
- **UC-IHC-7-A2: No local db exists (fresh project, no prior insights)** — CLI queries only the global db. Returns general insights matching the query and tag. Exits 0 with partial results.
- **UC-IHC-7-A3: Zero hits after tag filter** — The query matched candidates, but none carried the requested tag. CLI returns `[]` and exits 0.
- **UC-IHC-7-A4: Encoder unavailable (degraded mode)** — CLI falls back to lexical-only BM25 search. Results are BM25-ranked instead of hybrid-ranked. stderr emits `warning: encoder unavailable; falling back to lexical mode`. The tag filter still applies.

### Error Flows

- **UC-IHC-7-E1: Both dbs absent** — No insights exist at all. CLI returns empty results `[]`. Exits 0 (not an error).
- **UC-IHC-7-E2: Global db corrupt** — CLI emits error for the global db open failure; whether it falls back to local-only or exits 1 depends on implementation. Assumption: exits 1 with `error: global insights db invalid; re-ingest required`. Surfaced as an assumption in `## Facts`.

### Edge Cases

- **UC-IHC-7-EC1: A planted insight in a second unrelated project's db** — The test plants a row in `/tmp/other-project/.claude/knowledge/insights.db` that would match the query and tag. Since the default merge is `merge(local-project + global-general)`, and the planted db is neither the cwd-local db nor the global db, it is NOT queried. The planted row is absent from results.
- **UC-IHC-7-EC2: Tag filter applied with no `--tag` flag** — `insight search` without `--tag` returns ALL matching insights from both dbs (no tag restriction), subject to other filters. This is the backward-compatible default from v0.6.0.
- **UC-IHC-7-EC3: `--tag` value that exists only in the global db** — The merged search correctly returns the matching general insight; no project insight is spuriously returned.

### Data Requirements

- **Input:** query string, `--tag <≥1>` (optional), `--top-k`, `--json`.
- **Output:** `SearchHit` JSON array with `chunk_id`, `score`, `snippet`, `source`, `doc_id`.
- **Side Effects:** none (read-only).

---

## UC-IHC-8: Read with `--general-only` Narrowing

**Actor:** SDLC Reader-Agent or Pipeline Operator
**Preconditions:** Both local and global dbs have insights. The actor wants only cross-project general knowledge.

**Trigger:** `claudebase insight search "nginx graceful reload" --general-only --json`

### Primary Flow (Happy Path)

1. Actor invokes the search with `--general-only`.
2. CLI ignores the cwd-local `insights.db` entirely. Only `~/.claude/knowledge/insights.db` is opened.
3. CLI executes the hybrid search against the global db alone.
4. CLI returns hits. All hits have `category='general'` in their source `documents` row.
5. Exits 0.

**Postconditions:**
- Results contain only global db rows. Project-category insights are absent even if they would have matched the query.

### Alternative Flows

- **UC-IHC-8-A1: `--general-only` combined with `--tag <t>`** — Tag filter applied after the global-only search. Only global insights tagged `<t>` are returned.
- **UC-IHC-8-A2: `--general-only` combined with `--project <slug>`** — The `--project` flag is logically contradictory with `--general-only`. Behavior: `--general-only` takes precedence and the `--project` flag is ignored (or CLI emits a warning). Assumption: CLI ignores `--project` silently when `--general-only` is set.

### Error Flows

- **UC-IHC-8-E1: Global db absent** — No general insights exist. CLI returns `[]`. Exits 0.

### Edge Cases

- **UC-IHC-8-EC1: `--general-only` and `--project-only` both set** — Mutually exclusive flags. CLI exits 2 with `error: --general-only and --project-only are mutually exclusive`. Assumption: clap mutual-exclusion group or manual check.

### Data Requirements

- **Input:** query, `--general-only`, optional `--tag`, `--top-k`, `--json`.
- **Output:** hits from global db only.
- **Side Effects:** none.

---

## UC-IHC-9: Read with `--project-only` Narrowing

**Actor:** SDLC Reader-Agent
**Preconditions:** Local db has project insights. Actor wants only this project's insights (no general noise).

**Trigger:** `claudebase insight search "slice 3 decision" --project-only --json`

### Primary Flow (Happy Path)

1. Actor invokes with `--project-only`.
2. CLI ignores `~/.claude/knowledge/insights.db` entirely. Only the cwd-local `insights.db` is opened.
3. CLI executes hybrid search against the local db alone.
4. Results are project-category insights only. No general insights appear even if they match.
5. Exits 0.

**Postconditions:**
- Results contain only local db rows.

### Alternative Flows

- **UC-IHC-9-A1: `--project-only` combined with `--tag <t>`** — Tag filter applied within the local-only scope.

### Edge Cases

- **UC-IHC-9-EC1: `--project-only` when local db absent** — Returns `[]`. Exits 0.
- **UC-IHC-9-EC2: `--project-only` and `--general-only` both set** — See UC-IHC-8-EC1. Exit 2.

### Data Requirements

- **Input:** query, `--project-only`, optional `--tag`, `--json`.
- **Output:** hits from local db only.
- **Side Effects:** none.

---

## UC-IHC-10: `claudebase run` Startup Upserts Project into Registry

**Actor:** `claudebase run` launcher process
**Preconditions:**
- The operator or an SDLC script invokes `claudebase run` from within a project directory.
- `~/.claude/knowledge/` exists (or the binary creates it as part of global-resolver initialization).

**Trigger:** `claudebase run` is invoked from `/Users/operator/projects/claudebase`.

### Primary Flow (Happy Path)

1. `claudebase run` dispatches to `run_claude_with_preset`.
2. At the TOP of `run_claude_with_preset` (before `exec()`), the binary calls `upsert_project(cwd)`.
3. `upsert_project` derives `name = "claudebase"` (basename of the canonical path `/Users/operator/projects/claudebase`).
4. Binary reads `~/.claude/knowledge/projects.json`. If absent, initializes an empty array `[]`.
5. Binary searches the array for an entry with `path == canonical("/Users/operator/projects/claudebase")`. Not found (first run).
6. Binary appends `{"name": "claudebase", "path": "/Users/operator/projects/claudebase", "last_seen": <epoch-secs>}`.
7. Binary writes the updated array to a temp file in `~/.claude/knowledge/`, then atomically renames it to `projects.json` (POSIX `rename(2)` — atomic on the same filesystem; near-atomic on Windows).
8. Binary calls `exec(claude ...)` on Unix (or `.status()` on Windows/non-Unix), handing control to Claude Code.

**Postconditions:**
- `~/.claude/knowledge/projects.json` exists and contains an entry for `claudebase` with non-null `last_seen`.
- The registry write completed BEFORE the process was replaced by `exec()`.

### Alternative Flows

- **UC-IHC-10-A1: `projects.json` already exists with other projects** — Binary reads the existing array, appends the new entry, and writes atomically. Prior entries are preserved.
- **UC-IHC-10-A2: `~/.claude/knowledge/` absent** — `resolve_global_insights_db()` (or the registry's own dir-creation step) creates it before `upsert_project` tries to write. The registry write then succeeds.

### Error Flows

- **UC-IHC-10-E1: Write to temp file fails (disk full / permissions)** — `upsert_project` logs the error but does NOT abort `claudebase run`. The `exec()` still fires. The registry is left unmodified (the rename never happened). Rationale: registry upsert is a best-effort side effect; it must not block the user's Claude session.
- **UC-IHC-10-E2: `projects.json` contains malformed JSON** — Binary treats the malformed file as an empty registry (starts fresh with the current project). The malformed file is overwritten atomically. No crash.

### Edge Cases

- **UC-IHC-10-EC1: Registry write completes, then `exec()` replaces the process** — All code after `exec()` on Unix is unreachable. This is the correctness requirement behind FR-IHC-6.5: the write MUST precede `exec()`. If the binary incorrectly places the write after `exec()`, the registry is never updated.
- **UC-IHC-10-EC2: Symlinked project directory** — `upsert_project` uses `canonicalize(cwd)` to resolve symlinks before computing `name` and deduping by `path`. Two different symlink paths pointing to the same canonical dir result in one entry, not two.

### Data Requirements

- **Input:** cwd at `claudebase run` invocation time.
- **Output:** updated `~/.claude/knowledge/projects.json`.
- **Side Effects:** `projects.json` file created or updated atomically; `exec()` replaces the process.

---

## UC-IHC-11: Registry Upsert Is Idempotent

**Actor:** `claudebase run` launcher process
**Preconditions:** `projects.json` already contains an entry for the cwd project with an older `last_seen` epoch.

**Trigger:** `claudebase run` invoked a second time from the same directory.

### Primary Flow (Happy Path)

1. `upsert_project(cwd)` runs.
2. Binary reads `projects.json`, finds an existing entry with `path == canonical(cwd)`.
3. Binary UPDATES `last_seen` to the current epoch. The `name` and `path` fields are unchanged.
4. Binary writes the updated array atomically (temp+rename).
5. `projects.json` still contains exactly one entry for this project (not two).

**Postconditions:**
- Entry count for the cwd project is exactly 1 (no duplicate).
- `last_seen` is updated to the current epoch.

### Alternative Flows

- **UC-IHC-11-A1: Same project registered from two different representations of the same canonical path (e.g., absolute vs relative, or trailing slash variants)** — `canonicalize()` normalizes both to the same absolute path. The dedup fires; only one entry exists.

### Edge Cases

- **UC-IHC-11-EC1: Registry written concurrently by two `run` invocations that BOTH read the old file before either writes** — Both read a version with N entries. Both compute N+1. The second rename wins and the first's write is lost (classic last-write-wins on atomic rename). The registry ends with N+1 entries rather than N+2 — the dedup did not fire for the second writer. The cwd project is still registered; the `last_seen` of the winner is correct. No corruption. This is the acceptable trade-off of the atomic-rename strategy.

### Data Requirements

- **Input:** same cwd as prior invocation.
- **Output:** `projects.json` with one updated entry (not duplicated).
- **Side Effects:** `projects.json` atomically rewritten.

---

## UC-IHC-12: Concurrent `claudebase run` Invocations Race on `projects.json`

**Actor:** Two simultaneous `claudebase run` launcher processes (e.g., two terminal windows)
**Preconditions:** `~/.claude/knowledge/projects.json` exists with N entries. Neither invocation has the cwd project registered yet.

**Trigger:** Two `claudebase run` processes start within milliseconds of each other from the same or different project directories.

### Primary Flow (No Corruption — Happy Path)

1. Process A calls `upsert_project`. Reads `projects.json` (N entries).
2. Process B calls `upsert_project`. Reads `projects.json` (N entries, same snapshot).
3. Process A writes temp file → renames to `projects.json` (N+1 entries, project-A added).
4. Process B writes temp file → renames to `projects.json` (N+1 entries, project-B added — but project-A's entry from step 3 may be lost if B's temp file was computed from the step-2 snapshot).
5. `projects.json` is never in a partially-written / corrupt state — the atomic rename guarantees a complete JSON array is always readable.
6. Both processes call `exec()` and hand control to Claude Code.

**Postconditions:**
- `projects.json` is a valid JSON array (never corrupt or partial-write).
- One of the two projects MAY be absent if the last-write-wins race resulted in the losing write being overwritten. This is the documented acceptable trade-off per NFR-IHC-3.
- On the NEXT `claudebase run` invocation from the missing project, it will be registered then.

### Alternative Flows

(none — the invariant is no corruption, not perfect convergence)

### Error Flows

- **UC-IHC-12-E1: Rename system call fails on Windows (file locked)** — Windows does not guarantee that `rename` succeeds if the target is open. The second writer may receive an OS error. Mitigation: the write fails silently (best-effort); registry is left in a valid prior state.

### Edge Cases

- **UC-IHC-12-EC1: Three or more concurrent writers** — The same last-write-wins analysis applies. The registry remains valid JSON after every completed rename; no intermediate corrupt state is observable.

### Data Requirements

- **Input:** concurrent invocations.
- **Output:** `projects.json` remains valid JSON throughout; at least one project entry added.
- **Side Effects:** atomic rename ensures no torn write.

---

## UC-IHC-13: Schema v5 Migration of an Existing v4 `insights.db`

**Actor:** `claudebase` binary (any `insight` subcommand opens the db)
**Preconditions:**
- `<project>/.claude/knowledge/insights.db` exists at schema v4 (written by a v0.6.0 binary).
- The db has the 4 existing SDLC-repo insight rows (`source_path LIKE 'agent:%'`).
- The db also has zero or more books-corpus rows (`source_path NOT LIKE 'agent:%'`) in `index.db` (the books db is separate — not affected; but the `documents` table shared context applies).

**Trigger:** A v0.7.0 binary opens the db for any `insight` operation. `open_or_init_v2` detects `schema_version = 4` and runs the `v4 → 5` migration branch.

### Primary Flow (Happy Path)

1. `open_or_init_v2` reads `PRAGMA user_version` → returns 4.
2. Binary enters the `v == 4 → 5` migration branch.
3. Binary begins a transaction.
4. Binary executes `SCHEMA_V5_DELTA`: `ALTER TABLE documents ADD COLUMN category TEXT`, `ALTER TABLE documents ADD COLUMN project_slug TEXT`.
5. Binary creates `insight_tags(doc_id, tag, UNIQUE(doc_id, tag))` and the two indexes.
6. **Backfill:** `UPDATE documents SET category = 'project', project_slug = '<db-path-basename>' WHERE source_path LIKE 'agent:%' AND category IS NULL`. The 4 SDLC-repo insight rows receive `category='project'` and `project_slug='claudebase'` (or whatever the db's project basename is).
7. **Default tag insertion:** `INSERT OR IGNORE INTO insight_tags (doc_id, tag) SELECT id, COALESCE(NULLIF(feature_slug, ''), 'untagged') FROM documents WHERE source_path LIKE 'agent:%' AND category = 'project'`. Each of the 4 insight rows receives one tag row derived from its `feature_slug` (or `'untagged'` if `feature_slug` is NULL).
8. **Books rows assertion:** Any `documents` row where `source_path NOT LIKE 'agent:%'` retains `category = NULL` and receives zero `insight_tags` rows.
9. Binary sets `PRAGMA user_version = 5`. Commits the transaction.
10. Migration is complete. The calling subcommand proceeds normally.

**Postconditions:**
- `PRAGMA user_version` returns 5.
- `pragma_table_info(documents)` shows `category` and `project_slug` columns.
- `insight_tags` table exists with indexes.
- The 4 insight rows each have `category='project'`, non-empty `project_slug`, and at least one `insight_tags` row.
- All books-corpus `documents` rows have `category = NULL` and zero `insight_tags` rows.
- The migration was transactional — if it failed midway, it would have rolled back and left `schema_version = 4`.

### Alternative Flows

- **UC-IHC-13-A1: Idempotent re-open at v5** — If `schema_version` is already 5, `open_or_init_v2` enters the `v == 5` probe branch. It checks `pragma_table_info` for the `category` and `project_slug` columns and the `insight_tags` table. If present, proceeds without re-running the migration. No-op.
- **UC-IHC-13-A2: Fresh database (schema_version = 0 / new file)** — `open_or_init_v2` stamps schema version 5 and applies `SCHEMA_V5_DELTA` in a single pass (the fresh-database branch). No backfill needed (no existing rows).
- **UC-IHC-13-A3: `insight_tags` table already exists from a partial prior migration** — The `CREATE TABLE IF NOT EXISTS` guard means no error. The probe branch verifies correctness before proceeding.

### Error Flows

- **UC-IHC-13-E1: Migration fails midway (e.g., disk full during `ALTER TABLE`)** — The outer transaction rolls back. `schema_version` remains 4. The db is in a valid v4 state. On the next open, the migration is retried. No corruption.
- **UC-IHC-13-E2: `schema_version` is something other than 4 or 5 (e.g., 3, 6)** — Binary should emit `error: unsupported schema version <N>; manual migration required`, exits 1. Assumption: this is the defensive fallback branch.

### Edge Cases

- **UC-IHC-13-EC1: Existing v4 insights with `feature_slug = NULL`** — Backfill step 7 uses `COALESCE(NULLIF(feature_slug, ''), 'untagged')`. Such rows receive `tag = 'untagged'`. Valid; at least one tag is always present after migration.
- **UC-IHC-13-EC2: Existing v4 insights with an empty string `feature_slug`** — `NULLIF(feature_slug, '')` converts `''` to NULL; `COALESCE` then picks `'untagged'`. Same result as EC1.
- **UC-IHC-13-EC3: Zero v4 insight rows (only book rows in the db)** — Backfill UPDATE affects zero rows. `insight_tags` remains empty. Migration completes successfully. `pragma_table_info` assertions still pass (columns exist, table exists, zero rows is valid).

### Data Requirements

- **Input:** existing v4 `insights.db`.
- **Output:** v5 schema with `category`, `project_slug` columns, `insight_tags` table; existing insight rows backfilled.
- **Side Effects:** `PRAGMA user_version` updated to 5; new columns and table added; `insight_tags` rows inserted for v4 insight rows.

---

## UC-IHC-14: Global db Absent — First `--category general` Write Creates It

**Actor:** SDLC Writer-Agent
**Preconditions:**
- `~/.claude/knowledge/insights.db` does NOT exist.
- `~/.claude/knowledge/` directory does NOT exist (or exists but has no `insights.db`).

**Trigger:** First-ever `insight create --category general` call on a machine that has never had a global insights db.

### Primary Flow (Happy Path)

1. Agent invokes `claudebase insight create "some general lesson" --category general --tags general-knowledge --type agent-learned --agent prd-writer --salience medium`.
2. CLI routes to `resolve_global_insights_db()`.
3. `resolve_global_insights_db()` calls `create_dir_all("~/.claude/knowledge/")` — creates the directory (and any missing parent components) with appropriate permissions.
4. CLI calls `open_or_init_v2` on `~/.claude/knowledge/insights.db` (file does not yet exist).
5. SQLite creates the file. `open_or_init_v2` enters the fresh-database branch, applies `SCHEMA_V5_DELTA`, stamps schema version 5.
6. CLI writes the insight row, tags, and chunks. Exits 0.

**Postconditions:**
- `~/.claude/knowledge/` directory exists.
- `~/.claude/knowledge/insights.db` exists with schema v5.
- The new insight row has `category='general'`, `project_slug=NULL`.
- `insight_tags` has the `('general-knowledge')` row.

### Alternative Flows

- **UC-IHC-14-A1: `~/.claude/knowledge/` exists but `insights.db` is absent** — `create_dir_all` is a no-op (directory present). SQLite creates the db file. Proceeds from step 5 as above.

### Error Flows

- **UC-IHC-14-E1: `~/.claude/knowledge/` cannot be created (HOME is read-only)** — `create_dir_all` fails with OS error. CLI emits `error: could not create directory ~/.claude/knowledge/: <error>`, exits 1.

### Data Requirements

- **Input:** `--category general` on a machine with no prior global db.
- **Output:** `~/.claude/knowledge/insights.db` created, insight row inserted.
- **Side Effects:** directory and db file created.

---

## UC-IHC-15: SessionStart Hook Fires on New Context Window

**Actor:** SessionStart hook (`claudebase-read-insights-reminder.sh` / `.ps1`)
**Preconditions:**
- The hook is wired into `~/.claude/settings.json` under `hooks.SessionStart`.
- A Claude Code session is starting (startup, resume, or compact event).
- `claudebase` is on PATH.

**Trigger:** A `startup`, `resume`, or `compact` SessionStart event fires.

### Primary Flow (Happy Path — Unix)

1. Claude Code fires the `SessionStart` event with `event_type = "startup"` (or `"resume"` or `"compact"`).
2. `~/.claude/hooks/claudebase-read-insights-reminder.sh` executes.
3. The script emits `additionalContext` containing a reminder message of ≤200 words. The reminder:
   - States that the agent is entering a fresh context window.
   - Instructs the agent to call `claudebase insight tags --project <cwd-project>` to discover the available tag vocabulary.
   - Instructs the agent to call `claudebase insight search "<kw>" --tag <t>` to load relevant general and project insights.
   - Phrases the pull as conditional: "if entering fresh context" (to reduce nag on routine messages).
4. Claude Code injects the `additionalContext` into the agent's context.
5. The hook exits 0.

**Postconditions:**
- The agent's context window includes the read-insights reminder.
- The hook does not block the session startup.
- The hook does NOT itself execute any `claudebase` commands — it only emits the reminder text.

### Alternative Flows

- **UC-IHC-15-A1: Windows — PowerShell hook** — `claudebase-read-insights-reminder.ps1` fires. The `.ps1` file is ASCII-only (no BOM), passes `powershell -Command "& { ... }"` parse without error on PS 5.1. Emits equivalent `additionalContext`. Exits 0.
- **UC-IHC-15-A2: `claudebase` not on PATH at hook execution time** — The hook script does NOT invoke `claudebase` directly; it only emits a text reminder. The reminder mentions `claudebase insight tags` as a command the agent should call later. The hook itself succeeds regardless of whether `claudebase` is findable.

### Error Flows

- **UC-IHC-15-E1: Hook script has a bash syntax error** — `bash -n claudebase-read-insights-reminder.sh` exits non-zero. AC-IHC-14 is violated. The session may start without the reminder being injected. (This is a test-time detection; the hook file should be syntax-verified before deployment.)
- **UC-IHC-15-E2: Hook emits >200 words** — NFR-IHC-4 violated. The context injection still succeeds; the hook is not truncated by the harness. But the reminder violates the lightweight-additionalContext contract. Mitigation: word-count test in the hook test suite.

### Edge Cases

- **UC-IHC-15-EC1: `compact` event fires mid-long-session** — The hook fires again. The agent sees the reminder for a second (or third) time. The conditional phrasing ("if entering fresh context…") disambiguates: the agent should only act on the reminder if it genuinely lost context. If context is intact, the agent ignores the reminder. This is the documented acceptable posture per NFR-IHC-4.

### Data Requirements

- **Input:** SessionStart event from Claude Code harness.
- **Output:** `additionalContext` string ≤200 words injected into agent context.
- **Side Effects:** none (the hook is read-only).

---

## UC-IHC-16: SessionStart Hook Install Is Idempotent

**Actor:** Pipeline Operator running `bash install.sh --yes`
**Preconditions:**
- `~/.claude/settings.json` may or may not already contain a `hooks.SessionStart` entry for `claudebase-read-insights-reminder.sh`.

**Trigger:** `bash install.sh --yes` runs (once or multiple times).

### Primary Flow (First Install)

1. `install.sh` invokes the jq expression to add the new hook entry to `hooks.SessionStart` in `~/.claude/settings.json`.
2. The entry `{"event": "SessionStart", "command": "~/.claude/hooks/claudebase-read-insights-reminder.sh"}` is appended to the array.
3. `settings.json` is updated. Exit 0.

**Postconditions:**
- Exactly one entry for `claudebase-read-insights-reminder.sh` exists in `hooks.SessionStart`.

### Primary Flow (Idempotent Re-run)

1. `install.sh` is run a second time.
2. The jq expression checks whether the entry already exists (keyed by command string).
3. Because the entry is present, `install.sh` skips the append.
4. `settings.json` still contains exactly one entry for `claudebase-read-insights-reminder.sh`.

**Postconditions:**
- Entry count is still 1 (no duplicate appended).

### Alternative Flows

- **UC-IHC-16-A1: Windows — `install.ps1`** — Uses `ConvertFrom-Json` / `ConvertTo-Json`. The same idempotent-dedup-by-command-string logic is applied in PowerShell. Same postconditions.

### Error Flows

- **UC-IHC-16-E1: `~/.claude/settings.json` does not exist** — `install.sh` creates the file (or initializes it with a minimal skeleton) before appending the hook entry.
- **UC-IHC-16-E2: `jq` not installed** — `install.sh` cannot perform the jq expression. Should emit a warning and skip the hook wiring gracefully, rather than failing the entire install.

### Edge Cases

- **UC-IHC-16-EC1: `~/.claude/settings.json` contains malformed JSON** — `jq` will fail to parse. `install.sh` should detect the non-zero exit and emit a warning rather than overwriting the file with a truncated result.

### Data Requirements

- **Input:** `~/.claude/settings.json` (may or may not exist).
- **Output:** `~/.claude/settings.json` with exactly one `claudebase-read-insights-reminder.sh` entry.
- **Side Effects:** `~/.claude/settings.json` updated; hook script file at `~/.claude/hooks/claudebase-read-insights-reminder.sh` installed.

---

## UC-IHC-17: Cross-Project Read Attempt Walled Off (Default Posture)

**Actor:** SDLC Reader-Agent running in project A
**Preconditions:**
- Project A has a local `insights.db` at `<project-A>/.claude/knowledge/insights.db`.
- Project B has a local `insights.db` at `<project-B>/.claude/knowledge/insights.db` with insights that would match the query.
- The global db has general insights.

**Trigger:** Agent in project A invokes `claudebase insight search "query" --json` (default — no `--project B`).

### Primary Flow (Happy Path — Wall Enforced)

1. CLI determines cwd is project A. Opens project A's local db and the global `~/.claude/knowledge/insights.db`.
2. CLI does NOT open or query project B's db.
3. Search results contain: project A's local insights (if any match) + general insights from the global db.
4. Project B's insights are ABSENT from the results even if they would have scored higher than the returned hits.
5. Exits 0.

**Postconditions:**
- Project B's insights are not leaked to project A's agent.
- Scope isolation is enforced without any explicit opt-out flag required.

### Edge Cases

- **UC-IHC-17-EC1: Project B's db is referenced via a symlink in project A's directory tree** — `resolve_project_root` uses `canonicalize` to detect the project boundary. The symlink resolves to project B's path, which is outside project A's canonical root. CLI ignores it.

### Data Requirements

- **Input:** default `insight search` from project A.
- **Output:** project A + general results only.
- **Side Effects:** none.

---

## UC-IHC-18: Cross-Project Read via Explicit `--project <slug>` (Registry Lookup)

**Actor:** SDLC Reader-Agent or Pipeline Operator
**Preconditions:**
- Project B is registered in `~/.claude/knowledge/projects.json` (it has been run at least once with `claudebase run`).
- The operator/agent explicitly wants to read project B's insights.

**Trigger:** `claudebase insight search "slice 3 decision" --project claudebase --json` invoked from a different project's directory.

### Primary Flow (Happy Path)

1. CLI reads `--project claudebase`. Calls `resolve_project_path("claudebase")` against the registry.
2. Registry lookup finds `{"name": "claudebase", "path": "/Users/operator/projects/claudebase", "last_seen": ...}`. Returns the path.
3. CLI constructs the path: `/Users/operator/projects/claudebase/.claude/knowledge/insights.db`.
4. CLI opens project B's db (bypassing cwd-containment because this is an explicit opt-in).
5. CLI searches project B's db (and optionally the global db — behavior: if `--project` is supplied, the default merge changes to `merge(project-B + global-general)`, replacing the cwd-project with the named project).
6. Results contain project B's insights and general insights. Cwd-project's insights are excluded.
7. Exits 0.

**Postconditions:**
- Project B's insights are accessible via explicit opt-in.
- Cwd project (project A) insights are not included in the results.

### Alternative Flows

- **UC-IHC-18-A1: `--project` names the cwd project** — `resolve_project_path` returns the same path as `resolve_project_root`. Identical to the default behavior. No error.

### Error Flows

- **UC-IHC-18-E1: `--project <slug>` not found in registry** — CLI emits `error: project '<slug>' not found in registry ~/.claude/knowledge/projects.json`, exits 1.
- **UC-IHC-18-E2: Registry-resolved path does not exist on disk** — CLI emits `error: insights db not found at <path>`, exits 1. (The project may have been moved or deleted after registration.)

### Data Requirements

- **Input:** `--project <slug>`, query string.
- **Output:** hits from named project's db + global db.
- **Side Effects:** none.

---

## UC-IHC-19: `insight gc` Runs Against Both DBs (Default) and Global-Only (`--category general`)

**Actor:** Pipeline Operator
**Preconditions:** Both dbs have insights with varying salience and ages. Some are past their TTL.

**Trigger:** `claudebase insight gc --json`

### Primary Flow (Default — Both DBs)

1. Operator invokes `claudebase insight gc --json` from a project directory.
2. CLI runs TTL-purge against the local db: `DELETE FROM documents WHERE salience='medium' AND ingested_at < (now - 365d)`, and similarly for `low` (90d). On-delete-cascade removes `insight_tags` rows and `chunks` rows.
3. CLI runs TTL-purge against the global db: same logic.
4. CLI emits combined `{"medium_deleted": N, "low_deleted": M, "freed_bytes": B}`.
5. Exits 0.

### Alternative Flows

- **UC-IHC-19-A1: `--category general`** — Purge runs against global db only. Local db is unchanged.
- **UC-IHC-19-A2: `--project-only`** — Purge runs against local db only.
- **UC-IHC-19-A3: No expired insights** — Both purge queries return zero rows deleted. CLI emits `{"medium_deleted": 0, "low_deleted": 0, "freed_bytes": 0}`.

### Data Requirements

- **Input:** optional `--category`, `--project-only`, `--general-only`.
- **Output:** `{medium_deleted, low_deleted, freed_bytes}` JSON.
- **Side Effects:** `documents`, `insight_tags`, `chunks` rows deleted from one or both dbs.

---

## UC-IHC-20: Dedup Continues to Fire Per-DB After Mandatory-Flag Enforcement

**Actor:** SDLC Writer-Agent
**Preconditions:** An insight with the same body was already written to the same db within the last 30 days by the same agent.

**Trigger:** Agent invokes the same `insight create` body again (e.g., replayed from a template).

### Primary Flow (Exact-Sha Dedup — Project)

1. Agent invokes `claudebase insight create "same body" --category project --tags sometag --type agent-learned --agent planner --salience medium`.
2. CLI routes to local db (project category).
3. CLI computes sha256. Probe finds existing row with matching `(agent_name, sha256)` within 30 days.
4. CLI skips write. Emits `{"status":"deduped","doc_id":<N>}` and exits 0.
5. No duplicate `documents` row. No duplicate `insight_tags` rows.

### Alternative Flows

- **UC-IHC-20-A1: Dedup against general db** — Same body, `--category general`. Dedup probe is against `~/.claude/knowledge/insights.db`. Fires if the same `(agent_name, sha256)` was written to the global db within 30 days.
- **UC-IHC-20-A2: Project-category body re-submitted as general-category** — The bodies are identical, but the routing targets different dbs. The project-db dedup probe runs against the local db (not found — that body is in the global db). The general-db dedup probe runs against the global db (found). Each db's dedup is independent; this is correct behavior.
- **UC-IHC-20-A3: Same body, different agent** — Cross-agent non-dedup rule: both rows are kept. Matching UC-AIB-3 behavior extended to dual-db.

### Data Requirements

- **Input:** duplicate body with valid `--category` and `--tags`.
- **Output:** `{"status":"deduped","doc_id":<N>}`.
- **Side Effects:** none (no write).

---

## UC-IHC-21: `insight delete` with `--category general` Resolves Against Global DB

**Actor:** Pipeline Operator
**Preconditions:** An insight with `id=42` exists in `~/.claude/knowledge/insights.db`.

**Trigger:** `claudebase insight delete 42 --category general --json`

### Primary Flow (Happy Path)

1. Operator invokes `insight delete 42 --category general --json`.
2. CLI opens the global db via `resolve_global_insights_db()`.
3. CLI locates `documents.id = 42`. Confirms `source_path LIKE 'agent:%'` (refuses to delete books-corpus rows).
4. CLI deletes: `DELETE FROM documents WHERE id = 42`. On-delete-cascade removes `insight_tags` rows and `chunks` rows.
5. CLI emits `{"deleted": true, "id": 42}`. Exits 0.

**Postconditions:**
- Row 42 and all associated `insight_tags` and `chunks` rows are absent from the global db.
- The local project db is unchanged.

### Error Flows

- **UC-IHC-21-E1: `id=42` not found in global db** — CLI exits 1 with `error: insight 42 not found`.
- **UC-IHC-21-E2: `id=42` is a books-corpus row** — CLI refuses deletion. Exits 1 with `error: id 42 is a books-corpus row; delete via 'claudebase delete <source-id>'`.
- **UC-IHC-21-E3: `--category general` omitted but id exists only in global db** — CLI resolves against local db (default). Not found. Exits 1.

### Data Requirements

- **Input:** `<id>`, `--category general`.
- **Output:** `{"deleted": true, "id": N}`.
- **Side Effects:** document + cascade deleted from global db.

---

## Facts

### Verified facts

- PRD §18 read in full: lines 785-1108 of `docs/PRD.md` this session. 9 FR groups (FR-IHC-1..9), 17 ACs (AC-IHC-1..17), 7 NFRs. — salience: high
- `.claude/plan.md` read in full (199 lines) this session. Plan is consistent with PRD §18; no contradictions detected between the two sources. — salience: high
- `docs/use-cases/agent-insights-base_use_cases.md` read in full this session. Uses `UC-AIB-N` prefix. The new feature (§18) is not covered there — §16 coverage only. Creating a new file with `UC-IHC-N` prefix is correct. — salience: high
- `run_claude_with_preset` at `main.rs:154`; `exec()` called at `main.rs:199` on Unix. Registry write MUST precede `exec()` or it never runs — source: `.claude/plan.md` Verified facts + PRD §18 FR-IHC-6.5. — salience: high
- `chat.db` lives at `$HOME/.claude/knowledge/chat.db` (store.rs:1483) — precedent for a HOME-rooted global path — source: `.claude/plan.md` Verified facts line 160. — salience: medium
- `resolve_project_root` rejects paths not under cwd — `resolve_global_insights_db()` MUST bypass this gate — source: `.claude/plan.md` Verified facts line 162. — salience: high
- `insight create` v0.6.0 flags: `--type`, `--agent`, `--session`, `--feature`, `--salience`, `--source-artifact`, `--project-root`, `--db-name`; no `--category`, no `--tags` — source: `.claude/plan.md` External contracts, `src/cli.rs:705-912`. — salience: high
- `SCHEMA_V4_DELTA` at `src/store.rs:197-207` adds insight metadata as nullable columns on the shared `documents` table; books rows have them NULL — the v5 pattern (category/project_slug as columns) is consistent — source: `.claude/plan.md` Verified facts lines 155-156. — salience: high
- Knowledge base: `doc_count=0` (empty). Corpus scope verdict: No overlap (empty corpus, task is CLI engineering + SQLite schema migration). Topical queries skipped. — salience: low
- Insights corpus query `claudebase insight search "insights hybrid corpus tags category routing" --feature "insights-hybrid-corpus" --salience high --top-k 5`: returned 0 hits. No prior-session insights on this feature. — salience: low
- Commit `2d5eb8d` establishes the ASCII-only `.ps1` constraint (PS 5.1 parse failure with non-ASCII) — source: git log shown in session context. — salience: medium
- Existing use-cases files: `agent-chat-daemon_use_cases.md` and `agent-insights-base_use_cases.md` — confirmed by `ls` this session. Neither covers §18 domain. — salience: medium

### External contracts

- **`claudebase` CLI v0.6.0** — symbol: `insight create` required flags: `--category <general|project>` (new, breaking), `--tags <tag>` (new, breaking, repeatable, ≥1 required). Current v0.6.0 has neither. — source: `.claude/plan.md` External contracts block, verified this session — verified: yes — salience: high
- **`clap` derive macros** — symbol: `#[arg(long, value_enum)]` for `--category` enum; `#[arg(long)]` repeatable `Vec<String>` for `--tags`; required constraint for `--category` enforced at clap-parse time. The business-logic constraint for `--tags` (empty-vec check) is enforced post-parse in `run_insight_create`. — source: `.claude/plan.md` External contracts, `cli.rs:732,767` — verified: yes — salience: medium
- **`rusqlite`** — symbol: `Connection::transaction`, `execute_batch`, `pragma_table_info`, `ALTER TABLE … ADD COLUMN`, `ON DELETE CASCADE` foreign key — used by v4 migration; v5 reuses same primitives — source: `.claude/plan.md` External contracts, `store.rs:234-341` — verified: yes — salience: high
- **SQLite `rename(2)` atomicity** — symbol: POSIX `rename(2)` is atomic for same-filesystem files; Windows NTFS `MoveFileExW(MOVEFILE_REPLACE_EXISTING)` is near-atomic — used for registry write safety (FR-IHC-6.3). Source: established POSIX spec; cited from `.claude/plan.md` FR-IHC-6.3 and Risks section — verified: no — assumption; will be confirmed by security-auditor review at Slice 6. — salience: high
- **`std::env::var("HOME")`** — symbol: `std::env::var("HOME")` on Unix, `std::env::var("USERPROFILE")` on Windows — used by `resolve_global_insights_db()`. Returns `Err` if variable is unset. — source: PRD §18 FR-IHC-2.1, `.claude/plan.md` Slice 2 — verified: no — assumption (standard Rust stdlib, well-known behavior). — salience: medium

### Assumptions

- `insight_tags` insert for duplicate tags uses `INSERT OR IGNORE` (SQLite `UNIQUE(doc_id, tag)` constraint); the error is silently dropped rather than surfaced to the caller — risk: if the implementation uses plain `INSERT`, duplicates cause a constraint error that aborts the transaction — how to verify: Slice 3 test suite covers the duplicate-tag scenario. — salience: high
- `--general-only` and `--project-only` are mutually exclusive; CLI emits exit-2 if both are supplied — risk: if not validated, behavior is undefined — how to verify: Slice 5 implementation and test; UC-IHC-8-EC1 drives the test case. — salience: medium
- When `--general-only` and `--project` are combined, `--general-only` takes precedence and `--project` is silently ignored — risk: alternative is an exit-2 conflict error; either is acceptable but the test must match the implementation choice — how to verify: architect/implementer to decide in Slice 5; QA tests UC-IHC-8-A2. — salience: low
- The tag-filter logic uses `WHERE doc_id IN (SELECT doc_id FROM insight_tags WHERE tag IN (?, ?, ...))` with **OR / any-intersection semantics** across multiple `--tag` values (a result is returned if it carries AT LEAST ONE requested tag) — resolved by operator decision 2026-05-27; FR-IHC-5.1/5.3 specify OR; test explicitly asserts a result carrying only one of two requested tags IS present. — salience: high
- When `--project <slug>` is supplied to `insight search`, the default merge becomes `merge(project-B + global-general)` (replacing cwd-project with the named project) — risk: implementation may instead do a three-way merge or exclude global — how to verify: UC-IHC-18 drives the test. — salience: medium
- `insight gc` with `--category general` runs only against the global db; without `--category`, it runs against both — risk: partial GC semantics not explicitly stated in one FR; UC-IHC-19 drives the test — how to verify: FR-IHC-5.6 covers this explicitly; assumption is a direct read of the FR. — salience: medium
- Tag normalization strips leading `#` characters only (not trailing, not mid-string) and then lowercases — risk: if other characters are stripped/normalized, tag queries may not match stored values — how to verify: FR-IHC-3.5 specifies exactly this; test verifies the stored value. — salience: medium
- UC-IHC-7-E2 (global db corrupt): implementation exits 1 rather than falling back to local-only. This is a safe conservative behavior, but it could be relaxed to partial-success. Labeled as assumption since the PRD does not specify the corrupt-global-db error posture explicitly. — salience: low

### Open questions

- Should a corrupt global db in `insight search` cause exit 1 (fail fast) or fall back to local-only results (partial success)? Not specified in FR-IHC-5. Needs: architect call or planner decision in Slice 5. — salience: medium
- UC-IHC-8-A2: is `--general-only --project <slug>` an exit-2 conflict or a silent `--project`-ignore? Needs: implementer decision in Slice 5; both behaviors satisfy the PRD since the PRD does not address the combination. — salience: low
- Does the merged `insight tags` result (UC-IHC-5) sum counts across dbs for the same tag, or return per-db counts separately? PRD FR-IHC-4.5 says "merges tags from BOTH" — implicit that counts should be summed for a unified view, but the JSON shape `[{tag, count}]` does not distinguish source. Assumption: summed. Needs: QA confirmation in Slice 4 test. — salience: medium
- What is the behavior of `claudebase insight list` with the new dual-db posture? FR-IHC-5.1 adds the new flags but FR-IHC-5.2 specifies merge only for `search`, `list`, and `random`. `list` should follow the same merged default. UC-IHC-5 and UC-IHC-7 cover tags and search; `list` is not separately covered in these use cases because its additional scenarios are parallel to `search` and would be direct derivatives. Needs: QA planner to confirm coverage in test cases. — salience: low

---

## Decisions

### Inbound validation

- Task received: create `docs/use-cases/insights-hybrid-corpus_use_cases.md` for §18. Task is coherent. PRD §18 (lines 785-1108) and `.claude/plan.md` (199 lines) are consistent with each other — no upstream contradiction detected. Challenged: yes (checked for plan↔PRD drift; none found). Outcome: proceeded. — salience: high
- The task prompt specifies "actors: SDLC writer-agent, SDLC reader-agent on new context, operator, `claudebase run` launcher process". Cross-checked against PRD §18.1 and the plan's deliverables checklist — actors match. The "hook" is a separate actor distinct from "SDLC reader-agent" (the hook fires passively; the agent acts on it). A fifth actor, `SessionStart hook`, is added for clarity without violating the task spec — the task prompt's list is a minimum, not an exhaustive specification. — salience: medium
- Existing file `agent-insights-base_use_cases.md` covers §16 (UC-AIB-N prefix). §18 is a new domain (global db, hybrid corpus, tags, registry, hooks not present in §16). Creating a new file rather than updating is correct. Challenged: yes (verified no §18 domain overlap with §16 file). Outcome: new file created. — salience: high

### Decisions made

- **UC-IHC-N prefix chosen over UC-AIB-N continuation** — FR prefix in the PRD is `FR-IHC-N`; the feature slug is `insights-hybrid-corpus`. Using `UC-IHC-N` is consistent with the feature boundary and avoids renumbering the existing §16 use cases. Q1 hack? no. Q2 sane? yes. Q3 alternatives? UC-AIB-13 continuation (rejected: confuses §16 and §18 coverage in the test plan). Q4 cause. Q5 n/a. — salience: high
- **21 use cases authored** covering all 9 FR groups and all 17 ACs. The coverage table maps each UC to its FRs/ACs. The QA planner's test-case file should be derived directly from this table. Q1 hack? no. Q2 sane? yes. Q3 — completeness validated against the FR list in §18.3. Q4 cause. Q5 n/a. — salience: high
- **Tag-filter semantics resolved to OR / any-intersection by operator decision (2026-05-27)** — FR-IHC-5.1/5.3 specify OR (an insight carrying ≥1 requested tag is returned); the prior AND framing was superseded. Q1 hack? no. Q2 sane? yes (wider net suits the "pull relevant insights by tag" reader-agent goal). Q4 cause. Q5 tracked: Slice 5 test asserts a single-tag-match insight IS present. — salience: medium
- **UC-IHC-12 covers concurrency as a distinct use case** — The atomic-rename contract is a non-obvious correctness property that QA needs to test explicitly. Making it a UC rather than an edge case of UC-IHC-11 ensures it gets a dedicated test scenario. Q1 hack? no. Q2 sane? yes. Q3 alternatives? treat as EC of UC-IHC-11 (rejected: the concurrency scenario has a distinct actor setup and distinct postconditions). Q4 cause. Q5 n/a. — salience: medium

### Hacks acknowledged

(none)

### Symptom-only patches (with root-cause links)

(none)
