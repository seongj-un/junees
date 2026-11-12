//! The Slack gateway's settings: the bot and app tokens, who may talk to
//! the bot, and its limits.

use std::collections::HashSet;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use reqwest::Url;

use crate::gateway::config::{DEFAULT_APPROVAL_TIMEOUT, DEFAULT_PARALLEL};
use crate::task::budget::parse_duration;

/// The bot token, `xoxb-...` (`slack_bot_token`): posts, `auth.test` and
/// file downloads.
pub const BOT_TOKEN_ENV: &str = "JUNEES_SLACK_BOT_TOKEN";
/// The app-level token, `xapp-...` (`slack_app_token`): opens the Socket
/// Mode connection.
pub const APP_TOKEN_ENV: &str = "JUNEES_SLACK_APP_TOKEN";
/// Slack user ids whose messages the bot handles (`slack_allowed_users`).
pub const ALLOWED_USERS_ENV: &str = "JUNEES_SLACK_ALLOWED_USERS";
/// How many conversations may run a turn at the same time
/// (`slack_parallel`).
pub const PARALLEL_ENV: &str = "JUNEES_SLACK_PARALLEL";
/// How long an approval question waits for its answer
/// (`slack_approval_timeout`).
pub const APPROVAL_TIMEOUT_ENV: &str = "JUNEES_SLACK_APPROVAL_TIMEOUT";
/// The Web API base URL. Only for tests: it must be on this machine, and
/// it is not a key of the settings file. With it set, the Socket Mode
/// URL may be on this machine too.
pub const API_URL_ENV: &str = "JUNEES_SLACK_API_URL";
/// Where files are downloaded from instead of `files.slack.com`. Only for
/// tests: on this machine, not a key of the settings file.
pub const FILES_URL_ENV: &str = "JUNEES_SLACK_FILES_URL";

/// The Web API ("Web API basics": `https://slack.com/api/METHOD`).
pub const DEFAULT_API_URL: &str = "https://slack.com/api";

/// The host Slack serves its own files from ("Downloading files": confirm
/// "that the host exactly matches a Slack file host your app expects,
/// such as `files.slack.com`").
pub const FILES_HOST: &str = "files.slack.com";

/// Which of the two tokens a [`Token`] is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Bot,
    App,
}

impl Kind {
    fn prefix(self) -> &'static str {
        match self {
            Kind::Bot => "xoxb-",
            Kind::App => "xapp-",
        }
    }

    fn setting(self) -> &'static str {
        match self {
            Kind::Bot => "slack_bot_token",
            Kind::App => "slack_app_token",
        }
    }

    fn what(self) -> &'static str {
        match self {
            Kind::Bot => "bot token",
            Kind::App => "app-level token",
        }
    }
}

/// A Slack token. It goes only into the `Authorization` header (marked
/// sensitive), never into a URL: `Debug` hides it and there is no
/// `Display`.
#[derive(Clone)]
pub struct Token(String);

impl Token {
    /// A token as Slack shows it (`xoxb-...` for the bot, `xapp-...` for
    /// the app). Anything else is refused without repeating it: a header
    /// value must not hold spaces or control characters, and a pasted
    /// token of the wrong kind is a common slip.
    pub fn new(raw: &str, kind: Kind) -> Result<Self> {
        let raw = raw.trim();
        if raw.is_empty() {
            bail!("the Slack {} is empty", kind.what());
        }
        let body = raw.strip_prefix(kind.prefix());
        let valid = body.is_some_and(|body| {
            body.len() >= 8
                && body
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
        });
        if !valid {
            let other = match kind {
                Kind::Bot if raw.starts_with(Kind::App.prefix()) => {
                    " (it is the app-level token: put it in slack_app_token)"
                }
                Kind::App if raw.starts_with(Kind::Bot.prefix()) => {
                    " (it is the bot token: put it in slack_bot_token)"
                }
                _ => "",
            };
            bail!(
                "the Slack {} ({}) does not look like one: it starts with `{}` followed by \
                 letters, digits and `-`{other}",
                kind.what(),
                kind.setting(),
                kind.prefix()
            );
        }
        Ok(Self(raw.to_string()))
    }

    /// The token itself, for the `Authorization` header.
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
pub struct SlackConfig {
    pub bot_token: Token,
    pub app_token: Token,
    /// User ids the bot listens to; never empty.
    pub allowed_users: HashSet<String>,
    pub parallel: usize,
    pub approval_timeout: Duration,
    /// Web API base URL without a trailing slash: [`DEFAULT_API_URL`], or
    /// a URL on this machine.
    pub api_url: String,
    /// Files are downloaded from under this URL instead of
    /// [`FILES_HOST`] (tests only).
    pub files_url: Option<Url>,
}

impl SlackConfig {
    /// Reads the settings through `lookup` (environment first, then the
    /// settings file; see [`crate::config_file::Settings`]).
    pub fn from_lookup(lookup: impl Fn(&str) -> Option<String>) -> Result<Self> {
        let get = |key: &str| {
            lookup(key)
                .map(|value| value.trim().to_string())
                .filter(|value| !value.is_empty())
        };
        let bot_token = get(BOT_TOKEN_ENV).context(
            "no Slack bot token: set slack_bot_token (xoxb-...) in the settings file or \
             JUNEES_SLACK_BOT_TOKEN (see README, \"슬랙 게이트웨이\")",
        )?;
        let bot_token = Token::new(&bot_token, Kind::Bot)?;
        let app_token = get(APP_TOKEN_ENV).context(
            "no Slack app-level token: set slack_app_token (xapp-..., with the \
             connections:write scope) in the settings file or JUNEES_SLACK_APP_TOKEN (see \
             README, \"슬랙 게이트웨이\")",
        )?;
        let app_token = Token::new(&app_token, Kind::App)?;
        let allowed_users = user_ids(get(ALLOWED_USERS_ENV), ALLOWED_USERS_ENV)?;
        if allowed_users.is_empty() {
            bail!(
                "no allowed Slack users: set slack_allowed_users (or {ALLOWED_USERS_ENV}) to \
                 the member ids the bot may answer (U...), separated by commas. Without the \
                 list anyone in the workspace could run commands on this machine."
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
        let api_url = match get(API_URL_ENV) {
            None => DEFAULT_API_URL.to_string(),
            Some(url) => local_url(&url, Some(DEFAULT_API_URL)).context(API_URL_ENV)?,
        };
        let files_url = match get(FILES_URL_ENV) {
            None => None,
            Some(url) => {
                let url = local_url(&url, None).context(FILES_URL_ENV)?;
                Some(Url::parse(&format!("{url}/")).context(FILES_URL_ENV)?)
            }
        };
        Ok(Self {
            bot_token,
            app_token,
            allowed_users,
            parallel,
            approval_timeout,
            api_url,
            files_url,
        })
    }

    /// Whether the Web API is the real one (not a test server).
    pub fn is_slack(&self) -> bool {
        self.api_url == DEFAULT_API_URL
    }
}

/// Splits a comma list of Slack member ids (`U...`, or `W...` in an
/// Enterprise organization), upper-case letters and digits.
pub fn user_ids(value: Option<String>, name: &str) -> Result<HashSet<String>> {
    let mut ids = HashSet::new();
    for item in value.iter().flat_map(|value| value.split(',')) {
        let item = item.trim();
        if item.is_empty() {
            continue;
        }
        let valid = (item.starts_with('U') || item.starts_with('W'))
            && (3..=24).contains(&item.len())
            && item
                .bytes()
                .all(|b| b.is_ascii_uppercase() || b.is_ascii_digit());
        if !valid {
            // A pasted token is not repeated.
            let shown = if item.starts_with("xox") || item.starts_with("xapp") {
                "(not shown)".to_string()
            } else {
                format!("`{item}`")
            };
            bail!(
                "{name} takes Slack member ids (such as U012AB3CD; the profile's \"Copy member \
                 ID\" gives yours), got {shown}"
            );
        }
        ids.insert(item.to_string());
    }
    Ok(ids)
}

/// `url` without a trailing slash, if it is `real` or on this machine
/// (http or https, a loopback address or `localhost`, no user, query or
/// fragment): tokens go to it.
pub fn local_url(url: &str, real: Option<&str>) -> Result<String> {
    let trimmed = url.trim().trim_end_matches('/');
    if real == Some(trimmed) {
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
        match real {
            Some(real) => bail!(
                "the Slack URL must be {real} or a URL on this machine (for tests), got \
                 `{trimmed}`"
            ),
            None => bail!("the URL must be on this machine (for tests), got `{trimmed}`"),
        }
    }
    Ok(trimmed.to_string())
}

#[cfg(test)]
pub(crate) mod tests {
    use std::collections::HashMap;

    use super::*;

    /// Test tokens, built in pieces so no token-looking literal sits in
    /// the source.
    pub(crate) fn bot_token() -> String {
        ["xoxb", "1-2-fake-slack-bot-secret"].join("-")
    }

    pub(crate) fn app_token() -> String {
        ["xapp", "1-A1-fake-slack-app-secret"].join("-")
    }

    fn read(pairs: &[(&str, &str)]) -> Result<SlackConfig> {
        let map: HashMap<String, String> = pairs
            .iter()
            .map(|(key, value)| (key.to_string(), value.to_string()))
            .collect();
        SlackConfig::from_lookup(|key| map.get(key).cloned())
    }

    #[test]
    fn reads_the_settings() {
        let (bot, app) = (bot_token(), app_token());
        let config = read(&[
            (BOT_TOKEN_ENV, &format!(" {bot} ")),
            (APP_TOKEN_ENV, &app),
            (ALLOWED_USERS_ENV, " U111AAA, W222 ,,"),
            (PARALLEL_ENV, "3"),
            (APPROVAL_TIMEOUT_ENV, "90s"),
            (API_URL_ENV, "http://127.0.0.1:9/api/"),
            (FILES_URL_ENV, "http://127.0.0.1:9/files"),
        ])
        .unwrap();
        assert_eq!(config.bot_token.expose(), bot);
        assert_eq!(config.app_token.expose(), app);
        assert_eq!(
            config.allowed_users,
            HashSet::from(["U111AAA".to_string(), "W222".to_string()])
        );
        assert_eq!(config.parallel, 3);
        assert_eq!(config.approval_timeout, Duration::from_secs(90));
        assert_eq!(config.api_url, "http://127.0.0.1:9/api");
        assert!(!config.is_slack());
        assert_eq!(
            config.files_url.unwrap().as_str(),
            "http://127.0.0.1:9/files/"
        );

        let config = read(&[
            (BOT_TOKEN_ENV, &bot),
            (APP_TOKEN_ENV, &app),
            (ALLOWED_USERS_ENV, "U1A"),
        ])
        .unwrap();
        assert_eq!(config.parallel, DEFAULT_PARALLEL);
        assert_eq!(config.approval_timeout, DEFAULT_APPROVAL_TIMEOUT);
        assert_eq!(config.api_url, DEFAULT_API_URL);
        assert!(config.is_slack() && config.files_url.is_none());
    }

    #[test]
    fn refuses_to_start_without_tokens_or_allowed_users() {
        let (bot, app) = (bot_token(), app_token());
        for users in [None, Some(""), Some(" , ")] {
            let mut pairs = vec![(BOT_TOKEN_ENV, bot.as_str()), (APP_TOKEN_ENV, app.as_str())];
            if let Some(users) = users {
                pairs.push((ALLOWED_USERS_ENV, users));
            }
            let err = read(&pairs).err().unwrap();
            assert!(
                format!("{err:#}").contains("no allowed Slack users"),
                "{err:#}"
            );
        }
        let err = read(&[(APP_TOKEN_ENV, &app), (ALLOWED_USERS_ENV, "U1A")])
            .err()
            .unwrap();
        assert!(format!("{err:#}").contains("no Slack bot token"));
        let err = read(&[(BOT_TOKEN_ENV, &bot), (ALLOWED_USERS_ENV, "U1A")])
            .err()
            .unwrap();
        assert!(format!("{err:#}").contains("no Slack app-level token"));
        for bad in ["@someone", "u123", "C0123ABC", "U12-3", &bot] {
            let err = read(&[
                (BOT_TOKEN_ENV, &bot),
                (APP_TOKEN_ENV, &app),
                (ALLOWED_USERS_ENV, bad),
            ])
            .err()
            .unwrap();
            let text = format!("{err:#}");
            assert!(text.contains("Slack member ids"), "{text}");
            assert!(!text.contains("fake-slack"), "{text}");
        }
    }

    #[test]
    fn tokens_are_checked_and_never_shown() {
        let (bot, app) = (bot_token(), app_token());
        // Swapped tokens are named as such.
        let err = read(&[
            (BOT_TOKEN_ENV, &app),
            (APP_TOKEN_ENV, &bot),
            (ALLOWED_USERS_ENV, "U1A"),
        ])
        .err()
        .unwrap();
        let text = format!("{err:#}");
        assert!(text.contains("put it in slack_app_token"), "{text}");
        assert!(!text.contains("fake-slack"), "{text}");
        for bad in [
            "xoxb-",
            "xoxb-short",
            "xoxp-1-2-fake-slack-user-secret",
            "xoxb-1-2-fake slack",
            "xoxb-1-2-fake\nslack-x",
            "xoxb-1-2-fake/../slack",
        ] {
            let err = Token::new(bad, Kind::Bot).err().unwrap();
            let text = format!("{err:#}");
            assert!(text.contains("does not look like one"), "{text}");
            assert!(!text.contains("fake"), "{text}");
        }
        let token = Token::new(&bot, Kind::Bot).unwrap();
        assert_eq!(format!("{token:?}"), "Token(hidden)");
        // bash, MCP servers and doctor hide them like the other secrets.
        for name in [BOT_TOKEN_ENV, APP_TOKEN_ENV] {
            assert!(crate::tools::SECRET_ENV_VARS.contains(&name), "{name}");
        }
        assert!(crate::secrets::secret_kind(&app).is_some());
        assert!(crate::secrets::secret_kind(&bot).is_some());
        for bad in [(PARALLEL_ENV, "0"), (APPROVAL_TIMEOUT_ENV, "soon")] {
            let err = read(&[
                (BOT_TOKEN_ENV, &bot),
                (APP_TOKEN_ENV, &app),
                (ALLOWED_USERS_ENV, "U1A"),
                bad,
            ])
            .err()
            .unwrap();
            assert!(!format!("{err:#}").contains("fake-slack"), "{err:#}");
        }
    }

    #[test]
    fn test_urls_stay_on_this_machine() {
        for url in [
            "https://slack.com/api",
            "https://slack.com/api/",
            "http://127.0.0.1:8081",
            "https://localhost/slack",
            "http://[::1]:9/x",
        ] {
            local_url(url, Some(DEFAULT_API_URL)).unwrap();
        }
        for url in [
            "http://slack.com/api",
            "https://slack.com.evil.example/api",
            "https://evil.example/slack.com/api",
            "http://127.0.0.1.evil.example/",
            "http://user@127.0.0.1/",
            "http://127.0.0.1/?x=1",
            "ftp://127.0.0.1/",
            "slack.com",
        ] {
            assert!(local_url(url, Some(DEFAULT_API_URL)).is_err(), "{url}");
        }
        assert!(local_url("https://files.slack.com", None).is_err());
    }
}
