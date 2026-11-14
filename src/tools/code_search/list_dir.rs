//! `list_dir`: shows a folder as a tree.

use std::fmt::Write as _;

use async_trait::async_trait;
use serde_json::{Value, json};

use super::{Search, Stop, Walk, bounded, opt_bool, opt_str, shown};
use crate::tools::{Tool, ToolOutput};

/// The most entries shown.
pub const MAX_ENTRIES: usize = 400;

/// Shows a folder's files and folders as a tree, with sizes.
#[derive(Debug, Clone)]
pub struct ListDir {
    search: Search,
}

impl ListDir {
    pub fn new(search: Search) -> Self {
        Self { search }
    }
}

fn list(
    search: &Search,
    path: Option<&str>,
    depth: usize,
    hidden: bool,
    stop: &Stop,
) -> Result<String, String> {
    let start = search.start(path)?;
    if !start.is_dir {
        return Err(format!(
            "{} is not a folder; read it with read_file",
            path.unwrap_or(".")
        ));
    }
    let mut out = format!("{}/\n", shown(&start.root).trim_end_matches('/'));
    let mut listed = 0;
    let mut more = 0;
    let walk = Walk {
        hidden,
        depth: Some(depth),
    };
    let ended = search.walk(&start, walk, stop, |entry| {
        if listed >= MAX_ENTRIES {
            more += 1;
            return true;
        }
        let indent = "  ".repeat(entry.depth());
        let name = entry.file_name().to_string_lossy();
        let kind = entry.file_type();
        let line = if kind.is_some_and(|kind| kind.is_dir()) {
            format!("{indent}{name}/")
        } else if kind.is_some_and(|kind| kind.is_symlink()) {
            // Not followed: what it points to is not checked.
            format!("{indent}{name}@ (link)")
        } else {
            let size = entry.metadata().map(|meta| meta.len()).unwrap_or(0);
            format!("{indent}{name} ({})", crate::images::human_bytes(size))
        };
        out.push_str(&crate::prompt_text::escape_hidden(&line));
        out.push('\n');
        listed += 1;
        true
    });
    if listed == 0 {
        out.push_str("  (empty, or every entry is ignored or hidden)\n");
    }
    if more > 0 {
        let _ = writeln!(
            out,
            "[{more} more entr{} not shown; list a subfolder]",
            if more == 1 { "y" } else { "ies" }
        );
    }
    if let Some(note) = ended.note(search.time_limit) {
        out.push_str(&note);
        out.push('\n');
    }
    Ok(out)
}

#[async_trait]
impl Tool for ListDir {
    fn name(&self) -> &str {
        "list_dir"
    }

    fn description(&self) -> &str {
        "List a folder as a tree (folders end with `/`, files show their size), up to 400 \
         entries, 1 to 3 levels deep (default 1). Follows .gitignore; skips .git, target, \
         node_modules and dot files (unless hidden). Use this instead of ls or tree in bash."
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "path": {"type": "string", "description": "Folder to list (default: the working folder)"},
                "depth": {"type": "integer", "minimum": 1, "maximum": 3, "description": "Levels to show (default 1)"},
                "hidden": {"type": "boolean", "description": "Also show dot files and folders"}
            }
        })
    }

    async fn run(&self, input: Value) -> ToolOutput {
        let parsed = (|| {
            Ok::<_, String>((
                opt_str(&input, "path")?.map(str::to_string),
                bounded(&input, "depth", 1..=3, 1)? as usize,
                opt_bool(&input, "hidden")?,
            ))
        })();
        let (path, depth, hidden) = match parsed {
            Ok(parsed) => parsed,
            Err(err) => return ToolOutput::error(err),
        };
        let rules = self.search.clone();
        let limit = self.search.time_limit;
        match super::run_blocking(limit, move |stop| {
            list(&rules, path.as_deref(), depth, hidden, stop)
        })
        .await
        {
            Ok(Ok(out)) => ToolOutput::ok(out),
            Ok(Err(err)) | Err(err) => ToolOutput::error(err),
        }
    }
}
