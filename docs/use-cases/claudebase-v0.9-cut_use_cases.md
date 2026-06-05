# Use Cases: claudebase v0.9 Cut — Port-Forward v0.7 Insights Surface

> Based on [PRD §19](../PRD.md#§19-claudebase-v09-cut--port-forward-v07-insights-surface)
> and [.claude/plan.md](../../.claude/plan.md).
>
> Feature slug: `claudebase-v0.9-cut`. Branch: `feat/multi-agent-on-v0.6`.
>
> **Scope frame:** Wave A (insights schema v5 + dual-DB routing + project registry + hooks + skill)
> + Wave D (release infrastructure). Wave B and C are deferred to v0.10 by operator decision 2026-06-04.
>
> **Verification-class hint for downstream qa-planner:** UC-V9-CUT-1/2/3/6 are `CLI` + `DB` (Mixed).
> UC-V9-CUT-4/5 are `CLI` (hook process invocation, stdout assertion). UC-V9-CUT-7 is `Mixed`
> (CLI + FS). UC-V9-CUT-8 is `Mixed` (DB + CLI). UC-V9-CUT-9 is `CLI` + `DB` (Mixed).

---

## UC-V9-CUT-1: Operator Creates an Insight via `insight create` After Schema v5 Migration

**Actor**: SDLC pipeline operator (interactive shell, cwd = claudebase project root).

**Preconditions**:
- v0.9 binary is installed at `~/.claude/tools/claudebase/claudebase` (and accessible as `claudebase` via PATH alias).
- `<cwd>/.claude/knowledge/insights.db` either (a) does not yet exist, or (b) exists at any prior schema version (v1 through v4) and will be migrated on first open, or (c) was previously in a corrupt state that the v5 migration repaired (per FR-V9-1.4 — if repair-required, UC-V9-CUT-9 applies instead).
- The `--category`, `--tags`, and `--salience` flags are available on the v0.9 CLI (Slice 2a lands before this UC is testable).

**Trigger**: Operator runs `claudebase insight create --category project --tags v9-cut-smoke --salience high "Mira test"` from the cwd.

### Primary Flow (Happy Path)

1. The binary opens `<cwd>/.claude/knowledge/insights.db`. If the file does not exist, it is created at schema v5. If it exists at v1–v4, the migration `apply_v5_delta_and_backfill()` runs within a single transaction before any write (FR-V9-1.1, FR-V9-1.2).
2. CLI parses the required `--category project` flag — resolves to local-DB routing (FR-V9-2.4).
3. CLI parses the required `--tags v9-cut-smoke` flag — normalises: strips leading `#` if any, lowercases, trims whitespace, deduplicates within this call (FR-V9-2.3). Result: a single tag `"v9-cut-smoke"`.
4. CLI computes `sha256("Mira test")` and checks for an existing `(agent_name, sha256)` row within the last 30 days. None found (first write).
5. CLI begins an `IMMEDIATE` transaction. Inserts one row into `documents` with columns: `category = 'project'`, `project_slug` derived from cwd basename, `salience = 'high'`, `source_type = 'agent-<type>'` (or the caller's `--type` if provided), body = `"Mira test"`.
6. CLI writes chunks for the body into `chunks` and `chunks_fts`. Best-effort populates `chunks_vec` if the e5 encoder is present.
7. After the `documents` row is committed, CLI inserts one row into `insight_tags(doc_id, tag)` with `INSERT OR IGNORE` for the tag `"v9-cut-smoke"` (FR-V9-2.5).
8. Transaction commits. CLI emits stdout: `remembered: doc_id=<N> chunks=1 sha=<prefix> salience=high category=project` and exits 0.

**Postconditions**:
- `<cwd>/.claude/knowledge/insights.db` contains a `documents` row where body includes `"Mira test"` and `category = 'project'`.
- `insight_tags` contains one row `(doc_id=<N>, tag='v9-cut-smoke')`.
- `~/.claude/knowledge/insights.db` (global DB) is NOT written — the local-only write is correct for `--category project` (FR-V9-2.4).
- Exit code is 0. stderr is empty (no error lines).
- SQL assertion: `SELECT count(*) FROM documents WHERE source_type LIKE 'agent-%'` returns ≥ 1.

**FR Coverage**: FR-V9-1.1, FR-V9-1.2, FR-V9-2.1, FR-V9-2.3, FR-V9-2.4, FR-V9-2.5.
**AC Coverage**: AC-V9-1 (primary evidence case).

### Alternative Flows

- **UC-V9-CUT-1-A: `--category general` writes to global DB** — operator runs `claudebase insight create --category general --tags v9-cut-smoke --salience high "Mira test"`. At step 2 the CLI resolves `--category general` → routes to `~/.claude/knowledge/insights.db` (FR-V9-1.5, FR-V9-2.4). If that path does not exist it is created at schema v5. The local-project `<cwd>/.claude/knowledge/insights.db` is NOT written. Postconditions: global DB has the new row; local DB is unaffected. Exit 0.

### Exception Flows

- **UC-V9-CUT-1-E1: Missing `--category` flag → exit 2** — operator runs `claudebase insight create --tags v9-cut-smoke "Mira test"` (omitting `--category`). The CLI parser detects a missing required argument (FR-V9-2.1: `--category` exits 2 if absent). Stderr emits: `error: the following required arguments were not provided: --category <general|project>`. Exit code is 2. No DB write occurs.
- **UC-V9-CUT-1-E2: Missing `--tags` flag → exit 2** — operator runs `claudebase insight create --category project "Mira test"` (omitting `--tags`). The CLI parser detects a missing required argument (FR-V9-2.1: `--tags` exits 2 if absent). Stderr emits: `error: the following required arguments were not provided: --tags <TAGS>`. Exit code is 2. No DB write occurs.

### Edge Cases

- **UC-V9-CUT-1-EC1: Tag with `#` prefix and uppercase normalised correctly** — operator runs `claudebase insight create --category project --tags "#FOO,foo" --salience medium "dedup test"`. At step 3 the normaliser processes `["#FOO", "foo"]`: (a) `"#FOO"` → strip `#` → `"FOO"` → lowercase → `"foo"`; (b) `"foo"` → `"foo"`. After deduplication the result is a single tag `"foo"`. Only one row is inserted into `insight_tags` (FR-V9-2.3). SQL assertion: `SELECT count(*) FROM insight_tags WHERE tag = 'foo'` returns 1 (not 2).
- **UC-V9-CUT-1-EC2: Whitespace-only tag in list** — operator runs `--tags "v9-cut-smoke, , other"`. The normaliser trims each entry; the empty string entry after trimming is dropped. Result: tags `["v9-cut-smoke", "other"]`. No empty-tag row is written. Exit 0.

---

## UC-V9-CUT-2: Operator Searches Insights by Tag After Successful Create

**Actor**: SDLC pipeline operator (interactive shell), following UC-V9-CUT-1.

**Preconditions**:
- UC-V9-CUT-1's primary flow completed successfully: a row with body `"Mira test"`, tag `"v9-cut-smoke"`, `salience = 'high'`, `category = 'project'` exists in `<cwd>/.claude/knowledge/insights.db`.
- The v0.9 binary with `insight search --tag` filter is installed.

**Trigger**: Operator runs `claudebase insight search "Mira test" --tag v9-cut-smoke --salience high --top-k 3 --json`.

### Primary Flow (Happy Path)

1. CLI opens `<cwd>/.claude/knowledge/insights.db` (local DB, schema v5). Attempts to open `~/.claude/knowledge/insights.db` (global DB) via `open_global_insights_db()` (FR-V9-1.5). If the global DB is present, it opens; if absent, `None` is returned without error.
2. CLI builds the `--tag` filter for each DB: `WHERE chunk_id IN (SELECT doc_id FROM insight_tags WHERE tag IN (?))` with bound parameter `"v9-cut-smoke"` (FR-V9-3.3 — parameterised SQL, NO `format!` string interpolation).
3. CLI applies `--salience high` as an additional `WHERE salience = 'high'` filter on `documents`.
4. CLI executes the query against the local DB, collecting up to `top_k * 4` hits for RRF fusion headroom.
5. If the global DB is present, CLI executes the same query against it, collecting hits.
6. CLI calls `rrf_fuse_hits(local_hits, global_hits, 3)` keyed on `(source_corpus, chunk_id)` (FR-V9-3.1). If the global DB was absent, `global_hits` is empty — the fused result equals the local result.
7. CLI emits to stdout a JSON array of up to 3 hits. Each hit contains at minimum `{"doc_id": <N>, "snippet": "...", "salience": "high", ...}`. The hit created in UC-V9-CUT-1 is in this array. Exit code 0. stderr is empty.

**Postconditions**:
- stdout is a non-empty JSON array.
- At least one element contains the substring `"Mira test"` in the `snippet` or `body` field.
- At least one element has `"salience": "high"`.
- Exit code is 0.
- stderr does NOT contain `error: index database invalid`.

**FR Coverage**: FR-V9-3.1, FR-V9-3.2, FR-V9-3.3, FR-V9-3.4, FR-V9-3.5.
**AC Coverage**: AC-V9-2 (primary evidence case).

### Alternative Flows

- **UC-V9-CUT-2-A: `--tag` flag repeated (multiple tags, OR-semantics)** — operator runs `claudebase insight search "Mira" --tag v9-cut-smoke --tag other-tag --json`. At step 2 the filter becomes `WHERE tag IN (?, ?)` bound to `["v9-cut-smoke", "other-tag"]`. The UC-V9-CUT-1 row (which has tag `v9-cut-smoke`) is returned because it matches one of the OR conditions. A row with only tag `other-tag` would also be returned. The union is: all rows tagged with ANY of the specified tags (FR-V9-3.4).

### Exception Flows

- **UC-V9-CUT-2-E1: Malformed `--tag` empty string → graceful handling** — operator runs `claudebase insight search "Mira" --tag "" --json`. After normalisation the empty string is trimmed to `""`. The CLI MUST handle this gracefully: either (a) treat it as no-tag filter (skip the empty value), OR (b) emit a warning on stderr and proceed. It MUST NOT crash or emit `error: index database invalid`. Exit code MUST be 0 if at least one DB is readable. The specific handling is implementation-defined; what is PROHIBITED is an unguarded panic or an opaque error exit.

### Edge Cases

- **UC-V9-CUT-2-EC1: Global DB missing → local-only result with stderr warning, NOT exit 1** — operator's `~/.claude/knowledge/insights.db` does not exist. At step 1, `open_global_insights_db()` returns `None` (FR-V9-1.5). At step 6, `global_hits` is empty. The fused result equals the local result. The CLI MAY emit a single `warning: global insights DB not found at ~/.claude/knowledge/insights.db; local-only results` line on stderr. Exit code MUST be 0. The local DB row from UC-V9-CUT-1 is still returned correctly. (FR-V9-3.2: global DB absent → fallback to local-only with stderr warn, NOT exit-1.)

---

## UC-V9-CUT-3: Operator Views Merged Tag Vocabulary via `insight tags`

**Actor**: SDLC pipeline operator (interactive shell).

**Preconditions**:
- `<cwd>/.claude/knowledge/insights.db` (local DB) exists at schema v5 and contains at least the row from UC-V9-CUT-1 (tag `"v9-cut-smoke"`, count 1 in local DB).
- The `claudebase insight tags` subcommand is available in v0.9 (Slice 4).

**Trigger**: Operator runs `claudebase insight tags --json`.

### Primary Flow (Happy Path)

1. CLI opens local DB (`<cwd>/.claude/knowledge/insights.db`). Runs `SELECT tag, count(*) FROM insight_tags GROUP BY tag` against it.
2. CLI attempts to open global DB via `open_global_insights_db()`. If present, runs the same query against it. If absent, the global contribution is zero without error (FR-V9-4.3 — MUST NOT materialise an empty DB file, MUST NOT exit 1).
3. CLI fuses the two `(tag, count)` sets: union by tag string; for tags present in both DBs, `count = sum(local_count, global_count)` (FR-V9-4.2 merged semantics).
4. CLI sorts the result: primary key `count DESC`, secondary key `tag ASC` (deterministic tie-breaking on alphabetical tag, FR-V9-4.2).
5. CLI emits to stdout a JSON array of `{"tag": string, "count": integer}` objects in the specified sort order (FR-V9-4.4). Exit code 0.

**Postconditions**:
- stdout is a non-empty JSON array.
- The array contains `{"tag": "v9-cut-smoke", "count": 1}` (count = 1 because only UC-V9-CUT-1's local-DB row carries this tag; global DB either absent or does not have this tag).
- Sort order satisfies `count DESC, tag ASC` (verifiable by jq: `jq 'to_entries | all(.value.count >= (.key + 1 | . as $i | (.. | numbers | . <= $i)))'` or equivalent assertion).
- Exit code is 0.

**FR Coverage**: FR-V9-4.1, FR-V9-4.2, FR-V9-4.3, FR-V9-4.4.
**AC Coverage**: AC-V9-3 (primary evidence case).

### Alternative Flows

- **UC-V9-CUT-3-A: `--category general` narrows to global DB only** — operator runs `claudebase insight tags --category general --json`. At step 1 the CLI skips the local DB query entirely. At step 2 only the global DB is queried. Result: tag vocabulary from the global DB only. If the global DB is absent, the output is an empty JSON array `[]`. Exit code 0.
- **UC-V9-CUT-3-A2: `--project <slug>` resolves via registry + global** — operator runs `claudebase insight tags --project myproject --json`. CLI calls `resolve_project_path("myproject")` from `src/registry.rs` (FR-V9-5.3) to locate the local DB for that project slug. If found, queries that project's local DB + global DB and fuses. If `resolve_project_path` returns `None` (slug not registered), CLI emits a warning on stderr and falls back to global-DB-only results. Exit code 0 in both cases.

### Exception Flows

*(none — the subcommand has no required arguments; all failure modes produce graceful empty or partial results per FR-V9-4.3)*

### Edge Cases

- **UC-V9-CUT-3-EC1: Overlapping tag `foo` in both local and global DB → count is SUM** — setup: local DB has tag `"foo"` with count 3 (three insight rows tagged `foo`); global DB has tag `"foo"` with count 5. At step 3, the fused `count("foo")` = 3 + 5 = 8. Output contains `{"tag": "foo", "count": 8}`. This is NOT max (8, not 5) and NOT a set-union of rows (8, not 3 or 5). The sum semantics are the AC-V9-3 merged-semantics spec (FR-V9-4.2).
- **UC-V9-CUT-3-EC2: Both DBs empty → output is empty JSON array** — if both the local and global DBs exist but contain zero `insight_tags` rows, the output MUST be `[]` (an empty JSON array), NOT `null`, NOT an error. Exit code 0.

---

## UC-V9-CUT-4: SessionStart Hook Fires and Injects Corpus Reminder

**Actor**: SDLC pipeline operator (Claude Code session); the hook script `claudebase-read-insights-reminder.{sh,ps1}` (installed by the v0.9 installer).

**Preconditions**:
- v0.9 installer has been run (`install.sh --yes` or `install.ps1 -Yes`), wiring `hooks/claudebase-read-insights-reminder.{sh,ps1}` into `~/.claude/settings.json` under `hooks.SessionStart` (FR-V9-7.2).
- The hook scripts exist at the paths configured in `settings.json`.
- The hook scripts are ASCII-only (no bytes > 127) per FR-V9-7.1 / NFR-V9-2.

**Trigger**: Operator resumes a Claude Code session (CC event type: `resume`; also fires on `start` and `compact` per FR-V9-7.3).

### Primary Flow (Happy Path)

1. Claude Code fires the `SessionStart` event with reason `resume` (or `start` or `compact`).
2. CC evaluates each entry in `settings.json → hooks.SessionStart`. For the entry matching this hook's `command` string, CC invokes the hook script as a subprocess.
3. The hook script executes and emits to its stdout a JSON object: `{"hookSpecificOutput": {"additionalContext": "<reminder text>"}}` (FR-V9-7.1).
4. CC receives the hook stdout, parses the JSON, and injects the `additionalContext` value into the session's system context before the agent responds.
5. The injected `additionalContext` contains the literal substring `claudebase insight tags` (FR-V9-7.1 requirement: "containing the literal substring `claudebase insight tags`").
6. The agent (Mira or any spawned sub-agent) can read the reminder in its context.

**Postconditions**:
- The session's system context contains the hook's `additionalContext` text.
- The `additionalContext` contains the literal substring `claudebase insight tags`.
- The hook script's exit code is 0.
- No other `SessionStart` hooks are disrupted.

**FR Coverage**: FR-V9-7.1, FR-V9-7.2, FR-V9-7.3.

### Alternative Flows

- **UC-V9-CUT-4-A: Hook fires on `start` (initial session) AND on `compact` (not just `resume`)** — the `SessionStart` event fires for all three CC event types: `start`, `resume`, and `compact`. The hook scripts themselves need no special-casing for the event type (FR-V9-7.3: "no special-casing required by the hook scripts themselves"). Verification: start a fresh CC session → hook fires; trigger a context compact → hook fires again. The `additionalContext` is injected each time.

### Exception Flows

- **UC-V9-CUT-4-E1: Hook script missing — SessionStart event proceeds without hook** — if the hook script file referenced in `settings.json` does not exist on disk (e.g., installer was not run, or file was manually deleted), CC MUST proceed with the `SessionStart` event normally. The session starts without the `additionalContext` injection. CC does NOT crash, does NOT block, and does NOT emit an error to the operator beyond a possible log-level warning. The installer is the fix; this UC documents that the absence is non-fatal.

---

## UC-V9-CUT-5: UserPromptSubmit Hook Injects Cognitive Self-Check Reminder

**Actor**: SDLC pipeline operator (Claude Code session); the hook script `claudebase-selfcheck-reminder.{sh,ps1}` (installed by the v0.9 installer).

**Preconditions**:
- v0.9 installer has been run, wiring `hooks/claudebase-selfcheck-reminder.{sh,ps1}` into `~/.claude/settings.json` under `hooks.UserPromptSubmit` (FR-V9-6.3).
- The hook scripts exist and are ASCII-only (FR-V9-6.2 / NFR-V9-2).
- The installer ran idempotently: running the installer a second time MUST NOT create a second `UserPromptSubmit` entry for the same command string (FR-V9-6.3 dedup-by-command-string equality).

**Trigger**: Operator submits any prompt to Claude Code on any turn.

### Primary Flow (Happy Path)

1. Operator types a prompt and hits enter.
2. CC fires the `UserPromptSubmit` event before routing the prompt to the agent.
3. CC invokes the hook script subprocess.
4. The hook script emits to stdout: `{"hookSpecificOutput": {"additionalContext": "<self-check reminder text>"}}` (FR-V9-6.1).
5. CC parses the JSON and injects the `additionalContext` into the prompt context as it is delivered to the agent on THIS turn.
6. The agent receives the reminder text alongside the operator's prompt on the FIRST turn (i.e., every turn, not just session start).

**Postconditions**:
- The agent's context for this prompt includes the hook's `additionalContext`.
- The hook script exits 0.
- No duplicate hook entries exist in `settings.json` (idempotency was enforced by installer).

**FR Coverage**: FR-V9-6.1, FR-V9-6.2, FR-V9-6.3.

### Alternative Flows

- **UC-V9-CUT-5-A: Windows `.ps1` hook parses correctly (no `Unexpected token` errors)** — on Windows PowerShell 5.1 (operator's box: Windows 11 Home 10.0.26200), the `.ps1` hook is invoked. Because the file is ASCII-only (NFR-V9-2: all bytes ≤ 127), PowerShell 5.1 parses it correctly even without a BOM. No `Unexpected token` parsing errors appear in the PS console. The hook emits the same JSON output as the `.sh` variant. Verification: `(Get-Content <hook-path> -Encoding Byte | Where-Object { $_ -gt 127 }).Count -eq 0` returns `True` for the `.ps1` file.

### Exception Flows

*(none specified — hook script missing follows the same non-fatal pattern as UC-V9-CUT-4-E1)*

### Edge Cases

- **UC-V9-CUT-5-EC1: Hook execution is non-blocking — prompt still reaches agent even if hook hangs** — if the hook subprocess stalls (e.g., the shell is waiting for input it will never receive), CC MUST apply a timeout and deliver the prompt to the agent without the `additionalContext`. The timeout duration is a CC runtime concern, not this hook's concern. What is prohibited: the prompt being silently swallowed because the hook never returned. The hook scripts are designed to be pure-stdout writers with no I/O waits; this edge case is a safety assertion about CC's timeout semantics.

---

## UC-V9-CUT-6: Project Registry Populated on `claudebase run`

**Actor**: SDLC pipeline operator (interactive shell, cwd = any claudebase-managed project root).

**Preconditions**:
- v0.9 binary is installed.
- `src/registry.rs` has been added and wired into `src/lib.rs` and `src/main.rs` (Slice 5).
- `~/.claude/knowledge/` directory may or may not exist; `projects.json` may or may not exist.

**Trigger**: Operator runs `claudebase run` from a project root directory.

### Primary Flow (Happy Path)

1. `run_claude_with_preset` is called (in `src/main.rs`).
2. As its FIRST action, it calls `upsert_project(&cwd)` (FR-V9-5.4), where `cwd` is the canonicalised current working directory.
3. `upsert_project` derives `project_slug` from the canonicalised basename of `cwd` (FR-V9-5.2 — NEVER from user-supplied input).
4. `upsert_project` reads `~/.claude/knowledge/projects.json` (creating the file if absent). Upserts a `ProjectEntry` for this slug: `{slug, path: <cwd>, last_seen: <now>}`. Writes atomically — writes to a temp file in the same directory, then renames to `projects.json` (FR-V9-5.2 atomic write).
5. If the upsert fails (permission error, disk full), the failure is logged as a warning and `claudebase run` continues normally — the registry write is NON-FATAL (FR-V9-5.4).
6. `run_claude_with_preset` spawns the CC child process as usual.

**Postconditions**:
- `~/.claude/knowledge/projects.json` contains an entry for this project slug with the correct `path` and a fresh `last_seen` timestamp.
- `claudebase run` launches the child process successfully (the registry write does not block launch).
- The per-project `.claudebase/config.json` (from commit `25189bc`) is unaffected — registry is the cross-cutting INDEX; per-project config is the source of truth for `session_id` and `name` (FR-V9-5.5).

**FR Coverage**: FR-V9-5.1, FR-V9-5.2, FR-V9-5.3, FR-V9-5.4, FR-V9-5.5.

### Alternative Flows

- **UC-V9-CUT-6-A: Re-running from same cwd updates `last_seen` only (idempotent)** — operator runs `claudebase run` from the same cwd a second time. At step 4, `upsert_project` finds an existing entry with the same slug. It updates only the `last_seen` timestamp (and optionally the `path` if the canonicalised path differs). No duplicate entry is created. `projects.json` contains exactly one entry for this slug. The child process launch proceeds as usual.

### Exception Flows

*(none separate — registry write failure is handled inline in the primary flow step 5 as a non-fatal warning)*

### Edge Cases

- **UC-V9-CUT-6-EC1: 10-thread concurrent upserts from different shells → valid JSON file, no race** — 10 shell processes all run `claudebase run` from the same cwd simultaneously. Each calls `upsert_project`. Because each write is atomic (write-to-temp + rename), the final `projects.json` is always valid JSON regardless of interleaving (the OS `rename` syscall is atomic on the same filesystem). No partial JSON is written. The entry for this slug appears exactly once in the final file. This edge case is a correctness assertion about the atomic-write design (FR-V9-5.2).

---

## UC-V9-CUT-7: Operator Invokes `/update-claudebase` Skill for a Safe Binary Update

**Actor**: SDLC pipeline operator (Claude Code session invoking the skill).

**Preconditions**:
- The `/update-claudebase` skill file (`prompts/commands/update-claudebase.md`) has been deployed by the installer to `~/.claude/commands/` (Slice 8, FR-V9-8.1).
- The claudebase daemon is currently running (`claudebase daemon status` → `state: "running"`).
- A newer version of the binary is available (e.g., downloadable from the GitHub release page or buildable from the repo).
- The operator's `claudebase --version` currently reports a version LESS than the new binary's version.

**Trigger**: Operator types `/update-claudebase` in the Claude Code session.

### Primary Flow (Happy Path)

1. The agent reads the skill file (`~/.claude/commands/update-claudebase.md`). The skill instructs the agent to first fetch and read the project README (FR-V9-8.3 reads-README-first discipline).
2. Agent reads the README to extract the current install one-liner.
3. **Step 1 (pre-update PID capture):** Agent runs `claudebase daemon status --json` and captures the PID from the JSON response. Stores it as `pre_pid`.
4. **Step 2 (version-greater check):** Agent inspects the new binary's version (e.g., from the downloaded artifact or from a `./target/release/claudebase --version` call) and compares it to the running version (obtainable from `claudebase --version`). If the new version is NOT greater than the current version, the skill aborts — see UC-V9-CUT-7-A (refusal-on-downgrade).
5. **Step 3 (daemon stop):** Agent runs `claudebase daemon stop`. The daemon stops cleanly. Agent waits for the stop to complete (polls `claudebase daemon status` until it returns `stopped` or until timeout). If stop fails — see UC-V9-CUT-7-E1.
6. **Step 4 (atomic binary replace):** Agent replaces the binary at `~/.claude/tools/claudebase/claudebase` atomically: writes new binary to a temp path in the same directory, then renames to the final path (FR-V9-8.2 Step 4).
7. **Step 5 (daemon restart):** Agent runs `claudebase daemon start` using the new binary.
8. **Step 6 (new-PID verification):** Agent runs `claudebase daemon status --json` and reads the new PID. Verifies: (a) the new PID is DIFFERENT from `pre_pid`, AND (b) `status` field is `"running"`. Reports success to the operator.

**Postconditions**:
- `claudebase --version` reports the new version.
- `claudebase daemon status` reports `running` with a different PID than before the update.
- The binary at `~/.claude/tools/claudebase/claudebase` is the new version.
- No partial state: either the full update succeeded (new binary + new daemon PID) or the old binary + daemon state are preserved (if any step failed).

**FR Coverage**: FR-V9-8.1, FR-V9-8.2, FR-V9-8.3, FR-V9-8.4.
**AC Coverage**: AC-V9-4 (partial — this UC covers the skill steps; the version tag and GitHub release are covered by Slice 11).

### Alternative Flows

- **UC-V9-CUT-7-A: Refusal-on-downgrade path** — at step 4, the new binary's version is LESS THAN or EQUAL TO the currently-running version. The skill emits an error message to the operator: `"Refusing downgrade: new binary reports version X.Y.Z which is not greater than currently-running W.X.Y. No changes made."`. The skill exits WITHOUT replacing the binary and WITHOUT stopping the daemon. The daemon continues running at its current state. FR-V9-8.4: "skill MUST abort if fresh binary version is ≤ currently-running version."

### Exception Flows

- **UC-V9-CUT-7-E1: Daemon stop fails → skill aborts BEFORE binary replace** — at step 5, `claudebase daemon stop` returns a non-zero exit code or the daemon does not enter `stopped` state within the timeout. The skill MUST abort the update sequence at this point — it MUST NOT proceed to step 6 (binary replace) with the daemon still running. The operator sees: `"Daemon stop failed. Aborting update. Daemon and binary are unchanged."`. The binary is NOT replaced. The daemon continues in its current state (which may be errored — the operator must investigate separately). No partial state is introduced.

---

## UC-V9-CUT-8: Operator's Pre-Existing Schema-v1 `insights.db` Survives v5 Migration

**Actor**: SDLC pipeline operator (interactive shell); the v0.9 binary performing schema migration.

**Preconditions**:
- An `insights.db` file exists at `<cwd>/.claude/knowledge/insights.db` (or a path the operator specifies) created by a prior claudebase installation at schema v1 (the oldest supported pre-v5 version).
- The file contains N > 0 rows in the `documents` table (a non-empty legacy corpus).
- The `feature_slug` column exists on v1 (this is verified by Slice 1's `PRAGMA table_info` inspection — if absent on v1, the backfill SQL falls back to `COALESCE(NULL, 'untagged')`).

**Trigger**: Operator runs any `claudebase insight` command (e.g., `claudebase insight create --category project --tags tag1 "body"`) which opens the schema-v1 DB and triggers migration.

### Primary Flow (Happy Path)

1. The binary calls `open_or_init_v2` (the v0.9 rewrite from Slice 1) against the schema-v1 DB.
2. The function detects schema version 1. Selects the `v1→v5` migration path (FR-V9-1.2).
3. Within a single transaction, `apply_v5_delta_and_backfill(tx, db_path)` executes:
   - Adds columns `category TEXT NOT NULL DEFAULT 'project'` and `project_slug TEXT` via `ALTER TABLE` (guarded by `PRAGMA table_info` column-existence check to be idempotent).
   - Creates `insight_tags` table and `insight_tags_tag_idx` index via `CREATE TABLE IF NOT EXISTS` / `CREATE INDEX IF NOT EXISTS`.
   - Backfills: `UPDATE documents SET category = 'project'`; `UPDATE documents SET project_slug = feature_slug WHERE feature_slug IS NOT NULL AND feature_slug != ''`; `INSERT OR IGNORE INTO insight_tags (doc_id, tag) SELECT id, COALESCE(feature_slug, 'untagged') FROM documents WHERE source_type LIKE 'agent-%'`.
   - Updates schema version pragma to 5.
4. Transaction commits. All N rows are preserved (row count after migration = row count before migration).
5. The `insight create` (or whatever triggered the open) proceeds against the now-v5 DB.

**Postconditions**:
- Row count in `documents` after migration equals row count before migration (no data loss — NFR-V9-1).
- Every row has `category = 'project'` (backfill applied).
- Every row with a non-NULL, non-empty `feature_slug` has `project_slug = feature_slug`.
- Every agent row (`source_type LIKE 'agent-%'`) has exactly one corresponding row in `insight_tags` with tag = `COALESCE(feature_slug, 'untagged')`.
- `PRAGMA user_version` on the migrated DB = 5.
- No `error: index database invalid` is emitted. Exit code of the triggering command is 0.

**FR Coverage**: FR-V9-1.1, FR-V9-1.2, FR-V9-1.3, FR-V9-1.4.
**NFR Coverage**: NFR-V9-1 (no silent data loss).

### Alternative Flows

- **UC-V9-CUT-8-A: Schema-v2 DB survives v5 migration** — same as primary flow but starting from schema v2. The `v2→v5` path applies. Any columns that were added in v2 already exist; the `ALTER TABLE` guards (`PRAGMA table_info` column-existence checks) ensure they are not re-added. All rows preserved.
- **UC-V9-CUT-8-A2: Schema-v3 DB survives v5 migration** — same as primary flow but starting from schema v3. `v3→v5` path.
- **UC-V9-CUT-8-A3: Schema-v4 DB survives v5 migration** — same as primary flow but starting from schema v4. This is the most common real-world case (v0.6 baseline initialises at v4 per plan.md Facts). The `v4→v5` path applies only the delta: adds `insight_tags` table + index + the category/project_slug columns if not already present via `ALTER TABLE` + backfills.

### Edge Cases

- **UC-V9-CUT-8-EC1: Mixed-row DB — some rows already at v4 schema by accident → backfill is idempotent** — an `insights.db` where some rows were manually set to `category = 'project'` before the migration (e.g., the DB was partially migrated by an experimental binary). The `UPDATE documents SET category = 'project'` is a no-op for rows that already have the value; the `INSERT OR IGNORE INTO insight_tags` is a no-op for `(doc_id, tag)` pairs that already exist (the PRIMARY KEY constraint). The migration completes without errors, without duplicate rows, and with all rows preserved.

---

## UC-V9-CUT-9: Operator's Corrupt `insights.db` Hits Repair-Required Exit

**Actor**: SDLC pipeline operator (interactive shell); operator's `insights.db` is in a state that returns `error: index database invalid` on every operation under the pre-v0.9 binary.

**Preconditions**:
- An `insights.db` file exists at the operator's known corrupt path (`C:\Users\madwh\.claude\knowledge\insights.db` per R-V9-CUT-3 in `docs/plans/claudebase-v0.9-product-plan.md`).
- The v0.9 binary's `validate_schema_inner()` has been extended to validate schema versions 1 through 5 (FR-V9-1.4).
- The DB's structural corruption is beyond what the v5 migration can repair in-place (e.g., unrecognised schema version, or structural corruption detected by `validate_schema_inner()`).

**Trigger**: Operator runs any `claudebase insight` command that opens the corrupt DB.

### Primary Flow — Repair-Required Exit (when in-place repair fails)

1. The binary calls `open_or_init_v2` against the corrupt DB.
2. `validate_schema_inner()` detects structural corruption or an unrecognised schema version (not in `1..=5`).
3. The binary MUST NOT silently corrupt data (NFR-V9-1: no silent data loss).
4. The binary exits 1 with the EXACT literal stderr line: `error: index database invalid; run \`claudebase ingest --reset\` to recover` (FR-V9-1.4 — the exact literal is a contract; paraphrasing is a FAIL).
5. No partial write occurs. The `insights.db` file is either unchanged or left in a consistent (not partially-written) state.

**Postconditions**:
- Exit code is 1 (non-zero, as required by FR-V9-1.4).
- stderr contains the exact literal: `error: index database invalid; run \`claudebase ingest --reset\` to recover`.
- stdout is empty (no spurious output).
- The `insights.db` file is not silently corrupted or truncated.

**FR Coverage**: FR-V9-1.4, NFR-V9-1.

### Alternative Flows

- **UC-V9-CUT-9-A: Successful in-place repair path (migration path b)** — if the DB's corruption is limited to an outdated schema version that the v5 migration CAN repair (e.g., the schema is at v1 or v2 with valid table structure), the primary flow of UC-V9-CUT-8 applies instead of this UC. The operator does NOT see the repair-required message. The binary repairs the DB in place, preserves rows, and the triggering command proceeds normally. This is the PREFERRED outcome (backward-compat MANDATE: prefer repair over error). The repair-required exit (this UC) fires ONLY when in-place repair is not possible.
- **UC-V9-CUT-9-A2: Operator runs `claudebase ingest --reset` per the stderr message → DB is repaired** — after seeing the repair-required exit, operator runs `claudebase ingest --reset`. This wipes the corrupt DB, re-initialises it at schema v5, and exits 0. A subsequent `claudebase insight create --category project --tags tag1 "body"` succeeds with exit 0. Postcondition: the operator has a functional (empty) v5 DB; historical rows from the corrupt DB are lost (the operator chose the reset path; this is acceptable because the alternative was a non-functional DB).

### Exception Flows

- **UC-V9-CUT-9-E1: Repair-required exit code is non-zero AND stderr message is exact literal** — this is a first-class test case, not just a postcondition. The exit code MUST be 1 (any non-zero exit code is acceptable, but 1 is conventional). The stderr MUST contain the EXACT string `error: index database invalid; run \`claudebase ingest --reset\` to recover` — not a partial string, not a paraphrase, not a different error message. A test that checks for `exit_code != 0` AND `stderr.contains("error: index database invalid; run")` is a sufficient assertion. A test that ONLY checks `exit_code != 0` without checking the message literal is insufficient and would not catch a regression where the message was changed.

---

## Facts

### Verified facts

- PRD §19 read this session from `docs/PRD.md` lines 1303–1601 via Read tool — source: Read tool this session — salience: high.
- `.claude/plan.md` (265 lines) read this session in full — source: Read tool this session — salience: high.
- Existing use-cases files confirmed: `agent-chat-daemon_use_cases.md`, `agent-insights-base_use_cases.md`, `multi-agent-telegram-on-v0.6_use_cases.md` — source: Glob tool this session — salience: medium. None of the three files covers the `claudebase-v0.9-cut` domain (schema v5 migration, dual-DB routing, tag vocabulary, hooks, skill update) — creating a new file is correct per the ba-analyst process rules.
- The 9 user stories in PRD §19.2 are numbered 1–9 with direct mapping to FR-V9-1 through FR-V9-9 — source: PRD §19.2 lines 1324–1332 (Read this session) — salience: high.
- Schema v5 delta DDL (column names, table name, FK constraint, index name) read from PRD §19.7 lines 1480–1515 — source: Read tool this session — salience: high.
- FR-V9-2.1: `--category` and `--tags` are REQUIRED flags (exit 2 if absent); `--salience` is an existing optional flag (not introduced as required by v0.9) — source: PRD §19.3 FR-V9-2.1 line 1350 (Read this session) — salience: high.
- FR-V9-3.3: tag filter MUST use parameterised SQL `WHERE chunk_id IN (SELECT doc_id FROM insight_tags WHERE tag IN (?,?,...))` — NO `format!()` string interpolation — source: PRD §19.3 FR-V9-3.3 line 1360 (Read this session) — salience: high.
- FR-V9-4.2: "Merged" semantics = union by tag, count = SUM per tag, sort by `count DESC, tag ASC` — source: PRD §19.3 FR-V9-4.2 line 1367 (Read this session) — salience: high.
- FR-V9-7.1: SessionStart hook `additionalContext` MUST contain literal substring `claudebase insight tags` — source: PRD §19.3 FR-V9-7.1 line 1389 (Read this session) — salience: high.
- NFR-V9-2: ASCII-only constraint on `.ps1` hooks (all bytes ≤ 127); caused a production breakage in v0.7 fixed by commit `e43ca12` — source: PRD §19.4 NFR-V9-2 lines 1415–1416 (Read this session) — salience: high.
- NFR-V9-1: No silent data loss in schema migration — source: PRD §19.4 NFR-V9-1 lines 1414–1415 (Read this session) — salience: high.
- R-V9-CUT-3: Operator's `insights.db` at `C:\Users\madwh\.claude\knowledge\insights.db` is currently corrupt (returns `error: index database invalid; re-ingest required` on every `insight create`) — confirmed live this session: `claudebase insight search` against that path returned the expected error — salience: high.
- FR-V9-5.2: `project_slug` derived from canonicalised basename of cwd — NEVER from user-supplied input — source: PRD §19.3 FR-V9-5.2 line 1374 (Read this session) — salience: high.
- FR-V9-5.4: `upsert_project` call MUST be non-fatal; registry write failure MUST log warning and continue — source: PRD §19.3 FR-V9-5.4 lines 1376–1377 (Read this session) — salience: medium.
- FR-V9-8.2: Six-step daemon-state preservation contract: pre-PID capture → version-greater check → daemon stop → atomic binary replace → daemon start → new-PID+status verification — source: PRD §19.3 FR-V9-8.2 lines 1397–1403 (Read this session) — salience: high.
- Knowledge base `index.db` exists but has 0 documents; corpus scope relevance = No overlap (task domain: software use-case specification; no domain books indexed). Topical queries silently skipped per knowledge-base-tool.md § Corpus scope relevance protocol — source: `claudebase status --json` this session — salience: low.
- Insights corpus (`insights.db`) returned `error: index database invalid; re-ingest required` on insight search query this session — consistent with the known corrupt state (R-V9-CUT-3). Zero insights loaded from corpus — salience: low.

### External contracts

- **`claudebase insight create` CLI flags (v0.9 planned shape)** — symbol: REQUIRED `--category <general|project>`, REQUIRED `--tags <comma-separated>`, optional `--salience <high|medium|low>`, optional `--project <slug>` — source: PRD §19.3 FR-V9-2.1 line 1350 and plan.md Slice 2a lines 76–77 (both Read this session) — verified: yes (cited from PRD and plan, not from training data) — salience: high.
- **SQLite `INSERT OR IGNORE INTO insight_tags(doc_id, tag)`** — symbol: `INSERT OR IGNORE` conflict-resolution clause; idempotency via PRIMARY KEY `(doc_id, tag)` — source: PRD §19.7 lines 1509–1513 (Read this session) and PRD §19.3 FR-V9-2.5 line 1354 — verified: yes — salience: high.
- **`rrf_fuse_hits(local, general, top_k)` signature** — symbol: `pub fn rrf_fuse_hits(local: Vec<SearchHit>, general: Vec<SearchHit>, top_k: usize) -> Vec<SearchHit>` keyed on `(source_corpus, chunk_id)` — source: PRD §19.3 FR-V9-3.1 line 1358 and plan.md Slice 3a line 80 (both Read this session) — verified: yes — salience: high.
- **`validate_schema_inner()` extended to `1..=5`** — source: PRD §19.3 FR-V9-1.4 line 1345 (Read this session) — verified: yes — salience: high.
- **`resolve_global_insights_path()` and `open_global_insights_db()` signatures** — symbol: returns `~/.claire/knowledge/insights.db` (resolved as `$HOME/.claude/knowledge/insights.db`); `open_global_insights_db()` returns `Option<Connection>` — source: PRD §19.3 FR-V9-1.5 line 1346 (Read this session) — verified: yes — salience: high.
- **`upsert_project(root: &Path) -> Result<(), String>` and `resolve_project_path(slug: &str) -> Option<PathBuf>`** — symbol: atomic write-to-temp + rename, slug = canonicalised basename of root — source: PRD §19.3 FR-V9-5.2/5.3 lines 1374–1376 (Read this session) and plan.md Facts External contracts lines 194–195 — verified: yes — salience: high.
- **FR-V9-1.4 repair-required exit — exact stderr literal** — symbol: `error: index database invalid; run \`claudebase ingest --reset\` to recover` — source: PRD §19.3 FR-V9-1.4 line 1345 (Read this session) — verified: yes — salience: high.
- **`insight tags --json` output shape** — symbol: JSON array of `{"tag": string, "count": integer}` objects — source: PRD §19.3 FR-V9-4.4 line 1369 (Read this session) — verified: yes — salience: high.
- **AC-V9-3 merged semantics assertion via jq** — symbol: jq assertion `sort_by(-.count, .tag)` equivalence — source: PRD §19.5 AC-V9-3 line 1428 (Read this session) — verified: yes (PRD text specifies "sort order verified via jq assertion") — salience: medium.

### Assumptions

- The `--salience` flag on `insight create` was NOT made required in v0.9 (it remains optional from the v0.6 baseline). Risk: if v0.9 implementation makes `--salience` required (exit 2 if absent), UC-V9-CUT-1's primary flow as written would be correct (the example command includes `--salience high`), but the exception flows E1/E2 would need an additional E3 for missing `--salience`. How to verify: Slice 2a implementer confirms whether `--salience` is required or optional in the final CLI shape. Salience: medium.
- `claudebase daemon status --json` returns a `pid` field in its JSON output that the `/update-claudebase` skill can capture in UC-V9-CUT-7. Risk: if the JSON shape does not include `pid`, step 3 and step 8 of the 6-step protocol cannot be verified as specified. How to verify: implementer inspects the daemon status JSON shape during Slice 8. Salience: medium.
- The SessionStart hook fires on all three event types (`start`, `resume`, `compact`) without any special-casing in the hook scripts themselves (per FR-V9-7.3). Risk: if CC's `SessionStart` event fires only on `resume` and not on `start` or `compact`, UC-V9-CUT-4-A would fail. How to verify: Slice 7 implementer tests all three event types by starting a fresh session, resuming a session, and triggering a compact. Salience: medium.
- The backfill SQL `SELECT id, COALESCE(feature_slug, 'untagged') FROM documents WHERE source_type LIKE 'agent-%'` assumes that the `feature_slug` column exists in all four pre-v5 schema versions (v1 through v4). Risk: if `feature_slug` was added in v2 or v3 and is absent in v1, the SELECT fails with a column-not-found error during v1→v5 migration. How to verify: Slice 1 implementer inspects the v1 schema via `PRAGMA table_info(documents)` against the synthetic-v1 fixture before running backfill. If absent, the backfill SELECT is guarded with a column-existence check and falls back to `'untagged'` for all v1 rows. Salience: medium.

### Open questions

- OQ-UC-V9-CUT-1: Whether the `--salience` flag on `insight create` is optional or required in the v0.9 final implementation — needs: Slice 2a implementer decision. Governs whether UC-V9-CUT-1-E3 (missing `--salience`) needs to be added. Salience: medium.
- OQ-UC-V9-CUT-2: Whether `claudebase daemon status --json` includes a `pid` field in its current JSON output — needs: implementer inspection of the daemon status handler. Governs the testability of UC-V9-CUT-7's pre/post PID comparison steps 3 and 8. Salience: medium.
- OQ-UC-V9-CUT-3: Exact normalisation behaviour for an all-whitespace tag string `" "` — the PRD says "trim whitespace" but does not explicitly state what happens when the trimmed result is an empty string. Whether this produces exit 2 or silently drops the tag. Needs: Slice 2a implementer decision. Salience: low.

## Decisions

### Inbound validation

- Task received: write `docs/use-cases/claudebase-v0.9-cut_use_cases.md` using PRD §19 as source of truth and the 9 required UC families as the minimum scope. Challenged: yes — verified that the 9 required UC families map 1:1 to the 9 PRD user stories and that no existing use-cases file covers this domain. The task is coherent. Outcome: proceeded as-is. Salience: high.
- The task prompt specifies that `--salience` is part of the `insight create` command in UC-V9-CUT-1. Cross-checked against PRD FR-V9-2.1 (lines 1350–1351): only `--category` and `--tags` are listed as REQUIRED with exit 2; `--salience` is NOT listed as a new required flag. This means `--salience high` in the example command is valid (the flag exists from the v0.6 baseline) but its omission is NOT an error condition. The E1/E2 error flows correctly capture only the two REQUIRED flags. Outcome: proceeded with the task prompt's example command as-is; the distinction is surfaced as OQ-UC-V9-CUT-1 and as an Assumption. Salience: high.
- Confirmed: no upstream errors in PRD §19 or plan.md that this document would amplify. The two sources are mutually consistent (PRD §19 was written by prd-writer FROM plan.md; the two Facts blocks confirm shared source identifiers). Outcome: proceeded. Salience: medium.

### Decisions made

- Created a NEW file (`claudebase-v0.9-cut_use_cases.md`) rather than updating an existing file — rationale: none of the three existing use-case files covers the domain of schema v5 migration, dual-DB routing, tag vocabulary, Claude Code hooks, or the `/update-claudebase` skill. The domains are distinct. Q1 hack? no | Q2 sane? yes | Q3 alternatives? updating `agent-insights-base_use_cases.md` — considered and rejected because that file covers the §16 insights base; the v0.9 use cases extend the CLI surface in breaking ways (new required flags, new subcommands, new routing) that deserve a standalone document. Q4 cause | Q5 n/a. Salience: high.
- UC numbering scheme `UC-V9-CUT-N` chosen (matching the task prompt and consistent with the plan's `R-V9-CUT-N` risk numbering). Q1 hack? no | Q2 sane? yes (feature-slug-derived prefix per project convention) | Q3 alternatives? `UC-CUT-N` (shorter) — rejected (ambiguous across features). Salience: low.
- UC-V9-CUT-9 primary flow documents the repair-required exit (not in-place repair) as the PRIMARY flow, with in-place repair (UC-V9-CUT-8's scenario) documented as UC-V9-CUT-9-A. Rationale: the task prompt explicitly specified this as the primary scenario for UC-9. The in-place repair IS the preferred outcome per the backward-compat MANDATE, but the naming follows the task's intent. Q1 hack? no | Q2 sane? yes | Q3 alternatives? swap primary/alt — rejected (task prompt is explicit about the ordering). Salience: medium.
- Parameterised SQL requirement (FR-V9-3.3: NO `format!()` into SQL for tag filters) surfaced explicitly in UC-V9-CUT-2 primary flow step 2 and in the Facts External contracts. Decision: this is a security constraint (NFR-V9-4) that the qa-planner needs to see in the use cases to write an appropriate SQL-injection prevention test case. Q1 hack? no | Q2 sane? yes | Q3 n/a — it is a mandatory requirement, no alternative to document. Salience: high.
- `resolve_global_insights_path()` return value documented as `~/.claude/knowledge/insights.db`. The PRD at FR-V9-1.5 line 1346 uses the exact wording `~/.claire/knowledge/insights.db` which appears to be a typo in the PRD for `~/.claude/knowledge/insights.db`. The plan.md and all other references use `~/.claude/knowledge/insights.db`. This document uses the correct path. Surfaced as an inconsistency to flag for the qa-planner. Q1 hack? no | Q2 sane? yes (correct the typo in the use-case document; the PRD typo is an open finding for the prd-writer) | Q3 n/a. Salience: medium.

### Hacks / workarounds acknowledged

- (none)

### Symptom-only patches (with root-cause links)

- (none — this use-case document does not implement patches; it documents scenarios. The root-cause tracking for the v0.7/v0.8 brokenness is in `.claude/plan.md` §Symptom-only patches and `docs/plans/claudebase-v0.9-product-plan.md` §1, not in this document.)
