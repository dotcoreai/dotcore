//! Exercises the node metric stream across separate manager and worker backend processes under
//! faults: a manager path that goes silent past the liveness window, and a manager restart.
//!
//! The worker's `node.observing` series holds one point per second its hardware sampler recorded,
//! so its gaps and its oldest stored bucket show what the stream delivered.

use std::{
    collections::BTreeSet,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use dotcore::testing::Timing;
use dotcore_proto::{
    dotcore::{
        metrics::v1::{
            MetricAggregate, MetricQuery, MetricQueryResult, MetricReduce, QueryMetricsRequest,
            metric_point, metric_query, metrics_service_client::MetricsServiceClient,
        },
        site::v1::{
            ListMachinesRequest, NodeObservationState, site_service_client::SiteServiceClient,
        },
    },
    google::protobuf::Duration as ProtoDuration,
};

use crate::support;

const DEADLINE: Duration = Duration::from_secs(60);
const POLL_INTERVAL: Duration = Duration::from_millis(100);
const HISTORY_RANGE_SECONDS: i64 = 120;
/// Time after the outage a skip of its last seconds may still be logged in.
const SKIP_LOG_GRACE_MILLIS: i64 = 2_000;
/// Manager liveness window of the outage test, long enough for the outage to span whole buckets.
const OUTAGE_LIVENESS_TTL: Duration = Duration::from_secs(5);

#[tokio::test]
async fn a_silent_manager_path_leaves_no_gap_and_no_double_count_after_it_recovers() {
    let Some(harness) = support::TwoProcessObservationHarness::start_split(
        support::ProcessTestControls {
            timing: Timing {
                node_liveness_ttl: OUTAGE_LIVENESS_TTL,
                ..Timing::test()
            },
            ..support::ProcessTestControls::default()
        },
        support::ProcessTestControls::default(),
        &[],
    ) else {
        return;
    };
    let manager_address = harness
        .manager()
        .addresses()
        .management
        .expect("management listener");
    support::wait_for_worker_connection(manager_address).await;
    let mut metrics = metrics_client(manager_address).await;
    wait_for_observing(&mut metrics, |points| !points.is_empty()).await;

    // The path goes silent until the manager sees the worker as disconnected.
    let outage_start_millis = unix_millis();
    harness.manager_path().set_silent(true);
    let mut site = SiteServiceClient::connect(format!("http://{manager_address}"))
        .await
        .expect("manager site API connection");
    let deadline = deadline_from_now();
    while worker_state(&mut site).await != NodeObservationState::Disconnected {
        retry_before(deadline, "the worker to be seen disconnected").await;
    }
    let outage_end_millis = unix_millis();
    harness.manager_path().set_silent(false);

    // Buckets wholly inside the outage were buffered on the worker and arrive once it reconnects,
    // except for seconds its sampler reports as skipped.
    let (outage_start, outage_end) = (outage_start_millis / 1_000, outage_end_millis / 1_000);
    let outage: BTreeSet<i64> = (outage_start + 1..outage_end).collect();
    // The worker was last heard up to a second before the path went silent, and the partial
    // seconds at both ends are not whole buckets.
    assert!(
        outage.len() as u64 >= OUTAGE_LIVENESS_TTL.as_secs() - 3,
        "the outage {outage_start}..{outage_end} spans the liveness window"
    );
    let worker_root = harness.worker().site_root().to_owned();
    let deadline = deadline_from_now();
    let (result, missing, skipped) = loop {
        let result = observing_history(&mut metrics).await;
        let missing: Vec<i64> = outage
            .difference(&observing_buckets(&result))
            .copied()
            .collect();
        let skipped = skipped_seconds(
            &worker_root,
            outage_start_millis,
            outage_end_millis + SKIP_LOG_GRACE_MILLIS,
        );
        if missing.len() as u64 <= skipped {
            break (result, missing, skipped);
        }
        retry_before(deadline, "the outage to be backfilled").await;
    };
    assert!(
        missing.len() as u64 <= skipped,
        "outage seconds {missing:?} are neither stored nor skipped ({skipped}): {result:?}\n\
         worker log:\n{}",
        support::retained_log_text(&worker_root)
    );

    // A resent batch is never merged twice: every bucket holds exactly one observation.
    for series in &result.series {
        for point in &series.points {
            assert!(
                matches!(point.value, Some(metric_point::Value::Gauge(value)) if value == 1.0),
                "series {:?} point {point:?} is not a single observation",
                series.labels
            );
        }
    }
}

#[tokio::test]
async fn a_worker_streams_to_a_restarted_manager_without_buckets_before_its_start() {
    let Some(mut harness) = support::TwoProcessObservationHarness::start() else {
        return;
    };
    let manager_address = harness
        .manager()
        .addresses()
        .management
        .expect("management listener");
    support::wait_for_worker_connection(manager_address).await;
    let mut metrics = metrics_client(manager_address).await;
    let first = wait_for_observing(&mut metrics, |points| !points.is_empty()).await;
    let first_incarnation = first.incarnation.expect("incarnation").id;

    // The worker keeps sampling while the manager is down, so it holds buckets older than the
    // new store.
    harness.manager_mut().shutdown();
    let stopped_millis = unix_millis();
    harness.manager_mut().restart();
    let manager_address = harness
        .manager()
        .addresses()
        .management
        .expect("restarted management listener");
    let mut metrics = metrics_client(manager_address).await;

    let restarted = wait_for_observing(&mut metrics, |points| {
        points.contains_key(support::WORKER_NODE_ID)
    })
    .await;
    let incarnation = restarted.incarnation.clone().expect("incarnation");
    assert_ne!(incarnation.id, first_incarnation);
    let started_at = incarnation.started_at.expect("incarnation start");
    let started_millis = started_at.seconds * 1_000 + i64::from(started_at.nanos) / 1_000_000;

    // Buckets the worker closed after the old manager stopped and before the new store started can
    // only be delivered to the new manager, which must clip them. Buckets are sent in order, so
    // the stored worker points prove that backlog was already processed.
    let backlog: Vec<i64> = (stopped_millis / 1_000..)
        .take_while(|second| second * 1_000 < started_millis)
        .collect();
    assert!(
        !backlog.is_empty(),
        "no whole second between the manager stop at {stopped_millis} ms and the new store \
         start at {started_millis} ms to hold a backlog"
    );
    let worker_buckets = observing_buckets(&restarted);
    assert!(
        !worker_buckets.is_empty()
            && backlog
                .iter()
                .all(|second| !worker_buckets.contains(second)),
        "clipped backlog {backlog:?} appears among stored worker buckets {worker_buckets:?}"
    );
    for series in &restarted.series {
        for point in &series.points {
            let start = point.bucket_start.expect("bucket start");
            assert!(
                start.seconds * 1_000 >= started_millis,
                "series {:?} holds bucket {} before the store start {started_millis} ms",
                series.labels,
                start.seconds
            );
        }
    }
    assert!(
        harness.worker_mut().is_running(),
        "the worker streamed to the new manager without a restart"
    );
}

async fn metrics_client(
    address: std::net::SocketAddr,
) -> MetricsServiceClient<tonic::transport::Channel> {
    MetricsServiceClient::connect(format!("http://{address}"))
        .await
        .expect("manager metrics API connection")
}

/// Waits until the points of the observing history, as bucket starts per node, satisfy `ready`.
async fn wait_for_observing(
    metrics: &mut MetricsServiceClient<tonic::transport::Channel>,
    ready: impl Fn(&std::collections::BTreeMap<String, BTreeSet<i64>>) -> bool,
) -> MetricQueryResult {
    let deadline = deadline_from_now();
    loop {
        let result = observing_history(metrics).await;
        let points = result
            .series
            .iter()
            .filter_map(|series| {
                let node = series.labels.get("node")?.clone();
                let buckets = series
                    .points
                    .iter()
                    .filter_map(|point| point.bucket_start.map(|start| start.seconds))
                    .collect();
                Some((node, buckets))
            })
            .collect();
        if ready(&points) {
            return result;
        }
        retry_before(deadline, &format!("observing history, last {result:?}")).await;
    }
}

async fn observing_history(
    metrics: &mut MetricsServiceClient<tonic::transport::Channel>,
) -> MetricQueryResult {
    metrics
        .query_metrics(QueryMetricsRequest {
            query: Some(MetricQuery {
                metric: "node.observing".to_owned(),
                group_by: vec!["node".to_owned()],
                reduce: MetricReduce::Sum as i32,
                aggregate: MetricAggregate::Sum as i32,
                range: Some(metric_query::Range::Last(ProtoDuration {
                    seconds: HISTORY_RANGE_SECONDS,
                    nanos: 0,
                })),
                step_hint: Some(ProtoDuration {
                    seconds: 1,
                    nanos: 0,
                }),
                ..MetricQuery::default()
            }),
        })
        .await
        .expect("observing history query")
        .into_inner()
        .result
        .expect("query result")
}

/// Bucket starts, in Unix seconds, the worker's observing series holds.
fn observing_buckets(result: &MetricQueryResult) -> BTreeSet<i64> {
    result
        .series
        .iter()
        .filter(|series| {
            series.labels.get("node").map(String::as_str) == Some(support::WORKER_NODE_ID)
        })
        .flat_map(|series| series.points.iter())
        .filter_map(|point| point.bucket_start.map(|start| start.seconds))
        .collect()
}

async fn worker_state(
    site: &mut SiteServiceClient<tonic::transport::Channel>,
) -> NodeObservationState {
    let machines = site
        .list_machines(ListMachinesRequest {})
        .await
        .expect("manager machine inventory")
        .into_inner()
        .machines;
    machines
        .iter()
        .find(|machine| machine.machine_id == support::WORKER_NODE_ID)
        .and_then(|machine| machine.observation_freshness.as_ref())
        .map_or(NodeObservationState::Unspecified, |freshness| {
            NodeObservationState::try_from(freshness.state).expect("known observation state")
        })
}

/// Seconds the worker's sampler logged as skipped between `from_millis` and `to_millis` (Unix
/// time). The sampler exposes them only in its debug log, which the harness retains.
fn skipped_seconds(worker_root: &std::path::Path, from_millis: i64, to_millis: i64) -> u64 {
    support::retained_json_records(worker_root)
        .iter()
        .filter(|record| {
            record["fields"]["event_kind"] == "node_hardware_sample"
                && record["fields"]["outcome"] == "skipped"
                && (from_millis..=to_millis).contains(&logged_at_millis(record))
        })
        .map(|record| {
            let seconds = &record["fields"]["seconds"];
            seconds
                .as_u64()
                .or_else(|| seconds.as_str().and_then(|text| text.parse().ok()))
                .expect("skipped seconds")
        })
        .sum()
}

/// Unix time in milliseconds a retained log record was written at.
fn logged_at_millis(record: &serde_json::Value) -> i64 {
    let timestamp = record["timestamp"].as_str().expect("log record timestamp");
    let parsed =
        time::OffsetDateTime::parse(timestamp, &time::format_description::well_known::Rfc3339)
            .expect("RFC 3339 log timestamp");
    (parsed.unix_timestamp_nanos() / 1_000_000) as i64
}

fn unix_millis() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("time after epoch")
        .as_millis() as i64
}

fn deadline_from_now() -> tokio::time::Instant {
    tokio::time::Instant::now() + DEADLINE
}

/// Sleeps one poll interval, failing the test when `deadline` passed while waiting for `what`.
async fn retry_before(deadline: tokio::time::Instant, what: &str) {
    assert!(
        tokio::time::Instant::now() < deadline,
        "timed out waiting for {what}"
    );
    tokio::time::sleep(POLL_INTERVAL).await;
}
