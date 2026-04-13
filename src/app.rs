use std::time::Instant;

use anyhow::Result;
use crossterm::event::{Event, KeyCode, KeyEventKind, KeyModifiers};

use crate::config::Config;
use crate::cost::CostTracker;
use crate::provider::{Provider, ProviderKind, UsageSnapshot};

pub struct App {
    pub providers: Vec<Box<dyn Provider>>,
    pub snapshots: Vec<UsageSnapshot>,
    pub config: Config,
    pub last_refresh: Instant,
    pub paused: bool,
    pub should_quit: bool,
    pub cost_tracker: CostTracker,
}

impl App {
    pub fn new(config: Config, providers: Vec<Box<dyn Provider>>) -> Self {
        Self {
            snapshots: Vec::new(),
            providers,
            config,
            last_refresh: Instant::now(),
            paused: false,
            should_quit: false,
            cost_tracker: CostTracker::new(),
        }
    }

    /// Check whether a periodic refresh is due and trigger it if so.
    pub async fn tick(&mut self) {
        if self.paused {
            return;
        }

        let interval = self.config.general.refresh_interval_secs;
        if self.last_refresh.elapsed().as_secs() >= interval {
            if let Err(e) = self.refresh().await {
                tracing::error!("Refresh failed: {e}");
            }
        }
    }

    /// Fetch usage from all providers concurrently.
    pub async fn refresh(&mut self) -> Result<()> {
        let futures: Vec<_> = self
            .providers
            .iter()
            .map(|p| p.fetch_usage())
            .collect();

        let results = futures::future::join_all(futures).await;

        self.snapshots = results
            .into_iter()
            .map(|r| match r {
                Ok(snap) => snap,
                Err(e) => {
                    tracing::error!("Provider fetch error: {e}");
                    // Return a generic error snapshot; we don't know the provider kind
                    // so we just log it. In practice each future maps to its provider.
                    UsageSnapshot::not_configured(
                        ProviderKind::Claude,
                        &format!("Error: {e}"),
                    )
                }
            })
            .collect();

        // Scan session logs for cost estimation
        self.cost_tracker.scan();

        // Apply costs to matching snapshots
        for snap in &mut self.snapshots {
            let key = match snap.provider {
                ProviderKind::Claude => "claude",
                ProviderKind::Codex => "codex",
                ProviderKind::Gemini => "gemini",
            };
            if let Some(cost) = self.cost_tracker.cost_for(key) {
                snap.cost_30d = Some(cost);
            }
        }

        self.last_refresh = Instant::now();
        Ok(())
    }

    /// Handle a terminal key event.
    pub fn handle_key(&mut self, event: &Event) -> bool {
        if let Event::Key(key) = event {
            // Defense-in-depth: only process press events
            if key.kind != KeyEventKind::Press {
                return false;
            }
            // Support Ctrl+C to quit
            if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('c') {
                self.should_quit = true;
                return true;
            }

            match key.code {
                KeyCode::Char('q') => {
                    self.should_quit = true;
                    return true;
                }
                KeyCode::Char('r') => {
                    return true; // signal caller to trigger refresh
                }
                KeyCode::Char('p') => {
                    self.paused = !self.paused;
                }
                _ => {}
            }
        }
        false
    }
}
