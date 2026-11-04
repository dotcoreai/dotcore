//! Flushes written files and directory entries to stable storage for every durable write, such as
//! site configuration, retained state, node folders and prepared artifacts.
//!
//! Production always uses the strongest flush the platform offers: on macOS that flushes the
//! drive's own cache, which costs milliseconds per call and serializes across processes. Test
//! processes may switch, once and for the whole process, to a plain flush that keeps the same
//! calls and error paths without that cost.

use std::fs::File;
use std::io;
use std::os::fd::AsRawFd;
use std::sync::atomic::{AtomicBool, Ordering};

/// Whether this process uses the relaxed flush. This crate's own unit tests start relaxed; every
/// other process starts with the full flush until test support relaxes it.
static RELAXED: AtomicBool = AtomicBool::new(cfg!(test));

/// Flushes `file`'s data and metadata, or a directory's entries when `file` is an open directory,
/// to stable storage using this process's sync mode.
pub fn sync(file: &File) -> io::Result<()> {
    sync_with(SyncMode::current(), file)
}

/// Opens directory `path` and flushes its entries, making completed creations, renames and
/// removals in it durable.
pub fn sync_directory(path: &std::path::Path) -> io::Result<()> {
    sync(&File::open(path)?)
}

/// Switches this process to the relaxed flush for the rest of its life. Only test support calls
/// this; production processes never do.
#[cfg(any(test, feature = "test-support"))]
pub fn relax_sync_for_tests() {
    RELAXED.store(true, Ordering::Relaxed);
}

/// How a flush reaches stable storage.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum SyncMode {
    /// The platform's strongest flush: `F_FULLFSYNC` on macOS, `fsync` on Linux.
    Full,

    /// A plain `fsync`, which on macOS leaves data in the drive's cache.
    Relaxed,
}

impl SyncMode {
    fn current() -> Self {
        if RELAXED.load(Ordering::Relaxed) {
            Self::Relaxed
        } else {
            Self::Full
        }
    }
}

fn sync_with(mode: SyncMode, file: &File) -> io::Result<()> {
    match mode {
        SyncMode::Full => file.sync_all(),
        SyncMode::Relaxed => loop {
            // SAFETY: the descriptor stays open for the call because `file` is borrowed.
            if unsafe { libc::fsync(file.as_raw_fd()) } == 0 {
                return Ok(());
            }
            let error = io::Error::last_os_error();
            // Retried like the standard library's own sync, so a signal is not a failure.
            if error.kind() != io::ErrorKind::Interrupted {
                return Err(error);
            }
        },
    }
}

#[cfg(test)]
mod tests {
    use std::io::Write;

    use super::*;

    #[test]
    fn both_modes_flush_written_files_and_directories() {
        let directory = tempfile::tempdir().expect("temporary directory");
        let path = directory.path().join("state.pb");
        let mut file = File::create(&path).expect("file");
        file.write_all(b"state").expect("write");

        for mode in [SyncMode::Full, SyncMode::Relaxed] {
            sync_with(mode, &file).expect("file sync");
            sync_with(mode, &File::open(directory.path()).expect("directory"))
                .expect("directory sync");
        }
        assert_eq!(std::fs::read(&path).expect("read back"), b"state");
    }

    #[test]
    fn both_modes_report_a_failed_flush() {
        let (_reader, writer) = std::io::pipe().expect("pipe");
        let writer = File::from(std::os::fd::OwnedFd::from(writer));

        // macOS refuses `F_FULLFSYNC` on a pipe as a bad descriptor; `fsync` refuses it as invalid.
        let full_error = if cfg!(target_os = "macos") {
            libc::EBADF
        } else {
            libc::EINVAL
        };
        for (mode, expected) in [
            (SyncMode::Full, full_error),
            (SyncMode::Relaxed, libc::EINVAL),
        ] {
            let error = sync_with(mode, &writer).expect_err("a pipe cannot be flushed");
            assert_eq!(error.raw_os_error(), Some(expected), "{mode:?}: {error:?}");
        }
    }

    #[test]
    fn unit_tests_start_relaxed() {
        assert_eq!(SyncMode::current(), SyncMode::Relaxed);
    }
}
