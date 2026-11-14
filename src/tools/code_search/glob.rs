//! `glob`: finds files by name pattern.

use std::fmt::Write as _;
use std::path::PathBuf;
use std::time::SystemTime;

use async_trait::async_trait;
use serde_json::{Value, json};

use super::{Search, Stop, Walk, opt_bool, opt_str, shown};
use crate::tools::{Tool, ToolOutput};

/// The most files listed.
pub const MAX_FILES: usize = 500;

/// Finds files whose path matches a glob pattern.
#[derive(Debug, Clone)]
pub struct Glob {
    search: Search,
}

impl Glob {
    pub fn new(search: Search) -> Self {
        Self { search }
    }
}

/// The matcher for `pattern`, matched against paths relative to where the
/// search starts; the error for the model.
fn matcher(pattern: &str) -> Result<globset::GlobMatcher, String> {
    if pattern.is_empty() {
        return Err("the pattern is empty".into());
    }
    if pattern.starts_with('/') || pattern.split('/').any(|part| part == "..") {
        return Err(format!(
            "`{pattern}` leaves the folder searched: the pattern matches paths under `path` \
             (the working folder by default); give the folder in `path` instead"
        ));
    }
    Ok(globset::GlobBuilder::new(pattern)
        .literal_separator(true)
        .build()
        .map_err(|err| format!("invalid glob `{pattern}`: {err}"))?
        .compile_matcher())
}

fn search(
    search: &Search,
    pattern: &globset::GlobMatcher,
    path: Option<&str>,
    hidden: bool,
    stop: &Stop,
) -> Result<String, String> {
    let start = search.start(path)?;
    if !start.is_dir {
        return Err(format!("{} is not a folder", path.unwrap_or(".")));
    }
    let mut found: Vec<(SystemTime, PathBuf)> = Vec::new();
    let ended = search.walk(
        &start,
        Walk {
            hidden,
            depth: None,
        },
        stop,
        |entry| {
            // Links are not followed, nor listed.
            if entry.file_type().is_some_and(|kind| kind.is_file())
                && let Ok(relative) = entry.path().strip_prefix(&start.root)
                && pattern.is_match(relative)
            {
                found.push((super::modified(entry), entry.path().to_path_buf()));
            }
            true
        },
    );
    found.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| a.1.cmp(&b.1)));
    let mut out = String::new();
    for (_, path) in found.iter().take(MAX_FILES) {
        out.push_str(&shown(path));
        out.push('\n');
    }
    if found.is_empty() {
        let _ = writeln!(
            out,
            "No files match `{}` in {}.",
            pattern.glob().glob(),
            shown(&start.root)
        );
    } else if found.len() > MAX_FILES {
        let _ = writeln!(
            out,
            "[{} more file(s) not shown; use a narrower pattern or path]",
            found.len() - MAX_FILES
        );
    }
    if let Some(note) = ended.note(search.time_limit) {
        out.push_str(&note);
        out.push('\n');
    }
    Ok(out)
}

#[async_trait]
impl Tool for Glob {
    fn name(&self) -> &str {
        "glob"
    }

    fn description(&self) -> &str {
        "Find files by path pattern, e.g. `**/*.rs` or `src/**/test_*.py` (`*` stays within \
         a folder, `**` crosses folders), matched against paths under `path`. Returns up to \
         500 files, newest first. Follows .gitignore; skips .git, target, node_modules and \
         dot files (unless hidden). Use this instead of find or ls in bash."
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "pattern": {"type": "string", "description": "Glob pattern, relative to path"},
                "path": {"type": "string", "description": "Folder to search (default: the working folder)"},
                "hidden": {"type": "boolean", "description": "Also match dot files and folders"}
            },
            "required": ["pattern"]
        })
    }

    async fn run(&self, input: Value) -> ToolOutput {
        let parsed = (|| {
            let pattern = input
                .get("pattern")
                .and_then(Value::as_str)
                .ok_or("missing required string argument `pattern`")?;
            Ok::<_, String>((
                matcher(pattern)?,
                opt_str(&input, "path")?.map(str::to_string),
                opt_bool(&input, "hidden")?,
            ))
        })();
        let (pattern, path, hidden) = match parsed {
            Ok(parsed) => parsed,
            Err(err) => return ToolOutput::error(err),
        };
        let rules = self.search.clone();
        let limit = self.search.time_limit;
        match super::run_blocking(limit, move |stop| {
            search(&rules, &pattern, path.as_deref(), hidden, stop)
        })
        .await
        {
            Ok(Ok(out)) => ToolOutput::ok(out),
            Ok(Err(err)) | Err(err) => ToolOutput::error(err),
        }
    }
}
