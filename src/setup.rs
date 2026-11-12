//! `junees setup`: the questions that write the settings file, asked on
//! the first run and whenever the user runs it again.
//!
//! The wizard talks through [`Prompter`] (a terminal in `main`, scripted
//! answers in tests) and checks Claude Code through [`ClaudeProbe`].

use std::path::Path;

use anyhow::{Result, bail};
use async_trait::async_trait;

use crate::claude_code::{ClaudeCode, NotLoggedIn};
use crate::cli::positive;
use crate::config::DEFAULT_MODEL;
use crate::config_file::ConfigFile;
use crate::gateway::config::{Token, snowflakes};
use crate::secrets::secret_kind;
use crate::slack::config::{Kind, Token as SlackToken, user_ids};
use crate::task::budget::parse_duration;
use crate::tools::ToolSettings;

/// The time budget of a task run the wizard offers.
pub const DEFAULT_TASK_TIME: &str = "2h";

/// Every API key of Anthropic starts with this.
pub const KEY_PREFIX: &str = "sk-ant-";

/// Where the wizard's questions go and its answers come from.
#[async_trait]
pub trait Prompter: Send {
    /// Shows a line (or a few) of text.
    fn say(&mut self, text: &str);
    /// Asks for one line; `None` at the end of input.
    async fn line(&mut self, question: &str) -> Result<Option<String>>;
    /// Like [`Prompter::line`], with the answer not shown as it is typed.
    async fn secret(&mut self, question: &str) -> Result<Option<String>>;
}

/// What the check of Claude Code found.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ClaudeStatus {
    /// Installed, new enough and signed in with a subscription; the
    /// version it printed.
    Ready(String),
    /// Installed, but not signed in to a subscription.
    NotSignedIn(String),
    /// Not installed, too old, or signed in some other way.
    Unavailable(String),
}

#[async_trait]
pub trait ClaudeProbe: Send + Sync {
    async fn probe(&self) -> ClaudeStatus;
    /// Warnings about the machine (bubblewrap missing on Linux).
    fn notices(&self) -> Vec<String>;
}

#[async_trait]
impl ClaudeProbe for ClaudeCode {
    /// `claude --version` and `claude auth status --json`, the checks a
    /// task makes before its first session.
    async fn probe(&self) -> ClaudeStatus {
        match self.check(Path::new("/"), &ToolSettings::default()).await {
            Ok(version) => ClaudeStatus::Ready(version),
            Err(err) => match err.downcast_ref::<NotLoggedIn>() {
                Some(NotLoggedIn(detail)) => ClaudeStatus::NotSignedIn(detail.clone()),
                None => ClaudeStatus::Unavailable(format!("{err:#}")),
            },
        }
    }

    fn notices(&self) -> Vec<String> {
        ClaudeCode::notices(self)
    }
}

/// The steps to sign Claude Code in, which only the user can take.
pub fn login_steps() -> String {
    let mut steps = String::from(
        "Sign in to Claude Code yourself (Junees never sees your Claude login):\n\
         \x20 claude                then type /login and follow the steps\n\
         \x20 claude setup-token    instead, on a server without a browser; put the token in\n\
         \x20                       your shell profile: export CLAUDE_CODE_OAUTH_TOKEN=<token>\n",
    );
    if cfg!(target_os = "linux") {
        steps.push_str(
            "On Linux, Claude Code also needs bubblewrap to run commands safely:\n\
             \x20 sudo apt install bubblewrap    (or your distribution's package)\n",
        );
    }
    steps
}

const CANCELLED: &str = "setup cancelled; nothing was saved";

/// Asks the questions and returns the settings to save, starting from
/// `file` (the current settings; empty on the first run). Fails, saving
/// nothing, when the input ends.
pub async fn wizard(
    io: &mut dyn Prompter,
    claude: &dyn ClaudeProbe,
    mut file: ConfigFile,
    path: &Path,
) -> Result<ConfigFile> {
    io.say(&format!(
        "Setting up junees. The answers are saved to {} (only you can read it).",
        path.display()
    ));
    let current = file.get("backend").map(str::to_string);
    let default_choice = match current.as_deref() {
        Some("claude-code") => Some("1"),
        Some(_) => Some("2"),
        None => None,
    };
    io.say(
        "How should junees reach Claude?\n\
         \x20 1) Claude subscription (needs the Claude Code CLI, signed in)\n\
         \x20 2) Anthropic API key",
    );
    let subscription = loop {
        let question = match default_choice {
            Some(choice) => format!("choice [{choice}] > "),
            None => "choice [1/2] > ".to_string(),
        };
        let answer = ask(io, &question).await?;
        match answer.as_str() {
            "1" => break true,
            "2" => break false,
            "" if default_choice.is_some() => break default_choice == Some("1"),
            _ => io.say("  answer 1 or 2"),
        }
    };

    if subscription {
        file.set("backend", "claude-code");
        io.say("Checking Claude Code...");
        match claude.probe().await {
            ClaudeStatus::Ready(version) => io.say(&format!(
                "  Claude Code {version} is signed in with your Claude subscription."
            )),
            ClaudeStatus::NotSignedIn(detail) => {
                io.say(&format!(
                    "  Claude Code is installed but not signed in to a Claude subscription \
                     ({detail})."
                ));
                io.say(&login_steps());
                io.say("  The settings are saved anyway; sign in before you start.");
            }
            ClaudeStatus::Unavailable(problem) => {
                io.say(&format!("  Claude Code is not ready: {problem}"));
                io.say(
                    "  Install it from https://code.claude.com/docs/en/setup (or point \
                     claude_bin in the settings file at it), then:",
                );
                io.say(&login_steps());
                io.say("  The settings are saved anyway; fix this before you start.");
            }
        }
        for notice in claude.notices() {
            io.say(&format!("  Note: {notice}"));
        }
        io.say(
            "The subscription runs everything: the conversation (`junees`, \
             `junees \"<question>\"`)\nand long tasks (`junees task \"<goal>\"`, /task in \
             the conversation), each through Claude Code.",
        );
    } else {
        file.set("backend", "api");
        ask_key(io, &mut file).await?;
    }

    if !subscription && file.get("api_key").is_some() {
        let current = file.get("model").unwrap_or(DEFAULT_MODEL).to_string();
        loop {
            let answer = ask(io, &format!("Model for the conversation [{current}] > ")).await?;
            if answer.is_empty() {
                file.set("model", current.as_str());
                break;
            }
            if answer.contains(char::is_whitespace) {
                io.say("  a model id has no spaces, for example claude-sonnet-4-6");
                continue;
            }
            file.set("model", answer);
            break;
        }
    }

    let current = file
        .get("task_max_time")
        .unwrap_or(DEFAULT_TASK_TIME)
        .to_string();
    loop {
        let answer = ask(
            io,
            &format!("Time limit of each task run (90s, 30m, 2h, 1h30m) [{current}] > "),
        )
        .await?;
        let value = if answer.is_empty() {
            current.clone()
        } else {
            answer
        };
        match parse_duration(&value) {
            Ok(_) => {
                file.set("task_max_time", value);
                break;
            }
            Err(err) => io.say(&format!("  {err:#}")),
        }
    }
    let current = file.get("task_max_tokens").map(str::to_string);
    loop {
        let shown = current.as_deref().unwrap_or("none");
        let answer = ask(
            io,
            &format!("Token limit of a task over all its runs, or none [{shown}] > "),
        )
        .await?;
        if answer.is_empty() {
            break;
        }
        if answer.eq_ignore_ascii_case("none") {
            file.remove("task_max_tokens");
            break;
        }
        match positive(&answer) {
            Some(tokens) => {
                file.set("task_max_tokens", tokens.to_string());
                break;
            }
            None => io.say("  give a whole number such as 2000000, or none"),
        }
    }
    ask_discord(io, &mut file).await?;
    ask_slack(io, &mut file).await?;
    Ok(file)
}

/// The optional Discord step (`junees gateway`): the bot token (hidden),
/// the users it answers, and the channels where it needs no mention. The
/// end of input at its first question skips it; inside it, cancels.
async fn ask_discord(io: &mut dyn Prompter, file: &mut ConfigFile) -> Result<()> {
    let configured = file.get("discord_token").is_some();
    let question = if configured {
        "Change the Discord bot settings (junees gateway)? [y/N] > "
    } else {
        "Set up the Discord bot too (junees gateway, optional)? [y/N] > "
    };
    let Some(answer) = io.line(question).await? else {
        return Ok(());
    };
    if !matches!(answer.trim().to_ascii_lowercase().as_str(), "y" | "yes") {
        return Ok(());
    }
    io.say(
        "Create the bot at https://discord.com/developers/applications (Bot page: Reset Token, \
         and turn on MESSAGE CONTENT INTENT), then invite it; README, \"디스코드 게이트웨이\", has \
         the steps.",
    );
    let keep = if configured {
        "Enter keeps the current one"
    } else {
        "from the Bot page"
    };
    loop {
        let Some(answer) = io
            .secret(&format!(
                "Discord bot token (not shown as you type; {keep}) > "
            ))
            .await?
        else {
            bail!(CANCELLED);
        };
        let token = answer.trim();
        if token.is_empty() {
            if configured {
                break;
            }
            io.say("  the bot needs its token");
            continue;
        }
        // Never repeat what was typed: it may be some other secret.
        if secret_kind(token) != Some("a Discord bot token") || Token::new(token).is_err() {
            io.say(
                "  that does not look like a Discord bot token (three parts joined by dots); \
                 try again",
            );
            continue;
        }
        file.set("discord_token", token);
        io.say("  token saved.");
        break;
    }
    let current = file.get("discord_allowed_users").map(str::to_string);
    loop {
        let shown = current
            .as_deref()
            .map_or_else(String::new, |users| format!(" [{users}]"));
        let answer = ask(
            io,
            &format!("Discord user ids the bot answers, separated by commas{shown} > "),
        )
        .await?;
        let value = match (answer.is_empty(), &current) {
            (true, Some(users)) => users.clone(),
            _ => answer,
        };
        match snowflakes(Some(value), "discord_allowed_users") {
            Ok(ids) if !ids.is_empty() => {
                let mut ids: Vec<String> = ids.into_iter().collect();
                ids.sort();
                file.set("discord_allowed_users", ids.join(","));
                break;
            }
            Ok(_) => io.say(
                "  give at least one: in Discord turn on Developer Mode, then Copy User ID on \
                 yourself",
            ),
            Err(err) => io.say(&format!("  {err}")),
        }
    }
    let current = file.get("discord_channels").map(str::to_string);
    loop {
        let shown = current.as_deref().unwrap_or("none");
        let answer = ask(
            io,
            &format!("Channel ids where the bot answers without a mention, or none [{shown}] > "),
        )
        .await?;
        if answer.is_empty() {
            break;
        }
        if answer.eq_ignore_ascii_case("none") {
            file.remove("discord_channels");
            break;
        }
        match snowflakes(Some(answer), "discord_channels") {
            Ok(ids) => {
                let mut ids: Vec<String> = ids.into_iter().collect();
                ids.sort();
                if ids.is_empty() {
                    file.remove("discord_channels");
                } else {
                    file.set("discord_channels", ids.join(","));
                }
                break;
            }
            Err(err) => io.say(&format!("  {err}")),
        }
    }
    io.say("Start the bot with `junees gateway`.");
    Ok(())
}

/// The optional Slack step (`junees gateway slack`): the bot token and the
/// app-level token (both hidden) and the users it answers. Like the
/// Discord step, the end of input at its first question skips it; inside
/// it, cancels.
async fn ask_slack(io: &mut dyn Prompter, file: &mut ConfigFile) -> Result<()> {
    let configured = file.get("slack_bot_token").is_some() || file.get("slack_app_token").is_some();
    let question = if configured {
        "Change the Slack app settings (junees gateway slack)? [y/N] > "
    } else {
        "Set up the Slack app too (junees gateway slack, optional)? [y/N] > "
    };
    let Some(answer) = io.line(question).await? else {
        return Ok(());
    };
    if !matches!(answer.trim().to_ascii_lowercase().as_str(), "y" | "yes") {
        return Ok(());
    }
    io.say(
        "Create the app at https://api.slack.com/apps from the manifest in docs/gateways.md \
         (\"슬랙 게이트웨이\": Socket Mode and Interactivity on), then install it to the \
         workspace.",
    );
    ask_slack_token(
        io,
        file,
        Kind::Bot,
        "Slack bot token, xoxb-...",
        "the Bot User OAuth Token on the OAuth & Permissions page",
    )
    .await?;
    ask_slack_token(
        io,
        file,
        Kind::App,
        "Slack app-level token, xapp-...",
        "Basic Information, App-Level Tokens, scope connections:write",
    )
    .await?;
    let current = file.get("slack_allowed_users").map(str::to_string);
    loop {
        let shown = current
            .as_deref()
            .map_or_else(String::new, |users| format!(" [{users}]"));
        let answer = ask(
            io,
            &format!("Slack member ids the app answers (U...), separated by commas{shown} > "),
        )
        .await?;
        let value = match (answer.is_empty(), &current) {
            (true, Some(users)) => users.clone(),
            _ => answer,
        };
        match user_ids(Some(value), "slack_allowed_users") {
            Ok(ids) if !ids.is_empty() => {
                let mut ids: Vec<String> = ids.into_iter().collect();
                ids.sort();
                file.set("slack_allowed_users", ids.join(","));
                break;
            }
            Ok(_) => io.say(
                "  give at least one: in Slack open your profile, then the menu, then Copy \
                 member ID",
            ),
            Err(err) => io.say(&format!("  {err}")),
        }
    }
    io.say("Start the app with `junees gateway slack`.");
    Ok(())
}

/// Asks for one of the Slack tokens without showing it, until it has the
/// shape of `kind`; an empty answer keeps the one saved.
async fn ask_slack_token(
    io: &mut dyn Prompter,
    file: &mut ConfigFile,
    kind: Kind,
    what: &str,
    from: &str,
) -> Result<()> {
    let key = match kind {
        Kind::Bot => "slack_bot_token",
        Kind::App => "slack_app_token",
    };
    let saved = file.get(key).is_some();
    let keep = if saved {
        "Enter keeps the current one"
    } else {
        from
    };
    loop {
        let Some(answer) = io
            .secret(&format!("{what} (not shown as you type; {keep}) > "))
            .await?
        else {
            bail!(CANCELLED);
        };
        let token = answer.trim();
        if token.is_empty() {
            if saved {
                return Ok(());
            }
            io.say("  the app needs this token");
            continue;
        }
        // The error never repeats what was typed: it may be some other
        // secret. It says so when the two tokens are swapped.
        if let Err(err) = SlackToken::new(token, kind) {
            io.say(&format!("  {err}; try again"));
            continue;
        }
        file.set(key, token);
        io.say("  token saved.");
        return Ok(());
    }
}

/// One trimmed answer; the end of input cancels the setup.
async fn ask(io: &mut dyn Prompter, question: &str) -> Result<String> {
    match io.line(question).await? {
        Some(answer) => Ok(answer.trim().to_string()),
        None => bail!(CANCELLED),
    }
}

/// Asks for the API key without showing it, until it looks like one. An
/// empty answer keeps the current key.
async fn ask_key(io: &mut dyn Prompter, file: &mut ConfigFile) -> Result<()> {
    let has_key = file.get("api_key").is_some();
    let keep = if has_key {
        "Enter keeps the current one"
    } else {
        "get one at https://console.anthropic.com"
    };
    loop {
        let Some(answer) = io
            .secret(&format!(
                "Anthropic API key (not shown as you type; {keep}) > "
            ))
            .await?
        else {
            bail!(CANCELLED);
        };
        let key = answer.trim();
        if key.is_empty() {
            if has_key {
                return Ok(());
            }
            io.say("  the API key is needed for this choice");
            continue;
        }
        // Never repeat what was typed: it may be some other secret.
        if !key.starts_with(KEY_PREFIX) || key.contains(char::is_whitespace) {
            io.say(&format!(
                "  that does not look like an Anthropic API key (those start with {KEY_PREFIX}); \
                 try again"
            ));
            continue;
        }
        file.set("api_key", key);
        io.say("  API key saved.");
        return Ok(());
    }
}

/// Turns off the echo of a terminal while it lives, so a typed secret does
/// not show; restores the terminal's mode when dropped.
pub struct EchoOff {
    fd: i32,
    saved: libc::termios,
}

impl EchoOff {
    /// `None` when `fd` is not a terminal.
    pub fn new(fd: i32) -> std::io::Result<Option<Self>> {
        // SAFETY: isatty, tcgetattr and tcsetattr only read `fd` and the
        // termios struct they are given, which lives on this stack.
        unsafe {
            if libc::isatty(fd) != 1 {
                return Ok(None);
            }
            let mut saved: libc::termios = std::mem::zeroed();
            if libc::tcgetattr(fd, &mut saved) != 0 {
                return Err(std::io::Error::last_os_error());
            }
            let mut quiet = saved;
            quiet.c_lflag &= !(libc::ECHO | libc::ECHONL);
            // TCSANOW: what was typed ahead stays.
            if libc::tcsetattr(fd, libc::TCSANOW, &quiet) != 0 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(Some(Self { fd, saved }))
        }
    }
}

impl Drop for EchoOff {
    fn drop(&mut self) {
        // SAFETY: as in `new`; nothing to do if the terminal is gone.
        unsafe {
            libc::tcsetattr(self.fd, libc::TCSANOW, &self.saved);
        }
    }
}

#[cfg(test)]
mod tests {
    use std::collections::VecDeque;

    use super::*;

    /// Plays answers in order and records everything shown, with the
    /// questions whose answers were hidden marked.
    struct Scripted {
        answers: VecDeque<&'static str>,
        shown: String,
        secret_questions: usize,
    }

    impl Scripted {
        fn new(answers: &[&'static str]) -> Self {
            Self {
                answers: answers.iter().copied().collect(),
                shown: String::new(),
                secret_questions: 0,
            }
        }
    }

    #[async_trait]
    impl Prompter for Scripted {
        fn say(&mut self, text: &str) {
            self.shown.push_str(text);
            self.shown.push('\n');
        }

        async fn line(&mut self, question: &str) -> Result<Option<String>> {
            self.shown.push_str(question);
            let answer = self.answers.pop_front().map(str::to_string);
            // A visible answer shows on the terminal.
            if let Some(answer) = &answer {
                self.shown.push_str(answer);
            }
            self.shown.push('\n');
            Ok(answer)
        }

        async fn secret(&mut self, question: &str) -> Result<Option<String>> {
            self.shown.push_str(question);
            self.shown.push('\n');
            self.secret_questions += 1;
            Ok(self.answers.pop_front().map(str::to_string))
        }
    }

    struct Probe(ClaudeStatus);

    #[async_trait]
    impl ClaudeProbe for Probe {
        async fn probe(&self) -> ClaudeStatus {
            self.0.clone()
        }

        fn notices(&self) -> Vec<String> {
            vec!["`bwrap` is not on PATH".into()]
        }
    }

    fn ready() -> Probe {
        Probe(ClaudeStatus::Ready("2.1.319 (Claude Code)".into()))
    }

    async fn run(
        answers: &[&'static str],
        probe: &Probe,
        file: ConfigFile,
    ) -> (Result<ConfigFile>, Scripted) {
        let mut io = Scripted::new(answers);
        let result = wizard(&mut io, probe, file, Path::new("/h/.junees/config.toml")).await;
        assert!(io.answers.is_empty(), "unused answers: {:?}", io.answers);
        (result, io)
    }

    const KEY: &str = "sk-ant-api03-test-key";

    #[tokio::test]
    async fn the_api_path_asks_for_a_hidden_key() {
        let (file, io) = run(
            &["2", "not-a-key", KEY, "", "", ""],
            &ready(),
            ConfigFile::default(),
        )
        .await;
        let file = file.unwrap();
        assert_eq!(file.get("backend"), Some("api"));
        assert_eq!(file.get("api_key"), Some(KEY));
        assert_eq!(file.get("model"), Some(DEFAULT_MODEL));
        assert_eq!(file.get("task_max_time"), Some("2h"));
        assert_eq!(file.get("task_max_tokens"), None);
        // Both key answers were asked for hidden, and neither shows.
        assert_eq!(io.secret_questions, 2);
        assert!(!io.shown.contains("test-key"), "{}", io.shown);
        assert!(!io.shown.contains("not-a-key"), "{}", io.shown);
        assert!(io.shown.contains("does not look like an Anthropic API key"));
        assert!(io.shown.contains("API key saved."));
        assert!(io.shown.contains("/h/.junees/config.toml"));
        // The subscription was not checked.
        assert!(!io.shown.contains("Checking Claude Code"), "{}", io.shown);
    }

    #[tokio::test]
    async fn the_api_path_needs_a_key_and_takes_budget_answers() {
        let (file, _) = run(
            &[
                "3",
                "2",
                "",
                KEY,
                "claude-opus-4-1",
                "2 hours",
                "90m",
                "lots",
                "1500000",
            ],
            &ready(),
            ConfigFile::default(),
        )
        .await;
        let file = file.unwrap();
        assert_eq!(file.get("model"), Some("claude-opus-4-1"));
        assert_eq!(file.get("task_max_time"), Some("90m"));
        assert_eq!(file.get("task_max_tokens"), Some("1500000"));
    }

    #[tokio::test]
    async fn a_subscription_that_is_not_signed_in_gets_the_steps_and_is_saved() {
        let probe = Probe(ClaudeStatus::NotSignedIn(
            "`claude auth status`: none".into(),
        ));
        let (file, io) = run(&["1", "", ""], &probe, ConfigFile::default()).await;
        let file = file.unwrap();
        assert_eq!(file.get("backend"), Some("claude-code"));
        assert_eq!(file.get("api_key"), None);
        // No key, so no model question.
        assert_eq!(file.get("model"), None);
        assert_eq!(file.get("task_max_time"), Some("2h"));
        assert!(
            io.shown.contains("not signed in to a Claude subscription"),
            "{}",
            io.shown
        );
        assert!(io.shown.contains("then type /login"), "{}", io.shown);
        assert!(io.shown.contains("claude setup-token"), "{}", io.shown);
        if cfg!(target_os = "linux") {
            assert!(io.shown.contains("needs bubblewrap"), "{}", io.shown);
        }
        assert!(
            io.shown.contains("Note: `bwrap` is not on PATH"),
            "{}",
            io.shown
        );
        // The conversation runs on the subscription too: no key asked.
        assert!(
            io.shown
                .contains("The subscription runs everything: the conversation"),
            "{}",
            io.shown
        );
        assert!(!io.shown.contains("needs an API key"), "{}", io.shown);
        assert_eq!(io.secret_questions, 0);
    }

    #[tokio::test]
    async fn a_ready_subscription_asks_for_no_key_and_keeps_one_it_has() {
        let (file, io) = run(&["1", "", ""], &ready(), ConfigFile::default()).await;
        let file = file.unwrap();
        assert_eq!(file.get("backend"), Some("claude-code"));
        assert_eq!(file.get("api_key"), None);
        assert!(
            io.shown
                .contains("Claude Code 2.1.319 (Claude Code) is signed in")
        );
        assert!(!io.shown.contains("needs an API key"), "{}", io.shown);
        assert_eq!(io.secret_questions, 0);

        // A key saved for the api backend stays for a later switch back,
        // and no model of the API is asked about.
        let mut old = ConfigFile::default();
        old.set("backend", "api");
        old.set("api_key", "sk-ant-old");
        old.set("model", "claude-haiku-4-5");
        let (file, io) = run(&["1", "", ""], &ready(), old).await;
        let file = file.unwrap();
        assert_eq!(file.get("backend"), Some("claude-code"));
        assert_eq!(file.get("api_key"), Some("sk-ant-old"));
        assert!(
            !io.shown.contains("Model for the conversation"),
            "{}",
            io.shown
        );
    }

    #[tokio::test]
    async fn running_it_again_keeps_what_is_not_changed() {
        let mut old = ConfigFile::default();
        old.set("backend", "api");
        old.set("api_key", "sk-ant-old");
        old.set("model", "claude-haiku-4-5");
        old.set("approval", "all");
        old.set("task_max_tokens", "9000");
        let (file, io) = run(&["", "", "", "", ""], &ready(), old.clone()).await;
        let file = file.unwrap();
        assert!(io.shown.contains("choice [2] > "), "{}", io.shown);
        assert!(io.shown.contains("[claude-haiku-4-5]"), "{}", io.shown);
        assert!(io.shown.contains("[9000]"), "{}", io.shown);
        assert!(!io.shown.contains("sk-ant-old"));
        let mut expected = old;
        expected.set("task_max_time", "2h");
        assert!(file == expected);
        // `none` drops the token limit.
        let (file, _) = run(&["", "", "", "", "none"], &ready(), file).await;
        assert_eq!(file.unwrap().get("task_max_tokens"), None);
    }

    /// A token of the Discord shape, made at run time.
    fn discord_token() -> &'static str {
        // The first part is base64 of the id 1234567890123456789.
        let token = format!(
            "MTIzNDU2Nzg5MDEyMzQ1Njc4OQ.Gx1234.{}",
            "h7Kq_2-Lm".repeat(4)
        );
        Box::leak(token.into_boxed_str())
    }

    #[tokio::test]
    async fn the_discord_step_is_optional_and_hides_the_token() {
        let token = discord_token();
        let (file, io) = run(
            &[
                "2",
                KEY,
                "",
                "",
                "",
                "y",
                "not a token",
                token,
                "",
                "me",
                "111, 222",
                "333",
            ],
            &ready(),
            ConfigFile::default(),
        )
        .await;
        let file = file.unwrap();
        assert_eq!(file.get("discord_token"), Some(token));
        assert_eq!(file.get("discord_allowed_users"), Some("111,222"));
        assert_eq!(file.get("discord_channels"), Some("333"));
        // The key and both token answers were hidden.
        assert_eq!(io.secret_questions, 3);
        assert!(!io.shown.contains("h7Kq"), "{}", io.shown);
        assert!(io.shown.contains("at least one"), "{}", io.shown);
        assert!(io.shown.contains("Discord ids"), "{}", io.shown);

        // Again: Enter keeps everything; `none` drops the channels.
        let (again, _) = run(
            &["", "", "", "", "", "y", "", "", "none"],
            &ready(),
            file.clone(),
        )
        .await;
        let again = again.unwrap();
        assert_eq!(again.get("discord_token"), Some(token));
        assert_eq!(again.get("discord_allowed_users"), Some("111,222"));
        assert_eq!(again.get("discord_channels"), None);

        // No, or the end of input, skips the step.
        let (skipped, _) = run(
            &["2", KEY, "", "", "", "n"],
            &ready(),
            ConfigFile::default(),
        )
        .await;
        assert_eq!(skipped.unwrap().get("discord_token"), None);
    }

    /// Slack tokens of the right shapes, made at run time.
    fn slack_tokens() -> (String, String) {
        (
            ["xoxb", "1-2-wizard-secret-bot"].join("-"),
            ["xapp", "1-A1-wizard-secret-app"].join("-"),
        )
    }

    #[tokio::test]
    async fn the_slack_step_is_optional_and_hides_the_tokens() {
        let (bot, app) = slack_tokens();
        let (bot, app): (&'static str, &'static str) = (bot.leak(), app.leak());
        let (file, io) = run(
            &[
                "2",
                KEY,
                "",
                "",
                "",
                "n",
                "y",
                // The bot token: not one, then the app token, then it.
                "not a token",
                app,
                bot,
                // The app token: empty, the bot token, then it.
                "",
                bot,
                app,
                // The users: none, a name, then two ids.
                "",
                "me",
                "U2B, U1A",
            ],
            &ready(),
            ConfigFile::default(),
        )
        .await;
        let file = file.unwrap();
        assert_eq!(file.get("slack_bot_token"), Some(bot));
        assert_eq!(file.get("slack_app_token"), Some(app));
        assert_eq!(file.get("slack_allowed_users"), Some("U1A,U2B"));
        assert_eq!(file.get("discord_token"), None);
        // The key and the six token answers were hidden, and none shows.
        assert_eq!(io.secret_questions, 7);
        assert!(!io.shown.contains("wizard-secret"), "{}", io.shown);
        assert!(
            io.shown.contains("it is the app-level token"),
            "{}",
            io.shown
        );
        assert!(io.shown.contains("it is the bot token"), "{}", io.shown);
        assert!(
            io.shown.contains("the app needs this token"),
            "{}",
            io.shown
        );
        assert!(io.shown.contains("at least one"), "{}", io.shown);
        assert!(io.shown.contains("member ids"), "{}", io.shown);
        assert!(io.shown.contains("Interactivity on"), "{}", io.shown);
        assert!(io.shown.contains("`junees gateway slack`"), "{}", io.shown);

        // Again: Enter keeps everything.
        let (again, io) = run(
            &["", "", "", "", "", "n", "y", "", "", ""],
            &ready(),
            file.clone(),
        )
        .await;
        assert!(again.unwrap() == file);
        assert!(
            io.shown.contains("Change the Slack app settings"),
            "{}",
            io.shown
        );
        assert!(io.shown.contains("[U1A,U2B]"), "{}", io.shown);

        // No, or the end of input, skips the step; the end of input inside
        // it cancels.
        let (skipped, _) = run(
            &["2", KEY, "", "", "", "n", "n"],
            &ready(),
            ConfigFile::default(),
        )
        .await;
        assert_eq!(skipped.unwrap().get("slack_bot_token"), None);
        let (cancelled, _) = run(
            &["2", KEY, "", "", "", "n", "y", bot],
            &ready(),
            ConfigFile::default(),
        )
        .await;
        assert_eq!(cancelled.err().unwrap().to_string(), CANCELLED);
    }

    #[tokio::test]
    async fn the_end_of_input_cancels() {
        for answers in [&[][..], &["2"], &["2", KEY, ""]] {
            let mut io = Scripted::new(answers);
            let err = wizard(&mut io, &ready(), ConfigFile::default(), Path::new("/c"))
                .await
                .err()
                .unwrap();
            assert_eq!(err.to_string(), CANCELLED, "{answers:?}");
        }
    }

    #[test]
    fn echo_off_hides_typing_and_restores_the_terminal() {
        let (mut leader, mut follower) = (0, 0);
        // SAFETY: openpty fills in two descriptors that are closed below.
        unsafe {
            assert_eq!(
                libc::openpty(
                    &mut leader,
                    &mut follower,
                    std::ptr::null_mut(),
                    std::ptr::null_mut(),
                    std::ptr::null_mut(),
                ),
                0
            );
        }
        let lflag = || {
            // SAFETY: a valid descriptor and a zeroed struct to fill in.
            unsafe {
                let mut mode: libc::termios = std::mem::zeroed();
                assert_eq!(libc::tcgetattr(follower, &mut mode), 0);
                mode.c_lflag
            }
        };
        assert_ne!(lflag() & libc::ECHO, 0);
        let guard = EchoOff::new(follower).unwrap().expect("a terminal");
        assert_eq!(lflag() & libc::ECHO, 0);
        drop(guard);
        assert_ne!(lflag() & libc::ECHO, 0);
        // SAFETY: closing the descriptors openpty opened.
        unsafe {
            libc::close(leader);
            libc::close(follower);
        }
        // Not a terminal: nothing to do.
        let file = tempfile::tempfile().unwrap();
        use std::os::fd::AsRawFd;
        assert!(EchoOff::new(file.as_raw_fd()).unwrap().is_none());
    }
}
