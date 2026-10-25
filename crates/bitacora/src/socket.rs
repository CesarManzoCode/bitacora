//! Unix socket paths of any length. `sun_path` holds 108 bytes with the
//! terminating NUL, and a socket under a deep directory (a clone inside
//! `target/`, say) does not fit. A longer path is reached through its open
//! parent directory as `/proc/self/fd/<fd>/<name>`, which the kernel
//! resolves to the same file.

use std::fs::File;
use std::io;
use std::os::fd::AsRawFd;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};

/// Longest path `sun_path` holds, without the terminating NUL.
const SUN_PATH_MAX: usize = 107;

/// Binds a listener at `path`.
pub fn bind(path: &Path) -> io::Result<UnixListener> {
    with_short_path(path, |p| UnixListener::bind(p))
}

/// Connects to the listener at `path`.
pub fn connect(path: &Path) -> io::Result<UnixStream> {
    with_short_path(path, |p| UnixStream::connect(p))
}

fn with_short_path<T>(path: &Path, f: impl FnOnce(&Path) -> io::Result<T>) -> io::Result<T> {
    if path.as_os_str().len() <= SUN_PATH_MAX {
        return f(path);
    }
    let (Some(parent), Some(name)) = (path.parent(), path.file_name()) else {
        return f(path);
    };
    let parent = if parent.as_os_str().is_empty() {
        Path::new(".")
    } else {
        parent
    };
    // Kept open until `f` returns: the short path names it.
    let dir = File::open(parent)?;
    let short = PathBuf::from(format!("/proc/self/fd/{}", dir.as_raw_fd())).join(name);
    f(&short)
}
