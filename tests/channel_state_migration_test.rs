//! Slice 4 migration tests: legacy access.json (File A) → canonical access.json (File B).
//!
//! Uses tempdir + environment variable override (XDG_CONFIG_HOME) to avoid
//! touching the real ~/.config and ~/.claude directories.

use claudebase::daemon::channel_state::{self, Access, DmPolicy, MigrationOutcome};
use std::collections::BTreeMap;
use std::fs;

/// Helper: create a legacy File A payload as serde_json::Value.
/// Matches the old permissions module schema with numeric allowFrom.
fn create_legacy_file_a_payload(allow_from: Vec<i64>, dm_policy: &str) -> serde_json::Value {
    serde_json::json!({
        "dmPolicy": dm_policy,
        "allowFrom": allow_from,
        "groups": {},
        "pending": {}
    })
}

/// Helper: create a File B payload with string allowFrom.
fn create_canonical_file_b(allow_from: Vec<String>, dm_policy: DmPolicy) -> Access {
    Access {
        dm_policy,
        allow_from,
        groups: BTreeMap::new(),
        pending: BTreeMap::new(),
        mention_patterns: vec![],
    }
}

#[test]
fn migrates_numeric_allowfrom_into_file_b() {
    let tmpdir = tempfile::tempdir().unwrap();
    let legacy_path = tmpdir.path().join("access.json");
    let canonical_path = tmpdir.path().join("access_canonical.json");

    // Create File A with numeric IDs.
    let legacy_json = create_legacy_file_a_payload(vec![434566766, 777], "allowlist");
    fs::write(&legacy_path, serde_json::to_string(&legacy_json).unwrap()).unwrap();

    // Create File B with one existing ID.
    let canonical_b = create_canonical_file_b(vec!["111".to_string()], DmPolicy::Pairing);
    channel_state::save_access(&canonical_path, &canonical_b).unwrap();

    // Run migration.
    let outcome = channel_state::migrate_from_legacy_access(&legacy_path, &canonical_path).unwrap();
    assert_eq!(outcome, MigrationOutcome::Migrated);

    // Verify File B now contains all three IDs (deduplicated).
    let result_b = channel_state::load_access(&canonical_path).unwrap();
    assert_eq!(result_b.allow_from.len(), 3);
    assert!(result_b.allow_from.contains(&"111".to_string()));
    assert!(result_b.allow_from.contains(&"434566766".to_string()));
    assert!(result_b.allow_from.contains(&"777".to_string()));

    // Verify File A is renamed to .migrated.
    assert!(!legacy_path.exists());
    let migrated_marker = legacy_path.with_extension("json.migrated");
    assert!(migrated_marker.exists());

    // Verify the original File A was not corrupted (can read it).
    let migrated_content = fs::read_to_string(&migrated_marker).unwrap();
    let migrated_json: serde_json::Value = serde_json::from_str(&migrated_content).unwrap();
    assert_eq!(
        migrated_json.get("allowFrom").unwrap().as_array().unwrap().len(),
        2
    );
}

#[test]
fn idempotent_second_run_is_noop() {
    let tmpdir = tempfile::tempdir().unwrap();
    let legacy_path = tmpdir.path().join("access.json");
    let canonical_path = tmpdir.path().join("access_canonical.json");

    // Create File A.
    let legacy_json = create_legacy_file_a_payload(vec![434566766], "allowlist");
    fs::write(&legacy_path, serde_json::to_string(&legacy_json).unwrap()).unwrap();

    // Create File B.
    let canonical_b = create_canonical_file_b(vec!["111".to_string()], DmPolicy::Pairing);
    channel_state::save_access(&canonical_path, &canonical_b).unwrap();

    // First migration run.
    let outcome1 = channel_state::migrate_from_legacy_access(&legacy_path, &canonical_path).unwrap();
    assert_eq!(outcome1, MigrationOutcome::Migrated);

    // Save File B state after first migration.
    let file_b_after_first = channel_state::load_access(&canonical_path).unwrap();
    let first_allow_from_count = file_b_after_first.allow_from.len();

    // Second migration run — File A is now .migrated, so this should be a no-op.
    let outcome2 = channel_state::migrate_from_legacy_access(&legacy_path, &canonical_path).unwrap();
    assert_eq!(outcome2, MigrationOutcome::NoOpAlreadyMigrated);

    // Verify File B is unchanged (same allow_from count, same IDs).
    let file_b_after_second = channel_state::load_access(&canonical_path).unwrap();
    assert_eq!(file_b_after_second.allow_from.len(), first_allow_from_count);
    assert_eq!(file_b_after_first.allow_from, file_b_after_second.allow_from);
}

#[test]
fn absent_file_a_is_noop() {
    let tmpdir = tempfile::tempdir().unwrap();
    let legacy_path = tmpdir.path().join("access.json");
    let canonical_path = tmpdir.path().join("access_canonical.json");

    // File A does not exist.
    assert!(!legacy_path.exists());

    // Create File B.
    let canonical_b = create_canonical_file_b(vec!["111".to_string()], DmPolicy::Pairing);
    channel_state::save_access(&canonical_path, &canonical_b).unwrap();

    // Migration should be a no-op.
    let outcome = channel_state::migrate_from_legacy_access(&legacy_path, &canonical_path).unwrap();
    assert_eq!(outcome, MigrationOutcome::NoOpAbsent);

    // Verify File B is unchanged.
    let result_b = channel_state::load_access(&canonical_path).unwrap();
    assert_eq!(result_b.allow_from, vec!["111".to_string()]);

    // Verify no .migrated file was created.
    let migrated_marker = legacy_path.with_extension("json.migrated");
    assert!(!migrated_marker.exists());
}

#[test]
fn malformed_file_a_does_not_crash_or_rename() {
    let tmpdir = tempfile::tempdir().unwrap();
    let legacy_path = tmpdir.path().join("access.json");
    let canonical_path = tmpdir.path().join("access_canonical.json");

    // Create malformed File A.
    fs::write(&legacy_path, "{not json").unwrap();

    // Create File B.
    let canonical_b = create_canonical_file_b(vec!["111".to_string()], DmPolicy::Pairing);
    channel_state::save_access(&canonical_path, &canonical_b).unwrap();

    // Migration should return SkippedMalformed.
    let outcome = channel_state::migrate_from_legacy_access(&legacy_path, &canonical_path).unwrap();
    assert_eq!(outcome, MigrationOutcome::SkippedMalformed);

    // Verify File A is NOT renamed (left for operator inspection).
    assert!(legacy_path.exists());
    let migrated_marker = legacy_path.with_extension("json.migrated");
    assert!(!migrated_marker.exists());

    // Verify File B is unchanged.
    let result_b = channel_state::load_access(&canonical_path).unwrap();
    assert_eq!(result_b.allow_from, vec!["111".to_string()]);
}

#[test]
fn policy_kept_when_file_b_already_set() {
    let tmpdir = tempfile::tempdir().unwrap();
    let legacy_path = tmpdir.path().join("access.json");
    let canonical_path = tmpdir.path().join("access_canonical.json");

    // Create File A with dmPolicy=disabled (more restrictive).
    let legacy_json = create_legacy_file_a_payload(vec![434566766], "disabled");
    fs::write(&legacy_path, serde_json::to_string(&legacy_json).unwrap()).unwrap();

    // Create File B with dmPolicy=allowlist (less restrictive than disabled).
    let canonical_b = create_canonical_file_b(vec!["111".to_string()], DmPolicy::Allowlist);
    channel_state::save_access(&canonical_path, &canonical_b).unwrap();

    // Migration should preserve File B's dmPolicy (allowlist).
    let outcome = channel_state::migrate_from_legacy_access(&legacy_path, &canonical_path).unwrap();
    assert_eq!(outcome, MigrationOutcome::Migrated);

    // Verify File B's dmPolicy is still allowlist (not changed to disabled).
    let result_b = channel_state::load_access(&canonical_path).unwrap();
    assert_eq!(result_b.dm_policy, DmPolicy::Allowlist);
}

#[test]
fn file_a_disabled_does_not_lock_out_default_file_b() {
    // Security (Slice 4 MAJOR, security-auditor): dm_policy is NEVER migrated.
    // gate_dm checks dm_policy==Disabled BEFORE the allow_from membership check,
    // so Disabled drops ALL DMs — including allowlisted ids. A File A with
    // dmPolicy=disabled must therefore NOT flip a default-Pairing File B to
    // Disabled, or the migration would silently lock the operator out of the
    // very channel it just granted them. Grants (allow_from) still migrate.
    let tmpdir = tempfile::tempdir().unwrap();
    let legacy_path = tmpdir.path().join("access.json");
    let canonical_path = tmpdir.path().join("access_canonical.json");

    // File A: dmPolicy=disabled, but a grant we DO want carried over.
    let legacy_json = create_legacy_file_a_payload(vec![434566766], "disabled");
    fs::write(&legacy_path, serde_json::to_string(&legacy_json).unwrap()).unwrap();

    // File B: default Pairing.
    let canonical_b = create_canonical_file_b(vec!["111".to_string()], DmPolicy::Pairing);
    channel_state::save_access(&canonical_path, &canonical_b).unwrap();

    let outcome = channel_state::migrate_from_legacy_access(&legacy_path, &canonical_path).unwrap();
    assert_eq!(outcome, MigrationOutcome::Migrated);

    let result_b = channel_state::load_access(&canonical_path).unwrap();
    // Policy preserved as Pairing (NOT adopted as Disabled) — no lockout.
    assert_eq!(result_b.dm_policy, DmPolicy::Pairing);
    // The grant was still carried over (union of allow_from).
    assert!(result_b.allow_from.contains(&"434566766".to_string()));
    assert!(result_b.allow_from.contains(&"111".to_string()));
}

#[test]
fn string_and_numeric_ids_mixed_in_file_a() {
    let tmpdir = tempfile::tempdir().unwrap();
    let legacy_path = tmpdir.path().join("access.json");
    let canonical_path = tmpdir.path().join("access_canonical.json");

    // Create File A with mixed numeric and string IDs.
    let legacy_json = serde_json::json!({
        "dmPolicy": "pairing",
        "allowFrom": [434566766, "999999"],
        "groups": {},
        "pending": {}
    });
    fs::write(&legacy_path, serde_json::to_string(&legacy_json).unwrap()).unwrap();

    // Create empty File B.
    let canonical_b = create_canonical_file_b(vec![], DmPolicy::Pairing);
    channel_state::save_access(&canonical_path, &canonical_b).unwrap();

    // Migration should handle both forms.
    let outcome = channel_state::migrate_from_legacy_access(&legacy_path, &canonical_path).unwrap();
    assert_eq!(outcome, MigrationOutcome::Migrated);

    // Verify both IDs are now in File B as strings.
    let result_b = channel_state::load_access(&canonical_path).unwrap();
    assert_eq!(result_b.allow_from.len(), 2);
    assert!(result_b.allow_from.contains(&"434566766".to_string()));
    assert!(result_b.allow_from.contains(&"999999".to_string()));
}

#[test]
fn no_duplicate_ids_when_overlap() {
    let tmpdir = tempfile::tempdir().unwrap();
    let legacy_path = tmpdir.path().join("access.json");
    let canonical_path = tmpdir.path().join("access_canonical.json");

    // Create File A with overlapping IDs.
    let legacy_json = create_legacy_file_a_payload(vec![111, 222], "pairing");
    fs::write(&legacy_path, serde_json::to_string(&legacy_json).unwrap()).unwrap();

    // Create File B with one overlapping ID.
    let canonical_b = create_canonical_file_b(vec!["111".to_string()], DmPolicy::Pairing);
    channel_state::save_access(&canonical_path, &canonical_b).unwrap();

    // Migration should dedupe.
    let outcome = channel_state::migrate_from_legacy_access(&legacy_path, &canonical_path).unwrap();
    assert_eq!(outcome, MigrationOutcome::Migrated);

    // Verify no duplicates in File B.
    let result_b = channel_state::load_access(&canonical_path).unwrap();
    assert_eq!(result_b.allow_from.len(), 2);
    let count_111 = result_b.allow_from.iter().filter(|id| *id == "111").count();
    assert_eq!(count_111, 1); // Exactly one, not two.
}

#[test]
fn ignores_pending_entries_from_file_a() {
    let tmpdir = tempfile::tempdir().unwrap();
    let legacy_path = tmpdir.path().join("access.json");
    let canonical_path = tmpdir.path().join("access_canonical.json");

    // Create File A with pending entries (these should NOT be migrated).
    let legacy_json = serde_json::json!({
        "dmPolicy": "pairing",
        "allowFrom": [434566766],
        "groups": {},
        "pending": {
            "ABC123": {
                "telegram_user_id": 999999,
                "expires_at": 9999999999000i64
            }
        }
    });
    fs::write(&legacy_path, serde_json::to_string(&legacy_json).unwrap()).unwrap();

    // Create File B without any pending.
    let canonical_b = create_canonical_file_b(vec!["111".to_string()], DmPolicy::Pairing);
    channel_state::save_access(&canonical_path, &canonical_b).unwrap();

    // Migration should NOT import pending from File A.
    let outcome = channel_state::migrate_from_legacy_access(&legacy_path, &canonical_path).unwrap();
    assert_eq!(outcome, MigrationOutcome::Migrated);

    // Verify File B has NO pending entries from File A.
    let result_b = channel_state::load_access(&canonical_path).unwrap();
    assert_eq!(result_b.pending.len(), 0);
    // But File B's allow_from should have the migrated ID.
    assert!(result_b.allow_from.contains(&"434566766".to_string()));
}

#[test]
fn malformed_allowfrom_entries_are_skipped() {
    let tmpdir = tempfile::tempdir().unwrap();
    let legacy_path = tmpdir.path().join("access.json");
    let canonical_path = tmpdir.path().join("access_canonical.json");

    // Create File A with mixed good and bad allowFrom entries.
    let legacy_json = serde_json::json!({
        "dmPolicy": "pairing",
        "allowFrom": [111, "222", null, 333],
        "groups": {},
        "pending": {}
    });
    fs::write(&legacy_path, serde_json::to_string(&legacy_json).unwrap()).unwrap();

    // Create File B.
    let canonical_b = create_canonical_file_b(vec![], DmPolicy::Pairing);
    channel_state::save_access(&canonical_path, &canonical_b).unwrap();

    // Migration should skip the null, keep the valid ones.
    let outcome = channel_state::migrate_from_legacy_access(&legacy_path, &canonical_path).unwrap();
    assert_eq!(outcome, MigrationOutcome::Migrated);

    // Verify only valid entries are in File B.
    let result_b = channel_state::load_access(&canonical_path).unwrap();
    assert_eq!(result_b.allow_from.len(), 3);
    assert!(result_b.allow_from.contains(&"111".to_string()));
    assert!(result_b.allow_from.contains(&"222".to_string()));
    assert!(result_b.allow_from.contains(&"333".to_string()));
}
