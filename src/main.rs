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
        // The journal lives with the daemon, not under a project root.
        Command::Journal(_) => None,
        Command::Auth(_) => None,
        // Daemon / Plugin do NOT operate on a project filesystem — they
        // own a runtime-dir-scoped socket and chat.db under ~/.claude/.
        // The path-canonicalization gate still runs (project_root will
        // resolve to the current cwd) but the resolved root is unused
        // by the daemon/plugin dispatch.
        Command::Daemon(_) => None,
        // Chat reads `~/.claude/knowledge/chat.db` directly — user-level,
        // not per-project — so the project root is unused. The gate still
        // runs to keep the security backbone uniform.
        Command::Chat(_) => None,
        // `run` is an exec wrapper around the `claude` CLI — no filesystem
        // ops on a project root. The path gate still runs uniformly.
        Command::Run(_) => None,
        // `agent` reads chat.db (user-level) like `chat`. No per-project
        // root used; gate still runs uniformly.
        Command::Agent(_) => None,
        // `telegram` talks to the daemon over UDS and reads the user-level
        // chat.db; no project-scoped filesystem access.
        Command::Telegram(_) => None,
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
        Command::Journal(args) => {
            simple_async(claudebase::journal_cli::run(args.sub), "journal")
        }
        Command::Auth(args) => simple_async(claudebase::remote_cli::auth(args.sub), "auth"),
        Command::Daemon(args) => match args.sub {
            cli::DaemonSubcommand::Serve(serve_args) => run_daemon_serve(&serve_args),
            cli::DaemonSubcommand::Config(config_args) => match config_args.sub {
                cli::DaemonConfigSubcommand::Edit(a) => run_daemon_config_edit(&a),
                cli::DaemonConfigSubcommand::Show(a) => run_daemon_config_show(&a),
                cli::DaemonConfigSubcommand::Path => {
                    println!(
                        "{}",
                        claudebase::daemon::config::user_level_daemon_toml_path().display()
                    );
                    std::process::ExitCode::SUCCESS
                }
            },
            cli::DaemonSubcommand::Remote(a) => match a.sub {
                cli::DaemonRemoteSubcommand::Enable(a) => {
                    simple(claudebase::remote_cli::enable(&a.bind), "daemon remote enable")
                }
                cli::DaemonRemoteSubcommand::Disable => {
                    simple(claudebase::remote_cli::disable(), "daemon remote disable")
                }
                cli::DaemonRemoteSubcommand::Status => {
                    simple(claudebase::remote_cli::status(), "daemon remote status")
                }
                cli::DaemonRemoteSubcommand::Issue(a) => simple(
                    claudebase::remote_cli::issue(&a.client, a.host.as_deref()),
                    "daemon remote issue",
                ),
                cli::DaemonRemoteSubcommand::Revoke(a) => {
                    simple(claudebase::remote_cli::revoke(&a.client), "daemon remote revoke")
                }
                cli::DaemonRemoteSubcommand::Add(a) => {
                    simple(claudebase::remote_cli::add(&a.name, a.url.as_deref()), "daemon remote add")
                }
                cli::DaemonRemoteSubcommand::List => simple(claudebase::remote_cli::list(), "daemon remote list"),
                cli::DaemonRemoteSubcommand::Remove(a) => {
                    simple(claudebase::remote_cli::remove(&a.name), "daemon remote remove")
                }
                cli::DaemonRemoteSubcommand::Default(a) => {
                    simple(claudebase::remote_cli::set_default(&a.name), "daemon remote default")
                }
                cli::DaemonRemoteSubcommand::Test(a) => {
                    simple_async(claudebase::remote_cli::test(a.name.as_deref()), "daemon remote test")
                }
            },
            cli::DaemonSubcommand::Callback(a) => match a.sub {
                cli::DaemonCallbackSubcommand::Enable(a) => simple(
                    claudebase::callback_cli::enable(&a.bind, a.allow_remote),
                    "daemon callback enable",
                ),
                cli::DaemonCallbackSubcommand::Disable => {
                    simple(claudebase::callback_cli::disable(), "daemon callback disable")
                }
                cli::DaemonCallbackSubcommand::Status => {
                    simple(claudebase::callback_cli::status(), "daemon callback status")
                }
                cli::DaemonCallbackSubcommand::Token(a) => simple(
                    claudebase::callback_cli::token(&a.nick, a.reveal),
                    "daemon callback token",
                ),
                cli::DaemonCallbackSubcommand::Rotate(a) => simple(
                    claudebase::callback_cli::rotate(&a.nick, a.reveal),
                    "daemon callback rotate",
                ),
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
        Command::Chat(args) => match &args.sub {
            cli::ChatSubcommand::List(a) => run_chat_list(a),
            cli::ChatSubcommand::Threads(a) => run_chat_threads(a),
            cli::ChatSubcommand::Clone(a) => simple(run_chat_clone(a), "chat clone"),
            cli::ChatSubcommand::Dump(a) => simple(run_chat_dump(a), "chat dump"),
            cli::ChatSubcommand::Load(a) => simple(run_chat_load(a), "chat load"),
        },
        Command::Run(args) => run_claude_with_preset(&args),
        Command::Telegram(args) => match &args.sub {
            cli::TelegramSubcommand::Send(a) => run_telegram_send(a),
            cli::TelegramSubcommand::Status => run_telegram_status(),
            cli::TelegramSubcommand::Addbot(a) => run_telegram_addbot(a),
            cli::TelegramSubcommand::Bots => run_telegram_bots(),
            cli::TelegramSubcommand::GetMe(a) => simple_async(
                claudebase::telegram_cli::get_me_command(a.token.as_deref()),
                "telegram get_me",
            ),
            cli::TelegramSubcommand::Pair(a) => {
                simple(claudebase::access_cli::pair(&a.code), "telegram pair")
            }
            cli::TelegramSubcommand::Access => {
                simple(claudebase::access_cli::show(), "telegram access")
            }
            cli::TelegramSubcommand::Policy(a) => {
                simple(claudebase::access_cli::set_policy(&a.value), "telegram policy")
            }
            cli::TelegramSubcommand::Allow(a) => {
                simple(claudebase::access_cli::allow(&a.sender_id), "telegram allow")
            }
            cli::TelegramSubcommand::Revoke(a) => {
                simple(claudebase::access_cli::revoke(&a.sender_id), "telegram revoke")
            }
        },
        Command::Agent(args) => match args.sub {
            cli::AgentSubcommand::ListAlive(a) => run_agent_list_alive(&a),
            cli::AgentSubcommand::Inspect(a) => run_agent_inspect(&a),
            cli::AgentSubcommand::Send(a) => run_agent_send(&a),
            cli::AgentSubcommand::Whoami => {
                simple(claudebase::agent_cli::whoami(), "agent whoami")
            }
            cli::AgentSubcommand::Rename(a) => simple_async(
                claudebase::agent_cli::rename(&a.nick, a.new_callback_token),
                "agent rename",
            ),
            cli::AgentSubcommand::BindSession(a) => simple_async(
                claudebase::agent_cli::bind_session(a.session_id.clone(), a.stdin),
                "agent bind-session",
            ),
            cli::AgentSubcommand::Describe(a) => simple_async(
                claudebase::agent_cli::describe(&a.description, a.branch.as_deref()),
                "agent describe",
            ),
            cli::AgentSubcommand::List(a) => simple(
                claudebase::agent_cli::list(a.all, a.json),
                "agent list",
            ),
        },
    }
}

/// `claudebase agent list-alive --project current|all|<slug>` — Slice 6
/// of cli-to-cli-routing. Reads chat.db directly (daemon NOT required).
///
/// Project scope semantics:
///   * `current` (default) — resolves `git remote origin URL` of the
///     current working directory via `project_id::resolve_project_id`,
///     filters `WHERE project_id = ? AND project_id IS NOT NULL` so
///     legacy NULL-project_id rows are excluded.
///   * `all` — no filter; everything alive across all known projects.
///   * `<literal slug>` — exact match against the column.
/// Inspection commands that read this machine's `chat.db` directly. With a
/// remote daemon selected they would show this machine's data under the
/// remote's name, so they say so instead. Session commands (`agent send`,
/// `agent list`, `whoami`, `telegram send`, `journal`) follow the daemon.
fn refuse_remote(what: &str) -> Option<std::process::ExitCode> {
    match claudebase::daemon::remote::Endpoint::resolve() {
        Ok(e) if e.is_local() => None,
        Ok(e) => {
            eprintln!(
                "claudebase {what}: reads this machine's daemon only, and the daemon in use is {}.\n\
                 Run it on that machine, or here with {}=local.",
                e.describe(),
                claudebase::daemon::remote::DAEMON_ENV
            );
            Some(std::process::ExitCode::from(2))
        }
        Err(e) => {
            eprintln!("claudebase {what}: {e:#}");
            Some(std::process::ExitCode::from(2))
        }
    }
}

fn run_agent_list_alive(args: &cli::AgentListAliveArgs) -> std::process::ExitCode {
    if let Some(code) = refuse_remote("agent list-alive") {
        return code;
    }
    use claudebase::daemon::chat::open_chat_db;
    let conn = match open_chat_db() {
        Ok(c) => c,
        Err(e) => {
            eprintln!("error: open chat.db: {e}");
            return std::process::ExitCode::from(1);
        }
    };
    let (where_clause, project_value): (&str, Option<String>) = match args.project.as_str() {
        "all" => ("state = 'alive'", None),
        "current" => {
            let cwd = std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from("."));
            let pid = claudebase::project_id::resolve_project_id(&cwd);
            (
                "state = 'alive' AND project_id IS NOT NULL AND project_id = ?1",
                Some(pid),
            )
        }
        slug => (
            "state = 'alive' AND project_id IS NOT NULL AND project_id = ?1",
            Some(slug.to_string()),
        ),
    };
    let sql = format!(
        "SELECT agent_id, agent_name, project_id, branch, working_dir, \
         feature_description, last_pinged_at, dnd_until_ts \
         FROM agent_registry \
         WHERE {where_clause} \
         ORDER BY last_pinged_at DESC \
         LIMIT 1000"
    );
    let rows: Vec<AgentListRow> = {
        let mut stmt = match conn.prepare(&sql) {
            Ok(s) => s,
            Err(e) => {
                eprintln!("error: prepare query: {e}");
                return std::process::ExitCode::from(1);
            }
        };
        let mapper = |row: &rusqlite::Row<'_>| -> rusqlite::Result<AgentListRow> {
            Ok(AgentListRow {
                agent_id: row.get(0)?,
                agent_name: row.get(1)?,
                project_id: row.get(2)?,
                branch: row.get(3)?,
                working_dir: row.get(4)?,
                feature_description: row.get(5)?,
                last_seen_at: row.get(6)?,
                dnd_until_ts: row.get(7)?,
            })
        };
        let result: rusqlite::Result<Vec<AgentListRow>> = match project_value.as_ref() {
            Some(p) => stmt
                .query_map(rusqlite::params![p], mapper)
                .and_then(|it| it.collect()),
            None => stmt.query_map([], mapper).and_then(|it| it.collect()),
        };
        match result {
            Ok(r) => r,
            Err(e) => {
                eprintln!("error: query: {e}");
                return std::process::ExitCode::from(1);
            }
        }
    };
    if args.json {
        let json: Vec<serde_json::Value> = rows
            .iter()
            .map(|r| {
                serde_json::json!({
                    "agent_id": r.agent_id,
                    "agent_name": r.agent_name,
                    "project_id": r.project_id,
                    "branch": r.branch,
                    "working_dir": r.working_dir,
                    "feature_description": r.feature_description,
                    "last_seen_at": r.last_seen_at,
                    "dnd_until_ts": r.dnd_until_ts,
                })
            })
            .collect();
        println!("{}", serde_json::to_string_pretty(&json).unwrap_or_default());
    } else if rows.is_empty() {
        println!("no agents alive for scope {:?}", args.project);
    } else {
        println!(
            "{:<22} {:<12} {:<32} {:<28} {:<24} {:<10}",
            "AGENT_ID", "NAME", "PROJECT_ID", "BRANCH", "FEATURE", "DND_UNTIL"
        );
        for r in &rows {
            let dnd_label = match r.dnd_until_ts {
                None => "-".to_string(),
                Some(ts) if ts == i64::MAX => "indefinite".to_string(),
                Some(ts) => format!("{ts}"),
            };
            println!(
                "{:<22} {:<12} {:<32} {:<28} {:<24} {:<10}",
                truncate(&r.agent_id, 22),
                truncate(&r.agent_name, 12),
                truncate(r.project_id.as_deref().unwrap_or("-"), 32),
                truncate(r.branch.as_deref().unwrap_or("-"), 28),
                truncate(r.feature_description.as_deref().unwrap_or("-"), 24),
                dnd_label,
            );
        }
    }
    std::process::ExitCode::SUCCESS
}

/// `claudebase agent inspect <agent_id>` — Slice 6 + red team F-4
/// observability surface. Reads chat.db, returns a snapshot of the
/// registry row + undelivered_count from chat_messages. Exit 1 when
/// the agent_id is absent.
fn run_agent_inspect(args: &cli::AgentInspectArgs) -> std::process::ExitCode {
    if let Some(code) = refuse_remote("agent inspect") {
        return code;
    }
    use claudebase::daemon::chat::open_chat_db;
    let conn = match open_chat_db() {
        Ok(c) => c,
        Err(e) => {
            eprintln!("error: open chat.db: {e}");
            return std::process::ExitCode::from(1);
        }
    };
    let row: Option<AgentInspectRow> = conn
        .query_row(
            "SELECT agent_id, agent_name, state, connection_id, project_id, \
             branch, working_dir, feature_description, spawned_at, \
             last_pinged_at, dnd_until_ts \
             FROM agent_registry WHERE agent_id = ?1",
            rusqlite::params![&args.agent_id],
            |r| {
                Ok(AgentInspectRow {
                    agent_id: r.get(0)?,
                    agent_name: r.get(1)?,
                    state: r.get(2)?,
                    connection_id: r.get(3)?,
                    project_id: r.get(4)?,
                    branch: r.get(5)?,
                    working_dir: r.get(6)?,
                    feature_description: r.get(7)?,
                    spawned_at: r.get(8)?,
                    last_pinged_at: r.get(9)?,
                    dnd_until_ts: r.get(10)?,
                })
            },
        )
        .ok();
    let row = match row {
        Some(r) => r,
        None => {
            eprintln!("error: agent_id not found: {}", args.agent_id);
            return std::process::ExitCode::from(1);
        }
    };
    let thread = format!("agent:{}", args.agent_id);
    let undelivered_count: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM chat_messages \
             WHERE thread_id = ?1 AND delivered_at IS NULL",
            rusqlite::params![&thread],
            |r| r.get(0),
        )
        .unwrap_or(0);
    let now_ms = claudebase::daemon::chat::now_millis();
    let dnd_state = match row.dnd_until_ts {
        None => "off".to_string(),
        Some(ts) if ts == i64::MAX => "indefinite".to_string(),
        Some(ts) if ts > now_ms => format!("until_ms={ts}"),
        Some(_) => "expired".to_string(),
    };
    if args.json {
        let json = serde_json::json!({
            "agent_id": row.agent_id,
            "agent_name": row.agent_name,
            "state": row.state,
            "connection_id": row.connection_id,
            "project_id": row.project_id,
            "branch": row.branch,
            "working_dir": row.working_dir,
            "feature_description": row.feature_description,
            "spawned_at": row.spawned_at,
            "last_pinged_at": row.last_pinged_at,
            "dnd_until_ts": row.dnd_until_ts,
            "dnd_state": dnd_state,
            "undelivered_count": undelivered_count,
        });
        println!("{}", serde_json::to_string_pretty(&json).unwrap_or_default());
    } else {
        println!("agent_id:            {}", row.agent_id);
        println!("agent_name:          {}", row.agent_name);
        println!("state:               {}", row.state);
        println!(
            "project_id:          {}",
            row.project_id.as_deref().unwrap_or("-")
        );
        println!("branch:              {}", row.branch.as_deref().unwrap_or("-"));
        println!(
            "working_dir:         {}",
            row.working_dir.as_deref().unwrap_or("-")
        );
        println!(
            "feature_description: {}",
            row.feature_description.as_deref().unwrap_or("-")
        );
        println!("spawned_at:          {}", row.spawned_at);
        println!("last_pinged_at:      {}", row.last_pinged_at);
        println!("dnd_state:           {}", dnd_state);
        println!("undelivered_count:   {}", undelivered_count);
    }
    std::process::ExitCode::SUCCESS
}

fn truncate(s: &str, max: usize) -> String {
    if s.len() <= max {
        s.to_string()
    } else {
        format!("{}...", &s[..max.saturating_sub(3)])
    }
}

#[derive(Debug)]
struct AgentListRow {
    agent_id: String,
    agent_name: String,
    project_id: Option<String>,
    branch: Option<String>,
    working_dir: Option<String>,
    feature_description: Option<String>,
    last_seen_at: i64,
    dnd_until_ts: Option<i64>,
}

#[derive(Debug)]
struct AgentInspectRow {
    agent_id: String,
    agent_name: String,
    state: String,
    connection_id: String,
    project_id: Option<String>,
    branch: Option<String>,
    working_dir: Option<String>,
    feature_description: Option<String>,
    spawned_at: i64,
    last_pinged_at: i64,
    dnd_until_ts: Option<i64>,
}

/// `claudebase run [-- args...]` — exec `claude` with the telegram channel
/// plugin preset and any extra args forwarded verbatim. Replaces this
/// process (exec, not spawn) so signals + stdio flow straight to claude.
///
/// The SDLC SessionStart onboarding hook (if installed via claude-code-sdlc
/// install.sh) is wired into `~/.claude/settings.json` and auto-fires on
/// every session boot — no extra plumbing required here.
fn run_claude_with_preset(args: &cli::RunArgs) -> std::process::ExitCode {
    use std::ffi::OsString;

    let claude_path = match which("claude") {
        Some(p) => p,
        None => {
            eprintln!(
                "claudebase run: `claude` CLI not found on PATH.\n\
                 Install Claude Code from https://claude.com/code, then re-run."
            );
            return std::process::ExitCode::from(127);
        }
    };

    // Ensure daemon is alive BEFORE spawning claude so the plugin bridge's
    // first handshake sees a real tools/list. Skipped when --no-telegram
    // (plain claude, no MCP plugin slot needed). Best-effort: a timeout
    // here does NOT block claude startup — the bridge falls back to
    // daemon-down mode and emits tools/list_changed on reconnect.
    //
    // Pipe-probe (not SCM query) so we are immune to the v0.6 Windows
    // service::status() lying-success bug — see ensure_daemon_running.
    // Which daemon this session joins. Exported to `claude` so every
    // claudebase command the session runs — `agent send`, `journal add`,
    // `telegram send` — reaches the same daemon the session is registered on.
    let endpoint = match claudebase::daemon::remote::Endpoint::named(args.daemon.as_deref()) {
        Ok(e) => e,
        Err(e) => {
            eprintln!("claudebase run: {e:#}");
            return std::process::ExitCode::from(2);
        }
    };
    std::env::set_var(claudebase::daemon::remote::DAEMON_ENV, endpoint.env_value());
    if !endpoint.is_local() {
        // Never fall back to the local daemon: a session that silently lands
        // on another daemon would talk to the wrong agents.
        match claudebase::daemon::run_tokio(claudebase::remote_cli::test(Some(&endpoint.env_value()))) {
            Ok(report) => eprintln!("claudebase run: {report}"),
            Err(e) => {
                eprintln!("claudebase run: cannot join daemon {}: {e:#}", endpoint.describe());
                return std::process::ExitCode::from(2);
            }
        }
    }

    if !args.no_telegram {
        if endpoint.is_local() {
            ensure_daemon_running();
        }
        // Ensure <cwd>/.claudebase/config.json exists so the plugin
        // bridge (spawned later by CC) finds a stable per-project
        // session_id when it auto-registers. The bridge READS this
        // file but never CREATES it — only `claudebase run` does, so
        // a CC session launched from a non-project cwd (e.g. desktop
        // shortcut) silently falls back to cwd-basename / UUID per
        // bridge::derive_agent_id. Per-project persistence makes
        // /switch <id> stable across CC restarts from the same dir.
        ensure_project_config();
    }

    // pty-transport Slice 2 — the supervisor OWNS the terminal `claude` runs
    // in, so inbound Telegram / agent messages can be written into its input
    // exactly as if the operator typed them. This replaced the old
    // `exec claude --channels plugin:telegram@claude-plugins-official` path:
    // no plugin slot, no MCP channel, nothing that a Claude Code update or a
    // marketplace change can take away.
    //
    // `--no-telegram` keeps the legacy behaviour (plain exec, no daemon leg)
    // as an escape hatch when the supervisor itself is suspect.
    if args.no_telegram {
        let mut argv: Vec<OsString> = vec![claude_path.clone().into()];
        argv.extend(args.claude_args().into_iter().map(OsString::from));
        eprintln!("claudebase run → exec (no-telegram) {}", claude_path.display());
        #[cfg(unix)]
        {
            use std::os::unix::process::CommandExt;
            let err = std::process::Command::new(&claude_path).args(&argv[1..]).exec();
            eprintln!("claudebase run: exec failed: {err}");
            return std::process::ExitCode::from(126);
        }
        #[cfg(not(unix))]
        {
            return match std::process::Command::new(&claude_path).args(&argv[1..]).status() {
                Ok(status) => match status.code() {
                    Some(c) if (0..=255).contains(&c) => std::process::ExitCode::from(c as u8),
                    _ => std::process::ExitCode::SUCCESS,
                },
                Err(e) => {
                    eprintln!("claudebase run: spawn failed: {e}");
                    std::process::ExitCode::from(126)
                }
            };
        }
    }

    // Supervisor logs go to a FILE, never to the terminal.
    //
    // The supervisor shares the operator's terminal with Claude Code's TUI: a
    // log line printed there lands inside the rendered input box, which is what
    // it looked like when this shipped writing JSON to stderr. Diagnostics are
    // still needed — they are how the gate stalls and subscription problems get
    // found — so they are written to `~/.claude/logs/claudebase-run-<pid>.log`
    // and `CLAUDEBASE_LOG_STDERR=1` restores the old behaviour for anyone
    // debugging the supervisor itself outside a TUI.
    init_supervisor_tracing();
    match claudebase::supervisor::run(args) {
        Ok(code) => code,
        Err(e) => {
            eprintln!("claudebase run: {e:#}");
            std::process::ExitCode::FAILURE
        }
    }
}

/// Pipe-probe liveness check for the daemon. Returns true when a blocking
/// connect to the UDS / named-pipe path succeeds. Does NOT rely on
/// `service::status()` (SCM-only on Windows; blind to `daemon serve`
/// processes started outside the service manager — the v0.6 lying-success
/// bug operator hit on 2026-06-03).
fn is_daemon_alive() -> bool {
    use interprocess::local_socket::{prelude::*, GenericFilePath, Stream, ToFsName};
    let path = match claudebase::daemon::server::socket_path() {
        Ok(p) => p,
        Err(_) => return false,
    };
    let name = match path.to_fs_name::<GenericFilePath>() {
        Ok(n) => n,
        Err(_) => return false,
    };
    Stream::connect(name).is_ok()
}

/// Spawn `<current_exe> daemon serve` as a detached background process.
/// Delegate to the canonical helper in `claudebase::daemon::service` so
/// the CLI's `claudebase daemon start` (Windows platform module) and
/// `claudebase run`'s auto-spawn produce semantically identical daemon
/// processes (same log file, same HOME env injection, same detached
/// flags). Slice 6 followup 2026-06-04 moved the body to service.rs.
fn spawn_daemon_detached() -> std::io::Result<()> {
    claudebase::daemon::service::spawn_daemon_detached()
}

/// Ensure the daemon is reachable before claude exec. If pipe-probe says
/// down, spawn `daemon serve` detached and poll up to 3s for the pipe to
/// accept. Timeout warns but does NOT abort claude startup — the bridge
/// already has daemon-down fallback + reconnect logic.
fn ensure_daemon_running() {
    if is_daemon_alive() {
        return;
    }
    eprintln!("claudebase run: daemon not reachable, starting in background...");
    if let Err(e) = spawn_daemon_detached() {
        eprintln!(
            "claudebase run: failed to spawn daemon ({}) - claude will run with bridge in daemon-down mode",
            e
        );
        return;
    }
    // Poll up to 3 seconds (30 x 100 ms) for the pipe to come alive.
    // 3s covers tokio runtime construction + secrets/daemon-toml load +
    // server::serve bind on every supported OS we have measured.
    for _ in 0..30 {
        std::thread::sleep(std::time::Duration::from_millis(100));
        if is_daemon_alive() {
            // Only interesting when we had to start it — the "not reachable"
            // line above already told the operator something was happening.
            eprintln!("claudebase run: daemon up");
            return;
        }
    }
    eprintln!(
        "claudebase run: daemon start timed out after 3s - claude continuing; bridge will retry"
    );
}

/// Ensure `<cwd>/.claudebase/config.json` exists before exec'ing claude.
/// On first invocation from a project dir it creates the file with a
/// UUID v4 `session_id` and cwd-basename `name`; subsequent runs are a
/// no-op (the existing file is just re-read). Best-effort — a write
/// failure here logs a warning but does NOT block claude startup; the
/// bridge has a UUID fallback in `derive_agent_id` so the worst-case
/// outcome is "no per-project persistence today" rather than "claude
/// won't start".
fn ensure_project_config() {
    let cwd = match std::env::current_dir() {
        Ok(p) => p,
        Err(e) => {
            eprintln!(
                "claudebase run: cwd unavailable, skipping .claudebase/config.json bootstrap ({})",
                e
            );
            return;
        }
    };
    match claudebase::project_config::load_or_create(&cwd) {
        // Success is the normal case and says nothing the operator needs — the
        // terminal belongs to Claude Code's TUI, not to our bookkeeping. Only
        // failures, which change behaviour, are worth a line.
        Ok(_) => {}
        Err(e) => eprintln!(
            "claudebase run: failed to create .claudebase/config.json ({}) - bridge will fall back to cwd-basename / UUID",
            e
        ),
    }
}

/// Minimal PATH walker for `claude` executable. Avoids adding a `which`
/// crate dep for a 10-line helper.
fn which(name: &str) -> Option<std::path::PathBuf> {
    let path_env = std::env::var_os("PATH")?;
    let ext = if cfg!(windows) { ".exe" } else { "" };
    for dir in std::env::split_paths(&path_env) {
        let candidate = dir.join(format!("{}{}", name, ext));
        if candidate.is_file() {
            return Some(candidate);
        }
    }
    None
}

/// `claudebase chat list --thread X` — Slice 3 CLI introspection.
/// Reads chat.db directly without contacting the daemon. The schema is
/// ensured on open so a fresh box without a daemon ever running still
/// works (prints empty when no rows exist).
fn run_chat_list(args: &cli::ChatListArgs) -> std::process::ExitCode {
    if let Some(code) = refuse_remote("chat list") {
        return code;
    }
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

/// `claudebase chat clone <id|name> <name>` — copy a Claude Code conversation.
fn run_chat_clone(args: &cli::ChatCloneArgs) -> anyhow::Result<String> {
    use claudebase::chat_clone;
    let cwd = std::env::current_dir()?;
    let report =
        chat_clone::clone_conversation(&chat_clone::claude_dir()?, &cwd, &args.source, &args.name)?;
    Ok(format!(
        "cloned {} as `{}` ({} lines, {} dropped)\nnew id: {}\nfile:   {}\nresume: claude --resume {}   (or pick `{}` in /resume)",
        report.source_id,
        args.name.trim(),
        report.lines_copied,
        report.lines_dropped,
        report.new_id,
        report.transcript.display(),
        report.new_id,
        args.name.trim(),
    ))
}

/// `claudebase chat dump <id|name>` — pack a conversation into a .zip.
fn run_chat_dump(args: &cli::ChatDumpArgs) -> anyhow::Result<String> {
    use claudebase::{chat_clone, chat_transfer};
    let cwd = std::env::current_dir()?;
    // Read here rather than expanded by the caller's shell: an agent's
    // permission rules cannot vet `"$CLAUDE_CODE_SESSION_ID"` in a command
    // line, so a skill that passed it would stop at a prompt every time.
    let source = match &args.source {
        Some(s) => s.clone(),
        None => std::env::var(chat_transfer::SESSION_ID_ENV)
            .ok()
            .filter(|s| !s.is_empty())
            .ok_or_else(|| {
                anyhow::anyhow!(
                    "name the conversation to dump (its id or /resume name) — outside a Claude \
                     Code session there is no current one (${} is not set)",
                    chat_transfer::SESSION_ID_ENV
                )
            })?,
    };
    let claude_dir = chat_clone::claude_dir()?;
    // What claudebase keeps about this conversation and its directory. The
    // daemon owns the journal, so it is asked; a dump without it is still a
    // dump, but the operator is told, since the journal is half the point.
    let (id, source_cwd) = chat_transfer::conversation_source(&claude_dir, &cwd, &source)?;
    let project = source_cwd.as_deref().map(std::path::PathBuf::from).unwrap_or_else(|| cwd.clone());
    let mut notes = Vec::new();
    let journal = match claudebase::daemon::run_tokio(claudebase::journal_cli::export_entries(&project)) {
        Ok(j) => j,
        Err(e) => {
            notes.push(format!(
                "WARNING: the journal is NOT in this archive — the daemon did not answer ({e:#}). \
                 Start it (`claudebase daemon start`) and dump again."
            ));
            None
        }
    };
    let nick = conversation_nick(&id);
    let extras = chat_transfer::DumpExtras { journal, nick };
    let r = chat_transfer::dump_conversation(&claude_dir, &cwd, &id, args.out.as_deref(), &extras)?;
    let size = std::fs::metadata(&r.archive).map(|m| m.len()).unwrap_or(0);
    let mut msg = format!(
        "dumped {}{} ({} files, {:.1} MB -> {:.1} MB)\njournal: {}\nnick: {}\narchive: {}\non the other machine, from the project directory:\n  claudebase chat load {}",
        r.session_id,
        r.title.as_deref().map(|t| format!(" `{t}`")).unwrap_or_default(),
        r.files,
        r.bytes_in as f64 / 1e6,
        size as f64 / 1e6,
        match r.journal_entries {
            Some(n) => format!("{n} entries of {}", project.display()),
            None => format!("none for {}", project.display()),
        },
        extras.nick.as_deref().unwrap_or("(none remembered)"),
        r.archive.display(),
        r.archive.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default(),
    );
    for n in notes {
        msg.push_str(&format!("\n{n}"));
    }
    Ok(msg)
}

/// `claudebase chat load <file.zip>` — unpack a dump into this directory.
fn run_chat_load(args: &cli::ChatLoadArgs) -> anyhow::Result<String> {
    use claudebase::{chat_clone, chat_transfer};
    let cwd = std::env::current_dir()?;
    let r = chat_transfer::load_conversation(
        &chat_clone::claude_dir()?,
        &cwd,
        &args.archive,
        args.new_id,
        args.name.as_deref(),
    )?;
    let mut msg = format!(
        "loaded {}{} ({} lines)",
        r.session_id,
        r.title.as_deref().map(|t| format!(" `{t}`")).unwrap_or_default(),
        r.lines,
    );
    if r.session_id != r.source_id {
        msg.push_str(&format!(" — a copy of {}", r.source_id));
    }
    if let Some(from) = &r.source_cwd {
        msg.push_str(&format!("\nproject paths moved: {from} -> {}", cwd.display()));
    }
    msg.push_str(&format!("\n{}", load_nick(&r)));
    if r.journal_entries > 0 {
        let entries = chat_transfer::read_journal(&args.archive)?;
        let moved = claudebase::journal_cli::Moved {
            session: (r.session_id != r.source_id).then_some((r.source_id.as_str(), r.session_id.as_str())),
            source_project: r.source_cwd.as_deref(),
        };
        match claudebase::daemon::run_tokio(claudebase::journal_cli::import_entries(&cwd, entries, moved)) {
            Ok(report) => msg.push_str(&format!("\n{}", report.describe())),
            Err(e) => msg.push_str(&format!(
                "\nWARNING: the journal was NOT imported — the daemon did not answer ({e:#}).\n\
                 Start it (`claudebase daemon start`), then from this directory:\n  claudebase journal import {}",
                args.archive.display()
            )),
        }
    }
    msg.push_str(&format!(
        "\nfile:   {}\nresume: claude --resume {}{}",
        r.transcript.display(),
        r.session_id,
        r.title.as_deref().map(|t| format!("   (or pick `{t}` in /resume)")).unwrap_or_default(),
    ));
    Ok(msg)
}

/// The nick conversation `id` answers to, from the daemon in use.
fn conversation_nick(id: &str) -> Option<String> {
    let endpoint = claudebase::daemon::remote::Endpoint::resolve().ok()?;
    if endpoint.is_local() {
        let c = claudebase::daemon::chat::open_chat_db_readonly().ok()?;
        return claudebase::daemon::agent_registry::recall_conversation_nick(&c, id).ok().flatten();
    }
    let v = claudebase::daemon::run_tokio(async {
        let mut c = claudebase::daemon::client::DaemonClient::connect_to(&endpoint).await?;
        c.call_tool("session_conversation_nick", serde_json::json!({ "session_id": id })).await
    })
    .ok()?;
    v["nick"].as_str().map(str::to_owned)
}

/// Give the loaded conversation back the nick it had, unless this machine
/// already remembers one for it.
fn load_nick(r: &claudebase::chat_transfer::LoadReport) -> String {
    use claudebase::daemon::{agent_registry, chat};
    let Some(nick) = r.nick.as_deref() else {
        return "nick: none carried".into();
    };
    // From another machine: only a name the daemon would accept.
    if agent_registry::validate_agent_name(nick).is_err() {
        return format!("nick: `{nick}` is not a valid nick here, not kept");
    }
    match claudebase::daemon::remote::Endpoint::resolve() {
        Ok(e) if e.is_local() => {}
        // The conversation will be resumed against that daemon; it learns the
        // nick from the session's first rename or bind there.
        Ok(e) => return format!("nick: `{nick}` (not recorded: the daemon in use is {})", e.describe()),
        Err(e) => return format!("nick: `{nick}` not kept ({e:#})"),
    }
    let result = chat::open_chat_db().map_err(anyhow::Error::from).and_then(|c| {
        if agent_registry::recall_conversation_nick(&c, &r.session_id)?.is_some() {
            return Ok(false);
        }
        agent_registry::remember_conversation_nick(&c, &r.session_id, nick, chrono::Utc::now().timestamp_millis())?;
        Ok(true)
    });
    match result {
        Ok(true) => format!("nick: `{nick}` — the session resuming it will take that name"),
        Ok(false) => "nick: this machine already has one for this conversation, kept it".into(),
        Err(e) => format!("nick: `{nick}` not kept ({e:#})"),
    }
}

/// `claudebase chat threads` — list all known chat threads.
fn run_chat_threads(args: &cli::ChatThreadsArgs) -> std::process::ExitCode {
    if let Some(code) = refuse_remote("chat threads") {
        return code;
    }
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
/// Where a supervisor session writes its log.
///
/// Per-pid so concurrent sessions do not interleave, and under `~/.claude/logs`
/// next to the daemon's own logs rather than in the project directory.
fn supervisor_log_path() -> Option<std::path::PathBuf> {
    let home = std::env::var_os("HOME").or_else(|| std::env::var_os("USERPROFILE"))?;
    let dir = std::path::PathBuf::from(home).join(".claude").join("logs");
    std::fs::create_dir_all(&dir).ok()?;
    Some(dir.join(format!("claudebase-run-{}.log", std::process::id())))
}

/// Shared file handle usable as a `tracing` writer.
#[derive(Clone)]
struct SharedFile(std::sync::Arc<std::sync::Mutex<std::fs::File>>);

impl std::io::Write for SharedFile {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        match self.0.lock() {
            Ok(mut f) => f.write(buf),
            // A poisoned mutex must not take the session down over a log line.
            Err(_) => Ok(buf.len()),
        }
    }
    fn flush(&mut self) -> std::io::Result<()> {
        match self.0.lock() {
            Ok(mut f) => f.flush(),
            Err(_) => Ok(()),
        }
    }
}

impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for SharedFile {
    type Writer = SharedFile;
    fn make_writer(&'a self) -> Self::Writer {
        self.clone()
    }
}

/// Tracing for `claudebase run`: file by default, stderr only on request.
fn init_supervisor_tracing() {
    use tracing_subscriber::{fmt, prelude::*, EnvFilter};

    let filter = || {
        EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("claudebase=info"))
    };

    if std::env::var_os("CLAUDEBASE_LOG_STDERR").is_some() {
        let _ = tracing_subscriber::registry()
            .with(filter())
            .with(fmt::layer().json().with_writer(std::io::stderr))
            .try_init();
        return;
    }

    let Some(path) = supervisor_log_path() else { return };
    let Ok(file) = std::fs::OpenOptions::new().create(true).append(true).open(&path) else {
        return;
    };
    let writer = SharedFile(std::sync::Arc::new(std::sync::Mutex::new(file)));
    let _ = tracing_subscriber::registry()
        .with(filter())
        .with(fmt::layer().json().with_writer(writer))
        .try_init();
}

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
/// `claudebase agent send` — agent-to-agent messaging over the CLI.
fn run_agent_send(args: &cli::AgentSendArgs) -> std::process::ExitCode {
    use std::io::Read;

    let text = if args.stdin {
        let mut buf = String::new();
        if let Err(e) = std::io::stdin().read_to_string(&mut buf) {
            eprintln!("claudebase agent chat: reading stdin failed: {e}");
            return std::process::ExitCode::FAILURE;
        }
        buf
    } else {
        match args.text.as_deref() {
            Some(t) => t.to_string(),
            None => {
                eprintln!("claudebase agent send: pass the text as an argument or use --stdin");
                return std::process::ExitCode::from(2);
            }
        }
    };

    let (target, is_id) = match (args.agent_id.as_deref(), args.agent_nick.as_deref()) {
        (Some(id), _) => (id.to_string(), true),
        (None, Some(name)) => (name.to_string(), false),
        (None, None) => {
            eprintln!("claudebase agent send: pass --agent_nick <nick> or --agent_id <id>");
            return std::process::ExitCode::from(2);
        }
    };

    match claudebase::daemon::run_tokio(claudebase::agent_cli::send(
        text.trim_end_matches('\n'),
        &target,
        is_id,
    )) {
        Ok(msg) => {
            println!("{msg}");
            std::process::ExitCode::SUCCESS
        }
        Err(e) => {
            eprintln!("claudebase agent send: {e:#}");
            std::process::ExitCode::FAILURE
        }
    }
}

/// `claudebase telegram send` — pty-transport Slice 1.
///
/// Runs on the sync dispatch path like every other short CLI subcommand;
/// the tokio runtime is built only for the duration of the daemon round-trip
/// (invariant 1 in the module header: no `#[tokio::main]`).
fn run_telegram_send(args: &cli::TelegramSendArgs) -> std::process::ExitCode {
    use std::io::Read;

    let text = if args.stdin {
        let mut buf = String::new();
        if let Err(e) = std::io::stdin().read_to_string(&mut buf) {
            eprintln!("claudebase telegram send: reading stdin failed: {e}");
            return std::process::ExitCode::FAILURE;
        }
        buf
    } else {
        match args.text.as_deref() {
            Some(t) => t.to_string(),
            None => {
                eprintln!("claudebase telegram send: pass --text \"…\" or --stdin");
                return std::process::ExitCode::from(2);
            }
        }
    };

    let result = claudebase::daemon::run_tokio(claudebase::telegram_cli::send(
        text.trim_end_matches('\n'),
        args.thread.as_deref(),
    ));
    match result {
        Ok(msg) => {
            println!("{msg}");
            std::process::ExitCode::SUCCESS
        }
        Err(e) => {
            eprintln!("claudebase telegram send: {e:#}");
            std::process::ExitCode::FAILURE
        }
    }
}

/// Print a command's `Result<String>` and map it to an exit code.
///
/// Every access command has the same shape — succeed with a human-readable
/// report or fail with a message — so the plumbing lives in one place instead
/// of six near-identical runners.
fn simple(result: anyhow::Result<String>, what: &str) -> std::process::ExitCode {
    match result {
        Ok(msg) => {
            println!("{}", msg.trim_end());
            std::process::ExitCode::SUCCESS
        }
        Err(e) => {
            eprintln!("claudebase {what}: {e:#}");
            std::process::ExitCode::FAILURE
        }
    }
}

/// Same, for commands that need the tokio runtime (network calls).
fn simple_async<F: std::future::Future<Output = anyhow::Result<String>>>(
    fut: F,
    what: &str,
) -> std::process::ExitCode {
    simple(claudebase::daemon::run_tokio(fut), what)
}

/// `claudebase telegram status` — explain the resolved default target.
fn run_telegram_status() -> std::process::ExitCode {
    match claudebase::telegram_cli::status() {
        Ok(report) => {
            print!("{report}");
            std::process::ExitCode::SUCCESS
        }
        Err(e) => {
            eprintln!("claudebase telegram status: {e:#}");
            std::process::ExitCode::FAILURE
        }
    }
}

/// `claudebase telegram addbot <token>` — Slice 8.
fn run_telegram_addbot(args: &cli::TelegramAddbotArgs) -> std::process::ExitCode {
    match claudebase::daemon::run_tokio(claudebase::telegram_cli::addbot(
        &args.token,
        args.label.as_deref(),
    )) {
        Ok(msg) => {
            println!("{msg}");
            std::process::ExitCode::SUCCESS
        }
        Err(e) => {
            eprintln!("claudebase telegram addbot: {e:#}");
            std::process::ExitCode::FAILURE
        }
    }
}

/// `claudebase telegram bots` — list registered bots.
fn run_telegram_bots() -> std::process::ExitCode {
    match claudebase::telegram_cli::list_bots() {
        Ok(report) => {
            print!("{report}");
            std::process::ExitCode::SUCCESS
        }
        Err(e) => {
            eprintln!("claudebase telegram bots: {e:#}");
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

    // A missing daemon.toml means defaults, not failure: the file is optional
    // and the default backend is whisper. Refusing to run here removed the one
    // command that would have explained why voice notes were not transcribing
    // on exactly the machines where nothing had been configured.
    let toml_path = config::user_level_daemon_toml_path();
    let cfg = match config::load_daemon_toml_or_default(&toml_path) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("error: failed to load daemon.toml: {e}");
            return std::process::ExitCode::FAILURE;
        }
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
    use claudebase::daemon::{asr, config};

    // Someone is waiting on this — an operator, or an installer showing them
    // its output — and a model download can take many minutes.
    asr::download::REPORT_PROGRESS.store(true, std::sync::atomic::Ordering::Relaxed);

    // A missing daemon.toml means defaults, not failure: the file is optional
    // and the default backend is whisper. Refusing to run here removed the one
    // command that would have explained why voice notes were not transcribing
    // on exactly the machines where nothing had been configured.
    let toml_path = config::user_level_daemon_toml_path();
    let cfg = match config::load_daemon_toml_or_default(&toml_path) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("error: failed to load daemon.toml: {e}");
            return std::process::ExitCode::FAILURE;
        }
    };

    // Through the factory and the trait, so warming up a backend needs an
    // implementation rather than another branch here. This function used to
    // hold whisper's model path and download call inline behind an
    // `if backend != "whisper" { bail }`, which is why the installers could
    // pre-fetch exactly one backend's model.
    let backend = match asr::make_asr(&cfg) {
        Ok(b) => b,
        Err(e) => {
            eprintln!("warmup: {e}");
            return std::process::ExitCode::FAILURE;
        }
    };
    match backend.warmup() {
        Ok(()) => {
            println!("{}: model ready", backend.name());
            std::process::ExitCode::SUCCESS
        }
        Err(e) => {
            eprintln!("warmup: {} model download failed: {e:#}", backend.name());
            std::process::ExitCode::FAILURE
        }
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

    // Step 1: open + validate. Use the v1 entry point regardless of mode so
    // a truncated index.db trips AC-7 (`index database invalid; re-ingest
    // required`) BEFORE any vector-search dispatch attempts to query
    // chunks_vec. This preserves the corrupt-index test contract for
    // lexical, dense, AND hybrid modes uniformly.
    let (conn, _db_path) = match open_and_validate(root, &args.db_name) {
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

