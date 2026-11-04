//! The stable identifier grammar shared by catalog and site records, for example `qwen3-8b` or
//! `node-1`.
//!
//! An identifier is 1 to 63 bytes of lowercase ASCII letters, digits, and hyphens, and starts
//! with a lowercase letter, so it is safe in file names, labels, and URLs.

/// Validates the common stable identifier form used by catalog and site records.
pub fn valid_id(value: &str) -> bool {
    let bytes = value.as_bytes();
    (1..=63).contains(&bytes.len())
        && bytes.first().is_some_and(u8::is_ascii_lowercase)
        && bytes
            .iter()
            .all(|value| value.is_ascii_lowercase() || value.is_ascii_digit() || *value == b'-')
}

#[cfg(test)]
mod tests {
    use super::valid_id;

    #[test]
    fn accepts_lowercase_identifiers_up_to_63_bytes() {
        assert!(valid_id("a"));
        assert!(valid_id("qwen3-8b"));
        assert!(valid_id(&format!("a{}", "-".repeat(62))));
    }

    #[test]
    fn rejects_empty_long_or_non_canonical_identifiers() {
        assert!(!valid_id(""));
        assert!(!valid_id(&"a".repeat(64)));
        assert!(!valid_id("1node"));
        assert!(!valid_id("-node"));
        assert!(!valid_id("Node"));
        assert!(!valid_id("node_1"));
        assert!(!valid_id("node.1"));
    }
}
