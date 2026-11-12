//! The Telegram gateway's settings: the bot token, who may talk to the
//! bot, and its limits.

use std::collections::HashSet;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use reqwest::Url;

use crate::gateway::config::{DEFAULT_APPROVAL_TIMEOUT, DEFAULT_PARALLEL};
use crate::task::budget::parse_duration;

/// The bot token (`telegram_token`).
pub const TOKEN_ENV: &str = "JUNEES_TELEGRAM_TOKEN";
/// Telegram user ids whose messages the bot handles
/// (`telegram_allowed_users`).
pub const ALLOWED_USERS_ENV: &str = "JUNEES_TELEGRAM_ALLOWED_USERS";
/// How many chats may run a turn at the same time (`telegram_parallel`).
pub const PARALLEL_ENV: &str = "JUNEES_TELEGRAM_PARALLEL";
/// How long an approval question waits for its answer
/// (`telegram_approval_timeout`).
pub const APPROVAL_TIMEOUT_ENV: &str = "JUNEES_TELEGRAM_APPROVAL_TIMEOUT";
/// How replies are written (`telegram_format`): `html` (the default) or
/// `plain`.
pub const FORMAT_ENV: &str = "JUNEES_TELEGRAM_FORMAT";
/// The Bot API base URL. Only for tests: it must be on this machine, and
/// it is not a key of the settings file.
pub const API_URL_ENV: &str = "JUNEES_TELEGRAM_API_URL";

/// The Bot API ("Making requests": `https://api.telegram.org/bot<token>/METHOD_NAME`).
pub const DEFAULT_API_URL: &str = "https://api.telegram.org";

/// The largest user id the Bot API hands out: ids "have at most 52
/// significant bits" (Bot API, User `id`).
const MAX_ID: i64 = (1 << 52) - 1;

/// The bot token. The Bot API takes it in the URL path of every request
/// (`/bot<token>/METHOD_NAME`, and `/file/bot<token>/<file_path>` for
/// downloads), so a request URL is never shown: `Debug` hides the token,
/// there is no `Display`, and [`super::api`] builds errors without URLs.
#[derive(Clone)]
pub struct Token(String);

impl Token {
    /// A token as @BotFather shows it (`123456:ABC-DEF...`); a leading
    /// `bot` (from a copied URL) is dropped. Anything else is refused, so
    /// the token cannot change the path it is put in.
    pub fn new(raw: &str) -> Result<Self> {
        let raw = raw.trim();
        let raw = raw.strip_prefix("bot").unwrap_or(raw);
        if raw.is_empty() {
            bail!("the Telegram bot token is empty");
        }
        // Never repeat what was given: it is a secret.
        let valid = raw.split_once(':').is_some_and(|(id, secret)| {
            !id.is_empty()
                && id.len() <= 20
                && id.bytes().all(|b| b.is_ascii_digit())
                && !secret.is_empty()
                && secret
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
        });
        if !valid {
            bail!(
                "the Telegram bot token does not look like one (digits, a colon, then letters, \
                 digits, `_` and `-`, as @BotFather shows it)"
            );
        }
        Ok(Self(raw.to_string()))
    }

    /// The token itself, for the request paths.
    pub fn expose(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Debug for Token {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Token(hidden)")
    }
}

/// How replies are sent.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Format {
    /// `parse_mode` HTML: the model's Markdown becomes Telegram's
    /// formatting ([`super::format`]).
    #[default]
    Html,
    /// No `parse_mode`: the text shows as written.
    Plain,
}

/// The gateway's settings. Not `Debug` as a whole, like [`crate::config::Config`].
#[derive(Clone)]
pub struct TelegramConfig {
    pub token: Token,
    /// User ids the bot listens to; never empty.
    pub allowed_users: HashSet<i64>,
    pub parallel: usize,
    pub approval_timeout: Duration,
    pub format: Format,
    /// Bot API base URL without a trailing slash: [`DEFAULT_API_URL`], or
    /// a URL on this machine.
    pub api_url: String,
}

impl TelegramConfig {
    /// Reads the settings through `lookup` (environment first, then the
    /// settings file; see [`crate::config_file::Settings`]).
    pub fn from_lookup(lookup: impl Fn(&str) -> Option<String>) -> Result<Self> {
        let get = |key: &str| {
            lookup(key)
                .map(|value| value.trim().to_string())
                .filter(|value| !value.is_empty())
        };
        let token = get(TOKEN_ENV).context(
            "no Telegram bot token: set telegram_token in the settings file or \
             JUNEES_TELEGRAM_TOKEN (see README, \"텔레그램 게이트웨이\")",
        )?;
        let token = Token::new(&token)?;
        let allowed_users = user_ids(get(ALLOWED_USERS_ENV), ALLOWED_USERS_ENV)?;
        if allowed_users.is_empty() {
            bail!(
                "no allowed Telegram users: set telegram_allowed_users (or {ALLOWED_USERS_ENV}) \
                 to the numeric user ids the bot may answer, separated by commas. Without the \
                 list anyone who finds the bot could run commands on this machine."
            );
        }
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
        let format = match get(FORMAT_ENV)
            .map(|raw| raw.to_ascii_lowercase())
            .as_deref()
        {
            None | Some("html") => Format::Html,
            Some("plain") => Format::Plain,
            Some(_) => bail!("{FORMAT_ENV} (telegram_format) must be `html` or `plain`"),
        };
        let api_url = match get(API_URL_ENV) {
            None => DEFAULT_API_URL.to_string(),
            Some(url) => check_api_url(&url).context(API_URL_ENV)?,
        };
        Ok(Self {
            token,
            allowed_users,
            parallel,
            approval_timeout,
            format,
            api_url,
        })
    }
}

/// Splits a comma list of Telegram user ids (positive numbers).
pub fn user_ids(value: Option<String>, name: &str) -> Result<HashSet<i64>> {
    let mut ids = HashSet::new();
    for item in value.iter().flat_map(|value| value.split(',')) {
        let item = item.trim();
        if item.is_empty() {
            continue;
        }
        match item.parse::<i64>() {
            Ok(id) if item.bytes().all(|b| b.is_ascii_digit()) && (1..=MAX_ID).contains(&id) => {
                ids.insert(id);
            }
            _ => bail!(
                "{name} takes Telegram user ids (numbers such as 123456789; a bot such as \
                 @userinfobot tells yours), got `{item}`"
            ),
        }
    }
    Ok(ids)
}

/// The base URL `url` without a trailing slash, if it is the Bot API's
/// own or on this machine (http or https, a loopback address or
/// `localhost`): the token goes in every request path, and files are only
/// downloaded from there.
pub fn check_api_url(url: &str) -> Result<String> {
    let trimmed = url.trim().trim_end_matches('/');
    if trimmed == DEFAULT_API_URL {
        return Ok(trimmed.to_string());
    }
    let parsed = Url::parse(trimmed).map_err(|_| anyhow::anyhow!("`{trimmed}` is not a URL"))?;
    let host = parsed.host_str().unwrap_or_default();
    let ip = host.trim_start_matches('[').trim_end_matches(']');
    let loopback = host == "localhost"
        || ip
            .parse::<std::net::IpAddr>()
            .is_ok_and(|ip| ip.is_loopback());
    let plain = matches!(parsed.scheme(), "http" | "https")
        && parsed.username().is_empty()
        && parsed.password().is_none()
        && parsed.query().is_none()
        && parsed.fragment().is_none();
    if !(loopback && plain) {
        bail!(
            "the Bot API URL must be {DEFAULT_API_URL} or a URL on this machine (for tests), \
             got `{trimmed}`"
        );
    }
    Ok(trimmed.to_string())
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use super::*;

    fn read(pairs: &[(&str, &str)]) -> Result<TelegramConfig> {
        let map: HashMap<String, String> = pairs
            .iter()
            .map(|(key, value)| (key.to_string(), value.to_string()))
            .collect();
        TelegramConfig::from_lookup(|key| map.get(key).cloned())
    }

    const TOKEN: &str = "123456:test-token_for-tests";

    #[test]
    fn reads_the_settings() {
        let config = read(&[
            (TOKEN_ENV, &format!(" bot{TOKEN} ")),
            (ALLOWED_USERS_ENV, " 111, 222 ,,"),
            (PARALLEL_ENV, "3"),
            (APPROVAL_TIMEOUT_ENV, "90s"),
            (API_URL_ENV, "http://127.0.0.1:9/tg/"),
        ])
        .unwrap();
        assert_eq!(config.token.expose(), TOKEN);
        assert_eq!(config.allowed_users, HashSet::from([111, 222]));
        assert_eq!(config.parallel, 3);
        assert_eq!(config.approval_timeout, Duration::from_secs(90));
        assert_eq!(config.api_url, "http://127.0.0.1:9/tg");

        let config = read(&[(TOKEN_ENV, TOKEN), (ALLOWED_USERS_ENV, "1")]).unwrap();
        assert_eq!(config.parallel, DEFAULT_PARALLEL);
        assert_eq!(config.approval_timeout, DEFAULT_APPROVAL_TIMEOUT);
        assert_eq!(config.api_url, DEFAULT_API_URL);
        assert_eq!(config.format, Format::Html);
    }

    #[test]
    fn reads_the_reply_format() {
        let base = [(TOKEN_ENV, TOKEN), (ALLOWED_USERS_ENV, "1")];
        for (raw, format) in [
            ("html", Format::Html),
            (" HTML ", Format::Html),
            ("plain", Format::Plain),
            ("Plain", Format::Plain),
        ] {
            let mut pairs = base.to_vec();
            pairs.push((FORMAT_ENV, raw));
            assert_eq!(read(&pairs).unwrap().format, format, "{raw}");
        }
        let mut pairs = base.to_vec();
        pairs.push((FORMAT_ENV, "markdown"));
        let err = read(&pairs).err().unwrap();
        assert!(
            format!("{err:#}").contains("must be `html` or `plain`"),
            "{err:#}"
        );
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
                format!("{err:#}").contains("no allowed Telegram users"),
                "{err:#}"
            );
        }
        let err = read(&[(ALLOWED_USERS_ENV, "1")]).err().unwrap();
        assert!(format!("{err:#}").contains("no Telegram bot token"));
        for bad in ["@someone", "-5", "0", "1.5", "99999999999999999999"] {
            let err = read(&[(TOKEN_ENV, TOKEN), (ALLOWED_USERS_ENV, bad)])
                .err()
                .unwrap();
            assert!(format!("{err:#}").contains("Telegram user ids"), "{err:#}");
        }
    }

    #[test]
    fn tokens_that_could_change_the_path_are_refused_without_being_shown() {
        for bad in [
            "123456",
            ":abc",
            "123:",
            "12a:abc",
            "123:abc/../x",
            "123:abc?x=1",
            "123:abc#x",
            "123:a b",
            "123:abc%2F",
        ] {
            let err = read(&[(TOKEN_ENV, bad), (ALLOWED_USERS_ENV, "1")])
                .err()
                .unwrap();
            let text = format!("{err:#}");
            assert!(text.contains("does not look like one"), "{text}");
            assert!(!text.contains(bad.trim_start_matches("123")), "{text}");
        }
        let token = Token::new(TOKEN).unwrap();
        assert_eq!(format!("{token:?}"), "Token(hidden)");
        // bash, MCP servers and doctor hide it like the other secrets.
        assert!(crate::tools::SECRET_ENV_VARS.contains(&TOKEN_ENV));
        for bad in [(PARALLEL_ENV, "0"), (APPROVAL_TIMEOUT_ENV, "soon")] {
            let err = read(&[(TOKEN_ENV, TOKEN), (ALLOWED_USERS_ENV, "1"), bad])
                .err()
                .unwrap();
            assert!(!format!("{err:#}").contains("test-token"), "{err:#}");
        }
    }

    #[test]
    fn the_api_url_is_telegrams_or_on_this_machine() {
        for url in [
            "https://api.telegram.org",
            "https://api.telegram.org/",
            "http://127.0.0.1:8081",
            "https://localhost/tg",
            "http://[::1]:9/x",
            "http://127.0.0.2:9",
        ] {
            check_api_url(url).unwrap();
        }
        for url in [
            "http://api.telegram.org",
            "https://api.telegram.org.evil.example",
            "https://evil.example/api.telegram.org",
            "https://api.telegram.org:8443",
            "http://127.0.0.1.evil.example/",
            "http://user@127.0.0.1/",
            "http://127.0.0.1/?x=1",
            "ftp://127.0.0.1/",
            "api.telegram.org",
        ] {
            assert!(check_api_url(url).is_err(), "{url}");
        }
    }
}
