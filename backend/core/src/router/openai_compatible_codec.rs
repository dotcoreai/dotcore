//! Encodes and decodes the shared OpenAI-compatible Chat wire contract.
//!
//! Local engines, configured upstream providers, and tests use one request encoding and one
//! response decoding, so a control or reasoning field survives every dispatch path the same way;
//! local engines also receive history reasoning under the field their chat templates read.
//! Engine-specific controls are merged into the outbound body as their original JSON text.

use std::collections::BTreeMap;

use anyhow::anyhow;
use dotcore_proto::dotcore::inference::v1::{
    ChatCompletionResult, ChatFinishReason, ChatMessage, ChatMessageRole, ChatReasoningDelta,
    ChatRequest, ChatStreamEvent, FunctionToolCall, FunctionToolResult, TokenUsage, chat_message,
    chat_stream_event::Event, chat_text_delta,
};
use dotcore_proto::google::protobuf::NullValue;
use serde_json::{Map, Value, value::RawValue};

use crate::{
    inference::InvalidRequest,
    router::{peer_service::PEER_INFERENCE_MESSAGE_LIMIT_BYTES, request_controls},
};

/// Encodes a validated Chat request body for a selected endpoint that serves the model under
/// `wire_model`, such as an engine's served model id or an upstream provider's model id, in
/// place of the public route name.
pub(crate) fn encode_request_for_model(
    request: &ChatRequest,
    wire_model: &str,
) -> Result<Vec<u8>, InvalidRequest> {
    encode_body(request, wire_model, HistoryReasoning::Reasoning)
}

/// Encodes a validated Chat request body for a local engine serving the model as
/// `served_model_id`.
///
/// History reasoning also travels as `reasoning_content`, the field local engines and model chat
/// templates read to replay it.
pub(crate) fn encode_request_for_local_engine(
    request: &ChatRequest,
    served_model_id: &str,
) -> Result<Vec<u8>, InvalidRequest> {
    encode_body(
        request,
        served_model_id,
        HistoryReasoning::WithReasoningContent,
    )
}

/// Encodes a validated Chat request body that names the request's own route.
#[cfg(test)]
pub(crate) fn encode_request(request: &ChatRequest) -> Result<Vec<u8>, InvalidRequest> {
    encode_request_for_model(request, &request.model_id)
}

/// Fields that carry an assistant message's reasoning in an outbound request.
#[derive(Clone, Copy)]
enum HistoryReasoning {
    /// `reasoning` only.
    Reasoning,

    /// `reasoning` and the same text as `reasoning_content`.
    WithReasoningContent,
}

fn encode_body(
    request: &ChatRequest,
    wire_model: &str,
    history_reasoning: HistoryReasoning,
) -> Result<Vec<u8>, InvalidRequest> {
    request_controls::validate(request)?;
    let mut body = BTreeMap::new();
    for (name, value) in typed_request_fields(request, history_reasoning)? {
        body.insert(name, raw_json(&value));
    }
    body.insert(
        "model".to_owned(),
        raw_json(&Value::String(wire_model.to_owned())),
    );
    for (name, value) in request_controls::request_extensions(request)? {
        // Validation already refuses typed names; this keeps the merge independent of order.
        if body.contains_key(&name) {
            return Err(InvalidRequest::invalid_field(
                request_controls::recognized_param(&name),
            ));
        }
        body.insert(name, value);
    }
    let body = serde_json::to_vec(&body).expect("raw JSON values serialize");
    if body.len() > PEER_INFERENCE_MESSAGE_LIMIT_BYTES {
        return Err(InvalidRequest::request_limit(None));
    }
    Ok(body)
}

fn typed_request_fields(
    request: &ChatRequest,
    history_reasoning: HistoryReasoning,
) -> Result<Map<String, Value>, InvalidRequest> {
    let messages = request
        .messages
        .iter()
        .map(|message| encode_message(message, history_reasoning))
        .collect::<Vec<_>>();
    let mut value = serde_json::json!({
        "model": request.model_id,
        "messages": messages,
        "stream": request.stream.unwrap_or(false),
    });
    let object = value.as_object_mut().expect("JSON object");
    if !request.tools.is_empty() {
        object.insert(
            "tools".to_owned(),
            Value::Array(
                request
                    .tools
                    .iter()
                    .map(encode_tool)
                    .collect::<Result<_, _>>()?,
            ),
        );
    }
    if let Some(choice) = &request.tool_choice {
        let choice = match &choice.selection {
            Some(dotcore_proto::dotcore::inference::v1::chat_tool_choice::Selection::Mode(
                mode,
            )) => {
                match dotcore_proto::dotcore::inference::v1::ChatToolChoiceMode::try_from(*mode)
                    .unwrap_or(
                        dotcore_proto::dotcore::inference::v1::ChatToolChoiceMode::Unspecified,
                    ) {
                    dotcore_proto::dotcore::inference::v1::ChatToolChoiceMode::None => {
                        serde_json::json!("none")
                    }
                    dotcore_proto::dotcore::inference::v1::ChatToolChoiceMode::Auto => {
                        serde_json::json!("auto")
                    }
                    dotcore_proto::dotcore::inference::v1::ChatToolChoiceMode::Required => {
                        serde_json::json!("required")
                    }
                    dotcore_proto::dotcore::inference::v1::ChatToolChoiceMode::Unspecified => {
                        Value::Null
                    }
                }
            }
            Some(
                dotcore_proto::dotcore::inference::v1::chat_tool_choice::Selection::FunctionName(
                    name,
                ),
            ) => serde_json::json!({"type":"function","function":{"name":name}}),
            None => Value::Null,
        };
        if !choice.is_null() {
            object.insert("tool_choice".to_owned(), choice);
        }
    }
    if let Some(parallel) = request.parallel_tool_calls {
        object.insert(
            "parallel_tool_calls".to_owned(),
            serde_json::json!(parallel),
        );
    }
    // Streamed requests always ask for usage, whatever the client asked; the client-facing
    // encoder drops it when unrequested. Non-streamed requests omit the option, which some
    // providers reject, because their response body already carries usage.
    if request.stream.unwrap_or(false) {
        object.insert(
            "stream_options".to_owned(),
            serde_json::json!({"include_usage":true}),
        );
    }
    if let Some(store) = request.store {
        object.insert("store".to_owned(), Value::Bool(store));
    }
    if let Some(controls) = &request.generation_controls {
        for (name, value) in [
            (
                "temperature",
                controls.temperature.map(|value| serde_json::json!(value)),
            ),
            (
                "top_p",
                controls.top_p.map(|value| serde_json::json!(value)),
            ),
            (
                "max_tokens",
                controls.max_tokens.map(|value| serde_json::json!(value)),
            ),
            (
                "max_completion_tokens",
                controls
                    .max_completion_tokens
                    .map(|value| serde_json::json!(value)),
            ),
            (
                "reasoning_effort",
                controls
                    .reasoning_effort
                    .as_ref()
                    .map(|value| serde_json::json!(value)),
            ),
            (
                "presence_penalty",
                controls
                    .presence_penalty
                    .map(|value| serde_json::json!(value)),
            ),
            (
                "frequency_penalty",
                controls
                    .frequency_penalty
                    .map(|value| serde_json::json!(value)),
            ),
        ] {
            if let Some(value) = value {
                object.insert(name.to_owned(), value);
            }
        }
        if let Some(stop) = &controls.stop_sequences {
            object.insert("stop".to_owned(), serde_json::json!(stop.values));
        }
    }
    let Value::Object(object) = value else {
        unreachable!("the request body is built as an object");
    };
    Ok(object)
}

fn raw_json(value: &Value) -> Box<RawValue> {
    serde_json::value::to_raw_value(value).expect("JSON values serialize")
}

fn encode_tool(
    tool: &dotcore_proto::dotcore::inference::v1::FunctionToolDefinition,
) -> Result<Value, InvalidRequest> {
    let parameters = serde_json::from_str::<Value>(&tool.parameters_json_schema)
        .map_err(|_| InvalidRequest::invalid_field(Some("tools")))?;
    let mut function = Map::from_iter([
        ("name".to_owned(), serde_json::json!(tool.name)),
        ("parameters".to_owned(), parameters),
    ]);
    if let Some(description) = &tool.description {
        function.insert("description".to_owned(), serde_json::json!(description));
    }
    if let Some(strict) = tool.strict {
        function.insert("strict".to_owned(), serde_json::json!(strict));
    }
    Ok(serde_json::json!({"type":"function","function":function}))
}

fn encode_message(message: &ChatMessage, history_reasoning: HistoryReasoning) -> Value {
    let role = match ChatMessageRole::try_from(message.role).unwrap_or(ChatMessageRole::Unspecified)
    {
        ChatMessageRole::System => "system",
        ChatMessageRole::Developer => "developer",
        ChatMessageRole::User => "user",
        ChatMessageRole::Assistant => "assistant",
        ChatMessageRole::Tool => "tool",
        ChatMessageRole::Unspecified => "unknown",
    };
    let mut value = serde_json::json!({"role":role});
    let object = value.as_object_mut().expect("JSON object");
    match &message.content {
        Some(chat_message::Content::TextContent(content)) => {
            object.insert("content".to_owned(), serde_json::json!(content));
        }
        Some(chat_message::Content::NullContent(_)) => {
            object.insert("content".to_owned(), Value::Null);
        }
        Some(chat_message::Content::ToolResult(result)) => {
            object.insert("content".to_owned(), serde_json::json!(result.content));
            object.insert(
                "tool_call_id".to_owned(),
                serde_json::json!(result.tool_call_id),
            );
        }
        None => {}
    }
    if !message.tool_calls.is_empty() {
        object.insert(
            "tool_calls".to_owned(),
            Value::Array(
                message
                    .tool_calls
                    .iter()
                    .map(|call| serde_json::json!({"id":call.tool_call_id,"type":"function","function":{"name":call.function_name,"arguments":call.arguments_json}}))
                    .collect(),
            ),
        );
    }
    if let Some(name) = &message.name {
        object.insert("name".to_owned(), serde_json::json!(name));
    }
    if let Some(reasoning) = &message.reasoning {
        object.insert("reasoning".to_owned(), serde_json::json!(reasoning));
        if matches!(history_reasoning, HistoryReasoning::WithReasoningContent) {
            object.insert("reasoning_content".to_owned(), serde_json::json!(reasoning));
        }
    }
    if !message.content_parts.is_empty() {
        object.insert(
            "content".to_owned(),
            Value::Array(
                message
                    .content_parts
                    .iter()
                    .map(encode_content_part)
                    .collect(),
            ),
        );
    }
    value
}

fn encode_content_part(part: &dotcore_proto::dotcore::inference::v1::ChatContentPart) -> Value {
    match &part.content {
        Some(dotcore_proto::dotcore::inference::v1::chat_content_part::Content::TextContent(
            text,
        )) => serde_json::json!({"type":"text","text":text}),
        Some(dotcore_proto::dotcore::inference::v1::chat_content_part::Content::ImageContent(
            image,
        )) => {
            let media_type =
                match dotcore_proto::dotcore::inference::v1::ChatImageMediaType::try_from(
                    image.media_type,
                )
                .unwrap_or(dotcore_proto::dotcore::inference::v1::ChatImageMediaType::Unspecified)
                {
                    dotcore_proto::dotcore::inference::v1::ChatImageMediaType::Jpeg => "image/jpeg",
                    dotcore_proto::dotcore::inference::v1::ChatImageMediaType::Png => "image/png",
                    dotcore_proto::dotcore::inference::v1::ChatImageMediaType::Unspecified => {
                        "application/octet-stream"
                    }
                };
            serde_json::json!({"type":"image_url","image_url":{"url":format!(
                "data:{media_type};base64,{}",
                base64::Engine::encode(
                    &base64::engine::general_purpose::STANDARD,
                    &image.encoded_data,
                ),
            ),"detail":"auto"}})
        }
        None => Value::Null,
    }
}

pub(crate) fn decode_completion(body: &[u8]) -> Result<ChatCompletionResult, CodecError> {
    let value: Value = serde_json::from_slice(body)?;
    completion_from_value(&value)
}

/// Decodes one complete upstream SSE JSON payload.
pub(crate) fn decode_sse_frame(data: &str) -> Result<Vec<ChatStreamEvent>, CodecError> {
    let value = serde_json::from_str(data)?;
    stream_events_from_value(&value)
}

pub(crate) fn stream_event_choice_index(event: &ChatStreamEvent) -> Option<u32> {
    match event.event.as_ref()? {
        Event::TextDelta(event) => Some(event.choice_index),
        Event::ToolCallDelta(event) => Some(event.choice_index),
        Event::ReasoningDelta(event) => Some(event.choice_index),
        Event::Finished(event) => Some(event.choice_index),
        Event::Started(_) | Event::Usage(_) | Event::Completed(_) => None,
    }
}

/// A malformed or otherwise unusable OpenAI-compatible response payload.
#[derive(Debug, thiserror::Error)]
pub(crate) enum CodecError {
    #[error("OpenAI-compatible JSON decode error: {0}")]
    Json(#[from] serde_json::Error),

    #[error("OpenAI-compatible UTF-8 decode error: {0}")]
    Utf8(#[from] std::str::Utf8Error),

    #[error("OpenAI-compatible integer conversion error: {0}")]
    Integer(#[from] std::num::TryFromIntError),

    #[error("Invalid OpenAI-compatible response: {0}")]
    InvalidResponse(#[source] anyhow::Error),
}

macro_rules! invalid_response {
    ($format:literal $(, $argument:expr)* $(,)?) => {
        CodecError::InvalidResponse(anyhow!($format $(, $argument)*))
    };
    ($message:expr $(,)?) => {
        CodecError::InvalidResponse(anyhow!($message))
    };
}

/// Validates the order and completeness of one decoded OpenAI-compatible stream.
///
/// Every observed choice must finish, every tool call must have received an id, a function
/// name, and an argument string before its choice finishes, and real usage may only follow
/// the last finished choice. Individual deltas may omit any of those fields.
///
/// Usage is always requested from engines, so a stream whose client did not ask for it drops
/// misplaced usage instead of failing.
#[derive(Debug)]
pub(crate) struct StreamValidator {
    started: bool,
    choices: BTreeMap<u32, StreamChoice>,
    usage: bool,
    strict_usage: bool,
}

impl Default for StreamValidator {
    fn default() -> Self {
        Self::new(true)
    }
}

/// Whether a validated stream event reaches the caller.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum StreamAction {
    Forward,

    Filter,
}

impl StreamValidator {
    /// Creates a validator that rejects misplaced usage when `strict_usage` is set and drops it
    /// otherwise.
    pub(crate) fn new(strict_usage: bool) -> Self {
        Self {
            started: false,
            choices: BTreeMap::new(),
            usage: false,
            strict_usage,
        }
    }

    /// Validates one event in order; duplicate Started events are filtered.
    pub(crate) fn apply(&mut self, event: &ChatStreamEvent) -> Result<StreamAction, CodecError> {
        if self.usage && !matches!(event.event, Some(Event::Started(_))) {
            if !self.strict_usage {
                return Ok(StreamAction::Filter);
            }
            return Err(invalid_response!(
                "OpenAI-compatible stream emitted an event after terminal usage"
            ));
        }
        match &event.event {
            Some(Event::Started(_)) => {
                if self.started {
                    return Ok(StreamAction::Filter);
                }
                self.started = true;
                return Ok(StreamAction::Forward);
            }
            _ if !self.started => {
                return Err(invalid_response!(
                    "OpenAI-compatible stream emitted an event before its Started event"
                ));
            }
            Some(Event::Usage(_)) => {
                if self.choices.is_empty() || self.choices.values().any(|choice| !choice.finished) {
                    if !self.strict_usage {
                        return Ok(StreamAction::Filter);
                    }
                    return Err(invalid_response!(
                        "OpenAI-compatible stream emitted usage before its terminal choice"
                    ));
                }
                self.usage = true;
                return Ok(StreamAction::Forward);
            }
            _ => {}
        }
        if let Some(choice_index) = stream_event_choice_index(event) {
            let choice = self.choices.entry(choice_index).or_default();
            if choice.finished {
                return Err(invalid_response!(
                    "OpenAI-compatible stream emitted an event for choice {choice_index} after its Finished event"
                ));
            }
            match &event.event {
                Some(Event::ToolCallDelta(delta)) => {
                    let call = choice.tool_calls.entry(delta.tool_call_index).or_default();
                    call.id |= delta.tool_call_id.as_ref().is_some_and(|id| !id.is_empty());
                    call.name |= delta
                        .function_name
                        .as_ref()
                        .is_some_and(|name| !name.is_empty());
                    call.arguments |= delta.arguments_json_fragment.is_some();
                }
                Some(Event::Finished(_)) => {
                    if choice
                        .tool_calls
                        .values()
                        .any(|call| !(call.id && call.name && call.arguments))
                    {
                        return Err(invalid_response!(
                            "OpenAI-compatible stream finished choice {choice_index} with an incomplete tool call"
                        ));
                    }
                    choice.finished = true;
                }
                _ => {}
            }
        }
        Ok(StreamAction::Forward)
    }

    /// Checks the terminal state once the stream has ended.
    pub(crate) fn finish(&self, done: bool, include_usage: bool) -> Result<(), CodecError> {
        if !self.started || !done || self.choices.is_empty() {
            return Err(invalid_response!(
                "OpenAI-compatible stream ended without a valid terminal event"
            ));
        }
        if self.choices.values().any(|choice| !choice.finished) {
            return Err(invalid_response!(
                "OpenAI-compatible stream ended with an unfinished choice"
            ));
        }
        if include_usage && !self.usage {
            return Err(invalid_response!(
                "OpenAI-compatible stream ended without requested terminal usage"
            ));
        }
        Ok(())
    }
}

#[derive(Debug, Default)]
struct StreamChoice {
    finished: bool,
    tool_calls: BTreeMap<u32, StreamToolCall>,
}

#[derive(Debug, Default)]
struct StreamToolCall {
    id: bool,
    name: bool,
    arguments: bool,
}

fn completion_from_value(value: &Value) -> Result<ChatCompletionResult, CodecError> {
    let object = upstream_object(value, "completion")?;
    let choices = upstream_array(
        upstream_required(object, "choices", "completion")?,
        "choices",
    )?;
    if choices.is_empty() {
        return Err(invalid_response!(
            "OpenAI-compatible completion has no choices"
        ));
    }
    let choices = choices
        .iter()
        .map(choice_from_value)
        .collect::<Result<Vec<_>, _>>()?;
    Ok(ChatCompletionResult {
        completion_id: upstream_string(
            upstream_required(object, "id", "completion")?,
            "completion id",
        )?
        .to_owned(),
        model_id: upstream_string(
            upstream_required(object, "model", "completion")?,
            "completion model",
        )?
        .to_owned(),
        created_at_unix_seconds: upstream_u64(
            upstream_required(object, "created", "completion")?,
            "completion created",
        )?,
        choices,
        usage: present(object, "usage").map(usage_from_value).transpose()?,
    })
}
fn choice_from_value(
    value: &Value,
) -> Result<dotcore_proto::dotcore::inference::v1::ChatCompletionChoice, CodecError> {
    let object = upstream_object(value, "choice")?;
    let message = message_from_value(upstream_required(object, "message", "choice")?)?;
    let finish_reason = finish_from_value(upstream_required(object, "finish_reason", "choice")?)?;
    let has_tool_calls = !message.tool_calls.is_empty();
    if has_tool_calls && message.role != ChatMessageRole::Assistant as i32 {
        return Err(invalid_response!(
            "OpenAI-compatible completion has tool calls on a non-assistant message",
        ));
    }
    match finish_reason {
        ChatFinishReason::ToolCalls if !has_tool_calls => {
            return Err(invalid_response!(
                "OpenAI-compatible completion has a tool_calls finish reason without tool calls",
            ));
        }
        _ => {}
    }
    Ok(
        dotcore_proto::dotcore::inference::v1::ChatCompletionChoice {
            index: upstream_u32(
                upstream_required(object, "index", "choice")?,
                "choice index",
            )?,
            message: Some(message),
            finish_reason: finish_reason as i32,
        },
    )
}
fn message_from_value(value: &Value) -> Result<ChatMessage, CodecError> {
    let object = upstream_object(value, "message")?;
    let role = role_from_value(upstream_required(object, "role", "message")?)?;
    let has_tool_calls = present(object, "tool_calls").is_some();
    let null_content = || {
        Some(chat_message::Content::NullContent(
            NullValue::NullValue as i32,
        ))
    };
    let content = match object.get("content") {
        Some(Value::String(value)) => Some(chat_message::Content::TextContent(value.clone())),
        Some(Value::Null) => null_content(),
        // A tool-call message may omit its content, which then means null.
        None if has_tool_calls => null_content(),
        None => {
            return Err(invalid_response!(
                "OpenAI-compatible message has no content"
            ));
        }
        Some(_) => {
            return Err(invalid_response!(
                "OpenAI-compatible message content is invalid"
            ));
        }
    };
    let tool_calls = present(object, "tool_calls")
        .map(|calls| upstream_array(calls, "message tool_calls"))
        .transpose()?
        .map(|calls| {
            calls
                .iter()
                .map(tool_call_from_value)
                .collect::<Result<Vec<_>, _>>()
        })
        .transpose()?
        .unwrap_or_default();
    let content = if role == ChatMessageRole::Tool {
        match content.as_ref() {
            Some(chat_message::Content::TextContent(content)) => {
                Some(chat_message::Content::ToolResult(FunctionToolResult {
                    tool_call_id: upstream_string(
                        upstream_required(object, "tool_call_id", "tool message")?,
                        "tool_call_id",
                    )?
                    .to_owned(),
                    content: content.clone(),
                }))
            }
            _ => {
                return Err(invalid_response!(
                    "OpenAI-compatible tool message content is invalid"
                ));
            }
        }
    } else {
        content
    };
    Ok(ChatMessage {
        role: role as i32,
        content,
        tool_calls,
        name: present(object, "name")
            .map(|value| upstream_string(value, "message name").map(str::to_owned))
            .transpose()?,
        content_parts: Vec::new(),
        reasoning: reasoning_from_object(object, "message")?,
    })
}

/// Field names that carry one message's reasoning, in order of preference.
pub(crate) const REASONING_ALIASES: [&str; 3] =
    ["reasoning", "reasoning_content", "reasoning_text"];

/// Reads a message's reasoning from its accepted aliases; null means absent and equal values
/// collapse to one, while unequal values are a conflict rather than a silent choice.
pub(crate) fn reasoning_alias(
    object: &Map<String, Value>,
) -> Result<Option<String>, ReasoningAliasError> {
    let mut reasoning = None;
    for field in REASONING_ALIASES {
        let candidate = match object.get(field) {
            None | Some(Value::Null) => continue,
            Some(Value::String(value)) => value,
            Some(_) => return Err(ReasoningAliasError::NotText),
        };
        match &reasoning {
            Some(current) if current != candidate => return Err(ReasoningAliasError::Conflict),
            Some(_) => {}
            None => reasoning = Some(candidate.clone()),
        }
    }
    Ok(reasoning)
}

/// Invalid reasoning aliases on one message.
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
pub(crate) enum ReasoningAliasError {
    #[error("reasoning is not a string or null")]
    NotText,

    #[error("reasoning aliases carry different values")]
    Conflict,
}

fn reasoning_from_object(
    object: &Map<String, Value>,
    context: &str,
) -> Result<Option<String>, CodecError> {
    reasoning_alias(object)
        .map_err(|error| invalid_response!("OpenAI-compatible {context} {error}"))
}

fn tool_call_from_value(value: &Value) -> Result<FunctionToolCall, CodecError> {
    let object = upstream_object(value, "tool call")?;
    if upstream_string(
        upstream_required(object, "type", "tool call")?,
        "tool call type",
    )? != "function"
    {
        return Err(invalid_response!(
            "OpenAI-compatible tool call type is invalid"
        ));
    }
    let function = upstream_object(
        upstream_required(object, "function", "tool call")?,
        "tool call function",
    )?;
    Ok(FunctionToolCall {
        tool_call_id: upstream_string(
            upstream_required(object, "id", "tool call")?,
            "tool call id",
        )?
        .to_owned(),
        function_name: upstream_string(
            upstream_required(function, "name", "tool call function")?,
            "tool call function name",
        )?
        .to_owned(),
        arguments_json: upstream_string(
            upstream_required(function, "arguments", "tool call function")?,
            "tool call function arguments",
        )?
        .to_owned(),
    })
}
fn stream_events_from_value(value: &Value) -> Result<Vec<ChatStreamEvent>, CodecError> {
    let object = upstream_object(value, "stream event")?;
    let mut events = Vec::new();
    if object.contains_key("id") || object.contains_key("model") || object.contains_key("created") {
        events.push(ChatStreamEvent {
            event: Some(Event::Started(
                dotcore_proto::dotcore::inference::v1::ChatStreamStarted {
                    completion_id: upstream_string(
                        upstream_required(object, "id", "stream event")?,
                        "stream id",
                    )?
                    .to_owned(),
                    model_id: upstream_string(
                        upstream_required(object, "model", "stream event")?,
                        "stream model",
                    )?
                    .to_owned(),
                    created_at_unix_seconds: upstream_u64(
                        upstream_required(object, "created", "stream event")?,
                        "stream created",
                    )?,
                },
            )),
        });
    }
    let choices = upstream_array(
        upstream_required(object, "choices", "stream event")?,
        "stream choices",
    )?;
    for choice in choices {
        let choice_object = upstream_object(choice, "stream choice")?;
        let index = upstream_u32(
            upstream_required(choice_object, "index", "stream choice")?,
            "stream choice index",
        )?;
        let finish_reason = present(choice_object, "finish_reason");
        let delta = upstream_object(
            upstream_required(choice_object, "delta", "stream choice")?,
            "stream delta",
        )?;
        let tool_calls = present(delta, "tool_calls");
        {
            let role = present(delta, "role")
                .map(|value| role_from_value(value).map(|role| role as i32))
                .transpose()?;
            let content = match delta.get("content") {
                Some(Value::String(content)) => {
                    Some(chat_text_delta::Content::TextContent(content.to_owned()))
                }
                Some(Value::Null) => Some(chat_text_delta::Content::NullContent(
                    NullValue::NullValue as i32,
                )),
                Some(_) => {
                    return Err(invalid_response!(
                        "OpenAI-compatible stream content is invalid"
                    ));
                }
                None => None,
            };
            let reasoning_fields_present = REASONING_ALIASES
                .iter()
                .any(|field| delta.contains_key(*field));
            let reasoning = reasoning_from_object(delta, "stream delta")?;
            if role.is_some()
                || content.is_some()
                || (tool_calls.is_none() && !reasoning_fields_present && finish_reason.is_none())
            {
                events.push(ChatStreamEvent {
                    event: Some(Event::TextDelta(
                        dotcore_proto::dotcore::inference::v1::ChatTextDelta {
                            choice_index: index,
                            role,
                            content,
                        },
                    )),
                });
            }
            if let Some(reasoning) = reasoning {
                events.push(ChatStreamEvent {
                    event: Some(Event::ReasoningDelta(ChatReasoningDelta {
                        choice_index: index,
                        reasoning,
                    })),
                });
            }
        }
        if let Some(tool_calls) = tool_calls {
            for tool_call in upstream_array(tool_calls, "stream tool_calls")? {
                events.push(tool_call_delta_from_value(index, tool_call)?);
            }
        }
        if let Some(finish_reason) = finish_reason {
            events.push(ChatStreamEvent {
                event: Some(Event::Finished(
                    dotcore_proto::dotcore::inference::v1::ChatStreamFinished {
                        choice_index: index,
                        finish_reason: finish_from_value(finish_reason)? as i32,
                    },
                )),
            });
        }
    }
    if let Some(usage) = present(object, "usage") {
        events.push(ChatStreamEvent {
            event: Some(Event::Usage(usage_from_value(usage)?)),
        });
    }
    Ok(events)
}

fn tool_call_delta_from_value(
    choice_index: u32,
    value: &Value,
) -> Result<ChatStreamEvent, CodecError> {
    let call = upstream_object(value, "stream tool call")?;
    if let Some(kind) = present(call, "type")
        && upstream_string(kind, "stream tool call type")? != "function"
    {
        return Err(invalid_response!(
            "OpenAI-compatible stream tool call type is invalid",
        ));
    }
    let function = present(call, "function")
        .map(|value| upstream_object(value, "stream tool call function"))
        .transpose()?;
    Ok(ChatStreamEvent {
        event: Some(Event::ToolCallDelta(
            dotcore_proto::dotcore::inference::v1::ChatToolCallDelta {
                choice_index,
                tool_call_index: upstream_u32(
                    upstream_required(call, "index", "stream tool call")?,
                    "stream tool call index",
                )?,
                tool_call_id: present(call, "id")
                    .map(|value| upstream_string(value, "stream tool call id").map(str::to_owned))
                    .transpose()?,
                function_name: function
                    .and_then(|function| present(function, "name"))
                    .map(|value| {
                        upstream_string(value, "stream tool function name").map(str::to_owned)
                    })
                    .transpose()?,
                arguments_json_fragment: function
                    .and_then(|function| present(function, "arguments"))
                    .map(|value| {
                        upstream_string(value, "stream tool function arguments").map(str::to_owned)
                    })
                    .transpose()?,
            },
        )),
    })
}

fn usage_from_value(value: &Value) -> Result<TokenUsage, CodecError> {
    let object = upstream_object(value, "usage")?;
    let detail = |details: &str, field: &str| {
        present(object, details)
            .map(|value| upstream_object(value, details))
            .transpose()?
            .and_then(|details| present(details, field))
            .map(|value| upstream_u64(value, field))
            .transpose()
    };
    Ok(TokenUsage {
        prompt_tokens: upstream_u64(
            upstream_required(object, "prompt_tokens", "usage")?,
            "usage prompt_tokens",
        )?,
        completion_tokens: upstream_u64(
            upstream_required(object, "completion_tokens", "usage")?,
            "usage completion_tokens",
        )?,
        total_tokens: upstream_u64(
            upstream_required(object, "total_tokens", "usage")?,
            "usage total_tokens",
        )?,
        cached_prompt_tokens: detail("prompt_tokens_details", "cached_tokens")?,
        reasoning_tokens: detail("completion_tokens_details", "reasoning_tokens")?,
    })
}
fn finish_from_value(value: &Value) -> Result<ChatFinishReason, CodecError> {
    match upstream_string(value, "finish_reason")? {
        "stop" => Ok(ChatFinishReason::Stop),
        "length" => Ok(ChatFinishReason::Length),
        "tool_calls" | "function_call" => Ok(ChatFinishReason::ToolCalls),
        "content_filter" => Ok(ChatFinishReason::ContentFilter),
        _ => Err(invalid_response!(
            "OpenAI-compatible finish_reason is invalid"
        )),
    }
}

fn role_from_value(value: &Value) -> Result<ChatMessageRole, CodecError> {
    match upstream_string(value, "message role")? {
        "system" => Ok(ChatMessageRole::System),
        "developer" => Ok(ChatMessageRole::Developer),
        "user" => Ok(ChatMessageRole::User),
        "assistant" => Ok(ChatMessageRole::Assistant),
        "tool" => Ok(ChatMessageRole::Tool),
        _ => Err(invalid_response!(
            "OpenAI-compatible message role is invalid"
        )),
    }
}

/// Returns a field unless it is absent or an explicit null placeholder.
fn present<'a>(object: &'a Map<String, Value>, field: &str) -> Option<&'a Value> {
    object.get(field).filter(|value| !value.is_null())
}

fn upstream_required<'a>(
    object: &'a Map<String, Value>,
    field: &str,
    context: &str,
) -> Result<&'a Value, CodecError> {
    object
        .get(field)
        .ok_or_else(|| invalid_response!("OpenAI-compatible {context} has no {field}"))
}

fn upstream_object<'a>(value: &'a Value, name: &str) -> Result<&'a Map<String, Value>, CodecError> {
    value
        .as_object()
        .ok_or_else(|| invalid_response!("OpenAI-compatible {name} is not an object"))
}

fn upstream_array<'a>(value: &'a Value, name: &str) -> Result<&'a Vec<Value>, CodecError> {
    value
        .as_array()
        .ok_or_else(|| invalid_response!("OpenAI-compatible {name} is not an array"))
}

fn upstream_string<'a>(value: &'a Value, name: &str) -> Result<&'a str, CodecError> {
    value
        .as_str()
        .ok_or_else(|| invalid_response!("OpenAI-compatible {name} is not a string"))
}

fn upstream_u64(value: &Value, name: &str) -> Result<u64, CodecError> {
    value
        .as_u64()
        .ok_or_else(|| invalid_response!("OpenAI-compatible {name} is not an unsigned integer"))
}

fn upstream_u32(value: &Value, name: &str) -> Result<u32, CodecError> {
    upstream_u64(value, name).and_then(|value| u32::try_from(value).map_err(CodecError::from))
}

#[cfg(test)]
pub(crate) fn decode_sse(body: &[u8]) -> Result<Vec<ChatStreamEvent>, CodecError> {
    let text = std::str::from_utf8(body)?.replace("\r\n", "\n");
    let mut events = Vec::new();
    let mut validator = StreamValidator::default();
    let mut done = false;
    for frame in text.split("\n\n") {
        let Some(data) = frame.lines().find_map(|line| {
            line.strip_prefix("data:")
                .map(str::trim_start)
                .filter(|data| !data.is_empty())
        }) else {
            continue;
        };
        if data == "[DONE]" {
            done = true;
            break;
        }
        for event in stream_events_from_value(&serde_json::from_str(data)?)? {
            if validator.apply(&event)? == StreamAction::Forward {
                events.push(event);
            }
        }
    }
    validator.finish(done, false)?;
    events.push(ChatStreamEvent {
        event: Some(Event::Completed(Default::default())),
    });
    Ok(events)
}

#[cfg(test)]
mod tests {
    use std::error::Error as _;

    use dotcore_proto::dotcore::inference::v1::{
        ChatStreamFinished, ChatStreamStarted, ChatTextDelta, ChatToolCallDelta,
    };
    use serde_json::json;

    use super::*;
    use crate::router::{openai::OpenAiRequest, test_fixtures};

    #[test]
    fn shared_chat_fixture_preserves_openai_compatible_codec_fidelity() {
        let request_body =
            serde_json::to_vec(&test_fixtures::chat_request()).expect("fixture JSON");
        let OpenAiRequest::Chat(request) =
            crate::router::openai::decode("POST", "/v1/chat/completions", &request_body)
                .expect("Chat request")
        else {
            panic!("fixture must decode as Chat")
        };
        assert_eq!(encoded(&request), test_fixtures::llama_request_oracle());
        let encoded_body = encode_request(&request).expect("OpenAI-compatible request JSON");
        let OpenAiRequest::Chat(roundtrip_request) =
            crate::router::openai::decode("POST", "/v1/chat/completions", &encoded_body)
                .expect("encoded request is an OpenAI API Chat request")
        else {
            panic!("encoded request must decode as Chat")
        };
        // A non-streamed body never carries stream options, so the client's intent is not echoed.
        assert_eq!(
            roundtrip_request,
            ChatRequest {
                include_stream_usage: None,
                ..request
            }
        );

        let events = decode_sse(&test_fixtures::fragmented_stream_body()).expect("fragmented SSE");
        assert_eq!(
            event_kinds(&events),
            [
                "started",
                "text",
                "text",
                "text",
                "tool",
                "tool",
                "tool",
                "finished",
                "usage",
                "completed",
            ]
        );
        assert!(matches!(
            events[2].event.as_ref(),
            Some(Event::TextDelta(delta)) if delta.content.is_none()
        ));
        assert!(matches!(
            events[3].event.as_ref(),
            Some(Event::TextDelta(delta))
                if matches!(delta.content.as_ref(), Some(chat_text_delta::Content::NullContent(_)))
        ));
        let tool_fragments = events
            .iter()
            .filter_map(|event| match &event.event {
                Some(Event::ToolCallDelta(delta)) => delta.arguments_json_fragment.clone(),
                _ => None,
            })
            .collect::<Vec<_>>();
        assert_eq!(tool_fragments, ["{\"pa", "th", "}"]);
        assert!(events.iter().any(|event| matches!(
            event.event,
            Some(Event::Usage(ref usage))
                if usage.prompt_tokens == 21
                    && usage.completion_tokens == 7
                    && usage.total_tokens == 28
        )));

        let error =
            decode_sse(test_fixtures::invalid_sse().as_bytes()).expect_err("invalid SSE event");
        assert!(matches!(error, CodecError::Json(_)));
        assert!(decode_sse(b"data: []\n\n").is_err());
        assert!(decode_sse(&[0xff]).is_err());
    }

    #[test]
    fn reasoning_round_trips_through_complete_and_assistant_history_payloads() {
        for field in ["reasoning", "reasoning_content"] {
            let mut message = json!({"role": "assistant", "content": "answer"});
            message
                .as_object_mut()
                .expect("message object")
                .insert(field.to_owned(), json!("analysis"));
            let body = serde_json::to_vec(&json!({
                "id": "completion",
                "model": "upstream-model",
                "created": 1,
                "choices": [{
                    "index": 0,
                    "message": message,
                    "finish_reason": "stop"
                }]
            }))
            .expect("completion JSON");
            let completion = decode_completion(&body).expect("reasoning completion");
            assert_eq!(
                completion.choices[0]
                    .message
                    .as_ref()
                    .expect("assistant message")
                    .reasoning
                    .as_deref(),
                Some("analysis")
            );
        }

        let request = dotcore_proto::dotcore::inference::v1::ChatRequest {
            model_id: "route".to_owned(),
            messages: vec![ChatMessage {
                role: ChatMessageRole::Assistant as i32,
                content: Some(chat_message::Content::TextContent("answer".to_owned())),
                reasoning: Some("analysis".to_owned()),
                ..Default::default()
            }],
            ..Default::default()
        };
        assert_eq!(
            encoded(&request).pointer("/messages/0/reasoning"),
            Some(&json!("analysis"))
        );
    }

    #[test]
    fn named_tool_call_preserves_stop_and_length_finish_reasons() {
        for (finish_reason, expected) in [
            ("stop", ChatFinishReason::Stop),
            ("length", ChatFinishReason::Length),
        ] {
            let body = serde_json::to_vec(&json!({
                "id": "completion",
                "model": "upstream-model",
                "created": 1,
                "choices": [{
                    "index": 0,
                    "message": {
                        "role": "assistant",
                        "content": null,
                        "tool_calls": [{
                            "id": "call_ready",
                            "type": "function",
                            "function": {"name": "ready", "arguments": "{}"}
                        }]
                    },
                    "finish_reason": finish_reason
                }]
            }))
            .expect("completion JSON");

            let completion = decode_completion(&body).expect("named tool-call completion");
            let choice = &completion.choices[0];
            assert_eq!(choice.finish_reason, expected as i32);
            let calls = &choice
                .message
                .as_ref()
                .expect("assistant message")
                .tool_calls;
            assert_eq!(calls.len(), 1);
            assert_eq!(calls[0].tool_call_id, "call_ready");
            assert_eq!(calls[0].function_name, "ready");
            assert_eq!(calls[0].arguments_json, "{}");
        }
    }

    #[test]
    fn named_streaming_tool_call_preserves_stop_finish_reason() {
        let body = concat!(
            "data: {\"id\":\"completion\",\"model\":\"upstream-model\",\"created\":1,\"choices\":[{\"index\":0,\"delta\":{\"role\":\"assistant\",\"content\":null,\"tool_calls\":[{\"index\":0,\"id\":\"call_ready\",\"type\":\"function\",\"function\":{\"name\":\"ready\",\"arguments\":\"{}\"}}]},\"finish_reason\":null}]}\n\n",
            "data: {\"id\":\"completion\",\"model\":\"upstream-model\",\"created\":1,\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"stop\"}]}\n\n",
            "data: [DONE]\n\n"
        );

        let events = decode_sse(body.as_bytes()).expect("named streaming tool call");
        assert!(events.iter().any(|event| matches!(
            event.event,
            Some(Event::ToolCallDelta(ref delta))
                if delta.tool_call_id.as_deref() == Some("call_ready")
        )));
        assert!(events.iter().any(|event| matches!(
            event.event,
            Some(Event::Finished(ref finished))
                if finished.finish_reason == ChatFinishReason::Stop as i32
        )));
    }

    #[test]
    fn reasoning_preserves_absent_and_empty_values_and_rejects_conflicting_aliases() {
        let completion = |message: Value| {
            serde_json::to_vec(&json!({
                "id": "completion",
                "model": "upstream-model",
                "created": 1,
                "choices": [{"index": 0, "message": message, "finish_reason": "stop"}]
            }))
            .expect("completion JSON")
        };
        let absent = decode_completion(&completion(json!({"role":"assistant","content":"answer"})))
            .expect("completion without reasoning");
        assert_eq!(
            absent.choices[0]
                .message
                .as_ref()
                .expect("assistant message")
                .reasoning,
            None
        );
        let empty = decode_completion(&completion(
            json!({"role":"assistant","content":"answer","reasoning":""}),
        ))
        .expect("completion with empty reasoning");
        assert_eq!(
            empty.choices[0]
                .message
                .as_ref()
                .expect("assistant message")
                .reasoning
                .as_deref(),
            Some("")
        );
        assert!(
            decode_completion(&completion(json!({
                "role":"assistant",
                "content":"answer",
                "reasoning":"first",
                "reasoning_content":"second"
            })))
            .is_err()
        );
    }

    #[test]
    fn reasoning_only_stream_chunks_emit_one_reasoning_event() {
        let events = decode_sse_frame(
            &json!({
                "id": "completion",
                "model": "upstream-model",
                "created": 1,
                "choices": [{
                    "index": 3,
                    "delta": {"reasoning_content": "analysis"},
                    "finish_reason": null
                }]
            })
            .to_string(),
        )
        .expect("reasoning stream chunk");

        assert_eq!(event_kinds(&events), ["started", "reasoning"]);
        assert!(matches!(
            events[1].event.as_ref(),
            Some(Event::ReasoningDelta(delta))
                if delta.choice_index == 3 && delta.reasoning == "analysis"
        ));
    }

    #[test]
    fn malformed_completion_retains_json_source_chain() {
        let error = decode_completion(b"{").expect_err("invalid completion JSON");

        assert!(error.source().is_some());
    }

    #[test]
    fn invalid_response_retains_dynamic_diagnostic_chain() {
        let error = decode_completion(br#"{"choices":[]}"#).expect_err("empty choices");

        assert!(matches!(error, CodecError::InvalidResponse(_)));
        assert_eq!(
            error.to_string(),
            "Invalid OpenAI-compatible response: OpenAI-compatible completion has no choices"
        );
        assert!(error.source().is_some());
    }

    #[test]
    fn pi_controls_reach_the_engine_body_unchanged_under_the_selected_model() {
        let OpenAiRequest::Chat(request) = crate::router::openai::decode(
            "POST",
            "/v1/chat/completions",
            test_fixtures::PI_CHAT_REQUEST.as_bytes(),
        )
        .expect("Pi request") else {
            panic!("Pi fixture must decode as Chat")
        };

        let body = encode_request_for_model(&request, "served-model").expect("valid request");

        assert_eq!(
            serde_json::from_slice::<Value>(&body).expect("encoded request JSON"),
            json!({
                "model": "served-model",
                "messages": [
                    {"role": "developer", "content": "You are Pi."},
                    {"role": "user", "content": "Read the README."},
                    {
                        "role": "assistant",
                        "content": null,
                        "reasoning": "The file is needed.",
                        "tool_calls": [{"id": "call_1", "type": "function", "function": {"name": "read", "arguments": "{\"path\":\"README.md\"}"}}]
                    },
                    {"role": "tool", "tool_call_id": "call_1", "content": "# Readme"}
                ],
                "tools": [{"type": "function", "function": {"name": "read", "description": "Read a file.", "parameters": {"type": "object", "properties": {"path": {"type": "string"}}, "required": ["path"]}, "strict": false}}],
                "stream": true,
                "stream_options": {"include_usage": true},
                "store": false,
                "max_completion_tokens": 32000,
                "reasoning_effort": "none",
                "temperature": 1.0_f32,
                "top_p": 0.95_f32,
                "presence_penalty": 0.0,
                "frequency_penalty": 0.0,
                "top_k": 20,
                "repetition_penalty": 1.0,
                "min_p": 0,
                "seed": 18446744073709551615_u64,
                "chat_template_kwargs": {"enable_thinking": false, "preserve_thinking": true, "reasoning_effort": "low", "stops": [null, "</think>"]}
            })
        );
    }

    #[test]
    fn stream_options_follow_the_stream_flag_whatever_the_client_asked() {
        for client_intent in [None, Some(false), Some(true)] {
            for (stream, expected) in [
                (Some(true), Some(json!({"include_usage": true}))),
                (Some(false), None),
                (None, None),
            ] {
                let request = ChatRequest {
                    model_id: "route".to_owned(),
                    stream,
                    include_stream_usage: client_intent,
                    ..Default::default()
                };

                assert_eq!(
                    encoded(&request).get("stream_options").cloned(),
                    expected,
                    "stream {stream:?}, client intent {client_intent:?}"
                );
            }
        }
    }

    #[test]
    fn controls_merge_into_the_engine_body_as_their_original_text() {
        let request = ChatRequest {
            model_id: "route".to_owned(),
            request_extensions_json: Some(test_fixtures::EXACT_NUMBER_EXTENSIONS.to_owned()),
            ..Default::default()
        };

        let body = String::from_utf8(encode_request_for_model(&request, "served").expect("valid"))
            .expect("UTF-8 body");

        for control in [
            r#""above_2_53":9007199254740993"#,
            r#""above_u64":184467440737095516160"#,
            r#""float":0.1000000000000000055511151231257827"#,
            r#""i64_min":-9223372036854775808"#,
            r#""u64_max":18446744073709551615"#,
        ] {
            assert!(
                body.contains(control),
                "{control} must reach the engine verbatim"
            );
        }
    }

    #[test]
    fn zero_and_false_controls_are_sent_rather_than_treated_as_absent() {
        let request = ChatRequest {
            model_id: "route".to_owned(),
            parallel_tool_calls: Some(false),
            stream: Some(false),
            store: Some(false),
            generation_controls: Some(
                dotcore_proto::dotcore::inference::v1::ChatGenerationControls {
                    temperature: Some(0.0),
                    top_p: Some(0.0),
                    max_tokens: Some(0),
                    presence_penalty: Some(0.0),
                    frequency_penalty: Some(-0.5),
                    reasoning_effort: Some(String::new()),
                    ..Default::default()
                },
            ),
            ..Default::default()
        };

        assert_eq!(
            encoded(&request),
            json!({
                "model": "route",
                "messages": [],
                "parallel_tool_calls": false,
                "stream": false,
                "store": false,
                "temperature": 0.0,
                "top_p": 0.0,
                "max_tokens": 0,
                "presence_penalty": 0.0,
                "frequency_penalty": -0.5,
                "reasoning_effort": ""
            })
        );
    }

    #[test]
    fn invalid_generated_requests_are_refused_instead_of_coerced() {
        let tool = |schema: &str| ChatRequest {
            tools: vec![
                dotcore_proto::dotcore::inference::v1::FunctionToolDefinition {
                    name: "read".to_owned(),
                    parameters_json_schema: schema.to_owned(),
                    ..Default::default()
                },
            ],
            ..Default::default()
        };
        let oversized = ChatRequest {
            messages: vec![ChatMessage {
                role: ChatMessageRole::User as i32,
                content: Some(chat_message::Content::TextContent(
                    "x".repeat(PEER_INFERENCE_MESSAGE_LIMIT_BYTES),
                )),
                ..Default::default()
            }],
            ..Default::default()
        };
        for (request, expected) in [
            (
                tool("not json"),
                InvalidRequest::invalid_field(Some("tools")),
            ),
            (
                tool("\"text\""),
                InvalidRequest::invalid_field(Some("tools")),
            ),
            (
                ChatRequest {
                    request_extensions_json: Some(r#"{"model":"other"}"#.to_owned()),
                    ..Default::default()
                },
                InvalidRequest::invalid_field(Some("model")),
            ),
            (
                ChatRequest {
                    request_extensions_json: Some(r#"{"base_url":"http://evil"}"#.to_owned()),
                    ..Default::default()
                },
                InvalidRequest::invalid_field(None),
            ),
            (oversized, InvalidRequest::request_limit(None)),
        ] {
            assert_eq!(encode_request_for_model(&request, "served"), Err(expected));
        }
    }

    #[test]
    fn ninfer_text_stream_with_null_usage_placeholders_decodes() {
        let events =
            decode_sse(test_fixtures::NINFER_TEXT_STREAM.as_bytes()).expect("ninfer-shaped stream");

        assert_eq!(
            events,
            [
                Event::Started(ChatStreamStarted {
                    completion_id: "chatcmpl-1".to_owned(),
                    model_id: "qwen3.8-27b".to_owned(),
                    created_at_unix_seconds: 1759262400,
                }),
                Event::TextDelta(ChatTextDelta {
                    choice_index: 0,
                    role: Some(ChatMessageRole::Assistant as i32),
                    content: Some(chat_text_delta::Content::TextContent(String::new())),
                }),
                Event::TextDelta(ChatTextDelta {
                    choice_index: 0,
                    role: None,
                    content: Some(chat_text_delta::Content::TextContent("Hello".to_owned())),
                }),
                Event::Finished(ChatStreamFinished {
                    choice_index: 0,
                    finish_reason: ChatFinishReason::Stop as i32,
                }),
                Event::Usage(TokenUsage {
                    prompt_tokens: 12,
                    completion_tokens: 2,
                    total_tokens: 14,
                    cached_prompt_tokens: None,
                    reasoning_tokens: None,
                }),
                Event::Completed(Default::default()),
            ]
            .map(|event| ChatStreamEvent { event: Some(event) })
        );
    }

    #[test]
    fn reasoning_fragmented_tool_call_and_usage_details_decode_in_order() {
        let tool_delta = |id: Option<&str>, name: Option<&str>, arguments: &str| {
            Event::ToolCallDelta(ChatToolCallDelta {
                choice_index: 0,
                tool_call_index: 0,
                tool_call_id: id.map(str::to_owned),
                function_name: name.map(str::to_owned),
                arguments_json_fragment: Some(arguments.to_owned()),
            })
        };
        let reasoning = |text: &str| {
            Event::ReasoningDelta(ChatReasoningDelta {
                choice_index: 0,
                reasoning: text.to_owned(),
            })
        };

        let events = decode_sse(test_fixtures::REASONING_TOOL_STREAM.as_bytes())
            .expect("reasoning tool stream");

        assert_eq!(
            events,
            [
                Event::Started(ChatStreamStarted {
                    completion_id: "chatcmpl-2".to_owned(),
                    model_id: "engine-model".to_owned(),
                    created_at_unix_seconds: 7,
                }),
                Event::TextDelta(ChatTextDelta {
                    choice_index: 0,
                    role: Some(ChatMessageRole::Assistant as i32),
                    content: None,
                }),
                reasoning("Let me "),
                Event::TextDelta(ChatTextDelta {
                    choice_index: 0,
                    role: None,
                    content: Some(chat_text_delta::Content::NullContent(
                        NullValue::NullValue as i32,
                    )),
                }),
                reasoning("check."),
                tool_delta(Some("call_1"), Some("read"), ""),
                tool_delta(None, None, "{\"pa"),
                tool_delta(None, None, "th\":\"README.md\"}"),
                Event::Finished(ChatStreamFinished {
                    choice_index: 0,
                    finish_reason: ChatFinishReason::ToolCalls as i32,
                }),
                Event::Usage(TokenUsage {
                    prompt_tokens: 30,
                    completion_tokens: 12,
                    total_tokens: 42,
                    cached_prompt_tokens: Some(16),
                    reasoning_tokens: Some(5),
                }),
                Event::Completed(Default::default()),
            ]
            .map(|event| ChatStreamEvent { event: Some(event) })
        );
    }

    #[test]
    fn complete_usage_null_is_absent_and_details_survive() {
        let completion = |usage: Value| {
            decode_completion(
                &serde_json::to_vec(&json!({
                    "id": "completion",
                    "model": "engine",
                    "created": 1,
                    "choices": [{
                        "index": 0,
                        "message": {"role": "assistant", "tool_calls": [{"id": "call_1", "type": "function", "function": {"name": "read", "arguments": "{not json"}}]},
                        "finish_reason": "tool_calls"
                    }],
                    "usage": usage
                }))
                .expect("completion JSON"),
            )
        };

        let absent = completion(Value::Null).expect("null usage");
        assert_eq!(absent.usage, None);
        assert_eq!(
            absent.choices[0]
                .message
                .as_ref()
                .map(|message| &message.content),
            Some(&Some(chat_message::Content::NullContent(
                NullValue::NullValue as i32
            ))),
            "a tool-call message without content means null content"
        );
        assert_eq!(
            absent.choices[0]
                .message
                .as_ref()
                .expect("message")
                .tool_calls[0]
                .arguments_json,
            "{not json",
            "generated arguments stay caller-parsed"
        );
        assert_eq!(
            completion(json!({"prompt_tokens":3,"completion_tokens":2,"total_tokens":5,"prompt_tokens_details":{"cached_tokens":1,"audio_tokens":0},"completion_tokens_details":null}))
                .expect("usage details")
                .usage,
            Some(TokenUsage {
                prompt_tokens: 3,
                completion_tokens: 2,
                total_tokens: 5,
                cached_prompt_tokens: Some(1),
                reasoning_tokens: None,
            })
        );
        for usage in [
            json!("5"),
            json!({"prompt_tokens":3,"completion_tokens":2}),
            json!({"prompt_tokens":3,"completion_tokens":2,"total_tokens":5,"prompt_tokens_details":{"cached_tokens":"1"}}),
            json!({"prompt_tokens":3,"completion_tokens":2,"total_tokens":5,"completion_tokens_details":7}),
        ] {
            assert!(
                completion(usage.clone()).is_err(),
                "{usage} must be rejected"
            );
        }
    }

    #[test]
    fn stream_termination_rules_reject_incomplete_or_misordered_output() {
        let chunk = |choices: Value, usage: Value| {
            format!(
                "data: {}\n\n",
                json!({"id":"c","model":"m","created":1,"choices":choices,"usage":usage})
            )
        };
        let text =
            |index: u32| json!([{"index":index,"delta":{"content":"x"},"finish_reason":null}]);
        let stop = |index: u32| json!([{"index":index,"delta":{},"finish_reason":"stop"}]);
        let tool =
            |call: Value| json!([{"index":0,"delta":{"tool_calls":[call]},"finish_reason":null}]);
        let tool_stop = json!([{"index":0,"delta":{},"finish_reason":"tool_calls"}]);
        let usage = json!({"prompt_tokens":1,"completion_tokens":1,"total_tokens":2});
        let done = "data: [DONE]\n\n";
        let cases = [
            (
                "missing [DONE]",
                [chunk(text(0), Value::Null), chunk(stop(0), Value::Null)].concat(),
            ),
            (
                "unfinished second choice",
                [chunk(text(0), Value::Null), chunk(text(1), Value::Null), chunk(stop(0), Value::Null), done.to_owned()].concat(),
            ),
            (
                "output after its choice finished",
                [chunk(stop(0), Value::Null), chunk(text(0), Value::Null), done.to_owned()].concat(),
            ),
            (
                "output after terminal usage",
                [chunk(stop(0), Value::Null), chunk(json!([]), usage.clone()), chunk(text(1), Value::Null), done.to_owned()].concat(),
            ),
            (
                "usage before the terminal choice",
                [chunk(text(0), usage.clone()), chunk(stop(0), Value::Null), done.to_owned()].concat(),
            ),
            (
                "tool call without an id",
                [chunk(tool(json!({"index":0,"function":{"name":"read","arguments":"{}"}})), Value::Null), chunk(tool_stop.clone(), Value::Null), done.to_owned()].concat(),
            ),
            (
                "tool call without a name",
                [chunk(tool(json!({"index":0,"id":"call","function":{"arguments":"{}"}})), Value::Null), chunk(tool_stop.clone(), Value::Null), done.to_owned()].concat(),
            ),
            (
                "tool call without arguments",
                [chunk(tool(json!({"index":0,"id":"call","function":{"name":"read","arguments":null}})), Value::Null), chunk(tool_stop.clone(), Value::Null), done.to_owned()].concat(),
            ),
            (
                "conflicting reasoning aliases",
                [chunk(json!([{"index":0,"delta":{"reasoning":"a","reasoning_content":"b"},"finish_reason":null}]), Value::Null), chunk(stop(0), Value::Null), done.to_owned()].concat(),
            ),
            (
                "non-object usage",
                [chunk(stop(0), json!(2)), done.to_owned()].concat(),
            ),
            (
                "truncated frame",
                [chunk(stop(0), Value::Null), "data: {\"id\":\"c\"".to_owned()].concat(),
            ),
        ];
        for (case, body) in cases {
            assert!(
                decode_sse(body.as_bytes()).is_err(),
                "{case} must be rejected"
            );
        }

        let valid_partial = [
            chunk(
                tool(json!({"index":0,"id":"call","function":{"name":"read"}})),
                Value::Null,
            ),
            chunk(
                tool(json!({"index":0,"function":{"arguments":"{not json"}})),
                Value::Null,
            ),
            chunk(tool_stop, Value::Null),
            done.to_owned(),
        ]
        .concat();
        assert!(
            decode_sse(valid_partial.as_bytes()).is_ok(),
            "identity and arguments may arrive in separate fragments"
        );
    }

    #[test]
    fn requested_usage_must_arrive_once_after_every_choice_finishes() {
        let event = |event: Event| ChatStreamEvent { event: Some(event) };
        let mut validator = StreamValidator::default();
        for item in [
            event(Event::Started(ChatStreamStarted::default())),
            event(Event::Finished(ChatStreamFinished::default())),
        ] {
            assert_eq!(
                validator.apply(&item).expect("valid event"),
                StreamAction::Forward
            );
        }
        assert!(
            validator.finish(true, true).is_err(),
            "requested usage is missing"
        );
        assert!(validator.finish(true, false).is_ok());

        let usage = event(Event::Usage(TokenUsage::default()));
        assert_eq!(
            validator.apply(&usage).expect("terminal usage"),
            StreamAction::Forward
        );
        assert!(validator.finish(true, true).is_ok());
        assert!(validator.apply(&usage).is_err(), "usage arrives only once");
    }

    #[test]
    fn misplaced_usage_fails_a_requested_stream_and_is_dropped_otherwise() {
        let event = |event: Event| ChatStreamEvent { event: Some(event) };
        let started = event(Event::Started(ChatStreamStarted::default()));
        let usage = event(Event::Usage(TokenUsage::default()));
        let finished = event(Event::Finished(ChatStreamFinished::default()));
        let text = event(Event::TextDelta(Default::default()));

        // Usage before the terminal choice, and events after terminal usage.
        for strict in [true, false] {
            let mut early = StreamValidator::new(strict);
            early.apply(&started).expect("started");
            assert_eq!(
                early.apply(&usage).is_err(),
                strict,
                "early, strict {strict}"
            );

            let mut late = StreamValidator::new(strict);
            for item in [&started, &finished, &usage] {
                late.apply(item).expect("valid prefix");
            }
            assert_eq!(late.apply(&text).is_err(), strict, "late, strict {strict}");
            assert_eq!(
                late.apply(&usage).is_err(),
                strict,
                "twice, strict {strict}"
            );
            if !strict {
                assert!(matches!(late.apply(&text), Ok(StreamAction::Filter)));
                assert!(late.finish(true, false).is_ok());
            }
        }

        // A dropped early usage leaves the real terminal usage valid.
        let mut lenient = StreamValidator::new(false);
        lenient.apply(&started).expect("started");
        assert!(matches!(lenient.apply(&usage), Ok(StreamAction::Filter)));
        lenient.apply(&finished).expect("finished");
        assert!(matches!(lenient.apply(&usage), Ok(StreamAction::Forward)));
    }

    fn encoded(request: &ChatRequest) -> Value {
        serde_json::from_slice(&encode_request(request).expect("valid request"))
            .expect("encoded request JSON")
    }

    fn event_kinds(events: &[ChatStreamEvent]) -> Vec<&'static str> {
        events
            .iter()
            .map(|event| match event.event.as_ref() {
                Some(Event::Started(_)) => "started",
                Some(Event::TextDelta(_)) => "text",
                Some(Event::ToolCallDelta(_)) => "tool",
                Some(Event::ReasoningDelta(_)) => "reasoning",
                Some(Event::Finished(_)) => "finished",
                Some(Event::Usage(_)) => "usage",
                Some(Event::Completed(_)) => "completed",
                None => "absent",
            })
            .collect()
    }
}
