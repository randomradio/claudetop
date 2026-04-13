use anyhow::Result;
use chrono::{DateTime, TimeZone, Utc};
use serde::Deserialize;
use std::fs;
use std::path::PathBuf;
use std::time::{Duration, SystemTime};

use crate::config::ProviderConfig;
use crate::provider::{Provider, ProviderKind, ProviderStatus, RateWindow, UsageSnapshot};

/// Parsed from `event_msg` entries where `payload.type == "token_count"`.
#[derive(Debug, Deserialize)]
struct SessionEntry {
    #[serde(rename = "type")]
    entry_type: String,
    #[serde(default)]
    payload: serde_json::Value,
}

#[derive(Debug, Deserialize)]
struct TokenCountPayload {
    info: TokenCountInfo,
    #[serde(default)]
    rate_limits: Option<RateLimits>,
}

#[derive(Debug, Deserialize)]
struct TokenCountInfo {
    total_token_usage: TokenUsage,
}

#[derive(Debug, Deserialize)]
struct TokenUsage {
    #[serde(default)]
    input_tokens: u64,
    #[serde(default)]
    cached_input_tokens: u64,
    #[serde(default)]
    output_tokens: u64,
    #[serde(default)]
    reasoning_output_tokens: u64,
}

#[derive(Debug, Deserialize)]
struct RateLimits {
    #[serde(default)]
    limit_name: Option<String>,
    #[serde(default)]
    primary: Option<RateWindow_>,
    #[serde(default)]
    secondary: Option<RateWindow_>,
    #[serde(default)]
    plan_type: Option<String>,
}

#[derive(Debug, Deserialize)]
struct RateWindow_ {
    used_percent: f64,
    window_minutes: u64,
    resets_at: i64,
}

pub struct CodexProvider {
    #[allow(dead_code)]
    config: ProviderConfig,
}

impl CodexProvider {
    pub fn new(config: ProviderConfig) -> Self {
        Self { config }
    }

    /// Return the Codex sessions directory (~/.codex/sessions/).
    fn sessions_dir() -> Option<PathBuf> {
        dirs::home_dir().map(|h| h.join(".codex").join("sessions"))
    }

    /// Find all JSONL session files modified within the last 30 days.
    fn find_recent_sessions(dir: &PathBuf, cutoff: SystemTime) -> Vec<PathBuf> {
        let mut files = Vec::new();
        Self::collect_jsonl_files(dir, cutoff, &mut files);
        files
    }

    fn collect_jsonl_files(dir: &PathBuf, cutoff: SystemTime, out: &mut Vec<PathBuf>) {
        let entries = match fs::read_dir(dir) {
            Ok(e) => e,
            Err(_) => return,
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                Self::collect_jsonl_files(&path, cutoff, out);
                continue;
            }
            if path.extension().and_then(|e| e.to_str()) != Some("jsonl") {
                continue;
            }
            if let Ok(meta) = path.metadata() {
                if let Ok(modified) = meta.modified() {
                    if modified >= cutoff {
                        out.push(path);
                    }
                }
            }
        }
    }

    /// Parse a single session JSONL and return the last token_count entry's data.
    /// The last entry has cumulative totals for that session.
    fn parse_session(path: &PathBuf) -> Option<(TokenUsage, Option<RateLimits>)> {
        let contents = fs::read_to_string(path).ok()?;
        let mut last_usage: Option<TokenUsage> = None;
        let mut last_rate_limits: Option<RateLimits> = None;

        for line in contents.lines() {
            let line = line.trim();
            if line.is_empty() {
                continue;
            }
            let entry: SessionEntry = match serde_json::from_str(line) {
                Ok(e) => e,
                Err(_) => continue,
            };
            if entry.entry_type != "event_msg" {
                continue;
            }
            let payload_type = entry.payload.get("type").and_then(|v| v.as_str());
            if payload_type != Some("token_count") {
                continue;
            }
            let tc: TokenCountPayload = match serde_json::from_value(entry.payload) {
                Ok(tc) => tc,
                Err(_) => continue,
            };
            last_usage = Some(tc.info.total_token_usage);
            last_rate_limits = tc.rate_limits;
        }

        last_usage.map(|u| (u, last_rate_limits))
    }

    /// Estimate cost from token usage using Codex/OpenAI pricing.
    fn estimate_cost(usage: &TokenUsage) -> f64 {
        // GPT-5 / Codex pricing estimate (per 1M tokens):
        // Non-cached input: ~$2.50, Cached input: ~$0.25, Output: ~$10.00
        let non_cached_input = usage.input_tokens.saturating_sub(usage.cached_input_tokens);
        let input_cost = non_cached_input as f64 * 2.50 / 1_000_000.0;
        let cache_cost = usage.cached_input_tokens as f64 * 0.25 / 1_000_000.0;
        let output_cost = usage.output_tokens as f64 * 10.0 / 1_000_000.0;
        input_cost + cache_cost + output_cost
    }
}

#[async_trait::async_trait]
impl Provider for CodexProvider {
    fn kind(&self) -> ProviderKind {
        ProviderKind::Codex
    }

    async fn fetch_usage(&self) -> Result<UsageSnapshot> {
        let sessions_dir = match Self::sessions_dir() {
            Some(d) if d.exists() => d,
            _ => {
                return Ok(UsageSnapshot::not_configured(
                    ProviderKind::Codex,
                    "No Codex sessions found (~/.codex/sessions/)",
                ));
            }
        };

        let cutoff = SystemTime::now()
            .checked_sub(Duration::from_secs(30 * 24 * 3600))
            .unwrap_or(SystemTime::UNIX_EPOCH);

        let session_files = Self::find_recent_sessions(&sessions_dir, cutoff);
        if session_files.is_empty() {
            return Ok(UsageSnapshot::not_configured(
                ProviderKind::Codex,
                "No recent Codex sessions found",
            ));
        }

        // Sum token usage across all recent sessions, keep rate_limits from the newest.
        let mut total_input: u64 = 0;
        let mut total_cached: u64 = 0;
        let mut total_output: u64 = 0;
        let mut latest_rate_limits: Option<RateLimits> = None;

        // Sort by path (contains date) to process chronologically.
        let mut sorted_files = session_files;
        sorted_files.sort();

        for path in &sorted_files {
            if let Some((usage, rate_limits)) = Self::parse_session(path) {
                total_input += usage.input_tokens;
                total_cached += usage.cached_input_tokens;
                total_output += usage.output_tokens;
                // Keep overwriting — last file = most recent rate limits.
                if rate_limits.is_some() {
                    latest_rate_limits = rate_limits;
                }
            }
        }

        let total_usage = TokenUsage {
            input_tokens: total_input,
            cached_input_tokens: total_cached,
            output_tokens: total_output,
            reasoning_output_tokens: 0,
        };

        let cost_30d = Self::estimate_cost(&total_usage);

        // Build rate windows from the latest session's rate limits.
        let mut rate_windows = Vec::new();
        let mut plan_name: Option<String> = None;

        if let Some(ref rl) = latest_rate_limits {
            plan_name = rl.plan_type.clone();

            if let Some(ref primary) = rl.primary {
                let reset_at: Option<DateTime<Utc>> = Utc.timestamp_opt(primary.resets_at, 0).single();
                rate_windows.push(RateWindow {
                    label: format!("{}h window", primary.window_minutes / 60),
                    used_percent: primary.used_percent,
                    reset_at,
                });
            }

            if let Some(ref secondary) = rl.secondary {
                let reset_at: Option<DateTime<Utc>> = Utc.timestamp_opt(secondary.resets_at, 0).single();
                rate_windows.push(RateWindow {
                    label: format!("{}d window", secondary.window_minutes / 60 / 24),
                    used_percent: secondary.used_percent,
                    reset_at,
                });
            }
        }

        Ok(UsageSnapshot {
            provider: ProviderKind::Codex,
            status: ProviderStatus::Ok,
            plan_name,
            rate_windows,
            credits_remaining: None,
            cost_30d: Some(cost_30d),
            updated_at: Utc::now(),
        })
    }

    fn discover_credentials(&self) -> Option<String> {
        // Not needed for local log scanning, but kept for trait.
        Self::sessions_dir()
            .filter(|d| d.exists())
            .map(|d| d.to_string_lossy().to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    #[test]
    fn estimate_cost_basic() {
        let usage = TokenUsage {
            input_tokens: 1_000_000,
            cached_input_tokens: 0,
            output_tokens: 1_000_000,
            reasoning_output_tokens: 0,
        };
        let cost = CodexProvider::estimate_cost(&usage);
        // 1M * $2.50/M + 1M * $10/M = $12.50
        assert!((cost - 12.50).abs() < 0.01, "expected ~12.50, got {cost}");
    }

    #[test]
    fn estimate_cost_with_cache() {
        let usage = TokenUsage {
            input_tokens: 1_000_000,
            cached_input_tokens: 800_000,
            output_tokens: 100_000,
            reasoning_output_tokens: 0,
        };
        let cost = CodexProvider::estimate_cost(&usage);
        // non-cached: 200k * 2.50/M = 0.50
        // cached: 800k * 0.25/M = 0.20
        // output: 100k * 10/M = 1.00
        // total = 1.70
        assert!((cost - 1.70).abs() < 0.01, "expected ~1.70, got {cost}");
    }

    #[test]
    fn parse_session_extracts_token_count() {
        let dir = tempfile::tempdir().expect("create temp dir");
        let path = dir.path().join("session.jsonl");
        let mut f = fs::File::create(&path).expect("create file");

        let entry = serde_json::json!({
            "timestamp": "2026-04-12T15:00:00Z",
            "type": "event_msg",
            "payload": {
                "type": "token_count",
                "info": {
                    "total_token_usage": {
                        "input_tokens": 5000,
                        "cached_input_tokens": 3000,
                        "output_tokens": 1000,
                        "reasoning_output_tokens": 500,
                        "total_tokens": 6000
                    }
                },
                "rate_limits": {
                    "limit_id": "codex",
                    "limit_name": null,
                    "primary": {
                        "used_percent": 20.0,
                        "window_minutes": 300,
                        "resets_at": 1776010778
                    },
                    "secondary": {
                        "used_percent": 8.0,
                        "window_minutes": 10080,
                        "resets_at": 1776395455
                    },
                    "plan_type": "pro"
                }
            }
        });
        writeln!(f, "{}", entry).expect("write");
        drop(f);

        let result = CodexProvider::parse_session(&path.to_path_buf());
        assert!(result.is_some());
        let (usage, rate_limits) = result.unwrap();
        assert_eq!(usage.input_tokens, 5000);
        assert_eq!(usage.cached_input_tokens, 3000);
        assert_eq!(usage.output_tokens, 1000);

        let rl = rate_limits.unwrap();
        assert_eq!(rl.plan_type.as_deref(), Some("pro"));
        assert!((rl.primary.unwrap().used_percent - 20.0).abs() < 0.01);
    }

    #[test]
    fn parse_session_returns_last_entry() {
        let dir = tempfile::tempdir().expect("create temp dir");
        let path = dir.path().join("session.jsonl");
        let mut f = fs::File::create(&path).expect("create file");

        // First entry
        let entry1 = serde_json::json!({
            "type": "event_msg",
            "payload": {
                "type": "token_count",
                "info": {
                    "total_token_usage": {
                        "input_tokens": 100,
                        "cached_input_tokens": 0,
                        "output_tokens": 50,
                        "reasoning_output_tokens": 0,
                        "total_tokens": 150
                    }
                }
            }
        });
        writeln!(f, "{}", entry1).expect("write");

        // Second (later) entry — should win
        let entry2 = serde_json::json!({
            "type": "event_msg",
            "payload": {
                "type": "token_count",
                "info": {
                    "total_token_usage": {
                        "input_tokens": 5000,
                        "cached_input_tokens": 2000,
                        "output_tokens": 1000,
                        "reasoning_output_tokens": 0,
                        "total_tokens": 6000
                    }
                }
            }
        });
        writeln!(f, "{}", entry2).expect("write");
        drop(f);

        let (usage, _) = CodexProvider::parse_session(&path.to_path_buf()).unwrap();
        assert_eq!(usage.input_tokens, 5000);
        assert_eq!(usage.output_tokens, 1000);
    }
}
