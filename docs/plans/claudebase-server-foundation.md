# Plan: claudebase server foundation — HTTP/WSS + token auth + service install

**Owner:** Mira (orchestrator)
**Status:** draft — awaiting operator review
**Created:** 2026-05-24

**Blocks (downstream plans depend on this):**
- [`agent-registry-multi-cli.md`](./agent-registry-multi-cli.md) — registry + message bus + spawn + monitoring rides on top of this server
- [`telegram-multi-cli-orchestration.md`](./telegram-multi-cli-orchestration.md) — server-side TG poller + routing layer is a transport on this server
- [`claudebase-project-dir.md`](./claudebase-project-dir.md) — per-project `.claudebase/identity.local` mirrors records the server holds

This plan delivers ONLY the server foundation: lifecycle, HTTP/WSS surface, Bearer-token auth, OS service registration, TLS, health endpoints, audit logging. **No registry CRUD, no channel routing, no TG poller** — those are downstream plans.

## Goal

Bootstrap the **single load-bearing service** that everything else in the multi-cli / multi-transport orchestration depends on:

> A long-running `claudebase server --serve` process (foreground for dev,
> installable as OS service for production) that listens on a configurable
> port over HTTP or HTTPS, enforces Bearer-token auth on every request,
> preserves the existing UDS path for local plugin bridges, and exposes a
> stable `/health` + `/version` API surface that downstream plans
> (agent registry, channel bus, TG transport) extend with their own
> endpoints.

The two operator-decided constraints (D1 + D2 from
`agent-registry-multi-cli.md`) are restated here as **load-bearing
foundations**, not negotiable phases:

- **D1 — Auth is MANDATORY.** No `--no-auth` flag, no permissive default,
  no "trust localhost" shortcut. Every HTTP/WSS request (and every UDS
  request once the token-auth path is added there) requires `Authorization:
  Bearer <token>`. A request without the header is rejected with 401
  BEFORE any handler runs.
- **D2 — Installable OS service.** `claudebase server install` registers
  the process with launchd (macOS), systemd (linux), or Windows SCM via
  the existing `windows-service` crate in claudebase deps. Foreground mode
  (`claudebase server --serve --foreground`) preserved for dev/debug.

## Architecture (shared by all downstream plans)

```
                  ┌──────────────────────────────────────┐
                  │  claudebase server                   │
                  │  (claudebase server --serve)         │
                  │                                       │
                  │  ┌──────────────────┐                │
                  │  │ HTTP/WSS         │ ← Phase 1      │
                  │  │ listener         │   (this plan)  │
                  │  │ + UDS (existing) │                │
                  │  └──────────────────┘                │
                  │  ┌──────────────────┐                │
                  │  │ auth middleware  │ ← Phase 2      │
                  │  │ Bearer token     │   (this plan)  │
                  │  │ MANDATORY        │                │
                  │  └──────────────────┘                │
                  │  ┌──────────────────┐                │
                  │  │ /health /version │ ← Phase 5      │
                  │  │ /livez (no-auth) │   (this plan)  │
                  │  └──────────────────┘                │
                  │  ┌──────────────────┐                │
                  │  │ structured logs  │ ← Phase 6      │
                  │  │ + audit trail    │   (this plan)  │
                  │  └──────────────────┘                │
                  │  ─── extensions live downstream ──── │
                  │  ┌──────────────────┐                │
                  │  │ agent_registry   │ ← extends in   │
                  │  │ (canonical state)│   multi-cli    │
                  │  └──────────────────┘   plan Phase 2 │
                  │  ┌──────────────────┐                │
                  │  │ channel router   │ ← extends in   │
                  │  │ (the bus)        │   multi-cli    │
                  │  └──────────────────┘   plan Phase 3 │
                  │  ┌──────────────────┐                │
                  │  │ TG bridge        │ ┐ extends in   │
                  │  │ (transport       │ │ TG-orch plan │
                  │  │  adapter)        │ │              │
                  │  └──────────────────┘ │ ANY future   │
                  │  ┌──────────────────┐ │ transport:   │
                  │  │ cli-to-cli bridge│ │ Discord,     │
                  │  │ (same bus,       │ │ Slack,       │
                  │  │  no external)    │ │ Matrix,      │
                  │  └──────────────────┘ ┘ webhooks     │
                  └─────────────┬────────────────────────┘
                                │  channel callbacks
                                ▼  (after downstream plans)
   ┌──────────┐  ┌──────────┐  ┌──────────┐  ┌──────────┐
   │  cli A   │  │  cli B   │  │  cli C   │  │  cli D   │
   │ subscribes │ subscribes │ subscribes │ subscribes │
   └──────────┘  └──────────┘  └──────────┘  └──────────┘
```

**This plan delivers the top three boxes** (HTTP/WSS listener, auth
middleware, health endpoints) plus logging, plus the service install
machinery. Everything below — agent registry, channel router, TG bridge,
cli-to-cli bridge — is downstream plans extending the foundation.

## Why a separate plan

- **Atomic deliverable.** Server boots, accepts auth'd requests, returns
  health. That's a shippable artifact even if NO downstream feature is
  built on top yet — useful for operator verification ("is the server
  alive? does my token work?").
- **Critical-path blocker.** Without this, neither cli-to-cli nor TG
  orchestration can start. Sequencing it as its own plan makes the
  dependency explicit.
- **Auth + install have non-trivial scope.** Token lifecycle, TLS
  certificate handling, service registration across three OSes —
  collectively ~2 weeks of work that deserves focused attention without
  being conflated with the routing + transport features that consume it.
- **Foundation principles propagate.** D1 (auth mandatory) and D2
  (installable service) get LOCKED here. Downstream plans inherit, don't
  re-debate.

## What is in scope

| Component | Detail |
|---|---|
| Lifecycle command | `claudebase server --serve` (foreground); `claudebase server {install,uninstall,start,stop,restart,status,logs}` (service mgmt — extends existing `claudebase daemon *` surface) |
| Transport | HTTP/1.1 + WebSocket on configurable port; UDS path preserved unchanged for local plugin bridges (e.g. existing telegram-plugin-rs in pre-refactor form) |
| TLS | rustls (already in deps via reqwest); `--use-ssl` flag generates self-signed cert on first run; `--use-ssl --cert PATH --key PATH` for BYO-CA |
| Auth | Bearer-token MANDATORY on every HTTP/WSS request; 401 BEFORE handler. UDS path also enforces token (uniform) — no localhost shortcut |
| Token lifecycle | Auto-generate on first `claudebase server install` (32-byte URL-safe random); print once to stdout; `claudebase server rotate-token` rotates with operator confirm + active connection grace period |
| Token storage (operator-side) | Primary: env var `CLAUDEBASE_AUTH_TOKEN`; fallback: `~/.claude/settings.json::claudebase.authToken` (chmod 0600 enforced) |
| Service registration | macOS: launchd user agent at `~/Library/LaunchAgents/dev.codefather.claudebase.plist`; Linux: systemd user unit at `~/.config/systemd/user/claudebase.service`; Windows: SCM via `windows-service` crate (already in claudebase deps) |
| Endpoints | `GET /health` (auth required) → `{status, version, uptime_ms, transports: [http, uds]}`; `GET /version` (auth required) → `{version, git_sha, build_ts}`; `GET /livez` (NO auth) → `200 OK` literal for orchestrator readiness probes |
| Logging | Structured JSON to file `~/.claude/logs/claudebase-server.log`; daily rotation (keep 7 days); existing `tracing-subscriber` deps |
| Audit trail | Every authenticated request logged with `{ts, method, path, token_id (sha256 prefix only, never full token), remote_addr, status, latency_ms}` — separate file `~/.claude/logs/claudebase-server-audit.log` |
| Rate limiting | Simple per-token-id sliding window (default: 100 req/min); 429 with `Retry-After` header |
| CORS | Not enabled (server-to-server only, no browsers expected) |

## What is NOT in scope (deferred to downstream plans)

| Concern | Where it lives |
|---|---|
| `POST /agents/register`, `GET /agents/list_alive`, etc | [`agent-registry-multi-cli.md`](./agent-registry-multi-cli.md) Phase 2 |
| `POST /channels/publish`, channel subscribe via WebSocket | [`agent-registry-multi-cli.md`](./agent-registry-multi-cli.md) Phase 3 |
| `mcp__claudebase__agent_*` tool definitions | [`agent-registry-multi-cli.md`](./agent-registry-multi-cli.md) Phases 3-5 |
| TG poller, /agents bot command, tg_message_map | [`telegram-multi-cli-orchestration.md`](./telegram-multi-cli-orchestration.md) all phases |
| `.claudebase/identity.local` mirror | [`claudebase-project-dir.md`](./claudebase-project-dir.md) |
| Knowledge / insights replication endpoints | [`agent-registry-multi-cli.md`](./agent-registry-multi-cli.md) Phase 7 |

## Phases

### Phase 1 — HTTP/WSS listener over existing daemon

Extend the existing `claudebase daemon serve` (UDS) to ALSO accept TCP / TLS connections.

- New flag: `--listen-http <port>` (default 8443 if `--use-ssl`, 8080 otherwise; `0` disables HTTP listener and keeps UDS-only)
- New flag: `--listen-uds <path>` (defaults preserved for backward compat with existing plugin bridges)
- New flag: `--foreground` — run in current terminal instead of detaching
- New flag: `--data-dir <dir>` — override default `~/.claudebase/server/`
- WebSocket upgrade on HTTP (Sec-WebSocket-Protocol negotiation) for channel subscribe (full subscribe logic in downstream)
- Graceful shutdown on SIGTERM (close listeners; drain in-flight; existing UDS draining preserved)
- HTTP framework: `hyper` (lightweight, already transitively pulled in via reqwest)

**Done when:**
- `claudebase server --serve --listen-http 8443 --use-ssl --foreground` runs in terminal, accepts HTTPS connections
- `curl -k https://localhost:8443/livez` → `200 OK` (no auth on this endpoint — readiness probe)
- UDS path still works for existing telegram-plugin-rs bridge (smoke-tested)

### Phase 2 — Auth middleware (MANDATORY)

Every HTTP/WSS request (and every UDS request once Phase 2 lands fully) goes through auth check BEFORE handler runs.

- Middleware reads `Authorization: Bearer <token>` header
- Compares constant-time against the server's loaded token
- Missing header → 401 with body `{"error": "auth required"}`
- Token mismatch → 401 with body `{"error": "invalid token"}` (no token echo in error)
- Token match → request proceeds to handler
- The `/livez` endpoint is the ONLY exception (no-auth, for OS / orchestrator readiness probes that can't carry credentials)

**Done when:**
- `curl https://localhost:8443/health` → 401 (no auth)
- `curl -H "Authorization: Bearer wrong-token" https://localhost:8443/health` → 401
- `curl -H "Authorization: Bearer <correct>" https://localhost:8443/health` → 200

### Phase 3 — Token lifecycle

Operator never manually types a token at install. Server generates one for them.

- `claudebase server install` includes:
  - Generate 32-byte URL-safe random token
  - Write to `~/.claudebase/server/token` (chmod 0600)
  - Print token to stdout ONCE with explicit "stash this; it won't print again" warning
  - Service unit env file references it (or runs `claudebase server --token-file ~/.claudebase/server/token`)
- `claudebase server rotate-token`:
  - Prompts operator confirm: "This will invalidate the current token; all cli's MUST be re-configured."
  - Generates new token, writes to `token`, prints new token to stdout
  - Old token honored for 5-min grace period (in-flight requests survive)
- Token resolution at runtime (precedence):
  1. `--token-file <path>` CLI arg (highest precedence — overrides everything; used by service unit)
  2. `CLAUDEBASE_AUTH_TOKEN` env var (operator-shell-typed)
  3. `~/.claudebase/server/token` file (default location)
- Token format: `cb_<8-char-id>_<24-byte-base64>` so audit log can record just the 8-char id portion (auditable) without leaking the secret

**Done when:**
- Fresh `claudebase server install` prints `cb_a1b2c3d4_<24-chars>` once and writes file with chmod 0600
- `claudebase server rotate-token` works with 5-min grace
- Token format parsed correctly; audit log uses ID prefix only

### Phase 4 — TLS (rustls)

- `--use-ssl` flag enables TLS
- `--cert <path> --key <path>` for BYO certificate (production)
- Without `--cert/--key`: auto-generate self-signed cert on first `--use-ssl` run, stash at `~/.claudebase/server/{cert.pem, key.pem}` (chmod 0600 on key)
- `claudebase server regenerate-cert` overwrites the self-signed cert (no-op for BYO)
- Cert sane defaults: CN=`claudebase-server`, SAN includes `localhost`, `127.0.0.1`, and any hostnames found via `hostname` syscall
- Document: for production deploy, use BYO cert from a real CA (Let's Encrypt or internal). Self-signed is fine for single-machine + ssh-tunnel + dev.

**Done when:**
- `--use-ssl` with no cert flags → auto-generates self-signed, server starts, `curl -k https://...` works
- `--cert /path/to/server.crt --key /path/to/server.key` uses BYO
- `regenerate-cert` works on self-signed; refuses (with clear msg) on BYO

### Phase 5 — Cross-platform service install

Extend existing `claudebase daemon install/uninstall/start/stop/restart/status/logs` subcommand family to wire the HTTP/TLS listener.

- macOS: `launchctl load -w ~/Library/LaunchAgents/dev.codefather.claudebase.plist`
- Linux: `systemctl --user enable --now claudebase.service`
- Windows: SCM via `windows-service` crate (already in claudebase deps line 209 area, verified earlier this session):
  - `claudebase server install` registers the service with auto-start
  - `claudebase server uninstall` deletes the service
  - `claudebase server start/stop/restart` calls SCM control codes
  - Service appears in `services.msc` and survives reboot
- Status endpoint:
  - `claudebase server status` → calls health endpoint over UDS or HTTP; prints `{status, version, uptime, pid, listen_addrs}`
- Logs endpoint:
  - `claudebase server logs [--audit] [--tail N]` → tails `~/.claude/logs/claudebase-server{,-audit}.log`

**Done when:**
1. macOS: `claudebase server install` registers launchd unit; `claudebase server start` starts daemon; survives `launchctl unload` + reboot
2. Linux: ditto with systemd
3. Windows: appears in `services.msc`; survives reboot
4. Cross-platform: `claudebase server status` returns same JSON shape regardless of OS

### Phase 6 — Structured logging + audit trail

Two separate log streams:

- **General log** `~/.claude/logs/claudebase-server.log`:
  - JSON line per event via `tracing` + `tracing-subscriber` (both in deps)
  - Fields: `{ts, level, target, event, fields...}`
  - Daily rotation, keep 7 days (use `tracing-appender::rolling`)
  - Captures: lifecycle events, errors, warnings, info messages
- **Audit log** `~/.claude/logs/claudebase-server-audit.log`:
  - JSON line per authenticated request
  - Fields: `{ts, method, path, status, latency_ms, token_id, remote_addr, user_agent}`
  - `token_id` is the 8-char prefix from `cb_<id>_<secret>` — NEVER full token
  - Daily rotation, keep 30 days (longer than general for forensics)
- Both writable to stderr in `--foreground` mode (operator sees them live)
- Audit log includes 401s (failed auth attempts — important for security)

**Done when:**
- General log captures startup + every minute of uptime
- Audit log captures every authenticated request with `token_id` prefix only
- Rotation creates yesterday's file with date suffix, old files purged

### Phase 7 — Health/version endpoints + rate limit

Three endpoints; documentation:

| Endpoint | Auth | Body |
|---|---|---|
| `GET /livez` | NO | `200 OK` literal text — for OS readiness probes |
| `GET /health` | YES | JSON `{status: "ok", version: "0.6.0", uptime_ms: N, transports: ["http", "uds"], listen_addrs: ["https://0.0.0.0:8443", "/path/to/uds.sock"]}` |
| `GET /version` | YES | JSON `{version: "0.6.0", git_sha: "...", build_ts: "..."}` |

Rate limit:
- Sliding window per `token_id`: 100 req/min default
- 429 response with `Retry-After: <seconds>` header
- Configurable via `--rate-limit <reqs-per-min>` (0 disables)

**Done when:**
- All three endpoints return correct shapes
- 101 requests in 1 minute → request 101 gets 429
- `/livez` works without auth header

### Phase 8 — Docs + RELEASING.md update

- New section in `claudebase/README.md`: "Running as a server"
- Update `claudebase/docs/RELEASING.md`: server-related release notes template
- Migration doc: how to take an existing claudebase install (currently UDS-only daemon) and upgrade to HTTP/service mode
- Sample systemd unit / launchd plist / Windows service config files in `claudebase/deploy/`

**Done when:**
- README has runnable copy-paste examples for all 3 OSes
- Operator can follow docs from zero → service running with auth

## Acceptance per phase (compact)

| # | Phase | Done when |
|---|---|---|
| 1 | HTTP/WSS listener | `claudebase server --serve --foreground` accepts HTTPS; UDS still works |
| 2 | Auth middleware | 401 without/wrong token; 200 with correct token; `/livez` no-auth |
| 3 | Token lifecycle | install prints token once; rotate works with grace; format = `cb_<id>_<secret>` |
| 4 | TLS | auto-self-signed cert + BYO cert + regenerate work cleanly |
| 5 | Service install | launchd/systemd/SCM all work; status JSON identical shape |
| 6 | Logging + audit | rotated general + audit logs; `token_id` only in audit (never full) |
| 7 | Health + rate limit | 3 endpoints work; rate limit triggers 429 |
| 8 | Docs | runnable examples for all 3 OSes; migration doc |

## Risks + mitigations

| Risk | Mitigation |
|---|---|
| Token leakage via process listing / env var | Token in env var visible to ROOT but not other users; `--token-file` for service mode (read by uid only, chmod 0600); audit log uses ID prefix not full token |
| Self-signed cert UX (browsers / curl warnings) | Document `--cacert ~/.claudebase/server/cert.pem` for curl; production should use BYO from real CA |
| Service install requires elevated privileges on Windows | Install runs as user (no SCM admin needed for user-context services); document if operator wants system-context |
| Auth misconfigured → all requests 401 → service appears broken | `/livez` no-auth probe lets operator differentiate "auth misconfig" from "process not running" |
| Old UDS clients (current telegram-plugin-rs) break when UDS path adds auth | Phase 2 auth enforcement on UDS is gated by a flag so operator can flip safely; default OFF for UDS in foundation, ON in downstream when telegram-plugin-rs refactor lands |
| Token rotation downtime | 5-min grace period for old token; document operator runbook for staged rotation across cli fleet |
| Service crash → all downstream features die | OS init system (launchd/systemd/SCM) auto-restarts on crash; configure restart policy in service unit |
| Audit log fills disk | Daily rotation + 30-day retention + monitoring guidance in docs (operator can configure rotation policy via `--log-retention-days N`) |
| WebSocket connections survive token rotation? | Active WS connections expire on next ping after old token grace ends; client must reconnect with new token |

## Open questions

1. **UDS auth enforcement timing.** Should UDS path require token from Phase 2 (uniform), or delay enforcement until downstream plans refactor existing UDS clients (current telegram-plugin-rs)? Leaning: **flag-gated, default OFF in foundation, ON in downstream after refactor**. Avoids breaking existing UDS clients before they're ready.
2. **HTTP framework choice.** `hyper` (lightweight, direct) vs `axum` (richer ergonomics, built on hyper)? Both are in Rust mainstream. Leaning `hyper` for foundation (minimal deps); downstream plans can use `axum` selectively if they need its routing sugar.
3. **Token format.** `cb_<8-char-id>_<24-byte-base64>` proposed; should we use JWT instead (signed claims, expiry, etc)? Leaning **no JWT for v1** — random opaque token is simpler to operate; JWT adds key-management complexity.
4. **Sample service-unit files in repo.** Commit `deploy/launchd.plist.template` + `deploy/systemd.service.template` + `deploy/windows-service.toml` so operators have copy-paste starting points. Or generate at install time? Leaning: both (templates in repo as docs; install generates filled-in copy).
5. **`/health` endpoint depth.** Foundation returns lifecycle JSON. Should it ALSO ping downstream subsystems (registry, channel router, TG bridge)? Leaning **no** — keep `/health` cheap. Subsystems can have their own `/agents/health`, `/telegram/health` endpoints added in downstream plans.

## Effort estimate

| Phase | Estimate |
|---|---|
| 1 — HTTP/WSS listener | 1-2 days |
| 2 — Auth middleware | 0.5 day |
| 3 — Token lifecycle | 0.5-1 day |
| 4 — TLS via rustls | 1 day |
| 5 — Service install (3 OSes) | 1-2 days (cross-platform testing) |
| 6 — Logging + audit | 0.5 day |
| 7 — Health + rate limit | 0.5 day |
| 8 — Docs | 0.5-1 day |

**Total: ~5-8 dev days.** Foundation is intentionally compact — most of the surface area lives in downstream plans.

## Phasing pause-points

- **After Phase 2**: server boots, auth works, can hit `/health`. **This is the smallest viable foundation.** Could stop here briefly to let operator verify the auth flow before building service install on top.
- **After Phase 5**: service install works on operator's primary OS. Could pause here and start downstream plans against the running service before adding rate limit + audit polish.
- **Phase 6-8**: polish + docs. Necessary for production but not blocking downstream work.

## Files (planned changes)

```
claudebase/
├── src/
│   ├── daemon/
│   │   ├── server.rs                ← extend with HTTP/WSS listener (Phase 1)
│   │   ├── http.rs                  ← NEW — HTTP route handlers + middleware (Phases 1, 2, 7)
│   │   ├── auth.rs                  ← NEW — Bearer-token middleware (Phase 2)
│   │   ├── token.rs                 ← NEW — token generation + rotation + file IO (Phase 3)
│   │   ├── tls.rs                   ← NEW — rustls cert load + self-signed gen (Phase 4)
│   │   ├── service_install.rs       ← extend existing daemon install logic (Phase 5)
│   │   ├── audit_log.rs             ← NEW — structured audit trail (Phase 6)
│   │   └── rate_limit.rs            ← NEW — per-token sliding window (Phase 7)
│   └── cli.rs                       ← `claudebase server {install,uninstall,...,rotate-token,regenerate-cert,logs --audit}`
├── deploy/                          ← NEW dir
│   ├── launchd.plist.template
│   ├── systemd.service.template
│   └── windows-service.toml
├── docs/
│   ├── RELEASING.md                 ← server-mode release notes addendum (Phase 8)
│   ├── server-install.md            ← NEW — operator quickstart per OS (Phase 8)
│   └── plans/
│       └── claudebase-server-foundation.md  ← this file
└── tests/
    ├── server_lifecycle_test.rs     ← Phase 1
    ├── auth_middleware_test.rs      ← Phase 2
    ├── token_rotation_test.rs       ← Phase 3
    └── ...
```

## Downstream plans (after this foundation lands)

Once Phase 2 of this plan is done (server boots + auth works), downstream plans can be picked up in any order — they are largely independent of each other but all require this foundation:

### Plan A — Agent registry + cli-to-cli message bus + spawn

[`agent-registry-multi-cli.md`](./agent-registry-multi-cli.md)

Adds:
- `POST /agents/register`, `POST /agents/unregister`, `GET /agents/list_alive`, `GET /agents/query`
- WebSocket subscribe channel: `WSS /channels/subscribe?agent_id=X`
- `mcp__claudebase__agent_message`, `agent_query`, `agent_status`, `agent_spawn` MCP tools
- Permission-gated spawn flow

Effort estimate (post-foundation): ~7-10 dev days for Phases 2-7 (originally Phases 2-8 in that plan; Phase 1 has moved here).

### Plan B — Telegram-as-transport orchestration

[`telegram-multi-cli-orchestration.md`](./telegram-multi-cli-orchestration.md)

Adds:
- Server-side TG poller (one consumer per token)
- TG bot commands `/agents`, `/switch`, `/whoami`, `/here`
- `tg_message_map` for reply-quote routing
- `active_cli_per_user` state
- Refactors current `telegram-plugin-rs` to thin client of server

Effort estimate (post-foundation, post-Plan-A Phases 2+3): ~9-12 dev days.

### Plan C — Per-project `.claudebase/` dir

[`claudebase-project-dir.md`](./claudebase-project-dir.md)

Mostly cli-side and largely independent of server foundation — but the `identity.local` file mirrors the agent_registry record (from Plan A Phase 2), so it needs Plan A's registry to be meaningful.

Effort estimate (post-foundation): ~8-10 dev days. Can run partially in parallel with Plans A/B.

## Sequencing recommendation

```
[claudebase-server-foundation]   <-- THIS PLAN (5-8 days)
        │
        ├──> [agent-registry-multi-cli Phase 2-3]   (~3-4 days; minimal MVP for cli-to-cli)
        │           │
        │           ├──> [telegram-multi-cli-orchestration ALL phases]  (~9-12 days)
        │           │
        │           └──> [agent-registry-multi-cli Phase 4-7]   (~4-6 days; spawn, monitoring, knowledge replication)
        │
        └──> [claudebase-project-dir]   (~8-10 days; partially parallel after Plan A Phase 2)
```

**Critical path: foundation → Plan A Phases 2-3 → either Plan B or Plan A Phase 4+.** Total floor-time for full system: ~25-35 dev days. MVP (foundation + Plan A Phases 2-3 + Plan B MVP) ~17-22 dev days.

## Facts

### Verified facts

- `claudebase/src/daemon/server.rs` currently provides UDS-only IPC — verified by `ls daemon/` earlier this session (no `http.rs` / `tls.rs` exist yet). Salience: high (this plan adds them).
- `claudebase daemon {install,uninstall,start,stop,restart,status,logs}` subcommand family exists per `claudebase/src/main.rs::dispatch` block (read earlier this session). Phase 5 extends this; the install machinery is partially there. Salience: high.
- `windows-service` crate is in `claudebase/Cargo.toml` (target.'cfg(windows)' dependencies around line 200 area) — verified by grep this session. Phase 5 Windows path uses it. Salience: high.
- `tracing` + `tracing-subscriber` + reqwest's `rustls-tls` are in `claudebase/Cargo.toml` deps — verified by grep this session. No new dep needed for logging / TLS. Salience: medium.
- `hyper` is transitively pulled in via `reqwest` and `frankenstein`'s client-reqwest feature — verified by grep this session. Can be used directly as HTTP server framework. Salience: medium.

### External contracts

- `tracing-appender::rolling::Builder::new()` — symbol: file rotation policy (size/time-based) — source: docs.rs/tracing-appender. Already used downstream (via `tracing-subscriber`). Salience: medium.
- `windows-service` crate v0.8 — symbol: `Service::create`, `ServiceManager`, SCM dispatcher entry point — source: docs.rs/windows-service. Already in claudebase deps. Salience: high (Phase 5 Windows).
- `launchctl` CLI semantics — symbol: `launchctl load -w <plist>` / `launchctl bootstrap gui/$UID <plist>` (Big-Sur+) — source: Apple docs. Required for Phase 5 macOS. Salience: medium.
- `systemctl --user` semantics — symbol: `systemctl --user {enable,disable,start,stop,status} <unit>` — source: systemd.exec(5). Required for Phase 5 linux. Salience: medium.
- `hyper` v1.x — symbol: `Server::bind`, `Service` trait, `Body`, `Request<Body>` — source: docs.rs/hyper. Phase 1 HTTP listener. Salience: medium.
- `rustls` v0.23 — symbol: `ServerConfig`, `ServerName`, `CertificateChain` — source: docs.rs/rustls. Phase 4 TLS. Already in deps via reqwest. Salience: medium.

### Assumptions

- Operators on multi-user / multi-tenant systems are out of scope for v1 (single user, single token, single fleet). Multi-user multi-token would need RBAC; deferred. How to verify: revisit if multi-tenant demand arises. Salience: medium.
- Self-signed certs + ssh-tunnel are acceptable for dev / single-machine deploys; production needs BYO cert from real CA. Documented in Phase 4 risk row. Salience: medium.
- The existing `claudebase daemon serve` UDS path is preserved as-is — adding HTTP/WSS is additive, not replacing. Verified by reading existing `daemon/server.rs` earlier this session. Salience: high (no regression risk for current telegram-plugin-rs bridge).
- Operators are comfortable with audit logs containing token-id prefixes (8 chars of opaque random data) — this is standard logging practice. Salience: low.

### Open questions

(See `## Open questions` section above — 5 items deferred to Phase 1 kickoff.)

## Decisions

### Inbound validation

- Operator brief 2026-05-24: "сперва этот сервер потом вытекающее из него оркестрация. создай для него файл план с диаграммой ... и укажи в нем что после него те две следующие задачи. а в тех следующих укажи что они зависят от задачи с claudebase server". Coherent ask: extract Phase 1 of `agent-registry-multi-cli.md` into its own foundation plan; reference downstream from here; mark dependency upstream in the two downstream plans. Outcome: this file is the extraction; cross-refs added below in scope. Salience: high.

### Decisions made

- **Decision:** Foundation plan extracted as separate file (not Phase 1 inline in agent-registry-multi-cli). Alternatives considered: (a) keep Phase 1 inline — rejected because it conflates "load-bearing critical-path service infra" with "registry + bus features"; (b) merge all 4 plans into one mega-doc — rejected because each has different focal owners and review scope. Q1-Q5: not a hack ✓ / proportionate (single shippable foundation justifies own doc) ✓ / alternatives evaluated ✓ / addresses root cause (clear dependency chain) ✓ / n/a. Salience: high.
- **D1 — Auth MANDATORY (re-statement from agent-registry-multi-cli).** Locked here as a load-bearing principle. NO opt-out, NO permissive default. Salience: high.
- **D2 — Installable OS service (re-statement).** launchd/systemd/Windows SCM all supported; foreground mode preserved. Salience: high.
- **Decision:** Token format `cb_<8-char-id>_<24-byte-base64>` so audit log can record auditable token-id prefix without leaking secret. Alternative (JWT) rejected for v1 to keep secret-management simple. Salience: medium.
- **Decision:** `/livez` endpoint exempt from auth for OS-level readiness probes. NO other endpoint exempt. The `/livez` exemption is documented + scoped to "200 OK" literal — does NOT leak any state. Salience: medium.
- **Decision:** Phase 1 HTTP framework = `hyper` direct, NOT `axum`. Foundation keeps dep minimal; downstream plans can use `axum` selectively if they want route-DSL ergonomics. Salience: low.

### Hacks acknowledged

- v1 token storage uses simple file (chmod 0600) + env var fallback. Removal path: migrate to OS keychain (macOS Keychain / Windows Credential Manager / Linux Secret Service) in v2 when operator security posture demands it.
- v1 self-signed cert flow is "good enough" for dev / single-machine. Removal path: document BYO cert from CA for production deploys; consider built-in ACME (Let's Encrypt) integration in v2 if operator wants auto-renewal.

### Symptom-only patches

(none) — this plan addresses the root design (foundation service before features) rather than working around its absence.
