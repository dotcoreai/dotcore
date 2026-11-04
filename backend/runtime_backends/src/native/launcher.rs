//! Finds the `dotcore-launcher` binary that holds every native workload process. Packaged and
//! development builds install it in the same directory as the server executable, so by default
//! it is looked up beside the server's real executable path, for example
//! `/nix/store/<hash>-dotcore/bin/dotcore-launcher`; tests name an explicit path instead.
//!
//! Resolution only checks that the binary exists and is executable, so a missing installation
//! fails up front with the expected path rather than at the first workload start.

use std::{
    fs, io,
    os::unix::fs::PermissionsExt,
    path::{Path, PathBuf},
};

/// Returns the launcher binary at `explicit` when given, otherwise the one beside the current
/// executable, once it is an executable file.
pub fn resolve_launcher(explicit: Option<&Path>) -> Result<PathBuf, NativeLauncherError> {
    let path = match explicit {
        Some(path) => path.to_path_buf(),
        None => dotcore_launcher::supervisor::launcher_beside_current_exe()
            .map_err(|source| NativeLauncherError::CurrentExecutable { source })?,
    };
    let metadata = match fs::metadata(&path) {
        Ok(metadata) => metadata,
        Err(source) if source.kind() == io::ErrorKind::NotFound => {
            return Err(NativeLauncherError::Missing { path });
        }
        Err(source) => return Err(NativeLauncherError::Unreadable { path, source }),
    };
    if !metadata.is_file() || metadata.permissions().mode() & 0o111 == 0 {
        return Err(NativeLauncherError::NotExecutable { path });
    }
    Ok(path)
}

/// Failure to find a usable launcher binary.
#[derive(Debug, thiserror::Error)]
pub enum NativeLauncherError {
    #[error("cannot resolve the server executable path to find dotcore-launcher beside it")]
    CurrentExecutable { source: io::Error },

    #[error(
        "launcher binary {} is missing; install dotcore-launcher beside the server executable, or build it with `cargo build -p dotcore-launcher` in development",
        path.display()
    )]
    Missing { path: PathBuf },

    #[error("launcher binary {} cannot be inspected", path.display())]
    Unreadable { path: PathBuf, source: io::Error },

    #[error(
        "launcher binary {} is not an executable file; reinstall dotcore-launcher beside the server executable",
        path.display()
    )]
    NotExecutable { path: PathBuf },
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn explicit_executable_launcher_is_returned_unchanged() {
        let directory = tempfile::tempdir_in(dotcore_domain::test_support::canonical_temp_dir())
            .expect("temporary directory");
        let launcher = directory.path().join("dotcore-launcher");
        fs::write(&launcher, b"#!/bin/sh\n").expect("launcher file");
        fs::set_permissions(&launcher, fs::Permissions::from_mode(0o755)).expect("permissions");

        assert_eq!(
            resolve_launcher(Some(&launcher)).expect("launcher"),
            launcher
        );
    }

    #[test]
    fn missing_or_non_executable_launchers_fail_with_their_path() {
        let directory = tempfile::tempdir_in(dotcore_domain::test_support::canonical_temp_dir())
            .expect("temporary directory");
        let missing = directory.path().join("dotcore-launcher");
        let error = resolve_launcher(Some(&missing)).expect_err("missing launcher");
        assert_eq!(
            error.to_string(),
            format!(
                "launcher binary {} is missing; install dotcore-launcher beside the server executable, or build it with `cargo build -p dotcore-launcher` in development",
                missing.display()
            )
        );

        fs::write(&missing, b"data").expect("plain file");
        fs::set_permissions(&missing, fs::Permissions::from_mode(0o644)).expect("permissions");
        assert!(matches!(
            resolve_launcher(Some(&missing)),
            Err(NativeLauncherError::NotExecutable { path }) if path == missing
        ));
        assert!(matches!(
            resolve_launcher(Some(directory.path())),
            Err(NativeLauncherError::NotExecutable { .. })
        ));
    }

    #[test]
    fn default_launcher_is_looked_up_beside_the_resolved_current_executable() {
        let executable = fs::canonicalize(std::env::current_exe().expect("test executable"))
            .expect("canonical test executable");
        let expected = executable
            .parent()
            .expect("executable directory")
            .join("dotcore-launcher");

        // The test executable lives in Cargo's `deps` directory, where no launcher is built.
        let error = resolve_launcher(None).expect_err("no launcher beside the test executable");

        assert!(matches!(error, NativeLauncherError::Missing { path } if path == expected));
    }
}
