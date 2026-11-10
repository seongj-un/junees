//! The `memory` tool: lets the model save, change and remove entries of
//! the persistent memory.

use async_trait::async_trait;
use serde_json::{Value, json};

use super::{Change, Memory, Mode, Target};
use crate::tools::{Tool, ToolOutput};

pub struct MemoryTool {
    memory: Memory,
    mode: Mode,
}

impl MemoryTool {
    /// The tool over `memory`. In [`Mode::Task`] it changes only the
    /// project notes.
    pub fn new(memory: Memory, mode: Mode) -> Self {
        Self { memory, mode }
    }

    fn targets(&self) -> &'static [&'static str] {
        match self.mode {
            Mode::Task => &["project"],
            _ => &["user", "notes", "project"],
        }
    }
}

#[async_trait]
impl Tool for MemoryTool {
    fn name(&self) -> &str {
        "memory"
    }

    fn description(&self) -> &str {
        match self.mode {
            Mode::Task => {
                "Saves notes on this project that last across sessions (target project; \
                 the only one task sessions can change). action add saves content as a new \
                 entry; replace swaps the one entry containing old_text for content; remove \
                 deletes the one entry containing old_text. The MEMORY section of the system \
                 prompt shows memory as it was at session start."
            }
            _ => {
                "Saves short facts that last across sessions: about the user (target user), \
                 the environment and conventions (target notes), or this project (target \
                 project). action add saves content as a new entry; replace swaps the one \
                 entry containing old_text for content; remove deletes the one entry \
                 containing old_text. The MEMORY section of the system prompt shows memory \
                 as it was at session start."
            }
        }
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "target": {
                    "type": "string",
                    "enum": self.targets(),
                    "description": "Which memory to change."
                },
                "action": {
                    "type": "string",
                    "enum": ["add", "replace", "remove"]
                },
                "content": {
                    "type": "string",
                    "description": "The new entry (add), or the text that replaces the entry (replace)."
                },
                "old_text": {
                    "type": "string",
                    "description": "A part of the entry to replace or remove that no other entry contains."
                }
            },
            "required": ["target", "action"]
        })
    }

    async fn run(&self, input: Value) -> ToolOutput {
        let (target, change) = match parse(&input, self.mode) {
            Ok(parsed) => parsed,
            Err(message) => return ToolOutput::error(message),
        };
        let memory = self.memory.clone();
        // File work and a lock that may wait a moment: off the async threads.
        let result = tokio::task::spawn_blocking(move || memory.change(target, change)).await;
        match result {
            Ok(Ok(changed)) => ToolOutput::ok(format!(
                "{} ({} memory: {} entries, {}/{} characters). The system prompt keeps the \
                 memory this session started with; the change shows there from the next \
                 session.",
                changed.what,
                target.name(),
                changed.entries,
                changed.chars,
                changed.limit
            )),
            Ok(Err(message)) => ToolOutput::error(message),
            Err(err) => ToolOutput::error(format!("the memory change failed: {err}")),
        }
    }
}

/// The store and the change `input` asks for.
fn parse(input: &Value, mode: Mode) -> Result<(Target, Change), String> {
    let text = |key: &str| input.get(key).and_then(Value::as_str);
    let need =
        |key: &str| text(key).ok_or_else(|| format!("missing required string argument `{key}`"));
    let target_name = need("target")?;
    let target = Target::parse(target_name)
        .ok_or_else(|| format!("unknown target `{target_name}`; use user, notes or project"))?;
    if mode == Mode::Task && target != Target::Project {
        return Err(format!(
            "task sessions can only change the notes on this project (target project); \
             {} memory is changed only in conversations with the user",
            target.name()
        ));
    }
    let change = match need("action")? {
        "add" => Change::Add(need("content")?.to_string()),
        "replace" => Change::Replace {
            old: need("old_text")?.to_string(),
            new: need("content")?.to_string(),
        },
        "remove" => Change::Remove(need("old_text")?.to_string()),
        other => {
            return Err(format!(
                "unknown action `{other}`; use add, replace or remove"
            ));
        }
    };
    Ok((target, change))
}
