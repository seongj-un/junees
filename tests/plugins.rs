//! Runs the real binary with a plugin installed through `junees plugin`:
//! the model calls its command tool, its `pre_tool` hook refuses a bash
//! call, its `post_turn` hook hears the answer, and its prompt note is in
//! the system prompt.

use std::path::Path;
use std::process::{Output, Stdio};
use std::time::Duration;

use serde_json::{Value, json};

mod common;
use common::{body_of, fake_api, text_reply};

fn junees(home: &Path, cwd: &Path, args: &[&str]) -> tokio::process::Command {
    let mut command = common::junees_binary(cwd, home);
    command
        .args(args)
        .env("JUNEES_HOME", home)
        .env("JUNEES_STREAM", "0")
        .stdin(Stdio::null());
    command
}

fn check(output: &Output) -> String {
    assert!(
        output.status.success(),
        "junees failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8_lossy(&output.stdout).into_owned()
}

/// The plugin `kit` to install: a tool printing the variables it is asked
/// about and its folder, a hook refusing bash, a hook keeping each turn.
fn plugin_source(dir: &Path) -> std::path::PathBuf {
    let source = dir.join("kit");
    std::fs::create_dir_all(&source).unwrap();
    let turns = dir.join("turns.jsonl");
    let manifest = json!({
        "description": "Test kit",
        "tools": [{
            "name": "env",
            "description": "Shows variables",
            "input_schema": {"type": "object", "properties": {"names": {"type": "array"}}},
            "command": ["python3", "tool.py"],
        }],
        "hooks": {
            "pre_tool": [{"command": ["sh", "guard.sh"]}],
            "post_turn": [{"command": ["sh", "-c", format!("cat >> {}", turns.display())]}],
        },
        "prompt": "PROMPT.md",
    });
    std::fs::write(source.join("plugin.json"), manifest.to_string()).unwrap();
    std::fs::write(
        source.join("tool.py"),
        "import json, os, sys\n\
         names = json.load(sys.stdin)['names']\n\
         for name in names:\n    print(f\"{name}={os.environ.get(name, '<unset>')}\")\n\
         print('cwd=' + os.getcwd())\n",
    )
    .unwrap();
    std::fs::write(
        source.join("guard.sh"),
        "if grep -q '\"tool\":\"bash\"'; then echo 'no shell here'; exit 2; fi\n",
    )
    .unwrap();
    std::fs::write(
        source.join("PROMPT.md"),
        "Use plugin__kit__env to read variables.\n",
    )
    .unwrap();
    source
}

#[tokio::test]
async fn an_installed_and_enabled_plugin_works_in_a_run() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().canonicalize().unwrap();
    let (home, work) = (root.join("home"), root.join("work"));
    std::fs::create_dir_all(&home).unwrap();
    std::fs::create_dir_all(&work).unwrap();
    let source = plugin_source(&root);
    let source_arg = source.display().to_string();

    let run = |args: &'static [&'static str]| junees(&home, &work, args).output();
    let installed = check(
        &junees(&home, &work, &["plugin", "install", &source_arg])
            .output()
            .await
            .unwrap(),
    );
    assert!(
        installed.starts_with("installed plugin kit in "),
        "{installed}"
    );
    let listed = check(&run(&["plugin", "list"]).await.unwrap());
    assert!(
        listed
            .starts_with("  kit: disabled, 1 tool, 1 pre_tool hook, 1 post_turn hook, a prompt\n"),
        "{listed}"
    );
    check(&run(&["plugin", "enable", "kit"]).await.unwrap());
    let wrong = run(&["plugin", "enable"]).await.unwrap();
    assert!(!wrong.status.success());
    assert!(String::from_utf8_lossy(&wrong.stderr).contains("usage: junees plugin"));

    let marker = root.join("marker");
    let (url, api) = fake_api(vec![
        (
            200,
            json!({
                "id": "msg_1", "type": "message", "role": "assistant", "model": "m",
                "content": [
                    {"type": "tool_use", "id": "toolu_1", "name": "plugin__kit__env",
                     "input": {"names": ["ANTHROPIC_API_KEY", "JUNEES_DISCORD_TOKEN",
                                         "JUNEES_TEST_HIDDEN", "JUNEES_TEST_VISIBLE"]}},
                    {"type": "tool_use", "id": "toolu_2", "name": "bash",
                     "input": {"command": format!("touch {}", marker.display())}},
                ],
                "stop_reason": "tool_use",
                "usage": {"input_tokens": 10, "output_tokens": 5}
            }),
        ),
        (200, text_reply("done")),
    ])
    .await;
    let output = junees(&home, &work, &["-p", "go", "--no-session"])
        .env("ANTHROPIC_API_KEY", "sk-test-key")
        .env("ANTHROPIC_BASE_URL", &url)
        .env("JUNEES_DISCORD_TOKEN", "discord-secret")
        .env("JUNEES_HIDDEN_ENV", "JUNEES_TEST_HIDDEN")
        .env("JUNEES_TEST_HIDDEN", "hidden-value")
        .env("JUNEES_TEST_VISIBLE", "shown")
        .output()
        .await
        .unwrap();
    assert_eq!(check(&output), "done\n");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr
            .contains("plugin: kit enabled (1 tool, 1 pre_tool hook, 1 post_turn hook, a prompt)"),
        "{stderr}"
    );
    let requests = tokio::time::timeout(Duration::from_secs(10), api)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(requests.len(), 2);
    let first = body_of(&requests[0]);
    let tools: Vec<&str> = first["tools"]
        .as_array()
        .unwrap()
        .iter()
        .map(|tool| tool["name"].as_str().unwrap())
        .collect();
    assert!(tools.contains(&"plugin__kit__env"), "{tools:?}");
    assert!(
        first["system"]
            .to_string()
            .contains("## kit\\nUse plugin__kit__env to read variables.\\n"),
        "{}",
        first["system"]
    );

    let results = body_of(&requests[1])["messages"]
        .as_array()
        .unwrap()
        .last()
        .unwrap()["content"]
        .clone();
    let installed_dir = home.join("plugins/kit");
    assert_eq!(
        results[0],
        json!({"type": "tool_result", "tool_use_id": "toolu_1", "content": format!(
            "ANTHROPIC_API_KEY=<unset>\nJUNEES_DISCORD_TOKEN=<unset>\nJUNEES_TEST_HIDDEN=<unset>\n\
             JUNEES_TEST_VISIBLE=shown\ncwd={}",
            installed_dir.display()
        )})
    );
    assert_eq!(results[1]["is_error"], true);
    assert_eq!(
        results[1]["content"],
        "not run: the pre_tool hook of the plugin `kit` refused this call: no shell here"
    );
    assert!(!marker.exists(), "the refused bash call ran");

    let turn: Value =
        serde_json::from_str(&std::fs::read_to_string(root.join("turns.jsonl")).unwrap()).unwrap();
    assert_eq!(
        turn,
        json!({"event": "post_turn", "input": "go", "outcome": "answered", "answer": "done",
               "error": null, "cwd": work})
    );
}
