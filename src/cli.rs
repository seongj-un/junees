//! Command-line parsing and terminal output helpers.

use std::time::Duration;

use anyhow::{Context, Result, bail};

use crate::session::{Summary, format_utc};
use crate::task::DEFAULT_MAX_SESSIONS;
use crate::task::budget::parse_duration;
use crate::task::claude_code::Backend;

pub const HELP: &str = "\
junees - an AI agent in your terminal

Usage:
  junees [OPTIONS]              start an interactive session
  junees [OPTIONS] -p <PROMPT>  run one prompt and exit
  junees task init [--backend <B>] \"<GOAL>\"
                                plan a long-running task in this git repository
  junees task run [--backend <B>] [--max-sessions <N>] [--max-tokens <N>] [--max-time <DURATION>]
                                work on the task, one feature per session
  junees task status [--json]   show where the task stands (read-only)

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

--backend picks what runs the task sessions: `api` (default), Junees's own
agent on ANTHROPIC_API_KEY, or `claude-code`, the official Claude Code CLI
signed in with your own Claude subscription. Junees only starts `claude`;
sign in yourself first (run `claude`, then /login). It needs no
ANTHROPIC_API_KEY, and removes the API variables from claude's environment.

Environment:
  ANTHROPIC_API_KEY   required, except for task init/run with the claude-code backend
  ANTHROPIC_BASE_URL  API base URL (default https://api.anthropic.com)
  JUNEES_MODEL        model id (default claude-sonnet-4-6)
  JUNEES_MAX_TOKENS   max output tokens per model call (default 8192)
  JUNEES_MAX_TURNS    max model calls per prompt (default 50)
  JUNEES_CONTEXT_WINDOW     model context window in tokens (default 200000)
  JUNEES_COMPACT_THRESHOLD  share of the window at which old tool output is
                            cleared and the conversation compacted (default 0.8)
  JUNEES_HOME         where Junees keeps sessions (default ~/.junees;
                      with neither it nor HOME set, sessions are not saved)
  JUNEES_APPROVAL     which tool calls the conversation mode asks about first:
                      off, risky (default: risky commands, writes outside
                      the folder) or all (every bash/write_file/edit_file)
  JUNEES_NOTIFY_WEBHOOK       Discord webhook URL `task run` posts to when it stops
  JUNEES_NOTIFY_EACH_SESSION  1 to also post a line after every task session
  JUNEES_TASK_MAX_TOKENS  default --max-tokens of task run
  JUNEES_TASK_MAX_TIME    default --max-time of task run
  JUNEES_BACKEND       default --backend of task init/run (api or claude-code)
  JUNEES_CLAUDE_BIN    the claude program of the claude-code backend (default: claude)
  JUNEES_CLAUDE_MODEL  model passed to claude with --model (default: Claude Code's own)
";

pub const REPL_HELP: &str = "\
/sessions  list recent sessions
/reset     start a new session
/usage     show token usage of this session
/context   show the context size and its limit
/compact   replace the earlier conversation with a summary
/exit      quit (also /quit, Ctrl-D, or Ctrl-C twice at the prompt)
Ctrl-C during a turn cancels the turn and stops a running tool; at an
approval question it skips the call and cancels the turn.";

#[derive(Debug, PartialEq, Eq)]
pub enum Command {
    Run(RunArgs),
    Task(TaskCommand),
    Help,
    Version,
}

/// `junees task ...`.
#[derive(Debug, PartialEq, Eq)]
pub enum TaskCommand {
    /// Plan a new task for this goal.
    Init {
        goal: String,
        backend: Option<Backend>,
    },
    /// Work on the task for at most this many sessions, within the budget
    /// (unset parts come from the environment).
    Run {
        max_sessions: usize,
        max_tokens: Option<u64>,
        max_time: Option<Duration>,
        backend: Option<Backend>,
    },
    /// Show where the task stands, as text or JSON.
    Status { json: bool },
}

const TASK_USAGE: &str = "usage: junees task init [--backend api|claude-code] \"<goal>\" | \
                          junees task run [--backend api|claude-code] [--max-sessions N] \
                          [--max-tokens N] [--max-time 2h] | \
                          junees task status [--json]";

impl TaskCommand {
    /// The backend asked for on the command line, if any.
    pub fn backend(&self) -> Option<Backend> {
        match self {
            Self::Init { backend, .. } | Self::Run { backend, .. } => *backend,
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

pub fn parse_args(args: impl IntoIterator<Item = String>) -> Result<Command> {
    let mut args = args.into_iter().peekable();
    if args.peek().is_some_and(|arg| arg == "task") {
        args.next();
        return parse_task(args);
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
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "-h" | "--help" => return Ok(Command::Help),
            "-V" | "--version" => return Ok(Command::Version),
            "-p" | "--prompt" => {
                prompt = Some(args.next().context("-p/--prompt needs a value")?);
            }
            "-c" | "--continue" => set_session(SessionMode::Continue)?,
            "--resume" => {
                let id = args.next().context("--resume needs a session id")?;
                set_session(SessionMode::Resume(id))?;
            }
            "--no-session" => set_session(SessionMode::Off)?,
            other => bail!("unknown argument `{other}` (see --help)"),
        }
    }
    run.prompt = prompt;
    Ok(Command::Run(run))
}

fn parse_task(mut args: impl Iterator<Item = String>) -> Result<Command> {
    let command = match args.next().as_deref() {
        Some("-h" | "--help") => return Ok(Command::Help),
        Some("init") => {
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
            let mut max_sessions = DEFAULT_MAX_SESSIONS;
            let mut max_tokens = None;
            let mut max_time = None;
            let mut backend = None;
            while let Some(arg) = args.next() {
                match arg.as_str() {
                    "-h" | "--help" => return Ok(Command::Help),
                    "--backend" => backend = Some(parse_backend(args.next())?),
                    "--max-sessions" => {
                        let value = args.next().context("--max-sessions needs a number")?;
                        max_sessions =
                            value
                                .parse()
                                .ok()
                                .filter(|&n: &usize| n > 0)
                                .with_context(|| {
                                    format!(
                                        "--max-sessions must be a positive integer, got `{value}`"
                                    )
                                })?;
                    }
                    "--max-tokens" => {
                        let value = args.next().context("--max-tokens needs a number")?;
                        max_tokens = Some(positive(&value).with_context(|| {
                            format!("--max-tokens must be a positive integer, got `{value}`")
                        })?);
                    }
                    "--max-time" => {
                        let value = args
                            .next()
                            .context("--max-time needs a duration such as 2h")?;
                        max_time = Some(parse_duration(&value).context("--max-time")?);
                    }
                    other => bail!("unknown argument `{other}` for task run ({TASK_USAGE})"),
                }
            }
            TaskCommand::Run {
                max_sessions,
                max_tokens,
                max_time,
                backend,
            }
        }
        Some("status") => {
            let mut json = false;
            for arg in args {
                match arg.as_str() {
                    "-h" | "--help" => return Ok(Command::Help),
                    "--json" => json = true,
                    other => bail!("unknown argument `{other}` for task status ({TASK_USAGE})"),
                }
            }
            TaskCommand::Status { json }
        }
        _ => bail!(TASK_USAGE),
    };
    Ok(Command::Task(command))
}

/// A whole number above 0.
pub fn positive(text: &str) -> Option<u64> {
    text.trim().parse().ok().filter(|&n| n > 0)
}

/// One `/sessions` line: id, last write (UTC), first input.
pub fn session_line(summary: &Summary, current: bool) -> String {
    let marker = if current { '*' } else { ' ' };
    let first = if summary.first_input.trim().is_empty() {
        "(empty)".to_string()
    } else {
        preview(&summary.first_input, SESSION_PREVIEW_CHARS)
    };
    format!(
        "{marker} {}  {} UTC  {first}",
        summary.id,
        format_utc(summary.updated)
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
            Command::Task(TaskCommand::Run {
                max_sessions: DEFAULT_MAX_SESSIONS,
                max_tokens: None,
                max_time: None,
                backend: None,
            })
        );
        assert_eq!(
            parse(&["task", "run", "--max-sessions", "3"]).unwrap(),
            Command::Task(TaskCommand::Run {
                max_sessions: 3,
                max_tokens: None,
                max_time: None,
                backend: None,
            })
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
            Command::Task(TaskCommand::Run {
                max_sessions: DEFAULT_MAX_SESSIONS,
                max_tokens: Some(2_000_000),
                max_time: Some(Duration::from_secs(5400)),
                backend: None,
            })
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
        // Only a leading `task` is the subcommand.
        assert_eq!(
            parse(&["-p", "task"]).unwrap(),
            run(Some("task"), SessionMode::New)
        );
    }

    #[test]
    fn rejects_bad_task_args() {
        for bad in [
            &["task"][..],
            &["task", "go"],
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
        };
        let line = session_line(&summary, true);
        assert!(
            line.starts_with("* 20261109-061704-3fa2  2026-11-09 06:17 UTC  이 폴더에 뭐가"),
            "{line}"
        );
        assert!(line.ends_with("..."));
        let empty = Summary {
            first_input: String::new(),
            ..summary
        };
        assert!(session_line(&empty, false).ends_with("UTC  (empty)"));
    }

    #[test]
    fn preview_flattens_and_cuts() {
        assert_eq!(preview("a\n  b\tc", 10), "a b c");
        assert_eq!(preview("가나다라마바", 5), "가나...");
    }
}
