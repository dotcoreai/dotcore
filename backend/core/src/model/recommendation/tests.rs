use std::time::SystemTime;

use dotcore_catalog::{Catalog, OciArtifactPolicy};
use dotcore_node_hardware::{MachineDescription, MachineWorkload};
use dotcore_proto::dotcore::{
    catalog::v1::UseCase,
    deployment::v1::ExactPlacement,
    model::v1::{
        ModelDeployment, ModelDeploymentIntent, ModelDeploymentSnapshot, ModelDeploymentState,
        ModelDeploymentStatus, ResolvedLocalModelDeploymentSnapshot, ResolvedSelectedTarget,
        ResolvedSizing, model_deployment_snapshot,
    },
    observation::v1::AcceleratorMemoryArchitecture,
    site::v1::{
        ExecutionTarget, ModelRuntimeBinding, execution_target::RuntimeBinding,
        model_runtime_binding::Adapter,
    },
};
use dotcore_site::node::target_inventory::{
    NodeAcceleratorObservation, RuntimePlatformObservation, TargetCapabilityReconciler,
};

use super::{RecommendationFit, UseCaseRecommendations, recommend_models};

use RecommendationFit::{FitsNow, NeedsMemoryFreed, Unknown};
use UseCase::{CodingAndAgents, DecisionMaking, GeneralAssistant, Translation};

/// Expected recommendations for one site of declared machines under one occupancy and use case.
struct Case {
    machines: &'static [&'static str],
    occupancy: Occupancy,
    use_case: UseCase,
    rows: Vec<Row>,
}

/// What runs on the site's machines when recommendations are read.
#[derive(Clone, Copy, Debug)]
enum Occupancy {
    Idle,

    /// One ready deployment of the model and profile on the site's first machine, using its
    /// estimate.
    Running(&'static str, &'static str),

    /// One deployment of unknown size still downloading on the site's first machine.
    StartingUnsized,

    /// The site's machines last reported long ago.
    Stale,
}

/// One expected row: `model/profile`, then each target with its fit, whether the profile is
/// deployed there, and the total context a deployment starts from.
type Row = (
    &'static str,
    Vec<(&'static str, RecommendationFit, bool, u64)>,
);

#[test]
fn declared_machines_get_exact_ranked_recommendations_per_use_case_and_occupancy() {
    let cases = [
        // 8 GiB GPU: the GPU-model entry is unsupported and the entries above its memory or their
        // minimum are dropped; translation and decision making fall back to the vendor class.
        Case {
            machines: &["s8"],
            occupancy: Occupancy::Idle,
            use_case: CodingAndAgents,
            rows: vec![("small/q4", vec![("s8", FitsNow, false, 59_392)])],
        },
        Case {
            machines: &["s8"],
            occupancy: Occupancy::Idle,
            use_case: Translation,
            rows: vec![("small/q4", vec![("s8", FitsNow, false, 59_392)])],
        },
        Case {
            machines: &["s8"],
            occupancy: Occupancy::Idle,
            use_case: DecisionMaking,
            rows: vec![("decider/q8", vec![("s8", FitsNow, false, 69_632)])],
        },
        // 32 GiB GPU of the model the GPU-model class names: its only configuration comes first.
        Case {
            machines: &["x32"],
            occupancy: Occupancy::Idle,
            use_case: CodingAndAgents,
            rows: vec![
                ("x-only/x", vec![("x32", FitsNow, false, 293_888)]),
                ("big/q4", vec![("x32", FitsNow, false, 174_080)]),
                ("mid/q4", vec![("x32", FitsNow, false, 274_432)]),
                ("small/q4", vec![("x32", FitsNow, false, 314_368)]),
            ],
        },
        Case {
            machines: &["x32"],
            occupancy: Occupancy::Idle,
            use_case: Translation,
            rows: vec![
                ("mid/q4", vec![("x32", FitsNow, false, 274_432)]),
                ("small/q4", vec![("x32", FitsNow, false, 314_368)]),
            ],
        },
        Case {
            machines: &["x32"],
            occupancy: Occupancy::Idle,
            use_case: DecisionMaking,
            rows: vec![("decider/q8", vec![("x32", FitsNow, false, 324_608)])],
        },
        // The running model is deployed and a second one needs its memory freed; it starts from
        // the profile default without room for a largest fit.
        Case {
            machines: &["x32"],
            occupancy: Occupancy::Running("big", "q4"),
            use_case: CodingAndAgents,
            rows: vec![
                ("x-only/x", vec![("x32", FitsNow, false, 101_376)]),
                ("big/q4", vec![("x32", NeedsMemoryFreed, true, 32_768)]),
                ("mid/q4", vec![("x32", FitsNow, false, 81_920)]),
                ("small/q4", vec![("x32", FitsNow, false, 121_856)]),
            ],
        },
        Case {
            machines: &["x32"],
            occupancy: Occupancy::Running("big", "q4"),
            use_case: Translation,
            rows: vec![
                ("mid/q4", vec![("x32", FitsNow, false, 81_920)]),
                ("small/q4", vec![("x32", FitsNow, false, 121_856)]),
            ],
        },
        // Macs: entry minimums on system memory separate 16, 32 and 64 GiB.
        Case {
            machines: &["mac16"],
            occupancy: Occupancy::Idle,
            use_case: CodingAndAgents,
            rows: vec![("small/q4", vec![("mac16", FitsNow, false, 106_496)])],
        },
        Case {
            machines: &["mac16"],
            occupancy: Occupancy::Idle,
            use_case: Translation,
            rows: vec![("small/q4", vec![("mac16", FitsNow, false, 106_496)])],
        },
        Case {
            machines: &["mac16"],
            occupancy: Occupancy::Idle,
            use_case: DecisionMaking,
            rows: vec![("decider/q8", vec![("mac16", FitsNow, false, 116_736)])],
        },
        Case {
            machines: &["mac32"],
            occupancy: Occupancy::Idle,
            use_case: CodingAndAgents,
            rows: vec![
                ("mid/q4", vec![("mac32", FitsNow, false, 215_040)]),
                ("small/q4", vec![("mac32", FitsNow, false, 254_976)]),
            ],
        },
        Case {
            machines: &["mac32"],
            occupancy: Occupancy::Idle,
            use_case: Translation,
            rows: vec![
                ("mid/q4", vec![("mac32", FitsNow, false, 215_040)]),
                ("small/q4", vec![("mac32", FitsNow, false, 254_976)]),
            ],
        },
        Case {
            machines: &["mac64"],
            occupancy: Occupancy::Idle,
            use_case: CodingAndAgents,
            rows: vec![
                ("big/q4", vec![("mac64", FitsNow, false, 434_176)]),
                ("mid/q4", vec![("mac64", FitsNow, false, 533_504)]),
                ("small/q4", vec![("mac64", FitsNow, false, 573_440)]),
            ],
        },
        Case {
            machines: &["mac64"],
            occupancy: Occupancy::Running("mid", "q4"),
            use_case: CodingAndAgents,
            rows: vec![
                ("big/q4", vec![("mac64", FitsNow, false, 340_992)]),
                ("mid/q4", vec![("mac64", FitsNow, true, 441_344)]),
                ("small/q4", vec![("mac64", FitsNow, false, 481_280)]),
            ],
        },
        Case {
            machines: &["mac64"],
            occupancy: Occupancy::Idle,
            use_case: DecisionMaking,
            rows: vec![("decider/q8", vec![("mac64", FitsNow, false, 583_680)])],
        },
        // Unified 128 GiB: its own coding list, the vendor class's translation list.
        Case {
            machines: &["uni128"],
            occupancy: Occupancy::Idle,
            use_case: CodingAndAgents,
            rows: vec![
                ("big/q4", vec![("uni128", FitsNow, false, 1_114_112)]),
                ("mid/q4", vec![("uni128", FitsNow, false, 1_214_464)]),
            ],
        },
        Case {
            machines: &["uni128"],
            occupancy: Occupancy::Idle,
            use_case: Translation,
            rows: vec![
                ("mid/q4", vec![("uni128", FitsNow, false, 1_214_464)]),
                ("small/q4", vec![("uni128", FitsNow, false, 1_254_400)]),
            ],
        },
        Case {
            machines: &["uni128"],
            occupancy: Occupancy::Idle,
            use_case: DecisionMaking,
            rows: vec![("decider/q8", vec![("uni128", FitsNow, false, 1_264_640)])],
        },
        // A deployment of unknown size still starting leaves free memory unknown, while the
        // total memory still decides what is kept.
        Case {
            machines: &["uni128"],
            occupancy: Occupancy::StartingUnsized,
            use_case: CodingAndAgents,
            rows: vec![
                ("big/q4", vec![("uni128", Unknown, false, 32_768)]),
                ("mid/q4", vec![("uni128", Unknown, false, 32_768)]),
            ],
        },
        // Two machines: rows merge by their best position in either list, then the higher
        // intelligence score, and name every target that runs them.
        Case {
            machines: &["mac64", "x32"],
            occupancy: Occupancy::Idle,
            use_case: CodingAndAgents,
            rows: vec![
                ("x-only/x", vec![("x32", FitsNow, false, 293_888)]),
                (
                    "big/q4",
                    vec![
                        ("mac64", FitsNow, false, 434_176),
                        ("x32", FitsNow, false, 174_080),
                    ],
                ),
                (
                    "mid/q4",
                    vec![
                        ("mac64", FitsNow, false, 533_504),
                        ("x32", FitsNow, false, 274_432),
                    ],
                ),
                (
                    "small/q4",
                    vec![
                        ("mac64", FitsNow, false, 573_440),
                        ("x32", FitsNow, false, 314_368),
                    ],
                ),
            ],
        },
        Case {
            machines: &["mac64", "x32"],
            occupancy: Occupancy::Idle,
            use_case: Translation,
            rows: vec![
                (
                    "mid/q4",
                    vec![
                        ("mac64", FitsNow, false, 533_504),
                        ("x32", FitsNow, false, 274_432),
                    ],
                ),
                (
                    "small/q4",
                    vec![
                        ("mac64", FitsNow, false, 573_440),
                        ("x32", FitsNow, false, 314_368),
                    ],
                ),
            ],
        },
        // No class has a list for the general assistant.
        Case {
            machines: &["mac64", "x32"],
            occupancy: Occupancy::Idle,
            use_case: GeneralAssistant,
            rows: vec![],
        },
        // A target whose facts are stale can't be matched to a class, so it gets no rows.
        Case {
            machines: &["x32"],
            occupancy: Occupancy::Stale,
            use_case: CodingAndAgents,
            rows: vec![],
        },
    ];

    let catalog = catalog();
    for case in cases {
        let label = format!(
            "{} {:?} {}",
            case.machines.join("+"),
            case.occupancy,
            case.use_case.as_str_name()
        );
        let (targets, inventory, deployments) = site(case.machines, case.occupancy);

        let recommendations = recommend_models(
            &catalog,
            &targets,
            Some(&inventory),
            &deployments,
            &[case.use_case],
            SystemTime::now(),
        );

        assert_eq!(
            recommendations
                .iter()
                .map(|list| list.use_case)
                .collect::<Vec<_>>(),
            [case.use_case],
            "{label}"
        );
        assert_eq!(rows(&recommendations[0]), owned(case.rows), "{label}");
    }
}

#[test]
fn every_use_case_asked_gets_its_own_list_in_the_order_asked() {
    let catalog = catalog();
    let (targets, inventory, deployments) = site(&["s8"], Occupancy::Idle);

    let recommendations = recommend_models(
        &catalog,
        &targets,
        Some(&inventory),
        &deployments,
        &[DecisionMaking, GeneralAssistant, CodingAndAgents],
        SystemTime::now(),
    );

    assert_eq!(
        recommendations.iter().map(rows).collect::<Vec<_>>(),
        [
            owned(vec![("decider/q8", vec![("s8", FitsNow, false, 69_632)])]),
            Vec::new(),
            owned(vec![("small/q4", vec![("s8", FitsNow, false, 59_392)])]),
        ]
    );

    // Without an inventory every target is unknown and nothing is recommended.
    assert_eq!(
        recommend_models(
            &catalog,
            &targets,
            None,
            &deployments,
            &[CodingAndAgents],
            SystemTime::now(),
        )
        .iter()
        .map(rows)
        .collect::<Vec<_>>(),
        [Vec::<OwnedRow>::new()]
    );
}

/// The rows of one use case's recommendations, in the form `Row` describes.
fn rows(recommendations: &UseCaseRecommendations) -> Vec<OwnedRow> {
    recommendations
        .rows
        .iter()
        .map(|row| {
            let targets = row
                .targets
                .iter()
                .map(|target| {
                    (
                        target.placement.execution_target_id.clone(),
                        target.fit,
                        target.deployed,
                        target.starting_total_context_tokens,
                    )
                })
                .collect();
            (format!("{}/{}", row.model_id, row.profile_id), targets)
        })
        .collect()
}

/// Expected rows in the form `rows` reads them.
fn owned(rows: Vec<Row>) -> Vec<OwnedRow> {
    rows.into_iter()
        .map(|(name, targets)| {
            let targets = targets
                .into_iter()
                .map(|(target, fit, deployed, starting)| {
                    (target.to_owned(), fit, deployed, Some(starting))
                })
                .collect();
            (name.to_owned(), targets)
        })
        .collect()
}

type OwnedRow = (String, Vec<(String, RecommendationFit, bool, Option<u64>)>);

/// One target per machine, named after it, with the inventory the machines report under
/// `occupancy` and the site's deployments.
fn site(
    machines: &[&str],
    occupancy: Occupancy,
) -> (
    Vec<ExecutionTarget>,
    dotcore_site::node::target_inventory::TargetCapabilityInventory,
    Vec<ModelDeployment>,
) {
    let now = SystemTime::now();
    let described = machines
        .iter()
        .map(|&machine| MachineDescription::from_yaml(machine, machine_yaml(machine)))
        .collect::<Result<Vec<_>, _>>()
        .expect("declared machines");
    let targets = described
        .iter()
        .map(|description| target(description.name(), description))
        .collect::<Vec<_>>();

    let first = &targets[0];
    let first_gpu = described[0].gpu_ids().next().expect("a GPU").to_owned();
    let architecture = described[0].accelerator_facts()[0].memory_architecture();
    let (deployments, workloads) = match occupancy {
        Occupancy::Idle | Occupancy::Stale => (Vec::new(), Vec::new()),
        Occupancy::Running(model, profile) => {
            let bytes = default_estimate(model);
            (
                vec![deployment(
                    first,
                    architecture,
                    (model, profile),
                    ModelDeploymentStatus::Ready,
                    Some(bytes),
                )],
                vec![MachineWorkload {
                    gpu_bytes: [(first_gpu, bytes)].into(),
                    host_bytes: 0,
                }],
            )
        }
        Occupancy::StartingUnsized => (
            vec![deployment(
                first,
                architecture,
                ("small", "q4"),
                ModelDeploymentStatus::Downloading,
                None,
            )],
            Vec::new(),
        ),
    };
    let stale = matches!(occupancy, Occupancy::Stale);
    let reported_at = if stale {
        now - 2 * dotcore_site::node::target_inventory::TARGET_CAPABILITY_MAX_AGE
    } else {
        now
    };

    let accelerators = described
        .iter()
        .enumerate()
        .map(|(index, description)| {
            let running = if index == 0 { &workloads[..] } else { &[] };
            let snapshot = description
                .render(running, reported_at)
                .expect("rendered snapshot");
            NodeAcceleratorObservation::from_hardware_snapshot(description.name(), &snapshot, stale)
        })
        .collect::<Vec<_>>();
    let platforms = described
        .iter()
        .map(|description| {
            RuntimePlatformObservation::available_at(
                description.name(),
                description.platform().clone(),
                reported_at,
            )
        })
        .collect::<Vec<_>>();
    let inventory = TargetCapabilityReconciler::reconcile(&targets, platforms, accelerators);
    (targets, inventory, deployments)
}

/// The model execution target of a machine's first GPU: a native target on a Mac, a Docker one
/// elsewhere.
fn target(name: &str, description: &MachineDescription) -> ExecutionTarget {
    let platform = description.platform();
    let adapter = if platform.is_linux() {
        Adapter::DockerRuntime(Default::default())
    } else {
        Adapter::NativeRuntime(Default::default())
    };
    ExecutionTarget {
        execution_target_id: name.to_owned(),
        display_name: name.to_owned(),
        site_node_id: name.to_owned(),
        platform: Some(platform.to_oci_platform()),
        accelerator_device_ids: description.gpu_ids().take(1).map(str::to_owned).collect(),
        runtime_binding: Some(RuntimeBinding::ModelRuntime(ModelRuntimeBinding {
            adapter: Some(adapter),
        })),
    }
}

/// A local deployment of a model and profile on `target`, whose devices have `architecture`
/// memory, sized at `bytes` of accelerator memory when known.
fn deployment(
    target: &ExecutionTarget,
    architecture: AcceleratorMemoryArchitecture,
    (model_id, profile_id): (&str, &str),
    status: ModelDeploymentStatus,
    bytes: Option<u64>,
) -> ModelDeployment {
    ModelDeployment {
        model_deployment_id: format!("{model_id}-{profile_id}"),
        intent: Some(ModelDeploymentIntent {
            placement: Some(ExactPlacement {
                site_node_id: target.site_node_id.clone(),
                execution_target_id: target.execution_target_id.clone(),
            }),
            ..Default::default()
        }),
        snapshot: Some(ModelDeploymentSnapshot {
            source: Some(model_deployment_snapshot::Source::Local(
                ResolvedLocalModelDeploymentSnapshot {
                    model_id: model_id.to_owned(),
                    profile_id: profile_id.to_owned(),
                    selected_target: Some(ResolvedSelectedTarget {
                        accelerator_memory_architecture: architecture as i32,
                        ..Default::default()
                    }),
                    sizing: bytes.map(|bytes| ResolvedSizing {
                        accelerator_memory_bytes: Some(bytes),
                        ..Default::default()
                    }),
                    ..Default::default()
                },
            )),
        }),
        state: Some(ModelDeploymentState {
            status: status as i32,
            ..Default::default()
        }),
        ..Default::default()
    }
}

/// Accelerator memory of a fixture model at its default 32768-token total context.
fn default_estimate(model_id: &str) -> u64 {
    let model = MODELS
        .iter()
        .find(|model| model.id == model_id)
        .expect("fixture model");
    model.weight_bytes + 32_768 * BYTES_PER_CONTEXT_TOKEN
}

const BYTES_PER_CONTEXT_TOKEN: u64 = 100_000;

/// The declared machines, each with one GPU.
fn machine_yaml(machine: &str) -> &'static str {
    match machine {
        "s8" => NVIDIA_8G,
        "x32" => NVIDIA_X_32G,
        "mac16" => MAC_16G,
        "mac32" => MAC_32G,
        "mac64" => MAC_64G,
        "uni128" => UNIFIED_128G,
        other => panic!("unknown machine {other}"),
    }
}

/// A dedicated 8 GiB NVIDIA GPU of a model no class names, beside 32 GiB of host memory.
const NVIDIA_8G: &str = r#"
platform: linux/amd64
cpuCount: 8
hostMemory: { totalBytes: 34359738368, idleUsedBytes: 4294967296 }
gpus:
  - id: GPU-s8
    vendor: nvidia
    model: Fixture GPU S
    driverVersion: "580.173.02"
    memory: { architecture: dedicated, totalBytes: 8589934592, reservedBytes: 536870912 }
"#;

/// A dedicated 32 GiB NVIDIA GPU of the model the GPU-model class names, beside 64 GiB of host
/// memory.
const NVIDIA_X_32G: &str = r#"
platform: linux/amd64
cpuCount: 16
hostMemory: { totalBytes: 68719476736, idleUsedBytes: 4294967296 }
gpus:
  - id: GPU-x32
    vendor: nvidia
    model: Fixture GPU X
    driverVersion: "580.173.02"
    memory: { architecture: dedicated, totalBytes: 34359738368, reservedBytes: 536870912 }
"#;

/// A 16 GiB Mac, 4 GiB in use at idle.
const MAC_16G: &str = r#"
platform: darwin/arm64
cpuCount: 8
hostMemory: { totalBytes: 17179869184, idleUsedBytes: 4294967296 }
gpus:
  - { id: apple-gpu-0, vendor: apple, model: Fixture Apple GPU, memory: { architecture: unified } }
"#;

/// A 32 GiB Mac, 6 GiB in use at idle.
const MAC_32G: &str = r#"
platform: darwin/arm64
cpuCount: 10
hostMemory: { totalBytes: 34359738368, idleUsedBytes: 6442450944 }
gpus:
  - { id: apple-gpu-0, vendor: apple, model: Fixture Apple GPU, memory: { architecture: unified } }
"#;

/// A 64 GiB Mac, 8 GiB in use at idle.
const MAC_64G: &str = r#"
platform: darwin/arm64
cpuCount: 16
hostMemory: { totalBytes: 68719476736, idleUsedBytes: 8589934592 }
gpus:
  - { id: apple-gpu-0, vendor: apple, model: Fixture Apple GPU, memory: { architecture: unified } }
"#;

/// An NVIDIA GPU sharing 128 GiB of unified memory with its host, 8 GiB in use at idle.
const UNIFIED_128G: &str = r#"
platform: linux/arm64
cpuCount: 20
hostMemory: { totalBytes: 137438953472, idleUsedBytes: 8589934592 }
gpus:
  - id: GPU-uni128
    vendor: nvidia
    model: Fixture Unified GPU
    driverVersion: "580.173.02"
    memory: { architecture: unified }
"#;

/// One fixture model, with one configuration and one profile. Its estimate adds 100 kB per token
/// of total context to its weights.
struct FixtureModel {
    id: &'static str,
    weight_bytes: u64,
    intelligence: u32,
    classes: &'static [&'static str],
    request_kind: &'static str,
    profile: &'static str,
}

const MODELS: &[FixtureModel] = &[
    FixtureModel {
        id: "x-only",
        weight_bytes: 4_000_000_000,
        intelligence: 45,
        classes: &["nvidia-x"],
        request_kind: "INFERENCE_REQUEST_KIND_CHAT",
        profile: "x",
    },
    FixtureModel {
        id: "big",
        weight_bytes: 16_000_000_000,
        intelligence: 40,
        classes: CHAT_CLASSES,
        request_kind: "INFERENCE_REQUEST_KIND_CHAT",
        profile: "q4",
    },
    FixtureModel {
        id: "mid",
        weight_bytes: 6_000_000_000,
        intelligence: 25,
        classes: CHAT_CLASSES,
        request_kind: "INFERENCE_REQUEST_KIND_CHAT",
        profile: "q4",
    },
    FixtureModel {
        id: "small",
        weight_bytes: 2_000_000_000,
        intelligence: 10,
        classes: CHAT_CLASSES,
        request_kind: "INFERENCE_REQUEST_KIND_CHAT",
        profile: "q4",
    },
    FixtureModel {
        id: "decider",
        weight_bytes: 1_000_000_000,
        intelligence: 5,
        classes: CHAT_CLASSES,
        request_kind: "INFERENCE_REQUEST_KIND_DECISION",
        profile: "q8",
    },
];

const CHAT_CLASSES: &[&str] = &["nvidia-dedicated", "nvidia-unified", "apple"];

/// Writes and loads the made-up catalog: the vendor, memory architecture and GPU-model classes,
/// one llama.cpp engine for linux/amd64, linux/arm64 and darwin/arm64, the fixture models, and
/// their recommendation lists.
fn catalog() -> Catalog {
    let root = tempfile::tempdir().expect("catalog root");
    let mut files = vec![
        ("hardware-classes/nvidia.yaml", class("nvidia", "")),
        (
            "hardware-classes/nvidia-dedicated.yaml",
            class("nvidia-dedicated", DEDICATED),
        ),
        (
            "hardware-classes/nvidia-unified.yaml",
            class(
                "nvidia-unified",
                "  memoryArchitecture: \"ACCELERATOR_MEMORY_ARCHITECTURE_UNIFIED\"\n",
            ),
        ),
        (
            "hardware-classes/nvidia-x.yaml",
            class(
                "nvidia-x",
                &format!("{DEDICATED}  deviceModels: [\"Fixture GPU X\"]\n"),
            ),
        ),
        (
            "hardware-classes/apple.yaml",
            "hardwareClassId: \"apple\"\ndisplayName: \"apple\"\nselector:\n  accelerators: [\"apple\"]\n"
                .to_owned(),
        ),
        ("engines/llama.yaml", ENGINE.to_owned()),
        ("recommendations/nvidia.yaml", NVIDIA_LISTS.to_owned()),
        (
            "recommendations/nvidia-dedicated.yaml",
            DEDICATED_LISTS.to_owned(),
        ),
        ("recommendations/nvidia-unified.yaml", UNIFIED_LISTS.to_owned()),
        ("recommendations/apple.yaml", APPLE_LISTS.to_owned()),
    ];
    let models = MODELS
        .iter()
        .map(|model| (format!("models/{}.yaml", model.id), model_yaml(model)))
        .collect::<Vec<_>>();
    files.extend(
        models
            .iter()
            .map(|(path, body)| (path.as_str(), body.clone())),
    );
    for (path, body) in files {
        let path = root.path().join(path);
        std::fs::create_dir_all(path.parent().expect("directory")).expect("catalog directory");
        std::fs::write(path, body).expect("catalog declaration");
    }
    Catalog::load_with_policy(
        root.path(),
        OciArtifactPolicy::Development,
        crate::model::engine::declaration_validator(),
    )
    .expect("recommendation catalog")
}

const DEDICATED: &str = "  memoryArchitecture: \"ACCELERATOR_MEMORY_ARCHITECTURE_DEDICATED\"\n";

/// An NVIDIA hardware class with the extra selector lines `conditions`.
fn class(id: &str, conditions: &str) -> String {
    format!(
        "hardwareClassId: \"{id}\"\ndisplayName: \"{id}\"\nselector:\n  accelerators: [\"nvidia\"]\n{conditions}"
    )
}

fn model_yaml(model: &FixtureModel) -> String {
    let FixtureModel {
        id,
        weight_bytes: base,
        intelligence,
        classes,
        request_kind,
        profile,
    } = model;
    let support = classes
        .iter()
        .map(|class| {
            format!(
                "      - hardwareClassId: \"{class}\"\n        bindingSizing: [{{ bindingId: \"weights\" }}]\n"
            )
        })
        .collect::<String>();
    // Each artifact the configuration approves runs on one of its classes.
    let artifacts = [
        ("linux-amd64", "nvidia-"),
        ("linux-arm64", "nvidia-unified"),
        ("darwin-arm64", "apple"),
    ]
    .into_iter()
    .filter(|(_, class)| classes.iter().any(|supported| supported.starts_with(class)))
    .map(|(artifact, _)| {
        format!("      - {{ engineReleaseId: \"llama\", engineArtifactId: \"{artifact}\" }}\n")
    })
    .collect::<String>();
    format!(
        r#"modelId: "{id}"
displayName: "{id}"
version: "1"
maximumContextTokens: 262144
intelligence: {{ overall: {intelligence} }}
artifacts:
  - artifactId: "weights"
    weightQuantization: "Q4"
    source:
      repository: "dotcore/{id}"
      revision: "0123456789abcdef0123456789abcdef01234567"
      file:
        path: "weights.gguf"
        provenanceDigest: "0000000000000000000000000000000000000000000000000000000000000000"
configurations:
  - configurationId: "llama"
    engineArtifacts:
{artifacts}    artifactBindings: [{{ bindingId: "weights", modelArtifactId: "weights" }}]
    sizing:
      parameters: {{ baseBytes: {base}, bytesPerContextToken: {BYTES_PER_CONTEXT_TOKEN} }}
      estimates:
        acceleratorMemory:
          unit: bytes
          equation: "${{baseBytes}} + ${{bytesPerContextToken}} * ${{total-context}}"
    hardwareSupport:
{support}    engineConfiguration:
      engineKind: "ENGINE_KIND_LLAMA_CPP"
      llamaCpp: {{ fixedSettings: {{ gpuLayers: 0 }} }}
    executionCapabilities: {{ supportedParallelShapes: ["MODEL_PARALLEL_SHAPE_ONE"] }}
    requestKinds: ["{request_kind}"]
    inputCapabilities: {{ supportedModalities: ["MODEL_INPUT_MODALITY_TEXT"] }}
profiles:
  - profileId: "{profile}"
    displayName: "{profile}"
    configurationId: "llama"
    defaultModelArtifactBindingId: "weights"
    engineParameterDefaults:
      values: [{{ parameterId: "prompt-cache-memory-mib", unsignedInteger: "0" }}]
"#
    )
}

const ENGINE: &str = r#"
engineReleaseId: "llama"
engineKind: "ENGINE_KIND_LLAMA_CPP"
version: "1"
parameterSchema:
  schemaId: "llama-v2"
  parameters:
    - { parameterId: "total-context", displayName: "Total context", exposed: true, unsignedInteger: { minimum: "1024", maximum: "16777216", unit: "tokens", defaultValue: "32768" } }
    - { parameterId: "context-per-request", displayName: "Context per request", exposed: true, unsignedInteger: { minimum: "1024", maximum: "16777216", unit: "tokens" } }
    - { parameterId: "parallel-requests", displayName: "Parallel requests", exposed: true, unsignedInteger: { minimum: "1", maximum: "64", unit: "requests", defaultValue: "4" } }
    - { parameterId: "prompt-cache-memory-mib", displayName: "Prompt cache memory", unsignedInteger: { minimum: "0", maximum: "65536", unit: "MiB" } }
artifacts:
  - artifactId: "linux-amd64"
    oci:
      repository: { registry: { host: "registry.dotcore.test" }, name: "llama" }
      digest: { algorithm: "sha256", encoded: "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef" }
    selectors:
      - platform: { operatingSystem: "linux", architecture: "amd64" }
        accelerators: ["nvidia"]
        minimumNvidiaDriverMajorVersion: 580
  - artifactId: "linux-arm64"
    oci:
      repository: { registry: { host: "registry.dotcore.test" }, name: "llama" }
      digest: { algorithm: "sha256", encoded: "1123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef" }
    selectors:
      - platform: { operatingSystem: "linux", architecture: "arm64" }
        accelerators: ["nvidia"]
        minimumNvidiaDriverMajorVersion: 580
  - artifactId: "darwin-arm64"
    nix: { flake: "self", package: "engines.llama-cpp.native", executable: "llama-server" }
    selectors:
      - platform: { operatingSystem: "darwin", architecture: "arm64" }
        accelerators: ["apple"]
"#;

/// Lists every NVIDIA target falls back to.
const NVIDIA_LISTS: &str = r#"
hardwareClassId: "nvidia"
useCases:
  - useCase: "USE_CASE_TRANSLATION"
    entries:
      - { modelId: "mid", profileId: "q4" }
      - { modelId: "small", profileId: "q4" }
  - useCase: "USE_CASE_DECISION_MAKING"
    entries:
      - { modelId: "decider", profileId: "q8" }
"#;

/// Dedicated GPUs' coding list: the GPU-model entry first, then minimums on the GPU's memory.
const DEDICATED_LISTS: &str = r#"
hardwareClassId: "nvidia-dedicated"
useCases:
  - useCase: "USE_CASE_CODING_AND_AGENTS"
    entries:
      - { modelId: "x-only", profileId: "x" }
      - { modelId: "big", profileId: "q4", hardwareRequirements: { minimumMemoryBytesPerDevice: "24000000000" } }
      - { modelId: "mid", profileId: "q4", hardwareRequirements: { minimumMemoryBytesPerDevice: "12000000000" } }
      - { modelId: "small", profileId: "q4" }
"#;

const UNIFIED_LISTS: &str = r#"
hardwareClassId: "nvidia-unified"
useCases:
  - useCase: "USE_CASE_CODING_AND_AGENTS"
    entries:
      - { modelId: "big", profileId: "q4" }
      - { modelId: "mid", profileId: "q4" }
"#;

/// Macs' lists, with minimums on system memory.
const APPLE_LISTS: &str = r#"
hardwareClassId: "apple"
useCases:
  - useCase: "USE_CASE_CODING_AND_AGENTS"
    entries:
      - { modelId: "big", profileId: "q4", hardwareRequirements: { minimumSystemMemoryBytes: "48000000000" } }
      - { modelId: "mid", profileId: "q4", hardwareRequirements: { minimumSystemMemoryBytes: "24000000000" } }
      - { modelId: "small", profileId: "q4" }
  - useCase: "USE_CASE_TRANSLATION"
    entries:
      - { modelId: "mid", profileId: "q4", hardwareRequirements: { minimumSystemMemoryBytes: "24000000000" } }
      - { modelId: "small", profileId: "q4" }
  - useCase: "USE_CASE_DECISION_MAKING"
    entries:
      - { modelId: "decider", profileId: "q8" }
"#;
