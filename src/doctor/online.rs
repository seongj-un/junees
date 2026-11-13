//! `--online`: one light request that shows whether the API takes the key,
//! and one that asks GitHub for the latest release.

use std::time::Duration;

use crate::config::DEFAULT_BASE_URL;
use crate::config_file::Settings;

use super::Report;

const AREA: &str = "online";
const TIMEOUT: Duration = Duration::from_secs(10);

/// Whether a newer release is out, as `junees update --check` says.
pub(super) async fn release(report: &mut Report, settings: &Settings) {
    const NAME: &str = "release";
    let lookup = |key: &str| settings.env(key);
    let client =
        match crate::update::Source::from_lookup(&lookup).and_then(crate::update::Client::new) {
            Ok(client) => client,
            Err(err) => {
                report.warn(
                    AREA,
                    NAME,
                    format!("cannot look for a newer release: {err:#}"),
                    format!("unset {}", crate::update::BASE_ENV),
                );
                return;
            }
        };
    let current = crate::update::this_version();
    match client.latest().await {
        Ok(None) => report.skip(AREA, NAME, "no release is published yet"),
        Ok(Some(latest)) if latest > current => report.warn(
            AREA,
            NAME,
            format!("junees {latest} is out (this is {current})"),
            "run `junees update`",
        ),
        Ok(latest) => report.ok(
            AREA,
            NAME,
            crate::update::compare(&current, latest.as_ref()),
        ),
        Err(err) => report.warn(
            AREA,
            NAME,
            format!("{err:#}"),
            "check the network; `junees update --check` asks the same",
        ),
    }
}

/// `auth.test` with the Slack bot token, when the Slack gateway is set
/// up: whether Slack takes it, and which bot it is. The app-level token
/// is not tried: `apps.connections.open` would open a connection.
pub(super) async fn slack(report: &mut Report, settings: &Settings) {
    use crate::slack::api::Api;
    use crate::slack::config::SlackConfig;
    const NAME: &str = "Slack bot token";
    let Ok(config) = SlackConfig::from_lookup(|key| settings.get(key)) else {
        report.skip(AREA, NAME, "the Slack gateway is not set up");
        return;
    };
    let quiet: crate::gateway::Log = std::sync::Arc::new(|_: &str| {});
    let api = match Api::new(&config, quiet) {
        Ok(api) => api,
        Err(err) => {
            report.warn(AREA, NAME, format!("{err:#}"), "try again");
            return;
        }
    };
    match tokio::time::timeout(TIMEOUT * 3, api.auth_test()).await {
        Ok(Ok(me)) => report.ok(
            AREA,
            NAME,
            format!(
                "Slack takes it: bot {} ({}){}",
                crate::prompt_text::escape_hidden(&me.name),
                me.user_id,
                if me.team.is_empty() {
                    String::new()
                } else {
                    format!(" in {}", crate::prompt_text::escape_hidden(&me.team))
                }
            ),
        ),
        Ok(Err(err)) if err.is_fatal() => report.fail(
            AREA,
            NAME,
            format!("{err}"),
            "copy the Bot User OAuth Token (xoxb-) from the app's OAuth & Permissions page again",
        ),
        Ok(Err(err)) => report.warn(
            AREA,
            NAME,
            format!("{err}"),
            "check the network; try again later",
        ),
        Err(_) => report.warn(
            AREA,
            NAME,
            "Slack did not answer auth.test in time",
            "check the network; try again later",
        ),
    }
}

/// `GET /v1/models?limit=1` with the key: no tokens are used. Only on
/// the Anthropic provider.
pub(super) async fn api_key(report: &mut Report, settings: &Settings) {
    let provider =
        crate::provider::Kind::parse(settings.get(crate::provider::PROVIDER_ENV).as_deref());
    if provider.is_ok_and(|kind| kind != crate::provider::Kind::Anthropic) {
        return;
    }
    let Some(key) = settings.get("ANTHROPIC_API_KEY") else {
        report.skip(AREA, "API key", "no key to check");
        return;
    };
    let base = settings
        .get("ANTHROPIC_BASE_URL")
        .unwrap_or_else(|| DEFAULT_BASE_URL.to_string());
    let url = format!("{}/v1/models?limit=1", base.trim().trim_end_matches('/'));
    let host = reqwest::Url::parse(&url)
        .ok()
        .and_then(|url| url.host_str().map(str::to_string))
        .unwrap_or_else(|| "the API".to_string());
    let client = match reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .timeout(TIMEOUT)
        .build()
    {
        Ok(client) => client,
        Err(err) => {
            report.warn(
                AREA,
                "API key",
                format!("cannot set up the HTTP client: {}", err.without_url()),
                "try again",
            );
            return;
        }
    };
    let response = client
        .get(&url)
        .header("x-api-key", key.trim())
        .header("anthropic-version", crate::provider::anthropic::API_VERSION)
        .send()
        .await;
    match response {
        Err(err) => report.warn(
            AREA,
            "API key",
            format!(
                "no answer from {host}: {}",
                crate::notify::describe(err, TIMEOUT)
            ),
            "check the network and base_url (ANTHROPIC_BASE_URL)",
        ),
        Ok(response) => match response.status().as_u16() {
            200..=299 => report.ok(AREA, "API key", format!("{host} accepts the key")),
            status @ (401 | 403) => report.fail(
                AREA,
                "API key",
                format!("{host} refused the key (HTTP {status})"),
                "make a new key at https://console.anthropic.com and run `junees setup` (or \
                 set ANTHROPIC_API_KEY)",
            ),
            status => report.warn(
                AREA,
                "API key",
                format!("{host} answered HTTP {status}, so the key is not confirmed"),
                "try again later; check base_url if it stays",
            ),
        },
    }
}

/// With provider = openai, `GET <openai_base_url>/models` with the key
/// (none for a server on this machine without one): whether the server
/// takes it, and whether it lists the model. No tokens are used.
pub(super) async fn openai_models(report: &mut Report, settings: &Settings) {
    use crate::provider::{Kind, PROVIDER_ENV, openai};
    const NAME: &str = "OpenAI-compatible API";
    if Kind::parse(settings.get(PROVIDER_ENV).as_deref()).ok() != Some(Kind::OpenAi) {
        return;
    }
    let get = |key: &str| {
        settings
            .get(key)
            .map(|value| value.trim().to_string())
            .filter(|value| !value.is_empty())
    };
    let Ok(openai) = openai::Settings::read(get) else {
        report.skip(AREA, NAME, "openai_base_url does not read (see settings)");
        return;
    };
    if openai.api_key.is_none() && !openai.is_local() {
        report.skip(AREA, NAME, "no key to check");
        return;
    }
    let url = format!("{}/models", openai.base_url);
    let host = reqwest::Url::parse(&url)
        .ok()
        .and_then(|url| url.host_str().map(str::to_string))
        .unwrap_or_else(|| "the API".to_string());
    let client = match reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .timeout(TIMEOUT)
        .build()
    {
        Ok(client) => client,
        Err(err) => {
            report.warn(
                AREA,
                NAME,
                format!("cannot set up the HTTP client: {}", err.without_url()),
                "try again",
            );
            return;
        }
    };
    let mut request = client.get(&url);
    if let Some(key) = &openai.api_key {
        request = request.bearer_auth(key);
    }
    let response = match request.send().await {
        Ok(response) => response,
        Err(err) => {
            report.warn(
                AREA,
                NAME,
                format!(
                    "no answer from {host}: {}",
                    crate::notify::describe(err, TIMEOUT)
                ),
                "check the network and openai_base_url (OPENAI_BASE_URL); is the server running?",
            );
            return;
        }
    };
    let status = response.status().as_u16();
    if matches!(status, 401 | 403) {
        report.fail(
            AREA,
            NAME,
            format!("{host} refused the key (HTTP {status})"),
            "check openai_api_key (OPENAI_API_KEY)",
        );
        return;
    }
    if !(200..300).contains(&status) {
        report.warn(
            AREA,
            NAME,
            format!("{host} answered HTTP {status} to GET /models, so the key is not confirmed"),
            "try again later; check openai_base_url if it stays",
        );
        return;
    }
    let models: Vec<String> = match response.json::<serde_json::Value>().await {
        Ok(body) => body["data"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|model| model["id"].as_str().map(str::to_string))
            .collect(),
        Err(_) => Vec::new(),
    };
    let taken = if openai.api_key.is_some() {
        format!("{host} accepts the key")
    } else {
        format!("{host} answers without a key")
    };
    match settings
        .get("JUNEES_MODEL")
        .map(|model| model.trim().to_string())
    {
        Some(model) if !models.is_empty() && !models.contains(&model) => report.warn(
            AREA,
            NAME,
            format!(
                "{taken}, but the model `{model}` is not among the {} models it lists",
                models.len()
            ),
            format!(
                "set model to one it lists, such as {}",
                models
                    .iter()
                    .take(5)
                    .map(|model| format!("`{model}`"))
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
        ),
        Some(model) if !models.is_empty() => {
            report.ok(AREA, NAME, format!("{taken} and lists the model `{model}`"))
        }
        _ => report.ok(
            AREA,
            NAME,
            format!("{taken} ({} models listed)", models.len()),
        ),
    }
}

/// With `--online` and transcription on, `GET <base>/models`: a warning
/// at most, since a whisper server on this machine may not have it.
pub(super) async fn transcribe(report: &mut Report, settings: &Settings) {
    const NAME: &str = "transcription API";
    let Ok(Some(voice)) = crate::transcribe::Settings::read(|key| settings.get(key)) else {
        return;
    };
    let url = format!("{}/models", voice.base_url);
    let host = reqwest::Url::parse(&url)
        .ok()
        .and_then(|url| url.host_str().map(str::to_string))
        .unwrap_or_else(|| "the API".to_string());
    let Ok(client) = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .timeout(TIMEOUT)
        .build()
    else {
        report.warn(AREA, NAME, "cannot set up the HTTP client", "try again");
        return;
    };
    let mut request = client.get(&url);
    if let Some(key) = voice.key() {
        request = request.bearer_auth(key);
    }
    let fix = "check transcribe_base_url and transcribe_api_key; a local whisper server may \
               not answer GET /models, and voice messages can still work";
    match request.send().await {
        Err(err) => report.warn(
            AREA,
            NAME,
            format!(
                "no answer from {host}: {}",
                crate::notify::describe(err, TIMEOUT)
            ),
            fix,
        ),
        Ok(response) => {
            let status = response.status().as_u16();
            if matches!(status, 401 | 403) {
                report.warn(
                    AREA,
                    NAME,
                    format!("{host} refused the key (HTTP {status})"),
                    fix,
                );
            } else if (200..300).contains(&status) {
                report.ok(AREA, NAME, format!("{host} answers GET /models"));
            } else {
                report.warn(
                    AREA,
                    NAME,
                    format!("{host} answered HTTP {status} to GET /models"),
                    fix,
                );
            }
        }
    }
}
