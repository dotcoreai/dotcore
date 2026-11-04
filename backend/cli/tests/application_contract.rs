//! Process-level contracts for `dotc application`, run against the in-process fake manager.

use std::process::{Command, Output, Stdio};

use dotcore_proto::dotcore::application::v1::{
    ApplicationDeployment, ApplicationDeploymentDesiredState, ApplicationDeploymentFailure,
    ApplicationEndpoint, ApplicationLifecycleState, ApplicationReadinessState,
    ApplicationSelectedInferenceRoute, ApplicationServiceLifecycleState,
    ApplicationServiceObservation,
};
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
fn list_prints_one_record_block_per_deployment_in_every_state() {
    let manager = FakeManager::start(FakeState::with_applications(mixed_deployments()));

    let output = run(&manager, &["list"]);

    assert_eq!(text(&output.stderr), "");
    assert_eq!(output.status.code(), Some(0));
    assert_eq!(
        text(&output.stdout),
        [
            list_block("app-1", "Chat", "Ready", "false"),
            list_block("app-2", "Notes", "Starting", "false"),
            list_block("app-3", "Wiki", "Failed", "false"),
            list_block("app-4", "Docs", "Paused", "false"),
            list_block("app-5", "Old", "Tearing down", "false"),
            list_block("app-6", "Mirror", "Running", "true"),
        ]
        .join("\n")
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
        text(&output.stdout).contains("DOTCORE_APPLICATION_DEPLOYMENT_NODE=node-1\n"),
        "{}",
        text(&output.stdout)
    );
}

#[test]
fn list_of_an_empty_site_prints_nothing() {
    let manager = FakeManager::start(FakeState::with_applications(Vec::new()));

    let output = run(&manager, &["list"]);

    assert_eq!(output.status.code(), Some(0));
    assert_eq!(text(&output.stdout), "");
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
        block(&[
            ("ID", "app-1"),
            ("NAME", "Chat"),
            ("STATE", "Ready"),
            ("DESIRED_STATE", "Present"),
            ("READINESS", "Ready"),
            ("STALE", "false"),
            ("NODE", "gb10-host"),
            ("EXECUTION_TARGET", "node-1-apps"),
            ("APPLICATION", "Open WebUI"),
            ("SERVICES", "web: Running, db: Starting"),
            ("ENDPOINT", "http://gb10-host:8080"),
            ("ROUTE", "gemma"),
            ("FAILURE", ""),
            ("MESSAGE", ""),
        ])
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
        block(&[
            ("ID", "app-2"),
            ("NAME", "Notes"),
            ("STATE", "Starting"),
            ("DESIRED_STATE", "Present"),
            ("READINESS", ""),
            ("STALE", "false"),
            ("NODE", "gb10-host"),
            ("EXECUTION_TARGET", "node-1-apps"),
            ("APPLICATION", "Open WebUI"),
            ("SERVICES", "web: Starting"),
            ("ENDPOINT", ""),
            ("ROUTE", ""),
            ("FAILURE", ""),
            ("MESSAGE", "Pulling images"),
        ])
    );
    assert_eq!(
        text(&failed.stdout),
        block(&[
            ("ID", "app-3"),
            ("NAME", "Wiki"),
            ("STATE", "Failed"),
            ("DESIRED_STATE", "Present"),
            ("READINESS", ""),
            ("STALE", "false"),
            ("NODE", "gb10-host"),
            ("EXECUTION_TARGET", "node-1-apps"),
            ("APPLICATION", "Open WebUI"),
            ("SERVICES", "web: Failed"),
            ("ENDPOINT", ""),
            ("ROUTE", ""),
            ("FAILURE", "Image pull failed"),
            ("MESSAGE", ""),
        ])
    );
    assert_eq!(
        text(&stale.stdout),
        block(&[
            ("ID", "app-6"),
            ("NAME", "Mirror"),
            ("STATE", "Running"),
            ("DESIRED_STATE", "Present"),
            ("READINESS", "Not ready"),
            ("STALE", "true"),
            ("NODE", "gb10-host"),
            ("EXECUTION_TARGET", "node-1-apps"),
            ("APPLICATION", "Open WebUI"),
            ("SERVICES", ""),
            ("ENDPOINT", ""),
            ("ROUTE", ""),
            ("FAILURE", ""),
            ("MESSAGE", ""),
        ])
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
        text(&paused.stdout).contains(
            "STATE=Paused\nDOTCORE_APPLICATION_DEPLOYMENT_DESIRED_STATE=Paused\nDOTCORE_APPLICATION_DEPLOYMENT_READINESS=\n"
        ),
        "{}",
        text(&paused.stdout)
    );
    assert!(
        text(&pausing.stdout).contains(
            "STATE=Pausing\nDOTCORE_APPLICATION_DEPLOYMENT_DESIRED_STATE=Paused\nDOTCORE_APPLICATION_DEPLOYMENT_READINESS=\n"
        ) && text(&pausing.stdout).contains("DOTCORE_APPLICATION_DEPLOYMENT_ENDPOINT=\n"),
        "a requested pause hides readiness and endpoint\n{}",
        text(&pausing.stdout)
    );
    assert!(
        text(&tearing_down.stdout).contains(
            "STATE=Tearing down\nDOTCORE_APPLICATION_DEPLOYMENT_DESIRED_STATE=Teardown\n"
        ),
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
        block(&[
            ("ID", "app-7"),
            ("NAME", "Old Wiki"),
            ("STATE", "Tearing down"),
            ("DESIRED_STATE", "Teardown"),
            ("READINESS", ""),
            ("STALE", "false"),
            ("NODE", "gb10-host"),
            ("EXECUTION_TARGET", "node-1-apps"),
            ("APPLICATION", "Open WebUI"),
            ("SERVICES", ""),
            ("ENDPOINT", ""),
            ("ROUTE", ""),
            ("FAILURE", ""),
            ("MESSAGE", ""),
        ])
    );
    assert_eq!(
        text(&pausing.stdout),
        block(&[
            ("ID", "app-8"),
            ("NAME", "Calendar"),
            ("STATE", "Pausing"),
            ("DESIRED_STATE", "Paused"),
            ("READINESS", ""),
            ("STALE", "false"),
            ("NODE", "gb10-host"),
            ("EXECUTION_TARGET", "node-1-apps"),
            ("APPLICATION", "Open WebUI"),
            ("SERVICES", ""),
            ("ENDPOINT", ""),
            ("ROUTE", ""),
            ("FAILURE", ""),
            ("MESSAGE", ""),
        ])
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
            "Application deployment\nDOTCORE_APPLICATION_DEPLOYMENT_ID=app-9\nDOTCORE_APPLICATION_DEPLOYMENT_NAME=Open WebUI\n"
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
    let address = std::net::TcpListener::bind("127.0.0.1:0")
        .and_then(|listener| listener.local_addr())
        .expect("free loopback port");

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
    assert!(text(&output.stdout).contains("DOTCORE_APPLICATION_DEPLOYMENT_ID=app-1\n"));
    assert_eq!(
        std::fs::read_dir(empty.path())
            .expect("read working directory")
            .count(),
        0
    );
}

#[test]
fn help_lists_the_five_verbs_and_the_manager_flag() {
    let group = isolated_dotc()
        .args(["application", "--help"])
        .output()
        .expect("run help");
    let list = isolated_dotc()
        .args(["application", "list", "--help"])
        .output()
        .expect("run help");

    assert_eq!(group.status.code(), Some(0));
    let group = text(&group.stdout);
    for verb in ["list", "show", "pause", "resume", "teardown"] {
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

    assert_eq!(
        text(&paused.stderr),
        "Notice: Run `dotc application list` to follow its progress.\n"
    );
    assert_eq!(paused.status.code(), Some(0));
    assert_eq!(
        text(&paused.stdout),
        "Application deployment\nDOTCORE_APPLICATION_DEPLOYMENT_ID=app-1\nDOTCORE_APPLICATION_DEPLOYMENT_NAME=Chat\nDOTCORE_APPLICATION_DEPLOYMENT_STATE=Pausing\nDOTCORE_APPLICATION_DEPLOYMENT_DESIRED_STATE=Paused\n"
    );
    assert_eq!(
        manager.application_deployment("app-1").desired_state(),
        Paused
    );

    let resumed = run(&manager, &["resume", "app-4"]);

    assert_eq!(resumed.status.code(), Some(0));
    assert_eq!(
        text(&resumed.stdout),
        "Application deployment\nDOTCORE_APPLICATION_DEPLOYMENT_ID=app-4\nDOTCORE_APPLICATION_DEPLOYMENT_NAME=Docs\nDOTCORE_APPLICATION_DEPLOYMENT_STATE=Paused\nDOTCORE_APPLICATION_DEPLOYMENT_DESIRED_STATE=Present\n"
    );
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
    assert_eq!(
        text(&output.stdout),
        "Application deployment\nDOTCORE_APPLICATION_DEPLOYMENT_ID=app-3\nDOTCORE_APPLICATION_DEPLOYMENT_NAME=Wiki\nDOTCORE_APPLICATION_DEPLOYMENT_STATE=Tearing down\nDOTCORE_APPLICATION_DEPLOYMENT_DESIRED_STATE=Teardown\n"
    );
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

    let Some(run) = run_in_pty(terminal_command(&manager, &["list"]), Session::new([])) else {
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
        Session::new([]),
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
fn a_terminal_reports_a_pause_with_the_next_step() {
    let manager = FakeManager::start(FakeState::with_applications(mixed_deployments()));

    let Some(run) = run_in_pty(
        terminal_command(&manager, &["pause", "Chat"]),
        Session::new([]),
    ) else {
        return;
    };

    assert_eq!(run.code(), Some(0), "screen:\n{}", run.screen());
    assert_eq!(run.stdout(), "");
    assert_eq!(run.stderr(), "");
    let lines = run.screen().lines().map(str::trim).collect::<Vec<_>>();
    assert!(lines.contains(&"OK Pausing Chat"), "{}", run.screen());
    assert!(lines.contains(&"dotc application list"), "{}", run.screen());
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
            Step::Wait("Tearing down Chat"),
        ]),
    ) else {
        return;
    };

    assert_eq!(run.code(), Some(0), "screen:\n{}", run.screen());
    assert_eq!(run.stdout(), "");
    assert_eq!(run.stderr(), "");
    let screen = run
        .screen()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ");
    assert!(
        screen.contains(
            "Tear down Chat? This stops and removes its workloads and deletes its application data. The catalog application is kept."
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

/// The record block `application list` prints for one seeded deployment.
fn list_block(id: &str, name: &str, state: &str, stale: &str) -> String {
    block(&[
        ("ID", id),
        ("NAME", name),
        ("STATE", state),
        ("STALE", stale),
        ("NODE", "gb10-host"),
        ("APPLICATION", "Open WebUI"),
    ])
}

/// Renders `fields` as an application deployment record block.
fn block(fields: &[(&str, &str)]) -> String {
    let mut block = "Application deployment\n".to_owned();
    for (key, value) in fields {
        block.push_str(&format!("DOTCORE_APPLICATION_DEPLOYMENT_{key}={value}\n"));
    }
    block
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
