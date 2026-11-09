//! Variables named in JUNEES_HIDDEN_ENV must not reach bash, through the
//! whole path from the environment to Config, ToolRegistry and Bash. This
//! file holds a single test because it changes the process environment.

use junees::config::Config;
use junees::tools::ToolRegistry;
use serde_json::json;

#[tokio::test]
async fn junees_hidden_env_hides_extra_variables_from_bash() {
    // SAFETY: this test binary runs only this test, so no other thread reads
    // the environment concurrently.
    unsafe {
        std::env::set_var("ANTHROPIC_API_KEY", "sk-hidden-env-test");
        std::env::set_var("JUNEES_TEST_DB_PASSWORD", "hunter2");
        std::env::set_var("JUNEES_TEST_GITHUB_TOKEN", "ghp-test");
        std::env::set_var("JUNEES_TEST_VISIBLE", "shown");
        std::env::set_var(
            "JUNEES_HIDDEN_ENV",
            "JUNEES_TEST_DB_PASSWORD, JUNEES_TEST_GITHUB_TOKEN",
        );
    }
    let config = Config::from_env().unwrap();
    let registry = ToolRegistry::with_defaults(&config.tools);

    for name in [
        "JUNEES_TEST_DB_PASSWORD",
        "JUNEES_TEST_GITHUB_TOKEN",
        "ANTHROPIC_API_KEY",
    ] {
        let output = registry
            .dispatch("bash", json!({"command": format!("printenv {name}")}))
            .await;
        assert_eq!(output.content, "[exit code 1]", "{name} is visible");
    }

    let output = registry
        .dispatch("bash", json!({"command": "printenv JUNEES_TEST_VISIBLE"}))
        .await;
    assert_eq!(output.content, "shown\n[exit code 0]");
}
