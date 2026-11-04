//! Defines the shared syntax for callable inference route names.
//!
//! Names begin with a lowercase letter and contain at most 63 lowercase letters, digits, hyphens,
//! or periods, such as `claude-3.5-sonnet`.

/// Returns whether a value matches the callable inference route-name grammar.
pub fn is_valid_route_name(name: &str) -> bool {
    let bytes = name.as_bytes();
    (1..=63).contains(&bytes.len())
        && bytes.first().is_some_and(u8::is_ascii_lowercase)
        && bytes.iter().all(|byte| {
            byte.is_ascii_lowercase() || byte.is_ascii_digit() || matches!(*byte, b'-' | b'.')
        })
}

#[cfg(test)]
mod tests {
    use super::is_valid_route_name;

    #[test]
    fn route_names_allow_periods_without_widening_other_constraints() {
        assert!(is_valid_route_name("claude-3.5-sonnet"));
        assert!(is_valid_route_name("a"));
        assert!(is_valid_route_name(&format!("a{}", "b".repeat(62))));
        assert!(!is_valid_route_name(".claude"));
        assert!(!is_valid_route_name("3.5-model"));
        assert!(!is_valid_route_name("Claude-3.5"));
        assert!(!is_valid_route_name("model_name"));
        assert!(!is_valid_route_name(&format!("a{}", "b".repeat(63))));
    }
}
