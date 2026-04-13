use chrono::Utc;
use ratatui::{
    layout::{Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Paragraph},
    Frame,
};

use crate::app::App;
use crate::provider::{ProviderKind, ProviderStatus, UsageSnapshot};

/// Characters for the gauge bar.
const BAR_FILLED: char = '▓';
const BAR_EMPTY: char = '░';
const BAR_PULSE: char = '█';

pub fn draw(frame: &mut Frame, app: &App) {
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(3),  // title bar
            Constraint::Min(0),    // provider panels
            Constraint::Length(3), // status bar
        ])
        .split(frame.area());

    draw_title_bar(frame, chunks[0], app);
    draw_providers(frame, chunks[1], app);
    draw_status_bar(frame, chunks[2], app);
}

fn draw_title_bar(frame: &mut Frame, area: Rect, app: &App) {
    // Animated spinner: cycles through braille dots
    let spinners = ['⠋', '⠙', '⠹', '⠸', '⠼', '⠴', '⠦', '⠧', '⠇', '⠏'];
    let spinner = if app.paused {
        '⏸'
    } else {
        spinners[(app.tick_count as usize / 2) % spinners.len()]
    };

    let title = Paragraph::new(Line::from(vec![
        Span::styled(
            format!(" {} ", spinner),
            Style::default().fg(Color::Cyan),
        ),
        Span::styled(
            "ClaudeTop",
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled(
            "  AI Provider Dashboard",
            Style::default().fg(Color::DarkGray),
        ),
    ]))
    .block(Block::default().borders(Borders::ALL).border_style(Style::default().fg(Color::DarkGray)));

    frame.render_widget(title, area);
}

fn draw_providers(frame: &mut Frame, area: Rect, app: &App) {
    let count = app.snapshots.len().max(1);
    let constraints: Vec<Constraint> = (0..count)
        .map(|_| Constraint::Ratio(1, count as u32))
        .collect();

    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints(constraints)
        .split(area);

    for (i, snapshot) in app.snapshots.iter().enumerate() {
        draw_provider_panel(frame, chunks[i], snapshot, app.tick_count);
    }
}

/// Format a duration in seconds into a human-readable string.
fn format_duration_short(total_secs: i64) -> String {
    if total_secs <= 0 {
        return "now".to_string();
    }
    let hours = total_secs / 3600;
    let minutes = (total_secs % 3600) / 60;
    if hours > 0 {
        format!("{}h {}m", hours, minutes)
    } else if minutes > 0 {
        format!("{}m", minutes)
    } else {
        format!("{}s", total_secs)
    }
}

/// Pick a title color based on provider kind.
fn provider_color(kind: &ProviderKind) -> Color {
    match kind {
        ProviderKind::Claude => Color::Magenta,
        ProviderKind::Codex => Color::Green,
        ProviderKind::Gemini => Color::Blue,
    }
}

/// Pick a gauge color based on usage ratio, with smooth gradient feel.
fn gauge_color(ratio: f64) -> Color {
    if ratio > 0.9 {
        Color::Red
    } else if ratio > 0.75 {
        Color::Rgb(255, 165, 0) // orange
    } else if ratio > 0.5 {
        Color::Yellow
    } else {
        Color::Green
    }
}

/// Dim version of gauge color for the filled portion background.
fn gauge_color_dim(ratio: f64) -> Color {
    if ratio > 0.9 {
        Color::Rgb(120, 30, 30)
    } else if ratio > 0.75 {
        Color::Rgb(120, 80, 0)
    } else if ratio > 0.5 {
        Color::Rgb(120, 120, 0)
    } else {
        Color::Rgb(0, 80, 0)
    }
}

fn draw_provider_panel(frame: &mut Frame, area: Rect, snapshot: &UsageSnapshot, tick: u64) {
    let title_color = provider_color(&snapshot.provider);

    // Compute reset time string from the earliest reset_at across rate windows.
    let reset_text = snapshot
        .rate_windows
        .iter()
        .filter_map(|w| w.reset_at)
        .min()
        .map(|earliest| {
            let secs = (earliest - Utc::now()).num_seconds();
            if secs > 0 {
                format!("  resets in {}", format_duration_short(secs))
            } else {
                "  reset due".to_string()
            }
        })
        .unwrap_or_default();

    let title = format!(" {} {}", snapshot.provider, reset_text);

    let status_color = match &snapshot.status {
        ProviderStatus::Ok => Color::Green,
        ProviderStatus::Unavailable(_) => Color::Red,
        ProviderStatus::NotConfigured(_) => Color::DarkGray,
    };

    let border_color = if snapshot.status.is_ok() {
        title_color
    } else {
        status_color
    };

    let block = Block::default()
        .title(Span::styled(
            title,
            Style::default()
                .fg(title_color)
                .add_modifier(Modifier::BOLD),
        ))
        .borders(Borders::ALL)
        .border_style(Style::default().fg(border_color));

    let inner = block.inner(area);
    frame.render_widget(block, area);

    // For not-configured or unavailable providers, show hint.
    if !snapshot.status.is_ok() {
        let hint = snapshot.status.label();
        let hint_color = match &snapshot.status {
            ProviderStatus::NotConfigured(_) => Color::DarkGray,
            ProviderStatus::Unavailable(_) => Color::Red,
            _ => Color::White,
        };
        let lines = vec![
            Line::from(""),
            Line::from(Span::styled(
                format!("  {}", hint),
                Style::default()
                    .fg(hint_color)
                    .add_modifier(Modifier::ITALIC),
            )),
            Line::from(""),
            Line::from(Span::styled(
                "  Configure in ~/.config/claudetop/config.toml",
                Style::default().fg(Color::DarkGray),
            )),
        ];
        frame.render_widget(Paragraph::new(lines), inner);
        return;
    }

    // Layout: info line, then one row per gauge, then remaining space.
    let gauge_count = snapshot.rate_windows.len().min(3);
    let panel_chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1),                     // info line
            Constraint::Length(1),                     // spacer
            Constraint::Length(gauge_count as u16),    // gauges
            Constraint::Min(0),                        // remaining
        ])
        .split(inner);

    // Info line: plan + credits + cost
    let plan = snapshot.plan_name.as_deref().unwrap_or("--");
    let credits = snapshot.credits_remaining.as_deref().unwrap_or("--");
    let cost_str = snapshot
        .cost_30d
        .map(|c| format!("${:.2}", c))
        .unwrap_or_else(|| "--".to_string());

    let info_line = Line::from(vec![
        Span::styled(" Plan ", Style::default().fg(Color::DarkGray)),
        Span::styled(
            plan,
            Style::default().fg(Color::White).add_modifier(Modifier::BOLD),
        ),
        Span::raw("   "),
        Span::styled("Credits ", Style::default().fg(Color::DarkGray)),
        Span::styled(credits, Style::default().fg(Color::White)),
        Span::raw("   "),
        Span::styled("30d Cost ", Style::default().fg(Color::DarkGray)),
        Span::styled(
            cost_str,
            Style::default().fg(Color::White).add_modifier(Modifier::BOLD),
        ),
    ]);
    frame.render_widget(Paragraph::new(info_line), panel_chunks[0]);

    // Rate window gauges
    if !snapshot.rate_windows.is_empty() {
        let gauge_constraints: Vec<Constraint> = snapshot
            .rate_windows
            .iter()
            .take(3)
            .map(|_| Constraint::Length(1))
            .collect();

        let gauge_chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints(gauge_constraints)
            .split(panel_chunks[2]);

        for (j, window) in snapshot.rate_windows.iter().take(3).enumerate() {
            if j >= gauge_chunks.len() {
                break;
            }
            draw_gauge_bar(frame, gauge_chunks[j], &window.label, window.used_percent, tick);
        }
    }
}

/// Draw a single gauge bar spanning the full width of `area`.
///
/// Layout: ` label  ▓▓▓▓▓▓▓▓░░░░░░░░░  62% `
///
/// The filled portion uses BAR_FILLED (▓) in the gauge color,
/// with a pulse highlight (█) on the leading edge that moves every ~500ms.
/// The empty portion uses BAR_EMPTY (░) in a dim color.
fn draw_gauge_bar(frame: &mut Frame, area: Rect, label: &str, percent: f64, tick: u64) {
    let width = area.width as usize;
    if width < 10 {
        return;
    }

    // Fixed-width label: 12 chars + 2 padding
    let label_width = 14;
    // Percentage text: " XXX% " = 6 chars
    let pct_width = 6;

    let bar_width = width.saturating_sub(label_width + pct_width);
    if bar_width == 0 {
        return;
    }

    let ratio = (percent / 100.0).clamp(0.0, 1.0);
    let filled_count = ((bar_width as f64) * ratio).round() as usize;
    let empty_count = bar_width.saturating_sub(filled_count);

    let fg_color = gauge_color(ratio);
    let dim_color = gauge_color_dim(ratio);
    let empty_color = Color::Rgb(60, 60, 60);

    // Pulse position: moves along the filled portion every 2 ticks (~500ms)
    let pulse_pos = if filled_count > 0 {
        (tick as usize / 2) % filled_count
    } else {
        0
    };

    // Build the filled portion character by character
    let mut filled_str = String::with_capacity(filled_count * 3);
    let mut filled_spans: Vec<Span> = Vec::new();

    // Label (right-padded to label_width)
    let padded_label = format!(" {:width$}", label, width = label_width - 1);
    filled_spans.push(Span::styled(
        padded_label,
        Style::default().fg(Color::DarkGray),
    ));

    // Build filled section with pulse
    if filled_count > 0 {
        // Before pulse
        if pulse_pos > 0 {
            filled_str.clear();
            for _ in 0..pulse_pos {
                filled_str.push(BAR_FILLED);
            }
            filled_spans.push(Span::styled(
                filled_str.clone(),
                Style::default().fg(fg_color),
            ));
        }

        // Pulse character
        filled_spans.push(Span::styled(
            BAR_PULSE.to_string(),
            Style::default().fg(fg_color).add_modifier(Modifier::BOLD),
        ));

        // After pulse
        let after = filled_count.saturating_sub(pulse_pos + 1);
        if after > 0 {
            filled_str.clear();
            for _ in 0..after {
                filled_str.push(BAR_FILLED);
            }
            filled_spans.push(Span::styled(
                filled_str.clone(),
                Style::default().fg(dim_color),
            ));
        }
    }

    // Empty portion
    if empty_count > 0 {
        let empty_str: String = (0..empty_count).map(|_| BAR_EMPTY).collect();
        filled_spans.push(Span::styled(
            empty_str,
            Style::default().fg(empty_color),
        ));
    }

    // Percentage text
    let pct_text = format!(" {:>3.0}% ", percent);
    let pct_color = if ratio > 0.9 {
        Color::Red
    } else {
        Color::White
    };
    filled_spans.push(Span::styled(
        pct_text,
        Style::default().fg(pct_color).add_modifier(if ratio > 0.9 { Modifier::BOLD } else { Modifier::empty() }),
    ));

    frame.render_widget(Paragraph::new(Line::from(filled_spans)), area);
}

fn draw_status_bar(frame: &mut Frame, area: Rect, app: &App) {
    let elapsed_secs = app.last_refresh.elapsed().as_secs();
    let interval = app.config.general.refresh_interval_secs;

    let last_refresh_str = format_elapsed(elapsed_secs);

    let next_refresh = if app.paused {
        "paused".to_string()
    } else if interval > elapsed_secs {
        format!("{}s", interval - elapsed_secs)
    } else {
        "now".to_string()
    };

    // Compact keybinds
    let status = Line::from(vec![
        Span::styled(" r", Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD)),
        Span::styled(" refresh  ", Style::default().fg(Color::DarkGray)),
        Span::styled("p", Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD)),
        Span::styled(" pause  ", Style::default().fg(Color::DarkGray)),
        Span::styled("q", Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD)),
        Span::styled(" quit", Style::default().fg(Color::DarkGray)),
        Span::raw("  "),
        Span::styled("│", Style::default().fg(Color::Rgb(60, 60, 60))),
        Span::raw("  "),
        Span::styled(last_refresh_str, Style::default().fg(Color::DarkGray)),
        Span::raw("  "),
        Span::styled("next ", Style::default().fg(Color::Rgb(80, 80, 80))),
        Span::styled(next_refresh, Style::default().fg(Color::DarkGray)),
    ]);

    let bar = Paragraph::new(status)
        .block(Block::default().borders(Borders::ALL).border_style(Style::default().fg(Color::DarkGray)));
    frame.render_widget(bar, area);
}

/// Format elapsed seconds into a friendly string.
fn format_elapsed(secs: u64) -> String {
    if secs == 0 {
        return "just now".to_string();
    }
    let minutes = secs / 60;
    let remaining_secs = secs % 60;
    if minutes > 0 && remaining_secs > 0 {
        format!("{}m {}s ago", minutes, remaining_secs)
    } else if minutes > 0 {
        format!("{}m ago", minutes)
    } else {
        format!("{}s ago", secs)
    }
}
