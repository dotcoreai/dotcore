//! The gRPC transport edge for model deployments: it serves the generated
//! model-deployment service contract and translates each request into the
//! inference controller that owns the deployments.
//!
//! It validates and parses incoming requests, forwards lifecycle calls and
//! deployment-log reads and follows to the controller, and maps controller
//! errors to gRPC status codes.

use std::{
    collections::{BTreeMap, BTreeSet},
    num::NonZeroU16,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use dotcore_catalog::{CatalogModelId, shared_parameters::MINIMUM_CONTEXT_TOKENS};
use dotcore_domain::deployment::metadata::{DeploymentMetadataPatch, DeploymentMetadataPatchError};
use dotcore_domain::diagnostic::truncate_utf8_with_ellipsis;
use dotcore_domain::inference_route::is_valid_route_name;
use dotcore_domain::valid_id;
use dotcore_proto::dotcore::catalog::v1::UseCase;
use dotcore_proto::dotcore::model::v1::{
    CreateModelDeploymentRequest, CreateModelDeploymentResponse,
    EstimateLocalModelResourcesRequest, EstimateLocalModelResourcesResponse,
    FollowDeploymentLogsRequest, FollowDeploymentLogsResponse, ListModelDeploymentsRequest,
    ListModelDeploymentsResponse, ListModelProfileCompatibilityRequest,
    ListModelProfileCompatibilityResponse, ListModelRecommendationsRequest,
    ListModelRecommendationsResponse, PauseModelRequest, PauseModelResponse,
    PreviewLocalModelDeploymentRequest, PreviewLocalModelDeploymentResponse,
    ReadDeploymentLogsRequest, ReadDeploymentLogsResponse, RemoveModelDeploymentRequest,
    RemoveModelDeploymentResponse, ResumeModelRequest, ResumeModelResponse,
    UpdateModelDeploymentMetadataRequest, UpdateModelDeploymentMetadataResponse,
    UpdateModelDeploymentRequest, UpdateModelDeploymentResponse,
    create_model_deployment_request::Source as CreateModelDeploymentSource,
    model_deployment_exposure::Kind as ModelDeploymentExposureKind,
    model_deployment_service_server::ModelDeploymentService as ModelDeploymentServiceContract,
};
use futures_util::Stream;
use tonic::{Request, Response, Status};
use tracing::{Instrument, Span, debug_span, error, field, info_span, warn};

use super::{
    ModelTeardownApplicationRouteBlocker,
    controller::{
        MODEL_LOG_SOURCE_NAME, ModelDeploymentExposureIntent, ModelDeploymentLogController,
        ModelLifecycleController, ModelLifecycleError, ModelLifecycleErrorCategory,
        ServingTargetsSourceProvider,
    },
    domain::ModelDeploymentId,
    sizing_scenario::{InvalidSizingScenario, SizingScenario},
};

/// Translates model service requests to the owning inference controller.
#[derive(Debug)]
pub struct ModelDeploymentService<C> {
    controller: C,
    #[allow(
        dead_code,
        reason = "the deployment-log protocol consumes its composed admission capability"
    )]
    log_admission: crate::management_logs::ManagementLogAdmission,
}

impl<C> ModelDeploymentService<C> {
    /// Creates the transport-facing service around an inference controller.
    pub(crate) fn new(
        controller: C,
        log_admission: crate::management_logs::ManagementLogAdmission,
    ) -> Self {
        Self {
            controller,
            log_admission,
        }
    }

    #[allow(
        dead_code,
        reason = "the deployment-log protocol acquires permits through this narrow service seam"
    )]
    pub(crate) fn log_admission(&self) -> crate::management_logs::ManagementLogAdmission {
        self.log_admission.clone()
    }

    #[cfg(test)]
    pub(crate) fn controller(&self) -> &C {
        &self.controller
    }

    /// Translates a controller lifecycle error to the Tonic status at the service edge.
    fn translate_controller_error(error: ModelLifecycleError) -> Status {
        if let Some(message) = error.upstream_preflight_message() {
            return Status::failed_precondition(message);
        }
        if matches!(error, ModelLifecycleError::DesiredPersistence(_)) {
            return Status::unavailable(
                "Upstream model replacement persistence failed. The replacement remains active in memory but will be lost after restart unless persistence is repaired.",
            );
        }
        match error.category() {
            ModelLifecycleErrorCategory::Configuration => {
                Status::failed_precondition("Model lifecycle configuration failed")
            }
            ModelLifecycleErrorCategory::AlreadyExists => match error {
                ModelLifecycleError::RouteNameTaken(name) => {
                    Status::already_exists(format!("Inference route name already exists: {name}"))
                }
                _ => Status::already_exists("model lifecycle resource already exists"),
            },
            ModelLifecycleErrorCategory::FailedPrecondition => match error {
                ModelLifecycleError::TargetIncompatible(error) => {
                    Status::failed_precondition(error.to_string())
                }
                ModelLifecycleError::ApplicationRouteReferences(references) => {
                    Status::failed_precondition(application_route_references_message(&references))
                }
                ModelLifecycleError::SharedRouteReferences(names) => {
                    Status::failed_precondition(format!(
                        "cannot remove model deployment while inference routes shared with other deployments reference it: {}",
                        names.join(", ")
                    ))
                }
                _ => Status::failed_precondition(
                    "model lifecycle operation is not valid for the current state",
                ),
            },
            ModelLifecycleErrorCategory::OutcomeUncertain => Status::unavailable(
                "Model lifecycle outcome is uncertain. Retry the operation or inspect current state.",
            ),
            ModelLifecycleErrorCategory::InvalidArgument => match error {
                ModelLifecycleError::NotModelExecutionTarget(execution_target_id) => {
                    Status::invalid_argument(format!(
                        "Site execution target {execution_target_id:?} is not a model execution target"
                    ))
                }
                ModelLifecycleError::InvalidName => Status::invalid_argument(
                    "Model deployment name must be 1–128 non-control characters after trimming",
                ),
                ModelLifecycleError::ConfigurationRejected(error) => {
                    Status::invalid_argument(configuration_rejection_message(&error))
                }
                _ => Status::invalid_argument("model lifecycle request contains an invalid ID"),
            },
            ModelLifecycleErrorCategory::NotFound => match error {
                ModelLifecycleError::ModelNotFound(model_id) => {
                    Status::not_found(format!("Catalog model {model_id:?} was not found"))
                }
                ModelLifecycleError::ModelProfileNotFound {
                    model_id,
                    profile_id,
                } => Status::not_found(format!(
                    "Catalog model {model_id:?} has no profile {profile_id:?}"
                )),
                ModelLifecycleError::ExecutionTargetNotFound(execution_target_id) => {
                    Status::not_found(format!(
                        "Site execution target {execution_target_id:?} was not found"
                    ))
                }
                _ => Status::not_found("model lifecycle resource was not found"),
            },
        }
    }
}

/// Describes a local deployment configuration the launch rules reject, bounded because the
/// reason may name values the request supplied.
fn configuration_rejection_message(
    error: &crate::model::resolution::ModelLaunchResolutionError,
) -> String {
    const MAX_MESSAGE_BYTES: usize = 1_024;

    truncate_utf8_with_ellipsis(&error.to_string(), MAX_MESSAGE_BYTES)
}

fn application_route_references_message(
    references: &[ModelTeardownApplicationRouteBlocker],
) -> String {
    const MAX_MESSAGE_BYTES: usize = 2_048;
    const MAX_VISIBLE_REFERENCES: usize = 3;

    if references.is_empty() {
        return "model lifecycle operation is not valid for the current state".to_owned();
    }

    let (body, recovery) = if let [reference] = references {
        (
            format!(
                "Cannot teardown the model deployment because application deployment \"{}\" ({}) uses inference route \"{}\" ({}), which would be removed with the model deployment.",
                bounded_display_name(&reference.application_deployment_name),
                reference.application_deployment_id,
                bounded_display_name(&reference.inference_route_name),
                reference.inference_route_id,
            ),
            " Teardown the application and wait for it to be removed, then try again.",
        )
    } else {
        let mut visible = references
            .iter()
            .take(MAX_VISIBLE_REFERENCES)
            .map(application_route_reference_label)
            .collect::<Vec<_>>()
            .join("; ");
        if references.len() > MAX_VISIBLE_REFERENCES {
            visible.push_str(&format!(
                "; and {} more",
                references.len() - MAX_VISIBLE_REFERENCES
            ));
        }
        (
            format!(
                "Cannot teardown the model deployment because these application deployments use inference routes that would be removed with it: {visible}."
            ),
            " Teardown the applications and wait for them to be removed, then try again.",
        )
    };
    let bounded_body =
        truncate_utf8_with_ellipsis(&body, MAX_MESSAGE_BYTES.saturating_sub(recovery.len()));
    format!("{bounded_body}{recovery}")
}

fn application_route_reference_label(reference: &ModelTeardownApplicationRouteBlocker) -> String {
    format!(
        "\"{}\" ({}) via \"{}\" ({})",
        bounded_display_name(&reference.application_deployment_name),
        reference.application_deployment_id,
        bounded_display_name(&reference.inference_route_name),
        reference.inference_route_id,
    )
}

fn bounded_display_name(name: &str) -> String {
    truncate_utf8_with_ellipsis(name, 128)
}

impl<C: ModelLifecycleController> ModelDeploymentService<C> {
    /// Starts the controller observer that reconciles runtime state with desired deployments.
    ///
    /// Call this after service construction and before serving lifecycle requests.
    pub fn start_observer(&self) {
        self.controller.start_observer();
    }

    pub(crate) fn observer_controller(&self) -> C
    where
        C: Clone,
    {
        self.controller.clone()
    }
}

#[expect(
    private_bounds,
    reason = "the public selected service exposes router sources only to backend composition"
)]
impl<C: ServingTargetsSourceProvider> ModelDeploymentService<C> {
    /// Returns the controller-published private source for the inference router.
    pub(crate) fn serving_targets(
        &self,
    ) -> super::controller::serving_targets::ServingTargetsSource {
        self.controller.serving_targets()
    }
}

#[tonic::async_trait]
impl<C> ModelDeploymentServiceContract for ModelDeploymentService<C>
where
    C: ModelLifecycleController + ModelDeploymentLogController + 'static,
{
    async fn list_model_deployments(
        &self,
        _request: Request<ListModelDeploymentsRequest>,
    ) -> Result<Response<ListModelDeploymentsResponse>, Status> {
        let span = debug_span!(
            "model_list_deployments",
            operation = "model_list_deployments",
            deployment_count = field::Empty,
            outcome = field::Empty,
            error_kind = field::Empty,
            status = field::Empty,
        );
        let result = async {
            self.controller
                .list_deployments()
                .await
                .map(|response| {
                    span.record("deployment_count", response.deployments.len());
                    Response::new(response)
                })
                .map_err(Self::translate_controller_error)
        }
        .instrument(span.clone())
        .await;
        record_model_result(&span, &result, "model_lifecycle");
        result
    }

    async fn preview_local_model_deployment(
        &self,
        request: Request<PreviewLocalModelDeploymentRequest>,
    ) -> Result<Response<PreviewLocalModelDeploymentResponse>, Status> {
        let local = request.into_inner().local.ok_or_else(|| {
            Status::invalid_argument("Local model deployment preview requires a local selection")
        })?;
        parse_local_selection(local.clone())?;
        let resolved_snapshot = self
            .controller
            .preview_local_model(local)
            .await
            .map_err(Self::translate_controller_error)?;
        let advisories = super::advisory::local_deployment_advisories(
            &crate::model::engine::EngineIntegrationRegistry::production(),
            &resolved_snapshot,
        )
        .map_err(|error| {
            tracing::error!(error = %format!("{error:#}"), "local preview advisories failed");
            Status::internal("Local model deployment preview advisories could not be evaluated")
        })?;
        Ok(Response::new(PreviewLocalModelDeploymentResponse {
            resolved_snapshot: Some(resolved_snapshot),
            advisories: advisories
                .into_iter()
                .map(|advisory| advisory as i32)
                .collect(),
        }))
    }

    async fn estimate_local_model_resources(
        &self,
        request: Request<EstimateLocalModelResourcesRequest>,
    ) -> Result<Response<EstimateLocalModelResourcesResponse>, Status> {
        let (model_id, profile_id, configuration, placement) =
            parse_resource_estimate_selection(request.into_inner())?;
        self.controller
            .estimate_local_model_resources(model_id, profile_id, configuration, placement)
            .await
            .map(Response::new)
            .map_err(Self::translate_controller_error)
    }

    async fn list_model_profile_compatibility(
        &self,
        request: Request<ListModelProfileCompatibilityRequest>,
    ) -> Result<Response<ListModelProfileCompatibilityResponse>, Status> {
        let (model_id, target, scenario) =
            parse_profile_compatibility_request(request.into_inner())?;
        self.controller
            .list_profile_compatibility(model_id, target, scenario)
            .await
            .map(Response::new)
            .map_err(Self::translate_controller_error)
    }

    async fn list_model_recommendations(
        &self,
        request: Request<ListModelRecommendationsRequest>,
    ) -> Result<Response<ListModelRecommendationsResponse>, Status> {
        let use_cases = parse_model_recommendations_request(request.into_inner())?;
        self.controller
            .list_model_recommendations(use_cases)
            .await
            .map(Response::new)
            .map_err(Self::translate_controller_error)
    }

    async fn create_model_deployment(
        &self,
        request: Request<CreateModelDeploymentRequest>,
    ) -> Result<Response<CreateModelDeploymentResponse>, Status> {
        let request = request.into_inner();
        let exposure = parse_exposure(request.exposure)?;
        let source = request.source.ok_or_else(|| {
            Status::invalid_argument("Model deployment creation requires one source")
        })?;
        let result = match source {
            CreateModelDeploymentSource::Local(local) => {
                let (model_id, profile_id, placement, configuration) =
                    parse_local_selection(local)?;
                self.controller
                    .load_model(
                        CatalogModelId(model_id),
                        profile_id,
                        placement,
                        configuration,
                        exposure,
                        request.model_deployment_name,
                    )
                    .await
            }
            CreateModelDeploymentSource::Upstream(upstream) => {
                self.controller
                    .load_upstream_model(upstream, exposure, request.model_deployment_name)
                    .await
            }
        };
        result
            .map(|deployment| {
                Response::new(CreateModelDeploymentResponse {
                    deployment: Some(deployment),
                })
            })
            .map_err(Self::translate_controller_error)
    }

    async fn update_model_deployment_metadata(
        &self,
        request: Request<UpdateModelDeploymentMetadataRequest>,
    ) -> Result<Response<UpdateModelDeploymentMetadataResponse>, Status> {
        let request = request.into_inner();
        let patch = parse_metadata_patch(request.metadata, request.update_mask)?;
        self.controller
            .update_metadata(ModelDeploymentId(request.model_deployment_id), patch)
            .await
            .map(|deployment| {
                Response::new(UpdateModelDeploymentMetadataResponse {
                    deployment: Some(deployment),
                })
            })
            .map_err(Self::translate_controller_error)
    }

    async fn update_model_deployment(
        &self,
        request: Request<UpdateModelDeploymentRequest>,
    ) -> Result<Response<UpdateModelDeploymentResponse>, Status> {
        let request = request.into_inner();
        let configuration = request.upstream_desired_configuration.ok_or_else(|| {
            Status::invalid_argument("Upstream model update requires replacement configuration")
        })?;
        self.controller
            .update_upstream_model(
                ModelDeploymentId(request.model_deployment_id),
                configuration,
            )
            .await
            .map(|deployment| {
                Response::new(UpdateModelDeploymentResponse {
                    deployment: Some(deployment),
                })
            })
            .map_err(Self::translate_controller_error)
    }

    async fn remove_model_deployment(
        &self,
        request: Request<RemoveModelDeploymentRequest>,
    ) -> Result<Response<RemoveModelDeploymentResponse>, Status> {
        let deployment_id = request.into_inner().model_deployment_id;
        let span = info_span!(
            "model_unload",
            operation = "model_unload",
            deployment_id = %deployment_id,
            outcome = field::Empty,
            error_kind = field::Empty,
            status = field::Empty,
        );
        let result = async {
            self.controller
                .unload_model(ModelDeploymentId(deployment_id.clone()))
                .await
                .map(|deployment| {
                    Response::new(RemoveModelDeploymentResponse {
                        deployment: Some(deployment),
                    })
                })
                .map_err(Self::translate_controller_error)
        }
        .instrument(span.clone())
        .await;
        record_model_result(&span, &result, "model_lifecycle");
        result
    }

    async fn pause_model(
        &self,
        request: Request<PauseModelRequest>,
    ) -> Result<Response<PauseModelResponse>, Status> {
        let deployment_id = request.into_inner().model_deployment_id;
        let span = info_span!(
            "model_pause",
            operation = "model_pause",
            deployment_id = %deployment_id,
            outcome = field::Empty,
            error_kind = field::Empty,
            status = field::Empty,
        );
        let result = async {
            self.controller
                .pause_model(ModelDeploymentId(deployment_id))
                .await
                .map(|deployment| {
                    Response::new(PauseModelResponse {
                        deployment: Some(deployment),
                    })
                })
                .map_err(Self::translate_controller_error)
        }
        .instrument(span.clone())
        .await;
        record_model_result(&span, &result, "model_lifecycle");
        result
    }

    async fn resume_model(
        &self,
        request: Request<ResumeModelRequest>,
    ) -> Result<Response<ResumeModelResponse>, Status> {
        let deployment_id = request.into_inner().model_deployment_id;
        let span = info_span!(
            "model_resume",
            operation = "model_resume",
            deployment_id = %deployment_id,
            outcome = field::Empty,
            error_kind = field::Empty,
            status = field::Empty,
        );
        let result = async {
            self.controller
                .resume_model(ModelDeploymentId(deployment_id))
                .await
                .map(|deployment| {
                    Response::new(ResumeModelResponse {
                        deployment: Some(deployment),
                    })
                })
                .map_err(Self::translate_controller_error)
        }
        .instrument(span.clone())
        .await;
        record_model_result(&span, &result, "model_lifecycle");
        result
    }

    async fn read_deployment_logs(
        &self,
        request: Request<ReadDeploymentLogsRequest>,
    ) -> Result<Response<ReadDeploymentLogsResponse>, Status> {
        let request = request.into_inner();
        let query = parse_read_query(
            request.source_names,
            request.page_size,
            request.before_timestamp,
        )?;
        let page_size = usize::from(query.history().maximum_entries().get());
        let deployment_id = ModelDeploymentId(request.model_deployment_id);
        let diagnostic_context = crate::management_logs::DeploymentLogDiagnosticContext::new(
            "model",
            deployment_id.0.clone(),
            "read_history",
        );
        let protected = self
            .controller
            .protected_log_values(deployment_id.clone())
            .await
            .map_err(|error| {
                crate::management_logs::map_runtime_error_with_diagnostic(
                    error,
                    &diagnostic_context,
                    None,
                )
            })?;
        let histories = self
            .controller
            .read_deployment_logs(deployment_id, query)
            .await
            .map_err(|error| {
                crate::management_logs::map_runtime_error_with_diagnostic(
                    error,
                    &diagnostic_context,
                    Some(&protected),
                )
            })?;
        Ok(Response::new(map_history(
            crate::management_logs::aggregate_history(
                histories,
                page_size,
                &protected,
                &diagnostic_context,
            ),
        )))
    }

    type FollowDeploymentLogsStream = std::pin::Pin<
        Box<dyn Stream<Item = Result<FollowDeploymentLogsResponse, Status>> + Send + 'static>,
    >;

    async fn follow_deployment_logs(
        &self,
        request: Request<FollowDeploymentLogsRequest>,
    ) -> Result<Response<Self::FollowDeploymentLogsStream>, Status> {
        let request = request.into_inner();
        let cancellation = tokio_util::sync::CancellationToken::new();
        let (source_names, positions) =
            parse_follow_query(request.source_names, request.resume_positions)?;
        let permit = self
            .log_admission
            .try_acquire()
            .map_err(crate::management_logs::map_admission_error)?;
        let deployment_id = ModelDeploymentId(request.model_deployment_id);
        let diagnostic_context = crate::management_logs::DeploymentLogDiagnosticContext::new(
            "model",
            deployment_id.0.clone(),
            "follow",
        );
        let protected = self
            .controller
            .protected_log_values(deployment_id.clone())
            .await
            .map_err(|error| {
                crate::management_logs::map_runtime_error_with_diagnostic(
                    error,
                    &diagnostic_context,
                    None,
                )
            })?;
        let streams = self
            .controller
            .follow_deployment_logs(
                deployment_id,
                crate::management_logs::DeploymentLogFollowQuery::new(
                    source_names,
                    positions,
                    cancellation.clone(),
                ),
            )
            .await
            .map_err(|error| {
                crate::management_logs::map_runtime_error_with_diagnostic(
                    error,
                    &diagnostic_context,
                    Some(&protected),
                )
            })?;
        Ok(Response::new(crate::management_logs::follow_response(
            streams,
            protected,
            diagnostic_context,
            cancellation,
            permit,
            map_follow_event,
        )))
    }
}

fn parse_read_query(
    source_names: Vec<String>,
    page_size: u32,
    before_timestamp: Option<dotcore_proto::google::protobuf::Timestamp>,
) -> Result<crate::management_logs::DeploymentLogReadQuery, Status> {
    validate_sources(&source_names)?;
    let invalid_page_size = || {
        Status::invalid_argument(
            "Deployment log page size must be from 1 through 200, or 0 for the default",
        )
    };
    let limit = crate::management_logs::resolve_page_size(
        page_size,
        crate::management_logs::DEFAULT_LOG_PAGE_SIZE,
        u32::from(dotcore_runtime::MAX_RUNTIME_LOG_ENTRIES),
    )
    .and_then(|limit| u16::try_from(limit).ok())
    .and_then(NonZeroU16::new)
    .ok_or_else(invalid_page_size)?;
    let before = before_timestamp.map(timestamp_to_system_time).transpose()?;
    let history =
        dotcore_runtime::RuntimeLogQuery::new(limit, before).map_err(|_| invalid_page_size())?;
    Ok(crate::management_logs::DeploymentLogReadQuery::new(
        source_names,
        history,
    ))
}

fn parse_follow_query(
    source_names: Vec<String>,
    positions: Vec<dotcore_proto::dotcore::model::v1::DeploymentLogResumePosition>,
) -> Result<(Vec<String>, BTreeMap<String, SystemTime>), Status> {
    validate_sources(&source_names)?;
    let mut result = BTreeMap::new();
    for position in positions {
        if !is_model_log_source(&position.source_name)
            || (!source_names.is_empty() && !source_names.contains(&position.source_name))
            || result.contains_key(&position.source_name)
        {
            return Err(Status::invalid_argument(
                "Deployment log resume positions must name selected sources exactly once",
            ));
        }
        result.insert(
            position.source_name,
            timestamp_to_system_time(position.latest_timestamp.ok_or_else(|| {
                Status::invalid_argument("Deployment log resume position requires a timestamp")
            })?)?,
        );
    }
    Ok((source_names, result))
}

/// Whether `name` is a source a model deployment may have: its workload or a preparation step.
fn is_model_log_source(name: &str) -> bool {
    name == MODEL_LOG_SOURCE_NAME
        || crate::deployment_logs::step_from_source_name(name)
            .is_some_and(|step| crate::deployment_logs::model_steps().contains(&step))
}

fn validate_sources(source_names: &[String]) -> Result<(), Status> {
    if source_names.len() > 32
        || source_names.iter().any(String::is_empty)
        || source_names.iter().collect::<BTreeSet<_>>().len() != source_names.len()
    {
        return Err(Status::invalid_argument(
            "Deployment log sources must contain at most 32 unique names",
        ));
    }
    Ok(())
}

fn timestamp_to_system_time(
    timestamp: dotcore_proto::google::protobuf::Timestamp,
) -> Result<SystemTime, Status> {
    if timestamp.seconds < 0 || !(0..1_000_000_000).contains(&timestamp.nanos) {
        return Err(Status::invalid_argument(
            "Deployment log timestamp is invalid",
        ));
    }
    Ok(UNIX_EPOCH + Duration::new(timestamp.seconds as u64, timestamp.nanos as u32))
}

pub(crate) fn timestamp_from(time: SystemTime) -> dotcore_proto::google::protobuf::Timestamp {
    let duration = time.duration_since(UNIX_EPOCH).unwrap_or_default();
    dotcore_proto::google::protobuf::Timestamp {
        seconds: duration.as_secs().try_into().unwrap_or(i64::MAX),
        nanos: duration.subsec_nanos() as i32,
    }
}

fn map_history(
    history: crate::management_logs::DeploymentLogHistory,
) -> ReadDeploymentLogsResponse {
    use dotcore_proto::dotcore::model::v1::{
        DeploymentLogHistoryState, DeploymentLogResumePosition, DeploymentLogSource,
        DeploymentLogSourceState,
    };
    ReadDeploymentLogsResponse {
        sources: history
            .sources
            .into_iter()
            .map(|source| DeploymentLogSource {
                source_name: source.source.name,
                source_label: source.source.label,
                state: if source.available {
                    DeploymentLogSourceState::Available as i32
                } else {
                    DeploymentLogSourceState::Unavailable as i32
                },
            })
            .collect(),
        entries: history.entries.into_iter().map(map_entry).collect(),
        next_before_timestamp: history.next_before_timestamp.map(timestamp_from),
        history_state: if history.older_output_may_remain {
            DeploymentLogHistoryState::OlderOutputMayRemain as i32
        } else {
            DeploymentLogHistoryState::NoOlderOutputObserved as i32
        },
        response_truncated: history.response_truncated,
        latest_positions: history
            .latest_positions
            .into_iter()
            .map(|(source_name, timestamp)| DeploymentLogResumePosition {
                source_name,
                latest_timestamp: Some(timestamp_from(timestamp)),
            })
            .collect(),
    }
}

fn map_entry(
    entry: dotcore_runtime::RuntimeLogEntry,
) -> dotcore_proto::dotcore::model::v1::DeploymentLogEntry {
    use dotcore_proto::dotcore::model::v1::DeploymentLogStream;
    dotcore_proto::dotcore::model::v1::DeploymentLogEntry {
        source_name: entry.source.name,
        source_label: entry.source.label,
        stream: match entry.stream {
            dotcore_runtime::RuntimeLogStreamKind::Stdout => DeploymentLogStream::Stdout as i32,
            dotcore_runtime::RuntimeLogStreamKind::Stderr => DeploymentLogStream::Stderr as i32,
        },
        timestamp: Some(timestamp_from(entry.timestamp)),
        content: entry.content,
        truncated: entry.truncated,
    }
}

fn map_follow_event(
    event: crate::management_logs::DeploymentLogFollowEvent,
) -> FollowDeploymentLogsResponse {
    use dotcore_proto::dotcore::model::v1::{
        DeploymentLogSourceEvent, DeploymentLogSourceState, follow_deployment_logs_response::Event,
    };
    let event = match event {
        crate::management_logs::DeploymentLogFollowEvent::Entry(entry) => {
            Event::Entry(map_entry(entry))
        }
        crate::management_logs::DeploymentLogFollowEvent::Available(source) => {
            Event::SourceEvent(DeploymentLogSourceEvent {
                source_name: source.name,
                source_label: source.label,
                state: DeploymentLogSourceState::Available as i32,
            })
        }
        crate::management_logs::DeploymentLogFollowEvent::Unavailable(source) => {
            Event::SourceEvent(DeploymentLogSourceEvent {
                source_name: source.name,
                source_label: source.label,
                state: DeploymentLogSourceState::Unavailable as i32,
            })
        }
        crate::management_logs::DeploymentLogFollowEvent::Ended(source) => {
            Event::SourceEvent(DeploymentLogSourceEvent {
                source_name: source.name,
                source_label: source.label,
                state: DeploymentLogSourceState::Ended as i32,
            })
        }
    };
    FollowDeploymentLogsResponse { event: Some(event) }
}

fn parse_metadata_patch(
    metadata: Option<dotcore_proto::dotcore::model::v1::ModelDeploymentMetadata>,
    update_mask: Option<dotcore_proto::google::protobuf::FieldMask>,
) -> Result<DeploymentMetadataPatch, Status> {
    DeploymentMetadataPatch::parse(
        metadata
            .as_ref()
            .map(|metadata| metadata.display_name.as_deref()),
        update_mask.as_ref(),
    )
    .map_err(|error| match error {
        DeploymentMetadataPatchError::InvalidDisplayName(_) => Status::invalid_argument(
            "Model deployment name must be 1–128 non-control characters after trimming",
        ),
        _ => Status::invalid_argument("Model deployment metadata update is invalid"),
    })
}

/// Validates the use cases of a recommendations read: each must be a known, specified use case.
/// Duplicates collapse to their first occurrence, and none asks for every use case.
fn parse_model_recommendations_request(
    request: ListModelRecommendationsRequest,
) -> Result<Vec<UseCase>, Status> {
    if request.use_cases.is_empty() {
        return Ok(ALL_USE_CASES.to_vec());
    }
    let mut use_cases = Vec::new();
    for value in request.use_cases {
        let use_case = UseCase::try_from(value)
            .ok()
            .filter(|use_case| *use_case != UseCase::Unspecified)
            .ok_or_else(|| {
                Status::invalid_argument("Model recommendations require specified use cases")
            })?;
        if !use_cases.contains(&use_case) {
            use_cases.push(use_case);
        }
    }
    Ok(use_cases)
}

/// Every use case a recommendations read ranks when the request names none.
const ALL_USE_CASES: [UseCase; 3] = [
    UseCase::Coding,
    UseCase::ChatAssistant,
    UseCase::ImageGeneration,
];

/// Validated compatibility read selection: the model, the target and the sizing scenario.
type ProfileCompatibilitySelection = (
    Option<String>,
    Option<dotcore_proto::dotcore::deployment::v1::ExactPlacement>,
    Option<SizingScenario>,
);

fn parse_profile_compatibility_request(
    request: ListModelProfileCompatibilityRequest,
) -> Result<ProfileCompatibilitySelection, Status> {
    if request
        .model_id
        .as_deref()
        .is_some_and(|model_id| !valid_id(model_id))
    {
        return Err(Status::invalid_argument(
            "Model profile compatibility requires a valid model ID",
        ));
    }
    if request.target.as_ref().is_some_and(|target| {
        !valid_id(&target.site_node_id) || !valid_id(&target.execution_target_id)
    }) {
        return Err(Status::invalid_argument(
            "Model profile compatibility requires valid target placement IDs",
        ));
    }
    let scenario = request
        .scenario
        .map(|scenario| {
            SizingScenario::new(
                scenario.total_context_tokens,
                scenario.context_tokens_per_request,
            )
        })
        .transpose()
        .map_err(|error| match error {
            InvalidSizingScenario::TotalContextBelowMinimum { .. } => {
                Status::invalid_argument(format!(
                    "Sizing scenario requires a total context of at least {MINIMUM_CONTEXT_TOKENS} tokens"
                ))
            }
            InvalidSizingScenario::ContextPerRequestBelowMinimum { .. } => {
                Status::invalid_argument(format!(
                    "Sizing scenario requires a context per request of at least {MINIMUM_CONTEXT_TOKENS} tokens"
                ))
            }
            InvalidSizingScenario::ContextPerRequestAboveTotal { .. } => Status::invalid_argument(
                "Sizing scenario context per request must not be above its total context",
            ),
        })?;
    Ok((request.model_id, request.target, scenario))
}

fn parse_resource_estimate_selection(
    request: EstimateLocalModelResourcesRequest,
) -> Result<
    (
        String,
        String,
        dotcore_proto::dotcore::model::v1::LocalModelDeploymentConfiguration,
        Option<dotcore_proto::dotcore::deployment::v1::ExactPlacement>,
    ),
    Status,
> {
    if !valid_id(&request.model_id) {
        return Err(Status::invalid_argument(
            "Local model resource estimation requires a valid model ID",
        ));
    }
    if !valid_id(&request.profile_id) {
        return Err(Status::invalid_argument(
            "Local model resource estimation requires a valid profile ID",
        ));
    }
    let configuration = request.configuration.ok_or_else(|| {
        Status::invalid_argument("Local model resource estimation requires complete configuration")
    })?;
    if !valid_optional_binding_id(&configuration.model_artifact_binding_id) {
        return Err(Status::invalid_argument(
            "Local model resource estimation requires a valid model artifact binding ID",
        ));
    }
    if request.placement.as_ref().is_some_and(|placement| {
        !valid_id(&placement.site_node_id) || !valid_id(&placement.execution_target_id)
    }) {
        return Err(Status::invalid_argument(
            "Local model resource estimation requires valid target placement IDs",
        ));
    }
    Ok((
        request.model_id,
        request.profile_id,
        configuration,
        request.placement,
    ))
}

fn parse_local_selection(
    local: dotcore_proto::dotcore::model::v1::CreateLocalModelDeploymentRequest,
) -> Result<
    (
        String,
        String,
        dotcore_proto::dotcore::deployment::v1::ExactPlacement,
        dotcore_proto::dotcore::model::v1::LocalModelDeploymentConfiguration,
    ),
    Status,
> {
    if !valid_id(&local.model_id) {
        return Err(Status::invalid_argument(
            "Local model deployment requires a valid model ID",
        ));
    }
    if !valid_id(&local.profile_id) {
        return Err(Status::invalid_argument(
            "Local model deployment requires a valid profile ID",
        ));
    }
    let placement = local.placement.ok_or_else(|| {
        Status::invalid_argument("Local model deployment requires exact placement")
    })?;
    if !valid_id(&placement.site_node_id) || !valid_id(&placement.execution_target_id) {
        return Err(Status::invalid_argument(
            "Local model deployment requires valid placement IDs",
        ));
    }
    let configuration = local.configuration.ok_or_else(|| {
        Status::invalid_argument("Local model deployment requires complete configuration")
    })?;
    if !valid_optional_binding_id(&configuration.model_artifact_binding_id) {
        return Err(Status::invalid_argument(
            "Local model deployment requires a valid model artifact binding ID",
        ));
    }
    Ok((local.model_id, local.profile_id, placement, configuration))
}

/// Accepts an empty binding ID, which selects the profile's default binding, or a valid one.
fn valid_optional_binding_id(binding_id: &str) -> bool {
    binding_id.is_empty() || valid_id(binding_id)
}

fn parse_exposure(
    exposure: Option<dotcore_proto::dotcore::model::v1::ModelDeploymentExposure>,
) -> Result<ModelDeploymentExposureIntent, Status> {
    match exposure.and_then(|exposure| exposure.kind) {
        Some(ModelDeploymentExposureKind::Direct(direct))
            if is_valid_route_name(&direct.route_name) =>
        {
            Ok(ModelDeploymentExposureIntent::Direct {
                route_name: direct.route_name,
            })
        }
        Some(ModelDeploymentExposureKind::Direct(_)) => Err(Status::invalid_argument(
            "Direct model deployment exposure requires a valid route name",
        )),
        Some(ModelDeploymentExposureKind::Hidden(_)) => Ok(ModelDeploymentExposureIntent::Hidden),
        None => Err(Status::invalid_argument(
            "Model deployment exposure is required",
        )),
    }
}

fn record_model_result<T>(
    span: &Span,
    result: &Result<Response<T>, Status>,
    error_kind: &'static str,
) {
    match result {
        Ok(_) => {
            span.record("outcome", "success");
        }
        Err(status) => {
            span.record("outcome", "failed");
            span.record("status", status.code().to_string());
            let expected = matches!(
                status.code(),
                tonic::Code::InvalidArgument
                    | tonic::Code::NotFound
                    | tonic::Code::FailedPrecondition
            );
            if expected {
                span.record("error_kind", "request");
                warn!(parent: span, status = ?status.code(), "model request rejected");
            } else {
                span.record("error_kind", error_kind);
                error!(parent: span, status = ?status.code(), error_kind, "model operation failed");
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::{
        future::ready,
        time::{Duration, UNIX_EPOCH},
    };

    use dotcore_proto::dotcore::model::v1::{
        CreateLocalModelDeploymentRequest, CreateModelDeploymentRequest,
        CreateUpstreamModelDeploymentRequest, DirectRouteExposure,
        EstimateLocalModelResourcesRequest, ModelDeployment, ModelDeploymentExposure,
        ModelDeploymentMetadata, UpdateModelDeploymentMetadataRequest,
        UpstreamModelDesiredConfiguration,
        create_model_deployment_request::Source as CreateModelDeploymentSource,
        model_deployment_exposure::Kind as ModelDeploymentExposureKind,
    };
    use dotcore_proto::google::protobuf::FieldMask;
    use dotcore_proto::{
        dotcore::model::v1::{
            DeploymentLogResumePosition, FollowDeploymentLogsRequest, ReadDeploymentLogsRequest,
        },
        google::protobuf::Timestamp,
    };
    use tonic::Code;

    use super::*;
    use crate::model::ModelTeardownApplicationRouteBlocker;

    #[test]
    fn application_route_reference_returns_exact_actionable_failed_precondition() {
        let status = ModelDeploymentService::<TestController>::translate_controller_error(
            ModelLifecycleError::ApplicationRouteReferences(vec![application_route_blocker(
                "application-one",
                "Open WebUI",
                "route-one",
                "Gemma route",
            )]),
        );

        assert_eq!(status.code(), Code::FailedPrecondition);
        assert_eq!(
            status.message(),
            "Cannot teardown the model deployment because application deployment \"Open WebUI\" (application-one) uses inference route \"Gemma route\" (route-one), which would be removed with the model deployment. Teardown the application and wait for it to be removed, then try again."
        );
        assert_eq!(status.details(), b"");
    }

    #[test]
    fn route_name_taken_returns_already_exists_naming_the_route() {
        let status = ModelDeploymentService::<TestController>::translate_controller_error(
            ModelLifecycleError::RouteNameTaken("public-model".to_owned()),
        );

        assert_eq!(status.code(), Code::AlreadyExists);
        assert_eq!(
            status.message(),
            "Inference route name already exists: public-model"
        );
    }

    #[test]
    fn direct_exposure_accepts_dotted_route_names() {
        let exposure = parse_exposure(Some(ModelDeploymentExposure {
            kind: Some(ModelDeploymentExposureKind::Direct(DirectRouteExposure {
                route_name: "claude-3.5-sonnet".to_owned(),
            })),
        }))
        .expect("Direct exposure accepts a dotted route name");

        assert_eq!(
            exposure,
            ModelDeploymentExposureIntent::Direct {
                route_name: "claude-3.5-sonnet".to_owned(),
            }
        );
    }

    #[test]
    fn shared_route_references_return_exact_failed_precondition_naming_the_routes() {
        let status = ModelDeploymentService::<TestController>::translate_controller_error(
            ModelLifecycleError::SharedRouteReferences(vec![
                "chat".to_owned(),
                "coding".to_owned(),
            ]),
        );

        assert_eq!(status.code(), Code::FailedPrecondition);
        assert_eq!(
            status.message(),
            "cannot remove model deployment while inference routes shared with other deployments reference it: chat, coding"
        );
        assert_eq!(status.details(), b"");
    }

    #[test]
    fn application_route_references_return_exact_three_blockers_without_omission() {
        let status = ModelDeploymentService::<TestController>::translate_controller_error(
            ModelLifecycleError::ApplicationRouteReferences(vec![
                application_route_blocker("application-a", "App A", "route-a", "Route A"),
                application_route_blocker("application-b", "App B", "route-b", "Route B"),
                application_route_blocker("application-c", "App C", "route-c", "Route C"),
            ]),
        );

        assert_eq!(status.code(), Code::FailedPrecondition);
        assert_eq!(
            status.message(),
            "Cannot teardown the model deployment because these application deployments use inference routes that would be removed with it: \"App A\" (application-a) via \"Route A\" (route-a); \"App B\" (application-b) via \"Route B\" (route-b); \"App C\" (application-c) via \"Route C\" (route-c). Teardown the applications and wait for them to be removed, then try again."
        );
        assert_eq!(status.details(), b"");
    }

    #[test]
    fn application_route_references_report_omitted_count_after_first_three_blockers() {
        let status = ModelDeploymentService::<TestController>::translate_controller_error(
            ModelLifecycleError::ApplicationRouteReferences(vec![
                application_route_blocker("application-a", "App A", "route-a", "Route A"),
                application_route_blocker("application-b", "App B", "route-b", "Route B"),
                application_route_blocker("application-c", "App C", "route-c", "Route C"),
                application_route_blocker("application-d", "App D", "route-d", "Route D"),
            ]),
        );

        assert_eq!(status.code(), Code::FailedPrecondition);
        assert_eq!(
            status.message(),
            "Cannot teardown the model deployment because these application deployments use inference routes that would be removed with it: \"App A\" (application-a) via \"Route A\" (route-a); \"App B\" (application-b) via \"Route B\" (route-b); \"App C\" (application-c) via \"Route C\" (route-c); and 1 more. Teardown the applications and wait for them to be removed, then try again."
        );
        assert_eq!(status.details(), b"");
    }

    #[test]
    fn application_route_reference_message_bounds_multibyte_names_and_preserves_recovery() {
        let long_name = "é".repeat(100);
        let status = ModelDeploymentService::<TestController>::translate_controller_error(
            ModelLifecycleError::ApplicationRouteReferences(vec![application_route_blocker(
                "application-one",
                &long_name,
                "route-one",
                &long_name,
            )]),
        );
        let bounded_name = format!("{}…", "é".repeat(62));

        assert_eq!(status.code(), Code::FailedPrecondition);
        assert_eq!(
            status.message(),
            format!(
                "Cannot teardown the model deployment because application deployment \"{bounded_name}\" (application-one) uses inference route \"{bounded_name}\" (route-one), which would be removed with the model deployment. Teardown the application and wait for it to be removed, then try again."
            )
        );
        assert!(status.message().len() <= 2_048);
        assert!(
            status.message().ends_with(
                "Teardown the application and wait for it to be removed, then try again."
            )
        );
        assert!(status.message().is_char_boundary(status.message().len()));
        assert_eq!(status.details(), b"");
    }

    #[test]
    fn upstream_preflight_error_returns_one_exact_plain_http_status_message_without_details() {
        let status =
            ModelDeploymentService::<TestController>::translate_controller_error(preflight_error(
                "The endpoint rejected the verification request",
                "HTTP response received",
                "https://provider.example/v1/models",
                Some(503),
                Some(b"temporarily unavailable".to_vec()),
            ));

        assert_eq!(status.code(), Code::FailedPrecondition);
        assert_eq!(
            status.message(),
            "Upstream endpoint preflight failed: The endpoint rejected the verification request; request URL: https://provider.example/v1/models; HTTP status: 503; response: temporarily unavailable"
        );
        assert_eq!(status.details(), b"");
    }

    #[test]
    fn plain_preflight_message_bounds_multibyte_components_and_normalizes_response_whitespace() {
        let summary = Box::leak("é".repeat(300).into_boxed_str());
        let request_url = "u".repeat(800);
        let status =
            ModelDeploymentService::<TestController>::translate_controller_error(preflight_error(
                summary,
                "HTTP response received",
                request_url,
                Some(503),
                Some(b" provider\n\t temporarily   unavailable ".to_vec()),
            ));
        let expected = format!(
            "Upstream endpoint preflight failed: {}…; request URL: {}…; HTTP status: 503; response: provider temporarily unavailable",
            "é".repeat(190),
            "u".repeat(765),
        );

        assert_eq!(status.code(), Code::FailedPrecondition);
        assert_eq!(status.message(), expected);
        assert!(status.message().is_char_boundary(status.message().len()));
        assert!(!status.message().contains(['\n', '\r', '\t']));
        assert!(status.message().len() <= 2_048);
        assert_eq!(status.details(), b"");
    }

    #[test]
    fn plain_preflight_message_normalizes_invalid_raw_url_control_whitespace_before_truncation() {
        let raw_request_url = format!("{} \n\r\t{}", "é".repeat(190), "é".repeat(210));
        let status =
            ModelDeploymentService::<TestController>::translate_controller_error(preflight_error(
                "The endpoint configuration is invalid",
                "The upstream endpoint URL is invalid.",
                raw_request_url,
                None,
                None,
            ));
        let expected_request_url = format!("{} {}…", "é".repeat(190), "é".repeat(192));

        assert_eq!(
            status.message(),
            format!(
                "Upstream endpoint preflight failed: The endpoint configuration is invalid; request URL: {expected_request_url}; cause: The upstream endpoint URL is invalid."
            )
        );
        assert!(!status.message().contains(['\n', '\r', '\t']));
        assert_eq!(status.details(), b"");
    }

    #[test]
    fn upstream_preflight_error_returns_one_exact_plain_invalid_url_message_without_details() {
        let status =
            ModelDeploymentService::<TestController>::translate_controller_error(preflight_error(
                "The endpoint configuration is invalid",
                "The upstream endpoint URL is invalid.",
                "https://[not-an-ipv6-address",
                None,
                None,
            ));

        assert_eq!(status.code(), Code::FailedPrecondition);
        assert_eq!(
            status.message(),
            "Upstream endpoint preflight failed: The endpoint configuration is invalid; request URL: https://[not-an-ipv6-address; cause: The upstream endpoint URL is invalid."
        );
        assert_eq!(status.details(), b"");
    }

    #[test]
    fn target_incompatibility_passes_its_message() {
        let status = ModelDeploymentService::<TestController>::translate_controller_error(
            ModelLifecycleError::TargetIncompatible(Box::new(
                crate::model::resolution::ModelLaunchResolutionError::AcceleratorDeviceCountMismatch {
                    execution_target_id: "gpu-pair".to_owned(),
                    device_count: 2,
                    required: 1,
                },
            )),
        );

        assert_eq!(status.code(), Code::FailedPrecondition);
        assert_eq!(
            status.message(),
            "execution target \"gpu-pair\" has 2 accelerator devices, but the profile's launch uses 1"
        );
    }

    #[test]
    fn non_model_targets_are_invalid_arguments() {
        let status = ModelDeploymentService::<TestController>::translate_controller_error(
            ModelLifecycleError::NotModelExecutionTarget("apps".to_owned()),
        );

        assert_eq!(status.code(), Code::InvalidArgument);
        assert_eq!(
            status.message(),
            "Site execution target \"apps\" is not a model execution target"
        );
    }

    #[test]
    fn recommendation_requests_default_to_all_use_cases_and_reject_unspecified_ones() {
        let parse = |use_cases: &[i32]| {
            super::parse_model_recommendations_request(ListModelRecommendationsRequest {
                use_cases: use_cases.to_vec(),
                can_start_now: false,
            })
        };

        assert_eq!(
            parse(&[]).expect("all use cases"),
            [
                UseCase::Coding,
                UseCase::ChatAssistant,
                UseCase::ImageGeneration,
            ]
        );
        assert_eq!(
            parse(&[
                UseCase::ImageGeneration as i32,
                UseCase::Coding as i32,
                UseCase::ImageGeneration as i32,
            ])
            .expect("named use cases"),
            [UseCase::ImageGeneration, UseCase::Coding]
        );
        for invalid in [UseCase::Unspecified as i32, 99] {
            let status = parse(&[UseCase::ImageGeneration as i32, invalid]).expect_err("invalid");
            assert_eq!(status.code(), tonic::Code::InvalidArgument);
            assert_eq!(
                status.message(),
                "Model recommendations require specified use cases"
            );
        }
    }

    #[test]
    fn profile_compatibility_requests_reject_invalid_ids_and_scenarios() {
        let placement = |site_node_id: &str, execution_target_id: &str| {
            dotcore_proto::dotcore::deployment::v1::ExactPlacement {
                site_node_id: site_node_id.to_owned(),
                execution_target_id: execution_target_id.to_owned(),
            }
        };

        assert_eq!(
            super::parse_profile_compatibility_request(ListModelProfileCompatibilityRequest {
                model_id: Some("model".to_owned()),
                target: Some(placement("node", "target")),
                scenario: Some(dotcore_proto::dotcore::model::v1::SizingScenario {
                    total_context_tokens: 1024,
                    context_tokens_per_request: Some(1024),
                }),
            })
            .expect("valid request"),
            (
                Some("model".to_owned()),
                Some(placement("node", "target")),
                Some(SizingScenario::new(1024, Some(1024)).expect("minimum scenario"))
            )
        );
        assert_eq!(
            super::parse_profile_compatibility_request(Default::default())
                .expect("unfiltered request"),
            (None, None, None)
        );
        for (request, message) in [
            (
                ListModelProfileCompatibilityRequest {
                    model_id: Some(String::new()),
                    target: None,
                    scenario: None,
                },
                "Model profile compatibility requires a valid model ID",
            ),
            (
                ListModelProfileCompatibilityRequest {
                    model_id: None,
                    target: Some(placement("node", "")),
                    scenario: None,
                },
                "Model profile compatibility requires valid target placement IDs",
            ),
            (
                ListModelProfileCompatibilityRequest {
                    model_id: None,
                    target: None,
                    scenario: Some(dotcore_proto::dotcore::model::v1::SizingScenario {
                        total_context_tokens: 1023,
                        context_tokens_per_request: None,
                    }),
                },
                "Sizing scenario requires a total context of at least 1024 tokens",
            ),
            (
                ListModelProfileCompatibilityRequest {
                    model_id: None,
                    target: None,
                    scenario: Some(dotcore_proto::dotcore::model::v1::SizingScenario {
                        total_context_tokens: 4096,
                        context_tokens_per_request: Some(1023),
                    }),
                },
                "Sizing scenario requires a context per request of at least 1024 tokens",
            ),
            (
                ListModelProfileCompatibilityRequest {
                    model_id: None,
                    target: None,
                    scenario: Some(dotcore_proto::dotcore::model::v1::SizingScenario {
                        total_context_tokens: 4096,
                        context_tokens_per_request: Some(4097),
                    }),
                },
                "Sizing scenario context per request must not be above its total context",
            ),
        ] {
            let status =
                super::parse_profile_compatibility_request(request).expect_err("invalid request");
            assert_eq!(status.code(), Code::InvalidArgument);
            assert_eq!(status.message(), message);
        }
    }

    #[test]
    fn unrelated_errors_keep_the_generic_status_fallback() {
        let status = ModelDeploymentService::<TestController>::translate_controller_error(
            ModelLifecycleError::Configuration(anyhow::anyhow!("unrelated configuration")),
        );
        assert_eq!(status.code(), Code::FailedPrecondition);
        assert_eq!(status.message(), "Model lifecycle configuration failed");
        assert_eq!(status.details(), b"" as &[u8]);
    }

    #[test]
    fn internal_launch_resolution_failures_stay_opaque() {
        use crate::model::resolution::ModelLaunchResolutionError;

        for error in [
            ModelLaunchResolutionError::EngineRealization(anyhow::anyhow!(
                "selected auxiliary artifact is stale at /var/lib/dotcore/cache"
            )),
            ModelLaunchResolutionError::SizingUnavailable {
                binding_id: "fixture-artifact".to_owned(),
                options: "none".to_owned(),
                source: anyhow::anyhow!("sizing expression failed"),
            },
        ] {
            let status = ModelDeploymentService::<TestController>::translate_controller_error(
                ModelLifecycleError::from_model_repository(
                    crate::model::repository::launch_resolution_error(error),
                ),
            );

            assert_eq!(
                (status.code(), status.message()),
                (
                    Code::FailedPrecondition,
                    "Model lifecycle configuration failed"
                )
            );
        }
    }

    #[test]
    fn configuration_rejections_return_their_bounded_reason_as_invalid_arguments() {
        let status = ModelDeploymentService::<TestController>::translate_controller_error(
            ModelLifecycleError::from_model_repository(
                crate::model::repository::launch_resolution_error(
                    crate::model::resolution::ModelLaunchResolutionError::InvalidParameterValues(
                        anyhow::anyhow!("unknown engine parameter {:?}", "é".repeat(600)),
                    ),
                ),
            ),
        );

        assert_eq!(status.code(), Code::InvalidArgument);
        assert_eq!(
            status.message(),
            format!(
                "invalid engine parameter values: unknown engine parameter \"{}…",
                "é".repeat(481)
            )
        );
        assert_eq!(status.message().len(), 1_024);
    }

    #[tokio::test]
    async fn create_handler_routes_upstream_source_and_exposure_to_the_controller() {
        let service = ModelDeploymentService::new(
            TestController::accepted(),
            crate::management_logs::ManagementLogAdmission::new(),
        );
        let response = ModelDeploymentServiceContract::create_model_deployment(
            &service,
            Request::new(CreateModelDeploymentRequest {
                source: Some(CreateModelDeploymentSource::Upstream(
                    CreateUpstreamModelDeploymentRequest::default(),
                )),
                exposure: Some(ModelDeploymentExposure {
                    kind: Some(ModelDeploymentExposureKind::Direct(DirectRouteExposure {
                        route_name: "route-one".to_owned(),
                    })),
                }),
                ..Default::default()
            }),
        )
        .await
        .expect("upstream creation");
        assert_eq!(
            response.into_inner().deployment,
            Some(ModelDeployment::default())
        );
    }

    #[tokio::test]
    async fn resource_estimate_handler_returns_only_generic_scalar_results() {
        let service = ModelDeploymentService::new(
            TestController::accepted(),
            crate::management_logs::ManagementLogAdmission::new(),
        );
        let response = ModelDeploymentServiceContract::estimate_local_model_resources(
            &service,
            Request::new(EstimateLocalModelResourcesRequest {
                model_id: "model-one".to_owned(),
                profile_id: "profile-one".to_owned(),
                configuration: Some(
                    dotcore_proto::dotcore::model::v1::LocalModelDeploymentConfiguration {
                        model_artifact_binding_id: "binding-one".to_owned(),
                        engine_parameter_values: Some(Default::default()),
                        custom_argument_tokens: Vec::new(),
                        option_overrides: Vec::new(),
                    },
                ),
                placement: None,
            }),
        )
        .await
        .expect("resource estimate response")
        .into_inner();

        assert_eq!(
            response.hardware_class_estimates,
            vec![
                dotcore_proto::dotcore::model::v1::HardwareClassResourceEstimate {
                    hardware_class_id: "fixture-class".to_owned(),
                    estimates: vec![dotcore_proto::dotcore::model::v1::ScalarResourceEstimate {
                        resource: "acceleratorMemory".to_owned(),
                        value: 112,
                        unit: "bytes".to_owned(),
                    }],
                    derived_engine_parameter_values: None,
                    launch_rejection: None,
                }
            ]
        );
    }

    #[tokio::test]
    async fn resource_estimate_handler_rejects_malformed_selection_before_the_controller() {
        for request in [
            EstimateLocalModelResourcesRequest::default(),
            EstimateLocalModelResourcesRequest {
                model_id: "model-one".to_owned(),
                profile_id: "profile-one".to_owned(),
                ..Default::default()
            },
            EstimateLocalModelResourcesRequest {
                model_id: "model-one".to_owned(),
                profile_id: "profile-one".to_owned(),
                configuration: Some(Default::default()),
                placement: Some(dotcore_proto::dotcore::deployment::v1::ExactPlacement {
                    site_node_id: "node one".to_owned(),
                    execution_target_id: "target-one".to_owned(),
                }),
            },
        ] {
            let service = ModelDeploymentService::new(
                TestController::accepted(),
                crate::management_logs::ManagementLogAdmission::new(),
            );
            let error = ModelDeploymentServiceContract::estimate_local_model_resources(
                &service,
                Request::new(request),
            )
            .await
            .expect_err("malformed estimate request");
            assert_eq!(error.code(), tonic::Code::InvalidArgument);
        }
    }

    #[test]
    fn local_selection_requires_every_explicit_identifier() {
        for (selection, message) in [
            (
                CreateLocalModelDeploymentRequest::default(),
                "Local model deployment requires a valid model ID",
            ),
            (
                CreateLocalModelDeploymentRequest {
                    model_id: "model-one".to_owned(),
                    ..Default::default()
                },
                "Local model deployment requires a valid profile ID",
            ),
            (
                CreateLocalModelDeploymentRequest {
                    model_id: "model-one".to_owned(),
                    profile_id: "profile-one".to_owned(),
                    ..Default::default()
                },
                "Local model deployment requires exact placement",
            ),
            (
                CreateLocalModelDeploymentRequest {
                    model_id: "model-one".to_owned(),
                    profile_id: "profile-one".to_owned(),
                    placement: Some(dotcore_proto::dotcore::deployment::v1::ExactPlacement {
                        site_node_id: "node-one".to_owned(),
                        execution_target_id: String::new(),
                    }),
                    ..Default::default()
                },
                "Local model deployment requires valid placement IDs",
            ),
        ] {
            let error = parse_local_selection(selection).expect_err("selection is incomplete");
            assert_eq!(error.code(), tonic::Code::InvalidArgument);
            assert_eq!(error.message(), message);
        }
    }

    #[test]
    fn local_selection_leaves_an_omitted_engine_configuration_to_the_profile_defaults() {
        let placement = dotcore_proto::dotcore::deployment::v1::ExactPlacement {
            site_node_id: "node-one".to_owned(),
            execution_target_id: "target-one".to_owned(),
        };
        let selection = |configuration| CreateLocalModelDeploymentRequest {
            model_id: "model-one".to_owned(),
            profile_id: "profile-one".to_owned(),
            placement: Some(placement.clone()),
            configuration,
        };

        let (_, _, _, configuration) =
            parse_local_selection(selection(Some(Default::default()))).expect("omitted defaults");
        assert_eq!(configuration, Default::default());

        let error = parse_local_selection(selection(None)).expect_err("configuration is required");
        assert_eq!(
            error.message(),
            "Local model deployment requires complete configuration"
        );

        let error = parse_local_selection(selection(Some(
            dotcore_proto::dotcore::model::v1::LocalModelDeploymentConfiguration {
                model_artifact_binding_id: "Not An ID".to_owned(),
                ..Default::default()
            },
        )))
        .expect_err("malformed binding ID");
        assert_eq!(error.code(), tonic::Code::InvalidArgument);
        assert_eq!(
            error.message(),
            "Local model deployment requires a valid model artifact binding ID"
        );
    }

    #[tokio::test]
    async fn create_and_estimate_handlers_accept_an_omitted_engine_configuration() {
        let service = ModelDeploymentService::new(
            TestController::accepted(),
            crate::management_logs::ManagementLogAdmission::new(),
        );
        let created = ModelDeploymentServiceContract::create_model_deployment(
            &service,
            Request::new(CreateModelDeploymentRequest {
                source: Some(CreateModelDeploymentSource::Local(
                    CreateLocalModelDeploymentRequest {
                        model_id: "model-one".to_owned(),
                        profile_id: "profile-one".to_owned(),
                        placement: Some(dotcore_proto::dotcore::deployment::v1::ExactPlacement {
                            site_node_id: "node-one".to_owned(),
                            execution_target_id: "target-one".to_owned(),
                        }),
                        configuration: Some(Default::default()),
                    },
                )),
                exposure: Some(ModelDeploymentExposure {
                    kind: Some(ModelDeploymentExposureKind::Hidden(Default::default())),
                }),
                ..Default::default()
            }),
        )
        .await
        .expect("create with profile defaults")
        .into_inner();
        assert_eq!(created.deployment, Some(ModelDeployment::default()));

        let estimated = ModelDeploymentServiceContract::estimate_local_model_resources(
            &service,
            Request::new(EstimateLocalModelResourcesRequest {
                model_id: "model-one".to_owned(),
                profile_id: "profile-one".to_owned(),
                configuration: Some(Default::default()),
                placement: None,
            }),
        )
        .await
        .expect("estimate with profile defaults")
        .into_inner();
        assert_eq!(estimated.hardware_class_estimates.len(), 1);
        assert_eq!(
            estimated.hardware_class_estimates[0].estimates[0].value,
            112
        );
    }

    #[tokio::test]
    async fn create_handler_requires_source_and_exposure() {
        let service = ModelDeploymentService::new(
            TestController::accepted(),
            crate::management_logs::ManagementLogAdmission::new(),
        );
        let missing_source = ModelDeploymentServiceContract::create_model_deployment(
            &service,
            Request::new(CreateModelDeploymentRequest {
                exposure: Some(ModelDeploymentExposure {
                    kind: Some(ModelDeploymentExposureKind::Hidden(Default::default())),
                }),
                ..Default::default()
            }),
        )
        .await
        .expect_err("source is required");
        assert_eq!(missing_source.code(), tonic::Code::InvalidArgument);
        assert_eq!(
            missing_source.message(),
            "Model deployment creation requires one source"
        );

        let missing_exposure = ModelDeploymentServiceContract::create_model_deployment(
            &service,
            Request::new(CreateModelDeploymentRequest {
                source: Some(CreateModelDeploymentSource::Local(
                    CreateLocalModelDeploymentRequest {
                        model_id: "model-one".to_owned(),
                        profile_id: "profile-one".to_owned(),
                        placement: Some(dotcore_proto::dotcore::deployment::v1::ExactPlacement {
                            site_node_id: "node-one".to_owned(),
                            execution_target_id: "target-one".to_owned(),
                        }),
                        configuration: Some(Default::default()),
                    },
                )),
                ..Default::default()
            }),
        )
        .await
        .expect_err("exposure is required");
        assert_eq!(missing_exposure.code(), tonic::Code::InvalidArgument);
        assert_eq!(
            missing_exposure.message(),
            "Model deployment exposure is required"
        );
    }

    fn application_route_blocker(
        application_deployment_id: &str,
        application_deployment_name: &str,
        inference_route_id: &str,
        inference_route_name: &str,
    ) -> ModelTeardownApplicationRouteBlocker {
        ModelTeardownApplicationRouteBlocker {
            application_deployment_id: application_deployment_id.to_owned(),
            application_deployment_name: application_deployment_name.to_owned(),
            inference_route_id: inference_route_id.to_owned(),
            inference_route_name: inference_route_name.to_owned(),
        }
    }

    fn preflight_error(
        summary: &'static str,
        primary_cause: &'static str,
        request_url: impl Into<Box<str>>,
        http_status: Option<u32>,
        response_body: Option<Vec<u8>>,
    ) -> ModelLifecycleError {
        ModelLifecycleError::upstream_preflight(
            summary,
            primary_cause,
            request_url,
            http_status,
            response_body.map(Vec::into_boxed_slice),
            None,
        )
    }

    #[tokio::test]
    async fn deployment_log_transport_validates_exact_inputs_and_maps_safe_response_values() {
        let service = ModelDeploymentService::new(
            TestController::accepted(),
            crate::management_logs::ManagementLogAdmission::new(),
        );
        for request in [
            ReadDeploymentLogsRequest {
                page_size: 1,
                source_names: vec!["model".to_owned(), "model".to_owned()],
                ..Default::default()
            },
            ReadDeploymentLogsRequest {
                page_size: 1,
                before_timestamp: Some(Timestamp {
                    seconds: -1,
                    nanos: 0,
                }),
                ..Default::default()
            },
            ReadDeploymentLogsRequest {
                page_size: 1,
                before_timestamp: Some(Timestamp {
                    seconds: 1,
                    nanos: 1_000_000_000,
                }),
                ..Default::default()
            },
        ] {
            let error = ModelDeploymentServiceContract::read_deployment_logs(
                &service,
                Request::new(request),
            )
            .await
            .expect_err("invalid request");
            assert_eq!(error.code(), tonic::Code::InvalidArgument);
        }
        let oversized = ModelDeploymentServiceContract::read_deployment_logs(
            &service,
            Request::new(ReadDeploymentLogsRequest {
                page_size: 201,
                ..Default::default()
            }),
        )
        .await
        .expect_err("page size above the maximum");
        assert_eq!(
            oversized.message(),
            "Deployment log page size must be from 1 through 200, or 0 for the default"
        );
        for (page_size, expected) in [(0, 100), (200, 150)] {
            let response = ModelDeploymentServiceContract::read_deployment_logs(
                &service,
                Request::new(ReadDeploymentLogsRequest {
                    model_deployment_id: "deployment".to_owned(),
                    page_size,
                    ..Default::default()
                }),
            )
            .await
            .expect("bounded read response")
            .into_inner();
            assert_eq!(response.entries.len(), expected, "{page_size}");
        }
        let response = ModelDeploymentServiceContract::read_deployment_logs(
            &service,
            Request::new(ReadDeploymentLogsRequest {
                model_deployment_id: "deployment".to_owned(),
                source_names: vec!["model".to_owned()],
                page_size: 1,
                ..Default::default()
            }),
        )
        .await
        .expect("read response")
        .into_inner();
        assert_eq!(response.entries.len(), 1);
        assert_eq!(response.entries[0].source_name, "model");
        assert_eq!(response.entries[0].source_label, "Model");
        assert_eq!(response.entries[0].content, b"[REDACTED]");
        assert_eq!(
            response.entries[0].timestamp,
            Some(Timestamp {
                seconds: 3,
                nanos: 0
            })
        );
        assert_eq!(
            response.latest_positions,
            vec![DeploymentLogResumePosition {
                source_name: "model".to_owned(),
                latest_timestamp: Some(Timestamp {
                    seconds: 3,
                    nanos: 0
                }),
            }]
        );

        for request in [
            FollowDeploymentLogsRequest {
                source_names: vec!["model".to_owned()],
                resume_positions: vec![DeploymentLogResumePosition {
                    source_name: "other".to_owned(),
                    latest_timestamp: Some(Timestamp {
                        seconds: 1,
                        nanos: 0,
                    }),
                }],
                ..Default::default()
            },
            FollowDeploymentLogsRequest {
                resume_positions: vec![DeploymentLogResumePosition {
                    source_name: "other".to_owned(),
                    latest_timestamp: Some(Timestamp {
                        seconds: 1,
                        nanos: 0,
                    }),
                }],
                ..Default::default()
            },
            FollowDeploymentLogsRequest {
                resume_positions: vec![DeploymentLogResumePosition {
                    source_name: "model".to_owned(),
                    latest_timestamp: None,
                }],
                ..Default::default()
            },
        ] {
            let error = ModelDeploymentServiceContract::follow_deployment_logs(
                &service,
                Request::new(request),
            )
            .await;
            let error = match error {
                Ok(_) => panic!("invalid follow request was accepted"),
                Err(error) => error,
            };
            assert_eq!(error.code(), tonic::Code::InvalidArgument);
        }
    }

    #[test]
    fn a_follow_of_every_source_resumes_the_workload_and_each_preparation_step() {
        let position = |source_name: &str, seconds| DeploymentLogResumePosition {
            source_name: source_name.to_owned(),
            latest_timestamp: Some(Timestamp { seconds, nanos: 7 }),
        };
        let at = |seconds| UNIX_EPOCH + Duration::new(seconds, 7);

        let accepted = parse_follow_query(
            Vec::new(),
            vec![
                position("model", 4),
                position("prepare:download", 1),
                position("prepare:engine-build", 2),
                position("prepare:engine-image", 3),
            ],
        )
        .expect("every source a model follow may deliver resumes");
        let application_step =
            parse_follow_query(Vec::new(), vec![position("prepare:web:build", 1)]);
        let repeated = parse_follow_query(
            Vec::new(),
            vec![
                position("prepare:download", 1),
                position("prepare:download", 2),
            ],
        );

        assert_eq!(
            accepted,
            (
                Vec::new(),
                BTreeMap::from([
                    ("model".to_owned(), at(4)),
                    ("prepare:download".to_owned(), at(1)),
                    ("prepare:engine-build".to_owned(), at(2)),
                    ("prepare:engine-image".to_owned(), at(3)),
                ])
            )
        );
        assert_eq!(
            application_step.expect_err("an application step").code(),
            Code::InvalidArgument
        );
        assert_eq!(
            repeated.expect_err("a repeated source").code(),
            Code::InvalidArgument
        );
    }

    #[tokio::test]
    async fn metadata_handler_returns_the_exact_accepted_deployment() {
        let service = ModelDeploymentService::new(
            TestController::accepted(),
            crate::management_logs::ManagementLogAdmission::new(),
        );
        let response = ModelDeploymentServiceContract::update_model_deployment_metadata(
            &service,
            Request::new(metadata_request(Some("  exact accepted name  "))),
        )
        .await
        .expect("rename response")
        .into_inner();

        assert_eq!(
            response.deployment,
            Some(ModelDeployment {
                model_deployment_id: "deployment-one".to_owned(),
                model_deployment_name: "exact accepted name".to_owned(),
                ..Default::default()
            })
        );
    }

    #[tokio::test]
    async fn metadata_handler_preserves_name_lifecycle_and_persistence_outcomes() {
        for (outcome, code, message) in [
            (
                RenameOutcome::InvalidName,
                tonic::Code::InvalidArgument,
                "Model deployment name must be 1–128 non-control characters after trimming",
            ),
            (
                RenameOutcome::NotFound,
                tonic::Code::NotFound,
                "model lifecycle resource was not found",
            ),
            (
                RenameOutcome::Teardown,
                tonic::Code::FailedPrecondition,
                "model lifecycle operation is not valid for the current state",
            ),
            (
                RenameOutcome::PersistenceFailure,
                tonic::Code::FailedPrecondition,
                "Model lifecycle configuration failed",
            ),
            (
                RenameOutcome::OutcomeUncertain,
                tonic::Code::Unavailable,
                "Model lifecycle outcome is uncertain. Retry the operation or inspect current state.",
            ),
            (
                RenameOutcome::AcceptedPersistenceOutcome,
                tonic::Code::Unavailable,
                "Upstream model replacement persistence failed. The replacement remains active in memory but will be lost after restart unless persistence is repaired.",
            ),
        ] {
            let service = ModelDeploymentService::new(
                TestController { outcome },
                crate::management_logs::ManagementLogAdmission::new(),
            );
            let error = ModelDeploymentServiceContract::update_model_deployment_metadata(
                &service,
                Request::new(metadata_request(Some("requested name"))),
            )
            .await
            .expect_err("rename error");
            assert_eq!(error.code(), code);
            assert_eq!(error.message(), message);
        }
    }

    #[tokio::test]
    async fn metadata_handler_rejects_malformed_patches_before_the_controller() {
        for request in [
            UpdateModelDeploymentMetadataRequest::default(),
            UpdateModelDeploymentMetadataRequest {
                metadata: Some(ModelDeploymentMetadata::default()),
                ..Default::default()
            },
            UpdateModelDeploymentMetadataRequest {
                metadata: Some(ModelDeploymentMetadata::default()),
                update_mask: Some(FieldMask::default()),
                ..Default::default()
            },
            UpdateModelDeploymentMetadataRequest {
                metadata: Some(ModelDeploymentMetadata {
                    display_name: Some("valid".to_owned()),
                }),
                update_mask: Some(FieldMask {
                    paths: vec!["*".to_owned()],
                }),
                ..Default::default()
            },
            UpdateModelDeploymentMetadataRequest {
                metadata: Some(ModelDeploymentMetadata {
                    display_name: Some("valid".to_owned()),
                }),
                update_mask: Some(FieldMask {
                    paths: vec!["display_name".to_owned(), "display_name".to_owned()],
                }),
                ..Default::default()
            },
            UpdateModelDeploymentMetadataRequest {
                metadata: Some(ModelDeploymentMetadata {
                    display_name: Some("valid".to_owned()),
                }),
                update_mask: Some(FieldMask {
                    paths: vec!["future_field".to_owned()],
                }),
                ..Default::default()
            },
            UpdateModelDeploymentMetadataRequest {
                metadata: Some(ModelDeploymentMetadata {
                    display_name: Some("valid".to_owned()),
                }),
                update_mask: Some(FieldMask {
                    paths: vec!["displayName".to_owned()],
                }),
                ..Default::default()
            },
            UpdateModelDeploymentMetadataRequest {
                metadata: Some(ModelDeploymentMetadata {
                    display_name: Some("valid".to_owned()),
                }),
                update_mask: Some(FieldMask {
                    paths: vec!["metadata.display_name".to_owned()],
                }),
                ..Default::default()
            },
            UpdateModelDeploymentMetadataRequest {
                metadata: Some(ModelDeploymentMetadata {
                    display_name: Some("valid".to_owned()),
                }),
                update_mask: Some(FieldMask {
                    paths: vec!["display_name".to_owned(), "future_field".to_owned()],
                }),
                ..Default::default()
            },
            UpdateModelDeploymentMetadataRequest {
                metadata: Some(ModelDeploymentMetadata::default()),
                update_mask: Some(FieldMask {
                    paths: vec!["display_name".to_owned()],
                }),
                ..Default::default()
            },
            metadata_request(Some(" \t ")),
        ] {
            let service = ModelDeploymentService::new(
                TestController::accepted(),
                crate::management_logs::ManagementLogAdmission::new(),
            );
            let error = ModelDeploymentServiceContract::update_model_deployment_metadata(
                &service,
                Request::new(request),
            )
            .await
            .expect_err("invalid metadata patch");
            assert_eq!(error.code(), tonic::Code::InvalidArgument);
        }
    }

    #[derive(Clone, Copy)]
    struct TestController {
        outcome: RenameOutcome,
    }

    impl TestController {
        fn accepted() -> Self {
            Self {
                outcome: RenameOutcome::Accepted,
            }
        }
    }

    #[derive(Clone, Copy)]
    enum RenameOutcome {
        Accepted,

        InvalidName,

        NotFound,

        Teardown,

        OutcomeUncertain,

        PersistenceFailure,

        AcceptedPersistenceOutcome,
    }

    impl ModelLifecycleController for TestController {
        fn list_deployments(
            &self,
        ) -> impl std::future::Future<
            Output = Result<ListModelDeploymentsResponse, ModelLifecycleError>,
        > + Send {
            ready(Ok(ListModelDeploymentsResponse::default()))
        }
        fn estimate_local_model_resources(
            &self,
            _model_id: String,
            _profile_id: String,
            _configuration: dotcore_proto::dotcore::model::v1::LocalModelDeploymentConfiguration,
            _placement: Option<dotcore_proto::dotcore::deployment::v1::ExactPlacement>,
        ) -> impl std::future::Future<
            Output = Result<
                dotcore_proto::dotcore::model::v1::EstimateLocalModelResourcesResponse,
                ModelLifecycleError,
            >,
        > + Send {
            ready(Ok(
                dotcore_proto::dotcore::model::v1::EstimateLocalModelResourcesResponse {
                    hardware_class_estimates: vec![
                        dotcore_proto::dotcore::model::v1::HardwareClassResourceEstimate {
                            hardware_class_id: "fixture-class".to_owned(),
                            estimates: vec![
                                dotcore_proto::dotcore::model::v1::ScalarResourceEstimate {
                                    resource: "acceleratorMemory".to_owned(),
                                    value: 112,
                                    unit: "bytes".to_owned(),
                                },
                            ],
                            derived_engine_parameter_values: None,
                            launch_rejection: None,
                        },
                    ],
                    placement_preview: None,
                },
            ))
        }
        fn load_model(
            &self,
            _model_id: CatalogModelId,
            _profile_id: String,
            _placement: dotcore_proto::dotcore::deployment::v1::ExactPlacement,
            _configuration: dotcore_proto::dotcore::model::v1::LocalModelDeploymentConfiguration,
            _exposure: ModelDeploymentExposureIntent,
            _name: Option<String>,
        ) -> impl std::future::Future<Output = Result<ModelDeployment, ModelLifecycleError>> + Send
        {
            ready(Ok(ModelDeployment::default()))
        }
        fn load_upstream_model(
            &self,
            _request: CreateUpstreamModelDeploymentRequest,
            _exposure: ModelDeploymentExposureIntent,
            _name: Option<String>,
        ) -> impl std::future::Future<Output = Result<ModelDeployment, ModelLifecycleError>> + Send
        {
            ready(Ok(ModelDeployment::default()))
        }
        fn update_metadata(
            &self,
            deployment_id: ModelDeploymentId,
            patch: DeploymentMetadataPatch,
        ) -> impl std::future::Future<Output = Result<ModelDeployment, ModelLifecycleError>> + Send
        {
            let name = patch.display_name().to_owned();
            let result = match self.outcome {
                RenameOutcome::Accepted => Ok(ModelDeployment {
                    model_deployment_id: deployment_id.0,
                    model_deployment_name: name.trim().to_owned(),
                    ..Default::default()
                }),
                RenameOutcome::InvalidName => Err(ModelLifecycleError::InvalidName),
                RenameOutcome::NotFound => Err(ModelLifecycleError::NotFound),
                RenameOutcome::Teardown => Err(ModelLifecycleError::FailedPrecondition),
                RenameOutcome::PersistenceFailure => Err(ModelLifecycleError::Configuration(
                    anyhow::anyhow!("injected"),
                )),
                RenameOutcome::OutcomeUncertain => Err(ModelLifecycleError::OutcomeUncertain(
                    anyhow::anyhow!("injected"),
                )),
                RenameOutcome::AcceptedPersistenceOutcome => Err(
                    ModelLifecycleError::DesiredPersistence(anyhow::anyhow!("injected")),
                ),
            };
            ready(result)
        }
        fn update_upstream_model(
            &self,
            _deployment_id: ModelDeploymentId,
            _replacement: UpstreamModelDesiredConfiguration,
        ) -> impl std::future::Future<Output = Result<ModelDeployment, ModelLifecycleError>> + Send
        {
            ready(Ok(ModelDeployment::default()))
        }
        fn unload_model(
            &self,
            _deployment_id: ModelDeploymentId,
        ) -> impl std::future::Future<Output = Result<ModelDeployment, ModelLifecycleError>> + Send
        {
            ready(Ok(ModelDeployment::default()))
        }
        fn pause_model(
            &self,
            _deployment_id: ModelDeploymentId,
        ) -> impl std::future::Future<Output = Result<ModelDeployment, ModelLifecycleError>> + Send
        {
            ready(Ok(ModelDeployment::default()))
        }
        fn resume_model(
            &self,
            _deployment_id: ModelDeploymentId,
        ) -> impl std::future::Future<Output = Result<ModelDeployment, ModelLifecycleError>> + Send
        {
            ready(Ok(ModelDeployment::default()))
        }
    }

    impl ModelDeploymentLogController for TestController {
        /// Returns the newest entries of a synthetic 150-entry history, bounded by the query
        /// limit as the runtime bounds it.
        fn read_deployment_logs(
            &self,
            _: ModelDeploymentId,
            query: crate::management_logs::DeploymentLogReadQuery,
        ) -> impl std::future::Future<
            Output = Result<
                Vec<dotcore_runtime::RuntimeLogSourceHistory>,
                crate::management_logs::DeploymentLogError,
            >,
        > + Send {
            let limit = usize::from(query.history().maximum_entries().get());
            let entry = dotcore_runtime::RuntimeLogEntry {
                source: dotcore_runtime::RuntimeLogSource {
                    name: "model".to_owned(),
                    label: "Model".to_owned(),
                },
                timestamp: UNIX_EPOCH + Duration::from_secs(3),
                stream: dotcore_runtime::RuntimeLogStreamKind::Stdout,
                content: b"synthetic-required-launch-secret".to_vec(),
                truncated: false,
            };
            ready(Ok(vec![dotcore_runtime::RuntimeLogSourceHistory {
                source: dotcore_runtime::RuntimeLogSource {
                    name: "model".to_owned(),
                    label: "Model".to_owned(),
                },
                result: Ok(dotcore_runtime::RuntimeLogHistory {
                    entries: vec![entry; SYNTHETIC_LOG_ENTRIES.min(limit)],
                    has_older_output: limit < SYNTHETIC_LOG_ENTRIES,
                    candidate_budget_truncated: false,
                }),
            }]))
        }

        fn follow_deployment_logs(
            &self,
            _: ModelDeploymentId,
            _: crate::management_logs::DeploymentLogFollowQuery,
        ) -> impl std::future::Future<
            Output = Result<
                crate::management_logs::DeploymentLogStreams,
                crate::management_logs::DeploymentLogError,
            >,
        > + Send {
            ready(Err(
                crate::management_logs::DeploymentLogError::UnsupportedDeployment,
            ))
        }

        fn protected_log_values(
            &self,
            _: ModelDeploymentId,
        ) -> impl std::future::Future<
            Output = Result<
                crate::management_logs::ProtectedLogValues,
                crate::management_logs::DeploymentLogError,
            >,
        > + Send {
            ready(Ok(crate::management_logs::ProtectedLogValues::new([
                "synthetic-required-launch-secret".to_owned(),
            ])))
        }
    }

    /// Entry count of the synthetic deployment log history the test controller reads.
    const SYNTHETIC_LOG_ENTRIES: usize = 150;

    fn metadata_request(display_name: Option<&str>) -> UpdateModelDeploymentMetadataRequest {
        UpdateModelDeploymentMetadataRequest {
            model_deployment_id: "deployment-one".to_owned(),
            metadata: Some(ModelDeploymentMetadata {
                display_name: display_name.map(str::to_owned),
            }),
            update_mask: Some(FieldMask {
                paths: vec!["display_name".to_owned()],
            }),
        }
    }
}
