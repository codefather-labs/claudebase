//! `claudebase daemon remote …` (docs/design/remote-daemon.md).
//!
//! The serving side (`enable`, `issue`, `revoke`, `status`) writes `chat.db`
//! and the certificate directly, like `daemon callback …`: it administers this
//! machine's daemon and has to work while that daemon is down. It is never
//! reachable over a connection — remote or local — so a token holder cannot
//! issue themselves more tokens.
//!
//! The joining side (`add`, `list`, `remove`, `default`, `test`) keeps the
//! profiles in `~/.claude/claudebase/remotes.toml` (0600: the tokens are in it).

use std::io::Read;

use anyhow::{bail, Context, Result};

use crate::daemon::chat;
use crate::daemon::remote::{self, Endpoint, Profiles, Remote};

/// `daemon remote enable [--bind host:port]`
pub fn enable(bind: &str) -> Result<String> {
    let addr: std::net::SocketAddr = bind.parse().with_context(|| format!("`{bind}` is not a host:port address"))?;
    let pin = remote::ensure_certificate()?;
    let conn = chat::open_chat_db().context("open chat.db")?;
    remote::set_bind(&conn, bind)?;
    let mut out = format!(
        "remote listener set to {bind} (TLS, certificate pin {pin})\n\
         restart the daemon for it to take effect: claudebase daemon restart\n\n\
         give each client machine its own token:\n  claudebase daemon remote issue <client-name> [--host <name clients reach this machine by>]"
    );
    if !addr.ip().is_loopback() {
        out.push_str(
            "\n\nThis port is reachable from the network. Anyone holding a token can type into every \
             session on this daemon, including sessions running with permissions skipped: treat a \
             token like an SSH key to this machine. Open the port only to the machines that need it.",
        );
    }
    Ok(out)
}

/// `daemon remote disable`
pub fn disable() -> Result<String> {
    let conn = chat::open_chat_db().context("open chat.db")?;
    remote::clear_bind(&conn)?;
    Ok("remote listener disabled (tokens kept)\nrestart the daemon to stop listening: claudebase daemon restart".into())
}

/// `daemon remote status`
pub fn status() -> Result<String> {
    let conn = chat::open_chat_db_readonly().context("open chat.db")?;
    let bind = remote::configured_bind(&conn).ok().flatten();
    let mut out = match &bind {
        Some(b) => format!("listening: {b} (TLS) — as of the daemon's last start\n"),
        None => "listening: no (claudebase daemon remote enable)\n".to_string(),
    };
    match remote::certificate_pin() {
        Ok(pin) => out.push_str(&format!("certificate pin: {pin}\n")),
        Err(_) => out.push_str("certificate: none yet\n"),
    }
    drop(conn);
    let tokens = remote::list_tokens(&chat::open_chat_db().context("open chat.db")?, remote::TokenKind::Remote)?;
    if tokens.is_empty() {
        out.push_str("clients: none (claudebase daemon remote issue <client-name>)\n");
    } else {
        out.push_str("clients:\n");
        for t in tokens {
            let used = t.last_used_at.map(stamp).unwrap_or_else(|| "never".into());
            out.push_str(&format!("  {:<20} issued {}  last used {}\n", t.name, stamp(t.created_at), used));
        }
    }
    Ok(out)
}

/// `daemon remote issue <client> [--host H]`
pub fn issue(client: &str, host: Option<&str>) -> Result<String> {
    let conn = chat::open_chat_db().context("open chat.db")?;
    let bind = remote::configured_bind(&conn)?.ok_or_else(|| {
        anyhow::anyhow!("this daemon does not listen for other machines yet: claudebase daemon remote enable")
    })?;
    let pin = remote::certificate_pin()?;
    let port = bind
        .rsplit_once(':')
        .and_then(|(_, p)| p.parse::<u16>().ok())
        .unwrap_or(remote::DEFAULT_PORT);
    let token = remote::issue_token(&conn, remote::TokenKind::Remote, client)?;
    let host = host.map(str::to_owned).unwrap_or_else(remote::hostname);
    let url = Remote { host, port, pin, token }.url();
    Ok(format!(
        "token issued for `{client}` (any earlier one for that name stops working)\n\n\
         on {client}, run:\n  claudebase daemon remote add <name>\nand paste this line when asked:\n\n{url}\n\n\
         It is shown once and grants access to every session on this daemon: pass it privately \
         (not through a chat or an agent). Revoke with: claudebase daemon remote revoke {client}"
    ))
}

/// `daemon remote revoke <client>`
pub fn revoke(client: &str) -> Result<String> {
    let conn = chat::open_chat_db().context("open chat.db")?;
    if remote::revoke_token(&conn, remote::TokenKind::Remote, client)? {
        Ok(format!(
            "`{client}` revoked: new connections with its token are refused; one already open \
             stays until it drops or the daemon restarts (claudebase daemon restart)"
        ))
    } else {
        bail!("no client `{client}` (claudebase daemon remote status)")
    }
}

/// `daemon remote add <name> [url|-]`
pub fn add(name: &str, url: Option<&str>) -> Result<String> {
    remote::check_profile_name(name)?;
    let url = match url {
        Some(u) if u != "-" => u.to_owned(),
        _ => {
            eprintln!("paste the connection string from `claudebase daemon remote issue`, then Enter:");
            let mut s = String::new();
            std::io::stdin().read_line(&mut s).context("read stdin")?;
            if s.trim().is_empty() {
                std::io::stdin().read_to_string(&mut s)?;
            }
            s
        }
    };
    let r = Remote::parse(crate::input::strip_bom(&url))?;
    let mut p = Profiles::load()?;
    let replaced = p.remotes.insert(name.to_owned(), r.clone()).is_some();
    p.save()?;
    Ok(format!(
        "{} daemon `{name}` at {}\ncheck it:  claudebase daemon remote test {name}\nuse it:    claudebase run --daemon {name}",
        if replaced { "replaced" } else { "added" },
        r.display()
    ))
}

/// `daemon remote list`
pub fn list() -> Result<String> {
    let p = Profiles::load()?;
    let default = p.default.clone().unwrap_or_else(|| remote::LOCAL.to_string());
    let mut out = format!("{} local (this machine's daemon){}\n", mark(&default, remote::LOCAL), "");
    for (name, r) in &p.remotes {
        out.push_str(&format!("{} {name}  {}\n", mark(&default, name), r.display()));
    }
    out.push_str("\n* = used when none is named (claudebase daemon remote default <name|local>)");
    Ok(out)
}

fn mark(default: &str, name: &str) -> &'static str {
    if default == name { "*" } else { " " }
}

/// `daemon remote remove <name>`
pub fn remove(name: &str) -> Result<String> {
    let mut p = Profiles::load()?;
    if p.remotes.remove(name).is_none() {
        bail!("no daemon profile `{name}`");
    }
    let mut out = format!("removed `{name}`");
    if p.default.as_deref() == Some(name) {
        p.default = None;
        out.push_str("; it was the default, which is now local");
    }
    p.save()?;
    Ok(out)
}

/// `daemon remote default <name|local>`
pub fn set_default(name: &str) -> Result<String> {
    let mut p = Profiles::load()?;
    if name == remote::LOCAL {
        p.default = None;
    } else if p.remotes.contains_key(name) {
        p.default = Some(name.to_owned());
    } else {
        bail!("no daemon profile `{name}` (claudebase daemon remote list)");
    }
    p.save()?;
    Ok(format!(
        "default daemon: {name}\nevery `claudebase run` and claudebase command without --daemon / {} now uses it",
        remote::DAEMON_ENV
    ))
}

/// `daemon remote test [name]`
pub async fn test(name: Option<&str>) -> Result<String> {
    let endpoint = match name {
        Some(n) => Endpoint::named(Some(n))?,
        None => Endpoint::resolve()?,
    };
    let started = std::time::Instant::now();
    let mut client = crate::daemon::client::DaemonClient::connect_to(&endpoint).await?;
    let alive = client.call_tool("agent_list_alive", serde_json::json!({})).await?;
    let agents = alive.get("agents").and_then(|a| a.as_array()).map(Vec::len).unwrap_or(0);
    Ok(format!(
        "{}: connected in {} ms, {agents} session(s) online",
        endpoint.describe(),
        started.elapsed().as_millis()
    ))
}

fn stamp(secs: i64) -> String {
    chrono::DateTime::<chrono::Utc>::from_timestamp(secs, 0)
        .map(|d| d.format("%Y-%m-%d %H:%MZ").to_string())
        .unwrap_or_default()
}

/// `claudebase auth …` — sent to the daemon in use, applied on its machine.
pub async fn auth(sub: crate::cli::AuthSubcommand) -> Result<String> {
    use crate::cli::AuthSubcommand;
    let endpoint = Endpoint::resolve()?;
    let (tool, args) = match sub {
        AuthSubcommand::SetupToken(a) => {
            let token = match a.token.as_deref() {
                Some(t) if t != "-" => t.to_owned(),
                _ => {
                    eprintln!("paste the token from `claude setup-token`, then Enter:");
                    let mut s = String::new();
                    std::io::stdin().read_line(&mut s).context("read stdin")?;
                    s
                }
            };
            ("auth_set_token", serde_json::json!({ "token": crate::input::strip_bom(token.trim()) }))
        }
        AuthSubcommand::Status => ("auth_status", serde_json::json!({})),
        AuthSubcommand::ClearToken => ("auth_clear_token", serde_json::json!({})),
    };
    let mut client = crate::daemon::client::DaemonClient::connect_to(&endpoint).await?;
    let v = client.call_tool(tool, args).await?;
    let host = v["host"].as_str().unwrap_or("?");
    let list = |key: &str| -> String {
        v[key].as_array().into_iter().flatten().filter_map(|x| x.as_str()).map(|x| format!("\n  {x}")).collect()
    };
    Ok(match tool {
        "auth_set_token" => format!(
            "Claude token {} set on {host} (daemon {}). Applied to:{}\n\
             New `claudebase run` sessions there are signed in now; a bare `claude` after the next login.",
            v["tail"].as_str().unwrap_or("?"),
            endpoint.describe(),
            list("applied")
        ),
        "auth_status" if v["set"].as_bool() == Some(true) => format!(
            "{host}: Claude token {} set {} by {}. Applied to:{}",
            v["tail"].as_str().unwrap_or("?"),
            v["set_at"].as_i64().map(stamp).unwrap_or_default(),
            v["set_by"].as_str().unwrap_or("?"),
            list("applied")
        ),
        "auth_status" => format!("{host}: no Claude token set (claudebase auth setup-token)"),
        _ => format!(
            "{host}: Claude token {}. Removed:{}\nSessions already running keep theirs until they end.",
            if v["cleared"].as_bool() == Some(true) { "removed" } else { "was not set" },
            list("removed")
        ),
    })
}
