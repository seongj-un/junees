//! Command-line parsing and terminal output helpers.

use anyhow::{Context, Result, bail};

pub const HELP: &str = "\
junees - an AI agent in your terminal

Usage:
  junees              start an interactive session
  junees -p <PROMPT>  run one prompt and exit

Options:
  -p, --prompt <PROMPT>  run a single prompt non-interactively
  -h, --help             print this help
  -V, --version          print the version

Environment:
  ANTHROPIC_API_KEY   required
  ANTHROPIC_BASE_URL  API base URL (default https://api.anthropic.com)
  JUNEES_MODEL        model id (default claude-sonnet-4-6)
  JUNEES_MAX_TOKENS   max output tokens per model call (default 8192)
  JUNEES_MAX_TURNS    max model calls per prompt (default 50)
";

pub const REPL_HELP: &str = "\
/reset  forget the conversation
/usage  show token usage
/exit   quit (also /quit or Ctrl-D)";

#[derive(Debug, PartialEq, Eq)]
pub enum Command {
    Repl,
    Prompt(String),
    Help,
    Version,
}

pub fn parse_args(args: impl IntoIterator<Item = String>) -> Result<Command> {
    let mut args = args.into_iter();
    let mut command = Command::Repl;
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "-h" | "--help" => return Ok(Command::Help),
            "-V" | "--version" => return Ok(Command::Version),
            "-p" | "--prompt" => {
                let prompt = args.next().context("-p/--prompt needs a value")?;
                command = Command::Prompt(prompt);
            }
            other => bail!("unknown argument `{other}` (see --help)"),
        }
    }
    Ok(command)
}

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

    #[test]
    fn parses_commands() {
        assert_eq!(parse(&[]).unwrap(), Command::Repl);
        assert_eq!(
            parse(&["-p", "hello"]).unwrap(),
            Command::Prompt("hello".into())
        );
        assert_eq!(
            parse(&["--prompt", "hi"]).unwrap(),
            Command::Prompt("hi".into())
        );
        assert_eq!(parse(&["--help"]).unwrap(), Command::Help);
        assert_eq!(parse(&["-V"]).unwrap(), Command::Version);
    }

    #[test]
    fn rejects_bad_args() {
        assert!(parse(&["-p"]).is_err());
        assert!(parse(&["--wat"]).is_err());
    }

    #[test]
    fn preview_flattens_and_cuts() {
        assert_eq!(preview("a\n  b\tc", 10), "a b c");
        assert_eq!(preview("가나다라마바", 5), "가나...");
    }
}
