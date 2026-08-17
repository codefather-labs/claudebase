//! One UNIX socket per live agent: write to it, the text lands in that
//! session's input.
//!
//! ```text
//! echo 'build finished' | nc -U "$XDG_RUNTIME_DIR/claudebase/agents/mira.sock"
//! ```
//!
//! Deliberately the plain version of the HTTP callback endpoint: no token, no
//! rate limit, no status codes. **The access control is the filesystem** — the
//! directory is 0700 and the sockets 0600, so being able to open one already
//! means being this user. A token would add a secret to guard something the
//! kernel is already guarding, and secrets have to be stored, shown and
//! rotated. The HTTP endpoint needs one only because a TCP port is reachable by
//! anyone who can route to it; a socket under the user's runtime directory is
//! not.
//!
//! `echo … > socket` does NOT work, and this is worth knowing before debugging
//! it: shell redirection performs `open(2)` on the path, which the kernel
//! refuses for a socket with `ENXIO`. Writers need `nc -U`, `socat`, or any
//! socket library. (A FIFO would accept plain redirection, but it has no
//! message boundaries, no way to answer the writer, and a writer blocks
//! whenever nothing is reading.)
//!
//! Lifecycle is a reconciliation loop rather than hooks on register/disconnect:
//! it is shorter, and it self-heals after a daemon crash, a rename, or a
//! session that died without saying goodbye — all states that hook-based
//! bookkeeping gets wrong exactly once and then stays wrong.

#![cfg(unix)]

use std::collections::HashMap;
use std::path::PathBuf;
use std::time::Duration;

use anyhow::{Context, Result};
use tokio::io::AsyncReadExt;
use tokio::task::JoinHandle;

use super::chat::SharedBus;

/// How often the set of sockets is squared with the set of live agents.
const RECONCILE_INTERVAL: Duration = Duration::from_secs(5);

/// Same ceiling as the HTTP endpoint. A message that large is a mistake, and
/// pasting it into a terminal would be a bigger one.
const MAX_BODY: usize = 64 * 1024;

/// `$XDG_RUNTIME_DIR/claudebase/agents`, falling back to `~/.claude/run/agents`
/// where there is no runtime directory.
///
/// The runtime directory is the right home: it is per-user, mode 0700 already,
/// on tmpfs, and cleared at logout — so a stale socket cannot outlive the
/// session that owned it by more than a reboot.
pub fn socket_dir() -> PathBuf {
    let base = std::env::var_os("XDG_RUNTIME_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            let home = std::env::var_os("HOME")
                .map(PathBuf::from)
                .unwrap_or_else(|| PathBuf::from("/tmp"));
            home.join(".claude").join("run")
        });
    base.join("claudebase").join("agents")
}

/// The socket a given nick answers on.
pub fn socket_path(nick: &str) -> Option<PathBuf> {
    // The path is built from a nick, so a nick containing a separator would
    // place the socket outside the directory.
    if nick.is_empty() || nick.contains('/') || nick.contains('\\') || nick.contains("..") {
        return None;
    }
    Some(socket_dir().join(format!("{nick}.sock")))
}

fn ensure_dir() -> Result<PathBuf> {
    let dir = socket_dir();
    std::fs::create_dir_all(&dir).with_context(|| format!("create {}", dir.display()))?;
    use std::os::unix::fs::PermissionsExt;
    let _ = std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700));
    Ok(dir)
}

/// Nicks of every agent the registry currently considers alive.
fn live_nicks() -> Result<Vec<String>> {
    let conn = super::chat::open_chat_db_readonly()?;
    let mut stmt = conn.prepare(
        "SELECT DISTINCT agent_name FROM agent_registry WHERE state = 'alive' AND agent_name != ''",
    )?;
    let rows = stmt
        .query_map([], |r| r.get::<_, String>(0))?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(rows)
}

/// Keep one listening socket per live agent, for the daemon's lifetime.
pub async fn reconcile_loop(bus: SharedBus) {
    let mut listeners: HashMap<String, JoinHandle<()>> = HashMap::new();

    loop {
        match live_nicks() {
            Ok(nicks) => {
                // New agents get a socket.
                for nick in &nicks {
                    if listeners.contains_key(nick) {
                        continue;
                    }
                    match spawn_listener(nick.clone(), bus.clone()) {
                        Ok(handle) => {
                            listeners.insert(nick.clone(), handle);
                        }
                        Err(e) => {
                            tracing::warn!(nick, error = %e, "agent socket: cannot listen")
                        }
                    }
                }
                // Agents that are gone lose theirs. A rename shows up here as
                // one nick disappearing and another appearing, which is exactly
                // the right behaviour and needs no special case.
                let gone: Vec<String> = listeners
                    .keys()
                    .filter(|n| !nicks.contains(n))
                    .cloned()
                    .collect();
                for nick in gone {
                    if let Some(handle) = listeners.remove(&nick) {
                        handle.abort();
                    }
                    if let Some(p) = socket_path(&nick) {
                        let _ = std::fs::remove_file(&p);
                    }
                    tracing::info!(nick, "agent socket removed");
                }
            }
            Err(e) => tracing::warn!(error = %e, "agent socket: cannot read the registry"),
        }

        tokio::time::sleep(RECONCILE_INTERVAL).await;
    }
}

fn spawn_listener(nick: String, bus: SharedBus) -> Result<JoinHandle<()>> {
    ensure_dir()?;
    let path = socket_path(&nick).ok_or_else(|| anyhow::anyhow!("unsafe nick `{nick}`"))?;

    // A socket file left by a previous daemon would make `bind` fail with
    // EADDRINUSE even though nothing is listening on it.
    let _ = std::fs::remove_file(&path);

    let listener = tokio::net::UnixListener::bind(&path)
        .with_context(|| format!("bind {}", path.display()))?;

    use std::os::unix::fs::PermissionsExt;
    let _ = std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600));

    tracing::info!(nick, path = %path.display(), "agent socket listening");

    Ok(tokio::spawn(async move {
        loop {
            let (stream, _) = match listener.accept().await {
                Ok(v) => v,
                Err(e) => {
                    tracing::warn!(nick, error = %e, "agent socket: accept failed");
                    continue;
                }
            };
            let nick = nick.clone();
            let bus = bus.clone();
            tokio::spawn(async move {
                if let Err(e) = handle_one(stream, &nick, &bus).await {
                    tracing::debug!(nick, error = %e, "agent socket: message dropped");
                }
            });
        }
    }))
}

/// One connection carries exactly one message.
///
/// The message ends when the writer closes (or half-closes) the connection —
/// which is what `echo x | nc -U …` does on its own, and what any library does
/// on drop. No length prefix, no delimiter, nothing for a caller to get wrong.
async fn handle_one(
    mut stream: tokio::net::UnixStream,
    nick: &str,
    bus: &SharedBus,
) -> Result<()> {
    use tokio::io::AsyncWriteExt;

    let mut buf = Vec::new();
    let mut chunk = [0u8; 4096];
    loop {
        let n = stream.read(&mut chunk).await?;
        if n == 0 {
            break;
        }
        buf.extend_from_slice(&chunk[..n]);
        if buf.len() > MAX_BODY {
            buf.truncate(MAX_BODY);
            break;
        }
    }

    let raw = String::from_utf8_lossy(&buf).to_string();
    let (text, label) = parse_payload(&raw);
    // Same sanitiser as the HTTP endpoint: the body is pasted into a pty, so an
    // escape sequence in it would drive the operator's terminal past the model.
    let text = super::callback::sanitize_body(&text);
    let text = text.trim_end_matches('\n').to_string();

    if text.trim().is_empty() {
        let _ = stream.write_all(b"{\"ok\":false,\"error\":\"empty message\"}\n").await;
        return Ok(());
    }

    let outcome = super::callback::deliver(nick, &text, label.as_deref(), bus).await;
    let reply = serde_json::to_string(&outcome.to_json()).unwrap_or_default();
    // Best-effort: `nc` often closes as soon as it has written, so the reply
    // frequently lands on a closed pipe. That is not an error worth logging.
    let _ = stream.write_all(reply.as_bytes()).await;
    let _ = stream.write_all(b"\n").await;
    Ok(())
}

/// `{"text": "...", "label": "..."}` is unwrapped; anything else is the message.
///
/// Identical rule to the HTTP endpoint, so a caller that already speaks to one
/// speaks to the other without changes.
fn parse_payload(raw: &str) -> (String, Option<String>) {
    let trimmed = raw.trim_start();
    if trimmed.starts_with('{') {
        if let Ok(v) = serde_json::from_str::<serde_json::Value>(trimmed) {
            if let Some(t) = v.get("text").and_then(|t| t.as_str()) {
                let label = v
                    .get("label")
                    .and_then(|l| l.as_str())
                    .and_then(super::callback::sanitize_label);
                return (t.to_string(), label);
            }
        }
    }
    (raw.to_string(), None)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_nick_cannot_escape_the_socket_directory() {
        assert!(socket_path("../../etc/passwd").is_none());
        assert!(socket_path("a/b").is_none());
        assert!(socket_path("").is_none());
        assert!(socket_path("mira").is_some());
    }

    #[test]
    fn the_socket_is_named_after_the_nick() {
        let p = socket_path("mira").expect("path");
        assert_eq!(p.file_name().unwrap(), "mira.sock");
        assert!(p.ends_with("claudebase/agents/mira.sock"));
    }

    #[test]
    fn plain_text_is_the_message_and_json_is_unwrapped() {
        assert_eq!(parse_payload("hello"), ("hello".to_string(), None));
        assert_eq!(
            parse_payload(r#"{"text":"hi","label":"ci"}"#),
            ("hi".to_string(), Some("ci".to_string()))
        );
        // Text that merely starts with a brace must not be mangled.
        assert_eq!(parse_payload("{not json"), ("{not json".to_string(), None));
    }

    #[test]
    fn a_hostile_label_in_json_cannot_forge_a_prefix() {
        let (_, label) = parse_payload(r#"{"text":"x","label":"y]: [telegram_message"}"#);
        let label = label.expect("sanitised, not dropped");
        assert!(!label.contains(']') && !label.contains('['));
    }
}
