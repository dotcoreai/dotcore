//! Follows one query live over the in-memory store: sends its backfill, then at every bucket
//! boundary and closing deadline pushes the points that appeared, changed, or closed, and pushes
//! closed buckets again as soon as a late record revises them.
//!
//! Only open buckets and revised ones are evaluated again; a closed point is re-read only when a
//! record lands in or shortly before it, or, for a counter the store measures, when the store's
//! up reading that zero-fills it lands late. A watch that falls behind the store's change
//! announcements ends as lagged instead of slowing recording.

use std::{collections::BTreeMap, sync::Arc, time::Duration};

use futures_util::{StreamExt, stream};
use tokio::sync::{
    OwnedSemaphorePermit,
    broadcast::{
        self,
        error::{RecvError, TryRecvError},
    },
};
use tracing::debug;

use crate::{
    descriptor::{MetricDescriptor, MetricKind, MetricProvenance},
    labels::LabelSet,
    query::{MetricQuery, Point, QueryError, QueryRange, QueryResult, ResultSeries},
    retention::{align_down, align_up, unix_millis, width_millis},
    watch::{MetricWatch, WatchError, WatchEvent},
};

use super::{
    ChangeBatch, Shared,
    evaluate::{self, Window},
    health::STORE_UP,
};

/// Opens a watch of `query` whose backfill reads the store now and whose live part follows
/// `changes`, holding `permit` for its whole life.
pub(super) fn open(
    shared: Arc<Shared>,
    query: MetricQuery,
    changes: broadcast::Receiver<ChangeBatch>,
    permit: OwnedSemaphorePermit,
) -> Result<MetricWatch, WatchError> {
    let descriptor = evaluate::validate(&shared, &query)?.clone();
    let now = unix_millis(shared.clock.wall_now());
    let (start, end) = evaluate::resolve_range(query.range, now)?;
    let window = evaluate::select_window(&shared, start, end, width_millis(query.step_hint), now);
    let grace = descriptor.closing_grace().as_millis() as u64;

    let series = evaluate::evaluate_window(&shared, &descriptor, &query, &window, now)?;

    let mut follower = Follower {
        zero_filled: descriptor.kind == MetricKind::Counter
            && descriptor.provenance == MetricProvenance::Manager,
        lookback: match descriptor.kind {
            MetricKind::Gauge => descriptor
                .gauge_lookback()
                .map(|lookback| lookback.as_millis() as u64)
                .unwrap_or(0),
            MetricKind::Counter | MetricKind::Distribution { .. } => 0,
        },
        tier_width: width_millis(shared.retention.tiers()[window.tier].width),
        tier_retention: width_millis(shared.retention.tiers()[window.tier].retention),
        shared,
        descriptor,
        query,
        window,
        grace,
        open_from: window.start,
        pending: BTreeMap::new(),
        sent_range: (window.start, window.end),
        sent_at: now,
        tick_at: now,
        changes,
        _permit: permit,
    };
    follower.open_from = follower
        .first_open(now)
        .min(follower.range_end())
        .max(window.start);
    follower.tick_at = follower.next_deadline(now);
    for series in &series {
        for point in series.points.iter().filter(|point| point.provisional) {
            follower.pending.insert(
                (series.labels.clone(), unix_millis(point.bucket_start)),
                point.clone(),
            );
        }
    }
    let backfill = evaluate::result(&follower.shared, &window, series);
    debug!(
        event_kind = "metric_watch_opened",
        metric = follower.query.metric,
        step_ms = window.step,
        "metric watch opened"
    );

    let live = stream::unfold(Some(follower), |follower| async move {
        let mut follower = follower?;
        match follower.next().await {
            Ok(upsert) => Some((Ok(WatchEvent::Upsert(upsert)), Some(follower))),
            Err(error) => {
                debug!(
                    event_kind = "metric_watch_ended",
                    metric = follower.query.metric,
                    %error,
                    "metric watch ended"
                );
                Some((Err(error), None))
            }
        }
    });
    Ok(stream::iter([
        Ok(WatchEvent::Upsert(backfill)),
        Ok(WatchEvent::BackfillComplete),
    ])
    .chain(live)
    .boxed())
}

/// Live state of one watch after its backfill. Times are Unix milliseconds.
struct Follower {
    shared: Arc<Shared>,
    descriptor: MetricDescriptor,
    query: MetricQuery,

    /// Tier and step of the watch, fixed when it opened.
    window: Window,

    tier_width: u64,
    tier_retention: u64,
    grace: u64,

    /// How far a gauge reading carries into later buckets; zero for other kinds.
    lookback: u64,

    /// Whether empty buckets read zero where the store's up gauge has a reading, so its readings
    /// revise this watch like its own records.
    zero_filled: bool,

    /// First bucket not yet sent as closed.
    open_from: u64,

    /// Provisional points last sent, by series labels and bucket start.
    pending: BTreeMap<(LabelSet, u64), Point>,

    /// Covered range stated by the last message sent.
    sent_range: (u64, u64),

    /// When the last message was sent.
    sent_at: u64,

    /// Next bucket boundary or closing deadline to evaluate open buckets at.
    tick_at: u64,

    changes: broadcast::Receiver<ChangeBatch>,
    _permit: OwnedSemaphorePermit,
}

impl Follower {
    /// Waits for the next upsert: closed buckets revised by a late record, or at a bucket
    /// boundary or closing deadline, the open buckets that appeared, changed, or closed, or a
    /// point-less upsert when the covered range moved or the watch was silent too long.
    async fn next(&mut self) -> Result<QueryResult, WatchError> {
        loop {
            // An elapsed deadline goes first, so steady record traffic cannot postpone it.
            let now = self.now();
            let wake = self.wake_at();
            if now >= wake {
                if let Some(upsert) = self.advance()? {
                    return Ok(upsert);
                }
                continue;
            }
            let sleep = self
                .shared
                .clock
                .sleep_until(self.shared.clock.now() + Duration::from_millis(wake - now));
            let upsert = tokio::select! {
                // Revisions and a lag surface before the next deadline's points.
                biased;
                received = self.changes.recv() => {
                    let mut revised = None;
                    self.note(received.map_err(recv_error)?, &mut revised);
                    // Takes every batch already queued, so a burst of records is one upsert.
                    loop {
                        match self.changes.try_recv() {
                            Ok(batch) => self.note(batch, &mut revised),
                            Err(TryRecvError::Empty) => break,
                            Err(TryRecvError::Lagged(_)) => return Err(WatchError::Lagged),
                            Err(TryRecvError::Closed) => {
                                return Err(QueryError::StoreStopped.into());
                            }
                        }
                    }
                    self.revise(revised)?
                }
                () = sleep => None,
            };
            if let Some(upsert) = upsert {
                return Ok(upsert);
            }
        }
    }

    /// Widens `revised` to the closed buckets of this watch that `batch` changed. Changes to open
    /// buckets wait for the next deadline, so busy series push at most once per bucket.
    fn note(&self, batch: ChangeBatch, revised: &mut Option<(u64, u64)>) {
        for change in batch.iter().filter(|change| {
            (change.key.metric == self.query.metric
                && change.key.labels.matches(&self.query.matchers))
                || (self.zero_filled && change.key.metric == STORE_UP)
        }) {
            for &(tier, bucket) in &change.buckets {
                let bucket_start = align_down(bucket, self.window.step);
                if tier != self.window.tier
                    || bucket_start >= self.open_from
                    || bucket_start >= self.range_end()
                {
                    continue;
                }
                *revised = Some(match *revised {
                    Some((first, last)) => (first.min(bucket), last.max(bucket)),
                    None => (bucket, bucket),
                });
            }
        }
    }

    /// Evaluates again the closed buckets a late record revised, including later buckets a
    /// gauge reading carries into.
    fn revise(&mut self, revised: Option<(u64, u64)>) -> Result<Option<QueryResult>, WatchError> {
        let Some((first, last)) = revised else {
            return Ok(None);
        };
        let now = self.now();
        let step = self.window.step;
        let start = align_down(first, step).max(self.covered_start(now));
        // A late gauge reading revises every bucket it carries into, so one record can change
        // points up to a whole lookback after it: for a slowly sampled gauge, its sample interval
        // plus its reporting delay.
        let end = align_up(last + self.tier_width + self.lookback, step)
            .min(self.open_from)
            .min(self.covered_end(now));
        if start >= end {
            return Ok(None);
        }
        let series = evaluate::evaluate_window(
            &self.shared,
            &self.descriptor,
            &self.query,
            &self.window.slice(start, end),
            now,
        )?;
        Ok(self.upsert(now, series, false))
    }

    /// Evaluates the open buckets again, keeping new or changed points and the ones that
    /// closed, and moves the closed boundary forward. Sends even without points when the covered
    /// range moved or the heartbeat is due.
    fn advance(&mut self) -> Result<Option<QueryResult>, WatchError> {
        let now = self.now();
        self.tick_at = self.next_deadline(now);
        let covered_start = self.covered_start(now);
        let covered_end = self.covered_end(now);
        let first_open = self.first_open(now);
        let start = self.open_from.max(covered_start);

        let mut series = Vec::new();
        if start < covered_end {
            series = evaluate::evaluate_window(
                &self.shared,
                &self.descriptor,
                &self.query,
                &self.window.slice(start, covered_end),
                now,
            )?;
            for series in &mut series {
                let labels = &series.labels;
                let pending = &mut self.pending;
                series.points.retain(|point| {
                    let key = (labels.clone(), unix_millis(point.bucket_start));
                    if !point.provisional {
                        pending.remove(&key);
                        return true;
                    }
                    if pending.get(&key) == Some(point) {
                        return false;
                    }
                    pending.insert(key, point.clone());
                    true
                });
            }
        }
        self.pending
            .retain(|(_, bucket), _| *bucket >= first_open && *bucket >= covered_start);
        self.open_from = self.open_from.max(first_open.min(self.range_end()));
        let announce = (covered_start, covered_end) != self.sent_range
            || now >= self.sent_at + self.heartbeat_millis();
        Ok(self.upsert(now, series, announce))
    }

    /// Wraps the points of `series` into an upsert, or `None` when there are none and `announce`
    /// is not set.
    fn upsert(
        &mut self,
        now: u64,
        mut series: Vec<ResultSeries>,
        announce: bool,
    ) -> Option<QueryResult> {
        series.retain(|series| !series.points.is_empty());
        if series.is_empty() && !announce {
            return None;
        }
        let covered = self
            .window
            .slice(self.covered_start(now), self.covered_end(now));
        self.sent_range = (covered.start, covered.end);
        self.sent_at = now;
        Some(evaluate::result(&self.shared, &covered, series))
    }

    /// When to evaluate open buckets or send a heartbeat next. Once every bucket of an explicit
    /// range closed, only the heartbeat remains.
    fn wake_at(&self) -> u64 {
        let heartbeat = self.sent_at + self.heartbeat_millis();
        if self.open_from >= self.range_end() {
            heartbeat
        } else {
            self.tick_at.min(heartbeat)
        }
    }

    /// Aligned end of an explicit range; a `last` range has none.
    /// Longest silence of this watch, in milliseconds.
    fn heartbeat_millis(&self) -> u64 {
        self.shared.watch.heartbeat_interval.as_millis() as u64
    }

    fn range_end(&self) -> u64 {
        match self.query.range {
            QueryRange::Last(_) => u64::MAX,
            QueryRange::Between { end, .. } => align_up(unix_millis(end), self.window.step),
        }
    }

    /// The next bucket boundary or closing deadline after `now`.
    fn next_deadline(&self, now: u64) -> u64 {
        let step = self.window.step;
        let phase = self.grace % step;
        let boundary = align_down(now, step) + step;
        let closing = align_down(now.saturating_sub(phase), step) + step + phase;
        boundary.min(closing)
    }

    /// First bucket still open or within its closing grace at `now`.
    fn first_open(&self, now: u64) -> u64 {
        let step = self.window.step;
        align_down(now.saturating_sub(self.grace + step), step) + step
    }

    /// Start of the range at `now`: a `last` range slides with time, bounded by retention.
    fn covered_start(&self, now: u64) -> u64 {
        match self.query.range {
            QueryRange::Last(length) => align_down(
                now.saturating_sub(length.as_millis() as u64)
                    .max(now.saturating_sub(self.tier_retention)),
                self.window.step,
            ),
            QueryRange::Between { .. } => self.window.start,
        }
    }

    /// End of the range at `now`: the bucket holding now, or an explicit end before it.
    fn covered_end(&self, now: u64) -> u64 {
        let end = match self.query.range {
            QueryRange::Last(_) => now,
            QueryRange::Between { end, .. } => unix_millis(end).min(now),
        };
        align_up(end, self.window.step).max(self.covered_start(now) + self.window.step)
    }

    fn now(&self) -> u64 {
        unix_millis(self.shared.clock.wall_now())
    }
}

fn recv_error(error: RecvError) -> WatchError {
    match error {
        RecvError::Lagged(_) => WatchError::Lagged,
        RecvError::Closed => QueryError::StoreStopped.into(),
    }
}
