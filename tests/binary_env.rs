//! Binary tests get none of the test environment's Junees settings or
//! credentials: they start the binary through `common::junees_binary`.
//! Nothing here runs a command.

mod common;

use common::{junees_binary, scrub_env, scrubbed};

#[test]
fn settings_and_credentials_are_removed_and_nothing_else() {
    let names = [
        "JUNEES_BACKEND",
        "JUNEES_CLAUDE_BIN",
        "JUNEES_GITHUB_TOKEN",
        "ANTHROPIC_API_KEY",
        "ANTHROPIC_BASE_URL",
        "CLAUDE_CODE_OAUTH_TOKEN",
        "CLAUDE_CODE_USE_BEDROCK",
        "GH_TOKEN",
        "GITHUB_TOKEN",
        // Kept.
        "PATH",
        "HOME",
        "GH_CONFIG_DIR",
        "GITHUB_TOKENS",
        "MY_JUNEES_HOME",
        "junees_home",
        "TZ",
    ];
    let mut command = tokio::process::Command::new("true");
    scrub_env(&mut command, names);
    let mut removed: Vec<String> = command
        .as_std()
        .get_envs()
        .map(|(name, value)| {
            assert_eq!(value, None, "{name:?} was set, not removed");
            name.to_string_lossy().into_owned()
        })
        .collect();
    removed.sort();
    let mut expected = names[..9].to_vec();
    expected.sort();
    assert_eq!(removed, expected);
}

#[test]
fn the_binary_command_removes_what_the_test_environment_has() {
    let command = junees_binary();
    let removed: Vec<String> = command
        .as_std()
        .get_envs()
        .map(|(name, value)| {
            assert_eq!(value, None, "{name:?} was set, not removed");
            name.to_string_lossy().into_owned()
        })
        .collect();
    let present: Vec<String> = std::env::vars_os()
        .map(|(name, _)| name.to_string_lossy().into_owned())
        .filter(|name| scrubbed(name))
        .collect();
    for name in &present {
        assert!(removed.contains(name), "{name} reaches the binary");
    }
    assert_eq!(removed.len(), present.len(), "{removed:?}");
}
