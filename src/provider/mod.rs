pub mod claude;
pub mod codex;
pub mod gemini;

use chrono::{DateTime, Utc};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProviderKind {
    Claude,
    Codex,
    Gemini,
}

impl std::fmt::Display for ProviderKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ProviderKind::Claude => write!(f, "Claude"),
            ProviderKind::Codex => write!(f, "Codex"),
            ProviderKind::Gemini => write!(f, "Gemini"),
        }
    }
}

#[derive(Debug, Clone)]
pub enum ProviderStatus {
    Ok,
    Unavailable(String),
    NotConfigured(String),
}

impl ProviderStatus {
    pub fn label(&self) -> &str {
        match self {
            ProviderStatus::Ok => "OK",
            ProviderStatus::Unavailable(msg) => msg,
            ProviderStatus::NotConfigured(msg) => msg,
        }
    }

    pub fn is_ok(&self) -> bool {
        matches!(self, ProviderStatus::Ok)
    }
}

#[derive(Debug, Clone)]
pub struct RateWindow {
    pub label: String,
    pub used_percent: f64,
    pub reset_at: Option<DateTime<Utc>>,
}

#[derive(Debug, Clone)]
pub struct UsageSnapshot {
    pub provider: ProviderKind,
    pub status: ProviderStatus,
    pub plan_name: Option<String>,
    pub rate_windows: Vec<RateWindow>,
    pub credits_remaining: Option<String>,
    pub cost_30d: Option<f64>,
    pub updated_at: DateTime<Utc>,
}

impl UsageSnapshot {
    pub fn not_configured(kind: ProviderKind, reason: &str) -> Self {
        Self {
            provider: kind,
            status: ProviderStatus::NotConfigured(reason.to_string()),
            plan_name: None,
            rate_windows: Vec::new(),
            credits_remaining: None,
            cost_30d: None,
            updated_at: Utc::now(),
        }
    }
}

#[async_trait::async_trait]
pub trait Provider: Send + Sync {
    fn kind(&self) -> ProviderKind;
    async fn fetch_usage(&self) -> anyhow::Result<UsageSnapshot>;
    fn discover_credentials(&self) -> Option<String>;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn usage_snapshot_not_configured_creates_correct_snapshot() {
        let snap = UsageSnapshot::not_configured(ProviderKind::Claude, "no API key");
        assert_eq!(snap.provider, ProviderKind::Claude);
        assert!(matches!(snap.status, ProviderStatus::NotConfigured(_)));
        assert_eq!(snap.status.label(), "no API key");
        assert!(snap.plan_name.is_none());
        assert!(snap.rate_windows.is_empty());
        assert!(snap.credits_remaining.is_none());
        assert!(snap.cost_30d.is_none());
    }

    #[test]
    fn provider_status_label_returns_expected_strings() {
        assert_eq!(ProviderStatus::Ok.label(), "OK");
        assert_eq!(
            ProviderStatus::Unavailable("down".to_string()).label(),
            "down"
        );
        assert_eq!(
            ProviderStatus::NotConfigured("missing key".to_string()).label(),
            "missing key"
        );
    }

    #[test]
    fn provider_status_is_ok() {
        assert!(ProviderStatus::Ok.is_ok());
        assert!(!ProviderStatus::Unavailable("err".to_string()).is_ok());
        assert!(!ProviderStatus::NotConfigured("nc".to_string()).is_ok());
    }

    #[test]
    fn provider_kind_display_formats_correctly() {
        assert_eq!(format!("{}", ProviderKind::Claude), "Claude");
        assert_eq!(format!("{}", ProviderKind::Codex), "Codex");
        assert_eq!(format!("{}", ProviderKind::Gemini), "Gemini");
    }

    #[test]
    fn provider_kind_equality() {
        assert_eq!(ProviderKind::Claude, ProviderKind::Claude);
        assert_ne!(ProviderKind::Claude, ProviderKind::Codex);
    }
}
