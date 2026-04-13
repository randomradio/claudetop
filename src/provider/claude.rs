use anyhow::Result;
use chrono::{DateTime, Utc};
use reqwest::header::{HeaderMap, HeaderValue, CONTENT_TYPE};
use std::process::Command;
use std::time::Duration;

use crate::config::ProviderConfig;
use crate::provider::{Provider, ProviderKind, ProviderStatus, RateWindow, UsageSnapshot};

/// Describes how we authenticated — determines which usage endpoint to call.
enum AuthMethod {
    /// OAuth token obtained from Keychain or credentials file.
    OAuth(String),
    /// Direct API key from config or environment variable.
    ApiKey(String),
}

pub struct ClaudeProvider {
    config: ProviderConfig,
}

impl ClaudeProvider {
    pub fn new(config: ProviderConfig) -> Self {
        Self { config }
    }

    /// Try to read an OAuth token from macOS Keychain.
    ///
    /// The Keychain entry for Claude Code stores a JSON blob as the password.
    /// We extract the `accessToken` field from it.
    fn read_keychain_token() -> Option<String> {
        let output = Command::new("security")
            .args(["find-generic-password", "-s", "Claude Code-credentials", "-w"])
            .output()
            .ok()?;

        if !output.status.success() {
            return None;
        }

        let password = String::from_utf8(output.stdout).ok()?.trim().to_string();
        if password.is_empty() {
            return None;
        }

        // The password is JSON containing tokens — extract accessToken.
        if let Ok(value) = serde_json::from_str::<serde_json::Value>(&password) {
            if let Some(token) = value.get("accessToken").and_then(|v| v.as_str()) {
                return Some(token.to_string());
            }
        }

        // If not JSON or no accessToken field, use raw value as-is.
        Some(password)
    }

    /// Try to read an OAuth token from `~/.claude/.credentials.json`.
    fn read_credentials_file() -> Option<String> {
        let home = dirs::home_dir()?;
        let path = home.join(".claude").join(".credentials.json");
        let contents = std::fs::read_to_string(path).ok()?;
        let value: serde_json::Value = serde_json::from_str(&contents).ok()?;
        value
            .get("accessToken")
            .or_else(|| value.get("oauth_token"))
            .or_else(|| value.get("token"))
            .and_then(|v| v.as_str())
            .map(|s| s.to_string())
    }

    /// Discover credentials with priority ordering, returning both the token
    /// and its auth method so we know which endpoint to call.
    fn discover_auth(&self) -> Option<AuthMethod> {
        // 1. Config file token/api_key override (treated as API key).
        if let Some(key) = self.config.api_key.clone().or_else(|| self.config.token.clone()) {
            return Some(AuthMethod::ApiKey(key));
        }

        // 2. Environment variable ANTHROPIC_API_KEY.
        if let Ok(key) = std::env::var("ANTHROPIC_API_KEY") {
            return Some(AuthMethod::ApiKey(key));
        }

        // 3. macOS Keychain (OAuth token).
        if let Some(token) = Self::read_keychain_token() {
            return Some(AuthMethod::OAuth(token));
        }

        // 4. Credentials file (OAuth token).
        if let Some(token) = Self::read_credentials_file() {
            return Some(AuthMethod::OAuth(token));
        }

        None
    }

    /// Fetch usage via the OAuth usage endpoint.
    async fn fetch_oauth_usage(&self, token: &str) -> Result<UsageSnapshot> {
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(10))
            .build()?;

        let response = client
            .get("https://api.anthropic.com/api/oauth/usage")
            .header("Authorization", format!("Bearer {}", token))
            .header("anthropic-beta", "oauth-2025-04-20")
            .header("User-Agent", "claudetop/0.1.0")
            .send()
            .await;

        let resp = match response {
            Ok(r) => r,
            Err(e) => {
                return Ok(UsageSnapshot {
                    provider: ProviderKind::Claude,
                    status: ProviderStatus::Unavailable(format!("OAuth request failed: {e}")),
                    plan_name: None,
                    rate_windows: Vec::new(),
                    credits_remaining: None,
                    cost_30d: None,
                    updated_at: Utc::now(),
                });
            }
        };

        let status_code = resp.status();
        let body_text = resp.text().await.unwrap_or_default();

        let value: serde_json::Value = match serde_json::from_str(&body_text) {
            Ok(v) => v,
            Err(_) => {
                return Ok(UsageSnapshot {
                    provider: ProviderKind::Claude,
                    status: ProviderStatus::Unavailable(format!(
                        "OAuth endpoint returned {status_code}, non-JSON response: {}",
                        body_text.chars().take(200).collect::<String>()
                    )),
                    plan_name: None,
                    rate_windows: Vec::new(),
                    credits_remaining: None,
                    cost_30d: None,
                    updated_at: Utc::now(),
                });
            }
        };

        if !status_code.is_success() {
            let error_msg = value
                .get("error")
                .and_then(|e| e.get("message").or(Some(e)))
                .and_then(|v| v.as_str())
                .unwrap_or("unknown error");
            return Ok(UsageSnapshot {
                provider: ProviderKind::Claude,
                status: ProviderStatus::Unavailable(format!(
                    "OAuth endpoint returned {status_code}: {error_msg}"
                )),
                plan_name: None,
                rate_windows: Vec::new(),
                credits_remaining: None,
                cost_30d: None,
                updated_at: Utc::now(),
            });
        }

        // Extract plan name from various possible field names.
        let plan_name = value
            .get("rate_limit_tier")
            .or_else(|| value.get("rateLimitTier"))
            .or_else(|| value.get("plan"))
            .or_else(|| value.get("tier"))
            .and_then(|v| v.as_str())
            .map(|s| s.to_string());

        // Extract rate windows from array.
        let mut rate_windows = Vec::new();
        let windows_key = if value.get("rate_windows").is_some() {
            "rate_windows"
        } else {
            "rateWindows"
        };

        if let Some(windows) = value.get(windows_key).and_then(|v| v.as_array()) {
            for window in windows {
                let label = window
                    .get("kind")
                    .or_else(|| window.get("label"))
                    .or_else(|| window.get("name"))
                    .and_then(|v| v.as_str())
                    .unwrap_or("unknown")
                    .to_string();

                let used_percent = window
                    .get("used_percent")
                    .or_else(|| window.get("usedPercent"))
                    .or_else(|| window.get("usage_percent"))
                    .and_then(|v| v.as_f64())
                    .unwrap_or(0.0);

                let reset_at = window
                    .get("reset_at")
                    .or_else(|| window.get("resetAt"))
                    .and_then(|v| v.as_str())
                    .and_then(|s| DateTime::parse_from_rfc3339(s).ok())
                    .map(|dt| dt.with_timezone(&Utc));

                rate_windows.push(RateWindow {
                    label,
                    used_percent,
                    reset_at,
                });
            }
        }

        // Extract credits info if present.
        let credits_remaining = value
            .get("credits_remaining")
            .or_else(|| value.get("creditsRemaining"))
            .and_then(|v| {
                if let Some(n) = v.as_f64() {
                    Some(format!("${:.2}", n))
                } else {
                    v.as_str().map(|s| s.to_string())
                }
            });

        let status = if rate_windows.is_empty() && plan_name.is_none() {
            ProviderStatus::Unavailable(format!(
                "OAuth endpoint returned OK but unrecognised format: {}",
                body_text.chars().take(300).collect::<String>()
            ))
        } else {
            ProviderStatus::Ok
        };

        Ok(UsageSnapshot {
            provider: ProviderKind::Claude,
            status,
            plan_name,
            rate_windows,
            credits_remaining,
            cost_30d: None,
            updated_at: Utc::now(),
        })
    }

    /// Fallback: fetch usage by making a minimal probe API call and reading rate-limit headers.
    async fn fetch_api_key_usage(&self, api_key: &str) -> Result<UsageSnapshot> {
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
            .header("x-api-key", api_key)
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
        match self.discover_auth() {
            Some(AuthMethod::OAuth(token)) => self.fetch_oauth_usage(&token).await,
            Some(AuthMethod::ApiKey(key)) => self.fetch_api_key_usage(&key).await,
            None => Ok(UsageSnapshot::not_configured(
                ProviderKind::Claude,
                "Set ANTHROPIC_API_KEY or sign in to Claude Code",
            )),
        }
    }

    fn discover_credentials(&self) -> Option<String> {
        match self.discover_auth() {
            Some(AuthMethod::OAuth(token)) => Some(token),
            Some(AuthMethod::ApiKey(key)) => Some(key),
            None => None,
        }
    }
}
