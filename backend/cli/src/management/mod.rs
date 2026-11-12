//! The connection that commands open to the running site manager's management API, and the rules
//! deployment management shares for naming a deployment and reporting what the manager refused.
//!
//! The address comes from a flag or the environment and takes an optional `http://` scheme; this
//! is the only place that decides the transport, so TLS and credentials can be added here alone.
//! A deployment is named by its id or its name. Names are not unique, so a name matching several
//! deployments fails with their ids instead of picking one.

mod deployment;
mod logs;

use std::{collections::HashMap, fmt, str::FromStr, time::Duration};

use clap::Args;
use dotcore::composition::{DEFAULT_MANAGEMENT_ADDR, MANAGEMENT_ADDR_ENV};
use dotcore_proto::dotcore::{
    application::v1::application_deployment_service_client::ApplicationDeploymentServiceClient,
    model::v1::model_deployment_service_client::ModelDeploymentServiceClient,
    site::v1::{
        ConfiguredExecutionTarget, ListConfiguredExecutionTargetsRequest,
        site_service_client::SiteServiceClient,
    },
};
use tonic::{
    Code, Status,
    codegen::http::uri::InvalidUri,
    transport::{Channel, Endpoint, Uri},
};

pub(crate) use deployment::{
    Lifecycle, ManagedDeployment, change_lifecycle, find_deployment, list_deployments, teardown,
};
pub(crate) use logs::{
    LogEntry, LogEvent, LogEvents, LogPage, LogPosition, LogSource, LogSourceState,
    LoggedDeployment, LogsArgs, deployment_logs,
};

/// How long connecting to the manager may take before the command reports it unreachable.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);

/// The `--manager` flag of every command that calls the running site manager.
#[derive(Debug, Args)]
pub(crate) struct ManagerArgs {
    /// Management address of the running site manager, as host:port or http://host:port.
    #[arg(
        long = "manager",
        env = MANAGEMENT_ADDR_ENV,
        value_name = "ADDRESS",
        default_value = DEFAULT_MANAGEMENT_ADDR
    )]
    pub(crate) manager: ManagerAddress,
}

impl ManagerArgs {
    /// Connects to the manager the flag names.
    pub(crate) async fn connect(&self) -> Result<Manager, ManagementError> {
        Manager::connect(&self.manager).await
    }
}

/// A validated management address, always shown as `http://host:port`.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct ManagerAddress {
    uri: Uri,
}

impl FromStr for ManagerAddress {
    type Err = AddressError;

    /// Accepts `host:port` or `http://host:port`; any other scheme, a missing port, a path, a
    /// query, or user information is refused.
    fn from_str(text: &str) -> Result<Self, Self::Err> {
        if text.is_empty() {
            return Err(AddressError::Empty);
        }
        let parsed = text
            .parse::<Uri>()
            .map_err(|source| AddressError::Invalid {
                address: text.to_owned(),
                source,
            })?;
        if let Some(scheme) = parsed.scheme_str().filter(|scheme| *scheme != "http") {
            return Err(AddressError::UnsupportedScheme {
                address: text.to_owned(),
                scheme: scheme.to_owned(),
            });
        }
        let shape = || AddressError::Shape {
            address: text.to_owned(),
        };
        let authority = parsed
            .authority()
            .filter(|authority| authority.port_u16().is_some() && !authority.as_str().contains('@'))
            .ok_or_else(shape)?;
        if parsed
            .path_and_query()
            .is_some_and(|path| path.as_str() != "/")
        {
            return Err(shape());
        }

        let uri = Uri::builder()
            .scheme("http")
            .authority(authority.clone())
            .path_and_query("/")
            .build()
            .map_err(|_| shape())?;
        Ok(Self { uri })
    }
}

impl fmt::Display for ManagerAddress {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.uri.authority() {
            Some(authority) => write!(formatter, "http://{authority}"),
            None => write!(formatter, "{}", self.uri),
        }
    }
}

/// An open connection to the site manager's management API.
#[derive(Clone, Debug)]
pub(crate) struct Manager {
    channel: Channel,
}

impl Manager {
    /// Connects to the manager, failing when it cannot be reached within a few seconds.
    pub(crate) async fn connect(address: &ManagerAddress) -> Result<Self, ManagementError> {
        let channel = Endpoint::from(address.uri.clone())
            .connect_timeout(CONNECT_TIMEOUT)
            .connect()
            .await
            .map_err(|source| ManagementError::Unreachable {
                address: address.clone(),
                source,
            })?;
        Ok(Self { channel })
    }

    /// Returns a client of the model deployment service.
    pub(crate) fn models(&self) -> ModelDeploymentServiceClient<Channel> {
        ModelDeploymentServiceClient::new(self.channel.clone())
    }

    /// Returns a client of the application deployment service.
    pub(crate) fn applications(&self) -> ApplicationDeploymentServiceClient<Channel> {
        ApplicationDeploymentServiceClient::new(self.channel.clone())
    }

    /// Reads the node display names from the site's execution targets. Names only decorate the
    /// output, so a failed read is logged and leaves every node shown by its id.
    pub(crate) async fn node_names(&self) -> NodeNames {
        match SiteServiceClient::new(self.channel.clone())
            .list_configured_execution_targets(ListConfiguredExecutionTargetsRequest {})
            .await
        {
            Ok(response) => NodeNames::from_targets(&response.into_inner().execution_targets),
            Err(status) => {
                tracing::debug!(
                    code = ?status.code(),
                    message = status.message(),
                    "node names unavailable; showing node ids"
                );
                NodeNames::default()
            }
        }
    }
}

/// Node and execution target display names, for showing where a deployment runs.
#[derive(Debug, Default, Eq, PartialEq)]
pub(crate) struct NodeNames {
    names: HashMap<String, String>,
    targets: HashMap<(String, String), String>,
}

impl NodeNames {
    /// Returns the node's display name, or its id when the manager did not name it.
    pub(crate) fn display<'a>(&'a self, node_id: &'a str) -> &'a str {
        self.names.get(node_id).map_or(node_id, String::as_str)
    }

    /// Returns the execution target's display name, or its id when the target is no longer
    /// configured or the manager did not name it.
    pub(crate) fn target_display<'a>(&'a self, node_id: &str, target_id: &'a str) -> &'a str {
        self.targets
            .get(&(node_id.to_owned(), target_id.to_owned()))
            .map_or(target_id, String::as_str)
    }

    fn from_targets(configured: &[ConfiguredExecutionTarget]) -> Self {
        let targets = configured
            .iter()
            .filter(|target| !target.display_name.is_empty())
            .map(|target| {
                (
                    (
                        target.site_node_id.clone(),
                        target.execution_target_id.clone(),
                    ),
                    target.display_name.clone(),
                )
            })
            .collect();
        let names = configured
            .iter()
            .filter(|target| !target.site_node_display_name.is_empty())
            .map(|target| {
                (
                    target.site_node_id.clone(),
                    target.site_node_display_name.clone(),
                )
            })
            .collect();
        Self { names, targets }
    }
}

/// Which deployment command group a reference or request belongs to, which picks its wording and
/// the command that lists its deployments.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum DeploymentGroup {
    Model,

    Application,
}

impl DeploymentGroup {
    /// The operator word for one deployment, for example `model deployment`.
    pub(crate) fn noun(self) -> &'static str {
        match self {
            Self::Model => "model deployment",
            Self::Application => "application deployment",
        }
    }

    /// The operator word for one deployment with its article, for example `a model deployment`.
    pub(crate) fn indefinite_noun(self) -> &'static str {
        match self {
            Self::Model => "a model deployment",
            Self::Application => "an application deployment",
        }
    }

    /// The command that lists the group's deployments.
    pub(crate) fn list_command(self) -> &'static str {
        match self {
            Self::Model => "dotc model list",
            Self::Application => "dotc application list",
        }
    }

    /// The title line of a deployment's result record block, for example `Model deployment`.
    pub(crate) fn record_title(self) -> &'static str {
        match self {
            Self::Model => "Model deployment",
            Self::Application => "Application deployment",
        }
    }

    /// The prefix of a deployment's result record keys, for example `DOTCORE_MODEL_DEPLOYMENT`.
    pub(crate) fn record_prefix(self) -> &'static str {
        match self {
            Self::Model => "DOTCORE_MODEL_DEPLOYMENT",
            Self::Application => "DOTCORE_APPLICATION_DEPLOYMENT",
        }
    }

    /// What a teardown removes, asked after the deployment name in the confirmation.
    pub(crate) fn teardown_consequence(self) -> &'static str {
        match self {
            Self::Model => "This removes the deployment and its running workload resources.",
            Self::Application => {
                "This stops and removes its workloads and deletes its application data. The \
                 catalog application is kept."
            }
        }
    }
}

/// A deployment the operator can name by its id or its name.
pub(crate) trait Addressable {
    /// The stable deployment id.
    fn id(&self) -> &str;

    /// The name shown to the operator, which need not be unique.
    fn name(&self) -> &str;

    /// The name as the manager stores it, empty when none; it also names the deployment.
    fn stored_name(&self) -> &str;
}

/// Finds the deployment an operator reference names: an exact id first, else the one deployment
/// whose shown or stored name is exactly the reference.
pub(crate) fn resolve<'a, T: Addressable>(
    group: DeploymentGroup,
    reference: &str,
    deployments: &'a [T],
) -> Result<&'a T, ReferenceError> {
    if let Some(deployment) = deployments
        .iter()
        .find(|deployment| deployment.id() == reference)
    {
        return Ok(deployment);
    }

    let named = deployments
        .iter()
        .filter(|deployment| {
            deployment.name() == reference
                || (!deployment.stored_name().is_empty() && deployment.stored_name() == reference)
        })
        .collect::<Vec<_>>();
    match named.as_slice() {
        [] => Err(ReferenceError::Unknown {
            group,
            reference: reference.to_owned(),
        }),
        [deployment] => Ok(deployment),
        several => Err(ReferenceError::Ambiguous {
            group,
            reference: reference.to_owned(),
            ids: several
                .iter()
                .map(|deployment| deployment.id().to_owned())
                .collect(),
        }),
    }
}

/// Builds the mapping of a failed management call to the error shown to the operator, naming
/// the request in operator words, for example `call_failed(DeploymentGroup::Model, "pause Gemma")`.
pub(crate) fn call_failed(
    group: DeploymentGroup,
    action: impl Into<String>,
) -> impl FnOnce(Status) -> ManagementError {
    move |status| ManagementError::Call {
        group,
        action: action.into(),
        code: status.code(),
        message: status.message().to_owned(),
    }
}

/// Collapses a free-text value to one line, treating control characters as spaces, so it cannot
/// forge an extra record line or break a terminal layout.
pub(crate) fn single_line(value: &str) -> String {
    value
        .split(|character: char| character.is_whitespace() || character.is_control())
        .filter(|word| !word.is_empty())
        .collect::<Vec<_>>()
        .join(" ")
}

/// A management address that `--manager` or `DOTCORE_MANAGEMENT_ADDR` cannot use.
#[derive(Debug, thiserror::Error)]
pub(crate) enum AddressError {
    #[error("manager address is empty")]
    Empty,

    #[error("manager address {address:?} is not host:port or http://host:port")]
    Invalid {
        address: String,
        #[source]
        source: InvalidUri,
    },

    #[error("manager address {address:?} uses {scheme}://; the management API takes http:// only")]
    UnsupportedScheme { address: String, scheme: String },

    #[error("manager address {address:?} must be host:port or http://host:port")]
    Shape { address: String },
}

/// A failure talking to the site manager.
#[derive(Debug, thiserror::Error)]
pub(crate) enum ManagementError {
    /// The manager did not accept a connection.
    #[error("cannot reach the manager at {address}")]
    Unreachable {
        address: ManagerAddress,
        #[source]
        source: tonic::transport::Error,
    },

    /// The manager answered a request with an error status. The status message is kept as text
    /// because the status type renders its metadata in its own message.
    #[error("cannot {action}: the manager returned {code:?}: {message}")]
    Call {
        group: DeploymentGroup,
        action: String,
        code: Code,
        message: String,
    },

    /// The manager answered a lifecycle request without moving the deployment to the asked
    /// desired state.
    #[error("the manager did not accept the request to {action}; its desired state is {desired}")]
    NotAccepted {
        group: DeploymentGroup,
        action: String,
        desired: String,
    },
}

/// An operator reference that names no deployment, or several.
#[derive(Debug, thiserror::Error)]
pub(crate) enum ReferenceError {
    #[error("no {} is named or has id {reference:?}", .group.noun())]
    Unknown {
        group: DeploymentGroup,
        reference: String,
    },

    #[error("{} {}s are named {reference:?}: {}", .ids.len(), .group.noun(), .ids.join(", "))]
    Ambiguous {
        group: DeploymentGroup,
        reference: String,
        ids: Vec<String>,
    },
}

#[cfg(test)]
mod tests {
    use clap::{CommandFactory, Parser};
    use dotcore_proto::dotcore::site::v1::ConfiguredExecutionTarget;

    use super::*;

    #[derive(Debug, Parser)]
    struct Harness {
        #[command(flatten)]
        manager: ManagerArgs,
    }

    struct Named {
        id: &'static str,
        name: &'static str,
        stored: &'static str,
    }

    impl Addressable for Named {
        fn id(&self) -> &str {
            self.id
        }

        fn name(&self) -> &str {
            self.name
        }

        fn stored_name(&self) -> &str {
            self.stored
        }
    }

    fn address(text: &str) -> Result<String, String> {
        text.parse::<ManagerAddress>()
            .map(|address| address.to_string())
            .map_err(|error| error.to_string())
    }

    fn deployments() -> Vec<Named> {
        vec![
            Named {
                id: "model-1",
                name: "Gemma",
                stored: "",
            },
            Named {
                id: "model-2",
                name: "Qwen",
                stored: "",
            },
            Named {
                id: "model-3",
                name: "Qwen",
                stored: "",
            },
            Named {
                id: "model-4",
                name: "model-1",
                stored: "",
            },
            Named {
                id: "app-1",
                name: "Open WebUI",
                stored: "open-webui-1",
            },
        ]
    }

    #[test]
    fn addresses_take_an_optional_http_scheme_and_normalize_to_it() {
        assert_eq!(
            address("127.0.0.1:50051"),
            Ok("http://127.0.0.1:50051".into())
        );
        assert_eq!(
            address("localhost:7000"),
            Ok("http://localhost:7000".into())
        );
        assert_eq!(address("[::1]:50051"), Ok("http://[::1]:50051".into()));
        assert_eq!(
            address("http://10.0.0.5:50051"),
            Ok("http://10.0.0.5:50051".into())
        );
        assert_eq!(
            address("http://10.0.0.5:50051/"),
            Ok("http://10.0.0.5:50051".into())
        );
    }

    #[test]
    fn addresses_without_a_port_or_with_extra_parts_are_refused() {
        assert_eq!(address(""), Err("manager address is empty".into()));
        assert_eq!(
            address("https://10.0.0.5:50051"),
            Err(
                "manager address \"https://10.0.0.5:50051\" uses https://; the management API \
                 takes http:// only"
                    .into()
            )
        );
        for shape in [
            "10.0.0.5",
            "http://10.0.0.5",
            "http://10.0.0.5:50051/api",
            "http://10.0.0.5:50051?x=1",
            "http://user@10.0.0.5:50051",
        ] {
            assert_eq!(
                address(shape),
                Err(format!(
                    "manager address {shape:?} must be host:port or http://host:port"
                )),
                "{shape}"
            );
        }
        assert_eq!(
            address("not an address"),
            Err("manager address \"not an address\" is not host:port or http://host:port".into())
        );
    }

    #[test]
    fn the_manager_flag_defaults_to_the_local_manager_and_reads_the_environment() {
        let command = Harness::command();
        let flag = command
            .get_arguments()
            .find(|argument| argument.get_id() == "manager")
            .expect("manager flag");

        assert_eq!(flag.get_long(), Some("manager"));
        assert_eq!(
            flag.get_env(),
            Some(std::ffi::OsStr::new("DOTCORE_MANAGEMENT_ADDR"))
        );
        assert_eq!(flag.get_default_values(), ["127.0.0.1:50051"]);
        assert_eq!(
            DEFAULT_MANAGEMENT_ADDR
                .parse::<ManagerAddress>()
                .expect("default address")
                .to_string(),
            "http://127.0.0.1:50051"
        );
    }

    #[test]
    fn the_manager_flag_parses_and_rejects_addresses_as_usage_errors() {
        let parsed = Harness::try_parse_from(["harness", "--manager", "10.0.0.5:50051"])
            .expect("valid address");
        assert_eq!(parsed.manager.manager.to_string(), "http://10.0.0.5:50051");

        let error = Harness::try_parse_from(["harness", "--manager", "10.0.0.5"])
            .expect_err("address without a port");
        assert_eq!(error.kind(), clap::error::ErrorKind::ValueValidation);
        assert_eq!(error.exit_code(), 2);
    }

    #[test]
    fn references_match_an_id_before_a_name() {
        let deployments = deployments();

        assert_eq!(
            resolve(DeploymentGroup::Model, "model-2", &deployments)
                .map(|deployment| deployment.id)
                .ok(),
            Some("model-2")
        );
        assert_eq!(
            resolve(DeploymentGroup::Model, "Gemma", &deployments)
                .map(|deployment| deployment.id)
                .ok(),
            Some("model-1")
        );
        assert_eq!(
            resolve(DeploymentGroup::Model, "model-1", &deployments)
                .map(|deployment| deployment.id)
                .ok(),
            Some("model-1"),
            "an id wins over another deployment named like it"
        );
    }

    #[test]
    fn references_match_the_stored_name_as_well_as_the_shown_name() {
        let deployments = deployments();

        for reference in ["open-webui-1", "Open WebUI", "app-1"] {
            assert_eq!(
                resolve(DeploymentGroup::Application, reference, &deployments)
                    .map(|deployment| deployment.id)
                    .ok(),
                Some("app-1"),
                "{reference}"
            );
        }
        assert_eq!(
            resolve(DeploymentGroup::Application, "", &deployments)
                .err()
                .expect("an empty stored name never matches")
                .to_string(),
            "no application deployment is named or has id \"\""
        );
    }

    #[test]
    fn unknown_and_ambiguous_references_fail_with_the_matching_ids() {
        let deployments = deployments();

        let unknown = resolve(DeploymentGroup::Model, "gemma", &deployments)
            .err()
            .expect("names match exactly");
        assert_eq!(
            unknown.to_string(),
            "no model deployment is named or has id \"gemma\""
        );

        let ambiguous = resolve(DeploymentGroup::Application, "Qwen", &deployments)
            .err()
            .expect("two deployments share the name");
        assert!(matches!(
            &ambiguous,
            ReferenceError::Ambiguous { ids, .. } if ids == &["model-2", "model-3"]
        ));
        assert_eq!(
            ambiguous.to_string(),
            "2 application deployments are named \"Qwen\": model-2, model-3"
        );
    }

    #[test]
    fn single_line_collapses_whitespace_and_control_characters() {
        assert_eq!(single_line("  a\n\tb \u{1b}[2J c\r\n"), "a b [2J c");
        assert_eq!(single_line("\n\u{7}"), "");
    }

    #[test]
    fn node_names_fall_back_to_the_node_id() {
        let target = |node: &str, name: &str| ConfiguredExecutionTarget {
            site_node_id: node.to_owned(),
            site_node_display_name: name.to_owned(),
            ..Default::default()
        };
        let names = NodeNames::from_targets(&[
            target("node-1", "gb10-host"),
            target("node-1", "gb10-host"),
            target("node-2", ""),
        ]);

        assert_eq!(names.display("node-1"), "gb10-host");
        assert_eq!(names.display("node-2"), "node-2");
        assert_eq!(names.display("node-3"), "node-3");
    }

    #[test]
    fn target_names_fall_back_to_the_target_id() {
        let target = |node: &str, id: &str, name: &str| ConfiguredExecutionTarget {
            site_node_id: node.to_owned(),
            execution_target_id: id.to_owned(),
            display_name: name.to_owned(),
            ..Default::default()
        };
        let names = NodeNames::from_targets(&[
            target("node-1", "target-a", "Docker applications"),
            target("node-2", "target-a", "Simulated models"),
            target("node-2", "target-b", ""),
        ]);

        assert_eq!(
            names.target_display("node-1", "target-a"),
            "Docker applications"
        );
        assert_eq!(
            names.target_display("node-2", "target-a"),
            "Simulated models"
        );
        assert_eq!(names.target_display("node-2", "target-b"), "target-b");
        assert_eq!(names.target_display("node-3", "target-c"), "target-c");
    }

    #[test]
    fn failed_calls_keep_the_code_and_the_manager_message() {
        let error =
            call_failed(DeploymentGroup::Model, "pause Gemma")(Status::failed_precondition(
                "model lifecycle operation is not valid for the current state",
            ));

        assert_eq!(
            error.to_string(),
            "cannot pause Gemma: the manager returned FailedPrecondition: model lifecycle \
             operation is not valid for the current state"
        );
    }
}
