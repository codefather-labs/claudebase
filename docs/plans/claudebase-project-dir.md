# Concept: `.claudebase/` per-project directory

**Owner:** Mira (orchestrator)
**Status:** concept — no implementation today
**Created:** 2026-05-24

**DEPENDS ON (must land first for the auto-register-to-server behavior):**
- [`claudebase-server-foundation.md`](./claudebase-server-foundation.md)
  — `identity.local` mirrors a server-side agent_registry record;
  without the foundation server running and authenticated, registration
  has nothing to talk to. The `.claudebase/` dir CAN exist without a
  server (project-scoped inbox / logs still useful), but the `registered`
  section of `identity.local` requires the foundation + agent-registry
  Phase 2.
- [`agent-registry-multi-cli.md`](./agent-registry-multi-cli.md) Phase 2
  — auto-registration writes the `agent_id` into `identity.local`.
  Without that phase, only `session_id` + local metadata are tracked.

**Related:**
- [`agent-registry-multi-cli.md`](./agent-registry-multi-cli.md) — uses the per-project identity established here to register cli's in the server's agent registry.

## Goal

When the operator runs `claudebase run` inside a project directory, claudebase creates a project-scoped `.claudebase/` dir at the cwd root. This dir holds **everything claudebase needs to remember about this project across cli sessions and across multiple cli's** that work on the same project:

- The cli's identity for this project (name, role, session-id ↔ server-registered-id mapping)
- Project-level config (default agent role, default TG routing, server URL override)
- Channel state scoped per-project (inbox files, active thread pointer)
- Local cache of project's knowledge / insights (synced from server)
- Audit log of what this cli did in this project

Analogous to `.git/` for git, `node_modules/` for npm, `.terraform/` for terraform — a tool's well-known marker dir at the project root.

## Motivation

### Current pain (before .claudebase/)

1. **Identity is ad-hoc.** Every cli session registers in the server's agent registry, but the linkage "this cli, in this project, has name=X role=Y" exists only in the operator's head. If the cli restarts, the name might change. If two cli's run in the same project simultaneously, they collide on the name "planner" with no distinction.

2. **TG inbox files leak across projects.** All projects share `~/.claude/channels/telegram/inbox/` — a photo sent to the orchestrator while working on project A is indistinguishable from one sent while working on project B. The cli routing them has to guess.

3. **State and identifiers live in operator memory.** "Which cli was the architect for project X?" — operator has to recall. "What thread was open with the planner when I closed the laptop yesterday?" — gone.

4. **No project-level config seam.** Want a specific Mira always to play the "rust-specialist" role in project X? Today you have to remember to start her that way. There's no per-project default.

5. **Cross-cli coordination requires global state OR runtime queries.** Two cli's in the same project can't easily share "yeah we both know this project, here's the planner's thread", because nothing in their working dir says "I'm in project X". They only see `cwd`, which the operator could've cd'd into.

### What `.claudebase/` solves

- **Stable per-project identity.** The dir's existence IS the project marker. The cli reads `.claudebase/config.toml` and knows: "this is project `claudebase`, default role for any cli here is `developer`, my session needs to be registered as `developer-aleksandra-{ts}`."
- **Project-scoped channel state.** TG inbox/, active-thread pointer, message-id ↔ cli mapping cache all live per-project, no cross-project contamination.
- **Identifier mapping persisted.** The cli's local session-id maps to its server-registered agent name — both stored here. Operator can `cat .claudebase/identity.local` to see "I'm registered as planner-projectX-7f3a".
- **Config-as-code.** `config.toml` can be committed (or not — operator's call); other files (`.local` suffix, `state/`, `inbox/`, `logs/`, cached `knowledge/`) are auto-gitignored.
- **Adjacent to `.claude/` not on top of it.** `.claude/` is owned by Claude Code (CC's own config + SDLC pipeline rules); `.claudebase/` is owned by claudebase. They coexist cleanly.

## Directory layout

```
<project-root>/
├── .claude/                       ← owned by Claude Code + SDLC (existing)
│   ├── CLAUDE.md
│   ├── settings.json
│   ├── scratchpad.md
│   └── rules/
│
└── .claudebase/                   ← owned by claudebase (NEW)
    ├── config.toml                ← committable: project identity + defaults
    ├── identity.local             ← local-only (gitignored): this cli's id mapping
    ├── state/                     ← local-only: per-session ephemera
    │   ├── active_thread.json     ← which agent this cli last talked to (TG)
    │   ├── message_id_map.json    ← TG message_id ↔ outbound-cli mapping
    │   └── last_seen.json         ← last server sync timestamp
    ├── inbox/                     ← local-only: per-project TG attachments
    │   ├── 1779494400-<id>.jpg
    │   └── ...
    ├── knowledge/                 ← local-only: cache of project's insights/books
    │   ├── insights.cache.db
    │   └── index.cache.db
    ├── logs/                      ← local-only: per-project log files
    │   ├── telegram-rs.log        ← currently /tmp/telegram-rs.log (global)
    │   └── server-sync.log
    └── .gitignore                 ← auto-generated, gitignores .local + state/ + inbox/ + knowledge/ + logs/
```

### `config.toml` example (committable)

```toml
# .claudebase/config.toml
schema_version = 1

[project]
slug = "claudebase"
name = "claudebase"
description = "Local hybrid retrieval CLI for LLM agents"
homepage = "https://github.com/codefather-labs/claudebase"

[agent]
# Default identity for any cli started in this project via `claudebase run`.
# Operator can override via flags (`claudebase run --name X --role Y`).
default_role = "developer"
# Templated; resolved at first-run: {role}-{user}-{short-host}
default_name_pattern = "{role}-{user}"

[claudebase]
# Server endpoint. Overrides ~/.claude/settings.json claudebase.serverUrl
# if both set (project config wins for this project's cli's).
server_url = "https://localhost:8443"

[telegram]
# Optional per-project TG routing config. When set, server uses these
# defaults when this project's cli's emit/route via Telegram.
default_chat_id = "434566766"     # which TG chat does this project belong to?
prefix = "[claudebase] "          # prepended to outbound messages from this project
ack_reaction = "👀"               # auto-react to inbound from this project's cli's

[hooks]
# Per-project hooks the cli should fire on top of global ~/.claude/hooks/.
# Same wire format as ~/.claude/hooks/ but project-scoped.
post_turn = ".claudebase/hooks/post-turn.sh"
```

### `identity.local` example (gitignored)

```toml
# .claudebase/identity.local
# Auto-managed by claudebase. DO NOT COMMIT.
schema_version = 1

[session]
# CC session id of THIS cli process.
session_id = "a155424e-2ee2-4cd9-9be2-32395c440f0e"
pid = 79859
started_at = "2026-05-24T10:12:53Z"
launcher = "claudebase run"          # how this cli was started

[registered]
# Server-side identity (populated after auto-register succeeds).
server_url = "https://localhost:8443"
agent_id = "01HQXYZ..."              # server-assigned UUID
agent_name = "developer-aleksandra"
agent_role = "developer"
registered_at = "2026-05-24T10:12:55Z"
auth_token_env = "CLAUDEBASE_AUTH_TOKEN"  # which env var holds the token
```

### `.gitignore` (auto-generated on first `claudebase run`)

```gitignore
# Auto-generated by claudebase. Edit if you want to commit logs or inbox.
identity.local
state/
inbox/
knowledge/
logs/
*.local
*.cache.db
```

## Lifecycle

### 1. First-time bootstrap

Operator runs `claudebase run` inside a project for the first time:

1. `claudebase run` detects: no `.claudebase/` here yet.
2. Prompts (or runs non-interactively with sensible defaults):
   - "Initialize .claudebase/ for project `<basename(cwd)>`? [Y/n]"
   - "Role for cli's in this project? [developer]"
   - "Server URL? [from ~/.claude/settings.json or empty]"
3. Creates `.claudebase/{config.toml, .gitignore, state/, inbox/, knowledge/, logs/}`
4. Writes `identity.local` for THIS cli session (session_id + pid + started_at)
5. If `[claudebase].server_url` is set: registers this cli with server, stores returned `agent_id` back into `identity.local`
6. Then exec's the actual `claude` command (or `claudebase run` proceeds with the telegram-plugin preset)

### 2. Subsequent runs

Operator runs `claudebase run` again (same project, new terminal):

1. `.claudebase/config.toml` exists → load defaults from it.
2. Generate fresh `identity.local` (or append to existing — open question, see below).
3. Register with server using config.toml's `agent.default_role` + resolved `default_name_pattern`.
4. Exec `claude` with relevant env wired up.

### 3. Cross-cli coordination in the same project

Two cli's running in the same project's working dir read the SAME `.claudebase/config.toml`. They register with the server under predictably-similar names (e.g. `developer-aleksandra-1` and `developer-aleksandra-2`). The server links them as "same project" via the project_slug from config.toml. The orchestrator can query "alive cli's working on project X" and gets both back.

### 4. Cli shutdown

On EOF / SIGTERM:
- `claudebase server unregister <agent_id>` called by plugin shutdown path.
- `identity.local` either removed OR moved to `state/last-session.json` (for resume / forensics — open question).
- Inbox + logs preserved (operator decides retention).

## Interaction matrix with existing concepts

| Concept | Where it lives before | After `.claudebase/` |
|---|---|---|
| TG bot token | `~/.claude/channels/telegram/.env` (global) | unchanged — token is operator-level credential, not project-scoped |
| TG inbox files | `~/.claude/channels/telegram/inbox/` (global, all projects mixed) | per-project `<project>/.claudebase/inbox/` (no cross-contamination) |
| TG access.json | `~/.claude/channels/telegram/access.json` (global allowlist) | unchanged — allowlist is operator-level |
| TG plugin stderr log | `/tmp/telegram-rs.log` (single file, all projects merged) | per-project `<project>/.claudebase/logs/telegram-rs.log` |
| Agent registry record | server-side DB row (transient, exists while cli alive) | unchanged — server is authoritative; `identity.local` mirrors the registration for the cli's own reference |
| CC scratchpad (per-feature in-progress state) | `<project>/.claude/scratchpad.md` (existing) | unchanged — owned by SDLC pipeline, not claudebase |
| Project knowledge base | `<project>/.claude/knowledge/{index,insights}.db` (per-project file DB) | maybe → `<project>/.claudebase/knowledge/*.cache.db` as a cache of server-authoritative data (per `agent-registry-multi-cli.md` Phase 7). Or stay where it is. Open question. |
| `claudebase agent register` invocation | none today | auto-fires on `claudebase run` startup using `.claudebase/config.toml` to resolve role + name |

## Use cases

### 1. Operator clones a new project the team uses with claudebase

```
git clone github.com/team/foo
cd foo
ls .claudebase/      # config.toml committed by the team
claudebase run       # cli reads config.toml, registers as developer-aleksandra,
                     # joins the project's existing agent registry slot
```

The team committed `config.toml` so every developer's cli registers with consistent role + naming.

### 2. Operator working solo on multiple projects in parallel

Three terminals, three projects, three running cli's. Each cli has its OWN `.claudebase/` per project. TG inbox files don't cross-contaminate. Server registers all three with distinct names (project-A-dev, project-B-dev, project-C-dev). Operator says in TG: "ask project-A-dev what's the current branch" — server routes to that exact cli.

### 3. Project switches from solo → team → swarm

- Solo: one `claudebase run`, one `.claudebase/identity.local`
- Team: 4 developers each `claudebase run`, 4 cli's registered with same role + different name suffixes, server lists all 4 as alive
- Swarm: orchestrator + planner + architect + reviewer running concurrently in same project, each `.claudebase/identity.local` carries its role; orchestrator delegates via server

Same `config.toml` works for all three modes — only `identity.local` (per-cli) and the server-registered names change.

## Open questions (to settle before implementation)

1. **Multi-cli identity in same `.claudebase/`.** When N cli's run in the same project's cwd, do they all share ONE `identity.local` (append model) or each has its own keyed by session_id? Leaning per-cli (each session_id gets own file `state/cli-<session_id>.local`); the top-level `identity.local` becomes a symlink to "the most recently registered" for ergonomic `cat`-ing.
2. **`config.toml` schema versioning.** Already wired `schema_version = 1`; do we accept silent migration on bump, or fail-loud and require operator's `claudebase migrate-config`? Leaning fail-loud — config is operator-curated, silent migration is scary.
3. **Templated `default_name_pattern` tokens.** `{role}`, `{user}`, `{short-host}` listed in example. Need stable list: also `{cwd-slug}`, `{ts-short}`, `{pid}`, `{project-slug}`? Leaning minimal core (`{role}`, `{user}`, `{project-slug}`) + operator can override with literal string.
4. **Knowledge / insights cache here vs server-only.** Phase 7 of `agent-registry-multi-cli.md` makes server authoritative. Does the project ALSO keep a local cache in `.claudebase/knowledge/` for offline + speed? Or trust server roundtrip always? Leaning: local cache as transparent read-through, dirty-bit invalidation on server-side change events.
5. **`init` UX vs `run` UX.** Should bootstrap be `claudebase init` (explicit) and `run` only ever exec's? Or `claudebase run` auto-inits on first call if missing? Leaning **auto-init with `[Y/n]` prompt** + explicit `claudebase init` available for ahead-of-time setup (CI / team onboarding scripts).
6. **`.claudebase/` discovery walk.** Like git's `.git/` discovery walks up from cwd to find the repo root. Should `claudebase run` walk up to find an ancestor `.claudebase/`? Leaning yes — matches git ergonomic; operator cd's into a subdir but still gets project config.
7. **Old global state migration.** What about `~/.claude/channels/telegram/inbox/` files from before? Migrate them into projects? Most can't be attributed retroactively. Leaning: leave global path for legacy; new files go per-project; document migration as one-time operator chore.
8. **Backward compat with single-cli mode (no server registered).** A cli started WITHOUT `[claudebase].server_url` set should still work — just doesn't register, doesn't auto-route, plays solo. `.claudebase/` is still helpful (project-scoped inbox/logs) even without server. Leaning yes — make server registration strictly opt-in.
9. **Coexistence with `.claude/` rules.** Does claudebase look at `.claude/rules/` for anything, or only its own `.claudebase/hooks/`? Probably stay decoupled (claudebase ignores `.claude/`, CC ignores `.claudebase/`); SDLC pipeline rules unchanged. Leaning: decoupled.

## Risks + mitigations

| Risk | Mitigation |
|---|---|
| `.claudebase/` accidentally committed (operator includes `identity.local`) | Auto-generate `.gitignore` on init; document; pre-commit check via `claudebase doctor` (future) |
| Stale `state/active_thread.json` from prior session confuses fresh cli | Add age check on read; if stale > 24h, reset; don't trust blindly |
| `config.toml` schema drift between team members → registration name collisions | Schema version + fail-loud (open question 2 above) |
| Project rename → old `agent_name` in registry doesn't match new config | `claudebase doctor` detects + offers `--repair-project-slug` |
| Per-project knowledge cache disk usage | Cache size cap (e.g. 100 MB); LRU eviction; operator can `claudebase project knowledge clear` |
| Cli registers in registry with `cwd` that has moved (operator renamed project dir) | Server's heartbeat re-confirms `cwd`; stale entries reaped per existing `reap` logic |

## Effort estimate (concept-level, no commitment)

| Slice | Estimate |
|---|---|
| Init / config schema / `.gitignore` auto-gen | 1 day |
| Auto-init on `claudebase run` + identity.local generation | 1 day |
| Per-project inbox / logs redirect from global path | 0.5 day |
| Project-slug propagation into server agent_registry | 0.5 day |
| Discovery walk-up (find ancestor `.claudebase/`) | 0.5 day |
| `claudebase init` explicit subcommand + `claudebase doctor` health check | 1-2 days |
| Knowledge cache integration (depends on Phase 7 of multi-cli plan) | 2-3 days |
| Tests + docs | 1-2 days |

**Total: ~8-10 dev days**, mostly independent of the multi-cli plan but Phase 7 (knowledge cache) is shared work.

## Not in v1 (deferred)

- Per-project TG bot tokens (still single token at operator level)
- Cross-project secret store (e.g. shared API keys per project) — out of scope
- Project templates (`claudebase init --template rust-lib`) — future
- `.claudebase/` discovery from non-cli tools (e.g. shell hook to auto-load project context) — out of scope

## Facts

### Verified facts

- The current `telegram-plugin-rs` writes its inbox to `~/.claude/channels/telegram/inbox/` regardless of which project the cli is operating in — verified by reading `claudebase/plugins/telegram-rs/src/state.rs::state_dir` this session (returns `$HOME/.claude/channels/telegram` unless `TELEGRAM_STATE_DIR` env var overrides). Salience: high (this is one of the named pain points motivating `.claudebase/`).
- `claudebase agent register` / `unregister` / `list_alive` / `reap` already exist in `claudebase/src/daemon/agent_registry.rs` — verified by direct grep this session. The per-project `.claudebase/identity.local` will thinly wrap those existing APIs with `--project-slug` + `--role` + `--name` args. Salience: high.
- `.claude/` (the CC + SDLC config dir) is well-established and lives at project root — verified by every project this session having one. `.claudebase/` is intentionally adjacent, not nested, to keep ownership boundaries clean. Salience: medium.

### External contracts

- TOML 1.0 spec — symbol: standard `[section]` + `key = value` syntax — source: toml.io; already in claudebase deps (`toml` crate v0.8 in Cargo.toml line 113 area, verified in this session). Salience: medium.
- `gitignore` glob syntax — symbol: standard pattern matching — source: git docs. Salience: low.
- Cargo `agent_registry` schema (Phase 2 of agent-registry-multi-cli.md): the new fields `name`, `role`, `description`, `capabilities`, `cwd`, `host`, `pid`, `started_at`, `last_seen_at` are what `.claudebase/identity.local` will mirror. Salience: high (couples this plan to the parent multi-cli plan).

### Assumptions

- Operators are okay with a NEW dotdir at project root alongside `.claude/`. Some operators dislike dotdir proliferation; an alternative is nesting under `.claude/claudebase/` but that conflates ownership. Leaning: separate dir is worth the dotdir cost. How to verify: solicit operator feedback before implementation. Salience: medium.
- Schema versioning at `schema_version = 1` is a sufficient migration handle for v1. May need richer migration story later (e.g. claudebase version bumps schema, old projects on disk auto-flag for migration). Defer. Salience: low.
- The TG plugin will be refactored per `agent-registry-multi-cli.md` Phase 3.5 (server-mediated TG routing). Per-project inbox dir only makes sense if individual cli's still receive their own messages routed by server — which is the planned architecture. Salience: medium.

### Open questions

(See `## Open questions` section above — 9 items deferred to implementation kickoff.)

## Decisions

### Inbound validation

- Operator brief: "пускай в текущей директории создает папку .claudebase в которой будут сохраняться все конфиги per project в том числе возможно мепинг идентификаторов или типа того. создай отдельный файл план. сегодня делать не будем просто концепт". Coherent ask, no contradictions with prior context (multi-cli fleet plan establishes the registered-cli-identity primitive that `.claudebase/identity.local` mirrors). Outcome: write concept-only doc, defer all implementation slicing to a follow-up plan when operator green-lights. Salience: high.

### Decisions made

- **Decision:** Separate top-level `.claudebase/` dir, NOT nested under `.claude/`. Alternatives considered: (a) `.claude/claudebase/` subdir — rejected, conflates SDLC + claudebase ownership; (b) extend `.claude/settings.json` with a `claudebase` section — rejected, single big file mixes per-tool concerns. Q1-Q5: not a hack ✓ / proportionate (dotdir vs single subkey) ✓ / alternatives evaluated ✓ / addresses root cause (per-project identity needs dedicated home) ✓ / n/a. Salience: high.
- **Decision:** Two file classes — `config.toml` (committable, project identity) and `identity.local` + `state/` + `inbox/` + `knowledge/` + `logs/` (gitignored, per-cli/per-session). Mirrors how `.git/config` vs `.git/HEAD` split. Auto-generated `.gitignore` enforces the split. Salience: high.
- **Decision:** Auto-init on `claudebase run` with `[Y/n]` prompt; explicit `claudebase init` also exists for CI / non-interactive use. Operators on different paths get the same result. Salience: medium.
- **Decision:** Project-level `config.toml` overrides global `~/.claude/settings.json` claudebase keys for THIS project's cli's. Standard config-locality principle (most-specific wins). Salience: medium.
- **Decision:** Knowledge / insights replication strategy is OWNED by `agent-registry-multi-cli.md` Phase 7 — `.claudebase/knowledge/` is just the local-cache slot in this dir, not a new replication architecture. Don't duplicate the design here. Salience: medium.

### Hacks acknowledged

(none) — this is a concept doc, no implementation hacks.

### Symptom-only patches

(none) — concept addresses root cause (no per-project identity / scoping today) rather than working around it.
