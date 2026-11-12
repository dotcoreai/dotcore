//! gRPC service edge for apps-controller-owned application deployments.

use std::{
    collections::{BTreeMap, BTreeSet},
    num::NonZeroU16,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use dotcore_proto::dotcore::application::v1::{
    CreateApplicationDeploymentRequest, CreateApplicationDeploymentResponse,
    FollowDeploymentLogsRequest, FollowDeploymentLogsResponse, GetApplicationDeploymentRequest,
    GetApplicationDeploymentResponse, ListApplicationDeploymentsRequest,
    ListApplicationDeploymentsResponse, ListApplicationTargetsRequest,
    ListApplicationTargetsResponse, PauseDeploymentRequest, PauseDeploymentResponse,
    ReadDeploymentLogsRequest, ReadDeploymentLogsResponse, RemoveApplicationDeploymentRequest,
    RemoveApplicationDeploymentResponse, ResumeDeploymentRequest, ResumeDeploymentResponse,
    UpdateApplicationDeploymentMetadataRequest, UpdateApplicationDeploymentMetadataResponse,
    application_deployment_service_server::ApplicationDeploymentService as ApplicationServiceContract,
};
use futures_util::Stream;
use tonic::{Request, Response, Status};

use super::{
    ApplicationDeploymentLogController, ApplicationLifecycleController, ApplicationLifecycleError,
    InferenceRouteResolutionError,
};

/// Translates application service requests to the owning apps controller.
#[derive(Debug)]
pub struct ApplicationService<C: ApplicationLifecycleController> {
    controller: C,
    #[allow(
        dead_code,
        reason = "the deployment-log protocol consumes its composed admission capability"
    )]
    log_admission: crate::management_logs::ManagementLogAdmission,
}

impl<C: ApplicationLifecycleController> ApplicationService<C> {
    /// Creates the transport-facing application service around one apps controller.
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
}

impl<C: ApplicationLifecycleController> ApplicationService<C> {
    pub(crate) fn observer_controller(&self) -> C
    where
        C: Clone,
    {
        self.controller.clone()
    }
}

#[tonic::async_trait]
impl<C> ApplicationServiceContract for ApplicationService<C>
where
    C: ApplicationLifecycleController + ApplicationDeploymentLogController + 'static,
{
    async fn list_application_deployments(
        &self,
        _request: Request<ListApplicationDeploymentsRequest>,
    ) -> Result<Response<ListApplicationDeploymentsResponse>, Status> {
        self.controller
            .list_deployments()
            .await
            .map(Response::new)
            .map_err(|error| error.to_tonic_status())
    }

    async fn list_application_targets(
        &self,
        request: Request<ListApplicationTargetsRequest>,
    ) -> Result<Response<ListApplicationTargetsResponse>, Status> {
        self.controller
            .list_targets(request.into_inner().application_id)
            .await
            .map(Response::new)
            .map_err(|error| error.to_tonic_status())
    }

    async fn get_application_deployment(
        &self,
        request: Request<GetApplicationDeploymentRequest>,
    ) -> Result<Response<GetApplicationDeploymentResponse>, Status> {
        self.controller
            .get_deployment(request.into_inner().application_deployment_id)
            .await
            .map(|deployment| {
                Response::new(GetApplicationDeploymentResponse {
                    deployment: Some(deployment),
                })
            })
            .map_err(|error| error.to_tonic_status())
    }

    async fn create_application_deployment(
        &self,
        request: Request<CreateApplicationDeploymentRequest>,
    ) -> Result<Response<CreateApplicationDeploymentResponse>, Status> {
        self.controller
            .create_deployment(request.into_inner())
            .await
            .map(|deployment| {
                Response::new(CreateApplicationDeploymentResponse {
                    deployment: Some(deployment),
                })
            })
            .map_err(|error| error.to_tonic_status())
    }

    async fn remove_application_deployment(
        &self,
        request: Request<RemoveApplicationDeploymentRequest>,
    ) -> Result<Response<RemoveApplicationDeploymentResponse>, Status> {
        self.controller
            .remove_deployment(request.into_inner().application_deployment_id)
            .await
            .map(|deployment| {
                Response::new(RemoveApplicationDeploymentResponse {
                    deployment: Some(deployment),
                })
            })
            .map_err(|error| error.to_tonic_status())
    }

    async fn update_application_deployment_metadata(
        &self,
        request: Request<UpdateApplicationDeploymentMetadataRequest>,
    ) -> Result<Response<UpdateApplicationDeploymentMetadataResponse>, Status> {
        let request = request.into_inner();
        let patch = parse_metadata_patch(request.metadata, request.update_mask)?;
        self.controller
            .update_metadata(request.application_deployment_id, patch)
            .await
            .map(|deployment| {
                Response::new(UpdateApplicationDeploymentMetadataResponse {
                    deployment: Some(deployment),
                })
            })
            .map_err(|error| error.to_tonic_status())
    }

    async fn pause_deployment(
        &self,
        request: Request<PauseDeploymentRequest>,
    ) -> Result<Response<PauseDeploymentResponse>, Status> {
        self.controller
            .pause_deployment(request.into_inner().application_deployment_id)
            .await
            .map(|deployment| {
                Response::new(PauseDeploymentResponse {
                    deployment: Some(deployment),
                })
            })
            .map_err(|error| error.to_tonic_status())
    }

    async fn resume_deployment(
        &self,
        request: Request<ResumeDeploymentRequest>,
    ) -> Result<Response<ResumeDeploymentResponse>, Status> {
        self.controller
            .resume_deployment(request.into_inner().application_deployment_id)
            .await
            .map(|deployment| {
                Response::new(ResumeDeploymentResponse {
                    deployment: Some(deployment),
                })
            })
            .map_err(|error| error.to_tonic_status())
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
        let diagnostic_context = crate::management_logs::DeploymentLogDiagnosticContext::new(
            "application",
            request.application_deployment_id.clone(),
            "read_history",
        );
        let protected = self
            .controller
            .protected_log_values(request.application_deployment_id.clone())
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
            .read_deployment_logs(request.application_deployment_id, query)
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
        let (source_names, positions) =
            parse_follow_query(request.source_names, request.resume_positions)?;
        let permit = self
            .log_admission
            .try_acquire()
            .map_err(crate::management_logs::map_admission_error)?;
        let cancellation = tokio_util::sync::CancellationToken::new();
        let diagnostic_context = crate::management_logs::DeploymentLogDiagnosticContext::new(
            "application",
            request.application_deployment_id.clone(),
            "follow",
        );
        let protected = self
            .controller
            .protected_log_values(request.application_deployment_id.clone())
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
                request.application_deployment_id,
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
    positions: Vec<dotcore_proto::dotcore::application::v1::DeploymentLogResumePosition>,
) -> Result<(Vec<String>, BTreeMap<String, SystemTime>), Status> {
    validate_sources(&source_names)?;
    let mut result = BTreeMap::new();
    for position in positions {
        if position.source_name.is_empty()
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

fn timestamp_from(time: SystemTime) -> dotcore_proto::google::protobuf::Timestamp {
    let duration = time.duration_since(UNIX_EPOCH).unwrap_or_default();
    dotcore_proto::google::protobuf::Timestamp {
        seconds: duration.as_secs().try_into().unwrap_or(i64::MAX),
        nanos: duration.subsec_nanos() as i32,
    }
}

fn map_history(
    history: crate::management_logs::DeploymentLogHistory,
) -> ReadDeploymentLogsResponse {
    use dotcore_proto::dotcore::application::v1::{
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
) -> dotcore_proto::dotcore::application::v1::DeploymentLogEntry {
    use dotcore_proto::dotcore::application::v1::DeploymentLogStream;
    dotcore_proto::dotcore::application::v1::DeploymentLogEntry {
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
    use dotcore_proto::dotcore::application::v1::{
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
    metadata: Option<dotcore_proto::dotcore::application::v1::ApplicationDeploymentMetadata>,
    update_mask: Option<dotcore_proto::google::protobuf::FieldMask>,
) -> Result<dotcore_domain::deployment::metadata::DeploymentMetadataPatch, Status> {
    dotcore_domain::deployment::metadata::DeploymentMetadataPatch::parse(
        metadata
            .as_ref()
            .map(|metadata| metadata.display_name.as_deref()),
        update_mask.as_ref(),
    )
    .map_err(|error| match error {
        dotcore_domain::deployment::metadata::DeploymentMetadataPatchError::InvalidDisplayName(
            _,
        ) => Status::invalid_argument(
            "Application deployment name must be 1–128 non-control characters after trimming",
        ),
        _ => Status::invalid_argument("Application deployment metadata update is invalid"),
    })
}

trait TonicStatusExt {
    fn to_tonic_status(self) -> Status;
}

impl TonicStatusExt for ApplicationLifecycleError {
    fn to_tonic_status(self) -> Status {
        match self {
            Self::Configuration(_) => {
                Status::failed_precondition("Application lifecycle configuration failed")
            }
            Self::OutcomeUncertain(_) => Status::unavailable(
                "Application lifecycle outcome is uncertain. Retry the operation or inspect current state.",
            ),
            Self::FailedPrecondition
            | Self::IneligibleTarget { .. }
            | Self::MissingTargetObservation { .. }
            | Self::UnavailableTargetObservation { .. }
            | Self::StaleTargetObservation { .. }
            | Self::TargetPlatformMismatch { .. }
            | Self::UnsupportedServicePlatform { .. }
            | Self::RetainedTargetIncompatible(_) => Status::failed_precondition(self.to_string()),
            Self::InvalidArgument | Self::InvalidName | Self::InvalidParameter { .. } => {
                Status::invalid_argument(self.to_string())
            }
            Self::NotFound => Status::not_found(self.to_string()),
            Self::PublishedPortsExhausted => Status::resource_exhausted(self.to_string()),
            Self::InferenceRoute(error) => error.to_tonic_status(),
        }
    }
}

impl TonicStatusExt for InferenceRouteResolutionError {
    fn to_tonic_status(self) -> Status {
        match self {
            Self::Unknown | Self::NotChat => Status::failed_precondition(self.to_string()),
            Self::Stale => Status::aborted(self.to_string()),
            Self::Unavailable => Status::unavailable(self.to_string()),
        }
    }
}

#[cfg(test)]
mod tests {
    use std::{
        backtrace::BacktraceStatus,
        future::ready,
        time::{Duration, UNIX_EPOCH},
    };

    use anyhow::anyhow;
    use dotcore_proto::{
        dotcore::application::v1::{
            ApplicationDeployment, CreateApplicationDeploymentRequest, DeploymentLogResumePosition,
            ListApplicationDeploymentsResponse, ListApplicationTargetsResponse,
            ReadDeploymentLogsRequest, UpdateApplicationDeploymentMetadataRequest,
        },
        google::protobuf::Timestamp,
    };
    use tonic::{Code, Request};

    use super::{ApplicationService, ApplicationServiceContract, TonicStatusExt};
    use crate::application::{
        ApplicationDeploymentLogController, ApplicationLifecycleController,
        ApplicationLifecycleError, InferenceRouteResolutionError,
    };

    #[test]
    fn deployment_log_request_parsers_reject_exact_invalid_values() {
        for (sources, page_size, timestamp) in [
            (Vec::new(), 201, None),
            (vec!["web".to_owned(), "web".to_owned()], 1, None),
            (
                Vec::new(),
                1,
                Some(Timestamp {
                    seconds: -1,
                    nanos: 0,
                }),
            ),
            (
                Vec::new(),
                1,
                Some(Timestamp {
                    seconds: 0,
                    nanos: -1,
                }),
            ),
        ] {
            let error = super::parse_read_query(sources, page_size, timestamp)
                .expect_err("invalid read query");
            assert_eq!(error.code(), Code::InvalidArgument);
            assert_eq!(
                error.message(),
                if page_size == 201 {
                    "Deployment log page size must be from 1 through 200, or 0 for the default"
                } else if timestamp.is_some() {
                    "Deployment log timestamp is invalid"
                } else {
                    "Deployment log sources must contain at most 32 unique names"
                }
            );
        }
        for positions in [
            vec![DeploymentLogResumePosition {
                source_name: "web".to_owned(),
                latest_timestamp: None,
            }],
            vec![
                DeploymentLogResumePosition {
                    source_name: "web".to_owned(),
                    latest_timestamp: Some(Timestamp {
                        seconds: 1,
                        nanos: 0,
                    }),
                },
                DeploymentLogResumePosition {
                    source_name: "web".to_owned(),
                    latest_timestamp: Some(Timestamp {
                        seconds: 2,
                        nanos: 0,
                    }),
                },
            ],
        ] {
            let error = super::parse_follow_query(vec!["web".to_owned()], positions)
                .expect_err("invalid resume positions");
            assert_eq!(error.code(), Code::InvalidArgument);
        }
    }

    #[tokio::test]
    async fn deployment_log_page_size_defaults_when_unset_and_bounds_the_entries() {
        let service = ApplicationService::new(
            TestController::accepted(),
            crate::management_logs::ManagementLogAdmission::new(),
        );
        for (page_size, expected) in [(0, 100), (200, 150)] {
            let response = ApplicationServiceContract::read_deployment_logs(
                &service,
                Request::new(ReadDeploymentLogsRequest {
                    application_deployment_id: "web-ui".to_owned(),
                    page_size,
                    ..Default::default()
                }),
            )
            .await
            .expect("bounded read response")
            .into_inner();
            assert_eq!(response.entries.len(), expected, "{page_size}");
        }
    }

    #[tokio::test]
    async fn deployment_log_history_remains_available_when_follow_admission_is_exhausted() {
        let service = ApplicationService::new(
            TestController::accepted(),
            crate::management_logs::ManagementLogAdmission::new(),
        );
        let admission = service.log_admission();
        let permits = (0..8)
            .map(|_| admission.try_acquire().expect("admission permit"))
            .collect::<Vec<_>>();
        let response = ApplicationServiceContract::read_deployment_logs(
            &service,
            Request::new(ReadDeploymentLogsRequest {
                application_deployment_id: "web-ui".to_owned(),
                page_size: 1,
                ..Default::default()
            }),
        )
        .await
        .expect("history does not consume follow admission")
        .into_inner();
        assert_eq!(response.entries.len(), 1);
        assert_eq!(response.entries[0].content, b"[REDACTED]");
        drop(permits);
    }

    #[tokio::test]
    async fn deployment_log_handler_filters_application_values_and_maps_entries() {
        let service = ApplicationService::new(
            TestController::accepted(),
            crate::management_logs::ManagementLogAdmission::new(),
        );
        let response = ApplicationServiceContract::read_deployment_logs(
            &service,
            Request::new(ReadDeploymentLogsRequest {
                application_deployment_id: "web-ui".to_owned(),
                source_names: vec!["web".to_owned()],
                page_size: 1,
                ..Default::default()
            }),
        )
        .await
        .expect("application log response")
        .into_inner();
        assert_eq!(response.sources.len(), 1);
        assert_eq!(response.sources[0].source_name, "web");
        assert_eq!(response.entries.len(), 1);
        assert_eq!(response.entries[0].source_name, "web");
        assert_eq!(response.entries[0].content, b"[REDACTED]");
        assert_eq!(
            response.entries[0].timestamp,
            Some(Timestamp {
                seconds: 4,
                nanos: 0
            })
        );
    }

    #[test]
    fn lifecycle_errors_preserve_the_status_matrix() {
        for (error, expected_code, expected_message) in [
            (
                ApplicationLifecycleError::Configuration(anyhow!(
                    "repository path: /private/state"
                )),
                Code::FailedPrecondition,
                "Application lifecycle configuration failed",
            ),
            (
                ApplicationLifecycleError::OutcomeUncertain(anyhow!(
                    "runtime adapter: socket timeout"
                )),
                Code::Unavailable,
                "Application lifecycle outcome is uncertain. Retry the operation or inspect current state.",
            ),
            (
                ApplicationLifecycleError::InvalidArgument,
                Code::InvalidArgument,
                "Application lifecycle request contains an invalid ID",
            ),
            (
                ApplicationLifecycleError::InvalidParameter {
                    parameter_id: "mode".to_owned(),
                    reason: crate::application::ParameterRejection::InvalidValue(anyhow!(
                        "\"lowvram\" is not one of [\"auto\"]"
                    )),
                },
                Code::InvalidArgument,
                "Application parameter \"mode\" has an invalid value: \"lowvram\" is not one of [\"auto\"]",
            ),
            (
                ApplicationLifecycleError::NotFound,
                Code::NotFound,
                "Application deployment was not found",
            ),
            (
                ApplicationLifecycleError::FailedPrecondition,
                Code::FailedPrecondition,
                "Application lifecycle operation is not valid for the current state",
            ),
        ] {
            assert_status(error, expected_code, expected_message);
        }
    }

    #[test]
    fn oci_configuration_failure_preserves_the_fixed_public_status() {
        let error = ApplicationLifecycleError::Configuration(
            anyhow::Error::new(dotcore_domain::oci::OciReferenceError::MissingTagOrDigest)
                .context("Application OCI artifact is invalid"),
        );

        assert_status(
            error,
            Code::FailedPrecondition,
            "Application lifecycle configuration failed",
        );
    }

    #[test]
    fn inference_route_errors_preserve_the_status_matrix() {
        for (error, expected_code, expected_message) in [
            (
                InferenceRouteResolutionError::Unknown,
                Code::FailedPrecondition,
                "Inference route is unknown",
            ),
            (
                InferenceRouteResolutionError::Stale,
                Code::Aborted,
                "Inference route generation is stale",
            ),
            (
                InferenceRouteResolutionError::Unavailable,
                Code::Unavailable,
                "Inference routes are unavailable on this node",
            ),
        ] {
            assert_status(
                ApplicationLifecycleError::InferenceRoute(error),
                expected_code,
                expected_message,
            );
        }
    }

    #[test]
    fn application_errors_retain_contextual_sources_and_source_less_semantic_states() {
        for (error, expected_source) in [
            (
                ApplicationLifecycleError::Configuration(anyhow!(
                    "repository path: /private/state"
                )),
                "repository path: /private/state",
            ),
            (
                ApplicationLifecycleError::OutcomeUncertain(anyhow!(
                    "runtime adapter: socket timeout"
                )),
                "runtime adapter: socket timeout",
            ),
        ] {
            assert!(std::error::Error::source(&error).is_some());
            let source = match error {
                ApplicationLifecycleError::Configuration(source)
                | ApplicationLifecycleError::OutcomeUncertain(source) => source,
                _ => unreachable!("source-bearing lifecycle category"),
            };
            assert_eq!(source.to_string(), expected_source);
            if source.backtrace().status() == BacktraceStatus::Captured {
                assert!(!source.backtrace().to_string().is_empty());
            }
        }

        for error in [
            ApplicationLifecycleError::InvalidArgument,
            ApplicationLifecycleError::NotFound,
            ApplicationLifecycleError::FailedPrecondition,
            ApplicationLifecycleError::InferenceRoute(InferenceRouteResolutionError::Unknown),
        ] {
            assert!(std::error::Error::source(&error).is_none());
        }

        for error in [
            InferenceRouteResolutionError::Unknown,
            InferenceRouteResolutionError::Stale,
            InferenceRouteResolutionError::Unavailable,
        ] {
            assert!(std::error::Error::source(&error).is_none());
        }
    }

    #[tokio::test]
    async fn pause_and_resume_handlers_return_controller_deployments() {
        let service = ApplicationService::new(
            TestController::accepted(),
            crate::management_logs::ManagementLogAdmission::new(),
        );
        let paused = ApplicationServiceContract::pause_deployment(
            &service,
            Request::new(
                dotcore_proto::dotcore::application::v1::PauseDeploymentRequest {
                    application_deployment_id: "deployment".to_owned(),
                },
            ),
        )
        .await
        .expect("pause response")
        .into_inner();
        assert_eq!(
            paused
                .deployment
                .expect("pause deployment")
                .application_deployment_id,
            "paused"
        );

        let resumed = ApplicationServiceContract::resume_deployment(
            &service,
            Request::new(
                dotcore_proto::dotcore::application::v1::ResumeDeploymentRequest {
                    application_deployment_id: "deployment".to_owned(),
                },
            ),
        )
        .await
        .expect("resume response")
        .into_inner();
        assert_eq!(
            resumed
                .deployment
                .expect("resume deployment")
                .application_deployment_id,
            "resumed"
        );
    }

    fn assert_status(
        error: ApplicationLifecycleError,
        expected_code: Code,
        expected_message: &str,
    ) {
        let status = error.to_tonic_status();

        assert_eq!(status.code(), expected_code);
        assert_eq!(status.message(), expected_message);
    }

    #[tokio::test]
    async fn metadata_handler_returns_the_exact_accepted_deployment() {
        let service = ApplicationService::new(
            TestController::accepted(),
            crate::management_logs::ManagementLogAdmission::new(),
        );
        let response = ApplicationServiceContract::update_application_deployment_metadata(
            &service,
            Request::new(UpdateApplicationDeploymentMetadataRequest {
                application_deployment_id: "deployment-one".to_owned(),
                metadata: Some(
                    dotcore_proto::dotcore::application::v1::ApplicationDeploymentMetadata {
                        display_name: Some("  exact accepted name  ".to_owned()),
                    },
                ),
                update_mask: Some(dotcore_proto::google::protobuf::FieldMask {
                    paths: vec!["display_name".to_owned()],
                }),
            }),
        )
        .await
        .expect("metadata response")
        .into_inner();

        assert_eq!(
            response.deployment,
            Some(ApplicationDeployment {
                application_deployment_id: "deployment-one".to_owned(),
                application_deployment_name: "exact accepted name".to_owned(),
                ..Default::default()
            })
        );
    }

    #[tokio::test]
    async fn metadata_handler_preserves_name_lifecycle_and_persistence_outcomes() {
        for (outcome, code, message) in [
            (
                MetadataOutcome::InvalidName,
                Code::InvalidArgument,
                "Application deployment name must be 1–128 non-control characters after trimming",
            ),
            (
                MetadataOutcome::NotFound,
                Code::NotFound,
                "Application deployment was not found",
            ),
            (
                MetadataOutcome::Teardown,
                Code::FailedPrecondition,
                "Application lifecycle operation is not valid for the current state",
            ),
            (
                MetadataOutcome::PersistenceFailure,
                Code::FailedPrecondition,
                "Application lifecycle configuration failed",
            ),
            (
                MetadataOutcome::OutcomeUncertain,
                Code::Unavailable,
                "Application lifecycle outcome is uncertain. Retry the operation or inspect current state.",
            ),
        ] {
            let service = ApplicationService::new(
                TestController { outcome },
                crate::management_logs::ManagementLogAdmission::new(),
            );
            let error = ApplicationServiceContract::update_application_deployment_metadata(
                &service,
                Request::new(UpdateApplicationDeploymentMetadataRequest {
                    application_deployment_id: "deployment-one".to_owned(),
                    metadata: Some(
                        dotcore_proto::dotcore::application::v1::ApplicationDeploymentMetadata {
                            display_name: Some("requested name".to_owned()),
                        },
                    ),
                    update_mask: Some(dotcore_proto::google::protobuf::FieldMask {
                        paths: vec!["display_name".to_owned()],
                    }),
                }),
            )
            .await
            .expect_err("metadata error");
            assert_eq!(error.code(), code);
            assert_eq!(error.message(), message);
        }
    }

    #[tokio::test]
    async fn metadata_handler_rejects_malformed_masks_before_controller_mutation() {
        use dotcore_proto::dotcore::application::v1::ApplicationDeploymentMetadata;
        use dotcore_proto::google::protobuf::FieldMask;

        let requests = [
            UpdateApplicationDeploymentMetadataRequest {
                application_deployment_id: "deployment-one".to_owned(),
                metadata: None,
                update_mask: Some(FieldMask {
                    paths: vec!["display_name".to_owned()],
                }),
            },
            UpdateApplicationDeploymentMetadataRequest {
                application_deployment_id: "deployment-one".to_owned(),
                metadata: Some(ApplicationDeploymentMetadata {
                    display_name: Some("name".to_owned()),
                }),
                update_mask: Some(FieldMask {
                    paths: vec!["display_name".to_owned(), "display_name".to_owned()],
                }),
            },
            UpdateApplicationDeploymentMetadataRequest {
                application_deployment_id: "deployment-one".to_owned(),
                metadata: Some(ApplicationDeploymentMetadata { display_name: None }),
                update_mask: Some(FieldMask {
                    paths: vec!["display_name".to_owned()],
                }),
            },
        ];
        for request in requests {
            let service = ApplicationService::new(
                TestController::accepted(),
                crate::management_logs::ManagementLogAdmission::new(),
            );
            let error = ApplicationServiceContract::update_application_deployment_metadata(
                &service,
                Request::new(request),
            )
            .await
            .expect_err("invalid metadata patch");
            assert_eq!(error.code(), Code::InvalidArgument);
        }
    }

    #[derive(Clone, Copy)]
    struct TestController {
        outcome: MetadataOutcome,
    }

    impl TestController {
        fn accepted() -> Self {
            Self {
                outcome: MetadataOutcome::Accepted,
            }
        }
    }

    #[derive(Clone, Copy)]
    enum MetadataOutcome {
        Accepted,

        InvalidName,

        NotFound,

        Teardown,

        PersistenceFailure,

        OutcomeUncertain,
    }

    impl ApplicationLifecycleController for TestController {
        fn list_deployments(
            &self,
        ) -> impl std::future::Future<
            Output = Result<ListApplicationDeploymentsResponse, ApplicationLifecycleError>,
        > + Send {
            ready(Ok(ListApplicationDeploymentsResponse::default()))
        }

        fn get_deployment(
            &self,
            _deployment_id: String,
        ) -> impl std::future::Future<
            Output = Result<ApplicationDeployment, ApplicationLifecycleError>,
        > + Send {
            ready(Ok(ApplicationDeployment::default()))
        }

        fn list_targets(
            &self,
            _application_id: String,
        ) -> impl std::future::Future<
            Output = Result<ListApplicationTargetsResponse, ApplicationLifecycleError>,
        > + Send {
            ready(Ok(ListApplicationTargetsResponse::default()))
        }

        fn create_deployment(
            &self,
            _request: CreateApplicationDeploymentRequest,
        ) -> impl std::future::Future<
            Output = Result<ApplicationDeployment, ApplicationLifecycleError>,
        > + Send {
            ready(Ok(ApplicationDeployment::default()))
        }

        fn remove_deployment(
            &self,
            _deployment_id: String,
        ) -> impl std::future::Future<
            Output = Result<ApplicationDeployment, ApplicationLifecycleError>,
        > + Send {
            ready(Ok(ApplicationDeployment::default()))
        }

        fn update_metadata(
            &self,
            deployment_id: String,
            patch: dotcore_domain::deployment::metadata::DeploymentMetadataPatch,
        ) -> impl std::future::Future<
            Output = Result<ApplicationDeployment, ApplicationLifecycleError>,
        > + Send {
            let result = match self.outcome {
                MetadataOutcome::Accepted => Ok(ApplicationDeployment {
                    application_deployment_id: deployment_id,
                    application_deployment_name: patch.display_name().to_owned(),
                    ..Default::default()
                }),
                MetadataOutcome::InvalidName => Err(ApplicationLifecycleError::InvalidName),
                MetadataOutcome::NotFound => Err(ApplicationLifecycleError::NotFound),
                MetadataOutcome::Teardown => Err(ApplicationLifecycleError::FailedPrecondition),
                MetadataOutcome::PersistenceFailure => Err(
                    ApplicationLifecycleError::Configuration(anyhow!("injected")),
                ),
                MetadataOutcome::OutcomeUncertain => Err(
                    ApplicationLifecycleError::OutcomeUncertain(anyhow!("injected")),
                ),
            };
            ready(result)
        }

        fn pause_deployment(
            &self,
            _deployment_id: String,
        ) -> impl std::future::Future<
            Output = Result<ApplicationDeployment, ApplicationLifecycleError>,
        > + Send {
            ready(Ok(ApplicationDeployment {
                application_deployment_id: "paused".to_owned(),
                ..Default::default()
            }))
        }

        fn resume_deployment(
            &self,
            _deployment_id: String,
        ) -> impl std::future::Future<
            Output = Result<ApplicationDeployment, ApplicationLifecycleError>,
        > + Send {
            ready(Ok(ApplicationDeployment {
                application_deployment_id: "resumed".to_owned(),
                ..Default::default()
            }))
        }
    }

    impl ApplicationDeploymentLogController for TestController {
        /// Returns the newest entries of a synthetic 150-entry history, bounded by the query
        /// limit as the runtime bounds it.
        fn read_deployment_logs(
            &self,
            _: String,
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
                    name: "web".to_owned(),
                    label: "web".to_owned(),
                },
                timestamp: UNIX_EPOCH + Duration::from_secs(4),
                stream: dotcore_runtime::RuntimeLogStreamKind::Stderr,
                content: b"secret".to_vec(),
                truncated: false,
            };
            ready(Ok(vec![dotcore_runtime::RuntimeLogSourceHistory {
                source: dotcore_runtime::RuntimeLogSource {
                    name: "web".to_owned(),
                    label: "web".to_owned(),
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
            _: String,
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
            _: String,
        ) -> impl std::future::Future<
            Output = Result<
                crate::management_logs::ProtectedLogValues,
                crate::management_logs::DeploymentLogError,
            >,
        > + Send {
            ready(Ok(crate::management_logs::ProtectedLogValues::new([
                "secret".to_owned(),
            ])))
        }
    }

    /// Entry count of the synthetic deployment log history the test controller reads.
    const SYNTHETIC_LOG_ENTRIES: usize = 150;
}
