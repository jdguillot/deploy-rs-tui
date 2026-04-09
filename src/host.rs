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

use std::collections::{BTreeMap, BTreeSet};
use std::process::Stdio;
use std::time::{Duration, SystemTime};

use anyhow::{anyhow, Context, Result};
use tokio::io::AsyncReadExt;
use tokio::net::TcpStream;
use tokio::process::Command;
use tokio::sync::mpsc;
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
    /// True while the reachability TCP probe is in flight. Lets the UI
    /// show the same spinner as the update-check probes.
    pub checking_reachability: bool,
    /// Wall-clock time of the most recent successful reachability probe.
    /// Rendered in the details pane as an "up X ago" hint so the user
    /// can tell at a glance how fresh the online indicator actually is.
    pub last_online: Option<SystemTime>,
    pub last_error: Option<String>,
    /// Per-profile extra information harvested during update checks
    /// (paths, activation time, closure size, package diff). Populated
    /// lazily — `u` fills in the cheap tier, `U`/`p` fill in the rest.
    pub system_extra: ProfileExtra,
    pub home_extra: ProfileExtra,
}

/// Rich result of an update probe — always includes the store paths
/// and (when we can stat it) the remote activation time. These fields
/// come "for free" because we already ran the readlink over SSH, so
/// we surface them in the details pane whenever `u` is pressed.
#[derive(Debug, Clone)]
pub struct ProfileCheck {
    pub up_to_date: bool,
    pub local_path: String,
    pub remote_path: String,
    pub activation_time: Option<SystemTime>,
}

/// Extra details about a profile that the user can populate via the
/// update-check keys. `u` fills in the cheap tier (paths + activation
/// time); `U` fills in closure sizes; `p` fills in the full package
/// diff. Every field is optional so the UI can render whatever is
/// currently known without branching on tiers.
#[derive(Debug, Clone, Default)]
pub struct ProfileExtra {
    pub local_path: Option<String>,
    pub remote_path: Option<String>,
    pub activation_time: Option<SystemTime>,
    /// Closure size in bytes as reported by `nix path-info --closure-size`.
    pub local_size: Option<u64>,
    pub remote_size: Option<u64>,
    pub checking_size: bool,
    /// Raw output of `nix store diff-closures remote local` — rendered
    /// inline in the details pane so the user can see the full package
    /// delta. May be empty when the closures are identical.
    pub pkg_diff: Option<String>,
    pub checking_pkg: bool,
}

/// TCP-connect to the host's effective SSH endpoint.
///
/// Resolution order:
///   1. If the per-host override sets an explicit `hostname`, trust it
///      (the user was deliberate). Port still comes from `ssh -G`.
///   2. Otherwise run `ssh -G <hostname> [override args…]` to resolve
///      whatever `~/.ssh/config` says — this is what `ssh` would actually
///      use, so the "online" badge matches the user's real SSH setup.
///
/// Falls back to `<hostname>:22` if `ssh -G` fails for any reason.
pub async fn check_online(hostname: &str, override_: &SshOverride) -> Reachability {
    let (host, port) = resolve_ssh_endpoint(hostname, override_)
        .await
        .unwrap_or_else(|| {
            (
                override_.effective_host(hostname).to_string(),
                22,
            )
        });
    let target = format!("{host}:{port}");
    match timeout(Duration::from_secs(2), TcpStream::connect(&target)).await {
        Ok(Ok(_)) => Reachability::Online,
        _ => Reachability::Offline,
    }
}

/// Ask `ssh -G` to resolve a host the way `ssh` would: alias lookups,
/// `HostName` substitution, `Port`, all of it. Returns `None` if ssh
/// isn't on PATH, the config can't be parsed, or the relevant lines are
/// missing from the output.
async fn resolve_ssh_endpoint(
    hostname: &str,
    override_: &SshOverride,
) -> Option<(String, u16)> {
    let effective = override_.effective_host(hostname).to_string();
    let mut cmd = Command::new("ssh");
    cmd.arg("-G");
    // Per-host override args feed the same resolution as a real
    // connection would, so `-o Port=2222` in override opts lands in
    // the output without us having to parse `extra_opts`.
    for arg in override_.ssh_args() {
        cmd.arg(arg);
    }
    cmd.arg(&effective);
    let output = timeout(Duration::from_secs(2), cmd.output()).await.ok()?.ok()?;
    if !output.status.success() {
        return None;
    }
    let text = String::from_utf8_lossy(&output.stdout);
    let mut resolved_host = effective.clone();
    let mut resolved_port: u16 = 22;
    for line in text.lines() {
        let mut parts = line.splitn(2, ' ');
        let key = parts.next().unwrap_or("");
        let val = parts.next().unwrap_or("").trim();
        match key {
            "hostname" => {
                if !val.is_empty() {
                    resolved_host = val.to_string();
                }
            }
            "port" => {
                if let Ok(p) = val.parse::<u16>() {
                    resolved_port = p;
                }
            }
            _ => {}
        }
    }
    // If the override explicitly set a hostname, trust it over whatever
    // ssh_config resolved (the user's override is the most-recent
    // intent and might be a one-shot IP). The resolved port still
    // applies.
    if let Some(explicit) = override_.hostname.as_deref() {
        resolved_host = explicit.to_string();
    }
    Some((resolved_host, resolved_port))
}

/// Compare the locally-evaluated profile out-path against the remote
/// `/run/current-system` (for `system`) or the user's `current-home`
/// (for `home`) symlink target.
///
/// `override_` is the per-host SSH override (may be empty/default), and
/// it's used both to redirect the SSH connection and to inject extra
/// `-i`/`-o` arguments.
///
/// Returns the full [`ProfileCheck`] so callers can surface the resolved
/// paths and activation time in the UI — they're essentially free
/// byproducts of the readlink we'd be running anyway.
pub async fn check_profile_up_to_date(
    flake: &str,
    node: &Node,
    profile: &str,
    override_: &SshOverride,
) -> Result<ProfileCheck> {
    let local = local_profile_path(flake, &node.name, profile)
        .await
        .with_context(|| format!("evaluating local path for {}.{profile}", node.name))?;

    // Combined readlink + stat so we only pay one SSH round-trip. We
    // stat the *symlink itself*, not the resolved store path, because
    // Nix freezes store-path mtimes to 1 (epoch+1s) for reproducible
    // builds — staring the resolved path would always return "56
    // years ago". The symlink's mtime is the activation time.
    let remote_cmd = match profile {
        "system" => {
            "readlink -f /run/current-system && stat -c %Y /run/current-system".to_string()
        }
        "home" => {
            // Try the modern home-manager symlink first; fall back to
            // the legacy ~/.nix-profile. We pick whichever exists,
            // emit its resolved path, then stat the symlink we picked
            // so the activation time matches the path we reported.
            r#"if [ -L ~/.local/state/nix/profiles/home-manager ]; then link=~/.local/state/nix/profiles/home-manager; else link=~/.nix-profile; fi; readlink -f "$link" && stat -c %Y "$link""#.to_string()
        }
        other => return Err(anyhow!("unknown profile `{other}`")),
    };

    let target = build_ssh_target(node, profile, override_);
    let remote = ssh_capture(&target, &remote_cmd, override_).await?;

    // First line is the resolved store path; second line is the mtime
    // (seconds since epoch) of the symlink on the remote. Missing
    // second line just means we couldn't stat — not fatal.
    //
    // Defensive: drop suspiciously small values. Anything before
    // 2010 (mtime < 1262304000) is almost certainly a Nix-frozen
    // mtime and not a real activation time, so we hide it rather
    // than render "56 years ago".
    let mut lines = remote.lines();
    let remote_path = lines.next().unwrap_or("").trim().to_string();
    let activation_time = lines
        .next()
        .and_then(|s| s.trim().parse::<u64>().ok())
        .filter(|secs| *secs > 1_262_304_000)
        .map(|secs| std::time::UNIX_EPOCH + Duration::from_secs(secs));

    let local_trimmed = local.trim().to_string();
    Ok(ProfileCheck {
        up_to_date: remote_path == local_trimmed,
        local_path: local_trimmed,
        remote_path,
        activation_time,
    })
}

/// Medium-tier check: closure size delta.
///
/// Runs `nix path-info --closure-size` locally against the `local_path`
/// and again over SSH against `remote_path`, returning `(local_bytes,
/// remote_bytes)`. Both calls fail fast on non-zero exit so a missing
/// `nix` on the remote doesn't silently produce a bogus "0 B" delta.
pub async fn check_closure_sizes(
    node: &Node,
    profile: &str,
    local_path: &str,
    remote_path: &str,
    override_: &SshOverride,
) -> Result<(u64, u64)> {
    let local_size = local_closure_size(local_path)
        .await
        .context("local `nix path-info --closure-size`")?;
    let target = build_ssh_target(node, profile, override_);
    // Shell-quote the path defensively even though nix store paths are
    // ascii — if the user ever points at something weird we don't want
    // to explode the remote command.
    let remote_cmd = format!("nix path-info --closure-size '{remote_path}'");
    let remote = ssh_capture(&target, &remote_cmd, override_)
        .await
        .context("remote `nix path-info --closure-size`")?;
    let remote_size = parse_closure_size(&remote)
        .ok_or_else(|| anyhow!("unparseable remote closure size: `{}`", remote.trim()))?;
    Ok((local_size, remote_size))
}

/// Expensive-tier check: name+version diff between the local and the
/// remote closure.
///
/// We deliberately avoid `nix store diff-closures` here. The previous
/// implementation paid two heavy costs to use it: (1) `nix copy
/// --from ssh-ng://target <remote>` had to pull the *entire* closure
/// (every store path's actual contents) over the network into the
/// local store before the diff could run, and (2) the diff itself
/// then re-walks both closures. For a typical NixOS system that's
/// gigabytes of network transfer just to learn that openssl bumped
/// from 3.5.1 to 3.5.2.
///
/// Instead we do a metadata-only diff:
///
///   1. `nix-store --query --requisites <local_path>` locally — lists
///      every store path in the local closure (no I/O on the contents).
///   2. The same command on the remote over SSH — one round-trip,
///      typically a few hundred KB of text.
///   3. Parse `<hash>-<name>-<version>` from each store-path basename,
///      bucket by package name, and emit one line per name whose
///      version set differs (added / removed / updated).
///
/// This is dramatically faster (seconds vs minutes), at the cost of
/// not showing per-path closure-size deltas. The user explicitly
/// asked for "version change of each package" — that's exactly what
/// this surfaces.
///
/// `progress` receives one human-readable line per stage so the user
/// can see activity instead of staring at a silent spinner. The
/// channel is best-effort: a closed receiver is ignored.
pub async fn check_package_diff(
    node: &Node,
    profile: &str,
    local_path: &str,
    remote_path: &str,
    override_: &SshOverride,
    progress: mpsc::Sender<String>,
) -> Result<String> {
    let target = build_ssh_target(node, profile, override_);

    // Stage 1: list the local closure. This is a pure metadata query
    // against the local store and is essentially instantaneous.
    let _ = progress
        .send("[pkg] listing local closure …".to_string())
        .await;
    let local_paths = local_requisites(local_path).await.with_context(|| {
        format!("local `nix-store --query --requisites {local_path}`")
    })?;
    let _ = progress
        .send(format!("[pkg] local closure: {} paths", local_paths.len()))
        .await;

    // Stage 2: list the remote closure over SSH. One short ssh
    // round-trip; the response is a flat newline-separated list of
    // store paths. No actual store contents move across the wire.
    let _ = progress
        .send(format!("[pkg] listing remote closure on {target} …"))
        .await;
    let remote_cmd = format!("nix-store --query --requisites '{remote_path}'");
    let remote_out = ssh_capture(&target, &remote_cmd, override_)
        .await
        .with_context(|| {
            format!("remote `nix-store --query --requisites {remote_path}`")
        })?;
    let remote_paths: Vec<String> = remote_out
        .lines()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect();
    let _ = progress
        .send(format!(
            "[pkg] remote closure: {} paths",
            remote_paths.len()
        ))
        .await;

    // Stage 3: bucket each side by parsed package name → version set,
    // then walk the union of names and emit a line for any name whose
    // version differs (or where the package is only present on one
    // side). Names that match exactly produce no output, so the diff
    // length is the count of *real changes* — what the user wants to
    // see.
    let _ = progress
        .send("[pkg] computing version diff …".to_string())
        .await;
    let local_by_name = bucket_paths_by_name(&local_paths);
    let remote_by_name = bucket_paths_by_name(&remote_paths);

    let mut all_names: BTreeSet<&str> = BTreeSet::new();
    for k in local_by_name.keys() {
        all_names.insert(k.as_str());
    }
    for k in remote_by_name.keys() {
        all_names.insert(k.as_str());
    }

    let mut lines = Vec::<String>::new();
    for name in all_names {
        let l = local_by_name.get(name);
        let r = remote_by_name.get(name);
        let line = match (l, r) {
            (Some(lv), Some(rv)) if lv == rv => continue,
            (Some(lv), Some(rv)) => format!(
                "{name}: {} → {}",
                join_versions(rv),
                join_versions(lv)
            ),
            (Some(lv), None) => format!("{name}: + {}", join_versions(lv)),
            (None, Some(rv)) => format!("{name}: - {}", join_versions(rv)),
            (None, None) => continue,
        };
        let _ = progress.send(format!("[pkg] {line}")).await;
        lines.push(line);
    }

    let _ = progress
        .send(format!("[pkg] done ({} change(s))", lines.len()))
        .await;
    Ok(lines.join("\n"))
}

/// Run `nix-store --query --requisites <path>` against the local
/// store and return one line per store path. This is a metadata query
/// — it does not realise or copy any of the paths — so it works on
/// already-built closures and fails fast if the path isn't valid in
/// the local store.
///
/// `kill_on_drop(true)` is set so cancelling the awaiting future
/// (e.g. via the `x` key) actually reaps the child instead of
/// orphaning a long-running query.
async fn local_requisites(path: &str) -> Result<Vec<String>> {
    let out = Command::new("nix-store")
        .args(["--query", "--requisites", path])
        .stdin(Stdio::null())
        .kill_on_drop(true)
        .output()
        .await
        .context("spawning `nix-store --query --requisites`")?;
    if !out.status.success() {
        return Err(anyhow!(
            "nix-store --query --requisites failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        ));
    }
    Ok(String::from_utf8_lossy(&out.stdout)
        .lines()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect())
}

/// Bucket a list of store paths by parsed package name. Each value is
/// the set of distinct versions seen for that name in the closure
/// (most names map to a single version; multi-output derivations and
/// inputs that pin two versions of the same library are the
/// exceptions, hence a set rather than a single string).
fn bucket_paths_by_name(paths: &[String]) -> BTreeMap<String, BTreeSet<String>> {
    let mut map: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
    for p in paths {
        let base = p.rsplit('/').next().unwrap_or(p);
        let (name, version) = split_name_version(base);
        if name.is_empty() {
            continue;
        }
        map.entry(name).or_default().insert(version);
    }
    map
}

/// Parse `<hash>-<name>-<version>` from a Nix store path basename.
///
/// The hash is always the first dash-separated segment (32 lowercase
/// base32 chars in modern nix); after stripping it, we walk the
/// remainder looking for the first `-<digit>` boundary, which is
/// where nixpkgs convention puts the name/version split. Edge cases
/// (`linux-6.6.114-modules`, `bash-5.2-p37`, `python3.11-pip-24.0`)
/// all parse correctly because we only split at the *first* dash
/// followed by a digit. Paths that have no version (a bare derivation
/// name like `system-path`) are returned with an empty version
/// string.
fn split_name_version(basename: &str) -> (String, String) {
    let after_hash = match basename.find('-') {
        Some(i) => &basename[i + 1..],
        None => return (basename.to_string(), String::new()),
    };
    let bytes = after_hash.as_bytes();
    let mut split_at: Option<usize> = None;
    for i in 0..bytes.len() {
        if bytes[i] == b'-'
            && i + 1 < bytes.len()
            && bytes[i + 1].is_ascii_digit()
        {
            split_at = Some(i);
            break;
        }
    }
    match split_at {
        Some(i) => (
            after_hash[..i].to_string(),
            after_hash[i + 1..].to_string(),
        ),
        None => (after_hash.to_string(), String::new()),
    }
}

/// Render a sorted version set as a comma-separated string. Empty
/// versions render as `(no version)` so the diff doesn't print bare
/// dashes for derivations that didn't carry a version (e.g.
/// `system-path`).
fn join_versions(versions: &BTreeSet<String>) -> String {
    let parts: Vec<String> = versions
        .iter()
        .map(|v| {
            if v.is_empty() {
                "(no version)".to_string()
            } else {
                v.clone()
            }
        })
        .collect();
    parts.join(", ")
}

/// Ask the local nix store for the closure size of a path. Parses the
/// last whitespace-separated column of the first output line, which is
/// what `nix path-info --closure-size` emits.
///
/// Pre-checks that the path actually exists on the local filesystem
/// first. Without this, `nix path-info --closure-size` falls into its
/// "I'd need to build/substitute this to know" branch and emits the
/// cryptic `don't know how to build these paths` error. The user has
/// no way to act on that without knowing it really means "the local
/// store doesn't have this closure yet". We translate it into an
/// actionable hint instead.
async fn local_closure_size(path: &str) -> Result<u64> {
    if !std::path::Path::new(path).exists() {
        return Err(anyhow!(
            "local closure not built yet: {path}\n\
             hint: run `nix build {path}^*` (or build the deploy attribute that produces it) \
             so the closure is realised locally, then retry the size check"
        ));
    }
    let out = Command::new("nix")
        .args(["path-info", "--closure-size", path])
        .stdin(Stdio::null())
        .kill_on_drop(true)
        .output()
        .await
        .context("spawning `nix path-info`")?;
    if !out.status.success() {
        return Err(anyhow!(
            "nix path-info failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        ));
    }
    let text = String::from_utf8_lossy(&out.stdout);
    parse_closure_size(&text)
        .ok_or_else(|| anyhow!("unparseable local closure size: `{}`", text.trim()))
}

/// `nix path-info --closure-size` prints rows like `<path>\t<bytes>`;
/// pull the last whitespace column off the first line and parse it.
fn parse_closure_size(text: &str) -> Option<u64> {
    text.lines()
        .next()
        .and_then(|l| l.split_whitespace().last())
        .and_then(|s| s.parse().ok())
}

/// Build the `user@host` target the way `check_profile_up_to_date`
/// used to do inline. Factored out so the size/diff probes go through
/// the exact same resolution path — including the home-profile user
/// fallback — and can't drift.
fn build_ssh_target(node: &Node, profile: &str, override_: &SshOverride) -> String {
    let host = override_.effective_host(&node.hostname).to_string();
    let fallback_user = match profile {
        "home" => node
            .profiles
            .get("home")
            .and_then(|p| p.user.as_deref())
            .or(node.ssh_user.as_deref()),
        _ => node.ssh_user.as_deref(),
    };
    let user = override_.effective_user(fallback_user);
    match user {
        Some(u) => format!("{u}@{host}"),
        None => host,
    }
}

/// Ask Nix for the out-path of the activation derivation. This still
/// triggers evaluation (and a build of the closure if it's missing from the
/// store), so it should run in the background.
async fn local_profile_path(flake: &str, node: &str, profile: &str) -> Result<String> {
    let attr = format!("{flake}#deploy.nodes.{node}.profiles.{profile}.path");
    let output = Command::new("nix")
        .args(["eval", "--raw", "--no-warn-dirty", &attr])
        .stdin(Stdio::null())
        .kill_on_drop(true)
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
    // kill_on_drop is critical here: when the user presses `x`
    // mid-package-check, the spawned tokio task is aborted, which
    // drops the awaiting future and the Child along with it. Without
    // kill_on_drop the ssh process — and the remote nix-store command
    // it's running — would orphan and keep going.
    let mut child = cmd
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
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
