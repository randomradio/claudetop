use std::collections::HashMap;
use std::fs;
use std::path::PathBuf;
use std::time::{Duration, SystemTime};

/// Tracks costs by scanning session logs for token usage.
///
/// Scans JSONL session files from Claude and Codex, parses token usage,
/// and calculates estimated 30-day costs per provider.
pub struct CostTracker {
    costs: HashMap<String, f64>,
}

impl CostTracker {
    pub fn new() -> Self {
        Self {
            costs: HashMap::new(),
        }
    }

    /// Scan local session logs and calculate 30-day costs per provider.
    pub fn scan(&mut self) {
        let mut costs: HashMap<String, f64> = HashMap::new();

        let cutoff = SystemTime::now()
            .checked_sub(Duration::from_secs(30 * 24 * 3600))
            .unwrap_or(SystemTime::UNIX_EPOCH);

        // Scan Claude session logs
        if let Some(claude_dir) = claude_session_dir() {
            let cost = scan_directory(&claude_dir, cutoff);
            if cost > 0.0 {
                costs.insert("claude".to_string(), cost);
            }
        }

        // Codex costs are handled by the Codex provider directly from session logs.

        self.costs = costs;
    }

    /// Return the estimated 30-day cost for a provider.
    pub fn cost_for(&self, provider: &str) -> Option<f64> {
        self.costs.get(provider).copied()
    }
}

impl Default for CostTracker {
    fn default() -> Self {
        Self::new()
    }
}

/// Return the Claude session logs directory (~/.claude/projects/).
fn claude_session_dir() -> Option<PathBuf> {
    dirs::home_dir().map(|h| h.join(".claude").join("projects"))
}

/// Recursively scan a directory for JSONL files modified within the cutoff
/// and sum up token costs.
fn scan_directory(dir: &PathBuf, cutoff: SystemTime) -> f64 {
    let mut total_cost = 0.0;

    let entries = match fs::read_dir(dir) {
        Ok(e) => e,
        Err(_) => return 0.0,
    };

    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            total_cost += scan_directory(&path, cutoff);
            continue;
        }

        // Only process JSONL files
        let ext = path.extension().and_then(|e| e.to_str()).unwrap_or("");
        if ext != "jsonl" {
            continue;
        }

        // Skip files not modified in the last 30 days
        let modified = match path.metadata().and_then(|m| m.modified()) {
            Ok(t) => t,
            Err(_) => continue,
        };
        if modified < cutoff {
            continue;
        }

        total_cost += parse_jsonl_file(&path);
    }

    total_cost
}

/// Parse a JSONL file and return the total cost from token usage entries.
fn parse_jsonl_file(path: &PathBuf) -> f64 {
    let contents = match fs::read_to_string(path) {
        Ok(c) => c,
        Err(_) => return 0.0,
    };

    let mut cost = 0.0;

    for line in contents.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }

        let value: serde_json::Value = match serde_json::from_str(line) {
            Ok(v) => v,
            Err(_) => continue,
        };

        // Look for usage field with token counts
        let usage = match value.get("usage") {
            Some(u) => u,
            None => continue,
        };

        let input_tokens = usage
            .get("input_tokens")
            .and_then(|v| v.as_u64())
            .unwrap_or(0);
        let output_tokens = usage
            .get("output_tokens")
            .and_then(|v| v.as_u64())
            .unwrap_or(0);
        let cache_read = usage
            .get("cache_read_input_tokens")
            .and_then(|v| v.as_u64())
            .unwrap_or(0);

        // Cache reads are typically cheaper (roughly 10% of input price).
        // Subtract cache_read from input_tokens for pricing if present,
        // then price cache reads at the discounted rate.
        let non_cached_input = input_tokens.saturating_sub(cache_read);

        let model = value
            .get("model")
            .and_then(|v| v.as_str())
            .unwrap_or("");

        let (input_price, output_price) = price_per_million(model);
        let cache_price = input_price * 0.1;

        cost += (non_cached_input as f64 * input_price
            + cache_read as f64 * cache_price
            + output_tokens as f64 * output_price)
            / 1_000_000.0;
    }

    cost
}

/// Return (input_price, output_price) per 1M tokens for the given model.
fn price_per_million(model: &str) -> (f64, f64) {
    match model {
        m if m.contains("opus") => (15.0, 75.0),
        m if m.contains("sonnet") => (3.0, 15.0),
        m if m.contains("haiku") => (0.25, 1.25),
        m if m.contains("gpt-4o-mini") => (0.15, 0.60),
        m if m.contains("gpt-4o") => (2.50, 10.0),
        m if m.contains("gpt-4") => (10.0, 30.0),
        m if m.contains("gemini") => (0.075, 0.30),
        _ => (3.0, 15.0), // default to sonnet-level
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    #[test]
    fn price_per_million_known_models() {
        assert_eq!(price_per_million("claude-opus-4"), (15.0, 75.0));
        assert_eq!(price_per_million("claude-sonnet-4"), (3.0, 15.0));
        assert_eq!(price_per_million("claude-haiku-3.5"), (0.25, 1.25));
        assert_eq!(price_per_million("gpt-4o-mini"), (0.15, 0.60));
        assert_eq!(price_per_million("gpt-4o"), (2.50, 10.0));
        assert_eq!(price_per_million("gpt-4-turbo"), (10.0, 30.0));
        assert_eq!(price_per_million("gemini-pro"), (0.075, 0.30));
    }

    #[test]
    fn price_per_million_unknown_model_defaults_to_sonnet() {
        assert_eq!(price_per_million("unknown-model"), (3.0, 15.0));
    }

    #[test]
    fn parse_jsonl_file_computes_cost() {
        let dir = tempfile::tempdir().expect("failed to create temp dir");
        let file_path = dir.path().join("session.jsonl");
        let mut file = fs::File::create(&file_path).expect("failed to create file");

        // Write a JSONL entry with known token counts and a sonnet model
        let line = serde_json::json!({
            "model": "claude-sonnet-4",
            "usage": {
                "input_tokens": 1_000_000,
                "output_tokens": 1_000_000,
                "cache_read_input_tokens": 0
            }
        });
        writeln!(file, "{}", line).expect("failed to write");
        drop(file);

        let cost = parse_jsonl_file(&file_path.to_path_buf());
        // sonnet: input $3/M, output $15/M => 3.0 + 15.0 = 18.0
        assert!((cost - 18.0).abs() < 0.001, "expected ~18.0, got {cost}");
    }

    #[test]
    fn parse_jsonl_file_handles_cache_tokens() {
        let dir = tempfile::tempdir().expect("failed to create temp dir");
        let file_path = dir.path().join("session.jsonl");
        let mut file = fs::File::create(&file_path).expect("failed to create file");

        let line = serde_json::json!({
            "model": "claude-sonnet-4",
            "usage": {
                "input_tokens": 1_000_000,
                "output_tokens": 0,
                "cache_read_input_tokens": 500_000
            }
        });
        writeln!(file, "{}", line).expect("failed to write");
        drop(file);

        let cost = parse_jsonl_file(&file_path.to_path_buf());
        // non_cached = 500_000, cache = 500_000
        // cost = 500_000 * 3.0 / 1M + 500_000 * 0.3 / 1M = 1.5 + 0.15 = 1.65
        assert!((cost - 1.65).abs() < 0.001, "expected ~1.65, got {cost}");
    }

    #[test]
    fn cost_tracker_initializes_empty() {
        let tracker = CostTracker::new();
        assert!(tracker.costs.is_empty());
        assert!(tracker.cost_for("claude").is_none());
        assert!(tracker.cost_for("codex").is_none());
    }
}
