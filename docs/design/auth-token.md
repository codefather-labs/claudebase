# Claude auth token through the daemon

Status: implemented, 2026-10-26 (operator request).

## Goal

From the operator's own machine, joined to a remote daemon, hand that machine a Claude
subscription token once, so every Claude instance started there afterwards — pool workers
in particular — is signed in without anyone logging in on that machine.

```bash
claude setup-token                                   # on any machine with a browser: prints a long-lived token
claudebase daemon remote test lan                    # joined to the pool machine's daemon
CLAUDEBASE_DAEMON=lan claudebase auth setup-token    # paste the token (stdin); it goes to THAT daemon
claudebase auth status                               # installed, fingerprint …a1b2, where
```

`claudebase auth setuptoken` is accepted as a spelling of `setup-token`.

## Facts

- Claude Code reads a long-lived token from `CLAUDE_CODE_OAUTH_TOKEN` (the string is in the
  2.1.310 binary; `claude setup-token`: "Set up a long-lived authentication token (requires
  Claude subscription)").

## Design

- **The CLI sends, the daemon of that machine applies.** `auth setup-token` reads the token
  from stdin (or an argument, which lands in shell history) and calls `auth_set_token` on
  the daemon in use — local, or the remote profile (`--daemon` / `CLAUDEBASE_DAEMON`). Over a
  remote connection it travels inside TLS.
- **Stored with the daemon's secrets**, `~/.claude/claudebase/auth.toml`, mode 0600.
- **Applied two ways on that machine:**
  1. `claudebase run` there sets `CLAUDE_CODE_OAUTH_TOKEN` for the `claude` it starts,
     unless the environment already has one — effective for the next session at once, no
     shell restart. This is what pool workers rely on.
  2. The user's persistent environment, for a bare `claude` too:
     - Windows: `HKCU\Environment` (`setx`), picked up by newly started processes;
     - Linux: `~/.config/environment.d/claudebase-auth.conf` (systemd user sessions) and a
       marked block in `~/.profile` (login shells) — new logins;
     - macOS: the marked `~/.profile` / `~/.zprofile` block.
     Per user, not machine-wide: the daemon runs as the user and system-wide stores need
     admin rights.
- **Never readable back.** No tool returns the token; `auth status` shows only whether one
  is set, its last 4 characters, when, from which client, and where it was applied.
- `claudebase auth clear-token` removes it from all three places.
- Over a remote connection `auth_set_token`, `auth_status`, `auth_clear_token` are allowed:
  a remote token is already full trust in that daemon.

## Open

- Whether a freshly installed `claude` signed in only by the env var still shows its
  first-run screens (theme, trust prompt) — to check on the LAN test machine; if so,
  `claudebase run` there should pre-accept them.
