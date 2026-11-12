//! Runs the real binary against a fake API server: a bash command the
//! model sends runs in the Landlock sandbox, so a path the command policy
//! cannot see (built at run time) still does not reach the settings file,
//! the secrets folder or a data folder, while the working folder stays
//! writable. Skipped when the kernel has no Landlock.

use std::process::Stdio;
use std::time::Duration;

use serde_json::json;

mod common;
use common::{body_of, fake_api, text_reply};

#[tokio::test]
async fn the_models_bash_runs_in_the_sandbox() {
    if let junees::sandbox::Support::Unavailable(why) = junees::sandbox::Support::current() {
        eprintln!("skipped: {why}");
        return;
    }
    let home = tempfile::tempdir().unwrap();
    let junees = home.path().join(".junees");
    let work = home.path().join("work");
    for folder in [junees.join("secrets"), junees.join("memory"), work.clone()] {
        std::fs::create_dir_all(folder).unwrap();
    }
    std::fs::write(junees.join("config.toml"), "model = \"fake-model\"\n").unwrap();
    std::fs::write(junees.join("secrets/token"), "fake-secret\n").unwrap();
    // The policy refuses commands naming these paths; this one builds them
    // at run time, so only the sandbox stands in the way.
    let command = format!(
        "d={}/.jun; echo in > made && echo made-ok; \
         cat ${{d}}ees/config.toml || echo config-refused; \
         cat ${{d}}ees/secrets/token || echo secret-refused; \
         echo x > ${{d}}ees/memory/new.md || echo memory-refused",
        home.path().display()
    );
    let (url, server) = fake_api(vec![
        (
            200,
            json!({
                "id": "msg_1", "type": "message", "role": "assistant", "model": "m",
                "content": [{"type": "tool_use", "id": "toolu_1", "name": "bash",
                             "input": {"command": command}}],
                "stop_reason": "tool_use",
                "usage": {"input_tokens": 10, "output_tokens": 5}
            }),
        ),
        (200, text_reply("Done.")),
    ])
    .await;
    let mut binary = common::junees_binary(&work, home.path());
    binary
        .args(["-p", "try it"])
        .env("ANTHROPIC_API_KEY", "sk-fake")
        .env("ANTHROPIC_BASE_URL", &url)
        .env("JUNEES_HOME", &junees)
        .env("JUNEES_STREAM", "0")
        .stdin(Stdio::null());
    let output = tokio::time::timeout(Duration::from_secs(60), binary.output())
        .await
        .expect("junees did not end")
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let requests = tokio::time::timeout(Duration::from_secs(10), server)
        .await
        .expect("the fake API did not get every request")
        .unwrap();
    assert_eq!(requests.len(), 2);
    let result = body_of(&requests[1])["messages"]
        .as_array()
        .unwrap()
        .last()
        .unwrap()["content"][0]["content"]
        .to_string();
    for line in [
        "made-ok",
        "config-refused",
        "secret-refused",
        "memory-refused",
        "Permission denied",
    ] {
        assert!(result.contains(line), "{line}: {result}");
    }
    assert!(!result.contains("fake-model"), "{result}");
    assert!(!result.contains("fake-secret"), "{result}");
    assert!(work.join("made").exists());
    assert!(!junees.join("memory/new.md").exists());
}
