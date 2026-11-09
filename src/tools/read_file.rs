use std::fmt::Write as _;

use async_trait::async_trait;
use serde_json::{Value, json};

use std::path::Path;

use super::{
    MAX_FILE_BYTES, SettingsFile, Tool, ToolOutput, ToolSettings, opt_u64_arg, str_arg,
    truncate_middle,
};

const DEFAULT_LIMIT: u64 = 2000;
const MAX_OUTPUT_CHARS: usize = 50_000;

/// Reads a text file and returns it with line numbers.
#[derive(Default)]
pub struct ReadFile {
    settings_file: Option<SettingsFile>,
}

impl ReadFile {
    /// Refuses the settings file of `settings`.
    pub fn with_settings(settings: &ToolSettings) -> Self {
        Self {
            settings_file: settings.settings_file.clone(),
        }
    }
}

#[async_trait]
impl Tool for ReadFile {
    fn name(&self) -> &str {
        "read_file"
    }

    fn description(&self) -> &str {
        "Read a text file. Lines come back numbered as `LINE<TAB>CONTENT`. \
         Use offset and limit to page through long files."
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "path": {"type": "string", "description": "File path, absolute or relative to the working directory"},
                "offset": {"type": "integer", "description": "First line to read, 1-based (default 1)"},
                "limit": {"type": "integer", "description": "Maximum number of lines (default 2000)"}
            },
            "required": ["path"]
        })
    }

    async fn run(&self, input: Value) -> ToolOutput {
        let path = match str_arg(&input, "path") {
            Ok(path) => path,
            Err(err) => return err,
        };
        let offset = match opt_u64_arg(&input, "offset") {
            Ok(offset) => offset.unwrap_or(1).max(1) as usize,
            Err(err) => return err,
        };
        let limit = match opt_u64_arg(&input, "limit") {
            Ok(limit) => limit.unwrap_or(DEFAULT_LIMIT).max(1) as usize,
            Err(err) => return err,
        };
        if let Some(file) = &self.settings_file
            && file.is(Path::new(path))
        {
            return ToolOutput::error(file.refusal(path));
        }

        match tokio::fs::metadata(path).await {
            Ok(meta) if !meta.is_file() => {
                return ToolOutput::error(format!("{path} is not a regular file"));
            }
            Ok(meta) if meta.len() > MAX_FILE_BYTES => {
                return ToolOutput::error(format!(
                    "{path} is {} bytes; read_file reads files up to {} MB. \
                     Use bash (head, tail, sed -n, grep) to look at parts of it.",
                    meta.len(),
                    MAX_FILE_BYTES / (1024 * 1024)
                ));
            }
            Ok(_) => {}
            Err(err) => return ToolOutput::error(format!("cannot read {path}: {err}")),
        }
        let bytes = match tokio::fs::read(path).await {
            Ok(bytes) => bytes,
            Err(err) => return ToolOutput::error(format!("cannot read {path}: {err}")),
        };
        if bytes.contains(&0) {
            return ToolOutput::error(format!("{path} looks like a binary file"));
        }

        let text = String::from_utf8_lossy(&bytes);
        let lines: Vec<&str> = text.lines().collect();
        let total = lines.len();
        if total == 0 {
            return ToolOutput::ok(format!("{path} is empty"));
        }
        let start = offset - 1;
        if start >= total {
            return ToolOutput::error(format!(
                "offset {offset} is past the end of {path} ({total} lines)"
            ));
        }
        let end = start.saturating_add(limit).min(total);

        let mut out = String::new();
        for (index, line) in lines[start..end].iter().enumerate() {
            let _ = writeln!(out, "{:>6}\t{line}", start + index + 1);
        }
        if end < total {
            let _ = writeln!(
                out,
                "[{} more lines; continue with offset {}]",
                total - end,
                end + 1
            );
        }
        ToolOutput::ok(truncate_middle(&out, MAX_OUTPUT_CHARS))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write_temp(content: &[u8]) -> tempfile::NamedTempFile {
        let file = tempfile::NamedTempFile::new().unwrap();
        std::fs::write(file.path(), content).unwrap();
        file
    }

    async fn read(input: Value) -> ToolOutput {
        ReadFile::default().run(input).await
    }

    #[tokio::test]
    async fn refuses_the_settings_file_but_reads_the_rest_of_junees_home() {
        let (_dir, root, file) = crate::tools::settings_file::testing::home();
        let config = root.join(".junees/config.toml");
        std::os::unix::fs::symlink(&config, root.join("link")).unwrap();
        let tool = ReadFile {
            settings_file: Some(file),
        };
        for path in [
            config.clone(),
            root.join(".junees/sessions/../config.toml"),
            root.join("link"),
            crate::tools::settings_file::testing::relative(&config),
        ] {
            let output = tool.run(json!({"path": path})).await;
            assert!(output.is_error, "{}: {}", path.display(), output.content);
            assert!(output.content.contains("private Junees settings file"));
            assert!(!output.content.contains("secret-key"));
        }
        let output = tool
            .run(json!({"path": root.join(".junees/sessions/s.jsonl")}))
            .await;
        assert!(!output.is_error, "{}", output.content);
    }

    #[tokio::test]
    async fn numbers_lines() {
        let file = write_temp("첫째\nsecond\n".as_bytes());
        let output = read(json!({"path": file.path()})).await;
        assert_eq!(output, ToolOutput::ok("     1\t첫째\n     2\tsecond\n"));
    }

    #[tokio::test]
    async fn pages_with_offset_and_limit() {
        let file = write_temp(b"a\nb\nc\nd\n");
        let output = read(json!({"path": file.path(), "offset": 2, "limit": 2})).await;
        assert_eq!(
            output.content,
            "     2\tb\n     3\tc\n[1 more lines; continue with offset 4]\n"
        );
    }

    #[tokio::test]
    async fn offset_past_end_is_an_error() {
        let file = write_temp(b"a\n");
        let output = read(json!({"path": file.path(), "offset": 5})).await;
        assert!(output.is_error);
        assert!(output.content.contains("past the end"));
    }

    #[tokio::test]
    async fn empty_file() {
        let file = write_temp(b"");
        let output = read(json!({"path": file.path()})).await;
        assert!(!output.is_error);
        assert!(output.content.ends_with("is empty"));
    }

    #[tokio::test]
    async fn refuses_binary_files() {
        let file = write_temp(b"\x00\x01\x02");
        let output = read(json!({"path": file.path()})).await;
        assert!(output.is_error);
        assert!(output.content.contains("binary"));
    }

    #[tokio::test]
    async fn refuses_files_over_the_size_limit() {
        let file = tempfile::NamedTempFile::new().unwrap();
        file.as_file().set_len(MAX_FILE_BYTES + 1).unwrap();
        let output = read(json!({"path": file.path()})).await;
        assert!(output.is_error);
        assert!(output.content.contains("up to 10 MB"), "{}", output.content);
    }

    #[tokio::test]
    async fn refuses_devices_and_directories() {
        let output = read(json!({"path": "/dev/zero"})).await;
        assert!(output.is_error);
        assert!(output.content.ends_with("is not a regular file"));
        let dir = tempfile::tempdir().unwrap();
        assert!(read(json!({"path": dir.path()})).await.is_error);
    }

    #[tokio::test]
    async fn missing_file_is_an_error() {
        let output = read(json!({"path": "/definitely/not/here"})).await;
        assert!(output.is_error);
        assert!(
            output
                .content
                .starts_with("cannot read /definitely/not/here")
        );
    }
}
