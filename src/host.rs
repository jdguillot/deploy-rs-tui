//! Host status checks.
//!
//! - **Online**: TCP connect to port 22 with a short timeout (no ICMP, no
//!   raw sockets, no sudo).
//! - **Update**: optional / on-demand. Builds the system profile locally
//!   (`nix path-info`-style: we ask `nix eval --raw` for the out path of
//!   the activation derivation) and compares it to the remote machine's
//!   `/run/current-system` symlink read over SSH.
//!
//! Both checks are designed to be cheap to call from the TUI's async event
//! loop.

use std::process::Stdio;
use std::time::Duration;

use anyhow::{anyhow, Context, Result};
use tokio::io::AsyncReadExt;
use tokio::net::TcpStream;
use tokio::process::Command;
use tokio::time::timeout;

use crate::flake::Node;
use crate::ssh::SshOverride;

/// What we currently know about a host.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum Reachability {
    #[default]
    Unknown,
    Online,
    Offline,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum UpdateState {
    #[default]
    Unknown,
    UpToDate,
    NeedsUpdate,
    /// We tried to check but the comparison failed (host unreachable, eval
    /// error, etc.). The string is rendered in the details pane.
    Error,
}

#[derive(Debug, Clone, Default)]
pub struct HostStatus {
    pub reachability: Reachability,
    pub system_update: UpdateState,
    pub home_update: UpdateState,
    /// True while an update probe for the `system` profile is in flight.
    /// The previous value of `system_update` is kept around so the badge
    /// can show "previous result + spinner".
    pub checking_system: bool,
    pub checking_home: bool,
    pub last_error: Option<String>,
}

/// TCP-connect to `<hostname>:22` with a 2-second timeout.
///
/// `override_host` lets the caller redirect to a different IP/hostname
/// when the user has set an SSH override (e.g. the node isn't in
/// `~/.ssh/config` and they want to dial it directly).
pub async fn check_online(hostname: &str, override_host: Option<&str>) -> Reachability {
    let host = override_host.unwrap_or(hostname);
    let target = format!("{host}:22");
    match timeout(Duration::from_secs(2), TcpStream::connect(&target)).await {
        Ok(Ok(_)) => Reachability::Online,
        _ => Reachability::Offline,
    }
}

/// Compare the locally-evaluated profile out-path against the remote
/// `/run/current-system` (for `system`) or the user's `current-home`
/// (for `home`) symlink target.
///
/// `override_` is the per-host SSH override (may be empty/default), and
/// it's used both to redirect the SSH connection and to inject extra
/// `-i`/`-o` arguments.
///
/// Returns `Ok(true)` when the host already runs the latest build.
pub async fn check_profile_up_to_date(
    flake: &str,
    node: &Node,
    profile: &str,
    override_: &SshOverride,
) -> Result<bool> {
    let local = local_profile_path(flake, &node.name, profile)
        .await
        .with_context(|| format!("evaluating local path for {}.{profile}", node.name))?;

    let remote_cmd = match profile {
        "system" => "readlink -f /run/current-system".to_string(),
        "home" => {
            // home-manager symlink lives at ~/.local/state/nix/profiles/home-manager
            // (or the older ~/.nix-profile path). Try the modern location first.
            "readlink -f ~/.local/state/nix/profiles/home-manager 2>/dev/null \
             || readlink -f ~/.nix-profile"
                .to_string()
        }
        other => return Err(anyhow!("unknown profile `{other}`")),
    };

    let host = override_.effective_host(&node.hostname).to_string();
    // Per-profile user fallback: home profile uses the home-profile owner
    // (or node default), system uses the node default.
    let fallback_user = match profile {
        "home" => node
            .profiles
            .get("home")
            .and_then(|p| p.user.as_deref())
            .or(node.ssh_user.as_deref()),
        _ => node.ssh_user.as_deref(),
    };
    let user = override_.effective_user(fallback_user);
    let target = match user {
        Some(u) => format!("{u}@{host}"),
        None => host,
    };

    let remote = ssh_capture(&target, &remote_cmd, override_).await?;
    Ok(remote.trim() == local.trim())
}

/// Ask Nix for the out-path of the activation derivation. This still
/// triggers evaluation (and a build of the closure if it's missing from the
/// store), so it should run in the background.
async fn local_profile_path(flake: &str, node: &str, profile: &str) -> Result<String> {
    let attr = format!("{flake}#deploy.nodes.{node}.profiles.{profile}.path");
    let output = Command::new("nix")
        .args(["eval", "--raw", "--no-warn-dirty", &attr])
        .stdin(Stdio::null())
        .output()
        .await
        .context("spawning `nix eval --raw`")?;
    if !output.status.success() {
        return Err(anyhow!(
            "nix eval failed: {}",
            String::from_utf8_lossy(&output.stderr)
        ));
    }
    // deploy-rs path strings end with `/activate`; strip that to get the
    // actual store path the remote will compare against.
    let raw = String::from_utf8(output.stdout).context("`nix eval --raw` returned non-utf8")?;
    Ok(raw.trim_end_matches("/activate").to_string())
}

/// Run a non-interactive ssh command and return its stdout. Errors include
/// stderr to make TUI diagnostics legible.
async fn ssh_capture(target: &str, command: &str, override_: &SshOverride) -> Result<String> {
    let mut cmd = Command::new("ssh");
    cmd.args([
        "-o",
        "BatchMode=yes",
        "-o",
        "ConnectTimeout=3",
        "-o",
        "StrictHostKeyChecking=accept-new",
    ]);
    // Per-host overrides go *before* the target so they take precedence
    // over anything in the user's ssh_config.
    for arg in override_.ssh_args() {
        cmd.arg(arg);
    }
    cmd.arg(target);
    cmd.arg(command);
    let mut child = cmd
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .context("spawning ssh")?;

    let mut stdout = String::new();
    let mut stderr = String::new();
    if let Some(mut s) = child.stdout.take() {
        s.read_to_string(&mut stdout).await.ok();
    }
    if let Some(mut s) = child.stderr.take() {
        s.read_to_string(&mut stderr).await.ok();
    }
    let status = child.wait().await.context("waiting for ssh")?;
    if !status.success() {
        return Err(anyhow!("ssh `{command}` failed: {}", stderr.trim()));
    }
    Ok(stdout)
}
