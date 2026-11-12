//! The `browser` tool: a headless Chromium the model drives over the
//! Chrome DevTools Protocol (https://chromedevtools.github.io/devtools-protocol/),
//! with one page: open a URL, read a snapshot of the page (its text with
//! numbered links, buttons and fields), click, type, press keys, scroll,
//! go back, take a screenshot, run JavaScript (after the user approves)
//! and close.
//!
//! Off unless the user turns it on (`browser = on`, `JUNEES_BROWSER`).
//! Each session gets a new private profile in `$JUNEES_HOME/browser`
//! ([`PROFILES_FOLDER`], which the bash sandbox keeps commands out of),
//! so no login or cookie of the user's own browser is there; the browser and
//! the profile go away on `close`, after a quiet [`IDLE`], when a turn is
//! cancelled and when Junees ends (Chrome also ends when its DevTools pipe
//! closes, see [`process`]).
//!
//! Safety:
//! - only http and https; private, loopback and link-local addresses only
//!   with `browser_allow_private` ([`address`]), checked for every request
//!   of every page, frame and redirect, not only the URLs the model opens;
//! - downloads are refused and file pickers cancelled;
//! - `evaluate`, typing into password and payment fields and actions that
//!   submit such a form or look like a payment go to the approval
//!   question; where no one is asked (`-p`, `JUNEES_APPROVAL=off`, cron,
//!   webhooks) they are refused;
//! - between the check and the mouse or key events, the page cannot move
//!   another such element under the pointer or the focus: the click
//!   checks again after the mouse moved, and an input guard in every frame
//!   (`guard.js`, before the page's scripts) stops events whose target is
//!   not the one checked and needs approval;
//! - scheduled and webhook runs get the tool only with
//!   `browser_unattended`;
//! - page text, titles and script results are marked as data from the web,
//!   not instructions, and capped in size, as are screenshots.

pub mod address;
mod cdp;
mod page;
pub mod process;
mod tool;

#[cfg(test)]
mod tests;

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;

pub use tool::BrowserTool;

use crate::approval::ToolQuestions;
use crate::config::parse_flag;
use crate::tools::ToolRegistry;

/// The tool's name.
pub const TOOL_NAME: &str = "browser";
/// The folder under `$JUNEES_HOME` holding the running browsers'
/// profiles (cookies, logins of the session). The bash sandbox keeps
/// commands from reading or changing it, like the settings file
/// ([`crate::tools::SettingsFile::protected_paths`]).
pub const PROFILES_FOLDER: &str = "browser";
pub const ENABLE_ENV: &str = "JUNEES_BROWSER";
pub const PATH_ENV: &str = "JUNEES_BROWSER_PATH";
pub const ALLOW_PRIVATE_ENV: &str = "JUNEES_BROWSER_ALLOW_PRIVATE";
pub const UNATTENDED_ENV: &str = "JUNEES_BROWSER_UNATTENDED";
pub const NO_SANDBOX_ENV: &str = "JUNEES_BROWSER_NO_SANDBOX";

/// A browser nobody used for this long is closed.
pub const IDLE: Duration = Duration::from_secs(15 * 60);

/// The browser settings.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Settings {
    /// Gives the model the tool (`JUNEES_BROWSER`, off by default).
    pub enabled: bool,
    /// The browser program (`JUNEES_BROWSER_PATH`): a path, or a name on
    /// PATH; `None` looks for Chromium or Chrome on PATH.
    pub path: Option<PathBuf>,
    /// Lets pages load private, loopback and link-local addresses
    /// (`JUNEES_BROWSER_ALLOW_PRIVATE`).
    pub allow_private: bool,
    /// Gives scheduled and webhook runs the tool too
    /// (`JUNEES_BROWSER_UNATTENDED`).
    pub unattended: bool,
    /// Starts Chrome with `--no-sandbox` (`JUNEES_BROWSER_NO_SANDBOX`), for
    /// machines where its sandbox cannot start.
    pub no_sandbox: bool,
    /// Where the profiles go: [`PROFILES_FOLDER`] in the state folder,
    /// absolute; `None` (no state folder) puts them in the temporary
    /// folder.
    pub profiles: Option<PathBuf>,
}

impl Settings {
    /// Reads the `JUNEES_BROWSER*` settings; `get` gives trimmed,
    /// non-empty values.
    pub fn read(get: impl Fn(&str) -> Option<String>) -> Result<Self> {
        Ok(Self {
            enabled: parse_flag(get(ENABLE_ENV), ENABLE_ENV)?,
            path: get(PATH_ENV).map(PathBuf::from),
            allow_private: parse_flag(get(ALLOW_PRIVATE_ENV), ALLOW_PRIVATE_ENV)?,
            unattended: parse_flag(get(UNATTENDED_ENV), UNATTENDED_ENV)?,
            no_sandbox: parse_flag(get(NO_SANDBOX_ENV), NO_SANDBOX_ENV)?,
            profiles: crate::config_file::home_dir(&get)
                .and_then(|home| std::path::absolute(home.join(PROFILES_FOLDER)).ok()),
        })
    }
}

/// Who answers the tool's approval questions in a run.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Run {
    /// A conversation or the gateway with an approval gate.
    Asked,
    /// `-p`, or `JUNEES_APPROVAL=off`: nobody is asked.
    NotAsked,
    /// A scheduled or webhook run: the tool only with
    /// `browser_unattended`, and nobody is asked.
    Unattended,
}

/// Registers the `browser` tool when the settings turn it on for `run`,
/// and returns what the approval gate asks it about its calls
/// ([`crate::approval::Approval::with_questions`]). `hidden_env` are the
/// user's extra secret variables, kept from the browser like the built-in
/// ones.
pub fn register(
    tools: &mut ToolRegistry,
    settings: &Settings,
    hidden_env: &[String],
    run: Run,
) -> Option<Arc<dyn ToolQuestions>> {
    if !settings.enabled || (run == Run::Unattended && !settings.unattended) {
        return None;
    }
    let tool = BrowserTool::new(settings.clone(), hidden_env.to_vec(), run == Run::Asked);
    let questions = tool.questions();
    tools.register(tool);
    Some(questions)
}
