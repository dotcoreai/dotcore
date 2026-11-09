//! Declares what one metric is: its name, kind, unit, the labels its series carry, where it is
//! measured, and how often a gauge is expected to be sampled and how late its readings may come.
//!
//! The store accepts samples only for declared metrics and only with label sets that fit the
//! declaration, so every series of a metric can be grouped and filtered by the same dimensions.
//! Names, units and label names also follow the rules an external time-series database export
//! needs: each unit has its UCUM code, and no two metrics map to the same exported name.

use std::{collections::BTreeMap, time::Duration};

use crate::{
    histogram::{MAX_HISTOGRAM_SCALE, MIN_HISTOGRAM_SCALE},
    labels::LabelSet,
};

/// Typed declaration of one metric the store accepts.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MetricDescriptor {
    /// Dotted lowercase name, such as `node.host.memory.available`.
    pub name: String,

    /// How samples combine within a bucket.
    pub kind: MetricKind,

    /// Unit of every sample value.
    pub unit: MetricUnit,

    /// Label dimensions a series of this metric may or must carry.
    pub dimensions: Vec<MetricDimension>,

    /// Where the metric is measured, which decides how its samples are timestamped.
    pub provenance: MetricProvenance,

    /// Expected interval between gauge samples. Required for gauges, absent for counters and
    /// distributions.
    pub sample_interval: Option<Duration>,

    /// How late a node-measured gauge's next reading may come after its sample interval, and so
    /// how long its points wait for it. The sample interval when absent; set it for a gauge read
    /// on a slow schedule, whose interval is long but whose readings are never that late. A gauge
    /// reading stays valid for its sample interval plus this delay.
    pub reporting_delay: Option<Duration>,
}

impl MetricDescriptor {
    /// Checks that the declaration is usable: a dotted lowercase name with no dotted segment that
    /// is a unit word (a suffix an export appends, such as `bytes` or `seconds`) and, for a
    /// counter, not ending in `count` or `total`, since an export appends `_total`; unique
    /// dimensions named like Prometheus labels and not reserved by it, as `__` names are and, for
    /// a distribution, `le` and `quantile`; a sample interval exactly when the metric is a gauge;
    /// a reporting delay only on a node-measured gauge; and a supported histogram scale for a
    /// distribution.
    pub fn validate(&self) -> Result<(), DescriptorError> {
        let valid_name = !self.name.is_empty()
            && self.name.split('.').all(|part| {
                !part.is_empty()
                    && part
                        .chars()
                        .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_')
            });
        if !valid_name {
            return Err(DescriptorError::InvalidName {
                metric: self.name.clone(),
            });
        }

        if let Some(word) = self
            .name
            .split('.')
            .find(|segment| UNIT_WORDS.contains(segment))
        {
            return Err(DescriptorError::UnitInName {
                metric: self.name.clone(),
                word: word.to_owned(),
            });
        }
        if self.kind == MetricKind::Counter
            && self
                .name
                .split(['.', '_'])
                .next_back()
                .is_some_and(|word| COUNTER_SUFFIX_WORDS.contains(&word))
        {
            return Err(DescriptorError::CounterSuffix {
                metric: self.name.clone(),
            });
        }

        for (index, dimension) in self.dimensions.iter().enumerate() {
            if !is_label_name(&dimension.name) {
                return Err(DescriptorError::InvalidDimension {
                    metric: self.name.clone(),
                    dimension: dimension.name.clone(),
                });
            }
            if dimension.name.starts_with("__")
                || (matches!(self.kind, MetricKind::Distribution { .. })
                    && HISTOGRAM_LABELS.contains(&dimension.name.as_str()))
            {
                return Err(DescriptorError::ReservedDimension {
                    metric: self.name.clone(),
                    dimension: dimension.name.clone(),
                });
            }
            if self.dimensions[..index]
                .iter()
                .any(|other| other.name == dimension.name)
            {
                return Err(DescriptorError::DuplicateDimension {
                    metric: self.name.clone(),
                    dimension: dimension.name.clone(),
                });
            }
        }

        if self.reporting_delay.is_some()
            && (self.kind != MetricKind::Gauge || self.provenance != MetricProvenance::Node)
        {
            return Err(DescriptorError::UnexpectedReportingDelay {
                metric: self.name.clone(),
            });
        }

        match (self.kind, self.sample_interval) {
            (MetricKind::Gauge, None) | (MetricKind::Gauge, Some(Duration::ZERO)) => {
                Err(DescriptorError::MissingSampleInterval {
                    metric: self.name.clone(),
                })
            }
            (MetricKind::Counter | MetricKind::Distribution { .. }, Some(_)) => {
                Err(DescriptorError::UnexpectedSampleInterval {
                    metric: self.name.clone(),
                })
            }
            (MetricKind::Distribution { scale }, None)
                if !(MIN_HISTOGRAM_SCALE..=MAX_HISTOGRAM_SCALE).contains(&scale) =>
            {
                Err(DescriptorError::UnsupportedScale {
                    metric: self.name.clone(),
                    scale,
                })
            }
            _ => Ok(()),
        }
    }

    /// Checks that `labels` name only declared dimensions, every required one, and no empty
    /// value.
    pub fn validate_labels(&self, labels: &LabelSet) -> Result<(), LabelError> {
        for (name, value) in labels.iter() {
            if !self.has_dimension(name) {
                return Err(LabelError::UnknownDimension {
                    metric: self.name.clone(),
                    dimension: name.to_owned(),
                });
            }
            if value.is_empty() {
                return Err(LabelError::EmptyValue {
                    metric: self.name.clone(),
                    dimension: name.to_owned(),
                });
            }
        }

        match self
            .dimensions
            .iter()
            .find(|dimension| dimension.required && labels.get(&dimension.name).is_none())
        {
            Some(missing) => Err(LabelError::MissingDimension {
                metric: self.name.clone(),
                dimension: missing.name.clone(),
            }),
            None => Ok(()),
        }
    }

    /// The name an external time-series database export gives this metric: prefixed, dots as
    /// underscores, the unit's suffix when it has one, and `_total` for a counter.
    pub fn export_name(&self) -> String {
        let mut name = format!("{EXPORT_PREFIX}{}", self.name.replace('.', "_"));
        if let Some(suffix) = self.unit.export_suffix() {
            name.push('_');
            name.push_str(suffix);
        }
        if self.kind == MetricKind::Counter {
            name.push_str("_total");
        }
        name
    }

    /// Whether `name` is one of this metric's dimensions.
    pub fn has_dimension(&self, name: &str) -> bool {
        self.dimensions
            .iter()
            .any(|dimension| dimension.name == name)
    }

    /// How long a gauge reading stays valid after it was observed: its sample interval plus its
    /// reporting delay, so it lasts until the next reading is due at the latest.
    pub fn gauge_lookback(&self) -> Option<Duration> {
        self.sample_interval
            .map(|interval| interval + self.reporting_delay.unwrap_or(interval))
    }

    /// How long after a bucket ends its points stay provisional, waiting for samples still on
    /// their way. Manager-measured samples arrive at once; a node's report can trail its
    /// observation by up to the reporting delay, plus a second of transport.
    pub fn closing_grace(&self) -> Duration {
        match self.provenance {
            MetricProvenance::Manager => MANAGER_CLOSING_GRACE,
            MetricProvenance::Node => self.node_reporting_delay() + NODE_TRANSPORT_GRACE,
        }
    }

    /// How late a node's sample of this metric may come: the declared reporting delay, else the
    /// sample interval, else none for metrics without one.
    fn node_reporting_delay(&self) -> Duration {
        self.reporting_delay
            .or(self.sample_interval)
            .unwrap_or_default()
    }
}

/// Checks a whole metric catalog: every declaration is valid, and no two share a name or an
/// exported name.
pub fn validate_catalog(descriptors: &[MetricDescriptor]) -> Result<(), DescriptorError> {
    let mut names = BTreeMap::new();
    let mut export_names = BTreeMap::new();
    for descriptor in descriptors {
        descriptor.validate()?;
        if names.insert(descriptor.name.as_str(), ()).is_some() {
            return Err(DescriptorError::Duplicate {
                metric: descriptor.name.clone(),
            });
        }
        let export_name = descriptor.export_name();
        if let Some(other) = export_names.insert(export_name.clone(), descriptor.name.as_str()) {
            return Err(DescriptorError::ExportNameCollision {
                metric: descriptor.name.clone(),
                other: other.to_owned(),
                export_name,
            });
        }
    }
    Ok(())
}

/// How samples of a metric combine within a bucket.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MetricKind {
    /// A reading of a current value, such as available memory.
    Gauge,

    /// A count of events since the previous sample, such as requests served.
    Counter,

    /// Individual measurements whose spread matters, such as request latencies, counted in an
    /// exponential histogram of `scale` (3 keeps percentiles within about 4.4%).
    Distribution { scale: i8 },
}

/// Unit of a metric's sample values.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MetricUnit {
    /// Bytes of memory or storage.
    Bytes,

    /// Seconds of elapsed time.
    Seconds,

    /// Inference requests.
    Requests,

    /// Model tokens, such as prompt or generated tokens.
    Tokens,

    /// Model tokens processed or generated per second.
    TokensPerSecond,

    /// A dimensionless fraction from 0 to 1, such as the share of a bucket a node was observing.
    Ratio,

    /// Metric records, such as samples the store rejected.
    Records,

    /// Metric series the store holds.
    Series,

    /// Watts of electrical power.
    Watts,

    /// Degrees Celsius of temperature.
    DegreesCelsius,
}

impl MetricUnit {
    /// The unit's UCUM code, as OpenTelemetry declares units; countable things are annotations
    /// of the dimensionless unit, such as `{token}`.
    pub fn ucum_code(self) -> &'static str {
        match self {
            Self::Bytes => "By",
            Self::Seconds => "s",
            Self::Requests => "{request}",
            Self::Tokens => "{token}",
            Self::TokensPerSecond => "{token}/s",
            Self::Ratio => "1",
            Self::Records => "{record}",
            Self::Series => "{series}",
            Self::Watts => "W",
            Self::DegreesCelsius => "Cel",
        }
    }

    /// The suffix an export appends to the metric name, absent for an annotation-only unit.
    pub fn export_suffix(self) -> Option<&'static str> {
        match self {
            Self::Bytes => Some("bytes"),
            Self::Seconds => Some("seconds"),
            Self::TokensPerSecond => Some("per_second"),
            Self::Ratio => Some("ratio"),
            Self::Watts => Some("watts"),
            Self::DegreesCelsius => Some("celsius"),
            Self::Requests | Self::Tokens | Self::Records | Self::Series => None,
        }
    }
}

/// Where a metric is measured.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MetricProvenance {
    /// Measured in the site manager process, timestamped by its wall clock.
    Manager,

    /// Measured on a SiteNode and timestamped by that node's wall clock.
    Node,
}

/// One label dimension of a metric.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MetricDimension {
    /// Label name, such as `node`.
    pub name: String,

    /// Whether every series of the metric carries this label.
    pub required: bool,
}

impl MetricDimension {
    /// A dimension every series carries.
    pub fn required(name: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            required: true,
        }
    }

    /// A dimension a series may omit.
    pub fn optional(name: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            required: false,
        }
    }
}

/// An unusable metric declaration.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum DescriptorError {
    #[error("metric name {metric:?} is not dotted lowercase words")]
    InvalidName { metric: String },

    #[error("metric {metric} name carries the unit word {word:?}, which an export appends")]
    UnitInName { metric: String, word: String },

    #[error("counter metric {metric} name ends in count or total, which an export appends")]
    CounterSuffix { metric: String },

    #[error(
        "metric {metric} declares dimension {dimension:?}, which is not a lowercase label name"
    )]
    InvalidDimension { metric: String, dimension: String },

    #[error("metric {metric} declares dimension {dimension:?}, which Prometheus reserves")]
    ReservedDimension { metric: String, dimension: String },

    #[error("metric {metric} declares dimension {dimension} more than once")]
    DuplicateDimension { metric: String, dimension: String },

    #[error("gauge metric {metric} declares no sample interval")]
    MissingSampleInterval { metric: String },

    #[error("counter or distribution metric {metric} declares a sample interval")]
    UnexpectedSampleInterval { metric: String },

    #[error("metric {metric} declares a reporting delay but is not a node-measured gauge")]
    UnexpectedReportingDelay { metric: String },

    #[error("distribution metric {metric} declares unsupported histogram scale {scale}")]
    UnsupportedScale { metric: String, scale: i8 },

    #[error("metric {metric} is declared more than once")]
    Duplicate { metric: String },

    #[error("metrics {other} and {metric} both export as {export_name}")]
    ExportNameCollision {
        metric: String,
        other: String,
        export_name: String,
    },
}

/// A label set that does not fit its metric's declared dimensions.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum LabelError {
    #[error("metric {metric} has no dimension {dimension}")]
    UnknownDimension { metric: String, dimension: String },

    #[error("metric {metric} requires dimension {dimension}")]
    MissingDimension { metric: String, dimension: String },

    #[error("metric {metric} label {dimension} has an empty value")]
    EmptyValue { metric: String, dimension: String },
}

/// Prefix of every exported metric name.
const EXPORT_PREFIX: &str = "dotcore_";

/// Words an export appends as unit suffixes, so no dotted segment of a name is one of them.
const UNIT_WORDS: &[&str] = &["bytes", "seconds", "ratio", "watts", "celsius"];

/// Labels Prometheus gives histogram bucket and summary series, so a distribution never declares
/// them.
const HISTOGRAM_LABELS: &[&str] = &["le", "quantile"];

/// Last words a counter name may not end in, since an export appends `_total` to counters.
const COUNTER_SUFFIX_WORDS: &[&str] = &["count", "total"];

/// Closing grace of manager-measured metrics.
/// Not a behavior duration: part of the metric bucket closing contract.
const MANAGER_CLOSING_GRACE: Duration = Duration::from_millis(250);

/// Closing grace added to a node-reported metric's sample interval.
/// Not a behavior duration: part of the metric bucket closing contract.
const NODE_TRANSPORT_GRACE: Duration = Duration::from_secs(1);

/// Whether `name` fits the Prometheus label name charset, lowercase only: `[a-z_][a-z0-9_]*`.
fn is_label_name(name: &str) -> bool {
    let mut chars = name.chars();
    chars
        .next()
        .is_some_and(|first| first.is_ascii_lowercase() || first == '_')
        && chars.all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_')
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn labels_must_fit_the_declared_dimensions() {
        let descriptor = gauge_descriptor();

        assert_eq!(
            descriptor.validate_labels(&LabelSet::new().with("node", "n1").with("gpu", "0")),
            Err(LabelError::UnknownDimension {
                metric: "node.memory.used".to_owned(),
                dimension: "gpu".to_owned(),
            })
        );
        assert_eq!(
            descriptor.validate_labels(&LabelSet::new().with("device", "d0")),
            Err(LabelError::MissingDimension {
                metric: "node.memory.used".to_owned(),
                dimension: "node".to_owned(),
            })
        );
        assert_eq!(
            descriptor.validate_labels(&LabelSet::new().with("node", "")),
            Err(LabelError::EmptyValue {
                metric: "node.memory.used".to_owned(),
                dimension: "node".to_owned(),
            })
        );
        assert_eq!(
            descriptor.validate_labels(&LabelSet::new().with("node", "n1")),
            Ok(())
        );
        assert_eq!(
            descriptor.validate_labels(&LabelSet::new().with("node", "n1").with("device", "d0")),
            Ok(())
        );
    }

    #[test]
    fn declarations_are_validated() {
        assert_eq!(gauge_descriptor().validate(), Ok(()));

        let mut bad_name = gauge_descriptor();
        bad_name.name = "Node.Memory".to_owned();
        assert_eq!(
            bad_name.validate(),
            Err(DescriptorError::InvalidName {
                metric: "Node.Memory".to_owned(),
            })
        );

        let mut duplicate = gauge_descriptor();
        duplicate.dimensions.push(MetricDimension::optional("node"));
        assert_eq!(
            duplicate.validate(),
            Err(DescriptorError::DuplicateDimension {
                metric: "node.memory.used".to_owned(),
                dimension: "node".to_owned(),
            })
        );

        let mut gauge_without_interval = gauge_descriptor();
        gauge_without_interval.sample_interval = None;
        assert_eq!(
            gauge_without_interval.validate(),
            Err(DescriptorError::MissingSampleInterval {
                metric: "node.memory.used".to_owned(),
            })
        );

        let mut counter_with_interval = gauge_descriptor();
        counter_with_interval.kind = MetricKind::Counter;
        assert_eq!(
            counter_with_interval.validate(),
            Err(DescriptorError::UnexpectedSampleInterval {
                metric: "node.memory.used".to_owned(),
            })
        );

        let mut distribution = gauge_descriptor();
        distribution.kind = MetricKind::Distribution { scale: 3 };
        distribution.sample_interval = None;
        assert_eq!(distribution.validate(), Ok(()));
        distribution.kind = MetricKind::Distribution { scale: 9 };
        assert_eq!(
            distribution.validate(),
            Err(DescriptorError::UnsupportedScale {
                metric: "node.memory.used".to_owned(),
                scale: 9,
            })
        );
    }

    #[test]
    fn dimension_names_must_be_lowercase_label_names() {
        for name in ["", "Device", "1node", "node-id", "node.id"] {
            let mut descriptor = gauge_descriptor();
            descriptor.dimensions.push(MetricDimension::optional(name));

            assert_eq!(
                descriptor.validate(),
                Err(DescriptorError::InvalidDimension {
                    metric: "node.memory.used".to_owned(),
                    dimension: name.to_owned(),
                }),
                "{name:?}"
            );
        }

        let mut descriptor = gauge_descriptor();
        descriptor
            .dimensions
            .push(MetricDimension::optional("_target_kind2"));
        assert_eq!(descriptor.validate(), Ok(()));
    }

    #[test]
    fn dimension_names_prometheus_reserves_are_rejected() {
        let reserved = |descriptor: &MetricDescriptor, name: &str| {
            let mut descriptor = descriptor.clone();
            descriptor.dimensions.push(MetricDimension::optional(name));
            descriptor.validate()
        };
        let mut distribution = gauge_descriptor();
        distribution.kind = MetricKind::Distribution { scale: 3 };
        distribution.sample_interval = None;

        for (descriptor, name) in [
            (&gauge_descriptor(), "__name"),
            (&distribution, "__name__"),
            (&distribution, "le"),
            (&distribution, "quantile"),
        ] {
            assert_eq!(
                reserved(descriptor, name),
                Err(DescriptorError::ReservedDimension {
                    metric: "node.memory.used".to_owned(),
                    dimension: name.to_owned(),
                }),
                "{name}"
            );
        }

        // Histogram labels are reserved only where an export writes histogram series.
        assert_eq!(reserved(&gauge_descriptor(), "le"), Ok(()));
        assert_eq!(reserved(&gauge_descriptor(), "quantile"), Ok(()));
    }

    #[test]
    fn names_carry_no_unit_word_and_counters_no_total_suffix() {
        for (name, word) in [
            ("node.memory.bytes", "bytes"),
            ("router.request.seconds", "seconds"),
            ("node.accelerator.watts", "watts"),
            ("node.celsius.temperature", "celsius"),
            ("node.observing.ratio", "ratio"),
        ] {
            let mut descriptor = gauge_descriptor();
            descriptor.name = name.to_owned();

            assert_eq!(
                descriptor.validate(),
                Err(DescriptorError::UnitInName {
                    metric: name.to_owned(),
                    word: word.to_owned(),
                })
            );
        }

        for name in [
            "router.request.count",
            "router.request_total",
            "router.total",
        ] {
            let mut counter = counter_descriptor();
            counter.name = name.to_owned();

            assert_eq!(
                counter.validate(),
                Err(DescriptorError::CounterSuffix {
                    metric: name.to_owned(),
                }),
            );
        }

        // A unit word inside a segment is not a unit segment.
        let mut in_segment = gauge_descriptor();
        in_segment.name = "router.attempt.bytes_per_token".to_owned();
        assert_eq!(in_segment.validate(), Ok(()));

        // Only counters get the appended `_total`, so a gauge may end in `total`.
        let mut gauge = gauge_descriptor();
        gauge.name = "node.host.memory.total".to_owned();
        assert_eq!(gauge.validate(), Ok(()));
        assert_eq!(counter_descriptor().validate(), Ok(()));
    }

    #[test]
    fn export_names_add_the_prefix_the_unit_suffix_and_the_counter_total() {
        let mut power = gauge_descriptor();
        power.name = "node.accelerator.power".to_owned();
        power.unit = MetricUnit::Watts;
        let mut temperature = gauge_descriptor();
        temperature.name = "node.accelerator.temperature".to_owned();
        temperature.unit = MetricUnit::DegreesCelsius;

        assert_eq!(
            gauge_descriptor().export_name(),
            "dotcore_node_memory_used_bytes"
        );
        assert_eq!(power.export_name(), "dotcore_node_accelerator_power_watts");
        assert_eq!(
            temperature.export_name(),
            "dotcore_node_accelerator_temperature_celsius"
        );
        assert_eq!(
            counter_descriptor().export_name(),
            "dotcore_router_requests_total"
        );
    }

    #[test]
    fn every_unit_states_its_ucum_code() {
        assert_eq!(
            [
                MetricUnit::Bytes,
                MetricUnit::Seconds,
                MetricUnit::Requests,
                MetricUnit::Tokens,
                MetricUnit::TokensPerSecond,
                MetricUnit::Ratio,
                MetricUnit::Records,
                MetricUnit::Series,
                MetricUnit::Watts,
                MetricUnit::DegreesCelsius,
            ]
            .map(MetricUnit::ucum_code),
            [
                "By",
                "s",
                "{request}",
                "{token}",
                "{token}/s",
                "1",
                "{record}",
                "{series}",
                "W",
                "Cel"
            ]
        );
    }

    #[test]
    fn a_catalog_rejects_duplicate_names_and_colliding_export_names() {
        assert_eq!(
            validate_catalog(&[gauge_descriptor(), counter_descriptor()]),
            Ok(())
        );
        assert_eq!(
            validate_catalog(&[gauge_descriptor(), gauge_descriptor()]),
            Err(DescriptorError::Duplicate {
                metric: "node.memory.used".to_owned(),
            })
        );

        let mut colliding = gauge_descriptor();
        colliding.name = "node.memory_used".to_owned();
        assert_eq!(
            validate_catalog(&[gauge_descriptor(), colliding]),
            Err(DescriptorError::ExportNameCollision {
                metric: "node.memory_used".to_owned(),
                other: "node.memory.used".to_owned(),
                export_name: "dotcore_node_memory_used_bytes".to_owned(),
            })
        );

        let mut invalid = gauge_descriptor();
        invalid.sample_interval = None;
        assert_eq!(
            validate_catalog(&[invalid]),
            Err(DescriptorError::MissingSampleInterval {
                metric: "node.memory.used".to_owned(),
            })
        );
    }

    #[test]
    fn closing_grace_follows_where_the_metric_is_measured() {
        let mut descriptor = gauge_descriptor();
        assert_eq!(descriptor.closing_grace(), Duration::from_secs(6));

        descriptor.provenance = MetricProvenance::Manager;
        assert_eq!(descriptor.closing_grace(), Duration::from_millis(250));

        // A node counter's report trails only by transport.
        assert_eq!(
            MetricDescriptor {
                provenance: MetricProvenance::Node,
                ..counter_descriptor()
            }
            .closing_grace(),
            Duration::from_secs(1)
        );
    }

    #[test]
    fn a_reporting_delay_decouples_validity_and_grace_from_the_sample_interval() {
        let mut descriptor = gauge_descriptor();
        assert_eq!(descriptor.gauge_lookback(), Some(Duration::from_secs(10)));

        descriptor.sample_interval = Some(Duration::from_secs(60));
        descriptor.reporting_delay = Some(Duration::from_secs(5));
        assert_eq!(descriptor.validate(), Ok(()));
        assert_eq!(descriptor.gauge_lookback(), Some(Duration::from_secs(65)));
        assert_eq!(descriptor.closing_grace(), Duration::from_secs(6));
    }

    #[test]
    fn only_node_gauges_declare_a_reporting_delay() {
        let mut manager_gauge = gauge_descriptor();
        manager_gauge.provenance = MetricProvenance::Manager;
        let mut counter = counter_descriptor();
        counter.provenance = MetricProvenance::Node;

        for mut descriptor in [manager_gauge, counter] {
            descriptor.reporting_delay = Some(Duration::from_secs(5));
            assert_eq!(
                descriptor.validate(),
                Err(DescriptorError::UnexpectedReportingDelay {
                    metric: descriptor.name.clone(),
                })
            );
        }
    }

    fn counter_descriptor() -> MetricDescriptor {
        MetricDescriptor {
            name: "router.requests".to_owned(),
            kind: MetricKind::Counter,
            unit: MetricUnit::Requests,
            dimensions: vec![MetricDimension::required("outcome")],
            provenance: MetricProvenance::Manager,
            sample_interval: None,
            reporting_delay: None,
        }
    }

    fn gauge_descriptor() -> MetricDescriptor {
        MetricDescriptor {
            name: "node.memory.used".to_owned(),
            kind: MetricKind::Gauge,
            unit: MetricUnit::Bytes,
            dimensions: vec![
                MetricDimension::required("node"),
                MetricDimension::optional("device"),
            ],
            provenance: MetricProvenance::Node,
            sample_interval: Some(Duration::from_secs(5)),
            reporting_delay: None,
        }
    }
}
