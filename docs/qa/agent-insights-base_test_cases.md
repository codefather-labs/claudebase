# QA Test Cases — Agent Insights Base

PRD reference: `docs/PRD.md` §16. Use cases: `docs/use-cases/agent-insights-base_use_cases.md`.

## Facts

### Verified facts

- All 18 DONE test cases below have implementations in `tests/cli_insight_e2e_test.rs` and pass at commit `e7bcc1c` — verified by `cargo test --test cli_insight_e2e_test` returning 19/19 pass (one extra test covers `create_emits_deduped_status_on_second_call`) — salience: high.
- Each row carries a `Verification Class` and `Evidence Required` per the Plan-Critic QA-strictness contract — salience: high.

### External contracts

- **`assert_cmd::Command`** — symbol: `Command::cargo_bin("claudebase").assert().success()/failure().code(N)` — source: `tests/cli_insight_e2e_test.rs` — verified: yes — salience: medium.
- **`tempfile::tempdir`** — symbol: per-test tempdir as project root — source: `tests/cli_insight_e2e_test.rs::fresh_project` — verified: yes — salience: low.

### Assumptions

- The CLI exit-code contract (0 success, 1 runtime error, 2 usage error) is honored by every subcommand. Risk: a future refactor could break this. How to verify: every test asserts `.success()` or `.failure().code(N)` so a code-drift surfaces immediately. Salience: medium.

### Open questions

(none)

## Decisions

### Inbound validation

(none) — test cases were derived directly from the use-cases file (1:1 mapping).

### Decisions made

- Verification class `CLI` is used for all DONE test cases — claudebase has no UI surface and the binary is the only integration point. `DB` rows referenced in `Evidence Required` are verified via direct `rusqlite::Connection::open(&path).query_row(...)` calls within the test, NOT via a separate `Mixed` classification. Salience: medium.
- PLANNED test cases (TC-AIB-9..12) are scoped but left as `Status: planned` so the test plan stays honest about coverage state. Salience: medium.

### Hacks acknowledged

(none)

### Symptom-only patches (with root-cause links)

(none)

## Use Case Coverage

| Use Case | Test Case(s) | Status |
|----------|--------------|--------|
| UC-AIB-1 | TC-AIB-1.1, 1.2 | DONE |
| UC-AIB-2 | TC-AIB-2.1, 2.2 | DONE |
| UC-AIB-3 | TC-AIB-3.1 | DONE |
| UC-AIB-4 | TC-AIB-4.1 | DONE |
| UC-AIB-5 | TC-AIB-5.1, 5.2, 5.3 | DONE |
| UC-AIB-6 | TC-AIB-6.1 | DONE |
| UC-AIB-7 | TC-AIB-7.1, 7.2, 7.3, 7.4, 7.5 | DONE |
| UC-AIB-8 | TC-AIB-8.1 | DONE |
| UC-AIB-9  (planned) | TC-AIB-9.1 | planned |
| UC-AIB-10 (planned) | TC-AIB-10.1 | planned |
| UC-AIB-11 (planned) | TC-AIB-11.1 | planned |
| UC-AIB-12 (planned) | TC-AIB-12.1 | planned |
| UC-AIB-EC-1 | TC-AIB-EC-1 | DONE |
| UC-AIB-EC-2 | TC-AIB-EC-2 | planned (manual smoke) |
| UC-AIB-EC-3 | TC-AIB-EC-3 | implicit in CI when encoder model is uncached |
| UC-AIB-EC-4 | TC-AIB-EC-4 | DONE |
| UC-AIB-EC-5 | TC-AIB-EC-5a, 5b | DONE |

## Test Cases

| ID | Description | Verification Class | Evidence Required | Status |
|----|-------------|-------------------|--------------------|--------|
| TC-AIB-1.1 | `insight create` happy path writes one doc + ≥1 chunk to `insights.db` with the six v4 metadata columns populated. | CLI | `assert_cmd.assert().success()`; DB row count `documents=1`, `chunks≥1` via `rusqlite::query_row`; columns `source_type='agent-learned'`, `agent_name='reflection'`, `salience='high'` literal match. Test: `create_writes_one_doc_with_v4_metadata`. | DONE |
| TC-AIB-1.2 | `insight create` reads body from stdin when positional arg omitted. | CLI | `Command.write_stdin("body").assert().success()`; DB row count `documents=1`. Test: `create_reads_stdin_when_body_arg_omitted`. | DONE |
| TC-AIB-2.1 | Exact-sha dedup keeps `documents` count at 1 for two identical writes with same agent. | CLI | Two `assert().success()` invocations; final `SELECT COUNT(*) = 1`. Test: `create_exact_sha_dedup_keeps_doc_count_at_one`. | DONE |
| TC-AIB-2.2 | Second identical write emits JSON `"status": "deduped"`. | CLI | `--json` stdout contains literal `"status": "deduped"` substring. Test: `create_emits_deduped_status_on_second_call`. | DONE |
| TC-AIB-3.1 | Cross-agent same body produces two documents (not deduped). | CLI | Two `assert().success()` invocations with distinct `--agent`; final `SELECT COUNT(*) = 2`. Test: `create_cross_agent_same_body_is_not_deduped`. | DONE |
| TC-AIB-4.1 | `insight search` with `--mode lexical` retrieves a written insight by keyword. | CLI | `--json` stdout contains literal substring from body. Test: `search_returns_lexical_hit_for_written_insight`. | DONE |
| TC-AIB-5.1 | `insight list --offset 0` returns 10 rows newest-first on a corpus of 12 insights. | CLI | `--json` stdout parses; `returned == 10`, `total == 12`, `page_size == 10`, `rows[0].snippet` contains the newest body marker. Test: `list_default_page_size_is_ten_newest_first`. | DONE |
| TC-AIB-5.2 | `insight list --offset 1` returns the remaining 2 rows. | CLI | `--json` stdout `returned == 2`. Test: `list_offset_one_returns_remaining_two`. | DONE |
| TC-AIB-5.3 | `insight list --agent <X>` returns only rows whose `agent_name = X`. | CLI | `--json` `total == 1`, stdout contains the alpha body and NOT the beta body. Test: `list_filter_by_agent_only_returns_matches`. | DONE |
| TC-AIB-6.1 | `insight random` returns one row when corpus is non-empty. | CLI | `--json` stdout has `id` field and `body` field. Test: `random_returns_one_row_when_corpus_non_empty`. | DONE |
| TC-AIB-7.1 | `insight get <integer-id>` returns the full record. | CLI | `--json` `id == queried_id`, `body` contains literal target string. Test: `get_by_integer_id_returns_full_record`. | DONE |
| TC-AIB-7.2 | `insight get <sha-prefix>` matches via `LIKE 'prefix%'`. | CLI | `assert().success()` on the prefix subcommand. Test: `get_by_sha_prefix_matches_via_like`. | DONE |
| TC-AIB-7.3 | `insight get` with unknown id exits 1. | CLI | `assert().failure().code(1)`. Test: `get_unknown_id_exits_1`. | DONE |
| TC-AIB-7.4 | `insight get` with `<4`-char non-numeric ident exits 2. | CLI | `assert().failure().code(2)`. Test: `get_short_non_numeric_ident_exits_2`. | DONE |
| TC-AIB-7.5 | `insight get` with non-hex ident exits 2. | CLI | `assert().failure().code(2)`. Test: `get_non_hex_ident_exits_2`. | DONE |
| TC-AIB-8.1 | `insight create` does NOT create or modify `index.db`. | CLI + FS | `fs::metadata(tmp.path().join(".claude/knowledge/index.db")).is_err()`. Test: `create_does_not_create_index_db`. | DONE |
| TC-AIB-EC-1 | Empty / whitespace-only body exits 2. | CLI | `assert().failure().code(2)`. Test: `create_rejects_empty_body_with_exit_2`. | DONE |
| TC-AIB-EC-4 | `insight random` on empty corpus exits 1. | CLI | `assert().failure().code(1)`. Test: `random_exits_1_on_empty_corpus`. | DONE |
| TC-AIB-EC-5a | `insight get abc` (too short) exits 2. | CLI | `assert().failure().code(2)`. Test: `get_short_non_numeric_ident_exits_2`. | DONE |
| TC-AIB-EC-5b | `insight get zzzzzz` (non-hex) exits 2. | CLI | `assert().failure().code(2)`. Test: `get_non_hex_ident_exits_2`. | DONE |
| TC-AIB-EXTRA-1 | Synthetic `source_path` encodes `agent:<agent>:<session>:<feature>:<sha-prefix>`. | DB | `SELECT source_path FROM documents WHERE agent_name='planner'` → literal `starts_with("agent:planner:sess-abc:agent-insights-base:")`. Test: `create_source_path_encodes_metadata_segments`. | DONE |
| TC-AIB-9.1 | (PLANNED — Slice 5) Semantic-near-duplicate insights skipped via cosine > 0.92 within 30 days. | CLI + DB | Two writes with paraphrased bodies; second emits `cf-dedup: near-duplicate of doc #N`; `SELECT COUNT(*) = 1`. | planned |
| TC-AIB-10.1 | (PLANNED — Slice 6) `search --corpus all` returns hits from both `index.db` and `insights.db` labeled by `source_corpus`. | CLI | `--json` stdout has `source_corpus: "books"` and `source_corpus: "insights"` rows in the same response. | planned |
| TC-AIB-11.1 | (PLANNED — Slice 7) `insight gc` purges `salience=low` rows older than 90 days; preserves `medium` (1y) and `high` (∞). | CLI + DB | Backdate three insights via direct SQL `UPDATE documents SET ingested_at = now-100*86400`; run `insight gc`; assert only the low-salience row is gone. | planned |
| TC-AIB-12.1 | (PLANNED — Slice 8) An SDLC agent (e.g., `planner`) running under `/develop-feature` cites prior insights in its `## Facts → Verified facts` block when prior insights match the current feature-slug. | CLI + Integration | Spawn `/develop-feature` with a seeded `insights.db`; grep the agent's stdout for `knowledge-base: agent:` citation prefix. | planned |

## Acceptance Criteria Coverage

| AC | Test Case |
|----|-----------|
| FR-AIB-1.1..1.4 (schema v4 migration) | covered by `tests/store_test.rs::fresh_db_has_four_tables`, `tests/store_v2_test.rs::schema_version_is_four_on_fresh_v2_db` |
| FR-AIB-2.1..2.3 (db-name parameterization) | exercised by every test that uses `--db-name insights.db` |
| FR-AIB-3.1..3.6 (insight create) | TC-AIB-1.1, 1.2, 2.1, 2.2, 3.1, 8.1, EC-1, EC-2, EXTRA-1 |
| FR-AIB-4.1..4.2 (insight search) | TC-AIB-4.1 |
| FR-AIB-4.3 (search filters — PLANNED) | TC-AIB-10.1 (cross-corpus) extends; filters get their own test in Slice 4 |
| FR-AIB-5.1..5.6 (list / random / get admin) | TC-AIB-5.1, 5.2, 5.3, 6.1, 7.1, 7.2, 7.3, 7.4, 7.5 |
| FR-AIB-6 (semantic dedup — PLANNED) | TC-AIB-9.1 |
| FR-AIB-7 (--corpus all — PLANNED) | TC-AIB-10.1 |
| FR-AIB-8 (gc + delete — PLANNED) | TC-AIB-11.1 |
| FR-AIB-9 (agent-prompt integration — PLANNED) | TC-AIB-12.1 |
| NFR-AIB-4 (books-corpus zero-touch) | TC-AIB-8.1 |
