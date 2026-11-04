//! Launcher binary behavior with shell-script workloads: output capture, stop paths, exit
//! records, environment and working directory, log rotation, and watched lifetimes.

mod support;

use std::{
    collections::BTreeMap,
    os::{fd::AsRawFd, unix::process::CommandExt},
    path::PathBuf,
    process::{Command, Stdio},
    time::{Duration, Instant},
};

use dotcore_launcher::{
    supervisor::{AbsentReason, WorkloadState},
    workload::{
        ExitReason, ExitRecord, ExitStatus, LAUNCH_FILE, LOG_CAP_BYTES, LaunchSpec, Lifetime,
        LogStream, WORK_DIRECTORY, unix_time_ns,
    },
};

use support::{
    Fixture, Watched, exists, graceful_script, has_line, launcher_binary, lines, script, signal,
};

#[tokio::test]
async fn output_of_both_streams_is_captured_with_timestamps() {
    let fixture = Fixture::new();
    let before = unix_time_ns();

    fixture
        .supervisor
        .create("w1", &script("echo out; echo err >&2; exec sleep 30"))
        .await
        .unwrap();
    fixture.wait_for_log("w1", has_line("err")).await;
    let records = fixture.wait_for_log("w1", has_line("out")).await;

    assert_eq!(lines(&records, LogStream::Stdout), ["out"]);
    assert_eq!(lines(&records, LogStream::Stderr), ["err"]);
    let after = unix_time_ns();
    assert!(
        records
            .iter()
            .all(|record| (before..=after).contains(&record.time_ns))
    );
    assert!(matches!(
        fixture.supervisor.inspect("w1").await.unwrap(),
        WorkloadState::Running { .. }
    ));

    let WorkloadState::Exited { exit } = fixture.stop("w1").await else {
        panic!("stopped workload is exited");
    };
    assert_eq!(exit.status, Some(ExitStatus::Signal(libc::SIGTERM)));
    assert_eq!(exit.reason, ExitReason::Stopped);
}

#[tokio::test]
async fn stop_lets_the_process_exit_gracefully_on_sigterm() {
    let fixture = Fixture::new();
    fixture
        .supervisor
        .create("w1", &graceful_script())
        .await
        .unwrap();
    fixture.wait_for_log("w1", has_line("ready")).await;

    let started = Instant::now();
    let state = fixture.stop("w1").await;

    assert!(started.elapsed() < Duration::from_secs(5));
    assert!(matches!(
        state,
        WorkloadState::Exited {
            exit: ExitRecord {
                status: Some(ExitStatus::Code(3)),
                reason: ExitReason::Stopped,
                ..
            }
        }
    ));
    let records = fixture.supervisor.logs("w1").await.unwrap();
    assert_eq!(lines(&records, LogStream::Stdout), ["ready", "terminating"]);
}

#[tokio::test]
async fn stop_kills_the_process_group_after_the_grace_period() {
    let fixture = Fixture::new();
    let spec = LaunchSpec {
        grace_period_ms: 500,
        ..script("trap '' TERM; echo ready; while :; do sleep 0.1; done")
    };
    fixture.supervisor.create("w1", &spec).await.unwrap();
    fixture.wait_for_log("w1", has_line("ready")).await;

    let started = Instant::now();
    let state = fixture.stop("w1").await;

    assert!(started.elapsed() >= Duration::from_millis(500));
    assert!(matches!(
        state,
        WorkloadState::Exited {
            exit: ExitRecord {
                status: Some(ExitStatus::Signal(libc::SIGKILL)),
                reason: ExitReason::Stopped,
                ..
            }
        }
    ));
}

#[tokio::test]
async fn an_exit_on_its_own_is_recorded_with_its_status() {
    let fixture = Fixture::new();
    fixture
        .supervisor
        .create("w1", &script("echo bye; exit 7"))
        .await
        .unwrap();

    let state = fixture.wait("w1").await;

    assert!(matches!(
        state,
        WorkloadState::Exited {
            exit: ExitRecord {
                status: Some(ExitStatus::Code(7)),
                reason: ExitReason::Exited,
                ..
            }
        }
    ));
    let records = fixture.supervisor.logs("w1").await.unwrap();
    assert_eq!(lines(&records, LogStream::Stdout), ["bye"]);
}

#[tokio::test]
async fn a_missing_executable_is_recorded_as_a_spawn_failure() {
    let fixture = Fixture::new();
    let spec = LaunchSpec {
        executable: PathBuf::from("/nonexistent/executable"),
        ..script("")
    };
    fixture.supervisor.create("w1", &spec).await.unwrap();

    let state = fixture.wait("w1").await;

    assert!(matches!(
        state,
        WorkloadState::Exited {
            exit: ExitRecord {
                status: None,
                reason: ExitReason::SpawnFailed,
                ..
            }
        }
    ));
    let records = fixture.supervisor.logs("w1").await.unwrap();
    assert!(
        lines(&records, LogStream::Stderr)[0]
            .starts_with("dotcore-launcher: start /nonexistent/executable:")
    );
}

#[tokio::test]
async fn the_process_sees_only_declared_variables() {
    let fixture = Fixture::new();
    // Resolved from PATH because build sandboxes such as Nix's provide no `/usr/bin/env`.
    let env = std::env::split_paths(&std::env::var_os("PATH").unwrap())
        .map(|dir| dir.join("env"))
        .find(|path| path.is_file())
        .expect("env is on the test PATH");
    let spec = LaunchSpec {
        executable: env,
        args: Vec::new(),
        environment: BTreeMap::from([
            ("DECLARED_A".to_owned(), "1".to_owned()),
            ("DECLARED_B".to_owned(), "two words".to_owned()),
        ]),
        ..script("")
    };
    fixture.supervisor.create("w1", &spec).await.unwrap();

    fixture.wait("w1").await;

    let records = fixture.supervisor.logs("w1").await.unwrap();
    assert_eq!(
        lines(&records, LogStream::Stdout),
        ["DECLARED_A=1", "DECLARED_B=two words"]
    );
}

#[tokio::test]
async fn the_working_directory_is_the_work_directory_of_the_workload() {
    let fixture = Fixture::new();
    fixture
        .supervisor
        .create("w1", &script("pwd -P"))
        .await
        .unwrap();

    fixture.wait("w1").await;

    let records = fixture.supervisor.logs("w1").await.unwrap();
    assert_eq!(
        lines(&records, LogStream::Stdout),
        [fixture.dir("w1").join(WORK_DIRECTORY).to_str().unwrap()]
    );
}

#[tokio::test]
async fn output_beyond_the_log_cap_rotates_once_without_losing_records() {
    let fixture = Fixture::new();
    let (line, line_count) = cap_crossing_output();
    let spec = script(&format!("yes {line} | head -n {line_count}"));
    fixture.supervisor.create("w1", &spec).await.unwrap();

    fixture.wait("w1").await;

    let dir = fixture.dir("w1");
    let current = std::fs::metadata(dir.join("stdout.log")).unwrap().len();
    let rotated = std::fs::metadata(dir.join("stdout.log.1")).unwrap().len();
    assert!(current <= LOG_CAP_BYTES && rotated <= LOG_CAP_BYTES);
    assert!(current + rotated > LOG_CAP_BYTES);
    assert!(!exists(&dir.join("stdout.log.2")));
    let records = fixture.supervisor.logs("w1").await.unwrap();
    assert_eq!(records.len() as u64, line_count);
    assert!(records.iter().all(|record| record.line == line));
}

#[tokio::test]
async fn the_exit_of_the_watched_process_stops_the_workload_gracefully() {
    for signal in [libc::SIGTERM, libc::SIGKILL] {
        let fixture = Fixture::new();
        let mut watched = Watched::spawn();
        let spec = LaunchSpec {
            lifetime: watched.lifetime,
            ..graceful_script()
        };
        fixture.supervisor.create("w1", &spec).await.unwrap();
        fixture.wait_for_log("w1", has_line("ready")).await;

        watched.kill(signal);
        let state = fixture.wait("w1").await;

        assert!(
            matches!(
                state,
                WorkloadState::Absent {
                    reason: AbsentReason::WatchedProcessExited
                }
            ),
            "signal {signal}: {state:?}"
        );
        let exit = fixture.exit_record("w1");
        assert_eq!(exit.status, Some(ExitStatus::Code(3)), "signal {signal}");
        assert_eq!(exit.reason, ExitReason::WatchedProcessExited);
        let records = fixture.supervisor.logs("w1").await.unwrap();
        assert_eq!(lines(&records, LogStream::Stdout), ["ready", "terminating"]);
    }
}

#[tokio::test]
async fn a_watched_process_that_is_gone_or_replaced_stops_the_launch_at_once() {
    let mut gone = Watched::spawn();
    let gone_lifetime = gone.lifetime;
    gone.kill(libc::SIGKILL);
    let replaced = Lifetime::Watch {
        pid: std::process::id(),
        start_token: dotcore_host_process::start_token(std::process::id())
            .unwrap()
            .unwrap()
            + 1,
    };

    for lifetime in [gone_lifetime, replaced] {
        let fixture = Fixture::new();
        let spec = LaunchSpec {
            lifetime,
            ..script("echo started")
        };
        fixture.supervisor.create("w1", &spec).await.unwrap();

        let state = fixture.wait("w1").await;

        assert!(
            matches!(
                state,
                WorkloadState::Absent {
                    reason: AbsentReason::WatchedProcessExited
                }
            ),
            "{lifetime:?}: {state:?}"
        );
        assert_eq!(
            fixture.exit_record("w1"),
            ExitRecord {
                status: None,
                reason: ExitReason::WatchedProcessExited,
                ..fixture.exit_record("w1")
            }
        );
        assert!(fixture.supervisor.logs("w1").await.unwrap().is_empty());
    }
}

#[tokio::test]
async fn a_log_failure_keeps_the_process_running_and_its_exit_recorded() {
    let fixture = Fixture::new();
    let (line, line_count) = cap_crossing_output();
    let spec = script(&format!(
        "while [ ! -e go ]; do sleep 0.05; done; \
         yes {line} | head -n {line_count}; \
         echo after >&2"
    ));
    fixture.supervisor.create("w1", &spec).await.unwrap();
    // A non-empty directory where the rotated log goes makes the rotation fail.
    std::fs::create_dir_all(fixture.dir("w1").join("stdout.log.1/occupied")).unwrap();
    std::fs::write(fixture.dir("w1").join(WORK_DIRECTORY).join("go"), "").unwrap();

    let state = fixture.wait("w1").await;

    assert!(
        matches!(
            state,
            WorkloadState::Exited {
                exit: ExitRecord {
                    status: Some(ExitStatus::Code(0)),
                    reason: ExitReason::Exited,
                    ..
                }
            }
        ),
        "{state:?}"
    );
    std::fs::remove_dir_all(fixture.dir("w1").join("stdout.log.1")).unwrap();
    let records = fixture.supervisor.logs("w1").await.unwrap();
    assert!(
        lines(&records, LogStream::Stdout)
            .last()
            .unwrap()
            .starts_with("dotcore-launcher: stdout.log log disabled after a write failure:")
    );
    assert_eq!(lines(&records, LogStream::Stderr), ["after"]);
}

#[tokio::test]
async fn signals_other_than_stop_do_not_end_the_launcher() {
    let fixture = Fixture::new();
    fixture
        .supervisor
        .create("w1", &graceful_script())
        .await
        .unwrap();
    fixture.wait_for_log("w1", has_line("ready")).await;
    let launcher = fixture.launcher_record("w1").pid;

    for unrelated in [
        libc::SIGHUP,
        libc::SIGQUIT,
        libc::SIGUSR1,
        libc::SIGUSR2,
        libc::SIGALRM,
    ] {
        signal(launcher, unrelated);
    }
    tokio::time::sleep(Duration::from_millis(300)).await;

    assert!(matches!(
        fixture.supervisor.inspect("w1").await.unwrap(),
        WorkloadState::Running { .. }
    ));
    assert!(matches!(
        fixture.stop("w1").await,
        WorkloadState::Exited {
            exit: ExitRecord {
                status: Some(ExitStatus::Code(3)),
                reason: ExitReason::Stopped,
                ..
            }
        }
    ));
}

#[tokio::test]
async fn descriptors_leaked_to_the_launcher_do_not_reach_the_process() {
    let fixture = Fixture::new();
    let dir = fixture.dir("w1");
    std::fs::create_dir_all(dir.join(WORK_DIRECTORY)).unwrap();
    let spec = script("if ( : >&5 ) 2>/dev/null; then echo open; else echo closed; fi");
    std::fs::write(dir.join(LAUNCH_FILE), spec.encode().unwrap()).unwrap();
    let (_read, write) = std::io::pipe().unwrap();
    let leaked = write.as_raw_fd();

    let mut command = Command::new(launcher_binary());
    command.arg(&dir).stdin(Stdio::null());
    // SAFETY: the hook only calls dup2, which is async-signal-safe.
    unsafe {
        command.pre_exec(move || {
            // dup2 clears close-on-exec, so descriptor 5 is inherited like a leaked one.
            if libc::dup2(leaked, 5) < 0 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        })
    };
    assert!(command.status().unwrap().success());

    fixture.wait("w1").await;
    let records = fixture.supervisor.logs("w1").await.unwrap();
    assert_eq!(lines(&records, LogStream::Stdout), ["closed"]);
}

/// Length of each line in the log-cap tests: below the launcher's 64KiB record split, so every
/// line stays one record, yet long enough to cross the real cap in a few thousand records.
const CAP_CROSSING_LINE_BYTES: usize = 8 * 1024 - 1;

/// Line and line count whose stored records exceed one log cap but not two. Each record adds a
/// timestamp to the line and its newline, so the bare line count is already past the cap.
fn cap_crossing_output() -> (String, u64) {
    let line = "0123456789"
        .chars()
        .cycle()
        .take(CAP_CROSSING_LINE_BYTES)
        .collect();
    let line_count = LOG_CAP_BYTES / (CAP_CROSSING_LINE_BYTES as u64 + 1) + 128;
    (line, line_count)
}
