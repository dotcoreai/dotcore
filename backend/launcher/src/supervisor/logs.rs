//! Reads the log records a launcher wrote for its workload, either once or by following them
//! until the launcher exits.
//!
//! Each stream keeps a current file and one rotated file; reading starts with the rotated file and
//! a follower that sees a rotation finishes the old file before moving to the new one, so no
//! record is skipped while the reader keeps up with the writer.

use std::{
    collections::VecDeque,
    fs::{self, File},
    io::{self, Read},
    os::unix::fs::MetadataExt,
    path::{Path, PathBuf},
    time::Duration,
};

use super::{Result, Supervisor, SupervisorError};
use crate::workload::{LogRecord, LogStream};

/// Delay between two reads of a followed log that had nothing new.
const FOLLOW_INTERVAL: Duration = Duration::from_millis(100);

/// Reads the retained records of both streams, ordered by time.
pub(super) fn read_all(dir: &Path) -> io::Result<Vec<LogRecord>> {
    let mut records = Vec::new();
    for stream in [LogStream::Stdout, LogStream::Stderr] {
        let mut cursor = StreamCursor::new(dir, stream);
        cursor.read_new(&mut records)?;
    }
    records.sort_by_key(|record| record.time_ns);
    Ok(records)
}

/// Follows one workload's log records, oldest retained first, until its launcher exits.
pub struct LogFollower {
    state: Option<FollowState>,
    pending: VecDeque<LogRecord>,
    finished: bool,
}

impl LogFollower {
    pub(super) fn new(supervisor: Supervisor, dir: PathBuf) -> Self {
        let cursors = [
            StreamCursor::new(&dir, LogStream::Stdout),
            StreamCursor::new(&dir, LogStream::Stderr),
        ];
        Self {
            state: Some(FollowState {
                supervisor,
                dir,
                cursors,
            }),
            pending: VecDeque::new(),
            finished: false,
        }
    }

    /// Returns the next record, waiting for one while the launcher runs, or `None` once the
    /// launcher has exited and every record was returned.
    pub async fn next(&mut self) -> Result<Option<LogRecord>> {
        loop {
            if let Some(record) = self.pending.pop_front() {
                return Ok(Some(record));
            }
            if self.finished {
                return Ok(None);
            }

            let mut state = self
                .state
                .take()
                .expect("follow state is restored after each read");
            let (state, read) = tokio::task::spawn_blocking(move || {
                let read = state.read();
                (state, read)
            })
            .await
            .map_err(SupervisorError::Task)?;
            self.state = Some(state);
            let (records, launcher_running) = read?;

            if records.is_empty() {
                if !launcher_running {
                    self.finished = true;
                } else {
                    tokio::time::sleep(FOLLOW_INTERVAL).await;
                }
            }
            self.pending.extend(records);
        }
    }
}

struct FollowState {
    supervisor: Supervisor,
    dir: PathBuf,
    cursors: [StreamCursor; 2],
}

impl FollowState {
    /// Reads new records of both streams and whether the launcher was running before the read,
    /// in which case more records may follow.
    fn read(&mut self) -> Result<(Vec<LogRecord>, bool)> {
        let launcher_running = self.supervisor.live_launcher(&self.dir)?.is_some();
        let mut records = Vec::new();
        for cursor in &mut self.cursors {
            cursor
                .read_new(&mut records)
                .map_err(|source| SupervisorError::Io {
                    context: "read workload logs",
                    source,
                })?;
        }
        records.sort_by_key(|record| record.time_ns);
        Ok((records, launcher_running))
    }
}

/// Read position in one stream's log files: the open files still to finish, oldest first, read
/// through their descriptors so a rotation never moves data out from under the reader.
struct StreamCursor {
    stream: LogStream,
    path: PathBuf,
    rotated_path: PathBuf,
    files: VecDeque<File>,
    partial: Vec<u8>,
}

impl StreamCursor {
    fn new(dir: &Path, stream: LogStream) -> Self {
        Self {
            stream,
            path: dir.join(stream.file_name()),
            rotated_path: dir.join(stream.rotated_file_name()),
            files: VecDeque::new(),
            partial: Vec::new(),
        }
    }

    /// Appends the complete records written since the previous read.
    fn read_new(&mut self, records: &mut Vec<LogRecord>) -> io::Result<()> {
        if self.files.is_empty() {
            // The current file is opened first: if the writer rotates before the rotated file is
            // opened, both names reach the same file, which is then read once.
            let current = open_existing(&self.path)?;
            let rotated = open_existing(&self.rotated_path)?;
            match (rotated, current) {
                (Some(rotated), Some(current)) => {
                    if inode(&rotated)? != inode(&current)? {
                        self.files.push_back(rotated);
                    }
                    self.files.push_back(current);
                }
                (Some(only), None) | (None, Some(only)) => self.files.push_back(only),
                (None, None) => return Ok(()),
            }
        }

        let mut data = Vec::new();
        loop {
            let newest = self.files.back().expect("cursor keeps at least one file");
            // A different file at the path means the writer rotated: the open file is complete
            // once read to its end, and the new file holds everything after it.
            let next = match fs::metadata(&self.path) {
                Ok(current) if current.ino() != inode(newest)? => open_existing(&self.path)?,
                Ok(_) => None,
                Err(error) if error.kind() == io::ErrorKind::NotFound => None,
                Err(error) => return Err(error),
            };
            let mut oldest = self
                .files
                .pop_front()
                .expect("cursor keeps at least one file");
            oldest.read_to_end(&mut data)?;
            if let Some(next) = next {
                self.files.push_back(next);
            }
            if self.files.is_empty() {
                // Nothing newer exists yet, so keep reading this file later.
                self.files.push_back(oldest);
                break;
            }
        }
        self.take_records(&data, records);
        Ok(())
    }

    fn take_records(&mut self, data: &[u8], records: &mut Vec<LogRecord>) {
        self.partial.extend_from_slice(data);
        let Some(end) = self.partial.iter().rposition(|byte| *byte == b'\n') else {
            return;
        };
        let rest = self.partial.split_off(end + 1);
        records.extend(
            self.partial
                .split(|byte| *byte == b'\n')
                .filter(|record| !record.is_empty())
                .filter_map(|record| LogRecord::decode(self.stream, record)),
        );
        self.partial = rest;
    }
}

fn open_existing(path: &Path) -> io::Result<Option<File>> {
    match File::open(path) {
        Ok(file) => Ok(Some(file)),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error),
    }
}

fn inode(file: &File) -> io::Result<u64> {
    Ok(file.metadata()?.ino())
}

#[cfg(test)]
mod tests {
    use std::io::Write;

    use super::*;
    use crate::workload::encode_log_record;

    #[test]
    fn a_rotated_file_without_a_current_file_is_read_once() {
        let dir = tempfile::tempdir().unwrap();
        append(&dir.path().join("stdout.log.1"), &["old"]);
        let mut cursor = StreamCursor::new(dir.path(), LogStream::Stdout);

        assert_eq!(read(&mut cursor), ["old"]);
        assert!(read(&mut cursor).is_empty());

        append(&dir.path().join("stdout.log"), &["new"]);
        assert_eq!(read(&mut cursor), ["new"]);
    }

    #[test]
    fn one_file_under_both_names_is_read_once() {
        let dir = tempfile::tempdir().unwrap();
        append(&dir.path().join("stdout.log"), &["a", "b"]);
        fs::hard_link(
            dir.path().join("stdout.log"),
            dir.path().join("stdout.log.1"),
        )
        .unwrap();
        let mut cursor = StreamCursor::new(dir.path(), LogStream::Stdout);

        assert_eq!(read(&mut cursor), ["a", "b"]);
    }

    #[test]
    fn a_rotation_between_reads_keeps_every_record_once() {
        let dir = tempfile::tempdir().unwrap();
        let current = dir.path().join("stdout.log");
        append(&dir.path().join("stdout.log.1"), &["oldest"]);
        append(&current, &["first"]);
        let mut cursor = StreamCursor::new(dir.path(), LogStream::Stdout);
        assert_eq!(read(&mut cursor), ["oldest", "first"]);

        append(&current, &["second"]);
        fs::rename(&current, dir.path().join("stdout.log.1")).unwrap();
        assert_eq!(read(&mut cursor), ["second"]);
        append(&current, &["third"]);

        assert_eq!(read(&mut cursor), ["third"]);
        assert!(read(&mut cursor).is_empty());
    }

    fn read(cursor: &mut StreamCursor) -> Vec<String> {
        let mut records = Vec::new();
        cursor.read_new(&mut records).unwrap();
        records.into_iter().map(|record| record.line).collect()
    }

    fn append(path: &Path, lines: &[&str]) {
        let mut file = fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)
            .unwrap();
        for line in lines {
            file.write_all(&encode_log_record(1, line.as_bytes()))
                .unwrap();
        }
    }
}
