//! Infrastructure-neutral runtime contracts (crate `dotcore-runtime`): the workload and
//! workload-group intent a controller hands to a runtime backend, the observations and diagnostics
//! the backend reports back, the stopped-site cleanup the backends perform, and the host capability
//! probes a node runs.
//!
//! The contracts name no owner, engine, controller or backend, so the site and the controllers can
//! depend on them without reaching any backend. The Docker, native and simulated backends implement
//! them in the layer above.

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
compile_error!("Dotcore supports only Linux and macOS targets");

mod capability_probes;
mod errors;
mod individual_workload;
mod site_reset;
mod workload_group;
mod workload_spec;

#[cfg(any(test, feature = "test-support"))]
pub mod test_support;

pub use capability_probes::{
    DockerCapabilityProbe, DockerCapabilityReport, NativeCapabilityFailure, NativeCapabilityProbe,
    NativeCapabilityReport, RuntimeCapabilityProbes,
};
pub use errors::{RuntimeBackendError, RuntimeFuture};
pub use individual_workload::{
    ExecutionTarget, LoopbackEndpoint, MAX_RUNTIME_LOG_ENTRIES, MAX_RUNTIME_LOG_SOURCES,
    ObservedWorkload, ProcessMemory, ProcessMemoryObservation, ProcessMemoryUnavailable,
    RuntimeBackend, RuntimeBackendId, RuntimeLogEntry, RuntimeLogEvent, RuntimeLogFollowQuery,
    RuntimeLogFuture, RuntimeLogHistory, RuntimeLogQuery, RuntimeLogQueryError, RuntimeLogSource,
    RuntimeLogSourceSelection, RuntimeLogStream, RuntimeLogStreamKind,
    RuntimeWorkloadLogFollowQuery, RuntimeWorkloadLogQuery, WorkloadAssetMaterialization,
    WorkloadCorrelation, WorkloadId, WorkloadIpcMode, WorkloadLaunch,
    WorkloadMaterializationRequirements, WorkloadObservation, WorkloadRealizationProvenance,
    WorkloadServiceAccess, WorkloadServiceBinding, WorkloadServiceReference,
    WorkloadStartupActivity, WorkloadStartupObserver, WorkloadStartupPhase,
    WorkloadStartupProgress, WorkloadStartupProgressUnit, WorkloadStartupRequest,
    WorkloadStorageAttachment, WorkloadStorageKey, WorkloadStorageKind,
};
pub use site_reset::{
    LocatedWorkloadCleanup, RuntimeBindingKind, SiteRuntimeCleanup, SiteRuntimeCleanupReport,
    SiteRuntimeCleanups, SiteRuntimeInventory, SiteRuntimeResetConfigurationError,
    SiteRuntimeResource, SiteRuntimeScope, SkippedSiteRuntimeCleanup,
};
pub use workload_group::{
    AssignedWorkloadEndpoint, MAX_READY_LAUNCH_GATE_PATH_BYTES, MAX_READY_LAUNCH_GATE_TIMEOUT,
    MAX_WORKLOAD_GROUP_STALE_REASON_BYTES, ObservedWorkloadGroup, ReadyLaunchGate,
    ReadyLaunchGateError, ReadyLaunchGateFailure, ReadyLaunchGateOutcome,
    RuntimeGroupLogFollowQuery, RuntimeLogSourceHistory, RuntimeLogSourceStream,
    WorkloadDependencyCondition, WorkloadGroupBackend, WorkloadGroupId, WorkloadGroupLaunch,
    WorkloadGroupObservation, WorkloadGroupStaleReason, WorkloadPublicationRequest,
    WorkloadUnitDependency, WorkloadUnitLaunch, WorkloadUnitName, WorkloadUnitObservation,
    WorkloadUnitState,
};
pub use workload_spec::{
    ResolvedWorkloadCommand, WorkloadArtifactBinding, WorkloadExecutable,
    WorkloadExecutableArtifact, WorkloadExpectedMemory, WorkloadPort, WorkloadSpec,
    WorkloadSpecError, WorkloadToken, WorkloadTokenResolver, WorkloadValue, WorkloadValueSegment,
};

/// Longest workload id, in bytes, that every runtime backend accepts.
pub const MAX_WORKLOAD_ID_BYTES: usize = 156;

#[cfg(test)]
mod tests {
    use crate::test_support::assert_owner_neutral;

    #[test]
    fn runtime_contract_sources_have_no_engine_or_owner_vocabulary() {
        for (name, source) in [
            (
                "individual_workload.rs",
                include_str!("individual_workload.rs"),
            ),
            ("workload_group.rs", include_str!("workload_group.rs")),
            ("workload_spec.rs", include_str!("workload_spec.rs")),
            ("errors.rs", include_str!("errors.rs")),
            ("site_reset.rs", include_str!("site_reset.rs")),
            ("capability_probes.rs", include_str!("capability_probes.rs")),
        ] {
            assert_owner_neutral(name, source);
        }
    }
}
