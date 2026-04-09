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

use crate::app::{
    App, FocusPane, InputMode, LastDeploy, OverrideField, COMMANDS, TOGGLE_COUNT,
};
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

pub fn draw(frame: &mut Frame, app: &mut App) {
    let area = frame.area();

    // Vertical layout, top → bottom:
    //   1. title bar
    //   2. toggles strip — bordered block (3 or 4 rows — see below)
    //   3. body (hosts | details | job log)
    //   4. commands pane — bordered block (3 or 4 rows), split
    //      horizontally into an info-hints half (left) and a
    //      navigable command button row (right). The info half is
    //      context-aware: it re-renders its hint list based on the
    //      currently focused pane, so j/k mean different things in
    //      Hosts vs Details and the cheat-sheet follows.
    //   5. input prompt strip — only present (1 row) when an
    //      override/confirm input mode is active. In Normal mode the
    //      row collapses to 0 so the bottom edge of the commands box
    //      sits flush with the terminal border instead of leaving an
    //      empty gap.
    //
    // **Adaptive height.** The toggles/commands strips normally sit
    // at 3 rows (1 inner). On narrow terminals — or when the context
    // info line gets long — a single inner row can't fit the whole
    // content. Instead of truncating, we bump the relevant strip to
    // 4 rows (2 inner) and let ratatui's `Wrap` split the spans
    // across the two lines. Measurement below is pessimistic (adds a
    // small fudge for the leading space + trailing margin) so we
    // always bump *before* the content actually gets clipped.
    let needs_input_strip = !matches!(app.input, InputMode::Normal);
    let input_strip_height = if needs_input_strip { 1 } else { 0 };

    // Toggles: single-row content, full width minus borders.
    let toggles_content_w = toggles_content_width();
    let toggles_inner_w = area.width.saturating_sub(2) as usize;
    let toggles_height: u16 = if toggles_content_w > toggles_inner_w { 4 } else { 3 };

    // Commands row: two-column layout (40/60). Measure info +
    // commands independently and bump the whole strip if *either*
    // side overflows its column.
    let info_col_w = (area.width as usize * 40) / 100;
    let cmd_col_w = area.width as usize - info_col_w;
    let info_inner_w = info_col_w.saturating_sub(2);
    let cmd_inner_w = cmd_col_w.saturating_sub(2);
    let info_content_w = info_content_width(app);
    let cmd_content_w = commands_content_width();
    let commands_height: u16 =
        if info_content_w > info_inner_w || cmd_content_w > cmd_inner_w {
            4
        } else {
            3
        };

    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1),
            Constraint::Length(toggles_height),
            Constraint::Min(5),
            Constraint::Length(commands_height),
            Constraint::Length(input_strip_height),
        ])
        .split(area);

    draw_title(frame, chunks[0], app);
    draw_toggles_strip(frame, chunks[1], app);
    draw_body(frame, chunks[2], app);
    draw_commands_row(frame, chunks[3], app);
    if needs_input_strip {
        draw_input_strip(frame, chunks[4], app);
    }
    // Note: draw_help_popup borrows `app.help_scroll` mutably, so it
    // runs after the body which already returned its &mut borrow.

    if app.show_help {
        // The popup clamps `help_scroll` against the rendered content
        // height in-place, so the next keypress can't accumulate
        // phantom offset past the bottom of the cheat sheet.
        draw_help_popup(frame, area, app);
    }
    if let InputMode::EditIdentityPicker {
        entries,
        selected,
        buf,
    } = &app.input
    {
        draw_identity_picker_popup(frame, area, entries, *selected, buf);
    }
    if let InputMode::ConfirmDeploy {
        hosts,
        mode,
        profile,
    } = &app.input
    {
        draw_confirm_popup(frame, area, hosts, *mode, *profile);
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
        // Hint that the running job can be cancelled with `x`. Always
        // visible during a deploy so the user doesn't have to dig
        // through help.
        spans.push(Span::raw("  "));
        spans.push(Span::styled(
            "[x to cancel]",
            Style::default().fg(Color::DarkGray),
        ));
    } else if let Some(last) = &app.last_deploy {
        // Only show the last-deploy chip when nothing is currently
        // running, otherwise it's noise. Bright colours so the user
        // can't miss that "we are no longer mid-deploy".
        spans.push(Span::raw("  "));
        spans.push(deploy_outcome_chip(last));
    }
    frame.render_widget(Paragraph::new(Line::from(spans)), area);
}

/// Coloured pill rendering of a [`LastDeploy`]. Used in both the title
/// bar and the details summary so the same status is consistent.
fn deploy_outcome_chip(last: &LastDeploy) -> Span<'static> {
    let (icon, label, bg) = if last.ok {
        ("✓", "DONE", Color::Green)
    } else {
        ("✗", "FAILED", Color::Red)
    };
    Span::styled(
        format!(
            " {icon} {label}  {} ({} / {})  exit {} ",
            last.node,
            describe_mode(last.mode),
            describe_profile(last.profile),
            last.exit_code,
        ),
        Style::default()
            .bg(bg)
            .fg(Color::Black)
            .add_modifier(Modifier::BOLD),
    )
}

fn draw_body(frame: &mut Frame, area: Rect, app: &mut App) {
    // Three-column layout. The job log lives permanently in the right
    // column because that's where the actual deploy-rs stdout ends up
    // and it's the thing the user watches during a run — so it gets
    // the most horizontal space. Hosts and details stay narrower.
    let cols = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([
            Constraint::Percentage(22),
            Constraint::Percentage(28),
            Constraint::Percentage(50),
        ])
        .split(area);
    draw_host_list(frame, cols[0], app);
    // Details and job log both need to clamp their scroll offsets
    // against the rendered visible height, so they take &mut App and
    // mutate `log_scroll` / `job_log_scroll` in place. Held-key past
    // the edge can no longer accumulate phantom offset.
    draw_details(frame, cols[1], app);
    draw_job_log(frame, cols[2], app);
}

fn draw_host_list(frame: &mut Frame, area: Rect, app: &App) {
    let items: Vec<ListItem> = app
        .nodes
        .iter()
        .enumerate()
        .map(|(i, node)| {
            let status = app.status_for(&node.name);
            // Checking state uses the same Braille spinner the update
            // probes use so a reachability refresh visually matches the
            // `u` refresh (the user asked for parity).
            let reach = if status.checking_reachability {
                let frame = SPINNER_FRAMES
                    [(app.tick_counter as usize) % SPINNER_FRAMES.len()];
                Span::styled(
                    frame.to_string(),
                    Style::default().fg(Color::Cyan),
                )
            } else {
                match status.reachability {
                    Reachability::Online => {
                        Span::styled("●", Style::default().fg(Color::Green))
                    }
                    Reachability::Offline => {
                        Span::styled("●", Style::default().fg(Color::Red))
                    }
                    Reachability::Unknown => {
                        Span::styled("●", Style::default().fg(Color::DarkGray))
                    }
                }
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
            // Multi-select marker. A bright cyan `+` for marked hosts,
            // a dim space for unmarked — the column always exists so
            // the rest of the row stays aligned.
            let mark = if app.is_marked(&node.name) {
                Span::styled(
                    "+",
                    Style::default()
                        .fg(Color::Cyan)
                        .add_modifier(Modifier::BOLD),
                )
            } else {
                Span::raw(" ")
            };
            // Override marker — a small magenta bracket suffix when the
            // user has set any per-host SSH overrides for this node.
            let mut row = vec![
                mark,
                Span::raw(" "),
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

    // Title shows the marked count when the user has built a batch, so
    // they can spot at a glance how many hosts s/b/d will hit.
    let count_label = if app.marked.is_empty() {
        String::new()
    } else {
        format!(" [{} marked] ", app.marked.len())
    };
    let focused = app.focus == FocusPane::Hosts;
    // `g` was repurposed to "scroll to top" across every pane, so the
    // hosts jump letter is `f` (focus hosts) — the obvious `h` is
    // already the home-profile shortcut and `n` owns search-next.
    // We inject `f` manually (instead of letting pane_title_spans
    // find it in the label) so the bracket sits before the word.
    let mut title_spans = pane_title_spans("fhosts", 'f', focused);
    if !count_label.is_empty() {
        title_spans.push(Span::styled(
            count_label,
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        ));
    }
    let list = List::new(items).block(
        Block::default()
            .borders(Borders::ALL)
            .border_style(focus_border_style(focused))
            .title(Line::from(title_spans)),
    );
    frame.render_widget(list, area);
}

/// Border colour for a pane that can hold focus: magenta when the
/// pane owns the keyboard, default otherwise. Shares the colour
/// with the override chips/input strip so "this thing is active /
/// interactive" is one consistent visual cue across the UI.
fn focus_border_style(focused: bool) -> Style {
    if focused {
        Style::default().fg(Color::Yellow)
    } else {
        Style::default()
    }
}

/// Title styling that matches the border. Focused = bold yellow
/// (same hue as the `[k]` hot-letter backgrounds so the focused pane
/// visually "owns" its jump key), otherwise the terminal default.
fn focus_title_style(focused: bool) -> Style {
    if focused {
        Style::default()
            .fg(Color::Yellow)
            .add_modifier(Modifier::BOLD)
    } else {
        Style::default()
    }
}

/// Build a title row for a pane that has a direct-jump hotkey. The
/// hotkey letter is rendered in yellow (or bright yellow when focused)
/// so the user can see at a glance which key bounces to this pane.
///
/// We split on the first case-insensitive occurrence of `jump` inside
/// `label`; if the letter isn't in the label, the jump letter gets
/// prefixed as `[l] label`.
fn pane_title_spans(label: &str, jump: char, focused: bool) -> Vec<Span<'static>> {
    let base = focus_title_style(focused);
    let hot = Style::default()
        .fg(Color::Yellow)
        .add_modifier(Modifier::BOLD | if focused { Modifier::REVERSED } else { Modifier::empty() });
    // Find the first matching letter inside `label` so the title reads
    // naturally (e.g. "[h]osts" rather than "[h] hosts").
    if let Some(idx) = label
        .char_indices()
        .find(|(_, c)| c.eq_ignore_ascii_case(&jump))
        .map(|(i, _)| i)
    {
        let before = &label[..idx];
        // Advance past the matched character; char_indices gives us
        // the starting byte, so we need to take one char-width.
        let mut rest = label[idx..].chars();
        let hotchar = rest.next().unwrap_or(jump);
        let after: String = rest.collect();
        return vec![
            Span::raw(" "),
            Span::styled(before.to_string(), base),
            Span::styled(format!("[{hotchar}]"), hot),
            Span::styled(after, base),
            Span::raw(" "),
        ];
    }
    vec![
        Span::raw(" "),
        Span::styled(format!("[{jump}]"), hot),
        Span::raw(" "),
        Span::styled(label.to_string(), base),
        Span::raw(" "),
    ]
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

fn draw_details(frame: &mut Frame, area: Rect, app: &mut App) {
    let focused = app.focus == FocusPane::Details;
    // Surface the active search query in the pane title so the user
    // can see what they're filtering on without consulting the input
    // strip. Only fires when the committed search is pinned to the
    // details pane (job-log searches don't bleed across).
    let mut title_spans = pane_title_spans("details", 'i', focused);
    if let (Some(q), Some(crate::app::SearchTarget::DetailsLog)) =
        (app.log_search.as_ref(), app.log_search_target)
    {
        let (cur, total) = app.log_search_stats(crate::app::SearchTarget::DetailsLog);
        title_spans.push(search_chip(q, cur, total));
    }
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(focus_border_style(focused))
        .title(Line::from(title_spans));
    let inner = block.inner(area);
    frame.render_widget(block, area);

    // Build the extras lines up front so we can size the layout to
    // their actual height — using `Min(N)` made the extras pane
    // grab all the spare vertical space and pushed the log down
    // with a giant empty gap.
    let extras_lines = build_profile_extras_lines(app);
    let extras_height = extras_lines.len() as u16;
    if extras_height > 0 {
        let rows = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Length(11),
                Constraint::Length(extras_height),
                Constraint::Min(3),
            ])
            .split(inner);
        draw_node_summary(frame, rows[0], app);
        frame.render_widget(
            Paragraph::new(extras_lines).wrap(Wrap { trim: false }),
            rows[1],
        );
        draw_log(frame, rows[2], app);
    } else {
        let rows = Layout::default()
            .direction(Direction::Vertical)
            .constraints([Constraint::Length(11), Constraint::Min(3)])
            .split(inner);
        draw_node_summary(frame, rows[0], app);
        draw_log(frame, rows[1], app);
    }
}

/// Build the "update details" lines for the currently-selected node.
/// Returns an empty vector when nothing has been probed yet so the
/// caller can omit the section entirely.
fn build_profile_extras_lines(app: &App) -> Vec<Line<'static>> {
    let Some(node) = app.selected_node() else {
        return Vec::new();
    };
    let status = app.status_for(&node.name);
    let has_any = (node.has_system() && status.system_extra.local_path.is_some())
        || (node.has_home() && status.home_extra.local_path.is_some());
    if !has_any {
        return Vec::new();
    }

    let mut lines: Vec<Line<'static>> = Vec::new();
    lines.push(Line::from(Span::styled(
        "update details",
        Style::default()
            .fg(Color::Cyan)
            .add_modifier(Modifier::BOLD),
    )));
    for (label, has, extra) in [
        ("system", node.has_system(), &status.system_extra),
        ("home", node.has_home(), &status.home_extra),
    ] {
        if !has || extra.local_path.is_none() {
            continue;
        }
        lines.push(Line::from(vec![
            Span::styled(
                format!("{label:<6} "),
                Style::default()
                    .fg(Color::Magenta)
                    .add_modifier(Modifier::BOLD),
            ),
            short_hash_span("local ", extra.local_path.as_deref()),
            Span::raw("  "),
            short_hash_span("remote ", extra.remote_path.as_deref()),
        ]));
        // Activation time + closure size delta share one row when
        // both are available so short displays don't bloat vertically.
        let mut meta: Vec<Span<'static>> = vec![Span::raw("       ")];
        if let Some(t) = extra.activation_time {
            // The mtime we read is the remote profile *symlink* (e.g.
            // `/run/current-system`), which activate-rs rewrites every
            // time the profile is switched in — i.e. how long ago this
            // exact closure became the running one.
            meta.push(Span::styled(
                "activated ",
                Style::default().fg(Color::DarkGray),
            ));
            meta.push(Span::styled(
                format_time_ago(t),
                Style::default().fg(Color::Green),
            ));
            meta.push(Span::raw("  "));
        }
        if extra.checking_size {
            let frame_ch =
                SPINNER_FRAMES[(app.tick_counter as usize) % SPINNER_FRAMES.len()];
            meta.push(Span::styled(
                format!("size {frame_ch}"),
                Style::default().fg(Color::Cyan),
            ));
        } else if let (Some(local), Some(remote)) = (extra.local_size, extra.remote_size) {
            meta.push(Span::styled(
                "size ",
                Style::default().fg(Color::DarkGray),
            ));
            meta.push(size_delta_span(local, remote));
        } else {
            meta.push(Span::styled(
                "size ?  (Shift+U)",
                Style::default().fg(Color::DarkGray),
            ));
        }
        if meta.len() > 1 {
            lines.push(Line::from(meta));
        }
        // Package diff: one-line summary only. The per-package
        // version changes themselves stream into the job log via
        // `[pkg]`-tagged entries, so the details pane doesn't need to
        // (and shouldn't) duplicate them — keeping it compact lets
        // the actual log breathe and avoids two competing views of
        // the same data.
        if extra.checking_pkg {
            let frame_ch =
                SPINNER_FRAMES[(app.tick_counter as usize) % SPINNER_FRAMES.len()];
            lines.push(Line::from(vec![
                Span::raw("       "),
                Span::styled(
                    format!("packages {frame_ch}"),
                    Style::default().fg(Color::Cyan),
                ),
            ]));
        } else if let Some(diff) = extra.pkg_diff.as_deref() {
            if diff.is_empty() {
                lines.push(Line::from(vec![
                    Span::raw("       "),
                    Span::styled(
                        "packages identical",
                        Style::default().fg(Color::Green),
                    ),
                ]));
            } else {
                let total = diff.trim().lines().count();
                lines.push(Line::from(vec![
                    Span::raw("       "),
                    Span::styled(
                        format!(
                            "packages ({total} change{})",
                            if total == 1 { "" } else { "s" }
                        ),
                        Style::default()
                            .fg(Color::Yellow)
                            .add_modifier(Modifier::BOLD),
                    ),
                    Span::styled(
                        "  see job log for per-package details",
                        Style::default().fg(Color::DarkGray),
                    ),
                ]));
            }
        } else {
            lines.push(Line::from(vec![
                Span::raw("       "),
                Span::styled(
                    "packages ?  (p)",
                    Style::default().fg(Color::DarkGray),
                ),
            ]));
        }
    }
    lines
}

/// Condensed renderer for a store path — shows `label <hash-prefix>` so
/// the user can eyeball whether the local/remote match without reading
/// a 50-char `/nix/store/...` path. Falls back to `(unknown)` when the
/// path is missing.
fn short_hash_span(label: &'static str, path: Option<&str>) -> Span<'static> {
    let text = match path {
        Some(p) => {
            // Nix store paths are `/nix/store/<hash>-<name>`; grab the
            // first 10 chars of the hash as a fingerprint.
            let hash = p
                .rsplit('/')
                .next()
                .unwrap_or(p)
                .split('-')
                .next()
                .unwrap_or("");
            let short = hash.chars().take(10).collect::<String>();
            format!("{label}{short}")
        }
        None => format!("{label}?"),
    };
    Span::styled(text, Style::default().fg(Color::Cyan))
}

/// Humanised closure size delta: `+42.3 MiB` / `-7.0 MiB` / `±0 B`.
/// Coloured by direction (green shrinking, yellow growing).
fn size_delta_span(local: u64, remote: u64) -> Span<'static> {
    let (delta_abs, sign, color) = if local >= remote {
        (local - remote, '+', Color::Yellow)
    } else {
        (remote - local, '-', Color::Green)
    };
    let formatted = humanise_bytes(delta_abs);
    let text = if delta_abs == 0 {
        format!("{} (unchanged)", humanise_bytes(local))
    } else {
        format!(
            "{} (local {}, remote {})",
            format!("{sign}{formatted}"),
            humanise_bytes(local),
            humanise_bytes(remote),
        )
    };
    Span::styled(text, Style::default().fg(color))
}

/// Bytes → short human-readable string (B / KiB / MiB / GiB). Uses
/// binary prefixes to match `nix`.
fn humanise_bytes(b: u64) -> String {
    const UNITS: &[&str] = &["B", "KiB", "MiB", "GiB", "TiB"];
    let mut value = b as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit < UNITS.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{b} B")
    } else {
        format!("{value:.1} {}", UNITS[unit])
    }
}

/// Render a past `SystemTime` as a short "Xs ago" string. Falls back
/// to "just now" for anything under a second and for clock skew. Kept
/// simple on purpose — we only need enough resolution for a user to
/// judge "is this stale?".
fn format_time_ago(t: std::time::SystemTime) -> String {
    let now = std::time::SystemTime::now();
    match now.duration_since(t) {
        Ok(d) => {
            let secs = d.as_secs();
            if secs < 1 {
                "just now".to_string()
            } else if secs < 60 {
                format!("{secs}s ago")
            } else if secs < 3600 {
                format!("{}m ago", secs / 60)
            } else if secs < 86400 {
                format!("{}h ago", secs / 3600)
            } else {
                format!("{}d ago", secs / 86400)
            }
        }
        // Clock went backwards — treat as fresh so we don't render
        // nonsense negative durations.
        Err(_) => "just now".to_string(),
    }
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
            if status.checking_reachability {
                Span::styled("checking…", Style::default().fg(Color::Cyan))
            } else {
                match status.reachability {
                    Reachability::Online => {
                        Span::styled("online", Style::default().fg(Color::Green))
                    }
                    Reachability::Offline => {
                        Span::styled("offline", Style::default().fg(Color::Red))
                    }
                    Reachability::Unknown => {
                        Span::styled("unknown", Style::default().fg(Color::DarkGray))
                    }
                }
            },
        ]),
        // "last up" row: anchor the reachability badge to a wall-clock
        // timestamp so the user knows how stale it is after a refresh.
        // Hidden until we have actually seen the host up at least once
        // this session (otherwise it would claim "never" for hosts we
        // just haven't probed yet).
        Line::from(vec![
            Span::styled("last up  ", Style::default().fg(Color::DarkGray)),
            match status.last_online {
                Some(t) => Span::styled(
                    format_time_ago(t),
                    Style::default().fg(Color::Green),
                ),
                None => Span::styled("(never seen)", Style::default().fg(Color::DarkGray)),
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
        // "last     " row: persistent finished/failed indicator scoped
        // to *this* host. The title bar shows the global last deploy;
        // the details pane has to be per-host so navigating between
        // hosts doesn't make a previous host's outcome appear to belong
        // to whichever host the cursor lands on. We render the same
        // chip the title bar uses so the visual contract stays
        // single-source.
        match (&app.busy_label, app.last_deploys.get(&node.name)) {
            (Some(busy), _) => Line::from(vec![
                Span::styled("last     ", Style::default().fg(Color::DarkGray)),
                Span::styled(format!("⟳ {busy}"), Style::default().fg(Color::Yellow)),
            ]),
            (None, Some(last)) => Line::from(vec![
                Span::styled("last     ", Style::default().fg(Color::DarkGray)),
                deploy_outcome_chip(last),
            ]),
            (None, None) => Line::from(vec![
                Span::styled("last     ", Style::default().fg(Color::DarkGray)),
                Span::styled("(no deploy this session)", Style::default().fg(Color::DarkGray)),
            ]),
        },
    ];
    if let Some(err) = &status.last_error {
        lines.push(Line::from(vec![
            Span::styled("error    ", Style::default().fg(Color::Red)),
            Span::raw(err.clone()),
        ]));
    }

    frame.render_widget(Paragraph::new(lines).wrap(Wrap { trim: false }), area);
}

/// Palette used to colour host prefixes in the job-log pane. Chosen
/// to be distinct on a dark terminal background and to avoid the
/// colours already reserved by the rest of the UI (cyan = focus,
/// magenta = title chip, red/yellow = errors/busy).
const JOB_LOG_COLORS: &[Color] = &[
    Color::LightBlue,
    Color::LightGreen,
    Color::LightYellow,
    Color::LightMagenta,
    Color::LightCyan,
    Color::LightRed,
];

/// Stable host→colour mapping. Uses a tiny FNV-1a hash on the host
/// name so the same host always gets the same colour across frames
/// (and across runs). The palette is small enough that collisions are
/// expected for large fleets — that's fine, it's a rough guide.
fn job_log_color(host: &str) -> Color {
    let mut hash: u32 = 2166136261;
    for b in host.as_bytes() {
        hash ^= *b as u32;
        hash = hash.wrapping_mul(16777619);
    }
    JOB_LOG_COLORS[(hash as usize) % JOB_LOG_COLORS.len()]
}

/// Longest host name among tagged log entries, used to align the
/// prefix column in `draw_job_log`. Capped so a 40-char hostname
/// doesn't eat the whole pane.
fn job_log_prefix_width(app: &App) -> usize {
    const MAX: usize = 14;
    app.log
        .iter()
        .filter_map(|e| e.host.as_deref().map(str::len))
        .max()
        .unwrap_or(0)
        .min(MAX)
}

/// Right-column job log. This is where the actual `deploy` stdout
/// lands — every tagged line (single-host or batch) with a coloured
/// host prefix so interleaved output stays legible. Untagged lines
/// (status messages, reachability banners) stay in the details pane;
/// this pane is for the running job specifically.
///
/// The pane is always drawn (empty-state message when no deploy has
/// run yet) and always focusable, so the user can Tab or `l` to it
/// before kicking off a job and then scroll once output starts.
fn draw_job_log(frame: &mut Frame, area: Rect, app: &mut App) {
    let focused = app.focus == FocusPane::JobLog;
    // Pane title gets a `[/query]` chip when a job-log search is
    // active so the user can see what they're filtering on without
    // looking at the input strip, plus a `[↑N]` chip when the user
    // has scrolled back from the tail.
    let mut title_spans = pane_title_spans("job log", 'v', focused);
    if let (Some(q), Some(crate::app::SearchTarget::JobLog)) =
        (app.log_search.as_ref(), app.log_search_target)
    {
        let (cur, total) = app.log_search_stats(crate::app::SearchTarget::JobLog);
        title_spans.push(search_chip(q, cur, total));
    }
    if app.job_log_scroll > 0 {
        title_spans.push(scroll_chip(app.job_log_scroll));
    }

    let width = job_log_prefix_width(app);
    let tagged: Vec<&crate::app::LogEntry> =
        app.log.iter().filter(|e| e.host.is_some()).collect();

    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(focus_border_style(focused))
        .title(Line::from(title_spans));
    let inner = block.inner(area);
    frame.render_widget(block, area);

    if tagged.is_empty() {
        let empty = Line::styled(
            " (no deploy output yet — press s / b / d to start)",
            Style::default().fg(Color::DarkGray),
        );
        frame.render_widget(Paragraph::new(empty), inner);
        return;
    }

    // Clamp entry-based scroll so holding `k` past the oldest entry
    // doesn't accumulate phantom offset that later has to be unwound
    // with `j`. The max is "all but one entry scrolled off" because
    // wrapping makes a strict row-anchored max hard to compute here
    // — `scroll = total - 1` guarantees the oldest entry is still
    // reachable from the current row offset.
    let total = tagged.len();
    let max_entry_scroll = total.saturating_sub(1);
    if app.job_log_scroll > max_entry_scroll {
        app.job_log_scroll = max_entry_scroll;
    }
    let scroll = app.job_log_scroll;

    let query = if matches!(app.log_search_target, Some(crate::app::SearchTarget::JobLog)) {
        app.log_search.as_deref()
    } else {
        None
    };

    // Build every filtered line up-front. Rendering the full set (and
    // scrolling into it) is what fixes the wrap-clip bug: the earlier
    // version picked `height` entries and let Paragraph clip the
    // overflow from the bottom, so long `[pkg] listing …` lines would
    // push the tail off-screen even with `scroll == 0`.
    let all_lines: Vec<Line> = tagged
        .iter()
        .map(|entry| {
            let host = entry.host.as_deref().unwrap_or("");
            let pad = width.saturating_sub(host.len());
            let color = job_log_color(host);
            let prefix = format!("{host}{} │ ", " ".repeat(pad));
            let body_style = if entry.is_err {
                Style::default().fg(Color::Red)
            } else {
                Style::default()
            };
            let mut spans = vec![Span::styled(
                prefix,
                Style::default().fg(color).add_modifier(Modifier::BOLD),
            )];
            spans.extend(highlight_match(&entry.text, query, body_style));
            Line::from(spans)
        })
        .collect();

    let visible = inner.height as usize;
    let (y_offset, _max_row_offset) =
        compute_tail_scroll_offset(&all_lines, scroll, inner.width, visible);

    frame.render_widget(
        Paragraph::new(all_lines)
            .wrap(Wrap { trim: false })
            .scroll((y_offset, 0)),
        inner,
    );
}

/// Turn an *entry-based* scroll offset into a *row-based* paragraph
/// scroll offset so wrapped content anchors against the bottom of
/// `inner`. Shared by both log panes. Returns `(y_offset, max_row_offset)`
/// — the second component is handed back so the caller can clamp user
/// state against it if desired.
///
/// The trick: we measure the total wrapped-row count of the full
/// line list via `Paragraph::line_count(width)`, and also measure
/// how many rows the "scrolled-off" tail entries would occupy (the
/// last `scroll` entries). Subtracting both from `max_row_offset`
/// gives a paragraph-scroll value that places the tail flush with
/// the bottom edge when `scroll == 0`.
fn compute_tail_scroll_offset(
    all_lines: &[Line<'_>],
    scroll: usize,
    width: u16,
    visible: usize,
) -> (u16, usize) {
    if all_lines.is_empty() || width == 0 {
        return (0, 0);
    }
    let total_rows = Paragraph::new(all_lines.to_vec())
        .wrap(Wrap { trim: false })
        .line_count(width);
    let max_row_offset = total_rows.saturating_sub(visible);
    let row_scroll = if scroll == 0 {
        0
    } else {
        let tail_start = all_lines.len().saturating_sub(scroll);
        Paragraph::new(all_lines[tail_start..].to_vec())
            .wrap(Wrap { trim: false })
            .line_count(width)
    };
    let y = max_row_offset.saturating_sub(row_scroll);
    (y.min(u16::MAX as usize) as u16, max_row_offset)
}

fn draw_log(frame: &mut Frame, area: Rect, app: &mut App) {
    // The details-pane log shows app-level status messages. Untagged
    // lines (global state like reachability sweeps) are always
    // visible; host-tagged lines only show when that host is the
    // selected one — so moving between hosts swaps the pane rather
    // than revealing stale messages from the previously-highlighted
    // host. Actual deploy-rs stdout lives in the job log pane and
    // never reaches this filter.
    let selected_name = app.selected_node().map(|n| n.name.to_string());
    let filtered: Vec<&crate::app::LogEntry> = app
        .log
        .iter()
        .filter(|e| match (e.host.as_deref(), selected_name.as_deref()) {
            (None, _) => true,
            (Some(h), Some(sel)) => h == sel,
            (Some(_), None) => false,
        })
        .collect();

    // Clamp entry-based scroll against an entry-count cap so holding
    // `k` past the oldest entry doesn't accumulate phantom offset —
    // same rationale as `draw_job_log`. The row-count math for the
    // actual bottom-anchored offset happens below in
    // `compute_tail_scroll_offset`.
    let total = filtered.len();
    let max_entry_scroll = total.saturating_sub(1);
    if app.log_scroll > max_entry_scroll {
        app.log_scroll = max_entry_scroll;
    }
    let scroll = app.log_scroll;
    let query = if matches!(
        app.log_search_target,
        Some(crate::app::SearchTarget::DetailsLog)
    ) {
        app.log_search.as_deref()
    } else {
        None
    };
    if filtered.is_empty() {
        return;
    }
    // Build every line up-front so we can render the full paragraph
    // and use row-exact scroll. Picking `height` entries by count and
    // letting the Paragraph clip the overflow was dropping the tail
    // whenever long `[pkg] listing …` lines wrapped, so the details
    // log was missing its newest rows despite `scroll == 0`.
    let all_lines: Vec<Line> = filtered
        .iter()
        .map(|entry| {
            let style = if entry.is_err {
                Style::default().fg(Color::Red)
            } else {
                Style::default()
            };
            Line::from(highlight_match(&entry.text, query, style))
        })
        .collect();
    let visible = area.height as usize;
    let (y_offset, _max_row_offset) =
        compute_tail_scroll_offset(&all_lines, scroll, area.width, visible);
    frame.render_widget(
        Paragraph::new(all_lines)
            .wrap(Wrap { trim: false })
            .scroll((y_offset, 0)),
        area,
    );
}

/// Split `text` into spans, highlighting every occurrence of `query`
/// (when present) with a yellow background. The non-matching segments
/// keep `base_style`. When `query` is `None` or empty we return a
/// single span with `base_style` so callers don't have to special-case.
fn highlight_match(
    text: &str,
    query: Option<&str>,
    base_style: Style,
) -> Vec<Span<'static>> {
    let Some(q) = query.filter(|q| !q.is_empty()) else {
        return vec![Span::styled(text.to_string(), base_style)];
    };
    let hi = base_style
        .bg(Color::Magenta)
        .fg(Color::Black)
        .add_modifier(Modifier::BOLD);
    let mut spans = Vec::new();
    let mut cursor = 0;
    let qlen = q.len();
    while let Some(found) = text[cursor..].find(q) {
        let start = cursor + found;
        if start > cursor {
            spans.push(Span::styled(text[cursor..start].to_string(), base_style));
        }
        spans.push(Span::styled(text[start..start + qlen].to_string(), hi));
        cursor = start + qlen;
    }
    if cursor < text.len() {
        spans.push(Span::styled(text[cursor..].to_string(), base_style));
    }
    spans
}

/// Pane-title chip rendered next to a log pane label when the user
/// has scrolled back from the tail. Replaces the in-log "scrolled ↑N"
/// banner that used to sit at the top of the visible window —
/// rendering that banner inside the paragraph confused the row-exact
/// tail anchoring, so it moved to the title instead. The yellow
/// background matches the focused-pane palette.
fn scroll_chip(scroll: usize) -> Span<'static> {
    Span::styled(
        format!("[↑{scroll}] "),
        Style::default()
            .fg(Color::Black)
            .bg(Color::Yellow)
            .add_modifier(Modifier::BOLD),
    )
}

/// Pane-title chip rendered next to a log pane label when that pane
/// has an active committed search query. Includes `[current/total]`
/// match counters when any match exists in the filtered view, so the
/// user can see at a glance whether `n`/`Shift+N` will actually move
/// them anywhere. Compact so it doesn't push the title off the right
/// edge of narrow terminals.
fn search_chip(query: &str, current: usize, total: usize) -> Span<'static> {
    let label = if total > 0 {
        format!("[/{} {}/{}] ", query, current, total)
    } else {
        format!("[/{} 0/0] ", query)
    };
    Span::styled(
        label,
        Style::default()
            .fg(Color::Black)
            .bg(Color::Magenta)
            .add_modifier(Modifier::BOLD),
    )
}

fn draw_toggles_strip(frame: &mut Frame, area: Rect, app: &App) {
    let focused = app.focus == FocusPane::Toggles;
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(focus_border_style(focused))
        .title(Line::from(pane_title_spans("toggles", 't', focused)));
    let inner = block.inner(area);
    frame.render_widget(block, area);
    let spans = build_toggles_spans(app, focused);
    frame.render_widget(
        Paragraph::new(Line::from(spans)).wrap(Wrap { trim: false }),
        inner,
    );
}

/// Build the full span list for the toggles strip. Pulled out of
/// `draw_toggles_strip` so `toggles_content_width` can measure the
/// exact same content the renderer is about to draw — otherwise the
/// adaptive-height decision in `draw()` could disagree with what
/// actually gets rendered and bump the strip to 2 inner lines for
/// content that fits on one (or vice versa).
fn build_toggles_spans(app: &App, focused: bool) -> Vec<Span<'static>> {
    let t = app.toggles;
    // When the toggles pane has focus, the currently-navigated toggle
    // gets a reverse-video highlight so the user knows which one
    // Enter will flip.
    let sub = if focused { Some(app.toggle_index) } else { None };
    let values = [
        ("1", "skip-checks", t.skip_checks),
        ("2", "magic-rb", t.magic_rollback),
        ("3", "auto-rb", t.auto_rollback),
        ("4", "remote-build", t.remote_build),
        ("5", "int-sudo", t.interactive_sudo),
    ];
    debug_assert_eq!(values.len(), TOGGLE_COUNT);
    let mut spans = Vec::with_capacity(values.len() * 2 + 1);
    spans.push(Span::raw(" "));
    for (i, (key, label, on)) in values.iter().enumerate() {
        if i > 0 {
            spans.push(Span::raw("  "));
        }
        spans.push(toggle_span(key, label, *on, sub == Some(i)));
    }
    spans
}

/// Rendered width (in display columns) of the toggles strip content.
/// We ignore focus-dependent styling because the widths are the same
/// with or without focus — only the colours change.
fn toggles_content_width() -> usize {
    // Values deliberately match `build_toggles_spans`'s label list;
    // if a label ever grows here, bump it there too.
    let labels = ["skip-checks", "magic-rb", "auto-rb", "remote-build", "int-sudo"];
    // Each toggle renders as ` <key>:<icon> <label> ` = 6 fixed chars
    // + label; plus a 2-char separator between toggles and a leading
    // space.
    let per_toggle: usize = labels.iter().map(|l| 6 + l.len()).sum();
    let separators = 2 * (labels.len() - 1);
    1 + per_toggle + separators
}

fn toggle_span(key: &str, label: &str, on: bool, focused: bool) -> Span<'static> {
    // The indicator dot carries the on/off signal in colour. When
    // focused, the chip gets a darker (grey) background — distinct
    // from the cyan/black background that hosts use for their
    // selection. The intent: hosts persist their selection while the
    // user moves between panes, so they "own" the strong highlight;
    // toggles and commands are transient cursors and shouldn't
    // compete visually with that.
    let icon = if on { "●" } else { "○" };
    let style = if focused {
        Style::default()
            .fg(Color::White)
            .bg(Color::DarkGray)
            .add_modifier(Modifier::BOLD)
    } else if on {
        Style::default().fg(Color::Green)
    } else {
        Style::default()
    };
    Span::styled(format!(" {key}:{icon} {label} "), style)
}

/// Bottom commands row: bordered box with a left-side information hint
/// column and a right-side navigable command button row. Info holds
/// the non-command hints (select, mark, toggles, focus, help, quit);
/// commands holds the per-action buttons. Each half borders and
/// titles independently so focus lights up the commands pane without
/// lighting up the info pane.
///
/// The info column is **context-aware**: its hint list changes based
/// on `app.focus`, so `j/k` in Hosts says "move selection" while in
/// Details it says "scroll" and surfaces `g/G` and `/` instead. This
/// mirrors the way the pane-specific keys actually behave.
fn draw_commands_row(frame: &mut Frame, area: Rect, app: &App) {
    let cols = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(40), Constraint::Percentage(60)])
        .split(area);

    // Left: informational hints. Never takes focus.
    let info_block = Block::default().borders(Borders::ALL).title(" info ");
    let info_inner = info_block.inner(cols[0]);
    frame.render_widget(info_block, cols[0]);
    let info_spans = build_info_spans(app);
    frame.render_widget(
        Paragraph::new(Line::from(info_spans)).wrap(Wrap { trim: false }),
        info_inner,
    );

    // Right: navigable command buttons.
    let focused = app.focus == FocusPane::Commands;
    let cmd_block = Block::default()
        .borders(Borders::ALL)
        .border_style(focus_border_style(focused))
        .title(Line::from(pane_title_spans("commands", 'c', focused)));
    let cmd_inner = cmd_block.inner(cols[1]);
    frame.render_widget(cmd_block, cols[1]);

    let spans = build_commands_spans(app, focused);
    frame.render_widget(
        Paragraph::new(Line::from(spans)).wrap(Wrap { trim: false }),
        cmd_inner,
    );
}

/// Build the info-column hint spans for whichever pane currently has
/// focus. Each variant returns a `(key, description)` list rendered
/// the same way (yellow key + plain description), keeping the visual
/// style consistent no matter what's focused.
///
/// Kept paired with `info_content_width` below — if you add or
/// remove a hint here, the measurement function needs to match or
/// the adaptive-height decision in `draw()` will disagree with the
/// actual rendered width.
fn build_info_spans(app: &App) -> Vec<Span<'static>> {
    let hints: Vec<(&'static str, &'static str)> = info_hints_for(app);
    let yellow = Style::default().fg(Color::Yellow);
    let mut spans = Vec::with_capacity(hints.len() * 3 + 1);
    spans.push(Span::raw(" "));
    for (i, (key, desc)) in hints.iter().enumerate() {
        if i > 0 {
            spans.push(Span::raw("  "));
        }
        spans.push(Span::styled(*key, yellow));
        spans.push(Span::raw(format!(" {desc}")));
    }
    spans
}

/// The list of `(key, description)` pairs for the focused pane.
/// Pulled out of `build_info_spans` so both the renderer and the
/// width-measurer walk the exact same list.
fn info_hints_for(app: &App) -> Vec<(&'static str, &'static str)> {
    let search_active_here = match (app.focus, app.log_search_target, &app.log_search) {
        (FocusPane::Details, Some(crate::app::SearchTarget::DetailsLog), Some(_)) => true,
        (FocusPane::JobLog, Some(crate::app::SearchTarget::JobLog), Some(_)) => true,
        _ => false,
    };
    match app.focus {
        FocusPane::Hosts => vec![
            ("j/k", "move"),
            ("Space", "mark"),
            ("g/G", "top/bottom"),
            ("Tab", "focus"),
            ("?", "help"),
            ("q", "quit"),
        ],
        FocusPane::Details | FocusPane::JobLog => {
            let mut v: Vec<(&'static str, &'static str)> = vec![
                ("j/k", "scroll"),
                ("g/G", "top/tail"),
                ("/", "search"),
            ];
            if search_active_here {
                v.push(("n/N", "next/prev"));
                v.push(("Esc", "clear"));
            }
            v.push(("Tab", "focus"));
            v.push(("?", "help"));
            v.push(("q", "quit"));
            v
        }
        FocusPane::Toggles => vec![
            ("h/l", "move"),
            ("Enter", "flip"),
            ("1-5", "direct"),
            ("Tab", "focus"),
            ("?", "help"),
            ("q", "quit"),
        ],
        FocusPane::Commands => vec![
            ("h/l", "move"),
            ("Enter", "run"),
            ("Tab", "focus"),
            ("?", "help"),
            ("q", "quit"),
        ],
    }
}

/// Rendered width (in display columns) of the context-aware info
/// line for the currently focused pane. Mirrors the formatting in
/// `build_info_spans`: 1 leading space, then for each hint a
/// 2-char separator (after the first), the key, a space, and the
/// description.
fn info_content_width(app: &App) -> usize {
    let hints = info_hints_for(app);
    let mut w = 1; // leading space
    for (i, (key, desc)) in hints.iter().enumerate() {
        if i > 0 {
            w += 2; // "  " separator
        }
        w += key.len() + 1 + desc.len();
    }
    w
}

/// Build the right-hand commands button row. Pulled out of
/// `draw_commands_row` for the same width-measurement reason as
/// `build_toggles_spans`.
fn build_commands_spans(app: &App, focused: bool) -> Vec<Span<'static>> {
    let sub = if focused { Some(app.command_index) } else { None };
    let mut spans: Vec<Span> = Vec::with_capacity(COMMANDS.len() * 3 + 1);
    spans.push(Span::raw(" "));
    for (i, (_cmd, key, label)) in COMMANDS.iter().enumerate() {
        if i > 0 {
            spans.push(Span::raw(" "));
        }
        spans.extend(command_button(key, label, sub == Some(i)));
    }
    spans
}

/// Rendered width of the commands-button row. `command_button`
/// always emits `" <key>:<label> "` = 3 fixed chars + key + label,
/// separated by a single space between buttons, with one leading
/// space.
fn commands_content_width() -> usize {
    let per_button: usize = COMMANDS
        .iter()
        .map(|(_cmd, key, label)| 3 + key.len() + label.len())
        .sum();
    let separators = COMMANDS.len().saturating_sub(1); // 1-char spaces
    1 + per_button + separators
}

/// Two-span command button: the key in yellow (same colour as the
/// info pane hints) and the label in default text. When focused all
/// three spans share a single grey background — distinct from the
/// cyan/black highlight on the focused host row, because the host
/// selection persists across pane focus changes while the command
/// cursor is transient and shouldn't compete with it.
fn command_button(key: &'static str, label: &'static str, focused: bool) -> [Span<'static>; 3] {
    let key_style;
    let sep_style;
    let label_style;
    if focused {
        let bg = Color::DarkGray;
        key_style = Style::default()
            .fg(Color::Yellow)
            .bg(bg)
            .add_modifier(Modifier::BOLD);
        sep_style = Style::default().fg(Color::Gray).bg(bg);
        label_style = Style::default()
            .fg(Color::White)
            .bg(bg)
            .add_modifier(Modifier::BOLD);
    } else {
        key_style = Style::default().fg(Color::Yellow);
        sep_style = Style::default().fg(Color::DarkGray);
        label_style = Style::default();
    }
    [
        Span::styled(format!(" {key}"), key_style),
        Span::styled(":", sep_style),
        Span::styled(format!("{label} "), label_style),
    ]
}


/// Bottom input strip: renders prompt text when the user is mid-input
/// (override menu, edit field, confirm popup, etc.) and is left blank
/// in Normal mode — the commands row above already carries every
/// informational hint the old cheat sheet used to show.
fn draw_input_strip(frame: &mut Frame, area: Rect, app: &App) {
    let line = match &app.input {
        InputMode::ConfirmDeploy { .. } => Line::from(vec![
            Span::styled(
                " confirm ▸ ",
                Style::default()
                    .fg(Color::Black)
                    .bg(Color::Yellow)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::raw("  "),
            Span::styled("y", Style::default().fg(Color::Yellow)),
            Span::raw(" / "),
            Span::styled("Enter", Style::default().fg(Color::Yellow)),
            Span::raw(" confirm  "),
            Span::styled("n", Style::default().fg(Color::Yellow)),
            Span::raw(" / "),
            Span::styled("Esc", Style::default().fg(Color::Yellow)),
            Span::raw(" cancel"),
        ]),
        InputMode::EditIdentityPicker { .. } => Line::from(vec![
            Span::styled(
                " identity ▸ ",
                Style::default()
                    .fg(Color::Black)
                    .bg(Color::Magenta)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::raw("  "),
            Span::styled("Ctrl+J/K", Style::default().fg(Color::Yellow)),
            Span::raw(" pick  "),
            Span::styled("type", Style::default().fg(Color::Yellow)),
            Span::raw(" custom path  "),
            Span::styled("Enter", Style::default().fg(Color::Yellow)),
            Span::raw(" save  "),
            Span::styled("Esc", Style::default().fg(Color::Yellow)),
            Span::raw(" cancel"),
        ]),
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
        InputMode::SearchLog { target, buf } => {
            let label = match target {
                crate::app::SearchTarget::DetailsLog => " /search details ▸ ",
                crate::app::SearchTarget::JobLog => " /search job log ▸ ",
            };
            Line::from(vec![
                Span::styled(
                    label,
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
                Span::raw(" commit  "),
                Span::styled("Esc", Style::default().fg(Color::Yellow)),
                Span::raw(" cancel"),
            ])
        }
        InputMode::SearchHelp { buf } => Line::from(vec![
            Span::styled(
                " /filter help ▸ ",
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
            Span::raw(" commit  "),
            Span::styled("Esc", Style::default().fg(Color::Yellow)),
            Span::raw(" clear"),
        ]),
        // Normal mode has nothing extra to say — the commands row
        // above already surfaces every hint.
        InputMode::Normal => Line::raw(""),
    };
    frame.render_widget(Paragraph::new(line), area);
}

/// Centered help popup. We use ratatui's `Clear` widget to wipe the
/// underlying area before drawing, so the popup looks like a real
/// modal instead of overlapping the host list.
///
/// `scroll` is a `&mut` to the user's current offset into the
/// content. We clamp it in-place against the actual rendered length
/// so a held `j` past the bottom (or `k` past the top) cannot
/// accumulate phantom offset that the user would then have to grind
/// back through with the opposite key.
fn draw_help_popup(frame: &mut Frame, area: Rect, app: &mut App) {
    let popup = centered_rect(82, 86, area);
    frame.render_widget(Clear, popup);

    // Title swaps in a `/` reminder when no filter is active so the
    // user discovers the feature.
    let title = if app.help_search.is_some()
        || matches!(app.input, InputMode::SearchHelp { .. })
    {
        " help — ? / Esc close · / filter ".bold()
    } else {
        " help — ? / Esc close · j/k scroll · / filter ".bold()
    };
    let block = Block::default().borders(Borders::ALL).title(title);
    let inner = block.inner(popup);
    frame.render_widget(block, popup);

    let dim = Style::default().fg(Color::DarkGray);

    // Every entry in the popup uses `key_line` so the description column
    // is at the same x for every row in every section. This is what the
    // earlier hand-padded version got wrong — different sections used
    // different widths and the SSH-overrides parent/child indent looked
    // misaligned.
    let all_lines: Vec<Line> = vec![
        section("navigation"),
        key_line("↑/↓ j/k", "within pane: hosts = selection, details/joblog = scroll"),
        key_line("←/→ h/l", "toggles/commands = sub-cursor (vim-style hjkl)"),
        key_line("Shift+H/L", "horizontal pane move (hosts ↔ details ↔ job log)"),
        key_line("Shift+←/→", "same as Shift+H/L"),
        key_line("Shift+J/K", "vertical pane move (toggles ↔ middle ↔ commands)"),
        key_line("Shift+↑/↓", "same as Shift+J/K"),
        key_line("Tab", "cycle focus forward (toggles → hosts → details → joblog → commands)"),
        key_line("Shift+Tab", "cycle focus backward"),
        key_line("f/i/v/t/c", "btop-style jump: (f)ocus hosts, (i)nfo details, (v)iew job log, (t)oggles, (c)ommands"),
        key_line("Enter", "activate the focused toggle or command button"),
        key_line(
            "g",
            "vim-style 'go to top' — hosts=first host, details/joblog=oldest line, help=top",
        ),
        key_line("Shift+G", "vim-style 'go to bottom' — details/joblog snap to tail, help=bottom"),
        key_line("q", "quit (Esc closes popups/edits but never quits)"),
        key_line("Ctrl-C", "quit and kill any running deploy"),
        Line::raw(""),

        section("search"),
        key_line(
            "/",
            "open the search prompt (details/job log = match nav, help popup = filter)",
        ),
        key_line(
            "n / Shift+N",
            "next / previous match in the focused log pane (after committing /)",
        ),
        key_line(
            "Esc",
            "in log pane: clear the committed search. While typing: abort the prompt.",
        ),
        Line::raw(""),

        section("status"),
        key_line("r", "refresh online/offline (TCP-22 probe) for every host"),
        key_line(
            "u",
            "check selected host (cheap tier: paths, activation time)",
        ),
        key_line(
            "Shift+U",
            "medium tier: closure size delta (needs prior u)",
        ),
        key_line(
            "p",
            "package diff: per-name version changes via metadata-only requisites query (needs prior u)",
        ),
        Line::from(Span::styled(
            "              badges: ✓ up-to-date   ↑ behind   ! error   ? unchecked   - n/a   ⠋ checking",
            dim,
        )),
        Line::raw(""),

        section("deploy"),
        key_line(
            "a / y / h",
            "target all profiles / system (NixOS, sYs) / home (home-manager)",
        ),
        key_line("s", "switch — apply now (asks for confirmation)"),
        key_line(
            "b",
            "boot — install as next boot entry, don't activate now (asks for confirmation)",
        ),
        key_line("d", "dry-run — `deploy --dry-activate`, build + diff only"),
        key_line("x", "cancel running deploy AND drop any queued hosts (SIGKILL the child)"),
        Line::raw(""),

        section("multi-select / batch"),
        key_line("Space", "mark or unmark the highlighted host (the [+] column lights up)"),
        key_line("Shift+A", "mark every host"),
        key_line("Shift+X", "clear all marks"),
        Line::from(Span::styled(
            "              when any host is marked, s/b/d operate on the marked set",
            dim,
        )),
        Line::from(Span::styled(
            "              in insertion order; failures stop the queue, x cancels the rest",
            dim,
        )),
        Line::raw(""),

        section("toggles (number keys)"),
        key_line("1", "skip-checks — skip the pre-deploy `nix flake check`"),
        key_line(
            "2",
            "magic-rollback — wait for confirmation, auto-roll-back on timeout (default ON)",
        ),
        key_line("3", "auto-rollback — roll back if activation fails (default ON)"),
        key_line("4", "remote-build — perform the build on the target host"),
        key_line(
            "5",
            "interactive-sudo — prompt for sudo password (will hang the TUI)",
        ),
        Line::raw(""),

        section("ssh overrides (per host)"),
        key_line("o", "open the overrides menu for the selected host"),
        key_line("o → h", "set hostname / IP override"),
        key_line("o → u", "set ssh user override"),
        key_line(
            "o → k",
            "pick identity file (Ctrl+J/K to scroll list, type to enter custom path)",
        ),
        key_line(
            "o → o",
            "set extra ssh -o opts (whitespace-separated, e.g. `Port=2222`)",
        ),
        key_line("o → c", "clear all overrides for this host"),
        Line::from(Span::styled(
            "              hosts with active overrides show a magenta [ssh] tag in the list",
            dim,
        )),
    ];

    // Lazygit-style filter: the live buffer (while typing) takes
    // precedence over the committed query so the popup updates as the
    // user types. Empty buffer = show everything.
    let live_query: Option<String> = match &app.input {
        InputMode::SearchHelp { buf } if !buf.is_empty() => Some(buf.clone()),
        _ => app.help_search.clone(),
    };

    // Reserve one row at the bottom for the inline search prompt
    // whenever filtering is active OR the user is mid-type. Keeps the
    // input visible inside the popup itself rather than only on the
    // app-level input strip.
    let show_search_row = matches!(app.input, InputMode::SearchHelp { .. })
        || app.help_search.is_some();
    let (content_area, search_area) = if show_search_row {
        let rows = Layout::default()
            .direction(Direction::Vertical)
            .constraints([Constraint::Min(1), Constraint::Length(1)])
            .split(inner);
        (rows[0], Some(rows[1]))
    } else {
        (inner, None)
    };

    let lines: Vec<Line> = if let Some(q) = live_query.as_deref() {
        // Filter section-and-detail rows by substring match. Section
        // headers themselves don't get hidden by the filter — they
        // anchor the surviving rows under the right header. Blank
        // spacer lines also stay so the layout doesn't collapse.
        all_lines
            .into_iter()
            .filter(|line| {
                let text: String =
                    line.spans.iter().map(|s| s.content.as_ref()).collect();
                text.is_empty() || text.contains(q)
            })
            .collect()
    } else {
        all_lines
    };

    // Clamp scroll against the actual content length so j/k can't
    // reveal a blank popup once the user has passed the last line.
    // We mutate the App-owned scroll in place so a held key past the
    // end doesn't accumulate phantom offset that the user has to
    // grind back through with the opposite key.
    let total = lines.len() as u16;
    let visible = content_area.height;
    let max_scroll = total.saturating_sub(visible);
    if app.help_scroll > max_scroll {
        app.help_scroll = max_scroll;
    }
    frame.render_widget(
        Paragraph::new(lines)
            .wrap(Wrap { trim: false })
            .scroll((app.help_scroll, 0)),
        content_area,
    );

    // Bottom row: inline search bar. Shows the live buffer while the
    // user is typing, or the committed query (greyed out) when a
    // filter is in effect but not currently being edited.
    if let Some(rect) = search_area {
        let body = match &app.input {
            InputMode::SearchHelp { buf } => vec![
                Span::styled(
                    " /",
                    Style::default()
                        .fg(Color::Black)
                        .bg(Color::Yellow)
                        .add_modifier(Modifier::BOLD),
                ),
                Span::raw(" "),
                Span::raw(buf.clone()),
                Span::styled("▎", Style::default().fg(Color::Yellow)),
            ],
            _ => vec![
                Span::styled(" /", Style::default().fg(Color::DarkGray)),
                Span::raw(" "),
                Span::styled(
                    app.help_search.clone().unwrap_or_default(),
                    Style::default().fg(Color::DarkGray),
                ),
                Span::styled(
                    "  (press / to edit, Esc to clear)",
                    Style::default().fg(Color::DarkGray),
                ),
            ],
        };
        frame.render_widget(Paragraph::new(Line::from(body)), rect);
    }
}

/// Section header in the help popup. Bold cyan, with a blank-spacer
/// convention enforced by the call site.
fn section(name: &str) -> Line<'static> {
    Line::styled(
        name.to_string(),
        Style::default()
            .fg(Color::Cyan)
            .add_modifier(Modifier::BOLD),
    )
}

/// One row in the help popup: a fixed-width key column followed by a
/// description. The padding is computed in display columns so different
/// chord lengths (e.g. `Tab` vs `o → h`) all line up.
const HELP_KEY_COL: usize = 12;

fn key_line(keys: &str, desc: &str) -> Line<'static> {
    // 2-space indent, then `keys`, then enough spaces to reach
    // HELP_KEY_COL, then the description. Visible width is what matters
    // because every char in `keys` is single-width ASCII or one of `→ ↑↓`.
    let visible = keys.chars().count();
    let pad = HELP_KEY_COL.saturating_sub(visible);
    let key_col = format!("  {keys}{}", " ".repeat(pad));
    Line::from(vec![
        Span::styled(
            key_col,
            Style::default()
                .fg(Color::Yellow)
                .add_modifier(Modifier::BOLD),
        ),
        Span::raw(desc.to_string()),
    ])
}

/// Confirmation popup for s/b/d. Lists every host that will be touched
/// (sized to a sane window with overflow indication), the mode, and the
/// profile, then waits for `y`/`n`. The popup is intentionally yellow
/// so it reads as a "are you sure?" rather than a passive info dialog.
fn draw_confirm_popup(
    frame: &mut Frame,
    area: Rect,
    hosts: &[String],
    mode: Mode,
    profile: ProfileSel,
) {
    // Pick a popup size that scales with host count but stays bounded —
    // big batches still fit in a fixed window with a "+N more" tail.
    let popup = centered_rect(60, 60, area);
    frame.render_widget(Clear, popup);

    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(Color::Yellow))
        .title(Span::styled(
            " confirm deploy ",
            Style::default()
                .fg(Color::Yellow)
                .add_modifier(Modifier::BOLD),
        ));
    let inner = block.inner(popup);
    frame.render_widget(block, popup);

    let mut lines: Vec<Line> = Vec::new();

    // Summary header — mode + profile + host count.
    lines.push(Line::from(vec![
        Span::styled("mode    ", Style::default().fg(Color::DarkGray)),
        Span::styled(
            describe_mode(mode),
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        ),
    ]));
    lines.push(Line::from(vec![
        Span::styled("profile ", Style::default().fg(Color::DarkGray)),
        Span::styled(
            describe_profile(profile),
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        ),
    ]));
    lines.push(Line::from(vec![
        Span::styled("hosts   ", Style::default().fg(Color::DarkGray)),
        Span::styled(
            format!("{}", hosts.len()),
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        ),
        Span::raw(if hosts.len() == 1 { " host" } else { " hosts" }),
    ]));
    lines.push(Line::raw(""));

    // Host list. Capped so a 30-host batch doesn't blow past the popup
    // height; the user can read the full list in the host pane behind
    // the popup if they need to verify everything.
    const MAX_LIST: usize = 12;
    let visible = hosts.len().min(MAX_LIST);
    for name in &hosts[..visible] {
        lines.push(Line::from(vec![
            Span::styled("  • ", Style::default().fg(Color::Yellow)),
            Span::raw(name.clone()),
        ]));
    }
    if hosts.len() > MAX_LIST {
        lines.push(Line::from(vec![
            Span::styled("    ", Style::default()),
            Span::styled(
                format!("… +{} more", hosts.len() - MAX_LIST),
                Style::default().fg(Color::DarkGray),
            ),
        ]));
    }
    lines.push(Line::raw(""));
    lines.push(Line::from(vec![
        Span::styled(
            "  y / Enter ",
            Style::default()
                .fg(Color::Black)
                .bg(Color::Green)
                .add_modifier(Modifier::BOLD),
        ),
        Span::raw("  confirm    "),
        Span::styled(
            " n / Esc ",
            Style::default()
                .fg(Color::Black)
                .bg(Color::Red)
                .add_modifier(Modifier::BOLD),
        ),
        Span::raw("  cancel"),
    ]));

    frame.render_widget(Paragraph::new(lines).wrap(Wrap { trim: false }), inner);
}

/// Identity-file picker popup. Renders a scrollable list of scanned
/// `~/.ssh` candidates above a single-line text input. Ctrl+J/K (handled
/// in `app::handle_key_identity_picker`) moves the selection and syncs
/// the buffer; typing freely overrides the buffer with a custom path.
fn draw_identity_picker_popup(
    frame: &mut Frame,
    area: Rect,
    entries: &[std::path::PathBuf],
    selected: usize,
    buf: &str,
) {
    let popup = centered_rect(70, 60, area);
    frame.render_widget(Clear, popup);

    let block = Block::default()
        .borders(Borders::ALL)
        .title(" identity file — Ctrl+J/K pick · type custom · Enter save · Esc cancel ".bold());
    let inner = block.inner(popup);
    frame.render_widget(block, popup);

    // Split: list (fills) + 1-line buffer at the bottom.
    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Min(3), Constraint::Length(2)])
        .split(inner);

    // List items. Empty-state message keeps the layout stable when the
    // user has no scanned keys (e.g. fresh machine, or non-standard ssh
    // dir) — the typed buffer below still works.
    let items: Vec<ListItem> = if entries.is_empty() {
        vec![ListItem::new(Line::styled(
            "  (no keys found in ~/.ssh — type a path below)",
            Style::default().fg(Color::DarkGray),
        ))]
    } else {
        entries
            .iter()
            .enumerate()
            .map(|(i, p)| {
                let style = if i == selected {
                    Style::default()
                        .fg(Color::Black)
                        .bg(Color::Cyan)
                        .add_modifier(Modifier::BOLD)
                } else {
                    Style::default()
                };
                ListItem::new(Line::styled(format!(" {}", p.display()), style))
            })
            .collect()
    };
    frame.render_widget(
        List::new(items).block(Block::default().borders(Borders::BOTTOM).title("keys")),
        rows[0],
    );

    // Text input row. We render an explicit caret so the cursor is
    // visible even though we don't move the terminal cursor here.
    let input_line = Line::from(vec![
        Span::styled(
            " path ▸ ",
            Style::default()
                .fg(Color::Black)
                .bg(Color::Magenta)
                .add_modifier(Modifier::BOLD),
        ),
        Span::raw(" "),
        Span::raw(buf.to_string()),
        Span::styled("▎", Style::default().fg(Color::Magenta)),
    ]);
    frame.render_widget(Paragraph::new(input_line), rows[1]);
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
