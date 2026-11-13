//! `junees rewind` and the text that `/checkpoints` and `/undo` show.

use std::io::{BufRead, Write};
use std::path::Path;

use anyhow::{Context as _, Result, bail};

use super::{Manifest, SessionStore, Store, Undone};
use crate::tools::FileAccess;

pub const HELP: &str = "\
junees rewind --session <id> [--list] [--turn <n>] [--force] [--yes]

Undoes the file changes that write_file and edit_file made in a saved
session, from the file checkpoints in $JUNEES_HOME/checkpoints.

  --session <id>  the session (`junees sessions search`, the dashboard)
  --list          show its turns and the files each changed; change nothing
  --turn <n>      undo turn n and every later one (default: the last turn
                  that changed files)
  --force         put back also the files changed since the turn (by a
                  person or by bash); a file the turn created is still kept
                  when it changed
  --yes           do not ask before undoing

Files bash changed are not recorded, so they are not undone. The undo is a
checkpoint of its own: `junees rewind --session <id>` again undoes it. The
next message of the session starts with a line saying what was undone.
";

/// `junees rewind`'s options.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Options {
    pub session: String,
    pub list: bool,
    pub turn: Option<u64>,
    pub force: bool,
    pub yes: bool,
}

/// The options after `rewind`; `None` for `--help`.
pub fn parse(args: impl IntoIterator<Item = String>) -> Result<Option<Options>, String> {
    let mut options = Options::default();
    let mut session = None;
    let mut args = args.into_iter();
    while let Some(arg) = args.next() {
        let (name, inline) = match arg.split_once('=') {
            Some((name, value)) if name.starts_with("--") => {
                (name.to_string(), Some(value.to_string()))
            }
            _ => (arg.clone(), None),
        };
        let mut value = |what: &str| {
            inline
                .clone()
                .or_else(|| args.next())
                .ok_or_else(|| format!("{name} needs {what}"))
        };
        match name.as_str() {
            "-h" | "--help" => return Ok(None),
            "--session" => session = Some(value("a session id")?),
            "--turn" => {
                let turn = value("a turn number")?;
                options.turn = Some(
                    turn.parse::<u64>()
                        .ok()
                        .filter(|turn| *turn > 0)
                        .ok_or_else(|| format!("--turn takes a turn number, not `{turn}`"))?,
                );
            }
            "--list" if inline.is_none() => options.list = true,
            "--force" if inline.is_none() => options.force = true,
            "--yes" | "-y" if inline.is_none() => options.yes = true,
            _ => {
                return Err(format!(
                    "unknown option `{arg}`; see `junees rewind --help`"
                ));
            }
        }
    }
    options.session = session.ok_or("which session? use --session <id>")?;
    if options.list && (options.turn.is_some() || options.force || options.yes) {
        return Err("--list changes nothing; it takes no --turn, --force or --yes".into());
    }
    Ok(Some(options))
}

/// The turns, one block each, oldest first.
pub fn render(manifests: &[Manifest]) -> String {
    let mut text = String::new();
    for manifest in manifests {
        text.push_str(&format!("turn {} ({})", manifest.turn, manifest.created_at));
        if !manifest.undid.is_empty() {
            let undid: Vec<String> = manifest.undid.iter().map(u64::to_string).collect();
            text.push_str(&format!(", an undo of turn {}", undid.join(", ")));
        }
        text.push('\n');
        for file in &manifest.files {
            let note = if file.before == "too_large" {
                "  (too large to record: cannot be undone)"
            } else if !file.restorable() {
                "  (changed since: skipped unless forced)"
            } else {
                ""
            };
            text.push_str(&format!(
                "  {:<8} {}{note}\n",
                file.change().word(),
                file.path
            ));
        }
    }
    text
}

/// The turns from `from` on.
pub fn from_turn(manifests: &[Manifest], from: u64) -> Vec<Manifest> {
    manifests
        .iter()
        .filter(|manifest| manifest.turn >= from)
        .cloned()
        .collect()
}

/// What an undo did, for a terminal.
pub fn report(undone: &Undone) -> String {
    let mut text = String::new();
    for path in &undone.restored {
        text.push_str(&format!("  undone   {path}\n"));
    }
    for (path, reason) in &undone.skipped {
        text.push_str(&format!("  skipped  {path} ({reason})\n"));
    }
    if let Some(turn) = undone.checkpoint {
        text.push_str(&format!(
            "recorded as turn {turn}: undo it with `/undo` or `junees rewind --turn {turn}`\n"
        ));
    }
    text.push_str("files bash changed are not recorded, so they were not undone\n");
    text
}

/// Undoes from `from`, leaves the conversation its line and says what
/// happened.
pub fn undo(session: &SessionStore, from: u64, force: bool, access: &FileAccess) -> Result<Undone> {
    let undone = session
        .undo(from, force, access)
        .context("cannot read the file checkpoints")?;
    if !undone.turns.is_empty() {
        session
            .add_note(&undone.note())
            .context("cannot leave the conversation a note of the undo")?;
    }
    Ok(undone)
}

/// `junees rewind`: lists, or undoes after asking on `input`.
pub fn run(
    options: &Options,
    home: &Path,
    access: &FileAccess,
    input: &mut impl BufRead,
    out: &mut impl Write,
) -> Result<()> {
    let store = Store::new(home);
    let Some(session) = store.session(&options.session) else {
        bail!("`{}` is not a session id", options.session);
    };
    let manifests = session.list().context("cannot read the file checkpoints")?;
    let Some(last) = manifests.last() else {
        bail!(
            "session {} has no file checkpoints (turns that wrote files with write_file or \
             edit_file are recorded; older ones are removed after 30 days)",
            options.session
        );
    };
    if options.list {
        write!(out, "{}", render(&manifests))?;
        return Ok(());
    }
    let from = options.turn.unwrap_or(last.turn);
    let chosen = from_turn(&manifests, from);
    if chosen.is_empty() {
        bail!(
            "session {} has no checkpoint at turn {from} or later (the last is turn {})",
            options.session,
            last.turn
        );
    }
    writeln!(out, "undo these changes, newest turn first:")?;
    write!(out, "{}", render(&chosen))?;
    if !options.yes {
        write!(out, "undo? [y/N] ")?;
        out.flush()?;
        let mut answer = String::new();
        input.read_line(&mut answer)?;
        if !matches!(answer.trim().to_ascii_lowercase().as_str(), "y" | "yes") {
            writeln!(out, "(nothing undone)")?;
            return Ok(());
        }
    }
    let undone = undo(&session, from, options.force, access)?;
    write!(out, "{}", report(&undone))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(list: &[&str]) -> Result<Option<Options>, String> {
        parse(list.iter().map(|arg| arg.to_string()))
    }

    #[test]
    fn parses_the_options() {
        assert_eq!(
            args(&["--session", "s1", "--turn=3", "--force", "--yes"]).unwrap(),
            Some(Options {
                session: "s1".into(),
                list: false,
                turn: Some(3),
                force: true,
                yes: true,
            })
        );
        assert!(args(&["--session=s1", "--list"]).unwrap().unwrap().list);
        assert_eq!(args(&["-h"]).unwrap(), None);
        for (bad, says) in [
            (&["--list"][..], "which session"),
            (&["--session", "s", "--turn", "0"], "turn number"),
            (&["--session", "s", "--turn", "x"], "turn number"),
            (&["--session", "s", "--list", "--yes"], "changes nothing"),
            (&["--session", "s", "--what"], "unknown option"),
            (&["--session"], "needs a session id"),
        ] {
            let err = args(bad).unwrap_err();
            assert!(err.contains(says), "{bad:?}: {err}");
        }
    }
}
