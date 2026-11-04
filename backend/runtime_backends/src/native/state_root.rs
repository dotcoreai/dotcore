//! Locates the native runtime's workload state inside a site directory, at
//! `<site dir>/runtime/native`, beside the node store rather than inside it, so runtime custody
//! and node-store custody never share a directory.
//!
//! Each execution target keeps its workloads in its own directory below the root, for example
//! `runtime/native/mac-gpu/`, readable only by the current user. The root's canonical path is part
//! of every workload's ownership evidence, so a copied or moved site directory never claims the
//! original's workloads.

use std::{
    fs, io,
    os::unix::fs::DirBuilderExt,
    path::{Path, PathBuf},
};

use dotcore_domain::valid_id;

/// Directory of runtime-owned state beneath a site root.
const RUNTIME_DIRECTORY: &str = "runtime";

/// Directory of native workload state beneath the runtime directory.
const NATIVE_DIRECTORY: &str = "native";

/// Location of a site's native workload state, `<site dir>/runtime/native`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NativeStateRoot {
    path: PathBuf,
}

impl NativeStateRoot {
    /// Returns the native state root of the site directory `site_root`, without touching disk.
    pub fn for_site(site_root: impl AsRef<Path>) -> Self {
        Self {
            path: site_root
                .as_ref()
                .join(RUNTIME_DIRECTORY)
                .join(NATIVE_DIRECTORY),
        }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Creates the root and one execution target's directory, accessible only by the current
    /// user, and resolves the root's canonical path that ownership evidence records.
    pub fn open(&self, execution_target_id: &str) -> Result<OpenedStateRoot, NativeStateRootError> {
        if !valid_id(execution_target_id) {
            return Err(NativeStateRootError::InvalidExecutionTarget {
                execution_target_id: execution_target_id.to_owned(),
            });
        }
        let io_error = |context, source| NativeStateRootError::Io {
            context,
            path: self.path.clone(),
            source,
        };
        let mut builder = fs::DirBuilder::new();
        builder.recursive(true).mode(0o700);
        builder
            .create(self.path.join(execution_target_id))
            .map_err(|source| io_error("create native workload state directory", source))?;
        let canonical = fs::canonicalize(&self.path)
            .map_err(|source| io_error("resolve native workload state root", source))?;
        let Some(canonical_root) = canonical.to_str().map(str::to_owned) else {
            return Err(NativeStateRootError::NonUtf8 { path: canonical });
        };
        Ok(OpenedStateRoot {
            target_directory: canonical.join(execution_target_id),
            canonical_root,
        })
    }
}

/// Native state root resolved on disk for one execution target.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OpenedStateRoot {
    /// Canonical absolute path of the native state root, as ownership evidence records it.
    pub canonical_root: String,
    /// Directory holding the execution target's workload directories.
    pub target_directory: PathBuf,
}

/// Native workload state root that cannot be prepared.
#[derive(Debug, thiserror::Error)]
pub enum NativeStateRootError {
    #[error(
        "native workload state requires a valid execution target id, got {execution_target_id:?}"
    )]
    InvalidExecutionTarget { execution_target_id: String },

    #[error("{context} at {}", path.display())]
    Io {
        context: &'static str,
        path: PathBuf,
        #[source]
        source: io::Error,
    },

    #[error("native workload state root {} is not a UTF-8 path", path.display())]
    NonUtf8 { path: PathBuf },
}

#[cfg(test)]
mod tests {
    use std::os::unix::fs::PermissionsExt;

    use super::*;

    #[test]
    fn the_root_sits_beside_the_node_store_in_the_site_directory() {
        let root = NativeStateRoot::for_site("/sites/home");

        assert_eq!(root.path(), Path::new("/sites/home/runtime/native"));
    }

    #[test]
    fn opening_creates_private_directories_and_resolves_the_canonical_root() {
        let site = tempfile::tempdir().expect("Site directory");
        let root = NativeStateRoot::for_site(site.path());

        let opened = root.open("mac-gpu").expect("open state root");

        let canonical = fs::canonicalize(site.path())
            .expect("canonical site directory")
            .join("runtime/native");
        assert_eq!(opened.canonical_root, canonical.to_str().expect("UTF-8"));
        assert_eq!(opened.target_directory, canonical.join("mac-gpu"));
        for directory in [
            site.path().join("runtime"),
            site.path().join("runtime/native"),
            site.path().join("runtime/native/mac-gpu"),
        ] {
            let mode = fs::metadata(&directory)
                .expect("directory metadata")
                .permissions()
                .mode();
            assert_eq!(mode & 0o777, 0o700, "{}", directory.display());
        }
        assert_eq!(root.open("mac-gpu").expect("reopen"), opened);
    }

    #[test]
    fn opening_rejects_target_ids_that_are_not_one_directory_name() {
        let site = tempfile::tempdir().expect("Site directory");
        let root = NativeStateRoot::for_site(site.path());

        for target in ["", "..", "a/b", "Mac"] {
            assert!(
                matches!(
                    root.open(target),
                    Err(NativeStateRootError::InvalidExecutionTarget { .. })
                ),
                "{target:?}"
            );
        }
        assert!(!site.path().join("runtime").exists());
    }
}
