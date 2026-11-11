//! The settings: the file, the values, the keys (whether set, never
//! what), the Discord gateway's settings and the notification webhook.

use crate::claude_code::Backend;
use crate::config::Config;
use crate::config_file::Settings;
use crate::gateway::config::{ALLOWED_USERS_ENV, DiscordConfig, TOKEN_ENV};
use crate::notify::Notifier;

use super::{Report, shown};

const AREA: &str = "settings";

/// What the settings checks found that later checks need.
pub(super) struct Read {
    pub backend: Backend,
    /// The settings as Junees reads them, when they read.
    pub config: Option<Config>,
}

/// The settings file (parsed, every key known) and every value.
pub(super) fn settings(
    report: &mut Report,
    settings: &Settings,
    broken: Option<&anyhow::Error>,
) -> Read {
    match (settings.path(), broken) {
        (_, Some(err)) => report.fail(
            AREA,
            "config.toml",
            format!("{err:#}"),
            "fix that line, or run `junees setup` to write the file again (the checks below \
             ran without the file)",
        ),
        (Some(path), None) if settings.file_exists() => report.ok(
            AREA,
            "config.toml",
            format!("{}: reads, every key known", shown(path)),
        ),
        (Some(path), None) => report.skip(
            AREA,
            "config.toml",
            format!(
                "{} does not exist: the settings come from the environment only \
                 (`junees setup` writes it)",
                shown(path)
            ),
        ),
        (None, None) => report.skip(AREA, "config.toml", "no state folder to keep it in"),
    }
    let lookup = |key: &str| settings.get(key);
    let backend = match Backend::from_lookup(lookup) {
        Ok(backend) => {
            report.ok(
                AREA,
                "backend",
                match backend {
                    Backend::Api => "api: Junees's own agent on the Anthropic API",
                    Backend::ClaudeCode => {
                        "claude-code: the Claude Code CLI on your Claude subscription"
                    }
                },
            );
            backend
        }
        Err(err) => {
            report.fail(
                AREA,
                "backend",
                format!("{err:#}"),
                "set backend to api or claude-code (config.toml or JUNEES_BACKEND)",
            );
            Backend::Api
        }
    };
    let config = Config::from_lookup_without_key(lookup);
    let mut problems = Vec::new();
    if let Err(err) = &config {
        problems.push(format!("{err:#}"));
    }
    for result in [
        crate::cron::timezone(&lookup).map(drop),
        crate::cron::limits(&lookup).map(drop),
        crate::webhook::limits(&lookup).map(drop),
    ] {
        if let Err(err) = result {
            problems.push(format!("{err:#}"));
        }
    }
    if problems.is_empty() {
        report.ok(AREA, "values", "every setting reads");
    }
    for problem in problems {
        report.fail(
            AREA,
            "values",
            problem,
            "fix that setting in config.toml or the environment (README lists them)",
        );
    }
    Read {
        backend,
        config: config.ok(),
    }
}

/// Where the value of `name` comes from.
fn source(settings: &Settings, name: &str) -> &'static str {
    if settings.env(name).is_some() {
        "environment"
    } else {
        "config.toml"
    }
}

/// Whether the keys are set; never their values.
pub(super) fn keys(report: &mut Report, settings: &Settings, backend: Backend) {
    const AREA: &str = "keys";
    const KEY: &str = "ANTHROPIC_API_KEY";
    match (settings.get(KEY).is_some(), backend) {
        (true, _) => report.ok(
            AREA,
            "Anthropic API key",
            format!("set ({})", source(settings, KEY)),
        ),
        (false, Backend::Api) => report.fail(
            AREA,
            "Anthropic API key",
            "not set: the api backend needs it",
            "run `junees setup`, or set ANTHROPIC_API_KEY (get a key at \
             https://console.anthropic.com); or use the claude-code backend",
        ),
        (false, Backend::ClaudeCode) => report.skip(
            AREA,
            "Anthropic API key",
            "not set (the claude-code backend does not need it)",
        ),
    }
    let github = crate::github::TOKEN_ENV;
    if settings.get(github).is_some() {
        report.ok(
            AREA,
            "GitHub token",
            format!(
                "set ({}); /github shows its rate limit",
                source(settings, github)
            ),
        );
    } else {
        report.skip(AREA, "GitHub token", "not set (the github tool is off)");
    }
    let webhook = crate::notify::WEBHOOK_ENV;
    match Notifier::from_lookup(|key| settings.get(key)) {
        Ok(notifier) => match notifier.host() {
            Some(host) => report.ok(
                AREA,
                "notify webhook",
                format!(
                    "set ({}); `task run` posts to {host}",
                    source(settings, webhook)
                ),
            ),
            None => report.skip(AREA, "notify webhook", "not set (no task notifications)"),
        },
        Err(err) => report.fail(
            AREA,
            "notify webhook",
            format!("{err:#}"),
            "set notify_webhook to the https URL of a Discord webhook",
        ),
    }
}

/// The Discord gateway's token and allowed users.
pub(super) fn discord(report: &mut Report, settings: &Settings) {
    const AREA: &str = "discord";
    if settings.get(TOKEN_ENV).is_none() && settings.get(ALLOWED_USERS_ENV).is_none() {
        report.skip(
            AREA,
            "gateway",
            "not set up (`junees gateway` needs discord_token and discord_allowed_users)",
        );
        return;
    }
    match DiscordConfig::from_lookup(|key| settings.get(key)) {
        Ok(config) => report.ok(
            AREA,
            "gateway",
            format!(
                "token set ({}), {} allowed users, {} channels answered without a mention",
                source(settings, TOKEN_ENV),
                config.allowed_users.len(),
                config.channels.len()
            ),
        ),
        Err(err) => report.fail(
            AREA,
            "gateway",
            format!("{err:#}"),
            "set discord_token and discord_allowed_users (`junees setup`; README, 디스코드 \
             게이트웨이)",
        ),
    }
}
