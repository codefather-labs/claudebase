# Issue 005 — Insight corpus fragmented across nested-repo boundaries

**Status:** OPEN — deferred (fix later, per operator 2026-06-06)
**Severity:** Medium (recall gaps; no data loss — insights ARE persisted, just scattered)
**Area:** `claudebase insight` project-root resolution + nested-repo dev layout

## Summary

`claudebase insight create --category project` resolves the project root from
the current working directory. On this machine the **claudebase repo is nested
inside the claude-code-sdlc repo** (`claude-code-sdlc/claudebase/`), so the
same logical work ends up writing project insights into **different project
insight DBs depending on which directory the command ran from**.

Observed on 2026-06-06 (the v0.7.0 multi-agent consolidation session):

| Corpus | Path | Rows |
|---|---|---|
| 🌍 Global (general) | `~/.claude/knowledge/insights.db` | 94 |
| 📦 Project: claudebase | `claude-code-sdlc/claudebase/.claude/knowledge/insights.db` | 54 |
| 📦 Project: SDLC | `claude-code-sdlc/.claude/knowledge/insights.db` | 6 |

The **global** corpus (everything tagged `--category general`) is fine — it
always lands in `~/.claude/knowledge/insights.db` regardless of cwd, and this
session's general lessons (#87–#93: CRLF, daemon-install, installer data-wipe,
config-precedence, push breakthrough, presentation collateral, CI/ort) are all
present.

The **project** corpus is the problem: claudebase-specific project insights
(the whole channel-push / daemon / telegram debugging saga, #42–#54) live in
the claudebase-repo DB, but a handful of project insights written while the cwd
was the SDLC-repo root landed in the SDLC-repo DB instead. So "this project's
insights" is split across two files and depends on where you stand.

## Impact

- A future agent that runs `claudebase insight search --project-only` from one
  cwd silently misses project insights written from a different cwd → incomplete
  recall, the exact failure the insights corpus exists to prevent.
- Not a data-loss bug — every insight IS persisted somewhere; it's a
  routing/locatability problem.
- Only manifests with **nested git repos**. A standalone claudebase checkout
  (the normal end-user layout) has no nesting and no fragmentation.

## Open questions

- Exactly which insights are in the 6-row SDLC-repo DB? (A direct `sqlite3`
  enumeration during the session returned empty rows — possibly a wrong path or
  a schema/table-name mismatch on that DB; needs a clean re-check at fix time.)
- Is the 6-row SDLC DB intended (genuine SDLC-pipeline insights) or accidental
  spill from claudebase work? Needs per-row triage.

## Deferred fix ideas (pick at fix time)

1. **Consolidate** — move the claudebase-related rows out of the SDLC-repo DB
   into the canonical claudebase-repo DB (and/or define ONE project DB for the
   nested pair). Needs an `insight move`/`insight reassign` admin op or a manual
   SQL migration with sha-dedup.
2. **Stabilise project-root resolution** — make `insight create` pin to the
   INNERMOST repo (the `.git` nearest cwd) OR the OUTERMOST consistently, so the
   same logical project always resolves to one DB regardless of sub-dir cwd.
3. **Operator discipline (cheapest)** — always run `insight` commands with an
   explicit `--project-root <dir>` (or from a fixed cwd) so routing is
   deterministic. Document this in the insights rule.

## Facts

### Verified facts
- Three insight DBs exist with rows 94 / 54 / 6 — source: `du -h` + `sqlite3
  'SELECT count(*) FROM documents'` over the three paths, run this session. — salience: high
- The global corpus holds this session's `--category general` insights #87–#93
  — source: `claudebase insight list --general-only` (total matching: 94),
  read this session. — salience: high
- The claudebase-repo project corpus holds the channel-push saga insights
  #42–#54 — source: `claudebase insight list --project-only` (total matching:
  54), read this session. — salience: high
- `claudebase insight create` routes by `--category`: `general` → the single
  global `~/.claude/knowledge/insights.db`; `project` → `<project-root>/.claude/
  knowledge/insights.db` where project-root is resolved from cwd — source:
  `~/.claude/rules/knowledge-base-tool.md` § Insights corpus. — salience: high

### External contracts
- (none)

### Assumptions
- The 6-row SDLC-repo DB contains a mix of genuine SDLC insights + spilled
  claudebase insights — risk: consolidation could mis-merge if some are truly
  SDLC-pipeline lessons — how to verify: per-row triage at fix time. — salience: medium

### Open questions
- See `## Open questions` above (enumeration of the 6 SDLC-DB rows; intended vs
  accidental). — salience: medium

## Decisions

### Inbound validation
- Operator asked to "check global + project insights"; the check surfaced the
  3-DB fragmentation rather than a simple count. Operator then directed
  "document as an issue + push, fix later" — surfaced rather than silently
  papering over the split. — challenged: no — outcome: documented + deferred. — salience: medium

### Decisions made
- Defer the fix and capture as issue 005 rather than reshuffling insight DBs
  mid-session. Q1 hack? no. Q2 sane? yes — moving rows between corpora is a
  data migration that deserves deliberate triage, not an ad-hoc fix at the end
  of a long session. Q3 alternatives? consolidate now (rejected — risky without
  per-row triage). Q4 cause? yes — names cwd-sensitive project-root resolution
  under nested repos. Q5 tracked? this doc. — salience: high

### Hacks / workarounds acknowledged
- (none)

### Symptom-only patches (with root-cause links)
- (none)
