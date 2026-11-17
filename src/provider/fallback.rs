//! The fallback model: another model, maybe from another provider, that
//! takes over when the main one stays blocked (usage limit, overload, an
//! API that does not answer), and how long unattended runs wait for a
//! limit to lift. See docs/configuration.md, "예비 모델".

use std::time::Duration;

use anyhow::{Result, bail};

use super::{AddressNames, Kind, openai};

/// The fallback model's name (`fallback_model`); unset turns the fallback
/// off.
pub const MODEL_ENV: &str = "JUNEES_FALLBACK_MODEL";
/// Its API (`fallback_provider`): anthropic or openai, by default the main
/// one's.
pub const PROVIDER_ENV: &str = "JUNEES_FALLBACK_PROVIDER";
/// Its address (`fallback_base_url`), when it is not the main provider's
/// (or, for another kind, that kind's usual setting).
pub const BASE_URL_ENV: &str = "JUNEES_FALLBACK_BASE_URL";
/// The key for [`BASE_URL_ENV`] (`fallback_api_key`), read only from where
/// that address is set (`config_file::PAIRED_KEYS`).
pub const API_KEY_ENV: &str = "JUNEES_FALLBACK_API_KEY";
/// How long the main model may stay blocked before the fallback takes
/// over (`fallback_after`).
pub const AFTER_ENV: &str = "JUNEES_FALLBACK_AFTER";
/// How long an unattended run may wait for a usage limit to lift
/// (`wait_limit`); 0 turns waiting off.
pub const WAIT_LIMIT_ENV: &str = "JUNEES_WAIT_LIMIT";

pub const DEFAULT_AFTER: Duration = Duration::from_secs(2 * 60);
pub const DEFAULT_WAIT_LIMIT: Duration = Duration::from_secs(30 * 60);

pub const NAMES: AddressNames = AddressNames {
    env: BASE_URL_ENV,
    setting: "fallback_base_url",
    key_env: API_KEY_ENV,
    key_setting: "fallback_api_key",
};

/// Where the fallback model's calls go. Deliberately not `Debug`: it holds
/// keys.
#[derive(Clone, PartialEq)]
pub enum Target {
    Anthropic { api_key: String, base_url: String },
    OpenAi(openai::Settings),
}

/// The fallback model and where it is. Deliberately not `Debug`.
#[derive(Clone, PartialEq)]
pub struct Settings {
    pub model: String,
    pub target: Target,
}

impl Settings {
    pub fn kind(&self) -> Kind {
        match self.target {
            Target::Anthropic { .. } => Kind::Anthropic,
            Target::OpenAi(_) => Kind::OpenAi,
        }
    }

    /// Where the calls go, for messages.
    pub fn address(&self) -> &str {
        match &self.target {
            Target::Anthropic { base_url, .. } => base_url,
            Target::OpenAi(settings) => &settings.base_url,
        }
    }
}

/// What the main provider is, so a fallback of the same kind without an
/// address of its own reuses its address and key.
pub struct Main<'a> {
    pub kind: Kind,
    pub anthropic_key: &'a str,
    pub anthropic_url: &'a str,
    pub openai: Option<&'a openai::Settings>,
}

/// Reads the fallback settings; `None` when `fallback_model` is not set.
///
/// The address and key: `fallback_base_url` with `fallback_api_key` when
/// the address is set (the key only from the same place, and an OpenAI
/// server on this machine may do without one); otherwise the main
/// provider's when the kind is the same; otherwise that kind's usual
/// settings (`base_url`/`api_key` or `openai_base_url`/`openai_api_key`),
/// under their usual rules.
pub fn read(get: &impl Fn(&str) -> Option<String>, main: &Main<'_>) -> Result<Option<Settings>> {
    let Some(model) = get(MODEL_ENV) else {
        return Ok(None);
    };
    let kind = match get(PROVIDER_ENV) {
        None => main.kind,
        Some(value) => Kind::parse(Some(&value)).map_err(|_| {
            anyhow::anyhow!(
                "{PROVIDER_ENV} (`fallback_provider`) must be anthropic or openai, got `{value}`"
            )
        })?,
    };
    let target = match (get(BASE_URL_ENV), kind) {
        (Some(base_url), kind) => {
            super::check_base_url(&base_url, &NAMES)?;
            let base_url = base_url.trim().trim_end_matches('/').to_string();
            let key = get(API_KEY_ENV);
            match kind {
                Kind::Anthropic => Target::Anthropic {
                    api_key: key.ok_or_else(|| missing_key(&base_url))?,
                    base_url,
                },
                Kind::OpenAi => {
                    let mut settings = openai::Settings::new(base_url, key);
                    if settings.api_key.is_none() && !settings.is_local() {
                        return Err(missing_key(&settings.base_url));
                    }
                    if main.kind == Kind::OpenAi
                        && let Some(main) = main.openai
                    {
                        settings.limit_field = main.limit_field;
                        settings.reasoning_effort = main.reasoning_effort.clone();
                    }
                    Target::OpenAi(settings)
                }
            }
        }
        (None, _) if get(API_KEY_ENV).is_some() => bail!(
            "{API_KEY_ENV} (`fallback_api_key`) is set but {BASE_URL_ENV} (`fallback_base_url`) \
             is not: the key goes only to that address. Set the address, or leave out the key \
             to use the main provider's"
        ),
        (None, Kind::Anthropic) if main.kind == Kind::Anthropic => Target::Anthropic {
            api_key: main.anthropic_key.to_string(),
            base_url: main.anthropic_url.to_string(),
        },
        (None, Kind::OpenAi) if main.kind == Kind::OpenAi && main.openai.is_some() => {
            Target::OpenAi(main.openai.expect("checked above").clone())
        }
        (None, Kind::Anthropic) => {
            let base_url = get("ANTHROPIC_BASE_URL")
                .unwrap_or_else(|| crate::config::DEFAULT_BASE_URL.to_string());
            super::check_base_url(&base_url, &crate::config::ANTHROPIC_ADDRESS)?;
            let Some(api_key) = get("ANTHROPIC_API_KEY") else {
                bail!(
                    "fallback_provider = anthropic needs a key: set api_key or \
                     ANTHROPIC_API_KEY (in the same place as base_url, if that is set), or \
                     fallback_base_url with fallback_api_key"
                );
            };
            Target::Anthropic {
                api_key,
                base_url: base_url.trim().trim_end_matches('/').to_string(),
            }
        }
        (None, Kind::OpenAi) => {
            let settings = openai::Settings::read(get)?;
            settings.require_key()?;
            Target::OpenAi(settings)
        }
    };
    Ok(Some(Settings { model, target }))
}

fn missing_key(base_url: &str) -> anyhow::Error {
    let shown = if base_url.contains('@') {
        "fallback_base_url"
    } else {
        base_url
    };
    anyhow::anyhow!(
        "no key for the fallback model at {shown}: set fallback_api_key \
         ({API_KEY_ENV}) in the same place as fallback_base_url ({BASE_URL_ENV})"
    )
}

/// The client for the fallback model, set up like the main one (streaming,
/// and for Anthropic the thinking budget and web search).
pub fn client(
    settings: &Settings,
    config: &crate::config::Config,
) -> Result<Box<dyn super::Provider>> {
    Ok(match &settings.target {
        Target::Anthropic { api_key, base_url } => Box::new(
            super::anthropic::AnthropicProvider::new(
                api_key.clone(),
                base_url.clone(),
                settings.model.clone(),
            )?
            .with_thinking(config.thinking_budget)
            .with_streaming(config.stream)
            .with_web_search(config.web_search.clone()),
        ),
        Target::OpenAi(openai) => Box::new(
            super::openai::OpenAiProvider::new(openai.clone(), settings.model.clone())?
                .with_streaming(config.stream),
        ),
    })
}

/// `fallback_after` and `wait_limit`, with their defaults.
pub fn read_times(get: &impl Fn(&str) -> Option<String>) -> Result<(Duration, Duration)> {
    use anyhow::Context;
    let parse = |name: &str, default: Duration| -> Result<Duration> {
        match get(name) {
            None => Ok(default),
            Some(raw) if raw.trim() == "0" => Ok(Duration::ZERO),
            Some(raw) => crate::task::budget::parse_duration(&raw).context(name.to_string()),
        }
    };
    let after = parse(AFTER_ENV, DEFAULT_AFTER)?;
    if after.is_zero() {
        bail!("{AFTER_ENV} (`fallback_after`) must be longer than 0");
    }
    Ok((after, parse(WAIT_LIMIT_ENV, DEFAULT_WAIT_LIMIT)?))
}

#[cfg(test)]
mod tests;
