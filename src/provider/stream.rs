//! Server-sent events of the Messages API (`stream: true`): splitting the
//! byte stream into events and assembling the events into the response a
//! plain request would have returned.
//!
//! Event flow (platform.claude.com/docs/en/build-with-claude/streaming):
//! `message_start` (the message with empty `content` and the input usage),
//! then per content block `content_block_start`, `content_block_delta`s and
//! `content_block_stop`, then `message_delta`s (stop reason, cumulative
//! usage) and `message_stop`. `ping` and unknown events may come anywhere;
//! an `error` event ends the stream.

use serde::Deserialize;
use serde_json::Value;

use super::{CompletionResponse, ProviderError, StreamEvent, server_tool_use};
use crate::message::{ContentBlock, ServerToolUsage, StopReason, Usage};

/// One event of the stream: its `event:` name and `data:` lines.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct SseEvent {
    pub event: String,
    pub data: String,
}

/// Splits a `text/event-stream` body into events. The body may arrive in
/// any pieces: an event, a line or a UTF-8 character can span chunks.
#[derive(Default)]
pub(crate) struct SseParser {
    line: Vec<u8>,
    /// The last line ended with `\r`, so a `\n` right after it is part of
    /// that line break.
    after_cr: bool,
    event: String,
    data: String,
    has_data: bool,
}

impl SseParser {
    /// Feeds the next piece of the body and returns the events it
    /// completed.
    pub fn push(&mut self, chunk: &[u8]) -> Result<Vec<SseEvent>, String> {
        let mut events = Vec::new();
        for &byte in chunk {
            let after_cr = std::mem::take(&mut self.after_cr);
            match byte {
                b'\n' if after_cr => {}
                b'\n' | b'\r' => {
                    self.after_cr = byte == b'\r';
                    let line = std::mem::take(&mut self.line);
                    let line = String::from_utf8(line)
                        .map_err(|_| "the event stream is not valid UTF-8".to_string())?;
                    if let Some(event) = self.line(&line) {
                        events.push(event);
                    }
                }
                _ => self.line.push(byte),
            }
        }
        Ok(events)
    }

    /// Handles one line as the SSE format says; a blank line ends an event.
    fn line(&mut self, line: &str) -> Option<SseEvent> {
        if line.is_empty() {
            let event = std::mem::take(&mut self.event);
            let mut data = std::mem::take(&mut self.data);
            if !std::mem::take(&mut self.has_data) {
                return None;
            }
            data.pop(); // the `\n` after the last data line
            return Some(SseEvent { event, data });
        }
        let line = line.strip_prefix('\u{feff}').unwrap_or(line);
        if line.starts_with(':') {
            return None; // a comment
        }
        let (field, value) = line.split_once(':').unwrap_or((line, ""));
        let value = value.strip_prefix(' ').unwrap_or(value);
        match field {
            "event" => self.event = value.to_string(),
            "data" => {
                self.data.push_str(value);
                self.data.push('\n');
                self.has_data = true;
            }
            _ => {} // id, retry and unknown fields
        }
        None
    }
}

/// Builds the response from the events' data, in order.
#[derive(Default)]
pub(crate) struct Assembler {
    started: bool,
    /// Each block as the JSON object the API would have sent, by index.
    blocks: Vec<Value>,
    /// The `input_json_delta` pieces of each block, joined.
    inputs: Vec<Option<String>>,
    usage: Usage,
    stop_reason: Option<StopReason>,
}

/// Usage counts of a `message_delta`. They are cumulative, so the ones it
/// has replace the counts so far; the ones it leaves out stay.
#[derive(Deserialize)]
struct DeltaUsage {
    input_tokens: Option<u64>,
    output_tokens: Option<u64>,
    cache_creation_input_tokens: Option<u64>,
    cache_read_input_tokens: Option<u64>,
    server_tool_use: Option<ServerToolUsage>,
}

impl Assembler {
    /// Applies the data of one event, reporting text, thinking and server
    /// tool blocks to `report` as they come. Returns whether it was
    /// `message_stop`.
    pub fn apply(
        &mut self,
        data: &str,
        report: &mut impl FnMut(StreamEvent),
    ) -> Result<bool, ProviderError> {
        let event: Value = serde_json::from_str(data)
            .map_err(|err| decode(format!("an event is not JSON: {err}")))?;
        let kind = event.get("type").and_then(Value::as_str).unwrap_or("");
        if kind == "error" {
            return Err(stream_error(&event["error"]));
        }
        if !self.started && kind.starts_with("content_block_") {
            return Err(decode(format!("{kind} before message_start")));
        }
        match kind {
            "message_start" => self.start(&event["message"])?,
            "content_block_start" => {
                let index = index_of(&event)?;
                if index != self.blocks.len() {
                    return Err(decode(format!(
                        "content block {index} started after {} blocks",
                        self.blocks.len()
                    )));
                }
                let block = event["content_block"].clone();
                let Some(block_type) = block.get("type").and_then(Value::as_str) else {
                    return Err(decode(format!("content block {index} has no type")));
                };
                match block_type {
                    "text" => {
                        let text = block["text"].as_str().unwrap_or("");
                        if !text.is_empty() {
                            report(StreamEvent::TextDelta(text.to_string()));
                        }
                    }
                    "thinking" | "redacted_thinking" => report(StreamEvent::Thinking),
                    _ => {}
                }
                self.blocks.push(block);
                self.inputs.push(None);
            }
            "content_block_delta" => {
                let index = index_of(&event)?;
                let Some(block) = self.blocks.get_mut(index) else {
                    return Err(decode(format!(
                        "a delta for content block {index}, not started"
                    )));
                };
                let delta = &event["delta"];
                let text = |field: &str| delta[field].as_str().unwrap_or("");
                match delta["type"].as_str().unwrap_or("") {
                    "text_delta" => {
                        append(block, "text", text("text"));
                        report(StreamEvent::TextDelta(text("text").to_string()));
                    }
                    "input_json_delta" => self.inputs[index]
                        .get_or_insert_default()
                        .push_str(text("partial_json")),
                    "thinking_delta" => append(block, "thinking", text("thinking")),
                    "signature_delta" => block["signature"] = delta["signature"].clone(),
                    "citations_delta" => {
                        let citation = delta["citation"].clone();
                        match block.get_mut("citations") {
                            Some(Value::Array(citations)) => citations.push(citation),
                            _ => block["citations"] = Value::Array(vec![citation]),
                        }
                    }
                    // A delta type this version does not know; new ones
                    // may be added (the API's versioning policy).
                    _ => {}
                }
            }
            "content_block_stop" => {
                let index = index_of(&event)?;
                match self.blocks.get(index) {
                    Some(block) if block["type"] == "text" => report(StreamEvent::TextEnd),
                    // Reported once its input is whole.
                    Some(block) if block["type"] == "server_tool_use" => {
                        let mut block = block.clone();
                        if let Some(input) = self.inputs[index]
                            .as_deref()
                            .and_then(|json| serde_json::from_str::<Value>(json).ok())
                        {
                            block["input"] = input;
                        }
                        report(server_tool_use(&block));
                    }
                    Some(_) => {}
                    None => {
                        return Err(decode(format!(
                            "content block {index} stopped, not started"
                        )));
                    }
                }
            }
            "message_delta" => {
                if let Some(reason) = event["delta"].get("stop_reason").filter(|r| !r.is_null()) {
                    self.stop_reason = Some(stop_reason(reason)?);
                }
                if let Some(usage) = event.get("usage").filter(|u| !u.is_null()) {
                    let usage: DeltaUsage = serde_json::from_value(usage.clone())
                        .map_err(|err| decode(format!("message_delta usage: {err}")))?;
                    let total = &mut self.usage;
                    for (count, field) in [
                        (usage.input_tokens, &mut total.input_tokens),
                        (usage.output_tokens, &mut total.output_tokens),
                        (
                            usage.cache_creation_input_tokens,
                            &mut total.cache_creation_input_tokens,
                        ),
                        (
                            usage.cache_read_input_tokens,
                            &mut total.cache_read_input_tokens,
                        ),
                    ] {
                        if let Some(count) = count {
                            *field = count;
                        }
                    }
                    if let Some(server) = usage.server_tool_use {
                        total.server_tool_use = server;
                    }
                }
            }
            "message_stop" => {
                if !self.started {
                    return Err(decode("message_stop before message_start".into()));
                }
                return Ok(true);
            }
            // ping, and event types added after this version.
            _ => {}
        }
        Ok(false)
    }

    fn start(&mut self, message: &Value) -> Result<(), ProviderError> {
        if self.started {
            return Err(decode("a second message_start".into()));
        }
        self.started = true;
        if let Some(usage) = message.get("usage").filter(|u| !u.is_null()) {
            self.usage = serde_json::from_value(usage.clone())
                .map_err(|err| decode(format!("message_start usage: {err}")))?;
        }
        if let Some(reason) = message.get("stop_reason").filter(|r| !r.is_null()) {
            self.stop_reason = Some(stop_reason(reason)?);
        }
        Ok(())
    }

    /// The response, once `message_stop` came.
    pub fn finish(self) -> Result<CompletionResponse, ProviderError> {
        let stop_reason = self.stop_reason.unwrap_or(StopReason::Other);
        let mut content = Vec::with_capacity(self.blocks.len());
        for (mut block, input) in self.blocks.into_iter().zip(self.inputs) {
            if let Some(json) = input.filter(|json| !json.trim().is_empty()) {
                match serde_json::from_str::<Value>(&json) {
                    Ok(input) => block["input"] = input,
                    // Cut off by the token limit: the agent does not run
                    // such a call, so the input it started with will do.
                    Err(_) if stop_reason == StopReason::MaxTokens => {}
                    Err(err) => return Err(decode(format!("tool input is not JSON: {err}"))),
                }
            }
            content.push(ContentBlock::try_from(block).map_err(decode)?);
        }
        Ok(CompletionResponse {
            content,
            stop_reason,
            usage: self.usage,
        })
    }
}

fn decode(message: String) -> ProviderError {
    ProviderError::Decode(message)
}

fn index_of(event: &Value) -> Result<usize, ProviderError> {
    event["index"]
        .as_u64()
        .and_then(|index| usize::try_from(index).ok())
        .ok_or_else(|| decode(format!("{} without an index", event["type"])))
}

fn stop_reason(value: &Value) -> Result<StopReason, ProviderError> {
    serde_json::from_value(value.clone()).map_err(|err| decode(format!("stop_reason: {err}")))
}

/// Appends to a string field of a block, which was checked to be an object.
fn append(block: &mut Value, field: &str, piece: &str) {
    match block.get_mut(field) {
        Some(Value::String(text)) => text.push_str(piece),
        _ => block[field] = Value::String(piece.to_string()),
    }
}

/// An `error` event as the error a plain request would have returned: the
/// docs map `overloaded_error` to 529 and so on, so retries and the error
/// checks treat both alike. Unknown types count as server errors.
fn stream_error(error: &Value) -> ProviderError {
    let kind = error["type"].as_str().unwrap_or("api_error").to_string();
    let status = match kind.as_str() {
        "invalid_request_error" => 400,
        "authentication_error" => 401,
        "billing_error" => 402,
        "permission_error" => 403,
        "not_found_error" => 404,
        "request_too_large" => 413,
        "rate_limit_error" => 429,
        "timeout_error" => 504,
        "overloaded_error" => 529,
        _ => 500,
    };
    ProviderError::Api {
        status,
        kind: Some(kind),
        message: error["message"].as_str().unwrap_or("").to_string(),
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn parse_in_pieces(body: &[u8], size: usize) -> Vec<SseEvent> {
        let mut parser = SseParser::default();
        body.chunks(size)
            .flat_map(|chunk| parser.push(chunk).unwrap())
            .collect()
    }

    fn sse(event: &str, data: &str) -> SseEvent {
        SseEvent {
            event: event.into(),
            data: data.into(),
        }
    }

    #[test]
    fn events_survive_any_split_and_any_line_ending() {
        let body = "event: ping\ndata: {\"type\": \"ping\"}\n\n\
                    : a comment\r\nevent:content_block_delta\r\ndata: {\"text\": \"안녕\"}\r\n\r\n\
                    data: first\rdata:  second\r\rid: 7\nretry: 10\n\n\
                    event: lonely\n\n\
                    data\n\n\
                    event: unfinished\ndata: {}\n";
        let expected = vec![
            sse("ping", "{\"type\": \"ping\"}"),
            sse("content_block_delta", "{\"text\": \"안녕\"}"),
            sse("", "first\n second"),
            sse("", ""),
        ];
        // Every split point, including inside the Hangul and inside \r\n.
        for size in 1..=body.len() {
            assert_eq!(parse_in_pieces(body.as_bytes(), size), expected, "{size}");
        }
    }

    #[test]
    fn invalid_utf8_is_an_error() {
        let mut parser = SseParser::default();
        assert!(parser.push(b"data: \xff\n\n").is_err());
    }

    /// The tool-use example of the streaming docs, with its ping.
    const DOCS_TOOL_USE: &str = r#"event: message_start
data: {"type":"message_start","message":{"id":"msg_014p7gG3wDgGV9EUtLvnow3U","type":"message","role":"assistant","model":"claude-opus-5","stop_sequence":null,"usage":{"input_tokens":472,"output_tokens":2},"content":[],"stop_reason":null}}

event: content_block_start
data: {"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}

event: ping
data: {"type": "ping"}

event: content_block_delta
data: {"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"Okay"}}

event: content_block_delta
data: {"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":", let's check:"}}

event: content_block_stop
data: {"type":"content_block_stop","index":0}

event: content_block_start
data: {"type":"content_block_start","index":1,"content_block":{"type":"tool_use","id":"toolu_01T1x1fJ34qAmk2tNTrN7Up6","name":"get_weather","input":{}}}

event: content_block_delta
data: {"type":"content_block_delta","index":1,"delta":{"type":"input_json_delta","partial_json":""}}

event: content_block_delta
data: {"type":"content_block_delta","index":1,"delta":{"type":"input_json_delta","partial_json":"{\"location\":"}}

event: content_block_delta
data: {"type":"content_block_delta","index":1,"delta":{"type":"input_json_delta","partial_json":" \"San Francisc"}}

event: content_block_delta
data: {"type":"content_block_delta","index":1,"delta":{"type":"input_json_delta","partial_json":"o, CA\"}"}}

event: content_block_stop
data: {"type":"content_block_stop","index":1}

event: message_delta
data: {"type":"message_delta","delta":{"stop_reason":"tool_use","stop_sequence":null},"usage":{"output_tokens":89}}

event: message_stop
data: {"type":"message_stop"}

"#;

    /// Runs the data of `body`'s events through an assembler.
    fn assemble(body: &str) -> (Result<CompletionResponse, ProviderError>, Vec<StreamEvent>) {
        let mut reported = Vec::new();
        let mut assembler = Assembler::default();
        let mut report = |event| reported.push(event);
        for event in SseParser::default().push(body.as_bytes()).unwrap() {
            match assembler.apply(&event.data, &mut report) {
                Ok(true) => return (assembler.finish(), reported),
                Ok(false) => {}
                Err(err) => return (Err(err), reported),
            }
        }
        panic!("no message_stop in {body}");
    }

    #[test]
    fn assembles_the_docs_tool_use_example() {
        let (response, reported) = assemble(DOCS_TOOL_USE);
        let response = response.unwrap();
        assert_eq!(
            response.content,
            vec![
                ContentBlock::text("Okay, let's check:"),
                ContentBlock::ToolUse {
                    id: "toolu_01T1x1fJ34qAmk2tNTrN7Up6".into(),
                    name: "get_weather".into(),
                    input: json!({"location": "San Francisco, CA"}),
                },
            ]
        );
        assert_eq!(response.stop_reason, StopReason::ToolUse);
        // The input count of message_start, the output of message_delta:
        // not 2 + 89.
        assert_eq!(
            response.usage,
            Usage {
                input_tokens: 472,
                output_tokens: 89,
                ..Default::default()
            }
        );
        assert_eq!(
            reported,
            [
                StreamEvent::TextDelta("Okay".into()),
                StreamEvent::TextDelta(", let's check:".into()),
                StreamEvent::TextEnd,
            ]
        );
    }

    fn data(events: &[Value]) -> String {
        events
            .iter()
            .map(|event| {
                format!(
                    "event: {}\ndata: {event}\n\n",
                    event["type"].as_str().unwrap()
                )
            })
            .collect()
    }

    fn start(usage: Value) -> Value {
        json!({"type": "message_start", "message": {"id": "m", "type": "message",
               "role": "assistant", "content": [], "stop_reason": null, "usage": usage}})
    }

    fn stop(reason: &str, usage: Value) -> [Value; 2] {
        [
            json!({"type": "message_delta", "delta": {"stop_reason": reason}, "usage": usage}),
            json!({"type": "message_stop"}),
        ]
    }

    #[test]
    fn thinking_signature_and_unknown_blocks_come_out_as_sent() {
        let mut events = vec![
            start(json!({"input_tokens": 10, "cache_read_input_tokens": 900, "output_tokens": 1})),
            json!({"type": "content_block_start", "index": 0,
                   "content_block": {"type": "thinking", "thinking": "", "signature": ""}}),
            json!({"type": "content_block_delta", "index": 0,
                   "delta": {"type": "thinking_delta", "thinking": "먼저 "}}),
            json!({"type": "content_block_delta", "index": 0,
                   "delta": {"type": "thinking_delta", "thinking": "생각한다."}}),
            json!({"type": "content_block_delta", "index": 0,
                   "delta": {"type": "signature_delta", "signature": "EqQBCgIYAh=="}}),
            json!({"type": "content_block_stop", "index": 0}),
            json!({"type": "content_block_start", "index": 1,
                   "content_block": {"type": "redacted_thinking", "data": "EmwKAhgB"}}),
            json!({"type": "content_block_stop", "index": 1}),
            json!({"type": "content_block_start", "index": 2, "content_block":
                   {"type": "server_tool_use", "id": "srvtoolu_1", "name": "web_search", "input": {}}}),
            json!({"type": "content_block_delta", "index": 2,
                   "delta": {"type": "input_json_delta", "partial_json": "{\"query\": \"ru"}}),
            json!({"type": "content_block_delta", "index": 2,
                   "delta": {"type": "input_json_delta", "partial_json": "st\"}"}}),
            json!({"type": "content_block_stop", "index": 2}),
            json!({"type": "content_block_start", "index": 3, "content_block":
                   {"type": "web_search_tool_result", "tool_use_id": "srvtoolu_1",
                    "content": [{"type": "web_search_result", "url": "https://x"}]}}),
            json!({"type": "content_block_delta", "index": 3,
                   "delta": {"type": "brand_new_delta", "whatever": 1}}),
            json!({"type": "content_block_stop", "index": 3}),
            json!({"type": "something_new", "index": 9}),
        ];
        events.extend(stop(
            "end_turn",
            json!({"input_tokens": 10, "cache_read_input_tokens": 900, "output_tokens": 70,
                   "server_tool_use": {"web_search_requests": 1}}),
        ));
        let (response, reported) = assemble(&data(&events));
        let response = response.unwrap();
        assert_eq!(
            response.content,
            vec![
                ContentBlock::Other(json!({"type": "thinking", "thinking": "먼저 생각한다.",
                                           "signature": "EqQBCgIYAh=="})),
                ContentBlock::Other(json!({"type": "redacted_thinking", "data": "EmwKAhgB"})),
                ContentBlock::Other(json!({"type": "server_tool_use", "id": "srvtoolu_1",
                                           "name": "web_search", "input": {"query": "rust"}})),
                ContentBlock::Other(json!({"type": "web_search_tool_result",
                    "tool_use_id": "srvtoolu_1",
                    "content": [{"type": "web_search_result", "url": "https://x"}]})),
            ]
        );
        assert_eq!(
            response.usage,
            Usage {
                input_tokens: 10,
                output_tokens: 70,
                cache_read_input_tokens: 900,
                cache_creation_input_tokens: 0,
                reasoning_tokens: 0,
                server_tool_use: ServerToolUsage {
                    web_search_requests: 1
                },
            }
        );
        assert_eq!(
            reported,
            [
                StreamEvent::Thinking,
                StreamEvent::Thinking,
                // Once its input was whole.
                StreamEvent::ServerToolUse {
                    name: "web_search".into(),
                    input: json!({"query": "rust"}),
                },
            ]
        );
    }

    #[test]
    fn citations_stay_on_their_text_and_search_counts_are_cumulative() {
        let citation = |url: &str| {
            json!({"type": "web_search_result_location", "url": url, "title": "T",
                   "encrypted_index": "Eo8BCioIAhgB", "cited_text": "Rust 1.95"})
        };
        let mut events = vec![
            start(json!({"input_tokens": 5, "server_tool_use": {"web_search_requests": 0}})),
            json!({"type": "content_block_start", "index": 0,
                   "content_block": {"type": "text", "text": "", "citations": null}}),
            json!({"type": "content_block_delta", "index": 0,
                   "delta": {"type": "citations_delta", "citation": citation("https://a")}}),
            json!({"type": "content_block_delta", "index": 0,
                   "delta": {"type": "text_delta", "text": "Rust 1.95 is out."}}),
            json!({"type": "content_block_delta", "index": 0,
                   "delta": {"type": "citations_delta", "citation": citation("https://b")}}),
            json!({"type": "content_block_stop", "index": 0}),
        ];
        events.extend(stop(
            "end_turn",
            json!({"output_tokens": 9, "server_tool_use": {"web_search_requests": 2}}),
        ));
        let (response, reported) = assemble(&data(&events));
        let response = response.unwrap();
        assert_eq!(
            response.content,
            [ContentBlock::Text {
                text: "Rust 1.95 is out.".into(),
                citations: Some(json!([citation("https://a"), citation("https://b")])),
            }]
        );
        assert_eq!(response.usage.web_searches(), 2);
        assert_eq!(response.usage.input_tokens, 5);
        assert_eq!(
            reported,
            [
                StreamEvent::TextDelta("Rust 1.95 is out.".into()),
                StreamEvent::TextEnd
            ]
        );
    }

    #[test]
    fn a_tool_input_cut_off_by_the_token_limit_stays_empty() {
        let mut events = vec![
            start(json!({"input_tokens": 3})),
            json!({"type": "content_block_start", "index": 0, "content_block":
                   {"type": "tool_use", "id": "t", "name": "write_file", "input": {}}}),
            json!({"type": "content_block_delta", "index": 0,
                   "delta": {"type": "input_json_delta", "partial_json": "{\"path\": \"a\", "}}),
            json!({"type": "content_block_stop", "index": 0}),
        ];
        events.extend(stop("max_tokens", json!({"output_tokens": 8})));
        let response = assemble(&data(&events)).0.unwrap();
        assert_eq!(response.stop_reason, StopReason::MaxTokens);
        assert!(
            matches!(&response.content[0], ContentBlock::ToolUse { input, .. } if *input == json!({}))
        );

        // Any other stop reason with broken input is an error.
        let last = events.len() - 2;
        events[last]["delta"]["stop_reason"] = json!("tool_use");
        let err = assemble(&data(&events)).0.unwrap_err();
        assert!(err.to_string().contains("tool input is not JSON"), "{err}");
    }

    #[test]
    fn error_events_become_typed_api_errors() {
        let events = [
            start(json!({"input_tokens": 3})),
            json!({"type": "error", "error": {"type": "overloaded_error", "message": "Overloaded"}}),
        ];
        let err = assemble(&data(&events)).0.unwrap_err();
        assert_eq!(
            err.to_string(),
            "API error 529 (overloaded_error): Overloaded"
        );
        let too_long = stream_error(&json!({"type": "invalid_request_error",
                                            "message": "prompt is too long: 1 > 0"}));
        assert!(too_long.is_context_overflow());
        let unknown = stream_error(&json!({"type": "brand_new_error", "message": "?"}));
        assert!(matches!(unknown, ProviderError::Api { status: 500, .. }));
    }

    #[test]
    fn out_of_order_events_are_decode_errors() {
        let block = |index: u64| {
            json!({"type": "content_block_start", "index": index,
                   "content_block": {"type": "text", "text": ""}})
        };
        let delta = json!({"type": "content_block_delta", "index": 0,
                           "delta": {"type": "text_delta", "text": "x"}});
        for events in [
            vec![block(0)],
            vec![start(json!({})), block(1)],
            vec![start(json!({})), delta.clone()],
            vec![start(json!({})), start(json!({}))],
            vec![
                start(json!({})),
                json!({"type": "content_block_stop", "index": 0}),
            ],
            vec![
                start(json!({})),
                json!({"type": "content_block_start", "index": 0,
                                          "content_block": {"text": "no type"}}),
            ],
        ] {
            let err = assemble(&data(&events)).0.unwrap_err();
            assert!(matches!(err, ProviderError::Decode(_)), "{events:?}: {err}");
        }
        let mut assembler = Assembler::default();
        assert!(matches!(
            assembler.apply("not json", &mut |_| {}),
            Err(ProviderError::Decode(_))
        ));
    }
}
