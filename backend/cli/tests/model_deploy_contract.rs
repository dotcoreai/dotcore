//! Process-level contracts for deploying models with `dotc model catalog`, `template` and
//! `deploy`, run against the in-process fake manager.

use std::{
    io::Write,
    process::{Command, Output, Stdio},
};

use dotcore_proto::dotcore::{
    capacity::v1::PlacementPreview,
    catalog::v1::{
        BooleanParameterSchema, EngineParameterSchema, Model, ModelConfigurationOption,
        ModelEngineConfiguration, ModelOption, ModelOptionParameterDefinition,
        ModelOptionParameterValue, ModelOptionValues, ModelProfile, ParameterDefinition,
        ParameterValue, ParameterValues, ParameterVisibility, SecretParameterSchema,
        StringEnumModelOptionParameterSchema, UnsignedIntegerParameterSchema,
        model_option_parameter_definition, model_option_parameter_value, parameter_definition,
        parameter_value,
    },
    deployment::v1::{ExactPlacement, MemoryFit},
    model::v1::{
        CreateLocalModelDeploymentRequest, CreateModelDeploymentRequest, DirectRouteExposure,
        EstimateLocalModelResourcesRequest, EstimateLocalModelResourcesResponse,
        HardwareClassResourceEstimate, HiddenDeploymentExposure,
        ListModelProfileCompatibilityRequest, ListModelProfileCompatibilityResponse,
        LocalModelDeploymentConfiguration, ModelCompatibilityStatus, ModelDeploymentExposure,
        ModelProfileCompatibility, PreviewLocalModelDeploymentRequest,
        PreviewLocalModelDeploymentResponse, ResolvedLocalModelDeploymentSnapshot,
        ResolvedModelOption, StartingTotalContextSource, TargetCompatibility,
        create_model_deployment_request, model_deployment_exposure,
    },
};
use serde_json::json;
use tonic::Status;

mod support;

use support::{
    command::{dotc_at, isolated_dotc},
    fake_manager::{FakeManager, FakeState, MODEL_TARGET_ID, NODE_ID, Rpc, Scripted},
    pty::{Session, Step, run_in_pty},
    yaml_documents,
};

#[test]
fn catalog_lists_each_profile_with_its_fit_on_each_target() {
    let manager = FakeManager::start(catalog_site());

    let output = run(&manager, &["catalog"]);

    assert_eq!(text(&output.stderr), "");
    assert_eq!(output.status.code(), Some(0));
    assert_eq!(
        text(&output.stdout),
        "  MODEL         ID           PROFILE       TARGET                      FIT\n  \
         Qwen 3.8 27B  qwen3-8-27b  balanced      gb10-host Simulated models  fits now\n  \
         Qwen 3.8 27B  qwen3-8-27b  long-context  gb10-host Simulated models  too large\n"
    );
    assert_eq!(
        manager.state().catalog.compatibility_requests,
        [ListModelProfileCompatibilityRequest::default()]
    );
}

#[test]
fn catalog_of_one_model_narrows_the_compatibility_read_and_prints_yaml() {
    let manager = FakeManager::start(catalog_site());

    let output = run(&manager, &["--yaml", "catalog", "27b"]);

    assert_eq!(text(&output.stderr), "");
    assert_eq!(output.status.code(), Some(0));
    assert_eq!(
        yaml_documents(&text(&output.stdout)),
        [json!({
            "models": [{
                "modelId": "qwen3-8-27b",
                "displayName": "Qwen 3.8 27B",
                "profiles": [
                    {"profileId": "balanced", "configurationId": "gguf"},
                    {"profileId": "long-context", "configurationId": "gguf"},
                ],
            }],
            "compatibility": {
                "targets": [{
                    "target": {"siteNodeId": NODE_ID, "executionTargetId": MODEL_TARGET_ID},
                    "status": "MODEL_COMPATIBILITY_STATUS_RUNS",
                }],
                "entries": [
                    {
                        "modelId": "qwen3-8-27b",
                        "profileId": "balanced",
                        "target": {"siteNodeId": NODE_ID, "executionTargetId": MODEL_TARGET_ID},
                        "status": "MODEL_COMPATIBILITY_STATUS_RUNS",
                        "memoryFit": "MEMORY_FIT_FITS",
                        "freeMemoryFit": "MEMORY_FIT_FITS",
                    },
                    {
                        "modelId": "qwen3-8-27b",
                        "profileId": "long-context",
                        "target": {"siteNodeId": NODE_ID, "executionTargetId": MODEL_TARGET_ID},
                        "status": "MODEL_COMPATIBILITY_STATUS_RUNS",
                        "memoryFit": "MEMORY_FIT_EXCEEDS",
                        "freeMemoryFit": "MEMORY_FIT_EXCEEDS",
                    },
                ],
            },
        })]
    );
    assert_eq!(
        manager.state().catalog.compatibility_requests,
        [ListModelProfileCompatibilityRequest {
            model_id: Some("qwen3-8-27b".to_owned()),
            ..ListModelProfileCompatibilityRequest::default()
        }]
    );
}

#[test]
fn catalog_of_an_unknown_model_fails_listing_the_catalog_models() {
    let manager = FakeManager::start(catalog_site());

    let output = run(&manager, &["catalog", "llama"]);

    assert_eq!(output.status.code(), Some(1));
    assert_eq!(text(&output.stdout), "");
    assert_eq!(
        text(&output.stderr),
        "no catalog model matches \"llama\"; known catalog models: qwen3-8-27b (Qwen 3.8 27B)\n"
    );
}

#[test]
fn template_writes_the_starting_payload_with_guidance_comments() {
    let manager = FakeManager::start(deploy_site());

    let output = run(&manager, &["template", "qwen"]);

    assert_eq!(text(&output.stderr), "");
    assert_eq!(output.status.code(), Some(0));
    assert_eq!(
        text(&output.stdout),
        "# Qwen 3.8 27B · balanced on gb10-host Simulated models · fits now\n\
         model: qwen3-8-27b\n\
         profile: balanced\n\
         node: gb10-host\n\
         name: \"Qwen 3.8 27B\"\n\
         route: qwen3-8-27b  # null keeps it hidden\n\
         parameters:\n  \
         # Context per request. What context per request does. 4096-262144 tokens. Fits all 4 at once: 49152\n  \
         context-per-request: 32768\n  \
         # Parallel requests. What parallel requests does. 1-64\n  \
         parallel-requests: 4\n  \
         # Total context. What total context does. 4096 or more tokens. Follows context per request and parallel requests unless set. Largest fit: 196608\n  \
         # total-context: 131072\n  \
         # Flash attention. What flash attention does. true or false. Advanced\n  \
         # flash-attention: true\n  \
         # API key. What api key does. secret, generated when left out. Advanced\n  \
         # api-key:\n\
         options:\n  \
         # How precisely the context cache is stored.\n  \
         kv-cache-precision:\n    \
         # KV cache Precision. one of f16 | q8_0\n    \
         precision: f16\n"
    );
    let state = manager.state();
    assert_eq!(
        state.catalog.preview_requests,
        [PreviewLocalModelDeploymentRequest {
            local: Some(CreateLocalModelDeploymentRequest {
                model_id: "qwen3-8-27b".to_owned(),
                profile_id: "balanced".to_owned(),
                placement: Some(placement()),
                configuration: Some(LocalModelDeploymentConfiguration::default()),
            }),
        }]
    );
    assert_eq!(
        state.catalog.estimate_requests,
        [EstimateLocalModelResourcesRequest {
            model_id: "qwen3-8-27b".to_owned(),
            profile_id: "balanced".to_owned(),
            configuration: Some(LocalModelDeploymentConfiguration::default()),
            placement: Some(placement()),
        }]
    );
}

#[test]
fn template_overrides_write_their_values_and_are_estimated() {
    let manager = FakeManager::start(deploy_site());

    let output = run(
        &manager,
        &[
            "template",
            "qwen",
            "--profile",
            "balanced",
            "--node",
            "gb10",
            "--set",
            "flash-attention=false",
            "--set",
            "parallel-requests=2",
            "--set",
            "option.kv-cache-precision.precision=q8_0",
        ],
    );

    assert_eq!(text(&output.stderr), "");
    assert_eq!(output.status.code(), Some(0));
    let stdout = text(&output.stdout);
    assert!(
        stdout.contains(
            "  # Flash attention. What flash attention does. true or false\n  flash-attention: false\n"
        ),
        "{stdout}"
    );
    assert!(
        stdout.contains(
            "  # Context per request. What context per request does. 4096-262144 tokens. Fits all 2 at once: 49152\n"
        ),
        "{stdout}"
    );
    assert!(stdout.contains("    precision: q8_0\n"), "{stdout}");
    assert_eq!(
        manager.state().catalog.estimate_requests[0].configuration,
        Some(LocalModelDeploymentConfiguration {
            engine_parameter_values: Some(ParameterValues {
                values: vec![
                    value("flash-attention", parameter_value::Value::Boolean(false)),
                    value(
                        "parallel-requests",
                        parameter_value::Value::UnsignedInteger(2)
                    ),
                ],
            }),
            option_overrides: vec![ModelOptionValues {
                option_id: "kv-cache-precision".to_owned(),
                values: vec![ModelOptionParameterValue {
                    parameter_id: "precision".to_owned(),
                    value: Some(model_option_parameter_value::Value::String(
                        "q8_0".to_owned()
                    )),
                }],
            }],
            ..LocalModelDeploymentConfiguration::default()
        })
    );
}

#[test]
fn template_with_an_unknown_override_fails_before_estimating() {
    let manager = FakeManager::start(deploy_site());

    let output = run(&manager, &["template", "qwen", "--set", "batch=1"]);

    assert_eq!(output.status.code(), Some(1));
    assert_eq!(text(&output.stdout), "");
    assert_eq!(
        text(&output.stderr),
        "unknown parameter \"batch\"; this profile's parameters are: context-per-request, \
         parallel-requests, total-context, flash-attention, api-key\n"
    );
    assert_eq!(manager.state().catalog.estimate_requests, []);
}

#[test]
fn deploy_from_a_payload_on_stdin_sends_the_values_it_lists() {
    let manager = FakeManager::start(deploy_site());

    let output = run_with_input(
        &manager,
        &["deploy", "-f", "-", "--yes"],
        "model: qwen3-8-27b\nprofile: balanced\nnode: gb10-host\nname: Qwen\nroute: qwen\n\
         parameters:\n  context-per-request: 65536\n  flash-attention: false\n\
         options:\n  kv-cache-precision:\n    precision: q8_0\n",
    );

    assert_eq!(text(&output.stderr), "");
    assert_eq!(output.status.code(), Some(0));
    assert_eq!(text(&output.stdout), "✔ Deploying Qwen (created-1)\n");
    assert_eq!(
        manager.state().catalog.create_requests,
        [create_request(
            LocalModelDeploymentConfiguration {
                engine_parameter_values: Some(ParameterValues {
                    values: vec![
                        value(
                            "context-per-request",
                            parameter_value::Value::UnsignedInteger(65536)
                        ),
                        value("flash-attention", parameter_value::Value::Boolean(false)),
                    ],
                }),
                option_overrides: vec![ModelOptionValues {
                    option_id: "kv-cache-precision".to_owned(),
                    values: vec![ModelOptionParameterValue {
                        parameter_id: "precision".to_owned(),
                        value: Some(model_option_parameter_value::Value::String(
                            "q8_0".to_owned()
                        )),
                    }],
                }],
                ..LocalModelDeploymentConfiguration::default()
            },
            Some("qwen"),
            "Qwen",
        )]
    );
}

#[test]
fn deploy_with_yes_and_no_payload_deploys_the_starting_values_with_overrides() {
    let manager = FakeManager::start(deploy_site());

    let output = run(
        &manager,
        &["deploy", "qwen", "--yes", "--set", "parallel-requests=2"],
    );

    assert_eq!(text(&output.stderr), "");
    assert_eq!(output.status.code(), Some(0));
    assert_eq!(
        text(&output.stdout),
        "✔ Deploying Qwen 3.8 27B (created-1)\n"
    );
    assert_eq!(
        manager.state().catalog.create_requests,
        [create_request(
            LocalModelDeploymentConfiguration {
                engine_parameter_values: Some(ParameterValues {
                    values: vec![value(
                        "parallel-requests",
                        parameter_value::Value::UnsignedInteger(2)
                    )],
                }),
                ..LocalModelDeploymentConfiguration::default()
            },
            Some("qwen3-8-27b"),
            "Qwen 3.8 27B",
        )]
    );
}

#[test]
fn deploy_without_a_payload_or_yes_fails_before_calling_the_manager() {
    let manager = FakeManager::start(deploy_site());

    let output = run(&manager, &["deploy", "qwen"]);

    assert_eq!(output.status.code(), Some(1));
    assert_eq!(text(&output.stdout), "");
    assert_eq!(
        text(&output.stderr),
        "deploying needs a payload; pass -f <payload>, or --yes to deploy the starting values\n"
    );
    assert_eq!(manager.calls(), []);
}

#[test]
fn deploy_without_a_model_or_a_terminal_names_what_to_pass_and_never_asks() {
    let manager = FakeManager::start(deploy_site());

    for (arguments, message) in [
        (
            &["deploy"][..],
            "deploying needs a model; pass <model> with --yes, or -f <payload>, or run in a \
             terminal to choose one\n",
        ),
        (
            &["--yaml", "deploy"][..],
            "deploying needs a model, which --yaml never asks for; pass <model> with --yes, or \
             -f <payload>\n",
        ),
        (
            &["deploy", "--yes"][..],
            "deploying needs a model; pass <model>, or -f <payload>\n",
        ),
    ] {
        let output = run(&manager, arguments);

        assert_eq!(output.status.code(), Some(1), "{arguments:?}");
        assert_eq!(text(&output.stdout), "", "{arguments:?}");
        assert_eq!(text(&output.stderr), message, "{arguments:?}");
    }
    assert_eq!(manager.calls(), []);
}

#[test]
fn deploy_on_a_line_terminal_asks_each_field_and_creates_what_was_reviewed() {
    let mut state = deploy_site();
    state.catalog.models.push(Model {
        model_id: "gemma-4-e4b".to_owned(),
        display_name: "Gemma 4 E4B".to_owned(),
        ..Model::default()
    });
    let manager = FakeManager::start(state);

    let Some(run) = run_in_pty(
        terminal_command(&manager, &["deploy"]),
        Session::new([
            Step::Wait("Model & machine"),
            Step::Wait("gemma-4-e4b (unavailable: no machine can run models)"),
            Step::Send(b"\n"),
            Step::Wait("long-context  too large"),
            Step::Send(b"\n"),
            Step::Wait("KV cache Precision"),
            Step::Send(b"2\n"),
            Step::Wait("Parallel requests (1-64) [4]:"),
            Step::Send(b"2\n"),
            Step::Wait("Context per request (4096-262144 tokens) [32768]:"),
            Step::Send(b"\n"),
            Step::Wait("Total context (follows the two above) [131072]:"),
            Step::Send(b"\n"),
            Step::Wait("Name [Qwen 3.8 27B]:"),
            Step::Send(b"\n"),
            Step::Wait("Route (follows the name) [qwen-3.8-27b]:"),
            Step::Send(b"\n"),
            Step::Wait("Deploy?"),
            Step::Wait("Choice [1]:"),
            Step::Send(b"\n"),
        ]),
    ) else {
        return;
    };

    assert_eq!(run.code(), Some(0), "screen:\n{}", run.session());
    assert_eq!(run.stdout(), "OK Deploying Qwen 3.8 27B (created-1)\n");
    assert_eq!(run.stderr(), "");
    let session = run.session();
    let model = session
        .lines()
        .skip_while(|line| line.trim() != "Model")
        .take(4)
        .map(str::trim_end)
        .collect::<Vec<_>>();
    assert_eq!(
        model,
        [
            "Model",
            "  1) Qwen 3.8 27B  qwen3-8-27b · fits now",
            "  2) Gemma 4 E4B  gemma-4-e4b (unavailable: no machine can run models)",
            "Choice [1]:",
        ],
        "{session}"
    );
    for expected in [
        "OK Model & machine: Qwen 3.8 27B · balanced on gb10-host Simulated models",
        "OK Capabilities: KV cache Precision q8_0",
        "OK Memory: fits now",
        "Fits all 2 at once: 49152",
        "OK Context: 32768 per request · 2 parallel · 131072 total",
    ] {
        assert!(
            run.session().lines().any(|line| line.trim() == expected),
            "{expected}\n{}",
            run.session()
        );
    }
    let state = manager.state();
    assert_eq!(state.catalog.preview_requests.len(), 1);
    assert_eq!(
        state.catalog.create_requests,
        [CreateModelDeploymentRequest {
            exposure: Some(ModelDeploymentExposure {
                kind: Some(model_deployment_exposure::Kind::Direct(
                    DirectRouteExposure {
                        route_name: "qwen-3.8-27b".to_owned(),
                    }
                )),
            }),
            ..create_request(
                LocalModelDeploymentConfiguration {
                    engine_parameter_values: Some(ParameterValues {
                        values: vec![value(
                            "parallel-requests",
                            parameter_value::Value::UnsignedInteger(2)
                        )],
                    }),
                    option_overrides: vec![ModelOptionValues {
                        option_id: "kv-cache-precision".to_owned(),
                        values: vec![ModelOptionParameterValue {
                            parameter_id: "precision".to_owned(),
                            value: Some(model_option_parameter_value::Value::String(
                                "q8_0".to_owned()
                            )),
                        }],
                    }],
                    ..LocalModelDeploymentConfiguration::default()
                },
                None,
                "Qwen 3.8 27B",
            )
        }]
    );
}

#[test]
fn deploy_on_a_rich_terminal_shows_forms_and_leaves_one_line_per_step() {
    let manager = FakeManager::start(deploy_site());

    let Some(run) = run_in_pty(
        rich_terminal_command(&manager, &["deploy", "qwen"]),
        Session::new([
            Step::Wait("Profile"),
            Step::Send(b"\r"),
            Step::Wait("KV cache Precision"),
            Step::Send(b"\x1b[C\r"),
            Step::Wait("Parallel requests"),
            Step::Send(b"\x7f2"),
            Step::Wait("Fits all 2 at once: 49152"),
            Step::Send(b"\r"),
            Step::Wait("[ Deploy ]"),
            Step::Send(b"\r"),
        ])
        .with_stdout_on_terminal(),
    ) else {
        return;
    };

    assert_eq!(run.code(), Some(0), "screen:\n{}", run.session());
    let session = run.session();
    let lines = session
        .lines()
        .map(str::trim_end)
        .filter(|line| !line.is_empty())
        .collect::<Vec<_>>();
    assert_eq!(
        lines[lines.len() - 6..],
        [
            "OK Model & machine: Qwen 3.8 27B · balanced on gb10-host Simulated models",
            "OK Capabilities: KV cache Precision q8_0",
            "OK Context: 32768 per request · 2 parallel · 131072 total",
            "OK Review & deploy: Qwen 3.8 27B · route qwen-3.8-27b",
            "OK Memory on gb10-host Simulated models: fit unknown",
            "OK Deploying Qwen 3.8 27B (created-1)",
        ],
        "{session}"
    );
    assert!(
        !session.contains("this deployment"),
        "no memory block is left behind:\n{session}"
    );
    assert_eq!(manager.state().catalog.create_requests.len(), 1);
}

#[test]
fn ctrl_c_in_a_form_cancels_the_deploy_and_creates_nothing() {
    let manager = FakeManager::start(deploy_site());

    let Some(run) = run_in_pty(
        rich_terminal_command(&manager, &["deploy", "qwen", "--profile", "balanced"]),
        Session::new([Step::Wait("KV cache Precision"), Step::Send(b"\x03")]),
    ) else {
        return;
    };

    assert_eq!(run.code(), Some(130), "screen:\n{}", run.session());
    assert_eq!(run.stdout(), "");
    assert_eq!(manager.state().catalog.create_requests, []);
    run.assert_terminal_restored();
}

#[test]
fn deploy_in_yaml_on_a_terminal_names_the_payload_flags_and_never_asks() {
    let manager = FakeManager::start(deploy_site());

    let Some(run) = run_in_pty(
        terminal_command(&manager, &["--yaml", "deploy", "qwen"]),
        Session::new([]),
    ) else {
        return;
    };

    assert_eq!(run.code(), Some(1), "screen:\n{}", run.screen());
    assert_eq!(run.stdout(), "");
    assert_eq!(
        run.stderr(),
        "deploying needs a payload, which --yaml never asks for; pass -f <payload>, or --yes to \
         deploy the starting values\n"
    );
    assert_eq!(run.screen().trim(), "");
    assert_eq!(manager.calls(), []);
}

#[test]
fn deploy_shows_the_reason_the_server_rejects_a_payload() {
    let mut state = deploy_site();
    state.scripted.insert(
        Rpc::CreateModelDeployment,
        Scripted::Fail(Status::invalid_argument(
            "context per request 300000 is above the maximum 262144",
        )),
    );
    let manager = FakeManager::start(state);

    let output = run(
        &manager,
        &[
            "deploy",
            "qwen",
            "--yes",
            "--set",
            "context-per-request=300000",
        ],
    );

    assert_eq!(output.status.code(), Some(1));
    assert_eq!(text(&output.stdout), "");
    let stderr = text(&output.stderr);
    assert!(
        stderr.contains("context per request 300000 is above the maximum 262144"),
        "{stderr}"
    );
}

#[test]
fn a_dry_run_reports_an_exceeding_fit_without_creating_and_exits_zero() {
    let mut state = deploy_site();
    state.catalog.estimate.placement_preview = Some(PlacementPreview {
        free_memory_fit: MemoryFit::Exceeds.into(),
        ..PlacementPreview::default()
    });
    state.catalog.estimate.hardware_class_estimates = vec![HardwareClassResourceEstimate {
        hardware_class_id: "nvidia".to_owned(),
        derived_engine_parameter_values: Some(ParameterValues {
            values: vec![value(
                "total-context",
                parameter_value::Value::UnsignedInteger(131_072),
            )],
        }),
        ..HardwareClassResourceEstimate::default()
    }];
    let manager = FakeManager::start(state);

    let output = run(&manager, &["deploy", "qwen", "--dry-run"]);

    assert_eq!(text(&output.stderr), "");
    assert_eq!(output.status.code(), Some(0));
    assert_eq!(
        text(&output.stdout),
        "! Qwen 3.8 27B (balanced) on gb10-host Simulated models exceeds the free memory\n  \
         Context per request fitting all 4 at once  49152\n  \
         Largest total context                      196608\n  \
         Derived total-context                      131072\n"
    );
    assert_eq!(manager.state().catalog.create_requests, []);
}

#[test]
fn a_dry_run_in_yaml_prints_the_resolved_payload_and_the_estimate() {
    let manager = FakeManager::start(deploy_site());

    let output = run(
        &manager,
        &[
            "--yaml",
            "deploy",
            "qwen",
            "--dry-run",
            "--set",
            "parallel-requests=2",
        ],
    );

    assert_eq!(text(&output.stderr), "");
    assert_eq!(output.status.code(), Some(0));
    assert_eq!(
        yaml_documents(&text(&output.stdout)),
        [json!({
            "payload": {
                "model": "qwen3-8-27b",
                "profile": "balanced",
                "node": NODE_ID,
                "target": MODEL_TARGET_ID,
                "name": "Qwen 3.8 27B",
                "route": "qwen3-8-27b",
                "parameters": {"parallel-requests": 2},
            },
            "derived": [],
            "estimate": {
                "placementPreview": {"freeMemoryFit": "MEMORY_FIT_FITS"},
                "largestTotalContextTokens": "196608",
                "largestContextTokensPerRequest": "49152",
            },
            "advisories": [],
        })]
    );
    assert_eq!(manager.state().catalog.create_requests, []);
}

#[test]
fn a_dry_run_fails_when_a_launch_would_reject_the_values() {
    let mut state = deploy_site();
    state.catalog.estimate.hardware_class_estimates = vec![HardwareClassResourceEstimate {
        hardware_class_id: "nvidia".to_owned(),
        launch_rejection: Some("parallel requests above the engine's limit".to_owned()),
        ..HardwareClassResourceEstimate::default()
    }];
    let manager = FakeManager::start(state);

    let output = run(&manager, &["deploy", "qwen", "--dry-run"]);

    assert_eq!(output.status.code(), Some(1));
    assert_eq!(
        text(&output.stderr),
        "a launch would reject these values: parallel requests above the engine's limit\n"
    );
    assert_eq!(manager.state().catalog.create_requests, []);
}

fn create_request(
    configuration: LocalModelDeploymentConfiguration,
    route: Option<&str>,
    name: &str,
) -> CreateModelDeploymentRequest {
    CreateModelDeploymentRequest {
        source: Some(create_model_deployment_request::Source::Local(
            CreateLocalModelDeploymentRequest {
                model_id: "qwen3-8-27b".to_owned(),
                profile_id: "balanced".to_owned(),
                placement: Some(placement()),
                configuration: Some(configuration),
            },
        )),
        exposure: Some(ModelDeploymentExposure {
            kind: Some(match route {
                Some(route) => model_deployment_exposure::Kind::Direct(DirectRouteExposure {
                    route_name: route.to_owned(),
                }),
                None => model_deployment_exposure::Kind::Hidden(HiddenDeploymentExposure {}),
            }),
        }),
        model_deployment_name: Some(name.to_owned()),
    }
}

/// The catalog site whose balanced profile previews with context, sizing, advanced and secret
/// parameters and a KV cache precision option, starting from 32768 tokens per request and a
/// total context that follows, with the given largest fits.
fn deploy_site() -> FakeState {
    let mut state = catalog_site();
    let model = &mut state.catalog.models[0];
    model.configurations = vec![ModelEngineConfiguration {
        configuration_id: "gguf".to_owned(),
        options: vec![ModelConfigurationOption {
            option_id: "kv-cache-precision".to_owned(),
            parameters: vec![ModelOptionParameterDefinition {
                parameter_id: "precision".to_owned(),
                display_name: "Precision".to_owned(),
                parameter_type: Some(
                    model_option_parameter_definition::ParameterType::StringEnum(
                        StringEnumModelOptionParameterSchema {
                            values: vec!["f16".to_owned(), "q8_0".to_owned()],
                        },
                    ),
                ),
            }],
            ..ModelConfigurationOption::default()
        }],
        ..ModelEngineConfiguration::default()
    }];
    model.options = vec![ModelOption {
        option_id: "kv-cache-precision".to_owned(),
        display_name: "KV cache".to_owned(),
        summary: "How precisely the context cache is stored.".to_owned(),
        visibility: ParameterVisibility::Basic.into(),
        ..ModelOption::default()
    }];
    let balanced = &mut state.catalog.compatibility.entries[0];
    balanced.starting_total_context_tokens = Some(131_072);
    balanced.starting_total_context_source =
        StartingTotalContextSource::PerRequestTimesParallel.into();

    let tokens = |minimum, maximum| {
        parameter_definition::ParameterType::UnsignedInteger(UnsignedIntegerParameterSchema {
            minimum,
            maximum,
            unit: "tokens".to_owned(),
            ..UnsignedIntegerParameterSchema::default()
        })
    };
    state.catalog.preview = PreviewLocalModelDeploymentResponse {
        resolved_snapshot: Some(ResolvedLocalModelDeploymentSnapshot {
            model_id: "qwen3-8-27b".to_owned(),
            configuration_id: "gguf".to_owned(),
            profile_id: "balanced".to_owned(),
            engine_parameter_schema: Some(EngineParameterSchema {
                parameters: vec![
                    definition(
                        "context-per-request",
                        "Context per request",
                        ParameterVisibility::Basic,
                        tokens(4096, Some(262_144)),
                    ),
                    definition(
                        "parallel-requests",
                        "Parallel requests",
                        ParameterVisibility::Basic,
                        parameter_definition::ParameterType::UnsignedInteger(
                            UnsignedIntegerParameterSchema {
                                minimum: 1,
                                maximum: Some(64),
                                ..UnsignedIntegerParameterSchema::default()
                            },
                        ),
                    ),
                    definition(
                        "total-context",
                        "Total context",
                        ParameterVisibility::Basic,
                        tokens(4096, None),
                    ),
                    definition(
                        "flash-attention",
                        "Flash attention",
                        ParameterVisibility::Advanced,
                        parameter_definition::ParameterType::Boolean(
                            BooleanParameterSchema::default(),
                        ),
                    ),
                    definition(
                        "api-key",
                        "API key",
                        ParameterVisibility::Advanced,
                        parameter_definition::ParameterType::Secret(
                            SecretParameterSchema::default(),
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
                    value(
                        "total-context",
                        parameter_value::Value::UnsignedInteger(131_072),
                    ),
                    value("flash-attention", parameter_value::Value::Boolean(true)),
                    value(
                        "api-key",
                        parameter_value::Value::String("not-shown".to_owned()),
                    ),
                ],
            }),
            resolved_options: vec![ResolvedModelOption {
                values: Some(ModelOptionValues {
                    option_id: "kv-cache-precision".to_owned(),
                    values: vec![ModelOptionParameterValue {
                        parameter_id: "precision".to_owned(),
                        value: Some(model_option_parameter_value::Value::String(
                            "f16".to_owned(),
                        )),
                    }],
                }),
            }],
            ..ResolvedLocalModelDeploymentSnapshot::default()
        }),
        advisories: Vec::new(),
    };
    state.catalog.estimate = EstimateLocalModelResourcesResponse {
        placement_preview: Some(PlacementPreview {
            free_memory_fit: MemoryFit::Fits.into(),
            ..PlacementPreview::default()
        }),
        largest_context_tokens_per_request: Some(49152),
        largest_total_context_tokens: Some(196_608),
        ..EstimateLocalModelResourcesResponse::default()
    };
    state
}

fn definition(
    id: &str,
    name: &str,
    visibility: ParameterVisibility,
    kind: parameter_definition::ParameterType,
) -> ParameterDefinition {
    ParameterDefinition {
        parameter_id: id.to_owned(),
        display_name: name.to_owned(),
        summary: format!("What {} does.", name.to_lowercase()),
        visibility: visibility.into(),
        parameter_type: Some(kind),
        ..ParameterDefinition::default()
    }
}

fn value(id: &str, value: parameter_value::Value) -> ParameterValue {
    ParameterValue {
        parameter_id: id.to_owned(),
        value: Some(value),
    }
}

/// A site with one model target and one catalog model whose balanced profile fits it now and
/// whose long-context profile is too large for it.
fn catalog_site() -> FakeState {
    let mut state = FakeState::with_models(Vec::new());
    state.catalog.models = vec![Model {
        model_id: "qwen3-8-27b".to_owned(),
        display_name: "Qwen 3.8 27B".to_owned(),
        profiles: ["balanced", "long-context"]
            .into_iter()
            .map(|profile_id| ModelProfile {
                profile_id: profile_id.to_owned(),
                configuration_id: "gguf".to_owned(),
                ..ModelProfile::default()
            })
            .collect(),
        ..Model::default()
    }];
    state.catalog.compatibility = ListModelProfileCompatibilityResponse {
        targets: vec![TargetCompatibility {
            target: Some(placement()),
            status: ModelCompatibilityStatus::Runs.into(),
            ..TargetCompatibility::default()
        }],
        entries: vec![
            compatibility("balanced", MemoryFit::Fits),
            compatibility("long-context", MemoryFit::Exceeds),
        ],
        profiles: Vec::new(),
    };
    state
}

fn compatibility(profile_id: &str, fit: MemoryFit) -> ModelProfileCompatibility {
    ModelProfileCompatibility {
        model_id: "qwen3-8-27b".to_owned(),
        profile_id: profile_id.to_owned(),
        target: Some(placement()),
        status: ModelCompatibilityStatus::Runs.into(),
        memory_fit: fit.into(),
        free_memory_fit: fit.into(),
        ..ModelProfileCompatibility::default()
    }
}

fn placement() -> ExactPlacement {
    ExactPlacement {
        site_node_id: NODE_ID.to_owned(),
        execution_target_id: MODEL_TARGET_ID.to_owned(),
    }
}

fn run(manager: &FakeManager, arguments: &[&str]) -> Output {
    dotc_at("model", &manager.address().to_string(), arguments)
        .output()
        .expect("run dotc model")
}

fn run_with_input(manager: &FakeManager, arguments: &[&str], input: &str) -> Output {
    let mut child = dotc_at("model", &manager.address().to_string(), arguments)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("start dotc model");
    child
        .stdin
        .take()
        .expect("dotc stdin")
        .write_all(input.as_bytes())
        .expect("write the payload");
    child.wait_with_output().expect("run dotc model")
}

/// `dotc model <arguments>` against the manager, for a pseudo-terminal to run as its controlling
/// terminal.
fn terminal_command(manager: &FakeManager, arguments: &[&str]) -> Command {
    let mut command = isolated_dotc();
    command
        .arg("model")
        .args(arguments)
        .env("DOTCORE_MANAGEMENT_ADDR", manager.address().to_string());
    command
}

/// A command on a terminal type that draws arrow-key prompts.
fn rich_terminal_command(manager: &FakeManager, arguments: &[&str]) -> Command {
    let mut command = terminal_command(manager, arguments);
    command.env("TERM", "xterm-256color");
    command
}

fn text(value: &[u8]) -> String {
    String::from_utf8_lossy(value).into_owned()
}
