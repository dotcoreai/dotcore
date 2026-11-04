//! Describes the stream that carries a node's pre-aggregated metric buckets to the site manager:
//! aligned bucket summaries grouped in numbered batches, and the transport a node opens it on.
//!
//! Delivery is at least once and merging exactly once: a node sends a batch again until the
//! manager acknowledges the highest sequence its store has taken, and the manager skips a batch it
//! already applied. For example, a node that lost its connection after sending batches 7 and 8
//! sends both again; a manager that applied 7 acknowledges it without merging it twice, then
//! applies 8.

use std::time::{Duration, SystemTime};

use futures_util::{future::BoxFuture, stream::BoxStream};
use tokio::sync::mpsc;

use crate::{
    aggregate::Aggregate,
    labels::LabelSet,
    record::{MetricRecord, RecordValue},
};

/// Summaries of every series that received samples in one epoch-aligned bucket.
#[derive(Debug, Clone, PartialEq)]
pub struct MetricBucket {
    /// Aligned start of the bucket, a multiple of `width` in Unix epoch time.
    pub start: SystemTime,

    /// Bucket width.
    pub width: Duration,

    /// One summary per series.
    pub series: Vec<BucketSeries>,
}

impl MetricBucket {
    /// Store records of every series summary, each covering this bucket.
    pub fn into_records(self) -> impl Iterator<Item = MetricRecord> {
        self.series.into_iter().map(move |series| MetricRecord {
            metric: series.metric,
            labels: series.labels,
            observed_at: self.start,
            value: RecordValue::Aggregate {
                width: self.width,
                aggregate: series.aggregate,
            },
        })
    }
}

/// Summary of one series within one bucket.
#[derive(Debug, Clone, PartialEq)]
pub struct BucketSeries {
    /// Declared metric name.
    pub metric: String,

    /// Labels identifying the series.
    pub labels: LabelSet,

    /// Summary of the series' samples in the bucket.
    pub aggregate: Aggregate,
}

/// Buckets a node flushed together under one sequence of its session.
#[derive(Debug, Clone, PartialEq)]
pub struct BucketBatch {
    /// Nonzero position of the batch in its session; later batches carry larger sequences.
    pub sequence: u64,

    /// Aligned buckets of the batch.
    pub buckets: Vec<MetricBucket>,

    /// Whether the node sends the batch again after a stream failed or ended, so its buckets may
    /// land late without any clock skew.
    pub backfill: bool,
}

/// Identity of one stream: the sending SiteNode and its forwarder lifetime.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BucketStreamOpen {
    /// SiteNode id of the sending node.
    pub node_id: String,

    /// Random identity of the node's forwarder lifetime; sequences restart in a new session.
    pub session_id: String,
}

/// Opens metric bucket streams from a node to the site manager, over the network or in process.
pub trait MetricBucketTransport: Send + Sync {
    /// Opens one stream named by `open`.
    fn open(
        &self,
        open: BucketStreamOpen,
    ) -> BoxFuture<'_, Result<BucketStreamConnection, BucketStreamError>>;
}

/// The node's ends of one open stream.
pub struct BucketStreamConnection {
    /// Sends batches in sequence order; dropping it closes the node's side.
    pub batches: mpsc::Sender<BucketBatch>,

    /// Cumulative applied sequences from the manager, in order; it ends with the stream.
    pub applied: BoxStream<'static, Result<u64, BucketStreamError>>,
}

/// A metric bucket stream that could not open or failed.
#[derive(Debug, thiserror::Error)]
#[error("metric bucket stream failed")]
pub struct BucketStreamError(#[source] Box<dyn std::error::Error + Send + Sync>);

impl BucketStreamError {
    /// Wraps the transport or ingest failure `source`.
    pub fn new(source: impl Into<Box<dyn std::error::Error + Send + Sync>>) -> Self {
        Self(source.into())
    }
}
