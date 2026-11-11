//! GitHub through its REST API (v3): the model's `github` tool reads pull
//! requests, their diffs, comments and checks, and writes comments,
//! reviews, pull requests and merges with the user's token
//! (`JUNEES_GITHUB_TOKEN` or `github_token`).
//!
//! Writes go to the approval question in the conversation and the gateway
//! (a merge every time, "always" does not cover it). Where no one is asked
//! (`-p`, scheduled runs, `JUNEES_APPROVAL=off`) only the writes
//! `JUNEES_GITHUB_ALLOW_UNATTENDED` names run, and only when
//! `JUNEES_GITHUB_REPOS` is set. Every action is limited to
//! the repositories of `JUNEES_GITHUB_REPOS`, or to the working folder's
//! `origin` when that is empty. What GitHub sends back (titles, bodies,
//! comments, diffs) is written by other people, so the tool marks it as
//! data and escapes hidden characters.

mod client;
mod format;
mod tool;

#[cfg(test)]
mod tests;

use std::fmt;
use std::path::Path;
use std::process::Command;

use anyhow::{Context, Result, bail};
use reqwest::Url;
use serde_json::Value;

pub use client::Client;
pub use tool::{GithubTool, Question, question};

use crate::tools::ToolRegistry;

/// The tool's name.
pub const TOOL_NAME: &str = "github";
pub const TOKEN_ENV: &str = "JUNEES_GITHUB_TOKEN";
pub const API_URL_ENV: &str = "JUNEES_GITHUB_API_URL";
pub const REPOS_ENV: &str = "JUNEES_GITHUB_REPOS";
pub const UNATTENDED_ENV: &str = "JUNEES_GITHUB_ALLOW_UNATTENDED";
pub const DEFAULT_API_URL: &str = "https://api.github.com";

/// A GitHub token. Its value never shows: no `Display`, and `Debug`
/// prints a placeholder.
#[derive(Clone, PartialEq, Eq)]
pub struct Token(String);

impl Token {
    /// Refuses an empty value or one with whitespace or control
    /// characters (it goes into a header). The error never repeats it.
    pub fn new(value: &str) -> Result<Self> {
        let value = value.trim();
        if value.is_empty() || value.chars().any(|c| c.is_whitespace() || c.is_control()) {
            bail!("{TOKEN_ENV} must be one word without spaces or control characters");
        }
        Ok(Self(value.to_string()))
    }

    pub(crate) fn value(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for Token {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Token(hidden)")
    }
}

/// A write the tool can make, as `JUNEES_GITHUB_ALLOW_UNATTENDED` names it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Write {
    /// `pr_comment` and `review_reply`.
    Comment,
    /// `review` with COMMENT or REQUEST_CHANGES.
    Review,
    /// `review` with APPROVE.
    Approve,
    PrCreate,
    Merge,
}

impl Write {
    pub const ALL: [Write; 5] = [
        Write::Comment,
        Write::Review,
        Write::Approve,
        Write::PrCreate,
        Write::Merge,
    ];

    pub fn name(self) -> &'static str {
        match self {
            Write::Comment => "comment",
            Write::Review => "review",
            Write::Approve => "approve",
            Write::PrCreate => "pr_create",
            Write::Merge => "merge",
        }
    }

    fn parse(name: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|write| write.name() == name)
    }
}

/// A repository, `owner/name`.
#[derive(Debug, Clone)]
pub struct Repo {
    pub owner: String,
    pub name: String,
}

impl Repo {
    /// Reads `owner/name`. Only the characters GitHub allows in them, so
    /// the pair can go into a request path as it is.
    pub fn parse(text: &str) -> Option<Self> {
        let (owner, name) = text.trim().split_once('/')?;
        let name = name.strip_suffix(".git").unwrap_or(name);
        let owner_ok = (1..=39).contains(&owner.len())
            && owner.chars().all(|c| c.is_ascii_alphanumeric() || c == '-');
        let name_ok = (1..=100).contains(&name.len())
            && name
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
            && name != "."
            && name != "..";
        (owner_ok && name_ok).then(|| Self {
            owner: owner.to_string(),
            name: name.to_string(),
        })
    }
}

/// GitHub compares owner and repository names without case.
impl PartialEq for Repo {
    fn eq(&self, other: &Self) -> bool {
        self.owner.eq_ignore_ascii_case(&other.owner) && self.name.eq_ignore_ascii_case(&other.name)
    }
}

impl Eq for Repo {}

impl fmt::Display for Repo {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}/{}", self.owner, self.name)
    }
}

/// The GitHub settings. `Debug` shows the token as a placeholder.
#[derive(Debug, Clone)]
pub struct Settings {
    /// `None`: no `github` tool.
    pub token: Option<Token>,
    /// The REST API root (`JUNEES_GITHUB_API_URL`): https, or http on
    /// this machine (tests).
    pub api_url: Url,
    /// The repositories the tool may use (`JUNEES_GITHUB_REPOS`); empty
    /// means the working folder's `origin` only.
    pub repos: Vec<Repo>,
    /// Writes allowed where no one is asked
    /// (`JUNEES_GITHUB_ALLOW_UNATTENDED`); none by default.
    pub unattended: Vec<Write>,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            token: None,
            api_url: Url::parse(DEFAULT_API_URL).expect("valid URL"),
            repos: Vec::new(),
            unattended: Vec::new(),
        }
    }
}

impl Settings {
    /// Reads `JUNEES_GITHUB_TOKEN`, `JUNEES_GITHUB_API_URL`,
    /// `JUNEES_GITHUB_REPOS` and `JUNEES_GITHUB_ALLOW_UNATTENDED` (both
    /// comma-separated). `get` gives trimmed, non-empty values.
    pub fn read(get: impl Fn(&str) -> Option<String>) -> Result<Self> {
        let token = get(TOKEN_ENV).map(|raw| Token::new(&raw)).transpose()?;
        let api_url = match get(API_URL_ENV) {
            None => Url::parse(DEFAULT_API_URL).expect("valid URL"),
            Some(raw) => api_url(&raw)?,
        };
        let mut repos = Vec::new();
        for item in list(get(REPOS_ENV)) {
            let repo = Repo::parse(&item)
                .with_context(|| format!("{REPOS_ENV}: `{item}` is not owner/name"))?;
            repos.push(repo);
        }
        let mut unattended = Vec::new();
        for item in list(get(UNATTENDED_ENV)) {
            let write = Write::parse(&item.to_ascii_lowercase()).with_context(|| {
                let names: Vec<_> = Write::ALL.iter().map(|write| write.name()).collect();
                format!(
                    "{UNATTENDED_ENV}: unknown write `{item}` (use {})",
                    names.join(", ")
                )
            })?;
            unattended.push(write);
        }
        Ok(Self {
            token,
            api_url,
            repos,
            unattended,
        })
    }

    /// What `/github` shows about the settings, without the token.
    pub fn describe(&self, cwd: &Path) -> String {
        let mut text = match &self.token {
            Some(_) => format!(
                "GitHub token: set ({TOKEN_ENV} or github_token); API {}\n",
                self.api_url
            ),
            None => format!(
                "GitHub token: not set; set {TOKEN_ENV} (or github_token in the settings file) \
                 to give the model the `github` tool\n"
            ),
        };
        let allowed = self.allowed_repos(cwd);
        text.push_str(&match (self.repos.is_empty(), allowed.first()) {
            (false, _) => format!("repositories ({REPOS_ENV}): {}\n", join(&allowed)),
            (true, Some(origin)) => format!(
                "repositories: {origin} (the working folder's origin; set {REPOS_ENV} for \
                 others)\n"
            ),
            (true, None) => format!(
                "repositories: none (no GitHub origin in the working folder and {REPOS_ENV} \
                 is empty)\n"
            ),
        });
        let writes: Vec<_> = self.unattended.iter().map(|write| write.name()).collect();
        text.push_str(&format!(
            "writes without a question (-p, cron, webhooks): {}\n",
            if writes.is_empty() {
                "none".to_string()
            } else if self.repos.is_empty() {
                format!(
                    "none ({} allowed, but they need {REPOS_ENV}: the working folder's origin \
                     is not enough)",
                    writes.join(", ")
                )
            } else {
                writes.join(", ")
            }
        ));
        text
    }

    /// The repositories the tool may use from `cwd`.
    pub fn allowed_repos(&self, cwd: &Path) -> Vec<Repo> {
        if self.repos.is_empty() {
            self.origin(cwd).into_iter().collect()
        } else {
            self.repos.clone()
        }
    }

    /// The working folder's `origin` repository on this GitHub. For the
    /// public API the remote must be on github.com; for another API root
    /// (GitHub Enterprise, a test server) any host counts.
    pub fn origin(&self, cwd: &Path) -> Option<Repo> {
        let url = origin_url(cwd)?;
        let (host, repo) = parse_remote(&url)?;
        let public = self.api_url.host_str() == Some("api.github.com");
        (!public || host.eq_ignore_ascii_case("github.com")).then_some(repo)
    }
}

/// Registers the `github` tool when a token is set. `asks` says whether an
/// approval gate asks about its writes; without one only the writes the
/// settings allow unattended run, and only with JUNEES_GITHUB_REPOS set.
/// The repositories are decided now, so a session that changes `origin`
/// cannot widen them; a later run reads the origin again, which is why
/// writes without a question need the list.
pub fn register(tools: &mut ToolRegistry, settings: &Settings, cwd: &Path, asks: bool) {
    let Some(token) = &settings.token else {
        return;
    };
    match Client::new(&settings.api_url, token.clone()) {
        Ok(client) => tools.register(
            GithubTool::new(
                client,
                settings.origin(cwd),
                settings.allowed_repos(cwd),
                asks,
                settings.unattended.clone(),
            )
            .origin_only(settings.repos.is_empty()),
        ),
        Err(err) => eprintln!("  !! github tool not available: {err:#}"),
    }
}

/// `/github`: the settings and, with a token, the rate limit left.
pub async fn overview(settings: &Settings, cwd: &Path) -> String {
    let mut text = settings.describe(cwd);
    let Some(token) = &settings.token else {
        return text;
    };
    let line = match Client::new(&settings.api_url, token.clone()) {
        Ok(client) => match client.get("rate_limit").await {
            Ok(body) => rate_line(&body),
            Err(err) => format!("rate limit: unknown ({err})"),
        },
        Err(err) => format!("rate limit: unknown ({err:#})"),
    };
    text.push_str(&line);
    text.push('\n');
    text
}

/// `remaining/limit` of the core API from a `GET /rate_limit` body.
fn rate_line(body: &Value) -> String {
    let core = &body["resources"]["core"];
    let (Some(remaining), Some(limit)) = (core["remaining"].as_u64(), core["limit"].as_u64())
    else {
        return "rate limit: unknown (unexpected reply)".into();
    };
    let reset = core["reset"]
        .as_u64()
        .map(|reset| reset.saturating_sub(client::now_secs()))
        .map_or_else(String::new, |secs| {
            format!(", resets in {} min", secs.div_ceil(60))
        });
    format!("rate limit: {remaining}/{limit} requests left{reset}")
}

fn join(repos: &[Repo]) -> String {
    repos
        .iter()
        .map(Repo::to_string)
        .collect::<Vec<_>>()
        .join(", ")
}

fn list(value: Option<String>) -> Vec<String> {
    value
        .iter()
        .flat_map(|value| value.split(','))
        .map(str::trim)
        .filter(|item| !item.is_empty())
        .map(str::to_string)
        .collect()
}

/// An API root: https, or plain http only on this machine, since the
/// token would cross the network readable.
fn api_url(raw: &str) -> Result<Url> {
    let url = Url::parse(raw.trim())
        .ok()
        .filter(|url| matches!(url.scheme(), "http" | "https") && url.host_str().is_some())
        .with_context(|| format!("{API_URL_ENV} is not an http(s) URL"))?;
    let host = url.host_str().unwrap_or_default();
    if url.scheme() == "http" && !is_loopback(host) {
        bail!("{API_URL_ENV} must be an https URL (plain http only for localhost)");
    }
    if url.query().is_some() || url.fragment().is_some() || !url.username().is_empty() {
        bail!("{API_URL_ENV} must be a plain API root, without a query or user name");
    }
    Ok(url)
}

fn is_loopback(host: &str) -> bool {
    let ip = host.trim_start_matches('[').trim_end_matches(']');
    host.eq_ignore_ascii_case("localhost")
        || ip
            .parse::<std::net::IpAddr>()
            .is_ok_and(|ip| ip.is_loopback())
}

/// `git remote get-url origin` in `cwd`, run like task mode's git: none of
/// the variables that point git elsewhere, no credentials, no fsmonitor.
fn origin_url(cwd: &Path) -> Option<String> {
    let mut command = Command::new("git");
    command
        .args(["-c", "core.fsmonitor=false", "remote", "get-url", "origin"])
        .current_dir(cwd)
        .stdin(std::process::Stdio::null());
    for name in crate::task::git::REPO_ENV_VARS
        .iter()
        .chain(crate::tools::SECRET_ENV_VARS)
        .chain(crate::tools::CLAUDE_CODE_SECRET_ENV_VARS)
    {
        command.env_remove(name);
    }
    let output = command.output().ok()?;
    output
        .status
        .success()
        .then(|| String::from_utf8_lossy(&output.stdout).trim().to_string())
}

/// The host and repository of a remote URL: `https://host/o/n(.git)`,
/// `ssh://git@host[:port]/o/n.git`, `git@host:o/n.git`.
pub fn parse_remote(url: &str) -> Option<(String, Repo)> {
    let url = url.trim();
    let (host, path) = if url.contains("://") {
        let parsed = Url::parse(url).ok()?;
        (
            parsed.host_str()?.to_string(),
            parsed.path().trim_matches('/').to_string(),
        )
    } else {
        let (user_host, path) = url.split_once(':')?;
        let host = user_host.rsplit('@').next()?;
        (host.to_string(), path.trim_matches('/').to_string())
    };
    if host.is_empty() || path.matches('/').count() != 1 {
        return None;
    }
    Some((host, Repo::parse(&path)?))
}
