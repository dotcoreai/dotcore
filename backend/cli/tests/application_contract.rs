//! Process-level contracts for `dotc application`, run against the in-process fake manager.

use std::process::{Command, Output, Stdio};

use dotcore_proto::dotcore::catalog::v1::{
    ApplicationGraph, BooleanParameterSchema, ParameterDefinition, ParameterGroup, ParameterValue,
    SecretParameterSchema, StoragePlacement, StringEnumParameterSchema, TextParameterSchema,
    UnsignedIntegerParameterSchema, parameter_definition::ParameterType, parameter_value::Value,
};
use dotcore_proto::{
    dotcore::application::v1::{
        ApplicationDeployment, ApplicationDeploymentDesiredState, ApplicationDeploymentFailure,
        ApplicationEndpoint, ApplicationLifecycleState, ApplicationReadinessState,
        ApplicationSelectedInferenceRoute, ApplicationServiceLifecycleState,
        ApplicationServiceObservation, ApplicationStorageAttachment, DeploymentLogEntry,
        DeploymentLogResumePosition, DeploymentLogSource, DeploymentLogSourceEvent,
        DeploymentLogSourceState, FollowDeploymentLogsResponse, ReadDeploymentLogsResponse,
        follow_deployment_logs_response::Event,
    },
    google::protobuf::Timestamp,
};
use serde_json::json;
use tonic::Status;

mod support;

use ApplicationDeploymentDesiredState::{Paused, Present, Teardown};
use ApplicationLifecycleState::{Failed, Ready, Running, Starting};

use support::{
    command::{detach_from_controlling_terminal, dotc_at, isolated_dotc},
    fake_manager::{Call, FakeManager, FakeState, Rpc, Scripted, local_application_deployment},
    pty::{Session, Step, run_in_pty},
};

#[test]
fn list_prints_a_table_with_one_row_per_deployment_in_every_state() {
    let manager = FakeManager::start(FakeState::with_applications(mixed_deployments()));

    let output = run(&manager, &["list"]);

    assert_eq!(text(&output.stderr), "");
    assert_eq!(output.status.code(), Some(0));
    assert_eq!(
        text(&output.stdout),
        "  NAME    ID     STATE            NODE       APPLICATION\n  Chat    app-1  Ready            gb10-host  Open WebUI\n  Notes   app-2  Starting         gb10-host  Open WebUI\n  Wiki    app-3  Failed           gb10-host  Open WebUI\n  Docs    app-4  Paused           gb10-host  Open WebUI\n  Old     app-5  Tearing down     gb10-host  Open WebUI\n  Mirror  app-6  Running (stale)  gb10-host  Open WebUI\n"
    );
    assert_eq!(
        manager.calls(),
        vec![
            call(Rpc::ListApplicationDeployments, None),
            call(Rpc::ListConfiguredExecutionTargets, None)
        ]
    );
}

#[test]
fn list_shows_the_node_id_when_the_manager_does_not_name_the_node() {
    let mut state = FakeState::with_applications(mixed_deployments());
    state.execution_targets.clear();
    let manager = FakeManager::start(state);

    let output = run(&manager, &["list"]);

    assert_eq!(output.status.code(), Some(0));
    assert!(
        text(&output.stdout).contains("  Chat    app-1  Ready            node-1  Open WebUI\n"),
        "{}",
        text(&output.stdout)
    );
}

#[test]
fn list_of_an_empty_site_says_there_are_no_deployments() {
    let manager = FakeManager::start(FakeState::with_applications(Vec::new()));

    let output = run(&manager, &["list"]);

    assert_eq!(output.status.code(), Some(0));
    assert_eq!(text(&output.stdout), "No application deployments\n");
    assert_eq!(text(&output.stderr), "");
}

#[test]
fn the_app_alias_runs_the_same_commands() {
    let manager = FakeManager::start(FakeState::with_applications(mixed_deployments()));

    let full = run(&manager, &["list"]);
    let mut command = isolated_dotc();
    command
        .args(["app", "list"])
        .env("DOTCORE_MANAGEMENT_ADDR", manager.address().to_string());
    detach_from_controlling_terminal(&mut command);
    let alias = command.output().expect("run dotc app list");

    assert_eq!(alias.status.code(), Some(0), "{}", text(&alias.stderr));
    assert_eq!(text(&alias.stdout), text(&full.stdout));
    assert_eq!(text(&alias.stderr), "");
}

#[test]
fn show_prints_every_key_for_a_running_deployment_with_its_endpoint() {
    let manager = FakeManager::start(FakeState::with_applications(mixed_deployments()));

    let output = run(&manager, &["show", "Chat"]);

    assert_eq!(text(&output.stderr), "");
    assert_eq!(output.status.code(), Some(0));
    assert_eq!(
        text(&output.stdout),
        "Application deployment Chat\n  name         Chat\n  id           app-1\n  state        Ready\n  desired      Present\n  readiness    Ready\n  node         gb10-host\n  target       Docker applications\n  target id    node-1-apps\n  application  Open WebUI\n  service web  Running\n  service db   Starting\n  endpoint     http://gb10-host:8080\n  route        gemma\n"
    );
}

#[test]
fn show_reports_stage_failure_and_stale_observation_by_id() {
    let manager = FakeManager::start(FakeState::with_applications(mixed_deployments()));

    let starting = run(&manager, &["show", "app-2"]);
    let failed = run(&manager, &["show", "app-3"]);
    let stale = run(&manager, &["show", "app-6"]);

    assert_eq!(
        text(&starting.stdout),
        "Application deployment Notes\n  name         Notes\n  id           app-2\n  state        Starting\n  desired      Present\n  node         gb10-host\n  target       Docker applications\n  target id    node-1-apps\n  application  Open WebUI\n  service web  Starting\n  message      Pulling images\n"
    );
    assert_eq!(
        text(&failed.stdout),
        "Application deployment Wiki\n  name         Wiki\n  id           app-3\n  state        Failed\n  desired      Present\n  node         gb10-host\n  target       Docker applications\n  target id    node-1-apps\n  application  Open WebUI\n  service web  Failed\n  failure      Image pull failed\n"
    );
    assert_eq!(
        text(&stale.stdout),
        "Application deployment Mirror\n  name         Mirror\n  id           app-6\n  state        Running\n  desired      Present\n  readiness    Not ready\n  stale        yes\n  node         gb10-host\n  target       Docker applications\n  target id    node-1-apps\n  application  Open WebUI\n"
    );
}

#[test]
fn show_applies_the_console_state_overrides() {
    let manager = FakeManager::start(FakeState::with_applications(mixed_deployments()));
    let mut pausing = local_application_deployment("app-7", "Calendar", Ready, Paused);
    pausing.state.as_mut().expect("state").readiness_state =
        ApplicationReadinessState::Ready as i32;
    pausing.state.as_mut().expect("state").endpoint = Some(ApplicationEndpoint {
        url: "http://gb10-host:9090".to_owned(),
    });
    manager.state().application_deployments.push(pausing);

    let paused = run(&manager, &["show", "Docs"]);
    let pausing = run(&manager, &["show", "Calendar"]);
    let tearing_down = run(&manager, &["show", "Old"]);

    assert!(
        text(&paused.stdout).contains("  state        Paused\n  desired      Paused\n  node"),
        "{}",
        text(&paused.stdout)
    );
    let pausing = text(&pausing.stdout);
    assert!(
        pausing.contains("  state        Pausing\n  desired      Paused\n  node")
            && !pausing.contains("readiness")
            && !pausing.contains("endpoint"),
        "a requested pause hides readiness and endpoint\n{pausing}"
    );
    assert!(
        text(&tearing_down.stdout)
            .contains("  state        Tearing down\n  desired      Teardown\n  node"),
        "{}",
        text(&tearing_down.stdout)
    );
}

#[test]
fn show_hides_the_stage_that_a_requested_change_overrides() {
    let mut tearing_down = local_application_deployment("app-7", "Old Wiki", Ready, Teardown);
    tearing_down.state.as_mut().expect("state").stage = "Serving requests".to_owned();
    let mut pausing = local_application_deployment("app-8", "Calendar", Starting, Paused);
    pausing.state.as_mut().expect("state").stage = "Pulling images".to_owned();
    let manager = FakeManager::start(FakeState::with_applications(vec![tearing_down, pausing]));

    let tearing_down = run(&manager, &["show", "Old Wiki"]);
    let pausing = run(&manager, &["show", "Calendar"]);

    assert_eq!(
        text(&tearing_down.stdout),
        "Application deployment Old Wiki\n  name         Old Wiki\n  id           app-7\n  state        Tearing down\n  desired      Teardown\n  node         gb10-host\n  target       Docker applications\n  target id    node-1-apps\n  application  Open WebUI\n"
    );
    assert_eq!(
        text(&pausing.stdout),
        "Application deployment Calendar\n  name         Calendar\n  id           app-8\n  state        Pausing\n  desired      Paused\n  node         gb10-host\n  target       Docker applications\n  target id    node-1-apps\n  application  Open WebUI\n"
    );
}

#[test]
fn a_generated_stored_name_addresses_the_deployment_like_its_display_name() {
    let manager = FakeManager::start(FakeState::with_applications(vec![
        local_application_deployment("app-9", "open-webui-1", Ready, Present),
    ]));

    let by_stored_name = run(&manager, &["show", "open-webui-1"]);
    let by_display_name = run(&manager, &["show", "Open WebUI"]);
    let by_id = run(&manager, &["show", "app-9"]);

    assert_eq!(by_stored_name.status.code(), Some(0));
    assert!(
        text(&by_stored_name.stdout).starts_with(
            "Application deployment Open WebUI\n  name         Open WebUI\n  id           app-9\n"
        ),
        "{}",
        text(&by_stored_name.stdout)
    );
    assert_eq!(text(&by_display_name.stdout), text(&by_stored_name.stdout));
    assert_eq!(text(&by_id.stdout), text(&by_stored_name.stdout));
}

#[test]
fn an_unknown_or_ambiguous_reference_fails_with_exit_code_1() {
    let mut deployments = mixed_deployments();
    deployments.push(local_application_deployment(
        "app-7", "Chat", Ready, Present,
    ));
    let manager = FakeManager::start(FakeState::with_applications(deployments));

    let unknown = run(&manager, &["show", "nothing"]);
    let ambiguous = run(&manager, &["teardown", "Chat", "--yes"]);

    assert_eq!(unknown.status.code(), Some(1));
    assert_eq!(text(&unknown.stdout), "");
    assert_eq!(
        text(&unknown.stderr),
        "no application deployment is named or has id \"nothing\"; see `dotc application list`\n"
    );
    assert_eq!(ambiguous.status.code(), Some(1));
    assert_eq!(text(&ambiguous.stdout), "");
    assert_eq!(
        text(&ambiguous.stderr),
        "2 application deployments are named \"Chat\": app-1, app-7; pass one of the ids instead\n"
    );
    assert!(
        manager
            .calls()
            .iter()
            .all(|call| call.rpc != Rpc::RemoveApplicationDeployment),
        "an ambiguous teardown must not reach the manager"
    );
}

#[test]
fn an_unreachable_manager_fails_with_exit_code_1() {
    let reserved = dotcore::testing::reserve_port();
    let address = std::net::SocketAddr::from(([127, 0, 0, 1], reserved.port()));

    let output = dotc_at("application", &address.to_string(), &["list"])
        .output()
        .expect("run dotc application list");

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
    let manager = FakeManager::start(FakeState::with_applications(mixed_deployments()));
    let empty = support::canonical_tempdir().expect("empty working directory");

    let mut command = dotc_at("application", &manager.address().to_string(), &["list"]);
    command.current_dir(empty.path());
    let output = command.output().expect("run dotc application list");

    assert_eq!(output.status.code(), Some(0), "{}", text(&output.stderr));
    assert!(text(&output.stdout).contains("  Chat    app-1  Ready"));
    assert_eq!(
        std::fs::read_dir(empty.path())
            .expect("read working directory")
            .count(),
        0
    );
}

#[test]
fn logs_read_then_follow_an_application_deployment() {
    let manager = FakeManager::start(FakeState::with_applications(mixed_deployments()));
    {
        let mut state = manager.state();
        state.application_logs.page = ReadDeploymentLogsResponse {
            sources: vec![
                log_source("web", DeploymentLogSourceState::Available),
                log_source("db", DeploymentLogSourceState::Unavailable),
            ],
            entries: vec![log_entry("web", 10, b"serving")],
            latest_positions: vec![DeploymentLogResumePosition {
                source_name: "web".to_owned(),
                latest_timestamp: Some(timestamp(10)),
            }],
            ..Default::default()
        };
        state.application_logs.follow = vec![
            Ok(FollowDeploymentLogsResponse {
                event: Some(Event::SourceEvent(DeploymentLogSourceEvent {
                    source_name: "db".to_owned(),
                    source_label: label_of("db"),
                    state: DeploymentLogSourceState::Available as i32,
                })),
            }),
            Ok(FollowDeploymentLogsResponse {
                event: Some(Event::Entry(log_entry("db", 11, b"accepting connections"))),
            }),
        ];
    }

    let output = run(&manager, &["logs", "Chat", "--follow"]);

    assert_eq!(
        text(&output.stderr),
        "Db (db): unavailable\nDb (db): available\nthe log follow of application deployment Chat ended\n"
    );
    assert_eq!(output.status.code(), Some(0));
    assert_eq!(
        text(&output.stdout),
        "[Web (web)] serving\n[Db (db)] accepting connections\n"
    );
    let logs = &manager.state().application_logs;
    assert_eq!(logs.reads[0].application_deployment_id, "app-1");
    assert_eq!(logs.follows[0].application_deployment_id, "app-1");
    assert_eq!(
        logs.follows[0].resume_positions,
        [DeploymentLogResumePosition {
            source_name: "web".to_owned(),
            latest_timestamp: Some(timestamp(10)),
        }]
    );
}

#[test]
fn yaml_list_prints_the_manager_deployments_with_their_names() {
    let manager = FakeManager::start(FakeState::with_applications(vec![
        local_application_deployment("app-1", "Chat", Running, Present),
    ]));

    let output = run(&manager, &["list", "--yaml"]);

    assert_eq!(text(&output.stderr), "");
    assert_eq!(output.status.code(), Some(0));
    let placement = json!({
        "siteNodeId": "node-1",
        "executionTargetId": "node-1-apps",
    });
    assert_eq!(
        support::yaml_documents(&text(&output.stdout)),
        [json!({
            "deployments": [{
                "applicationDeploymentId": "app-1",
                "applicationDeploymentName": "Chat",
                "placement": placement,
                "snapshot": {
                    "applicationId": "open-webui",
                    "displayName": "Open WebUI",
                    "placement": placement,
                },
                "desiredState": "APPLICATION_DEPLOYMENT_DESIRED_STATE_PRESENT",
                "state": { "lifecycleState": "APPLICATION_LIFECYCLE_STATE_RUNNING" },
                "siteNodeDisplayName": "gb10-host",
                "executionTargetDisplayName": "Docker applications",
            }],
        })]
    );
}

#[test]
fn yaml_lifecycle_verbs_print_the_accepted_deployment_with_its_names() {
    let manager = FakeManager::start(FakeState::with_applications(vec![
        local_application_deployment("app-1", "Chat", Running, Present),
        local_application_deployment("app-2", "Notes", Running, Present),
        local_application_deployment("app-3", "Wiki", Running, Present),
    ]));
    let placement = json!({
        "siteNodeId": "node-1",
        "executionTargetId": "node-1-apps",
    });
    let document = |id: &str, name: &str, desired: &str, lifecycle: &str| {
        json!({
            "applicationDeploymentId": id,
            "applicationDeploymentName": name,
            "placement": placement,
            "snapshot": {
                "applicationId": "open-webui",
                "displayName": "Open WebUI",
                "placement": placement,
            },
            "desiredState": desired,
            "state": { "lifecycleState": lifecycle },
            "siteNodeDisplayName": "gb10-host",
            "executionTargetDisplayName": "Docker applications",
        })
    };

    let paused = run(&manager, &["pause", "Chat", "--yaml"]);
    let resumed = run(&manager, &["resume", "Notes", "--yaml"]);
    let torn_down = run(&manager, &["teardown", "Wiki", "--yaml", "--yes"]);
    let refused = run(&manager, &["teardown", "Chat", "--yaml"]);

    for output in [&paused, &resumed, &torn_down] {
        assert_eq!(text(&output.stderr), "");
        assert_eq!(output.status.code(), Some(0));
    }
    assert_eq!(
        support::yaml_documents(&text(&paused.stdout)),
        [document(
            "app-1",
            "Chat",
            "APPLICATION_DEPLOYMENT_DESIRED_STATE_PAUSED",
            "APPLICATION_LIFECYCLE_STATE_RUNNING"
        )]
    );
    assert_eq!(
        support::yaml_documents(&text(&resumed.stdout)),
        [document(
            "app-2",
            "Notes",
            "APPLICATION_DEPLOYMENT_DESIRED_STATE_PRESENT",
            "APPLICATION_LIFECYCLE_STATE_RUNNING"
        )]
    );
    assert_eq!(
        support::yaml_documents(&text(&torn_down.stdout)),
        [document(
            "app-3",
            "Wiki",
            "APPLICATION_DEPLOYMENT_DESIRED_STATE_TEARDOWN",
            "APPLICATION_LIFECYCLE_STATE_STOPPING"
        )]
    );
    assert_eq!(refused.status.code(), Some(1));
    assert_eq!(text(&refused.stdout), "");
    assert_eq!(
        text(&refused.stderr),
        "tearing down an application deployment needs confirmation, which --yaml never asks for; pass --yes to tear it down\n"
    );
    assert_eq!(
        lifecycle_calls(&manager),
        vec![
            call(Rpc::PauseApplicationDeployment, Some("app-1")),
            call(Rpc::ResumeApplicationDeployment, Some("app-2")),
            call(Rpc::RemoveApplicationDeployment, Some("app-3")),
        ]
    );
}

#[test]
fn yaml_list_and_show_mask_secret_values_unless_revealed() {
    let deployment = deployment_with_parameters();
    let manager = FakeManager::start(FakeState::with_applications(vec![deployment.clone()]));

    let listed = run(&manager, &["list", "--yaml"]);
    let masked = run(&manager, &["show", "Chat", "--yaml"]);
    let revealed = run(&manager, &["show", "Chat", "--yaml", "--reveal-secrets"]);

    let mut revealed_document = serde_json::to_value(&deployment).expect("deployment mapping");
    revealed_document["siteNodeDisplayName"] = json!("gb10-host");
    revealed_document["executionTargetDisplayName"] = json!("Docker applications");
    let mut masked_document = revealed_document.clone();
    let password = masked_document["snapshot"]["parameterValues"]
        .as_array_mut()
        .expect("values")
        .iter_mut()
        .find(|value| value["parameterId"] == "admin-password")
        .expect("password value");
    assert_eq!(password["string"], "s3cretPassw0rd");
    password["string"] = json!("********");
    assert_eq!(
        support::yaml_documents(&text(&listed.stdout)),
        [json!({ "deployments": [masked_document.clone()] })]
    );
    assert_eq!(
        support::yaml_documents(&text(&masked.stdout)),
        [masked_document]
    );
    assert_eq!(
        support::yaml_documents(&text(&revealed.stdout)),
        [revealed_document]
    );
    for output in [listed, masked, revealed] {
        assert_eq!(text(&output.stderr), "");
        assert_eq!(output.status.code(), Some(0));
    }
}

#[test]
fn yaml_logs_print_one_document_per_entry() {
    let manager = FakeManager::start(FakeState::with_applications(mixed_deployments()));
    {
        let mut state = manager.state();
        state.application_logs.page = ReadDeploymentLogsResponse {
            sources: vec![log_source("web", DeploymentLogSourceState::Available)],
            entries: vec![log_entry("web", 10, b"serving")],
            ..Default::default()
        };
        state.application_logs.follow = vec![Ok(FollowDeploymentLogsResponse {
            event: Some(Event::Entry(log_entry("web", 11, b"stopping"))),
        })];
    }

    let output = run(&manager, &["logs", "Chat", "--follow", "--yaml"]);

    assert_eq!(
        text(&output.stderr),
        "the log follow of application deployment Chat ended\n"
    );
    assert_eq!(output.status.code(), Some(0));
    assert_eq!(
        support::yaml_documents(&text(&output.stdout)),
        [
            json!({
                "sourceName": "web",
                "sourceLabel": "Web",
                "timestamp": { "seconds": "1794500010" },
                "content": "c2VydmluZw==",
                "text": "serving",
            }),
            json!({
                "sourceName": "web",
                "sourceLabel": "Web",
                "timestamp": { "seconds": "1794500011" },
                "content": "c3RvcHBpbmc=",
                "text": "stopping",
            }),
        ]
    );
}

#[test]
fn help_lists_the_six_verbs_and_the_manager_flag() {
    let group = isolated_dotc()
        .args(["application", "--help"])
        .output()
        .expect("run help");
    let list = isolated_dotc()
        .args(["application", "list", "-h"])
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
    let manager = FakeManager::start(FakeState::with_applications(mixed_deployments()));
    assert_eq!(
        manager.application_deployment("app-1").desired_state(),
        Present
    );

    let paused = run(&manager, &["pause", "Chat"]);

    assert_eq!(text(&paused.stderr), "");
    assert_eq!(paused.status.code(), Some(0));
    assert_eq!(text(&paused.stdout), "✔ Pausing Chat\n");
    assert_eq!(
        manager.application_deployment("app-1").desired_state(),
        Paused
    );

    let resumed = run(&manager, &["resume", "app-4"]);

    assert_eq!(resumed.status.code(), Some(0));
    assert_eq!(text(&resumed.stderr), "");
    assert_eq!(text(&resumed.stdout), "✔ Resuming Docs\n");
    assert_eq!(
        manager.application_deployment("app-4").desired_state(),
        Present
    );
    assert_eq!(
        lifecycle_calls(&manager),
        vec![
            call(Rpc::PauseApplicationDeployment, Some("app-1")),
            call(Rpc::ResumeApplicationDeployment, Some("app-4")),
        ]
    );
}

#[test]
fn teardown_with_yes_removes_the_deployment_and_prints_the_accepted_state() {
    let manager = FakeManager::start(FakeState::with_applications(mixed_deployments()));

    let output = run(&manager, &["teardown", "Wiki", "--yes"]);

    assert_eq!(output.status.code(), Some(0));
    assert_eq!(text(&output.stderr), "");
    assert_eq!(text(&output.stdout), "✔ Tearing down Wiki\n");
    assert_eq!(
        manager.application_deployment("app-3").desired_state(),
        Teardown
    );
    assert_eq!(
        lifecycle_calls(&manager),
        vec![call(Rpc::RemoveApplicationDeployment, Some("app-3"))]
    );
}

#[test]
fn teardown_without_a_terminal_and_without_yes_fails_before_calling_the_manager() {
    let manager = FakeManager::start(FakeState::with_applications(mixed_deployments()));

    let output = run(&manager, &["teardown", "Wiki"]);

    assert_eq!(output.status.code(), Some(1));
    assert_eq!(text(&output.stdout), "");
    assert_eq!(
        text(&output.stderr),
        "tearing down an application deployment needs a terminal; run in a terminal or pass --yes to tear it down without confirmation\n"
    );
    assert_eq!(manager.calls(), Vec::new());
}

#[test]
fn a_refused_request_fails_with_the_manager_message_and_changes_nothing() {
    let manager = FakeManager::start(FakeState::with_applications(mixed_deployments()));
    manager.state().scripted.insert(
        Rpc::PauseApplicationDeployment,
        Scripted::Fail(Status::failed_precondition(
            "Application lifecycle operation is not valid for the current state",
        )),
    );

    let output = run(&manager, &["pause", "Old"]);

    assert_eq!(output.status.code(), Some(1));
    assert_eq!(text(&output.stdout), "");
    assert_eq!(
        text(&output.stderr),
        "cannot pause Old: the manager returned FailedPrecondition: Application lifecycle operation is not valid for the current state; see `dotc application list`\n"
    );
    assert_eq!(
        manager.application_deployment("app-5").desired_state(),
        Teardown
    );
}

#[test]
fn an_answer_without_the_asked_desired_state_is_not_accepted() {
    let manager = FakeManager::start(FakeState::with_applications(mixed_deployments()));
    manager
        .state()
        .scripted
        .insert(Rpc::PauseApplicationDeployment, Scripted::Unchanged);

    let output = run(&manager, &["pause", "Chat"]);

    assert_eq!(output.status.code(), Some(1));
    assert_eq!(text(&output.stdout), "");
    assert_eq!(
        text(&output.stderr),
        "the manager did not accept the request to pause Chat; its desired state is Present; see `dotc application list`\n"
    );
    assert_eq!(
        manager.application_deployment("app-1").desired_state(),
        Present
    );
}

#[test]
fn a_terminal_shows_the_list_as_a_table() {
    let manager = FakeManager::start(FakeState::with_applications(mixed_deployments()));

    let Some(run) = run_in_pty(
        terminal_command(&manager, &["list"]),
        Session::new([]).with_stdout_on_terminal(),
    ) else {
        return;
    };

    assert_eq!(run.code(), Some(0), "screen:\n{}", run.screen());
    assert_eq!(run.stdout(), "");
    assert_eq!(run.stderr(), "");
    for expected in [
        "NAME    ID     STATE            NODE       APPLICATION",
        "Chat    app-1  Ready            gb10-host  Open WebUI",
        "Notes   app-2  Starting         gb10-host  Open WebUI",
        "Wiki    app-3  Failed           gb10-host  Open WebUI",
        "Docs    app-4  Paused           gb10-host  Open WebUI",
        "Old     app-5  Tearing down     gb10-host  Open WebUI",
        "Mirror  app-6  Running (stale)  gb10-host  Open WebUI",
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
    let manager = FakeManager::start(FakeState::with_applications(mixed_deployments()));

    let Some(run) = run_in_pty(
        terminal_command(&manager, &["show", "Chat"]),
        Session::new([]).with_stdout_on_terminal(),
    ) else {
        return;
    };

    assert_eq!(run.code(), Some(0), "screen:\n{}", run.screen());
    assert_eq!(run.stdout(), "");
    let lines = run.screen().lines().map(str::trim).collect::<Vec<_>>();
    for expected in [
        "Application deployment Chat",
        "name         Chat",
        "id           app-1",
        "state        Ready",
        "desired      Present",
        "readiness    Ready",
        "node         gb10-host",
        "target       Docker applications",
        "target id    node-1-apps",
        "application  Open WebUI",
        "service web  Running",
        "service db   Starting",
        "endpoint     http://gb10-host:8080",
        "route        gemma",
    ] {
        assert!(lines.contains(&expected), "{expected}\n{}", run.screen());
    }
    assert!(!lines.iter().any(|line| line.starts_with("failure")));
    assert!(!lines.iter().any(|line| line.starts_with("stale")));
}

#[test]
fn a_terminal_reports_a_pause_without_a_next_step() {
    let manager = FakeManager::start(FakeState::with_applications(mixed_deployments()));

    let Some(run) = run_in_pty(
        terminal_command(&manager, &["pause", "Chat"]),
        Session::new([]).with_stdout_on_terminal(),
    ) else {
        return;
    };

    assert_eq!(run.code(), Some(0), "screen:\n{}", run.screen());
    assert_eq!(run.stdout(), "");
    assert_eq!(run.stderr(), "");
    assert_eq!(run.screen().trim(), "OK Pausing Chat");
    assert_eq!(
        manager.application_deployment("app-1").desired_state(),
        Paused
    );
}

#[test]
fn a_terminal_teardown_warns_about_data_and_removes_when_accepted() {
    let manager = FakeManager::start(FakeState::with_applications(mixed_deployments()));

    let Some(run) = run_in_pty(
        terminal_command(&manager, &["teardown", "Chat"]),
        Session::new([
            Step::Wait("Tear down Chat?"),
            Step::Wait("[no]"),
            Step::Send(b"yes\n"),
        ]),
    ) else {
        return;
    };

    assert_eq!(run.code(), Some(0), "screen:\n{}", run.screen());
    assert_eq!(run.stdout(), "OK Tearing down Chat\n");
    assert_eq!(run.stderr(), "");
    let screen = run
        .screen()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ");
    assert!(
        screen.contains(
            "Tear down Chat? This stops and removes its workloads and deletes this deployment's data (data). It keeps the data shared by the app (models) and the catalog application."
        ),
        "{}",
        run.screen()
    );
    assert_eq!(
        manager.application_deployment("app-1").desired_state(),
        Teardown
    );
}

#[test]
fn a_terminal_teardown_cancels_by_default_with_exit_code_130() {
    let manager = FakeManager::start(FakeState::with_applications(mixed_deployments()));

    let Some(run) = run_in_pty(
        terminal_command(&manager, &["teardown", "Chat"]),
        Session::new([Step::Wait("Tear down Chat?"), Step::Send(b"\n")]),
    ) else {
        return;
    };

    assert_eq!(run.code(), Some(130), "screen:\n{}", run.screen());
    assert_eq!(run.stdout(), "");
    assert_eq!(
        manager.application_deployment("app-1").desired_state(),
        Present
    );
    assert_eq!(
        lifecycle_calls(&manager),
        Vec::new(),
        "a cancelled teardown must not reach the manager"
    );
}

#[test]
fn show_prints_parameters_in_catalog_order_with_secrets_masked() {
    let manager = FakeManager::start(FakeState::with_applications(vec![
        deployment_with_parameters(),
    ]));

    let output = run(&manager, &["show", "Chat"]);

    assert_eq!(text(&output.stderr), "");
    assert_eq!(output.status.code(), Some(0));
    let stdout = text(&output.stdout);
    assert!(!stdout.contains("s3cretPassw0rd"), "{stdout}");
    assert_eq!(support::parameter_rows(&stdout), PARAMETER_ROWS_MASKED);
}

#[test]
fn show_prints_only_retained_parameters() {
    let mut deployment = local_application_deployment("app-1", "Chat", Ready, Present);
    let snapshot = deployment.snapshot.as_mut().expect("seeded snapshot");
    let member = |id: &str, name: &str, parameter_type| ParameterDefinition {
        parameter_id: id.to_owned(),
        display_name: name.to_owned(),
        group_id: Some("login".to_owned()),
        parameter_type: Some(parameter_type),
        ..Default::default()
    };
    snapshot.graph = Some(ApplicationGraph {
        parameters: vec![
            member(
                "require-login",
                "Require login",
                ParameterType::Boolean(BooleanParameterSchema {
                    default_value: Some(false),
                }),
            ),
            member(
                "admin-email",
                "Admin email",
                ParameterType::Text(TextParameterSchema {
                    minimum_length: 3,
                    maximum_length: Some(254),
                    default_value: Some("admin@example.com".to_owned()),
                    format: None,
                }),
            ),
        ],
        parameter_groups: vec![ParameterGroup {
            group_id: "login".to_owned(),
            switch_parameter_id: Some("require-login".to_owned()),
            ..Default::default()
        }],
        ..Default::default()
    });
    // Login off: the switch is the only retained value.
    snapshot.parameter_values = vec![ParameterValue {
        parameter_id: "require-login".to_owned(),
        value: Some(Value::Boolean(false)),
    }];
    let manager = FakeManager::start(FakeState::with_applications(vec![deployment]));

    let output = run(&manager, &["show", "Chat"]);

    assert_eq!(output.status.code(), Some(0));
    assert_eq!(
        support::parameter_rows(&text(&output.stdout)),
        ["Require login false"]
    );
}

#[test]
fn show_reveals_secret_parameters_with_the_flag() {
    let manager = FakeManager::start(FakeState::with_applications(vec![
        deployment_with_parameters(),
    ]));

    let output = run(&manager, &["show", "Chat", "--reveal-secrets"]);

    assert_eq!(output.status.code(), Some(0));
    let revealed = PARAMETER_ROWS_MASKED.map(|line| line.replace("********", "s3cretPassw0rd"));
    assert_eq!(support::parameter_rows(&text(&output.stdout)), revealed);
}

#[test]
fn help_describes_the_reveal_flag() {
    let manager = FakeManager::start(FakeState::with_applications(Vec::new()));

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
    let manager = FakeManager::start(FakeState::with_applications(vec![
        deployment_with_parameters(),
    ]));

    let Some(run) = run_in_pty(
        terminal_command(&manager, &["show", "Chat"]),
        Session::new([]).with_stdout_on_terminal(),
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
            "Web search true",
            "Memory limit 2048 MB",
            "Mode fast",
            "Welcome note Empty",
            "Admin password ********",
        ]
    );
}

const PARAMETER_ROWS_MASKED: [&str; 5] = [
    "Web search true",
    "Memory limit 2048 MB",
    "Mode fast",
    "Welcome note Empty",
    "Admin password ********",
];

/// A deployment with one parameter of each kind: a default boolean, a non-default integer with a
/// unit, an enum, an empty text and a secret.
fn deployment_with_parameters() -> ApplicationDeployment {
    let definition = |id: &str, name: &str, parameter_type| ParameterDefinition {
        parameter_id: id.to_owned(),
        display_name: name.to_owned(),
        parameter_type: Some(parameter_type),
        ..Default::default()
    };
    let value = |id: &str, value| ParameterValue {
        parameter_id: id.to_owned(),
        value: Some(value),
    };

    let mut deployment = local_application_deployment("app-1", "Chat", Ready, Present);
    let snapshot = deployment.snapshot.as_mut().expect("seeded snapshot");
    snapshot.graph = Some(ApplicationGraph {
        parameters: vec![
            definition(
                "web-search",
                "Web search",
                ParameterType::Boolean(BooleanParameterSchema {
                    default_value: Some(true),
                }),
            ),
            definition(
                "memory-limit",
                "Memory limit",
                ParameterType::UnsignedInteger(UnsignedIntegerParameterSchema {
                    minimum: 512,
                    maximum: Some(8192),
                    unit: "MB".to_owned(),
                    default_value: Some(1024),
                    memory_pool: None,
                }),
            ),
            definition(
                "mode",
                "Mode",
                ParameterType::StringEnum(StringEnumParameterSchema {
                    values: vec!["fast".to_owned(), "careful".to_owned()],
                    default_value: Some("careful".to_owned()),
                }),
            ),
            definition(
                "welcome-note",
                "Welcome note",
                ParameterType::Text(TextParameterSchema {
                    minimum_length: 0,
                    maximum_length: Some(100),
                    default_value: None,
                    format: None,
                }),
            ),
            definition(
                "admin-password",
                "Admin password",
                ParameterType::Secret(SecretParameterSchema {
                    generated_length: 32,
                    minimum_length: 8,
                    maximum_length: None,
                }),
            ),
        ],
        ..Default::default()
    });
    snapshot.parameter_values = vec![
        value("web-search", Value::Boolean(true)),
        value("memory-limit", Value::UnsignedInteger(2048)),
        value("mode", Value::String("fast".to_owned())),
        value("welcome-note", Value::String(String::new())),
        value("admin-password", Value::String("s3cretPassw0rd".to_owned())),
    ];
    deployment
}

/// Seeds one deployment in each state the commands present differently.
fn mixed_deployments() -> Vec<ApplicationDeployment> {
    let mut ready = local_application_deployment("app-1", "Chat", Ready, Present);
    let state = ready.state.as_mut().expect("seeded state");
    state.readiness_state = ApplicationReadinessState::Ready as i32;
    state.endpoint = Some(ApplicationEndpoint {
        url: "http://gb10-host:8080".to_owned(),
    });
    state.services = vec![
        service("web", ApplicationServiceLifecycleState::Running),
        service("db", ApplicationServiceLifecycleState::Starting),
    ];
    ready.selected_inference_route = Some(ApplicationSelectedInferenceRoute {
        inference_route_id: "route-1".to_owned(),
        route_name: "gemma".to_owned(),
    });
    ready.storage_attachments = [
        ("data", StoragePlacement::Deployment),
        ("models", StoragePlacement::Application),
    ]
    .map(|(binding, placement)| ApplicationStorageAttachment {
        service_name: "web".to_owned(),
        binding_name: binding.to_owned(),
        storage_name: format!("web-{binding}"),
        placement: placement as i32,
    })
    .to_vec();

    let mut starting = local_application_deployment("app-2", "Notes", Starting, Present);
    let state = starting.state.as_mut().expect("seeded state");
    state.stage = "Pulling images".to_owned();
    state.services = vec![service("web", ApplicationServiceLifecycleState::Starting)];

    let mut failed = local_application_deployment("app-3", "Wiki", Failed, Present);
    let state = failed.state.as_mut().expect("seeded state");
    state.failure = Some(ApplicationDeploymentFailure {
        code: "image_pull".to_owned(),
        message: "Image pull failed".to_owned(),
        diagnostics: Vec::new(),
    });
    state.services = vec![service("web", ApplicationServiceLifecycleState::Failed)];

    let mut stale = local_application_deployment("app-6", "Mirror", Running, Present);
    let state = stale.state.as_mut().expect("seeded state");
    state.readiness_state = ApplicationReadinessState::NotReady as i32;
    state.observation_stale = true;

    vec![
        ready,
        starting,
        failed,
        local_application_deployment("app-4", "Docs", ApplicationLifecycleState::Paused, Paused),
        local_application_deployment("app-5", "Old", Ready, Teardown),
        stale,
    ]
}

fn service(name: &str, state: ApplicationServiceLifecycleState) -> ApplicationServiceObservation {
    ApplicationServiceObservation {
        service_name: name.to_owned(),
        lifecycle_state: state as i32,
        ..Default::default()
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
                Rpc::PauseApplicationDeployment
                    | Rpc::ResumeApplicationDeployment
                    | Rpc::RemoveApplicationDeployment
            )
        })
        .collect()
}

fn log_source(name: &str, state: DeploymentLogSourceState) -> DeploymentLogSource {
    DeploymentLogSource {
        source_name: name.to_owned(),
        source_label: label_of(name),
        state: state as i32,
    }
}

fn log_entry(source: &str, second: i64, content: &[u8]) -> DeploymentLogEntry {
    DeploymentLogEntry {
        source_name: source.to_owned(),
        source_label: label_of(source),
        timestamp: Some(timestamp(second)),
        content: content.to_vec(),
        ..Default::default()
    }
}

fn timestamp(second: i64) -> Timestamp {
    Timestamp {
        seconds: 1_794_500_000 + second,
        nanos: 0,
    }
}

/// Runs `dotc application <arguments>` against the fake without any terminal.
fn run(manager: &FakeManager, arguments: &[&str]) -> Output {
    dotc_at("application", &manager.address().to_string(), arguments)
        .output()
        .expect("run dotc application")
}

/// Builds `dotc application <arguments>` for a pseudo-terminal session with no `TERM`, which
/// selects the line prompts and the ASCII marks.
fn terminal_command(manager: &FakeManager, arguments: &[&str]) -> Command {
    let mut command = isolated_dotc();
    command
        .arg("application")
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
