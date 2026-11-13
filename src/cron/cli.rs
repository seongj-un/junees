//! Parsing `junees cron ...`.

use std::path::PathBuf;

use anyhow::{Context, Result, bail};

pub const USAGE: &str = "\
junees cron - prompts that run on a schedule

Usage:
  junees cron add \"<schedule>\" \"<prompt>\" [--name <name>] [--dir <folder>]
                  [--deliver discord:<channel id>|webhook|local] [--allow-home]
  junees cron list               the jobs, their next and last runs
  junees cron remove <id>        (also rm)
  junees cron pause <id>
  junees cron resume <id>
  junees cron allow-home <id>    let the job work in the home folder (resumes it
                                 if a refused run paused it)
  junees cron disallow-home <id> take that back (pauses it if it works there)
  junees cron run <id>           run it now, once, and print the answer
  junees cron daemon             run the jobs when they are due (keep it running)

A job is named by its id, its --name, or a unique start of its id.

Schedules (local times in the `timezone` setting, default the system's):
  every 30m, every 2h, every 1h30m   from now on, at that interval
  in 20m                             once
  daily 09:00, weekdays 09:00        every day, Monday to Friday
  0 9 * * 1-5                        a cron expression (minute hour day month weekday)
  2026-11-10T09:00                   once (also with :SS, Z or +09:00)

Each run is a new session in the job's folder (default: the current one),
which may not be the home folder (or a folder holding it, such as /)
unless --allow-home: the run's tools could change everything there but
the protected paths. A job added before that rule is paused at its first
refused run; `allow-home` marks it and resumes it. Like `junees -p`: no approval questions, the command policy applies, the
memory and skills can be read but not changed. A run stops at
cron_max_time (default 30m) or cron_max_tokens (default 1000000). The
answer goes to $JUNEES_HOME/cron/output/<id>/ and to --deliver (default
local: the file only): a Discord channel through the bot (discord_token),
or the notify_webhook webhook.
";

/// `junees cron ...`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CronCommand {
    Add {
        schedule: String,
        prompt: String,
        name: Option<String>,
        dir: Option<PathBuf>,
        deliver: Option<String>,
        /// `--allow-home`: the job may work in the home folder.
        allow_home: bool,
    },
    List,
    Remove(String),
    Pause(String),
    Resume(String),
    /// `allow-home`: mark the job as allowed to work in the home folder,
    /// and resume it if a refused run paused it.
    AllowHome(String),
    /// `disallow-home`: take that mark back.
    DisallowHome(String),
    /// Run now. The daemon passes the id of the run and its time.
    Run {
        job: String,
        run_id: Option<String>,
        due: Option<i64>,
    },
    Daemon,
    Help,
}

/// Parses the arguments after `cron`.
pub fn parse(args: impl IntoIterator<Item = String>) -> Result<CronCommand> {
    let mut args = args.into_iter();
    let Some(command) = args.next() else {
        return Ok(CronCommand::List);
    };
    let rest: Vec<String> = args.collect();
    if rest.iter().any(|arg| arg == "-h" || arg == "--help") && command != "add" {
        return Ok(CronCommand::Help);
    }
    let job = |rest: &[String], what: &str| -> Result<String> {
        match rest {
            [job] => Ok(job.clone()),
            [] => bail!("`junees cron {what}` needs a job id (`junees cron list` shows them)"),
            _ => bail!("`junees cron {what}` takes one job id"),
        }
    };
    let none = |rest: &[String], what: &str| -> Result<()> {
        match rest {
            [] => Ok(()),
            [other, ..] => bail!("`junees cron {what}` takes no arguments (got `{other}`)"),
        }
    };
    Ok(match command.as_str() {
        "-h" | "--help" | "help" => CronCommand::Help,
        "add" => add(rest)?,
        "list" | "ls" => {
            none(&rest, "list")?;
            CronCommand::List
        }
        "remove" | "rm" => CronCommand::Remove(job(&rest, "remove")?),
        "pause" => CronCommand::Pause(job(&rest, "pause")?),
        "resume" => CronCommand::Resume(job(&rest, "resume")?),
        "allow-home" => CronCommand::AllowHome(job(&rest, "allow-home")?),
        "disallow-home" => CronCommand::DisallowHome(job(&rest, "disallow-home")?),
        "run" => run(rest)?,
        "daemon" => {
            none(&rest, "daemon")?;
            CronCommand::Daemon
        }
        other => bail!("unknown command `junees cron {other}`; `junees cron --help` lists them"),
    })
}

/// The value of an option: `--name x` or `--name=x`.
fn option_value(
    arg: &str,
    names: &[&str],
    args: &mut std::vec::IntoIter<String>,
) -> Option<Result<String>> {
    for name in names {
        if arg == *name {
            return Some(args.next().with_context(|| format!("{name} needs a value")));
        }
        if let Some(value) = arg
            .strip_prefix(name)
            .and_then(|rest| rest.strip_prefix('='))
        {
            return Some(Ok(value.to_string()));
        }
    }
    None
}

fn add(rest: Vec<String>) -> Result<CronCommand> {
    let (mut name, mut dir, mut deliver) = (None, None, None);
    let mut allow_home = false;
    let mut positional = Vec::new();
    let mut args = rest.into_iter();
    let mut options_done = false;
    while let Some(arg) = args.next() {
        if options_done || !arg.starts_with("--") {
            positional.push(arg);
            continue;
        }
        if arg == "--" {
            options_done = true;
        } else if let Some(value) = option_value(&arg, &["--name"], &mut args) {
            name = Some(value?);
        } else if let Some(value) = option_value(&arg, &["--dir"], &mut args) {
            dir = Some(PathBuf::from(value?));
        } else if let Some(value) = option_value(&arg, &["--deliver"], &mut args) {
            deliver = Some(value?);
        } else if arg == "--allow-home" {
            allow_home = true;
        } else if arg == "--help" {
            return Ok(CronCommand::Help);
        } else {
            bail!("unknown option `{arg}` of `junees cron add`");
        }
    }
    match <[String; 2]>::try_from(positional) {
        Ok([schedule, prompt]) => Ok(CronCommand::Add {
            schedule,
            prompt,
            name,
            dir,
            deliver,
            allow_home,
        }),
        Err(positional) if positional.len() > 2 => bail!(
            "`junees cron add` takes the schedule and the prompt, each in quotes: \
             junees cron add \"every 30m\" \"<prompt>\" (got {} values)",
            positional.len()
        ),
        Err(_) => bail!(
            "usage: junees cron add \"<schedule>\" \"<prompt>\" [--name n] [--dir folder] [--deliver ...] \
             [--allow-home]"
        ),
    }
}

fn run(rest: Vec<String>) -> Result<CronCommand> {
    let (mut job, mut run_id, mut due) = (None, None, None);
    let mut args = rest.into_iter();
    while let Some(arg) = args.next() {
        if let Some(value) = option_value(&arg, &["--run-id"], &mut args) {
            let value = value?;
            if value.is_empty()
                || value.len() > 32
                || !value.chars().all(|c| c.is_ascii_alphanumeric())
            {
                bail!("--run-id takes letters and digits");
            }
            run_id = Some(value);
        } else if let Some(value) = option_value(&arg, &["--due"], &mut args) {
            let value = value?;
            due = Some(
                value
                    .parse()
                    .ok()
                    .with_context(|| format!("--due takes a unix time, got `{value}`"))?,
            );
        } else if arg.starts_with("--") {
            bail!("unknown option `{arg}` of `junees cron run`");
        } else if job.replace(arg).is_some() {
            bail!("`junees cron run` takes one job id");
        }
    }
    let job = job.context("`junees cron run` needs a job id (`junees cron list` shows them)")?;
    Ok(CronCommand::Run { job, run_id, due })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse_words(words: &[&str]) -> Result<CronCommand> {
        parse(words.iter().map(|word| word.to_string()))
    }

    #[test]
    fn parses_every_command() {
        assert_eq!(parse_words(&[]).unwrap(), CronCommand::List);
        assert_eq!(parse_words(&["ls"]).unwrap(), CronCommand::List);
        assert_eq!(
            parse_words(&["rm", "ab"]).unwrap(),
            CronCommand::Remove("ab".into())
        );
        assert_eq!(
            parse_words(&["pause", "ab"]).unwrap(),
            CronCommand::Pause("ab".into())
        );
        assert_eq!(
            parse_words(&["resume", "ab"]).unwrap(),
            CronCommand::Resume("ab".into())
        );
        assert_eq!(
            parse_words(&["allow-home", "ab"]).unwrap(),
            CronCommand::AllowHome("ab".into())
        );
        assert_eq!(
            parse_words(&["disallow-home", "ab"]).unwrap(),
            CronCommand::DisallowHome("ab".into())
        );
        assert_eq!(parse_words(&["daemon"]).unwrap(), CronCommand::Daemon);
        assert_eq!(parse_words(&["--help"]).unwrap(), CronCommand::Help);
        assert_eq!(parse_words(&["list", "-h"]).unwrap(), CronCommand::Help);
        assert_eq!(
            parse_words(&["run", "ab"]).unwrap(),
            CronCommand::Run {
                job: "ab".into(),
                run_id: None,
                due: None
            }
        );
        assert_eq!(
            parse_words(&["run", "--run-id", "f00d", "ab", "--due=17"]).unwrap(),
            CronCommand::Run {
                job: "ab".into(),
                run_id: Some("f00d".into()),
                due: Some(17)
            }
        );
        assert_eq!(
            parse_words(&[
                "add",
                "--name",
                "news",
                "daily 09:00",
                "--deliver=discord:42",
                "뉴스 요약",
                "--dir",
                "/srv",
                "--allow-home"
            ])
            .unwrap(),
            CronCommand::Add {
                schedule: "daily 09:00".into(),
                prompt: "뉴스 요약".into(),
                name: Some("news".into()),
                dir: Some(PathBuf::from("/srv")),
                deliver: Some("discord:42".into()),
                allow_home: true,
            }
        );
        assert_eq!(
            parse_words(&["add", "in 5m", "--", "--not an option"]).unwrap(),
            CronCommand::Add {
                schedule: "in 5m".into(),
                prompt: "--not an option".into(),
                name: None,
                dir: None,
                deliver: None,
                allow_home: false,
            }
        );
    }

    #[test]
    fn wrong_arguments_say_what_is_wrong() {
        for (words, wanted) in [
            (&["add", "every", "30m", "hi"][..], "each in quotes"),
            (&["add", "every 30m"][..], "usage"),
            (&["add", "every 30m", "hi", "--name"][..], "needs a value"),
            (&["add", "every 30m", "hi", "--color"][..], "unknown option"),
            (&["rm"][..], "needs a job id"),
            (&["allow-home"][..], "needs a job id"),
            (&["disallow-home", "a", "b"][..], "one job id"),
            (&["rm", "a", "b"][..], "one job id"),
            (&["list", "x"][..], "no arguments"),
            (&["run"][..], "needs a job id"),
            (&["run", "a", "b"][..], "one job id"),
            (&["run", "a", "--due", "soon"][..], "unix time"),
            (&["run", "a", "--run-id", "../x"][..], "letters and digits"),
            (&["start"][..], "unknown command"),
        ] {
            let err = parse_words(words).unwrap_err().to_string();
            assert!(err.contains(wanted), "{words:?}: {err}");
        }
    }
}
