//! Validates the canonical Chat request shared by every inference entry point, including the
//! engine-specific generation controls that travel with it as one JSON object, for example
//! `{"top_k":20,"chat_template_kwargs":{"enable_thinking":false}}`.
//!
//! Public decoding, worker admission, and outbound encoding apply the same rules, so a generated
//! or forwarded request never carries more than a public one. Controls keep their original JSON
//! text, numbers included, and can never set typed request fields, routing, endpoints, or
//! credentials. Errors name only fields from a fixed list of public request paths.

use std::collections::BTreeMap;

use dotcore_proto::dotcore::inference::v1::{
    ChatMessageRole, ChatRequest, ChatToolChoiceMode, chat_message, chat_tool_choice,
};
use serde_json::{Map, Value, value::RawValue};

use crate::inference::InvalidRequest;

/// Engine-specific generation controls keyed by top-level request field, each kept as raw JSON.
pub(crate) type RequestExtensions = BTreeMap<String, Box<RawValue>>;

/// Public request paths that errors and diagnostics may name.
const RECOGNIZED_PARAMS: &[&str] = &[
    "model",
    "messages",
    "tools",
    "tool_choice",
    "parallel_tool_calls",
    "stream",
    "stream_options",
    "temperature",
    "top_p",
    "max_tokens",
    "max_completion_tokens",
    "stop",
    "store",
    "reasoning_effort",
    "presence_penalty",
    "frequency_penalty",
    "n",
    "input",
    "instructions",
    "max_output_tokens",
    "reasoning",
    "prompt_cache_key",
    "top_k",
    "min_p",
    "repetition_penalty",
    "seed",
    "chat_template_kwargs",
    "response_format",
    "logprobs",
    "top_logprobs",
    "functions",
    "function_call",
    "audio",
    "modalities",
    "prediction",
    "web_search_options",
    "previous_response_id",
    "conversation",
    "background",
    "include",
    "text",
    "truncation",
    "prompt",
    "max_tool_calls",
    "state",
    "questions",
    "criteria",
];

/// Keys that controls may never set: typed request fields and transport or routing authority.
const RESERVED_KEYS: &[&str] = &[
    "model",
    "messages",
    "tools",
    "tool_choice",
    "parallel_tool_calls",
    "stream",
    "stream_options",
    "temperature",
    "top_p",
    "max_tokens",
    "max_completion_tokens",
    "stop",
    "store",
    "reasoning_effort",
    "presence_penalty",
    "frequency_penalty",
    "input",
    "instructions",
    "max_output_tokens",
    "request_extensions_json",
    "headers",
    "extra_headers",
    "extra_body",
    "extra_query",
    "authorization",
    "api_key",
    "endpoint",
    "base_url",
    "api_base",
    "url",
    "provider",
    "kv_transfer_params",
];

/// Output shapes, stateful features, and client features the platform cannot represent or
/// does not provide on any endpoint.
const UNSUPPORTED_KEYS: &[&str] = &[
    "response_format",
    "logprobs",
    "top_logprobs",
    "functions",
    "function_call",
    "audio",
    "modalities",
    "prediction",
    "web_search_options",
    "previous_response_id",
    "conversation",
    "background",
    "include",
    "text",
    "truncation",
    "prompt",
    "max_tool_calls",
];

/// Returns the allowlisted spelling of a public request path, or `None` for any other name.
pub(crate) fn recognized_param(name: &str) -> Option<&'static str> {
    RECOGNIZED_PARAMS
        .iter()
        .copied()
        .find(|recognized| *recognized == name)
}

/// Validates the platform-owned shape of a canonical request before any target sees it.
pub(crate) fn validate(request: &ChatRequest) -> Result<(), InvalidRequest> {
    request_extensions(request)?;
    if request.store == Some(true) {
        return Err(InvalidRequest::unsupported_feature(Some("store")));
    }
    for message in &request.messages {
        validate_message(message)?;
    }
    for tool in &request.tools {
        if tool.name.is_empty() || !is_json_object(&tool.parameters_json_schema) {
            return Err(InvalidRequest::invalid_field(Some("tools")));
        }
    }
    if let Some(choice) = &request.tool_choice {
        let valid = match &choice.selection {
            Some(chat_tool_choice::Selection::Mode(mode)) => ChatToolChoiceMode::try_from(*mode)
                .is_ok_and(|mode| mode != ChatToolChoiceMode::Unspecified),
            Some(chat_tool_choice::Selection::FunctionName(name)) => !name.is_empty(),
            None => false,
        };
        if !valid {
            return Err(InvalidRequest::invalid_field(Some("tool_choice")));
        }
    }
    if let Some(controls) = &request.generation_controls {
        for (name, value) in [
            ("temperature", controls.temperature.map(f64::from)),
            ("top_p", controls.top_p.map(f64::from)),
            ("presence_penalty", controls.presence_penalty),
            ("frequency_penalty", controls.frequency_penalty),
        ] {
            if value.is_some_and(|value| !value.is_finite()) {
                return Err(InvalidRequest::invalid_field(Some(name)));
            }
        }
    }
    Ok(())
}

/// Parses the request's control object, rejecting any other JSON shape or protected key.
pub(crate) fn request_extensions(
    request: &ChatRequest,
) -> Result<RequestExtensions, InvalidRequest> {
    let Some(text) = &request.request_extensions_json else {
        return Ok(RequestExtensions::new());
    };
    let extensions = serde_json::from_str::<RequestExtensions>(text)
        .map_err(|_| InvalidRequest::invalid_field(None))?;
    for key in extensions.keys() {
        check_extension_key(key)?;
    }
    Ok(extensions)
}

/// Rejects a control key that names a typed field, transport authority, or unsupported feature.
pub(crate) fn check_extension_key(key: &str) -> Result<(), InvalidRequest> {
    if UNSUPPORTED_KEYS.contains(&key) {
        return Err(InvalidRequest::unsupported_feature(recognized_param(key)));
    }
    if RESERVED_KEYS.contains(&key) {
        return Err(InvalidRequest::invalid_field(recognized_param(key)));
    }
    Ok(())
}

/// Serializes captured controls as canonical object text, or `None` when there are none.
pub(crate) fn extensions_json(extensions: &RequestExtensions) -> Option<String> {
    (!extensions.is_empty())
        .then(|| serde_json::to_string(extensions).expect("raw JSON values serialize"))
}

fn validate_message(
    message: &dotcore_proto::dotcore::inference::v1::ChatMessage,
) -> Result<(), InvalidRequest> {
    let invalid = || InvalidRequest::invalid_field(Some("messages"));
    let role = ChatMessageRole::try_from(message.role).map_err(|_| invalid())?;
    let is_tool_result = matches!(message.content, Some(chat_message::Content::ToolResult(_)));
    let valid = match role {
        ChatMessageRole::Unspecified => false,
        ChatMessageRole::Tool => is_tool_result && message.content_parts.is_empty(),
        ChatMessageRole::Assistant => !is_tool_result,
        ChatMessageRole::System | ChatMessageRole::Developer | ChatMessageRole::User => {
            !is_tool_result && message.tool_calls.is_empty() && message.reasoning.is_none()
        }
    };
    if valid { Ok(()) } else { Err(invalid()) }
}

fn is_json_object(text: &str) -> bool {
    serde_json::from_str::<Map<String, Value>>(text).is_ok()
}

#[cfg(test)]
mod tests {
    use dotcore_proto::dotcore::inference::v1::{
        ChatMessage, ChatToolChoice, FunctionToolDefinition, FunctionToolResult,
    };

    use super::*;

    #[test]
    fn controls_keep_their_original_json_text() {
        let text = r#"{"chat_template_kwargs":{"enable_thinking":false,"list":[1,null,"a"]},"seed":18446744073709551616,"top_k":20}"#;
        let request = ChatRequest {
            request_extensions_json: Some(text.to_owned()),
            ..Default::default()
        };

        let extensions = request_extensions(&request).expect("valid controls");

        assert_eq!(
            extensions["seed"].get(),
            "18446744073709551616",
            "integers beyond 64 bits keep their exact text"
        );
        assert_eq!(extensions_json(&extensions).as_deref(), Some(text));
    }

    #[test]
    fn malformed_control_objects_are_invalid_fields() {
        for text in ["[1]", "7", "null", "{", "{\"a\":}", "\"text\""] {
            let request = ChatRequest {
                request_extensions_json: Some(text.to_owned()),
                ..Default::default()
            };
            assert_eq!(
                validate(&request),
                Err(InvalidRequest::invalid_field(None)),
                "{text} must be rejected"
            );
        }
    }

    #[test]
    fn protected_and_unsupported_keys_are_rejected_with_safe_fields() {
        for (key, expected) in [
            ("model", InvalidRequest::invalid_field(Some("model"))),
            ("messages", InvalidRequest::invalid_field(Some("messages"))),
            ("stream", InvalidRequest::invalid_field(Some("stream"))),
            ("store", InvalidRequest::invalid_field(Some("store"))),
            ("authorization", InvalidRequest::invalid_field(None)),
            ("base_url", InvalidRequest::invalid_field(None)),
            ("kv_transfer_params", InvalidRequest::invalid_field(None)),
            (
                "request_extensions_json",
                InvalidRequest::invalid_field(None),
            ),
            (
                "response_format",
                InvalidRequest::unsupported_feature(Some("response_format")),
            ),
            (
                "function_call",
                InvalidRequest::unsupported_feature(Some("function_call")),
            ),
        ] {
            let request = ChatRequest {
                request_extensions_json: Some(format!("{{\"{key}\":1}}")),
                ..Default::default()
            };
            assert_eq!(validate(&request), Err(expected), "{key}");
        }
    }

    #[test]
    fn unknown_parameter_names_are_never_echoed() {
        assert_eq!(recognized_param("top_k"), Some("top_k"));
        assert_eq!(recognized_param("criteria"), Some("criteria"));
        assert_eq!(recognized_param("sk-secret-key"), None);
    }

    #[test]
    fn canonical_structure_is_validated_for_generated_callers() {
        let tool_message = |role: ChatMessageRole| ChatMessage {
            role: role as i32,
            content: Some(chat_message::Content::ToolResult(FunctionToolResult {
                tool_call_id: "call".to_owned(),
                content: "result".to_owned(),
            })),
            ..Default::default()
        };
        let cases = [
            (
                ChatRequest {
                    store: Some(true),
                    ..Default::default()
                },
                InvalidRequest::unsupported_feature(Some("store")),
            ),
            (
                ChatRequest {
                    messages: vec![ChatMessage::default()],
                    ..Default::default()
                },
                InvalidRequest::invalid_field(Some("messages")),
            ),
            (
                ChatRequest {
                    messages: vec![tool_message(ChatMessageRole::User)],
                    ..Default::default()
                },
                InvalidRequest::invalid_field(Some("messages")),
            ),
            (
                ChatRequest {
                    messages: vec![ChatMessage {
                        role: ChatMessageRole::User as i32,
                        reasoning: Some("thought".to_owned()),
                        ..Default::default()
                    }],
                    ..Default::default()
                },
                InvalidRequest::invalid_field(Some("messages")),
            ),
            (
                ChatRequest {
                    tools: vec![FunctionToolDefinition {
                        name: "read".to_owned(),
                        parameters_json_schema: "not json".to_owned(),
                        ..Default::default()
                    }],
                    ..Default::default()
                },
                InvalidRequest::invalid_field(Some("tools")),
            ),
            (
                ChatRequest {
                    tools: vec![FunctionToolDefinition {
                        name: "read".to_owned(),
                        parameters_json_schema: "[]".to_owned(),
                        ..Default::default()
                    }],
                    ..Default::default()
                },
                InvalidRequest::invalid_field(Some("tools")),
            ),
            (
                ChatRequest {
                    tool_choice: Some(ChatToolChoice { selection: None }),
                    ..Default::default()
                },
                InvalidRequest::invalid_field(Some("tool_choice")),
            ),
            (
                ChatRequest {
                    generation_controls: Some(
                        dotcore_proto::dotcore::inference::v1::ChatGenerationControls {
                            presence_penalty: Some(f64::NAN),
                            ..Default::default()
                        },
                    ),
                    ..Default::default()
                },
                InvalidRequest::invalid_field(Some("presence_penalty")),
            ),
        ];
        for (request, expected) in cases {
            assert_eq!(validate(&request), Err(expected));
        }

        let valid = ChatRequest {
            messages: vec![tool_message(ChatMessageRole::Tool)],
            store: Some(false),
            ..Default::default()
        };
        assert_eq!(validate(&valid), Ok(()));
    }
}
