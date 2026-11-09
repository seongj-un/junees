//! Tools the model can call, and the registry that dispatches them.

mod bash;
mod policy;
mod read_file;
mod write_file;

pub use bash::Bash;
#[cfg(test)]
pub(crate) use bash::testing;
pub use policy::{CommandPolicy, Verdict};
pub use read_file::ReadFile;
pub use write_file::WriteFile;

use async_trait::async_trait;
use serde_json::Value;

use crate::provider::ToolSpec;

/// Credentials Junees reads that the model's shell commands must not see.
pub const SECRET_ENV_VARS: &[&str] = &["ANTHROPIC_API_KEY", "ANTHROPIC_AUTH_TOKEN"];

/// User settings for the default tools, read by [`crate::config::Config`].
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ToolSettings {
    /// Environment variables bash must not see on top of
    /// [`SECRET_ENV_VARS`] (`JUNEES_HIDDEN_ENV`).
    pub hidden_env: Vec<String>,
    /// Command prefixes bash refuses on top of the built-in rules
    /// (`JUNEES_DENY_COMMANDS`).
    pub deny_commands: Vec<String>,
    /// Turns the built-in command rules off (`JUNEES_ALLOW_DANGEROUS`).
    pub allow_dangerous: bool,
}

/// What a tool hands back to the model. Failures are reported as output with
/// `is_error` set rather than as Rust errors, so the model can react to them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolOutput {
    pub content: String,
    pub is_error: bool,
}

impl ToolOutput {
    pub fn ok(content: impl Into<String>) -> Self {
        Self {
            content: content.into(),
            is_error: false,
        }
    }

    pub fn error(content: impl Into<String>) -> Self {
        Self {
            content: content.into(),
            is_error: true,
        }
    }
}

#[async_trait]
pub trait Tool: Send + Sync {
    fn name(&self) -> &str;
    fn description(&self) -> &str;
    /// JSON Schema of the tool input.
    fn input_schema(&self) -> Value;
    async fn run(&self, input: Value) -> ToolOutput;
}

#[derive(Default)]
pub struct ToolRegistry {
    tools: Vec<Box<dyn Tool>>,
}

impl ToolRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    /// bash, read_file and write_file. bash never sees [`SECRET_ENV_VARS`]
    /// or the extra hidden variables, and runs only commands its
    /// [`CommandPolicy`] allows.
    pub fn with_defaults(settings: &ToolSettings) -> Self {
        let mut registry = Self::new();
        registry.register(Bash::with_settings(settings));
        registry.register(ReadFile);
        registry.register(WriteFile);
        registry
    }

    /// Adds a tool. A tool with the same name replaces the earlier one.
    pub fn register(&mut self, tool: impl Tool + 'static) {
        self.tools.retain(|existing| existing.name() != tool.name());
        self.tools.push(Box::new(tool));
    }

    pub fn specs(&self) -> Vec<ToolSpec> {
        self.tools
            .iter()
            .map(|tool| ToolSpec {
                name: tool.name().to_string(),
                description: tool.description().to_string(),
                input_schema: tool.input_schema(),
            })
            .collect()
    }

    pub async fn dispatch(&self, name: &str, input: Value) -> ToolOutput {
        match self.tools.iter().find(|tool| tool.name() == name) {
            Some(tool) => tool.run(input).await,
            None => ToolOutput::error(format!("unknown tool `{name}`")),
        }
    }
}

/// Keeps the head and tail of `text` when it is longer than `max_chars`
/// characters. Cuts on char boundaries, so multi-byte text stays valid.
pub fn truncate_middle(text: &str, max_chars: usize) -> String {
    let total = text.chars().count();
    if total <= max_chars {
        return text.to_string();
    }
    let head = max_chars / 2;
    let tail = max_chars - head;
    let byte_at = |char_index: usize| {
        text.char_indices()
            .nth(char_index)
            .map_or(text.len(), |(i, _)| i)
    };
    let head_end = byte_at(head);
    let tail_start = byte_at(total - tail);
    format!(
        "{}\n\n[... {} characters omitted ...]\n\n{}",
        &text[..head_end],
        total - head - tail,
        &text[tail_start..]
    )
}

fn str_arg<'a>(input: &'a Value, key: &str) -> Result<&'a str, ToolOutput> {
    input
        .get(key)
        .and_then(Value::as_str)
        .ok_or_else(|| ToolOutput::error(format!("missing required string argument `{key}`")))
}

fn opt_u64_arg(input: &Value, key: &str) -> Result<Option<u64>, ToolOutput> {
    match input.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(value) => value
            .as_u64()
            .map(Some)
            .ok_or_else(|| ToolOutput::error(format!("`{key}` must be a non-negative integer"))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    struct Echo;

    #[async_trait]
    impl Tool for Echo {
        fn name(&self) -> &str {
            "echo"
        }
        fn description(&self) -> &str {
            "echoes"
        }
        fn input_schema(&self) -> Value {
            json!({"type": "object"})
        }
        async fn run(&self, input: Value) -> ToolOutput {
            ToolOutput::ok(input.to_string())
        }
    }

    #[tokio::test]
    async fn dispatches_by_name() {
        let mut registry = ToolRegistry::new();
        registry.register(Echo);
        let output = registry.dispatch("echo", json!({"a": 1})).await;
        assert_eq!(output, ToolOutput::ok(r#"{"a":1}"#));
    }

    #[tokio::test]
    async fn unknown_tool_is_an_error_output() {
        let output = ToolRegistry::new().dispatch("nope", json!({})).await;
        assert!(output.is_error);
        assert!(output.content.contains("unknown tool `nope`"));
    }

    #[test]
    fn register_replaces_same_name() {
        let mut registry = ToolRegistry::new();
        registry.register(Echo);
        registry.register(Echo);
        assert_eq!(registry.specs().len(), 1);
    }

    #[test]
    fn defaults_have_three_tools() {
        let names: Vec<String> = ToolRegistry::with_defaults(&ToolSettings::default())
            .specs()
            .into_iter()
            .map(|spec| spec.name)
            .collect();
        assert_eq!(names, ["bash", "read_file", "write_file"]);
    }

    #[tokio::test]
    async fn defaults_apply_the_command_settings() {
        let command = json!({"command": "echo deny-me && shutdown() { :; }"});
        let output = ToolRegistry::with_defaults(&ToolSettings::default())
            .dispatch("bash", command.clone())
            .await;
        assert_eq!(
            output.content,
            "blocked by policy: `shutdown` would stop the machine"
        );

        let settings = ToolSettings {
            deny_commands: vec!["echo deny-me".into()],
            allow_dangerous: true,
            ..ToolSettings::default()
        };
        let registry = ToolRegistry::with_defaults(&settings);
        let output = registry.dispatch("bash", command).await;
        assert_eq!(
            output.content,
            "blocked by policy: matches JUNEES_DENY_COMMANDS entry `echo deny-me`"
        );
        let output = registry
            .dispatch(
                "bash",
                json!({"command": "shutdown() { echo fake; }; shutdown"}),
            )
            .await;
        assert_eq!(output, ToolOutput::ok("fake\n[exit code 0]"));
    }

    #[test]
    fn truncate_keeps_short_text() {
        assert_eq!(truncate_middle("hello", 5), "hello");
    }

    #[test]
    fn truncate_keeps_head_and_tail_on_char_boundaries() {
        let text = "가나다라마바사아자차";
        let out = truncate_middle(text, 4);
        assert!(out.starts_with("가나\n"));
        assert!(out.ends_with("\n자차"));
        assert!(out.contains("[... 6 characters omitted ...]"));
    }

    #[test]
    fn arg_helpers_report_bad_input() {
        let input = json!({"s": "x", "n": 3, "neg": -1});
        assert_eq!(str_arg(&input, "s").unwrap(), "x");
        assert!(str_arg(&input, "missing").unwrap_err().is_error);
        assert_eq!(opt_u64_arg(&input, "n").unwrap(), Some(3));
        assert_eq!(opt_u64_arg(&input, "missing").unwrap(), None);
        assert!(opt_u64_arg(&input, "neg").is_err());
    }
}
