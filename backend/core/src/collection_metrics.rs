//! Records a hosting node's per-workload resource metrics once per hardware collection.
//!
//! Model deployment memory and application service resources come only from the node's full
//! hardware collection, which runs at the site's hardware refresh interval. The writer records
//! them each time the node publishes a collection, stamped with the time it records them, so every
//! collection gives one record per series and the metric store holds each value until the next.
//! It reads the figures the node's status reports, so history never diverges from the current
//! value.

use std::{
    sync::Arc,
    time::{Duration, SystemTime},
};

use dotcore_domain::clock::Clock;
use dotcore_observability::{MetricRecord, MetricRecorder};
use dotcore_site::node::agent::NodeObservationFeed;

/// How late a per-collection record may come after its refresh interval: the time a collection
/// may take beyond the previous one.
pub(crate) const COLLECTION_REPORTING_DELAY: Duration = Duration::from_secs(5);

/// Records the figures of each source once per collection one node publishes.
pub(crate) struct CollectionMetricsWriter {
    observations: NodeObservationFeed,
    clock: Arc<dyn Clock>,
    sources: Vec<CollectionMetricsSource>,
}

impl CollectionMetricsWriter {
    /// Creates the writer of the collections published to `observations`, stamping records with
    /// `clock`'s wall time.
    pub(crate) fn new(
        observations: NodeObservationFeed,
        sources: Vec<CollectionMetricsSource>,
        clock: Arc<dyn Clock>,
    ) -> Self {
        Self {
            observations,
            clock,
            sources,
        }
    }

    /// Records the collection already published, if any, then each newer one as it is published,
    /// until dropped. Collections published while it records count as one: the latest.
    pub(crate) async fn run(self) {
        let mut published = self.observations.published_revisions();
        let mut recorded = 0;
        loop {
            let revision = *published.borrow_and_update();
            if revision > recorded {
                self.record();
                recorded = revision;
            }
            // The writer holds the feed, so its announcements never end.
            if published.changed().await.is_err() {
                return;
            }
        }
    }

    /// Records every source's current figures, stamped now; a source with none records nothing.
    fn record(&self) {
        let at = self.clock.wall_now();
        for source in &self.sources {
            let records = (source.records)(at);
            if !records.is_empty() {
                source.recorder.record(records);
            }
        }
    }
}

/// The records of one kind of hosted workload and the recorder they go through.
pub(crate) struct CollectionMetricsSource {
    records: Box<dyn Fn(SystemTime) -> Vec<MetricRecord> + Send + Sync>,
    recorder: Arc<dyn MetricRecorder>,
}

impl CollectionMetricsSource {
    /// A source recording what `records` returns for a time through `recorder`.
    pub(crate) fn new(
        records: impl Fn(SystemTime) -> Vec<MetricRecord> + Send + Sync + 'static,
        recorder: Arc<dyn MetricRecorder>,
    ) -> Self {
        Self {
            records: Box::new(records),
            recorder,
        }
    }
}

#[cfg(test)]
mod tests {
    use std::{
        collections::BTreeMap,
        sync::Mutex,
        time::{Duration, UNIX_EPOCH},
    };

    use dotcore_domain::{clock::VirtualClock, timing::Timing};
    use dotcore_observability::{
        ForwarderConfig, InMemoryMetricStore, InMemoryStoreConfig, InProcessBucketTransport,
        MetricBucketIngest, MetricForwarder, MetricQuery, MetricStore, PointValue, QueryRange,
        Reduce, SeriesAggregate, forwarder_metric_descriptors, labels::LabelSet,
        test_support::CapturingRecorder,
    };
    use dotcore_proto::dotcore::site::private::v1::NodeObservationSnapshot;
    use dotcore_site::node::agent::NodeCollection;

    use super::*;
    use crate::model::controller::metrics::{
        DEPLOYMENT_ACCELERATOR_MEMORY_USED, deployment_metric_descriptors,
    };

    #[tokio::test]
    async fn each_publish_records_every_source_once_stamped_when_recorded() {
        let harness = Harness::new();
        let _writer = harness.start();

        harness.publish(1);
        let first = harness.models.next_batch().await;
        assert_eq!(first, vec![harness.record("model-a", 0, 1.0)]);
        assert_eq!(
            harness.apps.next_batch().await,
            vec![harness.record("app-a", 0, 1.0)]
        );

        // Time passing without a publish records nothing; the next publish records once more.
        harness.clock.advance(Duration::from_secs(4));
        harness.set_value(2.0);
        harness.publish(2);
        assert_eq!(
            harness.models.next_batch().await,
            vec![harness.record("model-a", 4, 2.0)]
        );
        assert_eq!(
            harness.apps.next_batch().await,
            vec![harness.record("app-a", 4, 2.0)]
        );
        assert_eq!(harness.models.try_next_batch(), None);
        assert_eq!(harness.apps.try_next_batch(), None);
    }

    #[tokio::test]
    async fn collections_published_before_the_writer_starts_record_once() {
        let harness = Harness::new();
        harness.publish(1);
        harness.publish(2);

        let _writer = harness.start();

        assert_eq!(
            harness.models.next_batch().await,
            vec![harness.record("model-a", 0, 1.0)]
        );
        // A later publish proves the writer handled its start before this check.
        harness.publish(3);
        assert_eq!(
            harness.models.next_batch().await,
            vec![harness.record("model-a", 0, 1.0)]
        );
        assert_eq!(harness.models.try_next_batch(), None);
    }

    #[tokio::test]
    async fn a_source_without_figures_records_nothing() {
        let harness = Harness::new();
        harness.hide_apps();
        let _writer = harness.start();

        harness.publish(1);
        assert_eq!(harness.models.next_batch().await.len(), 1);
        harness.publish(2);
        assert_eq!(harness.models.next_batch().await.len(), 1);

        assert_eq!(harness.apps.records(), vec![]);
    }

    #[tokio::test]
    async fn a_slow_collection_still_reaches_the_store_through_the_forwarder() {
        let clock = VirtualClock::starting_at(UNIX_EPOCH + Duration::from_secs(EPOCH_SECONDS));
        let store = InMemoryMetricStore::start(
            InMemoryStoreConfig::default(),
            [
                forwarder_metric_descriptors(),
                deployment_metric_descriptors(Duration::from_secs(5)),
            ]
            .concat(),
            Timing::test().metrics,
            Timing::test().watch,
            Arc::new(clock.clone()),
        )
        .expect("start metric store");
        let ingest = Arc::new(MetricBucketIngest::new(
            store.ingestor(),
            Timing::test().metrics,
            Arc::new(clock.clone()),
        ));
        let (forwarder, _forwarder_task) = MetricForwarder::start(
            "node-a",
            Arc::new(InProcessBucketTransport::new(ingest)),
            Arc::new(clock.clone()),
            ForwarderConfig::default(),
        )
        .expect("start metric forwarder");
        let feed = NodeObservationFeed::default();
        let _writer = tokio::spawn(
            CollectionMetricsWriter::new(
                feed.clone(),
                vec![CollectionMetricsSource::new(
                    |at| {
                        vec![MetricRecord::gauge(
                            DEPLOYMENT_ACCELERATOR_MEMORY_USED,
                            deployment_labels("model-a"),
                            at,
                            7.0,
                        )]
                    },
                    forwarder.recorder("deployment_memory"),
                )],
                Arc::new(clock.clone()),
            )
            .run(),
        );
        // Another writer records every second, so the forwarder sends each bucket as it closes.
        let busy = forwarder.recorder("busy");
        let tick = |clock: &VirtualClock| {
            busy.record(vec![MetricRecord::gauge(
                DEPLOYMENT_ACCELERATOR_MEMORY_USED,
                deployment_labels("busy"),
                clock.wall_now(),
                1.0,
            )]);
        };

        // The collection starts at second 0 and takes three seconds, past the forwarder's grace
        // for the buckets it started in.
        let collection_start = clock.wall_now();
        for _ in 0..12 {
            tick(&clock);
            clock.advance(Duration::from_millis(250));
            tokio::task::yield_now().await;
        }
        feed.publish(
            NodeCollection::without_processes(NodeObservationSnapshot::default()),
            1,
            collection_start,
        );

        let points = loop {
            tick(&clock);
            clock.advance(Duration::from_millis(250));
            tokio::time::sleep(Duration::from_millis(5)).await;
            let result = store
                .query(MetricQuery {
                    metric: DEPLOYMENT_ACCELERATOR_MEMORY_USED.to_owned(),
                    matchers: BTreeMap::from([("deployment".to_owned(), "model-a".to_owned())]),
                    group_by: vec!["deployment".to_owned()],
                    reduce: Some(Reduce::Last),
                    aggregate: Some(SeriesAggregate::Sum),
                    percentiles: vec![],
                    range: QueryRange::Last(Duration::from_secs(60)),
                    step_hint: Duration::from_secs(1),
                })
                .await
                .expect("deployment memory query");
            if let Some(series) = result.series.into_iter().next()
                && !series.points.is_empty()
            {
                break series.points;
            }
        };

        assert!(
            points
                .iter()
                .all(|point| point.value == PointValue::Gauge(7.0)),
            "every point holds the recorded value: {points:?}"
        );
        assert_eq!(forwarder.stale_records(), 0);
    }

    /// Whole seconds after the Unix epoch the virtual wall clock counts from.
    const EPOCH_SECONDS: u64 = 1_700_000_000;

    /// A writer over one model source and one app source whose value the test sets, recording
    /// into capturing recorders on a virtual clock that starts at second 0.
    struct Harness {
        clock: VirtualClock,
        feed: NodeObservationFeed,
        value: Arc<Mutex<f64>>,
        apps_hidden: Arc<Mutex<bool>>,
        models: Arc<CapturingRecorder>,
        apps: Arc<CapturingRecorder>,
    }

    impl Harness {
        fn new() -> Self {
            Self {
                clock: VirtualClock::starting_at(UNIX_EPOCH + Duration::from_secs(EPOCH_SECONDS)),
                feed: NodeObservationFeed::default(),
                value: Arc::new(Mutex::new(1.0)),
                apps_hidden: Arc::new(Mutex::new(false)),
                models: Arc::new(CapturingRecorder::default()),
                apps: Arc::new(CapturingRecorder::default()),
            }
        }

        fn start(&self) -> AbortOnDrop {
            let model_value = Arc::clone(&self.value);
            let app_value = Arc::clone(&self.value);
            let apps_hidden = Arc::clone(&self.apps_hidden);
            let writer = CollectionMetricsWriter::new(
                self.feed.clone(),
                vec![
                    CollectionMetricsSource::new(
                        move |at| vec![gauge("model-a", at, *model_value.lock().expect("value"))],
                        Arc::clone(&self.models) as Arc<dyn MetricRecorder>,
                    ),
                    CollectionMetricsSource::new(
                        move |at| {
                            if *apps_hidden.lock().expect("apps hidden") {
                                return vec![];
                            }
                            vec![gauge("app-a", at, *app_value.lock().expect("value"))]
                        },
                        Arc::clone(&self.apps) as Arc<dyn MetricRecorder>,
                    ),
                ],
                Arc::new(self.clock.clone()),
            );
            AbortOnDrop(tokio::spawn(writer.run()))
        }

        fn publish(&self, revision: u64) {
            self.feed.publish(
                NodeCollection::without_processes(NodeObservationSnapshot::default()),
                revision,
                self.clock.wall_now(),
            );
        }

        fn set_value(&self, value: f64) {
            *self.value.lock().expect("value") = value;
        }

        fn hide_apps(&self) {
            *self.apps_hidden.lock().expect("apps hidden") = true;
        }

        /// The record of `value` for `deployment` at second `second`.
        fn record(&self, deployment: &str, second: u64, value: f64) -> MetricRecord {
            gauge(
                deployment,
                UNIX_EPOCH + Duration::from_secs(EPOCH_SECONDS + second),
                value,
            )
        }
    }

    /// Aborts the writer task when the test ends.
    struct AbortOnDrop(tokio::task::JoinHandle<()>);

    impl Drop for AbortOnDrop {
        fn drop(&mut self) {
            self.0.abort();
        }
    }

    fn gauge(deployment: &str, at: SystemTime, value: f64) -> MetricRecord {
        MetricRecord::gauge(
            DEPLOYMENT_ACCELERATOR_MEMORY_USED,
            deployment_labels(deployment),
            at,
            value,
        )
    }

    fn deployment_labels(deployment: &str) -> LabelSet {
        LabelSet::new()
            .with("deployment", deployment)
            .with("node", "node-a")
    }
}
