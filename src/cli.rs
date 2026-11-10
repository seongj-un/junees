//! Command-line parsing and terminal output helpers.

use std::time::Duration;

use anyhow::{Context, Result, bail};

use crate::claude_code::Backend;
use crate::session::{Summary, format_utc};
use crate::task::DEFAULT_MAX_SESSIONS;
use crate::task::budget::parse_duration;

pub const HELP: &str = "\
junees - an AI agent in your terminal

Usage:
  junees                  talk with the agent (the first run sets it up)
  junees \"<question>\"     ask once and exit
  junees -c               continue the last conversation in this folder
  junees task \"<goal>\"    plan a long task in this git repository and work on it
  junees task             keep working on the task
  junees status           show where the task stands
  junees setup            change the settings
  junees gateway          run as a Discord bot (also `junees discord`; see README)

In the conversation, /help lists the commands (/task, /status, /sessions,
/new, /model, /backend, ...). Settings are in $JUNEES_HOME/config.toml
(default ~/.junees/config.toml); environment variables win over it.
A one-word question that is also a command (`status`) needs -p.

Advanced:
  junees [OPTIONS] [-p] <PROMPT>  run one prompt and exit
  junees task init [--backend <B>] \"<GOAL>\"
                                only plan a long-running task
  junees task [run] [--backend <B>] [--max-sessions <N>] [--max-tokens <N>] [--max-time <DURATION>]
                                work on the task, one feature per session
  junees task \"<GOAL>\" [the run options]
                                plan it, then work on it (task init + task run)
  junees [task] status [--json] show where the task stands (read-only)

Options:
  -p, --prompt <PROMPT>  run a single prompt non-interactively
  -c, --continue         continue the latest session started in this folder
      --resume <ID>      continue the session with this id (a unique prefix is enough)
      --no-session       do not save this session
  -h, --help             print this help
  -V, --version          print the version

Sessions are saved as they happen to $JUNEES_HOME/sessions/<id>.jsonl.

Task mode keeps the feature list, progress notes and state in .junees/ of
the repository. `task run` stops when every feature passes, after
--max-sessions sessions (default 20), after 3 sessions in a row without a
newly passing feature, when a session fails, or when the budget runs out:
--max-tokens counts the tokens of every session of the task (over all
runs), --max-time the time of this run (90s, 30m, 2h, 1h30m).

The backend picks what runs the conversation and the task sessions: `api`
(default), Junees's own agent on ANTHROPIC_API_KEY, or `claude-code`, the
official Claude Code CLI signed in with your own Claude subscription
(JUNEES_BACKEND, `backend` in config.toml, /backend in the conversation;
--backend for one task). Junees only starts `claude`; sign in yourself
first (run `claude`, then /login). It needs no ANTHROPIC_API_KEY, and
removes the API variables from claude's environment. On claude-code each
turn of the conversation is one `claude -p` run that continues the same
Claude Code session (--resume).

Environment (each also a key of config.toml, named in brackets):
  ANTHROPIC_API_KEY   [api_key] needed on the api backend only
  ANTHROPIC_BASE_URL  [base_url] API base URL (default https://api.anthropic.com)
  JUNEES_MODEL        [model] model id (default claude-sonnet-4-6)
  JUNEES_MAX_TOKENS   [max_tokens] max output tokens per model call (default 8192)
  JUNEES_MAX_TURNS    [max_turns] max model calls per prompt (default 50)
  JUNEES_CONTEXT_WINDOW     [context_window] model context window in tokens (default 200000)
  JUNEES_COMPACT_THRESHOLD  [compact_threshold] share of the window at which old tool
                            output is cleared and the conversation compacted (default 0.8)
  JUNEES_HOME         where Junees keeps settings and sessions (default ~/.junees;
                      with neither it nor HOME set, sessions are not saved)
  JUNEES_APPROVAL     [approval] which tool calls the conversation mode asks about
                      first: off, risky (default: risky commands, writes outside
                      the folder) or all (every bash/write_file/edit_file)
  JUNEES_NOTIFY_WEBHOOK       [notify_webhook] Discord webhook URL `task run` posts to
  JUNEES_NOTIFY_EACH_SESSION  [notify_each_session] 1 to also post after every session
  JUNEES_TASK_MAX_TOKENS  [task_max_tokens] default --max-tokens of task run
  JUNEES_TASK_MAX_TIME    [task_max_time] default --max-time of task run
  JUNEES_TASK_SKILL_MANAGE  [task_skill_manage] 1 lets task sessions change skills
                            (they only read them by default)
  JUNEES_BACKEND       [backend] what runs the conversation and tasks (api or claude-code)
  JUNEES_CLAUDE_BIN    [claude_bin] the claude program of the claude-code backend
  JUNEES_CLAUDE_MODEL  [claude_model] model passed to claude with --model
  JUNEES_DISCORD_TOKEN          [discord_token] bot token of `junees gateway`
  JUNEES_DISCORD_ALLOWED_USERS  [discord_allowed_users] Discord user ids the bot answers
                                (comma list; the gateway refuses to start without it)
  JUNEES_DISCORD_CHANNELS       [discord_channels] channel/thread ids answered without a
                                mention
  JUNEES_DISCORD_PARALLEL       [discord_parallel] channels running a turn at once (default 2)
  JUNEES_DISCORD_APPROVAL_TIMEOUT  [discord_approval_timeout] wait for an approval
                                answer before declining (default 5m)
  (also JUNEES_THINKING_BUDGET, JUNEES_STREAM, JUNEES_DENY_COMMANDS,
  JUNEES_ALLOW_DANGEROUS, JUNEES_HIDDEN_ENV: see README)
";

pub const REPL_HELP: &str = "\
/task <goal>       plan a long task in this git repository and work on it
/task              keep working on the task
/status            show where the task stands
/sessions          list recent sessions and pick one to continue
/new               start a new conversation (also /reset)
/model [<name>]    show the model, or switch to another for this session
/backend [api|claude-code]
                   show what runs the conversation and /task, or switch both
                   for this session (a switch starts a new conversation)
/compact           replace the earlier conversation with a summary (api backend;
                   Claude Code compacts its own)
/context           show the context size and its limit (api backend)
/usage             show token usage of this session
/memory            show what the persistent memory holds
/memory clear user|notes|project
                   empty one memory (asks first)
/skills            list the skills ($JUNEES_HOME/skills)
/skill <name>      show a skill
/mcp               show the MCP servers ($JUNEES_HOME/mcp.json) and their tools
/exit              quit (also /quit, Ctrl-D, or Ctrl-C twice at the prompt)
Ctrl-C during a turn cancels the turn and stops a running tool; at an
approval question it skips the call and cancels the turn. During /task it
stops the task like Ctrl-C in `junees task`.";

#[derive(Debug, PartialEq, Eq)]
pub enum Command {
    Run(RunArgs),
    Task(TaskCommand),
    /// `junees setup`: ask the setup questions again.
    Setup,
    /// `junees gateway` (or `junees discord`): run as a Discord bot.
    Gateway,
    Help,
    Version,
}

/// How long a `task run` goes on, and on what.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TaskRun {
    pub max_sessions: usize,
    /// Unset parts of the budget come from the settings.
    pub max_tokens: Option<u64>,
    pub max_time: Option<Duration>,
    pub backend: Option<Backend>,
}

impl Default for TaskRun {
    fn default() -> Self {
        Self {
            max_sessions: DEFAULT_MAX_SESSIONS,
            max_tokens: None,
            max_time: None,
            backend: None,
        }
    }
}

/// `junees task ...`.
#[derive(Debug, PartialEq, Eq)]
pub enum TaskCommand {
    /// Plan a new task for this goal.
    Init {
        goal: String,
        backend: Option<Backend>,
    },
    /// Work on the task.
    Run(TaskRun),
    /// Plan a new task for this goal, then work on it.
    Start { goal: String, run: TaskRun },
    /// Show where the task stands, as text or JSON.
    Status { json: bool },
}

const TASK_USAGE: &str = "usage: junees task \"<goal>\" | junees task | junees status; \
                          advanced: junees task init [--backend api|claude-code] \"<goal>\" | \
                          junees task [run] [--backend api|claude-code] [--max-sessions N] \
                          [--max-tokens N] [--max-time 2h] | \
                          junees task status [--json]";

impl TaskCommand {
    /// The backend asked for on the command line, if any.
    pub fn backend(&self) -> Option<Backend> {
        match self {
            Self::Init { backend, .. } => *backend,
            Self::Run(run) | Self::Start { run, .. } => run.backend,
            Self::Status { .. } => None,
        }
    }
}

fn parse_backend(value: Option<String>) -> Result<Backend> {
    let value = value.context("--backend needs api or claude-code")?;
    Backend::parse(&value).context("--backend")
}

/// What to do when not printing help or the version.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct RunArgs {
    /// Run this one prompt instead of starting the interactive mode.
    pub prompt: Option<String>,
    pub session: SessionMode,
}

#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub enum SessionMode {
    /// Start a new saved session.
    #[default]
    New,
    /// Continue the latest session of the working directory.
    Continue,
    /// Continue the session with this id or unique id prefix.
    Resume(String),
    /// Save nothing.
    Off,
}

/// Parses the arguments after the program name. A first argument that is
/// no command (`task`, `status`, `setup`) and no option is the prompt, as
/// with `-p`.
pub fn parse_args(args: impl IntoIterator<Item = String>) -> Result<Command> {
    let mut args = args.into_iter().peekable();
    match args.peek().map(String::as_str) {
        Some("task") => {
            args.next();
            return parse_task(args);
        }
        Some("status") => {
            args.next();
            return parse_status(args);
        }
        Some("setup") => {
            args.next();
            return match args.next().as_deref() {
                None => Ok(Command::Setup),
                Some("-h" | "--help") => Ok(Command::Help),
                Some(other) => bail!("setup takes no arguments (got `{other}`)"),
            };
        }
        Some(name @ ("gateway" | "discord")) => {
            let name = name.to_string();
            args.next();
            return match args.next().as_deref() {
                None => Ok(Command::Gateway),
                Some("-h" | "--help") => Ok(Command::Help),
                Some(other) => bail!("{name} takes no arguments (got `{other}`)"),
            };
        }
        _ => {}
    }
    let mut run = RunArgs::default();
    let mut set_session = |mode: SessionMode| {
        if run.session != SessionMode::New && run.session != mode {
            bail!("-c/--continue, --resume and --no-session cannot be combined");
        }
        run.session = mode;
        Ok(())
    };
    let mut prompt = None;
    let mut set_prompt = |text: String| {
        if prompt.is_some() {
            bail!("give one prompt; quote it if it has spaces (\"{text}\" is extra)");
        }
        prompt = Some(text);
        Ok(())
    };
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "-h" | "--help" => return Ok(Command::Help),
            "-V" | "--version" => return Ok(Command::Version),
            "-p" | "--prompt" => set_prompt(args.next().context("-p/--prompt needs a value")?)?,
            "-c" | "--continue" => set_session(SessionMode::Continue)?,
            "--resume" => {
                let id = args.next().context("--resume needs a session id")?;
                set_session(SessionMode::Resume(id))?;
            }
            "--no-session" => set_session(SessionMode::Off)?,
            // Everything after it is the prompt, even if it starts with -.
            "--" => {
                for text in args.by_ref() {
                    set_prompt(text)?;
                }
            }
            other if other.starts_with('-') && other != "-" => {
                bail!(
                    "unknown argument `{other}` (see --help; a prompt that starts with - goes after --)"
                )
            }
            _ => set_prompt(arg)?,
        }
    }
    run.prompt = prompt;
    Ok(Command::Run(run))
}

fn parse_status(args: impl Iterator<Item = String>) -> Result<Command> {
    let mut json = false;
    for arg in args {
        match arg.as_str() {
            "-h" | "--help" => return Ok(Command::Help),
            "--json" => json = true,
            other => bail!("unknown argument `{other}` for status ({TASK_USAGE})"),
        }
    }
    Ok(Command::Task(TaskCommand::Status { json }))
}

fn parse_task(mut args: std::iter::Peekable<impl Iterator<Item = String>>) -> Result<Command> {
    let command = match args.peek().map(String::as_str) {
        Some("-h" | "--help") => return Ok(Command::Help),
        Some("init") => {
            args.next();
            let mut goal = None;
            let mut backend = None;
            while let Some(arg) = args.next() {
                match arg.as_str() {
                    "-h" | "--help" => return Ok(Command::Help),
                    "--backend" => backend = Some(parse_backend(args.next())?),
                    _ if goal.is_some() => {
                        bail!("task init takes one goal; quote it (\"{arg}\" is extra)")
                    }
                    _ => goal = Some(arg),
                }
            }
            let goal = goal.filter(|goal| !goal.trim().is_empty());
            TaskCommand::Init {
                goal: goal.context(TASK_USAGE)?,
                backend,
            }
        }
        Some("run") => {
            args.next();
            match parse_task_run(args, false)? {
                Some((run, _)) => TaskCommand::Run(run),
                None => return Ok(Command::Help),
            }
        }
        Some("status") => {
            args.next();
            return parse_status(args);
        }
        // `junees task` keeps working, `junees task "<goal>"` starts.
        _ => match parse_task_run(args, true)? {
            Some((run, None)) => TaskCommand::Run(run),
            Some((run, Some(goal))) => TaskCommand::Start { goal, run },
            None => return Ok(Command::Help),
        },
    };
    Ok(Command::Task(command))
}

/// The options of `task run`, and the goal when `with_goal`; `None` for
/// `--help`.
fn parse_task_run(
    mut args: impl Iterator<Item = String>,
    with_goal: bool,
) -> Result<Option<(TaskRun, Option<String>)>> {
    let mut run = TaskRun::default();
    let mut goal: Option<String> = None;
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "-h" | "--help" => return Ok(None),
            "--backend" => run.backend = Some(parse_backend(args.next())?),
            "--max-sessions" => {
                let value = args.next().context("--max-sessions needs a number")?;
                run.max_sessions =
                    value
                        .parse()
                        .ok()
                        .filter(|&n: &usize| n > 0)
                        .with_context(|| {
                            format!("--max-sessions must be a positive integer, got `{value}`")
                        })?;
            }
            "--max-tokens" => {
                let value = args.next().context("--max-tokens needs a number")?;
                run.max_tokens = Some(positive(&value).with_context(|| {
                    format!("--max-tokens must be a positive integer, got `{value}`")
                })?);
            }
            "--max-time" => {
                let value = args
                    .next()
                    .context("--max-time needs a duration such as 2h")?;
                run.max_time = Some(parse_duration(&value).context("--max-time")?);
            }
            other if !with_goal || other.starts_with('-') => {
                bail!("unknown argument `{other}` for task run ({TASK_USAGE})")
            }
            _ if goal.is_some() => bail!("give the task one goal; quote it (\"{arg}\" is extra)"),
            _ if arg.trim().is_empty() => bail!("the goal is empty ({TASK_USAGE})"),
            _ => goal = Some(arg),
        }
    }
    Ok(Some((run, goal)))
}

/// A whole number above 0.
pub fn positive(text: &str) -> Option<u64> {
    text.trim().parse().ok().filter(|&n| n > 0)
}

/// One `/sessions` line: id, last write (UTC), backend and channel tags,
/// first input.
pub fn session_line(summary: &Summary, current: bool) -> String {
    let marker = if current { '*' } else { ' ' };
    let first = if summary.first_input.trim().is_empty() {
        "(empty)".to_string()
    } else {
        preview(&summary.first_input, SESSION_PREVIEW_CHARS)
    };
    let mut tags = String::new();
    if let Some(backend) = &summary.backend {
        tags.push_str(&format!("[{backend}] "));
    }
    if let Some(source) = &summary.source {
        let channel = summary.channel.as_deref().unwrap_or("?");
        tags.push_str(&format!("[{source} {channel}] "));
    }
    format!(
        "{marker} {}  {} UTC  {tags}{first}",
        summary.id,
        format_utc(summary.updated),
    )
}

const SESSION_PREVIEW_CHARS: usize = 60;

/// One-line preview: whitespace collapsed, cut to `max_chars` characters.
pub fn preview(text: &str, max_chars: usize) -> String {
    let flat = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if flat.chars().count() <= max_chars {
        return flat;
    }
    let cut: String = flat.chars().take(max_chars.saturating_sub(3)).collect();
    format!("{cut}...")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(args: &[&str]) -> Result<Command> {
        parse_args(args.iter().map(|arg| arg.to_string()))
    }

    fn run(prompt: Option<&str>, session: SessionMode) -> Command {
        Command::Run(RunArgs {
            prompt: prompt.map(Into::into),
            session,
        })
    }

    #[test]
    fn parses_commands() {
        assert_eq!(parse(&[]).unwrap(), run(None, SessionMode::New));
        assert_eq!(
            parse(&["-p", "hello"]).unwrap(),
            run(Some("hello"), SessionMode::New)
        );
        assert_eq!(
            parse(&["--prompt", "hi"]).unwrap(),
            run(Some("hi"), SessionMode::New)
        );
        assert_eq!(parse(&["--help"]).unwrap(), Command::Help);
        assert_eq!(parse(&["-V"]).unwrap(), Command::Version);
        assert_eq!(parse(&["setup"]).unwrap(), Command::Setup);
        assert!(parse(&["setup", "now"]).is_err());
        assert_eq!(parse(&["gateway"]).unwrap(), Command::Gateway);
        assert_eq!(parse(&["discord"]).unwrap(), Command::Gateway);
        assert_eq!(parse(&["gateway", "--help"]).unwrap(), Command::Help);
        assert!(parse(&["discord", "now"]).is_err());
    }

    #[test]
    fn a_first_word_that_is_no_command_is_the_prompt() {
        assert_eq!(
            parse(&["이 폴더에 뭐가 있어?"]).unwrap(),
            run(Some("이 폴더에 뭐가 있어?"), SessionMode::New)
        );
        assert_eq!(
            parse(&["-c", "계속"]).unwrap(),
            run(Some("계속"), SessionMode::Continue)
        );
        assert_eq!(
            parse(&["explain", "--no-session"]).unwrap(),
            run(Some("explain"), SessionMode::Off)
        );
        assert_eq!(
            parse(&["--", "-5 is negative?"]).unwrap(),
            run(Some("-5 is negative?"), SessionMode::New)
        );
        assert_eq!(parse(&["-"]).unwrap(), run(Some("-"), SessionMode::New));
        // A command word is only a prompt through -p (or after another word).
        assert_eq!(
            parse(&["-p", "status"]).unwrap(),
            run(Some("status"), SessionMode::New)
        );
        assert_eq!(
            parse(&["-c", "setup"]).unwrap(),
            run(Some("setup"), SessionMode::Continue)
        );
        for bad in [&["one", "two"][..], &["-p", "a", "b"], &["a", "-p", "b"]] {
            let err = parse(bad).unwrap_err();
            assert!(
                err.to_string().starts_with("give one prompt"),
                "{bad:?}: {err}"
            );
        }
        let err = parse(&["-x"]).unwrap_err();
        assert!(err.to_string().contains("goes after --"), "{err}");
    }

    #[test]
    fn parses_session_flags() {
        assert_eq!(parse(&["-c"]).unwrap(), run(None, SessionMode::Continue));
        assert_eq!(
            parse(&["--continue"]).unwrap(),
            run(None, SessionMode::Continue)
        );
        assert_eq!(
            parse(&["--resume", "2026"]).unwrap(),
            run(None, SessionMode::Resume("2026".into()))
        );
        assert_eq!(
            parse(&["--no-session"]).unwrap(),
            run(None, SessionMode::Off)
        );
        assert_eq!(
            parse(&["-c", "-p", "계속"]).unwrap(),
            run(Some("계속"), SessionMode::Continue)
        );
        assert_eq!(
            parse(&["-p", "go", "--resume", "abc"]).unwrap(),
            run(Some("go"), SessionMode::Resume("abc".into()))
        );
        assert_eq!(
            parse(&["--no-session", "-p", "x"]).unwrap(),
            run(Some("x"), SessionMode::Off)
        );
        assert_eq!(
            parse(&["-c", "--continue"]).unwrap(),
            run(None, SessionMode::Continue)
        );
    }

    #[test]
    fn rejects_bad_args() {
        assert!(parse(&["-p"]).is_err());
        assert!(parse(&["--wat"]).is_err());
        assert!(parse(&["--resume"]).is_err());
        for bad in [
            &["-c", "--no-session"][..],
            &["--resume", "a", "-c"],
            &["--no-session", "--resume", "a"],
            &["--resume", "a", "--resume", "b"],
        ] {
            let err = parse(bad).unwrap_err();
            assert!(err.to_string().contains("cannot be combined"), "{bad:?}");
        }
    }

    #[test]
    fn parses_the_task_backend() {
        let init = parse(&["task", "init", "--backend", "claude-code", "goal"]).unwrap();
        assert_eq!(
            init,
            Command::Task(TaskCommand::Init {
                goal: "goal".into(),
                backend: Some(Backend::ClaudeCode),
            })
        );
        let Command::Task(run) =
            parse(&["task", "run", "--max-sessions", "2", "--backend", "api"]).unwrap()
        else {
            panic!("not a task command");
        };
        assert_eq!(run.backend(), Some(Backend::Api));
        let Command::Task(start) = parse(&["task", "--backend", "claude-code", "goal"]).unwrap()
        else {
            panic!("not a task command");
        };
        assert_eq!(start.backend(), Some(Backend::ClaudeCode));
        let Command::Task(init) =
            parse(&["task", "init", "goal", "--backend", "claude-code"]).unwrap()
        else {
            panic!("not a task command");
        };
        assert_eq!(init.backend(), Some(Backend::ClaudeCode));
        let err = parse(&["task", "run", "--backend", "codex"]).unwrap_err();
        assert_eq!(
            format!("{err:#}"),
            "--backend: unknown backend `codex`; use `api` or `claude-code`"
        );
        assert!(parse(&["task", "init", "goal", "--backend"]).is_err());
    }

    #[test]
    fn parses_task_commands() {
        assert_eq!(
            parse(&["task", "init", "웹 서버를 만들어"]).unwrap(),
            Command::Task(TaskCommand::Init {
                goal: "웹 서버를 만들어".into(),
                backend: None,
            })
        );
        assert_eq!(
            parse(&["task", "run"]).unwrap(),
            Command::Task(TaskCommand::Run(TaskRun::default()))
        );
        assert_eq!(
            parse(&["task", "run", "--max-sessions", "3"]).unwrap(),
            Command::Task(TaskCommand::Run(TaskRun {
                max_sessions: 3,
                ..TaskRun::default()
            }))
        );
        assert_eq!(
            parse(&[
                "task",
                "run",
                "--max-time",
                "1h30m",
                "--max-tokens",
                "2000000"
            ])
            .unwrap(),
            Command::Task(TaskCommand::Run(TaskRun {
                max_tokens: Some(2_000_000),
                max_time: Some(Duration::from_secs(5400)),
                ..TaskRun::default()
            }))
        );
        assert_eq!(
            parse(&["task", "status"]).unwrap(),
            Command::Task(TaskCommand::Status { json: false })
        );
        assert_eq!(
            parse(&["task", "status", "--json"]).unwrap(),
            Command::Task(TaskCommand::Status { json: true })
        );
        assert_eq!(parse(&["task", "--help"]).unwrap(), Command::Help);
        assert_eq!(parse(&["task", "run", "--help"]).unwrap(), Command::Help);
        assert_eq!(parse(&["task", "goal", "-h"]).unwrap(), Command::Help);
        // Only a leading `task` is the subcommand.
        assert_eq!(
            parse(&["-p", "task"]).unwrap(),
            run(Some("task"), SessionMode::New)
        );
    }

    #[test]
    fn parses_the_short_task_commands() {
        // `junees task` keeps working on the task.
        assert_eq!(
            parse(&["task"]).unwrap(),
            Command::Task(TaskCommand::Run(TaskRun::default()))
        );
        assert_eq!(
            parse(&["task", "--max-time", "2h"]).unwrap(),
            Command::Task(TaskCommand::Run(TaskRun {
                max_time: Some(Duration::from_secs(7200)),
                ..TaskRun::default()
            }))
        );
        // `junees task "<goal>"` plans it and works on it.
        assert_eq!(
            parse(&["task", "할 일 앱을 만들어"]).unwrap(),
            Command::Task(TaskCommand::Start {
                goal: "할 일 앱을 만들어".into(),
                run: TaskRun::default(),
            })
        );
        assert_eq!(
            parse(&["task", "--max-sessions", "4", "go", "--max-tokens", "9"]).unwrap(),
            Command::Task(TaskCommand::Start {
                goal: "go".into(),
                run: TaskRun {
                    max_sessions: 4,
                    max_tokens: Some(9),
                    ..TaskRun::default()
                },
            })
        );
        // `junees status` is `junees task status`.
        assert_eq!(
            parse(&["status"]).unwrap(),
            Command::Task(TaskCommand::Status { json: false })
        );
        assert_eq!(
            parse(&["status", "--json"]).unwrap(),
            Command::Task(TaskCommand::Status { json: true })
        );
        assert!(parse(&["status", "now"]).is_err());
    }

    #[test]
    fn rejects_bad_task_args() {
        for bad in [
            &["task", "a", "b"][..],
            &["task", " "],
            &["task", "--max-wat", "goal"],
            &["task", "init"],
            &["task", "init", " "],
            &["task", "init", "a", "b"],
            &["task", "run", "--max-sessions"],
            &["task", "run", "--max-sessions", "0"],
            &["task", "run", "--max-sessions", "many"],
            &["task", "run", "-p", "x"],
            &["task", "status", "--yaml"],
            &["task", "run", "--max-tokens"],
            &["task", "run", "--max-tokens", "0"],
            &["task", "run", "--max-tokens", "1e6"],
            &["task", "run", "--max-time"],
            &["task", "run", "--max-time", "90"],
            &["task", "run", "--max-time", "0m"],
        ] {
            assert!(parse(bad).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn formats_session_lines() {
        let summary = Summary {
            id: "20261109-061704-3fa2".into(),
            updated: 1_794_205_024,
            cwd: "/work".into(),
            first_input: "이 폴더에\n뭐가 있어? ".repeat(10),
            backend: None,
            source: None,
            channel: None,
        };
        let line = session_line(&summary, true);
        assert!(
            line.starts_with("* 20261109-061704-3fa2  2026-11-09 06:17 UTC  이 폴더에 뭐가"),
            "{line}"
        );
        assert!(line.ends_with("..."));
        let empty = Summary {
            first_input: String::new(),
            ..summary.clone()
        };
        assert!(session_line(&empty, false).ends_with("UTC  (empty)"));
        // A conversation Claude Code runs says so.
        let claude = Summary {
            first_input: "hi".into(),
            backend: Some("claude-code".into()),
            ..summary
        };
        assert!(session_line(&claude, false).ends_with("UTC  [claude-code] hi"));
        // So does a conversation of a Discord channel.
        let channel = Summary {
            source: Some("discord".into()),
            channel: Some("505".into()),
            ..claude
        };
        assert!(session_line(&channel, false).ends_with("UTC  [claude-code] [discord 505] hi"));
    }

    #[test]
    fn preview_flattens_and_cuts() {
        assert_eq!(preview("a\n  b\tc", 10), "a b c");
        assert_eq!(preview("가나다라마바", 5), "가나...");
    }
}
