# Security Policy

## Supported versions

Only the latest minor release receives security fixes. Older minor versions are out of support — upgrade to the latest.

| Version | Supported |
|---|---|
| 0.6.x | ✅ |
| 0.5.x | ⚠️ critical fixes only until 2026-09-01 |
| < 0.5 | ❌ |

## Reporting a vulnerability

**Do NOT open a public GitHub issue for security-sensitive bugs.** Public disclosure before a patch lands risks active exploitation of users who haven't upgraded.

Instead, report via one of:

- **GitHub private advisory** (preferred) — https://github.com/codefather-labs/claudebase/security/advisories/new
- **Email** to the maintainer at the address listed in `Cargo.toml` author field

Please include:
- A description of the vulnerability + the affected components / versions
- Steps to reproduce, ideally with a minimal proof-of-concept
- Your assessment of the impact (information disclosure / privilege escalation / DoS / etc)
- Any suggested mitigation if you have one

You'll get an acknowledgement within 7 days, an initial triage within 14 days, and (assuming the report is valid + actionable) a fixed release within 30-90 days depending on severity.

## Scope

In scope:
- The `claudebase` Rust binary + everything in `src/`, `plugins/`, `bench/`, `tests/`
- The `install.sh` / `install.ps1` installers (path traversal, code injection via env, etc)
- The cross-platform release pipeline (`.github/workflows/release.yml`)
- The `plugins/telegram-rs/` Telegram channel plugin

Out of scope (file separately upstream):
- Vulnerabilities in dependencies — file with the dep author, then ping us so we bump
- Vulnerabilities in Claude Code itself or the official Anthropic Telegram plugin — file at [anthropics/claude-plugins-official](https://github.com/anthropics/claude-plugins-official) or via Anthropic's security channel
- Vulnerabilities in the user's local Telegram bot token (operator's BotFather token security is the operator's responsibility)

## What we consider a vulnerability

- Unauthorized read / write / modification of the SQLite knowledge / insights DB
- Path traversal allowing read / write outside the project root
- Code execution via crafted PDF / Markdown ingestion
- Bypass of the `assert_allowed_chat` security gate (Telegram outbound)
- Bypass of access.json allowlist / pairing flow
- TLS misconfiguration in the server foundation (see `docs/plans/claudebase-server-foundation.md`)
- Auth token leakage in logs / audit trail / error messages
- Privilege escalation via the OS service install path (launchd / systemd / Windows SCM)

## What we do NOT consider a vulnerability

- Behavior triggered only when the operator deliberately runs malicious input through the tool
- Performance issues / DoS via large input files
- Issues that require physical / root access to the machine
- Speculative attacks without working PoC
