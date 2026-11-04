//! OpenAI API-compatible Chat HTTP, JSON, and SSE codec.
//!
//! Requests decode into the canonical Chat request: known fields become typed values and any
//! other top-level field becomes an engine-specific control forwarded as its original JSON text.
//! Errors name only recognized public fields, never caller-supplied keys or values.

use std::collections::BTreeSet;

use dotcore_proto::dotcore::inference::v1::AvailableModels;
use dotcore_proto::dotcore::inference::v1::{
    ChatCompletionResult, ChatContentPart, ChatFinishReason, ChatGenerationControls, ChatMessage,
    ChatMessageRole, ChatRequest, ChatStreamEvent, ChatToolChoice, ChatToolChoiceMode,
    DecisionRequest, FunctionToolCall, FunctionToolDefinition, FunctionToolResult, TokenUsage,
    chat_content_part, chat_message, chat_stream_event::Event, chat_text_delta,
};
use dotcore_proto::google::protobuf::NullValue;
use serde_json::{Map, Value, json};

use crate::{
    inference::{InvalidRequest, InvalidRequestCategory},
    router::{
        images::{self, ImageValidationError},
        openai_compatible_codec::{ReasoningAliasError, reasoning_alias},
        request_controls::{self, RequestExtensions},
    },
};

/// Decoded public route request. It remains inside the public inference API boundary.
pub(crate) enum OpenAiRequest {
    Models,

    Chat(ChatRequest),

    Responses(ChatRequest),

    /// A System One decision request.
    SystemOne(DecisionRequest),
}

use super::OpenAiResponse;

pub(crate) fn decode(
    method: &str,
    path: &str,
    body: &[u8],
) -> Result<OpenAiRequest, OpenAiResponse> {
    match (method, path) {
        ("GET", "/v1/models") => Ok(OpenAiRequest::Models),
        ("POST", "/v1/chat/completions") => validated(decode_chat(body)?).map(OpenAiRequest::Chat),
        ("POST", "/v1/responses") => {
            validated(super::responses::decode_request(body)?).map(OpenAiRequest::Responses)
        }
        ("POST", "/v1/systemone") => {
            let request =
                crate::router::systemone::decode_request(body).map_err(|error| match error {
                    crate::router::systemone::RequestDecodeError::InvalidBody => error_response(
                        400,
                        "invalid_request_error",
                        "Request body must be a valid JSON object.",
                        None,
                        None,
                    ),
                    crate::router::systemone::RequestDecodeError::Invalid(error) => {
                        request_error(error)
                    }
                })?;
            crate::router::decision_validation::validate(&request).map_err(request_error)?;
            Ok(OpenAiRequest::SystemOne(request))
        }
        (_, "/v1/responses") | (_, "/v1/embeddings") => Err(error(
            400,
            "unsupported_endpoint",
            "This OpenAI endpoint is not supported.",
        )),
        _ => Err(error(
            404,
            "invalid_request_error",
            "The requested endpoint does not exist.",
        )),
    }
}

pub(crate) fn models(models: AvailableModels) -> OpenAiResponse {
    json_response(
        200,
        json!({"object":"list","data":models.models.into_iter().map(|model| json!({"id":model.model_id,"object":"model","created":0,"owned_by":"dotcore"})).collect::<Vec<_>>() }),
    )
}

pub(crate) fn completion(
    mut result: ChatCompletionResult,
    admitted_route_name: &str,
) -> OpenAiResponse {
    result.model_id = admitted_route_name.to_owned();
    json_response(200, completion_json(result))
}

/// Encodes validated internal stream events into public SSE frames.
pub(crate) struct StreamEncoder {
    stream_id: Option<String>,
    admitted_route_name: String,
    stream_created: Option<u64>,
    include_usage: bool,
}

impl StreamEncoder {
    /// Creates an encoder that emits the usage chunk only when the client asked for it.
    pub(crate) fn new(admitted_route_name: String, include_usage: bool) -> Self {
        Self {
            stream_id: None,
            admitted_route_name,
            stream_created: None,
            include_usage,
        }
    }

    pub(crate) fn encode(&mut self, event: ChatStreamEvent) -> Option<Vec<u8>> {
        if !self.include_usage && matches!(event.event, Some(Event::Usage(_))) {
            return None;
        }
        if let Some(Event::Started(started)) = event.event.as_ref() {
            self.stream_id = Some(started.completion_id.clone());
            self.stream_created = Some(started.created_at_unix_seconds);
        }
        let mut value = stream_event_json(event)?;
        if let Value::Object(object) = &mut value {
            if let Some(id) = &self.stream_id {
                object.insert("id".to_owned(), json!(id));
            }
            object.insert("model".to_owned(), json!(self.admitted_route_name));
            if let Some(created) = self.stream_created {
                object.insert("created".to_owned(), json!(created));
            }
        }
        let mut frame = b"data: ".to_vec();
        frame.extend_from_slice(
            serde_json::to_string(&value)
                .expect("JSON event")
                .as_bytes(),
        );
        frame.extend_from_slice(b"\n\n");
        Some(frame)
    }
}

/// Projects an engine's unsuccessful response, naming its field only when it is recognized.
pub(crate) fn upstream(
    status: u16,
    headers: Vec<(String, String)>,
    body: Vec<u8>,
) -> OpenAiResponse {
    let param = serde_json::from_slice::<Value>(&body)
        .ok()
        .and_then(|body| {
            body.pointer("/error/param")
                .and_then(Value::as_str)
                .and_then(request_controls::recognized_param)
        });
    let mut response = error_response(
        public_upstream_status(status),
        "upstream_error",
        "The inference engine rejected the request.",
        param,
        None,
    );
    response.passthrough_headers = headers;
    response
}

fn public_upstream_status(status: u16) -> u16 {
    match status {
        400..=499 => status,
        _ => 502,
    }
}

pub(crate) fn cancelled() -> OpenAiResponse {
    error(
        499,
        "request_cancelled",
        "The client cancelled the request.",
    )
}

pub(crate) fn timeout() -> OpenAiResponse {
    error(
        504,
        "request_timeout",
        "The inference request exceeded its configured timeout.",
    )
}

pub(crate) fn adapter_error() -> OpenAiResponse {
    error(
        502,
        "upstream_error",
        "The inference engine returned an invalid response.",
    )
}

/// Answers a request whose selected inference engine could not be reached.
pub(crate) fn engine_unreachable() -> OpenAiResponse {
    error(
        502,
        "upstream_error",
        "The inference engine could not be reached.",
    )
}

/// Encodes a terminal public Chat stream error after SSE headers are sent.
pub(crate) fn stream_error_frame() -> Vec<u8> {
    let mut frame = b"data: ".to_vec();
    frame.extend_from_slice(&adapter_error().body);
    frame.extend_from_slice(b"\n\n");
    frame
}

pub(crate) fn error(status: u16, kind: &str, message: &str) -> OpenAiResponse {
    error_response(status, kind, message, None, None)
}

/// Projects an invalid canonical request as a safe client error naming a recognized field.
pub(crate) fn request_error(error: InvalidRequest) -> OpenAiResponse {
    let (status, kind, code, message) = match (error.category, error.param) {
        (InvalidRequestCategory::InvalidField, Some(param)) => (
            400,
            "invalid_request_error",
            "invalid_value",
            format!("Request field {param} is invalid."),
        ),
        (InvalidRequestCategory::InvalidField, None) => (
            400,
            "invalid_request_error",
            "invalid_value",
            "The request contains an invalid engine control.".to_owned(),
        ),
        (InvalidRequestCategory::UnsupportedFeature, Some(param)) => (
            400,
            "unsupported_feature",
            "unsupported_parameter",
            format!("Request field {param} is not supported."),
        ),
        (InvalidRequestCategory::UnsupportedFeature, None) => (
            400,
            "unsupported_feature",
            "unsupported_parameter",
            "The request asks for an unsupported feature.".to_owned(),
        ),
        (InvalidRequestCategory::RequestLimit, _) => (
            413,
            "invalid_request_error",
            "request_too_large",
            "The request exceeds a size limit.".to_owned(),
        ),
        (InvalidRequestCategory::ContextLength, _) => (
            400,
            "invalid_request_error",
            "context_length_exceeded",
            "The request is larger than the deployment's context.".to_owned(),
        ),
    };
    error_response(status, kind, &message, error.param, Some(code))
}

/// Answers a request whose route name matches no enabled route.
pub(crate) fn route_not_found() -> OpenAiResponse {
    error_response(
        404,
        "invalid_request_error",
        "The requested model route does not exist.",
        Some("model"),
        Some("model_not_found"),
    )
}

/// Answers a request whose route has no member serving its request kind, such as chat on a
/// decision-only route.
pub(crate) fn request_kind_not_served() -> OpenAiResponse {
    error_response(
        400,
        "invalid_request_error",
        "The requested model route does not serve this kind of request.",
        Some("model"),
        Some("unsupported_request_kind"),
    )
}

/// Answers a request for an existing route with no ready serving target.
pub(crate) fn no_ready_target() -> OpenAiResponse {
    error_response(
        503,
        "server_error",
        "The requested model route has no ready serving target.",
        None,
        Some("model_not_ready"),
    )
}

pub(crate) fn route_unavailable() -> OpenAiResponse {
    json_response(
        503,
        json!({"error":{"message":"The requested route is temporarily unavailable.","type":"server_error","param":null,"code":"model_unavailable"}}),
    )
}

/// Projects a rejected image input as an actionable client error naming the rejected cause.
///
/// The cause names caller-supplied metadata such as the media type, dimensions, or image count,
/// so it never carries image bytes or prompt content.
pub(crate) fn image_error(cause: &ImageValidationError) -> OpenAiResponse {
    error_response(
        400,
        "invalid_request_error",
        &cause.to_string(),
        None,
        Some("invalid_image"),
    )
}

/// Answers an image request refused by a serving target that cannot accept it.
pub(crate) fn image_unsupported_capability() -> OpenAiResponse {
    error_response(
        400,
        "invalid_request_error",
        "Image input is not supported by the selected serving target.",
        None,
        Some("invalid_image"),
    )
}

pub(crate) fn body_too_large() -> OpenAiResponse {
    error(
        413,
        "invalid_request_error",
        "Request body exceeds the 32 MiB limit.",
    )
}

/// Splits a public request object into its known fields and its engine-specific controls.
///
/// Unknown keys keep their original JSON text; protected or unsupported keys are refused.
pub(super) fn split_request(
    body: &[u8],
    known: &[&str],
) -> Result<(Map<String, Value>, RequestExtensions), OpenAiResponse> {
    let invalid_body = || {
        error(
            400,
            "invalid_request_error",
            "Request body must be a valid JSON object.",
        )
    };
    let fields = serde_json::from_slice::<RequestExtensions>(body).map_err(|_| invalid_body())?;
    let mut object = Map::new();
    let mut extensions = RequestExtensions::new();
    for (name, value) in fields {
        if known.contains(&name.as_str()) {
            // The raw scan accepts text a value cannot hold, such as out-of-range numbers or
            // lone surrogate escapes.
            let value = serde_json::from_str(value.get()).map_err(|_| invalid_body())?;
            object.insert(name, value);
        } else {
            request_controls::check_extension_key(&name).map_err(request_error)?;
            extensions.insert(name, value);
        }
    }
    Ok((object, extensions))
}

/// Returns an optional field, treating explicit null as absent.
pub(super) fn optional<'a>(object: &'a Map<String, Value>, name: &str) -> Option<&'a Value> {
    object.get(name).filter(|value| !value.is_null())
}

/// Decodes the no-storage preference: absent or false, never durable storage.
pub(super) fn decode_store(object: &Map<String, Value>) -> Result<Option<bool>, OpenAiResponse> {
    match optional(object, "store") {
        None => Ok(None),
        Some(Value::Bool(false)) => Ok(Some(false)),
        Some(Value::Bool(true)) => Err(request_error(InvalidRequest::unsupported_feature(Some(
            "store",
        )))),
        Some(_) => Err(request_error(InvalidRequest::invalid_field(Some("store")))),
    }
}

fn validated(request: ChatRequest) -> Result<ChatRequest, OpenAiResponse> {
    request_controls::validate(&request).map_err(request_error)?;
    Ok(request)
}

fn decode_chat(body: &[u8]) -> Result<ChatRequest, OpenAiResponse> {
    let (object, mut extensions) = split_request(
        body,
        &[
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
        ],
    )?;
    let object = &object;
    // A null choice count means the default, like any other null control.
    if extensions
        .get("n")
        .is_some_and(|choices| choices.get() == "null")
    {
        extensions.remove("n");
    }
    if let Some(choices) = extensions.get("n")
        && !serde_json::from_str::<u64>(choices.get()).is_ok_and(|choices| choices >= 1)
    {
        return Err(request_error(InvalidRequest::invalid_field(Some("n"))));
    }
    let model_id = string(required(object, "model")?, "model")?.to_owned();
    let messages = array(required(object, "messages")?, "messages")?;
    if messages.is_empty() {
        return Err(error(
            400,
            "invalid_request_error",
            "messages must not be empty.",
        ));
    }
    let messages = messages
        .iter()
        .map(decode_message)
        .collect::<Result<Vec<_>, _>>()?;
    let tools = object
        .get("tools")
        .map(|value| array(value, "tools"))
        .transpose()?
        .map(|tools| tools.iter().map(decode_tool).collect())
        .transpose()?
        .unwrap_or_default();
    let tool_choice = object
        .get("tool_choice")
        .map(decode_tool_choice)
        .transpose()?;
    let parallel_tool_calls = object
        .get("parallel_tool_calls")
        .map(|value| boolean(value, "parallel_tool_calls"))
        .transpose()?;
    let stream = object
        .get("stream")
        .map(|value| boolean(value, "stream"))
        .transpose()?;
    let include_stream_usage = object
        .get("stream_options")
        .map(decode_stream_options)
        .transpose()?
        .flatten();
    let controls = decode_controls(object)?;
    Ok(ChatRequest {
        model_id,
        messages,
        tools,
        tool_choice,
        parallel_tool_calls,
        generation_controls: Some(controls),
        stream,
        include_stream_usage,
        request_extensions_json: request_controls::extensions_json(&extensions),
        store: decode_store(object)?,
    })
}

fn decode_message(value: &Value) -> Result<ChatMessage, OpenAiResponse> {
    let object = object(value, "message")?;
    reject_unknown(
        object,
        &[
            "role",
            "content",
            "tool_calls",
            "tool_call_id",
            "name",
            "reasoning",
            "reasoning_content",
            "reasoning_text",
        ],
    )?;
    let role = match string(required(object, "role")?, "message role")? {
        "system" => ChatMessageRole::System,
        "developer" => ChatMessageRole::Developer,
        "user" => ChatMessageRole::User,
        "assistant" => ChatMessageRole::Assistant,
        "tool" => ChatMessageRole::Tool,
        _ => {
            return Err(error(
                400,
                "invalid_request_error",
                "message role is invalid.",
            ));
        }
    };
    let content = match object.get("content") {
        Some(Value::String(value)) => Some(chat_message::Content::TextContent(value.clone())),
        Some(Value::Null) => Some(chat_message::Content::NullContent(
            NullValue::NullValue as i32,
        )),
        Some(Value::Array(parts)) if !parts.is_empty() => None,
        Some(_) => {
            return Err(error(
                400,
                "invalid_request_error",
                "message content must be text or null.",
            ));
        }
        None => None,
    };
    let tool_calls = object
        .get("tool_calls")
        .map(|value| array(value, "tool_calls"))
        .transpose()?
        .map(|calls| calls.iter().map(decode_tool_call).collect())
        .transpose()?
        .unwrap_or_default();
    let content = if role == ChatMessageRole::Tool {
        let tool_call_id = string(required(object, "tool_call_id")?, "tool_call_id")?.to_owned();
        let Some(chat_message::Content::TextContent(content)) = content else {
            return Err(error(
                400,
                "invalid_request_error",
                "tool messages require text content.",
            ));
        };
        Some(chat_message::Content::ToolResult(FunctionToolResult {
            tool_call_id,
            content,
        }))
    } else {
        content
    };
    let content_parts = if role == ChatMessageRole::Tool {
        Vec::new()
    } else {
        object
            .get("content")
            .and_then(Value::as_array)
            .map(|parts| parts.iter().map(decode_content_part).collect())
            .transpose()?
            .unwrap_or_default()
    };
    let reasoning = reasoning_alias(object).map_err(|error| match error {
        ReasoningAliasError::NotText => error_response(
            400,
            "invalid_request_error",
            "message reasoning must be a string.",
            Some("messages"),
            Some("invalid_value"),
        ),
        ReasoningAliasError::Conflict => error_response(
            400,
            "invalid_request_error",
            "message reasoning aliases carry different values.",
            Some("messages"),
            Some("invalid_value"),
        ),
    })?;
    if reasoning.is_some() && role != ChatMessageRole::Assistant {
        return Err(error(
            400,
            "invalid_request_error",
            "Only assistant messages may include reasoning.",
        ));
    }
    Ok(ChatMessage {
        role: role as i32,
        content,
        tool_calls,
        name: object
            .get("name")
            .map(|value| string(value, "name").map(str::to_owned))
            .transpose()?,
        content_parts,
        reasoning,
    })
}

fn decode_content_part(value: &Value) -> Result<ChatContentPart, OpenAiResponse> {
    let part = object(value, "content part")?;
    match string(required(part, "type")?, "content part type")? {
        "text" => {
            reject_unknown(part, &["type", "text"])?;
            Ok(ChatContentPart {
                content: Some(chat_content_part::Content::TextContent(
                    string(required(part, "text")?, "content text")?.to_owned(),
                )),
            })
        }
        "image_url" => {
            reject_unknown(part, &["type", "image_url"])?;
            let image = object(required(part, "image_url")?, "image_url")?;
            reject_unknown(image, &["url", "detail"])?;
            let image = images::decode_data_url(
                string(required(image, "url")?, "image_url.url")?,
                image
                    .get("detail")
                    .map(|value| string(value, "image_url.detail"))
                    .transpose()?
                    .unwrap_or("auto"),
            )
            .map_err(|cause| image_error(&cause))?;
            Ok(ChatContentPart {
                content: Some(chat_content_part::Content::ImageContent(image)),
            })
        }
        _ => Err(error(
            400,
            "unsupported_feature",
            "This Chat content part is not supported.",
        )),
    }
}

fn decode_tool(value: &Value) -> Result<FunctionToolDefinition, OpenAiResponse> {
    let tool = object(value, "tool")?;
    reject_unknown(tool, &["type", "function"])?;
    if string(required(tool, "type")?, "tool type")? != "function" {
        return Err(error(
            400,
            "invalid_request_error",
            "Only client-defined function tools are supported.",
        ));
    }
    let function = object(required(tool, "function")?, "function")?;
    reject_unknown(function, &["name", "description", "parameters", "strict"])?;
    // Canonical validation refuses a schema that is not a JSON object.
    let parameters = required(function, "parameters")?;
    Ok(FunctionToolDefinition {
        name: string(required(function, "name")?, "function name")?.to_owned(),
        description: function
            .get("description")
            .map(|value| string(value, "function description").map(str::to_owned))
            .transpose()?,
        parameters_json_schema: serde_json::to_string(parameters).expect("JSON schema"),
        strict: function
            .get("strict")
            .map(|value| boolean(value, "function strict"))
            .transpose()?,
    })
}

fn decode_tool_call(value: &Value) -> Result<FunctionToolCall, OpenAiResponse> {
    let call = object(value, "tool call")?;
    reject_unknown(call, &["id", "type", "function"])?;
    if string(required(call, "type")?, "tool call type")? != "function" {
        return Err(error(
            400,
            "invalid_request_error",
            "Only function tool calls are supported.",
        ));
    }
    let function = object(required(call, "function")?, "tool function")?;
    reject_unknown(function, &["name", "arguments"])?;
    Ok(FunctionToolCall {
        tool_call_id: string(required(call, "id")?, "tool call id")?.to_owned(),
        function_name: string(required(function, "name")?, "tool function name")?.to_owned(),
        arguments_json: string(required(function, "arguments")?, "tool arguments")?.to_owned(),
    })
}

fn decode_tool_choice(value: &Value) -> Result<ChatToolChoice, OpenAiResponse> {
    match value {
        Value::String(value) => {
            let mode = match value.as_str() {
                "none" => ChatToolChoiceMode::None,
                "auto" => ChatToolChoiceMode::Auto,
                "required" => ChatToolChoiceMode::Required,
                _ => {
                    return Err(error(
                        400,
                        "invalid_request_error",
                        "tool_choice is invalid.",
                    ));
                }
            };
            Ok(ChatToolChoice {
                selection: Some(
                    dotcore_proto::dotcore::inference::v1::chat_tool_choice::Selection::Mode(
                        mode as i32,
                    ),
                ),
            })
        }
        Value::Object(choice) => {
            reject_unknown(choice, &["type", "function"])?;
            if string(required(choice, "type")?, "tool_choice type")? != "function" {
                return Err(error(
                    400,
                    "invalid_request_error",
                    "tool_choice must select a function.",
                ));
            }
            let function = object(required(choice, "function")?, "tool_choice function")?;
            Ok(ChatToolChoice { selection: Some(dotcore_proto::dotcore::inference::v1::chat_tool_choice::Selection::FunctionName(string(required(function, "name")?, "tool_choice function name")?.to_owned())) })
        }
        _ => Err(error(
            400,
            "invalid_request_error",
            "tool_choice is invalid.",
        )),
    }
}

fn decode_stream_options(value: &Value) -> Result<Option<bool>, OpenAiResponse> {
    let object = object(value, "stream_options")?;
    reject_unknown(object, &["include_usage"])?;
    object
        .get("include_usage")
        .map(|value| boolean(value, "stream_options.include_usage"))
        .transpose()
}

fn decode_controls(object: &Map<String, Value>) -> Result<ChatGenerationControls, OpenAiResponse> {
    let stop_sequences = optional(object, "stop").map(decode_stop).transpose()?;
    Ok(ChatGenerationControls {
        temperature: optional(object, "temperature")
            .map(|value| number(value, "temperature").map(|value| value as f32))
            .transpose()?,
        top_p: optional(object, "top_p")
            .map(|value| number(value, "top_p").map(|value| value as f32))
            .transpose()?,
        max_tokens: optional(object, "max_tokens")
            .map(|value| unsigned(value, "max_tokens"))
            .transpose()?,
        max_completion_tokens: optional(object, "max_completion_tokens")
            .map(|value| unsigned(value, "max_completion_tokens"))
            .transpose()?,
        stop_sequences,
        reasoning_effort: optional(object, "reasoning_effort")
            .map(|value| string(value, "reasoning_effort").map(str::to_owned))
            .transpose()?,
        presence_penalty: optional(object, "presence_penalty")
            .map(|value| number(value, "presence_penalty"))
            .transpose()?,
        frequency_penalty: optional(object, "frequency_penalty")
            .map(|value| number(value, "frequency_penalty"))
            .transpose()?,
    })
}

fn decode_stop(
    value: &Value,
) -> Result<dotcore_proto::dotcore::inference::v1::StopSequences, OpenAiResponse> {
    let values = match value {
        Value::String(value) => vec![value.clone()],
        Value::Array(values) => values
            .iter()
            .map(|value| string(value, "stop value").map(str::to_owned))
            .collect::<Result<Vec<_>, _>>()?,
        _ => {
            return Err(error(
                400,
                "invalid_request_error",
                "stop must be a string or string array.",
            ));
        }
    };
    Ok(dotcore_proto::dotcore::inference::v1::StopSequences { values })
}

fn completion_json(result: ChatCompletionResult) -> Value {
    let mut value = json!({"id":result.completion_id,"object":"chat.completion","created":result.created_at_unix_seconds,"model":result.model_id,"choices":result.choices.into_iter().map(|choice| json!({"index":choice.index,"message":choice.message.map(message_json),"finish_reason":finish_reason(choice.finish_reason)})).collect::<Vec<_>>()});
    if let Some(usage) = result.usage {
        value
            .as_object_mut()
            .expect("completion object")
            .insert("usage".to_owned(), usage_json(usage));
    }
    value
}
fn message_json(message: ChatMessage) -> Value {
    let mut object = Map::new();
    object.insert("role".to_owned(), json!(role(message.role)));
    if let Some(content) = message.content {
        match content {
            chat_message::Content::TextContent(content) => {
                object.insert("content".to_owned(), json!(content));
            }
            chat_message::Content::NullContent(_) => {
                object.insert("content".to_owned(), Value::Null);
            }
            chat_message::Content::ToolResult(result) => {
                object.insert("content".to_owned(), json!(result.content));
                object.insert("tool_call_id".to_owned(), json!(result.tool_call_id));
            }
        }
    }
    if !message.tool_calls.is_empty() {
        object.insert("tool_calls".to_owned(), Value::Array(message.tool_calls.into_iter().map(|call|json!({"id":call.tool_call_id,"type":"function","function":{"name":call.function_name,"arguments":call.arguments_json}})).collect()));
    }
    if let Some(name) = message.name {
        object.insert("name".to_owned(), json!(name));
    }
    if let Some(reasoning) = message.reasoning {
        object.insert("reasoning".to_owned(), json!(reasoning));
    }
    Value::Object(object)
}
fn usage_json(usage: TokenUsage) -> Value {
    let mut value = json!({"prompt_tokens":usage.prompt_tokens,"completion_tokens":usage.completion_tokens,"total_tokens":usage.total_tokens});
    let object = value.as_object_mut().expect("usage object");
    if let Some(cached_tokens) = usage.cached_prompt_tokens {
        object.insert(
            "prompt_tokens_details".to_owned(),
            json!({"cached_tokens":cached_tokens}),
        );
    }
    if let Some(reasoning_tokens) = usage.reasoning_tokens {
        object.insert(
            "completion_tokens_details".to_owned(),
            json!({"reasoning_tokens":reasoning_tokens}),
        );
    }
    value
}
fn finish_reason(value: i32) -> Value {
    match ChatFinishReason::try_from(value).unwrap_or(ChatFinishReason::Unspecified) {
        ChatFinishReason::Stop => json!("stop"),
        ChatFinishReason::Length => json!("length"),
        ChatFinishReason::ToolCalls => json!("tool_calls"),
        ChatFinishReason::ContentFilter => json!("content_filter"),
        ChatFinishReason::Unspecified => Value::Null,
    }
}
fn role(value: i32) -> &'static str {
    match ChatMessageRole::try_from(value).unwrap_or(ChatMessageRole::Assistant) {
        ChatMessageRole::System => "system",
        ChatMessageRole::Developer => "developer",
        ChatMessageRole::User => "user",
        ChatMessageRole::Assistant => "assistant",
        ChatMessageRole::Tool => "tool",
        ChatMessageRole::Unspecified => "assistant",
    }
}
fn stream_event_json(event: ChatStreamEvent) -> Option<Value> {
    match event.event? {
        Event::Started(started) => Some(
            json!({"id":started.completion_id,"object":"chat.completion.chunk","created":started.created_at_unix_seconds,"model":started.model_id,"choices":[]}),
        ),
        Event::TextDelta(delta) => {
            let mut value = json!({"object":"chat.completion.chunk","choices":[{"index":delta.choice_index,"delta":{},"finish_reason":null}]});
            let delta_object = value
                .pointer_mut("/choices/0/delta")
                .and_then(Value::as_object_mut)
                .expect("text delta object");
            if let Some(role_value) = delta.role {
                delta_object.insert("role".to_owned(), json!(role(role_value)));
            }
            if let Some(content) = delta.content {
                delta_object.insert(
                    "content".to_owned(),
                    match content {
                        chat_text_delta::Content::TextContent(content) => json!(content),
                        chat_text_delta::Content::NullContent(_) => Value::Null,
                    },
                );
            }
            Some(value)
        }
        Event::ReasoningDelta(delta) => Some(
            json!({"object":"chat.completion.chunk","choices":[{"index":delta.choice_index,"delta":{"reasoning":delta.reasoning},"finish_reason":null}]}),
        ),
        Event::ToolCallDelta(delta) => {
            let mut function = Map::new();
            if let Some(name) = delta.function_name {
                function.insert("name".to_owned(), json!(name));
            }
            if let Some(arguments) = delta.arguments_json_fragment {
                function.insert("arguments".to_owned(), json!(arguments));
            }
            let mut call = Map::new();
            call.insert("index".to_owned(), json!(delta.tool_call_index));
            if let Some(id) = delta.tool_call_id {
                call.insert("id".to_owned(), json!(id));
            }
            call.insert("type".to_owned(), json!("function"));
            call.insert("function".to_owned(), Value::Object(function));
            Some(
                json!({"object":"chat.completion.chunk","choices":[{"index":delta.choice_index,"delta":{"tool_calls":[Value::Object(call)]},"finish_reason":null}]}),
            )
        }
        Event::Finished(finished) => Some(
            json!({"object":"chat.completion.chunk","choices":[{"index":finished.choice_index,"delta":{},"finish_reason":finish_reason(finished.finish_reason)}]}),
        ),
        Event::Usage(usage) => {
            Some(json!({"object":"chat.completion.chunk","choices":[],"usage":usage_json(usage)}))
        }
        Event::Completed(_) => None,
    }
}

fn error_response(
    status: u16,
    kind: &str,
    message: &str,
    param: Option<&str>,
    code: Option<&str>,
) -> OpenAiResponse {
    json_response(
        status,
        json!({"error":{"message":message,"type":kind,"param":param,"code":code}}),
    )
}

fn json_response(status: u16, value: Value) -> OpenAiResponse {
    OpenAiResponse {
        status,
        content_type: "application/json".to_owned(),
        body: serde_json::to_vec(&value).expect("JSON response"),
        passthrough_headers: Vec::new(),
    }
}
fn required<'a>(object: &'a Map<String, Value>, name: &str) -> Result<&'a Value, OpenAiResponse> {
    object.get(name).ok_or_else(|| {
        error(
            400,
            "invalid_request_error",
            &format!("{name} is required."),
        )
    })
}
fn object<'a>(value: &'a Value, name: &str) -> Result<&'a Map<String, Value>, OpenAiResponse> {
    value.as_object().ok_or_else(|| {
        error(
            400,
            "invalid_request_error",
            &format!("{name} must be an object."),
        )
    })
}
fn array<'a>(value: &'a Value, name: &str) -> Result<&'a Vec<Value>, OpenAiResponse> {
    value.as_array().ok_or_else(|| {
        error(
            400,
            "invalid_request_error",
            &format!("{name} must be an array."),
        )
    })
}
fn string<'a>(value: &'a Value, name: &str) -> Result<&'a str, OpenAiResponse> {
    value.as_str().ok_or_else(|| {
        error(
            400,
            "invalid_request_error",
            &format!("{name} must be a string."),
        )
    })
}
fn boolean(value: &Value, name: &str) -> Result<bool, OpenAiResponse> {
    value.as_bool().ok_or_else(|| {
        error(
            400,
            "invalid_request_error",
            &format!("{name} must be a boolean."),
        )
    })
}
fn number(value: &Value, name: &str) -> Result<f64, OpenAiResponse> {
    value.as_f64().ok_or_else(|| {
        error(
            400,
            "invalid_request_error",
            &format!("{name} must be a number."),
        )
    })
}
fn unsigned(value: &Value, name: &str) -> Result<u32, OpenAiResponse> {
    value
        .as_u64()
        .and_then(|value| u32::try_from(value).ok())
        .ok_or_else(|| {
            error(
                400,
                "invalid_request_error",
                &format!("{name} must be an unsigned 32-bit integer."),
            )
        })
}
fn reject_unknown(object: &Map<String, Value>, allowed: &[&str]) -> Result<(), OpenAiResponse> {
    let allowed = allowed.iter().copied().collect::<BTreeSet<_>>();
    if object.keys().any(|key| !allowed.contains(key.as_str())) {
        return Err(error(
            400,
            "invalid_request_error",
            "Request contains an unsupported field.",
        ));
    }
    Ok(())
}

#[cfg(test)]
pub(crate) fn stream(events: Vec<ChatStreamEvent>) -> OpenAiResponse {
    let mut body = Vec::new();
    let mut stream_id = None;
    let mut stream_model = None;
    let mut stream_created = None;
    for event in events {
        if let Some(Event::Started(started)) = event.event.as_ref() {
            stream_id = Some(started.completion_id.clone());
            stream_model = Some(started.model_id.clone());
            stream_created = Some(started.created_at_unix_seconds);
        }
        let Some(mut value) = stream_event_json(event) else {
            continue;
        };
        if let Value::Object(object) = &mut value {
            if let Some(id) = &stream_id {
                object.insert("id".to_owned(), json!(id));
            }
            if let Some(model) = &stream_model {
                object.insert("model".to_owned(), json!(model));
            }
            if let Some(created) = stream_created {
                object.insert("created".to_owned(), json!(created));
            }
        }
        body.extend_from_slice(b"data: ");
        body.extend_from_slice(
            serde_json::to_string(&value)
                .expect("JSON event")
                .as_bytes(),
        );
        body.extend_from_slice(b"\n\n");
    }
    body.extend_from_slice(b"data: [DONE]\n\n");
    OpenAiResponse {
        status: 200,
        content_type: "text/event-stream".to_owned(),
        body,
        passthrough_headers: Vec::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::router::test_fixtures;

    #[test]
    fn shared_chat_fixture_preserves_public_codec_fidelity() {
        assert_eq!(test_fixtures::CHAT_FIXTURE_VERSION, "chat-v1");
        let body = serde_json::to_vec(&test_fixtures::chat_request()).expect("fixture JSON");
        let OpenAiRequest::Chat(request) =
            decode("POST", "/v1/chat/completions", &body).expect("Chat request")
        else {
            panic!("fixture must decode as Chat")
        };
        assert!(request.messages[1].content.is_none());
        assert!(matches!(
            request.messages[2].content.as_ref(),
            Some(chat_message::Content::NullContent(_))
        ));
        assert!(matches!(
            request.messages[3].content.as_ref(),
            Some(chat_message::Content::ToolResult(_))
        ));

        let completion_body =
            serde_json::to_vec(&test_fixtures::completion()).expect("fixture JSON");
        let result = crate::router::openai_compatible_codec::decode_completion(&completion_body)
            .expect("completion fixture");
        assert_eq!(
            serde_json::from_slice::<Value>(&completion(result, "fixture-route").body)
                .expect("completion JSON"),
            test_fixtures::completion()
        );

        let events = crate::router::openai_compatible_codec::decode_sse(
            &test_fixtures::fragmented_stream_body(),
        )
        .expect("fragmented Chat stream");
        let encoded = stream(events.clone());
        assert!(
            std::str::from_utf8(&encoded.body)
                .expect("Chat SSE")
                .ends_with("data: [DONE]\n\n")
        );
        assert_eq!(
            crate::router::openai_compatible_codec::decode_sse(&encoded.body)
                .expect("Chat SSE roundtrip"),
            events
        );
    }

    #[test]
    fn stream_usage_chunk_is_emitted_only_when_the_client_asked() {
        let usage = ChatStreamEvent {
            event: Some(Event::Usage(TokenUsage {
                prompt_tokens: 2,
                completion_tokens: 1,
                total_tokens: 3,
                ..Default::default()
            })),
        };

        let mut hidden = StreamEncoder::new("public-route".to_owned(), false);
        assert_eq!(hidden.encode(usage.clone()), None);

        let mut shown = StreamEncoder::new("public-route".to_owned(), true);
        let frame = shown.encode(usage).expect("usage frame");
        let frame = std::str::from_utf8(&frame).expect("UTF-8 frame");
        let chunk = serde_json::from_str::<Value>(
            frame
                .strip_prefix("data: ")
                .and_then(|frame| frame.strip_suffix("\n\n"))
                .expect("SSE data frame"),
        )
        .expect("usage chunk JSON");
        assert_eq!(chunk["usage"]["total_tokens"], 3);
        assert_eq!(chunk["model"], "public-route");
    }

    #[test]
    fn public_completion_and_stream_use_the_admitted_route_name() {
        let completion = completion(
            ChatCompletionResult {
                model_id: "catalog-model".to_owned(),
                ..Default::default()
            },
            "public-route",
        );
        assert_eq!(
            serde_json::from_slice::<Value>(&completion.body).expect("completion JSON")["model"],
            "public-route"
        );

        let mut encoder = StreamEncoder::new("public-route".to_owned(), true);
        let frame = encoder
            .encode(ChatStreamEvent {
                event: Some(Event::Started(
                    dotcore_proto::dotcore::inference::v1::ChatStreamStarted {
                        completion_id: "chatcmpl_1".to_owned(),
                        model_id: "catalog-model".to_owned(),
                        created_at_unix_seconds: 1,
                    },
                )),
            })
            .expect("started frame");
        let frame = std::str::from_utf8(&frame)
            .expect("SSE frame")
            .strip_prefix("data: ")
            .and_then(|frame| frame.strip_suffix("\n\n"))
            .expect("SSE data");
        assert_eq!(
            serde_json::from_str::<Value>(frame).expect("SSE JSON")["model"],
            "public-route"
        );
    }

    #[test]
    fn public_chat_keeps_reasoning_separate_from_final_content() {
        let response = completion(
            ChatCompletionResult {
                completion_id: "chatcmpl_1".to_owned(),
                model_id: "catalog-model".to_owned(),
                created_at_unix_seconds: 1,
                choices: vec![
                    dotcore_proto::dotcore::inference::v1::ChatCompletionChoice {
                        index: 0,
                        message: Some(ChatMessage {
                            role: ChatMessageRole::Assistant as i32,
                            content: Some(chat_message::Content::TextContent("answer".to_owned())),
                            reasoning: Some("analysis".to_owned()),
                            ..Default::default()
                        }),
                        finish_reason: ChatFinishReason::Stop as i32,
                    },
                ],
                usage: None,
            },
            "public-route",
        );
        let value = serde_json::from_slice::<Value>(&response.body).expect("completion JSON");
        assert_eq!(
            value.pointer("/choices/0/message/reasoning"),
            Some(&json!("analysis"))
        );
        assert_eq!(
            value.pointer("/choices/0/message/content"),
            Some(&json!("answer"))
        );

        let OpenAiRequest::Chat(request) = decode(
            "POST",
            "/v1/chat/completions",
            br#"{"model":"public-route","messages":[{"role":"assistant","content":"answer","reasoning":"analysis"}]}"#,
        )
        .expect("assistant history with reasoning")
        else {
            panic!("request must decode as Chat")
        };
        assert_eq!(request.messages[0].reasoning.as_deref(), Some("analysis"));

        let mut encoder = StreamEncoder::new("public-route".to_owned(), true);
        let frame = encoder
            .encode(ChatStreamEvent {
                event: Some(Event::ReasoningDelta(
                    dotcore_proto::dotcore::inference::v1::ChatReasoningDelta {
                        choice_index: 0,
                        reasoning: "step".to_owned(),
                    },
                )),
            })
            .expect("reasoning frame");
        let data = std::str::from_utf8(&frame)
            .expect("SSE frame")
            .strip_prefix("data: ")
            .and_then(|value| value.strip_suffix("\n\n"))
            .expect("SSE data");
        let value = serde_json::from_str::<Value>(data).expect("SSE JSON");
        assert_eq!(
            value.pointer("/choices/0/delta/reasoning"),
            Some(&json!("step"))
        );
        assert!(value.pointer("/choices/0/delta/content").is_none());
    }

    #[test]
    fn upstream_completion_rejects_missing_or_malformed_required_fields() {
        for completion in [
            json!({"model":"model","created":1,"choices":[]}),
            json!({"id":"chatcmpl_1","created":1,"choices":[]}),
            json!({"id":"chatcmpl_1","model":"model","choices":[]}),
            json!({"id":"chatcmpl_1","model":"model","created":1,"choices":[]}),
            json!({"id":"chatcmpl_1","model":"model","created":1,"choices":{}}),
            json!({"id":"chatcmpl_1","model":"model","created":1,"choices":[{"message":{"role":"assistant","content":"ok"},"finish_reason":"stop"}]}),
            json!({"id":"chatcmpl_1","model":"model","created":1,"choices":[{"index":0,"finish_reason":"stop"}]}),
            json!({"id":"chatcmpl_1","model":"model","created":1,"choices":[{"index":0,"message":{"role":"assistant","content":"ok"}}]}),
            json!({"id":"chatcmpl_1","model":"model","created":1,"choices":[{"index":0,"message":{"role":"invalid","content":"ok"},"finish_reason":"stop"}]}),
            json!({"id":"chatcmpl_1","model":"model","created":1,"choices":[{"index":0,"message":{"role":"assistant","content":7},"finish_reason":"stop"}]}),
            json!({"id":"chatcmpl_1","model":"model","created":1,"choices":[{"index":0,"message":{"role":"assistant","content":null,"tool_calls":[{"type":"function","function":{"name":"read","arguments":"{}"}}]},"finish_reason":"tool_calls"}]}),
            json!({"id":"chatcmpl_1","model":"model","created":1,"choices":[{"index":0,"message":{"role":"assistant","content":null,"tool_calls":[{"id":"call_1","function":{"name":"read","arguments":"{}"}}]},"finish_reason":"tool_calls"}]}),
            json!({"id":"chatcmpl_1","model":"model","created":1,"choices":[{"index":0,"message":{"role":"assistant","content":null,"tool_calls":[{"id":"call_1","type":"function"}]},"finish_reason":"tool_calls"}]}),
            json!({"id":"chatcmpl_1","model":"model","created":1,"choices":[{"index":0,"message":{"role":"assistant","content":null,"tool_calls":[{"id":"call_1","type":"function","function":{"arguments":"{}"}}]},"finish_reason":"tool_calls"}]}),
            json!({"id":"chatcmpl_1","model":"model","created":1,"choices":[{"index":0,"message":{"role":"assistant","content":null,"tool_calls":[{"id":"call_1","type":"function","function":{"name":"read"}}]},"finish_reason":"tool_calls"}]}),
            json!({"id":"chatcmpl_1","model":"model","created":1,"choices":[{"index":0,"message":{"role":"assistant","content":null,"tool_calls":[{"id":"call_1","type":"function","function":{"name":"read","arguments":{}}}]},"finish_reason":"tool_calls"}]}),
            json!({"id":"chatcmpl_1","model":"model","created":1,"choices":[{"index":0,"message":{"role":"assistant","content":"ok"},"finish_reason":"unknown"}]}),
            json!({"id":"chatcmpl_1","model":"model","created":1,"choices":[],"usage":{"prompt_tokens":1,"completion_tokens":2}}),
        ] {
            assert!(
                crate::router::openai_compatible_codec::decode_completion(
                    &serde_json::to_vec(&completion).expect("JSON")
                )
                .is_err(),
                "malformed completion was accepted: {completion}"
            );
        }
    }

    #[test]
    fn upstream_completion_enforces_tool_call_role_and_required_call_consistency() {
        let call = json!({
            "id":"call_1",
            "type":"function",
            "function":{"name":"read","arguments":"{}"}
        });
        for choice in [
            json!({"index":0,"message":{"role":"user","content":null,"tool_calls":[call.clone()]},"finish_reason":"tool_calls"}),
            json!({"index":0,"message":{"role":"assistant","content":null},"finish_reason":"tool_calls"}),
            json!({"index":0,"message":{"role":"assistant","content":null,"tool_calls":[]},"finish_reason":"tool_calls"}),
        ] {
            let completion = json!({
                "id":"chatcmpl_1",
                "model":"model",
                "created":1,
                "choices":[choice]
            });
            assert!(
                crate::router::openai_compatible_codec::decode_completion(
                    &serde_json::to_vec(&completion).expect("JSON")
                )
                .is_err(),
                "inconsistent tool completion was accepted: {completion}"
            );
        }
    }

    #[test]
    fn codec_preserves_tool_arguments_and_stream_done() {
        let request=decode("POST","/v1/chat/completions",br#"{"model":"model","messages":[{"role":"user","content":"hello"}],"tools":[{"type":"function","function":{"name":"read","parameters":{"type":"object"}}}]}"#).expect("request");
        let OpenAiRequest::Chat(request) = request else {
            panic!("Chat")
        };
        assert_eq!(
            request.tools[0].parameters_json_schema,
            "{\"type\":\"object\"}"
        );
        let response = stream(vec![ChatStreamEvent {
            event: Some(Event::Completed(Default::default())),
        }]);
        assert!(
            std::str::from_utf8(&response.body)
                .expect("SSE")
                .ends_with("data: [DONE]\n\n")
        );
    }

    #[test]
    fn pi_shaped_request_decodes_every_control_exactly() {
        let OpenAiRequest::Chat(request) = decode(
            "POST",
            "/v1/chat/completions",
            test_fixtures::PI_CHAT_REQUEST.as_bytes(),
        )
        .expect("Pi request") else {
            panic!("Pi fixture must decode as Chat")
        };

        let text = |value: &str| Some(chat_message::Content::TextContent(value.to_owned()));
        assert_eq!(
            request,
            ChatRequest {
                model_id: "qwen3-8-27b".to_owned(),
                messages: vec![
                    ChatMessage {
                        role: ChatMessageRole::Developer as i32,
                        content: text("You are Pi."),
                        ..Default::default()
                    },
                    ChatMessage {
                        role: ChatMessageRole::User as i32,
                        content: text("Read the README."),
                        ..Default::default()
                    },
                    ChatMessage {
                        role: ChatMessageRole::Assistant as i32,
                        content: Some(chat_message::Content::NullContent(
                            NullValue::NullValue as i32,
                        )),
                        tool_calls: vec![FunctionToolCall {
                            tool_call_id: "call_1".to_owned(),
                            function_name: "read".to_owned(),
                            arguments_json: r#"{"path":"README.md"}"#.to_owned(),
                        }],
                        reasoning: Some("The file is needed.".to_owned()),
                        ..Default::default()
                    },
                    ChatMessage {
                        role: ChatMessageRole::Tool as i32,
                        content: Some(chat_message::Content::ToolResult(FunctionToolResult {
                            tool_call_id: "call_1".to_owned(),
                            content: "# Readme".to_owned(),
                        })),
                        ..Default::default()
                    },
                ],
                tools: vec![FunctionToolDefinition {
                    name: "read".to_owned(),
                    description: Some("Read a file.".to_owned()),
                    parameters_json_schema:
                        r#"{"properties":{"path":{"type":"string"}},"required":["path"],"type":"object"}"#
                            .to_owned(),
                    strict: Some(false),
                }],
                tool_choice: None,
                parallel_tool_calls: None,
                generation_controls: Some(ChatGenerationControls {
                    temperature: Some(1.0),
                    top_p: Some(0.95),
                    max_tokens: None,
                    max_completion_tokens: Some(32000),
                    stop_sequences: None,
                    reasoning_effort: Some("none".to_owned()),
                    presence_penalty: Some(0.0),
                    frequency_penalty: Some(0.0),
                }),
                stream: Some(true),
                include_stream_usage: Some(true),
                request_extensions_json: Some(test_fixtures::PI_CHAT_EXTENSIONS.to_owned()),
                store: Some(false),
            }
        );

        let xhigh = test_fixtures::PI_CHAT_REQUEST.replace(
            r#""reasoning_effort":"none""#,
            r#""reasoning_effort":"xhigh""#,
        );
        let OpenAiRequest::Chat(request) =
            decode("POST", "/v1/chat/completions", xhigh.as_bytes()).expect("xhigh request")
        else {
            panic!("xhigh fixture must decode as Chat")
        };
        assert_eq!(
            request
                .generation_controls
                .and_then(|controls| controls.reasoning_effort)
                .as_deref(),
            Some("xhigh")
        );
    }

    #[test]
    fn controls_keep_their_original_number_text_through_admission() {
        let controls = test_fixtures::EXACT_NUMBER_EXTENSIONS;
        let body = format!(
            r#"{{"model":"route","messages":[{{"role":"user","content":"hi"}}],{}}}"#,
            &controls[1..controls.len() - 1]
        );

        let OpenAiRequest::Chat(request) =
            decode("POST", "/v1/chat/completions", body.as_bytes()).expect("Chat request")
        else {
            panic!("fixture must decode as Chat")
        };

        assert_eq!(request.request_extensions_json.as_deref(), Some(controls));
        assert_eq!(request.store, None, "an omitted store stays absent");
    }

    #[test]
    fn chat_forwards_positive_choice_counts_and_absent_null_controls() {
        let OpenAiRequest::Chat(request) = decode(
            "POST",
            "/v1/chat/completions",
            br#"{"model":"route","messages":[{"role":"user","content":"hi"}],"n":2,"temperature":null,"store":null,"user":"client-7"}"#,
        )
        .expect("Chat request")
        else {
            panic!("fixture must decode as Chat")
        };

        assert_eq!(
            request.request_extensions_json.as_deref(),
            Some(r#"{"n":2,"user":"client-7"}"#)
        );
        assert_eq!(
            request
                .generation_controls
                .and_then(|controls| controls.temperature),
            None
        );
        assert_eq!(request.store, None);
    }

    #[test]
    fn invalid_controls_fail_with_safe_categories_and_recognized_fields() {
        let deep = format!(
            r#"{{"model":"route","messages":[{{"role":"user","content":"hi"}}],"nested":{}0,{}}}"#,
            "[".repeat(200),
            "]".repeat(200)
        );
        let invalid_value = |param: &str| json!({"error":{"message":format!("Request field {param} is invalid."),"type":"invalid_request_error","param":param,"code":"invalid_value"}});
        let unsupported = |param: &str| json!({"error":{"message":format!("Request field {param} is not supported."),"type":"unsupported_feature","param":param,"code":"unsupported_parameter"}});
        let cases = [
            (
                r#"{"model":"route","messages":[{"role":"user","content":"hi"}],"store":true}"#.to_owned(),
                unsupported("store"),
            ),
            (
                r#"{"model":"route","messages":[{"role":"user","content":"hi"}],"store":"false"}"#.to_owned(),
                invalid_value("store"),
            ),
            (
                r#"{"model":"route","messages":[{"role":"assistant","content":"a","reasoning":"one","reasoning_content":"two"}]}"#.to_owned(),
                json!({"error":{"message":"message reasoning aliases carry different values.","type":"invalid_request_error","param":"messages","code":"invalid_value"}}),
            ),
            (
                r#"{"model":"route","messages":[{"role":"user","content":"hi"}],"tools":[{"type":"function","function":{"name":"read","parameters":["path"]}}]}"#.to_owned(),
                invalid_value("tools"),
            ),
            (
                r#"{"model":"route","messages":[{"role":"user","content":"hi"}],"input":"smuggled"}"#.to_owned(),
                invalid_value("input"),
            ),
            (
                r#"{"model":"route","messages":[{"role":"user","content":"hi"}],"n":0}"#.to_owned(),
                invalid_value("n"),
            ),
            (
                r#"{"model":"route","messages":[{"role":"user","content":"hi"}],"response_format":{"type":"json_object"}}"#.to_owned(),
                unsupported("response_format"),
            ),
            (
                r#"{"model":"route","messages":[{"role":"user","content":"hi"}],"functions":[]}"#.to_owned(),
                unsupported("functions"),
            ),
            (
                r#"{"model":"route","messages":[{"role":"user","content":"hi"}],"background":false}"#.to_owned(),
                unsupported("background"),
            ),
            (
                r#"{"model":"route","messages":[{"role":"user","content":"hi"}],"previous_response_id":"resp_1"}"#.to_owned(),
                unsupported("previous_response_id"),
            ),
            (
                r#"{"model":"route","messages":[{"role":"user","content":"hi"}],"api_key":"sk-sentinel-value"}"#.to_owned(),
                json!({"error":{"message":"The request contains an invalid engine control.","type":"invalid_request_error","param":null,"code":"invalid_value"}}),
            ),
            (
                deep,
                json!({"error":{"message":"Request body must be a valid JSON object.","type":"invalid_request_error","param":null,"code":null}}),
            ),
            (
                "[1]".to_owned(),
                json!({"error":{"message":"Request body must be a valid JSON object.","type":"invalid_request_error","param":null,"code":null}}),
            ),
            (
                r#"{"model":"route","messages":[{"role":"user","content":"hi"}],"temperature":1e400}"#.to_owned(),
                json!({"error":{"message":"Request body must be a valid JSON object.","type":"invalid_request_error","param":null,"code":null}}),
            ),
            (
                r#"{"model":"\ud800","messages":[{"role":"user","content":"hi"}]}"#.to_owned(),
                json!({"error":{"message":"Request body must be a valid JSON object.","type":"invalid_request_error","param":null,"code":null}}),
            ),
        ];

        for (body, expected) in cases {
            let Err(response) = decode("POST", "/v1/chat/completions", body.as_bytes()) else {
                panic!("{body} must be rejected");
            };
            assert_eq!(response.status, 400, "{body}");
            assert_eq!(
                serde_json::from_slice::<Value>(&response.body).expect("error JSON"),
                expected,
                "{body}"
            );
            assert!(!String::from_utf8_lossy(&response.body).contains("sentinel"));
        }
    }

    #[test]
    fn a_null_choice_count_is_absent_rather_than_forwarded() {
        let OpenAiRequest::Chat(request) = decode(
            "POST",
            "/v1/chat/completions",
            br#"{"model":"route","messages":[{"role":"user","content":"hi"}],"n":null,"top_k":1}"#,
        )
        .expect("Chat request") else {
            panic!("fixture must decode as Chat")
        };

        assert_eq!(
            request.request_extensions_json.as_deref(),
            Some(r#"{"top_k":1}"#)
        );
    }

    #[test]
    fn equal_reasoning_aliases_normalize_to_one_history_value() {
        for aliases in [
            r#""reasoning_content":"thought""#,
            r#""reasoning_text":"thought""#,
            r#""reasoning":"thought","reasoning_content":"thought","reasoning_text":null"#,
        ] {
            let body = format!(
                r#"{{"model":"route","messages":[{{"role":"assistant","content":"a",{aliases}}}]}}"#
            );
            let OpenAiRequest::Chat(request) =
                decode("POST", "/v1/chat/completions", body.as_bytes()).expect("Chat request")
            else {
                panic!("fixture must decode as Chat")
            };
            assert_eq!(
                request.messages[0].reasoning.as_deref(),
                Some("thought"),
                "{aliases}"
            );
        }
    }

    #[test]
    fn engine_rejections_name_only_recognized_fields() {
        for (param, expected) in [("top_k", json!("top_k")), ("sk-sentinel", Value::Null)] {
            let response = upstream(
                400,
                Vec::new(),
                serde_json::to_vec(
                    &json!({"error":{"message":"bad sk-sentinel value","param":param}}),
                )
                .expect("engine error JSON"),
            );
            assert_eq!(response.status, 400);
            assert_eq!(
                serde_json::from_slice::<Value>(&response.body).expect("error JSON"),
                json!({"error":{"message":"The inference engine rejected the request.","type":"upstream_error","param":expected,"code":null}})
            );
        }
    }

    #[test]
    fn usage_details_render_only_when_supplied() {
        assert_eq!(
            usage_json(TokenUsage {
                prompt_tokens: 30,
                completion_tokens: 12,
                total_tokens: 42,
                cached_prompt_tokens: Some(16),
                reasoning_tokens: Some(0),
            }),
            json!({"prompt_tokens":30,"completion_tokens":12,"total_tokens":42,"prompt_tokens_details":{"cached_tokens":16},"completion_tokens_details":{"reasoning_tokens":0}})
        );
        assert_eq!(
            usage_json(TokenUsage {
                prompt_tokens: 1,
                completion_tokens: 1,
                total_tokens: 2,
                ..Default::default()
            }),
            json!({"prompt_tokens":1,"completion_tokens":1,"total_tokens":2})
        );
    }
}
