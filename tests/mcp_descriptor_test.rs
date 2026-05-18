//! Slice 2 TDD tests — `.mcp.json` descriptor serialisation (SEC-2-7).
//!
//! The descriptor is the only artefact written into Claude Code's
//! plugin directory; arg-vector smuggling here would let a malicious
//! actor execute arbitrary commands on the operator's behalf. The
//! tests confirm that:
//!
//! 1. Serialisation is via serde_json (round-trip stability).
//! 2. The `args` vector is hard-coded to `["plugin","serve"]`
//!    regardless of binary-path input.
//! 3. The descriptor's idempotency hook short-circuits a re-write when
//!    content matches.

use claudebase::daemon::service::{check_idempotency, IdempotencyDecision, McpDescriptor};
use std::fs;
use std::path::Path;
use tempfile::tempdir;

#[test]
fn test_mcp_json_uses_serde_serialization() {
    let d = McpDescriptor::new(Path::new("/usr/local/bin/claudebase"));
    let s = d.to_json().expect("serialise descriptor");
    // The pretty-printer puts each key on its own line and quotes
    // strings — confirm the JSON parses back to the same struct.
    let back: McpDescriptor = serde_json::from_str(&s).expect("parse round-trip");
    assert_eq!(d, back);
}

#[test]
fn test_mcp_json_args_are_hardcoded_plugin_serve() {
    let d = McpDescriptor::new(Path::new("/anywhere/claudebase"));
    assert_eq!(d.args, vec!["plugin".to_string(), "serve".to_string()]);
    // Even with a weird path that includes a space, the args are
    // unchanged — they are not derived from the binary path.
    let d2 = McpDescriptor::new(Path::new("/path with spaces/claudebase"));
    assert_eq!(d2.args, vec!["plugin".to_string(), "serve".to_string()]);
}

#[test]
fn test_mcp_json_idempotent_when_content_equal() {
    let dir = tempdir().unwrap();
    let path = dir.path().join(".mcp.json");
    let d = McpDescriptor::new(Path::new("/usr/local/bin/claudebase"));
    let body = d.to_json().unwrap();
    fs::write(&path, &body).unwrap();
    assert_eq!(
        check_idempotency(&path, body.as_bytes()),
        IdempotencyDecision::AlreadyInstalled
    );
}

#[test]
fn test_mcp_json_command_field_renders_binary_path() {
    let d = McpDescriptor::new(Path::new("/opt/claudebase"));
    let s = d.to_json().unwrap();
    let parsed: serde_json::Value = serde_json::from_str(&s).unwrap();
    assert_eq!(parsed["command"], "/opt/claudebase");
    assert_eq!(parsed["args"], serde_json::json!(["plugin", "serve"]));
}
