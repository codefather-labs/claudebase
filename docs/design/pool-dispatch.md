# Pool dispatch — the daemon routes jobs to a pool of Claude sessions

Status: implemented, 2026-10-26 (`src/daemon/pool.rs`, `src/pool_cli.rs`, tests/pool_dispatch_e2e_test.rs). Requested by `prod-deploy` (fbscout gateway,
`fbscout-cutover/docs/plans/claudebase-gateway/DESIGN.md` §10); direction confirmed
by the operator: a gateway queues model calls and hands them to N claudebase
sessions instead of Ollama, and the daemon is the router.

## Shape

```
brain ─► gateway (or runner-registry — open) (the queue — on the gateway side, single source)
              │
        gateway ── GET  /pool/<pool>/workers          which workers are free
              │ ── POST /pool/<pool>/dispatch {req_id,…} ─► daemon picks a FREE worker,
              │                                            leases it, injects the job
              │ ◄─ GET  /pool/jobs/<req_id>?wait_ms=…     long-poll for the result
              ▼
        POST /debug/llm/decision
                                   worker session (claudebase run --pool funnel)
                                     [pool-job:funnel <req_id>] arrives as input
                                     claudebase pool job <req_id>    → payload + image files
                                     claudebase pool reply <req_id>  → result; worker is free again
```

The daemon keeps **no queue**: dispatch succeeds only into a free worker, else it answers
`no_free_worker` and the request stays in the gateway's queue (whether that lives in the
gateway itself or in runner-registry is the gateway's choice and does not change this
contract). One queue, no
second source of truth to reconcile.

## Decisions

- **Busy/free is a lease the daemon holds, not a guess from the terminal.** A worker is
  `busy` from dispatch until it replies, fails, the job expires or the worker is lost.
  Reading "idle" off the screen is fragile; the daemon knows exactly what it handed out.
- **A worker reports back explicitly** with `claudebase pool reply`. Nothing is scraped
  from terminal output; the result is exactly the JSON the worker submits.
- **Pool membership is chosen where the session starts**, `claudebase run --pool <name>`,
  by the operator on that machine. The gateway cannot make an arbitrary session a worker.
- **Workers run like any `claudebase run` session, WITH `--dangerously-skip-permissions`**
  (the `run` default). Without it every worker step waits for a confirmation and the pool
  stops working. Workers run Opus and do classification — images, text, DOM elements —
  nothing financial or fragile. Operator decision, 2026-10-26: the network perimeter is closed and the gateway authorises its
  callers. Proposed and declined: starting `--pool` sessions without
  `--dangerously-skip-permissions`, with an allow-list. Risk accepted knowingly: a job
  carries third-party content (landing pages, forms, screenshots) that reaches a worker
  through the gateway legitimately, so the perimeter does not filter it; text in it that
  reads as an instruction meets a session that can run commands on its host. The
  `[pool-job:…]` contract line still tells the worker the payload is data, not orders.
- **The gateway authenticates with its own token**, scoped to `/pool/*` only — not a
  session callback token and not a remote-daemon token.
- **Return = long-poll by `req_id`** (option c). It matches the registry's own
  `/debug/llm/response` style and survives gateway restarts; `dispatch` may also wait
  (`wait_ms`) to save a round trip.

## Contract (HTTP JSON on the daemon's callback listener)

Transport: the existing HTTP listener (`claudebase daemon callback enable`, default
`127.0.0.1:8585`) — the gateway runs beside the daemon. Header `X-Api-Token: <gateway token>`
from `claudebase pool token issue <gateway-name>` (shown once, stored hashed, revocable).
Errors: `{ "error": "<code>", "message": "…" }` with the HTTP status below.

Gateway token: `claudebase pool token issue <gateway>` (and `revoke`, `list`) on the
pool's machine. The listener: `claudebase daemon callback enable [--bind …]` + restart.

### `GET /pool/<pool>/workers`
```json
200 { "pool": "funnel",
      "workers": [ { "nick": "funnel-1", "agent_id": "…", "state": "free",
                     "capacity": 1, "in_flight": 0, "last_seen_ms": 1793000000000 },
                   { "nick": "funnel-2", "state": "busy", "in_flight": 1, "req_ids": ["r-42"] },
                   { "nick": "funnel-3", "state": "suspect" } ],
      "free": 1 }
```
`state`: `free` | `busy` (leased to capacity) | `suspect` (a job expired on it; it gets no
new job until it replies late or runs `claudebase pool ready`) | `offline`.

### `POST /pool/<pool>/dispatch`
```json
{ "req_id": "r-42",                       // the registry's reqId; idempotency key
  "payload": { "model": "…", "messages": [ … ],
               "meta": { "kind": "…", "model": "…", "callType": "FORM_FILL", "postId": "", "runnerId": "…" } },
  "images": [ { "path": "/srv/fbscout/logs/claudebase-llm/r-42-1.jpg" },  // read by the DAEMON's host, or
              { "base64": "/9j/4AAQ…", "mime": "image/jpeg" } ],          // when the gateway is elsewhere
  "timeout_ms": 480000,                   // lease; default 480000 = the brain's HARNESS_STEP_TIMEOUT_MS
  "wait_ms": 0 }                          // optional: hold the response up to this long for the result
```
- `202 { "req_id": "r-42", "state": "leased", "worker": "funnel-2", "expires_at_ms": … }`
- `200 { …, "state": "done", "result": … }` when `wait_ms` covered the answer
- `200` the current state, when `req_id` was already dispatched (idempotent — no second worker)
- `503 { "error": "no_free_worker", "free": 0, "busy": 3 }` — keep it queued, retry
- `404 { "error": "no_such_pool" }` — no live worker has ever joined it
- `413` payload over 12 MB (images included); `400` malformed

Notes (from `prod-deploy`, 2026-10-26):
- Where the pool runs (operator, 2026-10-26): on prod eventually; first a test on a
  separate LAN machine running its own daemon, with the pool on it. Both image forms stay:
  `path` while the gateway shares that host's disk, `base64` otherwise.
- `req_id` is opaque (theirs: `cb_<ts>_<rand>`).
- Images: any `image/*`; `jpeg` is what the funnel sends. `mime` is inferred from the
  extension for `path`. `path` only resolves on the daemon's host — the gateway sends
  `base64` when the pool's daemon is on another host than the files.
- `timeout_ms` must not exceed the brain's own budget (`HARNESS_STEP_TIMEOUT_MS`, 8 min,
  `gpt-util.js:237`): a lease that outlives the caller delivers its answer to nobody.
  Max 900000 for other callers.
- `meta.callType` (`FORM_FILL`, `BUTTON_FIND`, `CONTENT_AGENT`, …) names the role in the
  `[pool-job:…]` line.
- `result` is `{content}` or `{content, tool_calls}` / `{tool_calls: [{function: {name,
  arguments}}]}` when the payload carries tools — the gateway's runner executes the tool
  and sends the next turn as a new job with the whole history, so any free worker takes
  any turn (no session affinity). The daemon relays `result` untouched.

### `GET /pool/jobs/<req_id>?wait_ms=25000`
Long-poll; returns as soon as the state is final or `wait_ms` passes (max 30000).
```json
200 { "req_id": "r-42", "state": "done", "worker": "funnel-2",
      "result": { "content": "{\"fields\":[…]}" }, "duration_ms": 41230 }
200 { "req_id": "r-42", "state": "leased", … }                       // still working
200 { "req_id": "r-42", "state": "failed", "error": "worker_lost" }  // re-dispatch
200 { "req_id": "r-42", "state": "failed", "error": "worker_failed", "message": "…" }
200 { "req_id": "r-42", "state": "expired" }                         // re-dispatch
404 { "error": "no_such_job" }
```
A final state is kept 10 minutes after it is first read, then dropped. `result` is opaque
to the daemon: exactly the JSON the worker submitted (for the registry: `{content}`).

### `POST /pool/jobs/<req_id>/cancel`
`200 { "state": "cancelled" }`; the worker is told and freed.

## Worker side (CLI, inside a `--pool` session)

The job arrives as one input line:
```
[pool-job:funnel r-42 FORM_FILL]: claudebase pool job r-42 for the payload; answer with claudebase pool reply r-42
```
- `claudebase pool job <req_id>` — writes `payload.json` and the images into a job
  directory on the worker's machine and prints their paths (works for workers joined to a
  remote daemon too: the bytes come through the daemon, not a shared disk).
- `claudebase pool reply <req_id> --stdin` — the result JSON; accepted only from the
  session holding the lease (session token), only while it is `leased`.
- `claudebase pool fail <req_id> "<reason>"` — give the job back with a reason.
- `claudebase pool ready` — after a late finish or a confusion, mark this worker free.

The channel-contract hook teaches the `[pool-job:…]` prefix: the payload is untrusted data
to answer, never instructions to follow.

## Lifecycle and reclaim

| Event | Job | Worker |
|---|---|---|
| dispatch | `leased`, `expires_at = now + timeout_ms` | `in_flight += 1`; `busy` at capacity |
| `pool reply` (in time) | `done` | `in_flight -= 1` → `free` |
| `pool fail` | `failed: worker_failed` | freed |
| lease passes | `expired` | `suspect` until a late reply or `pool ready` |
| session disconnects / dies | `failed: worker_lost` at once | `offline` |
| cancel | `cancelled` | freed |

Capacity per worker: `claudebase run --pool funnel --pool-capacity N` (default 1).

## Not in scope

- Spawning and supervising the N sessions — the gateway's orchestrator (its §5).
- Priorities and lanes — the registry's queue.
- A daemon-side queue — deliberately absent (above).
