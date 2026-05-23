# telegram-plugin-rs

Rust port of the official Anthropic Telegram channel plugin for Claude Code,
originally at [`anthropics/claude-plugins-official`](https://github.com/anthropics/claude-plugins-official)
`external_plugins/telegram` (Apache-2.0, source commit `3449c10c`).

See [`docs/plans/telegram-rust-port.md`](../docs/plans/telegram-rust-port.md)
in the parent repo for the full plan, slice list, and acceptance criteria.

## Status

Slice R1 — minimal MCP stdio echo. Responds to `initialize` + `tools/list` (empty).
Tools, Telegram bot, whisper transcription come in later slices.

## Build

```sh
cargo build --release --bin telegram-plugin-rs
```

Produces `target/release/telegram-plugin-rs` (~5-10 MB stripped).

## Deploy (alongside TSX, toggle via env var)

```sh
# Copy the binary into the plugin cache as server-rs.
cp target/release/telegram-plugin-rs \
   ~/.claude/plugins/cache/claude-plugins-official/telegram/0.0.6/server-rs
chmod +x ~/.claude/plugins/cache/claude-plugins-official/telegram/0.0.6/server-rs
```

Then patch `.mcp.json` in the same directory to add the toggle (Slice R2
will document the exact patch).

To enable the Rust server for a session:

```sh
TELEGRAM_USE_RUST_SERVER=1 claude --channels plugin:telegram@claude-plugins-official
```

Default (env var unset) = TSX server. Removing or non-executable `server-rs`
also falls back to TSX even with the env var set.

## Local smoke-test (no Claude Code required)

```sh
# Slice R1: initialize handshake.
echo '{"jsonrpc":"2.0","id":1,"method":"initialize","params":{}}' \
  | ./target/release/telegram-plugin-rs
```

Expected: a single JSON line on stdout with `result.protocolVersion = "2025-11-25"`,
followed by the process holding stdin open (waiting for more frames). Press
Ctrl-D to close stdin → graceful exit.

## License

Apache License 2.0 — see [`LICENSE`](./LICENSE) and [`NOTICE`](./NOTICE).
