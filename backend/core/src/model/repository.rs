//! The declaration port through which the inference controller creates,
//! updates, and removes model deployments, resolving each catalog selection
//! into a deployment declaration.
//!
//! Mutating operations read the shared site transaction, apply the
//! model-owned rules, and write the accepted declaration back to the site.
//! Read-only operations such as previews and resource estimates return a
//! result without changing site state.

use std::{
    collections::BTreeSet,
    sync::{Arc, RwLock},
};

use dotcore_catalog::{Catalog, CatalogModelId};
use dotcore_proto::dotcore::{
    catalog::v1::{ModelParallelShape, UseCase},
    deployment::v1::ExactPlacement,
    model::v1::{
        CreateUpstreamModelDeploymentRequest, EstimateLocalModelResourcesResponse,
        ListModelProfileCompatibilityResponse, ListModelRecommendationsResponse,
        LocalModelDeploymentConfiguration, ModelDeployment, ModelDeploymentDeclaration,
        ModelDeploymentDesiredState, ModelDeploymentLifecycleTransition, ModelRecommendationFit,
        RecommendedModelProfile, RecommendedProfileTarget, UpstreamModelDesiredConfiguration,
        UseCaseModelRecommendations,
        model_deployment_snapshot::Source as ModelDeploymentSnapshotSource,
    },
};
use dotcore_site::{
    node::target_inventory::TargetCapabilityInventory,
    repository::{
        DeploymentTeardownIdentity, ModelSiteSnapshot, ModelUnloadAcceptanceError,
        ScopedSiteMutationError, SiteRepository, SiteRepositoryError,
    },
    store::FileCurrentState,
};

use super::{
    ModelTeardownApplicationRouteBlocker,
    controller::ModelDeploymentExposureIntent,
    recommendation::{RecommendationFit, UseCaseRecommendations, recommend_models},
    sizing_scenario::SizingScenario,
};
use crate::route_manager::{RouteDeclarationRepository, RouteRepositoryError};

#[derive(Clone)]
pub struct ModelDeclarationRepository {
    site: SiteRepository,
    catalog: Arc<Catalog>,
    routes: RouteDeclarationRepository,
    target_capabilities: Arc<RwLock<TargetCapabilityInventory>>,
    lifecycle: Option<FileCurrentState>,
}

impl ModelDeclarationRepository {
    pub(crate) fn new(
        site: SiteRepository,
        catalog: Arc<Catalog>,
        target_capabilities: TargetCapabilityInventory,
    ) -> Self {
        Self {
            routes: RouteDeclarationRepository::new(site.clone()),
            site,
            catalog,
            target_capabilities: Arc::new(RwLock::new(target_capabilities)),
            lifecycle: None,
        }
    }

    pub(crate) fn new_with_lifecycle(
        site: SiteRepository,
        catalog: Arc<Catalog>,
        target_capabilities: TargetCapabilityInventory,
        lifecycle: FileCurrentState,
    ) -> Self {
        let mut repository = Self::new(site, catalog, target_capabilities);
        repository.lifecycle = Some(lifecycle);
        repository
    }
    pub(crate) fn replace_target_capabilities(&self, inventory: TargetCapabilityInventory) {
        *self
            .target_capabilities
            .write()
            .unwrap_or_else(|value| value.into_inner()) = inventory;
    }

    pub(crate) fn site_repository(&self) -> &SiteRepository {
        &self.site
    }

    #[cfg(test)]
    pub(crate) fn composition_pointers(&self) -> (*const Catalog, usize) {
        (Arc::as_ptr(&self.catalog), self.site.state_pointer())
    }

    pub(crate) fn snapshot(&self) -> Result<ModelSiteSnapshot, SiteRepositoryError> {
        self.site.model_snapshot()
    }

    /// Evaluates advisory resources and derived engine parameter values for a local selection
    /// without mutating site state, per hardware class. On a target, its facts in `inventory` pick
    /// the one class, and the response previews the configuration on the target's memory beside
    /// what `workloads` hold there; without one, every class the profile's configuration supports
    /// is evaluated. Custom arguments hide estimates and the preview's demand, since they may
    /// change the footprint, but derived values still apply to the launch.
    pub(crate) fn estimate_local_model_resources(
        &self,
        model_id: &str,
        profile_id: &str,
        configuration: &LocalModelDeploymentConfiguration,
        target: Option<EstimateTarget<'_>>,
    ) -> Result<EstimateLocalModelResourcesResponse, ModelDeclarationRepositoryError> {
        let catalog_model_id = CatalogModelId(model_id.to_owned());
        validate_model_profile(
            &catalog_model_id,
            profile_id,
            self.catalog.profiles_for_model(&catalog_model_id),
        )?;
        let configuration = crate::model::resolution::apply_profile_defaults(
            &self.catalog,
            &catalog_model_id,
            profile_id,
            configuration,
        )
        .map_err(launch_resolution_error)?;
        let estimates_apply = configuration.custom_argument_tokens.is_empty();
        let Some(target) = target else {
            let resolved = crate::model::resolution::estimate_local_values_per_class(
                &self.catalog,
                &catalog_model_id,
                profile_id,
                &configuration,
            )
            .map_err(launch_resolution_error)?;
            return Ok(EstimateLocalModelResourcesResponse {
                hardware_class_estimates: resolved
                    .into_iter()
                    .map(|resolved| resolved.into_hardware_class_estimate(estimates_apply))
                    .collect(),
                placement_preview: None,
            });
        };

        let site = self.site.model_snapshot()?;
        let execution_target = placement_target(&site, target.placement)?;
        let mut selected = crate::hardware::selection::select_target_capabilities(
            execution_target,
            target.inventory,
        )
        .map_err(|error| ModelDeclarationRepositoryError::TargetCapability(Box::new(error)))?;
        if let Some(workloads) = target.workloads.get(&execution_target.site_node_id) {
            selected.capacity = std::mem::take(&mut selected.capacity).with_workloads(workloads);
        }
        let resolved = crate::model::resolution::estimate_local_values_on_target(
            &self.catalog,
            &catalog_model_id,
            profile_id,
            &configuration,
            &selected,
        )
        .map_err(launch_resolution_error)?;
        let observed_at = target
            .inventory
            .for_target(&execution_target.execution_target_id)
            .and_then(|capabilities| capabilities.accelerator_observed_at);
        let placement_preview = crate::model::placement_preview::placement_preview(
            &resolved,
            &selected,
            estimates_apply,
            observed_at,
        );
        Ok(EstimateLocalModelResourcesResponse {
            hardware_class_estimates: vec![resolved.into_hardware_class_estimate(estimates_apply)],
            placement_preview: Some(placement_preview),
        })
    }

    pub(crate) fn load_model(
        &self,
        model_id: &str,
        profile_id: &str,
        placement: &ExactPlacement,
        configuration: &LocalModelDeploymentConfiguration,
        exposure: &ModelDeploymentExposureIntent,
        name: Option<&str>,
    ) -> Result<ModelDeploymentDeclaration, ModelDeclarationRepositoryError> {
        let deployment_id = self.site.reserve_model_deployment_id()?;
        let snapshot = self.site.model_snapshot()?;
        let mut deployment = self.resolve_model_deployment(
            &snapshot,
            model_id,
            profile_id,
            placement,
            configuration,
        )?;
        deployment.model_deployment_id = deployment_id;
        deployment.lifecycle_generation =
            self.reserve_initial_generation(&deployment.model_deployment_id)?;
        deployment.lifecycle_transition = ModelDeploymentLifecycleTransition::Create as i32;
        let default_name = model_display_name(&deployment);
        deployment.model_deployment_name =
            dotcore_domain::deployment::name::normalize_or_default(name, &default_name)
                .map_err(ModelDeclarationRepositoryError::InvalidName)?;
        let route = self.prepare_exposure(exposure, &deployment)?;
        self.site
            .accept_model_deployment_exposure(
                deployment.clone(),
                route.map(crate::route_manager::PreparedExposureRoute::into_declaration),
            )
            .map_err(composite_error)?;
        Ok(deployment)
    }

    /// Resolves a local deployment without changing desired site state.
    pub(crate) fn preview_local_model(
        &self,
        model_id: &str,
        profile_id: &str,
        placement: &ExactPlacement,
        configuration: &LocalModelDeploymentConfiguration,
    ) -> Result<
        dotcore_proto::dotcore::model::v1::ResolvedLocalModelDeploymentSnapshot,
        ModelDeclarationRepositoryError,
    > {
        let site = self.site.model_snapshot()?;
        let deployment =
            self.resolve_model_deployment(&site, model_id, profile_id, placement, configuration)?;
        match deployment.snapshot.and_then(|snapshot| snapshot.source) {
            Some(ModelDeploymentSnapshotSource::Local(local)) => Ok(local),
            Some(ModelDeploymentSnapshotSource::Upstream(_)) | None => {
                Err(ModelDeclarationRepositoryError::InvalidState(
                    "local preview resolved a non-local snapshot",
                ))
            }
        }
    }

    /// Evaluates which catalog model profiles can run on which configured model execution targets
    /// against `inventory` and what the deployments of each node hold of its memory, narrowed to one model and one
    /// target when given, without reading or mutating the retained target capabilities, and sizes
    /// each profile for `scenario`. A missing inventory makes every target unknown.
    pub(crate) fn profile_compatibility(
        &self,
        model_id: Option<&str>,
        target: Option<&ExactPlacement>,
        scenario: Option<&SizingScenario>,
        inventory: Option<&TargetCapabilityInventory>,
        workloads: &crate::hardware::capacity::NodeWorkloads,
    ) -> Result<ListModelProfileCompatibilityResponse, ModelDeclarationRepositoryError> {
        let site = self.site.model_snapshot()?;
        let models = self
            .catalog
            .list_model_releases()
            .into_iter()
            .filter(|model| model_id.is_none_or(|model_id| model.model_id == model_id))
            .collect::<Vec<_>>();
        if let Some(model_id) = model_id
            && models.is_empty()
        {
            return Err(ModelDeclarationRepositoryError::ModelNotFound(
                model_id.to_owned(),
            ));
        }
        let targets = match target {
            Some(placement) => {
                let target = placement_target(&site, placement)?;
                if !is_model_target(target) {
                    return Err(ModelDeclarationRepositoryError::NotModelExecutionTarget(
                        target.execution_target_id.clone(),
                    ));
                }
                vec![target.clone()]
            }
            None => site
                .execution_targets
                .into_iter()
                .filter(is_model_target)
                .collect(),
        };

        Ok(crate::model::compatibility::evaluate_profile_compatibility(
            &self.catalog,
            &models,
            &targets,
            scenario,
            inventory,
            workloads,
        ))
    }

    /// Ranks the catalog's recommended model profiles for each of `use_cases` on the site's
    /// configured model execution targets against `inventory` and the site's `deployments`,
    /// without reading or mutating the retained target capabilities. A missing inventory
    /// recommends nothing.
    pub(crate) fn model_recommendations(
        &self,
        use_cases: &[UseCase],
        inventory: Option<&TargetCapabilityInventory>,
        deployments: &[ModelDeployment],
    ) -> Result<ListModelRecommendationsResponse, ModelDeclarationRepositoryError> {
        let targets = self
            .site
            .model_snapshot()?
            .execution_targets
            .into_iter()
            .filter(is_model_target)
            .collect::<Vec<_>>();
        let recommendations =
            recommend_models(&self.catalog, &targets, inventory, deployments, use_cases);

        Ok(recommendations_response(recommendations))
    }

    pub(crate) fn load_upstream_model(
        &self,
        request: &CreateUpstreamModelDeploymentRequest,
        exposure: &ModelDeploymentExposureIntent,
        name: Option<&str>,
    ) -> Result<ModelDeploymentDeclaration, ModelDeclarationRepositoryError> {
        let snapshot = crate::model::resolution::resolve_upstream_snapshot(
            &self.catalog,
            &request.upstream_model_template_id,
        )
        .ok_or_else(|| {
            ModelDeclarationRepositoryError::UpstreamModelTemplateNotFound(
                request.upstream_model_template_id.clone(),
            )
        })?;
        let deployment_id = self.site.reserve_model_deployment_id()?;
        let lifecycle_generation = self.reserve_initial_generation(&deployment_id)?;
        let deployment = ModelDeploymentDeclaration {
            model_deployment_id: deployment_id,
            model_deployment_name: dotcore_domain::deployment::name::normalize_or_default(
                name,
                "Upstream model",
            )
            .map_err(ModelDeclarationRepositoryError::InvalidName)?,
            placement: None,
            requested_parallel_shape: ModelParallelShape::Unspecified as i32,
            snapshot: Some(snapshot),
            desired_state: ModelDeploymentDesiredState::Present as i32,
            upstream_desired_configuration: request.desired_configuration.clone(),
            lifecycle_generation,
            lifecycle_transition: ModelDeploymentLifecycleTransition::Create as i32,
            lifecycle_recovery_required: false,
            node_resolved_launch_identities: Vec::new(),
        };
        let route = self.prepare_exposure(exposure, &deployment)?;
        self.site
            .accept_model_deployment_exposure(
                deployment.clone(),
                route.map(crate::route_manager::PreparedExposureRoute::into_declaration),
            )
            .map_err(composite_error)?;
        Ok(deployment)
    }

    pub(crate) fn update_upstream_model(
        &self,
        deployment_id: &str,
        replacement: &UpstreamModelDesiredConfiguration,
    ) -> Result<ModelDeploymentDeclaration, ModelDeclarationRepositoryError> {
        self.site
            .mutate_model_deployments(|transaction| {
                let deployment = transaction
                    .deployments
                    .iter_mut()
                    .find(|deployment| deployment.model_deployment_id == deployment_id)
                    .ok_or_else(|| {
                        ModelDeclarationRepositoryError::DeploymentNotFound(
                            deployment_id.to_owned(),
                        )
                    })?;
                if !matches!(
                    deployment
                        .snapshot
                        .as_ref()
                        .and_then(|snapshot| snapshot.source.as_ref()),
                    Some(ModelDeploymentSnapshotSource::Upstream(_))
                ) {
                    return Err(ModelDeclarationRepositoryError::NotUpstreamDeployment(
                        deployment_id.to_owned(),
                    ));
                }
                let current = deployment.upstream_desired_configuration.as_ref().ok_or(
                    ModelDeclarationRepositoryError::InvalidState(
                        "Upstream deployment has no private configuration",
                    ),
                )?;
                let mut replacement = replacement.clone();
                replacement.api_key =
                    retain_existing_api_key(&current.api_key, &replacement.api_key).to_owned();
                deployment.upstream_desired_configuration = Some(replacement);
                Ok(deployment.clone())
            })
            .map_err(scoped_error)
    }

    pub(crate) fn update_metadata(
        &self,
        deployment_id: &str,
        patch: &dotcore_domain::deployment::metadata::DeploymentMetadataPatch,
    ) -> Result<ModelDeploymentDeclaration, ModelDeclarationRepositoryError> {
        self.site
            .mutate_model_deployment_metadata(|transaction| {
                let deployment = transaction
                    .deployments
                    .iter_mut()
                    .find(|deployment| deployment.model_deployment_id == deployment_id)
                    .ok_or_else(|| {
                        ModelDeclarationRepositoryError::DeploymentNotFound(
                            deployment_id.to_owned(),
                        )
                    })?;
                if deployment.desired_state == ModelDeploymentDesiredState::Teardown as i32 {
                    return Err(ModelDeclarationRepositoryError::InvalidTransition {
                        deployment_id: deployment_id.to_owned(),
                        operation: "update metadata",
                    });
                }
                deployment.model_deployment_name = patch.display_name().to_owned();
                Ok(deployment.clone())
            })
            .map_err(scoped_error)
    }

    /// Accepts teardown intent for one deployment. A deployment already in teardown is returned
    /// unchanged, so repeated unloads never allocate a generation or rewrite the site. Route and
    /// application blockers are rejected before a generation is reserved.
    pub(crate) fn unload_model(
        &self,
        deployment_id: &str,
    ) -> Result<ModelDeploymentDeclaration, ModelDeclarationRepositoryError> {
        let current = self.declared_deployment(deployment_id)?;
        if current.desired_state == ModelDeploymentDesiredState::Teardown as i32 {
            return Ok(current);
        }
        self.site
            .check_model_deployment_unload(deployment_id)
            .map_err(unload_acceptance_error)?;
        let generation = self.reserve_next_generation(&current)?;
        self.site
            .accept_model_deployment_unload(deployment_id, generation)
            .map_err(unload_acceptance_error)
    }

    pub(crate) fn pause_model(
        &self,
        deployment_id: &str,
    ) -> Result<ModelDeploymentDeclaration, ModelDeclarationRepositoryError> {
        self.transition_model(deployment_id, ModelDeploymentDesiredState::Paused, "pause")
    }

    pub(crate) fn resume_model(
        &self,
        deployment_id: &str,
    ) -> Result<ModelDeploymentDeclaration, ModelDeclarationRepositoryError> {
        self.transition_model(
            deployment_id,
            ModelDeploymentDesiredState::Present,
            "resume",
        )
    }

    /// Removes a deployment whose teardown is finalized, but only while its declaration still
    /// matches the exact teardown identity, generation, and placement node; the removal is then
    /// made durable and only then is its lifecycle record pruned. An already-removed deployment
    /// only has any earlier failed site write flushed, and a declaration that no longer matches
    /// is left untouched.
    pub(crate) fn finalize_teardown(
        &self,
        declaration: &ModelDeploymentDeclaration,
    ) -> Result<(), ModelDeclarationRepositoryError> {
        let teardown = DeploymentTeardownIdentity::of_model(declaration);
        self.site
            .with_deployment_teardowns(|teardowns| {
                if !teardowns.remove_teardown(&teardown)
                    && teardowns.model_deployment(teardown.deployment_id).is_some()
                {
                    return Err(ModelDeclarationRepositoryError::TeardownMismatch(
                        teardown.deployment_id.to_owned(),
                    ));
                }
                teardowns
                    .make_durable()
                    .map_err(ModelDeclarationRepositoryError::Site)?;
                self.lifecycle.as_ref().map_or(Ok(()), |lifecycle| {
                    lifecycle
                        .prune_lifecycle_records(
                            dotcore_domain::deployment::DeploymentOwner::Model,
                            &BTreeSet::from([teardown.deployment_id]),
                        )
                        .map_err(|error| {
                            ModelDeclarationRepositoryError::LifecycleState(error.into())
                        })
                })
            })
            .map_err(ModelDeclarationRepositoryError::Site)?
    }

    fn reserve_initial_generation(
        &self,
        deployment_id: &str,
    ) -> Result<u64, ModelDeclarationRepositoryError> {
        self.lifecycle
            .as_ref()
            .map(|lifecycle| {
                lifecycle
                    .reserve_lifecycle_generation(
                        dotcore_domain::deployment::DeploymentOwner::Model,
                        deployment_id,
                    )
                    .map(|record| record.generation_high_water)
                    .map_err(|error| ModelDeclarationRepositoryError::LifecycleState(error.into()))
            })
            .unwrap_or(Ok(1))
    }

    fn transition_model(
        &self,
        deployment_id: &str,
        target: ModelDeploymentDesiredState,
        operation: &'static str,
    ) -> Result<ModelDeploymentDeclaration, ModelDeclarationRepositoryError> {
        let current = self.declared_deployment(deployment_id)?;
        if current.desired_state == target as i32 {
            return Ok(current);
        }
        let generation = self.reserve_next_generation(&current)?;
        self.site
            .mutate_model_deployments(|transaction| {
                transition_desired_state(
                    transaction.deployments,
                    deployment_id,
                    target,
                    operation,
                    generation,
                )
            })
            .map_err(scoped_error)
    }

    fn declared_deployment(
        &self,
        deployment_id: &str,
    ) -> Result<ModelDeploymentDeclaration, ModelDeclarationRepositoryError> {
        self.snapshot()?
            .deployments
            .into_iter()
            .find(|deployment| deployment.model_deployment_id == deployment_id)
            .ok_or_else(|| {
                ModelDeclarationRepositoryError::DeploymentNotFound(deployment_id.to_owned())
            })
    }

    /// Reserves the next lifecycle generation from the durable high-water, which stays above
    /// every generation ever reserved even when its site write later failed.
    fn reserve_next_generation(
        &self,
        current: &ModelDeploymentDeclaration,
    ) -> Result<u64, ModelDeclarationRepositoryError> {
        self.lifecycle.as_ref().map_or_else(
            || {
                current.lifecycle_generation.checked_add(1).ok_or(
                    ModelDeclarationRepositoryError::InvalidState(
                        "deployment lifecycle generation is exhausted",
                    ),
                )
            },
            |lifecycle| {
                lifecycle
                    .reserve_lifecycle_generation(
                        dotcore_domain::deployment::DeploymentOwner::Model,
                        &current.model_deployment_id,
                    )
                    .map(|record| record.generation_high_water)
                    .map_err(|error| ModelDeclarationRepositoryError::LifecycleState(error.into()))
            },
        )
    }

    fn prepare_exposure(
        &self,
        exposure: &ModelDeploymentExposureIntent,
        deployment: &ModelDeploymentDeclaration,
    ) -> Result<Option<crate::route_manager::PreparedExposureRoute>, ModelDeclarationRepositoryError>
    {
        match exposure {
            ModelDeploymentExposureIntent::Direct { route_name } => self
                .routes
                .prepare_exposure_route(route_name.clone(), deployment)
                .map(Some)
                .map_err(ModelDeclarationRepositoryError::ExposureRoute),
            ModelDeploymentExposureIntent::Hidden => Ok(None),
        }
    }

    fn resolve_model_deployment(
        &self,
        site: &ModelSiteSnapshot,
        model_id: &str,
        profile_id: &str,
        placement: &ExactPlacement,
        configuration: &LocalModelDeploymentConfiguration,
    ) -> Result<ModelDeploymentDeclaration, ModelDeclarationRepositoryError> {
        let catalog_model_id = CatalogModelId(model_id.to_owned());
        validate_model_profile(
            &catalog_model_id,
            profile_id,
            self.catalog.profiles_for_model(&catalog_model_id),
        )?;
        let target = placement_target(site, placement)?;
        let configuration = crate::model::resolution::apply_profile_defaults(
            &self.catalog,
            &catalog_model_id,
            profile_id,
            configuration,
        )
        .map_err(launch_resolution_error)?;
        let local = crate::model::resolution::resolve_local_launch(
            &self.catalog,
            &catalog_model_id,
            profile_id,
            &configuration,
            target,
            &self
                .target_capabilities
                .read()
                .unwrap_or_else(|value| value.into_inner()),
            // The SiteNode named by the exact placement pins a local-development engine itself.
            None,
        )
        .map_err(launch_resolution_error)?;
        if local.model_id != model_id || local.profile_id != profile_id {
            return Err(ModelDeclarationRepositoryError::InvalidState(
                "Catalog snapshot does not match the selected model profile",
            ));
        }
        let snapshot = dotcore_proto::dotcore::model::v1::ModelDeploymentSnapshot {
            source: Some(ModelDeploymentSnapshotSource::Local(local)),
        };
        Ok(ModelDeploymentDeclaration {
            model_deployment_id: next_deployment_id(&site.deployments),
            placement: Some(placement.clone()),
            model_deployment_name: String::new(),
            requested_parallel_shape: ModelParallelShape::One as i32,
            snapshot: Some(snapshot),
            desired_state: ModelDeploymentDesiredState::Present as i32,
            upstream_desired_configuration: None,
            lifecycle_generation: 0,
            lifecycle_transition: ModelDeploymentLifecycleTransition::Unspecified as i32,
            lifecycle_recovery_required: false,
            node_resolved_launch_identities: Vec::new(),
        })
    }
}

/// The target a resource estimate is evaluated on: its placement, the target observations that
/// select its hardware class and build its memory pools, and what the site's deployments hold of
/// each node's memory.
#[derive(Clone, Copy)]
pub(crate) struct EstimateTarget<'a> {
    pub(crate) placement: &'a ExactPlacement,
    pub(crate) inventory: &'a TargetCapabilityInventory,
    pub(crate) workloads: &'a crate::hardware::capacity::NodeWorkloads,
}

/// Returns the execution target named by an exact placement when its SiteNode owns it.
fn placement_target<'a>(
    site: &'a ModelSiteSnapshot,
    placement: &ExactPlacement,
) -> Result<&'a dotcore_proto::dotcore::site::v1::ExecutionTarget, ModelDeclarationRepositoryError>
{
    site.execution_targets
        .iter()
        .find(|target| target.execution_target_id == placement.execution_target_id)
        .filter(|target| target.site_node_id == placement.site_node_id)
        .ok_or_else(|| {
            ModelDeclarationRepositoryError::ExecutionTargetNotFound(
                placement.execution_target_id.clone(),
            )
        })
}

/// The wire form of ranked recommendations, in the order the recommender ranked them.
fn recommendations_response(
    recommendations: Vec<UseCaseRecommendations>,
) -> ListModelRecommendationsResponse {
    ListModelRecommendationsResponse {
        use_cases: recommendations
            .into_iter()
            .map(|recommendations| UseCaseModelRecommendations {
                use_case: recommendations.use_case.into(),
                rows: recommendations
                    .rows
                    .into_iter()
                    .map(|row| RecommendedModelProfile {
                        model_id: row.model_id,
                        profile_id: row.profile_id,
                        targets: row
                            .targets
                            .into_iter()
                            .map(|target| RecommendedProfileTarget {
                                target: Some(target.placement),
                                fit: match target.fit {
                                    RecommendationFit::FitsNow => ModelRecommendationFit::FitsNow,
                                    RecommendationFit::NeedsMemoryFreed => {
                                        ModelRecommendationFit::NeedsMemoryFreed
                                    }
                                    RecommendationFit::Unknown => ModelRecommendationFit::Unknown,
                                    RecommendationFit::Unreachable => {
                                        ModelRecommendationFit::Unreachable
                                    }
                                }
                                .into(),
                                deployed: target.deployed,
                                starting_total_context_tokens: target.starting_total_context_tokens,
                            })
                            .collect(),
                    })
                    .collect(),
            })
            .collect(),
    }
}

fn is_model_target(target: &dotcore_proto::dotcore::site::v1::ExecutionTarget) -> bool {
    matches!(
        target.runtime_binding,
        Some(dotcore_proto::dotcore::site::v1::execution_target::RuntimeBinding::ModelRuntime(_))
    )
}

fn scoped_error(
    error: ScopedSiteMutationError<ModelDeclarationRepositoryError>,
) -> ModelDeclarationRepositoryError {
    match error {
        ScopedSiteMutationError::Owner(error) => error,
        ScopedSiteMutationError::Repository(error) => ModelDeclarationRepositoryError::Site(error),
    }
}

fn composite_error(error: SiteRepositoryError) -> ModelDeclarationRepositoryError {
    match error {
        SiteRepositoryError::InvalidSite(error) => {
            ModelDeclarationRepositoryError::ExposureConflict(error)
        }
        error => ModelDeclarationRepositoryError::Site(error),
    }
}

fn unload_acceptance_error(error: ModelUnloadAcceptanceError) -> ModelDeclarationRepositoryError {
    match error {
        ModelUnloadAcceptanceError::Site(error) => ModelDeclarationRepositoryError::Site(error),
        ModelUnloadAcceptanceError::DeploymentNotFound(id) => {
            ModelDeclarationRepositoryError::DeploymentNotFound(id)
        }
        ModelUnloadAcceptanceError::SharedRouteReferences(names) => {
            ModelDeclarationRepositoryError::SharedRouteReferences(names)
        }
        ModelUnloadAcceptanceError::ApplicationRouteReferences(references) => {
            ModelDeclarationRepositoryError::ApplicationRouteReferences(references)
        }
    }
}

pub(crate) fn launch_resolution_error(
    error: crate::model::resolution::ModelLaunchResolutionError,
) -> ModelDeclarationRepositoryError {
    match error {
        crate::model::resolution::ModelLaunchResolutionError::TargetCapability(error) => {
            ModelDeclarationRepositoryError::TargetCapability(Box::new(error))
        }
        crate::model::resolution::ModelLaunchResolutionError::EngineArtifactSelection(error) => {
            ModelDeclarationRepositoryError::EngineArtifactSelection(Box::new(error))
        }
        error @ (crate::model::resolution::ModelLaunchResolutionError::AcceleratorDeviceCountMismatch {
            ..
        }
        | crate::model::resolution::ModelLaunchResolutionError::HardwareClassSelection {
            ..
        }) => {
            ModelDeclarationRepositoryError::TargetIncompatible(Box::new(error))
        }
        crate::model::resolution::ModelLaunchResolutionError::CatalogResolution(error) => {
            ModelDeclarationRepositoryError::CatalogResolution(error)
        }
        crate::model::resolution::ModelLaunchResolutionError::InvalidConfiguration(message) => {
            ModelDeclarationRepositoryError::InvalidState(message)
        }
        error @ (crate::model::resolution::ModelLaunchResolutionError::ProfileDefaultsUnavailable {
            ..
        }
        | crate::model::resolution::ModelLaunchResolutionError::InvalidParameterValues(_)
        | crate::model::resolution::ModelLaunchResolutionError::InvalidOptionValues(_)
        | crate::model::resolution::ModelLaunchResolutionError::ContextAboveMaximum { .. }
        | crate::model::resolution::ModelLaunchResolutionError::DerivedParameterOutOfRange {
            ..
        }) => ModelDeclarationRepositoryError::ConfigurationRejected(Box::new(error)),
        crate::model::resolution::ModelLaunchResolutionError::EngineRealization(error) => {
            ModelDeclarationRepositoryError::EngineRealization(error)
        }
        crate::model::resolution::ModelLaunchResolutionError::LaunchPlan(error) => {
            ModelDeclarationRepositoryError::LaunchPlan(error)
        }
        error @ (crate::model::resolution::ModelLaunchResolutionError::EngineIdentityUnexpected {
            ..
        }
        | crate::model::resolution::ModelLaunchResolutionError::EngineImageIdentityMismatch {
            ..
        }
) => ModelDeclarationRepositoryError::EngineLaunchIdentity(Box::new(error)),
        error @ crate::model::resolution::ModelLaunchResolutionError::SizingUnavailable {
            ..
        } => ModelDeclarationRepositoryError::Sizing(Box::new(error)),
    }
}

fn validate_model_profile(
    model_id: &CatalogModelId,
    profile_id: &str,
    profiles: Vec<dotcore_proto::dotcore::catalog::v1::ModelProfile>,
) -> Result<(), ModelDeclarationRepositoryError> {
    if profiles.is_empty() {
        return Err(ModelDeclarationRepositoryError::ModelNotFound(
            model_id.0.clone(),
        ));
    }
    if profiles
        .iter()
        .any(|profile| profile.profile_id == profile_id)
    {
        Ok(())
    } else {
        Err(ModelDeclarationRepositoryError::ModelProfileNotFound {
            model_id: model_id.0.clone(),
            profile_id: profile_id.to_owned(),
        })
    }
}

fn retain_existing_api_key<'a>(existing: &'a str, replacement: &'a str) -> &'a str {
    if replacement.trim().is_empty() {
        existing
    } else {
        replacement
    }
}

fn transition_desired_state(
    deployments: &mut [ModelDeploymentDeclaration],
    deployment_id: &str,
    target: ModelDeploymentDesiredState,
    operation: &'static str,
    reserved_generation: u64,
) -> Result<ModelDeploymentDeclaration, ModelDeclarationRepositoryError> {
    let deployment = deployments
        .iter_mut()
        .find(|deployment| deployment.model_deployment_id == deployment_id)
        .ok_or_else(|| {
            ModelDeclarationRepositoryError::DeploymentNotFound(deployment_id.to_owned())
        })?;
    let current =
        ModelDeploymentDesiredState::try_from(deployment.desired_state).map_err(|_| {
            ModelDeclarationRepositoryError::InvalidState("deployment has an invalid desired state")
        })?;
    match (operation, current, target) {
        ("pause", ModelDeploymentDesiredState::Present, ModelDeploymentDesiredState::Paused)
        | ("resume", ModelDeploymentDesiredState::Paused, ModelDeploymentDesiredState::Present) => {
            if reserved_generation <= deployment.lifecycle_generation {
                return Err(ModelDeclarationRepositoryError::InvalidState(
                    "reserved lifecycle generation does not advance accepted intent",
                ));
            }
            deployment.lifecycle_generation = reserved_generation;
            deployment.lifecycle_transition = match operation {
                "pause" => ModelDeploymentLifecycleTransition::Pause as i32,
                "resume" => ModelDeploymentLifecycleTransition::Resume as i32,
                _ => unreachable!("validated lifecycle operation"),
            };
            deployment.lifecycle_recovery_required = false;
            deployment.desired_state = target as i32;
        }
        (_, current, target) if current == target => {}
        _ => {
            return Err(ModelDeclarationRepositoryError::InvalidTransition {
                deployment_id: deployment_id.to_owned(),
                operation,
            });
        }
    }
    Ok(deployment.clone())
}

/// Model-owned declaration rule failures over the shared site transaction boundary.
#[derive(Debug, thiserror::Error)]
pub enum ModelDeclarationRepositoryError {
    #[error(transparent)]
    Site(#[from] SiteRepositoryError),

    #[error("target capability selection failed: {0}")]
    TargetCapability(#[source] Box<crate::hardware::selection::TargetCapabilitySelectionError>),

    #[error("approved engine artifact selection failed: {0}")]
    EngineArtifactSelection(#[source] Box<crate::model::selection::EngineArtifactSelectionError>),

    #[error(transparent)]
    TargetIncompatible(Box<crate::model::resolution::ModelLaunchResolutionError>),

    /// The local deployment configuration breaks a launch rule the operator can fix by changing
    /// the request, such as an engine parameter value above what the configuration allows.
    #[error(transparent)]
    ConfigurationRejected(Box<crate::model::resolution::ModelLaunchResolutionError>),

    #[error("could not resolve catalog model: {0}")]
    CatalogResolution(#[source] dotcore_catalog::CatalogError),

    #[error("could not realize local engine state: {0}")]
    EngineRealization(#[source] anyhow::Error),

    #[error("Catalog returned an invalid model snapshot: {0}")]
    CatalogSnapshotValidation(#[source] dotcore_catalog::CatalogError),

    #[error("could not translate local model launch plan: {0}")]
    LaunchPlan(#[source] anyhow::Error),

    #[error("could not size local model launch: {0}")]
    Sizing(#[source] Box<crate::model::resolution::ModelLaunchResolutionError>),

    #[error("could not pin the engine launch identity: {0}")]
    EngineLaunchIdentity(#[source] Box<crate::model::resolution::ModelLaunchResolutionError>),

    #[error("Catalog model state is invalid: {0}")]
    InvalidState(&'static str),

    #[error("Catalog model {0:?} was not found")]
    ModelNotFound(String),

    #[error("Catalog model {model_id:?} has no profile {profile_id:?}")]
    ModelProfileNotFound {
        model_id: String,
        profile_id: String,
    },

    #[error("site execution target {0:?} was not found")]
    ExecutionTargetNotFound(String),

    #[error("site execution target {0:?} is not a model execution target")]
    NotModelExecutionTarget(String),

    #[error("Catalog upstream model template {0:?} was not found")]
    UpstreamModelTemplateNotFound(String),

    #[error("Model deployment exposure conflicts with current site declarations: {0}")]
    ExposureConflict(#[source] anyhow::Error),

    #[error("Inference routes shared with other deployments reference the model deployment: {0:?}")]
    SharedRouteReferences(Vec<String>),

    #[error(
        "Application deployments use inference routes that would be removed with the model deployment: {0:?}"
    )]
    ApplicationRouteReferences(Vec<ModelTeardownApplicationRouteBlocker>),

    #[error("Direct-exposure route preparation failed: {0}")]
    ExposureRoute(#[source] RouteRepositoryError),

    #[error("deployment name is invalid: {0}")]
    InvalidName(#[source] dotcore_domain::deployment::name::DeploymentNameError),

    #[error("desired deployment {0:?} was not found")]
    DeploymentNotFound(String),

    #[error("teardown finalization does not match declaration {0:?}")]
    TeardownMismatch(String),

    #[error("lifecycle generation state failed: {0}")]
    LifecycleState(#[source] crate::model::state::CurrentStateError),

    #[error("desired deployment {0:?} is not an upstream model")]
    NotUpstreamDeployment(String),

    #[error("cannot {operation} model deployment {deployment_id:?} from its current desired state")]
    InvalidTransition {
        deployment_id: String,
        operation: &'static str,
    },
}

fn model_display_name(deployment: &ModelDeploymentDeclaration) -> String {
    deployment
        .snapshot
        .as_ref()
        .and_then(|snapshot| match snapshot.source.as_ref()? {
            ModelDeploymentSnapshotSource::Local(local) => Some(local.model_display_name.clone()),
            ModelDeploymentSnapshotSource::Upstream(_) => None,
        })
        .expect("local model deployment has a catalog display name")
}

fn next_deployment_id(deployments: &[ModelDeploymentDeclaration]) -> String {
    for sequence in 1.. {
        let candidate = format!("deployment-{sequence}");
        if deployments
            .iter()
            .all(|deployment| deployment.model_deployment_id != candidate)
        {
            return candidate;
        }
    }
    unreachable!("unbounded deployment ID sequence must find an unused ID")
}

#[cfg(test)]
mod tests {
    use std::{fs, path::Path, sync::Arc};

    use dotcore_catalog::{Catalog, CatalogModelId, OciArtifactPolicy};
    use dotcore_proto::dotcore::{
        application::v1::{
            ApplicationDeploymentDeclaration, ApplicationDeploymentDesiredState,
            ResolvedApplicationDeploymentSnapshot,
        },
        catalog::v1::{
            EngineParameterValue, EngineParameterValues, OciPlatform, UpstreamModelBoundary,
            UpstreamModelDataEgressClass, UpstreamModelLocality,
            engine_parameter_value::Value as EngineParameterValueKind,
        },
        deployment::v1::ExactPlacement,
        inference::v1::{FallbackGroup, FallbackGroupSelection, InferenceRoute},
        model::{
            private::v1::{ModelAssignment, ModelDesiredViewSection},
            v1::{
                CreateUpstreamModelDeploymentRequest, EstimateLocalModelResourcesResponse,
                HardwareClassResourceEstimate, LocalModelDeploymentConfiguration,
                ModelDeploymentDeclaration, ModelDeploymentDesiredState,
                ModelDeploymentLifecycleTransition, OpenAiCompatibleEndpointConfiguration,
                ScalarResourceEstimate, UpstreamModelDesiredConfiguration,
                UpstreamModelInputCapabilities,
                model_deployment_snapshot::Source as ModelDeploymentSnapshotSource,
            },
        },
    };
    use dotcore_site::{
        node::target_inventory::{
            NodeAcceleratorFacts, NodeAcceleratorObservation, RuntimePlatformObservation,
            TargetCapabilityInventory, TargetCapabilityReconciler,
        },
        persistence::SiteStorageError,
        repository::{
            ModelUnloadAcceptanceError, SiteRepository, SiteRepositoryError, SiteWriteFault,
        },
        store::FileCurrentState,
        view::fencing::NodeViewProjection,
    };
    use prost::Message;

    use super::{
        ModelDeclarationRepository, ModelDeclarationRepositoryError, retain_existing_api_key,
        transition_desired_state, unload_acceptance_error, validate_model_profile,
    };
    use crate::model::{
        ModelTeardownApplicationRouteBlocker, controller::ModelDeploymentExposureIntent,
    };

    #[test]
    fn unload_acceptance_error_preserves_application_route_blockers() {
        let blockers = vec![ModelTeardownApplicationRouteBlocker {
            application_deployment_id: "application-one".to_owned(),
            application_deployment_name: "Open WebUI".to_owned(),
            inference_route_id: "route-one".to_owned(),
            inference_route_name: "Gemma route".to_owned(),
        }];

        let error = unload_acceptance_error(
            ModelUnloadAcceptanceError::ApplicationRouteReferences(blockers.clone()),
        );

        assert!(matches!(
            error,
            ModelDeclarationRepositoryError::ApplicationRouteReferences(actual)
                if actual == blockers
        ));
    }

    #[test]
    fn model_profile_validation_rejects_missing_model_or_profile() {
        let model_id = CatalogModelId("model-one".to_owned());
        assert!(matches!(
            validate_model_profile(&model_id, "profile-one", Vec::new()),
            Err(ModelDeclarationRepositoryError::ModelNotFound(id)) if id == "model-one"
        ));
        assert!(matches!(
            validate_model_profile(
                &model_id,
                "stale-profile",
                vec![dotcore_proto::dotcore::catalog::v1::ModelProfile {
                    profile_id: "profile-one".to_owned(),
                    ..Default::default()
                }],
            ),
            Err(ModelDeclarationRepositoryError::ModelProfileNotFound { model_id, profile_id })
                if model_id == "model-one" && profile_id == "stale-profile"
        ));
    }

    #[test]
    fn pause_and_resume_repository_transitions_are_legal_and_idempotent() {
        let mut recovery = deployment(ModelDeploymentDesiredState::Present);
        recovery.lifecycle_recovery_required = true;
        let mut deployments = vec![recovery];
        let paused = transition_desired_state(
            &mut deployments,
            "deployment",
            ModelDeploymentDesiredState::Paused,
            "pause",
            2,
        )
        .expect("pause");
        assert_eq!(
            paused.desired_state,
            ModelDeploymentDesiredState::Paused as i32
        );
        assert_eq!(paused.lifecycle_generation, 2);
        assert!(!paused.lifecycle_recovery_required);
        assert_eq!(
            paused.lifecycle_transition,
            ModelDeploymentLifecycleTransition::Pause as i32
        );
        let repeated_pause = transition_desired_state(
            &mut deployments,
            "deployment",
            ModelDeploymentDesiredState::Paused,
            "pause",
            3,
        )
        .expect("idempotent pause");
        assert_eq!(repeated_pause, paused);
        let resumed = transition_desired_state(
            &mut deployments,
            "deployment",
            ModelDeploymentDesiredState::Present,
            "resume",
            3,
        )
        .expect("resume");
        assert_eq!(
            resumed.desired_state,
            ModelDeploymentDesiredState::Present as i32
        );
        assert_eq!(resumed.lifecycle_generation, 3);
        assert!(!resumed.lifecycle_recovery_required);
        assert_eq!(
            resumed.lifecycle_transition,
            ModelDeploymentLifecycleTransition::Resume as i32
        );
    }

    #[test]
    fn teardown_finalization_removes_only_the_exact_durable_teardown() {
        let directory = test_directory();
        let site_path = directory.path().join("site.yaml");
        let fixture_root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../fixtures/simulated-site");
        fs::copy(fixture_root.join("site.yaml"), &site_path).expect("simulated site fixture");
        let site = SiteRepository::load(&site_path, crate::test_support::site_declaration_rules())
            .expect("validated simulated site");
        let catalog = Arc::new(crate::test_support::upstream_template_catalog());
        let repository =
            ModelDeclarationRepository::new(site.clone(), catalog, fixture_target_capabilities());
        let created = repository
            .load_upstream_model(
                &upstream_request("endpoint.example"),
                &ModelDeploymentExposureIntent::Hidden,
                None,
            )
            .expect("accepted upstream deployment");
        let teardown = repository
            .unload_model(&created.model_deployment_id)
            .expect("accepted teardown");
        let accepted_bytes = fs::read(&site_path).expect("accepted site.yaml");
        let mut other_generation = teardown.clone();
        other_generation.lifecycle_generation += 1;
        let mut other_node = teardown.clone();
        other_node.placement = Some(dotcore_proto::dotcore::deployment::v1::ExactPlacement {
            site_node_id: "worker-one".to_owned(),
            execution_target_id: "target-one".to_owned(),
        });

        for mismatched in [&created, &other_generation, &other_node] {
            assert!(matches!(
                repository.finalize_teardown(mismatched),
                Err(ModelDeclarationRepositoryError::TeardownMismatch(id))
                    if id == created.model_deployment_id
            ));
        }
        assert_eq!(
            fs::read(&site_path).expect("unchanged site.yaml"),
            accepted_bytes
        );
        assert_eq!(
            site.snapshot().expect("unchanged site").model_deployments,
            std::slice::from_ref(&teardown)
        );

        repository
            .finalize_teardown(&teardown)
            .expect("exact teardown finalization");
        let finalized_bytes = fs::read(&site_path).expect("finalized site.yaml");
        repository
            .finalize_teardown(&teardown)
            .expect("repeated finalization of an absent teardown");

        assert!(
            site.snapshot()
                .expect("finalized site")
                .model_deployments
                .is_empty()
        );
        assert!(
            SiteRepository::load(&site_path, crate::test_support::site_declaration_rules())
                .expect("restarted site")
                .snapshot()
                .expect("restarted site snapshot")
                .model_deployments
                .is_empty()
        );
        assert_eq!(
            fs::read(&site_path).expect("clean site.yaml"),
            finalized_bytes
        );
        assert!(!site.is_dirty());
    }

    #[test]
    fn tag_only_engine_is_created_unpinned_for_its_hosting_node_to_pin() {
        let directory = test_directory();
        let site_path = directory.path().join("site.yaml");
        let fixture_root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../fixtures/simulated-site");
        fs::copy(fixture_root.join("site.yaml"), &site_path).expect("simulated site fixture");
        let site = SiteRepository::load(&site_path, crate::test_support::site_declaration_rules())
            .expect("validated simulated site");
        let repository = ModelDeclarationRepository::new(
            site,
            Arc::new(crate::test_support::tag_only_engine_catalog()),
            fixture_target_capabilities(),
        );
        let configuration = crate::test_support::local_model_configuration();
        let placement = dotcore_proto::dotcore::deployment::v1::ExactPlacement {
            site_node_id: "fixture-node".to_owned(),
            execution_target_id: "fixture-target".to_owned(),
        };

        let preview = repository
            .preview_local_model(
                "fixture-model",
                "fixture-profile",
                &placement,
                &configuration,
            )
            .expect("unpinned preview");
        let created = repository
            .load_model(
                "fixture-model",
                "fixture-profile",
                &placement,
                &configuration,
                &ModelDeploymentExposureIntent::Hidden,
                None,
            )
            .expect("accepted unpinned deployment on the manager's node");

        let Some(ModelDeploymentSnapshotSource::Local(accepted)) =
            created.snapshot.and_then(|snapshot| snapshot.source)
        else {
            panic!("local snapshot");
        };
        assert_eq!(accepted, preview);
        let engine = accepted.engine_artifact.expect("engine artifact");
        assert_eq!(engine.launch_identity, "");
        assert_eq!(
            engine.preparation_policy,
            dotcore_proto::dotcore::deployment::v1::ArtifactPreparationPolicy::LocalDevelopment
                as i32
        );
    }

    #[test]
    fn development_source_engine_is_created_unpinned_on_a_remote_node_by_a_manager_without_nix() {
        let directory = test_directory();
        let site_path = directory.path().join("site.yaml");
        let fixture_root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../fixtures/two-node-site");
        let linux_worker = "  siteNodeId: worker-node\n  displayName: Simulated models\n  platform: { operatingSystem: linux, architecture: amd64 }\n  acceleratorDeviceIds: [GPU-00000000-0000-0000-0000-000000005090]\n  modelRuntime:";
        let site_yaml = fs::read_to_string(fixture_root.join("site.yaml")).expect("two-node site");
        assert!(
            site_yaml.contains(linux_worker),
            "worker model target fixture"
        );
        fs::write(
            &site_path,
            site_yaml.replace(
                linux_worker,
                "  siteNodeId: worker-node\n  displayName: Simulated models\n  platform: { operatingSystem: darwin, architecture: arm64 }\n  acceleratorDeviceIds: [apple-gpu-0]\n  modelRuntime:",
            ),
        )
        .expect("darwin worker site");
        let site = SiteRepository::load(&site_path, crate::test_support::site_declaration_rules())
            .expect("validated darwin worker site");
        let repository = ModelDeclarationRepository::new(
            site,
            Arc::new(development_source_darwin_catalog(directory.path())),
            TargetCapabilityReconciler::reconcile_now(
                [("worker-target", "worker-node")],
                [RuntimePlatformObservation::available(
                    "worker-target",
                    dotcore_domain::ExecutionPlatform::darwin_arm64(),
                )],
                [NodeAcceleratorObservation::available(
                    "worker-node",
                    NodeAcceleratorFacts::apple(),
                )
                .with_host_memory(34_190_917_632, 34_190_917_632)],
            ),
        );
        let configuration = crate::test_support::local_model_configuration();
        let placement = dotcore_proto::dotcore::deployment::v1::ExactPlacement {
            site_node_id: "worker-node".to_owned(),
            execution_target_id: "worker-target".to_owned(),
        };

        let preview = repository
            .preview_local_model(
                "fixture-model",
                "fixture-profile",
                &placement,
                &configuration,
            )
            .expect("unpinned preview");
        let created = repository
            .load_model(
                "fixture-model",
                "fixture-profile",
                &placement,
                &configuration,
                &ModelDeploymentExposureIntent::Hidden,
                None,
            )
            .expect("accepted unpinned deployment on the worker node");

        let Some(ModelDeploymentSnapshotSource::Local(accepted)) =
            created.snapshot.and_then(|snapshot| snapshot.source)
        else {
            panic!("local snapshot");
        };
        assert_eq!(accepted, preview);
        assert!(created.node_resolved_launch_identities.is_empty());
        let engine = accepted.engine_artifact.expect("engine artifact");
        assert_eq!(
            (engine.preparation_policy, engine.launch_identity.as_str()),
            (
                dotcore_proto::dotcore::deployment::v1::ArtifactPreparationPolicy::DevelopmentSource
                    as i32,
                ""
            )
        );
        assert_eq!(
            engine.artifact.and_then(|artifact| artifact.kind),
            Some(
                dotcore_proto::dotcore::catalog::v1::engine_artifact::Kind::Nix(
                    dotcore_proto::dotcore::catalog::v1::NixArtifact {
                        flake: "self".to_owned(),
                        package: Some("engines.llama-cpp.native".to_owned()),
                        provides: Some(
                            dotcore_proto::dotcore::catalog::v1::nix_artifact::Provides::Executable(
                                "llama-server".to_owned()
                            )
                        ),
                        store_paths: Vec::new(),
                    }
                )
            )
        );
    }

    #[test]
    fn preview_is_pure_and_creation_reuses_the_exact_resolved_snapshot() {
        let directory = test_directory();
        let site_path = directory.path().join("site.yaml");
        let fixture_root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../fixtures/simulated-site");
        fs::copy(fixture_root.join("site.yaml"), &site_path).expect("simulated site fixture");
        let site = SiteRepository::load(&site_path, crate::test_support::site_declaration_rules())
            .expect("validated simulated site");
        let catalog = Arc::new(
            Catalog::load_with_policy(
                &fixture_root.join("catalog"),
                OciArtifactPolicy::Development,
                crate::model::engine::declaration_validator(),
            )
            .expect("fixture catalog"),
        );
        let repository = ModelDeclarationRepository::new(
            site.clone(),
            catalog.clone(),
            fixture_target_capabilities(),
        );
        let mut configuration = crate::test_support::local_model_configuration();
        configuration.custom_argument_tokens = vec![
            "--port=9000".to_owned(),
            "--port".to_owned(),
            "8081".to_owned(),
            "  $(printf inert)  ".to_owned(),
        ];
        let before = site.snapshot().expect("Site before preview");

        let (target, inventory) = crate::test_support::execution_target_capabilities(
            dotcore_domain::ExecutionPlatform::linux_amd64(),
        );
        let direct = crate::model::resolution::resolve_local_launch(
            catalog.as_ref(),
            &CatalogModelId("fixture-model".to_owned()),
            "fixture-profile",
            &configuration,
            &target,
            &inventory,
            None,
        )
        .expect("direct launch resolution");

        let preview = repository
            .preview_local_model(
                "fixture-model",
                "fixture-profile",
                &dotcore_proto::dotcore::deployment::v1::ExactPlacement {
                    site_node_id: "fixture-node".to_owned(),
                    execution_target_id: "fixture-target".to_owned(),
                },
                &configuration,
            )
            .expect("resolved preview");

        assert_eq!(site.snapshot().expect("Site after preview"), before);
        assert_eq!(preview, direct);
        assert_eq!(preview.encode_to_vec(), direct.encode_to_vec());
        assert_eq!(
            preview
                .desired_launch_plan
                .as_ref()
                .map(Message::encode_to_vec),
            direct
                .desired_launch_plan
                .as_ref()
                .map(Message::encode_to_vec),
        );
        assert_eq!(preview.configuration_id, "fixture-configuration");
        assert_eq!(preview.profile_id, "fixture-profile");
        let engine = preview
            .engine_artifact
            .as_ref()
            .expect("resolved engine artifact");
        let selected_engine = engine.artifact.as_ref().expect("selected engine artifact");
        assert_eq!(selected_engine.artifact_id, "fixture-linux");
        assert_eq!(selected_engine.selectors, []);
        assert_eq!(
            (
                engine.platform.clone(),
                engine.accelerator.as_str(),
                engine.minimum_nvidia_driver_major_version
            ),
            (
                Some(dotcore_domain::ExecutionPlatform::linux_amd64().to_oci_platform()),
                "nvidia",
                580
            )
        );
        assert_eq!(
            dotcore_domain::oci::engine_artifact_reference(selected_engine)
                .and_then(|reference| reference.digest.as_ref())
                .map(|digest| digest.encoded.as_str()),
            Some("0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef")
        );
        assert_eq!(
            preview
                .model_artifact
                .as_ref()
                .map(|artifact| artifact.artifact_id.as_str()),
            Some("fixture-artifact")
        );
        assert_eq!(
            preview
                .desired_launch_plan
                .as_ref()
                .expect("desired plan")
                .format_version,
            4
        );
        assert_eq!(
            preview.custom_argument_tokens,
            ["--port=9000", "--port", "8081", "  $(printf inert)  ",]
        );
        assert_eq!(
            preview
                .desired_launch_plan
                .as_ref()
                .expect("desired plan")
                .arguments[18..]
                .iter()
                .map(|argument| argument.value.clone().expect("argument value"))
                .collect::<Vec<_>>(),
            [
                dotcore_proto::dotcore::model::v1::portable_model_launch_argument_token::Value::Literal("--port=9000".to_owned()),
                dotcore_proto::dotcore::model::v1::portable_model_launch_argument_token::Value::Literal("--port".to_owned()),
                dotcore_proto::dotcore::model::v1::portable_model_launch_argument_token::Value::Literal("8081".to_owned()),
                dotcore_proto::dotcore::model::v1::portable_model_launch_argument_token::Value::Literal("  $(printf inert)  ".to_owned()),
            ]
        );

        let created = repository
            .load_model(
                "fixture-model",
                "fixture-profile",
                &dotcore_proto::dotcore::deployment::v1::ExactPlacement {
                    site_node_id: "fixture-node".to_owned(),
                    execution_target_id: "fixture-target".to_owned(),
                },
                &configuration,
                &ModelDeploymentExposureIntent::Hidden,
                Some("Preview parity"),
            )
            .expect("created deployment");
        let allocator = before
            .model_deployment_identity_allocator
            .as_ref()
            .expect("upgraded identity allocator");
        assert_eq!(
            created.model_deployment_id,
            format!("model-{}-1", allocator.namespace)
        );
        assert_eq!(created.lifecycle_generation, 1);
        assert_eq!(
            created.lifecycle_transition,
            ModelDeploymentLifecycleTransition::Create as i32
        );
        let created_snapshot = match created.snapshot.expect("created snapshot").source {
            Some(ModelDeploymentSnapshotSource::Local(local)) => local,
            _ => panic!("created local snapshot"),
        };
        assert_eq!(created_snapshot, preview);
        assert_eq!(created.model_deployment_name, "Preview parity");
        let after = site.snapshot().expect("Site after creation");
        assert_eq!(
            after.model_deployments.len(),
            before.model_deployments.len() + 1
        );
        assert_eq!(
            after
                .model_deployment_identity_allocator
                .expect("persisted identity allocator")
                .next_sequence,
            2
        );
    }

    #[test]
    fn preview_and_creation_require_the_target_device_count_the_launch_uses() {
        let fixture_root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../fixtures/simulated-site");
        let simulated = "  acceleratorDeviceIds: [GPU-00000000-0000-0000-0000-000000005090]
  modelRuntime:
    simulatedRuntime:
      defaultScenario:
        readyDelayMillis: '2000'
        stopDelayMillis: '2000'
";
        let fixture = fs::read_to_string(fixture_root.join("site.yaml")).expect("site fixture");
        assert!(
            fixture.contains(simulated),
            "simulated model target fixture"
        );
        let placement = dotcore_proto::dotcore::deployment::v1::ExactPlacement {
            site_node_id: "fixture-node".to_owned(),
            execution_target_id: "fixture-target".to_owned(),
        };
        let configuration = crate::test_support::local_model_configuration();

        for (devices, device_count) in [
            ("[nvidia.com/gpu=0, nvidia.com/gpu=1]", Some(2)),
            ("[]", Some(0)),
            ("[nvidia.com/gpu=1]", None),
        ] {
            let directory = test_directory();
            let site_path = directory.path().join("site.yaml");
            let docker = format!(
                "  acceleratorDeviceIds: {devices}
  modelRuntime:
    dockerRuntime:
      acceleratorTransport: DOCKER_ACCELERATOR_TRANSPORT_CDI
"
            );
            fs::write(&site_path, fixture.replace(simulated, &docker)).expect("Docker site");
            let site =
                SiteRepository::load(&site_path, crate::test_support::site_declaration_rules())
                    .expect("validated Docker site");
            let catalog = Arc::new(
                Catalog::load_with_policy(
                    &fixture_root.join("catalog"),
                    OciArtifactPolicy::Development,
                    crate::model::engine::declaration_validator(),
                )
                .expect("fixture catalog"),
            );
            let repository = ModelDeclarationRepository::new(
                site.clone(),
                catalog,
                fixture_target_capabilities(),
            );
            let before = site.snapshot().expect("site before");

            let preview = repository.preview_local_model(
                "fixture-model",
                "fixture-profile",
                &placement,
                &configuration,
            );
            let Some(device_count) = device_count else {
                preview.expect("one-device preview");
                continue;
            };
            let created = repository
                .load_model(
                    "fixture-model",
                    "fixture-profile",
                    &placement,
                    &configuration,
                    &ModelDeploymentExposureIntent::Hidden,
                    None,
                )
                .expect_err("mismatched creation");

            for error in [preview.expect_err("mismatched preview"), created] {
                assert!(
                    matches!(
                        error,
                        ModelDeclarationRepositoryError::TargetIncompatible(_)
                    ),
                    "{error:?}"
                );
                assert_eq!(
                    error.to_string(),
                    format!(
                        "execution target \"fixture-target\" has {device_count} accelerator devices, but the profile's launch uses 1"
                    )
                );
            }
            assert_eq!(
                site.snapshot().expect("site after").model_deployments,
                before.model_deployments
            );
        }
    }

    #[test]
    fn profile_compatibility_narrows_to_model_targets_and_rejects_unknown_selections() {
        let directory = test_directory();
        let site_path = directory.path().join("site.yaml");
        let fixture_root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../fixtures/simulated-site");
        fs::copy(fixture_root.join("site.yaml"), &site_path).expect("simulated site fixture");
        let site = SiteRepository::load(&site_path, crate::test_support::site_declaration_rules())
            .expect("validated simulated site");
        let catalog = Arc::new(
            Catalog::load_with_policy(
                &fixture_root.join("catalog"),
                OciArtifactPolicy::Development,
                crate::model::engine::declaration_validator(),
            )
            .expect("fixture catalog"),
        );
        let repository =
            ModelDeclarationRepository::new(site, catalog, TargetCapabilityInventory::empty_now());
        let inventory = fixture_target_capabilities();
        let model_target = dotcore_proto::dotcore::deployment::v1::ExactPlacement {
            site_node_id: "fixture-node".to_owned(),
            execution_target_id: "fixture-target".to_owned(),
        };
        let read = |model_id: Option<&str>, target: Option<&ExactPlacement>| {
            repository.profile_compatibility(
                model_id,
                target,
                None,
                Some(&inventory),
                &Default::default(),
            )
        };
        let summary =
            |response: dotcore_proto::dotcore::model::v1::ListModelProfileCompatibilityResponse| {
                (
                    response
                        .targets
                        .into_iter()
                        .map(|target| (target.target, target.status))
                        .collect::<Vec<_>>(),
                    response
                        .entries
                        .into_iter()
                        .map(|entry| (entry.model_id, entry.profile_id, entry.status))
                        .collect::<Vec<_>>(),
                )
            };
        let runs = dotcore_proto::dotcore::model::v1::ModelCompatibilityStatus::Runs as i32;
        let expected = (
            vec![(Some(model_target.clone()), runs)],
            vec![(
                "fixture-model".to_owned(),
                "fixture-profile".to_owned(),
                runs,
            )],
        );

        assert_eq!(summary(read(None, None).expect("whole site")), expected);
        assert_eq!(
            summary(read(Some("fixture-model"), Some(&model_target)).expect("narrowed read")),
            expected
        );
        assert!(matches!(
            read(Some("missing-model"), None),
            Err(ModelDeclarationRepositoryError::ModelNotFound(model_id)) if model_id == "missing-model"
        ));
        assert!(matches!(
            read(
                None,
                Some(&ExactPlacement {
                    site_node_id: "fixture-node".to_owned(),
                    execution_target_id: "missing-target".to_owned(),
                })
            ),
            Err(ModelDeclarationRepositoryError::ExecutionTargetNotFound(target_id)) if target_id == "missing-target"
        ));
        assert!(matches!(
            read(
                None,
                Some(&ExactPlacement {
                    site_node_id: "fixture-node".to_owned(),
                    execution_target_id: "fixture-application-target".to_owned(),
                })
            ),
            Err(ModelDeclarationRepositoryError::NotModelExecutionTarget(target_id)) if target_id == "fixture-application-target"
        ));
    }

    #[test]
    fn profile_compatibility_of_a_site_without_model_targets_has_only_profile_sizing() {
        let directory = test_directory();
        let site_path = directory.path().join("site.yaml");
        let fixture_root =
            Path::new(env!("CARGO_MANIFEST_DIR")).join("../fixtures/docker-apps-site");
        fs::copy(fixture_root.join("site.yaml"), &site_path).expect("application site fixture");
        let site = SiteRepository::load(&site_path, crate::test_support::site_declaration_rules())
            .expect("validated application site");
        let catalog = Arc::new(
            Catalog::load_with_policy(
                &Path::new(env!("CARGO_MANIFEST_DIR")).join("../fixtures/simulated-site/catalog"),
                OciArtifactPolicy::Development,
                crate::model::engine::declaration_validator(),
            )
            .expect("fixture catalog"),
        );
        let repository =
            ModelDeclarationRepository::new(site, catalog, TargetCapabilityInventory::empty_now());

        assert_eq!(
            repository
                .profile_compatibility(None, None, None, None, &Default::default())
                .expect("empty site"),
            dotcore_proto::dotcore::model::v1::ListModelProfileCompatibilityResponse {
                targets: Vec::new(),
                entries: Vec::new(),
                profiles: vec![dotcore_proto::dotcore::model::v1::ModelProfileSizing {
                    model_id: "fixture-model".to_owned(),
                    profile_id: "fixture-profile".to_owned(),
                    engine_parameter_values: Some(resolved_values(
                        profile_default_values(),
                        32_768
                    )),
                    hardware_class_estimates: vec![
                        dotcore_proto::dotcore::model::v1::HardwareClassResourceEstimate {
                            hardware_class_id: "fixture-nvidia".to_owned(),
                            estimates: Vec::new(),
                            derived_engine_parameter_values: Some(filled_context_per_request(
                                32_768
                            )),
                            launch_rejection: None,
                        }
                    ],
                    unsupported_reason: 0,
                    unsupported_hardware_class_ids: Vec::new(),
                }],
            }
        );
    }

    #[test]
    fn remote_node_target_is_accepted_from_exact_retained_capabilities() {
        let directory = test_directory();
        let site_path = directory.path().join("site.yaml");
        let fixture_root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../fixtures/two-node-site");
        fs::copy(fixture_root.join("site.yaml"), &site_path).expect("two-node site fixture");
        let site = SiteRepository::load(&site_path, crate::test_support::site_declaration_rules())
            .expect("validated two-node site");
        let catalog = Arc::new(fixture_catalog_with_upstream_template());
        let repository =
            ModelDeclarationRepository::new(site.clone(), catalog, two_node_target_capabilities());
        let required_secret = "synthetic-required-launch-secret";
        let configuration = dotcore_proto::dotcore::model::v1::LocalModelDeploymentConfiguration {
            model_artifact_binding_id: "fixture-artifact".to_owned(),
            engine_parameter_values: Some(engine_values(30_000)),
            custom_argument_tokens: vec![
                "--synthetic-secret".to_owned(),
                required_secret.to_owned(),
            ],
            option_overrides: Vec::new(),
        };

        let declaration = repository
            .load_model(
                "fixture-model",
                "fixture-profile",
                &dotcore_proto::dotcore::deployment::v1::ExactPlacement {
                    site_node_id: "worker-node".to_owned(),
                    execution_target_id: "worker-target".to_owned(),
                },
                &configuration,
                &ModelDeploymentExposureIntent::Hidden,
                Some("Remote fixture"),
            )
            .expect("remote node admission");

        assert_eq!(
            declaration.placement,
            Some(dotcore_proto::dotcore::deployment::v1::ExactPlacement {
                site_node_id: "worker-node".to_owned(),
                execution_target_id: "worker-target".to_owned(),
            })
        );
        let accepted = site.snapshot().expect("accepted site");
        assert_eq!(
            accepted.model_deployments.as_slice(),
            std::slice::from_ref(&declaration)
        );

        let other_node_secret = "other-node-only-secret";
        let mut other_configuration = configuration;
        other_configuration.custom_argument_tokens = vec![other_node_secret.to_owned()];
        repository
            .load_model(
                "fixture-model",
                "fixture-profile",
                &dotcore_proto::dotcore::deployment::v1::ExactPlacement {
                    site_node_id: "manager-node".to_owned(),
                    execution_target_id: "manager-target".to_owned(),
                },
                &other_configuration,
                &ModelDeploymentExposureIntent::Hidden,
                Some("Other node fixture"),
            )
            .expect("other-node admission");
        let upstream_secret = "unrelated-upstream-api-secret";
        let mut upstream = upstream_request("unrelated.example");
        upstream
            .desired_configuration
            .as_mut()
            .expect("upstream configuration")
            .api_key = upstream_secret.to_owned();
        repository
            .load_upstream_model(&upstream, &ModelDeploymentExposureIntent::Hidden, None)
            .expect("unrelated upstream admission");
        let authoritative = site.snapshot().expect("authoritative site");
        let authoritative_bytes = authoritative.encode_to_vec();
        assert!(
            authoritative_bytes
                .windows(other_node_secret.len())
                .any(|window| window == other_node_secret.as_bytes())
        );
        assert!(
            authoritative_bytes
                .windows(upstream_secret.len())
                .any(|window| window == upstream_secret.as_bytes())
        );
        assert!(authoritative.model_deployments.iter().any(|candidate| {
            candidate
                .upstream_desired_configuration
                .as_ref()
                .is_some_and(|configuration| configuration.api_key == upstream_secret)
        }));

        let projection = NodeViewProjection::for_site_node(&authoritative, "worker-node")
            .expect("node desired view projection");
        assert_eq!(
            projection.model_deployments(),
            std::slice::from_ref(&declaration)
        );
        let encoded = ModelDesiredViewSection {
            assignments: projection
                .model_deployments()
                .iter()
                .cloned()
                .map(|declaration| ModelAssignment {
                    declaration: Some(declaration),
                })
                .collect(),
        }
        .encode_to_vec();
        assert!(
            encoded
                .windows(required_secret.len())
                .any(|window| window == required_secret.as_bytes())
        );
        assert!(
            !encoded
                .windows(other_node_secret.len())
                .any(|window| { window == other_node_secret.as_bytes() })
        );
        assert!(
            !encoded
                .windows(upstream_secret.len())
                .any(|window| { window == upstream_secret.as_bytes() })
        );
        let decoded =
            ModelDesiredViewSection::decode(encoded.as_slice()).expect("node desired view codec");
        assert_eq!(decoded.assignments.len(), 1);
        assert_eq!(decoded.assignments[0].declaration, Some(declaration));
    }

    #[test]
    fn recovered_resume_projects_without_authorization_after_later_transitions_fail() {
        let directory = test_directory();
        let site_path = directory.path().join("site.yaml");
        let fixture_root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../fixtures/two-node-site");
        fs::copy(fixture_root.join("site.yaml"), &site_path).expect("two-node site fixture");
        let current = FileCurrentState::open(&site_path, "two-node-fixture-site")
            .expect("manager current state");
        let catalog = Arc::new(fixture_catalog_with_upstream_template());
        let initial_site =
            SiteRepository::load(&site_path, crate::test_support::site_declaration_rules())
                .expect("initial site");
        let initial = ModelDeclarationRepository::new_with_lifecycle(
            initial_site.clone(),
            Arc::clone(&catalog),
            two_node_target_capabilities(),
            current.clone(),
        );
        let created = initial
            .load_model(
                "fixture-model",
                "fixture-profile",
                &dotcore_proto::dotcore::deployment::v1::ExactPlacement {
                    site_node_id: "worker-node".to_owned(),
                    execution_target_id: "worker-target".to_owned(),
                },
                &dotcore_proto::dotcore::model::v1::LocalModelDeploymentConfiguration {
                    model_artifact_binding_id: "fixture-artifact".to_owned(),
                    engine_parameter_values: Some(engine_values(30_000)),
                    ..Default::default()
                },
                &ModelDeploymentExposureIntent::Hidden,
                None,
            )
            .expect("accepted node deployment");
        initial
            .pause_model(&created.model_deployment_id)
            .expect("accepted pause");
        let resumed = initial
            .resume_model(&created.model_deployment_id)
            .expect("durable Resume");
        assert_eq!(resumed.lifecycle_generation, 3);
        drop(initial);
        drop(initial_site);

        let failing_site = SiteRepository::load_with_mutation_failure_for_test(&site_path)
            .expect("failure-injected site");
        let failing = ModelDeclarationRepository::new_with_lifecycle(
            failing_site,
            Arc::clone(&catalog),
            two_node_target_capabilities(),
            current.clone(),
        );
        assert!(failing.pause_model(&created.model_deployment_id).is_err());
        assert!(failing.resume_model(&created.model_deployment_id).is_err());
        drop(failing);
        drop(current);

        let reopened = FileCurrentState::open(&site_path, "two-node-fixture-site")
            .expect("manager current state after restart");
        let recovered =
            SiteRepository::load(&site_path, crate::test_support::site_declaration_rules())
                .expect("Site after restart")
                .snapshot()
                .expect("recovered site");
        let projection = NodeViewProjection::for_site_node(&recovered, "worker-node")
            .expect("recovered Resume projects");
        let encoded = ModelAssignment {
            declaration: Some(resumed.clone()),
        }
        .encode_to_vec();

        assert_eq!(
            projection.model_deployments(),
            std::slice::from_ref(&resumed)
        );
        assert_eq!(
            resumed.lifecycle_transition,
            ModelDeploymentLifecycleTransition::Resume as i32
        );
        assert_eq!(
            reopened
                .lifecycle_high_water(
                    dotcore_domain::deployment::DeploymentOwner::Model,
                    &created.model_deployment_id
                )
                .expect("lifecycle high-water"),
            Some(5)
        );
        assert_eq!(
            ModelAssignment::decode(encoded.as_slice()).expect("assignment codec"),
            ModelAssignment {
                declaration: Some(resumed),
            }
        );
        assert_eq!(
            encoded,
            ModelAssignment {
                declaration: projection.model_deployments().first().cloned(),
            }
            .encode_to_vec()
        );
    }

    #[test]
    fn unload_allocates_teardown_above_burned_high_water_and_repeats_idempotently() {
        let directory = test_directory();
        let site_path = directory.path().join("site.yaml");
        let fixture_root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../fixtures/simulated-site");
        fs::copy(fixture_root.join("site.yaml"), &site_path).expect("simulated site fixture");
        let current =
            FileCurrentState::open(&site_path, "fixture-site").expect("manager current state");
        let catalog = Arc::new(crate::test_support::upstream_template_catalog());
        let initial_site =
            SiteRepository::load(&site_path, crate::test_support::site_declaration_rules())
                .expect("initial site");
        let created = ModelDeclarationRepository::new_with_lifecycle(
            initial_site.clone(),
            Arc::clone(&catalog),
            fixture_target_capabilities(),
            current.clone(),
        )
        .load_upstream_model(
            &upstream_request("endpoint.example"),
            &ModelDeploymentExposureIntent::Hidden,
            None,
        )
        .expect("accepted deployment");
        drop(initial_site);

        let failing_site = SiteRepository::load_with_mutation_failure_for_test(&site_path)
            .expect("failure-injected site");
        assert!(matches!(
            ModelDeclarationRepository::new_with_lifecycle(
                failing_site.clone(),
                Arc::clone(&catalog),
                fixture_target_capabilities(),
                current.clone(),
            )
            .pause_model(&created.model_deployment_id),
            Err(ModelDeclarationRepositoryError::Site(
                SiteRepositoryError::Storage(SiteStorageError::Storage(_))
            ))
        ));
        drop(failing_site);
        drop(current);
        assert_eq!(
            lifecycle_high_water(&site_path, &created.model_deployment_id),
            2
        );

        let reopened = FileCurrentState::open(&site_path, "fixture-site")
            .expect("manager current state after restart");
        let restarted_site =
            SiteRepository::load(&site_path, crate::test_support::site_declaration_rules())
                .expect("Site after restart");
        assert_eq!(
            restarted_site
                .snapshot()
                .expect("recovered site")
                .model_deployments[0]
                .lifecycle_generation,
            1
        );
        let restarted = ModelDeclarationRepository::new_with_lifecycle(
            restarted_site,
            catalog,
            fixture_target_capabilities(),
            reopened,
        );
        let teardown = restarted
            .unload_model(&created.model_deployment_id)
            .expect("accepted teardown");

        assert_eq!(teardown.lifecycle_generation, 3);
        assert_eq!(
            teardown.lifecycle_transition,
            ModelDeploymentLifecycleTransition::Teardown as i32
        );
        assert_eq!(
            lifecycle_high_water(&site_path, &created.model_deployment_id),
            3
        );
        let site_bytes = fs::read(&site_path).expect("Site after teardown");

        let repeated = restarted
            .unload_model(&created.model_deployment_id)
            .expect("repeated teardown");

        assert_eq!(repeated, teardown);
        assert_eq!(fs::read(&site_path).expect("Site after repeat"), site_bytes);
        assert_eq!(
            lifecycle_high_water(&site_path, &created.model_deployment_id),
            3
        );
    }

    #[test]
    fn route_blocked_unload_rejects_without_reserving_a_generation_or_writing_state() {
        for blocker in [
            UnloadBlocker::SharedRoute,
            UnloadBlocker::ApplicationBinding,
        ] {
            let directory = test_directory();
            let site_path = directory.path().join("site.yaml");
            let fixture_root =
                Path::new(env!("CARGO_MANIFEST_DIR")).join("../fixtures/simulated-site");
            fs::copy(fixture_root.join("site.yaml"), &site_path).expect("simulated site fixture");
            let current =
                FileCurrentState::open(&site_path, "fixture-site").expect("manager current state");
            let catalog = Arc::new(crate::test_support::upstream_template_catalog());
            let site =
                SiteRepository::load(&site_path, crate::test_support::site_declaration_rules())
                    .expect("Site");
            let repository = ModelDeclarationRepository::new_with_lifecycle(
                site.clone(),
                catalog,
                fixture_target_capabilities(),
                current.clone(),
            );
            let created = repository
                .load_upstream_model(
                    &upstream_request("endpoint.example"),
                    &ModelDeploymentExposureIntent::Direct {
                        route_name: "sole-route".to_owned(),
                    },
                    None,
                )
                .expect("accepted deployment");
            let deployment_id = created.model_deployment_id.clone();
            blocker.install(&repository, &site, &deployment_id);
            let site_before = fs::read(&site_path).expect("Site before unload");
            let state_path = site_path.with_file_name("state.pb");
            let state_before = fs::read(&state_path).expect("state before unload");
            let high_water_before = lifecycle_high_water(&site_path, &deployment_id);

            let result = repository.unload_model(&deployment_id);

            match (blocker, &result) {
                (
                    UnloadBlocker::SharedRoute,
                    Err(ModelDeclarationRepositoryError::SharedRouteReferences(names)),
                ) => assert_eq!(names, &["shared-route"]),
                (
                    UnloadBlocker::ApplicationBinding,
                    Err(ModelDeclarationRepositoryError::ApplicationRouteReferences(blockers)),
                ) => assert_eq!(
                    blockers,
                    &[ModelTeardownApplicationRouteBlocker {
                        application_deployment_id: "application-one".to_owned(),
                        application_deployment_name: "Bound application".to_owned(),
                        inference_route_id: blockers[0].inference_route_id.clone(),
                        inference_route_name: "sole-route".to_owned(),
                    }]
                ),
                (blocker, other) => panic!("{blocker:?} unload returned {other:?}"),
            }
            assert_eq!(
                lifecycle_high_water(&site_path, &deployment_id),
                high_water_before
            );
            assert_eq!(
                fs::read(&state_path).expect("state after unload"),
                state_before
            );
            assert_eq!(
                fs::read(&site_path).expect("Site after unload"),
                site_before
            );
        }
    }

    #[test]
    fn durable_teardown_finalization_prunes_the_lifecycle_record_without_identity_reuse() {
        let directory = test_directory();
        let site_path = directory.path().join("site.yaml");
        let fixture_root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../fixtures/simulated-site");
        fs::copy(fixture_root.join("site.yaml"), &site_path).expect("simulated site fixture");
        let current =
            FileCurrentState::open(&site_path, "fixture-site").expect("manager current state");
        let catalog = Arc::new(crate::test_support::upstream_template_catalog());
        let (site, faults) =
            SiteRepository::load_with_write_faults_for_test(&site_path).expect("Site");
        let repository = ModelDeclarationRepository::new_with_lifecycle(
            site.clone(),
            Arc::clone(&catalog),
            fixture_target_capabilities(),
            current.clone(),
        );
        let created = repository
            .load_upstream_model(
                &upstream_request("endpoint.example"),
                &ModelDeploymentExposureIntent::Hidden,
                None,
            )
            .expect("accepted deployment");
        let deployment_id = created.model_deployment_id.clone();
        repository.pause_model(&deployment_id).expect("pause");
        let teardown = repository.unload_model(&deployment_id).expect("teardown");
        assert_eq!(teardown.lifecycle_generation, 3);

        faults.push(SiteWriteFault::Storage);
        assert!(matches!(
            repository.finalize_teardown(&teardown),
            Err(ModelDeclarationRepositoryError::Site(
                SiteRepositoryError::Storage(SiteStorageError::Storage(_))
            ))
        ));
        assert_eq!(lifecycle_high_water(&site_path, &deployment_id), 3);

        repository
            .finalize_teardown(&teardown)
            .expect("durable teardown finalization");

        assert!(!site.is_dirty());
        assert_eq!(
            current
                .lifecycle_high_water(
                    dotcore_domain::deployment::DeploymentOwner::Model,
                    &deployment_id
                )
                .expect("pruned high-water"),
            None
        );
        for operation in [
            ModelDeclarationRepository::pause_model,
            ModelDeclarationRepository::resume_model,
            ModelDeclarationRepository::unload_model,
        ] {
            assert!(matches!(
                operation(&repository, &deployment_id),
                Err(ModelDeclarationRepositoryError::DeploymentNotFound(id)) if id == deployment_id
            ));
        }
        let replacement = repository
            .load_upstream_model(
                &upstream_request("endpoint.example"),
                &ModelDeploymentExposureIntent::Hidden,
                None,
            )
            .expect("replacement deployment");
        assert_ne!(replacement.model_deployment_id, deployment_id);
        drop((repository, site, current));

        let restarted = FileCurrentState::open_at_startup(
            &site_path,
            &crate::test_support::site_declaration_rules(),
        )
        .expect("restart resynchronizes declared deployments only");
        assert_eq!(
            restarted
                .lifecycle_high_water(
                    dotcore_domain::deployment::DeploymentOwner::Model,
                    &deployment_id
                )
                .expect("pruned high-water after restart"),
            None
        );
        assert_eq!(
            restarted
                .lifecycle_high_water(
                    dotcore_domain::deployment::DeploymentOwner::Model,
                    &replacement.model_deployment_id
                )
                .expect("replacement high-water"),
            Some(1)
        );
        let restarted_site =
            SiteRepository::load(&site_path, crate::test_support::site_declaration_rules())
                .expect("Site after restart");
        assert_ne!(
            restarted_site
                .reserve_model_deployment_id()
                .expect("next identity"),
            deployment_id
        );
    }

    #[test]
    fn failed_model_creation_consumes_reserved_identity_across_restart() {
        let directory = test_directory();
        let site_path = directory.path().join("site.yaml");
        let fixture_root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../fixtures/simulated-site");
        fs::copy(fixture_root.join("site.yaml"), &site_path).expect("simulated site fixture");
        let site = SiteRepository::load(&site_path, crate::test_support::site_declaration_rules())
            .expect("validated simulated site");
        let namespace = site
            .snapshot()
            .expect("Site before failed creation")
            .model_deployment_identity_allocator
            .expect("identity allocator")
            .namespace;
        let catalog = Arc::new(
            Catalog::load_with_policy(
                &fixture_root.join("catalog"),
                OciArtifactPolicy::Development,
                crate::model::engine::declaration_validator(),
            )
            .expect("fixture catalog"),
        );
        let repository =
            ModelDeclarationRepository::new(site.clone(), catalog, fixture_target_capabilities());

        assert!(matches!(
            repository.load_model(
                "missing-model",
                "missing-profile",
                &dotcore_proto::dotcore::deployment::v1::ExactPlacement {
                    site_node_id: "fixture-node".to_owned(),
                    execution_target_id: "fixture-target".to_owned(),
                },
                &crate::test_support::local_model_configuration(),
                &ModelDeploymentExposureIntent::Hidden,
                None
            ),
            Err(ModelDeclarationRepositoryError::ModelNotFound(_))
        ));
        assert!(
            site.snapshot()
                .expect("Site after failed creation")
                .model_deployments
                .is_empty()
        );
        drop(repository);
        drop(site);

        let restarted =
            SiteRepository::load(&site_path, crate::test_support::site_declaration_rules())
                .expect("Site after restart");
        assert_eq!(
            restarted
                .reserve_model_deployment_id()
                .expect("next durable reservation"),
            format!("model-{namespace}-2")
        );
    }

    #[test]
    fn resource_estimation_is_pure_and_omits_missing_or_custom_argument_sizing() {
        let directory = test_directory();
        let fixture_root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../fixtures/simulated-site");
        let site_path = directory.path().join("site.yaml");
        fs::copy(fixture_root.join("site.yaml"), &site_path).expect("simulated site fixture");
        let site = SiteRepository::load(&site_path, crate::test_support::site_declaration_rules())
            .expect("validated simulated site");
        let catalog_root = write_sizing_catalog(directory.path(), &fixture_root);
        let catalog = Arc::new(
            Catalog::load_with_policy(
                &catalog_root,
                OciArtifactPolicy::Development,
                crate::model::engine::declaration_validator(),
            )
            .expect("sizing catalog"),
        );
        let repository =
            ModelDeclarationRepository::new(site.clone(), catalog, fixture_target_capabilities());
        let before = site.snapshot().expect("Site before estimate");
        let mut configuration = crate::test_support::local_model_configuration();
        configuration.engine_parameter_values = Some(engine_values(4096));

        let response = repository
            .estimate_local_model_resources(
                "fixture-model",
                "fixture-profile",
                &configuration,
                None,
            )
            .map(only_class)
            .expect("resource estimates");
        assert_eq!(
            response.derived_engine_parameter_values,
            Some(filled_context_per_request(4096))
        );
        assert_eq!(
            response.estimates,
            vec![
                ScalarResourceEstimate {
                    resource: "acceleratorMemory".to_owned(),
                    value: 12_388,
                    unit: "bytes".to_owned(),
                },
                ScalarResourceEstimate {
                    resource: "hostMemory".to_owned(),
                    value: 50,
                    unit: "bytes".to_owned(),
                },
            ]
        );
        assert_eq!(site.snapshot().expect("Site after estimate"), before);

        // Values a launch rejects keep their estimates, with the reason.
        let rejected = |total_context: u64, context_per_request: u64| {
            let mut configuration = configuration.clone();
            configuration.engine_parameter_values = Some(unsigned_values(&[
                ("total-context", total_context),
                ("context-per-request", context_per_request),
                ("prompt-cache-memory-mib", 0),
                ("parallel-requests", 4),
            ]));
            repository
                .estimate_local_model_resources(
                    "fixture-model",
                    "fixture-profile",
                    &configuration,
                    None,
                )
                .map(only_class)
                .expect("estimates of rejected values")
        };
        let above_total = rejected(4096, 8192);
        assert_eq!(
            above_total.launch_rejection.as_deref(),
            Some(
                "context per request of 8192 tokens is above the total context of 4096 tokens; lower the context per request or raise the total context"
            )
        );
        assert_eq!(above_total.estimates[0].value, 12_388);
        let above_maximum = rejected(300_000, 300_000);
        assert_eq!(
            above_maximum.launch_rejection.as_deref(),
            Some(
                "context per request of 300000 tokens is above the model's maximum context of 262144 tokens; lower the context per request"
            )
        );
        assert_eq!(above_maximum.estimates[0].resource, "acceleratorMemory");

        configuration.custom_argument_tokens = vec!["--override".to_owned()];
        assert_eq!(
            repository
                .estimate_local_model_resources(
                    "fixture-model",
                    "fixture-profile",
                    &configuration,
                    None
                )
                .map(only_class)
                .expect("custom arguments omit estimates")
                .estimates,
            Vec::new()
        );
        configuration.custom_argument_tokens.clear();
        configuration
            .engine_parameter_values
            .as_mut()
            .expect("parameter values")
            .values[0]
            .value = Some(EngineParameterValueKind::UnsignedInteger(5000));
        assert_eq!(
            repository
                .estimate_local_model_resources(
                    "fixture-model",
                    "fixture-profile",
                    &configuration,
                    None
                )
                .map(only_class)
                .expect("resource estimates")
                .estimates,
            vec![
                ScalarResourceEstimate {
                    resource: "acceleratorMemory".to_owned(),
                    value: 15_100,
                    unit: "bytes".to_owned(),
                },
                ScalarResourceEstimate {
                    resource: "hostMemory".to_owned(),
                    value: 50,
                    unit: "bytes".to_owned(),
                },
            ]
        );

        let catalog_without_sizing = Arc::new(
            Catalog::load_with_policy(
                &fixture_root.join("catalog"),
                OciArtifactPolicy::Development,
                crate::model::engine::declaration_validator(),
            )
            .expect("fixture catalog"),
        );
        let repository_without_sizing = ModelDeclarationRepository::new(
            site.clone(),
            catalog_without_sizing,
            fixture_target_capabilities(),
        );
        assert_eq!(
            repository_without_sizing
                .estimate_local_model_resources(
                    "fixture-model",
                    "fixture-profile",
                    &crate::test_support::local_model_configuration(),
                    None,
                )
                .map(only_class)
                .expect("missing sizing omits estimates")
                .estimates,
            Vec::new()
        );
        assert_eq!(site.snapshot().expect("unchanged site"), before);
    }

    #[test]
    fn resource_estimation_returns_derived_values_even_with_custom_arguments() {
        let directory = test_directory();
        let fixture_root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../fixtures/simulated-site");
        let site_path = directory.path().join("site.yaml");
        fs::copy(fixture_root.join("site.yaml"), &site_path).expect("simulated site fixture");
        let site = SiteRepository::load(&site_path, crate::test_support::site_declaration_rules())
            .expect("validated simulated site");
        let catalog_root = write_sizing_catalog(directory.path(), &fixture_root);
        let model_path = catalog_root.join("models/fixture-model.yaml");
        let source = fs::read_to_string(&model_path).expect("sizing model fixture");
        // The profile leaves the prompt cache out, so sizing derives it.
        fs::write(
            &model_path,
            source
                .replace(
                    "      estimates:\n",
                    "      derivedParameters:\n        prompt-cache-memory-mib: \"4\"\n      estimates:\n",
                )
                .replace(
                    "      values:\n        - { parameterId: \"prompt-cache-memory-mib\", unsignedInteger: \"0\" }\n",
                    "      values: []\n",
                ),
        )
        .expect("derived sizing model fixture");
        let catalog = Arc::new(
            Catalog::load_with_policy(
                &catalog_root,
                OciArtifactPolicy::Development,
                crate::model::engine::declaration_validator(),
            )
            .expect("derived sizing catalog"),
        );
        let repository =
            ModelDeclarationRepository::new(site, catalog, fixture_target_capabilities());
        let mut configuration = crate::test_support::local_model_configuration();
        configuration.engine_parameter_values = Some(unsigned_values(&[
            ("total-context", 4096),
            ("parallel-requests", 4),
        ]));
        let derived = Some(unsigned_values(&[
            ("context-per-request", 4096),
            ("prompt-cache-memory-mib", 4),
        ]));

        let response = repository
            .estimate_local_model_resources(
                "fixture-model",
                "fixture-profile",
                &configuration,
                None,
            )
            .map(only_class)
            .expect("derived estimates");
        assert_eq!(response.derived_engine_parameter_values, derived);
        assert_eq!(
            response.estimates[0],
            ScalarResourceEstimate {
                resource: "acceleratorMemory".to_owned(),
                value: 12_388,
                unit: "bytes".to_owned(),
            }
        );

        configuration.custom_argument_tokens = vec!["--override".to_owned()];
        let response = repository
            .estimate_local_model_resources(
                "fixture-model",
                "fixture-profile",
                &configuration,
                None,
            )
            .map(only_class)
            .expect("custom argument estimates");
        assert_eq!(response.derived_engine_parameter_values, derived);
        assert_eq!(response.estimates, Vec::new());
    }

    #[test]
    fn resource_estimation_on_a_target_uses_its_memory_architecture_class() {
        let directory = test_directory();
        let fixture_root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../fixtures/simulated-site");
        let site_path = directory.path().join("site.yaml");
        fs::copy(fixture_root.join("site.yaml"), &site_path).expect("simulated site fixture");
        let site = SiteRepository::load(&site_path, crate::test_support::site_declaration_rules())
            .expect("validated simulated site");
        let catalog_root = write_sizing_catalog(directory.path(), &fixture_root);
        for (id, architecture) in [("unified", "UNIFIED"), ("dedicated", "DEDICATED")] {
            fs::write(
                catalog_root.join(format!("hardware-classes/fixture-{id}.yaml")),
                format!(
                    "hardwareClassId: \"fixture-{id}\"\ndisplayName: \"Fixture {id}\"\nselector:\n  accelerators: [\"nvidia\"]\n  memoryArchitecture: \"ACCELERATOR_MEMORY_ARCHITECTURE_{architecture}\"\n"
                ),
            )
            .expect("memory architecture class fixture");
        }
        let model_path = catalog_root.join("models/fixture-model.yaml");
        let source = fs::read_to_string(&model_path).expect("sizing model fixture");
        let sized = source
            .split("    sizing:\n")
            .next()
            .expect("model head")
            .to_owned()
            + r#"    sizing:
      classParameters: ["cacheOnAccelerator"]
      parameters:
        base: 100
        rate: 3
        host: 50
      estimates:
        acceleratorMemory:
          unit: "bytes"
          equation: "${base} + ${rate} * ${total-context} + ${cacheOnAccelerator} * ${host}"
        hostMemory:
          unit: "bytes"
          availableWhen: "${cacheOnAccelerator} == 0"
          equation: "${host}"
    hardwareSupport:
      - hardwareClassId: "fixture-unified"
        parameters:
          cacheOnAccelerator: 1
        bindingSizing:
          - bindingId: "fixture-artifact"
      - hardwareClassId: "fixture-dedicated"
        parameters:
          cacheOnAccelerator: 0
        bindingSizing:
          - bindingId: "fixture-artifact"
"# + source
            .split("          - bindingId: \"fixture-artifact\"\n")
            .nth(1)
            .expect("model tail");
        fs::write(&model_path, sized).expect("two-class sizing model fixture");
        let catalog = Arc::new(
            Catalog::load_with_policy(
                &catalog_root,
                OciArtifactPolicy::Development,
                crate::model::engine::declaration_validator(),
            )
            .expect("two-class sizing catalog"),
        );
        let placement = ExactPlacement {
            site_node_id: "fixture-node".to_owned(),
            execution_target_id: "fixture-target".to_owned(),
        };
        let mut configuration = crate::test_support::local_model_configuration();
        configuration.engine_parameter_values = Some(engine_values(4096));
        let estimate = |memory_architecture| {
            let (_, inventory) = crate::test_support::execution_target_capabilities_with_memory(
                dotcore_domain::ExecutionPlatform::linux_amd64(),
                memory_architecture,
            );
            ModelDeclarationRepository::new(site.clone(), catalog.clone(), inventory.clone())
                .estimate_local_model_resources(
                    "fixture-model",
                    "fixture-profile",
                    &configuration,
                    Some(super::EstimateTarget {
                        placement: &placement,
                        inventory: &inventory,
                        workloads: &Default::default(),
                    }),
                )
                .map(|response| {
                    only_class(response)
                        .estimates
                        .into_iter()
                        .map(|estimate| (estimate.resource, estimate.value))
                        .collect::<Vec<_>>()
                })
        };
        let bytes = |resource: &str, value| (resource.to_owned(), value);

        // Unified memory counts the host term in accelerator memory.
        assert_eq!(
            estimate(dotcore_domain::MemoryArchitecture::Unified).expect("unified estimate"),
            vec![bytes("acceleratorMemory", 12_438)]
        );
        // Dedicated memory keeps it in a separate host memory estimate.
        assert_eq!(
            estimate(dotcore_domain::MemoryArchitecture::Dedicated).expect("dedicated estimate"),
            vec![bytes("acceleratorMemory", 12_388), bytes("hostMemory", 50)]
        );
        // Without a target, every supported class is estimated, in declaration order.
        let per_class =
            ModelDeclarationRepository::new(site, catalog, fixture_target_capabilities())
                .estimate_local_model_resources(
                    "fixture-model",
                    "fixture-profile",
                    &configuration,
                    None,
                )
                .expect("estimates per class")
                .hardware_class_estimates
                .into_iter()
                .map(|class| {
                    (
                        class.hardware_class_id,
                        class
                            .estimates
                            .into_iter()
                            .map(|estimate| (estimate.resource, estimate.value))
                            .collect::<Vec<_>>(),
                    )
                })
                .collect::<Vec<_>>();
        assert_eq!(
            per_class,
            vec![
                (
                    "fixture-unified".to_owned(),
                    vec![bytes("acceleratorMemory", 12_438)]
                ),
                (
                    "fixture-dedicated".to_owned(),
                    vec![bytes("acceleratorMemory", 12_388), bytes("hostMemory", 50)]
                ),
            ]
        );
    }

    #[test]
    fn profile_sizing_without_a_scenario_matches_the_profile_default_estimate() {
        let directory = test_directory();
        let fixture_root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../fixtures/simulated-site");
        let site_path = directory.path().join("site.yaml");
        fs::copy(fixture_root.join("site.yaml"), &site_path).expect("simulated site fixture");
        let site = SiteRepository::load(&site_path, crate::test_support::site_declaration_rules())
            .expect("validated simulated site");
        let catalog = Arc::new(
            Catalog::load_with_policy(
                &write_sizing_catalog(directory.path(), &fixture_root),
                OciArtifactPolicy::Development,
                crate::model::engine::declaration_validator(),
            )
            .expect("sizing catalog"),
        );
        let repository =
            ModelDeclarationRepository::new(site, catalog, fixture_target_capabilities());

        let estimate = repository
            .estimate_local_model_resources(
                "fixture-model",
                "fixture-profile",
                &LocalModelDeploymentConfiguration::default(),
                None,
            )
            .expect("profile default estimate");
        let compatibility = repository
            .profile_compatibility(Some("fixture-model"), None, None, None, &Default::default())
            .expect("profile compatibility");

        // The fixture sizes 100 + 3 × the default 32768-token total context on the accelerator and
        // 50 on the host.
        assert_eq!(
            estimate
                .hardware_class_estimates
                .iter()
                .flat_map(|class| class.estimates.iter().map(|estimate| estimate.value))
                .collect::<Vec<_>>(),
            [98_404, 50]
        );
        assert_eq!(
            compatibility
                .profiles
                .into_iter()
                .map(|profile| (
                    profile.profile_id,
                    profile.engine_parameter_values,
                    profile.hardware_class_estimates
                ))
                .collect::<Vec<_>>(),
            [(
                "fixture-profile".to_owned(),
                Some(resolved_values(profile_default_values(), 32_768)),
                estimate.hardware_class_estimates
            )]
        );
    }

    #[test]
    fn omitted_engine_configuration_resolves_the_profile_defaults_in_estimate_preview_and_create() {
        let directory = test_directory();
        let fixture_root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../fixtures/simulated-site");
        let site_path = directory.path().join("site.yaml");
        fs::copy(fixture_root.join("site.yaml"), &site_path).expect("simulated site fixture");
        let site = SiteRepository::load(&site_path, crate::test_support::site_declaration_rules())
            .expect("validated simulated site");
        let catalog_root = write_sizing_catalog(directory.path(), &fixture_root);
        let load_catalog = || {
            Arc::new(
                Catalog::load_with_policy(
                    &catalog_root,
                    OciArtifactPolicy::Development,
                    crate::model::engine::declaration_validator(),
                )
                .expect("sizing catalog"),
            )
        };
        let repository = ModelDeclarationRepository::new(
            site.clone(),
            load_catalog(),
            fixture_target_capabilities(),
        );
        let placement = ExactPlacement {
            site_node_id: "fixture-node".to_owned(),
            execution_target_id: "fixture-target".to_owned(),
        };
        let omitted = LocalModelDeploymentConfiguration::default();
        let profile_defaults = Some(resolved_values(profile_default_values(), 32_768));

        let estimate = repository
            .estimate_local_model_resources("fixture-model", "fixture-profile", &omitted, None)
            .map(only_class)
            .expect("estimate with profile defaults");
        let preview = repository
            .preview_local_model("fixture-model", "fixture-profile", &placement, &omitted)
            .expect("preview with profile defaults");
        let explicit = repository
            .preview_local_model(
                "fixture-model",
                "fixture-profile",
                &placement,
                &crate::test_support::local_model_configuration(),
            )
            .expect("preview with the explicit profile defaults");
        let created = repository
            .load_model(
                "fixture-model",
                "fixture-profile",
                &placement,
                &omitted,
                &ModelDeploymentExposureIntent::Hidden,
                None,
            )
            .expect("create with profile defaults");
        let created_snapshot = |deployment: &ModelDeploymentDeclaration| match deployment
            .snapshot
            .clone()
            .and_then(|snapshot| snapshot.source)
        {
            Some(ModelDeploymentSnapshotSource::Local(local)) => local,
            _ => panic!("created local snapshot"),
        };

        assert_eq!(preview, explicit);
        assert_eq!(preview.engine_parameter_values, profile_defaults);
        assert_eq!(
            preview
                .model_artifact
                .as_ref()
                .map(|artifact| artifact.artifact_id.as_str()),
            Some("fixture-artifact")
        );
        assert_eq!(created_snapshot(&created), preview);
        assert_eq!(
            estimate.derived_engine_parameter_values,
            Some(filled_context_per_request(32_768))
        );
        assert_eq!(
            estimate.estimates,
            vec![
                ScalarResourceEstimate {
                    resource: "acceleratorMemory".to_owned(),
                    value: 98_404,
                    unit: "bytes".to_owned(),
                },
                ScalarResourceEstimate {
                    resource: "hostMemory".to_owned(),
                    value: 50,
                    unit: "bytes".to_owned(),
                },
            ]
        );
        assert_eq!(
            preview
                .sizing
                .as_ref()
                .and_then(|sizing| sizing.accelerator_memory_bytes),
            Some(98_404)
        );

        let model_path = catalog_root.join("models/fixture-model.yaml");
        let source = fs::read_to_string(&model_path).expect("sizing model fixture");
        let edited = source.replace(
            r#"{ parameterId: "prompt-cache-memory-mib", unsignedInteger: "0" }"#,
            r#"{ parameterId: "prompt-cache-memory-mib", unsignedInteger: "7" }"#,
        );
        assert_ne!(edited, source, "profile default edit");
        fs::write(&model_path, edited).expect("edited model fixture");
        let edited_repository = ModelDeclarationRepository::new(
            site.clone(),
            load_catalog(),
            fixture_target_capabilities(),
        );
        assert_eq!(
            edited_repository
                .preview_local_model("fixture-model", "fixture-profile", &placement, &omitted)
                .expect("preview with edited defaults")
                .engine_parameter_values,
            Some(resolved_values(
                unsigned_values(&[
                    ("prompt-cache-memory-mib", 7),
                    ("total-context", 32_768),
                    ("parallel-requests", 4),
                ]),
                32_768
            ))
        );
        let retained = edited_repository
            .snapshot()
            .expect("Site after the catalog edit")
            .deployments
            .into_iter()
            .find(|deployment| deployment.model_deployment_id == created.model_deployment_id)
            .expect("retained deployment");
        assert_eq!(created_snapshot(&retained), preview);
    }

    #[test]
    fn omitted_engine_configuration_of_a_missing_profile_is_not_found() {
        let directory = test_directory();
        let fixture_root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../fixtures/simulated-site");
        let site_path = directory.path().join("site.yaml");
        fs::copy(fixture_root.join("site.yaml"), &site_path).expect("simulated site fixture");
        let site = SiteRepository::load(&site_path, crate::test_support::site_declaration_rules())
            .expect("validated simulated site");
        let catalog = Arc::new(
            Catalog::load_with_policy(
                &fixture_root.join("catalog"),
                OciArtifactPolicy::Development,
                crate::model::engine::declaration_validator(),
            )
            .expect("fixture catalog"),
        );
        let repository =
            ModelDeclarationRepository::new(site, catalog, fixture_target_capabilities());

        let error = repository
            .estimate_local_model_resources(
                "fixture-model",
                "missing-profile",
                &LocalModelDeploymentConfiguration::default(),
                None,
            )
            .expect_err("missing profile");
        assert_eq!(
            error.to_string(),
            "Catalog model \"fixture-model\" has no profile \"missing-profile\""
        );
    }

    #[test]
    fn whitespace_only_upstream_update_key_retains_the_existing_key() {
        assert_eq!(
            retain_existing_api_key("existing-key", " \t\n "),
            "existing-key"
        );
        assert_eq!(
            retain_existing_api_key("existing-key", "replacement-key"),
            "replacement-key"
        );
    }

    #[test]
    fn upstream_load_accepts_duplicate_provider_model_ids_for_distinct_deployments() {
        let directory = test_directory();
        let site_path = directory.path().join("site.yaml");
        fs::copy(
            Path::new(env!("CARGO_MANIFEST_DIR")).join("../fixtures/simulated-site/site.yaml"),
            &site_path,
        )
        .expect("simulated site fixture");
        let site = SiteRepository::load(&site_path, crate::test_support::site_declaration_rules())
            .expect("validated simulated site");
        let catalog = Arc::new(crate::test_support::upstream_template_catalog());
        let repository =
            ModelDeclarationRepository::new(site.clone(), catalog, fixture_target_capabilities());
        let first = repository
            .load_upstream_model(
                &upstream_request("endpoint-one.example"),
                &ModelDeploymentExposureIntent::Hidden,
                None,
            )
            .expect("first upstream model deployment");
        let second = repository
            .load_upstream_model(
                &upstream_request("endpoint-two.example"),
                &ModelDeploymentExposureIntent::Hidden,
                None,
            )
            .expect("second upstream model deployment");
        let snapshot = site.snapshot().expect("updated site authority");

        assert_ne!(first.model_deployment_id, second.model_deployment_id);
        assert_eq!(
            first
                .upstream_desired_configuration
                .as_ref()
                .expect("first configuration")
                .upstream_model_id,
            "shared-provider-model"
        );
        assert_eq!(
            second
                .upstream_desired_configuration
                .as_ref()
                .expect("second configuration")
                .upstream_model_id,
            "shared-provider-model"
        );
        assert!(matches!(
            first.snapshot.as_ref().and_then(|snapshot| snapshot.source.as_ref()),
            Some(ModelDeploymentSnapshotSource::Upstream(upstream))
                if upstream.upstream_model_template.is_some()
        ));
        assert_eq!(
            snapshot
                .inference_routes
                .iter()
                .filter(|route| route.fallback_groups.iter().any(|group| {
                    group.model_deployment_ids.iter().any(|id| {
                        id == &first.model_deployment_id || id == &second.model_deployment_id
                    })
                }))
                .count(),
            0
        );
    }

    #[test]
    fn teardown_never_reverses_or_accepts_pause_resume() {
        let mut deployments = vec![deployment(ModelDeploymentDesiredState::Teardown)];
        for (target, operation) in [
            (ModelDeploymentDesiredState::Paused, "pause"),
            (ModelDeploymentDesiredState::Present, "resume"),
        ] {
            assert!(matches!(
                transition_desired_state(&mut deployments, "deployment", target, operation, 2),
                Err(ModelDeclarationRepositoryError::InvalidTransition { .. })
            ));
        }
        assert_eq!(
            deployments[0].desired_state,
            ModelDeploymentDesiredState::Teardown as i32
        );
    }

    fn upstream_request(host: &str) -> CreateUpstreamModelDeploymentRequest {
        CreateUpstreamModelDeploymentRequest {
            upstream_model_template_id: "openai-compatible".to_owned(),
            desired_configuration: Some(UpstreamModelDesiredConfiguration {
                upstream_model_id: "shared-provider-model".to_owned(),
                api_key: "test-key".to_owned(),
                input_capabilities: Some(UpstreamModelInputCapabilities {
                    supports_text: true,
                    supports_image: false,
                }),
                endpoint: Some(OpenAiCompatibleEndpointConfiguration {
                    base_url: format!("https://{host}/v1"),
                    allow_insecure_http: false,
                }),
                boundary: Some(UpstreamModelBoundary {
                    locality: UpstreamModelLocality::PublicExternal as i32,
                    trust_domain: host.to_owned(),
                    data_egress_class: UpstreamModelDataEgressClass::PublicInternet as i32,
                }),
            }),
        }
    }

    fn deployment(desired_state: ModelDeploymentDesiredState) -> ModelDeploymentDeclaration {
        ModelDeploymentDeclaration {
            model_deployment_id: "deployment".to_owned(),
            desired_state: desired_state as i32,
            lifecycle_generation: 1,
            lifecycle_transition: ModelDeploymentLifecycleTransition::Create as i32,
            lifecycle_recovery_required: false,
            ..Default::default()
        }
    }

    /// The one hardware class result of an estimate for a single-class profile or a target.
    fn only_class(response: EstimateLocalModelResourcesResponse) -> HardwareClassResourceEstimate {
        let [class] =
            <[HardwareClassResourceEstimate; 1]>::try_from(response.hardware_class_estimates)
                .expect("exactly one hardware class estimate");
        class
    }

    /// Engine parameter values for a llama.cpp deployment: the total context, the prompt cache and
    /// the parallel requests that sizing never derives.
    fn engine_values(total_context: u64) -> EngineParameterValues {
        unsigned_values(&[
            ("total-context", total_context),
            ("prompt-cache-memory-mib", 0),
            ("parallel-requests", 4),
        ])
    }

    /// The fixture profile's engine parameter defaults: its prompt cache, then the engine defaults
    /// of the total context and the parallel requests.
    fn profile_default_values() -> EngineParameterValues {
        unsigned_values(&[
            ("prompt-cache-memory-mib", 0),
            ("total-context", 32_768),
            ("parallel-requests", 4),
        ])
    }

    /// The context per request resolution fills from the total context, within the fixture
    /// model's maximum context.
    fn filled_context_per_request(total_context: u64) -> EngineParameterValues {
        unsigned_values(&[("context-per-request", total_context.min(262_144))])
    }

    /// Values in the given order, followed by the filled context per request, as a resolved
    /// snapshot retains them.
    fn resolved_values(
        mut values: EngineParameterValues,
        total_context: u64,
    ) -> EngineParameterValues {
        values
            .values
            .extend(filled_context_per_request(total_context).values);
        values
    }

    fn unsigned_values(values: &[(&str, u64)]) -> EngineParameterValues {
        EngineParameterValues {
            values: values
                .iter()
                .copied()
                .map(|(parameter_id, value)| EngineParameterValue {
                    parameter_id: parameter_id.to_owned(),
                    value: Some(EngineParameterValueKind::UnsignedInteger(value)),
                })
                .collect(),
        }
    }

    fn write_sizing_catalog(directory: &Path, fixture_root: &Path) -> std::path::PathBuf {
        let catalog_root = directory.join("catalog");
        let models = catalog_root.join("models");
        let engines = catalog_root.join("engines");
        let hardware_classes = catalog_root.join("hardware-classes");
        for fixture_directory in [&models, &engines, &hardware_classes] {
            fs::create_dir_all(fixture_directory).expect("catalog fixture directory");
        }
        fs::copy(
            fixture_root.join("catalog/engines/fixture.yaml"),
            engines.join("fixture.yaml"),
        )
        .expect("Engine fixture");
        fs::copy(
            fixture_root.join("catalog/hardware-classes/fixture-nvidia.yaml"),
            hardware_classes.join("fixture-nvidia.yaml"),
        )
        .expect("hardware class fixture");
        let source = fs::read_to_string(fixture_root.join("catalog/models/fixture-model.yaml"))
            .expect("model fixture");
        let support = "    hardwareSupport:\n      - hardwareClassId: \"fixture-nvidia\"\n        hardwareRequirements:\n          minimumSystemMemoryBytes: \"1\"\n";
        let sized_support = r#"    sizing:
      parameters:
        base: 100
        rate: 3
        host: 50
      estimates:
        acceleratorMemory:
          unit: "bytes"
          equation: "${base} + ${rate} * ${total-context}"
        hostMemory:
          unit: "bytes"
          equation: "${host}"
    hardwareSupport:
      - hardwareClassId: "fixture-nvidia"
        hardwareRequirements:
          minimumSystemMemoryBytes: "1"
        bindingSizing:
          - bindingId: "fixture-artifact"
"#;
        let source = source.replace(support, sized_support);
        assert!(source.contains("sizing:"), "fixture support replacement");
        fs::write(models.join("fixture-model.yaml"), source).expect("sizing model fixture");
        catalog_root
    }

    fn fixture_target_capabilities() -> TargetCapabilityInventory {
        crate::test_support::execution_target_capabilities(
            dotcore_domain::ExecutionPlatform::linux_amd64(),
        )
        .1
    }

    /// Loads the simulated-site fixture catalog with its engine declared as package
    /// `engines.llama-cpp.native` of the development source for darwin/arm64 with Apple, and its model
    /// requiring an Apple accelerator.
    fn development_source_darwin_catalog(root: &Path) -> Catalog {
        let fixture =
            Path::new(env!("CARGO_MANIFEST_DIR")).join("../fixtures/simulated-site/catalog");
        let catalog = root.join("catalog");
        let rewrite = |file: &str, from: &str, to: &str| {
            let source = fs::read_to_string(fixture.join(file)).expect("fixture catalog file");
            assert!(source.contains(from), "{file} fixture");
            fs::create_dir_all(catalog.join(file).parent().expect("Catalog directory"))
                .expect("Catalog directory");
            fs::write(catalog.join(file), source.replace(from, to)).expect("Catalog file");
        };
        rewrite(
            "engines/fixture.yaml",
            r#"    oci:
      repository:
        registry: { host: "registry.dotcore.test" }
        name: "fixture"
      digest: { algorithm: "sha256", encoded: "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef" }
    selectors:
      - platform: { operatingSystem: "linux", architecture: "amd64" }
        accelerators: ["nvidia"]
        minimumNvidiaDriverMajorVersion: 580"#,
            r#"    nix: { flake: "self", package: "engines.llama-cpp.native", executable: "llama-server" }
    selectors:
      - platform: { operatingSystem: "darwin", architecture: "arm64" }
        accelerators: ["apple"]"#,
        );
        rewrite(
            "models/fixture-model.yaml",
            r#"hardwareClassId: "fixture-nvidia""#,
            r#"hardwareClassId: "fixture-apple""#,
        );
        rewrite(
            "hardware-classes/fixture-nvidia.yaml",
            r#"hardwareClassId: "fixture-nvidia"
displayName: "Fixture NVIDIA"
selector:
  accelerators: ["nvidia"]"#,
            r#"hardwareClassId: "fixture-apple"
displayName: "Fixture Apple"
selector:
  accelerators: ["apple"]"#,
        );
        Catalog::load_with_policy(
            &catalog,
            OciArtifactPolicy::Development,
            crate::model::engine::declaration_validator(),
        )
        .expect("development-source darwin catalog")
    }

    /// The simulated-site fixture catalog plus the OpenAI-compatible upstream template, for tests
    /// that admit both local and upstream deployments.
    fn fixture_catalog_with_upstream_template() -> Catalog {
        let fixture =
            Path::new(env!("CARGO_MANIFEST_DIR")).join("../fixtures/simulated-site/catalog");
        let root = tempfile::tempdir().expect("catalog root");
        for directory in ["engines", "hardware-classes", "models"] {
            let target = root.path().join(directory);
            fs::create_dir_all(&target).expect("catalog directory");
            for entry in fs::read_dir(fixture.join(directory)).expect("fixture directory") {
                let entry = entry.expect("fixture entry");
                fs::copy(entry.path(), target.join(entry.file_name())).expect("fixture file");
            }
        }
        let template = dotcore_catalog::test_support::openai_compatible_template();
        fs::create_dir_all(root.path().join("models/upstream")).expect("template directory");
        fs::write(
            root.path().join("models/upstream/openai-compatible.yaml"),
            format!(
                "upstreamModelTemplateId: {:?}\ndisplayName: {:?}\nwireProtocol: \"UPSTREAM_WIRE_PROTOCOL_OPENAI_COMPATIBLE\"\n",
                template.upstream_model_template_id, template.display_name
            ),
        )
        .expect("upstream template fixture");

        Catalog::load_with_policy(
            root.path(),
            OciArtifactPolicy::Development,
            crate::model::engine::declaration_validator(),
        )
        .expect("fixture catalog with upstream template")
    }

    fn two_node_target_capabilities() -> TargetCapabilityInventory {
        TargetCapabilityReconciler::reconcile_now(
            [
                ("worker-target", "worker-node"),
                ("manager-target", "manager-node"),
            ],
            [
                RuntimePlatformObservation::available(
                    "worker-target",
                    dotcore_domain::ExecutionPlatform::linux_amd64(),
                ),
                RuntimePlatformObservation::available(
                    "manager-target",
                    dotcore_domain::ExecutionPlatform::linux_amd64(),
                ),
            ],
            [
                NodeAcceleratorObservation::nvidia("worker-node", "580.173.02"),
                NodeAcceleratorObservation::nvidia("manager-node", "580.173.02"),
            ],
        )
    }

    /// Route dependency that must block a model deployment's unload.
    #[derive(Clone, Copy, Debug)]
    enum UnloadBlocker {
        SharedRoute,

        ApplicationBinding,
    }

    impl UnloadBlocker {
        /// Adds the blocker around a deployment already exposed through its own sole-member route.
        fn install(
            self,
            repository: &ModelDeclarationRepository,
            site: &SiteRepository,
            deployment_id: &str,
        ) {
            let sole_route_id = site
                .snapshot()
                .expect("Site with exposure route")
                .inference_routes
                .into_iter()
                .find(|route| route.name == "sole-route")
                .expect("sole-member exposure route")
                .inference_route_id;
            match self {
                Self::SharedRoute => {
                    let other = repository
                        .load_upstream_model(
                            &upstream_request("other.example"),
                            &ModelDeploymentExposureIntent::Hidden,
                            None,
                        )
                        .expect("second deployment");
                    site.mutate_inference_routes(|transaction| {
                        transaction.routes.push(InferenceRoute {
                            inference_route_id: "route-shared".to_owned(),
                            name: "shared-route".to_owned(),
                            enabled: true,
                            fallback_groups: vec![FallbackGroup {
                                selection: FallbackGroupSelection::RoundRobin as i32,
                                model_deployment_ids: vec![
                                    deployment_id.to_owned(),
                                    other.model_deployment_id,
                                ],
                            }],
                            generation: 1,
                        });
                        Ok::<(), std::convert::Infallible>(())
                    })
                    .expect("shared route");
                }
                Self::ApplicationBinding => {
                    let placement = ExactPlacement {
                        site_node_id: "fixture-node".to_owned(),
                        execution_target_id: "fixture-application-target".to_owned(),
                    };
                    site.mutate_application_deployments(|deployments| {
                        deployments.push(ApplicationDeploymentDeclaration {
                            lifecycle_generation: 1,
                            application_deployment_id: "application-one".to_owned(),
                            application_deployment_name: "Bound application".to_owned(),
                            placement: Some(placement.clone()),
                            snapshot: Some(ResolvedApplicationDeploymentSnapshot {
                                placement: Some(placement),
                                platform: Some(OciPlatform {
                                    operating_system: "linux".to_owned(),
                                    architecture: "amd64".to_owned(),
                                    variant: None,
                                }),
                                ..Default::default()
                            }),
                            desired_state: ApplicationDeploymentDesiredState::Present as i32,
                            inference_route_id: Some(sole_route_id),
                            ..Default::default()
                        });
                        Ok::<(), std::convert::Infallible>(())
                    })
                    .expect("route-bound application");
                }
            }
        }
    }

    fn test_directory() -> tempfile::TempDir {
        crate::test_support::canonical_tempdir().expect("fixture directory")
    }

    fn lifecycle_high_water(site_path: &Path, deployment_id: &str) -> u64 {
        use prost::Message as _;

        let state_path = site_path.with_file_name("state.pb");
        dotcore_proto::dotcore::site::store::v1::SiteCurrentState::decode(
            fs::read(state_path).expect("retained state").as_slice(),
        )
        .expect("decoded state")
        .manager_model_lifecycle_records
        .into_iter()
        .find(|record| record.deployment_id == deployment_id)
        .expect("lifecycle record")
        .generation_high_water
    }
}
