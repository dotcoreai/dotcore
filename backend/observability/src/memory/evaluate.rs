//! Answers one query from retained buckets: picks the tier and step, combines each series' buckets
//! into points, fills gaps only where evidence allows, then combines series per group.
//!
//! A gauge bucket without readings carries the latest earlier reading while it is younger than
//! the metric's lookback. A counter bucket without increments reads zero only where recorded data
//! shows its writer was observing: for manager-measured counters, the store's own up gauge has a
//! reading inside the bucket; otherwise it is absent. A distribution bucket merges the histograms
//! of every series in its group and is absent when empty.

use std::{
    collections::{BTreeMap, HashMap},
    time::Duration,
};

use crate::{
    aggregate::{Aggregate, DistributionAggregate, GaugeAggregate},
    descriptor::{MetricDescriptor, MetricKind, MetricProvenance},
    labels::LabelSet,
    query::{
        DistributionPoint, MAX_PERCENTILES, MetricQuery, Point, PointValue, QueryError, QueryRange,
        QueryResult, Reduce, ResultSeries, SeriesAggregate,
    },
    retention::{align_down, align_up, from_unix_millis, unix_millis, width_millis},
};

use super::{Series, SeriesKey, Shared, health::STORE_UP};

/// Most points one query may return across all of its series.
pub(super) const MAX_POINTS: u64 = 50_000;

pub(super) fn evaluate(shared: &Shared, query: &MetricQuery) -> Result<QueryResult, QueryError> {
    let descriptor = validate(shared, query)?;
    let now = unix_millis(shared.clock.wall_now());
    let (start, end) = resolve_range(query.range, now)?;
    let window = select_window(shared, start, end, width_millis(query.step_hint), now);
    let series = evaluate_window(shared, descriptor, query, &window, now)?;
    Ok(result(shared, &window, series))
}

/// Checks the query against its metric's declaration and returns that declaration.
pub(super) fn validate<'a>(
    shared: &'a Shared,
    query: &MetricQuery,
) -> Result<&'a MetricDescriptor, QueryError> {
    let descriptor =
        shared
            .descriptors
            .get(&query.metric)
            .ok_or_else(|| QueryError::UnknownMetric {
                metric: query.metric.clone(),
            })?;
    if let Some(unknown) = query
        .matchers
        .keys()
        .chain(&query.group_by)
        .find(|name| !descriptor.has_dimension(name))
    {
        return Err(QueryError::UnknownDimension {
            metric: query.metric.clone(),
            dimension: unknown.clone(),
        });
    }
    // Each operation is required exactly where the metric's kind uses it.
    let (uses_reduce, uses_aggregate) = match descriptor.kind {
        MetricKind::Gauge => (true, true),
        MetricKind::Counter => (false, true),
        MetricKind::Distribution { .. } => (false, false),
    };
    if uses_reduce && query.reduce.is_none() {
        return Err(QueryError::MissingReduce {
            metric: query.metric.clone(),
        });
    }
    if uses_aggregate && query.aggregate.is_none() {
        return Err(QueryError::MissingAggregate {
            metric: query.metric.clone(),
        });
    }
    if query.percentiles.len() > MAX_PERCENTILES {
        return Err(QueryError::TooManyPercentiles {
            count: query.percentiles.len(),
            limit: MAX_PERCENTILES,
        });
    }
    if let Some(&percentile) = query
        .percentiles
        .iter()
        .find(|percentile| !(0.0..=1.0).contains(*percentile))
    {
        return Err(QueryError::InvalidPercentile { percentile });
    }
    Ok(descriptor)
}

/// Start and end of `range` in Unix milliseconds, with the end clamped to `now`.
pub(super) fn resolve_range(range: QueryRange, now: u64) -> Result<(u64, u64), QueryError> {
    let (start, end) = match range {
        QueryRange::Last(length) => (now.saturating_sub(length.as_millis() as u64), now),
        QueryRange::Between { start, end } => (unix_millis(start), unix_millis(end).min(now)),
    };
    if start >= end {
        return Err(QueryError::InvalidRange);
    }
    Ok((start, end))
}

/// Points of every series group over `window`, one series per group that has any.
pub(super) fn evaluate_window(
    shared: &Shared,
    descriptor: &MetricDescriptor,
    query: &MetricQuery,
    window: &Window,
    now: u64,
) -> Result<Vec<ResultSeries>, QueryError> {
    let series = shared.series.lock().expect("metric store lock");
    let mut groups: BTreeMap<LabelSet, Vec<&Series>> = BTreeMap::new();
    for (key, retained) in series.iter() {
        if key.metric == query.metric && key.labels.matches(&query.matchers) {
            groups
                .entry(key.labels.project(&query.group_by))
                .or_default()
                .push(retained);
        }
    }
    let points = groups.len() as u64 * window.buckets();
    if points > MAX_POINTS {
        return Err(QueryError::TooManyPoints {
            points,
            limit: MAX_POINTS,
        });
    }

    let observing = match (descriptor.kind, descriptor.provenance) {
        (MetricKind::Counter, MetricProvenance::Manager) => store_up_buckets(&series, window, now),
        // Only counters zero-fill, and node-sourced ones only from node observing evidence, which
        // the store does not read yet.
        _ => vec![false; window.buckets() as usize],
    };
    let closes_after = window.step + descriptor.closing_grace().as_millis() as u64;
    let open = |bucket: u64| bucket + closes_after > now;
    Ok(groups
        .into_iter()
        .map(|(labels, members)| {
            let points = match descriptor.kind {
                MetricKind::Distribution { .. } => {
                    distribution_points(query, window, &open, &members)
                }
                MetricKind::Gauge | MetricKind::Counter => {
                    let values: Vec<Vec<Option<f64>>> = members
                        .iter()
                        .map(|member| series_values(descriptor, query, window, member, &observing))
                        .collect();
                    combine(descriptor, query, window, &open, &values)
                }
            };
            ResultSeries { labels, points }
        })
        .filter(|series| !series.points.is_empty() || window.covers_all)
        .collect())
}

/// Wraps `series` read over `window` into an answer.
pub(super) fn result(shared: &Shared, window: &Window, series: Vec<ResultSeries>) -> QueryResult {
    QueryResult {
        effective_step: Duration::from_millis(window.step),
        covered_start: from_unix_millis(window.start),
        covered_end: from_unix_millis(window.end),
        incarnation: shared.incarnation.clone(),
        series,
    }
}

/// Tier and aligned bucket grid one query reads.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct Window {
    pub(super) tier: usize,
    pub(super) step: u64,
    pub(super) start: u64,
    pub(super) end: u64,

    /// Whether the window is a whole answer, which lists every matching group even without
    /// points, rather than a slice of one whose empty groups are left out.
    pub(super) covers_all: bool,
}

impl Window {
    /// The same tier and step over `[start, end)`, as a slice of a larger answer.
    pub(super) fn slice(&self, start: u64, end: u64) -> Self {
        Self {
            start,
            end,
            covers_all: false,
            ..*self
        }
    }

    fn buckets(&self) -> u64 {
        (self.end - self.start) / self.step
    }

    fn bucket_start(&self, index: usize) -> u64 {
        self.start + index as u64 * self.step
    }
}

/// Picks the coarsest tier still at or below the step hint among tiers retaining the range start,
/// or the finest such tier when the hint is finer than all of them. A range older than every tier
/// reads the coarsest tier, clamped to what it retains.
pub(super) fn select_window(
    shared: &Shared,
    start: u64,
    end: u64,
    step_hint: u64,
    now: u64,
) -> Window {
    let tiers = shared.retention.tiers();
    let covering: Vec<usize> = (0..tiers.len())
        .filter(|&index| start >= now.saturating_sub(width_millis(tiers[index].retention)))
        .collect();
    let tier = covering
        .iter()
        .rev()
        .find(|&&index| width_millis(tiers[index].width) <= step_hint)
        .or(covering.first())
        .copied()
        .unwrap_or(tiers.len() - 1);

    let width = width_millis(tiers[tier].width);
    let step = align_up(step_hint.max(width), width);
    let retained_from = now.saturating_sub(width_millis(tiers[tier].retention));
    let start = align_down(start.max(retained_from), step);
    let end = align_up(end, step).max(start + step);
    Window {
        tier,
        step,
        start,
        end,
        covers_all: true,
    }
}

/// Whether the store's up gauge has a reading in each bucket of the window, at the window's tier,
/// which is when the store was observing manager-measured metrics. The bucket holding `now` is
/// observed too, since the store is answering in it before its ingest task marks it.
fn store_up_buckets(series: &HashMap<SeriesKey, Series>, window: &Window, now: u64) -> Vec<bool> {
    let up = series.get(&SeriesKey {
        metric: STORE_UP.to_owned(),
        labels: LabelSet::new(),
    });
    (0..window.buckets() as usize)
        .map(|index| {
            let bucket = window.bucket_start(index);
            (bucket..bucket + window.step).contains(&now)
                || up.is_some_and(|up| {
                    up.tiers[window.tier]
                        .range(bucket..bucket + window.step)
                        .next()
                        .is_some()
                })
        })
        .collect()
}

/// One series' value per bucket of the window, before combining series. A counter bucket without
/// increments reads zero where `observing` holds for it.
fn series_values(
    descriptor: &MetricDescriptor,
    query: &MetricQuery,
    window: &Window,
    series: &Series,
    observing: &[bool],
) -> Vec<Option<f64>> {
    let buckets = &series.tiers[window.tier];
    let mut retained = buckets.range(window.start..window.end).peekable();
    let mut values = Vec::with_capacity(window.buckets() as usize);

    match descriptor.kind {
        MetricKind::Gauge => {
            let lookback = descriptor
                .gauge_lookback()
                .map(|lookback| lookback.as_millis() as u64)
                .unwrap_or(0);
            let mut latest = buckets
                .range(..window.start)
                .next_back()
                .and_then(|(_, aggregate)| gauge(aggregate))
                .map(|reading| (reading.last(), unix_millis(reading.last_observed_at())));
            for index in 0..window.buckets() as usize {
                let bucket = window.bucket_start(index);
                let mut merged: Option<GaugeAggregate> = None;
                while let Some(&(&start, aggregate)) = retained.peek()
                    && start < bucket + window.step
                {
                    if let Some(reading) = gauge(aggregate) {
                        match &mut merged {
                            Some(merged) => merged.merge(reading),
                            None => merged = Some(reading.clone()),
                        }
                    }
                    retained.next();
                }
                values.push(match merged {
                    Some(merged) => {
                        latest = Some((merged.last(), unix_millis(merged.last_observed_at())));
                        Some(reduce(
                            &merged,
                            query.reduce.expect("validated gauge queries have a reduce"),
                        ))
                    }
                    None => latest
                        .filter(|(_, observed)| bucket < observed + lookback)
                        .map(|(value, _)| value),
                });
            }
        }
        MetricKind::Counter => {
            for (index, &observing) in observing.iter().enumerate() {
                let bucket = window.bucket_start(index);
                let mut delta: Option<f64> = None;
                while let Some(&(&start, aggregate)) = retained.peek()
                    && start < bucket + window.step
                {
                    if let Aggregate::Counter(counter) = aggregate {
                        *delta.get_or_insert(0.0) += counter.delta();
                    }
                    retained.next();
                }
                values.push(delta.or(observing.then_some(0.0)));
            }
        }
        MetricKind::Distribution { .. } => {
            unreachable!("distribution series merge histograms instead of values")
        }
    }
    values
}

/// Combines member series per bucket; a bucket with no member value is omitted.
fn combine(
    descriptor: &MetricDescriptor,
    query: &MetricQuery,
    window: &Window,
    open: &impl Fn(u64) -> bool,
    members: &[Vec<Option<f64>>],
) -> Vec<Point> {
    let step_seconds = window.step as f64 / 1000.0;
    (0..window.buckets() as usize)
        .filter_map(|index| {
            let present: Vec<f64> = members.iter().filter_map(|values| values[index]).collect();
            if present.is_empty() {
                return None;
            }
            let aggregate = query
                .aggregate
                .expect("validated gauge and counter queries have an aggregate");
            let combined = match aggregate {
                SeriesAggregate::Sum => present.iter().sum(),
                SeriesAggregate::Avg => present.iter().sum::<f64>() / present.len() as f64,
                SeriesAggregate::Min => present.iter().copied().fold(f64::INFINITY, f64::min),
                SeriesAggregate::Max => present.iter().copied().fold(f64::NEG_INFINITY, f64::max),
            };
            let bucket = window.bucket_start(index);
            Some(Point {
                bucket_start: from_unix_millis(bucket),
                provisional: open(bucket),
                value: match descriptor.kind {
                    MetricKind::Gauge => PointValue::Gauge(combined),
                    MetricKind::Counter => PointValue::Counter {
                        delta: combined,
                        rate_per_second: combined / step_seconds,
                    },
                    MetricKind::Distribution { .. } => {
                        unreachable!("distribution series merge histograms instead of values")
                    }
                },
            })
        })
        .collect()
}

/// Merges every member's distribution buckets per step and summarizes each non-empty one with
/// the requested percentiles.
fn distribution_points(
    query: &MetricQuery,
    window: &Window,
    open: &impl Fn(u64) -> bool,
    members: &[&Series],
) -> Vec<Point> {
    let mut merged: Vec<Option<DistributionAggregate>> = vec![None; window.buckets() as usize];
    for member in members {
        for (&start, aggregate) in member.tiers[window.tier].range(window.start..window.end) {
            let Aggregate::Distribution(distribution) = aggregate else {
                continue;
            };
            match &mut merged[((start - window.start) / window.step) as usize] {
                Some(slot) => slot.merge(distribution),
                slot => *slot = Some(distribution.clone()),
            }
        }
    }

    merged
        .into_iter()
        .enumerate()
        .filter_map(|(index, distribution)| {
            let distribution = distribution?;
            let bucket = window.bucket_start(index);
            Some(Point {
                bucket_start: from_unix_millis(bucket),
                provisional: open(bucket),
                value: PointValue::Distribution(DistributionPoint {
                    count: distribution.count(),
                    sum: distribution.sum(),
                    min: distribution.min(),
                    max: distribution.max(),
                    mean: distribution.mean(),
                    percentiles: query
                        .percentiles
                        .iter()
                        .map(|&quantile| (quantile, distribution.percentile(quantile)))
                        .collect(),
                }),
            })
        })
        .collect()
}

fn gauge(aggregate: &Aggregate) -> Option<&GaugeAggregate> {
    match aggregate {
        Aggregate::Gauge(gauge) => Some(gauge),
        Aggregate::Counter(_) | Aggregate::Distribution(_) => None,
    }
}

fn reduce(gauge: &GaugeAggregate, reduce: Reduce) -> f64 {
    match reduce {
        Reduce::Last => gauge.last(),
        Reduce::Avg => gauge.avg(),
        Reduce::Min => gauge.min(),
        Reduce::Max => gauge.max(),
        Reduce::Sum => gauge.sum(),
    }
}
