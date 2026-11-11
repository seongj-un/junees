//! `junees doctor` and `/doctor`: a look over the installation and the
//! settings. Every check ends ok, warning or failure (or skipped, when it
//! does not apply), with one line on how to fix it.
//!
//! It only reads: no file is written, no lock is kept (a running daemon
//! or server is seen through its lock, taken for a moment if free), no MCP server
//! or plugin command is started, and the only program run is
//! `claude --version`. The network is used only with `--online`: one
//! request that shows whether the API takes the key, and
//! `claude auth status`.
//!
//! Secrets never show: checks say whether a key is set and where, never
//! its value, and every value of a secret setting is cut out of the
//! report before it is printed, in case an error quotes one.

use std::path::{Path, PathBuf};

use anyhow::{Result, bail};
use serde::Serialize;
use serde_json::{Value, json};

use crate::config_file::Settings;
use crate::prompt_text::escape_hidden;

mod claude;
mod files;
mod online;
mod services;
mod settings;
#[cfg(test)]
mod tests;

pub const USAGE: &str = "usage: junees doctor [--json] [--online]";

pub const HELP: &str = "\
junees doctor [--json] [--online]

Checks the installation and the settings and prints ok, warn or FAIL for
each check, with one line on how to fix it:
  state folder  $JUNEES_HOME is yours and 0700; config.toml, mcp.json,
                cron/jobs.json, webhooks/routes.json and secrets/* are 0600
  settings      config.toml reads (every key known) and every value reads
  keys          which keys and tokens are set (never their values)
  claude-code   `claude --version`, and bubblewrap and user namespaces for
                Claude Code's environment scrub
  programs      git and gh on PATH
  and the Discord settings, the cron daemon, the webhook routes, their
  secrets and server, the size of the saved sessions, the MCP servers'
  commands (not started) and the enabled plugins' manifests.

Nothing is written or started, and the network is not used, unless:
  --online   also ask the API whether it takes the key (GET /v1/models,
             no tokens used) and run `claude auth status`
  --json     print the report as JSON

Exits with 1 when a check failed.
";

/// What `junees doctor` was asked for.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Options {
    /// Print the report as JSON.
    pub json: bool,
    /// Also ask the API whether it takes the key, and `claude auth status`.
    pub online: bool,
}

/// The arguments after `junees doctor`; `None` for `--help`.
pub fn parse(args: impl IntoIterator<Item = String>) -> Result<Option<Options>> {
    let mut options = Options::default();
    for arg in args {
        match arg.as_str() {
            "-h" | "--help" => return Ok(None),
            "--json" => options.json = true,
            "--online" => options.online = true,
            other => bail!("unknown argument `{other}` for doctor ({USAGE})"),
        }
    }
    Ok(Some(options))
}

/// How a check ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Status {
    Ok,
    Warn,
    Fail,
    /// The check does not apply (nothing set up, or offline).
    Skip,
}

impl Status {
    fn label(self) -> &'static str {
        match self {
            Status::Ok => "ok",
            Status::Warn => "warn",
            Status::Fail => "FAIL",
            Status::Skip => "--",
        }
    }
}

/// One check.
#[derive(Debug, Clone, Serialize)]
pub struct Check {
    /// What part of Junees it belongs to (`state folder`, `claude-code`).
    pub area: &'static str,
    pub name: String,
    pub status: Status,
    pub detail: String,
    /// How to fix it, for a warning or a failure.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub fix: Option<String>,
}

/// Every check, in the order they ran.
#[derive(Debug, Default)]
pub struct Report {
    /// The state folder looked at.
    pub home: Option<PathBuf>,
    pub online: bool,
    pub checks: Vec<Check>,
}

impl Report {
    fn push(
        &mut self,
        area: &'static str,
        name: &str,
        status: Status,
        detail: String,
        fix: Option<String>,
    ) {
        self.checks.push(Check {
            area,
            name: name.to_string(),
            status,
            detail,
            fix,
        });
    }

    fn ok(&mut self, area: &'static str, name: &str, detail: impl Into<String>) {
        self.push(area, name, Status::Ok, detail.into(), None);
    }

    fn skip(&mut self, area: &'static str, name: &str, detail: impl Into<String>) {
        self.push(area, name, Status::Skip, detail.into(), None);
    }

    fn warn(
        &mut self,
        area: &'static str,
        name: &str,
        detail: impl Into<String>,
        fix: impl Into<String>,
    ) {
        self.push(area, name, Status::Warn, detail.into(), Some(fix.into()));
    }

    fn fail(
        &mut self,
        area: &'static str,
        name: &str,
        detail: impl Into<String>,
        fix: impl Into<String>,
    ) {
        self.push(area, name, Status::Fail, detail.into(), Some(fix.into()));
    }

    /// A failure when the part is `needed`, else a warning.
    fn problem(
        &mut self,
        needed: bool,
        area: &'static str,
        name: &str,
        detail: impl Into<String>,
        fix: impl Into<String>,
    ) {
        let status = if needed { Status::Fail } else { Status::Warn };
        self.push(area, name, status, detail.into(), Some(fix.into()));
    }

    /// How many checks ended with `status`.
    pub fn count(&self, status: Status) -> usize {
        self.checks
            .iter()
            .filter(|check| check.status == status)
            .count()
    }

    /// Whether a check failed (the exit code is then 1).
    pub fn failed(&self) -> bool {
        self.count(Status::Fail) > 0
    }

    /// Cuts every one of `secrets` out of the report.
    fn redact(&mut self, secrets: &[String]) {
        let cut = |text: &mut String| {
            for secret in secrets {
                if text.contains(secret.as_str()) {
                    *text = text.replace(secret.as_str(), "[hidden]");
                }
            }
        };
        for check in &mut self.checks {
            cut(&mut check.name);
            cut(&mut check.detail);
            if let Some(fix) = &mut check.fix {
                cut(fix);
            }
        }
    }

    /// The report for a terminal: the checks by area, then the counts.
    pub fn text(&self) -> String {
        let mut out = match &self.home {
            Some(home) => format!(
                "junees doctor: {}",
                escape_hidden(&home.display().to_string())
            ),
            None => "junees doctor: no state folder".to_string(),
        };
        out.push_str(if self.online {
            " (online)\n"
        } else {
            " (offline; --online also asks the API and `claude auth status`)\n"
        });
        let mut area = "";
        for check in &self.checks {
            if check.area != area {
                area = check.area;
                out.push_str(&format!("\n{area}\n"));
            }
            out.push_str(&format!(
                "  {:<4}  {}: {}\n",
                check.status.label(),
                escape_hidden(&check.name),
                escape_hidden(&check.detail)
            ));
            if let Some(fix) = &check.fix {
                out.push_str(&format!("        fix: {}\n", escape_hidden(fix)));
            }
        }
        out.push_str(&format!(
            "\n{} failed, {} warnings, {} ok, {} skipped\n",
            self.count(Status::Fail),
            self.count(Status::Warn),
            self.count(Status::Ok),
            self.count(Status::Skip)
        ));
        out
    }

    /// The report as JSON (`--json`).
    pub fn json(&self) -> Value {
        json!({
            "home": self.home.as_ref().map(|home| home.display().to_string()),
            "online": self.online,
            "ok": !self.failed(),
            "counts": {
                "ok": self.count(Status::Ok),
                "warn": self.count(Status::Warn),
                "fail": self.count(Status::Fail),
                "skip": self.count(Status::Skip),
            },
            "checks": self.checks,
        })
    }
}

/// Where the checks look besides the settings; tests point it at fakes.
pub struct Probe {
    /// `/proc/sys`, for the user namespace switches.
    pub sysctl: PathBuf,
}

impl Default for Probe {
    fn default() -> Self {
        Self {
            sysctl: PathBuf::from("/proc/sys"),
        }
    }
}

/// Runs every check on `settings`. `broken` is why the settings file could
/// not be read, if it could not (the checks then go on without it).
pub async fn run(settings: &Settings, broken: Option<&anyhow::Error>, online: bool) -> Report {
    run_with(settings, broken, online, &Probe::default()).await
}

/// [`run`] looking where `probe` says.
pub async fn run_with(
    settings: &Settings,
    broken: Option<&anyhow::Error>,
    online: bool,
    probe: &Probe,
) -> Report {
    let home = crate::config_file::home_dir(|key| settings.env(key));
    let mut report = Report {
        home: home.clone(),
        online,
        checks: Vec::new(),
    };
    let path = settings.env("PATH");
    let path = path.as_deref().map(std::ffi::OsStr::new);
    files::state_folder(&mut report, home.as_deref());
    let read = settings::settings(&mut report, settings, broken);
    settings::keys(&mut report, settings, read.backend);
    claude::check(&mut report, settings, &read, path, probe, online).await;
    services::programs(&mut report, path);
    settings::discord(&mut report, settings);
    if let Some(home) = home.as_deref() {
        services::cron(&mut report, home);
        services::webhooks(&mut report, home, settings);
        files::sessions(&mut report, home);
        services::mcp(&mut report, home, path);
        services::plugins(&mut report, home);
    }
    if online {
        online::api_key(&mut report, settings).await;
    }
    report.redact(&secret_values(settings));
    report
}

/// The settings whose values are secrets.
const SECRET_SETTINGS: &[&str] = &[
    "ANTHROPIC_API_KEY",
    crate::gateway::config::TOKEN_ENV,
    crate::github::TOKEN_ENV,
    crate::notify::WEBHOOK_ENV,
];

/// The values of [`SECRET_SETTINGS`] that are long enough to be told
/// apart from ordinary words.
fn secret_values(settings: &Settings) -> Vec<String> {
    SECRET_SETTINGS
        .iter()
        .filter_map(|name| settings.get(name))
        .map(|value| value.trim().to_string())
        .filter(|value| value.chars().count() >= 8)
        .collect()
}

/// This process's user id.
fn uid() -> u32 {
    // SAFETY: getuid only returns this process's user id.
    unsafe { libc::getuid() }
}

/// `path` for a message.
fn shown(path: &Path) -> String {
    path.display().to_string()
}
