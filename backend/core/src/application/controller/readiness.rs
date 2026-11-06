//! Apps-controller endpoint readiness evaluation and state derivation.
//!
//! A deployment is ready once its primary endpoint answers its declared HTTP check with a
//! success status, or, on a runtime that runs nothing that serves, once its group runs with an
//! assigned endpoint. An endpoint that refuses, resets, or closes the connection before answering
//! is not ready yet, as while the application starts, so the realizer probes it again soon;
//! a malformed endpoint or a probe that times out still surfaces as an error.

use std::{
    io::ErrorKind,
    net::{Ipv4Addr, SocketAddr},
};

use dotcore_proto::dotcore::{
    application::v1::{
        ApplicationDeploymentDeclaration, ApplicationDeploymentState, ApplicationEndpoint,
        ApplicationLifecycleState,
    },
    catalog::v1::application_service_readiness,
};
use dotcore_runtime::{
    ObservedWorkloadGroup, WorkloadGroupLaunch, WorkloadGroupObservation, WorkloadUnitState,
};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tracing::debug;

use super::{
    ApplicationLifecycleError, READINESS_TIMEOUT, configuration, configuration_with_source,
    queued_state, ready_state, unavailable_state,
};

/// Stage recorded while some of a deployment's workload units are not running yet.
pub(super) const GROUP_NOT_RUNNING_STAGE: &str = "Application group is not running";

/// Stage recorded while a running deployment has no primary endpoint on the configured host yet.
pub(super) const ENDPOINT_NOT_ASSIGNED_STAGE: &str =
    "Application primary endpoint is not assigned on the configured host";

/// Stage recorded while a running deployment's primary endpoint does not answer its check yet.
pub(super) const ENDPOINT_NOT_READY_STAGE: &str = "Application primary endpoint is not ready";

/// Stages of a deployment whose workload group exists but does not serve yet: its units start,
/// its endpoint is published, then its endpoint answers.
const BOOT_STAGES: [&str; 3] = [
    GROUP_NOT_RUNNING_STAGE,
    ENDPOINT_NOT_ASSIGNED_STAGE,
    ENDPOINT_NOT_READY_STAGE,
];

/// How readiness checks a deployment's primary endpoint.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum EndpointProbe {
    /// Sends the declared HTTP check to the endpoint.
    Probe,

    /// Takes the assigned endpoint as ready, for a runtime that runs nothing that could answer.
    Skip,
}

pub(super) async fn state_from_observation(
    desired: &ApplicationDeploymentDeclaration,
    launch: &WorkloadGroupLaunch,
    observation: &WorkloadGroupObservation,
    publication_host: Ipv4Addr,
    probe: EndpointProbe,
) -> Result<ApplicationDeploymentState, ApplicationLifecycleError> {
    match observation {
        WorkloadGroupObservation::Observed(group) => {
            state_from_group(desired, launch, group, publication_host, probe).await
        }
        WorkloadGroupObservation::Absent { .. } => Ok(queued_state()),
        WorkloadGroupObservation::StaleConfiguration { .. }
        | WorkloadGroupObservation::Unknown { .. } => Ok(unavailable_state(
            "Runtime returned an inconclusive application group",
            None,
        )),
    }
}

pub(super) async fn state_from_group(
    desired: &ApplicationDeploymentDeclaration,
    launch: &WorkloadGroupLaunch,
    group: &ObservedWorkloadGroup,
    publication_host: Ipv4Addr,
    probe: EndpointProbe,
) -> Result<ApplicationDeploymentState, ApplicationLifecycleError> {
    let endpoint = group.assigned_endpoint.as_ref().filter(|endpoint| {
        endpoint.configured_host == publication_host
            && endpoint.unit_name == launch.publication.unit_name
            && endpoint.port_name == launch.publication.port_name
    });
    let endpoint = endpoint.map(|endpoint| ApplicationEndpoint {
        url: format!(
            "http://{}:{}",
            endpoint.configured_host, endpoint.assigned_port
        ),
    });
    if group
        .units
        .values()
        .any(|unit| unit.state != WorkloadUnitState::Running)
    {
        return Ok(unavailable_state(GROUP_NOT_RUNNING_STAGE, endpoint));
    }
    let Some(endpoint) = endpoint else {
        return Ok(unavailable_state(ENDPOINT_NOT_ASSIGNED_STAGE, None));
    };
    let ready = match probe {
        EndpointProbe::Probe => primary_endpoint_ready(desired, &endpoint.url).await?,
        EndpointProbe::Skip => true,
    };
    Ok(if ready {
        ready_state(group, endpoint)
    } else {
        unavailable_state(ENDPOINT_NOT_READY_STAGE, Some(endpoint))
    })
}

/// Whether a reconciled state waits for its workload group to boot, so the realizer probes it
/// again soon instead of at its settled cadence. A recorded failure, such as a failed creation,
/// keeps the settled cadence.
pub(super) fn awaits_readiness(state: &ApplicationDeploymentState) -> bool {
    matches!(
        ApplicationLifecycleState::try_from(state.lifecycle_state),
        Ok(ApplicationLifecycleState::Starting | ApplicationLifecycleState::Unavailable)
    ) && state.failure.is_none()
        && BOOT_STAGES.contains(&state.stage.as_str())
}

pub(super) async fn primary_endpoint_ready(
    desired: &ApplicationDeploymentDeclaration,
    url: &str,
) -> Result<bool, ApplicationLifecycleError> {
    let snapshot = desired
        .snapshot
        .as_ref()
        .ok_or_else(|| configuration("Application snapshot is missing"))?;
    let graph = snapshot
        .graph
        .as_ref()
        .ok_or_else(|| configuration("Application graph is missing"))?;
    let primary = graph
        .primary_endpoint
        .as_ref()
        .ok_or_else(|| configuration("Application primary endpoint is missing"))?;
    let service = graph
        .services
        .iter()
        .find(|service| service.service_name == primary.service_name)
        .ok_or_else(|| configuration("Application primary service is missing"))?;
    let Some(application_service_readiness::Check::HttpGet(readiness)) = service
        .readiness
        .as_ref()
        .and_then(|readiness| readiness.check.as_ref())
    else {
        return Ok(false);
    };
    if readiness.port_name != primary.port_name {
        return Ok(false);
    }
    let address = url
        .strip_prefix("http://")
        .ok_or_else(|| configuration("Application endpoint has invalid protocol"))?
        .parse::<SocketAddr>()
        .map_err(|source| configuration_with_source("Application endpoint is invalid", source))?;
    #[expect(
        clippy::disallowed_methods,
        reason = "bounds one real HTTP readiness probe"
    )]
    let result = tokio::time::timeout(READINESS_TIMEOUT, async {
        let mut stream = tokio::net::TcpStream::connect(address).await?;
        stream
            .write_all(
                format!(
                    "GET {} HTTP/1.1\r\nHost: workload\r\nConnection: close\r\n\r\n",
                    readiness.path
                )
                .as_bytes(),
            )
            .await?;
        let mut response = [0_u8; 64];
        let bytes = stream.read(&mut response).await?;
        Ok::<_, std::io::Error>(
            response[..bytes].starts_with(b"HTTP/1.1 2")
                || response[..bytes].starts_with(b"HTTP/1.0 2"),
        )
    })
    .await;
    match result {
        Ok(Ok(ready)) => Ok(ready),
        Ok(Err(source)) if endpoint_not_listening(&source) => {
            debug!(event_kind = "application_readiness_not_listening", endpoint = url, error = %source, "Application primary endpoint is not listening yet");
            Ok(false)
        }
        Ok(Err(source)) => Err(configuration_with_source(
            "Application readiness transport failed",
            source,
        )),
        Err(source) => Err(configuration_with_source(
            "Application readiness operation timed out",
            source,
        )),
    }
}

/// Whether a probe failure means nothing serves the endpoint yet: the connection is refused,
/// or accepted and dropped before a response, as Docker Desktop's port forwarder does until the
/// container listens.
fn endpoint_not_listening(error: &std::io::Error) -> bool {
    matches!(
        error.kind(),
        ErrorKind::ConnectionRefused
            | ErrorKind::ConnectionReset
            | ErrorKind::ConnectionAborted
            | ErrorKind::BrokenPipe
            | ErrorKind::UnexpectedEof
    )
}
