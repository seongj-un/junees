//! `plugin.json`: what a plugin offers.
//!
//! ```json
//! { "description": "Issue tracker tools",
//!   "trust": false,
//!   "tools": [{ "name": "search", "description": "Searches the issues",
//!               "input_schema": {"type": "object", "properties": {"q": {"type": "string"}}},
//!               "command": ["python3", "search.py"], "timeout_secs": 60 }],
//!   "hooks": { "pre_tool": [{ "command": ["./check.sh"], "timeout_secs": 10 }],
//!              "post_turn": [{ "command": ["./notify.sh"] }] },
//!   "prompt": "PROMPT.md" }
//! ```
//!
//! Commands are argument lists, never shell lines. They run in the
//! plugin's folder; a program given as a relative path (`./check.sh`,
//! `bin/tool`) is taken from that folder and may not leave it (`..`).
//! The file names programs Junees runs, so it must belong to the user and
//! be private (mode 0600), and the plugin's folder may not be writable by
//! other users, as for `mcp.json` ([`crate::mcp::config`]).

use std::collections::HashSet;
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::{Component, Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result, bail};
use serde::Deserialize;
use serde_json::{Value, json};

use crate::prompt_text::escape_hidden;

/// The manifest's name inside a plugin's folder.
pub const FILE_NAME: &str = "plugin.json";
/// The longest plugin name; the rest of the 64 characters of a tool name
/// (`plugin__<plugin>__<tool>`) is left for the tool.
pub const MAX_NAME_CHARS: usize = 32;
/// The longest tool name in a manifest (the Messages API limit).
pub const MAX_TOOL_NAME_CHARS: usize = 64;
/// How long a tool's command may run unless it sets `timeout_secs`.
pub const DEFAULT_TOOL_TIMEOUT: Duration = Duration::from_secs(60);
pub const MAX_TOOL_TIMEOUT_SECS: u64 = 3600;
/// How long a hook may run unless it sets `timeout_secs`.
pub const DEFAULT_HOOK_TIMEOUT: Duration = Duration::from_secs(10);
pub const MAX_HOOK_TIMEOUT_SECS: u64 = 300;
/// A larger manifest is refused unread.
const MAX_FILE_BYTES: u64 = 1024 * 1024;

/// A command of a tool or hook.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Command {
    /// The program, then its arguments.
    pub argv: Vec<String>,
    pub timeout: Duration,
}

impl Command {
    /// The program to run from the plugin's folder `dir`: a name looked up
    /// on `PATH` or an absolute path as given, a relative path taken from
    /// `dir`.
    pub fn program(&self, dir: &Path) -> PathBuf {
        let program = &self.argv[0];
        if program.contains('/') && Path::new(program).is_relative() {
            dir.join(program)
        } else {
            PathBuf::from(program)
        }
    }

    /// The program as written, for messages.
    pub fn shown(&self) -> String {
        escape_hidden(&self.argv[0])
    }
}

/// A tool of the manifest.
#[derive(Debug, Clone, PartialEq)]
pub struct ToolDef {
    pub name: String,
    pub description: String,
    pub input_schema: Value,
    pub command: Command,
}

/// A checked manifest.
#[derive(Debug, Clone, PartialEq)]
pub struct Manifest {
    pub description: String,
    /// `true`: its author recommends trusting it. Shown on install and
    /// enable; only `junees plugin enable --trust` trusts a plugin.
    pub trust: bool,
    pub tools: Vec<ToolDef>,
    pub pre_tool: Vec<Command>,
    pub post_turn: Vec<Command>,
    /// The prompt fragment's file, relative to the plugin's folder.
    pub prompt: Option<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawManifest {
    #[serde(default)]
    description: String,
    #[serde(default)]
    trust: bool,
    #[serde(default)]
    tools: Vec<RawTool>,
    #[serde(default)]
    hooks: RawHooks,
    prompt: Option<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawTool {
    name: String,
    description: String,
    input_schema: Option<Value>,
    command: Vec<String>,
    timeout_secs: Option<u64>,
}

#[derive(Deserialize, Default)]
#[serde(deny_unknown_fields)]
struct RawHooks {
    #[serde(default)]
    pre_tool: Vec<RawHook>,
    #[serde(default)]
    post_turn: Vec<RawHook>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawHook {
    command: Vec<String>,
    timeout_secs: Option<u64>,
}

/// A plugin name: 1 to [`MAX_NAME_CHARS`] ASCII letters, digits, `_` and
/// `-`, so it is a plain folder name and the tool names built from it stay
/// valid.
pub fn check_name(name: &str) -> Result<()> {
    if !is_word(name, MAX_NAME_CHARS) {
        bail!(
            "plugin name `{}` must be 1 to {MAX_NAME_CHARS} ASCII letters, digits, `_` or `-`",
            escape_hidden(name)
        );
    }
    Ok(())
}

fn is_word(name: &str, max: usize) -> bool {
    !name.is_empty()
        && name.len() <= max
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
}

/// Why a file or folder holding programs Junees runs may not be used: it
/// belongs to another user, or (with `private`) others may read or change
/// it, or (folders) others may change what it holds.
pub fn check_owner(path: &Path, metadata: &std::fs::Metadata, private: bool) -> Result<()> {
    // SAFETY: getuid only returns this process's user id.
    let uid = unsafe { libc::getuid() };
    if metadata.uid() != uid {
        bail!(
            "{} belongs to another user; it names programs Junees runs, so Junees uses it \
             only when it is yours",
            path.display()
        );
    }
    let mode = metadata.permissions().mode() & 0o777;
    let open = if private { mode & 0o077 } else { mode & 0o022 };
    if open != 0 {
        let fix = if private { "chmod 600" } else { "chmod go-w" };
        bail!(
            "{} can be {} by other users (mode {mode:o}); it names programs Junees runs, so \
             run `{fix} {}`",
            path.display(),
            if private {
                "read or changed"
            } else {
                "changed"
            },
            path.display()
        );
    }
    Ok(())
}

/// The manifest of the plugin in `dir`, once the folder and the file pass
/// [`check_owner`].
pub fn load(dir: &Path) -> Result<Manifest> {
    let metadata =
        std::fs::metadata(dir).with_context(|| format!("cannot read {}", dir.display()))?;
    if !metadata.is_dir() {
        bail!("{} is not a folder", dir.display());
    }
    check_owner(dir, &metadata, false)?;
    let path = dir.join(FILE_NAME);
    let metadata =
        std::fs::metadata(&path).with_context(|| format!("cannot read {}", path.display()))?;
    if !metadata.is_file() {
        bail!("{} is not a regular file", path.display());
    }
    check_owner(&path, &metadata, true)?;
    read(&path)
}

/// The manifest at `path`, checking only what it says (an install reads
/// the source this way, before its copy is made private).
pub fn read(path: &Path) -> Result<Manifest> {
    let metadata =
        std::fs::metadata(path).with_context(|| format!("cannot read {}", path.display()))?;
    if metadata.len() > MAX_FILE_BYTES {
        bail!("{} is larger than 1 MiB", path.display());
    }
    let text =
        std::fs::read_to_string(path).with_context(|| format!("cannot read {}", path.display()))?;
    parse(&text).with_context(|| format!("{} is not a valid plugin manifest", path.display()))
}

/// The manifest of a file's text.
pub fn parse(text: &str) -> Result<Manifest> {
    let raw: RawManifest = serde_json::from_str(text).map_err(|err| {
        // A type error quotes the value: name only where it is.
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
    let mut names = HashSet::new();
    let mut tools = Vec::new();
    for tool in raw.tools {
        let name = tool.name.clone();
        let tool = tool_def(tool).with_context(|| format!("tool `{}`", escape_hidden(&name)))?;
        if !names.insert(tool.name.clone()) {
            bail!("two tools are named `{}`", tool.name);
        }
        tools.push(tool);
    }
    let hooks = |list: Vec<RawHook>, kind: &str| -> Result<Vec<Command>> {
        list.into_iter()
            .enumerate()
            .map(|(i, hook)| {
                command(
                    hook.command,
                    hook.timeout_secs,
                    DEFAULT_HOOK_TIMEOUT,
                    MAX_HOOK_TIMEOUT_SECS,
                )
                .with_context(|| format!("{kind} hook {}", i + 1))
            })
            .collect()
    };
    let pre_tool = hooks(raw.hooks.pre_tool, "pre_tool")?;
    let post_turn = hooks(raw.hooks.post_turn, "post_turn")?;
    if let Some(prompt) = &raw.prompt {
        check_relative(prompt).context("`prompt`")?;
    }
    Ok(Manifest {
        description: raw.description.trim().to_string(),
        trust: raw.trust,
        tools,
        pre_tool,
        post_turn,
        prompt: raw.prompt,
    })
}

fn tool_def(raw: RawTool) -> Result<ToolDef> {
    if !is_word(&raw.name, MAX_TOOL_NAME_CHARS) {
        bail!("the name must be 1 to {MAX_TOOL_NAME_CHARS} ASCII letters, digits, `_` or `-`");
    }
    if raw.description.trim().is_empty() {
        bail!("`description` is empty");
    }
    let input_schema = match raw.input_schema {
        None => json!({"type": "object", "properties": {}}),
        Some(schema @ Value::Object(_)) if schema.get("type") == Some(&json!("object")) => schema,
        Some(_) => bail!("`input_schema` must be a JSON Schema object with \"type\": \"object\""),
    };
    Ok(ToolDef {
        name: raw.name,
        description: raw.description,
        input_schema,
        command: command(
            raw.command,
            raw.timeout_secs,
            DEFAULT_TOOL_TIMEOUT,
            MAX_TOOL_TIMEOUT_SECS,
        )?,
    })
}

fn command(
    argv: Vec<String>,
    timeout_secs: Option<u64>,
    default: Duration,
    max: u64,
) -> Result<Command> {
    let Some(program) = argv.first() else {
        bail!("`command` is empty");
    };
    if program.trim().is_empty() {
        bail!("`command` names no program");
    }
    if argv.iter().any(|arg| arg.contains('\0')) {
        bail!("`command` holds a NUL character");
    }
    if program.contains('/') && Path::new(program).is_relative() {
        check_relative(program).context("`command`")?;
    }
    let timeout = match timeout_secs {
        None => default,
        Some(secs) if (1..=max).contains(&secs) => Duration::from_secs(secs),
        Some(secs) => bail!("`timeout_secs` must be from 1 to {max}, got {secs}"),
    };
    Ok(Command { argv, timeout })
}

/// A path that stays inside the plugin's folder: relative, without `..`.
fn check_relative(path: &str) -> Result<()> {
    let inside = !path.is_empty()
        && !path.contains('\0')
        && Path::new(path)
            .components()
            .all(|part| matches!(part, Component::Normal(_) | Component::CurDir));
    if !inside {
        bail!(
            "`{}` must be a path inside the plugin's folder (relative, without `..`)",
            escape_hidden(path)
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_a_full_manifest_with_defaults() {
        let manifest = parse(
            r#"{"description": " Issue tools ", "trust": true,
                "tools": [
                  {"name": "search", "description": "Searches", "command": ["python3", "s.py"]},
                  {"name": "get-1", "description": "Gets", "timeout_secs": 5,
                   "input_schema": {"type": "object", "properties": {"id": {"type": "integer"}}},
                   "command": ["./bin/get", "--json"]}
                ],
                "hooks": {"pre_tool": [{"command": ["./check.sh"], "timeout_secs": 3}],
                          "post_turn": [{"command": ["/usr/bin/notify"]}]},
                "prompt": "PROMPT.md"}"#,
        )
        .unwrap();
        assert_eq!(manifest.description, "Issue tools");
        assert!(manifest.trust);
        assert_eq!(manifest.tools.len(), 2);
        let search = &manifest.tools[0];
        assert_eq!(
            search.input_schema,
            json!({"type": "object", "properties": {}})
        );
        assert_eq!(search.command.timeout, DEFAULT_TOOL_TIMEOUT);
        let get = &manifest.tools[1];
        assert_eq!(get.command.timeout, Duration::from_secs(5));
        assert_eq!(get.input_schema["properties"]["id"]["type"], "integer");
        let dir = Path::new("/plugins/p");
        assert_eq!(get.command.program(dir), dir.join("./bin/get"));
        assert_eq!(search.command.program(dir), Path::new("python3"));
        assert_eq!(manifest.pre_tool[0].timeout, Duration::from_secs(3));
        assert_eq!(manifest.post_turn[0].timeout, DEFAULT_HOOK_TIMEOUT);
        assert_eq!(
            manifest.post_turn[0].program(dir),
            Path::new("/usr/bin/notify")
        );
        assert_eq!(manifest.prompt.as_deref(), Some("PROMPT.md"));

        let empty = parse("{}").unwrap();
        assert!(empty.tools.is_empty() && empty.pre_tool.is_empty() && !empty.trust);
    }

    #[test]
    fn refuses_bad_manifests_without_quoting_values() {
        let err = |text: &str| format!("{:#}", parse(text).err().unwrap());
        let tool = |fields: &str| {
            err(&format!(
                r#"{{"tools": [{{"name": "t", "description": "d", "command": ["x"]{fields}}}]}}"#
            ))
        };
        assert!(tool(r#", "timeout_secs": 0"#).contains("must be from 1 to 3600, got 0"));
        assert!(tool(r#", "input_schema": {"type": "string"}"#).contains("\"type\": \"object\""));
        assert!(tool(r#", "shell": true"#).contains("unknown field `shell`"));
        assert!(
            err(r#"{"tools": [{"name": "a b", "description": "d", "command": ["x"]}]}"#)
                .contains("tool `a b`: the name must be 1 to 64")
        );
        assert!(
            err(r#"{"tools": [{"name": "t", "description": " ", "command": ["x"]}]}"#)
                .contains("`description` is empty")
        );
        assert!(
            err(r#"{"tools": [{"name": "t", "description": "d", "command": []}]}"#)
                .contains("`command` is empty")
        );
        let two = r#"{"tools": [{"name": "t", "description": "d", "command": ["x"]},
                                {"name": "t", "description": "e", "command": ["y"]}]}"#;
        assert!(err(two).contains("two tools are named `t`"));
        assert!(
            err(r#"{"hooks": {"pre_tool": [{"command": ["../escape.sh"]}]}}"#)
                .contains("pre_tool hook 1: `command`: `../escape.sh` must be a path inside")
        );
        assert!(
            err(r#"{"hooks": {"post_turn": [{"command": ["x"], "timeout_secs": 301}]}}"#)
                .contains("from 1 to 300")
        );
        assert!(err(r#"{"hooks": {"on_start": []}}"#).contains("unknown field `on_start`"));
        for prompt in ["/etc/passwd", "../x.md", "a/../../x.md", ""] {
            let message = err(&json!({ "prompt": prompt }).to_string());
            assert!(message.contains("`prompt`"), "{prompt}: {message}");
        }
        // A token where a flag belongs is not repeated.
        let message = err(r#"{"trust": "ghp_secret"}"#);
        assert!(message.contains("a value has the wrong type"), "{message}");
        assert!(!message.contains("ghp_secret"), "{message}");
    }

    #[test]
    fn checks_names() {
        assert!(check_name("issues_2-x").is_ok());
        for bad in ["", "a b", "../x", "x/y", ".hidden", &"x".repeat(33), "한글"] {
            assert!(check_name(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn loads_only_a_private_manifest_in_a_folder_of_the_user() {
        let dir = tempfile::tempdir().unwrap();
        let plugin = dir.path().join("p");
        std::fs::create_dir(&plugin).unwrap();
        let path = plugin.join(FILE_NAME);
        std::fs::write(&path, r#"{"description": "x"}"#).unwrap();
        let mode = |path: &Path, mode: u32| {
            std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode)).unwrap()
        };
        mode(&plugin, 0o755);
        mode(&path, 0o644);
        let message = format!("{:#}", load(&plugin).err().unwrap());
        assert!(message.contains("mode 644"), "{message}");
        assert!(message.contains("chmod 600"), "{message}");
        mode(&path, 0o600);
        assert_eq!(load(&plugin).unwrap().description, "x");
        mode(&plugin, 0o777);
        let message = format!("{:#}", load(&plugin).err().unwrap());
        assert!(message.contains("chmod go-w"), "{message}");
        mode(&plugin, 0o700);
        std::fs::write(&path, "{\"tools\": [").unwrap();
        let message = format!("{:#}", load(&plugin).err().unwrap());
        assert!(message.contains("not a valid plugin manifest"), "{message}");
        assert!(load(&dir.path().join("missing")).is_err());
    }
}
