//! The Claude subscription token of THIS machine (docs/design/auth-token.md).
//!
//! `claudebase auth setup-token` hands a token from `claude setup-token` to the
//! daemon in use — this machine's, or a remote one over TLS — and that daemon
//! makes it the machine's: stored with its secrets, given to every `claude`
//! that `claudebase run` starts here (`CLAUDE_CODE_OAUTH_TOKEN`), and written
//! into the user's persistent environment so a bare `claude` has it too.
//!
//! No tool ever returns the token; status shows its last four characters.

// The shell-profile helpers are Unix-only; Windows uses the registry.
#![cfg_attr(windows, allow(dead_code))]

use std::path::{Path, PathBuf};

use anyhow::{anyhow, bail, Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::daemon::remote::{claudebase_dir, write_private, Peer};

/// What Claude Code reads a long-lived token from.
pub const TOKEN_ENV: &str = "CLAUDE_CODE_OAUTH_TOKEN";

const BLOCK_BEGIN: &str = "# >>> claudebase auth >>>";
const BLOCK_END: &str = "# <<< claudebase auth <<<";

#[derive(Debug, Serialize, Deserialize)]
struct Stored {
    token: String,
    set_at: i64,
    set_by: String,
    applied: Vec<String>,
}

fn store_path() -> PathBuf {
    claudebase_dir().join("auth.toml")
}

/// Sourced by the shell profiles; holds the export, so the profiles never do.
fn env_file() -> PathBuf {
    claudebase_dir().join("auth.env")
}

fn home() -> PathBuf {
    std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."))
}

/// The token `claudebase run` passes to `claude` on this machine, if one was set.
pub fn stored_token() -> Option<String> {
    let s = std::fs::read_to_string(store_path()).ok()?;
    toml::from_str::<Stored>(&s).ok().map(|st| st.token)
}

/// A token is one opaque word: it goes into a shell file inside single quotes
/// and into the registry, so anything that could break out of those is refused.
fn check_token(token: &str) -> Result<()> {
    if token.len() < 20 || token.len() > 4096 {
        bail!("that does not look like a token from `claude setup-token` (length {})", token.len());
    }
    if !token.chars().all(|c| c.is_ascii_graphic() && c != '\'' && c != '"' && c != '\\' && c != '`' && c != '$') {
        bail!("the token has characters a token from `claude setup-token` does not have (spaces, quotes, …)");
    }
    Ok(())
}

fn tail(token: &str) -> String {
    format!("…{}", &token[token.len().saturating_sub(4)..])
}

pub async fn handle(tool: &str, args: Value, peer: &Peer) -> Result<Value> {
    let tool = tool.to_owned();
    let by = match peer {
        Peer::Local => "this machine".to_owned(),
        Peer::Remote { client } => format!("remote client `{client}`"),
    };
    tokio::task::spawn_blocking(move || match tool.as_str() {
        "auth_set_token" => {
            let token = args.get("token").and_then(Value::as_str).map(str::trim).unwrap_or("");
            set_token(token, &by)
        }
        "auth_status" => status(),
        "auth_clear_token" => clear(),
        other => bail!("unknown auth tool `{other}`"),
    })
    .await
    .map_err(|e| anyhow!("auth task failed: {e}"))?
}

fn set_token(token: &str, by: &str) -> Result<Value> {
    check_token(token)?;
    let applied = apply_user_env(token)?;
    let st = Stored { token: token.to_owned(), set_at: chrono::Utc::now().timestamp(), set_by: by.to_owned(), applied };
    write_private(&store_path(), toml::to_string(&st)?.as_bytes())?;
    tracing::info!(by, tail = %tail(token), "auth: Claude token set");
    Ok(json!({
        "set": true,
        "tail": tail(token),
        "host": crate::daemon::remote::hostname(),
        "applied": st.applied,
    }))
}

fn status() -> Result<Value> {
    let Ok(s) = std::fs::read_to_string(store_path()) else {
        return Ok(json!({ "set": false, "host": crate::daemon::remote::hostname() }));
    };
    let st: Stored = toml::from_str(&s).context("auth.toml is not valid")?;
    Ok(json!({
        "set": true,
        "tail": tail(&st.token),
        "set_at": st.set_at,
        "set_by": st.set_by,
        "applied": st.applied,
        "host": crate::daemon::remote::hostname(),
    }))
}

fn clear() -> Result<Value> {
    let had = store_path().exists();
    let _ = std::fs::remove_file(store_path());
    let removed = remove_user_env()?;
    Ok(json!({ "cleared": had, "removed": removed, "host": crate::daemon::remote::hostname() }))
}

// ---------------------------------------------------------------------------
// The user's persistent environment
// ---------------------------------------------------------------------------

/// Where a bare `claude` will find the token: per user, since the daemon runs
/// as the user and machine-wide stores need admin rights. Returns what was set.
fn apply_user_env(token: &str) -> Result<Vec<String>> {
    let mut applied = vec!["claudebase run (CLAUDE_CODE_OAUTH_TOKEN for every session started here)".to_owned()];
    #[cfg(windows)]
    {
        // HKCU\Environment; setx also tells running Explorer, so new processes see it.
        let ok = std::process::Command::new("setx").arg(TOKEN_ENV).arg(token).output()
            .map(|o| o.status.success()).unwrap_or(false);
        if ok {
            applied.push(format!("HKCU\\Environment\\{TOKEN_ENV} (new processes)"));
        } else {
            tracing::warn!("auth: setx failed; only claudebase run sessions get the token");
        }
    }
    #[cfg(unix)]
    {
        write_private(&env_file(), format!("export {TOKEN_ENV}='{token}'\n").as_bytes())?;
        let conf = home().join(".config/environment.d/claudebase-auth.conf");
        write_private(&conf, format!("{TOKEN_ENV}={token}\n").as_bytes())?;
        applied.push(format!("{} (systemd user sessions, next login)", conf.display()));
        for profile in profiles() {
            ensure_block(&profile)?;
            applied.push(format!("{} (login shells, next login)", profile.display()));
        }
    }
    Ok(applied)
}

fn remove_user_env() -> Result<Vec<String>> {
    let mut removed = Vec::new();
    #[cfg(windows)]
    {
        let ok = std::process::Command::new("reg")
            .args(["delete", "HKCU\\Environment", "/v", TOKEN_ENV, "/f"])
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false);
        if ok {
            removed.push(format!("HKCU\\Environment\\{TOKEN_ENV}"));
        }
    }
    #[cfg(unix)]
    {
        for f in [env_file(), home().join(".config/environment.d/claudebase-auth.conf")] {
            if std::fs::remove_file(&f).is_ok() {
                removed.push(f.display().to_string());
            }
        }
        for profile in profiles() {
            if remove_block(&profile)? {
                removed.push(profile.display().to_string());
            }
        }
    }
    Ok(removed)
}

/// The login-shell files that exist (bash reads `~/.bash_profile` INSTEAD of
/// `~/.profile` when it has one), plus `~/.profile` always.
#[cfg(unix)]
fn profiles() -> Vec<PathBuf> {
    let h = home();
    let mut out = vec![h.join(".profile")];
    for name in [".bash_profile", ".zprofile"] {
        let p = h.join(name);
        if p.exists() || (name == ".zprofile" && cfg!(target_os = "macos")) {
            out.push(p);
        }
    }
    out
}

fn block() -> String {
    format!(
        "{BLOCK_BEGIN}\n[ -r \"$HOME/.claude/claudebase/auth.env\" ] && . \"$HOME/.claude/claudebase/auth.env\"\n{BLOCK_END}\n"
    )
}

/// Add the marked block unless it is there. The block only sources the 0600
/// file; the token never sits in a profile.
fn ensure_block(path: &Path) -> Result<()> {
    let text = std::fs::read_to_string(path).unwrap_or_default();
    if text.contains(BLOCK_BEGIN) {
        return Ok(());
    }
    let mut new = text;
    if !new.is_empty() && !new.ends_with('\n') {
        new.push('\n');
    }
    new.push_str(&block());
    std::fs::write(path, new).with_context(|| format!("write {}", path.display()))
}

fn remove_block(path: &Path) -> Result<bool> {
    let Ok(text) = std::fs::read_to_string(path) else { return Ok(false) };
    let (Some(a), Some(b)) = (text.find(BLOCK_BEGIN), text.find(BLOCK_END)) else { return Ok(false) };
    if b < a {
        return Ok(false);
    }
    let mut end = b + BLOCK_END.len();
    if text[end..].starts_with('\n') {
        end += 1;
    }
    let new = format!("{}{}", &text[..a], &text[end..]);
    std::fs::write(path, new).with_context(|| format!("write {}", path.display()))?;
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tokens_are_checked_before_they_reach_a_shell_file() {
        assert!(check_token("sk-ant-oat01-abcdefghijklmnopqrstuvwxyz0123456789").is_ok());
        for bad in ["short", "sk-ant-oat01-abc def ghij klmnopqrstuv", "sk-ant-oat01-abc'; rm -rf ~;'xxxxxxxx", "sk-ant-$(id)-xxxxxxxxxxxxxxxxxxxx"] {
            assert!(check_token(bad).is_err(), "{bad}");
        }
        assert_eq!(tail("sk-ant-oat01-abcdefghijklmnopqrstuvwxyz0123456789"), "…6789");
    }

    #[test]
    fn the_profile_block_is_added_once_and_removed_cleanly() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join(".profile");
        std::fs::write(&p, "export PATH=$PATH:~/bin").unwrap();
        ensure_block(&p).unwrap();
        ensure_block(&p).unwrap();
        let text = std::fs::read_to_string(&p).unwrap();
        assert_eq!(text.matches(BLOCK_BEGIN).count(), 1);
        assert!(!text.contains("sk-ant"), "the token never sits in a profile");
        assert!(remove_block(&p).unwrap());
        assert_eq!(std::fs::read_to_string(&p).unwrap(), "export PATH=$PATH:~/bin\n");
        assert!(!remove_block(&p).unwrap());
    }
}
