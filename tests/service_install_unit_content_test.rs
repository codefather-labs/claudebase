//! Slice 2 TDD tests — content of generated service units / plists.
//!
//! These exercises hit pure-Rust generator functions (no FS, no
//! `Command::new`) so they run identically on every CI matrix node.
//! They catch the security-critical SEC-2-1 / SEC-2-6 directive set
//! the audit flagged as load-bearing for the platform service install.

use claudebase::daemon::service::{generate_launchd_plist, generate_systemd_unit};
use std::path::Path;

/// TC-2.6 — systemd unit must contain the hardening directives a USER manager
/// can actually apply.
///
/// `CapabilityBoundingSet=`, `AmbientCapabilities=` and `ProtectKernelModules=`
/// are deliberately NOT in this list, and their absence is asserted by
/// `test_systemd_user_unit_never_touches_capabilities` below. They were
/// required here until 2026-08-17, when a clean Ubuntu 24.04 install showed the
/// daemon crash-looping on `218/CAPABILITIES`: a user manager is unprivileged
/// and cannot drop capabilities, and `ProtectKernelModules` is capability
/// manipulation under another name (it drops CAP_SYS_MODULE). systemd 259
/// tolerates them, so this test passed on the maintainer's machine while
/// enforcing a directive set that made the service unstartable on the current
/// LTS — the suite was pinning the defect in place.
#[test]
fn test_systemd_unit_contains_all_hardening_directives() {
    let unit = generate_systemd_unit(Path::new("/usr/local/bin/claudebase"));
    let required = [
        "ProtectSystem=strict",
        "NoNewPrivileges=true",
        "PrivateTmp=true",
        "ProtectHome=read-only",
        "ReadWritePaths=%h/.claude %h/.config/claudebase",
        "ProtectKernelTunables=true",
        "ProtectControlGroups=true",
        "RestrictNamespaces=true",
        "RestrictRealtime=true",
        "LockPersonality=true",
        "MemoryDenyWriteExecute=true",
        "SystemCallArchitectures=native",
    ];
    for directive in required {
        assert!(
            unit.contains(directive),
            "systemd unit is missing required directive `{directive}`\nfull unit:\n{unit}"
        );
    }
}

/// The other half of TC-2.6: what the unit must NOT contain.
///
/// Bisected on the failing machine — removing `ProtectKernelModules` alone was
/// what turned `activating` into `active`.
#[test]
fn test_systemd_user_unit_never_touches_capabilities() {
    let unit = generate_systemd_unit(Path::new("/usr/local/bin/claudebase"));
    for forbidden in [
        "CapabilityBoundingSet",
        "AmbientCapabilities",
        "ProtectKernelModules",
    ] {
        assert!(
            !unit.contains(forbidden),
            "user unit contains `{forbidden}`; systemd 255 refuses to start it \
             (exit 218/CAPABILITIES)\nfull unit:\n{unit}"
        );
    }
}

/// SEC-2-1 — generated unit MUST NOT carry a `User=` directive (it is
/// a user unit, so the calling user owns it).
#[test]
fn test_systemd_unit_omits_user_root() {
    let unit = generate_systemd_unit(Path::new("/usr/local/bin/claudebase"));
    for line in unit.lines() {
        assert!(
            !line.trim_start().starts_with("User="),
            "found forbidden `User=` directive: `{line}`"
        );
    }
}

/// SEC-2-5 — the `ExecStart=` line must use an absolute path that
/// matches the binary argument verbatim (the production code passes
/// the canonical `current_exe()` result).
#[test]
fn test_systemd_unit_exec_start_is_absolute() {
    let bin = Path::new("/usr/local/bin/claudebase");
    let unit = generate_systemd_unit(bin);
    let line = unit
        .lines()
        .find(|l| l.starts_with("ExecStart="))
        .expect("ExecStart= line present");
    assert!(line.contains("/usr/local/bin/claudebase daemon serve"));
}

/// The `[Install]` section is what `systemctl --user enable` consumes
/// to wire the unit into `default.target`.
#[test]
fn test_systemd_unit_install_section_present() {
    let unit = generate_systemd_unit(Path::new("/usr/local/bin/claudebase"));
    assert!(unit.contains("[Install]"));
    assert!(unit.contains("WantedBy=default.target"));
}

/// SEC-2-6 — launchd plist is a LaunchAgent (not a LaunchDaemon).
/// LaunchAgents have no `UserName` key — they inherit the current user.
#[test]
fn test_launchd_plist_is_launch_agent_no_username() {
    let plist = generate_launchd_plist(
        Path::new("/usr/local/bin/claudebase"),
        Path::new("/tmp/out.log"),
        Path::new("/tmp/err.log"),
    );
    assert!(
        !plist.contains("<key>UserName</key>"),
        "plist must NOT contain UserName key (SEC-2-6)"
    );
    assert!(
        !plist.contains(">root<"),
        "plist must NOT mention root as a string value (SEC-2-6)"
    );
}

/// STRUCTURAL-2-1 — XML escaping is applied to every substituted value
/// so a binary path containing `<`, `>`, `&`, `"`, or `'` cannot break
/// the plist parser or smuggle a tag.
#[test]
fn test_launchd_plist_xml_escapes_binary_path() {
    let plist = generate_launchd_plist(
        Path::new("/weird/<tag>&amp\"'/claudebase"),
        Path::new("/tmp/out.log"),
        Path::new("/tmp/err.log"),
    );
    assert!(plist.contains("&lt;tag&gt;"));
    assert!(plist.contains("&amp;"));
    assert!(plist.contains("&quot;"));
    assert!(plist.contains("&apos;"));
    // The raw metacharacters MUST NOT appear inside the substituted path
    // (they still appear in surrounding `<string>` / `<key>` markup).
    assert!(!plist.contains("<tag>"));
}
