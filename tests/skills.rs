//! Skills through the real binary, against a fake API server: the skill
//! list in the system prompt, skill_view and skill_manage in a
//! conversation, `/skills`, `/skill <name>` and `/new` reading the list
//! again.

use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::process::Stdio;
use std::time::Duration;

use serde_json::{Value, json};

mod common;
use common::{body_of, fake_api, text_reply};

fn tool_calls(calls: &[(&str, Value)]) -> Value {
    let content: Vec<Value> = calls
        .iter()
        .enumerate()
        .map(|(i, (name, input))| {
            json!({"type": "tool_use", "id": format!("toolu_{i}"), "name": name, "input": input})
        })
        .collect();
    json!({
        "id": "msg_0", "type": "message", "role": "assistant", "model": "m",
        "content": content,
        "stop_reason": "tool_use",
        "usage": {"input_tokens": 3, "output_tokens": 1}
    })
}

/// The tool results of the last message of a request, as (text, is_error).
fn tool_results(request: &str) -> Vec<(String, bool)> {
    let body = body_of(request);
    let last = body["messages"].as_array().unwrap().last().unwrap().clone();
    last["content"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|block| block["type"] == "tool_result")
        .map(|block| {
            (
                block["content"].as_str().unwrap().to_string(),
                block["is_error"].as_bool().unwrap_or(false),
            )
        })
        .collect()
}

fn tool_names(request: &str) -> Vec<String> {
    body_of(request)["tools"]
        .as_array()
        .unwrap()
        .iter()
        .map(|tool| tool["name"].as_str().unwrap().to_string())
        .collect()
}

fn mode(path: &Path) -> u32 {
    std::fs::metadata(path).unwrap().permissions().mode() & 0o777
}

const DEPLOY: &str = "---\nname: deploy\ndescription: Use when deploying\n---\n1. ship it\n";
const NEW_ONE: &str = "---\nname: new-one\ndescription: Use when testing skills\n---\n\
                       1. write the test\n2. run it\n";

#[tokio::test]
async fn the_conversation_reads_and_keeps_skills() {
    let dir = tempfile::tempdir().unwrap();
    let home = dir.path().join("junees");
    let skills = home.join("skills");
    std::fs::create_dir_all(skills.join("deploy")).unwrap();
    std::fs::write(skills.join("deploy/SKILL.md"), DEPLOY).unwrap();
    let cwd = dir.path().join("work");
    std::fs::create_dir(&cwd).unwrap();

    let (url, server) = fake_api(vec![
        (
            200,
            tool_calls(&[
                ("skill_view", json!({"name": "deploy"})),
                (
                    "skill_manage",
                    json!({"action": "create", "name": "new-one", "content": NEW_ONE}),
                ),
            ]),
        ),
        (200, text_reply("Saved the skill.")),
        (200, text_reply("Second session.")),
    ])
    .await;
    let mut command = common::junees_binary(&cwd, dir.path());
    command
        .arg("--no-session")
        .env("ANTHROPIC_API_KEY", "sk-fake")
        .env("ANTHROPIC_BASE_URL", &url)
        .env("JUNEES_HOME", &home)
        .env("JUNEES_STREAM", "0")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = command.spawn().unwrap();
    {
        use tokio::io::AsyncWriteExt;
        let mut stdin = child.stdin.take().unwrap();
        stdin
            .write_all(
                b"save this as a skill\n/skills\n/skill new-one\n/skill nope\n/new\n\
                  next question\n/exit\n",
            )
            .await
            .unwrap();
    }
    let out = tokio::time::timeout(Duration::from_secs(60), child.wait_with_output())
        .await
        .expect("junees did not exit")
        .unwrap();
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(out.status.success(), "{stdout}\n{stderr}");
    let requests = tokio::time::timeout(Duration::from_secs(10), server)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(requests.len(), 3);

    // The first session lists the skill there was when it started and
    // offers the tools.
    let first = body_of(&requests[0])["system"].to_string();
    assert!(first.contains("# Skills"), "{first}");
    assert!(first.contains("- deploy: Use when deploying"), "{first}");
    assert!(!first.contains("new-one"), "{first}");
    let names = tool_names(&requests[0]);
    assert_eq!(
        names[names.len() - 3..],
        ["skill_view", "skill_manage", "delegate"]
    );
    let results = tool_results(&requests[1]);
    assert_eq!(results[0], (DEPLOY.to_string(), false));
    assert!(!results[1].1, "{}", results[1].0);
    assert!(
        results[1].0.starts_with("Created skill `new-one`."),
        "{}",
        results[1].0
    );
    // Its list stays as it was for the rest of the session.
    assert_eq!(body_of(&requests[1])["system"].to_string(), first);

    let saved = skills.join("new-one/SKILL.md");
    assert_eq!(std::fs::read_to_string(&saved).unwrap(), NEW_ONE);
    assert_eq!(mode(&saved), 0o600);
    assert_eq!(mode(&skills.join("new-one")), 0o700);

    // /skills and /skill show what is on disk now.
    assert!(
        stdout.contains(&format!(
            "2 skills in {} (prompt list 64 / 4000 characters):\n  \
             deploy: Use when deploying\n  new-one: Use when testing skills\n",
            skills.display()
        )),
        "{stdout}"
    );
    assert!(stdout.contains(NEW_ONE.trim_end()), "{stdout}");
    assert!(
        stderr.contains("no skill named `nope`; the skills are deploy, new-one"),
        "{stderr}"
    );

    // /new starts a session that reads the skills again.
    let second = body_of(&requests[2])["system"].to_string();
    assert!(
        second.contains("- new-one: Use when testing skills"),
        "{second}"
    );
    assert_eq!(
        body_of(&requests[2])["messages"].as_array().unwrap().len(),
        1
    );
}

#[tokio::test]
async fn the_skills_screen_escapes_hidden_characters() {
    let dir = tempfile::tempdir().unwrap();
    let home = dir.path().join("junees");
    let skills = home.join("skills");
    for (folder, name, description) in [
        ("evil", "evil", "ok\x1b[2K\rfake"),
        ("other", "x\x1b]0;title\x07", "d"),
    ] {
        std::fs::create_dir_all(skills.join(folder)).unwrap();
        std::fs::write(
            skills.join(folder).join("SKILL.md"),
            format!("---\nname: {name}\ndescription: {description}\n---\n1. go\n"),
        )
        .unwrap();
    }
    let cwd = dir.path().join("work");
    std::fs::create_dir(&cwd).unwrap();
    let mut command = common::junees_binary(&cwd, dir.path());
    command
        .arg("--no-session")
        .env("ANTHROPIC_API_KEY", "sk-fake")
        .env("ANTHROPIC_BASE_URL", "http://127.0.0.1:9")
        .env("JUNEES_HOME", &home)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = command.spawn().unwrap();
    {
        use tokio::io::AsyncWriteExt;
        let mut stdin = child.stdin.take().unwrap();
        stdin.write_all(b"/skills\n/exit\n").await.unwrap();
    }
    let out = tokio::time::timeout(Duration::from_secs(60), child.wait_with_output())
        .await
        .expect("junees did not exit")
        .unwrap();
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(out.status.success(), "{stdout}\n{stderr}");
    assert!(stdout.contains("  evil: ok\\u{1b}[2K\\rfake\n"), "{stdout}");
    assert!(
        stderr.contains("  !! skipped other/: its SKILL.md says `name: x\\u{1b}]0;title\\u{7}`"),
        "{stderr}"
    );
    for text in [&stdout, &stderr] {
        assert!(!text.contains(['\x1b', '\r', '\x07']), "{text:?}");
    }
}
