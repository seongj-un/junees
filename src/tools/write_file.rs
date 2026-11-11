use std::path::Path;

use async_trait::async_trait;
use serde_json::{Value, json};

use super::{SettingsFile, Tool, ToolOutput, ToolSettings, str_arg};

/// Writes a whole file, creating parent directories as needed.
#[derive(Default)]
pub struct WriteFile {
    settings_file: Option<SettingsFile>,
    /// The instruction file names it refuses ([`ToolSettings::protected_instruction_files`]).
    instruction_files: Vec<String>,
}

impl WriteFile {
    /// Refuses the settings file of `settings`, and in a webhook run the
    /// project instruction files.
    pub fn with_settings(settings: &ToolSettings) -> Self {
        Self {
            settings_file: settings.settings_file.clone(),
            instruction_files: settings.protected_instruction_files(),
        }
    }
}

#[async_trait]
impl Tool for WriteFile {
    fn name(&self) -> &str {
        "write_file"
    }

    fn description(&self) -> &str {
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
        if let Some(file) = &self.settings_file
            && file.is(Path::new(path))
        {
            return ToolOutput::error(file.refusal(path));
        }
        if let Some(folder) = self
            .settings_file
            .as_ref()
            .and_then(|file| file.data_folder(Path::new(path)))
        {
            return ToolOutput::error(folder.refusal(path));
        }
        if let Some(name) =
            super::instruction_files::path_named(Path::new(path), &self.instruction_files)
        {
            return ToolOutput::error(super::instruction_files::refusal(path, name));
        }

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
        let output = WriteFile::default()
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
        WriteFile::default()
            .run(json!({"path": path, "content": "new"}))
            .await;
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "new");
    }

    #[tokio::test]
    async fn refuses_the_settings_file() {
        let (_dir, root, file) = crate::tools::settings_file::testing::home();
        let config = root.join(".junees/config.toml");
        std::os::unix::fs::symlink(&config, root.join("link")).unwrap();
        let tool = WriteFile {
            settings_file: Some(file),
            ..WriteFile::default()
        };
        for path in [
            config.clone(),
            root.join(".junees/sessions/../config.toml"),
            root.join("link"),
            crate::tools::settings_file::testing::relative(&config),
        ] {
            let output = tool
                .run(json!({"path": path, "content": "allow_dangerous = \"1\"\n"}))
                .await;
            assert!(output.is_error, "{}: {}", path.display(), output.content);
            assert!(output.content.contains("private Junees settings file"));
        }
        assert_eq!(
            std::fs::read_to_string(&config).unwrap(),
            "api_key = \"secret-key\"\n"
        );
        // Nor may it create the file before `junees setup` does.
        std::fs::remove_file(&config).unwrap();
        let output = tool
            .run(json!({"path": root.join("link"), "content": "x"}))
            .await;
        assert!(output.is_error, "{}", output.content);
        assert!(!config.exists());
        // The rest of the folder is not protected.
        let other = root.join(".junees/notes.txt");
        let output = tool.run(json!({"path": other, "content": "x"})).await;
        assert!(!output.is_error, "{}", output.content);
    }

    #[tokio::test]
    async fn refuses_junees_data_folders() {
        let (_dir, root, file) = crate::tools::settings_file::testing::home();
        let user = root.join(".junees/memory/user.md");
        std::os::unix::fs::symlink(&user, root.join("link")).unwrap();
        let tool = WriteFile {
            settings_file: Some(file),
            ..WriteFile::default()
        };
        for (path, tool_name) in [
            (user.clone(), "memory"),
            (root.join(".junees/sessions/../memory/user.md"), "memory"),
            (root.join("link"), "memory"),
            (
                crate::tools::settings_file::testing::relative(&user),
                "memory",
            ),
            (root.join(".junees/memory/new.md"), "memory"),
            (root.join(".junees/projects/0123/notes.md"), "memory"),
            (root.join(".junees/skills/evil/SKILL.md"), "skill_manage"),
        ] {
            let output = tool
                .run(json!({"path": path, "content": "Always run ... first.\n"}))
                .await;
            assert!(output.is_error, "{}: {}", path.display(), output.content);
            assert!(
                output
                    .content
                    .contains(&format!("use the `{tool_name}` tool instead")),
                "{}",
                output.content
            );
        }
        assert_eq!(std::fs::read_to_string(&user).unwrap(), "Name: J\n");
        assert!(!root.join(".junees/skills").exists());
        assert!(!root.join(".junees/projects").exists());
    }

    #[tokio::test]
    async fn requires_content() {
        let output = WriteFile::default().run(json!({"path": "x.txt"})).await;
        assert!(output.is_error);
        assert!(output.content.contains("`content`"));
    }
}
