//! System One decision wire codec, in the dialect llama.cpp serves at `/v1/systemone`.
//!
//! The public route decodes System One requests and encodes their results, and the engine
//! transport encodes requests for an engine and decodes its answers, so both share one wire
//! mapping. Question and option order are kept, JSON values keep their exact source, and a missing
//! derived answer field is filled by the decision contract's rules. Errors name only fixed public
//! fields, never caller-supplied question or option names.

use std::{collections::HashSet, fmt};

use anyhow::{Context as _, anyhow};
use dotcore_proto::dotcore::inference::v1::{
    DecisionAnswer, DecisionChoice, DecisionChoiceAnswer, DecisionContentPart, DecisionOption,
    DecisionPredicate, DecisionPredicateAnswer, DecisionQuestion, DecisionRequest, DecisionResult,
    DecisionScore, DecisionScoreAnswer, DecisionScoreLevel, DecisionUsage, decision_answer::Answer,
    decision_content_part::Content, decision_question::Kind,
};
use serde::de::{Deserialize, Deserializer, MapAccess, Visitor};
use serde_json::{Map, Value, value::RawValue};

use crate::inference::{
    InvalidRequest,
    decision::{expected_level, most_probable, predicate_value},
};

/// Decodes one System One request body into the canonical decision request, leaving contract
/// rules such as size limits to the shared decision validator.
pub(crate) fn decode_request(body: &[u8]) -> Result<DecisionRequest, RequestDecodeError> {
    let fields = serde_json::from_slice::<OrderedObject>(body)
        .map_err(|_| RequestDecodeError::InvalidBody)?;
    let mut model_id = None;
    let mut state = None;
    let mut questions = None;
    for (name, value) in fields.0 {
        match name.as_str() {
            "model" => model_id = Some(decode_model(&value)?),
            "state" => state = Some(decode_state(&value)?),
            "questions" => questions = Some(decode_questions(&value)?),
            "stream" => decode_stream(&value)?,
            "images" => return Err(InvalidRequest::unsupported_feature(Some("images")).into()),
            _ => return Err(InvalidRequest::unsupported_feature(None).into()),
        }
    }
    Ok(DecisionRequest {
        model_id: model_id.ok_or(InvalidRequest::invalid_field(Some("model")))?,
        state: state.ok_or(InvalidRequest::invalid_field(Some("state")))?,
        questions: questions.ok_or(InvalidRequest::invalid_field(Some("questions")))?,
    })
}

/// Encodes a canonical decision request as a System One request for `model`.
///
/// System One takes the state as one value, so a request whose state has several parts is
/// refused. Instruction parts join as paragraphs, since System One takes one instruction string.
pub(crate) fn encode_request(
    request: &DecisionRequest,
    model: &str,
) -> Result<Vec<u8>, InvalidRequest> {
    let [state] = request.state.as_slice() else {
        return Err(InvalidRequest::unsupported_feature(Some("state")));
    };
    let mut body = String::from("{\"model\":");
    push_json(&mut body, model);
    body.push_str(",\"state\":");
    push_part(&mut body, Some(state));
    body.push_str(",\"questions\":{");
    for (index, question) in request.questions.iter().enumerate() {
        if index > 0 {
            body.push(',');
        }
        push_json(&mut body, &question.name);
        body.push(':');
        push_question(&mut body, question)?;
    }
    body.push_str("}}");
    Ok(body.into_bytes())
}

/// Encodes a decision result as a System One response for `model`.
///
/// Answers are keyed by the request's question names in request order, and score legends come
/// from the request's levels. A refusal or an answer that does not fit its question has no
/// System One form.
pub(crate) fn encode_result(
    request: &DecisionRequest,
    result: &DecisionResult,
    model: &str,
) -> Result<Vec<u8>, ResultEncodeError> {
    if result.answers.len() != request.questions.len() {
        return Err(ResultEncodeError::Mismatch);
    }
    let mut body = String::from("{\"model\":");
    push_json(&mut body, model);
    body.push_str(",\"answers\":{");
    for (index, (question, answer)) in request.questions.iter().zip(&result.answers).enumerate() {
        if index > 0 {
            body.push(',');
        }
        push_json(&mut body, &question.name);
        body.push(':');
        push_answer(&mut body, question, answer.answer.as_ref())?;
    }
    body.push('}');
    if let Some(usage) = &result.usage {
        body.push_str(",\"usage\":{\"input_tokens\":");
        push_json(&mut body, &usage.input_tokens);
        body.push_str(",\"output_tokens\":0}");
    }
    body.push('}');
    Ok(body.into_bytes())
}

/// Decodes a System One response into one answer per request question, in request order, with
/// the engine's probabilities as primary data.
///
/// Answers are found by question name; errors never name a question or option, since those are
/// client data.
pub(crate) fn decode_result(
    request: &DecisionRequest,
    body: &[u8],
) -> anyhow::Result<DecisionResult> {
    let response = serde_json::from_slice::<Map<String, Value>>(body)
        .context("System One response is not a JSON object")?;
    let answers = response
        .get("answers")
        .and_then(Value::as_object)
        .ok_or_else(|| anyhow!("System One response has no answers object"))?;
    let answers = request
        .questions
        .iter()
        .map(|question| {
            let answer = answers
                .get(&question.name)
                .and_then(Value::as_object)
                .ok_or_else(|| anyhow!("System One response misses an answer"))?;
            decode_answer(question, answer)
        })
        .collect::<anyhow::Result<Vec<_>>>()?;
    let usage = response
        .get("usage")
        .and_then(|usage| usage.get("input_tokens"))
        .and_then(Value::as_u64)
        .map(|input_tokens| DecisionUsage { input_tokens });
    Ok(DecisionResult { answers, usage })
}

/// A System One request body that cannot become a canonical decision request.
#[derive(Debug, thiserror::Error)]
pub(crate) enum RequestDecodeError {
    #[error("System One request body is not a JSON object")]
    InvalidBody,

    #[error(transparent)]
    Invalid(#[from] InvalidRequest),
}

/// A decision result that has no System One answer form.
#[derive(Debug, thiserror::Error)]
pub(crate) enum ResultEncodeError {
    #[error("the model refused a question, which System One cannot answer")]
    Refusal,

    #[error("the decision answers do not match the request questions")]
    Mismatch,
}

impl ResultEncodeError {
    /// The safe failure stage reported for this error.
    pub(crate) fn stage(&self) -> &'static str {
        match self {
            Self::Refusal => "decision_refusal",
            Self::Mismatch => "unexpected_result",
        }
    }
}

fn decode_model(value: &RawValue) -> Result<String, InvalidRequest> {
    match serde_json::from_str::<Value>(value.get()) {
        Ok(Value::String(model)) if !model.is_empty() => Ok(model),
        _ => Err(InvalidRequest::invalid_field(Some("model"))),
    }
}

/// Decodes the state: a string is one text part, any other JSON value one JSON part.
fn decode_state(value: &RawValue) -> Result<Vec<DecisionContentPart>, InvalidRequest> {
    part(value, "state")?
        .map(|part| vec![part])
        .ok_or(InvalidRequest::invalid_field(Some("state")))
}

/// Accepts only an absent or false stream flag, since decisions are never streamed.
fn decode_stream(value: &RawValue) -> Result<(), InvalidRequest> {
    match serde_json::from_str::<Value>(value.get()) {
        Ok(Value::Null | Value::Bool(false)) => Ok(()),
        Ok(Value::Bool(true)) => Err(InvalidRequest::unsupported_feature(Some("stream"))),
        _ => Err(InvalidRequest::invalid_field(Some("stream"))),
    }
}

fn decode_questions(value: &RawValue) -> Result<Vec<DecisionQuestion>, InvalidRequest> {
    let questions = serde_json::from_str::<OrderedObject>(value.get())
        .map_err(|_| InvalidRequest::invalid_field(Some("questions")))?;
    questions
        .0
        .into_iter()
        .map(|(name, question)| decode_question(name, &question))
        .collect()
}

fn decode_question(name: String, value: &RawValue) -> Result<DecisionQuestion, InvalidRequest> {
    let fields = serde_json::from_str::<OrderedObject>(value.get())
        .map_err(|_| InvalidRequest::invalid_field(Some("questions")))?;
    let mut type_name = None;
    let mut instructions = None;
    let mut criteria = None;
    for (field, value) in fields.0 {
        match field.as_str() {
            "type" => type_name = Some(value),
            "instructions" => instructions = Some(value),
            "criteria" => criteria = Some(value),
            _ => return Err(InvalidRequest::unsupported_feature(Some("questions"))),
        }
    }
    let instructions = match instructions.map(|value| serde_json::from_str::<Value>(value.get())) {
        Some(Ok(Value::String(text))) => vec![text_part(text)],
        _ => return Err(InvalidRequest::invalid_field(Some("instructions"))),
    };
    let type_name = type_name.map(|value| serde_json::from_str::<Value>(value.get()));
    let kind = match type_name {
        Some(Ok(Value::String(type_name))) => match type_name.as_str() {
            "noul" => Kind::Predicate(decode_predicate(criteria.as_deref())?),
            "choice" => Kind::Choice(decode_choice(criteria.as_deref())?),
            "score" => Kind::Score(decode_score(criteria.as_deref())?),
            _ => return Err(InvalidRequest::unsupported_feature(Some("type"))),
        },
        _ => return Err(InvalidRequest::invalid_field(Some("type"))),
    };
    Ok(DecisionQuestion {
        name,
        instructions,
        kind: Some(kind),
    })
}

/// Decodes optional `true` and `false` descriptions of a yes or no question.
fn decode_predicate(criteria: Option<&RawValue>) -> Result<DecisionPredicate, InvalidRequest> {
    let mut predicate = DecisionPredicate::default();
    let Some(criteria) = criteria.filter(|criteria| criteria.get() != "null") else {
        return Ok(predicate);
    };
    for (answer, description) in criteria_entries(criteria)? {
        let description = part(&description, "criteria")?;
        match answer.as_str() {
            "true" => predicate.true_description = description,
            "false" => predicate.false_description = description,
            _ => return Err(InvalidRequest::invalid_field(Some("criteria"))),
        }
    }
    Ok(predicate)
}

/// Decodes named options in order; a null description leaves the option undescribed.
fn decode_choice(criteria: Option<&RawValue>) -> Result<DecisionChoice, InvalidRequest> {
    let criteria = criteria.ok_or(InvalidRequest::invalid_field(Some("criteria")))?;
    Ok(DecisionChoice {
        options: criteria_entries(criteria)?
            .into_iter()
            .map(|(name, description)| {
                Ok(DecisionOption {
                    name,
                    description: part(&description, "criteria")?,
                })
            })
            .collect::<Result<_, InvalidRequest>>()?,
    })
}

/// Decodes level descriptions, lowest first.
fn decode_score(criteria: Option<&RawValue>) -> Result<DecisionScore, InvalidRequest> {
    let levels = criteria
        .and_then(|criteria| serde_json::from_str::<Vec<Box<RawValue>>>(criteria.get()).ok())
        .ok_or(InvalidRequest::invalid_field(Some("criteria")))?;
    Ok(DecisionScore {
        levels: levels
            .iter()
            .map(|level| {
                Ok(DecisionScoreLevel {
                    label: None,
                    description: part(level, "criteria")?,
                })
            })
            .collect::<Result<_, InvalidRequest>>()?,
    })
}

fn criteria_entries(criteria: &RawValue) -> Result<Vec<(String, Box<RawValue>)>, InvalidRequest> {
    serde_json::from_str::<OrderedObject>(criteria.get())
        .map(|entries| entries.0)
        .map_err(|_| InvalidRequest::invalid_field(Some("criteria")))
}

/// A content part for a JSON value: a string as text, null as absent, anything else as its
/// exact JSON source. A value no JSON value can hold, such as an out-of-range number, is
/// invalid for `param`.
fn part(
    value: &RawValue,
    param: &'static str,
) -> Result<Option<DecisionContentPart>, InvalidRequest> {
    // The raw scan accepts text a value cannot hold, such as out-of-range numbers or lone
    // surrogate escapes.
    match serde_json::from_str::<Value>(value.get()) {
        Ok(Value::Null) => Ok(None),
        Ok(Value::String(text)) => Ok(Some(text_part(text))),
        Ok(_) => Ok(Some(DecisionContentPart {
            content: Some(Content::JsonContent(value.get().to_owned())),
        })),
        Err(_) => Err(InvalidRequest::invalid_field(Some(param))),
    }
}

fn text_part(text: String) -> DecisionContentPart {
    DecisionContentPart {
        content: Some(Content::TextContent(text)),
    }
}

fn push_question(body: &mut String, question: &DecisionQuestion) -> Result<(), InvalidRequest> {
    let type_name = match &question.kind {
        Some(Kind::Predicate(_)) => "noul",
        Some(Kind::Choice(_)) => "choice",
        Some(Kind::Score(_)) => "score",
        None => return Err(InvalidRequest::invalid_field(Some("questions"))),
    };
    body.push_str("{\"type\":");
    push_json(body, type_name);
    body.push_str(",\"instructions\":");
    let instructions = question
        .instructions
        .iter()
        .map(part_text)
        .collect::<Vec<_>>()
        .join("\n\n");
    push_json(body, &instructions);
    match &question.kind {
        Some(Kind::Predicate(predicate)) => {
            let described = [
                ("true", &predicate.true_description),
                ("false", &predicate.false_description),
            ]
            .into_iter()
            .filter_map(|(name, description)| Some((name, description.as_ref()?)))
            .collect::<Vec<_>>();
            if !described.is_empty() {
                body.push_str(",\"criteria\":{");
                for (index, (name, description)) in described.into_iter().enumerate() {
                    if index > 0 {
                        body.push(',');
                    }
                    push_json(body, name);
                    body.push(':');
                    push_part(body, Some(description));
                }
                body.push('}');
            }
        }
        Some(Kind::Choice(choice)) => {
            body.push_str(",\"criteria\":{");
            for (index, option) in choice.options.iter().enumerate() {
                if index > 0 {
                    body.push(',');
                }
                push_json(body, &option.name);
                body.push(':');
                push_part(body, option.description.as_ref());
            }
            body.push('}');
        }
        Some(Kind::Score(score)) => {
            body.push_str(",\"criteria\":[");
            for (index, level) in score.levels.iter().enumerate() {
                if index > 0 {
                    body.push(',');
                }
                push_part(body, level.description.as_ref());
            }
            body.push(']');
        }
        None => {}
    }
    body.push('}');
    Ok(())
}

fn push_answer(
    body: &mut String,
    question: &DecisionQuestion,
    answer: Option<&Answer>,
) -> Result<(), ResultEncodeError> {
    match (&question.kind, answer) {
        (_, Some(Answer::Refusal(_))) => return Err(ResultEncodeError::Refusal),
        (Some(Kind::Predicate(_)), Some(Answer::Predicate(answer))) => {
            let probability = answer
                .true_probability
                .or_else(|| answer.value.map(|value| if value { 1.0 } else { 0.0 }))
                .ok_or(ResultEncodeError::Mismatch)?;
            body.push_str("{\"type\":\"noul\",\"noul\":");
            push_json(body, &probability);
        }
        (Some(Kind::Choice(choice)), Some(Answer::Choice(answer))) => {
            let probabilities = &answer.option_probabilities;
            if !probabilities.is_empty() && probabilities.len() != choice.options.len() {
                return Err(ResultEncodeError::Mismatch);
            }
            let selected = answer
                .selected_option_index
                .map(|index| index as usize)
                .or_else(|| most_probable(probabilities))
                .and_then(|index| choice.options.get(index))
                .ok_or(ResultEncodeError::Mismatch)?;
            body.push_str("{\"type\":\"choice\",\"choice\":");
            push_json(body, &selected.name);
            if !probabilities.is_empty() {
                body.push_str(",\"probabilities\":{");
                for (index, (option, probability)) in
                    choice.options.iter().zip(probabilities).enumerate()
                {
                    if index > 0 {
                        body.push(',');
                    }
                    push_json(body, &option.name);
                    body.push(':');
                    push_json(body, probability);
                }
                body.push('}');
            }
            push_confidence(body, answer.confidence);
        }
        (Some(Kind::Score(score)), Some(Answer::Score(answer))) => {
            let probabilities = &answer.level_probabilities;
            if !probabilities.is_empty() && probabilities.len() != score.levels.len() {
                return Err(ResultEncodeError::Mismatch);
            }
            let expected = answer
                .expected_level
                .or_else(|| (!probabilities.is_empty()).then(|| expected_level(probabilities)))
                .or_else(|| answer.selected_level_index.map(f64::from))
                .ok_or(ResultEncodeError::Mismatch)?;
            body.push_str("{\"type\":\"score\",\"score\":");
            push_json(body, &expected);
            body.push_str(",\"legend\":{");
            for (index, level) in score.levels.iter().enumerate() {
                if index > 0 {
                    body.push(',');
                }
                push_json(body, &index.to_string());
                body.push(':');
                push_part(body, level.description.as_ref());
            }
            body.push('}');
            if !probabilities.is_empty() {
                body.push_str(",\"probabilities\":{");
                for (index, probability) in probabilities.iter().enumerate() {
                    if index > 0 {
                        body.push(',');
                    }
                    push_json(body, &index.to_string());
                    body.push(':');
                    push_json(body, probability);
                }
                body.push('}');
            }
            push_confidence(body, answer.confidence);
        }
        _ => return Err(ResultEncodeError::Mismatch),
    }
    body.push('}');
    Ok(())
}

fn push_confidence(body: &mut String, confidence: Option<f64>) {
    if let Some(confidence) = confidence {
        body.push_str(",\"confidence\":");
        push_json(body, &confidence);
    }
}

/// Writes a part as a JSON value: text as a string, JSON content as its exact source, and an
/// absent part as null.
fn push_part(body: &mut String, part: Option<&DecisionContentPart>) {
    match part.and_then(|part| part.content.as_ref()) {
        Some(Content::TextContent(text)) => push_json(body, text),
        Some(Content::JsonContent(json)) => body.push_str(json),
        None => body.push_str("null"),
    }
}

/// The text of a part: its text, or its JSON source.
fn part_text(part: &DecisionContentPart) -> &str {
    match &part.content {
        Some(Content::TextContent(text) | Content::JsonContent(text)) => text,
        None => "",
    }
}

fn push_json<T: serde::Serialize + ?Sized>(body: &mut String, value: &T) {
    body.push_str(&serde_json::to_string(value).expect("JSON scalar"));
}

fn decode_answer(
    question: &DecisionQuestion,
    answer: &Map<String, Value>,
) -> anyhow::Result<DecisionAnswer> {
    let (type_name, decoded) = match &question.kind {
        Some(Kind::Predicate(_)) => {
            let probability = probability(answer.get("noul"))?;
            (
                "noul",
                Answer::Predicate(DecisionPredicateAnswer {
                    true_probability: Some(probability),
                    value: Some(predicate_value(probability)),
                    confidence: None,
                }),
            )
        }
        Some(Kind::Choice(choice)) => {
            let probabilities = answer.get("probabilities").and_then(Value::as_object);
            let option_probabilities = choice
                .options
                .iter()
                .map(|option| {
                    probability(probabilities.and_then(|values| values.get(&option.name)))
                })
                .collect::<anyhow::Result<Vec<_>>>()?;
            let selected = answer
                .get("choice")
                .and_then(Value::as_str)
                .ok_or_else(|| anyhow!("System One choice answer has no choice"))?;
            let selected_option_index = choice
                .options
                .iter()
                .position(|option| option.name == selected)
                .ok_or_else(|| anyhow!("System One choice answer selects an unknown option"))?;
            (
                "choice",
                Answer::Choice(DecisionChoiceAnswer {
                    option_probabilities,
                    selected_option_index: Some(u32::try_from(selected_option_index)?),
                    confidence: optional_number(answer.get("confidence"))?,
                }),
            )
        }
        Some(Kind::Score(score)) => {
            let probabilities = answer.get("probabilities").and_then(Value::as_object);
            // System One keys level probabilities by zero-based level index.
            let level_probabilities = (0..score.levels.len())
                .map(|index| {
                    probability(probabilities.and_then(|values| values.get(&index.to_string())))
                })
                .collect::<anyhow::Result<Vec<_>>>()?;
            let selected_level_index = most_probable(&level_probabilities)
                .map(u32::try_from)
                .transpose()?;
            (
                "score",
                Answer::Score(DecisionScoreAnswer {
                    level_probabilities,
                    expected_level: optional_number(answer.get("score"))?,
                    selected_level_index,
                    confidence: optional_number(answer.get("confidence"))?,
                }),
            )
        }
        None => return Err(anyhow!("decision question has no kind")),
    };
    if let Some(answered) = answer_type(answer)?
        && answered != type_name
    {
        return Err(anyhow!(
            "System One answer type does not match its question"
        ));
    }
    Ok(DecisionAnswer {
        answer: Some(decoded),
    })
}

fn answer_type(answer: &Map<String, Value>) -> anyhow::Result<Option<&str>> {
    answer
        .get("type")
        .map(|value| {
            value
                .as_str()
                .ok_or_else(|| anyhow!("System One answer type is not a string"))
        })
        .transpose()
}

/// A required probability from zero to one.
fn probability(value: Option<&Value>) -> anyhow::Result<f64> {
    let value = value
        .and_then(Value::as_f64)
        .ok_or_else(|| anyhow!("System One answer misses a probability"))?;
    if (0.0..=1.0).contains(&value) {
        Ok(value)
    } else {
        Err(anyhow!("System One probability is outside zero to one"))
    }
}

fn optional_number(value: Option<&Value>) -> anyhow::Result<Option<f64>> {
    value
        .map(|value| {
            value
                .as_f64()
                .ok_or_else(|| anyhow!("System One answer field is not a number"))
        })
        .transpose()
}

/// A JSON object's members in source order, refusing duplicate names.
struct OrderedObject(Vec<(String, Box<RawValue>)>);

impl<'de> Deserialize<'de> for OrderedObject {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        deserializer.deserialize_map(OrderedObjectVisitor)
    }
}

struct OrderedObjectVisitor;

impl<'de> Visitor<'de> for OrderedObjectVisitor {
    type Value = OrderedObject;

    fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("a JSON object with unique member names")
    }

    fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<Self::Value, A::Error> {
        let mut names = HashSet::new();
        let mut members = Vec::new();
        while let Some((name, value)) = map.next_entry::<String, Box<RawValue>>()? {
            if !names.insert(name.clone()) {
                return Err(serde::de::Error::custom("duplicate member name"));
            }
            members.push((name, value));
        }
        Ok(OrderedObject(members))
    }
}

#[cfg(test)]
mod tests {
    use dotcore_proto::dotcore::inference::v1::DecisionRefusal;
    use serde_json::json;

    use super::*;

    /// A System One request in llama.cpp's dialect asking each question kind, with question
    /// names and options deliberately out of alphabetical order.
    const REQUEST: &str = r#"{
        "model": "support-decisions",
        "state": {"ticket": "Refund my order", "tier": 2},
        "questions": {
            "urgent": {"type": "noul", "instructions": "Is it urgent?", "criteria": {"true": "Needs action today", "false": null}},
            "category": {"type": "choice", "instructions": "Pick the category.", "criteria": {"refund": "Money back", "other": null, "billing": {"area": "invoices"}}},
            "annoyed": {"type": "noul", "instructions": "Is the customer annoyed?"},
            "severity": {"type": "score", "instructions": "Rate the severity.", "criteria": ["Minor", "Major", {"level": "blocking"}]}
        }
    }"#;

    /// An engine answer to [`REQUEST`], with answers out of request order.
    const RESPONSE: &str = concat!(
        r#"{"model":"served","answers":{"#,
        r#""severity":{"type":"score","score":1.25,"legend":{"0":"Minor","1":"Major","2":"x"},"probabilities":{"0":0.25,"1":0.25,"2":0.5},"confidence":0.125},"#,
        r#""annoyed":{"type":"noul","noul":0.25},"#,
        r#""category":{"type":"choice","choice":"refund","probabilities":{"refund":0.5,"other":0.25,"billing":0.25},"confidence":0.375},"#,
        r#""urgent":{"type":"noul","noul":0.875}"#,
        r#"},"usage":{"input_tokens":96,"output_tokens":0}}"#
    );

    #[test]
    fn a_request_decodes_every_question_kind_in_source_order() {
        assert_eq!(
            decode_request(REQUEST.as_bytes()).expect("decoded"),
            request()
        );
    }

    #[test]
    fn a_string_state_is_one_text_part_and_a_false_stream_is_accepted() {
        let request = decode_request(
            br#"{"model":"route","state":"Refund please","stream":false,"questions":{"q":{"type":"noul","instructions":"Urgent?"}}}"#,
        )
        .expect("decoded request");

        assert_eq!(request.state, [text("Refund please")]);
    }

    #[test]
    fn wire_rules_reject_with_fixed_parameters_only() {
        let with = |change: &dyn Fn(&mut Value)| {
            let mut value = serde_json::from_str::<Value>(REQUEST).expect("request JSON");
            change(&mut value);
            serde_json::to_vec(&value).expect("request bytes")
        };
        let cases = vec![
            (
                "missing model",
                with(&|value| {
                    value.as_object_mut().expect("object").remove("model");
                }),
                InvalidRequest::invalid_field(Some("model")),
            ),
            (
                "empty model",
                with(&|value| value["model"] = json!("")),
                InvalidRequest::invalid_field(Some("model")),
            ),
            (
                "missing state",
                with(&|value| {
                    value.as_object_mut().expect("object").remove("state");
                }),
                InvalidRequest::invalid_field(Some("state")),
            ),
            (
                "images",
                with(&|value| value["images"] = json!(["aGVsbG8="])),
                InvalidRequest::unsupported_feature(Some("images")),
            ),
            (
                "stream",
                with(&|value| value["stream"] = json!(true)),
                InvalidRequest::unsupported_feature(Some("stream")),
            ),
            (
                "unknown field",
                with(&|value| value["assets"] = json!([])),
                InvalidRequest::unsupported_feature(None),
            ),
            (
                "unknown question type",
                with(&|value| value["questions"]["urgent"]["type"] = json!("box")),
                InvalidRequest::unsupported_feature(Some("type")),
            ),
            (
                "missing instructions",
                with(&|value| {
                    value["questions"]["urgent"]
                        .as_object_mut()
                        .expect("question")
                        .remove("instructions");
                }),
                InvalidRequest::invalid_field(Some("instructions")),
            ),
            (
                "score criteria not a list",
                with(&|value| value["questions"]["severity"]["criteria"] = json!({"0": "low"})),
                InvalidRequest::invalid_field(Some("criteria")),
            ),
            (
                "unknown predicate answer",
                with(&|value| value["questions"]["urgent"]["criteria"] = json!({"maybe": "?"})),
                InvalidRequest::invalid_field(Some("criteria")),
            ),
        ];

        for (case, body, expected) in cases {
            match decode_request(&body) {
                Err(RequestDecodeError::Invalid(error)) => assert_eq!(error, expected, "{case}"),
                other => panic!("{case} must be rejected, got {other:?}"),
            }
        }
        assert!(matches!(
            decode_request(b"[]"),
            Err(RequestDecodeError::InvalidBody)
        ));
    }

    #[test]
    fn duplicate_question_and_option_names_are_rejected_without_echoing_them() {
        let duplicate_question = br#"{"model":"route","state":"s","questions":{"secret-name":{"type":"noul","instructions":"a"},"secret-name":{"type":"noul","instructions":"b"}}}"#;
        let duplicate_option = br#"{"model":"route","state":"s","questions":{"q":{"type":"choice","instructions":"a","criteria":{"secret-option":null,"secret-option":"x"}}}}"#;

        for (body, param) in [
            (&duplicate_question[..], "questions"),
            (&duplicate_option[..], "criteria"),
        ] {
            let Err(RequestDecodeError::Invalid(error)) = decode_request(body) else {
                panic!("duplicates are rejected");
            };
            assert_eq!(error, InvalidRequest::invalid_field(Some(param)));
            assert!(!error.to_string().contains("secret"), "{error}");
        }
    }

    #[test]
    fn a_request_encodes_for_the_engine_in_request_order_with_exact_json_sources() {
        let mut request = request();
        request.questions[0]
            .instructions
            .push(json_part(r#"{"sla":4}"#));

        let body = encode_request(&request, "clef").expect("encoded request");

        assert_eq!(
            String::from_utf8(body).expect("UTF-8 body"),
            concat!(
                r#"{"model":"clef","state":{"ticket": "Refund my order", "tier": 2},"questions":{"#,
                r#""urgent":{"type":"noul","instructions":"Is it urgent?\n\n{\"sla\":4}","criteria":{"true":"Needs action today"}},"#,
                r#""category":{"type":"choice","instructions":"Pick the category.","criteria":{"refund":"Money back","other":null,"billing":{"area": "invoices"}}},"#,
                r#""annoyed":{"type":"noul","instructions":"Is the customer annoyed?"},"#,
                r#""severity":{"type":"score","instructions":"Rate the severity.","criteria":["Minor","Major",{"level": "blocking"}]}"#,
                "}}"
            )
        );
    }

    #[test]
    fn a_state_of_several_parts_has_no_system_one_form() {
        let request = DecisionRequest {
            state: vec![text("ticket"), json_part(r#"{"id":7}"#)],
            ..request()
        };

        assert_eq!(
            encode_request(&request, "clef"),
            Err(InvalidRequest::unsupported_feature(Some("state")))
        );
    }

    #[test]
    fn a_request_round_trips_through_the_wire() {
        let body = encode_request(&request(), "support-decisions").expect("encoded request");

        let mut decoded = decode_request(&body).expect("decoded request");
        decoded.model_id = "support-decisions".to_owned();

        assert_eq!(decoded, request());
    }

    #[test]
    fn engine_answers_decode_into_request_order_with_derived_fields() {
        let result = decode_result(&request(), RESPONSE.as_bytes()).expect("decoded answers");

        assert_eq!(result, result_fixture());
    }

    #[test]
    fn answers_breaking_the_request_shape_are_refused() {
        for (case, response) in [
            (
                "missing answer",
                RESPONSE.replace(r#""annoyed""#, r#""other""#),
            ),
            (
                "unknown option",
                RESPONSE.replace(r#""choice":"refund""#, r#""choice":"exchange""#),
            ),
            (
                "missing level",
                RESPONSE.replace(r#""2":0.5"#, r#""3":0.5"#),
            ),
            (
                "mismatched type",
                RESPONSE.replace(r#""type":"score""#, r#""type":"choice""#),
            ),
            ("out of range", RESPONSE.replace("0.875", "1.5")),
            ("not an object", "[]".to_owned()),
        ] {
            assert!(
                decode_result(&request(), response.as_bytes()).is_err(),
                "{case} must be refused"
            );
        }
    }

    #[test]
    fn a_result_encodes_as_answers_keyed_by_question_in_request_order() {
        let body = encode_result(&request(), &result_fixture(), "support-decisions")
            .expect("encoded answers");

        assert_eq!(
            String::from_utf8(body).expect("UTF-8 body"),
            concat!(
                r#"{"model":"support-decisions","answers":{"#,
                r#""urgent":{"type":"noul","noul":0.875},"#,
                r#""category":{"type":"choice","choice":"refund","probabilities":{"refund":0.5,"other":0.25,"billing":0.25},"confidence":0.375},"#,
                r#""annoyed":{"type":"noul","noul":0.25},"#,
                r#""severity":{"type":"score","score":1.25,"legend":{"0":"Minor","1":"Major","2":{"level": "blocking"}},"probabilities":{"0":0.25,"1":0.25,"2":0.5},"confidence":0.125}"#,
                r#"},"usage":{"input_tokens":96,"output_tokens":0}}"#
            )
        );
    }

    #[test]
    fn a_result_round_trips_through_the_wire() {
        let body = encode_result(&request(), &result_fixture(), "route").expect("encoded result");

        assert_eq!(
            decode_result(&request(), &body).expect("decoded result"),
            result_fixture()
        );
    }

    #[test]
    fn missing_derived_fields_are_filled_from_the_probabilities() {
        let request = decode_request(
            br#"{"model":"route","state":"s","questions":{"c":{"type":"choice","instructions":"a","criteria":{"x":null,"y":null}},"s":{"type":"score","instructions":"b","criteria":["low","high"]}}}"#,
        )
        .expect("decoded request");
        let result = DecisionResult {
            answers: vec![
                answer(Answer::Choice(DecisionChoiceAnswer {
                    option_probabilities: vec![0.25, 0.75],
                    ..Default::default()
                })),
                answer(Answer::Score(DecisionScoreAnswer {
                    level_probabilities: vec![0.5, 0.5],
                    ..Default::default()
                })),
            ],
            usage: None,
        };

        let body = encode_result(&request, &result, "route").expect("encoded result");

        assert_eq!(
            String::from_utf8(body).expect("UTF-8 body"),
            concat!(
                r#"{"model":"route","answers":{"#,
                r#""c":{"type":"choice","choice":"y","probabilities":{"x":0.25,"y":0.75}},"#,
                r#""s":{"type":"score","score":0.5,"legend":{"0":"low","1":"high"},"probabilities":{"0":0.5,"1":0.5}}"#,
                "}}"
            )
        );
    }

    #[test]
    fn refusals_and_answers_not_fitting_their_questions_have_no_system_one_form() {
        let request = decode_request(
            br#"{"model":"route","state":"s","questions":{"q":{"type":"choice","instructions":"a","criteria":{"x":null,"y":null}}}}"#,
        )
        .expect("decoded request");
        let encoded = |answers: Vec<DecisionAnswer>| {
            encode_result(
                &request,
                &DecisionResult {
                    answers,
                    usage: None,
                },
                "route",
            )
        };

        assert!(matches!(
            encoded(vec![answer(Answer::Refusal(DecisionRefusal {}))]),
            Err(ResultEncodeError::Refusal)
        ));
        for answers in [
            Vec::new(),
            vec![answer(
                Answer::Predicate(DecisionPredicateAnswer::default()),
            )],
            vec![answer(Answer::Choice(DecisionChoiceAnswer {
                option_probabilities: vec![1.0],
                ..Default::default()
            }))],
            vec![answer(Answer::Choice(DecisionChoiceAnswer {
                selected_option_index: Some(2),
                ..Default::default()
            }))],
        ] {
            assert!(matches!(encoded(answers), Err(ResultEncodeError::Mismatch)));
        }
    }

    /// The canonical form of [`REQUEST`].
    fn request() -> DecisionRequest {
        DecisionRequest {
            model_id: "support-decisions".to_owned(),
            state: vec![json_part(r#"{"ticket": "Refund my order", "tier": 2}"#)],
            questions: vec![
                question(
                    "urgent",
                    "Is it urgent?",
                    Kind::Predicate(DecisionPredicate {
                        true_description: Some(text("Needs action today")),
                        false_description: None,
                    }),
                ),
                question(
                    "category",
                    "Pick the category.",
                    Kind::Choice(DecisionChoice {
                        options: vec![
                            option("refund", Some(text("Money back"))),
                            option("other", None),
                            option("billing", Some(json_part(r#"{"area": "invoices"}"#))),
                        ],
                    }),
                ),
                question(
                    "annoyed",
                    "Is the customer annoyed?",
                    Kind::Predicate(DecisionPredicate::default()),
                ),
                question(
                    "severity",
                    "Rate the severity.",
                    Kind::Score(DecisionScore {
                        levels: vec![
                            level(text("Minor")),
                            level(text("Major")),
                            level(json_part(r#"{"level": "blocking"}"#)),
                        ],
                    }),
                ),
            ],
        }
    }

    /// The canonical form of [`RESPONSE`], with its derived fields.
    fn result_fixture() -> DecisionResult {
        DecisionResult {
            answers: vec![
                answer(Answer::Predicate(DecisionPredicateAnswer {
                    true_probability: Some(0.875),
                    value: Some(true),
                    confidence: None,
                })),
                answer(Answer::Choice(DecisionChoiceAnswer {
                    option_probabilities: vec![0.5, 0.25, 0.25],
                    selected_option_index: Some(0),
                    confidence: Some(0.375),
                })),
                answer(Answer::Predicate(DecisionPredicateAnswer {
                    true_probability: Some(0.25),
                    value: Some(false),
                    confidence: None,
                })),
                answer(Answer::Score(DecisionScoreAnswer {
                    level_probabilities: vec![0.25, 0.25, 0.5],
                    expected_level: Some(1.25),
                    selected_level_index: Some(2),
                    confidence: Some(0.125),
                })),
            ],
            usage: Some(DecisionUsage { input_tokens: 96 }),
        }
    }

    fn question(name: &str, instructions: &str, kind: Kind) -> DecisionQuestion {
        DecisionQuestion {
            name: name.to_owned(),
            instructions: vec![text(instructions)],
            kind: Some(kind),
        }
    }

    fn option(name: &str, description: Option<DecisionContentPart>) -> DecisionOption {
        DecisionOption {
            name: name.to_owned(),
            description,
        }
    }

    fn level(description: DecisionContentPart) -> DecisionScoreLevel {
        DecisionScoreLevel {
            label: None,
            description: Some(description),
        }
    }

    fn answer(answer: Answer) -> DecisionAnswer {
        DecisionAnswer {
            answer: Some(answer),
        }
    }

    fn text(value: &str) -> DecisionContentPart {
        text_part(value.to_owned())
    }

    fn json_part(value: &str) -> DecisionContentPart {
        DecisionContentPart {
            content: Some(Content::JsonContent(value.to_owned())),
        }
    }
}
