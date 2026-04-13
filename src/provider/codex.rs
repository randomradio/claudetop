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

/// Represents the type of credential discovered.
enum CredentialSource {
    /// An API key (from config or OPENAI_API_KEY env var).
    ApiKey(String),
    /// An OAuth access_token from ~/.codex/auth.json.
    OAuthToken(String),
}

/// Response shape for the OpenAI usage/completions endpoint.
#[derive(Deserialize)]
struct UsageResponse {
    #[serde(default)]
    data: Vec<UsageBucket>,
}

#[derive(Deserialize)]
struct UsageBucket {
    #[serde(default)]
    results: Vec<UsageResult>,
}

#[derive(Deserialize)]
struct UsageResult {
    #[serde(default)]
    input_tokens: u64,
    #[serde(default)]
    output_tokens: u64,
}

/// Response shape for the OpenAI credit_grants endpoint.
#[derive(Deserialize)]
#[allow(dead_code)]
struct CreditGrantsResponse {
    total_available: Option<f64>,
    total_granted: Option<f64>,
    total_used: Option<f64>,
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

    /// Discover an API key from config or environment (not OAuth).
    fn discover_api_key(&self) -> Option<String> {
        self.config
            .api_key
            .clone()
            .or_else(|| self.config.token.clone())
            .or_else(|| std::env::var("OPENAI_API_KEY").ok())
    }

    /// Discover the OAuth token from ~/.codex/auth.json.
    fn discover_oauth_token() -> Option<String> {
        Self::read_codex_auth_file()
    }

    /// Discover credentials with source information, in priority order:
    /// 1. Config file api_key/token override
    /// 2. Environment variable OPENAI_API_KEY
    /// 3. OAuth access_token from ~/.codex/auth.json
    fn discover_credential_source(&self) -> Option<CredentialSource> {
        if let Some(key) = self.discover_api_key() {
            return Some(CredentialSource::ApiKey(key));
        }
        Self::discover_oauth_token().map(CredentialSource::OAuthToken)
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

    /// Try to fetch usage via the OpenAI usage/completions endpoint using an OAuth token.
    async fn fetch_usage_via_oauth(
        &self,
        client: &reqwest::Client,
        token: &str,
    ) -> Option<UsageSnapshot> {
        let now = Utc::now().timestamp();
        let thirty_days_ago = now - (30 * 24 * 60 * 60);

        let url = format!(
            "https://api.openai.com/v1/organization/usage/completions?start_time={}&end_time={}&bucket_width=1d",
            thirty_days_ago, now
        );

        let response = client
            .get(&url)
            .header("Authorization", format!("Bearer {}", token))
            .send()
            .await
            .ok()?;

        if response.status().is_success() {
            let usage: UsageResponse = response.json().await.ok()?;
            let total_tokens: u64 = usage
                .data
                .iter()
                .flat_map(|b| &b.results)
                .map(|r| r.input_tokens + r.output_tokens)
                .sum();

            // Rough cost estimate: ~$2.50 per 1M tokens average across models
            let cost_30d = (total_tokens as f64) / 1_000_000.0 * 2.50;

            return Some(UsageSnapshot {
                provider: ProviderKind::Codex,
                status: ProviderStatus::Ok,
                plan_name: None,
                rate_windows: Vec::new(),
                credits_remaining: None,
                cost_30d: Some(cost_30d),
                updated_at: Utc::now(),
            });
        }

        // If the usage endpoint fails, try the credit_grants endpoint.
        self.fetch_credits_via_oauth(client, token).await
    }

    /// Try to fetch credit balance via the billing/credit_grants endpoint.
    async fn fetch_credits_via_oauth(
        &self,
        client: &reqwest::Client,
        token: &str,
    ) -> Option<UsageSnapshot> {
        let response = client
            .get("https://api.openai.com/dashboard/billing/credit_grants")
            .header("Authorization", format!("Bearer {}", token))
            .send()
            .await
            .ok()?;

        if !response.status().is_success() {
            return None;
        }

        let credits: CreditGrantsResponse = response.json().await.ok()?;
        let remaining = credits.total_available.map(|v| format!("${:.2}", v));
        let cost = credits.total_used;

        Some(UsageSnapshot {
            provider: ProviderKind::Codex,
            status: ProviderStatus::Ok,
            plan_name: None,
            rate_windows: Vec::new(),
            credits_remaining: remaining,
            cost_30d: cost,
            updated_at: Utc::now(),
        })
    }

    /// Fallback: make a minimal chat completion probe to extract rate-limit headers.
    /// Only used with an actual API key (not OAuth tokens).
    async fn fetch_usage_via_probe(
        &self,
        client: &reqwest::Client,
        api_key: &str,
    ) -> Result<UsageSnapshot> {
        let response = client
            .post("https://api.openai.com/v1/chat/completions")
            .header("Authorization", format!("Bearer {}", api_key))
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

        if !response.status().is_success()
            && headers.get("x-ratelimit-limit-requests").is_none()
        {
            let status_code = response.status();
            let body = response.text().await.unwrap_or_default();
            return Ok(UsageSnapshot {
                provider: ProviderKind::Codex,
                status: ProviderStatus::Unavailable(format!(
                    "API returned {} \u{2014} {}",
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

    /// Check if the codex CLI is available on the system.
    #[allow(dead_code)]
    fn shell_codex_status() -> Option<UsageSnapshot> {
        let which = std::process::Command::new("which")
            .arg("codex")
            .output()
            .ok()?;
        if !which.status.success() {
            return None;
        }

        // Codex CLI parsing can be added later if needed.
        None
    }
}

#[async_trait::async_trait]
impl Provider for CodexProvider {
    fn kind(&self) -> ProviderKind {
        ProviderKind::Codex
    }

    async fn fetch_usage(&self) -> Result<UsageSnapshot> {
        let credential = match self.discover_credential_source() {
            Some(c) => c,
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

        match credential {
            CredentialSource::OAuthToken(token) => {
                // Try OAuth-based usage endpoints first.
                if let Some(snapshot) = self.fetch_usage_via_oauth(&client, &token).await {
                    return Ok(snapshot);
                }
                // OAuth endpoints didn't work; report as connected but with no usage data.
                Ok(UsageSnapshot {
                    provider: ProviderKind::Codex,
                    status: ProviderStatus::Ok,
                    plan_name: None,
                    rate_windows: Vec::new(),
                    credits_remaining: None,
                    cost_30d: None,
                    updated_at: Utc::now(),
                })
            }
            CredentialSource::ApiKey(key) => {
                // API keys use the probe approach as fallback.
                self.fetch_usage_via_probe(&client, &key).await
            }
        }
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
