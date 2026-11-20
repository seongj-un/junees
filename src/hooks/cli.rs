//! `junees hooks list|trust|untrust|test <event>`: the hooks a session in
//! this folder runs, trusting the project's hooks file, and trying the
//! hooks of one point.

use std::path::Path;

use anyhow::{Result, bail};
use serde_json::json;

use super::config::Event;
use super::run::Input;
use super::{Hook, ProjectFile, project_file, project_path, trust, user_hooks};
use crate::prompt_text::escape_hidden;

/// What `junees hooks` was asked to do.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HooksCommand {
    List,
    /// Trusts the project's hooks file of this folder as it is now.
    Trust,
    /// Forgets the trust of the project's hooks file of this folder.
    Untrust,
    /// Runs the hooks of `event` once with a sample input; a tool event
    /// gets a call of `tool` with an empty input.
    Test {
        event: Event,
        tool: String,
    },
}

pub const USAGE: &str =
    "usage: junees hooks [list | trust | untrust | test <event> [--tool <name>]]";

pub fn parse(args: impl IntoIterator<Item = String>) -> Result<HooksCommand> {
    let args: Vec<String> = args.into_iter().collect();
    let words: Vec<&str> = args.iter().map(String::as_str).collect();
    let event = |name: &str| -> Result<Event> {
        Event::parse(name).ok_or_else(|| {
            anyhow::anyhow!(
                "unknown hook event (one of {})",
                Event::ALL.map(Event::name).join(", ")
            )
        })
    };
    Ok(match words.as_slice() {
        [] | ["list"] => HooksCommand::List,
        ["trust"] => HooksCommand::Trust,
        ["untrust"] => HooksCommand::Untrust,
        ["test", name] => HooksCommand::Test {
            event: event(name)?,
            tool: "bash".into(),
        },
        ["test", name, "--tool", tool] => HooksCommand::Test {
            event: event(name)?,
            tool: tool.to_string(),
        },
        _ => bail!("{USAGE}"),
    })
}

/// Where the command looks: the state folder, the folder it runs in,
/// HOME, and the tool settings (`test` runs in the bash sandbox).
pub struct Place<'a> {
    pub home: Option<&'a Path>,
    pub cwd: &'a Path,
    pub user_home: Option<&'a Path>,
    pub tools: &'a crate::tools::ToolSettings,
}

/// Runs `command`; the text to print.
pub async fn run(command: &HooksCommand, place: &Place<'_>) -> Result<String> {
    match command {
        HooksCommand::List => list(place),
        HooksCommand::Trust => {
            let home = need_home(place)?;
            let Some(file) = project(place)? else {
                // Report a ledger read error even with no project to approve.
                trust::Ledger::read(Some(home))?;
                bail!(
                    "there is no project hooks file for this folder (looked for {})",
                    project_path(place.cwd, place.user_home).display()
                );
            };
            trust::trust(home, &file)?;
            Ok(format!(
                "trusted {} as it is now (sha256 {}); its hooks run from the next session in \
                 this project, and any change asks again. It holds:\n{}",
                file.path.display(),
                &file.digest[..16],
                quoted(&file.text)
            ))
        }
        HooksCommand::Untrust => {
            let home = need_home(place)?;
            let path = project_path(place.cwd, place.user_home);
            Ok(if trust::untrust(home, &path)? {
                format!(
                    "{} is no longer trusted; its hooks do not run\n",
                    path.display()
                )
            } else {
                format!("{} was not trusted\n", path.display())
            })
        }
        HooksCommand::Test { event, tool } => test(*event, tool, place).await,
    }
}

fn need_home<'a>(place: &Place<'a>) -> Result<&'a Path> {
    place
        .home
        .ok_or_else(|| anyhow::anyhow!("no state folder: set JUNEES_HOME or HOME"))
}

fn project(place: &Place<'_>) -> Result<Option<ProjectFile>> {
    project_file(place.cwd, place.user_home, place.home).map_err(anyhow::Error::msg)
}

/// `text` with every line indented and hidden characters escaped, whole.
pub fn quoted(text: &str) -> String {
    text.lines()
        .map(|line| format!("  | {}\n", escape_hidden(line)))
        .collect()
}

/// One hook on one line: its event, `match`, command and the files it
/// pins, if any.
pub fn summary(hook: &Hook) -> String {
    let spec = &hook.spec;
    let matched = spec
        .matcher
        .as_ref()
        .map(|matcher| format!(" match `{}`", escape_hidden(&matcher.source)))
        .unwrap_or_default();
    let pinned: String = hook
        .pins
        .iter()
        .map(|pin| match &pin.digest {
            Ok(digest) => format!(" (pinned {} sha256 {})", pin.path.display(), &digest[..16]),
            Err(err) => format!(
                " (cannot pin {}: {err}; it does not run)",
                pin.path.display()
            ),
        })
        .collect();
    format!("{}{matched} -> {}{pinned}", spec.event.name(), spec.shown())
}

fn hook_line(hook: &Hook) -> String {
    let spec = &hook.spec;
    format!(
        "  line {}: {} (timeout {}, on_error {})\n",
        spec.line,
        summary(hook),
        crate::mcp::server::seconds(spec.timeout),
        spec.on_error.name()
    )
}

fn list(place: &Place<'_>) -> Result<String> {
    let ledger = trust::Ledger::read(place.home)?;
    let mut out = String::new();
    match place.home {
        None => out.push_str("user hooks: none (no state folder)\n"),
        Some(home) => {
            let hooks = user_hooks(home)?;
            if hooks.is_empty() {
                out.push_str("user hooks: none\n");
            }
            let mut file = None;
            for hook in &hooks {
                if file != Some(hook.origin.file()) {
                    file = Some(hook.origin.file());
                    out.push_str(&format!(
                        "user hooks in {}:\n",
                        hook.origin.file().display()
                    ));
                }
                out.push_str(&hook_line(hook));
            }
        }
    }
    match project(place)? {
        None => out.push_str(&format!(
            "project hooks: none ({})\n",
            project_path(place.cwd, place.user_home).display()
        )),
        Some(file) => {
            let state = match &file.hooks {
                Err(err) => format!("cannot be read, so they do not run: {err}"),
                Ok(_) if ledger.is_trusted(&file) => "trusted".into(),
                Ok(_) => "not trusted (new, or changed since it was trusted), so they do \
                               not run; read the file, then `junees hooks trust`"
                    .into(),
            };
            out.push_str(&format!(
                "project hooks in {}: {state}\n",
                file.path.display()
            ));
            for hook in file.hooks() {
                out.push_str(&hook_line(&hook));
            }
        }
    }
    Ok(out)
}

async fn test(event: Event, tool: &str, place: &Place<'_>) -> Result<String> {
    let loaded = super::find(place.home, place.cwd, place.cwd)?.build(
        place.cwd,
        place.tools,
        super::hider(),
    )?;
    let mut out: String = loaded
        .warnings
        .iter()
        .map(|warning| format!("!! {warning}\n"))
        .collect();
    let Some(hooks) = loaded.hooks else {
        out.push_str("no hook runs here\n");
        return Ok(out);
    };
    let empty = json!({});
    let input = Input {
        session_id: Some("hooks-test"),
        tool: event.has_tool().then_some((tool, &empty)),
        result: matches!(event, Event::PostTool | Event::TurnEnd).then_some((false, "test")),
        prompt: (event == Event::UserPrompt).then_some("test"),
    };
    let fired = hooks.fire(event, input).await;
    if fired.records.is_empty() {
        out.push_str(&format!("no {} hook runs for this input\n", event.name()));
    }
    for record in &fired.records {
        out.push_str(&format!("{} [{}]\n", record.line(), record.origin));
        if let Some(message) = &record.message {
            out.push_str(&format!("  message: {message}\n"));
        }
    }
    for warning in &fired.warnings {
        out.push_str(&format!("!! {warning}\n"));
    }
    if let Some(reason) = &fired.blocked {
        out.push_str(&format!("blocked: {reason}\n"));
    }
    Ok(out)
}
