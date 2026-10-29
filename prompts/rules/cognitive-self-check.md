# Cognitive Self-Check Protocol

Three checks, run around everything you emit. Order: Protocol 3 on receiving a task → Protocols 1 and 2 while you write, per claim and per decision → the `## Facts` and `## Decisions` blocks in the output.

| Protocol | Checks | Prevents | Output |
|---|---|---|---|
| 3 — Inbound | the task you were given | propagating an upstream error by executing it mechanically | `### Inbound validation` |
| 1 — Facts | every claim about external state | *fact-shaped lies*: an assumption shipped as fact (a status enum "PENDING" from memory; the API returns "in_progress") | `## Facts` |
| 2 — Decisions | every non-trivial decision | *decision-shaped hacks*: a band-aid shipped as the fix | `## Decisions` |

## Protocol 3 — Inbound validation (first, on task receipt)

The author of the prompt — user, upstream agent, plan slice, PRD, QA case, fix directive — is fallible.

1. **Is the task nonsensical?** Incoherent, disproportionate, or contradicting something else in the same context (PRD says X, plan says Y)?
2. **Is the upstream decision an error?** Run Protocol 2 on it. A slice that says "catch Exception and log a warning" to fix a crash is a hack; do not execute it as given.
3. **What justifies it?** A requirement, a scenario, a reported problem. "User instruction, no further context" is acceptable, but record it as such.
4. **Would executing it amplify an error elsewhere in the chain?** Only errors in your own blast radius (e.g. the plan cites UC-3, but UC-3 describes a different feature).

Push back on a task that fails 1 or 2, or where 4 has a clear answer: record the objection under `### Inbound validation`, then ask the user (interactive) or return BLOCKED (non-interactive). Push-back is not failure; silently executing nonsense is.

## Protocol 1 — Fact vs assumption (every claim)

1. **Source?** `file:line` you Read, command output, PRD §N, commit hash, an upstream `## Facts` entry; for third-party systems, the docs URL, SDK version + symbol path, OpenAPI/proto file:line, or an actual call. Memory of a similar system or training data is **not** a source.
2. **Verified this session?** Memory from before a compaction or an earlier session is stale. Re-read before acting on it.
3. **What am I assuming?** Surface it, especially field names, enum values, error codes, request/response shapes, signatures, defaults, limits, auth, version-specific behaviour.
4. **Is the assumption labelled?** A claim that fails 1 or 2 is an assumption. It goes under `### Assumptions`, or under `### External contracts` with `verified: no — assumption`. An unlabelled assumption is a fact-shaped lie.

**External contracts are the reason this rule exists.** For any third-party API, SDK, library or service: cite the exact source with version, and record the symbol verbatim (exact casing; `snake_case` stays `snake_case`). If you cannot verify it, the integration does not proceed without an explicit `verified: no — assumption` entry carrying the risk and how to verify it, or an `### Open questions` entry for the user.

## Protocol 2 — Decision quality (every non-trivial decision)

1. **Hack?** A workaround is acceptable only when it is labelled as one and has a tracked removal path. A workaround presented as the fix is a hack (`setTimeout` over a race, catch-and-log "shouldn't happen", copy-paste to dodge a refactor).
2. **Sane?** Would a senior engineer reading it in 6 months call it nonsense? Is the complexity proportional? No plugin system for two integrations, no event bus for two services, no class hierarchy for a switch.
3. **Alternatives?** Weigh 2–3 options and pick the most logical, maintainable and current one, giving the reasons. "I remembered it" is not a reason.
4. **Symptom or cause?** Retries on a flaky test, a silenced deprecation, or a NULL check without asking why the value is NULL all treat the symptom.
5. **Root cause tracked?** A symptom-only patch is sometimes right (the hotfix before a demo), but only with the root cause logged where someone will find it.

A decision that fails 1, 2 or 3 is reconsidered, not committed. A decision that fails 4 must satisfy 5; otherwise it is an unlabelled hack.

## Output blocks

Every in-scope artifact carries both blocks, `## Decisions` immediately after `## Facts`. Use these exact headings, since reviewers grep for them, and keep every subsection; write `(none)` when one is empty. List only load-bearing entries, not a read-log: if removing an entry would not change the verdict, it does not belong. A mechanical decision forced by upstream constraints needs no entry.

```
## Facts

### Verified facts
- [fact] — source: [file:line | command output | PRD §N | commit | journal#id | upstream ## Facts] — salience: [high|medium|low]

### External contracts
- [API/SDK/library] — symbol: [exact name] — source: [docs URL | SDK version + symbol path | spec file:line] — verified: [yes | no — assumption] — salience: [...]

### Assumptions
- [assumption] — risk: [what breaks if wrong] — how to verify: [...] — salience: [...]

### Open questions
- [question] — needs: [user decision | architect call | external research | follow-up agent] — salience: [...]

## Decisions

### Inbound validation
- [what I was asked / given] — challenged: [yes | no — why] — outcome: [proceeded as-is | pushed back: ... | escalated to user] — salience: [...]

### Decisions made
- [decision] — alternatives rejected: [...] — Q1-Q5: [hack? no | sane? yes | alternatives? listed | symptom-or-cause? cause | root-cause-tracked? n/a] — salience: [...]

### Hacks / workarounds acknowledged
- [hack] — why it's a hack: [...] — removal path: [...] — salience: [...]

### Symptom-only patches (with root-cause links)
- [patch] — symptom: [...] — root cause that remains: [...] — tracked at: [...] — salience: [...]
```

`### External contracts` is mandatory whenever a third-party identifier appears in the artifact; write `(none)` only if there are none. Hedging in prose ("for now", "quick fix", "workaround", "TODO: fix properly") without a matching `### Hacks / workarounds acknowledged` entry is the decision-shaped lie this rule exists to catch.

**Salience** decides what reviewers audit first (high → low) and how long the entry lives in the project journal:

- **high**: if it is wrong, the whole artifact is wrong (the enum an API returns, the database a feature targets). The journal keeps it forever. Use sparingly.
- **medium**: affects one slice or decision. Kept 365 days. This is the default.
- **low**: context only. Kept 90 days.

## Project journal

The blocks explain one artifact; the journal carries them across sessions (`~/.claude/rules/project-journal.md`). Before deciding, search it (`claudebase journal search "<keywords>" --tag <tag>`) and cite the hits you rely on as `journal#<id>`. After deciding, add one entry per decision, finding or falsified assumption, plus an `update` entry per change, using the same salience tag.

The blocks map onto journal flags, so they outlive the artifact:
- An `### Assumptions` entry goes in with `--unverified`. It is closed only after a separate `Verifies journal#N` entry says what was checked, how, under which conditions and how many times (with `--proof` when the project is evidence-first), or with `--abandon --resolution` when the check is dropped.
- An `### Open questions` entry goes in with `--question`.
- A hack's removal path goes in with `--todo`.
- A contradiction between sources goes in with `--disputed`.

Before relying on an entry, read `claudebase journal get <id>`. Its related block shows what replaces it and the closest entries by meaning. A `deprecated` entry is not evidence, and an `unverified` or `disputed` one belongs under `### Assumptions`, not `### Verified facts`.

## Scope

**In scope.** Every thinking agent, the main session included. The named SDLC agents put the blocks here:

| Where the blocks go | Agents |
|---|---|
| In their file | prd-writer (PRD section), ba-analyst (use-cases), qa-planner (test cases), planner (`.claude/plan.md`), resource-architect, role-planner, release-engineer (release notes) |
| At the top of their stdout report | architect, security-auditor, code-reviewer, verifier, refactor-cleaner, qa-engineer, red-team, consolidator, reflection |

Extra rules for some of them:

- qa-engineer verdicts cite the evidence: a PASS cites the tool output, a FAIL the expected-vs-actual mismatch, a BLOCKED the reasoning.
- Each red-team objection cites the line it attacks.
- Each consolidator drift finding cites two divergent `file:line` points.
- Reflection's observations cite concrete evidence.

**Exempt** (deterministic executors): test-writer, build-runner, e2e-runner, doc-updater, changelog-writer.

**Plan Critic** checks the file artifacts of the current cycle only:
- MAJOR: a missing `## Facts` or `## Decisions` block.
- MAJOR: an external identifier with no `### External contracts` entry.
- MAJOR: a decision, hack or symptom patch described in prose but absent from its subsection.
- MAJOR: a silently resolved contradiction between upstream sources.
- MINOR: an empty subsection without `(none)`.
- MINOR: a vague contract source.
- MINOR: a load-bearing decision without its Q1–Q5 tail.

Artifacts from earlier cycles are exempt until someone re-edits them; an artifact with a missing or malformed `Date:` counts as current.

## TL;DR

- **Inbound:** «А не херню ли мне тут пишут?» — challenge the task before executing it.
- **Facts:** «Это факт или чьи-то фантазии?» — cite a source verified this session, or label the claim an assumption.
- **Decisions:** «А не делаю ли я бред?» — hack? sane? alternatives? symptom or cause?
