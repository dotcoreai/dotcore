//! Apps-controller startup tracking: whether a present deployment whose workload group does not
//! serve yet is still starting or has become unavailable.
//!
//! A start is one lifecycle generation of a deployment on this node, such as a create or a
//! resume. A deployment that boots in it reports `Starting` until it is first ready, or until it
//! has booted for longer than the startup grace; after either, a deployment that does not serve
//! reports `Unavailable`. Tracking lives in memory, so a node restart begins a fresh start.

use std::{
    collections::{BTreeMap, BTreeSet},
    sync::Mutex,
    time::Duration,
};

use dotcore_proto::dotcore::application::v1::{
    ApplicationDeploymentState, ApplicationLifecycleState,
};
use tokio::time::Instant;

use super::{booting_state, readiness::awaits_readiness};

/// Tracks the current start of each deployment a realizer reconciles.
pub(super) struct StartupTracker {
    grace: Duration,
    starts: Mutex<BTreeMap<String, Start>>,
}

impl StartupTracker {
    pub(super) fn new(grace: Duration) -> Self {
        Self {
            grace,
            starts: Mutex::new(BTreeMap::new()),
        }
    }

    /// Returns the state to record for one present deployment's reconciled `state`: a booting
    /// state reads `Starting` while the deployment has not been ready in this start and boots
    /// within the grace.
    pub(super) fn settle(
        &self,
        deployment_id: &str,
        lifecycle_generation: u64,
        state: ApplicationDeploymentState,
    ) -> ApplicationDeploymentState {
        self.settle_at(deployment_id, lifecycle_generation, state, Instant::now())
    }

    /// Forgets deployments the realizer no longer reconciles.
    pub(super) fn retain(&self, deployment_ids: &BTreeSet<&str>) {
        self.lock()
            .retain(|deployment_id, _| deployment_ids.contains(deployment_id.as_str()));
    }

    fn settle_at(
        &self,
        deployment_id: &str,
        lifecycle_generation: u64,
        state: ApplicationDeploymentState,
        now: Instant,
    ) -> ApplicationDeploymentState {
        let mut starts = self.lock();
        let start = starts
            .entry(deployment_id.to_owned())
            .or_insert_with(|| Start::new(lifecycle_generation));
        if start.lifecycle_generation != lifecycle_generation {
            *start = Start::new(lifecycle_generation);
        }

        if state.lifecycle_state == ApplicationLifecycleState::Ready as i32 {
            start.ready = true;
            return state;
        }
        if start.ready || !awaits_readiness(&state) {
            return state;
        }

        // The grace runs from the first booting observation, so image preparation before the
        // group exists does not consume it.
        let booting_since = *start.booting_since.get_or_insert(now);
        if now.duration_since(booting_since) >= self.grace {
            return state;
        }
        booting_state(state)
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, BTreeMap<String, Start>> {
        self.starts
            .lock()
            .unwrap_or_else(|value| value.into_inner())
    }
}

/// One deployment's current start.
struct Start {
    lifecycle_generation: u64,
    booting_since: Option<Instant>,
    ready: bool,
}

impl Start {
    fn new(lifecycle_generation: u64) -> Self {
        Self {
            lifecycle_generation,
            booting_since: None,
            ready: false,
        }
    }
}

#[cfg(test)]
mod tests {
    use dotcore_proto::dotcore::application::v1::{ApplicationEndpoint, ApplicationReadinessState};

    use super::*;
    use crate::application::controller::{
        deployment_failure_state,
        readiness::{ENDPOINT_NOT_READY_STAGE, GROUP_NOT_RUNNING_STAGE},
        unavailable_state,
    };

    const GRACE: Duration = Duration::from_secs(60);

    #[test]
    fn booting_deployment_starts_until_the_grace_elapses() {
        let tracker = StartupTracker::new(GRACE);
        let first = Instant::now();

        let starting = tracker.settle_at("app", 1, not_ready(), first);
        assert_eq!(
            starting.lifecycle_state,
            ApplicationLifecycleState::Starting as i32
        );
        assert_eq!(
            starting.readiness_state,
            ApplicationReadinessState::NotReady as i32
        );
        assert_eq!(starting.stage, ENDPOINT_NOT_READY_STAGE);
        assert_eq!(starting.endpoint, None);

        let within = tracker.settle_at(
            "app",
            1,
            not_ready(),
            first + GRACE - Duration::from_millis(1),
        );
        assert_eq!(
            within.lifecycle_state,
            ApplicationLifecycleState::Starting as i32
        );

        let not_ready = not_ready();
        let expired = tracker.settle_at("app", 1, not_ready.clone(), first + GRACE);
        assert_eq!(expired, not_ready);
    }

    #[test]
    fn deployment_that_was_ready_in_this_start_is_unavailable_when_it_stops_serving() {
        let tracker = StartupTracker::new(GRACE);
        let now = Instant::now();

        let (ready, not_ready) = (ready(), not_ready());
        assert_eq!(tracker.settle_at("app", 1, ready.clone(), now), ready);
        assert_eq!(
            tracker.settle_at("app", 1, not_ready.clone(), now),
            not_ready
        );
    }

    #[test]
    fn new_lifecycle_generation_starts_again_with_a_fresh_grace() {
        let tracker = StartupTracker::new(GRACE);
        let first = Instant::now();
        tracker.settle_at("app", 1, not_ready(), first);
        tracker.settle_at("app", 1, ready(), first);

        let resumed = tracker.settle_at("app", 2, not_ready(), first + GRACE * 10);
        assert_eq!(
            resumed.lifecycle_state,
            ApplicationLifecycleState::Starting as i32
        );
    }

    #[test]
    fn transient_unavailable_state_during_boot_does_not_stick() {
        let tracker = StartupTracker::new(GRACE);
        let now = Instant::now();
        tracker.settle_at("app", 1, not_ready(), now);

        let inconclusive =
            unavailable_state("Runtime returned an inconclusive application group", None);
        assert_eq!(
            tracker.settle_at("app", 1, inconclusive.clone(), now),
            inconclusive
        );

        let booting = tracker.settle_at(
            "app",
            1,
            unavailable_state(GROUP_NOT_RUNNING_STAGE, None),
            now + Duration::from_secs(1),
        );
        assert_eq!(
            booting.lifecycle_state,
            ApplicationLifecycleState::Starting as i32
        );
        assert_eq!(booting.stage, GROUP_NOT_RUNNING_STAGE);
    }

    #[test]
    fn failed_or_unrelated_states_pass_through() {
        let tracker = StartupTracker::new(GRACE);
        let now = Instant::now();
        let failed = deployment_failure_state("readiness-operation", None);

        assert_eq!(tracker.settle_at("app", 1, failed.clone(), now), failed);
    }

    #[test]
    fn retain_forgets_deployments_no_longer_reconciled() {
        let tracker = StartupTracker::new(GRACE);
        let now = Instant::now();
        tracker.settle_at("kept", 1, ready(), now);
        tracker.settle_at("gone", 1, ready(), now);

        tracker.retain(&BTreeSet::from(["kept"]));

        assert_eq!(tracker.lock().keys().cloned().collect::<Vec<_>>(), ["kept"]);
    }

    fn not_ready() -> ApplicationDeploymentState {
        unavailable_state(ENDPOINT_NOT_READY_STAGE, Some(endpoint()))
    }

    fn ready() -> ApplicationDeploymentState {
        let mut state = unavailable_state("ready", Some(endpoint()));
        state.lifecycle_state = ApplicationLifecycleState::Ready as i32;
        state.readiness_state = ApplicationReadinessState::Ready as i32;
        state
    }

    fn endpoint() -> ApplicationEndpoint {
        ApplicationEndpoint {
            url: "http://127.0.0.1:41000".to_owned(),
        }
    }
}
