//! Settings read from environment variables (and, through
//! [`crate::config_file::Settings`], the settings file).

use std::fmt::Display;
use std::path::{Path, PathBuf};
use std::str::FromStr;
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};

use crate::approval::ApprovalMode;
use crate::memory::Limits;
use crate::subagent;
use crate::task::budget::parse_duration;
use crate::tools::{SettingsFile, ToolSettings};
use crate::web_search::WebSearch;

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
    /// Which tool calls the conversation mode asks about first
    /// (`JUNEES_APPROVAL`).
    pub approval: ApprovalMode,
    /// Default token budget of `task run` (`JUNEES_TASK_MAX_TOKENS`).
    pub task_max_tokens: Option<u64>,
    /// Default time budget of `task run` (`JUNEES_TASK_MAX_TIME`).
    pub task_max_time: Option<Duration>,
    /// Lets task sessions change skills with `skill_manage`
    /// (`JUNEES_TASK_SKILL_MANAGE`, off by default).
    pub task_skill_manage: bool,
    /// Stream model replies, showing text as it is written
    /// (`JUNEES_STREAM`, on by default).
    pub stream: bool,
    /// The API's web search tool, when it is turned on (`JUNEES_WEB_SEARCH`
    /// and its settings; see [`WebSearch::read`]).
    pub web_search: Option<WebSearch>,
    /// Character limits of the memory stores (`JUNEES_MEMORY_*_CHARS`).
    pub memory: Limits,
    /// The `delegate` tool's settings (`JUNEES_SUBAGENTS` and
    /// `JUNEES_SUBAGENT_*`); `None` turns it off.
    pub subagents: Option<subagent::Settings>,
    /// The `github` tool's settings (`JUNEES_GITHUB_*`).
    pub github: crate::github::Settings,
    /// The project instruction files a session reads
    /// (`JUNEES_CONTEXT_FILES`).
    pub context_files: crate::context_files::Setting,
}

impl Config {
    /// Reads `ANTHROPIC_API_KEY`, `ANTHROPIC_BASE_URL`, `JUNEES_MODEL`,
    /// `JUNEES_MAX_TOKENS`, `JUNEES_MAX_TURNS`, `JUNEES_CONTEXT_WINDOW`,
    /// `JUNEES_COMPACT_THRESHOLD`, `JUNEES_THINKING_BUDGET`, `JUNEES_HOME`, `HOME`,
    /// `JUNEES_DENY_COMMANDS`, `JUNEES_ALLOW_DANGEROUS`,
    /// `JUNEES_HIDDEN_ENV`, `JUNEES_APPROVAL`, `JUNEES_TASK_MAX_TOKENS`,
    /// `JUNEES_TASK_MAX_TIME`, `JUNEES_TASK_SKILL_MANAGE`, `JUNEES_STREAM`,
    /// the `JUNEES_WEB_SEARCH*` settings, the `JUNEES_MEMORY_*_CHARS`
    /// limits, the subagent settings and `JUNEES_CONTEXT_FILES`.
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
        Self::from_lookup_without_key(|key| std::env::var(key).ok())
    }

    /// [`Config::from_env_without_key`] with the variables from `lookup`.
    pub fn from_lookup_without_key(lookup: impl Fn(&str) -> Option<String>) -> Result<Self> {
        Self::read(lookup, false)
    }

    fn read(lookup: impl Fn(&str) -> Option<String>, need_key: bool) -> Result<Self> {
        let get = |key: &str| {
            lookup(key)
                .map(|value| value.trim().to_string())
                .filter(|value| !value.is_empty())
        };

        let api_key = if need_key {
            get("ANTHROPIC_API_KEY").context(
                "no API key: run `junees setup`, or set ANTHROPIC_API_KEY \
                 (get a key at https://console.anthropic.com)",
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
        let home = crate::config_file::home_dir(get);
        let context_files = crate::context_files::Setting::read(&lookup)?;
        let mut hidden_env = comma_list(get("JUNEES_HIDDEN_ENV"));
        // Header values of MCP HTTP servers come from these variables.
        for name in home
            .as_deref()
            .map(crate::mcp::config::header_env_names)
            .unwrap_or_default()
        {
            if !hidden_env.contains(&name) {
                hidden_env.push(name);
            }
        }
        let tools = ToolSettings {
            hidden_env,
            deny_commands: comma_list(get("JUNEES_DENY_COMMANDS")),
            allow_dangerous: parse_flag(get("JUNEES_ALLOW_DANGEROUS"), "JUNEES_ALLOW_DANGEROUS")?,
            settings_file: crate::config_file::path(get).map(|path| {
                SettingsFile::new(path, get("HOME").map(|home| PathBuf::from(home.trim())))
                    .with_config_home(get("XDG_CONFIG_HOME").map(|dir| PathBuf::from(dir.trim())))
            }),
            approval_gate: false,
            webhook: false,
            gh_config_dir: None,
            instruction_files: context_files.names.clone(),
            sandbox: Some(crate::sandbox::Settings::read(get)?),
        };
        let approval = ApprovalMode::parse(get("JUNEES_APPROVAL").as_deref())?;

        let task_max_tokens = match get("JUNEES_TASK_MAX_TOKENS") {
            None => None,
            Some(raw) => Some(crate::cli::positive(&raw).with_context(|| {
                format!("JUNEES_TASK_MAX_TOKENS must be a positive integer, got `{raw}`")
            })?),
        };
        let task_max_time = get("JUNEES_TASK_MAX_TIME")
            .map(|raw| parse_duration(&raw).context("JUNEES_TASK_MAX_TIME"))
            .transpose()?;
        let task_skill_manage =
            parse_flag(get("JUNEES_TASK_SKILL_MANAGE"), "JUNEES_TASK_SKILL_MANAGE")?;
        let stream = match get("JUNEES_STREAM") {
            None => true,
            raw => parse_flag(raw, "JUNEES_STREAM")?,
        };
        let web_search = WebSearch::read(get)?;
        let memory = Limits::read(get)?;
        let subagents = subagent::Settings::read(get)?;
        let github = crate::github::Settings::read(get)?;

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
            approval,
            task_max_tokens,
            task_max_time,
            task_skill_manage,
            stream,
            web_search,
            memory,
            subagents,
            github,
            context_files,
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
        assert_eq!(
            config.tools,
            ToolSettings {
                settings_file: Some(SettingsFile::new(
                    "/home/tester/.junees/config.toml",
                    Some(PathBuf::from("/home/tester")),
                )),
                instruction_files: crate::context_files::DEFAULT_NAMES
                    .iter()
                    .map(|name| name.to_string())
                    .collect(),
                sandbox: Some(crate::sandbox::Settings::read(|_| None).unwrap()),
                ..ToolSettings::default()
            }
        );
        let sandbox = self::config(&[
            ("ANTHROPIC_API_KEY", "k"),
            ("JUNEES_SANDBOX", "require"),
            ("JUNEES_SANDBOX_NETWORK", "0"),
        ])
        .unwrap()
        .tools
        .sandbox
        .unwrap();
        assert_eq!(sandbox.mode, Some(crate::sandbox::Mode::Require));
        assert!(!sandbox.network);
        assert!(self::config(&[("ANTHROPIC_API_KEY", "k"), ("JUNEES_SANDBOX", "on")]).is_err());
        let elsewhere =
            self::config(&[("ANTHROPIC_API_KEY", "k"), ("JUNEES_HOME", "/data/j")]).unwrap();
        assert_eq!(
            elsewhere.tools.settings_file,
            Some(SettingsFile::new(
                "/data/j/config.toml",
                Some(PathBuf::from("/home/tester"))
            ))
        );
        assert_eq!(config.approval, ApprovalMode::Risky);
        assert_eq!(config.task_max_tokens, None);
        assert_eq!(config.task_max_time, None);
        assert!(config.stream);
        assert_eq!(config.web_search, None);
        assert!(!config.task_skill_manage);
        assert_eq!(config.memory, Limits::default());
        assert_eq!(
            config.context_files,
            crate::context_files::Setting::with_home(Some(PathBuf::from("/home/tester")))
        );
    }

    #[test]
    fn project_instruction_files_can_be_renamed_or_turned_off() {
        let names = |value: &str| {
            config(&[("ANTHROPIC_API_KEY", "k"), ("JUNEES_CONTEXT_FILES", value)])
                .map(|config| config.context_files.names)
        };
        assert_eq!(
            names("RULES.md, AGENTS.md").unwrap(),
            ["RULES.md", "AGENTS.md"]
        );
        // Read as given: an empty value (`context_files = ""`) is off too.
        assert!(names("").unwrap().is_empty());
        assert!(names("off").unwrap().is_empty());
        // The tools know them, to keep webhook runs from writing them.
        let tools = config(&[
            ("ANTHROPIC_API_KEY", "k"),
            ("JUNEES_CONTEXT_FILES", "RULES.md"),
        ])
        .unwrap()
        .tools;
        assert_eq!(tools.instruction_files, ["RULES.md"]);
        let err = names("docs/AGENTS.md").unwrap_err().to_string();
        assert!(err.contains("JUNEES_CONTEXT_FILES"), "{err}");
    }

    #[test]
    fn task_sessions_may_be_allowed_to_change_skills() {
        let parsed = config(&[
            ("ANTHROPIC_API_KEY", "k"),
            ("JUNEES_TASK_SKILL_MANAGE", "1"),
        ]);
        assert!(parsed.unwrap().task_skill_manage);
        let err = config(&[
            ("ANTHROPIC_API_KEY", "k"),
            ("JUNEES_TASK_SKILL_MANAGE", "maybe"),
        ])
        .err()
        .unwrap();
        assert_eq!(
            err.to_string(),
            "JUNEES_TASK_SKILL_MANAGE must be 1 or 0, got `maybe`"
        );
    }

    #[test]
    fn reads_the_memory_limits() {
        let parsed = config(&[
            ("ANTHROPIC_API_KEY", "k"),
            ("JUNEES_MEMORY_USER_CHARS", " 300 "),
        ])
        .unwrap();
        assert_eq!(parsed.memory.user, 300);
        assert_eq!(parsed.memory.notes, crate::memory::DEFAULT_NOTES_CHARS);
        let err = config(&[
            ("ANTHROPIC_API_KEY", "k"),
            ("JUNEES_MEMORY_PROJECT_CHARS", "9000"),
        ])
        .err()
        .unwrap();
        assert!(
            err.to_string()
                .starts_with("JUNEES_MEMORY_PROJECT_CHARS must be")
        );
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
    fn reads_the_web_search_settings() {
        let on = config(&[
            ("ANTHROPIC_API_KEY", "k"),
            ("JUNEES_WEB_SEARCH", " on "),
            ("JUNEES_WEB_SEARCH_MAX_USES", " 2 "),
        ])
        .unwrap();
        assert_eq!(on.web_search.map(|search| search.max_uses), Some(2));
        let err = config(&[
            ("ANTHROPIC_API_KEY", "k"),
            ("JUNEES_WEB_SEARCH", "on"),
            ("JUNEES_WEB_SEARCH_MAX_USES", "0"),
        ])
        .err()
        .unwrap();
        assert!(
            err.to_string().contains("JUNEES_WEB_SEARCH_MAX_USES"),
            "{err}"
        );
    }

    #[test]
    fn reads_the_approval_mode() {
        for (raw, expected) in [
            ("off", ApprovalMode::Off),
            (" All ", ApprovalMode::All),
            ("risky", ApprovalMode::Risky),
        ] {
            let parsed = config(&[("ANTHROPIC_API_KEY", "k"), ("JUNEES_APPROVAL", raw)]);
            assert_eq!(parsed.unwrap().approval, expected, "{raw}");
        }
        let err = config(&[("ANTHROPIC_API_KEY", "k"), ("JUNEES_APPROVAL", "ask")])
            .err()
            .unwrap();
        assert!(err.to_string().contains("JUNEES_APPROVAL"), "{err}");
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
    fn hides_the_variables_of_mcp_http_headers() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let list = dir.path().join("mcp.json");
        std::fs::write(
            &list,
            r#"{"servers": {"docs": {"url": "https://docs.example/mcp",
                "headers": {"Authorization": "Bearer env:DOCS_TOKEN", "X-Db": "env:DB_PASSWORD"}}}}"#,
        )
        .unwrap();
        std::fs::set_permissions(&list, std::fs::Permissions::from_mode(0o600)).unwrap();
        let home = dir.path().display().to_string();
        let config = config(&[
            ("ANTHROPIC_API_KEY", "k"),
            ("JUNEES_HOME", &home),
            ("JUNEES_HIDDEN_ENV", "DB_PASSWORD"),
        ])
        .unwrap();
        assert_eq!(config.tools.hidden_env, ["DB_PASSWORD", "DOCS_TOKEN"]);
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
