//! Process-level round trip of `dotc application` against a real site manager whose application
//! target uses the simulated runtime, so no Docker or GPU is needed.

use std::process::Output;

use dotcore_proto::dotcore::application::v1::{
    ApplicationDeployment, ApplicationDeploymentDesiredState, ApplicationLifecycleState,
};

mod support;

use ApplicationDeploymentDesiredState::{Paused, Present, Teardown};

use support::{
    command::dotc_at,
    simulated_manager::{APPLICATION_NAME, APPLICATION_TARGET_ID, NODE_NAME, SimulatedManager},
};

#[test]
fn application_commands_drive_a_simulated_deployment_through_pause_resume_and_teardown() {
    let manager = SimulatedManager::start();
    let id = manager
        .create_application("Round trip")
        .application_deployment_id;
    // The simulated runtime publishes the requested port and runs nothing to probe, so the
    // deployment is ready once its group runs.
    let settled = manager
        .wait_for(&id, "a current Ready state", is_ready)
        .expect("settled deployment");
    let endpoint = settled
        .state
        .and_then(|state| state.endpoint)
        .expect("published endpoint")
        .url;

    let list = run(&manager, &["list"]);
    assert_eq!(text(&list.stderr), "");
    assert_eq!(list.status.code(), Some(0));
    assert_eq!(text(&list.stdout), list_row(&id, "Ready"));

    let show = run(&manager, &["show", "Round trip"]);
    assert_eq!(text(&show.stderr), "");
    assert_eq!(show.status.code(), Some(0));
    assert_eq!(
        text(&show.stdout),
        shown(&[
            ("name", "Round trip"),
            ("id", &id),
            ("state", "Ready"),
            ("desired", "Present"),
            ("readiness", "Ready"),
            ("node", NODE_NAME),
            ("target", APPLICATION_TARGET_ID),
            ("target id", APPLICATION_TARGET_ID),
            ("application", APPLICATION_NAME),
            ("service web", "Running"),
            ("endpoint", &endpoint),
        ])
    );

    // The simulated runtime retains twelve numbered lines for the deployment's one service.
    let logs = run(&manager, &["logs", "Round trip", "--lines", "3"]);
    assert_eq!(text(&logs.stderr), "");
    assert_eq!(logs.status.code(), Some(0));
    let lines = text(&logs.stdout);
    let lines = lines.lines().collect::<Vec<_>>();
    assert_eq!(lines.len(), 3, "{lines:?}");
    for (line, number) in lines.iter().zip(10..=12) {
        assert!(
            line.starts_with(&format!("simulated workload log line {number:03} owner="))
                && line.ends_with(" unit=web"),
            "{lines:?}"
        );
    }

    // The manager accepts resuming a present deployment as a no-op rather than rejecting it.
    let resumed = run(&manager, &["resume", "Round trip"]);
    assert_accepted(&resumed, "Resuming");
    assert_eq!(desired_state(&manager, &id), Some(Present));

    let paused = run(&manager, &["pause", "Round trip"]);
    assert_accepted(&paused, "Pausing");
    assert_eq!(desired_state(&manager, &id), Some(Paused));
    manager.wait_for(&id, "a current Paused state", |deployment| {
        is_current(deployment, ApplicationLifecycleState::Paused)
    });
    let list = run(&manager, &["list"]);
    assert_eq!(text(&list.stdout), list_row(&id, "Paused"));

    // Pausing again is accepted as well, and reports the settled state.
    let paused_again = run(&manager, &["pause", &id]);
    assert_accepted(&paused_again, "Pausing");

    let resumed = run(&manager, &["resume", &id]);
    assert_accepted(&resumed, "Resuming");
    assert_eq!(desired_state(&manager, &id), Some(Present));
    manager.wait_for(&id, "a current Ready state after resume", is_ready);

    let removed = run(&manager, &["teardown", "--yes", "Round trip"]);
    assert_accepted(&removed, "Tearing down");
    // The simulated runtime stops at once, so the manager may already have removed it here.
    assert!(matches!(
        desired_state(&manager, &id),
        None | Some(Teardown)
    ));
    manager.wait_for(&id, "removal", |deployment| deployment.is_none());

    let list = run(&manager, &["list"]);
    assert_eq!(list.status.code(), Some(0));
    assert_eq!(text(&list.stdout), "No application deployments\n");

    // A removed deployment is no longer listed, so the command stops before reaching the manager.
    let removed_again = run(&manager, &["teardown", "--yes", &id]);
    assert_eq!(removed_again.status.code(), Some(1));
    assert_eq!(text(&removed_again.stdout), "");
    assert_eq!(
        text(&removed_again.stderr),
        format!(
            "no application deployment is named or has id \"{id}\"; see `dotc application list`\n"
        )
    );
}

/// Asserts a lifecycle command succeeded and reported the progressive `verb` for the deployment.
fn assert_accepted(output: &Output, verb: &str) {
    assert_eq!(output.status.code(), Some(0), "{}", text(&output.stderr));
    assert_eq!(text(&output.stdout), format!("✔ {verb} Round trip\n"));
    assert_eq!(text(&output.stderr), "");
}

/// Whether the manager's view shows `lifecycle` from a current observation.
fn is_current(
    deployment: Option<&ApplicationDeployment>,
    lifecycle: ApplicationLifecycleState,
) -> bool {
    deployment
        .and_then(|deployment| deployment.state.as_ref())
        .is_some_and(|state| state.lifecycle_state == lifecycle as i32 && !state.observation_stale)
}

/// Whether the manager currently reports the deployment as Ready.
fn is_ready(deployment: Option<&ApplicationDeployment>) -> bool {
    is_current(deployment, ApplicationLifecycleState::Ready)
}

fn desired_state(
    manager: &SimulatedManager,
    id: &str,
) -> Option<ApplicationDeploymentDesiredState> {
    manager.application(id).map(|deployment| {
        ApplicationDeploymentDesiredState::try_from(deployment.desired_state)
            .expect("known desired state")
    })
}

/// The table `application list` prints for the one deployment of the test.
fn list_row(id: &str, state: &str) -> String {
    let (name, id_width, state_width) = ("Round trip", id.len().max(2), state.len().max(5));
    let node_width = NODE_NAME.len().max(4);
    format!(
        "  {:<10}  {:<id_width$}  {:<state_width$}  {:<node_width$}  APPLICATION\n  {name}  {id}  {state:<state_width$}  {NODE_NAME:<node_width$}  {APPLICATION_NAME}\n",
        "NAME", "ID", "STATE", "NODE"
    )
}

/// The labeled facts `application show` prints for the deployment, padded as the terminal pads
/// them.
fn shown(facts: &[(&str, &str)]) -> String {
    let width = facts
        .iter()
        .map(|(label, _)| label.len())
        .max()
        .unwrap_or(0);
    let mut text = "Application deployment Round trip\n".to_owned();
    for (label, value) in facts {
        text.push_str(&format!("  {label:<width$}  {value}\n"));
    }
    text
}

/// Runs `dotc application <arguments>` against the manager without any terminal.
fn run(manager: &SimulatedManager, arguments: &[&str]) -> Output {
    dotc_at("application", &manager.address().to_string(), arguments)
        .output()
        .expect("run dotc application")
}

fn text(value: &[u8]) -> String {
    String::from_utf8_lossy(value).into_owned()
}
