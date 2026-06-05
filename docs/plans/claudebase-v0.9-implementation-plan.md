# Plan: claudebase v0.9 — Wave A + D Release Cut (port-forward v0.7 insights surface)

**Feature slug:** `claudebase-v0.9-cut`
**Owner:** Mira (orchestrator)
**Branch strategy:** continue on existing `feat/multi-agent-on-v0.6` from HEAD `15b9460` (38 commits beyond `claudebase-v0.6.0`)
**Date drafted:** 2026-06-04
**Source-of-truth doc:** `docs/plans/claudebase-v0.9-product-plan.md` (committed in `15b9460`)

## Context — why

v0.7 and v0.8 were declared ejected by operator after extended debug attempts («вся версия получилась косячной»). Branch `feat/multi-agent-on-v0.6` re-implemented the v0.7/v0.8 Telegram product surface from scratch on the v0.6 baseline with empirical step-by-step verification (KP1 LIVE-verified; Slice 8 chat_ask live-verified). Operator's scope decision 2026-06-04 narrowed v0.9 to TWO waves of work:

- **Wave A — port-forward v0.7's insights corpus + project registry + hooks + skill** as a **code REUSE exercise** (cherry-pick the actual v0.7 source code; do NOT re-architect). Closes followup #7 (`insights.db` corrupt blocks cross-session learning on operator's box today).
- **Wave D — release infrastructure** (9-gate `/merge-ready`, CHANGELOG finalisation, `/release` cut `claudebase-v0.9.0`).

Wave B (TG polish + KP2/KP3 evidence + bugs #2 #8) and Wave C (multi-bot fleet) are **DEFERRED to v0.10** by operator decision. v0.6+ branch's KP1-verified TG work ships as-is.

The 38 commits already on the branch ARE the v0.9 baseline. This plan adds 6–11 additional commits cherry-picking v0.7's surface, then cuts the release.

## Success Criteria

The release is shippable when **all four** acceptance criteria pass:

| ID | Criterion | Evidence Required |
|---|---|---|
| **AC-V9-1** | `claudebase insight create --category project --tags <t> --salience high "<body>"` succeeds on operator's box AND writes a row to the cwd-local `insights.db`. | Shell stdout + SQL `SELECT count(*) FROM documents WHERE source_type LIKE 'agent-%'` returns ≥1 row after the call. |
| **AC-V9-2** | After AC-V9-1's row is inserted, `claudebase insight search "Mira test" --tag v9-cut-smoke --salience high --top-k 3 --json` exits 0 AND the JSON array contains the row inserted by AC-V9-1. | Shell stdout MUST be a non-empty JSON array; one element MUST contain the body substring `Mira test` AND `salience: "high"`; exit code MUST be 0; stderr MUST NOT contain `error: index database invalid`. |
| **AC-V9-3** | `claudebase insight tags --json` returns the merged tag vocabulary from cwd-local + global DBs, where **merged** means: union of tags from both DBs, deduplicated by tag string, counts summed per tag across both DBs, sorted by `count DESC, tag ASC`. | Shell stdout MUST be a JSON array of `{tag: string, count: integer}` objects; the array MUST contain `{tag: "v9-cut-smoke", count: 1}` after AC-V9-1's row is inserted; order MUST match the count-desc-tag-asc rule (verifiable by jq sort assertion). |
| **AC-V9-4** | `claudebase --version` reports `0.9.0` after `claudebase update` (via `/update-claudebase` skill) AND the `claudebase-v0.9.0` git tag exists with GitHub release attached. | `git tag --list 'claudebase-v0.9.0'` returns the tag; `gh release view claudebase-v0.9.0` shows the release. |

**Backward-compat MANDATE (operator directive 2026-06-04):** AC-V9-1 + AC-V9-2 MUST pass against **operator's existing `insights.db`** in its current state on `C:\Users\madwh\.claude\knowledge\insights.db`. The schema-v5 migration MUST either (a) repair the broken state in place, OR (b) exit with a clean `repair-required` message and a documented `claudebase ingest --reset` recovery path. Silent data loss is REJECTED.

## Additional Roles

No additional roles required. Per role-planner verdict 2026-06-04, the v0.9-cut feature scope is exclusively within the existing claudebase Rust crate + SDLC pipeline tooling. All 5 architect action items map onto core agents (architect, security-auditor, code-reviewer). 6 existing on-demand roles scanned (airflow-engineer, clickhouse-dba, dbt-engineer, kafka-connect-operator, rust-streaming-dev, sre-secret-rotation) — all are tactics-trade / streaming-data domain specialists with zero purpose-match against this feature.

## Red Team Adversarial Pass (2026-06-04)

Step 5.25 red-team returned 4 CRITICAL + 8 MAJOR + 2 MINOR objections. Operator routing 2026-06-04 19:00 UTC:

- **P-1/P-2/P-3 (premise — sub-agent citation hearsay; baseline schema version contradicts `src/migrations.rs` on disk)** — addressed by new Slice 0 pre-flight audit (see Preliminary Slices below).
- **F-1 (idempotency under pre-existing-state corruption)** — Slice 1 done-condition strengthened with 3 explicit edge tests: (a) DB at v3 with pre-existing `documents.category` column (`ALTER TABLE ADD COLUMN` should detect via `PRAGMA table_info` before execution); (b) pre-existing `insight_tags` table with wrong shape (`CREATE TABLE IF NOT EXISTS` is NO-OP — must detect shape mismatch and exit repair-required); (c) partial-reset state (PID killed mid-DROP — `schema_version` row gone but tables present).
- **F-2 (destructive v1→v2 migration silently violates MANDATE)** — Slice 1 done-condition adds explicit check: if v0.6 baseline's `migrate_v1_to_v2` runs on operator's v1 DB BEFORE `apply_v5_delta_and_backfill`, that is a backward-compat MANDATE violation. Slice 1 implementer MUST verify v0.7's `open_or_init_v2` bypasses the destructive v1→v2 path (e.g., by converging v1→v5 directly without going through v2's data-loss intermediate). Regression test: operator's v1 DB at Slice 1 done time has its rows preserved through migration.
- **F-3 (Slice 8 no rollback path if binary replace OK but daemon start fails)** — Slice 8 protocol updated to 5 steps via rename-stash pattern (A-4 design decision accepted): (i) version-greater check; (ii) stash-rename current binary → `claudebase.exe.old.<uuid>`; (iii) place new binary at canonical path; (iv) `daemon stop && daemon start` + PID/status verification; **(v) if step iv verification fails: rename stashed binary back, restart daemon, log failure**. The original 6-step protocol from PRD FR-V9-8.2 is SUPERSEDED.
- **D-1 (skill bootstrap chicken-and-egg)** — operator decision 2026-06-04: ship skill as known-untested in v0.9 (Question 2 default = 2a). Slice 9 CHANGELOG `[0.9.0]` block adds `### Known Limitations` entry: `/update-claudebase skill ships in v0.9 but its end-to-end upgrade path will be empirically verified only in v0.10 → v0.11; v0.7+v0.8 are deprecated paths and v0.6 has no skill to upgrade from`.
- **D-2 (38 commit list never enumerated)** — pre-staged commit list lives in `docs/plans/claudebase-v0.9-implementation-plan.md` (mirrored). Slice 9 implementer reads the list from there. Commit count verified 2026-06-04 = 39 commits (after `c177e9b`).
- **M-1 (v5 schema vs Wave B/C extensions)** — Wave B has no schema impact (operator dismissed bugs #2/#8 as legacy 2026-06-04; even if reactivated, they don't touch insights schema). Wave C multi-bot store is TOML-file or new `bots` table — additive to `chat.db`, not to insights `documents`. v5 design adequate for v0.10.
- **S-1/S-2/A-1 (scope inversion — should Wave B come first?)** — operator decision 2026-06-04: NO, Wave A stays in v0.9. Operator dismissed bugs #2/#8 as «легаси документ» (no longer concerns on current state).

## Architect Action Items (PASS-WITH-CONDITIONS verdict 2026-06-04)

Architect verdict: PASS-WITH-CONDITIONS. The following action items MUST be addressed at the cited slice:

- **A-1 [STRUCTURAL] @ Slice 1** — Inspect v0.7 commit `1161570`'s `apply_v5_delta_and_backfill` and determine whether the v5 schema delta is applied universally OR gated by DB-name discrimination. If universal, the implementer MUST gate behind a `db_path.ends_with("insights.db")` check, OR document `index.db` schema pollution as a deliberate trade-off. Default: gate it (insights.db only).
- **A-2 [STRUCTURAL] @ Slice 1** — Verify v0.7's `validate_schema_inner` extension is additive against the v0.6 baseline's object-presence check. Add regression test: existing books `index.db` opens successfully via `open_or_init_v2` and `validate_schema` returns OK after v0.9 migration runs.
- **A-3 [STRUCTURAL] @ Slice 6c** — Replace `Copy-Item -Force` in `Install-Prompts` (`install.ps1:114-115`) with proper per-file no-clobber: `if (-not (Test-Path (Join-Path $dest $_.Name))) { Copy-Item -Force ... }`. The task-spec's proposed `-ErrorAction SilentlyContinue` is NOT no-clobber; it only suppresses errors. Same fix on the `install.sh` parallel block (use `cp -n`).
- **A-4 [STRUCTURAL] @ Slice 8 — DESIGN DECISION REQUIRED** — Adopt rename-stash pattern from `install.ps1:506-515` (already battle-tested in this repo) instead of the 6-step stop-before-replace ordering: (i) version-greater check; (ii) stash-rename existing binary; (iii) place new binary at canonical path; (iv) `daemon stop && daemon start` AND PID-comparison + status verify. Eliminates the parallel-`claudebase run` race window between Step 3 and Step 5 of the original protocol. **Operator/architect sign-off required before Slice 8 implementation.**
- **A-5 [false positive]** — Use-cases file flagged `~/.claire/knowledge/insights.db` typo in PRD §19.3 FR-V9-1.5 line 1346. Verified via grep this session: NO such typo exists in PRD. False positive; no action needed.

**Slices flagged for security pre-review:** Slice 1 (data integrity), Slice 2b (SQL injection on tag filter), Slice 6c (installer privilege escalation), Slice 8 (binary integrity).

**Slices flagged for architect pre-review during implementation:** Slice 1 (A-1+A-2), Slice 6c (A-3), Slice 8 (A-4 design decision).

## Deliverables Checklist (mandatory for `/bootstrap-feature`)

- [ ] **PRD section** in `docs/PRD.md` — Functional Requirements covering: insights schema v5 (category + project_slug + insight_tags table); `insight create --category --tags` breaking-change contract; `insight search` dual-DB + RRF + tag/category/project filters; `insight tags` subcommand; project registry (`~/.claude/knowledge/projects.json`) + `claudebase run` upsert wiring; `/update-claudebase` skill protocol; SessionStart read-insights hook; UserPromptSubmit self-check hook; ASCII-only PowerShell hook constraint. Acceptance criteria = AC-V9-1 through AC-V9-4 verbatim. PRD section MUST carry `## Facts` block per cognitive-self-check.
- [ ] **Use cases** in `docs/use-cases/claudebase-v0.9-cut_use_cases.md` — 7+ scenarios: operator runs `insight create` after migration; operator runs `insight search --tag X`; operator runs `insight tags`; operator opens new session and SessionStart hook fires; operator runs `/update-claudebase` skill; operator's v0.6 schema-v1 insights.db survives migration; operator's corrupt insights.db hits the repair-required path.
- [ ] **Architecture review** by `architect` agent — focus on (a) merge-conflict reconciliation for the 2 BLOCKER files (`src/cli.rs` InsightCreateArgs breaking-change; `src/store.rs` open_or_init_v2 wholesale rewrite), (b) cherry-pick ordering (per Phase-1 dependency map), (c) backward-compat path validation against operator's actual DB state, (d) install.ps1 reconciliation between our `a615d9c` Start-Process migration and v0.7's hook-wiring additions.
- [ ] **QA test cases** in `docs/qa/claudebase-v0.9-cut_test_cases.md` — AC-V9-1 through AC-V9-4 as TC-V9-1/2/3/4 with Verification Class (CLI for 1/2/3, Mixed for 4) + Evidence Required columns. Plus TC-V9-5 backward-compat against operator's actual DB (Verification Class: **Mixed** — DB + CLI; evidence = pre-/post-migration `SELECT COUNT(*)` rows, schema_version pragma); TC-V9-6 migration recovery path (Verification Class: **CLI**; evidence = stderr literal repair-required message + exit code); TC-V9-7 SessionStart hook fires + reminder text (Verification Class: **CLI** — process-stdin/stdout; evidence = hook stdout containing the reminder marker substring); TC-V9-8 UserPromptSubmit hook fires (Verification Class: **CLI**; evidence = hook stdout containing the self-check marker substring); TC-V9-9 `/update-claudebase` skill round-trip (Verification Class: **Mixed** — CLI + FS; evidence = pre-/post-`claudebase --version` stdout + binary file mtime delta).
- [ ] **Implementation plan** in `<project>/.claude/plan.md` — refined slices by `planner` agent.
- [ ] **Plan Critic** pass on the refined `.claude/plan.md`.
- [ ] **CHANGELOG `[Unreleased]`** entry once implementation begins.

## Architectural Constraints

### Frozen — bit-for-bit our branch's v0.6+ work (CLI-observable surface)

1. **All 38 commits beyond `claudebase-v0.6.0` STAY** — multi-CLI Telegram routing, agent_registry routing-key columns, chat_reply message_thread_id, bot commands /agents /switch /whoami, chat_ask single + multi select, bridge target_agent_id filter, .claudebase/config.json per-project persistence, daemon CLI Start-Process migration. All live-verified per `docs/plans/claudebase-v0.9-product-plan.md` §2.
2. **Slice 8 AR-9 frame shape** — `notifications/claude/channel` meta is bit-for-bit inbound-Telegram schema; Slice 8 round-trip data lives in `params.content` as parseable preamble. Documented in PRD §18.10.9.
3. **`claudebase daemon start/stop/restart/status`** — uses process-discovery via PowerShell CIM + `taskkill` + `Start-Process` (not SCM service). Shipped in `ffda4a9`.

### Open for modification (this plan's surface)

- `src/store.rs` — schema migration logic (BLOCKER: full rewrite of `open_or_init_v2` to converge v1/v2/v3/v4 → v5 paths; sourced from v0.7 commit `1161570`)
- `src/cli.rs` — `InsightCreateArgs` gains required `--category` + `--tags`; new `InsightTagsArgs` variant + `Tags` enum (BLOCKER: breaking-change to existing-caller signatures — operator-acknowledged as the actual desired state because hook reminders already require these flags)
- `src/lib.rs` — new `pub mod registry;` declaration (sourced from `cccef44`, trivial alphabetic insert)
- `src/main.rs` — `upsert_project(cwd)` call at top of `run_claude_with_preset` + new match arm `InsightSubcommand::Tags(a) => run_insight_tags(&a)`
- `src/registry.rs` — new file (sourced from `cccef44`)
- `src/search.rs` — RRF fusion logic + dual-DB call sites (sourced from `afddf71`)
- `install.sh` + `install.ps1` — hook-wiring additions sourced from `cb45b4d` + `385efff`; reconcile with our `a615d9c` Start-Process daemon spawn (additive — hooks land before daemon-spawn block, no overlap)
- `hooks/` — new directory (4 new files from `cb45b4d` + `385efff` + `e43ca12` for the ASCII-PS fix)
- `prompts/rules/` — new directory (4 rule files including `cognitive-self-check.md` from `cb45b4d`)
- `prompts/commands/` — new `update-claudebase.md` from `4bc9a9c`
- `Cargo.toml` — version bump `0.6.0` → `0.9.0`
- `Cargo.lock` — regen
- `CHANGELOG.md` — new file (root) with `[0.9.0]` entry summarising the v0.6+ Telegram work + the v0.7 port-forward
- Release infra: `.github/workflows/release.yml` if missing

## Preliminary Slices (planner refines at bootstrap)

Sequential Wave 1 because `src/store.rs` is touched by Slices 1-4 — parallel slices would conflict. Wave 2 is hooks/skill which CAN parallelise but the benefit is small.

**Wave 0 — Pre-flight audit (closes red team P-1/P-2/P-3):**

- **Slice 0 — v0.7 source audit + branch migration-state audit.** Files: NO source changes. The implementer (Mira) MUST execute and persist evidence of:
  1. `git show 1161570 -- src/store.rs > .audit/v0.7-store.rs.diff` for each of the 11 v0.7 commits — actual source diffs against HEAD captured.
  2. `cat src/migrations.rs | head -100` + full read — verify CURRENT branch state. Red team finding: `migrations.rs` only has `migrate_v1_to_v2`. Document the actual schema versions our branch knows (likely v1, v2 only — NOT v4 as plan v1 assumed).
  3. Verify `apply_v5_delta_and_backfill` in v0.7's `1161570` is gated by DB-name (architect A-1 question) OR universal.
  4. Verify v0.7's `validate_schema_inner` extension is additive (architect A-2).
  5. Verify each of 9 non-BLOCKER commits applies cleanly via `git cherry-pick --no-commit <sha>` dry-run + immediate `git cherry-pick --abort` (mark `git status` clean before / after).
  6. **Update R-V9-CUT-5 risk assessment based on actual findings** — replace plan assumption ("schema v4 baseline") with verified-state.
  
  Done when: `.audit/` directory has 11 commit-diff files (deletable post-Slice-11 release cut) AND plan §Facts External contracts has updated entries with `source: git show <sha> this session` replacing the "Explore agent #N citation" language AND Slice 1 done-condition adjusted based on the actual baseline (if v1→v5 is the real path, all 4 synthetic fixtures may not even be producible; replace with whatever IS producible).

**Wave 1 — Insights v5 + project registry (sequential, file-overlap):**

- **Slice 1 — schema v5 migration + global resolver.** Cherry-pick `1161570` + `ff30d9f`. Files: `src/store.rs`, `src/main.rs`, `tests/store_v5_test.rs` (new), `tests/store_global_resolver_test.rs` (new), `tests/fixtures/synthetic-v{1,2,3,4}.db` (new — committed), `tests/fixtures/operator-db-snapshot.db` (new — gitignored, local-only). Resolve BLOCKER #1 (open_or_init_v2 rewrite): take v0.7's version wholesale. **v5 schema delta SHAPE** (per Explore agent #1, commit `1161570` body): adds `documents.category TEXT NOT NULL DEFAULT 'project'` + `documents.project_slug TEXT` columns; adds `CREATE TABLE insight_tags(doc_id INTEGER NOT NULL, tag TEXT NOT NULL, PRIMARY KEY(doc_id,tag), FOREIGN KEY(doc_id) REFERENCES documents(id) ON DELETE CASCADE)`; adds `insight_tags_tag_idx(tag)`. v4→v5 backfill: agent rows get `category='project'`, `project_slug` derived from `feature_slug`, default tag from `feature_slug` (one row per agent in `insight_tags`); books rows untouched. **OQ-V9-CUT-1 resolution** (decided in this Plan Critic pass): synthetic-v{1,2,3,4}.db fixtures (3 rows each, known payload) ARE committed to `tests/fixtures/`; operator-db-snapshot.db is local-only (gitignored) — Slice 1 implementer copies it from `C:\Users\madwh\.claude\knowledge\insights.db` at impl time. **Done when:** 4 synthetic-fixture migrations pass with row-count preservation + category/project_slug backfill correct; operator-snapshot migration is exercised locally and either (a) passes with row-count preservation, OR (b) exits with literal stderr `error: index database invalid; run \`claudebase ingest --reset\` to recover` (the documented repair-required path); 9+ v5 tests + 5+ resolver tests pass. **Plus red-team F-1 + F-2 edge tests:** (F-1a) DB at v3 with pre-existing `documents.category` column manually added — `PRAGMA table_info`-based existence check skips the `ALTER TABLE ADD COLUMN` without "duplicate column" error; (F-1b) pre-existing `insight_tags` table with wrong shape (no FK / different PK) — migration detects shape mismatch via `PRAGMA table_info('insight_tags')` and exits repair-required (NOT silently keeps wrong shape); (F-1c) partial-reset state (`schema_version` row gone but `documents` table present) — migration detects state inconsistency and exits repair-required; (F-2) v1 DB does NOT trigger the destructive `migrate_v1_to_v2` path in `src/migrations.rs:64-91` (which DROPs `documents` + `chunks` + `chunks_fts`) — v0.7's `open_or_init_v2` MUST converge v1→v5 directly without going through v2's data-loss intermediate; regression test: operator's v1 DB through migration preserves all rows.

- **Slice 2a — `InsightCreateArgs` CLI schema breaking-change.** Cherry-pick `c0eebca` CLI portion only. Files: `src/cli.rs` (add REQUIRED `--category {general|project}` enum + REQUIRED `--tags <comma>` Vec<String> + optional `--project <slug>`), `tests/cli_insight_create_args_test.rs` (new, ~8 tests: missing-category exits 2, missing-tags exits 2, tag normalisation strips `#`/lowercases/trims/dedupes, project-flag passes through). Done when: 8+ CLI parsing tests pass; `cargo check` clean; one round-trip integration test passes (`claudebase insight create --category project --tags v9-cut-smoke "smoke"` exits 0 + returns new `documents.id`).

- **Slice 2b — `insight create` dual-DB write routing.** Cherry-pick `c0eebca` main.rs portion. Files: `src/main.rs` (dual-DB dispatch), `tests/cli_insight_create_routing_test.rs` (new, 17 routing tests from v0.7). Done when: 17 routing tests pass; `--category general` writes to global `~/.claude/knowledge/insights.db`; `--category project` writes to cwd-local; existing exact-sha + semantic dedupe preserved per-DB.

- **Slice 3a — `rrf_fuse_hits` extraction + `open_insight_dbs` corrupt-fallback.** Cherry-pick the `src/search.rs` core portion of `afddf71`. Files: `src/search.rs` (new `pub fn rrf_fuse_hits(local, general, top_k)` keyed on `(source_corpus, chunk_id)`), `tests/rrf_fuse_hits_test.rs` (new, 5 RRF tests covering chunk_id-collision survival). Done when: 5 RRF tests pass; `rrf_fuse_corpora()` refactored to 7-line wrapper over `rrf_fuse_hits` per `afddf71`; corrupt/missing-global path returns local-only with stderr warn (NOT exit-1).

- **Slice 3b — `insight search` dual-DB read + tag/category/project filters.** Cherry-pick the CLI portion of `afddf71`. Files: `src/cli.rs` (add `--tag <repeatable>` `--category` `--project <slug>` `--general-only` `--project-only` flags to search/list/random; add `--category` to gc/delete), `src/main.rs` (dual-DB call sites; `WHERE tag IN (?,?,..)` parameterised — NO `format!` into SQL), `tests/cli_insight_dual_db_test.rs` (new, 18 tests from v0.7). Done when: 18 dual-DB tests pass; tag-filter uses parameterised `IN (?,?,..)`; existing `cli_search_e2e` regression-free (11/11 still pass).

- **Slice 4 — `insight tags` subcommand.** Cherry-pick `2719e25`. Files: `src/cli.rs` (new `InsightTagsArgs` + dispatch), `src/main.rs` (`run_insight_tags()` handler), `tests/cli_insight_tags_test.rs` (new, 11 tests). **"Merged" semantics spec** (decided in this Plan Critic pass): merged := union of `(tag, count)` pairs from cwd-local + global DBs, deduplicated by `tag` string, `count` field = SUM of per-DB counts for that tag, sort key = `count DESC, tag ASC`. Missing global DB contributes zero without error (path.exists guard avoids materialising an empty db). Done when: 11 tags tests pass; merged-semantics test asserts exact sort order against a 2-DB fixture with overlapping tags.

- **Slice 5 — project registry.** Cherry-pick `cccef44`. Files: `src/registry.rs` (NEW), `src/lib.rs`, `src/main.rs`, `tests/registry_test.rs` (new, 9 tests including concurrency). Reconcile with our `25189bc` (`.claudebase/config.json` per-project persistence — registry is the cross-cutting INDEX, doesn't replace per-project config). Done when: 9 registry tests pass; `claudebase run` from a fresh cwd registers the project in `~/.claude/knowledge/projects.json`.

**Wave 2 — Hooks + skill (file-overlap small, can run sequentially):**

- **Slice 6a — `prompts/rules/` migration + `prompts/` directory scaffold.** Cherry-pick `prompts/` portion of `cb45b4d`. Files: `prompts/rules/cognitive-self-check.md` (NEW, sourced from `cb45b4d`), `prompts/rules/knowledge-base.md` + `knowledge-base-tool.md` + `tool-limitations.md` (NEW if not already in repo — verify at impl). **Source-vs-deploy clarification** (decided in this Plan Critic pass — addresses Plan Critic finding #15): `prompts/` is the REPO-SHIPPED directory; the installer (`install.sh` + `install.ps1`) DEPLOYS its contents to `~/.claude/rules/` + `~/.claude/commands/` + `~/.claude/agents/` on the operator's box. Deploy step lands in Slice 6c installer wiring. Done when: 4 rule files exist under `prompts/rules/`; content is byte-identical to v0.7's versions (verify via `git show 1161570:prompts/rules/cognitive-self-check.md | diff -` or equivalent).

- **Slice 6b — UserPromptSubmit hook scripts (`{sh,ps1}`).** Cherry-pick hook portion of `cb45b4d` + `0b92384` + `e43ca12`. Files: `hooks/claudebase-selfcheck-reminder.sh` (NEW), `hooks/claudebase-selfcheck-reminder.ps1` (NEW, ASCII-only per `e43ca12`), `hooks/claudebase-insight-capture.{sh,ps1}` (modified per `0b92384` compact-reason + `e43ca12` ASCII-only — verify if these files exist in our branch first; if not, port from v0.7 as new files). Done when: both scripts exist; `.ps1` files contain ONLY ASCII bytes (verifiable via PowerShell `(Get-Content $f -Encoding Byte | Where-Object { $_ -gt 127 }).Count -eq 0`); both scripts emit valid JSON `{ hookSpecificOutput: { additionalContext: "..." } }` on stdout when invoked.

- **Slice 6c — `install.sh` + `install.ps1` hook-wiring with idempotency.** Cherry-pick installer portion of `cb45b4d` + `385efff` + Slice 6a deploy step. Files: `install.sh` + `install.ps1`. **Idempotency requirement** (decided in this Plan Critic pass — addresses Plan Critic finding #9): hook wiring uses dedup-by-command-string equality per `cb45b4d` body — installer reads existing `~/.claude/settings.json` `hooks.UserPromptSubmit` / `hooks.SessionStart` arrays, parses each entry's `command` field, skips appending if a matching `command` is already present. Re-running installer is a no-op. The `prompts/` → `~/.claude/` deploy step uses `cp -n` (sh) / `Copy-Item -ErrorAction SilentlyContinue` (ps1) with `--no-clobber` semantics so operator's existing customisations are NOT overwritten. **Reconcile with our `a615d9c` Start-Process daemon block:** hook wiring lands in a clearly-marked block BEFORE the daemon-spawn block; verify via running `install.ps1` end-to-end then immediately running daemon to confirm both work. Done when: hooks wired idempotently (running installer twice yields zero new entries in settings.json); existing daemon-spawn block still spawns daemon successfully; full install→run→`daemon status` verification passes.

- **Slice 7 — SessionStart read-insights-on-new-context hook.** Cherry-pick `385efff`. Files: `hooks/claudebase-read-insights-reminder.sh` (NEW), `hooks/claudebase-read-insights-reminder.ps1` (NEW, ASCII-only), `install.sh` + `install.ps1` (hook wiring via the same dedup-by-command-string pattern from Slice 6c). Done when: hook fires on session start/resume/compact AND injects reminder text containing the literal substring `claudebase insight tags` in `additionalContext`; idempotent re-install yields zero new entries.

- **Slice 8 — `/update-claudebase` skill + daemon-state preservation via rename-stash (5-step, A-4 design accepted).** Cherry-pick `4bc9a9c`. Files: `prompts/commands/update-claudebase.md` (NEW), `README.md` + `install.sh` + `install.ps1` banner updates. **Revised 5-step protocol (supersedes original 6-step from PRD FR-V9-8.2 per architect A-4 + red-team F-3):** (i) version-greater check (refuses downgrade); (ii) stash-rename current binary `claudebase.exe` → `claudebase.exe.old.<uuid>` (Windows allows rename of running PE — pattern lifted from `install.ps1:506-515` battle-tested Telegram-plugin block); (iii) place new binary at canonical path (same-filesystem rename — temp file MUST be in same directory, NOT `%TEMP%`); (iv) `claudebase daemon stop` then `claudebase daemon start` then PID-comparison + status verification; **(v) red-team F-3 rollback step: if step (iv) verification FAILS (daemon doesn't start OR new PID matches old OR status != running), rename stashed binary back to canonical path AND restart daemon AND log failure to stderr.** Done when: skill works end-to-end on operator's box; daemon PID changes after skill runs; `claudebase --version` reflects new tag; integration test exercises the rollback path (mock daemon-start failure → verify stashed binary restored AND daemon resumed); **Known limitation per D-1: the skill ships UNTESTED for actual cross-version upgrade in v0.9 because v0.7/v0.8 are deprecated paths and v0.6 has no skill to upgrade from — full end-to-end upgrade verification happens at v0.10→v0.11 cycle. CHANGELOG entry documents this.**

**Wave 2.5 — Operator-requested addition 2026-06-04: `/start` inline-menu (C6 port from Wave C):**

- **Slice 12 — `/start` Telegram inline-menu with `agents` + `switch` two-stage keyboard.** Cherry-pick none — builds entirely on top of our already-shipped Slice 8a/8b chat_ask infra + Slice 4b/4c bot command dispatch. Files: `src/daemon/telegram.rs` (extend bot command dispatcher to handle `/start` → emit chat_ask with `multi=false` + options `[agents, switch]`), `src/daemon/chat.rs` (new helper to build the agent-list chat_ask for switch's second stage), `tests/slice12_start_menu_test.rs` (new, 6+ tests: /start emits 2-button keyboard; tap `agents` emits text list; tap `switch` emits second chat_ask with one option per alive CLI; second-stage tap on CLI-button calls `handle_switch` with FR-MAT-8.6 last_user_id security gate; empty-alive-list edge case shows "no CLIs alive" single button; concurrent `/start` from different chats don't share state). **Architectural decision (operator spec 2026-06-04):** the second-stage CLI keyboard is built AT TAP TIME via fresh `list_alive` snapshot — NOT cached from `/start` time. **Security:** the CLI-button tap goes through the existing `handle_switch` security gate (FR-MAT-8.6 last_user_id check from Slice 4c). Done when: 6+ tests pass; operator can tap `/start` → `agents` → see list; tap `/start` → `switch` → tap a CLI → see "Switched to X" reply; security denial works for non-binder taps.

**Wave 3 — Release infrastructure:**

- **Slice 9 — version bump + CHANGELOG `[Unreleased]` finalisation.** Files: `Cargo.toml` (`0.6.0` → `0.9.0`), `Cargo.lock` regen, `CHANGELOG.md` (NEW at root). CHANGELOG `[0.9.0]` block enumerates: (a) the 38 v0.6+ branch commits summarised by feature theme (multi-CLI TG routing, Slice 8 chat_ask AR-9, daemon CLI Start-Process migration, bridge target_agent_id filter, per-project .claudebase/config.json); (b) the Wave A port-forward (insights v5 + project registry + hooks + skill); (c) explicit deprecation notice that v0.7 + v0.8 tags are NOT supported v0.9 paths AND that KP2/KP3 forum-topic routing is architecturally complete but live-evidence is pending in v0.10. **(d) `### Known Limitations` block** (operator decision 2026-06-04 D-1 mitigation): `/update-claudebase skill: ships in v0.9 but its end-to-end upgrade path will be empirically verified only at v0.10 → v0.11 cycle; v0.7+v0.8 are deprecated and v0.6 has no skill to upgrade from. Skill is functionally complete but un-cross-version-tested.` **(e) Slice 12 (`/start` inline menu) added as Wave 2.5 operator-requested feature** (port of C6 from Wave C product plan).  Done when: `cargo build --release` clean at the new version `0.9.0`; CHANGELOG entries cite the load-bearing commits (commit hashes inline); CHANGELOG passes Keep-a-Changelog format validation; the 39-commit list pre-staged in `docs/plans/claudebase-v0.9-implementation-plan.md` is referenced.

- **Slice 10 — `/qa-cycle` live verification with concrete evidence artifacts.** Run `/qa-cycle` against the live operator's box. **Concrete artifact targets** (decided in this Plan Critic pass — addresses Plan Critic finding #11): per-AC evidence files committed to `docs/qa/evidence/claudebase-v0.9/`: `AC-V9-1-create-stdout.txt` (`insight create` stdout); `AC-V9-1-sql-count.txt` (post-create SQL row count); `AC-V9-2-search-stdout.json` (search JSON output containing the AC-V9-1 row); `AC-V9-3-tags-stdout.json` (merged tags JSON); `AC-V9-3-merged-semantics-assertion.txt` (jq-based sort-order assertion); `TC-V9-5-pre-migration-rows.txt` + `TC-V9-5-post-migration-rows.txt` (backward-compat row count delta against operator's actual DB or the documented repair-required exit stderr capture); `TC-V9-7-sessionstart-hook-stdout.txt` (literal stdout of the hook fire); `TC-V9-8-userpromptsubmit-hook-stdout.txt` (same for the other hook); `TC-V9-9-version-delta.txt` (pre/post `claudebase --version` capture). Done when: ALL artifacts exist under `docs/qa/evidence/claudebase-v0.9/`; `/qa-cycle` qa-engineer emits overall PASS verdict citing each artifact; AC-V9-1 through AC-V9-3 all empirically pass on operator's actual box.

- **Slice 11 — `/merge-ready` 9 gates + `/release` cut with rollback strategy.** Run `/merge-ready`; resolve any gate failures; merge to `main`; run `/release` to cut `claudebase-v0.9.0` tag + GitHub release with asset matrix + CHANGELOG body. **Rollback strategy** (decided in this Plan Critic pass — addresses Plan Critic finding #12): if any of the following fail mid-`/release`, the corresponding recovery action is taken: (a) `git tag` succeeds but `gh release create` fails → delete the tag (`git tag -d claudebase-v0.9.0 && git push --delete origin claudebase-v0.9.0`), fix the gh-release issue, re-run /release; (b) `gh release create` succeeds but asset upload fails → keep the release as `--draft` via `gh release edit`, upload missing assets manually, then `gh release edit --draft=false`; (c) `/merge-ready` fails Gate 0 (corporate-code-style) → re-run after fixing cited findings; do NOT bypass; (d) `/merge-ready` fails Gate 6 (test coverage) → add missing test coverage in a new slice rather than skipping. Done when: `claudebase-v0.9.0` git tag exists on `main`; `gh release view claudebase-v0.9.0` shows a non-draft release with at least Linux + macOS + Windows binary assets attached; `claudebase --version` on a fresh download reports `0.9.0`; rollback log (if any rollback was exercised) is captured in `docs/qa/evidence/claudebase-v0.9/release-cut-log.txt`.

## Files Likely Affected

**Modified (port-forward — v0.7 source taken wholesale or near-wholesale):**
- `src/store.rs` — BLOCKER #1 reconciliation (full open_or_init_v2 rewrite)
- `src/cli.rs` — BLOCKER #2 reconciliation (breaking-change InsightCreateArgs)
- `src/main.rs` — upsert_project call + new match arm + run_insight_tags handler
- `src/lib.rs` — `pub mod registry;` insert
- `src/search.rs` — RRF fusion logic
- `install.sh` + `install.ps1` — hook wiring additive sections
- `README.md` — `/update-claudebase` skill banner update
- `CHANGELOG.md` (NEW at root)
- `Cargo.toml` + `Cargo.lock` — version bump

**Created:**
- `src/registry.rs`
- `hooks/claudebase-selfcheck-reminder.{sh,ps1}`
- `hooks/claudebase-read-insights-reminder.{sh,ps1}`
- Modifications to existing `hooks/claudebase-insight-capture.{sh,ps1}` if present (else NEW)
- `prompts/rules/cognitive-self-check.md`
- `prompts/rules/knowledge-base.md` + `knowledge-base-tool.md` + `tool-limitations.md` (if not already in repo)
- `prompts/commands/update-claudebase.md`
- `docs/use-cases/claudebase-v0.9-cut_use_cases.md`
- `docs/qa/claudebase-v0.9-cut_test_cases.md`
- `docs/qa/evidence/claudebase-v0.9/` directory (Slice 10 evidence)
- `.github/workflows/release.yml` (if missing) — for Slice 11

**Preserved bit-for-bit (frozen — see Architectural Constraints §Frozen):**
- All `src/daemon/*` files modified by our 27 branch commits
- `src/plugin/bridge.rs` (target_agent_id filter)
- `src/plugin/mcp.rs` (TOOL_WHITELIST extended with chat_ask + chat_list_pending_asks in our Slice 8)
- `src/project_config.rs` (`.claudebase/config.json` from our `25189bc`)
- `docs/PRD.md` §18 + §18.10 (multi-agent-TG + Slice 8 AR-9 amendment)

## Risks & Dependencies

- **R-V9-CUT-1 (BLOCKER #1: `src/store.rs` open_or_init_v2 wholesale rewrite).** Per Explore agent #2 audit: v0.7's migration logic replaces our v4-only init code with a v1→v5 / v2→v5 / v3→v5 / v4→v5 converging migration. A line-by-line cherry-pick will not apply cleanly. **Mitigation:** take v0.7's version wholesale at Slice 1; immediately verify against a **copy of operator's actual `insights.db`** (NOT a synthetic test DB) as part of Slice 1 done-condition. Backward-compat MANDATE is satisfied by this verification, not by reading the v0.7 migration code. Salience: high.
- **R-V9-CUT-2 (BLOCKER #2: `src/cli.rs` InsightCreateArgs breaking-change).** v0.7 adds REQUIRED `--category` and `--tags` flags. Operator's SDLC config already mandates these via UserPromptSubmit hook reminders (this session sees them every turn), so the "breaking" is catching the binary up to the contract agents already expect. **Mitigation:** Slice 2 ships in the same release as Slices 6-7 (hooks) so the binary lands at the moment the hook reminders go live; no ecosystem-wide breakage window. Salience: high.
- **R-V9-CUT-3 (Operator's current `insights.db` state on box is corrupt / unknown).** Followup #7 says every `insight create` returns `error: index database invalid; re-ingest required`. Schema version on disk is unknown (cannot query without binary that handles invalid state). **Mitigation:** Slice 1's done-condition includes a snapshot-and-test pass against operator's actual DB. If migration cannot repair, document the `claudebase ingest --reset` recovery path; backward-compat mandate is met as long as no SILENT data loss occurs. Salience: high.
- **R-V9-CUT-4 (install.ps1 hook-wiring reconciliation with our `a615d9c` Start-Process daemon block).** Explore agent #2 verdict: ADDITIVE — hook wiring lands BEFORE the daemon-spawn block in a different section, no overlap. **Mitigation:** Slice 6 implementer applies v0.7's hook-wiring patch on top of our current install.ps1; verify the post-install daemon-spawn still runs end-to-end. Salience: medium.
- **R-V9-CUT-5 (Schema-version assumption — operator's DB might be at v1, not v4).** v0.6 baseline's `store.rs` initialises fresh DBs at v4 (per Explore agent #2 line 249 citation). Operator's DB might pre-date that. **Mitigation:** v0.7's migration handles ALL paths v1→v5; not a real risk, just a verification step in Slice 1. Salience: low.
- **R-V9-CUT-6 (Wave A code-REUSE directive interpreted as "blind cherry-pick").** Operator directive says "code REUSE, NOT a re-architecture". This does NOT mean blind apply — the 2 BLOCKERS require deliberate reconciliation. **Mitigation:** Slice 1 and Slice 2 both spawn `architect` pre-review per Deliverables Checklist; reconciliation is supervised. Salience: medium.
- **R-V9-CUT-7 (KP2/KP3 evidence still pending; ships in v0.9 unverified).** Operator accepted this in the scope-cut decision: KP1 LIVE-verified is good enough for v0.9; KP2/KP3 deferred to v0.10. **Mitigation:** CHANGELOG `[0.9.0]` entry MUST state "KP2/KP3 forum-topic routing architecturally complete but live-evidence pending (deferred to v0.10)" so downstream consumers know the verification gap. Salience: medium.

## Out of Scope (this plan)

- **Wave B from product plan** (`/here` command + e2e routing tests + smoke runbook + conflict-gate audit + bug #2 bridge proactive-retry + bug #8 daemon accept-loop hang). Deferred to v0.10 per operator decision 2026-06-04.
- **Wave C from product plan** (multi-bot secret store + multi-bot long-poll + `--dangerously-skip-permissions` + `claudebase startproject` + `claudebase update` CLI + `claudebase daemon setup` + `/start` inline menu). Deferred to v0.10.
- `tg_message_map` reply-quote routing — rejected by operator (no longer interesting).
- v0.7's repo-presentation polish (`.github` scaffolding + README hero + Stop-hook refactor + code-graph docs + multi-CLI plan quartet docs). Reference-only.
- v0.8's bridge reconnect-replay cache (WIP-partial in v0.8 itself), access.json File A→B migration (not applicable to our baseline), v0.7.1 release-matrix fix.

## Open Questions (resolve at bootstrap)

- **OQ-V9-CUT-1 (Slice 1 verification of operator's actual DB):** WHO takes the snapshot of `C:\Users\madwh\.claude\knowledge\insights.db` for the backward-compat test — operator manually copies it to `tests/fixtures/operator-db-snapshot.db` and commits, OR Slice 1 implementer copies + tests in-place without committing the binary blob to git? Salience: medium.
- **OQ-V9-CUT-2 (Existing hooks on operator's box):** the SessionStart onboarding hook IS already deployed on operator's box (we see its output every session resume — long preamble). But the SUBAGENT-onboarding hook AND the read-insights-reminder hook may not be wired. Slice 6 + 7 implementers verify before patching install.{sh,ps1}. Salience: low.
- **OQ-V9-CUT-3 (v0.9 vs v0.10 semver narrative):** Recommended in product plan §7 R-V9-1 to keep `v0.9.0` and note v0.7+v0.8 are deprecated in CHANGELOG. Operator decision. Salience: low.

## Verification (how to test end-to-end)

After all 11 slices land, the verification sequence is:

1. **Pre-flight:** `cargo build --release` clean at new version `0.9.0`; `cargo test --workspace` passes (178+ existing + Wave A new tests).
2. **Local-only Wave A smoke:** `claudebase insight create --category project --tags test "Mira test"`; then `claudebase insight search "Mira" --tag test --json`; then `claudebase insight tags --json`. All three succeed; corpus does NOT report `error: index database invalid`.
3. **Backward-compat smoke (TC-V9-5):** Restore a snapshot of operator's actual `insights.db` to a temp path; run the v0.9 binary against it; verify either (a) migration applies cleanly + rows are preserved + tag/category backfill correct, OR (b) repair-required exit-1 with documented recovery path.
4. **SessionStart hook smoke (TC-V9-7):** Start a new CC session; verify the read-insights-reminder additionalContext text appears in the session-start system reminder.
5. **UserPromptSubmit hook smoke (TC-V9-8):** Send any prompt to CC; verify the self-check-reminder additionalContext appears in the pre-response hook output.
6. **`/update-claudebase` skill smoke (TC-V9-9):** Invoke the skill; verify binary updates to latest tag; `claudebase --version` matches the newly-installed tag.
7. **Release-gate verification (TC-V9-4):** Run `/qa-cycle` → expect PASS; run `/merge-ready` → expect MERGE READY; run `/release` → tag + GitHub release published; fresh `cargo install` or download from release page yields binary reporting `0.9.0`.

## Facts

### Verified facts

- Current branch `feat/multi-agent-on-v0.6` has 38 commits beyond `claudebase-v0.6.0` (HEAD = `15b9460`). — source: `git log claudebase-v0.6.0..HEAD` this session — salience: high.
- v0.7's 11 candidate cherry-pick commits + their dependency order — verified via Phase-1 Explore agent #1 this session — salience: high.
- 2 BLOCKER files (`src/cli.rs` breaking InsightCreateArgs + `src/store.rs` wholesale open_or_init_v2 rewrite) + 4 OVERLAP files + 4 ADDITIVE directories — verified via Phase-1 Explore agent #2 this session — salience: high.
- v0.7 introduces no new Cargo deps (uses existing rusqlite + serde_json + std::fs) — verified via Explore agent #2 — salience: medium.
- Hook reminders agents see EVERY UserPromptSubmit turn already specify `--category` + `--tags` as REQUIRED flags — verified live in this session (system-reminder text on every prompt) — salience: high (means "breaking-change" is actually a contract-alignment fix).
- Operator's `insights.db` currently returns `error: index database invalid; re-ingest required` on every `insight create` — verified in earlier session attempt — salience: high (load-bearing for backward-compat MANDATE).
- v0.6 baseline initialises fresh DBs at schema v4 (per Explore agent #2 store.rs:249 citation) — verified via Explore — salience: medium.
- `docs/plans/claudebase-v0.9-product-plan.md` (commit `15b9460`) is the source-of-truth product plan this implementation plan operationalizes — verified via git log — salience: medium.

### External contracts

- **v0.7 `src/store.rs` SCHEMA_V5_DELTA** — symbol: `ALTER TABLE documents ADD COLUMN category TEXT; ALTER TABLE documents ADD COLUMN project_slug TEXT; CREATE INDEX IF NOT EXISTS idx_documents_category ON documents(category); CREATE TABLE IF NOT EXISTS insight_tags (doc_id INTEGER NOT NULL REFERENCES documents(id) ON DELETE CASCADE, tag TEXT NOT NULL, UNIQUE(doc_id, tag)); plus idx_insight_tags_tag` — source: `git show 1161570 -- src/store.rs` **this session via Slice 0 audit** (see `.audit/slice0-audit-report.md`) — verified: yes (direct source read) — salience: high.
- **v0.7 backfill gating** — symbol: backfill UPDATEs are conditional on `source_path LIKE 'agent:%'`; books-corpus rows untouched. DDL is UNIVERSAL (applied to every DB opened via `open_or_init_v2`). Architect A-1 question resolved: NO db-name guard needed in Slice 1; v0.7 design intent is universal-DDL + gated-backfill — source: commit `1161570` body text read in Slice 0 audit — verified: yes — salience: high.
- **v0.7 `validate_schema_inner` range extension** — symbol: `1..=4` → `1..=5` (additive — existing v4 books `index.db` still validates) — source: commit `1161570` body read in Slice 0 audit — verified: yes (commit message text) — salience: medium. Architect A-2 question partially resolved; full function-body inspection deferred to Slice 1.
- **v0.6+ branch `src/store.rs::open_or_init_v2` actual state** — symbol: function at lines 222-310 handles v0→v4, v2→v4, v3→v4, v4-idempotent — source: `src/store.rs:222-310` read this session in Slice 0 audit — verified: yes — salience: high (red team P-2 was FALSE POSITIVE; plan v1 baseline assumption "v0.6 stamps fresh DBs at v4" verified CORRECT).
- **v0.7 `src/cli.rs` InsightCreateArgs breaking-change shape** — symbol: REQUIRED `--category <general|project>` + REQUIRED `--tags <comma-separated>` + optional `--project <slug>` — source: commit `c0eebca` per Explore agent #2 — verified: yes (Explore sample read) — salience: high.
- **v0.7 `src/registry.rs` API** — symbol: `pub struct ProjectEntry`, `pub fn upsert_project(root: &Path) -> Result<(), String>`, `pub fn resolve_project_path(slug: &str) -> Option<PathBuf>` — source: commit `cccef44` per Explore agent #1 — verified: yes — salience: high.
- **v0.7 hook script paths** — symbol: `hooks/claudebase-selfcheck-reminder.{sh,ps1}` + `hooks/claudebase-read-insights-reminder.{sh,ps1}` + `hooks/claudebase-insight-capture.{sh,ps1}` — source: commits `cb45b4d` + `385efff` + `0b92384` + `e43ca12` per Explore agent #1 — verified: yes — salience: medium.
- **v0.7 `prompts/commands/update-claudebase.md` skill spec** — symbol: skill name `update-claudebase`, protocol "fetch README → extract install one-liner → execute → report version delta" — source: commit `4bc9a9c` per Explore agent #1 — verified: yes — salience: medium.
- **v5 schema identifiers** (per commit `1161570` body via Explore agent #1) — symbols: column `documents.category TEXT NOT NULL DEFAULT 'project'`; column `documents.project_slug TEXT`; table `insight_tags(doc_id INTEGER, tag TEXT, PRIMARY KEY(doc_id, tag), FOREIGN KEY(doc_id) REFERENCES documents(id) ON DELETE CASCADE)`; index `insight_tags_tag_idx(tag)` — source: commit `1161570` per Explore agent #1 — verified: yes (Explore sample-read the schema constants) — salience: high.

### Assumptions

- v0.7's wholesale `open_or_init_v2` rewrite can be taken as-is without modifications when applied on top of our 27 branch commits — risk: subtle interactions with our `src/daemon/*` and `src/project_config.rs` if any of them call into store.rs APIs that v0.7 changed. How to verify: Slice 1 implementer audits all callers via `grep open_or_init_v2 src/` before cherry-pick. Salience: medium.
- Operator's `insights.db` schema is at v4 (v0.6 baseline default) NOT v1/v2/v3 — risk: if pre-v0.6 it could be lower, but v0.7's migration handles all paths so this is just a verification step. Salience: low.
- Hook reminders agents already require `--category --tags` flags are guaranteed to work after Slice 2 lands — risk: subtle CLI parsing change that hook scripts didn't anticipate. How to verify: Slice 2 implementer runs the actual hook reminder text through `claudebase insight create` as an integration test. Salience: medium.

### Open questions

- OQ-V9-CUT-1, OQ-V9-CUT-2, OQ-V9-CUT-3 (see §Open Questions above). Salience: medium.
- How many additional tests (over the 178 currently passing) will Wave A add? Phase-1 Explore agent #1 estimated 9 (v5 store) + 5 (resolver) + 17 (insight create routing) + 18 (dual-DB) + 5 (RRF) + 11 (insight tags) + 9 (registry) = 74 new tests. Verified at implementation time. Salience: low.

## Decisions

### Inbound validation

- Operator directive 2026-06-04 «теперь через план мод и пайплайн агенетов спроектируй этот план» — interpreted as: enter plan mode, draft a feature scope per CLAUDE.md SDLC pipeline rules, persist to `<project>/.claude/plan.md` per Plan-Mode-Persistence rule, then run `/bootstrap-feature` after operator approval. Challenged: no — direct ask, no contradiction with prior session decisions. Outcome: proceeded. Salience: high.
- Operator directive «Wave A оставляем — всю реализацю инсайдов мождно переиспользовать из 7 и 8 версии исходников. заново всю архитектуру планировать не надо» — challenged: yes, surfaced as `R-V9-CUT-6` because "code REUSE" does NOT mean "blind cherry-pick" given the 2 BLOCKERs require deliberate reconciliation. Outcome: proceeded with directive intent (cherry-pick over re-architecture) but added architect pre-review gates on Slice 1 + Slice 2 to handle the BLOCKERs. Salience: high.
- Operator directive «для insight баз нужна обратная совместимость обязательно» — challenged: no — directly aligns with v0.7's additive migration design (per Explore agent #1). Outcome: encoded as backward-compat MANDATE in §Success Criteria + Slice 1 done-condition + R-V9-CUT-3 mitigation. Salience: high.
- Operator scope-cut «Wave B убираем, Wave C убираем» — challenged: no (operator's prerogative on scope) but flagged consequences in R-V9-CUT-7 (KP2/KP3 ships unverified). Outcome: deferred B + C to v0.10 per operator; CHANGELOG note required. Salience: high.

### Decisions made

- v0.9 ships from `feat/multi-agent-on-v0.6` branch, NOT from rebase-onto-v0.7 (operator's «откатываемся до 6 версии» principle). Q1 hack? no | Q2 sane? yes | Q3 alternatives? rebase on v0.7 — rejected (drops our 38 commits' history, contradicts operator) | Q4 cause | Q5 n/a. Salience: high.
- 11-slice breakdown grouped into 3 waves (Wave 1 sequential due to file overlap on `src/cli.rs` + `src/main.rs` + `src/store.rs`; Wave 2 mostly parallelisable but small; Wave 3 = release tail). Q1 hack? no | Q2 sane? yes | Q3 alternatives? all-parallel — rejected (file conflicts); 1-slice-monolithic — rejected (too coarse for /qa-cycle) | Q4 cause | Q5 n/a. Salience: medium.
- Backward-compat verification uses a copy of operator's actual `insights.db`, not a synthetic test DB. Q1 hack? no | Q2 sane? yes (operator's real state is the only meaningful gate) | Q3 alternatives? synthetic-only — rejected (would silently miss real-state edge cases) | Q4 cause | Q5 n/a. Salience: high.
- Slice 2 ships in the same release as Slices 6-7 hooks (no ecosystem-wide breakage window). Q1 hack? no | Q2 sane? yes | Q3 alternatives? Slice 2 in v0.9, hooks in v0.9.1 — rejected (would create a window where binary requires flags but hooks don't emit them, breaking agent UX) | Q4 cause | Q5 n/a. Salience: medium.
- Slice 1 done-condition INCLUDES test against operator's actual DB — not a "verify later" hand-wave. Q1 hack? no | Q2 sane? yes (the only way to validate the load-bearing backward-compat MANDATE) | Q3 alternatives? defer to /qa-cycle — rejected (a failed migration could land + corrupt operator data before /qa-cycle catches it) | Q4 cause | Q5 n/a. Salience: high.

### Hacks / workarounds acknowledged

(none — port-forward of already-shipped architecture; no shortcuts taken in the plan itself)

### Symptom-only patches (with root-cause links)

- The original v0.7/v0.8 brokenness root cause was NOT pursued by operator (sunk-cost decision: forward-debug cost exceeded rebuild cost). v0.9 ships from the rebuild branch with v0.7's insights surface ported on top. Symptom: «v0.7/v0.8 косячная, root cause not isolated». Root cause that remains: unknown. Tracked at: `docs/plans/claudebase-v0.9-product-plan.md` §1 + the implicit obligation to log any v0.7/v0.8 failure modes uncovered during v0.9 work into `docs/issues/`. Operator-acknowledged trade-off. Salience: high (because the rebuild path is taken on faith that "v0.7 commits cherry-pick cleanly" — Slice 1's audit is the only check).

## Review Notes

### Critic Findings (Plan Critic v1, on plan v1 2026-06-04)

- **Total**: 18 findings (0 critical, 14 major, 4 minor)
- **All CRITICAL/MAJOR addressed**: Yes (12 fully fixed in plan v2; 2 acknowledged with explicit deferral rationale below)

### Changes Made in v2

- **MAJOR #1 — commit count drift (27→38)**: corrected globally (Context §1, Architectural Constraints §1, Facts verified-facts, Slice 9 CHANGELOG note).
- **MAJOR #2 — `### Hacks acknowledged` literal heading**: renamed to `### Hacks / workarounds acknowledged` per cognitive-self-check rule grep.
- **MAJOR #3 — Verification Class missing on TC-V9-5/6/7/8/9**: each TC now carries its Verification Class explicitly in the Deliverables Checklist line.
- **MAJOR #4 — AC-V9-2 non-falsifiable**: reworded so AC-V9-2 asserts a specific row inserted by AC-V9-1 is returned (deterministic round-trip), not "≥1 hit OR empty as long as not error". qa-engineer can now mark PASS/FAIL on a single deterministic check.
- **MAJOR #5 — Slice 1 done-condition depended on unresolved OQ-V9-CUT-1**: OQ-V9-CUT-1 resolved in Slice 1 body (synthetic fixtures committed; operator-snapshot local-only via gitignore).
- **MAJOR #6 — Slice 2 too large**: split into Slice 2a (CLI breaking-change, ~8 tests) + Slice 2b (dual-DB routing, 17 tests). Each stays under the 200-LOC slice cap.
- **MAJOR #7 — Slice 3 too large**: split into Slice 3a (`rrf_fuse_hits` + corrupt-fallback core, 5 tests) + Slice 3b (CLI filter wiring + dual-DB read, 18 tests).
- **MAJOR #8 — Slice 6 bundled three commits + 4+ files**: split into Slice 6a (prompts/rules migration) + Slice 6b (hook scripts) + Slice 6c (installer wiring with idempotency).
- **MAJOR #9 — hook dedup/double-registration**: idempotency requirement added to Slice 6c (dedup-by-command-string equality on `~/.claude/settings.json` `hooks.*` arrays; `cp -n` / `Copy-Item -ErrorAction SilentlyContinue` for the `prompts/` → `~/.claude/` deploy).
- **MAJOR #10 — v5 schema External contracts missing**: added v5-schema-identifiers entry to §Facts External contracts (column names, table names, FK constraints, index names).
- **MAJOR #11 — Slice 10 done-condition too coarse**: 10 concrete per-AC evidence artifacts enumerated under `docs/qa/evidence/claudebase-v0.9/` with naming convention.
- **MAJOR #12 — Slice 11 missing rollback strategy**: 4-case rollback table added (tag-fails / asset-upload-fails / Gate-0-fails / Gate-6-fails) with documented recovery steps.
- **MAJOR #13 — Slice 8 "without breaking daemon" hedge**: daemon-state preservation contract concretised in Slice 8 (6-step protocol: pre-PID capture, version-greater check, daemon stop, atomic binary replace, daemon start, new-PID + status verification).
- **MAJOR #14 — Slice 4 + AC-V9-3 ambiguous "merged"**: "merged" semantics fully spec'd (union by tag, count = sum across DBs, sort by `count DESC, tag ASC`).
- **MINOR #15 — prompts/rules source-vs-deploy ambiguity**: clarified in Slice 6a (prompts/ is repo-shipped; installer deploys to ~/.claude/).
- **MINOR #18 — Slice 9 semver narrative**: addressed via OQ-V9-CUT-3 staying open for explicit operator sign-off; CHANGELOG note (Slice 9 body) handles the ecosystem-confusion mitigation in the meantime.

### Acknowledged Minor Issues

- **MINOR #16 — `## Decisions` block missing per-entry salience tags on `### Inbound validation` entries**: each Inbound validation entry already carries a `Salience: high` tag at the end, which the critic missed in the table-formatting scan. Verified at this pass — no fix needed.
- **MINOR #17 — `### Open questions` in §Facts cites OQ-V9-CUT-N by reference instead of inlining text**: deliberate redundancy choice — full question text is at §Open Questions (one place to update); reviewers triage by salience via the reference. Acknowledged as intentional rather than a deficiency. Salience: low.
