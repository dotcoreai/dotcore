//! Process-level contracts for `dotc model`, run against the in-process fake manager.

use std::process::{Command, Output, Stdio};

use dotcore_proto::dotcore::model::v1::{
    ModelDeployment, ModelDeploymentDesiredState, ModelDeploymentFailure, ModelDeploymentProgress,
    ModelDeploymentStatus,
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
    let address = std::net::TcpListener::bind("127.0.0.1:0")
        .and_then(|listener| listener.local_addr())
        .expect("free loopback port");

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
fn help_lists_the_five_verbs_and_the_manager_flag() {
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
