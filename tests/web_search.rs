//! The API's web search tool through the real client and binary, against a
//! fake API server: the tool in the request, a turn the API pauses, and
//! search blocks and citations sent back as they came.

use std::process::{Output, Stdio};
use std::time::Duration;

use junees::agent::{Agent, AgentOptions, Event};
use junees::provider::anthropic::AnthropicProvider;
use junees::tools::ToolRegistry;
use junees::web_search::{self, WebSearch};
use serde_json::{Value, json};

mod common;
use common::{Served, body_of, fake_stream_api, sse_stream, text_reply};

fn citation(url: &str, title: &str) -> Value {
    json!({"type": "web_search_result_location", "url": url, "title": title,
           "encrypted_index": "Eo8BCioIAhgBIiQyYjQ0OWJmZi1lNm", "cited_text": "Rust 1.95.0"})
}

/// A reply the API paused while it searched: a search with its results,
/// and text citing one of them.
fn paused_reply() -> Value {
    json!({
        "id": "msg_1", "type": "message", "role": "assistant", "model": "m",
        "content": [
            {"type": "text", "text": "최신 소식을 찾아볼게요."},
            {"type": "server_tool_use", "id": "srvtoolu_01", "name": "web_search",
             "input": {"query": "rust release"}},
            {"type": "web_search_tool_result", "tool_use_id": "srvtoolu_01", "content": [
                {"type": "web_search_result", "url": "https://blog.rust-lang.org/",
                 "title": "Rust Blog", "encrypted_content": "EqgfCioIARgBIiQ3YTAwMjY1Mi1mZjM5",
                 "page_age": "November 1, 2026"},
                {"type": "web_search_result", "url": "https://doc.rust-lang.org/releases.html",
                 "title": "Releases", "encrypted_content": "EpUBCioIARgB", "page_age": null}
            ]},
            {"type": "text", "text": "Rust 1.95 came out",
             "citations": [citation("https://blog.rust-lang.org/", "Rust Blog")]}
        ],
        "stop_reason": "pause_turn",
        "stop_sequence": null,
        "usage": {"input_tokens": 600, "output_tokens": 40,
                  "server_tool_use": {"web_search_requests": 1}}
    })
}

/// The reply that finishes the paused turn, citing two pages, one of them
/// again.
fn final_reply() -> Value {
    json!({
        "id": "msg_2", "type": "message", "role": "assistant", "model": "m",
        "content": [
            {"type": "text", "text": " on October 30.",
             "citations": [citation("https://doc.rust-lang.org/releases.html", "Releases"),
                           citation("https://blog.rust-lang.org/", "Rust Blog")]}
        ],
        "stop_reason": "end_turn",
        "stop_sequence": null,
        "usage": {"input_tokens": 700, "output_tokens": 12,
                  "server_tool_use": {"web_search_requests": 0}}
    })
}

fn search() -> WebSearch {
    WebSearch {
        max_uses: 4,
        allowed_domains: vec![],
        blocked_domains: vec!["spam.example".into()],
    }
}

fn agent(url: String, stream: bool) -> Agent {
    let provider = AnthropicProvider::new("sk-test".into(), url, "test-model".into())
        .unwrap()
        .with_streaming(stream)
        .with_web_search(Some(search()));
    Agent::new(
        Box::new(provider),
        ToolRegistry::with_defaults(&Default::default()),
        AgentOptions {
            system: "sys".into(),
            max_tokens: 1024,
            max_turns: 5,
            context_window: 200_000,
            compact_threshold: 0.8,
        },
    )
}

/// Runs a paused search turn and a second input on `agent`, then checks
/// what went to the API and what was reported.
async fn search_then_follow_up(stream: bool) {
    let replies = [paused_reply(), final_reply(), text_reply("Anything else?")];
    let served = replies
        .iter()
        .map(|reply| {
            if stream {
                Served::Stream {
                    body: sse_stream(reply, 4),
                    chunk: 9,
                }
            } else {
                Served::Json(200, reply.clone())
            }
        })
        .collect();
    let (url, server) = fake_stream_api(served).await;
    let mut agent = agent(url, stream);
    let mut log = Vec::new();
    let answer = agent
        .run("Rust 최신 버전?", |event| match event {
            Event::ServerToolUse { name, input } => log.push(format!("server {name} {input}")),
            Event::ToolCall { name, .. } => log.push(format!("tool {name}")),
            Event::Sources(urls) => log.push(format!("sources {}", urls.join(" "))),
            _ => {}
        })
        .await
        .unwrap();
    assert_eq!(
        answer,
        "최신 소식을 찾아볼게요.\nRust 1.95 came out\n on October 30."
    );
    assert_eq!(
        log,
        [
            r#"server web_search {"query":"rust release"}"#,
            "sources https://blog.rust-lang.org/ https://doc.rust-lang.org/releases.html",
        ]
    );
    assert_eq!(agent.usage().web_searches(), 1);
    agent.run("thanks", |_| {}).await.unwrap();

    let requests = tokio::time::timeout(Duration::from_secs(10), server)
        .await
        .unwrap()
        .unwrap();
    let bodies: Vec<Value> = requests.iter().map(|r| body_of(r)).collect();
    for body in &bodies {
        // Offered in every request, after Junees's own tools and marked
        // for caching as the last one, with its line in the system prompt.
        let tools = body["tools"].as_array().unwrap();
        assert_eq!(
            tools.last().unwrap(),
            &json!({"type": "web_search_20250305", "name": "web_search", "max_uses": 4,
                    "blocked_domains": ["spam.example"], "cache_control": {"type": "ephemeral"}})
        );
        assert_eq!(body["system"][1]["text"], web_search::SYSTEM_NOTE);
        assert_eq!(body["stream"].as_bool().unwrap_or(false), stream);
    }
    // The paused reply went back as the API sent it, last, with no user
    // message after it; the cache mark sits on its last text block.
    let continued = &bodies[1]["messages"];
    assert_eq!(continued.as_array().unwrap().len(), 2);
    let mut paused = paused_reply()["content"].clone();
    paused[3]["cache_control"] = json!({"type": "ephemeral"});
    assert_eq!(
        continued[1],
        json!({"role": "assistant", "content": paused})
    );
    // The next input follows both replies, unchanged, citations included.
    let follow_up = bodies[2]["messages"].as_array().unwrap();
    assert_eq!(follow_up.len(), 4);
    assert_eq!(follow_up[1]["content"], paused_reply()["content"]);
    assert_eq!(follow_up[2]["content"], final_reply()["content"]);
    assert_eq!(follow_up[3]["role"], "user");
    // Nothing ran on this side, so nothing answered a tool call.
    assert!(
        !requests
            .iter()
            .any(|r| r.contains(r#""type":"tool_result""#))
    );
}

#[tokio::test]
async fn a_paused_search_turn_goes_on_and_its_blocks_go_back_unchanged() {
    search_then_follow_up(false).await;
}

#[tokio::test]
async fn the_same_holds_for_a_streamed_turn() {
    search_then_follow_up(true).await;
}

/// The binary in a temporary folder, talking to the fake API, saving no
/// session.
async fn junees(url: &str, extra_env: &[(&str, &str)]) -> Output {
    let cwd = tempfile::tempdir().unwrap();
    let mut command = tokio::process::Command::new(env!("CARGO_BIN_EXE_junees"));
    command
        .args(["--no-session", "-p", "news?"])
        .current_dir(cwd.path())
        .env("ANTHROPIC_API_KEY", "sk-fake")
        .env("ANTHROPIC_BASE_URL", url)
        .env("HOME", cwd.path())
        .env_remove("JUNEES_HOME")
        .env_remove("JUNEES_STREAM")
        .env_remove("JUNEES_WEB_SEARCH")
        .env_remove("JUNEES_WEB_SEARCH_MAX_USES")
        .env_remove("JUNEES_WEB_SEARCH_ALLOWED_DOMAINS")
        .env_remove("JUNEES_WEB_SEARCH_BLOCKED_DOMAINS")
        .envs(extra_env.iter().copied())
        .stdin(Stdio::null());
    command.output().await.unwrap()
}

#[tokio::test]
async fn the_binary_lists_the_cited_sources_after_the_answer() {
    let (url, server) = fake_stream_api(vec![
        Served::Stream {
            body: sse_stream(&paused_reply(), 3),
            chunk: 5,
        },
        Served::Stream {
            body: sse_stream(&final_reply(), 3),
            chunk: 5,
        },
    ])
    .await;
    let output = junees(&url, &[("JUNEES_WEB_SEARCH", "on")]).await;
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(output.status.success(), "{stderr}");
    assert_eq!(
        String::from_utf8_lossy(&output.stdout),
        "최신 소식을 찾아볼게요.\nRust 1.95 came out\n on October 30.\n\
         Sources:\n- https://blog.rust-lang.org/\n- https://doc.rust-lang.org/releases.html\n"
    );
    assert!(
        stderr.contains(r#"-> web_search {"query":"rust release"} (run by the API)"#),
        "{stderr}"
    );
    let requests = server.await.unwrap();
    let tools = body_of(&requests[0])["tools"].as_array().unwrap().clone();
    assert_eq!(tools.last().unwrap()["max_uses"], 5);
}

#[tokio::test]
async fn the_binary_offers_no_web_search_unless_it_is_on() {
    let (url, server) = fake_stream_api(vec![Served::Json(200, text_reply("hi"))]).await;
    let output = junees(&url, &[]).await;
    assert!(output.status.success());
    let body = body_of(&server.await.unwrap()[0]);
    assert!(!body["tools"].to_string().contains("web_search"), "{body}");
    assert_eq!(body["system"].as_array().unwrap().len(), 1);
}

#[tokio::test]
async fn the_binary_names_the_setting_when_the_organization_has_search_off() {
    let refused = json!({"type": "error", "error": {"type": "invalid_request_error",
        "message": "Web search is not enabled for this organization."}});
    let (url, server) = fake_stream_api(vec![Served::Json(400, refused)]).await;
    let output = junees(&url, &[("JUNEES_WEB_SEARCH", "on")]).await;
    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("set JUNEES_WEB_SEARCH=off"), "{stderr}");
    // A 400 is not retried.
    assert_eq!(server.await.unwrap().len(), 1);
}
