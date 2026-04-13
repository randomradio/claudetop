use anyhow::Result;
use chrono::Utc;
use serde::Deserialize;

use crate::config::ProviderConfig;
use crate::provider::{Provider, ProviderKind, ProviderStatus, RateWindow, UsageSnapshot};

#[derive(Deserialize)]
struct CodexAuth {
    tokens: Option<CodexTokens>,
}

#[derive(Deserialize)]
struct CodexTokens {
    access_token: Option<String>,
}

pub struct CodexProvider {
    config: ProviderConfig,
}

impl CodexProvider {
    pub fn new(config: ProviderConfig) -> Self {
        Self { config }
    }

    /// Read the Codex CLI auth file at ~/.codex/auth.json and extract the access token.
    fn read_codex_auth_file() -> Option<String> {
        let home = dirs::home_dir()?;
        let auth_path = home.join(".codex").join("auth.json");
        let contents = std::fs::read_to_string(auth_path).ok()?;
        let auth: CodexAuth = serde_json::from_str(&contents).ok()?;
        auth.tokens?.access_token
    }

    /// Parse a rate-limit header value as f64, returning None if missing or unparseable.
    fn parse_header(headers: &reqwest::header::HeaderMap, name: &str) -> Option<f64> {
        headers
            .get(name)?
            .to_str()
            .ok()?
            .parse::<f64>()
            .ok()
    }

    /// Build a RateWindow from limit/remaining header pairs.
    fn build_rate_window(
        headers: &reqwest::header::HeaderMap,
        label: &str,
        limit_header: &str,
        remaining_header: &str,
    ) -> Option<RateWindow> {
        let limit = Self::parse_header(headers, limit_header)?;
        let remaining = Self::parse_header(headers, remaining_header)?;
        if limit <= 0.0 {
            return None;
        }
        let used_percent = (limit - remaining) / limit * 100.0;
        Some(RateWindow {
            label: label.to_string(),
            used_percent,
            reset_at: None,
        })
    }
}

#[async_trait::async_trait]
impl Provider for CodexProvider {
    fn kind(&self) -> ProviderKind {
        ProviderKind::Codex
    }

    async fn fetch_usage(&self) -> Result<UsageSnapshot> {
        let token = match self.discover_credentials() {
            Some(t) => t,
            None => {
                return Ok(UsageSnapshot::not_configured(
                    ProviderKind::Codex,
                    "No API key configured",
                ));
            }
        };

        let client = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(10))
            .build()?;

        let response = client
            .post("https://api.openai.com/v1/chat/completions")
            .header("Authorization", format!("Bearer {}", token))
            .json(&serde_json::json!({
                "model": "gpt-4o-mini",
                "messages": [{"role": "user", "content": "hi"}],
                "max_tokens": 1
            }))
            .send()
            .await;

        let response = match response {
            Ok(r) => r,
            Err(e) => {
                return Ok(UsageSnapshot {
                    provider: ProviderKind::Codex,
                    status: ProviderStatus::Unavailable(format!("Request failed: {}", e)),
                    plan_name: None,
                    rate_windows: Vec::new(),
                    credits_remaining: None,
                    cost_30d: None,
                    updated_at: Utc::now(),
                });
            }
        };

        let headers = response.headers().clone();

        // Even a 4xx/5xx may carry rate-limit headers, but if we got no useful
        // headers at all and the status is an error, report it.
        if !response.status().is_success()
            && headers.get("x-ratelimit-limit-requests").is_none()
        {
            let status_code = response.status();
            let body = response.text().await.unwrap_or_default();
            return Ok(UsageSnapshot {
                provider: ProviderKind::Codex,
                status: ProviderStatus::Unavailable(format!(
                    "API returned {} — {}",
                    status_code,
                    body.chars().take(200).collect::<String>()
                )),
                plan_name: None,
                rate_windows: Vec::new(),
                credits_remaining: None,
                cost_30d: None,
                updated_at: Utc::now(),
            });
        }

        let mut rate_windows = Vec::new();

        if let Some(rw) = Self::build_rate_window(
            &headers,
            "Requests",
            "x-ratelimit-limit-requests",
            "x-ratelimit-remaining-requests",
        ) {
            rate_windows.push(rw);
        }

        if let Some(rw) = Self::build_rate_window(
            &headers,
            "Tokens",
            "x-ratelimit-limit-tokens",
            "x-ratelimit-remaining-tokens",
        ) {
            rate_windows.push(rw);
        }

        Ok(UsageSnapshot {
            provider: ProviderKind::Codex,
            status: ProviderStatus::Ok,
            plan_name: None,
            rate_windows,
            credits_remaining: None,
            cost_30d: None,
            updated_at: Utc::now(),
        })
    }

    fn discover_credentials(&self) -> Option<String> {
        self.config
            .api_key
            .clone()
            .or_else(|| self.config.token.clone())
            .or_else(|| std::env::var("OPENAI_API_KEY").ok())
            .or_else(|| Self::read_codex_auth_file())
    }
}
