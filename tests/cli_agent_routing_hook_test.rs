//! cli-to-cli-routing Slice 7b — SessionStart peer-agent reminder hook.
//!
//! Coverage:
//!   * NFR-C2C-6 ASCII-only constraint on `.ps1`.
//!   * Script body contains the key MCP tool names and the literal
//!     marker `[claudebase peer-agent channel]` so a session can
//!     grep-verify the hook fired.
//!   * sh variant carries the same content guarantees.

use std::fs;
use std::path::PathBuf;

fn repo_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

#[test]
fn nfr_c2c_6_ps1_routing_hook_is_ascii_only() {
    let p = repo_root()
        .join("hooks")
        .join("claudebase-agent-routing-reminder.ps1");
    let bytes = fs::read(&p).expect("read .ps1 hook");
    let bad: Vec<(usize, u8)> = bytes
        .iter()
        .enumerate()
        .filter(|(_, b)| **b > 127)
        .take(5)
        .map(|(i, b)| (i, *b))
        .collect();
    assert!(
        bad.is_empty(),
        "ps1 routing hook MUST be ASCII-only; first offsets: {:?}",
        bad
    );
}

#[test]
fn ps1_routing_hook_contains_required_markers_and_mcp_tool_names() {
    let p = repo_root()
        .join("hooks")
        .join("claudebase-agent-routing-reminder.ps1");
    let body = fs::read_to_string(&p).expect("read .ps1 hook");
    for marker in &[
        "[claudebase peer-agent channel]",
        "agent_describe",
        "agent_send",
        "agent_set_dnd",
        "claudebase agent list-alive",
        "claudebase agent inspect",
        "agent_to_agent",
        "PreToolUse",
        "plan mode",
        "COORDINATE",
    ] {
        assert!(
            body.contains(marker),
            "ps1 routing hook missing required marker {marker:?}"
        );
    }
}

#[test]
fn sh_routing_hook_contains_required_markers_and_mcp_tool_names() {
    let p = repo_root()
        .join("hooks")
        .join("claudebase-agent-routing-reminder.sh");
    let body = fs::read_to_string(&p).expect("read .sh hook");
    for marker in &[
        "[claudebase peer-agent channel]",
        "agent_describe",
        "agent_send",
        "agent_set_dnd",
        "claudebase agent list-alive",
        "claudebase agent inspect",
        "agent_to_agent",
        "PreToolUse",
        "plan mode",
        "COORDINATE",
    ] {
        assert!(
            body.contains(marker),
            "sh routing hook missing required marker {marker:?}"
        );
    }
}
