//! The Landlock system calls and the ruleset Junees builds with them
//! (<https://docs.kernel.org/userspace-api/landlock.html>). Written
//! against the kernel's ABI directly: three system calls and two small
//! structs, so no crate is needed.
//!
//! The ruleset is built in the parent (opening paths, reading folders),
//! and the child only calls `prctl(PR_SET_NO_NEW_PRIVS)` and
//! `landlock_restrict_self` between fork and exec: both are plain system
//! calls, safe there.

use std::io;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::path::{Path, PathBuf};

// Filesystem access rights (`LANDLOCK_ACCESS_FS_*`).
pub const EXECUTE: u64 = 1 << 0;
pub const WRITE_FILE: u64 = 1 << 1;
pub const READ_FILE: u64 = 1 << 2;
pub const REMOVE_DIR: u64 = 1 << 4;
pub const REMOVE_FILE: u64 = 1 << 5;
pub const MAKE_CHAR: u64 = 1 << 6;
pub const MAKE_DIR: u64 = 1 << 7;
pub const MAKE_REG: u64 = 1 << 8;
pub const MAKE_SOCK: u64 = 1 << 9;
pub const MAKE_FIFO: u64 = 1 << 10;
pub const MAKE_BLOCK: u64 = 1 << 11;
pub const MAKE_SYM: u64 = 1 << 12;
/// ABI 2: linking or renaming into another folder.
pub const REFER: u64 = 1 << 13;
/// ABI 3: truncating a file.
pub const TRUNCATE: u64 = 1 << 14;

// Network access rights (`LANDLOCK_ACCESS_NET_*`, ABI 4).
const NET_BIND_TCP: u64 = 1 << 0;
const NET_CONNECT_TCP: u64 = 1 << 1;

const CREATE_RULESET_VERSION: u32 = 1 << 0;
const RULE_PATH_BENEATH: libc::c_int = 1;

/// Reading a file and running it. Listing folders (`READ_DIR`) is left to
/// the usual permissions: a rule granting it on a folder grants it on
/// everything below, so it cannot leave out one folder and keep the ones
/// above it listable.
pub const READ: u64 = EXECUTE | READ_FILE;

/// Every way of changing the filesystem.
pub const WRITE: u64 = WRITE_FILE
    | REMOVE_DIR
    | REMOVE_FILE
    | MAKE_CHAR
    | MAKE_DIR
    | MAKE_REG
    | MAKE_SOCK
    | MAKE_FIFO
    | MAKE_BLOCK
    | MAKE_SYM
    | REFER
    | TRUNCATE;

/// Writing to a file that exists (`/dev/null`, a terminal) without
/// creating, removing or renaming anything.
pub const WRITE_EXISTING: u64 = WRITE_FILE | TRUNCATE;

/// The rights a rule on a file (not a folder) may carry.
const FILE_RIGHTS: u64 = EXECUTE | WRITE_FILE | READ_FILE | TRUNCATE;

/// The filesystem rights the kernel of `abi` knows of, out of the ones
/// Junees handles.
pub fn handled_fs(abi: u32) -> u64 {
    let mut rights = READ | WRITE;
    if abi < 2 {
        rights &= !REFER;
    }
    if abi < 3 {
        rights &= !TRUNCATE;
    }
    rights
}

/// The first ABI version that can restrict TCP.
pub const NETWORK_ABI: u32 = 4;

#[repr(C)]
struct RulesetAttr {
    handled_access_fs: u64,
    handled_access_net: u64,
}

#[repr(C, packed)]
struct PathBeneathAttr {
    allowed_access: u64,
    parent_fd: i32,
}

/// The Landlock ABI version of the running kernel, or why there is none.
pub fn abi() -> Result<u32, String> {
    // SAFETY: with a null attribute and size 0 the call only returns the
    // ABI version; it reads and writes no memory.
    let version = unsafe {
        libc::syscall(
            libc::SYS_landlock_create_ruleset,
            std::ptr::null::<RulesetAttr>(),
            0usize,
            CREATE_RULESET_VERSION,
        )
    };
    if version >= 1 {
        return u32::try_from(version).map_err(|_| format!("odd Landlock ABI {version}"));
    }
    let err = io::Error::last_os_error();
    Err(match err.raw_os_error() {
        Some(libc::ENOSYS) => "the kernel has no Landlock (it needs Linux 5.13 or later \
                               built with CONFIG_SECURITY_LANDLOCK)"
            .to_string(),
        Some(libc::EOPNOTSUPP) => "Landlock is turned off in this kernel (add `landlock` to \
                                   the `lsm=` boot parameter)"
            .to_string(),
        _ => format!("cannot ask the kernel for Landlock: {err}"),
    })
}

/// A ruleset being built: the paths and rights a confined command keeps.
pub struct Ruleset {
    fd: OwnedFd,
    handled_fs: u64,
}

impl Ruleset {
    /// A ruleset for the kernel of `abi` that handles reading and writing
    /// files, and TCP connect and bind too when `limit_tcp` (ABI 4 and
    /// later: no TCP rule is added, so every connect and bind is refused).
    pub fn new(abi: u32, limit_tcp: bool) -> io::Result<Self> {
        let handled_fs = handled_fs(abi);
        let limit_tcp = limit_tcp && abi >= NETWORK_ABI;
        let attr = RulesetAttr {
            handled_access_fs: handled_fs,
            handled_access_net: if limit_tcp {
                NET_BIND_TCP | NET_CONNECT_TCP
            } else {
                0
            },
        };
        // Kernels before ABI 4 know only the first field; the extra zero
        // field is accepted, a non-zero one would not be.
        let size = if abi >= NETWORK_ABI {
            std::mem::size_of::<RulesetAttr>()
        } else {
            std::mem::size_of::<u64>()
        };
        // SAFETY: `attr` lives across the call and `size` is at most its
        // size; the call returns a new close-on-exec file descriptor.
        let fd = unsafe {
            libc::syscall(
                libc::SYS_landlock_create_ruleset,
                &attr as *const RulesetAttr,
                size,
                0u32,
            )
        };
        if fd < 0 {
            return Err(io::Error::last_os_error());
        }
        let fd = RawFd::try_from(fd).map_err(|_| io::Error::other("odd ruleset fd"))?;
        // SAFETY: the kernel just returned this descriptor and nothing else
        // owns it.
        let fd = unsafe { OwnedFd::from_raw_fd(fd) };
        Ok(Self { fd, handled_fs })
    }

    /// Grants `rights` on everything at and below the open `path`; only
    /// the file rights when it is not a folder.
    fn add(&self, path: &OwnedFd, is_dir: bool, rights: u64) -> io::Result<()> {
        let mut allowed = rights & self.handled_fs;
        if !is_dir {
            allowed &= FILE_RIGHTS;
        }
        if allowed == 0 {
            return Ok(());
        }
        let attr = PathBeneathAttr {
            allowed_access: allowed,
            parent_fd: path.as_raw_fd(),
        };
        // SAFETY: `attr` lives across the call and the descriptor in it is
        // open; the call only reads the struct.
        let result = unsafe {
            libc::syscall(
                libc::SYS_landlock_add_rule,
                self.fd.as_raw_fd(),
                RULE_PATH_BENEATH,
                &attr as *const PathBeneathAttr,
                0u32,
            )
        };
        if result < 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }

    /// Grants `rights` on everything at and below `root` except the
    /// `except` paths and what is below them. Landlock rules only allow,
    /// so a folder holding an excepted path gets no rule itself: each of
    /// its entries gets one instead, and the walk goes on into the entry
    /// that holds the excepted path. Symbolic links met on the way get
    /// none (access through one is checked at its target), and the walk
    /// does not follow one that leads to an excepted path: an excepted
    /// path as written through a link (`~/.junees/config.toml` with
    /// `~/.junees` a link) keeps the link entry itself from being renamed
    /// or replaced, while its resolved form protects the target. `root`
    /// must be absolute and resolved ([`real_path`]), `except` absolute.
    ///
    /// The folders on the way to an excepted path get no rule of their
    /// own, so entries created there later (by another process) and new
    /// entries the command would create there are not covered.
    pub fn grant_except(&self, root: &Path, rights: u64, except: &[PathBuf]) -> io::Result<()> {
        if except.iter().any(|path| root.starts_with(path)) {
            return Ok(());
        }
        let Some((fd, kind)) = open_path(root, true)? else {
            return Ok(());
        };
        if !kind.is_dir() || !except.iter().any(|path| path.starts_with(root)) {
            return self.add(&fd, kind.is_dir(), rights);
        }
        drop(fd);
        let entries = match std::fs::read_dir(root) {
            Ok(entries) => entries,
            Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(()),
            Err(err) => return Err(err),
        };
        for entry in entries {
            let entry = entry?;
            let path = entry.path();
            if except.iter().any(|excepted| path.starts_with(excepted)) {
                continue;
            }
            if except.iter().any(|excepted| excepted.starts_with(&path)) {
                if !entry.file_type()?.is_symlink() {
                    self.grant_except(&path, rights, except)?;
                }
                continue;
            }
            // Not followed: a link swapped in now is skipped, not granted.
            if let Some((fd, kind)) = open_path(&path, false)?
                && !kind.is_symlink()
            {
                self.add(&fd, kind.is_dir(), rights)?;
            }
        }
        Ok(())
    }

    /// The ruleset's descriptor, for [`restrict_self`] in the child.
    pub fn raw_fd(&self) -> RawFd {
        self.fd.as_raw_fd()
    }
}

/// Opens `path` without reading it (`O_PATH`), following a final symbolic
/// link only when `follow`; `None` when it does not exist.
fn open_path(path: &Path, follow: bool) -> io::Result<Option<(OwnedFd, std::fs::FileType)>> {
    use std::os::unix::fs::OpenOptionsExt;
    let mut flags = libc::O_PATH | libc::O_CLOEXEC;
    if !follow {
        flags |= libc::O_NOFOLLOW;
    }
    let file = match std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(flags)
        .open(path)
    {
        Ok(file) => file,
        Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(None),
        // A dangling link, or an entry that went away.
        Err(err) if err.raw_os_error() == Some(libc::ELOOP) => return Ok(None),
        Err(err) => {
            return Err(io::Error::new(
                err.kind(),
                format!("{}: {err}", path.display()),
            ));
        }
    };
    let kind = file.metadata()?.file_type();
    Ok(Some((OwnedFd::from(file), kind)))
}

/// `path` made absolute with symbolic links, `.` and `..` resolved as far
/// as it exists; the missing rest is appended as written.
pub fn real_path(path: &Path) -> PathBuf {
    let path = std::path::absolute(path).unwrap_or_else(|_| path.to_path_buf());
    let mut existing = path.as_path();
    let mut rest = Vec::new();
    loop {
        if let Ok(real) = std::fs::canonicalize(existing) {
            return rest.iter().rev().fold(real, |path, part| path.join(part));
        }
        match (existing.parent(), existing.file_name()) {
            (Some(parent), Some(name)) => {
                rest.push(name.to_os_string());
                existing = parent;
            }
            _ => return path,
        }
    }
}

/// Puts the calling process under the ruleset `fd`, after setting
/// `no_new_privs` (setuid programs and file capabilities then give no
/// privileges, and Landlock needs it of an unprivileged process). Meant
/// for the child between fork and exec: it only makes two system calls
/// and allocates nothing.
pub fn restrict_self(fd: RawFd) -> io::Result<()> {
    // SAFETY: prctl with PR_SET_NO_NEW_PRIVS only sets a flag on this
    // process.
    if unsafe { libc::prctl(libc::PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0) } != 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: the call takes a descriptor and flags and touches no memory.
    if unsafe { libc::syscall(libc::SYS_landlock_restrict_self, fd, 0u32) } != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}
