//! The variables Junees hides from bash must not be readable through
//! `/proc/<pid>/environ` either: a command could read them from its parent,
//! the junees process, or from any other ancestor it walks up to.

#![cfg(target_os = "linux")]

use std::process::Stdio;

use serde_json::json;

mod common;
use common::{fake_api, text_reply};

const KEY: &str = "sk-probe-proc-environ";
const HOOK_TOKEN: &str = "hook-probe-proc-environ";

/// Run by a grandchild of junees (bash -> bash): prints `<pid> <verdict>`
/// for itself and every ancestor, where the verdict is `denied` (environ
/// cannot be read), `leak` (it holds a hidden value) or `open`.
const WALK: &str = r#"bash -c '
p=$$
while [ "$p" -gt 1 ]; do
  if env_text=$(tr "\0" "\n" 2>/dev/null < /proc/$p/environ); then
    case "$env_text" in
      *sk-probe-proc-environ*|*hook-probe-proc-environ*) echo "$p leak" ;;
      *) echo "$p open" ;;
    esac
  else
    echo "$p denied"
  fi
  p=$(awk "/^PPid:/ {print \$2}" /proc/$p/status)
done'"#;

#[tokio::test]
async fn bash_cannot_read_hidden_variables_from_any_ancestor() {
    let (url, api) = fake_api(vec![
        (
            200,
            json!({
                "id": "msg_1", "type": "message", "role": "assistant", "model": "m",
                "content": [{
                    "type": "tool_use", "id": "toolu_1", "name": "bash",
                    "input": {"command": WALK}
                }],
                "stop_reason": "tool_use",
                "usage": {"input_tokens": 3, "output_tokens": 1}
            }),
        ),
        (200, text_reply("done")),
    ])
    .await;
    // Junees and its bash keep off the user's state: their own
    // JUNEES_HOME (no settings file, MCP servers or plugins of the user's)
    // and HOME, run from a folder in there instead of the crate.
    let home = tempfile::tempdir().unwrap();
    let work = home.path().join("work");
    std::fs::create_dir(&work).unwrap();
    let child = common::junees_binary(&work, home.path())
        .args(["--no-session", "-p", "probe"])
        .env("JUNEES_HOME", home.path().join(".junees"))
        .env_remove("XDG_CONFIG_HOME")
        .env("ANTHROPIC_API_KEY", KEY)
        .env("ANTHROPIC_BASE_URL", &url)
        .env(
            "JUNEES_NOTIFY_WEBHOOK",
            format!("https://discord.invalid/api/webhooks/1/{HOOK_TOKEN}"),
        )
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let junees = child.id().unwrap();
    let output = child.wait_with_output().await.unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );

    let requests = tokio::time::timeout(std::time::Duration::from_secs(10), api)
        .await
        .expect("junees made fewer model calls than expected")
        .unwrap();
    let body = common::body_of(&requests[1]);
    let result = body["messages"][2]["content"][0]["content"]
        .as_str()
        .unwrap()
        .to_string();
    let verdicts: Vec<(u32, &str)> = result
        .lines()
        .filter_map(|line| {
            let (pid, verdict) = line.split_once(' ')?;
            Some((pid.parse().ok()?, verdict))
        })
        .collect();
    let verdict = |pid: u32| verdicts.iter().find(|(p, _)| *p == pid).map(|(_, v)| *v);

    assert!(!result.contains("leak"), "{result}");
    assert_eq!(verdict(junees), Some("denied"), "{result}");
    // The walk does read the environment of ordinary processes: this test
    // process is an ancestor too, and its environment is open.
    assert_eq!(verdict(std::process::id()), Some("open"), "{result}");
    // Both shells between junees and the walk are open, without the values.
    assert!(
        verdicts.iter().filter(|(_, v)| *v == "open").count() >= 3,
        "{result}"
    );
}
