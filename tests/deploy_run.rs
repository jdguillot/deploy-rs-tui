//! Integration tests for `deploy::run` using a PATH-shim `deploy` binary.
//!
//! Each test creates a tiny shell script that pretends to be the `deploy`
//! command. The script inspects `$@` to verify the flags we expect, writes
//! canned output to stdout/stderr, and exits with a controlled code. This
//! exercises the full `deploy::run` → `LogLine` channel pipeline without
//! touching a real Nix/deploy-rs installation.

use std::fs;
use std::os::unix::fs::PermissionsExt;

use deploy_rs_tui::deploy::{self, DeployRequest, LogLine, Mode, ProfileSel, Toggles};
use deploy_rs_tui::ssh::SshOverride;
use serial_test::serial;
use tempfile::TempDir;

/// Install a `deploy` shim that echoes to stdout/stderr and exits.
fn install_deploy_shim(script_body: &str) -> TempDir {
    let dir = TempDir::new().expect("create temp dir");
    let shim = dir.path().join("deploy");
    fs::write(&shim, format!("#!/bin/sh\n{script_body}\n")).expect("write deploy shim");
    fs::set_permissions(&shim, fs::Permissions::from_mode(0o755)).expect("chmod deploy shim");
    let path = format!(
        "{}:{}",
        dir.path().display(),
        std::env::var("PATH").unwrap_or_default()
    );
    std::env::set_var("PATH", path);
    dir
}

/// Drain all `LogLine`s from a deploy handle and return them.
async fn collect_lines(mut handle: deploy::DeployHandle) -> Vec<LogLine> {
    let mut lines = Vec::new();
    while let Some(line) = handle.rx.recv().await {
        lines.push(line);
    }
    lines
}

fn basic_request() -> DeployRequest {
    DeployRequest {
        flake: ".".into(),
        node: "myhost".into(),
        profile: ProfileSel::All,
        mode: Mode::Switch,
        toggles: Toggles::default(),
        ssh_override: SshOverride::default(),
    }
}

#[tokio::test]
#[serial]
async fn successful_deploy_streams_lines() {
    let _dir = install_deploy_shim(
        r#"echo "building closure"
echo "activating profile" >&2
echo "done"
exit 0"#,
    );

    let handle = deploy::run(basic_request());
    let lines = collect_lines(handle).await;

    // Should have at least stdout lines + an Exit(0).
    let stdout_lines: Vec<_> = lines
        .iter()
        .filter_map(|l| match l {
            LogLine::Stdout(s) => Some(s.as_str()),
            _ => None,
        })
        .collect();
    assert!(stdout_lines.contains(&"building closure"));
    assert!(stdout_lines.contains(&"done"));

    let stderr_lines: Vec<_> = lines
        .iter()
        .filter_map(|l| match l {
            LogLine::Stderr(s) => Some(s.as_str()),
            _ => None,
        })
        .collect();
    assert!(stderr_lines.contains(&"activating profile"));

    let exit = lines.iter().find_map(|l| match l {
        LogLine::Exit(code) => Some(*code),
        _ => None,
    });
    assert_eq!(exit, Some(0));
}

#[tokio::test]
#[serial]
async fn failed_deploy_reports_exit_code() {
    let _dir = install_deploy_shim(
        r#"echo "error: activation failed" >&2
exit 42"#,
    );

    let handle = deploy::run(basic_request());
    let lines = collect_lines(handle).await;

    let exit = lines.iter().find_map(|l| match l {
        LogLine::Exit(code) => Some(*code),
        _ => None,
    });
    assert_eq!(exit, Some(42));
}

#[tokio::test]
#[serial]
async fn boot_mode_flag() {
    // The shim dumps all args to stdout so we can verify --boot was passed.
    let _dir = install_deploy_shim(
        r#"for arg in "$@"; do echo "ARG:$arg"; done
exit 0"#,
    );

    let mut req = basic_request();
    req.mode = Mode::Boot;
    let handle = deploy::run(req);
    let lines = collect_lines(handle).await;

    let all_stdout: String = lines
        .iter()
        .filter_map(|l| match l {
            LogLine::Stdout(s) => Some(s.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n");
    assert!(all_stdout.contains("ARG:--boot"), "expected --boot in: {all_stdout}");
}

#[tokio::test]
#[serial]
async fn dry_run_mode_flag() {
    let _dir = install_deploy_shim(
        r#"for arg in "$@"; do echo "ARG:$arg"; done
exit 0"#,
    );

    let mut req = basic_request();
    req.mode = Mode::DryRun;
    let handle = deploy::run(req);
    let lines = collect_lines(handle).await;

    let all_stdout: String = lines
        .iter()
        .filter_map(|l| match l {
            LogLine::Stdout(s) => Some(s.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n");
    assert!(
        all_stdout.contains("ARG:--dry-activate"),
        "expected --dry-activate in: {all_stdout}"
    );
}

#[tokio::test]
#[serial]
async fn toggle_flags_emitted_correctly() {
    let _dir = install_deploy_shim(
        r#"for arg in "$@"; do echo "ARG:$arg"; done
exit 0"#,
    );

    let mut req = basic_request();
    req.toggles.skip_checks = true;
    req.toggles.magic_rollback = false; // differs from default → emit
    req.toggles.auto_rollback = false; // differs from default → emit
    req.toggles.remote_build = true;

    let handle = deploy::run(req);
    let lines = collect_lines(handle).await;

    let all_stdout: String = lines
        .iter()
        .filter_map(|l| match l {
            LogLine::Stdout(s) => Some(s.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n");
    assert!(all_stdout.contains("ARG:-s"), "expected -s flag: {all_stdout}");
    assert!(
        all_stdout.contains("ARG:--magic-rollback"),
        "expected --magic-rollback: {all_stdout}"
    );
    assert!(
        all_stdout.contains("ARG:--auto-rollback"),
        "expected --auto-rollback: {all_stdout}"
    );
    assert!(
        all_stdout.contains("ARG:--remote-build"),
        "expected --remote-build: {all_stdout}"
    );
}

#[tokio::test]
#[serial]
async fn ssh_override_flags() {
    let _dir = install_deploy_shim(
        r#"for arg in "$@"; do echo "ARG:$arg"; done
exit 0"#,
    );

    let mut req = basic_request();
    req.ssh_override = SshOverride {
        hostname: Some("10.0.0.5".into()),
        user: Some("admin".into()),
        identity: Some("/home/me/.ssh/id_ed25519".into()),
        extra_opts: Some("Port=2222".into()),
    };

    let handle = deploy::run(req);
    let lines = collect_lines(handle).await;

    let all_stdout: String = lines
        .iter()
        .filter_map(|l| match l {
            LogLine::Stdout(s) => Some(s.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n");
    assert!(all_stdout.contains("ARG:--hostname"), "expected --hostname: {all_stdout}");
    assert!(all_stdout.contains("ARG:10.0.0.5"), "expected IP: {all_stdout}");
    assert!(all_stdout.contains("ARG:--ssh-user"), "expected --ssh-user: {all_stdout}");
    assert!(all_stdout.contains("ARG:admin"), "expected user: {all_stdout}");
    assert!(all_stdout.contains("ARG:--ssh-opts"), "expected --ssh-opts: {all_stdout}");
}

#[tokio::test]
#[serial]
async fn ansi_stripped_from_output() {
    let _dir = install_deploy_shim(
        r#"printf '\033[32mgreen text\033[0m\n'
exit 0"#,
    );

    let handle = deploy::run(basic_request());
    let lines = collect_lines(handle).await;

    let stdout: Vec<_> = lines
        .iter()
        .filter_map(|l| match l {
            LogLine::Stdout(s) => Some(s.clone()),
            _ => None,
        })
        .collect();
    // The ANSI should be stripped — no \x1b in the output.
    for line in &stdout {
        assert!(
            !line.contains('\x1b'),
            "ANSI escape leaked through: {line:?}"
        );
    }
    assert!(
        stdout.iter().any(|l| l.contains("green text")),
        "expected 'green text' in: {stdout:?}"
    );
}

#[tokio::test]
#[serial]
async fn profile_suffix_in_target() {
    let _dir = install_deploy_shim(
        r#"for arg in "$@"; do echo "ARG:$arg"; done
exit 0"#,
    );

    let mut req = basic_request();
    req.profile = ProfileSel::System;
    let handle = deploy::run(req);
    let lines = collect_lines(handle).await;

    let all_stdout: String = lines
        .iter()
        .filter_map(|l| match l {
            LogLine::Stdout(s) => Some(s.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n");
    assert!(
        all_stdout.contains("ARG:.#myhost.system"),
        "expected .#myhost.system in: {all_stdout}"
    );
}
