//! Defines the files of one workload directory, shared by the launcher that writes them and the
//! supervisor that reads them back, for example after the supervising process restarts.
//!
//! The supervisor writes the launch specification before spawning; the launcher only reads it.
//! The launcher writes its own identity record before starting the workload process, the
//! process's identity once it runs, an exit record when it finishes, and one bounded, timestamped
//! text log per output stream. Records are protobuf messages of the launcher store contract,
//! written read-only to guard against accidental writes only: the held process runs as the same
//! user and could still replace them. It runs in a separate working directory inside the
//! workload directory.

use std::{
    collections::BTreeMap,
    fs, io,
    io::Write,
    os::unix::fs::OpenOptionsExt,
    path::{Path, PathBuf},
    time::{SystemTime, UNIX_EPOCH},
};

use dotcore_proto::dotcore::launcher::store::v1 as store;
use prost::Message;

/// Version of the launcher identity record format.
pub const SCHEMA_VERSION: u32 = 1;

/// Launch specification written by the supervisor, never rewritten.
pub const LAUNCH_FILE: &str = "launch.pb";

/// Hex SHA-256 of the stored launch specification bytes, written by the supervisor.
pub const LAUNCH_CHECKSUM_FILE: &str = "launch.sha256";

/// Launcher identity record, written before the workload process starts.
pub const LAUNCHER_FILE: &str = "launcher.pb";

/// Identity of the held process, which also leads its own process group, written after it starts.
pub const PROCESS_FILE: &str = "process.pb";

/// Exit record, written once the workload process has ended.
pub const EXIT_FILE: &str = "exit.pb";

/// Working directory of the workload process, created with the workload directory, so the
/// process writes its own files apart from the launcher records.
pub const WORK_DIRECTORY: &str = "work";

/// Grace period between the stop signal and the forced kill when a caller has no preference.
/// Not a behavior duration: time a real workload process gets to stop, which its launcher
/// measures across backend restarts.
pub const DEFAULT_GRACE_PERIOD_MS: u64 = 30_000;

/// Size cap of one stream's current log file; one rotated file is kept beside it.
pub const LOG_CAP_BYTES: u64 = 16 * 1024 * 1024;

/// Record stored as one protobuf file of a workload directory.
pub trait StoredRecord: Sized {
    /// File name of the record in its workload directory.
    const FILE: &'static str;

    /// Decodes and validates the stored bytes.
    fn decode(bytes: &[u8]) -> Result<Self, RecordError>;
}

/// Stored record that cannot be decoded into a valid value.
#[derive(Debug, thiserror::Error)]
pub enum RecordError {
    #[error("decode protobuf record")]
    Decode(#[source] prost::DecodeError),

    #[error("record field {field} is missing or invalid")]
    Invalid { field: &'static str },
}

/// Exact launch request for one workload process.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LaunchSpec {
    /// Absolute path of the executable.
    pub executable: PathBuf,
    /// Arguments after the executable name.
    pub args: Vec<String>,
    /// Complete environment of the workload process; nothing else is inherited.
    pub environment: BTreeMap<String, String>,
    /// Time between the stop signal and the forced kill.
    pub grace_period_ms: u64,
    pub lifetime: Lifetime,
    /// Caller-defined record, opaque to the launcher and covered by the stored checksum.
    pub owner: Vec<u8>,
    /// Caller-defined fingerprint of the launch intent, opaque to the launcher.
    pub fingerprint: String,
}

impl LaunchSpec {
    /// Encodes the specification for storage; the executable path must be valid UTF-8.
    pub fn encode(&self) -> Result<Vec<u8>, RecordError> {
        let executable = self
            .executable
            .to_str()
            .ok_or(RecordError::Invalid {
                field: "executable",
            })?
            .to_owned();
        let watch = match self.lifetime {
            Lifetime::Independent => None,
            Lifetime::Watch { pid, start_token } => {
                Some(store::WatchedProcess { pid, start_token })
            }
        };
        Ok(store::LaunchSpec {
            executable,
            args: self.args.clone(),
            environment: self.environment.clone().into_iter().collect(),
            grace_period_ms: self.grace_period_ms,
            watch,
            owner: self.owner.clone(),
            fingerprint: self.fingerprint.clone(),
        }
        .encode_to_vec())
    }
}

impl StoredRecord for LaunchSpec {
    const FILE: &'static str = LAUNCH_FILE;

    fn decode(bytes: &[u8]) -> Result<Self, RecordError> {
        let stored = store::LaunchSpec::decode(bytes).map_err(RecordError::Decode)?;
        let lifetime = match stored.watch {
            None => Lifetime::Independent,
            Some(watch) => Lifetime::Watch {
                pid: watch.pid,
                start_token: watch.start_token,
            },
        };
        Ok(Self {
            executable: PathBuf::from(stored.executable),
            args: stored.args,
            environment: stored.environment.into_iter().collect(),
            grace_period_ms: stored.grace_period_ms,
            lifetime,
            owner: stored.owner,
            fingerprint: stored.fingerprint,
        })
    }
}

/// How long a workload may outlive the process that created it.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Lifetime {
    /// The workload runs until stopped or until its process exits.
    #[default]
    Independent,

    /// The workload stops gracefully once the identified process exits.
    Watch { pid: u32, start_token: u64 },
}

/// Identity of a running launcher, written before it starts the workload process.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LauncherRecord {
    pub schema_version: u32,
    pub pid: u32,
    pub start_token: u64,
    pub boot_id: String,
}

impl LauncherRecord {
    /// Encodes the record for storage.
    pub fn encode(&self) -> Vec<u8> {
        store::LauncherRecord {
            schema_version: self.schema_version,
            pid: self.pid,
            start_token: self.start_token,
            boot_id: self.boot_id.clone(),
        }
        .encode_to_vec()
    }
}

impl StoredRecord for LauncherRecord {
    const FILE: &'static str = LAUNCHER_FILE;

    fn decode(bytes: &[u8]) -> Result<Self, RecordError> {
        let stored = store::LauncherRecord::decode(bytes).map_err(RecordError::Decode)?;
        Ok(Self {
            schema_version: stored.schema_version,
            pid: process_id(stored.pid)?,
            start_token: stored.start_token,
            boot_id: stored.boot_id,
        })
    }
}

/// Identity of the held workload process; its PID is also its process group id.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ProcessRecord {
    pub pid: u32,
    pub start_token: u64,
}

impl ProcessRecord {
    /// Encodes the record for storage.
    pub fn encode(&self) -> Vec<u8> {
        store::ProcessRecord {
            pid: self.pid,
            start_token: self.start_token,
        }
        .encode_to_vec()
    }
}

impl StoredRecord for ProcessRecord {
    const FILE: &'static str = PROCESS_FILE;

    fn decode(bytes: &[u8]) -> Result<Self, RecordError> {
        let stored = store::ProcessRecord::decode(bytes).map_err(RecordError::Decode)?;
        Ok(Self {
            pid: process_id(stored.pid)?,
            start_token: stored.start_token,
        })
    }
}

/// Final outcome of a workload process.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ExitRecord {
    /// Exit status, absent when the process never started.
    pub status: Option<ExitStatus>,
    pub reason: ExitReason,
    /// Unix time in milliseconds when the launcher recorded the exit.
    pub time_ms: u64,
}

impl ExitRecord {
    /// Encodes the record for storage.
    pub fn encode(&self) -> Vec<u8> {
        let status = self.status.map(|status| store::ExitStatus {
            kind: Some(match status {
                ExitStatus::Code(code) => store::exit_status::Kind::Code(code),
                ExitStatus::Signal(signal) => store::exit_status::Kind::Signal(signal),
            }),
        });
        let reason = match self.reason {
            ExitReason::Exited => store::ExitReason::Exited,
            ExitReason::Stopped => store::ExitReason::Stopped,
            ExitReason::WatchedProcessExited => store::ExitReason::WatchedProcessExited,
            ExitReason::SpawnFailed => store::ExitReason::SpawnFailed,
            ExitReason::LauncherFailed => store::ExitReason::LauncherFailed,
        };
        store::ExitRecord {
            status,
            reason: reason.into(),
            time_ms: self.time_ms,
        }
        .encode_to_vec()
    }
}

impl StoredRecord for ExitRecord {
    const FILE: &'static str = EXIT_FILE;

    fn decode(bytes: &[u8]) -> Result<Self, RecordError> {
        let stored = store::ExitRecord::decode(bytes).map_err(RecordError::Decode)?;
        let status = match stored.status.map(|status| status.kind) {
            None => None,
            Some(Some(store::exit_status::Kind::Code(code))) => Some(ExitStatus::Code(code)),
            Some(Some(store::exit_status::Kind::Signal(signal))) => {
                Some(ExitStatus::Signal(signal))
            }
            Some(None) => return Err(RecordError::Invalid { field: "status" }),
        };
        let reason = match store::ExitReason::try_from(stored.reason) {
            Ok(store::ExitReason::Exited) => ExitReason::Exited,
            Ok(store::ExitReason::Stopped) => ExitReason::Stopped,
            Ok(store::ExitReason::WatchedProcessExited) => ExitReason::WatchedProcessExited,
            Ok(store::ExitReason::SpawnFailed) => ExitReason::SpawnFailed,
            Ok(store::ExitReason::LauncherFailed) => ExitReason::LauncherFailed,
            Ok(store::ExitReason::Unspecified) | Err(_) => {
                return Err(RecordError::Invalid { field: "reason" });
            }
        };
        Ok(Self {
            status,
            reason,
            time_ms: stored.time_ms,
        })
    }
}

/// How a workload process ended.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ExitStatus {
    Code(i32),

    Signal(i32),
}

/// Why a workload process ended.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ExitReason {
    /// The process exited on its own.
    Exited,

    /// The launcher received a stop signal.
    Stopped,

    /// The watched process exited, was already gone, or had a different start token at launch.
    WatchedProcessExited,

    /// The executable could not be started.
    SpawnFailed,

    /// The launcher hit an internal failure and stopped the workload process.
    LauncherFailed,
}

/// Output stream of a workload process.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum LogStream {
    Stdout,

    Stderr,
}

impl LogStream {
    /// Current log file name of the stream.
    pub fn file_name(self) -> &'static str {
        match self {
            Self::Stdout => "stdout.log",
            Self::Stderr => "stderr.log",
        }
    }

    /// Name of the stream's single rotated log file.
    pub fn rotated_file_name(self) -> &'static str {
        match self {
            Self::Stdout => "stdout.log.1",
            Self::Stderr => "stderr.log.1",
        }
    }
}

/// One output line of a workload process.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LogRecord {
    pub stream: LogStream,
    /// Unix time in nanoseconds when the launcher read the line.
    pub time_ns: u64,
    /// Line text without its newline; invalid UTF-8 is replaced.
    pub line: String,
}

impl LogRecord {
    /// Parses one stored record line, `<unix nanoseconds> <text>`, without its newline.
    pub fn decode(stream: LogStream, record: &[u8]) -> Option<Self> {
        let separator = record.iter().position(|byte| *byte == b' ')?;
        let time_ns = std::str::from_utf8(&record[..separator])
            .ok()?
            .parse()
            .ok()?;
        Some(Self {
            stream,
            time_ns,
            line: String::from_utf8_lossy(&record[separator + 1..]).into_owned(),
        })
    }
}

/// Accepts a stored PID that names one process: zero and values beyond `pid_t` would make a
/// signal reach a whole process group instead.
fn process_id(pid: u32) -> Result<u32, RecordError> {
    if pid == 0 || libc::pid_t::try_from(pid).is_err() {
        return Err(RecordError::Invalid { field: "pid" });
    }
    Ok(pid)
}

/// Encodes one stored record line, newline included, for text that contains no newline.
pub fn encode_log_record(time_ns: u64, line: &[u8]) -> Vec<u8> {
    let mut record = format!("{time_ns} ").into_bytes();
    record.extend_from_slice(line);
    record.push(b'\n');
    record
}

/// Replaces a file in one step with a read-only one, so readers see either the old or the
/// complete new content.
pub fn write_file_atomically(path: &Path, contents: &[u8]) -> io::Result<()> {
    let mut staging = path.as_os_str().to_owned();
    staging.push(".tmp");
    // A staging file left by an interrupted write is read-only and cannot be opened again.
    match fs::remove_file(&staging) {
        Ok(()) => {}
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => return Err(error),
    }
    write_read_only_file(Path::new(&staging), contents)?;
    fs::rename(&staging, path)
}

/// Creates a new file readable only by its owner and writes its contents.
pub fn write_read_only_file(path: &Path, contents: &[u8]) -> io::Result<()> {
    fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o400)
        .open(path)?
        .write_all(contents)
}

/// Current Unix time in nanoseconds.
pub fn unix_time_ns() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_nanos() as u64)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn records_round_trip_through_their_stored_encoding() {
        for lifetime in [
            Lifetime::Independent,
            Lifetime::Watch {
                pid: 42,
                start_token: 7,
            },
        ] {
            let spec = LaunchSpec {
                executable: PathBuf::from("/bin/sh"),
                args: vec!["-c".to_owned(), "true".to_owned()],
                environment: BTreeMap::from([
                    ("A".to_owned(), "1".to_owned()),
                    ("B".to_owned(), "two words".to_owned()),
                ]),
                grace_period_ms: 1_500,
                lifetime,
                owner: vec![0, 1, 255],
                fingerprint: "fingerprint-1".to_owned(),
            };
            assert_eq!(LaunchSpec::decode(&spec.encode().unwrap()).unwrap(), spec);
        }

        let launcher = LauncherRecord {
            schema_version: SCHEMA_VERSION,
            pid: 10,
            start_token: 11,
            boot_id: "boot-1".to_owned(),
        };
        assert_eq!(
            LauncherRecord::decode(&launcher.encode()).unwrap(),
            launcher
        );
        let process = ProcessRecord {
            pid: 12,
            start_token: 13,
        };
        assert_eq!(ProcessRecord::decode(&process.encode()).unwrap(), process);
        for (status, reason) in [
            (Some(ExitStatus::Code(3)), ExitReason::Exited),
            (Some(ExitStatus::Signal(9)), ExitReason::Stopped),
            (None, ExitReason::WatchedProcessExited),
            (None, ExitReason::SpawnFailed),
            (Some(ExitStatus::Code(0)), ExitReason::LauncherFailed),
        ] {
            let exit = ExitRecord {
                status,
                reason,
                time_ms: 14,
            };
            assert_eq!(ExitRecord::decode(&exit.encode()).unwrap(), exit);
        }
    }

    #[test]
    fn exit_records_without_a_known_reason_or_status_kind_are_invalid() {
        for stored in [
            store::ExitRecord {
                status: None,
                reason: store::ExitReason::Unspecified.into(),
                time_ms: 1,
            },
            store::ExitRecord {
                status: None,
                reason: 99,
                time_ms: 1,
            },
        ] {
            assert!(matches!(
                ExitRecord::decode(&stored.encode_to_vec()),
                Err(RecordError::Invalid { field: "reason" })
            ));
        }
        let without_kind = store::ExitRecord {
            status: Some(store::ExitStatus { kind: None }),
            reason: store::ExitReason::Exited.into(),
            time_ms: 1,
        };
        assert!(matches!(
            ExitRecord::decode(&without_kind.encode_to_vec()),
            Err(RecordError::Invalid { field: "status" })
        ));
        assert!(matches!(
            ExitRecord::decode(b"\xff"),
            Err(RecordError::Decode(_))
        ));
    }

    #[test]
    fn records_with_a_pid_that_names_no_single_process_are_invalid() {
        for pid in [0, u32::MAX] {
            let launcher = LauncherRecord {
                schema_version: SCHEMA_VERSION,
                pid,
                start_token: 1,
                boot_id: "boot-1".to_owned(),
            };
            let process = ProcessRecord {
                pid,
                start_token: 1,
            };

            assert!(matches!(
                LauncherRecord::decode(&launcher.encode()),
                Err(RecordError::Invalid { field: "pid" })
            ));
            assert!(matches!(
                ProcessRecord::decode(&process.encode()),
                Err(RecordError::Invalid { field: "pid" })
            ));
        }
    }

    #[test]
    fn non_utf8_executables_cannot_be_stored() {
        use std::os::unix::ffi::OsStrExt;

        let spec = LaunchSpec {
            executable: PathBuf::from(std::ffi::OsStr::from_bytes(b"/bin/\xff")),
            args: Vec::new(),
            environment: BTreeMap::new(),
            grace_period_ms: 0,
            lifetime: Lifetime::Independent,
            owner: Vec::new(),
            fingerprint: String::new(),
        };

        assert!(matches!(
            spec.encode(),
            Err(RecordError::Invalid {
                field: "executable"
            })
        ));
    }

    #[test]
    fn records_are_written_read_only() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(LAUNCHER_FILE);

        write_file_atomically(&path, b"first").unwrap();
        write_file_atomically(&path, b"second").unwrap();

        assert_eq!(fs::read(&path).unwrap(), b"second");
        assert_eq!(
            fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o400
        );
    }

    #[test]
    fn log_records_round_trip_spaces_and_invalid_utf8() {
        let encoded = encode_log_record(1_700_000_000_123_456_789, b"a b \xff");

        assert_eq!(encoded.last(), Some(&b'\n'));
        assert_eq!(
            LogRecord::decode(LogStream::Stderr, &encoded[..encoded.len() - 1]),
            Some(LogRecord {
                stream: LogStream::Stderr,
                time_ns: 1_700_000_000_123_456_789,
                line: "a b \u{fffd}".to_owned(),
            })
        );
        assert_eq!(LogRecord::decode(LogStream::Stdout, b"no-time"), None);
    }
}
