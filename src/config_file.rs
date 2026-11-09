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

/// The file's name inside the state folder.
pub const FILE_NAME: &str = "config.toml";

/// The keys of the file and the environment variable each one stands for.
pub const KEYS: &[(&str, &str)] = &[
    ("backend", "JUNEES_BACKEND"),
    ("api_key", "ANTHROPIC_API_KEY"),
    ("base_url", "ANTHROPIC_BASE_URL"),
    ("model", "JUNEES_MODEL"),
    ("task_max_time", "JUNEES_TASK_MAX_TIME"),
    ("task_max_tokens", "JUNEES_TASK_MAX_TOKENS"),
    ("max_tokens", "JUNEES_MAX_TOKENS"),
    ("max_turns", "JUNEES_MAX_TURNS"),
    ("context_window", "JUNEES_CONTEXT_WINDOW"),
    ("compact_threshold", "JUNEES_COMPACT_THRESHOLD"),
    ("thinking_budget", "JUNEES_THINKING_BUDGET"),
    ("approval", "JUNEES_APPROVAL"),
    ("stream", "JUNEES_STREAM"),
    ("deny_commands", "JUNEES_DENY_COMMANDS"),
    ("allow_dangerous", "JUNEES_ALLOW_DANGEROUS"),
    ("hidden_env", "JUNEES_HIDDEN_ENV"),
    ("notify_webhook", "JUNEES_NOTIFY_WEBHOOK"),
    ("notify_each_session", "JUNEES_NOTIFY_EACH_SESSION"),
    ("claude_bin", "JUNEES_CLAUDE_BIN"),
    ("claude_model", "JUNEES_CLAUDE_MODEL"),
];

const HEADER: &str = "\
# Junees settings, written by `junees setup` (run it again to change them).
# Environment variables win over this file: ANTHROPIC_API_KEY over api_key,
# JUNEES_MODEL over model, and so on (see README).
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
    /// that sets it to something other than blanks.
    pub fn get(&self, name: &str) -> Option<String> {
        let set = |value: &String| !value.trim().is_empty();
        if let Some((_, value)) = self.overrides.iter().find(|(key, _)| key == name) {
            return Some(value.clone());
        }
        (self.env)(name)
            .filter(set)
            .or_else(|| self.file.env_value(name).map(str::to_string))
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
