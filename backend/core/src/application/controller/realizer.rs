//! Node-local application realizer that drives runtime workload groups for the deployments
//! assigned to one SiteNode.
//!
//! It runs one serialized observer that reconciles on intent changes, which include linked-route
//! renames projected into its assignments, or on a fixed cadence, and sooner while a deployment
//! boots and its group does not serve yet. It reads intent without
//! mutation authority and never writes or deletes declarations. It also keeps node records:
//! ownership evidence, once-resolved launch identities, and the view each deployment last
//! reconciled, and tracks whether each booting deployment is still starting.

use std::{
    net::Ipv4Addr,
    sync::{Arc, Mutex},
    time::Duration,
};

use dotcore_domain::deployment::launch_pin::LaunchPinResolver;
use tokio::sync::{Mutex as AsyncMutex, Notify};
use tracing::{debug, info, warn};

use super::{
    ApplicationLifecycleError, ApplicationRealizationInput, ApplicationRealizationResults,
    InferenceRouteResolver, NodeRealizationRecords, RECONCILE_CADENCE, ReconciledAssignments,
    STARTUP_GRACE, startup::StartupTracker,
};
use crate::{
    application::repository::CurrentApplicationRepository,
    deployment_logs::{PreparationLogs, WorkloadLifecycle},
    dispatch::application::ApplicationRuntimeMap,
};

/// Reconciles assigned application declarations against its node's runtime target map.
///
/// The realizer reads intent only through its realization input, writes only the current records
/// of assigned deployments, and reports completed teardown through its results port. It holds no
/// declaration repository, so it cannot write or delete declarations.
pub(crate) struct ApplicationRealizer {
    pub(super) input: Arc<dyn ApplicationRealizationInput>,
    pub(super) current: Arc<dyn CurrentApplicationRepository>,
    pub(super) results: Arc<dyn ApplicationRealizationResults>,
    pub(super) runtimes: ApplicationRuntimeMap,
    pub(super) publication_host: Ipv4Addr,
    pub(super) launch_pins: Option<Arc<dyn LaunchPinResolver>>,
    pub(super) artifact_manager: Option<dotcore_artifact::ArtifactManager>,
    pub(super) preparation_logs: PreparationLogs,
    pub(super) lifecycle: WorkloadLifecycle,
    pub(super) inference_route_resolver: Arc<dyn InferenceRouteResolver>,
    pub(super) coordination: RealizerCoordination,
    pub(super) node: NodeRealization,
    pub(super) accelerator_attribution:
        Option<super::accelerator_memory::GroupAcceleratorAttribution>,
    pub(super) startup: StartupTracker,
    pub(super) active_reconciliation: Mutex<Option<tokio_util::sync::CancellationToken>>,
    pub(super) shutdown: tokio_util::sync::CancellationToken,
    observer_started: Mutex<bool>,
}

/// Node realization state: durable node records and the view each deployment last reconciled.
/// Its realization records also guard teardown storage removal, so a node deletes only storage it
/// realized.
pub(super) struct NodeRealization {
    pub(super) records: Arc<dyn NodeRealizationRecords>,
    pub(super) reconciled: ReconciledAssignments,
}

/// Runtime-facing dependencies of one realizer, separate from its owner ports.
pub(super) struct RealizerDependencies {
    pub(super) runtimes: ApplicationRuntimeMap,
    pub(super) publication_host: Ipv4Addr,

    /// Resolver pinning development and locked service sources to launch identities.
    pub(super) launch_pins: Option<Arc<dyn LaunchPinResolver>>,

    /// Image preparation, present only on a node with a Docker application target.
    pub(super) artifact_manager: Option<dotcore_artifact::ArtifactManager>,

    /// Preparation logs each deployment's artifact preparation writes its steps to.
    pub(super) preparation_logs: PreparationLogs,

    /// Workload changes the open log follows of each deployment are told about.
    pub(super) lifecycle: WorkloadLifecycle,
    pub(super) inference_route_resolver: Arc<dyn InferenceRouteResolver>,
    pub(super) node: NodeRealization,
    pub(super) accelerator_attribution:
        Option<super::accelerator_memory::GroupAcceleratorAttribution>,
}

/// Serialization of one realizer's reconciliation passes and the wakeup that schedules the next.
pub(super) struct RealizerCoordination {
    pub(super) operation: AsyncMutex<()>,
    pub(super) wakeup: Arc<Notify>,
}

impl ApplicationRealizer {
    pub(super) fn new(
        input: Arc<dyn ApplicationRealizationInput>,
        current: Arc<dyn CurrentApplicationRepository>,
        results: Arc<dyn ApplicationRealizationResults>,
        dependencies: RealizerDependencies,
    ) -> Self {
        let wakeup = input.register_change_listener();
        Self {
            input,
            current,
            results,
            runtimes: dependencies.runtimes,
            publication_host: dependencies.publication_host,
            launch_pins: dependencies.launch_pins,
            artifact_manager: dependencies.artifact_manager,
            preparation_logs: dependencies.preparation_logs,
            lifecycle: dependencies.lifecycle,
            inference_route_resolver: dependencies.inference_route_resolver,
            coordination: RealizerCoordination {
                operation: AsyncMutex::new(()),
                wakeup,
            },
            node: dependencies.node,
            accelerator_attribution: dependencies.accelerator_attribution,
            startup: StartupTracker::new(STARTUP_GRACE),
            active_reconciliation: Mutex::new(None),
            shutdown: tokio_util::sync::CancellationToken::new(),
            observer_started: Mutex::new(false),
        }
    }

    /// Starts one serialized observer that reconciles on intent or cadence wakeups.
    pub(super) fn start_observer(realizer: &Arc<Self>) {
        if !realizer.claim_observer_start() {
            debug!(
                event_kind = "application_observer_noop",
                reason = "already_started",
                "Application observer start was already claimed"
            );
            return;
        }
        info!(
            event_kind = "application_observer_started",
            "Application observer started"
        );
        tokio::spawn(observer_loop(ObserverDependencies {
            realizer: Arc::downgrade(realizer),
            desired_wakeup: Arc::clone(&realizer.coordination.wakeup),
        }));
    }

    /// Returns the token cancelled when the composed realizer is dropped.
    pub(super) fn shutdown_token(&self) -> tokio_util::sync::CancellationToken {
        self.shutdown.clone()
    }

    fn claim_observer_start(&self) -> bool {
        let mut started = self
            .observer_started
            .lock()
            .unwrap_or_else(|value| value.into_inner());
        if *started {
            return false;
        }
        *started = true;
        true
    }
}

struct ObserverDependencies {
    realizer: std::sync::Weak<ApplicationRealizer>,
    desired_wakeup: Arc<Notify>,
}

async fn observer_loop(observer: ObserverDependencies) {
    let mut cadence = ReconcileCadence::new();
    loop {
        let Some(realizer) = observer.realizer.upgrade() else {
            return;
        };
        let result = realizer.reconcile().await;
        let delay = cadence.next_delay(record_observer_reconciliation(result));
        drop(realizer);
        if wait_for_observer_wakeup(&observer.desired_wakeup, delay).await {
            cadence.reset();
        }
    }
}

/// Whether a reconciliation pass left a deployment waiting for its primary endpoint.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum ReconciliationState {
    Pending,

    Settled,
}

/// First delay before probing a pending deployment again; it doubles up to the maximum.
const READINESS_RETRY_INITIAL_DELAY: Duration = Duration::from_millis(250);
const READINESS_RETRY_MAX_DELAY: Duration = Duration::from_secs(2);

/// Delay before the observer's next pass: short and growing while a deployment is pending, the
/// fixed cadence once every deployment is settled.
#[derive(Debug)]
pub(super) struct ReconcileCadence {
    pending_delay: Duration,
}

impl ReconcileCadence {
    pub(super) fn new() -> Self {
        Self {
            pending_delay: READINESS_RETRY_INITIAL_DELAY,
        }
    }

    pub(super) fn next_delay(&mut self, state: ReconciliationState) -> Duration {
        match state {
            ReconciliationState::Pending => {
                let delay = self.pending_delay;
                self.pending_delay = self
                    .pending_delay
                    .saturating_mul(2)
                    .min(READINESS_RETRY_MAX_DELAY);
                delay
            }
            ReconciliationState::Settled => {
                self.reset();
                RECONCILE_CADENCE
            }
        }
    }

    pub(super) fn reset(&mut self) {
        self.pending_delay = READINESS_RETRY_INITIAL_DELAY;
    }
}

/// Logs one observer pass outcome and returns its state; a failed pass is retried at the settled
/// cadence.
fn record_observer_reconciliation(
    result: Result<ReconciliationState, ApplicationLifecycleError>,
) -> ReconciliationState {
    match result {
        Err(source) => {
            warn!(event_kind = "application_observer_reconciliation_failed", error = ?source, "Application observer reconciliation failed");
            ReconciliationState::Settled
        }
        Ok(state) => {
            debug!(
                event_kind = "application_observer_reconciliation_completed",
                state = ?state,
                "Application observer reconciliation completed"
            );
            state
        }
    }
}

/// Waits for the next intent change or `delay`, and returns whether intent changed.
pub(super) async fn wait_for_observer_wakeup(desired_wakeup: &Notify, delay: Duration) -> bool {
    let (reason, desired_change) = tokio::select! {
        _ = desired_wakeup.notified() => ("desired_change", true),
        _ = tokio::time::sleep(delay) => ("cadence", false),
    };
    debug!(
        event_kind = "application_observer_wakeup",
        reason, "Application observer scheduled reconciliation"
    );
    desired_change
}
