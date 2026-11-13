//! `POST /api/cron/<id>/pause|resume` and
//! `POST /api/webhooks/<name>/pause|resume`: the dashboard's only writes.
//! They change a job or route with the CLI's own functions
//! ([`crate::cron::command::pause`], [`crate::cron::command::resume`],
//! [`crate::webhook::command::set_paused`]) under the store's lock, as
//! `junees cron pause` does, and answer with the item as `/api/cron` or
//! `/api/webhooks` lists it. A job or route paused because it works in
//! the home folder is not resumed here (409): `allow-home` is a decision
//! for the terminal. Every write that names a valid target is logged to
//! `$JUNEES_HOME/dashboard.log`.

use std::io::Write as _;
use std::path::Path;
use std::sync::Arc;

use serde_json::Value;

use super::api::{Failure, blocking, finish, home, job_json, route_json};
use super::server::{Answer, Context};
use crate::config_file::Settings;

/// The log of the dashboard's writes, in the state folder.
pub const LOG_FILE: &str = "dashboard.log";

/// What a write does.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Action {
    Pause,
    Resume,
}

impl Action {
    fn word(self) -> &'static str {
        match self {
            Action::Pause => "pause",
            Action::Resume => "resume",
        }
    }
}

/// What a write changes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Group {
    Cron,
    Webhooks,
}

/// A write path taken apart: the group, the job id or route name, and the
/// action. `None` for any other path.
pub(super) fn parse(path: &str) -> Option<(Group, &str, Action)> {
    let rest = path.strip_prefix("/api/")?;
    let mut parts = rest.split('/');
    let group = match parts.next()? {
        "cron" => Group::Cron,
        "webhooks" => Group::Webhooks,
        _ => return None,
    };
    let key = parts.next()?;
    let action = match parts.next()? {
        "pause" => Action::Pause,
        "resume" => Action::Resume,
        _ => return None,
    };
    if parts.next().is_some() || key.is_empty() {
        return None;
    }
    Some((group, key, action))
}

/// Whether `path` is a write path ([`parse`]).
pub(super) fn is_write_path(path: &str) -> bool {
    parse(path).is_some()
}

/// Whether `key` may name a job or route: 1 to 64 of `A-Z`, `a-z`,
/// `0-9`, `_` and `-` (job ids are hex; route names are narrower still).
fn valid_key(key: &str) -> bool {
    (1..=64).contains(&key.len())
        && key
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
}

/// The answer to an authorized `POST` on a write path.
pub(super) async fn write(path: &str, context: &Arc<Context>) -> Answer {
    let Some((group, key, action)) = parse(path) else {
        return Answer::error(404, "not_found", "there is no such API path");
    };
    if !valid_key(key) {
        return Answer::error(
            400,
            "bad_id",
            "a job id or route name is 1 to 64 letters, digits, `_` or `-`",
        );
    }
    let key = key.to_string();
    let env = context.env.clone();
    let (settings, _) = Settings::load_for_setup(move |name| env(name));
    let result = blocking(settings, move |settings| {
        let result = apply(settings, group, &key, action);
        if let Some(home) = home(settings) {
            log(&home, group, &key, action, &result);
        }
        result
    })
    .await;
    finish(result, context)
}

/// Pauses or resumes job or route `key` in the state folder of
/// `settings`.
fn apply(settings: &Settings, group: Group, key: &str, action: Action) -> Result<Value, Failure> {
    let home = home(settings).ok_or_else(|| missing(group))?;
    match group {
        Group::Cron => {
            let store = crate::cron::store::Store::new(&home);
            if !store.path().exists() {
                return Err(missing(group));
            }
            let mut locked = store.lock().map_err(|err| busy(&err))?;
            let index = locked
                .jobs
                .iter()
                .position(|job| job.id == key)
                .ok_or_else(|| missing(group))?;
            let job = &mut locked.jobs[index];
            match action {
                // Already paused: left as it is, so a pause for the home
                // folder keeps its mark (and the 409 of a resume).
                Action::Pause if job.paused => return Ok(job_json(settings, job)),
                Action::Pause => crate::cron::command::pause(job),
                Action::Resume => {
                    if job.paused_for_home {
                        return Err(home_conflict("job", key));
                    }
                    let zone = crate::cron::timezone(&|name: &str| settings.get(name))
                        .map_err(|err| Failure::new(500, "bad_settings", format!("{err:#}")))?;
                    crate::cron::command::resume(job, &zone)
                        .map_err(|err| Failure::new(409, "conflict", format!("{err:#}")))?;
                }
            }
            locked.save().map_err(|err| Failure::internal(&err))?;
            Ok(job_json(settings, &locked.jobs[index]))
        }
        Group::Webhooks => {
            let store = crate::webhook::store::Store::new(&home);
            if !store.path().exists() {
                return Err(missing(group));
            }
            let mut locked = store.lock().map_err(|err| busy(&err))?;
            let index = locked
                .routes
                .iter()
                .position(|route| route.name == key)
                .ok_or_else(|| missing(group))?;
            let route = &mut locked.routes[index];
            if action == Action::Pause && route.paused {
                return Ok(route_json(settings, route));
            }
            if action == Action::Resume && route.paused_for_home {
                return Err(home_conflict("route", key));
            }
            crate::webhook::command::set_paused(route, action == Action::Pause);
            locked.save().map_err(|err| Failure::internal(&err))?;
            Ok(route_json(settings, &locked.routes[index]))
        }
    }
}

/// 503 for a store whose lock another junees holds past the wait (10
/// seconds): the CLI, the daemon or a run is writing it.
fn busy(err: &anyhow::Error) -> Failure {
    Failure::new(503, "busy", format!("{err:#}"))
}

fn missing(group: Group) -> Failure {
    let what = match group {
        Group::Cron => "there is no such job",
        Group::Webhooks => "there is no such route",
    };
    Failure::new(404, "not_found", what)
}

/// 409 for resuming `what` (`job`, `route`) `key`, paused because it
/// works in the home folder.
fn home_conflict(what: &str, key: &str) -> Failure {
    let command = crate::cron::command::allow_home_command(what, key);
    let other = if what == "job" {
        ", or remove the job and add it again with --dir"
    } else {
        ""
    };
    Failure::new(
        409,
        "conflict",
        format!(
            "{what} {key} is paused because it works in the home folder; the dashboard does \
             not resume it. Run `{command}` in a terminal to allow that folder (it resumes the \
             {what}){other}"
        ),
    )
}

/// Appends one line for a write to the log in `home`: the time, the
/// action, the target and how it ended. Never the token or a body. A log
/// that cannot be written does not undo the write; it is reported on
/// stderr.
fn log(home: &Path, group: Group, key: &str, action: Action, result: &Result<Value, Failure>) {
    let target = match group {
        Group::Cron => "cron",
        Group::Webhooks => "webhook",
    };
    let outcome = match result {
        Ok(_) => "ok".to_string(),
        Err(failure) => format!("{} {}", failure.status, failure.code),
    };
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    log_line(
        home,
        &format!(
            "{} {} {target} {key} {outcome}\n",
            crate::session::format_utc_seconds(now),
            action.word()
        ),
    );
}

/// Appends `line` (ending in a newline) to the log in `home`, made 0600;
/// a failure is reported on stderr.
pub(super) fn log_line(home: &Path, line: &str) {
    let written = {
        use std::os::unix::fs::OpenOptionsExt;
        std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .mode(0o600)
            .open(home.join(LOG_FILE))
            .and_then(|mut file| file.write_all(line.as_bytes()))
    };
    if let Err(err) = written {
        eprintln!(
            "dashboard: cannot write {}: {err}",
            home.join(LOG_FILE).display()
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn write_paths_are_parsed_strictly() {
        assert_eq!(
            parse("/api/cron/a1b2c3d4/pause"),
            Some((Group::Cron, "a1b2c3d4", Action::Pause))
        );
        assert_eq!(
            parse("/api/webhooks/gh/resume"),
            Some((Group::Webhooks, "gh", Action::Resume))
        );
        for path in [
            "/api/cron",
            "/api/cron//pause",
            "/api/cron/a/stop",
            "/api/cron/a/pause/x",
            "/api/sessions/a/pause",
            "/api/cron/a",
            "/cron/a/pause",
        ] {
            assert_eq!(parse(path), None, "{path}");
        }
        assert!(valid_key("a1b2c3d4"));
        for key in ["..", "a.b", "a%2f", &"x".repeat(65)] {
            assert!(!valid_key(key), "{key}");
        }
    }

    #[test]
    fn the_home_conflict_names_the_command() {
        let failure = home_conflict("job", "a1b2c3d4");
        assert_eq!((failure.status, failure.code), (409, "conflict"));
        assert!(
            failure
                .message
                .contains("`junees cron allow-home a1b2c3d4`"),
            "{}",
            failure.message
        );
        let failure = home_conflict("route", "gh");
        assert!(
            failure.message.contains("`junees webhook allow-home gh`"),
            "{}",
            failure.message
        );
    }
}
