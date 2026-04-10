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

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    #[test]
    fn default_is_inactive() {
        let o = SshOverride::default();
        assert!(!o.is_active());
    }

    #[test]
    fn any_field_makes_active() {
        let cases = vec![
            SshOverride { hostname: Some("h".into()), ..Default::default() },
            SshOverride { user: Some("u".into()), ..Default::default() },
            SshOverride { identity: Some(PathBuf::from("/k")), ..Default::default() },
            SshOverride { extra_opts: Some("Port=22".into()), ..Default::default() },
        ];
        for o in cases {
            assert!(o.is_active(), "expected active for {o:?}");
        }
    }

    #[test]
    fn effective_host_prefers_override() {
        let o = SshOverride { hostname: Some("10.0.0.1".into()), ..Default::default() };
        assert_eq!(o.effective_host("fallback.example.com"), "10.0.0.1");
    }

    #[test]
    fn effective_host_falls_back() {
        let o = SshOverride::default();
        assert_eq!(o.effective_host("fallback.example.com"), "fallback.example.com");
    }

    #[test]
    fn effective_user_priority() {
        // Override wins over fallback.
        let o = SshOverride { user: Some("admin".into()), ..Default::default() };
        assert_eq!(o.effective_user(Some("root")), Some("admin"));

        // No override → fallback.
        let o = SshOverride::default();
        assert_eq!(o.effective_user(Some("root")), Some("root"));

        // Neither → None.
        let o = SshOverride::default();
        assert_eq!(o.effective_user(None), None);
    }

    #[test]
    fn ssh_args_empty_when_no_overrides() {
        let o = SshOverride::default();
        assert!(o.ssh_args().is_empty());
    }

    #[test]
    fn ssh_args_identity_and_opts() {
        let o = SshOverride {
            identity: Some(PathBuf::from("/home/me/.ssh/id_ed25519")),
            extra_opts: Some("Port=2222 ProxyJump=bastion".into()),
            ..Default::default()
        };
        let args = o.ssh_args();
        assert_eq!(
            args,
            vec![
                "-i", "/home/me/.ssh/id_ed25519",
                "-o", "Port=2222",
                "-o", "ProxyJump=bastion",
            ]
        );
    }

    #[test]
    fn deploy_ssh_opts_none_when_empty() {
        let o = SshOverride::default();
        assert_eq!(o.deploy_ssh_opts(), None);
    }

    #[test]
    fn deploy_ssh_opts_joins_all() {
        let o = SshOverride {
            identity: Some(PathBuf::from("/k")),
            extra_opts: Some("Port=22".into()),
            ..Default::default()
        };
        assert_eq!(o.deploy_ssh_opts(), Some("-i /k -o Port=22".into()));
    }

    #[test]
    fn summary_none_when_empty() {
        let o = SshOverride::default();
        assert_eq!(o.summary(), "(none)");
    }

    #[test]
    fn summary_shows_all_fields() {
        let o = SshOverride {
            hostname: Some("10.0.0.1".into()),
            user: Some("admin".into()),
            identity: Some(PathBuf::from("/k")),
            extra_opts: Some("Port=22".into()),
        };
        let s = o.summary();
        assert!(s.contains("host=10.0.0.1"), "{s}");
        assert!(s.contains("user=admin"), "{s}");
        assert!(s.contains("key=/k"), "{s}");
        assert!(s.contains("opts=Port=22"), "{s}");
    }
}
