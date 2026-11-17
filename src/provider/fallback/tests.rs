use std::collections::HashMap;
use std::time::Duration;

use crate::config::Config;
use crate::config_file::{ConfigFile, Settings as Files};
use crate::provider::Kind;

use super::Target;

fn config(pairs: &[(&str, &str)]) -> anyhow::Result<Config> {
    let mut map: HashMap<String, String> = pairs
        .iter()
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect();
    map.entry("HOME".into()).or_insert("/home/tester".into());
    Config::from_lookup(|key| map.get(key).cloned())
}

fn err(pairs: &[(&str, &str)]) -> String {
    match config(pairs) {
        Ok(_) => panic!("{pairs:?} read"),
        Err(err) => format!("{err:#}"),
    }
}

const MAIN: (&str, &str) = ("ANTHROPIC_API_KEY", "sk-ant-main");

#[test]
fn no_fallback_model_no_fallback() {
    let config = config(&[MAIN, ("JUNEES_FALLBACK_PROVIDER", "openai")]).unwrap();
    assert!(config.fallback.is_none());
    assert_eq!(config.fallback_after, super::DEFAULT_AFTER);
    assert_eq!(config.wait_limit, super::DEFAULT_WAIT_LIMIT);
}

#[test]
fn a_fallback_of_the_same_kind_reuses_the_main_address_and_key() {
    let config = config(&[
        MAIN,
        ("ANTHROPIC_BASE_URL", "http://127.0.0.1:9/"),
        ("JUNEES_FALLBACK_MODEL", "claude-haiku-4-5"),
    ])
    .unwrap();
    let fallback = config.fallback.unwrap();
    assert_eq!(fallback.model, "claude-haiku-4-5");
    assert!(matches!(
        fallback.target,
        Target::Anthropic { ref api_key, ref base_url }
            if api_key == "sk-ant-main" && base_url == "http://127.0.0.1:9/"
    ));

    let openai = config_openai(&[("JUNEES_FALLBACK_MODEL", "small")]);
    let fallback = openai.fallback.unwrap();
    assert_eq!(fallback.kind(), Kind::OpenAi);
    assert!(matches!(
        fallback.target,
        Target::OpenAi(ref settings)
            if settings.api_key.as_deref() == Some("sk-openai") && settings.reasoning_effort.as_deref() == Some("low")
    ));
}

fn config_openai(more: &[(&str, &str)]) -> Config {
    let mut pairs = vec![
        ("JUNEES_PROVIDER", "openai"),
        ("OPENAI_API_KEY", "sk-openai"),
        ("JUNEES_MODEL", "big"),
        ("JUNEES_OPENAI_REASONING_EFFORT", "low"),
    ];
    pairs.extend_from_slice(more);
    config(&pairs).unwrap()
}

#[test]
fn a_fallback_of_another_kind_reads_that_kinds_settings() {
    let config = config(&[
        MAIN,
        ("JUNEES_FALLBACK_MODEL", "gpt-small"),
        ("JUNEES_FALLBACK_PROVIDER", "openai"),
        ("OPENAI_API_KEY", "sk-openai"),
    ])
    .unwrap();
    let fallback = config.fallback.unwrap();
    assert_eq!(
        fallback.address(),
        crate::provider::openai::DEFAULT_BASE_URL
    );
    // The main model's thinking settings are Anthropic's, and stay there.
    assert!(matches!(fallback.target, Target::OpenAi(ref s) if s.reasoning_effort.is_none()));

    let message = err(&[
        MAIN,
        ("JUNEES_FALLBACK_MODEL", "gpt-small"),
        ("JUNEES_FALLBACK_PROVIDER", "openai"),
    ]);
    assert!(
        message.contains("no API key for https://api.openai.com/v1"),
        "{message}"
    );

    let back = config_openai(&[
        ("JUNEES_FALLBACK_MODEL", "claude-haiku-4-5"),
        ("JUNEES_FALLBACK_PROVIDER", "anthropic"),
        ("ANTHROPIC_API_KEY", "sk-ant"),
    ]);
    assert!(matches!(
        back.fallback.unwrap().target,
        Target::Anthropic { ref api_key, ref base_url }
            if api_key == "sk-ant" && base_url == crate::config::DEFAULT_BASE_URL
    ));
    let message = err(&[
        ("JUNEES_PROVIDER", "openai"),
        ("OPENAI_API_KEY", "sk-openai"),
        ("JUNEES_MODEL", "big"),
        ("JUNEES_FALLBACK_MODEL", "claude-haiku-4-5"),
        ("JUNEES_FALLBACK_PROVIDER", "anthropic"),
    ]);
    assert!(
        message.contains("fallback_provider = anthropic needs a key"),
        "{message}"
    );
}

#[test]
fn a_fallback_address_takes_only_its_own_key() {
    let config = config(&[
        MAIN,
        ("JUNEES_FALLBACK_MODEL", "llama"),
        ("JUNEES_FALLBACK_PROVIDER", "openai"),
        ("JUNEES_FALLBACK_BASE_URL", "http://localhost:11434/v1/"),
        ("OPENAI_API_KEY", "sk-openai"),
    ])
    .unwrap();
    // A server on this machine needs no key, and OpenAI's is not sent there.
    let Target::OpenAi(settings) = config.fallback.unwrap().target else {
        panic!("not openai");
    };
    assert_eq!(settings.base_url, "http://localhost:11434/v1");
    assert_eq!(settings.api_key, None);

    let message = err(&[
        MAIN,
        ("JUNEES_FALLBACK_MODEL", "m"),
        ("JUNEES_FALLBACK_PROVIDER", "openai"),
        ("JUNEES_FALLBACK_BASE_URL", "https://openrouter.ai/api/v1"),
        ("OPENAI_API_KEY", "sk-openai"),
    ]);
    assert!(message.contains("set fallback_api_key"), "{message}");
    let message = err(&[
        MAIN,
        ("JUNEES_FALLBACK_MODEL", "m"),
        (
            "JUNEES_FALLBACK_BASE_URL",
            "https://proxy.example/anthropic",
        ),
    ]);
    assert!(
        message.contains("no key for the fallback model at https://proxy.example/anthropic"),
        "{message}"
    );
    let message = err(&[
        MAIN,
        ("JUNEES_FALLBACK_MODEL", "m"),
        ("JUNEES_FALLBACK_BASE_URL", "http://proxy.example/anthropic"),
        ("JUNEES_FALLBACK_API_KEY", "k"),
    ]);
    assert!(
        message.contains("JUNEES_FALLBACK_BASE_URL (`fallback_base_url`) must use https"),
        "{message}"
    );
    let message = err(&[
        MAIN,
        ("JUNEES_FALLBACK_MODEL", "m"),
        ("JUNEES_FALLBACK_API_KEY", "k"),
    ]);
    assert!(
        message.contains("the key goes only to that address"),
        "{message}"
    );

    // config.toml's address with a key from the environment: not read.
    let mut file = ConfigFile::default();
    file.set("fallback_model", "m");
    file.set("fallback_base_url", "https://proxy.example/anthropic");
    let env = |key: &str| match key {
        "ANTHROPIC_API_KEY" => Some("sk-ant-main".to_string()),
        "JUNEES_FALLBACK_API_KEY" => Some("sk-elsewhere".to_string()),
        "HOME" => Some("/home/u".to_string()),
        _ => None,
    };
    let files = Files::new(env, file.clone(), None, true);
    assert!(files.key_away_from_address("JUNEES_FALLBACK_API_KEY"));
    let message = format!(
        "{:#}",
        Config::from_lookup(|key| files.get(key)).err().unwrap()
    );
    assert!(message.contains("set fallback_api_key"), "{message}");
    file.set("fallback_api_key", "sk-file");
    let files = Files::new(env, file, None, true);
    let config = Config::from_lookup(|key| files.get(key)).unwrap();
    assert!(matches!(
        config.fallback.unwrap().target,
        Target::Anthropic { ref api_key, .. } if api_key == "sk-file"
    ));
}

#[test]
fn times_and_the_claude_code_backend() {
    let config = config(&[
        MAIN,
        ("JUNEES_FALLBACK_AFTER", "90s"),
        ("JUNEES_WAIT_LIMIT", "0"),
    ])
    .unwrap();
    assert_eq!(config.fallback_after, Duration::from_secs(90));
    assert_eq!(config.wait_limit, Duration::ZERO);
    assert!(err(&[MAIN, ("JUNEES_FALLBACK_AFTER", "0")]).contains("must be longer than 0"));
    assert!(err(&[MAIN, ("JUNEES_WAIT_LIMIT", "soon")]).contains("JUNEES_WAIT_LIMIT"));
    assert!(
        err(&[
            MAIN,
            ("JUNEES_FALLBACK_MODEL", "m"),
            ("JUNEES_FALLBACK_PROVIDER", "gemini")
        ])
        .contains("fallback_provider")
    );

    let without_key = Config::from_lookup_without_key(|key| match key {
        "JUNEES_FALLBACK_MODEL" => Some("m".into()),
        "JUNEES_FALLBACK_BASE_URL" => Some("https://proxy.example".into()),
        _ => None,
    })
    .unwrap();
    assert!(without_key.fallback.is_none());
}
