//! Serves the management metrics API over the site's metric store: the declared metrics, the
//! resolutions history is kept at, aligned history queries, and live watches carrying several of
//! those queries on one stream.
//!
//! The service only translates between the public contract and the store; retention, alignment,
//! gap, and closing rules belong to the store. Invalid queries fail with an explicit status rather
//! than an empty answer, and a watch ends with one as soon as any of its queries cannot continue.

use std::{
    collections::{BTreeMap, BTreeSet},
    pin::Pin,
    sync::Arc,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use dotcore_observability::{
    MetricDescriptor, MetricKind, MetricProvenance, MetricQuery, MetricStore, MetricUnit,
    MetricWatch, PointValue, QueryError, QueryRange, QueryResult, Reduce, SeriesAggregate,
    WatchError, WatchEvent,
};
use dotcore_proto::{
    dotcore::metrics::v1::{
        self as proto, ListMetricDescriptorsRequest, ListMetricDescriptorsResponse,
        QueryMetricsRequest, QueryMetricsResponse, WatchMetricsRequest, WatchMetricsResponse,
        metric_point, metric_query,
        metrics_service_server::{MetricsService as MetricsServiceContract, MetricsServiceServer},
        watch_metrics_response,
    },
    google::protobuf::{Duration as ProtoDuration, Empty, Timestamp},
};
use futures_util::{
    Stream, StreamExt,
    stream::{self, BoxStream, SelectAll},
};
use tonic::{Request, Response, Status};

/// Public management `MetricsService` over one metric store.
#[derive(Clone)]
pub(crate) struct MetricsService {
    store: Arc<dyn MetricStore>,
}

impl MetricsService {
    /// Creates the service reading `store`.
    pub(crate) fn new(store: Arc<dyn MetricStore>) -> Self {
        Self { store }
    }

    /// Wraps the service for route assembly.
    pub(crate) fn into_server(self) -> MetricsServiceServer<Self> {
        MetricsServiceServer::new(self)
    }
}

#[tonic::async_trait]
impl MetricsServiceContract for MetricsService {
    async fn list_metric_descriptors(
        &self,
        _request: Request<ListMetricDescriptorsRequest>,
    ) -> Result<Response<ListMetricDescriptorsResponse>, Status> {
        Ok(Response::new(ListMetricDescriptorsResponse {
            descriptors: self.store.descriptors().iter().map(descriptor).collect(),
            resolutions: self
                .store
                .retention()
                .tiers()
                .iter()
                .map(|tier| proto::MetricResolution {
                    step: Some(duration(tier.width)),
                    retention: Some(duration(tier.retention)),
                })
                .collect(),
        }))
    }

    async fn query_metrics(
        &self,
        request: Request<QueryMetricsRequest>,
    ) -> Result<Response<QueryMetricsResponse>, Status> {
        let query = request
            .into_inner()
            .query
            .ok_or_else(|| Status::invalid_argument("Metric query is required"))?;
        let result = self
            .store
            .query(metric_query(query)?)
            .await
            .map_err(query_status)?;
        Ok(Response::new(QueryMetricsResponse {
            result: Some(query_result(result)),
        }))
    }

    type WatchMetricsStream =
        Pin<Box<dyn Stream<Item = Result<WatchMetricsResponse, Status>> + Send>>;

    async fn watch_metrics(
        &self,
        request: Request<WatchMetricsRequest>,
    ) -> Result<Response<Self::WatchMetricsStream>, Status> {
        let queries = watch_queries(request.into_inner())?;
        let watch = KeyedWatch {
            store: Arc::clone(&self.store),
            pending: queries.into_iter(),
            opening: None,
            completed: false,
            live: SelectAll::new(),
        };
        Ok(Response::new(Box::pin(stream::unfold(
            Some(watch),
            |watch| async move {
                let mut watch = watch?;
                match watch.next().await? {
                    Ok(event) => {
                        Some((Ok(WatchMetricsResponse { event: Some(event) }), Some(watch)))
                    }
                    // Dropping the watch closes every query's store watch and frees its permit.
                    Err(status) => Some((Err(status), None)),
                }
            },
        ))))
    }
}

pub(crate) fn duration(duration: Duration) -> ProtoDuration {
    ProtoDuration {
        seconds: duration.as_secs().try_into().unwrap_or(i64::MAX),
        nanos: duration.subsec_nanos() as i32,
    }
}

pub(crate) fn std_duration(duration: &ProtoDuration) -> Option<Duration> {
    let seconds = u64::try_from(duration.seconds).ok()?;
    let nanos = u32::try_from(duration.nanos)
        .ok()
        .filter(|nanos| *nanos < 1_000_000_000)?;
    Some(Duration::new(seconds, nanos))
}

pub(crate) fn timestamp(time: SystemTime) -> Timestamp {
    let since = time.duration_since(UNIX_EPOCH).unwrap_or_default();
    Timestamp {
        seconds: since.as_secs().try_into().unwrap_or(i64::MAX),
        nanos: since.subsec_nanos() as i32,
    }
}

pub(crate) fn system_time(timestamp: &Timestamp) -> Option<SystemTime> {
    let seconds = u64::try_from(timestamp.seconds).ok()?;
    let nanos = u32::try_from(timestamp.nanos)
        .ok()
        .filter(|nanos| *nanos < 1_000_000_000)?;
    UNIX_EPOCH.checked_add(Duration::new(seconds, nanos))
}

/// Most queries one watch stream carries.
const MAX_WATCH_QUERIES: usize = 128;

/// Longest key naming a watched query, in bytes.
const MAX_WATCH_KEY_BYTES: usize = 64;

/// Store watch of one query, with its events tagged by the query's key.
type KeyedEvents = BoxStream<'static, (String, Result<WatchEvent, WatchError>)>;

/// The keyed queries of one watch stream as they move from backfill to live.
///
/// Queries open one at a time in request order, each sending its backfill before the next opens,
/// so only one backfill is held at once. One completion follows the last backfill, then the live
/// upserts of every query are merged. Any error ends the whole stream.
struct KeyedWatch {
    store: Arc<dyn MetricStore>,

    /// Queries not opened yet, in request order.
    pending: std::vec::IntoIter<(String, MetricQuery)>,

    /// Query whose backfill is being forwarded.
    opening: Option<(String, MetricWatch)>,

    /// Whether the stream-level backfill completion was sent.
    completed: bool,

    /// Queries past their backfill.
    live: SelectAll<KeyedEvents>,
}

impl KeyedWatch {
    /// Next event of the stream, or `None` once every query's watch ended.
    async fn next(&mut self) -> Option<Result<watch_metrics_response::Event, Status>> {
        loop {
            if let Some((key, watch)) = &mut self.opening {
                match watch.next().await {
                    Some(Ok(WatchEvent::Upsert(result))) => {
                        return Some(Ok(keyed_upsert(key.clone(), result)));
                    }
                    Some(Ok(WatchEvent::BackfillComplete)) => {
                        if let Some((key, watch)) = self.opening.take() {
                            self.live
                                .push(watch.map(move |event| (key.clone(), event)).boxed());
                        }
                        continue;
                    }
                    Some(Err(error)) => return Some(Err(watch_status(error))),
                    None => {
                        return Some(Err(Status::internal(
                            "Metric watch ended before its backfill completed",
                        )));
                    }
                }
            }

            if let Some((key, query)) = self.pending.next() {
                match self.store.watch(query).await {
                    Ok(watch) => self.opening = Some((key, watch)),
                    Err(error) => return Some(Err(watch_status(error))),
                }
                continue;
            }

            if !self.completed {
                self.completed = true;
                return Some(Ok(watch_metrics_response::Event::BackfillComplete(
                    Empty {},
                )));
            }

            return match self.live.next().await? {
                (key, Ok(WatchEvent::Upsert(result))) => Some(Ok(keyed_upsert(key, result))),
                (_, Ok(WatchEvent::BackfillComplete)) => Some(Err(Status::internal(
                    "Metric watch completed its backfill twice",
                ))),
                (_, Err(error)) => Some(Err(watch_status(error))),
            };
        }
    }
}

fn watch_queries(request: WatchMetricsRequest) -> Result<Vec<(String, MetricQuery)>, Status> {
    let count = request.queries.len();
    if !(1..=MAX_WATCH_QUERIES).contains(&count) {
        return Err(Status::invalid_argument(format!(
            "Metric watch carries {count} queries; it takes 1 to {MAX_WATCH_QUERIES}"
        )));
    }

    let mut keys = BTreeSet::new();
    request
        .queries
        .into_iter()
        .map(|keyed| {
            if keyed.key.is_empty() {
                return Err(Status::invalid_argument(
                    "Metric watch query key is required",
                ));
            }
            if keyed.key.len() > MAX_WATCH_KEY_BYTES {
                return Err(Status::invalid_argument(format!(
                    "Metric watch query key is longer than {MAX_WATCH_KEY_BYTES} bytes"
                )));
            }
            if !keys.insert(keyed.key.clone()) {
                return Err(Status::invalid_argument(format!(
                    "Metric watch query key {:?} is repeated",
                    keyed.key
                )));
            }
            let query = keyed
                .query
                .ok_or_else(|| Status::invalid_argument("Metric query is required"))?;
            Ok((keyed.key, metric_query(query)?))
        })
        .collect()
}

fn keyed_upsert(key: String, result: QueryResult) -> watch_metrics_response::Event {
    watch_metrics_response::Event::Upsert(proto::KeyedMetricQueryResult {
        key,
        result: Some(query_result(result)),
    })
}

fn metric_query(query: proto::MetricQuery) -> Result<MetricQuery, Status> {
    // An unspecified operation is left to the store, which requires it only where the metric's
    // kind uses it.
    let reduce = match proto::MetricReduce::try_from(query.reduce) {
        Ok(proto::MetricReduce::Unspecified) => None,
        Ok(proto::MetricReduce::Last) => Some(Reduce::Last),
        Ok(proto::MetricReduce::Avg) => Some(Reduce::Avg),
        Ok(proto::MetricReduce::Min) => Some(Reduce::Min),
        Ok(proto::MetricReduce::Max) => Some(Reduce::Max),
        Ok(proto::MetricReduce::Sum) => Some(Reduce::Sum),
        Err(_) => return Err(Status::invalid_argument("Metric query reduce is invalid")),
    };
    let aggregate = match proto::MetricAggregate::try_from(query.aggregate) {
        Ok(proto::MetricAggregate::Unspecified) => None,
        Ok(proto::MetricAggregate::Sum) => Some(SeriesAggregate::Sum),
        Ok(proto::MetricAggregate::Avg) => Some(SeriesAggregate::Avg),
        Ok(proto::MetricAggregate::Min) => Some(SeriesAggregate::Min),
        Ok(proto::MetricAggregate::Max) => Some(SeriesAggregate::Max),
        Err(_) => {
            return Err(Status::invalid_argument(
                "Metric query aggregate is invalid",
            ));
        }
    };
    let range = match query.range {
        Some(metric_query::Range::Last(last)) => QueryRange::Last(
            std_duration(&last)
                .ok_or_else(|| Status::invalid_argument("Metric query range is invalid"))?,
        ),
        Some(metric_query::Range::Between(between)) => {
            let time = |timestamp: Option<Timestamp>| {
                timestamp
                    .as_ref()
                    .and_then(system_time)
                    .ok_or_else(|| Status::invalid_argument("Metric query range is invalid"))
            };
            QueryRange::Between {
                start: time(between.start)?,
                end: time(between.end)?,
            }
        }
        None => return Err(Status::invalid_argument("Metric query range is required")),
    };
    let step_hint = match &query.step_hint {
        Some(step) => std_duration(step)
            .ok_or_else(|| Status::invalid_argument("Metric query step hint is invalid"))?,
        None => Duration::ZERO,
    };
    Ok(MetricQuery {
        metric: query.metric,
        matchers: query.matchers.into_iter().collect::<BTreeMap<_, _>>(),
        group_by: query.group_by,
        reduce,
        aggregate,
        percentiles: query.percentiles,
        range,
        step_hint,
    })
}

fn query_status(error: QueryError) -> Status {
    match error {
        QueryError::UnknownMetric { .. } => Status::not_found(error.to_string()),
        QueryError::UnknownDimension { .. }
        | QueryError::MissingReduce { .. }
        | QueryError::MissingAggregate { .. }
        | QueryError::InvalidRange
        | QueryError::InvalidPercentile { .. }
        | QueryError::TooManyPercentiles { .. } => Status::invalid_argument(error.to_string()),
        QueryError::TooManyPoints { .. } => Status::resource_exhausted(error.to_string()),
        QueryError::StoreStopped => Status::unavailable(error.to_string()),
    }
}

fn watch_status(error: WatchError) -> Status {
    match error {
        WatchError::Query(error) => query_status(error),
        WatchError::TooManyWatches { .. } => Status::resource_exhausted(error.to_string()),
        WatchError::Lagged => Status::aborted(error.to_string()),
    }
}

fn query_result(result: QueryResult) -> proto::MetricQueryResult {
    proto::MetricQueryResult {
        effective_step: Some(duration(result.effective_step)),
        covered_range: Some(proto::MetricTimeRange {
            start: Some(timestamp(result.covered_start)),
            end: Some(timestamp(result.covered_end)),
        }),
        incarnation: Some(proto::MetricStoreIncarnation {
            id: result.incarnation.id,
            started_at: Some(timestamp(result.incarnation.started_at)),
        }),
        series: result
            .series
            .into_iter()
            .map(|series| proto::MetricSeries {
                labels: series
                    .labels
                    .iter()
                    .map(|(name, value)| (name.to_owned(), value.to_owned()))
                    .collect(),
                points: series
                    .points
                    .into_iter()
                    .map(|point| proto::MetricPoint {
                        bucket_start: Some(timestamp(point.bucket_start)),
                        provisional: point.provisional,
                        value: Some(match point.value {
                            PointValue::Gauge(value) => metric_point::Value::Gauge(value),
                            PointValue::Counter {
                                delta,
                                rate_per_second,
                            } => metric_point::Value::Counter(proto::MetricCounterValue {
                                delta,
                                rate_per_second,
                            }),
                            PointValue::Distribution(distribution) => {
                                metric_point::Value::Distribution(proto::MetricDistributionValue {
                                    count: distribution.count,
                                    sum: distribution.sum,
                                    min: distribution.min,
                                    max: distribution.max,
                                    mean: distribution.mean,
                                    percentiles: distribution
                                        .percentiles
                                        .into_iter()
                                        .map(|(quantile, value)| proto::MetricPercentile {
                                            quantile,
                                            value,
                                        })
                                        .collect(),
                                })
                            }
                        }),
                    })
                    .collect(),
            })
            .collect(),
    }
}

fn descriptor(descriptor: &MetricDescriptor) -> proto::MetricDescriptor {
    proto::MetricDescriptor {
        name: descriptor.name.clone(),
        kind: match descriptor.kind {
            MetricKind::Gauge => proto::MetricKind::Gauge,
            MetricKind::Counter => proto::MetricKind::Counter,
            MetricKind::Distribution { .. } => proto::MetricKind::Distribution,
        } as i32,
        unit: match descriptor.unit {
            MetricUnit::Bytes => proto::MetricUnit::Bytes,
            MetricUnit::Seconds => proto::MetricUnit::Seconds,
            MetricUnit::Requests => proto::MetricUnit::Requests,
            MetricUnit::Tokens => proto::MetricUnit::Tokens,
            MetricUnit::TokensPerSecond => proto::MetricUnit::TokensPerSecond,
            MetricUnit::Ratio => proto::MetricUnit::Ratio,
            MetricUnit::Records => proto::MetricUnit::Records,
            MetricUnit::Series => proto::MetricUnit::Series,
            MetricUnit::Watts => proto::MetricUnit::Watts,
            MetricUnit::DegreesCelsius => proto::MetricUnit::DegreesCelsius,
        } as i32,
        dimensions: descriptor
            .dimensions
            .iter()
            .map(|dimension| proto::MetricDimension {
                name: dimension.name.clone(),
                required: dimension.required,
            })
            .collect(),
        provenance: match descriptor.provenance {
            MetricProvenance::Manager => proto::MetricProvenance::Manager,
            MetricProvenance::Node => proto::MetricProvenance::Node,
        } as i32,
        sample_interval: descriptor.sample_interval.map(duration),
        histogram_scale: match descriptor.kind {
            MetricKind::Distribution { scale } => Some(scale.into()),
            MetricKind::Gauge | MetricKind::Counter => None,
        },
    }
}

#[cfg(test)]
mod tests {
    use std::{collections::HashMap, sync::Mutex};

    use dotcore_domain::{clock::ManualClock, timing::Timing};
    use dotcore_observability::{
        InMemoryMetricStore, InMemoryStoreConfig, RetentionConfig, StoreIncarnation,
    };
    use futures_util::future::BoxFuture;
    use tokio::sync::{Semaphore, mpsc};

    use super::*;

    #[tokio::test]
    async fn a_watch_backfills_every_key_then_completes_once_then_streams_live() {
        let store = ScriptedStore::new(4);
        let memory = store.serve("node.memory", 1);
        let tokens = store.serve("router.tokens", 10);
        let mut watch = store
            .watch(&[("memory", "node.memory"), ("tokens", "router.tokens")])
            .await;

        assert_eq!(next_upsert(&mut watch).await, ("memory".to_owned(), 1));
        // The second query opens only once the first one's backfill was sent.
        assert_eq!(store.open_watches(), 1);
        assert_eq!(next_upsert(&mut watch).await, ("tokens".to_owned(), 10));
        assert_eq!(store.open_watches(), 2);
        assert_eq!(
            next_event(&mut watch).await.ok(),
            Some(watch_metrics_response::Event::BackfillComplete(Empty {}))
        );

        tokens
            .send(Ok(WatchEvent::Upsert(upsert(10))))
            .expect("tokens live upsert");
        assert_eq!(next_upsert(&mut watch).await, ("tokens".to_owned(), 10));
        memory
            .send(Ok(WatchEvent::Upsert(upsert(1))))
            .expect("memory live upsert");
        assert_eq!(next_upsert(&mut watch).await, ("memory".to_owned(), 1));

        drop(watch);
        assert_eq!(store.open_watches(), 0);
    }

    #[tokio::test]
    async fn a_lagging_query_ends_the_whole_stream_with_aborted() {
        let store = ScriptedStore::new(4);
        let memory = store.serve("node.memory", 1);
        let tokens = store.serve("router.tokens", 10);
        let mut watch = store
            .watch(&[("memory", "node.memory"), ("tokens", "router.tokens")])
            .await;
        for _ in 0..3 {
            next_event(&mut watch).await.expect("backfill event");
        }

        memory.send(Err(WatchError::Lagged)).expect("memory lag");
        assert_eq!(
            next_event(&mut watch).await.map_err(|status| status.code()),
            Err(tonic::Code::Aborted)
        );
        assert!(watch.next().await.is_none());
        // The sibling query's store watch is closed, so no upsert of it can follow.
        assert!(tokens.is_closed());
        assert_eq!(store.open_watches(), 0);
    }

    #[tokio::test]
    async fn a_watch_that_cannot_open_every_query_holds_no_permits() {
        let store = ScriptedStore::new(2);
        let _memory = store.serve("node.memory", 1);
        let _tokens = store.serve("router.tokens", 10);
        let _requests = store.serve("router.requests", 1);
        let mut watch = store
            .watch(&[
                ("memory", "node.memory"),
                ("tokens", "router.tokens"),
                ("requests", "router.requests"),
            ])
            .await;

        assert_eq!(next_upsert(&mut watch).await, ("memory".to_owned(), 1));
        assert_eq!(next_upsert(&mut watch).await, ("tokens".to_owned(), 10));
        assert_eq!(
            next_event(&mut watch).await.map_err(|status| status.code()),
            Err(tonic::Code::ResourceExhausted)
        );
        assert!(watch.next().await.is_none());
        assert_eq!(store.open_watches(), 0);

        let _memory = store.serve("node.memory", 1);
        let mut unknown = store
            .watch(&[("memory", "node.memory"), ("unknown", "node.unknown")])
            .await;
        assert_eq!(next_upsert(&mut unknown).await, ("memory".to_owned(), 1));
        assert_eq!(
            next_event(&mut unknown)
                .await
                .map_err(|status| status.code()),
            Err(tonic::Code::NotFound)
        );
        assert_eq!(store.open_watches(), 0);
    }

    #[test]
    fn watch_keys_and_query_counts_are_validated() {
        let keyed = |key: &str| proto::KeyedMetricQuery {
            key: key.to_owned(),
            query: Some(valid_query("node.memory")),
        };
        let validate = |keys: Vec<String>| {
            watch_queries(WatchMetricsRequest {
                queries: keys.iter().map(|key| keyed(key)).collect(),
            })
            .map(|queries| queries.into_iter().map(|(key, _)| key).collect::<Vec<_>>())
            .map_err(|status| status.code())
        };
        let longest = "k".repeat(MAX_WATCH_KEY_BYTES);
        let most = (0..128).map(|index| index.to_string()).collect::<Vec<_>>();

        assert_eq!(validate(vec![longest.clone()]), Ok(vec![longest.clone()]));
        assert_eq!(validate(most.clone()), Ok(most.clone()));

        let invalid = Err(tonic::Code::InvalidArgument);
        assert_eq!(validate(vec![]), invalid);
        assert_eq!(validate(vec![String::new()]), invalid);
        assert_eq!(validate(vec![format!("{longest}k")]), invalid);
        assert_eq!(validate(vec!["a".to_owned(), "a".to_owned()]), invalid);
        assert_eq!(
            validate(most.into_iter().chain(["extra".to_owned()]).collect()),
            invalid
        );
    }

    #[test]
    fn watts_and_celsius_survive_the_wire() {
        let unit = |unit| {
            let declared = descriptor(&MetricDescriptor {
                name: "node.accelerator.signal".to_owned(),
                kind: MetricKind::Gauge,
                unit,
                dimensions: Vec::new(),
                provenance: MetricProvenance::Node,
                sample_interval: Some(Duration::from_secs(1)),
                reporting_delay: None,
            });
            let decoded = <proto::MetricDescriptor as prost::Message>::decode(
                prost::Message::encode_to_vec(&declared).as_slice(),
            )
            .expect("decoded descriptor");
            proto::MetricUnit::try_from(decoded.unit).expect("known unit")
        };

        assert_eq!(unit(MetricUnit::Watts), proto::MetricUnit::Watts);
        assert_eq!(
            unit(MetricUnit::DegreesCelsius),
            proto::MetricUnit::DegreesCelsius
        );
    }

    #[test]
    fn query_failures_map_to_explicit_statuses() {
        let status = |error| query_status(error).code();

        assert_eq!(
            status(QueryError::MissingReduce {
                metric: "node.host.memory.available".to_owned()
            }),
            tonic::Code::InvalidArgument
        );
        assert_eq!(
            status(QueryError::MissingAggregate {
                metric: "router.requests".to_owned()
            }),
            tonic::Code::InvalidArgument
        );

        assert_eq!(
            status(QueryError::TooManyPoints {
                points: 60_000,
                limit: 50_000
            }),
            tonic::Code::ResourceExhausted
        );
        assert_eq!(
            status(QueryError::UnknownMetric {
                metric: "node.unknown".to_owned()
            }),
            tonic::Code::NotFound
        );
        assert_eq!(
            status(QueryError::InvalidRange),
            tonic::Code::InvalidArgument
        );
        assert_eq!(status(QueryError::StoreStopped), tonic::Code::Unavailable);
        assert_eq!(
            status(QueryError::TooManyPercentiles {
                count: 17,
                limit: 16
            }),
            tonic::Code::InvalidArgument
        );
    }

    #[test]
    fn watch_failures_map_to_explicit_statuses() {
        let status = |error| watch_status(error).code();

        assert_eq!(
            status(WatchError::TooManyWatches { limit: 256 }),
            tonic::Code::ResourceExhausted
        );
        assert_eq!(status(WatchError::Lagged), tonic::Code::Aborted);
        assert_eq!(
            status(WatchError::Query(QueryError::InvalidRange)),
            tonic::Code::InvalidArgument
        );
    }

    #[test]
    fn a_query_without_range_or_with_unknown_operations_is_invalid() {
        let complete = valid_query("node.host.memory.available");
        assert_eq!(
            metric_query(complete.clone()).expect("valid query").range,
            QueryRange::Last(Duration::from_secs(60))
        );

        let without_range = proto::MetricQuery {
            range: None,
            ..complete.clone()
        };
        assert_eq!(
            metric_query(without_range)
                .expect_err("missing range")
                .code(),
            tonic::Code::InvalidArgument
        );
        let unknown_reduce = proto::MetricQuery {
            reduce: 99,
            ..complete.clone()
        };
        assert_eq!(
            metric_query(unknown_reduce)
                .expect_err("unknown reduce")
                .code(),
            tonic::Code::InvalidArgument
        );
        let unknown_aggregate = proto::MetricQuery {
            aggregate: 99,
            ..complete
        };
        assert_eq!(
            metric_query(unknown_aggregate)
                .expect_err("unknown aggregate")
                .code(),
            tonic::Code::InvalidArgument
        );
    }

    #[test]
    fn unspecified_operations_are_left_to_the_store() {
        let unspecified = proto::MetricQuery {
            reduce: proto::MetricReduce::Unspecified as i32,
            aggregate: proto::MetricAggregate::Unspecified as i32,
            ..valid_query("router.requests")
        };

        let query = metric_query(unspecified).expect("valid query");

        assert_eq!((query.reduce, query.aggregate), (None, None));
    }

    #[tokio::test]
    async fn operations_missing_where_the_kind_uses_them_fail_with_invalid_argument() {
        let (service, _store) = in_memory_service(InMemoryStoreConfig::default());
        let query =
            |metric: &str, reduce: proto::MetricReduce, aggregate: proto::MetricAggregate| {
                proto::MetricQuery {
                    reduce: reduce as i32,
                    aggregate: aggregate as i32,
                    ..valid_query(metric)
                }
            };
        let unspecified_reduce = proto::MetricReduce::Unspecified;
        let unspecified_aggregate = proto::MetricAggregate::Unspecified;
        let (last, sum) = (proto::MetricReduce::Last, proto::MetricAggregate::Sum);
        let cases = [
            (
                query("router.requests", unspecified_reduce, sum),
                tonic::Code::Ok,
            ),
            (
                query("router.request.ttft", unspecified_reduce, sum),
                tonic::Code::Ok,
            ),
            (
                query("router.request.ttft", last, unspecified_aggregate),
                tonic::Code::Ok,
            ),
            (
                query("node.host.memory.available", unspecified_reduce, sum),
                tonic::Code::InvalidArgument,
            ),
            (
                query("router.requests", last, unspecified_aggregate),
                tonic::Code::InvalidArgument,
            ),
            (
                query("node.host.memory.available", last, unspecified_aggregate),
                tonic::Code::InvalidArgument,
            ),
        ];

        for (query, expected) in cases {
            let metric = query.metric.clone();
            let queried = service
                .query_metrics(Request::new(QueryMetricsRequest {
                    query: Some(query.clone()),
                }))
                .await
                .map(|_| ())
                .map_err(|status| status.code());
            let mut watch = service
                .watch_metrics(Request::new(WatchMetricsRequest {
                    queries: vec![proto::KeyedMetricQuery {
                        key: "query".to_owned(),
                        query: Some(query),
                    }],
                }))
                .await
                .expect("watch stream")
                .into_inner();
            let watched = next_event(&mut watch)
                .await
                .map(|_| ())
                .map_err(|status| status.code());

            let outcome = |result: Result<(), tonic::Code>| result.err().unwrap_or(tonic::Code::Ok);
            assert_eq!(outcome(queried), expected, "query of {metric}");
            assert_eq!(outcome(watched), expected, "watch of {metric}");
        }
    }

    #[tokio::test]
    async fn a_watch_past_the_configured_cap_fails_and_holds_no_permits() {
        let (service, _store) = in_memory_service(InMemoryStoreConfig {
            max_open_queries: 3,
            ..InMemoryStoreConfig::default()
        });
        let watch = |count: usize| {
            let service = service.clone();
            async move {
                service
                    .watch_metrics(Request::new(WatchMetricsRequest {
                        queries: (0..count)
                            .map(|index| proto::KeyedMetricQuery {
                                key: index.to_string(),
                                query: Some(valid_query("node.host.memory.available")),
                            })
                            .collect(),
                    }))
                    .await
                    .expect("watch stream")
                    .into_inner()
            }
        };

        let mut over = watch(4).await;
        for _ in 0..3 {
            next_upsert(&mut over).await;
        }
        assert_eq!(
            next_event(&mut over).await.map_err(|status| status.code()),
            Err(tonic::Code::ResourceExhausted)
        );
        assert!(over.next().await.is_none());

        // Every permit of the refused stream is free again, so a stream at the cap opens fully.
        let mut at_cap = watch(3).await;
        for _ in 0..3 {
            next_upsert(&mut at_cap).await;
        }
        assert_eq!(
            next_event(&mut at_cap).await.ok(),
            Some(watch_metrics_response::Event::BackfillComplete(Empty {}))
        );
    }

    type Watch = Pin<Box<dyn Stream<Item = Result<WatchMetricsResponse, Status>> + Send>>;

    /// Live events fed to one served metric's store watch.
    type Feed = mpsc::UnboundedSender<Result<WatchEvent, WatchError>>;

    /// Metric store whose watches send a point-less backfill, then whatever the test feeds them.
    /// Each open watch holds one of a fixed number of permits, as the real store does.
    struct ScriptedStore {
        permits: Arc<Semaphore>,
        limit: usize,
        feeds: Mutex<HashMap<String, mpsc::UnboundedReceiver<Result<WatchEvent, WatchError>>>>,
        steps: Mutex<HashMap<String, u64>>,
    }

    impl ScriptedStore {
        fn new(limit: usize) -> Arc<Self> {
            Arc::new(Self {
                permits: Arc::new(Semaphore::new(limit)),
                limit,
                feeds: Mutex::default(),
                steps: Mutex::default(),
            })
        }

        /// Declares `metric`, answered at `step_seconds`, and returns its next watch's feed.
        fn serve(self: &Arc<Self>, metric: &str, step_seconds: u64) -> Feed {
            let (feed, events) = mpsc::unbounded_channel();
            self.feeds
                .lock()
                .expect("feeds")
                .insert(metric.to_owned(), events);
            self.steps
                .lock()
                .expect("steps")
                .insert(metric.to_owned(), step_seconds);
            feed
        }

        /// Opens a watch of `queries`, as (key, metric) pairs, through the service.
        async fn watch(self: &Arc<Self>, queries: &[(&str, &str)]) -> Watch {
            let service = MetricsService::new(Arc::clone(self) as Arc<dyn MetricStore>);
            let request = WatchMetricsRequest {
                queries: queries
                    .iter()
                    .map(|(key, metric)| proto::KeyedMetricQuery {
                        key: (*key).to_owned(),
                        query: Some(valid_query(metric)),
                    })
                    .collect(),
            };
            service
                .watch_metrics(Request::new(request))
                .await
                .expect("valid watch")
                .into_inner()
        }

        fn open_watches(&self) -> usize {
            self.limit - self.permits.available_permits()
        }
    }

    impl MetricStore for ScriptedStore {
        fn descriptors(&self) -> Vec<MetricDescriptor> {
            Vec::new()
        }

        fn retention(&self) -> RetentionConfig {
            RetentionConfig::default()
        }

        fn query(&self, _query: MetricQuery) -> BoxFuture<'_, Result<QueryResult, QueryError>> {
            Box::pin(async { Err(QueryError::StoreStopped) })
        }

        fn watch(&self, query: MetricQuery) -> BoxFuture<'_, Result<MetricWatch, WatchError>> {
            Box::pin(async move {
                let permit = Arc::clone(&self.permits)
                    .try_acquire_owned()
                    .map_err(|_| WatchError::TooManyWatches { limit: self.limit })?;
                let unknown = || QueryError::UnknownMetric {
                    metric: query.metric.clone(),
                };
                let events = self
                    .feeds
                    .lock()
                    .expect("feeds")
                    .remove(&query.metric)
                    .ok_or_else(unknown)?;
                let step = self.steps.lock().expect("steps")[&query.metric];
                let live = stream::unfold((events, permit), |(mut events, permit)| async move {
                    let event = events.recv().await?;
                    Some((event, (events, permit)))
                });
                Ok(stream::iter([
                    Ok(WatchEvent::Upsert(upsert(step))),
                    Ok(WatchEvent::BackfillComplete),
                ])
                .chain(live)
                .boxed())
            })
        }
    }

    fn upsert(step_seconds: u64) -> QueryResult {
        QueryResult {
            effective_step: Duration::from_secs(step_seconds),
            covered_start: UNIX_EPOCH,
            covered_end: UNIX_EPOCH,
            incarnation: StoreIncarnation {
                id: "incarnation".to_owned(),
                started_at: UNIX_EPOCH,
            },
            series: Vec::new(),
        }
    }

    async fn next_event(watch: &mut Watch) -> Result<watch_metrics_response::Event, Status> {
        watch
            .next()
            .await
            .expect("watch event")
            .map(|response| response.event.expect("event"))
    }

    /// Next event as an upsert's key and effective step.
    async fn next_upsert(watch: &mut Watch) -> (String, i64) {
        match next_event(watch).await {
            Ok(watch_metrics_response::Event::Upsert(upsert)) => (
                upsert.key,
                upsert
                    .result
                    .and_then(|result| result.effective_step)
                    .expect("effective step")
                    .seconds,
            ),
            other => panic!("expected an upsert, got {other:?}"),
        }
    }

    /// The service over a real in-memory store declaring a host memory gauge, a request counter
    /// and a latency distribution, all manager-measured. The store is returned to keep it running.
    fn in_memory_service(
        config: InMemoryStoreConfig,
    ) -> (MetricsService, Arc<InMemoryMetricStore>) {
        let descriptor = |name: &str, kind| MetricDescriptor {
            name: name.to_owned(),
            kind,
            unit: MetricUnit::Requests,
            dimensions: Vec::new(),
            provenance: MetricProvenance::Manager,
            sample_interval: matches!(kind, MetricKind::Gauge).then_some(Duration::from_secs(1)),
            reporting_delay: None,
        };
        let store = Arc::new(
            InMemoryMetricStore::start(
                config,
                vec![
                    descriptor("node.host.memory.available", MetricKind::Gauge),
                    descriptor("router.requests", MetricKind::Counter),
                    descriptor("router.request.ttft", MetricKind::Distribution { scale: 3 }),
                ],
                Timing::test().metrics,
                Timing::test().watch,
                Arc::new(ManualClock::new()),
            )
            .expect("start store"),
        );
        (
            MetricsService::new(Arc::clone(&store) as Arc<dyn MetricStore>),
            store,
        )
    }

    fn valid_query(metric: &str) -> proto::MetricQuery {
        proto::MetricQuery {
            metric: metric.to_owned(),
            reduce: proto::MetricReduce::Last as i32,
            aggregate: proto::MetricAggregate::Sum as i32,
            range: Some(metric_query::Range::Last(duration(Duration::from_secs(60)))),
            ..proto::MetricQuery::default()
        }
    }
}
