//! Identifies one series of a metric by its label values, such as `node=gpu-host-1`.
//!
//! Labels hold stable identities, never display names, and compare and order by value so series
//! group and sort deterministically.

use std::collections::BTreeMap;

/// Dimension naming the SiteNode a metric describes or was measured on.
pub const NODE: &str = "node";

/// Dimension naming an accelerator device of a node.
pub const DEVICE: &str = "device";

/// Dimension naming a model or application deployment; the metric name says which kind.
pub const DEPLOYMENT: &str = "deployment";

/// Dimension naming one service of an application deployment.
pub const SERVICE: &str = "service";

/// Dimension naming a catalog model.
pub const MODEL: &str = "model";

/// Dimension naming an inference route.
pub const ROUTE: &str = "route";

/// Store dimension naming the metric a health count is about.
pub const METRIC: &str = "metric";

/// Store dimension naming why a record was rejected or a bucket dropped.
pub const REASON: &str = "reason";

/// Store dimension telling whether a late record was a backfill.
pub const BACKFILL: &str = "backfill";

/// Dimension naming the writer a node's observing window belongs to.
pub const SOURCE: &str = "source";

/// Label values identifying one series of a metric.
#[derive(Debug, Clone, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct LabelSet(BTreeMap<String, String>);

impl LabelSet {
    /// An empty label set.
    pub fn new() -> Self {
        Self::default()
    }

    /// Returns this set with `name` set to `value`.
    pub fn with(mut self, name: impl Into<String>, value: impl Into<String>) -> Self {
        self.0.insert(name.into(), value.into());
        self
    }

    /// The value of label `name`, when present.
    pub fn get(&self, name: &str) -> Option<&str> {
        self.0.get(name).map(String::as_str)
    }

    /// Every label as `(name, value)`, ordered by name.
    pub fn iter(&self) -> impl Iterator<Item = (&str, &str)> {
        self.0
            .iter()
            .map(|(name, value)| (name.as_str(), value.as_str()))
    }

    /// Whether every `(name, value)` matcher equals this set's label.
    pub fn matches(&self, matchers: &BTreeMap<String, String>) -> bool {
        matchers
            .iter()
            .all(|(name, value)| self.get(name) == Some(value.as_str()))
    }

    /// The subset of labels named in `names`; absent labels stay absent.
    pub fn project(&self, names: &[String]) -> Self {
        Self(
            names
                .iter()
                .filter_map(|name| Some((name.clone(), self.0.get(name)?.clone())))
                .collect(),
        )
    }
}

impl<K: Into<String>, V: Into<String>> FromIterator<(K, V)> for LabelSet {
    fn from_iter<I: IntoIterator<Item = (K, V)>>(labels: I) -> Self {
        Self(
            labels
                .into_iter()
                .map(|(name, value)| (name.into(), value.into()))
                .collect(),
        )
    }
}
