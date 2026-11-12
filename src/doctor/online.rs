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

/// `GET /v1/models?limit=1` with the key: no tokens are used.
pub(super) async fn api_key(report: &mut Report, settings: &Settings) {
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
