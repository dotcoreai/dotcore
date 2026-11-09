//! Reads one node's retained backend JSONL logs as bounded, normalized diagnostic events.
//!
//! History pages backward through the retained files and follow tails the active file; both
//! expose only validated retained files, bounded records with filesystem paths removed, and
//! opaque byte cursors that never let a caller select a file.

use std::{
    io::{BufRead, BufReader},
    path::{Path, PathBuf},
    pin::Pin,
    time::Duration,
};

use dotcore_proto::{
    dotcore::diagnostic::{
        private::v1::{
            FollowNodeDiagnosticsResponse, NodeDiagnosticHistory,
            follow_node_diagnostics_response::Frame,
        },
        v1::{
            BackendDiagnosticEvent, BackendDiagnosticField, DiagnosticHistoryState,
            DiagnosticSourceState,
        },
    },
    google::protobuf::Timestamp,
};
use futures_util::Stream;
use prost::Message;
use serde_json::Value;
use time::{OffsetDateTime, format_description::well_known::Rfc3339};
use tokio::{
    io::{AsyncReadExt, AsyncSeekExt},
    sync::mpsc,
};
use tokio_util::sync::CancellationToken;

use super::MAX_RESPONSE_BYTES;

const LOG_DIRECTORY: &str = "logs";
const MAX_EVENT_BYTES: usize = 64 * 1024;
const READ_CHUNK_BYTES: usize = 8 * 1024;
const MAX_HISTORY_SOURCE_BYTES: usize = 16 * 1024 * 1024;
const MAX_FIELDS: usize = 32;
const MAX_FIELD_KEY_BYTES: usize = 128;
const MAX_FIELD_VALUE_BYTES: usize = 1024;
/// Pause before a follow of the retained diagnostics file looks again for new lines.
/// Not a behavior duration: poll of a real log file for new lines.
const POLL_INTERVAL: Duration = Duration::from_millis(100);
const REDACTION: &str = "[REDACTED]";

/// Retained backend logs of one node, under its Site or node folder root.
#[derive(Clone)]
pub(super) struct RetainedLogs {
    logs: PathBuf,
}

impl RetainedLogs {
    /// Reads the retained-log directory of the node rooted at `root`.
    pub(super) fn new(root: &Path) -> Self {
        Self {
            logs: root.join(LOG_DIRECTORY),
        }
    }

    /// Reads one bounded page before an opaque position, outside the async executor.
    pub(super) async fn read(
        &self,
        page_size: usize,
        before: Option<&[u8]>,
    ) -> Result<NodeDiagnosticHistory, InvalidPosition> {
        let before = before.map(parse_position).transpose()?;
        let logs = self.logs.clone();
        Ok(
            tokio::task::spawn_blocking(move || read_history(&logs, page_size, before))
                .await
                .unwrap_or_else(|_| empty_history(DiagnosticSourceState::Unavailable)),
        )
    }

    /// Follows the active retained file after an opaque position until `cancellation` fires.
    pub(super) fn follow(
        &self,
        resume: Option<&[u8]>,
        cancellation: CancellationToken,
    ) -> Result<
        impl Stream<Item = FollowNodeDiagnosticsResponse> + Send + Unpin + use<>,
        InvalidPosition,
    > {
        let resume = resume.map(parse_position).transpose()?;
        Ok(follow(self.logs.clone(), resume, cancellation))
    }
}

/// An opaque position that this node did not issue or that names no retained file.
#[derive(Debug, thiserror::Error)]
#[error("diagnostic position is invalid")]
pub(super) struct InvalidPosition;

/// Reads newest retained files backward, preserving byte-cursor ordering across page boundaries.
/// Each call reads at most 16 MiB of source data; truncation continues with a source-only
/// `source:<file>:<offset>` cursor that resumes at the retained source byte boundary.
fn read_history(logs: &Path, page_size: usize, before: Option<Position>) -> NodeDiagnosticHistory {
    let Ok(files) = log_files(logs) else {
        return empty_history(DiagnosticSourceState::Unavailable);
    };
    if files.is_empty() {
        return empty_history(DiagnosticSourceState::Unavailable);
    }

    let mut source_state = DiagnosticSourceState::Available;
    let mut source_budget = MAX_HISTORY_SOURCE_BYTES;
    let mut events = Vec::with_capacity(page_size);
    let mut latest_position = before
        .as_ref()
        .filter(|position| !position.source_only)
        .map(position_bytes_from_position);
    let mut capped_position = None;
    let mut older_source_remains = false;
    let (start_index, start_offset) = history_start(&files, before.as_ref());

    for index in (0..=start_index).rev() {
        let path = &files[index];
        let end = if index == start_index {
            start_offset
                .unwrap_or_else(|| std::fs::metadata(path).map_or(0, |metadata| metadata.len()))
        } else {
            std::fs::metadata(path).map_or(0, |metadata| metadata.len())
        };
        if end == 0 {
            continue;
        }
        match read_history_page(
            path,
            end,
            &mut source_budget,
            &before,
            page_size - events.len(),
        ) {
            Ok(page) => {
                if page.truncated {
                    source_state = DiagnosticSourceState::Truncated;
                }
                if latest_position.is_none() {
                    latest_position = page.events.last().map(position_bytes);
                }
                events.extend(page.events);
                older_source_remains |= page.full;
                if page.capped {
                    capped_position = Some(source_position(&page.file, page.resume_offset));
                    older_source_remains = true;
                    source_state = DiagnosticSourceState::Truncated;
                    break;
                }
            }
            Err(()) => source_state = DiagnosticSourceState::Unavailable,
        }
    }

    events.sort_by_key(NormalizedEvent::position);
    if before.is_none() {
        latest_position = events.last().map(position_bytes);
    }
    let mut response_truncated = capped_position.is_some();
    while encoded_history_response_bytes(
        &events,
        &latest_position,
        source_state,
        response_truncated,
    ) > MAX_RESPONSE_BYTES
    {
        response_truncated = true;
        events.remove(0);
        older_source_remains = true;
    }
    let next_before_position = events.first().map(position_bytes).or(capped_position);
    let newest_inclusive_position = events
        .last()
        .map(|event| source_position(&event.file, event.offset));
    NodeDiagnosticHistory {
        events: events
            .into_iter()
            .map(NormalizedEvent::into_proto)
            .collect(),
        next_before_position,
        latest_position,
        history_state: if older_source_remains || response_truncated {
            DiagnosticHistoryState::OlderOutputMayRemain as i32
        } else {
            DiagnosticHistoryState::NoOlderOutputObserved as i32
        },
        source_state: source_state as i32,
        response_truncated,
        newest_inclusive_position,
    }
}

struct HistoryPage {
    events: Vec<NormalizedEvent>,
    file: String,
    resume_offset: u64,
    truncated: bool,
    capped: bool,
    full: bool,
}

/// Maps a history cursor to the file and byte window used for backward paging.
fn history_start(files: &[PathBuf], before: Option<&Position>) -> (usize, Option<u64>) {
    let Some(before) = before else {
        return (files.len() - 1, None);
    };
    let index = files
        .iter()
        .position(|path| {
            path.file_name()
                .is_some_and(|name| name == before.file.as_str())
        })
        .unwrap_or_else(|| {
            files.partition_point(|path| {
                path.file_name()
                    .is_some_and(|name| name < before.file.as_str())
            })
        });
    let index = index.min(files.len() - 1);
    let end = if before.source_only {
        before.offset
    } else {
        before.offset.saturating_sub(1)
    };
    (index, Some(end))
}

/// Parses a bounded suffix; partial or malformed JSONL records are reported as truncation.
fn read_history_page(
    path: &Path,
    end: u64,
    source_budget: &mut usize,
    before: &Option<Position>,
    event_capacity: usize,
) -> Result<HistoryPage, ()> {
    let name = path
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or(())?
        .to_owned();
    let bytes_to_read = usize::try_from(end)
        .unwrap_or(usize::MAX)
        .min(*source_budget);
    let start = end - bytes_to_read as u64;
    let mut file = std::fs::File::open(path).map_err(|_| ())?;
    use std::io::{Read, Seek, SeekFrom};
    file.seek(SeekFrom::Start(start)).map_err(|_| ())?;
    let mut reader = BufReader::with_capacity(READ_CHUNK_BYTES, file.take(bytes_to_read as u64));
    let mut events = Vec::with_capacity(event_capacity);
    let mut line = Vec::with_capacity(READ_CHUNK_BYTES);
    let mut offset = start;
    let mut oversized = false;
    let mut truncated = start > 0;
    let mut discard_initial = start > 0;
    let mut full = false;
    loop {
        let available = reader.fill_buf().map_err(|_| ())?;
        if available.is_empty() {
            break;
        }
        let consumed = available.len();
        for byte in available {
            offset += 1;
            if *byte == b'\n' {
                if discard_initial {
                    discard_initial = false;
                } else if oversized {
                    truncated = true;
                } else if let Ok(text) = std::str::from_utf8(&line) {
                    match normalize(text, name.clone(), offset) {
                        Some(event)
                            if before
                                .as_ref()
                                .is_none_or(|before| event_before(&event, before)) =>
                        {
                            if event_capacity > 0 {
                                if events.len() == event_capacity {
                                    events.remove(0);
                                    full = true;
                                }
                                events.push(event);
                            }
                        }
                        Some(_) => {}
                        None => truncated = true,
                    }
                } else {
                    truncated = true;
                }
                line.clear();
                oversized = false;
            } else if !oversized {
                if line.len() == MAX_EVENT_BYTES {
                    line.clear();
                    oversized = true;
                } else {
                    line.push(*byte);
                }
            }
        }
        reader.consume(consumed);
    }
    *source_budget -= bytes_to_read;
    // JSONL publishes records only once their newline has been written. A window ending before the
    // file end may likewise contain only a partial record.
    if oversized || !line.is_empty() {
        truncated = true;
    }
    let capped = bytes_to_read < usize::try_from(end).unwrap_or(usize::MAX);
    let full = full || (event_capacity > 0 && events.len() == event_capacity);
    Ok(HistoryPage {
        events,
        file: name,
        resume_offset: start,
        truncated,
        capped,
        full,
    })
}

fn event_before(event: &NormalizedEvent, before: &Position) -> bool {
    if before.source_only {
        event.file < before.file || (event.file == before.file && event.offset <= before.offset)
    } else {
        event.position() < *before
    }
}

fn empty_history(source_state: DiagnosticSourceState) -> NodeDiagnosticHistory {
    NodeDiagnosticHistory {
        events: Vec::new(),
        next_before_position: None,
        latest_position: None,
        history_state: DiagnosticHistoryState::NoOlderOutputObserved as i32,
        source_state: source_state as i32,
        response_truncated: false,
        newest_inclusive_position: None,
    }
}

/// Follows only the active retained file, resetting the byte cursor on identity or rotation changes.
fn follow(
    logs: PathBuf,
    resume: Option<Position>,
    cancellation: CancellationToken,
) -> impl Stream<Item = FollowNodeDiagnosticsResponse> + Send + Unpin + use<> {
    let (sender, receiver) = mpsc::channel(256);
    tokio::spawn(async move {
        let mut cursor: Option<(PathBuf, FileIdentity)> = None;
        let mut offset = 0;
        let mut partial = Vec::with_capacity(READ_CHUNK_BYTES);
        let mut oversized = false;
        let mut announced_available = false;
        loop {
            if cancellation.is_cancelled() {
                return;
            }
            let Some(path) = active_file(&logs) else {
                announced_available = false;
                send_state(&sender, DiagnosticSourceState::Unavailable, &cancellation).await;
                wait(&cancellation).await;
                continue;
            };
            let Ok(metadata) = tokio::fs::metadata(&path).await else {
                send_state(&sender, DiagnosticSourceState::Unavailable, &cancellation).await;
                wait(&cancellation).await;
                continue;
            };
            let identity = FileIdentity::from_metadata(&metadata);
            let replaced = cursor.as_ref().is_some_and(|(current, current_identity)| {
                current != &path || current_identity != &identity
            });
            if cursor.is_none() {
                offset = metadata.len();
                cursor = Some((path.clone(), identity));
            } else if replaced {
                cursor = Some((path.clone(), identity));
                offset = 0;
                partial.clear();
                oversized = false;
                if !send_state(
                    &sender,
                    DiagnosticSourceState::ContinuityUncertain,
                    &cancellation,
                )
                .await
                {
                    return;
                }
            }
            let Ok(mut file) = tokio::fs::File::open(&path).await else {
                send_state(&sender, DiagnosticSourceState::Unavailable, &cancellation).await;
                wait(&cancellation).await;
                continue;
            };
            if metadata.len() < offset {
                offset = 0;
                partial.clear();
                oversized = false;
                if !send_state(
                    &sender,
                    DiagnosticSourceState::ContinuityUncertain,
                    &cancellation,
                )
                .await
                {
                    return;
                }
            }
            if file.seek(std::io::SeekFrom::Start(offset)).await.is_err() {
                send_state(&sender, DiagnosticSourceState::Unavailable, &cancellation).await;
                wait(&cancellation).await;
                continue;
            }
            if !announced_available {
                announced_available = true;
                if !send_state(&sender, DiagnosticSourceState::Available, &cancellation).await {
                    return;
                }
            }
            let mut chunk = [0_u8; READ_CHUNK_BYTES];
            loop {
                let Ok(read) = file.read(&mut chunk).await else {
                    send_state(&sender, DiagnosticSourceState::Unavailable, &cancellation).await;
                    break;
                };
                if read == 0 {
                    break;
                }
                for byte in &chunk[..read] {
                    offset += 1;
                    if *byte == b'\n' {
                        if oversized {
                            oversized = false;
                            partial.clear();
                            if !send_state(&sender, DiagnosticSourceState::Truncated, &cancellation)
                                .await
                            {
                                return;
                            }
                            continue;
                        }
                        let line = std::str::from_utf8(&partial).ok();
                        let event = line.and_then(|line| {
                            normalize(
                                line,
                                path.file_name()
                                    .and_then(|name| name.to_str())
                                    .unwrap_or_default()
                                    .to_owned(),
                                offset,
                            )
                        });
                        partial.clear();
                        match event {
                            Some(event) if should_follow(&event, resume.as_ref()) => {
                                if !send_event(&sender, event.into_proto(), &cancellation).await {
                                    return;
                                }
                            }
                            Some(_) => {}
                            None => {
                                if !send_state(
                                    &sender,
                                    DiagnosticSourceState::Truncated,
                                    &cancellation,
                                )
                                .await
                                {
                                    return;
                                }
                            }
                        }
                    } else if !oversized {
                        if partial.len() == MAX_EVENT_BYTES {
                            partial.clear();
                            oversized = true;
                        } else {
                            partial.push(*byte);
                        }
                    }
                }
            }
            wait(&cancellation).await;
        }
    });
    tokio_stream::wrappers::ReceiverStream::new(receiver)
}

/// Applies resume suppression only within the same retained file; replacement is replayed explicitly.
fn should_follow(event: &NormalizedEvent, resume: Option<&Position>) -> bool {
    let Some(resume) = resume else {
        return true;
    };
    // Positions only suppress records from the same retained file at or before its byte cursor.
    // A rotation or replacement is intentionally replayed with explicit continuity uncertainty.
    event.file != resume.file || event.offset > resume.offset
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct FileIdentity(u64, u64);

impl FileIdentity {
    fn from_metadata(metadata: &std::fs::Metadata) -> Self {
        use std::os::unix::fs::MetadataExt;
        Self(metadata.dev(), metadata.ino())
    }
}
#[expect(
    clippy::disallowed_methods,
    reason = "polls a real log file for new lines"
)]
async fn wait(cancellation: &CancellationToken) {
    tokio::select! { () = cancellation.cancelled() => {}, () = tokio::time::sleep(POLL_INTERVAL) => {} }
}

async fn send_event(
    sender: &mpsc::Sender<FollowNodeDiagnosticsResponse>,
    diagnostic: BackendDiagnosticEvent,
    cancellation: &CancellationToken,
) -> bool {
    send_frame(sender, Frame::Diagnostic(diagnostic), cancellation).await
}

async fn send_state(
    sender: &mpsc::Sender<FollowNodeDiagnosticsResponse>,
    state: DiagnosticSourceState,
    cancellation: &CancellationToken,
) -> bool {
    send_frame(sender, Frame::SourceState(state as i32), cancellation).await
}

async fn send_frame(
    sender: &mpsc::Sender<FollowNodeDiagnosticsResponse>,
    frame: Frame,
    cancellation: &CancellationToken,
) -> bool {
    let response = FollowNodeDiagnosticsResponse { frame: Some(frame) };
    tokio::select! { () = cancellation.cancelled() => false, result = sender.send(response) => result.is_ok() }
}

/// Lists regular files whose exact names match the backend JSONL retention format.
fn log_files(directory: &Path) -> std::io::Result<Vec<PathBuf>> {
    let mut files = std::fs::read_dir(directory)?
        .filter_map(Result::ok)
        .filter(|entry| entry.file_type().is_ok_and(|kind| kind.is_file()))
        .filter(|entry| is_log_filename(&entry.file_name()))
        .map(|entry| entry.path())
        .collect::<Vec<_>>();
    files.sort();
    Ok(files)
}

fn active_file(directory: &Path) -> Option<PathBuf> {
    log_files(directory).ok()?.pop()
}

fn is_log_filename(name: &std::ffi::OsStr) -> bool {
    let Some(name) = name.to_str() else {
        return false;
    };
    name.len() == 24
        && name.starts_with("backend.")
        && name.ends_with(".jsonl")
        && name.as_bytes()[8..18]
            .iter()
            .enumerate()
            .all(|(index, byte)| {
                matches!(index, 4 | 7) && *byte == b'-'
                    || !matches!(index, 4 | 7) && byte.is_ascii_digit()
            })
}

#[derive(Clone, Eq, Ord, PartialEq, PartialOrd)]
struct Position {
    timestamp: i128,
    file: String,
    offset: u64,
    source_only: bool,
}

/// Parses the opaque timestamp/file/byte cursor and rejects paths outside the retention format.
fn parse_position(value: &[u8]) -> Result<Position, InvalidPosition> {
    let value = std::str::from_utf8(value).map_err(|_| InvalidPosition)?;
    let mut parts = value.splitn(3, ':');
    let (Some(timestamp), Some(file), Some(offset)) = (parts.next(), parts.next(), parts.next())
    else {
        return Err(InvalidPosition);
    };
    if !is_log_filename(std::ffi::OsStr::new(file)) {
        return Err(InvalidPosition);
    }
    Ok(Position {
        timestamp: if timestamp == "source" {
            0
        } else {
            timestamp.parse().map_err(|_| InvalidPosition)?
        },
        file: file.to_owned(),
        offset: offset.parse().map_err(|_| InvalidPosition)?,
        source_only: timestamp == "source",
    })
}

fn position_bytes(event: &NormalizedEvent) -> Vec<u8> {
    position_bytes_from_position(&event.position())
}

fn position_bytes_from_position(position: &Position) -> Vec<u8> {
    format!(
        "{}:{}:{}",
        position.timestamp, position.file, position.offset
    )
    .into_bytes()
}

fn source_position(file: &str, offset: u64) -> Vec<u8> {
    format!("source:{file}:{offset}").into_bytes()
}

struct NormalizedEvent {
    timestamp: i128,
    file: String,
    offset: u64,
    level: String,
    target: String,
    message: String,
    fields: Vec<BackendDiagnosticField>,
    truncated: bool,
}
impl NormalizedEvent {
    fn position(&self) -> Position {
        Position {
            timestamp: self.timestamp,
            file: self.file.clone(),
            offset: self.offset,
            source_only: false,
        }
    }
    fn into_proto(self) -> BackendDiagnosticEvent {
        let position = position_bytes(&self);
        BackendDiagnosticEvent {
            timestamp: Some(Timestamp {
                seconds: (self.timestamp / 1_000_000_000) as i64,
                nanos: (self.timestamp % 1_000_000_000) as i32,
            }),
            level: self.level,
            target: self.target,
            message: self.message,
            fields: self.fields,
            truncated: self.truncated,
            position,
        }
    }
}

/// Converts one JSONL record into a bounded event while retaining only filesystem-path redaction.
fn normalize(line: &str, file: String, offset: u64) -> Option<NormalizedEvent> {
    let value: Value = serde_json::from_str(line).ok()?;
    let timestamp = value.get("timestamp")?.as_str()?;
    let timestamp = OffsetDateTime::parse(timestamp, &Rfc3339)
        .ok()?
        .unix_timestamp_nanos();
    let mut truncated = false;
    let level = bounded(
        redact_filesystem_paths(value.get("level").and_then(Value::as_str).unwrap_or("INFO")),
        MAX_FIELD_KEY_BYTES,
        &mut truncated,
    );
    let target = bounded(
        redact_filesystem_paths(
            value
                .get("target")
                .and_then(Value::as_str)
                .unwrap_or("backend"),
        ),
        MAX_FIELD_VALUE_BYTES,
        &mut truncated,
    );
    let object = value.get("fields")?.as_object()?;
    let message = bounded(
        redact_filesystem_paths(object.get("message").and_then(Value::as_str).unwrap_or("")),
        MAX_EVENT_BYTES,
        &mut truncated,
    );
    let mut fields = Vec::new();
    for (key, value) in object
        .iter()
        .filter(|(key, _)| key.as_str() != "message")
        .take(MAX_FIELDS)
    {
        let key = bounded(
            redact_filesystem_paths(key),
            MAX_FIELD_KEY_BYTES,
            &mut truncated,
        );
        let value = bounded(
            redact_filesystem_paths(&value_to_string(value)),
            MAX_FIELD_VALUE_BYTES,
            &mut truncated,
        );
        fields.push(BackendDiagnosticField { key, value });
    }
    if object.len().saturating_sub(1) > MAX_FIELDS {
        truncated = true;
    }
    let mut event = NormalizedEvent {
        timestamp,
        file,
        offset,
        level,
        target,
        message,
        fields,
        truncated,
    };
    while event.encoded_len() > MAX_EVENT_BYTES {
        event.message.pop();
        event.truncated = true;
        if event.message.is_empty() {
            break;
        }
    }
    Some(event)
}

impl NormalizedEvent {
    fn encoded_len(&self) -> usize {
        self.clone_for_size().encoded_len()
    }
    fn clone_for_size(&self) -> BackendDiagnosticEvent {
        BackendDiagnosticEvent {
            timestamp: Some(Timestamp {
                seconds: (self.timestamp / 1_000_000_000) as i64,
                nanos: (self.timestamp % 1_000_000_000) as i32,
            }),
            level: self.level.clone(),
            target: self.target.clone(),
            message: self.message.clone(),
            fields: self.fields.clone(),
            truncated: self.truncated,
            position: position_bytes(self),
        }
    }
}

fn value_to_string(value: &Value) -> String {
    value
        .as_str()
        .map(str::to_owned)
        .unwrap_or_else(|| value.to_string())
}

/// Redacts absolute filesystem paths without treating URL-style `//` prefixes as paths.
pub(crate) fn redact_filesystem_paths(value: &str) -> String {
    let bytes = value.as_bytes();
    let mut output = String::with_capacity(value.len());
    let mut index = 0;
    while index < bytes.len() {
        let starts_path = bytes[index] == b'/'
            && bytes.get(index + 1).is_some_and(|next| *next != b'/')
            && (index == 0
                || !matches!(bytes[index - 1], b':' | b'/' | b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'_' | b'-'));
        if starts_path {
            let end = bytes[index..]
                .iter()
                .position(|byte| {
                    byte.is_ascii_whitespace()
                        || matches!(*byte, b'"' | b'\\' | b',' | b')' | b']' | b'}')
                })
                .map_or(bytes.len(), |length| index + length);
            output.push_str(REDACTION);
            index = end;
        } else {
            let character = value[index..].chars().next().expect("valid UTF-8");
            output.push(character);
            index += character.len_utf8();
        }
    }
    output
}

/// Truncates on a UTF-8 boundary and records truncation in the containing event.
fn bounded(mut value: String, limit: usize, truncated: &mut bool) -> String {
    if value.len() > limit {
        let mut end = limit;
        while !value.is_char_boundary(end) {
            end -= 1;
        }
        value.truncate(end);
        *truncated = true;
    }
    value
}
fn encoded_history_response_bytes(
    events: &[NormalizedEvent],
    latest_position: &Option<Vec<u8>>,
    source_state: DiagnosticSourceState,
    response_truncated: bool,
) -> usize {
    NodeDiagnosticHistory {
        events: events.iter().map(NormalizedEvent::clone_for_size).collect(),
        next_before_position: events.first().map(position_bytes),
        latest_position: latest_position.clone(),
        history_state: DiagnosticHistoryState::OlderOutputMayRemain as i32,
        source_state: source_state as i32,
        response_truncated,
        newest_inclusive_position: events
            .last()
            .map(|event| source_position(&event.file, event.offset)),
    }
    .encoded_len()
}

/// Cancels the producer when the consumer drops the stream.
pub(super) struct CancellableDiagnosticStream<S> {
    inner: S,
    cancellation: CancellationToken,
}
impl<S> CancellableDiagnosticStream<S> {
    pub(super) fn new(inner: S, cancellation: CancellationToken) -> Self {
        Self {
            inner,
            cancellation,
        }
    }
}
impl<S: Stream + Unpin> Stream for CancellableDiagnosticStream<S> {
    type Item = S::Item;
    fn poll_next(
        mut self: Pin<&mut Self>,
        context: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Option<Self::Item>> {
        Pin::new(&mut self.inner).poll_next(context)
    }
}
impl<S> Drop for CancellableDiagnosticStream<S> {
    fn drop(&mut self) {
        self.cancellation.cancel();
    }
}

#[cfg(test)]
mod tests {
    use std::{fs, path::PathBuf, time::Duration};

    use futures_util::StreamExt;
    use prost::Message;
    use tokio_util::sync::CancellationToken;

    use super::{
        CancellableDiagnosticStream, DiagnosticHistoryState, DiagnosticSourceState, Frame,
        MAX_EVENT_BYTES, MAX_FIELDS, MAX_HISTORY_SOURCE_BYTES, READ_CHUNK_BYTES, REDACTION, follow,
        read_history,
    };

    #[test]
    fn history_preserves_credential_and_endpoint_values_and_reports_malformed_input() {
        let directory = temporary_logs();
        fs::write(
            directory.join("backend.2026-09-14.jsonl"),
            format!(
                "{}\nnot-json\n",
                record(
                    "2026-09-14T01:00:00Z",
                    "credential=secret https://private.example.test/v1",
                    "token=secret",
                    1,
                )
            ),
        )
        .expect("first log");
        fs::write(
            directory.join("backend.2026-09-15.jsonl"),
            format!("{}\n", record("2026-09-15T01:00:00Z", "second", "safe", 2)),
        )
        .expect("second log");

        let newest = read_history(&directory, 1, None);
        assert_eq!(newest.events.len(), 1);
        assert_eq!(newest.events[0].message, "second safe");
        assert_eq!(
            newest.history_state,
            DiagnosticHistoryState::OlderOutputMayRemain as i32
        );
        assert_eq!(newest.source_state, DiagnosticSourceState::Truncated as i32);
        let page = read_history(
            &directory,
            200,
            newest
                .next_before_position
                .as_deref()
                .map(super::parse_position)
                .transpose()
                .expect("position"),
        );
        assert_eq!(page.events.len(), 1);
        assert_eq!(
            page.events[0].message,
            "credential=secret https://private.example.test/v1 token=secret"
        );
        fs::remove_dir_all(directory.parent().expect("test root")).expect("cleanup");
    }

    #[test]
    fn normalization_enforces_record_and_field_limits() {
        let directory = temporary_logs();
        let fields = (0..(MAX_FIELDS + 1))
            .map(|index| format!("\"field-{index}\":\"{}\"", "x".repeat(1024)))
            .collect::<Vec<_>>()
            .join(",");
        fs::write(directory.join("backend.2026-09-15.jsonl"), format!("{{\"timestamp\":\"2026-09-15T01:00:00Z\",\"level\":\"INFO\",\"target\":\"backend\",\"fields\":{{\"message\":\"{}\",{fields}}}}}\n", "m".repeat(8 * 1024))).expect("log");
        let response = read_history(&directory, 200, None);
        let event = response.events.into_iter().next().expect("event");
        assert!(event.truncated);
        assert_eq!(event.fields.len(), MAX_FIELDS);
        assert!(event.message.len() <= MAX_EVENT_BYTES);
        assert!(event.fields.iter().all(|field| field.value.len() <= 1024));
        fs::remove_dir_all(directory.parent().expect("test root")).expect("cleanup");
    }

    #[test]
    fn unavailable_directory_has_no_filesystem_detail() {
        let missing = dotcore_domain::test_support::canonical_temp_dir()
            .join(format!("dotcore-diagnostic-missing-{}", unique()));
        let response = read_history(&missing, 200, None);
        assert_eq!(
            response.source_state,
            DiagnosticSourceState::Unavailable as i32
        );
        assert!(response.events.is_empty());
    }

    #[tokio::test]
    async fn follow_reports_a_missing_directory_unavailable_and_recovers() {
        let root = dotcore_domain::test_support::canonical_temp_dir()
            .join(format!("dotcore-diagnostic-follow-{}", unique()));
        let logs = root.join("logs");
        let cancellation = CancellationToken::new();
        let mut stream = CancellableDiagnosticStream::new(
            follow(logs.clone(), None, cancellation.clone()),
            cancellation,
        );
        let event = stream.next().await.expect("unavailable event");
        assert_eq!(
            event.frame,
            Some(Frame::SourceState(
                DiagnosticSourceState::Unavailable as i32
            ))
        );

        fs::create_dir_all(&logs).expect("log directory");
        fs::write(
            logs.join("backend.2026-09-15.jsonl"),
            format!(
                "{}\n",
                record("2026-09-15T01:00:00Z", "recovered", "safe", 1)
            ),
        )
        .expect("recovered log");
        let recovered = tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                let event = stream.next().await.expect("follow event");
                if event.frame == Some(Frame::SourceState(DiagnosticSourceState::Available as i32))
                {
                    return;
                }
            }
        })
        .await;
        assert!(recovered.is_ok(), "follow recovers once the file exists");

        drop(stream);
        fs::remove_dir_all(root).expect("cleanup");
    }

    #[tokio::test]
    async fn replacement_replays_initial_content_with_credential_and_endpoint_values() {
        let directory = temporary_logs();
        let active = directory.join("backend.2026-09-15.jsonl");
        fs::write(&active, "").expect("active log");
        let cancellation = CancellationToken::new();
        let mut stream = CancellableDiagnosticStream::new(
            follow(directory.clone(), None, cancellation.clone()),
            cancellation,
        );
        let available = stream.next().await.expect("available");
        assert!(
            available.frame == Some(Frame::SourceState(DiagnosticSourceState::Available as i32))
        );

        let replacement = directory.join("replacement.jsonl");
        fs::write(
            &replacement,
            format!(
                "{}\n",
                record(
                    "2026-09-15T01:00:00Z",
                    "replacement https://private.example.test/v1",
                    "credential=new-secret",
                    1,
                )
            ),
        )
        .expect("replacement");
        fs::rename(&replacement, &active).expect("replace active file");

        let continuity = tokio::time::timeout(Duration::from_secs(2), stream.next())
            .await
            .expect("continuity timeout")
            .expect("continuity");
        assert!(
            continuity.frame
                == Some(Frame::SourceState(
                    DiagnosticSourceState::ContinuityUncertain as i32
                ))
        );
        let event = tokio::time::timeout(Duration::from_secs(2), stream.next())
            .await
            .expect("event timeout")
            .expect("event");
        let Some(Frame::Diagnostic(event)) = event.frame else {
            panic!("expected replacement event");
        };
        assert_eq!(
            event.message,
            "replacement https://private.example.test/v1 credential=new-secret"
        );
        drop(stream);
        fs::remove_dir_all(directory.parent().expect("test root")).expect("cleanup");
    }

    #[test]
    fn history_redacts_filesystem_paths_and_rejects_unterminated_records() {
        let directory = temporary_logs();
        let complete = record(
            "2026-09-15T01:00:00Z",
            "path=/private/site/state.pb",
            "safe",
            1,
        );
        let incomplete = record("2026-09-15T02:00:00Z", "must-not-appear", "safe", 2);
        fs::write(
            directory.join("backend.2026-09-15.jsonl"),
            format!("{complete}\n{incomplete}"),
        )
        .expect("log");

        let response = read_history(&directory, 200, None);
        assert_eq!(response.events.len(), 1);
        assert!(response.events[0].message.contains(REDACTION));
        assert!(
            !response.events[0]
                .message
                .contains("/private/site/state.pb")
        );
        assert_eq!(
            response.source_state,
            DiagnosticSourceState::Truncated as i32
        );
        fs::remove_dir_all(directory.parent().expect("test root")).expect("cleanup");
    }

    #[test]
    fn history_response_including_metadata_is_never_larger_than_limit() {
        let directory = temporary_logs();
        let line = record("2026-09-15T01:00:00Z", &"m".repeat(60 * 1024), "safe", 1);
        fs::write(
            directory.join("backend.2026-09-15.jsonl"),
            std::iter::repeat_n(format!("{line}\n"), 100).collect::<String>(),
        )
        .expect("log");

        let response = read_history(&directory, 200, None);
        assert!(response.response_truncated);
        assert!(response.encoded_len() <= super::MAX_RESPONSE_BYTES);
        fs::remove_dir_all(directory.parent().expect("test root")).expect("cleanup");
    }

    #[test]
    fn history_pages_large_valid_retained_input_without_scanning_every_file() {
        let directory = temporary_logs();
        let line = format!("{}\n", record("2026-09-15T01:00:00Z", "valid", "safe", 1));
        fs::write(
            directory.join("backend.2026-09-15.jsonl"),
            std::iter::repeat_n(line, 200_000).collect::<String>(),
        )
        .expect("large valid log");

        let response = read_history(&directory, 200, None);
        assert_eq!(response.events.len(), 200);
        assert_eq!(response.events[0].message, "valid safe");
        assert!(response.response_truncated);
        assert_eq!(
            response.source_state,
            DiagnosticSourceState::Truncated as i32
        );
        assert_eq!(
            response.history_state,
            DiagnosticHistoryState::OlderOutputMayRemain as i32
        );
        assert!(response.next_before_position.is_some());
        fs::remove_dir_all(directory.parent().expect("test root")).expect("cleanup");
    }

    #[test]
    fn history_source_continuation_includes_record_ending_at_window_boundary() {
        let directory = temporary_logs();
        let boundary_record = record("2026-09-15T01:00:00Z", "boundary", "safe", 1);
        let prefix = "x".repeat(MAX_HISTORY_SOURCE_BYTES - boundary_record.len() - 2);
        let path = directory.join("backend.2026-09-15.jsonl");
        fs::write(
            &path,
            format!(
                "{prefix}\n{boundary_record}\n{}",
                "x".repeat(MAX_HISTORY_SOURCE_BYTES)
            ),
        )
        .expect("log");

        let newest = read_history(&directory, 200, None);
        assert!(newest.events.is_empty());
        let position = newest.next_before_position.expect("older position");
        assert_eq!(
            position,
            format!("source:backend.2026-09-15.jsonl:{MAX_HISTORY_SOURCE_BYTES}").into_bytes()
        );

        let older = read_history(
            &directory,
            200,
            Some(super::parse_position(&position).expect("older position")),
        );
        assert_eq!(older.events.len(), 1);
        assert_eq!(older.events[0].message, "boundary safe");
        assert!(older.next_before_position.is_some());
        fs::remove_dir_all(directory.parent().expect("test root")).expect("cleanup");
    }

    #[test]
    fn history_source_cap_truncates_large_malformed_input_with_an_older_position() {
        let directory = temporary_logs();
        fs::write(
            directory.join("backend.2026-09-15.jsonl"),
            "x".repeat(MAX_HISTORY_SOURCE_BYTES + READ_CHUNK_BYTES),
        )
        .expect("large malformed log");

        let response = read_history(&directory, 200, None);
        assert!(response.events.is_empty());
        assert!(response.response_truncated);
        assert_eq!(
            response.source_state,
            DiagnosticSourceState::Truncated as i32
        );
        assert_eq!(
            response.history_state,
            DiagnosticHistoryState::OlderOutputMayRemain as i32
        );
        let position = response.next_before_position.expect("older position");
        assert!(
            std::str::from_utf8(&position)
                .expect("position")
                .starts_with("source:")
        );
        let continued = read_history(
            &directory,
            200,
            Some(super::parse_position(&position).expect("older position")),
        );
        assert!(continued.events.is_empty());
        assert_eq!(
            continued.history_state,
            DiagnosticHistoryState::NoOlderOutputObserved as i32
        );
        fs::remove_dir_all(directory.parent().expect("test root")).expect("cleanup");
    }

    #[test]
    fn same_timestamp_positions_use_file_and_byte_cursor_for_follow() {
        let first = super::normalize(
            &record("2026-09-15T01:00:00Z", "first", "safe", 1),
            "backend.2026-09-15.jsonl".to_owned(),
            100,
        )
        .expect("event");
        let later = super::normalize(
            &record("2026-09-15T01:00:00Z", "later", "safe", 2),
            "backend.2026-09-15.jsonl".to_owned(),
            200,
        )
        .expect("event");
        let other_file = super::normalize(
            &record("2026-09-15T01:00:00Z", "rotated", "safe", 3),
            "backend.2026-09-16.jsonl".to_owned(),
            10,
        )
        .expect("event");

        assert!(!super::should_follow(&first, Some(&first.position())));
        assert!(super::should_follow(&later, Some(&first.position())));
        assert!(super::should_follow(&other_file, Some(&first.position())));
    }

    fn record(timestamp: &str, message: &str, value: &str, order: u8) -> String {
        format!(
            "{{\"timestamp\":\"{timestamp}\",\"level\":\"INFO\",\"target\":\"test\",\"fields\":{{\"message\":\"{message} {value}\",\"order\":{order}}}}}"
        )
    }

    fn temporary_logs() -> PathBuf {
        let root = dotcore_domain::test_support::canonical_temp_dir()
            .join(format!("dotcore-diagnostic-{}", unique()));
        let logs = root.join("logs");
        fs::create_dir_all(&logs).expect("log directory");
        logs
    }

    fn unique() -> String {
        dotcore_domain::test_support::unique_suffix()
    }
}
