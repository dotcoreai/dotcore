//! Controller-private upstream verification transport and deterministic health state.

use std::{
    error::Error,
    future::Future,
    pin::Pin,
    time::{Duration, SystemTime},
};

use dotcore_domain::{
    real_time,
    timing::{Backoff, ModelTiming},
};
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;

/// Longest one verification of an upstream endpoint may take.
pub(crate) const PROBE_TIMEOUT: Duration = Duration::from_secs(5);
pub(crate) const MAX_PROBE_BODY_BYTES: usize = 64 * 1024;

/// The expected semantic response for one verification request.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum VerificationExpectation {
    Health,
}

/// A controller-requested HTTP verification operation.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct VerificationRequest {
    pub(crate) url: url::Url,
    pub(crate) authorization: String,
    pub(crate) expectation: VerificationExpectation,
    pub(crate) expected_model_id: Option<String>,
}

/// The bounded HTTP evidence returned by the verification boundary.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct VerificationObservation {
    pub(crate) status: u16,
    pub(crate) minimum_retry: Option<Duration>,
    pub(crate) body: Vec<u8>,
}

/// Controller-private HTTP capability that executes only verification requests.
pub(crate) trait VerificationHttp: Send + Sync {
    fn verify(
        &self,
        request: VerificationRequest,
        cancellation: CancellationToken,
    ) -> Pin<
        Box<
            dyn Future<Output = Result<VerificationObservation, VerificationTransportError>>
                + Send
                + '_,
        >,
    >;
}

/// Structured TLS causes relevant to safe serving eligibility.
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
pub(crate) enum TlsFailure {
    #[error("TLS certificate identity failed")]
    CertificateIdentity,

    #[error("TLS negotiation failed")]
    Other,
}

/// Transport-level verification failures that do not expose request credentials.
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
pub(crate) enum VerificationTransportError {
    #[error("cancelled")]
    Cancelled,

    #[error("timed out")]
    Timeout,

    #[error("DNS lookup failed")]
    Dns,

    #[error("{0}")]
    Tls(TlsFailure),

    #[error("network failed")]
    Network,

    #[error("response body exceeded the verification limit")]
    BodyTooLarge,

    #[error("response was invalid")]
    InvalidResponse,
}

/// Production controller verification transport with redirects disabled.
pub(crate) struct ReqwestVerificationHttp {
    client: reqwest::Client,
}

impl ReqwestVerificationHttp {
    pub(crate) fn new() -> Result<Self, reqwest::Error> {
        Ok(Self {
            client: reqwest::Client::builder()
                .redirect(reqwest::redirect::Policy::none())
                .build()?,
        })
    }
}

impl VerificationHttp for ReqwestVerificationHttp {
    fn verify(
        &self,
        request: VerificationRequest,
        cancellation: CancellationToken,
    ) -> Pin<
        Box<
            dyn Future<Output = Result<VerificationObservation, VerificationTransportError>>
                + Send
                + '_,
        >,
    > {
        Box::pin(async move {
            let authorization = request.authorization;
            let mut request = self.client.get(request.url);
            if !authorization.is_empty() {
                request = request.header("authorization", authorization);
            }
            tokio::select! {
                biased;
                _ = cancellation.cancelled() => Err(VerificationTransportError::Cancelled),
                result = real_time::bound(PROBE_TIMEOUT, async {
                    let response = request.send().await.map_err(classify_reqwest_error)?;
                    let status = response.status().as_u16();
                    let minimum_retry = minimum_retry_delay(response.headers(), real_time::wall_now());
                    let mut body = Vec::new();
                    let mut stream = response.bytes_stream();
                    use futures_util::StreamExt;
                    while let Some(chunk) = stream.next().await {
                        let chunk = chunk.map_err(classify_reqwest_error)?;
                        if body.len().saturating_add(chunk.len()) > MAX_PROBE_BODY_BYTES {
                            return Err(VerificationTransportError::BodyTooLarge);
                        }
                        body.extend_from_slice(&chunk);
                    }
                    Ok(VerificationObservation {
                        status,
                        minimum_retry,
                        body,
                    })
                }) => result.map_err(|_| VerificationTransportError::Timeout)?,
            }
        })
    }
}

/// Controller-only normalized probe outcome.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ProbeClassification {
    Success,
    TransientFailure,
    ImmediateWithdrawal,
}

/// Per-deployment health hysteresis independent of controller scheduling.
#[derive(Clone, Debug)]
pub(crate) struct HealthState {
    ready: bool,
    consecutive_successes: u8,
    consecutive_failures: u8,
    has_verified: bool,
}

impl HealthState {
    pub(crate) fn new() -> Self {
        Self {
            ready: false,
            consecutive_successes: 0,
            consecutive_failures: 0,
            has_verified: false,
        }
    }

    /// Records evidence and returns whether eligibility changed.
    pub(crate) fn record(&mut self, classification: ProbeClassification) -> bool {
        let was_ready = self.ready;
        match classification {
            ProbeClassification::Success => {
                self.consecutive_successes = self.consecutive_successes.saturating_add(1);
                self.consecutive_failures = 0;
                if !self.has_verified || self.consecutive_successes >= 2 {
                    self.ready = true;
                    self.has_verified = true;
                }
            }
            ProbeClassification::TransientFailure => {
                self.consecutive_successes = 0;
                self.consecutive_failures = self.consecutive_failures.saturating_add(1);
                if self.consecutive_failures >= 3 {
                    self.ready = false;
                }
            }
            ProbeClassification::ImmediateWithdrawal => {
                self.consecutive_successes = 0;
                self.consecutive_failures = 0;
                self.ready = false;
            }
        }
        was_ready != self.ready
    }

    pub(crate) fn ready(&self) -> bool {
        self.ready
    }
}

/// Deterministic retry delay calculation. Jitter is applied by the scheduler.
#[derive(Clone, Debug)]
pub(crate) struct RetrySchedule {
    backoff: Backoff,
    delay: Duration,
}

impl RetrySchedule {
    pub(crate) fn new(backoff: Backoff) -> Self {
        Self {
            backoff,
            delay: backoff.initial,
        }
    }
    pub(crate) fn next(&mut self, minimum: Option<Duration>) -> Duration {
        let delay = self.delay.max(minimum.unwrap_or_default());
        self.delay = self.delay.saturating_mul(2).min(self.backoff.max);
        delay
    }
    pub(crate) fn reset(&mut self) {
        self.delay = self.backoff.initial;
    }
}

/// Injected stable per-target healthy-probe jitter source.
pub(crate) trait HealthJitter: Send + Sync {
    fn fraction(&self, deployment_id: &str) -> f64;
}

/// Stable identifier-derived jitter that spreads healthy probes without nondeterministic scheduling.
pub(crate) struct StableHealthJitter;
impl HealthJitter for StableHealthJitter {
    fn fraction(&self, deployment_id: &str) -> f64 {
        let hash = deployment_id.bytes().fold(0_u64, |value, byte| {
            value.wrapping_mul(31).wrapping_add(u64::from(byte))
        });
        (hash % 401) as f64 / 1000.0 - 0.2
    }
}

/// Controller-owned injectable health boundary dependencies.
pub(crate) struct HealthDependencies {
    pub(crate) verification_http: std::sync::Arc<dyn VerificationHttp>,
    pub(crate) jitter: std::sync::Arc<dyn HealthJitter>,
}

/// Time-injected per-target health scheduling and stale-readiness control.
#[derive(Clone, Debug)]
pub(crate) struct HealthSchedule {
    health: HealthState,
    timing: ModelTiming,
    retry: RetrySchedule,
    last_success_at: Option<Instant>,
    next_probe_at: Instant,
}

impl HealthSchedule {
    /// Starts a target's schedule at `now`, pacing its verifications by `timing`.
    pub(crate) fn new(now: Instant, timing: ModelTiming) -> Self {
        Self {
            health: HealthState::new(),
            timing,
            retry: RetrySchedule::new(timing.upstream_retry),
            last_success_at: None,
            next_probe_at: now,
        }
    }
    pub(crate) fn due(&self, now: Instant) -> bool {
        now >= self.next_probe_at
    }
    pub(crate) fn next_probe_at(&self) -> Instant {
        self.next_probe_at
    }
    pub(crate) fn ready(&self) -> bool {
        self.health.ready()
    }
    /// Applies one result at monotonic time `now` and returns eligibility change.
    pub(crate) fn record(
        &mut self,
        now: Instant,
        classification: ProbeClassification,
        minimum_retry: Option<Duration>,
        jitter_fraction: f64,
    ) -> bool {
        let changed = self.health.record(classification);
        match classification {
            ProbeClassification::Success => {
                self.last_success_at = Some(now);
                self.retry.reset();
                self.next_probe_at =
                    now + healthy_delay(self.timing.upstream_healthy_interval, jitter_fraction);
            }
            ProbeClassification::TransientFailure | ProbeClassification::ImmediateWithdrawal => {
                self.next_probe_at = now + self.retry.next(minimum_retry)
            }
        }
        changed
    }
    /// Withdraws an otherwise eligible target once no successful evidence remains fresh.
    pub(crate) fn withdraw_if_stale(&mut self, now: Instant) -> bool {
        let stale = self.last_success_at.is_some_and(|success| {
            now.saturating_duration_since(success) >= self.timing.upstream_success_max_age
        });
        if stale && self.health.ready {
            self.health.ready = false;
            true
        } else {
            false
        }
    }
}

/// Pause after a successful verification: the healthy `interval`, spread by the deployment's
/// `jitter_fraction`.
fn healthy_delay(interval: Duration, jitter_fraction: f64) -> Duration {
    interval.mul_f64((1.0 + jitter_fraction).clamp(0.8, 1.2))
}

/// Builds the generic endpoint availability request for a deployment.
pub(crate) fn verification_requests(
    deployment: &dotcore_proto::dotcore::model::v1::ModelDeploymentDeclaration,
) -> Result<Vec<VerificationRequest>, &'static str> {
    use dotcore_proto::dotcore::model::v1::model_deployment_snapshot::Source;
    let snapshot = deployment
        .snapshot
        .as_ref()
        .and_then(|snapshot| snapshot.source.as_ref());
    let Some(Source::Upstream(_)) = snapshot else {
        return Err("deployment is not upstream");
    };
    let desired = deployment
        .upstream_desired_configuration
        .as_ref()
        .ok_or("upstream deployment has no desired configuration")?;
    let endpoint = desired
        .endpoint
        .as_ref()
        .ok_or("upstream deployment has no endpoint configuration")?;
    let base =
        url::Url::parse(&endpoint.base_url).map_err(|_| "upstream verification URL is invalid")?;
    let authorization = if desired.api_key.is_empty() {
        String::new()
    } else {
        format!("Bearer {}", desired.api_key)
    };
    Ok(vec![verification_request_at(
        &base,
        &["models"],
        authorization,
        VerificationExpectation::Health,
        None,
    )?])
}

fn verification_request_at(
    base: &url::Url,
    segments: &[&str],
    authorization: String,
    expectation: VerificationExpectation,
    expected_model_id: Option<String>,
) -> Result<VerificationRequest, &'static str> {
    let mut url = base.clone();
    append_verification_segments(&mut url, segments.iter().copied())?;
    Ok(VerificationRequest {
        url,
        authorization,
        expectation,
        expected_model_id,
    })
}

fn append_verification_segments<'a>(
    url: &mut url::Url,
    segments: impl IntoIterator<Item = &'a str>,
) -> Result<(), &'static str> {
    let mut path = url
        .path_segments_mut()
        .map_err(|_| "upstream verification URL cannot accept a model path")?;
    for segment in segments {
        path.push(segment);
    }
    Ok(())
}

pub(crate) fn classify_observation(
    result: Result<VerificationObservation, VerificationTransportError>,
    expectation: VerificationExpectation,
    expected_model_id: Option<&str>,
) -> ProbeClassification {
    match result {
        Ok(observation)
            if (200..300).contains(&observation.status)
                && response_matches(&observation.body, expectation, expected_model_id) =>
        {
            ProbeClassification::Success
        }
        Ok(observation) if matches!(observation.status, 401 | 403 | 404) => {
            ProbeClassification::ImmediateWithdrawal
        }
        Ok(_) => ProbeClassification::ImmediateWithdrawal,
        Err(
            VerificationTransportError::BodyTooLarge
            | VerificationTransportError::InvalidResponse
            | VerificationTransportError::Tls(_),
        ) => ProbeClassification::ImmediateWithdrawal,
        Err(_) => ProbeClassification::TransientFailure,
    }
}

fn response_matches(
    _body: &[u8],
    _expectation: VerificationExpectation,
    _expected_model_id: Option<&str>,
) -> bool {
    true
}

fn minimum_retry_delay(headers: &reqwest::header::HeaderMap, now: SystemTime) -> Option<Duration> {
    let retry_after = headers
        .get(reqwest::header::RETRY_AFTER)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| parse_retry_after(value, now));
    let provider_reset = headers
        .get("x-ratelimit-reset")
        .and_then(|value| value.to_str().ok())
        .and_then(|value| parse_provider_reset(value, now));
    let request_reset = headers
        .get("x-ratelimit-reset-requests")
        .and_then(|value| value.to_str().ok())
        .and_then(parse_duration_reset);
    [retry_after, provider_reset, request_reset]
        .into_iter()
        .flatten()
        .max()
}

fn parse_retry_after(value: &str, now: SystemTime) -> Option<Duration> {
    value
        .trim()
        .parse::<u64>()
        .ok()
        .map(Duration::from_secs)
        .or_else(|| {
            httpdate::parse_http_date(value.trim())
                .ok()
                .map(|at| at.duration_since(now).unwrap_or_default())
        })
        .or_else(|| parse_duration_reset(value.trim()))
}

fn parse_provider_reset(value: &str, now: SystemTime) -> Option<Duration> {
    value
        .trim()
        .parse::<u64>()
        .ok()
        .and_then(|seconds| SystemTime::UNIX_EPOCH.checked_add(Duration::from_secs(seconds)))
        .and_then(|reset_at| reset_at.duration_since(now).ok())
        .or_else(|| {
            httpdate::parse_http_date(value.trim())
                .ok()
                .map(|at| at.duration_since(now).unwrap_or_default())
        })
}

fn parse_duration_reset(value: &str) -> Option<Duration> {
    let mut remaining = value.trim();
    let mut duration = Duration::ZERO;
    while !remaining.is_empty() {
        let digits = remaining.chars().take_while(char::is_ascii_digit).count();
        let (number, suffix) = remaining.split_at(digits);
        let amount = number.parse::<u64>().ok()?;
        let unit = suffix.chars().next()?;
        duration = duration.checked_add(match unit {
            'h' => Duration::from_secs(amount.saturating_mul(60 * 60)),
            'm' => Duration::from_secs(amount.saturating_mul(60)),
            's' => Duration::from_secs(amount),
            _ => return None,
        })?;
        remaining = &suffix[unit.len_utf8()..];
    }
    Some(duration)
}

fn classify_reqwest_error(error: reqwest::Error) -> VerificationTransportError {
    if error.is_timeout() {
        return VerificationTransportError::Timeout;
    }
    if error.is_builder() {
        return VerificationTransportError::InvalidResponse;
    }

    let mut source = error.source();
    let mut tls_failure = None;
    let mut dns_failure = false;
    while let Some(cause) = source {
        if let Some(rustls::Error::InvalidCertificate(certificate)) =
            cause.downcast_ref::<rustls::Error>()
        {
            tls_failure = Some(match certificate {
                rustls::CertificateError::NotValidForName
                | rustls::CertificateError::NotValidForNameContext { .. } => {
                    TlsFailure::CertificateIdentity
                }
                _ => TlsFailure::Other,
            });
            break;
        }
        if let Some(io) = cause.downcast_ref::<std::io::Error>()
            && io.kind() == std::io::ErrorKind::NotFound
        {
            dns_failure = true;
            break;
        }
        source = cause.source();
    }

    if let Some(failure) = tls_failure {
        VerificationTransportError::Tls(failure)
    } else if dns_failure {
        VerificationTransportError::Dns
    } else if error.is_connect() {
        VerificationTransportError::Network
    } else {
        VerificationTransportError::InvalidResponse
    }
}

#[cfg(test)]
#[derive(Clone)]
pub(crate) struct ScriptedVerificationHttp {
    observations: std::sync::Arc<
        std::sync::Mutex<
            std::collections::VecDeque<Result<VerificationObservation, VerificationTransportError>>,
        >,
    >,
    requests: std::sync::Arc<std::sync::Mutex<Vec<VerificationRequest>>>,
}
#[cfg(test)]
impl ScriptedVerificationHttp {
    pub(crate) fn new(
        observations: impl IntoIterator<
            Item = Result<VerificationObservation, VerificationTransportError>,
        >,
    ) -> Self {
        Self {
            observations: std::sync::Arc::new(std::sync::Mutex::new(
                observations.into_iter().collect(),
            )),
            requests: std::sync::Arc::new(std::sync::Mutex::new(Vec::new())),
        }
    }
    pub(crate) fn requests(&self) -> Vec<VerificationRequest> {
        self.requests
            .lock()
            .expect("scripted verification requests")
            .clone()
    }
}
#[cfg(test)]
impl VerificationHttp for ScriptedVerificationHttp {
    fn verify(
        &self,
        request: VerificationRequest,
        cancellation: CancellationToken,
    ) -> Pin<
        Box<
            dyn Future<Output = Result<VerificationObservation, VerificationTransportError>>
                + Send
                + '_,
        >,
    > {
        self.requests
            .lock()
            .expect("scripted verification requests")
            .push(request);
        let observation = self
            .observations
            .lock()
            .expect("scripted verification observations")
            .pop_front()
            .unwrap_or(Err(VerificationTransportError::Network));
        Box::pin(async move {
            if cancellation.is_cancelled() {
                Err(VerificationTransportError::Cancelled)
            } else {
                observation
            }
        })
    }
}

#[cfg(test)]
mod tests {
    use std::{
        sync::{
            Arc,
            atomic::{AtomicBool, Ordering},
        },
        time::{Duration, SystemTime},
    };

    use dotcore_domain::timing::Timing;
    use futures_util::FutureExt;

    use super::*;
    #[tokio::test(start_paused = true)]
    async fn probe_timeout_covers_a_stalled_response_body() {
        use tokio::{
            io::{AsyncReadExt, AsyncWriteExt},
            net::TcpListener,
        };

        let listener = TcpListener::bind("127.0.0.1:0").await.expect("listener");
        let address = listener.local_addr().expect("listener address");
        let body_stalled = Arc::new(tokio::sync::Notify::new());
        let stalled = Arc::new(AtomicBool::new(false));
        let server_stalled = body_stalled.clone();
        let server_stalled_flag = stalled.clone();
        let response_started = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.expect("probe connection");
            let mut request = [0];
            stream
                .read_exact(&mut request)
                .await
                .expect("probe request");
            stream
                .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\nx")
                .await
                .expect("partial probe response");
            server_stalled_flag.store(true, Ordering::Release);
            server_stalled.notify_one();
            std::future::pending::<()>().await;
        });
        let http = ReqwestVerificationHttp::new().expect("verification client");
        let probe = tokio::spawn(async move {
            http.verify(
                VerificationRequest {
                    url: format!("http://{address}").parse().expect("probe URL"),
                    authorization: "Bearer test".to_owned(),
                    expectation: VerificationExpectation::Health,
                    expected_model_id: None,
                },
                CancellationToken::new(),
            )
            .await
        });

        for _ in 0..1_024 {
            if stalled.load(Ordering::Acquire) {
                break;
            }
            tokio::task::yield_now().await;
        }
        assert!(
            stalled.load(Ordering::Acquire),
            "probe request must reach the fixture within a bounded number of scheduler turns"
        );
        assert!(
            body_stalled.notified().now_or_never().is_some(),
            "fixture must notify after stalling the response body"
        );
        tokio::time::advance(PROBE_TIMEOUT).await;
        assert!(matches!(
            probe.await.expect("probe task").expect_err("probe timeout"),
            VerificationTransportError::Timeout
        ));
        response_started.abort();
    }

    #[test]
    fn transport_variants_preserve_exact_display_shapes() {
        let failures = [
            (VerificationTransportError::Cancelled, "cancelled"),
            (VerificationTransportError::Timeout, "timed out"),
            (VerificationTransportError::Dns, "DNS lookup failed"),
            (
                VerificationTransportError::Tls(TlsFailure::CertificateIdentity),
                "TLS certificate identity failed",
            ),
            (
                VerificationTransportError::Tls(TlsFailure::Other),
                "TLS negotiation failed",
            ),
            (VerificationTransportError::Network, "network failed"),
            (
                VerificationTransportError::BodyTooLarge,
                "response body exceeded the verification limit",
            ),
            (
                VerificationTransportError::InvalidResponse,
                "response was invalid",
            ),
        ];

        for (failure, display) in failures {
            assert_eq!(failure.to_string(), display);
            assert!(failure.source().is_none());
        }
    }

    #[tokio::test]
    async fn network_failure_retains_the_reqwest_source_after_classification() {
        let reserved = dotcore_domain::test_support::ports::reserve_port();
        let address = std::net::SocketAddr::from(([127, 0, 0, 1], reserved.port()));
        let error = reqwest::Client::new()
            .get(format!("http://{address}"))
            .send()
            .await
            .expect_err("closed listener must reject connection");

        let failure = classify_reqwest_error(error);

        assert!(matches!(failure, VerificationTransportError::Network));
        assert_eq!(failure.to_string(), "network failed");
        assert!(failure.source().is_none());
    }

    #[test]
    fn retry_schedule_obeys_exponential_cap_and_provider_minimum() {
        let mut retry = RetrySchedule::new(Timing::production().model.upstream_retry);
        assert_eq!(retry.next(None), Duration::from_secs(10));
        assert_eq!(retry.next(None), Duration::from_secs(20));
        assert_eq!(
            retry.next(Some(Duration::from_secs(90))),
            Duration::from_secs(90)
        );
    }
    #[test]
    fn retry_after_accepts_delta_http_date_and_provider_reset_guidance() {
        let now = SystemTime::UNIX_EPOCH + Duration::from_secs(1_000);
        assert_eq!(parse_retry_after("10", now), Some(Duration::from_secs(10)));
        assert_eq!(
            parse_retry_after("Thu, 01 Jan 1970 00:17:00 GMT", now),
            Some(Duration::from_secs(20))
        );
        assert_eq!(
            parse_duration_reset("2m30s"),
            Some(Duration::from_secs(150))
        );
        let mut headers = reqwest::header::HeaderMap::new();
        headers.insert("retry-after", "10".parse().expect("header"));
        headers.insert("x-ratelimit-reset", "1020".parse().expect("header"));
        headers.insert("x-ratelimit-reset-requests", "30s".parse().expect("header"));
        assert_eq!(
            minimum_retry_delay(&headers, now),
            Some(Duration::from_secs(30))
        );
    }
    #[test]
    fn health_schedule_uses_exact_injected_retry_deadline() {
        let start = Instant::now();
        let mut schedule = HealthSchedule::new(start, Timing::production().model);
        schedule.record(start, ProbeClassification::TransientFailure, None, 0.0);
        assert_eq!(schedule.next_probe_at(), start + Duration::from_secs(10));
        assert!(!schedule.due(start + Duration::from_secs(9)));
        assert!(schedule.due(start + Duration::from_secs(10)));
    }
    #[test]
    fn scripted_port_records_requests_for_controller_tests() {
        let port = ScriptedVerificationHttp::new([]);
        assert_eq!(port.requests(), Vec::<VerificationRequest>::new());
    }

    #[test]
    fn certificate_identity_failure_withdraws_immediately() {
        assert_eq!(
            classify_observation(
                Err(VerificationTransportError::Tls(
                    TlsFailure::CertificateIdentity
                )),
                VerificationExpectation::Health,
                None
            ),
            ProbeClassification::ImmediateWithdrawal
        );
    }
}
