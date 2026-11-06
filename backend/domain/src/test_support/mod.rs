//! Test support for the domain primitives, compiled for this crate's unit tests and, through the
//! `test-support` feature, for the unit tests of the crates above it.

pub mod child;
pub mod host_resolver;
pub mod model;
pub mod nix;
pub mod oci;
pub mod ports;
pub mod tracing;

/// Returns the system temporary directory with symlinks resolved, because the product rejects
/// symlinked ancestors and macOS `TMPDIR` lives under the `/var` symlink. Every fixture directory
/// starts here, so it also switches the test process to the relaxed file sync: this crate's own
/// unit-test default does not reach the tests of the crates above.
pub fn canonical_temp_dir() -> std::path::PathBuf {
    crate::fs_sync::relax_sync_for_tests();
    std::fs::canonicalize(std::env::temp_dir()).expect("canonical temporary directory")
}

/// Creates a temporary directory under [`canonical_temp_dir`], removed on drop.
pub fn canonical_tempdir() -> std::io::Result<tempfile::TempDir> {
    tempfile::tempdir_in(canonical_temp_dir())
}

/// Returns a suffix that is unique across processes and within one, for fixture names. The
/// clock alone is not enough: macOS wall time has microsecond granularity, and each test runs in
/// its own process whose sequence starts at zero, so the process ID separates parallel tests.
pub fn unique_suffix() -> String {
    static SEQUENCE: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("system time")
        .as_nanos();
    format!(
        "{}-{nanos}-{}",
        std::process::id(),
        SEQUENCE.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
    )
}
