//! Integration tests for `flake::discover` using a PATH-shim `nix` binary.
//!
//! Each test writes a small shell script that pretends to be `nix`, prepends
//! its directory to `$PATH`, and then runs `flake::discover` normally. This
//! exercises the real parsing and error-handling code against controlled
//! output without requiring an actual Nix installation.

use std::fs;
use std::os::unix::fs::PermissionsExt;

use deploy_rs_tui::flake;
use serial_test::serial;
use tempfile::TempDir;

/// Create a temporary directory containing a `nix` shim script that echoes
/// the given `stdout` and exits with `exit_code`. Returns the `TempDir`
/// (must be kept alive for the duration of the test so the path stays
/// valid) and sets `$PATH` so `Command::new("nix")` finds the shim.
fn install_nix_shim(stdout: &str, stderr: &str, exit_code: i32) -> TempDir {
    let dir = TempDir::new().expect("create temp dir");
    let script = format!(
        "#!/bin/sh\nprintf '%s' '{}' >&2\nprintf '%s' '{}'\nexit {}\n",
        stderr.replace('\'', "'\\''"),
        stdout.replace('\'', "'\\''"),
        exit_code,
    );
    let nix_path = dir.path().join("nix");
    fs::write(&nix_path, script).expect("write nix shim");
    fs::set_permissions(&nix_path, fs::Permissions::from_mode(0o755)).expect("chmod nix shim");
    // Prepend our shim directory to PATH so it shadows any real `nix`.
    let path = format!(
        "{}:{}",
        dir.path().display(),
        std::env::var("PATH").unwrap_or_default()
    );
    std::env::set_var("PATH", path);
    dir
}

#[tokio::test]
#[serial]
async fn discover_parses_two_nodes() {
    let json = r#"{"alpha":{"hostname":"alpha.lan","sshUser":"root","profiles":{"system":{"user":null}}},"beta":{"hostname":"beta.lan","profiles":{"home":{"user":"jd"}}}}"#;
    let _dir = install_nix_shim(json, "", 0);

    let nodes = flake::discover(".").await.expect("discover should succeed");
    assert_eq!(nodes.len(), 2);

    let alpha = nodes.iter().find(|n| n.name == "alpha").unwrap();
    assert_eq!(alpha.hostname, "alpha.lan");
    assert_eq!(alpha.ssh_user.as_deref(), Some("root"));
    assert!(alpha.has_system());
    assert!(!alpha.has_home());

    let beta = nodes.iter().find(|n| n.name == "beta").unwrap();
    assert_eq!(beta.hostname, "beta.lan");
    assert_eq!(beta.ssh_user, None);
    assert!(!beta.has_system());
    assert!(beta.has_home());
}

#[tokio::test]
#[serial]
async fn discover_empty_nodes() {
    let _dir = install_nix_shim("{}", "", 0);
    let nodes = flake::discover(".").await.expect("discover should succeed");
    assert!(nodes.is_empty());
}

#[tokio::test]
#[serial]
async fn discover_nix_failure() {
    let _dir = install_nix_shim("", "error: flake not found", 1);
    let result = flake::discover("./nonexistent");
    let err = result.await.unwrap_err();
    let msg = format!("{err:#}");
    assert!(
        msg.contains("nix eval") || msg.contains("failed"),
        "error should mention nix eval failure: {msg}"
    );
}

#[tokio::test]
#[serial]
async fn discover_invalid_json() {
    let _dir = install_nix_shim("not valid json{{{", "", 0);
    let result = flake::discover(".");
    let err = result.await.unwrap_err();
    let msg = format!("{err:#}");
    assert!(
        msg.contains("parsing") || msg.contains("JSON") || msg.contains("json"),
        "error should mention JSON parsing: {msg}"
    );
}
