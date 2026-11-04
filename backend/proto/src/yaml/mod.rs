//! Reads and writes a restricted subset of YAML as generated protobuf messages.
//!
//! A single YAML document (for example a site configuration file) is parsed into a protobuf type,
//! and a protobuf type is written back out as YAML. The parser accepts only the JSON-compatible
//! YAML subset and decodes through ProtoJSON, so documents outside that subset fail with an error
//! that names the offending line and column.

use std::{fs, path::Path};

use serde::{Serialize, de::DeserializeOwned};
use serde_json::{Map, Number, Value as JsonValue};
use serde_yaml_ng::Value as YamlValue;
use yaml_rust2::{
    parser::{Event, MarkedEventReceiver, Parser},
    scanner::Marker,
};

#[cfg(test)]
mod tests;

/// Reads one restricted YAML document into a generated protobuf type.
pub fn from_file<T: DeserializeOwned>(path: &Path) -> Result<T, RestrictedYamlError> {
    from_value(value_from_file(path)?)
}

/// Decodes one restricted YAML document through generated ProtoJSON support.
pub fn from_str<T: DeserializeOwned>(contents: &str) -> Result<T, RestrictedYamlError> {
    from_value(value_from_str(contents)?)
}

/// Reads one restricted YAML document into its JSON-compatible tree.
pub fn value_from_file(path: &Path) -> Result<JsonValue, RestrictedYamlError> {
    let contents = read_yaml_file(path)?;
    value_from_str(&contents)
}

/// Decodes one restricted YAML document into its JSON-compatible tree.
pub fn value_from_str(contents: &str) -> Result<JsonValue, RestrictedYamlError> {
    let yaml = parse_restricted_yaml(contents)?;
    convert_yaml_to_json(yaml)
}

/// Decodes a JSON-compatible tree through generated ProtoJSON support.
pub fn from_value<T: DeserializeOwned>(value: JsonValue) -> Result<T, RestrictedYamlError> {
    decode_protojson(value)
}

/// Serializes a generated protobuf type as canonical typed YAML.
pub fn to_string<T: Serialize>(value: &T) -> Result<String, RestrictedYamlError> {
    let json = serde_json::to_value(value)?;
    Ok(serde_yaml_ng::to_string(&json)?)
}

/// A location reported by the restricted YAML event parser.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct YamlLocation {
    pub line: usize,
    pub column: usize,
}

impl std::fmt::Display for YamlLocation {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            formatter,
            "parser marker line {} column {}",
            self.line, self.column
        )
    }
}

/// Failures while enforcing the restricted YAML and ProtoJSON contract.
#[derive(Debug, thiserror::Error)]
pub enum RestrictedYamlError {
    #[error("YAML file read error")]
    FileReadError(#[from] std::io::Error),

    #[error("YAML scan error")]
    YamlScanError(#[from] yaml_rust2::scanner::ScanError),

    #[error("YAML codec error")]
    YamlCodecError(#[from] serde_yaml_ng::Error),

    #[error("ProtoJSON codec error")]
    ProtoJsonCodecError(#[from] serde_json::Error),

    #[error(
        "Unsupported YAML document count: found {count} YAML documents{}",
        document_location(location)
    )]
    DocumentCount {
        count: usize,
        location: Option<YamlLocation>,
    },

    #[error("Unsupported YAML alias: alias referencing anchor {anchor}; {location}")]
    Alias {
        anchor: usize,
        location: YamlLocation,
    },

    #[error("Unsupported YAML anchor: anchor {anchor}; {location}")]
    Anchor {
        anchor: usize,
        location: YamlLocation,
    },

    #[error("Unsupported YAML tag: tag {tag}{}", tag_location(location))]
    Tag {
        tag: String,
        location: Option<YamlLocation>,
    },

    #[error(
        "Unsupported YAML map key: mapping key {} is not a string",
        yaml_map_key_description(key)
    )]
    NonStringMapKey { key: YamlValue },

    #[error("Unsupported YAML merge key")]
    MergeKey,

    #[error("Non-finite YAML number: YAML number {value} is not finite")]
    NonFiniteNumber { value: f64 },

    #[error("Unsupported YAML number representation")]
    UnsupportedNumber { value: serde_yaml_ng::Number },
}

fn document_location(location: &Option<YamlLocation>) -> String {
    location.map_or_else(String::new, |location| {
        format!("; additional document; {location}")
    })
}

fn tag_location(location: &Option<YamlLocation>) -> String {
    location.map_or_else(String::new, |location| format!("; {location}"))
}

fn yaml_map_key_description(key: &YamlValue) -> String {
    match key {
        YamlValue::Null => "null".to_owned(),
        YamlValue::Bool(value) => format!("boolean {value}"),
        YamlValue::Number(value) => format!("number {value}"),
        YamlValue::String(value) => format!("string {value}"),
        YamlValue::Sequence(_) => "sequence".to_owned(),
        YamlValue::Mapping(_) => "mapping".to_owned(),
        YamlValue::Tagged(_) => "tagged value".to_owned(),
    }
}

fn yaml_location(mark: Marker) -> YamlLocation {
    YamlLocation {
        line: mark.line(),
        column: mark.col(),
    }
}

fn read_yaml_file(path: &Path) -> Result<String, RestrictedYamlError> {
    Ok(fs::read_to_string(path)?)
}

fn parse_restricted_yaml(contents: &str) -> Result<YamlValue, RestrictedYamlError> {
    // Event inspection fences policy before the independent YAML decoder can accept unsupported syntax.
    inspect_events(contents)?;
    let documents = serde_yaml_ng::Deserializer::from_str(contents).count();
    if documents != 1 {
        return Err(RestrictedYamlError::DocumentCount {
            count: documents,
            location: None,
        });
    }
    Ok(serde_yaml_ng::from_str(contents)?)
}

fn decode_protojson<T: DeserializeOwned>(json: JsonValue) -> Result<T, RestrictedYamlError> {
    Ok(serde_json::from_value(json)?)
}

fn inspect_events(contents: &str) -> Result<(), RestrictedYamlError> {
    let mut inspection = YamlInspection::default();
    Parser::new_from_str(contents).load(&mut inspection, true)?;
    if let Err(error) = inspection.rejection {
        Err(error)
    } else if inspection.documents != 1 {
        Err(RestrictedYamlError::DocumentCount {
            count: inspection.documents,
            location: inspection.additional_document.map(yaml_location),
        })
    } else {
        Ok(())
    }
}

struct YamlInspection {
    documents: usize,
    additional_document: Option<Marker>,
    rejection: Result<(), RestrictedYamlError>,
}

impl Default for YamlInspection {
    fn default() -> Self {
        Self {
            documents: 0,
            additional_document: None,
            rejection: Ok(()),
        }
    }
}

impl MarkedEventReceiver for YamlInspection {
    fn on_event(&mut self, event: Event, mark: Marker) {
        if self.rejection.is_err() {
            return;
        }
        match event {
            Event::DocumentStart => {
                self.documents += 1;
                if self.documents == 2 {
                    self.additional_document = Some(mark);
                }
            }
            Event::Alias(anchor) => {
                self.rejection = Err(RestrictedYamlError::Alias {
                    anchor,
                    location: yaml_location(mark),
                });
            }
            Event::Scalar(_, _, anchor, tag)
            | Event::SequenceStart(anchor, tag)
            | Event::MappingStart(anchor, tag) => {
                if anchor != 0 {
                    self.rejection = Err(RestrictedYamlError::Anchor {
                        anchor,
                        location: yaml_location(mark),
                    });
                } else if let Some(tag) = tag {
                    self.rejection = Err(RestrictedYamlError::Tag {
                        tag: format!("{}{}", tag.handle, tag.suffix),
                        location: Some(yaml_location(mark)),
                    });
                }
            }
            _ => {}
        }
    }
}

fn convert_yaml_to_json(value: YamlValue) -> Result<JsonValue, RestrictedYamlError> {
    match value {
        YamlValue::Null => Ok(JsonValue::Null),
        YamlValue::Bool(value) => Ok(JsonValue::Bool(value)),
        YamlValue::Number(value) => yaml_number(value).map(JsonValue::Number),
        YamlValue::String(value) => Ok(JsonValue::String(value)),
        YamlValue::Sequence(values) => values
            .into_iter()
            .map(convert_yaml_to_json)
            .collect::<Result<Vec<_>, _>>()
            .map(JsonValue::Array),
        YamlValue::Mapping(values) => {
            let mut object = Map::new();
            for (key, value) in values {
                let key = match key {
                    YamlValue::String(key) => key,
                    key => return Err(RestrictedYamlError::NonStringMapKey { key }),
                };
                if key == "<<" {
                    return Err(RestrictedYamlError::MergeKey);
                }
                object.insert(key, convert_yaml_to_json(value)?);
            }
            Ok(JsonValue::Object(object))
        }
        YamlValue::Tagged(tagged) => Err(RestrictedYamlError::Tag {
            tag: tagged.tag.to_string(),
            location: None,
        }),
    }
}

fn yaml_number(value: serde_yaml_ng::Number) -> Result<Number, RestrictedYamlError> {
    if let Some(value) = value.as_u64() {
        Ok(Number::from(value))
    } else if let Some(value) = value.as_i64() {
        Ok(Number::from(value))
    } else if let Some(value) = value.as_f64() {
        Number::from_f64(value).ok_or(RestrictedYamlError::NonFiniteNumber { value })
    } else {
        // serde_yaml_ng::Number stores only u64, i64, or f64 values; each is handled above.
        // Keep this fallback to detect an upstream representation expansion.
        Err(RestrictedYamlError::UnsupportedNumber { value })
    }
}
