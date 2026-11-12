//! The saved logins: `$JUNEES_HOME/secrets/mcp-oauth/<server>.json`, one
//! per server, mode 0600 in a 0700 folder.
//!
//! The secrets folder is one the model's tools may neither read nor write
//! ([`crate::tools::SettingsFile`]), like `config.toml`. MCP 2026-07-28,
//! security-considerations.md, "Token Theft": clients MUST implement
//! secure token storage.

use std::fs::{DirBuilder, File, OpenOptions};
use std::io::{Read, Write};
use std::os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde::{Deserialize, Serialize};

/// The folder under the secrets folder.
pub const FOLDER: &str = "mcp-oauth";
/// The file format this build writes; a newer one is refused.
pub const FORMAT: u32 = 1;
/// A larger file is refused unread.
const MAX_FILE_BYTES: u64 = 256 * 1024;
/// How long a refresh waits for another process's refresh.
const LOCK_WAIT: Duration = Duration::from_secs(15);

/// One saved login. Deliberately not `Debug`: it holds the tokens.
#[derive(Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Saved {
    pub format: u32,
    /// The `url` of the server the login is for: the tokens go nowhere
    /// else.
    pub server_url: String,
    /// The resource identifier the tokens were asked for (RFC 8707).
    pub resource: String,
    /// The authorization server that issued them.
    pub issuer: String,
    pub token_endpoint: String,
    pub client_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub client_secret: Option<String>,
    /// How the client authenticates at the token endpoint: `none`,
    /// `client_secret_basic` or `client_secret_post`.
    pub client_auth: String,
    pub access_token: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub refresh_token: Option<String>,
    /// Unix seconds; `None` when the server did not say.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expires_at: Option<u64>,
    /// The granted scopes, space-separated.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scope: Option<String>,
    /// Scopes the server asked for later (HTTP 403 `insufficient_scope`),
    /// which the next login asks for too (step-up).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub wanted_scope: Option<String>,
    /// Unix seconds of the login or the last refresh.
    pub saved_at: u64,
}

/// `$JUNEES_HOME/secrets/mcp-oauth`.
pub fn folder(home: &Path) -> PathBuf {
    crate::tools::secrets_dir(home).join(FOLDER)
}

/// The file of the server `name` (a checked server name: letters, digits,
/// `_` and `-`).
pub fn path(home: &Path, name: &str) -> PathBuf {
    folder(home).join(format!("{name}.json"))
}

/// The saved login at `path`; `None` when there is none. A file that is
/// not the user's own private regular file is refused. Errors never quote
/// the file.
pub fn read(path: &Path) -> Result<Option<Saved>, String> {
    let shown = path.display();
    let metadata = match std::fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(err) => return Err(format!("cannot read {shown}: {err}")),
    };
    if !metadata.is_file() {
        return Err(format!("{shown} is not a regular file"));
    }
    if metadata.uid() != uid() {
        return Err(format!("{shown} belongs to another user"));
    }
    let mode = metadata.permissions().mode() & 0o777;
    if mode & 0o077 != 0 {
        return Err(format!(
            "{shown} can be read or changed by other users (mode {mode:o}); it holds tokens, so \
             run `chmod 600 {shown}` (or log in again)"
        ));
    }
    if metadata.len() > MAX_FILE_BYTES {
        return Err(format!("{shown} is larger than 256 KiB"));
    }
    let mut text = String::new();
    OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(path)
        .and_then(|file| file.take(MAX_FILE_BYTES).read_to_string(&mut text))
        .map_err(|err| format!("cannot read {shown}: {err}"))?;
    // A serde error may quote a value: name only where it is.
    let saved: Saved = serde_json::from_str(&text).map_err(|err| {
        format!(
            "{shown} is not a saved login (line {}, column {}); log in again",
            err.line(),
            err.column()
        )
    })?;
    if saved.format > FORMAT {
        return Err(format!(
            "{shown} was written by a newer junees (format {}); update junees or log in again",
            saved.format
        ));
    }
    Ok(Some(saved))
}

/// Saves `saved` for the server `name`: a private temporary file renamed
/// over the old one, in a private folder.
pub fn write(home: &Path, name: &str, saved: &Saved) -> Result<(), String> {
    let folder = make_folder(home)?;
    let target = path(home, name);
    let temporary = folder.join(format!(
        ".{name}.json.{}.{}",
        std::process::id(),
        super::pkce::random_text(6)?
    ));
    let text = serde_json::to_string_pretty(saved).expect("serializable") + "\n";
    let written = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW)
        .open(&temporary)
        .and_then(|mut file| {
            file.write_all(text.as_bytes())?;
            file.sync_all()
        })
        .and_then(|()| std::fs::rename(&temporary, &target));
    if let Err(err) = written {
        let _ = std::fs::remove_file(&temporary);
        return Err(format!(
            "cannot save the login to {}: {err}",
            target.display()
        ));
    }
    Ok(())
}

/// Removes the saved login of `name`; whether there was one.
pub fn remove(home: &Path, name: &str) -> Result<bool, String> {
    let target = path(home, name);
    match std::fs::remove_file(&target) {
        Ok(()) => Ok(true),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(err) => Err(format!("cannot remove {}: {err}", target.display())),
    }
}

/// Takes the lock that serializes refreshes of every saved login (an
/// authorization server may rotate the refresh token, so two processes
/// refreshing at once would lose one): a `flock` on the folder, waited
/// for up to 15 s. Held while the file lives.
pub fn lock(home: &Path) -> Result<File, String> {
    let folder = make_folder(home)?;
    let file =
        File::open(&folder).map_err(|err| format!("cannot open {}: {err}", folder.display()))?;
    let started = std::time::Instant::now();
    loop {
        match crate::flock::try_exclusive(&file) {
            Ok(true) => return Ok(file),
            Ok(false) if started.elapsed() < LOCK_WAIT => {}
            Ok(false) => {
                return Err(format!(
                    "another junees kept {} locked for {} s",
                    folder.display(),
                    LOCK_WAIT.as_secs()
                ));
            }
            Err(err) => return Err(format!("cannot lock {}: {err}", folder.display())),
        }
    }
}

/// Creates the secrets folder and `mcp-oauth` in it (0700) when missing,
/// and checks `mcp-oauth` is the user's own private folder.
fn make_folder(home: &Path) -> Result<PathBuf, String> {
    let folder = folder(home);
    DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(&folder)
        .map_err(|err| format!("cannot create {}: {err}", folder.display()))?;
    let metadata = std::fs::symlink_metadata(&folder)
        .map_err(|err| format!("cannot read {}: {err}", folder.display()))?;
    if !metadata.is_dir() || metadata.uid() != uid() {
        return Err(format!("{} is not a folder of this user", folder.display()));
    }
    if metadata.permissions().mode() & 0o077 != 0 {
        std::fs::set_permissions(&folder, std::fs::Permissions::from_mode(0o700))
            .map_err(|err| format!("cannot make {} private: {err}", folder.display()))?;
    }
    Ok(folder)
}

fn uid() -> u32 {
    // SAFETY: getuid only returns this process's user id.
    unsafe { libc::getuid() }
}

/// Seconds since the Unix epoch.
pub fn now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

#[cfg(test)]
pub(super) fn sample() -> Saved {
    Saved {
        format: FORMAT,
        server_url: "https://mcp.example.com/mcp".into(),
        resource: "https://mcp.example.com/mcp".into(),
        issuer: "https://auth.example.com".into(),
        token_endpoint: "https://auth.example.com/token".into(),
        client_id: "client-1".into(),
        client_secret: None,
        client_auth: "none".into(),
        access_token: "access-s3cret".into(),
        refresh_token: Some("refresh-s3cret".into()),
        expires_at: Some(now() + 3600),
        scope: Some("files:read".into()),
        wanted_scope: None,
        saved_at: now(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mode(path: &Path) -> u32 {
        std::fs::metadata(path).unwrap().permissions().mode() & 0o777
    }

    #[test]
    fn saves_privately_and_reads_back() {
        let home = tempfile::tempdir().unwrap();
        let path = path(home.path(), "docs");
        assert!(read(&path).unwrap().is_none());
        write(home.path(), "docs", &sample()).unwrap();
        assert_eq!(mode(&path), 0o600);
        assert_eq!(mode(&folder(home.path())), 0o700);
        assert_eq!(mode(&crate::tools::secrets_dir(home.path())), 0o700);
        assert!(read(&path).unwrap().unwrap() == sample());
        // Rewritten in place; no temporary file stays.
        let mut changed = sample();
        changed.access_token = "other".into();
        write(home.path(), "docs", &changed).unwrap();
        assert_eq!(read(&path).unwrap().unwrap().access_token, "other");
        assert_eq!(std::fs::read_dir(folder(home.path())).unwrap().count(), 1);
        assert!(remove(home.path(), "docs").unwrap());
        assert!(!remove(home.path(), "docs").unwrap());
        assert!(read(&path).unwrap().is_none());
    }

    #[test]
    fn an_open_folder_is_made_private_again() {
        let home = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(folder(home.path())).unwrap();
        std::fs::set_permissions(folder(home.path()), std::fs::Permissions::from_mode(0o755))
            .unwrap();
        write(home.path(), "docs", &sample()).unwrap();
        assert_eq!(mode(&folder(home.path())), 0o700);
    }

    #[test]
    fn refuses_files_others_can_read_links_and_garbage_without_quoting() {
        let home = tempfile::tempdir().unwrap();
        write(home.path(), "docs", &sample()).unwrap();
        let path = path(home.path(), "docs");
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
        let message = read(&path).err().unwrap();
        assert!(message.contains("chmod 600"), "{message}");
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();

        let link = folder(home.path()).join("link.json");
        std::os::unix::fs::symlink(&path, &link).unwrap();
        assert!(read(&link).err().unwrap().contains("not a regular file"));

        std::fs::write(&path, r#"{"format": 1, "access_token": 12345678}"#).unwrap();
        let message = read(&path).err().unwrap();
        assert!(message.contains("not a saved login"), "{message}");
        assert!(!message.contains("12345678"), "{message}");

        let mut newer = serde_json::to_value(sample()).unwrap();
        newer["format"] = 99.into();
        std::fs::write(&path, newer.to_string()).unwrap();
        assert!(read(&path).err().unwrap().contains("newer junees"));
    }

    #[test]
    fn the_lock_is_exclusive() {
        let home = tempfile::tempdir().unwrap();
        let held = lock(home.path()).unwrap();
        let other = File::open(folder(home.path())).unwrap();
        assert!(!crate::flock::try_exclusive(&other).unwrap());
        drop(held);
        assert!(crate::flock::try_exclusive(&other).unwrap());
    }
}
