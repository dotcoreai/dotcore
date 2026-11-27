//! A fake site for the guided deploy tests: a small synthetic catalog on two machines, one fixed
//! preview and estimate, and a record of every request the wizard sends.

use std::{
    collections::VecDeque,
    sync::{Mutex, MutexGuard},
};

use dotcore_proto::dotcore::{
    capacity::v1::{
        MemoryPool, MemoryPoolKind, MemoryPoolPreview, MemorySegment, MemorySegmentSource,
        PlacementPreview,
    },
    catalog::v1::{
        BooleanModelOptionParameterSchema, BooleanParameterSchema, EngineParameterSchema, Model,
        ModelConfigurationOption, ModelEngineConfiguration, ModelOption,
        ModelOptionParameterDefinition, ModelProfile, ParameterDefinition, ParameterValue,
        ParameterValues, ParameterVisibility, UnsignedIntegerParameterSchema,
        model_option_parameter_definition, parameter_definition, parameter_value,
    },
    deployment::v1::{ExactPlacement, MemoryFit},
    model::v1::{
        CreateModelDeploymentRequest, EstimateLocalModelResourcesRequest,
        EstimateLocalModelResourcesResponse, HardwareClassResourceEstimate,
        ListModelProfileCompatibilityRequest, ListModelProfileCompatibilityResponse,
        ModelCompatibilityReason, ModelCompatibilityReasonCode, ModelCompatibilityStatus,
        ModelDeployment, ModelProfileCompatibility, PreviewLocalModelDeploymentRequest,
        PreviewLocalModelDeploymentResponse, ResolvedLocalModelDeploymentSnapshot,
        StartingTotalContextSource, TargetCompatibility,
    },
    site::v1::ConfiguredExecutionTarget,
};
use futures_util::future::BoxFuture;
use tonic::Status;

use super::super::site::DeploySite;
use crate::management::NodeNames;

/// A site answering from fixed data and recording what it was asked.
pub(super) struct FakeSite {
    pub(super) models: Vec<Model>,
    pub(super) compatibility: ListModelProfileCompatibilityResponse,
    pub(super) targets: Vec<ConfiguredExecutionTarget>,
    pub(super) preview: PreviewLocalModelDeploymentResponse,
    pub(super) estimate: EstimateLocalModelResourcesResponse,

    /// Failures the next creations return, in order, before any succeeds.
    pub(super) create_failures: Mutex<VecDeque<Status>>,
    requests: Mutex<Requests>,
}

/// The requests a fake site received, in order.
#[derive(Clone, Debug, Default)]
pub(super) struct Requests {
    pub(super) previews: Vec<PreviewLocalModelDeploymentRequest>,
    pub(super) estimates: Vec<EstimateLocalModelResourcesRequest>,
    pub(super) creates: Vec<CreateModelDeploymentRequest>,
}

impl FakeSite {
    /// Three catalog models on two machines: Llama runs nowhere, Gemma fits both. Qwen's balanced profile fits `gpu1` now and needs
    /// memory freed on `gpu2`, its long-context profile runs nowhere, and its preview has every
    /// kind of context field, a basic engine parameter, an advanced one and a basic vision
    /// option.
    pub(super) fn two_machines() -> Self {
        let profiles = |ids: &[&str]| {
            ids.iter()
                .map(|id| ModelProfile {
                    profile_id: (*id).to_owned(),
                    configuration_id: "gguf".to_owned(),
                    ..ModelProfile::default()
                })
                .collect()
        };
        let qwen = Model {
            model_id: "qwen3-8-27b".to_owned(),
            display_name: "Qwen 3.8 27B".to_owned(),
            profiles: profiles(&["balanced", "long-context"]),
            configurations: vec![ModelEngineConfiguration {
                configuration_id: "gguf".to_owned(),
                options: vec![ModelConfigurationOption {
                    option_id: "vision".to_owned(),
                    parameters: vec![ModelOptionParameterDefinition {
                        parameter_id: "enabled".to_owned(),
                        display_name: "enabled".to_owned(),
                        parameter_type: Some(
                            model_option_parameter_definition::ParameterType::Boolean(
                                BooleanModelOptionParameterSchema::default(),
                            ),
                        ),
                    }],
                    ..ModelConfigurationOption::default()
                }],
                ..ModelEngineConfiguration::default()
            }],
            options: vec![ModelOption {
                option_id: "vision".to_owned(),
                display_name: "Vision".to_owned(),
                summary: "Reads images in requests.".to_owned(),
                visibility: ParameterVisibility::Basic.into(),
                ..ModelOption::default()
            }],
            ..Model::default()
        };
        let llama = Model {
            model_id: "llama-4-tiny".to_owned(),
            display_name: "Llama 4 Tiny".to_owned(),
            profiles: profiles(&["default"]),
            ..Model::default()
        };
        let gemma = Model {
            model_id: "gemma-4-e4b".to_owned(),
            display_name: "Gemma 4 E4B".to_owned(),
            profiles: profiles(&["default"]),
            ..Model::default()
        };

        let entry = |model: &str, profile: &str, target: ExactPlacement, fit: MemoryFit| {
            ModelProfileCompatibility {
                model_id: model.to_owned(),
                profile_id: profile.to_owned(),
                target: Some(target),
                status: ModelCompatibilityStatus::Runs.into(),
                memory_fit: MemoryFit::Fits.into(),
                free_memory_fit: fit.into(),
                largest_context_tokens_per_request: Some(49152),
                starting_total_context_tokens: Some(131_072),
                starting_total_context_source: StartingTotalContextSource::PerRequestTimesParallel
                    .into(),
                ..ModelProfileCompatibility::default()
            }
        };
        let cannot_run =
            |model: &str, profile: &str, target: ExactPlacement| ModelProfileCompatibility {
                model_id: model.to_owned(),
                profile_id: profile.to_owned(),
                target: Some(target),
                status: ModelCompatibilityStatus::CannotRun.into(),
                reason: Some(ModelCompatibilityReason {
                    code: ModelCompatibilityReasonCode::NoSupportedHardwareClass.into(),
                    ..ModelCompatibilityReason::default()
                }),
                ..ModelProfileCompatibility::default()
            };

        Self {
            models: vec![llama, qwen, gemma],
            compatibility: ListModelProfileCompatibilityResponse {
                targets: [gpu1(), gpu2()]
                    .into_iter()
                    .map(|target| TargetCompatibility {
                        target: Some(target),
                        status: ModelCompatibilityStatus::Runs.into(),
                        ..TargetCompatibility::default()
                    })
                    .collect(),
                entries: vec![
                    entry("qwen3-8-27b", "balanced", gpu1(), MemoryFit::Fits),
                    entry("qwen3-8-27b", "balanced", gpu2(), MemoryFit::Exceeds),
                    cannot_run("qwen3-8-27b", "long-context", gpu1()),
                    cannot_run("qwen3-8-27b", "long-context", gpu2()),
                    cannot_run("llama-4-tiny", "default", gpu1()),
                    cannot_run("llama-4-tiny", "default", gpu2()),
                    entry("gemma-4-e4b", "default", gpu1(), MemoryFit::Fits),
                    entry("gemma-4-e4b", "default", gpu2(), MemoryFit::Fits),
                ],
                profiles: Vec::new(),
            },
            targets: vec![configured(&gpu1(), "gpu1"), configured(&gpu2(), "gpu2")],
            preview: preview(),
            estimate: estimate(),
            create_failures: Mutex::new(VecDeque::new()),
            requests: Mutex::new(Requests::default()),
        }
    }

    /// The same catalog with only Qwen, on `gpu1` alone, and no basic option.
    pub(super) fn one_machine() -> Self {
        let mut site = Self::two_machines();
        site.models.retain(|model| model.model_id == "qwen3-8-27b");
        site.models[0].options[0].visibility = ParameterVisibility::Advanced.into();
        site.compatibility
            .targets
            .retain(|target| target.target == Some(gpu1()));
        site.compatibility
            .entries
            .retain(|entry| entry.target == Some(gpu1()) && entry.model_id == "qwen3-8-27b");
        site.targets.truncate(1);
        site
    }

    /// Every request received so far.
    pub(super) fn requests(&self) -> Requests {
        self.lock().clone()
    }

    fn lock(&self) -> MutexGuard<'_, Requests> {
        self.requests.lock().expect("fake site requests")
    }
}

impl DeploySite for FakeSite {
    fn list_models(&self) -> BoxFuture<'_, Result<Vec<Model>, Status>> {
        Box::pin(async move { Ok(self.models.clone()) })
    }

    fn compatibility(
        &self,
        request: ListModelProfileCompatibilityRequest,
    ) -> BoxFuture<'_, Result<ListModelProfileCompatibilityResponse, Status>> {
        Box::pin(async move {
            let mut response = self.compatibility.clone();
            if let Some(model_id) = request.model_id {
                response.entries.retain(|entry| entry.model_id == model_id);
            }
            Ok(response)
        })
    }

    fn node_names(&self) -> BoxFuture<'_, NodeNames> {
        Box::pin(async move { NodeNames::of_targets(&self.targets) })
    }

    fn preview(
        &self,
        request: PreviewLocalModelDeploymentRequest,
    ) -> BoxFuture<'_, Result<PreviewLocalModelDeploymentResponse, Status>> {
        self.lock().previews.push(request);
        Box::pin(async move { Ok(self.preview.clone()) })
    }

    fn estimate(
        &self,
        request: EstimateLocalModelResourcesRequest,
    ) -> BoxFuture<'_, Result<EstimateLocalModelResourcesResponse, Status>> {
        self.lock().estimates.push(request);
        Box::pin(async move { Ok(self.estimate.clone()) })
    }

    fn create(
        &self,
        request: CreateModelDeploymentRequest,
    ) -> BoxFuture<'_, Result<ModelDeployment, Status>> {
        let name = request.model_deployment_name.clone().unwrap_or_default();
        self.lock().creates.push(request);
        let failure = self
            .create_failures
            .lock()
            .expect("fake site failures")
            .pop_front();
        Box::pin(async move {
            match failure {
                Some(status) => Err(status),
                None => Ok(ModelDeployment {
                    model_deployment_id: "created-1".to_owned(),
                    model_deployment_name: name,
                    ..ModelDeployment::default()
                }),
            }
        })
    }
}

/// The first machine's model target.
pub(super) fn gpu1() -> ExactPlacement {
    placement("node-1", "target-1")
}

/// The second machine's model target.
pub(super) fn gpu2() -> ExactPlacement {
    placement("node-2", "target-2")
}

/// An engine parameter value.
pub(super) fn value(id: &str, value: parameter_value::Value) -> ParameterValue {
    ParameterValue {
        parameter_id: id.to_owned(),
        value: Some(value),
    }
}

fn placement(node: &str, target: &str) -> ExactPlacement {
    ExactPlacement {
        site_node_id: node.to_owned(),
        execution_target_id: target.to_owned(),
    }
}

fn configured(placement: &ExactPlacement, node: &str) -> ConfiguredExecutionTarget {
    ConfiguredExecutionTarget {
        execution_target_id: placement.execution_target_id.clone(),
        display_name: "CUDA".to_owned(),
        site_node_id: placement.site_node_id.clone(),
        site_node_display_name: node.to_owned(),
        ..ConfiguredExecutionTarget::default()
    }
}

/// The balanced profile's preview: context fields, a basic batch size and an advanced flash
/// attention, starting from 32768 tokens per request and 4 parallel requests.
fn preview() -> PreviewLocalModelDeploymentResponse {
    let definition =
        |id: &str, name: &str, visibility: ParameterVisibility, kind| ParameterDefinition {
            parameter_id: id.to_owned(),
            display_name: name.to_owned(),
            summary: format!("What {} does.", name.to_lowercase()),
            visibility: visibility.into(),
            parameter_type: Some(kind),
            ..ParameterDefinition::default()
        };
    let integer = |minimum, maximum, unit: &str| {
        parameter_definition::ParameterType::UnsignedInteger(UnsignedIntegerParameterSchema {
            minimum,
            maximum,
            unit: unit.to_owned(),
            ..UnsignedIntegerParameterSchema::default()
        })
    };
    let basic = ParameterVisibility::Basic;
    PreviewLocalModelDeploymentResponse {
        resolved_snapshot: Some(ResolvedLocalModelDeploymentSnapshot {
            model_id: "qwen3-8-27b".to_owned(),
            configuration_id: "gguf".to_owned(),
            profile_id: "balanced".to_owned(),
            engine_parameter_schema: Some(EngineParameterSchema {
                parameters: vec![
                    definition(
                        "context-per-request",
                        "Context per request",
                        basic,
                        integer(4096, Some(262_144), "tokens"),
                    ),
                    definition(
                        "parallel-requests",
                        "Parallel requests",
                        basic,
                        integer(1, Some(64), ""),
                    ),
                    definition(
                        "total-context",
                        "Total context",
                        basic,
                        integer(4096, None, "tokens"),
                    ),
                    definition(
                        "batch-size",
                        "Batch size",
                        basic,
                        integer(1, Some(8192), "tokens"),
                    ),
                    definition(
                        "flash-attention",
                        "Flash attention",
                        ParameterVisibility::Advanced,
                        parameter_definition::ParameterType::Boolean(
                            BooleanParameterSchema::default(),
                        ),
                    ),
                ],
                ..EngineParameterSchema::default()
            }),
            engine_parameter_values: Some(ParameterValues {
                values: vec![
                    value(
                        "context-per-request",
                        parameter_value::Value::UnsignedInteger(32768),
                    ),
                    value(
                        "parallel-requests",
                        parameter_value::Value::UnsignedInteger(4),
                    ),
                    value("batch-size", parameter_value::Value::UnsignedInteger(512)),
                    value("flash-attention", parameter_value::Value::Boolean(true)),
                ],
            }),
            ..ResolvedLocalModelDeploymentSnapshot::default()
        }),
        advisories: Vec::new(),
    }
}

/// An estimate that fits one 24 GiB GPU with 6 GiB in use and 10 GiB added, deriving a total
/// context of 131072.
fn estimate() -> EstimateLocalModelResourcesResponse {
    const GIB: u64 = 1 << 30;

    let segment = |source: MemorySegmentSource, bytes| MemorySegment {
        source: source.into(),
        bytes,
        ..MemorySegment::default()
    };
    EstimateLocalModelResourcesResponse {
        hardware_class_estimates: vec![HardwareClassResourceEstimate {
            derived_engine_parameter_values: Some(ParameterValues {
                values: vec![value(
                    "total-context",
                    parameter_value::Value::UnsignedInteger(131_072),
                )],
            }),
            ..HardwareClassResourceEstimate::default()
        }],
        placement_preview: Some(PlacementPreview {
            pools: vec![MemoryPoolPreview {
                pool: Some(MemoryPool {
                    kind: MemoryPoolKind::Device.into(),
                    device_index: Some(0),
                    total_bytes: Some(24 * GIB),
                    segments: vec![
                        segment(MemorySegmentSource::Other, 6 * GIB),
                        segment(MemorySegmentSource::Previewed, 10 * GIB),
                    ],
                    ..MemoryPool::default()
                }),
                fit_line_bytes: Some(22 * GIB),
                free_memory_fit: MemoryFit::Fits.into(),
                ..MemoryPoolPreview::default()
            }],
            free_memory_fit: MemoryFit::Fits.into(),
            ..PlacementPreview::default()
        }),
        largest_context_tokens_per_request: Some(49152),
        ..EstimateLocalModelResourcesResponse::default()
    }
}
