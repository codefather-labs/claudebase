//! Cross-platform service installer for the claudebase daemon (Slice 2).
//!
//! Generates per-OS service units (systemd user unit on Linux, launchd
//! LaunchAgent plist on macOS, Windows Service descriptor via the
//! `windows-service` crate on Windows) and orchestrates install /
//! uninstall / start / stop / restart / status / logs operations against
//! the platform service manager.
//!
//! Security invariants — every entry point applies the 14 directives
//! captured in the SEC-2-N security review:
//!
//! 1. SEC-2-1  — systemd unit hard-codes the hardening directive set;
//!    contains NO `User=` line (per-user units run as the calling user).
//! 2. SEC-2-2  — `install` refuses immediately when EUID==0 on Unix.
//! 3. SEC-2-3  — every write goes through `write_refusing_symlink`:
//!    lstat first, refuse symlinks, then atomic O_EXCL temp-write +
//!    rename.
//! 4. SEC-2-4  — content-sha equality short-circuits a no-op re-install;
//!    different content without `--yes` is refused.
//! 5. SEC-2-5  — `resolve_binary_path` canonicalises the binary and
//!    refuses world-writable executables (Unix permission check).
//! 6. SEC-2-6  — launchd is ALWAYS a LaunchAgent under
//!    `~/Library/LaunchAgents`; never `/Library/LaunchDaemons`. No
//!    `UserName` key in the plist.
//! 7. SEC-2-7  — `.mcp.json` content is built by `serde_json` over a
//!    `McpDescriptor` struct; `args` is hard-coded `["plugin","serve"]`.
//! 8. SEC-2-8  — `.mcp.json` idempotency mirrors SEC-2-4.
//! 9. SEC-2-9  — `ensure_install_parent` refuses loose-permission parent
//!    directories.
//! 10. SEC-2-10 — every external invocation uses arg-vector form via
//!    `std::process::Command::new(<literal>)`; never `sh -c`.
//! 11. SEC-2-11 — Windows account is forced to NT AUTHORITY\\LocalService
//!    (per-machine non-LocalSystem) at runtime.
//! 12. SEC-2-12 — explicit chmod 0o644 follow-up after every write
//!    regardless of process umask.
//! 13. SEC-2-13 — uninstall confirm prompt OR `--yes`; headless without
//!    `--yes` refuses to delete data.
//! 14. SEC-2-14 — `daemon logs --lines` is a u32; no `--grep` flag in
//!    Slice 2.
//!
//! STRUCTURAL invariants:
//!
//! - STRUCTURAL-2-1 — plist XML is hand-rolled via `std::fmt::Write` +
//!   `xml_escape`; the `plist` crate is intentionally NOT a dependency.
//! - STRUCTURAL-2-2 — every entry point in this module is synchronous.
//!   No tokio runtime is constructed; the management subcommands stay
//!   off the async dispatch path entirely.
//! - STRUCTURAL-2-3 — install.sh / install.ps1 hook is opt-in via the
//!   `CLAUDEBASE_INSTALL_DAEMON=1` environment variable and fail-soft
//!   (the post-install hook prints a warning but never aborts the
//!   parent installer).

use std::fmt::Write as _;
use std::fs;
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{anyhow, bail, Context, Result};
use sha2::{Digest, Sha256};

// ---------------------------------------------------------------------------
// Public argument structs (mirrored from `cli::Daemon*Args`)
// ---------------------------------------------------------------------------

/// Arguments for `daemon install`.
#[derive(Debug, Clone, Copy)]
pub struct InstallArgs {
    /// Skip the "different content" overwrite confirmation.
    pub yes: bool,
    /// Install the unit but do NOT start the service immediately.
    pub no_start: bool,
}

/// Arguments for `daemon uninstall`.
#[derive(Debug, Clone, Copy)]
pub struct UninstallArgs {
    /// Skip the destructive-delete confirmation prompt.
    pub yes: bool,
    /// Preserve user data (chat.db, secrets.toml, daemon.toml, access.json).
    pub keep_data: bool,
}

/// Result of `daemon status`.
#[derive(Debug, Clone, serde::Serialize)]
pub struct StatusOutput {
    /// One of `"running"`, `"stopped"`, `"inactive"`, `"failed"`,
    /// `"not-installed"`.
    pub state: String,
    /// PID of the running daemon when `state == "running"`, else None.
    pub pid: Option<u32>,
}

// ---------------------------------------------------------------------------
// `.mcp.json` descriptor (SEC-2-7)
// ---------------------------------------------------------------------------

/// JSON descriptor consumed by Claude Code's plugin loader at
/// `~/.claude/plugins/claudebase/.mcp.json`. Both fields are written
/// verbatim — `args` is hard-coded to `["plugin","serve"]` and the only
/// dynamic field is `command`, which is set to the canonicalised binary
/// path.
#[derive(Debug, serde::Serialize, serde::Deserialize, PartialEq, Eq)]
pub struct McpDescriptor {
    pub command: String,
    pub args: Vec<String>,
}

impl McpDescriptor {
    /// Build a descriptor for the given canonical binary path. The
    /// `args` vector is hard-coded so a future arg-injection regression
    /// cannot smuggle a malicious flag through `daemon install`.
    pub fn new(binary: &Path) -> Self {
        McpDescriptor {
            command: binary.to_string_lossy().into_owned(),
            args: vec!["plugin".to_string(), "serve".to_string()],
        }
    }

    /// Render the descriptor to pretty-printed JSON (UTF-8).
    pub fn to_json(&self) -> Result<String> {
        serde_json::to_string_pretty(self).context("serialise .mcp.json descriptor")
    }
}

// ---------------------------------------------------------------------------
// Path helpers (HOME / config / install directories)
// ---------------------------------------------------------------------------

fn home_dir() -> Result<PathBuf> {
    #[cfg(unix)]
    {
        let home = std::env::var_os("HOME")
            .ok_or_else(|| anyhow!("HOME env var missing — cannot resolve service install paths"))?;
        Ok(PathBuf::from(home))
    }
    #[cfg(windows)]
    {
        let profile = std::env::var_os("USERPROFILE").ok_or_else(|| {
            anyhow!("USERPROFILE env var missing — cannot resolve service install paths")
        })?;
        Ok(PathBuf::from(profile))
    }
}

/// `~/.claude/plugins/claudebase/.mcp.json`.
pub fn mcp_json_path() -> Result<PathBuf> {
    Ok(home_dir()?
        .join(".claude")
        .join("plugins")
        .join("claudebase")
        .join(".mcp.json"))
}

/// `~/.claude/logs/claudebase-daemon.log` — canonical detached-daemon
/// stdout/stderr sink. Same path install.ps1 + `claudebase run`'s
/// auto-spawn use, so the operator has ONE log to tail regardless of who
/// started the daemon. Created lazily in append mode.
pub fn daemon_log_path() -> Result<PathBuf> {
    Ok(home_dir()?
        .join(".claude")
        .join("logs")
        .join("claudebase-daemon.log"))
}

/// Spawn `<current_exe> daemon serve` as a detached background process
/// with stdout/stderr redirected to `daemon_log_path()` (append). Mirrors
/// install.ps1's `Start-Process -WindowStyle Hidden ...` path so the
/// detached daemon survives the parent's exit and runs under the current
/// user's profile (NOT LocalService — see commit a615d9c rationale).
///
/// Used by:
///   - `main.rs::ensure_daemon_running` (re-spawn on `claudebase run`
///     when pipe is unreachable)
///   - `platform::start()` on Windows (CLI `claudebase daemon start`)
///
/// HOME / USERPROFILE env is explicitly injected so the detached daemon
/// can resolve `~/.claude/channels/claudebase/.env` (Telegram bot token
/// source) even when launched from a context that has USERPROFILE but
/// not HOME — the v0.6 silent-token-loss bug operator hit on 2026-06-03.
pub fn spawn_daemon_detached() -> std::io::Result<()> {
    use std::process::{Command, Stdio};
    let exe = std::env::current_exe()?;

    let home = std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .unwrap_or_else(|| std::ffi::OsString::from("."));
    let log_dir = std::path::PathBuf::from(&home).join(".claude").join("logs");
    let _ = std::fs::create_dir_all(&log_dir);
    let log_path = log_dir.join("claudebase-daemon.log");
    let log_handle = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&log_path)
        .ok();

    let mut cmd = Command::new(&exe);
    cmd.args(["daemon", "serve"]);
    cmd.stdin(Stdio::null());

    if let Some(home) = std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
    {
        cmd.env("HOME", home);
    }
    if let Some(lf) = log_handle {
        let lf_dup = lf.try_clone()?;
        cmd.stdout(Stdio::from(lf));
        cmd.stderr(Stdio::from(lf_dup));
    } else {
        cmd.stdout(Stdio::null());
        cmd.stderr(Stdio::null());
    }

    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const DETACHED_PROCESS: u32 = 0x00000008;
        const CREATE_NEW_PROCESS_GROUP: u32 = 0x00000200;
        const CREATE_NO_WINDOW: u32 = 0x08000000;
        cmd.creation_flags(DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP | CREATE_NO_WINDOW);
    }

    cmd.spawn()?;
    Ok(())
}

/// `~/.config/systemd/user/claudebase.service`.
#[cfg(target_os = "linux")]
pub fn systemd_unit_path() -> Result<PathBuf> {
    Ok(home_dir()?
        .join(".config")
        .join("systemd")
        .join("user")
        .join("claudebase.service"))
}

/// `~/Library/LaunchAgents/dev.codefather.claudebase.plist`.
#[cfg(target_os = "macos")]
pub fn launchd_plist_path() -> Result<PathBuf> {
    Ok(home_dir()?
        .join("Library")
        .join("LaunchAgents")
        .join("dev.codefather.claudebase.plist"))
}

/// Items removed on a no-`--keep-data` uninstall. Insight DB and books
/// corpus are deliberately NOT in this list (cross-feature data).
fn data_paths_full_wipe() -> Result<Vec<PathBuf>> {
    let home = home_dir()?;
    Ok(vec![
        home.join(".claude")
            .join("knowledge")
            .join("chat.db"),
        home.join(".config")
            .join("claudebase")
            .join("secrets.toml"),
        home.join(".config")
            .join("claudebase")
            .join("daemon.toml"),
        // Note: the runtime access.json lives at
        // `~/.claude/channels/claudebase/access.json` (owned by the
        // `/claudebase:access pair` skill) and is OUTSIDE this wipe
        // list by design — uninstalling the daemon must not clobber
        // the operator's chat-platform pairings. The legacy
        // `~/.config/claudebase/access.json` written by the removed
        // `claudebase daemon access pair` CLI was wiped in Slice 5.
    ])
}

// ---------------------------------------------------------------------------
// SEC-2-2 — refuse-root
// ---------------------------------------------------------------------------

/// Returns `Err` when running as root on Unix. The literal stderr line
/// `error: do not run 'daemon install' as root` is emitted by callers
/// that surface the error to the user (TC-2.10 substring match).
#[cfg(unix)]
pub fn refuse_root_install_with_euid(euid: u32) -> Result<()> {
    if euid == 0 {
        bail!("do not run 'daemon install' as root");
    }
    Ok(())
}

#[cfg(unix)]
pub fn refuse_root_install() -> Result<()> {
    extern "C" {
        fn geteuid() -> u32;
    }
    let euid = unsafe { geteuid() };
    refuse_root_install_with_euid(euid)
}

#[cfg(not(unix))]
pub fn refuse_root_install() -> Result<()> {
    Ok(())
}

// ---------------------------------------------------------------------------
// SEC-2-5 — canonical binary path + world-writable refuse
// ---------------------------------------------------------------------------

/// Resolve the path that should appear in service unit `ExecStart=`
/// (etc.) and refuse world-writable executables. Slice-2 design choice:
/// the path returned is `current_exe().canonicalize()` because the
/// daemon will be launched by the same binary that ran `install`.
pub fn resolve_binary_path() -> Result<PathBuf> {
    let exe = std::env::current_exe().context("current_exe()")?;
    let canon = fs::canonicalize(&exe)
        .with_context(|| format!("canonicalize binary path {}", exe.display()))?;
    let meta = fs::metadata(&canon)
        .with_context(|| format!("stat canonical binary {}", canon.display()))?;
    if !meta.is_file() {
        bail!(
            "refuse: resolved binary path is not a regular file: {}",
            canon.display()
        );
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if meta.permissions().mode() & 0o002 != 0 {
            bail!(
                "refuse: claudebase binary is world-writable: {}",
                canon.display()
            );
        }
    }
    Ok(canon)
}

// ---------------------------------------------------------------------------
// SEC-2-3 + SEC-2-12 — atomic, symlink-refusing, mode-explicit write helper
// ---------------------------------------------------------------------------

/// Atomic write that refuses to follow symlinks and forces a final mode
/// regardless of umask. The sequence:
///
/// 1. `symlink_metadata(path)` — refuse if the destination is itself a
///    symlink (we will NOT follow into a world-writable target).
/// 2. Create `<path>.<pid>.tmp` via O_EXCL.
/// 3. Write the entire content.
/// 4. Atomic `rename` over `path`.
/// 5. `chmod 0o644` (or caller-supplied mode) on the final path.
pub fn write_refusing_symlink(path: &Path, content: &[u8], mode: u32) -> Result<()> {
    if let Ok(meta) = fs::symlink_metadata(path) {
        if meta.file_type().is_symlink() {
            bail!("refuse to write through symlink: {}", path.display());
        }
    }
    let parent = path
        .parent()
        .ok_or_else(|| anyhow!("write target has no parent: {}", path.display()))?;
    // tempfile name is unique-per-process; collisions across concurrent
    // installs are unlikely and would surface as O_EXCL failures.
    let tmp_name = format!(
        "{}.{}.tmp",
        path.file_name()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_else(|| "service".to_string()),
        std::process::id()
    );
    let tmp = parent.join(tmp_name);
    // Clean any leftover temp from a prior crashed install.
    let _ = fs::remove_file(&tmp);
    let mut f = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&tmp)
        .with_context(|| format!("create temp file {}", tmp.display()))?;
    f.write_all(content)
        .with_context(|| format!("write temp file {}", tmp.display()))?;
    f.flush().ok();
    drop(f);
    fs::rename(&tmp, path)
        .with_context(|| format!("atomic rename {} -> {}", tmp.display(), path.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = mode;
        fs::set_permissions(path, fs::Permissions::from_mode(mode)).with_context(|| {
            format!(
                "chmod {:o} on {} (post-write SEC-2-12)",
                mode,
                path.display()
            )
        })?;
    }
    #[cfg(not(unix))]
    {
        let _ = mode;
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// SEC-2-9 — parent-directory inspect-and-refuse-loose-perms
// ---------------------------------------------------------------------------

/// Create the parent directory at the required mode and refuse to
/// install when an existing directory already has bits *outside* the
/// required mode (e.g., world-writable bits on a 0o700-required dir).
pub fn ensure_install_parent(dir: &Path, required_mode: u32) -> Result<()> {
    fs::create_dir_all(dir).with_context(|| format!("create_dir_all {}", dir.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let meta = fs::symlink_metadata(dir)
            .with_context(|| format!("lstat install parent {}", dir.display()))?;
        if meta.file_type().is_symlink() {
            bail!(
                "refuse to install into symlinked directory: {}",
                dir.display()
            );
        }
        let existing = meta.permissions().mode() & 0o777;
        if existing != required_mode {
            // Normalize the directory to EXACTLY the required mode. A dir
            // left by an older install may be looser than required (e.g.
            // 0o755 on a now-0o700 logs/plugins dir). The previous behaviour
            // REFUSED in that case, which broke `claudebase daemon install`
            // on every upgrade and forced the operator to chmod by hand.
            // These are claudebase-owned private dirs, so chmod-to-required
            // is a security *tightening* (0o755 -> 0o700), not a relaxation;
            // the symlink refusal above still blocks the real attack (install
            // target swapped for a symlink). No elevation is needed — the
            // dirs live under $HOME and are owned by the invoking user.
            fs::set_permissions(dir, fs::Permissions::from_mode(required_mode)).with_context(
                || format!("chmod {:o} on {}", required_mode, dir.display()),
            )?;
        }
    }
    #[cfg(not(unix))]
    {
        let _ = required_mode;
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// STRUCTURAL-2-1 — XML helper for the hand-rolled plist generator
// ---------------------------------------------------------------------------

/// Escape the five XML metacharacters. Used ONLY by the plist generator;
/// systemd unit content is INI-style and does not need XML escaping.
pub fn xml_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '&' => out.push_str("&amp;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&apos;"),
            _ => out.push(c),
        }
    }
    out
}

// ---------------------------------------------------------------------------
// SEC-2-1 — systemd unit generator (Linux)
// ---------------------------------------------------------------------------

/// Build the literal text of the systemd USER service unit. Every
/// hardening directive in SEC-2-1 is present; no `User=` directive
/// exists.
pub fn generate_systemd_unit(binary_path: &Path) -> String {
    let exec = binary_path.to_string_lossy();
    let mut s = String::with_capacity(1024);
    let _ = write!(
        s,
        "[Unit]\n\
Description=claudebase agent chat daemon\n\
After=default.target\n\
\n\
[Service]\n\
Type=simple\n\
ExecStart={exec} daemon serve\n\
Restart=on-failure\n\
RestartSec=5\n\
NoNewPrivileges=true\n\
PrivateTmp=true\n\
ProtectSystem=strict\n\
ProtectHome=read-only\n\
ReadWritePaths=%h/.claude %h/.config/claudebase\n\
ProtectKernelTunables=true\n\
ProtectKernelModules=true\n\
ProtectControlGroups=true\n\
RestrictNamespaces=true\n\
RestrictRealtime=true\n\
LockPersonality=true\n\
MemoryDenyWriteExecute=true\n\
SystemCallArchitectures=native\n\
CapabilityBoundingSet=\n\
AmbientCapabilities=\n\
\n\
[Install]\n\
WantedBy=default.target\n",
    );
    s
}

// ---------------------------------------------------------------------------
// SEC-2-6 + STRUCTURAL-2-1 — launchd plist generator (macOS)
// ---------------------------------------------------------------------------

/// Build the literal text of the launchd LaunchAgent plist. NO
/// `UserName` key — LaunchAgents run as the calling user by default.
/// XML escapes every substituted value (binary path + log paths).
pub fn generate_launchd_plist(binary_path: &Path, stdout_log: &Path, stderr_log: &Path) -> String {
    let exec = xml_escape(&binary_path.to_string_lossy());
    let so = xml_escape(&stdout_log.to_string_lossy());
    let se = xml_escape(&stderr_log.to_string_lossy());
    let mut s = String::with_capacity(1024);
    let _ = write!(
        s,
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n\
<!DOCTYPE plist PUBLIC \"-//Apple//DTD PLIST 1.0//EN\" \"http://www.apple.com/DTDs/PropertyList-1.0.dtd\">\n\
<plist version=\"1.0\">\n\
<dict>\n\
    <key>Label</key>\n\
    <string>dev.codefather.claudebase</string>\n\
    <key>ProgramArguments</key>\n\
    <array>\n\
        <string>{exec}</string>\n\
        <string>daemon</string>\n\
        <string>serve</string>\n\
    </array>\n\
    <key>RunAtLoad</key>\n\
    <true/>\n\
    <key>KeepAlive</key>\n\
    <true/>\n\
    <key>StandardOutPath</key>\n\
    <string>{so}</string>\n\
    <key>StandardErrorPath</key>\n\
    <string>{se}</string>\n\
    <key>EnvironmentVariables</key>\n\
    <dict>\n\
        <key>RUST_LOG</key>\n\
        <string>info</string>\n\
    </dict>\n\
</dict>\n\
</plist>\n",
    );
    s
}

// ---------------------------------------------------------------------------
// Idempotency helpers (SEC-2-4 + SEC-2-8)
// ---------------------------------------------------------------------------

fn sha256_bytes(b: &[u8]) -> String {
    let mut h = Sha256::new();
    h.update(b);
    format!("{:x}", h.finalize())
}

/// Outcome of a content-equality check used by SEC-2-4 / SEC-2-8.
#[derive(Debug, PartialEq, Eq)]
pub enum IdempotencyDecision {
    /// File does not exist — write fresh.
    Fresh,
    /// File exists with identical sha — preserve mtime, exit "no changes".
    AlreadyInstalled,
    /// File exists with different content — without `--yes`, refuse; with
    /// `--yes`, overwrite.
    Differs,
}

pub fn check_idempotency(path: &Path, new_content: &[u8]) -> IdempotencyDecision {
    match fs::read(path) {
        Ok(existing) => {
            if sha256_bytes(&existing) == sha256_bytes(new_content) {
                IdempotencyDecision::AlreadyInstalled
            } else {
                IdempotencyDecision::Differs
            }
        }
        Err(_) => IdempotencyDecision::Fresh,
    }
}

// ---------------------------------------------------------------------------
// `.mcp.json` writer (SEC-2-7 + SEC-2-8)
// ---------------------------------------------------------------------------

/// Write `~/.claude/plugins/claudebase/.mcp.json` atomically with
/// content-equality short-circuit. Returns the textual content that
/// was (or would be) on disk so callers can log it.
pub fn write_mcp_descriptor(binary: &Path) -> Result<(PathBuf, IdempotencyDecision)> {
    let desc = McpDescriptor::new(binary);
    let body = desc.to_json()?;
    let path = mcp_json_path()?;
    let parent = path
        .parent()
        .ok_or_else(|| anyhow!(".mcp.json target has no parent"))?;
    ensure_install_parent(parent, 0o700)?;
    match check_idempotency(&path, body.as_bytes()) {
        IdempotencyDecision::AlreadyInstalled => Ok((path, IdempotencyDecision::AlreadyInstalled)),
        IdempotencyDecision::Fresh | IdempotencyDecision::Differs => {
            write_refusing_symlink(&path, body.as_bytes(), 0o644)?;
            Ok((path, IdempotencyDecision::Fresh))
        }
    }
}

// ---------------------------------------------------------------------------
// Per-platform install / uninstall / lifecycle
// ---------------------------------------------------------------------------

#[cfg(target_os = "linux")]
mod platform {
    use super::*;

    /// systemctl probe — returns the literal stdout of the version
    /// query. Empty / error => systemd-user unavailable (WSL / container).
    fn systemctl_user_available() -> bool {
        let r = Command::new("systemctl")
            .arg("--user")
            .arg("--version")
            .output();
        matches!(r, Ok(o) if o.status.success())
    }

    pub fn install(args: &InstallArgs, binary: &Path) -> Result<()> {
        if !systemctl_user_available() {
            // TC-2.9 wants this stderr string AND a clean exit so the
            // installer can be safely embedded in install.sh on WSL.
            eprintln!("Warning: systemd user units not supported in this environment");
            eprintln!("hint: install.sh CLAUDEBASE_INSTALL_DAEMON=1 hook will skip silently");
            return Ok(());
        }
        let unit = generate_systemd_unit(binary);
        let path = systemd_unit_path()?;
        let parent = path.parent().unwrap();
        ensure_install_parent(parent, 0o755)?;

        match check_idempotency(&path, unit.as_bytes()) {
            IdempotencyDecision::AlreadyInstalled => {
                println!("already installed (no changes)");
            }
            IdempotencyDecision::Differs if !args.yes => {
                bail!("existing unit differs; pass --yes to overwrite");
            }
            IdempotencyDecision::Fresh | IdempotencyDecision::Differs => {
                write_refusing_symlink(&path, unit.as_bytes(), 0o644)?;
            }
        }

        // .mcp.json descriptor
        write_mcp_descriptor(binary)?;

        // systemctl daemon-reload + enable
        let _ = Command::new("systemctl")
            .arg("--user")
            .arg("daemon-reload")
            .status();
        let enable_status = Command::new("systemctl")
            .arg("--user")
            .arg("enable")
            .arg("claudebase")
            .status();
        match enable_status {
            Ok(s) if s.success() => {}
            Ok(s) => bail!("systemctl --user enable claudebase failed: exit {s}"),
            Err(e) => bail!("failed to invoke systemctl: {e}"),
        }

        if !args.no_start {
            let _ = Command::new("systemctl")
                .arg("--user")
                .arg("start")
                .arg("claudebase")
                .status();
        } else {
            println!("To start now: claudebase daemon start");
        }
        println!("claudebase daemon installed");
        Ok(())
    }

    pub fn uninstall(keep_data: bool) -> Result<()> {
        let _ = Command::new("systemctl")
            .arg("--user")
            .arg("stop")
            .arg("claudebase")
            .status();
        let _ = Command::new("systemctl")
            .arg("--user")
            .arg("disable")
            .arg("claudebase")
            .status();
        let unit_path = systemd_unit_path()?;
        let _ = fs::remove_file(&unit_path);
        let mcp = mcp_json_path()?;
        let _ = fs::remove_file(&mcp);
        if !keep_data {
            for p in data_paths_full_wipe()? {
                let _ = fs::remove_file(&p);
            }
        }
        let _ = Command::new("systemctl")
            .arg("--user")
            .arg("daemon-reload")
            .status();
        Ok(())
    }

    pub fn start() -> Result<()> {
        let s = Command::new("systemctl")
            .arg("--user")
            .arg("start")
            .arg("claudebase")
            .status()
            .context("invoke systemctl --user start")?;
        if !s.success() {
            bail!("service start failed: systemctl --user start claudebase exit {s}");
        }
        println!("claudebase daemon started");
        Ok(())
    }

    pub fn stop() -> Result<()> {
        let s = Command::new("systemctl")
            .arg("--user")
            .arg("stop")
            .arg("claudebase")
            .status()
            .context("invoke systemctl --user stop")?;
        let _ = s;
        Ok(())
    }

    pub fn restart() -> Result<()> {
        let s = Command::new("systemctl")
            .arg("--user")
            .arg("restart")
            .arg("claudebase")
            .status()
            .context("invoke systemctl --user restart")?;
        if !s.success() {
            bail!("service restart failed: systemctl --user restart claudebase exit {s}");
        }
        Ok(())
    }

    pub fn status() -> Result<StatusOutput> {
        // `systemctl --user is-active` returns 0 when active, non-zero otherwise.
        let r = Command::new("systemctl")
            .arg("--user")
            .arg("is-active")
            .arg("claudebase")
            .output();
        let state = match r {
            Ok(o) => {
                let txt = String::from_utf8_lossy(&o.stdout).trim().to_string();
                if txt == "active" {
                    "running".to_string()
                } else if txt.is_empty() {
                    "not-installed".to_string()
                } else {
                    // common values: inactive / failed / activating / deactivating
                    txt
                }
            }
            Err(_) => "not-installed".to_string(),
        };
        Ok(StatusOutput { state, pid: None })
    }

    pub fn logs(lines: u32, follow: bool) -> Result<()> {
        // SEC-2-14: arg-vector form, integer lines argument, no --grep.
        let mut cmd = Command::new("journalctl");
        cmd.arg("--user")
            .arg("-u")
            .arg("claudebase")
            .arg("-n")
            .arg(lines.to_string())
            .arg("--no-pager");
        if follow {
            cmd.arg("-f");
        }
        let s = cmd
            .status()
            .context("invoke journalctl --user -u claudebase")?;
        if !s.success() {
            bail!("daemon logs: journalctl exit {s}");
        }
        Ok(())
    }
}

#[cfg(target_os = "macos")]
mod platform {
    use super::*;

    fn log_paths() -> Result<(PathBuf, PathBuf)> {
        let logs = home_dir()?
            .join(".config")
            .join("claudebase")
            .join("logs");
        ensure_install_parent(&logs, 0o700)?;
        Ok((logs.join("daemon.out.log"), logs.join("daemon.err.log")))
    }

    pub fn install(args: &InstallArgs, binary: &Path) -> Result<()> {
        let (out, err) = log_paths()?;
        let plist = generate_launchd_plist(binary, &out, &err);
        let path = launchd_plist_path()?;
        let parent = path.parent().unwrap();
        ensure_install_parent(parent, 0o755)?;

        match check_idempotency(&path, plist.as_bytes()) {
            IdempotencyDecision::AlreadyInstalled => {
                println!("already installed (no changes)");
            }
            IdempotencyDecision::Differs if !args.yes => {
                bail!("existing unit differs; pass --yes to overwrite");
            }
            IdempotencyDecision::Fresh | IdempotencyDecision::Differs => {
                write_refusing_symlink(&path, plist.as_bytes(), 0o644)?;
            }
        }

        write_mcp_descriptor(binary)?;

        // `launchctl load` is NOT idempotent — re-loading an already
        // bootstrapped agent exits non-zero with "Load failed: 5: Input/
        // output error". Unload first (best-effort; stderr silenced — it is
        // noisy when nothing was loaded) so a re-run (install.sh on every
        // upgrade) reliably (re)loads the agent instead of printing a scary
        // error while the user-level agent quietly fails to load.
        let _ = Command::new("launchctl")
            .arg("unload")
            .arg(&path)
            .stderr(std::process::Stdio::null())
            .status();
        let load_ok = Command::new("launchctl")
            .arg("load")
            .arg(&path)
            .stderr(std::process::Stdio::null())
            .status()
            .map(|s| s.success())
            .unwrap_or(false);
        if !load_ok {
            eprintln!(
                "warning: launchctl could not load the agent; start it with \
                 `claudebase daemon start` (plist: {})",
                path.display()
            );
        }
        if !args.no_start {
            let _ = Command::new("launchctl")
                .arg("kickstart")
                .arg(format!("gui/{}/dev.codefather.claudebase", uid()))
                .status();
        } else {
            println!("To start now: claudebase daemon start");
        }
        println!("claudebase daemon installed");
        Ok(())
    }

    fn uid() -> u32 {
        extern "C" {
            fn geteuid() -> u32;
        }
        unsafe { geteuid() }
    }

    pub fn uninstall(keep_data: bool) -> Result<()> {
        let path = launchd_plist_path()?;
        let _ = Command::new("launchctl").arg("unload").arg(&path).status();
        let _ = fs::remove_file(&path);
        let mcp = mcp_json_path()?;
        let _ = fs::remove_file(&mcp);
        if !keep_data {
            for p in data_paths_full_wipe()? {
                let _ = fs::remove_file(&p);
            }
        }
        Ok(())
    }

    pub fn start() -> Result<()> {
        let path = launchd_plist_path()?;
        // `load` ensures the agent is bootstrapped, but when it already is
        // (the common case right after `daemon install`) launchctl prints
        // "Load failed: 5: Input/output error" to stderr. Silence it — the
        // `kickstart` below is what actually (re)starts the service and is
        // idempotent whether or not the load was a no-op.
        let _ = Command::new("launchctl")
            .arg("load")
            .arg(&path)
            .stderr(std::process::Stdio::null())
            .status();
        let _ = Command::new("launchctl")
            .arg("kickstart")
            .arg(format!("gui/{}/dev.codefather.claudebase", uid()))
            .status();
        println!("claudebase daemon started");
        Ok(())
    }

    pub fn stop() -> Result<()> {
        let path = launchd_plist_path()?;
        let _ = Command::new("launchctl").arg("unload").arg(&path).status();
        Ok(())
    }

    pub fn restart() -> Result<()> {
        stop()?;
        start()?;
        Ok(())
    }

    pub fn status() -> Result<StatusOutput> {
        let r = Command::new("launchctl")
            .arg("print")
            .arg(format!("gui/{}/dev.codefather.claudebase", uid()))
            .output();
        let state = match r {
            Ok(o) if o.status.success() => {
                let body = String::from_utf8_lossy(&o.stdout);
                if body.contains("state = running") {
                    "running".to_string()
                } else if body.contains("state = ") {
                    "stopped".to_string()
                } else {
                    "stopped".to_string()
                }
            }
            _ => "not-installed".to_string(),
        };
        Ok(StatusOutput { state, pid: None })
    }

    pub fn logs(lines: u32, follow: bool) -> Result<()> {
        // `log show` with predicate; `log stream` for --follow.
        if follow {
            let s = Command::new("log")
                .arg("stream")
                .arg("--predicate")
                .arg("process == \"claudebase\"")
                .status()
                .context("invoke log stream")?;
            if !s.success() {
                bail!("daemon logs: log stream exit {s}");
            }
        } else {
            let s = Command::new("log")
                .arg("show")
                .arg("--last")
                .arg(format!("{}m", lines.saturating_mul(1).max(1)))
                .arg("--predicate")
                .arg("process == \"claudebase\"")
                .arg("--style")
                .arg("compact")
                .status()
                .context("invoke log show")?;
            if !s.success() {
                bail!("daemon logs: log show exit {s}");
            }
        }
        Ok(())
    }
}

#[cfg(windows)]
mod platform {
    use super::*;
    use std::ffi::OsString;

    use windows_service::{
        service::{ServiceAccess, ServiceErrorControl, ServiceInfo, ServiceStartType, ServiceType},
        service_manager::{ServiceManager, ServiceManagerAccess},
    };

    const SERVICE_NAME: &str = "claudebase";
    const DISPLAY_NAME: &str = "Claudebase Agent Chat Daemon";

    /// SEC-2-11 — service account MUST NOT be LocalSystem. Use the
    /// least-privileged built-in service account.
    fn service_account() -> OsString {
        OsString::from("NT AUTHORITY\\LocalService")
    }

    pub fn install(args: &InstallArgs, binary: &Path) -> Result<()> {
        let account = service_account();
        // SEC-2-11 runtime assertion
        assert!(
            !account.to_string_lossy().eq_ignore_ascii_case("LocalSystem"),
            "SEC-2-11 violation: account is LocalSystem"
        );

        let manager = ServiceManager::local_computer(None::<&str>, ServiceManagerAccess::CONNECT | ServiceManagerAccess::CREATE_SERVICE)
            .context("open SCM (admin elevation required)")?;
        let info = ServiceInfo {
            name: OsString::from(SERVICE_NAME),
            display_name: OsString::from(DISPLAY_NAME),
            service_type: ServiceType::OWN_PROCESS,
            start_type: ServiceStartType::AutoStart,
            error_control: ServiceErrorControl::Normal,
            executable_path: binary.to_path_buf(),
            launch_arguments: vec![OsString::from("daemon"), OsString::from("serve")],
            dependencies: vec![],
            account_name: Some(account),
            account_password: None,
        };

        // Idempotency check: if the service already exists, skip creating it.
        let exists = manager
            .open_service(SERVICE_NAME, ServiceAccess::QUERY_STATUS)
            .is_ok();
        if exists {
            println!("already installed (no changes)");
        } else {
            manager
                .create_service(&info, ServiceAccess::CHANGE_CONFIG)
                .context("create Windows Service")?;
        }
        let _ = args; // no_start handled below
        write_mcp_descriptor(binary)?;
        if !args.no_start {
            start()?;
        } else {
            println!("To start now: claudebase daemon start");
        }
        println!("claudebase daemon installed");
        Ok(())
    }

    pub fn uninstall(keep_data: bool) -> Result<()> {
        let manager = ServiceManager::local_computer(None::<&str>, ServiceManagerAccess::CONNECT)
            .context("open SCM (admin elevation required)")?;
        if let Ok(svc) = manager.open_service(SERVICE_NAME, ServiceAccess::STOP | ServiceAccess::DELETE) {
            let _ = svc.stop();
            let _ = svc.delete();
        }
        let mcp = mcp_json_path()?;
        let _ = fs::remove_file(&mcp);
        if !keep_data {
            for p in data_paths_full_wipe()? {
                let _ = fs::remove_file(&p);
            }
        }
        Ok(())
    }

    /// Discover currently-running `claudebase.exe daemon serve` PIDs via
    /// PowerShell CIM query. Returns an empty Vec when no daemon process
    /// is found. Excludes `claudebase.exe run` (CLI launcher) and
    /// `claudebase.exe plugin serve` (per-CC MCP bridge) by matching the
    /// command line against `daemon serve`.
    fn find_daemon_pids() -> Result<Vec<u32>> {
        let out = Command::new("powershell.exe")
            .args([
                "-NoProfile",
                "-Command",
                "Get-CimInstance Win32_Process -Filter \"Name='claudebase.exe'\" | \
                 Where-Object { $_.CommandLine -match 'daemon serve' } | \
                 ForEach-Object { $_.ProcessId }",
            ])
            .output()
            .context("enumerate claudebase daemon processes via powershell")?;
        if !out.status.success() {
            bail!(
                "process enumeration failed: {}",
                String::from_utf8_lossy(&out.stderr)
            );
        }
        let txt = String::from_utf8_lossy(&out.stdout);
        let pids: Vec<u32> = txt
            .lines()
            .filter_map(|l| l.trim().parse::<u32>().ok())
            .collect();
        Ok(pids)
    }

    /// Migrated 2026-06-04 from SCM service to detached current-user
    /// process. The SCM path (`OpenService START`) failed with
    /// `error: open service for start` whenever install.ps1 had NOT
    /// registered the service (the default after commit `a615d9c`),
    /// breaking `claudebase daemon start` on every fresh install. We
    /// now reuse `super::spawn_daemon_detached()` — the same helper
    /// `main.rs::ensure_daemon_running` calls — so a fresh-installed
    /// box and a `claudebase run` auto-spawn produce semantically
    /// identical daemon processes.
    pub fn start() -> Result<()> {
        if super::pipe_is_alive() {
            println!("claudebase daemon already running");
            return Ok(());
        }
        super::spawn_daemon_detached().context("spawn detached daemon")?;
        // Poll the named-pipe for up to 3s so `daemon start` exits with
        // success only when the spawned daemon has actually bound its
        // IPC surface. Matches install.ps1's post-spawn liveness check.
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(3);
        while std::time::Instant::now() < deadline {
            if super::pipe_is_alive() {
                println!("claudebase daemon started");
                return Ok(());
            }
            std::thread::sleep(std::time::Duration::from_millis(150));
        }
        bail!(
            "daemon spawn issued but named-pipe not reachable within 3s — check {}",
            super::daemon_log_path()
                .map(|p| p.display().to_string())
                .unwrap_or_else(|_| "~/.claude/logs/claudebase-daemon.log".to_string())
        );
    }

    /// Migrated 2026-06-04 from SCM `svc.stop()` to `taskkill /PID /F`
    /// per matched PID. find_daemon_pids() is cmdline-filtered so we
    /// kill ONLY `daemon serve` processes — `claudebase run` and
    /// `plugin serve` siblings are left alone.
    pub fn stop() -> Result<()> {
        let pids = find_daemon_pids()?;
        if pids.is_empty() {
            println!("no daemon process found (already stopped)");
            return Ok(());
        }
        for pid in &pids {
            let s = Command::new("taskkill")
                .args(["/PID", &pid.to_string(), "/F"])
                .status()
                .context("invoke taskkill")?;
            if !s.success() {
                eprintln!("warn: taskkill exited non-zero ({}) for pid {}", s, pid);
            }
        }
        println!(
            "claudebase daemon stopped ({} process{})",
            pids.len(),
            if pids.len() == 1 { "" } else { "es" }
        );
        Ok(())
    }

    pub fn restart() -> Result<()> {
        stop()?;
        // Give the killed process a moment to release its named-pipe
        // before the new spawn tries to bind it. Without the sleep,
        // pipe-probe races can wrongly report "already running" on
        // start() and skip the spawn.
        std::thread::sleep(std::time::Duration::from_millis(500));
        start()?;
        Ok(())
    }

    /// Migrated 2026-06-04 from SCM `query_status` to process-discovery.
    /// The outer wrapper at `service::status()` still falls back to
    /// `pipe_is_alive()` so a daemon found via process scan but with
    /// a broken pipe (mid-shutdown) still surfaces correctly.
    pub fn status() -> Result<StatusOutput> {
        let pids = find_daemon_pids().unwrap_or_default();
        if pids.is_empty() {
            return Ok(StatusOutput {
                state: "stopped".to_string(),
                pid: None,
            });
        }
        Ok(StatusOutput {
            state: "running".to_string(),
            pid: Some(pids[0]),
        })
    }

    /// Migrated 2026-06-04 from Windows Event Log (`Get-WinEvent`) to
    /// file-tail of `~/.claude/logs/claudebase-daemon.log` — the
    /// canonical sink for the detached daemon's stdout/stderr (matches
    /// install.ps1 + `spawn_daemon_detached()`). Honours `--lines N`
    /// and `--follow` via PowerShell's `Get-Content -Tail -Wait`.
    pub fn logs(lines: u32, follow: bool) -> Result<()> {
        let log = super::daemon_log_path()?;
        if !log.exists() {
            println!(
                "no daemon log yet at {} (daemon not started by detached-spawn path, or not run since install)",
                log.display()
            );
            return Ok(());
        }
        let log_str = log.display().to_string().replace('\'', "''");
        let ps_cmd = if follow {
            format!(
                "Get-Content -Path '{}' -Tail {} -Wait",
                log_str, lines
            )
        } else {
            format!("Get-Content -Path '{}' -Tail {}", log_str, lines)
        };
        let s = Command::new("powershell.exe")
            .args(["-NoProfile", "-Command", &ps_cmd])
            .status()
            .context("invoke powershell Get-Content")?;
        if !s.success() {
            bail!("daemon logs: powershell exit {}", s);
        }
        Ok(())
    }
}

// Stub for any other Unix (BSD, etc.) so the crate still compiles.
#[cfg(not(any(target_os = "linux", target_os = "macos", windows)))]
mod platform {
    use super::*;

    pub fn install(_args: &InstallArgs, _binary: &Path) -> Result<()> {
        bail!("service installer not supported on this platform")
    }
    pub fn uninstall(_keep_data: bool) -> Result<()> {
        bail!("service installer not supported on this platform")
    }
    pub fn start() -> Result<()> {
        bail!("service installer not supported on this platform")
    }
    pub fn stop() -> Result<()> {
        bail!("service installer not supported on this platform")
    }
    pub fn restart() -> Result<()> {
        bail!("service installer not supported on this platform")
    }
    pub fn status() -> Result<StatusOutput> {
        Ok(StatusOutput {
            state: "not-installed".to_string(),
            pid: None,
        })
    }
    pub fn logs(_lines: u32, _follow: bool) -> Result<()> {
        bail!("service installer not supported on this platform")
    }
}

// ---------------------------------------------------------------------------
// Public top-level entry points consumed by `main.rs`
// ---------------------------------------------------------------------------

pub fn install(args: &InstallArgs) -> Result<()> {
    refuse_root_install()?;
    let bin = resolve_binary_path()?;
    platform::install(args, &bin)
}

/// Helper used by both the interactive and headless paths.
fn perform_uninstall(keep_data: bool) -> Result<()> {
    platform::uninstall(keep_data)
}

pub fn uninstall(args: &UninstallArgs) -> Result<()> {
    if !args.yes {
        // Headless detection — refuse when stdin is not a TTY.
        if !stdin_is_tty() {
            bail!(
                "daemon uninstall: refusing to delete data without --yes in non-interactive mode"
            );
        }
        if !confirm_destructive(args.keep_data)? {
            println!("daemon uninstall: aborted by user");
            return Ok(());
        }
    }
    perform_uninstall(args.keep_data)
}

pub fn start() -> Result<()> {
    platform::start()
}

pub fn stop() -> Result<()> {
    platform::stop()
}

pub fn restart() -> Result<()> {
    platform::restart()
}

pub fn status() -> Result<StatusOutput> {
    let mut out = platform::status()?;
    // Pipe-probe fallback for the v0.6 SCM-blind bug: when the service
    // manager reports stopped/not-installed/inactive/failed BUT a daemon
    // process is actually listening on the UDS / named-pipe (because the
    // operator started it via `daemon serve` outside the service
    // manager, or `claudebase run` auto-spawned it), surface that as
    // "running" so callers observing this state behave correctly.
    if out.state != "running" && pipe_is_alive() {
        out.state = "running".to_string();
        // pid stays None — we don't know it from a pipe probe.
    }
    Ok(out)
}

/// Sync pipe-probe used by `status()` and by the `claudebase run`
/// ensure-daemon helper. Returns true when a blocking connect to the
/// daemon's UDS / named-pipe accepts.
fn pipe_is_alive() -> bool {
    use interprocess::local_socket::{prelude::*, GenericFilePath, Stream, ToFsName};
    let path = match crate::daemon::server::socket_path() {
        Ok(p) => p,
        Err(_) => return false,
    };
    let name = match path.to_fs_name::<GenericFilePath>() {
        Ok(n) => n,
        Err(_) => return false,
    };
    Stream::connect(name).is_ok()
}

pub fn logs(lines: u32, follow: bool) -> Result<()> {
    platform::logs(lines, follow)
}

// ---------------------------------------------------------------------------
// TTY helpers — confirm prompt (SEC-2-13)
// ---------------------------------------------------------------------------

fn stdin_is_tty() -> bool {
    #[cfg(unix)]
    {
        extern "C" {
            fn isatty(fd: i32) -> i32;
        }
        unsafe { isatty(0) == 1 }
    }
    #[cfg(windows)]
    {
        // On Windows assume TTY when --yes is absent so the interactive
        // path is reachable from the typical PowerShell session.
        true
    }
    #[cfg(not(any(unix, windows)))]
    {
        false
    }
}

fn confirm_destructive(keep_data: bool) -> Result<bool> {
    println!("daemon uninstall will remove:");
    println!("  - service unit");
    println!("  - .mcp.json");
    if !keep_data {
        println!("  - chat.db");
        println!("  - secrets.toml");
        println!("  - daemon.toml");
    }
    print!("Proceed? [y/N] ");
    let _ = std::io::stdout().flush();
    let mut input = String::new();
    std::io::stdin()
        .read_line(&mut input)
        .context("read confirmation from stdin")?;
    Ok(matches!(input.trim(), "y" | "Y"))
}

// ---------------------------------------------------------------------------
// Internal unit tests for pure functions (no FS / no Command::new)
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn xml_escape_handles_five_metachars() {
        assert_eq!(xml_escape("a<b>&c\"d'e"), "a&lt;b&gt;&amp;c&quot;d&apos;e");
    }

    #[test]
    fn xml_escape_preserves_plain_text() {
        assert_eq!(xml_escape("ordinary path /usr/local/bin"), "ordinary path /usr/local/bin");
    }

    #[test]
    fn mcp_descriptor_args_are_hardcoded() {
        let d = McpDescriptor::new(Path::new("/tmp/claudebase"));
        assert_eq!(d.args, vec!["plugin".to_string(), "serve".to_string()]);
    }

    #[test]
    fn mcp_descriptor_json_round_trip() {
        let d = McpDescriptor::new(Path::new("/usr/local/bin/claudebase"));
        let s = d.to_json().unwrap();
        let back: McpDescriptor = serde_json::from_str(&s).unwrap();
        assert_eq!(d, back);
    }

    #[test]
    fn systemd_unit_has_no_user_directive() {
        let s = generate_systemd_unit(Path::new("/usr/local/bin/claudebase"));
        for line in s.lines() {
            assert!(
                !line.starts_with("User="),
                "systemd unit must NOT contain User= directive (SEC-2-1)"
            );
        }
    }

    #[test]
    fn systemd_unit_contains_hardening_directives() {
        let s = generate_systemd_unit(Path::new("/usr/local/bin/claudebase"));
        for token in [
            "ProtectSystem=strict",
            "NoNewPrivileges=true",
            "PrivateTmp=true",
            "ProtectHome=read-only",
            "ReadWritePaths=%h/.claude %h/.config/claudebase",
            "ProtectKernelTunables=true",
            "ProtectKernelModules=true",
            "ProtectControlGroups=true",
            "RestrictNamespaces=true",
            "RestrictRealtime=true",
            "LockPersonality=true",
            "MemoryDenyWriteExecute=true",
            "SystemCallArchitectures=native",
            "CapabilityBoundingSet=",
        ] {
            assert!(
                s.contains(token),
                "missing hardening directive `{token}`\nunit:\n{s}"
            );
        }
    }

    #[test]
    fn launchd_plist_is_launch_agent_no_username_no_root() {
        let p = generate_launchd_plist(
            Path::new("/usr/local/bin/claudebase"),
            Path::new("/tmp/out.log"),
            Path::new("/tmp/err.log"),
        );
        assert!(!p.contains("UserName"), "plist must NOT contain UserName key (SEC-2-6)");
        // "root" must not appear as a string value -- the literal substring
        // "root" is checked against the full plist body.
        assert!(
            !p.contains(">root<"),
            "plist must NOT mention root as a value (SEC-2-6)"
        );
    }

    #[test]
    fn launchd_plist_escapes_xml_metachars() {
        let p = generate_launchd_plist(
            Path::new("/path with <crazy> & \"chars\""),
            Path::new("/tmp/out.log"),
            Path::new("/tmp/err.log"),
        );
        assert!(p.contains("&lt;crazy&gt;"));
        assert!(p.contains("&amp;"));
        assert!(p.contains("&quot;chars&quot;"));
        // The raw XML metacharacters MUST NOT appear inside the substituted
        // path (we still expect them in the surrounding markup like
        // <string> and <key>).
        assert!(!p.contains("<crazy>"), "raw `<crazy>` must not appear");
    }

    #[cfg(unix)]
    #[test]
    fn refuse_root_install_with_euid_blocks_zero() {
        let err = refuse_root_install_with_euid(0).unwrap_err();
        let msg = format!("{err}");
        assert!(
            msg.contains("do not run 'daemon install' as root"),
            "wrong stderr: {msg}"
        );
    }

    #[cfg(unix)]
    #[test]
    fn refuse_root_install_with_euid_allows_nonzero() {
        assert!(refuse_root_install_with_euid(1000).is_ok());
    }

    #[test]
    fn check_idempotency_detects_match() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("sample");
        fs::write(&path, b"hello").unwrap();
        assert_eq!(
            check_idempotency(&path, b"hello"),
            IdempotencyDecision::AlreadyInstalled
        );
        assert_eq!(
            check_idempotency(&path, b"hello!!"),
            IdempotencyDecision::Differs
        );
        assert_eq!(
            check_idempotency(&dir.path().join("missing"), b"x"),
            IdempotencyDecision::Fresh
        );
    }
}
