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

/// Corpus selector for the standalone `search` subcommand (Slice 6 of
/// agent-insights-base). `books` opens `index.db`, `insights` opens
/// `insights.db`, `all` opens BOTH and cross-corpus RRF-fuses ranked
/// hits with a `source_corpus` JSON field on each hit.
///
/// When `--corpus` is set it overrides `--db-name`. When both are set
/// the CLI emits a stderr warning and the `--corpus` selection wins —
/// this is the deliberate forward-compat path (tests that hardcode
/// `--db-name` continue to work; new agent prompts use `--corpus`).
#[derive(ValueEnum, Clone, Copy, Debug, PartialEq, Eq)]
pub enum Corpus {
    /// User-curated books / regulations / docs (the default — `index.db`).
    Books,
    /// Agent-written cognitive insights (`insights.db`).
    Insights,
    /// Cross-corpus RRF fusion of both — hits carry `source_corpus`.
    All,
}

/// Salience tag per cognitive-self-check rule. Drives TTL on the insights
/// corpus: `high` survives forever, `medium` 1 year, `low` 90 days. The
/// tag is stored verbatim as TEXT in `documents.salience` (schema v4).
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

/// Corpus-scope category for the insights corpus (schema v5). `general`
/// routes the insight to the cross-project GLOBAL db at
/// `$HOME/.claude/knowledge/insights.db`; `project` routes it to the current
/// project's LOCAL `insights.db`. The value is stored verbatim as TEXT in
/// `documents.category` and is the SOLE selector of which db a write lands in
/// — `--project <slug>` is data (a `project_slug` column value), never a path.
#[derive(ValueEnum, Clone, Copy, Debug, PartialEq, Eq)]
pub enum InsightCategory {
    /// Cross-project lesson → GLOBAL `$HOME/.claude/knowledge/insights.db`.
    General,
    /// Project-scoped lesson → the current project's LOCAL `insights.db`.
    Project,
}

impl InsightCategory {
    pub fn as_str(&self) -> &'static str {
        match self {
            InsightCategory::General => "general",
            InsightCategory::Project => "project",
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
/// `insights.db` is the agent-written insights corpus (slice 1+).
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
    if name.contains('/') || name.contains('\\') || name.contains("..") || name.starts_with('.') && name != "index.db" && name != "insights.db" {
        // Allow only well-formed *.db names; reject anything with separators,
        // double-dots, or hidden-file prefixes (except the two canonical names).
        // The `.db` suffix dot is fine; the leading-dot check is for paths
        // like `.malicious.db`. Permit `index.db` and `insights.db` explicitly.
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
    /// Corpus file (under `<project>/.claude/knowledge/`). Default `index.db`
    /// (user-curated books); `insights.db` for the agent-written insights corpus.
    /// Overridden by `--corpus` when both are set.
    #[arg(long, default_value = DEFAULT_DB_NAME)]
    pub db_name: String,
    /// Corpus selector (Slice 6): `books` (default), `insights`, or `all`.
    /// `all` runs hybrid search against both corpora and RRF-fuses ranks
    /// — each hit then carries a `source_corpus` field.
    #[arg(long, value_enum)]
    pub corpus: Option<Corpus>,
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
    /// Unified agent-insights subcommand tree (`create / search / list /
    /// random / get`). Operates exclusively on `insights.db` — the books
    /// corpus (`index.db`) is untouched. See
    /// docs/design/agent-insights-base.md.
    Insight(InsightArgs),
    /// Run as a persistent daemon — owns the local IPC surface (UDS on
    /// Unix, named pipe on Windows) for MCP plugin bridges and
    /// agent-chat. Slice 1a wires the accept loop + echo; richer
    /// subcommands (install/uninstall/start/stop/status/logs/config)
    /// land in Slice 2.
    Daemon(DaemonArgs),
    /// Claude Code MCP plugin entry point. Slice 1b implements the
    /// STDIO↔daemon bridge; Slice 1a ships a stub that errors out.
    Plugin(PluginArgs),
    /// Chat introspection (Slice 3 of agent-chat-daemon). Reads
    /// `~/.claude/knowledge/chat.db` directly — daemon NOT required.
    /// Subcommands:
    ///   `chat list --thread X` — list messages in a thread (chronological)
    ///   `chat threads`         — list all known threads with counts
    Chat(ChatArgs),
    /// Launch `claude` with the Telegram plugin preset and any extra args
    /// forwarded verbatim. Equivalent to:
    ///   `claude --channels plugin:telegram@claude-plugins-official <args...>`
    /// The SDLC SessionStart onboarding hook (if installed) auto-fires on
    /// session boot — nothing extra wired here. Exec replaces this process.
    Run(RunArgs),
}

#[derive(clap::Args, Debug)]
pub struct RunArgs {
    /// Disable the telegram channel preset (just `claude` with `args`).
    /// Useful when the official Anthropic plugin isn't installed yet.
    #[arg(long)]
    pub no_telegram: bool,

    /// Additional arguments forwarded verbatim to `claude`. Use `--` to
    /// separate them from claudebase's own flags:
    ///   `claudebase run -- --debug --add-dir /some/path`
    #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
    pub args: Vec<String>,
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
    /// `daemon access pair <code>` / `daemon access list` — manage access.json
    /// (telegram permission/pairing flow, Slice 4).
    Access(DaemonAccessArgs),
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
    /// daemon.toml / access.json are deleted; the books and insights
    /// corpora under `~/.claude/knowledge/` are preserved regardless).
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
    /// Preserve user data (chat.db, secrets.toml, daemon.toml,
    /// access.json). The books corpus and insights corpus under
    /// `~/.claude/knowledge/` are preserved regardless.
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

/// `claudebase daemon access ...` — access.json management subcommands.
#[derive(Args, Debug)]
pub struct DaemonAccessArgs {
    #[command(subcommand)]
    pub sub: DaemonAccessSubcommand,
}

#[derive(Subcommand, Debug)]
pub enum DaemonAccessSubcommand {
    /// `daemon access pair <CODE>` — redeem a pairing code emitted by the
    /// bot in response to `/start`. On success the corresponding telegram
    /// user id is added to access.json `allowFrom` and the pending entry
    /// is removed. SEC-16: code lookup uses constant-time compare; the
    /// error message does NOT distinguish "invalid format" from "unknown
    /// code" (both surface the same generic message).
    Pair(DaemonAccessPairArgs),
    /// `daemon access list` — print authorized users + pending-code count.
    /// Pending codes themselves are NEVER printed (would defeat SEC-16's
    /// constant-time compare). Output is JSON when `--json` is set.
    List(DaemonAccessListArgs),
}

#[derive(Args, Debug)]
pub struct DaemonAccessPairArgs {
    /// The 6-char pairing code the user received from the bot. Must match
    /// the base32-no-confusables alphabet `^[A-HJ-NP-Z2-9]{6}$`.
    pub code: String,
    #[arg(long)]
    pub project_root: Option<PathBuf>,
}

#[derive(Args, Debug)]
pub struct DaemonAccessListArgs {
    #[arg(long)]
    pub project_root: Option<PathBuf>,
    #[arg(long)]
    pub json: bool,
}

/// `claudebase plugin ...` — plugin subcommands. Slice 1a only exposes
/// `serve`; the stub returns an error so harness wiring is testable
/// without committing to the full MCP shape.
#[derive(Args, Debug)]
pub struct PluginArgs {
    #[command(subcommand)]
    pub sub: PluginSubcommand,
}

#[derive(Subcommand, Debug)]
pub enum PluginSubcommand {
    /// Bridge stdin/stdout JSON-RPC frames to the daemon UDS.
    Serve(PluginServeArgs),
}

/// `claudebase plugin serve` — no flags in Slice 1a. Slice 1b adds
/// `--daemon-socket` override etc.
#[derive(Args, Debug)]
pub struct PluginServeArgs {}

#[derive(Args, Debug)]
pub struct InsightArgs {
    #[command(subcommand)]
    pub sub: InsightSubcommand,
}

#[derive(Subcommand, Debug)]
pub enum InsightSubcommand {
    /// Persist a cognitive insight. Same body within (agent, sha256) over
    /// the last 30 days is deduplicated.
    Create(InsightCreateArgs),
    /// Vector + lexical search against the insights corpus (hybrid by
    /// default — BM25 ⊕ dense via RRF k=60, with auto-fallback to lexical
    /// when the e5 encoder is unavailable).
    Search(InsightSearchArgs),
    /// List insights newest-first, 10 per page. `--offset 0` = latest 10,
    /// `--offset 1` = next 10, and so on.
    List(InsightListArgs),
    /// Return one random insight uniformly sampled from the corpus.
    Random(InsightRandomArgs),
    /// Fetch one insight by integer `documents.id` or sha256 prefix
    /// (≥4 hex chars matches the stored sha256 via LIKE 'prefix%').
    Get(InsightGetArgs),
    /// Garbage-collect insights past their salience-driven TTL.
    /// salience=high retained indefinitely. medium retained 365 days.
    /// low retained 90 days. Runs VACUUM after delete.
    Gc(InsightGcArgs),
    /// Delete one insight by integer `documents.id` (with chunks +
    /// chunks_vec cascade). Refuses to delete non-insight rows.
    Delete(InsightDeleteArgs),
    /// Aggregate tag frequencies across the insights corpus. Default merges
    /// the cwd-local project db with the global general db (summing counts for
    /// tags present in both). `--category general` restricts to the global db,
    /// `--category project` to the cwd-local db, `--project <slug>` to a
    /// registered project's db (looked up in `~/.claude/knowledge/projects.json`)
    /// merged with the global db.
    Tags(InsightTagsArgs),
}

/// `claudebase insight create "<body>"` — agent write surface for the
/// insights corpus (schema v4). Persists one cognitive insight per call;
/// same body within the same `(agent, sha256)` over the last 30 days is
/// deduplicated by `find_recent_insight_by_sha`.
///
/// Body semantics:
///   - positional `<body>` literal string
///   - `-` as the positional → read stdin
///   - omitted positional with piped stdin → read stdin
///   - omitted positional on an interactive TTY → exits 2 with usage
#[derive(Args, Debug)]
pub struct InsightCreateArgs {
    /// Insight body. Pass `-` or omit (with piped stdin) to read from stdin.
    /// On an interactive TTY without a body, the command exits 2.
    pub body: Option<String>,

    /// Insight kind — open enum tied to docs/design/agent-insights-base.md.
    /// Examples: agent-learned, self-bias-caught, peer-bias-observed,
    /// red-team-objection, consolidator-drift, prediction-error,
    /// assumption-falsified, plan-reality-gap, reflection-observation,
    /// operator-correction.
    #[arg(long = "type")]
    pub kind: String,

    /// Emitting agent name (planner, reflection, consolidator, red-team, ...).
    #[arg(long)]
    pub agent: String,

    /// Claude Code session id for trace linking. Optional but recommended;
    /// when absent the field stays NULL.
    #[arg(long)]
    pub session: Option<String>,

    /// Feature slug this insight belongs to (matches .claude/plan.md feature).
    #[arg(long)]
    pub feature: Option<String>,

    /// Salience tag per cognitive-self-check rule; drives retention TTL.
    #[arg(long, value_enum, default_value_t = Salience::Medium)]
    pub salience: Salience,

    /// Path or anchor of the artifact the insight was extracted from
    /// (e.g. `.claude/plan.md#slice-3`, `docs/PRD.md#FR-7.2`).
    #[arg(long = "source-artifact")]
    pub source_artifact: Option<String>,

    /// Corpus-scope category — REQUIRED. `general` writes to the global
    /// cross-project db; `project` writes to the current project's local db.
    /// No default: clap exits 2 when absent (a category MUST be explicit so an
    /// agent never silently lands a cross-project lesson in a single project).
    #[arg(long, value_enum)]
    pub category: InsightCategory,

    /// Tag(s) for this insight — REPEATABLE, at least one required (the empty
    /// check is business-logic in `run_insight_create`, not a clap-level
    /// required-arg, so the operator-facing error names `--tag` explicitly).
    /// Each tag is normalized: a single leading `#` stripped, lowercased,
    /// trimmed; empties dropped, duplicates collapsed (stable order).
    #[arg(long = "tags")]
    pub tags: Vec<String>,

    /// Explicit project slug stored in `documents.project_slug` (DATA, never a
    /// path). For `--category project` it overrides the cwd-basename default;
    /// for `--category general` it is silently ignored (project_slug stays
    /// NULL). NEVER used to construct a filesystem path.
    #[arg(long)]
    pub project: Option<String>,

    #[arg(long)]
    pub project_root: Option<PathBuf>,

    /// Corpus file — `insights.db` by default. Tests/admin may override.
    #[arg(long, default_value = "insights.db")]
    pub db_name: String,

    #[arg(long)]
    pub json: bool,
}

/// `claudebase insight search "<query>"` — hybrid retrieval against
/// `insights.db`. Default mode is `hybrid` (BM25 ⊕ dense via RRF k=60);
/// auto-falls-back to `lexical` when the e5 encoder model or the
/// chunks_vec virtual table is unavailable.
///
/// Slice 4 filter args (`--type / --agent / --salience / --feature /
/// --since`) post-filter the ranked hits against the document metadata.
/// Implementation note: filters are applied AFTER ranking — `top_k` is
/// over-fetched (×4 cap 100) so the filter doesn't starve thin pages.
#[derive(Args, Debug)]
pub struct InsightSearchArgs {
    pub query: String,
    #[arg(long, default_value_t = 5)]
    pub top_k: usize,
    #[arg(long, default_value_t = 0)]
    pub context: usize,
    #[arg(long, value_enum, default_value_t = SearchMode::Hybrid)]
    pub mode: SearchMode,
    /// Filter by `documents.source_type` (exact match).
    #[arg(long = "type")]
    pub kind: Option<String>,
    /// Filter by `documents.agent_name` (exact match).
    #[arg(long)]
    pub agent: Option<String>,
    /// Filter by `documents.salience` (high|medium|low).
    #[arg(long, value_enum)]
    pub salience: Option<Salience>,
    /// Filter by `documents.feature_slug` (exact match).
    #[arg(long)]
    pub feature: Option<String>,
    /// Relative-time filter on `documents.ingested_at`. Format: `<N><unit>`
    /// where unit is `s|m|h|d|w` (seconds / minutes / hours / days / weeks).
    /// Examples: `30d`, `12h`, `90m`, `4w`. Rejected if no unit suffix.
    #[arg(long)]
    pub since: Option<String>,
    /// Tag filter (repeatable). OR / any-intersection semantics: an insight
    /// is kept if it carries ANY of the listed tags. `--tag nginx --tag docker`
    /// returns insights tagged nginx, docker, or both. Bound as parameters.
    #[arg(long)]
    pub tag: Vec<String>,
    /// Corpus-scope filter: `general` reads only the global db, `project`
    /// reads only the cwd-local db. Absent → both legs merged.
    #[arg(long, value_enum)]
    pub category: Option<InsightCategory>,
    /// Registry slug of a project whose db replaces the cwd-local leg (merged
    /// with the global db). Looked up in `~/.claude/knowledge/projects.json`;
    /// unknown slug → exit 1. DATA, never a raw filesystem path.
    #[arg(long)]
    pub project: Option<String>,
    /// Read only the global db (skip the local/project leg). Mutually
    /// exclusive with `--project-only` (both set → exit 2).
    #[arg(long)]
    pub general_only: bool,
    /// Read only the local/project db (skip the global leg). Mutually
    /// exclusive with `--general-only` (both set → exit 2).
    #[arg(long)]
    pub project_only: bool,
    #[arg(long)]
    pub project_root: Option<PathBuf>,
    #[arg(long, default_value = "insights.db")]
    pub db_name: String,
    #[arg(long)]
    pub json: bool,
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

#[derive(Args, Debug)]
pub struct InsightListArgs {
    /// Page index (0-based). Page size is fixed at 10 by default but
    /// overrideable via `--page-size` for batch-scripted exports.
    #[arg(long, default_value_t = 0)]
    pub offset: usize,
    /// Page size — number of insights per page. Default 10. Capped at 100.
    #[arg(long, default_value_t = 10)]
    pub page_size: usize,
    /// Optional filter on `documents.source_type` (exact match).
    #[arg(long = "type")]
    pub kind: Option<String>,
    /// Optional filter on `documents.agent_name` (exact match).
    #[arg(long)]
    pub agent: Option<String>,
    /// Optional filter on `documents.salience` (exact match: high|medium|low).
    #[arg(long, value_enum)]
    pub salience: Option<Salience>,
    /// Optional filter on `documents.feature_slug` (exact match).
    #[arg(long)]
    pub feature: Option<String>,
    /// Tag filter (repeatable, OR / any-intersection). See `InsightSearchArgs`.
    #[arg(long)]
    pub tag: Vec<String>,
    /// Corpus-scope filter: `general` (global db) or `project` (cwd-local db).
    #[arg(long, value_enum)]
    pub category: Option<InsightCategory>,
    /// Registry slug of a project whose db replaces the cwd-local leg.
    #[arg(long)]
    pub project: Option<String>,
    /// Read only the global db. Mutually exclusive with `--project-only`.
    #[arg(long)]
    pub general_only: bool,
    /// Read only the local/project db. Mutually exclusive with `--general-only`.
    #[arg(long)]
    pub project_only: bool,
    #[arg(long)]
    pub project_root: Option<PathBuf>,
    #[arg(long, default_value = "insights.db")]
    pub db_name: String,
    #[arg(long)]
    pub json: bool,
}

#[derive(Args, Debug)]
pub struct InsightRandomArgs {
    /// Optional filter on `documents.source_type` (exact match).
    #[arg(long = "type")]
    pub kind: Option<String>,
    /// Optional filter on `documents.agent_name` (exact match).
    #[arg(long)]
    pub agent: Option<String>,
    /// Optional filter on `documents.salience` (exact match: high|medium|low).
    #[arg(long, value_enum)]
    pub salience: Option<Salience>,
    /// Optional filter on `documents.feature_slug` (exact match).
    #[arg(long)]
    pub feature: Option<String>,
    /// Tag filter (repeatable, OR / any-intersection). See `InsightSearchArgs`.
    #[arg(long)]
    pub tag: Vec<String>,
    /// Corpus-scope filter: `general` (global db) or `project` (cwd-local db).
    #[arg(long, value_enum)]
    pub category: Option<InsightCategory>,
    /// Registry slug of a project whose db replaces the cwd-local leg.
    #[arg(long)]
    pub project: Option<String>,
    /// Read only the global db. Mutually exclusive with `--project-only`.
    #[arg(long)]
    pub general_only: bool,
    /// Read only the local/project db. Mutually exclusive with `--general-only`.
    #[arg(long)]
    pub project_only: bool,
    #[arg(long)]
    pub project_root: Option<PathBuf>,
    #[arg(long, default_value = "insights.db")]
    pub db_name: String,
    #[arg(long)]
    pub json: bool,
}

#[derive(Args, Debug)]
pub struct InsightGcArgs {
    /// Show what would be deleted without actually deleting. JSON output
    /// surfaces `{would_delete_medium: N, would_delete_low: N}`.
    #[arg(long)]
    pub dry_run: bool,
    /// Corpus scope: `general` gc's only the global db. Absent → gc BOTH the
    /// cwd-local db and the global db sequentially, combining their reports.
    #[arg(long, value_enum)]
    pub category: Option<InsightCategory>,
    #[arg(long)]
    pub project_root: Option<PathBuf>,
    #[arg(long, default_value = "insights.db")]
    pub db_name: String,
    #[arg(long)]
    pub json: bool,
}

#[derive(Args, Debug)]
pub struct InsightDeleteArgs {
    /// Integer `documents.id` of the insight to delete. (Sha-prefix
    /// targeting is not supported here — use `insight get <prefix>` to
    /// confirm the id first, then `insight delete <id>`.)
    pub id: i64,
    /// Corpus scope: `general` resolves the id against the global db. Absent
    /// (or `project`) → the cwd-local db (existing behavior).
    #[arg(long, value_enum)]
    pub category: Option<InsightCategory>,
    #[arg(long)]
    pub project_root: Option<PathBuf>,
    #[arg(long, default_value = "insights.db")]
    pub db_name: String,
    #[arg(long)]
    pub json: bool,
}

#[derive(Args, Debug)]
pub struct InsightGetArgs {
    /// Insight identifier — integer `documents.id` OR sha256 prefix
    /// (≥4 hex chars, matched as `sha256 LIKE '<prefix>%'`).
    pub ident: String,
    #[arg(long)]
    pub project_root: Option<PathBuf>,
    #[arg(long, default_value = "insights.db")]
    pub db_name: String,
    #[arg(long)]
    pub json: bool,
}

/// `claudebase insight tags` — tag-frequency aggregation over the insights
/// corpus (schema v5 `insight_tags` table).
///
/// DB selection:
///   - default (no flags)            → cwd-local db + global db (merged)
///   - `--category general`          → global db only
///   - `--category project`          → cwd-local db only
///   - `--project <slug>`            → registered project's db + global db
///
/// `--category` and `--project` are not combined: when `--project` is set the
/// registry lookup drives the local leg and `--category` is ignored. `--project`
/// is a registry KEY (data), looked up against the trusted
/// `~/.claude/knowledge/projects.json` file — its resolved path comes from that
/// trusted file, never joined from raw CLI input.
#[derive(Args, Debug)]
pub struct InsightTagsArgs {
    /// Restrict to one corpus scope: `general` (global db) or `project`
    /// (cwd-local db). Absent → merge both. Ignored when `--project` is set.
    #[arg(long, value_enum)]
    pub category: Option<InsightCategory>,
    /// Registry slug of a project whose db to query (merged with the global
    /// db). Looked up in `~/.claude/knowledge/projects.json`; absent slug →
    /// exit 1. DATA, never a raw filesystem path.
    #[arg(long)]
    pub project: Option<String>,
    #[arg(long)]
    pub project_root: Option<PathBuf>,
    /// Corpus file — `insights.db` by default. Tests/admin may override.
    #[arg(long, default_value = "insights.db")]
    pub db_name: String,
    #[arg(long)]
    pub json: bool,
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

    #[test]
    fn resolve_returns_canonical_pathbuf_for_dot() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let prev = std::env::current_dir().expect("cwd");

        // Note: setting cwd in tests is process-global; tests in this `cfg(test)`
        // module are intentionally minimal and run serially per Cargo defaults
        // for the same compilation unit. We restore cwd at the end.
        std::env::set_current_dir(tmp.path()).expect("set cwd");

        let resolved = resolve_project_root(Some(Path::new("."))).expect("resolve `.`");
        let expected = std::fs::canonicalize(tmp.path()).expect("canonicalize tmp");

        assert_eq!(resolved, expected);
        assert!(resolved.is_absolute(), "resolved path must be absolute");

        std::env::set_current_dir(prev).expect("restore cwd");
    }

    #[test]
    fn resolve_default_returns_canonical_cwd() {
        let resolved = resolve_project_root(None).expect("resolve default");
        let cwd = std::env::current_dir().expect("cwd");
        let canonical = std::fs::canonicalize(&cwd).expect("canonicalize cwd");
        assert_eq!(resolved, canonical);
    }
}
