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
        .unwrap_or_else(|| (override_.effective_host(hostname).to_string(), 22));
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
async fn resolve_ssh_endpoint(hostname: &str, override_: &SshOverride) -> Option<(String, u16)> {
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
    let output = timeout(Duration::from_secs(2), cmd.output())
        .await
        .ok()?
        .ok()?;
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
        "system" => "readlink -f /run/current-system && stat -c %Y /run/current-system".to_string(),
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

    // `local_trimmed` is the deploy-rs activation *wrapper* path — its
    // store hash is distinct from the toplevel that
    // `/run/current-system` resolves to, so a naive string compare
    // against `remote_path` would *always* say "needs update" even
    // when the host is perfectly current. Resolve the wrapper to the
    // toplevel it encloses and compare that instead. When the wrapper
    // isn't in the local store (or the resolve otherwise fails) fall
    // back to a parsed name+version equality check so we don't
    // regress into the old false-positive behaviour.
    let resolved_local = resolve_local_toplevel_quiet(&local_trimmed, &remote_path).await;
    let up_to_date = match resolved_local.as_deref() {
        Some(toplevel) => toplevel == remote_path,
        None => parsed_paths_equivalent(&local_trimmed, &remote_path),
    };
    // Prefer reporting the resolved toplevel to downstream UI/probes:
    // both the closure-size and package-diff tiers want the toplevel,
    // not the wrapper. If we couldn't resolve, keep the wrapper so
    // the expensive tiers can still run their own resolution.
    let reported_local = resolved_local.unwrap_or(local_trimmed);

    Ok(ProfileCheck {
        up_to_date,
        local_path: reported_local,
        remote_path,
        activation_time,
    })
}

/// Heuristic fallback for when we can't resolve the local wrapper to
/// its toplevel (wrapper isn't in the local store yet, references
/// unavailable, etc.). Parses `<hash>-<name>-<version>` from each
/// basename and compares the pair after peeling any deploy-rs
/// activation suffixes from the local side. Not as strict as a path
/// equality check — two builds of the same package with different
/// inputs can share a name+version but have different hashes — but
/// matches the user's mental model of "same nixos-generation" and is
/// strictly better than the raw-path compare it replaces.
fn parsed_paths_equivalent(local_path: &str, remote_path: &str) -> bool {
    let local_base = local_path.rsplit('/').next().unwrap_or(local_path);
    let remote_base = remote_path.rsplit('/').next().unwrap_or(remote_path);
    let (mut local_name, local_ver) = split_name_version(local_base);
    let (remote_name, remote_ver) = split_name_version(remote_base);
    for suffix in ["-activate-path", "-activate-rs"] {
        if let Some(stripped) = local_name.strip_suffix(suffix) {
            local_name = stripped.to_string();
            break;
        }
    }
    !local_name.is_empty()
        && !local_ver.is_empty()
        && local_name == remote_name
        && local_ver == remote_ver
}

/// Progress-free twin of [`resolve_local_toplevel`]. The cheap-tier
/// update check (`u`) runs on every profile of every targeted host,
/// so it can't afford to thread an `mpsc::Sender` through; this
/// version silently returns `None` on any failure (wrapper not in
/// store, references unavailable, no match) and lets the caller pick
/// a fallback.
async fn resolve_local_toplevel_quiet(wrapper_path: &str, remote_path: &str) -> Option<String> {
    let remote_base = remote_path.rsplit('/').next().unwrap_or(remote_path);
    let (remote_name, _) = split_name_version(remote_base);
    if remote_name.is_empty() {
        return None;
    }
    if !std::path::Path::new(wrapper_path).exists() {
        return None;
    }
    let out = Command::new("nix-store")
        .args(["--query", "--references", wrapper_path])
        .stdin(Stdio::null())
        .kill_on_drop(true)
        .output()
        .await
        .ok()?;
    if !out.status.success() {
        return None;
    }
    for line in String::from_utf8_lossy(&out.stdout).lines() {
        let p = line.trim();
        if p.is_empty() {
            continue;
        }
        let base = p.rsplit('/').next().unwrap_or(p);
        let (name, _) = split_name_version(base);
        if name == remote_name {
            return Some(p.to_string());
        }
    }
    None
}

/// Medium-tier check: closure size delta.
///
/// Runs `nix path-info --closure-size` locally against the `local_path`
/// and again over SSH against `remote_path`, returning `(local_bytes,
/// remote_bytes)`. Both calls fail fast on non-zero exit so a missing
/// `nix` on the remote doesn't silently produce a bogus "0 B" delta.
///
/// If the local closure isn't in the store yet, we shell out to
/// `nix build` against the flake attribute to force evaluation +
/// build first (see `ensure_local_closure`). `flake` is the flake
/// reference — same value as the one passed to `check_profile_up_to_date`.
///
/// `progress` receives one human-readable line per stage — mostly so
/// the build step (which can take tens of seconds for a fresh NixOS
/// system that hasn't been built locally yet) isn't invisible to the
/// user. The channel is best-effort: a closed receiver is ignored.
pub async fn check_closure_sizes(
    flake: &str,
    node: &Node,
    profile: &str,
    local_path: &str,
    remote_path: &str,
    override_: &SshOverride,
    progress: mpsc::Sender<String>,
) -> Result<(u64, u64)> {
    // Step 1: make sure the deploy-rs activation wrapper is in the
    // local store. Nothing to do if it already is.
    ensure_local_closure(flake, &node.name, profile, local_path, &progress, "size").await?;
    // Step 2: resolve the wrapper down to the actual toplevel so we
    // compare apples-to-apples with the remote's /run/current-system
    // target. See `resolve_local_toplevel` for the rationale.
    let resolved_local = resolve_local_toplevel(local_path, remote_path, &progress, "size").await?;
    let _ = progress
        .send("[size] measuring local closure …".to_string())
        .await;
    let local_size = nix_closure_size(&resolved_local)
        .await
        .context("local `nix path-info --closure-size`")?;
    let _ = progress
        .send(format!("[size] local: {} bytes", local_size))
        .await;
    let target = build_ssh_target(node, profile, override_);
    // Shell-quote the path defensively even though nix store paths are
    // ascii — if the user ever points at something weird we don't want
    // to explode the remote command.
    let _ = progress
        .send(format!("[size] measuring remote closure on {target} …"))
        .await;
    let remote_cmd = format!("nix path-info --closure-size '{remote_path}'");
    let remote = ssh_capture(&target, &remote_cmd, override_)
        .await
        .context("remote `nix path-info --closure-size`")?;
    let remote_size = parse_closure_size(&remote)
        .ok_or_else(|| anyhow!("unparseable remote closure size: `{}`", remote.trim()))?;
    let _ = progress
        .send(format!("[size] remote: {} bytes", remote_size))
        .await;
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
    flake: &str,
    node: &Node,
    profile: &str,
    local_path: &str,
    remote_path: &str,
    override_: &SshOverride,
    progress: mpsc::Sender<String>,
) -> Result<String> {
    let target = build_ssh_target(node, profile, override_);

    // Step 1: make sure the wrapper is built, then resolve the real
    // toplevel inside it — same two-step as check_closure_sizes.
    ensure_local_closure(flake, &node.name, profile, local_path, &progress, "pkg").await?;
    let resolved_local = resolve_local_toplevel(local_path, remote_path, &progress, "pkg").await?;

    // Stage 1: list the local closure. This is a pure metadata query
    // against the local store and is essentially instantaneous.
    let _ = progress
        .send("[pkg] listing local closure …".to_string())
        .await;
    let local_paths = nix_requisites(&resolved_local)
        .await
        .with_context(|| format!("local `nix-store --query --requisites {resolved_local}`"))?;
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
        .with_context(|| format!("remote `nix-store --query --requisites {remote_path}`"))?;
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

    let _ = progress
        .send("[pkg] computing version diff …".to_string())
        .await;
    let lines = compute_version_diff(&local_paths, &remote_paths);
    for line in &lines {
        let _ = progress.send(format!("[pkg] {line}")).await;
    }
    let _ = progress
        .send(format!("[pkg] done ({} change(s))", lines.len()))
        .await;
    Ok(lines.join("\n"))
}

/// Pure-logic version + content diff between two closure path lists.
///
/// Buckets each side by parsed `<name, version>`, walks the union, and
/// emits one line per name whose version set differs. When no version
/// changes exist but the store-path sets still diverge (config-file
/// rebuilds), emits a `(content-only)` summary with sample basenames.
fn compute_version_diff(local_paths: &[String], remote_paths: &[String]) -> Vec<String> {
    let local_by_name = bucket_paths_by_name(local_paths);
    let remote_by_name = bucket_paths_by_name(remote_paths);

    let mut all_names: BTreeSet<&str> = BTreeSet::new();
    for k in local_by_name.keys() {
        all_names.insert(k.as_str());
    }
    for k in remote_by_name.keys() {
        all_names.insert(k.as_str());
    }

    let mut lines = Vec::<String>::new();
    for name in &all_names {
        let l = local_by_name.get(*name);
        let r = remote_by_name.get(*name);
        let line = match (l, r) {
            (Some(lv), Some(rv)) if lv == rv => continue,
            (Some(lv), Some(rv)) => {
                format!("{name}: {} → {}", join_versions(rv), join_versions(lv))
            }
            (Some(lv), None) => format!("{name}: + {}", join_versions(lv)),
            (None, Some(rv)) => format!("{name}: - {}", join_versions(rv)),
            (None, None) => continue,
        };
        lines.push(line);
    }

    // Content-only diff: every package name+version matches but the
    // actual store-path sets still differ (config-file rebuilds).
    if lines.is_empty() {
        let local_set: BTreeSet<&str> = local_paths.iter().map(|s| s.as_str()).collect();
        let remote_set: BTreeSet<&str> = remote_paths.iter().map(|s| s.as_str()).collect();
        let only_local: Vec<&str> = local_set.difference(&remote_set).copied().collect();
        let only_remote: Vec<&str> = remote_set.difference(&local_set).copied().collect();
        if !only_local.is_empty() || !only_remote.is_empty() {
            lines.push(format!(
                "(content-only) {} path(s) differ — same package versions, different contents",
                only_local.len().max(only_remote.len())
            ));
            for p in only_local.iter().take(8) {
                let base = p.rsplit('/').next().unwrap_or(p);
                lines.push(format!("  + {base}"));
            }
            for p in only_remote.iter().take(8) {
                let base = p.rsplit('/').next().unwrap_or(p);
                lines.push(format!("  - {base}"));
            }
            let extra = only_local.len().saturating_sub(8) + only_remote.len().saturating_sub(8);
            if extra > 0 {
                lines.push(format!("  … and {extra} more path(s)"));
            }
        }
    }
    lines
}

/// Run `nix-store --query --requisites <path>` against the local
/// store and return one line per store path. Pure metadata query —
/// the caller must guarantee the path is already in the local store
/// (see `ensure_local_closure`).
///
/// `kill_on_drop(true)` is set so cancelling the awaiting future
/// (e.g. via the `x` key) actually reaps the child instead of
/// orphaning a long-running query.
async fn nix_requisites(path: &str) -> Result<Vec<String>> {
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
///
/// **deploy-rs wrapper suffixes.** After the initial split we peel
/// known deploy-rs suffixes (`-activate-path`, `-activate-rs`) off
/// the end of the version and glue them back onto the name. Without
/// this, `nixos-system-host-26.05.20260405.68d8aa3-activate-path`
/// parses to name=`nixos-system-host`, version=`26.05.…-activate-path`
/// — which then looks like a "different version" of the real
/// `nixos-system-host-26.05.…` path when buckets are compared, and
/// the diff shows spurious "updates" even when the closures are
/// identical.
fn split_name_version(basename: &str) -> (String, String) {
    let after_hash = match basename.find('-') {
        Some(i) => &basename[i + 1..],
        None => return (basename.to_string(), String::new()),
    };
    let bytes = after_hash.as_bytes();
    let mut split_at: Option<usize> = None;
    for i in 0..bytes.len() {
        if bytes[i] == b'-' && i + 1 < bytes.len() && bytes[i + 1].is_ascii_digit() {
            split_at = Some(i);
            break;
        }
    }
    let (mut name, mut version) = match split_at {
        Some(i) => (after_hash[..i].to_string(), after_hash[i + 1..].to_string()),
        None => (after_hash.to_string(), String::new()),
    };
    for suffix in ["-activate-path", "-activate-rs"] {
        if let Some(stripped) = version.strip_suffix(suffix) {
            version = stripped.to_string();
            name.push_str(suffix);
            break;
        }
    }
    (name, version)
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
/// what `nix path-info --closure-size` emits. Pure measurement — the
/// caller must guarantee the path is already in the local store (see
/// `ensure_local_closure`).
async fn nix_closure_size(path: &str) -> Result<u64> {
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

/// Resolve the deploy-rs activation wrapper at `wrapper_path` down to
/// the actual system/home toplevel that the remote's
/// `/run/current-system` (or `~/.local/state/nix/profiles/home-manager`)
/// symlink will end up pointing at after activation.
///
/// Why this matters: the wrapper is a thin derivation whose closure is
/// a strict superset of the toplevel's — it adds a handful of paths
/// (the `activate` script, `deploy-rs` itself, `activatable-…`
/// sub-wrappers) that never reach the deployed system. Comparing the
/// wrapper against the remote toplevel produces a noisy "5 packages
/// changed" output even when the systems are bit-identical, because
/// those wrapper-only paths look like additions on the local side.
///
/// We identify the toplevel among the wrapper's direct references by
/// matching parsed package names against the remote basename. Both
/// sides use the same naming scheme (`nixos-system-<host>`,
/// `home-manager-generation`, etc.), so the match is unambiguous.
///
/// If anything goes wrong — `nix-store --query --references` fails, no
/// reference matches, etc. — we fall back to returning the wrapper
/// path itself and log a line so the user sees why the diff is noisy.
async fn resolve_local_toplevel(
    wrapper_path: &str,
    remote_path: &str,
    progress: &mpsc::Sender<String>,
    tag: &str,
) -> Result<String> {
    let remote_base = remote_path.rsplit('/').next().unwrap_or(remote_path);
    let (remote_name, _) = split_name_version(remote_base);
    if remote_name.is_empty() {
        return Ok(wrapper_path.to_string());
    }
    let out = Command::new("nix-store")
        .args(["--query", "--references", wrapper_path])
        .stdin(Stdio::null())
        .kill_on_drop(true)
        .output()
        .await
        .context("spawning `nix-store --query --references`")?;
    if !out.status.success() {
        let _ = progress
            .send(format!(
                "[{tag}] couldn't list wrapper references, diffing against wrapper"
            ))
            .await;
        return Ok(wrapper_path.to_string());
    }
    for line in String::from_utf8_lossy(&out.stdout).lines() {
        let p = line.trim();
        if p.is_empty() {
            continue;
        }
        let base = p.rsplit('/').next().unwrap_or(p);
        let (name, _) = split_name_version(base);
        if name == remote_name {
            let _ = progress
                .send(format!("[{tag}] resolved local toplevel: {p}"))
                .await;
            return Ok(p.to_string());
        }
    }
    let _ = progress
        .send(format!(
            "[{tag}] no wrapper reference matched `{remote_name}`, diffing against wrapper"
        ))
        .await;
    Ok(wrapper_path.to_string())
}

/// Make sure the local closure for a deploy-rs profile exists on this
/// machine. If the store path is already present we're done; otherwise
/// we drive `nix build` against the flake attribute so Nix evaluates
/// the profile, instantiates its derivation, pulls from substituters
/// or builds locally as needed, and populates the store.
///
/// `nix-store --realise` is NOT enough here — it only works when the
/// path's corresponding `.drv` is already in the local store. In our
/// case the caller got `path` from `nix eval --raw`, which *evaluates*
/// the profile's `.path` attribute (returning a string) but never
/// instantiates a derivation on disk. Without a `.drv`, `nix-store
/// --realise` can't know how to build the closure and dies with
/// `don't know how to build these paths`. Going through `nix build
/// <flake>#deploy.nodes.<node>.profiles.<profile>.path` re-enters the
/// flake, which *does* instantiate and build.
///
/// `tag` prefixes the progress lines so the user can see which feature
/// triggered the build (`size` for closure size, `pkg` for the
/// package diff). `kill_on_drop(true)` is set so cancelling the task
/// (key `x`) tears down the builder child instead of leaving it
/// running in the background.
async fn ensure_local_closure(
    flake: &str,
    node: &str,
    profile: &str,
    path: &str,
    progress: &mpsc::Sender<String>,
    tag: &str,
) -> Result<()> {
    if std::path::Path::new(path).exists() {
        return Ok(());
    }
    let attr = format!("{flake}#deploy.nodes.{node}.profiles.{profile}.path");
    let _ = progress
        .send(format!("[{tag}] local closure missing, building {attr} …"))
        .await;
    // --no-link avoids dropping a `result` symlink in the user's cwd;
    // --print-out-paths gives us the store path nix actually settled
    // on, which we can sanity-check against `path` afterwards.
    let out = Command::new("nix")
        .args([
            "build",
            "--no-link",
            "--no-warn-dirty",
            "--print-out-paths",
            &attr,
        ])
        .stdin(Stdio::null())
        .kill_on_drop(true)
        .output()
        .await
        .context("spawning `nix build`")?;
    if !out.status.success() {
        let stderr = String::from_utf8_lossy(&out.stderr);
        return Err(anyhow!(
            "`nix build {attr}` failed — couldn't materialise the \
             local closure.\n\
             hint: make sure the deploy attribute is buildable from \
             this machine (try running the command above by hand to \
             see the full nix output), then retry.\n\
             nix stderr: {}",
            stderr.trim()
        ));
    }
    // Pull the (possibly multi-line) out-path list from stdout and
    // sanity-check that at least one entry matches the path we were
    // asked to ensure. The profile's `.path` attribute in deploy-rs
    // stringifies as `<outPath>/activate`, so nix build prints the
    // parent directory — the same value the caller already trimmed to.
    let stdout = String::from_utf8_lossy(&out.stdout);
    let built: Vec<&str> = stdout
        .lines()
        .map(|s| s.trim())
        .filter(|s| !s.is_empty())
        .collect();
    let _ = progress
        .send(format!(
            "[{tag}] built {} path(s): {}",
            built.len(),
            built.join(" ")
        ))
        .await;
    // After nix build, the original path should exist. If it still
    // doesn't, something upstream has a mismatched `.path` attribute
    // vs. the built derivation — surface that clearly so the user
    // doesn't get a confusing downstream error from `nix path-info`.
    if !std::path::Path::new(path).exists() {
        return Err(anyhow!(
            "`nix build {attr}` succeeded but expected store path \
             `{path}` still isn't present.\n\
             hint: the profile's `.path` attribute may not match the \
             derivation it's wrapping. nix build produced: {}",
            built.join(" ")
        ));
    }
    Ok(())
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
    // deploy-rs path strings end with `/activate`; strip that to get
    // the store path of the activation *wrapper*. Note this is NOT
    // the toplevel the remote's `/run/current-system` resolves to —
    // the wrapper has the toplevel as one of its references and
    // lives at a different hash. Callers that compare against the
    // remote path need to resolve the wrapper first (see
    // `resolve_local_toplevel` / `resolve_local_toplevel_quiet`).
    let raw = String::from_utf8(output.stdout).context("`nix eval --raw` returned non-utf8")?;
    Ok(raw.trim_end_matches("/activate").to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeSet;

    // ---- split_name_version ----

    #[test]
    fn split_basic_package() {
        let (name, ver) = split_name_version("abc123-openssl-3.5.1");
        assert_eq!(name, "openssl");
        assert_eq!(ver, "3.5.1");
    }

    #[test]
    fn split_no_version() {
        let (name, ver) = split_name_version("abc123-system-path");
        assert_eq!(name, "system-path");
        assert_eq!(ver, "");
    }

    #[test]
    fn split_nixos_system_with_activate_suffix() {
        // deploy-rs wrapper path
        let (name, ver) =
            split_name_version("abc123-nixos-system-myhost-26.05.20260405.68d8aa3-activate-path");
        assert_eq!(name, "nixos-system-myhost-activate-path");
        assert_eq!(ver, "26.05.20260405.68d8aa3");
    }

    #[test]
    fn split_activate_rs_suffix() {
        let (name, ver) =
            split_name_version("abc123-nixos-system-myhost-26.05.12345-activate-rs");
        assert_eq!(name, "nixos-system-myhost-activate-rs");
        assert_eq!(ver, "26.05.12345");
    }

    #[test]
    fn split_linux_modules() {
        let (name, ver) = split_name_version("abc123-linux-6.6.114-modules");
        assert_eq!(name, "linux");
        assert_eq!(ver, "6.6.114-modules");
    }

    #[test]
    fn split_python_package() {
        let (name, ver) = split_name_version("abc123-python3.11-pip-24.0");
        assert_eq!(name, "python3.11-pip");
        assert_eq!(ver, "24.0");
    }

    #[test]
    fn split_bash_patch() {
        let (name, ver) = split_name_version("abc123-bash-5.2-p37");
        assert_eq!(name, "bash");
        assert_eq!(ver, "5.2-p37");
    }

    #[test]
    fn split_no_hash_separator() {
        let (name, ver) = split_name_version("nohash");
        assert_eq!(name, "nohash");
        assert_eq!(ver, "");
    }

    // ---- parsed_paths_equivalent ----

    #[test]
    fn equivalent_same_toplevel() {
        assert!(parsed_paths_equivalent(
            "/nix/store/abc123-nixos-system-host-26.05.12345",
            "/nix/store/abc123-nixos-system-host-26.05.12345",
        ));
    }

    #[test]
    fn equivalent_wrapper_vs_toplevel() {
        // Wrapper has -activate-path suffix; the function should peel it.
        assert!(parsed_paths_equivalent(
            "/nix/store/xyz-nixos-system-host-26.05.12345-activate-path",
            "/nix/store/abc-nixos-system-host-26.05.12345",
        ));
    }

    #[test]
    fn not_equivalent_different_versions() {
        assert!(!parsed_paths_equivalent(
            "/nix/store/abc-nixos-system-host-26.05.11111",
            "/nix/store/abc-nixos-system-host-26.05.22222",
        ));
    }

    // ---- parse_closure_size ----

    #[test]
    fn parse_closure_size_normal() {
        let input = "/nix/store/abc-foo\t1234567890\n";
        assert_eq!(parse_closure_size(input), Some(1234567890));
    }

    #[test]
    fn parse_closure_size_spaces() {
        let input = "/nix/store/abc-foo   999\n";
        assert_eq!(parse_closure_size(input), Some(999));
    }

    #[test]
    fn parse_closure_size_empty() {
        assert_eq!(parse_closure_size(""), None);
    }

    #[test]
    fn parse_closure_size_no_number() {
        assert_eq!(parse_closure_size("garbage"), None);
    }

    // ---- join_versions ----

    #[test]
    fn join_versions_single() {
        let mut s = BTreeSet::new();
        s.insert("3.5.1".to_string());
        assert_eq!(join_versions(&s), "3.5.1");
    }

    #[test]
    fn join_versions_empty_version() {
        let mut s = BTreeSet::new();
        s.insert(String::new());
        assert_eq!(join_versions(&s), "(no version)");
    }

    #[test]
    fn join_versions_multiple() {
        let mut s = BTreeSet::new();
        s.insert("1.0".to_string());
        s.insert("2.0".to_string());
        assert_eq!(join_versions(&s), "1.0, 2.0");
    }

    // ---- bucket_paths_by_name ----

    #[test]
    fn bucket_groups_by_name() {
        let paths = vec![
            "/nix/store/aaa-openssl-3.5.1".to_string(),
            "/nix/store/bbb-openssl-3.5.2".to_string(),
            "/nix/store/ccc-bash-5.2".to_string(),
        ];
        let map = bucket_paths_by_name(&paths);
        assert_eq!(map.len(), 2);
        let openssl = map.get("openssl").unwrap();
        assert!(openssl.contains("3.5.1"));
        assert!(openssl.contains("3.5.2"));
        let bash = map.get("bash").unwrap();
        assert!(bash.contains("5.2"));
    }

    // ---- compute_version_diff ----

    #[test]
    fn diff_identical_closures() {
        let paths = vec![
            "/nix/store/aaa-openssl-3.5.1".to_string(),
            "/nix/store/bbb-bash-5.2".to_string(),
        ];
        let lines = compute_version_diff(&paths, &paths);
        assert!(lines.is_empty(), "identical closures should have no diff");
    }

    #[test]
    fn diff_version_update() {
        let local = vec!["/nix/store/aaa-openssl-3.5.2".to_string()];
        let remote = vec!["/nix/store/bbb-openssl-3.5.1".to_string()];
        let lines = compute_version_diff(&local, &remote);
        assert_eq!(lines.len(), 1);
        assert!(lines[0].contains("openssl"), "{}", lines[0]);
        assert!(lines[0].contains("3.5.1"), "{}", lines[0]);
        assert!(lines[0].contains("3.5.2"), "{}", lines[0]);
    }

    #[test]
    fn diff_added_package() {
        let local = vec![
            "/nix/store/aaa-openssl-3.5.1".to_string(),
            "/nix/store/bbb-curl-8.0".to_string(),
        ];
        let remote = vec!["/nix/store/ccc-openssl-3.5.1".to_string()];
        let lines = compute_version_diff(&local, &remote);
        assert_eq!(lines.len(), 1);
        assert!(lines[0].contains("curl"), "{}", lines[0]);
        assert!(lines[0].starts_with("curl: +"), "{}", lines[0]);
    }

    #[test]
    fn diff_removed_package() {
        let local = vec!["/nix/store/aaa-openssl-3.5.1".to_string()];
        let remote = vec![
            "/nix/store/bbb-openssl-3.5.1".to_string(),
            "/nix/store/ccc-curl-8.0".to_string(),
        ];
        let lines = compute_version_diff(&local, &remote);
        assert_eq!(lines.len(), 1);
        assert!(lines[0].contains("curl"), "{}", lines[0]);
        assert!(lines[0].starts_with("curl: -"), "{}", lines[0]);
    }

    #[test]
    fn diff_content_only_change() {
        // Same name+version but different store hashes → content-only.
        let local = vec!["/nix/store/aaa-openssl-3.5.1".to_string()];
        let remote = vec!["/nix/store/bbb-openssl-3.5.1".to_string()];
        let lines = compute_version_diff(&local, &remote);
        assert!(!lines.is_empty());
        assert!(
            lines[0].contains("(content-only)"),
            "expected content-only marker, got: {}",
            lines[0]
        );
    }

    // ---- build_ssh_target ----

    #[test]
    fn build_ssh_target_no_override() {
        let node = Node {
            name: "myhost".into(),
            hostname: "myhost.example.com".into(),
            ssh_user: Some("root".into()),
            profiles: BTreeMap::new(),
        };
        let o = SshOverride::default();
        assert_eq!(build_ssh_target(&node, "system", &o), "root@myhost.example.com");
    }

    #[test]
    fn build_ssh_target_with_override() {
        let node = Node {
            name: "myhost".into(),
            hostname: "myhost.example.com".into(),
            ssh_user: Some("root".into()),
            profiles: BTreeMap::new(),
        };
        let o = SshOverride {
            hostname: Some("10.0.0.5".into()),
            user: Some("admin".into()),
            ..Default::default()
        };
        assert_eq!(build_ssh_target(&node, "system", &o), "admin@10.0.0.5");
    }

    #[test]
    fn build_ssh_target_no_user() {
        let node = Node {
            name: "myhost".into(),
            hostname: "myhost.example.com".into(),
            ssh_user: None,
            profiles: BTreeMap::new(),
        };
        let o = SshOverride::default();
        // No user at all → bare hostname.
        assert_eq!(build_ssh_target(&node, "system", &o), "myhost.example.com");
    }

    #[test]
    fn build_ssh_target_home_profile_user() {
        use crate::flake::Profile;
        let mut profiles = BTreeMap::new();
        profiles.insert(
            "home".into(),
            Profile { user: Some("jd".into()) },
        );
        let node = Node {
            name: "myhost".into(),
            hostname: "myhost.example.com".into(),
            ssh_user: None,
            profiles,
        };
        let o = SshOverride::default();
        // Home profile should use its own user.
        assert_eq!(build_ssh_target(&node, "home", &o), "jd@myhost.example.com");
    }
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
