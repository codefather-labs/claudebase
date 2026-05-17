# Async Discipline Invariants — claudebase daemon

This document is load-bearing. The five rules below are NOT preferences —
they are the load-bearing invariants that keep the daemon from deadlocking
or panicking the tokio runtime. Anyone touching `src/daemon/` or
`src/plugin/` MUST read it before adding async code.

The five rules:

## Rule 1 — `sync fn main` MUST be preserved

`src/main.rs` exports `fn main() -> std::process::ExitCode` (synchronous —
NOT `#[tokio::main]`). The tokio runtime is constructed lazily inside
`run_daemon_serve()` and `run_plugin_serve()` via the
`crate::daemon::run_tokio()` helper, and ONLY for those two CLI arms.

Why: every other CLI subcommand (`ingest`, `search`, `list`, `status`,
`page`, `insight ...`) is fully synchronous. Turning `main` into
`#[tokio::main]` would force the entire binary to bootstrap the
multi-thread runtime even for `claudebase --version`, wasting startup
time and forcing every subcommand to deal with tokio context where none
is needed.

A pull request that converts `fn main()` to `async fn main()` is a
regression — push back.

## Rule 2 — Never `.await` while holding a blocking Mutex

The three blocking mutexes in this codebase are `std::sync::Mutex`
(NOT `tokio::sync::Mutex`):

- `PDFIUM` — `src/pdf.rs`
- `ENCODER` — `src/encoder.rs`
- `OCR_ENGINE` — `src/ocr.rs`

Holding one of these across an `.await` point blocks the tokio worker
thread. Once all worker threads reach the same `.await` while holding
their guards, the runtime deadlocks — no task can make progress and the
daemon hangs without surfacing an error.

The correct pattern when async code MUST touch one of these mutexes:

```rust
let result = tokio::task::spawn_blocking(move || {
    let guard = PDFIUM.lock().expect("mutex poisoned");
    do_blocking_work(&guard)
}).await?;
```

Do NOT use `tokio::task::block_in_place` — it requires the multi-thread
runtime AND has subtle correctness traps when nested or when only one
worker thread remains. `spawn_blocking` is the only blessed escape
hatch.

Do NOT write `MUTEX.lock().unwrap()` inline in async context — that is
exactly the failure mode this rule exists to prevent.

## Rule 3 — `tokio::spawn` MUST be panic-safe

Every `tokio::spawn` call site MUST ensure the spawned future does not
panic OR catches its own panic before returning. Reasons:

1. A panic inside a spawned task crashes only that task, but the
   `JoinHandle::await` returns `Err(JoinError)` — easy to swallow.
2. Tasks that hold OS resources (the UDS listener, the chat.db
   connection pool, the Telegram long-poll session) leak those
   resources on panic if cleanup isn't explicit.
3. The accept-loop in `src/daemon/server.rs` MUST survive any single
   connection's panic — one buggy MCP client must not take down the
   daemon.

Concrete rules:

- Wrap connection handlers in an explicit `if let Err(e) = ... { log }`
  block — never propagate `?` out of the outer `tokio::spawn`.
- Avoid `.unwrap()` / `.expect()` in spawned task bodies for paths that
  can be reached at runtime (parsing untrusted input, network I/O).
  `expect` is acceptable only for invariants that are truly impossible
  to violate (e.g., a `Mutex` we just constructed cannot already be
  poisoned).
- Use `tracing::error!` to surface task-internal failures with structured
  fields (`connection_id`, `peer`, `op`) so the next slice's tracing
  infrastructure can route them.

## Rule 4 — `tokio::select!` MUST be cancellation-safe

When a branch of `tokio::select!` wins, every other branch's future is
CANCELLED — dropped mid-await. If those futures hold partial state
(half-read bytes, an in-progress write, a held lock), the state is lost
or corrupted.

Concrete rules:

- Inside `tokio::select!`, every future passed to a branch MUST be
  cancellation-safe per its documentation. `AsyncBufReadExt::read_line`
  IS cancellation-safe; `AsyncReadExt::read` IS NOT (per tokio docs).
- Do NOT spawn ad-hoc futures inside `select!` arms — pre-construct them
  outside or use `tokio::pin!` so cancellation drops cleanly.
- For our STDIO↔UDS bridge in `src/plugin/bridge.rs`, the select arms
  are: `lines.next_line()` (cancellation-safe), `read_frame(...)`
  (cancellation-safe — uses `read_exact` which docs say IS safe when
  the future is dropped before completion, as the bytes already
  consumed are lost together with the borrowed buffer), and a
  reconnect timer (`tokio::time::sleep`, trivially cancellation-safe).
- If a future is NOT cancellation-safe, wrap it in `tokio::spawn` and
  select over the join-handle (cancellation-safe) instead of the
  future itself.

## Rule 5 — Spawned tasks MUST NOT `.unwrap()` on runtime values

A panic in a spawned task is silent unless its `JoinHandle` is awaited
AND the await result is inspected. In our accept-loop pattern, we DO
NOT await the per-connection handles (they run independently for the
lifetime of the connection), so an unhandled panic vanishes without a
log line.

Concrete rules:

- `.unwrap()` and `.expect()` in spawned task bodies are tech debt
  unless guarded by an invariant the compiler can verify.
- All fallible operations inside a spawned task body return `Result`
  to the task entry point, where a wrapping `if let Err(e) = ...` logs
  via `tracing::error!` before the task ends.
- A `tracing::error!` IS the safety net — without it, a spawned task's
  failure is invisible. Reviewers MUST flag any `.unwrap()` inside a
  `tokio::spawn` body that is not justified by a compile-time invariant.

## See also

- `src/main.rs` — the INVARIANT comment block at the top of the file
  carries the short-form reference back to this document.
- `src/daemon/server.rs` — accept-loop and connection-handler patterns
  follow rules 3/4/5.
- `src/plugin/bridge.rs` — `tokio::select!` over STDIO + UDS is the
  canonical example of rule 4 in this codebase.
