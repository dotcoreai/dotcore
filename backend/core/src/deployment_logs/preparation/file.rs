//! Keeps one preparation step's output as timestamped line records in a size-capped file, with
//! exactly one rotated file beside it.
//!
//! Each record holds its time, its output stream and its text on one line, so both streams of a
//! step share one ordered file. Writing past the cap rotates the current file; a rotation that
//! discards an older rotated file leaves a marker, so readers can say earlier output was dropped.
//! Readers never see a partly written record, and a record a crash cut short is dropped when the
//! log is next written. Beside its log, a step keeps the artifact it last decided was prepared,
//! which rotation never drops.

use std::{
    fs::{self, File, OpenOptions},
    io::{self, BufRead, BufReader, Read, Seek, SeekFrom, Write},
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
};

use dotcore_runtime::RuntimeLogStreamKind;

/// Attempts a read makes to see no rotation while it reads both files before it settles for what
/// it read.
const READ_ATTEMPTS: usize = 8;

/// Bytes read from the end of a file to find its last record: two of the longest records, so the
/// newline before the last one is always among them.
const TAIL_BYTES: u64 = 2 * (64 * 1024 + 64);

/// The files of one step's log in its deployment's directory. Clones share their writer's
/// rotation count, so their reads see rotations.
#[derive(Clone, Debug)]
pub(super) struct StepFiles {
    directory: PathBuf,
    path: PathBuf,
    rotated_path: PathBuf,
    dropped_path: PathBuf,
    decision_path: PathBuf,

    /// Odd while a rotation moves the current file, then even again; a read that sees it change
    /// may have missed records.
    rotations: Arc<AtomicU64>,
}

impl StepFiles {
    pub(super) fn new(directory: &Path, stem: &str) -> Self {
        Self {
            directory: directory.to_owned(),
            path: directory.join(format!("{stem}.log")),
            rotated_path: directory.join(format!("{stem}.log.1")),
            dropped_path: directory.join(format!("{stem}.dropped")),
            decision_path: directory.join(format!("{stem}.decision")),
            rotations: Arc::new(AtomicU64::new(0)),
        }
    }

    /// Whether the step has written anything that is still retained.
    pub(super) fn exists(&self) -> bool {
        self.path.exists() || self.rotated_path.exists()
    }

    /// Returns the time of the oldest retained record, reading only the first line of a file.
    pub(super) fn first_time_ns(&self) -> Option<u64> {
        [&self.rotated_path, &self.path]
            .into_iter()
            .find_map(|path| {
                let mut line = Vec::new();
                BufReader::new(File::open(path).ok()?)
                    .read_until(b'\n', &mut line)
                    .ok()?;
                Some(Record::decode(line.strip_suffix(b"\n")?)?.time_ns)
            })
    }

    /// Returns the time of the newest retained record, reading only the end of a file.
    pub(super) fn last_time_ns(&self) -> io::Result<Option<u64>> {
        for path in [&self.path, &self.rotated_path] {
            let mut file = match File::open(path) {
                Ok(file) => file,
                Err(error) if error.kind() == io::ErrorKind::NotFound => continue,
                Err(error) => return Err(error),
            };
            let length = file.metadata()?.len();
            file.seek(SeekFrom::Start(length.saturating_sub(TAIL_BYTES)))?;
            let mut tail = Vec::new();
            file.read_to_end(&mut tail)?;
            if let Some(record) = complete_records(&tail).rev().find_map(Record::decode) {
                return Ok(Some(record.time_ns));
            }
        }
        Ok(None)
    }

    /// Reads every retained record, oldest first, and whether older output was dropped. A read
    /// that a rotation moved records under is retried.
    pub(super) fn read(&self) -> io::Result<StepRecords> {
        let mut attempt = 0;
        loop {
            attempt += 1;
            let before = self.rotations.load(Ordering::Acquire);
            let read = self.read_once()?;
            let after = self.rotations.load(Ordering::Acquire);
            if (before == after && before.is_multiple_of(2)) || attempt == READ_ATTEMPTS {
                return Ok(read);
            }
            std::thread::yield_now();
        }
    }

    /// Returns the artifact the step last found or made prepared, if any.
    pub(super) fn decision(&self) -> io::Result<Option<String>> {
        match fs::read_to_string(&self.decision_path) {
            Ok(artifact) => Ok(Some(artifact)),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
            Err(error) => Err(error),
        }
    }

    /// Records `artifact` as the step's last decision, or forgets it with `None`. The record is
    /// replaced whole, so a crash leaves either the old or the new decision.
    pub(super) fn record_decision(&self, artifact: Option<&str>) -> io::Result<()> {
        let Some(artifact) = artifact else {
            return match fs::remove_file(&self.decision_path) {
                Err(error) if error.kind() != io::ErrorKind::NotFound => Err(error),
                _ => Ok(()),
            };
        };
        let mut written = self.decision_path.clone().into_os_string();
        written.push(".tmp");
        fs::write(&written, artifact)?;
        fs::rename(&written, &self.decision_path)
    }

    fn read_once(&self) -> io::Result<StepRecords> {
        let mut records = Vec::new();
        for path in [&self.rotated_path, &self.path] {
            match fs::read(path) {
                Ok(data) => records.extend(complete_records(&data).filter_map(Record::decode)),
                Err(error) if error.kind() == io::ErrorKind::NotFound => {}
                Err(error) => return Err(error),
            }
        }
        Ok(StepRecords {
            records,
            dropped: self.dropped_path.exists(),
        })
    }
}

/// Returns the newline-terminated records of `data`; a last record still being written, or cut
/// short by a crash, is left out.
fn complete_records(data: &[u8]) -> impl DoubleEndedIterator<Item = &[u8]> {
    let end = data
        .iter()
        .rposition(|byte| *byte == b'\n')
        .map_or(0, |end| end + 1);
    data[..end]
        .split(|byte| *byte == b'\n')
        .filter(|record| !record.is_empty())
}

/// The retained records of one step, oldest first.
pub(super) struct StepRecords {
    pub(super) records: Vec<Record>,

    /// Whether a rotation discarded output older than the first record.
    pub(super) dropped: bool,
}

/// One stored line of step output.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct Record {
    /// Unix time in nanoseconds, strictly increasing within a step.
    pub(super) time_ns: u64,
    pub(super) stream: RuntimeLogStreamKind,
    /// Line text without a newline.
    pub(super) content: Vec<u8>,
}

impl Record {
    /// Encodes the record as `<unix nanoseconds> <o|e> <text>` and a newline.
    fn encode(&self) -> Vec<u8> {
        let stream = match self.stream {
            RuntimeLogStreamKind::Stdout => 'o',
            RuntimeLogStreamKind::Stderr => 'e',
        };
        let mut encoded = format!("{} {stream} ", self.time_ns).into_bytes();
        encoded.extend_from_slice(&self.content);
        encoded.push(b'\n');
        encoded
    }

    /// Parses one stored record without its newline; `None` when it is malformed, as a record
    /// cut by a crash mid-write is.
    fn decode(record: &[u8]) -> Option<Self> {
        let mut parts = record.splitn(3, |byte| *byte == b' ');
        let time_ns = std::str::from_utf8(parts.next()?).ok()?.parse().ok()?;
        let stream = match parts.next()? {
            b"o" => RuntimeLogStreamKind::Stdout,
            b"e" => RuntimeLogStreamKind::Stderr,
            _ => return None,
        };
        Some(Self {
            time_ns,
            stream,
            content: parts.next()?.to_vec(),
        })
    }
}

/// Appends records to one step's log, rotating it at its cap.
pub(super) struct StepFileWriter {
    files: StepFiles,
    file: File,
    size: u64,
    cap: u64,
}

impl StepFileWriter {
    /// Opens the step's current file for appending, creating it and its directory when absent.
    /// A last record a crash cut short is removed, so the next record starts on its own line.
    pub(super) fn open(files: StepFiles, cap: u64) -> io::Result<Self> {
        fs::create_dir_all(&files.directory)?;
        let file = append_to(&files.path)?;
        let mut size = file.metadata()?.len();
        if size > 0 {
            let mut reader = File::open(&files.path)?;
            reader.seek(SeekFrom::Start(size.saturating_sub(TAIL_BYTES)))?;
            let mut tail = Vec::new();
            reader.read_to_end(&mut tail)?;
            if tail.last() != Some(&b'\n') {
                let kept = tail.iter().rposition(|byte| *byte == b'\n').map_or(
                    // A cut record longer than the tail cannot exist, as records are bounded.
                    0,
                    |end| size - tail.len() as u64 + end as u64 + 1,
                );
                file.set_len(kept)?;
                size = kept;
            }
        }
        Ok(Self {
            files,
            file,
            size,
            cap,
        })
    }

    /// Appends one record with a single write, so a reader never sees half of it.
    pub(super) fn append(&mut self, record: &Record) -> io::Result<()> {
        let encoded = record.encode();
        if self.size > 0 && self.size + encoded.len() as u64 > self.cap {
            self.rotate()?;
        }
        self.file.write_all(&encoded)?;
        self.size += encoded.len() as u64;
        Ok(())
    }

    fn rotate(&mut self) -> io::Result<()> {
        if self.files.rotated_path.exists() {
            File::create(&self.files.dropped_path)?;
        }
        self.files.rotations.fetch_add(1, Ordering::AcqRel);
        let renamed = fs::rename(&self.files.path, &self.files.rotated_path);
        self.files.rotations.fetch_add(1, Ordering::AcqRel);
        renamed?;
        self.file = append_to(&self.files.path)?;
        self.size = 0;
        Ok(())
    }
}

fn append_to(path: &Path) -> io::Result<File> {
    OpenOptions::new().create(true).append(true).open(path)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn records_keep_their_time_stream_and_text_in_write_order() {
        let directory = tempfile::tempdir().unwrap();
        let files = StepFiles::new(directory.path(), "engine-build");
        let mut writer = StepFileWriter::open(files.clone(), 1024).unwrap();

        writer
            .append(&record(1, RuntimeLogStreamKind::Stdout, "started"))
            .unwrap();
        writer
            .append(&record(2, RuntimeLogStreamKind::Stderr, "two  spaces "))
            .unwrap();

        let read = files.read().unwrap();
        assert_eq!(
            read.records,
            [
                record(1, RuntimeLogStreamKind::Stdout, "started"),
                record(2, RuntimeLogStreamKind::Stderr, "two  spaces "),
            ]
        );
        assert!(!read.dropped);
        assert_eq!(
            fs::read_to_string(directory.path().join("engine-build.log")).unwrap(),
            "1 o started\n2 e two  spaces \n"
        );
    }

    #[test]
    fn writing_past_the_cap_keeps_one_rotated_file_and_marks_dropped_output() {
        let directory = tempfile::tempdir().unwrap();
        let files = StepFiles::new(directory.path(), "download");
        // Each record is 10 bytes, so a file holds 3 of them.
        let mut writer = StepFileWriter::open(files.clone(), 30).unwrap();

        for time in 10..16 {
            writer
                .append(&record(time, RuntimeLogStreamKind::Stdout, "line"))
                .unwrap();
        }
        let before_drop = files.read().unwrap();
        writer
            .append(&record(16, RuntimeLogStreamKind::Stdout, "line"))
            .unwrap();
        let after_drop = files.read().unwrap();

        assert_eq!(times(&before_drop), [10, 11, 12, 13, 14, 15]);
        assert!(!before_drop.dropped);
        assert_eq!(times(&after_drop), [13, 14, 15, 16]);
        assert!(after_drop.dropped);
        assert!(!directory.path().join("download.log.2").exists());
    }

    #[test]
    fn a_reopened_writer_appends_after_retained_records() {
        let directory = tempfile::tempdir().unwrap();
        let files = StepFiles::new(directory.path(), "download");
        StepFileWriter::open(files.clone(), 1024)
            .unwrap()
            .append(&record(1, RuntimeLogStreamKind::Stdout, "first"))
            .unwrap();

        StepFileWriter::open(files.clone(), 1024)
            .unwrap()
            .append(&record(2, RuntimeLogStreamKind::Stdout, "second"))
            .unwrap();

        assert_eq!(times(&files.read().unwrap()), [1, 2]);
    }

    #[test]
    fn a_record_cut_by_a_crash_is_dropped_before_the_next_one_is_written() {
        let directory = tempfile::tempdir().unwrap();
        let files = StepFiles::new(directory.path(), "download");
        fs::write(directory.path().join("download.log"), "1 o kept\n2 o cu").unwrap();
        assert_eq!(files.last_time_ns().unwrap(), Some(1));

        StepFileWriter::open(files.clone(), 1024)
            .unwrap()
            .append(&record(3, RuntimeLogStreamKind::Stdout, "next"))
            .unwrap();

        assert_eq!(
            fs::read_to_string(directory.path().join("download.log")).unwrap(),
            "1 o kept\n3 o next\n"
        );
        assert_eq!(times(&files.read().unwrap()), [1, 3]);
    }

    #[test]
    fn the_latest_time_comes_from_the_current_file_else_the_rotated_one() {
        let directory = tempfile::tempdir().unwrap();
        let files = StepFiles::new(directory.path(), "download");
        assert_eq!(files.last_time_ns().unwrap(), None);
        fs::write(
            directory.path().join("download.log.1"),
            "4 o old\n5 o older\n",
        )
        .unwrap();
        assert_eq!(files.last_time_ns().unwrap(), Some(5));

        let long = "x".repeat(64 * 1024);
        fs::write(
            directory.path().join("download.log"),
            format!("6 o {long}\n7 o {long}\n"),
        )
        .unwrap();

        assert_eq!(files.last_time_ns().unwrap(), Some(7));
    }

    #[test]
    fn malformed_records_are_skipped() {
        let directory = tempfile::tempdir().unwrap();
        let files = StepFiles::new(directory.path(), "download");
        fs::write(
            directory.path().join("download.log"),
            "1 o kept\nnot-a-time o x\n2 x bad stream\n3 e",
        )
        .unwrap();

        assert_eq!(
            files.read().unwrap().records,
            [record(1, RuntimeLogStreamKind::Stdout, "kept")]
        );
    }

    fn record(time_ns: u64, stream: RuntimeLogStreamKind, content: &str) -> Record {
        Record {
            time_ns,
            stream,
            content: content.as_bytes().to_vec(),
        }
    }

    fn times(read: &StepRecords) -> Vec<u64> {
        read.records.iter().map(|record| record.time_ns).collect()
    }
}
