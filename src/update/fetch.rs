//! Asking GitHub for the latest release and downloading its files.
//!
//! Everything goes to `https://github.com/seongj-un/junees/releases`
//! ([`DOWNLOAD_BASE`]): `latest` answers with a redirect to the newest
//! release's tag page (the redirect is read, not followed), and
//! `download/<tag>/<file>` redirects to GitHub's file hosts. Redirects are
//! followed only to [`RELEASE_HOSTS`] over https. Tests point
//! [`BASE_ENV`] at a server on this computer; only loopback addresses are
//! taken there, and redirects then stay on that same server.

use std::time::Duration;

use anyhow::{Context, Result, bail};
use reqwest::Url;
use reqwest::redirect::Policy;

use super::version::Version;

/// Where releases are published.
pub const DOWNLOAD_BASE: &str = "https://github.com/seongj-un/junees/releases";

/// Overrides [`DOWNLOAD_BASE`] for tests; loopback addresses only.
pub const BASE_ENV: &str = "JUNEES_DOWNLOAD_BASE";

/// The hosts release downloads may be redirected to.
pub const RELEASE_HOSTS: &[&str] = &[
    "github.com",
    "objects.githubusercontent.com",
    "release-assets.githubusercontent.com",
    "github-releases.githubusercontent.com",
];

const MAX_REDIRECTS: usize = 5;
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
/// For the question about the latest release.
pub const CHECK_TIMEOUT: Duration = Duration::from_secs(20);
/// For one download.
const DOWNLOAD_TIMEOUT: Duration = Duration::from_secs(600);

/// Where releases come from.
#[derive(Debug, Clone)]
pub struct Source {
    /// `.../releases`, without a trailing slash.
    base: Url,
    /// Whether this is the loopback server of [`BASE_ENV`].
    local: bool,
}

impl Source {
    /// GitHub.
    pub fn github() -> Self {
        Self {
            base: Url::parse(DOWNLOAD_BASE).expect("DOWNLOAD_BASE is a URL"),
            local: false,
        }
    }

    /// GitHub, or the server [`BASE_ENV`] names.
    pub fn from_lookup(lookup: &dyn Fn(&str) -> Option<String>) -> Result<Self> {
        match lookup(BASE_ENV).filter(|value| !value.trim().is_empty()) {
            Some(base) => Self::loopback(base.trim()),
            None => Ok(Self::github()),
        }
    }

    /// A server on this computer at `base` (`http://127.0.0.1:8080/releases`).
    pub fn loopback(base: &str) -> Result<Self> {
        let url = Url::parse(base).with_context(|| format!("{BASE_ENV} is not a URL"))?;
        if !matches!(url.scheme(), "http" | "https")
            || !crate::mcp::http::target::is_loopback(&url)
            || !url.username().is_empty()
            || url.password().is_some()
            || url.query().is_some()
            || url.fragment().is_some()
        {
            bail!(
                "{BASE_ENV} must be an http(s) address on this computer (127.0.0.1, ::1 or \
                 localhost) without a query; it is only for tests"
            );
        }
        let mut url = url;
        let path = url.path().trim_end_matches('/').to_string();
        url.set_path(&path);
        Ok(Self {
            base: url,
            local: true,
        })
    }

    /// `base/<path>`.
    fn url(&self, path: &str) -> Result<Url> {
        Url::parse(&format!(
            "{}/{path}",
            self.base.as_str().trim_end_matches('/')
        ))
        .with_context(|| format!("cannot build the address of {path}"))
    }

    /// Whether a download may go to (or be redirected to) `url`.
    pub fn allows(&self, url: &Url) -> bool {
        if !url.username().is_empty() || url.password().is_some() {
            return false;
        }
        if self.local {
            url.scheme() == self.base.scheme()
                && url.host_str() == self.base.host_str()
                && url.port_or_known_default() == self.base.port_or_known_default()
        } else {
            url.scheme() == "https"
                && url.port().is_none()
                && url
                    .host_str()
                    .is_some_and(|host| RELEASE_HOSTS.contains(&host))
        }
    }

    /// What the messages call this source.
    pub fn describe(&self) -> String {
        if self.local {
            format!("the test server {}", self.base)
        } else {
            "GitHub".to_string()
        }
    }
}

/// The HTTP side of an update.
pub struct Client {
    source: Source,
    /// Follows no redirect: for `latest`.
    plain: reqwest::Client,
    /// Follows redirects to the allowed hosts: for downloads.
    follow: reqwest::Client,
}

impl Client {
    pub fn new(source: Source) -> Result<Self> {
        let agent = concat!("junees/", env!("CARGO_PKG_VERSION"));
        let plain = reqwest::Client::builder()
            .user_agent(agent)
            .redirect(Policy::none())
            .connect_timeout(CONNECT_TIMEOUT)
            .timeout(CHECK_TIMEOUT)
            .build()
            .context("cannot set up the HTTP client")?;
        let allowed = source.clone();
        let policy = Policy::custom(move |attempt| {
            if attempt.previous().len() > MAX_REDIRECTS {
                attempt.error("too many redirects")
            } else if allowed.allows(attempt.url()) {
                attempt.follow()
            } else {
                let host = attempt.url().host_str().unwrap_or("no host").to_string();
                attempt.error(format!(
                    "redirected to {host}, which is not one of GitHub's release hosts"
                ))
            }
        });
        let follow = reqwest::Client::builder()
            .user_agent(agent)
            .redirect(policy)
            .connect_timeout(CONNECT_TIMEOUT)
            .timeout(DOWNLOAD_TIMEOUT)
            .build()
            .context("cannot set up the HTTP client")?;
        Ok(Self {
            source,
            plain,
            follow,
        })
    }

    pub fn source(&self) -> &Source {
        &self.source
    }

    /// The version of the latest release, or `None` when no release is
    /// published (GitHub then redirects `latest` to the release list).
    pub async fn latest(&self) -> Result<Option<Version>> {
        let url = self.source.url("latest")?;
        let response = self.plain.get(url.clone()).send().await.map_err(|err| {
            anyhow::anyhow!(
                "cannot ask {} for the latest release: {}",
                self.source.describe(),
                crate::notify::describe(err, CHECK_TIMEOUT)
            )
        })?;
        let status = response.status();
        if status.as_u16() == 404 {
            return Ok(None);
        }
        if !status.is_redirection() {
            bail!(
                "{} answered HTTP {status} when asked for the latest release",
                self.source.describe()
            );
        }
        let location = response
            .headers()
            .get(reqwest::header::LOCATION)
            .and_then(|value| value.to_str().ok())
            .context("the answer about the latest release names no release")?;
        let target = url
            .join(location)
            .context("the answer about the latest release names no release")?;
        let segments: Vec<&str> = target.path_segments().map_or(Vec::new(), Iterator::collect);
        match segments.iter().position(|segment| *segment == "tag") {
            Some(at) if at + 2 == segments.len() => Version::parse(segments[at + 1])
                .map(Some)
                .map_err(anyhow::Error::msg)
                .context("the latest release has a tag that is not a version"),
            // `.../releases`: nothing is published yet.
            _ => Ok(None),
        }
    }

    /// The release file `name` of `version`, at most `limit` bytes.
    pub async fn download(&self, version: &Version, name: &str, limit: u64) -> Result<Vec<u8>> {
        let url = self
            .source
            .url(&format!("download/{}/{name}", version.tag()))?;
        let failed = |err: reqwest::Error| {
            anyhow::anyhow!(
                "cannot download {name}: {}",
                crate::notify::describe(err, DOWNLOAD_TIMEOUT)
            )
        };
        let mut response = self.follow.get(url).send().await.map_err(failed)?;
        let status = response.status();
        if status.as_u16() == 404 {
            bail!(
                "release {} has no file {name} (HTTP 404); is the version right?",
                version.tag()
            );
        }
        if !status.is_success() {
            bail!("cannot download {name}: HTTP {status}");
        }
        if let Some(length) = response.content_length()
            && length > limit
        {
            bail!("{name} is {length} bytes, more than the {limit} an update takes");
        }
        let mut bytes = Vec::new();
        while let Some(chunk) = response.chunk().await.map_err(failed)? {
            bytes.extend_from_slice(&chunk);
            if bytes.len() as u64 > limit {
                bail!("{name} is more than the {limit} bytes an update takes");
            }
        }
        Ok(bytes)
    }
}
