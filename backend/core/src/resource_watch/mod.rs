//! Serves the management resource watch API: one stream carrying keyed subscriptions to owner
//! reads, each sent in full on open and again whenever a manager tick finds it changed.
//!
//! The management API owns no resource state here: every payload is an owner's existing read,
//! taken once per tick for all watches together. A new stream never waits for a tick: it starts
//! from the latest payloads, and a kind with none is read right away. A stream announces when its first round is
//! complete, stays alive with heartbeats while idle, and ends with an explicit status as soon as
//! any of its subscriptions cannot continue.

mod hub;
mod reads;

use std::{
    collections::{BTreeMap, BTreeSet, VecDeque},
    pin::Pin,
    sync::Arc,
    time::Duration,
};

use dotcore_domain::clock::Clock;
use dotcore_proto::{
    dotcore::watch::v1::{
        KeyedResource, WatchResourcesRequest, WatchResourcesResponse, resource_subscription,
        resource_watch_service_server::{
            ResourceWatchService as ResourceWatchServiceContract, ResourceWatchServiceServer,
        },
        watch_resources_response,
    },
    google::protobuf::Empty,
};
use futures_util::{Stream, stream};
use tokio::time::Instant;
use tonic::{Request, Response, Status};
use tracing::debug;

pub(crate) use hub::{ResourceWatchConfig, ResourceWatchHub};
pub(crate) use reads::owner_reads;

use hub::{AdmissionError, HubWatch, HubWatchError, Publication, PublishedRead, ResourceKind};

/// Most subscriptions one watch stream carries.
const MAX_SUBSCRIPTIONS: usize = 16;

/// Longest key naming a subscription, in bytes.
const MAX_KEY_BYTES: usize = 64;

/// Public management `ResourceWatchService` over the process's watch hub.
#[derive(Clone)]
pub(crate) struct ResourceWatchService {
    hub: ResourceWatchHub,
}

impl ResourceWatchService {
    /// Creates the service publishing through `hub`.
    pub(crate) fn new(hub: ResourceWatchHub) -> Self {
        Self { hub }
    }

    /// Wraps the service for route assembly.
    pub(crate) fn into_server(self) -> ResourceWatchServiceServer<Self> {
        ResourceWatchServiceServer::new(self)
    }
}

#[tonic::async_trait]
impl ResourceWatchServiceContract for ResourceWatchService {
    type WatchResourcesStream =
        Pin<Box<dyn Stream<Item = Result<WatchResourcesResponse, Status>> + Send>>;

    async fn watch_resources(
        &self,
        request: Request<WatchResourcesRequest>,
    ) -> Result<Response<Self::WatchResourcesStream>, Status> {
        let subscriptions = subscriptions(request.into_inner())?;
        let mut keys = BTreeMap::<ResourceKind, Vec<String>>::new();
        for (key, kind) in subscriptions {
            keys.entry(kind).or_default().push(key);
        }
        let mut watch = self
            .hub
            .watch(keys.keys().copied().collect())
            .map_err(admission_status)?;
        debug!(
            event_kind = "resource_watch",
            outcome = "opened",
            kind_count = keys.len(),
            "Resource watch opened"
        );

        let clock = self.hub.clock();
        let stream = ResourceStream {
            manager_session_id: self.hub.manager_session_id().to_owned(),
            awaiting: keys.keys().copied().collect(),
            cached: watch.take_cached().into(),
            watch,
            keys,
            completed: false,
            queued: VecDeque::new(),
            tick: 0,
            heartbeat_interval: self.hub.heartbeat_interval(),
            sent_at: clock.now(),
            clock,
        };
        Ok(Response::new(Box::pin(stream::unfold(
            Some(stream),
            |stream| async move {
                let mut stream = stream?;
                match stream.next().await {
                    Ok(response) => Some((Ok(response), Some(stream))),
                    Err(status) => {
                        debug!(
                            event_kind = "resource_watch",
                            outcome = "ended",
                            code = ?status.code(),
                            "Resource watch ended"
                        );
                        // Dropping the stream releases its kinds and its admission permit.
                        Some((Err(status), None))
                    }
                }
            },
        ))))
    }
}

/// One watch stream: the cached first round, then live publications fanned out to every key of
/// their kind, with heartbeats in between.
struct ResourceStream {
    manager_session_id: String,
    watch: HubWatch,
    clock: Arc<dyn Clock>,

    /// Subscription keys per watched kind, in request order.
    keys: BTreeMap<ResourceKind, Vec<String>>,

    /// Kinds that have not sent their first payload yet.
    awaiting: BTreeSet<ResourceKind>,

    /// Whether the first-round completion was sent.
    completed: bool,

    /// Payloads cached by the hub when the stream opened, not yet fanned out.
    cached: VecDeque<Publication>,

    /// Events ready to send, in order.
    queued: VecDeque<watch_resources_response::Event>,

    /// Latest tick sent on this stream.
    tick: u64,

    /// Longest this stream stays silent before it sends a heartbeat.
    heartbeat_interval: Duration,

    /// When the latest event was sent.
    sent_at: Instant,
}

impl ResourceStream {
    async fn next(&mut self) -> Result<WatchResourcesResponse, Status> {
        loop {
            if let Some(event) = self.queued.pop_front() {
                return Ok(self.send(event));
            }
            if !self.completed && self.awaiting.is_empty() {
                self.completed = true;
                return Ok(self.send(watch_resources_response::Event::SnapshotComplete(Empty {})));
            }
            if let Some(publication) = self.cached.pop_front() {
                self.accept(publication);
                continue;
            }

            let heartbeat_at = self.sent_at + self.heartbeat_interval;
            tokio::select! {
                biased;
                publication = self.watch.next() => {
                    self.accept(publication.map_err(watch_status)?);
                }
                () = self.clock.sleep_until(heartbeat_at) => {
                    self.tick = self.tick.max(self.watch.latest_tick());
                    return Ok(self.send(watch_resources_response::Event::Heartbeat(Empty {})));
                }
            }
        }
    }

    /// Queues `publication`'s payload once for every key of its kind.
    fn accept(&mut self, publication: Publication) {
        let PublishedRead::Payload(payload) = publication.outcome else {
            return;
        };
        self.tick = self.tick.max(publication.tick);
        self.awaiting.remove(&publication.kind);
        for key in self.keys.get(&publication.kind).into_iter().flatten() {
            self.queued
                .push_back(watch_resources_response::Event::Resource(KeyedResource {
                    key: key.clone(),
                    payload: Some(payload.as_ref().clone()),
                }));
        }
    }

    fn send(&mut self, event: watch_resources_response::Event) -> WatchResourcesResponse {
        self.sent_at = self.clock.now();
        WatchResourcesResponse {
            manager_session_id: self.manager_session_id.clone(),
            tick_sequence: self.tick,
            event: Some(event),
        }
    }
}

fn subscriptions(request: WatchResourcesRequest) -> Result<Vec<(String, ResourceKind)>, Status> {
    let count = request.subscriptions.len();
    if !(1..=MAX_SUBSCRIPTIONS).contains(&count) {
        return Err(Status::invalid_argument(format!(
            "Resource watch carries {count} subscriptions; it takes 1 to {MAX_SUBSCRIPTIONS}"
        )));
    }

    let mut keys = BTreeSet::new();
    request
        .subscriptions
        .into_iter()
        .map(|subscription| {
            if subscription.key.is_empty() {
                return Err(Status::invalid_argument(
                    "Resource watch subscription key is required",
                ));
            }
            if subscription.key.len() > MAX_KEY_BYTES {
                return Err(Status::invalid_argument(format!(
                    "Resource watch subscription key is longer than {MAX_KEY_BYTES} bytes"
                )));
            }
            if !keys.insert(subscription.key.clone()) {
                return Err(Status::invalid_argument(format!(
                    "Resource watch subscription key {:?} is repeated",
                    subscription.key
                )));
            }
            let kind = match subscription.kind {
                Some(resource_subscription::Kind::Machines(_)) => ResourceKind::Machines,
                Some(resource_subscription::Kind::HardwareSnapshots(_)) => {
                    ResourceKind::HardwareSnapshots
                }
                Some(resource_subscription::Kind::ModelDeployments(_)) => {
                    ResourceKind::ModelDeployments
                }
                Some(resource_subscription::Kind::ApplicationDeployments(_)) => {
                    ResourceKind::ApplicationDeployments
                }
                Some(resource_subscription::Kind::Routes(_)) => ResourceKind::Routes,
                Some(resource_subscription::Kind::ExecutionTargets(_)) => {
                    ResourceKind::ExecutionTargets
                }
                Some(resource_subscription::Kind::NodeMemory(_)) => ResourceKind::NodeMemory,
                None => {
                    return Err(Status::invalid_argument(
                        "Resource watch subscription kind is required",
                    ));
                }
            };
            Ok((subscription.key, kind))
        })
        .collect()
}

fn admission_status(error: AdmissionError) -> Status {
    match error {
        AdmissionError::TooManyWatches { .. } => Status::resource_exhausted(error.to_string()),
        AdmissionError::ShutDown => Status::unavailable(error.to_string()),
    }
}

fn watch_status(error: HubWatchError) -> Status {
    match error {
        HubWatchError::ReadFailed { .. } | HubWatchError::ShutDown => {
            Status::unavailable(error.to_string())
        }
        HubWatchError::Lagged => Status::aborted(error.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use dotcore_domain::timing::Timing;
    use std::time::{Duration, UNIX_EPOCH};

    use dotcore_proto::dotcore::{
        inference::v1::ListRoutesRequest,
        site::v1::ListMachinesRequest,
        watch::v1::{ResourceSubscription, keyed_resource::Payload},
    };
    use futures_util::StreamExt;

    use super::{
        hub::test_support::{ALL_KINDS, Harness, START_SECONDS, version},
        *,
    };

    type Events = <ResourceWatchService as ResourceWatchServiceContract>::WatchResourcesStream;

    #[tokio::test]
    async fn a_stream_sends_every_key_its_first_payload_at_once_then_completes_then_changes() {
        let harness = Harness::new(ResourceWatchConfig::new(&Timing::test().watch));
        let service = ResourceWatchService::new(harness.hub.clone());
        let mut events = open(
            &service,
            vec![machines("left"), routes("routes"), machines("right")],
        )
        .await
        .expect("watch");

        let mut first = vec![
            next(&mut events).await,
            next(&mut events).await,
            next(&mut events).await,
        ];
        first.sort();
        assert_eq!(
            first,
            [
                Seen::resource("left", 0, 0),
                Seen::resource("right", 0, 0),
                Seen::resource("routes", 0, 0),
            ]
        );
        assert_eq!(next(&mut events).await, Seen::complete(0));

        harness.set_version(ResourceKind::Routes, 1);
        harness.tick().await;
        assert_eq!(next(&mut events).await, Seen::resource("routes", 1, 1));
    }

    #[tokio::test]
    async fn a_stream_opened_between_ticks_completes_from_the_cached_payloads() {
        let harness = Harness::new(ResourceWatchConfig::new(&Timing::test().watch));
        let service = ResourceWatchService::new(harness.hub.clone());
        let mut first = open(&service, vec![machines("machines")])
            .await
            .expect("watch");
        next(&mut first).await;
        harness.tick().await;
        harness.until_reads(ResourceKind::Machines, 2).await;

        let mut joined = open(&service, vec![machines("machines")])
            .await
            .expect("watch");

        // The payload read on the first open is still the latest; tick 1 found it unchanged.
        assert_eq!(next(&mut joined).await, Seen::resource("machines", 0, 0));
        assert_eq!(next(&mut joined).await, Seen::complete(0));
        assert_eq!(harness.reads(ResourceKind::Machines), 2);
    }

    #[tokio::test]
    async fn an_idle_stream_sends_a_heartbeat_fifteen_seconds_after_its_last_event() {
        let harness = Harness::new(ResourceWatchConfig::new(&Timing::production().watch));
        let service = ResourceWatchService::new(harness.hub.clone());
        let mut events = open(&service, vec![machines("machines")])
            .await
            .expect("watch");
        next(&mut events).await;
        assert_eq!(next(&mut events).await, Seen::complete(0));
        let completed_at = harness.clock.wall_now();

        let heartbeat = {
            let mut pending = events.next();
            loop {
                // The stream is polled before every advance, so it answers at the first tick
                // that reaches its heartbeat.
                tokio::select! {
                    biased;
                    event = &mut pending => break event,
                    () = harness.tick_started() => {}
                }
            }
        }
        .expect("event")
        .expect("heartbeat");

        assert_eq!(
            harness
                .clock
                .wall_now()
                .duration_since(completed_at)
                .expect("later"),
            Timing::production().watch.heartbeat_interval
        );
        assert_eq!(heartbeat.manager_session_id, "a".repeat(32));
        assert!(matches!(
            heartbeat.event,
            Some(watch_resources_response::Event::Heartbeat(_))
        ));
        // Ticks 1 to 15 started by then; tick 16 falls a grace after the heartbeat.
        assert_eq!(heartbeat.tick_sequence, 15);
        assert_eq!(
            completed_at,
            UNIX_EPOCH + Duration::from_secs(START_SECONDS)
        );
    }

    #[tokio::test]
    async fn an_owner_read_failure_ends_the_stream_with_unavailable() {
        let harness = Harness::new(ResourceWatchConfig::new(&Timing::test().watch));
        let service = ResourceWatchService::new(harness.hub.clone());
        let mut events = open(&service, vec![machines("machines"), routes("routes")])
            .await
            .expect("watch");
        harness.fail(ResourceKind::Machines);

        harness.tick().await;
        let mut ended = None;
        while let Some(event) = events.next().await {
            if let Err(status) = event {
                ended = Some(status);
                break;
            }
        }

        let status = ended.expect("stream ends with a status");
        assert_eq!(status.code(), tonic::Code::Unavailable);
        assert_eq!(status.message(), "machines owner read failed");
        assert!(events.next().await.is_none());
    }

    #[tokio::test]
    async fn a_stream_past_the_process_cap_fails_and_a_closed_one_frees_its_place() {
        let harness = Harness::new(ResourceWatchConfig {
            max_watches: 1,
            ..ResourceWatchConfig::new(&Timing::test().watch)
        });
        let service = ResourceWatchService::new(harness.hub.clone());
        let held = open(&service, vec![machines("machines")])
            .await
            .expect("watch");

        let refused = open(&service, vec![machines("machines")]).await;
        assert_eq!(
            refused.err().map(|status| status.code()),
            Some(tonic::Code::ResourceExhausted)
        );
        drop(held);
        assert!(open(&service, vec![machines("machines")]).await.is_ok());
    }

    #[test]
    fn subscription_keys_counts_and_kinds_are_validated() {
        let validate = |subscriptions: Vec<ResourceSubscription>| {
            super::subscriptions(WatchResourcesRequest { subscriptions })
                .map_err(|status| status.code())
        };
        let longest = "k".repeat(MAX_KEY_BYTES);
        let most = (0..MAX_SUBSCRIPTIONS)
            .map(|index| machines(&index.to_string()))
            .collect::<Vec<_>>();

        assert_eq!(
            validate(vec![machines(&longest), routes("routes")]),
            Ok(vec![
                (longest.clone(), ResourceKind::Machines),
                ("routes".to_owned(), ResourceKind::Routes),
            ])
        );
        assert_eq!(validate(most.clone()).map(|valid| valid.len()), Ok(16));

        let invalid = Err(tonic::Code::InvalidArgument);
        assert_eq!(validate(vec![]), invalid);
        assert_eq!(validate(vec![machines("")]), invalid);
        assert_eq!(validate(vec![machines(&format!("{longest}k"))]), invalid);
        assert_eq!(validate(vec![machines("a"), routes("a")]), invalid);
        assert_eq!(
            validate(vec![ResourceSubscription {
                key: "none".to_owned(),
                kind: None,
            }]),
            invalid
        );
        assert_eq!(
            validate(most.into_iter().chain([machines("extra")]).collect()),
            invalid
        );
    }

    #[test]
    fn hub_failures_map_to_explicit_statuses() {
        let code = |status: Status| status.code();

        assert_eq!(
            code(admission_status(AdmissionError::TooManyWatches {
                limit: 1
            })),
            tonic::Code::ResourceExhausted
        );
        assert_eq!(
            code(admission_status(AdmissionError::ShutDown)),
            tonic::Code::Unavailable
        );
        assert_eq!(
            code(watch_status(HubWatchError::ReadFailed {
                kind: ResourceKind::Routes
            })),
            tonic::Code::Unavailable
        );
        assert_eq!(
            code(watch_status(HubWatchError::Lagged)),
            tonic::Code::Aborted
        );
        assert_eq!(
            code(watch_status(HubWatchError::ShutDown)),
            tonic::Code::Unavailable
        );
    }

    #[test]
    fn every_kind_answers_with_its_own_payload() {
        for kind in ALL_KINDS {
            let subscription = ResourceSubscription {
                key: "key".to_owned(),
                kind: Some(match kind {
                    ResourceKind::Machines => {
                        resource_subscription::Kind::Machines(ListMachinesRequest {})
                    }
                    ResourceKind::HardwareSnapshots => {
                        resource_subscription::Kind::HardwareSnapshots(Default::default())
                    }
                    ResourceKind::ModelDeployments => {
                        resource_subscription::Kind::ModelDeployments(Default::default())
                    }
                    ResourceKind::ApplicationDeployments => {
                        resource_subscription::Kind::ApplicationDeployments(Default::default())
                    }
                    ResourceKind::Routes => {
                        resource_subscription::Kind::Routes(ListRoutesRequest {})
                    }
                    ResourceKind::ExecutionTargets => {
                        resource_subscription::Kind::ExecutionTargets(Default::default())
                    }
                    ResourceKind::NodeMemory => {
                        resource_subscription::Kind::NodeMemory(Default::default())
                    }
                }),
            };
            assert_eq!(
                super::subscriptions(WatchResourcesRequest {
                    subscriptions: vec![subscription],
                })
                .expect("valid"),
                [("key".to_owned(), kind)]
            );
        }
    }

    /// One received event: a key's payload version, or a completion, with its tick sequence.
    #[derive(Debug, PartialEq, Eq, PartialOrd, Ord)]
    enum Seen {
        Resource {
            key: String,
            tick: u64,
            version: usize,
        },

        Complete {
            tick: u64,
        },
    }

    impl Seen {
        fn resource(key: &str, tick: u64, version: usize) -> Self {
            Self::Resource {
                key: key.to_owned(),
                tick,
                version,
            }
        }

        fn complete(tick: u64) -> Self {
            Self::Complete { tick }
        }
    }

    async fn open(
        service: &ResourceWatchService,
        subscriptions: Vec<ResourceSubscription>,
    ) -> Result<Events, Status> {
        ResourceWatchServiceContract::watch_resources(
            service,
            Request::new(WatchResourcesRequest { subscriptions }),
        )
        .await
        .map(Response::into_inner)
    }

    async fn next(events: &mut Events) -> Seen {
        let response = tokio::time::timeout(Duration::from_secs(10), events.next())
            .await
            .expect("event within the progress deadline")
            .expect("open stream")
            .expect("event");
        assert_eq!(response.manager_session_id, "a".repeat(32));
        match response.event.expect("event kind") {
            watch_resources_response::Event::Resource(resource) => {
                let payload = resource.payload.expect("payload");
                assert!(
                    matches!(
                        (&payload, resource.key.as_str()),
                        (Payload::Routes(_), "routes") | (Payload::Machines(_), _)
                    ),
                    "payload kind matches its key"
                );
                Seen::resource(&resource.key, response.tick_sequence, version(&payload))
            }
            watch_resources_response::Event::SnapshotComplete(_) => {
                Seen::complete(response.tick_sequence)
            }
            watch_resources_response::Event::Heartbeat(_) => panic!("unexpected heartbeat"),
        }
    }

    fn machines(key: &str) -> ResourceSubscription {
        ResourceSubscription {
            key: key.to_owned(),
            kind: Some(resource_subscription::Kind::Machines(
                ListMachinesRequest {},
            )),
        }
    }

    fn routes(key: &str) -> ResourceSubscription {
        ResourceSubscription {
            key: key.to_owned(),
            kind: Some(resource_subscription::Kind::Routes(ListRoutesRequest {})),
        }
    }
}
