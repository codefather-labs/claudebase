//! CLI argument structs + `resolve_project_root` security backbone.
//!
//! `resolve_project_root` is the ONLY path-from-user-input gate in this binary.
//! Every subcommand MUST funnel filesystem access through the canonicalized
//! `PathBuf` returned here. Adding any other public function in this module
//! that returns `PathBuf` will break the `test_cli_rs_has_single_pub_pathbuf_fn`
//! invariant in `tests/path_safety_test.rs`.
//!
//! Phase 1.5 Security MUST requirements implemented:
//!   1. Canonicalize BOTH `--project-root` arg AND `current_dir()` (macOS /tmp aliasing).
//!   2. Use `Path::starts_with` on canonicalized `PathBuf`s — never `str::starts_with`.
//!   3. Order: canonicalize → prefix-check (not the reverse).
//!   4. Literal stderr message + exit 2 (handled by caller in `main.rs`).
//!   5. Stay in `Path`/`PathBuf`/`OsStr`; never `to_str().unwrap()` on path bytes.
//!   6. Map ALL `canonicalize` errors uniformly to `EscapesCwd` (no info leak).
//!   7. Callers receive the canonicalized `PathBuf`, never the original arg (TOCTOU discipline).

use clap::{Args, Subcommand, ValueEnum};
use std::path::{Path, PathBuf};
use thiserror::Error;

/// Search mode (Slice 7 of vector-retrieval-backend). Default is `hybrid` —
/// best quality when the e5-multilingual-small model is installed; falls
/// back to `lexical` automatically when the encoder model is missing or
/// the schema is at v1 (no chunks_vec virtual table).
#[derive(ValueEnum, Clone, Copy, Debug, PartialEq, Eq)]
pub enum SearchMode {
    /// BM25-only via FTS5 (iter-1 baseline; works on v1 + v2 DBs without encoder)
    Lexical,
    /// Pure dense via sqlite-vec K-NN; requires e5 encoder + v2 schema
    Dense,
    /// BM25 ⊕ dense fused via RRF k=60; default mode (auto-fallback to lexical
    /// when encoder unavailable)
    Hybrid,
}

impl Default for SearchMode {
    fn default() -> Self {
        SearchMode::Hybrid
    }
}

/// Salience tag per cognitive-self-check rule. Drives retention in the
/// journal: `high` survives forever, `medium` 1 year, `low` 90 days.
#[derive(ValueEnum, Clone, Copy, Debug, PartialEq, Eq)]
pub enum Salience {
    /// Load-bearing for the whole artifact; retained indefinitely.
    High,
    /// Affects correctness of a slice/decision; retained ~1 year.
    Medium,
    /// Context-setting only; retained ~90 days then GC'd.
    Low,
}

impl Salience {
    pub fn as_str(&self) -> &'static str {
        match self {
            Salience::High => "high",
            Salience::Medium => "medium",
            Salience::Low => "low",
        }
    }
}

#[derive(Debug, Error)]
pub enum ProjectRootError {
    #[error("project-root must resolve under current working directory")]
    EscapesCwd,
}

/// Resolve a project-root argument under the current working directory.
///
/// Returns the canonicalized `PathBuf` on success. Any path that escapes the
/// canonicalized cwd — via `..` traversal, symlink target, or absolute path —
/// is rejected with `ProjectRootError::EscapesCwd`. All `canonicalize` errors
/// (ENOENT, EACCES, ELOOP, …) are mapped uniformly to the same variant to
/// avoid information leaks.
///
/// When `arg` is `None`, the canonicalized cwd itself is returned.
pub fn resolve_project_root(arg: Option<&Path>) -> Result<PathBuf, ProjectRootError> {
    let cwd = std::env::current_dir().map_err(|_| ProjectRootError::EscapesCwd)?;
    let cwd_canonical = std::fs::canonicalize(&cwd).map_err(|_| ProjectRootError::EscapesCwd)?;

    let target = match arg {
        Some(p) => p.to_path_buf(),
        None => return Ok(cwd_canonical),
    };

    // Resolve relative paths against the original cwd; canonicalize will then
    // walk the symlink chain on the resulting absolute path.
    let resolved = if target.is_absolute() {
        target
    } else {
        cwd.join(target)
    };

    let target_canonical =
        std::fs::canonicalize(&resolved).map_err(|_| ProjectRootError::EscapesCwd)?;

    if !target_canonical.starts_with(&cwd_canonical) {
        return Err(ProjectRootError::EscapesCwd);
    }

    Ok(target_canonical)
}

// ---------------------------------------------------------------------------
// Subcommand argument structs. Each carries `--project-root` and `--json`.
// ---------------------------------------------------------------------------

#[derive(Args, Debug)]
pub struct IngestArgs {
    /// File or directory to ingest.
    pub path: PathBuf,
    #[arg(long)]
    pub project_root: Option<PathBuf>,
    #[arg(long)]
    pub json: bool,
}

/// Which corpus file to open under `<project>/.claude/knowledge/`.
/// `index.db` (default) is the user-curated books corpus.
/// Anything else: must end in `.db` and contain no path separators.
const DEFAULT_DB_NAME: &str = "index.db";

/// Validate a `db_name` value: must end in `.db` and contain no path
/// separators or parent-directory escapes. The argument is then joined
/// with `<project>/.claude/knowledge/` to produce the final path —
/// rejecting traversal patterns here keeps the security backbone (per
/// `resolve_project_root`) intact for the combined path.
pub fn validate_db_name(name: &str) -> Result<&str, &'static str> {
    if name.is_empty() {
        return Err("db_name must not be empty");
    }
    if !name.ends_with(".db") {
        return Err("db_name must end with `.db`");
    }
    if name.contains('/') || name.contains('\\') || name.contains("..") || name.starts_with('.') {
        // Allow only well-formed *.db names; reject anything with separators,
        // double-dots, or hidden-file prefixes. The `.db` suffix dot is fine;
        // the leading-dot check is for paths like `.malicious.db`.
        return Err("db_name must be a bare filename ending in `.db`");
    }
    Ok(name)
}

#[derive(Args, Debug)]
pub struct SearchArgs {
    /// Query string.
    pub query: String,
    #[arg(long, default_value_t = 5)]
    pub top_k: usize,
    /// Expand each hit with ±N neighbor chunks from the same document so the
    /// agent gets paragraph-level context around the BM25 match. Default 0
    /// (backward-compat — no expansion). Capped at 10. With N=1 each hit
    /// returns ~1500 chars of context (3 chunks × ~500 chars); N=2 ≈ 2500
    /// chars; N=3 ≈ 3500 chars. The matching chunk's `chunk_id` and `score`
    /// remain unchanged — context is additive in the new `context` JSON
    /// field, omitted when N=0.
    #[arg(long, default_value_t = 0)]
    pub context: usize,
    /// Search mode: `lexical` (BM25 FTS5), `dense` (sqlite-vec K-NN), or
    /// `hybrid` (BM25 ⊕ dense via RRF k=60). Default `hybrid` — auto-falls-back
    /// to lexical when the e5 encoder model or chunks_vec virtual table is
    /// unavailable, with a warning printed to stderr.
    #[arg(long, value_enum, default_value_t = SearchMode::Hybrid)]
    pub mode: SearchMode,
    #[arg(long)]
    pub project_root: Option<PathBuf>,
    /// Corpus file (under `<project>/.claude/knowledge/`). Default `index.db`.
    #[arg(long, default_value = DEFAULT_DB_NAME)]
    pub db_name: String,
    #[arg(long)]
    pub json: bool,
}

#[derive(Args, Debug)]
pub struct ListArgs {
    #[arg(long)]
    pub project_root: Option<PathBuf>,
    /// Corpus file — see `search --db-name`.
    #[arg(long, default_value = DEFAULT_DB_NAME)]
    pub db_name: String,
    #[arg(long)]
    pub json: bool,
}

#[derive(Args, Debug)]
pub struct StatusArgs {
    #[arg(long)]
    pub project_root: Option<PathBuf>,
    /// Corpus file — see `search --db-name`.
    #[arg(long, default_value = DEFAULT_DB_NAME)]
    pub db_name: String,
    #[arg(long)]
    pub json: bool,
}

#[derive(Args, Debug)]
pub struct WarmupArgs {
    /// Suppress success output; only stderr warnings on failure.
    #[arg(long)]
    pub quiet: bool,
}

/// `claudebase page <doc> <page>` — fetch raw text of a specific page
/// from a specific document, exposing the LLM-navigable page-flip surface
/// described in Slice 12 of vector-retrieval-backend. Page numbering is
/// pdfium 1-indexed; out-of-range page numbers exit 1 with the literal
/// stderr line `error: page number out of range`.
#[derive(Args, Debug)]
pub struct PageArgs {
    /// Document identifier — either an integer `documents.id` (returned
    /// in `claudebase list --json`) OR a string matching `documents.source_path`
    /// by basename (e.g. `Mastering LangChain.pdf`).
    pub doc: String,
    /// 1-indexed page number per the pdfium convention. Independent of
    /// any "printed" numbering the document might use (Roman vs Arabic
    /// for preface vs body) — always counts physical pages 1..N.
    pub page: i64,
    /// Fetch ±N neighbor pages around `page` so the LLM can see a
    /// page-spread instead of a single page. Default 0 (single page).
    /// Capped at 20 (40-page neighborhood) for safety.
    #[arg(long, default_value_t = 0)]
    pub range: i64,
    #[arg(long)]
    pub project_root: Option<PathBuf>,
    /// Emit JSON `{doc, total_pages, pages: [{page_no, text}, …]}` instead
    /// of the human-readable concatenated form.
    #[arg(long)]
    pub json: bool,
}

/// `claudebase reindex-pages` — backfill the `pages` table for documents
/// already ingested under v2 schema (i.e., chunks + embeddings populated
/// but pages table empty). Re-parses each PDF via pdfium and populates
/// pages without touching chunks_fts or chunks_vec — preserves existing
/// embeddings + BM25 index. Idempotent: re-runs replace existing pages
/// rows for each document.
#[derive(Args, Debug)]
pub struct ReindexPagesArgs {
    /// Restrict backfill to a specific document (basename or integer id).
    /// When omitted, backfills every document whose source_path is still
    /// readable on disk.
    #[arg(long = "doc")]
    pub doc: Option<String>,
    #[arg(long)]
    pub project_root: Option<PathBuf>,
    /// Emit JSON summary `{succeeded: [...], skipped: [...], failed: [...]}`
    /// instead of human text.
    #[arg(long)]
    pub json: bool,
}

/// `claudebase compare <query>` — A/B-test all 3 search modes side-by-side.
/// Runs the same query through lexical / dense / hybrid and prints the
/// FULL chunk text (not the FTS5 snippet) for each hit so the operator
/// can judge retrieval quality + see exactly what would be sent to an
/// LLM as context-augmentation input.
#[derive(Args, Debug)]
pub struct CompareArgs {
    /// Query string to A/B test across modes.
    pub query: String,
    /// Top-K hits per mode (default 5).
    #[arg(long, default_value_t = 5)]
    pub top_k: usize,
    /// Expand each hit with ±N neighbor chunks from the same document so the
    /// preview shows about a page of context around the matched text.
    /// Chunks are ~500 chars (sliding-window fallback) or up to 1500 chars
    /// (heading-aware structural). At `--context 2` each hit returns 5
    /// chunks ≈ 2500 chars ≈ one printed page. Default 2 ("page-ish");
    /// pass `--context 0` for the bare matched chunk only. Capped at 10
    /// (search.rs MAX_CONTEXT_RADIUS).
    #[arg(long, default_value_t = 2)]
    pub context: usize,
    /// Truncate the assembled text (chunk + neighbors when --context > 0)
    /// to this many chars (0 = no truncation). Default 1500 ≈ one printed
    /// page — readable in a terminal AND fits comfortably in an LLM context
    /// window without overwhelming it. Pass `--max-chars 0` for the full
    /// assembled blob (when `--context 2` that's ~2500 chars).
    #[arg(long, default_value_t = 1500)]
    pub max_chars: usize,
    #[arg(long)]
    pub project_root: Option<PathBuf>,
    /// Emit JSON instead of human-readable side-by-side blocks.
    #[arg(long)]
    pub json: bool,
}

#[derive(Args, Debug)]
pub struct DeleteArgs {
    /// Source path (legacy positional form; mutually exclusive with `--by-id`).
    pub source_path: Option<String>,
    /// Delete by integer document id (mutually exclusive with positional `<source-path>`).
    #[arg(long = "by-id")]
    pub by_id: Option<i64>,
    #[arg(long)]
    pub project_root: Option<PathBuf>,
    /// Corpus file — see `search --db-name`.
    #[arg(long, default_value = DEFAULT_DB_NAME)]
    pub db_name: String,
    #[arg(long)]
    pub json: bool,
}

#[derive(Subcommand, Debug)]
pub enum Command {
    /// Ingest a file or directory into the knowledge base.
    Ingest(IngestArgs),
    /// Search the knowledge base with a BM25-ranked query.
    Search(SearchArgs),
    /// List ingested sources.
    List(ListArgs),
    /// Show knowledge base status (counts, size, schema version).
    Status(StatusArgs),
    /// Delete a source by ID.
    Delete(DeleteArgs),
    /// Pre-download the e5-multilingual-small encoder model so the first
    /// `ingest` / `search --mode hybrid` doesn't pay a 30-second cold-start
    /// model-download stall. Idempotent: re-runs are no-ops once the
    /// model is cached at `~/.claude/tools/claudebase/models/`. Network
    /// failures (offline install, HF rate limit) are warnings, not errors —
    /// fastembed falls back to lazy download on first real use.
    Warmup(WarmupArgs),
    /// A/B-test all three search modes (lexical / dense / hybrid) for the
    /// same query, side-by-side, with FULL chunk text so the operator can
    /// judge retrieval quality + preview exactly what an LLM would receive
    /// as context-augmentation input.
    Compare(CompareArgs),
    /// Fetch raw text of a specific page from a specific document.
    /// Lets the LLM navigate the source book by page number when a search
    /// hit's chunk doesn't carry enough context. Page numbering is pdfium
    /// 1-indexed; out-of-range page exits 1 with `error: page number out of range`.
    Page(PageArgs),
    /// Backfill the `pages` table for documents already ingested under v2
    /// schema. Re-parses each PDF via pdfium and populates pages without
    /// touching chunks_fts / chunks_vec — preserves embeddings.
    ReindexPages(ReindexPagesArgs),
    /// The project journal: decisions, findings and updates (the changelog),
    /// recorded by agents and searched by tag and category. Held by the
    /// daemon, one per project.
    Journal(JournalArgs),
    /// The Claude subscription token of the machine whose daemon is in use:
    /// hand it over once, and every Claude started there is signed in.
    Auth(AuthArgs),
    /// A pool of sessions that answer jobs from a gateway
    /// (docs/design/pool-dispatch.md): the worker's commands, and the tokens and
    /// view for the machine whose daemon routes them.
    Pool(PoolArgs),
    /// Run as a persistent daemon — owns the local IPC surface (UDS on
    /// Unix, named pipe on Windows) for MCP plugin bridges and
    /// agent-chat. Slice 1a wires the accept loop + echo; richer
    /// subcommands (install/uninstall/start/stop/status/logs/config)
    /// land in Slice 2.
    Daemon(DaemonArgs),
    /// Chat introspection (Slice 3 of agent-chat-daemon). Reads
    /// `~/.claude/knowledge/chat.db` directly — daemon NOT required.
    /// Subcommands:
    ///   `chat list --thread X` — list messages in a thread (chronological)
    ///   `chat threads`         — list all known threads with counts
    ///   `chat clone <id|name> <name>` — copy a Claude Code conversation for /resume
    ///   `chat dump <id|name>`    — pack a conversation into a .zip for another machine
    ///   `chat load <file.zip>`   — unpack one here, resumable from this directory
    Chat(ChatArgs),
    /// Launch `claude` with the Telegram plugin preset and any extra args
    /// forwarded verbatim. Equivalent to:
    ///   `claude --channels plugin:telegram@claude-plugins-official <args...>`
    /// The SDLC SessionStart onboarding hook (if installed) auto-fires on
    /// session boot — nothing extra wired here. Exec replaces this process.
    Run(RunArgs),
    /// `claudebase telegram ...` — the agent-facing outbound channel
    /// (pty-transport Slice 1). Replaces the `chat_reply` MCP tool:
    ///   `telegram send --text "…"`  — reply to the operator
    ///   `telegram status`           — show the resolved default target
    Telegram(TelegramArgs),
    /// `claudebase agent ...` — cli-to-cli routing discovery surface
    /// (Slice 6 of cli-to-cli-routing). Reads chat.db directly so this
    /// works whether or not the daemon is running:
    ///   `agent list-alive --project current|all|<slug>` — peer discovery
    ///   `agent inspect <agent_id>` — registry snapshot + queue depth
    Agent(AgentArgs),
}

/// `claudebase telegram …` — outbound surface for the running agent.
#[derive(Args, Debug)]
pub struct TelegramArgs {
    #[command(subcommand)]
    pub sub: TelegramSubcommand,
}

#[derive(Subcommand, Debug)]
pub enum TelegramSubcommand {
    /// Send a message to the operator's Telegram chat.
    Send(TelegramSendArgs),
    /// Show which chat this session would send to, and why.
    Status,
    /// Register a Telegram bot token in the claudebase registry. The token is
    /// verified against Telegram before being stored, and re-adding the same
    /// bot rotates its token rather than duplicating the entry.
    Addbot(TelegramAddbotArgs),
    /// List registered bots (secrets are never printed).
    Bots,
    /// Ask Telegram to identify the registered bot (`getMe`) and print the raw
    /// API response. Verifies the token without waiting for silence in the
    /// channel. Named `get_me` to match Telegram's own method name; `get-me`
    /// is accepted as an alias for muscle memory.
    #[command(name = "get_me", alias = "get-me")]
    GetMe(TelegramGetMeArgs),
    /// Approve a pending pairing request by its code.
    Pair(TelegramPairArgs),
    /// Show the channel access gate: policy, allowlist, pending pairings.
    Access,
    /// Set the DM policy (`pairing` | `allowlist` | `disabled`).
    Policy(TelegramPolicyArgs),
    /// Add a numeric Telegram user id to the allowlist.
    Allow(TelegramSenderArgs),
    /// Remove a sender from the allowlist.
    Revoke(TelegramSenderArgs),
}

#[derive(Args, Debug)]
pub struct TelegramGetMeArgs {
    /// Check this token instead of the registered default bot.
    #[arg(long)]
    pub token: Option<String>,
}

#[derive(Args, Debug)]
pub struct TelegramPairArgs {
    /// Pairing code the bot replied with.
    pub code: String,
}

#[derive(Args, Debug)]
pub struct TelegramPolicyArgs {
    /// `pairing` (strangers get a code), `allowlist` (only listed senders),
    /// or `disabled` (drop all DMs).
    pub value: String,
}

#[derive(Args, Debug)]
pub struct TelegramSenderArgs {
    /// Numeric Telegram user id.
    pub sender_id: String,
}

#[derive(Args, Debug)]
pub struct TelegramAddbotArgs {
    /// Bot token from @BotFather (`<bot_id>:<secret>`).
    pub token: String,

    /// Optional human label, e.g. which project or persona the bot serves.
    #[arg(long)]
    pub label: Option<String>,
}

#[derive(Args, Debug)]
pub struct TelegramSendArgs {
    /// Message body. Mutually exclusive with `--stdin`.
    #[arg(long)]
    pub text: Option<String>,

    /// Read the message body from stdin instead of `--text`. Use for
    /// multi-line content — quoting a long block through a shell is where
    /// agent-authored commands break.
    #[arg(long, conflicts_with = "text")]
    pub stdin: bool,

    /// Target thread (`telegram:<chat_id>`). Omit to let the session's
    /// binding decide; see `telegram status`.
    #[arg(long)]
    pub thread: Option<String>,
}

#[derive(clap::Args, Debug)]
pub struct RunArgs {
    /// Disable the telegram channel preset (just `claude` with `args`).
    /// Useful when the official Anthropic plugin isn't installed yet.
    #[arg(long)]
    pub no_telegram: bool,

    /// Opt out of the default-on `--dangerously-skip-permissions` flag
    /// that `claudebase run` adds for ergonomics. The flag bypasses
    /// Claude Code's per-action permission prompts; opt out when you
    /// want the normal prompts back (e.g., running in a less-trusted
    /// context with channel notifications coming in).
    #[arg(long)]
    pub no_skip_permissions: bool,

    /// Opt out of the default-on `--chrome` flag (Claude in Chrome
    /// integration). Also implied when the forwarded args already carry
    /// `--chrome` or `--no-chrome`.
    #[arg(long)]
    pub no_chrome: bool,

    /// Name this session, instead of deriving it from the project (every
    /// session opened in one repo derives the same name). Shown in
    /// `claudebase agent list` and in the Telegram `/switch` menu.
    #[arg(long)]
    pub nick: Option<String>,

    /// Join the daemon of another machine instead of this one's: a profile
    /// from `claudebase daemon remote add`, or `local`. Default: the default
    /// profile, else local. Every claudebase command the session runs follows.
    #[arg(long)]
    pub daemon: Option<String>,

    /// Answer Claude Code's startup questions without an operator: today the
    /// folder-trust question ("Yes, I trust this folder" — the directory is
    /// trusted). Other dialogs are left alone and logged. Implied by --pool.
    #[arg(long)]
    pub no_input: bool,

    /// Make this session a worker of a pool: the daemon may hand it jobs
    /// (`[pool-job:<pool> <req_id> <role>]`) from a gateway, and it answers with
    /// `claudebase pool reply` (docs/design/pool-dispatch.md).
    #[arg(long)]
    pub pool: Option<String>,

    /// How many pool jobs this session takes at once.
    #[arg(long, default_value_t = 1)]
    pub pool_capacity: i64,

    /// Extra thread to subscribe to for this session, repeatable
    /// (`--subscribe telegram:123 --subscribe agent:foo`). Paired Telegram
    /// chats and known telegram threads are subscribed automatically; this is
    /// for testing and for threads the access file does not know about yet.
    #[arg(long)]
    pub subscribe: Vec<String>,

    /// Additional arguments forwarded verbatim to `claude`. Use `--` to
    /// separate them from claudebase's own flags:
    ///   `claudebase run -- --debug --add-dir /some/path`
    #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
    pub args: Vec<String>,
}

impl RunArgs {
    /// Everything `claudebase run` passes to `claude`: the default-on flags,
    /// then the forwarded args verbatim.
    pub fn claude_args(&self) -> Vec<String> {
        let mut argv = Vec::new();
        if !self.no_skip_permissions {
            argv.push("--dangerously-skip-permissions".to_owned());
        }
        let chrome_given = self.args.iter().any(|a| a == "--chrome" || a == "--no-chrome");
        if !self.no_chrome && !chrome_given {
            argv.push("--chrome".to_owned());
        }
        argv.extend(self.args.iter().cloned());
        argv
    }
}

/// `claudebase chat ...` — chat introspection subcommands (Slice 3).
#[derive(Args, Debug)]
pub struct ChatArgs {
    #[command(subcommand)]
    pub sub: ChatSubcommand,
    /// `--project-root` is accepted for argument-shape parity with
    /// sibling subcommands; chat.db is user-level so the resolved root
    /// is unused.
    #[arg(long, global = true)]
    pub project_root: Option<PathBuf>,
}

#[derive(Subcommand, Debug)]
pub enum ChatSubcommand {
    /// List messages in a thread (chronological order). Reads chat.db
    /// directly — daemon is NOT required.
    List(ChatListArgs),
    /// List all known threads with their message counts. Reads chat.db
    /// directly — daemon is NOT required.
    Threads(ChatThreadsArgs),
    /// Copy a Claude Code conversation under a new id and name. The copy
    /// shows up in `/resume` under that name and continues from the same
    /// context; the original is not touched. Run it from the directory the
    /// conversation was started in.
    Clone(ChatCloneArgs),
    /// Pack a Claude Code conversation — transcript, subagent transcripts,
    /// spilled tool results and /rewind backups — with its nick and the
    /// project's journal into one .zip, to carry it to another machine and
    /// `chat load` it there.
    Dump(ChatDumpArgs),
    /// Unpack a `chat dump` archive into this directory's conversations, so
    /// `/resume` run here lists it. Keeps the conversation's id; project paths
    /// recorded on the source machine are moved to this directory, and the
    /// journal goes into this directory's journal, whatever it is called.
    Load(ChatLoadArgs),
}

#[derive(Args, Debug)]
pub struct ChatDumpArgs {
    /// The conversation to pack: its UUID or the name it is listed under in
    /// `/resume`. Omitted inside a Claude Code session: the conversation that
    /// session is (`$CLAUDE_CODE_SESSION_ID`).
    pub source: Option<String>,
    /// Where to write the archive. Default: `<name>-<id8>.zip` in the current
    /// directory. Never overwrites an existing file.
    #[arg(long, short)]
    pub out: Option<PathBuf>,
}

#[derive(Args, Debug)]
pub struct ChatLoadArgs {
    /// The archive `chat dump` produced.
    pub archive: PathBuf,
    /// Load under a fresh id, as a separate copy — for a conversation that is
    /// already on this machine.
    #[arg(long)]
    pub new_id: bool,
    /// List the loaded conversation in `/resume` under this name instead.
    #[arg(long)]
    pub name: Option<String>,
}

#[derive(Args, Debug)]
pub struct ChatCloneArgs {
    /// The conversation to copy: its UUID (the transcript file name under
    /// `~/.claude/projects/<dir>/<uuid>.jsonl`) or the name it is listed under
    /// in `/resume`.
    pub source: String,
    /// The name the copy is listed under in `/resume`.
    pub name: String,
}

#[derive(Args, Debug)]
pub struct ChatListArgs {
    /// Thread identifier (e.g. `telegram:12345`).
    #[arg(long)]
    pub thread: String,
    /// Cap the number of returned messages.
    #[arg(long)]
    pub limit: Option<i64>,
    /// Only return messages with `created_at > <since>` (millis since
    /// UNIX epoch).
    #[arg(long)]
    pub since: Option<i64>,
    /// Emit JSON `{messages: [...]}` instead of human-readable text.
    #[arg(long)]
    pub json: bool,
}

#[derive(Args, Debug)]
pub struct ChatThreadsArgs {
    /// Emit JSON `{threads: [...]}` instead of human-readable text.
    #[arg(long)]
    pub json: bool,
}

/// `claudebase agent ...` — cli-to-cli routing discovery surface
/// (Slice 6 of cli-to-cli-routing).
#[derive(Args, Debug)]
pub struct AgentArgs {
    #[command(subcommand)]
    pub sub: AgentSubcommand,
}

#[derive(Subcommand, Debug)]
pub enum AgentSubcommand {
    /// List currently-alive agents, scoped by project. Reads chat.db
    /// directly — daemon is NOT required. Default scope is `current`
    /// (resolves `git remote origin URL` of cwd to a `project_id` and
    /// filters by it). Pass `--project all` for cross-project listing
    /// or `--project <slug>` for an explicit project_id.
    ///
    /// **last_seen_at semantics:** `last_pinged_at` is set at register
    /// time and is NOT refreshed by subsequent calls — it reflects the
    /// timestamp of the most recent connect-or-re-register, not last
    /// activity. CLI output labels the field accordingly.
    ListAlive(AgentListAliveArgs),
    /// Inspect a single agent_registry row: identity + DND state +
    /// queue depth of undelivered messages. Read-only. Exit 1 if the
    /// agent_id is not present in the registry.
    Inspect(AgentInspectArgs),
    /// Send a direct message to another Claude session on this daemon:
    /// `agent send "текст" --agent_nick <nick>`. Replaces the `agent_send`
    /// MCP tool so cli-to-cli routing survives the plugin bridge's removal.
    Send(AgentSendArgs),
    /// List every known session with nick, id and online/offline status.
    List(AgentListArgs),
    /// Publish what this session is working on, so peers see it in `agent list`.
    Describe(AgentDescribeArgs),
    /// Rename THIS session, so `/switch` and `--agent_nick` address it by a
    /// name you chose instead of the project name every session shares.
    Rename(AgentRenameArgs),
    /// Print this session's own nick, id, and whether the nick was chosen or
    /// merely derived from the directory name. Read-only; works without the
    /// daemon.
    Whoami,
    /// Tell the daemon which Claude conversation this session is, so the
    /// conversation's nick is restored.
    ///
    /// Run by the SessionStart hook, not by hand: the conversation id only
    /// exists once `claude` is running, which is after the session has already
    /// been named.
    #[command(name = "bind-session")]
    BindSession(AgentBindSessionArgs),
}

#[derive(Args, Debug)]
pub struct AgentBindSessionArgs {
    /// The conversation id. Usually omitted in favour of `--stdin`.
    #[arg(long = "session-id")]
    pub session_id: Option<String>,

    /// Read the SessionStart hook's JSON payload from stdin and take
    /// `session_id` out of it.
    #[arg(long, conflicts_with = "session_id")]
    pub stdin: bool,
}

#[derive(Args, Debug)]
pub struct AgentSendArgs {
    /// Message body, as a positional argument:
    /// `claudebase agent send "текст" --agent_nick mira`.
    pub text: Option<String>,

    /// Read the message body from stdin (multi-line content).
    #[arg(long, conflicts_with = "text")]
    pub stdin: bool,

    /// Target session by NICK, as shown by `claudebase agent list`. Ambiguity
    /// is an error listing the candidates, never a silent pick.
    #[arg(long = "agent_nick", alias = "agent")]
    pub agent_nick: Option<String>,

    /// Target session by exact agent id — use when two live sessions share a
    /// nick.
    #[arg(long = "agent_id", alias = "agent-id", conflicts_with = "agent_nick")]
    pub agent_id: Option<String>,
}

#[derive(Args, Debug)]
pub struct AgentRenameArgs {
    /// New nick, e.g. `backend` or `ui-refactor`.
    pub nick: String,

    /// Issue a FRESH callback token for the new nick instead of carrying the
    /// existing one over.
    ///
    /// The default is to carry it over, so scripts already pinging this session
    /// keep working — a rename usually means "this window is now called
    /// something else", not "this is a different session". Use this when the
    /// rename marks a change of purpose or hands the window to someone else, and
    /// anything holding the old token should stop being able to reach it.
    #[arg(long = "new-callback-token")]
    pub new_callback_token: bool,
}

#[derive(Args, Debug)]
pub struct AgentDescribeArgs {
    /// Short human label, e.g. "pty transport slice 6".
    pub description: String,

    /// Optional branch override; omit to keep the value captured at register.
    #[arg(long)]
    pub branch: Option<String>,
}

#[derive(Args, Debug)]
pub struct AgentListArgs {
    /// Show offline sessions too (default: online only).
    #[arg(long)]
    pub all: bool,

    /// Machine-readable output.
    #[arg(long)]
    pub json: bool,
}

#[derive(Args, Debug)]
pub struct AgentListAliveArgs {
    /// Project scope filter. One of `current` (default — resolves
    /// project_id from cwd's git remote URL), `all` (no filter),
    /// or a literal project_id slug to match exactly.
    #[arg(long, default_value = "current")]
    pub project: String,
    /// Emit JSON `[{...}]` instead of a human-readable table.
    #[arg(long)]
    pub json: bool,
}

#[derive(Args, Debug)]
pub struct AgentInspectArgs {
    /// Target agent_id to inspect.
    pub agent_id: String,
    /// Emit JSON `{...}` instead of human-readable text.
    #[arg(long)]
    pub json: bool,
}

/// `claudebase daemon ...` — daemon lifecycle subcommands. Slice 1a
/// only implements `serve`; the lifecycle wrappers (install, start,
/// stop, status, logs, config) land in Slice 2.
#[derive(Args, Debug)]
pub struct DaemonArgs {
    #[command(subcommand)]
    pub sub: DaemonSubcommand,
}

#[derive(Subcommand, Debug)]
pub enum DaemonSubcommand {
    /// Bind the UDS / named pipe and serve concurrent IPC connections.
    /// Single-instance enforced via fslock on `daemon.pid`.
    Serve(DaemonServeArgs),
    /// `daemon config edit` / `daemon config show` — manage daemon.toml.
    Config(DaemonConfigArgs),
    /// `daemon callback …` — the HTTP endpoint external systems use to write
    /// into a session's terminal input.
    Callback(DaemonCallbackArgs),
    /// `daemon remote …` — sessions on other machines joining this daemon,
    /// and this machine's sessions joining another's (named profiles).
    Remote(DaemonRemoteArgs),
    /// `daemon doctor [--asr]` — health-check runtime backends without
    /// performing actual work. Exit 0 = healthy, 1 = unhealthy. The
    /// `--asr` flag scopes to the ASR backend (the only doctor target
    /// in Slice 6-MVP; informational in v1).
    Doctor(DaemonDoctorArgs),
    /// `daemon warmup [--asr]` — pre-fetch models / open lazy resources
    /// so the first user-facing operation doesn't pay a cold-start
    /// stall. Slice 6-MVP wires the whisper model download.
    Warmup(DaemonWarmupArgs),
    /// Install the platform service unit (systemd user unit on Linux,
    /// launchd LaunchAgent on macOS, Windows Service on Windows).
    Install(DaemonInstallArgs),
    /// Remove the installed service unit. Preserves user data unless
    /// `--keep-data` is omitted (in which case chat.db / secrets.toml /
    /// daemon.toml are deleted; the books corpora, the project journals under
    /// `~/.claude/claudebase/journals/` and the runtime access.json under
    /// `~/.claude/channels/claudebase/` are preserved regardless).
    Uninstall(DaemonUninstallArgs),
    /// Start the installed daemon service.
    Start,
    /// Stop the daemon service.
    Stop,
    /// Restart the daemon service.
    Restart,
    /// Print daemon status. `--json` emits a stable JSON document.
    Status(DaemonStatusArgs),
    /// Tail / stream platform-appropriate daemon logs (journalctl /
    /// `log show` / `Get-WinEvent`). Slice 2 supports `--lines` and
    /// `--follow`; no `--grep` flag.
    Logs(DaemonLogsArgs),
}

#[derive(Args, Debug)]
pub struct DaemonInstallArgs {
    /// Skip the "existing unit differs; overwrite?" guard.
    #[arg(long)]
    pub yes: bool,
    /// Install the unit but do not start the service immediately.
    #[arg(long)]
    pub no_start: bool,
}

#[derive(Args, Debug)]
pub struct DaemonUninstallArgs {
    /// Skip the destructive-delete confirmation prompt.
    #[arg(long)]
    pub yes: bool,
    /// Preserve user data (chat.db, secrets.toml, daemon.toml).
    /// The books corpora and the project journals are preserved regardless.
    #[arg(long)]
    pub keep_data: bool,
}

#[derive(Args, Debug)]
pub struct DaemonStatusArgs {
    /// Emit JSON instead of human-readable text.
    #[arg(long)]
    pub json: bool,
}

#[derive(Args, Debug)]
pub struct DaemonLogsArgs {
    /// How many lines of recent log history to print.
    #[arg(long, default_value_t = 50)]
    pub lines: u32,
    /// Follow new log entries until interrupted.
    #[arg(long)]
    pub follow: bool,
}

#[derive(Args, Debug)]
pub struct DaemonDoctorArgs {
    /// Limit the health check to the ASR backend.
    #[arg(long)]
    pub asr: bool,
}

#[derive(Args, Debug)]
pub struct DaemonWarmupArgs {
    /// Limit warmup to the ASR backend (downloads model if missing).
    #[arg(long)]
    pub asr: bool,
}

/// `claudebase daemon serve` — no flags in Slice 1a. The runtime dir
/// is computed from `$XDG_RUNTIME_DIR` (Unix) or `$LOCALAPPDATA`
/// (Windows). Slice 2 adds `--config`, `--log-level`, etc.
#[derive(Args, Debug)]
pub struct DaemonServeArgs {}

/// `claudebase daemon config ...` — daemon.toml management subcommands.
#[derive(Args, Debug)]
pub struct DaemonConfigArgs {
    #[command(subcommand)]
    pub sub: DaemonConfigSubcommand,
}

#[derive(Subcommand, Debug)]
pub enum DaemonConfigSubcommand {
    /// Open daemon.toml in `$EDITOR` (defaults to `vi`). Re-parses on
    /// editor exit; refuses to keep malformed TOML — exits 1 with a
    /// parse-error message that includes the literal "TOML" or "parse"
    /// so TC-4.13's substring match catches it. SEC-16: invokes editor
    /// via `Command::new(editor).arg(path)` — NEVER via `sh -c`.
    Edit(DaemonConfigEditArgs),
    /// Print current configuration. Loads daemon.toml AND secrets.toml
    /// (with token masked to `"***"` per SEC-10). Human-readable TOML
    /// by default; `--json` switches to JSON for machine consumers.
    Show(DaemonConfigShowArgs),
    /// Print the absolute path of `daemon.toml`.
    ///
    /// Exists so nothing has to REPLICATE that path. The installers need to
    /// write `[asr] backend` into it, and a second copy of the resolution rule
    /// is how the model path ended up documented in one place and read from
    /// another for a whole release.
    Path,
}

#[derive(Args, Debug)]
pub struct DaemonConfigEditArgs {
    /// Project-root parity — unused (config is user-level).
    #[arg(long)]
    pub project_root: Option<PathBuf>,
}

#[derive(Args, Debug)]
pub struct DaemonConfigShowArgs {
    /// Project-root parity — unused (config is user-level).
    #[arg(long)]
    pub project_root: Option<PathBuf>,
    /// Emit JSON instead of TOML.
    #[arg(long)]
    pub json: bool,
}


#[derive(Args, Debug)]
pub struct JournalArgs {
    #[command(subcommand)]
    pub sub: JournalSubcommand,
}

#[derive(Args, Debug)]
pub struct PoolArgs {
    #[command(subcommand)]
    pub sub: PoolSubcommand,
}

#[derive(Subcommand, Debug)]
pub enum PoolSubcommand {
    /// Worker: fetch job <req_id> — writes payload.json and the images into a
    /// job directory on this machine and prints their paths.
    Job(PoolJobArgs),
    /// Worker: answer job <req_id>. The result JSON comes as an argument, on
    /// stdin (`-` or omitted), or as `--content <text>` meaning {"content": text}.
    Reply(PoolReplyArgs),
    /// Worker: give job <req_id> back with a reason.
    Fail(PoolFailArgs),
    /// Worker: take jobs again after a job expired on this session.
    Ready,
    /// Gateway tokens for `/pool/*` (this machine's daemon only).
    Token(PoolTokenArgs),
    /// The workers of a pool and their state (this machine's daemon only).
    Workers(PoolWorkersArgs),
}

#[derive(Args, Debug)]
pub struct PoolJobArgs {
    pub req_id: String,
    /// Where to write the job's files. Default: ~/.claude/claudebase/pool-jobs/<req_id>/
    #[arg(long)]
    pub dir: Option<PathBuf>,
}

#[derive(Args, Debug)]
pub struct PoolReplyArgs {
    pub req_id: String,
    /// The result as JSON; omitted or `-` reads stdin.
    pub result: Option<String>,
    /// Shorthand for the result {"content": "<text>"} — what a model call returns.
    #[arg(long, conflicts_with = "result")]
    pub content: Option<String>,
    /// Read the result JSON from a file — the dependable route from PowerShell,
    /// whose pipes can add a BOM or re-encode.
    #[arg(long, conflicts_with_all = ["result", "content"])]
    pub file: Option<PathBuf>,
    /// Send a `content` that looks like JSON but does not parse, as it is.
    /// Without this, `{color: red}` is refused: the gateway's caller runs a
    /// strict JSON.parse on `content`.
    #[arg(long)]
    pub raw: bool,
}

#[derive(Args, Debug)]
pub struct PoolFailArgs {
    pub req_id: String,
    pub reason: String,
}

#[derive(Args, Debug)]
pub struct PoolTokenArgs {
    #[command(subcommand)]
    pub sub: PoolTokenSubcommand,
}

#[derive(Subcommand, Debug)]
pub enum PoolTokenSubcommand {
    /// Issue a token for a gateway (shown once; re-issuing retires the old one).
    Issue(RemoteClientArgs),
    /// Revoke a gateway's token.
    Revoke(RemoteClientArgs),
    /// Gateways with a token, and their last use.
    List,
}

#[derive(Args, Debug)]
pub struct PoolWorkersArgs {
    pub pool: String,
}

#[derive(Args, Debug)]
pub struct AuthArgs {
    #[command(subcommand)]
    pub sub: AuthSubcommand,
}

/// Goes to the daemon in use — this machine's, or the one `--daemon` /
/// `CLAUDEBASE_DAEMON` names — and is applied on THAT machine.
#[derive(Subcommand, Debug)]
pub enum AuthSubcommand {
    /// Give that machine a token from `claude setup-token`. Read from stdin
    /// when omitted or `-`, so it stays out of the shell history. Stored with
    /// the daemon's secrets, passed to every `claudebase run` there, and set in
    /// the user's environment (Windows: HKCU\Environment; Linux/macOS: login
    /// shells and systemd user sessions) for a bare `claude`.
    #[command(name = "setup-token", alias = "setuptoken")]
    SetupToken(AuthSetupTokenArgs),
    /// Whether that machine has a token: its last four characters, when and by
    /// whom it was set, where it was applied. Never the token itself.
    Status,
    /// Remove the token from that machine: secrets, environment, profiles.
    #[command(name = "clear-token")]
    ClearToken,
}

#[derive(Args, Debug)]
pub struct AuthSetupTokenArgs {
    pub token: Option<String>,
}

/// The project journal. Every subcommand goes through the daemon, which owns
/// the journals (`~/.claude/claudebase/journals/`) and decides which project a
/// call belongs to: the directory `claudebase run` was started in. Nothing is
/// ever read from or written to another project's journal.
#[derive(Subcommand, Debug)]
pub enum JournalSubcommand {
    /// Record a decision, a finding, an update — or, with a flag, a todo, an
    /// open question, a problem, a reminder, a hypothesis (`--unverified`), a
    /// constraint. Needs a session started with `claudebase run`. A
    /// `journal#N` in the text links the two entries. The same text in the
    /// same category within 30 days is stored once (its new tags and flags
    /// are merged in).
    Add(JournalAddArgs),
    /// Find earlier entries: BM25 plus semantic search, fused. Filters by tag,
    /// category and the rest are exact.
    Search(JournalSearchArgs),
    /// Entries newest first, a page at a time, each with its flags and links.
    List(JournalListArgs),
    /// One entry in full, with what it intersects: the entries it cites,
    /// replaces or fixes and those that do so to it, the closest entries by
    /// meaning, and those from the same source.
    Get(JournalGetArgs),
    /// Delete one entry. Needs a session started with `claudebase run`.
    Delete(JournalIdArgs),
    /// Tags used in this project's journal, most used first.
    Tags(JournalTagsArgs),
    /// Categories used in this project's journal, most used first.
    Categories(JournalJsonArgs),
    /// Apply retention now (the daemon also does it daily): salience `low`
    /// entries live 90 days, `medium` 365, `high` forever.
    Gc(JournalGcArgs),
    /// Every project journal the daemon holds, with entry counts.
    Projects(JournalJsonArgs),
    /// Bring in the journal packed in a `chat dump` archive, into the journal
    /// of the current directory. `chat load` does this itself; this is for when
    /// the daemon was not running then. Loading twice writes nothing new.
    Import(JournalImportArgs),
    /// Flag an existing entry: `journal mark 37 --todo`. Needs a session.
    Mark(JournalFlagIdArgs),
    /// Remove flags from an entry, open or closed. Needs a session.
    Unmark(JournalFlagIdArgs),
    /// Close an entry's open work items (todo, question, problem, reminder,
    /// unverified, disputed, ai-feedback) — the named ones, or all of them —
    /// saying how (`--resolution`). A todo closes only once its branch holds a
    /// `--verified` entry, and then closes the whole branch. Needs a session.
    Close(JournalCloseArgs),
    /// A todo and the work under it: the entries that implement, verify, fix
    /// or cite it, downstream, and whether it is open, verified or closed.
    /// Give the todo or any entry of its branch.
    Branch(JournalIdArgs),
    /// Open closed work items of an entry again. Needs a session.
    Reopen(JournalFlagIdArgs),
    /// Everything still open: work items, constraints and important entries.
    Agenda(JournalAgendaArgs),
    /// Link one entry to another when its text does not already say so
    /// (`journal#N` in an entry is linked by itself). Needs a session.
    Link(JournalLinkArgs),
    /// Remove a link. Needs a session.
    Unlink(JournalUnlinkArgs),
    /// Make another folder (a worktree, a branch checkout, any directory) use
    /// THIS folder's journal from now on — one live journal both write to.
    /// Its own journal, if any, is merged in first and moved aside. An agent
    /// runs this only when the operator asks (skill claudebase-journal-share).
    Share(JournalShareArgs),
    /// Stop a folder sharing: it takes a copy of the shared journal, as it is
    /// now, as its own, and the two diverge from then on.
    Unshare(JournalShareArgs),
    /// Every shared folder and the journal it writes into.
    Shares(JournalJsonArgs),
    /// The operator's view of every project's `ai-feedback` entries — agents'
    /// suggestions for claudebase itself. Open ones unless --all. Refused from
    /// a session: it reads every project's journal.
    Feedback(JournalFeedbackArgs),
    /// This project's journal policy. `unverified` is lifted only by a
    /// separate entry that verifies it (`Verifies journal#N: ...`) and says
    /// what was checked, how and how many times; evidence-first also needs
    /// `--proof` on it.
    Policy(JournalPolicyArgs),
}

#[derive(Args, Debug)]
pub struct JournalShareArgs {
    /// The other folder.
    pub path: PathBuf,
    #[arg(long)]
    pub json: bool,
}

#[derive(Args, Debug)]
pub struct JournalFeedbackArgs {
    /// Closed ones too.
    #[arg(long)]
    pub all: bool,
    #[arg(long)]
    pub json: bool,
}

#[derive(Args, Debug)]
pub struct JournalGetArgs {
    pub id: i64,
    /// How many of the closest entries by meaning to show.
    #[arg(long, default_value_t = 5)]
    pub similar: usize,
    #[arg(long)]
    pub json: bool,
}

#[derive(Clone, Copy, Debug, ValueEnum)]
pub enum LinkKind {
    Cites,
    Replaces,
    Fixes,
    Implements,
    Verifies,
}

impl LinkKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Cites => "cites",
            Self::Replaces => "replaces",
            Self::Fixes => "fixes",
            Self::Implements => "implements",
            Self::Verifies => "verifies",
        }
    }
}

#[derive(Args, Debug)]
pub struct JournalLinkArgs {
    /// The entry that refers.
    pub id: i64,
    /// The entry referred to.
    pub to: i64,
    /// `replaces` does not flag the older entry: mark it `--deprecated`
    /// yourself if it is no longer true.
    #[arg(long, value_enum, default_value_t = LinkKind::Cites)]
    pub kind: LinkKind,
    #[arg(long)]
    pub json: bool,
}

#[derive(Args, Debug)]
pub struct JournalUnlinkArgs {
    pub id: i64,
    pub to: i64,
    /// Only this kind; without it, every link from `id` to `to`.
    #[arg(long, value_enum)]
    pub kind: Option<LinkKind>,
    #[arg(long)]
    pub json: bool,
}

/// The flags an entry can carry (`journal::FLAGS`). Work items — todo,
/// question, problem, reminder, unverified, disputed, ai-feedback — stay open
/// until closed and keep the entry past retention while open. Marks —
/// constraint, important, deprecated — are removed with `unmark`.
#[derive(Args, Debug, Default, Clone)]
pub struct JournalFlags {
    /// Something to do.
    #[arg(long)]
    pub todo: bool,
    /// An open question.
    #[arg(long)]
    pub question: bool,
    /// An unresolved problem.
    #[arg(long)]
    pub problem: bool,
    /// Something to come back to.
    #[arg(long)]
    pub reminder: bool,
    /// A hypothesis or assumption not yet checked; close it once it is.
    #[arg(long)]
    pub unverified: bool,
    /// Contradicts another entry; close it once settled.
    #[arg(long)]
    pub disputed: bool,
    /// An agent's suggestion for improving claudebase itself.
    #[arg(long = "ai-feedback")]
    pub ai_feedback: bool,
    /// A rule to follow, not to weigh.
    #[arg(long)]
    pub constraint: bool,
    /// An important note.
    #[arg(long)]
    pub important: bool,
    /// Carries the evidence that a fix or feature works; lets its todo's
    /// branch close.
    #[arg(long)]
    pub verified: bool,
    /// No longer true: kept for history, ranked last in a search.
    #[arg(long)]
    pub deprecated: bool,
}

impl JournalFlags {
    pub fn names(&self) -> Vec<&'static str> {
        [
            (self.todo, "todo"),
            (self.question, "question"),
            (self.problem, "problem"),
            (self.reminder, "reminder"),
            (self.unverified, "unverified"),
            (self.disputed, "disputed"),
            (self.ai_feedback, "ai-feedback"),
            (self.constraint, "constraint"),
            (self.important, "important"),
            (self.verified, "verified"),
            (self.deprecated, "deprecated"),
        ]
        .into_iter()
        .filter_map(|(on, name)| on.then_some(name))
        .collect()
    }
}

#[derive(Args, Debug)]
pub struct JournalFlagIdArgs {
    pub id: i64,
    #[command(flatten)]
    pub flags: JournalFlags,
    #[arg(long)]
    pub json: bool,
}

#[derive(Args, Debug)]
pub struct JournalCloseArgs {
    pub id: i64,
    /// Close only these; without any, every open one.
    #[command(flatten)]
    pub flags: JournalFlags,
    /// How it was closed: the answer, the fix, the commit.
    #[arg(long)]
    pub resolution: Option<String>,
    /// Close a todo whose work was dropped, without a verified entry. Needs
    /// --resolution saying why.
    #[arg(long)]
    pub abandon: bool,
    #[arg(long)]
    pub json: bool,
}

#[derive(Args, Debug)]
pub struct JournalAgendaArgs {
    /// Entries shown per flag, newest first.
    #[arg(long, default_value_t = 20)]
    pub limit: usize,
    #[arg(long)]
    pub json: bool,
}

#[derive(Args, Debug)]
pub struct JournalImportArgs {
    /// The archive `chat dump` produced.
    pub archive: PathBuf,
    #[arg(long)]
    pub json: bool,
}

/// Filters shared by `search` and `list`.
#[derive(Args, Debug, Default)]
pub struct JournalFilterArgs {
    /// Only entries carrying ANY of these tags (repeatable).
    #[arg(long = "tag", alias = "tags")]
    pub tags: Vec<String>,
    /// Only this category.
    #[arg(long)]
    pub category: Option<String>,
    /// Only entries written by this agent.
    #[arg(long)]
    pub agent: Option<String>,
    #[arg(long, value_enum)]
    pub salience: Option<Salience>,
    /// Only entries for this feature slug.
    #[arg(long)]
    pub feature: Option<String>,
    /// Only entries newer than this: `<N>` + `s|m|h|d|w`, e.g. `30d`, `12h`.
    #[arg(long)]
    pub since: Option<String>,
    /// Only entries carrying ANY of these flags.
    #[command(flatten)]
    pub flags: JournalFlags,
    /// Only entries whose flag (the named ones, or any) is still open.
    #[arg(long, conflicts_with = "closed")]
    pub open: bool,
    /// Only entries whose flag (the named ones, or any) is closed.
    #[arg(long)]
    pub closed: bool,
}

/// `claudebase journal add "<text>" --category <c> --tag <t>`.
///
/// The text is the positional argument, or stdin when it is `-` or omitted
/// and stdin is not a terminal.
#[derive(Args, Debug)]
pub struct JournalAddArgs {
    pub body: Option<String>,
    /// What kind of entry: free text, normalized to lowercase-with-dashes.
    /// Reuse what `claudebase journal categories` lists before inventing one —
    /// typical: decision, finding, bug, lesson, assumption-falsified. With a
    /// flag and no category, the first flag is the category.
    #[arg(long)]
    pub category: Option<String>,
    /// Flags to set, open.
    #[command(flatten)]
    pub flags: JournalFlags,
    /// At least one (repeatable). Tags are how the entry is found again; reuse
    /// what `claudebase journal tags` lists.
    #[arg(long = "tag", alias = "tags")]
    pub tags: Vec<String>,
    /// Retention: `high` forever, `medium` 365 days, `low` 90 days.
    #[arg(long, value_enum, default_value_t = Salience::Medium)]
    pub salience: Salience,
    /// The agent that wrote it (planner, reviewer, ...). Defaults to the
    /// session's nick.
    #[arg(long)]
    pub agent: Option<String>,
    /// Feature slug the entry belongs to.
    #[arg(long)]
    pub feature: Option<String>,
    /// Where it came from: a file, `docs/PRD.md#FR-7`, a commit, a URL.
    #[arg(long = "source-artifact")]
    pub source_artifact: Option<String>,
    /// Where the evidence this entry describes is (repeatable): a screenshot,
    /// a log, a file, a URL. Required on the entry that lifts `unverified`
    /// when the project is evidence-first (`journal policy`).
    #[arg(long = "proof")]
    pub proofs: Vec<String>,
    /// Bring this entry back into this session's input every interval
    /// (`20m`, `2h`, `1d`) as `[reminder #N]: ...`, until it is closed
    /// (`journal close N`). Sets `--reminder`. Only the session that wrote it
    /// gets it, and only while it is online; one due while it was away comes
    /// when it is back. Without --every a reminder is a plain work item.
    #[arg(long, value_parser = parse_every)]
    pub every: Option<i64>,
    #[arg(long)]
    pub json: bool,
}

#[derive(Args, Debug)]
pub struct JournalPolicyArgs {
    /// Turn evidence-first on or off: evidence entries must then point at
    /// proofs (`--proof`), not only describe them. The operator's call, so
    /// it is refused from a session. Without it, the policy is shown.
    #[arg(long = "evidence-first", value_parser = ["on", "off"])]
    pub evidence_first: Option<String>,
    #[arg(long)]
    pub json: bool,
}

#[derive(Args, Debug)]
pub struct JournalSearchArgs {
    pub query: String,
    #[command(flatten)]
    pub filter: JournalFilterArgs,
    #[arg(long, default_value_t = 5)]
    pub top_k: usize,
    /// `hybrid` (default) falls back to `lexical` until the e5 model is installed.
    #[arg(long, value_enum, default_value_t = SearchMode::Hybrid)]
    pub mode: SearchMode,
    #[arg(long)]
    pub json: bool,
}

#[derive(Args, Debug)]
pub struct JournalListArgs {
    #[command(flatten)]
    pub filter: JournalFilterArgs,
    /// Page number, 0 = newest.
    #[arg(long, default_value_t = 0)]
    pub page: usize,
    /// Entries per page, at most 100.
    #[arg(long, default_value_t = 10)]
    pub page_size: usize,
    #[arg(long)]
    pub json: bool,
}

#[derive(Args, Debug)]
pub struct JournalIdArgs {
    pub id: i64,
    #[arg(long)]
    pub json: bool,
}

#[derive(Args, Debug)]
pub struct JournalTagsArgs {
    /// Only tags used within this category.
    #[arg(long)]
    pub category: Option<String>,
    #[arg(long)]
    pub json: bool,
}

#[derive(Args, Debug)]
pub struct JournalGcArgs {
    /// Count what would go, delete nothing.
    #[arg(long)]
    pub dry_run: bool,
    #[arg(long)]
    pub json: bool,
}

#[derive(Args, Debug)]
pub struct JournalJsonArgs {
    #[arg(long)]
    pub json: bool,
}

/// A reminder interval: the same syntax as `--since`.
pub fn parse_every(value: &str) -> Result<i64, String> {
    parse_since(value).map_err(|e| e.replace("--since", "--every"))
}

/// Parse a relative-time filter like `30d` / `12h` / `90m` into seconds.
///
/// Returns `Err(...)` for malformed input (empty, no unit, unknown unit,
/// non-numeric prefix, overflow). The numeric prefix is a `u64` so values
/// up to ~292B seconds (~9000y) parse cleanly; the practical upper bound
/// is the timestamp space itself.
pub fn parse_since(value: &str) -> Result<i64, String> {
    if value.is_empty() {
        return Err("--since value is empty".to_string());
    }
    let (num_part, unit) = match value.chars().last() {
        Some(c) if !c.is_ascii_digit() => (&value[..value.len() - c.len_utf8()], c),
        _ => return Err(format!("--since must end with unit (s|m|h|d|w); got `{value}`")),
    };
    if num_part.is_empty() {
        return Err(format!("--since numeric prefix is empty; got `{value}`"));
    }
    let n: u64 = num_part
        .parse()
        .map_err(|_| format!("--since numeric prefix must be a positive integer; got `{value}`"))?;
    let seconds_per_unit: u64 = match unit {
        's' => 1,
        'm' => 60,
        'h' => 3_600,
        'd' => 86_400,
        'w' => 7 * 86_400,
        other => {
            return Err(format!(
                "--since unit must be one of s|m|h|d|w; got `{other}` in `{value}`"
            ));
        }
    };
    let total = n
        .checked_mul(seconds_per_unit)
        .ok_or_else(|| format!("--since value overflows i64 seconds: {value}"))?;
    i64::try_from(total).map_err(|_| format!("--since value overflows i64 seconds: {value}"))
}

#[derive(clap::Parser, Debug)]
#[command(
    name = "claudebase",
    version,
    about = "Local knowledge base CLI for SDLC agents"
)]
pub struct Cli {
    #[command(subcommand)]
    pub command: Command,
}

// ---------------------------------------------------------------------------
// Unit tests for resolve_project_root (TOCTOU discipline + canonical PathBuf).
// ---------------------------------------------------------------------------
#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    /// The working directory is per-PROCESS, not per-thread, and cargo runs the
    /// tests in one binary on parallel threads — so a test that changes it
    /// changes it for every other test running at that moment.
    ///
    /// A comment here used to claim these tests "run serially per Cargo
    /// defaults", which is not true and is exactly why
    /// `resolve_default_returns_canonical_cwd` failed on 2026-08-17 seeing
    /// `/tmp/.tmpSCg9eY` where it expected the repository root: the sibling test
    /// had cwd pointed at its tempdir at that instant. Any test touching cwd
    /// takes this lock.
    static CWD_LOCK: Mutex<()> = Mutex::new(());

    #[test]
    fn resolve_returns_canonical_pathbuf_for_dot() {
        let _guard = CWD_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        let tmp = tempfile::tempdir().expect("tempdir");
        let prev = std::env::current_dir().expect("cwd");

        std::env::set_current_dir(tmp.path()).expect("set cwd");

        let resolved = resolve_project_root(Some(Path::new("."))).expect("resolve `.`");
        let expected = std::fs::canonicalize(tmp.path()).expect("canonicalize tmp");

        // Restore BEFORE asserting: a panic here would otherwise leave every
        // later test running from a deleted tempdir.
        std::env::set_current_dir(prev).expect("restore cwd");

        assert_eq!(resolved, expected);
        assert!(resolved.is_absolute(), "resolved path must be absolute");
    }

    #[test]
    fn resolve_default_returns_canonical_cwd() {
        let _guard = CWD_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        let resolved = resolve_project_root(None).expect("resolve default");
        let cwd = std::env::current_dir().expect("cwd");
        let canonical = std::fs::canonicalize(&cwd).expect("canonicalize cwd");
        assert_eq!(resolved, canonical);
    }
}

#[derive(Args, Debug)]
pub struct DaemonCallbackArgs {
    #[command(subcommand)]
    pub sub: DaemonCallbackSubcommand,
}

#[derive(Subcommand, Debug)]
pub enum DaemonCallbackSubcommand {
    /// Start listening. Off by default: a token exists for every session from
    /// its first registration, but an open port is a separate, deliberate act.
    Enable(CallbackEnableArgs),
    /// Stop listening. Tokens are kept, so re-enabling does not break scripts.
    Disable,
    /// Bind address, token fingerprints, and whether the listener is up.
    Status,
    /// Print the token for a nick. Shows a fingerprint unless `--reveal`.
    Token(CallbackTokenArgs),
    /// Issue a new token for a nick; the previous one stops working at once.
    Rotate(CallbackTokenArgs),
}

#[derive(Args, Debug)]
pub struct DaemonRemoteArgs {
    #[command(subcommand)]
    pub sub: DaemonRemoteSubcommand,
}

/// Two sides. On the machine whose daemon is shared: `enable`, `issue`,
/// `revoke`, `tokens`. On a machine joining it: `add`, `list`, `remove`,
/// `default`, `test`, then `claudebase run --daemon <name>`.
#[derive(Subcommand, Debug)]
pub enum DaemonRemoteSubcommand {
    /// Listen for other machines (TLS; a self-signed certificate is generated,
    /// clients pin it). Takes effect on the daemon's next start.
    Enable(RemoteEnableArgs),
    /// Stop listening (tokens are kept). Takes effect on the daemon's next start.
    Disable,
    /// Whether this daemon listens, its certificate pin, and its client tokens.
    Status,
    /// Issue a token for a client machine and print the connection string to
    /// give it. Re-issuing for the same name retires the old token.
    Issue(RemoteIssueArgs),
    /// Revoke a client's token; its connections are refused from then on.
    Revoke(RemoteClientArgs),
    /// Add a daemon of another machine under a name, from the connection
    /// string `issue` printed there. Reads it from stdin when omitted or `-`,
    /// so the token stays out of the shell history.
    Add(RemoteAddArgs),
    /// The daemons this machine can join, and the default.
    List,
    /// Forget a daemon profile.
    Remove(RemoteNameArgs),
    /// The daemon `claudebase run` and every claudebase command use when none
    /// is named: a profile, or `local` for this machine's own.
    Default(RemoteNameArgs),
    /// Connect to a daemon profile (default: the default one) and report.
    Test(RemoteTestArgs),
}

#[derive(Args, Debug)]
pub struct RemoteEnableArgs {
    /// `host:port` to listen on.
    #[arg(long, default_value = "0.0.0.0:8586")]
    pub bind: String,
}

#[derive(Args, Debug)]
pub struct RemoteIssueArgs {
    /// A name for the client machine, e.g. `laptop`.
    pub client: String,
    /// The host clients reach this machine by. Default: this machine's name.
    #[arg(long)]
    pub host: Option<String>,
}

#[derive(Args, Debug)]
pub struct RemoteClientArgs {
    pub client: String,
}

#[derive(Args, Debug)]
pub struct RemoteAddArgs {
    /// Profile name, e.g. `prod`.
    pub name: String,
    /// `claudebase://host:port?pin=sha256:…&token=…`; omitted or `-` reads stdin.
    pub url: Option<String>,
}

#[derive(Args, Debug)]
pub struct RemoteNameArgs {
    pub name: String,
}

#[derive(Args, Debug)]
pub struct RemoteTestArgs {
    pub name: Option<String>,
}

#[derive(Args, Debug)]
pub struct CallbackEnableArgs {
    /// `host:port`. Defaults to loopback — a callback endpoint reachable from
    /// the network writes into a session running with permissions skipped.
    #[arg(long, default_value = "127.0.0.1:8585")]
    pub bind: String,

    /// Required to bind anything other than loopback. The token travels in
    /// cleartext over plain HTTP, so prefer a localhost bind and an SSH tunnel.
    #[arg(long = "i-know-this-is-remote")]
    pub allow_remote: bool,
}

#[derive(Args, Debug)]
pub struct CallbackTokenArgs {
    /// Session nick, as shown by `claudebase agent list`.
    pub nick: String,

    /// Print the token itself rather than its fingerprint. Whatever captures
    /// this command's output keeps the secret.
    #[arg(long)]
    pub reveal: bool,
}
