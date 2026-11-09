//! Apps-controller owner operations: create, lifecycle transitions, and metadata acceptance.

use std::sync::Arc;

use dotcore_domain::{SiteNodeId, real_time};
use dotcore_proto::dotcore::{
    application::v1::{
        ApplicationDeploymentDeclaration, ApplicationDeploymentDesiredState,
        ApplicationLifecycleState, CreateApplicationDeploymentRequest,
        CurrentApplicationDeployment, ResolvedApplicationDeploymentSnapshot,
        ResolvedApplicationServiceSelection,
    },
    catalog::v1::application_service_artifact::Kind as ServiceArtifactKind,
    deployment::v1::{ArtifactPreparationPolicy, ExactPlacement},
    site::v1::{ExecutionTarget, Site},
};
use dotcore_site::node::target_inventory::CapabilityObservation;
use tracing::info;

use super::{
    ApplicationLifecycleError, ApplicationOwner, configuration, configuration_with_source,
    current_error, declaration_error, desired_state, eligible_target, partial_current_error,
    validate_id,
};
use crate::application::{
    published_port::{PUBLISHED_PORT_RANGE, allocate_published_port, deployment_start_offset},
    repository::{
        ApplicationDeclarationRepository, ApplicationLifecycleGenerations,
        CurrentApplicationRepository, has_inference_route_projection,
    },
};

/// Validates, persists, and projects a new application deployment.
pub(super) async fn create_deployment<D, S>(
    owner: &Arc<ApplicationOwner<D, S>>,
    request: CreateApplicationDeploymentRequest,
) -> Result<dotcore_proto::dotcore::application::v1::ApplicationDeployment, ApplicationLifecycleError>
where
    D: ApplicationDeclarationRepository,
    S: CurrentApplicationRepository + ApplicationLifecycleGenerations,
{
    let placement = validate_request(&request)?;
    let site = owner.application_site()?;
    let target = eligible_target(&site, &placement)?;
    let selected_platform = require_fresh_matching_platform(owner, target).await?;
    let application = resolve_application(owner, &request.application_id)?;
    validate_application_route_binding(&request, &application)?;
    let declaration_snapshot = owner.desired.snapshot().map_err(declaration_error)?;
    let mut declaration = declaration(
        owner,
        &placement,
        &selected_platform,
        request,
        application,
        &site,
        &declaration_snapshot,
    )?;
    declaration.lifecycle_generation =
        owner.reserve_lifecycle_generation(&declaration.application_deployment_id)?;
    persist_create(owner, &declaration, declaration_snapshot.storage_revision)?;
    let result = super::projection::project(
        &declaration,
        super::queued_state(),
        owner.inference_route_projection(&declaration)?,
    );
    if result.is_ok() {
        info!(
            event_kind = "application_mutation_accepted",
            operation = "create",
            deployment_id = %declaration.application_deployment_id,
            desired_state = "present",
            "Application deployment mutation accepted"
        );
    }
    result
}

/// Records teardown intent, persists its current state, and projects the accepted deployment.
pub(super) fn remove_deployment<D, S>(
    owner: &Arc<ApplicationOwner<D, S>>,
    deployment_id: String,
) -> Result<dotcore_proto::dotcore::application::v1::ApplicationDeployment, ApplicationLifecycleError>
where
    D: ApplicationDeclarationRepository,
    S: CurrentApplicationRepository + ApplicationLifecycleGenerations,
{
    validate_id(&deployment_id)?;
    let declaration_snapshot = owner.desired.snapshot().map_err(declaration_error)?;
    let desired = declaration_snapshot
        .deployments
        .iter()
        .find(|value| value.application_deployment_id == deployment_id)
        .cloned()
        .ok_or(ApplicationLifecycleError::NotFound)?;
    if desired_state(&desired)? == ApplicationDeploymentDesiredState::Teardown {
        return Err(ApplicationLifecycleError::FailedPrecondition);
    }
    let generation = owner.reserve_lifecycle_generation(&deployment_id)?;
    owner
        .desired
        .remove(
            &deployment_id,
            declaration_snapshot.storage_revision,
            generation,
        )
        .map_err(declaration_error)?;
    let state = record_intent(owner, &deployment_id, "Remove", |_| {
        (super::stopping_state(), true)
    })?;
    let mut teardown = desired;
    teardown.desired_state = ApplicationDeploymentDesiredState::Teardown as i32;
    teardown.lifecycle_generation = generation;
    let result = super::projection::project(
        &teardown,
        state,
        owner.inference_route_projection(&teardown)?,
    );
    if result.is_ok() {
        info!(
            event_kind = "application_mutation_accepted",
            operation = "remove",
            deployment_id = %teardown.application_deployment_id,
            desired_state = "teardown",
            "Application deployment mutation accepted"
        );
    }
    result
}

/// Applies validated presentation metadata to one retained deployment and projects current state.
pub(super) fn update_metadata<D, S>(
    owner: &Arc<ApplicationOwner<D, S>>,
    deployment_id: String,
    patch: dotcore_domain::deployment::metadata::DeploymentMetadataPatch,
) -> Result<dotcore_proto::dotcore::application::v1::ApplicationDeployment, ApplicationLifecycleError>
where
    D: ApplicationDeclarationRepository,
    S: CurrentApplicationRepository + ApplicationLifecycleGenerations,
{
    validate_id(&deployment_id)?;
    let snapshot = owner.desired.snapshot().map_err(declaration_error)?;
    owner
        .desired
        .update_metadata(&deployment_id, &patch, snapshot.storage_revision)
        .map_err(declaration_error)?;
    let desired = find_desired(owner, &deployment_id)?;
    let state = current_state(owner, &desired)?;
    let selected = owner.inference_route_projection(&desired)?;
    let result = super::projection::project(&desired, state, selected);
    if result.is_ok() {
        info!(
            event_kind = "application_mutation_accepted",
            operation = "update_metadata",
            deployment_id = %desired.application_deployment_id,
            "Application deployment mutation accepted"
        );
    }
    result
}

/// Persists pause intent, immediately suppresses public readiness, and wakes reconciliation.
pub(super) fn pause_deployment<D, S>(
    owner: &Arc<ApplicationOwner<D, S>>,
    deployment_id: String,
) -> Result<dotcore_proto::dotcore::application::v1::ApplicationDeployment, ApplicationLifecycleError>
where
    D: ApplicationDeclarationRepository,
    S: CurrentApplicationRepository + ApplicationLifecycleGenerations,
{
    validate_id(&deployment_id)?;
    let desired = find_desired(owner, &deployment_id)?;
    match desired_state(&desired)? {
        ApplicationDeploymentDesiredState::Paused => return pause_existing(owner, &desired),
        ApplicationDeploymentDesiredState::Present => {}
        ApplicationDeploymentDesiredState::Teardown
        | ApplicationDeploymentDesiredState::Unspecified => {
            return Err(ApplicationLifecycleError::FailedPrecondition);
        }
    }
    let declaration_snapshot = owner.desired.snapshot().map_err(declaration_error)?;
    let generation = owner.reserve_lifecycle_generation(&deployment_id)?;
    let (desired, persistence_error) = match owner.desired.pause(
        &deployment_id,
        declaration_snapshot.storage_revision,
        generation,
    ) {
        Ok(_) => (find_desired(owner, &deployment_id)?, None),
        Err(error) => {
            let persistence_error = declaration_error(error);
            let desired = find_desired(owner, &deployment_id)?;
            if desired_state(&desired)? != ApplicationDeploymentDesiredState::Paused {
                return Err(persistence_error);
            }
            (desired, Some(persistence_error))
        }
    };
    let state = record_pause_intent(owner, &desired)?;
    if let Some(error) = persistence_error {
        return Err(error);
    }
    let selected = owner.inference_route_projection(&desired)?;
    let result = super::projection::project(&desired, state, selected);
    if result.is_ok() {
        info!(
            event_kind = "application_mutation_accepted",
            operation = "pause",
            deployment_id = %desired.application_deployment_id,
            desired_state = "paused",
            "Application deployment mutation accepted"
        );
    }
    result
}

/// Persists present intent, uses the existing starting lifecycle, and wakes reconciliation.
pub(super) fn resume_deployment<D, S>(
    owner: &Arc<ApplicationOwner<D, S>>,
    deployment_id: String,
) -> Result<dotcore_proto::dotcore::application::v1::ApplicationDeployment, ApplicationLifecycleError>
where
    D: ApplicationDeclarationRepository,
    S: CurrentApplicationRepository + ApplicationLifecycleGenerations,
{
    validate_id(&deployment_id)?;
    let existing = find_desired(owner, &deployment_id)?;
    match desired_state(&existing)? {
        ApplicationDeploymentDesiredState::Present => return resume_existing(owner, &existing),
        ApplicationDeploymentDesiredState::Paused => {}
        ApplicationDeploymentDesiredState::Teardown
        | ApplicationDeploymentDesiredState::Unspecified => {
            return Err(ApplicationLifecycleError::FailedPrecondition);
        }
    }
    let declaration_snapshot = owner.desired.snapshot().map_err(declaration_error)?;
    let generation = owner.reserve_lifecycle_generation(&deployment_id)?;
    owner
        .desired
        .resume(
            &deployment_id,
            declaration_snapshot.storage_revision,
            generation,
        )
        .map_err(declaration_error)?;
    let desired = find_desired(owner, &deployment_id)?;
    let state = record_starting_intent(owner, &desired)?;
    let selected = owner.inference_route_projection(&desired)?;
    let result = super::projection::project(&desired, state, selected);
    if result.is_ok() {
        info!(
            event_kind = "application_mutation_accepted",
            operation = "resume",
            deployment_id = %desired.application_deployment_id,
            desired_state = "present",
            "Application deployment mutation accepted"
        );
    }
    result
}

fn find_desired<D, S>(
    owner: &Arc<ApplicationOwner<D, S>>,
    deployment_id: &str,
) -> Result<ApplicationDeploymentDeclaration, ApplicationLifecycleError>
where
    D: ApplicationDeclarationRepository,
    S: CurrentApplicationRepository + ApplicationLifecycleGenerations,
{
    owner
        .desired
        .snapshot()
        .map_err(declaration_error)?
        .deployments
        .into_iter()
        .find(|deployment| deployment.application_deployment_id == deployment_id)
        .ok_or(ApplicationLifecycleError::NotFound)
}

fn pause_existing<D, S>(
    owner: &Arc<ApplicationOwner<D, S>>,
    desired: &ApplicationDeploymentDeclaration,
) -> Result<dotcore_proto::dotcore::application::v1::ApplicationDeployment, ApplicationLifecycleError>
where
    D: ApplicationDeclarationRepository,
    S: CurrentApplicationRepository + ApplicationLifecycleGenerations,
{
    let result = if pause_current_record_requires_repair(owner, desired)? {
        record_pause_intent(owner, desired)
    } else {
        current_state(owner, desired)
    };
    let selected = owner.inference_route_projection(desired)?;
    let result = super::projection::project(desired, result?, selected);
    if result.is_ok() {
        info!(
            event_kind = "application_mutation_accepted",
            operation = "pause",
            deployment_id = %desired.application_deployment_id,
            desired_state = "paused",
            "Application deployment mutation accepted"
        );
    }
    result
}

fn resume_existing<D, S>(
    owner: &Arc<ApplicationOwner<D, S>>,
    desired: &ApplicationDeploymentDeclaration,
) -> Result<dotcore_proto::dotcore::application::v1::ApplicationDeployment, ApplicationLifecycleError>
where
    D: ApplicationDeclarationRepository,
    S: CurrentApplicationRepository + ApplicationLifecycleGenerations,
{
    let state = owner
        .current
        .snapshot()
        .map_err(current_error)?
        .deployments
        .into_iter()
        .find(|record| record.application_deployment_id == desired.application_deployment_id)
        .and_then(|record| record.state)
        .ok_or_else(|| configuration("Application deployment has no current-state record"))?;
    let state = if matches!(
        ApplicationLifecycleState::try_from(state.lifecycle_state),
        Ok(ApplicationLifecycleState::Pausing | ApplicationLifecycleState::Paused)
    ) {
        record_starting_intent(owner, desired)?
    } else {
        state
    };
    let selected = owner.inference_route_projection(desired)?;
    let result = super::projection::project(desired, state, selected);
    if result.is_ok() {
        info!(
            event_kind = "application_mutation_accepted",
            operation = "resume",
            deployment_id = %desired.application_deployment_id,
            desired_state = "present",
            "Application deployment mutation accepted"
        );
    }
    result
}

fn pause_current_record_requires_repair<D, S>(
    owner: &Arc<ApplicationOwner<D, S>>,
    desired: &ApplicationDeploymentDeclaration,
) -> Result<bool, ApplicationLifecycleError>
where
    D: ApplicationDeclarationRepository,
    S: CurrentApplicationRepository + ApplicationLifecycleGenerations,
{
    let current = owner
        .current
        .snapshot()
        .map_err(current_error)?
        .deployments
        .into_iter()
        .find(|record| record.application_deployment_id == desired.application_deployment_id)
        .and_then(|record| record.state);
    Ok(!current.is_some_and(|state| {
        matches!(
            ApplicationLifecycleState::try_from(state.lifecycle_state),
            Ok(ApplicationLifecycleState::Pausing
                | ApplicationLifecycleState::Paused
                | ApplicationLifecycleState::Unavailable)
        )
    }))
}

fn record_pause_intent<D, S>(
    owner: &Arc<ApplicationOwner<D, S>>,
    desired: &ApplicationDeploymentDeclaration,
) -> Result<
    dotcore_proto::dotcore::application::v1::ApplicationDeploymentState,
    ApplicationLifecycleError,
>
where
    D: ApplicationDeclarationRepository,
    S: CurrentApplicationRepository + ApplicationLifecycleGenerations,
{
    record_intent(
        owner,
        &desired.application_deployment_id,
        "Pause",
        |existing| (super::pausing_state(existing), true),
    )
}

fn current_state<D, S>(
    owner: &Arc<ApplicationOwner<D, S>>,
    desired: &ApplicationDeploymentDeclaration,
) -> Result<
    dotcore_proto::dotcore::application::v1::ApplicationDeploymentState,
    ApplicationLifecycleError,
>
where
    D: ApplicationDeclarationRepository,
    S: CurrentApplicationRepository + ApplicationLifecycleGenerations,
{
    owner
        .current
        .snapshot()
        .map_err(current_error)?
        .deployments
        .into_iter()
        .find(|record| record.application_deployment_id == desired.application_deployment_id)
        .and_then(|record| record.state)
        .ok_or_else(|| configuration("Application deployment has no current-state record"))
}

fn record_starting_intent<D, S>(
    owner: &Arc<ApplicationOwner<D, S>>,
    desired: &ApplicationDeploymentDeclaration,
) -> Result<
    dotcore_proto::dotcore::application::v1::ApplicationDeploymentState,
    ApplicationLifecycleError,
>
where
    D: ApplicationDeclarationRepository,
    S: CurrentApplicationRepository + ApplicationLifecycleGenerations,
{
    record_intent(
        owner,
        &desired.application_deployment_id,
        "Resume",
        |existing| (super::starting_state(existing), true),
    )
}

/// Records lifecycle intent on the latest current record of a declared deployment. `intent`
/// returns the state and whether to write it. The read and the conditional write happen under
/// the declaration guard, the same guard node status acceptance holds, so an accepted status
/// can never land between them; a deployment no longer declared gets no record.
fn record_intent<D, S>(
    owner: &Arc<ApplicationOwner<D, S>>,
    deployment_id: &str,
    operation: &str,
    intent: impl FnOnce(
        Option<&dotcore_proto::dotcore::application::v1::ApplicationDeploymentState>,
    ) -> (
        dotcore_proto::dotcore::application::v1::ApplicationDeploymentState,
        bool,
    ),
) -> Result<
    dotcore_proto::dotcore::application::v1::ApplicationDeploymentState,
    ApplicationLifecycleError,
>
where
    D: ApplicationDeclarationRepository,
    S: CurrentApplicationRepository + ApplicationLifecycleGenerations,
{
    owner
        .desired
        .with_declared_deployment_ids(|declared| {
            let existing = owner
                .current
                .snapshot()
                .map_err(current_error)?
                .deployments
                .into_iter()
                .find(|record| record.application_deployment_id == deployment_id);
            let (state, write) = intent(existing.as_ref().and_then(|record| record.state.as_ref()));
            if write && declared.contains(deployment_id) {
                owner
                    .current
                    .upsert(
                        CurrentApplicationDeployment {
                            application_deployment_id: deployment_id.to_owned(),
                            record_revision: 0,
                            state: Some(state.clone()),
                            reconciled_lifecycle_generation: None,
                        },
                        existing.map(|record| record.record_revision),
                    )
                    .map_err(|error| partial_current_error(operation, error))?;
            }
            Ok(state)
        })
        .map_err(declaration_error)?
}

fn validate_request(
    request: &CreateApplicationDeploymentRequest,
) -> Result<ExactPlacement, ApplicationLifecycleError> {
    validate_id(&request.application_id)?;
    let placement = request
        .placement
        .clone()
        .ok_or(ApplicationLifecycleError::InvalidArgument)?;
    SiteNodeId::parse(&placement.site_node_id)
        .map_err(|_| ApplicationLifecycleError::InvalidArgument)?;
    validate_id(&placement.execution_target_id)?;
    match request.inference_route_id.as_deref() {
        Some(inference_route_id)
            if dotcore_domain::valid_id(inference_route_id)
                && request.expected_inference_route_generation > 0 =>
        {
            Ok(placement)
        }
        None if request.expected_inference_route_generation == 0 => Ok(placement),
        _ => Err(ApplicationLifecycleError::InvalidArgument),
    }
}

fn validate_application_route_binding(
    request: &CreateApplicationDeploymentRequest,
    application: &dotcore_proto::dotcore::catalog::v1::Application,
) -> Result<(), ApplicationLifecycleError> {
    if request.inference_route_id.is_some() && !has_inference_route_projection(application) {
        return Err(ApplicationLifecycleError::InvalidArgument);
    }
    Ok(())
}

fn resolve_application<D, S>(
    owner: &ApplicationOwner<D, S>,
    application_id: &str,
) -> Result<dotcore_proto::dotcore::catalog::v1::Application, ApplicationLifecycleError> {
    owner
        .catalog
        .list_application_releases()
        .into_iter()
        .find(|application| application.application_id == application_id)
        .ok_or(ApplicationLifecycleError::NotFound)
}

/// Builds the declaration of a new deployment on `placement`, with its immutable snapshot for
/// the selected `platform`, the parameter values resolved from the request, and a published host
/// port no other retained deployment holds, even one still tearing down. Local-development launch
/// identities are left empty for the hosting node to pin once on its own image store.
fn declaration<D, S>(
    owner: &Arc<ApplicationOwner<D, S>>,
    placement: &ExactPlacement,
    platform: &dotcore_proto::dotcore::catalog::v1::OciPlatform,
    request: CreateApplicationDeploymentRequest,
    application: dotcore_proto::dotcore::catalog::v1::Application,
    site: &Site,
    snapshot: &crate::application::repository::ApplicationDeclarationSnapshot,
) -> Result<ApplicationDeploymentDeclaration, ApplicationLifecycleError>
where
    D: ApplicationDeclarationRepository,
    S: CurrentApplicationRepository + ApplicationLifecycleGenerations,
{
    let parameter_values = super::parameters::resolve_parameter_values(
        application
            .graph
            .as_ref()
            .ok_or(ApplicationLifecycleError::FailedPrecondition)?,
        &request.parameter_values,
    )?;
    // The link is checked for identity and generation only; route availability never gates creation.
    if let Some(inference_route_id) = request.inference_route_id.as_deref() {
        owner
            .inference_route_resolver
            .resolve_for_create(
                inference_route_id,
                request.expected_inference_route_generation,
            )
            .map_err(ApplicationLifecycleError::InferenceRoute)?;
    }
    let deployment_id = super::identity::allocate_deployment_id(
        &application.application_id,
        real_time::wall_now(),
        snapshot
            .deployments
            .iter()
            .map(|deployment| deployment.application_deployment_id.as_str()),
        (0..32).map(|_| super::identity::random_collision_suffix()),
    )?;
    let published_port = allocate_published_port(
        PUBLISHED_PORT_RANGE,
        deployment_start_offset(&deployment_id),
        &snapshot
            .deployments
            .iter()
            .filter_map(|deployment| u16::try_from(deployment.published_port).ok())
            .collect(),
    )
    .ok_or(ApplicationLifecycleError::PublishedPortsExhausted)?;
    let configuration_attachments = super::launch::configuration_attachments(
        &application,
        |name| {
            owner
                .configuration_sources
                .resolve(name, site, &placement.site_node_id, published_port)
        },
        &parameter_values,
        request.inference_route_id.is_some(),
    )?;
    let storage_attachments = super::launch::storage_attachments(&application)?;

    Ok(ApplicationDeploymentDeclaration {
        application_deployment_id: deployment_id,
        application_deployment_name: dotcore_domain::deployment::name::normalize_or_default(
            request.application_deployment_name.as_deref(),
            &application.display_name,
        )
        .map_err(|_| ApplicationLifecycleError::InvalidName)?,
        placement: Some(placement.clone()),
        snapshot: Some(ResolvedApplicationDeploymentSnapshot {
            parameter_values,
            ..resolve_snapshot(&application, site, placement, platform)?
        }),
        desired_state: ApplicationDeploymentDesiredState::Present as i32,
        storage_attachments,
        configuration_attachments,
        inference_route_id: request.inference_route_id,
        lifecycle_generation: 0,
        node_resolved_launch_identities: Vec::new(),
        published_port: u32::from(published_port.get()),
    })
}

async fn require_fresh_matching_platform<D, S>(
    owner: &ApplicationOwner<D, S>,
    target: &ExecutionTarget,
) -> Result<dotcore_proto::dotcore::catalog::v1::OciPlatform, ApplicationLifecycleError>
where
    D: ApplicationDeclarationRepository,
    S: CurrentApplicationRepository + ApplicationLifecycleGenerations,
{
    let declared = target
        .platform
        .as_ref()
        .ok_or(ApplicationLifecycleError::FailedPrecondition)?;
    let declared_platform =
        dotcore_domain::ExecutionPlatform::try_from(declared).map_err(|source| {
            configuration_with_source("Application target platform is invalid", source)
        })?;
    let inventory = owner
        .target_capabilities
        .observe()
        .await
        .map_err(|source| {
            configuration_with_source("Application target capability observation failed", source)
        })?;
    let capabilities = inventory
        .for_target(&target.execution_target_id)
        .ok_or_else(|| ApplicationLifecycleError::MissingTargetObservation {
            execution_target_id: target.execution_target_id.clone(),
        })?;
    let observed = match &capabilities.platform {
        CapabilityObservation::Available(platform) => platform,
        CapabilityObservation::Stale(_) => {
            return Err(ApplicationLifecycleError::StaleTargetObservation {
                execution_target_id: target.execution_target_id.clone(),
            });
        }
        CapabilityObservation::Unavailable => {
            return Err(ApplicationLifecycleError::UnavailableTargetObservation {
                execution_target_id: target.execution_target_id.clone(),
            });
        }
    };
    if observed != &declared_platform {
        return Err(ApplicationLifecycleError::TargetPlatformMismatch {
            execution_target_id: target.execution_target_id.clone(),
            declared: render_platform(&declared_platform),
            observed: render_platform(observed),
        });
    }
    Ok(declared_platform.to_oci_platform())
}

/// Resolves the immutable snapshot of a deployment on `placement`: digest-pinned services keep
/// their digest reference, while local-development and Docker-delivered Nix services keep an empty
/// launch identity that the hosting node pins.
pub(super) fn resolve_snapshot(
    application: &dotcore_proto::dotcore::catalog::v1::Application,
    site: &Site,
    placement: &ExactPlacement,
    platform: &dotcore_proto::dotcore::catalog::v1::OciPlatform,
) -> Result<ResolvedApplicationDeploymentSnapshot, ApplicationLifecycleError> {
    let target = site
        .execution_targets
        .iter()
        .find(|target| target.execution_target_id == placement.execution_target_id)
        .filter(|target| target.site_node_id == placement.site_node_id)
        .ok_or(ApplicationLifecycleError::FailedPrecondition)?;
    let graph = application
        .graph
        .as_ref()
        .ok_or(ApplicationLifecycleError::FailedPrecondition)?;
    let mut service_selections = Vec::with_capacity(graph.services.len());
    for service in &graph.services {
        if service
            .artifact
            .as_ref()
            .is_none_or(|artifact| !artifact.supported_platforms.contains(platform))
        {
            return Err(ApplicationLifecycleError::UnsupportedServicePlatform {
                service_name: service.service_name.clone(),
                platform: format!("{}/{}", platform.operating_system, platform.architecture),
            });
        }
        let artifact = service
            .artifact
            .as_ref()
            .ok_or(ApplicationLifecycleError::FailedPrecondition)?;
        let reference = match artifact.kind.as_ref() {
            Some(ServiceArtifactKind::Oci(reference)) => reference,
            Some(ServiceArtifactKind::Nix(nix)) => {
                let preparation_policy = dotcore_domain::nix::NixFlakeReference::parse(&nix.flake)
                    .map_err(|_| ApplicationLifecycleError::FailedPrecondition)?
                    .preparation_policy();
                service_selections.push(ResolvedApplicationServiceSelection {
                    service_name: service.service_name.clone(),
                    preparation_policy: preparation_policy as i32,
                    launch_identity: String::new(),
                });
                continue;
            }
            None => return Err(ApplicationLifecycleError::FailedPrecondition),
        };
        let (preparation_policy, launch_identity) = if reference.digest.is_some() {
            let rendered = dotcore_domain::oci::render_reference(reference)
                .map_err(|_| ApplicationLifecycleError::FailedPrecondition)?;
            let launch_identity = dotcore_domain::oci::canonical_digest_reference(&rendered)
                .map_err(|_| ApplicationLifecycleError::FailedPrecondition)?
                .ok_or(ApplicationLifecycleError::FailedPrecondition)?;
            (ArtifactPreparationPolicy::ImmutableRemote, launch_identity)
        } else {
            (ArtifactPreparationPolicy::LocalDevelopment, String::new())
        };
        service_selections.push(ResolvedApplicationServiceSelection {
            service_name: service.service_name.clone(),
            preparation_policy: preparation_policy as i32,
            launch_identity,
        });
    }
    Ok(ResolvedApplicationDeploymentSnapshot {
        application_id: application.application_id.clone(),
        display_name: application.display_name.clone(),
        version: application.version.clone(),
        notes: application.notes.clone(),
        graph: application.graph.clone(),
        placement: Some(placement.clone()),
        accelerator_device_ids: target.accelerator_device_ids.clone(),
        platform: Some(platform.clone()),
        service_selections,
        parameter_values: Vec::new(),
    })
}

fn render_platform(platform: &dotcore_domain::ExecutionPlatform) -> String {
    format!(
        "{}/{}",
        platform.operating_system(),
        platform.architecture()
    )
}

fn persist_create<D, S>(
    owner: &Arc<ApplicationOwner<D, S>>,
    desired: &ApplicationDeploymentDeclaration,
    revision: u64,
) -> Result<(), ApplicationLifecycleError>
where
    D: ApplicationDeclarationRepository,
    S: CurrentApplicationRepository + ApplicationLifecycleGenerations,
{
    owner
        .desired
        .upsert(desired.clone(), revision)
        .map_err(declaration_error)?;
    // A status the node already reported for the new deployment is kept over the queued state.
    record_intent(
        owner,
        &desired.application_deployment_id,
        "Create",
        |existing| (super::queued_state(), existing.is_none()),
    )?;
    Ok(())
}
