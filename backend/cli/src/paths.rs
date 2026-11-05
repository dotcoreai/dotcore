//! The site, catalog and cache folders every command can be pointed at, and the single place
//! that turns them into effective paths.
//!
//! Each folder comes from its option, else from its environment variable, else from the default
//! the site path resolver derives from the XDG state and cache directories or the home directory.
//! Values are validated only when a command asks for them, so an invalid variable cannot break a
//! command that never uses it.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result, anyhow, bail};
use clap::{
    Args,
    builder::{OsStringValueParser, TypedValueParser},
};
use dotcore::composition::PathOverrides;
use dotcore_domain::{
    CACHE_DIR, CATALOG_DIR, HOME, ResolvedSitePaths, SITE_DIR, SitePaths, XDG_CACHE_HOME,
    XDG_STATE_HOME,
};

use crate::config::environment_value;

/// Root options that take a value, as spelled on the command line.
pub(crate) const VALUE_OPTIONS: [&str; 3] = ["--site-dir", "--catalog-dir", "--cache-dir"];

/// Folder options accepted before or after any command; each is `None` when neither the option
/// nor its environment variable is set.
#[derive(Clone, Debug, Default, Args)]
pub struct RootPathArgs {
    /// Absolute site or node folder; defaults to $XDG_STATE_HOME/dotcore/site, else
    /// ~/.local/state/dotcore/site.
    #[arg(
        long,
        global = true,
        env = SITE_DIR,
        value_name = "PATH",
        value_parser = raw_path(),
        hide_env_values = true
    )]
    pub site_dir: Option<PathBuf>,

    /// Set by the parser when `--site-dir` was typed on the command line, as opposed to read from
    /// the environment.
    #[arg(skip)]
    pub(crate) site_dir_on_command_line: bool,

    /// Absolute master catalog folder.
    #[arg(
        long,
        global = true,
        env = CATALOG_DIR,
        value_name = "PATH",
        value_parser = raw_path(),
        hide_env_values = true
    )]
    pub catalog_dir: Option<PathBuf>,

    /// Absolute root of the recreatable caches; defaults to $XDG_CACHE_HOME/dotcore, else
    /// ~/.cache/dotcore.
    #[arg(
        long,
        global = true,
        env = CACHE_DIR,
        value_name = "PATH",
        value_parser = raw_path(),
        hide_env_values = true
    )]
    pub cache_dir: Option<PathBuf>,
}

impl RootPathArgs {
    /// Returns the site folder: the given one, else the default for this user.
    pub(crate) fn site_dir(&self) -> Result<PathBuf> {
        self.site_dir_in(&BaseDirectories::from_process()?)
    }

    /// Returns the site folder only when `--site-dir` was typed on the command line, for output
    /// that names a folder only when the reader's environment does not already supply it.
    pub(crate) fn command_line_site_dir(&self) -> Option<&Path> {
        self.site_dir
            .as_deref()
            .filter(|_| self.site_dir_on_command_line)
    }

    /// Returns the master catalog folder, if one was given.
    pub(crate) fn catalog_dir(&self) -> Result<Option<PathBuf>> {
        let catalog = explicit_value(self.catalog_dir.as_deref(), CATALOG_DIR)?;
        SitePaths::resolve_catalog_root(catalog.as_deref()).map_err(anyhow::Error::from)
    }

    /// Returns the master catalog folder, failing when none was given.
    pub(crate) fn master_catalog(&self) -> Result<PathBuf> {
        self.catalog_dir()?.ok_or_else(|| {
            anyhow!("no master catalog: pass --catalog-dir or set {CATALOG_DIR} to its folder")
        })
    }

    /// Returns the cache root: the given one, else the default for this user.
    pub(crate) fn cache_root(&self) -> Result<PathBuf> {
        self.cache_root_in(&BaseDirectories::from_process()?)
    }

    /// Returns the cache root when one is configured or can be derived, `None` when nothing is
    /// configured and no default location exists, and an error when a configured one is invalid,
    /// so a command that removes caches never silently skips them.
    pub(crate) fn configured_cache_root(&self) -> Result<Option<PathBuf>> {
        if self.cache_dir.is_some() {
            return self.cache_root().map(Some);
        }
        Ok(self.cache_root().ok())
    }

    /// Returns the site, catalog and cache folders together, as `config show` reports them.
    pub(crate) fn process_paths(&self) -> Result<ResolvedSitePaths> {
        self.process_paths_in(&BaseDirectories::from_process()?)
    }

    /// Returns the telemetry folder: its variable, else the site folder when one was given, else
    /// none.
    pub(crate) fn telemetry_dir(&self) -> Result<Option<PathBuf>> {
        let telemetry = environment_value(dotcore::telemetry::TELEMETRY_DIR)?;
        self.telemetry_dir_in(telemetry.as_deref())
    }

    /// Returns the given folders as overrides of the server's path variables.
    pub(crate) fn server_overrides(&self) -> Result<PathOverrides> {
        Ok(PathOverrides {
            site_dir: explicit_value(self.site_dir.as_deref(), SITE_DIR)?,
            catalog_dir: explicit_value(self.catalog_dir.as_deref(), CATALOG_DIR)?,
            cache_dir: explicit_value(self.cache_dir.as_deref(), CACHE_DIR)?,
        })
    }

    fn telemetry_dir_in(&self, telemetry: Option<&str>) -> Result<Option<PathBuf>> {
        let site = self
            .site_dir
            .is_some()
            .then(|| self.site_dir())
            .transpose()?;
        Ok(dotcore::telemetry::resolve_directory(
            telemetry.map(Path::new),
            site.as_deref(),
        ))
    }

    fn site_dir_in(&self, base: &BaseDirectories) -> Result<PathBuf> {
        let site = explicit_value(self.site_dir.as_deref(), SITE_DIR)?;
        SitePaths::resolve_site_root(
            site.as_deref(),
            base.xdg_state_home.as_deref(),
            base.home.as_deref(),
        )
        .map_err(anyhow::Error::from)
    }

    fn cache_root_in(&self, base: &BaseDirectories) -> Result<PathBuf> {
        let cache = explicit_value(self.cache_dir.as_deref(), CACHE_DIR)?;
        SitePaths::resolve_cache_root(
            cache.as_deref(),
            base.xdg_cache_home.as_deref(),
            base.home.as_deref(),
        )
        .map_err(anyhow::Error::from)
    }

    fn process_paths_in(&self, base: &BaseDirectories) -> Result<ResolvedSitePaths> {
        let site = explicit_value(self.site_dir.as_deref(), SITE_DIR)?;
        let catalog = explicit_value(self.catalog_dir.as_deref(), CATALOG_DIR)?;
        let cache = explicit_value(self.cache_dir.as_deref(), CACHE_DIR)?;
        SitePaths::resolve_values(
            site.as_deref(),
            catalog.as_deref(),
            cache.as_deref(),
            base.xdg_state_home.as_deref(),
            base.xdg_cache_home.as_deref(),
            base.home.as_deref(),
        )
        .map_err(anyhow::Error::from)
    }
}

/// Takes any text as a path, empty included, so the resolver reports an invalid one only to the
/// commands that use it.
fn raw_path() -> impl TypedValueParser<Value = PathBuf> {
    OsStringValueParser::new().map(PathBuf::from)
}

/// The process variables that locate the default folders.
struct BaseDirectories {
    xdg_state_home: Option<String>,
    xdg_cache_home: Option<String>,
    home: Option<String>,
}

impl BaseDirectories {
    fn from_process() -> Result<Self> {
        Ok(Self {
            xdg_state_home: environment_value(XDG_STATE_HOME)?,
            xdg_cache_home: environment_value(XDG_CACHE_HOME)?,
            home: environment_value(HOME)?,
        })
    }
}

/// Turns a given folder into the text the site path resolver validates. An absolute folder is
/// normalized, since site operations require that; a relative or empty one is passed through so
/// the resolver reports it.
fn explicit_value(path: Option<&Path>, variable: &'static str) -> Result<Option<String>> {
    let Some(path) = path else {
        return Ok(None);
    };
    let value = path
        .to_str()
        .with_context(|| format!("{variable} must be valid UTF-8"))?;
    if !path.is_absolute() {
        return Ok(Some(value.to_owned()));
    }
    let normalized = crate::site::parse_absolute_path(value)
        .map_err(|message| anyhow!("{variable} is invalid: {message}"))?;
    match normalized.to_str() {
        Some(normalized) => Ok(Some(normalized.to_owned())),
        None => bail!("{variable} must be valid UTF-8"),
    }
}

#[cfg(test)]
mod tests {
    use std::{ffi::OsString, os::unix::ffi::OsStringExt};

    use super::*;
    use crate::{Cli, Command};

    #[test]
    fn site_dir_parses_before_and_after_the_command() {
        let cases: [&[&str]; 4] = [
            &["dotc", "--site-dir", "/a", "node", "list"],
            &["dotc", "node", "list", "--site-dir", "/a"],
            &["dotc", "node", "--site-dir", "/a", "list"],
            &["dotc", "--site-dir=/a", "node", "list"],
        ];
        for arguments in cases {
            let cli = Cli::try_parse_from(arguments).expect("parses");
            assert!(matches!(cli.command, Command::Node(_)), "{arguments:?}");
            assert_eq!(
                cli.paths.site_dir,
                Some(PathBuf::from("/a")),
                "{arguments:?}"
            );
        }
    }

    #[test]
    fn every_root_path_parses_on_both_sides_of_a_nested_command() {
        let cli = Cli::try_parse_from([
            "dotc",
            "--catalog-dir",
            "/c",
            "dev",
            "workload",
            "inspect",
            "--cache-dir",
            "/k",
        ])
        .expect("parses");

        assert_eq!(cli.paths.catalog_dir, Some(PathBuf::from("/c")));
        assert_eq!(cli.paths.cache_dir, Some(PathBuf::from("/k")));

        let cli = Cli::try_parse_from(["dotc", "node", "list", "--site-dir", ""]).expect("empty");
        assert_eq!(cli.paths.site_dir, Some(PathBuf::new()));
    }

    #[test]
    fn site_dir_falls_back_to_the_xdg_state_then_the_home_default() {
        let paths = RootPathArgs::default();

        assert_eq!(
            paths
                .site_dir_in(&base(Some("/x"), None, Some("/h")))
                .expect("XDG default"),
            PathBuf::from("/x/dotcore/site")
        );
        assert_eq!(
            paths
                .site_dir_in(&base(None, None, Some("/h")))
                .expect("home default"),
            PathBuf::from("/h/.local/state/dotcore/site")
        );
    }

    #[test]
    fn cache_root_falls_back_to_the_xdg_cache_then_the_home_default() {
        let paths = RootPathArgs::default();

        assert_eq!(
            paths
                .cache_root_in(&base(None, Some("/xc"), Some("/h")))
                .expect("XDG default"),
            PathBuf::from("/xc/dotcore")
        );
        assert_eq!(
            paths
                .cache_root_in(&base(None, None, Some("/h")))
                .expect("home default"),
            PathBuf::from("/h/.cache/dotcore")
        );
    }

    #[test]
    fn a_given_folder_beats_the_default_and_is_normalized() {
        let paths = RootPathArgs {
            site_dir: Some(PathBuf::from("/srv/./a/../site")),
            ..RootPathArgs::default()
        };

        assert_eq!(
            paths
                .site_dir_in(&base(Some("/x"), None, Some("/h")))
                .expect("explicit"),
            PathBuf::from("/srv/site")
        );
    }

    #[test]
    fn command_line_site_dir_is_named_only_when_typed() {
        let typed =
            Cli::try_parse_from(["dotc", "node", "list", "--site-dir", "/a"]).expect("typed");
        let untyped = RootPathArgs {
            site_dir: Some(PathBuf::from("/a")),
            ..RootPathArgs::default()
        };

        assert_eq!(typed.paths.command_line_site_dir(), Some(Path::new("/a")));
        assert_eq!(untyped.command_line_site_dir(), None);
        assert_eq!(RootPathArgs::default().command_line_site_dir(), None);
    }

    #[test]
    fn relative_and_empty_folders_fail_with_the_absolute_path_error() {
        for (value, shown) in [("relative", "\"relative\""), ("", "\"\"")] {
            let given = Some(PathBuf::from(value));
            let site = RootPathArgs {
                site_dir: given.clone(),
                ..RootPathArgs::default()
            };
            let catalog = RootPathArgs {
                catalog_dir: given.clone(),
                ..RootPathArgs::default()
            };
            let cache = RootPathArgs {
                cache_dir: given,
                ..RootPathArgs::default()
            };
            let base = base(Some("/x"), Some("/xc"), Some("/h"));

            assert_eq!(
                format!("{:#}", site.site_dir_in(&base).expect_err("site")),
                format!(
                    "Invalid site path: DOTCORE_SITE_DIR must be an absolute path, got {shown}"
                )
            );
            assert_eq!(
                format!("{:#}", catalog.catalog_dir().expect_err("catalog")),
                format!(
                    "Invalid site path: DOTCORE_CATALOG_DIR must be an absolute path, got {shown}"
                )
            );
            assert_eq!(
                format!("{:#}", cache.cache_root_in(&base).expect_err("cache")),
                format!(
                    "Invalid site path: DOTCORE_CACHE_DIR must be an absolute path, got {shown}"
                )
            );
        }
    }

    #[test]
    fn the_cache_root_is_never_the_filesystem_root() {
        for value in ["/", "/a/.."] {
            let paths = RootPathArgs {
                cache_dir: Some(PathBuf::from(value)),
                ..RootPathArgs::default()
            };

            assert_eq!(
                format!(
                    "{:#}",
                    paths
                        .cache_root_in(&base(None, None, None))
                        .expect_err(value)
                ),
                "Invalid site path: DOTCORE_CACHE_DIR cannot be the filesystem root"
            );
        }
    }

    #[test]
    fn folders_with_line_breaks_or_invalid_text_are_rejected() {
        let line_break = RootPathArgs {
            site_dir: Some(PathBuf::from("/bad\nsite")),
            ..RootPathArgs::default()
        };
        assert_eq!(
            line_break
                .site_dir_in(&base(None, None, Some("/h")))
                .expect_err("line break")
                .to_string(),
            "DOTCORE_SITE_DIR is invalid: path must not contain carriage returns or newlines"
        );

        let invalid = RootPathArgs {
            site_dir: Some(PathBuf::from(OsString::from_vec(b"/bad\xff".to_vec()))),
            ..RootPathArgs::default()
        };
        assert_eq!(
            invalid
                .site_dir_in(&base(None, None, Some("/h")))
                .expect_err("invalid UTF-8")
                .to_string(),
            "DOTCORE_SITE_DIR must be valid UTF-8"
        );
    }

    #[test]
    fn process_paths_report_every_given_folder_and_the_derived_caches() {
        let paths = RootPathArgs {
            site_dir: Some(PathBuf::from("/s")),
            catalog_dir: Some(PathBuf::from("/c")),
            cache_dir: Some(PathBuf::from("/k")),
            ..RootPathArgs::default()
        };

        let resolved = paths
            .process_paths_in(&base(None, None, None))
            .expect("process paths");

        assert_eq!(resolved.root, PathBuf::from("/s"));
        assert_eq!(resolved.master_catalog_root, Some(PathBuf::from("/c")));
        assert_eq!(resolved.cache_root, PathBuf::from("/k"));
    }

    #[test]
    fn the_master_catalog_is_required_by_the_commands_that_load_it() {
        assert_eq!(
            RootPathArgs::default()
                .master_catalog()
                .expect_err("no catalog")
                .to_string(),
            "no master catalog: pass --catalog-dir or set DOTCORE_CATALOG_DIR to its folder"
        );
        assert_eq!(
            RootPathArgs {
                catalog_dir: Some(PathBuf::from("/c")),
                ..RootPathArgs::default()
            }
            .master_catalog()
            .expect("catalog"),
            PathBuf::from("/c")
        );
    }

    #[test]
    fn telemetry_falls_back_to_the_validated_normalized_site_folder() {
        let site = |value: &str| RootPathArgs {
            site_dir: Some(PathBuf::from(value)),
            ..RootPathArgs::default()
        };

        assert_eq!(
            site("/a/../b").telemetry_dir_in(None).expect("telemetry"),
            Some(PathBuf::from("/b/telemetry"))
        );
        assert_eq!(
            site("/a")
                .telemetry_dir_in(Some("/t"))
                .expect("explicit telemetry"),
            Some(PathBuf::from("/t"))
        );
        assert_eq!(
            RootPathArgs::default()
                .telemetry_dir_in(None)
                .expect("none"),
            None
        );
        for invalid in ["relative", ""] {
            assert_eq!(
                format!(
                    "{:#}",
                    site(invalid).telemetry_dir_in(None).expect_err(invalid)
                ),
                format!(
                    "Invalid site path: DOTCORE_SITE_DIR must be an absolute path, got {invalid:?}"
                )
            );
        }
    }

    #[test]
    fn server_overrides_carry_only_the_given_folders() {
        let paths = RootPathArgs {
            site_dir: Some(PathBuf::from("/s")),
            cache_dir: Some(PathBuf::from("/k")),
            catalog_dir: None,
            ..RootPathArgs::default()
        };

        assert_eq!(
            paths.server_overrides().expect("overrides"),
            PathOverrides {
                site_dir: Some("/s".to_owned()),
                catalog_dir: None,
                cache_dir: Some("/k".to_owned()),
            }
        );
    }

    fn base(
        xdg_state_home: Option<&str>,
        xdg_cache_home: Option<&str>,
        home: Option<&str>,
    ) -> BaseDirectories {
        BaseDirectories {
            xdg_state_home: xdg_state_home.map(str::to_owned),
            xdg_cache_home: xdg_cache_home.map(str::to_owned),
            home: home.map(str::to_owned),
        }
    }
}
