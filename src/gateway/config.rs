//! The gateway's settings: the bot token, who may talk to the bot, where
//! it answers without a mention, and its limits.

use std::collections::HashSet;
use std::time::Duration;

use anyhow::{Context, Result, bail};

use crate::task::budget::parse_duration;

/// The bot token (`discord_token`).
pub const TOKEN_ENV: &str = "JUNEES_DISCORD_TOKEN";
/// Discord user ids whose messages the bot handles (`discord_allowed_users`).
pub const ALLOWED_USERS_ENV: &str = "JUNEES_DISCORD_ALLOWED_USERS";
/// Channel and thread ids where the bot answers without a mention
/// (`discord_channels`).
pub const CHANNELS_ENV: &str = "JUNEES_DISCORD_CHANNELS";
/// How many channels may run a turn at the same time (`discord_parallel`).
pub const PARALLEL_ENV: &str = "JUNEES_DISCORD_PARALLEL";
/// How long an approval question waits for its answer
/// (`discord_approval_timeout`).
pub const APPROVAL_TIMEOUT_ENV: &str = "JUNEES_DISCORD_APPROVAL_TIMEOUT";
/// The REST API base URL. Only for tests and proxies, so it is not a key
/// of the settings file.
pub const API_URL_ENV: &str = "JUNEES_DISCORD_API_URL";

/// Discord's REST API, version 10.
pub const DEFAULT_API_URL: &str = "https://discord.com/api/v10";
pub const DEFAULT_PARALLEL: usize = 2;
pub const DEFAULT_APPROVAL_TIMEOUT: Duration = Duration::from_secs(5 * 60);

/// The bot token. It is only ever put into the `Authorization` header and
/// the gateway's IDENTIFY and RESUME payloads; `Debug` hides it and there
/// is no `Display`, so it cannot end up in a log line or an error.
#[derive(Clone)]
pub struct Token(String);

impl Token {
    /// A token as the Developer Portal shows it; a leading `Bot ` (from a
    /// copied header) is dropped.
    pub fn new(raw: &str) -> Result<Self> {
        let raw = raw.trim();
        let raw = raw.strip_prefix("Bot ").unwrap_or(raw).trim();
        if raw.is_empty() {
            bail!("the Discord bot token is empty");
        }
        // Never repeat what was given: it is a secret.
        if raw.chars().any(|c| c.is_whitespace() || c.is_control()) {
            bail!("the Discord bot token has spaces or control characters in it");
        }
        Ok(Self(raw.to_string()))
    }

    /// The token itself, for the places Discord needs it.
    pub fn expose(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Debug for Token {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Token(hidden)")
    }
}

/// The gateway's settings. Not `Debug` as a whole, like [`crate::config::Config`].
#[derive(Clone)]
pub struct DiscordConfig {
    pub token: Token,
    /// User ids (snowflakes) the bot listens to; never empty.
    pub allowed_users: HashSet<String>,
    /// Channel and thread ids where every message of an allowed user is
    /// for the bot, mention or not.
    pub channels: HashSet<String>,
    pub parallel: usize,
    pub approval_timeout: Duration,
    /// REST base URL without a trailing slash.
    pub api_url: String,
    /// Another base URL attachments may be downloaded from besides
    /// Discord's CDN ([`crate::images::discord::CDN_URL_ENV`]).
    pub cdn_url: Option<String>,
}

impl DiscordConfig {
    /// Reads the settings through `lookup` (environment first, then the
    /// settings file; see [`crate::config_file::Settings`]).
    pub fn from_lookup(lookup: impl Fn(&str) -> Option<String>) -> Result<Self> {
        let get = |key: &str| {
            lookup(key)
                .map(|value| value.trim().to_string())
                .filter(|value| !value.is_empty())
        };
        let token = get(TOKEN_ENV).context(
            "no Discord bot token: set discord_token in the settings file (`junees setup`) or \
             JUNEES_DISCORD_TOKEN (see README, \"디스코드 게이트웨이\")",
        )?;
        let token = Token::new(&token)?;
        let allowed_users = snowflakes(get(ALLOWED_USERS_ENV), ALLOWED_USERS_ENV)?;
        if allowed_users.is_empty() {
            bail!(
                "no allowed Discord users: set discord_allowed_users (or {ALLOWED_USERS_ENV}) to \
                 the user ids the bot may answer, separated by commas. Without the list anyone \
                 who can message the bot could run commands on this machine."
            );
        }
        let channels = snowflakes(get(CHANNELS_ENV), CHANNELS_ENV)?;
        let parallel = match get(PARALLEL_ENV) {
            None => DEFAULT_PARALLEL,
            Some(raw) => match raw.parse::<usize>() {
                Ok(value) if value > 0 => value,
                _ => bail!("{PARALLEL_ENV} must be a positive integer, got `{raw}`"),
            },
        };
        let approval_timeout = match get(APPROVAL_TIMEOUT_ENV) {
            None => DEFAULT_APPROVAL_TIMEOUT,
            Some(raw) => parse_duration(&raw).context(APPROVAL_TIMEOUT_ENV)?,
        };
        let api_url = get(API_URL_ENV).unwrap_or_else(|| DEFAULT_API_URL.to_string());
        check_url(&api_url, "https", "http").context(API_URL_ENV)?;
        let cdn_url = get(crate::images::discord::CDN_URL_ENV);
        if let Some(url) = &cdn_url {
            check_url(url, "https", "http").context(crate::images::discord::CDN_URL_ENV)?;
        }
        Ok(Self {
            token,
            allowed_users,
            channels,
            parallel,
            approval_timeout,
            api_url: api_url.trim_end_matches('/').to_string(),
            cdn_url,
        })
    }
}

/// Splits a comma list of Discord ids, each all digits.
pub fn snowflakes(value: Option<String>, name: &str) -> Result<HashSet<String>> {
    let mut ids = HashSet::new();
    for item in value.iter().flat_map(|value| value.split(',')) {
        let item = item.trim();
        if item.is_empty() {
            continue;
        }
        if !item.bytes().all(|b| b.is_ascii_digit()) || item.len() > 20 {
            bail!(
                "{name} takes Discord ids (numbers such as 123456789012345678; turn on Developer \
                 Mode in Discord and use Copy User ID / Copy Channel ID), got `{item}`"
            );
        }
        ids.insert(item.to_string());
    }
    Ok(ids)
}

/// Checks that `url` uses the `secure` scheme, or `plain` on this machine
/// only: the bot token goes with every request, and must not cross the
/// network in clear text.
pub fn check_url(url: &str, secure: &str, plain: &str) -> Result<()> {
    let Some((scheme, rest)) = url.split_once("://") else {
        bail!("`{url}` is not a URL");
    };
    let authority = rest.split(['/', '?', '#']).next().unwrap_or_default();
    let authority = authority
        .rsplit_once('@')
        .map_or(authority, |(_, host)| host);
    let host = match authority.strip_prefix('[') {
        Some(bracketed) => bracketed.split(']').next().unwrap_or_default(),
        None => authority.split(':').next().unwrap_or_default(),
    };
    let scheme = scheme.to_ascii_lowercase();
    if scheme == secure {
        return Ok(());
    }
    if scheme == plain && matches!(host, "127.0.0.1" | "localhost" | "::1") {
        return Ok(());
    }
    bail!("`{scheme}://{host}` must use {secure}:// ({plain}:// only on this machine)")
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use super::*;

    fn read(pairs: &[(&str, &str)]) -> Result<DiscordConfig> {
        let map: HashMap<String, String> = pairs
            .iter()
            .map(|(key, value)| (key.to_string(), value.to_string()))
            .collect();
        DiscordConfig::from_lookup(|key| map.get(key).cloned())
    }

    const TOKEN: &str = "token-for-tests";

    #[test]
    fn reads_the_settings() {
        let config = read(&[
            (TOKEN_ENV, &format!("Bot {TOKEN}")),
            (ALLOWED_USERS_ENV, " 111, 222 ,,"),
            (CHANNELS_ENV, "333"),
            (PARALLEL_ENV, "3"),
            (APPROVAL_TIMEOUT_ENV, "90s"),
            (API_URL_ENV, "http://127.0.0.1:9/api/"),
        ])
        .unwrap();
        assert_eq!(config.token.expose(), TOKEN);
        assert_eq!(
            config.allowed_users,
            HashSet::from(["111".to_string(), "222".to_string()])
        );
        assert_eq!(config.channels, HashSet::from(["333".to_string()]));
        assert_eq!(config.parallel, 3);
        assert_eq!(config.approval_timeout, Duration::from_secs(90));
        assert_eq!(config.api_url, "http://127.0.0.1:9/api");

        let config = read(&[(TOKEN_ENV, TOKEN), (ALLOWED_USERS_ENV, "1")]).unwrap();
        assert_eq!(config.parallel, DEFAULT_PARALLEL);
        assert_eq!(config.approval_timeout, DEFAULT_APPROVAL_TIMEOUT);
        assert_eq!(config.api_url, DEFAULT_API_URL);
        assert!(config.channels.is_empty());
        assert_eq!(config.cdn_url, None);

        let cdn = crate::images::discord::CDN_URL_ENV;
        let config = read(&[
            (TOKEN_ENV, TOKEN),
            (ALLOWED_USERS_ENV, "1"),
            (cdn, "http://127.0.0.1:9/"),
        ])
        .unwrap();
        assert_eq!(config.cdn_url.as_deref(), Some("http://127.0.0.1:9/"));
        let err = read(&[
            (TOKEN_ENV, TOKEN),
            (ALLOWED_USERS_ENV, "1"),
            (cdn, "http://cdn.example/"),
        ])
        .err()
        .unwrap();
        assert!(format!("{err:#}").contains(cdn), "{err:#}");
    }

    #[test]
    fn refuses_to_start_without_allowed_users() {
        for users in [None, Some(""), Some(" , ")] {
            let mut pairs = vec![(TOKEN_ENV, TOKEN)];
            if let Some(users) = users {
                pairs.push((ALLOWED_USERS_ENV, users));
            }
            let err = read(&pairs).err().unwrap();
            assert!(
                format!("{err:#}").contains("no allowed Discord users"),
                "{err:#}"
            );
        }
        let err = read(&[(ALLOWED_USERS_ENV, "1")]).err().unwrap();
        assert!(format!("{err:#}").contains("no Discord bot token"));
        let err = read(&[(TOKEN_ENV, TOKEN), (ALLOWED_USERS_ENV, "someone")])
            .err()
            .unwrap();
        assert!(format!("{err:#}").contains("Discord ids"), "{err:#}");
    }

    #[test]
    fn errors_never_show_the_token() {
        let secret = "secret part";
        let err = read(&[(TOKEN_ENV, secret), (ALLOWED_USERS_ENV, "1")])
            .err()
            .unwrap();
        assert!(!format!("{err:#}").contains(secret), "{err:#}");
        let token = Token::new(TOKEN).unwrap();
        assert_eq!(format!("{token:?}"), "Token(hidden)");
        for bad in [(PARALLEL_ENV, "0"), (APPROVAL_TIMEOUT_ENV, "soon")] {
            let err = read(&[(TOKEN_ENV, TOKEN), (ALLOWED_USERS_ENV, "1"), bad])
                .err()
                .unwrap();
            assert!(!format!("{err:#}").contains(TOKEN), "{err:#}");
        }
    }

    #[test]
    fn plain_urls_only_on_this_machine() {
        for url in [
            "https://discord.com/api/v10",
            "http://127.0.0.1:8080",
            "http://localhost/api",
            "http://[::1]:9/x",
        ] {
            check_url(url, "https", "http").unwrap();
        }
        for url in [
            "http://discord.com/api",
            "http://127.0.0.1.evil.example/",
            "http://user@evil.example",
            "ftp://127.0.0.1/",
            "discord.com",
        ] {
            assert!(check_url(url, "https", "http").is_err(), "{url}");
        }
        check_url("wss://gateway.discord.gg", "wss", "ws").unwrap();
        check_url("ws://127.0.0.1:1", "wss", "ws").unwrap();
        assert!(check_url("ws://gateway.discord.gg", "wss", "ws").is_err());
    }
}
