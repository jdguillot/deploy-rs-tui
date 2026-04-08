//! Wrapper around the `deploy` binary from serokell/deploy-rs.
//!
//! Each [`run`] call spawns `deploy` and forwards each stdout/stderr line
//! through an async channel so the TUI can render a live log. Cancellation
//! is achieved by dropping the join handle and killing the child via the
//! returned [`DeployHandle`].

use std::process::Stdio;

use anyhow::{Context, Result};
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::{Child, Command};
use tokio::sync::mpsc;
use tokio::task::JoinHandle;

use crate::ssh::SshOverride;

/// What kind of activation deploy-rs should perform on the remote.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    /// `switch-to-configuration switch` — apply immediately. Default.
    Switch,
    /// `--boot`: install the new generation as default but don't activate
    /// it until the next reboot.
    Boot,
    /// `--dry-activate`: build + diff only, no real activation.
    DryRun,
}

/// Which deploy-rs profiles to push for the selected node.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProfileSel {
    /// Both `system` and `home`. Equivalent to omitting the profile suffix.
    All,
    /// `system` only — NixOS host config.
    System,
    /// `home` only — home-manager.
    Home,
}

impl ProfileSel {
    fn target_suffix(self) -> &'static str {
        match self {
            ProfileSel::All => "",
            ProfileSel::System => ".system",
            ProfileSel::Home => ".home",
        }
    }
}

/// Boolean flags the user can toggle from the TUI. These all map directly
/// to deploy-rs CLI flags. We only emit a flag when the value differs
/// from deploy-rs's own default so the flake's `deploy.nodes.<name>`
/// settings stay authoritative for the un-overridden cases.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Toggles {
    /// `-s, --skip-checks` — skip the pre-deploy `nix flake check`.
    pub skip_checks: bool,
    /// `--magic-rollback <bool>`. deploy-rs default is `true`.
    pub magic_rollback: bool,
    /// `--auto-rollback <bool>`. deploy-rs default is `true`.
    pub auto_rollback: bool,
    /// `--remote-build` — perform the build on the target host.
    pub remote_build: bool,
    /// `--interactive-sudo true`. **Will hang the TUI** because the child
    /// reads a password from stdin and we run with `Stdio::null()`. Kept
    /// as a toggle for completeness; the help popup explains the catch.
    pub interactive_sudo: bool,
}

impl Default for Toggles {
    fn default() -> Self {
        // Match deploy-rs's own defaults so an "untouched" toggles state
        // is a no-op compared to running `deploy` directly.
        Self {
            skip_checks: false,
            magic_rollback: true,
            auto_rollback: true,
            remote_build: false,
            interactive_sudo: false,
        }
    }
}

#[derive(Debug, Clone)]
pub struct DeployRequest {
    pub flake: String,
    pub node: String,
    pub profile: ProfileSel,
    pub mode: Mode,
    pub toggles: Toggles,
    /// Per-host SSH override. Empty/default means "no override, use the
    /// flake / ssh_config as-is".
    pub ssh_override: SshOverride,
}

impl DeployRequest {
    fn target(&self) -> String {
        format!("{}#{}{}", self.flake, self.node, self.profile.target_suffix())
    }
}

/// A line of output emitted by the running `deploy` process. We tag the
/// stream so the TUI can colourise stderr differently if it wants to.
#[derive(Debug, Clone)]
pub enum LogLine {
    Stdout(String),
    Stderr(String),
    /// Final exit code; the channel closes after this.
    Exit(i32),
    /// Spawn or wait failure.
    Error(String),
}

pub struct DeployHandle {
    pub rx: mpsc::Receiver<LogLine>,
    /// Background task that owns the child. Drop or `.abort()` to cancel.
    pub task: JoinHandle<()>,
}

/// Spawn `deploy` for the given request and return a streaming handle.
pub fn run(req: DeployRequest) -> DeployHandle {
    let (tx, rx) = mpsc::channel(256);
    let task = tokio::spawn(async move {
        if let Err(e) = run_inner(req, tx.clone()).await {
            let _ = tx.send(LogLine::Error(format!("{e:#}"))).await;
        }
    });
    DeployHandle { rx, task }
}

async fn run_inner(req: DeployRequest, tx: mpsc::Sender<LogLine>) -> Result<()> {
    let mut cmd = Command::new("deploy");
    cmd.arg(req.target());

    // Mode → activation flag.
    match req.mode {
        Mode::Switch => {}
        Mode::Boot => {
            cmd.arg("--boot");
        }
        Mode::DryRun => {
            cmd.arg("--dry-activate");
        }
    }

    // User toggles. Only emit a flag when it differs from the deploy-rs
    // default; otherwise we'd silently shadow the flake's settings.
    let t = req.toggles;
    if t.skip_checks {
        cmd.arg("-s");
    }
    if !t.magic_rollback {
        cmd.args(["--magic-rollback", "false"]);
    }
    if !t.auto_rollback {
        cmd.args(["--auto-rollback", "false"]);
    }
    if t.remote_build {
        cmd.arg("--remote-build");
    }
    if t.interactive_sudo {
        cmd.args(["--interactive-sudo", "true"]);
    }

    // Per-host SSH override → --hostname / --ssh-user / --ssh-opts.
    if let Some(host) = &req.ssh_override.hostname {
        cmd.args(["--hostname", host]);
    }
    if let Some(user) = &req.ssh_override.user {
        cmd.args(["--ssh-user", user]);
    }
    if let Some(opts) = req.ssh_override.deploy_ssh_opts() {
        cmd.args(["--ssh-opts", &opts]);
    }

    cmd.stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        // kill_on_drop ensures the spawned `deploy` is reaped when the
        // owning task is aborted (cancel key) or the App exits.
        .kill_on_drop(true)
        // Make sure deploy-rs's coloured output stays human-readable when
        // we forward it line-by-line.
        .env("NO_COLOR", "1");

    let mut child: Child = cmd.spawn().context("spawning `deploy`")?;
    let stdout = child.stdout.take().expect("piped stdout");
    let stderr = child.stderr.take().expect("piped stderr");

    let tx_out = tx.clone();
    let stdout_task = tokio::spawn(async move {
        let mut reader = BufReader::new(stdout).lines();
        while let Ok(Some(line)) = reader.next_line().await {
            if tx_out.send(LogLine::Stdout(line)).await.is_err() {
                break;
            }
        }
    });

    let tx_err = tx.clone();
    let stderr_task = tokio::spawn(async move {
        let mut reader = BufReader::new(stderr).lines();
        while let Ok(Some(line)) = reader.next_line().await {
            if tx_err.send(LogLine::Stderr(line)).await.is_err() {
                break;
            }
        }
    });

    let status = child.wait().await.context("waiting for `deploy`")?;
    let _ = stdout_task.await;
    let _ = stderr_task.await;

    let code = status.code().unwrap_or(-1);
    let _ = tx.send(LogLine::Exit(code)).await;
    Ok(())
}
