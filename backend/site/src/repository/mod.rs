//! Shared site configuration repository with owner-scoped declaration views and mutations.

use std::{
    collections::BTreeSet,
    path::{Path, PathBuf},
    sync::{Arc, Mutex, MutexGuard},
};

use anyhow::anyhow;
use dotcore_domain::deployment::DeploymentOwner;
use dotcore_domain::inference_route::{RouteUnloadPlan, classify_unload};
use dotcore_proto::{
    dotcore::{
        application::v1::{ApplicationDeploymentDeclaration, ApplicationDeploymentDesiredState},
        inference::v1::InferenceRoute,
        model::v1::{
            ModelDeploymentDeclaration, ModelDeploymentDesiredState,
            ModelDeploymentLifecycleTransition,
        },
        site::v1::{ConsoleSettings, Site},
    },
    yaml,
};
use tracing::{error, info, warn};

use super::{
    persistence::{AtomicSiteFileWriter, SiteFileWriter},
    validation::{SiteDeclarationRules, load_validated_site, validate_site},
    view::desired::reject_node_view_growth,
};

type SiteChangeObserver = Arc<dyn Fn(&Site) + Send + Sync>;
type SiteChangeObservers = Arc<Mutex<Vec<SiteChangeObserver>>>;

/// Atomically captured model-controller site declarations.
#[derive(Clone, Debug, PartialEq)]
pub struct ModelSiteSnapshot {
    pub site_id: String,
    pub execution_targets: Vec<dotcore_proto::dotcore::site::v1::ExecutionTarget>,
    pub deployments: Vec<ModelDeploymentDeclaration>,
}

/// Atomically captured apps-controller site declarations.
#[derive(Clone, Debug, PartialEq)]
pub struct ApplicationSiteSnapshot {
    pub site_id: String,
    pub nodes: Vec<dotcore_proto::dotcore::site::v1::SiteNode>,
    pub execution_targets: Vec<dotcore_proto::dotcore::site::v1::ExecutionTarget>,
    pub deployments: Vec<ApplicationDeploymentDeclaration>,
}

/// Loads, validates, and rewrites the complete shared site configuration.
///
/// Accepted memory stays authoritative when a `site.yaml` write fails or is uncertain; the site is
/// then dirty until a later whole-file write succeeds, and dirty memory never justifies an
/// irreversible cross-node effect such as a cleanup acknowledgement.
pub struct SiteRepository {
    path: PathBuf,
    /// Site lock. It follows the node-control coordinator and any owner operation guard and
    /// precedes the current-state lock; it is never taken while holding current state. Change
    /// observers run under it and may take only leaf locks such as a route snapshot or `Notify`.
    state: Arc<Mutex<SiteState>>,
    writer: Arc<dyn SiteFileWriter>,
    listeners: Arc<Mutex<Vec<std::sync::Weak<tokio::sync::Notify>>>>,
    change_observers: SiteChangeObservers,

    /// Owner rules every loaded and mutated site must satisfy.
    rules: SiteDeclarationRules,
}

impl SiteRepository {
    /// Loads one complete desired site without rewriting human-authored YAML; it and every later
    /// mutation must satisfy the owners' `rules`.
    pub fn load(path: &Path, rules: SiteDeclarationRules) -> Result<Self, SiteRepositoryError> {
        Self::load_with_writer(path, Arc::new(AtomicSiteFileWriter), rules)
    }

    #[cfg(any(test, feature = "test-support"))]
    pub fn load_with_mutation_failure_for_test(path: &Path) -> Result<Self, SiteRepositoryError> {
        struct PreRenameMutationFailureWriter;

        impl SiteFileWriter for PreRenameMutationFailureWriter {
            fn replace(
                &self,
                _: &Path,
                _: &str,
            ) -> Result<(), super::persistence::SiteStorageError> {
                Err(super::persistence::SiteStorageError::Storage(
                    anyhow::anyhow!("injected accepted site mutation failure"),
                ))
            }
        }

        Self::load_with_writer(
            path,
            Arc::new(PreRenameMutationFailureWriter),
            crate::test_support::site_declaration_rules(),
        )
    }

    /// Loads a site whose `site.yaml` writes follow queued faults before real atomic replacement.
    #[cfg(any(test, feature = "test-support"))]
    pub fn load_with_write_faults_for_test(
        path: &Path,
    ) -> Result<(Self, SiteWriteFaults), SiteRepositoryError> {
        let faults = SiteWriteFaults::default();
        let repository = Self::load_with_writer(
            path,
            Arc::new(faults.clone()),
            crate::test_support::site_declaration_rules(),
        )?;
        Ok((repository, faults))
    }

    /// Returns the current process-local desired site view.
    pub fn snapshot(&self) -> Result<Site, SiteRepositoryError> {
        let state = lock(&self.state)?;
        Ok(state.site.clone())
    }

    #[cfg(any(test, feature = "test-support"))]
    pub fn state_pointer(&self) -> usize {
        Arc::as_ptr(&self.state) as usize
    }

    /// Replaces the complete desired site after validation and atomic persistence in tests.
    #[cfg(any(test, feature = "test-support"))]
    pub fn replace(&self, candidate: Site) -> Result<(), SiteRepositoryError> {
        self.mutate(|site| {
            *site = candidate;
            Ok(())
        })
    }

    /// Runs an owner-defined application declaration mutation through the shared transaction seam.
    pub fn mutate_application_deployments<T, E: std::error::Error + 'static>(
        &self,
        operation: impl FnOnce(&mut Vec<ApplicationDeploymentDeclaration>) -> Result<T, E>,
    ) -> Result<T, ScopedSiteMutationError<E>> {
        let mut current = lock(&self.state).map_err(ScopedSiteMutationError::Repository)?;
        let mut candidate = current.site.clone();
        let result = operation(&mut candidate.application_deployments)
            .map_err(ScopedSiteMutationError::Owner)?;
        validate_site(&candidate, &self.rules).map_err(ScopedSiteMutationError::Repository)?;
        let contents = yaml::to_string(&candidate)
            .map_err(SiteRepositoryError::SerializeYaml)
            .map_err(ScopedSiteMutationError::Repository)?;
        self.persist_mutation(&mut current, candidate, &contents)
            .map_err(ScopedSiteMutationError::Repository)?;
        Ok(result)
    }

    /// Runs an owner-defined model declaration mutation through the shared transaction seam.
    pub fn mutate_model_deployments<T, E: std::error::Error + 'static>(
        &self,
        operation: impl FnOnce(&mut ModelSiteTransaction<'_>) -> Result<T, E>,
    ) -> Result<T, ScopedSiteMutationError<E>> {
        let mut current = lock(&self.state).map_err(ScopedSiteMutationError::Repository)?;
        let mut candidate = current.site.clone();
        let mut transaction = ModelSiteTransaction {
            deployments: &mut candidate.model_deployments,
        };
        let result = operation(&mut transaction).map_err(ScopedSiteMutationError::Owner)?;
        validate_site(&candidate, &self.rules).map_err(ScopedSiteMutationError::Repository)?;
        let contents = yaml::to_string(&candidate)
            .map_err(SiteRepositoryError::SerializeYaml)
            .map_err(ScopedSiteMutationError::Repository)?;
        self.persist_mutation(&mut current, candidate, &contents)
            .map_err(ScopedSiteMutationError::Repository)?;
        Ok(result)
    }

    /// Persists a model presentation-metadata mutation without triggering reconciliation.
    ///
    /// Names are excluded from serving and runtime projections, so their durable update does not
    /// require controller observers to rebuild those projections.
    pub fn mutate_model_deployment_metadata<T, E: std::error::Error + 'static>(
        &self,
        operation: impl FnOnce(&mut ModelSiteTransaction<'_>) -> Result<T, E>,
    ) -> Result<T, ScopedSiteMutationError<E>> {
        self.mutate_without_reconciliation(|candidate| {
            let mut transaction = ModelSiteTransaction {
                deployments: &mut candidate.model_deployments,
            };
            operation(&mut transaction)
        })
    }

    /// Persists an application presentation-metadata mutation without triggering reconciliation.
    ///
    /// Names are excluded from runtime projections, so their durable update does not require the
    /// apps controller observer to rebuild a workload group.
    pub fn mutate_application_deployment_metadata<T, E: std::error::Error + 'static>(
        &self,
        operation: impl FnOnce(&mut Vec<ApplicationDeploymentDeclaration>) -> Result<T, E>,
    ) -> Result<T, ScopedSiteMutationError<E>> {
        self.mutate_without_reconciliation(|candidate| {
            operation(&mut candidate.application_deployments)
        })
    }

    /// Persists a site-wide console settings mutation without triggering reconciliation.
    ///
    /// Console settings are operator choices about the console only: no controller reads them
    /// and they never enter a node's desired view, so their update wakes nothing.
    pub fn mutate_console_settings<T, E: std::error::Error + 'static>(
        &self,
        operation: impl FnOnce(&mut ConsoleSettings) -> Result<T, E>,
    ) -> Result<T, ScopedSiteMutationError<E>> {
        self.mutate_without_reconciliation(|candidate| {
            operation(
                candidate
                    .console_settings
                    .get_or_insert_with(Default::default),
            )
        })
    }

    /// Accepts one owner-prepared deployment and its optional Direct-exposure route as one
    /// authoritative site update.
    pub fn accept_model_deployment_exposure(
        &self,
        deployment: ModelDeploymentDeclaration,
        route: Option<InferenceRoute>,
    ) -> Result<(), SiteRepositoryError> {
        let mut current = lock(&self.state)?;
        let mut candidate = current.site.clone();
        candidate.model_deployments.push(deployment);
        if let Some(route) = route {
            candidate.inference_routes.push(route);
        }
        validate_site(&candidate, &self.rules)?;
        let contents = yaml::to_string(&candidate).map_err(SiteRepositoryError::SerializeYaml)?;
        self.persist_mutation(&mut current, candidate, &contents)
    }

    /// Reports whether routes or applications currently block one deployment's unload, without
    /// mutating the site. The check is advisory: acceptance repeats it under the site lock.
    pub fn check_model_deployment_unload(
        &self,
        deployment_id: &str,
    ) -> Result<(), ModelUnloadAcceptanceError> {
        let current = lock(&self.state).map_err(ModelUnloadAcceptanceError::Site)?;
        unload_route_plan(&current.site, deployment_id).map(|_| ())
    }

    /// Accepts one deployment teardown at a reserved lifecycle generation and removes its
    /// sole-member routes in the same update. A deployment already in teardown is returned
    /// unchanged.
    ///
    /// The route manager's teardown rule runs on the locked site, so concurrent route edits are
    /// either fully seen or rejected.
    pub fn accept_model_deployment_unload(
        &self,
        deployment_id: &str,
        reserved_generation: u64,
    ) -> Result<ModelDeploymentDeclaration, ModelUnloadAcceptanceError> {
        let mut current = lock(&self.state).map_err(ModelUnloadAcceptanceError::Site)?;
        if let Some(existing) = current.site.model_deployments.iter().find(|deployment| {
            deployment.model_deployment_id == deployment_id
                && deployment.desired_state == ModelDeploymentDesiredState::Teardown as i32
        }) {
            return Ok(existing.clone());
        }
        let routes = unload_route_plan(&current.site, deployment_id)?;
        let mut candidate = current.site.clone();
        let deployment = candidate
            .model_deployments
            .iter_mut()
            .find(|deployment| deployment.model_deployment_id == deployment_id)
            .ok_or_else(|| {
                ModelUnloadAcceptanceError::DeploymentNotFound(deployment_id.to_owned())
            })?;
        if reserved_generation <= deployment.lifecycle_generation {
            return Err(ModelUnloadAcceptanceError::Site(
                SiteRepositoryError::invalid_site(
                    "reserved lifecycle generation does not advance accepted intent",
                ),
            ));
        }
        deployment.lifecycle_generation = reserved_generation;
        deployment.lifecycle_transition = ModelDeploymentLifecycleTransition::Teardown as i32;
        deployment.lifecycle_recovery_required = false;
        deployment.desired_state = ModelDeploymentDesiredState::Teardown as i32;
        let deployment = deployment.clone();
        candidate
            .inference_routes
            .retain(|route| !routes.removes(route));
        validate_site(&candidate, &self.rules).map_err(ModelUnloadAcceptanceError::Site)?;
        let contents = yaml::to_string(&candidate)
            .map_err(SiteRepositoryError::SerializeYaml)
            .map_err(ModelUnloadAcceptanceError::Site)?;
        self.persist_mutation(&mut current, candidate, &contents)
            .map_err(ModelUnloadAcceptanceError::Site)?;
        Ok(deployment)
    }

    /// Runs a route-manager-defined declaration mutation through the shared transaction seam.
    pub fn mutate_inference_routes<T, E: std::error::Error + 'static>(
        &self,
        operation: impl FnOnce(&mut RouteSiteTransaction<'_>) -> Result<T, E>,
    ) -> Result<T, ScopedSiteMutationError<E>> {
        let mut current = lock(&self.state).map_err(ScopedSiteMutationError::Repository)?;
        let mut candidate = current.site.clone();
        let mut transaction = RouteSiteTransaction {
            site_id: &candidate.site_id,
            deployments: &candidate.model_deployments,
            applications: &candidate.application_deployments,
            routes: &mut candidate.inference_routes,
        };
        let result = operation(&mut transaction).map_err(ScopedSiteMutationError::Owner)?;
        validate_site(&candidate, &self.rules).map_err(ScopedSiteMutationError::Repository)?;
        let contents = yaml::to_string(&candidate)
            .map_err(SiteRepositoryError::SerializeYaml)
            .map_err(ScopedSiteMutationError::Repository)?;
        self.persist_mutation(&mut current, candidate, &contents)
            .map_err(ScopedSiteMutationError::Repository)?;
        Ok(result)
    }

    /// Registers one controller listener for accepted management mutations or future reloads.
    pub fn register_change_listener(&self) -> Arc<tokio::sync::Notify> {
        let listener = Arc::new(tokio::sync::Notify::new());
        if let Ok(mut listeners) = self.listeners.lock() {
            // Listeners dropped since the last change are forgotten here too, so short-lived
            // listeners never accumulate while the site stays unchanged.
            listeners.retain(|listener| listener.strong_count() > 0);
            listeners.push(Arc::downgrade(&listener));
        }
        listener
    }

    /// Registers an owner-owned projection updated synchronously for accepted site changes.
    pub fn register_change_observer(&self, observer: SiteChangeObserver) {
        if let Ok(mut observers) = self.change_observers.lock() {
            observers.push(observer);
        }
    }

    fn load_with_writer(
        path: &Path,
        writer: Arc<dyn SiteFileWriter>,
        rules: SiteDeclarationRules,
    ) -> Result<Self, SiteRepositoryError> {
        let site = load_validated_site(path, &rules)?;
        Ok(Self {
            path: path.to_owned(),
            state: Arc::new(Mutex::new(SiteState { site, dirty: false })),
            writer,
            listeners: Arc::new(Mutex::new(Vec::new())),
            change_observers: Arc::new(Mutex::new(Vec::new())),
            rules,
        })
    }

    /// Accepts and writes a mutation of sections no controller or node view projects, without
    /// running change observers or waking listeners.
    fn mutate_without_reconciliation<T, E: std::error::Error + 'static>(
        &self,
        operation: impl FnOnce(&mut Site) -> Result<T, E>,
    ) -> Result<T, ScopedSiteMutationError<E>> {
        let mut current = lock(&self.state).map_err(ScopedSiteMutationError::Repository)?;
        let mut candidate = current.site.clone();
        let result = operation(&mut candidate).map_err(ScopedSiteMutationError::Owner)?;
        validate_site(&candidate, &self.rules).map_err(ScopedSiteMutationError::Repository)?;
        reject_node_view_growth(&current.site, &candidate)
            .map_err(ScopedSiteMutationError::Repository)?;
        let contents = yaml::to_string(&candidate)
            .map_err(SiteRepositoryError::SerializeYaml)
            .map_err(ScopedSiteMutationError::Repository)?;
        self.write_accepted(&mut current, candidate, &contents)
            .map_err(ScopedSiteMutationError::Repository)?;
        Ok(result)
    }

    #[cfg(any(test, feature = "test-support"))]
    fn mutate(
        &self,
        operation: impl FnOnce(&mut Site) -> Result<(), SiteRepositoryError>,
    ) -> Result<(), SiteRepositoryError> {
        let mut current = lock(&self.state)?;
        let (candidate, contents) = Self::prepare_mutation(&current.site, &self.rules, operation)?;
        self.persist_mutation(&mut current, candidate, &contents)
    }

    #[cfg(any(test, feature = "test-support"))]
    fn prepare_mutation(
        site: &Site,
        rules: &SiteDeclarationRules,
        operation: impl FnOnce(&mut Site) -> Result<(), SiteRepositoryError>,
    ) -> Result<(Site, String), SiteRepositoryError> {
        let mut candidate = site.clone();
        operation(&mut candidate)?;
        validate_site(&candidate, rules)?;
        let contents = yaml::to_string(&candidate).map_err(SiteRepositoryError::SerializeYaml)?;
        Ok((candidate, contents))
    }

    /// Accepts a validated candidate, writes it, and notifies controllers. A candidate that grows
    /// a node's desired view past the private wire limits is rejected before acceptance,
    /// without a write or notification.
    fn persist_mutation(
        &self,
        current: &mut SiteState,
        candidate: Site,
        contents: &str,
    ) -> Result<(), SiteRepositoryError> {
        reject_node_view_growth(&current.site, &candidate)?;
        let persistence = self.write_accepted(current, candidate, contents);
        if let Ok(observers) = self.change_observers.lock() {
            for observer in observers.iter() {
                observer(&current.site);
            }
        }
        if let Ok(mut listeners) = self.listeners.lock() {
            listeners.retain(|listener| {
                listener.upgrade().is_some_and(|listener| {
                    listener.notify_one();
                    true
                })
            });
        }
        persistence
    }

    /// The single `site.yaml` write seam. Dirty is set before accepted memory changes and cleared
    /// only after a confirmed whole-file replacement, which also persists any earlier failed or
    /// uncertain mutation still held in memory.
    fn write_accepted(
        &self,
        current: &mut SiteState,
        candidate: Site,
        contents: &str,
    ) -> Result<(), SiteRepositoryError> {
        current.dirty = true;
        current.site = candidate;
        let persistence = self.writer.replace(&self.path, contents);
        match &persistence {
            Ok(()) => {
                current.dirty = false;
                info!(
                    event_kind = "site_store_write_succeeded",
                    storage = "site.yaml",
                    path = %self.path.display(),
                    "Site store wrote site.yaml"
                )
            }
            Err(error)
                if matches!(
                    error,
                    super::persistence::SiteStorageError::OutcomeUncertain(_)
                ) =>
            {
                warn!(
                    event_kind = "site_store_write_uncertain",
                    storage = "site.yaml",
                    path = %self.path.display(),
                    error = %error,
                    "Site store persistence outcome is uncertain"
                )
            }
            Err(error) => error!(
                event_kind = "site_store_write_failed",
                storage = "site.yaml",
                path = %self.path.display(),
                error = %error,
                "Site store persistence failed"
            ),
        }
        persistence.map_err(SiteRepositoryError::Storage)
    }

    /// Rewrites unchanged accepted memory when an earlier write failed or is uncertain.
    fn flush(&self, current: &mut SiteState) -> Result<(), SiteRepositoryError> {
        if !current.dirty {
            return Ok(());
        }
        let site = current.site.clone();
        let contents = yaml::to_string(&site).map_err(SiteRepositoryError::SerializeYaml)?;
        self.write_accepted(current, site, &contents)
    }

    /// Durably reserves one never-reused model deployment identity before creation acceptance.
    pub fn reserve_model_deployment_id(&self) -> Result<String, SiteRepositoryError> {
        let mut current = lock(&self.state)?;
        let mut candidate = current.site.clone();
        let allocator = candidate
            .model_deployment_identity_allocator
            .as_mut()
            .ok_or_else(|| {
                SiteRepositoryError::invalid_site("model deployment identity allocator is missing")
            })?;
        if allocator.namespace.len() != 16
            || !allocator
                .namespace
                .bytes()
                .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
            || allocator.next_sequence == 0
        {
            return Err(SiteRepositoryError::invalid_site(
                "model deployment identity allocator is invalid",
            ));
        }
        let sequence = allocator.next_sequence;
        allocator.next_sequence = sequence.checked_add(1).ok_or_else(|| {
            SiteRepositoryError::invalid_site("model deployment identity sequence exhausted")
        })?;
        let deployment_id = format!("model-{}-{sequence}", allocator.namespace);
        validate_site(&candidate, &self.rules)?;
        let contents = yaml::to_string(&candidate).map_err(SiteRepositoryError::SerializeYaml)?;
        self.persist_mutation(&mut current, candidate, &contents)?;
        Ok(deployment_id)
    }

    /// Runs `read` over the accepted model declarations while holding the site guard, so no site
    /// mutation can interleave with it. `read` may take the current-state lock but never site.
    pub fn with_model_deployments<T>(
        &self,
        read: impl FnOnce(&[ModelDeploymentDeclaration]) -> T,
    ) -> Result<T, SiteRepositoryError> {
        let current = lock(&self.state)?;
        Ok(read(&current.site.model_deployments))
    }

    /// Runs `read` over the accepted application declarations while holding the site guard, so no
    /// site mutation can interleave with it. `read` may take the current-state lock but never site.
    pub fn with_application_deployments<T>(
        &self,
        read: impl FnOnce(&[ApplicationDeploymentDeclaration]) -> T,
    ) -> Result<T, SiteRepositoryError> {
        let current = lock(&self.state)?;
        Ok(read(&current.site.application_deployments))
    }

    /// Runs `read` over the complete accepted site while holding the site guard, so no site
    /// mutation can interleave with it. `read` may take only leaf locks, never site or current
    /// state.
    pub fn with_site<T>(&self, read: impl FnOnce(&Site) -> T) -> Result<T, SiteRepositoryError> {
        let current = lock(&self.state)?;
        Ok(read(&current.site))
    }

    /// Runs `finalize` over accepted deployment declarations while holding the site guard, so
    /// teardown classification, conditional removal, and durability cannot interleave with another
    /// site mutation. `finalize` may take the current-state lock but never site.
    pub fn with_deployment_teardowns<T>(
        &self,
        finalize: impl FnOnce(&mut DeploymentTeardownTransaction<'_>) -> T,
    ) -> Result<T, SiteRepositoryError> {
        let mut current = lock(&self.state)?;
        let mut transaction = DeploymentTeardownTransaction {
            repository: self,
            current: &mut current,
            candidate: None,
        };
        Ok(finalize(&mut transaction))
    }

    #[cfg(any(test, feature = "test-support"))]
    pub fn is_dirty(&self) -> bool {
        lock(&self.state).expect("desired site").dirty
    }

    /// Returns the required atomic site declaration view for model operations.
    pub fn model_snapshot(&self) -> Result<ModelSiteSnapshot, SiteRepositoryError> {
        let site = self.snapshot()?;
        Ok(ModelSiteSnapshot {
            site_id: site.site_id,
            execution_targets: site.execution_targets,
            deployments: site.model_deployments,
        })
    }

    /// Returns the required atomic site declaration view for application operations.
    pub fn application_snapshot(&self) -> Result<ApplicationSiteSnapshot, SiteRepositoryError> {
        let site = self.snapshot()?;
        Ok(ApplicationSiteSnapshot {
            site_id: site.site_id,
            nodes: site.nodes,
            execution_targets: site.execution_targets,
            deployments: site.application_deployments,
        })
    }
}

impl Clone for SiteRepository {
    fn clone(&self) -> Self {
        Self {
            path: self.path.clone(),
            state: Arc::clone(&self.state),
            writer: Arc::clone(&self.writer),
            listeners: Arc::clone(&self.listeners),
            change_observers: Arc::clone(&self.change_observers),
            rules: self.rules,
        }
    }
}

struct SiteState {
    site: Site,
    /// Whether accepted memory may differ from the last confirmed `site.yaml` write.
    dirty: bool,
}

#[derive(Debug, thiserror::Error)]
pub enum SiteRepositoryError {
    #[error("Invalid desired site: {0}")]
    InvalidSite(#[source] anyhow::Error),

    #[error("Desired site YAML failed: {0}")]
    Yaml(#[from] dotcore_proto::yaml::RestrictedYamlError),

    /// site.yaml is well-formed YAML that this build's site schema cannot decode, as happens
    /// with a site written by a build whose schema differs.
    #[error(
        "site.yaml does not match the site schema of this build ({0}); correct the named value or reset the site"
    )]
    SchemaMismatch(#[source] serde_json::Error),

    #[error("could not serialize site.yaml: {0}")]
    SerializeYaml(#[source] dotcore_proto::yaml::RestrictedYamlError),

    #[error("Desired site storage failed: {0}")]
    Storage(#[source] super::persistence::SiteStorageError),

    /// The mutation would grow one `SiteNode`'s desired view past the private wire limits, so its
    /// node could never install it.
    #[error("desired view for node {node_id:?} would exceed the private node-view limits")]
    NodeViewLimitExceeded { node_id: String },

    #[error("desired site is unavailable")]
    Unavailable,

    /// This node holds no accepted desired view from the site manager, either before its first
    /// view or after a manager-session change cleared it. It clears once a view is installed.
    #[error("this node has not accepted a desired view from the site manager yet")]
    NoAcceptedView,
}

impl SiteRepositoryError {
    pub(crate) fn invalid_site(message: impl Into<String>) -> Self {
        Self::InvalidSite(anyhow!(message.into()))
    }

    pub(crate) fn invalid_site_with_source(
        message: impl Into<String>,
        source: impl std::error::Error + Send + Sync + 'static,
    ) -> Self {
        Self::InvalidSite(anyhow::Error::new(source).context(message.into()))
    }
}

/// Outcomes from the focused deployment-unload and sole-member route cleanup acceptance seam.
#[derive(Debug, thiserror::Error)]
pub enum ModelUnloadAcceptanceError {
    #[error(transparent)]
    Site(#[from] SiteRepositoryError),

    #[error("desired deployment {0:?} was not found")]
    DeploymentNotFound(String),

    #[error("Inference routes shared with other deployments reference the model deployment: {0:?}")]
    SharedRouteReferences(Vec<String>),

    #[error(
        "Application deployments use inference routes that would be removed with the model deployment: {0:?}"
    )]
    ApplicationRouteReferences(Vec<ApplicationRouteReference>),
}

/// One application deployment using an inference route, as named in the site declarations: an
/// immutable fact that crosses repository and controller error boundaries without owning
/// application lifecycle.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ApplicationRouteReference {
    pub application_deployment_id: String,
    pub application_deployment_name: String,
    pub inference_route_id: String,
    pub inference_route_name: String,
}

/// Mutable inference-controller-owned deployment section.
pub struct ModelSiteTransaction<'a> {
    pub deployments: &'a mut Vec<ModelDeploymentDeclaration>,
}

/// Exact teardown that authorizes removal of one owner's deployment declaration: its owner,
/// identity, teardown lifecycle generation, and the `SiteNode` named by its placement, if any.
#[derive(Clone, Copy, Debug)]
pub struct DeploymentTeardownIdentity<'a> {
    pub owner: DeploymentOwner,
    pub deployment_id: &'a str,
    pub lifecycle_generation: u64,
    pub site_node_id: Option<&'a str>,
}

impl<'a> DeploymentTeardownIdentity<'a> {
    /// Names the teardown carried by one model declaration.
    pub fn of_model(declaration: &'a ModelDeploymentDeclaration) -> Self {
        Self {
            owner: DeploymentOwner::Model,
            deployment_id: &declaration.model_deployment_id,
            lifecycle_generation: declaration.lifecycle_generation,
            site_node_id: declaration
                .placement
                .as_ref()
                .map(|placement| placement.site_node_id.as_str()),
        }
    }

    fn matches_model(&self, declaration: &ModelDeploymentDeclaration) -> bool {
        self.owner == DeploymentOwner::Model
            && declaration.model_deployment_id == self.deployment_id
            && declaration.lifecycle_generation == self.lifecycle_generation
            && declaration.desired_state == ModelDeploymentDesiredState::Teardown as i32
            && declaration
                .placement
                .as_ref()
                .map(|placement| placement.site_node_id.as_str())
                == self.site_node_id
    }

    fn matches_application(&self, declaration: &ApplicationDeploymentDeclaration) -> bool {
        self.owner == DeploymentOwner::Application
            && declaration.application_deployment_id == self.deployment_id
            && declaration.lifecycle_generation == self.lifecycle_generation
            && declaration.desired_state == ApplicationDeploymentDesiredState::Teardown as i32
            && declaration
                .placement
                .as_ref()
                .map(|placement| placement.site_node_id.as_str())
                == self.site_node_id
    }
}

/// Owner finalization of node reports over accepted site memory while the site guard is held:
/// teardown removals and node-resolved launch identities.
///
/// Changes are staged until [`Self::make_durable`], which writes them in one whole-file
/// replacement. No removal happens by deployment identity alone.
pub struct DeploymentTeardownTransaction<'a> {
    repository: &'a SiteRepository,
    current: &'a mut SiteState,
    candidate: Option<Site>,
}

impl DeploymentTeardownTransaction<'_> {
    /// Returns the accepted model declaration, excluding staged removals.
    pub fn model_deployment(&self, deployment_id: &str) -> Option<&ModelDeploymentDeclaration> {
        self.accepted()
            .model_deployments
            .iter()
            .find(|deployment| deployment.model_deployment_id == deployment_id)
    }

    /// Returns the accepted application declaration, excluding staged removals.
    pub fn application_deployment(
        &self,
        deployment_id: &str,
    ) -> Option<&ApplicationDeploymentDeclaration> {
        self.accepted()
            .application_deployments
            .iter()
            .find(|deployment| deployment.application_deployment_id == deployment_id)
    }

    /// Stages removal of the owner's declaration exactly matching `teardown` and reports whether
    /// one matched. Any other declaration with the same identity is left untouched.
    pub fn remove_teardown(&mut self, teardown: &DeploymentTeardownIdentity<'_>) -> bool {
        let matched = match teardown.owner {
            DeploymentOwner::Model => self
                .model_deployment(teardown.deployment_id)
                .is_some_and(|deployment| teardown.matches_model(deployment)),
            DeploymentOwner::Application => self
                .application_deployment(teardown.deployment_id)
                .is_some_and(|deployment| teardown.matches_application(deployment)),
        };
        if !matched {
            return false;
        }
        let candidate = self
            .candidate
            .get_or_insert_with(|| self.current.site.clone());
        match teardown.owner {
            DeploymentOwner::Model => candidate
                .model_deployments
                .retain(|deployment| !teardown.matches_model(deployment)),
            DeploymentOwner::Application => candidate
                .application_deployments
                .retain(|deployment| !teardown.matches_application(deployment)),
        }
        true
    }

    /// Returns the declared `SiteNode`, for example to check the host a node reports.
    pub fn site_node(&self, node_id: &str) -> Option<&dotcore_proto::dotcore::site::v1::SiteNode> {
        self.accepted()
            .nodes
            .iter()
            .find(|node| node.node_id == node_id)
    }

    /// Stages an updated model declaration, such as one recording the engine launch identity its
    /// node resolved, in place of the accepted declaration with the same identity. Reports
    /// whether one was replaced; the caller validates the update.
    pub fn replace_model_deployment(&mut self, declaration: ModelDeploymentDeclaration) -> bool {
        if self
            .model_deployment(&declaration.model_deployment_id)
            .is_none()
        {
            return false;
        }
        let candidate = self
            .candidate
            .get_or_insert_with(|| self.current.site.clone());
        for existing in &mut candidate.model_deployments {
            if existing.model_deployment_id == declaration.model_deployment_id {
                *existing = declaration;
                return true;
            }
        }
        false
    }

    /// Stages an updated application declaration, such as one recording the launch identities its
    /// node resolved, in place of the accepted declaration with the same identity. Reports
    /// whether one was replaced; the caller validates the update.
    pub fn replace_application_deployment(
        &mut self,
        declaration: ApplicationDeploymentDeclaration,
    ) -> bool {
        if self
            .application_deployment(&declaration.application_deployment_id)
            .is_none()
        {
            return false;
        }
        let candidate = self
            .candidate
            .get_or_insert_with(|| self.current.site.clone());
        for existing in &mut candidate.application_deployments {
            if existing.application_deployment_id == declaration.application_deployment_id {
                *existing = declaration;
                return true;
            }
        }
        false
    }

    /// Makes accepted memory durable: writes staged removals, or rewrites a dirty site. `Ok`
    /// means every accepted declaration, including each staged removal, is on disk. After a
    /// failed write the staged removals remain accepted in memory and the site stays dirty; a
    /// validation failure changes nothing.
    pub fn make_durable(&mut self) -> Result<(), SiteRepositoryError> {
        let Some(candidate) = self.candidate.take() else {
            return self.repository.flush(self.current);
        };
        validate_site(&candidate, &self.repository.rules)?;
        let contents = yaml::to_string(&candidate).map_err(SiteRepositoryError::SerializeYaml)?;
        self.repository
            .persist_mutation(self.current, candidate, &contents)
    }

    fn accepted(&self) -> &Site {
        self.candidate.as_ref().unwrap_or(&self.current.site)
    }
}

/// Mutable route-manager section plus neutral site context needed for one atomic transaction.
pub struct RouteSiteTransaction<'a> {
    #[allow(
        dead_code,
        reason = "the neutral shared site transaction context is available to every owner even when a particular mutation does not consume its site identity"
    )]
    pub site_id: &'a str,
    pub deployments: &'a [ModelDeploymentDeclaration],
    pub applications:
        &'a [dotcore_proto::dotcore::application::v1::ApplicationDeploymentDeclaration],
    pub routes: &'a mut Vec<InferenceRoute>,
}

/// Distinguishes an owner-rule rejection from a shared site transaction failure.
#[derive(Debug, thiserror::Error)]
pub enum ScopedSiteMutationError<E: std::error::Error + 'static> {
    #[error(transparent)]
    Owner(E),

    #[error(transparent)]
    Repository(SiteRepositoryError),
}

/// Applies the route manager's teardown rule to one deployment. Shared routes block first;
/// applications bound to a route that would be removed block only when no shared route does.
fn unload_route_plan(
    site: &Site,
    deployment_id: &str,
) -> Result<RouteUnloadPlan, ModelUnloadAcceptanceError> {
    let routes = classify_unload(&site.inference_routes, deployment_id);
    if !routes.shared_route_names().is_empty() {
        return Err(ModelUnloadAcceptanceError::SharedRouteReferences(
            routes.shared_route_names().to_vec(),
        ));
    }

    let application_references = application_route_references(
        &site.application_deployments,
        &site.inference_routes,
        &routes,
        &BTreeSet::new(),
    );
    if !application_references.is_empty() {
        return Err(ModelUnloadAcceptanceError::ApplicationRouteReferences(
            application_references,
        ));
    }
    Ok(routes)
}

/// Lists the applications bound to a route the unload removes. Applications named in
/// `withdrawn_application_ids` leave the site in the same change and never block.
pub fn application_route_references(
    applications: &[ApplicationDeploymentDeclaration],
    routes: &[InferenceRoute],
    unload: &RouteUnloadPlan,
    withdrawn_application_ids: &BTreeSet<&str>,
) -> Vec<ApplicationRouteReference> {
    let mut references = applications
        .iter()
        .filter(|application| {
            !withdrawn_application_ids.contains(application.application_deployment_id.as_str())
        })
        .filter_map(|application| {
            let route_id = application.inference_route_id.as_deref()?;
            let route = routes
                .iter()
                .find(|route| route.inference_route_id == route_id && unload.removes(route))?;
            Some(ApplicationRouteReference {
                application_deployment_id: application.application_deployment_id.clone(),
                application_deployment_name: application.application_deployment_name.clone(),
                inference_route_id: route.inference_route_id.clone(),
                inference_route_name: route.name.clone(),
            })
        })
        .collect::<Vec<_>>();
    references.sort_by(|left, right| {
        left.application_deployment_id
            .cmp(&right.application_deployment_id)
    });
    references
}

fn lock<T>(mutex: &Mutex<T>) -> Result<MutexGuard<'_, T>, SiteRepositoryError> {
    mutex.lock().map_err(|_| SiteRepositoryError::Unavailable)
}

/// Test-only `site.yaml` writer that applies queued faults, then writes real files atomically.
#[cfg(any(test, feature = "test-support"))]
#[derive(Clone, Default)]
pub struct SiteWriteFaults {
    queued: Arc<Mutex<std::collections::VecDeque<SiteWriteFault>>>,
    attempts: Arc<std::sync::atomic::AtomicUsize>,
}

/// One injected `site.yaml` write outcome.
#[cfg(any(test, feature = "test-support"))]
#[derive(Clone, Copy, Debug)]
pub enum SiteWriteFault {
    /// The write fails before `site.yaml` changes.
    Storage,

    /// `site.yaml` is replaced, but the parent directory sync is reported unconfirmed.
    Uncertain,
}

#[cfg(any(test, feature = "test-support"))]
impl SiteWriteFaults {
    pub fn push(&self, fault: SiteWriteFault) {
        self.queued
            .lock()
            .expect("Site write faults")
            .push_back(fault);
    }

    /// Returns every attempted `site.yaml` write, successful or not.
    pub fn attempts(&self) -> usize {
        self.attempts.load(std::sync::atomic::Ordering::SeqCst)
    }
}

#[cfg(any(test, feature = "test-support"))]
impl SiteFileWriter for SiteWriteFaults {
    fn replace(
        &self,
        path: &Path,
        contents: &str,
    ) -> Result<(), super::persistence::SiteStorageError> {
        self.attempts
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        let fault = self.queued.lock().expect("Site write faults").pop_front();
        match fault {
            Some(SiteWriteFault::Storage) => Err(super::persistence::SiteStorageError::Storage(
                anyhow!("injected site.yaml write failure"),
            )),
            Some(SiteWriteFault::Uncertain) => {
                AtomicSiteFileWriter.replace(path, contents)?;
                Err(super::persistence::SiteStorageError::OutcomeUncertain(
                    anyhow!("injected site.yaml directory sync uncertainty"),
                ))
            }
            None => AtomicSiteFileWriter.replace(path, contents),
        }
    }
}

#[cfg(test)]
mod tests;
