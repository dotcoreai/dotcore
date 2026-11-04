//! Stateless OpenAI Responses API codec over generated Chat messages.
//!
//! Requests map onto the canonical Chat request, including sampling, reasoning effort, and
//! engine-specific controls, while stored, stateful, and multi-choice features are refused.
//! Output always carries exactly one generated choice.

use std::collections::{BTreeMap, BTreeSet};

use dotcore_proto::dotcore::inference::v1::{
    ChatCompletionResult, ChatContentPart, ChatGenerationControls, ChatMessage, ChatMessageRole,
    ChatRequest, ChatStreamEvent, ChatToolChoice, ChatToolChoiceMode, FunctionToolCall,
    FunctionToolDefinition, FunctionToolResult, TokenUsage, chat_content_part, chat_message,
    chat_stream_event::Event, chat_text_delta, chat_tool_choice,
};
use dotcore_proto::google::protobuf::NullValue;
use serde_json::{Map, Value, json};

use super::{OpenAiResponse, chat};
use crate::{
    inference::InvalidRequest,
    router::{images, request_controls},
};

/// Decodes the supported stateless text and client-function Responses profile.
pub(crate) fn decode_request(body: &[u8]) -> Result<ChatRequest, OpenAiResponse> {
    let (object, mut extensions) = chat::split_request(
        body,
        &[
            "model",
            "input",
            "instructions",
            "tools",
            "tool_choice",
            "parallel_tool_calls",
            "stream",
            "store",
            "max_output_tokens",
            "temperature",
            "top_p",
            "presence_penalty",
            "frequency_penalty",
            "reasoning",
            "reasoning_effort",
            "prompt_cache_key",
            "n",
            "background",
        ],
    )?;
    let object = &object;
    if chat::decode_store(object)?.is_none() {
        return Err(unsupported("store must be false for this endpoint."));
    }
    match chat::optional(object, "n") {
        None => {}
        Some(value) if value.as_u64() == Some(1) => {}
        Some(value) if value.as_u64().is_some_and(|choices| choices > 1) => {
            return Err(chat::request_error(InvalidRequest::unsupported_feature(
                Some("n"),
            )));
        }
        Some(_) => {
            return Err(chat::request_error(InvalidRequest::invalid_field(Some(
                "n",
            ))));
        }
    }
    // Clients such as n8n always send `background`; false asks for the synchronous response this
    // endpoint already gives, so it is accepted and dropped.
    match chat::optional(object, "background") {
        None | Some(Value::Bool(false)) => {}
        Some(Value::Bool(true)) => {
            return Err(chat::request_error(InvalidRequest::unsupported_feature(
                Some("background"),
            )));
        }
        Some(_) => {
            return Err(chat::request_error(InvalidRequest::invalid_field(Some(
                "background",
            ))));
        }
    }
    if let Some(prompt_cache_key) = chat::optional(object, "prompt_cache_key") {
        text(prompt_cache_key, "prompt_cache_key")?;
        extensions.insert(
            "prompt_cache_key".to_owned(),
            serde_json::value::to_raw_value(prompt_cache_key).expect("JSON text serializes"),
        );
    }
    let stream = object
        .get("stream")
        .map(|value| boolean(value, "stream"))
        .transpose()?;
    let mut messages = object
        .get("instructions")
        .map(|value| text(value, "instructions"))
        .transpose()?
        .map(|content| ChatMessage {
            role: ChatMessageRole::System as i32,
            content: Some(chat_message::Content::TextContent(content)),
            ..Default::default()
        })
        .into_iter()
        .collect::<Vec<_>>();
    messages.extend(decode_input(required(object, "input")?)?);
    if messages.is_empty() {
        return Err(invalid("input must not be empty."));
    }
    let tools = object
        .get("tools")
        .map(|value| array(value, "tools"))
        .transpose()?
        .map(|tools| tools.iter().map(decode_tool).collect())
        .transpose()?
        .unwrap_or_default();
    Ok(ChatRequest {
        model_id: text(required(object, "model")?, "model")?,
        messages,
        tools,
        tool_choice: object
            .get("tool_choice")
            .map(decode_tool_choice)
            .transpose()?,
        parallel_tool_calls: object
            .get("parallel_tool_calls")
            .map(|value| boolean(value, "parallel_tool_calls"))
            .transpose()?,
        generation_controls: Some(decode_controls(object)?),
        stream,
        // Streamed responses always report usage in their completed event.
        include_stream_usage: stream.unwrap_or(false).then_some(true),
        request_extensions_json: request_controls::extensions_json(&extensions),
        store: Some(false),
    })
}

/// Encodes one generated Chat result as a completed stateless Response.
///
/// A result with more than one choice violates the requested single-choice shape and is
/// reported as an invalid engine response instead of merging choices.
pub(crate) fn completion(
    mut result: ChatCompletionResult,
    admitted_route_name: &str,
) -> OpenAiResponse {
    if result.choices.len() > 1 {
        return chat::adapter_error();
    }
    result.model_id = admitted_route_name.to_owned();
    json_response(
        200,
        response_json(&result, "completed", output_items(&result)),
    )
}

/// Encodes a terminal public Responses stream error after SSE headers are sent.
pub(crate) fn stream_error_frame() -> Vec<u8> {
    frame(
        "response.failed",
        json!({
            "type":"response.failed",
            "response":{
                "id":"resp_pending",
                "object":"response",
                "created_at":0,
                "status":"failed",
                "model":"",
                "output":[],
                "store":false,
                "error":{"code":"upstream_error","message":"The inference engine returned an invalid response."},
                "incomplete_details":null
            }
        }),
    )
}

/// Encodes generated Chat stream events as typed Responses SSE frames.
pub(crate) struct StreamEncoder {
    response_id: Option<String>,
    admitted_route_name: String,
    created_at: Option<u64>,
    reasoning_index: Option<u32>,
    reasoning: String,
    text_index: Option<u32>,
    text: String,
    next_output_index: u32,
    tools: BTreeMap<u32, StreamTool>,
    usage: Option<TokenUsage>,
}

impl StreamEncoder {
    /// Creates an encoder for one public Responses SSE stream.
    pub(crate) fn new(admitted_route_name: String) -> Self {
        Self {
            response_id: None,
            admitted_route_name,
            created_at: None,
            reasoning_index: None,
            reasoning: String::new(),
            text_index: None,
            text: String::new(),
            next_output_index: 0,
            tools: BTreeMap::new(),
            usage: None,
        }
    }

    /// Encodes one internal event in its public Responses event sequence.
    ///
    /// An event for any choice other than the first is refused before it changes the response.
    pub(crate) fn encode(
        &mut self,
        event: ChatStreamEvent,
    ) -> Result<Vec<Vec<u8>>, UnexpectedChoice> {
        if crate::router::openai_compatible_codec::stream_event_choice_index(&event)
            .is_some_and(|index| index != 0)
        {
            return Err(UnexpectedChoice);
        }
        Ok(self.encode_single_choice(event))
    }

    fn encode_single_choice(&mut self, event: ChatStreamEvent) -> Vec<Vec<u8>> {
        match event.event {
            Some(Event::Started(started)) => {
                self.response_id = Some(response_id(&started.completion_id));
                self.created_at = Some(started.created_at_unix_seconds);
                let response = self.response("in_progress", Vec::new());
                vec![
                    frame(
                        "response.created",
                        json!({"type":"response.created","response":response}),
                    ),
                    frame(
                        "response.in_progress",
                        json!({"type":"response.in_progress","response":self.response("in_progress", Vec::new())}),
                    ),
                ]
            }
            Some(Event::TextDelta(delta)) => self.text_delta(delta),
            Some(Event::ToolCallDelta(delta)) => self.tool_delta(delta),
            Some(Event::ReasoningDelta(delta)) => self.reasoning_delta(delta),
            Some(Event::Finished(_)) => self.finish_output(),
            Some(Event::Usage(usage)) => {
                self.usage = Some(usage);
                Vec::new()
            }
            Some(Event::Completed(_)) => vec![frame(
                "response.completed",
                json!({"type":"response.completed","response":self.response("completed", self.current_output())}),
            )],
            None => Vec::new(),
        }
    }

    fn reasoning_delta(
        &mut self,
        delta: dotcore_proto::dotcore::inference::v1::ChatReasoningDelta,
    ) -> Vec<Vec<u8>> {
        let mut frames = Vec::new();
        let output_index = match self.reasoning_index {
            Some(index) => index,
            None => {
                let index = self.allocate_output_index();
                self.reasoning_index = Some(index);
                frames.push(frame(
                    "response.output_item.added",
                    json!({
                        "type":"response.output_item.added",
                        "output_index":index,
                        "item":reasoning_item(self.reasoning_id(), "", "in_progress")
                    }),
                ));
                index
            }
        };
        self.reasoning.push_str(&delta.reasoning);
        frames.push(frame(
            "response.reasoning_text.delta",
            json!({
                "type":"response.reasoning_text.delta",
                "item_id":self.reasoning_id(),
                "output_index":output_index,
                "content_index":0,
                "delta":delta.reasoning
            }),
        ));
        frames
    }

    fn text_delta(
        &mut self,
        delta: dotcore_proto::dotcore::inference::v1::ChatTextDelta,
    ) -> Vec<Vec<u8>> {
        let Some(content) = delta.content else {
            return Vec::new();
        };
        let content = match content {
            chat_text_delta::Content::TextContent(content) => content,
            chat_text_delta::Content::NullContent(_) => return Vec::new(),
        };
        if content.is_empty() && self.text_index.is_none() {
            return Vec::new();
        }
        let mut frames = Vec::new();
        let output_index = match self.text_index {
            Some(index) => index,
            None => {
                let index = self.allocate_output_index();
                self.text_index = Some(index);
                frames.push(frame("response.output_item.added", json!({"type":"response.output_item.added","output_index":index,"item":message_item(self.message_id(), "", "in_progress")})));
                frames.push(frame("response.content_part.added", json!({"type":"response.content_part.added","item_id":self.message_id(),"output_index":index,"content_index":0,"part":{"type":"output_text","text":"","annotations":[]}})));
                index
            }
        };
        self.text.push_str(&content);
        frames.push(frame("response.output_text.delta", json!({"type":"response.output_text.delta","item_id":self.message_id(),"output_index":output_index,"content_index":0,"delta":content})));
        frames
    }

    fn tool_delta(
        &mut self,
        delta: dotcore_proto::dotcore::inference::v1::ChatToolCallDelta,
    ) -> Vec<Vec<u8>> {
        let existing_index = self
            .tools
            .get(&delta.tool_call_index)
            .and_then(|tool| tool.output_index);
        let output_index = existing_index.unwrap_or_else(|| self.allocate_output_index());
        let tool = self.tools.entry(delta.tool_call_index).or_default();
        if let Some(call_id) = delta.tool_call_id {
            tool.call_id = Some(call_id);
        }
        if let Some(name) = delta.function_name {
            tool.name = Some(name);
        }
        let mut frames = Vec::new();
        if existing_index.is_none() {
            tool.output_index = Some(output_index);
            frames.push(frame("response.output_item.added", json!({"type":"response.output_item.added","output_index":output_index,"item":function_item(tool, "", "in_progress")})));
        }
        if let Some(fragment) = delta.arguments_json_fragment {
            tool.arguments.push_str(&fragment);
            frames.push(frame("response.function_call_arguments.delta", json!({"type":"response.function_call_arguments.delta","item_id":function_item_id(tool),"output_index":output_index,"delta":fragment})));
        }
        frames
    }

    fn finish_output(&mut self) -> Vec<Vec<u8>> {
        let mut frames = Vec::new();
        if let Some(output_index) = self.reasoning_index {
            frames.push(frame("response.reasoning_text.done", json!({"type":"response.reasoning_text.done","item_id":self.reasoning_id(),"output_index":output_index,"content_index":0,"text":self.reasoning})));
            frames.push(frame("response.output_item.done", json!({"type":"response.output_item.done","output_index":output_index,"item":reasoning_item(self.reasoning_id(), &self.reasoning, "completed")})));
        }
        if let Some(output_index) = self.text_index {
            frames.push(frame("response.output_text.done", json!({"type":"response.output_text.done","item_id":self.message_id(),"output_index":output_index,"content_index":0,"text":self.text})));
            frames.push(frame("response.content_part.done", json!({"type":"response.content_part.done","item_id":self.message_id(),"output_index":output_index,"content_index":0,"part":{"type":"output_text","text":self.text,"annotations":[]}})));
            frames.push(frame("response.output_item.done", json!({"type":"response.output_item.done","output_index":output_index,"item":message_item(self.message_id(), &self.text, "completed")})));
        }
        for tool in self.tools.values() {
            if let Some(output_index) = tool.output_index {
                frames.push(frame("response.function_call_arguments.done", json!({"type":"response.function_call_arguments.done","item_id":function_item_id(tool),"output_index":output_index,"arguments":tool.arguments})));
                frames.push(frame("response.output_item.done", json!({"type":"response.output_item.done","output_index":output_index,"item":function_item(tool, &tool.arguments, "completed")})));
            }
        }
        frames
    }

    fn current_output(&self) -> Vec<Value> {
        let mut output = Vec::new();
        if let Some(index) = self.reasoning_index {
            output.push((
                index,
                reasoning_item(self.reasoning_id(), &self.reasoning, "completed"),
            ));
        }
        if let Some(index) = self.text_index {
            output.push((
                index,
                message_item(self.message_id(), &self.text, "completed"),
            ));
        }
        output.extend(self.tools.values().filter_map(|tool| {
            tool.output_index
                .map(|index| (index, function_item(tool, &tool.arguments, "completed")))
        }));
        output.sort_by_key(|(index, _)| *index);
        output.into_iter().map(|(_, item)| item).collect()
    }
    fn allocate_output_index(&mut self) -> u32 {
        let index = self.next_output_index;
        self.next_output_index = self
            .next_output_index
            .checked_add(1)
            .expect("Responses output index overflow");
        index
    }

    fn response(&self, status: &str, output: Vec<Value>) -> Value {
        let mut value = json!({"id":self.response_id.as_deref().unwrap_or("resp_pending"),"object":"response","created_at":self.created_at.unwrap_or_default(),"status":status,"model":self.admitted_route_name,"output":output,"store":false,"error":null,"incomplete_details":null});
        if let Some(usage) = &self.usage {
            value
                .as_object_mut()
                .expect("response object")
                .insert("usage".to_owned(), usage_json(usage));
        }
        value
    }

    fn reasoning_id(&self) -> String {
        format!("rs_{}", self.response_id.as_deref().unwrap_or("pending"))
    }

    fn message_id(&self) -> String {
        format!("msg_{}", self.response_id.as_deref().unwrap_or("pending"))
    }
}

/// An engine stream event for a second choice, which a single-choice Response cannot carry.
#[derive(Debug, thiserror::Error)]
#[error("Responses stream received an event for a choice other than the first")]
pub(crate) struct UnexpectedChoice;

#[derive(Default)]
struct StreamTool {
    call_id: Option<String>,
    name: Option<String>,
    arguments: String,
    output_index: Option<u32>,
}

fn decode_input(value: &Value) -> Result<Vec<ChatMessage>, OpenAiResponse> {
    match value {
        Value::String(content) => Ok(vec![text_message(ChatMessageRole::User, content.clone())]),
        Value::Array(items) if !items.is_empty() => {
            let mut messages: Vec<ChatMessage> = Vec::new();
            for item in items {
                let is_function_call = object(item, "input item")?
                    .get("type")
                    .and_then(Value::as_str)
                    == Some("function_call");
                let message = decode_item(item)?;
                merge_function_call_history(&mut messages, is_function_call, message);
            }
            Ok(messages)
        }
        Value::Array(_) => Err(invalid("input must not be empty.")),
        _ => Err(invalid("input must be text or an item array.")),
    }
}

fn merge_function_call_history(
    messages: &mut Vec<ChatMessage>,
    is_function_call: bool,
    message: ChatMessage,
) {
    if is_function_call
        && let Some(previous) = messages.last_mut()
        && previous.role == ChatMessageRole::Assistant as i32
        && matches!(
            previous.content,
            Some(chat_message::Content::NullContent(_))
        )
    {
        previous.tool_calls.extend(message.tool_calls);
    } else {
        messages.push(message);
    }
}

fn decode_item(value: &Value) -> Result<ChatMessage, OpenAiResponse> {
    let item = object(value, "input item")?;
    let kind = item
        .get("type")
        .map(|value| text(value, "input item type"))
        .transpose()?;
    match kind.as_deref() {
        None | Some("message") => decode_message_item(item, kind.is_some()),
        Some("function_call") => {
            reject_unknown(
                item,
                &["type", "id", "status", "call_id", "name", "arguments"],
            )?;
            Ok(ChatMessage {
                role: ChatMessageRole::Assistant as i32,
                content: Some(chat_message::Content::NullContent(
                    NullValue::NullValue as i32,
                )),
                tool_calls: vec![FunctionToolCall {
                    tool_call_id: text(required(item, "call_id")?, "function call ID")?,
                    function_name: text(required(item, "name")?, "function name")?,
                    arguments_json: text(required(item, "arguments")?, "function arguments")?,
                }],
                name: None,
                content_parts: Vec::new(),
                reasoning: None,
            })
        }
        Some("function_call_output") => {
            reject_unknown(item, &["type", "id", "status", "call_id", "output"])?;
            Ok(ChatMessage {
                role: ChatMessageRole::Tool as i32,
                content: Some(chat_message::Content::ToolResult(FunctionToolResult {
                    tool_call_id: text(required(item, "call_id")?, "function call ID")?,
                    content: decode_text_content(required(item, "output")?)?,
                })),
                tool_calls: Vec::new(),
                name: None,
                content_parts: Vec::new(),
                reasoning: None,
            })
        }
        Some("item_reference") => Err(unsupported("Response item references are not supported.")),
        _ => Err(unsupported("This Responses input item is not supported.")),
    }
}

fn decode_message_item(
    item: &Map<String, Value>,
    has_type: bool,
) -> Result<ChatMessage, OpenAiResponse> {
    let allowed = if has_type {
        &["type", "id", "status", "role", "content"][..]
    } else {
        &["id", "status", "role", "content"][..]
    };
    reject_unknown(item, allowed)?;
    let role = match text(required(item, "role")?, "message role")?.as_str() {
        "system" => ChatMessageRole::System,
        "developer" => ChatMessageRole::Developer,
        "user" => ChatMessageRole::User,
        "assistant" => ChatMessageRole::Assistant,
        _ => return Err(invalid("message role is invalid.")),
    };
    decode_message_content(role, required(item, "content")?)
}

fn decode_message_content(
    role: ChatMessageRole,
    value: &Value,
) -> Result<ChatMessage, OpenAiResponse> {
    match value {
        Value::String(content) => Ok(text_message(role, content.clone())),
        Value::Array(parts) if !parts.is_empty() => Ok(ChatMessage {
            role: role as i32,
            content: None,
            content_parts: parts
                .iter()
                .map(decode_content_part)
                .collect::<Result<_, _>>()?,
            ..Default::default()
        }),
        Value::Array(_) => Err(invalid("message content must not be empty.")),
        _ => Err(invalid("message content must be text or text parts.")),
    }
}

fn decode_text_content(value: &Value) -> Result<String, OpenAiResponse> {
    match value {
        Value::String(content) => Ok(content.clone()),
        Value::Array(parts) if !parts.is_empty() => parts.iter().map(decode_text_part).collect(),
        Value::Array(_) => Err(invalid("message content must not be empty.")),
        _ => Err(invalid("message content must be text or text parts.")),
    }
}

fn decode_text_part(value: &Value) -> Result<String, OpenAiResponse> {
    let part = decode_content_part(value)?;
    match part.content {
        Some(chat_content_part::Content::TextContent(text)) => Ok(text),
        Some(chat_content_part::Content::ImageContent(_)) => {
            Err(invalid("function output must be text."))
        }
        None => Err(invalid("content part is invalid.")),
    }
}

fn decode_content_part(value: &Value) -> Result<ChatContentPart, OpenAiResponse> {
    let part = object(value, "content part")?;
    let kind = text(required(part, "type")?, "content part type")?;
    match kind.as_str() {
        "input_text" | "output_text" => {
            reject_unknown(part, &["type", "text", "annotations"])?;
            Ok(ChatContentPart {
                content: Some(chat_content_part::Content::TextContent(text(
                    required(part, "text")?,
                    "content text",
                )?)),
            })
        }
        "input_image" => {
            reject_unknown(part, &["type", "image_url", "detail"])?;
            let detail = part
                .get("detail")
                .map(|value| text(value, "image detail"))
                .transpose()?
                .unwrap_or_else(|| "auto".to_owned());
            let image =
                images::decode_data_url(&text(required(part, "image_url")?, "image URL")?, &detail)
                    .map_err(|cause| chat::image_error(&cause))?;
            Ok(ChatContentPart {
                content: Some(chat_content_part::Content::ImageContent(image)),
            })
        }
        _ => Err(unsupported("This Responses content part is not supported.")),
    }
}

fn decode_tool(value: &Value) -> Result<FunctionToolDefinition, OpenAiResponse> {
    let tool = object(value, "tool")?;
    if text(required(tool, "type")?, "tool type")? != "function" {
        return Err(unsupported(
            "Only client-defined function tools are supported.",
        ));
    }
    reject_unknown(
        tool,
        &["type", "name", "description", "parameters", "strict"],
    )?;
    Ok(FunctionToolDefinition {
        name: text(required(tool, "name")?, "function name")?,
        description: tool
            .get("description")
            .map(|value| text(value, "function description"))
            .transpose()?,
        parameters_json_schema: serde_json::to_string(required(tool, "parameters")?)
            .expect("JSON schema"),
        strict: tool
            .get("strict")
            .map(|value| boolean(value, "function strict"))
            .transpose()?,
    })
}

fn decode_tool_choice(value: &Value) -> Result<ChatToolChoice, OpenAiResponse> {
    let selection = match value {
        Value::String(mode) => match mode.as_str() {
            "none" => chat_tool_choice::Selection::Mode(ChatToolChoiceMode::None as i32),
            "auto" => chat_tool_choice::Selection::Mode(ChatToolChoiceMode::Auto as i32),
            "required" => chat_tool_choice::Selection::Mode(ChatToolChoiceMode::Required as i32),
            _ => return Err(invalid("tool_choice is invalid.")),
        },
        Value::Object(choice) => {
            reject_unknown(choice, &["type", "name"])?;
            if text(required(choice, "type")?, "tool_choice type")? != "function" {
                return Err(invalid("tool_choice must select a function."));
            }
            chat_tool_choice::Selection::FunctionName(text(
                required(choice, "name")?,
                "tool_choice function name",
            )?)
        }
        _ => return Err(invalid("tool_choice is invalid.")),
    };
    Ok(ChatToolChoice {
        selection: Some(selection),
    })
}

fn decode_controls(object: &Map<String, Value>) -> Result<ChatGenerationControls, OpenAiResponse> {
    Ok(ChatGenerationControls {
        temperature: chat::optional(object, "temperature")
            .map(|value| number(value, "temperature").map(|value| value as f32))
            .transpose()?,
        top_p: chat::optional(object, "top_p")
            .map(|value| number(value, "top_p").map(|value| value as f32))
            .transpose()?,
        max_tokens: None,
        max_completion_tokens: chat::optional(object, "max_output_tokens")
            .map(|value| unsigned(value, "max_output_tokens"))
            .transpose()?,
        stop_sequences: None,
        reasoning_effort: decode_reasoning_effort(object)?,
        presence_penalty: chat::optional(object, "presence_penalty")
            .map(|value| number(value, "presence_penalty"))
            .transpose()?,
        frequency_penalty: chat::optional(object, "frequency_penalty")
            .map(|value| number(value, "frequency_penalty"))
            .transpose()?,
    })
}

/// Reads `reasoning.effort`, accepting the Chat-style `reasoning_effort` alias only when equal.
fn decode_reasoning_effort(object: &Map<String, Value>) -> Result<Option<String>, OpenAiResponse> {
    let nested = match chat::optional(object, "reasoning") {
        None => None,
        Some(Value::Object(reasoning)) => {
            if reasoning.keys().any(|key| key != "effort") {
                return Err(chat::request_error(InvalidRequest::unsupported_feature(
                    Some("reasoning"),
                )));
            }
            chat::optional(reasoning, "effort")
                .map(|value| text(value, "reasoning.effort"))
                .transpose()?
        }
        Some(_) => {
            return Err(chat::request_error(InvalidRequest::invalid_field(Some(
                "reasoning",
            ))));
        }
    };
    let alias = chat::optional(object, "reasoning_effort")
        .map(|value| text(value, "reasoning_effort"))
        .transpose()?;
    match (nested, alias) {
        (Some(nested), Some(alias)) if nested != alias => Err(chat::request_error(
            InvalidRequest::invalid_field(Some("reasoning")),
        )),
        (nested, alias) => Ok(nested.or(alias)),
    }
}

fn output_items(result: &ChatCompletionResult) -> Vec<Value> {
    result.choices.iter().flat_map(|choice| choice.message.as_ref()).flat_map(|message| {
        let mut output = Vec::new();
        if let Some(reasoning) = &message.reasoning {
            output.push(reasoning_item(
                format!("rs_{}", response_id(&result.completion_id)),
                reasoning,
                "completed",
            ));
        }
        if let Some(chat_message::Content::TextContent(text)) = &message.content {
            output.push(message_item(
                format!("msg_{}", response_id(&result.completion_id)),
                text,
                "completed",
            ));
        }
        output.extend(message.tool_calls.iter().map(|call| json!({"id":format!("fc_{}", call.tool_call_id),"type":"function_call","status":"completed","call_id":call.tool_call_id,"name":call.function_name,"arguments":call.arguments_json})));
        output
    }).collect()
}

fn response_json(result: &ChatCompletionResult, status: &str, output: Vec<Value>) -> Value {
    let mut value = json!({"id":response_id(&result.completion_id),"object":"response","created_at":result.created_at_unix_seconds,"status":status,"model":result.model_id,"output":output,"store":false,"error":null,"incomplete_details":null});
    if let Some(usage) = &result.usage {
        value
            .as_object_mut()
            .expect("response object")
            .insert("usage".to_owned(), usage_json(usage));
    }
    value
}

fn response_id(completion_id: &str) -> String {
    format!("resp_{completion_id}")
}
fn reasoning_item(id: impl AsRef<str>, text: &str, status: &str) -> Value {
    let content = if text.is_empty() {
        Vec::new()
    } else {
        vec![json!({"type":"reasoning_text","text":text})]
    };
    json!({
        "id":id.as_ref(),
        "type":"reasoning",
        "status":status,
        "summary":[],
        "content":content
    })
}

fn message_item(id: impl AsRef<str>, text: &str, status: &str) -> Value {
    json!({"id":id.as_ref(),"type":"message","status":status,"role":"assistant","content":[{"type":"output_text","text":text,"annotations":[]}]})
}
fn function_item_id(tool: &StreamTool) -> String {
    format!("fc_{}", tool.call_id.as_deref().unwrap_or("pending"))
}
fn function_item(tool: &StreamTool, arguments: &str, status: &str) -> Value {
    json!({"id":function_item_id(tool),"type":"function_call","status":status,"call_id":tool.call_id.as_deref().unwrap_or(""),"name":tool.name.as_deref().unwrap_or(""),"arguments":arguments})
}
fn usage_json(usage: &TokenUsage) -> Value {
    let mut value = json!({"input_tokens":usage.prompt_tokens,"output_tokens":usage.completion_tokens,"total_tokens":usage.total_tokens});
    let object = value.as_object_mut().expect("usage object");
    if let Some(cached_tokens) = usage.cached_prompt_tokens {
        object.insert(
            "input_tokens_details".to_owned(),
            json!({"cached_tokens":cached_tokens}),
        );
    }
    if let Some(reasoning_tokens) = usage.reasoning_tokens {
        object.insert(
            "output_tokens_details".to_owned(),
            json!({"reasoning_tokens":reasoning_tokens}),
        );
    }
    value
}
fn frame(name: &str, value: Value) -> Vec<u8> {
    format!(
        "event: {name}\ndata: {}\n\n",
        serde_json::to_string(&value).expect("JSON event")
    )
    .into_bytes()
}
fn text_message(role: ChatMessageRole, content: String) -> ChatMessage {
    ChatMessage {
        role: role as i32,
        content: Some(chat_message::Content::TextContent(content)),
        content_parts: Vec::new(),
        ..Default::default()
    }
}
fn json_response(status: u16, value: Value) -> OpenAiResponse {
    OpenAiResponse {
        status,
        content_type: "application/json".to_owned(),
        body: serde_json::to_vec(&value).expect("JSON response"),
        passthrough_headers: Vec::new(),
    }
}
fn invalid(message: &str) -> OpenAiResponse {
    chat::error(400, "invalid_request_error", message)
}
fn unsupported(message: &str) -> OpenAiResponse {
    chat::error(400, "unsupported_feature", message)
}
fn required<'a>(object: &'a Map<String, Value>, name: &str) -> Result<&'a Value, OpenAiResponse> {
    object
        .get(name)
        .ok_or_else(|| invalid(&format!("{name} is required.")))
}
fn object<'a>(value: &'a Value, name: &str) -> Result<&'a Map<String, Value>, OpenAiResponse> {
    value
        .as_object()
        .ok_or_else(|| invalid(&format!("{name} must be an object.")))
}
fn array<'a>(value: &'a Value, name: &str) -> Result<&'a Vec<Value>, OpenAiResponse> {
    value
        .as_array()
        .ok_or_else(|| invalid(&format!("{name} must be an array.")))
}
fn text(value: &Value, name: &str) -> Result<String, OpenAiResponse> {
    value
        .as_str()
        .map(str::to_owned)
        .ok_or_else(|| invalid(&format!("{name} must be text.")))
}
fn boolean(value: &Value, name: &str) -> Result<bool, OpenAiResponse> {
    value
        .as_bool()
        .ok_or_else(|| invalid(&format!("{name} must be a boolean.")))
}
fn number(value: &Value, name: &str) -> Result<f64, OpenAiResponse> {
    value
        .as_f64()
        .ok_or_else(|| invalid(&format!("{name} must be a number.")))
}
fn unsigned(value: &Value, name: &str) -> Result<u32, OpenAiResponse> {
    value
        .as_u64()
        .and_then(|value| u32::try_from(value).ok())
        .ok_or_else(|| invalid(&format!("{name} must be an unsigned 32-bit integer.")))
}
fn reject_unknown(object: &Map<String, Value>, allowed: &[&str]) -> Result<(), OpenAiResponse> {
    let allowed = allowed.iter().copied().collect::<BTreeSet<_>>();
    if object.keys().any(|key| !allowed.contains(key.as_str())) {
        return Err(unsupported("Request contains an unsupported field."));
    }
    Ok(())
}

/// Encodes a whole internal event sequence for production codec fidelity tests.
#[cfg(test)]
pub(crate) fn stream(events: Vec<ChatStreamEvent>) -> OpenAiResponse {
    let mut encoder = StreamEncoder::new("fixture-route".to_owned());
    let body = events
        .into_iter()
        .flat_map(|event| encoder.encode(event).expect("single-choice event"))
        .flatten()
        .collect();
    OpenAiResponse {
        status: 200,
        content_type: "text/event-stream".to_owned(),
        body,
        passthrough_headers: Vec::new(),
    }
}

#[cfg(test)]
mod tests {
    use dotcore_proto::dotcore::inference::v1::{
        ChatCompletionChoice, ChatFinishReason, ChatStreamStarted, ChatTextDelta,
    };

    use super::*;
    use crate::router::{openai::OpenAiRequest, test_fixtures};

    #[test]
    fn shared_responses_fixture_preserves_chat_translation_and_typed_sse() {
        let request =
            decode_request(&test_fixtures::responses_request_bytes()).expect("Responses request");
        assert_eq!(request.model_id, "fixture-route");
        assert_eq!(request.messages.len(), 5);
        assert_eq!(request.tools[0].name, "read_file");
        assert_eq!(request.parallel_tool_calls, Some(true));
        assert_eq!(
            request
                .generation_controls
                .as_ref()
                .and_then(|controls| controls.max_completion_tokens),
            Some(64)
        );
        let mut sync_request = request.clone();
        sync_request.stream = Some(false);
        sync_request.include_stream_usage = None;
        let encoded_body = crate::router::openai_compatible_codec::encode_request(&sync_request)
            .expect("OpenAI-compatible request JSON");
        let OpenAiRequest::Chat(roundtrip_request) =
            crate::router::openai::decode("POST", "/v1/chat/completions", &encoded_body)
                .expect("encoded OpenAI-compatible Chat request")
        else {
            panic!("encoded request must decode as Chat")
        };
        assert_eq!(roundtrip_request, sync_request);

        let stream_events = crate::router::openai_compatible_codec::decode_sse(
            &test_fixtures::fragmented_stream_body(),
        )
        .expect("fragmented Chat stream");
        let response_stream = stream(stream_events);
        let text = std::str::from_utf8(&response_stream.body).expect("Responses SSE");
        assert!(!text.contains("[DONE]"));
        assert_eq!(
            response_stream_event_names(text),
            [
                "response.created",
                "response.in_progress",
                "response.output_item.added",
                "response.content_part.added",
                "response.output_text.delta",
                "response.output_item.added",
                "response.function_call_arguments.delta",
                "response.function_call_arguments.delta",
                "response.function_call_arguments.delta",
                "response.output_text.done",
                "response.content_part.done",
                "response.output_item.done",
                "response.function_call_arguments.done",
                "response.output_item.done",
                "response.completed",
            ]
        );
    }

    #[test]
    fn responses_emit_reasoning_items_separately_from_final_text() {
        let completion_response = completion(
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
                        finish_reason: dotcore_proto::dotcore::inference::v1::ChatFinishReason::Stop
                            as i32,
                    },
                ],
                usage: None,
            },
            "public-route",
        );
        let completion_json =
            serde_json::from_slice::<Value>(&completion_response.body).expect("response JSON");
        assert_eq!(
            completion_json.pointer("/output/0/type"),
            Some(&json!("reasoning"))
        );
        assert_eq!(
            completion_json.pointer("/output/0/content/0"),
            Some(&json!({"type":"reasoning_text","text":"analysis"}))
        );
        assert_eq!(
            completion_json.pointer("/output/1/type"),
            Some(&json!("message"))
        );
        assert_eq!(
            completion_json.pointer("/output/1/content/0/text"),
            Some(&json!("answer"))
        );

        let response_stream = stream(vec![
            ChatStreamEvent {
                event: Some(Event::Started(
                    dotcore_proto::dotcore::inference::v1::ChatStreamStarted {
                        completion_id: "chatcmpl_1".to_owned(),
                        model_id: "catalog-model".to_owned(),
                        created_at_unix_seconds: 1,
                    },
                )),
            },
            ChatStreamEvent {
                event: Some(Event::TextDelta(
                    dotcore_proto::dotcore::inference::v1::ChatTextDelta {
                        choice_index: 0,
                        role: Some(ChatMessageRole::Assistant as i32),
                        content: Some(chat_text_delta::Content::TextContent(String::new())),
                    },
                )),
            },
            ChatStreamEvent {
                event: Some(Event::ReasoningDelta(
                    dotcore_proto::dotcore::inference::v1::ChatReasoningDelta {
                        choice_index: 0,
                        reasoning: "analysis".to_owned(),
                    },
                )),
            },
            ChatStreamEvent {
                event: Some(Event::TextDelta(
                    dotcore_proto::dotcore::inference::v1::ChatTextDelta {
                        choice_index: 0,
                        role: Some(ChatMessageRole::Assistant as i32),
                        content: Some(chat_text_delta::Content::TextContent("answer".to_owned())),
                    },
                )),
            },
            ChatStreamEvent {
                event: Some(Event::ToolCallDelta(
                    dotcore_proto::dotcore::inference::v1::ChatToolCallDelta {
                        choice_index: 0,
                        tool_call_index: 0,
                        tool_call_id: Some("call_1".to_owned()),
                        function_name: Some("read_file".to_owned()),
                        arguments_json_fragment: Some("{}".to_owned()),
                    },
                )),
            },
            ChatStreamEvent {
                event: Some(Event::Finished(
                    dotcore_proto::dotcore::inference::v1::ChatStreamFinished {
                        choice_index: 0,
                        finish_reason:
                            dotcore_proto::dotcore::inference::v1::ChatFinishReason::ToolCalls
                                as i32,
                    },
                )),
            },
            ChatStreamEvent {
                event: Some(Event::Completed(Default::default())),
            },
        ]);
        let body = std::str::from_utf8(&response_stream.body).expect("Responses SSE");
        assert_eq!(
            response_stream_event_names(body),
            [
                "response.created",
                "response.in_progress",
                "response.output_item.added",
                "response.reasoning_text.delta",
                "response.output_item.added",
                "response.content_part.added",
                "response.output_text.delta",
                "response.output_item.added",
                "response.function_call_arguments.delta",
                "response.reasoning_text.done",
                "response.output_item.done",
                "response.output_text.done",
                "response.content_part.done",
                "response.output_item.done",
                "response.function_call_arguments.done",
                "response.output_item.done",
                "response.completed",
            ]
        );
        assert_eq!(
            response_stream_indexed_items(body),
            [
                (
                    "response.output_item.added".to_owned(),
                    "rs_resp_chatcmpl_1".to_owned(),
                    0,
                ),
                (
                    "response.reasoning_text.delta".to_owned(),
                    "rs_resp_chatcmpl_1".to_owned(),
                    0,
                ),
                (
                    "response.output_item.added".to_owned(),
                    "msg_resp_chatcmpl_1".to_owned(),
                    1,
                ),
                (
                    "response.content_part.added".to_owned(),
                    "msg_resp_chatcmpl_1".to_owned(),
                    1,
                ),
                (
                    "response.output_text.delta".to_owned(),
                    "msg_resp_chatcmpl_1".to_owned(),
                    1,
                ),
                (
                    "response.output_item.added".to_owned(),
                    "fc_call_1".to_owned(),
                    2,
                ),
                (
                    "response.function_call_arguments.delta".to_owned(),
                    "fc_call_1".to_owned(),
                    2,
                ),
                (
                    "response.reasoning_text.done".to_owned(),
                    "rs_resp_chatcmpl_1".to_owned(),
                    0,
                ),
                (
                    "response.output_item.done".to_owned(),
                    "rs_resp_chatcmpl_1".to_owned(),
                    0,
                ),
                (
                    "response.output_text.done".to_owned(),
                    "msg_resp_chatcmpl_1".to_owned(),
                    1,
                ),
                (
                    "response.content_part.done".to_owned(),
                    "msg_resp_chatcmpl_1".to_owned(),
                    1,
                ),
                (
                    "response.output_item.done".to_owned(),
                    "msg_resp_chatcmpl_1".to_owned(),
                    1,
                ),
                (
                    "response.function_call_arguments.done".to_owned(),
                    "fc_call_1".to_owned(),
                    2,
                ),
                (
                    "response.output_item.done".to_owned(),
                    "fc_call_1".to_owned(),
                    2,
                ),
            ]
        );
        let completed = body
            .split("\n\n")
            .find(|frame| frame.starts_with("event: response.completed\n"))
            .and_then(|frame| frame.split_once("\ndata: ").map(|(_, data)| data))
            .map(|data| serde_json::from_str::<Value>(data).expect("completed event JSON"))
            .expect("completed event");
        assert_eq!(
            completed.pointer("/response/output/0/content/0"),
            Some(&json!({"type":"reasoning_text","text":"analysis"}))
        );
        assert_eq!(
            completed.pointer("/response/output/1/content/0"),
            Some(&json!({"type":"output_text","text":"answer","annotations":[]}))
        );
        assert_eq!(
            completed.pointer("/response/output/2"),
            Some(&json!({
                "id":"fc_call_1",
                "type":"function_call",
                "call_id":"call_1",
                "name":"read_file",
                "arguments":"{}",
                "status":"completed"
            }))
        );
    }

    #[test]
    fn empty_priming_content_does_not_create_a_tool_only_message_item() {
        let response_stream = stream(vec![
            ChatStreamEvent {
                event: Some(Event::Started(
                    dotcore_proto::dotcore::inference::v1::ChatStreamStarted {
                        completion_id: "chatcmpl_tool".to_owned(),
                        model_id: "catalog-model".to_owned(),
                        created_at_unix_seconds: 1,
                    },
                )),
            },
            ChatStreamEvent {
                event: Some(Event::TextDelta(
                    dotcore_proto::dotcore::inference::v1::ChatTextDelta {
                        choice_index: 0,
                        role: Some(ChatMessageRole::Assistant as i32),
                        content: Some(chat_text_delta::Content::TextContent(String::new())),
                    },
                )),
            },
            ChatStreamEvent {
                event: Some(Event::ToolCallDelta(
                    dotcore_proto::dotcore::inference::v1::ChatToolCallDelta {
                        choice_index: 0,
                        tool_call_index: 0,
                        tool_call_id: Some("call_1".to_owned()),
                        function_name: Some("read_file".to_owned()),
                        arguments_json_fragment: Some("{}".to_owned()),
                    },
                )),
            },
            ChatStreamEvent {
                event: Some(Event::Finished(Default::default())),
            },
            ChatStreamEvent {
                event: Some(Event::Completed(Default::default())),
            },
        ]);
        let body = std::str::from_utf8(&response_stream.body).expect("Responses SSE");
        assert_eq!(
            response_stream_event_names(body),
            [
                "response.created",
                "response.in_progress",
                "response.output_item.added",
                "response.function_call_arguments.delta",
                "response.function_call_arguments.done",
                "response.output_item.done",
                "response.completed",
            ]
        );
        assert_eq!(
            response_stream_indexed_items(body),
            [
                (
                    "response.output_item.added".to_owned(),
                    "fc_call_1".to_owned(),
                    0,
                ),
                (
                    "response.function_call_arguments.delta".to_owned(),
                    "fc_call_1".to_owned(),
                    0,
                ),
                (
                    "response.function_call_arguments.done".to_owned(),
                    "fc_call_1".to_owned(),
                    0,
                ),
                (
                    "response.output_item.done".to_owned(),
                    "fc_call_1".to_owned(),
                    0,
                ),
            ]
        );
        let completed = body
            .split("\n\n")
            .find(|frame| frame.starts_with("event: response.completed\n"))
            .and_then(|frame| frame.split_once("\ndata: ").map(|(_, data)| data))
            .map(|data| serde_json::from_str::<Value>(data).expect("completed event JSON"))
            .expect("completed event");
        assert_eq!(
            completed.pointer("/response/output"),
            Some(&json!([{
                "id":"fc_call_1",
                "type":"function_call",
                "call_id":"call_1",
                "name":"read_file",
                "arguments":"{}",
                "status":"completed"
            }]))
        );
    }

    fn response_stream_event_names(body: &str) -> Vec<&str> {
        body.split("\n\n")
            .filter_map(|frame| frame.strip_prefix("event: "))
            .filter_map(|frame| frame.split_once('\n').map(|(name, _)| name))
            .collect()
    }

    fn response_stream_indexed_items(body: &str) -> Vec<(String, String, u64)> {
        body.split("\n\n")
            .filter_map(|frame| {
                let (event, data) = frame.strip_prefix("event: ")?.split_once("\ndata: ")?;
                let data = serde_json::from_str::<Value>(data).expect("SSE event JSON");
                let output_index = data.get("output_index")?.as_u64()?;
                let item_id = data
                    .get("item_id")
                    .or_else(|| data.pointer("/item/id"))?
                    .as_str()?;
                Some((event.to_owned(), item_id.to_owned(), output_index))
            })
            .collect()
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
            serde_json::from_slice::<Value>(&completion.body).expect("response JSON")["model"],
            "public-route"
        );

        let mut encoder = StreamEncoder::new("public-route".to_owned());
        let frames = encoder
            .encode(ChatStreamEvent {
                event: Some(Event::Started(
                    dotcore_proto::dotcore::inference::v1::ChatStreamStarted {
                        completion_id: "chatcmpl_1".to_owned(),
                        model_id: "catalog-model".to_owned(),
                        created_at_unix_seconds: 1,
                    },
                )),
            })
            .expect("single-choice event");
        assert_eq!(frames.len(), 2);
        for frame in frames {
            let frame = std::str::from_utf8(&frame)
                .expect("SSE frame")
                .split_once("\ndata: ")
                .map(|(_, frame)| frame.strip_suffix("\n\n").expect("SSE terminator"))
                .expect("SSE data");
            assert_eq!(
                serde_json::from_str::<Value>(frame).expect("SSE JSON")["response"]["model"],
                "public-route"
            );
        }
    }

    #[test]
    fn parallel_function_history_uses_one_assistant_chat_message() {
        let request = decode_request(
            br#"{"model":"model","store":false,"input":[{"type":"function_call","call_id":"call_1","name":"one","arguments":"{}"},{"type":"function_call","call_id":"call_2","name":"two","arguments":"{}"}]}"#,
        )
        .expect("Responses request");

        assert_eq!(request.messages.len(), 1);
        assert_eq!(request.messages[0].tool_calls.len(), 2);
    }

    #[test]
    fn n8n_default_request_decodes_with_background_off() {
        let body = br#"{"model":"route","input":[{"role":"user","content":"hi"}],"parallel_tool_calls":true,"store":false,"background":false}"#;

        let OpenAiRequest::Responses(request) =
            crate::router::openai::decode("POST", "/v1/responses", body)
                .expect("Responses request")
        else {
            panic!("fixture must decode as Responses")
        };

        assert_eq!(
            request,
            ChatRequest {
                model_id: "route".to_owned(),
                messages: vec![ChatMessage {
                    role: ChatMessageRole::User as i32,
                    content: Some(chat_message::Content::TextContent("hi".to_owned())),
                    ..Default::default()
                }],
                parallel_tool_calls: Some(true),
                generation_controls: Some(ChatGenerationControls::default()),
                store: Some(false),
                ..Default::default()
            }
        );
    }

    #[test]
    fn null_background_is_accepted() {
        let request =
            decode_request(br#"{"model":"route","store":false,"input":"hi","background":null}"#)
                .expect("Responses request");

        assert_eq!(request.request_extensions_json, None);
    }

    #[test]
    fn pi_equivalent_request_maps_to_the_same_canonical_controls() {
        let body = br#"{"model":"qwen3-8-27b","store":false,"instructions":"You are Pi.","input":"Read the README.","stream":true,"max_output_tokens":32000,"reasoning":{"effort":"none"},"reasoning_effort":"none","temperature":1.0,"top_p":0.95,"presence_penalty":0.0,"frequency_penalty":0,"n":1,"prompt_cache_key":"session-1","top_k":20,"repetition_penalty":1.0,"min_p":0,"seed":18446744073709551615,"chat_template_kwargs":{"enable_thinking":false,"preserve_thinking":true,"reasoning_effort":"low","stops":[null,"</think>"]}}"#;

        let OpenAiRequest::Responses(request) =
            crate::router::openai::decode("POST", "/v1/responses", body)
                .expect("Responses request")
        else {
            panic!("fixture must decode as Responses")
        };

        assert_eq!(
            request.generation_controls,
            Some(ChatGenerationControls {
                temperature: Some(1.0),
                top_p: Some(0.95),
                max_tokens: None,
                max_completion_tokens: Some(32000),
                stop_sequences: None,
                reasoning_effort: Some("none".to_owned()),
                presence_penalty: Some(0.0),
                frequency_penalty: Some(0.0),
            })
        );
        assert_eq!(
            request.request_extensions_json.as_deref(),
            Some(
                r#"{"chat_template_kwargs":{"enable_thinking":false,"preserve_thinking":true,"reasoning_effort":"low","stops":[null,"</think>"]},"min_p":0,"prompt_cache_key":"session-1","repetition_penalty":1.0,"seed":18446744073709551615,"top_k":20}"#
            )
        );
        assert_eq!(request.store, Some(false));
        assert_eq!(request.stream, Some(true));
        assert_eq!(
            request.include_stream_usage,
            Some(true),
            "streamed Responses always request terminal usage"
        );

        let OpenAiRequest::Responses(request) = crate::router::openai::decode(
            "POST",
            "/v1/responses",
            br#"{"model":"route","store":false,"input":"hi"}"#,
        )
        .expect("Responses request") else {
            panic!("fixture must decode as Responses")
        };
        assert_eq!(request.include_stream_usage, None);
        assert_eq!(request.request_extensions_json, None);
    }

    #[test]
    fn unsupported_responses_controls_fail_with_safe_fields() {
        let cases = [
            (
                r#""reasoning":{"effort":"low","summary":"auto"}"#,
                "unsupported_parameter",
                "reasoning",
            ),
            (r#""reasoning":"low""#, "invalid_value", "reasoning"),
            (
                r#""reasoning":{"effort":"low"},"reasoning_effort":"high""#,
                "invalid_value",
                "reasoning",
            ),
            (r#""n":2"#, "unsupported_parameter", "n"),
            (r#""n":1.5"#, "invalid_value", "n"),
            (r#""functions":[]"#, "unsupported_parameter", "functions"),
            (
                r#""background":true"#,
                "unsupported_parameter",
                "background",
            ),
            (r#""background":"yes""#, "invalid_value", "background"),
            (
                r#""previous_response_id":"resp_1""#,
                "unsupported_parameter",
                "previous_response_id",
            ),
            (
                r#""text":{"format":{"type":"json_schema"}}"#,
                "unsupported_parameter",
                "text",
            ),
            (r#""messages":[]"#, "invalid_value", "messages"),
            (
                r#""tools":[{"type":"function","name":"read","parameters":[]}]"#,
                "invalid_value",
                "tools",
            ),
        ];
        for (field, code, param) in cases {
            let body = format!(r#"{{"model":"route","store":false,"input":"hi",{field}}}"#);
            let Err(response) =
                crate::router::openai::decode("POST", "/v1/responses", body.as_bytes())
            else {
                panic!("{field} must be rejected");
            };
            let error = serde_json::from_slice::<Value>(&response.body).expect("error JSON");
            assert_eq!(response.status, 400, "{field}");
            assert_eq!(error["error"]["code"], code, "{field}");
            assert_eq!(error["error"]["param"], param, "{field}");
        }
    }

    #[test]
    fn values_the_raw_scan_admits_but_json_values_cannot_hold_are_invalid_bodies() {
        for body in [
            br#"{"model":"route","store":false,"input":"hi","temperature":1e400}"#.as_slice(),
            br#"{"model":"route","store":false,"input":"hi","instructions":"\ud800"}"#.as_slice(),
        ] {
            let Err(response) = crate::router::openai::decode("POST", "/v1/responses", body) else {
                panic!("{} must be rejected", String::from_utf8_lossy(body));
            };
            assert_eq!(response.status, 400);
            assert_eq!(
                serde_json::from_slice::<Value>(&response.body).expect("error JSON"),
                json!({"error":{"message":"Request body must be a valid JSON object.","type":"invalid_request_error","param":null,"code":null}})
            );
        }
    }

    #[test]
    fn responses_render_supplied_usage_details() {
        assert_eq!(
            usage_json(&TokenUsage {
                prompt_tokens: 30,
                completion_tokens: 12,
                total_tokens: 42,
                cached_prompt_tokens: Some(16),
                reasoning_tokens: Some(5),
            }),
            json!({"input_tokens":30,"output_tokens":12,"total_tokens":42,"input_tokens_details":{"cached_tokens":16},"output_tokens_details":{"reasoning_tokens":5}})
        );
    }

    #[test]
    fn a_second_engine_choice_is_refused_instead_of_merged() {
        let choice = |index| ChatCompletionChoice {
            index,
            message: Some(ChatMessage {
                role: ChatMessageRole::Assistant as i32,
                content: Some(chat_message::Content::TextContent("answer".to_owned())),
                ..Default::default()
            }),
            finish_reason: ChatFinishReason::Stop as i32,
        };
        let response = completion(
            ChatCompletionResult {
                completion_id: "completion".to_owned(),
                model_id: "engine".to_owned(),
                created_at_unix_seconds: 1,
                choices: vec![choice(0), choice(1)],
                usage: None,
            },
            "route",
        );
        assert_eq!(response.status, 502);
        assert!(!String::from_utf8_lossy(&response.body).contains("answer"));

        let mut encoder = StreamEncoder::new("route".to_owned());
        let started = encoder
            .encode(ChatStreamEvent {
                event: Some(Event::Started(ChatStreamStarted {
                    completion_id: "completion".to_owned(),
                    model_id: "engine".to_owned(),
                    created_at_unix_seconds: 1,
                })),
            })
            .expect("first-choice event");
        assert!(!started.is_empty());
        assert!(
            encoder
                .encode(ChatStreamEvent {
                    event: Some(Event::TextDelta(ChatTextDelta {
                        choice_index: 1,
                        role: None,
                        content: Some(chat_text_delta::Content::TextContent("other".to_owned())),
                    })),
                })
                .is_err()
        );
    }
}
