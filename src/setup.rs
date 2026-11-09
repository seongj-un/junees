//! `junees setup`: the questions that write the settings file, asked on
//! the first run and whenever the user runs it again.
//!
//! The wizard talks through [`Prompter`] (a terminal in `main`, scripted
//! answers in tests) and checks Claude Code through [`ClaudeProbe`].

use std::path::Path;

use anyhow::{Result, bail};
use async_trait::async_trait;

use crate::cli::positive;
use crate::config::DEFAULT_MODEL;
use crate::config_file::ConfigFile;
use crate::task::budget::parse_duration;
use crate::task::claude_code::{ClaudeCode, NotLoggedIn};
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
                io.say("  The settings are saved anyway; sign in before your first task.");
            }
            ClaudeStatus::Unavailable(problem) => {
                io.say(&format!("  Claude Code is not ready: {problem}"));
                io.say(
                    "  Install it from https://code.claude.com/docs/en/setup (or point \
                     claude_bin in the settings file at it), then:",
                );
                io.say(&login_steps());
                io.say("  The settings are saved anyway; fix this before your first task.");
            }
        }
        for notice in claude.notices() {
            io.say(&format!("  Note: {notice}"));
        }
        io.say(
            "The subscription runs long tasks (`junees task \"<goal>\"`, /task in the \
             conversation).\nThe conversation itself (`junees`, `junees \"<question>\"`) \
             still talks to the Anthropic API\nand needs an API key: it cannot use the \
             subscription yet.",
        );
        let has_key = file.get("api_key").is_some();
        let question = if has_key {
            "Change the API key for the conversation? [y/N] > "
        } else {
            "Add an API key for the conversation now? [y/N] > "
        };
        let answer = ask(io, question).await?;
        if matches!(answer.to_ascii_lowercase().as_str(), "y" | "yes") {
            ask_key(io, &mut file, true).await?;
        }
    } else {
        file.set("backend", "api");
        ask_key(io, &mut file, false).await?;
    }

    if file.get("api_key").is_some() {
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
    Ok(file)
}

/// One trimmed answer; the end of input cancels the setup.
async fn ask(io: &mut dyn Prompter, question: &str) -> Result<String> {
    match io.line(question).await? {
        Some(answer) => Ok(answer.trim().to_string()),
        None => bail!(CANCELLED),
    }
}

/// Asks for the API key without showing it, until it looks like one. An
/// empty answer keeps the current key, or skips it when `optional`.
async fn ask_key(io: &mut dyn Prompter, file: &mut ConfigFile, optional: bool) -> Result<()> {
    let has_key = file.get("api_key").is_some();
    let keep = if has_key {
        "Enter keeps the current one"
    } else if optional {
        "Enter skips"
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
            if has_key || optional {
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
        let (file, io) = run(&["1", "n", "", ""], &probe, ConfigFile::default()).await;
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
        // Honest about the conversation mode.
        assert!(
            io.shown
                .contains("needs an API key: it cannot use the subscription")
        );
        assert_eq!(io.secret_questions, 0);
    }

    #[tokio::test]
    async fn a_ready_subscription_can_add_a_key_for_the_conversation() {
        let (file, io) = run(
            &["1", "y", KEY, "", "", ""],
            &ready(),
            ConfigFile::default(),
        )
        .await;
        let file = file.unwrap();
        assert_eq!(file.get("backend"), Some("claude-code"));
        assert_eq!(file.get("api_key"), Some(KEY));
        assert!(
            io.shown
                .contains("Claude Code 2.1.319 (Claude Code) is signed in")
        );
        assert!(!io.shown.contains(KEY));
        // An optional key can be skipped with Enter.
        let (file, _) = run(&["1", "yes", "", "", ""], &ready(), ConfigFile::default()).await;
        assert_eq!(file.unwrap().get("api_key"), None);
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
                    std::ptr::null(),
                    std::ptr::null(),
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
