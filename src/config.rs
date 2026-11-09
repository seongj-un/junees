//! Settings read from environment variables.

use std::fmt::Display;
use std::path::{Path, PathBuf};
use std::str::FromStr;

use anyhow::{Context, Result, anyhow, bail};

pub const DEFAULT_BASE_URL: &str = "https://api.anthropic.com";
pub const DEFAULT_MODEL: &str = "claude-sonnet-4-6";
pub const DEFAULT_MAX_TOKENS: u32 = 8192;
pub const DEFAULT_MAX_TURNS: usize = 50;
pub const DEFAULT_CONTEXT_WINDOW: u64 = 200_000;
pub const DEFAULT_COMPACT_THRESHOLD: f64 = 0.8;

/// Runtime settings. Deliberately not `Debug`, so the API key cannot end up
/// in logs by accident.
#[derive(Clone)]
pub struct Config {
    pub api_key: String,
    pub base_url: String,
    pub model: String,
    pub max_tokens: u32,
    pub max_turns: usize,
    /// The model's context window in tokens.
    pub context_window: u64,
    /// Share of the context window at which old tool output is cleared and
    /// then the conversation compacted; in (0, 1].
    pub compact_threshold: f64,
    /// Where Junees keeps its state (`$JUNEES_HOME`, default `$HOME/.junees`).
    /// `None` when neither is set (cron, some services and containers):
    /// Junees still runs, it just cannot save or find sessions.
    pub home: Option<PathBuf>,
}

impl Config {
    /// Reads `ANTHROPIC_API_KEY`, `ANTHROPIC_BASE_URL`, `JUNEES_MODEL`,
    /// `JUNEES_MAX_TOKENS`, `JUNEES_MAX_TURNS`, `JUNEES_CONTEXT_WINDOW`,
    /// `JUNEES_COMPACT_THRESHOLD`, `JUNEES_HOME` and `HOME`.
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
        let context_window = parse_or(
            get("JUNEES_CONTEXT_WINDOW"),
            "JUNEES_CONTEXT_WINDOW",
            DEFAULT_CONTEXT_WINDOW,
        )?;
        if context_window == 0 {
            bail!("JUNEES_CONTEXT_WINDOW must be greater than 0");
        }
        let compact_threshold = match get("JUNEES_COMPACT_THRESHOLD") {
            None => DEFAULT_COMPACT_THRESHOLD,
            Some(raw) => match raw.parse::<f64>() {
                Ok(value) if value > 0.0 && value <= 1.0 => value,
                _ => bail!(
                    "JUNEES_COMPACT_THRESHOLD must be a number greater than 0 and at most 1, \
                     got `{raw}`"
                ),
            },
        };
        let home = get("JUNEES_HOME")
            .map(PathBuf::from)
            .or_else(|| get("HOME").map(|home| PathBuf::from(home).join(".junees")));

        Ok(Self {
            api_key,
            base_url: get("ANTHROPIC_BASE_URL").unwrap_or_else(|| DEFAULT_BASE_URL.to_string()),
            model: get("JUNEES_MODEL").unwrap_or_else(|| DEFAULT_MODEL.to_string()),
            max_tokens,
            max_turns,
            context_window,
            compact_threshold,
            home,
        })
    }

    /// The state folder, for the things that need one.
    pub fn require_home(&self) -> Result<&Path> {
        self.home
            .as_deref()
            .context("neither JUNEES_HOME nor HOME is set")
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

    fn lookup(pairs: &[(&str, &str)]) -> Result<Config> {
        let map: HashMap<String, String> = pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();
        Config::from_lookup(|key| map.get(key).cloned())
    }

    /// Like [`lookup`], with `HOME` set unless the pairs set it.
    fn config(pairs: &[(&str, &str)]) -> Result<Config> {
        let mut pairs = pairs.to_vec();
        if !pairs.iter().any(|(key, _)| *key == "HOME") {
            pairs.push(("HOME", "/home/tester"));
        }
        lookup(&pairs)
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
        assert_eq!(config.context_window, 200_000);
        assert_eq!(config.compact_threshold, 0.8);
        assert_eq!(config.home, Some(PathBuf::from("/home/tester/.junees")));
    }

    #[test]
    fn junees_home_overrides_home() {
        let config = config(&[("ANTHROPIC_API_KEY", "k"), ("JUNEES_HOME", "/data/j")]).unwrap();
        assert_eq!(config.home, Some(PathBuf::from("/data/j")));
        let config = lookup(&[("ANTHROPIC_API_KEY", "k"), ("JUNEES_HOME", "/data/j")]).unwrap();
        assert_eq!(config.home, Some(PathBuf::from("/data/j")));
    }

    #[test]
    fn home_is_only_needed_for_sessions() {
        let config = lookup(&[("ANTHROPIC_API_KEY", "k")]).unwrap();
        assert_eq!(config.home, None);
        let err = config.require_home().unwrap_err();
        assert_eq!(err.to_string(), "neither JUNEES_HOME nor HOME is set");
        let config = lookup(&[("ANTHROPIC_API_KEY", "k"), ("HOME", " ")]).unwrap();
        assert_eq!(config.home, None);
    }

    #[test]
    fn reads_overrides() {
        let config = config(&[
            ("ANTHROPIC_API_KEY", "sk-test"),
            ("ANTHROPIC_BASE_URL", "http://localhost:8080"),
            ("JUNEES_MODEL", "claude-haiku-4-5"),
            ("JUNEES_MAX_TOKENS", "1024"),
            ("JUNEES_MAX_TURNS", " 7 "),
            ("JUNEES_CONTEXT_WINDOW", "1000000"),
            ("JUNEES_COMPACT_THRESHOLD", "1"),
        ])
        .unwrap();
        assert_eq!(config.context_window, 1_000_000);
        assert_eq!(config.compact_threshold, 1.0);
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

    #[test]
    fn compact_threshold_must_be_a_share_of_the_window() {
        for bad in ["0", "-0.5", "1.01", "80%", "NaN", "inf"] {
            let err = config(&[
                ("ANTHROPIC_API_KEY", "k"),
                ("JUNEES_COMPACT_THRESHOLD", bad),
            ])
            .err()
            .unwrap_or_else(|| panic!("{bad} was accepted"));
            assert!(
                err.to_string().contains("JUNEES_COMPACT_THRESHOLD"),
                "{err}"
            );
        }
        let ok = config(&[
            ("ANTHROPIC_API_KEY", "k"),
            ("JUNEES_COMPACT_THRESHOLD", "0.5"),
        ]);
        assert_eq!(ok.unwrap().compact_threshold, 0.5);
        assert!(config(&[("ANTHROPIC_API_KEY", "k"), ("JUNEES_CONTEXT_WINDOW", "0")]).is_err());
    }
}
