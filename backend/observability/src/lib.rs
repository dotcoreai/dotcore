//! Observability metrics for the site: typed metric descriptors, samples aggregated into time
//! buckets aligned to the Unix epoch, and the store boundary that retains and queries them.
//!
//! Writers measure and record through a write-only, non-blocking recorder; the store owns
//! retention and answers queries whose points line up across every source, because each bucket
//! of width `w` covers `[k·w, (k+1)·w)` in epoch time. For example, a host memory reading taken at
//! 12:00:03.400 lands in the one-second bucket starting at 12:00:03 and the ten-second bucket
//! starting at 12:00:00.

mod aggregate;
mod bucket_stream;
mod descriptor;
mod forwarder;
mod histogram;
mod ingest;
pub mod labels;
mod memory;
mod query;
mod record;
mod retention;
mod store;
#[cfg(any(test, feature = "test-support"))]
pub mod test_support;
mod watch;

pub use aggregate::{
    Aggregate, AggregateKindMismatch, CounterAggregate, DistributionAggregate, GaugeAggregate,
};
pub use bucket_stream::{
    BucketBatch, BucketSeries, BucketStreamConnection, BucketStreamError, BucketStreamOpen,
    MetricBucket, MetricBucketTransport,
};
pub use descriptor::{
    DescriptorError, LabelError, MetricDescriptor, MetricDimension, MetricKind, MetricProvenance,
    MetricUnit, validate_catalog,
};
pub use forwarder::{
    ForwarderConfig, ForwarderStartError, METRIC_BUCKET_WIDTH, MetricForwarder,
    MetricForwarderTask, NODE_OBSERVING, forwarder_metric_descriptors,
};
pub use histogram::{ExponentialHistogram, MAX_HISTOGRAM_SCALE, MIN_HISTOGRAM_SCALE};
pub use ingest::{BucketStreamEnd, InProcessBucketTransport, MetricBucketIngest};
pub use memory::{InMemoryMetricStore, InMemoryStoreConfig, StoreStartError};
pub use query::{
    DistributionPoint, MAX_PERCENTILES, MetricQuery, Point, PointValue, QueryError, QueryRange,
    QueryResult, Reduce, ResultSeries, SeriesAggregate, StoreIncarnation,
};
pub use record::{MetricRecord, RecordValue};
pub use retention::{RetentionConfig, RetentionError, RetentionTier, aligned_bucket_start};
pub use store::{MetricIngest, MetricRecorder, MetricStore, StoreStopped};
pub use watch::{MetricWatch, WatchError, WatchEvent};
