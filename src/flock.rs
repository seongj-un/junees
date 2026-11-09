//! Exclusive `flock`s that a fork in another thread cannot fake as taken.

use std::fs::File;
use std::io;
use std::os::fd::AsRawFd;
use std::time::Duration;

/// How often, and how far apart, [`try_exclusive`] tries a taken lock.
const RETRIES: usize = 20;
const RETRY_PAUSE: Duration = Duration::from_millis(10);

/// Takes an exclusive `flock` on `file` without blocking; `Ok(false)` if
/// another holder keeps it.
///
/// A lock that looks taken is tried again for a moment (about 200 ms): a
/// child that another thread of this process forks shares every
/// descriptor, and with it a lock this process just released, until the
/// child execs and the close-on-exec descriptor goes away.
pub fn try_exclusive(file: &File) -> io::Result<bool> {
    for attempt in 0..RETRIES {
        if attempt > 0 {
            std::thread::sleep(RETRY_PAUSE);
        }
        // SAFETY: `file` owns the descriptor and outlives the call.
        if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } == 0 {
            return Ok(true);
        }
        let err = io::Error::last_os_error();
        if err.kind() != io::ErrorKind::WouldBlock {
            return Err(err);
        }
    }
    Ok(false)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Instant;

    #[test]
    fn a_held_lock_is_refused_after_the_retry_window() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("f");
        let held = File::create(&path).unwrap();
        assert!(try_exclusive(&held).unwrap());
        let other = File::open(&path).unwrap();
        let start = Instant::now();
        assert!(!try_exclusive(&other).unwrap());
        assert!(start.elapsed() >= RETRY_PAUSE * (RETRIES as u32 - 1));
        drop(held);
        assert!(try_exclusive(&other).unwrap());
    }

    #[test]
    fn a_lock_released_during_the_retry_window_is_taken() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("f");
        let held = File::create(&path).unwrap();
        assert!(try_exclusive(&held).unwrap());
        // Stands in for a forked child that execs a moment later.
        let release = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(50));
            drop(held);
        });
        let other = File::open(&path).unwrap();
        assert!(try_exclusive(&other).unwrap());
        release.join().unwrap();
    }
}
