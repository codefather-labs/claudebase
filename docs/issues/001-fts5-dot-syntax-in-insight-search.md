# Issue #001 — `insight search` exits 1 on queries containing `.`

**Status:** OPEN — needs fix in v0.5.1
**Filed:** 2026-05-16
**Severity:** MAJOR
**Surface:** `claudebase insight search` AND `claudebase search` (both lexical mode)
**Discovered by:** DMN reflection pass on `codefather.dev` (persisted as agent-insights doc #—)
**Reference:** [knowledge-base-tool.md § Insights corpus retrieval protocol]

## Symptom

```
$ claudebase insight search "codefather.dev" --top-k 5 --json
error: invalid search query: fts5: syntax error near "."
exit 1
```

Any query containing a literal `.` character — domain names, version
strings, semver tags, dotted method names — hits this. Reproduces in
both `insight search` and the standalone `search` subcommand in
`--mode lexical` (the dense / hybrid modes go through the e5 encoder
and are unaffected at the query level, but the lexical leg in hybrid
mode still trips this).

## Reproduction (exit 1 cases)

```
claudebase insight search "codefather.dev"
claudebase insight search "claudebase v0.5.0"
claudebase insight search "foo.bar.baz"
claudebase search "Stripe.charges.retrieve"  --mode lexical
claudebase search "RFC 7519 §2.1"            --mode lexical
```

## Reproduction (works — bare-token queries)

```
claudebase insight search "codefather"
claudebase insight search "stripe charges"
```

## Root cause

SQLite FTS5's `unicode61` tokenizer treats `.` as a token separator.
Bare-token queries are tokenized cleanly. But when the agent passes a
multi-token query containing a dot, FTS5's `MATCH` grammar parses the
substring `name.subname` as the column-qualifier syntax
(`column-name:term`) rather than as two tokens — and bails with
`syntax error near "."` because the LHS isn't a registered column
name on the `chunks_fts` virtual table.

See: https://www.sqlite.org/fts5.html — Section 3.1 "FTS5 Query Syntax".

## Impact

The `~/.claude/rules/knowledge-base-tool.md` § Insights corpus
retrieval protocol instructs in-scope thinking agents to invoke

```
claudebase insight search "<feature-keywords>" --feature "$FEATURE_SLUG"
```

at task receipt. Agents that derive `<feature-keywords>` from a feature
slug, domain name, or version string containing a `.` get **exit 1
instead of zero results.** The failure is silent in the sense that the
agent's wrapping code may not distinguish "no insights match" from
"query rejected by FTS5". The downstream effect is missed cross-session
memory — the corpus has the data but the agent never sees it.

Concrete affected query patterns observed in the wild during the DMN
pass that surfaced this issue:

- `codefather.dev` (the project the reflection pass ran against)
- `v0.5.0`, `claudebase-v0.5.0` (release tags)
- `0.4.0 → 0.5.0` (version-bump prose) — fails BOTH on the dot AND on
  the unicode arrow

## Proposed fixes (pick one — architect decides)

### Option A — Phrase-wrap user queries (preferred)

In `search::search` (and the equivalent code path in `insight search`),
wrap the user-supplied query in double quotes before passing to FTS5:

```rust
let fts5_query = format!("\"{}\"", query.replace('"', "\"\""));
```

This forces FTS5 to treat the input as a single phrase, bypassing the
column-qualifier grammar. Pros: simplest, agent-transparent. Cons:
loses the ability to use FTS5 operators (`AND`, `OR`, `NEAR`) — which
agents are NOT currently using, so the cost is theoretical.

### Option B — Graceful FTS5-syntax-error → empty result

In `search::search`, catch `SearchError::FtsSyntax` and return
`Ok(Vec::new())` instead of bubbling it up. Caller treats syntax-rejection
as "no hits" with a stderr warning. Pros: backward-compatible for any
operator who currently relies on FTS5 operator syntax. Cons: silent
data-loss when the agent's query was meaningful and just contained a
dot — the agent thinks "nothing matched" when it should have been
"query was rejected by tokenizer".

### Option C — Pre-sanitize via token-stripping

In `cli` layer, before passing query to FTS5, strip `.`, `-`, `:`, `/`
and other unicode61 separator chars (replacing each with a space). The
input `codefather.dev` becomes the two-token query `codefather dev`
which FTS5 ANDs internally. Pros: behaves like a search engine front-end;
keeps the dense / hybrid path unchanged (encoder handles dots fine).
Cons: changes the semantic of multi-dotted queries — `Stripe.charges.retrieve`
becomes `Stripe charges retrieve` which is too lossy for code-symbol
search.

**Mira's recommendation: A** — phrase-wrap. Agents already pass
human-readable keyword strings in practice; nobody is using FTS5
operator syntax through this surface. A is one-line and reversible.
B+C trade silent data loss for query semantics; A trades a feature
nobody uses for a bug everyone hits.

## Test cases (write before fix)

```
TC-FTS5-DOT-1: insight search "codefather.dev" returns the seeded
               reflection insight when one exists with that feature_slug;
               exit 0 with non-empty hits.
TC-FTS5-DOT-2: insight search "" rejects with exit 2 (empty query, not
               an FTS5 syntax error).
TC-FTS5-DOT-3: search "v0.5.0" --mode lexical returns 0 hits (no match)
               with exit 0, NOT exit 1.
TC-FTS5-DOT-4: search --mode hybrid with dotted query falls back to
               lexical only when encoder is unavailable; in encoder-up
               mode the hybrid path produces dense hits even when the
               lexical leg returns empty (via phrase-wrap mismatch).
TC-FTS5-DOT-5: literal-quote edge case: search "say \"hello.world\""
               survives the phrase-wrap escaping (double the inner ").
```

## Acceptance criteria

- All 5 test cases above pass.
- Existing 24 test suites stay green (no regression on `tests/cli_search_e2e_test.rs`
  hybrid + lexical paths).
- One added smoke test in `tests/cli_insight_e2e_test.rs` exercises a
  dotted feature_slug end-to-end (write insight with `--feature foo.bar`,
  query with `insight search "foo.bar"`, assert the hit appears).
- Release as part of v0.5.1 patch bump — no SDLC install.sh bump needed
  (agents continue calling the same subcommand surface).

## Out of scope

- FTS5 operator syntax (`AND` / `OR` / `NEAR`) survival — explicitly
  killed by Option A. If a future feature needs FTS5 operators, add a
  separate `--raw-fts5` flag.
- Whitespace normalization, accent folding, case folding — handled by
  the unicode61 tokenizer already; not affected by this fix.
- Dense / hybrid mode behavior — unaffected (encoder handles arbitrary
  text including dots).

## Facts

### Verified facts

- `claudebase insight search "codefather.dev" --top-k 5 --json` exits 1
  with literal `error: invalid search query: fts5: syntax error near "."`
  — verified in this session by direct invocation, also surfaced by the
  DMN reflection pass that triggered this issue file.
- The agent-prompts integration in 16 SDLC agents instructs them to
  invoke `claudebase insight search "<feature-keywords>" --feature "$FEATURE_SLUG"`
  at task receipt — verified by reading `~/.claude/agents/planner.md`
  line containing `## Insights Corpus (when present)` after `bash
  install.sh --yes --local` finished.
- The FTS5 `unicode61` tokenizer treats `.` as a separator — verified
  against the SQLite FTS5 docs (Section 3.1) cited above.
- The bug applies to lexical-mode searches only at the FTS5 syntax
  level; the dense / hybrid path goes through `encoder::encode_query`
  which does not tokenize dots — verified by tracing
  `src/main.rs::run_insight_search` and `src/main.rs::run_search`.

### External contracts

- **SQLite FTS5** — symbol: `MATCH` operator + `unicode61` tokenizer
  + column-qualifier grammar (`column:term`) — source:
  https://www.sqlite.org/fts5.html § 3.1 — verified: yes (this issue).

### Assumptions

- All five proposed test cases will land in `tests/cli_insight_e2e_test.rs`
  rather than a new test binary. Risk: if the test count threshold for
  splitting into a second binary has been crossed, the suite layout
  may need a small refactor. How to verify: implementer counts current
  tests in the file pre-change and decides.

### Open questions

- (none) — the fix is one of three concrete options; architect picks one.

## Decisions

### Inbound validation

- User asked for a task file capturing the FTS5-dot bug. The bug was
  surfaced by a DMN reflection pass and verified directly in this
  session. The task file goes to `docs/issues/` (new directory). —
  challenged: yes (briefly: is `docs/issues/` the right home vs a
  GitHub issue?). — outcome: file is the right call — this captures
  the cognitive context (DMN observation + three-option fix + verified
  facts) richer than a GitHub issue body comfortably holds. The user
  can paste the relevant section into a GH issue later if needed.
  — salience: low

### Decisions made

- Filed as `docs/issues/001-...` rather than a GH issue or root
  `TODO.md`. Alternatives: (a) `gh issue create` — rejected because
  the operator hasn't opened any GH issues on this repo yet and a
  one-issue-only registry is heavier than one markdown file; (b)
  `TODO.md` at repo root — rejected because TODO is a bag-of-things,
  this is a structured issue with reproduction + root cause + three
  fix options. Q1-Q5: hack? no. Sane? yes. Alternatives? listed.
  Symptom-or-cause? cause (FTS5 grammar). Root-cause-tracked? yes
  (this file). — salience: medium
- Recommended Option A (phrase-wrap). Alternatives B and C documented
  with their trade-offs so the architect doing the pre-review can
  challenge the recommendation. The recommendation is non-binding. —
  salience: high.

### Hacks / workarounds acknowledged

- (none — the proposed fix is structural, not a band-aid)

### Symptom-only patches (with root-cause links)

- (none)
