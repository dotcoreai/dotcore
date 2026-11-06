use std::time::{Duration, SystemTime};

use dotcore_domain::clock::{ManualClock, VirtualClock};
use futures_util::StreamExt;

use super::*;
use crate::{
    descriptor::{MetricDimension, MetricProvenance, MetricUnit},
    query::{
        DistributionPoint, Point, PointValue, QueryRange, QueryResult, Reduce, SeriesAggregate,
    },
    retention::{RetentionTier, aligned_bucket_start},
    watch::WatchEvent,
};

#[tokio::test]
async fn coarse_tiers_equal_the_fine_samples_merged() {
    let (store, clock) = start_store();
    // Samples every 5s over 20 minutes, so the range start is older than the 1s tier keeps.
    let start = aligned(clock.wall_now() - Duration::from_secs(20 * 60), 60);
    let samples: Vec<(SystemTime, f64)> = (0..240)
        .map(|index| (start + Duration::from_secs(index * 5), (index % 7) as f64))
        .collect();
    let recorder = store.recorder();
    recorder.record(
        samples
            .iter()
            .rev()
            .map(|(at, value)| gauge_record("node-a", *at, *value))
            .collect(),
    );

    for reduce in [
        Reduce::Last,
        Reduce::Avg,
        Reduce::Min,
        Reduce::Max,
        Reduce::Sum,
    ] {
        let result = store
            .query(gauge_query(
                reduce,
                QueryRange::Between {
                    start,
                    end: start + Duration::from_secs(20 * 60),
                },
                Duration::from_secs(1),
            ))
            .await
            .expect("query");

        assert_eq!(result.effective_step, Duration::from_secs(10));
        assert_eq!(result.covered_start, start);
        let points = &result.series[0].points;
        assert_eq!(points.len(), 120);
        for (index, point) in points.iter().enumerate() {
            let bucket = &samples[index * 2..index * 2 + 2];
            let values = bucket.iter().map(|(_, value)| *value);
            let expected = match reduce {
                Reduce::Last => bucket[1].1,
                Reduce::Avg => values.sum::<f64>() / 2.0,
                Reduce::Min => values.fold(f64::INFINITY, f64::min),
                Reduce::Max => values.fold(f64::NEG_INFINITY, f64::max),
                Reduce::Sum => values.sum(),
            };
            assert_eq!(point.bucket_start, bucket[0].0);
            assert_eq!(
                point.value,
                PointValue::Gauge(expected),
                "{reduce:?} at {index}"
            );
        }
    }
}

#[tokio::test]
async fn a_late_sample_updates_every_tier_it_falls_in() {
    let (store, clock) = start_store();
    let start = aligned(clock.wall_now() - Duration::from_secs(120), 10);
    let recorder = store.recorder();
    recorder.record(vec![gauge_record("node-a", start, 4.0)]);
    store
        .query(gauge_query(
            Reduce::Sum,
            QueryRange::Last(Duration::from_secs(300)),
            Duration::from_secs(1),
        ))
        .await
        .expect("query");

    // Arrives after its one-second bucket ended and was already read.
    recorder.record(vec![gauge_record(
        "node-a",
        start + Duration::from_millis(500),
        6.0,
    )]);

    let range = QueryRange::Between {
        start,
        end: start + Duration::from_secs(10),
    };
    let fine = store
        .query(gauge_query(Reduce::Sum, range, Duration::from_secs(1)))
        .await
        .expect("fine query");
    let coarse = store
        .query(gauge_query(Reduce::Sum, range, Duration::from_secs(10)))
        .await
        .expect("coarse query");

    assert_eq!(fine.series[0].points[0].value, PointValue::Gauge(10.0));
    assert_eq!(
        coarse.series[0].points,
        vec![Point {
            bucket_start: start,
            provisional: false,
            value: PointValue::Gauge(10.0),
        }]
    );
}

#[tokio::test]
async fn a_gauge_reading_holds_for_its_lookback_then_goes_absent() {
    let (store, clock) = start_store();
    let start = aligned(clock.wall_now() - Duration::from_secs(120), 1);
    store.recorder().record(
        (0..=12)
            .map(|index| {
                gauge_record(
                    "node-a",
                    start + Duration::from_secs(index * 5),
                    index as f64,
                )
            })
            .collect(),
    );

    let result = store
        .query(gauge_query(
            Reduce::Last,
            QueryRange::Between {
                start,
                end: start + Duration::from_secs(100),
            },
            Duration::from_secs(1),
        ))
        .await
        .expect("query");

    // The last reading, at 60s, stays valid for twice the 5s sample interval.
    let points = &result.series[0].points;
    assert_eq!(points.len(), 70);
    for (second, point) in points.iter().enumerate() {
        assert_eq!(
            point.bucket_start,
            start + Duration::from_secs(second as u64)
        );
        assert_eq!(point.value, PointValue::Gauge((second / 5).min(12) as f64));
    }
}

#[tokio::test]
async fn a_slowly_sampled_gauge_holds_until_its_next_reading_plus_the_reporting_delay() {
    let (store, clock) = start_store();
    let start = aligned(clock.wall_now() - Duration::from_secs(120), 1);
    store.recorder().record(vec![
        collection_record(start, 1.0),
        collection_record(start + Duration::from_secs(30), 2.0),
    ]);

    let result = store
        .query(collection_query(QueryRange::Between {
            start,
            end: start + Duration::from_secs(100),
        }))
        .await
        .expect("query");

    // Each reading fills every step until the next; the last stays valid for its 30s sample
    // interval plus the 5s reporting delay, then goes absent.
    let points = &result.series[0].points;
    assert_eq!(points.len(), 65);
    for (second, point) in points.iter().enumerate() {
        assert_eq!(
            point.bucket_start,
            start + Duration::from_secs(second as u64)
        );
        let expected = if second < 30 { 1.0 } else { 2.0 };
        assert_eq!(point.value, PointValue::Gauge(expected));
    }
}

#[tokio::test]
async fn a_slowly_sampled_gauge_bucket_closes_one_second_after_its_reporting_delay() {
    let (store, clock) = start_store();
    let now = aligned(clock.wall_now(), 1);
    store
        .recorder()
        .record(vec![collection_record(now - Duration::from_secs(10), 1.0)]);

    let result = store
        .query(collection_query(QueryRange::Between {
            start: now - Duration::from_secs(10),
            end: now,
        }))
        .await
        .expect("query");

    // Buckets ending at least 6s ago are final; later ones still wait for late node samples.
    let provisional = result.series[0]
        .points
        .iter()
        .map(|point| {
            let ended = now.duration_since(point.bucket_start).expect("past bucket")
                - Duration::from_secs(1);
            (ended.as_secs(), point.provisional)
        })
        .collect::<Vec<_>>();
    assert_eq!(
        provisional,
        (0..10)
            .rev()
            .map(|ended| (ended, ended < 6))
            .collect::<Vec<_>>()
    );
}

#[tokio::test]
async fn an_idle_counter_reads_zero_only_during_the_store_lifetime() {
    let (store, clock) = start_store();
    let started = store.incarnation().started_at;
    clock.advance(Duration::from_secs(30));
    store.recorder().record(vec![MetricRecord::counter(
        "router.requests",
        LabelSet::new(),
        started + Duration::from_secs(10),
        3.0,
    )]);

    let result = store
        .query(MetricQuery {
            metric: "router.requests".to_owned(),
            matchers: BTreeMap::new(),
            group_by: vec![],
            reduce: Some(Reduce::Sum),
            aggregate: Some(SeriesAggregate::Sum),
            percentiles: vec![],
            range: QueryRange::Between {
                start: started - Duration::from_secs(10),
                end: started + Duration::from_secs(20),
            },
            step_hint: Duration::from_secs(1),
        })
        .await
        .expect("query");

    let points = &result.series[0].points;
    let first = aligned(started, 1);
    assert_eq!(points[0].bucket_start, first);
    assert_eq!(points.len() as u64, 20 + u64::from(first < started));
    for point in points {
        let delta = if point.bucket_start == aligned(started + Duration::from_secs(10), 1) {
            3.0
        } else {
            0.0
        };
        assert_eq!(
            point.value,
            PointValue::Counter {
                delta,
                rate_per_second: delta
            }
        );
    }
}

#[tokio::test]
async fn a_manager_counter_reads_zero_only_where_the_store_was_up() {
    let (store, clock) = start_virtual_store();
    let first = aligned(store.incarnation().started_at, 1);
    store
        .recorder()
        .record(vec![request_record(store.incarnation().started_at, 2.0)]);
    clock.advance(Duration::from_secs(10));
    store.drain().await.expect("drain");
    // As if the store had not run during the fourth to sixth seconds.
    remove_up_readings(
        &store,
        first + Duration::from_secs(3),
        first + Duration::from_secs(6),
    );

    let result = store
        .query(counter_query(
            QueryRange::Between {
                start: first - Duration::from_secs(5),
                end: first + Duration::from_secs(10),
            },
            Duration::from_secs(1),
        ))
        .await
        .expect("query");

    assert_eq!(
        counter_deltas(&result),
        [
            (0, 2.0),
            (1, 0.0),
            (2, 0.0),
            (6, 0.0),
            (7, 0.0),
            (8, 0.0),
            (9, 0.0)
        ]
        .map(|(second, delta)| (first + Duration::from_secs(second), delta))
        .to_vec()
    );
}

#[tokio::test]
async fn a_step_spanning_several_buckets_reads_zero_when_any_was_up() {
    let (store, clock) = start_virtual_store();
    let step = aligned(store.incarnation().started_at, 5) + Duration::from_secs(5);
    store
        .recorder()
        .record(vec![request_record(store.incarnation().started_at, 2.0)]);
    clock.advance(Duration::from_secs(20));
    store.drain().await.expect("drain");
    // The first step keeps only its first up reading; the second keeps none.
    remove_up_readings(
        &store,
        step + Duration::from_secs(1),
        step + Duration::from_secs(10),
    );

    let result = store
        .query(counter_query(
            QueryRange::Between {
                start: step,
                end: step + Duration::from_secs(10),
            },
            Duration::from_secs(5),
        ))
        .await
        .expect("query");

    assert_eq!(result.effective_step, Duration::from_secs(5));
    assert_eq!(counter_deltas(&result), vec![(step, 0.0)]);
}

#[tokio::test]
async fn the_bucket_the_store_starts_in_is_up() {
    let (store, _clock) = start_virtual_store();
    let first = aligned(store.incarnation().started_at, 1);

    assert_eq!(
        up_readings(&store),
        vec![(first, 1, first + Duration::from_millis(500))]
    );
}

#[tokio::test]
async fn a_wide_finest_tier_still_zero_fills_manager_counters() {
    let (store, clock) = start_virtual_store_with(InMemoryStoreConfig {
        retention: RetentionConfig::new(vec![RetentionTier {
            width: Duration::from_secs(30),
            retention: Duration::from_secs(60 * 60),
        }])
        .expect("retention"),
        ..InMemoryStoreConfig::default()
    });
    let first = aligned(store.incarnation().started_at, 30);
    store
        .recorder()
        .record(vec![request_record(store.incarnation().started_at, 2.0)]);
    clock.advance(Duration::from_secs(90));

    let result = store
        .query(counter_query(
            QueryRange::Between {
                start: first,
                end: first + Duration::from_secs(90),
            },
            Duration::from_secs(30),
        ))
        .await
        .expect("query");

    assert_eq!(
        counter_deltas(&result),
        [(0, 2.0), (30, 0.0), (60, 0.0)]
            .map(|(second, delta)| (first + Duration::from_secs(second), delta))
            .to_vec()
    );
}

#[tokio::test]
async fn the_bucket_holding_now_reads_zero_before_its_up_reading_lands() {
    let (store, clock) = start_virtual_store();
    let first = aligned(store.incarnation().started_at, 1);
    store
        .recorder()
        .record(vec![request_record(store.incarnation().started_at, 2.0)]);
    clock.advance(Duration::from_secs(3));
    store.drain().await.expect("drain");
    // As if the bucket started after the store's last wake.
    remove_up_readings(
        &store,
        first + Duration::from_secs(3),
        first + Duration::from_secs(4),
    );

    let result = store
        .query(counter_query(
            QueryRange::Between {
                start: first,
                end: first + Duration::from_secs(4),
            },
            Duration::from_secs(1),
        ))
        .await
        .expect("query");

    assert_eq!(
        counter_deltas(&result),
        [(0, 2.0), (1, 0.0), (2, 0.0), (3, 0.0)]
            .map(|(second, delta)| (first + Duration::from_secs(second), delta))
            .to_vec()
    );
}

#[tokio::test]
async fn an_advance_records_one_up_reading_inside_each_elapsed_bucket() {
    let (store, clock) = start_virtual_store();
    let first = aligned(store.incarnation().started_at, 1);

    clock.advance(Duration::from_secs(30));
    store.drain().await.expect("drain");

    assert_eq!(
        up_readings(&store),
        (0..=30)
            .map(|second| {
                let bucket = first + Duration::from_secs(second);
                (bucket, 1, bucket + Duration::from_millis(500))
            })
            .collect::<Vec<_>>()
    );
}

#[tokio::test]
async fn reduce_and_aggregate_are_required_only_where_the_kind_uses_them() {
    let (store, _clock) = start_store();
    let query = |metric: &str, reduce, aggregate| MetricQuery {
        metric: metric.to_owned(),
        matchers: BTreeMap::new(),
        group_by: vec![],
        reduce,
        aggregate,
        percentiles: vec![],
        range: QueryRange::Last(Duration::from_secs(60)),
        step_hint: Duration::from_secs(1),
    };
    let missing_reduce = |metric: &str| {
        Err(QueryError::MissingReduce {
            metric: metric.to_owned(),
        })
    };
    let missing_aggregate = |metric: &str| {
        Err(QueryError::MissingAggregate {
            metric: metric.to_owned(),
        })
    };
    let cases = [
        ("router.requests", None, Some(SeriesAggregate::Sum), Ok(())),
        (
            "router.request.ttft",
            None,
            Some(SeriesAggregate::Sum),
            Ok(()),
        ),
        ("router.request.ttft", Some(Reduce::Last), None, Ok(())),
        (
            "node.memory.used",
            None,
            Some(SeriesAggregate::Sum),
            missing_reduce("node.memory.used"),
        ),
        (
            "node.memory.used",
            Some(Reduce::Last),
            None,
            missing_aggregate("node.memory.used"),
        ),
        (
            "router.requests",
            Some(Reduce::Sum),
            None,
            missing_aggregate("router.requests"),
        ),
    ];

    for (metric, reduce, aggregate, expected) in cases {
        let queried = store.query(query(metric, reduce, aggregate)).await;
        let watched = store.watch(query(metric, reduce, aggregate)).await;

        assert_eq!(queried.map(|_| ()), expected, "query of {metric}");
        assert_eq!(
            watched.map(|_| ()),
            expected.map_err(WatchError::from),
            "watch of {metric}"
        );
    }
}

#[tokio::test]
async fn series_group_by_labels_or_combine() {
    let (store, clock) = start_store();
    let at = aligned(clock.wall_now() - Duration::from_secs(60), 1);
    store.recorder().record(vec![
        gauge_record("node-a", at, 2.0),
        gauge_record("node-a", at + Duration::from_millis(400), 5.0),
        gauge_record("node-b", at + Duration::from_millis(200), 7.0),
    ]);
    let range = QueryRange::Between {
        start: at,
        end: at + Duration::from_secs(1),
    };

    let mut grouped = gauge_query(Reduce::Last, range, Duration::from_secs(1));
    grouped.group_by = vec!["node".to_owned()];
    let grouped = store.query(grouped).await.expect("grouped query");
    let combined = store
        .query(gauge_query(Reduce::Last, range, Duration::from_secs(1)))
        .await
        .expect("combined query");

    let values = |series: &crate::query::ResultSeries| series.points[0].value.clone();
    assert_eq!(
        grouped
            .series
            .iter()
            .map(|series| (series.labels.get("node").map(str::to_owned), values(series)))
            .collect::<Vec<_>>(),
        vec![
            (Some("node-a".to_owned()), PointValue::Gauge(5.0)),
            (Some("node-b".to_owned()), PointValue::Gauge(7.0)),
        ]
    );
    assert_eq!(combined.series.len(), 1);
    assert_eq!(combined.series[0].labels, LabelSet::new());
    assert_eq!(values(&combined.series[0]), PointValue::Gauge(12.0));
}

#[tokio::test]
async fn a_step_finer_than_the_retained_tier_is_coarsened() {
    let (store, _clock) = start_store();

    let result = store
        .query(gauge_query(
            Reduce::Last,
            QueryRange::Last(Duration::from_secs(60 * 60)),
            Duration::from_secs(1),
        ))
        .await
        .expect("query");

    assert_eq!(result.effective_step, Duration::from_secs(10));
    assert_eq!(
        result
            .covered_end
            .duration_since(result.covered_start)
            .expect("ordered"),
        Duration::from_secs(60 * 60 + 10)
    );
}

#[tokio::test]
async fn invalid_queries_are_rejected() {
    let (store, _clock) = start_store();
    let range = QueryRange::Last(Duration::from_secs(60));

    let mut unknown_metric = gauge_query(Reduce::Last, range, Duration::from_secs(1));
    unknown_metric.metric = "node.unknown".to_owned();
    assert_eq!(
        store.query(unknown_metric).await,
        Err(QueryError::UnknownMetric {
            metric: "node.unknown".to_owned()
        })
    );

    let mut unknown_dimension = gauge_query(Reduce::Last, range, Duration::from_secs(1));
    unknown_dimension.group_by = vec!["gpu".to_owned()];
    assert_eq!(
        store.query(unknown_dimension).await,
        Err(QueryError::UnknownDimension {
            metric: "node.memory.used".to_owned(),
            dimension: "gpu".to_owned()
        })
    );

    let now = SystemTime::now();
    assert_eq!(
        store
            .query(gauge_query(
                Reduce::Last,
                QueryRange::Between {
                    start: now,
                    end: now
                },
                Duration::from_secs(1)
            ))
            .await,
        Err(QueryError::InvalidRange)
    );
}

#[tokio::test]
async fn a_query_above_the_point_limit_is_rejected() {
    let (store, clock) = start_store();
    let at = clock.wall_now() - Duration::from_secs(5);
    store.recorder().record(
        (0..60)
            .map(|node| gauge_record(&format!("node-{node}"), at, 1.0))
            .collect(),
    );
    let mut query = gauge_query(
        Reduce::Last,
        QueryRange::Last(Duration::from_secs(15 * 60)),
        Duration::from_secs(1),
    );
    query.group_by = vec!["node".to_owned()];

    assert!(matches!(
        store.query(query).await,
        Err(QueryError::TooManyPoints { limit: 50_000, .. })
    ));
}

#[tokio::test]
async fn records_outside_retention_or_declarations_are_rejected() {
    let (store, clock) = start_store();
    let now = clock.wall_now();
    store.recorder().record(vec![
        gauge_record("node-a", now - Duration::from_secs(49 * 60 * 60), 1.0),
        gauge_record("node-a", now + Duration::from_secs(60), 1.0),
        MetricRecord::gauge("node.unknown", LabelSet::new(), now, 1.0),
        MetricRecord::gauge("node.memory.used", LabelSet::new(), now, 1.0),
        MetricRecord::counter("node.memory.used", node_labels("node-a"), now, 1.0),
        gauge_record("node-a", now, 1.0),
    ]);
    store
        .query(gauge_query(
            Reduce::Last,
            QueryRange::Last(Duration::from_secs(60)),
            Duration::from_secs(1),
        ))
        .await
        .expect("query");

    assert_eq!(store.rejected_records(), 5);
}

#[tokio::test(flavor = "current_thread")]
async fn recording_never_waits_for_a_stalled_store() {
    let (store, clock) = start_store();
    let recorder = store.recorder();
    let at = clock.wall_now();

    // The ingest task cannot run until this test yields, so the queue fills.
    for _ in 0..RECORDER_CAPACITY + 3 {
        recorder.record(vec![
            gauge_record("node-a", at, 1.0),
            gauge_record("node-b", at, 1.0),
        ]);
    }

    assert_eq!(store.dropped_records(), 6);
}

#[tokio::test]
async fn distributions_merge_series_and_steps_into_estimated_percentiles() {
    let (store, clock) = start_store();
    let start = aligned(clock.wall_now() - Duration::from_secs(120), 10);
    store.recorder().record(vec![
        ttft_record("route-a", start, 0.125),
        ttft_record("route-a", start + Duration::from_secs(1), 0.25),
        ttft_record("route-b", start + Duration::from_secs(3), 0.375),
        ttft_record("route-b", start + Duration::from_secs(9), 0.75),
        ttft_record("route-b", start + Duration::from_secs(9), 1.5),
        // The next step, outside the queried range.
        ttft_record("route-b", start + Duration::from_secs(10), 9.0),
    ]);

    let result = store
        .query(MetricQuery {
            metric: "router.request.ttft".to_owned(),
            matchers: BTreeMap::new(),
            group_by: vec![],
            reduce: Some(Reduce::Last),
            aggregate: Some(SeriesAggregate::Sum),
            percentiles: vec![0.0, 0.5, 0.95],
            range: QueryRange::Between {
                start,
                end: start + Duration::from_secs(10),
            },
            step_hint: Duration::from_secs(10),
        })
        .await
        .expect("query");

    // Nearest-rank samples 0.125, 0.375 and 1.5 fall in scale-3 buckets -24, -11 and 5; each
    // estimate is its bucket's geometric midpoint, kept within the smallest and largest sample.
    let midpoint = |index: f64| (((index - 1.0) / 8.0).exp2() * (index / 8.0).exp2()).sqrt();
    assert_eq!(result.effective_step, Duration::from_secs(10));
    assert_eq!(
        result.series[0].points,
        vec![Point {
            bucket_start: start,
            provisional: false,
            value: PointValue::Distribution(DistributionPoint {
                count: 5,
                sum: 3.0,
                min: 0.125,
                max: 1.5,
                mean: 0.6,
                percentiles: vec![(0.0, 0.125), (0.5, midpoint(-11.0)), (0.95, midpoint(5.0))],
            }),
        }]
    );
}

#[tokio::test]
async fn invalid_distribution_samples_and_percentiles_are_rejected() {
    let (store, clock) = start_store();
    let now = clock.wall_now();
    store.recorder().record(vec![
        ttft_record("route-a", now, -1.0),
        ttft_record("route-a", now, f64::NAN),
        ttft_record("route-a", now, 0.5),
    ]);

    let mut query = gauge_query(
        Reduce::Last,
        QueryRange::Last(Duration::from_secs(60)),
        Duration::from_secs(1),
    );
    query.metric = "router.request.ttft".to_owned();
    query.percentiles = vec![0.5, 1.5];

    assert_eq!(
        store.query(query.clone()).await,
        Err(QueryError::InvalidPercentile { percentile: 1.5 })
    );
    query.percentiles = vec![0.5; 17];
    assert_eq!(
        store.query(query.clone()).await,
        Err(QueryError::TooManyPercentiles {
            count: 17,
            limit: 16
        })
    );
    query.percentiles = vec![0.5; 16];
    assert!(store.query(query).await.is_ok());
    assert_eq!(store.rejected_records(), 2);
}

#[test]
fn worst_case_distribution_series_memory_at_the_default_retention() {
    // Every bucket of every tier filled with a histogram at its 160-bucket limit: 5940
    // buckets of key, aggregate and 160 counts each, about 8.1 MB of payload per
    // continuously recorded distribution series before B-tree overhead. Typical buckets hold
    // a few samples across a handful of histogram buckets and cost a few hundred bytes.
    let buckets: u64 = RetentionConfig::default()
        .tiers()
        .iter()
        .map(|tier| tier.retention.as_secs() / tier.width.as_secs())
        .sum();
    let per_bucket = (std::mem::size_of::<u64>()
        + std::mem::size_of::<Aggregate>()
        + crate::histogram::MAX_HISTOGRAM_BUCKETS * std::mem::size_of::<u64>())
        as u64;

    assert_eq!(buckets, 5940);
    assert_eq!(per_bucket, 1360);
    assert_eq!(buckets * per_bucket, 8_078_400);
}

#[test]
fn worst_case_gauge_series_memory_at_the_default_retention() {
    // Every bucket of every tier filled: 900 + 2160 + 2880 = 5940 buckets. Each costs its key
    // and aggregate plus B-tree node overhead, bounded here at twice the payload: about
    // 0.6 MB per continuously reported gauge series.
    let buckets: u64 = RetentionConfig::default()
        .tiers()
        .iter()
        .map(|tier| tier.retention.as_secs() / tier.width.as_secs())
        .sum();
    let payload = (std::mem::size_of::<u64>() + std::mem::size_of::<Aggregate>()) as u64;

    assert_eq!(buckets, 5940);
    assert!(buckets * payload * 2 < 1024 * 1024);
}

#[tokio::test]
async fn a_watch_with_late_samples_converges_to_the_query() {
    let (store, clock) = start_store();
    let base = aligned(clock.wall_now(), 1);
    let at = |offset: i64| offset_time(base, offset);
    let recorder = store.recorder();
    recorder.record(vec![
        gauge_record("node-a", at(-20), 1.0),
        gauge_record("node-a", at(-15), 2.0),
    ]);
    let range = QueryRange::Between {
        start: at(-30),
        end: at(30),
    };
    let mut query = gauge_query(Reduce::Last, range, Duration::from_secs(1));
    query.group_by = vec!["node".to_owned()];
    let mut watch = store.watch(query.clone()).await.expect("open watch");
    let mut received = Received::default();
    received.apply(next_event(&mut watch).await);
    assert_eq!(next_event(&mut watch).await, WatchEvent::BackfillComplete);
    assert_eq!(
        received.value("node-a", at(-15)),
        Some(PointValue::Gauge(2.0))
    );

    clock.advance(Duration::from_secs(2));
    // A live reading, one inside the node grace of six seconds, and one after its bucket
    // closed, which revises closed points at once.
    recorder.record(vec![
        gauge_record("node-a", at(1), 4.0),
        gauge_record("node-a", at(-2), 9.0),
        gauge_record("node-a", at(-12), 7.0),
    ]);
    received
        .read_until(&mut watch, |received| {
            received.value("node-a", at(-12)) == Some(PointValue::Gauge(7.0))
        })
        .await;

    clock.advance(Duration::from_secs(40));
    received
        .read_until(&mut watch, |received| {
            // The last reading, at 1s, carries through its 10s lookback.
            received.closed("node-a", at(10)) && received.closed("node-a", at(-2))
        })
        .await;
    let expected = store.query(query).await.expect("query");

    assert_eq!(received.points, Received::from_result(&expected).points);
    assert_eq!(
        received.value("node-a", at(-3)),
        Some(PointValue::Gauge(7.0))
    );
    assert_eq!(
        received.value("node-a", at(-2)),
        Some(PointValue::Gauge(9.0))
    );
    assert_eq!(
        received.value("node-a", at(10)),
        Some(PointValue::Gauge(4.0))
    );
    assert_eq!(received.value("node-a", at(11)), None);
}

#[tokio::test(flavor = "current_thread")]
async fn an_idle_manager_counter_watch_converges_when_up_readings_land_late() {
    let (store, clock) = start_virtual_store();
    let first = aligned(store.incarnation().started_at, 1);
    let at = |second: u64| first + Duration::from_secs(second);
    let query = counter_query(
        QueryRange::Between {
            start: first,
            end: at(20),
        },
        Duration::from_secs(1),
    );
    store
        .recorder()
        .record(vec![request_record(store.incarnation().started_at, 2.0)]);
    let mut watch = store.watch(query.clone()).await.expect("open watch");
    let mut received = Received::default();
    received.apply(next_event(&mut watch).await);
    assert_eq!(next_event(&mut watch).await, WatchEvent::BackfillComplete);

    // The watch closes these buckets before the store's task runs again, so they read absent
    // until the store's up readings for them land.
    clock.advance(Duration::from_secs(5));
    received.apply(next_event(&mut watch).await);
    assert!(received.closed("", at(0)));
    assert_eq!(received.value("", at(2)), None);
    received
        .read_until(&mut watch, |received| {
            (1..5).all(|second| received.closed("", at(second)))
        })
        .await;

    clock.advance(Duration::from_secs(20));
    received
        .read_until(&mut watch, |received| received.closed("", at(19)))
        .await;
    let expected = store.query(query).await.expect("query");

    assert_eq!(received.points, Received::from_result(&expected).points);
    assert_eq!(
        received
            .points
            .values()
            .map(|point| match point.value {
                PointValue::Counter { delta, .. } => delta,
                _ => panic!("counter point"),
            })
            .collect::<Vec<_>>(),
        [vec![2.0], vec![0.0; 19]].concat()
    );
}

#[tokio::test]
async fn a_reopened_watch_backfills_late_writes_into_closed_buckets() {
    let (store, clock) = start_store();
    let base = aligned(clock.wall_now(), 1);
    let at = |offset: i64| offset_time(base, offset);
    let recorder = store.recorder();
    recorder.record(vec![request_record(at(-5), 3.0)]);
    let query = MetricQuery {
        metric: "router.requests".to_owned(),
        matchers: BTreeMap::new(),
        group_by: vec![],
        reduce: Some(Reduce::Sum),
        aggregate: Some(SeriesAggregate::Sum),
        percentiles: vec![],
        range: QueryRange::Between {
            start: at(-20),
            end: at(20),
        },
        step_hint: Duration::from_secs(1),
    };
    let mut watch = store.watch(query.clone()).await.expect("open watch");
    let mut received = Received::default();
    received.apply(next_event(&mut watch).await);
    assert_eq!(next_event(&mut watch).await, WatchEvent::BackfillComplete);
    assert_eq!(
        received.value("", at(-5)),
        Some(PointValue::Counter {
            delta: 3.0,
            rate_per_second: 3.0
        })
    );
    drop(watch);

    // Written while disconnected, into a bucket that closed long before the reopen.
    clock.advance(Duration::from_secs(30));
    recorder.record(vec![request_record(
        at(-5) + Duration::from_millis(100),
        4.0,
    )]);
    clock.advance(Duration::from_secs(30));

    let mut reopened = store.watch(query.clone()).await.expect("reopen watch");
    let WatchEvent::Upsert(backfill) = next_event(&mut reopened).await else {
        panic!("a watch opens with an upsert");
    };
    let expected = store.query(query).await.expect("query");

    assert_eq!(backfill, expected);
    received.apply(WatchEvent::Upsert(backfill));
    assert_eq!(
        received.value("", at(-5)),
        Some(PointValue::Counter {
            delta: 7.0,
            rate_per_second: 7.0
        })
    );
    assert_eq!(
        next_event(&mut reopened).await,
        WatchEvent::BackfillComplete
    );
}

#[tokio::test]
async fn a_watcher_that_stops_reading_lags_without_blocking_records() {
    let (store, clock) = start_store();
    let at = aligned(clock.wall_now() - Duration::from_secs(60), 1);
    let mut watch = store
        .watch(gauge_query(
            Reduce::Last,
            QueryRange::Last(Duration::from_secs(120)),
            Duration::from_secs(1),
        ))
        .await
        .expect("open watch");
    next_event(&mut watch).await;
    assert_eq!(next_event(&mut watch).await, WatchEvent::BackfillComplete);

    let recorder = store.recorder();
    for round in 0..3 {
        for index in 0..CHANGES_CAPACITY / 2 {
            recorder.record(vec![gauge_record(
                "node-a",
                at,
                (round * CHANGES_CAPACITY + index) as f64,
            )]);
        }
        // Each query drains the queue, so records keep merging while the watcher is idle.
        store
            .query(gauge_query(
                Reduce::Last,
                QueryRange::Last(Duration::from_secs(120)),
                Duration::from_secs(1),
            ))
            .await
            .expect("query");
    }

    assert_eq!(store.dropped_records(), 0);
    // A deadline that elapsed meanwhile may still deliver its upsert first.
    let error = loop {
        match tokio::time::timeout(Duration::from_secs(10), watch.next())
            .await
            .expect("watch event deadline")
            .expect("watch stream open")
        {
            Ok(WatchEvent::Upsert(_)) => continue,
            Ok(event) => panic!("unexpected watch event {event:?}"),
            Err(error) => break error,
        }
    };
    assert_eq!(error, WatchError::Lagged);
    assert_eq!(watch.next().await, None);
}

#[tokio::test]
async fn an_idle_sliding_watch_announces_its_moving_range() {
    let (store, clock) = start_store();
    let mut watch = store
        .watch(gauge_query(
            Reduce::Last,
            QueryRange::Last(Duration::from_secs(60)),
            Duration::from_secs(1),
        ))
        .await
        .expect("open watch");
    let WatchEvent::Upsert(backfill) = next_event(&mut watch).await else {
        panic!("a watch opens with an upsert");
    };
    assert_eq!(next_event(&mut watch).await, WatchEvent::BackfillComplete);

    clock.advance(Duration::from_secs(5));
    let WatchEvent::Upsert(slid) = next_event(&mut watch).await else {
        panic!("live watch messages are upserts");
    };

    assert_eq!(slid.series, vec![]);
    assert!(slid.covered_start >= backfill.covered_start + Duration::from_secs(5));
    assert!(slid.covered_end >= backfill.covered_end + Duration::from_secs(5));
}

#[tokio::test]
async fn revisions_of_an_ended_range_stay_inside_it() {
    let (store, clock) = start_store();
    let base = aligned(clock.wall_now(), 1);
    let at = |offset: i64| offset_time(base, offset);
    let recorder = store.recorder();
    recorder.record(vec![gauge_record("node-a", at(-15), 1.0)]);
    let mut query = gauge_query(
        Reduce::Last,
        QueryRange::Between {
            start: at(-20),
            end: at(-10),
        },
        Duration::from_secs(1),
    );
    query.group_by = vec!["node".to_owned()];
    let mut watch = store.watch(query).await.expect("open watch");
    let mut received = Received::default();
    received.apply(next_event(&mut watch).await);
    assert_eq!(next_event(&mut watch).await, WatchEvent::BackfillComplete);

    // Both buckets closed; only the first lies inside the range, and its reading would
    // carry past the range end.
    recorder.record(vec![
        gauge_record("node-a", at(-12), 3.0),
        gauge_record("node-a", at(-9), 5.0),
    ]);
    received
        .read_until(&mut watch, |received| {
            received.value("node-a", at(-12)) == Some(PointValue::Gauge(3.0))
        })
        .await;

    let points: Vec<(SystemTime, PointValue)> = received
        .points
        .iter()
        .map(|((_, bucket), point)| (*bucket, point.value.clone()))
        .collect();
    assert_eq!(
        points,
        vec![
            (at(-15), PointValue::Gauge(1.0)),
            (at(-14), PointValue::Gauge(1.0)),
            (at(-13), PointValue::Gauge(1.0)),
            (at(-12), PointValue::Gauge(3.0)),
            (at(-11), PointValue::Gauge(3.0)),
        ]
    );
}

#[tokio::test]
async fn an_idle_ended_watch_sends_heartbeats() {
    let (store, clock) = start_store();
    let base = aligned(clock.wall_now(), 1);
    let range = QueryRange::Between {
        start: offset_time(base, -20),
        end: offset_time(base, -10),
    };
    let mut watch = store
        .watch(gauge_query(Reduce::Last, range, Duration::from_secs(1)))
        .await
        .expect("open watch");
    let WatchEvent::Upsert(backfill) = next_event(&mut watch).await else {
        panic!("a watch opens with an upsert");
    };
    assert_eq!(next_event(&mut watch).await, WatchEvent::BackfillComplete);

    clock.advance(dotcore_domain::timing::Timing::production().watch_heartbeat_interval);

    assert_eq!(
        next_event(&mut watch).await,
        WatchEvent::Upsert(QueryResult {
            series: vec![],
            ..backfill
        })
    );
}

#[tokio::test]
async fn an_elapsed_deadline_goes_before_pending_changes() {
    let (store, clock) = start_store();
    let base = aligned(clock.wall_now(), 1);
    let at = |offset: i64| offset_time(base, offset);
    let recorder = store.recorder();
    recorder.record(vec![gauge_record("node-a", at(-30), 1.0)]);
    let query = gauge_query(
        Reduce::Last,
        QueryRange::Last(Duration::from_secs(60)),
        Duration::from_secs(1),
    );
    let mut watch = store.watch(query.clone()).await.expect("open watch");
    let WatchEvent::Upsert(backfill) = next_event(&mut watch).await else {
        panic!("a watch opens with an upsert");
    };
    assert_eq!(next_event(&mut watch).await, WatchEvent::BackfillComplete);

    // A revision of a closed bucket is announced, and the next deadline passes, before the
    // watcher reads again.
    recorder.record(vec![gauge_record("node-a", at(-20), 2.0)]);
    store.query(query).await.expect("query");
    clock.advance(Duration::from_secs(2));

    let WatchEvent::Upsert(tick) = next_event(&mut watch).await else {
        panic!("live watch messages are upserts");
    };
    let mut received = Received::default();
    received.apply(WatchEvent::Upsert(tick.clone()));
    assert!(tick.covered_end > backfill.covered_end);
    assert_eq!(received.value("", at(-20)), None);

    received.apply(next_event(&mut watch).await);
    assert_eq!(received.value("", at(-20)), Some(PointValue::Gauge(2.0)));
}

#[tokio::test]
async fn watches_beyond_the_configured_cap_are_refused() {
    let (store, _clock) = start_store_with(InMemoryStoreConfig {
        max_open_queries: 3,
        ..InMemoryStoreConfig::default()
    });
    let query = gauge_query(
        Reduce::Last,
        QueryRange::Last(Duration::from_secs(60)),
        Duration::from_secs(1),
    );
    let mut watches = Vec::new();
    for _ in 0..3 {
        watches.push(store.watch(query.clone()).await.expect("open watch"));
    }

    assert!(matches!(
        store.watch(query.clone()).await,
        Err(WatchError::TooManyWatches { limit: 3 })
    ));
    drop(watches.pop());
    assert!(store.watch(query).await.is_ok());
}

#[tokio::test]
async fn the_sweep_trims_idle_series_and_removes_them_once_empty() {
    // Time moves only when advanced, so no up reading lands after the last sweep.
    let clock = VirtualClock::starting_at(SystemTime::now());
    let store = start_store_on(InMemoryStoreConfig::default(), Arc::new(clock.clone()));
    let start = clock.wall_now();
    let recorder = store.recorder();
    recorder.record(vec![
        gauge_record("node-a", start, 1.0),
        gauge_record("node-b", start, 2.0),
    ]);
    store.drain().await.expect("drain");
    assert_eq!(tier_buckets(&store, "node-a"), Some(vec![1, 1, 1]));

    // Past the one-second tier's 15 minutes, with node-b still reporting.
    clock.advance(Duration::from_secs(16 * 60));
    let later = clock.wall_now();
    recorder.record(vec![gauge_record("node-b", later, 3.0)]);
    let first_sweep = wait_for_gauge(&store, health::STORE_SERIES, later).await;

    // Both nodes and the store's own up gauge.
    assert_eq!(first_sweep, 3.0);
    assert_eq!(tier_buckets(&store, "node-a"), Some(vec![0, 1, 1]));
    assert_eq!(tier_buckets(&store, "node-b"), Some(vec![1, 2, 2]));
    let mut first_reading = gauge_query(
        Reduce::Last,
        QueryRange::Between {
            start: aligned(start, 10),
            end: aligned(start, 10) + Duration::from_secs(10),
        },
        Duration::from_secs(10),
    );
    first_reading.matchers = BTreeMap::from([("node".to_owned(), "node-b".to_owned())]);
    let first_reading = store.query(first_reading).await.expect("query");
    assert_eq!(
        first_reading.series[0].points[0].value,
        PointValue::Gauge(2.0)
    );

    // Past the coarsest tier's 48 hours nothing was written, so every series but the store's
    // up gauge empties.
    clock.advance(Duration::from_secs(49 * 60 * 60));
    let removed = clock.wall_now();
    wait_for_gauge(&store, health::STORE_SERIES, removed).await;

    assert_eq!(tier_buckets(&store, "node-a"), None);
    assert_eq!(tier_buckets(&store, "node-b"), None);
    assert_eq!(latest_gauge(&store, health::STORE_SERIES).await, Some(1.0));
    let up_bytes = {
        let series = store.shared.series.lock().expect("metric store lock");
        let key = up_key();
        estimated_bytes(&key, &series[&key])
    };
    assert_eq!(
        latest_gauge(&store, health::STORE_MEMORY).await,
        Some(up_bytes as f64)
    );
}

#[tokio::test]
async fn the_sweep_records_rejected_and_late_records() {
    let (store, clock) = start_store();
    let now = clock.wall_now();
    let recorder = store.recorder();
    recorder.record(vec![
        MetricRecord::gauge("node.unknown.a", LabelSet::new(), now, 1.0),
        MetricRecord::gauge("node.unknown.b", LabelSet::new(), now, 1.0),
        MetricRecord::gauge("node.memory.used", LabelSet::new(), now, 1.0),
        // Within the manager's quarter-second grace, then well past it.
        request_record(now - Duration::from_millis(100), 1.0),
        request_record(now - Duration::from_secs(5), 1.0),
    ]);
    let ingest = store.ingestor();
    ingest
        .ingest(
            "node-a",
            vec![
                gauge_record("node-a", now - Duration::from_secs(20), 1.0),
                gauge_record("node-b", now, 1.0),
            ],
            true,
        )
        .await
        .expect("ingest");
    store.drain().await.expect("drain");

    clock.advance(InMemoryStoreConfig::default().sweep_interval);
    wait_for_gauge(&store, health::STORE_SERIES, clock.wall_now()).await;

    assert_eq!(
        counter_totals(
            &store,
            health::SAMPLES_REJECTED,
            &["metric", "node", "reason"]
        )
        .await,
        vec![
            (
                "metric=node.memory.used,node=node-a,reason=other_node".to_owned(),
                1.0
            ),
            (
                "metric=node.memory.used,reason=invalid_labels".to_owned(),
                1.0
            ),
            ("metric=unknown,reason=unknown_metric".to_owned(), 2.0),
        ]
    );
    assert_eq!(
        counter_totals(
            &store,
            health::SAMPLES_LATE,
            &["metric", "node", "backfill"]
        )
        .await,
        vec![
            ("backfill=false,metric=router.requests".to_owned(), 1.0),
            (
                "backfill=true,metric=node.memory.used,node=node-a".to_owned(),
                1.0
            ),
        ]
    );
}

#[tokio::test(flavor = "current_thread")]
async fn the_sweep_records_drops_of_a_full_queue() {
    let (store, clock) = start_store();
    let recorder = store.recorder();
    let at = clock.wall_now();

    // The ingest task cannot run until this test yields, so the queue fills.
    for _ in 0..RECORDER_CAPACITY + 3 {
        recorder.record(vec![
            gauge_record("node-a", at, 1.0),
            gauge_record("node-b", at, 1.0),
        ]);
    }
    // Lets the ingest task start, and schedule its first sweep, before time moves.
    store.drain().await.expect("drain");
    clock.advance(InMemoryStoreConfig::default().sweep_interval);
    wait_for_gauge(&store, health::STORE_SERIES, clock.wall_now()).await;

    assert_eq!(store.dropped_records(), 6);
    assert_eq!(
        counter_totals(&store, health::RECORDER_DROPPED, &[]).await,
        vec![(String::new(), 6.0)]
    );
}

fn start_store() -> (InMemoryMetricStore, ManualClock) {
    start_store_with(InMemoryStoreConfig::default())
}

fn start_store_with(config: InMemoryStoreConfig) -> (InMemoryMetricStore, ManualClock) {
    let clock = ManualClock::new();
    (start_store_on(config, Arc::new(clock.clone())), clock)
}

fn start_store_on(config: InMemoryStoreConfig, clock: Arc<dyn Clock>) -> InMemoryMetricStore {
    InMemoryMetricStore::start(
        config,
        vec![
            MetricDescriptor {
                name: "node.memory.used".to_owned(),
                kind: MetricKind::Gauge,
                unit: MetricUnit::Bytes,
                dimensions: vec![MetricDimension::required("node")],
                provenance: MetricProvenance::Node,
                sample_interval: Some(Duration::from_secs(5)),
                reporting_delay: None,
            },
            MetricDescriptor {
                name: COLLECTION_GAUGE.to_owned(),
                kind: MetricKind::Gauge,
                unit: MetricUnit::Bytes,
                dimensions: vec![MetricDimension::required("node")],
                provenance: MetricProvenance::Node,
                sample_interval: Some(Duration::from_secs(30)),
                reporting_delay: Some(Duration::from_secs(5)),
            },
            MetricDescriptor {
                name: "router.requests".to_owned(),
                kind: MetricKind::Counter,
                unit: MetricUnit::Bytes,
                dimensions: vec![],
                provenance: MetricProvenance::Manager,
                sample_interval: None,
                reporting_delay: None,
            },
            MetricDescriptor {
                name: "router.request.ttft".to_owned(),
                kind: MetricKind::Distribution { scale: 3 },
                unit: MetricUnit::Seconds,
                dimensions: vec![MetricDimension::required("route")],
                provenance: MetricProvenance::Manager,
                sample_interval: None,
                reporting_delay: None,
            },
        ],
        clock,
    )
    .expect("start store")
}

/// A store on a clock that moves only when advanced, started half a second into a second.
fn start_virtual_store() -> (InMemoryMetricStore, VirtualClock) {
    start_virtual_store_with(InMemoryStoreConfig::default())
}

fn start_virtual_store_with(config: InMemoryStoreConfig) -> (InMemoryMetricStore, VirtualClock) {
    let clock = VirtualClock::starting_at(
        SystemTime::UNIX_EPOCH + Duration::from_millis(1_800_000_000_600),
    );
    let store = InMemoryMetricStore::start(
        config,
        vec![MetricDescriptor {
            name: "router.requests".to_owned(),
            kind: MetricKind::Counter,
            unit: MetricUnit::Requests,
            dimensions: vec![],
            provenance: MetricProvenance::Manager,
            sample_interval: None,
            reporting_delay: None,
        }],
        Arc::new(clock.clone()),
    )
    .expect("start store");
    (store, clock)
}

/// Bucket start, reading count and latest observation of each one-second bucket of the
/// store's up gauge.
fn up_readings(store: &InMemoryMetricStore) -> Vec<(SystemTime, u64, SystemTime)> {
    let series = store.shared.series.lock().expect("metric store lock");
    series[&up_key()].tiers[0]
        .iter()
        .map(|(&bucket, aggregate)| {
            let Aggregate::Gauge(gauge) = aggregate else {
                panic!("the up gauge holds gauge buckets");
            };
            (
                to_system_time(bucket),
                gauge.count(),
                gauge.last_observed_at(),
            )
        })
        .collect()
}

/// Removes the one-second up readings of the buckets in `[start, end)`.
fn remove_up_readings(store: &InMemoryMetricStore, start: SystemTime, end: SystemTime) {
    let mut series = store.shared.series.lock().expect("metric store lock");
    let up = series.get_mut(&up_key()).expect("up gauge");
    up.tiers[0].retain(|&bucket, _| !(unix_millis(start)..unix_millis(end)).contains(&bucket));
}

/// Bucket start and delta of every point of a single-series counter result.
fn counter_deltas(result: &QueryResult) -> Vec<(SystemTime, f64)> {
    result
        .series
        .iter()
        .flat_map(|series| &series.points)
        .map(|point| match point.value {
            PointValue::Counter { delta, .. } => (point.bucket_start, delta),
            _ => panic!("counter point"),
        })
        .collect()
}

fn up_key() -> SeriesKey {
    SeriesKey {
        metric: health::STORE_UP.to_owned(),
        labels: LabelSet::new(),
    }
}

/// Retained buckets per tier of the `node.memory.used` series of `node`, when it exists.
fn tier_buckets(store: &InMemoryMetricStore, node: &str) -> Option<Vec<usize>> {
    let series = store.shared.series.lock().expect("metric store lock");
    series
        .get(&SeriesKey {
            metric: "node.memory.used".to_owned(),
            labels: node_labels(node),
        })
        .map(|series| series.tiers.iter().map(BTreeMap::len).collect())
}

/// The latest reading of the label-free health gauge `metric` in the last 30 seconds.
async fn latest_gauge(store: &InMemoryMetricStore, metric: &str) -> Option<f64> {
    let mut query = gauge_query(
        Reduce::Last,
        QueryRange::Last(Duration::from_secs(30)),
        Duration::from_secs(1),
    );
    query.metric = metric.to_owned();
    let result = store.query(query).await.expect("query");
    let point = result.series.first()?.points.last()?;
    match point.value {
        PointValue::Gauge(value) => Some(value),
        _ => None,
    }
}

/// Waits until a sweep recorded the health gauge `metric` at or after `since`, and returns
/// its reading.
async fn wait_for_gauge(store: &InMemoryMetricStore, metric: &str, since: SystemTime) -> f64 {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    loop {
        let mut query = gauge_query(
            Reduce::Last,
            QueryRange::Last(Duration::from_secs(30)),
            Duration::from_secs(1),
        );
        query.metric = metric.to_owned();
        let result = store.query(query).await.expect("query");
        let swept = result.series.first().and_then(|series| {
            series
                .points
                .iter()
                .find(|point| point.bucket_start >= aligned(since, 1))
        });
        if let Some(Point {
            value: PointValue::Gauge(value),
            ..
        }) = swept
        {
            return *value;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "no sweep recorded {metric} before the deadline"
        );
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
}

/// Total increments of counter `metric` in the last minute per series grouped by `group_by`,
/// keyed by its labels as `name=value` pairs joined by commas.
async fn counter_totals(
    store: &InMemoryMetricStore,
    metric: &str,
    group_by: &[&str],
) -> Vec<(String, f64)> {
    let result = store
        .query(MetricQuery {
            metric: metric.to_owned(),
            matchers: BTreeMap::new(),
            group_by: group_by.iter().map(|name| (*name).to_owned()).collect(),
            reduce: Some(Reduce::Sum),
            aggregate: Some(SeriesAggregate::Sum),
            percentiles: vec![],
            range: QueryRange::Last(Duration::from_secs(60)),
            step_hint: Duration::from_secs(1),
        })
        .await
        .expect("query");
    result
        .series
        .iter()
        .map(|series| {
            let labels = series
                .labels
                .iter()
                .map(|(name, value)| format!("{name}={value}"))
                .collect::<Vec<_>>()
                .join(",");
            let total = series
                .points
                .iter()
                .map(|point| match point.value {
                    PointValue::Counter { delta, .. } => delta,
                    _ => panic!("counter point"),
                })
                .sum();
            (labels, total)
        })
        .collect()
}

fn counter_query(range: QueryRange, step_hint: Duration) -> MetricQuery {
    MetricQuery {
        metric: "router.requests".to_owned(),
        matchers: BTreeMap::new(),
        group_by: vec![],
        reduce: None,
        aggregate: Some(SeriesAggregate::Sum),
        percentiles: vec![],
        range,
        step_hint,
    }
}

fn gauge_query(reduce: Reduce, range: QueryRange, step_hint: Duration) -> MetricQuery {
    MetricQuery {
        metric: "node.memory.used".to_owned(),
        matchers: BTreeMap::new(),
        group_by: vec![],
        reduce: Some(reduce),
        aggregate: Some(SeriesAggregate::Sum),
        percentiles: vec![],
        range,
        step_hint,
    }
}

/// A node gauge sampled every 30s whose readings come at most 5s late.
const COLLECTION_GAUGE: &str = "node.collection.used";

fn collection_query(range: QueryRange) -> MetricQuery {
    MetricQuery {
        metric: COLLECTION_GAUGE.to_owned(),
        ..gauge_query(Reduce::Last, range, Duration::from_secs(1))
    }
}

fn collection_record(at: SystemTime, value: f64) -> MetricRecord {
    MetricRecord::gauge(COLLECTION_GAUGE, node_labels("node-a"), at, value)
}

fn gauge_record(node: &str, at: SystemTime, value: f64) -> MetricRecord {
    MetricRecord::gauge("node.memory.used", node_labels(node), at, value)
}

fn ttft_record(route: &str, at: SystemTime, seconds: f64) -> MetricRecord {
    MetricRecord::distribution(
        "router.request.ttft",
        LabelSet::new().with("route", route),
        at,
        seconds,
    )
}

fn request_record(at: SystemTime, delta: f64) -> MetricRecord {
    MetricRecord::counter("router.requests", LabelSet::new(), at, delta)
}

fn node_labels(node: &str) -> LabelSet {
    LabelSet::new().with("node", node)
}

fn aligned(time: SystemTime, width_seconds: u64) -> SystemTime {
    aligned_bucket_start(time, Duration::from_secs(width_seconds))
}

fn offset_time(base: SystemTime, seconds: i64) -> SystemTime {
    if seconds < 0 {
        base - Duration::from_secs(seconds.unsigned_abs())
    } else {
        base + Duration::from_secs(seconds as u64)
    }
}

async fn next_event(watch: &mut MetricWatch) -> WatchEvent {
    tokio::time::timeout(Duration::from_secs(10), watch.next())
        .await
        .expect("watch event deadline")
        .expect("watch stream open")
        .expect("watch event")
}

/// Points accumulated from watch upserts, keyed by series node label and bucket start.
#[derive(Default)]
struct Received {
    points: BTreeMap<(String, SystemTime), Point>,
}

impl Received {
    fn from_result(result: &QueryResult) -> Self {
        let mut received = Self::default();
        received.apply(WatchEvent::Upsert(result.clone()));
        received
    }

    fn apply(&mut self, event: WatchEvent) {
        let WatchEvent::Upsert(result) = event else {
            return;
        };
        for series in result.series {
            let node = series.labels.get("node").unwrap_or_default().to_owned();
            for point in series.points {
                self.points
                    .insert((node.clone(), point.bucket_start), point);
            }
        }
    }

    async fn read_until(&mut self, watch: &mut MetricWatch, done: impl Fn(&Self) -> bool) {
        while !done(self) {
            let event = next_event(watch).await;
            self.apply(event);
        }
    }

    fn value(&self, node: &str, bucket: SystemTime) -> Option<PointValue> {
        self.points
            .get(&(node.to_owned(), bucket))
            .map(|point| point.value.clone())
    }

    fn closed(&self, node: &str, bucket: SystemTime) -> bool {
        self.points
            .get(&(node.to_owned(), bucket))
            .is_some_and(|point| !point.provisional)
    }
}
