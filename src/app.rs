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

use std::collections::{HashMap, VecDeque};
use std::path::PathBuf;

use anyhow::Result;
use crossterm::event::{Event as CtEvent, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use tokio::sync::mpsc;
use tokio::task::JoinHandle;

use crate::deploy::{self, DeployRequest, LogLine, Mode, ProfileSel, Toggles};
use crate::event::{spawn as spawn_events, AppEvent};
use crate::flake::Node;
use crate::host::{self, HostStatus, ProfileCheck, Reachability, UpdateState};
use crate::ssh::SshOverride;
use crate::ui::{self, Tui};

/// Focusable regions of the UI. Each one has its own keyboard
/// affordance when focused: Hosts moves the selection, Details scrolls
/// the log, Toggles lets you flip the deploy-rs flags without hitting
/// 1–5, and Commands exposes every keybind action as a navigable button
/// row. Tab/Shift-Tab cycles forward/back; Shift+H/L also crosses
/// sub-nav boundaries inside Toggles and Commands.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FocusPane {
    Toggles,
    Hosts,
    Details,
    JobLog,
    Commands,
}

impl FocusPane {
    /// Row in the grid layout. 0 = toggles (top), 1 = middle (hosts /
    /// details / job log), 2 = commands (bottom). Used by the vertical
    /// pane-move keys to decide what "up" and "down" mean.
    pub fn row(self) -> usize {
        match self {
            FocusPane::Toggles => 0,
            FocusPane::Hosts | FocusPane::Details | FocusPane::JobLog => 1,
            FocusPane::Commands => 2,
        }
    }
}

/// Number of toggle cells, kept in one place so the nav bounds check
/// stays consistent with the rendering code.
pub const TOGGLE_COUNT: usize = 5;

/// Every action that can be bound to a command-pane button. The pane
/// renders each variant as a short label and `activate_command`
/// dispatches by index. The order is the order the buttons appear in
/// the pane; reordering here is how you rearrange the bottom row.
///
/// Note: `?` (help) is intentionally NOT a command button — it lives
/// in the info pane next to the other meta hints (quit, focus, …) so
/// the commands row stays scoped to "things that act on hosts".
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Command {
    Refresh,
    Updates,
    ProfileAll,
    ProfileSystem,
    ProfileHome,
    Switch,
    Boot,
    DryRun,
    Cancel,
    Override,
}

/// Single source of truth for the command pane — label + key hint per
/// command. The key column is informational (the real binding lives in
/// `handle_key_normal`); if you rename a binding, update both.
pub const COMMANDS: &[(Command, &str, &str)] = &[
    (Command::Refresh, "r", "refresh"),
    (Command::Updates, "u", "updates"),
    (Command::ProfileAll, "a", "all"),
    // `y` (sYs) replaced the original `n` so `/` + `n`/`Shift+N` can
    // own log search the way vim and lazygit users expect. The letter
    // is otherwise unbound; the y/n confirmation popup lives in its
    // own input mode so there's no real collision.
    (Command::ProfileSystem, "y", "sys"),
    (Command::ProfileHome, "h", "home"),
    (Command::Switch, "s", "switch"),
    (Command::Boot, "b", "boot"),
    (Command::DryRun, "d", "dry"),
    (Command::Cancel, "x", "cancel"),
    (Command::Override, "o", "override"),
];

#[derive(Debug, Clone)]
pub struct LogEntry {
    pub text: String,
    pub is_err: bool,
    /// Which host's deploy produced this line, if any. `None` is for
    /// app-level status messages (reachability sweeps, toggle flips,
    /// banner strings, etc.). Used by the batch-log pane to colour-tag
    /// each line with its origin host.
    pub host: Option<String>,
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

/// Which log pane an in-progress `/` search is targeted at. The two
/// log panes (details + job log) maintain independent scroll positions
/// and content filters, so a single global search would land on the
/// wrong line. We pin the target at the moment the user presses `/` and
/// keep using it for `n` / `Shift+N` until they start a new search.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SearchTarget {
    /// The middle-column details log (host-scoped + global messages).
    DetailsLog,
    /// The right-column job log (host-tagged deploy output only).
    JobLog,
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
    /// Picking an SSH identity file. The user can either pick one of the
    /// scanned `entries` with Ctrl+J/K or type a custom path into `buf`.
    /// `entries` may be empty if `~/.ssh` couldn't be read or had no
    /// candidate keys; the buffer is the source of truth on save.
    EditIdentityPicker {
        entries: Vec<PathBuf>,
        selected: usize,
        buf: String,
    },
    /// Confirmation popup for `s`/`b`/`d`. The popup snapshots which
    /// hosts will be hit and how, so the user can review (and bail) on
    /// `n`/`Esc` before any side effects happen.
    ConfirmDeploy {
        hosts: Vec<String>,
        mode: Mode,
        profile: ProfileSel,
    },
    /// User pressed `/` while one of the log panes was focused and is
    /// typing a search query. Enter commits (`App.log_search` set,
    /// jumps to the nearest match), Esc cancels (search cleared).
    /// While in this mode `n`/`Shift+N` are still typed into the buf —
    /// they only become "next match" / "previous match" after Enter.
    SearchLog { target: SearchTarget, buf: String },
    /// User pressed `/` while the help popup was open and is typing a
    /// filter. Lazygit-style: lines that don't contain the buf are
    /// hidden as the user types. Enter commits the filter, Esc clears
    /// it. The popup stays open the whole time.
    SearchHelp { buf: String },
}

/// What we remember about the most recently completed deploy. Rendered
/// in the title bar and the details summary so the user can tell at a
/// glance that a deploy actually finished (instead of staring at a
/// quiet log and wondering whether magic-rollback ate it).
#[derive(Debug, Clone)]
pub struct LastDeploy {
    pub node: String,
    pub mode: Mode,
    pub profile: ProfileSel,
    pub exit_code: i32,
    pub ok: bool,
}

/// Background work updates we receive over the status channel.
#[derive(Debug)]
enum StatusUpdate {
    Reachability(String, Reachability),
    UpdateProbe {
        node: String,
        profile: String,
        result: Result<ProfileCheck, String>,
    },
    /// Closure-size probe result: `(local_bytes, remote_bytes)`. Owned
    /// by the medium-tier update details (`U`).
    SizeProbe {
        node: String,
        profile: String,
        result: Result<(u64, u64), String>,
    },
    /// `nix store diff-closures` output for the expensive-tier check
    /// (`p`). Empty string = closures identical.
    PkgDiffProbe {
        node: String,
        profile: String,
        result: Result<String, String>,
    },
    /// Free-form progress line from a long-running probe (currently
    /// only the package diff). Forwarded into the host-tagged log so
    /// the user sees activity instead of a silent spinner.
    LogLine {
        node: String,
        text: String,
        is_err: bool,
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
    /// Multi-selection for batch deploy. Insertion-ordered so the queue
    /// runs in the order the user clicked them. Empty means "operate on
    /// the highlighted host only" — the existing single-host behaviour.
    pub marked: Vec<String>,
    pub focus: FocusPane,
    /// Cursor inside the toggles pane when focused. `0..TOGGLE_COUNT`.
    /// Stays stable when focus leaves so returning to the pane lands in
    /// the same place the user left it.
    pub toggle_index: usize,
    /// Cursor inside the commands pane when focused. `0..COMMANDS.len()`.
    pub command_index: usize,
    pub mode: Mode,
    pub profile_sel: ProfileSel,
    pub toggles: Toggles,

    pub log: Vec<LogEntry>,
    pub busy_label: Option<String>,
    /// Committed log search query. `Some(q)` means a search has been
    /// committed via Enter from `SearchLog` and `n`/`Shift+N` will jump
    /// between matches. `None` means no search is active and matching
    /// lines aren't highlighted. Cleared by Esc in the prompt or by
    /// committing an empty query.
    pub log_search: Option<String>,
    /// Which pane the committed `log_search` belongs to. The two log
    /// panes share `App.log_search` storage but only the targeted one
    /// renders highlights and responds to `n`/`Shift+N`.
    pub log_search_target: Option<SearchTarget>,
    /// Committed help-popup filter. `Some(q)` hides every help line
    /// that doesn't contain the substring; `None` shows everything.
    /// Lives outside InputMode because the help popup is its own modal
    /// layer that sits *over* the InputMode dispatcher.
    pub help_search: Option<String>,
    /// Most-recent finished deploy across the whole session. Drives the
    /// title-bar chip so the user can tell at a glance what the last
    /// thing they ran was, regardless of which host they're inspecting.
    pub last_deploy: Option<LastDeploy>,
    /// Per-host outcome of the most-recent deploy that touched each
    /// host. Drives the details-pane "last" chip so navigating between
    /// hosts shows the right history per host instead of bleeding the
    /// global last-deploy onto every selection.
    pub last_deploys: HashMap<String, LastDeploy>,
    /// Lines from the bottom of the details/status log the user has
    /// scrolled up. `0` means "auto-tail" (always show the latest line).
    pub log_scroll: usize,
    /// Same contract as `log_scroll` but for the job log pane, which
    /// has its own independent scroll state so the user can focus it
    /// and scroll without disturbing the details log position.
    pub job_log_scroll: usize,
    pub show_help: bool,
    /// Vertical scroll position of the help popup. 0 = top; bumped by
    /// arrow keys / j/k while the popup is open so the help works on
    /// small terminals where the full cheat sheet would overflow.
    pub help_scroll: u16,
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
    /// Background probe tasks (update / closure-size / package-diff
    /// checks). Held so `x` can abort them mid-flight; finished
    /// handles are pruned opportunistically each time we spawn a new
    /// one. The aborted tasks' Commands run with `kill_on_drop(true)`
    /// inside `host.rs` so the underlying nix/ssh children are
    /// reaped, not orphaned.
    probe_tasks: Vec<JoinHandle<()>>,
    /// Pending hosts to deploy after the current one finishes. Populated
    /// when the user kicks off a multi-host deploy. The currently
    /// running host is NOT in this queue (it lives in `current_target`).
    deploy_queue: VecDeque<String>,
    /// Sticky parameters for the in-flight queue so each subsequent
    /// host is deployed with the same mode/profile/toggles the user
    /// originally confirmed.
    queue_mode: Mode,
    queue_profile: ProfileSel,
    /// Total hosts in the run that produced the current queue. Stays
    /// fixed while the queue drains so progress is `done/total`. Reset
    /// to 0 when the queue is empty.
    pub queue_total: usize,
    pub queue_done: usize,
    /// The host currently being deployed (if any). Separate from the
    /// queue so the running host can be displayed independently.
    pub current_target: Option<String>,

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
            marked: Vec::new(),
            focus: FocusPane::Hosts,
            toggle_index: 0,
            command_index: 0,
            mode: Mode::Switch,
            profile_sel: ProfileSel::All,
            toggles: Toggles::default(),
            log: Vec::new(),
            busy_label: None,
            log_search: None,
            log_search_target: None,
            help_search: None,
            last_deploy: None,
            last_deploys: HashMap::new(),
            log_scroll: 0,
            job_log_scroll: 0,
            show_help: false,
            help_scroll: 0,
            input: InputMode::Normal,
            tick_counter: 0,
            status_tx,
            status_rx,
            deploy_rx: None,
            deploy_task: None,
            probe_tasks: Vec::new(),
            deploy_queue: VecDeque::new(),
            queue_mode: Mode::Switch,
            queue_profile: ProfileSel::All,
            queue_total: 0,
            queue_done: 0,
            current_target: None,
            should_quit: false,
        }
    }

    /// True if `name` is in the multi-select set.
    pub fn is_marked(&self, name: &str) -> bool {
        self.marked.iter().any(|n| n == name)
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

        // The help popup is modal: ?/Esc/Enter/q close it, and j/k/arrow
        // keys scroll so the cheat sheet stays usable on small terminals
        // where the full content can't fit in the popup at once.
        //
        // While the help popup is open AND a `SearchHelp` prompt is
        // active we must NOT consume the keystrokes here — they need to
        // reach the InputMode dispatch path so the search-prompt handler
        // can append to the buffer. Same logic applies if a help search
        // has already been committed: `/` would re-open the prompt and
        // typing letters mustn't be eaten by the j/k scroll fall-through.
        if self.show_help && !matches!(self.input, InputMode::SearchHelp { .. }) {
            match key.code {
                // `/` opens the lazygit-style filter prompt. We hand
                // off to the InputMode dispatch by transitioning into
                // SearchHelp here and falling through.
                KeyCode::Char('/') => {
                    self.input = InputMode::SearchHelp { buf: String::new() };
                    return;
                }
                KeyCode::Char('?')
                | KeyCode::Esc
                | KeyCode::Enter
                | KeyCode::Char('q') => {
                    self.show_help = false;
                    // Reset so the next `?` lands at the top again.
                    self.help_scroll = 0;
                    // Closing the popup also drops any committed
                    // help filter so reopening starts clean.
                    self.help_search = None;
                }
                KeyCode::Down | KeyCode::Char('j') => {
                    self.help_scroll = self.help_scroll.saturating_add(1);
                }
                KeyCode::Up | KeyCode::Char('k') => {
                    self.help_scroll = self.help_scroll.saturating_sub(1);
                }
                KeyCode::PageDown => {
                    self.help_scroll = self.help_scroll.saturating_add(5);
                }
                KeyCode::PageUp => {
                    self.help_scroll = self.help_scroll.saturating_sub(5);
                }
                KeyCode::Home => self.help_scroll = 0,
                // Vim-style "g" → top of the popup, "G" → bottom.
                // The renderer clamps `u16::MAX` against the rendered
                // content height in-place.
                KeyCode::Char('g') => self.help_scroll = 0,
                KeyCode::Char('G') => self.help_scroll = u16::MAX,
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
            InputMode::EditIdentityPicker {
                entries,
                selected,
                buf,
            } => {
                self.handle_key_identity_picker(key, entries, selected, buf);
            }
            InputMode::ConfirmDeploy {
                hosts,
                mode,
                profile,
            } => {
                self.handle_key_confirm_deploy(key, hosts, mode, profile);
            }
            InputMode::SearchLog { target, buf } => {
                self.handle_key_search_log(key, target, buf);
            }
            InputMode::SearchHelp { buf } => {
                self.handle_key_search_help(key, buf);
            }
        }
    }

    fn handle_key_normal(&mut self, key: KeyEvent) {
        // Treat "uppercase letter" as shift-held even if the modifier
        // bit isn't set — some terminals report Char('H') without
        // SHIFT, others report Char('h')+SHIFT. Accepting both keeps
        // the bindings consistent regardless of terminal quirks.
        let shift = key.modifiers.contains(KeyModifiers::SHIFT)
            || matches!(key.code, KeyCode::Char(c) if c.is_ascii_uppercase());

        // ---- pane-navigation layer (vim-style) ----
        //
        // Shifted keys always move focus between panes (never within).
        //   horizontal (row 2 only): Shift+H/L, Shift+Left/Right
        //   vertical (between rows): Shift+J/K, Shift+Up/Down
        //
        // h/l mean "left/right" exactly like vim, and j/k mean
        // "down/up". The earlier version swapped these and confused
        // anyone with vim muscle memory.
        if shift {
            match key.code {
                // Horizontal pane move (h = left, l = right).
                KeyCode::Char('H')
                | KeyCode::Char('h')
                | KeyCode::Char('L')
                | KeyCode::Char('l')
                | KeyCode::Left
                | KeyCode::Right => {
                    let left = matches!(
                        key.code,
                        KeyCode::Char('H') | KeyCode::Char('h') | KeyCode::Left
                    );
                    self.pane_move_horizontal(if left { -1 } else { 1 });
                    return;
                }
                // Vertical pane move (j = down, k = up).
                KeyCode::Char('J')
                | KeyCode::Char('j')
                | KeyCode::Char('K')
                | KeyCode::Char('k')
                | KeyCode::Up
                | KeyCode::Down => {
                    let up = matches!(
                        key.code,
                        KeyCode::Char('K') | KeyCode::Char('k') | KeyCode::Up
                    );
                    self.pane_move_vertical(if up { -1 } else { 1 });
                    return;
                }
                // Shift+A / Shift+X: batch mark/unmark. Global.
                KeyCode::Char('A') => {
                    self.mark_all();
                    return;
                }
                KeyCode::Char('X') => {
                    self.clear_marks();
                    return;
                }
                // Shift+U: medium-tier update details (closure size
                // delta). Requires a prior `u` to have populated the
                // cached paths; `refresh_sizes_for_selected` logs a
                // hint if not.
                KeyCode::Char('U') => {
                    self.refresh_sizes_for_selected();
                    return;
                }
                // Shift+G: vim-style "go to end" — snap the focused
                // scroll pane back to its tail (auto-follow). Useful
                // after the user has scrolled up to read history and
                // wants to resume tailing the live log.
                KeyCode::Char('G') => {
                    self.snap_to_tail();
                    return;
                }
                _ => {}
            }
        }

        // ---- global keys (any focus, unshifted) ----
        match key.code {
            KeyCode::Tab => {
                self.focus_next();
                return;
            }
            KeyCode::BackTab => {
                self.focus_prev();
                return;
            }
            KeyCode::Char('?') => {
                self.show_help = true;
                self.help_scroll = 0;
                return;
            }
            KeyCode::Char('q') => {
                self.should_quit = true;
                return;
            }
            // Esc was an accidental quit before — now it just no-ops
            // in Normal mode so a stray escape doesn't kill the TUI.
            // Modal handlers (override / confirm / identity picker)
            // still consume Esc to back out themselves.
            KeyCode::Esc => return,
            // Vim-style "g" → scroll/jump to the top of whatever the
            // focused pane is showing. This used to be a direct-jump
            // to the Hosts pane; it got repurposed because `gg`/`G`
            // for top/bottom is more useful on the log panes and the
            // user reaches Hosts via Tab/Shift+H anyway. `G` snaps
            // to the tail (handled in the shift block above).
            KeyCode::Char('g') => {
                self.jump_to_top();
                return;
            }
            // btop-style direct pane jumps. Picked letters that don't
            // collide with anything else: `f` = focus hosts (the
            // obvious `h` is taken by the home-profile shortcut and
            // `n` is taken by search-next), `i` = inspect details,
            // `v` = view job log, `t` = toggles, `c` = commands.
            KeyCode::Char('f') => {
                self.focus = FocusPane::Hosts;
                return;
            }
            KeyCode::Char('i') => {
                self.focus = FocusPane::Details;
                return;
            }
            KeyCode::Char('v') => {
                self.focus = FocusPane::JobLog;
                return;
            }
            KeyCode::Char('t') => {
                self.focus = FocusPane::Toggles;
                return;
            }
            KeyCode::Char('c') => {
                self.focus = FocusPane::Commands;
                return;
            }
            _ => {}
        }

        // ---- per-pane within-pane actions ----
        //
        // Unshifted arrows + j/k/h/l stay within the focused pane.
        // Toggles and Commands accept h/l as vim-style sub-cursor
        // motion (left/right); the row-2 panes use j/k for scroll
        // but leave h/l alone so they fall through to the global
        // action keys below (e.g. `h` = home profile).
        match self.focus {
            FocusPane::Hosts => match key.code {
                KeyCode::Up | KeyCode::Char('k') => {
                    self.move_selection(-1);
                    return;
                }
                KeyCode::Down | KeyCode::Char('j') => {
                    self.move_selection(1);
                    return;
                }
                KeyCode::Char(' ') => {
                    self.toggle_mark_selected();
                    return;
                }
                _ => {}
            },
            FocusPane::Details => match key.code {
                KeyCode::Up | KeyCode::Char('k') => {
                    self.scroll_log(1);
                    return;
                }
                KeyCode::Down | KeyCode::Char('j') => {
                    self.scroll_log(-1);
                    return;
                }
                // `/` opens the search prompt for the details log.
                // We pin the target here so the eventual `n`/`Shift+N`
                // can't get re-routed if the user navigates away first.
                KeyCode::Char('/') => {
                    self.input = InputMode::SearchLog {
                        target: SearchTarget::DetailsLog,
                        buf: String::new(),
                    };
                    return;
                }
                // Esc clears an active search pinned to this pane so
                // the highlights and chip disappear. Falls through to
                // the global Esc no-op when no search is live.
                KeyCode::Esc
                    if matches!(self.log_search_target, Some(SearchTarget::DetailsLog))
                        && self.log_search.is_some() =>
                {
                    self.clear_log_search();
                    return;
                }
                // `n` / `Shift+N` navigate matches once a search has
                // been committed for *this* pane. Until then they fall
                // through to the global action keys below (where they
                // currently land on nothing useful — `n` was rebound
                // to `y` so search can own it).
                KeyCode::Char('n')
                    if matches!(self.log_search_target, Some(SearchTarget::DetailsLog))
                        && self.log_search.is_some() =>
                {
                    self.search_log_jump(1);
                    return;
                }
                KeyCode::Char('N')
                    if matches!(self.log_search_target, Some(SearchTarget::DetailsLog))
                        && self.log_search.is_some() =>
                {
                    self.search_log_jump(-1);
                    return;
                }
                _ => {}
            },
            FocusPane::JobLog => match key.code {
                KeyCode::Up | KeyCode::Char('k') => {
                    self.scroll_job_log(1);
                    return;
                }
                KeyCode::Down | KeyCode::Char('j') => {
                    self.scroll_job_log(-1);
                    return;
                }
                KeyCode::Char('/') => {
                    self.input = InputMode::SearchLog {
                        target: SearchTarget::JobLog,
                        buf: String::new(),
                    };
                    return;
                }
                // Esc clears an active search pinned to this pane.
                // See the matching arm in FocusPane::Details for why.
                KeyCode::Esc
                    if matches!(self.log_search_target, Some(SearchTarget::JobLog))
                        && self.log_search.is_some() =>
                {
                    self.clear_log_search();
                    return;
                }
                KeyCode::Char('n')
                    if matches!(self.log_search_target, Some(SearchTarget::JobLog))
                        && self.log_search.is_some() =>
                {
                    self.search_job_log_jump(1);
                    return;
                }
                KeyCode::Char('N')
                    if matches!(self.log_search_target, Some(SearchTarget::JobLog))
                        && self.log_search.is_some() =>
                {
                    self.search_job_log_jump(-1);
                    return;
                }
                _ => {}
            },
            FocusPane::Toggles => match key.code {
                KeyCode::Left | KeyCode::Char('h') => {
                    self.move_toggle_index(-1);
                    return;
                }
                KeyCode::Right | KeyCode::Char('l') => {
                    self.move_toggle_index(1);
                    return;
                }
                KeyCode::Enter | KeyCode::Char(' ') => {
                    self.activate_toggle(self.toggle_index);
                    return;
                }
                _ => {}
            },
            FocusPane::Commands => match key.code {
                KeyCode::Left | KeyCode::Char('h') => {
                    self.move_command_index(-1);
                    return;
                }
                KeyCode::Right | KeyCode::Char('l') => {
                    self.move_command_index(1);
                    return;
                }
                KeyCode::Enter | KeyCode::Char(' ') => {
                    self.activate_command(self.command_index);
                    return;
                }
                _ => {}
            },
        }

        // ---- global unshifted action keys ----
        //
        // These fire from any focus. The pane-jump block above has
        // already consumed g/i/v/t/c, and the per-pane block above
        // consumed h/l when Toggles/Commands are focused — so there
        // are no remaining collisions here.
        match key.code {
            KeyCode::Char('r') => self.refresh_reachability(),
            KeyCode::Char('u') => self.refresh_updates_for_selected(),

            // Profile selection — home restored to `h` now that the
            // pane-jump no longer steals it. `y` (sYs) replaced the
            // original `n` so log search can own `n`/Shift+N for
            // next/previous match in vim/lazygit style.
            KeyCode::Char('a') => self.profile_sel = ProfileSel::All,
            KeyCode::Char('y') => self.profile_sel = ProfileSel::System,
            KeyCode::Char('h') => self.profile_sel = ProfileSel::Home,

            // Deploy modes — `d` restored to dry-run for the same
            // reason (details pane is now `i`).
            KeyCode::Char('s') => self.request_deploy(Mode::Switch),
            KeyCode::Char('b') => self.request_deploy(Mode::Boot),
            KeyCode::Char('d') => self.request_deploy(Mode::DryRun),
            KeyCode::Char('x') => self.cancel_deploy(),

            // Expensive-tier update details: full package diff.
            // Behind lowercase `p` to keep the cheap/medium/expensive
            // tiers evaluable independently.
            KeyCode::Char('p') => self.refresh_pkg_diff_for_selected(),

            // Toggles by direct number key.
            KeyCode::Char('1') => self.activate_toggle(0),
            KeyCode::Char('2') => self.activate_toggle(1),
            KeyCode::Char('3') => self.activate_toggle(2),
            KeyCode::Char('4') => self.activate_toggle(3),
            KeyCode::Char('5') => self.activate_toggle(4),

            // Overrides menu.
            KeyCode::Char('o') => self.input = InputMode::OverridesMenu,

            _ => {}
        }
    }

    /// Advance focus in reading order: Toggles → Hosts → Details →
    /// JobLog → Commands → Toggles. Tab uses this; Shift+Tab uses
    /// [`focus_prev`].
    fn focus_next(&mut self) {
        self.focus = match self.focus {
            FocusPane::Toggles => FocusPane::Hosts,
            FocusPane::Hosts => FocusPane::Details,
            FocusPane::Details => FocusPane::JobLog,
            FocusPane::JobLog => FocusPane::Commands,
            FocusPane::Commands => FocusPane::Toggles,
        };
    }

    fn focus_prev(&mut self) {
        self.focus = match self.focus {
            FocusPane::Toggles => FocusPane::Commands,
            FocusPane::Hosts => FocusPane::Toggles,
            FocusPane::Details => FocusPane::Hosts,
            FocusPane::JobLog => FocusPane::Details,
            FocusPane::Commands => FocusPane::JobLog,
        };
    }

    /// Horizontal pane move — only meaningful in the middle row where
    /// Hosts / Details / JobLog sit side by side. From Toggles or
    /// Commands this is a no-op because those panes don't have a
    /// horizontal sibling. Movement is **clamped** at both ends: from
    /// JobLog going right is a no-op (not a wrap to Hosts), and from
    /// Hosts going left is a no-op. The user specifically asked for
    /// this so a stray Shift+L while reading the job log doesn't
    /// teleport them back to the host list. Tab/Shift+Tab still wrap
    /// for cycling.
    fn pane_move_horizontal(&mut self, delta: i32) {
        let order = [FocusPane::Hosts, FocusPane::Details, FocusPane::JobLog];
        let Some(pos) = order.iter().position(|p| *p == self.focus) else {
            // Not in row 2 — nothing to move to.
            return;
        };
        let next = (pos as i32 + delta).clamp(0, order.len() as i32 - 1) as usize;
        self.focus = order[next];
    }

    /// Vertical pane move — jumps between rows: Toggles (row 0) →
    /// middle row (row 1) → Commands (row 2). Clamped at both ends
    /// like `pane_move_horizontal` so pressing Shift+J from Commands
    /// doesn't wrap back to Toggles. When crossing into the middle
    /// row, lands on Hosts by default.
    fn pane_move_vertical(&mut self, delta: i32) {
        let row = self.focus.row() as i32;
        let next_row = (row + delta).clamp(0, 2);
        self.focus = match next_row {
            0 => FocusPane::Toggles,
            1 => FocusPane::Hosts,
            2 => FocusPane::Commands,
            _ => self.focus,
        };
    }

    fn move_toggle_index(&mut self, delta: i32) {
        let len = TOGGLE_COUNT as i32;
        self.toggle_index = ((self.toggle_index as i32 + delta).rem_euclid(len)) as usize;
    }

    fn move_command_index(&mut self, delta: i32) {
        if COMMANDS.is_empty() {
            return;
        }
        let len = COMMANDS.len() as i32;
        self.command_index =
            ((self.command_index as i32 + delta).rem_euclid(len)) as usize;
    }

    /// Flip the toggle at `idx`. `idx` is expected to be `0..TOGGLE_COUNT`
    /// — out-of-range input is ignored so callers don't have to bounds
    /// check themselves. Kept in one place so both direct-number keys
    /// (`1-5`) and Enter-on-focus go through identical logic.
    fn activate_toggle(&mut self, idx: usize) {
        match idx {
            0 => {
                self.toggles.skip_checks = !self.toggles.skip_checks;
                self.log_toggle("skip-checks", self.toggles.skip_checks);
            }
            1 => {
                self.toggles.magic_rollback = !self.toggles.magic_rollback;
                self.log_toggle("magic-rollback", self.toggles.magic_rollback);
            }
            2 => {
                self.toggles.auto_rollback = !self.toggles.auto_rollback;
                self.log_toggle("auto-rollback", self.toggles.auto_rollback);
            }
            3 => {
                self.toggles.remote_build = !self.toggles.remote_build;
                self.log_toggle("remote-build", self.toggles.remote_build);
            }
            4 => {
                self.toggles.interactive_sudo = !self.toggles.interactive_sudo;
                self.log_toggle("interactive-sudo", self.toggles.interactive_sudo);
                if self.toggles.interactive_sudo {
                    self.push_log(
                        "  ! interactive-sudo will hang the TUI — see ? for details",
                        true,
                    );
                }
            }
            _ => {}
        }
    }

    /// Dispatch a command-pane button. This is the single source of
    /// truth for what each command does; the direct-key shortcuts above
    /// call the same underlying helpers.
    fn activate_command(&mut self, idx: usize) {
        let Some((cmd, _, _)) = COMMANDS.get(idx).copied() else {
            return;
        };
        match cmd {
            Command::Refresh => self.refresh_reachability(),
            Command::Updates => self.refresh_updates_for_selected(),
            Command::ProfileAll => self.profile_sel = ProfileSel::All,
            Command::ProfileSystem => self.profile_sel = ProfileSel::System,
            Command::ProfileHome => self.profile_sel = ProfileSel::Home,
            Command::Switch => self.request_deploy(Mode::Switch),
            Command::Boot => self.request_deploy(Mode::Boot),
            Command::DryRun => self.request_deploy(Mode::DryRun),
            Command::Cancel => self.cancel_deploy(),
            Command::Override => self.input = InputMode::OverridesMenu,
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
                self.push_log_tagged(
                    format!("→ cleared SSH overrides for {}", node.name).as_str(),
                    false,
                    Some(node.name.clone()),
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
        // Identity gets a richer modal: scan `~/.ssh` for candidate keys
        // so the user can scroll-and-pick instead of remembering paths.
        // The buf is still authoritative on save, so a typed custom path
        // wins over the highlighted entry.
        if field == OverrideField::Identity {
            let entries = scan_ssh_keys();
            // If the pre-filled buf matches one of the scanned entries,
            // start with that entry highlighted.
            let selected = entries
                .iter()
                .position(|p| p.display().to_string() == buf)
                .unwrap_or(0);
            self.input = InputMode::EditIdentityPicker {
                entries,
                selected,
                buf,
            };
            return;
        }
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
                self.push_log_tagged(
                    format!(
                        "→ set {} for {}: {}",
                        field.label(),
                        node_name,
                        value.as_deref().unwrap_or("(cleared)")
                    )
                    .as_str(),
                    false,
                    Some(node_name.clone()),
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

    fn handle_key_identity_picker(
        &mut self,
        key: KeyEvent,
        entries: Vec<PathBuf>,
        mut selected: usize,
        mut buf: String,
    ) {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        // Ctrl+J / Ctrl+K (and bare Up/Down for ergonomics) navigate the
        // scanned key list. Moving the highlight syncs `buf` so Enter
        // saves the highlighted path with no extra step. Plain typing
        // overrides the buffer freely so a custom path always wins.
        let nav_down =
            (ctrl && matches!(key.code, KeyCode::Char('j') | KeyCode::Char('J')))
                || matches!(key.code, KeyCode::Down);
        let nav_up = (ctrl && matches!(key.code, KeyCode::Char('k') | KeyCode::Char('K')))
            || matches!(key.code, KeyCode::Up);
        if !entries.is_empty() && (nav_down || nav_up) {
            let len = entries.len() as i32;
            let delta: i32 = if nav_down { 1 } else { -1 };
            selected = ((selected as i32 + delta).rem_euclid(len)) as usize;
            buf = entries[selected].display().to_string();
            self.input = InputMode::EditIdentityPicker {
                entries,
                selected,
                buf,
            };
            return;
        }
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
                let value: Option<String> = if trimmed.is_empty() {
                    None
                } else {
                    Some(trimmed)
                };
                let entry = self.override_mut(&node_name);
                entry.identity = value.clone().map(PathBuf::from);
                let active = entry.is_active();
                if !active {
                    self.overrides.remove(&node_name);
                }
                self.push_log_tagged(
                    format!(
                        "→ set identity file for {}: {}",
                        node_name,
                        value.as_deref().unwrap_or("(cleared)")
                    )
                    .as_str(),
                    false,
                    Some(node_name.clone()),
                );
                self.input = InputMode::Normal;
            }
            KeyCode::Backspace => {
                buf.pop();
                self.input = InputMode::EditIdentityPicker {
                    entries,
                    selected,
                    buf,
                };
            }
            KeyCode::Char(c) if !ctrl => {
                buf.push(c);
                self.input = InputMode::EditIdentityPicker {
                    entries,
                    selected,
                    buf,
                };
            }
            _ => {
                self.input = InputMode::EditIdentityPicker {
                    entries,
                    selected,
                    buf,
                };
            }
        }
    }

    fn handle_key_confirm_deploy(
        &mut self,
        key: KeyEvent,
        hosts: Vec<String>,
        mode: Mode,
        profile: ProfileSel,
    ) {
        match key.code {
            KeyCode::Char('y') | KeyCode::Char('Y') | KeyCode::Enter => {
                self.input = InputMode::Normal;
                self.run_confirmed(hosts, mode, profile);
            }
            KeyCode::Char('n')
            | KeyCode::Char('N')
            | KeyCode::Esc
            | KeyCode::Char('q') => {
                self.input = InputMode::Normal;
                self.push_log("• deploy cancelled at confirmation", false);
            }
            _ => {
                // Re-arm the modal so unrelated keystrokes don't dismiss
                // it accidentally — only y/n/Enter/Esc resolve.
                self.input = InputMode::ConfirmDeploy {
                    hosts,
                    mode,
                    profile,
                };
            }
        }
    }

    /// Handle a keystroke while the user is typing a `/` search query
    /// for one of the log panes. Enter commits, Esc cancels (clearing
    /// any prior committed search), Backspace edits, every other
    /// printable char appends to the buffer.
    fn handle_key_search_log(
        &mut self,
        key: KeyEvent,
        target: SearchTarget,
        mut buf: String,
    ) {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        match key.code {
            KeyCode::Esc => {
                // Cancel: drop the buffer AND any previously-committed
                // query so the highlights vanish. Most-explicit way
                // for the user to "turn search off entirely".
                self.input = InputMode::Normal;
                self.log_search = None;
                self.log_search_target = None;
            }
            KeyCode::Enter => {
                self.input = InputMode::Normal;
                let trimmed = buf.trim().to_string();
                if trimmed.is_empty() {
                    // Committing empty == clearing.
                    self.log_search = None;
                    self.log_search_target = None;
                    return;
                }
                self.log_search = Some(trimmed);
                self.log_search_target = Some(target);
                // Jump to the first match nearest the tail (newest).
                // `0` here means "stay at current position then walk
                // toward newer entries until a hit", which is the
                // friendlier default than dumping the cursor at line 1.
                match target {
                    SearchTarget::DetailsLog => {
                        self.log_scroll = 0;
                        self.search_log_jump_initial();
                    }
                    SearchTarget::JobLog => {
                        self.job_log_scroll = 0;
                        self.search_job_log_jump_initial();
                    }
                }
            }
            KeyCode::Backspace => {
                buf.pop();
                self.input = InputMode::SearchLog { target, buf };
            }
            KeyCode::Char(c) if !ctrl => {
                buf.push(c);
                self.input = InputMode::SearchLog { target, buf };
            }
            _ => {
                self.input = InputMode::SearchLog { target, buf };
            }
        }
    }

    /// Same contract as [`handle_key_search_log`] but for the help
    /// popup filter. Lazygit-style: every keystroke updates the live
    /// filter, Enter commits (drops the typing UI but keeps the
    /// filter), Esc clears.
    fn handle_key_search_help(&mut self, key: KeyEvent, mut buf: String) {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        match key.code {
            KeyCode::Esc => {
                self.input = InputMode::Normal;
                self.help_search = None;
            }
            KeyCode::Enter => {
                self.input = InputMode::Normal;
                let trimmed = buf.trim().to_string();
                self.help_search = if trimmed.is_empty() {
                    None
                } else {
                    Some(trimmed)
                };
            }
            KeyCode::Backspace => {
                buf.pop();
                self.help_search = if buf.is_empty() {
                    None
                } else {
                    Some(buf.clone())
                };
                self.input = InputMode::SearchHelp { buf };
            }
            KeyCode::Char(c) if !ctrl => {
                buf.push(c);
                // Live filter — every keystroke updates the visible
                // line set so the user sees results as they type.
                self.help_search = Some(buf.clone());
                self.input = InputMode::SearchHelp { buf };
            }
            _ => {
                self.input = InputMode::SearchHelp { buf };
            }
        }
    }

    /// Snap the details log scroll to the next/previous entry whose
    /// text contains the active search query. `direction = 1` advances
    /// toward newer lines (smaller scroll value, closer to the tail);
    /// `direction = -1` walks toward older lines (larger scroll). The
    /// log buffer order is oldest→newest so newer = higher index.
    fn search_log_jump(&mut self, direction: i32) {
        let Some(query) = self.log_search.clone() else {
            return;
        };
        let filtered = self.filtered_log_indices_for_details();
        if filtered.is_empty() {
            return;
        }
        // Convert the current scroll into a "cursor index" inside the
        // filtered slice. scroll == 0 means the cursor sits at the
        // last entry; scroll == filtered.len() - 1 sits at the first.
        let cursor = filtered
            .len()
            .saturating_sub(1)
            .saturating_sub(self.log_scroll);
        if let Some(next) = next_match(&filtered, &self.log, &query, cursor, direction) {
            self.log_scroll = filtered.len().saturating_sub(1).saturating_sub(next);
        }
    }

    fn search_job_log_jump(&mut self, direction: i32) {
        let Some(query) = self.log_search.clone() else {
            return;
        };
        let filtered = self.filtered_log_indices_for_job_log();
        if filtered.is_empty() {
            return;
        }
        let cursor = filtered
            .len()
            .saturating_sub(1)
            .saturating_sub(self.job_log_scroll);
        if let Some(next) = next_match(&filtered, &self.log, &query, cursor, direction) {
            self.job_log_scroll = filtered.len().saturating_sub(1).saturating_sub(next);
        }
    }

    /// First-jump variant: like `search_log_jump(-1)` but starts the
    /// walk at the very last filtered entry (the tail) instead of
    /// requiring the user to be already-near a match. Used right after
    /// commit so the cursor lands on something visible.
    fn search_log_jump_initial(&mut self) {
        let Some(query) = self.log_search.clone() else {
            return;
        };
        let filtered = self.filtered_log_indices_for_details();
        if filtered.is_empty() {
            return;
        }
        // Walk from tail (newest) backwards looking for the first hit.
        let last = filtered.len() - 1;
        for i in (0..=last).rev() {
            if self.log[filtered[i]].text.contains(&query) {
                self.log_scroll = last - i;
                return;
            }
        }
    }

    fn search_job_log_jump_initial(&mut self) {
        let Some(query) = self.log_search.clone() else {
            return;
        };
        let filtered = self.filtered_log_indices_for_job_log();
        if filtered.is_empty() {
            return;
        }
        let last = filtered.len() - 1;
        for i in (0..=last).rev() {
            if self.log[filtered[i]].text.contains(&query) {
                self.job_log_scroll = last - i;
                return;
            }
        }
    }

    /// Drop the committed log search. Leaves the scroll positions
    /// alone so the user stays where they were when they pressed Esc.
    fn clear_log_search(&mut self) {
        self.log_search = None;
        self.log_search_target = None;
    }

    /// Return `(current, total)` match counts for a committed log
    /// search on `target`. `current` is the 1-based index of the
    /// last match at or before the pane's current cursor position;
    /// `0` means the cursor sits above the first match (no match
    /// behind the view yet). `total` is the full count across the
    /// pane's filtered view. Returns `(0, 0)` when no search is
    /// active for `target`.
    pub fn log_search_stats(&self, target: SearchTarget) -> (usize, usize) {
        let Some(query) = self.log_search.as_ref() else {
            return (0, 0);
        };
        if self.log_search_target != Some(target) {
            return (0, 0);
        }
        let (filtered, scroll) = match target {
            SearchTarget::DetailsLog => {
                (self.filtered_log_indices_for_details(), self.log_scroll)
            }
            SearchTarget::JobLog => {
                (self.filtered_log_indices_for_job_log(), self.job_log_scroll)
            }
        };
        if filtered.is_empty() {
            return (0, 0);
        }
        let cursor = filtered.len().saturating_sub(1).saturating_sub(scroll);
        let mut total = 0usize;
        let mut current = 0usize;
        for (i, &idx) in filtered.iter().enumerate() {
            if self.log[idx].text.contains(query) {
                total += 1;
                if i <= cursor {
                    current = total;
                }
            }
        }
        (current, total)
    }

    /// Indices into `self.log` that the details pane currently shows
    /// for the highlighted host. Mirrors the filter inside `draw_log`
    /// in `ui.rs` so search and rendering agree.
    fn filtered_log_indices_for_details(&self) -> Vec<usize> {
        let selected = self.selected_node().map(|n| n.name.as_str());
        self.log
            .iter()
            .enumerate()
            .filter_map(|(i, e)| match (e.host.as_deref(), selected) {
                (None, _) => Some(i),
                (Some(h), Some(sel)) if h == sel => Some(i),
                _ => None,
            })
            .collect()
    }

    /// Indices into `self.log` that the job-log pane currently shows.
    /// Mirrors the filter inside `draw_job_log` in `ui.rs`.
    fn filtered_log_indices_for_job_log(&self) -> Vec<usize> {
        self.log
            .iter()
            .enumerate()
            .filter_map(|(i, e)| e.host.as_ref().map(|_| i))
            .collect()
    }

    fn log_toggle(&mut self, name: &str, value: bool) {
        let state = if value { "on" } else { "off" };
        self.push_log(format!("• {name} = {state}").as_str(), false);
    }

    fn toggle_mark_selected(&mut self) {
        let Some(name) = self.selected_node().map(|n| n.name.clone()) else {
            return;
        };
        if let Some(idx) = self.marked.iter().position(|n| n == &name) {
            self.marked.remove(idx);
            self.push_log_tagged(
                format!("• unmarked {name}").as_str(),
                false,
                Some(name.clone()),
            );
        } else {
            self.marked.push(name.clone());
            self.push_log_tagged(
                format!("• marked {name}").as_str(),
                false,
                Some(name.clone()),
            );
        }
    }

    fn mark_all(&mut self) {
        self.marked = self.nodes.iter().map(|n| n.name.clone()).collect();
        self.push_log(
            format!("• marked all ({})", self.marked.len()).as_str(),
            false,
        );
    }

    fn clear_marks(&mut self) {
        if self.marked.is_empty() {
            return;
        }
        let n = self.marked.len();
        self.marked.clear();
        self.push_log(format!("• cleared {n} marked").as_str(), false);
    }

    fn move_selection(&mut self, delta: i32) {
        if self.nodes.is_empty() {
            return;
        }
        let len = self.nodes.len() as i32;
        let next = (self.selected as i32 + delta).rem_euclid(len);
        self.selected = next as usize;
    }

    /// Scroll the log buffer. Positive `delta` scrolls UP (older lines),
    /// negative scrolls DOWN (towards the tail). `log_scroll == 0` is
    /// auto-tail. We saturate at the bottom; the upper bound is enforced
    /// by `draw_log` against the actual rendered height so the scroll
    /// position can never reveal a blank pane.
    fn scroll_log(&mut self, delta: i32) {
        let cur = self.log_scroll as i32;
        let next = (cur + delta).max(0) as usize;
        // Hard cap so we don't scroll past the start of the buffer.
        // The render layer also clamps against the visible height.
        self.log_scroll = next.min(self.log.len().saturating_sub(1));
    }

    /// Same contract as [`scroll_log`] but for the job log pane. The
    /// job log only shows tagged (deploy-originated) entries, so we
    /// clamp against that filtered count.
    fn scroll_job_log(&mut self, delta: i32) {
        let cur = self.job_log_scroll as i32;
        let next = (cur + delta).max(0) as usize;
        let tagged = self.log.iter().filter(|e| e.host.is_some()).count();
        self.job_log_scroll = next.min(tagged.saturating_sub(1));
    }

    /// Vim-style "gg": jump to the top of whatever the focused pane
    /// is showing. For scroll panes "top" means the oldest line in
    /// the buffer (i.e. the maximum scroll-back offset); the renderer
    /// clamps the value against the real buffer length so over-
    /// shooting here is fine. For list panes it moves the cursor to
    /// the first entry. Every focus variant is handled explicitly so
    /// a new pane can't silently skip "g".
    fn jump_to_top(&mut self) {
        match self.focus {
            FocusPane::Hosts => {
                if !self.nodes.is_empty() {
                    self.selected = 0;
                }
            }
            FocusPane::Details => {
                // Hard cap is `log.len() - 1`; renderer clamps against
                // the visible height on top of that.
                self.log_scroll = self.log.len().saturating_sub(1);
            }
            FocusPane::JobLog => {
                let tagged = self.log.iter().filter(|e| e.host.is_some()).count();
                self.job_log_scroll = tagged.saturating_sub(1);
            }
            FocusPane::Toggles => self.toggle_index = 0,
            FocusPane::Commands => self.command_index = 0,
        }
    }

    /// Snap whichever scroll pane currently has focus back to its
    /// tail (offset 0). The Details and Job Log panes both maintain
    /// their own offset; outside those panes this is a no-op.
    fn snap_to_tail(&mut self) {
        match self.focus {
            FocusPane::Details => self.log_scroll = 0,
            FocusPane::JobLog => self.job_log_scroll = 0,
            _ => {}
        }
    }

    // ---------- background work ----------

    /// Spawn one task per node to probe reachability. Each task runs
    /// `ssh -G` (cheap, no connection) to resolve the node the way the
    /// real `ssh` would, then TCP-probes the resolved host:port. This
    /// makes the online badge honour `~/.ssh/config` (ProxyJump aside —
    /// we're still doing a direct TCP dial).
    fn refresh_reachability(&mut self) {
        // Flip every host into the "checking" state before spawning so
        // the UI shows the spinner on the very next frame instead of
        // waiting for the first TCP probe to return.
        for node in &self.nodes {
            self.status
                .entry(node.name.clone())
                .or_default()
                .checking_reachability = true;
        }
        // Snapshot the (name, hostname) pairs before the spawn loop
        // so the closure-capturing iteration doesn't hold an
        // immutable borrow of `self.nodes` while we mutably reborrow
        // `self.probe_tasks` via `track_probe` inside the body.
        let targets: Vec<(String, String)> = self
            .nodes
            .iter()
            .map(|n| (n.name.clone(), n.hostname.clone()))
            .collect();
        for (name, host) in targets {
            // Pass the full override so `ssh -G` sees any `-i`/`-o`
            // args the user set, not just the raw hostname.
            let override_ = self.override_for(&name).clone();
            let tx = self.status_tx.clone();
            let task_name = name.clone();
            let handle = tokio::spawn(async move {
                let r = host::check_online(&host, &override_).await;
                let _ = tx.send(StatusUpdate::Reachability(task_name, r)).await;
            });
            self.track_probe(handle);
        }
        self.push_log("→ refreshing reachability", false);
    }

    /// Park a freshly-spawned probe task so it can be aborted later
    /// and prune any handles that have already finished. Called
    /// every time we start a new background probe (reachability,
    /// update, size, package diff).
    fn track_probe(&mut self, handle: JoinHandle<()>) {
        self.probe_tasks.retain(|h| !h.is_finished());
        self.probe_tasks.push(handle);
    }

    /// Compare local-build vs remote symlink for the selected node's
    /// available profiles. Always populates the cheap-tier details
    /// (paths + activation time) as a byproduct; medium/expensive
    /// tiers live behind `U` and `p`.
    /// Resolve which hosts a per-host command (updates / sizes / pkg
    /// diff) should target. Mirrors the "marked wins over cursor"
    /// semantics that `request_deploy` uses so all per-host actions
    /// behave consistently: mark multiple and one keypress hits all
    /// of them.
    fn target_nodes(&self) -> Vec<Node> {
        if self.marked.is_empty() {
            self.selected_node().cloned().into_iter().collect()
        } else {
            self.marked
                .iter()
                .filter_map(|name| self.nodes.iter().find(|n| &n.name == name).cloned())
                .collect()
        }
    }

    fn refresh_updates_for_selected(&mut self) {
        let targets = self.target_nodes();
        if targets.is_empty() {
            return;
        }
        for node in targets {
            self.refresh_updates_for_node(&node);
        }
    }

    fn refresh_updates_for_node(&mut self, node: &Node) {
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
            let handle = tokio::spawn(async move {
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
            self.track_probe(handle);
        }
        self.push_log_tagged(
            format!("→ checking updates for {}", node.name).as_str(),
            false,
            Some(node.name.clone()),
        );
    }

    /// Medium-tier update details: closure size delta for each of the
    /// selected host's profiles. Requires a prior `u` so we have the
    /// local/remote store paths to compare — if they're missing we
    /// log a hint and skip.
    fn refresh_sizes_for_selected(&mut self) {
        let targets = self.target_nodes();
        if targets.is_empty() {
            return;
        }
        for node in targets {
            self.refresh_sizes_for_node(&node);
        }
    }

    fn refresh_sizes_for_node(&mut self, node: &Node) {
        let mut launched = 0usize;
        let status = self.status.entry(node.name.clone()).or_default();
        let profiles: Vec<(String, Option<String>, Option<String>)> = node
            .profiles
            .keys()
            .map(|p| {
                let extra = match p.as_str() {
                    "system" => &status.system_extra,
                    "home" => &status.home_extra,
                    _ => return (p.clone(), None, None),
                };
                (p.clone(), extra.local_path.clone(), extra.remote_path.clone())
            })
            .collect();
        for (profile, local, remote) in profiles {
            let (Some(local_path), Some(remote_path)) = (local, remote) else {
                continue;
            };
            // Flag "in flight" on the extras so the UI can spin.
            let entry = self.status.entry(node.name.clone()).or_default();
            match profile.as_str() {
                "system" => entry.system_extra.checking_size = true,
                "home" => entry.home_extra.checking_size = true,
                _ => {}
            }
            let node_cloned = node.clone();
            let override_ = self.override_for(&node.name).clone();
            let tx = self.status_tx.clone();
            let profile_cloned = profile.clone();
            let flake_cloned = self.flake.clone();
            let handle = tokio::spawn(async move {
                // Same forwarder pattern as refresh_pkg_diff_for_selected:
                // host::check_closure_sizes emits free-form progress
                // strings (especially when it has to build the local
                // closure, which can take tens of seconds). We convert
                // each one to a LogLine tagged with the node name so
                // it lands in that host's details log alongside the
                // spinner.
                let (prog_tx, mut prog_rx) =
                    mpsc::channel::<String>(64);
                let forwarder_tx = tx.clone();
                let forwarder_node = node_cloned.name.clone();
                let forwarder = tokio::spawn(async move {
                    while let Some(line) = prog_rx.recv().await {
                        let _ = forwarder_tx
                            .send(StatusUpdate::LogLine {
                                node: forwarder_node.clone(),
                                text: line,
                                is_err: false,
                            })
                            .await;
                    }
                });
                let result = host::check_closure_sizes(
                    &flake_cloned,
                    &node_cloned,
                    &profile_cloned,
                    &local_path,
                    &remote_path,
                    &override_,
                    prog_tx,
                )
                .await
                .map_err(|e| format!("{e:#}"));
                // Drain the forwarder before publishing the final probe
                // result so the closing "[size] remote: …" line lands
                // before the inline sizes snap into place.
                let _ = forwarder.await;
                let _ = tx
                    .send(StatusUpdate::SizeProbe {
                        node: node_cloned.name.clone(),
                        profile: profile_cloned,
                        result,
                    })
                    .await;
            });
            self.track_probe(handle);
            launched += 1;
        }
        if launched == 0 {
            self.push_log_tagged(
                format!("! no cached paths for {} — press u first", node.name).as_str(),
                true,
                Some(node.name.clone()),
            );
        } else {
            self.push_log_tagged(
                format!("→ checking closure sizes for {}", node.name).as_str(),
                false,
                Some(node.name.clone()),
            );
        }
    }

    /// Expensive-tier update details: full `nix store diff-closures`
    /// output. Same cached-paths precondition as `refresh_sizes_for_selected`.
    fn refresh_pkg_diff_for_selected(&mut self) {
        let targets = self.target_nodes();
        if targets.is_empty() {
            return;
        }
        for node in targets {
            self.refresh_pkg_diff_for_node(&node);
        }
    }

    fn refresh_pkg_diff_for_node(&mut self, node: &Node) {
        let status = self.status.entry(node.name.clone()).or_default();
        let profiles: Vec<(String, Option<String>, Option<String>)> = node
            .profiles
            .keys()
            .map(|p| {
                let extra = match p.as_str() {
                    "system" => &status.system_extra,
                    "home" => &status.home_extra,
                    _ => return (p.clone(), None, None),
                };
                (p.clone(), extra.local_path.clone(), extra.remote_path.clone())
            })
            .collect();
        let mut launched = 0usize;
        for (profile, local, remote) in profiles {
            let (Some(local_path), Some(remote_path)) = (local, remote) else {
                continue;
            };
            let entry = self.status.entry(node.name.clone()).or_default();
            match profile.as_str() {
                "system" => entry.system_extra.checking_pkg = true,
                "home" => entry.home_extra.checking_pkg = true,
                _ => {}
            }
            let tx = self.status_tx.clone();
            let node_cloned = node.clone();
            let profile_cloned = profile.clone();
            let override_ = self.override_for(&node.name).clone();
            let flake_cloned = self.flake.clone();
            let handle = tokio::spawn(async move {
                // Bridge: host::check_package_diff emits free-form
                // progress strings; we forward each one as a LogLine
                // status update tagged with the node name so it lands
                // in the host's details log alongside the spinner.
                // The bridge task ends naturally when the sender side
                // is dropped at function exit.
                let (prog_tx, mut prog_rx) =
                    mpsc::channel::<String>(64);
                let forwarder_tx = tx.clone();
                let forwarder_node = node_cloned.name.clone();
                let forwarder = tokio::spawn(async move {
                    while let Some(line) = prog_rx.recv().await {
                        let _ = forwarder_tx
                            .send(StatusUpdate::LogLine {
                                node: forwarder_node.clone(),
                                text: line,
                                is_err: false,
                            })
                            .await;
                    }
                });

                let result = host::check_package_diff(
                    &flake_cloned,
                    &node_cloned,
                    &profile_cloned,
                    &local_path,
                    &remote_path,
                    &override_,
                    prog_tx,
                )
                .await
                .map_err(|e| format!("{e:#}"));
                // Make sure the forwarder drains anything still in
                // flight before we publish the final probe result, so
                // the user sees the closing "[pkg] done" line before
                // the inline diff snaps into place.
                let _ = forwarder.await;
                let _ = tx
                    .send(StatusUpdate::PkgDiffProbe {
                        node: node_cloned.name.clone(),
                        profile: profile_cloned,
                        result,
                    })
                    .await;
            });
            self.track_probe(handle);
            launched += 1;
        }
        if launched == 0 {
            self.push_log_tagged(
                format!("! no cached paths for {} — press u first", node.name).as_str(),
                true,
                Some(node.name.clone()),
            );
        } else {
            self.push_log_tagged(
                format!("→ computing package diff for {}", node.name).as_str(),
                false,
                Some(node.name.clone()),
            );
        }
    }

    fn apply_status(&mut self, update: StatusUpdate) {
        match update {
            StatusUpdate::Reachability(name, r) => {
                let entry = self.status.entry(name).or_default();
                entry.reachability = r;
                entry.checking_reachability = false;
                // Stamp the "last seen up" time on every successful
                // probe so the details pane can show something freshly
                // anchored ("up 3s ago") rather than the stale label
                // from whatever the previous sweep found.
                if r == Reachability::Online {
                    entry.last_online = Some(std::time::SystemTime::now());
                }
            }
            StatusUpdate::UpdateProbe {
                node,
                profile,
                result,
            } => {
                let entry = self.status.entry(node).or_default();
                let state = match &result {
                    Ok(c) if c.up_to_date => UpdateState::UpToDate,
                    Ok(_) => UpdateState::NeedsUpdate,
                    Err(e) => {
                        entry.last_error = Some(e.clone());
                        UpdateState::Error
                    }
                };
                // Cache the cheap details (paths + activation time) on
                // the per-profile extras so the details pane can render
                // them without any extra work. An error clears the old
                // cached values so we never show stale paths alongside
                // a failed probe.
                let extra = match profile.as_str() {
                    "system" => Some(&mut entry.system_extra),
                    "home" => Some(&mut entry.home_extra),
                    _ => None,
                };
                if let Some(ex) = extra {
                    match &result {
                        Ok(c) => {
                            ex.local_path = Some(c.local_path.clone());
                            ex.remote_path = Some(c.remote_path.clone());
                            ex.activation_time = c.activation_time;
                        }
                        Err(_) => {
                            ex.local_path = None;
                            ex.remote_path = None;
                            ex.activation_time = None;
                            // The medium/expensive results are scoped
                            // to the paths we just invalidated — drop
                            // them so a later `U`/`p` doesn't render
                            // garbage for the wrong closure.
                            ex.local_size = None;
                            ex.remote_size = None;
                            ex.pkg_diff = None;
                        }
                    }
                }
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
            StatusUpdate::SizeProbe {
                node,
                profile,
                result,
            } => {
                let entry = self.status.entry(node).or_default();
                let extra = match profile.as_str() {
                    "system" => Some(&mut entry.system_extra),
                    "home" => Some(&mut entry.home_extra),
                    _ => None,
                };
                if let Some(ex) = extra {
                    ex.checking_size = false;
                    match result {
                        Ok((local, remote)) => {
                            ex.local_size = Some(local);
                            ex.remote_size = Some(remote);
                        }
                        Err(e) => {
                            ex.local_size = None;
                            ex.remote_size = None;
                            entry.last_error = Some(e);
                        }
                    }
                }
            }
            StatusUpdate::PkgDiffProbe {
                node,
                profile,
                result,
            } => {
                let entry = self.status.entry(node).or_default();
                let extra = match profile.as_str() {
                    "system" => Some(&mut entry.system_extra),
                    "home" => Some(&mut entry.home_extra),
                    _ => None,
                };
                if let Some(ex) = extra {
                    ex.checking_pkg = false;
                    match result {
                        Ok(diff) => ex.pkg_diff = Some(diff),
                        Err(e) => {
                            ex.pkg_diff = None;
                            entry.last_error = Some(e);
                        }
                    }
                }
            }
            StatusUpdate::LogLine {
                node,
                text,
                is_err,
            } => {
                self.push_log_tagged(&text, is_err, Some(node));
            }
        }
    }

    /// Build the candidate target list for `mode` and open the
    /// confirmation popup. Marked hosts win over the cursor selection,
    /// because that's the more deliberate action: if the user took the
    /// trouble to mark, that's what they want.
    fn request_deploy(&mut self, mode: Mode) {
        if self.deploy_task.is_some() {
            self.push_log(
                "! a deploy is already running — press x to cancel",
                true,
            );
            return;
        }
        let hosts: Vec<String> = if self.marked.is_empty() {
            match self.selected_node().map(|n| n.name.clone()) {
                Some(name) => vec![name],
                None => {
                    self.push_log("! no host selected", true);
                    return;
                }
            }
        } else {
            self.marked.clone()
        };
        if hosts.is_empty() {
            self.push_log("! no hosts to deploy", true);
            return;
        }
        // Open the modal — actual side effects happen when the user
        // presses `y`.
        self.input = InputMode::ConfirmDeploy {
            hosts,
            mode,
            profile: self.profile_sel,
        };
    }

    /// Confirmed by the user. Stash the queue and kick off the first
    /// deploy. The remaining hosts are run sequentially as each child
    /// exits cleanly (see `handle_deploy_line`).
    fn run_confirmed(&mut self, hosts: Vec<String>, mode: Mode, profile: ProfileSel) {
        self.mode = mode;
        self.queue_mode = mode;
        self.queue_profile = profile;
        self.queue_total = hosts.len();
        self.queue_done = 0;
        self.deploy_queue = hosts.into_iter().collect();
        // Fresh run wipes the previous outcome and snaps both logs to
        // auto-tail so the user sees the new output in the details
        // pane and the job log pane simultaneously.
        self.last_deploy = None;
        self.log_scroll = 0;
        self.job_log_scroll = 0;
        self.start_next_in_queue();
    }

    /// Pop the next host from `deploy_queue` and spawn the deploy. Skips
    /// hosts that lack the requested profile (logs a warning) so a single
    /// bad target doesn't poison the whole batch.
    fn start_next_in_queue(&mut self) {
        // Drain hosts that turn out to be impossible up front so the
        // queue progress stays consistent (the user-visible total still
        // includes them — they're just counted as "done" with a skip).
        while let Some(name) = self.deploy_queue.pop_front() {
            let Some(node) = self.nodes.iter().find(|n| n.name == name).cloned() else {
                self.push_log_tagged(
                    format!("! unknown host {name} — skipped").as_str(),
                    true,
                    Some(name.clone()),
                );
                self.queue_done = self.queue_done.saturating_add(1);
                continue;
            };
            let profile = match self.queue_profile {
                ProfileSel::Home if !node.has_home() => {
                    self.push_log_tagged(
                        format!("! {name} has no home profile — skipped").as_str(),
                        true,
                        Some(name.clone()),
                    );
                    self.queue_done = self.queue_done.saturating_add(1);
                    continue;
                }
                ProfileSel::System if !node.has_system() => {
                    self.push_log_tagged(
                        format!("! {name} has no system profile — skipped").as_str(),
                        true,
                        Some(name.clone()),
                    );
                    self.queue_done = self.queue_done.saturating_add(1);
                    continue;
                }
                other => other,
            };
            let req = DeployRequest {
                flake: self.flake.clone(),
                node: node.name.clone(),
                profile,
                mode: self.queue_mode,
                toggles: self.toggles,
                ssh_override: self.override_for(&node.name).clone(),
            };
            self.push_log_tagged(
                format!(
                    "→ deploy [{}/{}] {} ({}, {})",
                    self.queue_done + 1,
                    self.queue_total,
                    node.name,
                    describe_mode(self.queue_mode),
                    describe_profile(profile),
                )
                .as_str(),
                false,
                Some(node.name.clone()),
            );
            let handle = deploy::run(req);
            self.deploy_rx = Some(handle.rx);
            self.deploy_task = Some(handle.task);
            self.busy_label = if self.queue_total > 1 {
                Some(format!(
                    "deploying [{}/{}] {}",
                    self.queue_done + 1,
                    self.queue_total,
                    node.name
                ))
            } else {
                Some(format!("deploying {}", node.name))
            };
            self.current_target = Some(node.name);
            return;
        }
        // Queue drained without spawning anything (every host was a skip).
        self.queue_total = 0;
        self.queue_done = 0;
        self.current_target = None;
    }

    fn cancel_deploy(&mut self) {
        // First: cancel any in-flight probe tasks. This is what makes
        // `x` actually stop a long-running package check (the most
        // common reason the user reaches for cancel when no deploy is
        // running). The Commands inside `host.rs` set
        // `kill_on_drop(true)`, so aborting the awaiting future also
        // reaps the underlying nix-store / ssh children instead of
        // orphaning them.
        let probes_aborted = self.cancel_probes();

        if let Some(t) = self.deploy_task.take() {
            t.abort();
            self.deploy_rx = None;
            self.busy_label = None;
            // Cancelling kills the queue too — otherwise pressing `x`
            // mid-batch would surprise-deploy the next host.
            let drained = self.deploy_queue.len();
            self.deploy_queue.clear();
            let target = self.current_target.clone();
            if drained > 0 {
                self.push_log_tagged(
                    format!("! deploy cancelled — dropped {drained} queued host(s)")
                        .as_str(),
                    true,
                    target.clone(),
                );
            } else {
                self.push_log_tagged("! deploy cancelled", true, target);
            }
            if let Some(node_name) = self.current_target.take() {
                let entry = LastDeploy {
                    node: node_name.clone(),
                    mode: self.queue_mode,
                    profile: self.queue_profile,
                    exit_code: -1,
                    ok: false,
                };
                self.last_deploys.insert(node_name, entry.clone());
                self.last_deploy = Some(entry);
            }
            self.queue_total = 0;
            self.queue_done = 0;
        } else if probes_aborted > 0 {
            // No deploy was running but probes were — surface that so
            // the user gets feedback for their `x` press.
            self.push_log(
                format!("! cancelled {probes_aborted} in-flight check(s)")
                    .as_str(),
                true,
            );
        }
    }

    /// Abort every tracked probe task and clear the per-host
    /// `checking_*` flags so spinners stop spinning. Returns the
    /// number of probes that were actually still in flight (i.e.
    /// hadn't already finished naturally) so the caller can decide
    /// whether to push a user-visible message.
    fn cancel_probes(&mut self) -> usize {
        let mut aborted = 0usize;
        for h in self.probe_tasks.drain(..) {
            if !h.is_finished() {
                aborted += 1;
                h.abort();
            }
        }
        // Clear every in-flight indicator. The aborted tasks will
        // never publish their final StatusUpdate, so without this
        // sweep the spinners would spin forever.
        for s in self.status.values_mut() {
            s.checking_reachability = false;
            s.checking_system = false;
            s.checking_home = false;
            s.system_extra.checking_size = false;
            s.home_extra.checking_size = false;
            s.system_extra.checking_pkg = false;
            s.home_extra.checking_pkg = false;
        }
        aborted
    }

    fn handle_deploy_line(&mut self, line: LogLine) {
        match line {
            LogLine::Stdout(s) => {
                let host = self.current_target.clone();
                self.push_log_tagged(&s, false, host);
            }
            LogLine::Stderr(s) => {
                let host = self.current_target.clone();
                self.push_log_tagged(&s, true, host);
            }
            LogLine::Exit(code) => {
                let ok = code == 0;
                let banner = if ok {
                    format!("← deploy succeeded (exit {code})")
                } else {
                    format!("← deploy failed (exit {code}) — magic-rollback may have reverted")
                };
                // Snapshot the host before clearing `current_target` so
                // every follow-up log line and the per-host last-deploy
                // entry below can be tagged with it. The post-failure
                // "batch stopped" notice in particular has to be tagged
                // — otherwise the job-log pane filters it out and the
                // last visible line lags behind the details pane.
                let exit_host = self.current_target.take();
                self.push_log_tagged(&banner, !ok, exit_host.clone());
                self.deploy_task = None;
                self.deploy_rx = None;
                self.busy_label = None;
                if let Some(name) = exit_host.clone() {
                    let entry = LastDeploy {
                        node: name.clone(),
                        mode: self.queue_mode,
                        profile: self.queue_profile,
                        exit_code: code,
                        ok,
                    };
                    self.last_deploys.insert(name.clone(), entry.clone());
                    self.last_deploy = Some(entry);
                    if ok {
                        // Stale-update marks: a successful push
                        // invalidates the previously-cached probe.
                        if let Some(s) = self.status.get_mut(&name) {
                            s.system_update = UpdateState::Unknown;
                            s.home_update = UpdateState::Unknown;
                        }
                    }
                }
                self.queue_done = self.queue_done.saturating_add(1);
                if ok {
                    // Drain the next host. If the queue is empty,
                    // start_next_in_queue resets the queue counters.
                    if !self.deploy_queue.is_empty() {
                        self.start_next_in_queue();
                    } else {
                        self.queue_total = 0;
                        self.queue_done = 0;
                    }
                } else {
                    // Stop the batch on failure — safer than blindly
                    // continuing to push to more hosts after one breaks.
                    let dropped = self.deploy_queue.len();
                    if dropped > 0 {
                        self.deploy_queue.clear();
                        self.push_log_tagged(
                            format!(
                                "! batch stopped after failure — {dropped} host(s) skipped"
                            )
                            .as_str(),
                            true,
                            exit_host,
                        );
                    }
                    self.queue_total = 0;
                    self.queue_done = 0;
                }
            }
            LogLine::Error(e) => {
                // Same snapshot-before-take pattern as Exit: we need
                // the host name for the spawn-failure banner, the
                // per-host last-deploy entry, and the post-failure
                // batch-stopped notice. All three want the same string.
                let err_host = self.current_target.take();
                self.push_log_tagged(
                    format!("! deploy spawn failed: {e}").as_str(),
                    true,
                    err_host.clone(),
                );
                self.deploy_task = None;
                self.deploy_rx = None;
                self.busy_label = None;
                if let Some(name) = err_host.clone() {
                    let entry = LastDeploy {
                        node: name.clone(),
                        mode: self.queue_mode,
                        profile: self.queue_profile,
                        exit_code: -1,
                        ok: false,
                    };
                    self.last_deploys.insert(name, entry.clone());
                    self.last_deploy = Some(entry);
                }
                let dropped = self.deploy_queue.len();
                self.deploy_queue.clear();
                if dropped > 0 {
                    self.push_log_tagged(
                        format!("! batch stopped — {dropped} host(s) skipped").as_str(),
                        true,
                        err_host,
                    );
                }
                self.queue_total = 0;
                self.queue_done = 0;
            }
        }
    }

    fn push_log(&mut self, text: &str, is_err: bool) {
        self.push_log_tagged(text, is_err, None);
    }

    /// Push a log line that belongs to a specific host's deploy. Used
    /// by the deploy event handler so the batch log pane can colourise
    /// per host. `host = None` is equivalent to `push_log`.
    fn push_log_tagged(&mut self, text: &str, is_err: bool, host: Option<String>) {
        self.log.push(LogEntry {
            text: text.to_string(),
            is_err,
            host,
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

/// Walk a filtered slice of log indices looking for the next entry
/// whose text contains `query`. `cursor` is the user's current
/// position inside `filtered` and `direction` is `+1` for "newer"
/// (toward the tail, higher index) or `-1` for "older" (toward the
/// head, lower index). Returns the new cursor index inside `filtered`,
/// or `None` if there's no match in the requested direction.
fn next_match(
    filtered: &[usize],
    log: &[LogEntry],
    query: &str,
    cursor: usize,
    direction: i32,
) -> Option<usize> {
    if filtered.is_empty() {
        return None;
    }
    let len = filtered.len() as i32;
    let mut i = cursor as i32 + direction;
    while i >= 0 && i < len {
        let idx = filtered[i as usize];
        if log[idx].text.contains(query) {
            return Some(i as usize);
        }
        i += direction;
    }
    None
}

/// Walk `~/.ssh` and return the paths that look like private keys. We
/// keep the filter conservative — anything that isn't a public key
/// (`*.pub`) or one of the well-known non-key files. The user can still
/// type a custom path in the picker, so missing a key here only costs a
/// keystroke, not correctness.
fn scan_ssh_keys() -> Vec<PathBuf> {
    let Some(home) = std::env::var_os("HOME") else {
        return Vec::new();
    };
    let dir = PathBuf::from(home).join(".ssh");
    let Ok(read) = std::fs::read_dir(&dir) else {
        return Vec::new();
    };
    let blacklist = [
        "config",
        "known_hosts",
        "known_hosts.old",
        "authorized_keys",
        "authorized_keys2",
        "environment",
        "rc",
    ];
    let mut out: Vec<PathBuf> = read
        .flatten()
        .filter_map(|entry| {
            let path = entry.path();
            let ft = entry.file_type().ok()?;
            if !ft.is_file() {
                return None;
            }
            let name = entry.file_name();
            let name = name.to_string_lossy();
            if name.ends_with(".pub") || blacklist.iter().any(|b| name == *b) {
                return None;
            }
            Some(path)
        })
        .collect();
    out.sort();
    out
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
