//! Checking a downloaded release and putting its binary in place.

use std::fs;
use std::io::{Read, Write};
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use anyhow::{Context, Result, bail};

use super::version::Version;

/// Largest archive an update downloads.
pub const MAX_ARCHIVE_BYTES: u64 = 100 << 20;
/// Largest `SHA256SUMS` an update reads.
pub const MAX_SUMS_BYTES: u64 = 64 << 10;
/// Largest binary taken out of an archive.
const MAX_BINARY_BYTES: u64 = 300 << 20;
/// Largest listing of the binary's entry read from `tar -tv`.
const MAX_LISTING_BYTES: u64 = 64 << 10;

/// The name of the checksum file of a release.
pub const SUMS_FILE: &str = "SHA256SUMS";

/// The release build for `os`/`arch` (`std::env::consts`), if there is one.
pub fn target_for(os: &str, arch: &str) -> Option<&'static str> {
    Some(match (os, arch) {
        ("linux", "x86_64") => "x86_64-unknown-linux-musl",
        ("linux", "aarch64") => "aarch64-unknown-linux-musl",
        ("macos", "x86_64") => "x86_64-apple-darwin",
        ("macos", "aarch64") => "aarch64-apple-darwin",
        _ => return None,
    })
}

/// The release build for this computer, if there is one.
pub fn this_target() -> Option<&'static str> {
    target_for(std::env::consts::OS, std::env::consts::ARCH)
}

/// `junees-<version>-<target>`: the archive's name without `.tar.gz`, and
/// the folder in it.
pub fn stem(version: &Version, target: &str) -> String {
    format!("junees-{version}-{target}")
}

/// The SHA-256 that `sums` (the lines of `sha256sum`: `<hex>  <name>`, or
/// `<hex> *<name>`) gives for the file `name`.
pub fn checksum_of(sums: &[u8], name: &str) -> Result<[u8; 32]> {
    let sums = std::str::from_utf8(sums).context("SHA256SUMS is not text")?;
    for line in sums.lines() {
        let Some((hex, file)) = line.split_once(' ') else {
            continue;
        };
        let file = file.trim_start_matches(' ').trim_start_matches('*');
        if file != name {
            continue;
        }
        return parse_hex(hex).with_context(|| format!("SHA256SUMS has a broken line for {name}"));
    }
    bail!("SHA256SUMS lists no {name}")
}

fn parse_hex(hex: &str) -> Option<[u8; 32]> {
    if hex.len() != 64 || !hex.bytes().all(|b| b.is_ascii_hexdigit()) {
        return None;
    }
    let mut out = [0u8; 32];
    for (i, byte) in out.iter_mut().enumerate() {
        *byte = u8::from_str_radix(&hex[2 * i..2 * i + 2], 16).ok()?;
    }
    Some(out)
}

fn to_hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// Fails unless `bytes` has the SHA-256 `expected`.
pub fn verify(name: &str, bytes: &[u8], expected: &[u8; 32]) -> Result<()> {
    let digest = ring::digest::digest(&ring::digest::SHA256, bytes);
    if digest.as_ref() != expected {
        bail!(
            "{name} does not match SHA256SUMS (got {}, expected {}); nothing was replaced",
            to_hex(digest.as_ref()),
            to_hex(expected)
        );
    }
    Ok(())
}

/// The file `member` of the gzipped tar `archive`, read with the system's
/// `tar` (macOS and Linux both have one) from standard input to standard
/// output. The archive checked against SHA256SUMS and the binary never
/// touch the disk before the install: in a temporary folder, a process
/// of the same user (a confined command a session left running there)
/// could swap them between the check and the install. Only that one
/// member is taken out, and it must be the archive's one regular file of
/// that name.
pub fn extract(archive: &[u8], member: &str) -> Result<Vec<u8>> {
    // `tar -tv` starts each line with the entry's type and mode, as `ls
    // -l` does: `-` for a regular file (a hard link is `h`).
    let listing = tar(archive, &["-tvzf", "-", member], member, MAX_LISTING_BYTES)?;
    let listing = String::from_utf8_lossy(&listing);
    let lines: Vec<&str> = listing.lines().collect();
    match lines[..] {
        [line] if line.starts_with('-') => {}
        [_] => bail!("{member} in the release archive is not a regular file"),
        _ => bail!(
            "the release archive has {} entries for {member}",
            lines.len()
        ),
    }
    let binary = tar(archive, &["-xzOf", "-", member], member, MAX_BINARY_BYTES)?;
    if binary.is_empty() {
        bail!("{member} in the release archive is empty");
    }
    Ok(binary)
}

/// Runs `tar` with `args` on `archive` (its standard input) and returns
/// its standard output, failing past `limit` bytes.
fn tar(archive: &[u8], args: &[&str], member: &str, limit: u64) -> Result<Vec<u8>> {
    let mut child = Command::new("tar")
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .context("cannot run `tar` to unpack the release")?;
    let (Some(mut stdin), Some(stdout), Some(stderr)) =
        (child.stdin.take(), child.stdout.take(), child.stderr.take())
    else {
        bail!("cannot talk to `tar`");
    };
    let (read, errors) = std::thread::scope(|scope| {
        // Written from its own thread so that a full output pipe cannot
        // stall both sides. tar stopping early is an error of its own.
        scope.spawn(move || {
            let _ = stdin.write_all(archive);
        });
        let errors = scope.spawn(move || {
            let mut text = Vec::new();
            let _ = stderr.take(MAX_LISTING_BYTES).read_to_end(&mut text);
            text
        });
        let mut out = Vec::new();
        let read = stdout.take(limit + 1).read_to_end(&mut out).map(|_| out);
        if !matches!(&read, Ok(out) if out.len() as u64 <= limit) {
            let _ = child.kill();
        }
        (read, errors.join().unwrap_or_default())
    });
    let status = child.wait().context("cannot wait for `tar`")?;
    let out = read.context("cannot read what `tar` unpacked")?;
    if out.len() as u64 > limit {
        bail!("{member} in the release archive is over {limit} bytes");
    }
    if !status.success() {
        bail!(
            "cannot unpack {member} from the release archive: {}",
            crate::prompt_text::escape_hidden(String::from_utf8_lossy(&errors).trim())
        );
    }
    Ok(out)
}

/// Removes a file when dropped, unless disarmed.
struct RemoveOnDrop(Option<PathBuf>);

impl Drop for RemoveOnDrop {
    fn drop(&mut self) {
        if let Some(path) = self.0.take() {
            let _ = fs::remove_file(path);
        }
    }
}

/// Replaces the program at `exe` with `binary`: a temporary file next to
/// it (same folder, so the same file system) gets the bytes, the old
/// file's permissions and an fsync, then one `rename` puts it in place, so
/// the old binary stays whole until the new one is. A link is followed to
/// the file it points at, which is what gets replaced. Returns that file.
pub fn replace(exe: &Path, binary: &[u8]) -> Result<PathBuf> {
    let real = exe
        .canonicalize()
        .with_context(|| format!("cannot find the running program {}", exe.display()))?;
    let dir = real
        .parent()
        .with_context(|| format!("{} has no folder", real.display()))?;
    let name = real
        .file_name()
        .with_context(|| format!("{} has no file name", real.display()))?
        .to_string_lossy()
        .into_owned();
    let mode = fs::metadata(&real)
        .with_context(|| format!("cannot read {}", real.display()))?
        .permissions()
        .mode()
        & 0o777;
    let temp = dir.join(format!(
        ".{name}.update-{}",
        crate::cron::store::random_hex(16)
    ));
    let mut file = match fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o700)
        .open(&temp)
    {
        Ok(file) => file,
        Err(err)
            if matches!(
                err.kind(),
                std::io::ErrorKind::PermissionDenied | std::io::ErrorKind::ReadOnlyFilesystem
            ) =>
        {
            bail!(
                "cannot write in {} ({err}), so {} cannot be replaced: run `junees update` as \
                 the user who installed it, or install again into a folder of yours with \
                 install.sh (JUNEES_INSTALL_DIR)",
                dir.display(),
                real.display()
            );
        }
        Err(err) => {
            return Err(err).with_context(|| format!("cannot create {}", temp.display()));
        }
    };
    let mut guard = RemoveOnDrop(Some(temp.clone()));
    file.write_all(binary)
        .and_then(|()| file.set_permissions(fs::Permissions::from_mode(mode | 0o700)))
        .and_then(|()| file.sync_all())
        .with_context(|| format!("cannot write {}", temp.display()))?;
    drop(file);
    fs::rename(&temp, &real)
        .with_context(|| format!("cannot move the new binary to {}", real.display()))?;
    guard.0 = None;
    // The rename is only durable once the folder is synced; failing that
    // the new binary is in place all the same.
    if let Ok(folder) = fs::File::open(dir) {
        let _ = folder.sync_all();
    }
    Ok(real)
}

/// What to tell after `path` was replaced: a cargo install, and running
/// Junees processes (seen through their locks) that keep the old binary
/// until they restart.
pub fn notes(path: &Path, cargo_bin: Option<&Path>, home: Option<&Path>) -> Vec<String> {
    let mut notes = Vec::new();
    if let Some(bin) = cargo_bin.and_then(|bin| bin.canonicalize().ok())
        && path.parent() == Some(bin.as_path())
    {
        notes.push(format!(
            "{} came from `cargo install`; it was replaced in place, and a later \
             `cargo install` puts its own build back",
            path.display()
        ));
    }
    if let Some(home) = home {
        let running = [
            (
                held(&home.join(crate::gateway::LOCK_FILE)),
                "the Discord gateway (`junees gateway`)",
            ),
            (
                held(&home.join(crate::telegram::LOCK_FILE)),
                "the Telegram gateway (`junees telegram`)",
            ),
            (
                held(&home.join(crate::slack::LOCK_FILE)),
                "the Slack gateway (`junees slack`)",
            ),
            (
                crate::cron::daemon::is_running(home),
                "the cron daemon (`junees cron daemon`)",
            ),
            (
                crate::webhook::is_serving(home),
                "the webhook server (`junees webhook serve`)",
            ),
        ];
        for (running, what) in running {
            if running {
                notes.push(format!(
                    "{what} is running the old binary: restart it to use the new one"
                ));
            }
        }
    }
    notes
}

/// Whether a process holds the lock `path` now.
fn held(path: &Path) -> bool {
    let Ok(file) = fs::File::open(path) else {
        return false;
    };
    !crate::flock::try_exclusive(&file).unwrap_or(false)
}
