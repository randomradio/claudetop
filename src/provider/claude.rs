use anyhow::Result;
use chrono::{DateTime, Utc};
use reqwest::header::{HeaderMap, HeaderValue, CONTENT_TYPE};
use std::process::Command;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use crate::config::ProviderConfig;
use crate::provider::{Provider, ProviderKind, ProviderStatus, RateWindow, UsageSnapshot};

/// Describes how we authenticated — determines which usage endpoint to call.
enum AuthMethod {
    /// OAuth token obtained from Keychain or credentials file.
    OAuth(String),
    /// Direct API key from config or environment variable.
    ApiKey(String),
}

/// Cached snapshot with backoff tracking to avoid 429s.
struct Cache {
    snapshot: Option<UsageSnapshot>,
    /// When we last hit a 429, used for backoff.
    last_rate_limited: Option<Instant>,
    /// Current backoff duration, doubles on each consecutive 429.
    backoff: Duration,
}

impl Cache {
    fn new() -> Self {
        Self {
            snapshot: None,
            last_rate_limited: None,
            backoff: Duration::from_secs(60),
        }
    }

    /// Check if we should skip the API call due to recent 429.
    fn should_skip(&self) -> bool {
        if let Some(last) = self.last_rate_limited {
            last.elapsed() < self.backoff
        } else {
            false
        }
    }

    /// Record a 429 and increase backoff (up to 30 minutes).
    fn record_rate_limit(&mut self) {
        self.last_rate_limited = Some(Instant::now());
        self.backoff = (self.backoff * 2).min(Duration::from_secs(1800));
    }

    /// Record a successful fetch, reset backoff.
    fn record_success(&mut self, snapshot: UsageSnapshot) {
        self.snapshot = Some(snapshot);
        self.last_rate_limited = None;
        self.backoff = Duration::from_secs(60);
    }
}

pub struct ClaudeProvider {
    config: ProviderConfig,
    cache: Mutex<Cache>,
}

impl ClaudeProvider {
    pub fn new(config: ProviderConfig) -> Self {
        Self {
            config,
            cache: Mutex::new(Cache::new()),
        }
    }

    /// Extract plan info from Keychain JSON (called separately from token discovery).
    fn read_keychain_plan_info() -> Option<(String, String)> {
        let output = Command::new("security")
            .args(["find-generic-password", "-s", "Claude Code-credentials", "-w"])
            .output()
            .ok()?;
        if !output.status.success() {
            return None;
        }
        let password = String::from_utf8(output.stdout).ok()?.trim().to_string();
        let value: serde_json::Value = serde_json::from_str(&password).ok()?;
        let oauth = value.get("claudeAiOauth")?;
        let tier = oauth.get("rateLimitTier").and_then(|v| v.as_str()).unwrap_or("unknown");
        let sub = oauth.get("subscriptionType").and_then(|v| v.as_str()).unwrap_or("unknown");
        Some((sub.to_string(), tier.to_string()))
    }

    /// Try to read an OAuth token from macOS Keychain.
    ///
    /// The Keychain entry for Claude Code stores a JSON blob as the password,
    /// with tokens nested under `claudeAiOauth`.
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

        // The password is JSON. Tokens are nested under `claudeAiOauth.accessToken`.
        if let Ok(value) = serde_json::from_str::<serde_json::Value>(&password) {
            // Primary: claudeAiOauth.accessToken (actual macOS Keychain format)
            if let Some(token) = value
                .get("claudeAiOauth")
                .and_then(|o| o.get("accessToken"))
                .and_then(|v| v.as_str())
            {
                return Some(token.to_string());
            }
            // Fallback: top-level accessToken
            if let Some(token) = value.get("accessToken").and_then(|v| v.as_str()) {
                return Some(token.to_string());
            }
        }

        // If not JSON or no token found, use raw value as-is.
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

        if status_code == reqwest::StatusCode::TOO_MANY_REQUESTS {
            // Signal 429 to caller for backoff handling.
            return Err(anyhow::anyhow!("rate_limited"));
        }

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

        // The actual API response format has flat keys:
        // {"five_hour":{"utilization":32.0,"resets_at":"..."},
        //  "seven_day":{"utilization":37.0,"resets_at":"..."},
        //  "seven_day_sonnet":{"utilization":12.0,...},
        //  "extra_usage":{"is_enabled":false,...}}

        // Get plan name from Keychain metadata if available.
        let plan_name = Self::read_keychain_plan_info()
            .map(|(sub, _tier)| capitalize(&sub))
            .or_else(|| {
                value.get("rate_limit_tier")
                    .or_else(|| value.get("plan"))
                    .and_then(|v| v.as_str())
                    .map(|s| s.to_string())
            });

        // Parse rate windows from the flat response structure.
        let mut rate_windows = Vec::new();

        // Known rate window keys and their display labels
        let window_defs = [
            ("five_hour", "5h session"),
            ("seven_day", "7d total"),
            ("seven_day_sonnet", "7d Sonnet"),
            ("seven_day_opus", "7d Opus"),
            ("seven_day_cowork", "7d Cowork"),
            ("iguana_necktie", "Special"),
        ];

        for (key, label) in &window_defs {
            if let Some(window) = value.get(key) {
                let utilization = window.get("utilization").and_then(|v| v.as_f64());
                if let Some(pct) = utilization {
                    let reset_at = window
                        .get("resets_at")
                        .or_else(|| window.get("reset_at"))
                        .and_then(|v| v.as_str())
                        .and_then(|s| DateTime::parse_from_rfc3339(s).ok())
                        .map(|dt| dt.with_timezone(&Utc));

                    rate_windows.push(RateWindow {
                        label: label.to_string(),
                        used_percent: pct,
                        reset_at,
                    });
                }
            }
        }

        // Parse extra_usage for credit info
        let credits_remaining = value
            .get("extra_usage")
            .and_then(|eu| {
                let enabled = eu.get("is_enabled").and_then(|v| v.as_bool()).unwrap_or(false);
                if !enabled {
                    return Some("\u{221e}".to_string()); // ∞ for unlimited
                }
                let used = eu.get("used_credits").and_then(|v| v.as_f64());
                let limit = eu.get("monthly_limit").and_then(|v| v.as_f64());
                match (used, limit) {
                    (Some(u), Some(l)) => Some(format!("${:.0}/${:.0}", l - u, l)),
                    _ => Some("Active".to_string()),
                }
            });

        let status = if rate_windows.is_empty() && plan_name.is_none() {
            ProviderStatus::Unavailable(format!(
                "OAuth returned OK but unrecognised format: {}",
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

/// Capitalize the first letter of a string (e.g. "max" -> "Max").
fn capitalize(s: &str) -> String {
    let mut c = s.chars();
    match c.next() {
        None => String::new(),
        Some(f) => f.to_uppercase().collect::<String>() + c.as_str(),
    }
}

#[async_trait::async_trait]
impl Provider for ClaudeProvider {
    fn kind(&self) -> ProviderKind {
        ProviderKind::Claude
    }

    async fn fetch_usage(&self) -> Result<UsageSnapshot> {
        let auth = match self.discover_auth() {
            Some(a) => a,
            None => {
                return Ok(UsageSnapshot::not_configured(
                    ProviderKind::Claude,
                    "Set ANTHROPIC_API_KEY or sign in to Claude Code",
                ));
            }
        };

        // Check backoff — if recently rate-limited, return cached data.
        {
            let cache = self.cache.lock().unwrap();
            if cache.should_skip() {
                if let Some(ref cached) = cache.snapshot {
                    tracing::info!("Claude: returning cached data (backoff active)");
                    return Ok(cached.clone());
                }
            }
        }

        let result = match auth {
            AuthMethod::OAuth(token) => self.fetch_oauth_usage(&token).await,
            AuthMethod::ApiKey(key) => self.fetch_api_key_usage(&key).await,
        };

        match result {
            Ok(snapshot) => {
                if snapshot.status.is_ok() {
                    self.cache.lock().unwrap().record_success(snapshot.clone());
                }
                Ok(snapshot)
            }
            Err(e) if e.to_string().contains("rate_limited") => {
                let mut cache = self.cache.lock().unwrap();
                cache.record_rate_limit();
                tracing::warn!(
                    "Claude: 429 rate limited, backoff for {}s",
                    cache.backoff.as_secs()
                );
                if let Some(ref cached) = cache.snapshot {
                    Ok(cached.clone())
                } else {
                    Ok(UsageSnapshot {
                        provider: ProviderKind::Claude,
                        status: ProviderStatus::Unavailable(
                            "Rate limited (429), no cached data yet".to_string(),
                        ),
                        plan_name: None,
                        rate_windows: Vec::new(),
                        credits_remaining: None,
                        cost_30d: None,
                        updated_at: Utc::now(),
                    })
                }
            }
            Err(e) => Err(e),
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
