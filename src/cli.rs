//! Command-line parsing and terminal output helpers.

use anyhow::{Context, Result, bail};

use crate::session::{Summary, format_utc};

pub const HELP: &str = "\
junees - an AI agent in your terminal

Usage:
  junees [OPTIONS]              start an interactive session
  junees [OPTIONS] -p <PROMPT>  run one prompt and exit

Options:
  -p, --prompt <PROMPT>  run a single prompt non-interactively
  -c, --continue         continue the latest session started in this folder
      --resume <ID>      continue the session with this id (a unique prefix is enough)
      --no-session       do not save this session
  -h, --help             print this help
  -V, --version          print the version

Sessions are saved as they happen to $JUNEES_HOME/sessions/<id>.jsonl.

Environment:
  ANTHROPIC_API_KEY   required
  ANTHROPIC_BASE_URL  API base URL (default https://api.anthropic.com)
  JUNEES_MODEL        model id (default claude-sonnet-4-6)
  JUNEES_MAX_TOKENS   max output tokens per model call (default 8192)
  JUNEES_MAX_TURNS    max model calls per prompt (default 50)
  JUNEES_HOME         where Junees keeps sessions (default ~/.junees)
";

pub const REPL_HELP: &str = "\
/sessions  list recent sessions
/reset     start a new session
/usage     show token usage of this session
/exit      quit (also /quit or Ctrl-D)";

#[derive(Debug, PartialEq, Eq)]
pub enum Command {
    Run(RunArgs),
    Help,
    Version,
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
    let mut args = args.into_iter();
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
