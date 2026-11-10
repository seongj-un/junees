//! `skill_view` and `skill_manage`, the model's tools for its skills.

use std::path::Path;

use async_trait::async_trait;
use serde_json::{Value, json};

use super::{FILE_DIRS, Store, dir};
use crate::tools::{Tool, ToolOutput, ToolRegistry};

/// Adds `skill_view`, and `skill_manage` when `manage` is true, for the
/// skills of the state folder `home`; nothing without one.
pub fn register(tools: &mut ToolRegistry, home: Option<&Path>, manage: bool) {
    let Some(home) = home else {
        return;
    };
    let store = Store::new(dir(home));
    tools.register(SkillView {
        store: store.clone(),
    });
    if manage {
        tools.register(SkillManage { store });
    }
}

/// Runs a store operation on a blocking thread (it does file I/O and must
/// finish once started, even if the turn is cancelled).
async fn blocking(op: impl FnOnce() -> Result<String, String> + Send + 'static) -> ToolOutput {
    match tokio::task::spawn_blocking(op).await {
        Ok(Ok(text)) => ToolOutput::ok(text),
        Ok(Err(err)) => ToolOutput::error(err),
        Err(err) => ToolOutput::error(format!("the skill operation failed: {err}")),
    }
}

fn string(input: &Value, key: &str) -> Result<Option<String>, ToolOutput> {
    match input.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(text)) => Ok(Some(text.clone())),
        Some(_) => Err(ToolOutput::error(format!("`{key}` must be a string"))),
    }
}

fn required(input: &Value, key: &str, action: &str) -> Result<String, ToolOutput> {
    string(input, key)?
        .ok_or_else(|| ToolOutput::error(format!("`{key}` is required for {action}")))
}

pub struct SkillView {
    store: Store,
}

#[async_trait]
impl Tool for SkillView {
    fn name(&self) -> &str {
        "skill_view"
    }

    fn description(&self) -> &str {
        "Read a skill: its SKILL.md (the default, followed by the list of its files) or \
         one of its files (file_path, relative to the skill, such as references/api.md). \
         Read a skill from the list in the system prompt before a task it fits, and follow it."
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "name": {"type": "string", "description": "The skill's name"},
                "file_path": {
                    "type": "string",
                    "description": "A file of the skill, relative to its folder; SKILL.md if omitted"
                }
            },
            "required": ["name"]
        })
    }

    async fn run(&self, input: Value) -> ToolOutput {
        let (name, file_path) = match (
            required(&input, "name", "skill_view"),
            string(&input, "file_path"),
        ) {
            (Ok(name), Ok(file_path)) => (name, file_path),
            (Err(err), _) | (_, Err(err)) => return err,
        };
        let store = self.store.clone();
        blocking(move || store.view(&name, file_path.as_deref())).await
    }
}

pub struct SkillManage {
    store: Store,
}

#[async_trait]
impl Tool for SkillManage {
    fn name(&self) -> &str {
        "skill_manage"
    }

    fn description(&self) -> &str {
        "Create, change or delete a skill (a saved procedure for a recurring task). \
         create: `content` is the whole SKILL.md, starting with a front matter \
         (---, `name: <the skill's name>`, `description: <when to use it, at most 60 \
         characters>`, ---) followed by the steps and pitfalls. patch: replace \
         `old_string`, which must occur exactly once, with `new_string` in SKILL.md or in \
         `file_path`. write_file: write `file_content` to `file_path` under references/, \
         templates/, scripts/ or assets/. remove_file: remove such a file. delete: move the \
         whole skill to the trash. Names are 1-64 of a-z, 0-9 and -. Content that looks like \
         a secret (API keys, tokens, private keys) is refused."
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "action": {
                    "type": "string",
                    "enum": ["create", "patch", "write_file", "remove_file", "delete"]
                },
                "name": {"type": "string", "description": "The skill's name"},
                "content": {"type": "string", "description": "create: the whole SKILL.md"},
                "old_string": {"type": "string", "description": "patch: the exact text to replace"},
                "new_string": {"type": "string", "description": "patch: the text to put in its place"},
                "file_path": {
                    "type": "string",
                    "description": format!(
                        "patch: the file to change (SKILL.md if omitted); write_file, \
                         remove_file: a file under {}/",
                        FILE_DIRS.join("/, ")
                    )
                },
                "file_content": {"type": "string", "description": "write_file: the file's text"}
            },
            "required": ["action", "name"]
        })
    }

    async fn run(&self, input: Value) -> ToolOutput {
        let store = self.store.clone();
        let parsed = (|| {
            let action = required(&input, "action", "skill_manage")?;
            let name = required(&input, "name", "skill_manage")?;
            let op: Box<dyn FnOnce() -> Result<String, String> + Send> = match action.as_str() {
                "create" => {
                    let content = required(&input, "content", "create")?;
                    Box::new(move || store.create(&name, &content))
                }
                "patch" => {
                    let old = required(&input, "old_string", "patch")?;
                    let new = required(&input, "new_string", "patch")?;
                    let file_path = string(&input, "file_path")?;
                    Box::new(move || store.patch(&name, file_path.as_deref(), &old, &new))
                }
                "write_file" => {
                    let file_path = required(&input, "file_path", "write_file")?;
                    let content = required(&input, "file_content", "write_file")?;
                    Box::new(move || store.write_file(&name, &file_path, &content))
                }
                "remove_file" => {
                    let file_path = required(&input, "file_path", "remove_file")?;
                    Box::new(move || store.remove_file(&name, &file_path))
                }
                "delete" => Box::new(move || store.delete(&name)),
                other => {
                    return Err(ToolOutput::error(format!(
                        "unknown action `{}`; use create, patch, write_file, remove_file or \
                         delete",
                        other.escape_debug()
                    )));
                }
            };
            Ok(op)
        })();
        match parsed {
            Ok(op) => blocking(op).await,
            Err(err) => err,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn skill(name: &str) -> String {
        format!("---\nname: {name}\ndescription: Use when testing\n---\n1. run the tests\n")
    }

    fn registry(home: &Path, manage: bool) -> ToolRegistry {
        let mut tools = ToolRegistry::new();
        register(&mut tools, Some(home), manage);
        tools
    }

    fn names(tools: &ToolRegistry) -> Vec<String> {
        tools.specs().into_iter().map(|spec| spec.name).collect()
    }

    #[test]
    fn registers_by_home_and_permission() {
        let home = tempfile::tempdir().unwrap();
        assert_eq!(
            names(&registry(home.path(), true)),
            ["skill_view", "skill_manage"]
        );
        // Task sessions without JUNEES_TASK_SKILL_MANAGE.
        assert_eq!(names(&registry(home.path(), false)), ["skill_view"]);
        let mut none = ToolRegistry::new();
        register(&mut none, None, true);
        assert!(names(&none).is_empty());
    }

    #[tokio::test]
    async fn manages_and_views_skills_through_the_tools() {
        let home = tempfile::tempdir().unwrap();
        let tools = registry(home.path(), true);
        let out = tools
            .dispatch(
                "skill_manage",
                json!({"action": "create", "name": "tests", "content": skill("tests")}),
            )
            .await;
        assert!(!out.is_error, "{}", out.content);
        let out = tools
            .dispatch(
                "skill_manage",
                json!({"action": "patch", "name": "tests", "old_string": "1. run", "new_string": "1. always run"}),
            )
            .await;
        assert!(!out.is_error, "{}", out.content);
        let out = tools
            .dispatch(
                "skill_manage",
                json!({"action": "write_file", "name": "tests", "file_path": "references/a.md", "file_content": "notes"}),
            )
            .await;
        assert!(!out.is_error, "{}", out.content);
        let out = tools.dispatch("skill_view", json!({"name": "tests"})).await;
        assert_eq!(
            out,
            ToolOutput::ok(
                "---\nname: tests\ndescription: Use when testing\n---\n1. always run the tests\n\
                 \n[files of this skill, read with skill_view file_path: references/a.md]"
            )
        );
        let out = tools
            .dispatch(
                "skill_view",
                json!({"name": "tests", "file_path": "references/a.md"}),
            )
            .await;
        assert_eq!(out, ToolOutput::ok("notes"));
        let out = tools
            .dispatch(
                "skill_manage",
                json!({"action": "remove_file", "name": "tests", "file_path": "references/a.md"}),
            )
            .await;
        assert!(!out.is_error, "{}", out.content);
        let out = tools
            .dispatch("skill_manage", json!({"action": "delete", "name": "tests"}))
            .await;
        assert!(!out.is_error, "{}", out.content);
        let out = tools.dispatch("skill_view", json!({"name": "tests"})).await;
        assert_eq!(
            out,
            ToolOutput::error("no skill named `tests`; there are no skills yet")
        );
    }

    #[tokio::test]
    async fn explains_bad_input() {
        let home = tempfile::tempdir().unwrap();
        let tools = registry(home.path(), true);
        for (input, expected) in [
            (json!({}), "`action` is required for skill_manage"),
            (
                json!({"action": "create"}),
                "`name` is required for skill_manage",
            ),
            (
                json!({"action": "create", "name": "x"}),
                "`content` is required for create",
            ),
            (
                json!({"action": "patch", "name": "x", "old_string": "a"}),
                "`new_string` is required for patch",
            ),
            (
                json!({"action": "write_file", "name": "x", "file_path": "references/a"}),
                "`file_content` is required for write_file",
            ),
            (
                json!({"action": "rename", "name": "x"}),
                "unknown action `rename`",
            ),
            (
                json!({"action": "delete", "name": 3}),
                "`name` must be a string",
            ),
            (
                json!({"action": "delete", "name": "x"}),
                "no skill named `x`",
            ),
        ] {
            let out = tools.dispatch("skill_manage", input.clone()).await;
            assert!(out.is_error, "{input}");
            assert!(out.content.contains(expected), "{input}: {}", out.content);
        }
        let out = tools.dispatch("skill_view", json!({})).await;
        assert_eq!(out, ToolOutput::error("`name` is required for skill_view"));
        let out = tools.dispatch("skill_view", json!({"name": "../x"})).await;
        assert!(
            out.content.contains("invalid skill name"),
            "{}",
            out.content
        );
    }
}
