//! A streamed Chat Completions answer (`stream: true`): its chunks put
//! together into the answer a plain request would have returned.
//!
//! Each server-sent event's `data` is a `chat.completion.chunk` whose
//! `choices[0].delta` carries the next piece of `content` and of the
//! `tool_calls`: a call's `id` and `function.name` come in its first
//! piece, its `function.arguments` in pieces to join, and `index` says
//! which call a piece belongs to. `finish_reason` comes with the last
//! delta. With `stream_options: {include_usage: true}` one more chunk with
//! empty `choices` carries the `usage` of the whole request, and the
//! stream ends with `data: [DONE]` (the API reference, "Streaming", and
//! `ChatCompletionStreamOptions` and `ChatCompletionMessageToolCallChunk`
//! of the OpenAPI spec). An error that comes after the answer started is a
//! chunk with an `error` object.

use serde_json::Value;

use super::convert::{ApiError, ApiUsage, Call, response, stream_status};
use crate::message::Usage;
use crate::provider::{CompletionResponse, ProviderError, StreamEvent};

/// The answer so far.
#[derive(Default)]
pub(super) struct Chunks {
    text: String,
    /// Text was reported since the last [`StreamEvent::TextEnd`].
    text_open: bool,
    refused: bool,
    calls: Vec<PartialCall>,
    finish_reason: Option<String>,
    usage: Option<ApiUsage>,
}

/// A tool call whose pieces are still coming.
struct PartialCall {
    /// The `index` the stream gives it.
    index: u64,
    id: String,
    name: String,
    arguments: String,
    /// Arguments some servers send whole, as an object.
    object: Option<Value>,
}

impl Chunks {
    /// Applies the data of one event, reporting text as it comes. Returns
    /// whether it was the `[DONE]` that ends the stream.
    pub fn apply(
        &mut self,
        data: &str,
        report: &mut impl FnMut(StreamEvent),
    ) -> Result<bool, ProviderError> {
        if data.trim() == "[DONE]" {
            return Ok(true);
        }
        let chunk: Value = serde_json::from_str(data)
            .map_err(|err| ProviderError::Decode(format!("a chunk is not JSON: {err}")))?;
        if chunk["error"].is_object() {
            let error = ApiError::from_value(&chunk["error"]);
            let status = stream_status(&error);
            return Err(error.into_error(status));
        }
        if let Some(usage) = chunk.get("usage").filter(|usage| !usage.is_null()) {
            self.usage = Some(
                serde_json::from_value(usage.clone())
                    .map_err(|err| ProviderError::Decode(format!("chunk usage: {err}")))?,
            );
        }
        // Only the first choice: Junees asks for one.
        let Some(choice) = chunk["choices"]
            .as_array()
            .and_then(|choices| {
                choices
                    .iter()
                    .find(|choice| choice["index"].as_u64().unwrap_or(0) == 0)
            })
            .cloned()
        else {
            return Ok(false);
        };
        let delta = &choice["delta"];
        for (field, refusal) in [("content", false), ("refusal", true)] {
            if let Some(piece) = delta[field].as_str().filter(|piece| !piece.is_empty()) {
                self.text.push_str(piece);
                self.text_open = true;
                self.refused |= refusal;
                report(StreamEvent::TextDelta(piece.to_string()));
            }
        }
        if let Some(pieces) = delta["tool_calls"].as_array() {
            self.end_text(report);
            for piece in pieces {
                self.call_piece(piece);
            }
        }
        if let Some(reason) = choice["finish_reason"].as_str() {
            self.finish_reason = Some(reason.to_string());
        }
        Ok(false)
    }

    fn call_piece(&mut self, piece: &Value) {
        let id = piece["id"].as_str().unwrap_or_default();
        let index = match piece["index"].as_u64() {
            Some(index) => index,
            // No index: a new call when it brings an id the last one does
            // not have, else more of the last one.
            None => match self.calls.last() {
                Some(last) if id.is_empty() || last.id == id => last.index,
                Some(last) => last.index + 1,
                None => 0,
            },
        };
        let call = match self.calls.iter().position(|call| call.index == index) {
            Some(at) => &mut self.calls[at],
            None => {
                self.calls.push(PartialCall {
                    index,
                    id: String::new(),
                    name: String::new(),
                    arguments: String::new(),
                    object: None,
                });
                self.calls.last_mut().expect("just pushed")
            }
        };
        // Some servers repeat the id and name in every piece.
        if call.id.is_empty() {
            call.id = id.to_string();
        }
        let function = &piece["function"];
        if call.name.is_empty()
            && let Some(name) = function["name"].as_str()
        {
            call.name = name.to_string();
        }
        match &function["arguments"] {
            Value::String(text) => call.arguments.push_str(text),
            Value::Null => {}
            object => call.object = Some(object.clone()),
        }
    }

    /// Reports the end of the text, if text was reported.
    fn end_text(&mut self, report: &mut impl FnMut(StreamEvent)) {
        if std::mem::take(&mut self.text_open) {
            report(StreamEvent::TextEnd);
        }
    }

    /// Whether the answer is over: a stream may end without `[DONE]`
    /// after its `finish_reason`.
    pub fn finished(&self) -> bool {
        self.finish_reason.is_some()
    }

    /// The answer.
    pub fn finish(mut self, report: &mut impl FnMut(StreamEvent)) -> CompletionResponse {
        self.end_text(report);
        let mut calls = self.calls;
        calls.sort_by_key(|call| call.index);
        let calls = calls
            .into_iter()
            .enumerate()
            .map(|(at, call)| Call {
                id: call.id,
                name: call.name,
                arguments: call.object.unwrap_or(Value::String(call.arguments)),
                index: at,
            })
            .collect();
        response(
            self.text,
            calls,
            self.finish_reason.as_deref(),
            self.refused,
            self.usage.map(Usage::from).unwrap_or_default(),
        )
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;
    use crate::message::{ContentBlock, StopReason};

    fn run(chunks: &[Value]) -> (Result<CompletionResponse, ProviderError>, Vec<StreamEvent>) {
        let mut events = Vec::new();
        let mut report = |event| events.push(event);
        let mut assembled = Chunks::default();
        for chunk in chunks {
            match assembled.apply(&chunk.to_string(), &mut report) {
                Ok(_) => {}
                Err(err) => return (Err(err), events),
            }
        }
        assert!(assembled.apply("[DONE]", &mut report).unwrap());
        let response = assembled.finish(&mut report);
        (Ok(response), events)
    }

    fn delta(delta: Value) -> Value {
        json!({"object": "chat.completion.chunk", "choices": [{"index": 0, "delta": delta,
               "finish_reason": null}], "usage": null})
    }

    #[test]
    fn joins_text_and_the_argument_pieces_of_parallel_calls() {
        let chunks = [
            delta(json!({"role": "assistant", "content": ""})),
            delta(json!({"content": "Checking "})),
            delta(json!({"content": "both."})),
            delta(
                json!({"tool_calls": [{"index": 0, "id": "call_a", "type": "function",
                                         "function": {"name": "bash", "arguments": ""}}]}),
            ),
            delta(json!({"tool_calls": [{"index": 0, "function": {"arguments": "{\"comm"}}]})),
            delta(
                json!({"tool_calls": [{"index": 1, "id": "call_b", "type": "function",
                                         "function": {"name": "read_file", "arguments": "{\"pa"}}]}),
            ),
            delta(
                json!({"tool_calls": [{"index": 0, "function": {"arguments": "and\": \"ls\"}"}}]}),
            ),
            delta(
                json!({"tool_calls": [{"index": 1, "function": {"arguments": "th\": \"한글.txt\"}"}}]}),
            ),
            json!({"choices": [{"index": 0, "delta": {}, "finish_reason": "tool_calls"}]}),
            json!({"choices": [], "usage": {"prompt_tokens": 50, "completion_tokens": 20,
                   "prompt_tokens_details": {"cached_tokens": 10}}}),
        ];
        let (response, events) = run(&chunks);
        let response = response.unwrap();
        assert_eq!(
            events,
            vec![
                StreamEvent::TextDelta("Checking ".into()),
                StreamEvent::TextDelta("both.".into()),
                StreamEvent::TextEnd,
            ]
        );
        assert_eq!(
            response.content,
            vec![
                ContentBlock::text("Checking both."),
                ContentBlock::ToolUse {
                    id: "call_a".into(),
                    name: "bash".into(),
                    input: json!({"command": "ls"})
                },
                ContentBlock::ToolUse {
                    id: "call_b".into(),
                    name: "read_file".into(),
                    input: json!({"path": "한글.txt"})
                },
            ]
        );
        assert_eq!(response.stop_reason, StopReason::ToolUse);
        assert_eq!(
            (
                response.usage.input_tokens,
                response.usage.cache_read_input_tokens,
                response.usage.output_tokens
            ),
            (40, 10, 20)
        );
    }

    #[test]
    fn whole_calls_without_an_index_and_repeated_names() {
        // As some local servers send them: each call whole, no index.
        let chunks = [
            delta(json!({"tool_calls": [
                {"id": "c1", "function": {"name": "bash", "arguments": {"command": "pwd"}}},
            ]})),
            delta(json!({"tool_calls": [
                {"id": "c2", "function": {"name": "bash", "arguments": "{\"command\": \"id\"}"}},
            ]})),
            delta(json!({"tool_calls": [{"index": 5, "id": "c3", "function": {"name": "x"}},]})),
            delta(
                json!({"tool_calls": [{"index": 5, "id": "c3", "function": {"name": "x",
                                         "arguments": "{}"}}]}),
            ),
            json!({"choices": [{"index": 0, "delta": {}, "finish_reason": "stop"}]}),
        ];
        let (response, events) = run(&chunks);
        let response = response.unwrap();
        assert!(events.is_empty(), "{events:?}");
        let calls: Vec<(String, String, Value)> = response
            .content
            .into_iter()
            .map(|block| match block {
                ContentBlock::ToolUse { id, name, input } => (id, name, input),
                other => panic!("{other:?}"),
            })
            .collect();
        assert_eq!(
            calls,
            vec![
                ("c1".into(), "bash".into(), json!({"command": "pwd"})),
                ("c2".into(), "bash".into(), json!({"command": "id"})),
                ("c3".into(), "x".into(), json!({})),
            ]
        );
        assert_eq!(response.stop_reason, StopReason::ToolUse);
        assert_eq!(response.usage, Usage::default());
    }

    #[test]
    fn an_error_chunk_is_an_api_error() {
        let chunks = [
            delta(json!({"content": "Hal"})),
            json!({"error": {"code": 502, "message": "Provider returned error"}}),
        ];
        let (response, events) = run(&chunks);
        let err = response.err().unwrap();
        assert!(
            matches!(err, ProviderError::Api { status: 502, .. }),
            "{err}"
        );
        assert_eq!(events, vec![StreamEvent::TextDelta("Hal".into())]);
        let mut chunks = Chunks::default();
        let err = chunks.apply("{not json", &mut |_| {}).unwrap_err();
        assert!(matches!(err, ProviderError::Decode(_)), "{err}");
    }

    #[test]
    fn a_cut_off_answer_keeps_its_text_and_says_so() {
        let chunks = [
            delta(json!({"content": "Half an ans"})),
            delta(
                json!({"tool_calls": [{"index": 0, "id": "c", "function": {"name": "write_file",
                                         "arguments": "{\"path\": \"a\", \"content\": \"..."}}]}),
            ),
            json!({"choices": [{"index": 0, "delta": {}, "finish_reason": "length"}]}),
        ];
        let (response, _) = run(&chunks);
        let response = response.unwrap();
        assert_eq!(response.stop_reason, StopReason::MaxTokens);
        assert_eq!(response.content[0], ContentBlock::text("Half an ans"));
        let mut chunks = Chunks::default();
        assert!(!chunks.finished());
        chunks
            .apply(
                &json!({"choices": [{"index": 0, "delta": {}, "finish_reason": "stop"}]})
                    .to_string(),
                &mut |_| {},
            )
            .unwrap();
        assert!(chunks.finished());
    }
}
