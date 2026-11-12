//! Owner-facing repositories for application declarations and current deployment state.

use std::collections::BTreeSet;

use dotcore_catalog::{
    OciArtifactPolicy,
    typed_parameters::{canonical_value_text, is_secret},
    valid_configuration_value, valid_environment_variable, validate_application_with_policy,
};
use dotcore_domain::{
    deployment::{
        artifact::validate_retained_launch_identity,
        launch_pin::{PinSource, RetainedArtifact, pin_source},
    },
    valid_id,
};
use dotcore_proto::dotcore::{
    application::v1::{
        ApplicationConfigurationAttachment, ApplicationDeploymentDeclaration,
        ApplicationDeploymentDesiredState, ApplicationLifecycleState, ApplicationReadinessState,
        ApplicationSecretAttachment, ApplicationServiceLifecycleState,
        ApplicationStorageAttachment, CurrentApplicationDeployment,
        ResolvedApplicationDeploymentSnapshot, ResolvedApplicationServiceSelection,
    },
    catalog::v1::{ApplicationSecretForm, application_configuration_binding},
    site::store::v1::SiteCurrentState,
};
use dotcore_site::{
    repository::{
        ApplicationSiteSnapshot, ScopedSiteMutationError, SiteRepository, SiteRepositoryError,
    },
    store::{CurrentStateStore, StoreError, StoredCurrentState, reserve_lifecycle_generation},
};

use super::published_port::valid_published_port;

/// An isolated read view of apps-controller declarations.
#[derive(Clone, Debug, PartialEq)]
pub struct ApplicationDeclarationSnapshot {
    pub site_id: String,
    pub storage_revision: u64,
    pub deployments: Vec<ApplicationDeploymentDeclaration>,
}

/// An isolated read view of apps-controller current records.
#[derive(Clone, Debug, PartialEq)]
pub struct CurrentApplicationSnapshot {
    pub site_id: String,
    pub storage_revision: u64,
    pub deployments: Vec<CurrentApplicationDeployment>,
}

/// Revisions assigned by one successful repository mutation.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ApplicationRepositoryMutation {
    pub storage_revision: u64,
    pub record_revision: u64,
}

/// Apps-controller declaration persistence port.
///
/// Lifecycle transitions take a generation reserved through [`ApplicationLifecycleGenerations`]
/// and accept it only when it advances the declaration, so accepted intent never reuses a
/// generation.
pub(crate) trait ApplicationDeclarationRepository: Send + Sync {
    fn snapshot(&self) -> Result<ApplicationDeclarationSnapshot, ApplicationRepositoryError>;

    fn upsert(
        &self,
        deployment: ApplicationDeploymentDeclaration,
        expected_storage_revision: u64,
    ) -> Result<ApplicationRepositoryMutation, ApplicationRepositoryError>;

    /// Retains the deployment and its canonical name with `TEARDOWN` intent at the reserved
    /// lifecycle generation.
    fn remove(
        &self,
        deployment_id: &str,
        expected_storage_revision: u64,
        reserved_generation: u64,
    ) -> Result<ApplicationRepositoryMutation, ApplicationRepositoryError>;

    fn pause(
        &self,
        deployment_id: &str,
        expected_storage_revision: u64,
        reserved_generation: u64,
    ) -> Result<ApplicationRepositoryMutation, ApplicationRepositoryError>;

    fn resume(
        &self,
        deployment_id: &str,
        expected_storage_revision: u64,
        reserved_generation: u64,
    ) -> Result<ApplicationRepositoryMutation, ApplicationRepositoryError>;

    fn update_metadata(
        &self,
        _deployment_id: &str,
        _patch: &dotcore_domain::deployment::metadata::DeploymentMetadataPatch,
        _expected_storage_revision: u64,
    ) -> Result<ApplicationRepositoryMutation, ApplicationRepositoryError> {
        Err(ApplicationRepositoryError::Unavailable)
    }

    fn declaration_snapshot(&self) -> Result<ApplicationSiteSnapshot, ApplicationRepositoryError>;

    /// Runs `read` with every declared deployment ID while holding the declaration guard, so no
    /// declaration mutation or node status acceptance can interleave with it. `read` may take the
    /// current-state lock but never the declaration guard.
    fn with_declared_deployment_ids<T>(
        &self,
        read: impl FnOnce(&BTreeSet<String>) -> T,
    ) -> Result<T, ApplicationRepositoryError>;
}

/// Apps-controller lifecycle generation allocator backed by durable manager records.
///
/// A reserved generation is burned even when the site write accepting it later fails, so no
/// generation is ever issued twice for one deployment.
pub(crate) trait ApplicationLifecycleGenerations: Send + Sync {
    fn reserve_lifecycle_generation(
        &self,
        deployment_id: &str,
    ) -> Result<u64, ApplicationRepositoryError>;
}

/// Apps-controller current-state persistence port.
pub trait CurrentApplicationRepository: Send + Sync {
    fn snapshot(&self) -> Result<CurrentApplicationSnapshot, ApplicationRepositoryError>;

    fn upsert(
        &self,
        deployment: CurrentApplicationDeployment,
        expected_record_revision: Option<u64>,
    ) -> Result<ApplicationRepositoryMutation, ApplicationRepositoryError>;

    fn remove(
        &self,
        deployment_id: &str,
        expected_record_revision: u64,
    ) -> Result<ApplicationRepositoryMutation, ApplicationRepositoryError>;
}

/// Every site store and node store serves the application records it holds; each mutation
/// validates them before the store commits it.
impl<S: CurrentStateStore> CurrentApplicationRepository for S {
    fn snapshot(&self) -> Result<CurrentApplicationSnapshot, ApplicationRepositoryError> {
        self.read(current_snapshot)?
    }

    fn upsert(
        &self,
        deployment: CurrentApplicationDeployment,
        expected_record_revision: Option<u64>,
    ) -> Result<ApplicationRepositoryMutation, ApplicationRepositoryError> {
        self.transact(|state| {
            let mutation = upsert_current(state, deployment, expected_record_revision)?;
            validate_current_records(state)?;
            Ok(mutation)
        })
    }

    fn remove(
        &self,
        deployment_id: &str,
        expected_record_revision: u64,
    ) -> Result<ApplicationRepositoryMutation, ApplicationRepositoryError> {
        self.transact(|state| {
            let mutation = remove_current(state, deployment_id, expected_record_revision)?;
            validate_current_records(state)?;
            Ok(mutation)
        })
    }
}

/// The site store's durable manager lifecycle records allocate application generations.
impl<S: CurrentStateStore<State = SiteCurrentState>> ApplicationLifecycleGenerations for S {
    fn reserve_lifecycle_generation(
        &self,
        deployment_id: &str,
    ) -> Result<u64, ApplicationRepositoryError> {
        self.transact(|state| {
            reserve_lifecycle_generation(
                state,
                dotcore_domain::deployment::DeploymentOwner::Application,
                deployment_id,
            )
            .map(|record| record.generation_high_water)
            .map_err(ApplicationRepositoryError::from)
        })
    }
}

impl ApplicationDeclarationRepository for SiteRepository {
    fn snapshot(&self) -> Result<ApplicationDeclarationSnapshot, ApplicationRepositoryError> {
        let snapshot = self.application_snapshot().map_err(site_repository_error)?;
        validate_declarations(&snapshot.deployments)?;
        Ok(ApplicationDeclarationSnapshot {
            site_id: snapshot.site_id,
            storage_revision: 0,
            deployments: snapshot.deployments,
        })
    }

    fn upsert(
        &self,
        deployment: ApplicationDeploymentDeclaration,
        expected_storage_revision: u64,
    ) -> Result<ApplicationRepositoryMutation, ApplicationRepositoryError> {
        if expected_storage_revision != 0 {
            return Err(ApplicationRepositoryError::RevisionConflict);
        }
        self.mutate_application_deployments(|deployments| {
            upsert_declaration(deployments, deployment)
        })
        .map_err(scoped_site_mutation_error)?;
        Ok(ApplicationRepositoryMutation {
            storage_revision: 0,
            record_revision: 0,
        })
    }

    fn remove(
        &self,
        deployment_id: &str,
        expected_storage_revision: u64,
        reserved_generation: u64,
    ) -> Result<ApplicationRepositoryMutation, ApplicationRepositoryError> {
        if expected_storage_revision != 0 {
            return Err(ApplicationRepositoryError::RevisionConflict);
        }
        self.mutate_application_deployments(|deployments| {
            mark_declaration_for_teardown(deployments, deployment_id, reserved_generation)
        })
        .map_err(scoped_site_mutation_error)?;
        Ok(ApplicationRepositoryMutation {
            storage_revision: 0,
            record_revision: 0,
        })
    }

    fn pause(
        &self,
        deployment_id: &str,
        expected_storage_revision: u64,
        reserved_generation: u64,
    ) -> Result<ApplicationRepositoryMutation, ApplicationRepositoryError> {
        if expected_storage_revision != 0 {
            return Err(ApplicationRepositoryError::RevisionConflict);
        }
        self.mutate_application_deployments(|deployments| {
            transition_desired_state(
                deployments,
                deployment_id,
                ApplicationDeploymentDesiredState::Paused,
                "pause",
                reserved_generation,
            )
            .map(|_| ())
        })
        .map_err(scoped_site_mutation_error)?;
        Ok(ApplicationRepositoryMutation {
            storage_revision: 0,
            record_revision: 0,
        })
    }

    fn resume(
        &self,
        deployment_id: &str,
        expected_storage_revision: u64,
        reserved_generation: u64,
    ) -> Result<ApplicationRepositoryMutation, ApplicationRepositoryError> {
        if expected_storage_revision != 0 {
            return Err(ApplicationRepositoryError::RevisionConflict);
        }
        self.mutate_application_deployments(|deployments| {
            transition_desired_state(
                deployments,
                deployment_id,
                ApplicationDeploymentDesiredState::Present,
                "resume",
                reserved_generation,
            )
            .map(|_| ())
        })
        .map_err(scoped_site_mutation_error)?;
        Ok(ApplicationRepositoryMutation {
            storage_revision: 0,
            record_revision: 0,
        })
    }

    fn update_metadata(
        &self,
        deployment_id: &str,
        patch: &dotcore_domain::deployment::metadata::DeploymentMetadataPatch,
        expected_storage_revision: u64,
    ) -> Result<ApplicationRepositoryMutation, ApplicationRepositoryError> {
        if expected_storage_revision != 0 {
            return Err(ApplicationRepositoryError::RevisionConflict);
        }
        self.mutate_application_deployment_metadata(|deployments| {
            let deployment = deployments
                .iter_mut()
                .find(|deployment| deployment.application_deployment_id == deployment_id)
                .ok_or(ApplicationRepositoryError::NotFound)?;
            if deployment.desired_state == ApplicationDeploymentDesiredState::Teardown as i32 {
                return Err(ApplicationRepositoryError::InvalidTransition);
            }
            deployment.application_deployment_name = patch.display_name().to_owned();
            Ok(())
        })
        .map_err(scoped_site_mutation_error)?;
        Ok(ApplicationRepositoryMutation {
            storage_revision: 0,
            record_revision: 0,
        })
    }

    fn declaration_snapshot(&self) -> Result<ApplicationSiteSnapshot, ApplicationRepositoryError> {
        self.application_snapshot().map_err(site_repository_error)
    }

    fn with_declared_deployment_ids<T>(
        &self,
        read: impl FnOnce(&BTreeSet<String>) -> T,
    ) -> Result<T, ApplicationRepositoryError> {
        self.with_application_deployments(|deployments| {
            read(
                &deployments
                    .iter()
                    .map(|deployment| deployment.application_deployment_id.clone())
                    .collect(),
            )
        })
        .map_err(site_repository_error)
    }
}

fn site_repository_error(error: SiteRepositoryError) -> ApplicationRepositoryError {
    ApplicationRepositoryError::Storage {
        operation: "site.yaml mutation",
        path: "site.yaml".to_owned(),
        source: anyhow::Error::new(error),
    }
}

fn scoped_site_mutation_error(
    error: ScopedSiteMutationError<ApplicationRepositoryError>,
) -> ApplicationRepositoryError {
    match error {
        ScopedSiteMutationError::Owner(error) => error,
        ScopedSiteMutationError::Repository(error) => site_repository_error(error),
    }
}

pub(crate) fn has_inference_route_projection(
    application: &dotcore_proto::dotcore::catalog::v1::Application,
) -> bool {
    application.graph.as_ref().is_some_and(|graph| {
        graph
            .services
            .iter()
            .flat_map(|service| &service.configuration_bindings)
            .any(is_inference_route_projection)
    })
}

pub(crate) fn current_snapshot<S: StoredCurrentState>(
    state: &S,
) -> Result<CurrentApplicationSnapshot, ApplicationRepositoryError> {
    validate_current_records(state)?;
    Ok(CurrentApplicationSnapshot {
        site_id: state.site_id().to_owned(),
        storage_revision: state.storage_revision(),
        deployments: state.application_deployments().to_vec(),
    })
}

pub(crate) fn upsert_declaration(
    deployments: &mut Vec<ApplicationDeploymentDeclaration>,
    deployment: ApplicationDeploymentDeclaration,
) -> Result<(), ApplicationRepositoryError> {
    validate_declaration(&deployment)?;
    let position = deployments.iter().position(|current| {
        current.application_deployment_id == deployment.application_deployment_id
    });
    if position.is_some() {
        return Err(ApplicationRepositoryError::AlreadyExists);
    }
    deployments.push(deployment);
    deployments.sort_by(|left, right| {
        left.application_deployment_id
            .cmp(&right.application_deployment_id)
    });
    Ok(())
}

pub(crate) fn mark_declaration_for_teardown(
    deployments: &mut [ApplicationDeploymentDeclaration],
    deployment_id: &str,
    reserved_generation: u64,
) -> Result<(), ApplicationRepositoryError> {
    let deployment = deployments
        .iter_mut()
        .find(|current| current.application_deployment_id == deployment_id)
        .ok_or(ApplicationRepositoryError::NotFound)?;
    if ApplicationDeploymentDesiredState::try_from(deployment.desired_state)
        == Ok(ApplicationDeploymentDesiredState::Teardown)
    {
        return Err(ApplicationRepositoryError::AlreadyExists);
    }
    advance_lifecycle_generation(deployment, reserved_generation)?;
    deployment.desired_state = ApplicationDeploymentDesiredState::Teardown as i32;
    Ok(())
}

/// Applies a pause or resume at the reserved lifecycle generation; an already reached target is
/// left unchanged.
fn transition_desired_state(
    deployments: &mut [ApplicationDeploymentDeclaration],
    deployment_id: &str,
    target: ApplicationDeploymentDesiredState,
    operation: &'static str,
    reserved_generation: u64,
) -> Result<ApplicationDeploymentDeclaration, ApplicationRepositoryError> {
    let deployment = deployments
        .iter_mut()
        .find(|deployment| deployment.application_deployment_id == deployment_id)
        .ok_or(ApplicationRepositoryError::NotFound)?;
    let current = ApplicationDeploymentDesiredState::try_from(deployment.desired_state)
        .map_err(|_| ApplicationRepositoryError::InvalidState)?;
    match (operation, current, target) {
        (
            "pause",
            ApplicationDeploymentDesiredState::Present,
            ApplicationDeploymentDesiredState::Paused,
        )
        | (
            "resume",
            ApplicationDeploymentDesiredState::Paused,
            ApplicationDeploymentDesiredState::Present,
        ) => {
            advance_lifecycle_generation(deployment, reserved_generation)?;
            deployment.desired_state = target as i32;
        }
        (_, current, target) if current == target => {}
        _ => {
            return Err(ApplicationRepositoryError::InvalidTransition);
        }
    }
    Ok(deployment.clone())
}

fn advance_lifecycle_generation(
    deployment: &mut ApplicationDeploymentDeclaration,
    reserved_generation: u64,
) -> Result<(), ApplicationRepositoryError> {
    if reserved_generation <= deployment.lifecycle_generation {
        return Err(ApplicationRepositoryError::InvalidState);
    }
    deployment.lifecycle_generation = reserved_generation;
    Ok(())
}

pub(crate) fn upsert_current<S: StoredCurrentState>(
    state: &mut S,
    mut deployment: CurrentApplicationDeployment,
    expected_record_revision: Option<u64>,
) -> Result<ApplicationRepositoryMutation, ApplicationRepositoryError> {
    validate_current_deployment(&deployment, false)?;
    let records = state.application_deployments_mut();
    let position = records.iter().position(|current| {
        current.application_deployment_id == deployment.application_deployment_id
    });
    let record_revision = match (position, expected_record_revision) {
        (None, None) => 1,
        (Some(index), Some(expected)) if records[index].record_revision == expected => {
            next_revision(expected)?
        }
        (Some(_), None) => return Err(ApplicationRepositoryError::AlreadyExists),
        (None, Some(_)) => return Err(ApplicationRepositoryError::NotFound),
        (Some(_), Some(_)) => return Err(ApplicationRepositoryError::RevisionConflict),
    };
    deployment.record_revision = record_revision;
    match position {
        Some(index) => records[index] = deployment,
        None => records.push(deployment),
    }
    sort_current(records);
    let storage_revision = advance_storage_revision(state)?;
    Ok(ApplicationRepositoryMutation {
        storage_revision,
        record_revision,
    })
}

pub(crate) fn remove_current<S: StoredCurrentState>(
    state: &mut S,
    deployment_id: &str,
    expected_record_revision: u64,
) -> Result<ApplicationRepositoryMutation, ApplicationRepositoryError> {
    let records = state.application_deployments_mut();
    let index = records
        .iter()
        .position(|current| current.application_deployment_id == deployment_id)
        .ok_or(ApplicationRepositoryError::NotFound)?;
    if records[index].record_revision != expected_record_revision {
        return Err(ApplicationRepositoryError::RevisionConflict);
    }
    records.remove(index);
    let storage_revision = advance_storage_revision(state)?;
    Ok(ApplicationRepositoryMutation {
        storage_revision,
        record_revision: expected_record_revision,
    })
}

fn validate_declarations(
    deployments: &[ApplicationDeploymentDeclaration],
) -> Result<(), ApplicationRepositoryError> {
    let mut ids = BTreeSet::new();
    let mut ports = BTreeSet::new();
    for deployment in deployments {
        validate_declaration(deployment)?;
        if !ids.insert(&deployment.application_deployment_id) {
            return Err(ApplicationRepositoryError::Corrupt);
        }
        if !ports.insert(deployment.published_port) {
            return Err(ApplicationRepositoryError::InvalidPublishedPort {
                application_deployment_id: deployment.application_deployment_id.clone(),
            });
        }
    }
    Ok(())
}

pub(crate) fn validate_current_records<S: StoredCurrentState>(
    state: &S,
) -> Result<(), ApplicationRepositoryError> {
    let mut ids = BTreeSet::new();
    for deployment in state.application_deployments() {
        validate_current_deployment(deployment, true)?;
        if !ids.insert(&deployment.application_deployment_id) {
            return Err(ApplicationRepositoryError::Corrupt);
        }
    }
    Ok(())
}

/// Validates one complete application declaration: identity, display name, placement, desired
/// state, resolved snapshot, published host port, storage, route link, and configuration
/// attachments. A
/// local-development selection may leave its launch identity empty for the hosting node to
/// resolve once, and the declaration may project that node's pins back.
pub(crate) fn validate_declaration(
    deployment: &ApplicationDeploymentDeclaration,
) -> Result<(), ApplicationRepositoryError> {
    let desired_state = ApplicationDeploymentDesiredState::try_from(deployment.desired_state)
        .map_err(|_| ApplicationRepositoryError::InvalidState)?;
    let snapshot = deployment
        .snapshot
        .as_ref()
        .ok_or(ApplicationRepositoryError::InvalidState)?;
    if !valid_id(&deployment.application_deployment_id)
        || deployment.lifecycle_generation == 0
        || deployment.application_deployment_id.len() > 40
        || deployment.application_deployment_name.trim().is_empty()
        || deployment.application_deployment_name.chars().count() > 128
        || deployment
            .application_deployment_name
            .chars()
            .any(char::is_control)
        || deployment.placement.as_ref().is_none_or(|placement| {
            !valid_id(&placement.site_node_id) || !valid_id(&placement.execution_target_id)
        })
        || desired_state == ApplicationDeploymentDesiredState::Unspecified
        || validate_resolved_snapshot(snapshot).is_err()
    {
        return Err(ApplicationRepositoryError::InvalidState);
    }
    if !valid_published_port(deployment.published_port) {
        return Err(ApplicationRepositoryError::InvalidPublishedPort {
            application_deployment_id: deployment.application_deployment_id.clone(),
        });
    }
    validate_node_resolved_launch_identities(deployment)?;
    validate_storage_attachments(deployment)?;
    validate_inference_route_binding(deployment)?;
    validate_configuration_attachments(deployment)?;
    validate_secret_attachments(deployment)?;
    Ok(())
}

fn validate_resolved_snapshot(
    snapshot: &ResolvedApplicationDeploymentSnapshot,
) -> Result<(), ApplicationRepositoryError> {
    let application = dotcore_proto::dotcore::catalog::v1::Application {
        application_id: snapshot.application_id.clone(),
        display_name: snapshot.display_name.clone(),
        description: String::new(),
        notes: snapshot.notes.clone(),
        version: snapshot.version.clone(),
        graph: snapshot.graph.clone(),
    };
    if !validate_application_with_policy(&application, OciArtifactPolicy::Development).is_empty()
        || snapshot.placement.as_ref().is_none_or(|placement| {
            !valid_id(&placement.site_node_id) || !valid_id(&placement.execution_target_id)
        })
        || snapshot.platform.as_ref().is_none_or(|platform| {
            platform.operating_system != "linux"
                || !matches!(platform.architecture.as_str(), "amd64" | "arm64")
                || platform.variant.is_some()
        })
    {
        return Err(ApplicationRepositoryError::InvalidState);
    }
    let graph = snapshot
        .graph
        .as_ref()
        .ok_or(ApplicationRepositoryError::InvalidState)?;
    let platform = snapshot
        .platform
        .as_ref()
        .ok_or(ApplicationRepositoryError::InvalidState)?;
    let services = graph
        .services
        .iter()
        .map(|service| (service.service_name.as_str(), service))
        .collect::<std::collections::BTreeMap<_, _>>();
    let mut selected = BTreeSet::new();
    for selection in &snapshot.service_selections {
        let service = services
            .get(selection.service_name.as_str())
            .ok_or(ApplicationRepositoryError::InvalidState)?;
        let artifact = service
            .artifact
            .as_ref()
            .ok_or(ApplicationRepositoryError::InvalidState)?;
        let retained = RetainedArtifact::of_service(artifact)
            .ok_or(ApplicationRepositoryError::InvalidState)?;
        if !selected.insert(selection.service_name.as_str())
            || selection.launch_identity.chars().any(char::is_whitespace)
            || !artifact.supported_platforms.contains(platform)
        {
            return Err(ApplicationRepositoryError::InvalidState);
        }
        if selection.launch_identity.is_empty() {
            if selection_pin_source(snapshot, selection).is_none() {
                return Err(ApplicationRepositoryError::InvalidState);
            }
            continue;
        }
        validate_retained_launch_identity(
            retained,
            selection.preparation_policy,
            &selection.launch_identity,
        )
        .map_err(|_| ApplicationRepositoryError::InvalidState)?;
    }
    if selected.len() != services.len() {
        return Err(ApplicationRepositoryError::InvalidState);
    }
    Ok(())
}

/// Returns what the hosting node resolves for a selection that leaves its launch identity to it:
/// the tag of a local-development image, or the store path of a Docker-delivered Nix artifact.
pub(super) fn selection_pin_source(
    snapshot: &ResolvedApplicationDeploymentSnapshot,
    selection: &ResolvedApplicationServiceSelection,
) -> Option<PinSource> {
    let artifact = snapshot
        .graph
        .as_ref()?
        .services
        .iter()
        .find(|service| service.service_name == selection.service_name)?
        .artifact
        .as_ref()?;
    pin_source(
        RetainedArtifact::of_service(artifact)?,
        selection.preparation_policy,
        &selection.launch_identity,
    )
}

/// Projected node pins must each name a distinct node-resolved selection with an image ID.
fn validate_node_resolved_launch_identities(
    deployment: &ApplicationDeploymentDeclaration,
) -> Result<(), ApplicationRepositoryError> {
    let pins = &deployment.node_resolved_launch_identities;
    let snapshot = deployment
        .snapshot
        .as_ref()
        .expect("Application declaration snapshot was validated");
    let expected_kind = |key: &str| {
        snapshot
            .service_selections
            .iter()
            .find(|selection| selection.service_name == key)
            .and_then(|selection| selection_pin_source(snapshot, selection))
            .map(|source| source.kind())
    };
    if !dotcore_domain::deployment::launch_pin::valid_node_pins(pins, expected_kind) {
        return Err(ApplicationRepositoryError::InvalidState);
    }
    Ok(())
}

fn validate_storage_attachments(
    deployment: &ApplicationDeploymentDeclaration,
) -> Result<(), ApplicationRepositoryError> {
    let snapshot = deployment
        .snapshot
        .as_ref()
        .expect("Application declaration snapshot was validated");
    let graph = snapshot
        .graph
        .as_ref()
        .expect("Application snapshot was validated");
    let expected = graph
        .services
        .iter()
        .flat_map(|service| {
            service
                .storage_bindings
                .iter()
                .map(move |binding| (service.service_name.as_str(), binding.binding_name.as_str()))
        })
        .collect::<BTreeSet<_>>();
    let mut actual = BTreeSet::new();
    let mut storage_names = BTreeSet::new();
    for ApplicationStorageAttachment {
        service_name,
        binding_name,
        storage_name,
    } in &deployment.storage_attachments
    {
        if !valid_id(service_name)
            || !valid_id(binding_name)
            || !valid_id(storage_name)
            || !actual.insert((service_name.as_str(), binding_name.as_str()))
            || !storage_names.insert(storage_name.as_str())
        {
            return Err(ApplicationRepositoryError::InvalidState);
        }
    }
    if actual != expected {
        return Err(ApplicationRepositoryError::InvalidState);
    }
    Ok(())
}

fn validate_configuration_attachments(
    deployment: &ApplicationDeploymentDeclaration,
) -> Result<(), ApplicationRepositoryError> {
    let snapshot = deployment
        .snapshot
        .as_ref()
        .expect("Application declaration snapshot was validated");
    let graph = snapshot
        .graph
        .as_ref()
        .expect("Application snapshot was validated");
    let expected = graph
        .services
        .iter()
        .flat_map(|service| {
            service
                .configuration_bindings
                .iter()
                .filter_map(move |binding| {
                    (deployment.inference_route_id.is_none()
                        || !is_inference_route_projection(binding))
                    .then_some((
                        (service.service_name.as_str(), binding.binding_name.as_str()),
                        binding.environment_variable.as_str(),
                    ))
                })
        })
        .collect::<BTreeSet<_>>();
    let mut actual = BTreeSet::new();
    let mut environment_destinations = BTreeSet::new();
    for ApplicationConfigurationAttachment {
        service_name,
        binding_name,
        environment_variable,
        value,
    } in &deployment.configuration_attachments
    {
        if !valid_id(service_name)
            || !valid_id(binding_name)
            || !valid_environment_variable(environment_variable)
            || !valid_configuration_value(value)
            || !actual.insert((
                (service_name.as_str(), binding_name.as_str()),
                environment_variable.as_str(),
            ))
            || !environment_destinations
                .insert((service_name.as_str(), environment_variable.as_str()))
        {
            return Err(ApplicationRepositoryError::InvalidState);
        }
    }
    if actual != expected {
        return Err(ApplicationRepositoryError::InvalidState);
    }
    // Retained parameter values are authoritative; their attachments must carry the same text.
    for service in &graph.services {
        for binding in &service.configuration_bindings {
            let Some(application_configuration_binding::Source::ParameterId(id)) =
                binding.source.as_ref()
            else {
                continue;
            };
            let attachment = deployment
                .configuration_attachments
                .iter()
                .find(|attachment| {
                    attachment.service_name == service.service_name
                        && attachment.binding_name == binding.binding_name
                })
                .map(|attachment| attachment.value.as_str());
            if attachment != canonical_value_text(&snapshot.parameter_values, id).as_deref() {
                return Err(ApplicationRepositoryError::InvalidState);
            }
        }
    }
    Ok(())
}

/// Checks that every secret binding has exactly one attachment carrying its parameter's retained
/// value in the binding's form, and that the generated parameters are distinct retained secrets.
fn validate_secret_attachments(
    deployment: &ApplicationDeploymentDeclaration,
) -> Result<(), ApplicationRepositoryError> {
    let snapshot = deployment
        .snapshot
        .as_ref()
        .expect("Application declaration snapshot was validated");
    let graph = snapshot
        .graph
        .as_ref()
        .expect("Application snapshot was validated");
    let mut expected = BTreeSet::new();
    for service in &graph.services {
        for binding in &service.secret_bindings {
            let value = canonical_value_text(&snapshot.parameter_values, &binding.parameter_id)
                .ok_or(ApplicationRepositoryError::InvalidState)?;
            let value = match ApplicationSecretForm::try_from(binding.form) {
                Ok(ApplicationSecretForm::Plain) => value,
                Ok(ApplicationSecretForm::Unspecified) | Err(_) => {
                    return Err(ApplicationRepositoryError::InvalidState);
                }
            };
            expected.insert((
                service.service_name.as_str(),
                binding.binding_name.as_str(),
                binding.environment_variable.as_str(),
                value,
            ));
        }
    }
    let mut actual = BTreeSet::new();
    for ApplicationSecretAttachment {
        service_name,
        binding_name,
        environment_variable,
        value,
    } in &deployment.secret_attachments
    {
        if !valid_configuration_value(value)
            || !actual.insert((
                service_name.as_str(),
                binding_name.as_str(),
                environment_variable.as_str(),
                value.clone(),
            ))
        {
            return Err(ApplicationRepositoryError::InvalidState);
        }
    }
    if actual != expected {
        return Err(ApplicationRepositoryError::InvalidState);
    }

    let mut generated = BTreeSet::new();
    for id in &snapshot.generated_parameter_ids {
        let retained_secret = graph
            .parameters
            .iter()
            .find(|definition| definition.parameter_id == *id)
            .is_some_and(is_secret)
            && snapshot
                .parameter_values
                .iter()
                .any(|value| value.parameter_id == *id);
        if !retained_secret || !generated.insert(id.as_str()) {
            return Err(ApplicationRepositoryError::InvalidState);
        }
    }
    Ok(())
}

fn validate_inference_route_binding(
    deployment: &ApplicationDeploymentDeclaration,
) -> Result<(), ApplicationRepositoryError> {
    let snapshot = deployment
        .snapshot
        .as_ref()
        .expect("Application declaration snapshot was validated");
    let graph = snapshot
        .graph
        .as_ref()
        .expect("Application snapshot was validated");
    let route_inputs = graph
        .services
        .iter()
        .flat_map(|service| &service.configuration_bindings)
        .filter(|binding| is_inference_route_input(binding))
        .collect::<Vec<_>>();
    if route_inputs
        .iter()
        .any(|binding| !is_inference_route_projection(binding))
    {
        return Err(ApplicationRepositoryError::InvalidState);
    }
    match deployment.inference_route_id.as_deref() {
        Some(inference_route_id) if valid_id(inference_route_id) && !route_inputs.is_empty() => {
            Ok(())
        }
        None => Ok(()),
        _ => Err(ApplicationRepositoryError::InvalidState),
    }
}

fn is_inference_route_input(
    binding: &dotcore_proto::dotcore::catalog::v1::ApplicationConfigurationBinding,
) -> bool {
    matches!(
        binding.input.as_ref().and_then(|input| input.kind.as_ref()),
        Some(
            dotcore_proto::dotcore::catalog::v1::application_deployment_input::Kind::InferenceRoute(
                _
            )
        )
    )
}

fn is_inference_route_projection(
    binding: &dotcore_proto::dotcore::catalog::v1::ApplicationConfigurationBinding,
) -> bool {
    matches!(
        binding
            .input
            .as_ref()
            .and_then(|input| input.kind.as_ref()),
        Some(dotcore_proto::dotcore::catalog::v1::application_deployment_input::Kind::InferenceRoute(input))
            if matches!(
                dotcore_proto::dotcore::catalog::v1::ApplicationInferenceRouteProjection::try_from(input.projection),
                Ok(
                    dotcore_proto::dotcore::catalog::v1::ApplicationInferenceRouteProjection::RouteName
                )
            )
    )
}

fn validate_current_deployment(
    deployment: &CurrentApplicationDeployment,
    persisted: bool,
) -> Result<(), ApplicationRepositoryError> {
    let state = deployment
        .state
        .as_ref()
        .ok_or(ApplicationRepositoryError::InvalidState)?;
    let lifecycle = ApplicationLifecycleState::try_from(state.lifecycle_state)
        .map_err(|_| ApplicationRepositoryError::InvalidState)?;
    let readiness = ApplicationReadinessState::try_from(state.readiness_state)
        .map_err(|_| ApplicationRepositoryError::InvalidState)?;
    if !valid_id(&deployment.application_deployment_id)
        || (persisted && deployment.record_revision == 0)
        || lifecycle == ApplicationLifecycleState::Unspecified
        || readiness == ApplicationReadinessState::Unspecified
        || state.stage.trim().is_empty()
        || state
            .endpoint
            .as_ref()
            .is_some_and(|endpoint| endpoint.url.trim().is_empty())
    {
        return Err(ApplicationRepositoryError::InvalidState);
    }
    let mut service_names = BTreeSet::new();
    for service in &state.services {
        let lifecycle = ApplicationServiceLifecycleState::try_from(service.lifecycle_state)
            .map_err(|_| ApplicationRepositoryError::InvalidState)?;
        let readiness = ApplicationReadinessState::try_from(service.readiness_state)
            .map_err(|_| ApplicationRepositoryError::InvalidState)?;
        if !valid_id(&service.service_name)
            || !service_names.insert(&service.service_name)
            || lifecycle == ApplicationServiceLifecycleState::Unspecified
            || readiness == ApplicationReadinessState::Unspecified
        {
            return Err(ApplicationRepositoryError::InvalidState);
        }
    }
    Ok(())
}

fn sort_current(records: &mut [CurrentApplicationDeployment]) {
    records.sort_by(|left, right| {
        left.application_deployment_id
            .cmp(&right.application_deployment_id)
    });
}

fn advance_storage_revision<S: StoredCurrentState>(
    state: &mut S,
) -> Result<u64, ApplicationRepositoryError> {
    let revision = state.storage_revision_mut();
    *revision = next_revision(*revision)?;
    Ok(*revision)
}

fn next_revision(value: u64) -> Result<u64, ApplicationRepositoryError> {
    value
        .checked_add(1)
        .ok_or(ApplicationRepositoryError::RevisionExhausted)
}

#[derive(Debug, thiserror::Error)]
pub enum ApplicationRepositoryError {
    #[error("Application repository state is invalid")]
    InvalidState,

    #[error("Application deployment already exists")]
    AlreadyExists,

    #[error(
        "Application deployment {application_deployment_id} has a missing, out-of-range or shared published host port; delete the deployment or reset the site"
    )]
    InvalidPublishedPort { application_deployment_id: String },

    #[error("Application deployment name is invalid: {0}")]
    InvalidName(#[source] dotcore_domain::deployment::name::DeploymentNameError),

    #[error("Application deployment was not found")]
    NotFound,

    #[error("Application repository revision does not match")]
    RevisionConflict,

    #[error("Application repository revision is exhausted")]
    RevisionExhausted,

    #[error("Application repository state is corrupt")]
    Corrupt,

    #[error("Application repository state exceeds the storage limit")]
    LimitExceeded,

    #[error("Application repository {operation} failed for {path}: {source}")]
    Storage {
        operation: &'static str,
        path: String,
        #[source]
        source: anyhow::Error,
    },

    #[error("Application repository {operation} has an uncertain outcome for {path}: {source}")]
    OutcomeUncertain {
        operation: &'static str,
        path: String,
        #[source]
        source: anyhow::Error,
    },

    #[error("Application repository durability is uncertain")]
    Poisoned,

    #[error("Application repository is unavailable")]
    Unavailable,

    #[error("Application deployment desired-state transition is invalid")]
    InvalidTransition,

    #[error("Application deployment declaration no longer matches the finalized teardown")]
    TeardownMismatch,
}

impl From<StoreError> for ApplicationRepositoryError {
    fn from(error: StoreError) -> Self {
        match error {
            StoreError::RevisionExhausted => Self::RevisionExhausted,
            StoreError::LimitExceeded => Self::LimitExceeded,
            StoreError::Poisoned => Self::Poisoned,
            StoreError::Unavailable => Self::Unavailable,
            StoreError::Storage(source) => Self::Storage {
                operation: "persistence",
                path: "state.pb".to_owned(),
                source,
            },
            StoreError::OutcomeUncertain(source) => Self::OutcomeUncertain {
                operation: "persistence",
                path: "state.pb".to_owned(),
                source,
            },
            error => Self::Storage {
                operation: "shared state",
                path: "state.pb".to_owned(),
                source: anyhow::Error::new(error),
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use dotcore_domain::test_support::oci::{ServiceArtifactOci as _, digest_artifact};
    use dotcore_proto::dotcore::{
        application::v1::{
            ApplicationConfigurationAttachment, ApplicationDeploymentDeclaration,
            ApplicationDeploymentDesiredState, ApplicationSecretAttachment,
            ResolvedApplicationDeploymentSnapshot, ResolvedApplicationServiceSelection,
        },
        catalog::v1::{
            Application, ApplicationConfigurationBinding, ApplicationEndpointProtocol,
            ApplicationGraph, ApplicationPrimaryEndpoint, ApplicationSecretBinding,
            ApplicationSecretForm, ApplicationService, ApplicationServicePort, ParameterDefinition,
            ParameterValue, ParameterVisibility, SecretParameterSchema, StringEnumParameterSchema,
            TextParameterSchema, application_configuration_binding,
            parameter_definition::ParameterType, parameter_value,
        },
        deployment::v1::{ArtifactPreparationPolicy, NodeResolvedLaunchIdentity},
    };
    use dotcore_site::store::InMemoryCurrentState;

    use super::{
        ApplicationRepositoryError, CurrentApplicationRepository, mark_declaration_for_teardown,
        transition_desired_state, upsert_declaration, validate_declaration, validate_declarations,
    };

    #[test]
    fn oversized_application_record_is_limit_exceeded_and_leaves_the_state_unchanged() {
        let state = InMemoryCurrentState::new("site-one").expect("memory state");
        state
            .upsert(starting("application-one"), None)
            .expect("application write");
        let before = state.snapshot().expect("application snapshot");
        let mut oversized = starting("application-two");
        oversized.state.as_mut().expect("state").stage = "x".repeat(4 * 1024 * 1024);

        assert!(matches!(
            state.upsert(oversized, None),
            Err(ApplicationRepositoryError::LimitExceeded)
        ));
        assert_eq!(state.snapshot().expect("unchanged snapshot"), before);
    }

    #[test]
    fn declarations_need_a_published_port_in_range_that_no_other_deployment_holds() {
        let with_port = |id: &str, port: u32| {
            let mut deployment = valid_deployment();
            deployment.application_deployment_id = id.to_owned();
            deployment.published_port = port;
            deployment
        };
        let invalid_port = |id: &str| ApplicationRepositoryError::InvalidPublishedPort {
            application_deployment_id: id.to_owned(),
        };

        assert!(
            validate_declarations(&[with_port("first", 30_000), with_port("second", 32_767)])
                .is_ok()
        );
        for port in [0, 29_999, 32_768] {
            let error = validate_declaration(&with_port("first", port)).expect_err("port");
            assert_eq!(error.to_string(), invalid_port("first").to_string());
        }
        let error =
            validate_declarations(&[with_port("first", 31_000), with_port("second", 31_000)])
                .expect_err("shared port");
        assert_eq!(
            error.to_string(),
            "Application deployment second has a missing, out-of-range or shared published host port; delete the deployment or reset the site"
        );
    }

    #[test]
    fn declaration_validation_rejects_snapshot_notes_that_break_the_limit() {
        let with_notes = |notes: String| {
            let mut deployment = valid_deployment();
            deployment.snapshot.as_mut().expect("snapshot").notes = notes;
            deployment
        };

        assert!(validate_declaration(&with_notes("line one\nline two".to_owned())).is_ok());
        assert!(matches!(
            validate_declaration(&with_notes("x".repeat(1001))),
            Err(ApplicationRepositoryError::InvalidState)
        ));
        assert!(matches!(
            validate_declaration(&with_notes("tab\tcharacter".to_owned())),
            Err(ApplicationRepositoryError::InvalidState)
        ));
    }

    #[test]
    fn pause_and_resume_repository_transitions_are_legal_and_idempotent() {
        let mut deployments = vec![deployment(ApplicationDeploymentDesiredState::Present)];
        let paused = transition_desired_state(
            &mut deployments,
            "deployment",
            ApplicationDeploymentDesiredState::Paused,
            "pause",
            2,
        )
        .expect("pause");
        assert_eq!(
            (paused.desired_state, paused.lifecycle_generation),
            (ApplicationDeploymentDesiredState::Paused as i32, 2)
        );
        let repeated = transition_desired_state(
            &mut deployments,
            "deployment",
            ApplicationDeploymentDesiredState::Paused,
            "pause",
            3,
        )
        .expect("idempotent pause");
        assert_eq!(
            (repeated.desired_state, repeated.lifecycle_generation),
            (ApplicationDeploymentDesiredState::Paused as i32, 2)
        );
        let resumed = transition_desired_state(
            &mut deployments,
            "deployment",
            ApplicationDeploymentDesiredState::Present,
            "resume",
            4,
        )
        .expect("resume");
        assert_eq!(
            (resumed.desired_state, resumed.lifecycle_generation),
            (ApplicationDeploymentDesiredState::Present as i32, 4)
        );
    }

    #[test]
    fn lifecycle_transitions_reject_a_reserved_generation_that_does_not_advance() {
        let mut deployments = vec![deployment(ApplicationDeploymentDesiredState::Present)];
        deployments[0].lifecycle_generation = 3;

        assert!(matches!(
            transition_desired_state(
                &mut deployments,
                "deployment",
                ApplicationDeploymentDesiredState::Paused,
                "pause",
                3,
            ),
            Err(ApplicationRepositoryError::InvalidState)
        ));
        assert!(matches!(
            mark_declaration_for_teardown(&mut deployments, "deployment", 2),
            Err(ApplicationRepositoryError::InvalidState)
        ));
        assert_eq!(
            (
                deployments[0].desired_state,
                deployments[0].lifecycle_generation
            ),
            (ApplicationDeploymentDesiredState::Present as i32, 3)
        );

        mark_declaration_for_teardown(&mut deployments, "deployment", 4).expect("teardown");
        assert_eq!(
            (
                deployments[0].desired_state,
                deployments[0].lifecycle_generation
            ),
            (ApplicationDeploymentDesiredState::Teardown as i32, 4)
        );
    }

    #[test]
    fn route_binding_without_projection_is_rejected_on_upsert() {
        let mut deployments = Vec::new();

        assert!(matches!(
            upsert_declaration(&mut deployments, deployment_with_route_but_no_projection()),
            Err(ApplicationRepositoryError::InvalidState)
        ));
        assert_eq!(deployments, Vec::new());
    }

    #[test]
    fn route_binding_without_projection_is_rejected_on_snapshot_validation() {
        assert!(matches!(
            validate_declarations(&[deployment_with_route_but_no_projection()]),
            Err(ApplicationRepositoryError::InvalidState)
        ));
    }

    #[test]
    fn parameter_attachment_must_match_its_retained_value() {
        let with_attachment = |value: &str| {
            let mut deployment = valid_deployment();
            let snapshot = deployment.snapshot.as_mut().expect("snapshot");
            let graph = snapshot.graph.as_mut().expect("graph");
            graph.parameters = vec![ParameterDefinition {
                parameter_id: "mode".to_owned(),
                display_name: "Mode".to_owned(),
                description: String::new(),
                parameter_type: Some(ParameterType::StringEnum(StringEnumParameterSchema {
                    values: vec!["auto".to_owned(), "highvram".to_owned()],
                    default_value: Some("auto".to_owned()),
                })),
                visibility: ParameterVisibility::Advanced as i32,
            }];
            graph.services[0].configuration_bindings = vec![ApplicationConfigurationBinding {
                binding_name: "mode".to_owned(),
                environment_variable: "MODE".to_owned(),
                source: Some(application_configuration_binding::Source::ParameterId(
                    "mode".to_owned(),
                )),
                input: None,
            }];
            snapshot.parameter_values = vec![ParameterValue {
                parameter_id: "mode".to_owned(),
                value: Some(parameter_value::Value::String("highvram".to_owned())),
            }];
            deployment.configuration_attachments = vec![ApplicationConfigurationAttachment {
                service_name: "service".to_owned(),
                binding_name: "mode".to_owned(),
                environment_variable: "MODE".to_owned(),
                value: value.to_owned(),
            }];
            deployment
        };

        assert!(validate_declarations(&[with_attachment("highvram")]).is_ok());
        assert!(matches!(
            validate_declarations(&[with_attachment("auto")]),
            Err(ApplicationRepositoryError::InvalidState)
        ));
    }

    #[test]
    fn secret_attachment_must_carry_its_retained_value_and_generated_ids_name_secrets() {
        let with_secret = |attachment_value: &str, generated: &[&str]| {
            let mut deployment = valid_deployment();
            let snapshot = deployment.snapshot.as_mut().expect("snapshot");
            let graph = snapshot.graph.as_mut().expect("graph");
            graph.parameters = vec![
                ParameterDefinition {
                    parameter_id: "username".to_owned(),
                    display_name: "Username".to_owned(),
                    description: String::new(),
                    parameter_type: Some(ParameterType::Text(TextParameterSchema {
                        minimum_length: 1,
                        maximum_length: 64,
                        default_value: Some("admin".to_owned()),
                    })),
                    visibility: ParameterVisibility::Basic as i32,
                },
                ParameterDefinition {
                    parameter_id: "password".to_owned(),
                    display_name: "Password".to_owned(),
                    description: String::new(),
                    parameter_type: Some(ParameterType::Secret(SecretParameterSchema {
                        generated_length: 32,
                        minimum_length: 12,
                    })),
                    visibility: ParameterVisibility::Basic as i32,
                },
            ];
            graph.services[0].configuration_bindings = vec![ApplicationConfigurationBinding {
                binding_name: "username".to_owned(),
                environment_variable: "USERNAME".to_owned(),
                source: Some(application_configuration_binding::Source::ParameterId(
                    "username".to_owned(),
                )),
                input: None,
            }];
            graph.services[0].secret_bindings = vec![ApplicationSecretBinding {
                binding_name: "password".to_owned(),
                environment_variable: "PASSWORD".to_owned(),
                parameter_id: "password".to_owned(),
                form: ApplicationSecretForm::Plain as i32,
            }];
            snapshot.parameter_values = vec![
                ParameterValue {
                    parameter_id: "username".to_owned(),
                    value: Some(parameter_value::Value::String("admin".to_owned())),
                },
                ParameterValue {
                    parameter_id: "password".to_owned(),
                    value: Some(parameter_value::Value::String(
                        "retained-password".to_owned(),
                    )),
                },
            ];
            snapshot.generated_parameter_ids =
                generated.iter().map(|id| (*id).to_owned()).collect();
            deployment.configuration_attachments = vec![ApplicationConfigurationAttachment {
                service_name: "service".to_owned(),
                binding_name: "username".to_owned(),
                environment_variable: "USERNAME".to_owned(),
                value: "admin".to_owned(),
            }];
            deployment.secret_attachments = vec![ApplicationSecretAttachment {
                service_name: "service".to_owned(),
                binding_name: "password".to_owned(),
                environment_variable: "PASSWORD".to_owned(),
                value: attachment_value.to_owned(),
            }];
            deployment
        };

        assert!(validate_declarations(&[with_secret("retained-password", &["password"])]).is_ok());
        assert!(validate_declarations(&[with_secret("retained-password", &[])]).is_ok());
        for invalid in [
            with_secret("other-password-1", &["password"]),
            with_secret("retained-password", &["username"]),
            with_secret("retained-password", &["password", "password"]),
            {
                let mut missing = with_secret("retained-password", &[]);
                missing.secret_attachments.clear();
                missing
            },
        ] {
            assert!(matches!(
                validate_declarations(&[invalid]),
                Err(ApplicationRepositoryError::InvalidState)
            ));
        }
    }

    #[test]
    fn resolved_snapshot_validation_rejects_incomplete_or_ambiguous_selection_contracts() {
        let valid = valid_deployment();
        assert!(validate_declarations(std::slice::from_ref(&valid)).is_ok());

        let mutations: [fn(&mut ResolvedApplicationDeploymentSnapshot); 9] = [
            |snapshot| {
                snapshot
                    .placement
                    .as_mut()
                    .expect("placement")
                    .execution_target_id
                    .clear()
            },
            |snapshot| snapshot.platform = None,
            |snapshot| {
                snapshot.platform.as_mut().expect("platform").architecture = "any".to_owned()
            },
            |snapshot| {
                snapshot.platform.as_mut().expect("platform").architecture = "arm64".to_owned()
            },
            |snapshot| snapshot.service_selections[0].preparation_policy = 99,
            |snapshot| snapshot.service_selections[0].launch_identity.clear(),
            |snapshot| snapshot.service_selections.clear(),
            |snapshot| {
                snapshot
                    .service_selections
                    .push(snapshot.service_selections[0].clone())
            },
            |snapshot| snapshot.service_selections[0].service_name = "missing".to_owned(),
        ];
        for mutate in mutations {
            let mut invalid = valid.clone();
            mutate(invalid.snapshot.as_mut().expect("snapshot"));
            assert!(matches!(
                validate_declarations(&[invalid]),
                Err(ApplicationRepositoryError::InvalidState)
            ));
        }
    }

    #[test]
    fn local_development_selection_requires_a_tag_and_immutable_local_image_id() {
        let mut deployment = valid_deployment();
        let snapshot = deployment.snapshot.as_mut().expect("snapshot");
        let graph_reference = snapshot.graph.as_mut().expect("graph").services[0]
            .artifact
            .as_mut()
            .expect("artifact")
            .oci_mut()
            .expect("reference");
        graph_reference.tag = Some("dev".to_owned());
        graph_reference.digest = None;
        snapshot.service_selections[0].preparation_policy =
            ArtifactPreparationPolicy::LocalDevelopment as i32;
        snapshot.service_selections[0].launch_identity =
            "sha256:bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb".to_owned();

        assert!(validate_declarations(&[deployment.clone()]).is_ok());

        deployment
            .snapshot
            .as_mut()
            .expect("snapshot")
            .service_selections[0]
            .launch_identity = "dotcore/application:dev".to_owned();
        assert!(matches!(
            validate_declarations(&[deployment]),
            Err(ApplicationRepositoryError::InvalidState)
        ));
    }

    #[test]
    fn only_assigned_declarations_leave_local_development_identities_for_the_node() {
        const IMAGE: &str =
            "sha256:bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";
        let mut node_resolved = valid_deployment();
        let snapshot = node_resolved.snapshot.as_mut().expect("snapshot");
        let graph_reference = snapshot.graph.as_mut().expect("graph").services[0]
            .artifact
            .as_mut()
            .expect("artifact")
            .oci_mut()
            .expect("reference");
        graph_reference.tag = Some("dev".to_owned());
        graph_reference.digest = None;
        snapshot.service_selections[0].preparation_policy =
            ArtifactPreparationPolicy::LocalDevelopment as i32;
        snapshot.service_selections[0].launch_identity.clear();
        let service = snapshot.service_selections[0].service_name.clone();
        let pin = |selection_key: &str, launch_identity: &str| NodeResolvedLaunchIdentity {
            selection_key: selection_key.to_owned(),
            launch_identity: launch_identity.to_owned(),
        };

        assert!(validate_declaration(&node_resolved).is_ok());
        let mut projected = node_resolved.clone();
        projected.node_resolved_launch_identities = vec![pin(&service, IMAGE)];
        assert!(validate_declaration(&projected).is_ok());
        let mut deployments = Vec::new();
        upsert_declaration(&mut deployments, node_resolved.clone())
            .expect("the hosting node resolves the identity");
        assert_eq!(deployments, [node_resolved.clone()]);

        let mut digest_without_identity = valid_deployment();
        digest_without_identity
            .snapshot
            .as_mut()
            .expect("snapshot")
            .service_selections[0]
            .launch_identity
            .clear();
        for invalid in [
            digest_without_identity,
            ApplicationDeploymentDeclaration {
                node_resolved_launch_identities: vec![pin(&service, "dotcore/app:dev")],
                ..node_resolved.clone()
            },
            ApplicationDeploymentDeclaration {
                node_resolved_launch_identities: vec![pin("missing", IMAGE)],
                ..node_resolved.clone()
            },
            ApplicationDeploymentDeclaration {
                node_resolved_launch_identities: vec![pin(&service, IMAGE), pin(&service, IMAGE)],
                ..node_resolved.clone()
            },
            ApplicationDeploymentDeclaration {
                node_resolved_launch_identities: vec![pin(&service, IMAGE)],
                ..valid_deployment()
            },
        ] {
            assert!(matches!(
                validate_declaration(&invalid),
                Err(ApplicationRepositoryError::InvalidState)
            ));
        }
    }

    #[test]
    fn teardown_never_reverses_or_accepts_pause_resume() {
        let mut deployments = vec![deployment(ApplicationDeploymentDesiredState::Teardown)];
        for (target, operation) in [
            (ApplicationDeploymentDesiredState::Paused, "pause"),
            (ApplicationDeploymentDesiredState::Present, "resume"),
        ] {
            assert!(matches!(
                transition_desired_state(&mut deployments, "deployment", target, operation, 2),
                Err(ApplicationRepositoryError::InvalidTransition)
            ));
        }
        assert_eq!(
            deployments[0].desired_state,
            ApplicationDeploymentDesiredState::Teardown as i32
        );
    }

    fn deployment(
        desired_state: ApplicationDeploymentDesiredState,
    ) -> ApplicationDeploymentDeclaration {
        ApplicationDeploymentDeclaration {
            application_deployment_id: "deployment".to_owned(),
            desired_state: desired_state as i32,
            lifecycle_generation: 1,
            ..Default::default()
        }
    }

    fn valid_deployment() -> ApplicationDeploymentDeclaration {
        let mut deployment = deployment_with_route_but_no_projection();
        deployment.inference_route_id = None;
        deployment
    }

    fn deployment_with_route_but_no_projection() -> ApplicationDeploymentDeclaration {
        ApplicationDeploymentDeclaration {
            lifecycle_generation: 1,
            application_deployment_id: "deployment".to_owned(),
            application_deployment_name: "Deployment".to_owned(),
            placement: Some(dotcore_proto::dotcore::deployment::v1::ExactPlacement {
                site_node_id: "node-one".to_owned(),
                execution_target_id: "application-target".to_owned(),
            }),
            snapshot: Some(resolved_snapshot(Application {
                application_id: "application".to_owned(),
                display_name: "Application".to_owned(),
                description: String::new(),
                notes: String::new(),
                version: "1.0.0".to_owned(),
                graph: Some(ApplicationGraph {
                    services: vec![ApplicationService {
                        service_name: "service".to_owned(),
                        artifact: Some(digest_artifact(
                            Some("example.test"),
                            "application",
                            "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
                            None,
                        )),
                        command: vec!["application".to_owned()],
                        ports: vec![ApplicationServicePort {
                            port_name: "http".to_owned(),
                            port_number: 8080,
                        }],
                        ..Default::default()
                    }],
                    primary_endpoint: Some(ApplicationPrimaryEndpoint {
                        service_name: "service".to_owned(),
                        port_name: "http".to_owned(),
                        protocol: ApplicationEndpointProtocol::Http as i32,
                    }),
                    parameters: Vec::new(),
                }),
            })),
            desired_state: ApplicationDeploymentDesiredState::Present as i32,
            inference_route_id: Some("route-id".to_owned()),
            published_port: 30_000,
            ..Default::default()
        }
    }

    fn resolved_snapshot(application: Application) -> ResolvedApplicationDeploymentSnapshot {
        let graph = application.graph.clone().expect("Application graph");
        ResolvedApplicationDeploymentSnapshot {
            application_id: application.application_id,
            display_name: application.display_name,
            version: application.version,
            notes: application.notes,
            graph: Some(graph),
            placement: Some(dotcore_proto::dotcore::deployment::v1::ExactPlacement {
                site_node_id: "node-one".to_owned(),
                execution_target_id: "application-target".to_owned(),
            }),
            platform: Some(dotcore_proto::dotcore::catalog::v1::OciPlatform {
                operating_system: "linux".to_owned(),
                architecture: "amd64".to_owned(),
                variant: None,
            }),
            accelerator_device_ids: Vec::new(),
            service_selections: vec![ResolvedApplicationServiceSelection {
                service_name: "service".to_owned(),
                preparation_policy: ArtifactPreparationPolicy::ImmutableRemote as i32,
                launch_identity: "example.test/application@sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa".to_owned(),
            }],
            parameter_values: Vec::new(),
            generated_parameter_ids: Vec::new(),
        }
    }

    fn starting(id: &str) -> dotcore_proto::dotcore::application::v1::CurrentApplicationDeployment {
        use dotcore_proto::dotcore::application::v1::{
            ApplicationDeploymentState, ApplicationLifecycleState, ApplicationReadinessState,
            CurrentApplicationDeployment,
        };

        CurrentApplicationDeployment {
            application_deployment_id: id.to_owned(),
            state: Some(ApplicationDeploymentState {
                lifecycle_state: ApplicationLifecycleState::Starting as i32,
                readiness_state: ApplicationReadinessState::NotReady as i32,
                stage: "starting".to_owned(),
                ..Default::default()
            }),
            ..Default::default()
        }
    }
}
