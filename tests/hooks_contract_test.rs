//! Hook payload contracts.
//!
//! The hooks are how a session learns the channel contract — they inject text
//! into the model's context, and nothing else does. A hook that drifts from the
//! CLI it documents is worse than no hook: the agent confidently runs a command
//! that no longer exists. These tests pin the load-bearing strings.
//!
//! Formerly `cli_agent_routing_hook_test.rs`, which asserted the v0.8 contract
//! (MCP tool names, `agent list-alive`, the `<channel>` envelope). v0.10 moved
//! peer messaging to the `claudebase agent` CLI and inbound messages to
//! `[telegram_message]:` / `[agent-to-agent:<nick>]:` prefixes, so the markers
//! changed with it.
//!
//! ASCII-only on `.ps1` is not cosmetic: `docs/issues/003` records a UTF-8 BOM
//! written by Windows PowerShell silently breaking JSON parsing downstream.

use std::fs;
use std::path::PathBuf;

fn hook(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("hooks").join(name)
}

fn assert_ascii_only(name: &str) {
    let bytes = fs::read(hook(name)).unwrap_or_else(|e| panic!("read {name}: {e}"));
    let bad: Vec<(usize, u8)> = bytes
        .iter()
        .enumerate()
        .filter(|(_, b)| **b > 127)
        .take(5)
        .map(|(i, b)| (i, *b))
        .collect();
    assert!(
        bad.is_empty(),
        "{name} MUST be ASCII-only (docs/issues/003); first offsets: {bad:?}"
    );
}

fn assert_contains(name: &str, markers: &[&str]) {
    let body = fs::read_to_string(hook(name)).unwrap_or_else(|e| panic!("read {name}: {e}"));
    for m in markers {
        assert!(body.contains(m), "{name} is missing required marker {m:?}");
    }
}

fn assert_absent(name: &str, stale: &[&str]) {
    let body = fs::read_to_string(hook(name)).unwrap_or_else(|e| panic!("read {name}: {e}"));
    for m in stale {
        assert!(
            !body.contains(m),
            "{name} still references the removed v0.8 contract: {m:?}"
        );
    }
}

/// Surfaces that no longer exist. A hook naming any of these would send the
/// agent after a command or a wire shape that was deleted in v0.10.
const REMOVED_SURFACES: &[&str] = &[
    "agent_send(",
    "agent_describe(",
    "MCP tool agent_send",
    "claudebase agent list-alive",
    "claudebase agent chat",
    "<channel ",
    "chat_reply",
    "plugin serve",
];

#[test]
fn ps1_hooks_are_ascii_only() {
    for name in [
        "claudebase-agent-routing-reminder.ps1",
        "claudebase-channel-contract.ps1",
        "claudebase-feature-describe.ps1",
        "claudebase-read-insights-reminder.ps1",
        "claudebase-selfcheck-reminder.ps1",
    ] {
        assert_ascii_only(name);
    }
}

#[test]
fn routing_hook_documents_the_cli_peer_contract() {
    for name in [
        "claudebase-agent-routing-reminder.sh",
        "claudebase-agent-routing-reminder.ps1",
    ] {
        assert_contains(
            name,
            &[
                "[claudebase peer-session channel]",
                "claudebase agent list",
                "claudebase agent send",
                "--agent_nick",
                "[agent-to-agent:<nick>]",
                "claudebase run",
                "plan mode",
                "COORDINATE",
            ],
        );
        assert_absent(name, REMOVED_SURFACES);
    }
}

#[test]
fn channel_contract_hook_teaches_both_prefixes_and_both_reply_commands() {
    for name in [
        "claudebase-channel-contract.sh",
        "claudebase-channel-contract.ps1",
    ] {
        assert_contains(
            name,
            &[
                "[claudebase channel contract]",
                "[telegram_message]: <text>",
                "[agent-to-agent:<nick>]: <text>",
                "claudebase telegram send",
                "claudebase agent send",
                "claudebase agent list",
                // The safety half is as load-bearing as the how-to half: with
                // --dangerously-skip-permissions default-on, this text is the
                // only thing marking channel content as data (plan risk R-6).
                "untrusted data",
                "claudebase telegram pair",
            ],
        );
        assert_absent(name, REMOVED_SURFACES);
    }
}

#[test]
fn describe_hook_points_at_the_cli_not_the_removed_tool() {
    for name in [
        "claudebase-feature-describe.sh",
        "claudebase-feature-describe.ps1",
    ] {
        assert_contains(
            name,
            &[
                "claudebase agent describe",
                // Kept from the superseded cli_feature_describe_hook_test:
                // QA greps for this marker to prove the hook fired, and the
                // event name has to match what the hook is wired to.
                "feature-describe mandate",
                "PostToolUse",
            ],
        );
        assert_absent(name, REMOVED_SURFACES);
    }
}

/// Every `.sh` hook must emit valid JSON on stdout — Claude Code discards the
/// whole payload otherwise, silently, and the session simply never learns the
/// contract.
#[test]
#[cfg(unix)]
fn sh_hooks_emit_valid_hook_json() {
    for name in [
        "claudebase-channel-contract.sh",
        "claudebase-agent-routing-reminder.sh",
    ] {
        let out = std::process::Command::new("bash")
            .arg(hook(name))
            .output()
            .unwrap_or_else(|e| panic!("run {name}: {e}"));
        assert!(out.status.success(), "{name} exited non-zero");
        let parsed: serde_json::Value = serde_json::from_slice(&out.stdout)
            .unwrap_or_else(|e| panic!("{name} stdout is not JSON: {e}"));
        let ctx = parsed
            .pointer("/hookSpecificOutput/additionalContext")
            .and_then(|v| v.as_str())
            .unwrap_or_else(|| panic!("{name} has no additionalContext"));
        assert!(
            ctx.len() > 200,
            "{name} additionalContext looks truncated ({} chars)",
            ctx.len()
        );
    }
}
