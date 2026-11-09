//! Measures the public inference requests the site manager routes, per inference route and
//! request kind: how many end in each outcome, how many are in progress, total duration, and
//! fallbacks to a later group. Time to first token is per route only, since only streamed chat
//! has a first token. Each dispatch of a request to a model deployment is also measured as an
//! attempt of that deployment, with its token usage and its decode and prefill speeds; a
//! decision's input tokens count as prompt tokens.
//!
//! Measuring never waits or changes a response: samples go to a non-blocking metric recorder and
//! are observed at completion on the manager wall clock. These metrics are lossy operational
//! telemetry, never usage accounting.

use std::{
    collections::BTreeMap,
    sync::{Arc, Mutex},
    time::Duration,
};

use dotcore_domain::{
    background_loop::log_unexpected_loop_end,
    clock::{Clock, next_boundary},
};
use dotcore_observability::{
    MetricDescriptor, MetricDimension, MetricKind, MetricProvenance, MetricRecord, MetricRecorder,
    MetricUnit,
    labels::{self, LabelSet},
};
use dotcore_proto::dotcore::catalog::v1::InferenceRequestKind;
use dotcore_proto::dotcore::inference::v1::{
    ChatStreamEvent, DecisionUsage, TokenUsage, chat_stream_event::Event, chat_text_delta,
};
use tokio::{task::JoinHandle, time::Instant};

use super::selection::SelectedInferenceTarget;
use crate::route_manager::RouteManager;

/// Requests that ended, by route and outcome; rejections before a route is known carry no route.
pub(crate) const REQUESTS: &str = "router.requests";

/// Admitted requests in progress per route, sampled every second.
pub(crate) const REQUEST_IN_FLIGHT: &str = "router.request.in_flight";

/// Time from the start of route handling, once the request is read, to the first text, reasoning
/// or tool-call delta of its stream, including any failed attempt before a fallback.
pub(crate) const REQUEST_TTFT: &str = "router.request.ttft";

/// Time from the start of route handling, once the request is read, until its response ends, by
/// route and outcome.
pub(crate) const REQUEST_DURATION: &str = "router.request.duration";

/// Fallbacks to a later group of the route after a target refused before dispatch.
pub(crate) const REQUEST_FALLBACKS: &str = "router.request.fallbacks";

/// Attempts that ended, by deployment and outcome; an unsuccessful one names its failure stage.
pub(crate) const ATTEMPTS: &str = "router.attempts";

/// Time from an attempt's dispatch to its first text, reasoning or tool-call delta, excluding
/// earlier attempts of the request.
pub(crate) const ATTEMPT_TTFT: &str = "router.attempt.ttft";

/// Time from an attempt's dispatch until it answers, or until its stream ends.
pub(crate) const ATTEMPT_DURATION: &str = "router.attempt.duration";

/// Generation speed of a streamed attempt: completion tokens after the first, divided by the time
/// between its first and last content deltas.
pub(crate) const ATTEMPT_DECODE_RATE: &str = "router.attempt.decode_rate";

/// Effective prompt processing speed of a streamed attempt: prompt tokens not served from the
/// prompt cache, divided by the attempt's time to first token, engine queueing included.
pub(crate) const ATTEMPT_PREFILL_RATE: &str = "router.attempt.prefill_rate";

/// Prompt tokens of attempts, as their usage reports.
pub(crate) const TOKENS_PROMPT: &str = "router.tokens.prompt";

/// Generated completion tokens of attempts, as their usage reports.
pub(crate) const TOKENS_COMPLETION: &str = "router.tokens.completion";

/// Prompt tokens served from the engine's prompt cache, for attempts whose engine reports them.
pub(crate) const TOKENS_CACHED_PROMPT: &str = "router.tokens.cached_prompt";

/// Successful attempts whose response reported no token usage.
pub(crate) const USAGE_MISSING: &str = "router.usage.missing";

/// Request metering shared by every clone of one router; its in-flight sampler stops when the
/// last clone is dropped.
#[derive(Clone)]
pub(crate) struct RouterMetrics {
    shared: Arc<Shared>,
    _sampler: Arc<SamplerTask>,
}

impl RouterMetrics {
    /// Starts metering into `recorder` on `clock`, sampling in-flight requests for every route
    /// `routes` declares, idle ones included, so an idle route reads zero rather than absent.
    /// Must run inside a Tokio runtime.
    pub(crate) fn start(
        recorder: Arc<dyn MetricRecorder>,
        clock: Arc<dyn Clock>,
        routes: Option<RouteManager>,
    ) -> Self {
        let shared = Arc::new(Shared {
            recorder,
            clock,
            in_flight: Mutex::default(),
        });
        let sampler = tokio::spawn(log_unexpected_loop_end(
            "router_in_flight_sampler",
            sample_in_flight(Arc::clone(&shared), routes),
        ));
        Self {
            shared,
            _sampler: Arc::new(SamplerTask(sampler)),
        }
    }

    /// A meter for one request of `kind` whose route handling starts now.
    pub(crate) fn meter(&self, kind: InferenceRequestKind) -> RequestMeter {
        RequestMeter {
            started: self.shared.clock.now(),
            shared: Some(Arc::clone(&self.shared)),
            kind,
            route: None,
            admitted: false,
            first_content: None,
            fallbacks: 0,
            attempt: None,
            finished: false,
        }
    }
}

/// Measures one request from the start of its route handling, and each of its attempts; dropping
/// it unfinished records the request and its open attempt as cancelled, so every path that
/// abandons a request releases its in-flight count.
pub(crate) struct RequestMeter {
    shared: Option<Arc<Shared>>,
    started: Instant,
    kind: InferenceRequestKind,
    route: Option<String>,
    admitted: bool,
    first_content: Option<Instant>,
    fallbacks: u64,
    attempt: Option<AttemptMeter>,
    finished: bool,
}

impl RequestMeter {
    /// A meter that records nothing, for a router composed without metrics.
    pub(crate) fn unmetered(kind: InferenceRequestKind) -> Self {
        Self {
            shared: None,
            started: Instant::now(),
            kind,
            route: None,
            admitted: false,
            first_content: None,
            fallbacks: 0,
            attempt: None,
            finished: false,
        }
    }

    /// Labels the request with the inference route it resolved to.
    pub(crate) fn resolved(&mut self, route_id: &str) {
        self.route = Some(route_id.to_owned());
    }

    /// Counts the request in progress on its resolved route until it finishes.
    pub(crate) fn admitted(&mut self) {
        let (Some(shared), Some(route)) = (&self.shared, &self.route) else {
            return;
        };
        if !self.admitted {
            self.admitted = true;
            *shared
                .in_flight()
                .entry((route.clone(), self.kind))
                .or_default() += 1;
        }
    }

    /// Counts one fallback to a later group of the route.
    pub(crate) fn fallback(&mut self) {
        self.fallbacks += 1;
    }

    /// Starts measuring an attempt dispatched now to `target`, on the resolved route.
    pub(crate) fn start_attempt(&mut self, target: &SelectedInferenceTarget) {
        let (Some(shared), Some(route)) = (&self.shared, &self.route) else {
            return;
        };
        let labels = LabelSet::new()
            .with(labels::ROUTE, route.clone())
            .with(REQUEST_KIND, kind_label(self.kind))
            .with(labels::DEPLOYMENT, target.deployment_id());
        let labels = match target {
            SelectedInferenceTarget::Peer(target) => labels
                .with(labels::MODEL, target.model_id.0.clone())
                .with(labels::NODE, target.site_node_id.clone())
                .with(TARGET_KIND, "local"),
            SelectedInferenceTarget::Upstream(_) => labels.with(TARGET_KIND, "upstream"),
        };
        self.attempt = Some(AttemptMeter {
            labels,
            usage_labels: LabelSet::new()
                .with(labels::ROUTE, route.clone())
                .with(REQUEST_KIND, kind_label(self.kind))
                .with(labels::DEPLOYMENT, target.deployment_id()),
            started: shared.clock.now(),
            first_content: None,
            last_content: None,
            usage: None,
        });
    }

    /// Notes content delta times and token usage among the stream events handed to the client
    /// writer, before any filtering.
    pub(crate) fn observe(&mut self, event: &ChatStreamEvent) {
        let Some(shared) = &self.shared else {
            return;
        };
        if let Some(Event::Usage(usage)) = &event.event {
            self.observe_usage(usage);
        } else if is_content(event) {
            let now = shared.clock.now();
            self.first_content.get_or_insert(now);
            if let Some(attempt) = &mut self.attempt {
                attempt.first_content.get_or_insert(now);
                attempt.last_content = Some(now);
            }
        }
    }

    /// Notes the token usage the current attempt reported.
    pub(crate) fn observe_usage(&mut self, usage: &TokenUsage) {
        if let Some(attempt) = &mut self.attempt {
            attempt.usage = Some(*usage);
        }
    }

    /// Notes the usage of the current decision attempt, whose input tokens are its prompt and
    /// which generates no completion tokens.
    pub(crate) fn observe_decision_usage(&mut self, usage: &DecisionUsage) {
        self.observe_usage(&TokenUsage {
            prompt_tokens: usage.input_tokens,
            completion_tokens: 0,
            total_tokens: usage.input_tokens,
            cached_prompt_tokens: None,
            reasoning_tokens: None,
        });
    }

    /// Records the current attempt's end with `outcome` and, when unsuccessful, the failure
    /// `stage`.
    pub(crate) fn end_attempt(&mut self, outcome: RequestOutcome, stage: Option<&'static str>) {
        if let (Some(shared), Some(attempt)) = (&self.shared, self.attempt.take()) {
            shared
                .recorder
                .record(attempt.records(&*shared.clock, outcome, stage));
        }
    }

    /// Records the request's end with `outcome`.
    pub(crate) fn finish(mut self, outcome: RequestOutcome) {
        self.record_end(outcome);
    }

    /// Records the request's end; an attempt still open ends with the request's outcome.
    fn record_end(&mut self, outcome: RequestOutcome) {
        if self.finished {
            return;
        }
        self.finished = true;
        self.end_attempt(outcome, None);
        let Some(shared) = &self.shared else {
            return;
        };
        let ended = shared.clock.now();
        let observed_at = shared.clock.wall_now();
        let route = self
            .route
            .as_ref()
            .map(|route| LabelSet::new().with(labels::ROUTE, route.clone()));
        let kind = kind_label(self.kind);

        let mut records = vec![MetricRecord::counter(
            REQUESTS,
            route
                .clone()
                .unwrap_or_default()
                .with(REQUEST_KIND, kind)
                .with(OUTCOME, outcome.as_str()),
            observed_at,
            1.0,
        )];
        if let (true, Some(route)) = (self.admitted, route) {
            records.push(MetricRecord::distribution(
                REQUEST_DURATION,
                route
                    .clone()
                    .with(REQUEST_KIND, kind)
                    .with(OUTCOME, outcome.as_str()),
                observed_at,
                ended.saturating_duration_since(self.started).as_secs_f64(),
            ));
            if let Some(first_content) = self.first_content {
                records.push(MetricRecord::distribution(
                    REQUEST_TTFT,
                    route.clone(),
                    observed_at,
                    first_content
                        .saturating_duration_since(self.started)
                        .as_secs_f64(),
                ));
            }
            if self.fallbacks > 0 {
                records.push(MetricRecord::counter(
                    REQUEST_FALLBACKS,
                    route.with(REQUEST_KIND, kind),
                    observed_at,
                    self.fallbacks as f64,
                ));
            }
            shared.release(self.route.as_deref().unwrap_or_default(), self.kind);
        }
        shared.recorder.record(records);
    }
}

impl Drop for RequestMeter {
    fn drop(&mut self) {
        self.record_end(RequestOutcome::Cancelled);
    }
}

/// How a request or an attempt ended, the `outcome` label of request and attempt metrics.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum RequestOutcome {
    /// A successful response or a complete stream.
    Success,

    /// An error response, a failed stream, or exhausted fallbacks.
    Failed,

    /// The client disconnected, the request timed out, or its route was revoked.
    Cancelled,

    /// Refused at admission: an invalid request, an unknown route, or no eligible target.
    Rejected,
}

impl RequestOutcome {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::Success => "success",
            Self::Failed => "failed",
            Self::Cancelled => "cancelled",
            Self::Rejected => "rejected",
        }
    }
}

/// Measures one dispatch to a model deployment, from the time it was dispatched.
struct AttemptMeter {
    /// Route, deployment, and target labels shared by the attempt's metrics.
    labels: LabelSet,

    /// Route and deployment labels of a missing-usage count.
    usage_labels: LabelSet,
    started: Instant,
    first_content: Option<Instant>,
    last_content: Option<Instant>,
    usage: Option<TokenUsage>,
}

impl AttemptMeter {
    /// The records of the attempt ending now with `outcome`: its count and duration, its time to
    /// first token when it streamed content, and its tokens and speeds when it reported usage. A
    /// successful attempt without usage counts as missing usage instead.
    fn records(
        self,
        clock: &dyn Clock,
        outcome: RequestOutcome,
        stage: Option<&'static str>,
    ) -> Vec<MetricRecord> {
        let ended = clock.now();
        let observed_at = clock.wall_now();
        let counted = self.labels.clone().with(OUTCOME, outcome.as_str());
        let mut records = vec![
            MetricRecord::counter(
                ATTEMPTS,
                match stage {
                    Some(stage) => counted.clone().with(STAGE, stage),
                    None => counted.clone(),
                },
                observed_at,
                1.0,
            ),
            MetricRecord::distribution(
                ATTEMPT_DURATION,
                counted,
                observed_at,
                ended.saturating_duration_since(self.started).as_secs_f64(),
            ),
        ];
        let ttft = self
            .first_content
            .map(|first| first.saturating_duration_since(self.started));
        if let Some(ttft) = ttft {
            records.push(MetricRecord::distribution(
                ATTEMPT_TTFT,
                self.labels.clone(),
                observed_at,
                ttft.as_secs_f64(),
            ));
        }

        let Some(usage) = self.usage else {
            if outcome == RequestOutcome::Success {
                records.push(MetricRecord::counter(
                    USAGE_MISSING,
                    self.usage_labels,
                    observed_at,
                    1.0,
                ));
            }
            return records;
        };
        let tokens = |metric, count: u64| {
            MetricRecord::counter(metric, self.labels.clone(), observed_at, count as f64)
        };
        records.push(tokens(TOKENS_PROMPT, usage.prompt_tokens));
        records.push(tokens(TOKENS_COMPLETION, usage.completion_tokens));
        if let Some(cached) = usage.cached_prompt_tokens {
            records.push(tokens(TOKENS_CACHED_PROMPT, cached));
        }
        let speed = |metric, count: u64, elapsed: Duration| {
            (count > 0 && !elapsed.is_zero()).then(|| {
                MetricRecord::distribution(
                    metric,
                    self.labels.clone(),
                    observed_at,
                    count as f64 / elapsed.as_secs_f64(),
                )
            })
        };
        // The first token arrives at the end of prefill, so decoding covers the tokens after it.
        if let (Some(first), Some(last)) = (self.first_content, self.last_content) {
            records.extend(speed(
                ATTEMPT_DECODE_RATE,
                usage.completion_tokens.saturating_sub(1),
                last.saturating_duration_since(first),
            ));
        }
        if let Some(ttft) = ttft {
            records.extend(speed(
                ATTEMPT_PREFILL_RATE,
                usage
                    .prompt_tokens
                    .saturating_sub(usage.cached_prompt_tokens.unwrap_or(0)),
                ttft,
            ));
        }
        records
    }
}

/// Dimension naming how a request or attempt ended.
const OUTCOME: &str = "outcome";

/// Dimension naming the stage an attempt failed in.
const STAGE: &str = "stage";

/// Dimension naming the inference request kind of a request or attempt, such as `chat`.
const REQUEST_KIND: &str = "request_kind";

/// Dimension telling whether an attempt targeted a local deployment or an upstream endpoint.
const TARGET_KIND: &str = "target_kind";

/// Interval between in-flight samples.
/// Not a behavior duration: the one-second metric bucket contract.
const IN_FLIGHT_SAMPLE_INTERVAL: Duration = Duration::from_secs(1);

/// Histogram scale of latency distributions: percentiles within about 4.4%.
const LATENCY_SCALE: i8 = 3;

/// Histogram scale of token speed distributions: percentiles within about 9%.
const SPEED_SCALE: i8 = 2;

/// Declarations of the request and attempt metrics the router records.
pub(crate) fn router_metric_descriptors() -> Vec<MetricDescriptor> {
    let descriptor = |name: &str, kind, unit, dimensions: Vec<MetricDimension>| MetricDescriptor {
        name: name.to_owned(),
        kind,
        unit,
        dimensions,
        provenance: MetricProvenance::Manager,
        sample_interval: matches!(kind, MetricKind::Gauge).then_some(IN_FLIGHT_SAMPLE_INTERVAL),
        reporting_delay: None,
    };
    let route = || MetricDimension::required(labels::ROUTE);
    let outcome = || MetricDimension::required(OUTCOME);
    let kind = || MetricDimension::required(REQUEST_KIND);
    let latency = MetricKind::Distribution {
        scale: LATENCY_SCALE,
    };
    let speed = MetricKind::Distribution { scale: SPEED_SCALE };
    // Upstream targets have no hosting node or catalog model.
    let attempt = |extra: &[MetricDimension]| {
        let mut dimensions = vec![
            route(),
            kind(),
            MetricDimension::required(labels::DEPLOYMENT),
            MetricDimension::optional(labels::MODEL),
            MetricDimension::optional(labels::NODE),
            MetricDimension::required(TARGET_KIND),
        ];
        dimensions.extend_from_slice(extra);
        dimensions
    };
    vec![
        descriptor(
            REQUESTS,
            MetricKind::Counter,
            MetricUnit::Requests,
            vec![MetricDimension::optional(labels::ROUTE), kind(), outcome()],
        ),
        descriptor(
            REQUEST_IN_FLIGHT,
            MetricKind::Gauge,
            MetricUnit::Requests,
            vec![route(), kind()],
        ),
        descriptor(REQUEST_TTFT, latency, MetricUnit::Seconds, vec![route()]),
        descriptor(
            REQUEST_DURATION,
            latency,
            MetricUnit::Seconds,
            vec![route(), kind(), outcome()],
        ),
        descriptor(
            REQUEST_FALLBACKS,
            MetricKind::Counter,
            MetricUnit::Requests,
            vec![route(), kind()],
        ),
        descriptor(
            ATTEMPTS,
            MetricKind::Counter,
            MetricUnit::Requests,
            attempt(&[outcome(), MetricDimension::optional(STAGE)]),
        ),
        descriptor(ATTEMPT_TTFT, latency, MetricUnit::Seconds, attempt(&[])),
        descriptor(
            ATTEMPT_DURATION,
            latency,
            MetricUnit::Seconds,
            attempt(&[outcome()]),
        ),
        descriptor(
            ATTEMPT_DECODE_RATE,
            speed,
            MetricUnit::TokensPerSecond,
            attempt(&[]),
        ),
        descriptor(
            ATTEMPT_PREFILL_RATE,
            speed,
            MetricUnit::TokensPerSecond,
            attempt(&[]),
        ),
        descriptor(
            TOKENS_PROMPT,
            MetricKind::Counter,
            MetricUnit::Tokens,
            attempt(&[]),
        ),
        descriptor(
            TOKENS_COMPLETION,
            MetricKind::Counter,
            MetricUnit::Tokens,
            attempt(&[]),
        ),
        descriptor(
            TOKENS_CACHED_PROMPT,
            MetricKind::Counter,
            MetricUnit::Tokens,
            attempt(&[]),
        ),
        descriptor(
            USAGE_MISSING,
            MetricKind::Counter,
            MetricUnit::Requests,
            vec![
                route(),
                kind(),
                MetricDimension::required(labels::DEPLOYMENT),
            ],
        ),
    ]
}

/// The `request_kind` label value of an inference request kind.
fn kind_label(kind: InferenceRequestKind) -> &'static str {
    match kind {
        InferenceRequestKind::Chat => "chat",
        InferenceRequestKind::Decision => "decision",
        InferenceRequestKind::Unspecified => "unspecified",
    }
}

struct Shared {
    recorder: Arc<dyn MetricRecorder>,
    clock: Arc<dyn Clock>,

    /// Admitted requests in progress by route id and request kind; an entry leaves the map when
    /// it reaches zero.
    in_flight: Mutex<BTreeMap<(String, InferenceRequestKind), u64>>,
}

impl Shared {
    fn in_flight(
        &self,
    ) -> std::sync::MutexGuard<'_, BTreeMap<(String, InferenceRequestKind), u64>> {
        self.in_flight
            .lock()
            .unwrap_or_else(|value| value.into_inner())
    }

    fn release(&self, route: &str, kind: InferenceRequestKind) {
        let mut in_flight = self.in_flight();
        let key = (route.to_owned(), kind);
        if let Some(count) = in_flight.get_mut(&key) {
            *count -= 1;
            if *count == 0 {
                in_flight.remove(&key);
            }
        }
    }
}

/// Aborts the in-flight sampler when dropped.
struct SamplerTask(JoinHandle<()>);

impl Drop for SamplerTask {
    fn drop(&mut self) {
        self.0.abort();
    }
}

/// Records every declared route's in-flight count for each request kind its members declare,
/// at each wall-clock multiple of the interval, plus any deleted route or kind still serving
/// requests. Waiting for the next multiple, rather than one interval after the last sample,
/// keeps samples on the same second boundaries as every other writer.
async fn sample_in_flight(shared: Arc<Shared>, routes: Option<RouteManager>) {
    loop {
        let wall = shared.clock.wall_now();
        let boundary = next_boundary(wall, IN_FLIGHT_SAMPLE_INTERVAL);
        shared
            .clock
            .sleep_until(shared.clock.now() + boundary.duration_since(wall).unwrap_or_default())
            .await;
        // Counts read after the sample time, so a sample never predates the counts it holds.
        let observed_at = shared.clock.wall_now();
        let mut counts = BTreeMap::new();
        if let Some(routes) = &routes {
            let snapshot = routes.snapshot();
            for route in snapshot.routes() {
                for kind in snapshot.request_kinds(&route.inference_route_id) {
                    counts.insert((route.inference_route_id.clone(), kind), 0);
                }
            }
        }
        counts.extend(shared.in_flight().clone());
        shared.recorder.record(
            counts
                .into_iter()
                .map(|((route, kind), count)| {
                    MetricRecord::gauge(
                        REQUEST_IN_FLIGHT,
                        LabelSet::new()
                            .with(labels::ROUTE, route)
                            .with(REQUEST_KIND, kind_label(kind)),
                        observed_at,
                        count as f64,
                    )
                })
                .collect(),
        );
    }
}

/// Whether `event` carries generated content: non-empty text or reasoning, or a tool call.
fn is_content(event: &ChatStreamEvent) -> bool {
    match &event.event {
        Some(Event::TextDelta(delta)) => matches!(
            &delta.content,
            Some(chat_text_delta::Content::TextContent(text)) if !text.is_empty()
        ),
        Some(Event::ReasoningDelta(delta)) => !delta.reasoning.is_empty(),
        Some(Event::ToolCallDelta(_)) => true,
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use dotcore_domain::clock::{ManualClock, VirtualClock};
    use dotcore_observability::{RecordValue, test_support::CapturingRecorder};
    use dotcore_proto::dotcore::inference::v1::{
        ChatReasoningDelta, ChatStreamStarted, ChatTextDelta, ChatToolCallDelta,
    };

    use super::*;
    use crate::model::controller::serving_targets::node_target_for_test;

    #[test]
    fn router_metrics_are_valid_declarations() {
        for descriptor in router_metric_descriptors() {
            assert_eq!(descriptor.validate(), Ok(()), "{}", descriptor.name);
        }
    }

    #[tokio::test]
    async fn in_flight_samples_land_on_wall_clock_seconds() {
        let start = std::time::UNIX_EPOCH + Duration::from_secs(1_700_000_000);
        // Started 0.4s into a second, the sampler still samples on whole seconds.
        let clock = VirtualClock::starting_at(start + Duration::from_millis(400));
        let recorder = Arc::new(CapturingRecorder::default());
        let metrics = RouterMetrics::start(
            Arc::clone(&recorder) as Arc<dyn MetricRecorder>,
            Arc::new(clock.clone()),
            None,
        );
        let mut meter = metrics.meter(InferenceRequestKind::Chat);
        meter.resolved("route-id");
        meter.admitted();

        for second in 1..=3 {
            clock.advance_to_next_wake().await;
            let batch = recorder.next_batch().await;
            assert_eq!(
                batch
                    .iter()
                    .map(|record| (record.metric.as_str(), record.observed_at))
                    .collect::<Vec<_>>(),
                vec![(REQUEST_IN_FLIGHT, start + Duration::from_secs(second))],
                "sample {second}"
            );
        }
    }

    #[tokio::test]
    async fn decision_samples_carry_their_kind_and_count_input_tokens_as_prompt_tokens() {
        let recorder = Arc::new(CapturingRecorder::default());
        let metrics = RouterMetrics::start(
            Arc::clone(&recorder) as Arc<dyn MetricRecorder>,
            Arc::new(ManualClock::new()),
            None,
        );
        let mut meter = metrics.meter(InferenceRequestKind::Decision);
        meter.resolved("route-id");
        meter.admitted();
        meter.start_attempt(&SelectedInferenceTarget::Peer(node_target_for_test(
            "deployment",
            &"b".repeat(64),
        )));
        meter.observe_decision_usage(&DecisionUsage { input_tokens: 42 });
        meter.end_attempt(RequestOutcome::Success, None);
        meter.finish(RequestOutcome::Success);

        let request = [("route", "route-id"), ("request_kind", "decision")];
        let attempt = [
            ("route", "route-id"),
            ("request_kind", "decision"),
            ("deployment", "deployment"),
            ("model", "fixture-model"),
            ("node", "worker-one"),
            ("target_kind", "local"),
        ];
        let sample = |metric: &str, labels: &[(&str, &str)], outcome: bool, value| {
            let mut labels = labels
                .iter()
                .map(|(name, value)| ((*name).to_owned(), (*value).to_owned()))
                .collect::<BTreeMap<_, _>>();
            if outcome {
                labels.insert("outcome".to_owned(), "success".to_owned());
            }
            (metric.to_owned(), labels, value)
        };
        // Durations follow the real clock, so only their series are asserted.
        assert_eq!(
            samples(&recorder),
            vec![
                sample(ATTEMPTS, &attempt, true, Some(1.0)),
                sample(ATTEMPT_DURATION, &attempt, true, None),
                sample(TOKENS_PROMPT, &attempt, false, Some(42.0)),
                sample(TOKENS_COMPLETION, &attempt, false, Some(0.0)),
                sample(REQUESTS, &request, true, Some(1.0)),
                sample(REQUEST_DURATION, &request, true, None),
            ]
        );
    }

    #[test]
    fn only_generated_content_starts_the_first_token() {
        let text = |content: &str| {
            Event::TextDelta(ChatTextDelta {
                content: Some(chat_text_delta::Content::TextContent(content.to_owned())),
                ..Default::default()
            })
        };
        let reasoning = |reasoning: &str| {
            Event::ReasoningDelta(ChatReasoningDelta {
                choice_index: 0,
                reasoning: reasoning.to_owned(),
            })
        };
        let cases = [
            (Event::Started(ChatStreamStarted::default()), false),
            (text(""), false),
            (Event::TextDelta(ChatTextDelta::default()), false),
            (reasoning(""), false),
            (text("Hi"), true),
            (reasoning("Thinking"), true),
            (Event::ToolCallDelta(ChatToolCallDelta::default()), true),
        ];

        for (event, content) in cases {
            assert_eq!(
                is_content(&ChatStreamEvent {
                    event: Some(event.clone())
                }),
                content,
                "{event:?}"
            );
        }
    }

    /// One recorded sample: its metric, labels, and the value of a counter.
    type Sample = (String, BTreeMap<String, String>, Option<f64>);

    /// Every sample `recorder` kept except in-flight samples, whose timing the sampler owns.
    fn samples(recorder: &CapturingRecorder) -> Vec<Sample> {
        recorder
            .records()
            .iter()
            .filter(|record| record.metric != REQUEST_IN_FLIGHT)
            .map(|record| {
                let value = match record.value {
                    RecordValue::Counter(value) => Some(value),
                    _ => None,
                };
                (
                    record.metric.clone(),
                    record
                        .labels
                        .iter()
                        .map(|(name, value)| (name.to_owned(), value.to_owned()))
                        .collect(),
                    value,
                )
            })
            .collect()
    }
}
