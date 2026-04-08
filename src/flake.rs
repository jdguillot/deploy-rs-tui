//! Flake discovery — talks to `nix eval` to enumerate `deploy.nodes`.
//!
//! We deliberately stay shallow: we read only `hostname`, `sshUser`, and the
//! list of profile names. Touching `path` would force evaluation of the full
//! NixOS / home-manager configurations, which is slow and not needed to draw
//! the host list.

use std::collections::BTreeMap;
use std::process::Stdio;

use anyhow::{anyhow, Context, Result};
use serde::Deserialize;
use tokio::process::Command;

/// One profile (e.g. `system`, `home`) attached to a node.
#[derive(Debug, Clone, Deserialize)]
pub struct Profile {
    /// SSH user used to push this profile (defaults vary; deploy-rs falls
    /// back to root for system, the profile owner for home).
    #[serde(default)]
    pub user: Option<String>,
}

/// One entry in `deploy.nodes`.
#[derive(Debug, Clone, Deserialize)]
pub struct Node {
    /// The attribute name in `deploy.nodes`.
    #[serde(skip)]
    pub name: String,
    /// The hostname deploy-rs will SSH to.
    pub hostname: String,
    /// SSH user at the node level (profiles can override).
    #[serde(default, rename = "sshUser")]
    pub ssh_user: Option<String>,
    /// Profile attrs keyed by name (`system`, `home`, …).
    #[serde(default)]
    pub profiles: BTreeMap<String, Profile>,
}

impl Node {
    /// True if a `system` profile is present (NixOS / "host config").
    pub fn has_system(&self) -> bool {
        self.profiles.contains_key("system")
    }

    /// True if a `home` profile is present (home-manager).
    pub fn has_home(&self) -> bool {
        self.profiles.contains_key("home")
    }
}

/// Run `nix eval --json` on the flake and parse the resulting attrset.
pub async fn discover(flake: &str) -> Result<Vec<Node>> {
    // Apply function strips the heavy `path` derivations and keeps only the
    // metadata we render. Doing this in Nix avoids forcing evaluation of
    // the per-host modules.
    let apply = r#"nodes: builtins.mapAttrs (n: v: {
      hostname = v.hostname;
      sshUser = v.sshUser or null;
      profiles = builtins.mapAttrs (pn: pv: {
        user = pv.user or null;
      }) v.profiles;
    }) nodes"#;

    let target = format!("{flake}#deploy.nodes");
    let output = Command::new("nix")
        .args([
            "eval",
            "--json",
            "--no-warn-dirty",
            &target,
            "--apply",
            apply,
        ])
        .stdin(Stdio::null())
        .output()
        .await
        .context("spawning `nix eval`")?;

    if !output.status.success() {
        return Err(anyhow!(
            "`nix eval {target}` failed:\n{}",
            String::from_utf8_lossy(&output.stderr)
        ));
    }

    let raw: BTreeMap<String, Node> =
        serde_json::from_slice(&output.stdout).context("parsing `nix eval` JSON output")?;

    Ok(raw
        .into_iter()
        .map(|(name, mut node)| {
            node.name = name;
            node
        })
        .collect())
}
