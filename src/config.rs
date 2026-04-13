use anyhow::Result;
use serde::Deserialize;
use std::path::PathBuf;

#[derive(Debug, Clone, Deserialize)]
pub struct Config {
    #[serde(default)]
    pub general: GeneralConfig,
    #[serde(default)]
    pub claude: ProviderConfig,
    #[serde(default)]
    pub codex: ProviderConfig,
    #[serde(default)]
    pub gemini: ProviderConfig,
}

#[derive(Debug, Clone, Deserialize)]
pub struct GeneralConfig {
    #[serde(default = "default_refresh_interval")]
    pub refresh_interval_secs: u64,
}

fn default_refresh_interval() -> u64 {
    300
}

impl Default for GeneralConfig {
    fn default() -> Self {
        Self {
            refresh_interval_secs: default_refresh_interval(),
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct ProviderConfig {
    #[serde(default = "default_enabled")]
    pub enabled: bool,
    pub token: Option<String>,
    pub api_key: Option<String>,
}

fn default_enabled() -> bool {
    true
}

impl Default for ProviderConfig {
    fn default() -> Self {
        Self {
            enabled: default_enabled(),
            token: None,
            api_key: None,
        }
    }
}

impl Default for Config {
    fn default() -> Self {
        Self {
            general: GeneralConfig::default(),
            claude: ProviderConfig::default(),
            codex: ProviderConfig::default(),
            gemini: ProviderConfig::default(),
        }
    }
}

impl Config {
    pub fn config_path() -> Option<PathBuf> {
        dirs::config_dir().map(|d| d.join("claudetop").join("config.toml"))
    }

    pub fn load() -> Result<Self> {
        let Some(path) = Self::config_path() else {
            tracing::info!("Could not determine config directory, using defaults");
            return Ok(Config::default());
        };

        if !path.exists() {
            tracing::info!("Config file not found at {}, using defaults", path.display());
            return Ok(Config::default());
        }

        let contents = std::fs::read_to_string(&path)?;
        let config: Config = toml::from_str(&contents)?;
        tracing::info!("Loaded config from {}", path.display());
        Ok(config)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_config_has_expected_refresh_interval() {
        let config = Config::default();
        assert_eq!(config.general.refresh_interval_secs, 300);
    }

    #[test]
    fn default_config_providers_are_enabled() {
        let config = Config::default();
        assert!(config.claude.enabled);
        assert!(config.codex.enabled);
        assert!(config.gemini.enabled);
    }

    #[test]
    fn load_returns_defaults_when_no_config_file() {
        // Config::load should succeed even without a config file on disk.
        let config = Config::load().expect("load should not fail");
        assert_eq!(config.general.refresh_interval_secs, 300);
        assert!(config.claude.token.is_none());
    }
}
