mod app;
mod config;
mod cost;
mod provider;
mod ui;

use std::io;
use std::time::Duration;

use anyhow::Result;
use crossterm::{
    event::{self, Event, KeyCode, KeyEventKind},
    execute,
    terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen},
};
use ratatui::prelude::*;

use app::App;
use config::Config;
use provider::claude::ClaudeProvider;
use provider::codex::CodexProvider;
use provider::gemini::GeminiProvider;
use provider::Provider;

#[tokio::main]
async fn main() -> Result<()> {
    // Initialize tracing (logs to stderr so they don't interfere with TUI)
    tracing_subscriber::fmt()
        .with_writer(io::stderr)
        .with_env_filter("claudetop=info")
        .init();

    let config = Config::load()?;

    let providers: Vec<Box<dyn Provider>> = vec![
        Box::new(ClaudeProvider::new(config.claude.clone())),
        Box::new(CodexProvider::new(config.codex.clone())),
        Box::new(GeminiProvider::new(config.gemini.clone())),
    ];

    let mut app = App::new(config, providers);

    // Initial fetch
    app.refresh().await?;

    // Terminal setup
    enable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen)?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;

    // Main loop
    let tick_rate = Duration::from_millis(250);
    loop {
        terminal.draw(|frame| ui::draw(frame, &app))?;

        if event::poll(tick_rate)? {
            if let Event::Key(key) = event::read()? {
                // Process Press events. On terminals without Kitty protocol,
                // kind may always be Press. On terminals with it, filter out
                // Release/Repeat to avoid double-firing.
                if key.kind == KeyEventKind::Press || key.kind == KeyEventKind::Repeat {
                    match key.code {
                        KeyCode::Char('q') | KeyCode::Esc => {
                            break;
                        }
                        KeyCode::Char('c')
                            if key
                                .modifiers
                                .contains(crossterm::event::KeyModifiers::CONTROL) =>
                        {
                            break;
                        }
                        KeyCode::Char('r') => {
                            let _ = app.refresh().await;
                        }
                        KeyCode::Char('p') => {
                            app.paused = !app.paused;
                        }
                        _ => {}
                    }
                }
            }
        }

        app.tick().await;
    }

    // Terminal restore
    disable_raw_mode()?;
    execute!(terminal.backend_mut(), LeaveAlternateScreen)?;
    terminal.show_cursor()?;

    Ok(())
}
