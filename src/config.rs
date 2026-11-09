//! Settings read from environment variables.

use std::fmt::Display;
use std::str::FromStr;

use anyhow::{Context, Result, anyhow, bail};

pub const DEFAULT_BASE_URL: &str = "https://api.anthropic.com";
pub const DEFAULT_MODEL: &str = "claude-sonnet-4-6";
pub const DEFAULT_MAX_TOKENS: u32 = 8192;
pub const DEFAULT_MAX_TURNS: usize = 50;

/// Runtime settings. Deliberately not `Debug`, so the API key cannot end up
/// in logs by accident.
#[derive(Clone)]
pub struct Config {
    pub api_key: String,
    pub base_url: String,
    pub model: String,
    pub max_tokens: u32,
    pub max_turns: usize,
}

impl Config {
    /// Reads `ANTHROPIC_API_KEY`, `ANTHROPIC_BASE_URL`, `JUNEES_MODEL`,
    /// `JUNEES_MAX_TOKENS` and `JUNEES_MAX_TURNS`.
    pub fn from_env() -> Result<Self> {
        Self::from_lookup(|key| std::env::var(key).ok())
    }

    pub fn from_lookup(lookup: impl Fn(&str) -> Option<String>) -> Result<Self> {
        let get = |key: &str| {
            lookup(key)
                .map(|value| value.trim().to_string())
                .filter(|value| !value.is_empty())
        };

        let api_key = get("ANTHROPIC_API_KEY")
            .context("ANTHROPIC_API_KEY is not set (get one at https://console.anthropic.com)")?;
        let max_tokens = parse_or(
            get("JUNEES_MAX_TOKENS"),
            "JUNEES_MAX_TOKENS",
            DEFAULT_MAX_TOKENS,
        )?;
        let max_turns = parse_or(
            get("JUNEES_MAX_TURNS"),
            "JUNEES_MAX_TURNS",
            DEFAULT_MAX_TURNS,
        )?;
        if max_tokens == 0 {
            bail!("JUNEES_MAX_TOKENS must be greater than 0");
        }
        if max_turns == 0 {
            bail!("JUNEES_MAX_TURNS must be greater than 0");
        }

        Ok(Self {
            api_key,
            base_url: get("ANTHROPIC_BASE_URL").unwrap_or_else(|| DEFAULT_BASE_URL.to_string()),
            model: get("JUNEES_MODEL").unwrap_or_else(|| DEFAULT_MODEL.to_string()),
            max_tokens,
            max_turns,
        })
    }
}

fn parse_or<T>(value: Option<String>, name: &str, default: T) -> Result<T>
where
    T: FromStr,
    T::Err: Display,
{
    match value {
        None => Ok(default),
        Some(raw) => raw
            .parse()
            .map_err(|err| anyhow!("{name} must be a positive integer, got `{raw}`: {err}")),
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use super::*;

    fn config(pairs: &[(&str, &str)]) -> Result<Config> {
        let map: HashMap<String, String> = pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();
        Config::from_lookup(|key| map.get(key).cloned())
    }

    #[test]
    fn requires_api_key() {
        let err = config(&[]).err().unwrap();
        assert!(err.to_string().contains("ANTHROPIC_API_KEY"));
        assert!(config(&[("ANTHROPIC_API_KEY", "  ")]).is_err());
    }

    #[test]
    fn uses_defaults() {
        let config = config(&[("ANTHROPIC_API_KEY", "sk-test")]).unwrap();
        assert_eq!(config.api_key, "sk-test");
        assert_eq!(config.base_url, DEFAULT_BASE_URL);
        assert_eq!(config.model, DEFAULT_MODEL);
        assert_eq!(config.max_tokens, DEFAULT_MAX_TOKENS);
        assert_eq!(config.max_turns, DEFAULT_MAX_TURNS);
    }

    #[test]
    fn reads_overrides() {
        let config = config(&[
            ("ANTHROPIC_API_KEY", "sk-test"),
            ("ANTHROPIC_BASE_URL", "http://localhost:8080"),
            ("JUNEES_MODEL", "claude-haiku-4-5"),
            ("JUNEES_MAX_TOKENS", "1024"),
            ("JUNEES_MAX_TURNS", " 7 "),
        ])
        .unwrap();
        assert_eq!(config.base_url, "http://localhost:8080");
        assert_eq!(config.model, "claude-haiku-4-5");
        assert_eq!(config.max_tokens, 1024);
        assert_eq!(config.max_turns, 7);
    }

    #[test]
    fn rejects_bad_numbers() {
        let err = config(&[("ANTHROPIC_API_KEY", "k"), ("JUNEES_MAX_TURNS", "many")])
            .err()
            .unwrap();
        assert!(
            err.to_string()
                .contains("JUNEES_MAX_TURNS must be a positive integer")
        );
        assert!(config(&[("ANTHROPIC_API_KEY", "k"), ("JUNEES_MAX_TURNS", "0")]).is_err());
        assert!(config(&[("ANTHROPIC_API_KEY", "k"), ("JUNEES_MAX_TOKENS", "-5")]).is_err());
    }
}
