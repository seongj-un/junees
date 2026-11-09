//! Context management through the real Anthropic client: old tool output
//! is cleared and the conversation compacted once the usage the fake API
//! reports crosses the threshold, and the session journal replays to the
//! same history.

use junees::agent::{Agent, AgentOptions};
use junees::context::SUMMARY_SYSTEM;
use junees::provider::anthropic::AnthropicProvider;
use junees::session::{CLEARED, Session, SessionWriter};
use junees::tools::{ToolRegistry, ToolSettings};
use serde_json::{Value, json};

mod common;
use common::{body_of, fake_api};

fn reply(content: Value, stop_reason: &str, usage: Value) -> Value {
    json!({
        "id": "msg_1", "type": "message", "role": "assistant", "model": "m",
        "content": content,
        "stop_reason": stop_reason,
        "usage": usage
    })
}

fn tool_call(id: &str, name: &str, input: Value) -> Value {
    reply(
        json!([{"type": "tool_use", "id": id, "name": name, "input": input}]),
        "tool_use",
        json!({"input_tokens": 10, "output_tokens": 5}),
    )
}

fn answer(text: &str, usage: Value) -> Value {
    reply(json!([{"type": "text", "text": text}]), "end_turn", usage)
}

/// Usage that puts the context at 900 of 1000 tokens, mostly cache reads.
fn big() -> Value {
    json!({"input_tokens": 10, "cache_read_input_tokens": 880, "output_tokens": 10})
}

fn small() -> Value {
    json!({"input_tokens": 20, "output_tokens": 2})
}

#[tokio::test]
async fn clears_then_compacts_over_the_threshold_and_replays() {
    let (url, server) = fake_api(
        [
            // "first": four tool calls, then an answer.
            tool_call("toolu_1", "bash", json!({"command": "echo junees-a"})),
            tool_call("toolu_2", "nope", json!({})),
            tool_call("toolu_3", "nope", json!({})),
            tool_call("toolu_4", "nope", json!({})),
            answer("one", small()),
            // "second" goes over the threshold.
            answer("two", big()),
            // "third": old tool output is cleared first, still over.
            answer("three", big()),
            // "fourth": compacted first, then answered.
            answer("SUMMARY", json!({"input_tokens": 300, "output_tokens": 20})),
            answer("four", small()),
        ]
        .into_iter()
        .map(|body| (200, body))
        .collect(),
    )
    .await;

    let home = tempfile::tempdir().unwrap();
    let (meta, writer) = SessionWriter::create(home.path(), home.path(), "test-model").unwrap();
    let path = writer.path().to_path_buf();
    let provider = AnthropicProvider::new("sk-test".into(), url, "test-model".into()).unwrap();
    let mut agent = Agent::new(
        Box::new(provider),
        ToolRegistry::with_defaults(&ToolSettings::default()),
        AgentOptions {
            system: "sys".into(),
            max_tokens: 64,
            max_turns: 10,
            context_window: 1000,
            compact_threshold: 0.8,
        },
    )
    .with_journal(Box::new(writer));
    for input in ["first", "second", "third", "fourth"] {
        agent.run(input, |_| {}).await.unwrap();
    }
    let requests: Vec<Value> = server.await.unwrap().iter().map(|r| body_of(r)).collect();
    assert_eq!(requests.len(), 9);

    // Before "third": the oldest of four tool results is cleared, keeping
    // its id; the last three, including an error, are untouched.
    let messages = &requests[6]["messages"];
    assert_eq!(
        messages[2]["content"][0],
        json!({"type": "tool_result", "tool_use_id": "toolu_1", "content": CLEARED})
    );
    assert_eq!(
        requests[5]["messages"][2]["content"][0]["content"],
        "junees-a\n[exit code 0]"
    );
    assert_eq!(
        messages[4]["content"][0],
        json!({"type": "tool_result", "tool_use_id": "toolu_2",
               "content": "unknown tool `nope`", "is_error": true})
    );

    // Before "fourth": a summary call without tools ...
    let summary = &requests[7];
    assert!(summary.get("tools").is_none(), "{summary}");
    assert_eq!(summary["system"][0]["text"], SUMMARY_SYSTEM);
    assert_eq!(summary["max_tokens"], 4096);
    assert_eq!(summary["messages"].as_array().unwrap().len(), 1);
    // It is sent once, so nothing in it is marked for caching.
    assert!(!summary.to_string().contains("cache_control"), "{summary}");
    let asked = summary["messages"][0]["content"][0]["text"]
        .as_str()
        .unwrap();
    assert!(asked.contains("[user]\nsecond"), "{asked}");
    assert!(!asked.contains("third"), "{asked}");

    // ... whose text leads the kept tail.
    let last = &requests[8];
    let messages = last["messages"].as_array().unwrap();
    assert_eq!(messages.len(), 3);
    assert_eq!(
        messages[0]["content"][0]["text"],
        "[Summary of the earlier conversation]\nSUMMARY"
    );
    assert_eq!(messages[0]["content"][1]["text"], "third");
    assert_eq!(messages[1]["content"][0]["text"], "three");
    assert_eq!(messages[2]["content"][0]["text"], "fourth");
    assert_eq!(
        messages[2]["content"][0]["cache_control"]["type"],
        "ephemeral"
    );
    // The last tool carries the mark.
    let tools = last["tools"].as_array().unwrap();
    assert_eq!(
        tools.last().unwrap()["cache_control"]["type"],
        "ephemeral",
        "{last}"
    );

    let usage = agent.usage();
    assert_eq!(usage.cache_read_input_tokens, 2 * 880);
    assert_eq!(usage.input_tokens, 4 * 10 + 20 + 10 + 10 + 300 + 20);
    assert_eq!(agent.context_tokens(), 22);

    // The journal replays to exactly this state.
    let (messages, context_tokens) = (agent.messages().to_vec(), agent.context_tokens());
    agent.reset(None);
    let session = Session::load(&path).unwrap();
    assert_eq!(session.meta.id, meta.id);
    assert_eq!(session.meta.format, 2);
    assert_eq!(session.transcript.messages, messages);
    assert_eq!(session.transcript.usage, usage);
    assert_eq!(session.transcript.context_tokens, context_tokens);
}
