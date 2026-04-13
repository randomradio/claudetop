use std::time::Duration;

use anyhow::Result;
use serde::Deserialize;

use crate::config::ProviderConfig;
use crate::provider::{Provider, ProviderKind, ProviderStatus, RateWindow, UsageSnapshot};

const GEMINI_API_BASE: &str = "https://generativelanguage.googleapis.com/v1beta";

/// Response from Gemini generateContent endpoint.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct GenerateContentResponse {
    usage_metadata: Option<UsageMetadata>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct UsageMetadata {
    prompt_token_count: Option<u64>,
    candidates_token_count: Option<u64>,
    #[allow(dead_code)]
    total_token_count: Option<u64>,
}

/// Credential entry from gcloud application default credentials.
#[derive(Debug, Deserialize)]
struct GcloudCredentials {
    /// OAuth2 client credentials won't have this, but ADC service accounts might.
    /// We look for the `client_id` to confirm the file is present / parseable,
    /// but the actual usable token for Gemini API is the `api_key` field or
    /// we fall back — gcloud ADC doesn't directly provide a Gemini API key.
    #[serde(default)]
    #[allow(dead_code)]
    client_id: Option<String>,
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

    /// Try to read an API key from gcloud application default credentials.
    /// This file doesn't normally contain a Gemini API key, but we check it
    /// as a last-resort signal that some Google credentials exist.
    fn read_gcloud_adc() -> Option<String> {
        let home = dirs::home_dir()?;
        let path = home.join(".config/gcloud/application_default_credentials.json");
        let contents = std::fs::read_to_string(&path).ok()?;
        // The ADC file is OAuth-based and doesn't contain a raw API key.
        // We parse it to confirm it exists and is valid JSON, but can't
        // extract a usable Gemini API key from it.
        let _creds: GcloudCredentials = serde_json::from_str(&contents).ok()?;
        tracing::debug!("Found gcloud ADC at {}, but it doesn't contain a Gemini API key", path.display());
        None
    }

    /// Make a minimal generateContent request to verify the API key and gather metadata.
    async fn probe_api(client: &reqwest::Client, api_key: &str) -> Result<UsageSnapshot> {
        let url = format!(
            "{}/models/gemini-2.0-flash:generateContent?key={}",
            GEMINI_API_BASE, api_key
        );

        let body = serde_json::json!({
            "contents": [{"parts": [{"text": "hi"}]}],
            "generationConfig": {"maxOutputTokens": 1}
        });

        let response = client.post(&url).json(&body).send().await?;
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

        if status == reqwest::StatusCode::FORBIDDEN || status == reqwest::StatusCode::UNAUTHORIZED {
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
            let _error_text = response.text().await.unwrap_or_default();
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

        let resp: GenerateContentResponse = response.json().await?;

        let label = if let Some(ref meta) = resp.usage_metadata {
            format!(
                "API Status ({}T prompt, {}T output)",
                meta.prompt_token_count.unwrap_or(0),
                meta.candidates_token_count.unwrap_or(0),
            )
        } else {
            "API Status".to_string()
        };

        Ok(UsageSnapshot {
            provider: ProviderKind::Gemini,
            status: ProviderStatus::Ok,
            plan_name: Some("API Key".to_string()),
            rate_windows: vec![RateWindow {
                label,
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
        let api_key = match self.discover_credentials() {
            Some(k) => k,
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

        match Self::probe_api(&client, &api_key).await {
            Ok(snapshot) => Ok(snapshot),
            Err(e) => Ok(UsageSnapshot {
                provider: ProviderKind::Gemini,
                status: ProviderStatus::Unavailable(format!("Request failed: {}", e)),
                plan_name: Some("API Key".to_string()),
                rate_windows: Vec::new(),
                credits_remaining: None,
                cost_30d: None,
                updated_at: chrono::Utc::now(),
            }),
        }
    }

    fn discover_credentials(&self) -> Option<String> {
        self.config
            .api_key
            .clone()
            .or_else(|| self.config.token.clone())
            .or_else(|| std::env::var("GEMINI_API_KEY").ok())
            .or_else(|| std::env::var("GOOGLE_API_KEY").ok())
            .or_else(|| Self::read_gcloud_adc())
    }
}
