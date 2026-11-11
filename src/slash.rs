//! The slash commands of the conversation mode.

use crate::claude_code::Backend;
use crate::memory::Target;

/// One command typed at the conversation prompt.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Slash {
    /// `/task <goal>` plans a task and works on it; `/task` keeps working.
    Task(Option<String>),
    Status,
    Sessions,
    /// `/search <words>` searches the saved sessions; the text after the
    /// command, options included ([`crate::search::args::HumanSearch`]).
    Search(String),
    /// `/new` (or `/reset`).
    New,
    /// `/model` shows the model, `/model <name>` switches to it.
    Model(Option<String>),
    /// `/backend` shows what runs the conversation and `/task`,
    /// `/backend <name>` switches both.
    Backend(Option<Backend>),
    Compact,
    Context,
    Usage,
    /// `/memory` shows what the persistent memory holds.
    Memory,
    /// `/memory clear <target>` empties one store, after asking.
    MemoryClear(Target),
    /// `/skills` lists the skills.
    Skills,
    /// `/skill <name>` shows one.
    Skill(String),
    /// `/mcp` shows the MCP servers and their tools.
    Mcp,
    /// `/plugins` shows the plugins.
    Plugins,
    /// `/cron` lists the scheduled jobs.
    Cron,
    /// `/github` shows the GitHub settings and the rate limit left.
    Github,
    /// `/webhooks` lists the webhook routes.
    Webhooks,
    /// `/doctor` checks the installation and the settings.
    Doctor,
    Help,
    Exit,
    /// A command typed wrong: what to tell the user.
    Bad(String),
}

/// The command in `line`, or `None` when the line is input for the model
/// (anything not starting with `/word`, such as `/etc/hosts 읽어 줘`).
pub fn parse(line: &str) -> Option<Slash> {
    let line = line.trim();
    let rest = line.strip_prefix('/')?;
    let (name, arg) = match rest.split_once(char::is_whitespace) {
        Some((name, arg)) => (name, arg.trim()),
        None => (rest, ""),
    };
    let is_word = !name.is_empty()
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_');
    if !is_word {
        return None;
    }
    let arg = (!arg.is_empty()).then(|| arg.to_string());
    let plain = |command: Slash| match &arg {
        None => command,
        Some(_) => Slash::Bad(format!("/{name} takes no argument")),
    };
    Some(match name {
        "task" => Slash::Task(arg),
        "status" => plain(Slash::Status),
        "sessions" => plain(Slash::Sessions),
        "search" => match arg {
            None => Slash::Bad(
                "/search takes the words to look for, for example /search \"build failed\"".into(),
            ),
            Some(arg) => Slash::Search(arg),
        },
        "new" | "reset" => plain(Slash::New),
        "model" => match arg {
            Some(model) if model.contains(char::is_whitespace) => {
                Slash::Bad("a model id has no spaces, for example /model claude-sonnet-4-6".into())
            }
            model => Slash::Model(model),
        },
        "backend" => match arg.as_deref().map(Backend::parse) {
            None => Slash::Backend(None),
            Some(Ok(backend)) => Slash::Backend(Some(backend)),
            Some(Err(err)) => Slash::Bad(format!("{err}")),
        },
        "compact" => plain(Slash::Compact),
        "context" => plain(Slash::Context),
        "usage" => plain(Slash::Usage),
        "memory" => memory(arg.as_deref()),
        "skills" => plain(Slash::Skills),
        "mcp" => plain(Slash::Mcp),
        "plugins" => plain(Slash::Plugins),
        "cron" => plain(Slash::Cron),
        "github" => plain(Slash::Github),
        "webhooks" => plain(Slash::Webhooks),
        "doctor" => plain(Slash::Doctor),
        "skill" => match arg {
            None => Slash::Bad("/skill takes a skill name; /skills lists them".into()),
            Some(name) => match crate::skills::check_name(&name) {
                Ok(()) => Slash::Skill(name),
                Err(err) => Slash::Bad(err),
            },
        },
        "help" => plain(Slash::Help),
        "exit" | "quit" => plain(Slash::Exit),
        _ => Slash::Bad(format!("unknown command /{name}; /help lists the commands")),
    })
}

/// `/memory` and `/memory clear <target>`.
fn memory(arg: Option<&str>) -> Slash {
    const USAGE: &str = "use /memory, or /memory clear user|notes|project";
    let Some(arg) = arg else {
        return Slash::Memory;
    };
    let mut words = arg.split_whitespace();
    match (words.next(), words.next(), words.next()) {
        (Some("clear"), Some(name), None) => match Target::parse(name) {
            Some(target) => Slash::MemoryClear(target),
            None => Slash::Bad(format!("unknown memory `{name}`; {USAGE}")),
        },
        (Some("clear"), None, _) => Slash::Bad(format!("which memory? {USAGE}")),
        _ => Slash::Bad(USAGE.into()),
    }
}

/// The session picked from a numbered list of `count`: `Ok(None)` for an
/// empty answer, the index for `1..=count`.
pub fn pick(answer: &str, count: usize) -> Result<Option<usize>, String> {
    let answer = answer.trim();
    if answer.is_empty() {
        return Ok(None);
    }
    match answer.parse::<usize>() {
        Ok(n) if (1..=count).contains(&n) => Ok(Some(n - 1)),
        _ => Err(format!("pick a number from 1 to {count}")),
    }
}

/// The name of a backend as typed.
pub fn backend_name(backend: Backend) -> &'static str {
    match backend {
        Backend::Api => "api",
        Backend::ClaudeCode => "claude-code",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_commands() {
        assert_eq!(
            parse(" /task  make a todo app "),
            Some(Slash::Task(Some("make a todo app".into())))
        );
        assert_eq!(parse("/task"), Some(Slash::Task(None)));
        assert_eq!(parse("/status"), Some(Slash::Status));
        assert_eq!(parse("/sessions"), Some(Slash::Sessions));
        assert_eq!(
            parse("/search  --all \"빌드 실패\" clippy "),
            Some(Slash::Search("--all \"빌드 실패\" clippy".into()))
        );
        assert_eq!(parse("/new"), Some(Slash::New));
        assert_eq!(parse("/reset"), Some(Slash::New));
        assert_eq!(parse("/model"), Some(Slash::Model(None)));
        assert_eq!(
            parse("/model claude-opus-4-1"),
            Some(Slash::Model(Some("claude-opus-4-1".into())))
        );
        assert_eq!(parse("/backend"), Some(Slash::Backend(None)));
        assert_eq!(
            parse("/backend claude-code"),
            Some(Slash::Backend(Some(Backend::ClaudeCode)))
        );
        assert_eq!(
            parse("/backend api"),
            Some(Slash::Backend(Some(Backend::Api)))
        );
        for (line, command) in [
            ("/compact", Slash::Compact),
            ("/context", Slash::Context),
            ("/usage", Slash::Usage),
            ("/memory", Slash::Memory),
            ("/memory clear user", Slash::MemoryClear(Target::User)),
            ("/memory  clear   notes ", Slash::MemoryClear(Target::Notes)),
            ("/memory clear project", Slash::MemoryClear(Target::Project)),
            ("/skills", Slash::Skills),
            ("/cron", Slash::Cron),
            ("/github", Slash::Github),
            ("/webhooks", Slash::Webhooks),
            ("/doctor", Slash::Doctor),
            ("/skill deploy-app", Slash::Skill("deploy-app".into())),
            ("/help", Slash::Help),
            ("/exit", Slash::Exit),
            ("/quit", Slash::Exit),
        ] {
            assert_eq!(parse(line), Some(command), "{line}");
        }
    }

    #[test]
    fn mistakes_are_explained() {
        let bad = |line: &str| match parse(line) {
            Some(Slash::Bad(message)) => message,
            other => panic!("{line}: {other:?}"),
        };
        assert_eq!(
            bad("/wat"),
            "unknown command /wat; /help lists the commands"
        );
        assert_eq!(bad("/status now"), "/status takes no argument");
        assert!(bad("/search").starts_with("/search takes the words"));
        assert!(bad("/backend codex").contains("unknown backend `codex`"));
        assert!(bad("/model two words").contains("no spaces"));
        assert_eq!(
            bad("/memory clear"),
            "which memory? use /memory, or /memory clear user|notes|project"
        );
        assert_eq!(
            bad("/memory clear all"),
            "unknown memory `all`; use /memory, or /memory clear user|notes|project"
        );
        for line in ["/memory show", "/memory clear user notes"] {
            assert_eq!(
                bad(line),
                "use /memory, or /memory clear user|notes|project",
                "{line}"
            );
        }
        assert_eq!(
            bad("/skill"),
            "/skill takes a skill name; /skills lists them"
        );
        assert!(bad("/skill ../x").contains("invalid skill name"));
        assert_eq!(bad("/skills all"), "/skills takes no argument");
    }

    #[test]
    fn other_input_goes_to_the_model() {
        for line in [
            "hello",
            "/etc/hosts 읽어 줘",
            "/ 안녕",
            "//",
            "/한글",
            "a /task",
        ] {
            assert_eq!(parse(line), None, "{line}");
        }
    }

    #[test]
    fn picks_a_session_by_number() {
        assert_eq!(pick("", 3), Ok(None));
        assert_eq!(pick(" 1 ", 3), Ok(Some(0)));
        assert_eq!(pick("3", 3), Ok(Some(2)));
        for bad in ["0", "4", "x", "-1"] {
            assert_eq!(
                pick(bad, 3),
                Err("pick a number from 1 to 3".into()),
                "{bad}"
            );
        }
    }
}
