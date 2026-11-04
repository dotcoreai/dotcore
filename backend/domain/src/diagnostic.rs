//! Renders a failure with its retained causes as one bounded line for a log event, for example
//! `model configuration failed: classified operation failure: leaf cause`.
//!
//! Causes are read through the standard error interface, so chains built with `anyhow` context
//! render in full. Every rendered value is bounded in bytes and cut on a character boundary.

/// Longest rendered failure chain, in bytes.
const DIAGNOSTIC_LIMIT_BYTES: usize = 2_048;

/// Renders one failure and every retained cause for a backend log event.
///
/// The rendered value joins the failure with each cause in order, separated by
/// `": "`, and is bounded to a fixed byte length. Causes are read through the
/// standard error interface, so a chain built with `anyhow` context is rendered
/// in full rather than only its outermost message. A cause whose text is
/// already visible in the rendered value is skipped, because error messages
/// commonly inline their source and repeating it only adds noise.
pub fn diagnostic_chain(error: &(dyn std::error::Error + 'static)) -> String {
    let mut rendered = error.to_string();
    let mut next = error.source();
    while let Some(cause) = next {
        let text = cause.to_string();
        if !text.is_empty() && !rendered.contains(&text) {
            rendered.push_str(": ");
            rendered.push_str(&text);
        }
        next = cause.source();
    }
    truncate_utf8_with_ellipsis(&rendered, DIAGNOSTIC_LIMIT_BYTES)
}

/// Shortens a value to a byte limit on a character boundary, marking the cut.
pub fn truncate_utf8_with_ellipsis(value: &str, max_bytes: usize) -> String {
    const ELLIPSIS: &str = "…";

    if value.len() <= max_bytes {
        return value.to_owned();
    }
    let mut end = max_bytes.saturating_sub(ELLIPSIS.len());
    while !value.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}{ELLIPSIS}", &value[..end])
}

#[cfg(test)]
mod tests {
    use std::io;

    use super::{DIAGNOSTIC_LIMIT_BYTES, diagnostic_chain, truncate_utf8_with_ellipsis};

    #[test]
    fn diagnostic_chain_renders_every_retained_cause() {
        let error = OuterFailure {
            source: io::Error::other("leaf cause"),
        };

        assert_eq!(
            diagnostic_chain(&error),
            "outer operation failed: leaf cause"
        );
    }

    #[test]
    fn diagnostic_chain_reaches_causes_behind_anyhow_contexts() {
        let error = AnyhowFailure {
            source: anyhow::Error::new(io::Error::other("leaf cause"))
                .context("classified operation failure"),
        };

        assert_eq!(
            diagnostic_chain(&error),
            "model configuration failed: classified operation failure: leaf cause"
        );
    }

    #[test]
    fn diagnostic_chain_does_not_repeat_an_inlined_cause() {
        let error = InlinedFailure {
            source: anyhow::anyhow!("estimation failed"),
        };

        assert_eq!(
            diagnostic_chain(&error),
            "resource estimation failed: estimation failed"
        );
    }

    #[test]
    fn diagnostic_chain_bounds_long_failures_on_a_character_boundary() {
        let error = OuterFailure {
            source: io::Error::other("é".repeat(2_048)),
        };

        let rendered = diagnostic_chain(&error);

        assert!(
            rendered.len() <= DIAGNOSTIC_LIMIT_BYTES,
            "{}",
            rendered.len()
        );
        assert!(
            rendered.len() > DIAGNOSTIC_LIMIT_BYTES - 8,
            "{}",
            rendered.len()
        );
        assert!(rendered.ends_with('…'), "{rendered}");
    }

    #[test]
    fn truncate_utf8_with_ellipsis_keeps_character_boundaries() {
        assert_eq!(truncate_utf8_with_ellipsis("abc", 3), "abc");
        assert_eq!(truncate_utf8_with_ellipsis("abcdef", 5), "ab…");
        // A cut inside a two-byte character backs up to the previous boundary.
        assert_eq!(truncate_utf8_with_ellipsis("ééé", 4), "…");
        assert_eq!(truncate_utf8_with_ellipsis("ééé", 5), "é…");
    }

    #[derive(Debug, thiserror::Error)]
    #[error("outer operation failed")]
    struct OuterFailure {
        #[source]
        source: io::Error,
    }

    #[derive(Debug, thiserror::Error)]
    #[error("model configuration failed")]
    struct AnyhowFailure {
        #[source]
        source: anyhow::Error,
    }

    #[derive(Debug, thiserror::Error)]
    #[error("resource estimation failed: {source}")]
    struct InlinedFailure {
        #[source]
        source: anyhow::Error,
    }
}
