//! Implements the lifecycle operations the model service exposes for model
//! deployments.
//!
//! Loading, unloading, pausing, and resuming a model and updating its
//! metadata each validate the request, apply the accepted mutation through
//! the desired-state boundary, record the accepted intent, and return the
//! projected deployment. Accepted mutations stay authoritative in memory; the
//! hosting node performs the runtime work from its next desired view, and the
//! upstream reconciler is woken for upstream deployments.

use dotcore_domain::diagnostic::diagnostic_chain;
use dotcore_proto::dotcore::model::v1::{
    CreateUpstreamModelDeploymentRequest, CurrentModelDeployment, ModelDeployment,
    ModelDeploymentDeclaration, ModelDeploymentDesiredState, ModelDeploymentState,
    ModelDeploymentStatus, UpstreamModelDesiredConfiguration,
};
use dotcore_site::{
    node::target_inventory::TargetCapabilityInventory, repository::SiteRepositoryError,
};
use tracing::{Instrument, Span, debug_span, error, field, info, info_span, warn};

use super::state::{
    current_error, desired_error, loading_state, partial_current_error, pausing_state,
    resuming_state, unloading_state, validate_id,
};
use super::{
    CurrentStateRepository, DesiredDeploymentRepository, ManagerInferenceControllerInner,
    ModelLifecycleError,
};
use super::{
    ModelLifecycleController,
    state::{desired_state, is_absent},
};
use crate::model::domain::ModelDeploymentId;

impl<D, S> ModelLifecycleController for super::InferenceController<D, S>
where
    D: super::DesiredDeploymentRepository + 'static,
    S: CurrentStateRepository + 'static,
{
    fn list_deployments(
        &self,
    ) -> impl std::future::Future<
        Output = Result<
            dotcore_proto::dotcore::model::v1::ListModelDeploymentsResponse,
            super::ModelLifecycleError,
        >,
    > + Send {
        let inner = self.inner.clone();
        let span = debug_span!(
            "model_list_deployments",
            operation = "model_list_deployments",
            deployment_count = field::Empty,
            outcome = field::Empty,
            error_kind = field::Empty,
        );
        let operation_span = span.clone();
        async move {
            let result = execute_list_deployments(&inner).await;
            if let Ok(response) = &result {
                operation_span.record("deployment_count", response.deployments.len());
            }
            record_operation_result(&operation_span, &result);
            result
        }
        .instrument(span)
    }

    fn preview_local_model(
        &self,
        request: dotcore_proto::dotcore::model::v1::CreateLocalModelDeploymentRequest,
    ) -> impl std::future::Future<
        Output = Result<
            dotcore_proto::dotcore::model::v1::ResolvedLocalModelDeploymentSnapshot,
            super::ModelLifecycleError,
        >,
    > + Send {
        let inner = self.inner.clone();
        let span = info_span!(
            "model_preview",
            operation = "model_preview",
            model_id = %request.model_id,
            profile_id = %request.profile_id,
            outcome = field::Empty,
            error_kind = field::Empty,
        );
        let operation_span = span.clone();
        async move {
            let result = execute_preview(&inner, &request).await;
            record_operation_result(&operation_span, &result);
            result
        }
        .instrument(span)
    }

    fn estimate_local_model_resources(
        &self,
        model_id: String,
        profile_id: String,
        configuration: dotcore_proto::dotcore::model::v1::LocalModelDeploymentConfiguration,
        placement: Option<dotcore_proto::dotcore::deployment::v1::ExactPlacement>,
    ) -> impl std::future::Future<
        Output = Result<
            dotcore_proto::dotcore::model::v1::EstimateLocalModelResourcesResponse,
            super::ModelLifecycleError,
        >,
    > + Send {
        let inner = self.inner.clone();
        let span = info_span!(
            "model_estimate_resources",
            operation = "model_estimate_resources",
            model_id = %model_id,
            profile_id = %profile_id,
            outcome = field::Empty,
            error_kind = field::Empty,
        );
        let operation_span = span.clone();
        async move {
            let result = execute_estimate(
                &inner,
                &model_id,
                &profile_id,
                &configuration,
                placement.as_ref(),
            )
            .await;
            record_operation_result(&operation_span, &result);
            result
        }
        .instrument(span)
    }

    fn list_profile_compatibility(
        &self,
        model_id: Option<String>,
        target: Option<dotcore_proto::dotcore::deployment::v1::ExactPlacement>,
        scenario: Option<crate::model::sizing_scenario::SizingScenario>,
    ) -> impl std::future::Future<
        Output = Result<
            dotcore_proto::dotcore::model::v1::ListModelProfileCompatibilityResponse,
            super::ModelLifecycleError,
        >,
    > + Send {
        let inner = self.inner.clone();
        // The console polls this read, so its span stays at debug.
        let span = debug_span!(
            "model_profile_compatibility",
            operation = "model_profile_compatibility",
            model_id = model_id.as_deref().unwrap_or_default(),
            outcome = field::Empty,
            error_kind = field::Empty,
        );
        let operation_span = span.clone();
        async move {
            let result = execute_profile_compatibility(
                &inner,
                model_id.as_deref(),
                target.as_ref(),
                scenario.as_ref(),
            )
            .await;
            record_operation_result(&operation_span, &result);
            result
        }
        .instrument(span)
    }

    fn list_model_recommendations(
        &self,
        use_cases: Vec<dotcore_proto::dotcore::catalog::v1::UseCase>,
    ) -> impl std::future::Future<
        Output = Result<
            dotcore_proto::dotcore::model::v1::ListModelRecommendationsResponse,
            super::ModelLifecycleError,
        >,
    > + Send {
        let inner = self.inner.clone();
        // The console polls this read, so its span stays at debug.
        let span = debug_span!(
            "model_recommendations",
            operation = "model_recommendations",
            outcome = field::Empty,
            error_kind = field::Empty,
        );
        let operation_span = span.clone();
        async move {
            let result = execute_model_recommendations(&inner, &use_cases).await;
            record_operation_result(&operation_span, &result);
            result
        }
        .instrument(span)
    }

    fn load_model(
        &self,
        model_id: dotcore_catalog::CatalogModelId,
        profile_id: String,
        placement: dotcore_proto::dotcore::deployment::v1::ExactPlacement,
        configuration: dotcore_proto::dotcore::model::v1::LocalModelDeploymentConfiguration,
        exposure: super::ModelDeploymentExposureIntent,
        name: Option<String>,
    ) -> impl std::future::Future<
        Output = Result<
            dotcore_proto::dotcore::model::v1::ModelDeployment,
            super::ModelLifecycleError,
        >,
    > + Send {
        let inner = self.inner.clone();
        let model_id = model_id.0;
        let span = info_span!(
            "model_load",
            operation = "model_load",
            model_id = %model_id,
            deployment_id = field::Empty,
            outcome = field::Empty,
            error_kind = field::Empty,
        );
        let operation_span = span.clone();
        async move {
            let result = execute_load(
                &inner,
                &model_id,
                &profile_id,
                &placement,
                &configuration,
                &exposure,
                name.as_deref(),
            )
            .await;
            if let Ok(deployment) = &result {
                operation_span.record("deployment_id", deployment.model_deployment_id.as_str());
            }
            record_operation_result(&operation_span, &result);
            result
        }
        .instrument(span)
    }

    fn load_upstream_model(
        &self,
        request: CreateUpstreamModelDeploymentRequest,
        exposure: super::ModelDeploymentExposureIntent,
        name: Option<String>,
    ) -> impl std::future::Future<Output = Result<ModelDeployment, super::ModelLifecycleError>> + Send
    {
        let inner = self.inner.clone();
        let template_id = request.upstream_model_template_id.clone();
        let (exposure_kind, route_name) = exposure_fields(&exposure);
        let span = info_span!(
            "model_load_upstream",
            operation = "model_load_upstream",
            template_id = %template_id,
            exposure = exposure_kind,
            route_name,
            deployment_id = field::Empty,
            outcome = field::Empty,
            error_kind = field::Empty,
            request_url = field::Empty,
            http_status = field::Empty,
        );
        let operation_span = span.clone();
        async move {
            let result = execute_load_upstream(&inner, &request, &exposure, name.as_deref()).await;
            if let Ok(deployment) = &result {
                operation_span.record("deployment_id", deployment.model_deployment_id.as_str());
            }
            record_upstream_operation_result(&operation_span, &result);
            result
        }
        .instrument(span)
    }

    fn update_metadata(
        &self,
        deployment_id: ModelDeploymentId,
        patch: dotcore_domain::deployment::metadata::DeploymentMetadataPatch,
    ) -> impl std::future::Future<Output = Result<ModelDeployment, super::ModelLifecycleError>> + Send
    {
        let inner = self.inner.clone();
        let deployment_id = deployment_id.0;
        let span = info_span!(
            "model_update_metadata",
            operation = "model_update_metadata",
            deployment_id = %deployment_id,
            outcome = field::Empty,
            error_kind = field::Empty,
        );
        let operation_span = span.clone();
        async move {
            let result = execute_update_metadata(&inner, &deployment_id, &patch).await;
            if result.is_ok() {
                info!(event_kind = "model_mutation_accepted", operation = "update_metadata", deployment_id = %deployment_id, "model deployment metadata mutation accepted");
            }
            record_operation_result(&operation_span, &result);
            result
        }
        .instrument(span)
    }

    fn update_upstream_model(
        &self,
        deployment_id: crate::model::domain::ModelDeploymentId,
        replacement: UpstreamModelDesiredConfiguration,
    ) -> impl std::future::Future<Output = Result<ModelDeployment, super::ModelLifecycleError>> + Send
    {
        let inner = self.inner.clone();
        let deployment_id = deployment_id.0;
        let span = info_span!(
            "model_update_upstream",
            operation = "model_update_upstream",
            deployment_id = %deployment_id,
            outcome = field::Empty,
            error_kind = field::Empty,
            request_url = field::Empty,
            http_status = field::Empty,
        );
        let operation_span = span.clone();
        async move {
            let result = execute_update_upstream(&inner, &deployment_id, &replacement).await;
            if result.is_ok() {
                info!(parent: &operation_span, event_kind = "model_mutation_accepted", operation = "update_upstream", deployment_id = %deployment_id, "model deployment mutation accepted");
            }
            record_upstream_operation_result(&operation_span, &result);
            result
        }
        .instrument(span)
    }

    fn unload_model(
        &self,
        deployment_id: crate::model::domain::ModelDeploymentId,
    ) -> impl std::future::Future<
        Output = Result<
            dotcore_proto::dotcore::model::v1::ModelDeployment,
            super::ModelLifecycleError,
        >,
    > + Send {
        let inner = self.inner.clone();
        let deployment_id = deployment_id.0;
        let span = info_span!(
            "model_unload",
            operation = "model_unload",
            deployment_id = %deployment_id,
            outcome = field::Empty,
            error_kind = field::Empty,
        );
        let operation_span = span.clone();
        async move {
            let result = execute_unload(&inner, &deployment_id).await;
            if result.is_ok() {
                info!(parent: &operation_span, event_kind = "model_mutation_accepted", operation = "unload", deployment_id = %deployment_id, desired_state = "teardown", "model deployment mutation accepted");
            }
            record_operation_result(&operation_span, &result);
            result
        }
        .instrument(span)
    }

    fn pause_model(
        &self,
        deployment_id: crate::model::domain::ModelDeploymentId,
    ) -> impl std::future::Future<Output = Result<ModelDeployment, super::ModelLifecycleError>> + Send
    {
        let inner = self.inner.clone();
        let deployment_id = deployment_id.0;
        let span = info_span!(
            "model_pause",
            operation = "model_pause",
            deployment_id = %deployment_id,
            outcome = field::Empty,
            error_kind = field::Empty,
        );
        let operation_span = span.clone();
        async move {
            let result = execute_pause(&inner, &deployment_id).await;
            if result.is_ok() {
                info!(parent: &operation_span, event_kind = "model_mutation_accepted", operation = "pause", deployment_id = %deployment_id, desired_state = "paused", "model deployment mutation accepted");
            }
            record_operation_result(&operation_span, &result);
            result
        }
        .instrument(span)
    }

    fn resume_model(
        &self,
        deployment_id: crate::model::domain::ModelDeploymentId,
    ) -> impl std::future::Future<Output = Result<ModelDeployment, super::ModelLifecycleError>> + Send
    {
        let inner = self.inner.clone();
        let deployment_id = deployment_id.0;
        let span = info_span!(
            "model_resume",
            operation = "model_resume",
            deployment_id = %deployment_id,
            outcome = field::Empty,
            error_kind = field::Empty,
        );
        let operation_span = span.clone();
        async move {
            let result = execute_resume(&inner, &deployment_id).await;
            if result.is_ok() {
                info!(parent: &operation_span, event_kind = "model_mutation_accepted", operation = "resume", deployment_id = %deployment_id, desired_state = "present", "model deployment mutation accepted");
            }
            record_operation_result(&operation_span, &result);
            result
        }
        .instrument(span)
    }

    fn start_observer(&self) {
        super::InferenceController::start_observer(self);
    }
}

async fn execute_list_deployments<D, S>(
    inner: &ManagerInferenceControllerInner<D, S>,
) -> Result<dotcore_proto::dotcore::model::v1::ListModelDeploymentsResponse, ModelLifecycleError>
where
    D: super::DesiredDeploymentRepository,
    S: CurrentStateRepository,
{
    let _operation = inner.operation.lock().await;
    inner.public_projection()
}

async fn execute_preview<D, S>(
    inner: &ManagerInferenceControllerInner<D, S>,
    request: &dotcore_proto::dotcore::model::v1::CreateLocalModelDeploymentRequest,
) -> Result<
    dotcore_proto::dotcore::model::v1::ResolvedLocalModelDeploymentSnapshot,
    ModelLifecycleError,
>
where
    D: super::DesiredDeploymentRepository,
    S: CurrentStateRepository,
{
    let inventory = inner
        .target_capabilities
        .observe()
        .await
        .map_err(ModelLifecycleError::configuration_source)?;
    inner.desired.replace_target_capabilities(inventory);
    let _operation = inner.operation.lock().await;
    inner
        .desired
        .preview_local_model(request)
        .map_err(ModelLifecycleError::from_model_repository)
}

async fn execute_estimate<D, S>(
    inner: &ManagerInferenceControllerInner<D, S>,
    model_id: &str,
    profile_id: &str,
    configuration: &dotcore_proto::dotcore::model::v1::LocalModelDeploymentConfiguration,
    placement: Option<&dotcore_proto::dotcore::deployment::v1::ExactPlacement>,
) -> Result<
    dotcore_proto::dotcore::model::v1::EstimateLocalModelResourcesResponse,
    ModelLifecycleError,
>
where
    D: super::DesiredDeploymentRepository,
    S: CurrentStateRepository,
{
    // Like the compatibility read, an estimate reads observations and deployments without the
    // operation lock or a retained inventory write; only an estimate on a target needs them.
    let Some(placement) = placement else {
        return inner
            .desired
            .estimate_local_model_resources(model_id, profile_id, configuration, None)
            .map_err(ModelLifecycleError::from_model_repository);
    };
    let inventory = inner
        .target_capabilities
        .observe()
        .await
        .map_err(ModelLifecycleError::configuration_source)?;
    let site_workloads = inner
        .site_workloads
        .read()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .clone();
    let workloads = estimate_workloads(site_workloads.as_deref(), &inventory, || {
        Ok(inner.public_projection()?.deployments)
    })?;
    inner
        .desired
        .estimate_local_model_resources(
            model_id,
            profile_id,
            configuration,
            Some(crate::model::repository::EstimateTarget {
                placement,
                inventory: &inventory,
                workloads: &workloads,
            }),
        )
        .map_err(ModelLifecycleError::from_model_repository)
}

/// What each node's deployments hold of its memory for an estimate: every lane's, from the site's
/// workload source once composition installs it, else this controller's own `deployments`, with
/// the memory of other lanes left as other use. Either way the verdicts are the same; the source
/// only names more of the measured use.
fn estimate_workloads(
    site_workloads: Option<&dyn crate::hardware::workloads::SiteWorkloadSource>,
    inventory: &TargetCapabilityInventory,
    deployments: impl FnOnce() -> Result<Vec<ModelDeployment>, ModelLifecycleError>,
) -> Result<crate::hardware::capacity::NodeWorkloads, ModelLifecycleError> {
    if let Some(source) = site_workloads {
        match source.node_workloads(Some(inventory)) {
            Ok(workloads) => return Ok(workloads),
            Err(error) => warn!(
                error = %diagnostic_chain(&error),
                "site workloads are unavailable; the estimate counts other lanes' memory as other use"
            ),
        }
    }
    Ok(crate::model::compatibility::node_workloads(
        &deployments()?,
        Some(inventory),
    ))
}

async fn execute_profile_compatibility<D, S>(
    inner: &ManagerInferenceControllerInner<D, S>,
    model_id: Option<&str>,
    target: Option<&dotcore_proto::dotcore::deployment::v1::ExactPlacement>,
    scenario: Option<&crate::model::sizing_scenario::SizingScenario>,
) -> Result<
    dotcore_proto::dotcore::model::v1::ListModelProfileCompatibilityResponse,
    ModelLifecycleError,
>
where
    D: super::DesiredDeploymentRepository,
    S: CurrentStateRepository,
{
    // Reads observations directly, without the operation lock or a retained inventory write, so
    // polling never serializes with create and delete.
    let inventory = inner
        .target_capabilities
        .observe()
        .await
        .inspect_err(|error| {
            warn!(
                error = %diagnostic_chain(error),
                "target observations are unavailable; profile compatibility reads every target as unknown"
            );
        })
        .ok();
    // Deployments still starting claim their estimate, so memory they are about to take never
    // reads as free.
    let deployments = inner.public_projection()?.deployments;
    let workloads = crate::model::compatibility::node_workloads(&deployments, inventory.as_ref());
    inner
        .desired
        .profile_compatibility(model_id, target, scenario, inventory.as_ref(), &workloads)
        .map_err(ModelLifecycleError::from_model_repository)
}

async fn execute_model_recommendations<D, S>(
    inner: &ManagerInferenceControllerInner<D, S>,
    use_cases: &[dotcore_proto::dotcore::catalog::v1::UseCase],
) -> Result<dotcore_proto::dotcore::model::v1::ListModelRecommendationsResponse, ModelLifecycleError>
where
    D: super::DesiredDeploymentRepository,
    S: CurrentStateRepository,
{
    // Reads observations and deployments as the compatibility read does, without the operation
    // lock or a retained inventory write.
    let inventory = inner
        .target_capabilities
        .observe()
        .await
        .inspect_err(|error| {
            warn!(
                error = %diagnostic_chain(error),
                "target observations are unavailable; model recommendations list nothing"
            );
        })
        .ok();
    let deployments = inner.public_projection()?.deployments;
    inner
        .desired
        .model_recommendations(use_cases, inventory.as_ref(), &deployments)
        .map_err(ModelLifecycleError::from_model_repository)
}

async fn execute_load<D, S>(
    inner: &ManagerInferenceControllerInner<D, S>,
    model_id: &str,
    profile_id: &str,
    placement: &dotcore_proto::dotcore::deployment::v1::ExactPlacement,
    configuration: &dotcore_proto::dotcore::model::v1::LocalModelDeploymentConfiguration,
    exposure: &super::ModelDeploymentExposureIntent,
    name: Option<&str>,
) -> Result<dotcore_proto::dotcore::model::v1::ModelDeployment, ModelLifecycleError>
where
    D: super::DesiredDeploymentRepository,
    S: CurrentStateRepository,
{
    validate_id(model_id)?;
    let inventory = inner
        .target_capabilities
        .observe()
        .await
        .map_err(ModelLifecycleError::configuration_source)?;
    inner.desired.replace_target_capabilities(inventory);
    let _operation = inner.operation.lock().await;
    let desired = match inner.desired.load_model(
        model_id,
        profile_id,
        placement,
        configuration,
        exposure,
        name,
    ) {
        Ok(desired) => desired,
        Err(error) => {
            inner.notify_reconciliation();
            return Err(ModelLifecycleError::from_model_repository(error));
        }
    };
    let state = record_load_intent(inner, &desired)?;
    inner.project_deployment(&desired, state)
}

async fn execute_load_upstream<D, S>(
    inner: &ManagerInferenceControllerInner<D, S>,
    request: &CreateUpstreamModelDeploymentRequest,
    exposure: &super::ModelDeploymentExposureIntent,
    name: Option<&str>,
) -> Result<ModelDeployment, ModelLifecycleError>
where
    D: super::DesiredDeploymentRepository,
    S: CurrentStateRepository,
{
    validate_id(&request.upstream_model_template_id)?;
    let configuration = request
        .desired_configuration
        .as_ref()
        .ok_or(ModelLifecycleError::InvalidArgument)?;
    preflight_upstream_candidate(inner, configuration).await?;
    let _operation = inner.operation.lock().await;
    let desired = match inner.desired.load_upstream_model(request, exposure, name) {
        Ok(desired) => desired,
        Err(error) => {
            inner.notify_reconciliation();
            return Err(ModelLifecycleError::from_model_repository(error));
        }
    };
    let state = record_load_intent(inner, &desired)?;
    inner.project_deployment(&desired, state)
}

async fn execute_update_metadata<D, S>(
    inner: &ManagerInferenceControllerInner<D, S>,
    deployment_id: &str,
    patch: &dotcore_domain::deployment::metadata::DeploymentMetadataPatch,
) -> Result<ModelDeployment, ModelLifecycleError>
where
    D: super::DesiredDeploymentRepository,
    S: CurrentStateRepository,
{
    validate_id(deployment_id)?;
    let _operation = inner.operation.lock().await;
    let desired = inner
        .desired
        .update_metadata(deployment_id, patch)
        .map_err(ModelLifecycleError::from_model_repository)?;
    let state = inner
        .state
        .snapshot()
        .map_err(current_error)?
        .model_deployments
        .into_iter()
        .find(|record| record.model_deployment_id == deployment_id)
        .and_then(|record| record.state)
        .ok_or_else(|| {
            ModelLifecycleError::configuration("desired deployment has no current-state record")
        })?;
    inner.project_deployment(&desired, state)
}

async fn execute_update_upstream<D, S>(
    inner: &ManagerInferenceControllerInner<D, S>,
    deployment_id: &str,
    replacement: &UpstreamModelDesiredConfiguration,
) -> Result<ModelDeployment, ModelLifecycleError>
where
    D: super::DesiredDeploymentRepository,
    S: CurrentStateRepository,
{
    validate_id(deployment_id)?;
    let _operation = inner.operation.lock().await;
    let existing = find_desired(inner, deployment_id)?;
    if !super::is_upstream_deployment(&existing) {
        return Err(ModelLifecycleError::FailedPrecondition);
    }
    let mut candidate = replacement.clone();
    if let Some(current) = existing.upstream_desired_configuration.as_ref() {
        if candidate.endpoint.is_none() {
            candidate.endpoint = current.endpoint.clone();
        }
        if candidate.boundary.is_none() {
            candidate.boundary = current.boundary.clone();
        }
        if candidate.input_capabilities.is_none() {
            candidate.input_capabilities = current.input_capabilities;
        }
        if candidate.upstream_model_id.is_empty() {
            candidate.upstream_model_id = current.upstream_model_id.clone();
        }
    }
    preflight_upstream_candidate(inner, &candidate).await?;
    let desired = match inner
        .desired
        .update_upstream_model(deployment_id, replacement)
    {
        Ok(desired) => desired,
        Err(error) if replacement_persistence_failed(&error) => {
            let desired = find_desired(inner, deployment_id)?;
            inner
                .serving_targets_publisher
                .withdraw_deployment(&ModelDeploymentId(deployment_id.to_owned()));
            record_load_intent(inner, &desired)?;
            return Err(ModelLifecycleError::desired_persistence(error));
        }
        Err(error) => {
            inner.notify_reconciliation();
            return Err(ModelLifecycleError::from_model_repository(error));
        }
    };
    inner
        .serving_targets_publisher
        .withdraw_deployment(&ModelDeploymentId(deployment_id.to_owned()));
    inner.upstream.cancel_health(deployment_id).await;
    let state = record_load_intent(inner, &desired)?;
    inner.project_deployment(&desired, state)
}

async fn preflight_upstream_candidate<D, S>(
    inner: &ManagerInferenceControllerInner<D, S>,
    configuration: &UpstreamModelDesiredConfiguration,
) -> Result<(), ModelLifecycleError>
where
    D: super::DesiredDeploymentRepository,
    S: CurrentStateRepository,
{
    let endpoint = configuration.endpoint.as_ref().ok_or_else(|| {
        invalid_preflight_error("", "The upstream endpoint configuration has no endpoint.")
    })?;
    let mut models = url::Url::parse(&endpoint.base_url).map_err(|error| {
        invalid_preflight_error_with_source(
            &endpoint.base_url,
            "The upstream endpoint URL is invalid.",
            anyhow::Error::new(error).context("Upstream endpoint URL parsing failed"),
        )
    })?;
    if !models.path().ends_with('/') {
        let path = format!("{}/", models.path());
        models.set_path(&path);
    }
    let models = models.join("models").map_err(|error| {
        invalid_preflight_error_with_source(
            &endpoint.base_url,
            "The upstream endpoint URL cannot accept the /models path.",
            anyhow::Error::new(error).context("Upstream endpoint URL join failed"),
        )
    })?;
    if !matches!(models.scheme(), "https" | "http")
        || models.host().is_none()
        || !models.username().is_empty()
        || models.password().is_some()
        || models.query().is_some()
        || models.fragment().is_some()
        || (models.scheme() == "http" && !endpoint.allow_insecure_http)
        || (models.scheme() == "https" && endpoint.allow_insecure_http)
    {
        return Err(invalid_preflight_error(
            &endpoint.base_url,
            "The upstream endpoint URL or transport security configuration is invalid.",
        ));
    }
    let boundary = configuration.boundary.as_ref().ok_or_else(|| {
        invalid_preflight_error(
            &endpoint.base_url,
            "The upstream endpoint configuration has no trust boundary.",
        )
    })?;
    if boundary.trust_domain.trim().is_empty() {
        return Err(invalid_preflight_error(
            &endpoint.base_url,
            "The upstream endpoint trust domain is blank.",
        ));
    }
    let authorization = if configuration.api_key.is_empty() {
        String::new()
    } else {
        format!("Bearer {}", configuration.api_key)
    };
    let result = inner
        .upstream
        .verify(crate::model::upstream::VerificationRequest {
            url: models.clone(),
            authorization,
            expectation: crate::model::upstream::VerificationExpectation::Health,
            expected_model_id: None,
        })
        .await;
    match result {
        Ok(observation) if (200..300).contains(&observation.status) => Ok(()),
        Ok(observation) => Err(ModelLifecycleError::upstream_preflight(
            "The endpoint rejected the verification request",
            "HTTP response received",
            models.as_str(),
            Some(u32::from(observation.status)),
            Some(observation.body.into_boxed_slice()),
            None,
        )),
        Err(error) => {
            let (summary, primary_cause) = preflight_failure_message(&error);
            let source =
                anyhow::Error::new(error).context("Upstream endpoint preflight GET /models failed");
            Err(ModelLifecycleError::upstream_preflight(
                summary,
                primary_cause,
                models.as_str(),
                None,
                None,
                Some(source),
            ))
        }
    }
}

fn invalid_preflight_error(request_url: &str, cause: &'static str) -> ModelLifecycleError {
    invalid_preflight_error_with_optional_source(request_url, cause, None)
}

fn invalid_preflight_error_with_source(
    request_url: &str,
    cause: &'static str,
    source: anyhow::Error,
) -> ModelLifecycleError {
    invalid_preflight_error_with_optional_source(request_url, cause, Some(source))
}

fn invalid_preflight_error_with_optional_source(
    request_url: &str,
    cause: &'static str,
    source: Option<anyhow::Error>,
) -> ModelLifecycleError {
    ModelLifecycleError::upstream_preflight(
        "The endpoint configuration is invalid",
        cause,
        request_url,
        None,
        None,
        source,
    )
}

fn preflight_failure_message(
    error: &crate::model::upstream::VerificationTransportError,
) -> (&'static str, &'static str) {
    use crate::model::upstream::{TlsFailure, VerificationTransportError};

    match error {
        VerificationTransportError::Cancelled => (
            "Upstream endpoint verification was cancelled",
            "verification cancelled",
        ),
        VerificationTransportError::Timeout => (
            "The endpoint did not respond before the verification timeout",
            "verification timed out",
        ),
        VerificationTransportError::Dns => (
            "The endpoint host could not be resolved",
            "DNS resolution failed",
        ),
        VerificationTransportError::Tls(TlsFailure::CertificateIdentity) => (
            "The endpoint TLS certificate does not match its hostname",
            "TLS certificate identity check failed",
        ),
        VerificationTransportError::Tls(TlsFailure::Other) => (
            "TLS verification of the endpoint failed",
            "TLS verification failed",
        ),
        VerificationTransportError::Network => (
            "The endpoint could not be reached",
            "network connection failed",
        ),
        VerificationTransportError::BodyTooLarge => (
            "The endpoint response exceeded the verification limit",
            "response body exceeded the verification limit",
        ),
        VerificationTransportError::InvalidResponse => (
            "The endpoint returned an invalid response",
            "invalid provider response",
        ),
    }
}

/// Moves a deployment to teardown under the operation fence. Its hosting node, or the upstream
/// reconciler for an upstream deployment, completes the teardown from the changed intent.
async fn execute_unload<D, S>(
    inner: &ManagerInferenceControllerInner<D, S>,
    deployment_id: &str,
) -> Result<ModelDeployment, ModelLifecycleError>
where
    D: super::DesiredDeploymentRepository,
    S: CurrentStateRepository,
{
    validate_id(deployment_id)?;
    let _operation = inner.operation.lock().await;
    let withdraw = || {
        inner
            .serving_targets_publisher
            .withdraw_deployment(&ModelDeploymentId(deployment_id.to_owned()))
    };
    if let Some(desired) = inner.retryable_unload(deployment_id)? {
        withdraw();
        let state = record_unload_intent(inner, deployment_id)?;
        return inner.project_deployment(&desired, state);
    }
    let upstream = super::is_upstream_deployment(&find_desired(inner, deployment_id)?);
    let desired = match inner.desired.unload_model(deployment_id) {
        Ok(desired) => desired,
        Err(error) => {
            let lifecycle_error = ModelLifecycleError::from_model_repository(error);
            if find_desired(inner, deployment_id)
                .and_then(|desired| desired_state(&desired))
                .is_ok_and(|state| state == ModelDeploymentDesiredState::Teardown)
            {
                withdraw();
            }
            inner.notify_reconciliation();
            return Err(lifecycle_error);
        }
    };
    withdraw();
    if upstream {
        inner.upstream.cancel_health(deployment_id).await;
    }
    let state = record_unload_intent(inner, deployment_id)?;
    inner.project_deployment(&desired, state)
}

async fn execute_pause<D, S>(
    inner: &ManagerInferenceControllerInner<D, S>,
    deployment_id: &str,
) -> Result<ModelDeployment, ModelLifecycleError>
where
    D: super::DesiredDeploymentRepository,
    S: CurrentStateRepository,
{
    validate_id(deployment_id)?;
    let _operation = inner.operation.lock().await;
    let desired = find_desired(inner, deployment_id)?;
    match desired_state(&desired)? {
        ModelDeploymentDesiredState::Paused => {
            return record_existing_pause_intent(inner, &desired);
        }
        ModelDeploymentDesiredState::Present => {}
        ModelDeploymentDesiredState::Teardown | ModelDeploymentDesiredState::Unspecified => {
            return Err(ModelLifecycleError::FailedPrecondition);
        }
    }
    let (desired, persistence_error) = match inner.desired.pause_model(deployment_id) {
        Ok(desired) => (desired, None),
        Err(error) => {
            let persistence_error = ModelLifecycleError::from_model_repository(error);
            let desired = find_desired(inner, deployment_id)?;
            if desired_state(&desired)? != ModelDeploymentDesiredState::Paused {
                inner.notify_reconciliation();
                return Err(persistence_error);
            }
            (desired, Some(persistence_error))
        }
    };
    accept_pause_intent(inner, &desired, true)?;
    if let Some(error) = persistence_error {
        return Err(error);
    }
    let state = pause_state(inner, &desired)?;
    inner.project_deployment(&desired, state)
}

async fn execute_resume<D, S>(
    inner: &ManagerInferenceControllerInner<D, S>,
    deployment_id: &str,
) -> Result<ModelDeployment, ModelLifecycleError>
where
    D: super::DesiredDeploymentRepository,
    S: CurrentStateRepository,
{
    validate_id(deployment_id)?;
    let _operation = inner.operation.lock().await;
    let existing = find_desired(inner, deployment_id)?;
    match desired_state(&existing)? {
        ModelDeploymentDesiredState::Present => {
            if resume_current_record_requires_repair(inner, &existing)? {
                let state = record_resume_intent(inner, &existing)?;
                return inner.project_deployment(&existing, state);
            }
            return project_existing(inner, &existing);
        }
        ModelDeploymentDesiredState::Teardown | ModelDeploymentDesiredState::Unspecified => {
            return Err(ModelLifecycleError::FailedPrecondition);
        }
        ModelDeploymentDesiredState::Paused => {}
    }
    let (desired, persistence_error) = match inner.desired.resume_model(deployment_id) {
        Ok(desired) => (desired, None),
        Err(error) => {
            let persistence_error = ModelLifecycleError::from_model_repository(error);
            let desired = find_desired(inner, deployment_id)?;
            if desired_state(&desired)? != ModelDeploymentDesiredState::Present {
                inner.notify_reconciliation();
                return Err(persistence_error);
            }
            (desired, Some(persistence_error))
        }
    };
    let state = record_resume_intent(inner, &desired);
    if let Some(error) = persistence_error {
        return Err(error);
    }
    let state = state?;
    inner.project_deployment(&desired, state)
}

fn replacement_persistence_failed(
    error: &crate::model::repository::ModelDeclarationRepositoryError,
) -> bool {
    matches!(
        error,
        crate::model::repository::ModelDeclarationRepositoryError::Site(
            SiteRepositoryError::Storage(_)
        )
    )
}

fn find_desired<D, S>(
    inner: &ManagerInferenceControllerInner<D, S>,
    deployment_id: &str,
) -> Result<ModelDeploymentDeclaration, ModelLifecycleError>
where
    D: super::DesiredDeploymentRepository,
    S: CurrentStateRepository,
{
    inner
        .desired
        .model_deployments()
        .map_err(desired_error)?
        .into_iter()
        .find(|deployment| deployment.model_deployment_id == deployment_id)
        .ok_or(ModelLifecycleError::NotFound)
}

fn project_existing<D, S>(
    inner: &ManagerInferenceControllerInner<D, S>,
    desired: &ModelDeploymentDeclaration,
) -> Result<ModelDeployment, ModelLifecycleError>
where
    D: super::DesiredDeploymentRepository,
    S: CurrentStateRepository,
{
    let state = inner
        .state
        .snapshot()
        .map_err(current_error)?
        .model_deployments
        .into_iter()
        .find(|record| record.model_deployment_id == desired.model_deployment_id)
        .and_then(|record| record.state)
        .ok_or_else(|| {
            ModelLifecycleError::configuration("desired deployment has no current-state record")
        })?;
    inner.project_deployment(desired, state)
}

fn resume_current_record_requires_repair<D, S>(
    inner: &ManagerInferenceControllerInner<D, S>,
    desired: &ModelDeploymentDeclaration,
) -> Result<bool, ModelLifecycleError>
where
    D: super::DesiredDeploymentRepository,
    S: CurrentStateRepository,
{
    let lifecycle = inner
        .state
        .snapshot()
        .map_err(current_error)?
        .model_deployments
        .into_iter()
        .find(|record| record.model_deployment_id == desired.model_deployment_id)
        .and_then(|record| record.state)
        .and_then(|state| ModelDeploymentStatus::try_from(state.status).ok());
    Ok(matches!(
        lifecycle,
        Some(ModelDeploymentStatus::Pausing | ModelDeploymentStatus::Paused)
    ))
}

fn record_existing_pause_intent<D, S>(
    inner: &ManagerInferenceControllerInner<D, S>,
    desired: &ModelDeploymentDeclaration,
) -> Result<ModelDeployment, ModelLifecycleError>
where
    D: super::DesiredDeploymentRepository,
    S: CurrentStateRepository,
{
    accept_pause_intent(inner, desired, false)?;
    let state = pause_state(inner, desired)?;
    inner.project_deployment(desired, state)
}

fn accept_pause_intent<D, S>(
    inner: &ManagerInferenceControllerInner<D, S>,
    desired: &ModelDeploymentDeclaration,
    force_current_record: bool,
) -> Result<(), ModelLifecycleError>
where
    D: super::DesiredDeploymentRepository,
    S: CurrentStateRepository,
{
    inner
        .serving_targets_publisher
        .withdraw_deployment(&ModelDeploymentId(desired.model_deployment_id.clone()));
    let result = if force_current_record || pause_current_record_required(inner, desired)? {
        record_pause_intent(inner, desired).map(|_| ())
    } else {
        Ok(())
    };
    inner.notify_reconciliation();
    result
}

fn pause_current_record_required<D, S>(
    inner: &ManagerInferenceControllerInner<D, S>,
    desired: &ModelDeploymentDeclaration,
) -> Result<bool, ModelLifecycleError>
where
    D: super::DesiredDeploymentRepository,
    S: CurrentStateRepository,
{
    let existing = inner
        .state
        .snapshot()
        .map_err(current_error)?
        .model_deployments
        .into_iter()
        .find(|record| record.model_deployment_id == desired.model_deployment_id)
        .and_then(|record| record.state);
    Ok(!existing.is_some_and(|state| {
        matches!(
            dotcore_proto::dotcore::model::v1::ModelDeploymentStatus::try_from(state.status),
            Ok(
                dotcore_proto::dotcore::model::v1::ModelDeploymentStatus::Pausing
                    | dotcore_proto::dotcore::model::v1::ModelDeploymentStatus::Paused
                    | dotcore_proto::dotcore::model::v1::ModelDeploymentStatus::Unavailable
            )
        )
    }))
}

fn pause_state<D, S>(
    inner: &ManagerInferenceControllerInner<D, S>,
    desired: &ModelDeploymentDeclaration,
) -> Result<dotcore_proto::dotcore::model::v1::ModelDeploymentState, ModelLifecycleError>
where
    D: super::DesiredDeploymentRepository,
    S: CurrentStateRepository,
{
    inner
        .state
        .snapshot()
        .map_err(current_error)?
        .model_deployments
        .into_iter()
        .find(|record| record.model_deployment_id == desired.model_deployment_id)
        .and_then(|record| record.state)
        .ok_or_else(|| {
            ModelLifecycleError::configuration(
                "paused desired deployment has no current-state record",
            )
        })
}

fn exposure_fields(exposure: &super::ModelDeploymentExposureIntent) -> (&'static str, &str) {
    match exposure {
        super::ModelDeploymentExposureIntent::Direct { route_name } => ("direct", route_name),
        super::ModelDeploymentExposureIntent::Hidden => ("hidden", ""),
    }
}

fn record_upstream_operation_result<T>(span: &Span, result: &Result<T, ModelLifecycleError>) {
    let Err(ModelLifecycleError::UpstreamPreflight {
        summary,
        primary_cause,
        request_url,
        http_status,
        response_body,
        source,
    }) = result
    else {
        record_operation_result(span, result);
        return;
    };
    span.record("outcome", "failed");
    span.record("error_kind", "upstream_preflight");
    span.record("request_url", request_url.as_ref());
    if let Some(status) = http_status {
        span.record("http_status", status);
    }
    error!(
        parent: span,
        event_kind = "upstream_preflight_failed",
        error_kind = "upstream_preflight",
        request_url = %request_url,
        http_status = ?http_status,
        summary,
        primary_cause,
        response_body_size = response_body.as_ref().map_or(0, |body| body.len()),
        response_body_excerpt = %response_body
            .as_deref()
            .map(super::response_excerpt)
            .unwrap_or_default(),
        source = ?source,
        "upstream model preflight failed"
    );
}

fn record_operation_result<T>(span: &Span, result: &Result<T, ModelLifecycleError>) {
    match result {
        Ok(_) => {
            span.record("outcome", "success");
        }
        Err(error) => {
            let cause = diagnostic_chain(error);
            match error.category() {
                super::ModelLifecycleErrorCategory::InvalidArgument
                | super::ModelLifecycleErrorCategory::NotFound
                | super::ModelLifecycleErrorCategory::AlreadyExists
                | super::ModelLifecycleErrorCategory::FailedPrecondition => {
                    span.record("outcome", "rejected");
                    span.record("error_kind", "request");
                    warn!(
                        parent: span,
                        error_kind = "request",
                        error = %cause,
                        "model request rejected"
                    );
                }
                super::ModelLifecycleErrorCategory::Configuration => {
                    span.record("outcome", "failed");
                    span.record("error_kind", "configuration");
                    error!(
                        parent: span,
                        error_kind = "configuration",
                        error = %cause,
                        "model configuration failed"
                    );
                }
                super::ModelLifecycleErrorCategory::OutcomeUncertain => {
                    span.record("outcome", "uncertain");
                    span.record("error_kind", "outcome_uncertain");
                    error!(
                        parent: span,
                        error_kind = "outcome_uncertain",
                        error = %cause,
                        "model outcome is uncertain"
                    );
                }
            }
        }
    }
}

fn record_load_intent<D, S>(
    inner: &ManagerInferenceControllerInner<D, S>,
    desired: &ModelDeploymentDeclaration,
) -> Result<ModelDeploymentState, ModelLifecycleError>
where
    D: DesiredDeploymentRepository,
    S: CurrentStateRepository,
{
    let state = record_intent(inner, &desired.model_deployment_id, "Load", |_| {
        (loading_state(), true)
    })?;
    info!(
        event_kind = "model_mutation_accepted",
        operation = "load",
        deployment_id = %desired.model_deployment_id,
        desired_state = "present",
        "model deployment mutation accepted",
    );
    inner.notify_reconciliation();
    Ok(state)
}

fn record_pause_intent<D, S>(
    inner: &ManagerInferenceControllerInner<D, S>,
    desired: &ModelDeploymentDeclaration,
) -> Result<ModelDeploymentState, ModelLifecycleError>
where
    D: DesiredDeploymentRepository,
    S: CurrentStateRepository,
{
    record_intent(inner, &desired.model_deployment_id, "Pause", |existing| {
        (pausing_state(existing), true)
    })
}

fn record_resume_intent<D, S>(
    inner: &ManagerInferenceControllerInner<D, S>,
    desired: &ModelDeploymentDeclaration,
) -> Result<ModelDeploymentState, ModelLifecycleError>
where
    D: DesiredDeploymentRepository,
    S: CurrentStateRepository,
{
    let result = record_intent(inner, &desired.model_deployment_id, "Resume", |_| {
        (resuming_state(desired.lifecycle_generation), true)
    });
    inner.notify_reconciliation();
    result
}

fn record_unload_intent<D, S>(
    inner: &ManagerInferenceControllerInner<D, S>,
    deployment_id: &str,
) -> Result<ModelDeploymentState, ModelLifecycleError>
where
    D: DesiredDeploymentRepository,
    S: CurrentStateRepository,
{
    let state = record_intent(inner, deployment_id, "Unload", |existing| {
        let unloading =
            existing.is_some_and(|value| value.status == ModelDeploymentStatus::Unloading as i32);
        (unloading_state(existing), !unloading)
    })?;
    inner.notify_reconciliation();
    Ok(state)
}

/// Records one accepted lifecycle intent in its deployment's current record.
///
/// The latest record is read and conditionally replaced under the declaration guard, the guard
/// under which node status acceptance and teardown finalization also write current records, so
/// neither can land between the read and the write. `intent` maps the latest recorded state to
/// the state to report and whether to write it; a deployment no longer declared gets no record.
fn record_intent<D, S>(
    inner: &ManagerInferenceControllerInner<D, S>,
    deployment_id: &str,
    operation: &str,
    intent: impl Fn(Option<&ModelDeploymentState>) -> (ModelDeploymentState, bool),
) -> Result<ModelDeploymentState, ModelLifecycleError>
where
    D: DesiredDeploymentRepository,
    S: CurrentStateRepository,
{
    let mut recorded = None;
    inner
        .desired
        .with_declared_deployment_ids(&mut |declared| {
            let existing = inner
                .state
                .snapshot()
                .map_err(current_error)?
                .model_deployments
                .into_iter()
                .find(|record| record.model_deployment_id == deployment_id);
            let (state, write) = intent(existing.as_ref().and_then(|record| record.state.as_ref()));
            if write && declared.contains(deployment_id) {
                inner
                    .state
                    .upsert(
                        CurrentModelDeployment {
                            model_deployment_id: deployment_id.to_owned(),
                            reconciled_lifecycle_generation: None,
                            record_revision: 0,
                            recovery_barrier: None,
                            state: Some(state.clone()),
                        },
                        existing.map(|record| record.record_revision),
                    )
                    .map_err(|error| partial_current_error(operation, error))?;
            }
            recorded = Some(state);
            Ok(())
        })?;
    recorded.ok_or_else(|| {
        ModelLifecycleError::configuration("the declaration guard did not run its read")
    })
}

impl<D, S> ManagerInferenceControllerInner<D, S>
where
    D: DesiredDeploymentRepository,
    S: CurrentStateRepository,
{
    fn retryable_unload(
        &self,
        deployment_id: &str,
    ) -> Result<Option<ModelDeploymentDeclaration>, ModelLifecycleError> {
        let deployments = self.desired.model_deployments().map_err(desired_error)?;
        Ok(deployments
            .into_iter()
            .find(|deployment| deployment.model_deployment_id == deployment_id)
            .filter(is_absent))
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use dotcore_catalog::CatalogModelId;
    use dotcore_domain::test_support::tracing::{Record, capture_records, capture_records_async};
    use dotcore_proto::dotcore::model::v1::{
        CreateLocalModelDeploymentRequest, CreateUpstreamModelDeploymentRequest,
        LocalModelDeploymentConfiguration, ModelDeploymentDesiredState, ModelDeploymentStatus,
        ScalarResourceEstimate, UpstreamModelDesiredConfiguration,
    };

    use super::super::state::pausing_state;
    use super::super::{
        InferenceController, ModelDeploymentExposureIntent, ModelLifecycleController,
        ModelLifecycleError, ModelRealizationInput,
    };
    use crate::model::{
        controller::{
            serving_targets::{ServingTarget, node_target_for_test},
            test_support::{
                TestCurrent, TestDesired, TestFailure, assert_result, current_deployment,
                test_placement, upstream_desired_deployment,
            },
        },
        domain::ModelDeploymentId,
        state::CurrentStateRepository,
        upstream::{ScriptedVerificationHttp, VerificationObservation, VerificationTransportError},
    };

    #[test]
    fn an_estimate_counts_every_lane_from_the_site_workloads_once_they_are_installed() {
        use std::cell::Cell;

        use dotcore_domain::MemoryArchitecture;
        use dotcore_site::node::target_inventory::TargetCapabilityInventory;

        use crate::hardware::{
            capacity::{Estimate, NodeWorkloads, Workload, WorkloadKey, WorkloadPhase},
            workloads::{SiteWorkloadSource, SiteWorkloadsError},
        };

        struct Fixed(Option<NodeWorkloads>);

        impl SiteWorkloadSource for Fixed {
            fn node_workloads(
                &self,
                _inventory: Option<&TargetCapabilityInventory>,
            ) -> Result<NodeWorkloads, SiteWorkloadsError> {
                self.0
                    .clone()
                    .ok_or_else(|| SiteWorkloadsError::new("applications unreadable"))
            }
        }

        let application = NodeWorkloads::from([(
            "node-one".to_owned(),
            vec![Workload {
                key: WorkloadKey::applications("app"),
                phase: WorkloadPhase::Other,
                expects_use: true,
                memory_architecture: MemoryArchitecture::Dedicated,
                device_ids: vec!["GPU-a".to_owned()],
                estimate: Estimate::default(),
                attributed: Some([("GPU-a".to_owned(), 1024)].into()),
            }],
        )]);
        let inventory = TargetCapabilityInventory::empty(
            dotcore_domain::timing::Timing::production()
                .node_control
                .capability_max_age,
            std::time::SystemTime::now(),
        );
        let own_reads = Cell::new(0);
        let estimate_workloads = |source: Option<&dyn SiteWorkloadSource>| {
            super::estimate_workloads(source, &inventory, || {
                own_reads.set(own_reads.get() + 1);
                Ok(Vec::new())
            })
            .expect("estimate workloads")
        };

        // Before composition installs the source, only this controller's deployments count.
        assert_eq!(estimate_workloads(None), NodeWorkloads::new());
        assert_eq!(own_reads.get(), 1);
        assert_eq!(
            estimate_workloads(Some(&Fixed(Some(application.clone())))),
            application
        );
        assert_eq!(own_reads.get(), 1);
        // A source that can't answer leaves the estimate to this controller's deployments.
        assert_eq!(estimate_workloads(Some(&Fixed(None))), NodeWorkloads::new());
        assert_eq!(own_reads.get(), 2);
    }

    #[test]
    fn operation_failure_records_the_cause_for_every_category() {
        let cases = [
            (
                ModelLifecycleError::ModelProfileNotFound {
                    model_id: "fixture-model".to_owned(),
                    profile_id: "fixture-profile".to_owned(),
                },
                "request",
                "WARN",
                "Catalog model \"fixture-model\" has no profile \"fixture-profile\"",
            ),
            (
                ModelLifecycleError::ModelRepository(
                    crate::model::repository::ModelDeclarationRepositoryError::InvalidState(
                        "profile \"nvfp4-text\" requires complete engine parameter defaults",
                    ),
                ),
                "configuration",
                "ERROR",
                "Catalog model state is invalid: profile \"nvfp4-text\" requires complete engine parameter defaults",
            ),
            (
                ModelLifecycleError::OutcomeUncertain(anyhow::anyhow!("persistence uncertain")),
                "outcome_uncertain",
                "ERROR",
                "Model lifecycle outcome is uncertain: persistence uncertain",
            ),
        ];

        for (error, error_kind, level, expected_cause) in cases {
            let ((), records, _) = capture_records(|| {
                let span = tracing::info_span!(
                    "model_load",
                    operation = "model_load",
                    outcome = tracing::field::Empty,
                    error_kind = tracing::field::Empty,
                );
                super::record_operation_result(&span, &Err::<(), _>(error));
            });

            let failure_events = failure_events(&records);
            assert_eq!(failure_events.len(), 1, "{error_kind}: {records:#?}");
            assert!(
                matches!(
                    failure_events.as_slice(),
                    [Record::Event { level: recorded_level, fields }]
                        if fields.get("error_kind") == Some(&format!("\"{error_kind}\""))
                            && recorded_level.as_str() == level
                            && fields.get("error") == Some(&expected_cause.to_owned())
                ),
                "{error_kind}: {failure_events:#?}"
            );
        }
    }

    #[tokio::test]
    async fn rejected_preview_records_one_failure_event_with_the_cause() {
        let calls = Arc::new(Mutex::new(Vec::new()));
        let desired = TestDesired::new(calls.clone());
        let current = TestCurrent::new(calls.clone());
        let controller = InferenceController::new(
            desired,
            current,
            crate::model::controller::test_support::test_target_capability_source(),
            dotcore_domain::timing::Timing::production().model,
            std::sync::Arc::new(dotcore_domain::clock::SystemClock),
        )
        .expect("controller");

        let (result, records, _) = capture_records_async(|| {
            controller.preview_local_model(CreateLocalModelDeploymentRequest {
                model_id: "fixture-model".to_owned(),
                profile_id: "fixture-profile".to_owned(),
                placement: Some(test_placement()),
                configuration: Some(LocalModelDeploymentConfiguration::default()),
            })
        })
        .await;

        assert!(matches!(
            result,
            Err(ModelLifecycleError::ModelRepository(_))
        ));
        let failure_events = failure_events(&records);
        assert_eq!(failure_events.len(), 1, "{records:#?}");
        assert!(
            matches!(
                failure_events.as_slice(),
                [Record::Event { level, fields }]
                    if level.as_str() == "ERROR"
                        && fields.get("error_kind") == Some(&"\"configuration\"".to_owned())
                        && fields.get("error")
                            == Some(&"Catalog model state is invalid: local deployment preview is unavailable".to_owned())
            ),
            "{failure_events:#?}"
        );
        assert!(records.iter().any(|record| matches!(
            record,
            Record::SpanClosed { name, fields }
                if name == "model_preview"
                    && fields.get("model_id") == Some(&"fixture-model".to_owned())
                    && fields.get("profile_id") == Some(&"fixture-profile".to_owned())
                    && fields.get("outcome") == Some(&"\"failed\"".to_owned())
        )));
    }

    #[tokio::test]
    async fn rejected_estimate_records_one_failure_event_with_the_cause() {
        let calls = Arc::new(Mutex::new(Vec::new()));
        let desired = TestDesired::new(calls.clone());
        let current = TestCurrent::new(calls.clone());
        let controller = InferenceController::new(
            desired.clone(),
            current,
            crate::model::controller::test_support::test_target_capability_source(),
            dotcore_domain::timing::Timing::production().model,
            std::sync::Arc::new(dotcore_domain::clock::SystemClock),
        )
        .expect("controller");
        desired.fail_next(TestFailure::Failed);

        let (result, records, _) = capture_records_async(|| {
            controller.estimate_local_model_resources(
                "fixture-model".to_owned(),
                "fixture-profile".to_owned(),
                LocalModelDeploymentConfiguration::default(),
                None,
            )
        })
        .await;

        assert!(result.is_err());
        let failure_events = failure_events(&records);
        assert_eq!(failure_events.len(), 1, "{records:#?}");
        assert!(
            matches!(
                failure_events.as_slice(),
                [Record::Event { level, fields }]
                    if level.as_str() == "ERROR"
                        && fields.get("error_kind") == Some(&"\"configuration\"".to_owned())
                        && fields.get("error")
                            == Some(&"Desired site storage failed: site storage failed: injected desired write failure".to_owned())
            ),
            "{failure_events:#?}"
        );
        assert!(records.iter().any(|record| matches!(
            record,
            Record::SpanClosed { name, fields }
                if name == "model_estimate_resources"
                    && fields.get("model_id") == Some(&"fixture-model".to_owned())
                    && fields.get("outcome") == Some(&"\"failed\"".to_owned())
        )));
    }

    fn failure_events(records: &[Record]) -> Vec<&Record> {
        records
            .iter()
            .filter(|record| {
                matches!(
                    record,
                    Record::Event { fields, .. }
                        if fields.contains_key("error_kind") && fields.contains_key("error")
                )
            })
            .collect()
    }

    #[tokio::test]
    async fn resource_estimation_uses_only_the_pure_desired_catalog_boundary() {
        let calls = Arc::new(Mutex::new(Vec::new()));
        let desired = TestDesired::new(calls.clone());
        let current = TestCurrent::new(calls.clone());
        let controller = InferenceController::new(
            desired.clone(),
            current.clone(),
            crate::model::controller::test_support::test_target_capability_source(),
            dotcore_domain::timing::Timing::production().model,
            std::sync::Arc::new(dotcore_domain::clock::SystemClock),
        )
        .expect("controller");
        let desired_before = desired.state.lock().expect("desired state").clone();
        let current_before = current.state.snapshot().expect("current state");
        calls.lock().expect("calls").clear();

        let estimates = controller
            .estimate_local_model_resources(
                "fixture-model".to_owned(),
                "fixture-profile".to_owned(),
                LocalModelDeploymentConfiguration::default(),
                None,
            )
            .await
            .expect("resource estimates");

        assert_eq!(
            estimates.hardware_class_estimates[0].estimates,
            vec![ScalarResourceEstimate {
                resource: "acceleratorMemory".to_owned(),
                value: 112,
                unit: "bytes".to_owned(),
            }]
        );
        assert_eq!(*calls.lock().expect("calls"), ["desired estimate"]);
        assert_eq!(
            *desired.state.lock().expect("desired state"),
            desired_before
        );
        assert_eq!(
            current.state.snapshot().expect("current state"),
            current_before
        );
    }

    #[tokio::test]
    async fn upstream_http_preflight_failure_stays_source_less() {
        let calls = Arc::new(Mutex::new(Vec::new()));
        let desired = TestDesired::new(calls.clone());
        let current = TestCurrent::new(calls);
        let controller = InferenceController::new_with_verification(
            desired,
            current,
            Arc::new(ScriptedVerificationHttp::new([Ok(
                VerificationObservation {
                    status: 503,
                    minimum_retry: None,
                    body: b"temporarily unavailable".to_vec(),
                },
            )])),
        )
        .expect("controller");
        let configuration = upstream_desired_deployment()
            .upstream_desired_configuration
            .expect("upstream configuration");

        let (result, records, _) = capture_records_async(|| {
            controller.load_upstream_model(
                CreateUpstreamModelDeploymentRequest {
                    upstream_model_template_id: "openai-compatible".to_owned(),
                    desired_configuration: Some(configuration),
                },
                ModelDeploymentExposureIntent::Direct {
                    route_name: "external".to_owned(),
                },
                None,
            )
        })
        .await;

        let Err(ModelLifecycleError::UpstreamPreflight {
            summary,
            request_url,
            http_status,
            response_body,
            source,
            ..
        }) = result
        else {
            panic!("HTTP preflight failure")
        };
        assert_eq!(summary, "The endpoint rejected the verification request");
        assert_eq!(request_url.as_ref(), "https://upstream.test/v1/models");
        assert_eq!(http_status, Some(503));
        assert_eq!(
            response_body.as_deref(),
            Some(b"temporarily unavailable".as_slice())
        );
        assert!(source.is_none());
        assert!(
            records.iter().any(|record| matches!(
                record,
                Record::Event { fields, .. }
                    if fields.get("event_kind") == Some(&"\"upstream_preflight_failed\"".to_owned())
                        && fields.get("response_body_size") == Some(&"23".to_owned())
                        && fields.get("response_body_excerpt")
                            == Some(&"temporarily unavailable".to_owned())
            )),
            "{records:#?}"
        );
    }

    #[tokio::test]
    async fn invalid_upstream_endpoint_retains_its_url_parse_error() {
        let calls = Arc::new(Mutex::new(Vec::new()));
        let desired = TestDesired::new(calls.clone());
        let current = TestCurrent::new(calls);
        let controller = InferenceController::new_with_verification(
            desired,
            current,
            Arc::new(ScriptedVerificationHttp::new([])),
        )
        .expect("controller");
        let mut configuration = upstream_desired_deployment()
            .upstream_desired_configuration
            .expect("upstream configuration");
        configuration.endpoint.as_mut().expect("endpoint").base_url =
            "https://[not-an-ipv6-address\n\r\twith control whitespace".to_owned();

        let result = controller
            .load_upstream_model(
                CreateUpstreamModelDeploymentRequest {
                    upstream_model_template_id: "openai-compatible".to_owned(),
                    desired_configuration: Some(configuration),
                },
                ModelDeploymentExposureIntent::Direct {
                    route_name: "external".to_owned(),
                },
                None,
            )
            .await;

        let Err(ModelLifecycleError::UpstreamPreflight {
            primary_cause,
            request_url,
            source,
            ..
        }) = result
        else {
            panic!("invalid endpoint must fail preflight")
        };
        assert_eq!(primary_cause, "The upstream endpoint URL is invalid.");
        assert_eq!(
            request_url.as_ref(),
            "https://[not-an-ipv6-address with control whitespace"
        );
        let source = source.expect("URL parse source");
        assert_eq!(source.to_string(), "Upstream endpoint URL parsing failed");
        assert!(
            source
                .source()
                .expect("URL parse leaf")
                .is::<url::ParseError>()
        );
    }

    #[tokio::test]
    async fn upstream_endpoint_join_failure_retains_its_url_parse_error() {
        let calls = Arc::new(Mutex::new(Vec::new()));
        let desired = TestDesired::new(calls.clone());
        let current = TestCurrent::new(calls);
        let controller = InferenceController::new_with_verification(
            desired,
            current,
            Arc::new(ScriptedVerificationHttp::new([])),
        )
        .expect("controller");
        let mut configuration = upstream_desired_deployment()
            .upstream_desired_configuration
            .expect("upstream configuration");
        configuration.endpoint.as_mut().expect("endpoint").base_url =
            "mailto:ops@example.test".to_owned();

        let result = controller
            .load_upstream_model(
                CreateUpstreamModelDeploymentRequest {
                    upstream_model_template_id: "openai-compatible".to_owned(),
                    desired_configuration: Some(configuration),
                },
                ModelDeploymentExposureIntent::Direct {
                    route_name: "external".to_owned(),
                },
                None,
            )
            .await;

        let Err(ModelLifecycleError::UpstreamPreflight {
            primary_cause,
            source,
            ..
        }) = result
        else {
            panic!("endpoint without a hierarchical base must fail preflight")
        };
        assert_eq!(
            primary_cause,
            "The upstream endpoint URL cannot accept the /models path."
        );
        let source = source.expect("URL join source");
        assert_eq!(source.to_string(), "Upstream endpoint URL join failed");
        assert!(
            source
                .source()
                .expect("URL join leaf")
                .is::<url::ParseError>()
        );
    }

    #[tokio::test]
    async fn upstream_create_failure_records_one_complete_operation_event() {
        let calls = Arc::new(Mutex::new(Vec::new()));
        let desired = TestDesired::new(calls.clone());
        let current = TestCurrent::new(calls);
        let controller = InferenceController::new_with_verification(
            desired,
            current,
            Arc::new(ScriptedVerificationHttp::new([Err(
                VerificationTransportError::Network,
            )])),
        )
        .expect("controller");
        let configuration = upstream_desired_deployment()
            .upstream_desired_configuration
            .expect("upstream configuration");

        let (result, records, _) = capture_records_async(|| {
            controller.load_upstream_model(
                CreateUpstreamModelDeploymentRequest {
                    upstream_model_template_id: "openai-compatible".to_owned(),
                    desired_configuration: Some(configuration),
                },
                ModelDeploymentExposureIntent::Direct {
                    route_name: "external".to_owned(),
                },
                None,
            )
        })
        .await;

        assert!(matches!(
            result,
            Err(ModelLifecycleError::UpstreamPreflight { .. })
        ));
        let failure_events = records
            .iter()
            .filter(|record| {
                matches!(
                    record,
                    Record::Event { fields, .. }
                        if fields.get("event_kind")
                            == Some(&"\"upstream_preflight_failed\"".to_owned())
                )
            })
            .collect::<Vec<_>>();
        assert_eq!(failure_events.len(), 1);
        assert!(
            matches!(
                failure_events.as_slice(),
                [Record::Event { fields, .. }]
                    if fields.get("error_kind") == Some(&"\"upstream_preflight\"".to_owned())
                        && fields.get("request_url")
                            == Some(&"https://upstream.test/v1/models".to_owned())
                        && fields.get("http_status") == Some(&"None".to_owned())
                        && fields.get("summary")
                            == Some(&"\"The endpoint could not be reached\"".to_owned())
                        && fields.get("primary_cause")
                            == Some(&"\"network connection failed\"".to_owned())
                        && fields.get("response_body_excerpt") == Some(&String::new())
                        && fields.get("response_body_size") == Some(&"0".to_owned())
            ),
            "{failure_events:#?}"
        );
        assert!(records.iter().any(|record| matches!(
            record,
            Record::SpanClosed { name, fields }
                if name == "model_load_upstream"
                    && fields.get("operation") == Some(&"\"model_load_upstream\"".to_owned())
                    && fields.get("outcome") == Some(&"\"failed\"".to_owned())
                    && fields.get("request_url")
                        == Some(&"\"https://upstream.test/v1/models\"".to_owned())
        )));
    }

    #[tokio::test]
    async fn upstream_update_failure_records_one_complete_operation_event() {
        let calls = Arc::new(Mutex::new(Vec::new()));
        let desired = TestDesired::new(calls.clone());
        desired
            .state
            .lock()
            .expect("desired state")
            .model_deployments
            .push(upstream_desired_deployment());
        let current = TestCurrent::new(calls);
        let controller = InferenceController::new_with_verification(
            desired,
            current,
            Arc::new(ScriptedVerificationHttp::new([Err(
                VerificationTransportError::Network,
            )])),
        )
        .expect("controller");

        let (result, records, _) = capture_records_async(|| {
            controller.update_upstream_model(
                ModelDeploymentId("deployment".to_owned()),
                UpstreamModelDesiredConfiguration::default(),
            )
        })
        .await;

        assert!(matches!(
            result,
            Err(ModelLifecycleError::UpstreamPreflight { .. })
        ));
        let failure_events = records
            .iter()
            .filter(|record| {
                matches!(
                    record,
                    Record::Event { fields, .. }
                        if fields.get("event_kind")
                            == Some(&"\"upstream_preflight_failed\"".to_owned())
                )
            })
            .collect::<Vec<_>>();
        assert_eq!(failure_events.len(), 1);
        assert!(
            matches!(
                failure_events.as_slice(),
                [Record::Event { fields, .. }]
                    if fields.get("error_kind") == Some(&"\"upstream_preflight\"".to_owned())
                        && fields.get("request_url")
                            == Some(&"https://upstream.test/v1/models".to_owned())
                        && fields.get("http_status") == Some(&"None".to_owned())
                        && fields.get("summary")
                            == Some(&"\"The endpoint could not be reached\"".to_owned())
                        && fields.get("primary_cause")
                            == Some(&"\"network connection failed\"".to_owned())
                        && fields.get("response_body_excerpt") == Some(&String::new())
                        && fields.get("response_body_size") == Some(&"0".to_owned())
            ),
            "{failure_events:#?}"
        );
        assert!(records.iter().any(|record| matches!(
            record,
            Record::SpanClosed { name, fields }
                if name == "model_update_upstream"
                    && fields.get("operation") == Some(&"\"model_update_upstream\"".to_owned())
                    && fields.get("deployment_id") == Some(&"deployment".to_owned())
                    && fields.get("outcome") == Some(&"\"failed\"".to_owned())
                    && fields.get("request_url")
                        == Some(&"\"https://upstream.test/v1/models\"".to_owned())
        )));
    }

    #[tokio::test]
    async fn successful_upstream_update_records_success_without_a_failure_event() {
        let calls = Arc::new(Mutex::new(Vec::new()));
        let desired = TestDesired::new(calls.clone());
        desired
            .state
            .lock()
            .expect("desired state")
            .model_deployments
            .push(upstream_desired_deployment());
        let current = TestCurrent::new(calls);
        let controller = InferenceController::new_with_verification(
            desired,
            current,
            Arc::new(ScriptedVerificationHttp::new([Ok(
                VerificationObservation {
                    status: 200,
                    minimum_retry: None,
                    body: Vec::new(),
                },
            )])),
        )
        .expect("controller");

        let (result, records, _) = capture_records_async(|| {
            controller.update_upstream_model(
                ModelDeploymentId("deployment".to_owned()),
                UpstreamModelDesiredConfiguration::default(),
            )
        })
        .await;

        assert_eq!(
            result
                .expect("successful upstream update")
                .model_deployment_id,
            "deployment"
        );
        assert_eq!(
            records
                .iter()
                .filter(|record| matches!(
                    record,
                    Record::Event { fields, .. }
                        if fields.get("event_kind")
                            == Some(&"\"upstream_preflight_failed\"".to_owned())
                ))
                .count(),
            0
        );
        assert!(
            records.iter().any(|record| matches!(
                record,
                Record::SpanClosed { name, fields }
                    if name == "model_update_upstream"
                        && fields.get("operation")
                            == Some(&"\"model_update_upstream\"".to_owned())
                        && fields.get("deployment_id") == Some(&"deployment".to_owned())
                        && fields.get("outcome") == Some(&"\"success\"".to_owned())
            )),
            "{records:#?}"
        );
    }

    #[tokio::test]
    async fn update_rejects_local_deployment_without_withdrawing_its_route() {
        let calls = Arc::new(Mutex::new(Vec::new()));
        let desired = TestDesired::new(calls.clone());
        desired.persist_present("model");
        let current = TestCurrent::new(calls);
        let controller = test_controller(desired, current);
        publish_serving_target(&controller);

        assert!(matches!(
            controller
                .update_upstream_model(
                    ModelDeploymentId("deployment".to_owned()),
                    UpstreamModelDesiredConfiguration::default(),
                )
                .await,
            Err(ModelLifecycleError::FailedPrecondition)
        ));
        assert_eq!(controller.serving_targets().snapshot().targets.len(), 1);
    }

    #[tokio::test]
    async fn accepted_upstream_replacement_persistence_failure_withdraws_and_records_reverification()
     {
        let calls = Arc::new(Mutex::new(Vec::new()));
        let desired = TestDesired::new(calls.clone());
        desired
            .state
            .lock()
            .expect("desired state")
            .model_deployments
            .push(upstream_desired_deployment());
        let current = TestCurrent::new(calls.clone());
        let controller = test_controller(desired.clone(), current.clone());
        publish_serving_target(&controller);
        calls.lock().expect("calls").clear();
        desired.fail_next(TestFailure::Write);
        let replacement = UpstreamModelDesiredConfiguration {
            api_key: "replacement-key".to_owned(),
            ..Default::default()
        };

        assert!(matches!(
            controller
                .update_upstream_model(ModelDeploymentId("deployment".to_owned()), replacement)
                .await,
            Err(ModelLifecycleError::DesiredPersistence(_))
        ));
        assert_eq!(
            desired
                .state
                .lock()
                .expect("desired state")
                .model_deployments[0]
                .upstream_desired_configuration
                .as_ref()
                .expect("replacement configuration")
                .api_key,
            "replacement-key"
        );
        assert_eq!(
            current.snapshot().expect("current state").model_deployments[0]
                .state
                .as_ref()
                .expect("loading state")
                .status,
            ModelDeploymentStatus::Loading as i32
        );
        assert!(controller.serving_targets().snapshot().targets.is_empty());
        assert_eq!(
            calls.lock().expect("calls").as_slice(),
            ["desired update", "current upsert"]
        );
    }

    #[tokio::test]
    async fn accepted_load_records_intent_and_emits_mutation_event() {
        let calls = Arc::new(Mutex::new(Vec::new()));
        let desired = TestDesired::new(calls.clone());
        let current = TestCurrent::new(calls);
        let controller = test_controller(desired.clone(), current);

        let (result, records, _) = capture_records_async(|| {
            controller.load_model(
                CatalogModelId("model".to_owned()),
                "profile".to_owned(),
                test_placement(),
                crate::test_support::local_model_configuration(),
                ModelDeploymentExposureIntent::Hidden,
                None,
            )
        })
        .await;

        assert_eq!(
            result.expect("accepted load").model_deployment_id,
            "deployment"
        );
        assert_eq!(
            desired
                .model_deployments()
                .expect("manager declarations")
                .iter()
                .map(|deployment| deployment.model_deployment_id.as_str())
                .collect::<Vec<_>>(),
            ["deployment"]
        );
        assert!(records.iter().any(|record| matches!(
            record,
            Record::Event { level, fields }
                if level == "INFO"
                    && fields.get("event_kind") == Some(&"\"model_mutation_accepted\"".to_owned())
                    && fields.get("operation") == Some(&"\"load\"".to_owned())
                    && fields.get("deployment_id") == Some(&"deployment".to_owned())
        )), "{records:#?}");
    }

    #[tokio::test]
    async fn load_and_unload_write_interleavings_have_defined_outcomes() {
        for failure in [TestFailure::Write, TestFailure::Sync] {
            let calls = Arc::new(Mutex::new(Vec::new()));
            let desired = TestDesired::new(calls.clone());
            let current = TestCurrent::new(calls.clone());
            desired.fail_next(failure);
            let result = test_controller(desired, current)
                .load_model(
                    CatalogModelId("model".to_owned()),
                    "profile".to_owned(),
                    test_placement(),
                    crate::test_support::local_model_configuration(),
                    ModelDeploymentExposureIntent::Hidden,
                    None,
                )
                .await;
            assert_result(failure, result);
            assert_eq!(calls.lock().expect("calls").as_slice(), ["desired load"]);
        }

        for failure in [TestFailure::Current, TestFailure::CurrentUncertain] {
            let calls = Arc::new(Mutex::new(Vec::new()));
            let desired = TestDesired::new(calls.clone());
            let current = TestCurrent::new(calls.clone());
            let controller = test_controller(desired, current.clone());
            current.fail_next(failure);
            let result = controller
                .load_model(
                    CatalogModelId("model".to_owned()),
                    "profile".to_owned(),
                    test_placement(),
                    crate::test_support::local_model_configuration(),
                    ModelDeploymentExposureIntent::Hidden,
                    None,
                )
                .await;
            assert!(matches!(
                result,
                Err(ModelLifecycleError::OutcomeUncertain(_))
            ));
            assert_eq!(
                calls.lock().expect("calls").as_slice(),
                ["desired load", "current upsert"]
            );
        }
    }

    #[tokio::test]
    async fn retry_completes_load_and_unload_after_current_state_write_failure() {
        let calls = Arc::new(Mutex::new(Vec::new()));
        let desired = TestDesired::new(calls.clone());
        let current = TestCurrent::new(calls.clone());
        let controller = test_controller(desired.clone(), current.clone());
        current.fail_next(TestFailure::Current);

        assert!(matches!(
            controller
                .load_model(
                    CatalogModelId("model".to_owned()),
                    "profile".to_owned(),
                    test_placement(),
                    crate::test_support::local_model_configuration(),
                    ModelDeploymentExposureIntent::Hidden,
                    None,
                )
                .await,
            Err(ModelLifecycleError::OutcomeUncertain(_))
        ));
        let loaded = controller
            .load_model(
                CatalogModelId("model".to_owned()),
                "profile".to_owned(),
                test_placement(),
                crate::test_support::local_model_configuration(),
                ModelDeploymentExposureIntent::Hidden,
                None,
            )
            .await
            .expect("Load retry");
        assert_eq!(loaded.model_deployment_id, "deployment");
        assert_eq!(
            calls
                .lock()
                .expect("calls")
                .iter()
                .filter(|call| **call == "desired load")
                .count(),
            2
        );

        let calls = Arc::new(Mutex::new(Vec::new()));
        let desired = TestDesired::new(calls.clone());
        let current = TestCurrent::new(calls.clone());
        let controller = test_controller(desired.clone(), current.clone());
        desired.persist_present("model");
        current
            .upsert(current_deployment("deployment"), None)
            .expect("existing current state");
        current.fail_next(TestFailure::Current);

        assert!(matches!(
            controller
                .unload_model(ModelDeploymentId("deployment".to_owned()))
                .await,
            Err(ModelLifecycleError::OutcomeUncertain(_))
        ));
        let unloaded = controller
            .unload_model(ModelDeploymentId("deployment".to_owned()))
            .await
            .expect("Unload retry");
        assert_eq!(
            ModelDeploymentDesiredState::try_from(unloaded.desired_state),
            Ok(ModelDeploymentDesiredState::Teardown)
        );
        assert_eq!(
            calls
                .lock()
                .expect("calls")
                .iter()
                .filter(|call| **call == "desired unload")
                .count(),
            1
        );
    }

    /// Builds a controller running `target-one`, with a node-local image store when `docker`.
    fn test_controller(
        desired: TestDesired,
        current: TestCurrent,
    ) -> InferenceController<TestDesired, TestCurrent> {
        InferenceController::new_with_verification(
            desired,
            current,
            Arc::new(crate::model::upstream::ScriptedVerificationHttp::new(
                (0..1_024).map(|_| {
                    Ok(crate::model::upstream::VerificationObservation {
                        status: 204,
                        minimum_retry: None,
                        body: Vec::new(),
                    })
                }),
            )),
        )
        .expect("controller")
    }

    #[tokio::test]
    async fn pause_acceptance_projects_pausing_and_persists_current_state() {
        let calls = Arc::new(Mutex::new(Vec::new()));
        let desired = TestDesired::new(calls.clone());
        desired.persist_present("model");
        let current = TestCurrent::new(calls.clone());
        current
            .upsert(current_deployment("deployment"), None)
            .expect("current deployment");
        let controller = test_controller(desired.clone(), current.clone());
        calls.lock().expect("calls").clear();

        let paused = controller
            .pause_model(ModelDeploymentId("deployment".to_owned()))
            .await
            .expect("pause acceptance");

        assert_eq!(
            ModelDeploymentDesiredState::try_from(paused.desired_state),
            Ok(ModelDeploymentDesiredState::Paused)
        );
        assert_eq!(
            paused.state.expect("pause state").status,
            dotcore_proto::dotcore::model::v1::ModelDeploymentStatus::Pausing as i32
        );
        assert_eq!(
            calls.lock().expect("calls").as_slice(),
            ["desired pause", "current upsert"]
        );
    }

    #[tokio::test]
    async fn pause_with_desired_write_rename_or_sync_fault_withdraws_serving_and_records_intent() {
        for failure in [TestFailure::Write, TestFailure::Rename, TestFailure::Sync] {
            let calls = Arc::new(Mutex::new(Vec::new()));
            let desired = TestDesired::new(calls.clone());
            desired.persist_present("model");
            let wakeup = desired.listener();
            let current = TestCurrent::new(calls.clone());
            current
                .upsert(current_deployment("deployment"), None)
                .expect("current deployment");
            let controller = test_controller(desired.clone(), current.clone());
            publish_serving_target(&controller);
            calls.lock().expect("calls").clear();
            desired.fail_next(failure);

            assert_result(
                failure,
                controller
                    .pause_model(ModelDeploymentId("deployment".to_owned()))
                    .await,
            );

            assert_eq!(
                ModelDeploymentDesiredState::try_from(
                    desired
                        .state
                        .lock()
                        .expect("desired state")
                        .model_deployments[0]
                        .desired_state,
                ),
                Ok(ModelDeploymentDesiredState::Paused)
            );
            assert_eq!(
                current.snapshot().expect("current state").model_deployments[0]
                    .state
                    .as_ref()
                    .expect("pause state")
                    .status,
                ModelDeploymentStatus::Pausing as i32
            );
            assert!(controller.serving_targets().snapshot().targets.is_empty());
            assert!(controller.serving_targets().snapshot().targets.is_empty());
            assert_eq!(
                calls.lock().expect("calls").as_slice(),
                ["desired pause", "current upsert"]
            );
            tokio::time::timeout(std::time::Duration::from_secs(1), wakeup.notified())
                .await
                .expect("pause wakes reconciliation after desired persistence fault");
        }
    }

    #[tokio::test]
    async fn pause_current_state_fault_withdraws_serving_then_retry_converges_without_second_mutation()
     {
        for failure in [TestFailure::Current, TestFailure::CurrentUncertain] {
            let calls = Arc::new(Mutex::new(Vec::new()));
            let desired = TestDesired::new(calls.clone());
            desired.persist_present("model");
            let wakeup = desired.listener();
            let current = TestCurrent::new(calls.clone());
            current
                .upsert(current_deployment("deployment"), None)
                .expect("current deployment");
            let controller = test_controller(desired.clone(), current.clone());
            publish_serving_target(&controller);
            calls.lock().expect("calls").clear();
            current.fail_next(failure);

            assert!(matches!(
                controller
                    .pause_model(ModelDeploymentId("deployment".to_owned()))
                    .await,
                Err(ModelLifecycleError::OutcomeUncertain(_))
            ));
            assert!(controller.serving_targets().snapshot().targets.is_empty());
            assert!(controller.serving_targets().snapshot().targets.is_empty());
            tokio::time::timeout(std::time::Duration::from_secs(1), wakeup.notified())
                .await
                .expect("pause wakes reconciliation after current-state fault");

            let retried = controller
                .pause_model(ModelDeploymentId("deployment".to_owned()))
                .await
                .expect("pause retry");

            assert_eq!(
                retried.state.expect("retry state").status,
                ModelDeploymentStatus::Pausing as i32
            );
            assert_eq!(
                calls
                    .lock()
                    .expect("calls")
                    .iter()
                    .filter(|call| **call == "desired pause")
                    .count(),
                1
            );
            assert_eq!(
                calls
                    .lock()
                    .expect("calls")
                    .iter()
                    .filter(|call| **call == "current upsert")
                    .count(),
                2
            );
        }
    }

    #[tokio::test]
    async fn list_sanitizes_stale_ready_state_for_paused_and_teardown_intent() {
        let calls = Arc::new(Mutex::new(Vec::new()));
        let desired = TestDesired::new(calls.clone());
        desired.persist_present("model");
        let mut teardown = desired
            .state
            .lock()
            .expect("desired state")
            .model_deployments[0]
            .clone();
        teardown.model_deployment_id = "teardown".to_owned();
        teardown.desired_state = ModelDeploymentDesiredState::Teardown as i32;
        {
            let mut deployments = desired.state.lock().expect("desired state");
            deployments.model_deployments[0].desired_state =
                ModelDeploymentDesiredState::Paused as i32;
            deployments.model_deployments.push(teardown);
        }
        let current = TestCurrent::new(calls);
        let mut ready = current_deployment("deployment");
        ready.state.as_mut().expect("state").status = ModelDeploymentStatus::Ready as i32;
        current.upsert(ready, None).expect("paused stale state");
        let mut teardown_ready = current_deployment("teardown");
        teardown_ready.state.as_mut().expect("state").status = ModelDeploymentStatus::Ready as i32;
        current
            .upsert(teardown_ready, None)
            .expect("teardown stale state");
        let controller = test_controller(desired.clone(), current.clone());
        // The hosting node's evidence is current, so each retained record stands for it.
        let records = current
            .snapshot()
            .expect("retained records")
            .model_deployments;
        let deployments = desired
            .model_deployments()
            .expect("declarations")
            .iter()
            .zip(records)
            .map(|(declaration, record)| {
                controller
                    .inner
                    .project_deployment(declaration, record.state.expect("retained state"))
                    .expect("safe projection")
            })
            .collect::<Vec<_>>();

        assert_eq!(
            deployments[0].state.as_ref().expect("paused state").status,
            ModelDeploymentStatus::Pausing as i32
        );
        assert_eq!(
            deployments[1]
                .state
                .as_ref()
                .expect("teardown state")
                .status,
            ModelDeploymentStatus::Unloading as i32
        );
    }

    fn publish_serving_target(controller: &InferenceController<TestDesired, TestCurrent>) {
        controller
            .inner
            .serving_targets_publisher
            .publish(vec![ServingTarget::Node(node_target_for_test(
                "deployment",
                &"b".repeat(64),
            ))]);
    }

    #[tokio::test]
    async fn pause_accepts_present_deployments_from_loading_failed_or_unavailable() {
        for lifecycle in [
            dotcore_proto::dotcore::model::v1::ModelDeploymentStatus::Loading,
            dotcore_proto::dotcore::model::v1::ModelDeploymentStatus::Failed,
            dotcore_proto::dotcore::model::v1::ModelDeploymentStatus::Unavailable,
        ] {
            let calls = Arc::new(Mutex::new(Vec::new()));
            let desired = TestDesired::new(calls.clone());
            desired.persist_present("model");
            let current = TestCurrent::new(calls.clone());
            current
                .upsert(current_deployment("deployment"), None)
                .expect("current deployment");
            let revision =
                current.snapshot().expect("current state").model_deployments[0].record_revision;
            let mut previous = current.snapshot().expect("current state").model_deployments[0]
                .state
                .clone()
                .expect("loading state");
            previous.status = lifecycle as i32;
            previous.status_message = "current lifecycle".to_owned();
            current
                .upsert(
                    dotcore_proto::dotcore::model::v1::CurrentModelDeployment {
                        model_deployment_id: "deployment".to_owned(),
                        reconciled_lifecycle_generation: None,
                        record_revision: 0,
                        recovery_barrier: None,
                        state: Some(previous),
                    },
                    Some(revision),
                )
                .expect("current lifecycle");
            let controller = test_controller(desired, current);
            calls.lock().expect("calls").clear();

            let paused = controller
                .pause_model(ModelDeploymentId("deployment".to_owned()))
                .await
                .expect("pause acceptance");
            assert_eq!(
                paused.state.expect("pause state").status,
                dotcore_proto::dotcore::model::v1::ModelDeploymentStatus::Pausing as i32
            );
        }
    }

    #[tokio::test]
    async fn repeated_pause_and_resume_are_projection_only_no_ops() {
        let calls = Arc::new(Mutex::new(Vec::new()));
        let desired = TestDesired::new(calls.clone());
        desired.persist_present("model");
        let current = TestCurrent::new(calls.clone());
        current
            .upsert(current_deployment("deployment"), None)
            .expect("current deployment");
        let controller = test_controller(desired.clone(), current.clone());
        controller
            .pause_model(ModelDeploymentId("deployment".to_owned()))
            .await
            .expect("pause acceptance");
        let revision =
            current.snapshot().expect("current state").model_deployments[0].record_revision;
        let previous = current.snapshot().expect("current state").model_deployments[0]
            .state
            .clone()
            .expect("pausing state");
        let mut paused = pausing_state(Some(&previous));
        paused.status = dotcore_proto::dotcore::model::v1::ModelDeploymentStatus::Paused as i32;
        paused.status_message = "paused".to_owned();
        current
            .upsert(
                dotcore_proto::dotcore::model::v1::CurrentModelDeployment {
                    model_deployment_id: "deployment".to_owned(),
                    reconciled_lifecycle_generation: None,
                    record_revision: 0,
                    recovery_barrier: None,
                    state: Some(paused),
                },
                Some(revision),
            )
            .expect("paused observation");
        calls.lock().expect("calls").clear();

        let repeated_pause = controller
            .pause_model(ModelDeploymentId("deployment".to_owned()))
            .await
            .expect("idempotent pause");
        assert_eq!(
            repeated_pause
                .state
                .as_ref()
                .expect("paused state")
                .status_message,
            "paused"
        );
        assert!(calls.lock().expect("calls").is_empty());

        let resumed = controller
            .resume_model(ModelDeploymentId("deployment".to_owned()))
            .await
            .expect("resume acceptance");
        assert_eq!(
            resumed.state.expect("resume state").status,
            dotcore_proto::dotcore::model::v1::ModelDeploymentStatus::Loading as i32
        );
        let calls_after_resume = calls.lock().expect("calls").clone();
        let repeated_resume = controller
            .resume_model(ModelDeploymentId("deployment".to_owned()))
            .await
            .expect("idempotent resume");
        assert_eq!(
            repeated_resume.state.expect("loading state").status,
            dotcore_proto::dotcore::model::v1::ModelDeploymentStatus::Loading as i32
        );
        assert_eq!(calls.lock().expect("calls").as_slice(), calls_after_resume);
    }

    #[tokio::test]
    async fn resume_desired_persistence_fault_records_loading_before_returning_the_original_error()
    {
        for failure in [TestFailure::Write, TestFailure::Rename, TestFailure::Sync] {
            for lifecycle in [
                ModelDeploymentStatus::Pausing,
                ModelDeploymentStatus::Paused,
            ] {
                let calls = Arc::new(Mutex::new(Vec::new()));
                let desired = TestDesired::new(calls.clone());
                desired.persist_present("model");
                desired
                    .state
                    .lock()
                    .expect("desired state")
                    .model_deployments[0]
                    .desired_state = ModelDeploymentDesiredState::Paused as i32;
                let wakeup = desired.listener();
                let current = TestCurrent::new(calls.clone());
                let mut paused = current_deployment("deployment");
                paused.state.as_mut().expect("paused state").status = lifecycle as i32;
                paused.state.as_mut().expect("paused state").status_message = "paused".to_owned();
                current.upsert(paused, None).expect("paused current state");
                let controller = test_controller(desired.clone(), current.clone());
                calls.lock().expect("calls").clear();
                desired.fail_next(failure);

                assert_result(
                    failure,
                    controller
                        .resume_model(ModelDeploymentId("deployment".to_owned()))
                        .await,
                );
                let loading =
                    current.snapshot().expect("current state").model_deployments[0].clone();
                assert_eq!(
                    ModelDeploymentDesiredState::try_from(
                        desired
                            .state
                            .lock()
                            .expect("desired state")
                            .model_deployments[0]
                            .desired_state,
                    ),
                    Ok(ModelDeploymentDesiredState::Present)
                );
                assert_eq!(
                    loading.state.as_ref().expect("loading state").status,
                    ModelDeploymentStatus::Loading as i32
                );
                assert_eq!(loading.record_revision, 2);
                tokio::time::timeout(std::time::Duration::from_secs(1), wakeup.notified())
                    .await
                    .expect("Resume wakes reconciliation after desired persistence fault");

                let retried = controller
                    .resume_model(ModelDeploymentId("deployment".to_owned()))
                    .await
                    .expect("Resume retry");
                assert_eq!(retried.state, loading.state);
                assert_eq!(
                    current.snapshot().expect("current state").model_deployments[0],
                    loading
                );
                assert_eq!(
                    calls.lock().expect("calls").as_slice(),
                    ["desired resume", "current upsert"]
                );
            }
        }
    }

    #[tokio::test]
    async fn resume_current_state_fault_retries_stale_paused_lifecycle_without_repeating_desired_mutation()
     {
        for failure in [TestFailure::Current, TestFailure::CurrentUncertain] {
            for lifecycle in [
                ModelDeploymentStatus::Pausing,
                ModelDeploymentStatus::Paused,
            ] {
                let calls = Arc::new(Mutex::new(Vec::new()));
                let desired = TestDesired::new(calls.clone());
                desired.persist_present("model");
                desired
                    .state
                    .lock()
                    .expect("desired state")
                    .model_deployments[0]
                    .desired_state = ModelDeploymentDesiredState::Paused as i32;
                let wakeup = desired.listener();
                let current = TestCurrent::new(calls.clone());
                let mut paused = current_deployment("deployment");
                paused.state.as_mut().expect("paused state").status = lifecycle as i32;
                paused.state.as_mut().expect("paused state").status_message = "paused".to_owned();
                current
                    .upsert(paused.clone(), None)
                    .expect("paused current state");
                let paused =
                    current.snapshot().expect("current state").model_deployments[0].clone();
                let controller = test_controller(desired.clone(), current.clone());
                calls.lock().expect("calls").clear();
                current.fail_next(failure);

                assert!(matches!(
                    controller
                        .resume_model(ModelDeploymentId("deployment".to_owned()))
                        .await,
                    Err(ModelLifecycleError::OutcomeUncertain(_))
                ));
                assert_eq!(
                    current.snapshot().expect("current state").model_deployments[0],
                    paused
                );
                tokio::time::timeout(std::time::Duration::from_secs(1), wakeup.notified())
                    .await
                    .expect("Resume wakes reconciliation after current-state persistence fault");

                let retried = controller
                    .resume_model(ModelDeploymentId("deployment".to_owned()))
                    .await
                    .expect("Resume retry");
                assert_eq!(
                    retried.state.expect("loading state").status,
                    ModelDeploymentStatus::Loading as i32
                );
                let repaired =
                    current.snapshot().expect("current state").model_deployments[0].clone();
                assert_eq!(repaired.record_revision, 2);
                assert_eq!(
                    repaired.state.as_ref().expect("repaired state").status,
                    ModelDeploymentStatus::Loading as i32
                );
                tokio::time::timeout(std::time::Duration::from_secs(1), wakeup.notified())
                    .await
                    .expect("Resume retry wakes reconciliation after repairing stale paused state");
                assert_eq!(
                    calls
                        .lock()
                        .expect("calls")
                        .iter()
                        .filter(|call| **call == "desired resume")
                        .count(),
                    1
                );
                assert_eq!(
                    calls
                        .lock()
                        .expect("calls")
                        .iter()
                        .filter(|call| **call == "current upsert")
                        .count(),
                    2
                );
            }
        }
    }

    #[tokio::test]
    async fn resume_is_an_exact_no_op_for_healthy_present_deployment() {
        let calls = Arc::new(Mutex::new(Vec::new()));
        let desired = TestDesired::new(calls.clone());
        desired.persist_present("model");
        let current = TestCurrent::new(calls.clone());
        let mut ready = current_deployment("deployment");
        ready.state.as_mut().expect("ready state").status = ModelDeploymentStatus::Ready as i32;
        ready.state.as_mut().expect("ready state").status_message = "ready".to_owned();
        current
            .upsert(ready.clone(), None)
            .expect("ready current state");
        let ready = current.snapshot().expect("current state").model_deployments[0].clone();
        let controller = test_controller(desired, current.clone());
        calls.lock().expect("calls").clear();

        let resumed = controller
            .resume_model(ModelDeploymentId("deployment".to_owned()))
            .await
            .expect("healthy Resume no-op");

        assert_eq!(resumed.state, ready.state);
        assert_eq!(
            current.snapshot().expect("current state").model_deployments[0],
            ready
        );
        assert!(calls.lock().expect("calls").is_empty());
    }

    #[tokio::test]
    async fn pause_and_resume_reject_teardown_without_mutation() {
        let calls = Arc::new(Mutex::new(Vec::new()));
        let desired = TestDesired::new(calls.clone());
        desired.persist_present("model");
        desired
            .state
            .lock()
            .expect("desired state")
            .model_deployments[0]
            .desired_state = ModelDeploymentDesiredState::Teardown as i32;
        let current = TestCurrent::new(calls.clone());
        current
            .upsert(current_deployment("deployment"), None)
            .expect("current deployment");
        let controller = test_controller(desired, current);
        calls.lock().expect("calls").clear();

        assert!(matches!(
            controller
                .pause_model(ModelDeploymentId("deployment".to_owned()))
                .await,
            Err(ModelLifecycleError::FailedPrecondition)
        ));
        assert!(matches!(
            controller
                .resume_model(ModelDeploymentId("deployment".to_owned()))
                .await,
            Err(ModelLifecycleError::FailedPrecondition)
        ));
        assert!(calls.lock().expect("calls").is_empty());
    }

    #[tokio::test]
    async fn unload_records_its_intent_over_a_status_accepted_during_its_desired_write() {
        let calls = Arc::new(Mutex::new(Vec::new()));
        let desired = TestDesired::new(calls.clone());
        desired.persist_present("model");
        let current = TestCurrent::new(calls.clone());
        current
            .upsert(current_deployment("deployment"), None)
            .expect("loading record");
        let controller = test_controller(desired.clone(), current.clone());
        let reporter = current.clone();
        desired.before_next_unload(move || {
            let mut ready = current_deployment("deployment");
            if let Some(state) = ready.state.as_mut() {
                state.status = ModelDeploymentStatus::Ready as i32;
            }
            reporter
                .upsert(ready, Some(1))
                .expect("status accepted during the desired unload");
        });

        let unloaded = controller
            .unload_model(ModelDeploymentId("deployment".to_owned()))
            .await
            .expect("unload over a concurrent status");

        assert_eq!(
            unloaded.state.expect("unload state").status,
            ModelDeploymentStatus::Unloading as i32
        );
        let record = current
            .snapshot()
            .expect("current state")
            .model_deployments
            .into_iter()
            .find(|record| record.model_deployment_id == "deployment")
            .expect("unloading record");
        assert_eq!(record.record_revision, 3);
        assert_eq!(
            record.state.expect("recorded state").status,
            ModelDeploymentStatus::Unloading as i32
        );
    }

    #[tokio::test]
    async fn unload_from_paused_transitions_directly_to_teardown() {
        let calls = Arc::new(Mutex::new(Vec::new()));
        let desired = TestDesired::new(calls.clone());
        desired.persist_present("model");
        desired
            .state
            .lock()
            .expect("desired state")
            .model_deployments[0]
            .desired_state = ModelDeploymentDesiredState::Paused as i32;
        let current = TestCurrent::new(calls.clone());
        current
            .upsert(current_deployment("deployment"), None)
            .expect("current deployment");
        let controller = test_controller(desired, current);
        calls.lock().expect("calls").clear();

        let unloaded = controller
            .unload_model(ModelDeploymentId("deployment".to_owned()))
            .await
            .expect("unload from paused");
        assert_eq!(
            ModelDeploymentDesiredState::try_from(unloaded.desired_state),
            Ok(ModelDeploymentDesiredState::Teardown)
        );
        assert_eq!(
            unloaded.state.expect("unload state").status,
            dotcore_proto::dotcore::model::v1::ModelDeploymentStatus::Unloading as i32
        );
    }
}
