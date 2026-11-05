//! Inference protobuf contract tests.
//!
//! These tests pin message-only inference descriptors, public and private field identities,
//! private-field exclusions, and absent-versus-explicit payload semantics. Breaking changes can
//! alter chat transport compatibility, drop caller controls, or leak controller details. Service
//! behavior, backend routing, and runtime execution are intentionally excluded and tested by their
//! owning integration suites.

use dotcore_proto::{
    FILE_DESCRIPTOR_SET,
    dotcore::inference::{
        private::v1::{
            ExactTargetFailure, ExactTargetFailureDisposition, ExactTargetFailureKind,
            ExactTargetRejected, ExactTargetRejectionReason, RequestErrorCategory,
        },
        v1::{
            ChatCompletionChoice, ChatCompletionResult, ChatFinishReason, ChatGenerationControls,
            ChatMessage, ChatMessageRole, ChatRequest, ChatStreamCompleted, ChatStreamEvent,
            ChatStreamFinished, ChatStreamStarted, ChatTextDelta, ChatToolCallDelta,
            FunctionToolCall, FunctionToolDefinition, FunctionToolResult, TokenUsage, chat_message,
            chat_stream_event::Event, chat_text_delta,
        },
    },
    google::protobuf::NullValue,
};
use prost::Message;
use prost_types::{
    DescriptorProto, EnumDescriptorProto, FileDescriptorSet, field_descriptor_proto::Type,
};

#[test]
fn generated_bindings_preserve_chat_payload_semantics() {
    let schema = r#"{"multipleOf":0.0000000000000000000000000000000001}"#;
    let absent_content = ChatMessage {
        role: ChatMessageRole::Assistant as i32,
        ..Default::default()
    };
    let null_content = ChatMessage {
        role: ChatMessageRole::Assistant as i32,
        content: Some(chat_message::Content::NullContent(
            NullValue::NullValue as i32,
        )),
        ..Default::default()
    };
    let absent_delta_content = ChatTextDelta {
        choice_index: 0,
        ..Default::default()
    };
    let null_delta_content = ChatTextDelta {
        choice_index: 0,
        content: Some(chat_text_delta::Content::NullContent(
            NullValue::NullValue as i32,
        )),
        ..Default::default()
    };
    let tool = FunctionToolDefinition {
        name: "read_file".to_owned(),
        description: Some("Read one text file.".to_owned()),
        parameters_json_schema: schema.to_owned(),
        strict: Some(true),
    };
    let result = FunctionToolResult {
        tool_call_id: "call_1".to_owned(),
        content: "file contents".to_owned(),
    };
    let request = ChatRequest {
        model_id: "catalog-model".to_owned(),
        messages: vec![
            ChatMessage {
                role: ChatMessageRole::User as i32,
                content: Some(chat_message::Content::TextContent(
                    "Read the file.".to_owned(),
                )),
                ..Default::default()
            },
            ChatMessage {
                role: ChatMessageRole::Tool as i32,
                content: Some(chat_message::Content::ToolResult(result.clone())),
                ..Default::default()
            },
        ],
        tools: vec![tool.clone()],
        generation_controls: Some(ChatGenerationControls {
            temperature: Some(0.25),
            top_p: Some(0.9),
            max_tokens: Some(128),
            max_completion_tokens: Some(64),
            stop_sequences: Some(Default::default()),
            ..Default::default()
        }),
        ..Default::default()
    };
    let completion = ChatCompletionResult {
        completion_id: "chatcmpl_1".to_owned(),
        model_id: "catalog-model".to_owned(),
        created_at_unix_seconds: 1_754_331_600,
        choices: vec![ChatCompletionChoice {
            index: 0,
            message: Some(ChatMessage {
                role: ChatMessageRole::Assistant as i32,
                content: Some(chat_message::Content::NullContent(
                    NullValue::NullValue as i32,
                )),
                tool_calls: vec![FunctionToolCall {
                    tool_call_id: "call_1".to_owned(),
                    function_name: "read_file".to_owned(),
                    arguments_json: "{\"path\":\"README.md\"}".to_owned(),
                }],
                ..Default::default()
            }),
            finish_reason: ChatFinishReason::ToolCalls as i32,
        }],
        usage: Some(TokenUsage {
            prompt_tokens: 10,
            completion_tokens: 4,
            total_tokens: 14,
            ..Default::default()
        }),
    };
    let events = vec![
        ChatStreamEvent {
            event: Some(Event::Started(ChatStreamStarted {
                completion_id: "chatcmpl_1".to_owned(),
                model_id: "catalog-model".to_owned(),
                created_at_unix_seconds: 1_754_331_600,
            })),
        },
        ChatStreamEvent {
            event: Some(Event::TextDelta(ChatTextDelta {
                choice_index: 0,
                role: Some(ChatMessageRole::Assistant as i32),
                content: Some(chat_text_delta::Content::TextContent(
                    "A tool call follows.".to_owned(),
                )),
            })),
        },
        ChatStreamEvent {
            event: Some(Event::ToolCallDelta(ChatToolCallDelta {
                choice_index: 0,
                tool_call_index: 0,
                tool_call_id: Some("call_1".to_owned()),
                function_name: Some("read_file".to_owned()),
                arguments_json_fragment: Some("{\"path\":".to_owned()),
            })),
        },
        ChatStreamEvent {
            event: Some(Event::Finished(ChatStreamFinished {
                choice_index: 0,
                finish_reason: ChatFinishReason::ToolCalls as i32,
            })),
        },
        ChatStreamEvent {
            event: Some(Event::Usage(TokenUsage {
                prompt_tokens: 10,
                completion_tokens: 4,
                total_tokens: 14,
                cached_prompt_tokens: Some(6),
                reasoning_tokens: Some(0),
            })),
        },
        ChatStreamEvent {
            event: Some(Event::Completed(ChatStreamCompleted {})),
        },
    ];
    assert_ne!(absent_content.encode_to_vec(), null_content.encode_to_vec());
    assert_ne!(
        absent_delta_content.encode_to_vec(),
        null_delta_content.encode_to_vec()
    );
    assert_eq!(
        ChatMessage::decode(null_content.encode_to_vec().as_slice())
            .expect("the null content fixture must decode")
            .content,
        null_content.content,
    );
    assert_eq!(
        ChatRequest::decode(request.encode_to_vec().as_slice())
            .expect("the request fixture must decode"),
        request,
    );
    assert_eq!(
        ChatCompletionResult::decode(completion.encode_to_vec().as_slice())
            .expect("the completion fixture must decode"),
        completion,
    );
    assert_eq!(
        events
            .iter()
            .map(|event| {
                ChatStreamEvent::decode(event.encode_to_vec().as_slice())
                    .expect("the stream event fixture must decode")
            })
            .collect::<Vec<_>>(),
        events,
    );
}

#[test]
fn inference_contracts_keep_payloads_private() {
    for contract in [
        include_str!("../dotcore/inference/v1/chat.proto"),
        include_str!("../dotcore/inference/v1/decision.proto"),
    ] {
        assert!(!contract.contains("\nservice "));
        assert!(!contract.contains("deployment_id"));
        assert!(!contract.contains("runtime_id"));
        assert!(!contract.contains("container_id"));
        assert!(!contract.contains("address"));
    }
}

#[test]
fn chat_contract_keeps_wire_numbers_types_and_presence() {
    let chat = message("dotcore.inference.v1.ChatRequest");
    assert_eq!(
        fields(&chat),
        [
            ("model_id", 1, Type::String, false),
            ("messages", 2, Type::Message, false),
            ("tools", 3, Type::Message, false),
            ("tool_choice", 4, Type::Message, false),
            ("parallel_tool_calls", 5, Type::Bool, true),
            ("generation_controls", 6, Type::Message, false),
            ("stream", 7, Type::Bool, true),
            ("include_stream_usage", 8, Type::Bool, true),
            ("request_extensions_json", 9, Type::String, true),
            ("store", 10, Type::Bool, true),
        ]
    );
    assert_eq!(
        fields(&message("dotcore.inference.v1.ChatGenerationControls")),
        [
            ("temperature", 1, Type::Float, true),
            ("top_p", 2, Type::Float, true),
            ("max_tokens", 3, Type::Uint32, true),
            ("max_completion_tokens", 4, Type::Uint32, true),
            ("stop_sequences", 5, Type::Message, false),
            ("reasoning_effort", 6, Type::String, true),
            ("presence_penalty", 7, Type::Double, true),
            ("frequency_penalty", 8, Type::Double, true),
        ]
    );
    assert_eq!(
        fields(&message("dotcore.inference.v1.TokenUsage")),
        [
            ("prompt_tokens", 1, Type::Uint64, false),
            ("completion_tokens", 2, Type::Uint64, false),
            ("total_tokens", 3, Type::Uint64, false),
            ("cached_prompt_tokens", 4, Type::Uint64, true),
            ("reasoning_tokens", 5, Type::Uint64, true),
        ]
    );
}

#[test]
fn decision_contract_keeps_wire_numbers_types_and_presence() {
    assert_eq!(
        fields(&message("dotcore.inference.v1.DecisionRequest")),
        [
            ("model_id", 1, Type::String, false),
            ("state", 2, Type::Message, false),
            ("questions", 3, Type::Message, false),
        ]
    );
    assert_eq!(
        fields(&message("dotcore.inference.v1.DecisionQuestion")),
        [
            ("name", 1, Type::String, false),
            ("instructions", 2, Type::Message, false),
            ("predicate", 3, Type::Message, false),
            ("choice", 4, Type::Message, false),
            ("score", 5, Type::Message, false),
        ]
    );
    assert_eq!(
        fields(&message("dotcore.inference.v1.DecisionAnswer")),
        [
            ("predicate", 1, Type::Message, false),
            ("choice", 2, Type::Message, false),
            ("score", 3, Type::Message, false),
            ("refusal", 4, Type::Message, false),
        ]
    );
    assert_eq!(
        fields(&message("dotcore.inference.v1.DecisionChoiceAnswer")),
        [
            ("option_probabilities", 1, Type::Double, false),
            ("selected_option_index", 2, Type::Uint32, true),
            ("confidence", 3, Type::Double, true),
        ]
    );
    assert_eq!(
        fields(&message("dotcore.inference.v1.DecisionScoreAnswer")),
        [
            ("level_probabilities", 1, Type::Double, false),
            ("expected_level", 2, Type::Double, true),
            ("selected_level_index", 3, Type::Uint32, true),
            ("confidence", 4, Type::Double, true),
        ]
    );
}

#[test]
fn peer_contract_keeps_request_error_numbers_and_presence() {
    assert_eq!(
        fields(&message(
            "dotcore.inference.private.v1.InvokeExactTargetRequest"
        )),
        [
            ("target", 1, Type::Message, false),
            ("chat", 2, Type::Message, false),
            ("decision", 3, Type::Message, false),
        ]
    );
    assert_eq!(
        fields(&message(
            "dotcore.inference.private.v1.InvokeExactTargetResponse"
        )),
        [
            ("admitted", 1, Type::Message, false),
            ("rejected", 2, Type::Message, false),
            ("completion", 3, Type::Message, false),
            ("stream_event", 4, Type::Message, false),
            ("decision_result", 7, Type::Message, false),
            ("passthrough", 5, Type::Message, false),
            ("failure", 6, Type::Message, false),
        ]
    );
    assert_eq!(
        fields(&message("dotcore.inference.private.v1.ExactTargetRejected")),
        [
            ("reason", 1, Type::Enum, false),
            ("safe_param", 2, Type::String, true),
            ("request_error_category", 3, Type::Enum, true),
        ]
    );
    assert_eq!(
        fields(&message("dotcore.inference.private.v1.ExactTargetFailure")),
        [
            ("disposition", 1, Type::Enum, false),
            ("kind", 2, Type::Enum, false),
            ("safe_param", 3, Type::String, true),
            ("request_error_category", 4, Type::Enum, true),
        ]
    );
    assert_eq!(
        values(&enumeration(
            "dotcore.inference.private.v1.ExactTargetFailureDisposition"
        )),
        [
            ("EXACT_TARGET_FAILURE_DISPOSITION_UNSPECIFIED", 0),
            ("EXACT_TARGET_FAILURE_DISPOSITION_SAFE_TO_FALLBACK", 1),
            ("EXACT_TARGET_FAILURE_DISPOSITION_STARTED_OR_AMBIGUOUS", 2),
            ("EXACT_TARGET_FAILURE_DISPOSITION_NON_RETRYABLE", 3),
        ]
    );
    assert_eq!(
        values(&enumeration(
            "dotcore.inference.private.v1.ExactTargetFailureKind"
        ))
        .last()
        .copied(),
        Some(("EXACT_TARGET_FAILURE_KIND_UNSUPPORTED_REQUEST_KIND", 7))
    );
    assert_eq!(
        values(&enumeration(
            "dotcore.inference.private.v1.RequestErrorCategory"
        )),
        [
            ("REQUEST_ERROR_CATEGORY_UNSPECIFIED", 0),
            ("REQUEST_ERROR_CATEGORY_INVALID_FIELD", 1),
            ("REQUEST_ERROR_CATEGORY_UNSUPPORTED_FEATURE", 2),
            ("REQUEST_ERROR_CATEGORY_REQUEST_LIMIT", 3),
            ("REQUEST_ERROR_CATEGORY_CONTEXT_LENGTH", 4),
        ]
    );
}

#[test]
fn generated_bindings_preserve_control_presence_and_extension_text() {
    // Integers beyond 64 bits and non-canonical spacing must survive as exact text.
    let extensions = r#"{"chat_template_kwargs":{"enable_thinking":false},"seed":18446744073709551616,"top_k":20}"#;
    let request = ChatRequest {
        model_id: "route".to_owned(),
        generation_controls: Some(ChatGenerationControls {
            temperature: Some(0.0),
            reasoning_effort: Some("none".to_owned()),
            presence_penalty: Some(0.0),
            frequency_penalty: None,
            ..Default::default()
        }),
        request_extensions_json: Some(extensions.to_owned()),
        store: Some(false),
        ..Default::default()
    };

    let decoded = ChatRequest::decode(request.encode_to_vec().as_slice()).expect("request");

    assert_eq!(decoded, request);
    assert_eq!(decoded.request_extensions_json.as_deref(), Some(extensions));
    assert_eq!(decoded.store, Some(false));
    let controls = decoded.generation_controls.expect("controls");
    assert_eq!(controls.presence_penalty, Some(0.0));
    assert_eq!(controls.frequency_penalty, None);
    assert_ne!(
        ChatRequest::default().encode_to_vec(),
        ChatRequest {
            store: Some(false),
            ..Default::default()
        }
        .encode_to_vec(),
        "an explicit false store must stay distinguishable from an absent one"
    );
}

#[test]
fn generated_bindings_preserve_peer_request_error_details() {
    let rejected = ExactTargetRejected {
        reason: ExactTargetRejectionReason::InvalidRequest as i32,
        safe_param: Some("chat_template_kwargs".to_owned()),
        request_error_category: Some(RequestErrorCategory::InvalidField as i32),
    };
    let failure = ExactTargetFailure {
        disposition: ExactTargetFailureDisposition::NonRetryable as i32,
        kind: ExactTargetFailureKind::InvalidRequest as i32,
        safe_param: None,
        request_error_category: Some(RequestErrorCategory::RequestLimit as i32),
    };

    assert_eq!(
        ExactTargetRejected::decode(rejected.encode_to_vec().as_slice()).expect("rejection"),
        rejected
    );
    assert_eq!(
        ExactTargetFailure::decode(failure.encode_to_vec().as_slice()).expect("failure"),
        failure
    );
    assert_eq!(
        ExactTargetRejected::decode(
            ExactTargetRejected {
                reason: ExactTargetRejectionReason::BindingUnavailable as i32,
                ..Default::default()
            }
            .encode_to_vec()
            .as_slice()
        )
        .expect("availability rejection")
        .request_error_category,
        None,
        "availability rejections carry no request error"
    );
}

fn message(full_name: &str) -> DescriptorProto {
    let (package, name) = full_name.rsplit_once('.').expect("qualified name");
    descriptors()
        .file
        .into_iter()
        .filter(|file| file.package() == package)
        .flat_map(|file| file.message_type)
        .find(|message| message.name() == name)
        .unwrap_or_else(|| panic!("{full_name} is declared"))
}

fn enumeration(full_name: &str) -> EnumDescriptorProto {
    let (package, name) = full_name.rsplit_once('.').expect("qualified name");
    descriptors()
        .file
        .into_iter()
        .filter(|file| file.package() == package)
        .flat_map(|file| file.enum_type)
        .find(|enumeration| enumeration.name() == name)
        .unwrap_or_else(|| panic!("{full_name} is declared"))
}

fn descriptors() -> FileDescriptorSet {
    FileDescriptorSet::decode(FILE_DESCRIPTOR_SET).expect("descriptor set")
}

fn fields(message: &DescriptorProto) -> Vec<(&str, i32, Type, bool)> {
    message
        .field
        .iter()
        .map(|field| {
            (
                field.name(),
                field.number(),
                field.r#type(),
                field.proto3_optional(),
            )
        })
        .collect()
}

fn values(enumeration: &EnumDescriptorProto) -> Vec<(&str, i32)> {
    enumeration
        .value
        .iter()
        .map(|value| (value.name(), value.number()))
        .collect()
}
