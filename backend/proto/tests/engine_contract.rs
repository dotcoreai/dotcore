//! Engine-boundary protobuf descriptor contract tests.
//!
//! These tests keep catalog and deployment messages structurally engine-neutral
//! while engine-owned packages retain concrete engine configurations.

use dotcore_proto::FILE_DESCRIPTOR_SET;
use prost::Message;
use prost_types::{FileDescriptorProto, FileDescriptorSet};

#[test]
fn descriptors_keep_engine_payloads_out_of_catalog_and_deployment_contracts() {
    let descriptor = FileDescriptorSet::decode(FILE_DESCRIPTOR_SET)
        .expect("the generated descriptor set must decode");
    let engine = descriptor_file(&descriptor, "dotcore/engine/v1/engine.proto");
    let llama_cpp = descriptor_file(&descriptor, "dotcore/engine/llama_cpp/v1/model.proto");
    let vllm = descriptor_file(&descriptor, "dotcore/engine/vllm/v1/model.proto");
    let ninfer = descriptor_file(&descriptor, "dotcore/engine/ninfer/v1/model.proto");
    let strata = descriptor_file(&descriptor, "dotcore/engine/strata/v1/model.proto");
    let catalog = descriptor_file(&descriptor, "dotcore/catalog/v1/model.proto");
    let deployment = descriptor_file(&descriptor, "dotcore/model/v1/deployment.proto");

    let inference = descriptor_file(&descriptor, "dotcore/inference/v1/chat.proto");
    assert_eq!(
        field_layout(engine, "EngineModelConfiguration"),
        [
            ("engine_kind", 1, ".dotcore.engine.v1.EngineKind"),
            (
                "llama_cpp",
                2,
                ".dotcore.engine.llama_cpp.v1.LlamaCppModelConfiguration",
            ),
            ("vllm", 3, ".dotcore.engine.vllm.v1.VllmModelConfiguration",),
            (
                "ninfer",
                4,
                ".dotcore.engine.ninfer.v1.NinferModelConfiguration",
            ),
            (
                "strata",
                5,
                ".dotcore.engine.strata.v1.StrataModelConfiguration",
            ),
        ]
    );
    assert_eq!(
        field_layout(engine, "ResolvedEngineModelState"),
        [
            ("engine_kind", 1, ".dotcore.engine.v1.EngineKind"),
            ("semantics_version", 2, "TYPE_UINT32"),
            (
                "llama_cpp",
                3,
                ".dotcore.engine.llama_cpp.v1.LlamaCppResolvedModelState",
            ),
            ("vllm", 4, ".dotcore.engine.vllm.v1.VllmResolvedModelState",),
            (
                "ninfer",
                5,
                ".dotcore.engine.ninfer.v1.NinferResolvedModelState",
            ),
            (
                "strata",
                6,
                ".dotcore.engine.strata.v1.StrataResolvedModelState",
            ),
        ]
    );
    assert_eq!(
        field_numbers(llama_cpp, "LlamaCppModelConfiguration"),
        [("fixed_settings", 1), ("option_realizations", 2)]
    );
    assert_eq!(
        field_numbers(llama_cpp, "LlamaCppResolvedModelState"),
        [("fixed_settings", 1), ("option_realizations", 2)]
    );
    assert_eq!(
        field_numbers(vllm, "VllmModelConfiguration"),
        [("fixed_settings", 1), ("option_realizations", 2)]
    );
    assert_eq!(
        field_numbers(vllm, "VllmResolvedModelState"),
        [("fixed_settings", 1), ("option_realizations", 2)]
    );
    assert_eq!(
        field_numbers(vllm, "VllmFixedModelSettings"),
        [
            ("reasoning_parser", 2),
            ("tool_call_parser", 3),
            ("chat_template_auxiliary_artifact_id", 4),
            ("kv_cache_data_type", 5),
            ("kv_cache_memory_bytes_parameter_id", 6),
            ("maximum_batched_tokens_parameter_id", 8),
            ("maximum_cuda_graph_capture_size_parameter_id", 9),
            ("kv_offloading_size_gib_parameter_id", 10),
            ("kv_offloading_backend", 11),
            ("container_port", 12),
            ("readiness_policy", 13),
            ("requires_host_ipc", 14),
            ("trust_remote_code", 15),
            ("enable_log_requests", 16),
            ("enable_auto_tool_choice", 17),
            ("enable_chunked_prefill", 18),
            ("enable_async_scheduling", 19),
            ("enable_prefix_caching", 20),
            ("enable_prompt_token_details", 21),
        ]
    );
    assert_eq!(
        field_numbers(ninfer, "NinferModelConfiguration"),
        [("fixed_settings", 1), ("option_realizations", 2)]
    );
    assert_eq!(
        field_numbers(ninfer, "NinferResolvedModelState"),
        [("fixed_settings", 1), ("option_realizations", 2)]
    );
    assert_eq!(
        field_layout(ninfer, "NinferFixedModelSettings"),
        [
            (
                "kv_cache_data_type",
                1,
                ".dotcore.engine.ninfer.v1.NinferKvCacheDataType",
            ),
            ("pending_timeout_milliseconds", 2, "TYPE_UINT32"),
            ("preserve_thinking", 3, "TYPE_BOOL"),
            ("host_context_mib_parameter_id", 7, "TYPE_STRING"),
            ("state_image_bytes", 8, "TYPE_UINT64"),
            ("kv_bytes_per_token", 9, "TYPE_UINT64"),
            ("draft_kv_bytes_per_token", 10, "TYPE_UINT64"),
        ]
    );
    assert_eq!(
        field_layout(ninfer, "NinferOptionRealization"),
        [
            ("option_id", 1, "TYPE_STRING"),
            (
                "vision",
                2,
                ".dotcore.engine.ninfer.v1.NinferVisionOptionRealization",
            ),
            (
                "mtp",
                3,
                ".dotcore.engine.ninfer.v1.NinferMtpOptionRealization",
            ),
        ]
    );
    assert_eq!(
        field_numbers(ninfer, "NinferVisionOptionRealization"),
        [("enabled_parameter_id", 1)]
    );
    assert_eq!(
        field_numbers(ninfer, "NinferMtpOptionRealization"),
        [("optimized_proposal_head", 3)]
    );
    assert_eq!(
        field_numbers(strata, "StrataModelConfiguration"),
        [("fixed_settings", 1)]
    );
    assert_eq!(
        field_numbers(strata, "StrataResolvedModelState"),
        [("fixed_settings", 1)]
    );
    assert_eq!(
        field_layout(strata, "StrataFixedModelSettings"),
        [
            ("expert_vram_mib_parameter_id", 1, "TYPE_STRING"),
            ("architecture", 2, "TYPE_STRING"),
            ("mtp_draft_tokens", 3, "TYPE_UINT32"),
            (
                "single_gguf",
                4,
                ".dotcore.engine.strata.v1.StrataSingleGguf",
            ),
            ("split_gguf", 5, ".dotcore.engine.strata.v1.StrataSplitGguf"),
            ("ready_pack", 6, ".dotcore.engine.strata.v1.StrataReadyPack"),
        ]
    );
    assert_eq!(
        field_numbers(strata, "StrataSplitGguf"),
        [("draft_layer_auxiliary_artifact_id", 1)]
    );
    assert_eq!(
        field_numbers(strata, "StrataReadyPack"),
        [
            ("draft_layer_auxiliary_artifact_id", 1),
            ("pack_path", 2),
            ("dense_gguf_path", 3),
            ("lookup_table_gguf_path", 4),
            ("token_embedding_gguf_path", 5),
        ]
    );

    assert_eq!(
        field_layout(catalog, "ModelArtifact"),
        [
            ("source", 1, ".dotcore.catalog.v1.HuggingFaceArtifactSource",),
            ("artifact_id", 2, "TYPE_STRING"),
            ("weight_quantization", 3, "TYPE_STRING"),
        ]
    );
    assert_eq!(
        field_layout(catalog, "ModelAuxiliaryArtifact"),
        [
            ("auxiliary_artifact_id", 1, "TYPE_STRING"),
            ("source", 2, ".dotcore.catalog.v1.HuggingFaceArtifactSource",),
        ]
    );
    assert_eq!(
        field_numbers(catalog, "HuggingFaceArtifactSource"),
        [
            ("repository", 1),
            ("revision", 2),
            ("file", 3),
            ("snapshot", 4),
        ]
    );
    assert_eq!(
        field_numbers(catalog, "HuggingFaceFileArtifact"),
        [("path", 1), ("provenance_digest", 2)]
    );
    assert_eq!(
        field_numbers(catalog, "HuggingFaceSnapshotArtifact"),
        [("files", 1)]
    );
    assert_eq!(
        field_numbers(catalog, "HuggingFaceSnapshotFile"),
        [("path", 1), ("provenance_digest", 2)]
    );
    assert_eq!(
        field_numbers(catalog, "ModelEngineConfiguration"),
        [
            ("configuration_id", 1),
            ("engine_artifacts", 2),
            ("artifact_bindings", 3),
            ("hardware_support", 9),
            ("engine_configuration", 5),
            ("execution_capabilities", 6),
            ("input_capabilities", 7),
            ("request_kinds", 11),
            ("options", 8),
            ("maximum_context_tokens", 10),
        ]
    );
    assert_eq!(
        field_numbers(catalog, "ModelConfigurationOption"),
        [
            ("option_id", 1),
            ("parameters", 2),
            ("default_values", 3),
            ("supported_binding_ids", 4),
            ("compatible_when", 5),
        ]
    );
    assert!(
        catalog.message_type.iter().all(|message| {
            let name = message.name.as_deref().unwrap_or_default();
            !name.contains("LlamaCpp")
                && !name.contains("Vllm")
                && !name.contains("Ninfer")
                && !name.contains("Strata")
        }),
        "Catalog messages must not define engine-owned payloads"
    );

    assert_eq!(
        field_numbers(deployment, "ResolvedLocalModelDeploymentSnapshot"),
        [
            ("model_id", 1),
            ("model_display_name", 2),
            ("model_version", 3),
            ("configuration_id", 4),
            ("profile_id", 5),
            ("engine_artifact", 6),
            ("model_artifact", 7),
            ("auxiliary_artifacts", 8),
            ("hardware_support", 21),
            ("resolved_engine_state", 10),
            ("execution_capabilities", 11),
            ("input_capabilities", 12),
            ("request_kinds", 22),
            ("engine_parameter_values", 13),
            ("engine_parameter_schema_id", 14),
            ("engine_parameter_schema", 15),
            ("profile_engine_parameter_defaults", 23),
            ("profile_engine_parameter_visibility_overrides", 24),
            ("generated_engine_parameter_ids", 25),
            ("desired_launch_plan", 16),
            ("custom_argument_tokens", 17),
            ("resolved_options", 18),
            ("selected_target", 19),
            ("sizing", 20),
        ]
    );
    assert_eq!(
        field_layout(deployment, "ResolvedModelOption"),
        [("values", 1, ".dotcore.catalog.v1.ModelOptionValues")]
    );
    assert_eq!(
        field_numbers(inference, "ChatMessage"),
        [
            ("role", 1),
            ("text_content", 2),
            ("null_content", 3),
            ("tool_result", 4),
            ("tool_calls", 5),
            ("name", 6),
            ("content_parts", 7),
            ("reasoning", 8),
        ]
    );
    assert_eq!(
        field_numbers(inference, "ChatStreamEvent"),
        [
            ("started", 1),
            ("text_delta", 2),
            ("tool_call_delta", 3),
            ("finished", 4),
            ("usage", 5),
            ("completed", 6),
            ("reasoning_delta", 7),
        ]
    );
    assert_eq!(
        field_numbers(inference, "ChatReasoningDelta"),
        [("choice_index", 1), ("reasoning", 2)]
    );

    assert!(
        deployment.message_type.iter().all(|message| !message
            .name
            .as_deref()
            .unwrap_or_default()
            .contains("Translator")),
        "deployment messages must not retain obsolete translator identity"
    );
}

fn descriptor_file<'a>(descriptor: &'a FileDescriptorSet, name: &str) -> &'a FileDescriptorProto {
    descriptor
        .file
        .iter()
        .find(|file| file.name.as_deref() == Some(name))
        .unwrap_or_else(|| panic!("descriptor set must contain {name}"))
}

fn field_numbers<'a>(file: &'a FileDescriptorProto, message: &str) -> Vec<(&'a str, i32)> {
    file.message_type
        .iter()
        .find(|descriptor| descriptor.name.as_deref() == Some(message))
        .unwrap_or_else(|| panic!("descriptor file must contain {message}"))
        .field
        .iter()
        .map(|field| {
            (
                field.name.as_deref().expect("field name"),
                field.number.expect("field number"),
            )
        })
        .collect()
}

fn field_layout<'a>(file: &'a FileDescriptorProto, message: &str) -> Vec<(&'a str, i32, &'a str)> {
    file.message_type
        .iter()
        .find(|descriptor| descriptor.name.as_deref() == Some(message))
        .unwrap_or_else(|| panic!("descriptor file must contain {message}"))
        .field
        .iter()
        .map(|field| {
            let field_type = field.type_name.as_deref().unwrap_or_else(|| {
                field
                    .r#type
                    .and_then(|value| {
                        prost_types::field_descriptor_proto::Type::try_from(value).ok()
                    })
                    .map(|value| value.as_str_name())
                    .unwrap_or("TYPE_UNKNOWN")
            });
            (
                field.name.as_deref().expect("field name"),
                field.number.expect("field number"),
                field_type,
            )
        })
        .collect()
}
