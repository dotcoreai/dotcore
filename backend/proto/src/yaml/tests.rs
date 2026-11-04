//! Contract tests for restricted YAML decoding and canonical encoding.

use std::{error::Error, path::Path};

use serde::{Serialize, Serializer, ser::Error as SerdeError};
use yaml_rust2::{
    parser::{Event, MarkedEventReceiver, Parser},
    scanner::Marker,
};

use super::*;
use crate::dotcore::catalog::v1::{License, Model};

#[test]
fn parser_rejects_anchors_and_non_string_keys_directly() {
    let anchor =
        from_str::<Model>("modelId: &identity model\n").expect_err("YAML anchors must be rejected");
    let RestrictedYamlError::Anchor {
        anchor: id,
        location,
    } = &anchor
    else {
        panic!("expected YAML anchor rejection, got {anchor}");
    };
    assert_eq!(*id, 1);
    assert_eq!(
        *location,
        YamlLocation {
            line: 1,
            column: 19,
        }
    );
    assert_eq!(
        anchor.to_string(),
        "Unsupported YAML anchor: anchor 1; parser marker line 1 column 19"
    );
    assert!(
        anchor.source().is_none(),
        "YAML policy rejections must not retain synthetic sources"
    );

    let non_string_key =
        from_str::<Model>("1: model\n").expect_err("non-string YAML keys must be rejected");
    let RestrictedYamlError::NonStringMapKey { key } = &non_string_key else {
        panic!("expected non-string map key rejection, got {non_string_key}");
    };
    assert_eq!(*key, serde_yaml_ng::Value::Number(1.into()));
    assert_eq!(
        non_string_key.to_string(),
        "Unsupported YAML map key: mapping key number 1 is not a string"
    );
    assert!(
        non_string_key.source().is_none(),
        "YAML policy rejections must not retain synthetic sources"
    );
}

#[test]
fn parser_rejects_aliases_with_parser_event_data() {
    let mut events = AliasEvents::default();
    Parser::new_from_str("modelId: &identity model\ndisplayName: *identity\n")
        .load(&mut events, true)
        .expect("YAML with an anchor and alias must parse into events");
    let (anchor, marker) = events.alias.expect("parser must emit the alias event");

    let mut inspection = YamlInspection::default();
    inspection.on_event(Event::Alias(anchor), marker);
    let error = inspection
        .rejection
        .expect_err("alias parser event must be rejected");

    let RestrictedYamlError::Alias { anchor, location } = error else {
        panic!("expected YAML alias rejection, got {error}");
    };
    assert_eq!(anchor, 1);
    assert_eq!(
        location,
        YamlLocation {
            line: 2,
            column: 13
        }
    );
    assert_eq!(
        error.to_string(),
        "Unsupported YAML alias: alias referencing anchor 1; parser marker line 2 column 13"
    );
    assert!(
        error.source().is_none(),
        "YAML policy rejections must not retain synthetic sources"
    );
}

#[test]
fn policy_variants_preserve_structured_data_without_sources() {
    let policies = [
        RestrictedYamlError::DocumentCount {
            count: 2,
            location: Some(YamlLocation { line: 3, column: 0 }),
        },
        RestrictedYamlError::Alias {
            anchor: 7,
            location: YamlLocation { line: 3, column: 4 },
        },
        RestrictedYamlError::Anchor {
            anchor: 1,
            location: YamlLocation {
                line: 1,
                column: 19,
            },
        },
        RestrictedYamlError::Tag {
            tag: "!custom".to_owned(),
            location: Some(YamlLocation {
                line: 1,
                column: 17,
            }),
        },
        RestrictedYamlError::NonStringMapKey {
            key: serde_yaml_ng::Value::Number(1.into()),
        },
        RestrictedYamlError::MergeKey,
        RestrictedYamlError::NonFiniteNumber { value: f64::NAN },
        RestrictedYamlError::UnsupportedNumber { value: 1.into() },
    ];

    assert!(matches!(
        &policies[0],
        RestrictedYamlError::DocumentCount {
            count: 2,
            location: Some(YamlLocation { line: 3, column: 0 }),
        }
    ));
    assert!(matches!(
        &policies[1],
        RestrictedYamlError::Alias {
            anchor: 7,
            location: YamlLocation { line: 3, column: 4 },
        }
    ));
    assert!(matches!(
        &policies[2],
        RestrictedYamlError::Anchor {
            anchor: 1,
            location: YamlLocation {
                line: 1,
                column: 19,
            },
        }
    ));
    assert!(matches!(
        &policies[3],
        RestrictedYamlError::Tag {
            tag,
            location: Some(YamlLocation {
                line: 1,
                column: 17,
            }),
        } if tag == "!custom"
    ));
    assert!(matches!(
        &policies[4],
        RestrictedYamlError::NonStringMapKey {
            key: serde_yaml_ng::Value::Number(_),
        }
    ));
    assert!(matches!(&policies[5], RestrictedYamlError::MergeKey));
    assert!(matches!(
        &policies[6],
        RestrictedYamlError::NonFiniteNumber { value } if value.is_nan()
    ));
    assert!(matches!(
        &policies[7],
        RestrictedYamlError::UnsupportedNumber { .. }
    ));

    for policy in policies {
        assert!(
            policy.source().is_none(),
            "YAML policy rejections must not retain synthetic sources"
        );
    }
}

#[test]
fn independent_parser_inspection_precedes_yaml_codec_errors() {
    let error = from_str::<Model>("modelId: &identity model\nmodelId: second\n")
        .expect_err("anchors must be rejected before duplicate-key decoding");

    assert_eq!(
        error.to_string(),
        "Unsupported YAML anchor: anchor 1; parser marker line 1 column 19"
    );
}

#[test]
fn parser_rejects_merge_key_even_without_alias_expansion() {
    let error = from_str::<Model>("<<: {}\n").expect_err("merge key must fail before ProtoJSON");
    assert!(matches!(error, RestrictedYamlError::MergeKey));
    assert_eq!(error.to_string(), "Unsupported YAML merge key");
    assert!(
        error.source().is_none(),
        "YAML policy rejections must not retain synthetic sources"
    );
}

#[test]
fn parser_rejects_multiple_documents_as_application_policy() {
    let error = from_str::<Model>("---\nmodelId: first\n---\nmodelId: second\n")
        .expect_err("multiple YAML documents must be rejected");

    let RestrictedYamlError::DocumentCount { count, location } = &error else {
        panic!("expected YAML document count rejection, got {error}");
    };
    assert_eq!(*count, 2);
    assert_eq!(*location, Some(YamlLocation { line: 3, column: 0 }));
    assert_eq!(
        error.to_string(),
        "Unsupported YAML document count: found 2 YAML documents; additional document; parser marker line 3 column 0"
    );
    assert!(
        error.source().is_none(),
        "YAML policy rejections must not retain synthetic sources"
    );
}

#[test]
fn parser_rejects_tags_with_construct_location_context() {
    let error =
        from_str::<Model>("modelId: !custom model\n").expect_err("YAML tags must be rejected");

    let RestrictedYamlError::Tag { tag, location } = &error else {
        panic!("expected YAML tag rejection, got {error}");
    };
    assert_eq!(tag, "!custom");
    assert_eq!(
        *location,
        Some(YamlLocation {
            line: 1,
            column: 17,
        })
    );
    assert_eq!(
        error.to_string(),
        "Unsupported YAML tag: tag !custom; parser marker line 1 column 17"
    );
    assert!(
        error.source().is_none(),
        "YAML policy rejections must not retain synthetic sources"
    );
}

#[test]
fn non_finite_numbers_include_the_rejected_value() {
    let error = yaml_number(serde_yaml_ng::Number::from(f64::NAN))
        .expect_err("non-finite YAML numbers must be rejected");

    let RestrictedYamlError::NonFiniteNumber { value } = &error else {
        panic!("expected non-finite YAML number rejection, got {error}");
    };
    assert!(value.is_nan());
    assert_eq!(
        error.to_string(),
        "Non-finite YAML number: YAML number NaN is not finite"
    );
    assert!(
        error.source().is_none(),
        "YAML policy rejections must not retain synthetic sources"
    );
}

#[test]
fn yaml_number_accepts_every_serde_yaml_number_representation() {
    assert_eq!(
        yaml_number(u64::MAX.into()).expect("unsigned YAML number"),
        serde_json::Number::from(u64::MAX)
    );
    assert_eq!(
        yaml_number(i64::MIN.into()).expect("signed YAML number"),
        serde_json::Number::from(i64::MIN)
    );
    assert_eq!(
        yaml_number(1.5.into()).expect("floating-point YAML number"),
        serde_json::Number::from_f64(1.5).expect("finite JSON number")
    );
}

#[test]
fn canonical_yaml_round_trips_typed_values() {
    let source = "version: \"1\"\ndisplayName: Demo\nmodelId: model\n";
    let model = from_str::<Model>(source).expect("typed YAML should decode");
    let canonical = to_string(&model).expect("typed value should encode");

    assert_eq!(
        canonical,
        "displayName: Demo\nmodelId: model\nversion: '1'\n"
    );
    assert_eq!(
        model,
        from_str::<Model>(&canonical).expect("canonical YAML should decode")
    );
}

#[test]
fn model_license_decodes_in_both_spellings_and_stays_optional() {
    let camel = from_str::<Model>("license:\n  id: MIT\n  termsUrl: https://example.test/mit\n")
        .expect("camelCase licence should decode");
    let snake = from_str::<Model>("license:\n  id: MIT\n  terms_url: https://example.test/mit\n")
        .expect("snake_case licence should decode");
    let expected = License {
        id: "MIT".to_owned(),
        terms_url: "https://example.test/mit".to_owned(),
    };

    assert_eq!(camel.license, Some(expected));
    assert_eq!(camel, snake);
    assert_eq!(
        from_str::<Model>("modelId: model\n")
            .expect("model without a licence should decode")
            .license,
        None
    );
}

#[test]
fn canonical_yaml_normalizes_input_key_order() {
    let first = from_str::<Model>("version: \"1\"\ndisplayName: Demo\nmodelId: model\n")
        .expect("first typed YAML should decode");
    let second = from_str::<Model>("modelId: model\ndisplayName: Demo\nversion: \"1\"\n")
        .expect("second typed YAML should decode");

    assert_eq!(
        to_string(&first).expect("first value should encode"),
        to_string(&second).expect("second value should encode")
    );
}

#[test]
fn file_errors_preserve_the_typed_filesystem_source() {
    let path = Path::new("/tmp/dotcore-yaml-source-chain-missing.yaml");
    let error = from_file::<Model>(path).expect_err("missing YAML input must fail");

    let RestrictedYamlError::FileReadError(source) = &error else {
        panic!("expected read error, got {error}");
    };
    assert_eq!(source.kind(), std::io::ErrorKind::NotFound);
    assert_eq!(error.to_string(), "YAML file read error");
    assert_eq!(
        error
            .source()
            .and_then(|value| value.downcast_ref::<std::io::Error>())
            .map(std::io::Error::kind),
        Some(std::io::ErrorKind::NotFound)
    );
}

#[test]
fn scan_errors_preserve_the_typed_yaml_scanner_source() {
    let error = from_str::<Model>("modelId: [\n").expect_err("invalid YAML must fail");

    let RestrictedYamlError::YamlScanError(source) = &error else {
        panic!("expected YAML scan error, got {error}");
    };
    let expected =
        "while parsing a node, did not find expected node content at byte 11 line 2 column 1";
    assert_eq!(source.to_string(), expected);
    assert_eq!(error.to_string(), "YAML scan error");
    assert_eq!(
        error
            .source()
            .and_then(|value| value.downcast_ref::<yaml_rust2::scanner::ScanError>())
            .map(ToString::to_string),
        Some(expected.to_owned())
    );
}

#[test]
fn yaml_codec_errors_preserve_the_typed_serde_yaml_source() {
    let error = from_str::<Model>("modelId: first\nmodelId: second\n")
        .expect_err("duplicate YAML keys must fail");

    let RestrictedYamlError::YamlCodecError(source) = &error else {
        panic!("expected YAML codec error, got {error}");
    };
    assert_eq!(source.to_string(), "duplicate entry with key \"modelId\"");
    assert_eq!(error.to_string(), "YAML codec error");
    assert_eq!(
        error
            .source()
            .and_then(|value| value.downcast_ref::<serde_yaml_ng::Error>())
            .map(ToString::to_string),
        Some("duplicate entry with key \"modelId\"".to_owned())
    );
}

#[test]
fn protojson_errors_preserve_the_typed_serde_json_source() {
    let error =
        from_str::<Model>("unknownField: value\n").expect_err("unknown ProtoJSON fields must fail");

    let RestrictedYamlError::ProtoJsonCodecError(source) = &error else {
        panic!("expected ProtoJSON codec error, got {error}");
    };
    let expected = "unknown field `unknownField`, expected one of `model_id`, `modelId`, `display_name`, `displayName`, `version`, `maximum_context_tokens`, `maximumContextTokens`, `artifacts`, `profiles`, `auxiliary_artifacts`, `auxiliaryArtifacts`, `configurations`, `options`, `intelligence`, `license`";
    assert_eq!(source.to_string(), expected);
    assert_eq!(error.to_string(), "ProtoJSON codec error");
    assert_eq!(
        error
            .source()
            .and_then(|value| value.downcast_ref::<serde_json::Error>())
            .map(ToString::to_string),
        Some(expected.to_owned())
    );
}

#[derive(Default)]
struct AliasEvents {
    alias: Option<(usize, Marker)>,
}

impl MarkedEventReceiver for AliasEvents {
    fn on_event(&mut self, event: Event, marker: Marker) {
        if let Event::Alias(anchor) = event {
            self.alias = Some((anchor, marker));
        }
    }
}

struct FailingSerialize;

impl Serialize for FailingSerialize {
    fn serialize<S>(&self, _serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        Err(SerdeError::custom("serialize failure"))
    }
}

#[test]
fn serialization_errors_preserve_the_typed_serde_json_source() {
    let error = to_string(&FailingSerialize).expect_err("serialization must fail");

    let RestrictedYamlError::ProtoJsonCodecError(source) = &error else {
        panic!("expected ProtoJSON codec error, got {error}");
    };
    assert_eq!(source.to_string(), "serialize failure");
    assert_eq!(error.to_string(), "ProtoJSON codec error");
    assert_eq!(
        error
            .source()
            .and_then(|value| value.downcast_ref::<serde_json::Error>())
            .map(ToString::to_string),
        Some("serialize failure".to_owned())
    );
}
