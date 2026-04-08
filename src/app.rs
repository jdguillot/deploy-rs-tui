//! TUI application state and the main event loop.
//!
//! The App owns:
//! - the discovered nodes and their per-node status
//! - the currently selected node + deploy mode + profile selection
//! - a tail-buffered log
//! - any in-flight background work (status checks, deploy run)
//!
//! The loop is a single `tokio::select!` over (a) terminal/tick events,
//! (b) status-check completions, and (c) deploy log lines.

use std::collections::HashMap;
use std::path::PathBuf;

use anyhow::Result;
use crossterm::event::{Event as CtEvent, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use tokio::sync::mpsc;
use tokio::task::JoinHandle;

use crate::deploy::{self, DeployRequest, LogLine, Mode, ProfileSel, Toggles};
use crate::event::{spawn as spawn_events, AppEvent};
use crate::flake::Node;
use crate::host::{self, HostStatus, Reachability, UpdateState};
use crate::ssh::SshOverride;
use crate::ui::{self, Tui};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FocusPane {
    Hosts,
    Details,
}

#[derive(Debug, Clone)]
pub struct LogEntry {
    pub text: String,
    pub is_err: bool,
}

/// Which override field the user is currently editing. Drives both the
/// prompt label and where the parsed buffer gets stored on Enter.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OverrideField {
    Hostname,
    User,
    Identity,
    Opts,
}

impl OverrideField {
    pub fn label(self) -> &'static str {
        match self {
            OverrideField::Hostname => "hostname / IP",
            OverrideField::User => "ssh user",
            OverrideField::Identity => "identity file",
            OverrideField::Opts => "extra ssh opts",
        }
    }
}

/// Top-level input mode. The vast majority of the time we're in `Normal`;
/// when the user opens an override prompt or the overrides menu we route
/// keys differently.
#[derive(Debug, Clone)]
pub enum InputMode {
    Normal,
    /// User pressed `o` and is picking which field to edit (or `c` to
    /// clear). Single-key sub-menu.
    OverridesMenu,
    /// User is typing into a single-line text buffer for `field`.
    EditOverride { field: OverrideField, buf: String },
}

/// Background work updates we receive over the status channel.
#[derive(Debug)]
enum StatusUpdate {
    Reachability(String, Reachability),
    UpdateProbe {
        node: String,
        profile: String,
        result: Result<bool, String>,
    },
}

pub struct App {
    pub flake: String,
    pub nodes: Vec<Node>,
    pub status: HashMap<String, HostStatus>,
    /// Per-node SSH overrides keyed by node name. Empty unless the user
    /// explicitly sets something.
    pub overrides: HashMap<String, SshOverride>,

    pub selected: usize,
    pub focus: FocusPane,
    pub mode: Mode,
    pub profile_sel: ProfileSel,
    pub toggles: Toggles,

    pub log: Vec<LogEntry>,
    pub busy_label: Option<String>,
    pub show_help: bool,
    pub input: InputMode,
    /// Monotonic counter incremented on every tick. The UI uses it to pick
    /// a spinner frame so in-flight work animates without us tracking time
    /// explicitly per host.
    pub tick_counter: u64,

    /// Channel that background tasks publish status updates on.
    status_tx: mpsc::Sender<StatusUpdate>,
    status_rx: mpsc::Receiver<StatusUpdate>,

    /// In-flight deploy. We hold both the receiver (for log lines) and the
    /// task handle so we can cancel.
    deploy_rx: Option<mpsc::Receiver<LogLine>>,
    deploy_task: Option<JoinHandle<()>>,

    /// True once we receive a quit request.
    should_quit: bool,
}

impl App {
    pub fn new(flake: String, nodes: Vec<Node>) -> Self {
        let (status_tx, status_rx) = mpsc::channel(64);
        let mut status = HashMap::new();
        for n in &nodes {
            status.insert(n.name.clone(), HostStatus::default());
        }
        Self {
            flake,
            nodes,
            status,
            overrides: HashMap::new(),
            selected: 0,
            focus: FocusPane::Hosts,
            mode: Mode::Switch,
            profile_sel: ProfileSel::All,
            toggles: Toggles::default(),
            log: Vec::new(),
            busy_label: None,
            show_help: false,
            input: InputMode::Normal,
            tick_counter: 0,
            status_tx,
            status_rx,
            deploy_rx: None,
            deploy_task: None,
            should_quit: false,
        }
    }

    pub fn selected_node(&self) -> Option<&Node> {
        self.nodes.get(self.selected)
    }

    pub fn status_for(&self, name: &str) -> HostStatus {
        self.status.get(name).cloned().unwrap_or_default()
    }

    /// Borrow the SSH override for a node. Returns a reference to a
    /// shared default-empty override when nothing is set, so callers
    /// don't need to handle `Option`.
    pub fn override_for(&self, name: &str) -> &SshOverride {
        // A `'static` empty override avoids returning a temporary.
        static EMPTY: std::sync::OnceLock<SshOverride> = std::sync::OnceLock::new();
        self.overrides
            .get(name)
            .unwrap_or_else(|| EMPTY.get_or_init(SshOverride::default))
    }

    fn override_mut(&mut self, name: &str) -> &mut SshOverride {
        self.overrides.entry(name.to_string()).or_default()
    }

    pub async fn run(&mut self, terminal: &mut Tui) -> Result<()> {
        let mut events = spawn_events();

        // Kick off an initial reachability sweep so the first frame isn't
        // all "unknown".
        self.refresh_reachability();

        terminal.draw(|f| ui::draw(f, self))?;

        while !self.should_quit {
            tokio::select! {
                biased;

                Some(ev) = events.recv() => {
                    self.handle_event(ev);
                }

                Some(update) = self.status_rx.recv() => {
                    self.apply_status(update);
                }

                Some(line) = recv_optional(&mut self.deploy_rx) => {
                    self.handle_deploy_line(line);
                }
            }

            terminal.draw(|f| ui::draw(f, self))?;
        }

        // Cancel any running deploy when we exit. The child will be reaped
        // by tokio when its handles drop.
        if let Some(t) = self.deploy_task.take() {
            t.abort();
        }

        Ok(())
    }

    // ---------- event handling ----------

    fn handle_event(&mut self, ev: AppEvent) {
        match ev {
            AppEvent::Tick => self.tick_counter = self.tick_counter.wrapping_add(1),
            AppEvent::Term(CtEvent::Key(key)) => self.handle_key(key),
            AppEvent::Term(_) => {}
        }
    }

    fn handle_key(&mut self, key: KeyEvent) {
        if key.kind != KeyEventKind::Press {
            return;
        }
        // Ctrl-C is the universal escape hatch — works in every mode and
        // also kills any running deploy.
        if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('c') {
            self.should_quit = true;
            return;
        }

        // The help popup is modal: only ?/Esc/Enter close it. We handle it
        // here so it short-circuits both Normal and overrides-menu modes.
        if self.show_help {
            match key.code {
                KeyCode::Char('?') | KeyCode::Esc | KeyCode::Enter => self.show_help = false,
                _ => {}
            }
            return;
        }

        // Route by current input mode.
        match std::mem::replace(&mut self.input, InputMode::Normal) {
            InputMode::Normal => {
                self.input = InputMode::Normal;
                self.handle_key_normal(key);
            }
            InputMode::OverridesMenu => self.handle_key_overrides_menu(key),
            InputMode::EditOverride { field, buf } => {
                self.handle_key_edit_override(key, field, buf);
            }
        }
    }

    fn handle_key_normal(&mut self, key: KeyEvent) {
        match key.code {
            KeyCode::Char('q') | KeyCode::Esc => self.should_quit = true,
            KeyCode::Up | KeyCode::Char('k') => self.move_selection(-1),
            KeyCode::Down | KeyCode::Char('j') => self.move_selection(1),
            KeyCode::Tab => {
                self.focus = match self.focus {
                    FocusPane::Hosts => FocusPane::Details,
                    FocusPane::Details => FocusPane::Hosts,
                };
            }
            KeyCode::Char('r') => self.refresh_reachability(),
            KeyCode::Char('u') => self.refresh_updates_for_selected(),

            // Profile selection.
            KeyCode::Char('a') => self.profile_sel = ProfileSel::All,
            KeyCode::Char('n') => self.profile_sel = ProfileSel::System,
            KeyCode::Char('h') => self.profile_sel = ProfileSel::Home,

            // Deploy modes.
            KeyCode::Char('s') => self.start_deploy(Mode::Switch),
            KeyCode::Char('b') => self.start_deploy(Mode::Boot),
            KeyCode::Char('d') => self.start_deploy(Mode::DryRun),
            KeyCode::Char('x') => self.cancel_deploy(),

            // Toggles.
            KeyCode::Char('1') => {
                self.toggles.skip_checks = !self.toggles.skip_checks;
                self.log_toggle("skip-checks", self.toggles.skip_checks);
            }
            KeyCode::Char('2') => {
                self.toggles.magic_rollback = !self.toggles.magic_rollback;
                self.log_toggle("magic-rollback", self.toggles.magic_rollback);
            }
            KeyCode::Char('3') => {
                self.toggles.auto_rollback = !self.toggles.auto_rollback;
                self.log_toggle("auto-rollback", self.toggles.auto_rollback);
            }
            KeyCode::Char('4') => {
                self.toggles.remote_build = !self.toggles.remote_build;
                self.log_toggle("remote-build", self.toggles.remote_build);
            }
            KeyCode::Char('5') => {
                self.toggles.interactive_sudo = !self.toggles.interactive_sudo;
                self.log_toggle("interactive-sudo", self.toggles.interactive_sudo);
                if self.toggles.interactive_sudo {
                    self.push_log(
                        "  ! interactive-sudo will hang the TUI — see ? for details",
                        true,
                    );
                }
            }

            // Overrides menu + help.
            KeyCode::Char('o') => self.input = InputMode::OverridesMenu,
            KeyCode::Char('?') => self.show_help = true,

            _ => {}
        }
    }

    fn handle_key_overrides_menu(&mut self, key: KeyEvent) {
        let Some(node) = self.selected_node().cloned() else {
            return;
        };
        match key.code {
            KeyCode::Esc => {
                self.input = InputMode::Normal;
            }
            KeyCode::Char('h') => self.begin_edit_override(OverrideField::Hostname, &node),
            KeyCode::Char('u') => self.begin_edit_override(OverrideField::User, &node),
            KeyCode::Char('k') => self.begin_edit_override(OverrideField::Identity, &node),
            KeyCode::Char('o') => self.begin_edit_override(OverrideField::Opts, &node),
            KeyCode::Char('c') => {
                self.overrides.remove(&node.name);
                self.push_log(
                    format!("→ cleared SSH overrides for {}", node.name).as_str(),
                    false,
                );
                self.input = InputMode::Normal;
            }
            _ => {
                // Unknown sub-key — stay in the menu so the user can try again.
                self.input = InputMode::OverridesMenu;
            }
        }
    }

    fn begin_edit_override(&mut self, field: OverrideField, node: &Node) {
        // Pre-fill the buffer with the current value so the user can edit
        // rather than retype.
        let current = self.override_for(&node.name);
        let buf = match field {
            OverrideField::Hostname => current.hostname.clone().unwrap_or_default(),
            OverrideField::User => current.user.clone().unwrap_or_default(),
            OverrideField::Identity => current
                .identity
                .as_ref()
                .map(|p| p.display().to_string())
                .unwrap_or_default(),
            OverrideField::Opts => current.extra_opts.clone().unwrap_or_default(),
        };
        self.input = InputMode::EditOverride { field, buf };
    }

    fn handle_key_edit_override(
        &mut self,
        key: KeyEvent,
        field: OverrideField,
        mut buf: String,
    ) {
        match key.code {
            KeyCode::Esc => {
                self.input = InputMode::Normal;
            }
            KeyCode::Enter => {
                let Some(node_name) = self.selected_node().map(|n| n.name.clone()) else {
                    self.input = InputMode::Normal;
                    return;
                };
                let trimmed = buf.trim().to_string();
                let entry = self.override_mut(&node_name);
                let value: Option<String> = if trimmed.is_empty() { None } else { Some(trimmed) };
                match field {
                    OverrideField::Hostname => entry.hostname = value.clone(),
                    OverrideField::User => entry.user = value.clone(),
                    OverrideField::Identity => entry.identity = value.clone().map(PathBuf::from),
                    OverrideField::Opts => entry.extra_opts = value.clone(),
                }
                let active = entry.is_active();
                if !active {
                    // Cleaning every field clears the entry entirely so
                    // the indicator and `override_for` agree.
                    self.overrides.remove(&node_name);
                }
                self.push_log(
                    format!(
                        "→ set {} for {}: {}",
                        field.label(),
                        node_name,
                        value.as_deref().unwrap_or("(cleared)")
                    )
                    .as_str(),
                    false,
                );
                self.input = InputMode::Normal;
            }
            KeyCode::Backspace => {
                buf.pop();
                self.input = InputMode::EditOverride { field, buf };
            }
            KeyCode::Char(c) => {
                buf.push(c);
                self.input = InputMode::EditOverride { field, buf };
            }
            _ => {
                self.input = InputMode::EditOverride { field, buf };
            }
        }
    }

    fn log_toggle(&mut self, name: &str, value: bool) {
        let state = if value { "on" } else { "off" };
        self.push_log(format!("• {name} = {state}").as_str(), false);
    }

    fn move_selection(&mut self, delta: i32) {
        if self.nodes.is_empty() {
            return;
        }
        let len = self.nodes.len() as i32;
        let next = (self.selected as i32 + delta).rem_euclid(len);
        self.selected = next as usize;
    }

    // ---------- background work ----------

    /// Spawn one task per node to TCP-probe port 22.
    fn refresh_reachability(&mut self) {
        for node in &self.nodes {
            let name = node.name.clone();
            let host = node.hostname.clone();
            // Honour the per-node hostname override so the indicator
            // matches what `deploy` will actually dial.
            let override_host = self
                .overrides
                .get(&node.name)
                .and_then(|o| o.hostname.clone());
            let tx = self.status_tx.clone();
            tokio::spawn(async move {
                let r = host::check_online(&host, override_host.as_deref()).await;
                let _ = tx.send(StatusUpdate::Reachability(name, r)).await;
            });
        }
        self.push_log("→ refreshing reachability", false);
    }

    /// Compare local-build vs remote symlink for the selected node's
    /// available profiles.
    fn refresh_updates_for_selected(&mut self) {
        let Some(node) = self.selected_node().cloned() else {
            return;
        };
        // Mark every probe in flight *before* spawning so the UI flips to
        // its spinner state on the very next frame, not just after the
        // first task scheduling round-trip.
        {
            let entry = self.status.entry(node.name.clone()).or_default();
            for profile in node.profiles.keys() {
                match profile.as_str() {
                    "system" => entry.checking_system = true,
                    "home" => entry.checking_home = true,
                    _ => {}
                }
            }
            entry.last_error = None;
        }

        let flake = self.flake.clone();
        let override_ = self.override_for(&node.name).clone();
        for profile in node.profiles.keys() {
            let profile = profile.clone();
            let node = node.clone();
            let flake = flake.clone();
            let override_ = override_.clone();
            let tx = self.status_tx.clone();
            tokio::spawn(async move {
                let result =
                    host::check_profile_up_to_date(&flake, &node, &profile, &override_)
                        .await
                        .map_err(|e| format!("{e:#}"));
                let _ = tx
                    .send(StatusUpdate::UpdateProbe {
                        node: node.name.clone(),
                        profile,
                        result,
                    })
                    .await;
            });
        }
        self.push_log(
            format!("→ checking updates for {}", node.name).as_str(),
            false,
        );
    }

    fn apply_status(&mut self, update: StatusUpdate) {
        match update {
            StatusUpdate::Reachability(name, r) => {
                self.status.entry(name).or_default().reachability = r;
            }
            StatusUpdate::UpdateProbe {
                node,
                profile,
                result,
            } => {
                let entry = self.status.entry(node).or_default();
                let state = match &result {
                    Ok(true) => UpdateState::UpToDate,
                    Ok(false) => UpdateState::NeedsUpdate,
                    Err(e) => {
                        entry.last_error = Some(e.clone());
                        UpdateState::Error
                    }
                };
                match profile.as_str() {
                    "system" => {
                        entry.checking_system = false;
                        entry.system_update = state;
                    }
                    "home" => {
                        entry.checking_home = false;
                        entry.home_update = state;
                    }
                    _ => {}
                }
            }
        }
    }

    fn start_deploy(&mut self, mode: Mode) {
        if self.deploy_task.is_some() {
            self.push_log("! a deploy is already running — press x to cancel", true);
            return;
        }
        let Some(node) = self.selected_node().cloned() else {
            return;
        };
        // Filter out impossible selections (e.g. home-only on a node that
        // has no home profile).
        let profile = match self.profile_sel {
            ProfileSel::Home if !node.has_home() => {
                self.push_log("! selected node has no home profile", true);
                return;
            }
            ProfileSel::System if !node.has_system() => {
                self.push_log("! selected node has no system profile", true);
                return;
            }
            other => other,
        };

        self.mode = mode;
        let req = DeployRequest {
            flake: self.flake.clone(),
            node: node.name.clone(),
            profile,
            mode,
            toggles: self.toggles,
            ssh_override: self.override_for(&node.name).clone(),
        };
        self.push_log(
            format!(
                "→ deploy {} ({}, {})",
                node.name,
                describe_mode(mode),
                describe_profile(profile),
            )
            .as_str(),
            false,
        );
        let handle = deploy::run(req);
        self.deploy_rx = Some(handle.rx);
        self.deploy_task = Some(handle.task);
        self.busy_label = Some(format!("deploying {}", node.name));
    }

    fn cancel_deploy(&mut self) {
        if let Some(t) = self.deploy_task.take() {
            t.abort();
            self.deploy_rx = None;
            self.busy_label = None;
            self.push_log("! deploy cancelled", true);
        }
    }

    fn handle_deploy_line(&mut self, line: LogLine) {
        match line {
            LogLine::Stdout(s) => self.push_log(&s, false),
            LogLine::Stderr(s) => self.push_log(&s, true),
            LogLine::Exit(code) => {
                let ok = code == 0;
                self.push_log(
                    format!("← deploy exited with code {code}").as_str(),
                    !ok,
                );
                self.deploy_task = None;
                self.deploy_rx = None;
                self.busy_label = None;
                if ok {
                    // A successful deploy makes the previous update probe
                    // results stale; clear them so the user knows to re-run
                    // `u` if they care.
                    let name = self.selected_node().map(|n| n.name.clone());
                    if let Some(name) = name {
                        if let Some(s) = self.status.get_mut(&name) {
                            s.system_update = UpdateState::Unknown;
                            s.home_update = UpdateState::Unknown;
                        }
                    }
                }
            }
            LogLine::Error(e) => {
                self.push_log(format!("! deploy spawn failed: {e}").as_str(), true);
                self.deploy_task = None;
                self.deploy_rx = None;
                self.busy_label = None;
            }
        }
    }

    fn push_log(&mut self, text: &str, is_err: bool) {
        self.log.push(LogEntry {
            text: text.to_string(),
            is_err,
        });
        // Cap so we don't grow forever during long sessions.
        const MAX: usize = 2000;
        if self.log.len() > MAX {
            let drop = self.log.len() - MAX;
            self.log.drain(0..drop);
        }
    }
}

/// Receive from an `Option<Receiver<T>>`. Returns `None` (i.e. the branch
/// stays pending) when the option is empty, so `select!` can ignore it.
async fn recv_optional<T>(rx: &mut Option<mpsc::Receiver<T>>) -> Option<T> {
    match rx {
        Some(r) => r.recv().await,
        None => std::future::pending().await,
    }
}

fn describe_mode(mode: Mode) -> &'static str {
    match mode {
        Mode::Switch => "switch",
        Mode::Boot => "boot",
        Mode::DryRun => "dry-run",
    }
}

fn describe_profile(p: ProfileSel) -> &'static str {
    match p {
        ProfileSel::All => "all",
        ProfileSel::System => "system",
        ProfileSel::Home => "home",
    }
}
