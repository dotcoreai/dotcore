//! Validates the canonical decision request shared by every inference entry point.
//!
//! Route resolution and worker admission apply the same platform rules, so a forwarded decision
//! request never carries more than a public one, and wire decoders keep only their own format
//! rules. Errors name only fixed public request paths and never echo client question or option
//! names.

use std::collections::BTreeSet;

use dotcore_proto::dotcore::inference::v1::{
    DecisionContentPart, DecisionRequest, decision_content_part::Content, decision_question::Kind,
};
use serde::de::IgnoredAny;

use crate::inference::InvalidRequest;

/// Largest state, summed over its content parts' text and JSON source.
const MAX_STATE_BYTES: usize = 1024 * 1024;

/// Most questions one request may ask.
const MAX_QUESTIONS: usize = 128;

/// Most options one choice question may offer.
const MAX_OPTIONS: usize = 256;

/// Fewest and most levels of a score question.
const SCORE_LEVELS: std::ops::RangeInclusive<usize> = 2..=10;

/// Validates the platform-owned shape of a canonical decision request before any target sees it.
pub(crate) fn validate(request: &DecisionRequest) -> Result<(), InvalidRequest> {
    validate_state(&request.state)?;
    if request.questions.is_empty() {
        return Err(InvalidRequest::invalid_field(Some("questions")));
    }
    if request.questions.len() > MAX_QUESTIONS {
        return Err(InvalidRequest::request_limit(Some("questions")));
    }
    let mut names = BTreeSet::new();
    for question in &request.questions {
        if question.name.is_empty() || !names.insert(question.name.as_str()) {
            return Err(InvalidRequest::invalid_field(Some("questions")));
        }
        validate_parts(&question.instructions, "instructions")?;
        match &question.kind {
            Some(Kind::Predicate(predicate)) => {
                for description in [&predicate.true_description, &predicate.false_description]
                    .into_iter()
                    .flatten()
                {
                    validate_part(description, "criteria")?;
                }
            }
            Some(Kind::Choice(choice)) => {
                if choice.options.is_empty() {
                    return Err(InvalidRequest::invalid_field(Some("criteria")));
                }
                if choice.options.len() > MAX_OPTIONS {
                    return Err(InvalidRequest::request_limit(Some("criteria")));
                }
                let mut options = BTreeSet::new();
                for option in &choice.options {
                    if option.name.is_empty() || !options.insert(option.name.as_str()) {
                        return Err(InvalidRequest::invalid_field(Some("criteria")));
                    }
                    if let Some(description) = &option.description {
                        validate_part(description, "criteria")?;
                    }
                }
            }
            Some(Kind::Score(score)) => {
                if !SCORE_LEVELS.contains(&score.levels.len()) {
                    return Err(InvalidRequest::invalid_field(Some("criteria")));
                }
                for level in &score.levels {
                    let description = level
                        .description
                        .as_ref()
                        .ok_or(InvalidRequest::invalid_field(Some("criteria")))?;
                    validate_part(description, "criteria")?;
                }
            }
            None => return Err(InvalidRequest::invalid_field(Some("questions"))),
        }
    }
    Ok(())
}

fn validate_state(state: &[DecisionContentPart]) -> Result<(), InvalidRequest> {
    if state.is_empty() {
        return Err(InvalidRequest::invalid_field(Some("state")));
    }
    validate_parts(state, "state")?;
    let size = state
        .iter()
        .map(|part| match &part.content {
            Some(Content::TextContent(text) | Content::JsonContent(text)) => text.len(),
            None => 0,
        })
        .sum::<usize>();
    if size > MAX_STATE_BYTES {
        return Err(InvalidRequest::request_limit(Some("state")));
    }
    Ok(())
}

fn validate_parts(
    parts: &[DecisionContentPart],
    param: &'static str,
) -> Result<(), InvalidRequest> {
    parts.iter().try_for_each(|part| validate_part(part, param))
}

/// A part must carry content, and JSON content must be one well-formed JSON value.
fn validate_part(part: &DecisionContentPart, param: &'static str) -> Result<(), InvalidRequest> {
    let valid = match &part.content {
        Some(Content::TextContent(_)) => true,
        Some(Content::JsonContent(json)) => serde_json::from_str::<IgnoredAny>(json).is_ok(),
        None => false,
    };
    if valid {
        Ok(())
    } else {
        Err(InvalidRequest::invalid_field(Some(param)))
    }
}

#[cfg(test)]
mod tests {
    use dotcore_proto::dotcore::inference::v1::{
        DecisionChoice, DecisionOption, DecisionPredicate, DecisionQuestion, DecisionScore,
        DecisionScoreLevel,
    };

    use super::*;

    #[test]
    fn a_request_asking_each_question_kind_is_valid() {
        assert_eq!(validate(&valid_request()), Ok(()));
    }

    #[test]
    fn state_must_be_present_well_formed_and_bounded() {
        let cases = [
            (Vec::new(), InvalidRequest::invalid_field(Some("state"))),
            (
                vec![DecisionContentPart { content: None }],
                InvalidRequest::invalid_field(Some("state")),
            ),
            (
                vec![json(r#"{"open":"#)],
                InvalidRequest::invalid_field(Some("state")),
            ),
            (
                vec![text(&"a".repeat(MAX_STATE_BYTES)), json("1")],
                InvalidRequest::request_limit(Some("state")),
            ),
        ];

        for (state, expected) in cases {
            let request = DecisionRequest {
                state,
                ..valid_request()
            };
            assert_eq!(validate(&request), Err(expected));
        }
        let at_limit = DecisionRequest {
            state: vec![text(&"a".repeat(MAX_STATE_BYTES - 1)), json("1")],
            ..valid_request()
        };
        assert_eq!(validate(&at_limit), Ok(()));
    }

    #[test]
    fn questions_must_be_present_named_unique_typed_and_bounded() {
        let mut empty_name = valid_request();
        empty_name.questions[0].name.clear();
        let mut duplicate = valid_request();
        duplicate.questions[1].name = duplicate.questions[0].name.clone();
        let mut untyped = valid_request();
        untyped.questions[0].kind = None;
        let mut too_many = valid_request();
        too_many.questions = (0..=MAX_QUESTIONS)
            .map(|index| predicate(&format!("question-{index}")))
            .collect();
        let mut bad_instructions = valid_request();
        bad_instructions.questions[0].instructions = vec![json("not json")];

        for (request, expected) in [
            (
                DecisionRequest {
                    questions: Vec::new(),
                    ..valid_request()
                },
                InvalidRequest::invalid_field(Some("questions")),
            ),
            (empty_name, InvalidRequest::invalid_field(Some("questions"))),
            (duplicate, InvalidRequest::invalid_field(Some("questions"))),
            (untyped, InvalidRequest::invalid_field(Some("questions"))),
            (too_many, InvalidRequest::request_limit(Some("questions"))),
            (
                bad_instructions,
                InvalidRequest::invalid_field(Some("instructions")),
            ),
        ] {
            assert_eq!(validate(&request), Err(expected));
        }
        let mut at_limit = valid_request();
        at_limit.questions = (0..MAX_QUESTIONS)
            .map(|index| predicate(&format!("question-{index}")))
            .collect();
        assert_eq!(validate(&at_limit), Ok(()));
    }

    #[test]
    fn choice_options_must_be_present_named_unique_and_bounded() {
        let options = |names: &[&str]| -> Vec<DecisionOption> {
            names
                .iter()
                .map(|name| DecisionOption {
                    name: (*name).to_owned(),
                    description: None,
                })
                .collect()
        };
        let too_many = (0..=MAX_OPTIONS)
            .map(|index| DecisionOption {
                name: format!("option-{index}"),
                description: None,
            })
            .collect();
        let bad_description = vec![DecisionOption {
            name: "a".to_owned(),
            description: Some(DecisionContentPart { content: None }),
        }];

        for (options, expected) in [
            (Vec::new(), InvalidRequest::invalid_field(Some("criteria"))),
            (
                options(&["a", ""]),
                InvalidRequest::invalid_field(Some("criteria")),
            ),
            (
                options(&["a", "b", "a"]),
                InvalidRequest::invalid_field(Some("criteria")),
            ),
            (too_many, InvalidRequest::request_limit(Some("criteria"))),
            (
                bad_description,
                InvalidRequest::invalid_field(Some("criteria")),
            ),
        ] {
            assert_eq!(
                validate(&with_kind(Kind::Choice(DecisionChoice { options }))),
                Err(expected)
            );
        }
        assert_eq!(
            validate(&with_kind(Kind::Choice(DecisionChoice {
                options: (0..MAX_OPTIONS)
                    .map(|index| DecisionOption {
                        name: format!("option-{index}"),
                        description: None,
                    })
                    .collect(),
            }))),
            Ok(())
        );
    }

    #[test]
    fn score_levels_must_number_two_to_ten_and_be_described() {
        let levels = |count: usize| {
            (0..count)
                .map(|index| DecisionScoreLevel {
                    label: None,
                    description: Some(text(&format!("level {index}"))),
                })
                .collect::<Vec<_>>()
        };
        let mut undescribed = levels(3);
        undescribed[1].description = None;

        for (levels, expected) in [
            (
                levels(1),
                Err(InvalidRequest::invalid_field(Some("criteria"))),
            ),
            (levels(2), Ok(())),
            (levels(10), Ok(())),
            (
                levels(11),
                Err(InvalidRequest::invalid_field(Some("criteria"))),
            ),
            (
                undescribed,
                Err(InvalidRequest::invalid_field(Some("criteria"))),
            ),
        ] {
            assert_eq!(
                validate(&with_kind(Kind::Score(DecisionScore { levels }))),
                expected
            );
        }
    }

    #[test]
    fn predicate_descriptions_must_be_well_formed() {
        assert_eq!(
            validate(&with_kind(Kind::Predicate(DecisionPredicate {
                true_description: Some(text("yes")),
                false_description: Some(json("[")),
            }))),
            Err(InvalidRequest::invalid_field(Some("criteria")))
        );
    }

    fn valid_request() -> DecisionRequest {
        DecisionRequest {
            model_id: "route".to_owned(),
            state: vec![json(r#"{"ticket":"refund please"}"#)],
            questions: vec![
                predicate("urgent"),
                DecisionQuestion {
                    name: "category".to_owned(),
                    instructions: vec![text("Pick the category.")],
                    kind: Some(Kind::Choice(DecisionChoice {
                        options: vec![
                            DecisionOption {
                                name: "billing".to_owned(),
                                description: Some(text("Payments")),
                            },
                            DecisionOption {
                                name: "other".to_owned(),
                                description: None,
                            },
                        ],
                    })),
                },
                DecisionQuestion {
                    name: "severity".to_owned(),
                    instructions: vec![text("Rate the severity.")],
                    kind: Some(Kind::Score(DecisionScore {
                        levels: vec![
                            DecisionScoreLevel {
                                label: Some("low".to_owned()),
                                description: Some(text("Minor")),
                            },
                            DecisionScoreLevel {
                                label: None,
                                description: Some(text("Blocking")),
                            },
                        ],
                    })),
                },
            ],
        }
    }

    fn with_kind(kind: Kind) -> DecisionRequest {
        let mut request = valid_request();
        request.questions[0].kind = Some(kind);
        request
    }

    fn predicate(name: &str) -> DecisionQuestion {
        DecisionQuestion {
            name: name.to_owned(),
            instructions: vec![text("Is it urgent?")],
            kind: Some(Kind::Predicate(DecisionPredicate::default())),
        }
    }

    fn text(value: &str) -> DecisionContentPart {
        DecisionContentPart {
            content: Some(Content::TextContent(value.to_owned())),
        }
    }

    fn json(value: &str) -> DecisionContentPart {
        DecisionContentPart {
            content: Some(Content::JsonContent(value.to_owned())),
        }
    }
}
