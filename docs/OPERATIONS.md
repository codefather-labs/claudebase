# Operating the claudebase daemon

The commands an operator runs on a machine to keep its daemon working: the service
itself, its two network listeners, the pool, the remote links, the Claude token, and
what to check when something does not arrive. Design behind each part:
[`docs/design/`](design/).

Everything here runs **on the machine whose daemon it is**, in that machine's terminal
(PowerShell on Windows), as the user the daemon runs as.

## 1. The service

```bash
claudebase daemon status            # running / stopped; --json for scripts
claudebase daemon start
claudebase daemon stop
claudebase daemon restart           # after any listener change below
claudebase daemon logs [--lines 200] [--follow]
claudebase daemon install|uninstall # the service unit; the installers do this
```

- Linux: a systemd user unit (`loginctl enable-linger "$USER"` to start at boot without
  a login). macOS: a LaunchAgent. Windows: a background `daemon serve` process, started
  again by the first `claudebase run` after a reboot.
- `uninstall --keep-data` keeps `chat.db`, secrets and config; the installers always
  pass it.
- Windows over SSH: processes started in an SSH session die with it (OpenSSH for Windows
  puts the session in a job object), so a daemon started by `install.ps1` or `daemon start`
  through SSH stops when you disconnect (seen 2026-10-26 on the pool machine). The next
  `claudebase run` started in a desktop window brings it back; run that first.
- Upgrading (`install.sh` / `install.ps1`) restarts the daemon. Open `claudebase run`
  sessions re-register by themselves; they need no restart.

## 2. The HTTP listener (callbacks and the pool API)

One listener serves both `POST /callback/<nick>` and `/pool/*`. Off by default.

```bash
# this machine only (default bind 127.0.0.1:8585):
claudebase daemon callback enable

# reachable from the LAN — the pool machine, so a gateway elsewhere can call /pool/*:
claudebase daemon callback enable --bind 0.0.0.0:8585 --i-know-this-is-remote

claudebase daemon restart           # REQUIRED: the listener starts with the daemon
claudebase daemon callback status   # bind + session tokens
claudebase daemon callback disable  # then restart
```

- A non-loopback `--bind` is refused without `--i-know-this-is-remote`.
- Plain HTTP: tokens travel in cleartext. Acceptable inside a closed network; across
  untrusted networks keep loopback and tunnel (`ssh -N -L 8585:127.0.0.1:8585 …`).
- Windows: allow the port for the LAN once, PowerShell as Administrator:
  `New-NetFirewallRule -DisplayName "claudebase http" -Direction Inbound -Protocol TCP -LocalPort 8585 -Action Allow -Profile Private`
- Session callback tokens: `claudebase daemon callback token <nick> [--reveal]`,
  `claudebase daemon callback rotate <nick>`.

## 3. The pool (gateway → sessions)

[`design/pool-dispatch.md`](design/pool-dispatch.md). On the pool machine, after §2 with a
LAN bind:

```bash
claudebase pool token issue gateway       # X-Api-Token for /pool/*, shown once
claudebase run --pool funnel              # one per worker, N windows
claudebase pool workers funnel            # free / busy / suspect / offline, and their jobs
claudebase pool token list | revoke gateway
```

The gateway then calls `http://<pool-machine>:8585/pool/funnel/…` with that token.
Workers run like every `claudebase run` session (permissions skipped).

| Worker state | Meaning | Operator action |
|---|---|---|
| `free` | takes the next job | — |
| `busy` | holds jobs up to its capacity | — |
| `suspect` | a job expired on it (usually the subscription's rate limit) | back to `free` by itself after 8 min (`CLAUDEBASE_POOL_SUSPECT_COOLDOWN_MS` in the daemon's environment), or sooner on a late answer / `claudebase pool ready`. A worker that keeps cycling `busy` → `suspect` is hung: restart its session |
| `offline` | session gone | its jobs fail as `worker_lost` after 15 s; start a new `claudebase run --pool funnel` |

## 4. Remote daemon (a session here, its daemon elsewhere)

[`design/remote-daemon.md`](design/remote-daemon.md); skill `/claudebase-setup-remote-daemon`.

On the machine whose daemon is shared:

```bash
claudebase daemon remote enable [--bind 0.0.0.0:8586]   # TLS, self-signed cert generated
claudebase daemon restart
claudebase daemon remote issue <client> --host <address clients use>   # connection string, once
claudebase daemon remote status | revoke <client> | disable
```

On a machine joining it:

```bash
claudebase daemon remote add <name>       # paste the connection string (stdin)
claudebase daemon remote test <name>
claudebase run --daemon <name>
claudebase daemon remote list | remove <name> | default <name|local>
```

Firewall: port 8586/tcp as in §2.

## 5. The Claude token of a machine

[`design/auth-token.md`](design/auth-token.md); skill `/claudebase-setup-claude-token`.

```bash
claude setup-token                                    # any machine with a browser
CLAUDEBASE_DAEMON=<name> claudebase auth setup-token  # paste; applied on THAT machine
claudebase auth status | clear-token
```

## 6. Journal maintenance

```bash
claudebase journal projects          # every journal this daemon holds
claudebase journal gc [--dry-run]    # retention now (the daemon also runs it daily)
```

## 7. When something does not arrive

| Symptom | Check |
|---|---|
| `connection refused` on :8585 / :8586 | `daemon callback status` / `daemon remote status` — enabled? then `daemon restart`; firewall |
| `401` on `/pool/*` | the gateway's `X-Api-Token`; `claudebase pool token list` |
| `404 no_such_pool` | no `claudebase run --pool <pool>` session has registered |
| `503 no_free_worker` | `claudebase pool workers <pool>` — all busy or suspect |
| a session missing from `agent list` | is its `claudebase run` alive; `daemon logs` for `agent_register FAILED` |
| `journal add` refused | inside a `claudebase run` session? `agent whoami` |
| remote `test` fails | the table in `/claudebase-setup-remote-daemon` |

Logs: `claudebase daemon logs --follow`; supervisor logs per session in
`~/.claude/logs/claudebase-run-<pid>.log`.
