//! Creates launcher-held workloads under one state root and finds them again after the
//! supervising process restarts, for the native runtime.
//!
//! A create stages the workload directory under a temporary name, renames it into place, and only
//! then starts the launcher, so an interrupted create is always recognizable. A scan classifies
//! every workload from its files and live process checks, failing closed: a payload that does not
//! match its checksum, a record from another boot, or a launcher watching another process is never
//! reported as adoptable. Status needs no connection to the launcher: files, start tokens, and an
//! exit watch on the launcher process are enough.

mod logs;

use std::{
    collections::BTreeSet,
    fs, io,
    os::unix::fs::DirBuilderExt,
    path::{Path, PathBuf},
    process::Stdio,
    time::{Duration, SystemTime},
};

use dotcore_host_process::{self as process, ExitWatch};
use sha2::{Digest, Sha256};
use tokio::io::{Interest, unix::AsyncFd};

pub use self::logs::LogFollower;
use crate::workload::{
    ExitReason, ExitRecord, LAUNCH_CHECKSUM_FILE, LAUNCH_FILE, LaunchSpec, LauncherRecord,
    Lifetime, LogRecord, ProcessRecord, RecordError, SCHEMA_VERSION, StoredRecord, WORK_DIRECTORY,
    write_read_only_file,
};

/// Age after which a workload directory without a launcher record is an interrupted create.
pub const INTERRUPTED_CREATE_AGE: Duration = Duration::from_secs(60);

/// Longest wait for a started launcher to detach; the launcher bounds its own wait below this.
const START_TIMEOUT: Duration = Duration::from_secs(20);

/// Name prefix of a workload directory that is still being staged.
const STAGING_PREFIX: &str = ".create-";

/// File name of the launcher binary, expected beside the supervising executable.
const LAUNCHER_BINARY: &str = "dotcore-launcher";

/// Client for the workloads under one state root, bound to the current process's identity.
#[derive(Clone, Debug)]
pub struct Supervisor {
    root: PathBuf,
    launcher: PathBuf,
    pid: u32,
    start_token: u64,
    boot_id: String,
}

impl Supervisor {
    /// Opens a state root, creating it when missing, with the launcher binary to start. A
    /// relative root is resolved against the current directory once, here.
    pub fn new(root: impl Into<PathBuf>, launcher: impl Into<PathBuf>) -> Result<Self> {
        let root = root.into();
        fs::create_dir_all(&root).map_err(|source| SupervisorError::Io {
            context: "create workload state root",
            source,
        })?;
        let root = fs::canonicalize(&root).map_err(|source| SupervisorError::Io {
            context: "resolve workload state root",
            source,
        })?;
        let pid = std::process::id();
        let start_token = process::start_token(pid)
            .and_then(|token| token.ok_or_else(|| anyhow::anyhow!("current process not found")))
            .map_err(|source| SupervisorError::Process {
                context: "read supervising process start token",
                source,
            })?;
        let boot_id = process::boot_id().map_err(|source| SupervisorError::Process {
            context: "read boot id",
            source,
        })?;
        Ok(Self {
            root,
            launcher: launcher.into(),
            pid,
            start_token,
            boot_id,
        })
    }

    /// Lifetime that stops a workload once the current process exits.
    pub fn watch_this_process(&self) -> Lifetime {
        Lifetime::Watch {
            pid: self.pid,
            start_token: self.start_token,
        }
    }

    /// Creates a workload and starts its launcher, returning once the launcher recorded itself.
    ///
    /// Fails when any directory for the id exists, including one still draining; wait for or
    /// stop it and remove it first. A launcher that does not detach within a bounded time leaves
    /// its directory in place, for a later scan to classify.
    pub async fn create(&self, id: &str, spec: &LaunchSpec) -> Result<()> {
        let payload = spec.encode().map_err(SupervisorError::Encode)?;
        let id = id.to_owned();
        let dir = self
            .blocking({
                let id = id.clone();
                move |supervisor| {
                    supervisor.stage(&id, &payload)?;
                    supervisor.publish(&id)
                }
            })
            .await?;

        let error = match self.start_launcher(&dir).await {
            Ok(()) => return Ok(()),
            Err(error @ SupervisorError::LaunchTimedOut) => return Err(error),
            Err(error) => error,
        };
        // The launcher reported that it stopped before recording itself; the directory is
        // removed only when no launcher record says otherwise.
        self.blocking(move |supervisor| {
            if supervisor
                .live_launcher(&supervisor.workload_dir(&id)?)?
                .is_none()
            {
                supervisor.remove_dirs(&id)?;
            }
            Ok(())
        })
        .await?;
        Err(error)
    }

    /// Classifies every workload under the state root.
    pub async fn scan(&self) -> Result<Vec<ScannedWorkload>> {
        self.blocking(|supervisor| {
            let entries = fs::read_dir(&supervisor.root).map_err(|source| SupervisorError::Io {
                context: "list workload state root",
                source,
            })?;
            let mut ids = BTreeSet::new();
            for entry in entries {
                let entry = entry.map_err(|source| SupervisorError::Io {
                    context: "list workload state root",
                    source,
                })?;
                let name = entry.file_name().to_string_lossy().into_owned();
                let id = name.strip_prefix(STAGING_PREFIX).unwrap_or(&name);
                if valid_id(id) {
                    ids.insert(id.to_owned());
                }
            }
            ids.into_iter()
                .map(|id| {
                    Ok(ScannedWorkload {
                        state: supervisor.classify(&id)?,
                        id,
                    })
                })
                .collect()
        })
        .await
    }

    /// Classifies one workload.
    pub async fn inspect(&self, id: &str) -> Result<WorkloadState> {
        let id = id.to_owned();
        self.blocking(move |supervisor| supervisor.classify(&id))
            .await
    }

    /// Reads back the exact launch payload written at create and verifies its checksum.
    pub async fn payload(&self, id: &str) -> Result<LaunchPayload> {
        let id = id.to_owned();
        self.blocking(move |supervisor| {
            read_payload(&supervisor.workload_dir(&id)?).map_err(SupervisorError::Inconsistent)
        })
        .await
    }

    /// Waits until the workload's launcher exits, then classifies the workload.
    pub async fn wait(&self, id: &str) -> Result<WorkloadState> {
        self.signal_and_wait(id, None).await
    }

    /// Asks the workload's launcher to stop gracefully, waits until it exits, then classifies
    /// the workload. A workload whose launcher is not running is only classified.
    pub async fn stop(&self, id: &str) -> Result<WorkloadState> {
        self.signal_and_wait(id, Some(libc::SIGTERM)).await
    }

    /// Removes a finished workload: exited, absent, or lost. A lost workload's held process
    /// group is killed first when its recorded process still runs. Every other state is refused,
    /// including creates that may still be running and files that cannot be trusted.
    pub async fn remove(&self, id: &str) -> Result<()> {
        let watch = {
            let id = id.to_owned();
            self.blocking(move |supervisor| match supervisor.classify(&id)? {
                WorkloadState::Exited { .. } | WorkloadState::Absent { .. } => Ok(None),
                WorkloadState::Lost => supervisor.kill_held_group(&supervisor.workload_dir(&id)?),
                _ => Err(SupervisorError::NotRemovable { id }),
            })
            .await?
        };
        wait_for_exit(watch).await?;
        let id = id.to_owned();
        self.blocking(move |supervisor| supervisor.remove_dirs(&id))
            .await
    }

    /// Removes an interrupted create. Only the owner of the state root calls this, once it knows
    /// no create of its own is still running for the id.
    pub async fn remove_interrupted(&self, id: &str) -> Result<()> {
        let id = id.to_owned();
        self.blocking(move |supervisor| match supervisor.classify(&id)? {
            WorkloadState::Interrupted => supervisor.remove_dirs(&id),
            _ => Err(SupervisorError::NotRemovable { id }),
        })
        .await
    }

    /// Returns the retained log records of both streams, oldest first.
    pub async fn logs(&self, id: &str) -> Result<Vec<LogRecord>> {
        let id = id.to_owned();
        self.blocking(move |supervisor| {
            logs::read_all(&supervisor.workload_dir(&id)?).map_err(|source| SupervisorError::Io {
                context: "read workload logs",
                source,
            })
        })
        .await
    }

    /// Follows the workload's logs from the oldest retained record until its launcher exits.
    pub fn follow_logs(&self, id: &str) -> Result<LogFollower> {
        let dir = self.workload_dir(id)?;
        Ok(LogFollower::new(self.clone(), dir))
    }

    /// Runs the launcher for a published workload directory until it has detached.
    async fn start_launcher(&self, dir: &Path) -> Result<()> {
        let output = tokio::process::Command::new(&self.launcher)
            .arg(dir)
            .env_clear()
            .current_dir(dir)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .kill_on_drop(true)
            .output();
        let output = tokio::time::timeout(START_TIMEOUT, output)
            .await
            .map_err(|_| SupervisorError::LaunchTimedOut)?
            .map_err(|source| SupervisorError::Io {
                context: "start launcher",
                source,
            })?;
        if output.status.success() {
            return Ok(());
        }
        Err(SupervisorError::LaunchFailed {
            message: String::from_utf8_lossy(&output.stderr).trim().to_owned(),
        })
    }

    async fn signal_and_wait(
        &self,
        id: &str,
        signal: Option<libc::c_int>,
    ) -> Result<WorkloadState> {
        let watch = {
            let id = id.to_owned();
            self.blocking(move |supervisor| {
                let dir = supervisor.workload_dir(&id)?;
                if let Some(record) = supervisor.live_launcher(&dir)? {
                    return signal_verified(record.pid, record.start_token, signal, false);
                }
                // Without its launcher, a lost workload's held process group is stopped directly.
                if signal.is_some() && matches!(supervisor.classify(&id)?, WorkloadState::Lost) {
                    return supervisor.kill_held_group(&dir);
                }
                Ok(None)
            })
            .await?
        };
        wait_for_exit(watch).await?;
        self.inspect(id).await
    }

    /// Returns the launcher record when that launcher runs in this boot with its recorded token,
    /// `None` when there is no record or its launcher is gone, and an error when the record
    /// cannot be trusted, so callers never treat an unreadable record as a stopped launcher.
    fn live_launcher(&self, dir: &Path) -> Result<Option<LauncherRecord>> {
        let Some(record) =
            read_record::<LauncherRecord>(dir).map_err(SupervisorError::Inconsistent)?
        else {
            return Ok(None);
        };
        if record.boot_id != self.boot_id {
            return Ok(None);
        }
        let token = process::start_token(record.pid).map_err(process_error)?;
        Ok((token == Some(record.start_token)).then_some(record))
    }

    /// Kills the process group of a lost workload when its recorded held process still runs,
    /// returning a watch on that process. Without a record no process was started; an
    /// untrustworthy record is refused.
    fn kill_held_group(&self, dir: &Path) -> Result<Option<ExitWatch>> {
        let Some(record) =
            read_record::<ProcessRecord>(dir).map_err(SupervisorError::Inconsistent)?
        else {
            return Ok(None);
        };
        // The held process leads its own group, so the group id is its PID; the group is only
        // signalled while that leader is verified, which keeps the id from naming another group.
        signal_verified(record.pid, record.start_token, Some(libc::SIGKILL), true)
    }

    fn classify(&self, id: &str) -> Result<WorkloadState> {
        let dir = self.workload_dir(id)?;
        let record = match read_record::<LauncherRecord>(&dir) {
            Ok(Some(record)) => record,
            Ok(None) => {
                let staging = self.staging_dir(id);
                return match [&dir, &staging].into_iter().find(|path| path.exists()) {
                    Some(path) => unrecorded_state(id, path),
                    None => Err(SupervisorError::NotFound { id: id.to_owned() }),
                };
            }
            Err(problem) => return Ok(WorkloadState::Inconsistent { problem }),
        };
        if record.schema_version != SCHEMA_VERSION {
            return Ok(WorkloadState::Inconsistent {
                problem: Inconsistency::UnsupportedSchema {
                    version: record.schema_version,
                },
            });
        }
        if record.boot_id != self.boot_id {
            return Ok(WorkloadState::Absent {
                reason: AbsentReason::Rebooted,
            });
        }
        if let Some(state) = exit_state(&dir) {
            return Ok(state);
        }
        if process::start_token(record.pid).map_err(process_error)? != Some(record.start_token) {
            // The launcher writes its exit record just before exiting, so look once more.
            return Ok(exit_state(&dir).unwrap_or(WorkloadState::Lost));
        }

        let payload = match read_payload(&dir) {
            Ok(payload) => payload,
            Err(problem) => return Ok(WorkloadState::Inconsistent { problem }),
        };
        Ok(match payload.spec.lifetime {
            Lifetime::Watch { pid, start_token }
                if (pid, start_token) != (self.pid, self.start_token) =>
            {
                WorkloadState::Terminating
            }
            _ => WorkloadState::Running { payload },
        })
    }

    /// Writes the launch payload and an empty working directory into a fresh staging directory,
    /// both accessible only by the current user.
    fn stage(&self, id: &str, payload: &[u8]) -> Result<()> {
        let dir = self.workload_dir(id)?;
        let staging = self.staging_dir(id);
        if dir.exists() {
            return Err(SupervisorError::AlreadyExists { id: id.to_owned() });
        }
        let mut builder = fs::DirBuilder::new();
        builder.mode(0o700);
        match builder.create(&staging) {
            Ok(()) => {}
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
                return Err(SupervisorError::AlreadyExists { id: id.to_owned() });
            }
            Err(source) => {
                return Err(SupervisorError::Io {
                    context: "create workload staging directory",
                    source,
                });
            }
        }
        builder
            .create(staging.join(WORK_DIRECTORY))
            .and_then(|()| write_read_only_file(&staging.join(LAUNCH_FILE), payload))
            .and_then(|()| {
                write_read_only_file(
                    &staging.join(LAUNCH_CHECKSUM_FILE),
                    sha256_hex(payload).as_bytes(),
                )
            })
            .map_err(|source| SupervisorError::Io {
                context: "write launch specification",
                source,
            })
    }

    /// Moves a staged workload directory into place and returns its final path. Losing a race
    /// with another create of the id removes this create's staging directory.
    fn publish(&self, id: &str) -> Result<PathBuf> {
        let dir = self.workload_dir(id)?;
        let staging = self.staging_dir(id);
        match fs::rename(&staging, &dir) {
            Ok(()) => Ok(dir),
            Err(error)
                if matches!(
                    error.raw_os_error(),
                    Some(libc::ENOTEMPTY | libc::EEXIST | libc::EISDIR)
                ) =>
            {
                fs::remove_dir_all(&staging).map_err(|source| SupervisorError::Io {
                    context: "remove workload staging directory",
                    source,
                })?;
                Err(SupervisorError::AlreadyExists { id: id.to_owned() })
            }
            Err(source) => Err(SupervisorError::Io {
                context: "publish workload directory",
                source,
            }),
        }
    }

    fn remove_dirs(&self, id: &str) -> Result<()> {
        for dir in [self.workload_dir(id)?, self.staging_dir(id)] {
            match fs::remove_dir_all(&dir) {
                Ok(()) => {}
                Err(error) if error.kind() == io::ErrorKind::NotFound => {}
                Err(source) => {
                    return Err(SupervisorError::Io {
                        context: "remove workload directory",
                        source,
                    });
                }
            }
        }
        Ok(())
    }

    fn workload_dir(&self, id: &str) -> Result<PathBuf> {
        if !valid_id(id) {
            return Err(SupervisorError::InvalidId { id: id.to_owned() });
        }
        Ok(self.root.join(id))
    }

    fn staging_dir(&self, id: &str) -> PathBuf {
        self.root.join(format!("{STAGING_PREFIX}{id}"))
    }

    /// Runs filesystem and process-table work outside the async executor.
    async fn blocking<T: Send + 'static>(
        &self,
        task: impl FnOnce(&Supervisor) -> Result<T> + Send + 'static,
    ) -> Result<T> {
        let supervisor = self.clone();
        tokio::task::spawn_blocking(move || task(&supervisor))
            .await
            .map_err(SupervisorError::Task)?
    }
}

/// Signals a process, or the process group it leads, after verifying its start token, and
/// returns a watch on its exit, or `None` when it is not running.
fn signal_verified(
    pid: u32,
    start_token: u64,
    signal: Option<libc::c_int>,
    group: bool,
) -> Result<Option<ExitWatch>> {
    // Zero or a PID beyond `pid_t` would turn the signal into a process-group broadcast.
    let Ok(target) = libc::pid_t::try_from(pid) else {
        return Ok(None);
    };
    if target == 0 {
        return Ok(None);
    }
    let Some(watch) = ExitWatch::register(pid).map_err(process_error)? else {
        return Ok(None);
    };
    // Checked after registering: an exit in between still wakes the watch, and a reused PID is
    // never signalled.
    if process::start_token(pid).map_err(process_error)? != Some(start_token) {
        return Ok(None);
    }
    if let Some(signal) = signal {
        let target = if group { -target } else { target };
        // SAFETY: kill only sends a signal to the verified process or the group it leads.
        unsafe { libc::kill(target, signal) };
    }
    Ok(Some(watch))
}

/// Waits until a watched process exits.
async fn wait_for_exit(watch: Option<ExitWatch>) -> Result<()> {
    let Some(watch) = watch else {
        return Ok(());
    };
    // SAFETY: ExitWatch owns its descriptor, and it is not replaced or closed while registered.
    let watch =
        unsafe { AsyncFd::register_with_interest(watch, Interest::READABLE) }.map_err(|error| {
            SupervisorError::Io {
                context: "register process exit watch",
                source: error.into(),
            }
        })?;
    let _ready = watch
        .readable()
        .await
        .map_err(|source| SupervisorError::Io {
            context: "wait for process exit",
            source,
        })?;
    Ok(())
}

/// Returns the launcher binary path beside the current executable, after resolving symlinks in
/// the executable's path, so a symlinked or wrapped executable finds the launcher installed next
/// to the real binary.
pub fn launcher_beside_current_exe() -> io::Result<PathBuf> {
    let executable = fs::canonicalize(std::env::current_exe()?)?;
    let dir = executable
        .parent()
        .ok_or_else(|| io::Error::other("current executable has no parent directory"))?;
    Ok(dir.join(LAUNCHER_BINARY))
}

/// Result type of supervisor operations.
pub type Result<T, E = SupervisorError> = std::result::Result<T, E>;

/// Failure of a supervisor operation.
#[derive(Debug, thiserror::Error)]
pub enum SupervisorError {
    #[error("invalid workload id {id:?}")]
    InvalidId { id: String },

    #[error("workload already exists: {id}")]
    AlreadyExists { id: String },

    #[error("workload not found: {id}")]
    NotFound { id: String },

    #[error("workload cannot be removed in its current state: {id}")]
    NotRemovable { id: String },

    #[error("launcher failed to start: {message}")]
    LaunchFailed { message: String },

    #[error("launcher did not detach within the start timeout")]
    LaunchTimedOut,

    #[error("launch payload is inconsistent")]
    Inconsistent(#[source] Inconsistency),

    #[error("encode launch specification")]
    Encode(#[source] RecordError),

    #[error("{context}")]
    Io {
        context: &'static str,
        #[source]
        source: io::Error,
    },

    #[error("{context}")]
    Process {
        context: &'static str,
        #[source]
        source: anyhow::Error,
    },

    #[error("supervisor background task failed")]
    Task(#[source] tokio::task::JoinError),
}

/// One workload found by a scan.
#[derive(Debug)]
pub struct ScannedWorkload {
    pub id: String,
    pub state: WorkloadState,
}

/// What the files and live process checks say about one workload.
#[derive(Debug)]
pub enum WorkloadState {
    /// No launcher record yet, recently enough that a create may still be running.
    Creating,

    /// No launcher record long after the directory was written: an interrupted create that only
    /// the owner of the state root removes.
    Interrupted,

    /// The launcher runs and the workload may be adopted: independent, or watching this process.
    Running { payload: LaunchPayload },

    /// The launcher runs but watches another process, so it stops once that process is gone and
    /// is never adopted; wait for or stop it before creating the id again.
    Terminating,

    /// The launcher finished and recorded how the workload process ended.
    Exited { exit: ExitRecord },

    /// The launcher is gone without an exit record, for example after being killed.
    Lost,

    /// The workload is conclusively gone and its directory can be removed.
    Absent { reason: AbsentReason },

    /// The workload's files cannot be trusted; the workload is never adopted.
    Inconsistent { problem: Inconsistency },
}

/// Why a workload is conclusively gone.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AbsentReason {
    /// The host rebooted since the launcher started.
    Rebooted,

    /// The launcher stopped the workload because its watched process exited.
    WatchedProcessExited,
}

/// Workload file that cannot be trusted.
#[derive(Debug, thiserror::Error)]
pub enum Inconsistency {
    #[error("read workload file {file}")]
    Unreadable {
        file: &'static str,
        #[source]
        source: io::Error,
    },

    #[error("parse workload file {file}")]
    Malformed {
        file: &'static str,
        #[source]
        source: RecordError,
    },

    #[error("launch specification does not match its checksum")]
    ChecksumMismatch,

    #[error("unsupported launcher record schema version {version}")]
    UnsupportedSchema { version: u32 },
}

/// Launch payload exactly as written at create, with its decoded specification. Callers compare
/// the decoded specification, never re-encoded bytes, because map encoding order is unstable.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LaunchPayload {
    pub bytes: Vec<u8>,
    pub spec: LaunchSpec,
}

fn read_payload(dir: &Path) -> Result<LaunchPayload, Inconsistency> {
    let read = |file: &'static str| {
        fs::read(dir.join(file)).map_err(|source| Inconsistency::Unreadable { file, source })
    };
    let bytes = read(LAUNCH_FILE)?;
    let checksum = read(LAUNCH_CHECKSUM_FILE)?;
    if checksum.trim_ascii() != sha256_hex(&bytes).as_bytes() {
        return Err(Inconsistency::ChecksumMismatch);
    }
    let spec = LaunchSpec::decode(&bytes).map_err(|source| Inconsistency::Malformed {
        file: LAUNCH_FILE,
        source,
    })?;
    Ok(LaunchPayload { bytes, spec })
}

/// Reads and decodes one stored record, or returns `None` when its file does not exist.
fn read_record<R: StoredRecord>(dir: &Path) -> Result<Option<R>, Inconsistency> {
    let bytes = match fs::read(dir.join(R::FILE)) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(source) => {
            return Err(Inconsistency::Unreadable {
                file: R::FILE,
                source,
            });
        }
    };
    R::decode(&bytes)
        .map(Some)
        .map_err(|source| Inconsistency::Malformed {
            file: R::FILE,
            source,
        })
}

/// Classifies a finished workload from its exit record, or returns `None` without one.
fn exit_state(dir: &Path) -> Option<WorkloadState> {
    match read_record::<ExitRecord>(dir) {
        Ok(None) => None,
        Ok(Some(exit)) if exit.reason == ExitReason::WatchedProcessExited => {
            Some(WorkloadState::Absent {
                reason: AbsentReason::WatchedProcessExited,
            })
        }
        Ok(Some(exit)) => Some(WorkloadState::Exited { exit }),
        Err(problem) => Some(WorkloadState::Inconsistent { problem }),
    }
}

/// Classifies a directory without a launcher record by how long ago it was last written.
fn unrecorded_state(id: &str, dir: &Path) -> Result<WorkloadState> {
    let modified = fs::metadata(dir)
        .and_then(|metadata| metadata.modified())
        .map_err(|source| match source.kind() {
            io::ErrorKind::NotFound => SupervisorError::NotFound { id: id.to_owned() },
            _ => SupervisorError::Io {
                context: "read workload directory age",
                source,
            },
        })?;
    let age = SystemTime::now()
        .duration_since(modified)
        .unwrap_or_default();
    Ok(if age < INTERRUPTED_CREATE_AGE {
        WorkloadState::Creating
    } else {
        WorkloadState::Interrupted
    })
}

/// Accepts ids usable as one directory name that cannot collide with staging names.
fn valid_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 128
        && !id.starts_with('.')
        && id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'))
}

fn sha256_hex(bytes: &[u8]) -> String {
    Sha256::digest(bytes)
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

fn process_error(source: anyhow::Error) -> SupervisorError {
    SupervisorError::Process {
        context: "read launcher process facts",
        source,
    }
}

#[cfg(test)]
mod tests {
    use std::{collections::BTreeMap, fs::File};

    use super::*;
    use crate::workload::LAUNCHER_FILE;

    #[test]
    fn a_pid_that_names_no_single_process_is_never_signalled() {
        // The null signal keeps the check harmless even if the guard failed.
        for pid in [0, u32::MAX] {
            assert!(signal_verified(pid, 0, Some(0), true).unwrap().is_none());
        }
    }

    #[test]
    fn an_unrecorded_directory_removed_before_its_age_read_is_not_found() {
        let (_root, supervisor) = supervisor();
        supervisor.stage("w1", b"{}").unwrap();
        let dir = supervisor.staging_dir("w1");
        fs::remove_dir_all(&dir).unwrap();

        assert!(matches!(
            unrecorded_state("w1", &dir),
            Err(SupervisorError::NotFound { id }) if id == "w1"
        ));
    }

    #[test]
    fn other_directory_age_errors_are_reported() {
        let (_root, supervisor) = supervisor();
        let file = supervisor.root.join("not-a-directory");
        File::create(&file).unwrap();

        assert!(matches!(
            unrecorded_state("w1", &file.join("child")),
            Err(SupervisorError::Io { context: "read workload directory age", source })
                if source.kind() == io::ErrorKind::NotADirectory
        ));
    }

    #[tokio::test]
    async fn a_crash_after_staging_leaves_a_creating_then_interrupted_workload() {
        let (_root, supervisor) = supervisor();
        supervisor.stage("w1", b"{}").unwrap();

        assert!(matches!(
            supervisor.inspect("w1").await.unwrap(),
            WorkloadState::Creating
        ));
        assert_not_removable(&supervisor).await;
        backdate(&supervisor.staging_dir("w1"));
        let scanned = supervisor.scan().await.unwrap();
        assert_eq!(scanned.len(), 1);
        assert_eq!(scanned[0].id, "w1");
        assert!(matches!(scanned[0].state, WorkloadState::Interrupted));
        assert!(matches!(
            supervisor.create("w1", &spec()).await,
            Err(SupervisorError::AlreadyExists { .. })
        ));
        assert!(matches!(
            supervisor.remove("w1").await,
            Err(SupervisorError::NotRemovable { .. })
        ));

        supervisor.remove_interrupted("w1").await.unwrap();
        assert!(supervisor.scan().await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn a_crash_after_publishing_leaves_a_creating_then_interrupted_workload() {
        let (_root, supervisor) = supervisor();
        let payload = spec().encode().unwrap();
        supervisor.stage("w1", &payload).unwrap();
        let dir = supervisor.publish("w1").unwrap();

        assert!(matches!(
            supervisor.inspect("w1").await.unwrap(),
            WorkloadState::Creating
        ));
        backdate(&dir);
        assert!(matches!(
            supervisor.inspect("w1").await.unwrap(),
            WorkloadState::Interrupted
        ));
        assert_eq!(supervisor.payload("w1").await.unwrap().bytes, payload);
        assert!(matches!(
            supervisor.remove("w1").await,
            Err(SupervisorError::NotRemovable { .. })
        ));

        supervisor.remove_interrupted("w1").await.unwrap();
        assert!(!dir.exists());
    }

    #[tokio::test]
    async fn a_record_from_another_boot_is_conclusively_absent() {
        let (_root, supervisor) = supervisor();
        let dir = fabricated_workload(&supervisor, std::process::id(), supervisor.start_token);
        write_record(
            &dir,
            std::process::id(),
            supervisor.start_token,
            "previous-boot",
        );

        assert!(matches!(
            supervisor.inspect("w1").await.unwrap(),
            WorkloadState::Absent {
                reason: AbsentReason::Rebooted
            }
        ));
        // A launcher from another boot is never signalled, even when its PID and token match.
        assert!(matches!(
            supervisor.stop("w1").await.unwrap(),
            WorkloadState::Absent { .. }
        ));
    }

    #[tokio::test]
    async fn a_reused_pid_with_another_start_token_is_not_the_launcher() {
        let (_root, supervisor) = supervisor();
        // This process stands in for an unrelated process that reused the launcher's PID.
        fabricated_workload(&supervisor, std::process::id(), supervisor.start_token + 1);

        assert!(matches!(
            supervisor.inspect("w1").await.unwrap(),
            WorkloadState::Lost
        ));
        assert!(matches!(
            supervisor.stop("w1").await.unwrap(),
            WorkloadState::Lost
        ));
        supervisor.remove("w1").await.unwrap();
    }

    #[tokio::test]
    async fn a_live_record_is_running_and_cannot_be_removed() {
        let (_root, supervisor) = supervisor();
        fabricated_workload(&supervisor, std::process::id(), supervisor.start_token);

        assert!(matches!(
            supervisor.inspect("w1").await.unwrap(),
            WorkloadState::Running { .. }
        ));
        assert_not_removable(&supervisor).await;
        assert!(matches!(
            supervisor.remove_interrupted("w1").await,
            Err(SupervisorError::NotRemovable { .. })
        ));
    }

    #[tokio::test]
    async fn modified_or_truncated_payloads_are_inconsistent() {
        let (_root, supervisor) = supervisor();
        let dir = fabricated_workload(&supervisor, std::process::id(), supervisor.start_token);
        let original = fs::read(dir.join(LAUNCH_FILE)).unwrap();

        let modified = LaunchSpec {
            fingerprint: "fingerprint-2".to_owned(),
            ..spec()
        };
        replace_file(&dir.join(LAUNCH_FILE), &modified.encode().unwrap());
        assert_inconsistent_payload(&supervisor).await;

        replace_file(&dir.join(LAUNCH_FILE), &original[..original.len() / 2]);
        assert_inconsistent_payload(&supervisor).await;

        replace_file(&dir.join(LAUNCH_FILE), &original);
        replace_file(&dir.join(LAUNCH_CHECKSUM_FILE), b"");
        assert_inconsistent_payload(&supervisor).await;
    }

    #[tokio::test]
    async fn malformed_records_are_inconsistent() {
        let (_root, supervisor) = supervisor();
        let dir = fabricated_workload(&supervisor, std::process::id(), supervisor.start_token);

        replace_file(&dir.join(LAUNCHER_FILE), b"\xff");
        assert!(matches!(
            supervisor.inspect("w1").await.unwrap(),
            WorkloadState::Inconsistent {
                problem: Inconsistency::Malformed { .. }
            }
        ));
        // An unreadable record never counts as a stopped launcher.
        assert_not_removable(&supervisor).await;
        assert!(matches!(
            supervisor.stop("w1").await,
            Err(SupervisorError::Inconsistent(_))
        ));
        assert!(dir.exists());
    }

    #[tokio::test]
    async fn the_loser_of_a_publish_race_reports_the_id_taken_and_drops_its_staging() {
        let (_root, supervisor) = supervisor();
        supervisor.stage("w1", b"loser").unwrap();
        // The winner published between the loser's staging and its rename.
        let winner = supervisor.workload_dir("w1").unwrap();
        fs::create_dir(&winner).unwrap();
        fs::write(winner.join(LAUNCH_FILE), b"winner").unwrap();

        assert!(matches!(
            supervisor.publish("w1"),
            Err(SupervisorError::AlreadyExists { .. })
        ));
        assert!(!supervisor.staging_dir("w1").exists());
        assert_eq!(fs::read(winner.join(LAUNCH_FILE)).unwrap(), b"winner");
    }

    #[test]
    fn a_relative_root_is_resolved_once() {
        let root = tempfile::tempdir().unwrap();
        let canonical = fs::canonicalize(root.path()).unwrap();
        let relative = relative_to_current_dir(&canonical);
        assert!(relative.is_relative());

        let supervisor = Supervisor::new(&relative, "/nonexistent/dotcore-launcher").unwrap();

        assert_eq!(supervisor.root, canonical);
    }

    #[test]
    fn ids_are_plain_directory_names() {
        assert!(valid_id("deployment-1_a.b"));
        for invalid in ["", ".create-x", "a/b", "..", "a b"] {
            assert!(!valid_id(invalid), "{invalid:?}");
        }
    }

    async fn assert_not_removable(supervisor: &Supervisor) {
        assert!(matches!(
            supervisor.remove("w1").await,
            Err(SupervisorError::NotRemovable { .. })
        ));
    }

    /// Spells an absolute path relative to the current directory, through the filesystem root.
    fn relative_to_current_dir(path: &Path) -> PathBuf {
        let depth = std::env::current_dir().unwrap().components().count() - 1;
        let mut relative = PathBuf::new();
        for _ in 0..depth {
            relative.push("..");
        }
        relative.join(path.strip_prefix("/").unwrap())
    }

    async fn assert_inconsistent_payload(supervisor: &Supervisor) {
        assert!(matches!(
            supervisor.inspect("w1").await.unwrap(),
            WorkloadState::Inconsistent { .. }
        ));
        assert!(matches!(
            supervisor.payload("w1").await,
            Err(SupervisorError::Inconsistent(_))
        ));
    }

    fn supervisor() -> (tempfile::TempDir, Supervisor) {
        let root = tempfile::tempdir().unwrap();
        let supervisor = Supervisor::new(root.path(), "/nonexistent/dotcore-launcher").unwrap();
        (root, supervisor)
    }

    /// Writes a published workload whose launcher record names the given process.
    fn fabricated_workload(supervisor: &Supervisor, pid: u32, start_token: u64) -> PathBuf {
        let payload = spec().encode().unwrap();
        supervisor.stage("w1", &payload).unwrap();
        let dir = supervisor.publish("w1").unwrap();
        write_record(&dir, pid, start_token, &supervisor.boot_id);
        dir
    }

    fn write_record(dir: &Path, pid: u32, start_token: u64, boot_id: &str) {
        let record = LauncherRecord {
            schema_version: SCHEMA_VERSION,
            pid,
            start_token,
            boot_id: boot_id.to_owned(),
        };
        replace_file(&dir.join(LAUNCHER_FILE), &record.encode());
    }

    /// Replaces a stored file, read-only like the original, with other contents.
    fn replace_file(path: &Path, contents: &[u8]) {
        let _ = fs::remove_file(path);
        write_read_only_file(path, contents).unwrap();
    }

    fn backdate(dir: &Path) {
        File::open(dir)
            .unwrap()
            .set_modified(SystemTime::now() - INTERRUPTED_CREATE_AGE * 2)
            .unwrap();
    }

    fn spec() -> LaunchSpec {
        LaunchSpec {
            executable: PathBuf::from("/bin/sh"),
            args: vec!["-c".to_owned(), "true".to_owned()],
            environment: BTreeMap::new(),
            grace_period_ms: 1_000,
            lifetime: Lifetime::Independent,
            owner: b"owner-1".to_vec(),
            fingerprint: "fingerprint-1".to_owned(),
        }
    }
}
