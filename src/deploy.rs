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
        format!(
            "{}#{}{}",
            self.flake,
            self.node,
            self.profile.target_suffix()
        )
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
            if tx_out
                .send(LogLine::Stdout(strip_ansi(&line)))
                .await
                .is_err()
            {
                break;
            }
        }
    });

    let tx_err = tx.clone();
    let stderr_task = tokio::spawn(async move {
        let mut reader = BufReader::new(stderr).lines();
        while let Ok(Some(line)) = reader.next_line().await {
            if tx_err
                .send(LogLine::Stderr(strip_ansi(&line)))
                .await
                .is_err()
            {
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

/// Remove ANSI terminal control sequences from a captured line.
///
/// `NO_COLOR=1` in the spawned environment tames `deploy-rs` itself,
/// but the nested `nix` / `nix-daemon` / `ssh` children don't all
/// honour it — in particular, remote `nix build` output that arrives
/// through ssh carries SGR colour codes, OSC title updates, cursor
/// moves, and the occasional raw ESC that ratatui's `Paragraph`
/// widget will happily render as literal bytes. When those bytes mix
/// into a `Line`, ratatui's width accounting drifts and individual
/// characters get dropped from the visible text (the classic
/// `dotfiles` → `dotf les` corruption).
///
/// We strip the common offenders here so every line that reaches the
/// TUI is plain utf-8 text:
///   - CSI sequences: `ESC [` … final byte in `0x40..=0x7e`
///   - OSC sequences: `ESC ]` … terminated by `BEL` or `ESC \\`
///   - Bare control bytes `\x00..=\x08`, `\x0b..=\x1f`, `\x7f`
///     except `\t` (tab, 0x09), which we keep verbatim
///
/// Line endings are already stripped by the line-buffered reader.
fn strip_ansi(input: &str) -> String {
    let bytes = input.as_bytes();
    let mut out = String::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        let b = bytes[i];
        if b == 0x1b {
            // ESC — try to classify the sequence that follows.
            if i + 1 >= bytes.len() {
                i += 1;
                continue;
            }
            match bytes[i + 1] {
                // CSI: ESC [ params final
                b'[' => {
                    let mut j = i + 2;
                    while j < bytes.len() {
                        let c = bytes[j];
                        if (0x40..=0x7e).contains(&c) {
                            j += 1;
                            break;
                        }
                        j += 1;
                    }
                    i = j;
                }
                // OSC: ESC ] … BEL | ESC \
                b']' => {
                    let mut j = i + 2;
                    while j < bytes.len() {
                        if bytes[j] == 0x07 {
                            j += 1;
                            break;
                        }
                        if bytes[j] == 0x1b && j + 1 < bytes.len() && bytes[j + 1] == b'\\' {
                            j += 2;
                            break;
                        }
                        j += 1;
                    }
                    i = j;
                }
                // Two-byte escape: ESC <char>
                _ => {
                    i += 2;
                }
            }
            continue;
        }
        // Keep tabs and printable bytes; drop other control bytes.
        if b == b'\t' || b >= 0x20 && b != 0x7f {
            // Push as many contiguous printable bytes as possible in
            // one shot to keep the utf-8 sequences intact.
            let start = i;
            while i < bytes.len() {
                let c = bytes[i];
                if c == 0x1b || (c < 0x20 && c != b'\t') || c == 0x7f {
                    break;
                }
                i += 1;
            }
            out.push_str(std::str::from_utf8(&bytes[start..i]).unwrap_or(""));
            continue;
        }
        i += 1;
    }
    out
}

#[cfg(test)]
mod tests {
    use super::strip_ansi;

    #[test]
    fn strips_csi_color_sequences() {
        let input = "\x1b[38;5;120mhello\x1b[0m world";
        assert_eq!(strip_ansi(input), "hello world");
    }

    #[test]
    fn preserves_utf8_and_tabs() {
        let input = "→ deploying\t/home/jdguillot/.dotfiles";
        assert_eq!(strip_ansi(input), "→ deploying\t/home/jdguillot/.dotfiles");
    }

    #[test]
    fn strips_osc_title_sequence() {
        let input = "\x1b]0;title\x07after";
        assert_eq!(strip_ansi(input), "after");
    }

    #[test]
    fn strips_bare_esc_and_control_bytes() {
        let input = "warn\x05ing \x1b ok\x7f";
        assert_eq!(strip_ansi(input), "warning  ok");
    }
}
