//! Settings read from environment variables.

use std::fmt::Display;
use std::path::{Path, PathBuf};
use std::str::FromStr;

use anyhow::{Context, Result, anyhow, bail};

use crate::tools::ToolSettings;

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
    /// Where Junees keeps its state (`$JUNEES_HOME`, default `$HOME/.junees`).
    /// `None` when neither is set (cron, some services and containers):
    /// Junees still runs, it just cannot save or find sessions.
    pub home: Option<PathBuf>,
    /// Settings for the default tools: the bash command policy and the
    /// variables hidden from bash.
    pub tools: ToolSettings,
}

impl Config {
    /// Reads `ANTHROPIC_API_KEY`, `ANTHROPIC_BASE_URL`, `JUNEES_MODEL`,
    /// `JUNEES_MAX_TOKENS`, `JUNEES_MAX_TURNS`, `JUNEES_HOME`, `HOME`,
    /// `JUNEES_DENY_COMMANDS`, `JUNEES_ALLOW_DANGEROUS` and
    /// `JUNEES_HIDDEN_ENV`.
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
        let home = get("JUNEES_HOME")
            .map(PathBuf::from)
            .or_else(|| get("HOME").map(|home| PathBuf::from(home).join(".junees")));
        let tools = ToolSettings {
            hidden_env: comma_list(get("JUNEES_HIDDEN_ENV")),
            deny_commands: comma_list(get("JUNEES_DENY_COMMANDS")),
            allow_dangerous: parse_flag(get("JUNEES_ALLOW_DANGEROUS"), "JUNEES_ALLOW_DANGEROUS")?,
        };

        Ok(Self {
            api_key,
            base_url: get("ANTHROPIC_BASE_URL").unwrap_or_else(|| DEFAULT_BASE_URL.to_string()),
            model: get("JUNEES_MODEL").unwrap_or_else(|| DEFAULT_MODEL.to_string()),
            max_tokens,
            max_turns,
            home,
            tools,
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

/// Splits `a, b,,c` into `["a", "b", "c"]`.
fn comma_list(value: Option<String>) -> Vec<String> {
    value
        .iter()
        .flat_map(|value| value.split(','))
        .map(str::trim)
        .filter(|item| !item.is_empty())
        .map(str::to_string)
        .collect()
}

fn parse_flag(value: Option<String>, name: &str) -> Result<bool> {
    match value.as_deref().map(str::to_ascii_lowercase).as_deref() {
        None | Some("0" | "false" | "no" | "off") => Ok(false),
        Some("1" | "true" | "yes" | "on") => Ok(true),
        Some(_) => bail!("{name} must be 1 or 0, got `{}`", value.unwrap_or_default()),
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
        assert_eq!(config.home, Some(PathBuf::from("/home/tester/.junees")));
        assert_eq!(config.tools, ToolSettings::default());
    }

    #[test]
    fn reads_hidden_env() {
        let config = config(&[
            ("ANTHROPIC_API_KEY", "k"),
            ("JUNEES_HIDDEN_ENV", "DB_PASSWORD, GITHUB_TOKEN,"),
        ])
        .unwrap();
        assert_eq!(config.tools.hidden_env, ["DB_PASSWORD", "GITHUB_TOKEN"]);
    }

    #[test]
    fn reads_command_policy_settings() {
        let tools = config(&[
            ("ANTHROPIC_API_KEY", "k"),
            ("JUNEES_DENY_COMMANDS", " docker rm, kubectl delete ,, "),
            ("JUNEES_ALLOW_DANGEROUS", "1"),
        ])
        .unwrap()
        .tools;
        assert_eq!(tools.deny_commands, ["docker rm", "kubectl delete"]);
        assert!(tools.allow_dangerous);

        for (raw, expected) in [("0", false), ("true", true), ("No", false), ("", false)] {
            let parsed = config(&[("ANTHROPIC_API_KEY", "k"), ("JUNEES_ALLOW_DANGEROUS", raw)]);
            assert_eq!(parsed.unwrap().tools.allow_dangerous, expected, "{raw}");
        }
        let err = config(&[
            ("ANTHROPIC_API_KEY", "k"),
            ("JUNEES_ALLOW_DANGEROUS", "maybe"),
        ])
        .err()
        .unwrap();
        assert_eq!(
            err.to_string(),
            "JUNEES_ALLOW_DANGEROUS must be 1 or 0, got `maybe`"
        );
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
