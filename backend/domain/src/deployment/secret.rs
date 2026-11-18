//! A deployment secret value, such as an app password or its derived hash.
//!
//! Holding a secret in this type keeps it out of debug output, log lines and error text: both
//! formatting traits print a redaction marker. Code reads the plaintext only through an explicit
//! accessor, where it hands the value to a workload, a client or the deployment record.

use std::fmt;

/// A secret value a deployment holds, printed redacted by `Debug` and `Display`.
///
/// Equality compares plaintext and is not constant-time: secrets are compared to detect a changed
/// launch, never to authenticate a caller.
#[derive(Clone, Eq, PartialEq)]
pub struct SecretValue(String);

impl SecretValue {
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }

    /// Returns the plaintext, for the code that hands it to a workload, a client or the record.
    pub fn expose(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for SecretValue {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("SecretValue(<redacted>)")
    }
}

impl fmt::Display for SecretValue {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("<redacted>")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn formatting_never_prints_the_plaintext() {
        let secret = SecretValue::new("hunter2-plaintext");

        assert_eq!(format!("{secret:?}"), "SecretValue(<redacted>)");
        assert_eq!(format!("{secret}"), "<redacted>");
        assert_eq!(
            format!("{:?}", Some(vec![secret.clone()])),
            "Some([SecretValue(<redacted>)])"
        );
        assert_eq!(secret.expose(), "hunter2-plaintext");
    }
}
