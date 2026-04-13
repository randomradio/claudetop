use std::time::Duration;

use anyhow::Result;
use serde::Deserialize;

use crate::config::ProviderConfig;
use crate::provider::{Provider, ProviderKind, ProviderStatus, RateWindow, UsageSnapshot};

const GEMINI_API_BASE: &str = "https://generativelanguage.googleapis.com/v1beta";
const CLOUDCODE_QUOTA_URL: &str =
    "https://cloudcode-pa.googleapis.com/v1internal:retrieveUserQuota";

/// OAuth credentials stored by Gemini CLI at ~/.gemini/oauth_creds.json.
#[derive(Debug, Deserialize)]
struct GeminiOAuthCreds {
    access_token: Option<String>,
    #[allow(dead_code)]
    refresh_token: Option<String>,
    #[allow(dead_code)]
    expiry_date: Option<u64>,
}

/// Represents the resolved credential: either an OAuth token or an API key.
enum GeminiCredential {
    OAuthToken(String),
    ApiKey(String),
}

pub struct GeminiProvider {
    config: ProviderConfig,
}

impl GeminiProvider {
    pub fn new(config: ProviderConfig) -> Self {
        Self { config }
    }

    fn build_client() -> Result<reqwest::Client> {
        Ok(reqwest::Client::builder()
            .timeout(Duration::from_secs(10))
            .build()?)
    }

    /// Read OAuth access token from ~/.gemini/oauth_creds.json.
    fn read_gemini_oauth() -> Option<String> {
        let home = dirs::home_dir()?;
        let path = home.join(".gemini").join("oauth_creds.json");
        let contents = std::fs::read_to_string(path).ok()?;
        let creds: GeminiOAuthCreds = serde_json::from_str(&contents).ok()?;
        creds.access_token
    }

    /// Read credentials from gcloud application default credentials.
    /// This file is OAuth-based; we extract the access_token if present.
    fn read_gcloud_adc() -> Option<String> {
        let home = dirs::home_dir()?;
        let path = home.join(".config/gcloud/application_default_credentials.json");
        let contents = std::fs::read_to_string(&path).ok()?;
        let value: serde_json::Value = serde_json::from_str(&contents).ok()?;
        // ADC files may contain an access_token directly, or client credentials
        // for OAuth refresh. We only use it if there's a direct access_token.
        value
            .get("access_token")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string())
    }

    /// Discover credentials with full priority chain, returning the type of credential found.
    fn discover_credential(&self) -> Option<GeminiCredential> {
        // 1. Config file api_key/token override
        if let Some(key) = self.config.api_key.clone().or_else(|| self.config.token.clone()) {
            return Some(GeminiCredential::ApiKey(key));
        }

        // 2. GEMINI_API_KEY env var
        if let Some(key) = std::env::var("GEMINI_API_KEY").ok() {
            return Some(GeminiCredential::ApiKey(key));
        }

        // 3. GOOGLE_API_KEY env var
        if let Some(key) = std::env::var("GOOGLE_API_KEY").ok() {
            return Some(GeminiCredential::ApiKey(key));
        }

        // 4. OAuth from ~/.gemini/oauth_creds.json
        if let Some(token) = Self::read_gemini_oauth() {
            return Some(GeminiCredential::OAuthToken(token));
        }

        // 5. gcloud ADC
        if let Some(token) = Self::read_gcloud_adc() {
            return Some(GeminiCredential::OAuthToken(token));
        }

        None
    }

    /// Fetch quota information using the CloudCode internal API with an OAuth token.
    async fn fetch_oauth_quota(
        client: &reqwest::Client,
        oauth_token: &str,
    ) -> Result<UsageSnapshot> {
        let response = client
            .post(CLOUDCODE_QUOTA_URL)
            .header("Authorization", format!("Bearer {}", oauth_token))
            .header("Content-Type", "application/json")
            .body("{}")
            .send()
            .await?;

        let status = response.status();

        if status == reqwest::StatusCode::UNAUTHORIZED
            || status == reqwest::StatusCode::FORBIDDEN
        {
            return Ok(UsageSnapshot {
                provider: ProviderKind::Gemini,
                status: ProviderStatus::Unavailable(format!("Auth failed ({})", status)),
                plan_name: Some("OAuth".to_string()),
                rate_windows: Vec::new(),
                credits_remaining: None,
                cost_30d: None,
                updated_at: chrono::Utc::now(),
            });
        }

        if !status.is_success() {
            // Quota endpoint failed — fall back to showing the token is valid
            // but we can't get quota details.
            tracing::debug!(
                "CloudCode quota endpoint returned {}, falling back to basic status",
                status
            );
            return Ok(UsageSnapshot {
                provider: ProviderKind::Gemini,
                status: ProviderStatus::Ok,
                plan_name: Some("OAuth".to_string()),
                rate_windows: Vec::new(),
                credits_remaining: Some("Active (quota unavailable)".to_string()),
                cost_30d: None,
                updated_at: chrono::Utc::now(),
            });
        }

        let body: serde_json::Value = response.json().await?;
        let rate_windows = Self::parse_quota_response(&body);

        let credits_label = if rate_windows.is_empty() {
            "Active".to_string()
        } else {
            "Active (quota data available)".to_string()
        };

        Ok(UsageSnapshot {
            provider: ProviderKind::Gemini,
            status: ProviderStatus::Ok,
            plan_name: Some("OAuth".to_string()),
            rate_windows,
            credits_remaining: Some(credits_label),
            cost_30d: None,
            updated_at: chrono::Utc::now(),
        })
    }

    /// Try to extract rate windows from the quota response.
    /// The exact schema is not documented, so we search for arrays of objects
    /// with usage/limit/remaining fields.
    fn parse_quota_response(body: &serde_json::Value) -> Vec<RateWindow> {
        let mut windows = Vec::new();

        // Try known field names for the top-level array
        let bucket_keys = [
            "quotaBuckets",
            "quota_buckets",
            "buckets",
            "quotas",
            "rateLimit",
            "rateLimits",
        ];

        let buckets = bucket_keys
            .iter()
            .filter_map(|key| body.get(key))
            .find(|v| v.is_array())
            .and_then(|v| v.as_array());

        // If no known key, try the body itself if it's an array
        let buckets = buckets.or_else(|| body.as_array());

        // If still nothing, try to find any array value in the top-level object
        let owned_arr;
        let buckets = match buckets {
            Some(b) => Some(b),
            None => {
                if let Some(obj) = body.as_object() {
                    owned_arr = obj
                        .values()
                        .find(|v| v.is_array())
                        .and_then(|v| v.as_array())
                        .cloned();
                    owned_arr.as_ref()
                } else {
                    None
                }
            }
        };

        if let Some(items) = buckets {
            for item in items {
                if let Some(window) = Self::parse_quota_bucket(item) {
                    windows.push(window);
                }
            }
        }

        windows
    }

    /// Parse a single quota bucket object into a RateWindow.
    fn parse_quota_bucket(item: &serde_json::Value) -> Option<RateWindow> {
        let model = item
            .get("model")
            .or_else(|| item.get("modelName"))
            .or_else(|| item.get("model_name"))
            .and_then(|v| v.as_str())
            .unwrap_or("Unknown");

        // Try usage/limit pair
        let usage = item
            .get("usage")
            .or_else(|| item.get("used"))
            .or_else(|| item.get("requestCount"))
            .and_then(|v| v.as_f64());

        let limit = item
            .get("limit")
            .or_else(|| item.get("maxLimit"))
            .or_else(|| item.get("requestLimit"))
            .and_then(|v| v.as_f64());

        let remaining = item
            .get("remaining")
            .or_else(|| item.get("remainingRequests"))
            .and_then(|v| v.as_f64());

        let used_percent = match (usage, limit) {
            (Some(u), Some(l)) if l > 0.0 => u / l * 100.0,
            _ => match (remaining, limit) {
                (Some(r), Some(l)) if l > 0.0 => (l - r) / l * 100.0,
                _ => return None,
            },
        };

        let reset_at = item
            .get("resetTime")
            .or_else(|| item.get("reset_time"))
            .and_then(|v| v.as_str())
            .and_then(|s| chrono::DateTime::parse_from_rfc3339(s).ok())
            .map(|dt| dt.with_timezone(&chrono::Utc));

        let label = match (usage, limit) {
            (Some(u), Some(l)) => format!("{} ({}/{})", model, u as u64, l as u64),
            _ => model.to_string(),
        };

        Some(RateWindow {
            label,
            used_percent,
            reset_at,
        })
    }

    /// Verify an API key by listing models (free endpoint, no quota consumed).
    async fn verify_api_key(
        client: &reqwest::Client,
        api_key: &str,
    ) -> Result<UsageSnapshot> {
        let url = format!(
            "{}/models?key={}&pageSize=1",
            GEMINI_API_BASE, api_key
        );

        let response = client.get(&url).send().await?;
        let status = response.status();

        if status == reqwest::StatusCode::TOO_MANY_REQUESTS {
            return Ok(UsageSnapshot {
                provider: ProviderKind::Gemini,
                status: ProviderStatus::Ok,
                plan_name: Some("API Key".to_string()),
                rate_windows: vec![RateWindow {
                    label: "API Status".to_string(),
                    used_percent: 100.0,
                    reset_at: None,
                }],
                credits_remaining: Some("Rate limited".to_string()),
                cost_30d: None,
                updated_at: chrono::Utc::now(),
            });
        }

        if status == reqwest::StatusCode::FORBIDDEN
            || status == reqwest::StatusCode::UNAUTHORIZED
        {
            return Ok(UsageSnapshot {
                provider: ProviderKind::Gemini,
                status: ProviderStatus::Unavailable(format!("Auth failed ({})", status)),
                plan_name: Some("API Key".to_string()),
                rate_windows: Vec::new(),
                credits_remaining: None,
                cost_30d: None,
                updated_at: chrono::Utc::now(),
            });
        }

        if !status.is_success() {
            return Ok(UsageSnapshot {
                provider: ProviderKind::Gemini,
                status: ProviderStatus::Unavailable(format!("API error ({})", status)),
                plan_name: Some("API Key".to_string()),
                rate_windows: Vec::new(),
                credits_remaining: None,
                cost_30d: None,
                updated_at: chrono::Utc::now(),
            });
        }

        Ok(UsageSnapshot {
            provider: ProviderKind::Gemini,
            status: ProviderStatus::Ok,
            plan_name: Some("API Key".to_string()),
            rate_windows: vec![RateWindow {
                label: "API Status".to_string(),
                used_percent: 0.0,
                reset_at: None,
            }],
            credits_remaining: Some("Active".to_string()),
            cost_30d: None,
            updated_at: chrono::Utc::now(),
        })
    }
}

#[async_trait::async_trait]
impl Provider for GeminiProvider {
    fn kind(&self) -> ProviderKind {
        ProviderKind::Gemini
    }

    async fn fetch_usage(&self) -> Result<UsageSnapshot> {
        let credential = match self.discover_credential() {
            Some(c) => c,
            None => {
                return Ok(UsageSnapshot::not_configured(
                    ProviderKind::Gemini,
                    "No API key configured",
                ));
            }
        };

        let client = match Self::build_client() {
            Ok(c) => c,
            Err(e) => {
                return Ok(UsageSnapshot {
                    provider: ProviderKind::Gemini,
                    status: ProviderStatus::Unavailable(format!("HTTP client error: {}", e)),
                    plan_name: None,
                    rate_windows: Vec::new(),
                    credits_remaining: None,
                    cost_30d: None,
                    updated_at: chrono::Utc::now(),
                });
            }
        };

        let result = match &credential {
            GeminiCredential::OAuthToken(token) => {
                Self::fetch_oauth_quota(&client, token).await
            }
            GeminiCredential::ApiKey(key) => Self::verify_api_key(&client, key).await,
        };

        match result {
            Ok(snapshot) => Ok(snapshot),
            Err(e) => {
                let plan = match &credential {
                    GeminiCredential::OAuthToken(_) => "OAuth",
                    GeminiCredential::ApiKey(_) => "API Key",
                };
                Ok(UsageSnapshot {
                    provider: ProviderKind::Gemini,
                    status: ProviderStatus::Unavailable(format!("Request failed: {}", e)),
                    plan_name: Some(plan.to_string()),
                    rate_windows: Vec::new(),
                    credits_remaining: None,
                    cost_30d: None,
                    updated_at: chrono::Utc::now(),
                })
            }
        }
    }

    fn discover_credentials(&self) -> Option<String> {
        match self.discover_credential()? {
            GeminiCredential::OAuthToken(t) => Some(t),
            GeminiCredential::ApiKey(k) => Some(k),
        }
    }
}
