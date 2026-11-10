//! Keeps the model's tools away from the user's settings file
//! (`$JUNEES_HOME/config.toml`). It holds the API key, and writing it could
//! turn the command rules off for the next run (`allow_dangerous`). The MCP
//! server list next to it (`mcp.json`) gets the same protection: it may
//! hold tokens, and writing it would start programs in the next session.
//!
//! read_file, write_file and edit_file refuse it; the command policy refuses
//! bash commands that name it (also through a glob or braces) or that use
//! its whole folder other than to list it. The bash part is a deny list, not
//! a sandbox: paths built from other variables or at run time
//! (`cat $(echo ~/.junees)/config.toml`), commands on a folder above it
//! (`grep -r api_key ~`) and scripts written to a file first still reach it.
//!
//! The same checks keep writes out of the folders Junees changes only
//! through its own tools ([`DATA_FOLDERS`]: memory, project memory,
//! skills): write_file and edit_file refuse paths in them, and the command
//! policy refuses commands that name them unless the command only reads
//! ([`Named::Data`]). Reading them stays allowed.

use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};

use super::policy::resolve;

/// How many symbolic links a dangling chain may have before giving up.
const MAX_LINKS: usize = 40;

/// How many words brace expansion of one shell word may give before the
/// rest are not checked.
const MAX_BRACE_WORDS: usize = 256;

/// What a shell word points at.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Named {
    /// The settings file (or a glob that can match it).
    File,
    /// The folder holding it, `$JUNEES_HOME` itself.
    Folder,
    /// One of the [`DATA_FOLDERS`] (by index) or a path in it.
    Data(usize),
}

/// A folder under `$JUNEES_HOME` that Junees changes only through one of
/// its tools, which check what is written (task sessions change only
/// project memory, no secrets, no hidden characters). The model may read
/// it, but write_file, edit_file and bash may not change it.
#[derive(Debug, PartialEq, Eq)]
pub struct DataFolder {
    /// The folder name under `$JUNEES_HOME`.
    pub name: &'static str,
    /// What it holds, for messages.
    pub holds: &'static str,
    /// The tool that changes it, or the `junees` command the user changes
    /// it with (a name with a space: no tool of the model does).
    pub tool: &'static str,
}

/// The folders under `$JUNEES_HOME` the model may read but not write.
pub const DATA_FOLDERS: &[DataFolder] = &[
    DataFolder {
        name: "memory",
        holds: "the saved memory",
        tool: "memory",
    },
    DataFolder {
        name: "projects",
        holds: "the saved project memory",
        tool: "memory",
    },
    DataFolder {
        name: "skills",
        holds: "the saved skills",
        tool: "skill_manage",
    },
    DataFolder {
        name: "plugins",
        holds: "the installed plugins",
        tool: "junees plugin",
    },
];

impl DataFolder {
    /// The error write_file and edit_file return for `path`.
    pub fn refusal(&self, path: &str) -> String {
        format!(
            "{path} is in `$JUNEES_HOME/{}/`, {} of Junees. The file tools may read it \
             but not change it; {}.",
            self.name,
            self.holds,
            self.instead()
        )
    }

    /// Why the command policy refuses a command that may write to `word`.
    pub fn reason(&self, word: &str) -> String {
        format!(
            "`{word}` is in `$JUNEES_HOME/{}/`, {} of Junees. Commands may read it (cat, \
             grep, ls and the like) but not change, move or copy it; {}.",
            self.name,
            self.holds,
            self.instead()
        )
    }

    fn instead(&self) -> String {
        if self.tool.contains(' ') {
            format!("only the user changes it, with `{}`", self.tool)
        } else {
            format!("use the `{}` tool instead", self.tool)
        }
    }
}

/// The settings file the tools must leave alone. The rest of
/// `$JUNEES_HOME` (sessions, journals) stays readable.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SettingsFile {
    path: PathBuf,
    /// What `~` and `$HOME` mean in bash commands.
    home: Option<PathBuf>,
}

impl SettingsFile {
    /// `path` is the settings file ([`crate::config_file::path`]), `home`
    /// the `HOME` bash commands see.
    pub fn new(path: impl Into<PathBuf>, home: Option<PathBuf>) -> Self {
        Self {
            path: path.into(),
            home,
        }
    }

    /// The file as an absolute path (relative ones start from the current
    /// folder), `.`, `..` and symbolic links resolved like the kernel does.
    pub fn resolved_path(&self) -> PathBuf {
        let path = std::path::absolute(&self.path).unwrap_or_else(|_| self.path.clone());
        resolve(&path).unwrap_or(path)
    }

    /// What `~` and `$HOME` mean in bash commands.
    pub fn home(&self) -> Option<&Path> {
        self.home.as_deref()
    }

    /// Whether the file tools must refuse `path` (relative paths start from
    /// the current folder): it is the settings file once `.`, `..` and
    /// symbolic links are resolved, a dangling link to where it would be,
    /// or a hard link to it.
    pub fn is(&self, path: &Path) -> bool {
        if path.is_absolute() {
            return self.same(path);
        }
        std::env::current_dir().is_ok_and(|cwd| self.same(&cwd.join(path)))
    }

    /// The [`DATA_FOLDERS`] entry `path` is in (relative paths start from
    /// the current folder), once `.`, `..` and symbolic links are resolved
    /// and dangling links followed to where writing would create the file.
    pub fn data_folder(&self, path: &Path) -> Option<&'static DataFolder> {
        let path = if path.is_absolute() {
            path.to_path_buf()
        } else {
            std::env::current_dir().ok()?.join(path)
        };
        self.data_index(&self.lands(&path)?)
            .map(|index| &DATA_FOLDERS[index])
    }

    /// Whether the shell word `word` names the settings file, its folder
    /// or a data folder (in that order of precedence), relative paths taken from `cwd` (`None`: unknown,
    /// not checked). `~`, `$HOME`, `${HOME}`, `$JUNEES_HOME` and
    /// `${JUNEES_HOME}` at the start and `/proc/self/cwd` are expanded, so
    /// are braces (`{a,b}`); a glob (`*`, `?`, `[...]`) counts when it can
    /// match. The part after the first `=` counts too (`--file=PATH`,
    /// `if=PATH`). Other variables are not expanded.
    pub fn names(&self, word: &str, cwd: Option<&Path>) -> Option<Named> {
        let mut words = Vec::new();
        for word in std::iter::once(word).chain(word.split_once('=').map(|(_, value)| value)) {
            expand_braces(word, &mut words);
        }
        words
            .iter()
            .filter_map(|word| self.locate(word, cwd))
            .filter_map(|path| self.points_at(&path))
            .min()
    }

    /// Where `cd args` (or `pushd args`) from `cwd` goes, `None` when that
    /// is not known here.
    pub fn cd(&self, args: &[String], cwd: Option<&Path>) -> Option<PathBuf> {
        let mut args = args
            .iter()
            .map(String::as_str)
            .skip_while(|arg| arg.starts_with('-') && arg.len() > 1 && *arg != "--");
        let target = match args.next() {
            Some("--") => args.next(),
            target => target,
        };
        match target {
            None => self.home.clone(),
            Some("-") => None,
            Some(target) => self.locate(target, cwd),
        }
    }

    /// The error a file tool returns for `path`.
    pub fn refusal(&self, path: &str) -> String {
        format!(
            "{path} is the user's private Junees settings file (it holds the API key and \
             other secrets). Junees tools do not read or change it; ask the user to change settings \
             with `junees setup` instead."
        )
    }

    /// Why the command policy refuses a command that names it as `word`.
    pub fn named_reason(&self, word: &str) -> String {
        format!(
            "`{word}` is the user's private Junees settings file (it holds the API key and \
             other secrets); commands may not read or change it. Ask the user to change settings with \
             `junees setup` instead."
        )
    }

    /// Why the command policy refuses a command that uses the whole folder
    /// as `word`.
    pub fn folder_reason(&self, word: &str) -> String {
        format!(
            "`{word}` is the folder of the user's private Junees settings file (it holds \
             the API key); commands may list it or `cd` into it but not read, copy, move \
             or change it as a whole. Use the paths inside it (such as `sessions/`) instead."
        )
    }

    /// Whether the absolute `path` (possibly holding glob patterns) leads
    /// to the settings file, its folder or into a data folder.
    fn points_at(&self, path: &Path) -> Option<Named> {
        let folder = self.path.parent()?;
        let Some(first_glob) = path
            .components()
            .position(|c| is_glob(&c.as_os_str().to_string_lossy()))
        else {
            if self.same(path) {
                return Some(Named::File);
            }
            let target = resolve(folder).unwrap_or_else(|| folder.to_path_buf());
            if resolve(path) == Some(target) {
                return Some(Named::Folder);
            }
            return self.data_index(&self.lands(path)?).map(Named::Data);
        };
        let prefix: PathBuf = path.components().take(first_glob).collect();
        let prefix = resolve(&prefix)?;
        let patterns: Vec<_> = path.components().skip(first_glob).collect();
        // Whether the patterns match the components of `target` below the
        // prefix: all of them (`whole`), or the first ones (`target` is a
        // folder the path may lead into).
        let matches = |target: &Path, whole: bool| {
            let Ok(rest) = target.strip_prefix(&prefix) else {
                return false;
            };
            let rest: Vec<_> = rest.components().collect();
            (if whole {
                rest.len() == patterns.len()
            } else {
                rest.len() <= patterns.len()
            }) && rest.iter().zip(&patterns).all(|(name, pattern)| {
                glob_match(
                    &pattern.as_os_str().to_string_lossy(),
                    &name.as_os_str().to_string_lossy(),
                )
            })
        };
        let mcp = self.mcp_path();
        [
            (&*self.path, Named::File),
            (&*mcp, Named::File),
            (folder, Named::Folder),
        ]
        .into_iter()
        .find_map(|(target, named)| {
            let target = resolve(target).unwrap_or_else(|| target.to_path_buf());
            matches(&target, true).then_some(named)
        })
        .or_else(|| {
            (0..DATA_FOLDERS.len())
                .find(|&index| {
                    self.data_dir(index)
                        .is_some_and(|dir| prefix.starts_with(&dir) || matches(&dir, false))
                })
                .map(Named::Data)
        })
    }

    /// The data folder `index` of [`DATA_FOLDERS`], absolute and resolved.
    fn data_dir(&self, index: usize) -> Option<PathBuf> {
        let dir = std::path::absolute(self.path.parent()?.join(DATA_FOLDERS[index].name)).ok()?;
        Some(resolve(&dir).unwrap_or(dir))
    }

    /// The index in [`DATA_FOLDERS`] of the folder the resolved absolute
    /// `path` is in (or is).
    fn data_index(&self, path: &Path) -> Option<usize> {
        (0..DATA_FOLDERS.len()).find(|&index| {
            self.data_dir(index)
                .is_some_and(|dir| path.starts_with(dir))
        })
    }

    /// Where writing to the absolute `path` lands: `.`, `..` and symbolic
    /// links resolved, a dangling link followed to where it points.
    fn lands(&self, path: &Path) -> Option<PathBuf> {
        let mut path = path.to_path_buf();
        for _ in 0..MAX_LINKS {
            match resolve(&path) {
                Some(resolved) => return Some(resolved),
                None => match (std::fs::read_link(&path), path.parent()) {
                    (Ok(link), Some(parent)) => path = parent.join(link),
                    _ => return None,
                },
            }
        }
        None
    }

    /// The absolute path `word` names from `cwd`, without resolving it.
    fn locate(&self, word: &str, cwd: Option<&Path>) -> Option<PathBuf> {
        let junees_home = self.path.parent();
        let home = self.home.as_deref();
        let expansions = [
            ("~", home),
            ("$HOME", home),
            ("${HOME}", home),
            ("$JUNEES_HOME", junees_home),
            ("${JUNEES_HOME}", junees_home),
        ];
        for (var, base) in expansions {
            if let Some(rest) = word.strip_prefix(var)
                && (rest.is_empty() || rest.starts_with('/'))
            {
                return Some(base?.join(rest.trim_start_matches('/')));
            }
        }
        // `/proc/self` is the shell there, not Junees.
        for var in ["/proc/self/cwd", "/proc/thread-self/cwd"] {
            if let Some(rest) = word.strip_prefix(var)
                && (rest.is_empty() || rest.starts_with('/'))
            {
                return Some(cwd?.join(rest.trim_start_matches('/')));
            }
        }
        if word.is_empty() || word.starts_with('~') || word.contains(['$', '`']) {
            None
        } else if word.starts_with('/') {
            Some(PathBuf::from(word))
        } else {
            Some(cwd?.join(word))
        }
    }

    /// The MCP server list next to the settings file.
    fn mcp_path(&self) -> PathBuf {
        self.path.with_file_name(crate::mcp::config::FILE_NAME)
    }

    /// Whether the absolute `path` leads to the settings file or the MCP
    /// server list.
    fn same(&self, path: &Path) -> bool {
        self.same_as(path, &self.path) || self.same_as(path, &self.mcp_path())
    }

    /// Whether the absolute `path` leads to the file `file`.
    fn same_as(&self, path: &Path, file: &Path) -> bool {
        let target = resolve(file).unwrap_or_else(|| file.to_path_buf());
        let mut path = path.to_path_buf();
        for _ in 0..MAX_LINKS {
            match resolve(&path) {
                Some(resolved) if resolved == target => return true,
                Some(_) => break,
                // A dangling link: writing through it would create the
                // file where it points.
                None => match (std::fs::read_link(&path), path.parent()) {
                    (Ok(link), Some(parent)) => path = parent.join(link),
                    _ => break,
                },
            }
        }
        match (std::fs::metadata(&path), std::fs::metadata(file)) {
            (Ok(a), Ok(b)) => a.dev() == b.dev() && a.ino() == b.ino(),
            _ => false,
        }
    }
}

/// Whether a path component holds a glob pattern.
fn is_glob(name: &str) -> bool {
    name.contains(['*', '?', '['])
}

/// One part of a glob pattern.
enum Glob {
    /// `*`: any run of characters.
    Star,
    /// `?`: any one character.
    Any,
    /// `[...]`: one character of the set (or not of it).
    Set {
        negate: bool,
        items: Vec<char>,
    },
    Literal(char),
}

impl Glob {
    /// Whether this part (not `*`) matches the one character `c`.
    fn matches(&self, c: char) -> bool {
        match self {
            Glob::Star | Glob::Any => true,
            Glob::Literal(literal) => *literal == c,
            Glob::Set { negate, items } => {
                let mut hit = false;
                let mut i = 0;
                while i < items.len() {
                    if i + 2 < items.len() && items[i + 1] == '-' {
                        hit |= (items[i]..=items[i + 2]).contains(&c);
                        i += 3;
                    } else {
                        hit |= items[i] == c;
                        i += 1;
                    }
                }
                hit != *negate
            }
        }
    }
}

/// Whether the glob `pattern` (`*`, `?`, `[...]`) matches `name`. A `*`
/// also matches a leading `.` (as with `dotglob`), to err on the safe side.
/// Runs in time `pattern` times `name`, however many `*` there are.
fn glob_match(pattern: &str, name: &str) -> bool {
    let pattern: Vec<char> = pattern.chars().collect();
    let mut parts = Vec::new();
    let mut i = 0;
    while let Some(&c) = pattern.get(i) {
        i += 1;
        parts.push(match c {
            '*' => Glob::Star,
            '?' => Glob::Any,
            '[' => {
                let negate = matches!(pattern.get(i), Some('!' | '^'));
                let body = i + usize::from(negate);
                // A `]` right after the opening is part of the set.
                match pattern.iter().skip(body + 1).position(|&c| c == ']') {
                    Some(end) => {
                        let end = body + 1 + end;
                        i = end + 1;
                        Glob::Set {
                            negate,
                            items: pattern[body..end].to_vec(),
                        }
                    }
                    // No closing `]`: a plain `[`.
                    None => Glob::Literal('['),
                }
            }
            c => Glob::Literal(c),
        });
    }
    let name: Vec<char> = name.chars().collect();
    // `row[j]`: whether the parts so far match the first `j` characters.
    let mut row = vec![false; name.len() + 1];
    row[0] = true;
    for part in &parts {
        let mut next = vec![false; name.len() + 1];
        for j in 0..=name.len() {
            next[j] = match part {
                Glob::Star => row[j] || (j > 0 && next[j - 1]),
                part => j > 0 && row[j - 1] && part.matches(name[j - 1]),
            };
        }
        row = next;
    }
    row[name.len()]
}

/// Appends the words bash brace expansion makes of `word` (`a{b,c}d` gives
/// `abd` and `acd`) to `out`, at most [`MAX_BRACE_WORDS`] of them. Braces
/// without a top-level comma (`${HOME}`) stay as they are.
fn expand_braces(word: &str, out: &mut Vec<String>) {
    if out.len() >= MAX_BRACE_WORDS {
        return;
    }
    let mut search = 0;
    while let Some(open) = word[search..].find('{').map(|i| i + search) {
        let mut depth = 0;
        let mut commas = Vec::new();
        let mut close = None;
        for (i, c) in word[open..].char_indices() {
            match c {
                '{' => depth += 1,
                '}' => {
                    depth -= 1;
                    if depth == 0 {
                        close = Some(open + i);
                        break;
                    }
                }
                ',' if depth == 1 => commas.push(open + i),
                _ => {}
            }
        }
        match close {
            Some(close) if !commas.is_empty() => {
                let (head, tail) = (&word[..open], &word[close + 1..]);
                let mut start = open + 1;
                for end in commas.into_iter().chain([close]) {
                    expand_braces(&format!("{head}{}{tail}", &word[start..end]), out);
                    start = end + 1;
                }
                return;
            }
            _ => search = open + 1,
        }
    }
    out.push(word.to_string());
}

/// Fixtures for the tests of the tools and the command policy.
#[cfg(test)]
pub(crate) mod testing {
    use super::*;

    /// A home folder (the second value, canonical) with
    /// `.junees/config.toml` holding the key `secret-key`, a session file
    /// `.junees/sessions/s.jsonl`, a memory file `.junees/memory/user.md`
    /// holding `Name: J`, and the [`SettingsFile`] for it.
    pub fn home() -> (tempfile::TempDir, PathBuf, SettingsFile) {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        std::fs::create_dir_all(root.join(".junees/sessions")).unwrap();
        std::fs::create_dir_all(root.join(".junees/memory")).unwrap();
        std::fs::write(root.join(".junees/memory/user.md"), "Name: J\n").unwrap();
        std::fs::write(
            root.join(".junees/config.toml"),
            "api_key = \"secret-key\"\n",
        )
        .unwrap();
        std::fs::write(root.join(".junees/sessions/s.jsonl"), "{}\n").unwrap();
        let file = SettingsFile::new(root.join(".junees/config.toml"), Some(root.clone()));
        (dir, root, file)
    }

    /// The absolute `path` as a path relative to the current folder,
    /// climbing to `/` with `..` first.
    pub fn relative(path: &Path) -> PathBuf {
        let cwd = std::env::current_dir().unwrap();
        let mut relative = PathBuf::new();
        for _ in 1..cwd.components().count() {
            relative.push("..");
        }
        relative.join(path.strip_prefix("/").unwrap())
    }
}

#[cfg(test)]
mod tests {
    use std::os::unix::fs::symlink;

    use super::testing::{home, relative};
    use super::{Named, expand_braces};

    #[test]
    fn knows_the_file_by_any_path() {
        let (_dir, root, file) = home();
        symlink(root.join(".junees/config.toml"), root.join("link")).unwrap();
        symlink(root.join(".junees"), root.join("dirlink")).unwrap();
        std::fs::hard_link(root.join(".junees/config.toml"), root.join("hard")).unwrap();
        for path in [
            ".junees/config.toml",
            ".junees/sessions/../config.toml",
            ".junees/./config.toml",
            "link",
            "dirlink/config.toml",
            "dirlink/sessions/../config.toml",
            "hard",
        ] {
            assert!(file.is(&root.join(path)), "{path}");
        }
        assert!(file.is(&relative(&root.join(".junees/config.toml"))));
        for path in [".junees/sessions/s.jsonl", ".junees", "config.toml", "x"] {
            assert!(!file.is(&root.join(path)), "{path}");
        }
    }

    #[test]
    fn knows_a_dangling_link_to_a_missing_file() {
        let (_dir, root, file) = home();
        std::fs::remove_file(root.join(".junees/config.toml")).unwrap();
        symlink(root.join(".junees/config.toml"), root.join("link")).unwrap();
        symlink("link", root.join("link2")).unwrap();
        assert!(file.is(&root.join(".junees/config.toml")));
        assert!(file.is(&root.join("link")));
        assert!(file.is(&root.join("link2")));
        symlink(root.join("nowhere"), root.join("other")).unwrap();
        assert!(!file.is(&root.join("other")));
    }

    #[test]
    fn expands_home_variables_in_shell_words() {
        let (_dir, root, file) = home();
        let junees = root.join(".junees");
        for word in [
            "~/.junees/config.toml",
            "$HOME/.junees/config.toml",
            "${HOME}/.junees//config.toml",
            "$JUNEES_HOME/config.toml",
            "${JUNEES_HOME}/config.toml",
            "--config=~/.junees/config.toml",
            "if=$JUNEES_HOME/config.toml",
            "config.toml",
            "sessions/../config.toml",
            "/proc/self/cwd/config.toml",
            // Globs and braces that can reach it.
            "~/.junees/*",
            "~/.junees/conf*.toml",
            "$JUNEES_HOME/c?nfig.[st]oml",
            "~/.jun*/config.toml",
            "~/*/config.toml",
            "~/.junees/{config.toml,x}",
            "~/.junees/{con{f,x}ig,y}.toml",
        ] {
            assert_eq!(file.names(word, Some(&junees)), Some(Named::File), "{word}");
        }
        for word in [
            "~/.junees",
            "$JUNEES_HOME/",
            "${HOME}/.junees/.",
            "~/.junees/sessions/..",
            ".",
            "/proc/self/cwd",
            "--dir=~/.junees",
            "~/.jun*",
            "~/.junees{,}",
        ] {
            assert_eq!(
                file.names(word, Some(&junees)),
                Some(Named::Folder),
                "{word}"
            );
        }
        for word in [
            "~/.junees/sessions/s.jsonl",
            "~/.junees/sessions/*",
            "~/.junees/x*",
            "~/.junees/[!c]onfig.toml",
            "~/.junees/{x,y}",
            "~",
            "$JUNEES_HOMEX/config.toml",
            "$DIR/config.toml",
            "~other/.junees/config.toml",
            "`pwd`/config.toml",
            "cat",
            "",
        ] {
            assert_eq!(file.names(word, Some(&junees)), None, "{word}");
        }
        // A relative word needs a known folder.
        assert_eq!(file.names("config.toml", None), None);
        assert_eq!(file.names("/proc/self/cwd/config.toml", None), None);
    }

    #[test]
    fn matches_globs_like_bash() {
        use super::glob_match;
        for (pattern, name) in [
            ("*", "config.toml"),
            ("*", ".junees"),
            ("c*g.t*l", "config.toml"),
            ("c?nfig.[st]oml", "config.toml"),
            ("[]c]onfig", "config"),
            ("[a-d]onfig", "config"),
            ("[!x]onfig", "config"),
            ("[c", "[c"),
            ("**", ""),
        ] {
            assert!(glob_match(pattern, name), "{pattern} {name}");
        }
        for (pattern, name) in [
            ("[!c]onfig", "config"),
            ("?", ""),
            ("config", "config.toml"),
            ("[c", "c"),
        ] {
            assert!(!glob_match(pattern, name), "{pattern} {name}");
        }
        // Many stars must not take exponential time.
        assert!(!glob_match(&"*a".repeat(5000), &"a".repeat(200)));
    }

    #[test]
    fn expands_braces_like_bash() {
        let expand = |word: &str| {
            let mut out = Vec::new();
            expand_braces(word, &mut out);
            out
        };
        assert_eq!(expand("a{b,c}d"), ["abd", "acd"]);
        assert_eq!(expand("{a,b{c,d}}"), ["a", "bc", "bd"]);
        assert_eq!(expand("${HOME}/{x,y}"), ["${HOME}/x", "${HOME}/y"]);
        assert_eq!(expand("{a}b"), ["{a}b"]);
        assert_eq!(expand("{a,b"), ["{a,b"]);
        assert_eq!(expand(&"{a,b}".repeat(20)).len(), super::MAX_BRACE_WORDS);
    }

    #[test]
    fn knows_the_data_folders_by_any_path() {
        let (_dir, root, file) = home();
        let junees = root.join(".junees");
        symlink(junees.join("memory/user.md"), root.join("link")).unwrap();
        symlink(junees.join("memory"), root.join("dirlink")).unwrap();
        symlink(junees.join("skills/new/SKILL.md"), root.join("dangling")).unwrap();
        let data = |path: &str| file.data_folder(&root.join(path)).map(|folder| folder.name);
        for (path, name) in [
            (".junees/memory/user.md", "memory"),
            (".junees/memory", "memory"),
            (".junees/memory/new.md", "memory"),
            (".junees/sessions/../memory/./user.md", "memory"),
            (".junees/projects/0123/notes.md", "projects"),
            (".junees/skills/new/SKILL.md", "skills"),
            ("link", "memory"),
            ("dirlink/notes.md", "memory"),
            ("dangling", "skills"),
        ] {
            assert_eq!(data(path), Some(name), "{path}");
        }
        assert_eq!(
            file.data_folder(&relative(&junees.join("memory/user.md")))
                .map(|folder| folder.tool),
            Some("memory")
        );
        for path in [
            ".junees/sessions/s.jsonl",
            ".junees/memoryx",
            ".junees/config.toml",
            ".junees",
            "memory/user.md",
        ] {
            assert_eq!(data(path), None, "{path}");
        }

        for (word, index) in [
            ("~/.junees/memory/user.md", 0),
            ("$HOME/.junees/memory", 0),
            ("${JUNEES_HOME}/memory/", 0),
            ("$JUNEES_HOME/projects/x/notes.md", 1),
            ("~/.junees/skills/a/SKILL.md", 2),
            ("memory/user.md", 0),
            ("sessions/../memory/notes.md", 0),
            ("/proc/self/cwd/memory", 0),
            ("--file=~/.junees/memory/user.md", 0),
            ("~/dirlink/user.md", 0),
            // Globs and braces that can lead into one.
            ("~/.junees/memory/*", 0),
            ("~/.junees/mem*", 0),
            ("~/.junees/m*/user.md", 0),
            ("~/*/memory/x", 0),
            ("~/.junees/{sessions,projects}/x", 1),
            ("~/.junees/s*", 2),
        ] {
            assert_eq!(
                file.names(word, Some(&junees)),
                Some(Named::Data(index)),
                "{word}"
            );
        }
        // The settings file and its folder come first.
        assert_eq!(file.names("~/.junees/*", Some(&junees)), Some(Named::File));
        assert_eq!(
            file.names("~/.junees/memory/..", Some(&junees)),
            Some(Named::Folder)
        );
        for word in [
            "~/.junees/sessions/s.jsonl",
            "~/.junees/memoryx",
            "~/.junees/se*",
            "~/memory/user.md",
            "$DIR/memory",
        ] {
            assert_eq!(file.names(word, Some(&junees)), None, "{word}");
        }
    }

    #[test]
    fn follows_cd_targets() {
        let (_dir, root, file) = home();
        let args = |args: &[&str]| args.iter().map(|arg| arg.to_string()).collect::<Vec<_>>();
        assert_eq!(file.cd(&args(&[]), None), Some(root.clone()));
        assert_eq!(
            file.cd(&args(&["-P", "--", "~/.junees"]), None),
            Some(root.join(".junees"))
        );
        assert_eq!(
            file.cd(&args(&[".junees"]), Some(&root)),
            Some(root.join(".junees"))
        );
        assert_eq!(file.cd(&args(&["-"]), Some(&root)), None);
        assert_eq!(file.cd(&args(&["$DIR"]), Some(&root)), None);
    }
}
