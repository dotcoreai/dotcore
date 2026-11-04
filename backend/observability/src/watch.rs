//! Describes a live read of one metric query: a backfill of the range, a marker once it is sent,
//! then points pushed as buckets close or are revised.
//!
//! Every pushed point replaces the one with the same series labels and bucket start, so applying
//! the same message twice changes nothing. A watcher that reopens receives the backfill of its full
//! range again, so samples that landed while it was away are never missed.

use futures_util::stream::BoxStream;

use crate::query::{QueryError, QueryResult};

/// Messages of one open watch; the stream ends after its first error.
pub type MetricWatch = BoxStream<'static, Result<WatchEvent, WatchError>>;

/// One message of a watch.
#[derive(Debug, Clone, PartialEq)]
pub enum WatchEvent {
    /// Points to insert or replace by series labels and bucket start. It also states the watch's
    /// effective step, its covered range as of this message (a `last` range slides with time,
    /// and points before its start can be dropped), and the store incarnation. An upsert without
    /// points announces a moved covered range or, at least every 15 seconds, keeps an idle watch
    /// writing so a vanished watcher is noticed.
    Upsert(QueryResult),

    /// Every backfill point was sent; later upserts are live.
    BackfillComplete,
}

/// A watch the store cannot open or continue.
#[derive(Debug, Clone, PartialEq, thiserror::Error)]
pub enum WatchError {
    #[error(transparent)]
    Query(#[from] QueryError),

    #[error("metric watches are at their limit of {limit}")]
    TooManyWatches { limit: usize },

    #[error("metric watch fell behind the store's changes")]
    Lagged,
}
