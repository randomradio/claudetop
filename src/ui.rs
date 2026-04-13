use chrono::Utc;
use ratatui::{
    layout::{Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Gauge, Paragraph},
    Frame,
};

use crate::app::App;
use crate::provider::{ProviderKind, ProviderStatus, UsageSnapshot};

pub fn draw(frame: &mut Frame, app: &App) {
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(3),  // title bar
            Constraint::Min(0),    // provider panels
            Constraint::Length(3), // status bar
        ])
        .split(frame.area());

    draw_title_bar(frame, chunks[0]);
    draw_providers(frame, chunks[1], app);
    draw_status_bar(frame, chunks[2], app);
}

fn draw_title_bar(frame: &mut Frame, area: Rect) {
    let title = Paragraph::new(Line::from(vec![
        Span::styled(
            " ClaudeTop ",
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled(
            "- AI Provider Dashboard",
            Style::default().fg(Color::DarkGray),
        ),
    ]))
    .block(Block::default().borders(Borders::ALL));

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
        draw_provider_panel(frame, chunks[i], snapshot);
    }
}

/// Format a duration in seconds into a human-readable string like "2h 15m" or "45m" or "30s".
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

fn draw_provider_panel(frame: &mut Frame, area: Rect, snapshot: &UsageSnapshot) {
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
                format!("  Resets in {}", format_duration_short(secs))
            } else {
                "  Reset due".to_string()
            }
        })
        .unwrap_or_default();

    let title = format!(" {} {}", snapshot.provider, reset_text);

    let status_color = match &snapshot.status {
        ProviderStatus::Ok => Color::Green,
        ProviderStatus::Unavailable(_) => Color::Red,
        ProviderStatus::NotConfigured(_) => Color::Yellow,
    };

    let block = Block::default()
        .title(Span::styled(
            title,
            Style::default()
                .fg(title_color)
                .add_modifier(Modifier::BOLD),
        ))
        .borders(Borders::ALL)
        .border_style(Style::default().fg(status_color));

    let inner = block.inner(area);
    frame.render_widget(block, area);

    // For not-configured or unavailable providers, show a prominent hint message.
    if !snapshot.status.is_ok() {
        let hint = snapshot.status.label();
        let hint_color = match &snapshot.status {
            ProviderStatus::NotConfigured(_) => Color::Yellow,
            ProviderStatus::Unavailable(_) => Color::Red,
            _ => Color::White,
        };
        let lines = vec![
            Line::from(""),
            Line::from(Span::styled(
                format!("  {}", hint),
                Style::default()
                    .fg(hint_color)
                    .add_modifier(Modifier::BOLD),
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

    // Layout inside the provider panel: status, plan/credits/cost, then gauges.
    let gauge_count = snapshot.rate_windows.len().min(3);
    let panel_chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1), // status line
            Constraint::Length(1), // plan / credits / cost line
            Constraint::Length(gauge_count as u16), // rate window gauges
            Constraint::Min(0),   // remaining space
        ])
        .split(inner);

    // Status line
    let status_text = Line::from(vec![
        Span::styled("Status: ", Style::default().fg(Color::DarkGray)),
        Span::styled(
            snapshot.status.label(),
            Style::default().fg(status_color),
        ),
    ]);
    frame.render_widget(Paragraph::new(status_text), panel_chunks[0]);

    // Plan / credits / cost line
    let plan = snapshot.plan_name.as_deref().unwrap_or("--");
    let credits = snapshot.credits_remaining.as_deref().unwrap_or("--");
    let cost_str = snapshot
        .cost_30d
        .map(|c| format!("${:.2}", c))
        .unwrap_or_else(|| "--".to_string());

    let info_line = Line::from(vec![
        Span::styled("Plan: ", Style::default().fg(Color::DarkGray)),
        Span::raw(plan),
        Span::raw("  "),
        Span::styled("Credits: ", Style::default().fg(Color::DarkGray)),
        Span::raw(credits),
        Span::raw("  "),
        Span::styled("Cost (30d): ", Style::default().fg(Color::DarkGray)),
        Span::raw(cost_str),
    ]);
    frame.render_widget(Paragraph::new(info_line), panel_chunks[1]);

    // Rate windows as gauges (up to 3)
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
            let ratio = (window.used_percent / 100.0).clamp(0.0, 1.0);
            let color = if ratio > 0.9 {
                Color::Red
            } else if ratio > 0.7 {
                Color::Yellow
            } else {
                Color::Green
            };
            let label = format!("{}: {:.0}%", window.label, window.used_percent);
            let gauge = Gauge::default()
                .gauge_style(Style::default().fg(color))
                .ratio(ratio)
                .label(label);
            frame.render_widget(gauge, gauge_chunks[j]);
        }
    }
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

    let status = Line::from(vec![
        Span::styled(" [r] ", Style::default().fg(Color::Cyan)),
        Span::raw("refresh"),
        Span::raw("  "),
        Span::styled("[p] ", Style::default().fg(Color::Cyan)),
        Span::raw("pause"),
        Span::raw("  "),
        Span::styled("[q/Esc] ", Style::default().fg(Color::Cyan)),
        Span::raw("quit"),
        Span::raw("  |  "),
        Span::styled("Last refresh: ", Style::default().fg(Color::DarkGray)),
        Span::raw(last_refresh_str),
        Span::raw("  "),
        Span::styled("Next: ", Style::default().fg(Color::DarkGray)),
        Span::raw(next_refresh),
    ]);

    let bar = Paragraph::new(status).block(Block::default().borders(Borders::ALL));
    frame.render_widget(bar, area);
}

/// Format elapsed seconds into a friendly string like "2s ago", "1m 30s ago".
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
