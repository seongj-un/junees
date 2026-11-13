//! The settings file `$JUNEES_HOME/config.toml` (default
//! `~/.junees/config.toml`), written by `junees setup`, and [`Settings`],
//! which puts the environment in front of it.
//!
//! The file holds a small, flat part of TOML: `key = value` lines with
//! quoted strings, whole numbers, decimals or `true`/`false`, and `#`
//! comments. Every key stands for one environment variable ([`KEYS`]); the
//! variable wins when both are set. Nothing else is read from the file, so
//! it cannot set variables Junees otherwise never reads.

use std::fs;
use std::io::Write;
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};

use crate::provider::openai;

/// The API keys read only from where their address is set: (key,
/// address, the address used when none is set), as environment variable
/// names ([`Settings::get`]).
pub const PAIRED_KEYS: &[(&str, &str, &str)] = &[
    (
        ANTHROPIC_KEY_ENV,
        ANTHROPIC_BASE_URL_ENV,
        crate::config::DEFAULT_BASE_URL,
    ),
    (
        openai::API_KEY_ENV,
        openai::BASE_URL_ENV,
        openai::DEFAULT_BASE_URL,
    ),
    // No default address: without one, transcription is off.
    (
        crate::transcribe::API_KEY_ENV,
        crate::transcribe::BASE_URL_ENV,
        "",
    ),
];

/// Whether `address` is `default` as written down (case of letters and a
/// trailing `/` aside): such an address counts as not set, since every
/// key it could take goes to the same official API.
pub fn is_default_address(address: &str, default: &str) -> bool {
    address
        .trim()
        .trim_end_matches('/')
        .eq_ignore_ascii_case(default)
}

/// The Anthropic key and the address it goes to.
pub const ANTHROPIC_KEY_ENV: &str = "ANTHROPIC_API_KEY";
pub const ANTHROPIC_BASE_URL_ENV: &str = "ANTHROPIC_BASE_URL";

/// The file's name inside the state folder.
pub const FILE_NAME: &str = "config.toml";

/// The keys of the file and the environment variable each one stands for.
pub const KEYS: &[(&str, &str)] = &[
    ("backend", "JUNEES_BACKEND"),
    ("api_key", "ANTHROPIC_API_KEY"),
    ("base_url", "ANTHROPIC_BASE_URL"),
    ("provider", "JUNEES_PROVIDER"),
    ("openai_base_url", "OPENAI_BASE_URL"),
    ("openai_api_key", "OPENAI_API_KEY"),
    ("openai_max_tokens_field", "JUNEES_OPENAI_MAX_TOKENS_FIELD"),
    ("openai_reasoning_effort", "JUNEES_OPENAI_REASONING_EFFORT"),
    ("model", "JUNEES_MODEL"),
    ("prices", "JUNEES_PRICES"),
    ("task_max_time", "JUNEES_TASK_MAX_TIME"),
    ("task_max_tokens", "JUNEES_TASK_MAX_TOKENS"),
    ("task_skill_manage", "JUNEES_TASK_SKILL_MANAGE"),
    ("max_tokens", "JUNEES_MAX_TOKENS"),
    ("max_turns", "JUNEES_MAX_TURNS"),
    ("subagents", "JUNEES_SUBAGENTS"),
    ("subagent_max_turns", "JUNEES_SUBAGENT_MAX_TURNS"),
    ("subagent_max_tokens", "JUNEES_SUBAGENT_MAX_TOKENS"),
    ("context_window", "JUNEES_CONTEXT_WINDOW"),
    ("compact_threshold", "JUNEES_COMPACT_THRESHOLD"),
    ("thinking_budget", "JUNEES_THINKING_BUDGET"),
    ("approval", "JUNEES_APPROVAL"),
    ("stream", "JUNEES_STREAM"),
    ("deny_commands", "JUNEES_DENY_COMMANDS"),
    ("allow_dangerous", "JUNEES_ALLOW_DANGEROUS"),
    ("hidden_env", "JUNEES_HIDDEN_ENV"),
    ("sandbox", "JUNEES_SANDBOX"),
    ("sandbox_write", "JUNEES_SANDBOX_WRITE"),
    ("sandbox_read", "JUNEES_SANDBOX_READ"),
    ("webhook_sandbox_read", "JUNEES_WEBHOOK_SANDBOX_READ"),
    ("sandbox_network", "JUNEES_SANDBOX_NETWORK"),
    ("notify_webhook", "JUNEES_NOTIFY_WEBHOOK"),
    ("notify_each_session", "JUNEES_NOTIFY_EACH_SESSION"),
    ("claude_bin", "JUNEES_CLAUDE_BIN"),
    ("claude_model", "JUNEES_CLAUDE_MODEL"),
    ("claude_code_unattended", "JUNEES_CLAUDE_CODE_UNATTENDED"),
    ("web_search", "JUNEES_WEB_SEARCH"),
    ("web_search_max_uses", "JUNEES_WEB_SEARCH_MAX_USES"),
    (
        "web_search_allowed_domains",
        "JUNEES_WEB_SEARCH_ALLOWED_DOMAINS",
    ),
    (
        "web_search_blocked_domains",
        "JUNEES_WEB_SEARCH_BLOCKED_DOMAINS",
    ),
    ("memory_user_chars", "JUNEES_MEMORY_USER_CHARS"),
    ("memory_notes_chars", "JUNEES_MEMORY_NOTES_CHARS"),
    ("memory_project_chars", "JUNEES_MEMORY_PROJECT_CHARS"),
    ("context_files", "JUNEES_CONTEXT_FILES"),
    ("discord_token", "JUNEES_DISCORD_TOKEN"),
    ("discord_allowed_users", "JUNEES_DISCORD_ALLOWED_USERS"),
    ("discord_channels", "JUNEES_DISCORD_CHANNELS"),
    ("discord_parallel", "JUNEES_DISCORD_PARALLEL"),
    ("telegram_token", "JUNEES_TELEGRAM_TOKEN"),
    ("telegram_allowed_users", "JUNEES_TELEGRAM_ALLOWED_USERS"),
    ("telegram_parallel", "JUNEES_TELEGRAM_PARALLEL"),
    (
        "telegram_approval_timeout",
        "JUNEES_TELEGRAM_APPROVAL_TIMEOUT",
    ),
    ("slack_bot_token", "JUNEES_SLACK_BOT_TOKEN"),
    ("slack_app_token", "JUNEES_SLACK_APP_TOKEN"),
    ("slack_allowed_users", "JUNEES_SLACK_ALLOWED_USERS"),
    ("slack_parallel", "JUNEES_SLACK_PARALLEL"),
    ("slack_approval_timeout", "JUNEES_SLACK_APPROVAL_TIMEOUT"),
    ("telegram_format", "JUNEES_TELEGRAM_FORMAT"),
    ("timezone", "JUNEES_TIMEZONE"),
    ("cron_max_time", "JUNEES_CRON_MAX_TIME"),
    ("cron_max_tokens", "JUNEES_CRON_MAX_TOKENS"),
    ("webhook_max_time", "JUNEES_WEBHOOK_MAX_TIME"),
    ("webhook_max_tokens", "JUNEES_WEBHOOK_MAX_TOKENS"),
    (
        "discord_approval_timeout",
        "JUNEES_DISCORD_APPROVAL_TIMEOUT",
    ),
    ("github_token", "JUNEES_GITHUB_TOKEN"),
    ("github_api_url", "JUNEES_GITHUB_API_URL"),
    ("github_repos", "JUNEES_GITHUB_REPOS"),
    ("github_allow_unattended", "JUNEES_GITHUB_ALLOW_UNATTENDED"),
    ("github_flow_merge", "JUNEES_GITHUB_FLOW_MERGE"),
    ("browser", "JUNEES_BROWSER"),
    ("browser_path", "JUNEES_BROWSER_PATH"),
    ("browser_allow_private", "JUNEES_BROWSER_ALLOW_PRIVATE"),
    ("browser_unattended", "JUNEES_BROWSER_UNATTENDED"),
    ("browser_no_sandbox", "JUNEES_BROWSER_NO_SANDBOX"),
    ("transcribe_base_url", "JUNEES_TRANSCRIBE_BASE_URL"),
    ("transcribe_api_key", "JUNEES_TRANSCRIBE_API_KEY"),
    ("transcribe_model", "JUNEES_TRANSCRIBE_MODEL"),
    ("transcribe_language", "JUNEES_TRANSCRIBE_LANGUAGE"),
    ("transcribe_max_bytes", "JUNEES_TRANSCRIBE_MAX_BYTES"),
    ("transcribe_timeout", "JUNEES_TRANSCRIBE_TIMEOUT"),
];

const HEADER: &str = "\
# Junees settings, written by `junees setup` (run it again to change them).
# Environment variables win over this file: JUNEES_MODEL over model, and so
# on. An API key comes from where its address is set: api_key with base_url,
# ANTHROPIC_API_KEY with ANTHROPIC_BASE_URL (see docs/configuration.md).
";

/// The state folder: `JUNEES_HOME`, else `$HOME/.junees`; `None` when
/// neither is set.
pub fn home_dir(lookup: impl Fn(&str) -> Option<String>) -> Option<PathBuf> {
    let get = |key: &str| lookup(key).filter(|value| !value.trim().is_empty());
    get("JUNEES_HOME")
        .map(|home| PathBuf::from(home.trim()))
        .or_else(|| get("HOME").map(|home| PathBuf::from(home.trim()).join(".junees")))
}

/// Where the settings file is, if there is a state folder.
pub fn path(lookup: impl Fn(&str) -> Option<String>) -> Option<PathBuf> {
    home_dir(lookup).map(|home| home.join(FILE_NAME))
}

/// The contents of a settings file, in [`KEYS`] order. Deliberately not
/// `Debug`: it may hold the API key.
#[derive(Clone, Default, PartialEq)]
pub struct ConfigFile {
    /// (key, value) for the keys that are set.
    values: Vec<(&'static str, String)>,
}

impl ConfigFile {
    /// Reads the file at `path`; a missing file is an empty one.
    pub fn load(path: &Path) -> Result<Self> {
        let text = match fs::read_to_string(path) {
            Ok(text) => text,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(Self::default()),
            Err(err) => {
                return Err(err).with_context(|| format!("cannot read {}", path.display()));
            }
        };
        Self::parse(&text).with_context(|| {
            format!(
                "{} is not a valid settings file (fix it, delete it, or run `junees setup`)",
                path.display()
            )
        })
    }

    pub fn parse(text: &str) -> Result<Self> {
        let mut file = Self::default();
        for (index, line) in text.lines().enumerate() {
            let number = index + 1;
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            if line.starts_with('[') {
                bail!("line {number}: sections are not supported; put every setting at the top");
            }
            let Some((key, rest)) = line.split_once('=') else {
                bail!("line {number}: expected `key = value`");
            };
            let key = key.trim();
            let Some(&(known, _)) = KEYS.iter().find(|(name, _)| *name == key) else {
                // Only a name-like key is shown: the text before `=` may
                // be a pasted secret.
                let shown = if key.len() <= 32
                    && key.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_')
                {
                    format!(" `{key}`")
                } else {
                    String::new()
                };
                bail!(
                    "line {number}: unknown setting{shown} (known: {})",
                    KEYS.iter()
                        .map(|(name, _)| *name)
                        .collect::<Vec<_>>()
                        .join(", ")
                );
            };
            if file.get(known).is_some() {
                bail!("line {number}: `{key}` is set twice");
            }
            let value = parse_value(rest.trim()).map_err(|err| {
                anyhow::anyhow!("line {number}: {err} (for example: {key} = \"value\")")
            })?;
            file.values.push((known, value));
        }
        file.sort();
        Ok(file)
    }

    /// The value of a file key.
    pub fn get(&self, key: &str) -> Option<&str> {
        self.values
            .iter()
            .find(|(name, _)| *name == key)
            .map(|(_, value)| value.as_str())
    }

    /// The value the file gives the environment variable `name`.
    pub fn env_value(&self, name: &str) -> Option<&str> {
        let (key, _) = KEYS.iter().find(|(_, var)| *var == name)?;
        self.get(key)
    }

    /// Sets a file key. Panics on a key that is not in [`KEYS`]: that is a
    /// bug in the caller, not an input error.
    pub fn set(&mut self, key: &str, value: impl Into<String>) {
        let (known, _) = KEYS
            .iter()
            .find(|(name, _)| *name == key)
            .unwrap_or_else(|| panic!("`{key}` is not a settings key"));
        self.remove(key);
        self.values.push((known, value.into()));
        self.sort();
    }

    pub fn remove(&mut self, key: &str) {
        self.values.retain(|(name, _)| *name != key);
    }

    fn sort(&mut self) {
        let rank = |key: &str| KEYS.iter().position(|(name, _)| *name == key);
        self.values.sort_by_key(|(key, _)| rank(key));
    }

    /// The file's text.
    pub fn render(&self) -> String {
        let mut text = HEADER.to_string();
        for (key, value) in &self.values {
            let shown = if !value.is_empty() && value.bytes().all(|b| b.is_ascii_digit()) {
                value.clone()
            } else {
                quote(value)
            };
            text.push_str(&format!("{key} = {shown}\n"));
        }
        text
    }

    /// Writes the file: the folder is created `0700` if missing, the file
    /// is `0600` (it may hold the API key), and it is replaced through a
    /// temporary file and a rename, so a crash leaves the old or the new
    /// version.
    pub fn save(&self, path: &Path) -> Result<()> {
        let dir = path.parent().context("the settings file has no folder")?;
        fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(dir)
            .with_context(|| format!("cannot create {}", dir.display()))?;
        let tmp = dir.join(format!(".{FILE_NAME}.tmp"));
        let write = || -> std::io::Result<()> {
            match fs::remove_file(&tmp) {
                Err(err) if err.kind() != std::io::ErrorKind::NotFound => return Err(err),
                _ => {}
            }
            let mut file = fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .mode(0o600)
                .open(&tmp)?;
            file.set_permissions(fs::Permissions::from_mode(0o600))?;
            file.write_all(self.render().as_bytes())?;
            file.sync_all()?;
            fs::rename(&tmp, path)
        };
        write().with_context(|| format!("cannot write {}", path.display()))
    }
}

/// One value: a basic (`"..."`) or literal (`'...'`) string, or a bare
/// number or boolean, optionally followed by a comment.
fn parse_value(text: &str) -> Result<String> {
    let (value, rest) = if let Some(body) = text.strip_prefix('"') {
        basic_string(body)?
    } else if let Some(body) = text.strip_prefix('\'') {
        let end = body.find('\'').context("the string has no closing '")?;
        (body[..end].to_string(), &body[end + 1..])
    } else {
        let end = text.find(['#', ' ', '\t']).unwrap_or(text.len());
        let bare = &text[..end];
        let valid = bare == "true"
            || bare == "false"
            || bare.parse::<i64>().is_ok()
            || (bare.parse::<f64>().is_ok() && bare.bytes().any(|b| b.is_ascii_digit()));
        if bare.is_empty() {
            bail!("the value is missing");
        }
        if !valid {
            bail!("the value needs quotes");
        }
        (bare.to_string(), &text[end..])
    };
    let rest = rest.trim();
    if !rest.is_empty() && !rest.starts_with('#') {
        bail!("unexpected text after the value");
    }
    Ok(value)
}

/// The rest of a `"` string: its value and what follows the closing quote.
fn basic_string(body: &str) -> Result<(String, &str)> {
    let mut value = String::new();
    let mut chars = body.char_indices();
    while let Some((at, c)) = chars.next() {
        match c {
            '"' => return Ok((value, &body[at + 1..])),
            '\\' => {
                let (_, escape) = chars.next().context("the string has no closing \"")?;
                match escape {
                    '"' => value.push('"'),
                    '\\' => value.push('\\'),
                    'n' => value.push('\n'),
                    't' => value.push('\t'),
                    'r' => value.push('\r'),
                    'u' | 'U' => {
                        let digits = if escape == 'u' { 4 } else { 8 };
                        let hex: String = chars.by_ref().take(digits).map(|(_, c)| c).collect();
                        let code = u32::from_str_radix(&hex, 16)
                            .ok()
                            .filter(|_| hex.len() == digits)
                            .and_then(char::from_u32)
                            .with_context(|| format!("bad \\{escape} escape"))?;
                        value.push(code);
                    }
                    _ => bail!("unknown escape"),
                }
            }
            c => value.push(c),
        }
    }
    bail!("the string has no closing \"")
}

/// `value` as a TOML basic string.
fn quote(value: &str) -> String {
    let mut quoted = String::from("\"");
    for c in value.chars() {
        match c {
            '"' => quoted.push_str("\\\""),
            '\\' => quoted.push_str("\\\\"),
            '\n' => quoted.push_str("\\n"),
            '\t' => quoted.push_str("\\t"),
            '\r' => quoted.push_str("\\r"),
            c if c.is_control() => quoted.push_str(&format!("\\u{:04X}", c as u32)),
            c => quoted.push(c),
        }
    }
    quoted.push('"');
    quoted
}

type Lookup = Box<dyn Fn(&str) -> Option<String> + Send + Sync>;

/// Where a setting's value comes from.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Origin {
    /// Changed for this process only (`/model`, `/backend`).
    Override,
    Env,
    File,
}

/// Where settings come from, first match wins: changes made for this
/// process only (`/model`, `/backend`), the environment, then the file.
/// Not `Debug`, like the file.
pub struct Settings {
    overrides: Vec<(String, String)>,
    env: Lookup,
    file: ConfigFile,
    /// Where the file is (or would be), if there is a state folder.
    path: Option<PathBuf>,
    /// Whether the file was there.
    exists: bool,
}

impl Settings {
    /// The process environment and the file it points at.
    pub fn from_env() -> Result<Self> {
        Self::load(|key| std::env::var(key).ok())
    }

    /// `env` and the file it points at ([`path`]).
    pub fn load(env: impl Fn(&str) -> Option<String> + Send + Sync + 'static) -> Result<Self> {
        let path = path(&env);
        let (file, exists) = match &path {
            Some(path) if path.exists() => (ConfigFile::load(path)?, true),
            _ => (ConfigFile::default(), false),
        };
        Ok(Self::new(env, file, path, exists))
    }

    /// Like [`Settings::load`] for `junees setup`, which must start even
    /// when the file is broken: a file that cannot be read counts as an
    /// empty one that exists, and its error comes back alongside.
    pub fn load_for_setup(
        env: impl Fn(&str) -> Option<String> + Send + Sync + 'static,
    ) -> (Self, Option<anyhow::Error>) {
        let path = path(&env);
        let (file, exists, broken) = match &path {
            Some(path) if path.exists() => match ConfigFile::load(path) {
                Ok(file) => (file, true, None),
                Err(err) => (ConfigFile::default(), true, Some(err)),
            },
            _ => (ConfigFile::default(), false, None),
        };
        (Self::new(env, file, path, exists), broken)
    }

    pub fn new(
        env: impl Fn(&str) -> Option<String> + Send + Sync + 'static,
        file: ConfigFile,
        path: Option<PathBuf>,
        exists: bool,
    ) -> Self {
        Self {
            overrides: Vec::new(),
            env: Box::new(env),
            file,
            path,
            exists,
        }
    }

    /// The value of the environment variable `name`, from the first source
    /// that sets it to something other than blanks; an API key
    /// ([`PAIRED_KEYS`]) only from the place its address comes from
    /// ([`Settings::paired_key`]).
    pub fn get(&self, name: &str) -> Option<String> {
        self.lookup(name).map(|(value, _)| value)
    }

    /// Where [`Settings::get`]'s value of `name` comes from.
    pub fn origin(&self, name: &str) -> Option<Origin> {
        self.lookup(name).map(|(_, origin)| origin)
    }

    fn lookup(&self, name: &str) -> Option<(String, Origin)> {
        match PAIRED_KEYS.iter().find(|(key, _, _)| *key == name) {
            Some((key, address, default)) => self.paired_key(key, address, default),
            None => self.first(name),
        }
    }

    /// The first source's value of `name`, by the plain order.
    fn first(&self, name: &str) -> Option<(String, Origin)> {
        self.overridden(name)
            .or_else(|| self.env(name).map(|value| (value, Origin::Env)))
            .or_else(|| self.in_file(name))
    }

    fn overridden(&self, name: &str) -> Option<(String, Origin)> {
        let (_, value) = self.overrides.iter().find(|(key, _)| key == name)?;
        Some((value.clone(), Origin::Override))
    }

    fn in_file(&self, name: &str) -> Option<(String, Origin)> {
        let value = self.file.env_value(name)?;
        Some((value.to_string(), Origin::File))
    }

    /// An API key goes with its address: from the file when the address
    /// comes from the file, from the environment when the address
    /// variable does. Taken key by key, a key exported for other tools
    /// would go to the file's server (OpenRouter, a proxy), or the file's
    /// key to an exported address. With the default address, unset or
    /// written down ([`is_default_address`]), either one is taken, the
    /// environment first.
    fn paired_key(&self, key: &str, address: &str, default: &str) -> Option<(String, Origin)> {
        let set = self
            .first(address)
            .filter(|(value, _)| !is_default_address(value, default));
        match set.map(|(_, origin)| origin) {
            None | Some(Origin::Override) => self.first(key),
            Some(Origin::Env) => self
                .overridden(key)
                .or_else(|| self.env(key).map(|value| (value, Origin::Env))),
            Some(Origin::File) => self.overridden(key).or_else(|| self.in_file(key)),
        }
    }

    /// Whether the key `name` is set in the environment or the file but
    /// [`Settings::get`] leaves it out, because its address is set in the
    /// other place.
    pub fn key_away_from_address(&self, name: &str) -> bool {
        self.get(name).is_none() && (self.env(name).is_some() || self.in_file(name).is_some())
    }

    /// Only the environment's value of `name`.
    pub fn env(&self, name: &str) -> Option<String> {
        (self.env)(name).filter(|value| !value.trim().is_empty())
    }

    /// Sets `name` for this process only; the file is not changed.
    pub fn set_override(&mut self, name: &str, value: impl Into<String>) {
        self.overrides.retain(|(key, _)| key != name);
        self.overrides.push((name.to_string(), value.into()));
    }

    pub fn file(&self) -> &ConfigFile {
        &self.file
    }

    pub fn path(&self) -> Option<&Path> {
        self.path.as_deref()
    }

    pub fn file_exists(&self) -> bool {
        self.exists
    }

    /// Takes a freshly saved file.
    pub fn replace_file(&mut self, file: ConfigFile) {
        self.file = file;
        self.exists = true;
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use super::*;
    use crate::config::Config;

    fn env(pairs: &[(&str, &str)]) -> impl Fn(&str) -> Option<String> + Send + Sync + 'static {
        let map: HashMap<String, String> = pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();
        move |key| map.get(key).cloned()
    }

    #[test]
    fn parses_the_flat_toml_subset() {
        let file = ConfigFile::parse(
            "# a comment\n\
             \n\
             backend = \"claude-code\"   # trailing comment\n\
             api_key = 'sk-ant-literal'\n\
             max_turns = 7\n\
             compact_threshold = 0.5\n\
             stream = false\n\
             model = \"m\\u00e9 \\\"x\\\"\"\n",
        )
        .unwrap();
        assert_eq!(file.get("backend"), Some("claude-code"));
        assert_eq!(file.get("api_key"), Some("sk-ant-literal"));
        assert_eq!(file.env_value("JUNEES_MAX_TURNS"), Some("7"));
        assert_eq!(file.env_value("JUNEES_COMPACT_THRESHOLD"), Some("0.5"));
        assert_eq!(file.env_value("JUNEES_STREAM"), Some("false"));
        assert_eq!(file.get("model"), Some("mé \"x\""));
        assert_eq!(file.env_value("ANTHROPIC_TOKEN"), None);
        assert_eq!(file.env_value("HOME"), None);
    }

    #[test]
    fn rejects_bad_files_with_the_line() {
        for (text, expected) in [
            ("api_key = sk-ant-x\n", "line 1: the value needs quotes"),
            ("\nmodel\n", "line 2: expected `key = value`"),
            ("token = \"x\"\n", "line 1: unknown setting `token`"),
            ("[junees]\n", "line 1: sections are not supported"),
            (
                "model = \"a\"\nmodel = \"b\"\n",
                "line 2: `model` is set twice",
            ),
            ("model = \"open\n", "line 1: the string has no closing \""),
            (
                "model = \"a\" b\n",
                "line 1: unexpected text after the value",
            ),
            ("task_max_time = 2h\n", "line 1: the value needs quotes"),
            ("model =\n", "line 1: the value is missing"),
            ("model = \"\\q\"\n", "line 1: unknown escape"),
        ] {
            let err = ConfigFile::parse(text)
                .err()
                .unwrap_or_else(|| panic!("{text}"));
            assert!(err.to_string().starts_with(expected), "{text}: {err}");
        }
    }

    #[test]
    fn errors_never_show_the_value() {
        let secret = "sk-ant-api03-SECRETvalue";
        for text in [
            format!("api_key = {secret}\n"),
            format!("api_key = \"x\" {secret}\n"),
            format!("api_key = '{secret}' {secret}\n"),
            format!("api_key = \"{secret}\\q\"\n"),
            format!("api_key = \"{secret}\\uZZZZ\"\n"),
            format!("{secret}x = \"x\"\n"),
        ] {
            let err = format!("{:#}", ConfigFile::parse(&text).err().unwrap());
            assert!(!err.contains("SECRET"), "{text}: {err}");
            assert!(err.starts_with("line 1: "), "{err}");
        }
    }

    #[test]
    fn a_bad_file_names_its_path() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(FILE_NAME);
        fs::write(&path, "model = claude\n").unwrap();
        let err = format!("{:#}", ConfigFile::load(&path).err().unwrap());
        assert!(
            err.starts_with(&format!("{} is not a valid settings file", path.display())),
            "{err}"
        );
        assert!(err.ends_with("line 1: the value needs quotes (for example: model = \"value\")"));
        // So does Settings, which main uses.
        let home = dir.path().to_string_lossy().into_owned();
        let err = Settings::load(env(&[("JUNEES_HOME", &home)]))
            .err()
            .unwrap();
        assert!(format!("{err:#}").contains("line 1"), "{err:#}");
    }

    #[test]
    fn writes_a_private_file_that_reads_back() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state/config.toml");
        let mut file = ConfigFile::default();
        file.set("task_max_time", "2h");
        file.set("api_key", "sk-ant-\"quoted\"\\");
        file.set("backend", "api");
        file.set("task_max_tokens", "2000000");
        file.save(&path).unwrap();

        let mode = |path: &Path| fs::metadata(path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode(&path), 0o600);
        assert_eq!(mode(path.parent().unwrap()), 0o700);
        let text = fs::read_to_string(&path).unwrap();
        assert!(text.starts_with("# Junees settings"), "{text}");
        assert!(
            text.ends_with(
                "backend = \"api\"\napi_key = \"sk-ant-\\\"quoted\\\"\\\\\"\n\
                 task_max_time = \"2h\"\ntask_max_tokens = 2000000\n"
            ),
            "{text}"
        );
        assert!(ConfigFile::load(&path).unwrap() == file);

        // Saving again replaces it and leaves no temporary file.
        file.remove("task_max_tokens");
        file.save(&path).unwrap();
        assert!(ConfigFile::load(&path).unwrap() == file);
        assert_eq!(fs::read_dir(path.parent().unwrap()).unwrap().count(), 1);
        assert_eq!(mode(&path), 0o600);
    }

    #[test]
    fn a_missing_file_is_empty() {
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path().to_string_lossy().into_owned();
        let settings = Settings::load(env(&[("JUNEES_HOME", &home)])).unwrap();
        assert!(!settings.file_exists());
        assert_eq!(settings.path(), Some(dir.path().join(FILE_NAME).as_path()));
        assert!(settings.file() == &ConfigFile::default());
    }

    #[test]
    fn finds_the_file_like_the_state_folder() {
        assert_eq!(
            path(env(&[("HOME", "/home/u")])),
            Some(PathBuf::from("/home/u/.junees/config.toml"))
        );
        assert_eq!(
            path(env(&[("HOME", "/home/u"), ("JUNEES_HOME", "/data/j")])),
            Some(PathBuf::from("/data/j/config.toml"))
        );
        assert_eq!(path(env(&[("HOME", " ")])), None);
    }

    #[test]
    fn the_environment_wins_over_the_file() {
        let mut file = ConfigFile::default();
        file.set("api_key", "sk-ant-file");
        file.set("model", "file-model");
        file.set("task_max_time", "2h");
        let settings = Settings::new(
            env(&[
                ("JUNEES_MODEL", "env-model"),
                ("ANTHROPIC_API_KEY", " "),
                ("HOME", "/home/u"),
            ]),
            file,
            None,
            true,
        );
        let config = Config::from_lookup(|key| settings.get(key)).unwrap();
        assert_eq!(config.model, "env-model");
        // A blank variable does not hide the file.
        assert_eq!(config.api_key, "sk-ant-file");
        assert_eq!(
            config.task_max_time,
            Some(std::time::Duration::from_secs(7200))
        );
        assert_eq!(settings.env("ANTHROPIC_API_KEY"), None);
    }

    #[test]
    fn the_openai_settings_follow_the_same_order() {
        use crate::provider::openai::LimitField;
        let mut file = ConfigFile::default();
        file.set("provider", "openai");
        file.set("model", "file-model");
        file.set("openai_base_url", "https://openrouter.ai/api/v1");
        file.set("openai_api_key", "file-key");
        file.set("openai_reasoning_effort", "low");
        file.set("openai_max_tokens_field", "max_tokens");
        let settings = Settings::new(
            env(&[
                ("OPENAI_BASE_URL", " "),
                ("OPENAI_API_KEY", " "),
                ("JUNEES_OPENAI_REASONING_EFFORT", "high"),
                ("HOME", "/home/u"),
            ]),
            file,
            None,
            true,
        );
        let openai = Config::from_lookup(|key| settings.get(key))
            .unwrap()
            .openai
            .unwrap();
        // Each variable wins over its key, one by one.
        assert_eq!(openai.reasoning_effort.as_deref(), Some("high"));
        // A blank variable does not hide the file.
        assert_eq!(openai.base_url, "https://openrouter.ai/api/v1");
        assert_eq!(openai.api_key.as_deref(), Some("file-key"));
        assert_eq!(openai.limit_field, LimitField::MaxTokens);
        assert!(!openai.uses_max_completion_tokens());
    }

    #[test]
    fn the_openai_key_comes_from_where_the_address_does() {
        const KEY: &str = "OPENAI_API_KEY";
        let settings = |env_pairs: &[(&str, &str)], file_pairs: &[(&str, &str)]| {
            let mut file = ConfigFile::default();
            for (key, value) in file_pairs {
                file.set(key, *value);
            }
            Settings::new(env(env_pairs), file, None, true)
        };
        let file_server = ("openai_base_url", "https://openrouter.ai/api/v1");
        let env_server = ("OPENAI_BASE_URL", "https://proxy.example/v1");

        // A key exported for other tools does not go to the file's server.
        let both = settings(
            &[(KEY, "env-key")],
            &[file_server, ("openai_api_key", "file-key")],
        );
        assert_eq!(both.get(KEY).as_deref(), Some("file-key"));
        assert_eq!(both.origin(KEY), Some(Origin::File));
        let file_address_only = settings(&[(KEY, "env-key")], &[file_server]);
        assert_eq!(file_address_only.get(KEY), None);
        assert_eq!(file_address_only.env(KEY).as_deref(), Some("env-key"));

        // Nor the file's key to an exported address.
        let env_address = settings(&[env_server], &[("openai_api_key", "file-key")]);
        assert_eq!(env_address.get(KEY), None);
        let env_pair = settings(
            &[env_server, (KEY, "env-key")],
            &[file_server, ("openai_api_key", "file-key")],
        );
        assert_eq!(env_pair.get(KEY).as_deref(), Some("env-key"));
        assert_eq!(env_pair.origin(KEY), Some(Origin::Env));

        // The default address takes either, the environment first.
        let default = settings(&[(KEY, "env-key")], &[("openai_api_key", "file-key")]);
        assert_eq!(default.get(KEY).as_deref(), Some("env-key"));
        let default = settings(&[], &[("openai_api_key", "file-key")]);
        assert_eq!(default.get(KEY).as_deref(), Some("file-key"));
        // So does the default address written down.
        let written = settings(
            &[("OPENAI_BASE_URL", "https://api.openai.com/v1/")],
            &[("openai_api_key", "file-key")],
        );
        assert_eq!(written.get(KEY).as_deref(), Some("file-key"));
        let written = settings(
            &[(KEY, "env-key")],
            &[("openai_base_url", "https://api.openai.com/v1")],
        );
        assert_eq!(written.get(KEY).as_deref(), Some("env-key"));
        assert!(!is_default_address(
            "https://api.openai.com",
            openai::DEFAULT_BASE_URL
        ));

        // Starting refuses a remote address without its own key.
        let mut file = ConfigFile::default();
        file.set("provider", "openai");
        file.set("model", "m");
        file.set("openai_base_url", "https://openrouter.ai/api/v1");
        let settings = Settings::new(env(&[(KEY, "env-key")]), file, None, true);
        let err = Config::from_lookup(|key| settings.get(key))
            .err()
            .unwrap()
            .to_string();
        assert!(err.contains("same place"), "{err}");
    }

    #[test]
    fn the_anthropic_key_comes_from_where_the_address_does() {
        const KEY: &str = "ANTHROPIC_API_KEY";
        let settings = |env_pairs: &[(&str, &str)], file_pairs: &[(&str, &str)]| {
            let mut file = ConfigFile::default();
            for (key, value) in file_pairs {
                file.set(key, *value);
            }
            let mut env_pairs = env_pairs.to_vec();
            env_pairs.push(("HOME", "/home/u"));
            Settings::new(env(&env_pairs), file, None, true)
        };
        let file_server = ("base_url", "https://proxy.example");
        let env_server = ("ANTHROPIC_BASE_URL", "https://gateway.example");

        // A key exported for other tools does not go to the file's server.
        let both = settings(
            &[(KEY, "sk-ant-env")],
            &[file_server, ("api_key", "sk-ant-file")],
        );
        assert_eq!(both.get(KEY).as_deref(), Some("sk-ant-file"));
        assert_eq!(both.origin(KEY), Some(Origin::File));
        let file_address_only = settings(&[(KEY, "sk-ant-env")], &[file_server]);
        assert_eq!(file_address_only.get(KEY), None);
        assert!(file_address_only.key_away_from_address(KEY));

        // Nor the file's key to an exported address.
        let env_address = settings(&[env_server], &[("api_key", "sk-ant-file")]);
        assert_eq!(env_address.get(KEY), None);
        assert!(env_address.key_away_from_address(KEY));
        let env_pair = settings(
            &[env_server, (KEY, "sk-ant-env")],
            &[file_server, ("api_key", "sk-ant-file")],
        );
        assert_eq!(env_pair.get(KEY).as_deref(), Some("sk-ant-env"));
        assert_eq!(env_pair.origin(KEY), Some(Origin::Env));
        assert!(!env_pair.key_away_from_address(KEY));

        // The default address takes either, the environment first.
        let default = settings(&[(KEY, "sk-ant-env")], &[("api_key", "sk-ant-file")]);
        assert_eq!(default.get(KEY).as_deref(), Some("sk-ant-env"));
        assert_eq!(default.origin(KEY), Some(Origin::Env));
        let default = settings(&[], &[("api_key", "sk-ant-file")]);
        assert_eq!(default.get(KEY).as_deref(), Some("sk-ant-file"));
        assert_eq!(default.origin(KEY), Some(Origin::File));
        assert!(!settings(&[], &[]).key_away_from_address(KEY));

        // Starting on the api backend refuses an address without its own
        // key and says where the key goes.
        let err = Config::from_lookup(|key| file_address_only.get(key))
            .err()
            .unwrap()
            .to_string();
        assert!(
            err.starts_with("no API key for https://proxy.example:"),
            "{err}"
        );
        assert!(err.contains("same place"), "{err}");
        let config = Config::from_lookup(|key| both.get(key)).unwrap();
        assert_eq!(config.base_url, "https://proxy.example");
        assert_eq!(config.api_key, "sk-ant-file");
        // A password in the address is not shown.
        let secret_address = settings(
            &[("ANTHROPIC_BASE_URL", "https://u:hunter2@proxy.example")],
            &[],
        );
        let err = Config::from_lookup(|key| secret_address.get(key))
            .err()
            .unwrap()
            .to_string();
        assert!(
            err.starts_with("no API key for the address in base_url:"),
            "{err}"
        );
        assert!(!err.contains("hunter2"), "{err}");
        // The default address written down counts as not set: the key may
        // come from either place, since both go to the official API.
        for (env_pairs, file_pairs, expected) in [
            (
                &[("ANTHROPIC_BASE_URL", "https://api.anthropic.com")][..],
                &[("api_key", "sk-ant-file")][..],
                "sk-ant-file",
            ),
            (
                &[(KEY, "sk-ant-env")][..],
                &[("base_url", "https://API.anthropic.com/ ")][..],
                "sk-ant-env",
            ),
        ] {
            let settings = settings(env_pairs, file_pairs);
            assert_eq!(settings.get(KEY).as_deref(), Some(expected));
            assert!(!settings.key_away_from_address(KEY));
            let config = Config::from_lookup(|key| settings.get(key)).unwrap();
            assert_eq!(config.api_key, expected);
        }
        // Without any key that is the plain error, not "same place".
        let none = settings(&[("ANTHROPIC_BASE_URL", "https://api.anthropic.com/")], &[]);
        let err = Config::from_lookup(|key| none.get(key))
            .err()
            .unwrap()
            .to_string();
        assert!(err.starts_with("no API key: run `junees setup`"), "{err}");
        // A path after it is another address.
        let other = settings(
            &[(KEY, "sk-ant-env")],
            &[("base_url", "https://api.anthropic.com/proxy")],
        );
        assert_eq!(other.get(KEY), None);

        // The claude-code backend reads no key, so the pairing changes
        // nothing there.
        let config = Config::from_lookup_without_key(|key| env_address.get(key)).unwrap();
        assert_eq!(config.api_key, "");
        assert_eq!(config.base_url, "https://gateway.example");
    }

    #[test]
    fn the_file_can_turn_on_web_search() {
        let file = ConfigFile::parse(
            "api_key = \"k\"\n\
             web_search = true\n\
             web_search_max_uses = 3\n\
             web_search_blocked_domains = \"spam.example, ads.example\"\n",
        )
        .unwrap();
        let settings = Settings::new(
            env(&[("JUNEES_WEB_SEARCH_MAX_USES", "2"), ("HOME", "/home/u")]),
            file,
            None,
            true,
        );
        let config = Config::from_lookup(|key| settings.get(key)).unwrap();
        let search = config.web_search.unwrap();
        assert_eq!(search.max_uses, 2);
        assert_eq!(search.blocked_domains, ["spam.example", "ads.example"]);
    }

    #[test]
    fn overrides_win_over_everything_and_stay_in_memory() {
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path().to_string_lossy().into_owned();
        let mut file = ConfigFile::default();
        file.set("backend", "api");
        file.save(&dir.path().join(FILE_NAME)).unwrap();
        let mut settings =
            Settings::load(env(&[("JUNEES_HOME", &home), ("JUNEES_BACKEND", "api")])).unwrap();
        assert!(settings.file_exists());
        settings.set_override("JUNEES_BACKEND", "claude-code");
        assert_eq!(
            settings.get("JUNEES_BACKEND").as_deref(),
            Some("claude-code")
        );
        assert!(ConfigFile::load(&dir.path().join(FILE_NAME)).unwrap() == file);
    }

    #[test]
    #[should_panic(expected = "not a settings key")]
    fn set_refuses_unknown_keys() {
        ConfigFile::default().set("ANTHROPIC_TOKEN", "x");
    }
}
