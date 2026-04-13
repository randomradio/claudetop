use anyhow::Result;
use chrono::{DateTime, Utc};
use reqwest::header::{HeaderMap, HeaderValue, CONTENT_TYPE};
use std::time::Duration;

use crate::config::ProviderConfig;
use crate::provider::{Provider, ProviderKind, ProviderStatus, RateWindow, UsageSnapshot};

pub struct ClaudeProvider {
    config: ProviderConfig,
}

impl ClaudeProvider {
    pub fn new(config: ProviderConfig) -> Self {
        Self { config }
    }

    /// Try to read an OAuth token from `~/.claude/.credentials.json`.
    fn read_credentials_file() -> Option<String> {
        let home = dirs::home_dir()?;
        let path = home.join(".claude").join(".credentials.json");
        let contents = std::fs::read_to_string(path).ok()?;
        let value: serde_json::Value = serde_json::from_str(&contents).ok()?;
        // The credentials file may contain an `oauth_token` or `token` field.
        value
            .get("oauth_token")
            .or_else(|| value.get("token"))
            .and_then(|v| v.as_str())
            .map(|s| s.to_string())
    }

    /// Parse a rate-limit header value as u64, returning None on missing/invalid.
    fn parse_header_u64(headers: &HeaderMap, name: &str) -> Option<u64> {
        headers
            .get(name)
            .and_then(|v| v.to_str().ok())
            .and_then(|s| s.parse::<u64>().ok())
    }

    /// Parse a rate-limit reset header as a DateTime<Utc>.
    fn parse_header_datetime(headers: &HeaderMap, name: &str) -> Option<DateTime<Utc>> {
        headers
            .get(name)
            .and_then(|v| v.to_str().ok())
            .and_then(|s| DateTime::parse_from_rfc3339(s).ok())
            .map(|dt| dt.with_timezone(&Utc))
    }

    /// Build a RateWindow from limit/remaining/reset headers.
    fn build_rate_window(
        headers: &HeaderMap,
        label: &str,
        limit_header: &str,
        remaining_header: &str,
        reset_header: &str,
    ) -> Option<RateWindow> {
        let limit = Self::parse_header_u64(headers, limit_header)?;
        let remaining = Self::parse_header_u64(headers, remaining_header)?;
        let reset_at = Self::parse_header_datetime(headers, reset_header);

        let used_percent = if limit > 0 {
            ((limit - remaining.min(limit)) as f64 / limit as f64) * 100.0
        } else {
            0.0
        };

        Some(RateWindow {
            label: label.to_string(),
            used_percent,
            reset_at,
        })
    }
}

#[async_trait::async_trait]
impl Provider for ClaudeProvider {
    fn kind(&self) -> ProviderKind {
        ProviderKind::Claude
    }

    async fn fetch_usage(&self) -> Result<UsageSnapshot> {
        let api_key = match self.discover_credentials() {
            Some(t) => t,
            None => {
                return Ok(UsageSnapshot::not_configured(
                    ProviderKind::Claude,
                    "Set ANTHROPIC_API_KEY or add api_key to config",
                ));
            }
        };

        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(10))
            .build()?;

        let body = serde_json::json!({
            "model": "claude-sonnet-4-20250514",
            "max_tokens": 1,
            "messages": [{"role": "user", "content": "hi"}]
        });

        let response = client
            .post("https://api.anthropic.com/v1/messages")
            .header("x-api-key", &api_key)
            .header("anthropic-version", "2023-06-01")
            .header(CONTENT_TYPE, HeaderValue::from_static("application/json"))
            .json(&body)
            .send()
            .await;

        let resp = match response {
            Ok(r) => r,
            Err(e) => {
                return Ok(UsageSnapshot {
                    provider: ProviderKind::Claude,
                    status: ProviderStatus::Unavailable(format!("Request failed: {e}")),
                    plan_name: None,
                    rate_windows: Vec::new(),
                    credits_remaining: None,
                    cost_30d: None,
                    updated_at: Utc::now(),
                });
            }
        };

        let headers = resp.headers().clone();

        // Even a 4xx/5xx response usually includes rate-limit headers, so we
        // parse them regardless of status code.
        let mut rate_windows = Vec::new();

        if let Some(rw) = Self::build_rate_window(
            &headers,
            "Requests",
            "anthropic-ratelimit-requests-limit",
            "anthropic-ratelimit-requests-remaining",
            "anthropic-ratelimit-requests-reset",
        ) {
            rate_windows.push(rw);
        }

        if let Some(rw) = Self::build_rate_window(
            &headers,
            "Tokens",
            "anthropic-ratelimit-tokens-limit",
            "anthropic-ratelimit-tokens-remaining",
            "anthropic-ratelimit-tokens-reset",
        ) {
            rate_windows.push(rw);
        }

        let status = if rate_windows.is_empty() {
            ProviderStatus::Unavailable("No rate-limit headers in response".to_string())
        } else {
            ProviderStatus::Ok
        };

        Ok(UsageSnapshot {
            provider: ProviderKind::Claude,
            status,
            plan_name: None,
            rate_windows,
            credits_remaining: None,
            cost_30d: None,
            updated_at: Utc::now(),
        })
    }

    fn discover_credentials(&self) -> Option<String> {
        // 1. Config file token/api_key override
        self.config
            .api_key
            .clone()
            .or_else(|| self.config.token.clone())
            // 2. Environment variable
            .or_else(|| std::env::var("ANTHROPIC_API_KEY").ok())
            // 3. Credentials file (~/.claude/.credentials.json)
            .or_else(Self::read_credentials_file)
    }
}
