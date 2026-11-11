//! `$JUNEES_HOME/webhooks/routes.json`: the routes and how their last run
//! went, locked (`routes.lock`) and written atomically like the cron jobs
//! ([`crate::cron::store`]). Secrets are not in it: a route names the
//! environment variable or the private file that holds its secret.

use std::fs::File;
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

use crate::cron::deliver::Delivery;
use crate::cron::store::{LastRun, lock_file, read_private, write_private};

/// The format of routes.json this build writes; newer ones are refused.
pub const FORMAT: u32 = 1;

/// Longest route name.
pub const NAME_CHARS: usize = 64;

/// What sends the requests of a route.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Kind {
    /// GitHub: `X-GitHub-Event`, `X-GitHub-Delivery`, `X-Hub-Signature-256`.
    Github,
    /// Anything else: `X-Junees-Event`, `X-Junees-Delivery`,
    /// `X-Junees-Signature`.
    Generic,
}

impl Kind {
    pub fn parse(text: &str) -> Result<Self> {
        match text.trim() {
            "github" => Ok(Self::Github),
            "generic" => Ok(Self::Generic),
            other => bail!("--kind takes github or generic (got `{other}`)"),
        }
    }

    pub fn word(self) -> &'static str {
        match self {
            Self::Github => "github",
            Self::Generic => "generic",
        }
    }
}

/// Where a route's secret is.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Secret {
    /// An environment variable of `junees webhook serve`.
    Env(String),
    /// A file only this user may read (0600).
    File(PathBuf),
}

impl Secret {
    /// The secret's bytes. `env` reads the server's environment.
    pub fn read(&self, env: &dyn Fn(&str) -> Option<String>) -> Result<Vec<u8>> {
        let value = match self {
            Self::Env(name) => env(name).with_context(|| format!("{name} is not set"))?,
            Self::File(path) => read_secret_file(path)?,
        };
        let value = value.trim_end_matches(['\r', '\n']);
        if value.is_empty() {
            bail!("the secret is empty");
        }
        Ok(value.as_bytes().to_vec())
    }

    /// Where it is, for listings.
    pub fn describe(&self) -> String {
        match self {
            Self::Env(name) => format!("env {name}"),
            Self::File(path) => format!("file {}", path.display()),
        }
    }
}

/// A secret file: a regular file of this user that nobody else may read.
fn read_secret_file(path: &Path) -> Result<String> {
    let metadata =
        std::fs::metadata(path).with_context(|| format!("cannot read {}", path.display()))?;
    if !metadata.is_file() {
        bail!("{} is not a regular file", path.display());
    }
    // SAFETY: getuid only returns this process's user id.
    let uid = unsafe { libc::getuid() };
    let mode = metadata.permissions().mode() & 0o777;
    if metadata.uid() != uid || mode & 0o077 != 0 {
        bail!(
            "{} must be yours and private (mode {mode:o}); run `chmod 600 {}`",
            path.display(),
            path.display()
        );
    }
    std::fs::read_to_string(path).with_context(|| format!("cannot read {}", path.display()))
}

/// One webhook route: `POST /hooks/<name>`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Route {
    pub name: String,
    pub kind: Kind,
    /// The prompt template ([`super::template`]).
    pub prompt: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub secret: Option<Secret>,
    /// The events that run (`pull_request` or `pull_request.opened`);
    /// empty for every event.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub events: Vec<String>,
    /// The folder a run works in (absolute).
    pub dir: PathBuf,
    pub deliver: Delivery,
    #[serde(default)]
    pub paused: bool,
    /// Unix time.
    pub created_at: i64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last: Option<LastRun>,
    /// The event of the last run (`pull_request.opened`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_event: Option<String>,
}

impl Route {
    /// Whether `event` (with `action`) passes the `--events` filter.
    pub fn wants(&self, event: &str, action: Option<&str>) -> bool {
        self.events.is_empty()
            || self
                .events
                .iter()
                .any(|wanted| match wanted.split_once('.') {
                    None => wanted == event,
                    Some((name, wanted_action)) => name == event && Some(wanted_action) == action,
                })
    }
}

/// `event` or `event.action`, for messages.
pub fn event_label(event: &str, action: Option<&str>) -> String {
    match action {
        Some(action) => format!("{event}.{action}"),
        None => event.to_string(),
    }
}

/// A route name: lower-case letters, digits, `-` and `_`, starting with a
/// letter or digit (it is part of the URL and of a folder name).
pub fn check_name(name: &str) -> Result<()> {
    let ok = !name.is_empty()
        && name.len() <= NAME_CHARS
        && name
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-' || b == b'_')
        && name
            .bytes()
            .next()
            .is_some_and(|b| b.is_ascii_alphanumeric());
    if !ok {
        bail!(
            "a route name is 1 to {NAME_CHARS} lower-case letters, digits, `-` or `_`, \
             starting with a letter or digit"
        );
    }
    Ok(())
}

/// One entry of `--events`: `event` or `event.action`, each part lower
/// case letters, digits and `_`.
pub fn check_event(entry: &str) -> Result<()> {
    let part = |text: &str| {
        !text.is_empty()
            && text.len() <= 64
            && text
                .bytes()
                .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_')
    };
    let ok = match entry.split_once('.') {
        None => part(entry),
        Some((event, action)) => part(event) && part(action),
    };
    if !ok {
        bail!(
            "`{}` is not an event: write `pull_request` or `pull_request.opened`",
            crate::prompt_text::escape_hidden(entry)
        );
    }
    Ok(())
}

#[derive(Serialize, Deserialize)]
struct File_ {
    format: u32,
    routes: Vec<Route>,
}

/// The webhooks folder of a state folder.
#[derive(Debug, Clone)]
pub struct Store {
    dir: PathBuf,
}

/// The routes, read under the lock; [`Locked::save`] writes them back.
pub struct Locked {
    _lock: File,
    path: PathBuf,
    pub routes: Vec<Route>,
}

impl Store {
    /// The store in `$JUNEES_HOME/webhooks`.
    pub fn new(home: &Path) -> Self {
        Self {
            dir: super::dir(home),
        }
    }

    pub fn path(&self) -> PathBuf {
        self.dir.join("routes.json")
    }

    /// Takes the lock (waiting up to 10 seconds) and reads the routes.
    pub fn lock(&self) -> Result<Locked> {
        let lock = lock_file(&self.dir, "routes.lock")?;
        let path = self.path();
        let routes = read(&path)?;
        Ok(Locked {
            _lock: lock,
            path,
            routes,
        })
    }

    /// The routes as they are now.
    pub fn routes(&self) -> Result<Vec<Route>> {
        Ok(self.lock()?.routes)
    }
}

impl Locked {
    pub fn save(&self) -> Result<()> {
        let text = serde_json::to_string_pretty(&File_ {
            format: FORMAT,
            routes: self.routes.clone(),
        })?;
        write_private(&self.path, &text)
    }

    /// The index of route `name`.
    pub fn find(&self, name: &str) -> Result<usize> {
        self.routes
            .iter()
            .position(|route| route.name == name)
            .with_context(|| {
                format!(
                    "no route `{}`; `junees webhook list` shows them",
                    crate::prompt_text::escape_hidden(name)
                )
            })
    }
}

fn read(path: &Path) -> Result<Vec<Route>> {
    let Some(text) = read_private(path)? else {
        return Ok(Vec::new());
    };
    let file: File_ =
        serde_json::from_str(&text).with_context(|| format!("{} is broken", path.display()))?;
    if file.format > FORMAT {
        bail!(
            "{} was written by a newer junees (format {}); update junees",
            path.display(),
            file.format
        );
    }
    Ok(file.routes)
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    pub fn route(name: &str) -> Route {
        Route {
            name: name.to_string(),
            kind: Kind::Github,
            prompt: "review {{payload.number}}".into(),
            secret: Some(Secret::Env("TEST_HOOK_SECRET".into())),
            events: Vec::new(),
            dir: PathBuf::from("/"),
            deliver: Delivery::Local,
            paused: false,
            created_at: 0,
            last: None,
            last_event: None,
        }
    }

    #[test]
    fn saves_privately_without_the_secret_value() {
        let home = tempfile::tempdir().unwrap();
        let store = Store::new(home.path());
        assert!(store.routes().unwrap().is_empty());
        let mut locked = store.lock().unwrap();
        locked.routes.push(route("pr"));
        locked.save().unwrap();
        drop(locked);
        assert_eq!(store.routes().unwrap(), [route("pr")]);
        let text = std::fs::read_to_string(store.path()).unwrap();
        assert!(text.contains(r#""env": "TEST_HOOK_SECRET""#), "{text}");
        let mode = std::fs::metadata(store.path())
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o600);
        // Others may not change it.
        std::fs::set_permissions(store.path(), std::fs::Permissions::from_mode(0o666)).unwrap();
        assert!(store.routes().unwrap_err().to_string().contains("mode 666"));
        std::fs::set_permissions(store.path(), std::fs::Permissions::from_mode(0o600)).unwrap();
        let locked = store.lock().unwrap();
        assert_eq!(locked.find("pr").unwrap(), 0);
        assert!(
            locked
                .find("nope")
                .unwrap_err()
                .to_string()
                .contains("no route")
        );
        drop(locked);
        std::fs::write(store.path(), r#"{"format": 2, "routes": []}"#).unwrap();
        assert!(
            store
                .routes()
                .unwrap_err()
                .to_string()
                .contains("newer junees")
        );
    }

    #[test]
    fn filters_events_and_checks_names() {
        let mut route = route("pr");
        assert!(route.wants("push", None));
        route.events = vec!["pull_request.opened".into(), "issue_comment".into()];
        assert!(route.wants("pull_request", Some("opened")));
        assert!(!route.wants("pull_request", Some("closed")));
        assert!(!route.wants("pull_request", None));
        assert!(route.wants("issue_comment", Some("created")));
        assert!(route.wants("issue_comment", None));
        assert!(!route.wants("push", None));
        for good in ["pr", "pr-review_2", "0x"] {
            check_name(good).unwrap();
        }
        for bad in ["", "-pr", "PR", "a/b", "a.b", "가", &"a".repeat(65)] {
            assert!(check_name(bad).is_err(), "{bad}");
        }
        for good in ["push", "pull_request.opened", "check_suite.completed"] {
            check_event(good).unwrap();
        }
        for bad in ["", "a.", ".b", "a.b.c", "Push", "a b", "a`"] {
            assert!(check_event(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn reads_secrets_from_the_environment_or_a_private_file() {
        let env = |key: &str| (key == "HOOK_S").then(|| "s3cret\n".to_string());
        assert_eq!(Secret::Env("HOOK_S".into()).read(&env).unwrap(), b"s3cret");
        let err = Secret::Env("UNSET".into()).read(&env).unwrap_err();
        assert!(err.to_string().contains("UNSET is not set"), "{err}");
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("secret");
        std::fs::write(&path, "from-file\n").unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
        let secret = Secret::File(path.clone());
        let err = secret.read(&env).unwrap_err().to_string();
        assert!(err.contains("chmod 600"), "{err}");
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
        assert_eq!(secret.read(&env).unwrap(), b"from-file");
        std::fs::write(&path, "\n").unwrap();
        assert!(secret.read(&env).unwrap_err().to_string().contains("empty"));
    }
}
