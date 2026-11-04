//! Validated filesystem paths used to compose one local site.
//!
//! Every recreatable Dotcore cache lives below one cache root, for example
//! `~/.cache/dotcore`, with one fixed subfolder per owner such as
//! `model-artifacts/v1` or `workload-cache/v2`. Only the root is configurable.

use std::path::PathBuf;

use anyhow::anyhow;

/// Environment variable containing the writable local site root.
pub const SITE_DIR: &str = "DOTCORE_SITE_DIR";

/// Environment variable containing the immutable platform catalog root.
pub const CATALOG_DIR: &str = "DOTCORE_CATALOG_DIR";

/// Environment variable containing the root of every recreatable Dotcore cache.
pub const CACHE_DIR: &str = "DOTCORE_CACHE_DIR";

/// Environment variable containing the XDG persistent-state base directory.
pub const XDG_STATE_HOME: &str = "XDG_STATE_HOME";

/// Environment variable containing the XDG cache base directory.
pub const XDG_CACHE_HOME: &str = "XDG_CACHE_HOME";

/// Environment variable containing the current user's home directory.
pub const HOME: &str = "HOME";

/// Immutable path inputs used by backend composition.
///
/// The paths locate inputs. They do not own catalog declarations, cache
/// inventory, site store records, or runtime policy.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SitePaths {
    pub root: PathBuf,
    /// Absent only for a joined node, which never loads a catalog.
    pub master_catalog_root: Option<PathBuf>,
    pub cache_root: PathBuf,
}

impl SitePaths {
    /// Builds site paths from explicit environment-equivalent values.
    pub fn from_values(
        site_root: Option<&str>,
        master_catalog_root: Option<&str>,
        cache_root: Option<&str>,
    ) -> SitePathsResult<Self> {
        Ok(Self {
            root: absolute_path(site_root, SITE_DIR)?,
            master_catalog_root: Some(absolute_path(master_catalog_root, CATALOG_DIR)?),
            cache_root: absolute_non_root_path(cache_root, CACHE_DIR)?,
        })
    }

    /// Resolves the writable site root from its override and XDG state inputs.
    pub fn resolve_site_root(
        site_root: Option<&str>,
        xdg_state_home: Option<&str>,
        home: Option<&str>,
    ) -> SitePathsResult<PathBuf> {
        resolve_mutable_path(
            site_root,
            SITE_DIR,
            xdg_state_home,
            XDG_STATE_HOME,
            home,
            &[".local", "state", "dotcore", "site"],
            &["dotcore", "site"],
        )
    }

    /// Resolves the Dotcore cache root from its override and XDG cache inputs.
    pub fn resolve_cache_root(
        cache_root: Option<&str>,
        xdg_cache_home: Option<&str>,
        home: Option<&str>,
    ) -> SitePathsResult<PathBuf> {
        let path = resolve_mutable_path(
            cache_root,
            CACHE_DIR,
            xdg_cache_home,
            XDG_CACHE_HOME,
            home,
            &[".cache", "dotcore"],
            &["dotcore"],
        )?;
        if path_components_are_root(&path) {
            return Err(SitePathsError::invalid(format!(
                "{CACHE_DIR} cannot be the filesystem root"
            )));
        }
        Ok(path)
    }

    /// Validates the optional master catalog root: absent stays absent, and a given value must
    /// be an absolute path.
    pub fn resolve_catalog_root(catalog_root: Option<&str>) -> SitePathsResult<Option<PathBuf>> {
        catalog_root
            .map(|value| absolute_path(Some(value), CATALOG_DIR))
            .transpose()
    }

    /// Resolves mutable paths from Dotcore overrides and XDG base directories.
    pub fn resolve_values(
        site_root: Option<&str>,
        master_catalog_root: Option<&str>,
        cache_root: Option<&str>,
        xdg_state_home: Option<&str>,
        xdg_cache_home: Option<&str>,
        home: Option<&str>,
    ) -> SitePathsResult<ResolvedSitePaths> {
        ResolvedSitePaths::from_values(
            site_root,
            master_catalog_root,
            cache_root,
            xdg_state_home,
            xdg_cache_home,
            home,
        )
    }

    /// Returns the artifact manager's model artifact cache below the cache root.
    pub fn model_artifact_cache_root(&self) -> PathBuf {
        CacheLayout(&self.cache_root).model_artifacts()
    }

    /// Returns the runtime's workload cache below the cache root.
    pub fn workload_cache_root(&self) -> PathBuf {
        CacheLayout(&self.cache_root).workloads()
    }
}

/// Effective process paths before the backend requires a master catalog.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ResolvedSitePaths {
    pub root: PathBuf,
    pub master_catalog_root: Option<PathBuf>,
    pub cache_root: PathBuf,
}

impl ResolvedSitePaths {
    /// Resolves paths with explicit Dotcore values taking precedence over XDG defaults.
    pub fn from_values(
        site_root: Option<&str>,
        master_catalog_root: Option<&str>,
        cache_root: Option<&str>,
        xdg_state_home: Option<&str>,
        xdg_cache_home: Option<&str>,
        home: Option<&str>,
    ) -> SitePathsResult<Self> {
        let root = SitePaths::resolve_site_root(site_root, xdg_state_home, home)?;
        let cache_root = SitePaths::resolve_cache_root(cache_root, xdg_cache_home, home)?;
        let master_catalog_root = SitePaths::resolve_catalog_root(master_catalog_root)?;

        Ok(Self {
            root,
            master_catalog_root,
            cache_root,
        })
    }

    /// Requires the explicit master catalog needed by backend composition.
    pub fn require_master_catalog(self) -> SitePathsResult<SitePaths> {
        let master_catalog_root = self
            .master_catalog_root
            .ok_or_else(|| SitePathsError::invalid(format!("{CATALOG_DIR} is not set")))?;
        Ok(SitePaths {
            root: self.root,
            master_catalog_root: Some(master_catalog_root),
            cache_root: self.cache_root,
        })
    }

    /// Drops the master catalog for a node that never loads a catalog.
    pub fn without_master_catalog(self) -> SitePaths {
        SitePaths {
            root: self.root,
            master_catalog_root: None,
            cache_root: self.cache_root,
        }
    }

    /// Returns the artifact manager's model artifact cache below the cache root.
    pub fn model_artifact_cache_root(&self) -> PathBuf {
        CacheLayout(&self.cache_root).model_artifacts()
    }

    /// Returns the runtime's workload cache below the cache root.
    pub fn workload_cache_root(&self) -> PathBuf {
        CacheLayout(&self.cache_root).workloads()
    }
}

/// Fixed owner subfolders below one Dotcore cache root.
#[derive(Clone, Copy, Debug)]
pub struct CacheLayout<'a>(pub &'a std::path::Path);

impl CacheLayout<'_> {
    /// Artifact-manager-owned model artifact cache.
    pub fn model_artifacts(self) -> PathBuf {
        self.0.join("model-artifacts").join("v1")
    }

    /// Runtime-owned node-local workload caches.
    pub fn workloads(self) -> PathBuf {
        self.0.join("workload-cache").join("v2")
    }

    /// Returns every workload cache folder one site may own: the current layout's, then the
    /// earlier layout's, which an older version may have left behind.
    pub fn site_workload_caches(self, site_id: &str) -> [PathBuf; 2] {
        [
            self.workloads().join(site_id),
            self.0.join("workload-cache").join("v1").join(site_id),
        ]
    }

    /// Development-only application data for development workloads.
    pub fn development_applications(self) -> PathBuf {
        self.0.join("development-applications")
    }
}

/// Errors raised while reading or validating site path inputs.
#[derive(Debug, thiserror::Error)]
pub enum SitePathsError {
    #[error("Invalid site path")]
    Invalid(#[source] anyhow::Error),
}

impl SitePathsError {
    fn invalid(message: impl Into<String>) -> Self {
        Self::Invalid(anyhow!(message.into()))
    }
}

/// Result type owned by the site path boundary.
pub type SitePathsResult<T> = Result<T, SitePathsError>;

fn absolute_path(value: Option<&str>, variable: &'static str) -> Result<PathBuf, SitePathsError> {
    let value = value.ok_or_else(|| SitePathsError::invalid(format!("{variable} is not set")))?;
    let path = PathBuf::from(value);
    if !path.is_absolute() {
        return Err(SitePathsError::invalid(format!(
            "{variable} must be an absolute path, got {value:?}"
        )));
    }
    Ok(path)
}

fn resolve_mutable_path(
    explicit: Option<&str>,
    explicit_variable: &'static str,
    xdg: Option<&str>,
    xdg_variable: &'static str,
    home: Option<&str>,
    home_suffix: &[&str],
    xdg_suffix: &[&str],
) -> Result<PathBuf, SitePathsError> {
    if let Some(explicit) = explicit {
        return absolute_path(Some(explicit), explicit_variable);
    }
    if let Some(xdg) = nonempty(xdg) {
        let mut path = absolute_path(Some(xdg), xdg_variable)?;
        path.extend(xdg_suffix);
        return Ok(path);
    }
    let home = nonempty(home).ok_or_else(|| {
        SitePathsError::invalid(format!(
            "cannot resolve {explicit_variable}: {xdg_variable} and HOME are unset or empty"
        ))
    })?;
    let mut path = absolute_path(Some(home), HOME)?;
    path.extend(home_suffix);
    Ok(path)
}

fn nonempty(value: Option<&str>) -> Option<&str> {
    value.filter(|value| !value.is_empty())
}

fn absolute_non_root_path(
    value: Option<&str>,
    variable: &'static str,
) -> Result<PathBuf, SitePathsError> {
    let path = absolute_path(value, variable)?;
    if path_components_are_root(&path) {
        return Err(SitePathsError::invalid(format!(
            "{variable} cannot be the filesystem root"
        )));
    }
    Ok(path)
}

fn path_components_are_root(path: &std::path::Path) -> bool {
    let mut depth = 0_usize;
    for component in path.components() {
        match component {
            std::path::Component::RootDir => {}
            std::path::Component::CurDir => {}
            std::path::Component::ParentDir => depth = depth.saturating_sub(1),
            std::path::Component::Normal(_) => depth += 1,
            std::path::Component::Prefix(_) => {}
        }
    }
    depth == 0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_catalog_root_is_optional_and_must_be_absolute() {
        assert_eq!(SitePaths::resolve_catalog_root(None).expect("absent"), None);
        assert_eq!(
            SitePaths::resolve_catalog_root(Some("/catalog")).expect("absolute"),
            Some(PathBuf::from("/catalog"))
        );
        for relative in ["catalog", ""] {
            assert_eq!(
                SitePaths::resolve_catalog_root(Some(relative))
                    .expect_err("relative")
                    .to_string(),
                "Invalid site path"
            );
        }
    }

    #[test]
    fn mutable_paths_follow_explicit_xdg_and_home_precedence() {
        let explicit = SitePaths::resolve_values(
            Some("/explicit/site"),
            Some("/catalog"),
            Some("/explicit/cache"),
            Some("/xdg/state"),
            Some("/xdg/cache"),
            Some("/home/user"),
        )
        .expect("explicit paths");
        assert_eq!(explicit.root, PathBuf::from("/explicit/site"));
        assert_eq!(
            explicit.master_catalog_root,
            Some(PathBuf::from("/catalog"))
        );
        assert_eq!(explicit.cache_root, PathBuf::from("/explicit/cache"));

        let xdg = SitePaths::resolve_values(
            None,
            None,
            None,
            Some("/xdg/state"),
            Some("/xdg/cache"),
            Some("/home/user"),
        )
        .expect("XDG paths");
        assert_eq!(xdg.root, PathBuf::from("/xdg/state/dotcore/site"));
        assert_eq!(xdg.master_catalog_root, None);
        assert_eq!(xdg.cache_root, PathBuf::from("/xdg/cache/dotcore"));
        assert_eq!(
            xdg.model_artifact_cache_root(),
            PathBuf::from("/xdg/cache/dotcore/model-artifacts/v1")
        );
        assert_eq!(
            xdg.workload_cache_root(),
            PathBuf::from("/xdg/cache/dotcore/workload-cache/v2")
        );

        assert_eq!(
            CacheLayout(&xdg.cache_root).site_workload_caches("site-one"),
            [
                PathBuf::from("/xdg/cache/dotcore/workload-cache/v2/site-one"),
                PathBuf::from("/xdg/cache/dotcore/workload-cache/v1/site-one"),
            ]
        );

        let home =
            SitePaths::resolve_values(None, None, None, Some(""), Some(""), Some("/home/user"))
                .expect("HOME paths");
        assert_eq!(
            home.root,
            PathBuf::from("/home/user/.local/state/dotcore/site")
        );
        assert_eq!(home.cache_root, PathBuf::from("/home/user/.cache/dotcore"));
    }

    #[test]
    fn cache_defaults_use_supplied_linux_and_macos_home_values() {
        for home in ["/home/user", "/Users/user"] {
            for xdg_cache_home in [None, Some("")] {
                let paths = SitePaths::resolve_values(
                    Some("/site"),
                    None,
                    None,
                    None,
                    xdg_cache_home,
                    Some(home),
                )
                .expect("HOME cache path");
                assert_eq!(
                    paths.cache_root,
                    PathBuf::from(format!("{home}/.cache/dotcore"))
                );
            }
        }

        let explicit = SitePaths::resolve_values(
            Some("/site"),
            None,
            Some("/explicit/cache"),
            None,
            Some("relative-cache"),
            None,
        )
        .expect("explicit cache path");
        assert_eq!(explicit.cache_root, PathBuf::from("/explicit/cache"));
    }

    #[test]
    fn mutable_path_base_errors_are_actionable() {
        let relative_xdg = SitePaths::resolve_values(
            None,
            None,
            Some("/cache"),
            Some("relative-state"),
            None,
            Some("/home/user"),
        )
        .expect_err("relative XDG state");
        assert_eq!(
            format!("{:#}", anyhow::Error::from(relative_xdg)),
            "Invalid site path: XDG_STATE_HOME must be an absolute path, got \"relative-state\""
        );

        let relative_home = SitePaths::resolve_values(
            None,
            None,
            Some("/cache"),
            None,
            None,
            Some("relative-home"),
        )
        .expect_err("relative HOME");
        assert_eq!(
            format!("{:#}", anyhow::Error::from(relative_home)),
            "Invalid site path: HOME must be an absolute path, got \"relative-home\""
        );

        let missing =
            SitePaths::resolve_values(None, None, Some("/cache"), Some(""), None, Some(""))
                .expect_err("missing state base");
        assert_eq!(
            format!("{:#}", anyhow::Error::from(missing)),
            "Invalid site path: cannot resolve DOTCORE_SITE_DIR: XDG_STATE_HOME and HOME are unset or empty"
        );
    }

    #[test]
    fn path_errors_name_directory_environment_variables() {
        let missing_site = SitePaths::from_values(None, None, None).expect_err("Site directory");
        assert_eq!(
            format!("{:#}", anyhow::Error::from(missing_site)),
            "Invalid site path: DOTCORE_SITE_DIR is not set"
        );

        let missing_catalog =
            SitePaths::from_values(Some("/site"), None, None).expect_err("Catalog directory");
        assert_eq!(
            format!("{:#}", anyhow::Error::from(missing_catalog)),
            "Invalid site path: DOTCORE_CATALOG_DIR is not set"
        );

        let missing_cache = SitePaths::from_values(Some("/site"), Some("/catalog"), None)
            .expect_err("cache directory");
        assert_eq!(
            format!("{:#}", anyhow::Error::from(missing_cache)),
            "Invalid site path: DOTCORE_CACHE_DIR is not set"
        );
    }

    #[test]
    fn path_validation_rejects_root_cache_without_creating_it() {
        for root_alias in ["/", "/.", "/tmp/.."] {
            let root_cache =
                SitePaths::from_values(Some("/site"), Some("/catalog"), Some(root_alias))
                    .expect_err("root cache directory");
            assert_eq!(
                format!("{:#}", anyhow::Error::from(root_cache)),
                "Invalid site path: DOTCORE_CACHE_DIR cannot be the filesystem root"
            );
        }

        let cache = std::fs::canonicalize(std::env::temp_dir())
            .expect("canonical temporary directory")
            .join(format!("dotcore-site-paths-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&cache);
        let paths = SitePaths::from_values(
            Some("/site"),
            Some("/catalog"),
            Some(cache.to_str().expect("cache path")),
        )
        .expect("Site paths");
        assert_eq!(paths.cache_root, cache);
        assert!(!paths.cache_root.exists());
    }
}
