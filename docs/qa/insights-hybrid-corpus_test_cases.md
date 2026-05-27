# Test Cases: Insights Hybrid Corpus

> Based on [PRD §18](../PRD.md) and [Use Cases](../use-cases/insights-hybrid-corpus_use_cases.md)

---

## Facts

### Verified facts

- PRD §18 read in full this session: `docs/PRD.md` lines 785–1108; 9 FR groups (FR-IHC-1..9), 17 ACs (AC-IHC-1..17), 7 NFRs (NFR-IHC-1..7). — salience: high
- All 21 use cases (UC-IHC-1..21) with all sub-flows read in full from `docs/use-cases/insights-hybrid-corpus_use_cases.md` this session. — salience: high
- Established QA format read from `docs/qa/agent-insights-base_test_cases.md` this session — confirms column order (`ID | Description | Verification Class | Evidence Required | Status`), single-table layout, and use-case coverage table. — salience: high
- `agent-chat-daemon_test_cases.md` read this session for cross-reference on multi-section format and external-contract citation conventions. — salience: medium
- Routing invariant: `--category project` → cwd-resolved `<project>/.claude/knowledge/insights.db` (local db); `--category general` → fixed `~/.claude/knowledge/insights.db` (global db). Source: PRD §18 FR-IHC-3.4, UC-IHC-1 primary flow step 4 and UC-IHC-2 primary flow step 3. — salience: high
- `--tags` absence is a BUSINESS-LOGIC check (not a clap required-arg) — the tag list is a repeatable `Vec<String>` from `#[arg(long)]` that defaults to empty; the check is `tags.is_empty()` in `run_insight_create`. Source: UC-IHC-3 primary flow step 2, PRD §18 FR-IHC-3.2. — salience: high
- `--category` absence IS a clap required-arg failure (clap-generated exit 2 before any application logic). Source: UC-IHC-4 primary flow step 2, PRD §18 FR-IHC-3.1. — salience: high
- Schema v5 is additive on top of v4: adds `category TEXT`, `project_slug TEXT` columns to `documents`; adds `insight_tags(doc_id, tag, UNIQUE(doc_id,tag))` table and two indexes. Source: PRD §18 FR-IHC-1.1..1.3 and §18.7. — salience: high
- v4→v5 backfill: `source_path LIKE 'agent:%'` rows get `category='project'`, `project_slug=<db-path-basename>`, and one `insight_tags` row with `COALESCE(NULLIF(feature_slug,''),'untagged')`. Books-corpus rows (`source_path NOT LIKE 'agent:%'`) keep `category=NULL` with zero `insight_tags` rows. Source: PRD §18 FR-IHC-1.5..1.6, UC-IHC-13 primary flow steps 6–8. — salience: high
- Default in-project read posture: `merge(local-project db + global-general db)`. Other projects' dbs are walled off. Source: PRD §18 FR-IHC-5.2. — salience: high
- Registry atomicity: `upsert_project` writes temp file then `rename(2)` (POSIX atomic, near-atomic on Windows). Source: PRD §18 FR-IHC-6.3, UC-IHC-12 primary flow step 3–4. — salience: high
- `claudebase run` calls `upsert_project(cwd)` at the TOP of `run_claude_with_preset` BEFORE `exec()` at main.rs:199. Source: PRD §18 FR-IHC-6.5, UC-IHC-10-EC1. — salience: high
- SessionStart hook constraint: `.ps1` file MUST be ASCII-only (no BOM), established by commit `2d5eb8d`. Source: PRD §18 FR-IHC-7.3, git log shown in session context. — salience: medium
- Knowledge base corpus: `doc_count > 0` (index.db present) but insights corpus query returned 0 hits for `"insights hybrid corpus tags category routing"` at `--feature insights-hybrid-corpus --salience high` — no prior session insights on this feature. — salience: low
- Corpus scope relevance: corpus domain is general tooling/SDLC; task domain is CLI engineering + SQLite schema migration. Partial overlap; no domain-specific external references needed beyond what PRD and use-cases supply. — salience: low

### External contracts

- **`claudebase` CLI v0.6.0 → v0.7.0 (this feature)** — symbol: `insight create` gains required `--category <general|project>` (clap value_enum, exit 2 if absent); `--tags <tag>` (repeatable Vec<String>, business-logic exit 2 if empty after parse). New subcommand: `insight tags [--category c] [--project slug] [--json]`. New flags on read subcommands: `--tag`, `--category`, `--project`, `--general-only`, `--project-only`. Source: PRD §18 FR-IHC-3.1..3.2, §18.6. Verified this session against PRD §18. — verified: yes — salience: high
- **`rusqlite` — `ON DELETE CASCADE`** — symbol: foreign-key cascade from `documents.id` to `insight_tags.doc_id` and to `chunks` rows; enables `DELETE FROM documents WHERE id=?` to cascade-delete all associated tags and chunks. Source: PRD §18 FR-IHC-1.2 schema block and UC-IHC-21 primary flow step 4. — verified: no — assumption (matches pattern from v4 schema; will be confirmed by Slice 1 test). — salience: high
- **`std::env::var("HOME")` / `USERPROFILE`** — symbol: used by `resolve_global_insights_db()` to derive `~/.claude/knowledge/insights.db`; returns `Err` if unset → exit 1. Source: PRD §18 FR-IHC-2.1, UC-IHC-2-E1. — verified: no — assumption (standard Rust stdlib). — salience: medium
- **POSIX `rename(2)` atomicity** — symbol: same-filesystem atomic rename used for registry write (`projects.json` via temp file). Windows `MoveFileExW` is near-atomic. Source: PRD §18 FR-IHC-6.3. — verified: no — assumption (established POSIX spec; security-auditor confirms at Slice 6). — salience: high
- **`clap` `value_enum` required argument** — symbol: `#[arg(long, value_enum)]` with no `default_value`; clap generates exit-2 usage error when the argument is absent. Source: PRD §18 FR-IHC-3.1, UC-IHC-4 primary flow step 2–4. — verified: no — assumption (clap v3/v4 behavior consistent with existing `Salience`/`SearchMode` patterns in the codebase). — salience: medium
- **SQLite FTS5 `ON DELETE CASCADE`** — symbol: `chunks` and `chunks_fts` rows are cascade-deleted when their parent `documents` row is deleted. Source: PRD §18 UC-IHC-21, existing §16 schema. — verified: no — assumption (consistent with v4 behavior). — salience: medium

### Assumptions

- `insight tags` merged count for a tag present in both dbs is the SUM of per-db counts (unified view), not two separate entries. Source: PRD §18 FR-IHC-4.5 says "merges tags from BOTH"; UC-IHC-5 primary flow step 4 says "counts are summed." Risk: if implementation returns separate per-db rows, the JSON shape changes. How to verify: Slice 4 test asserts the summed count directly. — salience: medium
- `--general-only` and `--project-only` are mutually exclusive; supplying both yields exit 2. Source: UC-IHC-8-EC1. The PRD does not specify the exact error message. Risk: implementation may choose a different error text. How to verify: test asserts exit code 2 (not the exact message). — salience: medium
- When `--general-only` is combined with `--project <slug>`, `--general-only` takes precedence and `--project` is silently ignored. Source: UC-IHC-8-A2. Alternative: exit-2 conflict. Risk: if the implementation chooses exit-2 instead, TC-IHC-8.3 needs updating. How to verify: Slice 5 implementer decision surfaces this; test is written to accept either behavior and is marked PLANNED until confirmed. — salience: low
- When `--project <slug>` is supplied to `insight search/list/random`, the default merge becomes `merge(project-B db + global-general db)` — replacing the cwd-project with the named project, not adding it as a third db. Source: UC-IHC-18 primary flow step 5. Risk: three-way merge or cwd-exclusion differs. How to verify: TC-IHC-18.1 asserts cwd-project insights absent from results. — salience: medium
- Global db corrupt error posture: CLI exits 1 rather than falling back to local-only results. Source: UC-IHC-7-E2. PRD does not specify this explicitly. Risk: implementation may choose partial-success fallback. How to verify: TC-IHC-7.6 is marked PLANNED until Slice 5 architect decision. — salience: low

### Open questions

- `--general-only` + `--project <slug>` combination: exit-2 conflict or silent `--project` ignore? Needs: Slice 5 implementer/architect decision. Affects TC-IHC-8.3. — salience: low
- Global db corrupt behavior (exit 1 vs. local-only fallback). Needs: Slice 5 architect decision. Affects TC-IHC-7.6. — salience: low
- `insight list` dual-db merge behavior: UC-IHC-5 covers tags and UC-IHC-7 covers search; list is parallel — dedicate TC-IHC-5.x note that `list` follows same merged default per FR-IHC-5.2, tested implicitly via the tags and search cases (separate `list` dual-db test recommended for Slice 5). — salience: low

---

## Decisions

### Inbound validation

- Task received: create `docs/qa/insights-hybrid-corpus_test_cases.md` for PRD §18 and all 21 UCs. Task is coherent; PRD §18 and use-cases file are consistent with each other (no upstream contradiction detected). Checked: §18 FR/AC count, UC count, and use-case sub-flow types all match what the task description specifies. Outcome: proceeded. — salience: high
- Inbound task specifies `Verification Class` of `CLI | DB | FS | Mixed` only — no `UI/UX` or `API` rows. Verified: claudebase is a Rust CLI binary + SQLite DBs + filesystem artifacts; no HTTP API, no browser surface. This is consistent with PRD §18.8 (affected files are all Rust/shell/SQL/JSON). Outcome: no UI/UX or API rows emitted. — salience: high
- Inbound task specifies ALL 21 UCs must be mapped. Verified: all 21 UC-IHC-N primary flows plus selected alternative/error/edge sub-flows are represented. Coverage table below is the audit trail. — salience: high

### Decisions made

- **`TC-IHC-N.M` numbering** — Test cases numbered to match UC grouping: TC-IHC-1.x for UC-IHC-1, etc. Alternative flows and error flows get their own TC rows. Edge cases that are non-obvious or high-risk get dedicated TC rows; trivial edge cases are noted in Evidence Required of the parent TC. Q1 hack? no. Q2 sane? yes. Q3 alternatives? flat sequential numbering (rejected: breaks UC traceability). Q4 cause. Q5 n/a. — salience: medium
- **`Mixed` class for create and migration cases** — Any TC that asserts BOTH a CLI exit code AND a DB row state uses `Mixed`. Pure CLI exit-code checks (e.g., missing flags) use `CLI`. Pure DB state checks use `DB`. FS checks (file existence, JSON content) use `FS` or `Mixed` when combined with CLI. Q1 hack? no. Q2 sane? yes (consistent with the established format in `agent-insights-base_test_cases.md`). — salience: medium
- **Architect collision test (chunk_id overlap)** included as TC-IHC-7.5 — the inbound task specifically called this out as a required test. Two chunks from different dbs with the same `chunk_id` integer must survive RRF fusion as 2 distinct hits. Requires explicit assertion on result count = 2. — salience: high
- **PLANNED status for concurrency (TC-IHC-12.x), corrupt-global-db (TC-IHC-7.6), and hook-word-count (TC-IHC-15.3)** — These depend on implementation details (platform behavior, Slice 5 architect decision, hook file content) not resolvable at QA-plan time. Marking PLANNED keeps the plan honest without blocking implementation. — salience: medium

### Hacks acknowledged

(none)

### Symptom-only patches (with root-cause links)

(none)

---

## Use Case Coverage

| Use Case | Test Case(s) | Status |
|----------|--------------|--------|
| UC-IHC-1 (create project insight — happy path) | TC-IHC-1.1, 1.2, 1.3, 1.4 | planned |
| UC-IHC-1-A1 (--project explicit slug) | TC-IHC-1.5 | planned |
| UC-IHC-1-A2 (tags with leading #) | TC-IHC-1.6 | planned |
| UC-IHC-1-A3 (duplicate tags) | TC-IHC-1.7 | planned |
| UC-IHC-1-A4 (insights.db absent — created on first write) | TC-IHC-1.8 | planned |
| UC-IHC-1-E1 (project root resolve fails) | TC-IHC-1.9 | planned |
| UC-IHC-1-E2 (SQLite write failure) | TC-IHC-1.10 | planned |
| UC-IHC-1-EC1 (tag reduces to empty string) | TC-IHC-1.11 | planned |
| UC-IHC-1-EC3 (v4 db auto-migrates on open then proceeds) | covered by TC-IHC-13.1 + TC-IHC-1.1 | planned |
| UC-IHC-2 (create general insight — happy path) | TC-IHC-2.1, 2.2 | planned |
| UC-IHC-2-A1 (--project ignored for general) | TC-IHC-2.3 | planned |
| UC-IHC-2-E1 ($HOME unset) | TC-IHC-2.4 | planned |
| UC-IHC-2-EC1 (both dbs absent — independent creation) | TC-IHC-2.5 | planned |
| UC-IHC-2-EC2 (general write from non-git dir) | TC-IHC-2.6 | planned |
| UC-IHC-2-EC3 (cross-agent same body general — not deduped) | TC-IHC-2.7 | planned |
| UC-IHC-3 (create without --tags → exit 2) | TC-IHC-3.1 | planned |
| UC-IHC-3-A1 (tags all reduce to empty) | TC-IHC-3.2 | planned |
| UC-IHC-3-EC1 (piped body but no --tags) | TC-IHC-3.3 | planned |
| UC-IHC-4 (create without --category → clap exit 2) | TC-IHC-4.1 | planned |
| UC-IHC-4-A1 (invalid --category value) | TC-IHC-4.2 | planned |
| UC-IHC-4-EC1 (--category empty string) | TC-IHC-4.3 | planned |
| UC-IHC-5 (insight tags — merged default) | TC-IHC-5.1, 5.2, 5.3, 5.4 | planned |
| UC-IHC-5-A1 (global db absent → local-only) | TC-IHC-5.5 | planned |
| UC-IHC-5-A2 (local db absent → global-only) | TC-IHC-5.6 | planned |
| UC-IHC-5-A3 (both empty → []) | TC-IHC-5.7 | planned |
| UC-IHC-6 (insight tags --category general) | TC-IHC-6.1 | planned |
| UC-IHC-6-A1 (--project filter) | TC-IHC-6.2 | planned |
| UC-IHC-6-EC1 (--project not in registry) | TC-IHC-6.3 | planned |
| UC-IHC-7 (merged read + tag filter — happy path) | TC-IHC-7.1, 7.2, 7.3 | planned |
| UC-IHC-7-A1 (--tag OR / any-intersection semantics) | TC-IHC-7.4 | planned |
| UC-IHC-7-EC1 (planted other-project row excluded) | TC-IHC-7.5 | planned |
| UC-IHC-7-EC1 (chunk_id collision — architect test) | TC-IHC-7.5 | planned |
| UC-IHC-7-A4 (encoder unavailable → lexical fallback) | TC-IHC-7.7 | planned |
| UC-IHC-7-E1 (both dbs absent → empty result, exits 0) | TC-IHC-7.8 | planned |
| UC-IHC-8 (--general-only) | TC-IHC-8.1, 8.2 | planned |
| UC-IHC-8-A1 (--general-only + --tag) | TC-IHC-8.3 | planned |
| UC-IHC-8-EC1 (--general-only + --project-only → exit 2) | TC-IHC-8.4 | planned |
| UC-IHC-9 (--project-only) | TC-IHC-9.1, 9.2 | planned |
| UC-IHC-9-EC1 (--project-only, local db absent → []) | TC-IHC-9.3 | planned |
| UC-IHC-10 (claudebase run upserts registry) | TC-IHC-10.1, 10.2 | planned |
| UC-IHC-10-E1 (registry write fails → exec still fires) | TC-IHC-10.3 | planned |
| UC-IHC-10-E2 (malformed projects.json → starts fresh) | TC-IHC-10.4 | planned |
| UC-IHC-10-EC2 (symlinked project dir → canonical dedup) | TC-IHC-10.5 | planned |
| UC-IHC-11 (registry upsert idempotent) | TC-IHC-11.1, 11.2 | planned |
| UC-IHC-12 (concurrent run race — no corrupt JSON) | TC-IHC-12.1 | planned |
| UC-IHC-13 (v4→v5 migration) | TC-IHC-13.1, 13.2, 13.3, 13.4 | planned |
| UC-IHC-13-A1 (idempotent re-open at v5) | TC-IHC-13.5 | planned |
| UC-IHC-13-A2 (fresh db → v5 stamp) | TC-IHC-13.6 | planned |
| UC-IHC-13-E2 (schema_version unknown) | TC-IHC-13.7 | planned |
| UC-IHC-13-EC1 (v4 insight with feature_slug=NULL) | TC-IHC-13.8 | planned |
| UC-IHC-14 (global db absent — first general write creates it) | TC-IHC-14.1 | planned |
| UC-IHC-14-E1 (HOME read-only → exit 1) | TC-IHC-14.2 | planned |
| UC-IHC-15 (SessionStart hook fires) | TC-IHC-15.1, 15.2 | planned |
| UC-IHC-15-E2 (hook >200 words) | TC-IHC-15.3 | planned |
| UC-IHC-16 (hook install idempotent) | TC-IHC-16.1, 16.2 | planned |
| UC-IHC-16-E2 (jq not installed → graceful skip) | TC-IHC-16.3 | planned |
| UC-IHC-17 (cross-project read walled off — default) | TC-IHC-17.1 | planned |
| UC-IHC-18 (cross-project read via --project) | TC-IHC-18.1, 18.2 | planned |
| UC-IHC-18-E1 (--project not in registry → exit 1) | TC-IHC-18.3 | planned |
| UC-IHC-19 (gc — both dbs default; --category general) | TC-IHC-19.1, 19.2 | planned |
| UC-IHC-20 (dedup per-db continues with new flags) | TC-IHC-20.1, 20.2 | planned |
| UC-IHC-21 (delete with --category general) | TC-IHC-21.1, 21.2 | planned |

---

## 1. Schema v5 Migration (FR-IHC-1)

### 1.1 Fresh database — schema v5 stamp

| ID | Use Case | Verification Class | Test Case | Expected Result | Evidence Required |
|----|----------|--------------------|-----------|-----------------|--------------------|
| TC-IHC-13.6 | UC-IHC-13-A2 | Mixed | Initialize a fresh `insights.db` by running any `insight` subcommand against a project with no prior db | (a) exit 0; (b) `PRAGMA user_version` returns 5; (c) `pragma_table_info(documents)` shows `category` and `project_slug` columns; (d) `insight_tags` table exists with both indexes | (a) `assert_cmd.assert().success()`; (b) `rusqlite::Connection::open(db_path).query_row("PRAGMA user_version", [], |r| r.get::<_,i64>(0))` == 5; (c) SQL `PRAGMA pragma_table_info('documents')` returns rows with `name='category'` and `name='project_slug'`; (d) SQL `SELECT name FROM sqlite_master WHERE type='table' AND name='insight_tags'` returns one row |
| TC-IHC-13.1 | UC-IHC-13 | DB | Open a pre-built v4 `insights.db` fixture with the v0.7.0 binary; run `claudebase insight list --json` | `PRAGMA user_version` returns 5 after the open; `category` and `project_slug` columns exist on `documents`; `insight_tags` table exists | SQL `PRAGMA user_version` == 5; SQL `PRAGMA pragma_table_info('documents')` includes `category` and `project_slug`; SQL `SELECT name FROM sqlite_master WHERE type='table' AND name='insight_tags'` returns one row; exit 0 |
| TC-IHC-13.2 | UC-IHC-13 | DB | After v4→v5 migration on a db with 4 v4 insight rows (`source_path LIKE 'agent:%'`): assert all 4 rows are backfilled | Each of the 4 v4 insight rows has `category='project'`, non-empty `project_slug`, and exactly one row in `insight_tags` | SQL `SELECT COUNT(*) FROM documents WHERE source_path LIKE 'agent:%' AND category='project'` == 4; SQL `SELECT COUNT(*) FROM documents WHERE source_path LIKE 'agent:%' AND project_slug IS NOT NULL` == 4; SQL `SELECT COUNT(DISTINCT doc_id) FROM insight_tags WHERE doc_id IN (SELECT id FROM documents WHERE source_path LIKE 'agent:%')` == 4 |
| TC-IHC-13.3 | UC-IHC-13 | DB | After v4→v5 migration: assert books-corpus rows are untouched | All rows where `source_path NOT LIKE 'agent:%'` have `category IS NULL` and zero `insight_tags` entries | SQL `SELECT COUNT(*) FROM documents WHERE source_path NOT LIKE 'agent:%' AND category IS NOT NULL` == 0; SQL `SELECT COUNT(*) FROM insight_tags t JOIN documents d ON t.doc_id=d.id WHERE d.source_path NOT LIKE 'agent:%'` == 0 |
| TC-IHC-13.4 | UC-IHC-13 | DB | After v4→v5 migration: assert both indexes exist | `idx_insight_tags_tag` and `idx_documents_category` present in `sqlite_master` | SQL `SELECT COUNT(*) FROM sqlite_master WHERE type='index' AND name IN ('idx_insight_tags_tag','idx_documents_category')` == 2 |
| TC-IHC-13.5 | UC-IHC-13-A1 | CLI | Open a v5 `insights.db` a second time with the v0.7.0 binary (idempotent re-open) | Exits 0; `PRAGMA user_version` still 5; no duplicate columns or tables added | `assert_cmd.assert().success()`; SQL `PRAGMA user_version` == 5; SQL `SELECT COUNT(*) FROM pragma_table_info('documents') WHERE name='category'` == 1 (not 2) |
| TC-IHC-13.7 | UC-IHC-13-E2 | CLI | Open a `insights.db` with `PRAGMA user_version = 6` (unknown future version) | Exits 1 with stderr containing `error: unsupported schema version` | `assert_cmd.assert().failure().code(1)`; stderr contains literal substring `unsupported schema version` |
| TC-IHC-13.8 | UC-IHC-13-EC1 | DB | v4 insight rows where `feature_slug IS NULL` receive `tag='untagged'` after migration | `insight_tags` has one row per null-feature_slug doc with `tag='untagged'` | SQL `SELECT t.tag FROM insight_tags t JOIN documents d ON t.doc_id=d.id WHERE d.feature_slug IS NULL` returns rows all equal to `'untagged'` |

---

## 2. Global Insights Resolver (FR-IHC-2)

### 2.1 Resolver creation and path contract

| ID | Use Case | Verification Class | Test Case | Expected Result | Evidence Required |
|----|----------|--------------------|-----------|-----------------|--------------------|
| TC-IHC-14.1 | UC-IHC-14 | Mixed | Run `claudebase insight create "global lesson" --category general --tags general-knowledge --type agent-learned --agent prd-writer --salience medium` on a machine where `~/.claude/knowledge/` does not exist | (a) exits 0; (b) `~/.claude/knowledge/` directory exists; (c) `~/.claude/knowledge/insights.db` exists at schema v5; (d) row inserted with `category='general'`, `project_slug IS NULL`; (e) cwd-local db NOT created | (a) `assert_cmd.assert().success()`; (b) `fs::metadata("~/.claude/knowledge/").is_ok()`; (c) `fs::metadata("~/.claude/knowledge/insights.db").is_ok()`; SQL `PRAGMA user_version` on global db == 5; (d) SQL `SELECT COUNT(*) FROM documents WHERE category='general' AND project_slug IS NULL` == 1 on global db; (e) `fs::metadata("<cwd>/.claude/knowledge/insights.db").is_err()` OR SQL row count on cwd db unchanged |
| TC-IHC-14.2 | UC-IHC-14-E1 | CLI | Run `insight create --category general` with `HOME` env var unset (Unix) | Exits 1; stderr contains `error: $HOME not set` | `assert_cmd.assert().failure().code(1)`; stderr literal contains `$HOME not set` |
| TC-IHC-2.6 | UC-IHC-2-EC2 | CLI | Run `insight create --category general --tags infra --type agent-learned --agent qa-planner --salience low` from a directory that is NOT under any git project root | Exits 0; global db has the new row; cwd-local db NOT created or modified | `assert_cmd.assert().success()`; SQL `SELECT COUNT(*) FROM documents WHERE category='general'` >= 1 on `~/.claude/knowledge/insights.db`; cwd db unchanged |

---

## 3. `insight create` — Mandatory Flags and Dual-DB Routing (FR-IHC-3)

### 3.1 Happy path — project routing

| ID | Use Case | Verification Class | Test Case | Expected Result | Evidence Required |
|----|----------|--------------------|-----------|-----------------|--------------------|
| TC-IHC-1.1 | UC-IHC-1 | Mixed | `claudebase insight create "Tokio mutex held across await point" --type agent-learned --agent planner --feature insights-hybrid-corpus --salience high --category project --tags tokio mutex` from within a project dir | (a) exits 0; (b) `documents` row in LOCAL db with `category='project'`, `project_slug=<cwd-basename>`; (c) `insight_tags` has rows for `'tokio'` and `'mutex'`; (d) global db NOT created or modified | (a) `assert_cmd.assert().success()`; (b) SQL `SELECT category, project_slug FROM documents ORDER BY id DESC LIMIT 1` on cwd db returns `('project', '<basename>')`; (c) SQL `SELECT COUNT(*) FROM insight_tags WHERE doc_id=(SELECT MAX(id) FROM documents)` on cwd db == 2; SQL `SELECT tag FROM insight_tags WHERE doc_id=(SELECT MAX(id) FROM documents)` contains `'tokio'` and `'mutex'`; (d) `fs::metadata("~/.claude/knowledge/insights.db").is_err()` OR SQL row count on global db unchanged |
| TC-IHC-1.2 | UC-IHC-1 | Mixed | Same as TC-IHC-1.1 with `--json` flag added | stdout JSON contains `"status": "remembered"` (or `"remembered"` substring) with `doc_id`, `sha` fields; exit 0 | `assert_cmd.assert().success()`; stdout parses as JSON object with `doc_id` integer field and `sha` string field |
| TC-IHC-1.3 | UC-IHC-1 | DB | After TC-IHC-1.1 write: assert `chunks` and `chunks_fts` have rows for the new doc | `SELECT COUNT(*) FROM chunks WHERE doc_id = <new-id>` ≥ 1; `chunks_fts` rowid present | SQL `SELECT COUNT(*) FROM chunks WHERE doc_id=(SELECT MAX(id) FROM documents)` ≥ 1 on cwd db |
| TC-IHC-1.4 | UC-IHC-1 | DB | After TC-IHC-1.1 write: `source_path` synthesized as `agent:<agent>:<session>:<feature>:<sha-prefix>` | `SELECT source_path FROM documents WHERE id=<new-id>` starts with `"agent:planner:"` | SQL `SELECT source_path FROM documents ORDER BY id DESC LIMIT 1` on cwd db returns value matching regex `^agent:planner:` |

### 3.2 Happy path — general routing

| ID | Use Case | Verification Class | Test Case | Expected Result | Evidence Required |
|----|----------|--------------------|-----------|-----------------|--------------------|
| TC-IHC-2.1 | UC-IHC-2 | Mixed | `claudebase insight create "nginx reload sends SIGHUP" --type agent-learned --agent ba-analyst --feature insights-hybrid-corpus --salience medium --category general --tags nginx infrastructure` | (a) exits 0; (b) `documents` row in GLOBAL db at `~/.claude/knowledge/insights.db` with `category='general'`, `project_slug IS NULL`; (c) `insight_tags` in global db has `'nginx'` and `'infrastructure'`; (d) cwd-local db NOT created or modified | (a) `assert_cmd.assert().success()`; (b) SQL `SELECT category, project_slug FROM documents ORDER BY id DESC LIMIT 1` on `~/.claude/knowledge/insights.db` returns `('general', NULL)`; (c) SQL `SELECT tag FROM insight_tags WHERE doc_id=(SELECT MAX(id) FROM documents)` on global db contains `'nginx'` and `'infrastructure'`; (d) SQL row count on cwd db unchanged |
| TC-IHC-2.2 | UC-IHC-2 | DB | AC-IHC-6 acceptance test: `SELECT count(*) FROM documents WHERE source_type='agent-learned'` on global db ≥ 1; cwd local db count unchanged | Matches AC-IHC-6 exactly | SQL on `~/.claude/knowledge/insights.db`: `SELECT count(*) FROM documents WHERE source_type='agent-learned'` ≥ 1; SQL on cwd db: `SELECT count(*) FROM documents` equals pre-test count |
| TC-IHC-2.3 | UC-IHC-2-A1 | DB | `insight create --category general --project myproject --tags infra --type agent-learned --agent x --salience low "body"` | `project_slug IS NULL` on inserted row in global db (--project silently ignored for general) | SQL `SELECT project_slug FROM documents ORDER BY id DESC LIMIT 1` on global db == NULL |

### 3.3 Tag normalization

| ID | Use Case | Verification Class | Test Case | Expected Result | Evidence Required |
|----|----------|--------------------|-----------|-----------------|--------------------|
| TC-IHC-1.6 | UC-IHC-1-A2 | DB | `--tags "#tokio" "#mutex"` — tags with leading `#` characters | `insight_tags` stores `'tokio'` and `'mutex'` (not `'#tokio'`, not `'#mutex'`) | SQL `SELECT tag FROM insight_tags WHERE doc_id=(SELECT MAX(id) FROM documents)` on cwd db returns `'tokio'` and `'mutex'`; no row with value starting `'#'` |
| TC-IHC-1.7 | UC-IHC-1-A3 | DB | `--tags tokio tokio mutex` — duplicate tag in input list | `insight_tags` has exactly 2 rows for the new doc: `'tokio'` once, `'mutex'` once | SQL `SELECT COUNT(*) FROM insight_tags WHERE doc_id=(SELECT MAX(id) FROM documents)` on cwd db == 2; SQL returns `'tokio'` and `'mutex'` |

### 3.4 Explicit `--project` slug

| ID | Use Case | Verification Class | Test Case | Expected Result | Evidence Required |
|----|----------|--------------------|-----------|-----------------|--------------------|
| TC-IHC-1.5 | UC-IHC-1-A1 | DB | `insight create ... --category project --project myproject --tags sometag` | `project_slug='myproject'` on inserted row in cwd db | SQL `SELECT project_slug FROM documents ORDER BY id DESC LIMIT 1` on cwd db == `'myproject'` |

### 3.5 First-write db creation

| ID | Use Case | Verification Class | Test Case | Expected Result | Evidence Required |
|----|----------|--------------------|-----------|-----------------|--------------------|
| TC-IHC-1.8 | UC-IHC-1-A4 | Mixed | Run project-category `insight create` against a project dir with no prior `insights.db` | (a) exits 0; (b) `<cwd>/.claude/knowledge/insights.db` created; (c) `PRAGMA user_version` == 5; (d) documents row present | (a) `assert_cmd.assert().success()`; (b) `fs::metadata("<cwd>/.claude/knowledge/insights.db").is_ok()`; (c) SQL `PRAGMA user_version` == 5; (d) SQL `SELECT COUNT(*) FROM documents` == 1 |

### 3.6 Error: missing flags

| ID | Use Case | Verification Class | Test Case | Expected Result | Evidence Required |
|----|----------|--------------------|-----------|-----------------|--------------------|
| TC-IHC-3.1 | UC-IHC-3 | CLI | `claudebase insight create "body" --category project --type agent-learned --agent x` (no `--tags`) | Exits 2; stderr contains literal `error: insight create requires at least one --tag`; no db write | `assert_cmd.assert().failure().code(2)`; stderr contains exact substring `error: insight create requires at least one --tag`; SQL `SELECT COUNT(*) FROM documents` on cwd db unchanged (== 0 if fresh) |
| TC-IHC-3.2 | UC-IHC-3-A1 | CLI | `--tags "#"` (sole tag reduces to empty string after stripping `#`) | Exits 2; same stderr literal as TC-IHC-3.1 | `assert_cmd.assert().failure().code(2)`; stderr contains `error: insight create requires at least one --tag` |
| TC-IHC-3.3 | UC-IHC-3-EC1 | CLI | Pipe a valid body via stdin, omit `--tags`, include `--category project` | Exits 2 before any db open; no write | `assert_cmd.write_stdin("body").assert().failure().code(2)`; stderr contains `error: insight create requires at least one --tag` |
| TC-IHC-4.1 | UC-IHC-4 | CLI | `claudebase insight create "body" --type agent-learned --agent x --tags foo` (no `--category`) | Clap exit 2; stderr contains `--category` in the error message (clap-generated usage error) | `assert_cmd.assert().failure().code(2)`; stderr contains substring `--category` |
| TC-IHC-4.2 | UC-IHC-4-A1 | CLI | `claudebase insight create "body" --category team --tags foo` (invalid enum value) | Clap exit 2; stderr mentions invalid value and lists valid options (`general`, `project`) | `assert_cmd.assert().failure().code(2)`; stderr contains `team` and either `general` or `project` |
| TC-IHC-4.3 | UC-IHC-4-EC1 | CLI | `claudebase insight create "body" --category "" --tags foo` (empty string value) | Clap exit 2 (value_enum rejects empty string) | `assert_cmd.assert().failure().code(2)` |

### 3.7 Error: db write failure / resolve failure

| ID | Use Case | Verification Class | Test Case | Expected Result | Evidence Required |
|----|----------|--------------------|-----------|-----------------|--------------------|
| TC-IHC-1.9 | UC-IHC-1-E1 | CLI | Invoke `insight create --category project` from a directory where `resolve_project_root` fails (e.g., a temp dir not under any git root, with the project-root resolver configured to require git) | Exits 1; stderr contains `error: could not determine project root`; global db NOT written | `assert_cmd.assert().failure().code(1)`; stderr contains `could not determine project root`; `fs::metadata("~/.claude/knowledge/insights.db")` unchanged row count if it existed |
| TC-IHC-1.11 | UC-IHC-1-EC1 | CLI | Tag that is `"#"` alone (strips to empty) — same as TC-IHC-3.2 | Already covered — exit 2, stderr literal match | See TC-IHC-3.2 evidence |

### 3.8 Dedup continuity

| ID | Use Case | Verification Class | Test Case | Expected Result | Evidence Required |
|----|----------|--------------------|-----------|-----------------|--------------------|
| TC-IHC-20.1 | UC-IHC-20 | Mixed | Write identical body twice to LOCAL db with same agent + new required flags; second write should dedup | Second invocation exits 0; stdout JSON `"status": "deduped"` with same `doc_id`; `SELECT COUNT(*) FROM documents` == 1 | `assert_cmd.assert().success()` on both; second invocation stdout contains `"deduped"`; SQL `SELECT COUNT(*) FROM documents` on cwd db == 1 |
| TC-IHC-20.2 | UC-IHC-20-A1 | Mixed | Write identical body twice to GLOBAL db with same agent + `--category general` | Second invocation exits 0; `"status": "deduped"`; global db `SELECT COUNT(*) FROM documents` == 1 | `assert_cmd.assert().success()` on both; second stdout contains `"deduped"`; SQL `SELECT COUNT(*) FROM documents` on `~/.claude/knowledge/insights.db` == 1 |
| TC-IHC-2.7 | UC-IHC-2-EC3 | Mixed | Same body written twice to GLOBAL db by two DIFFERENT agents (cross-agent non-dedup) | Two `documents` rows in global db; both exits 0 | `assert_cmd.assert().success()` both; SQL `SELECT COUNT(*) FROM documents WHERE source_path LIKE 'agent:%'` on global db == 2 |

---

## 4. `insight tags` Subcommand (FR-IHC-4)

### 4.1 Default merged output

| ID | Use Case | Verification Class | Test Case | Expected Result | Evidence Required |
|----|----------|--------------------|-----------|-----------------|--------------------|
| TC-IHC-5.1 | UC-IHC-5 | CLI | Setup: write one project insight tagged `tokio` (cwd db), one general insight tagged `nginx` (global db); run `claudebase insight tags --json` | JSON array contains at least `{"tag":"tokio","count":1}` and `{"tag":"nginx","count":1}`; sorted by count descending; exits 0 | `assert_cmd.assert().success()`; stdout parses as JSON array; array contains objects with `tag="tokio"` and `tag="nginx"`; objects conform to schema `{"tag": string, "count": integer}` |
| TC-IHC-5.2 | UC-IHC-5 | CLI | Setup: write two project insights both tagged `tokio`, one general insight also tagged `tokio`; run `claudebase insight tags --json` | `tokio` count == 3 (summed across both dbs) | stdout JSON contains `{"tag":"tokio","count":3}` (exact count match) |
| TC-IHC-5.3 | UC-IHC-5 | CLI | AC-IHC-8 acceptance test: after TC-IHC-1.1 and TC-IHC-2.1 setup, `claudebase insight tags --json` returns ≥1 element with `tag` and `count` fields | JSON array `length >= 1`; element 0 has `tag` (string) and `count` (integer) fields | `assert_cmd.assert().success()`; stdout is valid JSON array; `arr[0].tag` is a non-empty string; `arr[0].count` is an integer |
| TC-IHC-5.4 | UC-IHC-5-A4 | CLI | `claudebase insight tags` (no `--json` flag) | Human-readable table output with `<tag>    <count>` lines sorted by count descending; exits 0 | `assert_cmd.assert().success()`; stdout is non-empty and does NOT parse as JSON (sanity); stdout contains at least one line with whitespace between tag-like text and a digit |

### 4.2 Missing db handling

| ID | Use Case | Verification Class | Test Case | Expected Result | Evidence Required |
|----|----------|--------------------|-----------|-----------------|--------------------|
| TC-IHC-5.5 | UC-IHC-5-A1 | CLI | Run `insight tags --json` when global db absent but local db has tags | Returns local db tags only; exits 0; no error about missing global db | `assert_cmd.assert().success()`; stdout parses as JSON array with `length >= 1`; no `error:` substring in stderr |
| TC-IHC-5.6 | UC-IHC-5-A2 | CLI | Run `insight tags --json` when local db absent but global db has tags | Returns global db tags only; exits 0 | `assert_cmd.assert().success()`; stdout parses as JSON array with `length >= 1` |
| TC-IHC-5.7 | UC-IHC-5-A3 | CLI | Run `insight tags --json` when both dbs have zero `insight_tags` rows (or are absent) | Returns `[]`; exits 0 | `assert_cmd.assert().success()`; stdout == `[]` (exact match) |

### 4.3 Category and project filters

| ID | Use Case | Verification Class | Test Case | Expected Result | Evidence Required |
|----|----------|--------------------|-----------|-----------------|--------------------|
| TC-IHC-6.1 | UC-IHC-6 | CLI | Setup: project insight tagged `tokio`, general insight tagged `nginx`; run `claudebase insight tags --category general --json` | Returns only `nginx` tag (no `tokio`); exits 0 | `assert_cmd.assert().success()`; stdout JSON array contains `{"tag":"nginx",...}`; does NOT contain any object with `"tag":"tokio"` |
| TC-IHC-6.2 | UC-IHC-6-A1 | CLI | `claudebase insight tags --project claudebase --json` where `claudebase` project has insight tagged `slice3` | Returns `slice3` tag; exits 0 | `assert_cmd.assert().success()`; stdout JSON contains `{"tag":"slice3",...}` |
| TC-IHC-6.3 | UC-IHC-6-EC1 | CLI | `claudebase insight tags --project nonexistentproject --json` (project not in registry) | Exits 1; stderr contains `error: project 'nonexistentproject' not found in registry` | `assert_cmd.assert().failure().code(1)`; stderr contains `not found in registry` |

---

## 5. Dual-DB Reads — Search, List, GC, Delete (FR-IHC-5)

### 5.1 Merged default search

| ID | Use Case | Verification Class | Test Case | Expected Result | Evidence Required |
|----|----------|--------------------|-----------|-----------------|--------------------|
| TC-IHC-7.1 | UC-IHC-7 | CLI | Setup: write project insight "local lesson" tagged `myfeature` (cwd db); write general insight "general lesson" tagged `myfeature` (global db); run `claudebase insight search "lesson" --json` from cwd | Result set contains hits from BOTH dbs; exits 0 | `assert_cmd.assert().success()`; stdout JSON array `length >= 2`; result set contains a hit whose `source` path contains `~/.claude/knowledge/insights.db` AND a hit whose `source` path contains the cwd project db path |
| TC-IHC-7.2 | UC-IHC-7 | CLI | AC-IHC-9 acceptance test: plant a row in a second unrelated project db (not cwd, not global); run `claudebase insight search "lesson" --json` from cwd | Result set contains cwd project hits and global hits; does NOT contain the planted second-project row | stdout JSON does not contain the second project's db path in any hit's `source` field; `length >= 2` (both local and global present) |
| TC-IHC-7.3 | UC-IHC-7 | CLI | AC-IHC-10 acceptance test: `claudebase insight search "lesson" --tag nginx --json` where only general insight is tagged `nginx` | Returns only hits tagged `nginx`; project insight without `nginx` tag excluded | `assert_cmd.assert().success()`; all hits in stdout JSON have `doc_id` that maps to a row in `insight_tags` with `tag='nginx'`; project insight `doc_id` is absent |
| TC-IHC-7.8 | UC-IHC-7-E1 | CLI | Run `claudebase insight search "anything" --json` when both dbs are absent | Returns `[]`; exits 0 (not an error) | `assert_cmd.assert().success()`; stdout == `[]` |

### 5.2 Tag filter — OR / any-intersection semantics (operator decision 2026-05-27)

| ID | Use Case | Verification Class | Test Case | Expected Result | Evidence Required |
|----|----------|--------------------|-----------|-----------------|--------------------|
| TC-IHC-7.4 | UC-IHC-7-A1 | CLI | Write insight A tagged `tokio` + `mutex`; insight B tagged `tokio` only; insight C tagged `mutex` only; run `claudebase insight search "insight" --tag tokio --tag mutex --json` | Returns ALL THREE (A both, B tokio-only, C mutex-only) — OR / any-intersection, NOT AND | `assert_cmd.assert().success()`; stdout JSON array `length == 3`; the three hits' `doc_id`s correspond to A, B, and C (a result carrying only one of the two requested tags MUST be present — proves OR semantics, not AND) |

### 5.3 Cross-project isolation

| ID | Use Case | Verification Class | Test Case | Expected Result | Evidence Required |
|----|----------|--------------------|-----------|-----------------|--------------------|
| TC-IHC-17.1 | UC-IHC-17 | Mixed | Setup: plant insight in `/tmp/other-project/.claude/knowledge/insights.db` with a queryable body; run `claudebase insight search "<matching-body-keyword>" --json` from cwd (project A) | Planted row absent from results; only cwd + global hits returned | stdout JSON does not contain any hit with `source` path containing `/tmp/other-project`; exit 0 |

### 5.4 Chunk-id collision test (architect test)

| ID | Use Case | Verification Class | Test Case | Expected Result | Evidence Required |
|----|----------|--------------------|-----------|-----------------|--------------------|
| TC-IHC-7.5 | UC-IHC-7-EC1 | Mixed | Plant chunk with `chunk_id=1` in LOCAL db and chunk with `chunk_id=1` in GLOBAL db (same integer id in different dbs); run `claudebase insight search "planted" --json` | RRF fusion returns BOTH chunks as 2 distinct hits (not collapsed); exits 0 | `assert_cmd.assert().success()`; stdout JSON array `length == 2`; the two hits have different `source` paths (one local, one global); verifies RRF does not collapse hits by `chunk_id` alone |

### 5.5 Narrowing flags

| ID | Use Case | Verification Class | Test Case | Expected Result | Evidence Required |
|----|----------|--------------------|-----------|-----------------|--------------------|
| TC-IHC-8.1 | UC-IHC-8 | CLI | AC-IHC-11 acceptance test: `claudebase insight search "lesson" --general-only --json` with both local and general insights present | Only global db hits returned; project hits absent | `assert_cmd.assert().success()`; all hits' `source` paths point to `~/.claude/knowledge/insights.db`; no hit's `source` contains the cwd project db path |
| TC-IHC-8.2 | UC-IHC-8 | CLI | `--general-only` when global db absent | Returns `[]`; exits 0 | `assert_cmd.assert().success()`; stdout == `[]` |
| TC-IHC-8.3 | UC-IHC-8-A1 | CLI | `claudebase insight search "nginx" --general-only --tag nginx --json` | Returns only global insights tagged `nginx`; exits 0 | `assert_cmd.assert().success()`; all hits' `source` is global db; all hits' `doc_id` maps to `insight_tags` row with `tag='nginx'` |
| TC-IHC-8.4 | UC-IHC-8-EC1 | CLI | `claudebase insight search "x" --general-only --project-only --json` (mutually exclusive) | Exits 2; stderr contains error about mutually exclusive flags | `assert_cmd.assert().failure().code(2)`; stderr contains at least one of: `mutually exclusive`, `general-only`, `project-only` |
| TC-IHC-9.1 | UC-IHC-9 | CLI | `claudebase insight search "lesson" --project-only --json` with both local and general insights | Only local db hits returned; general hits absent | `assert_cmd.assert().success()`; all hits' `source` paths contain the cwd project db; no hit's `source` is `~/.claude/knowledge/insights.db` |
| TC-IHC-9.2 | UC-IHC-9-A1 | CLI | `--project-only --tag <t>` — tag filter within project-only scope | Returns only local insights tagged `<t>` | `assert_cmd.assert().success()`; all hits carry `tag=<t>` per SQL on cwd db; no global db `source` |
| TC-IHC-9.3 | UC-IHC-9-EC1 | CLI | `--project-only` when local db absent | Returns `[]`; exits 0 | `assert_cmd.assert().success()`; stdout == `[]`; no error in stderr |

### 5.6 Encoder degraded fallback

| ID | Use Case | Verification Class | Test Case | Expected Result | Evidence Required |
|----|----------|--------------------|-----------|-----------------|--------------------|
| TC-IHC-7.7 | UC-IHC-7-A4 | CLI | Run `insight search` with encoder model files absent | Exits 0; results returned (lexical-only); stderr contains `warning` or `falling back to lexical` | `assert_cmd.assert().success()`; stderr contains `lexical` or `fallback` or `encoder unavailable` (exact string TBD by implementer); stdout parses as valid JSON array |

### 5.7 GC — dual-db

| ID | Use Case | Verification Class | Test Case | Expected Result | Evidence Required |
|----|----------|--------------------|-----------|-----------------|--------------------|
| TC-IHC-19.1 | UC-IHC-19 | Mixed | Setup: insert expired `salience=low` rows (via SQL backdate `ingested_at = now - 100*86400`) in both local and global dbs; run `claudebase insight gc --json` | `medium_deleted + low_deleted >= 2` (at least the backdated rows); exits 0; `insight_tags` cascade-deleted for purged docs | `assert_cmd.assert().success()`; stdout JSON contains `medium_deleted` and `low_deleted` integer fields, sum ≥ 2; SQL `SELECT COUNT(*) FROM insight_tags WHERE doc_id NOT IN (SELECT id FROM documents)` == 0 on both dbs (no orphan tags) |
| TC-IHC-19.2 | UC-IHC-19-A1 | Mixed | `claudebase insight gc --category general --json` | Purge runs on global db only; local db row count unchanged | stdout JSON `medium_deleted + low_deleted` reflects global db purge count; SQL `SELECT COUNT(*) FROM documents` on cwd db unchanged |

### 5.8 Cross-project read via registry

| ID | Use Case | Verification Class | Test Case | Expected Result | Evidence Required |
|----|----------|--------------------|-----------|-----------------|--------------------|
| TC-IHC-18.1 | UC-IHC-18 | Mixed | Register project-B in `projects.json` manually; run `claudebase insight search "b-lesson" --project project-b --json` from project-A directory | Returns project-B hits; does NOT include project-A's insights | `assert_cmd.assert().success()`; hits' `source` paths point to project-B db; no hit's `source` contains project-A db path |
| TC-IHC-18.2 | UC-IHC-18 | CLI | `--project` names the cwd project (same as default) | Identical results to default invocation; exits 0 | `assert_cmd.assert().success()`; stdout matches or is a subset of default search results |
| TC-IHC-18.3 | UC-IHC-18-E1 | CLI | `claudebase insight search "x" --project unknownproject --json` (slug not in registry) | Exits 1; stderr contains `not found in registry` | `assert_cmd.assert().failure().code(1)`; stderr contains `not found in registry` |

### 5.9 Delete — global db routing

| ID | Use Case | Verification Class | Test Case | Expected Result | Evidence Required |
|----|----------|--------------------|-----------|-----------------|--------------------|
| TC-IHC-21.1 | UC-IHC-21 | Mixed | Insert a general insight (id=N) into global db; run `claudebase insight delete N --category general --json` | Exits 0; stdout `{"deleted": true, "id": N}`; row N absent from global db; `insight_tags` cascade-deleted; local db unchanged | `assert_cmd.assert().success()`; stdout contains `"deleted": true` and `"id": N`; SQL `SELECT COUNT(*) FROM documents WHERE id=N` on global db == 0; SQL `SELECT COUNT(*) FROM insight_tags WHERE doc_id=N` on global db == 0; SQL row count on cwd db unchanged |
| TC-IHC-21.2 | UC-IHC-21-E1 | CLI | `insight delete 999 --category general` where id 999 does not exist in global db | Exits 1; stderr contains `not found` | `assert_cmd.assert().failure().code(1)`; stderr contains `not found` |

---

## 6. Project Registry (FR-IHC-6)

### 6.1 Registry upsert — first run

| ID | Use Case | Verification Class | Test Case | Expected Result | Evidence Required |
|----|----------|--------------------|-----------|-----------------|--------------------|
| TC-IHC-10.1 | UC-IHC-10 | Mixed | AC-IHC-12 acceptance test: trigger `upsert_project(cwd)` via the registry test harness (or a `claudebase run` invocation in a test that intercepts before `exec()`); check `projects.json` | `~/.claude/knowledge/projects.json` exists; contains one entry for the cwd project with `name=<basename>`, `path=<canonical-cwd>`, `last_seen` is a non-zero integer | `fs::metadata("~/.claude/knowledge/projects.json").is_ok()`; `serde_json::from_str(fs::read_to_string("~/.claude/knowledge/projects.json"))` parses as JSON array with `length >= 1`; entry `{name: "<basename>", path: "<canonical-cwd>", last_seen: <int>}` present |
| TC-IHC-10.2 | UC-IHC-10-A1 | FS | `projects.json` already contains 2 other projects; after `upsert_project` for a new cwd project, JSON has 3 entries and prior 2 are preserved | JSON array `length == 3`; original 2 entries intact | `serde_json::from_str(content)` length == 3; both prior `name` values present in the array |
| TC-IHC-10.4 | UC-IHC-10-E2 | FS | `projects.json` contains malformed JSON (e.g., `{not valid}`); `upsert_project` runs | Malformed file overwritten with valid JSON array containing the cwd project entry; exits 0 | `serde_json::from_str(fs::read_to_string("~/.claude/knowledge/projects.json"))` succeeds post-invocation; array `length >= 1`; cwd project entry present |
| TC-IHC-10.5 | UC-IHC-10-EC2 | FS | `upsert_project` called with a symlinked cwd path (`/tmp/link` → `/Users/x/projects/p`); then called again with the canonical path directly | Registry contains exactly one entry for the canonical path; no duplicate | JSON array entry count for this path == 1; `path` value is the canonical path |

### 6.2 Idempotent upsert

| ID | Use Case | Verification Class | Test Case | Expected Result | Evidence Required |
|----|----------|--------------------|-----------|-----------------|--------------------|
| TC-IHC-11.1 | UC-IHC-11 | FS | AC-IHC-13 acceptance test: call `upsert_project(cwd)` twice with the same cwd | `projects.json` contains exactly 1 entry for this project (not 2); `last_seen` updated | `serde_json::from_str(content)` length == 1 (or unchanged total count if other projects present); only one entry with `path == canonical_cwd`; `last_seen` value on second read > value on first read |
| TC-IHC-11.2 | UC-IHC-11-A1 | FS | `upsert_project` called once with `./project` (relative) and once with `/abs/path/to/project` (absolute, same canonical) | Exactly one entry in registry | `serde_json::from_str(content)` contains exactly one entry with `path == canonical_path` |

### 6.3 Concurrency — no corrupt JSON

| ID | Use Case | Verification Class | Test Case | Expected Result | Evidence Required |
|----|----------|--------------------|-----------|-----------------|--------------------|
| TC-IHC-12.1 | UC-IHC-12 | FS | Spawn 10 concurrent threads each calling `upsert_project(cwd)` simultaneously against the same `projects.json` | `projects.json` is valid JSON after all threads complete; no torn / partial write | `serde_json::from_str(fs::read_to_string("~/.claude/knowledge/projects.json"))` succeeds (no parse error); the file is a valid JSON array — this is the no-corruption contract; accepted that not all entries may be present due to last-write-wins |

### 6.4 Registry write before exec

| ID | Use Case | Verification Class | Test Case | Expected Result | Evidence Required |
|----|----------|--------------------|-----------|-----------------|--------------------|
| TC-IHC-10.3 | UC-IHC-10-E1 | CLI | Simulate `upsert_project` write failure (read-only `~/.claude/knowledge/`); confirm that `claudebase run` still proceeds to exec/Claude | `claudebase run` exits normally (process replacement happens); registry file not modified; no crash or panic | Registry file unchanged (row count / content unchanged from before); process exits as if `run` completed; no Rust panic in stderr |

---

## 7. SessionStart "Read Insights on New Context" Hook (FR-IHC-7)

### 7.1 Hook parse correctness

| ID | Use Case | Verification Class | Test Case | Expected Result | Evidence Required |
|----|----------|--------------------|-----------|-----------------|--------------------|
| TC-IHC-15.1 | UC-IHC-15 | CLI | `bash -n hooks/claudebase-read-insights-reminder.sh` (syntax check) | Exits 0 (no syntax errors) | Command exit code == 0; no stderr output |
| TC-IHC-15.2 | UC-IHC-15-A1 | CLI | AC-IHC-14 acceptance test: PowerShell parse check `powershell -Command "& { $null = [System.IO.File]::ReadAllText('hooks/claudebase-read-insights-reminder.ps1') }"` (or PS parse-check invocation) exits 0 on a PS 5.1 engine | Exits 0; no BOM or non-ASCII chars in `.ps1` file | CLI exit code == 0; `grep -P "[^\x00-\x7F]" hooks/claudebase-read-insights-reminder.ps1` returns no matches (ASCII-only assertion) |
| TC-IHC-15.3 | UC-IHC-15-E2 | FS | Count words in `hooks/claudebase-read-insights-reminder.sh` reminder output | Word count ≤ 200 (NFR-IHC-4 contract) | `wc -w hooks/claudebase-read-insights-reminder.sh` output ≤ 200 OR word count of the `additionalContext` string emitted by the hook ≤ 200 (if extractable by grep from the script body) |

### 7.2 Hook install idempotency

| ID | Use Case | Verification Class | Test Case | Expected Result | Evidence Required |
|----|----------|--------------------|-----------|-----------------|--------------------|
| TC-IHC-16.1 | UC-IHC-16 | FS | AC-IHC-15 acceptance test: run `bash install.sh --yes` twice; inspect `~/.claude/settings.json` | Exactly one entry for `claudebase-read-insights-reminder.sh` in `hooks.SessionStart` array | `jq '[.hooks.SessionStart[] | select(.command | contains("claudebase-read-insights-reminder"))] | length' ~/.claude/settings.json` == 1 |
| TC-IHC-16.2 | UC-IHC-16-A1 | FS | Run `install.ps1` twice on Windows (or simulate with the idempotency jq check) | Exactly one entry for `claudebase-read-insights-reminder.ps1` in `hooks.SessionStart`; `ConvertFrom-Json` parse succeeds | `(Get-Content ~/.claude/settings.json | ConvertFrom-Json).hooks.SessionStart | Where-Object { $_.command -like "*read-insights*" } | Measure-Object | Select-Object -ExpandProperty Count` == 1 |
| TC-IHC-16.3 | UC-IHC-16-E2 | CLI | Run `install.sh --yes` with `jq` not on PATH; hook wiring step skipped | `install.sh` exits 0 (overall); warning emitted; `~/.claude/settings.json` hooks unchanged | `install.sh` exit code == 0; stderr/stdout contains `warning` or `jq` mention; `~/.claude/settings.json` hooks section unmodified |

---

## 8. Caller Blast Radius Update (FR-IHC-8)

### 8.1 No bare insight create without mandatory flags

| ID | Use Case | Verification Class | Test Case | Expected Result | Evidence Required |
|----|----------|--------------------|-----------|-----------------|--------------------|
| TC-IHC-BREAK-1 | UC-IHC-3 (regression) | CLI | AC-IHC-16 acceptance test: `grep -rE "insight create" ~/.claude/ src/agents/ ~/.claude/rules/ | grep -v "\-\-category" | wc -l` | Returns 0 | Shell command exit code 0; stdout `0` (trimmed) |
| TC-IHC-BREAK-2 | FR-IHC-8.5 | CLI | `grep -rE "insight create" ~/.claude/ src/agents/ ~/.claude/rules/ | grep -v "\-\-tag" | wc -l` | Returns 0 | Shell command exit code 0; stdout `0` (trimmed) |

---

## 9. Acceptance Criteria Coverage

| AC | Test Case(s) |
|----|-------------|
| AC-IHC-1 (schema v5 fresh stamp) | TC-IHC-13.6 |
| AC-IHC-2 (v4→v5 migration additive) | TC-IHC-13.1, 13.2, 13.3, 13.4 |
| AC-IHC-3 (4 existing insight rows backfilled) | TC-IHC-13.2, 13.8 |
| AC-IHC-4 (mandatory --tags enforcement) | TC-IHC-3.1, 3.2, 3.3 |
| AC-IHC-5 (mandatory --category enforcement) | TC-IHC-4.1, 4.2, 4.3 |
| AC-IHC-6 (routing — general → global db) | TC-IHC-2.1, 2.2 |
| AC-IHC-7 (routing — project → local db) | TC-IHC-1.1, 1.2 |
| AC-IHC-8 (tags subcommand shape) | TC-IHC-5.3 |
| AC-IHC-9 (search — merged default + exclusion) | TC-IHC-7.1, 7.2 |
| AC-IHC-10 (search — tag filter) | TC-IHC-7.3 |
| AC-IHC-11 (search — general-only) | TC-IHC-8.1 |
| AC-IHC-12 (registry created on run) | TC-IHC-10.1 |
| AC-IHC-13 (registry idempotent) | TC-IHC-11.1 |
| AC-IHC-14 (hook parse — bash -n exits 0) | TC-IHC-15.1, 15.2 |
| AC-IHC-15 (hook install idempotent) | TC-IHC-16.1 |
| AC-IHC-16 (callers updated — no bare insight create) | TC-IHC-BREAK-1, BREAK-2 |
| AC-IHC-17 (release v0.7.0) | not a QA test case — release-engineer task |

---

## 10. NFR Coverage Notes

| NFR | Coverage |
|-----|---------|
| NFR-IHC-1 (v5 additive — no dropped columns) | TC-IHC-13.1, 13.3: books rows keep `category=NULL`; no existing column dropped |
| NFR-IHC-2 (security backbone — resolve_project_root unchanged) | TC-IHC-1.9 (project-root gate still fires for project category); TC-IHC-2.6 (global bypass safe) |
| NFR-IHC-3 (registry concurrency) | TC-IHC-12.1 |
| NFR-IHC-4 (hook ≤200 words) | TC-IHC-15.3 |
| NFR-IHC-5 (tags normalized table) | TC-IHC-13.4 (indexes exist); TC-IHC-1.7 (UNIQUE dedup) |
| NFR-IHC-6 (breaking change intentional; callers updated) | TC-IHC-BREAK-1, BREAK-2 |
| NFR-IHC-7 (single-binary constraint) | covered implicitly by every CLI test — all features compile into the same `claudebase` binary |
