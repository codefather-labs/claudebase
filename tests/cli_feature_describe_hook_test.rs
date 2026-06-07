//! cli-to-cli-routing Slice 7 — PostToolUse:ExitPlanMode hook scripts.
//!
//! Coverage:
//!   * NFR-C2C-6 ASCII-only constraint on `.ps1` hook file.
//!   * Script-output smoke: the hook produces a valid JSON envelope
//!     with the literal substring "feature-describe mandate" in
//!     additionalContext (Slice 9 QA verifies live).
//!   * Pre-flight evidence file at
//!     docs/qa/evidence/cli-to-cli-routing/slice-7-preflight.txt
//!     is present (records hook-event verification status).

use std::fs;
use std::path::PathBuf;

fn repo_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

#[test]
fn nfr_c2c_6_ps1_hook_is_ascii_only() {
    let p = repo_root()
        .join("hooks")
        .join("claudebase-feature-describe.ps1");
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
        "ps1 hook MUST be ASCII-only (NFR-C2C-6); first non-ASCII offsets/bytes: {:?}",
        bad
    );
}

#[test]
fn ps1_hook_contains_feature_describe_mandate_marker() {
    let p = repo_root()
        .join("hooks")
        .join("claudebase-feature-describe.ps1");
    let body = fs::read_to_string(&p).expect("read .ps1 hook");
    assert!(
        body.contains("feature-describe mandate"),
        "ps1 hook body must inject the 'feature-describe mandate' marker so Slice 9 QA can grep for it"
    );
    assert!(
        body.contains("agent_describe"),
        "ps1 hook body must mention the MCP tool name 'agent_describe'"
    );
    assert!(
        body.contains("PostToolUse"),
        "ps1 hook body must declare hookEventName as PostToolUse"
    );
}

#[test]
fn sh_hook_contains_feature_describe_mandate_marker() {
    let p = repo_root()
        .join("hooks")
        .join("claudebase-feature-describe.sh");
    let body = fs::read_to_string(&p).expect("read .sh hook");
    assert!(body.contains("feature-describe mandate"));
    assert!(body.contains("agent_describe"));
    assert!(body.contains("PostToolUse"));
}

#[test]
fn pre_flight_evidence_file_exists_with_decision_record() {
    let p = repo_root()
        .join("docs")
        .join("qa")
        .join("evidence")
        .join("cli-to-cli-routing")
        .join("slice-7-preflight.txt");
    assert!(p.exists(), "pre-flight evidence file required: {:?}", p);
    let body = fs::read_to_string(&p).expect("read pre-flight evidence");
    assert!(body.contains("PostToolUse"));
    assert!(
        body.contains("Fallback"),
        "evidence file must document the fallback chain"
    );
    assert!(
        body.contains("Slice 9 QA"),
        "evidence file must point to the live-verification path"
    );
}
