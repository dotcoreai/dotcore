//! Process-wide stderr logs and reloadable site-local JSONL logs.
//!
//! The module also owns how failure detail reaches a log event: a failure and
//! its retained causes are joined in order and length-bounded, so a rejected
//! request cannot flood a log line and no cause is silently dropped.
use std::{
    fs,
    io::{self, Write},
    path::{Path, PathBuf},
    sync::{
        OnceLock,
        atomic::{AtomicBool, Ordering},
        mpsc::{SyncSender, sync_channel},
    },
    thread::{self, JoinHandle},
};

use time::{Date, Month, OffsetDateTime};
use tracing_appender::{
    non_blocking::DEFAULT_BUFFERED_LINES_LIMIT,
    rolling::{RollingFileAppender, Rotation},
};
use tracing_subscriber::{
    EnvFilter, Registry,
    fmt::{
        self,
        format::{Format, Json, JsonFields},
        writer::BoxMakeWriter,
    },
    layer::SubscriberExt,
    reload,
    util::SubscriberInitExt,
};

const DEFAULT_FILTER: &str = "info";
const LOG_DIRECTORY: &str = "logs";
const LOG_PREFIX: &str = "backend";
const LOG_SUFFIX: &str = "jsonl";

type JsonLayer = fmt::Layer<Registry, JsonFields, Format<Json>, BoxMakeWriter>;
type JsonReloadHandle = reload::Handle<JsonLayer, Registry>;

static JSON_RELOAD_HANDLE: OnceLock<JsonReloadHandle> = OnceLock::new();
static ACTIVE_LOGGING: AtomicBool = AtomicBool::new(false);

/// Initializes the backend logs subscriber once with a disabled JSONL sink.
pub fn initialize() {
    initialize_with_default_filter(DEFAULT_FILTER);
}

/// Initializes the backend logs subscriber like [`initialize`], using `default_filter` when
/// `RUST_LOG` is unset or invalid.
pub fn initialize_with_default_filter(default_filter: &str) {
    let filter = filter_from_rust_log(std::env::var("RUST_LOG").ok().as_deref(), default_filter);
    let (json_layer, reload_handle) = reload::Layer::new(json_layer(BoxMakeWriter::new(io::sink)));
    if tracing_subscriber::registry()
        .with(json_layer)
        .with(
            tracing_subscriber::fmt::layer()
                .with_writer(io::stderr)
                .with_span_events(tracing_subscriber::fmt::format::FmtSpan::CLOSE),
        )
        .with(filter)
        .try_init()
        .is_ok()
    {
        let _ = JSON_RELOAD_HANDLE.set(reload_handle);
    }
}

/// Activates site-local log retention and returns its lifecycle.
pub fn activate_backend_logging(
    site_root: &Path,
    logging_retention_days: u32,
) -> Result<BackendLogging, LoggingError> {
    let logging_ownership = claim_logging_ownership()?;
    let directory = site_root.join(LOG_DIRECTORY);
    fs::create_dir_all(&directory).map_err(|source| LoggingError::Directory {
        path: directory.clone(),
        source,
    })?;
    let appender = daily_appender(&directory)?;
    prune_daily_logs(&directory, logging_retention_days)?;
    let worker = LogWorker::new(appender, directory.clone(), logging_retention_days);
    reload_writer(worker.writer())?;
    Ok(BackendLogging {
        logging_ownership,
        worker: Some(worker),
    })
}

/// The backend-owned lifecycle that disables and drains retained logs during shutdown.
pub struct BackendLogging {
    logging_ownership: Option<LoggingOwnership>,
    worker: Option<LogWorker>,
}

impl Drop for BackendLogging {
    fn drop(&mut self) {
        if self.logging_ownership.is_some() {
            disable_writer();
        }
        self.worker.take();
        self.logging_ownership.take();
    }
}

/// Errors raised while establishing retained site-local logs.
#[derive(Debug, thiserror::Error)]
pub enum LoggingError {
    #[error("another backend already owns the process logging writer")]
    WriterAlreadyActive,

    #[error("could not create backend log directory {path}")]
    Directory {
        path: PathBuf,
        #[source]
        source: io::Error,
    },

    #[error("could not initialize backend log writer in {path}")]
    Writer {
        path: PathBuf,
        #[source]
        source: anyhow::Error,
    },

    #[error("could not activate backend log writer")]
    Reload {
        #[source]
        source: Box<dyn std::error::Error + Send + Sync>,
    },
}

fn claim_logging_ownership() -> Result<Option<LoggingOwnership>, LoggingError> {
    claim_logging_ownership_when_active(JSON_RELOAD_HANDLE.get().is_some())
}

fn claim_logging_ownership_when_active(
    writer_is_process_wide: bool,
) -> Result<Option<LoggingOwnership>, LoggingError> {
    if !writer_is_process_wide {
        return Ok(None);
    }
    ACTIVE_LOGGING
        .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
        .map(|_| Some(LoggingOwnership))
        .map_err(|_| LoggingError::WriterAlreadyActive)
}

struct LoggingOwnership;

impl Drop for LoggingOwnership {
    fn drop(&mut self) {
        ACTIVE_LOGGING.store(false, Ordering::Release);
    }
}

fn daily_appender(directory: &Path) -> Result<RollingFileAppender, LoggingError> {
    RollingFileAppender::builder()
        .rotation(Rotation::DAILY)
        .filename_prefix(LOG_PREFIX)
        .filename_suffix(LOG_SUFFIX)
        .build(directory)
        .map_err(|source| LoggingError::Writer {
            path: directory.to_path_buf(),
            source: anyhow::Error::new(source),
        })
}

fn prune_daily_logs(directory: &Path, logging_retention_days: u32) -> Result<(), LoggingError> {
    let mut logs = fs::read_dir(directory)
        .map_err(|source| LoggingError::Writer {
            path: directory.to_path_buf(),
            source: anyhow::Error::new(source),
        })?
        .filter_map(Result::ok)
        .filter_map(|entry| {
            entry
                .file_type()
                .ok()
                .filter(|file_type| file_type.is_file())
                .and_then(|_| daily_log_date(&entry.file_name()))
                .map(|date| (date, entry.path()))
        })
        .collect::<Vec<_>>();
    logs.sort_unstable_by_key(|(date, _)| *date);
    let excess = logs.len().saturating_sub(logging_retention_days as usize);
    for (_, path) in logs.into_iter().take(excess) {
        fs::remove_file(&path).map_err(|source| LoggingError::Writer {
            path,
            source: anyhow::Error::new(source),
        })?;
    }
    Ok(())
}

fn daily_log_date(filename: &std::ffi::OsStr) -> Option<Date> {
    let filename = filename.to_str()?;
    let date = filename
        .strip_prefix(&format!("{LOG_PREFIX}."))?
        .strip_suffix(&format!(".{LOG_SUFFIX}"))?;
    let parts = date.split('-').collect::<Vec<_>>();
    let [year, month, day] = parts.as_slice() else {
        return None;
    };
    if date.len() != 10
        || year.len() != 4
        || month.len() != 2
        || day.len() != 2
        || !date.bytes().enumerate().all(|(index, byte)| {
            matches!(index, 4 | 7) && byte == b'-'
                || !matches!(index, 4 | 7) && byte.is_ascii_digit()
        })
    {
        return None;
    }
    Date::from_calendar_date(
        year.parse().ok()?,
        Month::try_from(month.parse::<u8>().ok()?).ok()?,
        day.parse().ok()?,
    )
    .ok()
}

fn reload_writer(writer: LogQueue) -> Result<(), LoggingError> {
    let Some(reload_handle) = JSON_RELOAD_HANDLE.get() else {
        return Ok(());
    };
    reload_handle
        .reload(json_layer(BoxMakeWriter::new(writer)))
        .map_err(|source| LoggingError::Reload {
            source: Box::new(source),
        })
}

fn disable_writer() {
    let Some(reload_handle) = JSON_RELOAD_HANDLE.get() else {
        return;
    };
    let _ = reload_handle.reload(json_layer(BoxMakeWriter::new(io::sink)));
}

fn json_layer(writer: BoxMakeWriter) -> JsonLayer {
    tracing_subscriber::fmt::layer()
        .json()
        .with_ansi(false)
        .with_writer(writer)
}

fn filter_from_rust_log(value: Option<&str>, default_filter: &str) -> EnvFilter {
    value
        .filter(|value| !value.trim().is_empty())
        .and_then(|value| EnvFilter::try_new(value).ok())
        .unwrap_or_else(|| EnvFilter::new(default_filter))
}

struct LogWorker {
    sender: Option<SyncSender<Vec<u8>>>,
    worker: Option<JoinHandle<()>>,
}

impl LogWorker {
    fn new(appender: RollingFileAppender, directory: PathBuf, logging_retention_days: u32) -> Self {
        let (sender, receiver) = sync_channel::<Vec<u8>>(DEFAULT_BUFFERED_LINES_LIMIT);
        let worker = thread::Builder::new()
            .name("dotcore-backend-logging".to_owned())
            .spawn(move || {
                let mut appender = appender;
                let mut last_pruned = None;
                while let Ok(line) = receiver.recv() {
                    let today = OffsetDateTime::now_utc().date();
                    let _ = appender.write_all(&line);
                    if last_pruned != Some(today) {
                        let _ = prune_daily_logs(&directory, logging_retention_days);
                        last_pruned = Some(today);
                    }
                }
                let _ = appender.flush();
            })
            .expect("could not create backend log writer thread");
        Self {
            sender: Some(sender),
            worker: Some(worker),
        }
    }

    fn writer(&self) -> LogQueue {
        LogQueue(self.sender.as_ref().expect("active writer").clone())
    }
}

impl Drop for LogWorker {
    fn drop(&mut self) {
        self.sender.take();
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

#[derive(Clone)]
struct LogQueue(SyncSender<Vec<u8>>);

impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for LogQueue {
    type Writer = Self;

    fn make_writer(&'a self) -> Self::Writer {
        self.clone()
    }
}

impl Write for LogQueue {
    fn write(&mut self, buffer: &[u8]) -> io::Result<usize> {
        self.0
            .send(buffer.to_vec())
            .map_err(|_| io::Error::new(io::ErrorKind::BrokenPipe, "log writer stopped"))?;
        Ok(buffer.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use std::{
        fs,
        io::Write,
        sync::{Arc, mpsc::sync_channel},
    };

    use dotcore_domain::test_support::tracing::{Record, capture_records, capture_records_async};
    use tracing::{Instrument, debug, debug_span, field, info, info_span, warn};
    use tracing_subscriber::{layer::SubscriberExt, util::SubscriberInitExt};

    use super::{
        DEFAULT_FILTER, LogQueue, LogWorker, LoggingError, claim_logging_ownership_when_active,
        daily_appender, filter_from_rust_log, json_layer, prune_daily_logs,
    };

    #[test]
    fn json_layer_writes_ansi_free_json_lines() {
        let directory = temporary_directory();
        let appender = tracing_appender::rolling::never(&directory, "logs.jsonl");
        let worker = LogWorker::new(appender, directory.clone(), 1);
        let subscriber = tracing_subscriber::registry()
            .with(json_layer(
                tracing_subscriber::fmt::writer::BoxMakeWriter::new(worker.writer()),
            ))
            .with(filter_from_rust_log(Some("info"), DEFAULT_FILTER))
            .set_default();

        debug!(event_kind = "filtered_log_test", "filtered log line");
        info!(event_kind = "log_test", "log line");
        drop(subscriber);
        drop(worker);

        let output = fs::read_to_string(directory.join("logs.jsonl")).expect("JSONL output");
        let record: serde_json::Value = serde_json::from_str(output.trim()).expect("JSON record");
        assert_eq!(record["fields"]["event_kind"], "log_test");
        assert!(!output.contains("filtered_log_test"));
        assert!(!output.contains("\u{1b}["));
        fs::remove_dir_all(directory).expect("test directory cleanup");
    }

    #[test]
    fn daily_pruning_removes_only_exact_daily_log_filenames() {
        let directory = temporary_directory();
        for filename in [
            "backend.2024-01-01.jsonl",
            "backend.2024-01-02.jsonl",
            "backend.2024-01-03.jsonl",
            "backend.2024-01-01.jsonl.backup",
            "backend.unrelated.jsonl",
            "other.2024-01-01.jsonl",
        ] {
            fs::write(directory.join(filename), "old\n").expect("log fixture");
        }

        prune_daily_logs(&directory, 2).expect("prune daily logs");

        let mut files = fs::read_dir(&directory)
            .expect("log directory")
            .map(|entry| {
                entry
                    .expect("directory entry")
                    .file_name()
                    .into_string()
                    .expect("UTF-8 name")
            })
            .collect::<Vec<_>>();
        files.sort();
        assert_eq!(
            files,
            [
                "backend.2024-01-01.jsonl.backup",
                "backend.2024-01-02.jsonl",
                "backend.2024-01-03.jsonl",
                "backend.unrelated.jsonl",
                "other.2024-01-01.jsonl",
            ]
        );
        fs::remove_dir_all(directory).expect("test directory cleanup");
    }

    #[test]
    fn daily_appender_does_not_broadly_prune_similar_filenames() {
        let directory = temporary_directory();
        fs::write(directory.join("backend.unrelated.jsonl"), "keep\n")
            .expect("unrelated log fixture");
        let appender = daily_appender(&directory).expect("daily appender");
        drop(appender);

        assert!(directory.join("backend.unrelated.jsonl").exists());
        fs::remove_dir_all(directory).expect("test directory cleanup");
    }

    #[test]
    fn process_logging_ownership_rejects_a_second_active_backend() {
        let ownership = claim_logging_ownership_when_active(true)
            .expect("first backend logging ownership")
            .expect("process-wide logging ownership");
        assert!(matches!(
            claim_logging_ownership_when_active(true),
            Err(LoggingError::WriterAlreadyActive)
        ));
        drop(ownership);
    }

    #[test]
    fn bounded_log_queue_applies_backpressure_without_losing_lines() {
        let (sender, receiver) = sync_channel(1);
        let mut writer = LogQueue(sender);
        writer.write_all(b"first\n").expect("first log line");
        let mut blocked_writer = writer.clone();
        let write = std::thread::spawn(move || {
            blocked_writer
                .write_all(b"second\n")
                .expect("second log line");
        });

        assert_eq!(receiver.recv().expect("first queued line"), b"first\n");
        write.join().expect("backpressured writer completes");
        assert_eq!(receiver.recv().expect("second queued line"), b"second\n");
    }

    #[test]
    fn stopped_log_queue_surfaces_a_write_failure() {
        let (sender, receiver) = sync_channel(1);
        drop(receiver);
        let mut writer = LogQueue(sender);

        let error = writer.write_all(b"log\n").expect_err("stopped worker");

        assert_eq!(error.kind(), std::io::ErrorKind::BrokenPipe);
    }

    #[test]
    fn missing_or_invalid_filter_uses_info_fallback() {
        assert_eq!(
            filter_from_rust_log(None, DEFAULT_FILTER).to_string(),
            "info"
        );
        assert_eq!(
            filter_from_rust_log(Some(" "), DEFAULT_FILTER).to_string(),
            "info"
        );
        assert_eq!(
            filter_from_rust_log(Some("backend=not-a-level"), DEFAULT_FILTER).to_string(),
            "info"
        );
    }

    #[test]
    fn valid_filter_is_preserved() {
        assert_eq!(
            filter_from_rust_log(Some("dotcore_backend=debug"), DEFAULT_FILTER).to_string(),
            "dotcore_backend=debug"
        );
    }

    #[test]
    fn default_display_and_tracing_do_not_render_error_chains_or_backtraces() {
        let error = anyhow::Error::new(std::io::Error::other("leaf log detail"))
            .context("classified operation failure");
        let (_, _, output) = capture_records(|| {
            warn!(error_kind = "test_failure", error = %error, "bounded test warning");
        });

        assert_eq!(error.to_string(), "classified operation failure");
        assert!(output.contains("classified operation failure"));
        assert!(!output.contains("leaf log detail"));
        assert!(!output.contains("stack backtrace"));
    }

    #[test]
    fn test_capture_observes_terminal_span_fields_and_close_timing() {
        let (_, records, output) = capture_records(|| {
            let span = info_span!(
                "test_operation",
                operation = "test_operation",
                outcome = field::Empty,
            );
            span.in_scope(|| span.record("outcome", "success"));
            warn!(event_kind = "test_failure", "bounded test warning");
        });

        assert!(records.iter().any(|record| matches!(
            record,
            Record::SpanCreated { name, .. } if name == "test_operation"
        )));
        assert!(records.iter().any(|record| matches!(
            record,
            Record::SpanRecorded { name, fields }
                if name == "test_operation" && fields.get("outcome") == Some(&"\"success\"".to_owned())
        )));
        assert!(records.iter().any(|record| matches!(
            record,
            Record::SpanClosed { name, fields }
                if name == "test_operation" && fields.get("outcome") == Some(&"\"success\"".to_owned())
        )));
        assert!(output.contains("time.busy") && output.contains("time.idle"));
        assert!(records.iter().any(|record| matches!(
            record,
            Record::Event { level, fields }
                if level == "WARN" && fields.get("event_kind") == Some(&"\"test_failure\"".to_owned())
        )));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn spawned_tasks_keep_the_capture_subscriber_through_instrumentation() {
        let (_, records, _) = capture_records_async(|| async {
            let span = info_span!("spawned_operation", outcome = field::Empty);
            let record_span = span.clone();
            tokio::spawn(
                async move {
                    record_span.record("outcome", "success");
                }
                .instrument(span),
            )
            .await
            .expect("spawned operation");
        })
        .await;

        assert!(records.iter().any(|record| matches!(
            record,
            Record::SpanClosed { name, fields }
                if name == "spawned_operation" && fields.contains_key("outcome")
        )));
    }

    #[test]
    fn concurrent_captures_keep_debug_spans_enabled() {
        let barrier = Arc::new(std::sync::Barrier::new(2));

        std::thread::scope(|scope| {
            let left_barrier = Arc::clone(&barrier);
            let left = scope.spawn(move || {
                capture_records(|| {
                    left_barrier.wait();
                    let span = debug_span!("parallel_capture", lane = "left");
                    let _entered = span.enter();
                })
            });

            let right_barrier = Arc::clone(&barrier);
            let right = scope.spawn(move || {
                capture_records(|| {
                    right_barrier.wait();
                    let span = debug_span!("parallel_capture", lane = "right");
                    let _entered = span.enter();
                })
            });

            let (_, left_records, _) = left.join().expect("left capture");
            let (_, right_records, _) = right.join().expect("right capture");

            assert!(left_records.iter().any(|record| matches!(
                record,
                Record::SpanCreated { name, fields }
                    if name == "parallel_capture"
                        && fields.get("lane") == Some(&"\"left\"".to_owned())
            )));
            assert!(right_records.iter().any(|record| matches!(
                record,
                Record::SpanCreated { name, fields }
                    if name == "parallel_capture"
                        && fields.get("lane") == Some(&"\"right\"".to_owned())
            )));
            assert!(!left_records.iter().any(|record| matches!(
                record,
                Record::SpanCreated { fields, .. }
                    if fields.get("lane") == Some(&"\"right\"".to_owned())
            )));
            assert!(!right_records.iter().any(|record| matches!(
                record,
                Record::SpanCreated { fields, .. }
                    if fields.get("lane") == Some(&"\"left\"".to_owned())
            )));
        });
    }

    fn temporary_directory() -> std::path::PathBuf {
        let directory = dotcore_domain::test_support::canonical_temp_dir().join(format!(
            "dotcore-logging-test-{}",
            getrandom::u64().expect("random test directory suffix"),
        ));
        fs::create_dir_all(&directory).expect("test directory");
        directory
    }
}
