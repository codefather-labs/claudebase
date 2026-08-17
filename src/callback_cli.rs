//! `claudebase daemon callback …` — operator-facing control of the HTTP
//! endpoint that lets external systems write into a session's input.
//!
//! Every command here reads or writes `chat.db` directly rather than going
//! through the daemon: the operator must be able to read a token and see the
//! configuration when the daemon is down, which is exactly when they are
//! debugging why a callback did not arrive.

use anyhow::{bail, Context, Result};

use crate::daemon::callback;
use crate::daemon::chat;

/// `daemon callback enable [--bind host:port]`
pub fn enable(bind: &str, allow_remote: bool) -> Result<String> {
    let addr: std::net::SocketAddr = bind
        .parse()
        .with_context(|| format!("`{bind}` is not a host:port address"))?;

    if !addr.ip().is_loopback() && !allow_remote {
        bail!(
            "refusing to bind {bind}: a callback endpoint reachable from the network writes into \n\
             a session running with permissions skipped, and the token travels in cleartext over \n\
             plain HTTP.\n\n\
             Prefer keeping the daemon on loopback and tunnelling:\n  \
             ssh -N -L {port}:127.0.0.1:{port} <user>@<this-host>\n\n\
             If you really mean it, pass --i-know-this-is-remote.",
            port = addr.port()
        );
    }

    let conn = chat::open_chat_db().context("open chat.db")?;
    callback::set_bind(&conn, bind)?;

    let mut out = format!(
        "callback endpoint set to {bind}\n\
         restart the daemon for it to take effect: claudebase daemon restart"
    );
    if !addr.ip().is_loopback() {
        out.push_str(
            "\n\nWARNING: this address is reachable from the network. The X-Api-Token travels \
             in cleartext; anyone who can see the traffic can replay it.",
        );
    }
    Ok(out)
}

/// `daemon callback disable`
pub fn disable() -> Result<String> {
    let conn = chat::open_chat_db().context("open chat.db")?;
    callback::clear_bind(&conn)?;
    Ok("callback endpoint disabled (tokens kept, so re-enabling does not break scripts)\n\
        restart the daemon to stop listening: claudebase daemon restart"
        .to_string())
}

/// `daemon callback status`
///
/// Read-only: this command only reads, and `open_chat_db` runs the schema
/// ensure — a WRITE transaction — on every open. Opening read-write to read
/// made `status` fail with `database is locked` whenever the daemon happened to
/// be writing, which is exactly when an operator runs it. Same reasoning that
/// moved the nick lookup off the write path.
pub fn status() -> Result<String> {
    // A read-only handle does not create the file, so on a machine where the
    // daemon has never run there is nothing to open. That is a normal state
    // right after installing, not an error worth a database message.
    let Ok(conn) = chat::open_chat_db_readonly() else {
        return Ok("bind: (disabled — enable with `claudebase daemon callback enable`)\n\n\
                   no tokens yet — they are minted when a session registers, and the daemon \
                   has not run here yet\n"
            .to_string());
    };
    let bind = callback::configured_bind(&conn)?;

    let mut out = String::new();
    match &bind {
        Some(b) => out.push_str(&format!("bind: {b}\n")),
        None => out.push_str("bind: (disabled — enable with `claudebase daemon callback enable`)\n"),
    }

    let mut stmt = conn
        .prepare("SELECT nick, token, created_at FROM callback_tokens ORDER BY nick")
        .context("read tokens")?;
    let rows: Vec<(String, String, i64)> = stmt
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?
        .collect::<rusqlite::Result<Vec<_>>>()?;

    if rows.is_empty() {
        out.push_str("\nno tokens yet — they are minted when a session registers\n");
        return Ok(out);
    }

    out.push_str("\nNICK                 FINGERPRINT\n");
    for (nick, token, _) in &rows {
        out.push_str(&format!("{:<20} {}\n", nick, callback::fingerprint(token)));
    }
    out.push_str(
        "\nfingerprints identify a token without being usable as one — compare them against \
         a script's copy.\nread the token itself with: claudebase daemon callback token <nick> --reveal\n",
    );
    Ok(out)
}

/// `daemon callback token <nick> [--reveal]`
///
/// Read-only, for the same reason as `status`.
pub fn token(nick: &str, reveal: bool) -> Result<String> {
    let conn = chat::open_chat_db_readonly().map_err(|_| {
        anyhow::anyhow!(
            "no callback tokens yet — the daemon has not run on this machine.\n\
             Start a session with `claudebase run`, which registers and mints one."
        )
    })?;
    let Some(tok) = callback::token_for(&conn, nick)? else {
        bail!(
            "no callback token for `{nick}` — tokens are minted when a session registers.\n\
             Check the nick with `claudebase agent list`."
        );
    };
    let bind = callback::configured_bind(&conn)?;

    if reveal {
        let target = bind.unwrap_or_else(|| "<enable the endpoint first>".to_string());
        return Ok(format!(
            "{tok}\n\n\
             curl -sS -X POST 'http://{target}/callback/{nick}?label=<label>' \\\n  \
             -H 'X-Api-Token: {tok}' \\\n  \
             --data-binary 'your message'\n"
        ));
    }

    Ok(format!(
        "nick: {nick}\nfingerprint: {}\nfile: ~/.claude/callback-tokens/{nick}\n\n\
         the token itself is not printed by default — whatever captured this output would keep it.\n\
         pass --reveal to print it.",
        callback::fingerprint(&tok)
    ))
}

/// `daemon callback rotate <nick>`
pub fn rotate(nick: &str, reveal: bool) -> Result<String> {
    let conn = chat::open_chat_db().context("open chat.db")?;
    let fresh = callback::rotate(&conn, nick, chat::now_millis())?;
    if reveal {
        return Ok(format!(
            "{fresh}\n\nthe previous token for `{nick}` no longer works — update anything holding it."
        ));
    }
    Ok(format!(
        "rotated token for `{nick}`\nfingerprint: {}\n\n\
         the previous token no longer works — update anything holding it.\n\
         read the new one with: claudebase daemon callback token {nick} --reveal",
        callback::fingerprint(&fresh)
    ))
}
