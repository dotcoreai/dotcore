//! Carries node metric bucket streams over the private mutual-TLS transport: the site manager's
//! receiving service and a worker's client.
//!
//! The service accepts a stream only from the authenticated worker it names and hands its batches
//! to observability's ingest; ordering, deduplication and acknowledgement belong to that ingest.
//! The client opens each stream on one reused channel to the manager. Malformed messages end the
//! stream with an explicit status rather than being skipped.

use std::{pin::Pin, sync::Arc};

use dotcore_observability::{
    Aggregate, BucketBatch, BucketSeries, BucketStreamConnection, BucketStreamEnd,
    BucketStreamError, BucketStreamOpen, CounterAggregate, GaugeAggregate, MetricBucket,
    MetricBucketIngest, MetricBucketTransport, labels::LabelSet,
};
use dotcore_proto::dotcore::metrics::{
    private::v1::{
        self as proto, StreamMetricBucketsRequest, StreamMetricBucketsResponse,
        metric_bucket_stream_service_client::MetricBucketStreamServiceClient,
        metric_bucket_stream_service_server::{
            MetricBucketStreamService as MetricBucketStreamContract,
            MetricBucketStreamServiceServer,
        },
        stream_metric_buckets_request,
    },
    v1::{self as metrics, metric_series_aggregate},
};
use dotcore_site::private_transport::{
    AuthenticatedRoleAuthorization, PrivateNodeRole, PrivateServiceContext,
};
use futures_util::{Stream, StreamExt, future::BoxFuture, stream};
use tokio::sync::mpsc;
use tonic::{
    Request, Response, Status, Streaming,
    transport::{Channel, Endpoint},
};

use crate::metrics::{duration, std_duration, system_time, timestamp};

/// Batches a worker queues toward its stream before the forwarder waits.
const OUTBOUND_QUEUE: usize = 16;

/// Manager-hosted receiver of worker metric bucket streams.
pub(crate) struct MetricBucketStreamService {
    ingest: Arc<MetricBucketIngest>,
    authorization: AuthenticatedRoleAuthorization,
}

impl MetricBucketStreamService {
    /// Creates a receiver authorized for worker callers, writing into `ingest`.
    pub(crate) fn new(ingest: Arc<MetricBucketIngest>, context: &PrivateServiceContext) -> Self {
        Self {
            ingest,
            authorization: context.authorize_role(PrivateNodeRole::Worker),
        }
    }

    /// Wraps the service for route assembly.
    pub(crate) fn into_server(self) -> MetricBucketStreamServiceServer<Self> {
        MetricBucketStreamServiceServer::new(self)
    }
}

#[tonic::async_trait]
impl MetricBucketStreamContract for MetricBucketStreamService {
    type StreamMetricBucketsStream =
        Pin<Box<dyn Stream<Item = Result<StreamMetricBucketsResponse, Status>> + Send>>;

    async fn stream_metric_buckets(
        &self,
        request: Request<Streaming<StreamMetricBucketsRequest>>,
    ) -> Result<Response<Self::StreamMetricBucketsStream>, Status> {
        if self.authorization.receiver_role() != PrivateNodeRole::Manager {
            return Err(Status::permission_denied(
                "private operation is not authorized",
            ));
        }
        let identity = self.authorization.authorize(&request)?;
        let mut inbound = request.into_inner();
        let first = self
            .ingest
            .first_message(inbound.message())
            .await
            .ok_or_else(|| {
                Status::deadline_exceeded("metric bucket stream sent no open in time")
            })??;
        let open = match first {
            Some(StreamMetricBucketsRequest {
                message: Some(stream_metric_buckets_request::Message::Open(open)),
            }) => open,
            _ => {
                return Err(Status::invalid_argument(
                    "metric bucket stream must start with open",
                ));
            }
        };
        if open.site_id != identity.site_id || open.node_id != identity.node_id {
            return Err(Status::permission_denied(
                "metric bucket stream is not authorized for this node",
            ));
        }
        if open.session_id.is_empty() {
            return Err(Status::invalid_argument(
                "metric bucket stream session is required",
            ));
        }

        let batches = inbound
            .map(|message| match message {
                Ok(StreamMetricBucketsRequest {
                    message: Some(stream_metric_buckets_request::Message::Batch(batch)),
                }) => batch_from_proto(batch).ok_or(BucketStreamEnd::Malformed),
                Ok(_) => Err(BucketStreamEnd::Malformed),
                Err(_) => Err(BucketStreamEnd::Interrupted),
            })
            .boxed();
        let applied = self.ingest.serve(
            BucketStreamOpen {
                node_id: open.node_id,
                session_id: open.session_id,
            },
            batches,
        );
        Ok(Response::new(Box::pin(applied.map(|applied| {
            applied
                .map(|applied_sequence| StreamMetricBucketsResponse { applied_sequence })
                .map_err(end_status)
        }))))
    }
}

/// A worker's metric bucket transport to the site manager over one reused private channel, which
/// connects on first use and reconnects on a later stream after a lost connection.
pub(crate) struct ManagerMetricBucketClient {
    client: MetricBucketStreamServiceClient<Channel>,
    site_id: String,
}

impl ManagerMetricBucketClient {
    /// Creates a client of site `site_id`'s manager reached through `endpoint`.
    pub(crate) fn new(endpoint: Endpoint, site_id: String) -> Self {
        Self {
            client: MetricBucketStreamServiceClient::new(endpoint.connect_lazy()),
            site_id,
        }
    }
}

impl MetricBucketTransport for ManagerMetricBucketClient {
    fn open(
        &self,
        open: BucketStreamOpen,
    ) -> BoxFuture<'_, Result<BucketStreamConnection, BucketStreamError>> {
        Box::pin(async move {
            let (batches, receiver) = mpsc::channel(OUTBOUND_QUEUE);
            let open = StreamMetricBucketsRequest {
                message: Some(stream_metric_buckets_request::Message::Open(
                    proto::MetricBucketStreamOpen {
                        site_id: self.site_id.clone(),
                        node_id: open.node_id,
                        session_id: open.session_id,
                    },
                )),
            };
            let outbound = stream::once(async { open }).chain(
                tokio_stream::wrappers::ReceiverStream::new(receiver).map(|batch| {
                    StreamMetricBucketsRequest {
                        message: Some(stream_metric_buckets_request::Message::Batch(
                            batch_to_proto(batch),
                        )),
                    }
                }),
            );
            let applied = self
                .client
                .clone()
                .stream_metric_buckets(outbound)
                .await
                .map_err(BucketStreamError::new)?
                .into_inner()
                .map(|response| {
                    response
                        .map(|response| response.applied_sequence)
                        .map_err(BucketStreamError::new)
                })
                .boxed();
            Ok(BucketStreamConnection { batches, applied })
        })
    }
}

fn end_status(end: BucketStreamEnd) -> Status {
    let message = end.to_string();
    match end {
        BucketStreamEnd::Replaced => Status::aborted(message),
        BucketStreamEnd::Idle => Status::deadline_exceeded(message),
        BucketStreamEnd::StoreStopped => Status::unavailable(message),
        BucketStreamEnd::OutOfOrder
        | BucketStreamEnd::MisalignedBucket
        | BucketStreamEnd::Malformed => Status::invalid_argument(message),
        BucketStreamEnd::Interrupted => Status::cancelled(message),
    }
}

fn batch_to_proto(batch: BucketBatch) -> proto::MetricBucketBatch {
    proto::MetricBucketBatch {
        sequence: batch.sequence,
        buckets: batch
            .buckets
            .into_iter()
            .map(|bucket| metrics::MetricBucketAggregate {
                bucket_start: Some(timestamp(bucket.start)),
                width: Some(duration(bucket.width)),
                series: bucket
                    .series
                    .into_iter()
                    .filter_map(series_to_proto)
                    .collect(),
            })
            .collect(),
        backfill: batch.backfill,
    }
}

/// The wire form of one series summary; `None` for kinds the wire does not carry yet.
fn series_to_proto(series: BucketSeries) -> Option<metrics::MetricSeriesAggregate> {
    let aggregate = match series.aggregate {
        Aggregate::Gauge(gauge) => {
            metric_series_aggregate::Aggregate::Gauge(metrics::MetricGaugeAggregate {
                count: gauge.count(),
                sum: gauge.sum(),
                min: gauge.min(),
                max: gauge.max(),
                last: gauge.last(),
                last_observed_at: Some(timestamp(gauge.last_observed_at())),
            })
        }
        Aggregate::Counter(counter) => {
            metric_series_aggregate::Aggregate::Counter(metrics::MetricCounterAggregate {
                delta: counter.delta(),
            })
        }
        // Forwarders never summarize distributions; they gain a wire form with their first node
        // producer.
        Aggregate::Distribution(_) => return None,
    };
    Some(metrics::MetricSeriesAggregate {
        metric: series.metric,
        labels: series
            .labels
            .iter()
            .map(|(name, value)| (name.to_owned(), value.to_owned()))
            .collect(),
        aggregate: Some(aggregate),
    })
}

/// The batch a wire message describes; `None` when a required field is missing or a summary is
/// inconsistent.
fn batch_from_proto(batch: proto::MetricBucketBatch) -> Option<BucketBatch> {
    let buckets = batch
        .buckets
        .into_iter()
        .map(|bucket| {
            Some(MetricBucket {
                start: system_time(&bucket.bucket_start?)?,
                width: std_duration(&bucket.width?)?,
                series: bucket
                    .series
                    .into_iter()
                    .map(series_from_proto)
                    .collect::<Option<_>>()?,
            })
        })
        .collect::<Option<_>>()?;
    Some(BucketBatch {
        sequence: batch.sequence,
        buckets,
        backfill: batch.backfill,
    })
}

fn series_from_proto(series: metrics::MetricSeriesAggregate) -> Option<BucketSeries> {
    let aggregate = match series.aggregate? {
        metric_series_aggregate::Aggregate::Gauge(gauge) => {
            Aggregate::Gauge(GaugeAggregate::from_parts(
                gauge.count,
                gauge.sum,
                gauge.min,
                gauge.max,
                gauge.last,
                system_time(&gauge.last_observed_at?)?,
            )?)
        }
        metric_series_aggregate::Aggregate::Counter(counter) => {
            if !counter.delta.is_finite() {
                return None;
            }
            Aggregate::Counter(CounterAggregate::from_delta(counter.delta))
        }
    };
    Some(BucketSeries {
        metric: series.metric,
        labels: series.labels.into_iter().collect::<LabelSet>(),
        aggregate,
    })
}

#[cfg(test)]
mod tests {
    use std::{
        collections::BTreeMap,
        error::Error as _,
        net::SocketAddr,
        time::{Duration, SystemTime},
    };

    use dotcore_domain::clock::{Clock as _, ManualClock};
    use dotcore_observability::{
        InMemoryMetricStore, InMemoryStoreConfig, MetricDescriptor, MetricDimension, MetricKind,
        MetricProvenance, MetricQuery, MetricStore, MetricUnit, PointValue, QueryRange, Reduce,
        SeriesAggregate, aligned_bucket_start,
    };
    use dotcore_site::{
        node::folder::CertificateFingerprint,
        private_transport::{PinnedPeerVerifier, pem},
    };
    use tonic::transport::{ClientTlsConfig, Identity};

    use super::*;
    use crate::{
        server::private::PrivateServiceAssembly, test_support::private_tls::TestAuthority,
    };

    const SITE_ID: &str = "site-one";
    const MANAGER_ID: &str = "manager-one";
    const WORKER_ID: &str = "worker-one";

    #[test]
    fn gauge_and_counter_summaries_and_the_backfill_flag_cross_the_wire_unchanged() {
        let start = aligned_bucket_start(SystemTime::now(), Duration::from_secs(1));
        let mut bucket = memory_bucket(start, 4.0);
        bucket.series.push(BucketSeries {
            metric: "observability.forwarder.dropped".to_owned(),
            labels: LabelSet::new()
                .with("node", "node-a")
                .with("reason", "stale"),
            aggregate: Aggregate::Counter(CounterAggregate::from_delta(3.0)),
        });
        let sent = BucketBatch {
            backfill: true,
            ..batch(7, bucket)
        };

        assert_eq!(batch_from_proto(batch_to_proto(sent.clone())), Some(sent));
    }

    #[test]
    fn a_counter_summary_that_is_not_finite_is_malformed() {
        let start = aligned_bucket_start(SystemTime::now(), Duration::from_secs(1));
        let mut wire = batch_to_proto(batch(1, memory_bucket(start, 4.0)));
        wire.buckets[0].series[0].aggregate = Some(metric_series_aggregate::Aggregate::Counter(
            metrics::MetricCounterAggregate { delta: f64::NAN },
        ));

        assert_eq!(batch_from_proto(wire), None);
    }

    #[tokio::test]
    async fn a_worker_streams_buckets_over_mtls_and_a_reconnect_replaces_its_stream() {
        let fixture = Fixture::start().await;
        let client = fixture.client();
        let bucket = aligned_bucket_start(fixture.clock.wall_now(), Duration::from_secs(1))
            - Duration::from_secs(5);
        let later = bucket + Duration::from_secs(1);
        let first = batch(1, memory_bucket(bucket, 4.0));

        let mut lost = client.open(open(WORKER_ID)).await.expect("open stream");
        lost.batches.send(first.clone()).await.expect("send batch");
        assert_eq!(applied(&mut lost).await.expect("applied"), 1);

        let mut stream = client.open(open(WORKER_ID)).await.expect("reopen stream");
        let replaced = applied(&mut lost).await.expect_err("replaced stream ends");
        assert_eq!(status_code(&replaced), tonic::Code::Aborted);
        stream.batches.send(first).await.expect("resend batch");
        stream
            .batches
            .send(batch(2, memory_bucket(later, 8.0)))
            .await
            .expect("send batch");

        assert_eq!(applied(&mut stream).await.expect("applied"), 1);
        assert_eq!(applied(&mut stream).await.expect("applied"), 2);
        assert_eq!(fixture.memory(bucket).await, Some(PointValue::Gauge(4.0)));
        assert_eq!(fixture.memory(later).await, Some(PointValue::Gauge(8.0)));
    }

    #[tokio::test]
    async fn a_stream_naming_another_node_is_refused() {
        let fixture = Fixture::start().await;

        let Err(error) = fixture.client().open(open("worker-two")).await else {
            panic!("a stream for another node must be refused");
        };

        assert_eq!(status_code(&error), tonic::Code::PermissionDenied);
    }

    #[tokio::test]
    async fn a_malformed_batch_ends_the_stream() {
        let fixture = Fixture::start().await;
        let mut stream = fixture
            .client()
            .open(open(WORKER_ID))
            .await
            .expect("open stream");
        let bucket = aligned_bucket_start(fixture.clock.wall_now(), Duration::from_secs(1));

        stream
            .batches
            .send(batch(
                1,
                memory_bucket(bucket + Duration::from_millis(500), 1.0),
            ))
            .await
            .expect("send batch");

        let error = applied(&mut stream).await.expect_err("stream ends");
        assert_eq!(status_code(&error), tonic::Code::InvalidArgument);
    }

    /// A manager private listener serving the metric bucket stream into a store, and the
    /// credentials of one worker allowed to call it.
    struct Fixture {
        address: SocketAddr,
        authority: TestAuthority,
        manager_certificate: Vec<u8>,
        worker: (Vec<u8>, Vec<u8>),
        store: Arc<InMemoryMetricStore>,
        clock: ManualClock,
    }

    impl Fixture {
        async fn start() -> Self {
            let authority = TestAuthority::new();
            let manager = authority.issue(SITE_ID, MANAGER_ID, true);
            let worker = authority.issue(SITE_ID, WORKER_ID, false);
            let clock = ManualClock::new();
            let store = Arc::new(
                InMemoryMetricStore::start(
                    InMemoryStoreConfig::default(),
                    vec![MetricDescriptor {
                        name: "node.memory.used".to_owned(),
                        kind: MetricKind::Gauge,
                        unit: MetricUnit::Bytes,
                        dimensions: vec![MetricDimension::required("node")],
                        provenance: MetricProvenance::Node,
                        sample_interval: Some(Duration::from_secs(1)),
                        reporting_delay: None,
                    }],
                    Arc::new(clock.clone()),
                )
                .expect("start store"),
            );
            // Leaves history after the store start for buckets to land in.
            clock.advance(Duration::from_secs(60));
            let ingest = Arc::new(MetricBucketIngest::new(
                store.ingestor(),
                Arc::new(clock.clone()),
            ));
            let manager_certificate = manager.0.clone();
            let server = crate::server::private::prepare(
                authority.private_configuration(
                    SITE_ID,
                    MANAGER_ID,
                    PrivateNodeRole::Manager,
                    manager,
                    [(WORKER_ID, PrivateNodeRole::Worker, worker.0.as_slice())],
                ),
                PrivateServiceAssembly::new(move |context, routes| {
                    routes.add_service(
                        MetricBucketStreamService::new(ingest, &context).into_server(),
                    );
                })
                .assemble(PrivateNodeRole::Manager, MANAGER_ID),
                std::future::pending(),
            )
            .await
            .expect("manager private listener");
            let address = server.address;
            tokio::spawn(server.serve());
            Self {
                address,
                authority,
                manager_certificate,
                worker,
                store,
                clock,
            }
        }

        /// The worker's client, pinned to the manager's certificate.
        fn client(&self) -> ManagerMetricBucketClient {
            let verifier = Arc::new(
                PinnedPeerVerifier::new(
                    self.authority.certificate.der(),
                    SITE_ID,
                    MANAGER_ID,
                    CertificateFingerprint::for_certificate(&self.manager_certificate),
                )
                .expect("pinned manager verifier"),
            );
            let endpoint = Endpoint::from_shared(format!("https://{}", self.address))
                .expect("manager endpoint")
                .tls_config_with_verifier(
                    ClientTlsConfig::new()
                        .domain_name("127.0.0.1")
                        .identity(Identity::from_pem(
                            pem("CERTIFICATE", &self.worker.0),
                            pem("PRIVATE KEY", &self.worker.1),
                        ))
                        .assume_http2(true),
                    verifier,
                )
                .expect("worker TLS");
            ManagerMetricBucketClient::new(endpoint, SITE_ID.to_owned())
        }

        /// The summed memory reading of the one-second bucket starting at `bucket`.
        async fn memory(&self, bucket: SystemTime) -> Option<PointValue> {
            let result = self
                .store
                .query(MetricQuery {
                    metric: "node.memory.used".to_owned(),
                    matchers: BTreeMap::new(),
                    group_by: vec![],
                    reduce: Some(Reduce::Sum),
                    aggregate: Some(SeriesAggregate::Sum),
                    percentiles: vec![],
                    range: QueryRange::Between {
                        start: bucket,
                        end: bucket + Duration::from_secs(1),
                    },
                    step_hint: Duration::from_secs(1),
                })
                .await
                .expect("query");
            let series = result.series.into_iter().next()?;
            series
                .points
                .into_iter()
                .find(|point| point.bucket_start == bucket)
                .map(|point| point.value)
        }
    }

    fn open(node_id: &str) -> BucketStreamOpen {
        BucketStreamOpen {
            node_id: node_id.to_owned(),
            session_id: "session-a".to_owned(),
        }
    }

    fn batch(sequence: u64, bucket: MetricBucket) -> BucketBatch {
        BucketBatch {
            sequence,
            buckets: vec![bucket],
            backfill: false,
        }
    }

    fn memory_bucket(start: SystemTime, value: f64) -> MetricBucket {
        MetricBucket {
            start,
            width: Duration::from_secs(1),
            series: vec![BucketSeries {
                metric: "node.memory.used".to_owned(),
                labels: LabelSet::new().with("node", WORKER_ID),
                aggregate: Aggregate::Gauge(GaugeAggregate::from_sample(
                    value,
                    start + Duration::from_millis(100),
                )),
            }],
        }
    }

    async fn applied(connection: &mut BucketStreamConnection) -> Result<u64, BucketStreamError> {
        tokio::time::timeout(Duration::from_secs(10), connection.applied.next())
            .await
            .expect("acknowledgement deadline")
            .expect("stream open")
    }

    fn status_code(error: &BucketStreamError) -> tonic::Code {
        error
            .source()
            .and_then(|source| source.downcast_ref::<Status>())
            .expect("transport status")
            .code()
    }
}
