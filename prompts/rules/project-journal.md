# Project Journal

Every project has a journal: the decisions, findings and updates of every earlier session and agent. It is two things at once — the record of **why** the project is the way it is (decisions and findings, so the next session does not re-decide or re-discover them) and the project's **changelog** (updates: what changed, when, where). Read it before you decide, write to it as you work.

## Where it lives and whose it is

- The claudebase daemon owns every journal (`~/.claude/claudebase/journals/`, one file per project). `claudebase journal …` asks the daemon; never open, copy or edit those files yourself.
- The project is the repository of the directory `claudebase run` was started in (its git remote), or that directory itself when it has no remote — so every checkout of a repository, on this machine or a machine joined to this daemon (`claudebase run --daemon <name>`), shares one journal. Every command — from any subdirectory — works on that project's journal and only that one: you never see, and must not try to reach, another project's entries, categories or tags.
- Writing (`add`, `delete`) needs a session started with `claudebase run`. If `journal add` answers that it needs such a session, or that the daemon is not reachable, tell the operator once and carry on without journaling; do not work around it.

## Read — before you decide

On a fresh context window (session start, resume, after compaction), and before any non-trivial decision:

```bash
claudebase journal categories                          # what kinds of entries exist
claudebase journal tags [--category decision]          # the tag vocabulary, most used first
claudebase journal search "<keywords>" --tag <tag>     # ranked, BM25 + semantic
claudebase journal list --category decision --tag <tag> [--since 30d]
claudebase journal get <id>                            # one entry in full
```

Read by category and tag, not by paging through everything. Filters: `--tag` (repeatable, any of), `--category`, `--agent`, `--salience`, `--feature`, `--since 30d|12h|2w`; add `--json` when you parse the output.

An entry you rely on is evidence: cite it as `journal#<id>` (under `### Verified facts` when you emit a `## Facts` block). An entry is what an earlier session believed — if the code now says otherwise, the code wins, and that contradiction is itself a finding to journal.

## Write — as you work

Journal **every decision you make, every finding you uncover and every update you make**, one entry each, when it happens — not in a batch at the end, where the details are already gone.

```bash
claudebase journal add "<entry>" --category <category> --tag <tag> [--tag <tag>] \
  [--salience high|medium|low] [--feature <slug>] [--source-artifact <file#anchor|commit|url>] [--agent <name>]
```

Long text goes on stdin: `claudebase journal add - --category decision --tag x <<'EOF' … EOF`.

What counts:

- **decision** — a choice between alternatives: what was chosen, why, what was rejected and why.
- **finding** — something learned about the code, a system, a tool or the domain that was not obvious: a cause found, a constraint discovered, a behaviour verified.
- **bug** — a defect found: symptom, cause, where, whether fixed.
- **assumption-falsified** — something believed that turned out wrong, and what is true instead.
- **lesson** — a correction from the operator, or a mistake of your own, worth not repeating.
- **update** — a change you made to the project: code, config, docs, dependencies, infrastructure. What changed and where (files, the commit hash once there is one), and why — cite the `journal#<id>` of the decision behind it when there is one. These entries are the changelog: `claudebase journal list --category update --since 2w` answers "what changed lately".

Reuse what `journal categories` lists before inventing a category; add a new one only for a genuinely new kind of entry.

How to write one:

- **Self-contained.** A later session reads it without this conversation: name the component, the file (`path:line`), the command, the version.
- **The why, not only the what.** "Chose WAL mode because concurrent readers blocked the daemon's writer (journal.rs:open); rejected a write queue as more moving parts" — not "switched to WAL".
- **Tags are how it is found again.** 1–4 specific nouns — component, technology, feature (`sqlite`, `daemon`, `telegram`, `install`); reuse existing tags rather than near-synonyms.
- **Salience is retention.** `high` — forever, for load-bearing decisions and constraints; `medium` (default) — a year; `low` — 90 days, for ambient context. Do not mark everything `high`.
- **Reversals cite what they reverse.** "Replaces journal#42: …".

Not for the journal: narration of routine steps (running tests, reading files), restatements of the task, and secrets or credentials of any kind. A commit message does not replace an `update` entry: the journal is searched by tag and category across sessions, git history is not.
