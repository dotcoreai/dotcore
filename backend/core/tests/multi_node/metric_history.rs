//! Exercises site-wide memory history across separate manager and worker backend processes.
//!
//! Both SiteNodes report real host observations; the manager records them as metric history, and
//! one query returns each node's series on the same epoch-aligned bucket timestamps.

use std::{collections::BTreeSet, time::Duration};

use dotcore_proto::{
    dotcore::metrics::v1::{
        MetricAggregate, MetricQuery, MetricQueryResult, MetricReduce, QueryMetricsRequest,
        metric_point, metric_query, metrics_service_client::MetricsServiceClient,
    },
    google::protobuf::Duration as ProtoDuration,
};

use crate::support;

const HISTORY_DEADLINE: Duration = Duration::from_secs(30);
const POLL_INTERVAL: Duration = Duration::from_millis(100);

#[tokio::test]
async fn memory_history_of_both_nodes_aligns_on_shared_buckets() {
    let Some(harness) = support::TwoProcessObservationHarness::start() else {
        return;
    };
    let manager_address = harness
        .manager()
        .addresses()
        .management
        .expect("management listener");
    let mut metrics = MetricsServiceClient::connect(format!("http://{manager_address}"))
        .await
        .expect("manager metrics API connection");

    let result = wait_for_history(&mut metrics, |result| {
        let nodes = node_series(result);
        nodes.contains("fixture-node")
            && nodes.contains(support::WORKER_NODE_ID)
            && !shared_buckets(result).is_empty()
    })
    .await;

    assert_eq!(result.effective_step.expect("effective step").seconds, 1);
    assert_eq!(
        node_series(&result),
        BTreeSet::from(["fixture-node", support::WORKER_NODE_ID])
    );
    for series in &result.series {
        assert!(series.points.iter().all(|point| matches!(
            point.value,
            Some(metric_point::Value::Gauge(bytes)) if bytes > 0.0
        )));
        assert!(
            series
                .points
                .iter()
                .all(|point| { point.bucket_start.is_some_and(|start| start.nanos == 0) })
        );
    }
}

async fn wait_for_history(
    metrics: &mut MetricsServiceClient<tonic::transport::Channel>,
    ready: impl Fn(&MetricQueryResult) -> bool,
) -> MetricQueryResult {
    let deadline = tokio::time::Instant::now() + HISTORY_DEADLINE;
    loop {
        let result = metrics
            .query_metrics(QueryMetricsRequest {
                query: Some(MetricQuery {
                    metric: "node.host.memory.available".to_owned(),
                    group_by: vec!["node".to_owned()],
                    reduce: MetricReduce::Last as i32,
                    aggregate: MetricAggregate::Sum as i32,
                    range: Some(metric_query::Range::Last(ProtoDuration {
                        seconds: 60,
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
            .expect("memory history query")
            .into_inner()
            .result
            .expect("query result");
        if ready(&result) {
            return result;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "memory history of both nodes did not appear: {result:?}"
        );
        tokio::time::sleep(POLL_INTERVAL).await;
    }
}

fn node_series(result: &MetricQueryResult) -> BTreeSet<&str> {
    result
        .series
        .iter()
        .filter(|series| !series.points.is_empty())
        .filter_map(|series| series.labels.get("node").map(String::as_str))
        .collect()
}

/// Bucket starts present in every series.
fn shared_buckets(result: &MetricQueryResult) -> BTreeSet<i64> {
    let mut series = result.series.iter().map(|series| {
        series
            .points
            .iter()
            .filter_map(|point| point.bucket_start.map(|start| start.seconds))
            .collect::<BTreeSet<_>>()
    });
    let first = series.next().unwrap_or_default();
    series.fold(first, |shared, buckets| {
        shared.intersection(&buckets).copied().collect()
    })
}
