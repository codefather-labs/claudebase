# Test Cases: CLI-to-CLI Routing — Agent-to-Agent Communication via Daemon

> Based on [PRD §20](../PRD.md#20-cli-to-cli-routing--agent-to-agent-communication-via-daemon) and [Use Cases](../use-cases/cli-to-cli-routing_use_cases.md).
>
> Feature slug: `cli-to-cli-routing`. Branch: `feat/multi-agent-on-v0.6`.
> Date: 2026-06-06.
>
> **Architecture verdict:** PASS-WITH-CONDITIONS — Slice 4 and Slice 7 require security pre-review before implementation. All architect amendments (corrected column names, migration file location, `thread_id` vs `thread`) applied in the use-cases document and carried forward here.

---

## Facts

### Verified facts

- PRD §20 lines 1604–1892 read in full this session via Read tool (offset 1604 limit 289) — source: Read tool invocation this session — salience: high.
- Use-cases file `docs/use-cases/cli-to-cli-routing_use_cases.md` lines 1–797 read in full this session across four Read tool calls — source: Read tool invocations this session — salience: high.
- Existing QA format reference: `docs/qa/multi-agent-telegram-on-v0.6_test_cases.md` lines 1–197 read this session — confirms 9-column table, `## Facts` / `## Decisions` blocks, UC coverage map, convention section — salience: medium.
- FR-C2C-1.1: v5→v6 migration adds `project_id TEXT`, `branch TEXT`, `working_dir TEXT`, `feature_description TEXT NULL`, `dnd_until_ts INTEGER NULL` with probe-before-ALTER idempotency — source: PRD §20 line 1629, Read this session — salience: high.
- FR-C2C-4.6: `from_agent_id` MUST be resolved from `agent_registry WHERE connection_id = <caller's connection_id>`, NOT from caller-supplied field — source: PRD §20 line 1678, Read this session — salience: high.
- FR-C2C-5.5: drain emits at most 10 `notifications/claude/channel` per agent per 30s tick; oldest 10 by `created_at ASC`; remainder left with `delivered_at = NULL` for next tick — source: PRD §20 line 1686, Read this session — salience: high.
- FR-C2C-5.1: indefinite DND (`"on"` state) encoded as `dnd_until_ts = i64::MAX = 9223372036854775807`; `NULL` means no DND; drain `WHERE dnd_until_ts < now()` naturally excludes `i64::MAX` — source: PRD §20 line 1682 + UC-C2C-4-C, Read this session — salience: high.
- FR-C2C-7.2: pre-flight hook event verification mandatory before Slice 7 implementation; fallback chain documented: Primary → PostToolUse+ExitPlanMode; Fallback A → UserPromptSubmit; Fallback B → Stop hook; Fallback C → operator-driven — source: PRD §20 line 1699, Read this session — salience: high.
- AC-C2C-1..AC-C2C-5 exact text verified at PRD §20 lines 1726–1730 — source: Read this session — salience: high.
- `chat_messages.thread_id` column (NOT `thread`) verified at use-cases file line 741; no CHECK constraint on prefix — source: use-cases file verified facts, PRD §20 line 1787, Read this session — salience: high.
- Migration function: `apply_agent_registry_c2c_migration` in `src/daemon/chat.rs`, invoked from `ensure_chat_db_schema` AFTER `apply_routing_migration` and `apply_pending_asks_migration` — source: PRD §20 line 1629 + use-cases file line 664, Read this session — salience: high.
- 18 use-case scenarios documented: UC-C2C-1 through UC-C2C-18, each with primary flow, alt flows (A/B/C where present), error flows (E1/E2), and edge cases (EC1/EC2/EC3) — source: use-cases file lines 32–717, Read this session — salience: high.
- Knowledge base corpus scope: `index.db` is present but `claudebase list` returned 0 documents on a prior ba-analyst run (use-cases Facts line 727). Task domain (daemon MCP routing, SQLite schema migration) has no overlap with an empty corpus. No topical queries executed — salience: low.
- Insights corpus `insights.db` exists; prior ba-analyst noted `error: search failed: no such column: to` on insight search invocation (use-cases Facts line 728) — treating corpus as unavailable this session. No insights-base citations — salience: medium.
- Evidence directory `docs/qa/evidence/cli-to-cli-routing/` does not yet exist — will be created by qa-engineer at qa-cycle execution time. Glob confirmed absence this session — salience: low.

### External contracts

- **`claudebase agent list-alive --project current --json`** — symbol: CLI subcommand output is a JSON array; each element contains `agent_id`, `branch`, `working_dir`, `feature_description` (nullable), `last_seen_at`, `dnd_until_ts` (nullable) per FR-C2C-6.2 — source: PRD §20 line 1691, Read this session — verified: yes (PRD as source) — salience: high.
- **`PRAGMA table_info(agent_registry)`** — symbol: SQLite pragma returning one row per column; column names match the schema declared in `src/daemon/chat.rs:443-518` — source: SQLite built-in pragma (not opened this session) — verified: no — assumption (well-established SQLite contract) — salience: medium.
- **`notifications/claude/channel` wire format** — symbol: meta fields `source` (string), `chat_id` (string), `thread` (string), `target_agent_id` (string), `meta.kind` (optional string) — source: PRD §18 frozen contract (PRD §20 line 1675 confirms frozen), Read this session — verified: yes — salience: high.
- **`chat_messages` table columns** — symbol: `thread_id TEXT NOT NULL`, `delivered_at INTEGER NULL`, `from_agent TEXT NOT NULL` — source: `src/daemon/chat.rs:426-434` verified by architect (use-cases External contracts line 751), Read this session — verified: yes — salience: high.
- **MCP tool `agent_describe` input schema** — symbol: `{ feature_id: string, branch: string, description: string }` per FR-C2C-3.2 — source: PRD §20 line 1663, Read this session — verified: yes (PRD as source) — salience: high.
- **MCP tool `agent_send` input schema** — symbol: `{ to_agent_id: string, content: string, urgent?: boolean }` per FR-C2C-4.1; response on DND-active: `{ queued: true, delivered_when: <ISO-8601> }` per FR-C2C-5.3 — source: PRD §20 lines 1669 + 1683, Read this session — verified: yes (PRD as source) — salience: high.
- **MCP tool `agent_set_dnd` input schema** — symbol: `{ state: string }` where state ∈ `{ "on", "off", "<N>m", "<N>h", "until HH:MM" }` per FR-C2C-5.1 — source: PRD §20 line 1682, Read this session — verified: yes (PRD as source) — salience: high.
- **`PostToolUse:ExitPlanMode` hook event** — symbol: `hooks.PostToolUse` array entry with `matchers: ["ExitPlanMode"]` in `~/.claude/settings.json` — source: PRD §20 line 1699; NOT directly verified against CC hook schema this session — verified: no — assumption. Risk: hook event semantics unconfirmed; Slice 7 pre-flight check mandatory per FR-C2C-7.2 — salience: high.
- **`i64::MAX` = `9223372036854775807`** — symbol: Rust `i64::MAX` constant; SQLite stores as INTEGER (up to 8-byte signed) — source: Rust stdlib + PRD §20 line 1682, OQ-UC-C2C-1 resolved — verified: yes — salience: high.

### Assumptions

- Daemon log line literals cited in `Evidence Required` columns (e.g., `sender identity rebound from X to Y`, `DND drain tick:`) are planning-time projections for what the implementer will emit. Risk: actual log wording may differ. Mitigation: qa-engineer accepts substring matches; implementer MUST update both test-cases and use-cases files if log wording diverges. Salience: high.
- Evidence directory `docs/qa/evidence/cli-to-cli-routing/` will be created by qa-engineer on first execution. All evidence file paths are relative to this directory. Salience: medium.
- Unit test function names cited in evidence columns (e.g., `tests/store_v6_test.rs::v6_migration_adds_five_columns`) are suggestions; actual names may differ. qa-engineer accepts any test function whose body covers the stated assertion. Salience: medium.
- The `from_agent` column exists in `chat_messages` per use-cases External contracts. Risk: if the column name differs in the actual schema, evidence SQL queries citing it will fail. Verify: `PRAGMA table_info(chat_messages)` at qa-cycle start. Salience: high.

### Open questions

- **OQ-TC-C2C-1**: Hook event log line and `additionalContext` injection format (TC-5.1, TC-5.2) depend on the Slice 7 pre-flight verification outcome. If fallback mode (UC-C2C-5-C) is chosen, TC-5.1 and TC-5.2 steps and evidence change accordingly. Needs: Slice 7 pre-flight result before qa-engineer executes TC-5.1/5.2. Salience: high.
- **OQ-TC-C2C-2**: `from_agent` column name in `chat_messages` — assumed from use-cases External contracts block but not verified by direct `src/daemon/chat.rs` read this session. Needs: implementer confirms column name at Slice 4 test-writer stage. Salience: medium.
- Corpus scope relevance: `index.db` present but 0 documents; task domain (CLI daemon, SQLite, MCP) not represented. No topical queries executed. Knowledge-base: corpus is absent/empty; task domain has no overlap. Skipping topical queries — corpus enrichment with daemon-protocol or SQLite reference materials would help future similar tasks. Salience: low.

## Decisions

### Inbound validation

- Task received: author `docs/qa/cli-to-cli-routing_test_cases.md` from PRD §20 and use-cases. Challenged: yes (Protocol 3). Q1: task coherent — PRD §20 has 8 FRs, 5 ACs, 18 UCs; all inputs verified by Read this session. Q2: no upstream errors in the authoring task itself; noted that use-cases UC-C2C-17 conflates two migration steps (routing migration vs. C2C extension migration) — task prompt resolves this by saying "PRD §20 as ground truth"; applied. Q3: justification is SDLC Step 4. Q4: no amplification risk. Outcome: proceeded. Salience: high.
- Architect amendments applied in use-cases file (corrected column names, `src/daemon/chat.rs` as migration host, `thread_id` not `thread`). These corrections are carried forward verbatim in all TC SQL queries. No new corrections needed at QA-planner stage. Salience: high.
- Coverage requirement: NO UI/UX cases — daemon-only feature. All Verification Classes are CLI, DB, FS, API, or Mixed. Confirmed: use-cases file line 28 states "primary flows are CLI + DB (Mixed). DND flows are CLI + DB. Hook flow is FS + CLI. Error flows are CLI. Edge cases are DB + CLI." — salience: high.

### Decisions made

- TC numbering scheme: `TC-C2C-N.M` where N = functional area number (matches section heading) and M = sequential within area. Q1 hack? no. Q2 sane? yes — consistent with TC-MAT-N pattern in existing file. Q3 alternatives? flat TC-N numbering rejected (harder to trace to functional area). Salience: low.
- Every UC scenario (UC-C2C-X, UC-C2C-X-A, UC-C2C-X-E1, UC-C2C-X-EC1) gets at least one test case. Total 18 UCs × their sub-scenarios = 52 test cases. Q1 hack? no. Q2 sane? yes. Q3 alternatives? merging alt-flows into primary rows rejected (loses individual evidence traceability). Salience: high.
- AC-C2C-1..AC-C2C-5 each get a dedicated acceptance-gate TC in Section 10. These are separate from the per-UC TCs that cover the same flows — the AC TCs are end-to-end integration evidence (two real CC instances), the per-UC TCs are unit/integration level. Q1 hack? no. Q2 sane? yes. Salience: high.
- Architect-mandated special TCs (FR-C2C-4.6 identity binding, FR-C2C-5.5 rate-limit drain, i64::MAX sentinel, Slice 7 pre-flight, v5→v6 migration idempotency) are placed in the most relevant functional-area section AND cross-referenced in the UC coverage map. Salience: high.
- SQL evidence uses `sqlite3 <db_path> "<query>"` pattern consistent with the multi-agent-telegram file's convention. DB path for `chat.db` is `~/.claude/knowledge/chat.db` per §17 OQ-ACD-4 resolution (use-cases inherited convention). Salience: medium.

### Hacks / workarounds acknowledged

- (none)

### Symptom-only patches (with root-cause links)

- (none)

---

## Conventions

- **`Verification Class`** — one of `CLI`, `DB`, `FS`, `API`, `Mixed`. No `UI/UX` cases — this is a daemon/MCP/CLI feature with no browser surface. Mixed = two or more classes; QA engineer must verify ALL classes listed in the evidence.
- **`Evidence Required`** — concrete artifact names. Vague entries ("works correctly", "behaves as expected", "no errors") are auto-FAIL at qa-engineer execution time.
- **SQL command** — `sqlite3 ~/.claude/knowledge/chat.db "<query>"` for `chat.db`. Log tail via `claudebase daemon logs --tail 50`.
- **Evidence storage** — all evidence files live under `docs/qa/evidence/cli-to-cli-routing/`. QA engineer creates the directory at first run.
- **Log literal matching** — expected log substrings are planning-time projections. QA engineer accepts substring matches. If wording diverges, implementer MUST update use-cases + test-cases file.
- **`agent_id` placeholders** — `<A_id>` = agent A's registered `agent_id`; `<B_id>` = agent B's registered `agent_id`. QA engineer substitutes concrete values at runtime.
- **DB path** — `chat.db` lives at `~/.claude/knowledge/chat.db` per §17 OQ-ACD-4 resolution (user-level, unchanged by this feature).
- **`cargo test` commands** — unit-test names are suggestions; QA engineer accepts any test function covering the stated assertion.

---

## Use Case Coverage Map

| Use Case | Test Case(s) |
|----------|--------------|
| UC-C2C-1 (list-alive primary) | TC-C2C-1.1 |
| UC-C2C-1-A (`--project all`) | TC-C2C-1.2 |
| UC-C2C-1-B (literal slug) | TC-C2C-1.3 |
| UC-C2C-1-E1 (no alive agents) | TC-C2C-1.4 |
| UC-C2C-1-E2 (daemon not running) | TC-C2C-1.5 |
| UC-C2C-1-EC1 (DND agent still listed) | TC-C2C-1.6 |
| UC-C2C-1-EC2 (stale agent filtered) | TC-C2C-1.7 |
| UC-C2C-1-EC3 (legacy cwd=NULL rows) | TC-C2C-1.8 |
| UC-C2C-2 (agent_describe primary) | TC-C2C-2.1 |
| UC-C2C-2-A (idempotent re-describe) | TC-C2C-2.2 |
| UC-C2C-2-B (empty description) | TC-C2C-2.3 |
| UC-C2C-2-E1 (agent_id not found) | TC-C2C-2.4 |
| UC-C2C-2-EC1 (concurrent describe race) | TC-C2C-2.5 |
| UC-C2C-3 (agent_send primary) | TC-C2C-3.1 |
| UC-C2C-3-A (urgent flag no-op) | TC-C2C-3.2 |
| UC-C2C-3-B (self-send) | TC-C2C-3.3 |
| UC-C2C-3-E1 (to_agent_id not found) | TC-C2C-3.4 |
| UC-C2C-3-E2 (spoofed from ignored) | TC-C2C-3.5 (FR-C2C-4.6 identity binding) |
| UC-C2C-3-EC1 (subscription dropped mid-flight) | TC-C2C-3.6 |
| UC-C2C-4 (DND primary) | TC-C2C-4.1 |
| UC-C2C-4-A (explicit "off" before expiry) | TC-C2C-4.2 |
| UC-C2C-4-B (multi-message queue) | TC-C2C-4.3 |
| UC-C2C-4-C (indefinite DND i64::MAX) | TC-C2C-4.4 |
| UC-C2C-4-E1 (invalid state string) | TC-C2C-4.5 |
| UC-C2C-4-EC1 (DND expired during daemon restart) | TC-C2C-4.6 |
| UC-C2C-4-EC3 (rate-limit drain schedule) | TC-C2C-4.7 (FR-C2C-5.5) |
| UC-C2C-5 (hook primary) | TC-C2C-5.1 |
| UC-C2C-5-A (no plan.md) | TC-C2C-5.2 |
| UC-C2C-5-B (installer idempotency) | TC-C2C-5.3 |
| UC-C2C-5-C (fallback hook mode) | TC-C2C-5.4 |
| UC-C2C-5-E1 (agent_describe MCP fail) | TC-C2C-5.5 |
| UC-C2C-5-EC1 (multiple ExitPlanMode in same session) | TC-C2C-5.6 |
| UC-C2C-5-EC2 (long heading) | TC-C2C-5.7 |
| UC-C2C-6 (no-git fallback primary) | TC-C2C-6.1 |
| UC-C2C-6-A (config.json override) | TC-C2C-6.2 |
| UC-C2C-6-B (config.json empty field) | TC-C2C-6.3 |
| UC-C2C-6-E1 (cwd deleted) | TC-C2C-6.4 |
| UC-C2C-6-EC1 (symlink resolution) | TC-C2C-6.5 |
| UC-C2C-7 (last-write-wins collision) | TC-C2C-7.1 |
| UC-C2C-7-A (unique agent IDs) | TC-C2C-7.2 |
| UC-C2C-7-EC1 (describe after overwrite) | TC-C2C-7.3 |
| UC-C2C-8 (git worktree same project_id) | TC-C2C-8.1 |
| UC-C2C-8-EC1 (fork different project_id) | TC-C2C-8.2 |
| UC-C2C-9 (send to ghost agent) | TC-C2C-9.1 |
| UC-C2C-9-EC1 (agent dies mid-compose) | TC-C2C-9.2 |
| UC-C2C-10 (auto-subscribe failure) | TC-C2C-10.1 |
| UC-C2C-10-E1 (daemon unreachable) | TC-C2C-10.2 |
| UC-C2C-11 (DND drain DB error) | TC-C2C-11.1 |
| UC-C2C-11-EC1 (repeated DB errors) | TC-C2C-11.2 |
| UC-C2C-12 (SSH/HTTPS URL normalization) | TC-C2C-12.1 |
| UC-C2C-12-EC1 (mixed-case URL) | TC-C2C-12.2 |
| UC-C2C-13 (no origin remote) | TC-C2C-13.1 |
| UC-C2C-13-A (non-origin remote) | TC-C2C-13.2 |
| UC-C2C-14 (until HH:MM parser) | TC-C2C-14.1 |
| UC-C2C-14-EC1 (midnight rollover) | TC-C2C-14.2 |
| UC-C2C-14-EC2 (daemon timezone ≠ operator timezone) | TC-C2C-14.3 |
| UC-C2C-14-EC3 (DST boundary) | TC-C2C-14.4 |
| UC-C2C-15 (Telegram regression) | TC-C2C-15.1 |
| UC-C2C-15-EC1 (unknown meta.kind fallthrough) | TC-C2C-15.2 |
| UC-C2C-16 (register-time identity capture) | TC-C2C-16.1 |
| UC-C2C-16-E1 (detached HEAD) | TC-C2C-16.2 |
| UC-C2C-17 (schema migration idempotency) | TC-C2C-17.1 |
| UC-C2C-17-A (second-run idempotency) | TC-C2C-17.2 |
| UC-C2C-18 (agent-message tag attributes) | TC-C2C-18.1 |
| UC-C2C-18-EC1 (ts absent) | TC-C2C-18.2 |
| AC-C2C-1 (acceptance gate) | TC-C2C-AC1 |
| AC-C2C-2 (acceptance gate) | TC-C2C-AC2 |
| AC-C2C-3 (acceptance gate) | TC-C2C-AC3 |
| AC-C2C-4 (acceptance gate) | TC-C2C-AC4 |
| AC-C2C-5 (acceptance gate) | TC-C2C-AC5 |

**Total: 52 use-case test cases + 5 acceptance-gate test cases = 57 test cases.**

---

## 1. `claudebase agent list-alive` — CLI Subcommand (FR-C2C-6)

### 1.1 Primary and Alternative Flows

| # | Use Case | Verification Class | Test Case | Expected Result | Evidence Required |
|---|----------|--------------------|-----------|-----------------|-------------------|
| TC-C2C-1.1 | UC-C2C-1 | Mixed (CLI + DB) | Two CC sessions alive in same project (same git origin URL). Run `claudebase agent list-alive --project current --json` from clone A. | stdout is a JSON array containing exactly 2 objects; each object has `agent_id`, `branch`, `working_dir`, `feature_description` (nullable), `last_seen_at`, `dnd_until_ts` (nullable); both objects have `project_id = "github.com/codefather-labs/claudebase"`; no agents from unrelated projects appear; exit code 0. | (a) text file `docs/qa/evidence/cli-to-cli-routing/tc-1.1-list-alive-json.txt` capturing stdout — MUST be valid JSON array with exactly 2 elements, each containing all 6 required fields; (b) `sqlite3 ~/.claude/knowledge/chat.db "SELECT agent_id, project_id, branch, working_dir FROM agent_registry WHERE state='alive' AND project_id IS NOT NULL"` output saved in `tc-1.1-db-rows.txt` — MUST match the 2 agents in stdout; `cargo test --test cli_agent_list_alive_test -- project_current_returns_only_same_project_agents` exits 0. |
| TC-C2C-1.2 | UC-C2C-1-A | Mixed (CLI + DB) | Run `claudebase agent list-alive --project all --json` with 2 agents in project A and 1 agent in an unrelated project B registered. | stdout JSON array contains all 3 agents across both projects; rows with `project_id IS NULL` (legacy) also included; exit code 0. | text file `docs/qa/evidence/cli-to-cli-routing/tc-1.2-list-alive-all.txt` capturing stdout — MUST contain at least 3 elements including agents from both project_id values; `sqlite3 ... "SELECT COUNT(*) FROM agent_registry WHERE state='alive'"` output in `tc-1.2-total-count.txt` — count must match the JSON array length. |
| TC-C2C-1.3 | UC-C2C-1-B | Mixed (CLI + DB) | Run `claudebase agent list-alive --project github.com/codefather-labs/claudebase --json` (literal slug) from a cwd that is NOT a git repo. | Result identical to TC-C2C-1.1 — same 2 agents scoped to the given slug; literal slug used directly without git resolution; exit code 0. | text file `docs/qa/evidence/cli-to-cli-routing/tc-1.3-list-alive-slug.txt` capturing stdout — MUST be a JSON array with the same agent_id values as TC-C2C-1.1. Confirm cwd has no `.git` dir: `dir` / `ls` output showing absence of `.git` directory saved in `tc-1.3-cwd-check.txt`. |

### 1.2 Error Flows

| # | Use Case | Verification Class | Test Case | Expected Result | Evidence Required |
|---|----------|--------------------|-----------|-----------------|-------------------|
| TC-C2C-1.4 | UC-C2C-1-E1 | CLI | Run `claudebase agent list-alive --project current --json` when no CC sessions are alive in this project (all `state = 'dead'` or `state = 'orphaned'`). | stdout is `[]`; exit code 0 (not an error state per UC-C2C-1-E1). | text file `docs/qa/evidence/cli-to-cli-routing/tc-1.4-empty-list.txt` containing the literal `[]` as the entire stdout. |
| TC-C2C-1.5 | UC-C2C-1-E2 | CLI | Run `claudebase agent list-alive --project current` when the daemon is NOT running (socket file absent or no process). | stderr contains `{"error": "daemon not running"}` (or equivalent structured error); exit code 1. | text file `docs/qa/evidence/cli-to-cli-routing/tc-1.5-daemon-down.txt` capturing stderr — MUST contain `"daemon not running"` substring; text file `tc-1.5-exit-code.txt` recording exit code value `1`. |

### 1.3 Edge Cases

| # | Use Case | Verification Class | Test Case | Expected Result | Evidence Required |
|---|----------|--------------------|-----------|-----------------|-------------------|
| TC-C2C-1.6 | UC-C2C-1-EC1 | Mixed (CLI + DB) | Agent B has `dnd_until_ts` set to a future timestamp (DND active). Run `claudebase agent list-alive --project current --json`. | Agent B still appears in the JSON array; `dnd_until_ts` field shows the future timestamp (non-NULL); DND does NOT filter the agent from list-alive output. | text file `docs/qa/evidence/cli-to-cli-routing/tc-1.6-dnd-agent-visible.txt` capturing stdout — MUST contain agent B's row with a non-null, non-zero `dnd_until_ts` value; `sqlite3 ... "SELECT agent_id, dnd_until_ts FROM agent_registry WHERE agent_id=?"` output in `tc-1.6-db-dnd.txt` confirming the timestamp matches. |
| TC-C2C-1.7 | UC-C2C-1-EC2 | Mixed (CLI + DB) | Agent with `state='orphaned'` (or `last_pinged_at` more than 30s ago) exists in `agent_registry`. Run `claudebase agent list-alive --project current --json`. | The stale/orphaned agent does NOT appear in the output; only `state='alive'` agents with recent `last_pinged_at` appear. | text file `docs/qa/evidence/cli-to-cli-routing/tc-1.7-stale-filtered.txt` capturing stdout — MUST NOT contain the orphaned agent's `agent_id`; `sqlite3 ... "SELECT agent_id, state FROM agent_registry WHERE state != 'alive'"` in `tc-1.7-orphaned-row.txt` confirming the orphaned row exists in DB but is absent from stdout. |
| TC-C2C-1.8 | UC-C2C-1-EC3 | Mixed (CLI + DB) | A legacy `agent_registry` row with `project_id IS NULL` and `cwd IS NULL` (predating C2C migration) exists. Run (a) `list-alive --project current --json` and (b) `list-alive --project all`. | (a) Legacy row does NOT appear in `--project current` output; (b) Legacy row DOES appear in `--project all` output with `project_id` shown as `null` (or displayed as `—` / `(legacy)` in human-readable form). No error in either case. | text file `docs/qa/evidence/cli-to-cli-routing/tc-1.8-current-no-legacy.txt` capturing `--project current` stdout — MUST NOT contain the legacy agent_id; text file `tc-1.8-all-with-legacy.txt` capturing `--project all` stdout — MUST contain the legacy agent_id with `project_id: null`; `sqlite3 ... "SELECT agent_id, project_id FROM agent_registry WHERE cwd IS NULL"` in `tc-1.8-legacy-db.txt` confirming the row exists with `project_id IS NULL`. |

---

## 2. `agent_describe` MCP Tool — Feature Description Publishing (FR-C2C-3)

### 2.1 Primary and Alternative Flows

| # | Use Case | Verification Class | Test Case | Expected Result | Evidence Required |
|---|----------|--------------------|-----------|-----------------|-------------------|
| TC-C2C-2.1 | UC-C2C-2 | Mixed (API + DB) | Agent A calls MCP tool `agent_describe({ feature_id: "cli-to-cli-routing", branch: "feat/multi-agent-on-v0.6", description: "Wiring agent-to-agent comms via daemon" })`. | (a) MCP tool response `{ "ok": true }` received by CC #1 within 1s; (b) `agent_registry` row for agent A has `feature_description = "Wiring agent-to-agent comms via daemon"` and `branch = "feat/multi-agent-on-v0.6"`; (c) `claudebase agent list-alive --project current --json` within 5s shows agent A with the updated `feature_description`. | (a) transcript excerpt in `docs/qa/evidence/cli-to-cli-routing/tc-2.1-agent-describe-response.txt` showing MCP tool response containing literal `"ok":true`; (b) `sqlite3 ~/.claude/knowledge/chat.db "SELECT feature_description, branch FROM agent_registry WHERE agent_id=?"` output in `tc-2.1-db-after.txt` — MUST show `feature_description = "Wiring agent-to-agent comms via daemon"` and `branch = "feat/multi-agent-on-v0.6"`; (c) text file `tc-2.1-list-alive-after.txt` capturing `list-alive --json` stdout — MUST contain agent A's row with the updated `feature_description`; `cargo test --test agent_describe_test -- describe_roundtrip_updates_feature_description` exits 0. |
| TC-C2C-2.2 | UC-C2C-2-A | Mixed (API + DB) | Agent A calls `agent_describe` a second time with `description: "Updated: schema migration complete"`. | `agent_registry` row for agent A has `feature_description = "Updated: schema migration complete"` (overwritten); previous value is gone; `list-alive` shows only the latest description. | `sqlite3 ... "SELECT feature_description FROM agent_registry WHERE agent_id=?"` in `docs/qa/evidence/cli-to-cli-routing/tc-2.2-db-after.txt` — MUST show only `"Updated: schema migration complete"`. |
| TC-C2C-2.3 | UC-C2C-2-B | Mixed (API + DB) | Agent A calls `agent_describe` with `description: ""` (empty string). | MCP tool returns `{ "ok": true }`; `agent_registry` row has `feature_description = ""` (empty string, not NULL); `list-alive` shows `feature_description: ""`. No error. | `sqlite3 ... "SELECT feature_description FROM agent_registry WHERE agent_id=?"` in `docs/qa/evidence/cli-to-cli-routing/tc-2.3-empty-description.txt` — MUST show empty string `""` (NOT NULL). |

### 2.2 Error Flows

| # | Use Case | Verification Class | Test Case | Expected Result | Evidence Required |
|---|----------|--------------------|-----------|-----------------|-------------------|
| TC-C2C-2.4 | UC-C2C-2-E1 | API | `agent_describe` is called by an agent whose `agent_id` has been removed from `agent_registry` between bridge connect and the MCP call (simulate by deleting the row). | MCP tool returns a structured error `{ "error": "agent not found" }` (or equivalent); no DB write occurs. CC #1 receives an MCP tool error response. | transcript excerpt in `docs/qa/evidence/cli-to-cli-routing/tc-2.4-not-found-response.txt` showing MCP tool error containing `"agent not found"` substring; `sqlite3 ... "SELECT COUNT(*) FROM agent_registry WHERE agent_id=?"` in `tc-2.4-count.txt` returns `0` (confirming row was absent during the call). |

### 2.3 Edge Cases

| # | Use Case | Verification Class | Test Case | Expected Result | Evidence Required |
|---|----------|--------------------|-----------|-----------------|-------------------|
| TC-C2C-2.5 | UC-C2C-2-EC1 | DB | Two CC sessions registered under the same `agent_id` (R-C2C-3 race). Both call `agent_describe` with different descriptions within 100ms of each other. | Exactly one description survives in `agent_registry` (last-write-wins via SQLite write serialization); no data corruption; `PRAGMA integrity_check` returns `ok`. | `sqlite3 ... "SELECT feature_description FROM agent_registry WHERE agent_id=?"` in `docs/qa/evidence/cli-to-cli-routing/tc-2.5-last-write-wins.txt` — exactly ONE non-empty value; `sqlite3 ... "PRAGMA integrity_check"` in `tc-2.5-integrity.txt` returning literal `ok`. |

---

## 3. `agent_send` MCP Tool — Direct Messaging and Identity Binding (FR-C2C-4)

### 3.1 Primary and Alternative Flows

| # | Use Case | Verification Class | Test Case | Expected Result | Evidence Required |
|---|----------|--------------------|-----------|-----------------|-------------------|
| TC-C2C-3.1 | UC-C2C-3 | Mixed (API + DB) | Agent A (CC #1) calls `agent_send({ to_agent_id: "<B_id>", content: "I am about to touch bridge.rs in slice 4 — please hold off on that file." })`. Agent B is alive, DND inactive. | (a) MCP tool returns `{ "delivered": true }` to CC #1 within 2s; (b) `chat_messages` row inserted with `thread_id = 'agent:<B_id>'`, `from_agent = '<A_id>'` (daemon-bound), `delivered_at` non-NULL; (c) `notifications/claude/channel` emitted with `target_agent_id = "<B_id>"` and `meta.kind = "agent-to-agent"`; (d) CC #2 bridge receives and renders `<agent-message from="<A_id>" thread="agent:<B_id>" ts="...">...</agent-message>`; total wall time ≤ 2s. | (a) transcript excerpt `docs/qa/evidence/cli-to-cli-routing/tc-3.1-send-response.txt` containing literal `"delivered":true`; (b) `sqlite3 ... "SELECT from_agent, thread_id, delivered_at FROM chat_messages WHERE thread_id='agent:<B_id>' ORDER BY rowid DESC LIMIT 1"` in `tc-3.1-chat-messages-row.txt` — MUST show `from_agent='<A_id>'`, `thread_id='agent:<B_id>'`, `delivered_at` non-NULL integer; (c) CC #2 transcript excerpt in `tc-3.1-cc2-agent-message.txt` containing the literal substring `<agent-message from="<A_id>"` AND `thread="agent:<B_id>"`; `cargo test --test agent_send_test -- send_delivers_notification_when_dnd_inactive` exits 0. |
| TC-C2C-3.2 | UC-C2C-3-A | API | Agent A calls `agent_send({ to_agent_id: "<B_id>", content: "test", urgent: true })`. DND inactive. | MCP tool returns `{ "delivered": true }`; `urgent: true` flag is accepted without error and silently ignored (no behavior change vs. TC-C2C-3.1 in MVP); message delivered normally. | transcript excerpt `docs/qa/evidence/cli-to-cli-routing/tc-3.2-urgent-noop.txt` containing `"delivered":true`; `sqlite3 ... "SELECT COUNT(*) FROM chat_messages WHERE thread_id='agent:<B_id>' AND delivered_at IS NOT NULL"` in `tc-3.2-delivered-count.txt` returning count incremented by 1. |
| TC-C2C-3.3 | UC-C2C-3-B | Mixed (API + DB) | Agent A calls `agent_send({ to_agent_id: "<A_id>", content: "self-test" })` (sends to itself). | MCP returns `{ "delivered": true }`; `chat_messages` row inserted with `thread_id = 'agent:<A_id>'`, `from_agent = '<A_id>'`; `notifications/claude/channel` emitted with `target_agent_id = "<A_id>"`; CC #1's own bridge receives and renders `<agent-message from="<A_id>" ...>self-test</agent-message>`. | `sqlite3 ... "SELECT from_agent, thread_id, delivered_at FROM chat_messages WHERE thread_id='agent:<A_id>' ORDER BY rowid DESC LIMIT 1"` in `docs/qa/evidence/cli-to-cli-routing/tc-3.3-self-send.txt` — `from_agent = '<A_id>'`, `thread_id = 'agent:<A_id>'`, `delivered_at` non-NULL. |

### 3.2 Security — Sender Identity Binding (FR-C2C-4.6 — Architect-Mandated TC)

| # | Use Case | Verification Class | Test Case | Expected Result | Evidence Required |
|---|----------|--------------------|-----------|-----------------|-------------------|
| TC-C2C-3.5 | UC-C2C-3-E2 | Mixed (API + DB) | A caller sends `agent_send` with an explicit `from: "spoofed-agent"` field in the request body. The caller's actual `connection_id` maps to agent A in `agent_registry`. Agent B is the recipient, DND inactive. | (a) MCP call succeeds with `{ "delivered": true }` — no error for the unexpected `from` field; (b) `chat_messages` row has `from_agent = '<A_id>'` (daemon-bound), NOT `"spoofed-agent"`; (c) CC #2's `<agent-message>` tag shows `from="<A_id>"`, NOT `from="spoofed-agent"`; daemon log contains substring `sender identity rebound` (or equivalent, indicating FR-C2C-4.6 enforcement). | (a) `sqlite3 ... "SELECT from_agent FROM chat_messages WHERE thread_id='agent:<B_id>' ORDER BY rowid DESC LIMIT 1"` in `docs/qa/evidence/cli-to-cli-routing/tc-3.5-identity-binding.txt` — MUST show `from_agent = '<A_id>'` (NOT `"spoofed-agent"`); (b) CC #2 transcript excerpt in `tc-3.5-cc2-from-attribute.txt` — `<agent-message>` tag MUST have `from="<A_id>"` and MUST NOT contain `spoofed-agent`; (c) `claudebase daemon logs --tail 50` in `tc-3.5-daemon-log.txt` — MUST contain a log line indicating sender identity was resolved from connection state (exact wording TBD by implementer, but substring `identity` or `connection_id` MUST appear in the relevant log line); `cargo test --test agent_send_test -- sender_identity_bound_from_connection_not_from_request` exits 0. |

### 3.3 Error Flows

| # | Use Case | Verification Class | Test Case | Expected Result | Evidence Required |
|---|----------|--------------------|-----------|-----------------|-------------------|
| TC-C2C-3.4 | UC-C2C-3-E1 | API | Agent A calls `agent_send({ to_agent_id: "ghost-agent", content: "hello?" })`. No agent with `agent_id = "ghost-agent"` exists in `agent_registry`. | MCP tool returns structured error `{ "error": "agent not found", "agent_id": "ghost-agent" }`; no `chat_messages` row inserted; no notification emitted; exit 1 (error response). | transcript excerpt `docs/qa/evidence/cli-to-cli-routing/tc-3.4-not-found.txt` containing `"agent not found"` and `"ghost-agent"` substrings; `sqlite3 ... "SELECT COUNT(*) FROM chat_messages WHERE thread_id='agent:ghost-agent'"` in `tc-3.4-no-chat-row.txt` returning `0`; `cargo test --test agent_send_test -- send_to_unknown_agent_fails_loudly` exits 0. |

### 3.4 Edge Cases

| # | Use Case | Verification Class | Test Case | Expected Result | Evidence Required |
|---|----------|--------------------|-----------|-----------------|-------------------|
| TC-C2C-3.6 | UC-C2C-3-EC1 | DB | CC #2's bridge subscription to `agent:<B_id>` is dropped after auto-subscribe (simulate by calling `chat_unsubscribe` or restarting CC #2's bridge mid-test). Agent A then calls `agent_send`. | `chat_messages` row IS persisted with `thread_id = 'agent:<B_id>'` and `delivered_at` set (daemon set it after emit); CC #2 does NOT see the message in real time (no live notification); message IS retrievable via `chat_list --thread agent:<B_id>`. | `sqlite3 ... "SELECT thread_id, delivered_at FROM chat_messages WHERE thread_id='agent:<B_id>' ORDER BY rowid DESC LIMIT 1"` in `docs/qa/evidence/cli-to-cli-routing/tc-3.6-persisted-no-delivery.txt` — `thread_id = 'agent:<B_id>'`, `delivered_at` non-NULL (message persisted); CC #2 transcript in `tc-3.6-cc2-transcript.txt` shows NO `<agent-message>` block after the send (subscription was dropped). |

---

## 4. DND State Machine — `agent_set_dnd` + Background Drain (FR-C2C-5)

### 4.1 Primary and Alternative Flows

| # | Use Case | Verification Class | Test Case | Expected Result | Evidence Required |
|---|----------|--------------------|-----------|-----------------|-------------------|
| TC-C2C-4.1 | UC-C2C-4 | Mixed (API + DB) | Agent B calls `agent_set_dnd({ state: "30m" })`; agent A calls `agent_send` while DND active; DND expires (or `agent_set_dnd("off")` called); drain delivers the message. | (a) `agent_set_dnd("30m")` returns `{ "ok": true, "dnd_until": "<ISO-8601 ts 30m from now>" }` within 2s; (b) `chat_messages` row inserted with `delivered_at = NULL` during DND window; (c) `agent_send` returns `{ "queued": true, "delivered_when": "<same ISO-8601>" }` within 2s; (d) no `notifications/claude/channel` emitted during DND window (CC #2 transcript silent); (e) after DND expires/off, drain fires within 30s: `dnd_until_ts = NULL` in DB, `delivered_at` non-NULL, CC #2 receives `<agent-message>`. | (a) transcript `docs/qa/evidence/cli-to-cli-routing/tc-4.1-dnd-set.txt` containing `"ok":true` and ISO-8601 timestamp; (b) `sqlite3 ... "SELECT delivered_at FROM chat_messages WHERE thread_id='agent:<B_id>' ORDER BY rowid DESC LIMIT 1"` immediately after send in `tc-4.1-queued.txt` — MUST show `delivered_at = NULL`; (c) transcript `tc-4.1-send-queued.txt` containing `"queued":true` and `"delivered_when"` substring; (d) CC #2 transcript excerpt `tc-4.1-cc2-silent.txt` covering the DND window showing NO `<agent-message>` block; (e) after drain: `sqlite3 ... "SELECT dnd_until_ts, delivered_at FROM agent_registry JOIN chat_messages ..."` in `tc-4.1-after-drain.txt` showing `dnd_until_ts = NULL` and `delivered_at` non-NULL; CC #2 transcript `tc-4.1-cc2-after-drain.txt` containing `<agent-message>` block. |
| TC-C2C-4.2 | UC-C2C-4-A | Mixed (API + DB) | Agent B calls `agent_set_dnd("30m")`; then before 30m elapses, calls `agent_set_dnd("off")`. Agent A had queued a message during the DND window. | `agent_set_dnd("off")` sets `dnd_until_ts = NULL` immediately; drain task fires within 30s of the "off" call; queued message delivered to CC #2. | `sqlite3 ... "SELECT dnd_until_ts FROM agent_registry WHERE agent_id='<B_id>'"` immediately after "off" call in `docs/qa/evidence/cli-to-cli-routing/tc-4.2-dnd-cleared.txt` — MUST return `NULL`; CC #2 transcript `tc-4.2-cc2-drained.txt` showing `<agent-message>` received within 30s of "off" call. |
| TC-C2C-4.3 | UC-C2C-4-B | Mixed (API + DB) | Agent B in DND; agent A sends 5 messages. DND turns off. | All 5 messages queued (`delivered_at IS NULL`); on drain, all 5 delivered in the first tick (count ≤ 10, within rate limit); each has `delivered_at` set to drain emission time (not original `created_at`); CC #2 receives all 5 `<agent-message>` blocks. | `sqlite3 ... "SELECT COUNT(*) FROM chat_messages WHERE thread_id='agent:<B_id>' AND delivered_at IS NULL"` immediately before drain in `docs/qa/evidence/cli-to-cli-routing/tc-4.3-pre-drain-count.txt` returning `5`; after drain: same query in `tc-4.3-post-drain-count.txt` returning `0`; CC #2 transcript `tc-4.3-cc2-all-drained.txt` containing 5 `<agent-message>` blocks. |
| TC-C2C-4.4 | UC-C2C-4-C | DB | Agent B calls `agent_set_dnd("on")` (indefinite). | `agent_registry.dnd_until_ts = 9223372036854775807` (i64::MAX); drain task NEVER fires for this agent (drain's `WHERE dnd_until_ts < now()` always false for i64::MAX); only explicit `agent_set_dnd("off")` clears it. | `sqlite3 ... "SELECT dnd_until_ts FROM agent_registry WHERE agent_id='<B_id>'"` in `docs/qa/evidence/cli-to-cli-routing/tc-4.4-indefinite-dnd.txt` — MUST return exactly `9223372036854775807`; wait ≥ 35s and confirm drain has NOT delivered any queued messages: `sqlite3 ... "SELECT COUNT(*) FROM chat_messages WHERE thread_id='agent:<B_id>' AND delivered_at IS NULL"` in `tc-4.4-no-drain.txt` — count UNCHANGED; `cargo test --test agent_dnd_test -- indefinite_dnd_uses_i64_max_not_null` exits 0. |

### 4.2 DND Drain Rate Limit (FR-C2C-5.5 — Architect-Mandated TC)

| # | Use Case | Verification Class | Test Case | Expected Result | Evidence Required |
|---|----------|--------------------|-----------|-----------------|-------------------|
| TC-C2C-4.7 | UC-C2C-4-EC3 | Mixed (DB + CLI) | Agent B has 30 messages queued (`delivered_at IS NULL`) in `chat_messages` for `thread_id = 'agent:<B_id>'`. DND turns off. Monitor drain over 3 consecutive 30s ticks. | Tick 1 (0–30s after DND off): exactly 10 messages drained (oldest 10 by `created_at ASC`), `delivered_at` set on those 10, remaining 20 still NULL. Tick 2 (30–60s): 10 more drained. Tick 3 (60–90s): final 10 drained. No messages dropped — all 30 eventually delivered. | (a) after Tick 1: `sqlite3 ... "SELECT COUNT(*) FROM chat_messages WHERE thread_id='agent:<B_id>' AND delivered_at IS NOT NULL"` in `docs/qa/evidence/cli-to-cli-routing/tc-4.7-tick1.txt` returning `10`; (b) after Tick 2: same query in `tc-4.7-tick2.txt` returning `20`; (c) after Tick 3: same query in `tc-4.7-tick3.txt` returning `30`; (d) daemon logs in `tc-4.7-drain-logs.txt` must contain 3 separate drain log lines each with `count=10` (or equivalent) spaced ≥30s apart by timestamp; (e) `cargo test --test agent_dnd_test -- drain_rate_limit_10_per_tick` exits 0. |

### 4.3 Error Flows and Edge Cases

| # | Use Case | Verification Class | Test Case | Expected Result | Evidence Required |
|---|----------|--------------------|-----------|-----------------|-------------------|
| TC-C2C-4.5 | UC-C2C-4-E1 | API | Agent B calls `agent_set_dnd({ state: "5d" })` (days — not in the accepted value set). | MCP tool returns structured error `{ "error": "invalid state", "state": "5d", "accepted_values": ["on", "off", "<N>m", "<N>h", "until HH:MM"] }` (or subset); no DB write; exit 1 (error response). | transcript `docs/qa/evidence/cli-to-cli-routing/tc-4.5-invalid-state.txt` containing `"invalid state"` and `"5d"` substrings; `sqlite3 ... "SELECT dnd_until_ts FROM agent_registry WHERE agent_id='<B_id>'"` in `tc-4.5-db-unchanged.txt` — value unchanged from pre-call state. |
| TC-C2C-4.6 | UC-C2C-4-EC1 | Mixed (CLI + DB) | DND expires while daemon is restarting (drain task not running). After daemon restarts, drain task runs its first tick. | Queued messages delivered within 30s of daemon restart (first tick after restart finds `dnd_until_ts < now()` and drains). DB state after restart: `dnd_until_ts = NULL`, `delivered_at` non-NULL on all previously-queued messages. | `sqlite3 ... "SELECT dnd_until_ts, COUNT(*) FROM agent_registry JOIN chat_messages ... WHERE thread_id='agent:<B_id>' AND delivered_at IS NULL"` immediately after restart in `docs/qa/evidence/cli-to-cli-routing/tc-4.6-post-restart.txt` — within 30s both should flip; daemon logs `tc-4.6-restart-drain-log.txt` containing drain log entry post-startup. |

---

## 5. `PostToolUse:ExitPlanMode` Hook (FR-C2C-7) — Slice 7

### 5.1 Primary and Alternative Flows

| # | Use Case | Verification Class | Test Case | Expected Result | Evidence Required |
|---|----------|--------------------|-----------|-----------------|-------------------|
| TC-C2C-5.1 | UC-C2C-5 | Mixed (FS + CLI + DB) | Operator exits plan mode in CC #1 (ExitPlanMode fires). `.claude/plan.md` exists with heading `# Plan: claudebase cli-to-cli-routing — agent-to-agent communication via daemon`. Agent A is alive and registered. | (a) Hook script executes without error; (b) `additionalContext` injected mandating both writes; (c) agent A calls `agent_describe` in the same turn — daemon row updated with non-NULL `feature_description`; (d) `.claude/scratchpad.md` `## Feature:` line updated to match daemon's `feature_description`. Both writes happen in the same agent turn. | (a) hook output captured in `docs/qa/evidence/cli-to-cli-routing/tc-5.1-hook-stdout.txt` — no error lines, exit 0; (b) session transcript excerpt `tc-5.1-transcript.txt` showing both `agent_describe` MCP call AND scratchpad edit in the same agent turn; (c) `sqlite3 ... "SELECT feature_description FROM agent_registry WHERE agent_id='<A_id>'"` in `tc-5.1-db-feature.txt` — non-NULL value matching the plan heading; (d) `grep "## Feature:" .claude/scratchpad.md` stdout in `tc-5.1-scratchpad-feature.txt` — value matches `feature_description` from DB. |
| TC-C2C-5.2 | UC-C2C-5-A | FS | `.claude/plan.md` does not exist (or first line has no `#` heading). ExitPlanMode fires. | Hook emits empty `additionalContext` or a minimal "no plan found" note; no `agent_describe` call is mandated; no crash; agent turn proceeds normally. Hook exits 0. | hook output in `docs/qa/evidence/cli-to-cli-routing/tc-5.2-no-plan-hook.txt` — exit 0; no `agent_describe` call in the session transcript excerpt `tc-5.2-no-call-transcript.txt`; confirm `.claude/plan.md` absence: `dir .claude/plan.md` / `ls .claude/plan.md` output in `tc-5.2-no-plan.txt` showing file not found. |
| TC-C2C-5.3 | UC-C2C-5-B | FS | Re-run installer (`install.sh` or `install.ps1`) when the hook is already wired in `~/.claude/settings.json`. | Installer exits 0; `~/.claude/settings.json` `hooks.PostToolUse` array is UNCHANGED — no new duplicate entry added for the claudebase-feature-describe script. | `~/.claude/settings.json` content captured before and after in `docs/qa/evidence/cli-to-cli-routing/tc-5.3-settings-before.json` and `tc-5.3-settings-after.json` — diff MUST exit 0 (byte-identical); installer exit code captured in `tc-5.3-installer-exit.txt` showing `0`. |
| TC-C2C-5.4 | UC-C2C-5-C | Mixed (FS + CLI) | Slice 7 pre-flight verification result is captured. Implementer ran pre-flight check; fallback hook event selected (if primary PostToolUse+ExitPlanMode unavailable). | Pre-flight log file exists at `docs/qa/evidence/cli-to-cli-routing/slice-7-preflight.txt` documenting: (a) which hook event was verified (Primary or Fallback A/B/C), (b) how it was verified (e.g., grep against settings schema, test hook invocation), (c) the chosen implementation strategy. Observable outcome: `feature_description` is updated in daemon + scratchpad regardless of which hook strategy fires (postconditions identical to TC-C2C-5.1). | text file `docs/qa/evidence/cli-to-cli-routing/slice-7-preflight.txt` MUST exist and contain: (1) one of the strings `Primary: PostToolUse+ExitPlanMode`, `Fallback A: UserPromptSubmit`, `Fallback B: Stop hook`, or `Fallback C: operator-driven`; (2) verification evidence (e.g., settings.json grep output or test invocation result); `cargo test --test agent_describe_test -- hook_writes_feature_description_and_scratchpad_in_same_turn` exits 0. |

### 5.2 Error Flows and Edge Cases

| # | Use Case | Verification Class | Test Case | Expected Result | Evidence Required |
|---|----------|--------------------|-----------|-----------------|-------------------|
| TC-C2C-5.5 | UC-C2C-5-E1 | Mixed (CLI + FS) | `agent_describe` MCP call fails inside the hook-triggered turn (simulate by stopping daemon mid-turn). | Agent A receives MCP tool error; surfaces the failure to the operator in CC #1 turn transcript. Hook itself does NOT retry. `.claude/scratchpad.md` may or may not have been updated. | CC #1 transcript excerpt `docs/qa/evidence/cli-to-cli-routing/tc-5.5-mcp-fail-transcript.txt` containing an MCP error response from `agent_describe`; daemon logs `tc-5.5-daemon-down.txt` confirming daemon was not running at call time. |
| TC-C2C-5.6 | UC-C2C-5-EC1 | Mixed (FS + DB) | ExitPlanMode fires twice in the same session (operator enters and exits plan mode twice). | Hook fires both times; each firing may update `feature_description` with the latest plan title; no cumulative side effect; idempotent. Second description OVERWRITES first in `agent_registry`. | `sqlite3 ... "SELECT feature_description FROM agent_registry WHERE agent_id='<A_id>'"` in `docs/qa/evidence/cli-to-cli-routing/tc-5.6-second-fire.txt` — shows SECOND hook's extracted title (latest value wins); no duplicate rows, no crash; daemon logs `tc-5.6-daemon-logs.txt` contain exactly 2 `agent_describe` call log entries. |
| TC-C2C-5.7 | UC-C2C-5-EC2 | DB | `.claude/plan.md` first heading is 250 characters long. Hook fires. `agent_describe` is called. | Full 250-char string stored in `agent_registry.feature_description` (SQLite TEXT has no length limit per spec); no truncation; `list-alive --json` shows the full string. | `sqlite3 ... "SELECT length(feature_description) FROM agent_registry WHERE agent_id='<A_id>'"` in `docs/qa/evidence/cli-to-cli-routing/tc-5.7-long-description.txt` — MUST return `250`; `list-alive --json` stdout in `tc-5.7-list-alive.txt` contains the full 250-char string without truncation. |

---

## 6. `project_id` Resolver — Fallback Chain (FR-C2C-2)

### 6.1 Primary Flow — No Git, No Config

| # | Use Case | Verification Class | Test Case | Expected Result | Evidence Required |
|---|----------|--------------------|-----------|-----------------|-------------------|
| TC-C2C-6.1 | UC-C2C-6 | Mixed (CLI + DB) | `resolve_project_id` called in a cwd with no `.git` directory and no `.claudebase/config.json`. | Returns `local:[0-9a-f]{16}` (22-char string); two calls from the SAME cwd return the SAME value; `agent_registry` row for agent in this cwd has `project_id = "local:<16-hex-chars>"`; two agents in the same non-git cwd see each other via `list-alive --project current`. | `sqlite3 ... "SELECT project_id FROM agent_registry WHERE working_dir=?"` in `docs/qa/evidence/cli-to-cli-routing/tc-6.1-local-project-id.txt` — value matches pattern `local:[0-9a-f]{16}`; `cargo test --test project_id_test -- no_git_repo_returns_local_hash_prefix` exits 0. |

### 6.2 Alternative Flows

| # | Use Case | Verification Class | Test Case | Expected Result | Evidence Required |
|---|----------|--------------------|-----------|-----------------|-------------------|
| TC-C2C-6.2 | UC-C2C-6-A | Mixed (CLI + FS) | `resolve_project_id` called in a cwd with no `.git` but with `.claudebase/config.json` containing `{ "project_id": "custom-project-slug" }`. | Step 2 succeeds; returns `"custom-project-slug"` (NOT a local hash); two agents in different cwds sharing this config file would see each other. | `sqlite3 ... "SELECT project_id FROM agent_registry WHERE agent_id='<A_id>'"` in `docs/qa/evidence/cli-to-cli-routing/tc-6.2-config-override.txt` — MUST return `"custom-project-slug"`; `cargo test --test project_id_test -- config_json_override_takes_priority_over_hash` exits 0. |
| TC-C2C-6.3 | UC-C2C-6-B | CLI | `.claudebase/config.json` exists but the `project_id` field is absent (or empty string). | Step 2 fails; falls through to Step 3; returns `local:<16-hex-chars>`. | `cargo test --test project_id_test -- config_json_empty_project_id_falls_to_hash` exits 0; `sqlite3 ... "SELECT project_id FROM agent_registry WHERE agent_id='<A_id>'"` in `docs/qa/evidence/cli-to-cli-routing/tc-6.3-empty-config.txt` matching `local:[0-9a-f]{16}`. |

### 6.3 Error Flow and Edge Case

| # | Use Case | Verification Class | Test Case | Expected Result | Evidence Required |
|---|----------|--------------------|-----------|-----------------|-------------------|
| TC-C2C-6.4 | UC-C2C-6-E1 | CLI | `resolve_project_id` called on a cwd path that no longer exists (deleted between call setup and resolution). | Returns a deterministic fallback (e.g., `"local:unknown"`) rather than panicking; caller logs a warning; no crash; `agent_register` succeeds with the fallback `project_id`. | `cargo test --test project_id_test -- deleted_cwd_returns_fallback_not_panic` exits 0 (unit test simulating non-existent path); daemon logs `docs/qa/evidence/cli-to-cli-routing/tc-6.4-deleted-cwd-log.txt` containing a warning substring (NOT a panic backtrace). |
| TC-C2C-6.5 | UC-C2C-6-EC1 | Mixed (CLI + DB) | Two cwds that are symlinks resolving to the same canonical absolute path. `resolve_project_id` called for each. | Both calls return the SAME `local:<16-hex-chars>` value (symlinks resolved before hashing); both agents discover each other via `list-alive --project current`. | `cargo test --test project_id_test -- symlink_cwds_produce_same_project_id` exits 0; `sqlite3 ... "SELECT DISTINCT project_id FROM agent_registry WHERE agent_id IN (?,?)"` in `docs/qa/evidence/cli-to-cli-routing/tc-6.5-symlink-same-id.txt` returning exactly 1 distinct value. |

---

## 7. Agent ID Collision — Last-Write-Wins (FR-C2C-3.1, R-C2C-3)

| # | Use Case | Verification Class | Test Case | Expected Result | Evidence Required |
|---|----------|--------------------|-----------|-----------------|-------------------|
| TC-C2C-7.1 | UC-C2C-7 | DB | CC #1 and CC #2 both launch from the same cwd and both call `agent_register("claudebase", ...)` with their respective `connection_id` values. CC #2 registers AFTER CC #1. | Single row in `agent_registry` for `agent_id = "claudebase"`; row has CC #2's `connection_id` and `last_pinged_at`; CC #1's routing is broken (notifications now go to CC #2's connection); `PRAGMA integrity_check` returns `ok`. | `sqlite3 ... "SELECT agent_id, connection_id, state FROM agent_registry WHERE agent_id='claudebase'"` in `docs/qa/evidence/cli-to-cli-routing/tc-7.1-last-write-wins.txt` — exactly 1 row with CC #2's `connection_id`; `sqlite3 ... "PRAGMA integrity_check"` in `tc-7.1-integrity.txt` returning `ok`. |
| TC-C2C-7.2 | UC-C2C-7-A | DB | Operator forces CC #1 to register as `claudebase-1` and CC #2 as `claudebase-2` (different agent IDs). | Two distinct rows in `agent_registry` — `agent_id = "claudebase-1"` and `agent_id = "claudebase-2"` — both `state = 'alive'`; both visible in `list-alive`; no collision. | `sqlite3 ... "SELECT agent_id, state FROM agent_registry WHERE agent_id IN ('claudebase-1','claudebase-2')"` in `docs/qa/evidence/cli-to-cli-routing/tc-7.2-two-distinct.txt` — 2 rows both with `state = 'alive'`. |
| TC-C2C-7.3 | UC-C2C-7-EC1 | DB | After CC #2 overwrites CC #1's registration (TC-C2C-7.1), CC #1 calls `agent_describe({ feature_id: "x", branch: "main", description: "stale" })`. | `agent_describe` succeeds (it uses the calling agent's `agent_id`, which is `"claudebase"`); `feature_description = "stale"` is written to the row; CC #2 sees this updated description via `list-alive` as if CC #2 had published it. No crash. | `sqlite3 ... "SELECT feature_description FROM agent_registry WHERE agent_id='claudebase'"` in `docs/qa/evidence/cli-to-cli-routing/tc-7.3-describe-after-overwrite.txt` returning `"stale"`. |

---

## 8. Git Worktree — Same `project_id` (FR-C2C-2.2, FR-C2C-6.4, FR-C2C-6.5)

| # | Use Case | Verification Class | Test Case | Expected Result | Evidence Required |
|---|----------|--------------------|-----------|-----------------|-------------------|
| TC-C2C-8.1 | UC-C2C-8 | Mixed (CLI + DB) | CC #1 in main clone (`branch = main`); CC #2 in git worktree (`branch = feat/multi-agent-on-v0.6`). Both share the same `remote.origin.url`. Both register. `list-alive --project current` from either cwd. | Both agents returned in `list-alive`; both have the same `project_id = "github.com/codefather-labs/claudebase"`; `working_dir` and `branch` columns differentiate the two rows. | `sqlite3 ... "SELECT agent_id, project_id, working_dir, branch FROM agent_registry WHERE project_id='github.com/codefather-labs/claudebase' AND state='alive'"` in `docs/qa/evidence/cli-to-cli-routing/tc-8.1-worktree-same-project.txt` — 2 rows with same `project_id` but different `working_dir` and `branch`; `cargo test --test project_id_test -- git_worktree_same_project_id_different_branch` exits 0. |
| TC-C2C-8.2 | UC-C2C-8-EC1 | DB | CC #1 in original repo (origin = `github.com/owner/claudebase`); CC #2 in a fork (origin = `github.com/fork-user/claudebase`). Both register. `list-alive --project current` from CC #1's cwd. | CC #2 (fork) does NOT appear in CC #1's `list-alive --project current` (different `project_id`); the two agents are isolated. | `sqlite3 ... "SELECT agent_id, project_id FROM agent_registry WHERE state='alive'"` in `docs/qa/evidence/cli-to-cli-routing/tc-8.2-fork-isolated.txt` — 2 rows with DIFFERENT `project_id` values; `list-alive --project current` stdout in `tc-8.2-list-current.txt` contains only CC #1's agent. |

---

## 9. `agent_send` to Non-Existent Agent (FR-C2C-4.2) and Bridge Auto-Subscribe (FR-C2C-4.4)

| # | Use Case | Verification Class | Test Case | Expected Result | Evidence Required |
|---|----------|--------------------|-----------|-----------------|-------------------|
| TC-C2C-9.1 | UC-C2C-9 | API | Agent A calls `agent_send({ to_agent_id: "ghost-agent", content: "hello?" })`. No agent with `agent_id = "ghost-agent"` in registry. | Returns `{ "error": "agent not found", "agent_id": "ghost-agent" }`; no `chat_messages` row inserted; no notification; daemon log records the failed lookup. | transcript `docs/qa/evidence/cli-to-cli-routing/tc-9.1-ghost-agent.txt` containing `"agent not found"` and `"ghost-agent"`; `sqlite3 ... "SELECT COUNT(*) FROM chat_messages WHERE thread_id='agent:ghost-agent'"` in `tc-9.1-no-row.txt` returning `0`; daemon logs `tc-9.1-daemon-log.txt` containing a lookup-failure log line. |
| TC-C2C-9.2 | UC-C2C-9-EC1 | Mixed (API + DB) | Agent B alive when A composed the message; B deregisters (or daemon prunes it) in the milliseconds before `agent_send` fires. | Same error flow as TC-C2C-9.1 — `{ "error": "agent not found" }`; message not persisted; no notification. | transcript `docs/qa/evidence/cli-to-cli-routing/tc-9.2-race-deregister.txt` containing `"agent not found"` substring; `sqlite3 ... "SELECT COUNT(*) FROM chat_messages WHERE thread_id='agent:<B_id>'"` in `tc-9.2-no-row.txt` returning `0` (no row from this call). |
| TC-C2C-10.1 | UC-C2C-10 | Mixed (CLI + DB) | CC #1's bridge auto-subscribe to `agent:<A_id>` fails at connect time (simulate by making `chat.db` temporarily unwritable). | Bridge logs `warn: auto-subscribe for agent-inbox failed; inbound agent-to-agent messages will not be delivered until reconnect`; CC #1 remains connected for other MCP calls; reconnect eventually re-attempts subscribe and succeeds. | daemon logs `docs/qa/evidence/cli-to-cli-routing/tc-10.1-auto-subscribe-fail.txt` containing `auto-subscribe` and `failed` substrings; bridge reconnect log `tc-10.1-reconnect.txt` showing successful subscribe after reconnect; CC #1 remains functional for e.g. `chat_list` calls during the subscribe-failure window (verify: `chat_list --thread telegram:<chat_id>` succeeds). |
| TC-C2C-10.2 | UC-C2C-10-E1 | CLI | Daemon is entirely unreachable (no UDS socket). Bridge connect attempt fails. | Existing `try_reconnect` loop handles this (pre-existing behavior, not new). Auto-subscribe is not separately observable in this case. | daemon logs `docs/qa/evidence/cli-to-cli-routing/tc-10.2-daemon-unreachable.txt` containing `reconnect` or `connection failed` substring; `claudebase daemon status --json` exit 1 or equivalent error. |

---

## 10. DND Drain Background Task Error Handling (FR-C2C-5.2)

| # | Use Case | Verification Class | Test Case | Expected Result | Evidence Required |
|---|----------|--------------------|-----------|-----------------|-------------------|
| TC-C2C-11.1 | UC-C2C-11 | Mixed (DB + CLI) | DND drain task fires its 30s tick; agent B's DND has expired; `chat.db` encounters a transient I/O error mid-drain. | Task logs `warn: DND drain tick failed for agent <B>: <error>`; swallows the error; background task remains alive (does NOT crash or panic); agent B's `dnd_until_ts` remains SET (was not cleared because update failed); next tick successfully drains. | daemon logs `docs/qa/evidence/cli-to-cli-routing/tc-11.1-drain-error.txt` containing `DND drain tick failed` and `warn` substrings; daemon process alive after the error tick: `pgrep claudebase` / `Get-Process claudebase` in `tc-11.1-daemon-alive.txt` showing running PID; next-tick drain: `sqlite3 ... "SELECT dnd_until_ts, delivered_at FROM ..."` in `tc-11.1-next-tick-drained.txt` showing `dnd_until_ts = NULL` and `delivered_at` non-NULL. |
| TC-C2C-11.2 | UC-C2C-11-EC1 | Mixed (DB + CLI) | Repeated `chat.db` errors across 3+ drain ticks. Agent B's `dnd_until_ts` stays non-NULL; messages accumulate. | Agent B visible in `list-alive` with `dnd_until_ts` still set (stuck DND visible to operator); operator calls `agent_set_dnd("off")` explicitly — if `chat.db` recovers, `dnd_until_ts` clears; if `chat.db` remains corrupt, error is surfaced. | `list-alive --json` stdout in `docs/qa/evidence/cli-to-cli-routing/tc-11.2-stuck-dnd.txt` showing non-NULL `dnd_until_ts` after 3 failed ticks; explicit `agent_set_dnd("off")` transcript in `tc-11.2-explicit-off.txt` showing either success or structured DB error. |

---

## 11. URL Normalization — `project_id` Resolver (FR-C2C-2.2, FR-C2C-2.3)

| # | Use Case | Verification Class | Test Case | Expected Result | Evidence Required |
|---|----------|--------------------|-----------|-----------------|-------------------|
| TC-C2C-12.1 | UC-C2C-12 | Mixed (CLI + DB) | Clone A: `remote.origin.url = "https://github.com/codefather-labs/claudebase.git"`. Clone B: `remote.origin.url = "git@github.com:codefather-labs/claudebase.git"`. Both call `resolve_project_id`. | Both return identical `project_id = "github.com/codefather-labs/claudebase"` (protocol stripped, colon replaced, `.git` removed, lowercased); `list-alive --project current` from either returns both agents. | `cargo test --test project_id_test -- https_and_ssh_urls_normalize_to_same_id` exits 0; `sqlite3 ... "SELECT DISTINCT project_id FROM agent_registry WHERE state='alive'"` in `docs/qa/evidence/cli-to-cli-routing/tc-12.1-url-normalization.txt` returning exactly 1 distinct value `"github.com/codefather-labs/claudebase"`. |
| TC-C2C-12.2 | UC-C2C-12-EC1 | CLI | `git@GitHub.COM:OWNER/Repo.git` (mixed-case URL). `resolve_project_id` normalizes it. | Returns `"github.com/owner/repo"` (fully lowercased); matches the expected form from a standard lowercase HTTPS clone. | `cargo test --test project_id_test -- mixed_case_url_normalized_to_lowercase` exits 0. |
| TC-C2C-13.1 | UC-C2C-13 | CLI | `git init` in temp dir; no `git remote add origin`; no `.claudebase/config.json`. `resolve_project_id` called. | Returns `local:[0-9a-f]{16}`; no error, no panic. | `cargo test --test project_id_test -- no_origin_remote_falls_through_to_hash` exits 0; output in `docs/qa/evidence/cli-to-cli-routing/tc-13.1-no-origin.txt` matching `local:[0-9a-f]{16}`. |
| TC-C2C-13.2 | UC-C2C-13-A | CLI | Git repo with `upstream` remote (no `origin`). `resolve_project_id` called. | Step 1 fails (`remote.origin.url` not found); falls through to Step 2 (config.json absent), then Step 3 (path hash). Returns `local:<16-hex-chars>`. | `cargo test --test project_id_test -- non_origin_remote_falls_through_to_hash` exits 0. |

---

## 12. DND `"until HH:MM"` Parser (FR-C2C-5.1)

| # | Use Case | Verification Class | Test Case | Expected Result | Evidence Required |
|---|----------|--------------------|-----------|-----------------|-------------------|
| TC-C2C-14.1 | UC-C2C-14 | DB | Current local time is 14:30. Agent B calls `agent_set_dnd({ state: "until 17:00" })`. | `dnd_until_ts` in `agent_registry` equals a Unix epoch integer representing today at 17:00 local (daemon timezone); response contains `{ "ok": true, "dnd_until": "...T17:00:00..." }` (ISO-8601 with offset). | `sqlite3 ... "SELECT dnd_until_ts FROM agent_registry WHERE agent_id='<B_id>'"` in `docs/qa/evidence/cli-to-cli-routing/tc-14.1-until-17.txt` — verify value corresponds to today 17:00 local (compute `strftime('%Y-%m-%d 17:00', 'now', 'localtime')` and compare epoch); `cargo test --test agent_dnd_test -- until_hhmm_sets_correct_epoch` exits 0. |
| TC-C2C-14.2 | UC-C2C-14-EC1 | DB | Current local time is 23:45. Agent B calls `agent_set_dnd({ state: "until 01:00" })`. | `dnd_until_ts` represents TOMORROW at 01:00 (midnight rollover); NOT today at 01:00 (which is in the past). | `cargo test --test agent_dnd_test -- until_hhmm_midnight_rollover` exits 0; if daemon is running locally, `sqlite3 ... "SELECT dnd_until_ts FROM agent_registry WHERE agent_id='<B_id>'"` in `docs/qa/evidence/cli-to-cli-routing/tc-14.2-midnight-rollover.txt` — value must be > `strftime('%s', 'now')` (i.e., future). |
| TC-C2C-14.3 | UC-C2C-14-EC2 | DB | Daemon running in UTC timezone; operator expects local timezone (e.g., UTC+2). Agent B calls `agent_set_dnd({ state: "until 17:00" })`. | `dnd_until_ts` represents 17:00 UTC (daemon's timezone), NOT 17:00 UTC+2; this is documented behavior. DND expires at 17:00 UTC. | `sqlite3 ... "SELECT dnd_until_ts FROM agent_registry WHERE agent_id='<B_id>'"` in `docs/qa/evidence/cli-to-cli-routing/tc-14.3-timezone.txt` — epoch value corresponds to 17:00 UTC (confirm via `date -d @<epoch>` or PowerShell equivalent). |
| TC-C2C-14.4 | UC-C2C-14-EC3 | DB | DST spring-forward day; `agent_set_dnd({ state: "until 02:30" })` called (clocks skip 02:00–03:00). | Implementation-specific behavior — either: (a) parser detects the non-existent time and rolls forward to 03:00, or (b) parser returns a structured error. No panic, no crash. | `cargo test --test agent_dnd_test -- dst_boundary_no_panic` exits 0; daemon logs `docs/qa/evidence/cli-to-cli-routing/tc-14.4-dst.txt` containing no `panic` / `unwrap` / `SIGABRT` strings. |

---

## 13. Telegram Inbound Regression Safety (FR-C2C-8.2, FR-C2C-8.4 — NFR-C2C-5)

| # | Use Case | Verification Class | Test Case | Expected Result | Evidence Required |
|---|----------|--------------------|-----------|-----------------|-------------------|
| TC-C2C-15.1 | UC-C2C-15 | Mixed (CLI + DB) | All Slices 1-8 landed. Operator sends "hello" via Telegram DM to bot `@X`. Bridge receives `notifications/claude/channel` with no `meta.kind` field. | Bridge renders `<channel source="plugin:telegram:telegram" chat_id="..." ...>hello</channel>` — NOT `<agent-message>`; all existing Telegram functionality (chat_post, chat_reply, chat_subscribe, `/start`) remains functional; existing ≥ 178 unit tests still pass. | (a) CC #1 transcript excerpt `docs/qa/evidence/cli-to-cli-routing/tc-15.1-tg-channel-render.txt` containing literal `<channel source="plugin:telegram:telegram"` AND NOT containing `<agent-message`; (b) `cargo test --test bridge_agent_message_render_test -- telegram_inbound_still_renders_channel_shape` exits 0; (c) `cargo test --workspace` in `tc-15.1-full-test-suite.txt` — existing ≥ 178 tests pass (no regressions). |
| TC-C2C-15.2 | UC-C2C-15-EC1 | CLI | Inbound `notifications/claude/channel` notification with `meta.kind = "future-extension-value"` (unknown value, not `"agent-to-agent"`). | Bridge falls through to `<channel>` rendering (the `else` branch); no error, no panic, no dropped notification; `meta.kind` value is not inspected further. | `cargo test --test bridge_agent_message_render_test -- unknown_meta_kind_falls_through_to_channel` exits 0; CC #1 transcript excerpt `docs/qa/evidence/cli-to-cli-routing/tc-15.2-unknown-kind-channel.txt` containing `<channel` shape AND NOT containing `<agent-message`. |

---

## 14. Register-Time Identity Capture (FR-C2C-3.1)

| # | Use Case | Verification Class | Test Case | Expected Result | Evidence Required |
|---|----------|--------------------|-----------|-----------------|-------------------|
| TC-C2C-16.1 | UC-C2C-16 | DB | CC #1 in `C:\Users\madwh\Documents\claudebase`, branch `feat/multi-agent-on-v0.6`, git origin `git@github.com:codefather-labs/claudebase.git`. Bridge fires `agent_register`. | `agent_registry` row for agent A has: `project_id = "github.com/codefather-labs/claudebase"`, `branch = "feat/multi-agent-on-v0.6"`, `working_dir = "C:\Users\madwh\Documents\claudebase"`, `feature_description = NULL`, `dnd_until_ts = NULL`, `state = 'alive'`. | `sqlite3 ... "SELECT project_id, branch, working_dir, feature_description, dnd_until_ts FROM agent_registry WHERE agent_id='<A_id>'"` in `docs/qa/evidence/cli-to-cli-routing/tc-16.1-register-identity.txt` — all three C2C identity columns non-NULL, `feature_description` NULL, `dnd_until_ts` NULL; `cargo test --test agent_describe_test -- register_persists_project_id_branch_working_dir` exits 0. |
| TC-C2C-16.2 | UC-C2C-16-E1 | DB | CC in detached HEAD state (`git rev-parse --abbrev-ref HEAD` returns `HEAD`). Bridge fires `agent_register`. | `branch = "HEAD"` stored literally; no error; `list-alive --json` shows `"branch": "HEAD"`. | `sqlite3 ... "SELECT branch FROM agent_registry WHERE agent_id='<A_id>'"` in `docs/qa/evidence/cli-to-cli-routing/tc-16.2-detached-head.txt` returning literal `"HEAD"`. |

---

## 15. Schema Migration v5→v6 Idempotency (FR-C2C-1 — Architect-Mandated TC)

| # | Use Case | Verification Class | Test Case | Expected Result | Evidence Required |
|---|----------|--------------------|-----------|-----------------|-------------------|
| TC-C2C-17.1 | UC-C2C-17 | DB | Daemon starts against a `chat.db` that has the base `agent_registry` v5 schema (only base columns from `src/daemon/chat.rs:443-453`; routing migration columns absent). `ensure_chat_db_schema` runs, calling `apply_routing_migration` then `apply_agent_registry_c2c_migration`. | After migration: `PRAGMA table_info(agent_registry)` shows all 5 new C2C extension columns (`project_id`, `branch`, `working_dir`, `feature_description`, `dnd_until_ts`) AND all 6 routing migration columns (`routing_chat_id`, `routing_thread_id`, `last_user_id`, `host`, `cwd`, `pid`); index `agent_registry_project_id_idx` exists; existing rows survive with `NULL` in all new columns; no error during migration; migration MUST run from `src/daemon/chat.rs::apply_agent_registry_c2c_migration` (NOT `src/store.rs`). | Pre-migration: `sqlite3 ... "PRAGMA table_info(agent_registry)"` in `docs/qa/evidence/cli-to-cli-routing/tc-17.1-schema-before.txt` — must NOT contain `project_id`, `branch`, `working_dir`, `feature_description`, `dnd_until_ts`; post-migration: same pragma in `tc-17.1-schema-after.txt` — MUST contain all 5 C2C columns AND all 6 routing columns; `sqlite3 ... "SELECT COUNT(*) FROM sqlite_master WHERE type='index' AND name='agent_registry_project_id_idx'"` in `tc-17.1-index.txt` returning `1`; `cargo test --test store_v6_test -- v6_migration_adds_five_c2c_columns_idempotently` exits 0. |
| TC-C2C-17.2 | UC-C2C-17-A | DB | `apply_agent_registry_c2c_migration` is called a SECOND time against an already-v6 schema (columns already present). | Migration exits cleanly (exit 0 / `Ok(())`); no duplicate columns; no error; `PRAGMA table_info(agent_registry)` output is byte-identical before and after the second run. | `PRAGMA table_info(agent_registry)` captured before and after second migration call in `docs/qa/evidence/cli-to-cli-routing/tc-17.2-idempotent-before.txt` and `tc-17.2-idempotent-after.txt` — diff MUST exit 0; `cargo test --test store_v6_test -- v6_migration_is_idempotent_on_second_run` exits 0. |

---

## 16. `<agent-message>` Tag Rendering (FR-C2C-8.1, FR-C2C-8.3)

| # | Use Case | Verification Class | Test Case | Expected Result | Evidence Required |
|---|----------|--------------------|-----------|-----------------|-------------------|
| TC-C2C-18.1 | UC-C2C-18 | Mixed (API + CLI) | Agent A sends a message to agent B (UC-C2C-3 primary flow completed). CC #2 bridge receives the `notifications/claude/channel` event with `meta.kind = "agent-to-agent"`. | Bridge renders: `<agent-message from="<A_id>" thread="agent:<B_id>" ts="<ISO_timestamp>">CONTENT</agent-message>`; all three attributes (`from`, `thread`, `ts`) are populated from notification meta (`chat_id`, `thread`, message timestamp respectively). | CC #2 transcript excerpt `docs/qa/evidence/cli-to-cli-routing/tc-18.1-agent-message-tag.txt` containing the literal substrings `<agent-message from="<A_id>"`, `thread="agent:<B_id>"`, and a `ts="` attribute with a non-empty value; `cargo test --test bridge_agent_message_render_test -- agent_to_agent_renders_agent_message_with_correct_attributes` exits 0. |
| TC-C2C-18.2 | UC-C2C-18-EC1 | CLI | Inbound notification has `meta.kind = "agent-to-agent"` but `meta.ts` timestamp field is absent. | Bridge renders `<agent-message from="..." thread="..." ts="">CONTENT</agent-message>` with empty `ts` attribute — does NOT omit the attribute, does NOT crash. | `cargo test --test bridge_agent_message_render_test -- absent_ts_renders_empty_attribute_not_crash` exits 0; CC #2 transcript excerpt `docs/qa/evidence/cli-to-cli-routing/tc-18.2-empty-ts.txt` containing `ts=""` substring (empty attribute, not absent). |

---

## 17. Acceptance Gates (AC-C2C-1..AC-C2C-5)

These end-to-end integration cases require two real CC instances running simultaneously.

| # | Use Case | Verification Class | Test Case | Expected Result | Evidence Required |
|---|----------|--------------------|-----------|-----------------|-------------------|
| TC-C2C-AC1 | UC-C2C-1 (AC-C2C-1) | Mixed (CLI + DB) | Operator opens 2 CC windows — CC #1 in clone A (branch `feat/multi-agent-on-v0.6`), CC #2 in clone B (branch `main`). Both clones share the same `git remote origin URL`. Both CC instances register. Operator runs `claudebase agent list-alive --project current --json` from CC #1. | stdout JSON array lists both agents; each entry contains `agent_id`, `branch`, `working_dir`, `feature_description`, `last_seen_at`, `dnd_until_ts`; agents from unrelated projects absent; at least one agent has non-NULL `feature_description`; agents from other projects do NOT appear; exit 0. | (a) stdout JSON array saved as `docs/qa/evidence/cli-to-cli-routing/AC-C2C-1-list-alive-stdout.json` — MUST be valid JSON with ≥ 2 elements, both having `project_id = "github.com/codefather-labs/claudebase"` (or resolved equivalent), all 6 required fields present; (b) `sqlite3 ... "SELECT COUNT(DISTINCT project_id) FROM agent_registry WHERE state='alive'"` in `AC-C2C-1-distinct-projects.txt` — count of distinct non-NULL project_ids present confirms isolation. |
| TC-C2C-AC2 | UC-C2C-2 (AC-C2C-2) | Mixed (API + DB + CLI) | Agent A calls `agent_describe({ feature_id: "cli-to-cli-routing", branch: "feat/multi-agent-on-v0.6", description: "Wiring agent-to-agent comms via daemon" })`. Within 5 seconds, agent B runs `claudebase agent list-alive --project current`. | Daemon-side DB has `feature_description = "Wiring agent-to-agent comms via daemon"` for agent A; second CC's `list-alive` output shows the updated `feature_description` within 5s. | (a) `sqlite3 ... "SELECT feature_description FROM agent_registry WHERE agent_id=?"` in `docs/qa/evidence/cli-to-cli-routing/AC-C2C-2-describe-roundtrip.txt` returning `"Wiring agent-to-agent comms via daemon"`; (b) `list-alive --json` stdout captured ≤ 5s after `agent_describe` call in `AC-C2C-2-list-alive-after.json` — agent A's row shows `feature_description: "Wiring agent-to-agent comms via daemon"`. |
| TC-C2C-AC3 | UC-C2C-3 (AC-C2C-3) | Mixed (API + DB + CLI) | Agent A in CC #1 calls `agent_send({ to_agent_id: "<B_id>", content: "coordination test" })`. Agent B is NOT in DND. | Within 2 seconds: CC #2 transcript shows `<agent-message from="<A_id>" thread="agent:<B_id>" ...>coordination test</agent-message>`; `chat_messages` row has `from_agent='<A_id>'`, `thread_id='agent:<B_id>'`, `delivered_at` non-NULL. | (a) CC #2 transcript excerpt saved as `docs/qa/evidence/cli-to-cli-routing/AC-C2C-3-send-receive-transcript.md` containing `<agent-message from="<A_id>"` AND `thread="agent:<B_id>"` AND the content `coordination test`; (b) `sqlite3 ... "SELECT from_agent, thread_id, delivered_at FROM chat_messages WHERE thread_id='agent:<B_id>' ORDER BY rowid DESC LIMIT 1"` in `AC-C2C-3-chat-message-row.txt` — `from_agent='<A_id>'`, `thread_id='agent:<B_id>'`, `delivered_at` non-NULL integer. |
| TC-C2C-AC4 | UC-C2C-4 (AC-C2C-4) | Mixed (API + DB + CLI) | Agent B sets DND `"30m"`. Agent A sends 3 messages. CC #2 receives NO notifications during DND. `agent_set_dnd("off")` called. Drain fires within 30s × ceil(3/10) = 30s. | During DND: `agent_send` for each of 3 messages returns `{ "queued": true }`; no `<agent-message>` in CC #2 transcript. After "off": all 3 queued messages drain within 30s; `chat_messages.delivered_at` non-NULL on all 3; CC #2 transcript shows all 3 `<agent-message>` blocks. | (a) CC #2 transcript excerpt during DND window `docs/qa/evidence/cli-to-cli-routing/AC-C2C-4-dnd-queued-then-drained.md` — MUST contain 3 `"queued":true` responses AND zero `<agent-message>` blocks before "off"; (b) after drain: same file extended to show 3 `<agent-message>` blocks; (c) `sqlite3 ... "SELECT COUNT(*) FROM chat_messages WHERE thread_id='agent:<B_id>' AND delivered_at IS NOT NULL"` in `AC-C2C-4-drain-complete.txt` returning `3` after drain completes; (d) `sqlite3 ... "SELECT dnd_until_ts FROM agent_registry WHERE agent_id='<B_id>'"` in `AC-C2C-4-dnd-cleared.txt` returning `NULL` after drain. |
| TC-C2C-AC5 | UC-C2C-5 (AC-C2C-5) | Mixed (FS + CLI + DB) | ExitPlanMode fires in CC #1 (operator exits plan mode with `.claude/plan.md` containing a heading). Hook fires; injected context mandates both writes. Agent A calls `agent_describe` AND updates `.claude/scratchpad.md` in the same turn. | Hook output captured in session transcript; daemon row updated with non-NULL `feature_description`; `.claude/scratchpad.md` `## Feature:` line matches the daemon's `feature_description`. Both writes in the SAME agent turn. | (a) hook stdout captured in `docs/qa/evidence/cli-to-cli-routing/AC-C2C-5-hook-fired-stdout.txt` — non-empty, no error, exit 0; (b) session transcript excerpt `AC-C2C-5-transcript.txt` showing `agent_describe` MCP call AND scratchpad edit in the SAME turn (both within the same agent response); (c) `sqlite3 ... "SELECT feature_description FROM agent_registry WHERE agent_id='<A_id>'"` in `AC-C2C-5-db-feature.txt` — non-NULL value; (d) `grep "## Feature:" .claude/scratchpad.md` stdout in `AC-C2C-5-scratchpad-match.txt` — value matches daemon `feature_description` exactly. |

---

## Coverage Summary

**Total test cases: 57**
- 52 use-case test cases (mapping all UC-C2C-1 through UC-C2C-18 primary flows, alt flows, error flows, and edge cases)
- 5 acceptance-gate test cases (AC-C2C-1 through AC-C2C-5)

**UC coverage: all 18 UCs mapped (zero gaps)**

| UC | Scenarios covered |
|----|-------------------|
| UC-C2C-1 | primary + 2 alt + 2 error + 3 edge = 8 TCs |
| UC-C2C-2 | primary + 2 alt + 1 error + 1 edge = 5 TCs |
| UC-C2C-3 | primary + 2 alt + 2 error (incl. FR-C2C-4.6) + 1 edge = 6 TCs |
| UC-C2C-4 | primary + 3 alt + 1 error + 2 edge (incl. FR-C2C-5.5 rate-limit) = 7 TCs |
| UC-C2C-5 | primary + 3 alt + 1 error + 2 edge = 7 TCs |
| UC-C2C-6 | primary + 2 alt + 1 error + 1 edge = 5 TCs |
| UC-C2C-7 | primary + 1 alt + 1 edge = 3 TCs |
| UC-C2C-8 | primary + 1 edge = 2 TCs |
| UC-C2C-9 | primary + 1 edge = 2 TCs |
| UC-C2C-10 | primary + 1 error = 2 TCs |
| UC-C2C-11 | primary + 1 edge = 2 TCs |
| UC-C2C-12 | primary + 1 edge = 2 TCs |
| UC-C2C-13 | primary + 1 alt = 2 TCs |
| UC-C2C-14 | primary + 3 edge = 4 TCs |
| UC-C2C-15 | primary + 1 edge = 2 TCs |
| UC-C2C-16 | primary + 1 error = 2 TCs |
| UC-C2C-17 | primary + 1 alt = 2 TCs |
| UC-C2C-18 | primary + 1 edge = 2 TCs |

**Verification Class distribution:**
| Class | Count |
|-------|-------|
| CLI | 5 |
| DB | 12 |
| FS | 1 |
| API | 4 |
| Mixed (CLI + DB) | 18 |
| Mixed (API + DB) | 8 |
| Mixed (API + DB + CLI) | 3 |
| Mixed (FS + CLI + DB) | 2 |
| Mixed (FS + CLI) | 2 |
| Mixed (DB + CLI) | 2 |
| **Total** | **57** |

**Architect-mandated special TCs:**
- FR-C2C-4.6 (sender identity binding): TC-C2C-3.5
- FR-C2C-5.5 (DND drain rate limit): TC-C2C-4.7
- i64::MAX sentinel (OQ-UC-C2C-1 resolution): TC-C2C-4.4
- Slice 7 pre-flight verification fallback chain: TC-C2C-5.4
- v5→v6 migration idempotency in `src/daemon/chat.rs` (NOT `src/store.rs`): TC-C2C-17.1, TC-C2C-17.2
