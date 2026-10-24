# Project Journal

Every project has a journal: the decisions and findings of every earlier session and agent, kept so the next one does not re-decide or re-discover them. It is the project's memory across sessions — read it before you decide, write to it as you work.

## Where it lives and whose it is

- The claudebase daemon owns every journal (`~/.claude/claudebase/journals/`, one file per project). `claudebase journal …` asks the daemon; never open, copy or edit those files yourself.
- The project is the directory `claudebase run` was started in. Every command — from any subdirectory — works on that project's journal and only that one: you never see, and must not try to reach, another project's entries, categories or tags.
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

Journal **every decision you make and every finding you uncover**, one entry each, when it happens — not in a batch at the end, where the details are already gone.

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

Reuse what `journal categories` lists before inventing a category; add a new one only for a genuinely new kind of entry.

How to write one:

- **Self-contained.** A later session reads it without this conversation: name the component, the file (`path:line`), the command, the version.
- **The why, not only the what.** "Chose WAL mode because concurrent readers blocked the daemon's writer (journal.rs:open); rejected a write queue as more moving parts" — not "switched to WAL".
- **Tags are how it is found again.** 1–4 specific nouns — component, technology, feature (`sqlite`, `daemon`, `telegram`, `install`); reuse existing tags rather than near-synonyms.
- **Salience is retention.** `high` — forever, for load-bearing decisions and constraints; `medium` (default) — a year; `low` — 90 days, for ambient context. Do not mark everything `high`.
- **Reversals cite what they reverse.** "Replaces journal#42: …".

Not for the journal: narration of routine steps, restatements of the task, secrets or credentials of any kind, and anything already obvious from the code or the commit message.
