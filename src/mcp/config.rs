//! `$JUNEES_HOME/mcp.json`: the MCP servers Junees starts.
//!
//! ```json
//! { "servers": { "github": { "command": "github-mcp", "args": ["stdio"],
//!   "env": {"GITHUB_TOKEN": "..."}, "enabled": true, "trust": false,
//!   "timeout_secs": 120, "cwd": "/home/me/servers" },
//!   "docs": { "url": "https://mcp.example.com/mcp",
//!   "headers": {"Authorization": "Bearer env:DOCS_TOKEN"} } } }
//! ```
//!
//! A server has either a `command` (stdio) or a `url` (Streamable HTTP,
//! [`super::http::target`]).
//!
//! Servers run in `cwd`, or in `/` without one, never in the folder Junees
//! works in: a server started there may run files from it (`python -m`
//! looks in the current folder first), and the model writes files there
//! without a question.
//!
//! JSON rather than a table in `config.toml`: the settings file parser
//! reads flat keys only. The file starts programs and may hold tokens, so
//! it must belong to the user and be private (mode 0600), and the model's
//! tools may not read or change it (see [`crate::tools::SettingsFile`]).

use std::collections::BTreeMap;
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result, bail};
use serde::Deserialize;

use super::http::HttpConfig;

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
    /// The folder it runs in (absolute); `None` runs it in `/`.
    pub cwd: Option<PathBuf>,
    /// `Some` for a server reached over Streamable HTTP; `command` is then
    /// empty and `args`, `env` and `cwd` are unused.
    pub http: Option<HttpConfig>,
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
    #[serde(default)]
    command: Option<String>,
    url: Option<String>,
    #[serde(default)]
    headers: BTreeMap<String, String>,
    #[serde(default)]
    args: Vec<String>,
    #[serde(default)]
    env: BTreeMap<String, String>,
    #[serde(default = "enabled_by_default")]
    enabled: bool,
    #[serde(default)]
    trust: bool,
    timeout_secs: Option<u64>,
    cwd: Option<String>,
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
    let call_timeout = match raw.timeout_secs {
        None => DEFAULT_CALL_TIMEOUT,
        Some(secs @ 1..=MAX_CALL_TIMEOUT_SECS) => Duration::from_secs(secs),
        Some(secs) => {
            bail!("`timeout_secs` must be from 1 to {MAX_CALL_TIMEOUT_SECS}, got {secs}")
        }
    };
    let command = match (raw.command, &raw.url) {
        (Some(_), Some(_)) => bail!("give either `command` (stdio) or `url` (HTTP), not both"),
        (None, None) => bail!("needs a `command` to run or a `url` to connect to"),
        (Some(command), None) => command,
        (None, Some(url)) => {
            if !raw.args.is_empty() || !raw.env.is_empty() || raw.cwd.is_some() {
                bail!("`args`, `env` and `cwd` are for `command` servers, not `url` ones");
            }
            return Ok(ServerConfig {
                name,
                command: String::new(),
                args: Vec::new(),
                env: BTreeMap::new(),
                enabled: raw.enabled,
                trust: raw.trust,
                call_timeout,
                cwd: None,
                http: Some(HttpConfig::parse(url, &raw.headers)?),
            });
        }
    };
    if !raw.headers.is_empty() {
        bail!("`headers` are for `url` servers");
    }
    if command.trim().is_empty() {
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
    if raw.args.iter().any(|arg| arg.contains('\0')) || command.contains('\0') {
        bail!("`command` or `args` holds a NUL character");
    }
    if let Some(cwd) = &raw.cwd
        && (cwd.contains('\0') || !Path::new(cwd).is_absolute())
    {
        bail!("`cwd` must be an absolute path");
    }
    Ok(ServerConfig {
        name,
        command,
        args: raw.args,
        env: raw.env,
        enabled: raw.enabled,
        trust: raw.trust,
        call_timeout,
        cwd: raw.cwd.map(PathBuf::from),
        http: None,
    })
}

impl ServerConfig {
    /// The folder the server runs in: its `cwd`, or `/`. Never the folder
    /// Junees works in, unless `cwd` names it.
    pub fn run_folder(&self) -> &Path {
        self.cwd.as_deref().unwrap_or(Path::new("/"))
    }

    /// The program to run: `command` as given when it is a name to look up
    /// on `PATH` or an absolute path; a relative path (`./server`,
    /// `bin/server`) is taken from [`ServerConfig::run_folder`].
    pub fn program(&self) -> PathBuf {
        if self.has_relative_path() {
            self.run_folder().join(&self.command)
        } else {
            PathBuf::from(&self.command)
        }
    }

    fn has_relative_path(&self) -> bool {
        self.command.contains('/') && Path::new(&self.command).is_relative()
    }

    /// Why the server may not start while Junees works in `workdir`: a
    /// relative `command` that leads into it would run a file the model may
    /// write without a question. `trust` allows it.
    pub fn check_workdir(&self, workdir: &Path) -> std::result::Result<(), String> {
        if self.trust || self.http.is_some() || !self.has_relative_path() {
            return Ok(());
        }
        if inside(&self.program(), workdir) {
            return Err(format!(
                "`command` `{}` is a relative path into the folder Junees works in ({}), \
                 where the model may write files without a question; give an absolute path \
                 outside it, or set `\"trust\": true` to run it anyway",
                crate::prompt_text::escape_hidden(&self.command),
                workdir.display()
            ));
        }
        Ok(())
    }
}

/// The environment variables the HTTP servers of `<home>/mcp.json` read
/// their header values from; none when the file is missing or broken.
/// Junees hides them from bash, MCP servers and plugins.
pub fn header_env_names(home: &Path) -> Vec<String> {
    let Ok(servers) = load(&home.join(FILE_NAME)) else {
        return Vec::new();
    };
    let mut names: Vec<String> = servers
        .iter()
        .filter_map(|server| server.http.as_ref())
        .flat_map(|http| http.env_names().map(str::to_string))
        .collect();
    names.sort();
    names.dedup();
    names
}

/// Whether the absolute `path` lies in `folder`, resolving links and `..`
/// like the kernel. The last part counts unresolved too: a link there is a
/// file in the folder that could be changed to point elsewhere.
fn inside(path: &Path, folder: &Path) -> bool {
    let folder = resolve(folder);
    let unresolved_name = match (path.parent(), path.file_name()) {
        (Some(parent), Some(name)) => Some(resolve(parent).join(name)),
        _ => None,
    };
    [unresolved_name, Some(resolve(path))]
        .into_iter()
        .flatten()
        .any(|path| path.starts_with(&folder))
}

/// The absolute `path` with links and `..` resolved as far as it exists;
/// the parts that do not exist yet (the model may create them later) are
/// added as written.
fn resolve(path: &Path) -> PathBuf {
    for existing in path.ancestors() {
        let Ok(mut resolved) = existing.canonicalize() else {
            continue;
        };
        let rest = path.strip_prefix(existing).unwrap_or(Path::new(""));
        for part in rest.components() {
            match part {
                std::path::Component::ParentDir => {
                    resolved.pop();
                }
                std::path::Component::Normal(name) => resolved.push(name),
                _ => {}
            }
        }
        return resolved;
    }
    path.to_path_buf()
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
                         "enabled": false, "trust": true, "timeout_secs": 5,
                         "cwd": "/srv/mcp"},
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
        assert_eq!(alpha.cwd, None);
        assert_eq!(alpha.run_folder(), Path::new("/"));
        let zeta = &servers[1];
        assert_eq!(zeta.args, ["-v"]);
        assert_eq!(zeta.env.get("TOKEN").map(String::as_str), Some("t"));
        assert!(!zeta.enabled && zeta.trust);
        assert_eq!(zeta.call_timeout, Duration::from_secs(5));
        assert_eq!(zeta.run_folder(), Path::new("/srv/mcp"));
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
        assert!(
            err(r#"{"servers": {"a": {"command": "x", "cwd": "servers"}}}"#)
                .contains("`cwd` must be an absolute path")
        );
        // A token where a flag belongs is not repeated.
        let message = err(r#"{"servers": {"a": {"command": "x", "trust": "ghp_secret"}}}"#);
        assert!(message.contains("a value has the wrong type"), "{message}");
        assert!(!message.contains("ghp_secret"), "{message}");
        let message = err(r#"{"servers": {"a": {"command": "x", "env": {"T": 12345}}}}"#);
        assert!(!message.contains("12345"), "{message}");
    }

    #[test]
    fn url_servers_take_headers_and_no_process_settings() {
        let servers = parse(
            r#"{"servers": {"docs": {"url": "https://mcp.example.com/mcp",
                "headers": {"Authorization": "Bearer env:DOCS_TOKEN"},
                "trust": true, "timeout_secs": 9}}}"#,
        )
        .unwrap();
        let docs = &servers[0];
        let http = docs.http.as_ref().unwrap();
        assert_eq!(http.origin(), "https://mcp.example.com");
        assert_eq!(
            http.describe_headers(),
            "Authorization (from env DOCS_TOKEN)"
        );
        assert!(docs.trust && docs.enabled && docs.command.is_empty());
        assert_eq!(docs.call_timeout, Duration::from_secs(9));
        assert_eq!(docs.check_workdir(Path::new("/")), Ok(()));

        let err = |text: &str| format!("{:#}", parse(text).err().unwrap());
        assert!(
            err(r#"{"servers": {"a": {"url": "https://x.dev/", "command": "x"}}}"#)
                .contains("not both")
        );
        assert!(err(r#"{"servers": {"a": {}}}"#).contains("needs a `command`"));
        assert!(
            err(r#"{"servers": {"a": {"url": "https://x.dev/", "env": {"A": "b"}}}}"#)
                .contains("not `url` ones")
        );
        assert!(
            err(r#"{"servers": {"a": {"command": "x", "headers": {"A": "env:B"}}}}"#)
                .contains("are for `url` servers")
        );
        let message =
            err(r#"{"servers": {"a": {"url": "https://x.dev/", "headers": {"A": "s3cret"}}}}"#);
        assert!(message.contains("server `a`"), "{message}");
        assert!(!message.contains("s3cret"), "{message}");
        assert!(err(r#"{"servers": {"a": {"url": "http://x.dev/"}}}"#).contains("https"));
    }

    #[test]
    fn header_variables_are_listed_for_hiding() {
        let dir = tempfile::tempdir().unwrap();
        assert!(header_env_names(dir.path()).is_empty());
        let path = dir.path().join(FILE_NAME);
        std::fs::write(
            &path,
            r#"{"servers": {
                "a": {"url": "https://a.dev/", "headers": {"Authorization": "Bearer env:A_TOKEN", "X-Team": "env:TEAM"}},
                "b": {"url": "https://b.dev/", "headers": {"X-Key": "env:A_TOKEN"}, "enabled": false},
                "c": {"command": "x", "env": {"NOT_THIS": "v"}}
            }}"#,
        )
        .unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
        assert_eq!(header_env_names(dir.path()), ["A_TOKEN", "TEAM"]);
    }

    #[test]
    fn relative_commands_into_the_working_folder_need_trust() {
        let work = tempfile::tempdir().unwrap();
        let other = tempfile::tempdir().unwrap();
        std::fs::create_dir(work.path().join("bin")).unwrap();
        std::os::unix::fs::symlink(work.path(), other.path().join("link")).unwrap();
        let server = |command: &str, cwd: &Path| {
            let text = json_list(command, cwd);
            parse(&text).unwrap().remove(0)
        };
        let check = |config: &ServerConfig| config.check_workdir(work.path());

        // Names on PATH and absolute paths are the user's choice.
        assert_eq!(check(&server("python3", work.path())), Ok(()));
        let absolute = work.path().join("bin/server").display().to_string();
        assert_eq!(check(&server(&absolute, work.path())), Ok(()));
        // Relative paths are taken from the run folder.
        let relative = server("./bin/server", work.path());
        assert_eq!(relative.program(), work.path().join("./bin/server"));
        let message = check(&relative).unwrap_err();
        assert!(message.contains("relative path into the folder Junees works in"));
        assert!(message.contains("\"trust\": true"), "{message}");
        assert!(check(&server("bin/../bin/server", work.path())).is_err());
        // Through a link and `..` into the folder.
        assert!(check(&server("link/bin/server", other.path())).is_err());
        assert!(check(&server("../x/bin/server", &work.path().join("bin"))).is_err());
        // Folders the model has yet to create count too.
        assert!(check(&server("new/server", work.path())).is_err());
        // Outside it, or trusted, it may start.
        assert_eq!(check(&server("./server", other.path())), Ok(()));
        let mut trusted = relative.clone();
        trusted.trust = true;
        assert_eq!(check(&trusted), Ok(()));
        // Without `cwd` the run folder is `/`, not the working folder.
        let plain = parse(r#"{"servers": {"a": {"command": "./server"}}}"#)
            .unwrap()
            .remove(0);
        assert_eq!(plain.program(), Path::new("/server"));
        assert_eq!(check(&plain), Ok(()));
    }

    fn json_list(command: &str, cwd: &Path) -> String {
        serde_json::json!({"servers": {"a": {"command": command, "cwd": cwd}}}).to_string()
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
