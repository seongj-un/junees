//! The default bash tool must not leak the API key. This file holds a single
//! test because it changes the process environment.

use junees::tools::{ToolRegistry, ToolSettings};
use serde_json::json;

#[tokio::test]
async fn default_bash_cannot_read_the_api_key() {
    // SAFETY: this test binary runs only this test, so no other thread reads
    // the environment concurrently.
    unsafe {
        std::env::set_var("ANTHROPIC_API_KEY", "sk-should-not-leak");
    }
    let registry = ToolRegistry::with_defaults(&ToolSettings::default());

    let output = registry
        .dispatch("bash", json!({"command": "printenv ANTHROPIC_API_KEY"}))
        .await;
    assert_eq!(output.content, "[exit code 1]");

    let output = registry
        .dispatch(
            "bash",
            json!({"command": "env | grep -c sk-should-not-leak"}),
        )
        .await;
    assert!(output.content.starts_with("0\n"), "{}", output.content);
}
