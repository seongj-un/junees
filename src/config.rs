//! Settings read from environment variables.

use std::fmt::Display;
use std::path::{Path, PathBuf};
use std::str::FromStr;
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};

use crate::task::budget::parse_duration;
use crate::tools::ToolSettings;

pub const DEFAULT_BASE_URL: &str = "https://api.anthropic.com";
pub const DEFAULT_MODEL: &str = "claude-sonnet-4-6";
pub const DEFAULT_MAX_TOKENS: u32 = 8192;
pub const DEFAULT_MAX_TURNS: usize = 50;
pub const DEFAULT_CONTEXT_WINDOW: u64 = 200_000;
pub const DEFAULT_COMPACT_THRESHOLD: f64 = 0.8;
/// The smallest extended thinking budget the API accepts.
pub const MIN_THINKING_BUDGET: u32 = 1024;

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
    /// Extended thinking budget in tokens (`JUNEES_THINKING_BUDGET`); 0
    /// leaves thinking off. Otherwise at least [`MIN_THINKING_BUDGET`] and
    /// below `max_tokens`, since thinking counts toward it.
    pub thinking_budget: u32,
    /// Where Junees keeps its state (`$JUNEES_HOME`, default `$HOME/.junees`).
    /// `None` when neither is set (cron, some services and containers):
    /// Junees still runs, it just cannot save or find sessions.
    pub home: Option<PathBuf>,
    /// Settings for the default tools: the bash command policy and the
    /// variables hidden from bash.
    pub tools: ToolSettings,
    /// Default token budget of `task run` (`JUNEES_TASK_MAX_TOKENS`).
    pub task_max_tokens: Option<u64>,
    /// Default time budget of `task run` (`JUNEES_TASK_MAX_TIME`).
    pub task_max_time: Option<Duration>,
    /// Stream model replies, showing text as it is written
    /// (`JUNEES_STREAM`, on by default).
    pub stream: bool,
}

impl Config {
    /// Reads `ANTHROPIC_API_KEY`, `ANTHROPIC_BASE_URL`, `JUNEES_MODEL`,
    /// `JUNEES_MAX_TOKENS`, `JUNEES_MAX_TURNS`, `JUNEES_CONTEXT_WINDOW`,
    /// `JUNEES_COMPACT_THRESHOLD`, `JUNEES_THINKING_BUDGET`, `JUNEES_HOME`, `HOME`,
    /// `JUNEES_DENY_COMMANDS`, `JUNEES_ALLOW_DANGEROUS`,
    /// `JUNEES_HIDDEN_ENV`, `JUNEES_TASK_MAX_TOKENS`,
    /// `JUNEES_TASK_MAX_TIME` and `JUNEES_STREAM`.
    pub fn from_env() -> Result<Self> {
        Self::from_lookup(|key| std::env::var(key).ok())
    }

    pub fn from_lookup(lookup: impl Fn(&str) -> Option<String>) -> Result<Self> {
        Self::read(lookup, true)
    }

    /// [`Config::from_env`] for `task init`/`task run` with the
    /// `claude-code` backend: Claude Code signs in on its own and Junees
    /// calls no API, so `ANTHROPIC_API_KEY` is neither needed nor read
    /// (`api_key` stays empty).
    pub fn from_env_without_key() -> Result<Self> {
        Self::read(|key| std::env::var(key).ok(), false)
    }

    fn read(lookup: impl Fn(&str) -> Option<String>, need_key: bool) -> Result<Self> {
        let get = |key: &str| {
            lookup(key)
                .map(|value| value.trim().to_string())
                .filter(|value| !value.is_empty())
        };

        let api_key = if need_key {
            get("ANTHROPIC_API_KEY").context(
                "ANTHROPIC_API_KEY is not set (get one at https://console.anthropic.com)",
            )?
        } else {
            String::new()
        };
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
        let thinking_budget = parse_or(
            get("JUNEES_THINKING_BUDGET"),
            "JUNEES_THINKING_BUDGET",
            0u32,
        )?;
        if thinking_budget > 0 && thinking_budget < MIN_THINKING_BUDGET {
            bail!(
                "JUNEES_THINKING_BUDGET must be 0 (off) or at least {MIN_THINKING_BUDGET}, \
                 got {thinking_budget}"
            );
        }
        if thinking_budget > 0 && thinking_budget >= max_tokens {
            bail!(
                "JUNEES_THINKING_BUDGET ({thinking_budget}) must be less than \
                 JUNEES_MAX_TOKENS ({max_tokens}): thinking counts toward the output limit \
                 and the answer needs room too"
            );
        }
        let home = get("JUNEES_HOME")
            .map(PathBuf::from)
            .or_else(|| get("HOME").map(|home| PathBuf::from(home).join(".junees")));
        let tools = ToolSettings {
            hidden_env: comma_list(get("JUNEES_HIDDEN_ENV")),
            deny_commands: comma_list(get("JUNEES_DENY_COMMANDS")),
            allow_dangerous: parse_flag(get("JUNEES_ALLOW_DANGEROUS"), "JUNEES_ALLOW_DANGEROUS")?,
        };

        let task_max_tokens = match get("JUNEES_TASK_MAX_TOKENS") {
            None => None,
            Some(raw) => Some(crate::cli::positive(&raw).with_context(|| {
                format!("JUNEES_TASK_MAX_TOKENS must be a positive integer, got `{raw}`")
            })?),
        };
        let task_max_time = get("JUNEES_TASK_MAX_TIME")
            .map(|raw| parse_duration(&raw).context("JUNEES_TASK_MAX_TIME"))
            .transpose()?;
        let stream = match get("JUNEES_STREAM") {
            None => true,
            raw => parse_flag(raw, "JUNEES_STREAM")?,
        };

        Ok(Self {
            api_key,
            base_url: get("ANTHROPIC_BASE_URL").unwrap_or_else(|| DEFAULT_BASE_URL.to_string()),
            model: get("JUNEES_MODEL").unwrap_or_else(|| DEFAULT_MODEL.to_string()),
            max_tokens,
            max_turns,
            context_window,
            compact_threshold,
            thinking_budget,
            home,
            tools,
            task_max_tokens,
            task_max_time,
            stream,
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

pub(crate) fn parse_flag(value: Option<String>, name: &str) -> Result<bool> {
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
    fn the_claude_code_backend_needs_no_api_key() {
        let config =
            Config::read(|key| (key == "JUNEES_MAX_TURNS").then(|| "7".into()), false).unwrap();
        assert_eq!(config.api_key, "");
        assert_eq!(config.max_turns, 7);
        // Not even read when it is there.
        let config = Config::read(
            |key| (key == "ANTHROPIC_API_KEY").then(|| "sk-ant-x".into()),
            false,
        )
        .unwrap();
        assert_eq!(config.api_key, "");
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
        assert_eq!(config.thinking_budget, 0);
        assert_eq!(config.home, Some(PathBuf::from("/home/tester/.junees")));
        assert_eq!(config.tools, ToolSettings::default());
        assert_eq!(config.task_max_tokens, None);
        assert_eq!(config.task_max_time, None);
        assert!(config.stream);
    }

    #[test]
    fn streaming_can_be_turned_off() {
        for (raw, expected) in [("0", false), ("off", false), ("1", true), (" ", true)] {
            let parsed = config(&[("ANTHROPIC_API_KEY", "k"), ("JUNEES_STREAM", raw)]);
            assert_eq!(parsed.unwrap().stream, expected, "{raw}");
        }
        let err = config(&[("ANTHROPIC_API_KEY", "k"), ("JUNEES_STREAM", "maybe")])
            .err()
            .unwrap();
        assert_eq!(err.to_string(), "JUNEES_STREAM must be 1 or 0, got `maybe`");
    }

    #[test]
    fn reads_the_task_budget() {
        let budget = config(&[
            ("ANTHROPIC_API_KEY", "k"),
            ("JUNEES_TASK_MAX_TOKENS", " 500000 "),
            ("JUNEES_TASK_MAX_TIME", "2h"),
        ])
        .unwrap();
        assert_eq!(budget.task_max_tokens, Some(500_000));
        assert_eq!(budget.task_max_time, Some(Duration::from_secs(7200)));
        for (key, bad) in [
            ("JUNEES_TASK_MAX_TOKENS", "0"),
            ("JUNEES_TASK_MAX_TOKENS", "lots"),
            ("JUNEES_TASK_MAX_TIME", "120"),
            ("JUNEES_TASK_MAX_TIME", "2 days"),
        ] {
            let err = config(&[("ANTHROPIC_API_KEY", "k"), (key, bad)])
                .err()
                .unwrap_or_else(|| panic!("{key}={bad} was accepted"));
            assert!(format!("{err:#}").contains(key), "{err:#}");
        }
    }

    #[test]
    fn reads_and_checks_the_thinking_budget() {
        let with = |pairs: &[(&str, &str)]| {
            let mut all = vec![("ANTHROPIC_API_KEY", "k")];
            all.extend_from_slice(pairs);
            config(&all)
        };
        assert_eq!(with(&[]).unwrap().thinking_budget, 0);
        assert_eq!(
            with(&[("JUNEES_THINKING_BUDGET", "0")])
                .unwrap()
                .thinking_budget,
            0
        );
        assert_eq!(
            with(&[("JUNEES_THINKING_BUDGET", " 4096 ")])
                .unwrap()
                .thinking_budget,
            4096
        );
        let ok = with(&[
            ("JUNEES_THINKING_BUDGET", "1024"),
            ("JUNEES_MAX_TOKENS", "1025"),
        ])
        .unwrap();
        assert_eq!((ok.thinking_budget, ok.max_tokens), (1024, 1025));

        for (pairs, expected) in [
            (vec![("JUNEES_THINKING_BUDGET", "1023")], "at least 1024"),
            (vec![("JUNEES_THINKING_BUDGET", "lots")], "positive integer"),
            (vec![("JUNEES_THINKING_BUDGET", "-1")], "positive integer"),
            // The default JUNEES_MAX_TOKENS is 8192.
            (
                vec![("JUNEES_THINKING_BUDGET", "8192")],
                "less than JUNEES_MAX_TOKENS (8192)",
            ),
            (
                vec![
                    ("JUNEES_THINKING_BUDGET", "2048"),
                    ("JUNEES_MAX_TOKENS", "2048"),
                ],
                "less than JUNEES_MAX_TOKENS (2048)",
            ),
        ] {
            let err = with(&pairs)
                .err()
                .unwrap_or_else(|| panic!("{pairs:?} was accepted"))
                .to_string();
            assert!(err.contains("JUNEES_THINKING_BUDGET"), "{err}");
            assert!(err.contains(expected), "{err}");
        }
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
