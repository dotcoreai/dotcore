//! Serialized host-local hardware collection for the local host.
//!
//! The capability runs one complete observation at a time on a dedicated worker thread, so a
//! timed-out vendor call cannot retain Tokio blocking-pool work during backend shutdown. Concurrent
//! callers share the in-flight observation instead of starting overlapping collection. Light memory
//! samples run the same way on a second worker thread, so a slow full observation never holds back
//! the next sample.

use std::{
    panic::{AssertUnwindSafe, catch_unwind},
    sync::{
        Arc, Mutex,
        mpsc::{self, Sender},
    },
    thread,
};

use dotcore_domain::hardware_observation::{
    HardwareSample, HardwareSnapshotSource, NodeHardwareObservation,
};
#[cfg(test)]
use dotcore_proto::dotcore::observation::v1::HardwareSnapshot;
use tokio::sync::oneshot;

/// Collects current hardware facts and memory samples for the local host.
pub struct LocalNodeCapability<C> {
    observations: SharedCollection<NodeHardwareObservation>,
    samples: SharedCollection<HardwareSample>,
    _collector: std::marker::PhantomData<fn() -> C>,
}

impl<C> LocalNodeCapability<C>
where
    C: HardwareSnapshotSource + Send + Sync + 'static,
{
    /// Creates the local collection capability over one hardware collector.
    pub fn new(collector: C) -> Self {
        let collector = Arc::new(collector);
        let observing = Arc::clone(&collector);
        Self {
            observations: SharedCollection::spawn("dotcore-hardware-collector", move || {
                observing.collect_observation()
            }),
            samples: SharedCollection::spawn("dotcore-hardware-sampler", move || {
                collector.sample()
            }),
            _collector: std::marker::PhantomData,
        }
    }

    /// Collects a fresh snapshot without blocking an async executor worker.
    #[cfg(test)]
    pub(crate) async fn collect_snapshot(&self) -> Result<HardwareSnapshot, LocalCollectionError> {
        Ok(self.collect_observation().await?.snapshot)
    }

    /// Collects one complete public and private observation outside async executor workers.
    pub async fn collect_observation(
        &self,
    ) -> Result<NodeHardwareObservation, LocalCollectionError> {
        self.observations.collect().await
    }

    /// Takes one light memory sample outside async executor workers.
    pub async fn sample(&self) -> Result<HardwareSample, LocalCollectionError> {
        self.samples.collect().await
    }
}

/// Reason a local collection worker produced no observation or sample.
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
pub enum LocalCollectionError {
    #[error("hardware collection worker stopped before producing a result")]
    WorkerUnavailable,

    #[error("hardware collection worker panicked while producing a result")]
    WorkerPanicked,
}

/// One dedicated worker thread producing a `T` on request; callers arriving while one is in
/// flight share its result.
struct SharedCollection<T> {
    requests: Sender<()>,
    state: Arc<CollectionState<T>>,
}

impl<T> SharedCollection<T>
where
    T: Clone + Send + 'static,
{
    fn spawn(name: &str, collect: impl Fn() -> T + Send + 'static) -> Self {
        let (requests, receiver) = mpsc::channel();
        let state = Arc::new(CollectionState::default());
        thread::Builder::new()
            .name(name.to_owned())
            .spawn({
                let state = Arc::clone(&state);
                move || run_collection_worker(collect, receiver, state)
            })
            .expect("hardware collection worker thread starts");
        Self { requests, state }
    }

    async fn collect(&self) -> Result<T, LocalCollectionError> {
        let (sender, receiver) = oneshot::channel();
        let should_start = {
            let mut state = self.state.inner.lock().expect("collection state");
            if state.stopped {
                return Err(LocalCollectionError::WorkerUnavailable);
            }
            state.waiters.push(sender);
            if state.collecting {
                false
            } else {
                state.collecting = true;
                true
            }
        };

        if should_start && self.requests.send(()).is_err() {
            let mut state = self.state.inner.lock().expect("collection state");
            state.collecting = false;
            state.stopped = true;
            state.waiters.clear();
            return Err(LocalCollectionError::WorkerUnavailable);
        }

        receiver
            .await
            .map_err(|_| LocalCollectionError::WorkerUnavailable)?
    }
}

struct CollectionState<T> {
    inner: Mutex<CollectionStateInner<T>>,
}

impl<T> Default for CollectionState<T> {
    fn default() -> Self {
        Self {
            inner: Mutex::new(CollectionStateInner {
                collecting: false,
                stopped: false,
                waiters: Vec::new(),
            }),
        }
    }
}

struct CollectionStateInner<T> {
    collecting: bool,
    stopped: bool,
    waiters: Vec<oneshot::Sender<Result<T, LocalCollectionError>>>,
}

fn run_collection_worker<T: Clone>(
    collect: impl Fn() -> T,
    requests: mpsc::Receiver<()>,
    state: Arc<CollectionState<T>>,
) {
    while requests.recv().is_ok() {
        let observation = catch_unwind(AssertUnwindSafe(&collect))
            .map_err(|_| LocalCollectionError::WorkerPanicked);
        let worker_panicked = observation.is_err();
        let waiters = {
            let mut state = state.inner.lock().expect("collection state");
            state.collecting = false;
            state.stopped = worker_panicked;
            std::mem::take(&mut state.waiters)
        };
        for waiter in waiters {
            let _ = waiter.send(observation.clone());
        }
        if worker_panicked {
            break;
        }
    }
}

#[cfg(test)]
mod tests {
    use std::{
        future::{Future, poll_fn},
        pin::Pin,
        sync::{
            Arc, Mutex,
            atomic::{AtomicUsize, Ordering},
            mpsc::{self, Receiver, Sender},
        },
        task::Poll,
        thread,
        time::Duration,
    };

    use tokio::{sync::Notify, time::sleep};

    use super::*;

    struct BlockingCollector {
        snapshot: HardwareSnapshot,
        started: Sender<()>,
        release: Mutex<Receiver<()>>,
        probe: CollectorProbe,
    }

    #[derive(Clone)]
    struct CollectorProbe {
        calls: Arc<AtomicUsize>,
        active: Arc<AtomicUsize>,
        maximum_active: Arc<AtomicUsize>,
        started: Arc<Notify>,
        active_started: Arc<Notify>,
    }

    impl BlockingCollector {
        fn new(
            snapshot: HardwareSnapshot,
            started: Sender<()>,
            release: Receiver<()>,
        ) -> (Self, CollectorProbe) {
            let probe = CollectorProbe {
                calls: Arc::new(AtomicUsize::new(0)),
                active: Arc::new(AtomicUsize::new(0)),
                maximum_active: Arc::new(AtomicUsize::new(0)),
                started: Arc::new(Notify::new()),
                active_started: Arc::new(Notify::new()),
            };
            (
                Self {
                    snapshot,
                    started,
                    release: Mutex::new(release),
                    probe: probe.clone(),
                },
                probe,
            )
        }
    }

    impl CollectorProbe {
        fn calls(&self) -> usize {
            self.calls.load(Ordering::SeqCst)
        }

        fn maximum_active(&self) -> usize {
            self.maximum_active.load(Ordering::SeqCst)
        }

        async fn wait_for_active(&self, count: usize) {
            loop {
                let notified = self.active_started.notified();
                if self.active.load(Ordering::SeqCst) >= count {
                    return;
                }
                notified.await;
            }
        }

        async fn wait_for_calls(&self, count: usize) {
            loop {
                let notified = self.started.notified();
                if self.calls() >= count {
                    return;
                }
                notified.await;
            }
        }
    }

    impl HardwareSnapshotSource for BlockingCollector {
        fn collect_snapshot(&self) -> HardwareSnapshot {
            self.probe.calls.fetch_add(1, Ordering::SeqCst);
            self.probe.started.notify_waiters();
            let active = self.probe.active.fetch_add(1, Ordering::SeqCst) + 1;
            self.probe
                .maximum_active
                .fetch_max(active, Ordering::SeqCst);
            self.probe.active_started.notify_waiters();
            self.started
                .send(())
                .expect("test receives collection start");
            self.release
                .lock()
                .expect("release receiver")
                .recv()
                .expect("test releases collection");
            self.probe.active.fetch_sub(1, Ordering::SeqCst);
            self.snapshot.clone()
        }

        fn sample(&self) -> HardwareSample {
            sample()
        }
    }

    struct PanickingCollector;

    impl HardwareSnapshotSource for PanickingCollector {
        fn collect_snapshot(&self) -> HardwareSnapshot {
            panic!("collector panic")
        }

        fn sample(&self) -> HardwareSample {
            panic!("collector panic")
        }
    }

    async fn assert_request_waits_for_in_flight_collection<T>(
        mut request: Pin<&mut impl Future<Output = T>>,
    ) {
        poll_fn(|context| match request.as_mut().poll(context) {
            Poll::Pending => Poll::Ready(()),
            Poll::Ready(_) => {
                panic!("request completed while hardware collection is still in flight")
            }
        })
        .await;
    }

    #[tokio::test(flavor = "current_thread")]
    async fn collection_leaves_a_current_thread_worker_available_for_timer_progress() {
        let (started_sender, started_receiver) = mpsc::channel();
        let (release_sender, release_receiver) = mpsc::channel();
        let (timer_complete_sender, timer_complete_receiver) = mpsc::channel();
        let (collector, _) = BlockingCollector::new(
            HardwareSnapshot::default(),
            started_sender,
            release_receiver,
        );
        let capability = Arc::new(LocalNodeCapability::new(collector));
        let runtime = tokio::runtime::Handle::current();
        let observer = thread::spawn(move || {
            started_receiver.recv().expect("collection starts");
            runtime.spawn(async move {
                sleep(Duration::from_millis(1)).await;
                timer_complete_sender
                    .send(())
                    .expect("observer receives timer");
            });
            let timer_completed_before_release = timer_complete_receiver
                .recv_timeout(Duration::from_secs(1))
                .is_ok();
            release_sender.send(()).expect("collection release");
            timer_completed_before_release
        });

        let snapshot = capability
            .collect_snapshot()
            .await
            .expect("collection task completes");

        assert_eq!(snapshot, HardwareSnapshot::default());
        assert!(observer.join().expect("observer completes"));
    }

    #[tokio::test]
    async fn a_sample_does_not_wait_for_an_in_flight_collection() {
        let (started_sender, _started_receiver) = mpsc::channel();
        let (release_sender, release_receiver) = mpsc::channel();
        let (collector, probe) = BlockingCollector::new(
            HardwareSnapshot::default(),
            started_sender,
            release_receiver,
        );
        let capability = Arc::new(LocalNodeCapability::new(collector));
        let collection = tokio::spawn({
            let capability = Arc::clone(&capability);
            async move { capability.collect_snapshot().await }
        });
        probe.wait_for_active(1).await;

        assert_eq!(capability.sample().await, Ok(sample()));

        release_sender.send(()).expect("release collection");
        collection
            .await
            .expect("collection task completes")
            .expect("collector succeeds");
    }

    #[tokio::test]
    async fn concurrent_callers_share_one_collection() {
        let (started_sender, _started_receiver) = mpsc::channel();
        let (release_sender, release_receiver) = mpsc::channel();
        let (collector, probe) = BlockingCollector::new(
            HardwareSnapshot::default(),
            started_sender,
            release_receiver,
        );
        let capability = Arc::new(LocalNodeCapability::new(collector));

        let first = tokio::spawn({
            let capability = Arc::clone(&capability);
            async move { capability.collect_snapshot().await }
        });
        probe.wait_for_active(1).await;

        let second = capability.collect_snapshot();
        tokio::pin!(second);
        assert_request_waits_for_in_flight_collection(second.as_mut()).await;
        assert_eq!(probe.calls(), 1);
        assert_eq!(probe.maximum_active(), 1);

        release_sender.send(()).expect("release collection");
        let first_snapshot = first
            .await
            .expect("first collection task completes")
            .expect("first collector succeeds");
        let second_snapshot = second.await.expect("second collector succeeds");

        assert_eq!(first_snapshot, HardwareSnapshot::default());
        assert_eq!(second_snapshot, first_snapshot);
        assert_eq!(probe.calls(), 1);
        assert_eq!(probe.maximum_active(), 1);
    }

    #[tokio::test]
    async fn cancelled_caller_leaves_in_flight_collection_available() {
        let (started_sender, _started_receiver) = mpsc::channel();
        let (release_sender, release_receiver) = mpsc::channel();
        let (collector, probe) = BlockingCollector::new(
            HardwareSnapshot::default(),
            started_sender,
            release_receiver,
        );
        let capability = Arc::new(LocalNodeCapability::new(collector));

        let first = tokio::spawn({
            let capability = Arc::clone(&capability);
            async move { capability.collect_snapshot().await }
        });
        probe.wait_for_calls(1).await;
        first.abort();
        assert!(
            first
                .await
                .expect_err("request is cancelled")
                .is_cancelled()
        );

        let second = capability.collect_snapshot();
        tokio::pin!(second);
        assert_request_waits_for_in_flight_collection(second.as_mut()).await;
        assert_eq!(probe.calls(), 1);

        release_sender.send(()).expect("release collection");
        let snapshot = second.await.expect("second collector succeeds");

        assert_eq!(snapshot, HardwareSnapshot::default());
        assert_eq!(probe.calls(), 1);
        assert_eq!(probe.maximum_active(), 1);
    }

    #[tokio::test]
    async fn cancelling_while_waiting_for_the_gate_does_not_collect() {
        let (started_sender, _started_receiver) = mpsc::channel();
        let (release_sender, release_receiver) = mpsc::channel();
        let (collector, probe) = BlockingCollector::new(
            HardwareSnapshot::default(),
            started_sender,
            release_receiver,
        );
        let capability = Arc::new(LocalNodeCapability::new(collector));

        let first = tokio::spawn({
            let capability = Arc::clone(&capability);
            async move { capability.collect_snapshot().await }
        });
        probe.wait_for_calls(1).await;

        {
            let waiting = capability.collect_snapshot();
            tokio::pin!(waiting);
            assert_request_waits_for_in_flight_collection(waiting.as_mut()).await;
        }

        release_sender.send(()).expect("release first collection");
        first
            .await
            .expect("first collection task completes")
            .expect("first collector succeeds");

        assert_eq!(probe.calls(), 1);
    }

    #[tokio::test]
    async fn collector_panic_is_reported_and_stops_the_worker() {
        let capability = LocalNodeCapability::new(PanickingCollector);

        assert_eq!(
            capability.collect_snapshot().await,
            Err(LocalCollectionError::WorkerPanicked)
        );
        assert_eq!(
            capability.collect_snapshot().await,
            Err(LocalCollectionError::WorkerUnavailable)
        );
    }
    #[test]
    fn timed_out_collection_does_not_block_runtime_shutdown() {
        let (started_sender, started_receiver) = mpsc::channel();
        let (release_sender, release_receiver) = mpsc::channel();
        let (shutdown_sender, shutdown_receiver) = mpsc::channel();
        let (collector, _) = BlockingCollector::new(
            HardwareSnapshot::default(),
            started_sender,
            release_receiver,
        );
        let runtime_thread = thread::spawn(move || {
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_time()
                .build()
                .expect("test runtime builds");
            runtime.block_on(async move {
                let capability = LocalNodeCapability::new(collector);
                let result =
                    tokio::time::timeout(Duration::from_millis(10), capability.collect_snapshot())
                        .await;
                assert!(result.is_err(), "blocked collection times out");
            });
            drop(runtime);
            shutdown_sender
                .send(())
                .expect("runtime shutdown is observed");
        });

        started_receiver
            .recv_timeout(Duration::from_secs(1))
            .expect("collection starts");
        let shutdown_completed = shutdown_receiver
            .recv_timeout(Duration::from_secs(1))
            .is_ok();
        release_sender.send(()).expect("release collection");
        runtime_thread.join().expect("runtime thread completes");

        assert!(
            shutdown_completed,
            "timed-out hardware collection retained blocking work during runtime shutdown"
        );
    }

    fn sample() -> HardwareSample {
        HardwareSample {
            host: dotcore_domain::hardware_observation::HostSample {
                memory: Some(dotcore_domain::hardware_observation::HostMemorySample {
                    total_bytes: 64,
                    available_bytes: 40,
                    cached_bytes: None,
                }),
                ..Default::default()
            },
            accelerators: vec![],
        }
    }
}
