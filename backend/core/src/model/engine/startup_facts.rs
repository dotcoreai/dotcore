//! Extracts engine-reported memory facts, such as vLLM's KV cache token
//! capacity, from a workload's startup output for workload resource telemetry.
//!
//! Runtime log history returns a bounded page of the newest entries, and
//! request logging quickly pushes startup lines out of that page. The reader
//! therefore walks backwards page by page until every fact is found, the
//! workload's first output is reached, or a fixed page budget is spent.
//! Engines without an extractor read no logs and report no facts.

use std::{future::Future, num::NonZeroU16, time::Duration};

use dotcore_proto::dotcore::telemetry::v1::workload_resource_observation::EngineFacts;
use dotcore_runtime::{MAX_RUNTIME_LOG_ENTRIES, RuntimeLogHistory, RuntimeLogQuery};

/// Maximum history pages read per extraction, bounding the cost of long-running workloads.
pub(crate) const MAX_STARTUP_LOG_PAGES: usize = 20;

/// Parses one engine's startup output into typed facts.
pub(crate) trait StartupFactExtractor: Send + Sync {
    /// Extracts facts from startup output in chronological order, one entry per line.
    fn extract(&self, output: &str) -> ExtractedStartupFacts;
}

/// Facts found so far, and the names of the facts the extractor knows but did not find.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct ExtractedStartupFacts {
    pub(crate) facts: EngineFacts,
    pub(crate) missing: Vec<&'static str>,
}

impl ExtractedStartupFacts {
    /// True when every fact the extractor knows was found.
    pub(crate) fn complete(&self) -> bool {
        self.missing.is_empty()
    }
}

/// Outcome of one startup facts read: the facts found and why the read stopped.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct StartupFactsRead {
    pub(crate) extracted: ExtractedStartupFacts,
    pub(crate) end: StartupFactsReadEnd,
    /// True when a second holding more than one page of output was left partly unread, because
    /// runtime log bounds are whole seconds.
    pub(crate) skipped_crowded_second: bool,
}

/// Why a startup facts read stopped.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum StartupFactsReadEnd {
    Complete,
    FirstOutputReached,
    PageBudgetSpent,
}

/// Reads log history backwards through `read_page` until the extractor reports complete facts.
/// Returns none when the workload has no output.
pub(crate) async fn read_startup_facts<F, Fut, E>(
    extractor: &dyn StartupFactExtractor,
    mut read_page: F,
) -> Result<Option<StartupFactsRead>, E>
where
    F: FnMut(RuntimeLogQuery) -> Fut,
    Fut: Future<Output = Result<RuntimeLogHistory, E>>,
{
    let page_size = NonZeroU16::new(MAX_RUNTIME_LOG_ENTRIES).expect("page size is non-zero");
    let mut pages: Vec<String> = Vec::new();
    let mut before = None;
    let mut read: Option<StartupFactsRead> = None;
    let mut skipped_crowded_second = false;
    for _ in 0..MAX_STARTUP_LOG_PAGES {
        let query =
            RuntimeLogQuery::new(page_size, before).expect("page size is within the runtime limit");
        let history = read_page(query).await?;
        let Some(oldest) = history.entries.first().map(|entry| entry.timestamp) else {
            if let Some(read) = read.as_mut() {
                read.end = StartupFactsReadEnd::FirstOutputReached;
            }
            break;
        };
        pages.push(
            history
                .entries
                .iter()
                .map(|entry| {
                    String::from_utf8_lossy(&entry.content)
                        .trim_end()
                        .to_owned()
                })
                .collect::<Vec<_>>()
                .join("\n"),
        );
        let output = pages
            .iter()
            .rev()
            .map(String::as_str)
            .collect::<Vec<_>>()
            .join("\n");
        let extracted = extractor.extract(&output);
        let end = if extracted.complete() {
            StartupFactsReadEnd::Complete
        } else if !history.has_older_output {
            StartupFactsReadEnd::FirstOutputReached
        } else {
            StartupFactsReadEnd::PageBudgetSpent
        };
        read = Some(StartupFactsRead {
            extracted,
            end,
            skipped_crowded_second,
        });
        if end != StartupFactsReadEnd::PageBudgetSpent {
            break;
        }
        // Runtime bounds are inclusive whole seconds, so the next page overlaps the oldest entry's
        // second to avoid skipping its earlier lines; step back a second when no progress is made.
        let next = truncate_to_second(oldest) + Duration::from_secs(1);
        before = Some(match before {
            Some(previous) if next >= previous => {
                skipped_crowded_second = true;
                previous - Duration::from_secs(1)
            }
            _ => next,
        });
    }
    Ok(read)
}

fn truncate_to_second(time: std::time::SystemTime) -> std::time::SystemTime {
    let since_epoch = time
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default();
    std::time::UNIX_EPOCH + Duration::from_secs(since_epoch.as_secs())
}

#[cfg(test)]
mod tests {
    use std::{
        convert::Infallible,
        sync::Mutex,
        time::{SystemTime, UNIX_EPOCH},
    };

    use dotcore_proto::dotcore::telemetry::v1::VllmStartupFacts;
    use dotcore_runtime::{RuntimeLogEntry, RuntimeLogSource, RuntimeLogStreamKind};

    use super::*;

    struct MarkerExtractor;

    impl StartupFactExtractor for MarkerExtractor {
        fn extract(&self, output: &str) -> ExtractedStartupFacts {
            let first = output.contains("first-fact");
            let second = output.contains("second-fact");
            ExtractedStartupFacts {
                facts: facts(first, second),
                missing: [(!first).then_some("first"), (!second).then_some("second")]
                    .into_iter()
                    .flatten()
                    .collect(),
            }
        }
    }

    /// Fake runtime history with evenly spaced entries, answering the newest page at or before an
    /// inclusive bound, as Docker does.
    struct FakeHistory {
        lines: Vec<String>,
        spacing: Duration,
        queries: Mutex<Vec<Option<SystemTime>>>,
    }

    impl FakeHistory {
        fn timestamp(&self, index: usize) -> SystemTime {
            UNIX_EPOCH + Duration::from_secs(1_000) + self.spacing * index as u32
        }

        fn page(&self, query: &RuntimeLogQuery) -> RuntimeLogHistory {
            self.queries
                .lock()
                .expect("queries")
                .push(query.before_timestamp());
            let eligible = self
                .lines
                .iter()
                .enumerate()
                .filter(|(index, _)| {
                    query
                        .before_timestamp()
                        .is_none_or(|before| self.timestamp(*index) <= before)
                })
                .collect::<Vec<_>>();
            let limit = usize::from(query.maximum_entries().get());
            let start = eligible.len().saturating_sub(limit);
            RuntimeLogHistory {
                entries: eligible[start..]
                    .iter()
                    .map(|(index, line)| RuntimeLogEntry {
                        source: RuntimeLogSource {
                            name: "workload".to_owned(),
                            label: "workload".to_owned(),
                        },
                        timestamp: self.timestamp(*index),
                        stream: RuntimeLogStreamKind::Stdout,
                        content: format!("{line}\n").into_bytes(),
                        truncated: false,
                    })
                    .collect(),
                has_older_output: start > 0,
                candidate_budget_truncated: false,
            }
        }
    }

    fn history(lines: Vec<String>) -> FakeHistory {
        spaced_history(lines, Duration::from_secs(1))
    }

    fn spaced_history(lines: Vec<String>, spacing: Duration) -> FakeHistory {
        FakeHistory {
            lines,
            spacing,
            queries: Mutex::new(Vec::new()),
        }
    }

    fn facts(first: bool, second: bool) -> EngineFacts {
        EngineFacts::Vllm(VllmStartupFacts {
            kv_cache_token_capacity: first.then_some(1),
            model_loading_bytes: second.then_some(2),
            ..VllmStartupFacts::default()
        })
    }

    fn outcome(
        first: bool,
        second: bool,
        end: StartupFactsReadEnd,
        skipped_crowded_second: bool,
    ) -> Option<StartupFactsRead> {
        Some(StartupFactsRead {
            extracted: MarkerExtractor.extract(
                &[
                    first.then_some("first-fact"),
                    second.then_some("second-fact"),
                ]
                .into_iter()
                .flatten()
                .collect::<Vec<_>>()
                .join("\n"),
            ),
            end,
            skipped_crowded_second,
        })
    }

    async fn read(history: &FakeHistory) -> Option<StartupFactsRead> {
        read_startup_facts(&MarkerExtractor, |query| {
            let page = history.page(&query);
            async move { Ok::<_, Infallible>(page) }
        })
        .await
        .expect("infallible read")
    }

    #[tokio::test]
    async fn pages_back_past_request_logs_to_find_every_fact() {
        let mut lines = vec!["first-fact".to_owned(), "second-fact".to_owned()];
        lines.extend((0..450).map(|index| format!("request {index}")));
        let history = history(lines);

        assert_eq!(
            read(&history).await,
            outcome(true, true, StartupFactsReadEnd::Complete, false)
        );
        assert_eq!(history.queries.lock().expect("queries").len(), 3);
    }

    #[tokio::test]
    async fn overlaps_page_boundaries_within_one_second() {
        let mut lines = vec!["noise".to_owned(); 3];
        lines.extend(["first-fact".to_owned(), "second-fact".to_owned()]);
        lines.extend((0..200).map(|index| format!("request {index}")));
        let history = spaced_history(lines, Duration::from_millis(100));

        assert_eq!(
            read(&history).await,
            outcome(true, true, StartupFactsReadEnd::Complete, false)
        );
    }

    #[tokio::test]
    async fn stops_after_the_first_page_when_facts_are_complete() {
        let history = history(vec![
            "noise".to_owned(),
            "first-fact".to_owned(),
            "second-fact".to_owned(),
        ]);

        read(&history).await;

        assert_eq!(*history.queries.lock().expect("queries"), vec![None]);
    }

    #[tokio::test]
    async fn stops_at_the_page_budget_with_partial_facts() {
        let mut lines = vec!["first-fact".to_owned()];
        lines.extend((0..5_000).map(|index| format!("request {index}")));
        let history = history(lines);

        let read = read(&history).await;

        assert_eq!(
            history.queries.lock().expect("queries").len(),
            MAX_STARTUP_LOG_PAGES
        );
        assert_eq!(
            read,
            outcome(false, false, StartupFactsReadEnd::PageBudgetSpent, false)
        );
    }

    #[tokio::test]
    async fn reaching_the_first_output_reports_the_missing_facts() {
        let history = history(vec!["noise".to_owned(), "second-fact".to_owned()]);

        let read = read(&history).await;

        assert_eq!(
            read,
            outcome(false, true, StartupFactsReadEnd::FirstOutputReached, false)
        );
        assert_eq!(read.expect("read").extracted.missing, vec!["first"]);
    }

    #[tokio::test]
    async fn a_second_holding_more_than_one_page_is_reported_as_skipped() {
        // All lines share one second: whole-second bounds reach only the line at its exact start,
        // so the second fact, inside the crowded second but before the newest page, is lost.
        let mut lines = vec!["first-fact".to_owned(), "second-fact".to_owned()];
        lines.extend((0..300).map(|index| format!("request {index}")));
        let history = spaced_history(lines, Duration::from_millis(1));

        assert_eq!(
            read(&history).await,
            outcome(true, false, StartupFactsReadEnd::FirstOutputReached, true)
        );
    }

    #[tokio::test]
    async fn empty_history_reports_no_read() {
        assert_eq!(read(&history(Vec::new())).await, None);
    }

    #[test]
    fn truncates_to_whole_seconds() {
        assert_eq!(
            truncate_to_second(UNIX_EPOCH + Duration::from_millis(5_900)),
            UNIX_EPOCH + Duration::from_secs(5)
        );
    }
}
