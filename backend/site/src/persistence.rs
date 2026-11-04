//! Atomic replacement of canonical desired site YAML.

use std::os::unix::fs::OpenOptionsExt;
use std::{
    fs::{self, File, OpenOptions},
    io::Write,
    path::Path,
};

use anyhow::anyhow;

pub trait SiteFileWriter: Send + Sync {
    fn replace(&self, path: &Path, contents: &str) -> Result<(), SiteStorageError>;
}

pub struct AtomicSiteFileWriter;

impl SiteFileWriter for AtomicSiteFileWriter {
    fn replace(&self, path: &Path, contents: &str) -> Result<(), SiteStorageError> {
        let parent = Self::parent_directory(path)?;
        let temporary = Self::temporary_path(parent, path)?;
        let write_result = Self::persist_replacement(parent, path, &temporary, contents);
        if write_result.is_err() {
            let _ = fs::remove_file(&temporary);
        }
        write_result
    }
}

#[derive(Debug, thiserror::Error)]
pub enum SiteStorageError {
    #[error("site storage failed: {0}")]
    Storage(#[source] anyhow::Error),

    #[error("site storage outcome is uncertain: {0}")]
    OutcomeUncertain(#[source] anyhow::Error),
}

impl AtomicSiteFileWriter {
    fn parent_directory(path: &Path) -> Result<&Path, SiteStorageError> {
        path.parent().ok_or_else(|| {
            SiteStorageError::Storage(anyhow!("site.yaml path has no parent directory"))
        })
    }

    fn temporary_path(parent: &Path, path: &Path) -> Result<std::path::PathBuf, SiteStorageError> {
        static SEQUENCE: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
        let file_name = path
            .file_name()
            .ok_or_else(|| SiteStorageError::Storage(anyhow!("site.yaml path has no file name")))?;
        Ok(parent.join(format!(
            ".{}.{}.tmp",
            file_name.to_string_lossy(),
            SEQUENCE.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        )))
    }

    fn persist_replacement(
        parent: &Path,
        path: &Path,
        temporary: &Path,
        contents: &str,
    ) -> Result<(), SiteStorageError> {
        let mut options = OpenOptions::new();
        options.create_new(true).write(true);
        options.mode(0o600);
        let mut file = options.open(temporary).map_err(|error| {
            SiteStorageError::Storage(
                anyhow::Error::new(error).context("could not create site.yaml replacement"),
            )
        })?;
        Self::write_and_sync_replacement(&mut file, contents)?;
        Self::rename_replacement(temporary, path)?;
        Self::confirm_replacement(parent)
    }

    fn write_and_sync_replacement(file: &mut File, contents: &str) -> Result<(), SiteStorageError> {
        // Replacement contents must be durable before their name replaces site.yaml.
        file.write_all(contents.as_bytes())
            .and_then(|_| dotcore_domain::fs_sync::sync(file))
            .map_err(|error| {
                SiteStorageError::Storage(
                    anyhow::Error::new(error).context("could not write site.yaml replacement"),
                )
            })
    }

    fn rename_replacement(temporary: &Path, path: &Path) -> Result<(), SiteStorageError> {
        fs::rename(temporary, path).map_err(|error| {
            SiteStorageError::Storage(
                anyhow::Error::new(error).context("could not atomically replace site.yaml"),
            )
        })
    }

    fn confirm_replacement(parent: &Path) -> Result<(), SiteStorageError> {
        // The renamed site.yaml entry is durable only after syncing its parent directory.
        File::open(parent)
            .and_then(|directory| dotcore_domain::fs_sync::sync(&directory))
            .map_err(|error| {
                SiteStorageError::OutcomeUncertain(
                    anyhow::Error::new(error).context(
                        "could not confirm site.yaml parent directory sync after replacement",
                    ),
                )
            })
    }
}

#[cfg(test)]
mod tests {
    use std::os::unix::fs::PermissionsExt;

    use super::{AtomicSiteFileWriter, SiteFileWriter};

    #[test]
    fn replacement_creates_an_owner_only_site_file() {
        let directory = dotcore_domain::test_support::canonical_temp_dir().join(format!(
            "dotcore-site-permissions-{}",
            dotcore_domain::test_support::unique_suffix()
        ));
        std::fs::create_dir(&directory).expect("fixture directory");
        let path = directory.join("site.yaml");
        AtomicSiteFileWriter
            .replace(&path, "siteId: site-one\n")
            .expect("atomic replacement");
        assert_eq!(
            std::fs::metadata(&path)
                .expect("site metadata")
                .permissions()
                .mode()
                & 0o777,
            0o600
        );
        std::fs::remove_dir_all(directory).expect("fixture cleanup");
    }
}
