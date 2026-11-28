//! Inputs every deploy command shares: the payload text, `--set` overrides and the plain values
//! they carry.
//!
//! A payload or an override holds a value as written, a boolean, a number or text. It gets its
//! parameter's type here, so a mistyped value fails naming the parameter before anything is sent.
//! Ranges, choices and completeness stay with the server, which validates every deployment.

use std::{
    fmt,
    io::{self, Read},
    path::{Path, PathBuf},
};

use anyhow::Context;
use serde::{Deserialize, Serialize};

/// A value as a payload or an override writes it, before it gets its parameter's type.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(untagged)]
pub(crate) enum PlainValue {
    /// A YAML boolean.
    Boolean(bool),

    /// A YAML number that is a non-negative integer.
    Number(u64),

    /// Any other scalar, and every `--set` value.
    Text(String),
}

impl PlainValue {
    /// The value with the given type: a boolean from `true` or `false`, an integer from its
    /// decimal digits, and text from any scalar. Fails with what the type expects.
    pub(crate) fn typed(&self, kind: ValueKind) -> Result<TypedValue, ValueError> {
        match (kind, self) {
            (ValueKind::Boolean, Self::Boolean(value)) => Ok(TypedValue::Boolean(*value)),
            (ValueKind::Boolean, Self::Text(text)) => match text.as_str() {
                "true" => Ok(TypedValue::Boolean(true)),
                "false" => Ok(TypedValue::Boolean(false)),
                _ => Err(ValueError::Expected(kind)),
            },
            (ValueKind::UnsignedInteger, Self::Number(value)) => {
                Ok(TypedValue::UnsignedInteger(*value))
            }
            (ValueKind::UnsignedInteger, Self::Text(text)) => text
                .parse()
                .map(TypedValue::UnsignedInteger)
                .map_err(|_| ValueError::Expected(kind)),
            (ValueKind::Text, Self::Text(text)) => Ok(TypedValue::Text(text.clone())),
            (ValueKind::Text, Self::Number(value)) => Ok(TypedValue::Text(value.to_string())),
            (ValueKind::Text, Self::Boolean(value)) => Ok(TypedValue::Text(value.to_string())),
            (ValueKind::Boolean | ValueKind::UnsignedInteger, _) => Err(ValueError::Expected(kind)),
        }
    }
}

/// The type a parameter gives its value.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum ValueKind {
    /// `true` or `false`.
    Boolean,

    /// A non-negative whole number.
    UnsignedInteger,

    /// Text: a choice, free text or a secret.
    Text,
}

impl fmt::Display for ValueKind {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::Boolean => "true or false",
            Self::UnsignedInteger => "a whole number",
            Self::Text => "text",
        })
    }
}

/// A value with its parameter's type.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum TypedValue {
    /// A boolean value.
    Boolean(bool),

    /// An unsigned integer value.
    UnsignedInteger(u64),

    /// A text value.
    Text(String),
}

impl From<TypedValue> for PlainValue {
    fn from(value: TypedValue) -> Self {
        match value {
            TypedValue::Boolean(value) => Self::Boolean(value),
            TypedValue::UnsignedInteger(value) => Self::Number(value),
            TypedValue::Text(value) => Self::Text(value),
        }
    }
}

/// Why a value doesn't have its parameter's type.
#[derive(Debug, thiserror::Error)]
pub(crate) enum ValueError {
    /// The value is of another type than the parameter's.
    #[error("expects {0}")]
    Expected(ValueKind),
}

/// One `--set <id>=<value>` override, applied over the payload or the starting values.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct Setting {
    /// What the override sets, such as a parameter ID.
    pub(crate) key: String,

    /// The value as written.
    pub(crate) value: PlainValue,
}

impl Setting {
    /// Parses `<key>=<value>` for clap; the value may itself contain `=`.
    pub(crate) fn parse(text: &str) -> Result<Self, String> {
        match text.split_once('=') {
            Some((key, value)) if !key.is_empty() => Ok(Self {
                key: key.to_owned(),
                value: PlainValue::Text(value.to_owned()),
            }),
            _ => Err("expected <id>=<value>".to_owned()),
        }
    }
}

/// Where a payload is read from: a file, or stdin for `-`.
#[derive(Clone, Debug)]
pub(crate) struct PayloadSource(PathBuf);

impl PayloadSource {
    /// Reads the whole payload text.
    pub(crate) fn read(&self) -> anyhow::Result<String> {
        let mut text = String::new();
        if self.0 == Path::new("-") {
            io::stdin()
                .read_to_string(&mut text)
                .context("read the payload from stdin")?;
        } else {
            text = std::fs::read_to_string(&self.0)
                .with_context(|| format!("read the payload file {}", self.0.display()))?;
        }
        Ok(text)
    }
}

impl From<PathBuf> for PayloadSource {
    fn from(path: PathBuf) -> Self {
        Self(path)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plain_values_take_their_parameter_type() {
        let cases = [
            (
                PlainValue::Boolean(true),
                ValueKind::Boolean,
                TypedValue::Boolean(true),
            ),
            (
                PlainValue::Text("false".into()),
                ValueKind::Boolean,
                TypedValue::Boolean(false),
            ),
            (
                PlainValue::Number(4),
                ValueKind::UnsignedInteger,
                TypedValue::UnsignedInteger(4),
            ),
            (
                PlainValue::Text("32768".into()),
                ValueKind::UnsignedInteger,
                TypedValue::UnsignedInteger(32768),
            ),
            (
                PlainValue::Text("f16".into()),
                ValueKind::Text,
                TypedValue::Text("f16".into()),
            ),
            (
                PlainValue::Number(8),
                ValueKind::Text,
                TypedValue::Text("8".into()),
            ),
            (
                PlainValue::Boolean(true),
                ValueKind::Text,
                TypedValue::Text("true".into()),
            ),
        ];
        for (plain, kind, typed) in cases {
            assert_eq!(
                plain.typed(kind).expect("typed"),
                typed,
                "{plain:?} as {kind:?}"
            );
        }
    }

    #[test]
    fn a_value_of_another_type_says_what_the_parameter_expects() {
        let cases = [
            (
                PlainValue::Text("yes".into()),
                ValueKind::Boolean,
                "expects true or false",
            ),
            (
                PlainValue::Number(1),
                ValueKind::Boolean,
                "expects true or false",
            ),
            (
                PlainValue::Text("-1".into()),
                ValueKind::UnsignedInteger,
                "expects a whole number",
            ),
            (
                PlainValue::Boolean(true),
                ValueKind::UnsignedInteger,
                "expects a whole number",
            ),
        ];
        for (plain, kind, message) in cases {
            let error = plain.typed(kind).expect_err("mistyped");
            assert_eq!(error.to_string(), message, "{plain:?} as {kind:?}");
        }
    }

    #[test]
    fn a_setting_splits_at_the_first_equals_sign() {
        assert_eq!(
            Setting::parse("custom=a=b").expect("setting"),
            Setting {
                key: "custom".into(),
                value: PlainValue::Text("a=b".into()),
            }
        );
        assert_eq!(
            Setting::parse("=1").expect_err("no key"),
            "expected <id>=<value>"
        );
        assert_eq!(
            Setting::parse("total").expect_err("no value"),
            "expected <id>=<value>"
        );
    }
}
