//! Read-only application deployment-log resolution from current immutable launch declarations.
//!
//! The manager reads a deployment's logs from its hosting node, in process for the node sharing
//! its process and over the private deployment-log service otherwise. The node serves the output
//! of each preparation step it ran for the deployment's services, resolves its own group from the
//! installed assignment and its pinned launch identities, exactly as it launches it, and redacts
//! the launch environment values from both before any entry leaves it.

use std::{future::Future, net::Ipv4Addr, sync::Arc};

use anyhow::anyhow;
use dotcore_artifact::PreparationStep;
use dotcore_proto::dotcore::{
    application::v1::{ApplicationDeploymentDeclaration, ApplicationDeploymentDesiredState},
    deployment::private::v1::DeploymentOwnerKind,
};
use dotcore_runtime::{
    RuntimeBackendError, RuntimeLogSource, RuntimeLogSourceHistory, RuntimeLogSourceSelection,
    RuntimeLogSourceStream, WorkloadGroupLaunch,
};
use dotcore_site::view::fencing::AcceptedApplicationOwnerInput;

use super::{
    ApplicationController, ApplicationLifecycleError, ApplicationOwner, InferenceRouteResolver,
    NodeRealizationRecords, classify_application_assignment, launch::workload_group_launch,
    reconciliation::with_launch_identities,
};
use crate::application::repository::{
    ApplicationDeclarationRepository, CurrentApplicationRepository,
};
use crate::{
    deployment_logs::{
        DeploymentLogOwner, DeploymentLogOwnerFuture, NodeDeploymentLogs, NodeLogRequest,
        NodeLogSelection, OwnerLogSources, PreparationLogs, PreparationOwner, WorkloadLifecycle,
        service_steps, step_from_source_name, step_source,
    },
    dispatch::application::ApplicationRuntimeMap,
    management_logs::{
        DeploymentLogError, DeploymentLogFollowQuery, DeploymentLogReadQuery, DeploymentLogStreams,
        ProtectedLogValues,
    },
};

/// Read-only owner boundary for one application deployment's runtime diagnostics.
pub trait ApplicationDeploymentLogController: Send + Sync {
    fn read_deployment_logs(
        &self,
        deployment_id: String,
        query: DeploymentLogReadQuery,
    ) -> impl Future<Output = Result<Vec<RuntimeLogSourceHistory>, DeploymentLogError>> + Send;

    fn follow_deployment_logs(
        &self,
        deployment_id: String,
        query: DeploymentLogFollowQuery,
    ) -> impl Future<Output = Result<DeploymentLogStreams, DeploymentLogError>> + Send;

    fn protected_log_values(
        &self,
        deployment_id: String,
    ) -> impl Future<Output = Result<ProtectedLogValues, DeploymentLogError>> + Send;
}

impl<D, S> ApplicationDeploymentLogController for ApplicationController<D, S>
where
    D: ApplicationDeclarationRepository + 'static,
    S: CurrentApplicationRepository + 'static,
{
    fn read_deployment_logs(
        &self,
        deployment_id: String,
        query: DeploymentLogReadQuery,
    ) -> impl Future<Output = Result<Vec<RuntimeLogSourceHistory>, DeploymentLogError>> + Send {
        let shared = Arc::clone(&self.owner);
        async move {
            let location = locate(&shared, &deployment_id, query.source_names())?;
            location
                .logs
                .read(location.request(&deployment_id), &query)
                .await
        }
    }

    fn follow_deployment_logs(
        &self,
        deployment_id: String,
        query: DeploymentLogFollowQuery,
    ) -> impl Future<Output = Result<DeploymentLogStreams, DeploymentLogError>> + Send {
        let shared = Arc::clone(&self.owner);
        async move {
            let location = locate(&shared, &deployment_id, query.source_names())?;
            location
                .logs
                .follow(location.request(&deployment_id), &query)
                .await
        }
    }

    /// The hosting node redacts its launch values before entries leave it, so the owner adds
    /// none.
    fn protected_log_values(
        &self,
        deployment_id: String,
    ) -> impl Future<Output = Result<ProtectedLogValues, DeploymentLogError>> + Send {
        let shared = Arc::clone(&self.owner);
        async move {
            locate(&shared, &deployment_id, &[])?;
            Ok(ProtectedLogValues::default())
        }
    }
}

impl<D, S> ApplicationController<D, S> {
    /// Installs the manager's reader of deployment logs on their hosting nodes.
    pub(crate) fn set_node_log_source(&self, logs: NodeDeploymentLogs) {
        *self
            .owner
            .node_logs
            .write()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(logs);
    }
}

/// Node-side log owner for the application deployments assigned in the node's installed
/// view.
///
/// It serves the preparation steps the node retains for a deployment's services ahead of its
/// workloads, and compiles each group from the installed assignment, with the launch identities
/// this node pinned and the route name the view projects, exactly as the node's realizer
/// launches it. A deployment this node has not started realizing, one assigned for teardown, or
/// one whose assignment is invalid for this node reads as not created, while its steps stay
/// readable. A follow of every source also receives the workload changes the realizer
/// publishes, so it can announce each container again.
pub(crate) struct NodeApplicationLogs {
    input: AcceptedApplicationOwnerInput,
    records: Arc<dyn NodeRealizationRecords>,
    routes: Arc<dyn InferenceRouteResolver>,
    runtimes: ApplicationRuntimeMap,
    publication_host: Ipv4Addr,
    preparation: PreparationLogs,
    lifecycle: WorkloadLifecycle,
}

impl NodeApplicationLogs {
    pub(super) fn new(
        input: AcceptedApplicationOwnerInput,
        records: Arc<dyn NodeRealizationRecords>,
        routes: Arc<dyn InferenceRouteResolver>,
        runtimes: ApplicationRuntimeMap,
        publication_host: Ipv4Addr,
        preparation: PreparationLogs,
        lifecycle: WorkloadLifecycle,
    ) -> Self {
        Self {
            input,
            records,
            routes,
            runtimes,
            publication_host,
            preparation,
            lifecycle,
        }
    }

    fn resolve(
        &self,
        deployment_id: &str,
        requested_sources: &[String],
    ) -> Result<NodeDeploymentSources, DeploymentLogError> {
        let Some(installed) = self.input.installed() else {
            // Without an installed view, as after a manager restart cleared it, this incarnation
            // realizes nothing and knows no service, so only the named sources can be answered:
            // retained steps from the store, the other names as not created. No launch value is
            // known to redact from step lines then.
            if requested_sources.is_empty() {
                return Err(DeploymentLogError::NotFound);
            }
            let (steps, workload): (Vec<_>, Vec<_>) = requested_sources
                .iter()
                .map(|name| match step_from_source_name(name) {
                    Some(step) if step_owner_is_application(&step) => Ok(step),
                    _ => Err(RuntimeLogSource {
                        name: name.clone(),
                        label: name.clone(),
                    }),
                })
                .partition(Result::is_ok);
            return Ok(NodeDeploymentSources {
                steps: steps.into_iter().filter_map(Result::ok).collect(),
                known_steps: Vec::new(),
                workload: NodeGroupSource::NotCreated(
                    RuntimeLogSourceSelection::new(
                        workload.into_iter().filter_map(Result::err).collect(),
                    )
                    .map_err(DeploymentLogError::configuration)?,
                ),
                protected: ProtectedLogValues::default(),
            });
        };
        let (assignment, declaration) = installed
            .assignments
            .iter()
            .find_map(|assignment| {
                assignment
                    .declaration
                    .as_ref()
                    .filter(|declaration| declaration.application_deployment_id == deployment_id)
                    .map(|declaration| (assignment, declaration))
            })
            .ok_or(DeploymentLogError::NotFound)?;
        let services = service_sources(declaration)?;
        let known_steps = step_sources(&services);
        let split = self.preparation.select(
            PreparationOwner::Application,
            deployment_id,
            requested_sources,
            &known_steps,
            services.len(),
        );
        let not_created = |selection| NodeDeploymentSources {
            steps: split.steps.clone(),
            known_steps: known_steps.clone(),
            workload: NodeGroupSource::NotCreated(selection),
            protected: ProtectedLogValues::new(attachment_values(declaration)),
        };
        let Some(workload_names) = &split.workload else {
            return Ok(not_created(
                RuntimeLogSourceSelection::new(Vec::new())
                    .map_err(DeploymentLogError::configuration)?,
            ));
        };
        let selection = select_sources(&services, workload_names)?;
        let realizable = classify_application_assignment(
            &installed.node_id,
            &installed.execution_targets,
            assignment,
        )
        .is_ok()
            && declaration.desired_state != ApplicationDeploymentDesiredState::Teardown as i32;
        let realization = match realizable {
            true => self
                .records
                .realization(deployment_id)
                .map_err(DeploymentLogError::configuration)?,
            false => None,
        };
        let Some(realization) = realization else {
            return Ok(not_created(selection));
        };
        let effective =
            with_launch_identities(declaration, &realization.resolved_launch_identities);
        let local = local_group(
            &effective,
            self.publication_host,
            self.routes.as_ref(),
            selection,
        )?;
        Ok(NodeDeploymentSources {
            steps: split.steps,
            known_steps,
            protected: local.protected.clone(),
            workload: NodeGroupSource::Readable(Box::new(local)),
        })
    }
}

impl DeploymentLogOwner for NodeApplicationLogs {
    fn read_logs<'a>(
        &'a self,
        deployment_id: &'a str,
        query: DeploymentLogReadQuery,
    ) -> DeploymentLogOwnerFuture<'a, OwnerLogSources<RuntimeLogSourceHistory>> {
        Box::pin(async move {
            let resolved = self.resolve(deployment_id, query.source_names())?;
            let mut sources = self
                .preparation
                .read(deployment_id, &resolved.steps, query.history())
                .await?;
            match resolved.workload {
                NodeGroupSource::Readable(local) => {
                    sources.extend(read_group(&self.runtimes, &local, &query).await?);
                }
                NodeGroupSource::NotCreated(selection) => {
                    sources.extend(selection.sources().iter().map(|source| {
                        RuntimeLogSourceHistory {
                            source: source.clone(),
                            result: Err(RuntimeBackendError::workload_not_created()),
                        }
                    }));
                }
            }
            Ok(OwnerLogSources::protected(sources, resolved.protected))
        })
    }

    fn follow_logs<'a>(
        &'a self,
        deployment_id: &'a str,
        query: DeploymentLogFollowQuery,
    ) -> DeploymentLogOwnerFuture<'a, OwnerLogSources<RuntimeLogSourceStream>> {
        Box::pin(async move {
            // Subscribing before resolving the group means no change falls between the two.
            let changes = query
                .source_names()
                .is_empty()
                .then(|| self.lifecycle.subscribe(deployment_id));
            let resolved = self.resolve(deployment_id, query.source_names())?;
            let mut sources = self.preparation.follow(
                deployment_id,
                &resolved.steps,
                query.resume_positions(),
                query.cancellation(),
            );
            // A follow of every source also lists the steps that start while it runs.
            let late = query.source_names().is_empty().then(|| {
                self.preparation.follow_started(
                    PreparationOwner::Application,
                    deployment_id,
                    resolved.known_steps.clone(),
                    &resolved.steps,
                    resolved.workload.source_count(),
                    &query,
                )
            });
            match resolved.workload {
                NodeGroupSource::Readable(local) => {
                    sources.extend(follow_group(&self.runtimes, &local, &query).await?);
                }
                NodeGroupSource::NotCreated(selection) => {
                    sources.extend(selection.sources().iter().map(|source| {
                        RuntimeLogSourceStream {
                            source: source.clone(),
                            result: Err(RuntimeBackendError::workload_not_created()),
                        }
                    }));
                }
            }
            Ok(OwnerLogSources::protected(sources, resolved.protected)
                .with_late(late)
                .with_changes(changes))
        })
    }
}

/// What a node serves for one assigned deployment: its selected preparation steps, its
/// workload sources, and the launch values redacted from both.
struct NodeDeploymentSources {
    steps: Vec<PreparationStep>,

    /// Every step the deployment's services may run.
    known_steps: Vec<PreparationStep>,
    workload: NodeGroupSource,
    protected: ProtectedLogValues,
}

/// How a node reads one assigned deployment's workload logs.
enum NodeGroupSource {
    /// Its group may exist, so the exact local runtime is read.
    Readable(Box<LocalGroup>),

    /// No group can exist on this node, so every selected source reads as not created.
    NotCreated(RuntimeLogSourceSelection),
}

impl NodeGroupSource {
    /// Number of workload sources selected.
    fn source_count(&self) -> usize {
        match self {
            Self::Readable(local) => local.selection.sources().len(),
            Self::NotCreated(selection) => selection.sources().len(),
        }
    }
}

/// The hosting node of a deployment, the reader that reaches it, and the selected sources.
struct LogLocation {
    logs: NodeDeploymentLogs,
    node_id: String,

    /// Every source the deployment may have: its services and their preparation steps.
    known: Vec<RuntimeLogSource>,

    /// The requested sources, or the services when the request names none.
    sources: Vec<RuntimeLogSource>,

    /// Whether the request names no source, so the node answers with every source it holds.
    all: bool,
}

impl LogLocation {
    fn request<'a>(&'a self, deployment_id: &'a str) -> NodeLogRequest<'a> {
        NodeLogRequest {
            node_id: &self.node_id,
            owner_kind: DeploymentOwnerKind::Application,
            deployment_id,
            selection: if self.all {
                NodeLogSelection::All {
                    known: &self.known,
                    defaults: &self.sources,
                }
            } else {
                NodeLogSelection::Named(&self.sources)
            },
        }
    }
}

/// The exact local group, selected sources, and protected launch values of one deployment.
struct LocalGroup {
    execution_target_id: String,
    group: WorkloadGroupLaunch,
    selection: RuntimeLogSourceSelection,
    protected: ProtectedLogValues,
}

/// Resolves the node hosting a deployment and the sources selected among its services.
fn locate<D, S>(
    shared: &ApplicationOwner<D, S>,
    deployment_id: &str,
    requested_sources: &[String],
) -> Result<LogLocation, DeploymentLogError>
where
    D: ApplicationDeclarationRepository,
    S: CurrentApplicationRepository,
{
    let snapshot = shared
        .desired
        .snapshot()
        .map_err(DeploymentLogError::configuration)?;
    let deployment = snapshot
        .deployments
        .iter()
        .find(|deployment| deployment.application_deployment_id == deployment_id)
        .ok_or(DeploymentLogError::NotFound)?;
    let node_id = deployment
        .placement
        .as_ref()
        .ok_or_else(|| {
            DeploymentLogError::configuration(anyhow!("Application exact placement is missing"))
        })?
        .site_node_id
        .clone();
    let services = service_sources(deployment)?;
    let known = services
        .iter()
        .cloned()
        .chain(step_sources(&services).iter().map(step_source))
        .collect::<Vec<_>>();
    let all = requested_sources.is_empty();
    let sources = if all {
        services
    } else {
        select_sources(&known, requested_sources)?
            .sources()
            .to_vec()
    };
    let logs = shared
        .node_logs
        .read()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .clone()
        .ok_or(DeploymentLogError::NotFound)?;
    Ok(LogLocation {
        logs,
        node_id,
        known,
        sources,
        all,
    })
}

/// Compiles the exact group of a deployment hosted on this node.
fn local_group(
    deployment: &ApplicationDeploymentDeclaration,
    publication_host: Ipv4Addr,
    inference_route_resolver: &dyn InferenceRouteResolver,
    selection: RuntimeLogSourceSelection,
) -> Result<LocalGroup, DeploymentLogError> {
    let execution_target_id = deployment
        .placement
        .as_ref()
        .ok_or_else(|| {
            DeploymentLogError::configuration(anyhow!("Application exact placement is missing"))
        })?
        .execution_target_id
        .clone();
    let group = workload_group_launch(deployment, publication_host, inference_route_resolver)
        .map_err(map_lifecycle_error)?;
    let protected = ProtectedLogValues::new(
        group
            .units
            .values()
            .flat_map(|unit| unit.environment.values().cloned()),
    );
    Ok(LocalGroup {
        execution_target_id,
        group,
        selection,
        protected,
    })
}

async fn read_group(
    runtimes: &ApplicationRuntimeMap,
    local: &LocalGroup,
    query: &DeploymentLogReadQuery,
) -> Result<Vec<RuntimeLogSourceHistory>, DeploymentLogError> {
    runtimes
        .read_workload_group_logs(
            &local.execution_target_id,
            &local.group,
            &local.selection,
            query.history(),
        )
        .await
        .map_err(DeploymentLogError::configuration)
}

async fn follow_group(
    runtimes: &ApplicationRuntimeMap,
    local: &LocalGroup,
    query: &DeploymentLogFollowQuery,
) -> Result<Vec<RuntimeLogSourceStream>, DeploymentLogError> {
    let follow = query
        .group_query(&local.selection)
        .map_err(DeploymentLogError::configuration)?;
    runtimes
        .follow_workload_group_logs(
            &local.execution_target_id,
            &local.group,
            &local.selection,
            follow,
        )
        .await
        .map_err(DeploymentLogError::configuration)
}

/// Returns the workload source of each declared service, in graph order.
fn service_sources(
    deployment: &ApplicationDeploymentDeclaration,
) -> Result<Vec<RuntimeLogSource>, DeploymentLogError> {
    Ok(deployment
        .snapshot
        .as_ref()
        .and_then(|snapshot| snapshot.graph.as_ref())
        .ok_or_else(|| DeploymentLogError::configuration(anyhow!("Application graph is missing")))?
        .services
        .iter()
        .map(|service| RuntimeLogSource {
            name: service.service_name.clone(),
            label: service.service_name.clone(),
        })
        .collect())
}

/// Whether a step is one an application service runs.
fn step_owner_is_application(step: &PreparationStep) -> bool {
    matches!(
        step,
        PreparationStep::ServiceBuild { .. } | PreparationStep::ServiceImage { .. }
    )
}

/// Returns the preparation steps the services may run, service by service.
fn step_sources(services: &[RuntimeLogSource]) -> Vec<PreparationStep> {
    services
        .iter()
        .flat_map(|service| service_steps(&service.name))
        .collect()
}

/// Returns the configuration and secret values the deployment's launch receives, which step
/// lines must not reveal either.
fn attachment_values(
    deployment: &ApplicationDeploymentDeclaration,
) -> impl Iterator<Item = String> {
    deployment
        .configuration_attachments
        .iter()
        .map(|attachment| attachment.value.clone())
        .chain(
            deployment
                .secret_attachments
                .iter()
                .map(|attachment| attachment.value.clone()),
        )
}

/// Selects the requested sources among `declared`, or all of them.
fn select_sources(
    declared: &[RuntimeLogSource],
    requested_sources: &[String],
) -> Result<RuntimeLogSourceSelection, DeploymentLogError> {
    let selected = if requested_sources.is_empty() {
        declared.to_vec()
    } else {
        requested_sources
            .iter()
            .map(|name| {
                declared
                    .iter()
                    .find(|source| source.name == *name)
                    .cloned()
                    .ok_or(DeploymentLogError::InvalidSource)
            })
            .collect::<Result<Vec<_>, _>>()?
    };
    RuntimeLogSourceSelection::new(selected).map_err(DeploymentLogError::configuration)
}

fn map_lifecycle_error(error: ApplicationLifecycleError) -> DeploymentLogError {
    match error {
        ApplicationLifecycleError::NotFound => DeploymentLogError::NotFound,
        other => DeploymentLogError::configuration(other),
    }
}

#[cfg(test)]
mod tests {
    use std::{num::NonZeroU16, path::Path};

    use dotcore_domain::timing::Timing;
    use dotcore_proto::dotcore::application::v1::ApplicationDeploymentDesiredState;
    use dotcore_runtime::RuntimeBackendError;
    use dotcore_site::{
        node::control::ManagerNodeControlCoordinator, private_transport::PrivateNodeRole,
        repository::SiteRepository, store::InMemoryCurrentState, view::fencing::ManagerSession,
        worker_channels::WorkerChannels,
    };

    use super::*;
    use crate::application::controller::test_support::{
        TestApplicationDeclarationRepository, deployment, owner_controller, site, test_directory,
    };

    #[tokio::test]
    async fn logs_are_read_from_the_hosting_node() {
        let desired = TestApplicationDeclarationRepository::open(Path::new("/unused"), "site-one")
            .expect("desired repository");
        let controller = owner_controller(
            desired.clone(),
            InMemoryCurrentState::new("site-one").expect("current state"),
        );
        let mut remote = deployment(ApplicationDeploymentDesiredState::Present);
        remote.placement.as_mut().expect("placement").site_node_id = "node-two".to_owned();
        desired.seed(remote);
        let query = || {
            DeploymentLogReadQuery::new(
                Vec::new(),
                dotcore_runtime::RuntimeLogQuery::new(NonZeroU16::new(2).expect("limit"), None)
                    .expect("query"),
            )
        };

        let without_node_logs = controller
            .read_deployment_logs("open-webui-1".to_owned(), query())
            .await;
        controller.set_node_log_source(disconnected_node_logs());
        let histories = controller
            .read_deployment_logs("open-webui-1".to_owned(), query())
            .await
            .expect("a disconnected node is not an error");
        let follow = controller
            .follow_deployment_logs(
                "open-webui-1".to_owned(),
                DeploymentLogFollowQuery::new(
                    Vec::new(),
                    Default::default(),
                    tokio_util::sync::CancellationToken::new(),
                ),
            )
            .await;
        let invalid = controller
            .read_deployment_logs(
                "open-webui-1".to_owned(),
                DeploymentLogReadQuery::new(
                    vec!["node".to_owned()],
                    dotcore_runtime::RuntimeLogQuery::new(NonZeroU16::new(2).expect("limit"), None)
                        .expect("query"),
                ),
            )
            .await;
        let protected = controller
            .protected_log_values("open-webui-1".to_owned())
            .await
            .expect("protected values");

        assert!(matches!(
            without_node_logs,
            Err(DeploymentLogError::NotFound)
        ));
        assert_eq!(
            histories
                .iter()
                .map(|history| (
                    history.source.name.as_str(),
                    matches!(
                        history.result,
                        Err(RuntimeBackendError::RemoteLogUnavailable)
                    )
                ))
                .collect::<Vec<_>>(),
            [("web", true)]
        );
        assert!(
            matches!(follow, Err(DeploymentLogError::NodeUnavailable)),
            "a follow fails while its node is unreachable"
        );
        assert!(matches!(invalid, Err(DeploymentLogError::InvalidSource)));
        assert_eq!(protected, ProtectedLogValues::default());
    }

    /// The manager's node log client with no node connected.
    fn disconnected_node_logs() -> NodeDeploymentLogs {
        let directory = test_directory();
        let path = directory.join("site.yaml");
        std::fs::write(
            &path,
            dotcore_proto::yaml::to_string(&site()).expect("site YAML"),
        )
        .expect("write site YAML");
        let authority = crate::test_support::private_tls::TestAuthority::new();
        let (worker_certificate, _) = authority.issue("site-one", "node-two", true);
        NodeDeploymentLogs::new(
            Some(WorkerChannels::new(
                &authority.private_configuration(
                    "site-one",
                    "node-one",
                    PrivateNodeRole::Manager,
                    authority.issue("site-one", "node-one", false),
                    [(
                        "node-two",
                        PrivateNodeRole::Worker,
                        worker_certificate.as_slice(),
                    )],
                ),
                Timing::test().private,
            )),
            SiteRepository::load(&path, crate::test_support::site_declaration_rules())
                .expect("Site repository"),
            ManagerNodeControlCoordinator::on_system_clock(
                ManagerSession::parse("a".repeat(32)).expect("session"),
            ),
        )
    }
}
