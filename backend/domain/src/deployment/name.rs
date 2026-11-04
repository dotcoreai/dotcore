//! Shared validation for mutable deployment presentation names.

/// Normalizes a deployment presentation name at the owning controller boundary.
pub fn normalize(value: &str) -> Result<String, DeploymentNameError> {
    let value = value.trim();
    if value.is_empty() {
        return Err(DeploymentNameError::Empty);
    }
    if value.chars().count() > 128 {
        return Err(DeploymentNameError::TooLong);
    }
    if value.chars().any(char::is_control) {
        return Err(DeploymentNameError::ControlCharacter);
    }
    Ok(value.to_owned())
}

/// Normalizes an optional name, applying the owning catalog-derived default when omitted.
pub fn normalize_or_default(
    value: Option<&str>,
    default: &str,
) -> Result<String, DeploymentNameError> {
    normalize(value.unwrap_or(default))
}

/// Deployment presentation-name validation failures.
#[derive(Clone, Copy, Debug, thiserror::Error)]
pub enum DeploymentNameError {
    #[error("deployment name must contain at least one character")]
    Empty,

    #[error("deployment name must contain at most 128 characters")]
    TooLong,

    #[error("deployment name must not contain control characters")]
    ControlCharacter,
}

#[cfg(test)]
mod tests {
    use super::{normalize, normalize_or_default};

    #[test]
    fn normalizes_valid_unicode_presentation_names() {
        assert_eq!(
            normalize("  Punctuated 名称!  ").expect("valid name"),
            "Punctuated 名称!"
        );
    }

    #[test]
    fn normalizes_padded_catalog_defaults() {
        assert_eq!(
            normalize_or_default(None, "  Catalog Model  ").expect("valid default"),
            "Catalog Model"
        );
    }

    #[test]
    fn rejects_catalog_defaults_with_controls_or_129_scalars() {
        for value in ["Catalog\u{0007}Model", &"x".repeat(129)] {
            assert!(
                normalize_or_default(None, value).is_err(),
                "{value:?} must be rejected"
            );
        }
    }

    #[test]
    fn rejects_blank_control_and_too_long_presentation_names() {
        for value in [" \t", "name\nvalue", &"x".repeat(129)] {
            assert!(normalize(value).is_err(), "{value:?} must be rejected");
        }
    }
}
