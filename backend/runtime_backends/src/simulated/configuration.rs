//! Configuration and test support for simulated runtime lifecycle behavior.

use std::{
    collections::{BTreeMap, BTreeSet},
    num::NonZeroU16,
    sync::{Arc, Mutex},
    time::Duration,
};

use dotcore_domain::{
    ExecutionPlatform, SiteNodeId,
    clock::{Clock, SystemClock},
};
use tokio::sync::watch;
use tokio_util::sync::CancellationToken;

/// Typed runtime lifecycle behavior for one exact execution target.
#[derive(Clone, Debug)]
pub struct SimulatedRuntimeConfig {
    /// Platform the simulated workloads report as their execution platform.
    pub execution_platform: ExecutionPlatform,

    /// SiteNode of the execution target, naming the node of every simulated workload process.
    pub managed_node_id: Option<SiteNodeId>,

    /// Machine the node declares, on which every model workload takes its expected memory and
    /// every running application unit its synthetic memory, as simulated processes. Composition
    /// sets it on every target of a node that declares a machine, and model targets require it;
    /// without it, application units report no resources.
    pub machine: Option<super::DeclaredMachine>,

    /// Ready delay used unless test support or the deployment sets one.
    pub default_ready_delay: Duration,

    /// Stop delay used unless test support or the deployment sets one.
    pub default_stop_delay: Duration,

    /// Ready delays keyed by deployment id.
    pub deployment_ready_delays: BTreeMap<String, Duration>,

    /// Stop delays keyed by deployment id.
    pub deployment_stop_delays: BTreeMap<String, Duration>,

    /// Time an application workload group takes to be created, started or stopped, as a container
    /// runtime takes to run or stop its containers.
    pub group_transition_delay: Duration,

    /// Whether every delay and startup step takes a random share of its base duration instead of
    /// exactly the base.
    pub vary_durations: bool,

    /// Shared call evidence and failure injection, when tests or development observe the backend.
    pub test_support: Option<SimulatedRuntimeTestSupport>,

    /// Startup progress reported during a first activation, when simulated.
    pub startup: Option<SimulatedStartupConfig>,

    /// Backend clock the workloads time their transitions and startup steps on.
    pub clock: Arc<dyn Clock>,
}

impl Default for SimulatedRuntimeConfig {
    fn default() -> Self {
        Self {
            execution_platform: ExecutionPlatform::linux_amd64(),
            managed_node_id: None,
            machine: None,
            default_ready_delay: Duration::ZERO,
            default_stop_delay: Duration::ZERO,
            deployment_ready_delays: BTreeMap::new(),
            deployment_stop_delays: BTreeMap::new(),
            group_transition_delay: Duration::ZERO,
            vary_durations: false,
            test_support: None,
            startup: None,
            clock: Arc::new(SystemClock),
        }
    }
}

impl SimulatedRuntimeConfig {
    /// One occurrence of a `base` delay: the base itself, or a random duration between half and
    /// twice the base when durations vary.
    pub(super) fn transition_duration(&self, base: Duration) -> Duration {
        if !self.vary_durations || base.is_zero() {
            return base;
        }

        // Variation is a development aid: without a random source, the base delay still holds.
        let Ok(random) = getrandom::u32() else {
            return base;
        };
        base.mul_f64(0.5 + 1.5 * f64::from(random) / f64::from(u32::MAX))
    }
}

/// Runtime-owned representative startup sequence for first activation.
#[derive(Clone, Debug)]
pub struct SimulatedStartupConfig {
    /// Total progress units, such as bytes loaded.
    pub total: u64,

    /// Completed units reported at each step, in order.
    pub completed_steps: Arc<[u64]>,

    /// Time between two reported steps.
    pub step_duration: Duration,
}

impl SimulatedStartupConfig {
    /// An 8 GiB load reported in five steps, as the development site shows it.
    pub fn development_default(step_duration: Duration) -> Self {
        const GIBIBYTE: u64 = 1024 * 1024 * 1024;
        Self {
            total: 8 * GIBIBYTE,
            completed_steps: Arc::from([0, 2 * GIBIBYTE, 5 * GIBIBYTE, 7 * GIBIBYTE, 8 * GIBIBYTE]),
            step_duration,
        }
    }

    /// A startup sequence with explicit totals and steps for tests.
    #[cfg(any(test, feature = "test-support"))]
    pub fn for_test(
        total: u64,
        completed_steps: impl Into<Arc<[u64]>>,
        step_duration: Duration,
    ) -> Self {
        Self {
            total,
            completed_steps: completed_steps.into(),
            step_duration,
        }
    }
}
/// Bounded lifecycle-call evidence and failure injection for simulated runtime tests.
#[derive(Clone, Debug)]
pub struct SimulatedRuntimeTestSupport {
    pub(super) state: Arc<Mutex<SimulatedRuntimeTestState>>,
    representative_startup: bool,
    lifecycle_delays: Option<(Duration, Duration)>,
    preparation_gate: Option<Arc<RetainedGate>>,
    readiness_gate: Option<Arc<RetainedGate>>,
    stop_gate: Option<Arc<RetainedGate>>,
}

/// Deterministic group drift injected by composed runtime tests.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SimulatedRuntimeGroupDrift {
    Absent,
    Partial,
    Stale,
}

/// Exact group lifecycle call evidence from the simulated runtime.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SimulatedRuntimeGroupCall {
    pub operation: String,
    pub group_id: String,
}

impl SimulatedRuntimeGroupCall {
    pub fn create(group_id: &str) -> Self {
        Self::new("create", group_id)
    }

    pub fn start(group_id: &str) -> Self {
        Self::new("start", group_id)
    }

    pub fn stop(group_id: &str) -> Self {
        Self::new("stop", group_id)
    }

    pub fn remove(group_id: &str) -> Self {
        Self::new("remove", group_id)
    }

    pub fn inspect(group_id: &str) -> Self {
        Self::new("inspect", group_id)
    }

    pub fn ensure_absent(group_id: &str) -> Self {
        Self::new("ensure-absent", group_id)
    }

    pub fn remove_storage(key: &str) -> Self {
        Self::new("remove-storage", key)
    }

    pub(super) fn new(operation: &str, group_id: &str) -> Self {
        Self {
            operation: operation.to_owned(),
            group_id: group_id.to_owned(),
        }
    }
}

impl PartialEq for SimulatedRuntimeTestSupport {
    fn eq(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.state, &other.state)
    }
}

impl Eq for SimulatedRuntimeTestSupport {}

impl SimulatedRuntimeTestSupport {
    /// Fails this many valid stop calls, then allows all later stop calls to succeed.
    pub fn with_stop_failures(stop_failures: usize) -> Self {
        Self::new(SimulatedRuntimeTestState {
            remaining_stop_failures: stop_failures,
            call_counts: SimulatedRuntimeCallCounts::default(),
            ..Default::default()
        })
    }

    /// Fails this many runtime discovery calls, then allows later calls to succeed.
    pub fn with_discovery_failures(discovery_failures: usize) -> Self {
        Self::new(SimulatedRuntimeTestState {
            remaining_discovery_failures: discovery_failures,
            ..Default::default()
        })
    }

    /// Supplies one deterministic endpoint for application group readiness checks.
    pub fn with_application_endpoint(port: u16) -> Self {
        Self::new(SimulatedRuntimeTestState {
            application_endpoint: NonZeroU16::new(port),
            ..Default::default()
        })
    }

    /// Assigns `port` as the endpoint of every application group while keeping the controls
    /// already configured, for example on the development test support of an external process.
    pub fn with_application_endpoint_port(self, port: u16) -> Self {
        self.state
            .lock()
            .expect("simulated runtime test support")
            .application_endpoint = NonZeroU16::new(port);
        self
    }

    /// Keeps each execution target's workloads across every runtime backend built with this
    /// support, so a backend restarted in the same process finds the workloads it left running.
    pub fn with_retained_workloads(self) -> Self {
        self.state
            .lock()
            .expect("simulated runtime test support")
            .retained_workloads = Some(BTreeMap::new());
        self
    }

    /// Returns the retained workloads of one execution target, when retention is enabled.
    pub(super) fn retained_workloads(
        &self,
        execution_target_id: &str,
    ) -> Option<Arc<Mutex<super::adapter::FakeState>>> {
        self.state
            .lock()
            .expect("simulated runtime test support")
            .retained_workloads
            .as_mut()
            .map(|retained| Arc::clone(retained.entry(execution_target_id.to_owned()).or_default()))
    }

    /// Enables the representative model startup sequence, whose steps follow the simulated
    /// timing.
    pub fn with_representative_startup(mut self) -> Self {
        self.representative_startup = true;
        self
    }

    /// Overrides individual-workload ready and stop delays for this test support instance.
    pub fn with_lifecycle_delays(mut self, ready: Duration, stop: Duration) -> Self {
        self.lifecycle_delays = Some((ready, stop));
        self
    }

    /// Enables retained gates after preparation activity and before runtime readiness.
    pub fn with_model_transition_gates(mut self) -> Self {
        self.preparation_gate = Some(Arc::new(RetainedGate::default()));
        self.readiness_gate = Some(Arc::new(RetainedGate::default()));
        self
    }

    /// Waits until model preparation has published startup activity and reached its gate.
    pub async fn wait_for_preparation_gate(&self) {
        self.preparation_gate
            .as_ref()
            .expect("model preparation gate is not enabled")
            .wait_until_reached()
            .await;
    }

    /// Releases model preparation after the test has observed download activity.
    pub fn release_preparation_gate(&self) {
        self.preparation_gate
            .as_ref()
            .expect("model preparation gate is not enabled")
            .release();
    }

    /// Waits until the simulated workload would otherwise become ready.
    pub async fn wait_for_readiness_gate(&self) {
        self.readiness_gate
            .as_ref()
            .expect("model readiness gate is not enabled")
            .wait_until_reached()
            .await;
    }

    /// Releases simulated workload readiness after the test has observed loading.
    pub fn release_readiness_gate(&self) {
        self.readiness_gate
            .as_ref()
            .expect("model readiness gate is not enabled")
            .release();
    }

    /// Enables a retained gate that holds stopping individual workloads before they stop.
    pub fn with_stop_gate(mut self) -> Self {
        self.stop_gate = Some(Arc::new(RetainedGate::default()));
        self
    }

    /// Waits until a stopping simulated workload would otherwise have stopped.
    pub async fn wait_for_stop_gate(&self) {
        self.stop_gate
            .as_ref()
            .expect("workload stop gate is not enabled")
            .wait_until_reached()
            .await;
    }

    /// Releases stopping simulated workloads after the test has observed them stopping.
    pub fn release_stop_gate(&self) {
        self.stop_gate
            .as_ref()
            .expect("workload stop gate is not enabled")
            .release();
    }

    /// Whether workloads of this test support report the representative startup sequence.
    pub fn representative_startup(&self) -> bool {
        self.representative_startup
    }

    /// Returns the ready and stop delays this test support imposes, if any.
    pub fn lifecycle_delays(&self) -> Option<(Duration, Duration)> {
        self.lifecycle_delays
    }

    /// Fails this many valid application group stop calls, then allows later calls.
    pub fn with_group_stop_failures(self, failures: usize) -> Self {
        self.state
            .lock()
            .expect("simulated runtime test support")
            .remaining_group_stop_failures = failures;
        self
    }

    /// Fails this many valid application group start calls, then allows later calls.
    pub fn with_group_start_failures(self, failures: usize) -> Self {
        self.state
            .lock()
            .expect("simulated runtime test support")
            .remaining_group_start_failures = failures;
        self
    }

    /// Overrides one group observation until the override is cleared.
    pub fn set_group_drift(&self, group_id: &str, drift: Option<SimulatedRuntimeGroupDrift>) {
        let mut state = self.state.lock().expect("simulated runtime test support");
        if let Some(drift) = drift {
            state.group_drifts.insert(group_id.to_owned(), drift);
        } else {
            state.group_drifts.remove(group_id);
        }
    }

    /// Returns the number of runtime workload-discovery calls.
    pub fn discovery_call_count(&self) -> usize {
        self.state
            .lock()
            .expect("simulated runtime test support")
            .discovery_calls
    }

    /// Returns exact individual-workload lifecycle call counts.
    pub fn call_counts(&self) -> SimulatedRuntimeCallCounts {
        self.state
            .lock()
            .expect("simulated runtime test support")
            .call_counts
    }

    /// Returns exact application group lifecycle calls in issue order.
    pub fn group_calls(&self) -> Vec<SimulatedRuntimeGroupCall> {
        self.state
            .lock()
            .expect("simulated runtime test support")
            .group_calls
            .clone()
    }

    /// Returns the exact application group launches received by the simulated runtime.
    pub fn group_launches(&self) -> Vec<dotcore_runtime::WorkloadGroupLaunch> {
        self.state
            .lock()
            .expect("simulated runtime test support")
            .group_launches
            .clone()
    }

    /// Reports whether simulated storage remains under one owner key.
    pub fn storage_exists(&self, storage: &dotcore_runtime::WorkloadStorageKey) -> bool {
        self.state
            .lock()
            .expect("simulated runtime test support")
            .storage
            .contains(storage)
    }

    /// Places simulated storage under one owner key, as left by an earlier realization this
    /// runtime no longer knows about.
    pub fn insert_storage(&self, storage: dotcore_runtime::WorkloadStorageKey) {
        self.record_storage([storage]);
    }

    fn new(state: SimulatedRuntimeTestState) -> Self {
        Self {
            state: Arc::new(Mutex::new(state)),
            representative_startup: false,
            lifecycle_delays: None,
            preparation_gate: None,
            readiness_gate: None,
            stop_gate: None,
        }
    }

    pub(super) async fn hold_preparation(
        &self,
        cancellation: &CancellationToken,
    ) -> Result<(), ()> {
        let Some(gate) = &self.preparation_gate else {
            return Ok(());
        };
        gate.arrive_and_wait(cancellation).await
    }

    pub(super) fn readiness_released(&self) -> bool {
        self.readiness_gate
            .as_ref()
            .is_none_or(|gate| gate.arrive_and_is_released())
    }

    pub(super) fn stop_released(&self) -> bool {
        self.stop_gate
            .as_ref()
            .is_none_or(|gate| gate.arrive_and_is_released())
    }

    pub(super) fn record_discovery(&self) -> bool {
        let mut state = self.state.lock().expect("simulated runtime test support");
        state.discovery_calls += 1;
        if state.remaining_discovery_failures == 0 {
            return false;
        }
        state.remaining_discovery_failures -= 1;
        true
    }

    pub(super) fn record_create(&self) {
        self.state
            .lock()
            .expect("simulated runtime test support")
            .call_counts
            .creates += 1;
    }

    pub(super) fn record_start(&self) {
        self.state
            .lock()
            .expect("simulated runtime test support")
            .call_counts
            .starts += 1;
    }

    pub(super) fn record_stop(&self) -> bool {
        let mut state = self.state.lock().expect("simulated runtime test support");
        state.call_counts.stops += 1;
        if state.remaining_stop_failures == 0 {
            return false;
        }
        state.remaining_stop_failures -= 1;
        state.call_counts.failed_stops += 1;
        true
    }

    pub(super) fn record_group_call(&self, operation: &str, group_id: &str) -> bool {
        let mut state = self.state.lock().expect("simulated runtime test support");
        state
            .group_calls
            .push(SimulatedRuntimeGroupCall::new(operation, group_id));
        let failures = match operation {
            "start" => &mut state.remaining_group_start_failures,
            "stop" => &mut state.remaining_group_stop_failures,
            _ => return false,
        };
        if *failures == 0 {
            return false;
        }
        *failures -= 1;
        true
    }

    pub(super) fn group_drift(&self, group_id: &str) -> Option<SimulatedRuntimeGroupDrift> {
        self.state
            .lock()
            .expect("simulated runtime test support")
            .group_drifts
            .get(group_id)
            .copied()
    }

    pub(super) fn application_endpoint(&self) -> Option<NonZeroU16> {
        self.state
            .lock()
            .expect("simulated runtime test support")
            .application_endpoint
    }

    pub(super) fn record_storage(
        &self,
        storage: impl IntoIterator<Item = dotcore_runtime::WorkloadStorageKey>,
    ) {
        self.state
            .lock()
            .expect("simulated runtime test support")
            .storage
            .extend(storage);
    }

    pub(super) fn remove_storage(&self, storage: &dotcore_runtime::WorkloadStorageKey) {
        self.state
            .lock()
            .expect("simulated runtime test support")
            .storage
            .remove(storage);
    }
}

/// Exact individual-workload lifecycle calls issued to a simulated runtime.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct SimulatedRuntimeCallCounts {
    pub creates: usize,
    pub starts: usize,
    pub stops: usize,
    pub failed_stops: usize,
}

#[derive(Debug)]
struct RetainedGate {
    state: watch::Sender<RetainedGateState>,
}

impl Default for RetainedGate {
    fn default() -> Self {
        let (state, _) = watch::channel(RetainedGateState::default());
        Self { state }
    }
}

impl RetainedGate {
    async fn wait_until_reached(&self) {
        let mut state = self.state.subscribe();
        while !state.borrow().reached {
            state
                .changed()
                .await
                .expect("simulated runtime gate retained");
        }
    }

    async fn arrive_and_wait(&self, cancellation: &CancellationToken) -> Result<(), ()> {
        self.state.send_modify(|state| state.reached = true);
        let mut state = self.state.subscribe();
        while !state.borrow().released {
            tokio::select! {
                () = cancellation.cancelled() => return Err(()),
                changed = state.changed() => changed.expect("simulated runtime gate retained"),
            }
        }
        Ok(())
    }

    fn arrive_and_is_released(&self) -> bool {
        self.state.send_modify(|state| state.reached = true);
        self.state.borrow().released
    }

    fn release(&self) {
        self.state.send_modify(|state| state.released = true);
    }
}

#[derive(Clone, Copy, Debug, Default)]
struct RetainedGateState {
    reached: bool,
    released: bool,
}

#[derive(Debug, Default)]
pub(super) struct SimulatedRuntimeTestState {
    remaining_discovery_failures: usize,
    discovery_calls: usize,
    remaining_stop_failures: usize,
    call_counts: SimulatedRuntimeCallCounts,
    remaining_group_start_failures: usize,
    remaining_group_stop_failures: usize,
    application_endpoint: Option<NonZeroU16>,
    pub(super) group_calls: Vec<SimulatedRuntimeGroupCall>,
    pub(super) group_launches: Vec<dotcore_runtime::WorkloadGroupLaunch>,
    group_drifts: BTreeMap<String, SimulatedRuntimeGroupDrift>,
    storage: BTreeSet<dotcore_runtime::WorkloadStorageKey>,

    /// Workloads of each execution target kept across backends, when enabled.
    retained_workloads: Option<BTreeMap<String, Arc<Mutex<super::adapter::FakeState>>>>,
}
