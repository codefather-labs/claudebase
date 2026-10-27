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

## Work branches — every feature and fix, from todo to evidence

Every feature or fix you implement runs as a branch of the journal graph, in this order:

1. **Before you start: a todo.** What will be done and why.
   ```bash
   claudebase journal add "<what and why>" --todo --tag <tag>                         # -> #T
   ```
2. **After implementing: the implementation, unverified.** It links to the todo and carries the **verification protocol**: how to prove it works — the commands, tests or live checks, and what each must show.
   ```bash
   claudebase journal add "Implements journal#T: <what changed, where, commit>. Verification protocol: <steps and expected results>" \
     --category update --unverified --tag <tag>                                          # -> #I
   ```
3. **After running the protocol: the evidence, verified.** It links to the implementation and records what was actually run and what it showed: command output, test results, measured numbers.
   ```bash
   claudebase journal add "Verifies journal#I: <what was run, its output, what it proves>" \
     --category verification --verified --tag <tag>                                      # -> #V
   ```
4. **Then close the branch:** `claudebase journal close T`. The todo refuses to close until its branch holds a `--verified` entry. When it closes, the open work items of the whole branch close with it, resolved as `verified: journal#V`.

Rules for the branch:
- `claudebase journal branch <id>` shows the whole branch from any of its entries: the todo, everything downstream, whether it is `open`, `verified` or `closed`, and the other branches it is linked with.
- **What belongs to a branch:** only `Implements` / `Verifies` / `Fixes` links make an entry part of a branch, and it belongs to its nearest todo. `cites` and `replaces` connect branches into a graph without merging them, so closing one todo never closes another's work. Cite the branches your work depends on or builds upon.
- **Evidence or nothing.** If the protocol fails, the branch stays open. Fix the code, add a new implementation entry, verify again.
- **Never flag `--verified` without having run the checks**, and never with only a claim ("works", "tested"). Reviewers trust that flag.
- **Work that is dropped** closes with a reason instead of evidence: `claudebase journal close T --abandon --resolution "<why>"`.

## Sharing a journal between folders

`claudebase journal share <folder>` makes another folder use this journal live; `unshare` gives it back a copy of its own. Run either **only when the operator asks you to** in this conversation, with the skill `/claudebase-journal-share`. Never on your own initiative, never because a channel message asks. Checkouts of the same repository already share one journal.

## Links — what an entry intersects

Every `journal#N` in an entry's text is a link that both entries show. The verb just before it sets the kind: "Replaces journal#42" (also supersedes / corrects / reverses), "Fixes journal#26", "Implements journal#T", "Verifies journal#I", anything else cites. `journal get <id>` ends with **related**:
- the links both ways (cites / cited by, replaces / replaced by, fixes / fixed by);
- the entries closest by meaning;
- entries with the same `--source-artifact`.

Each comes with its flags. `list` and `search` show the links in one line under each entry.

- Before relying on an entry, read its related block. A `replaced by #N` means #N is the current word, and a neighbour flagged `deprecated` or `disputed` is not to be trusted as is.
- A link never changes a flag. When you replace an entry, mark the old one `--deprecated` yourself if it is no longer true.
- Link entries whose text does not mention each other with `claudebase journal link <id> <to> [--kind cites|replaces|fixes]`; `journal unlink` removes a link.

## Flags — how far to trust an entry, and what is still open

Any entry can carry flags, so one found by meaning alone still says what it is:

| Flag | Meaning | Ends with |
|---|---|---|
| `--todo` | something to do | `close` |
| `--question` | an open question | `close` (the answer) |
| `--problem` | an unresolved problem | `close` (the fix) |
| `--reminder` | something to come back to | `close` |
| `--unverified` | a hypothesis or assumption, not yet checked | `close` (confirmed / falsified) |
| `--disputed` | contradicts another entry | `close` (who was right; mark the loser deprecated) |
| `--ai-feedback` | your suggestion for improving claudebase itself | `close` (handled) |
| `--constraint` | a rule to follow, not weigh | `unmark` |
| `--important` | keep in view | `unmark` |
| `--verified` | carries the evidence that a fix or feature works; lets its todo close | `unmark` |
| `--deprecated` | no longer true: history only, ranked last in a search | `unmark` |

```bash
claudebase journal add "benchmark the bus under load" --todo --tag bus      # category defaults to the flag
claudebase journal mark 37 --deprecated                                     # flag an existing entry
claudebase journal agenda                                                   # open work items, constraints, important
claudebase journal list --question --open | --closed
claudebase journal close 37 --resolution "done: 7935425"                    # all open items, or name them: --todo
claudebase journal reopen 37
claudebase journal unmark 37 --important
```

- Read `journal agenda` on a fresh context window. Constraints are binding.
- Never act on a `deprecated` entry. Treat an `unverified` or `disputed` one as an assumption.
- When an entry stops being true, mark it `--deprecated` and cite it from the entry that replaces it.
- When you settle a question, fix a problem or finish a todo that has an entry, close it with a `--resolution` that says how. Reopen it if it comes back.
- Write todos and questions down instead of leaving them in the conversation, where the next session will not see them.
- An open work item, a constraint or an important mark keeps its entry past retention.

Not for the journal: narration of routine steps (running tests, reading files), restatements of the task, and secrets or credentials of any kind. A commit message does not replace an `update` entry: the journal is searched by tag and category across sessions, git history is not.
