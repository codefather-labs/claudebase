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

/// A flag an agent is expected to reach for must be reachable from what the
/// agent reads.
///
/// `--new-callback-token` retires a callback token as part of a rename. It lives
/// in the CLI help, which an agent only sees if it already suspects the flag
/// exists — so the places that teach renaming and tokens have to name it too, or
/// it is a feature only its author knows about.
#[test]
fn the_documents_an_agent_reads_mention_the_rename_token_flag() {
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
    for doc in [
        "hooks/claudebase-channel-contract.sh",
        "hooks/claudebase-channel-contract.ps1",
        "prompts/skills/claudebase-daemon-change-nick/SKILL.md",
        "prompts/skills/claudebase-daemon-setup-auth-token/SKILL.md",
        "prompts/skills/claudebase-daemon-callback-info/SKILL.md",
        "README.md",
    ] {
        let text = std::fs::read_to_string(root.join(doc))
            .unwrap_or_else(|e| panic!("read {doc}: {e}"));
        assert!(
            text.contains("--new-callback-token"),
            "{doc} teaches renaming or tokens but never names --new-callback-token"
        );
    }
}

/// Both installers announce what they installed, and both under-reported it.
///
/// The closing summary listed the four `commands/` entries and CALLED them
/// skills, while the three real `skills/` entries went unmentioned — so an
/// operator reading the installer's own output would conclude claudebase ships
/// three slash-commands when it ships seven. This test pins the summary against
/// what the repository actually carries, so adding a skill without announcing it
/// fails here rather than being discovered by someone counting directories.
#[test]
fn the_installers_announce_every_skill_they_ship() {
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
    let shipped: Vec<String> = std::fs::read_dir(root.join("prompts/skills"))
        .expect("prompts/skills")
        .filter_map(|e| e.ok())
        .filter(|e| e.path().is_dir())
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .collect();
    assert!(!shipped.is_empty(), "no skills found to check against");

    for installer in ["install.sh", "install.ps1"] {
        let text = std::fs::read_to_string(root.join(installer))
            .unwrap_or_else(|e| panic!("read {installer}: {e}"));
        for skill in &shipped {
            assert!(
                text.contains(skill.as_str()),
                "{installer} never mentions the skill `{skill}` it installs — \
                 its summary tells the operator less than it ships"
            );
        }
    }
}

/// Both installers used to tell operators to pre-download the whisper model to
/// `whisper-cpp/models/`, a path the daemon has never read — it looks under
/// `.claude/tools/claudebase/models/whisper/`. Anyone who followed the advice
/// put 1.5 GB somewhere claudebase ignores, then watched voice notes fail with
/// "MISSING model file" pointing at a different path entirely.
#[test]
fn the_installers_name_the_model_path_the_daemon_actually_reads() {
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
    for installer in ["install.sh", "install.ps1"] {
        let text = std::fs::read_to_string(root.join(installer))
            .unwrap_or_else(|e| panic!("read {installer}: {e}"));
        for line in text.lines() {
            // The one surviving mention is the comment explaining that the path
            // was wrong; anything that still POINTS there is the bug.
            let points_at_it = line.contains("whisper-cpp")
                && (line.contains("pre-download") || line.contains("drops it"));
            assert!(
                !points_at_it,
                "{installer} still directs operators at a whisper-cpp path the daemon never reads:\n  {line}"
            );
        }
        assert!(
            text.contains("models/whisper/ggml-medium.bin")
                || text.contains("models\\whisper\\ggml-medium.bin"),
            "{installer} never names the model path the daemon reads"
        );
    }
}

/// The installer is a `.ps1` too, and it was the one nobody checked.
///
/// PowerShell 5.1 — still the default on Windows — reads a BOM-less script in
/// the system ANSI codepage, not UTF-8. Three em-dashes in comments and one
/// string were therefore mis-decoded, and the parser died with
/// `Missing closing '}'` at a function that was perfectly balanced. The Windows
/// installer did not run AT ALL, and nothing caught it: this test existed but
/// covered only `hooks/`.
///
/// Verified on a real Windows 11 box on 2026-08-18: the same bytes parse clean
/// as UTF-8 and fail as ANSI.
#[test]
fn the_windows_installer_is_ascii_only() {
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("install.ps1");
    let bytes = std::fs::read(&path).expect("read install.ps1");
    let offenders: Vec<(usize, u8)> = bytes
        .iter()
        .enumerate()
        .filter(|(_, b)| **b > 127)
        .map(|(i, b)| (i, *b))
        .take(5)
        .collect();
    assert!(
        offenders.is_empty(),
        "install.ps1 must be pure ASCII — PowerShell 5.1 reads a BOM-less script as ANSI and \
         mis-decodes anything else, which stops the installer from parsing at all. \
         First offending bytes (offset, value): {offenders:?}"
    );
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
fn channel_contract_hook_teaches_every_prefix_and_every_reply_command() {
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
                // A session that meets an unexplained `[callback]:` line has no
                // way to know it is external data rather than the operator
                // typing. Every prefix the transport can produce has to be here,
                // so this list grows whenever a new source is added.
                "[callback]: <text>",
                "[callback:<label>]: <text>",
                // A transcript is not typed text and must not read as if it
                // were: whisper mishears names, numbers and flags, which is
                // exactly the content an agent would otherwise act on verbatim.
                "[telegram_voice_message]: <text>",
                "claudebase telegram send",
                "claudebase agent send",
                "claudebase agent list",
                // The safety half is as load-bearing as the how-to half: with
                // --dangerously-skip-permissions default-on, this text is the
                // only thing marking channel content as data (plan risk R-6).
                "untrusted data",
                "claudebase telegram pair",
                // The two callback skills are how an operator gets a token and
                // debugs a callback that never arrived; unreferenced, they are
                // invisible.
                "claudebase-daemon-callback-info",
                "claudebase-daemon-setup-auth-token",
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
