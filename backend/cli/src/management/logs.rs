//! Reads and follows the logs of one deployment through the manager's public logs API, the same
//! one the web console uses, so it works for a deployment on any node of the site.
//!
//! Log lines go to stdout and nothing else does, so they can be piped. Source availability,
//! truncation notices and the end of a follow go to stderr. A read whose every source is
//! unavailable fails, so an empty result always has a reason; a follow keeps going while sources
//! come and go, and ends when the deployment is removed.

use std::{
    io::{self, IsTerminal, Write},
    pin::Pin,
};

use anyhow::Context;
use clap::Args;
use dotcore_proto::google::protobuf::Timestamp;
use futures_util::{Stream, StreamExt};
use time::{OffsetDateTime, format_description::well_known::Rfc3339};
use tonic::{Code, Status};

use super::{
    ManagedDeployment, ManagementError, Manager, ManagerArgs, call_failed, find_deployment,
};
use crate::terminal::PromptError;

/// Most recent lines one read returns, the manager's page limit.
const MAX_LINES: u32 = 200;

/// Arguments of a deployment's logs command.
#[derive(Debug, Args)]
pub(crate) struct LogsArgs {
    /// Name or id of the deployment.
    #[arg(value_name = "DEPLOYMENT")]
    reference: String,

    /// Number of most recent lines to print, at most 200.
    #[arg(
        short = 'n',
        long,
        value_name = "COUNT",
        default_value_t = 100,
        value_parser = clap::value_parser!(u32).range(1..=i64::from(MAX_LINES))
    )]
    lines: u32,

    /// Keep printing new lines until interrupted or the deployment is removed.
    #[arg(short, long)]
    follow: bool,

    /// Print only this log source; repeat for several. Without it, every source is printed.
    #[arg(long = "source", value_name = "SOURCE")]
    sources: Vec<String>,

    /// Start each line with its timestamp.
    #[arg(short, long)]
    timestamps: bool,

    #[command(flatten)]
    manager: ManagerArgs,
}

/// Prints the referenced deployment's most recent lines, then follows new ones when asked.
pub(crate) async fn deployment_logs<T: LoggedDeployment>(
    arguments: LogsArgs,
) -> anyhow::Result<()> {
    let manager = arguments.manager.connect().await?;
    let deployment = find_deployment::<T>(&manager, &arguments.reference).await?;
    let subject = format!("{} {}", T::GROUP.noun(), deployment.name());
    let id = deployment.id().to_owned();

    let page = T::read_logs(
        &manager,
        id.clone(),
        arguments.sources.clone(),
        arguments.lines,
    )
    .await
    .map_err(|status| logs_failed(T::GROUP, &subject, &arguments.sources, status))?;
    let mut output = LogOutput::new(&arguments, page.sources.len() > 1);
    output
        .print(|output| output.page(&page))
        .context("cannot write the logs to standard output")?;

    // The reader of the lines is gone, such as `head` having read enough: nothing left to do.
    if output.closed {
        return Ok(());
    }
    if !arguments.follow {
        let unavailable = !page.sources.is_empty()
            && page
                .sources
                .iter()
                .all(|source| source.state == LogSourceState::Unavailable);
        if unavailable {
            anyhow::bail!(
                "every log source of {subject} is unavailable; its node may be unreachable"
            );
        }
        return Ok(());
    }

    let events = T::follow_logs(&manager, id, arguments.sources.clone(), page.positions)
        .await
        .map_err(|status| logs_failed(T::GROUP, &subject, &arguments.sources, status))?;
    tokio::select! {
        followed = output.follow(events) => {
            match followed {
                Ok(()) if output.closed => Ok(()),
                Ok(()) => {
                    notice(format_args!("the log follow of {subject} ended"));
                    Ok(())
                }
                Err(FollowFailure::Manager(status)) => {
                    Err(logs_failed(T::GROUP, &subject, &arguments.sources, status))
                }
                Err(FollowFailure::Output(error)) => Err(
                    anyhow::Error::new(error).context("cannot write the logs to standard output"),
                ),
            }
        }
        _ = tokio::signal::ctrl_c() => Err(anyhow::Error::new(PromptError::Cancelled)),
    }
}

/// A deployment kind whose logs the manager serves. Each kind's calls use its own messages, which
/// the implementations convert to the shared shapes below.
pub(crate) trait LoggedDeployment: ManagedDeployment {
    /// Reads the most recent `page_size` lines of the deployment's selected sources.
    async fn read_logs(
        manager: &Manager,
        id: String,
        sources: Vec<String>,
        page_size: u32,
    ) -> Result<LogPage, Status>;

    /// Follows the deployment's selected sources from the given positions.
    async fn follow_logs(
        manager: &Manager,
        id: String,
        sources: Vec<String>,
        positions: Vec<LogPosition>,
    ) -> Result<LogEvents, Status>;
}

/// The events of a log follow, in arrival order.
pub(crate) type LogEvents = Pin<Box<dyn Stream<Item = Result<LogEvent, Status>> + Send>>;

/// One read of a deployment's most recent lines.
#[derive(Debug)]
pub(crate) struct LogPage {
    /// The selected sources and their availability at the time of the read.
    pub(crate) sources: Vec<LogSource>,

    /// The lines, oldest first.
    pub(crate) entries: Vec<LogEntry>,

    /// Whether the manager's response size limit dropped older lines of the page.
    pub(crate) truncated: bool,

    /// Where each source's lines stop, so a follow continues after them.
    pub(crate) positions: Vec<LogPosition>,
}

/// One log source of a deployment, such as one of its containers.
#[derive(Debug)]
pub(crate) struct LogSource {
    /// The stable name that selects the source.
    pub(crate) name: String,

    /// The name shown to people.
    pub(crate) label: String,

    pub(crate) state: LogSourceState,
}

/// The availability of a log source.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum LogSourceState {
    Available,

    Unavailable,

    /// The source ended during a follow, for example because its workload stopped.
    Ended,

    /// The manager sent no state.
    Unknown,
}

/// One line of a log source.
#[derive(Debug)]
pub(crate) struct LogEntry {
    pub(crate) source_name: String,
    pub(crate) source_label: String,
    pub(crate) timestamp: Option<Timestamp>,
    pub(crate) content: Vec<u8>,

    /// Whether the manager cut the line at its length limit.
    pub(crate) truncated: bool,
}

/// The timestamp of the latest line already printed for one source.
#[derive(Debug)]
pub(crate) struct LogPosition {
    pub(crate) source_name: String,
    pub(crate) timestamp: Option<Timestamp>,
}

/// One event of a log follow.
#[derive(Debug)]
pub(crate) enum LogEvent {
    /// A source became available or unavailable, or ended.
    Source(LogSource),

    /// A new line.
    Entry(LogEntry),
}

/// Why a follow stopped early.
#[derive(Debug)]
enum FollowFailure {
    /// The manager failed the follow.
    Manager(Status),

    /// A line could not be written locally.
    Output(io::Error),
}

/// Where log lines and notices are printed, and how lines are shaped.
struct LogOutput {
    stdout: io::Stdout,
    labeled: bool,
    timestamps: bool,
    sanitized: bool,
    closed: bool,
}

impl LogOutput {
    fn new(arguments: &LogsArgs, labeled: bool) -> Self {
        let stdout = io::stdout();
        // A terminal would act on control sequences in a line; a pipe receives the line as is.
        let sanitized = stdout.is_terminal();
        Self {
            stdout,
            labeled,
            timestamps: arguments.timestamps,
            sanitized,
            closed: false,
        }
    }

    /// Runs `print`, treating a closed stdout, such as a pipe into `head`, as the reader being
    /// done rather than as a failure.
    fn print(&mut self, print: impl FnOnce(&mut Self) -> io::Result<()>) -> io::Result<()> {
        if self.closed {
            return Ok(());
        }
        match print(self) {
            Err(error) if error.kind() == io::ErrorKind::BrokenPipe => {
                self.closed = true;
                Ok(())
            }
            printed => printed,
        }
    }

    fn page(&mut self, page: &LogPage) -> io::Result<()> {
        for entry in &page.entries {
            self.entry(entry)?;
        }
        for source in &page.sources {
            if source.state != LogSourceState::Available {
                source_notice(source);
            }
        }
        if page.truncated {
            notice(format_args!(
                "older lines were left out to keep the response within its size limit"
            ));
        }
        Ok(())
    }

    /// Prints events until the follow ends, returning the failure that ended it, if any.
    async fn follow(&mut self, mut events: LogEvents) -> Result<(), FollowFailure> {
        while let Some(event) = events.next().await {
            match event.map_err(FollowFailure::Manager)? {
                LogEvent::Source(source) => source_notice(&source),
                LogEvent::Entry(entry) => {
                    self.print(|output| output.entry(&entry))
                        .map_err(FollowFailure::Output)?;
                    if self.closed {
                        return Ok(());
                    }
                }
            }
        }
        Ok(())
    }

    fn entry(&mut self, entry: &LogEntry) -> io::Result<()> {
        let mut line = String::new();
        if self.timestamps {
            line.push_str(&format_timestamp(entry.timestamp.as_ref()));
            line.push(' ');
        }
        if self.labeled {
            line.push_str(&format!(
                "[{}] ",
                source_display(&entry.source_name, &entry.source_label)
            ));
        }
        let content = String::from_utf8_lossy(&entry.content);
        if self.sanitized {
            line.push_str(&sanitize(&content));
        } else {
            line.push_str(&content);
        }
        if entry.truncated {
            line.push_str(" [truncated]");
        }
        let mut stdout = self.stdout.lock();
        writeln!(stdout, "{line}")?;
        stdout.flush()
    }
}

/// Reports a source that is not available on stderr, for example `Web service (web): unavailable`.
fn source_notice(source: &LogSource) {
    let state = match source.state {
        LogSourceState::Available => "available",
        LogSourceState::Unavailable => "unavailable",
        LogSourceState::Ended => "ended",
        LogSourceState::Unknown => "in an unknown state",
    };
    notice(format_args!(
        "{}: {state}",
        source_display(&source.name, &source.label)
    ));
}

/// Shows a source by its label, followed by its name when they differ, since the name is what
/// selects the source.
fn source_display(name: &str, label: &str) -> String {
    if name == label {
        label.to_owned()
    } else {
        format!("{label} ({name})")
    }
}

/// Writes a notice on stderr. A closed stderr loses the notice rather than failing the command.
fn notice(message: std::fmt::Arguments<'_>) {
    let _ = writeln!(io::stderr().lock(), "{message}");
}

/// Maps a failed logs call to the error shown. When the manager refused the selected sources, it
/// says how to see the deployment's sources instead of pointing at the deployment list.
fn logs_failed(
    group: super::DeploymentGroup,
    subject: &str,
    sources: &[String],
    status: Status,
) -> anyhow::Error {
    let refused_sources = status.code() == Code::InvalidArgument && !sources.is_empty();
    let error: ManagementError = call_failed(group, format!("read the logs of {subject}"))(status);
    if refused_sources {
        return anyhow::anyhow!(
            "{error}; run the command without --source to see the source of each line"
        );
    }
    anyhow::Error::new(error)
}

/// Formats a line's timestamp as RFC 3339 in UTC, or `-` when the line has none.
fn format_timestamp(timestamp: Option<&Timestamp>) -> String {
    timestamp
        .and_then(|timestamp| {
            let nanos = i128::from(timestamp.seconds) * 1_000_000_000 + i128::from(timestamp.nanos);
            OffsetDateTime::from_unix_timestamp_nanos(nanos).ok()
        })
        .and_then(|time| time.format(&Rfc3339).ok())
        .unwrap_or_else(|| "-".to_owned())
}

/// Removes terminal escape sequences and replaces other control characters, except tabs, so a
/// line cannot move the cursor or change the terminal's presentation.
fn sanitize(content: &str) -> String {
    let mut sanitized = String::with_capacity(content.len());
    let mut characters = content.chars().peekable();
    while let Some(character) = characters.next() {
        if character == '\u{1b}' && characters.peek() == Some(&'[') {
            characters.next();
            // A control sequence ends at its first final byte.
            for next in characters.by_ref() {
                if ('@'..='~').contains(&next) {
                    break;
                }
            }
        } else if character.is_control() && character != '\t' {
            sanitized.push('\u{fffd}');
        } else {
            sanitized.push(character);
        }
    }
    sanitized
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sanitizing_drops_escape_sequences_and_replaces_other_controls() {
        assert_eq!(
            sanitize("\u{1b}[31mred\u{1b}[0m\tdone\u{7}\r"),
            "red\tdone\u{fffd}\u{fffd}"
        );
        assert_eq!(sanitize("plain é line"), "plain é line");
    }

    #[test]
    fn a_source_shows_its_name_only_when_it_differs_from_its_label() {
        assert_eq!(source_display("web", "Web service"), "Web service (web)");
        assert_eq!(source_display("web", "web"), "web");
    }

    #[test]
    fn timestamps_format_in_utc_and_a_missing_one_is_a_dash() {
        let timestamp = Timestamp {
            seconds: 1_794_500_000,
            nanos: 120_000_000,
        };

        assert_eq!(
            format_timestamp(Some(&timestamp)),
            "2026-11-12T16:13:20.12Z"
        );
        assert_eq!(format_timestamp(None), "-");
    }
}
