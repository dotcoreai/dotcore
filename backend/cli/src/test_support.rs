//! Shared support for CLI unit tests.
//!
//! Provides a temporary directory root that matches what the product accepts on every host, fake
//! executables that parallel tests can run safely, and a scripted site runtime that records what
//! site delete and reset ask of it.

/// Returns the system temporary directory with symlinks resolved, because the product rejects
/// symlinked ancestors and macOS `TMPDIR` lives under the `/var` symlink. Every fixture directory
/// starts here, so it also switches the test process to the relaxed file sync: the backend's own
/// unit-test default does not reach this crate's tests.
pub(crate) fn canonical_temp_dir() -> std::path::PathBuf {
    dotcore_backend::testing::relax_sync_for_tests();
    std::fs::canonicalize(std::env::temp_dir()).expect("canonical temporary directory")
}

/// Creates a temporary directory under the canonical system temporary directory, removed on drop.
pub(crate) fn canonical_tempdir() -> std::io::Result<tempfile::TempDir> {
    tempfile::tempdir_in(canonical_temp_dir())
}

/// Writes an executable script at `path` with `mode` through a child process, so this test
/// process never holds a write descriptor on it. A process another parallel test forks while
/// such a descriptor is open keeps a copy until its own exec, and running the script meanwhile
/// fails as busy.
pub(crate) fn write_executable_script(path: &std::path::Path, contents: &str, mode: u32) {
    use std::io::Write as _;

    let mut writer = std::process::Command::new("/bin/sh")
        .args(["-c", r#"cat > "$1" && chmod "$2" "$1""#, "sh"])
        .arg(path)
        .arg(format!("{mode:o}"))
        .stdin(std::process::Stdio::piped())
        .spawn()
        .expect("spawn the script writer");
    writer
        .stdin
        .take()
        .expect("script writer input")
        .write_all(contents.as_bytes())
        .expect("send the script");
    let status = writer.wait().expect("wait for the script writer");
    assert!(status.success(), "script writer failed: {status}");
}

/// Returns a suffix that is unique across processes and within one, for fixture names. The
/// clock alone is not enough: macOS wall time has microsecond granularity, and each test runs in
/// its own process whose sequence starts at zero, so the process ID separates parallel tests.
pub(crate) fn unique_suffix() -> String {
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

/// Scripted site runtime cleanup whose clones share one record of resources and calls, so a
/// test keeps a handle while the command under test owns a boxed copy.
#[derive(Clone, Default)]
pub(crate) struct FakeSiteRuntime {
    state: std::sync::Arc<std::sync::Mutex<FakeSiteRuntimeState>>,
}

#[derive(Default)]
struct FakeSiteRuntimeState {
    resources: Vec<String>,
    calls: Vec<String>,
    fail_cleanup: bool,
}

impl FakeSiteRuntime {
    /// Creates a runtime that owns resources with these descriptions for every site id.
    pub(crate) fn with_resources(resources: &[&str]) -> Self {
        let runtime = Self::default();
        runtime.state().resources = resources.iter().map(|name| (*name).to_owned()).collect();
        runtime
    }

    /// Makes every later cleanup fail without removing anything.
    pub(crate) fn fail_cleanup(&self) {
        self.state().fail_cleanup = true;
    }

    /// Returns the calls made so far, as `inventory:<site id>` and `cleanup:<site id>`.
    pub(crate) fn calls(&self) -> Vec<String> {
        self.state().calls.clone()
    }

    /// Returns the descriptions of the resources still owned.
    pub(crate) fn resources(&self) -> Vec<String> {
        self.state().resources.clone()
    }

    fn state(&self) -> std::sync::MutexGuard<'_, FakeSiteRuntimeState> {
        self.state.lock().expect("fake site runtime state")
    }
}

impl dotcore_runtime::SiteRuntimeCleanup for FakeSiteRuntime {
    fn inventory<'a>(
        &'a self,
        site_id: &'a str,
        _scope: &'a dotcore_runtime::SiteRuntimeScope,
    ) -> std::pin::Pin<
        Box<
            dyn std::future::Future<Output = anyhow::Result<dotcore_runtime::SiteRuntimeInventory>>
                + Send
                + 'a,
        >,
    > {
        let mut state = self.state();
        state.calls.push(format!("inventory:{site_id}"));
        let resources = state
            .resources
            .iter()
            .map(|description| dotcore_runtime::SiteRuntimeResource {
                runtime: "Test Runtime",
                description: description.clone(),
            })
            .collect();
        Box::pin(std::future::ready(Ok(
            dotcore_runtime::SiteRuntimeInventory {
                resources,
                skipped: Vec::new(),
            },
        )))
    }

    fn cleanup<'a>(
        &'a self,
        site_id: &'a str,
        _scope: &'a dotcore_runtime::SiteRuntimeScope,
    ) -> std::pin::Pin<
        Box<
            dyn std::future::Future<
                    Output = anyhow::Result<dotcore_runtime::SiteRuntimeCleanupReport>,
                > + Send
                + 'a,
        >,
    > {
        let mut state = self.state();
        state.calls.push(format!("cleanup:{site_id}"));
        let result = if state.fail_cleanup {
            Err(anyhow::anyhow!("injected cleanup failure"))
        } else {
            state.resources.clear();
            Ok(dotcore_runtime::SiteRuntimeCleanupReport::default())
        };
        Box::pin(std::future::ready(result))
    }
}
