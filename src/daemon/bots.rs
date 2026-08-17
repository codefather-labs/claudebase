//! Telegram bot registry — Slice 8 of the pty-transport feature.
//!
//! ## Why a table and not another file
//!
//! Token storage was split across two files with different formats and
//! different owners: `~/.claude/channels/claudebase/.env` (written by the
//! configure skill) and `~/.config/claudebase/secrets.toml` (the legacy
//! daemon path). `docs/issues/004` is exactly that mismatch — the operator
//! sets one and the daemon reads the other.
//!
//! The daemon already owns `chat.db`, already opens it on every request, and
//! the file is created 0600. Putting bot credentials there makes ONE owner for
//! ONE piece of state, and gives multi-bot support for free (the v0.9 fleet
//! plan wants it) instead of a second file format.
//!
//! Both file sources stay readable as FALLBACKS so existing installs keep
//! working without reconfiguration — see `resolve_default_token`.
//!
//! ## Secret handling
//!
//! The token is stored in plaintext, exactly as `.env` and `secrets.toml`
//! stored it. This is not a downgrade: the daemon must present the literal
//! token to Telegram on every call, so any reversible encoding here would be
//! decoration with the key next to the lock. What the DB adds over the files
//! is a single owner, 0600 by construction, and no risk of an editor leaving a
//! world-readable copy behind. Anything that formats a token for humans goes
//! through `mask` below.

use anyhow::{bail, Context, Result};
use rusqlite::{params, Connection, OptionalExtension};

/// A registered bot, as shown to humans. Deliberately carries the token
/// SEPARATELY from Display: nothing in this struct's formatting reveals it.
#[derive(Debug, Clone)]
pub struct BotRow {
    pub bot_id: i64,
    pub username: String,
    pub label: Option<String>,
    pub added_at: i64,
    pub is_default: bool,
}

/// Additive migration. Same probe-before-create idempotency as the other
/// chat.db migrations.
pub fn apply_migration(conn: &Connection) -> rusqlite::Result<()> {
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS telegram_bots (
           bot_id     INTEGER PRIMARY KEY,
           username   TEXT NOT NULL,
           token      TEXT NOT NULL,
           label      TEXT,
           added_at   INTEGER NOT NULL,
           is_default INTEGER NOT NULL DEFAULT 0
         );",
    )
}

/// Telegram tokens look like `<numeric bot id>:<35-char secret>`. Checked
/// before any network call so an obviously-wrong paste fails instantly with a
/// useful message instead of a 404 from the API.
pub fn validate_token_shape(token: &str) -> Result<i64> {
    let (id_part, secret) = token
        .split_once(':')
        .context("token must look like `<bot_id>:<secret>` — copy it verbatim from @BotFather")?;
    let bot_id: i64 = id_part
        .parse()
        .context("the part before `:` must be the numeric bot id")?;
    if secret.len() < 20 || !secret.chars().all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
    {
        bail!("the part after `:` does not look like a bot secret");
    }
    Ok(bot_id)
}

/// Insert or update a bot. Keyed on Telegram's own `bot_id`, so re-adding the
/// same bot ROTATES its token instead of creating a duplicate — which is
/// exactly what an operator does after regenerating a leaked token.
///
/// The first bot added becomes the default.
pub fn upsert(
    conn: &Connection,
    bot_id: i64,
    username: &str,
    token: &str,
    label: Option<&str>,
    now_ms: i64,
) -> Result<bool> {
    let had_any: i64 = conn
        .query_row("SELECT COUNT(*) FROM telegram_bots", [], |r| r.get(0))
        .context("count bots")?;
    let is_default = had_any == 0;

    conn.execute(
        "INSERT INTO telegram_bots (bot_id, username, token, label, added_at, is_default) \
         VALUES (?1, ?2, ?3, ?4, ?5, ?6) \
         ON CONFLICT(bot_id) DO UPDATE SET \
           username = excluded.username, \
           token    = excluded.token, \
           label    = COALESCE(excluded.label, telegram_bots.label)",
        params![bot_id, username, token, label, now_ms, is_default as i64],
    )
    .context("store bot")?;
    Ok(is_default)
}

/// All registered bots, default first.
pub fn list(conn: &Connection) -> Result<Vec<BotRow>> {
    let mut stmt = conn
        .prepare(
            "SELECT bot_id, username, label, added_at, is_default \
             FROM telegram_bots ORDER BY is_default DESC, added_at ASC",
        )
        .context("prepare bot list")?;
    let rows = stmt
        .query_map([], |row| {
            Ok(BotRow {
                bot_id: row.get(0)?,
                username: row.get(1)?,
                label: row.get(2)?,
                added_at: row.get(3)?,
                is_default: row.get::<_, i64>(4)? != 0,
            })
        })
        .context("query bots")?;
    let mut out = Vec::new();
    for r in rows {
        out.push(r.context("read bot row")?);
    }
    Ok(out)
}

/// Token of the default bot, if one is registered.
pub fn default_token(conn: &Connection) -> Result<Option<String>> {
    let token: Option<String> = conn
        .query_row(
            "SELECT token FROM telegram_bots ORDER BY is_default DESC, added_at ASC LIMIT 1",
            [],
            |r| r.get(0),
        )
        .optional()
        .context("read default bot token")?;
    Ok(token)
}

/// Format a token for humans: enough to recognise which bot it is, not enough
/// to use. The bot id before `:` is public (it is in the bot's username page),
/// the secret never is.
pub fn mask(token: &str) -> String {
    match token.split_once(':') {
        Some((id, secret)) if secret.len() > 6 => {
            format!("{id}:{}…{}", &secret[..3], &secret[secret.len() - 3..])
        }
        _ => "***".to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn db() -> Connection {
        let c = Connection::open_in_memory().unwrap();
        apply_migration(&c).unwrap();
        c
    }

    #[test]
    fn valid_token_shape_yields_the_bot_id() {
        assert_eq!(
            validate_token_shape("1234567890:AAFakeTokenForTestsOnly_NotARealBot00").unwrap(),
            1234567890
        );
    }

    #[test]
    fn obviously_wrong_tokens_are_refused_before_any_network_call() {
        for bad in [
            "no-colon-at-all",
            "notanumber:AAFakeTokenForTestsOnly_NotARealBot00",
            "123:short",
            "123:has spaces in the secret part!!",
        ] {
            assert!(validate_token_shape(bad).is_err(), "{bad} should be refused");
        }
    }

    #[test]
    fn first_bot_becomes_default_and_second_does_not() {
        let c = db();
        assert!(upsert(&c, 1, "first_bot", "1:aaaaaaaaaaaaaaaaaaaaaa", None, 10).unwrap());
        assert!(!upsert(&c, 2, "second_bot", "2:bbbbbbbbbbbbbbbbbbbbbb", None, 20).unwrap());
        let bots = list(&c).unwrap();
        assert_eq!(bots.len(), 2);
        assert_eq!(bots[0].username, "first_bot");
        assert!(bots[0].is_default);
    }

    #[test]
    fn re_adding_the_same_bot_rotates_the_token_instead_of_duplicating() {
        let c = db();
        upsert(&c, 1, "bot", "1:oldoldoldoldoldoldold", None, 10).unwrap();
        upsert(&c, 1, "bot", "1:newnewnewnewnewnewnew", None, 20).unwrap();
        assert_eq!(list(&c).unwrap().len(), 1, "rotation must not duplicate");
        assert_eq!(
            default_token(&c).unwrap().unwrap(),
            "1:newnewnewnewnewnewnew"
        );
    }

    #[test]
    fn default_token_is_none_on_an_empty_registry() {
        assert!(default_token(&db()).unwrap().is_none());
    }

    #[test]
    fn mask_never_reveals_the_secret() {
        // A synthetic token on purpose: a real bot token in a test file is a
        // real bot token in the repository's history.
        let token = "1234567890:AAFakeTokenForTestsOnly_NotARealBot00";
        let masked = mask(token);
        assert!(masked.starts_with("1234567890:"));
        assert!(!masked.contains("AAFakeTokenForTests"));
        assert!(masked.len() < token.len());
        assert_eq!(mask("garbage"), "***");
    }
}
