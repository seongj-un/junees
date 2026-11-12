//! Junees's conversation (Messages API shaped blocks) as a Chat Completions
//! request, and a Chat Completions answer as Junees's blocks.
//!
//! Shapes from the API reference (developers.openai.com/api/reference/
//! resources/chat/subresources/completions/methods/create, and the
//! `CreateChatCompletionRequest`/`CreateChatCompletionResponse` schemas of
//! github.com/openai/openai-openapi):
//!
//! - The system prompt is the first message, `role: "system"`.
//! - A tool call is an entry of the assistant message's `tool_calls`
//!   (`{id, type: "function", function: {name, arguments}}`, `arguments`
//!   a JSON text); its result is a message of its own, `role: "tool"` with
//!   the call's `tool_call_id`, and takes text only. So the images of a
//!   tool result go in a user message after the tool messages.
//! - Images are `image_url` content parts; a data URL carries the bytes.
//! - Tools are `{type: "function", function: {name, description,
//!   parameters}}`.
//! - `finish_reason`: `stop`, `length`, `tool_calls`, `content_filter`,
//!   `function_call` (deprecated).
//! - `usage.prompt_tokens` counts every prompt token, the cached ones
//!   (`prompt_tokens_details.cached_tokens`, and `cache_write_tokens`
//!   where a server reports writes) included.

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::images::{Image, Source};
use crate::message::{ContentBlock, Message, Role, StopReason, Usage};
use crate::provider::{CompletionRequest, CompletionResponse, ProviderError, ToolSpec};

/// A request body.
#[derive(Serialize)]
pub(super) struct ChatRequest<'a> {
    pub model: &'a str,
    pub messages: Vec<Value>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub tools: Vec<Value>,
    /// The output limit under its old name, which the compatible servers
    /// (Ollama, vLLM, LM Studio, OpenRouter) take.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_tokens: Option<u32>,
    /// The output limit under the name OpenAI's own API wants: it
    /// deprecated `max_tokens`, and its reasoning models refuse it.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_completion_tokens: Option<u32>,
    /// How hard a reasoning model thinks, when the user set it.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reasoning_effort: Option<&'a str>,
    #[serde(skip_serializing_if = "is_false")]
    pub stream: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stream_options: Option<Value>,
}

fn is_false(value: &bool) -> bool {
    !*value
}

/// Room for reasoning added to the output limit of a call that asks for
/// no thinking (a summary, whose small limit is meant for its text) when a
/// reasoning effort is set: `max_completion_tokens` counts reasoning
/// tokens too (the Chat Completions reference), and the reasoning guide
/// recommends reserving at least 25,000 tokens for reasoning and output.
pub(super) const REASONING_ROOM: u32 = 25_000;

/// What the provider's settings add to each request.
#[derive(Clone, Copy, Default)]
pub(super) struct Options<'a> {
    /// The output limit goes as `max_completion_tokens`
    /// ([`super::Settings::uses_max_completion_tokens`]).
    pub max_completion_tokens: bool,
    /// `reasoning_effort`, when set.
    pub reasoning_effort: Option<&'a str>,
}

impl<'a> ChatRequest<'a> {
    /// The body for `request`. The request's `cache`, `thinking` and
    /// `web_search` are Anthropic's features and not sent: OpenAI caches
    /// long prompts on its own. `thinking: false` (a summary) only gets
    /// [`REASONING_ROOM`] when a reasoning effort other than `none` is set.
    pub fn new(model: &'a str, request: CompletionRequest<'a>, options: Options<'a>) -> Self {
        let reasons = options
            .reasoning_effort
            .is_some_and(|effort| effort != "none");
        let limit = if reasons && !request.thinking {
            request.max_tokens.saturating_add(REASONING_ROOM)
        } else {
            request.max_tokens
        };
        Self {
            model,
            messages: messages(request.system, request.messages),
            tools: request.tools.iter().map(tool).collect(),
            max_tokens: (!options.max_completion_tokens).then_some(limit),
            max_completion_tokens: options.max_completion_tokens.then_some(limit),
            reasoning_effort: options.reasoning_effort,
            stream: false,
            stream_options: None,
        }
    }

    /// Asks for a stream with a last chunk that carries the usage.
    pub fn streamed(mut self) -> Self {
        self.stream = true;
        self.stream_options = Some(json!({"include_usage": true}));
        self
    }
}

fn tool(spec: &ToolSpec) -> Value {
    json!({
        "type": "function",
        "function": {
            "name": spec.name,
            "description": spec.description,
            "parameters": spec.input_schema,
        }
    })
}

/// The system prompt and the conversation as Chat Completions messages.
pub(super) fn messages(system: &str, history: &[Message]) -> Vec<Value> {
    let mut out = Vec::new();
    if !system.is_empty() {
        out.push(json!({"role": "system", "content": system}));
    }
    for message in history {
        match message.role {
            Role::Assistant => out.extend(assistant(message)),
            Role::User => user(message, &mut out),
        }
    }
    out
}

/// An assistant message: its text, and its tool calls. Blocks only the
/// Anthropic API knows (thinking, server tool calls) are left out; a
/// message of nothing else is left out whole, since an empty assistant
/// message is refused.
fn assistant(message: &Message) -> Option<Value> {
    let text = message.text();
    let calls: Vec<Value> = message
        .content
        .iter()
        .filter_map(|block| match block {
            ContentBlock::ToolUse { id, name, input } => Some(json!({
                "id": id,
                "type": "function",
                "function": {"name": name, "arguments": arguments(input)},
            })),
            _ => None,
        })
        .collect();
    if text.is_empty() && calls.is_empty() {
        return None;
    }
    let mut out = json!({"role": "assistant", "content": text});
    if !calls.is_empty() {
        if text.is_empty() {
            out["content"] = Value::Null;
        }
        out["tool_calls"] = Value::Array(calls);
    }
    Some(out)
}

/// A tool call's `arguments`: its input as JSON text. Arguments that were
/// no JSON object when they came (kept as their text, see [`input`]) go
/// back as `{}`: servers that parse them (Ollama) refuse the request
/// otherwise, every later one of the session too. The call's error result
/// already told the model what was wrong.
fn arguments(input: &Value) -> String {
    match input {
        Value::Object(_) => input.to_string(),
        _ => "{}".to_string(),
    }
}

/// A tool call's input from its `arguments`: the JSON value, `{}` for
/// none, or the text itself when it is not JSON (a model can write broken
/// arguments; the tool then refuses the input and the model sees why).
fn input(arguments: &Value) -> Value {
    match arguments {
        Value::String(text) if text.trim().is_empty() => json!({}),
        Value::String(text) => {
            serde_json::from_str(text).unwrap_or_else(|_| Value::String(text.clone()))
        }
        Value::Null => json!({}),
        // Some servers send the arguments as an object.
        other => other.clone(),
    }
}

/// A user message: a `tool` message for each tool result, then one user
/// message with the images of those results and the user's own text and
/// images.
fn user(message: &Message, out: &mut Vec<Value>) {
    let mut parts = Vec::new();
    let mut user_parts = Vec::new();
    for block in &message.content {
        match block {
            ContentBlock::ToolResult {
                tool_use_id,
                content,
                is_error,
                images,
            } => {
                let content = if *is_error {
                    format!("[tool error] {content}")
                } else {
                    content.clone()
                };
                out.push(json!({"role": "tool", "tool_call_id": tool_use_id, "content": content}));
                if !images.is_empty() {
                    parts.push(Part::Text(format!(
                        "The images of tool call {tool_use_id}:"
                    )));
                    parts.extend(images.iter().map(Part::Image));
                }
            }
            ContentBlock::Text { text, .. } => user_parts.push(Part::Text(text.clone())),
            ContentBlock::Image(image) => user_parts.push(Part::Image(image)),
            ContentBlock::ToolUse { .. } | ContentBlock::Other(_) => {}
        }
    }
    parts.extend(user_parts);
    if parts.is_empty() {
        return;
    }
    let content = if parts.iter().all(|part| matches!(part, Part::Text(_))) {
        // Plain text as a string: every compatible server takes that.
        Value::String(
            parts
                .iter()
                .map(|part| match part {
                    Part::Text(text) => text.as_str(),
                    Part::Image(_) => "",
                })
                .collect::<Vec<_>>()
                .join("\n"),
        )
    } else {
        Value::Array(parts.iter().map(Part::to_json).collect())
    };
    out.push(json!({"role": "user", "content": content}));
}

enum Part<'a> {
    Text(String),
    Image(&'a Image),
}

impl Part<'_> {
    fn to_json(&self) -> Value {
        match self {
            Part::Text(text) => json!({"type": "text", "text": text}),
            Part::Image(image) => match &image.source {
                Source::Base64(data) => json!({
                    "type": "image_url",
                    "image_url": {
                        "url": format!("data:{};base64,{data}", image.media_type.as_str())
                    }
                }),
                // Session journals keep these; loading a session reads the
                // file back, so a request never has one.
                Source::Stored { .. } => json!({"type": "text", "text": "[image not available]"}),
            },
        }
    }
}

/// A whole (not streamed) answer.
#[derive(Deserialize)]
pub(super) struct ChatResponse {
    #[serde(default)]
    choices: Vec<Choice>,
    #[serde(default)]
    usage: Option<ApiUsage>,
    /// An error in place of the answer (OpenRouter, with status 200).
    #[serde(default)]
    error: Option<Value>,
}

#[derive(Deserialize)]
struct Choice {
    #[serde(default)]
    message: ResponseMessage,
    #[serde(default)]
    finish_reason: Option<String>,
}

#[derive(Deserialize, Default)]
struct ResponseMessage {
    #[serde(default)]
    content: Option<Value>,
    #[serde(default)]
    refusal: Option<String>,
    #[serde(default)]
    tool_calls: Option<Vec<ToolCall>>,
}

#[derive(Deserialize)]
struct ToolCall {
    #[serde(default)]
    id: Option<String>,
    function: Function,
}

#[derive(Deserialize)]
struct Function {
    name: String,
    #[serde(default)]
    arguments: Value,
}

/// `usage` of an answer or of the last chunk of a stream.
#[derive(Deserialize, Default, Clone, Copy)]
pub(super) struct ApiUsage {
    #[serde(default)]
    prompt_tokens: Option<u64>,
    #[serde(default)]
    completion_tokens: Option<u64>,
    #[serde(default)]
    prompt_tokens_details: Option<PromptDetails>,
    #[serde(default)]
    completion_tokens_details: Option<CompletionDetails>,
}

#[derive(Deserialize, Default, Clone, Copy)]
struct CompletionDetails {
    /// Part of `completion_tokens`: reasoning, which Chat Completions never
    /// returns, so it is not sent back either.
    #[serde(default)]
    reasoning_tokens: Option<u64>,
}

#[derive(Deserialize, Default, Clone, Copy)]
struct PromptDetails {
    #[serde(default)]
    cached_tokens: Option<u64>,
    #[serde(default)]
    cache_write_tokens: Option<u64>,
}

impl From<ApiUsage> for Usage {
    /// Junees counts input without the cached prompt tokens, which are
    /// priced apart; Chat Completions counts them in `prompt_tokens`.
    /// `completion_tokens` holds the reasoning tokens too, which are billed
    /// as output (the reasoning guide), so output keeps them and
    /// `reasoning_tokens` says how many there were.
    fn from(usage: ApiUsage) -> Self {
        let prompt = usage.prompt_tokens.unwrap_or(0);
        let details = usage.prompt_tokens_details.unwrap_or_default();
        let read = details.cached_tokens.unwrap_or(0).min(prompt);
        let written = details.cache_write_tokens.unwrap_or(0).min(prompt - read);
        let output = usage.completion_tokens.unwrap_or(0);
        let reasoning = usage
            .completion_tokens_details
            .and_then(|details| details.reasoning_tokens)
            .unwrap_or(0)
            .min(output);
        Usage {
            input_tokens: prompt - read - written,
            output_tokens: output,
            cache_creation_input_tokens: written,
            cache_read_input_tokens: read,
            reasoning_tokens: reasoning,
            ..Usage::default()
        }
    }
}

impl ChatResponse {
    /// The error the body holds instead of an answer, if it does.
    pub fn error(&self) -> Option<ApiError> {
        let error = self.error.as_ref().filter(|_| self.choices.is_empty())?;
        match error {
            Value::Object(_) => Some(ApiError::from_value(error)),
            Value::String(message) => Some(ApiError {
                kind: None,
                code: None,
                message: message.clone(),
            }),
            _ => None,
        }
    }

    pub fn into_response(self) -> Result<CompletionResponse, ProviderError> {
        let usage = self.usage.map(Usage::from).unwrap_or_default();
        let Some(choice) = self.choices.into_iter().next() else {
            return Err(ProviderError::Decode("the answer has no choices".into()));
        };
        let message = choice.message;
        let mut text = match message.content {
            Some(Value::String(text)) => text,
            // Content parts, as some servers send them.
            Some(Value::Array(parts)) => parts
                .iter()
                .filter_map(|part| part["text"].as_str())
                .collect::<Vec<_>>()
                .join(""),
            _ => String::new(),
        };
        let refused = message.refusal.as_deref().is_some_and(|r| !r.is_empty());
        if let Some(refusal) = message.refusal.filter(|r| !r.is_empty()) {
            text.push_str(&refusal);
        }
        let calls = message
            .tool_calls
            .unwrap_or_default()
            .into_iter()
            .enumerate()
            .map(|(index, call)| Call {
                id: call.id.unwrap_or_default(),
                name: call.function.name,
                arguments: call.function.arguments,
                index,
            })
            .collect();
        Ok(response(
            text,
            calls,
            choice.finish_reason.as_deref(),
            refused,
            usage,
        ))
    }
}

/// A tool call of an answer, whole.
pub(super) struct Call {
    pub id: String,
    pub name: String,
    /// The `arguments` as they came: JSON text, or an object.
    pub arguments: Value,
    /// Its place among the answer's calls, for an id it lacks.
    pub index: usize,
}

/// The response from the answer's parts.
pub(super) fn response(
    text: String,
    calls: Vec<Call>,
    finish_reason: Option<&str>,
    refused: bool,
    usage: Usage,
) -> CompletionResponse {
    let mut content = Vec::new();
    if !text.is_empty() {
        content.push(ContentBlock::text(text));
    }
    let called = !calls.is_empty();
    for call in calls {
        let id = if call.id.is_empty() {
            missing_id(call.index)
        } else {
            call.id
        };
        content.push(ContentBlock::ToolUse {
            id,
            name: call.name,
            input: input(&call.arguments),
        });
    }
    CompletionResponse {
        content,
        stop_reason: stop_reason(finish_reason, called, refused),
        usage,
    }
}

/// An id for a call the server sent without one (some local servers do):
/// unique in the conversation, since the next answer's calls get new ones.
fn missing_id(index: usize) -> String {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|since| since.as_nanos())
        .unwrap_or_default();
    format!("call_junees_{nanos:x}_{index}")
}

/// The stop reason of a `finish_reason`. Some servers end a tool call
/// with `stop`; the calls are what counts.
fn stop_reason(finish_reason: Option<&str>, called: bool, refused: bool) -> StopReason {
    match finish_reason {
        Some("length") => StopReason::MaxTokens,
        Some("content_filter") => StopReason::Refusal,
        _ if refused => StopReason::Refusal,
        Some("tool_calls" | "function_call") => StopReason::ToolUse,
        _ if called => StopReason::ToolUse,
        Some("stop") => StopReason::EndTurn,
        _ => StopReason::Other,
    }
}

/// What an error body says: `{"error": {"message", "type", "code"}}` as
/// OpenAI and most compatible servers write it, `{"error": "..."}` as
/// Ollama does, or plain text.
pub(super) struct ApiError {
    pub kind: Option<String>,
    pub code: Option<String>,
    pub message: String,
}

impl ApiError {
    pub fn parse(body: &[u8]) -> Self {
        match serde_json::from_slice::<Value>(body) {
            Ok(value) if value["error"].is_object() => Self::from_value(&value["error"]),
            Ok(value) if value["error"].is_string() => Self {
                kind: None,
                code: None,
                message: value["error"].as_str().unwrap_or_default().to_string(),
            },
            _ => Self {
                kind: None,
                code: None,
                message: plain_error(body),
            },
        }
    }

    pub fn from_value(error: &Value) -> Self {
        let text = |field: &str| error[field].as_str().map(str::to_string);
        Self {
            kind: text("type"),
            // OpenRouter writes the HTTP status as a number here.
            code: text("code").or_else(|| error["code"].as_u64().map(|code| code.to_string())),
            message: text("message").unwrap_or_default(),
        }
    }

    /// A 429 that waiting does not fix: the account is out of credit or
    /// over a spend limit. The error code guide
    /// (developers.openai.com/api/docs/guides/error-codes) names the
    /// codes, says `type` can still be `insufficient_quota` for any of
    /// them, and that retrying them won't restore access.
    pub fn is_quota(&self) -> bool {
        self.kind.as_deref() == Some("insufficient_quota")
            || matches!(
                self.code.as_deref(),
                Some(
                    "insufficient_quota"
                        | "credit_balance_exhausted"
                        | "organization_spend_limit_exceeded"
                        | "project_spend_limit_exceeded"
                        | "organization_usage_limit_exceeded"
                )
            )
    }

    /// The error Junees reacts to. `kind` is the error's `type`, or
    /// `context_length_exceeded` for an overflow, so
    /// [`ProviderError::is_context_overflow`] sees it whatever the message
    /// says.
    pub fn into_error(self, status: u16) -> ProviderError {
        let kind = if self.code.as_deref() == Some("context_length_exceeded") {
            self.code
        } else {
            self.kind.or(self.code)
        };
        ProviderError::Api {
            status,
            kind,
            message: self.message,
        }
    }
}

/// How many characters of an error body that is no JSON (a proxy's HTML
/// error page) the error keeps: it goes to the terminal and to chat
/// channels.
const PLAIN_ERROR_CHARS: usize = 300;

fn plain_error(body: &[u8]) -> String {
    let text = String::from_utf8_lossy(body);
    let text = text.trim();
    match text.char_indices().nth(PLAIN_ERROR_CHARS) {
        Some((at, _)) => format!("{}... ({} bytes in all)", &text[..at], body.len()),
        None => text.to_string(),
    }
}

/// The HTTP status an error that came inside a stream would have had:
/// OpenRouter names it in `code`, others only give a `type`.
pub(super) fn stream_status(error: &ApiError) -> u16 {
    if let Some(status) = error
        .code
        .as_deref()
        .and_then(|code| code.parse::<u16>().ok())
        .filter(|status| (400..600).contains(status))
    {
        return status;
    }
    match (error.code.as_deref(), error.kind.as_deref()) {
        (Some("context_length_exceeded"), _) => 400,
        (_, Some("invalid_request_error")) => 400,
        (_, Some("authentication_error")) => 401,
        (_, Some("permission_error")) => 403,
        (_, Some("not_found_error")) => 404,
        (_, Some("rate_limit_error" | "rate_limit_exceeded" | "insufficient_quota")) => 429,
        _ => 500,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::images::MediaType;

    fn request(messages: &[Message], tools: &[ToolSpec], official: bool) -> Value {
        let request = CompletionRequest {
            system: "be nice",
            messages,
            tools,
            max_tokens: 100,
            cache: true,
            thinking: true,
            web_search: true,
        };
        let options = Options {
            max_completion_tokens: official,
            reasoning_effort: None,
        };
        serde_json::to_value(ChatRequest::new("m", request, options)).unwrap()
    }

    fn png(data: &str) -> Image {
        Image {
            media_type: MediaType::Png,
            source: Source::Base64(data.into()),
        }
    }

    #[test]
    fn a_request_has_the_system_prompt_tools_and_the_limit_and_no_anthropic_fields() {
        let tools = [ToolSpec {
            name: "bash".into(),
            description: "runs".into(),
            input_schema: json!({"type": "object", "properties": {"command": {"type": "string"}}}),
        }];
        let value = request(&[Message::user_text("hi")], &tools, false);
        assert_eq!(
            value,
            json!({
                "model": "m",
                "messages": [
                    {"role": "system", "content": "be nice"},
                    {"role": "user", "content": "hi"}
                ],
                "tools": [{"type": "function", "function": {
                    "name": "bash", "description": "runs",
                    "parameters": {"type": "object", "properties": {"command": {"type": "string"}}}
                }}],
                "max_tokens": 100
            })
        );
        let official = request(&[Message::user_text("hi")], &[], true);
        assert_eq!(official["max_completion_tokens"], 100);
        assert!(official.get("max_tokens").is_none(), "{official}");
        assert!(official.get("tools").is_none(), "{official}");
        let streamed = serde_json::to_value(
            ChatRequest::new(
                "m",
                CompletionRequest {
                    system: "",
                    messages: &[],
                    tools: &[],
                    max_tokens: 1,
                    cache: false,
                    thinking: false,
                    web_search: false,
                },
                Options::default(),
            )
            .streamed(),
        )
        .unwrap();
        assert_eq!(streamed["stream"], true);
        assert_eq!(streamed["stream_options"], json!({"include_usage": true}));
        assert_eq!(streamed["messages"], json!([]));
        assert!(streamed.get("reasoning_effort").is_none(), "{streamed}");
    }

    #[test]
    fn a_reasoning_effort_is_sent_and_a_summary_gets_room_to_reason() {
        let body = |effort: Option<&'static str>, thinking: bool, completion: bool| {
            let request = CompletionRequest {
                system: "",
                messages: &[],
                tools: &[],
                max_tokens: 4096,
                cache: false,
                thinking,
                web_search: false,
            };
            let options = Options {
                max_completion_tokens: completion,
                reasoning_effort: effort,
            };
            serde_json::to_value(ChatRequest::new("o", request, options)).unwrap()
        };
        // A turn: the limit as set, the effort along.
        let turn = body(Some("low"), true, true);
        assert_eq!(turn["reasoning_effort"], "low");
        assert_eq!(turn["max_completion_tokens"], 4096);
        // A summary: its limit plus room for the reasoning it cannot skip.
        let summary = body(Some("high"), false, true);
        assert_eq!(summary["reasoning_effort"], "high");
        assert_eq!(summary["max_completion_tokens"], 4096 + REASONING_ROOM);
        let summary = body(Some("medium"), false, false);
        assert_eq!(summary["max_tokens"], 4096 + REASONING_ROOM);
        // No reasoning: no room.
        let none = body(Some("none"), false, true);
        assert_eq!(none["reasoning_effort"], "none");
        assert_eq!(none["max_completion_tokens"], 4096);
        let unset = body(None, false, false);
        assert!(unset.get("reasoning_effort").is_none(), "{unset}");
        assert_eq!(unset["max_tokens"], 4096);
    }

    #[test]
    fn tool_calls_and_results_become_tool_calls_and_tool_messages() {
        let history = [
            Message::user_text("list and count"),
            Message {
                role: Role::Assistant,
                content: vec![
                    ContentBlock::Other(
                        json!({"type": "thinking", "thinking": "x", "signature": "s"}),
                    ),
                    ContentBlock::text("Sure."),
                    ContentBlock::ToolUse {
                        id: "call_1".into(),
                        name: "bash".into(),
                        input: json!({"command": "ls"}),
                    },
                    ContentBlock::ToolUse {
                        id: "call_2".into(),
                        name: "bash".into(),
                        input: Value::String("{\"command\": \"wc".into()),
                    },
                ],
            },
            Message {
                role: Role::User,
                content: vec![
                    ContentBlock::tool_result("call_1", "a\nb", false),
                    ContentBlock::tool_result("call_2", "bad input", true),
                    ContentBlock::text("and then?"),
                ],
            },
            Message {
                role: Role::Assistant,
                content: vec![ContentBlock::ToolUse {
                    id: "call_3".into(),
                    name: "read_file".into(),
                    input: json!({}),
                }],
            },
            Message {
                role: Role::User,
                content: vec![ContentBlock::tool_result("call_3", "", false)],
            },
        ];
        assert_eq!(
            messages("", &history),
            vec![
                json!({"role": "user", "content": "list and count"}),
                json!({"role": "assistant", "content": "Sure.", "tool_calls": [
                    {"id": "call_1", "type": "function",
                     "function": {"name": "bash", "arguments": "{\"command\":\"ls\"}"}},
                    // Broken arguments go back as a JSON object.
                    {"id": "call_2", "type": "function",
                     "function": {"name": "bash", "arguments": "{}"}}
                ]}),
                json!({"role": "tool", "tool_call_id": "call_1", "content": "a\nb"}),
                json!({"role": "tool", "tool_call_id": "call_2", "content": "[tool error] bad input"}),
                json!({"role": "user", "content": "and then?"}),
                json!({"role": "assistant", "content": null, "tool_calls": [
                    {"id": "call_3", "type": "function",
                     "function": {"name": "read_file", "arguments": "{}"}}
                ]}),
                json!({"role": "tool", "tool_call_id": "call_3", "content": ""}),
            ]
        );
        // A reply of only thinking has nothing to send.
        let only_thought = Message {
            role: Role::Assistant,
            content: vec![ContentBlock::Other(json!({"type": "thinking"}))],
        };
        assert!(messages("", &[only_thought]).is_empty());
    }

    #[test]
    fn images_go_as_data_urls_and_tool_result_images_in_a_user_message() {
        let history = [
            Message {
                role: Role::User,
                content: vec![
                    ContentBlock::text("what is this?"),
                    ContentBlock::Image(png("AAAA")),
                ],
            },
            Message {
                role: Role::User,
                content: vec![ContentBlock::ToolResult {
                    tool_use_id: "call_9".into(),
                    content: "read shot.png".into(),
                    is_error: false,
                    images: vec![png("BBBB")],
                }],
            },
        ];
        assert_eq!(
            messages("", &history),
            vec![
                json!({"role": "user", "content": [
                    {"type": "text", "text": "what is this?"},
                    {"type": "image_url", "image_url": {"url": "data:image/png;base64,AAAA"}}
                ]}),
                json!({"role": "tool", "tool_call_id": "call_9", "content": "read shot.png"}),
                json!({"role": "user", "content": [
                    {"type": "text", "text": "The images of tool call call_9:"},
                    {"type": "image_url", "image_url": {"url": "data:image/png;base64,BBBB"}}
                ]}),
            ]
        );
    }

    #[test]
    fn an_answer_becomes_text_and_tool_uses() {
        let answer: ChatResponse = serde_json::from_value(json!({
            "id": "chatcmpl-1", "object": "chat.completion",
            "choices": [{"index": 0, "finish_reason": "tool_calls", "message": {
                "role": "assistant", "content": "Let me look.", "refusal": null,
                "tool_calls": [
                    {"id": "call_a", "type": "function",
                     "function": {"name": "bash", "arguments": "{\"command\":\"ls\"}"}},
                    {"id": "call_b", "type": "function",
                     "function": {"name": "bash", "arguments": "{\"command\": \"wc"}},
                    {"type": "function", "function": {"name": "noop", "arguments": ""}}
                ]
            }}],
            "usage": {"prompt_tokens": 100, "completion_tokens": 7, "total_tokens": 107,
                      "prompt_tokens_details": {"cached_tokens": 60, "audio_tokens": 0}}
        }))
        .unwrap();
        let response = answer.into_response().unwrap();
        assert_eq!(response.stop_reason, StopReason::ToolUse);
        assert_eq!(response.content[0], ContentBlock::text("Let me look."));
        assert_eq!(
            response.content[1],
            ContentBlock::ToolUse {
                id: "call_a".into(),
                name: "bash".into(),
                input: json!({"command": "ls"})
            }
        );
        // Broken arguments stay text, so the tool refuses them.
        let ContentBlock::ToolUse { input, .. } = &response.content[2] else {
            panic!("{:?}", response.content);
        };
        assert_eq!(input, &Value::String("{\"command\": \"wc".into()));
        let ContentBlock::ToolUse { id, input, .. } = &response.content[3] else {
            panic!("{:?}", response.content);
        };
        assert!(id.starts_with("call_junees_"), "{id}");
        assert_eq!(input, &json!({}));
        assert_eq!(
            response.usage,
            Usage {
                input_tokens: 40,
                output_tokens: 7,
                cache_read_input_tokens: 60,
                ..Usage::default()
            }
        );
    }

    #[test]
    fn finish_reasons_map_to_stop_reasons() {
        for (reason, called, expected) in [
            (Some("stop"), false, StopReason::EndTurn),
            (Some("stop"), true, StopReason::ToolUse),
            (Some("tool_calls"), true, StopReason::ToolUse),
            (Some("length"), true, StopReason::MaxTokens),
            (Some("content_filter"), false, StopReason::Refusal),
            (None, false, StopReason::Other),
            (Some("eos"), false, StopReason::Other),
        ] {
            assert_eq!(stop_reason(reason, called, false), expected, "{reason:?}");
        }
        assert_eq!(stop_reason(Some("stop"), false, true), StopReason::Refusal);
        let refused: ChatResponse = serde_json::from_value(json!({"choices": [{
            "finish_reason": "stop",
            "message": {"content": null, "refusal": "I can't help with that."}}]}))
        .unwrap();
        let response = refused.into_response().unwrap();
        assert_eq!(response.stop_reason, StopReason::Refusal);
        assert_eq!(
            response.content,
            vec![ContentBlock::text("I can't help with that.")]
        );
        assert_eq!(response.usage, Usage::default());
        let none: ChatResponse = serde_json::from_value(json!({"choices": []})).unwrap();
        assert!(matches!(
            none.into_response(),
            Err(ProviderError::Decode(_))
        ));
    }

    #[test]
    fn usage_counts_cache_writes_and_reads_apart() {
        let usage: ApiUsage = serde_json::from_value(json!({
            "prompt_tokens": 1000, "completion_tokens": 5,
            "prompt_tokens_details": {"cached_tokens": 600, "cache_write_tokens": 300}
        }))
        .unwrap();
        let usage = Usage::from(usage);
        assert_eq!(
            (
                usage.input_tokens,
                usage.cache_creation_input_tokens,
                usage.cache_read_input_tokens,
                usage.output_tokens
            ),
            (100, 300, 600, 5)
        );
        // Counts that do not add up never go below zero.
        let odd: ApiUsage = serde_json::from_value(json!({
            "prompt_tokens": 10, "prompt_tokens_details": {"cached_tokens": 50}
        }))
        .unwrap();
        assert_eq!(Usage::from(odd).input_tokens, 0);
        let nulls: ApiUsage = serde_json::from_value(json!({
            "prompt_tokens": null, "completion_tokens": 3, "prompt_tokens_details": null
        }))
        .unwrap();
        assert_eq!(Usage::from(nulls).output_tokens, 3);
    }

    #[test]
    fn reasoning_tokens_stay_in_the_output_and_are_noted() {
        let usage: ApiUsage = serde_json::from_value(json!({
            "prompt_tokens": 1000, "completion_tokens": 900,
            "completion_tokens_details": {"reasoning_tokens": 832, "audio_tokens": 0,
                "accepted_prediction_tokens": 0, "rejected_prediction_tokens": 0}
        }))
        .unwrap();
        let usage = Usage::from(usage);
        assert_eq!((usage.output_tokens, usage.reasoning_tokens), (900, 832));
        assert_eq!(usage.context_tokens(), 1068);
        // More reasoning than output is not believed.
        let odd: ApiUsage = serde_json::from_value(json!({
            "completion_tokens": 5, "completion_tokens_details": {"reasoning_tokens": 50}
        }))
        .unwrap();
        assert_eq!(Usage::from(odd).reasoning_tokens, 5);
        let null: ApiUsage = serde_json::from_value(json!({
            "completion_tokens": 5, "completion_tokens_details": null
        }))
        .unwrap();
        assert_eq!(Usage::from(null).reasoning_tokens, 0);
    }

    #[test]
    fn error_bodies_map_to_provider_errors() {
        let overflow = ApiError::parse(
            br#"{"error": {"message": "This model's maximum context length is 128000 tokens.",
                "type": "invalid_request_error", "param": "messages",
                "code": "context_length_exceeded"}}"#,
        )
        .into_error(400);
        assert!(overflow.is_context_overflow(), "{overflow}");
        let denied = ApiError::parse(
            br#"{"error": {"message": "Incorrect API key provided", "type": "invalid_request_error",
                "code": "invalid_api_key", "param": null}}"#,
        );
        assert!(!denied.is_quota());
        let denied = denied.into_error(401);
        assert!(denied.is_auth());
        assert_eq!(
            denied.to_string(),
            "API error 401 (invalid_request_error): Incorrect API key provided"
        );
        let quota = ApiError::parse(
            br#"{"error": {"message": "You exceeded your current quota", "type": "insufficient_quota",
                "code": "insufficient_quota"}}"#,
        );
        assert!(quota.is_quota());
        let ollama = ApiError::parse(br#"{"error": "model \"llama9\" not found"}"#).into_error(404);
        assert_eq!(
            ollama.to_string(),
            "API error 404: model \"llama9\" not found"
        );
        let plain = ApiError::parse(b"Bad Gateway").into_error(502);
        assert_eq!(plain.to_string(), "API error 502: Bad Gateway");
        // A long page that is no JSON is cut, on a character boundary.
        let page = format!("<html>{}</html>", "오류".repeat(400));
        let cut = ApiError::parse(page.as_bytes()).message;
        assert!(
            cut.starts_with("<html>오류")
                && cut.ends_with(&format!("... ({} bytes in all)", page.len())),
            "{cut}"
        );
        assert_eq!(
            cut.chars().count(),
            PLAIN_ERROR_CHARS + format!("... ({} bytes in all)", page.len()).len()
        );
        let image = ApiError::parse(
            br#"{"error": {"message": "Invalid image data", "type": "invalid_request_error",
                "code": "invalid_image_format"}}"#,
        )
        .into_error(400);
        assert!(image.is_image_rejected(), "{image}");
    }

    #[test]
    fn an_error_in_place_of_the_answer_is_found() {
        let parse = |body: Value| serde_json::from_value::<ChatResponse>(body).unwrap();
        let error = parse(json!({"error": {"code": 502, "message": "Provider returned error"}}))
            .error()
            .unwrap();
        assert_eq!(stream_status(&error), 502);
        assert_eq!(error.message, "Provider returned error");
        let error = parse(json!({"error": "model is loading"})).error().unwrap();
        assert_eq!(error.message, "model is loading");
        // An answer is taken as one, whatever else the body says.
        assert!(
            parse(json!({"choices": [{"message": {"content": "hi"}}], "error": null}))
                .error()
                .is_none()
        );
        assert!(
            parse(json!({"choices": [{"message": {"content": "hi"}}], "error": {"code": 1}}))
                .error()
                .is_none()
        );
        assert!(parse(json!({"choices": []})).error().is_none());
    }

    #[test]
    fn errors_inside_a_stream_get_the_status_they_name() {
        let status = |error: Value| stream_status(&ApiError::from_value(&error));
        assert_eq!(
            status(json!({"code": 502, "message": "Provider returned error"})),
            502
        );
        assert_eq!(
            status(json!({"type": "rate_limit_error", "message": "slow"})),
            429
        );
        assert_eq!(
            status(json!({"code": "context_length_exceeded", "message": "x"})),
            400
        );
        assert_eq!(
            status(json!({"type": "server_error", "message": "oops"})),
            500
        );
        assert_eq!(status(json!({"message": "?"})), 500);
    }
}
