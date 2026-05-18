//! claudebase — local knowledge base CLI for SDLC agents.
//!
//! Wires `clap` argument parsing to the per-subcommand runners
//! (`Ingest`, `Search`, `List`, `Status`, `Delete`). The path-canonicalization
//! security backbone in `cli::resolve_project_root` runs BEFORE any subcommand
//! body so every filesystem-touching subcommand receives a canonical project
//! root (Phase 1.5 Security MUST #3 + #4 + #7).
//
// INVARIANTS (Slice 1c — async discipline). Five rules, summarised here;
// the canonical reference is `src/daemon/ASYNC_INVARIANTS.md`. A change
// that breaks any of these is a regression.
//
//   1. `fn main` STAYS synchronous. The tokio runtime is constructed by
//      `crate::daemon::run_tokio()` lazily inside `run_daemon_serve` and
//      `run_plugin_serve` ONLY. Do NOT convert to `#[tokio::main]`.
//
//   2. NEVER `.await` while holding a `std::sync::Mutex` guard. The three
//      blocking mutexes (`PDFIUM` in src/pdf.rs, `ENCODER` in
//      src/encoder.rs, `OCR_ENGINE` in src/ocr.rs) are blocking, NOT
//      tokio mutexes. Holding one across `.await` deadlocks the worker.
//      Wrap the lock-and-use site in `tokio::task::spawn_blocking`:
//
//          let result = tokio::task::spawn_blocking(move || {
//              let guard = PDFIUM.lock().expect("mutex poisoned");
//              do_blocking_work(&guard)
//          }).await?;
//
//      Do NOT use `tokio::task::block_in_place` — it has subtle traps
//      with nested calls and with single-worker runtimes.
//
//   3. `tokio::spawn` MUST be panic-safe. Wrap spawned bodies in
//      `if let Err(e) = ... { tracing::error!(...) }`; never let a panic
//      vanish silently.
//
//   4. `tokio::select!` MUST be cancellation-safe. Only pass branches
//      futures whose docs explicitly say they are cancellation-safe
//      (e.g. `AsyncBufReadExt::read_line`, `tokio::time::sleep`).
//
//   5. NEVER `.unwrap()` on runtime values inside a spawned task body.
//      Propagate via `Result` to the task entry point where a
//      `tracing::error!` logs the failure before the task ends.
//
// Slice 1a/1b/1c daemon code does NOT touch the blocking mutexes. Future
// slices that DO need them MUST follow rule 2 verbatim.

use clap::Parser;

use claudebase::cli::{self, Cli, Command};
use claudebase::{encoder, ingest, migrations, output, pdf, search, store};

fn main() -> std::process::ExitCode {
    let cli = Cli::parse();

    // Resolve project_root for ALL subcommands BEFORE any subcommand-specific work.
    // This is the load-bearing FS-access gate (Phase 1.5 Security MUST #3 + #4 + #7).
    let project_root_arg = match &cli.command {
        Command::Ingest(a) => a.project_root.as_deref(),
        Command::Search(a) => a.project_root.as_deref(),
        Command::List(a) => a.project_root.as_deref(),
        Command::Status(a) => a.project_root.as_deref(),
        Command::Delete(a) => a.project_root.as_deref(),
        // Warmup does not touch project filesystem — encoder cache is in $HOME.
        // resolve_project_root still runs (to keep the path-canonicalization
        // gate uniform for all subcommands) but the resolved root is unused.
        Command::Warmup(_) => None,
        Command::Compare(a) => a.project_root.as_deref(),
        Command::Page(a) => a.project_root.as_deref(),
        Command::ReindexPages(a) => a.project_root.as_deref(),
        Command::Insight(a) => match &a.sub {
            cli::InsightSubcommand::Create(c) => c.project_root.as_deref(),
            cli::InsightSubcommand::Search(s) => s.project_root.as_deref(),
            cli::InsightSubcommand::List(l) => l.project_root.as_deref(),
            cli::InsightSubcommand::Random(r) => r.project_root.as_deref(),
            cli::InsightSubcommand::Get(g) => g.project_root.as_deref(),
            cli::InsightSubcommand::Gc(g) => g.project_root.as_deref(),
            cli::InsightSubcommand::Delete(d) => d.project_root.as_deref(),
        },
        // Daemon / Plugin do NOT operate on a project filesystem — they
        // own a runtime-dir-scoped socket and chat.db under ~/.claude/.
        // The path-canonicalization gate still runs (project_root will
        // resolve to the current cwd) but the resolved root is unused
        // by the daemon/plugin dispatch.
        Command::Daemon(_) => None,
        Command::Plugin(_) => None,
        // Chat reads `~/.claude/knowledge/chat.db` directly — user-level,
        // not per-project — so the project root is unused. The gate still
        // runs to keep the security backbone uniform.
        Command::Chat(_) => None,
    };

    let root = match cli::resolve_project_root(project_root_arg) {
        Ok(p) => p,
        Err(_) => {
            // Uniform error mapping: every canonicalize failure prints the same
            // literal stderr and exits 2 (Phase 1.5 Security MUST #4 + #6).
            eprintln!("error: project-root must resolve under current working directory");
            return std::process::ExitCode::from(2);
        }
    };

    match cli.command {
        Command::Ingest(args) => run_ingest(&root, &args),
        Command::Search(args) => run_search(&root, &args),
        Command::List(args) => run_list(&root, &args),
        Command::Status(args) => run_status(&root, &args),
        Command::Delete(args) => run_delete(&root, &args),
        Command::Warmup(args) => run_warmup(&args),
        Command::Compare(args) => run_compare(&root, &args),
        Command::Page(args) => run_page(&root, &args),
        Command::ReindexPages(args) => run_reindex_pages(&root, &args),
        Command::Insight(args) => match args.sub {
            cli::InsightSubcommand::Create(a) => run_insight_create(&root, &a),
            cli::InsightSubcommand::Search(a) => run_insight_search(&root, &a),
            cli::InsightSubcommand::List(a) => run_insight_list(&root, &a),
            cli::InsightSubcommand::Random(a) => run_insight_random(&root, &a),
            cli::InsightSubcommand::Get(a) => run_insight_get(&root, &a),
            cli::InsightSubcommand::Gc(a) => run_insight_gc(&root, &a),
            cli::InsightSubcommand::Delete(a) => run_insight_delete(&root, &a),
        },
        Command::Daemon(args) => match args.sub {
            cli::DaemonSubcommand::Serve(serve_args) => run_daemon_serve(&serve_args),
            cli::DaemonSubcommand::Config(config_args) => match config_args.sub {
                cli::DaemonConfigSubcommand::Edit(a) => run_daemon_config_edit(&a),
                cli::DaemonConfigSubcommand::Show(a) => run_daemon_config_show(&a),
            },
            cli::DaemonSubcommand::Access(access_args) => match access_args.sub {
                cli::DaemonAccessSubcommand::Pair(a) => run_daemon_access_pair(&a),
                cli::DaemonAccessSubcommand::List(a) => run_daemon_access_list(&a),
            },
            cli::DaemonSubcommand::Doctor(a) => run_daemon_doctor(&a),
            cli::DaemonSubcommand::Warmup(a) => run_daemon_warmup(&a),
            cli::DaemonSubcommand::Install(a) => run_daemon_install(&a),
            cli::DaemonSubcommand::Uninstall(a) => run_daemon_uninstall(&a),
            cli::DaemonSubcommand::Start => run_daemon_start(),
            cli::DaemonSubcommand::Stop => run_daemon_stop(),
            cli::DaemonSubcommand::Restart => run_daemon_restart(),
            cli::DaemonSubcommand::Status(a) => run_daemon_status(&a),
            cli::DaemonSubcommand::Logs(a) => run_daemon_logs(&a),
        },
        Command::Plugin(args) => match &args.sub {
            cli::PluginSubcommand::Serve(serve_args) => run_plugin_serve(serve_args),
        },
        Command::Chat(args) => match &args.sub {
            cli::ChatSubcommand::List(a) => run_chat_list(a),
            cli::ChatSubcommand::Threads(a) => run_chat_threads(a),
        },
    }
}

/// `claudebase chat list --thread X` — Slice 3 CLI introspection.
/// Reads chat.db directly without contacting the daemon. The schema is
/// ensured on open so a fresh box without a daemon ever running still
/// works (prints empty when no rows exist).
fn run_chat_list(args: &cli::ChatListArgs) -> std::process::ExitCode {
    use claudebase::daemon::chat;
    let path = claudebase::store::user_level_chat_db_path();
    if let Some(parent) = path.parent() {
        if let Err(e) = std::fs::create_dir_all(parent) {
            eprintln!("error: failed to create chat.db parent: {e}");
            return std::process::ExitCode::FAILURE;
        }
    }
    let conn = match rusqlite::Connection::open_with_flags(
        &path,
        rusqlite::OpenFlags::SQLITE_OPEN_CREATE | rusqlite::OpenFlags::SQLITE_OPEN_READ_WRITE,
    ) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("error: open chat.db: {e}");
            return std::process::ExitCode::FAILURE;
        }
    };
    if let Err(e) = chat::ensure_chat_db_schema(&conn) {
        eprintln!("error: chat schema: {e}");
        return std::process::ExitCode::FAILURE;
    }
    let messages = match chat::list_messages(&conn, &args.thread, args.since, args.limit) {
        Ok(m) => m,
        Err(e) => {
            eprintln!("error: list messages: {e}");
            return std::process::ExitCode::FAILURE;
        }
    };
    if args.json {
        let payload = serde_json::json!({
            "messages": messages.iter().map(|m| m.to_json()).collect::<Vec<_>>(),
        });
        println!("{}", payload);
    } else {
        for m in &messages {
            println!(
                "[{}] {} <{}> {}",
                m.created_at, m.id, m.from_agent, m.content
            );
        }
    }
    std::process::ExitCode::SUCCESS
}

/// `claudebase chat threads` — list all known chat threads.
fn run_chat_threads(args: &cli::ChatThreadsArgs) -> std::process::ExitCode {
    use claudebase::daemon::chat;
    let path = claudebase::store::user_level_chat_db_path();
    if let Some(parent) = path.parent() {
        if let Err(e) = std::fs::create_dir_all(parent) {
            eprintln!("error: failed to create chat.db parent: {e}");
            return std::process::ExitCode::FAILURE;
        }
    }
    let conn = match rusqlite::Connection::open_with_flags(
        &path,
        rusqlite::OpenFlags::SQLITE_OPEN_CREATE | rusqlite::OpenFlags::SQLITE_OPEN_READ_WRITE,
    ) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("error: open chat.db: {e}");
            return std::process::ExitCode::FAILURE;
        }
    };
    if let Err(e) = chat::ensure_chat_db_schema(&conn) {
        eprintln!("error: chat schema: {e}");
        return std::process::ExitCode::FAILURE;
    }
    let threads = match chat::list_threads(&conn) {
        Ok(t) => t,
        Err(e) => {
            eprintln!("error: list threads: {e}");
            return std::process::ExitCode::FAILURE;
        }
    };
    if args.json {
        let entries: Vec<_> = threads
            .iter()
            .map(|t| {
                serde_json::json!({
                    "id": t.id,
                    "message_count": t.message_count,
                    "last_created_at": t.last_created_at,
                })
            })
            .collect();
        println!("{}", serde_json::json!({ "threads": entries }));
    } else {
        for t in &threads {
            let last = t
                .last_created_at
                .map(|n| n.to_string())
                .unwrap_or_else(|| "-".to_string());
            println!("{}\t{}\t{}", t.id, t.message_count, last);
        }
    }
    std::process::ExitCode::SUCCESS
}

/// Initialise the structured-logging subscriber for daemon/plugin arms.
///
/// fmt::json() emits one JSON-encoded line per event to stderr. The
/// env-filter honours `RUST_LOG` with a sensible info-level default.
/// `try_init()` is idempotent and is the supported entry point — a
/// double-install (e.g. plugin spawning a daemon in-process during
/// tests) becomes a silent no-op rather than a panic.
fn init_tracing() {
    use tracing_subscriber::{fmt, prelude::*, EnvFilter};

    let filter = EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| EnvFilter::new("info"));

    let _ = tracing_subscriber::registry()
        .with(filter)
        .with(fmt::layer().json().with_writer(std::io::stderr))
        .try_init();
}

/// `claudebase daemon serve` — entry point. Constructs the tokio
/// runtime (the ONLY tokio entry in the binary; sync subcommands
/// remain on the sync dispatch path) and runs the accept loop until
/// fatal error or graceful shutdown.
fn run_daemon_serve(args: &cli::DaemonServeArgs) -> std::process::ExitCode {
    // Slice 4 SEC-9 / SEC-15 FAST EXIT — perform the secrets.toml +
    // daemon.toml validation BEFORE init_tracing and run_tokio. The
    // tokio multi-thread runtime build is the slowest step in startup
    // (~200ms+ on macOS); the test harness's TC-4.14 only allows 1
    // second of wall-clock before checking try_wait(). By moving the
    // check ahead of runtime build the bad-perm/symlink/forbidden-field
    // exit lands well inside the budget on every supported OS.
    use claudebase::daemon::config;
    let secrets_path = config::user_level_secrets_toml_path();
    match std::fs::symlink_metadata(&secrets_path) {
        Ok(_) => {
            if let Err(e) = config::load_secrets_toml(&secrets_path) {
                eprintln!("error: {e}");
                return std::process::ExitCode::FAILURE;
            }
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            // No secrets.toml — daemon runs chat-only. No-op here; the
            // long-poll spawn in server::serve() also short-circuits.
        }
        Err(e) => {
            eprintln!("error: failed to stat secrets.toml: {e}");
            return std::process::ExitCode::FAILURE;
        }
    }
    let daemon_toml_path = config::user_level_daemon_toml_path();
    if daemon_toml_path.exists() {
        if let Err(e) = config::load_daemon_toml(&daemon_toml_path) {
            eprintln!("error: {e}");
            return std::process::ExitCode::FAILURE;
        }
    }

    init_tracing();
    match claudebase::daemon::run_tokio(claudebase::daemon::server::serve(args)) {
        Ok(()) => std::process::ExitCode::SUCCESS,
        Err(e) => {
            // Subscriber is installed; route the fatal cause through
            // tracing so the JSON line carries the standard fields. The
            // anyhow chain is appended with the alternate formatter.
            tracing::error!(error = %format!("{e:#}"), "claudebase daemon fatal");
            std::process::ExitCode::FAILURE
        }
    }
}

/// `claudebase plugin serve` — entry point. Slice 1a stub.
fn run_plugin_serve(args: &cli::PluginServeArgs) -> std::process::ExitCode {
    init_tracing();
    match claudebase::daemon::run_tokio(claudebase::plugin::serve(args)) {
        Ok(()) => std::process::ExitCode::SUCCESS,
        Err(e) => {
            tracing::error!(error = %format!("{e:#}"), "claudebase plugin fatal");
            std::process::ExitCode::FAILURE
        }
    }
}

// ---------------------------------------------------------------------------
// Slice 4 — daemon config + daemon access subcommand runners.
// ---------------------------------------------------------------------------

/// `claudebase daemon config edit` (Slice 4).
///
/// 1. Resolve daemon.toml path.
/// 2. Symlink refuse (SEC-15).
/// 3. Open `$EDITOR` (defaults to `vi`) via arg-vector form (SEC-16).
/// 4. Re-parse on editor exit. Refuse and exit 1 if parse fails. The
///    file is NOT reverted — the operator can re-edit. This mirrors what
///    `visudo` does on a broken sudoers file: keep the broken content
///    visible so the operator can see what they typed.
fn run_daemon_config_edit(_args: &cli::DaemonConfigEditArgs) -> std::process::ExitCode {
    use claudebase::daemon::config;

    let path = config::user_level_daemon_toml_path();

    // Ensure the parent dir + file exist so the editor has something to
    // open. Missing daemon.toml is NOT an error — create an empty one
    // and let the operator fill it in.
    if let Some(parent) = path.parent() {
        if let Err(e) = std::fs::create_dir_all(parent) {
            eprintln!("error: failed to create config dir: {e}");
            return std::process::ExitCode::FAILURE;
        }
    }
    if !path.exists() {
        if let Err(e) = std::fs::write(&path, "") {
            eprintln!("error: failed to create daemon.toml: {e}");
            return std::process::ExitCode::FAILURE;
        }
    }

    // SEC-15 step 1: symlink refuse via lstat (pre-edit).
    match std::fs::symlink_metadata(&path) {
        Ok(meta) if meta.file_type().is_symlink() => {
            eprintln!("error: refuse to read symlink: {}", path.display());
            return std::process::ExitCode::FAILURE;
        }
        Ok(_) => {}
        Err(e) => {
            eprintln!("error: failed to stat daemon.toml: {e}");
            return std::process::ExitCode::FAILURE;
        }
    }

    let editor = std::env::var("EDITOR").unwrap_or_else(|_| "vi".to_string());

    // SEC-16: arg-vector form. NEVER `sh -c "$editor $path"`.
    let status = std::process::Command::new(&editor).arg(&path).status();
    let status = match status {
        Ok(s) => s,
        Err(e) => {
            eprintln!("error: failed to spawn editor `{editor}`: {e}");
            return std::process::ExitCode::FAILURE;
        }
    };
    if !status.success() {
        eprintln!("error: editor `{editor}` exited non-zero: {status}");
        return std::process::ExitCode::FAILURE;
    }

    // Re-parse to catch malformed TOML / forbidden bot_token field.
    match config::load_daemon_toml(&path) {
        Ok(_) => std::process::ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("error: TOML parse failed after edit: {e}");
            std::process::ExitCode::FAILURE
        }
    }
}

/// `claudebase daemon config show` (Slice 4).
///
/// Loads daemon.toml + secrets.toml; merges into a single view; prints
/// either TOML (default) or JSON (`--json`). The bot token is ALWAYS
/// masked to `"***"` (SEC-10 — `RedactedToken::Display` + serde impls).
fn run_daemon_config_show(args: &cli::DaemonConfigShowArgs) -> std::process::ExitCode {
    use claudebase::daemon::config;

    let daemon_path = config::user_level_daemon_toml_path();
    let secrets_path = config::user_level_secrets_toml_path();

    // daemon.toml is mandatory once it exists; if it doesn't exist, print
    // a minimal default and return 0 (so config show works on a fresh box).
    let daemon_cfg = if daemon_path.exists() {
        match config::load_daemon_toml(&daemon_path) {
            Ok(c) => c,
            Err(e) => {
                eprintln!("error: failed to load daemon.toml: {e}");
                return std::process::ExitCode::FAILURE;
            }
        }
    } else {
        config::Config::default()
    };

    let token_mask = "***"; // SEC-10 literal mask required by TC-4.12.
    let token_present = match config::load_secrets_toml(&secrets_path) {
        Ok(_) => true,
        Err(e) => {
            // We tolerate missing secrets.toml here — `config show`
            // should still work for fresh installs without bot tokens.
            // BUT if the file exists and FAILED to parse / is symlinked
            // / has wrong perms, that's a real error: surface it.
            if secrets_path.exists() {
                eprintln!("error: failed to load secrets.toml: {e}");
                return std::process::ExitCode::FAILURE;
            }
            false
        }
    };

    if args.json {
        let mut payload = serde_json::json!({
            "asr": {
                "backend": daemon_cfg.asr.backend,
            },
            "telegram": {
                "enabled": daemon_cfg.telegram.enabled,
                "poll_interval_secs": daemon_cfg.telegram.poll_interval_secs,
                "dmPolicy": daemon_cfg.telegram.dm_policy,
            },
            "daemon": {
                "log_level": daemon_cfg.daemon.log_level,
                "port": daemon_cfg.daemon.port,
            },
        });
        if token_present {
            payload["telegram"]["bot_token"] = serde_json::json!(token_mask);
        }
        println!("{}", payload);
    } else {
        // Plain TOML view. We hand-format so the bot_token mask appears
        // even when the secrets.toml file would normally be a different
        // file. The format is intentionally simple — operators read this
        // to confirm configuration.
        println!("[asr]");
        if let Some(b) = &daemon_cfg.asr.backend {
            println!("backend = \"{b}\"");
        }
        println!();
        println!("[telegram]");
        println!("enabled = {}", daemon_cfg.telegram.enabled);
        println!("poll_interval_secs = {}", daemon_cfg.telegram.poll_interval_secs);
        let policy_str = match daemon_cfg.telegram.dm_policy {
            config::DmPolicy::Pairing => "pairing",
            config::DmPolicy::Allowlist => "allowlist",
            config::DmPolicy::Disabled => "disabled",
        };
        println!("dmPolicy = \"{policy_str}\"");
        if token_present {
            println!("bot_token = \"{token_mask}\"");
        }
        println!();
        println!("[daemon]");
        if let Some(ll) = &daemon_cfg.daemon.log_level {
            println!("log_level = \"{ll}\"");
        }
        if let Some(p) = daemon_cfg.daemon.port {
            println!("port = {p}");
        }
    }

    std::process::ExitCode::SUCCESS
}

/// `claudebase daemon access pair <code>` (Slice 4).
///
/// Loads access.json, redeems the code via constant-time compare
/// (SEC-16), saves atomically (SEC-12). The error message for unknown /
/// invalid format is generic — the operator does NOT learn which check
/// failed, defeating timing + content side-channels.
fn run_daemon_access_pair(args: &cli::DaemonAccessPairArgs) -> std::process::ExitCode {
    use claudebase::daemon::permissions;

    let path = permissions::user_level_access_json_path();
    let mut access = match permissions::load_access(&path) {
        Ok(a) => a,
        Err(e) => {
            eprintln!("error: failed to load access.json: {e}");
            return std::process::ExitCode::FAILURE;
        }
    };

    let now = permissions::now_ms();
    match permissions::redeem_pairing_code(&mut access, &args.code, now) {
        Ok(user_id) => {
            if let Err(e) = permissions::save_access(&path, &access) {
                eprintln!("error: failed to save access.json: {e}");
                return std::process::ExitCode::FAILURE;
            }
            println!("paired user_id={user_id}");
            std::process::ExitCode::SUCCESS
        }
        Err(e) => {
            // Display impl returns the same string for `Unknown` and
            // `InvalidFormat` (SEC-16). `Expired` surfaces distinctly so
            // operators can re-issue a code.
            eprintln!("error: {e}");
            std::process::ExitCode::FAILURE
        }
    }
}

/// `claudebase daemon access list` (Slice 4).
///
/// Prints authorized users + pending-code count. Pending codes themselves
/// are NEVER printed (SEC-16 — leaking active codes defeats the
/// constant-time-pair flow). `allowFrom` user ids are shown verbatim.
fn run_daemon_access_list(args: &cli::DaemonAccessListArgs) -> std::process::ExitCode {
    use claudebase::daemon::permissions;

    let path = permissions::user_level_access_json_path();
    let access = match permissions::load_access(&path) {
        Ok(a) => a,
        Err(e) => {
            eprintln!("error: failed to load access.json: {e}");
            return std::process::ExitCode::FAILURE;
        }
    };

    let summary = permissions::redacted_summary(&access);
    if args.json {
        println!("{}", summary);
    } else {
        let policy = summary.get("dmPolicy").map(|v| v.to_string()).unwrap_or_default();
        let allow: Vec<i64> = summary
            .get("allowFrom")
            .and_then(|v| v.as_array())
            .map(|arr| arr.iter().filter_map(|v| v.as_i64()).collect())
            .unwrap_or_default();
        let pending_count = summary.get("pending_count").and_then(|v| v.as_i64()).unwrap_or(0);
        println!("dmPolicy = {policy}");
        print!("allowFrom = [");
        for (i, u) in allow.iter().enumerate() {
            if i > 0 {
                print!(", ");
            }
            print!("{u}");
        }
        println!("]");
        println!("pending = {pending_count}");
    }
    std::process::ExitCode::SUCCESS
}

// ---------------------------------------------------------------------------
// Slice 6-MVP — `daemon doctor` + `daemon warmup` runners.
//
// Both stay synchronous. They load daemon.toml, build the Asr factory,
// and exercise either the health_check() surface (doctor) or the
// model-download path (warmup). Neither requires the tokio runtime.
// ---------------------------------------------------------------------------

/// `claudebase daemon doctor [--asr]` — health-check the ASR backend
/// (Slice 6-MVP scope; other backends added later). Exit 0 = healthy,
/// 1 = unhealthy. The full stderr / stdout shape is consumed by
/// `tests/daemon_doctor_test.rs` and TC-6.16 / TC-6.17.
fn run_daemon_doctor(_args: &cli::DaemonDoctorArgs) -> std::process::ExitCode {
    use claudebase::daemon::asr;
    use claudebase::daemon::config;

    // Load daemon.toml. Missing file → exit 1 with a clear message.
    let toml_path = config::user_level_daemon_toml_path();
    let cfg = if toml_path.exists() {
        match config::load_daemon_toml(&toml_path) {
            Ok(c) => c,
            Err(e) => {
                eprintln!("error: failed to load daemon.toml: {e}");
                return std::process::ExitCode::FAILURE;
            }
        }
    } else {
        eprintln!(
            "error: daemon.toml not found at {} — run `claudebase daemon config edit` first",
            toml_path.display()
        );
        return std::process::ExitCode::FAILURE;
    };

    let backend_name = cfg.asr.backend.as_deref().unwrap_or("<none>");
    match asr::make_asr(&cfg) {
        Ok(handle) => match handle.health_check() {
            Ok(()) => {
                println!("{backend_name} — OK (model loaded successfully)");
                std::process::ExitCode::SUCCESS
            }
            Err(e) => {
                eprintln!("{backend_name}: ERROR — {e}");
                std::process::ExitCode::FAILURE
            }
        },
        Err(e) => {
            eprintln!("{backend_name}: ERROR — {e}");
            std::process::ExitCode::FAILURE
        }
    }
}

/// `claudebase daemon warmup [--asr]` — pre-fetch the configured ASR
/// model so the first voice note doesn't pay the cold-start download
/// stall (PRD FR-ACD-7.9). Exit 0 on success, 1 on failure.
fn run_daemon_warmup(_args: &cli::DaemonWarmupArgs) -> std::process::ExitCode {
    use claudebase::daemon::config;

    let toml_path = config::user_level_daemon_toml_path();
    let cfg = if toml_path.exists() {
        match config::load_daemon_toml(&toml_path) {
            Ok(c) => c,
            Err(e) => {
                eprintln!("error: failed to load daemon.toml: {e}");
                return std::process::ExitCode::FAILURE;
            }
        }
    } else {
        eprintln!(
            "error: daemon.toml not found at {} — run `claudebase daemon config edit` first",
            toml_path.display()
        );
        return std::process::ExitCode::FAILURE;
    };

    let backend_name = cfg.asr.backend.as_deref().unwrap_or("<none>");
    if backend_name != "whisper" {
        // Slice 6-MVP: only whisper has a warmup path. sherpa/nim
        // would have their own model file conventions; we surface
        // an informative exit 1 so the operator knows there's no-op.
        eprintln!(
            "warmup: backend '{backend_name}' has no warmup path in v1 — see Wave 6"
        );
        return std::process::ExitCode::FAILURE;
    }

    #[cfg(feature = "asr-whisper")]
    {
        use claudebase::daemon::asr::whisper;
        let path = match whisper::model_path() {
            Ok(p) => p,
            Err(e) => {
                eprintln!("warmup: cannot resolve model path: {e}");
                return std::process::ExitCode::FAILURE;
            }
        };
        match whisper::ensure_model(&path) {
            Ok(()) => {
                println!("whisper: model present at {}", path.display());
                std::process::ExitCode::SUCCESS
            }
            Err(e) => {
                eprintln!("warmup: whisper model download failed: {e}");
                std::process::ExitCode::FAILURE
            }
        }
    }
    #[cfg(not(feature = "asr-whisper"))]
    {
        eprintln!(
            "warmup: asr-whisper feature not compiled in — rebuild with `cargo build --features asr-whisper`"
        );
        std::process::ExitCode::FAILURE
    }
}

// ---------------------------------------------------------------------------
// Slice 2 — daemon service-installer runners. All synchronous (no tokio).
// ---------------------------------------------------------------------------

fn run_daemon_install(args: &cli::DaemonInstallArgs) -> std::process::ExitCode {
    use claudebase::daemon::service;
    let install_args = service::InstallArgs {
        yes: args.yes,
        no_start: args.no_start,
    };
    match service::install(&install_args) {
        Ok(()) => std::process::ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("error: {e}");
            std::process::ExitCode::FAILURE
        }
    }
}

fn run_daemon_uninstall(args: &cli::DaemonUninstallArgs) -> std::process::ExitCode {
    use claudebase::daemon::service;
    let u = service::UninstallArgs {
        yes: args.yes,
        keep_data: args.keep_data,
    };
    match service::uninstall(&u) {
        Ok(()) => std::process::ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("error: {e}");
            std::process::ExitCode::FAILURE
        }
    }
}

fn run_daemon_start() -> std::process::ExitCode {
    use claudebase::daemon::service;
    match service::start() {
        Ok(()) => std::process::ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("error: {e}");
            std::process::ExitCode::FAILURE
        }
    }
}

fn run_daemon_stop() -> std::process::ExitCode {
    use claudebase::daemon::service;
    match service::stop() {
        Ok(()) => std::process::ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("error: {e}");
            std::process::ExitCode::FAILURE
        }
    }
}

fn run_daemon_restart() -> std::process::ExitCode {
    use claudebase::daemon::service;
    match service::restart() {
        Ok(()) => std::process::ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("error: {e}");
            std::process::ExitCode::FAILURE
        }
    }
}

fn run_daemon_status(args: &cli::DaemonStatusArgs) -> std::process::ExitCode {
    use claudebase::daemon::service;
    match service::status() {
        Ok(s) => {
            if args.json {
                println!("{}", serde_json::to_string(&s).unwrap_or_else(|_| "{}".to_string()));
            } else {
                println!("state: {}", s.state);
                if let Some(pid) = s.pid {
                    println!("pid: {pid}");
                }
            }
            std::process::ExitCode::SUCCESS
        }
        Err(e) => {
            eprintln!("error: {e}");
            std::process::ExitCode::FAILURE
        }
    }
}

fn run_daemon_logs(args: &cli::DaemonLogsArgs) -> std::process::ExitCode {
    use claudebase::daemon::service;
    match service::logs(args.lines, args.follow) {
        Ok(()) => std::process::ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("error: {e}");
            std::process::ExitCode::FAILURE
        }
    }
}

/// `insight create "<body>"` — agent write surface for the insights corpus
/// (schema v4).
///
/// Reads the insight body from the positional arg or stdin (TTY refused),
/// runs the exact-sha dedup probe (`agent_name`+sha256 within last 30 days),
/// chunks the body via the canonical 500/100 sliding window, and writes
/// via `store::upsert_insight_document` + `store::replace_chunks`. Encoder
/// population into `chunks_vec` is best-effort — silent no-op when the e5
/// model is missing, matching the ingest path's degraded-mode behavior.
fn run_insight_create(
    root: &std::path::Path,
    args: &cli::InsightCreateArgs,
) -> std::process::ExitCode {
    use std::io::Read;
    use std::time::{SystemTime, UNIX_EPOCH};

    // 1) Resolve body — positional literal, `-`, or piped stdin.
    let body_string = match args.body.as_deref() {
        Some("-") | None => {
            // Refuse to block on an interactive TTY — guard against
            // accidental invocation from a human shell. Agents always
            // pipe stdin, so the non-TTY path is the load-bearing one.
            if std::io::IsTerminal::is_terminal(&std::io::stdin()) {
                eprintln!(
                    "error: body required (positional `<body>` or pipe input to stdin); refusing to block on TTY"
                );
                return std::process::ExitCode::from(2);
            }
            let mut buf = String::new();
            if let Err(e) = std::io::stdin().read_to_string(&mut buf) {
                eprintln!("error: failed to read stdin: {e}");
                return std::process::ExitCode::from(1);
            }
            buf
        }
        Some(literal) => literal.to_string(),
    };
    let body = body_string.trim();
    if body.is_empty() {
        eprintln!("error: insight body is empty");
        return std::process::ExitCode::from(2);
    }

    // 2) Validate the args that aren't typed at the parser level.
    if args.kind.trim().is_empty() {
        eprintln!("error: --type must not be empty");
        return std::process::ExitCode::from(2);
    }
    if args.agent.trim().is_empty() {
        eprintln!("error: --agent must not be empty");
        return std::process::ExitCode::from(2);
    }

    // 3) Compute sha256(body) for dedup + synthesize the source_path.
    //
    // source_path shape: `agent:{agent}:{session}:{feature}:{sha[..16]}`.
    // The `agent:` prefix keeps insight rows lexically distinct from real
    // file paths in the same documents table on shared corpora. Missing
    // session / feature collapse to `-` so the source_path remains
    // valid (UNIQUE doesn't permit NULL components in a literal string).
    let sha_full = {
        use sha2::{Digest, Sha256};
        let mut h = Sha256::new();
        h.update(body.as_bytes());
        let d = h.finalize();
        let mut s = String::with_capacity(64);
        for b in d {
            use std::fmt::Write;
            let _ = write!(s, "{b:02x}");
        }
        s
    };
    let sha_short = &sha_full[..16];
    let session_token = args.session.as_deref().unwrap_or("-");
    let feature_token = args.feature.as_deref().unwrap_or("-");
    let source_path = format!(
        "agent:{}:{}:{}:{}",
        args.agent.trim(),
        session_token,
        feature_token,
        sha_short
    );
    let now: i64 = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);

    // 4) Open insights.db (default — caller may override via --db-name).
    let (mut conn, _db_path) = match open_and_validate(root, &args.db_name) {
        Ok(t) => t,
        Err(code) => return code,
    };

    // 5a) Exact-sha dedup probe — same agent, same sha, within last 30d.
    const DEDUP_WINDOW_SECS: i64 = 30 * 86400;
    let cutoff = now - DEDUP_WINDOW_SECS;
    match store::find_recent_insight_by_sha(&conn, &sha_full, args.agent.trim(), cutoff) {
        Ok(Some(existing_id)) => {
            if args.json {
                let payload = serde_json::json!({
                    "status":      "deduped",
                    "doc_id":      existing_id,
                    "source_path": source_path,
                    "sha256":      sha_full,
                    "agent":       args.agent,
                    "type":        args.kind,
                });
                println!(
                    "{}",
                    serde_json::to_string_pretty(&payload).unwrap_or_default()
                );
            } else {
                println!(
                    "deduped: existing doc id {existing_id} (sha={} agent={})",
                    &sha_full[..12],
                    args.agent
                );
            }
            return std::process::ExitCode::SUCCESS;
        }
        Ok(None) => {} // proceed to semantic-dedup probe
        Err(e) => {
            eprintln!("error: dedup probe failed: {e}");
            return std::process::ExitCode::from(1);
        }
    }

    // 5b) Semantic-dedup probe — encode the body and K-NN against
    // chunks_vec to find near-duplicates from the same agent within the
    // same 30-day window. Cosine threshold 0.92 maps to L2 distance ~0.4
    // for L2-normalized e5 vectors (cosine = 1 − L2² / 2). Best-effort:
    // silently no-ops when the encoder is unavailable OR chunks_vec is
    // empty (degraded mode parity with the chunks_vec population path).
    match find_semantic_duplicate(&conn, body, args.agent.trim(), cutoff) {
        Some(existing_id) => {
            if args.json {
                let payload = serde_json::json!({
                    "status":      "near-duplicate",
                    "doc_id":      existing_id,
                    "source_path": source_path,
                    "sha256":      sha_full,
                    "agent":       args.agent,
                    "type":        args.kind,
                });
                println!(
                    "{}",
                    serde_json::to_string_pretty(&payload).unwrap_or_default()
                );
            } else {
                println!(
                    "near-duplicate: existing doc id {existing_id} (semantic match, agent={})",
                    args.agent
                );
            }
            return std::process::ExitCode::SUCCESS;
        }
        None => {} // no near-duplicate found, proceed to write
    }

    // 6) Chunk the body — flat 500/100 sliding window. Insights have no
    // page provenance so page_start / page_end stay NULL.
    let chunks = ingest::chunk(body);
    if chunks.is_empty() {
        // chunk() returns empty only when the body has zero chars; the
        // earlier emptiness check covers this, but the gate is cheap.
        eprintln!("error: body produced zero chunks (empty after normalization)");
        return std::process::ExitCode::from(2);
    }

    // 7) Transactional write: upsert document + replace chunks atomically.
    let salience_str = args.salience.as_str();
    let session_opt = args.session.as_deref();
    let feature_opt = args.feature.as_deref();
    let parent_opt = args.source_artifact.as_deref();
    let doc_id = {
        let tx = match conn
            .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
        {
            Ok(t) => t,
            Err(e) => {
                eprintln!("error: failed to begin transaction: {e}");
                return std::process::ExitCode::from(1);
            }
        };
        let id = match store::upsert_insight_document(
            &tx,
            &source_path,
            now,
            &sha_full,
            now,
            args.kind.trim(),
            args.agent.trim(),
            session_opt,
            feature_opt,
            salience_str,
            parent_opt,
        ) {
            Ok(id) => id,
            Err(e) => {
                eprintln!("error: failed to upsert insight document: {e}");
                return std::process::ExitCode::from(1);
            }
        };
        let chunk_refs: Vec<(usize, &str, Option<i64>, Option<i64>)> = chunks
            .iter()
            .map(|c| (c.ord, c.text.as_str(), c.page_start, c.page_end))
            .collect();
        if let Err(e) = store::replace_chunks(&tx, id, &chunk_refs) {
            eprintln!("error: failed to write chunks: {e}");
            return std::process::ExitCode::from(1);
        }
        if let Err(e) = tx.commit() {
            eprintln!("error: commit failed: {e}");
            return std::process::ExitCode::from(1);
        }
        id
    };

    // 8) Best-effort dense vector write — silent on encoder failure so a
    // freshly-installed environment without the e5 model still records
    // the insight (BM25-only retrieval still works for `recall`).
    let _ = try_populate_insight_chunks_vec(&mut conn, doc_id, &chunks);

    // 9) Emit outcome.
    if args.json {
        let payload = serde_json::json!({
            "status":      "written",
            "doc_id":      doc_id,
            "source_path": source_path,
            "sha256":      sha_full,
            "chunks":      chunks.len(),
            "agent":       args.agent,
            "type":        args.kind,
            "salience":    salience_str,
        });
        println!(
            "{}",
            serde_json::to_string_pretty(&payload).unwrap_or_default()
        );
    } else {
        println!(
            "remembered: doc_id={doc_id} chunks={} sha={} agent={} type={} salience={}",
            chunks.len(),
            &sha_full[..12],
            args.agent,
            args.kind,
            salience_str,
        );
    }
    std::process::ExitCode::SUCCESS
}

/// `insight search "<query>"` — hybrid retrieval against the insights
/// corpus. Reuses the existing search dispatch (lexical / dense / hybrid +
/// auto-fallback) but pins `--db-name insights.db` so books-corpus rows
/// never bleed in. Default mode is `hybrid` (BM25 ⊕ dense via RRF k=60).
///
/// Slice 4 metadata filters are applied AFTER ranking. The search engine
/// is corpus-agnostic, so we over-fetch by ×4 (capped at 100) and then
/// drop hits whose document doesn't match the filter set. The metadata
/// lookups are cached per `doc_id` so multi-chunk hits from the same
/// document share a single SQL query.
fn run_insight_search(
    root: &std::path::Path,
    args: &cli::InsightSearchArgs,
) -> std::process::ExitCode {
    let user_top_k = args.top_k.max(1) as u32;
    let has_filters = args.kind.is_some()
        || args.agent.is_some()
        || args.salience.is_some()
        || args.feature.is_some()
        || args.since.is_some();
    // Over-fetch only when filters are present — otherwise the behavior is
    // byte-identical to pre-Slice-4 (user_top_k passed straight through).
    let fetch_top_k = if has_filters {
        user_top_k.saturating_mul(4).min(search::MAX_TOP_K)
    } else {
        user_top_k
    };
    let context_radius = args.context as u32;
    let (conn, _db_path) = match open_and_validate(root, &args.db_name) {
        Ok(t) => t,
        Err(code) => return code,
    };

    // Parse --since up-front so a bad value exits 2 before opening the DB
    // wastes time on a doomed search.
    let since_cutoff: Option<i64> = match args.since.as_deref() {
        Some(s) => match cli::parse_since(s) {
            Ok(seconds) => {
                use std::time::{SystemTime, UNIX_EPOCH};
                let now = SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .map(|d| d.as_secs() as i64)
                    .unwrap_or(0);
                Some(now - seconds)
            }
            Err(msg) => {
                eprintln!("error: {msg}");
                return std::process::ExitCode::from(2);
            }
        },
        None => None,
    };
    let hits_result = match args.mode {
        cli::SearchMode::Lexical => search::search(&conn, &args.query, fetch_top_k, context_radius),
        cli::SearchMode::Dense | cli::SearchMode::Hybrid => {
            match encoder::encode_query(&args.query) {
                Ok(emb) => match args.mode {
                    cli::SearchMode::Dense => search::dense_search(&conn, &emb, fetch_top_k),
                    cli::SearchMode::Hybrid => {
                        search::hybrid_search(&conn, &args.query, &emb, fetch_top_k)
                    }
                    cli::SearchMode::Lexical => unreachable!(),
                },
                Err(e) => {
                    eprintln!(
                        "warning: encoder unavailable ({e}); falling back to lexical mode. Run `bash install.sh --yes` to install the e5-multilingual-small model."
                    );
                    search::search(&conn, &args.query, fetch_top_k, context_radius)
                }
            }
        }
    };
    // Vector-search failures fall back to lexical with a stderr warning —
    // same UX as the standalone `search` subcommand.
    let raw_hits = match hits_result {
        Ok(h) => h,
        Err(search::SearchError::FtsSyntax(msg)) => {
            eprintln!("error: invalid search query: {msg}");
            return std::process::ExitCode::from(1);
        }
        Err(search::SearchError::Db(e)) => {
            eprintln!(
                "warning: vector search failed ({e}); falling back to lexical mode."
            );
            match search::search(&conn, &args.query, fetch_top_k, context_radius) {
                Ok(h) => h,
                Err(e2) => {
                    eprintln!("error: search failed: {e2}");
                    return std::process::ExitCode::from(1);
                }
            }
        }
    };

    // Post-filter via per-doc_id metadata lookup with a tiny cache.
    let hits = if has_filters {
        filter_insight_hits(
            &conn,
            raw_hits,
            args.kind.as_deref(),
            args.agent.as_deref(),
            args.salience.as_ref().map(|s| s.as_str()),
            args.feature.as_deref(),
            since_cutoff,
            user_top_k as usize,
        )
    } else {
        raw_hits
    };

    if args.json {
        println!("{}", output::render_search_json(&hits));
    } else {
        print!("{}", output::render_search_human(&hits));
    }
    std::process::ExitCode::SUCCESS
}

/// Post-filter ranked hits against the v4 insight-metadata columns. Caches
/// `DocMetadata` lookups per `doc_id` so repeated hits from the same
/// document only hit SQLite once.
fn filter_insight_hits(
    conn: &rusqlite::Connection,
    hits: Vec<search::SearchHit>,
    kind: Option<&str>,
    agent: Option<&str>,
    salience: Option<&str>,
    feature: Option<&str>,
    since_cutoff: Option<i64>,
    user_top_k: usize,
) -> Vec<search::SearchHit> {
    let mut cache: std::collections::HashMap<i64, Option<store::DocMetadata>> =
        std::collections::HashMap::new();
    let mut out = Vec::with_capacity(user_top_k);
    for hit in hits {
        if out.len() >= user_top_k {
            break;
        }
        let meta = cache
            .entry(hit.doc_id)
            .or_insert_with(|| store::get_doc_metadata(conn, hit.doc_id).ok().flatten());
        let Some(m) = meta.as_ref() else {
            continue;
        };
        if let Some(k) = kind {
            if m.source_type.as_deref() != Some(k) {
                continue;
            }
        }
        if let Some(a) = agent {
            if m.agent_name.as_deref() != Some(a) {
                continue;
            }
        }
        if let Some(s) = salience {
            if m.salience.as_deref() != Some(s) {
                continue;
            }
        }
        if let Some(f) = feature {
            if m.feature_slug.as_deref() != Some(f) {
                continue;
            }
        }
        if let Some(cutoff) = since_cutoff {
            if m.ingested_at < cutoff {
                continue;
            }
        }
        out.push(hit);
    }
    out
}

/// `insight list [--offset N] [--page-size N] [filters]` — paginated
/// metadata-summary list of insights, newest-first. Default page size 10
/// matches the spec; `--offset 0` returns the latest page.
fn run_insight_list(
    root: &std::path::Path,
    args: &cli::InsightListArgs,
) -> std::process::ExitCode {
    let (conn, _db_path) = match open_and_validate(root, &args.db_name) {
        Ok(t) => t,
        Err(code) => return code,
    };
    let page_size = args.page_size.clamp(1, 100) as i64;
    let offset_rows = (args.offset as i64).saturating_mul(page_size);
    let kind = args.kind.as_deref();
    let agent = args.agent.as_deref();
    let salience = args.salience.as_ref().map(|s| s.as_str());
    let feature = args.feature.as_deref();
    let total = match store::count_insights(&conn, kind, agent, salience, feature) {
        Ok(n) => n,
        Err(e) => {
            eprintln!("error: count failed: {e}");
            return std::process::ExitCode::from(1);
        }
    };
    let rows = match store::list_insights(
        &conn,
        kind,
        agent,
        salience,
        feature,
        page_size,
        offset_rows,
    ) {
        Ok(r) => r,
        Err(e) => {
            eprintln!("error: list failed: {e}");
            return std::process::ExitCode::from(1);
        }
    };
    if args.json {
        let payload = serde_json::json!({
            "total":    total,
            "offset":   args.offset,
            "page_size": page_size,
            "returned": rows.len(),
            "rows":     rows,
        });
        println!(
            "{}",
            serde_json::to_string_pretty(&payload).unwrap_or_default()
        );
    } else {
        println!(
            "# insights — page {} (page_size={}) — total matching: {}",
            args.offset, page_size, total
        );
        if rows.is_empty() {
            println!("(no insights match)");
        }
        for r in &rows {
            let agent = r.agent_name.as_deref().unwrap_or("?");
            let kind = r.source_type.as_deref().unwrap_or("?");
            let sal = r.salience.as_deref().unwrap_or("?");
            let feat = r.feature_slug.as_deref().unwrap_or("-");
            println!();
            println!(
                "[{}] sha={} {} {} salience={} feature={}",
                r.id, r.sha256_short, agent, kind, sal, feat
            );
            println!("    {}", r.snippet);
        }
    }
    std::process::ExitCode::SUCCESS
}

/// `insight random` — uniform-random pick, optionally filtered.
fn run_insight_random(
    root: &std::path::Path,
    args: &cli::InsightRandomArgs,
) -> std::process::ExitCode {
    let (conn, _db_path) = match open_and_validate(root, &args.db_name) {
        Ok(t) => t,
        Err(code) => return code,
    };
    let kind = args.kind.as_deref();
    let agent = args.agent.as_deref();
    let salience = args.salience.as_ref().map(|s| s.as_str());
    let feature = args.feature.as_deref();
    let rec = match store::random_insight(&conn, kind, agent, salience, feature) {
        Ok(Some(r)) => r,
        Ok(None) => {
            eprintln!("error: no insights match the filters");
            return std::process::ExitCode::from(1);
        }
        Err(e) => {
            eprintln!("error: random fetch failed: {e}");
            return std::process::ExitCode::from(1);
        }
    };
    emit_insight_record(&rec, args.json);
    std::process::ExitCode::SUCCESS
}

/// `insight get <ident>` — fetch by integer `documents.id` or by sha256
/// prefix (≥4 hex chars matched via `sha256 LIKE 'prefix%'`).
fn run_insight_get(
    root: &std::path::Path,
    args: &cli::InsightGetArgs,
) -> std::process::ExitCode {
    let (conn, _db_path) = match open_and_validate(root, &args.db_name) {
        Ok(t) => t,
        Err(code) => return code,
    };
    let rec_result = if let Ok(id) = args.ident.parse::<i64>() {
        store::get_insight_by_id(&conn, id)
    } else {
        // Sha-prefix branch — reject obviously-bad input (too short OR
        // contains non-hex chars) before hitting the DB.
        if args.ident.len() < 4 {
            eprintln!(
                "error: sha prefix must be ≥4 hex chars (got `{}`)",
                args.ident
            );
            return std::process::ExitCode::from(2);
        }
        if !args.ident.chars().all(|c| c.is_ascii_hexdigit()) {
            eprintln!(
                "error: identifier must be an integer id or a hex sha prefix (got `{}`)",
                args.ident
            );
            return std::process::ExitCode::from(2);
        }
        store::get_insight_by_sha_prefix(&conn, &args.ident)
    };
    match rec_result {
        Ok(Some(rec)) => {
            emit_insight_record(&rec, args.json);
            std::process::ExitCode::SUCCESS
        }
        Ok(None) => {
            eprintln!("error: insight not found: {}", args.ident);
            std::process::ExitCode::from(1)
        }
        Err(e) => {
            eprintln!("error: fetch failed: {e}");
            std::process::ExitCode::from(1)
        }
    }
}

/// `insight gc [--dry-run]` — purge insights past their salience-driven
/// retention window (high=∞, medium=365d, low=90d). Runs VACUUM after
/// the deletes to reclaim storage; emits `{medium_deleted, low_deleted,
/// chunks_vec_orphans_cleared, freed_bytes}` on `--json`.
fn run_insight_gc(
    root: &std::path::Path,
    args: &cli::InsightGcArgs,
) -> std::process::ExitCode {
    let (mut conn, db_path) = match open_and_validate(root, &args.db_name) {
        Ok(t) => t,
        Err(code) => return code,
    };
    let now: i64 = {
        use std::time::{SystemTime, UNIX_EPOCH};
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs() as i64)
            .unwrap_or(0)
    };
    if args.dry_run {
        let summary = match store::count_insights_past_ttl(&conn, now) {
            Ok(s) => s,
            Err(e) => {
                eprintln!("error: gc dry-run failed: {e}");
                return std::process::ExitCode::from(1);
            }
        };
        if args.json {
            let payload = serde_json::json!({
                "dry_run":            true,
                "would_delete_medium": summary.medium_deleted,
                "would_delete_low":    summary.low_deleted,
                "would_delete_total":  summary.medium_deleted + summary.low_deleted,
            });
            println!(
                "{}",
                serde_json::to_string_pretty(&payload).unwrap_or_default()
            );
        } else {
            println!(
                "dry-run: would delete {} medium-salience + {} low-salience = {} total",
                summary.medium_deleted,
                summary.low_deleted,
                summary.medium_deleted + summary.low_deleted
            );
        }
        return std::process::ExitCode::SUCCESS;
    }
    let size_before = std::fs::metadata(&db_path).map(|m| m.len()).unwrap_or(0);
    let summary = match store::gc_insights_by_salience(&mut conn, now) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("error: gc failed: {e}");
            return std::process::ExitCode::from(1);
        }
    };
    // VACUUM cannot run inside a transaction. After gc_insights_by_salience
    // returns, the tx has committed and the connection is idle, so VACUUM
    // is safe here. Failures are warnings — the deletes already landed.
    if let Err(e) = conn.execute_batch("VACUUM") {
        eprintln!("warning: VACUUM after gc failed: {e}");
    }
    let size_after = std::fs::metadata(&db_path).map(|m| m.len()).unwrap_or(0);
    let freed_bytes = size_before.saturating_sub(size_after);
    if args.json {
        let payload = serde_json::json!({
            "dry_run":                     false,
            "medium_deleted":              summary.medium_deleted,
            "low_deleted":                 summary.low_deleted,
            "chunks_vec_orphans_cleared":  summary.chunks_vec_orphans_cleared,
            "deleted_total":               summary.medium_deleted + summary.low_deleted,
            "size_before_bytes":           size_before,
            "size_after_bytes":            size_after,
            "freed_bytes":                 freed_bytes,
        });
        println!(
            "{}",
            serde_json::to_string_pretty(&payload).unwrap_or_default()
        );
    } else {
        println!(
            "gc: deleted {} medium + {} low = {} insights; cleared {} orphan vectors; freed {} bytes",
            summary.medium_deleted,
            summary.low_deleted,
            summary.medium_deleted + summary.low_deleted,
            summary.chunks_vec_orphans_cleared,
            freed_bytes
        );
    }
    std::process::ExitCode::SUCCESS
}

/// `insight delete <id>` — single-insight delete by integer
/// `documents.id` with chunks + chunks_vec cascade. Refuses to delete
/// non-insight rows (source_type IS NULL — books corpus).
fn run_insight_delete(
    root: &std::path::Path,
    args: &cli::InsightDeleteArgs,
) -> std::process::ExitCode {
    let (mut conn, _db_path) = match open_and_validate(root, &args.db_name) {
        Ok(t) => t,
        Err(code) => return code,
    };
    let summary = match store::insight_delete_with_summary(&mut conn, args.id) {
        Ok(Some(s)) => s,
        Ok(None) => {
            // Disambiguate "not found" vs "not an insight" — re-probe the
            // row directly so the message is helpful.
            use rusqlite::OptionalExtension;
            let row: Option<Option<String>> = conn
                .query_row(
                    "SELECT source_type FROM documents WHERE id = ?1",
                    rusqlite::params![args.id],
                    |r| r.get(0),
                )
                .optional()
                .unwrap_or(None);
            match row {
                Some(Some(_)) => unreachable!("delete_with_summary returns Some when source_type set"),
                Some(None) => {
                    eprintln!(
                        "error: id {} is a books-corpus document, not an insight; refusing to delete",
                        args.id
                    );
                    return std::process::ExitCode::from(2);
                }
                None => {
                    eprintln!("error: no insight with id {}", args.id);
                    return std::process::ExitCode::from(1);
                }
            }
        }
        Err(e) => {
            eprintln!("error: insight delete failed: {e}");
            return std::process::ExitCode::from(1);
        }
    };
    if args.json {
        println!("{}", output::render_delete_by_id_json(&summary));
    } else {
        println!(
            "deleted: id={} source={} chunks={}",
            summary.deleted_id, summary.source_path, summary.chunks_removed
        );
    }
    std::process::ExitCode::SUCCESS
}

/// Shared formatter for `insight random` and `insight get`.
fn emit_insight_record(rec: &store::InsightRecord, json: bool) {
    if json {
        println!(
            "{}",
            serde_json::to_string_pretty(rec).unwrap_or_default()
        );
    } else {
        println!(
            "[{}] sha={} agent={} type={} salience={} feature={}",
            rec.id,
            &rec.sha256[..16.min(rec.sha256.len())],
            rec.agent_name.as_deref().unwrap_or("?"),
            rec.source_type.as_deref().unwrap_or("?"),
            rec.salience.as_deref().unwrap_or("?"),
            rec.feature_slug.as_deref().unwrap_or("-"),
        );
        if let Some(sa) = rec.parent_artifact.as_deref() {
            println!("source artifact: {sa}");
        }
        if let Some(sid) = rec.session_id.as_deref() {
            println!("session: {sid}");
        }
        println!();
        println!("{}", rec.body);
    }
}

/// Slice 5 — best-effort semantic-duplicate probe.
///
/// Returns `Some(doc_id)` when an existing insight in `insights.db` is a
/// near-duplicate of `body` according to the cosine-0.92 threshold AND
/// was emitted by the SAME `agent` within `cutoff_ingested_at`.
///
/// Best-effort: returns `None` silently when any of these conditions hold:
///   - The e5 encoder model is unavailable (degraded-install).
///   - The `chunks_vec` virtual table is absent (v1 schema).
///   - The K-NN query fails for any reason.
///
/// Threshold derivation: e5-multilingual-small emits L2-normalized
/// vectors, so cosine = 1 − L2² / 2. Solving for L2 at cosine = 0.92
/// gives L2 = sqrt(2 × (1 − 0.92)) = sqrt(0.16) = 0.4. sqlite-vec
/// returns L2 distance; we filter `distance < 0.4`.
///
/// Why same-agent only: cross-agent near-duplicates ARE load-bearing
/// signal (cross-agent agreement on an observation), so we let them
/// through — same rule as the exact-sha probe.
fn find_semantic_duplicate(
    conn: &rusqlite::Connection,
    body: &str,
    agent: &str,
    cutoff_ingested_at: i64,
) -> Option<i64> {
    /// Cosine 0.92 → L2 distance ~0.4 for L2-normalized e5 vectors.
    const SEMANTIC_DEDUP_L2_THRESHOLD: f64 = 0.4;

    // Skip silently when chunks_vec is absent (v1 schema or never had
    // vectors populated). Probing sqlite_master is cheaper than failing
    // the K-NN query.
    let has_vec: bool = conn
        .query_row(
            "SELECT 1 FROM sqlite_master WHERE type='table' AND name='chunks_vec'",
            [],
            |_| Ok(true),
        )
        .unwrap_or(false);
    if !has_vec {
        return None;
    }

    // Encode the body as a single passage. The encoder uses the "passage:"
    // prefix internally (e5 convention). For multi-chunk bodies the first
    // chunk's vector is sufficient as a topical signature — bodies that
    // share opening semantics are near-duplicates in practice.
    let embeddings = encoder::encode_passages(&[body]).ok()?;
    let body_emb = embeddings.first()?;
    let bytes: Vec<u8> = body_emb.iter().flat_map(|f| f.to_le_bytes()).collect();

    // K-NN against chunks_vec. JOIN here is intentional — sqlite-vec's
    // MATCH operator returns `distance` as a virtual column, and JOIN
    // with chunks + documents lets us filter same-agent + within-window
    // in the same query. LIMIT 5 over-fetches to handle the case where
    // the top-1 hit fails the agent/recency filter but a top-2..5 hit
    // would pass — rare but realistic in dense corpora.
    let mut stmt = conn
        .prepare(
            "SELECT cv.rowid, cv.distance, c.doc_id, d.agent_name, d.ingested_at \
             FROM chunks_vec cv \
             JOIN chunks c ON c.id = cv.rowid \
             JOIN documents d ON d.id = c.doc_id \
             WHERE cv.embedding MATCH ?1 AND k = 5 \
             ORDER BY cv.distance",
        )
        .ok()?;
    let rows = stmt
        .query_map(rusqlite::params![bytes], |r| {
            Ok((
                r.get::<_, i64>(0)?, // chunk_id (unused — kept for debugging)
                r.get::<_, f64>(1)?, // distance (L2)
                r.get::<_, i64>(2)?, // doc_id
                r.get::<_, Option<String>>(3)?, // agent_name
                r.get::<_, i64>(4)?, // ingested_at
            ))
        })
        .ok()?;

    for row in rows.flatten() {
        let (_chunk_id, distance, doc_id, doc_agent, doc_ingested_at) = row;
        if distance >= SEMANTIC_DEDUP_L2_THRESHOLD {
            // Sorted by distance ASC — once we exceed the threshold the
            // remaining candidates are even further; bail out early.
            break;
        }
        if doc_agent.as_deref() == Some(agent) && doc_ingested_at >= cutoff_ingested_at {
            return Some(doc_id);
        }
    }
    None
}

/// Best-effort embedding write into chunks_vec for an insight document.
///
/// Mirrors `ingest::try_populate_chunks_vec` but is reachable from main.rs
/// without exposing the private helper. Silent no-op when chunks_vec is
/// absent, the encoder is unavailable, or the id-count drift check trips.
fn try_populate_insight_chunks_vec(
    conn: &mut rusqlite::Connection,
    doc_id: i64,
    chunks: &[ingest::Chunk],
) -> Result<(), ()> {
    if chunks.is_empty() {
        return Ok(());
    }
    let has_vec: bool = conn
        .query_row(
            "SELECT 1 FROM sqlite_master WHERE type='table' AND name='chunks_vec'",
            [],
            |_| Ok(true),
        )
        .unwrap_or(false);
    if !has_vec {
        return Err(());
    }
    let texts: Vec<&str> = chunks.iter().map(|c| c.text.as_str()).collect();
    let embeddings = match encoder::encode_passages(&texts) {
        Ok(v) => v,
        Err(_) => return Err(()),
    };
    let ids: Vec<i64> = {
        let mut stmt = conn
            .prepare("SELECT id FROM chunks WHERE doc_id = ?1 ORDER BY ord")
            .map_err(|_| ())?;
        let rows = stmt
            .query_map(rusqlite::params![doc_id], |r| r.get::<_, i64>(0))
            .map_err(|_| ())?;
        rows.filter_map(Result::ok).collect()
    };
    if ids.len() != embeddings.len() {
        return Err(());
    }
    let tx = conn
        .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
        .map_err(|_| ())?;
    {
        let mut stmt = tx
            .prepare("INSERT OR REPLACE INTO chunks_vec(rowid, embedding) VALUES (?1, ?2)")
            .map_err(|_| ())?;
        for (id, emb) in ids.iter().zip(embeddings.iter()) {
            let bytes: Vec<u8> = emb.iter().flat_map(|f| f.to_le_bytes()).collect();
            stmt.execute(rusqlite::params![id, bytes]).map_err(|_| ())?;
        }
    }
    tx.commit().map_err(|_| ())?;
    Ok(())
}

/// `page <doc> <page> [--range N] [--json]` — Slice 12 page-level navigation.
///
/// Resolves the doc identifier (integer id OR basename match), looks up
/// the page in the `pages` table, and emits either the raw text (human
/// mode) or a structured JSON envelope including doc metadata and the
/// page neighborhood. Out-of-range page numbers exit 1 with the literal
/// `error: page number out of range` per the architect-resolved contract.
fn run_page(root: &std::path::Path, args: &cli::PageArgs) -> std::process::ExitCode {
    let (conn, _db_path) = match open_and_validate(root, "index.db") {
        Ok(t) => t,
        Err(code) => return code,
    };
    let resolved = match store::resolve_doc_id(&conn, &args.doc) {
        Ok(Some(t)) => t,
        Ok(None) => {
            eprintln!("error: document not found: {}", args.doc);
            return std::process::ExitCode::from(1);
        }
        Err(e) => {
            eprintln!("error: doc lookup failed: {e}");
            return std::process::ExitCode::from(1);
        }
    };
    let (doc_id, source_path, total_pages) = resolved;
    // Out-of-range gate: when total_pages is known, validate the requested
    // page falls within [1..total_pages]. When total_pages is NULL (pages
    // table not yet backfilled for this doc), fall through to the
    // pages-table lookup which will return None and we surface the same
    // error message.
    if let Some(tp) = total_pages {
        if args.page < 1 || args.page > tp {
            eprintln!("error: page number out of range");
            return std::process::ExitCode::from(1);
        }
    }
    let range = args.range.max(0).min(20);
    let lo = (args.page - range).max(1);
    let hi = args.page + range;
    let pages = match store::fetch_page_range(&conn, doc_id, lo, hi) {
        Ok(p) => p,
        Err(e) => {
            eprintln!("error: page fetch failed: {e}");
            return std::process::ExitCode::from(1);
        }
    };
    if pages.is_empty() {
        // Either the page IS out of range (and total_pages was NULL so we
        // couldn't gate above) OR the pages table hasn't been backfilled
        // for this doc — both surface the same user-facing error.
        eprintln!(
            "error: page number out of range (or pages not yet backfilled — run `claudebase reindex-pages --doc {}`)",
            args.doc
        );
        return std::process::ExitCode::from(1);
    }
    if args.json {
        let payload = serde_json::json!({
            "doc_id": doc_id,
            "source_path": source_path,
            "total_pages": total_pages,
            "requested_page": args.page,
            "range": range,
            "pages": pages.iter().map(|p| serde_json::json!({
                "page_no": p.page_no,
                "text": p.text,
            })).collect::<Vec<_>>(),
        });
        println!(
            "{}",
            serde_json::to_string_pretty(&payload).unwrap_or_default()
        );
    } else {
        let basename = std::path::Path::new(&source_path)
            .file_name()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_else(|| source_path.clone());
        println!("# {} — pages {}–{}", basename, lo, hi);
        if let Some(tp) = total_pages {
            println!("# (document has {} total pages)", tp);
        }
        for p in &pages {
            println!();
            println!("──── PAGE {} ────", p.page_no);
            println!();
            println!("{}", p.text);
        }
    }
    std::process::ExitCode::SUCCESS
}

/// `reindex-pages [--doc X] [--json]` — Slice 12 backfill subcommand.
///
/// For each ingested document (or just the one selected via `--doc`),
/// re-parses the source PDF via `pdf::read_pages` and populates the
/// `pages` table. Does NOT touch chunks /
/// chunks_fts / chunks_vec — preserves existing BM25 + embedding state.
/// Skips non-PDF sources (text/markdown documents have no concept of
/// pages) and missing-on-disk sources (logged as skipped, not failed).
fn run_reindex_pages(
    root: &std::path::Path,
    args: &cli::ReindexPagesArgs,
) -> std::process::ExitCode {
    let (mut conn, _db_path) = match open_and_validate(root, "index.db") {
        Ok(t) => t,
        Err(code) => return code,
    };
    // Build the list of (doc_id, source_path) tuples to process.
    let docs: Vec<(i64, String)> = {
        let sql = if args.doc.is_some() {
            "SELECT id, source_path FROM documents WHERE id = ?1 OR source_path = ?1 OR source_path LIKE ?2"
        } else {
            "SELECT id, source_path FROM documents ORDER BY id"
        };
        let mut stmt = match conn.prepare(sql) {
            Ok(s) => s,
            Err(e) => {
                eprintln!("error: prepare failed: {e}");
                return std::process::ExitCode::from(1);
            }
        };
        let rows: Result<Vec<(i64, String)>, _> = if let Some(d) = &args.doc {
            stmt.query_map(rusqlite::params![d, format!("%/{d}")], |r| {
                Ok((r.get(0)?, r.get(1)?))
            })
            .and_then(|it| it.collect())
        } else {
            stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?)))
                .and_then(|it| it.collect())
        };
        match rows {
            Ok(v) => v,
            Err(e) => {
                eprintln!("error: query failed: {e}");
                return std::process::ExitCode::from(1);
            }
        }
    };
    if docs.is_empty() {
        eprintln!("error: no matching documents");
        return std::process::ExitCode::from(1);
    }
    let mut succeeded: Vec<serde_json::Value> = Vec::new();
    let mut skipped: Vec<serde_json::Value> = Vec::new();
    let mut failed: Vec<serde_json::Value> = Vec::new();
    for (doc_id, source_path) in &docs {
        let path = std::path::PathBuf::from(source_path);
        let basename = path
            .file_name()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_else(|| source_path.clone());
        // Skip non-PDF — we can't extract pages from .md / .txt.
        let is_pdf = path
            .extension()
            .and_then(|e| e.to_str())
            .map(|e| e.eq_ignore_ascii_case("pdf"))
            .unwrap_or(false);
        if !is_pdf {
            if !args.json {
                eprintln!("skip: {basename} (not a PDF)");
            }
            skipped.push(serde_json::json!({
                "doc_id": doc_id, "source": basename, "reason": "not a PDF"
            }));
            continue;
        }
        if !path.exists() {
            if !args.json {
                eprintln!("skip: {basename} (source no longer on disk)");
            }
            skipped.push(serde_json::json!({
                "doc_id": doc_id, "source": basename, "reason": "missing on disk"
            }));
            continue;
        }
        if !args.json {
            eprintln!("processing: {basename}");
        }
        match pdf::read_pages(&path) {
            Ok(pages) => {
                let n = pages.len();
                let page_refs: Vec<(i64, &str)> = pages
                    .iter()
                    .enumerate()
                    .map(|(i, t)| ((i + 1) as i64, t.as_str()))
                    .collect();
                let tx_result = conn
                    .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
                    .and_then(|tx| {
                        store::replace_pages(&tx, *doc_id, &page_refs)?;
                        tx.commit()
                    });
                if let Err(e) = tx_result {
                    if !args.json {
                        eprintln!("FAIL: {basename}: {e}");
                    }
                    failed.push(serde_json::json!({
                        "doc_id": doc_id, "source": basename, "error": e.to_string()
                    }));
                } else {
                    if !args.json {
                        eprintln!("  ok ({n} pages)");
                    }
                    succeeded.push(serde_json::json!({
                        "doc_id": doc_id, "source": basename, "pages": n
                    }));
                }
            }
            Err(e) => {
                if !args.json {
                    eprintln!("FAIL: {basename}: {e}");
                }
                failed.push(serde_json::json!({
                    "doc_id": doc_id, "source": basename, "error": e.to_string()
                }));
            }
        }
    }
    if args.json {
        let payload = serde_json::json!({
            "succeeded": succeeded,
            "skipped": skipped,
            "failed": failed,
        });
        println!(
            "{}",
            serde_json::to_string_pretty(&payload).unwrap_or_default()
        );
    } else {
        eprintln!(
            "summary: {} succeeded, {} skipped, {} failed",
            succeeded.len(),
            skipped.len(),
            failed.len()
        );
    }
    std::process::ExitCode::SUCCESS
}

/// `compare <query> [--top-k N] [--max-chars N] [--json]` — A/B test all
/// three search modes side-by-side with FULL chunk text. Surfaces exactly
/// what an LLM would receive as RAG context-augmentation input.
fn run_compare(root: &std::path::Path, args: &cli::CompareArgs) -> std::process::ExitCode {
    let (conn, _db_path) = match open_and_validate(root, "index.db") {
        Ok(t) => t,
        Err(code) => return code,
    };
    let top_k = args.top_k as u32;

    // Run all three modes. Encoder failures fall back to empty results
    // for that specific mode (NOT to lexical) — the whole point of
    // `compare` is to see what each mode actually produces.
    let lex_hits = match search::search(&conn, &args.query, top_k, 0) {
        Ok(h) => h,
        Err(e) => {
            eprintln!("warning: lexical search failed: {e}");
            Vec::new()
        }
    };
    let (dense_hits, hybrid_hits) = match encoder::encode_query(&args.query) {
        Ok(emb) => {
            let d = search::dense_search(&conn, &emb, top_k).unwrap_or_else(|e| {
                eprintln!("warning: dense search failed: {e}");
                Vec::new()
            });
            let h = search::hybrid_search(&conn, &args.query, &emb, top_k).unwrap_or_else(|e| {
                eprintln!("warning: hybrid search failed: {e}");
                Vec::new()
            });
            (d, h)
        }
        Err(e) => {
            eprintln!(
                "warning: encoder unavailable ({e}); dense + hybrid columns will be empty. \
                 Run `bash install.sh --yes` to install the e5-multilingual-small model."
            );
            (Vec::new(), Vec::new())
        }
    };

    if args.json {
        let value = serde_json::json!({
            "query": &args.query,
            "top_k": args.top_k,
            "context_radius": args.context,
            "modes": {
                "lexical": expand_full_text(&conn, &lex_hits, args.context, args.max_chars),
                "dense": expand_full_text(&conn, &dense_hits, args.context, args.max_chars),
                "hybrid": expand_full_text(&conn, &hybrid_hits, args.context, args.max_chars),
            }
        });
        println!("{}", serde_json::to_string_pretty(&value).unwrap_or_default());
        return std::process::ExitCode::SUCCESS;
    }

    // Human-readable side-by-side: vertical sections per mode with FULL text.
    println!("============================================================");
    println!("QUERY: {}", &args.query);
    println!("TOP-K: {}  CONTEXT: ±{} chunks per hit", args.top_k, args.context);
    println!("============================================================");
    print_compare_section(&conn, "LEXICAL (BM25)", &lex_hits, args.context, args.max_chars);
    print_compare_section(&conn, "DENSE (sqlite-vec)", &dense_hits, args.context, args.max_chars);
    print_compare_section(&conn, "HYBRID (RRF k=60)", &hybrid_hits, args.context, args.max_chars);
    std::process::ExitCode::SUCCESS
}

/// Pretty-print one mode's hits with full chunk text + ±context neighbors
/// fetched from the DB. When `context_radius` > 0, each hit shows ~one
/// page of text instead of just the matched chunk.
fn print_compare_section(
    conn: &rusqlite::Connection,
    label: &str,
    hits: &[search::SearchHit],
    context_radius: usize,
    max_chars: usize,
) {
    println!();
    println!("──── MODE: {label} ────");
    if hits.is_empty() {
        println!("(no results)");
        return;
    }
    for (idx, hit) in hits.iter().enumerate() {
        let basename = std::path::Path::new(&hit.source)
            .file_name()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_else(|| hit.source.clone());
        println!();
        println!(
            "[{}] chunk_id={} ord={} score={:.4} source={}",
            idx + 1,
            hit.chunk_id,
            hit.ord,
            hit.score,
            basename
        );
        // Optional component scores when present (hybrid + dense modes).
        if let (Some(b), Some(d), Some(r)) =
            (hit.bm25_score, hit.dense_score, hit.rrf_score)
        {
            println!(
                "    bm25={:.4}  dense={:.4}  rrf={:.4}",
                b, d, r
            );
        }
        let full_text = fetch_chunk_with_context(conn, hit.chunk_id, context_radius)
            .unwrap_or_else(|_| {
                // Fallback to the FTS5 snippet if the lookup fails.
                hit.snippet.clone()
            });
        let char_count = full_text.chars().count();
        let preview = if max_chars > 0 && char_count > max_chars {
            let mut s: String = full_text.chars().take(max_chars).collect();
            s.push_str("…");
            s
        } else {
            full_text
        };
        // Indent each line of chunk text for readability.
        for line in preview.lines() {
            println!("    {}", line);
        }
    }
}

/// Look up the full `chunks.text` for a chunk_id. Used by `compare` to show
/// exactly what an LLM would see as RAG input rather than the FTS5 snippet.
fn fetch_chunk_text(conn: &rusqlite::Connection, chunk_id: i64) -> Result<String, rusqlite::Error> {
    conn.query_row(
        "SELECT text FROM chunks WHERE id = ?1",
        rusqlite::params![chunk_id],
        |r| r.get::<_, String>(0),
    )
}

/// Fetch the matched chunk PLUS ±`radius` neighbor chunks from the same
/// document, joined into one ~page-sized blob. When radius=0, this is
/// equivalent to `fetch_chunk_text`. Neighbors are joined with a literal
/// `\n\n--- chunk break ---\n\n` separator so the LLM (and human reader)
/// can see chunk boundaries.
///
/// Boundary clipping: requested ord values that fall outside the
/// document's actual ord range simply don't return rows — the SQL
/// `BETWEEN` is silently bounded by what exists. So a hit at ord=0 with
/// radius=2 returns chunks at ord ∈ {0,1,2} (3 chunks instead of 5).
fn fetch_chunk_with_context(
    conn: &rusqlite::Connection,
    chunk_id: i64,
    radius: usize,
) -> Result<String, rusqlite::Error> {
    if radius == 0 {
        return fetch_chunk_text(conn, chunk_id);
    }
    // 1. Look up the (doc_id, ord) of the matched chunk.
    let (doc_id, ord): (i64, i64) = conn.query_row(
        "SELECT doc_id, ord FROM chunks WHERE id = ?1",
        rusqlite::params![chunk_id],
        |r| Ok((r.get(0)?, r.get(1)?)),
    )?;
    // 2. Cap radius at search::MAX_CONTEXT_RADIUS (10) for safety.
    let r = (radius as u32).min(search::MAX_CONTEXT_RADIUS) as i64;
    let lo = ord - r;
    let hi = ord + r;
    // 3. Fetch the window in ascending ord order.
    let mut stmt = conn.prepare(
        "SELECT text FROM chunks \
         WHERE doc_id = ?1 AND ord BETWEEN ?2 AND ?3 \
         ORDER BY ord",
    )?;
    let texts: Vec<String> = stmt
        .query_map(rusqlite::params![doc_id, lo, hi], |r| {
            r.get::<_, String>(0)
        })?
        .filter_map(Result::ok)
        .collect();
    if texts.is_empty() {
        // Fallback: matched chunk vanished between ranking and context fetch
        // (concurrent delete?). Return just the matched chunk's snippet via
        // the simple lookup.
        return fetch_chunk_text(conn, chunk_id);
    }
    Ok(texts.join("\n\n--- chunk break ---\n\n"))
}

/// JSON-output helper: hydrate hits with full chunk text + ±context
/// neighbors + truncate per max_chars. Returns serde_json::Value array.
fn expand_full_text(
    conn: &rusqlite::Connection,
    hits: &[search::SearchHit],
    context_radius: usize,
    max_chars: usize,
) -> Vec<serde_json::Value> {
    hits.iter()
        .map(|h| {
            let basename = std::path::Path::new(&h.source)
                .file_name()
                .map(|s| s.to_string_lossy().into_owned())
                .unwrap_or_else(|| h.source.clone());
            let full = fetch_chunk_with_context(conn, h.chunk_id, context_radius)
                .unwrap_or_else(|_| h.snippet.clone());
            let truncated = if max_chars > 0 && full.chars().count() > max_chars {
                let mut s: String = full.chars().take(max_chars).collect();
                s.push_str("…");
                s
            } else {
                full
            };
            serde_json::json!({
                "chunk_id": h.chunk_id,
                "ord": h.ord,
                "score": h.score,
                "bm25_score": h.bm25_score,
                "dense_score": h.dense_score,
                "rrf_score": h.rrf_score,
                "source": basename,
                "text": truncated,
            })
        })
        .collect()
}

/// `warmup [--quiet]` — Slice 11 install-time encoder pre-load.
///
/// Triggers fastembed to download + cache the e5-multilingual-small ONNX
/// model into `~/.claude/tools/claudebase/models/` so the FIRST
/// `claudebase ingest` or `claudebase search --mode hybrid` doesn't pay
/// a 30-second cold-start stall. Idempotent — fastembed checks the cache
/// before redownloading; subsequent calls are <1 s. Network failures
/// (offline install, HF rate limit) are warnings, NOT errors — the
/// fallback path is fastembed's lazy download on first real use.
fn run_warmup(args: &cli::WarmupArgs) -> std::process::ExitCode {
    if !args.quiet {
        eprintln!(
            "warmup: pre-loading e5-multilingual-small encoder into ~/.claude/tools/claudebase/models/ ..."
        );
    }
    match encoder::encode_query("warmup") {
        Ok(v) => {
            if !args.quiet {
                eprintln!("warmup: ok — encoder ready ({} dims)", v.len());
            }
            std::process::ExitCode::SUCCESS
        }
        Err(e) => {
            eprintln!(
                "warmup: WARN — encoder pre-load failed ({e}); fastembed will retry on first real use"
            );
            // Exit 0 even on failure — warmup is best-effort. install.sh
            // proceeds; fastembed lazy-downloads on first ingest.
            std::process::ExitCode::SUCCESS
        }
    }
}

/// Open the index DB at `<root>/.claude/knowledge/index.db`, run migrations
/// (so a freshly-created DB has its `schema_version=1` row), and run the
/// corrupt-index gate (`validate_schema`). Any failure prints the literal
/// AC-7 user-facing stderr and returns `Err(ExitCode 1)`.
///
/// Running migrations on the read path is safe and idempotent — it inserts
/// the `schema_version` row only when missing — and lets reads against a
/// brand-new project (where `ingest` has never run) return empty results
/// instead of falsely flagging "corrupt".
fn open_and_validate(
    root: &std::path::Path,
    db_name: &str,
) -> Result<(rusqlite::Connection, std::path::PathBuf), std::process::ExitCode> {
    let db_name = match cli::validate_db_name(db_name) {
        Ok(n) => n,
        Err(e) => {
            eprintln!("error: {e}");
            return Err(std::process::ExitCode::from(2));
        }
    };
    let db_path = root.join(".claude").join("knowledge").join(db_name);
    // Tech-debt #4 wiring: use the v2 entry point so fresh DBs are stamped
    // with schema_version=2 and the chunks_vec virtual table is created.
    // Existing v1 DBs are left at v1 (open_or_init_v2 does NOT auto-migrate
    // — that is migrate_v1_to_v2's destructive-confirmation contract). This
    // means new ingests on fresh DBs populate chunks_vec; pre-existing v1
    // ingests continue to work as before until the user opts into migration.
    let mut conn = match store::open_or_init_v2(&db_path) {
        Ok(c) => c,
        Err(_) => {
            // open_or_init_v2 also creates parent dirs; a failure here means
            // the file exists but isn't a valid SQLite database. Map to AC-7.
            eprintln!("error: index database invalid; re-ingest required");
            return Err(std::process::ExitCode::from(1));
        }
    };
    if migrations::run_migrations(&mut conn).is_err() {
        // A migration failure on a freshly-opened DB also signals corruption.
        eprintln!("error: index database invalid; re-ingest required");
        return Err(std::process::ExitCode::from(1));
    }
    if store::validate_schema(&conn).is_err() {
        eprintln!("error: index database invalid; re-ingest required");
        return Err(std::process::ExitCode::from(1));
    }
    Ok((conn, db_path))
}

fn run_ingest(root: &std::path::Path, args: &cli::IngestArgs) -> std::process::ExitCode {
    // The user-supplied path may be relative; resolve against root.
    let target = if args.path.is_absolute() {
        args.path.clone()
    } else {
        root.join(&args.path)
    };

    let db_path = root.join(".claude").join("knowledge").join("index.db");

    // Tech-debt #4 wiring: ingest opens with v2 entry point so fresh DBs get
    // chunks_vec + type/image_bytes columns. Pre-existing v1 DBs continue to
    // work but skip the chunks_vec hook silently (architect-resolved
    // migration UX is destructive opt-in via migrate_v1_to_v2).
    let mut conn = match store::open_or_init_v2(&db_path) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("error: failed to open index database: {e}");
            return std::process::ExitCode::from(1);
        }
    };
    if let Err(e) = migrations::run_migrations(&mut conn) {
        eprintln!("error: migration failed: {e}");
        return std::process::ExitCode::from(1);
    }

    let result = match ingest::ingest(root, &target, &mut conn) {
        Ok(r) => r,
        Err(e) => {
            eprintln!("error: ingest failed: {e}");
            return std::process::ExitCode::from(1);
        }
    };

    if args.json {
        // Minimal JSON shape for downstream Slice 3 / agent consumers.
        let succeeded: Vec<String> =
            result.succeeded.iter().map(|p| p.display().to_string()).collect();
        let failed: Vec<serde_json::Value> = result
            .failed
            .iter()
            .map(|(p, msg)| {
                serde_json::json!({ "path": p.display().to_string(), "error": msg })
            })
            .collect();
        let unchanged: Vec<String> =
            result.unchanged.iter().map(|p| p.display().to_string()).collect();
        let payload = serde_json::json!({
            "succeeded": succeeded,
            "failed": failed,
            "unchanged": unchanged,
            "succeeded_count": result.succeeded.len(),
            "failed_count": result.failed.len(),
            "unchanged_count": result.unchanged.len(),
        });
        println!("{}", serde_json::to_string_pretty(&payload).unwrap());
    } else {
        for p in &result.succeeded {
            println!("ingested: {}", p.display());
        }
        for p in &result.unchanged {
            println!("unchanged: {}", p.display());
        }
        for (p, e) in &result.failed {
            println!("failed: {} — {}", p.display(), e);
        }
        println!(
            "summary: {} succeeded, {} unchanged, {} failed",
            result.succeeded.len(),
            result.unchanged.len(),
            result.failed.len()
        );
    }

    // Per FR-2.6: batch continues; return 0 even when some files failed.
    std::process::ExitCode::SUCCESS
}

/// `search <query> [--top-k N] [--mode lexical|dense|hybrid] [--json]`.
///
/// Mode dispatch (Slice 7 + technical-debt CLI wiring):
/// - `lexical` (iter-1 baseline): FTS5 BM25 only, works on v1 + v2 schemas
///   without requiring the e5 encoder model
/// - `dense`: sqlite-vec K-NN, requires v2 schema (chunks_vec) AND e5 model
/// - `hybrid` (default): BM25 ⊕ dense fused via RRF k=60; falls back to
///   lexical with a stderr warning when encoder unavailable OR chunks_vec
///   missing on a v1 DB
///
/// Corrupt-DB (AC-7) handling is uniform across modes — open + validate
/// happens BEFORE any mode-specific dispatch so a truncated index.db
/// always exits 1 with the canonical literal stderr message.
fn run_search(root: &std::path::Path, args: &cli::SearchArgs) -> std::process::ExitCode {
    let top_k = args.top_k as u32;
    let context_radius = args.context as u32;

    // Slice 6 — `--corpus all` cross-corpus RRF fusion. Dispatched BEFORE
    // the single-corpus path so we don't waste cycles opening one DB twice.
    if matches!(args.corpus, Some(cli::Corpus::All)) {
        if args.db_name != "index.db" {
            eprintln!(
                "warning: --corpus all overrides --db-name `{}` (opening both index.db and insights.db)",
                args.db_name
            );
        }
        return run_search_cross_corpus(root, args, top_k, context_radius);
    }

    // Single-corpus dispatch: resolve the effective db_name from --corpus
    // (Slice 6) when set, otherwise fall back to --db-name (legacy).
    let effective_db_name: String = match args.corpus {
        Some(cli::Corpus::Books) => {
            if args.db_name != "index.db" {
                eprintln!(
                    "warning: --corpus books overrides --db-name `{}`",
                    args.db_name
                );
            }
            "index.db".to_string()
        }
        Some(cli::Corpus::Insights) => {
            if args.db_name != "index.db" {
                eprintln!(
                    "warning: --corpus insights overrides --db-name `{}`",
                    args.db_name
                );
            }
            "insights.db".to_string()
        }
        Some(cli::Corpus::All) => unreachable!("handled above"),
        None => args.db_name.clone(),
    };

    // Step 1: open + validate. Use the v1 entry point regardless of mode so
    // a truncated index.db trips AC-7 (`index database invalid; re-ingest
    // required`) BEFORE any vector-search dispatch attempts to query
    // chunks_vec. This preserves the corrupt-index test contract for
    // lexical, dense, AND hybrid modes uniformly.
    let (conn, _db_path) = match open_and_validate(root, &effective_db_name) {
        Ok(t) => t,
        Err(code) => return code,
    };

    let hits_result = match args.mode {
        cli::SearchMode::Lexical => search::search(&conn, &args.query, top_k, context_radius),
        cli::SearchMode::Dense | cli::SearchMode::Hybrid => {
            run_search_with_encoder(&conn, args, top_k, context_radius)
        }
    };

    let hits = match hits_result {
        Ok(h) => h,
        Err(search::SearchError::FtsSyntax(msg)) => {
            eprintln!("error: invalid search query: {msg}");
            return std::process::ExitCode::from(1);
        }
        Err(search::SearchError::Db(e)) => {
            eprintln!("error: search failed: {e}");
            return std::process::ExitCode::from(1);
        }
    };

    if args.json {
        println!("{}", output::render_search_json(&hits));
    } else {
        print!("{}", output::render_search_human(&hits));
    }
    std::process::ExitCode::SUCCESS
}

/// Dense / hybrid search dispatch with graceful fallback to lexical.
///
/// Caller has already opened + validated the connection; this function
/// owns the encoder + vec-query lifecycle plus the two fallback paths:
/// 1. `encoder::encode_query` produces the 384-dim query vector. Failure
///    (model missing / runtime error) → fall back to lexical with stderr
///    warning. Most common during initial install before
///    `bash install.sh --yes` has populated `~/.claude/tools/claudebase/models/`.
/// 2. `dense_search` or `hybrid_search` runs the vector query. Failure
///    (chunks_vec missing on a v1 DB / SQLite error) → fall back to
///    lexical with a stderr warning. Most common when the user has a
///    pre-existing v1 corpus and hasn't yet re-ingested under v2.
fn run_search_with_encoder(
    conn: &rusqlite::Connection,
    args: &cli::SearchArgs,
    top_k: u32,
    context_radius: u32,
) -> Result<Vec<search::SearchHit>, search::SearchError> {
    let embedding = match encoder::encode_query(&args.query) {
        Ok(v) => v,
        Err(e) => {
            eprintln!(
                "warning: encoder unavailable ({e}); falling back to lexical mode. Run `bash install.sh --yes` to install the e5-multilingual-small model."
            );
            return search::search(conn, &args.query, top_k, context_radius);
        }
    };

    let result = match args.mode {
        cli::SearchMode::Dense => search::dense_search(conn, &embedding, top_k),
        cli::SearchMode::Hybrid => search::hybrid_search(conn, &args.query, &embedding, top_k),
        cli::SearchMode::Lexical => unreachable!("lexical handled by caller"),
    };

    match result {
        Ok(h) => Ok(h),
        Err(search::SearchError::Db(e)) => {
            // Most likely "no such table: chunks_vec" on a v1 DB OR
            // sqlite-vec extension not registered (auto-extension load
            // race with the v1-only open path). Fall back to lexical
            // with a clear warning explaining the migration path.
            eprintln!(
                "warning: vector search failed ({e}); falling back to lexical mode. Run `claudebase ingest <path>` to populate the v2 schema with embeddings."
            );
            search::search(conn, &args.query, top_k, context_radius)
        }
        Err(other) => Err(other),
    }
}

/// `search --corpus all` — cross-corpus search with RRF fusion (Slice 6).
///
/// Opens BOTH `index.db` (books) and `insights.db` (agent-written), runs
/// the configured search mode on each, then fuses the two ranked lists
/// via Reciprocal Rank Fusion (k=60 — same constant the in-corpus hybrid
/// search uses). Each emitted hit carries `source_corpus = "books"` or
/// `"insights"` so downstream consumers can distinguish.
///
/// Failure modes:
///   - One corpus missing on disk → silently treat as empty (the user may
///     have insights but no books, or vice versa).
///   - Both corpora empty → return zero hits with exit 0.
///   - Search error in one corpus → log warning, use the other corpus only.
fn run_search_cross_corpus(
    root: &std::path::Path,
    args: &cli::SearchArgs,
    top_k: u32,
    context_radius: u32,
) -> std::process::ExitCode {
    let books_hits = run_one_corpus_for_fusion(root, args, "index.db", top_k, context_radius);
    let insights_hits =
        run_one_corpus_for_fusion(root, args, "insights.db", top_k, context_radius);
    let fused = rrf_fuse_corpora(books_hits, insights_hits, top_k as usize);
    if args.json {
        println!("{}", output::render_search_json(&fused));
    } else {
        print!("{}", output::render_search_human(&fused));
    }
    std::process::ExitCode::SUCCESS
}

/// Helper for `run_search_cross_corpus`: opens one corpus, runs the search,
/// tags every hit with `source_corpus`. Returns an empty vec on any error
/// (missing DB, search failure) so the cross-corpus pass survives one-
/// corpus failure.
fn run_one_corpus_for_fusion(
    root: &std::path::Path,
    args: &cli::SearchArgs,
    db_name: &str,
    top_k: u32,
    context_radius: u32,
) -> Vec<search::SearchHit> {
    let (conn, _db_path) = match open_and_validate(root, db_name) {
        Ok(t) => t,
        Err(_) => {
            // One-corpus failure is expected (project may have books but no
            // insights yet); silently return empty.
            return Vec::new();
        }
    };
    let hits = match args.mode {
        cli::SearchMode::Lexical => {
            search::search(&conn, &args.query, top_k, context_radius).unwrap_or_default()
        }
        cli::SearchMode::Dense | cli::SearchMode::Hybrid => {
            run_search_with_encoder(&conn, args, top_k, context_radius).unwrap_or_default()
        }
    };
    let label = if db_name == "insights.db" {
        "insights"
    } else {
        "books"
    };
    hits.into_iter()
        .map(|mut h| {
            h.source_corpus = Some(label.to_string());
            h
        })
        .collect()
}

/// Reciprocal Rank Fusion across two corpus-tagged ranked lists.
///
/// Implements the canonical Cormack et al. 2009 formula with k=60 (the
/// same constant used by the in-corpus hybrid path in `search.rs`):
///     score(d) = Σ_corpus 1 / (60 + rank_in_corpus(d))
///
/// Each hit's identity is `(source_corpus, chunk_id)` — chunk_ids are
/// scoped per-DB so two hits with `chunk_id=42` but different corpora
/// are distinct documents.
///
/// The returned `score` field is the RRF score; the per-corpus `score`
/// is preserved in the original `bm25_score` / `dense_score` / `rrf_score`
/// fields when the source mode populated them.
fn rrf_fuse_corpora(
    books: Vec<search::SearchHit>,
    insights: Vec<search::SearchHit>,
    top_k: usize,
) -> Vec<search::SearchHit> {
    const RRF_K: f64 = 60.0;
    use std::collections::HashMap;
    // Key: (source_corpus, chunk_id) — both needed to disambiguate.
    let mut score_acc: HashMap<(String, i64), f64> = HashMap::new();
    let mut by_key: HashMap<(String, i64), search::SearchHit> = HashMap::new();
    for (rank, hit) in books.into_iter().enumerate() {
        let key = ("books".to_string(), hit.chunk_id);
        let inc = 1.0 / (RRF_K + (rank as f64 + 1.0));
        *score_acc.entry(key.clone()).or_insert(0.0) += inc;
        by_key.entry(key).or_insert(hit);
    }
    for (rank, hit) in insights.into_iter().enumerate() {
        let key = ("insights".to_string(), hit.chunk_id);
        let inc = 1.0 / (RRF_K + (rank as f64 + 1.0));
        *score_acc.entry(key.clone()).or_insert(0.0) += inc;
        by_key.entry(key).or_insert(hit);
    }
    let mut scored: Vec<(f64, (String, i64))> = score_acc
        .into_iter()
        .map(|(k, s)| (s, k))
        .collect();
    scored.sort_by(|a, b| b.0.partial_cmp(&a.0).unwrap_or(std::cmp::Ordering::Equal));
    scored
        .into_iter()
        .take(top_k)
        .filter_map(|(rrf_score, key)| {
            by_key.remove(&key).map(|mut h| {
                h.score = rrf_score;
                h.rrf_score = Some(rrf_score);
                h
            })
        })
        .collect()
}

/// `list [--json]` — list ingested documents with chunk counts.
fn run_list(root: &std::path::Path, args: &cli::ListArgs) -> std::process::ExitCode {
    let (conn, _db_path) = match open_and_validate(root, &args.db_name) {
        Ok(t) => t,
        Err(code) => return code,
    };

    let docs = match store::list_documents(&conn) {
        Ok(d) => d,
        Err(e) => {
            eprintln!("error: list failed: {e}");
            return std::process::ExitCode::from(1);
        }
    };

    if args.json {
        println!("{}", output::render_list_json(&docs));
    } else {
        print!("{}", output::render_list_human(&docs));
    }
    std::process::ExitCode::SUCCESS
}

/// `status [--json]` — schema_version + counts + db_path.
fn run_status(root: &std::path::Path, args: &cli::StatusArgs) -> std::process::ExitCode {
    let (conn, db_path) = match open_and_validate(root, &args.db_name) {
        Ok(t) => t,
        Err(code) => return code,
    };

    let info = match store::status_summary(&conn, &db_path) {
        Ok(i) => i,
        Err(e) => {
            eprintln!("error: status failed: {e}");
            return std::process::ExitCode::from(1);
        }
    };

    if args.json {
        println!("{}", output::render_status_json(&info));
    } else {
        print!("{}", output::render_status_human(&info));
    }
    std::process::ExitCode::SUCCESS
}

/// `delete --by-id <int>` OR `delete <source-path>` — mutually exclusive per
/// FR-4.1 (Slice 2). The two branches differ in their security posture:
///   - `--by-id` operates on the integer primary key, which never originated
///     from a user-controlled file path. The DB-open project-root canonicalize
///     gate (in `cli::resolve_project_root`) is the load-bearing security
///     boundary; no additional path check is needed (FR-4.3).
///   - The positional `<source-path>` branch (legacy iter-1 form) keeps the
///     Slice 1 cross-slice canonicalize-and-prefix-check in place verbatim.
fn run_delete(root: &std::path::Path, args: &cli::DeleteArgs) -> std::process::ExitCode {
    // FR-4.1 mutual exclusion — checked BEFORE opening the DB so a malformed
    // invocation never side-effects on the index.
    match (&args.by_id, &args.source_path) {
        (Some(_), Some(_)) => {
            eprintln!("error: --by-id and <source-path> are mutually exclusive");
            return std::process::ExitCode::from(2);
        }
        (None, None) => {
            eprintln!("error: --by-id or <source-path> required");
            return std::process::ExitCode::from(2);
        }
        _ => {}
    }

    let (mut conn, _db_path) = match open_and_validate(root, &args.db_name) {
        Ok(t) => t,
        Err(code) => return code,
    };

    // --by-id branch (FR-4.4 transactional via store helper, FR-4.5 JSON shape).
    if let Some(id) = args.by_id {
        let summary = match store::delete_by_id_with_summary(&mut conn, id) {
            Ok(Some(s)) => s,
            Ok(None) => {
                // FR-4.2: literal stderr + exit 1; transaction already rolled back.
                eprintln!("error: no document with id {id}");
                return std::process::ExitCode::from(1);
            }
            Err(e) => {
                eprintln!("error: delete failed: {e}");
                return std::process::ExitCode::from(1);
            }
        };
        if args.json {
            println!("{}", output::render_delete_by_id_json(&summary));
        } else {
            println!(
                "deleted: id={} source={} chunks={}",
                summary.deleted_id, summary.source_path, summary.chunks_removed
            );
        }
        return std::process::ExitCode::SUCCESS;
    }

    // Positional <source-path> branch — preserve iter-1 canonicalize-and-prefix
    // check verbatim. We unwrap because the mutual-exclusion check above
    // guarantees exactly one of (by_id, source_path) is Some at this point.
    let source_arg = args
        .source_path
        .as_ref()
        .expect("mutual exclusion guarantees source_path is Some here");

    // String path branch — canonicalize-and-prefix-check first (Slice 1
    // cross-slice security flag). The DB stores the path string EXACTLY as
    // ingest emitted it (`p.display().to_string()` from the canonical path),
    // so for the DELETE to match, we use the same canonical string here.
    let raw = std::path::Path::new(source_arg);
    let candidate: std::path::PathBuf = if raw.is_absolute() {
        raw.to_path_buf()
    } else {
        root.join(raw)
    };
    let canonical = match std::fs::canonicalize(&candidate) {
        Ok(p) => p,
        Err(_) => {
            // The file may have already been deleted from disk — fall back to
            // a verbatim string match against documents.source_path.
            // We still ENFORCE the prefix-check by requiring the raw string
            // to be either absolute-under-root or relative (which we resolved
            // against root above). A path that escapes root (`/etc/passwd`)
            // resolves to an absolute path NOT under root and is rejected.
            let not_canonical = candidate.clone();
            if !not_canonical.starts_with(root) {
                eprintln!(
                    "error: source path must resolve under project root: {}",
                    source_arg
                );
                return std::process::ExitCode::from(2);
            }
            not_canonical
        }
    };
    if !canonical.starts_with(root) {
        eprintln!(
            "error: source path must resolve under project root: {}",
            source_arg
        );
        return std::process::ExitCode::from(2);
    }

    // Match the exact form ingest stored: `canonical.display().to_string()`.
    let key = canonical.display().to_string();
    let n = match store::delete_by_source_path(&conn, &key) {
        Ok(n) => n,
        Err(e) => {
            eprintln!("error: delete failed: {e}");
            return std::process::ExitCode::from(1);
        }
    };
    if args.json {
        let escaped = serde_json::to_string(&key).unwrap_or_else(|_| "\"\"".to_string());
        println!(
            "{{\"deleted\": {n}, \"by\": \"source_path\", \"source_path\": {escaped}}}"
        );
    } else {
        println!("deleted {n} document(s) by source_path={key}");
    }
    std::process::ExitCode::SUCCESS
}

