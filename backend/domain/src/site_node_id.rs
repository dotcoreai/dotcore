//! Validated SiteNode identity for authored backend domain paths.
//!
//! Backend code receives a SiteNode identity as a plain string from `node.yaml`,
//! command arguments, protobuf, or persisted state, and parses it once through this module,
//! for example `local-node-1`. Wire, YAML, persistence, and diagnostic boundaries
//! keep plain strings and project the parsed identity back through a borrowed view.

use std::{fmt, str::FromStr};

use crate::valid_id;

/// A SiteNode identity that satisfies the shared site identifier rule.
///
/// It carries the identity of one declared SiteNode and keeps the accepted
/// `[a-z][a-z0-9-]{0,62}` grammar checked at every entry point, so backend code
/// cannot mix it with an unrelated raw identifier.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct SiteNodeId(String);

impl SiteNodeId {
    /// Parses one candidate SiteNode identity and rejects values outside the
    /// shared site identifier rule.
    pub fn parse(value: &str) -> Result<Self, SiteNodeIdError> {
        Self::from_verified(value.to_owned())
    }

    /// Returns the identity as a borrowed string for protocol, lookup, and
    /// diagnostic boundaries.
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// Builds a trusted unit-test identity through the same validation that
    /// production parsing applies, so test fixtures can never hold a value the
    /// backend would reject at its ingress boundaries.
    ///
    /// # Panics
    ///
    /// Panics when `value` does not satisfy the shared site identifier rule.
    #[cfg(any(test, feature = "test-support"))]
    pub fn for_test(value: &str) -> Self {
        Self::parse(value).expect("valid node identity")
    }

    fn from_verified(value: String) -> Result<Self, SiteNodeIdError> {
        if !valid_id(&value) {
            return Err(SiteNodeIdError::Invalid { value });
        }
        Ok(Self(value))
    }
}

impl fmt::Display for SiteNodeId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

impl FromStr for SiteNodeId {
    type Err = SiteNodeIdError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        Self::parse(value)
    }
}

impl TryFrom<String> for SiteNodeId {
    type Error = SiteNodeIdError;

    fn try_from(value: String) -> Result<Self, Self::Error> {
        Self::from_verified(value)
    }
}

impl TryFrom<&str> for SiteNodeId {
    type Error = SiteNodeIdError;

    fn try_from(value: &str) -> Result<Self, Self::Error> {
        Self::parse(value)
    }
}

/// A candidate SiteNode identity does not satisfy the site identifier rule.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum SiteNodeIdError {
    /// The rejected identity does not satisfy `[a-z][a-z0-9-]{0,62}`.
    #[error("node identity {value:?} must match [a-z][a-z0-9-]{{0,62}}")]
    Invalid {
        /// Rejected identity.
        value: String,
    },
}

#[cfg(test)]
mod tests {
    use std::{collections::BTreeMap, str::FromStr};

    use super::{SiteNodeId, SiteNodeIdError};

    #[test]
    fn accepts_the_shortest_and_longest_site_node_identities() {
        let longest = format!("a{}", "0".repeat(62));
        for value in ["a", longest.as_str()] {
            let node_id = SiteNodeId::parse(value).expect("valid node identity");
            assert_eq!(node_id.as_str(), value);
            assert_eq!(node_id.to_string(), value);
        }
    }

    #[test]
    fn rejects_identities_outside_the_site_identifier_rule() {
        let overlong = format!("a{}", "0".repeat(63));
        for value in ["", "Local-node", "local_node", overlong.as_str()] {
            let error = SiteNodeId::parse(value).expect_err("invalid node identity");
            assert_eq!(
                error,
                SiteNodeIdError::Invalid {
                    value: value.to_owned()
                }
            );
            assert_eq!(
                error.to_string(),
                format!("node identity {value:?} must match [a-z][a-z0-9-]{{0,62}}")
            );
        }
    }

    #[test]
    fn string_conversions_share_the_site_identifier_rule() {
        let invalid = SiteNodeIdError::Invalid {
            value: "Local-node".to_owned(),
        };
        assert_eq!(
            SiteNodeId::from_str("local-node-1")
                .expect("valid node identity")
                .as_str(),
            "local-node-1"
        );
        assert_eq!(
            SiteNodeId::try_from("local-node-1")
                .expect("valid node identity")
                .as_str(),
            "local-node-1"
        );
        assert_eq!(
            SiteNodeId::try_from("local-node-1".to_owned())
                .expect("valid node identity")
                .as_str(),
            "local-node-1"
        );
        assert_eq!(SiteNodeId::from_str("Local-node"), Err(invalid.clone()));
        assert_eq!(SiteNodeId::try_from("Local-node"), Err(invalid.clone()));
        assert_eq!(SiteNodeId::try_from("Local-node".to_owned()), Err(invalid));
    }

    #[test]
    fn identities_order_and_hash_as_distinct_map_keys() {
        let node_a = SiteNodeId::for_test("node-a");
        let node_b = SiteNodeId::for_test("node-b");
        let mut nodes = BTreeMap::new();
        nodes.insert(node_b.clone(), "second");
        nodes.insert(node_a.clone(), "first");
        assert_eq!(nodes.keys().collect::<Vec<_>>(), vec![&node_a, &node_b]);
        assert_eq!(nodes.get(&SiteNodeId::for_test("node-a")), Some(&"first"));
        assert_eq!(nodes.get(&SiteNodeId::for_test("node-b")), Some(&"second"));
        assert_ne!(node_a, node_b);
    }

    #[test]
    fn trusted_test_construction_uses_the_site_identifier_rule() {
        assert_eq!(
            SiteNodeId::for_test("local-node-1").as_str(),
            "local-node-1"
        );
    }

    #[test]
    #[should_panic(expected = "valid node identity")]
    fn trusted_test_construction_panics_on_an_invalid_identity() {
        SiteNodeId::for_test("Local-node");
    }
}
