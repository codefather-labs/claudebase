//! cli-to-cli-routing Slice 8 — agent-to-agent notification frame
//! shape tests.
//!
//! NFR-C2C-8 / red team F-8: the daemon emits the same
//! `notifications/claude/channel` method as the TG inbound path, but
//! with two distinguishing meta keys: `source = "claudebase:agent"`
//! and `kind = "agent-to-agent"`. These tests pin the shape so a
//! future bridge / CC version that wants to render agent-to-agent
//! differently has a stable contract to detect.
//!
//! UC-C2C-15-EC1 fallthrough rule: any frame whose `meta.kind != "agent-to-agent"`
//! is TG inbound and falls through to the existing `<channel>` shape.
//! The TG builder is exercised by separate `chat_*` test files; here
//! we just verify the two paths are distinguishable on the wire.

use claudebase::daemon::chat::build_channel_notification_agent_to_agent;
use serde_json::Value;

#[test]
fn frame_method_is_notifications_claude_channel() {
    let f = build_channel_notification_agent_to_agent("hi", "mira", "vela", "m-1", false);
    assert_eq!(f["method"], "notifications/claude/channel");
    assert_eq!(f["jsonrpc"], "2.0");
}

#[test]
fn meta_target_agent_id_present_for_bridge_filter() {
    // Bridge filter (should_relay_channel_notification at bridge.rs:944-966)
    // reads /params/meta/target_agent_id to decide whether to relay.
    let f = build_channel_notification_agent_to_agent("hi", "mira", "vela-cc2", "m-1", false);
    assert_eq!(f["params"]["meta"]["target_agent_id"], "vela-cc2");
}

#[test]
fn meta_does_not_contain_extra_keys_that_cc_rejects() {
    // CC channel surface drops frames with unknown meta keys (v0.9-cut
    // Slice 8 AR-9 amendment + Wave 5 live QA repeat). The 6 keys
    // allowed are: chat_id / message_id / user / user_id / ts /
    // target_agent_id. Anything else MUST live in params.content.
    let f = build_channel_notification_agent_to_agent("hi", "mira", "vela", "m-1", false);
    let meta = f["params"]["meta"].as_object().expect("meta object");
    let allowed = [
        "chat_id",
        "message_id",
        "user",
        "user_id",
        "ts",
        "target_agent_id",
    ];
    for key in meta.keys() {
        assert!(
            allowed.contains(&key.as_str()),
            "meta has disallowed key {key}; would cause CC to drop the frame"
        );
    }
    // Conversely, the distinguishers MUST land in content as a parseable
    // JSON preamble so the receiving model can identify the agent-to-
    // agent context.
    let body = f["params"]["content"].as_str().expect("content string");
    assert!(body.contains("\"agent_to_agent\""));
    assert!(body.contains("\"from_agent_id\":\"mira\""));
    assert!(body.contains("\"target_agent_id\":\"vela\""));
}

#[test]
fn meta_includes_tg_shape_keys_so_cc_does_not_silently_drop() {
    // Wave 5 live QA root cause: CC's channel surface silently drops
    // frames whose meta diverges from the TG-frozen contract. Slice 8
    // hotfix adds the 5 required keys so the frame actually reaches
    // the receiving model's prompt context.
    let f = build_channel_notification_agent_to_agent("hi", "mira", "vela", "m-1", false);
    let meta = &f["params"]["meta"];
    assert!(meta["chat_id"].is_string(), "chat_id must be a string (TG contract)");
    assert!(meta["message_id"].is_string(), "message_id must be a string");
    assert!(meta["user"].is_string(), "user must be a string");
    assert!(meta["user_id"].is_string(), "user_id must be a string");
    assert!(meta["ts"].is_string(), "ts must be a string");
    assert_eq!(meta["user"], "mira");
    assert_eq!(meta["user_id"], "mira");
    assert_eq!(meta["chat_id"], "agent:vela");
}

#[test]
fn content_preamble_carries_drained_from_dnd_flag() {
    // Slice 5 DND drain path sets drained_from_dnd=true so the
    // receiving model can detect "this message was delayed". After
    // hotfix #2 the flag lives in the params.content preamble, NOT
    // in meta (where CC would have dropped the frame).
    let f_direct = build_channel_notification_agent_to_agent("hi", "mira", "vela", "m-1", false);
    let f_drained = build_channel_notification_agent_to_agent("hi", "mira", "vela", "m-2", true);
    let body_direct = f_direct["params"]["content"].as_str().unwrap();
    let body_drained = f_drained["params"]["content"].as_str().unwrap();
    assert!(body_direct.contains("\"drained_from_dnd\":false"));
    assert!(body_drained.contains("\"drained_from_dnd\":true"));
}

#[test]
fn params_content_carries_the_message_body_after_preamble() {
    // Body shape: <preamble-json-line>\n\n<original content>
    let content = "multi-line\ncontent with \"quotes\" + emoji 🎉";
    let f = build_channel_notification_agent_to_agent(content, "mira", "vela", "m-1", false);
    let body = f["params"]["content"].as_str().unwrap();
    assert!(body.ends_with(content), "verbatim user content must end the body");
    assert!(body.starts_with('{'), "preamble must be the first line as JSON");
}

#[test]
fn no_id_field_on_notification_frame() {
    // Notifications per JSON-RPC 2.0 do NOT carry an `id` field —
    // that's what distinguishes them from request frames in CC's
    // dispatch logic.
    let f = build_channel_notification_agent_to_agent("hi", "mira", "vela", "m-1", false);
    assert!(f.get("id").is_none(), "notification frame must NOT carry an id");
}
