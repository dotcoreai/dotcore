//! Process-level contracts for `dotc model`, run against the in-process fake manager.

use std::process::{Command, Output, Stdio};

use dotcore_proto::{
    dotcore::{
        catalog::v1::{
            BooleanParameterSchema, EngineParameterSchema, ParameterDefinition, ParameterValue,
            ParameterValues, ParameterVisibility, ParameterVisibilityOverride,
            SecretParameterSchema, UnsignedIntegerParameterSchema,
            parameter_definition::ParameterType, parameter_value::Value,
        },
        model::v1::{
            DeploymentLogEntry, DeploymentLogResumePosition, DeploymentLogSource,
            DeploymentLogSourceEvent, DeploymentLogSourceState, FollowDeploymentLogsResponse,
            ModelDeployment, ModelDeploymentDesiredState, ModelDeploymentFailure,
            ModelDeploymentProgress, ModelDeploymentSnapshot, ModelDeploymentStatus,
            ReadDeploymentLogsResponse, ResolvedSizing, UpstreamModelDesiredConfiguration,
            follow_deployment_logs_response::Event, model_deployment_snapshot::Source,
        },
    },
    google::protobuf::Timestamp,
};
use tonic::Status;

mod support;

use ModelDeploymentDesiredState::{Paused, Present, Teardown};
use ModelDeploymentStatus::{Downloading, Failed, Ready};

use support::{
    command::{detach_from_controlling_terminal, dotc_at, isolated_dotc},
    fake_manager::{Call, FakeManager, FakeState, Rpc, Scripted, local_model_deployment},
    pty::{Session, Step, run_in_pty},
};

#[test]
fn list_prints_one_record_block_per_deployment_in_every_state() {
    let manager = FakeManager::start(FakeState::with_models(mixed_deployments()));

    let output = run(&manager, &["list"]);

    assert_eq!(text(&output.stderr), "");
    assert_eq!(output.status.code(), Some(0));
    assert_eq!(
        text(&output.stdout),
        "Model deployment\nDOTCORE_MODEL_DEPLOYMENT_ID=model-1\nDOTCORE_MODEL_DEPLOYMENT_NAME=Gemma\nDOTCORE_MODEL_DEPLOYMENT_STATE=Ready\nDOTCORE_MODEL_DEPLOYMENT_NODE=gb10-host\nDOTCORE_MODEL_DEPLOYMENT_MODEL=Gemma 4 E4B\n\
\n\
Model deployment\nDOTCORE_MODEL_DEPLOYMENT_ID=model-2\nDOTCORE_MODEL_DEPLOYMENT_NAME=Qwen\nDOTCORE_MODEL_DEPLOYMENT_STATE=Downloading\nDOTCORE_MODEL_DEPLOYMENT_NODE=gb10-host\nDOTCORE_MODEL_DEPLOYMENT_MODEL=Gemma 4 E4B\n\
\n\
Model deployment\nDOTCORE_MODEL_DEPLOYMENT_ID=model-3\nDOTCORE_MODEL_DEPLOYMENT_NAME=Llama\nDOTCORE_MODEL_DEPLOYMENT_STATE=Failed\nDOTCORE_MODEL_DEPLOYMENT_NODE=gb10-host\nDOTCORE_MODEL_DEPLOYMENT_MODEL=Gemma 4 E4B\n\
\n\
Model deployment\nDOTCORE_MODEL_DEPLOYMENT_ID=model-4\nDOTCORE_MODEL_DEPLOYMENT_NAME=Mistral\nDOTCORE_MODEL_DEPLOYMENT_STATE=Paused\nDOTCORE_MODEL_DEPLOYMENT_NODE=gb10-host\nDOTCORE_MODEL_DEPLOYMENT_MODEL=Gemma 4 E4B\n\
\n\
Model deployment\nDOTCORE_MODEL_DEPLOYMENT_ID=model-5\nDOTCORE_MODEL_DEPLOYMENT_NAME=Phi\nDOTCORE_MODEL_DEPLOYMENT_STATE=Tearing down\nDOTCORE_MODEL_DEPLOYMENT_NODE=gb10-host\nDOTCORE_MODEL_DEPLOYMENT_MODEL=Gemma 4 E4B\n"
    );
    assert_eq!(
        manager.calls(),
        vec![
            call(Rpc::ListModelDeployments, None),
            call(Rpc::ListConfiguredExecutionTargets, None)
        ]
    );
}

#[test]
fn list_shows_the_node_id_when_the_manager_does_not_name_the_node() {
    let mut state = FakeState::with_models(mixed_deployments());
    state.execution_targets.clear();
    let manager = FakeManager::start(state);

    let output = run(&manager, &["list"]);

    assert_eq!(output.status.code(), Some(0));
    assert!(
        text(&output.stdout).contains("DOTCORE_MODEL_DEPLOYMENT_NODE=node-1\n"),
        "{}",
        text(&output.stdout)
    );
}

#[test]
fn list_of_an_empty_site_prints_nothing() {
    let manager = FakeManager::start(FakeState::with_models(Vec::new()));

    let output = run(&manager, &["list"]);

    assert_eq!(output.status.code(), Some(0));
    assert_eq!(text(&output.stdout), "");
    assert_eq!(text(&output.stderr), "");
}

#[test]
fn show_prints_every_key_for_a_ready_deployment() {
    let manager = FakeManager::start(FakeState::with_models(mixed_deployments()));

    let output = run(&manager, &["show", "Gemma"]);

    assert_eq!(text(&output.stderr), "");
    assert_eq!(output.status.code(), Some(0));
    assert_eq!(
        text(&output.stdout),
        "Model deployment\nDOTCORE_MODEL_DEPLOYMENT_ID=model-1\nDOTCORE_MODEL_DEPLOYMENT_NAME=Gemma\nDOTCORE_MODEL_DEPLOYMENT_STATE=Ready\nDOTCORE_MODEL_DEPLOYMENT_DESIRED_STATE=Present\nDOTCORE_MODEL_DEPLOYMENT_NODE=gb10-host\nDOTCORE_MODEL_DEPLOYMENT_EXECUTION_TARGET=node-1-model\nDOTCORE_MODEL_DEPLOYMENT_MODEL=Gemma 4 E4B\nDOTCORE_MODEL_DEPLOYMENT_PROFILE=default\nDOTCORE_MODEL_DEPLOYMENT_ENGINE=llama-cpp\nDOTCORE_MODEL_DEPLOYMENT_PROGRESS=\nDOTCORE_MODEL_DEPLOYMENT_FAILURE=\nDOTCORE_MODEL_DEPLOYMENT_MESSAGE=\n"
    );
}

#[test]
fn show_reports_progress_failure_and_status_message_by_id() {
    let manager = FakeManager::start(FakeState::with_models(mixed_deployments()));

    let downloading = run(&manager, &["show", "model-2"]);
    let failed = run(&manager, &["show", "model-3"]);

    assert_eq!(
        text(&downloading.stdout),
        "Model deployment\nDOTCORE_MODEL_DEPLOYMENT_ID=model-2\nDOTCORE_MODEL_DEPLOYMENT_NAME=Qwen\nDOTCORE_MODEL_DEPLOYMENT_STATE=Downloading\nDOTCORE_MODEL_DEPLOYMENT_DESIRED_STATE=Present\nDOTCORE_MODEL_DEPLOYMENT_NODE=gb10-host\nDOTCORE_MODEL_DEPLOYMENT_EXECUTION_TARGET=node-1-model\nDOTCORE_MODEL_DEPLOYMENT_MODEL=Gemma 4 E4B\nDOTCORE_MODEL_DEPLOYMENT_PROFILE=default\nDOTCORE_MODEL_DEPLOYMENT_ENGINE=llama-cpp\nDOTCORE_MODEL_DEPLOYMENT_PROGRESS=42%\nDOTCORE_MODEL_DEPLOYMENT_FAILURE=\nDOTCORE_MODEL_DEPLOYMENT_MESSAGE=Fetching weights\n"
    );
    assert_eq!(
        text(&failed.stdout),
        "Model deployment\nDOTCORE_MODEL_DEPLOYMENT_ID=model-3\nDOTCORE_MODEL_DEPLOYMENT_NAME=Llama\nDOTCORE_MODEL_DEPLOYMENT_STATE=Failed\nDOTCORE_MODEL_DEPLOYMENT_DESIRED_STATE=Present\nDOTCORE_MODEL_DEPLOYMENT_NODE=gb10-host\nDOTCORE_MODEL_DEPLOYMENT_EXECUTION_TARGET=node-1-model\nDOTCORE_MODEL_DEPLOYMENT_MODEL=Gemma 4 E4B\nDOTCORE_MODEL_DEPLOYMENT_PROFILE=default\nDOTCORE_MODEL_DEPLOYMENT_ENGINE=llama-cpp\nDOTCORE_MODEL_DEPLOYMENT_PROGRESS=\nDOTCORE_MODEL_DEPLOYMENT_FAILURE=Out of memory on the GPU\nDOTCORE_MODEL_DEPLOYMENT_MESSAGE=\n"
    );
}

#[test]
fn show_applies_the_console_state_overrides() {
    let manager = FakeManager::start(FakeState::with_models(mixed_deployments()));

    let paused = run(&manager, &["show", "Mistral"]);
    let tearing_down = run(&manager, &["show", "Phi"]);

    assert!(
        text(&paused.stdout).contains(
            "DOTCORE_MODEL_DEPLOYMENT_STATE=Paused\nDOTCORE_MODEL_DEPLOYMENT_DESIRED_STATE=Paused\n"
        ),
        "{}",
        text(&paused.stdout)
    );
    assert!(
        text(&tearing_down.stdout).contains(
            "DOTCORE_MODEL_DEPLOYMENT_STATE=Tearing down\nDOTCORE_MODEL_DEPLOYMENT_DESIRED_STATE=Teardown\n"
        ),
        "{}",
        text(&tearing_down.stdout)
    );
}

#[test]
fn show_hides_the_status_message_that_a_requested_change_overrides() {
    let mut tearing_down = local_model_deployment("model-6", "Old", Ready, Teardown);
    tearing_down
        .state
        .as_mut()
        .expect("seeded state")
        .status_message = "Serving requests".to_owned();
    let mut pausing = local_model_deployment("model-7", "Slow", Downloading, Paused);
    pausing.state.as_mut().expect("seeded state").status_message = "Fetching weights".to_owned();
    let manager = FakeManager::start(FakeState::with_models(vec![tearing_down, pausing]));

    let tearing_down = run(&manager, &["show", "Old"]);
    let pausing = run(&manager, &["show", "Slow"]);

    assert_eq!(
        text(&tearing_down.stdout),
        "Model deployment\nDOTCORE_MODEL_DEPLOYMENT_ID=model-6\nDOTCORE_MODEL_DEPLOYMENT_NAME=Old\nDOTCORE_MODEL_DEPLOYMENT_STATE=Tearing down\nDOTCORE_MODEL_DEPLOYMENT_DESIRED_STATE=Teardown\nDOTCORE_MODEL_DEPLOYMENT_NODE=gb10-host\nDOTCORE_MODEL_DEPLOYMENT_EXECUTION_TARGET=node-1-model\nDOTCORE_MODEL_DEPLOYMENT_MODEL=Gemma 4 E4B\nDOTCORE_MODEL_DEPLOYMENT_PROFILE=default\nDOTCORE_MODEL_DEPLOYMENT_ENGINE=llama-cpp\nDOTCORE_MODEL_DEPLOYMENT_PROGRESS=\nDOTCORE_MODEL_DEPLOYMENT_FAILURE=\nDOTCORE_MODEL_DEPLOYMENT_MESSAGE=\n"
    );
    assert_eq!(
        text(&pausing.stdout),
        "Model deployment\nDOTCORE_MODEL_DEPLOYMENT_ID=model-7\nDOTCORE_MODEL_DEPLOYMENT_NAME=Slow\nDOTCORE_MODEL_DEPLOYMENT_STATE=Pausing\nDOTCORE_MODEL_DEPLOYMENT_DESIRED_STATE=Paused\nDOTCORE_MODEL_DEPLOYMENT_NODE=gb10-host\nDOTCORE_MODEL_DEPLOYMENT_EXECUTION_TARGET=node-1-model\nDOTCORE_MODEL_DEPLOYMENT_MODEL=Gemma 4 E4B\nDOTCORE_MODEL_DEPLOYMENT_PROFILE=default\nDOTCORE_MODEL_DEPLOYMENT_ENGINE=llama-cpp\nDOTCORE_MODEL_DEPLOYMENT_PROGRESS=\nDOTCORE_MODEL_DEPLOYMENT_FAILURE=\nDOTCORE_MODEL_DEPLOYMENT_MESSAGE=\n"
    );
}

#[test]
fn an_unknown_or_ambiguous_reference_fails_with_exit_code_1() {
    let mut deployments = mixed_deployments();
    deployments.push(local_model_deployment("model-6", "Gemma", Ready, Present));
    let manager = FakeManager::start(FakeState::with_models(deployments));

    let unknown = run(&manager, &["show", "nothing"]);
    let ambiguous = run(&manager, &["teardown", "Gemma", "--yes"]);

    assert_eq!(unknown.status.code(), Some(1));
    assert_eq!(text(&unknown.stdout), "");
    assert_eq!(
        text(&unknown.stderr),
        "no model deployment is named or has id \"nothing\"; see `dotc model list`\n"
    );
    assert_eq!(ambiguous.status.code(), Some(1));
    assert_eq!(text(&ambiguous.stdout), "");
    assert_eq!(
        text(&ambiguous.stderr),
        "2 model deployments are named \"Gemma\": model-1, model-6; pass one of the ids instead\n"
    );
    assert!(
        manager
            .calls()
            .iter()
            .all(|call| call.rpc != Rpc::RemoveModelDeployment),
        "an ambiguous teardown must not reach the manager"
    );
}

#[test]
fn an_unreachable_manager_fails_with_exit_code_1() {
    let reserved = dotcore::testing::reserve_port();
    let address = std::net::SocketAddr::from(([127, 0, 0, 1], reserved.port()));

    let output = dotc_at("model", &address.to_string(), &["list"])
        .output()
        .expect("run dotc model list");

    assert_eq!(output.status.code(), Some(1));
    assert_eq!(text(&output.stdout), "");
    let stderr = text(&output.stderr);
    assert!(
        stderr.starts_with(&format!("cannot reach the manager at http://{address}")),
        "{stderr}"
    );
}

#[test]
fn commands_need_no_site_folder() {
    let manager = FakeManager::start(FakeState::with_models(mixed_deployments()));
    let empty = support::canonical_tempdir().expect("empty working directory");

    let mut command = dotc_at("model", &manager.address().to_string(), &["list"]);
    command.current_dir(empty.path());
    let output = command.output().expect("run dotc model list");

    assert_eq!(output.status.code(), Some(0), "{}", text(&output.stderr));
    assert!(text(&output.stdout).contains("DOTCORE_MODEL_DEPLOYMENT_ID=model-1\n"));
    assert_eq!(
        std::fs::read_dir(empty.path())
            .expect("read working directory")
            .count(),
        0
    );
}

#[test]
fn the_manager_flag_overrides_the_environment_address() {
    let manager = FakeManager::start(FakeState::with_models(mixed_deployments()));
    let mut command = isolated_dotc();
    command
        .args(["model", "list", "--manager"])
        .arg(format!("http://{}", manager.address()))
        .env("DOTCORE_MANAGEMENT_ADDR", "127.0.0.1:1");
    detach_from_controlling_terminal(&mut command);

    let output = command.output().expect("run dotc model list");

    assert_eq!(output.status.code(), Some(0), "{}", text(&output.stderr));
    assert!(text(&output.stdout).contains("DOTCORE_MODEL_DEPLOYMENT_ID=model-5\n"));
}

#[test]
fn help_lists_the_six_verbs_and_the_manager_flag() {
    let group = isolated_dotc()
        .args(["model", "--help"])
        .output()
        .expect("run help");
    let list = isolated_dotc()
        .args(["model", "list", "--help"])
        .output()
        .expect("run help");

    assert_eq!(group.status.code(), Some(0));
    let group = text(&group.stdout);
    for verb in ["list", "show", "pause", "resume", "teardown", "logs"] {
        assert!(
            group
                .lines()
                .any(|line| line.trim_start().starts_with(verb)),
            "{verb}\n{group}"
        );
    }
    let list = text(&list.stdout);
    assert!(list.contains("--manager <ADDRESS>"), "{list}");
    assert!(list.contains("DOTCORE_MANAGEMENT_ADDR"), "{list}");
    assert!(list.lines().count() < 20, "{list}");
}

#[test]
fn pause_and_resume_request_the_change_and_print_the_accepted_state() {
    let manager = FakeManager::start(FakeState::with_models(mixed_deployments()));
    assert_eq!(manager.model_deployment("model-1").desired_state(), Present);

    let paused = run(&manager, &["pause", "Gemma"]);

    assert_eq!(
        text(&paused.stderr),
        "Notice: Run `dotc model list` to follow its progress.\n"
    );
    assert_eq!(paused.status.code(), Some(0));
    assert_eq!(
        text(&paused.stdout),
        "Model deployment\nDOTCORE_MODEL_DEPLOYMENT_ID=model-1\nDOTCORE_MODEL_DEPLOYMENT_NAME=Gemma\nDOTCORE_MODEL_DEPLOYMENT_STATE=Pausing\nDOTCORE_MODEL_DEPLOYMENT_DESIRED_STATE=Paused\n"
    );
    assert_eq!(manager.model_deployment("model-1").desired_state(), Paused);

    let resumed = run(&manager, &["resume", "model-4"]);

    assert_eq!(resumed.status.code(), Some(0));
    assert_eq!(
        text(&resumed.stdout),
        "Model deployment\nDOTCORE_MODEL_DEPLOYMENT_ID=model-4\nDOTCORE_MODEL_DEPLOYMENT_NAME=Mistral\nDOTCORE_MODEL_DEPLOYMENT_STATE=Paused\nDOTCORE_MODEL_DEPLOYMENT_DESIRED_STATE=Present\n"
    );
    assert_eq!(manager.model_deployment("model-4").desired_state(), Present);
    assert_eq!(
        lifecycle_calls(&manager),
        vec![
            call(Rpc::PauseModel, Some("model-1")),
            call(Rpc::ResumeModel, Some("model-4")),
        ]
    );
}

#[test]
fn teardown_with_yes_removes_the_deployment_and_prints_the_accepted_state() {
    let manager = FakeManager::start(FakeState::with_models(mixed_deployments()));

    let output = run(&manager, &["teardown", "Llama", "--yes"]);

    assert_eq!(output.status.code(), Some(0));
    assert_eq!(
        text(&output.stdout),
        "Model deployment\nDOTCORE_MODEL_DEPLOYMENT_ID=model-3\nDOTCORE_MODEL_DEPLOYMENT_NAME=Llama\nDOTCORE_MODEL_DEPLOYMENT_STATE=Tearing down\nDOTCORE_MODEL_DEPLOYMENT_DESIRED_STATE=Teardown\n"
    );
    assert_eq!(
        manager.model_deployment("model-3").desired_state(),
        Teardown
    );
    assert_eq!(
        lifecycle_calls(&manager),
        vec![call(Rpc::RemoveModelDeployment, Some("model-3"))]
    );
}

#[test]
fn teardown_without_a_terminal_and_without_yes_fails_before_calling_the_manager() {
    let manager = FakeManager::start(FakeState::with_models(mixed_deployments()));

    let output = run(&manager, &["teardown", "Llama"]);

    assert_eq!(output.status.code(), Some(1));
    assert_eq!(text(&output.stdout), "");
    assert_eq!(
        text(&output.stderr),
        "tearing down a model deployment needs a terminal; run in a terminal or pass --yes to tear it down without confirmation\n"
    );
    assert_eq!(manager.calls(), Vec::new());
}

#[test]
fn a_refused_request_fails_with_the_manager_message_and_changes_nothing() {
    let manager = FakeManager::start(FakeState::with_models(mixed_deployments()));
    manager.state().scripted.insert(
        Rpc::PauseModel,
        Scripted::Fail(Status::failed_precondition(
            "model deployment is not in a state that allows this request",
        )),
    );

    let output = run(&manager, &["pause", "Phi"]);

    assert_eq!(output.status.code(), Some(1));
    assert_eq!(text(&output.stdout), "");
    assert_eq!(
        text(&output.stderr),
        "cannot pause Phi: the manager returned FailedPrecondition: model deployment is not in a state that allows this request; see `dotc model list`\n"
    );
    assert_eq!(
        manager.model_deployment("model-5").desired_state(),
        Teardown
    );
}

#[test]
fn an_answer_without_the_asked_desired_state_is_not_accepted() {
    let manager = FakeManager::start(FakeState::with_models(mixed_deployments()));
    manager
        .state()
        .scripted
        .insert(Rpc::PauseModel, Scripted::Unchanged);

    let output = run(&manager, &["pause", "Gemma"]);

    assert_eq!(output.status.code(), Some(1));
    assert_eq!(text(&output.stdout), "");
    assert_eq!(
        text(&output.stderr),
        "the manager did not accept the request to pause Gemma; its desired state is Present; see `dotc model list`\n"
    );
    assert_eq!(manager.model_deployment("model-1").desired_state(), Present);
}

#[test]
fn logs_print_the_recent_lines_with_their_source_and_report_unavailable_sources() {
    let manager = FakeManager::start(FakeState::with_models(mixed_deployments()));
    manager.state().model_logs.page = log_page(
        vec![
            log_source("server", DeploymentLogSourceState::Available),
            log_source("proxy", DeploymentLogSourceState::Unavailable),
        ],
        vec![
            log_entry("server", 10, b"loading weights"),
            log_entry("proxy", 11, b"listening"),
        ],
    );

    let output = run(&manager, &["logs", "Gemma", "--lines", "20"]);

    assert_eq!(text(&output.stderr), "Proxy (proxy): unavailable\n");
    assert_eq!(output.status.code(), Some(0));
    assert_eq!(
        text(&output.stdout),
        "[Server (server)] loading weights\n[Proxy (proxy)] listening\n"
    );
    let logs = &manager.state().model_logs;
    assert_eq!(logs.reads.len(), 1);
    assert_eq!(logs.reads[0].model_deployment_id, "model-1");
    assert_eq!(logs.reads[0].page_size, 20);
    assert!(logs.reads[0].source_names.is_empty());
    assert!(logs.follows.is_empty());
}

#[test]
fn logs_of_one_source_print_bare_lines_with_timestamps_and_keep_their_bytes() {
    let manager = FakeManager::start(FakeState::with_models(mixed_deployments()));
    manager.state().model_logs.page = log_page(
        vec![log_source("server", DeploymentLogSourceState::Available)],
        vec![
            log_entry("server", 10, b"\x1b[32mready\x1b[0m"),
            log_entry("server", 11, b"bad \xff byte"),
        ],
    );

    let output = run(
        &manager,
        &["logs", "model-1", "--source", "server", "--timestamps"],
    );

    assert_eq!(text(&output.stderr), "");
    assert_eq!(output.status.code(), Some(0));
    assert_eq!(
        text(&output.stdout),
        "2026-11-12T16:13:30Z \x1b[32mready\x1b[0m\n2026-11-12T16:13:31Z bad \u{fffd} byte\n"
    );
    let logs = &manager.state().model_logs;
    assert_eq!(logs.reads[0].source_names, ["server"]);
    assert_eq!(logs.reads[0].page_size, 100);
}

#[test]
fn logs_with_follow_continue_after_the_read_and_report_source_changes() {
    let manager = FakeManager::start(FakeState::with_models(mixed_deployments()));
    {
        let mut state = manager.state();
        state.model_logs.page = log_page(
            vec![
                log_source("server", DeploymentLogSourceState::Available),
                log_source("proxy", DeploymentLogSourceState::Unavailable),
            ],
            vec![log_entry("server", 10, b"loading weights")],
        );
        state.model_logs.follow = vec![
            Ok(follow_entry("server", 12, b"ready")),
            Ok(follow_source("proxy", DeploymentLogSourceState::Available)),
            Ok(follow_entry("proxy", 13, b"listening")),
            Ok(follow_source("server", DeploymentLogSourceState::Ended)),
        ];
    }

    let output = run(&manager, &["logs", "Gemma", "--follow"]);

    assert_eq!(
        text(&output.stderr),
        "Proxy (proxy): unavailable\nProxy (proxy): available\nServer (server): ended\nthe log follow of model deployment Gemma ended\n"
    );
    assert_eq!(output.status.code(), Some(0));
    assert_eq!(
        text(&output.stdout),
        "[Server (server)] loading weights\n[Server (server)] ready\n[Proxy (proxy)] listening\n"
    );
    let logs = &manager.state().model_logs;
    assert_eq!(logs.follows.len(), 1);
    assert_eq!(logs.follows[0].model_deployment_id, "model-1");
    // Only the source that had lines resumes after them; the other one starts fresh.
    assert_eq!(
        logs.follows[0].resume_positions,
        [DeploymentLogResumePosition {
            source_name: "server".to_owned(),
            latest_timestamp: Some(timestamp(10)),
        }]
    );
}

#[test]
fn a_follow_the_manager_ends_with_an_error_fails() {
    let manager = FakeManager::start(FakeState::with_models(mixed_deployments()));
    {
        let mut state = manager.state();
        state.model_logs.page = log_page(
            vec![log_source("server", DeploymentLogSourceState::Available)],
            Vec::new(),
        );
        state.model_logs.follow = vec![
            Ok(follow_entry("server", 12, b"ready")),
            Err(Status::unavailable(
                "Deployment logs are unavailable right now; retry shortly",
            )),
        ];
    }

    let output = run(&manager, &["logs", "Gemma", "--follow"]);

    assert_eq!(output.status.code(), Some(1));
    assert_eq!(text(&output.stdout), "ready\n");
    assert_eq!(
        text(&output.stderr),
        "cannot read the logs of model deployment Gemma: the manager returned Unavailable: Deployment logs are unavailable right now; retry shortly\n"
    );
}

#[test]
fn logs_stop_quietly_without_following_once_the_reader_of_the_lines_is_gone() {
    let manager = FakeManager::start(FakeState::with_models(mixed_deployments()));
    {
        let mut state = manager.state();
        state.model_logs.page = log_page(
            vec![log_source("server", DeploymentLogSourceState::Available)],
            vec![log_entry("server", 10, b"loading weights")],
        );
        state.model_logs.follow = vec![Ok(follow_entry("server", 12, b"ready"))];
    }
    let (reader, writer) = std::io::pipe().expect("create the stdout pipe");
    drop(reader);

    let output = dotc_at(
        "model",
        &manager.address().to_string(),
        &["logs", "Gemma", "--follow"],
    )
    .stdout(writer)
    .output()
    .expect("run dotc model");

    assert_eq!(text(&output.stderr), "");
    assert_eq!(output.status.code(), Some(0));
    let logs = &manager.state().model_logs;
    assert_eq!(logs.reads.len(), 1);
    assert!(logs.follows.is_empty());
}

#[test]
fn logs_fail_without_panicking_when_stderr_is_closed() {
    let manager = FakeManager::start(FakeState::with_models(mixed_deployments()));
    manager.state().model_logs.page = log_page(
        vec![log_source("server", DeploymentLogSourceState::Unavailable)],
        Vec::new(),
    );
    let (reader, writer) = std::io::pipe().expect("create the stderr pipe");
    drop(reader);

    let output = dotc_at("model", &manager.address().to_string(), &["logs", "Gemma"])
        .stderr(writer)
        .output()
        .expect("run dotc model");

    assert_eq!(output.status.code(), Some(1));
    assert_eq!(text(&output.stdout), "");
}

#[test]
fn logs_fail_when_every_source_is_unavailable_or_the_node_cannot_be_reached() {
    let manager = FakeManager::start(FakeState::with_models(mixed_deployments()));
    manager.state().model_logs.page = log_page(
        vec![log_source("server", DeploymentLogSourceState::Unavailable)],
        Vec::new(),
    );

    let unavailable = run(&manager, &["logs", "Gemma"]);

    assert_eq!(unavailable.status.code(), Some(1));
    assert_eq!(text(&unavailable.stdout), "");
    assert_eq!(
        text(&unavailable.stderr),
        "Server (server): unavailable\nevery log source of model deployment Gemma is unavailable; its node may be unreachable\n"
    );

    manager.state().scripted.insert(
        Rpc::ReadModelLogs,
        Scripted::Fail(Status::unavailable(
            "Deployment logs are unavailable right now; retry shortly",
        )),
    );
    let unreachable = run(&manager, &["logs", "Gemma"]);

    assert_eq!(unreachable.status.code(), Some(1));
    assert_eq!(
        text(&unreachable.stderr),
        "cannot read the logs of model deployment Gemma: the manager returned Unavailable: Deployment logs are unavailable right now; retry shortly\n"
    );
}

#[test]
fn logs_of_an_unknown_source_fail_with_the_manager_reason_and_a_hint() {
    let manager = FakeManager::start(FakeState::with_models(mixed_deployments()));
    manager.state().scripted.insert(
        Rpc::ReadModelLogs,
        Scripted::Fail(Status::invalid_argument(
            "Deployment log request contains an invalid source",
        )),
    );

    let output = run(&manager, &["logs", "Gemma", "--source", "nope"]);

    assert_eq!(output.status.code(), Some(1));
    assert_eq!(text(&output.stdout), "");
    assert_eq!(
        text(&output.stderr),
        "cannot read the logs of model deployment Gemma: the manager returned InvalidArgument: Deployment log request contains an invalid source; run the command without --source to see the source of each line\n"
    );
}

#[test]
fn logs_of_an_unknown_deployment_fail_before_reading_any_log() {
    let manager = FakeManager::start(FakeState::with_models(mixed_deployments()));

    let output = run(&manager, &["logs", "missing"]);

    assert_eq!(output.status.code(), Some(1));
    assert_eq!(
        text(&output.stderr),
        "no model deployment is named or has id \"missing\"; see `dotc model list`\n"
    );
    assert!(manager.state().model_logs.reads.is_empty());
}

#[test]
fn a_terminal_shows_the_list_as_a_table() {
    let manager = FakeManager::start(FakeState::with_models(mixed_deployments()));

    let Some(run) = run_in_pty(terminal_command(&manager, &["list"]), Session::new([])) else {
        return;
    };

    assert_eq!(run.code(), Some(0), "screen:\n{}", run.screen());
    assert_eq!(run.stdout(), "");
    assert_eq!(run.stderr(), "");
    for expected in [
        "NAME     ID       STATE         NODE       MODEL",
        "Gemma    model-1  Ready         gb10-host  Gemma 4 E4B",
        "Qwen     model-2  Downloading   gb10-host  Gemma 4 E4B",
        "Llama    model-3  Failed        gb10-host  Gemma 4 E4B",
        "Mistral  model-4  Paused        gb10-host  Gemma 4 E4B",
        "Phi      model-5  Tearing down  gb10-host  Gemma 4 E4B",
    ] {
        assert!(
            run.screen().lines().any(|line| line.trim() == expected),
            "{expected}\n{}",
            run.screen()
        );
    }
}

#[test]
fn a_terminal_shows_the_details_of_one_deployment() {
    let manager = FakeManager::start(FakeState::with_models(mixed_deployments()));

    let Some(run) = run_in_pty(
        terminal_command(&manager, &["show", "Qwen"]),
        Session::new([]),
    ) else {
        return;
    };

    assert_eq!(run.code(), Some(0), "screen:\n{}", run.screen());
    assert_eq!(run.stdout(), "");
    let lines = run.screen().lines().map(str::trim).collect::<Vec<_>>();
    for expected in [
        "Model deployment Qwen",
        "name       Qwen",
        "id         model-2",
        "state      Downloading",
        "desired    Present",
        "node       gb10-host",
        "target     Simulated models",
        "target id  node-1-model",
        "model      Gemma 4 E4B",
        "profile    default",
        "engine     llama-cpp",
        "progress   42%",
        "message    Fetching weights",
    ] {
        assert!(lines.contains(&expected), "{expected}\n{}", run.screen());
    }
    assert!(!lines.iter().any(|line| line.starts_with("failure")));
}

#[test]
fn a_terminal_reports_a_pause_with_the_next_step() {
    let manager = FakeManager::start(FakeState::with_models(mixed_deployments()));

    let Some(run) = run_in_pty(
        terminal_command(&manager, &["pause", "Gemma"]),
        Session::new([]),
    ) else {
        return;
    };

    assert_eq!(run.code(), Some(0), "screen:\n{}", run.screen());
    assert_eq!(run.stdout(), "");
    assert_eq!(run.stderr(), "");
    let lines = run.screen().lines().map(str::trim).collect::<Vec<_>>();
    assert!(lines.contains(&"OK Pausing Gemma"), "{}", run.screen());
    assert!(lines.contains(&"dotc model list"), "{}", run.screen());
    assert_eq!(manager.model_deployment("model-1").desired_state(), Paused);
}

#[test]
fn a_terminal_teardown_asks_and_removes_when_accepted() {
    let manager = FakeManager::start(FakeState::with_models(mixed_deployments()));

    let Some(run) = run_in_pty(
        terminal_command(&manager, &["teardown", "Gemma"]),
        Session::new([
            Step::Wait(
                "Tear down Gemma? This removes the deployment and its running workload resources. [no]",
            ),
            Step::Send(b"yes\n"),
            Step::Wait("Tearing down Gemma"),
        ]),
    ) else {
        return;
    };

    assert_eq!(run.code(), Some(0), "screen:\n{}", run.screen());
    assert_eq!(run.stdout(), "");
    assert_eq!(run.stderr(), "");
    assert_eq!(
        manager.model_deployment("model-1").desired_state(),
        Teardown
    );
}

#[test]
fn a_terminal_teardown_cancels_by_default_with_exit_code_130() {
    let manager = FakeManager::start(FakeState::with_models(mixed_deployments()));

    let Some(run) = run_in_pty(
        terminal_command(&manager, &["teardown", "Gemma"]),
        Session::new([Step::Wait("Tear down Gemma?"), Step::Send(b"\n")]),
    ) else {
        return;
    };

    assert_eq!(run.code(), Some(130), "screen:\n{}", run.screen());
    assert_eq!(run.stdout(), "");
    assert_eq!(manager.model_deployment("model-1").desired_state(), Present);
    assert_eq!(
        lifecycle_calls(&manager),
        Vec::new(),
        "a cancelled teardown must not reach the manager"
    );
}

#[test]
fn a_terminal_shows_a_failure_as_a_verdict_with_next_steps() {
    let manager = FakeManager::start(FakeState::with_models(mixed_deployments()));

    let Some(run) = run_in_pty(
        terminal_command(&manager, &["show", "nothing"]),
        Session::new([]).with_stderr_on_terminal(),
    ) else {
        return;
    };

    assert_eq!(run.code(), Some(1), "screen:\n{}", run.screen());
    let lines = run.screen().lines().map(str::trim).collect::<Vec<_>>();
    for expected in [
        "x No model deployment matches nothing",
        "> List the model deployments:",
        "dotc model list",
    ] {
        assert!(lines.contains(&expected), "{expected}\n{}", run.screen());
    }
}

#[test]
fn show_prints_visible_parameters_in_schema_order_with_secrets_masked() {
    let manager = FakeManager::start(FakeState::with_models(vec![deployment_with_parameters()]));

    let output = run(&manager, &["show", "Gemma"]);

    assert_eq!(text(&output.stderr), "");
    assert_eq!(output.status.code(), Some(0));
    let stdout = text(&output.stdout);
    assert!(!stdout.contains("s3cretKey"), "{stdout}");
    assert_eq!(parameter_lines(&stdout), PARAMETER_LINES_MASKED);
}

#[test]
fn show_reveals_secret_parameters_with_the_flag() {
    let manager = FakeManager::start(FakeState::with_models(vec![deployment_with_parameters()]));

    let output = run(&manager, &["show", "Gemma", "--reveal-secrets"]);

    assert_eq!(output.status.code(), Some(0));
    let revealed = PARAMETER_LINES_MASKED.map(|line| line.replace("********", "s3cretKey"));
    assert_eq!(parameter_lines(&text(&output.stdout)), revealed);
}

#[test]
fn show_prints_the_upstream_api_key_masked_unless_revealed() {
    let mut upstream = local_model_deployment("model-9", "Remote", Ready, Present);
    upstream.snapshot = Some(ModelDeploymentSnapshot {
        source: Some(Source::Upstream(Default::default())),
    });
    upstream.upstream_desired_configuration = Some(UpstreamModelDesiredConfiguration {
        api_key: "sk-remote".to_owned(),
        ..Default::default()
    });
    let manager = FakeManager::start(FakeState::with_models(vec![upstream]));

    let masked = text(&run(&manager, &["show", "Remote"]).stdout);
    let revealed = text(&run(&manager, &["show", "Remote", "--reveal-secrets"]).stdout);

    assert!(!masked.contains("sk-remote"), "{masked}");
    assert_eq!(
        parameter_lines(&masked),
        ["DOTCORE_MODEL_DEPLOYMENT_PARAMETER_API_KEY=********"]
    );
    assert_eq!(
        parameter_lines(&revealed),
        ["DOTCORE_MODEL_DEPLOYMENT_PARAMETER_API_KEY=sk-remote"]
    );
}

#[test]
fn help_describes_the_reveal_flag() {
    let manager = FakeManager::start(FakeState::with_models(Vec::new()));

    let output = run(&manager, &["show", "--help"]);

    let help = text(&output.stdout);
    let line = help
        .lines()
        .find(|line| line.contains("--reveal-secrets"))
        .unwrap_or_else(|| panic!("no flag in help\n{help}"));
    assert_eq!(
        line.split_whitespace().collect::<Vec<_>>().join(" "),
        "--reveal-secrets Print secret parameter values instead of masking them"
    );
}

#[test]
fn a_terminal_shows_parameters_under_their_own_heading() {
    let manager = FakeManager::start(FakeState::with_models(vec![deployment_with_parameters()]));

    let Some(run) = run_in_pty(
        terminal_command(&manager, &["show", "Gemma"]),
        Session::new([]),
    ) else {
        return;
    };

    assert_eq!(run.code(), Some(0), "screen:\n{}", run.screen());
    let lines = run.screen().lines().map(str::trim).collect::<Vec<_>>();
    let start = lines
        .iter()
        .position(|line| *line == "Parameters")
        .unwrap_or_else(|| panic!("no Parameters heading\n{}", run.screen()));
    let rows = lines[start + 1..]
        .iter()
        .map(|line| line.split_whitespace().collect::<Vec<_>>().join(" "))
        .filter(|line| !line.is_empty())
        .collect::<Vec<_>>();
    assert_eq!(
        rows,
        [
            "Flash attention true",
            "Parallel requests 4",
            "Expert cap 24576 MiB",
            "Tuned flag false",
            "API key ********",
        ]
    );
}

const PARAMETER_LINES_MASKED: [&str; 5] = [
    "DOTCORE_MODEL_DEPLOYMENT_PARAMETER_FLASH_ATTENTION=true",
    "DOTCORE_MODEL_DEPLOYMENT_PARAMETER_PARALLEL_REQUESTS=4",
    "DOTCORE_MODEL_DEPLOYMENT_PARAMETER_EXPERT_CAP=24576 MiB",
    "DOTCORE_MODEL_DEPLOYMENT_PARAMETER_TUNED_FLAG=false",
    "DOTCORE_MODEL_DEPLOYMENT_PARAMETER_API_KEY=********",
];

/// The parameter lines of a record block.
fn parameter_lines(stdout: &str) -> Vec<&str> {
    stdout
        .lines()
        .filter(|line| line.starts_with("DOTCORE_MODEL_DEPLOYMENT_PARAMETER_"))
        .collect()
}

/// A local deployment whose schema has a default flag, a plain integer, a sized integer, a flag
/// the profile makes visible, a secret and two hidden parameters (one hidden by the
/// engine, one by the profile).
fn deployment_with_parameters() -> ModelDeployment {
    let definition = |id: &str, name: &str, visibility, parameter_type| ParameterDefinition {
        parameter_id: id.to_owned(),
        display_name: name.to_owned(),
        visibility: visibility as i32,
        parameter_type: Some(parameter_type),
        ..Default::default()
    };
    let integer = |default_value| {
        ParameterType::UnsignedInteger(UnsignedIntegerParameterSchema {
            minimum: 1,
            maximum: 100_000,
            unit: String::new(),
            default_value,
        })
    };
    let flag = |default_value| ParameterType::Boolean(BooleanParameterSchema { default_value });
    let value = |id: &str, value| ParameterValue {
        parameter_id: id.to_owned(),
        value: Some(value),
    };
    use ParameterVisibility::{Advanced, Basic, Hidden};

    let mut deployment = local_model_deployment("model-1", "Gemma", Ready, Present);
    let Some(Source::Local(local)) = deployment
        .snapshot
        .as_mut()
        .and_then(|snapshot| snapshot.source.as_mut())
    else {
        unreachable!("seeded local snapshot");
    };
    let mut expert_cap = definition("expert-cap", "Expert cap", Advanced, integer(None));
    if let Some(ParameterType::UnsignedInteger(schema)) = expert_cap.parameter_type.as_mut() {
        schema.unit = "MiB".to_owned();
    }
    local.engine_parameter_schema = Some(EngineParameterSchema {
        parameters: vec![
            definition(
                "flash-attention",
                "Flash attention",
                Basic,
                flag(Some(false)),
            ),
            definition(
                "parallel-requests",
                "Parallel requests",
                Basic,
                integer(Some(1)),
            ),
            expert_cap,
            definition("engine-hidden", "Engine hidden", Hidden, integer(Some(7))),
            definition("profile-hidden", "Profile hidden", Basic, integer(Some(8))),
            definition("tuned-flag", "Tuned flag", Hidden, flag(Some(true))),
            definition(
                "api-key",
                "API key",
                Basic,
                ParameterType::Secret(SecretParameterSchema {
                    generated_length: 32,
                    minimum_length: 8,
                }),
            ),
        ],
        ..Default::default()
    });
    local.engine_parameter_values = Some(ParameterValues {
        values: vec![
            value("flash-attention", Value::Boolean(true)),
            value("parallel-requests", Value::UnsignedInteger(4)),
            value("expert-cap", Value::UnsignedInteger(24576)),
            value("engine-hidden", Value::UnsignedInteger(7)),
            value("profile-hidden", Value::UnsignedInteger(8)),
            value("tuned-flag", Value::Boolean(false)),
            value("api-key", Value::String("s3cretKey".to_owned())),
        ],
    });
    local.profile_engine_parameter_defaults = Some(ParameterValues {
        values: vec![
            value("flash-attention", Value::Boolean(true)),
            value("parallel-requests", Value::UnsignedInteger(1)),
            value("tuned-flag", Value::Boolean(true)),
        ],
    });
    local.profile_engine_parameter_visibility_overrides = vec![
        ParameterVisibilityOverride {
            parameter_id: "profile-hidden".to_owned(),
            visibility: Hidden as i32,
        },
        ParameterVisibilityOverride {
            parameter_id: "tuned-flag".to_owned(),
            visibility: Basic as i32,
        },
    ];
    local.sizing = Some(ResolvedSizing {
        derived_engine_parameter_ids: vec!["expert-cap".to_owned()],
        ..Default::default()
    });
    deployment
}

/// Seeds one deployment in each state the commands present differently.
fn mixed_deployments() -> Vec<ModelDeployment> {
    let mut downloading = local_model_deployment("model-2", "Qwen", Downloading, Present);
    let state = downloading.state.as_mut().expect("seeded state");
    state.status_message = "Fetching weights".to_owned();
    state.progress = Some(ModelDeploymentProgress {
        percent: Some(42),
        ..Default::default()
    });
    let mut failed = local_model_deployment("model-3", "Llama", Failed, Present);
    failed.state.as_mut().expect("seeded state").failure = Some(ModelDeploymentFailure {
        code: "out_of_memory".to_owned(),
        message: "Out of memory on the GPU".to_owned(),
        diagnostics: Vec::new(),
    });

    vec![
        local_model_deployment("model-1", "Gemma", Ready, Present),
        downloading,
        failed,
        local_model_deployment("model-4", "Mistral", ModelDeploymentStatus::Paused, Paused),
        local_model_deployment("model-5", "Phi", Ready, Teardown),
    ]
}

fn log_page(
    sources: Vec<DeploymentLogSource>,
    entries: Vec<DeploymentLogEntry>,
) -> ReadDeploymentLogsResponse {
    let mut latest: Vec<DeploymentLogResumePosition> = Vec::new();
    for entry in &entries {
        latest.retain(|position| position.source_name != entry.source_name);
        latest.push(DeploymentLogResumePosition {
            source_name: entry.source_name.clone(),
            latest_timestamp: entry.timestamp,
        });
    }
    ReadDeploymentLogsResponse {
        sources,
        entries,
        latest_positions: latest,
        ..Default::default()
    }
}

fn log_source(name: &str, state: DeploymentLogSourceState) -> DeploymentLogSource {
    DeploymentLogSource {
        source_name: name.to_owned(),
        source_label: label_of(name),
        state: state as i32,
    }
}

/// A line of `source` written `second` seconds after 2026-11-12T16:13:20Z.
fn log_entry(source: &str, second: i64, content: &[u8]) -> DeploymentLogEntry {
    DeploymentLogEntry {
        source_name: source.to_owned(),
        source_label: label_of(source),
        timestamp: Some(timestamp(second)),
        content: content.to_vec(),
        ..Default::default()
    }
}

fn follow_entry(source: &str, second: i64, content: &[u8]) -> FollowDeploymentLogsResponse {
    FollowDeploymentLogsResponse {
        event: Some(Event::Entry(log_entry(source, second, content))),
    }
}

fn follow_source(name: &str, state: DeploymentLogSourceState) -> FollowDeploymentLogsResponse {
    FollowDeploymentLogsResponse {
        event: Some(Event::SourceEvent(DeploymentLogSourceEvent {
            source_name: name.to_owned(),
            source_label: label_of(name),
            state: state as i32,
        })),
    }
}

fn timestamp(second: i64) -> Timestamp {
    Timestamp {
        seconds: 1_794_500_000 + second,
        nanos: 0,
    }
}

fn call(rpc: Rpc, id: Option<&str>) -> Call {
    Call {
        rpc,
        id: id.map(str::to_owned),
    }
}

/// The lifecycle calls the manager received, without the reads that precede them.
fn lifecycle_calls(manager: &FakeManager) -> Vec<Call> {
    manager
        .calls()
        .into_iter()
        .filter(|call| {
            matches!(
                call.rpc,
                Rpc::PauseModel | Rpc::ResumeModel | Rpc::RemoveModelDeployment
            )
        })
        .collect()
}

/// Runs `dotc model <arguments>` against the fake without any terminal.
fn run(manager: &FakeManager, arguments: &[&str]) -> Output {
    dotc_at("model", &manager.address().to_string(), arguments)
        .output()
        .expect("run dotc model")
}

/// Builds `dotc model <arguments>` for a pseudo-terminal session with no `TERM`, which selects
/// the line prompts and the ASCII marks.
fn terminal_command(manager: &FakeManager, arguments: &[&str]) -> Command {
    let mut command = isolated_dotc();
    command
        .arg("model")
        .args(arguments)
        .env("DOTCORE_MANAGEMENT_ADDR", manager.address().to_string())
        .stdin(Stdio::null());
    command
}

fn text(value: &[u8]) -> String {
    String::from_utf8_lossy(value).into_owned()
}

/// The label the manager shows for a source, different from its name like real sources.
fn label_of(name: &str) -> String {
    let mut characters = name.chars();
    characters
        .next()
        .map(|first| first.to_uppercase().chain(characters).collect())
        .unwrap_or_default()
}
