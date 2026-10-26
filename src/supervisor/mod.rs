//! `claudebase run` — PTY supervisor. Slice 2 + 3 of the pty-transport feature.
//!
//! Replaces the old `exec claude --channels plugin:telegram@…` wrapper. Instead
//! of handing the process image to `claude` and disappearing, the supervisor
//! stays alive as its parent:
//!
//! ```text
//!   operator's terminal ⇄ [supervisor] ⇄ PTY ⇄ claude
//!                              ↑
//!                    daemon (UDS): inbound Telegram / agent messages
//! ```
//!
//! Inbound messages are written into the PTY master, which is byte-for-byte
//! indistinguishable from the operator typing them. Nothing here depends on
//! Claude Code's plugin machinery, MCP protocol version, or channel allowlist —
//! the interface is a terminal, which is the most stable interface there is.
//!
//! ## The three findings this module is built around
//!
//! Measured in `spikes/pty_inject/`, evidence in `docs/qa/evidence/pty-inject/`:
//!
//! * **F-2** — a submit key written in the same buffer as the paste does
//!   nothing. Paste and Enter must be two writes separated by a pause.
//! * **F-3** — a modal dialog swallows the message entirely AND the submit key
//!   answers it (a stray CR once confirmed "Yes, use my browser"). So injection
//!   is gated on a modal detector, and a queued message is never dropped.
//! * **F-6** — injecting over a half-typed line silently concatenates: the
//!   operator's draft is merged into the inbound message and submitted. Since
//!   the supervisor proxies every keystroke, it tracks draft state exactly
//!   instead of guessing with a timer.

mod draft;
pub mod inject;
mod screen;

use std::io::{Read, Write};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Arc, Mutex};
use std::time::Duration;

use anyhow::{Context, Result};
use portable_pty::{native_pty_system, CommandBuilder, PtySize};
use serde_json::json;

use crate::cli::RunArgs;

pub use draft::DraftTracker;
pub use inject::{InboundMessage, Injector};
pub use identity::AgentIdentity;
pub use screen::ModalDetector;

/// Env vars exported into the `claude` child so any Bash call made from inside
/// that session can attribute itself without arguments (`telegram send`).
const ENV_AGENT_ID: &str = "CLAUDEBASE_AGENT_ID";
const ENV_SESSION: &str = "CLAUDEBASE_SESSION";
/// How many times a reconnect tries to register before giving up until the
/// next one. Three attempts over ~0.6s covers the daemon's startup contention
/// without delaying a session whose daemon is genuinely down.
const REGISTER_ATTEMPTS: u32 = 3;

/// Per-session token minted here, stored by the daemon at `agent_register`,
/// and presented by short-lived CLI processes (`claudebase agent chat`) to
/// prove which agent they belong to without the daemon trusting a bare id.
const ENV_SESSION_TOKEN: &str = "CLAUDEBASE_SESSION_TOKEN";

/// Turning "the operator's session is over" into "the child must exit".
///
/// The supervisor used to have no such path at all. It blocked on
/// `child.wait()` with no signal handlers, and the stdin pump swallowed EOF
/// silently -- so nothing connected the outer terminal going away to the
/// `claude` running on the INNER pty, which by construction cannot see the
/// outer terminal. A session left in that state keeps its transcript open, and
/// Claude Code will not offer a still-running conversation to `/resume`: the
/// operator loses access to their own history until the process is found and
/// killed by hand.
mod shutdown {
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::Arc;
    use std::time::{Duration, Instant};

    static REQUESTED: AtomicBool = AtomicBool::new(false);

    /// Signal handler. The ONLY thing it does is a store -- everything else
    /// (allocating, logging, killing) is unsafe from a handler context.
    #[cfg(unix)]
    extern "C" fn on_signal(_sig: libc::c_int) {
        REQUESTED.store(true, Ordering::SeqCst);
    }

    /// SIGHUP is the terminal going away, SIGTERM is a service stop, SIGINT is
    /// an explicit `kill -INT`. Ctrl+C is NOT affected: the terminal is in raw
    /// mode, so it arrives as a byte on stdin and is forwarded to the child
    /// like any other keystroke.
    #[cfg(unix)]
    pub fn install_handlers() {
        unsafe {
            libc::signal(libc::SIGHUP, on_signal as libc::sighandler_t);
            libc::signal(libc::SIGTERM, on_signal as libc::sighandler_t);
            libc::signal(libc::SIGINT, on_signal as libc::sighandler_t);
        }
    }

    #[cfg(not(unix))]
    pub fn install_handlers() {}

    pub fn request() {
        REQUESTED.store(true, Ordering::SeqCst);
    }

    pub fn requested() -> bool {
        REQUESTED.load(Ordering::SeqCst)
    }

    /// Give the child a chance to save, then insist.
    ///
    /// SIGHUP first because that is exactly what it would have received had it
    /// been on the operator's real terminal rather than ours. SIGTERM next.
    /// SIGKILL last and bounded: a wedged child must not be able to hold the
    /// operator's transcript hostage indefinitely, which is the whole failure
    /// being fixed.
    #[cfg(unix)]
    fn escalate(pid: i32, done: &Arc<AtomicBool>) {
        const LADDER: [(libc::c_int, Duration); 2] = [
            (libc::SIGHUP, Duration::from_secs(3)),
            (libc::SIGTERM, Duration::from_secs(5)),
        ];

        for (sig, grace) in LADDER {
            signal_child(pid, sig);
            let deadline = Instant::now() + grace;
            while Instant::now() < deadline {
                if done.load(Ordering::SeqCst) {
                    return;
                }
                std::thread::sleep(Duration::from_millis(50));
            }
        }

        tracing::warn!(pid, "child ignored SIGHUP and SIGTERM; sending SIGKILL");
        signal_child(pid, libc::SIGKILL);
    }

    /// Signal the child, and only the child.
    ///
    /// `claude` is the session leader of the inner pty, so its process group is
    /// its own and signalling the group reaches the helpers it spawned -- the
    /// same reach a real hangup would have. The group is used ONLY after
    /// confirming `getpgid(pid) == pid`; without that check a wrong pgid would
    /// signal our own group, and the supervisor would kill the operator's
    /// shell along with itself.
    #[cfg(unix)]
    fn signal_child(pid: i32, sig: libc::c_int) {
        unsafe {
            if libc::getpgid(pid) == pid {
                libc::killpg(pid, sig);
            } else {
                libc::kill(pid, sig);
            }
        }
    }

    /// Watch for a shutdown request for as long as the child is alive.
    #[cfg(unix)]
    pub fn spawn_watcher(child_pid: Option<u32>, done: Arc<AtomicBool>) {
        let Some(pid) = child_pid else {
            tracing::warn!("no child pid — cannot propagate session shutdown to `claude`");
            return;
        };
        let pid = pid as i32;
        std::thread::spawn(move || {
            loop {
                if done.load(Ordering::SeqCst) {
                    return;
                }
                if requested() {
                    tracing::info!(pid, "session ending — asking `claude` to exit");
                    escalate(pid, &done);
                    return;
                }
                std::thread::sleep(Duration::from_millis(100));
            }
        });
    }

    #[cfg(not(unix))]
    pub fn spawn_watcher(_child_pid: Option<u32>, _done: Arc<AtomicBool>) {}
}

/// Entry point for `claudebase run`.
pub fn run(args: &RunArgs) -> Result<std::process::ExitCode> {
    let identity = identity::derive(args.nick.as_deref());
    tracing::info!(agent_id = %identity.agent_id, name = %identity.name, "supervisor identity");

    let (rows, cols) = term::win_size().unwrap_or((24, 80));
    let pty = native_pty_system();
    let pair = pty
        .openpty(PtySize {
            rows,
            cols,
            pixel_width: 0,
            pixel_height: 0,
        })
        .context("openpty")?;

    let mut cmd = CommandBuilder::new("claude");
    // NOTE: no `--channels`. The whole point of this transport is that Claude
    // Code runs unmodified and unaware.
    cmd.args(args.claude_args());
    if let Ok(cwd) = std::env::current_dir() {
        cmd.cwd(cwd);
    }
    cmd.env(ENV_AGENT_ID, &identity.agent_id);
    cmd.env(ENV_SESSION, &identity.name);
    cmd.env(ENV_SESSION_TOKEN, &identity.session_token);
    // The Claude token handed to this machine with `claudebase auth setup-token`
    // (docs/design/auth-token.md) — unless the operator's environment already
    // carries one, which wins.
    if std::env::var_os(crate::daemon::auth::TOKEN_ENV).is_none() {
        if let Some(token) = crate::daemon::auth::stored_token() {
            cmd.env(crate::daemon::auth::TOKEN_ENV, token);
        }
    }

    let mut child = pair
        .slave
        .spawn_command(cmd)
        .context("spawn `claude` — is the Claude Code CLI on PATH?")?;
    drop(pair.slave);

    // Captured at spawn, from the child handle itself. Every later signal is
    // aimed at THIS pid and nothing else -- no matching by process name, which
    // reliably finds the operator's other sessions instead.
    let child_pid = child.process_id();

    let mut reader = pair.master.try_clone_reader().context("clone pty reader")?;
    let writer = Arc::new(Mutex::new(pair.master.take_writer().context("pty writer")?));
    let master = Arc::new(Mutex::new(pair.master));

    let done = Arc::new(AtomicBool::new(false));
    shutdown::install_handlers();
    shutdown::spawn_watcher(child_pid, done.clone());

    let draft = Arc::new(DraftTracker::new());
    let modal = Arc::new(ModalDetector::new());

    // Raw mode last: any failure above still prints on a sane terminal. The
    // guard restores termios on every exit path, including panics.
    let raw = term::RawGuard::enter()?;
    let interactive = raw.is_some();
    if !interactive {
        tracing::warn!("stdin is not a TTY — operator proxying disabled, injection still active");
    }

    // ---- pty -> our stdout, feeding the modal detector on the way ----
    //
    // `drained_tx` says "the pty gave me everything it had". Exit waits on it
    // BRIEFLY rather than joining: on Windows a ConPTY read does not necessarily
    // return when the child dies — the master handle is still alive, held here
    // and by the resize thread — so joining this thread can park the supervisor
    // forever after `claude` has already gone.
    let (drained_tx, drained_rx) = std::sync::mpsc::channel::<()>();
    let done_out = done.clone();
    let modal_out = modal.clone();
    let pump_out = std::thread::spawn(move || {
        let mut buf = [0u8; 8192];
        let mut out = std::io::stdout();
        loop {
            match reader.read(&mut buf) {
                Ok(0) => break,
                Ok(n) => {
                    modal_out.feed(&buf[..n]);
                    if out.write_all(&buf[..n]).is_err() || out.flush().is_err() {
                        break;
                    }
                }
                Err(_) => break,
            }
        }
        done_out.store(true, Ordering::SeqCst);
        let _ = drained_tx.send(());
    });

    // ---- operator's stdin -> pty, feeding the draft tracker on the way ----
    //
    // Detached: a blocking read on a real terminal cannot be interrupted
    // portably, so this thread is left parked on read(2) at exit and reaped
    // by the OS.
    if interactive {
        let writer_in = writer.clone();
        let done_in = done.clone();
        let draft_in = draft.clone();
        std::thread::spawn(move || {
            let mut buf = [0u8; 1024];
            let mut stdin = std::io::stdin();
            loop {
                if done_in.load(Ordering::SeqCst) {
                    break;
                }
                match stdin.read(&mut buf) {
                    // stdin is a TTY here (this thread only runs when it is),
                    // so EOF means the operator's terminal went away. The child
                    // is on a different pty and will never notice on its own.
                    Ok(0) => {
                        shutdown::request();
                        break;
                    }
                    Ok(n) => {
                        draft_in.observe_operator_input(&buf[..n]);
                        let Ok(mut w) = writer_in.lock() else { break };
                        if w.write_all(&buf[..n]).is_err() || w.flush().is_err() {
                            shutdown::request();
                            break;
                        }
                    }
                    Err(_) => {
                        shutdown::request();
                        break;
                    }
                }
            }
        });
    }

    // ---- window-size follower ----
    let master_rs = master.clone();
    let done_rs = done.clone();
    std::thread::spawn(move || {
        let mut last = (rows, cols);
        while !done_rs.load(Ordering::SeqCst) {
            std::thread::sleep(Duration::from_millis(500));
            if let Some(cur) = term::win_size() {
                if cur != last {
                    if let Ok(m) = master_rs.lock() {
                        let _ = m.resize(PtySize {
                            rows: cur.0,
                            cols: cur.1,
                            pixel_width: 0,
                            pixel_height: 0,
                        });
                    }
                    last = cur;
                }
            }
        }
    });

    // ---- injector: the only writer of inbound messages ----
    let (inbound_tx, inbound_rx) = mpsc::channel::<InboundMessage>();
    let injector = Injector::new(writer.clone(), draft.clone(), modal.clone(), done.clone());
    let inject_thread = std::thread::spawn(move || injector.run(inbound_rx));

    // ---- daemon leg: register, subscribe, forward notifications ----
    //
    // Runs on its own tokio runtime in a dedicated thread so the blocking
    // terminal pumps above never share a scheduler with the socket.
    let subscribe_extra = args.subscribe.clone();
    let identity_for_daemon = identity.clone();
    let done_daemon = done.clone();
    let daemon_thread = std::thread::spawn(move || {
        crate::daemon::run_tokio(daemon_leg(
            identity_for_daemon,
            subscribe_extra,
            inbound_tx,
            done_daemon,
        ));
    });

    let status = child.wait().context("wait for claude")?;
    done.store(true, Ordering::SeqCst);

    // Give the terminal back FIRST, before any bookkeeping.
    //
    // This used to be the last statement, after three `join()`s. The child was
    // already gone by then, so the guard had no work left to do — but any join
    // that did not return kept the console in raw mode, with echo and line
    // input still off. The operator saw exactly that: `claude` exited, the
    // shell came back, and typing produced nothing but a newline, because the
    // console was still configured for a full-screen program that was no longer
    // running. On Windows there is no shell that quietly resets it afterwards.
    //
    // Restoring here costs nothing if the joins are quick and saves the
    // operator's terminal if they are not.
    drop(raw);

    // The pty pump is worth a SHORT wait: it copies the child's last output to
    // the screen. Bounded, because on Windows the read it sits in may never
    // return — see `drained_tx`. A truncated final line is a smaller price than
    // a supervisor that never exits, which is what the operator reported: the
    // session ended, the prompt came back, and the terminal was still held.
    let _ = drained_rx.recv_timeout(std::time::Duration::from_millis(300));
    drop(pump_out);

    // The other two are NOT joined, deliberately. Both are parked on a blocking
    // read — the injector on its channel, the daemon thread on its socket — and
    // neither notices `done` until something arrives, which for a session that
    // is ending may be never. They own nothing that needs unwinding: the daemon
    // marks this agent's row dead when the connection drops, which is precisely
    // what process exit does. Same reasoning the stdin pump already carries.
    drop(inject_thread);
    drop(daemon_thread);

    tracing::info!(?status, "claude exited; supervisor shutting down");
    Ok(if status.success() {
        std::process::ExitCode::SUCCESS
    } else {
        std::process::ExitCode::FAILURE
    })
}

/// Own the daemon connection for the session's lifetime: register this agent,
/// subscribe to the threads it should hear about, and forward every inbound
/// channel notification to the injector.
///
/// Every failure here is non-fatal by design — a session whose daemon is down
/// must still be a usable `claude` session, just without inbound messages.
async fn daemon_leg(
    identity: identity::AgentIdentity,
    extra_threads: Vec<String>,
    tx: mpsc::Sender<InboundMessage>,
    done: Arc<AtomicBool>,
) {
    use crate::daemon::subscribe_client::SubscribeClient;

    while !done.load(Ordering::SeqCst) {
        let mut client = match SubscribeClient::connect().await {
            Ok(c) => c,
            Err(e) => {
                tracing::warn!(error = %e, "daemon unreachable; retrying in 5s");
                tokio::time::sleep(Duration::from_secs(5)).await;
                continue;
            }
        };

        // Registration is retried, because failing it is not a small thing:
        // an unregistered session is absent from `agent list`, from the
        // `/switch` menu, and from every nick-addressed route -- it is still
        // running, and nothing can reach it. A single attempt was enough to
        // lose a session to one transient `database is locked` during the
        // daemon's own startup burst, and it then stayed lost for the whole
        // life of the connection, because nothing ever tried again.
        // Resolved HERE, where the checkout is: a daemon on another machine has
        // no such directory to run `git` in, and the project id is what lets a
        // session on a laptop share prod's journal for the same repository.
        let here = std::env::current_dir().ok();
        let project_id = here.as_deref().map(crate::project_id::resolve_project_id);
        let branch = here.as_deref().and_then(crate::project_id::current_branch);
        let register_payload = || {
            json!({
                "project_id": project_id,
                "branch": branch,
                "agent_id": identity.agent_id,
                "name": identity.name,
                // `--nick` was never remembered: only a rename wrote the
                // memory, so a session started under a chosen name came
                // back as the project default next time. Saying so here
                // lets the daemon -- the only writer -- record it.
                "nick_chosen": identity.chosen,
                "session_token": identity.session_token,
                "cwd": std::env::current_dir().ok().map(|p| p.display().to_string()),
                // Where this session runs, so the daemon can tell a live
                // session from a row left by one that died while it was down.
                "host": crate::daemon::agent_registry::this_host(),
                "pid": std::process::id() as i64,
                // The per-window key for the nick memory; see
                // `identity::controlling_terminal`.
                "terminal": identity::controlling_terminal(),
            })
        };
        let mut registered = false;
        for attempt in 1..=REGISTER_ATTEMPTS {
            match client.call("agent_register", register_payload()).await {
                Ok(_) => {
                    registered = true;
                    break;
                }
                Err(e) => {
                    tracing::warn!(
                        attempt,
                        of = REGISTER_ATTEMPTS,
                        error = %e,
                        "agent_register failed"
                    );
                    if attempt < REGISTER_ATTEMPTS {
                        tokio::time::sleep(Duration::from_millis(200 * attempt as u64)).await;
                    }
                }
            }
        }
        if !registered {
            tracing::error!(
                "agent_register failed every attempt — this session is running but \
                 unreachable by nick until it reconnects"
            );
        }

        client.subscribe_all(&identity, &extra_threads).await;

        // Pump notifications until the connection dies, then reconnect and
        // re-subscribe — a daemon restart must not silently end delivery
        // (the failure mode documented in docs/issues/006).
        if let Err(e) = client
            .pump(&identity, &extra_threads, &tx, &done)
            .await
        {
            tracing::warn!(error = %e, "daemon connection ended; will reconnect");
        }
        if done.load(Ordering::SeqCst) {
            break;
        }
        tokio::time::sleep(Duration::from_secs(2)).await;
    }
}

/// Threads this session listens on: every paired Telegram chat, every Telegram
/// thread already in chat.db, this agent's own inbox, plus whatever the
/// operator named with `--subscribe`.
/// Threads this session listens on.
///
/// **Only what is addressed to THIS session.** An earlier version subscribed to
/// every Telegram thread in `access.json` plus every thread in `chat.db`, on the
/// theory that more subscriptions can only help. It does the opposite:
///
/// * a session in an unrelated project received the operator's chat even though
///   `/switch` had bound that chat to a different session — observed live
///   2026-08-16, an `fbscout` session receiving traffic meant for `planner`;
/// * `chat_subscribe` DRAINS the thread's backlog and marks it delivered, so
///   whichever session subscribed first swallowed messages meant for another
///   one — they were not merely copied, they were taken.
///
/// So the scope is: this agent's own inbox, the Telegram chats `/switch`-bound
/// to it, and whatever the operator named explicitly with `--subscribe`
/// (testing, or a chat the binding does not know about yet).
///
/// Consequence worth knowing: until the operator runs `/switch`, no session is
/// bound and Telegram messages reach nobody. That is the daemon's designed
/// behaviour — the bot answers "No CLI is bound to this chat/topic" — and it is
/// better than the alternative of every session hearing every chat.
pub(crate) fn threads_to_subscribe(identity: &identity::AgentIdentity, extra: &[String]) -> Vec<String> {
    let mut threads: Vec<String> = Vec::new();
    threads.push(format!("agent:{}", identity.agent_id));

    if let Ok(conn) = crate::daemon::chat::open_chat_db() {
        threads.extend(crate::daemon::agent_registry::bound_telegram_threads(&conn, &identity.agent_id).unwrap_or_default());
    }

    threads.extend(extra.iter().cloned());
    threads.sort();
    threads.dedup();
    threads
}


/// Is `thread` still addressed to this session?
///
/// Checked at INJECTION time, not only at subscribe time, because there is no
/// unsubscribe: after the operator `/switch`-es a chat away, the previous
/// session keeps its subscription until it reconnects. Without this check it
/// would go on injecting messages that now belong to someone else.
/// Does this session own `(chat_id, topic)`?
///
/// The chat-wide question `thread_belongs_to` asks has no single answer in a
/// forum: topic 3 belongs to one session and topic 4 to another, and picking
/// "the most recently pinged session in this chat" hands the whole chat to one
/// of them. That is what silently discarded a second session's entire backlog
/// with "chat is bound to another session".
pub(crate) fn routing_belongs_to(agent_id: &str, chat_id: i64, topic: Option<i64>) -> bool {
    let conn = match crate::daemon::chat::open_chat_db_readonly() {
        Ok(c) => c,
        Err(e) => {
            // Fail closed, for the reason spelled out in `thread_belongs_to`:
            // a withheld message comes back from the backlog, a misdelivered
            // one cannot be taken back.
            tracing::warn!(error = %e, chat_id, ?topic, "cannot determine topic ownership; withholding");
            return false;
        }
    };
    let bound = crate::daemon::agent_registry::topic_owner(&conn, chat_id, topic).unwrap_or(None);
    match bound {
        Some(owner) => owner == agent_id,
        // Nobody holds this exact topic. Withhold rather than broadcast: an
        // unbound topic is one the operator has not assigned yet, and guessing
        // is what the whole addressed-delivery rule exists to stop.
        None => false,
    }
}

pub(crate) fn thread_belongs_to(agent_id: &str, thread: &str) -> bool {
    let Some(chat) = thread.strip_prefix("telegram:") else {
        // `agent:<self>` and explicit `--subscribe` threads are ours by
        // construction; only Telegram chats carry a routing binding.
        return true;
    };
    let Ok(chat_id) = chat.parse::<i64>() else {
        return true;
    };
    // Read-only: `open_chat_db` runs the schema ensure, a WRITE transaction, on
    // every open, so asking "who owns this chat" used to compete for the lock
    // with the daemon writing the very message being routed.
    let conn = match crate::daemon::chat::open_chat_db_readonly() {
        Ok(c) => c,
        Err(e) => {
            // Fail CLOSED. This used to `return true`, which turned a busy
            // database into "nobody owns this chat" and delivered the message to
            // every subscriber — the operator then watched two sessions answer
            // each other in their own Telegram. A message withheld is recovered
            // from the backlog on the next reconnect; a message delivered to the
            // wrong session cannot be taken back.
            tracing::warn!(error = %e, thread, "cannot determine chat ownership; withholding");
            return false;
        }
    };

    // Any topic in this chat counts. The per-MESSAGE check decides which
    // topic is actually ours; this one only answers "is this chat any of our
    // business at all", and answering it with a single chat-wide owner is what
    // discarded a second session's backlog.
    let bound: Option<String> = match crate::daemon::agent_registry::chat_bound_to(&conn, chat_id, agent_id) {
        Ok(v) => v,
        Err(e) => {
            // Same reasoning, and this is the path that actually fired: `.ok()`
            // collapsed a failed query into `None`, which the match below read
            // as "unbound" and broadcast.
            tracing::warn!(error = %e, thread, "ownership query failed; withholding");
            return false;
        }
    };

    match bound {
        // Bound elsewhere -> not ours. Genuinely unbound -> nobody claimed it,
        // and the daemon broadcasts to whoever subscribed; keep it. That is a
        // real answer from the database, not a failure dressed as one.
        Some(owner) => owner == agent_id,
        None => true,
    }
}

pub mod identity {
    /// Session identity. `agent_id` is UNIQUE PER PROCESS — this is the fix for
    /// root cause 1 in docs/issues/006, where a pinned `session_id` in
    /// `.claudebase/config.json` made every session of a project
    /// indistinguishable and broadcasts landed on stale bridges. The `name`
    /// stays stable so `/switch` bindings and `agent list-alive` remain
    /// human-readable across restarts.
    #[derive(Clone, Debug)]
    pub struct AgentIdentity {
        pub agent_id: String,
        pub name: String,
        /// Capability handed to the child process; see ENV_SESSION_TOKEN.
        pub session_token: String,
        /// True when the operator named this session rather than the project
        /// naming it — `--nick`, or a name recalled from a previous choice.
        ///
        /// The daemon uses it to decide whether the name is worth remembering
        /// for the next session in this directory. A derived default is not:
        /// it is re-derived anyway, and remembering a disambiguated `-2` form
        /// would pin that suffix on the directory forever.
        pub chosen: bool,
    }

    /// The terminal this session is attached to, or an empty string when there
    /// is none.
    ///
    /// This is the per-window key the nick memory needs. It is available here,
    /// BEFORE `claude` is spawned, which is what makes it usable: the nick has
    /// to be decided at spawn time, and anything the conversation itself
    /// generates — a session id, a transcript path — only exists afterwards.
    pub fn controlling_terminal() -> String {
        #[cfg(unix)]
        {
            let name = unsafe { libc::ttyname(libc::STDIN_FILENO) };
            if name.is_null() {
                return String::new();
            }
            unsafe { std::ffi::CStr::from_ptr(name) }
                .to_string_lossy()
                .into_owned()
        }
        #[cfg(not(unix))]
        {
            String::new()
        }
    }

    /// Make the nick unique among sessions that are actually running.
    ///
    /// The base nick comes from the project, so every session opened in one repo
    /// is called the same thing — and `/switch` then shows N identical buttons
    /// while silently binding to whichever registered last. A suffix is added
    /// only when a LIVE session already holds the name; restarting a session
    /// reuses the plain nick because the old row is superseded at register.
    /// A directory name is a label, not a nick: `tacticstrade.ai` has a dot,
    /// which `validate_agent_name` refuses, and a refused name left the session
    /// off the map entirely — no journal, no messages. Map everything outside
    /// `[a-zA-Z0-9_-]` to `-`, leaving room for `disambiguate`'s `-NN`.
    pub(crate) fn nick_from_label(label: &str) -> String {
        let mapped: String = label
            .chars()
            .map(|c| if c.is_ascii_alphanumeric() || c == '_' || c == '-' { c } else { '-' })
            .collect();
        let mut nick = String::new();
        for c in mapped.chars() {
            if !(c == '-' && nick.ends_with('-')) {
                nick.push(c);
            }
        }
        let nick: String = nick.trim_matches('-').chars().take(60).collect();
        if nick.is_empty() { "claude".to_string() } else { nick }
    }


    /// `explicit` comes from `--nick` and is used verbatim: the operator chose
    /// it so they can find it in `/switch`, and silently suffixing it would make
    /// the menu entry not match what they typed. A collision with a live session
    /// surfaces at register time as an error rather than being papered over.
    /// The nick this session starts under, asked of the daemon it joins: the
    /// one remembered for this host, directory and terminal if still free,
    /// else `base` made unique (`agent_registry::startup_nick`). Best-effort —
    /// a session that cannot ask must still start, under `base`.
    fn startup_nick(endpoint: &crate::daemon::remote::Endpoint, cwd: Option<&std::path::Path>, base: &str) -> (String, bool) {
        let Some(cwd) = cwd else { return (base.to_owned(), false) };
        let (host, terminal) = (crate::daemon::agent_registry::this_host(), controlling_terminal());
        let cwd = cwd.to_string_lossy().into_owned();
        if endpoint.is_local() {
            return match crate::daemon::chat::open_chat_db_readonly() {
                Ok(conn) => crate::daemon::agent_registry::startup_nick(&conn, &host, &cwd, &terminal, base),
                Err(_) => (base.to_owned(), false),
            };
        }
        let asked = crate::daemon::run_tokio(async {
            let mut c = crate::daemon::client::DaemonClient::connect_to(endpoint).await?;
            c.call_tool(
                "session_startup_nick",
                serde_json::json!({ "host": host, "cwd": cwd, "terminal": terminal, "base": base }),
            )
            .await
        });
        match asked {
            Ok(v) => (
                v["nick"].as_str().unwrap_or(base).to_owned(),
                v["chosen"].as_bool().unwrap_or(false),
            ),
            Err(e) => {
                tracing::warn!(error = %format!("{e:#}"), "could not ask the daemon for a nick; using the project default");
                (base.to_owned(), false)
            }
        }
    }

    pub fn derive(explicit: Option<&str>) -> AgentIdentity {
        if let Some(nick) = explicit.map(str::trim).filter(|s| !s.is_empty()) {
            return AgentIdentity {
                agent_id: uuid::Uuid::new_v4().to_string(),
                name: nick.to_string(),
                session_token: uuid::Uuid::new_v4().to_string(),
                chosen: true,
            };
        }

        let cwd = std::env::current_dir().ok();

        // A nick deliberately chosen for this directory wins over the project
        // default. Without this, every restart came back as `<project>` (or
        // `<project>-2`), and the Telegram binding `/switch` made against the
        // old name resolved to nobody — the operator had to `/switch` again
        // after every restart. Recalling the name is what makes
        // `restore_bindings_for` find anything to restore.
        let derived = cwd
            .as_ref()
            .and_then(|cwd| {
                crate::project_config::load(cwd)
                    .map(|cfg| cfg.name)
                    .or_else(|| {
                        cwd.file_name()
                            .map(|n| n.to_string_lossy().trim().to_string())
                            .filter(|s| !s.is_empty())
                    })
            })
            .unwrap_or_else(|| "claude".to_string());
        // `claudebase run` resolved and exported the daemon before we got here.
        let endpoint = crate::daemon::remote::Endpoint::resolve().unwrap_or(crate::daemon::remote::Endpoint::Local);
        let (name, chosen) = startup_nick(&endpoint, cwd.as_deref(), &nick_from_label(&derived));

        AgentIdentity {
            agent_id: uuid::Uuid::new_v4().to_string(),
            name,
            session_token: uuid::Uuid::new_v4().to_string(),
            chosen,
        }
    }
}

#[cfg(unix)]
mod term {
    use anyhow::Result;

    /// Saved termios of the real terminal, restored on drop.
    pub struct RawGuard {
        fd: i32,
        saved: libc::termios,
    }

    impl RawGuard {
        pub fn enter() -> Result<Option<Self>> {
            let fd = libc::STDIN_FILENO;
            if unsafe { libc::isatty(fd) } != 1 {
                return Ok(None);
            }
            let mut saved: libc::termios = unsafe { std::mem::zeroed() };
            if unsafe { libc::tcgetattr(fd, &mut saved) } != 0 {
                anyhow::bail!("tcgetattr: {}", std::io::Error::last_os_error());
            }
            let mut raw = saved;
            unsafe { libc::cfmakeraw(&mut raw) };
            if unsafe { libc::tcsetattr(fd, libc::TCSANOW, &raw) } != 0 {
                anyhow::bail!("tcsetattr: {}", std::io::Error::last_os_error());
            }
            Ok(Some(Self { fd, saved }))
        }
    }

    impl Drop for RawGuard {
        fn drop(&mut self) {
            unsafe { libc::tcsetattr(self.fd, libc::TCSANOW, &self.saved) };
        }
    }

    pub fn win_size() -> Option<(u16, u16)> {
        let mut ws: libc::winsize = unsafe { std::mem::zeroed() };
        let rc = unsafe { libc::ioctl(libc::STDIN_FILENO, libc::TIOCGWINSZ, &mut ws) };
        if rc != 0 || ws.ws_row == 0 || ws.ws_col == 0 {
            return None;
        }
        Some((ws.ws_row, ws.ws_col))
    }
}

/// Windows console equivalent of raw mode.
///
/// This used to be a stub returning `Ok(None)`, which the caller reads as "not
/// a TTY" and answers by never starting the keyboard-to-pty thread. So on
/// Windows nothing the operator typed ever reached `claude` — including the
/// terminal's automatic answer to the cursor-position query the TUI sends
/// before it draws. `claudebase run` therefore printed `ESC[6n` and hung
/// forever, which is what the operator saw as an endless loading screen.
///
/// Captured from the machine itself: the process was alive, the pty worked, the
/// child had started, and stdout contained exactly one escape sequence.
///
/// The Windows equivalent of `cfmakeraw` is three input flags off and one on:
/// `ENABLE_LINE_INPUT` and `ENABLE_ECHO_INPUT` make the console buffer a whole
/// line and echo it, which a full-screen TUI must do itself;
/// `ENABLE_PROCESSED_INPUT` lets the console eat Ctrl-C before we see it;
/// `ENABLE_VIRTUAL_TERMINAL_INPUT` is what makes keys arrive as the escape
/// sequences the child expects rather than as console key events. On the output
/// side `ENABLE_VIRTUAL_TERMINAL_PROCESSING` makes the child's own sequences
/// render instead of appearing literally.
#[cfg(windows)]
mod term {
    use anyhow::{bail, Result};
    use windows_sys::Win32::Foundation::{HANDLE, INVALID_HANDLE_VALUE};
    use windows_sys::Win32::System::Console::{
        GetConsoleMode, GetConsoleScreenBufferInfo, GetStdHandle, SetConsoleMode, CONSOLE_MODE,
        CONSOLE_SCREEN_BUFFER_INFO, ENABLE_ECHO_INPUT, ENABLE_LINE_INPUT, ENABLE_PROCESSED_INPUT,
        ENABLE_VIRTUAL_TERMINAL_INPUT, ENABLE_VIRTUAL_TERMINAL_PROCESSING, STD_INPUT_HANDLE,
        STD_OUTPUT_HANDLE,
    };

    /// Saved console modes, restored on drop — including on panic, which is why
    /// this is a guard and not a pair of calls.
    pub struct RawGuard {
        stdin: HANDLE,
        stdout: HANDLE,
        saved_in: CONSOLE_MODE,
        saved_out: CONSOLE_MODE,
    }

    impl RawGuard {
        pub fn enter() -> Result<Option<Self>> {
            unsafe {
                let stdin = GetStdHandle(STD_INPUT_HANDLE);
                let stdout = GetStdHandle(STD_OUTPUT_HANDLE);
                if stdin == INVALID_HANDLE_VALUE || stdout == INVALID_HANDLE_VALUE {
                    return Ok(None);
                }
                let mut saved_in: CONSOLE_MODE = 0;
                let mut saved_out: CONSOLE_MODE = 0;
                // Fails when the handle is a pipe rather than a console — the
                // Windows answer to "is this a TTY", and the same answer the
                // Unix branch gives via isatty: not interactive, carry on.
                if GetConsoleMode(stdin, &mut saved_in) == 0 {
                    return Ok(None);
                }
                if GetConsoleMode(stdout, &mut saved_out) == 0 {
                    return Ok(None);
                }

                let raw_in = (saved_in
                    & !(ENABLE_LINE_INPUT | ENABLE_ECHO_INPUT | ENABLE_PROCESSED_INPUT))
                    | ENABLE_VIRTUAL_TERMINAL_INPUT;
                if SetConsoleMode(stdin, raw_in) == 0 {
                    bail!(
                        "SetConsoleMode on stdin failed: {}",
                        std::io::Error::last_os_error()
                    );
                }
                // Best effort: an older console without VT processing still
                // delivers input, which is the half that was actually broken.
                let _ = SetConsoleMode(stdout, saved_out | ENABLE_VIRTUAL_TERMINAL_PROCESSING);

                Ok(Some(Self {
                    stdin,
                    stdout,
                    saved_in,
                    saved_out,
                }))
            }
        }
    }

    impl Drop for RawGuard {
        fn drop(&mut self) {
            unsafe {
                SetConsoleMode(self.stdin, self.saved_in);
                SetConsoleMode(self.stdout, self.saved_out);
            }
        }
    }

    /// The console WINDOW, not the screen buffer.
    ///
    /// `dwSize` is the buffer, which on Windows is routinely taller than what
    /// is visible — handing that to the pty makes the child draw a screen the
    /// operator can only see part of.
    pub fn win_size() -> Option<(u16, u16)> {
        unsafe {
            let stdout = GetStdHandle(STD_OUTPUT_HANDLE);
            if stdout == INVALID_HANDLE_VALUE {
                return None;
            }
            let mut info: CONSOLE_SCREEN_BUFFER_INFO = std::mem::zeroed();
            if GetConsoleScreenBufferInfo(stdout, &mut info) == 0 {
                return None;
            }
            let cols = info.srWindow.Right - info.srWindow.Left + 1;
            let rows = info.srWindow.Bottom - info.srWindow.Top + 1;
            if rows <= 0 || cols <= 0 {
                return None;
            }
            Some((rows as u16, cols as u16))
        }
    }
}

#[cfg(not(any(unix, windows)))]
mod term {
    use anyhow::Result;
    /// Neither unix nor windows: no console handling, injection still works.
    pub struct RawGuard;
    impl RawGuard {
        pub fn enter() -> Result<Option<Self>> {
            Ok(None)
        }
    }
    pub fn win_size() -> Option<(u16, u16)> {
        None
    }
}

#[cfg(test)]
mod ownership_tests {
    use super::thread_belongs_to;
    use std::sync::Mutex;

    static HOME_LOCK: Mutex<()> = Mutex::new(());

    /// Ownership must FAIL CLOSED.
    ///
    /// The check used to `return true` when the database could not be read and
    /// collapsed a failed query into "unbound" via `.ok()`. Either turned a
    /// transient lock into "nobody owns this chat", and the message went to every
    /// subscriber — which on 2026-08-18 had two sessions answering each other
    /// inside the operator's own Telegram chat. `open_chat_db` runs a write
    /// transaction on every open, so contention was not hypothetical.
    ///
    /// Pointing HOME at an empty directory makes the database unopenable, which
    /// is the same observable condition as an unreadable one.
    #[test]
    fn ownership_withholds_when_the_database_cannot_be_read() {
        let _guard = HOME_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        let empty = tempfile::tempdir().expect("tempdir");
        let prev_home = std::env::var_os("HOME");
        let prev_xdg = std::env::var_os("XDG_RUNTIME_DIR");
        std::env::set_var("HOME", empty.path());
        std::env::remove_var("XDG_RUNTIME_DIR");

        let verdict = thread_belongs_to("some-agent", "telegram:434566766");

        match prev_home {
            Some(v) => std::env::set_var("HOME", v),
            None => std::env::remove_var("HOME"),
        }
        if let Some(v) = prev_xdg {
            std::env::set_var("XDG_RUNTIME_DIR", v);
        }

        assert!(
            !verdict,
            "an undeterminable owner must withhold the message, not broadcast it: \
             a withheld message returns via the backlog, a misdelivered one cannot be recalled"
        );
    }

    /// Threads that carry no routing binding are ours by construction and must
    /// not be caught by the stricter rule.
    #[test]
    fn non_telegram_threads_are_always_ours() {
        assert!(thread_belongs_to("me", "agent:me"));
        assert!(thread_belongs_to("me", "some-explicit-subscription"));
    }
}

#[cfg(test)]
mod topic_ownership_tests {
    /// Two topics in one chat, owned by two sessions, and each must see only
    /// its own.
    ///
    /// The chat-wide ownership query took "the most recently pinged session
    /// bound to this chat" as the owner of the whole chat, so a forum with two
    /// bound topics handed everything to one session and the other's entire
    /// backlog was discarded with "chat is bound to another session". Confirmed
    /// live: the operator switched a second topic to a second session, sent a
    /// message, and that session reported receiving nothing at all.
    #[test]
    fn each_topic_belongs_to_its_own_session() {
        let dir = tempfile::tempdir().expect("tempdir");
        let prev = std::env::var_os("HOME");
        std::env::set_var("HOME", dir.path());
        std::env::set_var("CLAUDEBASE_HOME_OVERRIDE", dir.path());

        let result = (|| -> anyhow::Result<()> {
            let conn = crate::daemon::chat::open_chat_db()?;
            for (id, name, topic) in [("a-1", "transport", 3i64), ("a-2", "cutover", 4)] {
                conn.execute(
                    "INSERT INTO agent_registry \
                     (agent_id, agent_name, connection_id, chat_thread_id, spawned_at, \
                      last_pinged_at, state, routing_chat_id, routing_thread_id) \
                     VALUES (?1, ?2, ?1, NULL, 1, 1, 'alive', -1004451125152, ?3)",
                    rusqlite::params![id, name, topic],
                )?;
            }
            Ok(())
        })();

        let verdicts = if result.is_ok() {
            Some((
                super::routing_belongs_to("a-1", -1004451125152, Some(3)),
                super::routing_belongs_to("a-1", -1004451125152, Some(4)),
                super::routing_belongs_to("a-2", -1004451125152, Some(4)),
                super::routing_belongs_to("a-2", -1004451125152, Some(3)),
                super::routing_belongs_to("a-1", -1004451125152, Some(9)),
            ))
        } else {
            None
        };

        std::env::remove_var("CLAUDEBASE_HOME_OVERRIDE");
        match prev {
            Some(v) => std::env::set_var("HOME", v),
            None => std::env::remove_var("HOME"),
        }

        let (own3, other4, own4, other3, unbound) = verdicts.expect("fixture db");
        assert!(own3, "the session bound to topic 3 must own topic 3");
        assert!(own4, "the session bound to topic 4 must own topic 4");
        assert!(!other4, "topic 4 is not transport's, whoever pinged last");
        assert!(!other3, "topic 3 is not cutover's");
        assert!(!unbound, "an unassigned topic belongs to nobody, so withhold");
    }
}

#[cfg(test)]
mod default_nick_tests {
    use super::identity::nick_from_label;
    use crate::daemon::agent_registry::validate_agent_name;

    /// A directory named `tacticstrade.ai` produced the nick `tacticstrade.ai`,
    /// the daemon refused it, and the session never got on the map.
    #[test]
    fn a_directory_name_always_becomes_a_valid_nick() {
        for (label, nick) in [
            ("tacticstrade.ai", "tacticstrade-ai"),
            ("my project", "my-project"),
            ("claudebase", "claudebase"),
            ("..", "claude"),
            ("проект", "claude"),
            ("a..b", "a-b"),
        ] {
            assert_eq!(nick_from_label(label), nick);
            validate_agent_name(&nick_from_label(label)).unwrap();
        }
        assert!(nick_from_label(&"x".repeat(200)).len() <= 60);
    }
}
