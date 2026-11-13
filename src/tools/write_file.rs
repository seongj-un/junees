use std::path::Path;

use async_trait::async_trait;
use serde_json::{Value, json};

use super::file_access::FileAccess;
use super::{Tool, ToolOutput, ToolSettings, str_arg};

/// Writes a whole file, creating parent directories as needed.
#[derive(Default)]
pub struct WriteFile {
    access: FileAccess,
    /// The instruction file names it refuses ([`ToolSettings::protected_instruction_files`]).
    instruction_files: Vec<String>,
}

impl WriteFile {
    /// Refuses the settings file of `settings`, in a webhook run the
    /// project instruction files, and in an unattended run paths outside
    /// its folders ([`FileAccess`]).
    pub fn with_settings(settings: &ToolSettings) -> Self {
        Self {
            access: FileAccess::new(settings),
            instruction_files: settings.protected_instruction_files(),
        }
    }

    /// A tool with the rules of `access` (tests).
    #[cfg(test)]
    pub(crate) fn with_access(access: FileAccess) -> Self {
        Self {
            access,
            ..Self::default()
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
        if let Some(file) = self.access.settings_file()
            && file.is(Path::new(path))
        {
            return ToolOutput::error(file.refusal(path));
        }
        if let Some(folder) = self
            .access
            .settings_file()
            .and_then(|file| file.data_folder(Path::new(path)))
        {
            return ToolOutput::error(folder.refusal(path));
        }
        if let Some(name) =
            super::instruction_files::path_named(Path::new(path), &self.instruction_files)
        {
            return ToolOutput::error(super::instruction_files::refusal(path, name));
        }

        let access = self.access.clone();
        let names = self.instruction_files.clone();
        let (owned, content) = (path.to_string(), content.to_string());
        // Task-local: taken here, not on the blocking thread.
        let turn = crate::checkpoints::current();
        // One blocking call from the checks to the rename: dropping this
        // future (a cancelled turn) cannot stop it half way.
        tokio::task::spawn_blocking(move || {
            write(&access, &names, &owned, &content, turn.as_deref())
        })
        .await
        .unwrap_or_else(|err| ToolOutput::error(format!("cannot write {path}: {err}")))
    }
}

/// Writes `content` to `path`: its folder (made when missing) opened and
/// checked, the new content renamed over the name, or written in place
/// when the folder has no room for a temporary file.
/// With `turn`, the file's state before is recorded first, and its hash
/// after.
fn write(
    access: &FileAccess,
    names: &[String],
    path: &str,
    content: &str,
    turn: Option<&crate::checkpoints::Turn>,
) -> ToolOutput {
    let target = match access.open_write(path, true) {
        Ok(target) => target,
        Err(refusal) => return ToolOutput::error(refusal.message(path, "write")),
    };
    if let Some(name) = super::instruction_files::path_named(target.real(), names) {
        return ToolOutput::error(super::instruction_files::refusal(path, name));
    }
    if let Some(turn) = turn {
        match target.before() {
            Ok(before) => turn.before(target.real(), before),
            Err(refusal) => return ToolOutput::error(refusal.message(path, "write")),
        }
    }
    let written = target.replace(content.as_bytes(), true);
    if let Some(turn) = turn {
        match &written {
            Ok(()) => turn.after(target.real(), Some(content.as_bytes())),
            Err(_) => turn.after_disk(target.real()),
        }
    }
    match written {
        Ok(()) => ToolOutput::ok(format!("wrote {} bytes to {path}", content.len())),
        Err(err) => ToolOutput::error(format!("cannot write {path}: {err}")),
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
        let tool = WriteFile::with_settings(&ToolSettings {
            settings_file: Some(file),
            ..ToolSettings::default()
        });
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
        let tool = WriteFile::with_settings(&ToolSettings {
            settings_file: Some(file),
            ..ToolSettings::default()
        });
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
