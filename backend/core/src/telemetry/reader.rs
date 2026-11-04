//! Reads workload resource records back from one or more local telemetry
//! directories, for example one copied from each host, as a single deduplicated set.
//!
//! A record present in several directories counts once. An unterminated
//! final line, left by a writer that was interrupted, is skipped silently;
//! any other unreadable line is reported with its file and line number
//! instead of aborting the read.

use std::{
    collections::BTreeSet,
    fs, io,
    path::{Path, PathBuf},
};

use dotcore_proto::dotcore::telemetry::v1::WorkloadResourceRecord;

use super::{RECORD_FILE_EXTENSION, WORKLOAD_RESOURCES_CHILD};

/// Workload resource records read from telemetry directories, in file and line order.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct WorkloadResourceRecords {
    pub records: Vec<WorkloadResourceRecord>,
    pub malformed: Vec<MalformedTelemetryLine>,
}

impl WorkloadResourceRecords {
    /// Reads every record under the given directories, deduplicated by record id.
    pub fn read(directories: &[PathBuf]) -> Result<Self, TelemetryReadError> {
        let mut result = Self::default();
        let mut seen = BTreeSet::new();
        for directory in directories {
            if !directory.is_dir() {
                return Err(TelemetryReadError::MissingDirectory(directory.clone()));
            }
            for path in record_files(&directory.join(WORKLOAD_RESOURCES_CHILD))? {
                result.read_file(&path, &mut seen)?;
            }
        }
        Ok(result)
    }

    fn read_file(
        &mut self,
        path: &Path,
        seen: &mut BTreeSet<String>,
    ) -> Result<(), TelemetryReadError> {
        let bytes = fs::read(path).map_err(|source| TelemetryReadError::Io {
            path: path.to_path_buf(),
            source,
        })?;
        // Invalid UTF-8 from a damaged write fails only its own line's decoding.
        let content = String::from_utf8_lossy(&bytes);
        let terminated = content.ends_with('\n');
        let lines = content.lines().collect::<Vec<_>>();
        for (index, line) in lines.iter().enumerate() {
            if line.trim().is_empty() || (!terminated && index + 1 == lines.len()) {
                continue;
            }
            match serde_json::from_str::<WorkloadResourceRecord>(line) {
                Ok(record) => {
                    if seen.insert(record.record_id.clone()) {
                        self.records.push(record);
                    }
                }
                Err(error) => self.malformed.push(MalformedTelemetryLine {
                    path: path.to_path_buf(),
                    line: index + 1,
                    message: error.to_string(),
                }),
            }
        }
        Ok(())
    }
}

/// One telemetry line that could not be decoded.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct MalformedTelemetryLine {
    pub path: PathBuf,
    pub line: usize,
    pub message: String,
}

/// Failure to read a telemetry directory.
#[derive(Debug, thiserror::Error)]
pub enum TelemetryReadError {
    #[error("telemetry directory {0} does not exist")]
    MissingDirectory(PathBuf),

    #[error("telemetry path {path} could not be read")]
    Io {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
}

fn record_files(directory: &Path) -> Result<Vec<PathBuf>, TelemetryReadError> {
    let entries = match fs::read_dir(directory) {
        Ok(entries) => entries,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(source) => {
            return Err(TelemetryReadError::Io {
                path: directory.to_path_buf(),
                source,
            });
        }
    };
    let mut files = Vec::new();
    for entry in entries {
        let path = entry
            .map_err(|source| TelemetryReadError::Io {
                path: directory.to_path_buf(),
                source,
            })?
            .path();
        if path.extension().and_then(|value| value.to_str()) == Some(RECORD_FILE_EXTENSION) {
            files.push(path);
        }
    }
    files.sort();
    Ok(files)
}

#[cfg(test)]
mod tests {
    use std::io::Write;

    use super::*;
    use crate::telemetry::{FileTelemetrySink, tests::populated_record};

    #[test]
    fn merges_directories_and_dedupes_shared_records() {
        let first = crate::test_support::canonical_tempdir().expect("first directory");
        let second = crate::test_support::canonical_tempdir().expect("second directory");
        let shared = populated_record("shared");
        let only_first = populated_record("first");
        let only_second = populated_record("second");
        let first_sink = FileTelemetrySink::new(first.path().to_path_buf());
        let second_sink = FileTelemetrySink::new(second.path().to_path_buf());
        for record in [&shared, &only_first] {
            first_sink
                .append_workload_resource(record)
                .expect("first append");
        }
        for record in [&shared, &only_second] {
            second_sink
                .append_workload_resource(record)
                .expect("second append");
        }

        let records = WorkloadResourceRecords::read(&[
            first.path().to_path_buf(),
            second.path().to_path_buf(),
        ])
        .expect("records read");

        assert_eq!(records.records, vec![shared, only_first, only_second]);
        assert_eq!(records.malformed, Vec::new());
    }

    #[test]
    fn skips_partial_final_line_and_reports_other_malformed_lines() {
        let directory = crate::test_support::canonical_tempdir().expect("temporary directory");
        let sink = FileTelemetrySink::new(directory.path().to_path_buf());
        let record = populated_record("valid");
        let path = sink
            .append_workload_resource(&record)
            .expect("record appends");
        let mut file = fs::OpenOptions::new()
            .append(true)
            .open(&path)
            .expect("file opens");
        file.write_all(b"{not json}\n{\"recordId\":\"trunc")
            .expect("corruption appends");

        let records =
            WorkloadResourceRecords::read(&[directory.path().to_path_buf()]).expect("records read");

        assert_eq!(records.records, vec![record]);
        assert_eq!(records.malformed.len(), 1);
        assert_eq!(records.malformed[0].path, path);
        assert_eq!(records.malformed[0].line, 2);
    }

    #[test]
    fn missing_directory_fails_and_empty_directory_reads_nothing() {
        let directory = crate::test_support::canonical_tempdir().expect("temporary directory");

        assert_eq!(
            WorkloadResourceRecords::read(&[directory.path().to_path_buf()])
                .expect("empty directory reads"),
            WorkloadResourceRecords::default()
        );
        assert!(matches!(
            WorkloadResourceRecords::read(&[directory.path().join("absent")]),
            Err(TelemetryReadError::MissingDirectory(_))
        ));
    }
}
