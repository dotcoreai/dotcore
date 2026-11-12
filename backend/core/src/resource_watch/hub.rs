//! Publishes console-facing owner reads once per manager tick, a short read delay after each
//! wall-aligned tick slot.
//!
//! Each watched resource kind is read from its owner once per tick however many watches follow
//! it, and only while at least one does; a read whose encoding changed goes to every watch at
//! once, tagged with the tick sequence. Kinds are read independently, so a slow owner delays only
//! its own kind. A watch never waits for a tick to start: it gets the latest published payloads
//! at once, and a kind with none is read right away, without missing or repeating an update.

use std::{
    collections::{BTreeMap, BTreeSet},
    sync::{Arc, Mutex, MutexGuard},
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use dotcore_domain::{
    clock::{Clock, next_boundary},
    timing::WatchTiming,
};
use dotcore_proto::dotcore::watch::v1::keyed_resource::Payload;
use futures_util::future::BoxFuture;
use tokio::{
    sync::{
        OwnedSemaphorePermit, Semaphore,
        broadcast::{self, error::RecvError},
    },
    task::{JoinHandle, JoinSet},
};
use tokio_util::sync::CancellationToken;
use tracing::{debug, warn};

/// Publications a watch may fall behind by before it is cut as lagged: about ten ticks of every
/// kind changing.
const PUBLICATION_CAPACITY: usize = 64;

/// Resource kind a watch can follow; each is one owner read.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum ResourceKind {
    Machines,

    HardwareSnapshots,

    ModelDeployments,

    ApplicationDeployments,

    Routes,

    ExecutionTargets,

    NodeMemory,

    ConsoleSettings,
}

impl ResourceKind {
    /// Lowercase plural name for logs and status messages.
    pub(crate) fn name(self) -> &'static str {
        match self {
            Self::Machines => "machines",
            Self::HardwareSnapshots => "hardware snapshots",
            Self::ModelDeployments => "model deployments",
            Self::ApplicationDeployments => "application deployments",
            Self::Routes => "routes",
            Self::ExecutionTargets => "execution targets",
            Self::NodeMemory => "node memory",
            Self::ConsoleSettings => "console settings",
        }
    }
}

/// One owner's read of a resource kind, answering its full current projection.
pub(crate) trait ResourceRead: Send + Sync {
    /// Reads the owner's current projection. An implementation that blocks the calling thread,
    /// such as one taking a lock held across disk writes, moves that work off the async executor.
    fn read(&self) -> BoxFuture<'_, Result<Payload, ResourceReadError>>;
}

/// A resource owner could not answer its read.
#[derive(Debug, thiserror::Error)]
#[error("resource owner read failed")]
pub(crate) struct ResourceReadError {
    #[source]
    source: Box<dyn std::error::Error + Send + Sync>,
}

impl ResourceReadError {
    pub(crate) fn new(source: impl Into<Box<dyn std::error::Error + Send + Sync>>) -> Self {
        Self {
            source: source.into(),
        }
    }
}

/// The owner read of every resource kind.
pub(crate) struct ResourceReads {
    pub(crate) machines: Arc<dyn ResourceRead>,

    pub(crate) hardware_snapshots: Arc<dyn ResourceRead>,

    pub(crate) model_deployments: Arc<dyn ResourceRead>,

    pub(crate) application_deployments: Arc<dyn ResourceRead>,

    pub(crate) routes: Arc<dyn ResourceRead>,

    pub(crate) execution_targets: Arc<dyn ResourceRead>,

    pub(crate) node_memory: Arc<dyn ResourceRead>,

    pub(crate) console_settings: Arc<dyn ResourceRead>,
}

impl ResourceReads {
    fn of(&self, kind: ResourceKind) -> Arc<dyn ResourceRead> {
        Arc::clone(match kind {
            ResourceKind::Machines => &self.machines,
            ResourceKind::HardwareSnapshots => &self.hardware_snapshots,
            ResourceKind::ModelDeployments => &self.model_deployments,
            ResourceKind::ApplicationDeployments => &self.application_deployments,
            ResourceKind::Routes => &self.routes,
            ResourceKind::ExecutionTargets => &self.execution_targets,
            ResourceKind::NodeMemory => &self.node_memory,
            ResourceKind::ConsoleSettings => &self.console_settings,
        })
    }
}

/// Tick and admission settings of the hub.
#[derive(Clone, Copy, Debug)]
pub(crate) struct ResourceWatchConfig {
    /// Tick slots, aligned on multiples of the tick interval since the epoch, the read delay
    /// after each slot starts, and the heartbeat of a silent watch stream.
    pub(crate) timing: WatchTiming,

    /// Most watches open at once across the process.
    pub(crate) max_watches: usize,
}

impl ResourceWatchConfig {
    /// Ticks on the watch slots of `timing` and sends its heartbeats, admitting the default
    /// number of watches.
    pub(crate) fn new(timing: WatchTiming) -> Self {
        Self {
            timing,
            max_watches: 256,
        }
    }
}

/// Shared publisher of owner reads to every open resource watch of the process.
#[derive(Clone)]
pub(crate) struct ResourceWatchHub {
    shared: Arc<Shared>,
}

impl ResourceWatchHub {
    /// Creates a parked hub publishing under `manager_session_id`; it ticks only while watched.
    pub(crate) fn new(
        manager_session_id: String,
        reads: ResourceReads,
        clock: Arc<dyn Clock>,
        config: ResourceWatchConfig,
    ) -> Self {
        let (publications, _) = broadcast::channel(PUBLICATION_CAPACITY);
        Self {
            shared: Arc::new(Shared {
                manager_session_id,
                reads,
                clock,
                timing: config.timing,
                max_watches: config.max_watches,
                permits: Arc::new(Semaphore::new(config.max_watches)),
                shutdown: CancellationToken::new(),
                publications,
                state: Mutex::new(HubState::default()),
            }),
        }
    }

    pub(crate) fn manager_session_id(&self) -> &str {
        &self.shared.manager_session_id
    }

    pub(crate) fn clock(&self) -> Arc<dyn Clock> {
        Arc::clone(&self.shared.clock)
    }

    /// Watch timing of this hub, which its streams' heartbeats follow.
    pub(crate) fn timing(&self) -> WatchTiming {
        self.shared.timing
    }

    /// Opens a watch of `kinds`, starting the ticks if none ran. The watch first holds the latest
    /// published payload of each kind that has one, up to one tick old; a kind with none is read
    /// right away rather than at the next tick, so the first round never waits for a tick.
    pub(crate) fn watch(&self, kinds: BTreeSet<ResourceKind>) -> Result<HubWatch, AdmissionError> {
        let permit = Arc::clone(&self.shared.permits)
            .try_acquire_owned()
            .map_err(|_| AdmissionError::TooManyWatches {
                limit: self.shared.max_watches,
            })?;
        if self.shared.shutdown.is_cancelled() {
            return Err(AdmissionError::ShutDown);
        }

        let mut state = self.shared.state();
        // Subscribing under the lock that publishing holds means every later publication reaches
        // this watch and none it receives is older than the payloads cached below.
        let publications = self.shared.publications.subscribe();
        let mut cached = kinds
            .iter()
            .filter_map(|kind| {
                state.latest.get(kind).map(|latest| Publication {
                    tick: latest.tick,
                    kind: *kind,
                    outcome: PublishedRead::Payload(Arc::clone(&latest.payload)),
                })
            })
            .collect::<Vec<_>>();
        cached.sort_by_key(|publication| publication.tick);
        for kind in &kinds {
            *state.watchers.entry(*kind).or_default() += 1;
        }
        if state.ticker.is_none() {
            state.epoch += 1;
            state.ticker = Some(tokio::spawn(run(Arc::clone(&self.shared), state.epoch)));
            debug!(
                event_kind = "resource_watch_ticks",
                outcome = "started",
                "Resource watch ticks started"
            );
        }
        // A kind already being read publishes that read to this watch too.
        let unread = kinds
            .iter()
            .filter(|kind| !state.latest.contains_key(kind) && !state.reading.contains(kind))
            .copied()
            .collect::<Vec<_>>();
        state.reading.extend(unread.iter().copied());
        let epoch = state.epoch;
        drop(state);
        for kind in unread {
            tokio::spawn(read_and_publish(Arc::clone(&self.shared), epoch, kind));
        }

        Ok(HubWatch {
            shared: Arc::clone(&self.shared),
            kinds,
            cached,
            publications,
            _permit: permit,
        })
    }

    /// Ends every open watch as unavailable, refuses new ones, and stops the ticks.
    pub(crate) fn shutdown(&self) {
        self.shared.shutdown.cancel();
    }
}

/// Admission failures of a new watch.
#[derive(Debug, thiserror::Error)]
pub(crate) enum AdmissionError {
    #[error("resource watch limit of {limit} open watches is reached")]
    TooManyWatches { limit: usize },

    #[error("resource watches are shutting down")]
    ShutDown,
}

/// One open watch's view of the hub: the payloads cached when it opened, then every publication
/// of its kinds. Dropping it releases its kinds and its admission permit.
pub(crate) struct HubWatch {
    shared: Arc<Shared>,
    kinds: BTreeSet<ResourceKind>,

    /// Latest payloads of the watched kinds when the watch opened, oldest tick first.
    cached: Vec<Publication>,

    publications: broadcast::Receiver<Publication>,
    _permit: OwnedSemaphorePermit,
}

impl HubWatch {
    /// Takes the payloads cached when the watch opened; later calls return none.
    pub(crate) fn take_cached(&mut self) -> Vec<Publication> {
        std::mem::take(&mut self.cached)
    }

    /// Waits for the next publication of a watched kind. Stops at an owner read failure of a
    /// watched kind, a watch fallen behind the publications, or shutdown.
    pub(crate) async fn next(&mut self) -> Result<Publication, HubWatchError> {
        loop {
            let received = tokio::select! {
                biased;
                () = self.shared.shutdown.cancelled() => return Err(HubWatchError::ShutDown),
                received = self.publications.recv() => received,
            };
            match received {
                Ok(publication) if !self.kinds.contains(&publication.kind) => continue,
                Ok(Publication {
                    kind,
                    outcome: PublishedRead::Failed,
                    ..
                }) => return Err(HubWatchError::ReadFailed { kind }),
                Ok(publication) => return Ok(publication),
                Err(RecvError::Lagged(_)) => return Err(HubWatchError::Lagged),
                Err(RecvError::Closed) => return Err(HubWatchError::ShutDown),
            }
        }
    }

    /// Sequence of the latest tick the hub started.
    pub(crate) fn latest_tick(&self) -> u64 {
        self.shared.state().tick
    }
}

impl Drop for HubWatch {
    fn drop(&mut self) {
        self.shared.release(&self.kinds);
    }
}

/// Reasons a watch stops following the hub.
#[derive(Debug, thiserror::Error)]
pub(crate) enum HubWatchError {
    #[error("{} owner read failed", kind.name())]
    ReadFailed { kind: ResourceKind },

    #[error("resource watch fell behind the published ticks")]
    Lagged,

    #[error("resource watches are shutting down")]
    ShutDown,
}

/// One kind's changed read, or its failure, at one tick.
#[derive(Clone, Debug)]
pub(crate) struct Publication {
    pub(crate) tick: u64,
    pub(crate) kind: ResourceKind,
    pub(crate) outcome: PublishedRead,
}

#[derive(Clone, Debug)]
pub(crate) enum PublishedRead {
    Payload(Arc<Payload>),

    Failed,
}

struct Shared {
    manager_session_id: String,
    reads: ResourceReads,
    clock: Arc<dyn Clock>,
    timing: WatchTiming,
    max_watches: usize,
    permits: Arc<Semaphore>,
    shutdown: CancellationToken,

    /// Sent only under the state lock, so publication order matches the cached payloads.
    publications: broadcast::Sender<Publication>,

    state: Mutex<HubState>,
}

impl Shared {
    fn state(&self) -> MutexGuard<'_, HubState> {
        self.state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// Starts the next tick sequence and claims every watched kind with no read in flight.
    fn begin_tick(&self) -> (u64, Vec<ResourceKind>) {
        let mut state = self.state();
        state.tick += 1;
        let kinds = state
            .watchers
            .keys()
            .filter(|kind| !state.reading.contains(kind))
            .copied()
            .collect::<Vec<_>>();
        state.reading.extend(kinds.iter().copied());
        (state.tick, kinds)
    }

    /// Publishes `kind`'s read when its encoding changed, or its failure, tagged with the latest
    /// tick. A read finishing after its kind or the whole hub stopped being watched is dropped.
    fn publish(&self, epoch: u64, kind: ResourceKind, read: Result<Payload, ResourceReadError>) {
        let mut state = self.state();
        if state.epoch != epoch {
            return;
        }
        state.reading.remove(&kind);
        if !state.watchers.contains_key(&kind) {
            return;
        }

        let tick = state.tick;
        let outcome = match read {
            Ok(payload) => {
                let mut encoding = Vec::with_capacity(payload.encoded_len());
                payload.encode(&mut encoding);
                if state
                    .latest
                    .get(&kind)
                    .is_some_and(|latest| latest.encoding == encoding)
                {
                    return;
                }
                let payload = Arc::new(payload);
                state.latest.insert(
                    kind,
                    Latest {
                        tick,
                        payload: Arc::clone(&payload),
                        encoding,
                    },
                );
                PublishedRead::Payload(payload)
            }
            Err(error) => {
                warn!(
                    event_kind = "resource_watch_read",
                    outcome = "failed",
                    resource_kind = kind.name(),
                    tick,
                    error = %error,
                    source = %error.source,
                    "Resource owner read failed; its watches end"
                );
                // A watch reopened after the failure must wait for a fresh read.
                state.latest.remove(&kind);
                PublishedRead::Failed
            }
        };
        // No receiver only means nobody is subscribed right now.
        let _ = self.publications.send(Publication {
            tick,
            kind,
            outcome,
        });
    }

    /// Releases one watch's kinds; the last watch of a kind drops its cached payload, and the last
    /// watch of all parks the ticks.
    fn release(&self, kinds: &BTreeSet<ResourceKind>) {
        let mut state = self.state();
        for kind in kinds {
            if let Some(count) = state.watchers.get_mut(kind) {
                *count -= 1;
                if *count == 0 {
                    state.watchers.remove(kind);
                    state.latest.remove(kind);
                }
            }
        }
        if state.watchers.is_empty()
            && let Some(ticker) = state.ticker.take()
        {
            ticker.abort();
            state.reading.clear();
            debug!(
                event_kind = "resource_watch_ticks",
                outcome = "parked",
                tick = state.tick,
                "Resource watch ticks parked"
            );
        }
    }
}

#[derive(Default)]
struct HubState {
    /// Sequence of the latest tick started; never reset, so it grows across parks.
    tick: u64,

    /// Run of the ticks, renewed at every start so reads of a parked run are dropped.
    epoch: u64,

    /// Open watches per watched kind.
    watchers: BTreeMap<ResourceKind, usize>,

    /// Latest published read per watched kind.
    latest: BTreeMap<ResourceKind, Latest>,

    /// Kinds with a read in flight, skipped by later ticks until it finishes.
    reading: BTreeSet<ResourceKind>,

    ticker: Option<JoinHandle<()>>,
}

struct Latest {
    tick: u64,
    payload: Arc<Payload>,
    encoding: Vec<u8>,
}

/// Ticks a read delay after every tick slot until parked or shut down, starting one read per
/// watched kind with none in flight. A wall clock stepped across the awaited tick, or a wake
/// delayed past it, resumes at the next tick after the wall time it observes: never a burst of
/// catch-up ticks, never a wait for a tick that has passed.
async fn run(shared: Arc<Shared>, epoch: u64) {
    // Dropping the set when parked aborts the reads still in flight.
    let mut reads = JoinSet::new();
    loop {
        let wall = shared.clock.wall_now();
        let due = next_tick_at(
            wall,
            shared.timing.tick_interval,
            shared.timing.tick_read_delay,
        );
        let wait = due.duration_since(wall).unwrap_or_default();
        tokio::select! {
            biased;
            () = shared.shutdown.cancelled() => return,
            () = shared.clock.sleep(wait) => {}
        }

        let woken = shared.clock.wall_now();
        if woken < due || woken >= due + shared.timing.tick_interval {
            continue;
        }
        while reads.try_join_next().is_some() {}
        let (tick, kinds) = shared.begin_tick();
        if !kinds.is_empty() {
            debug!(
                event_kind = "resource_watch_tick",
                tick,
                kind_count = kinds.len(),
                "Resource watch tick reads its watched kinds"
            );
        }
        for kind in kinds {
            reads.spawn(read_and_publish(Arc::clone(&shared), epoch, kind));
        }
    }
}

/// Reads `kind` from its owner and publishes the result under the run `epoch` that claimed it.
async fn read_and_publish(shared: Arc<Shared>, epoch: u64, kind: ResourceKind) {
    let result = shared.reads.of(kind).read().await;
    shared.publish(epoch, kind, result);
}

/// First tick strictly after `wall`: a multiple of `interval` since the epoch plus
/// `read_delay`.
fn next_tick_at(wall: SystemTime, interval: Duration, read_delay: Duration) -> SystemTime {
    next_boundary(wall.checked_sub(read_delay).unwrap_or(UNIX_EPOCH), interval) + read_delay
}

#[cfg(test)]
mod tests {
    use std::time::{Duration, UNIX_EPOCH};

    use dotcore_domain::timing::Timing;

    use super::{
        test_support::{ALL_KINDS, Harness, START_SECONDS, payload},
        *,
    };

    #[tokio::test]
    async fn each_watched_kind_is_read_on_open_then_once_per_tick_whatever_the_watchers() {
        let harness = Harness::new(ResourceWatchConfig::new(Timing::test().watch));
        let kinds = BTreeSet::from([ResourceKind::Machines, ResourceKind::Routes]);
        let mut watches = (0..3)
            .map(|_| harness.hub.watch(kinds.clone()).expect("watch"))
            .collect::<Vec<_>>();

        for watch in &mut watches {
            let mut first = vec![harness.next(watch).await, harness.next(watch).await];
            first.sort_by_key(|(kind, ..)| *kind);
            assert_eq!(
                first,
                [(ResourceKind::Machines, 0, 0), (ResourceKind::Routes, 0, 0)]
            );
        }
        for _ in 0..3 {
            harness.tick().await;
        }
        harness.until_reads(ResourceKind::Machines, 4).await;
        harness.until_reads(ResourceKind::Routes, 4).await;

        assert_eq!(harness.reads(ResourceKind::Machines), 4);
        assert_eq!(harness.reads(ResourceKind::Routes), 4);
        for kind in [
            ResourceKind::HardwareSnapshots,
            ResourceKind::ModelDeployments,
            ResourceKind::ApplicationDeployments,
            ResourceKind::ExecutionTargets,
            ResourceKind::NodeMemory,
            ResourceKind::ConsoleSettings,
        ] {
            assert_eq!(harness.reads(kind), 0, "{} are not watched", kind.name());
        }
    }

    #[tokio::test]
    async fn an_unchanged_read_publishes_nothing_and_a_change_publishes_at_its_tick() {
        for kind in ALL_KINDS {
            let harness = Harness::new(ResourceWatchConfig::new(Timing::test().watch));
            let mut watch = harness.hub.watch(BTreeSet::from([kind])).expect("watch");

            assert_eq!(
                harness.next(&mut watch).await,
                (kind, 0, 0),
                "{}",
                kind.name()
            );
            harness.tick().await;
            harness.until_reads(kind, 2).await;
            harness.set_version(kind, 1);
            harness.tick().await;

            // Publications are ordered, so the change arriving first proves tick 1 sent nothing.
            assert_eq!(
                harness.next(&mut watch).await,
                (kind, 2, 1),
                "{}",
                kind.name()
            );
        }
    }

    #[tokio::test]
    async fn kinds_changed_in_one_tick_share_its_tick_sequence() {
        let harness = Harness::new(ResourceWatchConfig::new(Timing::test().watch));
        let kinds = BTreeSet::from([
            ResourceKind::Machines,
            ResourceKind::ModelDeployments,
            ResourceKind::Routes,
        ]);
        let mut watch = harness.hub.watch(kinds).expect("watch");
        for _ in 0..3 {
            harness.next(&mut watch).await;
        }

        harness.set_version(ResourceKind::Machines, 1);
        harness.set_version(ResourceKind::Routes, 1);
        harness.tick().await;
        let mut changed = vec![
            harness.next(&mut watch).await,
            harness.next(&mut watch).await,
        ];
        changed.sort_by_key(|(kind, ..)| *kind);

        assert_eq!(
            changed,
            [(ResourceKind::Machines, 1, 1), (ResourceKind::Routes, 1, 1)]
        );
    }

    #[tokio::test]
    async fn time_derived_state_is_published_on_the_next_tick_without_any_event() {
        let disconnected_at = UNIX_EPOCH + Duration::from_secs(START_SECONDS + 3);
        let harness = Harness::with_disconnection_at(
            ResourceWatchConfig::new(Timing::production().watch),
            disconnected_at,
        );
        let mut watch = harness
            .hub
            .watch(BTreeSet::from([ResourceKind::Machines]))
            .expect("watch");

        assert_eq!(
            harness.next(&mut watch).await,
            (ResourceKind::Machines, 0, 0)
        );
        for _ in 0..4 {
            harness.tick().await;
        }

        // Ticks 1 to 3 fall before the disconnection; tick 4 is the first after it.
        assert_eq!(
            harness.next(&mut watch).await,
            (ResourceKind::Machines, 4, 1)
        );
    }

    #[tokio::test]
    async fn a_blocked_kind_does_not_delay_another_kind() {
        let harness = Harness::new(ResourceWatchConfig::new(Timing::test().watch));
        let kinds = BTreeSet::from([ResourceKind::ModelDeployments, ResourceKind::Routes]);
        harness.block(ResourceKind::ModelDeployments, true);
        let mut watch = harness.hub.watch(kinds).expect("watch");

        assert_eq!(harness.next(&mut watch).await, (ResourceKind::Routes, 0, 0));
        harness.set_version(ResourceKind::Routes, 1);
        harness.tick().await;
        assert_eq!(harness.next(&mut watch).await, (ResourceKind::Routes, 1, 1));

        // The blocked read is not started again while in flight, and lands at the latest tick.
        assert_eq!(harness.reads(ResourceKind::ModelDeployments), 1);
        harness.block(ResourceKind::ModelDeployments, false);
        assert_eq!(
            harness.next(&mut watch).await,
            (ResourceKind::ModelDeployments, 1, 0)
        );
    }

    #[tokio::test]
    async fn a_watch_joining_between_ticks_starts_without_waiting_and_misses_no_update() {
        let harness = Harness::new(ResourceWatchConfig::new(Timing::test().watch));
        let mut first = harness
            .hub
            .watch(BTreeSet::from([ResourceKind::Machines]))
            .expect("watch");
        harness.next(&mut first).await;

        harness.set_version(ResourceKind::Machines, 1);
        let mut joined = harness
            .hub
            .watch(BTreeSet::from([
                ResourceKind::Machines,
                ResourceKind::Routes,
            ]))
            .expect("watch");
        let cached = joined
            .take_cached()
            .into_iter()
            .map(test_support::summary)
            .collect::<Vec<_>>();
        assert_eq!(cached, [(ResourceKind::Machines, 0, 0)]);
        assert!(joined.take_cached().is_empty());
        // Routes had no payload, so they are read right away rather than at the next tick.
        assert_eq!(
            harness.next(&mut joined).await,
            (ResourceKind::Routes, 0, 0)
        );

        harness.tick().await;
        assert_eq!(
            harness.next(&mut joined).await,
            (ResourceKind::Machines, 1, 1)
        );
        assert_eq!(
            harness.next(&mut first).await,
            (ResourceKind::Machines, 1, 1)
        );
    }

    #[tokio::test]
    async fn a_wall_clock_stepped_back_or_forward_ticks_once_per_corrected_second() {
        let harness = Harness::new(ResourceWatchConfig::new(Timing::production().watch));
        let _watch = harness
            .hub
            .watch(BTreeSet::from([ResourceKind::Machines]))
            .expect("watch");
        harness.tick().await;
        harness.until_reads(ResourceKind::Machines, 2).await;

        harness.clock.step_wall_back(Duration::from_secs(32));
        // Woken before its tick by the stepped clock, the hub waits for the next corrected one.
        harness.tick().await;
        harness.tick().await;
        harness.until_reads(ResourceKind::Machines, 3).await;
        harness.clock.step_wall_forward(Duration::from_secs(102));
        // Woken past its tick, the hub skips the missed ones instead of catching up.
        harness.tick().await;
        harness.tick().await;
        harness.until_reads(ResourceKind::Machines, 4).await;

        assert_eq!(
            harness.read_walls(ResourceKind::Machines),
            [1_000_000, 1_000_250, 970_250, 1_074_250],
            "the read on open, then one per tick of the corrected clock, in milliseconds"
        );
    }

    #[tokio::test]
    async fn nothing_is_read_while_nobody_watches_and_the_sequence_survives_a_park() {
        let harness = Harness::new(ResourceWatchConfig::new(Timing::test().watch));
        harness.clock.advance(Duration::from_secs(5));
        tokio::task::yield_now().await;
        assert_eq!(harness.reads(ResourceKind::Machines), 0);

        let mut watch = harness
            .hub
            .watch(BTreeSet::from([ResourceKind::Machines]))
            .expect("watch");
        harness.next(&mut watch).await;
        harness.tick().await;
        harness.until_reads(ResourceKind::Machines, 2).await;
        drop(watch);
        harness.clock.advance(Duration::from_secs(5));
        tokio::task::yield_now().await;
        assert_eq!(harness.reads(ResourceKind::Machines), 2);

        let mut reopened = harness
            .hub
            .watch(BTreeSet::from([ResourceKind::Machines]))
            .expect("watch");
        assert!(
            reopened.take_cached().is_empty(),
            "a parked hub keeps no payload"
        );

        // Read right away under tick 1, the last one started before the park.
        assert_eq!(
            harness.next(&mut reopened).await,
            (ResourceKind::Machines, 1, 0)
        );
    }

    #[tokio::test]
    async fn an_owner_read_failure_ends_its_watches_and_drops_its_cached_payload() {
        let harness = Harness::new(ResourceWatchConfig::new(Timing::test().watch));
        let mut machines = harness
            .hub
            .watch(BTreeSet::from([ResourceKind::Machines]))
            .expect("watch");
        let mut routes = harness
            .hub
            .watch(BTreeSet::from([ResourceKind::Routes]))
            .expect("watch");
        harness.next(&mut machines).await;
        harness.next(&mut routes).await;

        harness.fail(ResourceKind::Machines);
        harness.set_version(ResourceKind::Routes, 1);
        harness.tick().await;

        assert!(matches!(
            machines.next().await,
            Err(HubWatchError::ReadFailed {
                kind: ResourceKind::Machines
            })
        ));
        assert_eq!(
            harness.next(&mut routes).await,
            (ResourceKind::Routes, 1, 1)
        );
        let mut reopened = harness
            .hub
            .watch(BTreeSet::from([ResourceKind::Machines]))
            .expect("watch");
        assert!(reopened.take_cached().is_empty());
    }

    #[tokio::test]
    async fn a_watch_fallen_behind_the_publications_is_lagged() {
        let harness = Harness::new(ResourceWatchConfig::new(Timing::test().watch));
        let mut watch = harness
            .hub
            .watch(BTreeSet::from([ResourceKind::Machines]))
            .expect("watch");
        let epoch = harness.hub.shared.state().epoch;

        for version in 0..=PUBLICATION_CAPACITY {
            harness.hub.shared.publish(
                epoch,
                ResourceKind::Machines,
                Ok(payload(ResourceKind::Machines, version)),
            );
        }

        assert!(matches!(watch.next().await, Err(HubWatchError::Lagged)));
    }

    #[tokio::test]
    async fn watches_past_the_process_cap_are_refused_until_one_closes() {
        let harness = Harness::new(ResourceWatchConfig {
            max_watches: 2,
            ..ResourceWatchConfig::new(Timing::test().watch)
        });
        let kinds = BTreeSet::from([ResourceKind::Machines]);
        let first = harness.hub.watch(kinds.clone()).expect("first watch");
        let _second = harness.hub.watch(kinds.clone()).expect("second watch");

        assert!(matches!(
            harness.hub.watch(kinds.clone()),
            Err(AdmissionError::TooManyWatches { limit: 2 })
        ));
        drop(first);
        assert!(harness.hub.watch(kinds).is_ok());
    }

    #[tokio::test]
    async fn shutdown_ends_open_watches_and_refuses_new_ones() {
        let harness = Harness::new(ResourceWatchConfig::new(Timing::test().watch));
        let kinds = BTreeSet::from([ResourceKind::Machines]);
        let mut watch = harness.hub.watch(kinds.clone()).expect("watch");

        harness.hub.shutdown();

        assert!(matches!(watch.next().await, Err(HubWatchError::ShutDown)));
        assert!(matches!(
            harness.hub.watch(kinds),
            Err(AdmissionError::ShutDown)
        ));
    }

    #[test]
    fn ticks_fall_a_read_delay_after_each_wall_aligned_slot() {
        let millisecond = Duration::from_millis(1);

        for timing in [Timing::production().watch, Timing::test().watch] {
            let (interval, delay) = (timing.tick_interval, timing.tick_read_delay);
            let slot = UNIX_EPOCH + interval * 1_000;
            let next = slot + interval + delay;
            // Wall time, then the tick it is followed by: the slot's own tick until it passes,
            // then the next slot's.
            let ticks = [
                (slot, slot + delay),
                (slot + delay - millisecond, slot + delay),
                (slot + delay, next),
                (slot + interval - millisecond, next),
            ];

            for (wall, tick) in ticks {
                assert_eq!(
                    next_tick_at(wall, interval, delay),
                    tick,
                    "{wall:?} under {timing:?}"
                );
            }
        }
    }
}

#[cfg(test)]
pub(super) mod test_support {
    use std::{
        collections::BTreeMap,
        sync::{
            Arc, Mutex,
            atomic::{AtomicBool, AtomicUsize, Ordering},
        },
        time::{Duration, SystemTime, UNIX_EPOCH},
    };

    use dotcore_domain::{
        clock::{Clock, VirtualClock},
        timing::WatchTiming,
    };
    use dotcore_proto::dotcore::{
        application::v1::ListApplicationDeploymentsResponse,
        capacity::v1::ListNodeMemoryResponse,
        inference::v1::ListRoutesResponse,
        model::v1::ListModelDeploymentsResponse,
        site::v1::{
            ConsoleSettings, GetConsoleSettingsResponse, ListConfiguredExecutionTargetsResponse,
            ListHardwareSnapshotsResponse, ListMachinesResponse,
        },
        watch::v1::keyed_resource::Payload,
    };
    use futures_util::future::BoxFuture;
    use tokio::sync::watch;

    use super::{
        HubWatch, Publication, PublishedRead, ResourceKind, ResourceRead, ResourceReadError,
        ResourceReads, ResourceWatchConfig, ResourceWatchHub,
    };

    /// Wall second the harness clock starts on, so ticks fall at its quarter seconds.
    pub(crate) const START_SECONDS: u64 = 1_000;

    /// Real-time bound for progress the test waits on; never reached when the hub works.
    const PROGRESS_DEADLINE: Duration = Duration::from_secs(10);

    pub(crate) const ALL_KINDS: [ResourceKind; 8] = [
        ResourceKind::Machines,
        ResourceKind::HardwareSnapshots,
        ResourceKind::ModelDeployments,
        ResourceKind::ApplicationDeployments,
        ResourceKind::Routes,
        ResourceKind::ExecutionTargets,
        ResourceKind::NodeMemory,
        ResourceKind::ConsoleSettings,
    ];

    /// A hub over fake owners and a virtual clock.
    pub(crate) struct Harness {
        pub(crate) clock: VirtualClock,
        pub(crate) hub: ResourceWatchHub,
        owners: BTreeMap<ResourceKind, Arc<FakeRead>>,
        timing: WatchTiming,
    }

    impl Harness {
        pub(crate) fn new(config: ResourceWatchConfig) -> Self {
            Self::build(config, None)
        }

        /// A harness whose machines read turns one machine disconnected once wall time reaches
        /// `disconnected_at`, with no event announcing it.
        pub(crate) fn with_disconnection_at(
            config: ResourceWatchConfig,
            disconnected_at: SystemTime,
        ) -> Self {
            Self::build(config, Some(disconnected_at))
        }

        fn build(config: ResourceWatchConfig, disconnected_at: Option<SystemTime>) -> Self {
            let clock = VirtualClock::starting_at(UNIX_EPOCH + Duration::from_secs(START_SECONDS));
            let owners = ALL_KINDS
                .into_iter()
                .map(|kind| {
                    (
                        kind,
                        Arc::new(FakeRead {
                            kind,
                            clock: clock.clone(),
                            version: AtomicUsize::new(0),
                            failing: AtomicBool::new(false),
                            blocked: watch::Sender::new(false),
                            read_walls: Mutex::new(Vec::new()),
                            disconnected_at: disconnected_at
                                .filter(|_| kind == ResourceKind::Machines),
                        }),
                    )
                })
                .collect::<BTreeMap<_, _>>();
            let read = |kind| Arc::clone(&owners[&kind]) as Arc<dyn ResourceRead>;
            let reads = ResourceReads {
                machines: read(ResourceKind::Machines),
                hardware_snapshots: read(ResourceKind::HardwareSnapshots),
                model_deployments: read(ResourceKind::ModelDeployments),
                application_deployments: read(ResourceKind::ApplicationDeployments),
                routes: read(ResourceKind::Routes),
                execution_targets: read(ResourceKind::ExecutionTargets),
                node_memory: read(ResourceKind::NodeMemory),
                console_settings: read(ResourceKind::ConsoleSettings),
            };
            let hub = ResourceWatchHub::new("a".repeat(32), reads, Arc::new(clock.clone()), config);
            Self {
                clock,
                hub,
                owners,
                timing: config.timing,
            }
        }

        /// Advances to the hub's next wake, which is its next tick unless the wall clock was
        /// stepped.
        pub(crate) async fn tick(&self) {
            self.clock
                .advance_to_next_wake_within(
                    self.timing.tick_interval + self.timing.tick_read_delay,
                )
                .await;
        }

        /// Advances to the next tick and waits until the hub started it, so a later advance
        /// never overtakes it.
        pub(crate) async fn tick_started(&self) {
            let started = self.hub.shared.state().tick + 1;
            self.tick().await;
            let reached = tokio::time::timeout(PROGRESS_DEADLINE, async {
                while self.hub.shared.state().tick < started {
                    tokio::task::yield_now().await;
                }
            })
            .await;
            assert!(reached.is_ok(), "tick {started} started");
        }

        /// Next publication of `watch` as kind, tick sequence and payload version.
        pub(crate) async fn next(&self, watch: &mut HubWatch) -> (ResourceKind, u64, usize) {
            let publication = tokio::time::timeout(PROGRESS_DEADLINE, watch.next())
                .await
                .expect("publication within the progress deadline")
                .expect("publication");
            summary(publication)
        }

        /// Waits until `kind` was read `count` times.
        pub(crate) async fn until_reads(&self, kind: ResourceKind, count: usize) {
            let reached = tokio::time::timeout(PROGRESS_DEADLINE, async {
                while self.reads(kind) < count {
                    tokio::task::yield_now().await;
                }
            })
            .await;
            assert!(
                reached.is_ok(),
                "{} read {} times, expected {count}",
                kind.name(),
                self.reads(kind)
            );
        }

        pub(crate) fn reads(&self, kind: ResourceKind) -> usize {
            self.owners[&kind].read_walls.lock().expect("reads").len()
        }

        /// Wall time of every read of `kind`, in Unix milliseconds.
        pub(crate) fn read_walls(&self, kind: ResourceKind) -> Vec<u64> {
            self.owners[&kind]
                .read_walls
                .lock()
                .expect("reads")
                .iter()
                .map(|wall| wall_millis(*wall))
                .collect()
        }

        /// Makes `kind`'s owner answer `version` from its next read.
        pub(crate) fn set_version(&self, kind: ResourceKind, version: usize) {
            self.owners[&kind].version.store(version, Ordering::SeqCst);
        }

        /// Makes `kind`'s owner hold its reads until unblocked.
        pub(crate) fn block(&self, kind: ResourceKind, blocked: bool) {
            self.owners[&kind].blocked.send_replace(blocked);
        }

        /// Makes every later read of `kind` fail.
        pub(crate) fn fail(&self, kind: ResourceKind) {
            self.owners[&kind].failing.store(true, Ordering::SeqCst);
        }
    }

    /// Kind, tick sequence and payload version of one publication.
    pub(crate) fn summary(publication: Publication) -> (ResourceKind, u64, usize) {
        let PublishedRead::Payload(payload) = publication.outcome else {
            panic!("{} read failed", publication.kind.name());
        };
        (publication.kind, publication.tick, version(&payload))
    }

    /// A payload of `kind` whose version is its entry count.
    pub(crate) fn payload(kind: ResourceKind, version: usize) -> Payload {
        match kind {
            ResourceKind::Machines => Payload::Machines(ListMachinesResponse {
                machines: vec![Default::default(); version],
            }),
            ResourceKind::HardwareSnapshots => {
                Payload::HardwareSnapshots(ListHardwareSnapshotsResponse {
                    machines: vec![Default::default(); version],
                })
            }
            ResourceKind::ModelDeployments => {
                Payload::ModelDeployments(ListModelDeploymentsResponse {
                    deployments: vec![Default::default(); version],
                })
            }
            ResourceKind::ApplicationDeployments => {
                Payload::ApplicationDeployments(ListApplicationDeploymentsResponse {
                    deployments: vec![Default::default(); version],
                })
            }
            ResourceKind::Routes => Payload::Routes(ListRoutesResponse {
                routes: vec![Default::default(); version],
            }),
            ResourceKind::ExecutionTargets => {
                Payload::ExecutionTargets(ListConfiguredExecutionTargetsResponse {
                    execution_targets: vec![Default::default(); version],
                })
            }
            ResourceKind::NodeMemory => Payload::NodeMemory(ListNodeMemoryResponse {
                nodes: vec![Default::default(); version],
            }),
            // Console settings hold one flag, so their only versions are 0 and 1.
            ResourceKind::ConsoleSettings => Payload::ConsoleSettings(GetConsoleSettingsResponse {
                settings: Some(ConsoleSettings {
                    get_started_hidden: version == 1,
                }),
            }),
        }
    }

    pub(crate) fn version(payload: &Payload) -> usize {
        match payload {
            Payload::Machines(response) => response.machines.len(),
            Payload::HardwareSnapshots(response) => response.machines.len(),
            Payload::ModelDeployments(response) => response.deployments.len(),
            Payload::ApplicationDeployments(response) => response.deployments.len(),
            Payload::Routes(response) => response.routes.len(),
            Payload::ExecutionTargets(response) => response.execution_targets.len(),
            Payload::NodeMemory(response) => response.nodes.len(),
            Payload::ConsoleSettings(response) => usize::from(
                response
                    .settings
                    .as_ref()
                    .is_some_and(|settings| settings.get_started_hidden),
            ),
        }
    }

    /// Unix milliseconds of `wall`.
    pub(crate) fn wall_millis(wall: SystemTime) -> u64 {
        wall.duration_since(UNIX_EPOCH)
            .expect("after epoch")
            .as_millis() as u64
    }

    /// Owner of one kind answering a settable version, which can be held or made to fail.
    struct FakeRead {
        kind: ResourceKind,
        clock: VirtualClock,
        version: AtomicUsize,
        failing: AtomicBool,
        blocked: watch::Sender<bool>,
        read_walls: Mutex<Vec<SystemTime>>,

        /// Wall time from which the answer gains one entry, as a node turning disconnected.
        disconnected_at: Option<SystemTime>,
    }

    impl ResourceRead for FakeRead {
        fn read(&self) -> BoxFuture<'_, Result<Payload, ResourceReadError>> {
            Box::pin(async move {
                let wall = self.clock.wall_now();
                self.read_walls.lock().expect("reads").push(wall);
                let mut blocked = self.blocked.subscribe();
                blocked
                    .wait_for(|blocked| !*blocked)
                    .await
                    .expect("owner stays");
                if self.failing.load(Ordering::SeqCst) {
                    return Err(ResourceReadError::new(std::io::Error::other(
                        "owner unavailable",
                    )));
                }
                let derived = self
                    .disconnected_at
                    .is_some_and(|disconnected_at| wall >= disconnected_at);
                Ok(payload(
                    self.kind,
                    self.version.load(Ordering::SeqCst) + usize::from(derived),
                ))
            })
        }
    }
}
