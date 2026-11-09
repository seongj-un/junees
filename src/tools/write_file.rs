use std::path::Path;

use async_trait::async_trait;
use serde_json::{Value, json};

use super::{Tool, ToolOutput, str_arg};

/// Writes a whole file, creating parent directories as needed.
pub struct WriteFile;

#[async_trait]
impl Tool for WriteFile {
    fn name(&self) -> &'static str {
        "write_file"
    }

    fn description(&self) -> &'static str {
        "Write content to a file, replacing it if it exists. \
         Parent directories are created automatically."
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "path": {"type": "string", "description": "File path, absolute or relative to the working directory"},
                "content": {"type": "string", "description": "The full new content of the file"}
            },
            "required": ["path", "content"]
        })
    }

    async fn run(&self, input: Value) -> ToolOutput {
        let path = match str_arg(&input, "path") {
            Ok(path) => path,
            Err(err) => return err,
        };
        let content = match str_arg(&input, "content") {
            Ok(content) => content,
            Err(err) => return err,
        };

        if let Some(parent) = Path::new(path).parent()
            && !parent.as_os_str().is_empty()
            && let Err(err) = tokio::fs::create_dir_all(parent).await
        {
            return ToolOutput::error(format!("cannot create {}: {err}", parent.display()));
        }
        match tokio::fs::write(path, content).await {
            Ok(()) => ToolOutput::ok(format!("wrote {} bytes to {path}", content.len())),
            Err(err) => ToolOutput::error(format!("cannot write {path}: {err}")),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn writes_and_creates_parent_dirs() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("a/b/note.txt");
        let output = WriteFile
            .run(json!({"path": path, "content": "안녕"}))
            .await;
        assert!(!output.is_error, "{}", output.content);
        assert!(output.content.starts_with("wrote 6 bytes"));
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "안녕");
    }

    #[tokio::test]
    async fn overwrites_existing_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("f.txt");
        std::fs::write(&path, "old content").unwrap();
        WriteFile.run(json!({"path": path, "content": "new"})).await;
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "new");
    }

    #[tokio::test]
    async fn requires_content() {
        let output = WriteFile.run(json!({"path": "x.txt"})).await;
        assert!(output.is_error);
        assert!(output.content.contains("`content`"));
    }
}
