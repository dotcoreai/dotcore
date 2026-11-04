//! Checks whether this host can run native workloads: the launcher binary is installed, the
//! workload state root is writable, `nix` is available for engines built from Nix packages, and
//! the host platform matches the execution target's platform.
//!
//! The checks are cheap and never run `nix build`, so a node can repeat them on every
//! observation cycle. Each failure carries a stable code and an actionable message.

use std::{
    fs,
    os::unix::fs::DirBuilderExt as _,
    path::{Path, PathBuf},
};

use dotcore_domain::ExecutionPlatform;
use dotcore_runtime::{NativeCapabilityProbe, NativeCapabilityReport};

use super::{NativeStateRoot, resolve_launcher};

/// File created and removed inside the state root to prove it is writable.
const WRITE_CHECK_FILE: &str = ".dotcore-probe";

/// Host-local checks for one site's native execution targets.
#[derive(Clone, Debug)]
pub struct NativeProbe {
    state_root: NativeStateRoot,
    launcher: Option<PathBuf>,
    host_platform: Option<ExecutionPlatform>,
    find_nix: fn() -> Option<PathBuf>,
}

impl NativeProbe {
    /// Probes under `state_root`, with the launcher beside the server executable unless
    /// `launcher` names one, and `nix` found by the shared Nix discovery.
    pub fn new(state_root: NativeStateRoot, launcher: Option<PathBuf>) -> Self {
        Self {
            state_root,
            launcher,
            host_platform: host_platform(),
            find_nix: dotcore_domain::nix::discover_nix,
        }
    }

    /// Reports `platform` as the host platform instead of the detected one, for tests.
    #[cfg(test)]
    fn with_host_platform(mut self, platform: Option<ExecutionPlatform>) -> Self {
        self.host_platform = platform;
        self
    }

    /// Finds the Nix executable with `find_nix` instead of the host search, for tests.
    #[cfg(test)]
    fn with_nix_discovery(mut self, find_nix: fn() -> Option<PathBuf>) -> Self {
        self.find_nix = find_nix;
        self
    }
}

impl NativeCapabilityProbe for NativeProbe {
    fn probe(&self) -> NativeCapabilityReport {
        NativeCapabilityReport {
            host_platform: self.host_platform.clone(),
            launcher: resolve_launcher(self.launcher.as_deref())
                .map(drop)
                .map_err(|error| error.to_string()),
            state_root: check_state_root(self.state_root.path()),
            nix: match (self.find_nix)() {
                Some(_) => Ok(()),
                None => Err(format!(
                    "Nix was not found in {}; install Nix on this node",
                    dotcore_domain::nix::search_locations()
                )),
            },
        }
    }
}

fn host_platform() -> Option<ExecutionPlatform> {
    let operating_system = match std::env::consts::OS {
        "macos" => "darwin",
        other => other,
    };
    ExecutionPlatform::normalize(operating_system, std::env::consts::ARCH).ok()
}

/// Creates the state root when missing, then proves it accepts a new file.
fn check_state_root(path: &Path) -> Result<(), String> {
    let io_error = |action: &str, error: std::io::Error| {
        format!(
            "cannot {action} at {}: {error}; make the directory writable by the server user",
            path.display()
        )
    };
    fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(path)
        .map_err(|error| io_error("create the state directory", error))?;
    let check_file = path.join(WRITE_CHECK_FILE);
    fs::write(&check_file, b"").map_err(|error| io_error("write to the state directory", error))?;
    fs::remove_file(&check_file).map_err(|error| io_error("clean the state directory", error))
}

#[cfg(test)]
mod tests {
    use std::os::unix::fs::PermissionsExt as _;

    use super::*;

    fn executable(directory: &Path) -> PathBuf {
        let path = directory.join("dotcore-launcher");
        fs::write(&path, "#!/bin/sh\n").expect("launcher");
        fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).expect("permissions");
        path
    }

    fn some_nix() -> Option<PathBuf> {
        Some(PathBuf::from("/fake/nix"))
    }

    fn no_nix() -> Option<PathBuf> {
        None
    }

    fn probe(directory: &Path) -> NativeProbe {
        NativeProbe::new(
            NativeStateRoot::for_site(directory.join("site")),
            Some(executable(directory)),
        )
        .with_host_platform(Some(ExecutionPlatform::darwin_arm64()))
        .with_nix_discovery(some_nix)
    }

    #[test]
    fn ready_host_qualifies_and_creates_the_state_root() {
        let directory = tempfile::tempdir().expect("directory");
        let probe = probe(directory.path());

        let report = probe.probe();

        assert_eq!(report.qualify(&ExecutionPlatform::darwin_arm64()), Ok(()));
        let root = directory.path().join("site/runtime/native");
        assert!(root.is_dir());
        assert!(!root.join(WRITE_CHECK_FILE).exists());
    }

    #[test]
    fn platform_mismatch_is_reported_first() {
        let directory = tempfile::tempdir().expect("directory");
        let probe = probe(directory.path()).with_nix_discovery(no_nix);

        let failure = probe
            .probe()
            .qualify(&ExecutionPlatform::linux_arm64())
            .expect_err("platform mismatch");

        assert_eq!(failure.code, "PLATFORM_UNSUPPORTED");
        assert!(failure.platform_mismatch);
        assert_eq!(
            failure.message,
            "Native workloads run on this node's darwin/arm64 platform, but the execution target requires linux/arm64."
        );
    }

    #[test]
    fn missing_launcher_names_the_expected_path() {
        let directory = tempfile::tempdir().expect("directory");
        let missing = directory.path().join("absent-launcher");
        let probe = NativeProbe::new(
            NativeStateRoot::for_site(directory.path().join("site")),
            Some(missing.clone()),
        )
        .with_host_platform(Some(ExecutionPlatform::darwin_arm64()))
        .with_nix_discovery(some_nix);

        let failure = probe
            .probe()
            .qualify(&ExecutionPlatform::darwin_arm64())
            .expect_err("missing launcher");

        assert_eq!(failure.code, "LAUNCHER_UNAVAILABLE");
        assert!(!failure.platform_mismatch);
        assert_eq!(
            failure.message,
            format!(
                "The native launcher is unavailable on this node: launcher binary {} is missing; install dotcore-launcher beside the server executable, or build it with `cargo build -p dotcore-launcher` in development.",
                missing.display()
            )
        );
    }

    #[test]
    fn unwritable_state_root_names_its_location() {
        let directory = tempfile::tempdir().expect("directory");
        let probe = probe(directory.path());
        // A file where the site directory belongs makes the root impossible to create.
        fs::write(directory.path().join("site"), "").expect("blocking file");

        let failure = probe
            .probe()
            .qualify(&ExecutionPlatform::darwin_arm64())
            .expect_err("unwritable state root");

        assert_eq!(failure.code, "STATE_ROOT_UNWRITABLE");
        let root = directory.path().join("site/runtime/native");
        assert!(
            failure.message.contains(&root.display().to_string())
                && failure
                    .message
                    .contains("make the directory writable by the server user"),
            "{}",
            failure.message
        );
    }

    #[test]
    fn missing_nix_lists_where_it_was_searched() {
        let directory = tempfile::tempdir().expect("directory");
        let probe = probe(directory.path()).with_nix_discovery(no_nix);

        let failure = probe
            .probe()
            .qualify(&ExecutionPlatform::darwin_arm64())
            .expect_err("missing nix");

        assert_eq!(failure.code, "NIX_UNAVAILABLE");
        assert_eq!(
            failure.message,
            format!(
                "Nix is unavailable on this node: Nix was not found in {}; install Nix on this node.",
                dotcore_domain::nix::search_locations()
            )
        );
    }
}
