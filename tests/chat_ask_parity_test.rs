//! telegram-multi-cli Slice 5 — `chat_ask` 3-surface parity (TC-TMC-18.x).
//!
//! `chat_ask` is only reachable end-to-end if it is present on ALL THREE
//! surfaces simultaneously:
//!
//! 1. `mcp.rs::TOOL_WHITELIST` — the plugin SEC-7 gate; absence here returns
//!    `-32601` BEFORE the daemon ever sees the call (TC-TMC-18.3 regression).
//! 2. `server.rs` tools/call dispatch — the daemon arm that runs the handler.
//! 3. `server.rs::build_tools_list_response` — the descriptor the agent reads
//!    to learn the tool exists.
//!
//! The whitelist surface is checked against the live constant
//! `claudebase::plugin::mcp::TOOL_WHITELIST` (TC-TMC-18.1). The two server.rs
//! surfaces are source-grepped (the dispatch arm + descriptor are not exposed
//! as introspectable values, and an end-to-end daemon spin-up is covered by
//! `plugin_tools_list_proxy.rs::test_tools_list_daemon_up_returns_chat_tools`
//! which this file complements with a fast static check).

use std::fs;
use std::path::PathBuf;

fn repo_root() -> PathBuf {
    // CARGO_MANIFEST_DIR is the crate root (the `claudebase/` dir).
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

#[test]
fn chat_ask_present_in_tool_whitelist() {
    // TC-TMC-18.1 — surface 1: the plugin SEC-7 whitelist.
    assert!(
        claudebase::plugin::mcp::TOOL_WHITELIST.contains(&"chat_ask"),
        "chat_ask must be in TOOL_WHITELIST; got {:?}",
        claudebase::plugin::mcp::TOOL_WHITELIST
    );
}

#[test]
fn chat_ask_present_in_server_dispatch_arm() {
    // Surface 2: the tools/call dispatch arm in server.rs.
    let src = fs::read_to_string(repo_root().join("src/daemon/server.rs"))
        .expect("read server.rs");
    assert!(
        src.contains("\"chat_ask\" => {"),
        "server.rs tools/call dispatch must have a \"chat_ask\" => {{ arm"
    );
    assert!(
        src.contains("handle_chat_ask("),
        "server.rs must call handle_chat_ask(...)"
    );
}

#[test]
fn chat_ask_present_in_tools_list_descriptor() {
    // Surface 3: build_tools_list_response descriptor.
    let src = fs::read_to_string(repo_root().join("src/daemon/server.rs"))
        .expect("read server.rs");
    assert!(
        src.contains("\"name\": \"chat_ask\""),
        "build_tools_list_response must include a chat_ask descriptor"
    );
    // The descriptor must advertise the async + DM-only contract so agents
    // know the tool returns a question_id (not the answer) and is DM-scoped.
    assert!(
        src.contains("question_id") && src.to_lowercase().contains("dm"),
        "chat_ask descriptor should document the question_id return + DM-only scope"
    );
}

#[test]
fn all_three_surfaces_agree() {
    // The conjunction is the actual TC-TMC-18 contract: all three present.
    let whitelisted = claudebase::plugin::mcp::TOOL_WHITELIST.contains(&"chat_ask");
    let src = fs::read_to_string(repo_root().join("src/daemon/server.rs"))
        .expect("read server.rs");
    let dispatched = src.contains("\"chat_ask\" => {");
    let described = src.contains("\"name\": \"chat_ask\"");
    assert!(
        whitelisted && dispatched && described,
        "3-surface parity: whitelist={whitelisted} dispatch={dispatched} descriptor={described}"
    );
}
