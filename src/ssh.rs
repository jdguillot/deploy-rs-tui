//! Per-host SSH overrides.
//!
//! When a node isn't in `~/.ssh/config` (or you want to deploy to a
//! different IP / user / key for one session), the user can set fields
//! here from the TUI. The same overrides feed both the status checks in
//! `host.rs` and the `deploy` invocation in `deploy.rs`, so behaviour
//! stays consistent.

use std::path::PathBuf;

#[derive(Debug, Clone, Default)]
pub struct SshOverride {
    /// Override the hostname / IP. When set, this is what we connect to
    /// instead of `node.hostname`.
    pub hostname: Option<String>,
    /// Override the SSH user.
    pub user: Option<String>,
    /// Path to a private key (`-i`). Stored as a `PathBuf` so we can
    /// validate it later if needed.
    pub identity: Option<PathBuf>,
    /// Free-form `-o` options as a single string, e.g.
    /// `Port=2222 ProxyJump=bastion`. Each whitespace-separated token is
    /// passed as its own `-o ...` pair.
    pub extra_opts: Option<String>,
}

impl SshOverride {
    /// True if any field is set. Used by the UI to render the
    /// "this host has overrides" indicator.
    pub fn is_active(&self) -> bool {
        self.hostname.is_some()
            || self.user.is_some()
            || self.identity.is_some()
            || self.extra_opts.is_some()
    }

    /// The hostname/IP we should actually dial — override or fallback.
    pub fn effective_host<'a>(&'a self, fallback: &'a str) -> &'a str {
        self.hostname.as_deref().unwrap_or(fallback)
    }

    /// The SSH user we should use — override, then provided fallback,
    /// then `None` (defer to ssh's own default).
    pub fn effective_user<'a>(&'a self, fallback: Option<&'a str>) -> Option<&'a str> {
        self.user.as_deref().or(fallback)
    }

    /// Build an `ssh ...` argv suffix that applies this override. The
    /// returned vector contains every flag *before* the target host —
    /// callers should append `[target, command]` themselves.
    pub fn ssh_args(&self) -> Vec<String> {
        let mut args = Vec::new();
        if let Some(id) = &self.identity {
            args.push("-i".to_string());
            args.push(id.to_string_lossy().into_owned());
        }
        if let Some(opts) = &self.extra_opts {
            for token in opts.split_whitespace() {
                args.push("-o".to_string());
                args.push(token.to_string());
            }
        }
        args
    }

    /// Build the value for deploy-rs's `--ssh-opts "..."` flag (a single
    /// string, not split per token like ssh's argv). Returns `None` when
    /// there's nothing to pass — the caller skips the flag entirely in
    /// that case so we don't accidentally clobber the flake's own
    /// `sshOpts`.
    pub fn deploy_ssh_opts(&self) -> Option<String> {
        let mut parts: Vec<String> = Vec::new();
        if let Some(id) = &self.identity {
            parts.push("-i".to_string());
            parts.push(id.to_string_lossy().into_owned());
        }
        if let Some(opts) = &self.extra_opts {
            for token in opts.split_whitespace() {
                parts.push("-o".to_string());
                parts.push(token.to_string());
            }
        }
        if parts.is_empty() {
            None
        } else {
            Some(parts.join(" "))
        }
    }

    /// Short, human-readable summary for the details pane.
    pub fn summary(&self) -> String {
        let mut bits = Vec::new();
        if let Some(h) = &self.hostname {
            bits.push(format!("host={h}"));
        }
        if let Some(u) = &self.user {
            bits.push(format!("user={u}"));
        }
        if let Some(i) = &self.identity {
            bits.push(format!("key={}", i.display()));
        }
        if let Some(o) = &self.extra_opts {
            bits.push(format!("opts={o}"));
        }
        if bits.is_empty() {
            "(none)".to_string()
        } else {
            bits.join("  ")
        }
    }
}
