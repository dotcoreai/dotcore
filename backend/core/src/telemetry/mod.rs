//! Observability's local telemetry dataset: workload resource records that
//! the inference controller and development workload VRAM measurement emit,
//! and that the sizing fit reads back to fit catalog sizing constants.
//!
//! Records are appended as one JSON line each to a per-day file under the
//! telemetry directory, for example `<dir>/workload-resources/2026-09-25.jsonl`.
//! Recording is best-effort: a failed write is logged and never fails the
//! launch, measurement, or reconciliation that produced it. Writers only
//! emit; retention and reading stay inside this module.

use std::{
    fs::{self, OpenOptions},
    io::{self, Write},
    path::{Path, PathBuf},
    time::{SystemTime, UNIX_EPOCH},
};

use dotcore_domain::real_time;
use dotcore_proto::{
    dotcore::telemetry::v1::{WorkloadResourceRecord, WorkloadResourceSource},
    google::protobuf::Timestamp,
};
use time::OffsetDateTime;
use tracing::{debug, warn};

pub mod fit;
mod reader;

pub use reader::{MalformedTelemetryLine, TelemetryReadError, WorkloadResourceRecords};

/// Environment variable selecting the telemetry directory; composition roots read it, this module never does.
pub const TELEMETRY_DIR: &str = "DOTCORE_TELEMETRY_DIR";

const SITE_TELEMETRY_CHILD: &str = "telemetry";
const WORKLOAD_RESOURCES_CHILD: &str = "workload-resources";
const RECORD_FILE_EXTENSION: &str = "jsonl";
const PRODUCER_VERSION: &str = env!("CARGO_PKG_VERSION");

/// Receives telemetry records; implementations never fail or block their caller on storage errors.
pub trait TelemetrySink: Send + Sync {
    /// Records one workload resource observation, best-effort.
    fn record_workload_resource(&self, record: &WorkloadResourceRecord);
}

/// Appends records to per-day JSON-lines files under one local telemetry directory.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct FileTelemetrySink {
    directory: PathBuf,
}

impl FileTelemetrySink {
    pub fn new(directory: PathBuf) -> Self {
        Self { directory }
    }

    /// Returns the file a record with this timestamp is appended to.
    pub fn workload_resource_path(&self, recorded_at: Option<&Timestamp>) -> PathBuf {
        let date = recorded_at
            .and_then(|timestamp| {
                OffsetDateTime::from_unix_timestamp(timestamp.seconds)
                    .ok()
                    .map(OffsetDateTime::date)
            })
            .unwrap_or_else(|| OffsetDateTime::now_utc().date());
        self.directory
            .join(WORKLOAD_RESOURCES_CHILD)
            .join(format!("{date}.{RECORD_FILE_EXTENSION}"))
    }

    /// Appends one record and returns its file, reporting failures to the caller.
    pub fn append_workload_resource(
        &self,
        record: &WorkloadResourceRecord,
    ) -> Result<PathBuf, TelemetryWriteError> {
        let path = self.workload_resource_path(record.recorded_at.as_ref());
        let mut line = serde_json::to_vec(record).map_err(TelemetryWriteError::Encode)?;
        line.push(b'\n');
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).map_err(|source| TelemetryWriteError::Io {
                path: parent.to_path_buf(),
                source,
            })?;
        }
        // One append-mode write per complete line keeps concurrent writers from interleaving.
        let written = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
            .and_then(|mut file| file.write(&line));
        match written {
            Ok(count) if count == line.len() => Ok(path),
            Ok(count) => Err(TelemetryWriteError::Io {
                path,
                source: io::Error::new(
                    io::ErrorKind::WriteZero,
                    format!("short append of {count} of {} bytes", line.len()),
                ),
            }),
            Err(source) => Err(TelemetryWriteError::Io { path, source }),
        }
    }
}

impl TelemetrySink for FileTelemetrySink {
    fn record_workload_resource(&self, record: &WorkloadResourceRecord) {
        match self.append_workload_resource(record) {
            Ok(path) => debug!(
                event_kind = "telemetry_recorded",
                record_id = %record.record_id,
                path = %path.display(),
                "workload resource telemetry recorded"
            ),
            Err(error) => warn!(
                event_kind = "telemetry_record_failed",
                record_id = %record.record_id,
                error = %error,
                "workload resource telemetry was not recorded"
            ),
        }
    }
}

/// Failure to append one telemetry record.
#[derive(Debug, thiserror::Error)]
pub enum TelemetryWriteError {
    #[error("telemetry record could not be encoded")]
    Encode(#[source] serde_json::Error),

    #[error("telemetry file {path} could not be written")]
    Io {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
}

/// Resolves the telemetry directory from an explicit value, else the site directory's `telemetry` child.
pub fn resolve_directory(explicit: Option<&Path>, site_root: Option<&Path>) -> Option<PathBuf> {
    match explicit.filter(|value| !value.as_os_str().is_empty()) {
        Some(value) => Some(value.to_path_buf()),
        None => site_root.map(|root| root.join(SITE_TELEMETRY_CHILD)),
    }
}

/// Creates a record with a fresh unique id, the observation time, and this build's version.
pub fn new_workload_resource_record(
    source: WorkloadResourceSource,
    recorded_at: SystemTime,
) -> WorkloadResourceRecord {
    WorkloadResourceRecord {
        record_id: new_record_id(),
        recorded_at: Some(timestamp(recorded_at)),
        producer_version: PRODUCER_VERSION.to_owned(),
        source: source.into(),
        ..WorkloadResourceRecord::default()
    }
}

fn new_record_id() -> String {
    let mut bytes = [0_u8; 16];
    // A clock-derived id still dedupes correctly when entropy is unavailable.
    if getrandom::fill(&mut bytes).is_err() {
        let nanos = real_time::wall_now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        bytes = nanos.to_le_bytes();
    }
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

fn timestamp(time: SystemTime) -> Timestamp {
    let duration = time.duration_since(UNIX_EPOCH).unwrap_or_default();
    Timestamp {
        seconds: duration.as_secs().try_into().unwrap_or(i64::MAX),
        nanos: duration.subsec_nanos() as i32,
    }
}

#[cfg(test)]
mod tests {
    use std::{
        sync::Arc,
        time::{Duration, UNIX_EPOCH},
    };

    use dotcore_proto::{
        dotcore::{
            catalog::v1::{
                EngineParameterValue, EngineParameterValues, ModelOptionParameterValue,
                ModelOptionValues, engine_parameter_value::Value as EngineValue,
                model_option_parameter_value::Value as OptionValue,
            },
            observation::v1::AcceleratorMemoryArchitecture,
            telemetry::v1::{
                VllmStartupFacts, WorkloadRealizationKey, WorkloadResourceDevice,
                WorkloadResourceInputs, WorkloadResourceNode, WorkloadResourceObservation,
                WorkloadResourceObservationKind, WorkloadResourceRecord, WorkloadResourceSource,
                workload_resource_observation::EngineFacts,
            },
        },
        google::protobuf::Duration as ProtoDuration,
    };

    use super::*;

    #[test]
    fn populated_record_round_trips_through_json() {
        let record = populated_record("round-trip");

        let json = serde_json::to_string(&record).expect("record encodes");
        let decoded: WorkloadResourceRecord = serde_json::from_str(&json).expect("record decodes");

        assert_eq!(decoded, record);
        assert!(!json.contains('\n'));
    }

    #[test]
    fn explicit_directory_wins_over_site_fallback() {
        let site = Path::new("/srv/site");

        assert_eq!(
            resolve_directory(Some(Path::new("/data/telemetry")), Some(site)),
            Some(PathBuf::from("/data/telemetry"))
        );
        assert_eq!(
            resolve_directory(None, Some(site)),
            Some(PathBuf::from("/srv/site/telemetry"))
        );
        assert_eq!(
            resolve_directory(Some(Path::new("")), Some(site)),
            Some(PathBuf::from("/srv/site/telemetry"))
        );
        assert_eq!(resolve_directory(None, None), None);
    }

    #[test]
    fn records_land_in_a_utc_day_file() {
        let directory = crate::test_support::canonical_tempdir().expect("temporary directory");
        let sink = FileTelemetrySink::new(directory.path().to_path_buf());
        let record = populated_record("day-file");

        let path = sink
            .append_workload_resource(&record)
            .expect("record appends");

        assert_eq!(
            path,
            directory
                .path()
                .join("workload-resources")
                .join("2026-09-25.jsonl")
        );
        let records =
            WorkloadResourceRecords::read(&[directory.path().to_path_buf()]).expect("records read");
        assert_eq!(records.records, vec![record]);
    }

    #[test]
    fn concurrent_writers_produce_only_complete_lines() {
        let directory = crate::test_support::canonical_tempdir().expect("temporary directory");
        let sink = Arc::new(FileTelemetrySink::new(directory.path().to_path_buf()));
        let writers = (0..8)
            .map(|writer| {
                let sink = Arc::clone(&sink);
                std::thread::spawn(move || {
                    for index in 0..50 {
                        sink.record_workload_resource(&populated_record(&format!(
                            "{writer}-{index}"
                        )));
                    }
                })
            })
            .collect::<Vec<_>>();
        for writer in writers {
            writer.join().expect("writer finishes");
        }

        let records =
            WorkloadResourceRecords::read(&[directory.path().to_path_buf()]).expect("records read");

        assert_eq!(records.records.len(), 400);
        assert_eq!(records.malformed, Vec::new());
    }

    #[test]
    fn unwritable_directory_is_reported_without_panicking() {
        let directory = crate::test_support::canonical_tempdir().expect("temporary directory");
        let blocker = directory.path().join("blocker");
        fs::write(&blocker, b"not a directory").expect("blocker file");
        let sink = FileTelemetrySink::new(blocker);

        assert!(matches!(
            sink.append_workload_resource(&populated_record("blocked")),
            Err(TelemetryWriteError::Io { .. })
        ));
        sink.record_workload_resource(&populated_record("blocked"));
    }

    #[test]
    fn new_records_have_unique_ids_and_the_build_version() {
        let now = UNIX_EPOCH + Duration::from_secs(1_790_000_000);
        let first = new_workload_resource_record(WorkloadResourceSource::Development, now);
        let second = new_workload_resource_record(WorkloadResourceSource::Development, now);

        assert_ne!(first.record_id, second.record_id);
        assert_eq!(first.record_id.len(), 32);
        assert_eq!(first.producer_version, PRODUCER_VERSION);
        assert_eq!(
            first.recorded_at,
            Some(Timestamp {
                seconds: 1_790_000_000,
                nanos: 0
            })
        );
        assert_eq!(first.source, WorkloadResourceSource::Development as i32);
    }

    pub(super) fn populated_record(record_id: &str) -> WorkloadResourceRecord {
        WorkloadResourceRecord {
            record_id: record_id.to_owned(),
            recorded_at: Some(Timestamp {
                seconds: 1_790_298_000,
                nanos: 5,
            }),
            producer_version: "0.1.0".to_owned(),
            source: WorkloadResourceSource::Deployment.into(),
            realization: Some(WorkloadRealizationKey {
                owner_id: "deployment-1".to_owned(),
                generation: 7,
            }),
            inputs: Some(WorkloadResourceInputs {
                model_id: "qwen3-8-27b".to_owned(),
                configuration_id: "vllm-nvidia-cuda".to_owned(),
                hardware_class_id: "nvidia".to_owned(),
                profile_id: "nvfp4-text".to_owned(),
                model_artifact_id: "nvfp4".to_owned(),
                auxiliary_artifact_ids: vec!["chat-template".to_owned()],
                engine_release_id: "vllm-v0-32-0".to_owned(),
                engine_artifact_id: "linux-amd64-cuda".to_owned(),
                engine_artifact_reference: "docker.io/vllm/vllm-openai@sha256:d392".to_owned(),
                engine_parameter_values: Some(EngineParameterValues {
                    values: vec![
                        EngineParameterValue {
                            parameter_id: "kv-cache-memory-bytes".to_owned(),
                            value: Some(EngineValue::UnsignedInteger(6_000_000_000)),
                        },
                        EngineParameterValue {
                            parameter_id: "prefix-caching".to_owned(),
                            value: Some(EngineValue::Boolean(false)),
                        },
                    ],
                }),
                option_values: vec![ModelOptionValues {
                    option_id: "mtp".to_owned(),
                    values: vec![ModelOptionParameterValue {
                        parameter_id: "token-count".to_owned(),
                        value: Some(OptionValue::UnsignedInteger(0)),
                    }],
                }],
                custom_argument_tokens: vec![
                    "--seed".to_owned(),
                    "7".to_owned(),
                    "--api-key".to_owned(),
                    "<redacted>".to_owned(),
                ],
            }),
            node: Some(WorkloadResourceNode {
                node_id: "gpu1".to_owned(),
                host_total_memory_bytes: Some(64_000_000_000),
                host_available_memory_bytes: Some(40_000_000_000),
                devices: vec![WorkloadResourceDevice {
                    device_id: "GPU-1".to_owned(),
                    model: "NVIDIA GeForce RTX 5090".to_owned(),
                    driver_version: "580.95".to_owned(),
                    memory_architecture: AcceleratorMemoryArchitecture::Dedicated.into(),
                    total_memory_bytes: Some(34_190_917_632),
                    attributed_memory_bytes: 30_559_698_944,
                    unattributed_peak_memory_bytes: Some(1_200_000_000),
                }],
            }),
            observation: Some(WorkloadResourceObservation {
                kind: WorkloadResourceObservationKind::Startup.into(),
                since_ready: Some(ProtoDuration {
                    seconds: 12,
                    nanos: 0,
                }),
                attributed_memory_bytes: 30_559_698_944,
                engine_facts: Some(EngineFacts::Vllm(VllmStartupFacts {
                    extractor_version: 1,
                    kv_cache_token_capacity: Some(172_480),
                    kv_cache_reserved_bytes: Some(6_002_196_234),
                    model_loading_bytes: Some(22_913_568_604),
                    cuda_graph_bytes: Some(601_295_421),
                    startup_cuda_free_bytes: Some(33_135_910_010),
                })),
            }),
        }
    }
}
