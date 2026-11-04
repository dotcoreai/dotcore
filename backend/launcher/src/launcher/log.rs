//! Turns one output stream of the held process into timestamped line records in a size-capped
//! file, keeping exactly one rotated file beside it.
//!
//! A stream that never ends a line is split into records of a bounded length, so memory stays
//! bounded whatever the process writes. Log failures never stop the held process: the writer
//! drops further output of that stream and leaves one note in the log when it still can.

use std::{
    fs::{self, File, OpenOptions},
    io::{self, BufWriter, Write},
    path::{Path, PathBuf},
};

use crate::workload::{LogStream, encode_log_record, unix_time_ns};

/// Longest record text; longer lines are split into several records.
const MAX_RECORD_BYTES: usize = 64 * 1024;

/// Rotating record writer for one output stream.
pub(super) struct LogWriter {
    stream: LogStream,
    path: PathBuf,
    rotated_path: PathBuf,
    /// Open log file, or `None` once a failure disabled the stream.
    file: Option<BufWriter<File>>,
    size: u64,
    cap: u64,
    partial: Vec<u8>,
    /// Whether the last record was cut at the length bound, so a newline that follows right away
    /// ends that record instead of starting an empty one.
    split: bool,
}

impl LogWriter {
    /// Opens the stream's log; a failure leaves a writer that drops output.
    pub(super) fn open(dir: &Path, stream: LogStream, cap: u64) -> Self {
        let path = dir.join(stream.file_name());
        let opened = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
            .and_then(|file| Ok((file.metadata()?.len(), file)));
        let mut writer = Self {
            stream,
            rotated_path: dir.join(stream.rotated_file_name()),
            path,
            file: None,
            size: 0,
            cap,
            partial: Vec::new(),
            split: false,
        };
        match opened {
            Ok((size, file)) => {
                writer.size = size;
                writer.file = Some(BufWriter::new(file));
            }
            Err(error) => writer.disable(&error),
        }
        writer
    }

    /// Records every complete line of the chunk and keeps a trailing partial line for later.
    pub(super) fn write(&mut self, chunk: &[u8]) {
        if let Err(error) = self.try_write(chunk) {
            self.disable(&error);
        }
    }

    /// Records a trailing partial line, used once the stream has ended.
    pub(super) fn finish(&mut self) {
        let result = if self.partial.is_empty() {
            Ok(())
        } else {
            self.flush_partial(unix_time_ns())
        };
        if let Err(error) = result.and_then(|()| self.flush()) {
            self.disable(&error);
        }
    }

    /// Records a launcher diagnostic line in this stream.
    pub(super) fn note(&mut self, message: &str) {
        self.write(format!("dotcore-launcher: {message}\n").as_bytes());
    }

    fn try_write(&mut self, chunk: &[u8]) -> io::Result<()> {
        if self.file.is_none() {
            return Ok(());
        }
        let time_ns = unix_time_ns();
        for piece in chunk.split_inclusive(|byte| *byte == b'\n') {
            match piece.strip_suffix(b"\n") {
                Some(line) => {
                    let ends_split_record =
                        line.is_empty() && self.partial.is_empty() && self.split;
                    self.split = false;
                    if !ends_split_record {
                        self.partial.extend_from_slice(line);
                        self.flush_partial(time_ns)?;
                    }
                }
                None => {
                    self.partial.extend_from_slice(piece);
                    // The cut can fall inside a multi-byte UTF-8 character; readers replace the
                    // two halves, which is accepted for lines this long.
                    while self.partial.len() >= MAX_RECORD_BYTES {
                        let rest = self.partial.split_off(MAX_RECORD_BYTES);
                        self.flush_partial(time_ns)?;
                        self.partial = rest;
                        self.split = true;
                    }
                }
            }
        }
        self.flush()
    }

    fn flush_partial(&mut self, time_ns: u64) -> io::Result<()> {
        let record = encode_log_record(time_ns, &self.partial);
        self.partial.clear();
        if self.size > 0 && self.size + record.len() as u64 > self.cap {
            self.rotate()?;
        }
        if let Some(file) = &mut self.file {
            file.write_all(&record)?;
        }
        self.size += record.len() as u64;
        Ok(())
    }

    fn rotate(&mut self) -> io::Result<()> {
        self.flush()?;
        fs::rename(&self.path, &self.rotated_path)?;
        self.file = Some(BufWriter::new(File::create(&self.path)?));
        self.size = 0;
        Ok(())
    }

    fn flush(&mut self) -> io::Result<()> {
        match &mut self.file {
            Some(file) => file.flush(),
            None => Ok(()),
        }
    }

    /// Stops logging this stream and leaves one note at the end of its current file, best effort.
    fn disable(&mut self, error: &io::Error) {
        self.file = None;
        self.partial.clear();
        let note = format!(
            "dotcore-launcher: {} log disabled after a write failure: {error}",
            self.stream.file_name()
        );
        if let Ok(mut file) = OpenOptions::new().append(true).open(&self.path) {
            let _ = file.write_all(&encode_log_record(unix_time_ns(), note.as_bytes()));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::workload::LogRecord;

    #[test]
    fn lines_become_timestamped_records_and_partial_lines_wait_for_their_end() {
        let dir = tempfile::tempdir().unwrap();
        let mut writer = LogWriter::open(dir.path(), LogStream::Stdout, 1024);

        writer.write(b"first\nsec");
        writer.write(b"ond\nthi");
        assert_eq!(lines(&dir.path().join("stdout.log")), ["first", "second"]);

        writer.finish();
        assert_eq!(
            lines(&dir.path().join("stdout.log")),
            ["first", "second", "thi"]
        );
    }

    #[test]
    fn writing_past_the_cap_keeps_one_rotated_file() {
        let dir = tempfile::tempdir().unwrap();
        let mut writer = LogWriter::open(dir.path(), LogStream::Stderr, 100);

        for index in 0..30 {
            writer.write(format!("line {index:02}\n").as_bytes());
        }

        let current = lines(&dir.path().join("stderr.log"));
        let rotated = lines(&dir.path().join("stderr.log.1"));
        assert!(fs::metadata(dir.path().join("stderr.log")).unwrap().len() <= 100);
        assert!(fs::metadata(dir.path().join("stderr.log.1")).unwrap().len() <= 100);
        assert!(!dir.path().join("stderr.log.2").exists());
        assert_eq!(current.last().unwrap(), "line 29");
        assert_eq!(
            rotated.last().unwrap(),
            &format!("line {:02}", 29 - current.len())
        );
    }

    #[test]
    fn unterminated_output_is_split_into_bounded_records() {
        let dir = tempfile::tempdir().unwrap();
        let mut writer = LogWriter::open(dir.path(), LogStream::Stdout, u64::MAX);

        writer.write(&vec![b'x'; MAX_RECORD_BYTES * 2 + 3]);
        writer.finish();

        let lengths = lines(&dir.path().join("stdout.log"))
            .iter()
            .map(String::len)
            .collect::<Vec<_>>();
        assert_eq!(lengths, [MAX_RECORD_BYTES, MAX_RECORD_BYTES, 3]);
    }

    #[test]
    fn a_newline_right_after_a_split_record_adds_no_empty_record() {
        let dir = tempfile::tempdir().unwrap();
        let mut writer = LogWriter::open(dir.path(), LogStream::Stdout, u64::MAX);

        writer.write(&vec![b'x'; MAX_RECORD_BYTES]);
        writer.write(b"\nnext\n\n");

        let lengths = lines(&dir.path().join("stdout.log"))
            .iter()
            .map(String::len)
            .collect::<Vec<_>>();
        assert_eq!(lengths, [MAX_RECORD_BYTES, 4, 0]);
    }

    #[test]
    fn a_rotation_failure_disables_the_stream_and_leaves_a_note() {
        let dir = tempfile::tempdir().unwrap();
        fs::create_dir_all(dir.path().join("stdout.log.1/occupied")).unwrap();
        let mut writer = LogWriter::open(dir.path(), LogStream::Stdout, 30);

        for index in 0..10 {
            writer.write(format!("line {index}\n").as_bytes());
        }
        writer.finish();

        let current = lines(&dir.path().join("stdout.log"));
        assert_eq!(current[0], "line 0");
        assert!(
            current
                .last()
                .unwrap()
                .starts_with("dotcore-launcher: stdout.log log disabled after a write failure:"),
            "{current:?}"
        );
        assert!(current.len() < 10);
    }

    fn lines(path: &Path) -> Vec<String> {
        fs::read(path)
            .unwrap()
            .split(|byte| *byte == b'\n')
            .filter(|record| !record.is_empty())
            .map(|record| LogRecord::decode(LogStream::Stdout, record).unwrap().line)
            .collect()
    }
}
