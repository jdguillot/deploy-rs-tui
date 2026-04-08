//! TUI bootstrap + rendering primitives.
//!
//! `init` / `restore` set up the terminal in raw mode with the alternate
//! screen, and `draw` paints the current [`App`] state. Keep all crossterm
//! plumbing here so the App can stay focused on state transitions.

use std::io::{stdout, Stdout};

use anyhow::{Context, Result};
use crossterm::{
    execute,
    terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen},
};
use ratatui::{
    backend::CrosstermBackend,
    layout::{Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style, Stylize},
    text::{Line, Span},
    widgets::{Block, Borders, Clear, List, ListItem, Paragraph, Wrap},
    Frame, Terminal,
};

use crate::app::{App, FocusPane, InputMode, OverrideField};
use crate::deploy::{Mode, ProfileSel};
use crate::host::{Reachability, UpdateState};

pub type Tui = Terminal<CrosstermBackend<Stdout>>;

pub fn init() -> Result<Tui> {
    enable_raw_mode().context("enabling raw mode")?;
    let mut out = stdout();
    execute!(out, EnterAlternateScreen).context("entering alternate screen")?;
    let backend = CrosstermBackend::new(out);
    let terminal = Terminal::new(backend).context("constructing terminal")?;
    Ok(terminal)
}

pub fn restore() -> Result<()> {
    let mut out = stdout();
    execute!(out, LeaveAlternateScreen).ok();
    disable_raw_mode().ok();
    Ok(())
}

pub fn draw(frame: &mut Frame, app: &App) {
    let area = frame.area();

    // Vertical layout, top → bottom:
    //   1. title bar
    //   2. toggles strip (one line, always visible — it IS the contract)
    //   3. body (host list ← → details + log)
    //   4. status / input strip
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1),
            Constraint::Length(1),
            Constraint::Min(5),
            Constraint::Length(1),
        ])
        .split(area);

    draw_title(frame, chunks[0], app);
    draw_toggles_strip(frame, chunks[1], app);
    draw_body(frame, chunks[2], app);
    draw_bottom_strip(frame, chunks[3], app);

    if app.show_help {
        draw_help_popup(frame, area);
    }
}

fn draw_title(frame: &mut Frame, area: Rect, app: &App) {
    let mut spans = vec![
        Span::styled(
            " deploy-rs-tui ",
            Style::default()
                .bg(Color::Magenta)
                .fg(Color::Black)
                .add_modifier(Modifier::BOLD),
        ),
        Span::raw(" "),
        Span::styled(&app.flake, Style::default().fg(Color::Cyan)),
    ];
    if let Some(busy) = &app.busy_label {
        spans.push(Span::raw("  "));
        spans.push(Span::styled(
            format!("⟳ {busy}"),
            Style::default().fg(Color::Yellow),
        ));
    }
    frame.render_widget(Paragraph::new(Line::from(spans)), area);
}

fn draw_body(frame: &mut Frame, area: Rect, app: &App) {
    let cols = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(40), Constraint::Percentage(60)])
        .split(area);

    draw_host_list(frame, cols[0], app);
    draw_details(frame, cols[1], app);
}

fn draw_host_list(frame: &mut Frame, area: Rect, app: &App) {
    let items: Vec<ListItem> = app
        .nodes
        .iter()
        .enumerate()
        .map(|(i, node)| {
            let status = app.status_for(&node.name);
            let reach = match status.reachability {
                Reachability::Online => Span::styled("●", Style::default().fg(Color::Green)),
                Reachability::Offline => Span::styled("●", Style::default().fg(Color::Red)),
                Reachability::Unknown => Span::styled("●", Style::default().fg(Color::DarkGray)),
            };
            let sys = badge(
                "sys",
                node.has_system(),
                status.system_update,
                status.checking_system,
                app.tick_counter,
            );
            let home = badge(
                "home",
                node.has_home(),
                status.home_update,
                status.checking_home,
                app.tick_counter,
            );
            let selected = i == app.selected;
            let name_style = if selected {
                Style::default()
                    .fg(Color::Black)
                    .bg(Color::Cyan)
                    .add_modifier(Modifier::BOLD)
            } else {
                Style::default()
            };
            // Override marker — a small magenta bracket suffix when the
            // user has set any per-host SSH overrides for this node.
            let mut row = vec![
                reach,
                Span::raw(" "),
                Span::styled(node.name.clone(), name_style),
            ];
            if app.override_for(&node.name).is_active() {
                row.push(Span::styled(
                    " [ssh]",
                    Style::default()
                        .fg(Color::Magenta)
                        .add_modifier(Modifier::BOLD),
                ));
            }
            row.push(Span::raw("  "));
            row.push(sys);
            row.push(Span::raw(" "));
            row.push(home);
            ListItem::new(Line::from(row))
        })
        .collect();

    let title = if app.focus == FocusPane::Hosts {
        " hosts ".bold()
    } else {
        " hosts ".into()
    };
    let list = List::new(items).block(Block::default().borders(Borders::ALL).title(title));
    frame.render_widget(list, area);
}

/// Braille spinner — same frames `cargo`/`nix` use, distinct from any
/// static badge icon so an in-flight probe is unambiguously visible.
const SPINNER_FRAMES: &[char] = &['⠋', '⠙', '⠹', '⠸', '⠼', '⠴', '⠦', '⠧', '⠇', '⠏'];

fn badge(
    label: &str,
    present: bool,
    state: UpdateState,
    checking: bool,
    tick: u64,
) -> Span<'static> {
    if !present {
        return Span::styled(format!("{label}:-"), Style::default().fg(Color::DarkGray));
    }
    if checking {
        // Render the previous icon (dimmed) followed by the spinner so the
        // user can simultaneously see "what we knew before" and "we are
        // re-checking right now". When there's no prior result this just
        // collapses to the spinner.
        let frame = SPINNER_FRAMES[(tick as usize) % SPINNER_FRAMES.len()];
        let prior = match state {
            UpdateState::UpToDate => Some('✓'),
            UpdateState::NeedsUpdate => Some('↑'),
            UpdateState::Error => Some('!'),
            UpdateState::Unknown => None,
        };
        let text = match prior {
            Some(p) => format!("{label}:{p}{frame}"),
            None => format!("{label}:{frame}"),
        };
        return Span::styled(text, Style::default().fg(Color::Cyan));
    }
    let (icon, color) = match state {
        UpdateState::UpToDate => ("✓", Color::Green),
        UpdateState::NeedsUpdate => ("↑", Color::Yellow),
        UpdateState::Error => ("!", Color::Red),
        UpdateState::Unknown => ("?", Color::DarkGray),
    };
    Span::styled(format!("{label}:{icon}"), Style::default().fg(color))
}

fn draw_details(frame: &mut Frame, area: Rect, app: &App) {
    let title = if app.focus == FocusPane::Details {
        " details ".bold()
    } else {
        " details ".into()
    };
    let block = Block::default().borders(Borders::ALL).title(title);
    let inner = block.inner(area);
    frame.render_widget(block, area);

    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(9), Constraint::Min(3)])
        .split(inner);

    draw_node_summary(frame, rows[0], app);
    draw_log(frame, rows[1], app);
}

fn draw_node_summary(frame: &mut Frame, area: Rect, app: &App) {
    let Some(node) = app.selected_node() else {
        frame.render_widget(Paragraph::new("no nodes"), area);
        return;
    };
    let status = app.status_for(&node.name);

    let override_ = app.override_for(&node.name);
    let mut lines = vec![
        Line::from(vec![
            Span::styled("name     ", Style::default().fg(Color::DarkGray)),
            Span::raw(node.name.clone()),
        ]),
        Line::from(vec![
            Span::styled("hostname ", Style::default().fg(Color::DarkGray)),
            Span::raw(node.hostname.clone()),
        ]),
        Line::from(vec![
            Span::styled("profiles ", Style::default().fg(Color::DarkGray)),
            Span::raw(
                node.profiles
                    .keys()
                    .cloned()
                    .collect::<Vec<_>>()
                    .join(", "),
            ),
        ]),
        Line::from(vec![
            Span::styled("status   ", Style::default().fg(Color::DarkGray)),
            match status.reachability {
                Reachability::Online => Span::styled("online", Style::default().fg(Color::Green)),
                Reachability::Offline => Span::styled("offline", Style::default().fg(Color::Red)),
                Reachability::Unknown => {
                    Span::styled("unknown", Style::default().fg(Color::DarkGray))
                }
            },
        ]),
        Line::from(vec![
            Span::styled("mode     ", Style::default().fg(Color::DarkGray)),
            Span::raw(format!(
                "{} / {}",
                describe_mode(app.mode),
                describe_profile(app.profile_sel)
            )),
        ]),
        Line::from(vec![
            Span::styled("override ", Style::default().fg(Color::DarkGray)),
            if override_.is_active() {
                Span::styled(override_.summary(), Style::default().fg(Color::Magenta))
            } else {
                Span::styled("(none)", Style::default().fg(Color::DarkGray))
            },
        ]),
    ];
    if let Some(err) = &status.last_error {
        lines.push(Line::from(vec![
            Span::styled("error    ", Style::default().fg(Color::Red)),
            Span::raw(err.clone()),
        ]));
    }

    frame.render_widget(Paragraph::new(lines).wrap(Wrap { trim: false }), area);
}

fn draw_log(frame: &mut Frame, area: Rect, app: &App) {
    // Show the tail of the log so the most recent line is always visible.
    let height = area.height as usize;
    let start = app.log.len().saturating_sub(height);
    let lines: Vec<Line> = app.log[start..]
        .iter()
        .map(|entry| {
            let style = if entry.is_err {
                Style::default().fg(Color::Red)
            } else {
                Style::default()
            };
            Line::styled(entry.text.clone(), style)
        })
        .collect();
    frame.render_widget(
        Paragraph::new(lines).wrap(Wrap { trim: false }),
        area,
    );
}

fn draw_toggles_strip(frame: &mut Frame, area: Rect, app: &App) {
    let t = app.toggles;
    let spans = vec![
        Span::styled(" toggles ", Style::default().fg(Color::DarkGray)),
        toggle_span("1", "skip-checks", t.skip_checks),
        Span::raw("  "),
        toggle_span("2", "magic-rb", t.magic_rollback),
        Span::raw("  "),
        toggle_span("3", "auto-rb", t.auto_rollback),
        Span::raw("  "),
        toggle_span("4", "remote-build", t.remote_build),
        Span::raw("  "),
        toggle_span("5", "int-sudo", t.interactive_sudo),
    ];
    frame.render_widget(Paragraph::new(Line::from(spans)), area);
}

fn toggle_span(key: &str, label: &str, on: bool) -> Span<'static> {
    let icon = if on { "●" } else { "○" };
    let color = if on { Color::Green } else { Color::DarkGray };
    Span::styled(
        format!("{key}:{icon} {label}"),
        Style::default().fg(color),
    )
}

/// Bottom strip switches between three things depending on state:
///   - editing an override field → input prompt
///   - in the overrides menu → sub-menu hint
///   - normal → keybind cheat sheet
fn draw_bottom_strip(frame: &mut Frame, area: Rect, app: &App) {
    let line = match &app.input {
        InputMode::EditOverride { field, buf } => {
            let label = match field {
                OverrideField::Hostname => "hostname / IP",
                OverrideField::User => "ssh user",
                OverrideField::Identity => "identity file",
                OverrideField::Opts => "extra ssh opts",
            };
            Line::from(vec![
                Span::styled(
                    format!(" {label} ▸ "),
                    Style::default()
                        .fg(Color::Black)
                        .bg(Color::Magenta)
                        .add_modifier(Modifier::BOLD),
                ),
                Span::raw(" "),
                Span::raw(buf.clone()),
                Span::styled("▎", Style::default().fg(Color::Magenta)),
                Span::raw("   "),
                Span::styled("Enter", Style::default().fg(Color::Yellow)),
                Span::raw(" save  "),
                Span::styled("Esc", Style::default().fg(Color::Yellow)),
                Span::raw(" cancel"),
            ])
        }
        InputMode::OverridesMenu => Line::from(vec![
            Span::styled(
                " override ",
                Style::default()
                    .fg(Color::Black)
                    .bg(Color::Magenta)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::raw("  "),
            Span::styled("h", Style::default().fg(Color::Yellow)),
            Span::raw(" host  "),
            Span::styled("u", Style::default().fg(Color::Yellow)),
            Span::raw(" user  "),
            Span::styled("k", Style::default().fg(Color::Yellow)),
            Span::raw(" key  "),
            Span::styled("o", Style::default().fg(Color::Yellow)),
            Span::raw(" opts  "),
            Span::styled("c", Style::default().fg(Color::Yellow)),
            Span::raw(" clear  "),
            Span::styled("Esc", Style::default().fg(Color::Yellow)),
            Span::raw(" back"),
        ]),
        InputMode::Normal => Line::from(vec![
            Span::styled(" ?", Style::default().fg(Color::Yellow)),
            Span::raw(" help  "),
            Span::styled("↑/↓", Style::default().fg(Color::Yellow)),
            Span::raw(" select  "),
            Span::styled("r", Style::default().fg(Color::Yellow)),
            Span::raw(" online  "),
            Span::styled("u", Style::default().fg(Color::Yellow)),
            Span::raw(" updates  "),
            Span::styled("a/n/h", Style::default().fg(Color::Yellow)),
            Span::raw(" profile  "),
            Span::styled("s/b/d", Style::default().fg(Color::Yellow)),
            Span::raw(" switch/boot/dry  "),
            Span::styled("o", Style::default().fg(Color::Yellow)),
            Span::raw(" override  "),
            Span::styled("1-5", Style::default().fg(Color::Yellow)),
            Span::raw(" toggles  "),
            Span::styled("x", Style::default().fg(Color::Yellow)),
            Span::raw(" cancel  "),
            Span::styled("q", Style::default().fg(Color::Yellow)),
            Span::raw(" quit"),
        ]),
    };
    frame.render_widget(Paragraph::new(line), area);
}

/// Centered help popup. We use ratatui's `Clear` widget to wipe the
/// underlying area before drawing, so the popup looks like a real modal
/// instead of overlapping the host list.
fn draw_help_popup(frame: &mut Frame, area: Rect) {
    let popup = centered_rect(78, 80, area);
    frame.render_widget(Clear, popup);

    let block = Block::default()
        .borders(Borders::ALL)
        .title(" help — press ? or Esc to close ".bold());
    let inner = block.inner(popup);
    frame.render_widget(block, popup);

    let dim = Style::default().fg(Color::DarkGray);
    let key = Style::default().fg(Color::Yellow);
    let head = Style::default()
        .fg(Color::Cyan)
        .add_modifier(Modifier::BOLD);

    let lines: Vec<Line> = vec![
        Line::styled("navigation", head),
        Line::from(vec![
            Span::styled("  ↑/↓ j/k  ", key),
            Span::raw("move host selection"),
        ]),
        Line::from(vec![
            Span::styled("  Tab      ", key),
            Span::raw("swap focus between host list and details pane"),
        ]),
        Line::from(vec![
            Span::styled("  q / Esc  ", key),
            Span::raw("quit (Ctrl-C also works and kills any running deploy)"),
        ]),
        Line::raw(""),
        Line::styled("status", head),
        Line::from(vec![
            Span::styled("  r  ", key),
            Span::raw("refresh online/offline (TCP-22 probe) for every host"),
        ]),
        Line::from(vec![
            Span::styled("  u  ", key),
            Span::raw(
                "check whether the selected host needs an update (compares local build vs remote profile symlink)",
            ),
        ]),
        Line::from(vec![
            Span::styled("       ", dim),
            Span::styled(
                "badges: ✓ up-to-date  ↑ behind  ! error  ? unchecked  - n/a  ⠋ checking",
                dim,
            ),
        ]),
        Line::raw(""),
        Line::styled("deploy", head),
        Line::from(vec![
            Span::styled("  a / n / h  ", key),
            Span::raw("target all profiles / system (NixOS) / home (home-manager)"),
        ]),
        Line::from(vec![
            Span::styled("  s          ", key),
            Span::raw("switch — apply now"),
        ]),
        Line::from(vec![
            Span::styled("  b          ", key),
            Span::raw("boot — install as next boot entry, don't activate now"),
        ]),
        Line::from(vec![
            Span::styled("  d          ", key),
            Span::raw("dry-run — `deploy --dry-activate`, build + diff only"),
        ]),
        Line::from(vec![
            Span::styled("  x          ", key),
            Span::raw("cancel a running deploy (sends SIGKILL to the child)"),
        ]),
        Line::raw(""),
        Line::styled("toggles (number keys)", head),
        Line::from(vec![
            Span::styled("  1  ", key),
            Span::raw("skip-checks — skip the pre-deploy `nix flake check`"),
        ]),
        Line::from(vec![
            Span::styled("  2  ", key),
            Span::raw(
                "magic-rollback — wait for confirmation, auto-roll-back on timeout (default ON)",
            ),
        ]),
        Line::from(vec![
            Span::styled("  3  ", key),
            Span::raw("auto-rollback — roll back if activation fails (default ON)"),
        ]),
        Line::from(vec![
            Span::styled("  4  ", key),
            Span::raw("remote-build — perform the build on the target host"),
        ]),
        Line::from(vec![
            Span::styled("  5  ", key),
            Span::raw(
                "interactive-sudo — prompt for sudo password (will hang the TUI; use passwordless sudo)",
            ),
        ]),
        Line::raw(""),
        Line::styled("ssh overrides (per host)", head),
        Line::from(vec![
            Span::styled("  o      ", key),
            Span::raw("open the overrides menu for the selected host"),
        ]),
        Line::from(vec![
            Span::styled("    h    ", key),
            Span::raw("set hostname / IP override (use this if the node isn't in ~/.ssh/config)"),
        ]),
        Line::from(vec![
            Span::styled("    u    ", key),
            Span::raw("set ssh user"),
        ]),
        Line::from(vec![
            Span::styled("    k    ", key),
            Span::raw("set identity file path (passed as `ssh -i`)"),
        ]),
        Line::from(vec![
            Span::styled("    o    ", key),
            Span::raw(
                "set extra ssh -o opts (whitespace-separated, e.g. `Port=2222 ProxyJump=bastion`)",
            ),
        ]),
        Line::from(vec![
            Span::styled("    c    ", key),
            Span::raw("clear all overrides for this host"),
        ]),
        Line::from(vec![
            Span::styled("       ", dim),
            Span::styled(
                "hosts with overrides show a magenta [ssh] tag in the list",
                dim,
            ),
        ]),
    ];

    frame.render_widget(
        Paragraph::new(lines).wrap(Wrap { trim: false }),
        inner,
    );
}

/// Compute a centered popup `Rect` of the requested percentage size.
fn centered_rect(percent_x: u16, percent_y: u16, r: Rect) -> Rect {
    let vertical = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Percentage((100 - percent_y) / 2),
            Constraint::Percentage(percent_y),
            Constraint::Percentage((100 - percent_y) / 2),
        ])
        .split(r);
    Layout::default()
        .direction(Direction::Horizontal)
        .constraints([
            Constraint::Percentage((100 - percent_x) / 2),
            Constraint::Percentage(percent_x),
            Constraint::Percentage((100 - percent_x) / 2),
        ])
        .split(vertical[1])[1]
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
        ProfileSel::All => "all profiles",
        ProfileSel::System => "system only",
        ProfileSel::Home => "home only",
    }
}
