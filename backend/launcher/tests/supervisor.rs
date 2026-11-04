//! Supervisor behavior across supervising processes: re-attaching after the creating process is
//! killed, classifying workloads that watch another process, and recreating an id after drain.

mod support;

use std::{
    io::{BufRead, BufReader},
    path::PathBuf,
    process::{Command, Stdio},
};

use dotcore_launcher::{
    supervisor::{AbsentReason, Supervisor, SupervisorError, WorkloadState},
    workload::{
        ExitReason, ExitRecord, ExitStatus, LAUNCH_FILE, LaunchSpec, LogStream, unix_time_ns,
    },
};

use support::{
    Fixture, TIMEOUT, Watched, graceful_script, has_line, running, script, signal, wait_until,
};

/// Environment variable naming the state root the helper process creates a workload in.
const HELPER_ROOT: &str = "DOTCORE_LAUNCHER_TEST_HELPER_ROOT";

#[tokio::test]
async fn a_new_supervisor_reattaches_after_the_creating_process_is_killed() {
    let fixture = Fixture::new();
    let mut helper = Command::new(std::env::current_exe().unwrap())
        .args([
            "--ignored",
            "--exact",
            "tests::create_and_hold_helper",
            "--nocapture",
            "--test-threads=1",
        ])
        .env(HELPER_ROOT, fixture.root.path())
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    let mut output = BufReader::new(helper.stdout.take().unwrap()).lines();
    assert!(
        output.any(|line| line.unwrap() == "created"),
        "helper never created the workload"
    );

    helper.kill().unwrap();
    helper.wait().unwrap();
    let killed_at = unix_time_ns();

    let WorkloadState::Running { payload } = fixture.supervisor.inspect("w1").await.unwrap() else {
        panic!("independent workload keeps running after its creator is killed");
    };
    assert_eq!(payload.spec, ticking_spec());
    assert_eq!(
        payload.bytes,
        std::fs::read(fixture.dir("w1").join(LAUNCH_FILE)).unwrap()
    );
    assert_eq!(
        fixture.supervisor.payload("w1").await.unwrap().bytes,
        payload.bytes
    );

    let mut follower = fixture.supervisor.follow_logs("w1").unwrap();
    let tick_after_kill = tokio::time::timeout(TIMEOUT, async {
        loop {
            let record = follower
                .next()
                .await
                .unwrap()
                .expect("record while running");
            if record.line == "tick" && record.time_ns > killed_at {
                return record;
            }
        }
    })
    .await
    .expect("a tick after the creator was killed");
    assert_eq!(tick_after_kill.stream, LogStream::Stdout);

    let state = fixture.stop("w1").await;
    assert!(matches!(
        state,
        WorkloadState::Exited {
            exit: ExitRecord {
                status: Some(ExitStatus::Code(0)),
                reason: ExitReason::Stopped,
                ..
            }
        }
    ));
    let rest = tokio::time::timeout(TIMEOUT, async {
        let mut rest = Vec::new();
        while let Some(record) = follower.next().await.unwrap() {
            rest.push(record.line);
        }
        rest
    })
    .await
    .expect("follow ends once the launcher exits");
    assert_eq!(rest.last().map(String::as_str), Some("stopping"));
}

#[tokio::test]
async fn a_workload_watching_another_process_is_terminating_and_can_be_stopped() {
    let fixture = Fixture::new();
    let watched = Watched::spawn();
    let spec = LaunchSpec {
        lifetime: watched.lifetime,
        ..graceful_script()
    };
    fixture.supervisor.create("w1", &spec).await.unwrap();

    let scanned = fixture.supervisor.scan().await.unwrap();
    assert_eq!(scanned.len(), 1);
    assert!(matches!(scanned[0].state, WorkloadState::Terminating));

    assert!(matches!(
        fixture.stop("w1").await,
        WorkloadState::Exited {
            exit: ExitRecord {
                reason: ExitReason::Stopped,
                ..
            }
        }
    ));
}

#[tokio::test]
async fn a_workload_watching_this_process_is_adoptable() {
    let fixture = Fixture::new();
    let spec = LaunchSpec {
        lifetime: fixture.supervisor.watch_this_process(),
        ..graceful_script()
    };
    fixture.supervisor.create("w1", &spec).await.unwrap();

    assert!(matches!(
        fixture.supervisor.inspect("w1").await.unwrap(),
        WorkloadState::Running { .. }
    ));
    fixture.stop("w1").await;
}

#[tokio::test]
async fn an_id_is_created_again_only_after_its_draining_launcher_is_gone() {
    let fixture = Fixture::new();
    let mut watched = Watched::spawn();
    let draining = LaunchSpec {
        lifetime: watched.lifetime,
        ..script(
            "trap 'echo draining; sleep 1; exit 0' TERM; echo ready; while :; do sleep 0.1; done",
        )
    };
    fixture.supervisor.create("w1", &draining).await.unwrap();
    fixture.wait_for_log("w1", has_line("ready")).await;

    watched.kill(libc::SIGKILL);
    fixture.wait_for_log("w1", has_line("draining")).await;

    assert!(matches!(
        fixture.supervisor.inspect("w1").await.unwrap(),
        WorkloadState::Terminating
    ));
    assert!(matches!(
        fixture.supervisor.create("w1", &graceful_script()).await,
        Err(SupervisorError::AlreadyExists { .. })
    ));
    assert!(matches!(
        fixture.supervisor.remove("w1").await,
        Err(SupervisorError::NotRemovable { .. })
    ));

    assert!(matches!(
        fixture.wait("w1").await,
        WorkloadState::Absent {
            reason: AbsentReason::WatchedProcessExited
        }
    ));
    fixture.supervisor.remove("w1").await.unwrap();
    fixture
        .supervisor
        .create("w1", &graceful_script())
        .await
        .unwrap();
    assert!(matches!(
        fixture.supervisor.inspect("w1").await.unwrap(),
        WorkloadState::Running { .. }
    ));
    fixture.stop("w1").await;
}

#[tokio::test]
async fn a_launcher_that_cannot_start_leaves_no_workload() {
    let fixture = Fixture::new();
    let supervisor = Supervisor::new(fixture.root.path(), "/nonexistent/dotcore-launcher").unwrap();

    assert!(matches!(
        supervisor.create("w1", &graceful_script()).await,
        Err(SupervisorError::Io { .. })
    ));
    assert!(supervisor.scan().await.unwrap().is_empty());
}

#[tokio::test]
async fn a_lost_workload_is_removed_only_after_its_process_group_is_killed() {
    let fixture = Fixture::new();
    fixture
        .supervisor
        .create("w1", &script("echo ready; exec sleep 60"))
        .await
        .unwrap();
    fixture.wait_for_log("w1", has_line("ready")).await;
    let launcher = fixture.launcher_record("w1");
    let held = fixture.process_record("w1");

    signal(launcher.pid, libc::SIGKILL);
    wait_until("launcher exit", || {
        !running(launcher.pid, launcher.start_token)
    })
    .await;
    assert!(matches!(
        fixture.supervisor.inspect("w1").await.unwrap(),
        WorkloadState::Lost
    ));
    // Linux kills the held process with its launcher; macOS leaves it for the supervisor.
    #[cfg(target_os = "linux")]
    wait_until("held process exit", || !running(held.pid, held.start_token)).await;

    tokio::time::timeout(TIMEOUT, fixture.supervisor.remove("w1"))
        .await
        .expect("remove timed out")
        .unwrap();

    assert!(!running(held.pid, held.start_token));
    assert!(!fixture.dir("w1").exists());
}

/// Independent workload printing a tick every 100 ms until stopped.
fn ticking_spec() -> LaunchSpec {
    script("trap 'echo stopping; exit 0' TERM; while :; do echo tick; sleep 0.1; done")
}

mod tests {
    use super::*;

    /// Creates a workload, reports it on stdout, and then waits to be killed.
    #[tokio::test]
    #[ignore = "subprocess helper"]
    async fn create_and_hold_helper() {
        // Plain `--ignored` runs reach this helper too; only the spawned subprocess acts.
        let Some(root) = std::env::var_os(HELPER_ROOT) else {
            return;
        };
        let supervisor = Supervisor::new(PathBuf::from(root), support::launcher_binary()).unwrap();
        supervisor.create("w1", &ticking_spec()).await.unwrap();
        // libtest prints the test name on the same line first.
        println!("\ncreated");
        std::future::pending::<()>().await;
    }
}
