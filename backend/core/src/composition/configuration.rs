//! Immutable process-input snapshot and validation for backend composition.

use std::path::PathBuf;

use dotcore_domain::{BindAddress, SiteNodeHost, SitePaths};
use dotcore_site::node::folder::WORKER_MANIFEST_FILE;
use http::HeaderValue;
use tracing::{error, info};

/// Immutable snapshot of all process inputs needed to compose one backend.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BackendConfiguration {
    pub site_paths: SitePaths,
    pub role: BackendRole,
    /// Local directory receiving workload resource telemetry, or none to record nothing.
    pub telemetry_directory: Option<PathBuf>,
    /// Dotcore sources this process's SiteNode builds Nix artifacts from: its development checkout,
    /// without which `self` deployments fail on this node, and the source a packaged runtime was
    /// built from, which serves locked artifacts at its revision.
    pub dotcore_sources: dotcore_domain::nix::DotcoreSources,
}

impl BackendConfiguration {
    /// Captures all process inputs in one immutable snapshot.
    pub fn from_environment() -> Result<Self, BackendConfigurationError> {
        Self::from_environment_with_paths(&PathOverrides::default())
    }

    /// Captures all process inputs like `from_environment`, except that each path in
    /// `overrides` replaces its variable.
    pub fn from_environment_with_paths(
        overrides: &PathOverrides,
    ) -> Result<Self, BackendConfigurationError> {
        let result = Self::from_environment_with_overrides(overrides, environment_value);
        match &result {
            Ok(configuration) => info!(
                event_kind = "process_configuration_loaded",
                site_root = %configuration.site_paths.root.display(),
                "backend process configuration loaded"
            ),
            Err(error) => error!(
                event_kind = "process_configuration_failed",
                error_kind = "configuration",
                error = %error,
                "backend process configuration failed"
            ),
        }
        result
    }

    fn from_environment_with_overrides(
        overrides: &PathOverrides,
        mut read: impl FnMut(&'static str) -> Result<Option<String>, BackendConfigurationError>,
    ) -> Result<Self, BackendConfigurationError> {
        Self::from_environment_with(|variable| match overrides.value(variable) {
            Some(value) => Ok(Some(value.to_owned())),
            None => read(variable),
        })
    }

    pub(super) fn from_environment_with(
        mut read: impl FnMut(&'static str) -> Result<Option<String>, BackendConfigurationError>,
    ) -> Result<Self, BackendConfigurationError> {
        let site = read(dotcore_domain::SITE_DIR)?;
        let catalog = read(dotcore_domain::CATALOG_DIR)?;
        let cache = read(dotcore_domain::CACHE_DIR)?;
        let xdg_state_home = read(dotcore_domain::XDG_STATE_HOME)?;
        let xdg_cache_home = read(dotcore_domain::XDG_CACHE_HOME)?;
        let home = read(dotcore_domain::HOME)?;
        let resolved_paths = SitePaths::resolve_values(
            site.as_deref(),
            catalog.as_deref(),
            cache.as_deref(),
            xdg_state_home.as_deref(),
            xdg_cache_home.as_deref(),
            home.as_deref(),
        )?;
        let management = read(MANAGEMENT_ADDR_ENV)?;
        let inference = read(INFERENCE_ADDR_ENV)?;
        let origin = read(FRONTEND_ORIGIN_ENV)?;
        let private = read(PRIVATE_ADDR_ENV)?;
        let telemetry = read(crate::telemetry::TELEMETRY_DIR)?;
        let development = read(DEV_SOURCE_FLAKE_ENV)?
            .map(|value| value.trim().to_owned())
            .filter(|value| !value.is_empty());
        let packaged = read(PACKAGED_SOURCE_FLAKE_ENV)?
            .map(|value| value.trim().to_owned())
            .filter(|value| !value.is_empty())
            .map(|value| {
                dotcore_domain::nix::LocalDotcoreSource::parse(&value).ok_or_else(|| {
                    configuration_error(format!(
                        "invalid {PACKAGED_SOURCE_FLAKE_ENV} value {value:?}: expected a path: flake URL of a store path pinning its rev"
                    ))
                })
            })
            .transpose()?;
        let dotcore_sources = dotcore_domain::nix::DotcoreSources {
            development,
            packaged,
        };
        let telemetry_directory = crate::telemetry::resolve_directory(
            telemetry.as_deref().map(std::path::Path::new),
            Some(&resolved_paths.root),
        );
        // A joined node serves only its private channel, so its catalog and public listener
        // settings are ignored.
        if node_role(&resolved_paths.root) == NodeRole::JoinedNode {
            // A joined node publishes its bind host to the manager, so it must be specific.
            let private = private
                .as_deref()
                .map(|value| specific_bind_address(value, PRIVATE_ADDR_ENV))
                .transpose()?;
            return Ok(Self {
                site_paths: resolved_paths.without_master_catalog(),
                role: BackendRole::JoinedNode {
                    private_bind_address: private,
                },
                telemetry_directory,
                dotcore_sources,
            });
        }
        let mut configuration = Self::from_resolved_values(
            resolved_paths.require_master_catalog()?,
            management,
            inference,
            origin,
        )?;
        configuration.telemetry_directory = telemetry_directory;
        configuration.dotcore_sources = dotcore_sources;
        if let BackendRole::Manager(server) = &mut configuration.role {
            server.private_listener = ManagerPrivateListener::from_value(private.as_deref())?;
        }
        Ok(configuration)
    }

    pub fn from_values(
        site: Option<String>,
        catalog: Option<String>,
        cache: Option<String>,
        management: Option<String>,
        inference: Option<String>,
        origin: Option<String>,
    ) -> Result<Self, BackendConfigurationError> {
        let site_paths =
            SitePaths::from_values(site.as_deref(), catalog.as_deref(), cache.as_deref())?;
        Self::from_resolved_values(site_paths, management, inference, origin)
    }

    fn from_resolved_values(
        site_paths: SitePaths,
        management: Option<String>,
        inference: Option<String>,
        origin: Option<String>,
    ) -> Result<Self, BackendConfigurationError> {
        let management = required(management, MANAGEMENT_ADDR_ENV)?;
        let inference = required(inference, INFERENCE_ADDR_ENV)?;
        let management_bind_address = specific_bind_address(&management, MANAGEMENT_ADDR_ENV)?;
        let inference_bind_address = bind_address(&inference, INFERENCE_ADDR_ENV)?;
        let allowed_origins = origin
            .map(|origin| {
                validate_origin(&origin)?;
                origin.parse::<HeaderValue>().map_err(|error| {
                    configuration_source(
                        format!("invalid {FRONTEND_ORIGIN_ENV} value {origin:?}"),
                        error,
                    )
                })
            })
            .transpose()?
            .into_iter()
            .collect();
        let telemetry_directory = crate::telemetry::resolve_directory(None, Some(&site_paths.root));
        Ok(Self {
            site_paths,
            telemetry_directory,
            dotcore_sources: Default::default(),
            role: BackendRole::Manager(ServerConfiguration {
                management_bind_address,
                inference_bind_address,
                private_listener: ManagerPrivateListener::SiteDefault {
                    port: DEFAULT_PRIVATE_PORT,
                },
                allowed_origins,
            }),
        })
    }
}

/// What this process serves: the site manager's public and private transports, or a joined node's
/// private channel alone.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum BackendRole {
    /// The site manager, serving management, inference, and the private transport its
    /// [`ManagerPrivateListener`] selects.
    Manager(ServerConfiguration),

    /// A node joined to a site, started from its node folder. Binds no public listener.
    JoinedNode {
        /// Overrides the private bind address recorded in the node folder.
        private_bind_address: Option<BindAddress>,
    },
}

/// Immutable transport inputs the manager supplies to the transport-only Server. Listener
/// addresses may name a host, resolved when the listeners bind.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ServerConfiguration {
    pub management_bind_address: BindAddress,
    pub inference_bind_address: BindAddress,
    pub private_listener: ManagerPrivateListener,
    pub allowed_origins: Vec<HeaderValue>,
}

/// Where the manager's private listener, the one joined nodes dial, binds. Composition resolves
/// the site default once the site is loaded, since it depends on the declared manager host.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ManagerPrivateListener {
    /// The address `DOTCORE_PRIVATE_ADDR` names. A wildcard binds every interface, as needed
    /// behind NAT where the declared manager host is not a local address.
    Configured(BindAddress),

    /// The declared manager SiteNode host with `port`, opened only when the site has joined
    /// nodes. Port 0 binds an ephemeral port, which only tests use.
    SiteDefault { port: u16 },
}

impl ManagerPrivateListener {
    /// Reads the manager's listener from a `DOTCORE_PRIVATE_ADDR` value, or the site default
    /// with [`DEFAULT_PRIVATE_PORT`] when the variable is unset.
    pub fn from_value(value: Option<&str>) -> Result<Self, BackendConfigurationError> {
        match value {
            Some(value) => bind_address(value, PRIVATE_ADDR_ENV).map(Self::Configured),
            None => Ok(Self::SiteDefault {
                port: DEFAULT_PRIVATE_PORT,
            }),
        }
    }

    /// Port the manager's private listener binds.
    pub fn port(&self) -> u16 {
        match self {
            Self::Configured(address) => address.port(),
            Self::SiteDefault { port } => *port,
        }
    }
}

/// Explicit path values that replace the site, catalog, and cache variables; `None` reads the
/// variable. Defaults and validation stay with the site path resolver.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct PathOverrides {
    pub site_dir: Option<String>,
    pub catalog_dir: Option<String>,
    pub cache_dir: Option<String>,
}

impl PathOverrides {
    fn value(&self, variable: &str) -> Option<&str> {
        match variable {
            dotcore_domain::SITE_DIR => self.site_dir.as_deref(),
            dotcore_domain::CATALOG_DIR => self.catalog_dir.as_deref(),
            dotcore_domain::CACHE_DIR => self.cache_dir.as_deref(),
            _ => None,
        }
    }
}

/// Errors raised while capturing or validating backend process inputs.
#[derive(Debug, thiserror::Error)]
pub enum BackendConfigurationError {
    #[error(transparent)]
    SitePaths(#[from] dotcore_domain::SitePathsError),

    #[error("Invalid backend configuration: {0}")]
    Invalid(String),

    #[error("Invalid backend configuration")]
    Sourced(#[source] anyhow::Error),
}

pub const MANAGEMENT_ADDR_ENV: &str = "DOTCORE_MANAGEMENT_ADDR";

/// Management address clients assume when none is configured. The server does not apply it: the
/// management address variable stays required to start a backend.
pub const DEFAULT_MANAGEMENT_ADDR: &str = "127.0.0.1:50051";
pub const INFERENCE_ADDR_ENV: &str = "DOTCORE_INFERENCE_ADDR";

/// Inference API port clients assume on the manager's host when no inference address is
/// configured. The server does not apply it: the inference address variable stays required to
/// start a backend.
pub const DEFAULT_INFERENCE_PORT: u16 = 50052;
pub const FRONTEND_ORIGIN_ENV: &str = "DOTCORE_FRONTEND_ORIGIN";
pub const PRIVATE_ADDR_ENV: &str = "DOTCORE_PRIVATE_ADDR";

/// Default private listener port: the manager's site default, the port `dotc node add` gives
/// joining nodes, and the port `dotc node join` proposes for a node's own listener. Only the
/// composition root and the CLI read it; site modules take ports as parameters, since
/// composition depends on them.
pub const DEFAULT_PRIVATE_PORT: u16 = 50053;
pub const DEV_SOURCE_FLAKE_ENV: &str = "DOTCORE_DEV_SOURCE_FLAKE";
pub const PACKAGED_SOURCE_FLAKE_ENV: &str = "DOTCORE_PACKAGED_SOURCE_FLAKE";

/// Role a node folder plays, derived from its contents.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NodeRole {
    /// Holds the site configuration and serves every site-manager listener.
    Manager,
    /// Holds only a worker manifest and serves its private listener.
    JoinedNode,
}

/// Detects the role of the node folder at `root`: a worker manifest without a manager site
/// configuration is a joined node, anything else (including an empty folder) is a manager.
pub fn node_role(root: &std::path::Path) -> NodeRole {
    if !root.join(dotcore_site::SITE_FILE).exists() && root.join(WORKER_MANIFEST_FILE).is_file() {
        NodeRole::JoinedNode
    } else {
        NodeRole::Manager
    }
}

fn environment_value(variable: &'static str) -> Result<Option<String>, BackendConfigurationError> {
    match std::env::var(variable) {
        Ok(value) => Ok(Some(value)),
        Err(std::env::VarError::NotPresent) => Ok(None),
        Err(error) => Err(configuration_source(
            format!("could not read {variable}"),
            error,
        )),
    }
}

fn bind_address(value: &str, variable: &str) -> Result<BindAddress, BackendConfigurationError> {
    value
        .parse::<BindAddress>()
        .map_err(|error| configuration_source(format!("invalid {variable} value {value:?}"), error))
}

fn specific_bind_address(
    value: &str,
    variable: &str,
) -> Result<BindAddress, BackendConfigurationError> {
    let address = bind_address(value, variable)?;
    if address.is_wildcard() {
        return Err(configuration_error(format!(
            "{variable} must use a specific IPv4 address or host name, got {address}"
        )));
    }
    Ok(address)
}

/// Checks that the frontend origin is `http://host:port` with a SiteNode host in canonical form,
/// since browsers send the origin lowercased and CORS compares it byte for byte.
fn validate_origin(origin: &str) -> Result<(), BackendConfigurationError> {
    let invalid = || {
        configuration_error(format!(
            "{FRONTEND_ORIGIN_ENV} must be an http origin with a lowercase IPv4 address or host \
             name and a port, got {origin:?}"
        ))
    };
    let (host, port) = origin
        .strip_prefix("http://")
        .and_then(|address| address.rsplit_once(':'))
        .ok_or_else(invalid)?;
    port.parse::<u16>().map_err(|_| invalid())?;
    SiteNodeHost::parse(host).map_err(|_| invalid())?;
    Ok(())
}

fn required(
    value: Option<String>,
    variable: &'static str,
) -> Result<String, BackendConfigurationError> {
    value.ok_or_else(|| configuration_error(format!("{variable} is not set")))
}

fn configuration_error(message: impl Into<String>) -> BackendConfigurationError {
    BackendConfigurationError::Invalid(message.into())
}

fn configuration_source(
    message: impl Into<String>,
    source: impl Into<anyhow::Error>,
) -> BackendConfigurationError {
    BackendConfigurationError::Sourced(source.into().context(message.into()))
}

#[cfg(test)]
mod tests {
    use std::error::Error;

    use dotcore_domain::nix::DotcoreSources;
    use dotcore_site::node::folder::WORKER_MANIFEST_FILE;

    use super::*;

    #[test]
    fn semantic_configuration_states_are_source_less() {
        let error = BackendConfiguration::from_values(
            Some("/site".to_owned()),
            Some("/catalog".to_owned()),
            Some("/cache".to_owned()),
            None,
            Some("127.0.0.1:50052".to_owned()),
            Some("http://127.0.0.1:5173".to_owned()),
        )
        .expect_err("missing management address");

        assert!(matches!(
            &error,
            BackendConfigurationError::Invalid(message) if message == "DOTCORE_MANAGEMENT_ADDR is not set"
        ));
        assert_eq!(
            error.to_string(),
            "Invalid backend configuration: DOTCORE_MANAGEMENT_ADDR is not set"
        );
        assert!(error.source().is_none());
    }

    #[test]
    fn dotcore_sources_are_read_trimmed_from_the_environment_and_empty_means_none() {
        let environment = |development: Option<&'static str>, packaged: Option<&'static str>| {
            move |variable: &'static str| {
                Ok(match variable {
                    dotcore_domain::SITE_DIR => Some("/site".to_owned()),
                    dotcore_domain::CATALOG_DIR => Some("/catalog".to_owned()),
                    dotcore_domain::CACHE_DIR => Some("/cache".to_owned()),
                    MANAGEMENT_ADDR_ENV => Some("127.0.0.1:50051".to_owned()),
                    INFERENCE_ADDR_ENV => Some("127.0.0.1:50052".to_owned()),
                    DEV_SOURCE_FLAKE_ENV => development.map(str::to_owned),
                    PACKAGED_SOURCE_FLAKE_ENV => packaged.map(str::to_owned),
                    _ => None,
                })
            }
        };
        let sources = |development, packaged| {
            BackendConfiguration::from_environment_with(environment(development, packaged))
                .map(|configuration| configuration.dotcore_sources)
        };
        let packaged = "path:/nix/store/0123456789abcdfghijklmnpqrsvwxyz-source?rev=0123456789abcdef0123456789abcdef01234567";

        assert_eq!(
            sources(None, None).expect("sources"),
            DotcoreSources::default()
        );
        assert_eq!(
            sources(Some(" "), Some(" ")).expect("sources"),
            DotcoreSources::default()
        );
        assert_eq!(
            sources(Some(" git+file:///src/dotcore\n"), Some(packaged)).expect("sources"),
            DotcoreSources {
                development: Some("git+file:///src/dotcore".to_owned()),
                packaged: dotcore_domain::nix::LocalDotcoreSource::parse(packaged),
            }
        );
        let invalid = sources(None, Some("github:dotcoreai/dotcore")).expect_err("invalid");
        assert!(
            invalid
                .to_string()
                .contains("invalid DOTCORE_PACKAGED_SOURCE_FLAKE value"),
            "{invalid}"
        );
    }

    #[test]
    fn telemetry_directory_prefers_the_environment_over_the_site() {
        let environment = |telemetry: Option<&'static str>| {
            move |variable: &'static str| {
                Ok(match variable {
                    dotcore_domain::SITE_DIR => Some("/site".to_owned()),
                    dotcore_domain::CATALOG_DIR => Some("/catalog".to_owned()),
                    dotcore_domain::CACHE_DIR => Some("/cache".to_owned()),
                    MANAGEMENT_ADDR_ENV => Some("127.0.0.1:50051".to_owned()),
                    INFERENCE_ADDR_ENV => Some("127.0.0.1:50052".to_owned()),
                    crate::telemetry::TELEMETRY_DIR => telemetry.map(str::to_owned),
                    _ => None,
                })
            }
        };

        let fallback = BackendConfiguration::from_environment_with(environment(None))
            .expect("configuration without telemetry variable");
        let explicit =
            BackendConfiguration::from_environment_with(environment(Some("/data/telemetry")))
                .expect("configuration with telemetry variable");

        assert_eq!(
            fallback.telemetry_directory,
            Some(std::path::PathBuf::from("/site/telemetry"))
        );
        assert_eq!(
            explicit.telemetry_directory,
            Some(std::path::PathBuf::from("/data/telemetry"))
        );
    }

    #[test]
    fn site_path_failure_retains_its_concrete_source() {
        let error = BackendConfiguration::from_values(
            Some("relative-site".to_owned()),
            Some("/catalog".to_owned()),
            Some("/cache".to_owned()),
            Some("127.0.0.1:50051".to_owned()),
            Some("127.0.0.1:50052".to_owned()),
            Some("http://127.0.0.1:5173".to_owned()),
        )
        .expect_err("relative site path");

        let BackendConfigurationError::SitePaths(site_paths) = error else {
            panic!("invalid site path should retain its typed category");
        };
        assert_eq!(site_paths.to_string(), "Invalid site path");
        assert_eq!(
            site_paths
                .source()
                .expect("Site path validation source")
                .to_string(),
            "DOTCORE_SITE_DIR must be an absolute path, got \"relative-site\""
        );
    }

    #[test]
    fn header_parse_failure_retains_its_concrete_source() {
        let header_parse = "origin\nvalue"
            .parse::<HeaderValue>()
            .expect_err("invalid header value");
        let error = configuration_source("invalid frontend origin header", header_parse);

        let BackendConfigurationError::Sourced(context) = &error else {
            panic!("header parse should be a sourced configuration error");
        };
        assert_eq!(context.to_string(), "invalid frontend origin header");
        assert!(context.source().is_some_and(|source| {
            source
                .downcast_ref::<http::header::InvalidHeaderValue>()
                .is_some()
        }));
    }

    #[test]
    fn operational_configuration_failures_retain_their_concrete_sources() {
        let parse = BackendConfiguration::from_values(
            Some("/site".to_owned()),
            Some("/catalog".to_owned()),
            Some("/cache".to_owned()),
            Some("not-an-address".to_owned()),
            Some("127.0.0.1:50052".to_owned()),
            Some("http://127.0.0.1:5173".to_owned()),
        )
        .expect_err("invalid management address");
        assert_eq!(parse.to_string(), "Invalid backend configuration");
        let parse_context = parse.source().expect("parse context");
        assert_eq!(
            parse_context.to_string(),
            "invalid DOTCORE_MANAGEMENT_ADDR value \"not-an-address\""
        );
        assert!(parse_context.source().is_some_and(|source| {
            source
                .downcast_ref::<dotcore_domain::BindAddressError>()
                .is_some()
        }));

        let environment = BackendConfiguration::from_environment_with(|_| {
            Err(configuration_source(
                "injected environment read failure",
                std::io::Error::other("injected environment source"),
            ))
        })
        .expect_err("environment failure");
        assert_eq!(environment.to_string(), "Invalid backend configuration");
        let environment_context = environment.source().expect("environment context");
        assert_eq!(
            environment_context.to_string(),
            "injected environment read failure"
        );
        assert!(
            environment_context
                .source()
                .is_some_and(|source| source.downcast_ref::<std::io::Error>().is_some())
        );
    }

    #[test]
    fn joined_node_needs_only_its_site_directory() {
        let root = site_directory("joined-minimal", &[WORKER_MANIFEST_FILE]);

        let configuration = from_environment(&root, &[]).expect("joined node configuration");

        assert_eq!(configuration.site_paths.root, root);
        assert_eq!(configuration.site_paths.master_catalog_root, None);
        assert_eq!(
            configuration.role,
            BackendRole::JoinedNode {
                private_bind_address: None
            }
        );
    }

    #[test]
    fn joined_node_private_address_comes_from_the_environment_override() {
        let root = site_directory("joined-private", &[WORKER_MANIFEST_FILE]);

        let configuration = from_environment(&root, &[(PRIVATE_ADDR_ENV, "127.0.0.2:50053")])
            .expect("joined node configuration");
        let invalid = from_environment(&root, &[(PRIVATE_ADDR_ENV, "0.0.0.0:50053")])
            .expect_err("unspecified private address");

        assert_eq!(
            configuration.role,
            BackendRole::JoinedNode {
                private_bind_address: Some("127.0.0.2:50053".parse().expect("private address"))
            }
        );
        assert_eq!(
            invalid.to_string(),
            "Invalid backend configuration: DOTCORE_PRIVATE_ADDR must use a specific IPv4 address or host name, got 0.0.0.0:50053"
        );
    }

    #[test]
    fn manager_private_listener_defaults_to_the_site_and_accepts_a_wildcard_override() {
        let root = site_directory("manager-private", &[dotcore_site::SITE_FILE]);
        let manager = |extra: &[(&'static str, &'static str)]| {
            let mut variables = vec![
                (dotcore_domain::CATALOG_DIR, "/catalog"),
                (MANAGEMENT_ADDR_ENV, "127.0.0.1:50051"),
                (INFERENCE_ADDR_ENV, "0.0.0.0:50052"),
            ];
            variables.extend_from_slice(extra);
            let configuration = from_environment(&root, &variables).expect("manager configuration");
            let BackendRole::Manager(server) = configuration.role else {
                panic!("manager role");
            };
            server.private_listener
        };

        assert_eq!(
            manager(&[]),
            ManagerPrivateListener::SiteDefault { port: 50053 }
        );
        assert_eq!(
            manager(&[(PRIVATE_ADDR_ENV, "0.0.0.0:50053")]),
            ManagerPrivateListener::Configured("0.0.0.0:50053".parse().expect("private address"))
        );
        assert_eq!(manager(&[(PRIVATE_ADDR_ENV, "0.0.0.0:6000")]).port(), 6000);
    }

    #[test]
    fn listener_settings_keep_host_names_for_startup() {
        let root = site_directory("manager-names", &[dotcore_site::SITE_FILE]);

        let configuration = from_environment(
            &root,
            &[
                (dotcore_domain::CATALOG_DIR, "/catalog"),
                (MANAGEMENT_ADDR_ENV, "Manager.Example.TS.net:50051"),
                (INFERENCE_ADDR_ENV, "0.0.0.0:50052"),
                (PRIVATE_ADDR_ENV, "localhost:50053"),
                (FRONTEND_ORIGIN_ENV, "http://manager.example.ts.net:5173"),
            ],
        )
        .expect("manager configuration with host names");
        let wildcard = from_environment(
            &root,
            &[
                (dotcore_domain::CATALOG_DIR, "/catalog"),
                (MANAGEMENT_ADDR_ENV, "0.0.0.0:50051"),
                (INFERENCE_ADDR_ENV, "0.0.0.0:50052"),
            ],
        )
        .expect_err("wildcard management address");

        let BackendRole::Manager(server) = configuration.role else {
            panic!("manager role");
        };
        assert_eq!(
            server.management_bind_address.to_string(),
            "manager.example.ts.net:50051"
        );
        assert_eq!(server.inference_bind_address.to_string(), "0.0.0.0:50052");
        assert_eq!(
            server.private_listener,
            ManagerPrivateListener::Configured("localhost:50053".parse().expect("private address"))
        );
        assert_eq!(
            server.allowed_origins,
            [HeaderValue::from_static(
                "http://manager.example.ts.net:5173"
            )]
        );
        assert_eq!(
            wildcard.to_string(),
            "Invalid backend configuration: DOTCORE_MANAGEMENT_ADDR must use a specific IPv4 address or host name, got 0.0.0.0:50051"
        );
        for origin in [
            "http://Manager.example.ts.net:5173",
            "http://manager.example.ts.net.:5173",
            "http://0.0.0.0:5173",
            "https://manager.example.ts.net:5173",
            "http://manager.example.ts.net",
        ] {
            let error = from_environment(
                &root,
                &[
                    (dotcore_domain::CATALOG_DIR, "/catalog"),
                    (MANAGEMENT_ADDR_ENV, "127.0.0.1:50051"),
                    (INFERENCE_ADDR_ENV, "0.0.0.0:50052"),
                    (FRONTEND_ORIGIN_ENV, origin),
                ],
            )
            .expect_err(origin);
            assert_eq!(
                error.to_string(),
                format!(
                    "Invalid backend configuration: DOTCORE_FRONTEND_ORIGIN must be an http \
                     origin with a lowercase IPv4 address or host name and a port, got {origin:?}"
                )
            );
        }
    }

    #[test]
    fn joined_node_ignores_public_listener_and_catalog_settings() {
        let root = site_directory("joined-listeners", &[WORKER_MANIFEST_FILE]);

        let configuration = from_environment(
            &root,
            &[
                (MANAGEMENT_ADDR_ENV, "127.0.0.1:50051"),
                (INFERENCE_ADDR_ENV, "127.0.0.1:50052"),
                (dotcore_domain::CATALOG_DIR, "/catalog"),
            ],
        )
        .expect("joined node configuration with extra settings");

        assert_eq!(
            configuration.role,
            BackendRole::JoinedNode {
                private_bind_address: None
            }
        );
        assert_eq!(configuration.site_paths.master_catalog_root, None);
    }

    #[test]
    fn joined_node_reads_its_development_source_flake_trimmed() {
        let root = site_directory("joined-source", &[WORKER_MANIFEST_FILE]);

        let configured = from_environment(
            &root,
            &[(DEV_SOURCE_FLAKE_ENV, " git+file:///src/dotcore\n")],
        )
        .expect("joined node configuration with development source");
        let empty = from_environment(&root, &[(DEV_SOURCE_FLAKE_ENV, " ")])
            .expect("joined node configuration with empty development source");

        assert_eq!(
            configured.role,
            BackendRole::JoinedNode {
                private_bind_address: None
            }
        );
        assert_eq!(
            configured.dotcore_sources.development.as_deref(),
            Some("git+file:///src/dotcore")
        );
        assert_eq!(
            empty.role,
            BackendRole::JoinedNode {
                private_bind_address: None
            }
        );
        assert_eq!(empty.dotcore_sources, DotcoreSources::default());
    }

    #[test]
    fn manager_still_requires_its_catalog_and_listener_settings() {
        let without_worker = site_directory("manager-plain", &[]);
        let with_site = site_directory(
            "manager-with-site",
            &[WORKER_MANIFEST_FILE, dotcore_site::SITE_FILE],
        );

        for root in [&without_worker, &with_site] {
            let catalog = from_environment(root, &[]).expect_err("missing catalog directory");
            let management = from_environment(root, &[(dotcore_domain::CATALOG_DIR, "/catalog")])
                .expect_err("missing management address");
            let inference = from_environment(
                root,
                &[
                    (dotcore_domain::CATALOG_DIR, "/catalog"),
                    (MANAGEMENT_ADDR_ENV, "127.0.0.1:50051"),
                ],
            )
            .expect_err("missing inference address");

            assert_eq!(
                format!("{:#}", anyhow::Error::from(catalog)),
                "Invalid site path: DOTCORE_CATALOG_DIR is not set"
            );
            assert_eq!(
                management.to_string(),
                "Invalid backend configuration: DOTCORE_MANAGEMENT_ADDR is not set"
            );
            assert_eq!(
                inference.to_string(),
                "Invalid backend configuration: DOTCORE_INFERENCE_ADDR is not set"
            );
        }
    }

    /// Creates a site directory holding empty files with the given names.
    #[test]
    fn path_overrides_beat_the_environment_one_path_at_a_time() {
        let environment = |variable: &'static str| {
            Ok(match variable {
                dotcore_domain::SITE_DIR => Some("/env-site".to_owned()),
                dotcore_domain::CATALOG_DIR => Some("/env-catalog".to_owned()),
                dotcore_domain::CACHE_DIR => Some("/env-cache".to_owned()),
                MANAGEMENT_ADDR_ENV => Some("127.0.0.1:50051".to_owned()),
                INFERENCE_ADDR_ENV => Some("127.0.0.1:50052".to_owned()),
                _ => None,
            })
        };
        let paths = |overrides: PathOverrides| {
            let configuration =
                BackendConfiguration::from_environment_with_overrides(&overrides, environment)
                    .expect("configuration")
                    .site_paths;
            (
                configuration.root,
                configuration.master_catalog_root,
                configuration.cache_root,
            )
        };
        let expected = |site: &str, catalog: &str, cache: &str| {
            (
                PathBuf::from(site),
                Some(PathBuf::from(catalog)),
                PathBuf::from(cache),
            )
        };

        assert_eq!(
            paths(PathOverrides::default()),
            expected("/env-site", "/env-catalog", "/env-cache")
        );
        assert_eq!(
            paths(PathOverrides {
                site_dir: Some("/flag-site".to_owned()),
                ..PathOverrides::default()
            }),
            expected("/flag-site", "/env-catalog", "/env-cache")
        );
        assert_eq!(
            paths(PathOverrides {
                catalog_dir: Some("/flag-catalog".to_owned()),
                ..PathOverrides::default()
            }),
            expected("/env-site", "/flag-catalog", "/env-cache")
        );
        assert_eq!(
            paths(PathOverrides {
                cache_dir: Some("/flag-cache".to_owned()),
                ..PathOverrides::default()
            }),
            expected("/env-site", "/env-catalog", "/flag-cache")
        );
    }

    #[test]
    fn a_relative_path_override_is_rejected_even_when_the_environment_is_valid() {
        let error = BackendConfiguration::from_environment_with_overrides(
            &PathOverrides {
                site_dir: Some("relative".to_owned()),
                ..PathOverrides::default()
            },
            |variable| {
                Ok(match variable {
                    dotcore_domain::SITE_DIR => Some("/env-site".to_owned()),
                    dotcore_domain::CATALOG_DIR => Some("/env-catalog".to_owned()),
                    MANAGEMENT_ADDR_ENV => Some("127.0.0.1:50051".to_owned()),
                    INFERENCE_ADDR_ENV => Some("127.0.0.1:50052".to_owned()),
                    _ => None,
                })
            },
        )
        .expect_err("relative override");

        assert_eq!(
            error.source().expect("source").to_string(),
            "DOTCORE_SITE_DIR must be an absolute path, got \"relative\""
        );
    }

    #[test]
    fn the_management_default_is_not_applied_to_the_server() {
        assert_eq!(DEFAULT_MANAGEMENT_ADDR, "127.0.0.1:50051");
        let error = BackendConfiguration::from_values(
            Some("/site".to_owned()),
            Some("/catalog".to_owned()),
            Some("/cache".to_owned()),
            None,
            Some("127.0.0.1:50052".to_owned()),
            Some("http://127.0.0.1:5173".to_owned()),
        )
        .expect_err("management address stays required");
        assert_eq!(
            error.to_string(),
            "Invalid backend configuration: DOTCORE_MANAGEMENT_ADDR is not set"
        );
    }

    fn site_directory(label: &str, files: &[&str]) -> PathBuf {
        let root = dotcore_domain::test_support::canonical_temp_dir().join(format!(
            "dotcore-configuration-{label}-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).expect("Site directory");
        for file in files {
            std::fs::write(root.join(file), "").expect("Site directory file");
        }
        root
    }

    fn from_environment(
        root: &std::path::Path,
        extra: &[(&'static str, &'static str)],
    ) -> Result<BackendConfiguration, BackendConfigurationError> {
        let root = root.to_str().expect("Site directory path").to_owned();
        BackendConfiguration::from_environment_with(|variable| {
            Ok(match variable {
                dotcore_domain::SITE_DIR => Some(root.clone()),
                dotcore_domain::CACHE_DIR => Some("/cache".to_owned()),
                _ => extra
                    .iter()
                    .find(|(name, _)| *name == variable)
                    .map(|(_, value)| (*value).to_owned()),
            })
        })
    }
}
