//! Separates writing metrics from reading them: writers hold a recorder that never waits, and
//! readers query a store whose backing (memory today, a time-series database later) they never
//! see.

use futures_util::future::BoxFuture;

use crate::{
    descriptor::MetricDescriptor,
    query::{MetricQuery, QueryError, QueryResult},
    record::MetricRecord,
    retention::RetentionConfig,
    watch::{MetricWatch, WatchError},
};

/// Write-only handle given to code that measures metrics.
pub trait MetricRecorder: Send + Sync {
    /// Hands `records` to the store without waiting. Records the store cannot take now are
    /// dropped and counted, never retried, so measuring never slows the measured work.
    fn record(&self, records: Vec<MetricRecord>);
}

/// Write handle for the metrics a SiteNode streams to the site manager: it waits for room in the
/// store instead of dropping records, so the stream can confirm delivery onward.
pub trait MetricIngest: Send + Sync {
    /// Hands `records` SiteNode `node_id` measured to the store as one batch, waiting while the
    /// store is busy. Resolves once the store has taken them, so every later query includes them.
    /// The store accepts only node-measured metrics labeled with `node_id`; it rejects and counts
    /// every other record, like a recorder's invalid records. `backfill` marks a batch the node
    /// sends again after its stream failed or ended, so records merging late because of the
    /// disconnect count apart from late ones a skewed clock produced.
    fn ingest<'a>(
        &'a self,
        node_id: &'a str,
        records: Vec<MetricRecord>,
        backfill: bool,
    ) -> BoxFuture<'a, Result<(), StoreStopped>>;
}

/// Read access to retained metric history.
pub trait MetricStore: Send + Sync {
    /// Every metric the store accepts.
    fn descriptors(&self) -> Vec<MetricDescriptor>;

    /// Resolutions and retention the store serves.
    fn retention(&self) -> RetentionConfig;

    /// Answers `query` from retained history, including every record handed to a recorder
    /// before the call. Percentiles and other summaries are estimates where documented.
    fn query(&self, query: MetricQuery) -> BoxFuture<'_, Result<QueryResult, QueryError>>;

    /// Opens a live read of `query`: a backfill of its full range, then points as buckets close or are revised. After
    /// quiescence the points received equal a query over the same range. A watcher that stops
    /// reading never slows recording; it falls behind and its watch ends with an error.
    fn watch(&self, query: MetricQuery) -> BoxFuture<'_, Result<MetricWatch, WatchError>>;
}

/// The store stopped and takes no more records.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[error("metric store stopped")]
pub struct StoreStopped;
