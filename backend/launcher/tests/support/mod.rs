//! Shared fixtures for launcher and supervisor integration tests: a state root with a supervisor
//! bound to the built launcher binary, shell-script workloads, and polling helpers.

#![allow(dead_code, reason = "each test target uses a different subset")]

use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    time::{Duration, Instant},
};

use dotcore_host_process as process;
use dotcore_launcher::{
    supervisor::{Supervisor, WorkloadState},
    workload::{
        EXIT_FILE, ExitRecord, LAUNCHER_FILE, LaunchSpec, LauncherRecord, Lifetime, LogRecord,
        LogStream, PROCESS_FILE, ProcessRecord, StoredRecord,
    },
};

/// How long a test waits for an expected launcher or workload change.
pub const TIMEOUT: Duration = Duration::from_secs(20);

/// State root in a canonical temporary directory with a supervisor bound to the built launcher.
pub struct Fixture {
    pub root: tempfile::TempDir,
    pub supervisor: Supervisor,
}

impl Fixture {
    pub fn new() -> Self {
        let root = tempfile::tempdir_in(std::env::temp_dir().canonicalize().unwrap()).unwrap();
        let supervisor = Supervisor::new(root.path(), launcher_binary()).unwrap();
        Self { root, supervisor }
    }

    pub fn dir(&self, id: &str) -> PathBuf {
        self.root.path().join(id)
    }

    /// Reads the exit record directly, including for workloads the scan reports absent.
    pub fn exit_record(&self, id: &str) -> ExitRecord {
        ExitRecord::decode(&std::fs::read(self.dir(id).join(EXIT_FILE)).unwrap()).unwrap()
    }

    /// Reads the launcher identity record of a workload.
    pub fn launcher_record(&self, id: &str) -> LauncherRecord {
        LauncherRecord::decode(&std::fs::read(self.dir(id).join(LAUNCHER_FILE)).unwrap()).unwrap()
    }

    /// Reads the held process record of a workload.
    pub fn process_record(&self, id: &str) -> ProcessRecord {
        ProcessRecord::decode(&std::fs::read(self.dir(id).join(PROCESS_FILE)).unwrap()).unwrap()
    }

    /// Waits until the workload's logs contain a record matching the predicate.
    pub async fn wait_for_log(
        &self,
        id: &str,
        matches: impl Fn(&LogRecord) -> bool,
    ) -> Vec<LogRecord> {
        let deadline = Instant::now() + TIMEOUT;
        loop {
            let records = self.supervisor.logs(id).await.unwrap();
            if records.iter().any(&matches) {
                return records;
            }
            assert!(Instant::now() < deadline, "log never matched: {records:?}");
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    }

    /// Stops a workload and returns its state, failing the test on a timeout.
    pub async fn stop(&self, id: &str) -> WorkloadState {
        tokio::time::timeout(TIMEOUT, self.supervisor.stop(id))
            .await
            .expect("stop timed out")
            .unwrap()
    }

    /// Waits for a workload's launcher to exit and returns its state.
    pub async fn wait(&self, id: &str) -> WorkloadState {
        tokio::time::timeout(TIMEOUT, self.supervisor.wait(id))
            .await
            .expect("wait timed out")
            .unwrap()
    }
}

impl Drop for Fixture {
    /// Stops launchers a failed test left running, so no workload outlives the test run.
    fn drop(&mut self) {
        let Ok(entries) = std::fs::read_dir(self.root.path()) else {
            return;
        };
        for entry in entries.flatten() {
            let Ok(bytes) = std::fs::read(entry.path().join(LAUNCHER_FILE)) else {
                continue;
            };
            let record = LauncherRecord::decode(&bytes).unwrap();
            if process::start_token(record.pid).unwrap() == Some(record.start_token) {
                // SAFETY: kill only sends a signal to the verified launcher.
                unsafe { libc::kill(record.pid as libc::pid_t, libc::SIGTERM) };
            }
        }
    }
}

/// Polls a condition until it holds, failing the test on a timeout.
pub async fn wait_until(what: &str, condition: impl Fn() -> bool) {
    let deadline = Instant::now() + TIMEOUT;
    while !condition() {
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

/// Whether a process with this PID and start token still runs.
pub fn running(pid: u32, start_token: u64) -> bool {
    process::start_token(pid).unwrap() == Some(start_token)
}

/// Sends a signal to a process.
pub fn signal(pid: u32, signal: libc::c_int) {
    // SAFETY: kill only sends a signal.
    assert_eq!(unsafe { libc::kill(pid as libc::pid_t, signal) }, 0);
}

pub fn launcher_binary() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_dotcore-launcher"))
}

/// Shell-script workload with this test's `PATH` as its only declared variable.
pub fn script(script: &str) -> LaunchSpec {
    LaunchSpec {
        executable: PathBuf::from("/bin/sh"),
        args: vec!["-c".to_owned(), script.to_owned()],
        environment: BTreeMap::from([("PATH".to_owned(), std::env::var("PATH").unwrap())]),
        grace_period_ms: 10_000,
        lifetime: Lifetime::Independent,
        owner: b"owner-1".to_vec(),
        fingerprint: "fingerprint-1".to_owned(),
    }
}

/// Workload that prints a line, stops gracefully on SIGTERM with exit code 3, and otherwise runs.
pub fn graceful_script() -> LaunchSpec {
    script("trap 'echo terminating; exit 3' TERM; echo ready; while :; do sleep 0.1; done")
}

/// Process standing in for a watched supervising process.
pub struct Watched {
    pub child: Child,
    pub lifetime: Lifetime,
}

impl Watched {
    pub fn spawn() -> Self {
        let child = Command::new("sleep")
            .arg("60")
            .stdin(Stdio::null())
            .spawn()
            .unwrap();
        let start_token = process::start_token(child.id()).unwrap().unwrap();
        Self {
            lifetime: Lifetime::Watch {
                pid: child.id(),
                start_token,
            },
            child,
        }
    }

    pub fn kill(&mut self, signal: libc::c_int) {
        // SAFETY: kill only sends a signal to the spawned child.
        assert_eq!(
            unsafe { libc::kill(self.child.id() as libc::pid_t, signal) },
            0
        );
        self.child.wait().unwrap();
    }
}

impl Drop for Watched {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

pub fn lines(records: &[LogRecord], stream: LogStream) -> Vec<String> {
    records
        .iter()
        .filter(|record| record.stream == stream)
        .map(|record| record.line.clone())
        .collect()
}

pub fn has_line(text: &str) -> impl Fn(&LogRecord) -> bool + '_ {
    move |record| record.line == text
}

pub fn exists(path: &Path) -> bool {
    path.try_exists().unwrap()
}
