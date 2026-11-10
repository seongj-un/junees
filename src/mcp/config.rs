//! `$JUNEES_HOME/mcp.json`: the MCP servers Junees starts.
//!
//! ```json
//! { "servers": { "github": { "command": "github-mcp", "args": ["stdio"],
//!   "env": {"GITHUB_TOKEN": "..."}, "enabled": true, "trust": false,
//!   "timeout_secs": 120 } } }
//! ```
//!
//! JSON rather than a table in `config.toml`: the settings file parser
//! reads flat keys only. The file starts programs and may hold tokens, so
//! it must belong to the user and be private (mode 0600), and the model's
//! tools may not read or change it (see [`crate::tools::SettingsFile`]).

use std::collections::BTreeMap;
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::Path;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use serde::Deserialize;

/// The file's name inside the state folder, next to `config.toml`.
pub const FILE_NAME: &str = "mcp.json";
/// How long a tool call may take unless the server sets `timeout_secs`.
pub const DEFAULT_CALL_TIMEOUT: Duration = Duration::from_secs(120);
/// The largest `timeout_secs`.
pub const MAX_CALL_TIMEOUT_SECS: u64 = 3600;
/// The longest server name; the rest of the 64 characters of a tool name
/// (`mcp__<server>__<tool>`) is left for the tool.
pub const MAX_NAME_CHARS: usize = 32;
/// A larger file is refused unread.
const MAX_FILE_BYTES: u64 = 1024 * 1024;

/// One server of the file. Deliberately not `Debug`: `env` may hold
/// tokens, and nothing should print them.
#[derive(Clone, PartialEq, Eq)]
pub struct ServerConfig {
    /// The key in `servers`: letters, digits, `_` and `-`.
    pub name: String,
    /// The program, looked up on `PATH` like a shell would.
    pub command: String,
    pub args: Vec<String>,
    /// Set for the server on top of Junees's environment, which loses the
    /// variables bash never sees first.
    pub env: BTreeMap<String, String>,
    /// `false` keeps the entry without starting the server.
    pub enabled: bool,
    /// `true`: its tools run without an approval question.
    pub trust: bool,
    /// How long one tool call may take.
    pub call_timeout: Duration,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawFile {
    #[serde(default)]
    servers: BTreeMap<String, RawServer>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawServer {
    command: String,
    #[serde(default)]
    args: Vec<String>,
    #[serde(default)]
    env: BTreeMap<String, String>,
    #[serde(default = "enabled_by_default")]
    enabled: bool,
    #[serde(default)]
    trust: bool,
    timeout_secs: Option<u64>,
}

fn enabled_by_default() -> bool {
    true
}

/// The servers of the file at `path`, in name order; no file means no
/// servers. A file other users can read or change is refused.
pub fn load(path: &Path) -> Result<Vec<ServerConfig>> {
    let metadata = match std::fs::metadata(path) {
        Ok(metadata) => metadata,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(err) => return Err(err).with_context(|| format!("cannot read {}", path.display())),
    };
    if !metadata.is_file() {
        bail!("{} is not a regular file", path.display());
    }
    // SAFETY: getuid only returns this process's user id.
    let uid = unsafe { libc::getuid() };
    if metadata.uid() != uid {
        bail!(
            "{} belongs to another user; it names programs Junees runs, so Junees reads it \
             only when it is yours",
            path.display()
        );
    }
    let mode = metadata.permissions().mode() & 0o777;
    if mode & 0o077 != 0 {
        bail!(
            "{} can be read or changed by other users (mode {mode:o}); it names programs \
             Junees runs and may hold tokens, so run `chmod 600 {}`",
            path.display(),
            path.display()
        );
    }
    if metadata.len() > MAX_FILE_BYTES {
        bail!("{} is larger than 1 MiB", path.display());
    }
    let text =
        std::fs::read_to_string(path).with_context(|| format!("cannot read {}", path.display()))?;
    parse(&text).with_context(|| format!("{} is not a valid MCP server list", path.display()))
}

/// The servers of a file's text, in name order.
pub fn parse(text: &str) -> Result<Vec<ServerConfig>> {
    let raw: RawFile = serde_json::from_str(text).map_err(|err| {
        // A type error quotes the value, which may be a token in the
        // wrong place: name only where it is.
        let message = err.to_string();
        let what = if message.starts_with("invalid type") || message.starts_with("invalid value") {
            "a value has the wrong type".to_string()
        } else {
            message
                .split(" at line ")
                .next()
                .unwrap_or_default()
                .to_string()
        };
        anyhow::anyhow!("{what} (line {}, column {})", err.line(), err.column())
    })?;
    raw.servers
        .into_iter()
        .map(|(name, server)| {
            check_name(&name)?;
            server_config(name.clone(), server).with_context(|| format!("server `{name}`"))
        })
        .collect()
}

fn server_config(name: String, raw: RawServer) -> Result<ServerConfig> {
    if raw.command.trim().is_empty() {
        bail!("`command` is empty");
    }
    for (key, value) in &raw.env {
        if key.is_empty() || key.contains(['=', '\0']) {
            bail!("`env` has a variable name that is empty or holds `=`");
        }
        if value.contains('\0') {
            bail!("`env` value of {key} holds a NUL character");
        }
    }
    if raw.args.iter().any(|arg| arg.contains('\0')) || raw.command.contains('\0') {
        bail!("`command` or `args` holds a NUL character");
    }
    let call_timeout = match raw.timeout_secs {
        None => DEFAULT_CALL_TIMEOUT,
        Some(secs @ 1..=MAX_CALL_TIMEOUT_SECS) => Duration::from_secs(secs),
        Some(secs) => {
            bail!("`timeout_secs` must be from 1 to {MAX_CALL_TIMEOUT_SECS}, got {secs}")
        }
    };
    Ok(ServerConfig {
        name,
        command: raw.command,
        args: raw.args,
        env: raw.env,
        enabled: raw.enabled,
        trust: raw.trust,
        call_timeout,
    })
}

/// A server name: 1 to [`MAX_NAME_CHARS`] ASCII letters, digits, `_` and
/// `-`, so the tool names built from it stay valid.
pub fn check_name(name: &str) -> Result<()> {
    let valid = !name.is_empty()
        && name.len() <= MAX_NAME_CHARS
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-');
    if !valid {
        bail!(
            "server name `{}` must be 1 to {MAX_NAME_CHARS} ASCII letters, digits, `_` or `-`",
            crate::prompt_text::escape_hidden(name)
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_servers_in_name_order_with_defaults() {
        let servers = parse(
            r#"{"servers": {
                "zeta": {"command": "z", "args": ["-v"], "env": {"TOKEN": "t"},
                         "enabled": false, "trust": true, "timeout_secs": 5},
                "alpha": {"command": "a"}
            }}"#,
        )
        .unwrap();
        assert_eq!(servers.len(), 2);
        let alpha = &servers[0];
        assert_eq!(alpha.name, "alpha");
        assert_eq!(alpha.command, "a");
        assert!(alpha.args.is_empty() && alpha.env.is_empty());
        assert!(alpha.enabled && !alpha.trust);
        assert_eq!(alpha.call_timeout, DEFAULT_CALL_TIMEOUT);
        let zeta = &servers[1];
        assert_eq!(zeta.args, ["-v"]);
        assert_eq!(zeta.env.get("TOKEN").map(String::as_str), Some("t"));
        assert!(!zeta.enabled && zeta.trust);
        assert_eq!(zeta.call_timeout, Duration::from_secs(5));
        assert!(parse("{}").unwrap().is_empty());
    }

    #[test]
    fn refuses_bad_entries_without_quoting_values() {
        let err = |text: &str| format!("{:#}", parse(text).err().unwrap());
        assert!(
            err(r#"{"servers": {"a b": {"command": "x"}}}"#).contains("server name `a b`"),
            "{}",
            err(r#"{"servers": {"a b": {"command": "x"}}}"#)
        );
        let long = "x".repeat(MAX_NAME_CHARS + 1);
        assert!(
            err(&format!(
                r#"{{"servers": {{"{long}": {{"command": "x"}}}}}}"#
            ))
            .contains(&long)
        );
        assert!(err(r#"{"servers": {"a": {"command": " "}}}"#).contains("`command` is empty"));
        assert!(
            err(r#"{"servers": {"a": {"command": "x", "trusted": true}}}"#)
                .contains("unknown field `trusted`")
        );
        assert!(
            err(r#"{"servers": {"a": {"command": "x", "timeout_secs": 0}}}"#)
                .contains("`timeout_secs` must be from 1 to 3600, got 0")
        );
        assert!(
            err(r#"{"servers": {"a": {"command": "x", "env": {"A=B": "c"}}}}"#)
                .contains("holds `=`")
        );
        // A token where a flag belongs is not repeated.
        let message = err(r#"{"servers": {"a": {"command": "x", "trust": "ghp_secret"}}}"#);
        assert!(message.contains("a value has the wrong type"), "{message}");
        assert!(!message.contains("ghp_secret"), "{message}");
        let message = err(r#"{"servers": {"a": {"command": "x", "env": {"T": 12345}}}}"#);
        assert!(!message.contains("12345"), "{message}");
    }

    #[test]
    fn loads_only_a_private_file_of_the_user() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(FILE_NAME);
        assert!(load(&path).unwrap().is_empty());
        std::fs::write(&path, r#"{"servers": {"a": {"command": "x"}}}"#).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
        let message = format!("{:#}", load(&path).err().unwrap());
        assert!(message.contains("mode 644"), "{message}");
        assert!(message.contains("chmod 600"), "{message}");
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
        assert_eq!(load(&path).unwrap()[0].name, "a");
        std::fs::write(&path, "{\"servers\": [").unwrap();
        let message = format!("{:#}", load(&path).err().unwrap());
        assert!(message.contains("not a valid MCP server list"), "{message}");
    }
}
