//! The state folder: who may open it and the files that hold settings,
//! programs or secrets; the size of the saved sessions.

use std::collections::HashSet;
use std::fs;
use std::io::ErrorKind;
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::Path;

use super::{Report, shown, uid};

const AREA: &str = "state folder";

/// Saved sessions above this size get a warning.
const LARGE_SESSIONS: u64 = 1024 * 1024 * 1024;

/// Files in the state folder that must be private, and whether they may
/// hold secrets (readable by others is then a failure, not a warning).
const PRIVATE_FILES: &[(&str, bool)] = &[
    (crate::config_file::FILE_NAME, true),
    (crate::mcp::config::FILE_NAME, true),
    ("cron/jobs.json", false),
    ("webhooks/routes.json", false),
];

/// `$JUNEES_HOME` (0700), its private files (0600) and `secrets/` (its
/// folders 0700, its files 0600).
pub(super) fn state_folder(report: &mut Report, home: Option<&Path>) {
    let Some(home) = home else {
        report.warn(
            AREA,
            "folder",
            "neither JUNEES_HOME nor HOME is set: settings, sessions and memory are not saved",
            "set HOME, or JUNEES_HOME to the folder Junees should use",
        );
        return;
    };
    if !private_folder(report, home, "folder", true) {
        return;
    }
    for &(name, holds_secrets) in PRIVATE_FILES {
        private_file(report, &home.join(name), name, holds_secrets);
    }
    let secrets = crate::tools::secrets_dir(home);
    if !private_folder(report, &secrets, "secrets/", false) {
        return;
    }
    let mut seen = HashSet::new();
    if let Ok(metadata) = fs::metadata(&secrets) {
        seen.insert((metadata.dev(), metadata.ino()));
    }
    secret_entries(report, &secrets, "secrets/", &mut seen);
}

/// Checks what is in the secrets folder `dir`, shown as `label` (ending in
/// `/`): folders must be 0700 and are looked into (a webhook secret file
/// may sit in one), everything else is a file that must be 0600. `seen`
/// holds the folders looked into already, so a link back up ends there.
fn secret_entries(report: &mut Report, dir: &Path, label: &str, seen: &mut HashSet<(u64, u64)>) {
    let entries = match fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(err) => {
            report.fail(
                AREA,
                label,
                format!("cannot list {}: {err}", shown(dir)),
                format!("make it yours and readable: chmod 700 {}", shown(dir)),
            );
            return;
        }
    };
    let mut names: Vec<String> = entries
        .filter_map(|entry| entry.ok())
        .map(|entry| entry.file_name().to_string_lossy().into_owned())
        .collect();
    names.sort();
    for name in names {
        let path = dir.join(&name);
        let name = format!("{label}{name}");
        match fs::metadata(&path) {
            Ok(metadata) if metadata.is_dir() => {
                let folder = format!("{name}/");
                if private_folder(report, &path, &folder, false)
                    && seen.insert((metadata.dev(), metadata.ino()))
                {
                    secret_entries(report, &path, &folder, seen);
                }
            }
            _ => private_file(report, &path, &name, true),
        }
    }
}

/// Checks the folder `path`; returns whether it is there to look into.
/// A missing folder is a warning when `required`, else nothing.
fn private_folder(report: &mut Report, path: &Path, name: &str, required: bool) -> bool {
    let metadata = match fs::metadata(path) {
        Ok(metadata) => metadata,
        Err(err) if err.kind() == ErrorKind::NotFound => {
            if required {
                report.warn(
                    AREA,
                    name,
                    format!("{} does not exist yet", shown(path)),
                    "run `junees setup` (or any junees command that saves something)",
                );
            }
            return false;
        }
        Err(err) => {
            report.fail(
                AREA,
                name,
                format!("cannot read {}: {err}", shown(path)),
                format!("make it yours and readable: chmod 700 {}", shown(path)),
            );
            return false;
        }
    };
    if !metadata.is_dir() {
        report.fail(
            AREA,
            name,
            format!("{} is not a folder", shown(path)),
            format!(
                "move it away; Junees keeps a folder there ({})",
                shown(path)
            ),
        );
        return false;
    }
    let mode = metadata.permissions().mode() & 0o777;
    if metadata.uid() != uid() {
        report.fail(
            AREA,
            name,
            format!(
                "{} belongs to another user (uid {}); Junees reads settings and runs programs \
                 from it",
                shown(path),
                metadata.uid()
            ),
            format!(
                "use a folder of your own, or chown it to yourself ({})",
                shown(path)
            ),
        );
    } else if mode & 0o077 != 0 {
        report.fail(
            AREA,
            name,
            format!(
                "{}: other users can open it (mode {mode:o}); it holds settings, keys and \
                 sessions",
                shown(path)
            ),
            format!("chmod 700 {}", shown(path)),
        );
    } else {
        report.ok(
            AREA,
            name,
            format!("{}: private (mode {mode:o})", shown(path)),
        );
    }
    true
}

/// Checks the file `path` if it is there: yours, and 0600.
fn private_file(report: &mut Report, path: &Path, name: &str, holds_secrets: bool) {
    let metadata = match fs::metadata(path) {
        Ok(metadata) => metadata,
        Err(err) if err.kind() == ErrorKind::NotFound => return,
        Err(err) => {
            report.fail(
                AREA,
                name,
                format!("cannot read {}: {err}", shown(path)),
                format!("make it yours and readable: chmod 600 {}", shown(path)),
            );
            return;
        }
    };
    let mode = metadata.permissions().mode() & 0o777;
    let chmod = format!("chmod 600 {}", shown(path));
    if !metadata.is_file() {
        report.fail(
            AREA,
            name,
            format!("{} is not a regular file", shown(path)),
            format!("replace it with a file ({})", shown(path)),
        );
    } else if metadata.uid() != uid() {
        report.fail(
            AREA,
            name,
            format!(
                "{} belongs to another user (uid {}), so Junees refuses it",
                shown(path),
                metadata.uid()
            ),
            format!("replace it with a file of your own ({})", shown(path)),
        );
    } else if mode & 0o022 != 0 {
        report.fail(
            AREA,
            name,
            format!("other users can change it (mode {mode:o})"),
            chmod,
        );
    } else if mode & 0o077 != 0 {
        let detail = format!("other users can read it (mode {mode:o})");
        if holds_secrets {
            report.fail(
                AREA,
                name,
                format!("{detail}; it may hold keys or tokens"),
                chmod,
            );
        } else {
            report.warn(AREA, name, detail, chmod);
        }
    } else {
        report.ok(AREA, name, format!("private (mode {mode:o})"));
    }
}

/// The saved sessions: how many files and bytes.
pub(super) fn sessions(report: &mut Report, home: &Path) {
    sessions_over(report, home, LARGE_SESSIONS);
}

/// [`sessions`], warning above `large` bytes.
pub(super) fn sessions_over(report: &mut Report, home: &Path, large: u64) {
    const AREA: &str = "sessions";
    let dir = crate::session::sessions_dir(home);
    if !dir.is_dir() {
        report.skip(
            AREA,
            "folder",
            format!("no saved sessions yet ({})", shown(&dir)),
        );
        return;
    }
    let (files, bytes) = size(&dir);
    let detail = format!("{files} files, {} in {}", human_size(bytes), shown(&dir));
    if bytes > large {
        report.warn(
            AREA,
            "folder",
            detail,
            format!(
                "delete old sessions there (`ls -t {}`); they are only needed to continue \
                 or search them",
                shown(&dir)
            ),
        );
    } else {
        report.ok(AREA, "folder", detail);
    }
}

/// Files and bytes under `dir`, not following links.
fn size(dir: &Path) -> (u64, u64) {
    let (mut files, mut bytes) = (0, 0);
    let mut folders = vec![dir.to_path_buf()];
    while let Some(folder) = folders.pop() {
        let Ok(entries) = fs::read_dir(&folder) else {
            continue;
        };
        for entry in entries.filter_map(|entry| entry.ok()) {
            let Ok(metadata) = fs::symlink_metadata(entry.path()) else {
                continue;
            };
            if metadata.is_dir() {
                folders.push(entry.path());
            } else if metadata.is_file() {
                files += 1;
                bytes += metadata.len();
            }
        }
    }
    (files, bytes)
}

/// `bytes` in B, KiB, MiB or GiB.
pub(super) fn human_size(bytes: u64) -> String {
    const UNITS: &[&str] = &["KiB", "MiB", "GiB", "TiB"];
    if bytes < 1024 {
        return format!("{bytes} B");
    }
    let mut value = bytes as f64 / 1024.0;
    let mut unit = 0;
    while value >= 1024.0 && unit + 1 < UNITS.len() {
        value /= 1024.0;
        unit += 1;
    }
    format!("{value:.1} {}", UNITS[unit])
}
