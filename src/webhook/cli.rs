//! Parsing `junees webhook ...`.

use std::path::PathBuf;

use anyhow::{Context, Result, bail};

pub const USAGE: &str = "\
junees webhook - HTTP requests (GitHub webhooks and others) that start a run

Usage:
  junees webhook serve [--listen 127.0.0.1:8650]
                                   receive POST /hooks/<name> (keep it running)
  junees webhook add <name> --prompt \"<template>\" [--kind github|generic]
                     [--secret-env <VAR> | --secret-file <path>]
                     [--events pull_request.opened,issue_comment,...]
                     [--dir <folder>] [--deliver discord:<channel id>|webhook|local]
  junees webhook list              the routes and their last runs
  junees webhook remove <name>     (also rm)
  junees webhook pause <name>
  junees webhook resume <name>
  junees webhook test <name> <payload.json> [--event <event>] [--delivery <id>]
                                   run it once now, without the server

A route is POST /hooks/<name>. github routes read X-GitHub-Event,
X-GitHub-Delivery and check X-Hub-Signature-256; generic routes read
X-Junees-Event (default `webhook`), X-Junees-Delivery and check
X-Junees-Signature (sha256=<hex HMAC-SHA256 of the body>). The secret is
read from the environment variable or the private (0600) file named, never
stored; a file must be in $JUNEES_HOME/secrets/, which the model's tools
may not read. A route without a secret is only served on a loopback address,
to JSON requests sent straight to it: Host 127.0.0.1:<port>, localhost:<port>
or [::1]:<port>, Content-Type application/json, no Origin or Sec-Fetch-*
header (browsers) and no X-Forwarded-For, Forwarded, CF-Connecting-IP or
X-Real-IP header (proxies and tunnels).
--events keeps the events that run (`pull_request` or `pull_request.opened`;
default every event); GitHub's ping is answered and does not run.

The prompt template takes {{event}}, {{action}}, {{delivery}}, {{route}},
{{payload}} and {{payload.a.b}} (numbers index arrays). The values are put
in data tags and the prompt says they are data, not instructions.

Each request runs like a scheduled job (see `junees cron --help`): a new
session in the route's folder (default: the current one), no approval
questions, the command policy, the memory and skills read only, up to
webhook_max_time (default 30m) and webhook_max_tokens (default 1000000).
One run per route at a time; up to 20 more wait. The answer goes to
$JUNEES_HOME/webhooks/output/<name>/ and to --deliver (default local).
";

/// `junees webhook ...`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WebhookCommand {
    Serve {
        listen: Option<String>,
    },
    Add(AddArgs),
    List,
    Remove(String),
    Pause(String),
    Resume(String),
    Test {
        name: String,
        payload: PathBuf,
        event: Option<String>,
        delivery: Option<String>,
    },
    /// One run of the server, the request on standard input.
    Run {
        name: String,
        run_id: String,
    },
    Help,
}

/// `junees webhook add`.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct AddArgs {
    pub name: String,
    pub prompt: String,
    pub kind: Option<String>,
    pub secret_env: Option<String>,
    pub secret_file: Option<PathBuf>,
    pub events: Option<String>,
    pub dir: Option<PathBuf>,
    pub deliver: Option<String>,
}

/// Parses the arguments after `webhook`.
pub fn parse(args: impl IntoIterator<Item = String>) -> Result<WebhookCommand> {
    let mut args = args.into_iter();
    let Some(command) = args.next() else {
        return Ok(WebhookCommand::List);
    };
    let rest: Vec<String> = args.collect();
    if rest.iter().any(|arg| arg == "-h" || arg == "--help") {
        return Ok(WebhookCommand::Help);
    }
    let one = |rest: &[String], what: &str| -> Result<String> {
        match rest {
            [name] => Ok(name.clone()),
            [] => bail!("`junees webhook {what}` needs a route name (`junees webhook list`)"),
            _ => bail!("`junees webhook {what}` takes one route name"),
        }
    };
    Ok(match command.as_str() {
        "-h" | "--help" | "help" => WebhookCommand::Help,
        "serve" => serve(rest)?,
        "add" => WebhookCommand::Add(add(rest)?),
        "list" | "ls" => match rest.first() {
            None => WebhookCommand::List,
            Some(other) => bail!("`junees webhook list` takes no arguments (got `{other}`)"),
        },
        "remove" | "rm" => WebhookCommand::Remove(one(&rest, "remove")?),
        "pause" => WebhookCommand::Pause(one(&rest, "pause")?),
        "resume" => WebhookCommand::Resume(one(&rest, "resume")?),
        "test" => test(rest)?,
        "run" => run(rest)?,
        other => {
            bail!("unknown command `junees webhook {other}`; `junees webhook --help` lists them")
        }
    })
}

/// The value of an option: `--name x` or `--name=x`.
fn option_value(
    arg: &str,
    name: &str,
    args: &mut std::vec::IntoIter<String>,
) -> Option<Result<String>> {
    if arg == name {
        return Some(args.next().with_context(|| format!("{name} needs a value")));
    }
    arg.strip_prefix(name)
        .and_then(|rest| rest.strip_prefix('='))
        .map(|value| Ok(value.to_string()))
}

/// `(option, value)` pairs, in the order given.
type Options = Vec<(String, String)>;

/// Splits `rest` into positional words and `(option, value)` pairs of
/// `options`; `--` ends the options.
fn split(rest: Vec<String>, options: &[&str], what: &str) -> Result<(Vec<String>, Options)> {
    let mut positional = Vec::new();
    let mut found = Vec::new();
    let mut args = rest.into_iter();
    let mut options_done = false;
    'next: while let Some(arg) = args.next() {
        if options_done || !arg.starts_with("--") {
            positional.push(arg);
            continue;
        }
        if arg == "--" {
            options_done = true;
            continue;
        }
        for option in options {
            if let Some(value) = option_value(&arg, option, &mut args) {
                found.push((option.to_string(), value?));
                continue 'next;
            }
        }
        bail!("unknown option `{arg}` of `junees webhook {what}`");
    }
    Ok((positional, found))
}

fn serve(rest: Vec<String>) -> Result<WebhookCommand> {
    let (positional, options) = split(rest, &["--listen"], "serve")?;
    if let Some(other) = positional.first() {
        bail!("`junees webhook serve` takes no arguments (got `{other}`)");
    }
    Ok(WebhookCommand::Serve {
        listen: options.into_iter().last().map(|(_, value)| value),
    })
}

fn add(rest: Vec<String>) -> Result<AddArgs> {
    let (positional, options) = split(
        rest,
        &[
            "--prompt",
            "--kind",
            "--secret-env",
            "--secret-file",
            "--events",
            "--dir",
            "--deliver",
        ],
        "add",
    )?;
    let mut args = AddArgs::default();
    let mut prompt = None;
    for (option, value) in options {
        match option.as_str() {
            "--prompt" => prompt = Some(value),
            "--kind" => args.kind = Some(value),
            "--secret-env" => args.secret_env = Some(value),
            "--secret-file" => args.secret_file = Some(PathBuf::from(value)),
            "--events" => args.events = Some(value),
            "--dir" => args.dir = Some(PathBuf::from(value)),
            _ => args.deliver = Some(value),
        }
    }
    args.name = match <[String; 1]>::try_from(positional) {
        Ok([name]) => name,
        Err(_) => bail!(
            "usage: junees webhook add <name> --prompt \"<template>\" [--kind ...] \
             [--secret-env VAR | --secret-file path] [--events ...] [--dir ...] [--deliver ...]"
        ),
    };
    args.prompt = prompt.context("`junees webhook add` needs --prompt \"<template>\"")?;
    if args.secret_env.is_some() && args.secret_file.is_some() {
        bail!("give --secret-env or --secret-file, not both");
    }
    Ok(args)
}

fn test(rest: Vec<String>) -> Result<WebhookCommand> {
    let (positional, options) = split(rest, &["--event", "--delivery"], "test")?;
    let (mut event, mut delivery) = (None, None);
    for (option, value) in options {
        if option == "--event" {
            event = Some(value);
        } else {
            delivery = Some(value);
        }
    }
    match <[String; 2]>::try_from(positional) {
        Ok([name, payload]) => Ok(WebhookCommand::Test {
            name,
            payload: PathBuf::from(payload),
            event,
            delivery,
        }),
        Err(_) => bail!("usage: junees webhook test <name> <payload.json> [--event <event>]"),
    }
}

fn run(rest: Vec<String>) -> Result<WebhookCommand> {
    let (positional, options) = split(rest, &["--run-id"], "run")?;
    let run_id = options
        .into_iter()
        .last()
        .map(|(_, value)| value)
        .context("`junees webhook run` needs --run-id (the server starts it)")?;
    if run_id.is_empty() || run_id.len() > 32 || !run_id.chars().all(|c| c.is_ascii_alphanumeric())
    {
        bail!("--run-id takes letters and digits");
    }
    match <[String; 1]>::try_from(positional) {
        Ok([name]) => Ok(WebhookCommand::Run { name, run_id }),
        Err(_) => bail!("`junees webhook run` takes one route name"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse_words(words: &[&str]) -> Result<WebhookCommand> {
        parse(words.iter().map(|word| word.to_string()))
    }

    #[test]
    fn parses_every_command() {
        assert_eq!(parse_words(&[]).unwrap(), WebhookCommand::List);
        assert_eq!(parse_words(&["ls"]).unwrap(), WebhookCommand::List);
        assert_eq!(
            parse_words(&["serve"]).unwrap(),
            WebhookCommand::Serve { listen: None }
        );
        assert_eq!(
            parse_words(&["serve", "--listen=0.0.0.0:9000"]).unwrap(),
            WebhookCommand::Serve {
                listen: Some("0.0.0.0:9000".into())
            }
        );
        assert_eq!(
            parse_words(&["rm", "pr"]).unwrap(),
            WebhookCommand::Remove("pr".into())
        );
        assert_eq!(
            parse_words(&["pause", "pr"]).unwrap(),
            WebhookCommand::Pause("pr".into())
        );
        assert_eq!(
            parse_words(&["resume", "pr"]).unwrap(),
            WebhookCommand::Resume("pr".into())
        );
        assert_eq!(
            parse_words(&["add", "--help"]).unwrap(),
            WebhookCommand::Help
        );
        assert_eq!(
            parse_words(&[
                "add",
                "pr",
                "--prompt",
                "리뷰 {{payload.number}}",
                "--kind=generic",
                "--secret-env",
                "PR_SECRET",
                "--events",
                "pull_request.opened,issue_comment",
                "--dir",
                "/srv",
                "--deliver",
                "discord:42",
            ])
            .unwrap(),
            WebhookCommand::Add(AddArgs {
                name: "pr".into(),
                prompt: "리뷰 {{payload.number}}".into(),
                kind: Some("generic".into()),
                secret_env: Some("PR_SECRET".into()),
                secret_file: None,
                events: Some("pull_request.opened,issue_comment".into()),
                dir: Some(PathBuf::from("/srv")),
                deliver: Some("discord:42".into()),
            })
        );
        assert_eq!(
            parse_words(&["test", "pr", "p.json", "--event", "push"]).unwrap(),
            WebhookCommand::Test {
                name: "pr".into(),
                payload: PathBuf::from("p.json"),
                event: Some("push".into()),
                delivery: None,
            }
        );
        assert_eq!(
            parse_words(&["run", "pr", "--run-id", "ab12"]).unwrap(),
            WebhookCommand::Run {
                name: "pr".into(),
                run_id: "ab12".into()
            }
        );
    }

    #[test]
    fn wrong_arguments_say_what_is_wrong() {
        for (words, wanted) in [
            (&["add", "pr"][..], "needs --prompt"),
            (&["add", "--prompt", "x"][..], "usage"),
            (&["add", "a", "b", "--prompt", "x"][..], "usage"),
            (
                &[
                    "add",
                    "a",
                    "--prompt",
                    "x",
                    "--secret-env",
                    "A",
                    "--secret-file",
                    "f",
                ][..],
                "not both",
            ),
            (&["add", "a", "--prompt"][..], "needs a value"),
            (&["add", "a", "--color", "x"][..], "unknown option"),
            (&["serve", "now"][..], "no arguments"),
            (&["rm"][..], "needs a route name"),
            (&["pause", "a", "b"][..], "one route name"),
            (&["test", "a"][..], "usage"),
            (&["run", "a"][..], "needs --run-id"),
            (&["run", "a", "--run-id", "../x"][..], "letters and digits"),
            (&["list", "x"][..], "no arguments"),
            (&["start"][..], "unknown command"),
        ] {
            let err = parse_words(words).unwrap_err().to_string();
            assert!(err.contains(wanted), "{words:?}: {err}");
        }
    }
}
