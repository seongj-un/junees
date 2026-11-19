//! Hooks: commands the user sets to run at fixed points of a session
//! (`pre_tool`, `post_tool`, `turn_end`, `session_start`, `user_prompt`).
//! A hook can block a tool call or an input (exit code 2) or add a note
//! for the model (`{"message": "..."}` on stdout); it can never change a
//! tool's input, and a call it lets through still goes through the
//! approval questions and the policies. See `docs/hooks.md`.
//!
//! Where hooks come from:
//! - the user's: the `[[hooks]]` tables of `$JUNEES_HOME/config.toml`
//!   (after its flat settings), then of `$JUNEES_HOME/hooks.toml`. They
//!   run without a question; a broken one stops the run from starting,
//!   since a policy hook that silently drops out would let calls through.
//! - the project's: `.junees/hooks.toml` in the folder the project
//!   instruction files are read up to ([`crate::context_files::levels`]).
//!   Someone else may have written it, so it runs only once a person
//!   trusted its exact content ([`trust`]); unattended runs never ask and
//!   skip it with a warning.

pub mod cli;
pub mod config;
pub mod run;
pub mod trust;

#[cfg(test)]
mod tests;

use std::io::Read;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

pub use config::{Event, OnError, Spec};
pub use run::Hooks;

/// The user's hooks file in the state folder.
pub const FILE_NAME: &str = "hooks.toml";
/// A project's hooks file, under the folder the instruction files are
/// read up to.
pub const PROJECT_FILE: &str = ".junees/hooks.toml";
/// A larger hooks file is refused unread.
const MAX_FILE_BYTES: u64 = 256 * 1024;

/// Where a hook was set.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Origin {
    /// The user's settings file or `hooks.toml`.
    User(PathBuf),
    /// A project's `.junees/hooks.toml`.
    Project(PathBuf),
}

impl Origin {
    pub fn file(&self) -> &Path {
        match self {
            Origin::User(file) | Origin::Project(file) => file,
        }
    }

    /// `user` or `project`.
    pub fn kind(&self) -> &'static str {
        match self {
            Origin::User(_) => "user",
            Origin::Project(_) => "project",
        }
    }
}

/// A hook and where it was set.
#[derive(Debug, Clone)]
pub struct Hook {
    pub spec: Spec,
    pub origin: Origin,
    /// The files it runs, when they are in the session's working folder
    /// (which the model may change): pinned when the hook was read, and
    /// checked again before each run ([`pin_paths_for`]).
    pub pins: Vec<Pin>,
}

/// The file a hook runs, pinned by its content.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Pin {
    /// Its path, the folders resolved, the last part as written.
    pub path: PathBuf,
    /// The word of `command` that names it: 0 for the program, else an
    /// interpreter's argument.
    pub arg: usize,
    /// The hook runs a copy of it in place of that word: the program, or
    /// the script an interpreter runs. Other files an interpreter is given
    /// are only checked.
    pub copied: bool,
    /// The SHA-256 of its content when the hook was read, or why it
    /// could not be read (the hook then does not run).
    pub digest: Result<String, String>,
}

/// A larger file a hook runs cannot be pinned (the hook does not run).
const MAX_PINNED_BYTES: u64 = 16 * 1024 * 1024;

/// The folder under `$JUNEES_HOME` where a hook's pinned files are copied
/// to run ([`run`]): one of the data folders the model's tools and the
/// sandbox may not change.
pub const COPIES_FOLDER: &str = "hook-copies";

/// What a hook whose file changed after it was read says instead of
/// running.
pub const CHANGED: &str = "훅이 부르는 파일이 바뀌어 실행하지 않았습니다";
/// What a hook whose file could not be pinned when it was read (missing,
/// a link, too large) says instead of running.
pub const NOT_PINNED: &str = "훅이 부르는 파일을 고정하지 못해 실행하지 않았습니다";

/// The user hook pins for files the hook runs, when they are in `workdir` or, for
/// a project's hook, in the project's folder `project`: the model may
/// change files there, so the hook must run only what was read. The files are `command[0]` when it has a
/// `/`, and the script a known interpreter runs ([`script_arg`]). A
/// program found on PATH and files outside those folders are not pinned.
///
/// Which word an interpreter runs is read from its options, and an option
/// Junees does not know can make it read wrong (`node --title ./a.js
/// ./b.js` runs `b.js`). So every other argument of an interpreter that
/// names an existing file there (also as `--opt=./x` or `-X./x`) is
/// pinned too, checked before each run without being copied.
fn user_pins(spec: &Spec, workdir: &Path) -> Vec<Pin> {
    pins_from(pin_paths_for(spec, workdir, None).expect("only project paths are validated"))
}

struct PinPath {
    arg: usize,
    path: PathBuf,
    copied: bool,
}

fn pins_from(paths: Vec<PinPath>) -> Vec<Pin> {
    paths
        .into_iter()
        .map(|PinPath { arg, path, copied }| Pin {
            digest: pinned_digest(&path),
            path,
            arg,
            copied,
        })
        .collect()
}

/// Resolve and validate the file words once, using the same plan at
/// approval and before each run. Only the program and selected script
/// are copied for execution.
fn pin_paths_for(
    spec: &Spec,
    workdir: &Path,
    project: Option<&Path>,
) -> Result<Vec<PinPath>, String> {
    let mut pins: Vec<PinPath> = Vec::new();
    for (arg, candidate, copied) in file_words(&spec.command) {
        let explicit = copied || explicit_path(candidate);
        let Some(path) = resolve(candidate, workdir) else {
            if let Some(project) = project.filter(|_| explicit) {
                return Err(project_path_error(
                    spec,
                    project,
                    arg,
                    candidate,
                    "cannot be resolved, possibly outside",
                ));
            }
            continue;
        };
        let in_scope = inside(&path, workdir, project);
        if let Some(project) = project.filter(|_| explicit && !in_scope) {
            return Err(outside_project(spec, project, arg, candidate, &path));
        }
        // User hooks retain their existing scope and non-runnable pins.
        if project.is_none() && !in_scope {
            continue;
        }
        let metadata = std::fs::symlink_metadata(&path);
        if let Some(project) = project {
            if !in_scope && metadata.is_ok() {
                return Err(outside_project(spec, project, arg, candidate, &path));
            }
            // Explicit interpreter inputs must be present at approval;
            // errors inspecting one are not treated as absence. A missing
            // program is reported when its pin is read.
            if arg != 0
                && explicit
                && let Err(err) = &metadata
            {
                let detail = if err.kind() == std::io::ErrorKind::NotFound {
                    "does not exist in".to_string()
                } else {
                    format!("cannot be inspected ({err}) in")
                };
                return Err(project_path_error(spec, project, arg, candidate, &detail));
            }
        }
        if in_scope
            && (copied
                || (metadata.is_ok_and(|meta| !meta.is_dir())
                    && !pins.iter().any(|pin| pin.path == path)))
        {
            pins.push(PinPath { arg, path, copied });
        }
    }
    Ok(pins)
}

/// Each potential file, including option values, as seen by both the
/// project validator and the pin collector. The copy candidates come
/// first to retain approval digests for unchanged pins.
fn file_words(command: &[String]) -> impl Iterator<Item = (usize, &str, bool)> {
    let program = command[0]
        .contains('/')
        .then(|| (0, command[0].as_str(), true));
    let script = script_arg(command);
    let copy = match script {
        Script::File(arg) => Some((arg, command[arg].as_str(), true)),
        _ => None,
    };
    program.into_iter().chain(copy).chain(
        command
            .iter()
            .enumerate()
            .skip(1)
            .filter(move |(arg, _)| {
                script != Script::NotInterpreter && script != Script::File(*arg)
            })
            .flat_map(|(arg, word)| {
                path_candidates(word)
                    .into_iter()
                    .map(move |path| (arg, path, false))
            }),
    )
}

/// The paths a word of an interpreter's command may name: the word, the
/// value of `--opt=value`, and the value of a short option written
/// `-Xvalue`.
fn path_candidates(word: &str) -> Vec<&str> {
    let mut candidates = Vec::new();
    if let Some(long) = word.strip_prefix("--") {
        if let Some((_, value)) = long.split_once('=') {
            candidates.push(value);
        }
    } else if let Some(short) = word.strip_prefix('-') {
        if let Some((at, letter)) = short.char_indices().next() {
            let value = &short[at + letter.len_utf8()..];
            candidates.push(value.strip_prefix('=').unwrap_or(value));
        }
    } else {
        candidates.push(word);
    }
    candidates.retain(|candidate| !candidate.is_empty());
    candidates
}

/// What a known interpreter in `command[0]` runs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Script {
    /// `command[0]` is not one of [`INTERPRETERS`].
    NotInterpreter,
    /// The file named by this word of `command`.
    File(usize),
    /// Code or a module named by this option (`-c`, `-e`, `-m`): nothing
    /// to pin.
    Code(String),
    /// No file: it reads its program from stdin.
    None,
}

/// An interpreter whose first file argument is pinned, and its options
/// that matter for finding it.
struct Interpreter {
    names: &'static [&'static str],
    /// Short options that give code or a module instead of a file (also
    /// inside a cluster such as `-ec`).
    code: &'static str,
    /// Short options that take a value (the rest of the word, or the next
    /// word).
    value: &'static str,
    /// Long options that give code or load a module.
    long_code: &'static [&'static str],
    /// Long options that take the next word as their value.
    long_value: &'static [&'static str],
}

/// The interpreters whose script argument is pinned like `command[0]`.
const INTERPRETERS: &[Interpreter] = &[
    Interpreter {
        names: &["sh", "bash", "zsh"],
        code: "c",
        value: "oO",
        long_code: &[],
        long_value: &["--rcfile", "--init-file"],
    },
    Interpreter {
        names: &["python3", "python"],
        code: "cm",
        value: "WX",
        long_code: &[],
        long_value: &["--check-hash-based-pycs"],
    },
    Interpreter {
        names: &["node"],
        code: "epr",
        value: "",
        long_code: &[
            "--eval",
            "--print",
            "--require",
            "--import",
            "--loader",
            "--experimental-loader",
        ],
        long_value: &[],
    },
    Interpreter {
        names: &["ruby"],
        // `-C dir` runs the script from another folder.
        code: "erC",
        value: "IEF",
        long_code: &[],
        long_value: &[],
    },
    Interpreter {
        names: &["perl"],
        code: "eEMmx",
        value: "",
        long_code: &[],
        long_value: &[],
    },
];

/// Programs that run what the project's files say, and which files.
const BUILD_TOOLS: &[(&str, &str)] = &[
    ("make", "the Makefile"),
    ("npm", "package.json"),
    ("npx", "the project's packages"),
    ("yarn", "package.json"),
    ("pnpm", "package.json"),
];

/// Programs that run another command given as their arguments, which
/// Junees does not look into.
const WRAPPERS: &[&str] = &[
    "env", "timeout", "nice", "nohup", "xargs", "exec", "sudo", "time",
];

/// Interpreters Junees does not read the options of, so the script they
/// run is not pinned.
const UNKNOWN_INTERPRETERS: &[&str] = &[
    "dash", "ash", "ksh", "mksh", "csh", "tcsh", "fish", "deno", "bun", "php", "lua", "luajit",
    "Rscript", "tclsh", "pwsh",
];

/// Why `hook` runs files Junees cannot pin, if it does (`junees doctor`
/// warns): a build tool, a command that runs another one (`env`,
/// `timeout`), an interpreter Junees does not know or one given
/// code or a module, or a user hook whose script is relative to whichever
/// project the session is in.
pub fn indirect(hook: &Hook) -> Option<String> {
    let command = &hook.spec.command;
    let name = Path::new(&command[0])
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or_default();
    if WRAPPERS.contains(&name) {
        return Some(format!(
            "`{name}` runs another command, which Junees does not look into: if that runs a \
             file of the working folder, the file is not pinned"
        ));
    }
    if UNKNOWN_INTERPRETERS.contains(&name) || UNKNOWN_INTERPRETERS.contains(&without_version(name))
    {
        return Some(format!(
            "`{}` is an interpreter Junees does not know, so the script it runs is not pinned",
            crate::prompt_text::escape_hidden(name)
        ));
    }
    if name == "cargo" {
        // `cargo fmt` only calls rustfmt; the others may build the
        // project.
        let sub = command[1..]
            .iter()
            .find(|word| !word.starts_with('+') && !word.starts_with('-'))
            .map(String::as_str)
            .unwrap_or_default();
        if sub == "fmt" {
            return None;
        }
        return Some(format!(
            "`cargo {}` may build the project, which runs its build.rs and proc macros; \
             Junees cannot pin them",
            crate::prompt_text::escape_hidden(sub)
        ));
    }
    if let Some((tool, files)) = BUILD_TOOLS.iter().find(|(tool, _)| *tool == name) {
        return Some(format!(
            "`{tool}` runs what {files} of the working folder say, which Junees cannot pin"
        ));
    }
    match script_arg(command) {
        Script::Code(option) => Some(format!(
            "`{name} {}` runs code or a module given on the command line: if that calls or \
             loads files of the working folder, they are not pinned",
            crate::prompt_text::escape_hidden(&option)
        )),
        Script::File(arg)
            if matches!(hook.origin, Origin::User(_))
                && !Path::new(&command[arg]).is_absolute() =>
        {
            Some(format!(
                "a user hook runs `{}` relative to the session's folder, so it runs that file \
                 of whichever project is open (pinned when the session starts)",
                crate::prompt_text::escape_hidden(&command[arg])
            ))
        }
        _ => None,
    }
}

/// The known interpreter named `name`, also with a version after the
/// name (`python3.12`, `node22`, `perl5.36`).
fn interpreter(name: &str) -> Option<&'static Interpreter> {
    let bare = without_version(name);
    INTERPRETERS
        .iter()
        .find(|known| known.names.contains(&name) || known.names.contains(&bare))
}

/// `name` without the digits and dots of a version at its end.
fn without_version(name: &str) -> &str {
    name.trim_end_matches(|c: char| c.is_ascii_digit() || c == '.')
}

/// What the interpreter `command[0]` (by its file name) runs: the first
/// word after its options, unless an option gives code or a module first.
/// `--` ends the options.
pub fn script_arg(command: &[String]) -> Script {
    let name = Path::new(&command[0])
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or_default();
    let Some(known) = interpreter(name) else {
        return Script::NotInterpreter;
    };
    let mut at = 1;
    while let Some(word) = command.get(at) {
        if word == "--" {
            return match command.get(at + 1) {
                Some(next) if next != "-" => Script::File(at + 1),
                _ => Script::None,
            };
        }
        if word == "-" {
            return Script::None;
        }
        if !word.starts_with('-') {
            return Script::File(at);
        }
        if let Some(long) = word.strip_prefix("--") {
            let (name, has_value) = match long.split_once('=') {
                Some((name, _)) => (name, true),
                None => (long, false),
            };
            let name = format!("--{name}");
            if known.long_code.contains(&name.as_str()) {
                return Script::Code(word.clone());
            }
            if known.long_value.contains(&name.as_str()) && !has_value {
                at += 1;
            }
        } else {
            let letters = &word[1..];
            for (index, letter) in letters.char_indices() {
                if known.code.contains(letter) {
                    return Script::Code(word.clone());
                }
                if known.value.contains(letter) {
                    if index + letter.len_utf8() == letters.len() {
                        at += 1;
                    }
                    break;
                }
            }
        }
        at += 1;
    }
    Script::None
}

/// `path` as a process working in `workdir` finds it: absolute, its
/// folders resolved (a linked folder cannot point it elsewhere unseen),
/// the last part as written (never followed). `None` when it names no
/// file (`..`, a trailing `/`) or its folder does not exist and it climbs
/// with `..`, so where it leads cannot be told.
fn resolve(path: &str, workdir: &Path) -> Option<PathBuf> {
    if path.ends_with('/') || path.ends_with("/.") {
        return None;
    }
    let workdir = canonical(workdir);
    let given = Path::new(path);
    let path = if given.is_absolute() {
        given.to_path_buf()
    } else {
        workdir.join(given)
    };
    let name = path.file_name()?;
    if path.components().next_back() != Some(std::path::Component::Normal(name)) {
        return None;
    }
    let parent = path.parent()?;
    match std::fs::canonicalize(parent) {
        Ok(parent) => Some(parent.join(name)),
        Err(_)
            if !path
                .components()
                .any(|part| part == std::path::Component::ParentDir) =>
        {
            Some(path)
        }
        Err(_) => None,
    }
}

/// Whether the resolved `path` is in `workdir` or in `project`.
fn inside(path: &Path, workdir: &Path, project: Option<&Path>) -> bool {
    std::iter::once(workdir)
        .chain(project)
        .any(|folder| path.starts_with(canonical(folder)))
}

/// `path` with its links resolved, or as given when it cannot be.
fn canonical(path: &Path) -> PathBuf {
    std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf())
}

/// An explicit path argument, not a logical name such as `team/default`.
/// The selected program and script are handled separately. Other values
/// without these prefixes are still candidates if they name existing files.
fn explicit_path(value: &str) -> bool {
    value.starts_with('/') || value.starts_with("./") || value.starts_with("../")
}

fn outside_project(
    spec: &Spec,
    project: &Path,
    arg: usize,
    candidate: &str,
    path: &Path,
) -> String {
    let path = crate::prompt_text::escape_hidden(&path.display().to_string());
    project_path_error(
        spec,
        project,
        arg,
        candidate,
        &format!("leads to {path}, outside"),
    )
}

fn project_path_error(spec: &Spec, project: &Path, arg: usize, path: &str, detail: &str) -> String {
    let place = if arg == 0 {
        "starts with".to_string()
    } else {
        format!("gives the interpreter (word {})", arg + 1)
    };
    format!(
        "hook at line {}: `command` {place} `{}`, which {detail} the project folder {}",
        spec.line,
        crate::prompt_text::escape_hidden(path),
        crate::prompt_text::escape_hidden(&project.display().to_string()),
    )
}

/// Revalidate the original project arguments, not just the locations
/// they resolved to at approval: even an unchanged file at an old path
/// is not evidence that the argument still names it.
fn check_project_pins(hook: &Hook, workdir: &Path) -> Result<(), String> {
    let Origin::Project(file) = &hook.origin else {
        return Ok(());
    };
    let project = file
        .parent()
        .and_then(Path::parent)
        .ok_or_else(|| "the project hooks file has no project folder".to_string())?;
    let paths = pin_paths_for(&hook.spec, workdir, Some(project))?;
    if !paths
        .iter()
        .map(|pin| (pin.arg, &pin.path, pin.copied))
        .eq(hook.pins.iter().map(|pin| (pin.arg, &pin.path, pin.copied)))
    {
        return Err("the file arguments no longer match the paths approved for this hook".into());
    }
    Ok(())
}

/// The SHA-256 of the regular file `path` (no link, at most
/// [`MAX_PINNED_BYTES`]), or why it cannot be had.
pub fn pinned_digest(path: &Path) -> Result<String, String> {
    pinned_bytes(path).map(|bytes| digest(&bytes))
}

/// The bytes of the regular file `path` (no link, at most
/// [`MAX_PINNED_BYTES`]), or why they cannot be had.
pub fn pinned_bytes(path: &Path) -> Result<Vec<u8>, String> {
    match read_capped(path, MAX_PINNED_BYTES) {
        Ok(Some(bytes)) => Ok(bytes),
        Ok(None) => Err("it does not exist".into()),
        Err(err) => Err(err.to_string()),
    }
}

/// The user's hooks of the state folder `home`: the settings file's, then
/// `hooks.toml`'s, each in file order. An error names the file and line.
pub fn user_hooks(home: &Path) -> Result<Vec<Hook>> {
    let mut hooks = Vec::new();
    let settings = home.join(crate::config_file::FILE_NAME);
    let file = crate::config_file::ConfigFile::load(&settings)?;
    if let Some((first_line, text)) = file.hooks() {
        for spec in config::parse(text, first_line)
            .with_context(|| format!("the hooks in {}", settings.display()))?
        {
            check_user_program(&spec, &settings)?;
            hooks.push(Hook {
                spec,
                origin: Origin::User(settings.clone()),
                pins: Vec::new(),
            });
        }
    }
    let path = home.join(FILE_NAME);
    if let Some(bytes) =
        read_file(&path).with_context(|| format!("cannot read {}", path.display()))?
    {
        let text = String::from_utf8(bytes)
            .ok()
            .with_context(|| format!("{} is not UTF-8 text", path.display()))?;
        for spec in
            config::parse(&text, 1).with_context(|| format!("the hooks in {}", path.display()))?
        {
            check_user_program(&spec, &path)?;
            hooks.push(Hook {
                spec,
                origin: Origin::User(path.clone()),
                pins: Vec::new(),
            });
        }
    }
    Ok(hooks)
}

/// A user hook runs in every project without a question, so its program
/// must not be a path relative to the project (`./scripts/x.sh` would run
/// whatever file the project opened at the time has there): an absolute
/// path or a name on PATH.
fn check_user_program(spec: &Spec, file: &Path) -> Result<()> {
    let program = &spec.command[0];
    if program.contains('/') && !Path::new(program).is_absolute() {
        anyhow::bail!(
            "the hooks in {}: hook at line {}: `command` starts with the relative path `{}`; \
             a user hook runs in every project, so it would run that project's file. Give an \
             absolute path (`~` is not expanded) or a program name on PATH",
            file.display(),
            spec.line,
            crate::prompt_text::escape_hidden(program)
        );
    }
    Ok(())
}

/// A project's hooks file as read now.
#[derive(Debug, Clone)]
pub struct ProjectFile {
    /// Its path, under the canonical top folder.
    pub path: PathBuf,
    /// The SHA-256 of its bytes, in hex: what a person trusts.
    pub digest: String,
    /// Its text, for the trust question.
    pub text: String,
    /// Its hooks, or why they cannot be read.
    pub hooks: Result<Vec<Spec>, String>,
    /// The pins of its hooks ([`pin_paths_for`]), in order; their digests are
    /// part of [`ProjectFile::digest`].
    pub pins: Vec<Vec<Pin>>,
}

impl ProjectFile {
    /// Its hooks, marked as the project's; none when it is broken.
    pub fn hooks(&self) -> Vec<Hook> {
        self.hooks
            .as_ref()
            .map(|specs| {
                specs
                    .iter()
                    .cloned()
                    .zip(
                        self.pins
                            .iter()
                            .cloned()
                            .chain(std::iter::repeat(Vec::new())),
                    )
                    .map(|(spec, pins)| Hook {
                        spec,
                        origin: Origin::Project(self.path.clone()),
                        pins,
                    })
                    .collect()
            })
            .unwrap_or_default()
    }
}

/// Where the hooks file of a project worked on in `cwd` would be: under
/// the folder the instruction files are read up to (the repository's top,
/// HOME, or `cwd` itself). `user_home` is HOME.
pub fn project_path(cwd: &Path, user_home: Option<&Path>) -> PathBuf {
    let start = std::fs::canonicalize(cwd).unwrap_or_else(|_| cwd.to_path_buf());
    let user_home =
        user_home.map(|home| std::fs::canonicalize(home).unwrap_or_else(|_| home.to_path_buf()));
    let levels = crate::context_files::levels(&start, user_home.as_deref());
    levels.last().cloned().unwrap_or(start).join(PROJECT_FILE)
}

/// The hooks file of the project worked on in `cwd`, if there is one.
/// `None` too when it is the user's own `hooks.toml` (the walk stopped at
/// HOME and the state folder is `~/.junees`). `Err` when it is there but
/// cannot be read (a link, not a regular file, too large).
pub fn project_file(
    cwd: &Path,
    user_home: Option<&Path>,
    junees_home: Option<&Path>,
) -> Result<Option<ProjectFile>, String> {
    project_file_in(cwd, cwd, user_home, junees_home)
}

/// [`project_file`] of the project in `project_dir` for a session working
/// in `workdir`: the files its hooks run are pinned there, and their
/// digests are part of what a person trusts, so a changed script asks
/// again.
pub fn project_file_in(
    project_dir: &Path,
    workdir: &Path,
    user_home: Option<&Path>,
    junees_home: Option<&Path>,
) -> Result<Option<ProjectFile>, String> {
    let path = project_path(project_dir, user_home);
    if junees_home.is_some_and(|home| is_user_hooks_file(&path, home)) {
        return Ok(None);
    }
    read_project_file(path, workdir)
}

fn is_user_hooks_file(path: &Path, home: &Path) -> bool {
    canonical(path) == canonical(&home.join(FILE_NAME))
}

fn read_project_file(path: PathBuf, workdir: &Path) -> Result<Option<ProjectFile>, String> {
    let bytes = match read_file(&path) {
        Ok(Some(bytes)) => bytes,
        Ok(None) => return Ok(None),
        Err(err) => return Err(format!("cannot read {}: {err}", path.display())),
    };
    let mut digest = digest(&bytes);
    // `<top>/.junees/hooks.toml`.
    let project = path
        .parent()
        .and_then(Path::parent)
        .ok_or_else(|| format!("{} has no project folder", path.display()))?;
    let mut pins = Vec::new();
    let (text, hooks) = match String::from_utf8(bytes) {
        Ok(text) => {
            let hooks = config::parse(&text, 1)
                .map_err(|err| format!("{err:#}"))
                .and_then(|specs| {
                    let checked = specs
                        .iter()
                        .map(|spec| {
                            let pins = pins_from(pin_paths_for(spec, workdir, Some(project))?);
                            for pin in &pins {
                                if let Err(err) = &pin.digest {
                                    let path = crate::prompt_text::escape_hidden(
                                        &pin.path.display().to_string(),
                                    );
                                    return Err(format!(
                                        "hook at line {}: cannot pin `{path}`: {err}",
                                        spec.line
                                    ));
                                }
                            }
                            Ok(pins)
                        })
                        .collect::<Result<Vec<_>, String>>()?;
                    pins = checked;
                    Ok(specs)
                });
            (text, hooks)
        }
        Err(_) => (String::new(), Err("it is not UTF-8 text".to_string())),
    };
    // Lines as before interpreter scripts were pinned, so a file trusted
    // then stays trusted unless one of its hooks runs such a script.
    if pins.iter().any(|pins| !pins.is_empty()) {
        let mut all = format!("{digest}\n");
        for pin in pins.iter().flatten() {
            if let Ok(state) = &pin.digest {
                all.push_str(&format!("{}\t{state}\n", pin.path.display()));
            }
        }
        digest = self::digest(all.as_bytes());
    }
    Ok(Some(ProjectFile {
        path,
        digest,
        text,
        hooks,
        pins,
    }))
}

/// The hooks a run finds for a project in `project_dir`: the user's, the
/// project's when trusted, and any project file awaiting approval.
#[derive(Debug, Default)]
pub struct Found {
    hooks: Vec<Hook>,
    untrusted: Option<ProjectFile>,
    requires_reapproval: bool,
    warnings: Vec<String>,
}

/// Executable hooks (if any) and warnings after resolving approval.
pub struct Loaded {
    pub hooks: Option<std::sync::Arc<Hooks>>,
    pub warnings: Vec<String>,
}

/// The hooks of a run of the state folder `home` on the project in
/// `project_dir`. Fails when a user hook or a previously trusted project
/// file cannot be validated: a policy hook must not silently drop out.
pub fn find(home: Option<&Path>, project_dir: &Path, workdir: &Path) -> Result<Found> {
    let mut found = Found::default();
    if let Some(home) = home {
        found.hooks = user_hooks(home)?;
    }
    let user_home = std::env::var_os("HOME").map(PathBuf::from);
    let path = project_path(project_dir, user_home.as_deref());
    let own = home.is_some_and(|home| is_user_hooks_file(&path, home));
    let file = if own {
        Ok(None)
    } else {
        read_project_file(path.clone(), workdir)
    };
    let error = match file {
        Ok(None) => None,
        Ok(Some(file)) => match &file.hooks {
            Err(err) => Some(err.clone()),
            Ok(_) => {
                let status = home.map_or(trust::Status::New, |home| trust::status(home, &file));
                if status == trust::Status::Current {
                    found.hooks.extend(file.hooks());
                } else {
                    found.requires_reapproval = status == trust::Status::Changed;
                    found.untrusted = Some(file);
                }
                return Ok(found);
            }
        },
        Err(err) => Some(err),
    };
    let name = crate::prompt_text::escape_hidden(&path.display().to_string());
    if home.is_some_and(|home| trust::was_trusted(home, &path)) {
        let err = error.as_deref().unwrap_or(if own {
            "the path now refers to the user's own hooks file"
        } else {
            "the file does not exist"
        });
        anyhow::bail!(
            "cannot validate the previously trusted project hooks file {name}: {err}; \
             fix it or explicitly remove its approval with `junees hooks untrust`"
        );
    }
    if let Some(err) = error {
        found.warnings.push(format!(
            "cannot validate the project hooks file {name}; its hooks do not run: {err}"
        ));
    }
    Ok(found)
}

impl Found {
    /// The project file to show to a person who can approve it.
    pub fn untrusted(&self) -> Option<&ProjectFile> {
        self.untrusted.as_ref()
    }

    pub fn requires_reapproval(&self) -> bool {
        self.requires_reapproval
    }

    /// Skip a never-approved project, but never drop a changed policy.
    fn skip_untrusted(&mut self) -> Result<()> {
        if let Some(file) = &self.untrusted {
            let path = crate::prompt_text::escape_hidden(&file.path.display().to_string());
            if self.requires_reapproval {
                anyhow::bail!(
                    "the previously trusted project hooks file {path} no longer matches its approval; \
                     review it and run `junees hooks trust` to approve it or \
                     `junees hooks untrust` to explicitly remove its approval"
                );
            }
            self.warnings.push(format!(
                "the project hooks file {path} is not trusted, so its hooks do not run; \
                 read it, then run `junees hooks trust` in the project"
            ));
            self.untrusted = None;
        }
        Ok(())
    }

    /// Add the approved hooks only after saving their approval succeeds.
    pub fn trust_untrusted(&mut self, home: &Path) -> Result<()> {
        if let Some(file) = &self.untrusted {
            trust::trust(home, file)?;
            self.hooks.extend(file.hooks());
            self.untrusted = None;
            self.requires_reapproval = false;
        }
        Ok(())
    }

    /// Build this session's hooks or fail if a changed policy still needs
    /// approval, including when the caller cannot ask or the user declines.
    pub fn build(
        mut self,
        workdir: &Path,
        tools: &crate::tools::ToolSettings,
        hide: run::Hide,
    ) -> Result<Loaded> {
        self.skip_untrusted()?;
        if self.hooks.is_empty() {
            return Ok(Loaded {
                hooks: None,
                warnings: self.warnings,
            });
        }
        let mut hooks = self.hooks;
        // A project's hooks were pinned when the file was read and trusted.
        for hook in &mut hooks {
            if matches!(hook.origin, Origin::User(_)) {
                hook.pins = user_pins(&hook.spec, workdir);
            }
        }
        let mut built = Hooks::new(
            hooks,
            workdir,
            &tools.hidden_env,
            crate::sandbox::Sandbox::for_tools(tools),
            hide,
        );
        if let Some(state) = tools.settings_file.as_ref().and_then(|file| file.folder()) {
            built = built.with_copies(state.join(COPIES_FOLDER));
        }
        Ok(Loaded {
            hooks: Some(std::sync::Arc::new(built)),
            warnings: self.warnings,
        })
    }
}

/// The question a person answers before a project's hooks file first
/// runs, whole: they approve what they read.
///
/// After the text comes what will run, parsed, right above the question:
/// a long file cannot push a hook out of sight with empty lines.
pub fn trust_question(file: &ProjectFile) -> String {
    let hooks = file.hooks();
    let mut parsed = format!("  훅 {}개:\n", hooks.len());
    for hook in &hooks {
        parsed.push_str(&format!("  - {}\n", cli::summary(hook)));
    }
    format!(
        "  이 프로젝트의 훅 파일을 처음 보거나, 승인한 뒤 내용이나 훅이 부르는 파일이 바뀌었습니다: {}\n  \
         승인하면 이 내용의 훅이 이 세션부터 실행됩니다(바뀌면 다시 묻습니다).\n{}{parsed}",
        file.path.display(),
        cli::quoted(&file.text)
    )
}

/// Hides the values of the secret settings and variables in what hooks
/// get ([`crate::doctor::hide_known_secrets`]).
pub fn hider() -> run::Hide {
    match crate::config_file::Settings::from_env() {
        Ok(settings) => crate::doctor::known_secrets_hider(&settings),
        Err(_) => crate::doctor::env_secrets_hider(),
    }
}

/// The SHA-256 of `bytes` in hex.
pub fn digest(bytes: &[u8]) -> String {
    ring::digest::digest(&ring::digest::SHA256, bytes)
        .as_ref()
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

/// The bytes of the regular file `path`, `None` when it does not exist.
/// A link (as the last part), a FIFO or a file over [`MAX_FILE_BYTES`] is
/// an error, never followed or waited on.
fn read_file(path: &Path) -> std::io::Result<Option<Vec<u8>>> {
    read_capped(path, MAX_FILE_BYTES)
}

/// [`read_file`] with the size limit `max`.
fn read_capped(path: &Path, max: u64) -> std::io::Result<Option<Vec<u8>>> {
    use std::os::unix::fs::OpenOptionsExt;
    let file = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(path);
    let file = match file {
        Ok(file) => file,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(err) if err.raw_os_error() == Some(libc::ELOOP) => {
            return Err(std::io::Error::other(
                "it is a link; it must be a regular file",
            ));
        }
        Err(err) => return Err(err),
    };
    let meta = file.metadata()?;
    if !meta.is_file() {
        return Err(std::io::Error::other("it is not a regular file"));
    }
    if meta.len() > max {
        return Err(std::io::Error::other(format!(
            "it is larger than {} KiB",
            max / 1024
        )));
    }
    let mut bytes = Vec::new();
    file.take(max + 1).read_to_end(&mut bytes)?;
    if bytes.len() as u64 > max {
        return Err(std::io::Error::other("it grew while being read"));
    }
    Ok(Some(bytes))
}
