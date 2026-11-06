//! Bounded readiness probing for a ninfer service on its loopback endpoint.
//!
//! ninfer is ready when `/health` answers 200 with `{"status":"ok"}` and `/v1/models` lists the
//! exact served model id. While the weights load, ninfer accepts connections but sends no HTTP
//! response, so each request has its own deadline and a timeout counts as still loading rather
//! than failure. A `/health` 503 after an engine-wide failure counts as not ready; a failed start
//! exits the process and surfaces as a workload exit instead.

use std::time::Duration;

use dotcore_runtime::LoopbackEndpoint;
use serde_json::Value;
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpStream,
    time::Instant,
};

use crate::{
    inference::{
        EngineProbeError, EngineReadiness, EngineReadinessProbe, InferenceCancellation,
        InferenceFuture,
    },
    model::engine::probe,
};

const MAX_HTTP_RESPONSE_BYTES: usize = 1024 * 1024;
/// A ready ninfer answers on loopback in milliseconds, while a loading one accepts and never
/// answers. The controller probes deployments one after another in each reconcile pass, so this
/// bound is how long a pass waits on each loading deployment; a slow answer from a ready engine
/// only defers readiness to a later pass.
// Not a behavior duration: bound on real loopback I/O of a readiness probe.
const READINESS_IO_TIMEOUT: Duration = Duration::from_millis(500);

/// Probes one ninfer service on its loopback endpoint for readiness under its served model id.
pub(super) struct NinferReadinessProbe {
    endpoint: LoopbackEndpoint,
    served_model_id: String,
}

impl NinferReadinessProbe {
    pub(super) fn new(endpoint: LoopbackEndpoint, served_model_id: String) -> Self {
        Self {
            endpoint,
            served_model_id,
        }
    }
}

impl EngineReadinessProbe for NinferReadinessProbe {
    fn probe<'a>(
        &'a self,
        cancellation: InferenceCancellation,
    ) -> InferenceFuture<'a, Result<EngineReadiness, EngineProbeError>> {
        probe::probe_readiness(
            cancellation,
            &self.served_model_id,
            is_ready(&self.endpoint, &self.served_model_id),
        )
    }
}

/// Evaluates ninfer health and exact served-model discovery from an assigned runtime endpoint.
async fn is_ready(
    endpoint: &LoopbackEndpoint,
    served_model_id: &str,
) -> Result<bool, EngineProbeError> {
    let Some(health) = probe_response(endpoint.host_port, "/health").await? else {
        return Ok(false);
    };
    if !health.is_healthy()? {
        return Ok(false);
    }
    let Some(models) = probe_response(endpoint.host_port, "/v1/models").await? else {
        return Ok(false);
    };
    models.has_exact_model_id(served_model_id)
}

/// Returns the response, or none when the attempt was transiently unavailable.
async fn probe_response(port: u16, path: &str) -> Result<Option<HttpResponse>, EngineProbeError> {
    match get_loopback_until(port, path, Instant::now() + READINESS_IO_TIMEOUT).await {
        Ok(response) => Ok(Some(response)),
        Err(EngineProbeError::Transient) => Ok(None),
        Err(error @ EngineProbeError::Terminal(_)) => Err(error),
    }
}

struct HttpResponse {
    status: u16,
    body: Vec<u8>,
}

impl HttpResponse {
    fn parse_if_complete(
        response: &[u8],
        peer_closed: bool,
    ) -> Result<Option<Self>, anyhow::Error> {
        let Some(header_end) = response.windows(4).position(|window| window == b"\r\n\r\n") else {
            return if peer_closed {
                Err(anyhow::anyhow!(
                    "ninfer readiness response has no complete HTTP headers"
                ))
            } else {
                Ok(None)
            };
        };
        let (status, content_length) = parse_http_headers(&response[..header_end])?;
        let body = &response[header_end + 4..];
        let Some(content_length) = content_length else {
            return Ok(peer_closed.then(|| Self {
                status,
                body: body.to_vec(),
            }));
        };
        if content_length > MAX_HTTP_RESPONSE_BYTES.saturating_sub(header_end.saturating_add(4)) {
            return Err(anyhow::anyhow!(
                "ninfer readiness response declared body exceeds size limit"
            ));
        }
        if body.len() < content_length {
            return if peer_closed {
                Err(anyhow::anyhow!(
                    "ninfer readiness response body is truncated"
                ))
            } else {
                Ok(None)
            };
        }
        Ok(Some(Self {
            status,
            body: body[..content_length].to_vec(),
        }))
    }

    fn is_healthy(&self) -> Result<bool, EngineProbeError> {
        if self.status != 200 {
            return Ok(false);
        }
        let value = self.json("ninfer /health returned invalid JSON")?;
        Ok(value.get("status") == Some(&Value::String("ok".to_owned())))
    }

    fn has_exact_model_id(&self, served_model_id: &str) -> Result<bool, EngineProbeError> {
        if self.status != 200 {
            return Ok(false);
        }
        let value = self.json("ninfer /v1/models returned invalid JSON")?;
        Ok(
            value.get("object") == Some(&Value::String("list".to_owned()))
                && value
                    .get("data")
                    .and_then(Value::as_array)
                    .is_some_and(|models| {
                        models.iter().any(|model| {
                            model.get("id") == Some(&Value::String(served_model_id.to_owned()))
                        })
                    }),
        )
    }

    fn json(&self, context: &'static str) -> Result<Value, EngineProbeError> {
        serde_json::from_slice(&self.body)
            .map_err(|error| probe::terminal(anyhow::Error::new(error).context(context)))
    }
}

#[expect(
    clippy::disallowed_methods,
    reason = "bounds real loopback I/O of a readiness probe"
)]
async fn get_loopback_until(
    port: u16,
    path: &str,
    deadline: Instant,
) -> Result<HttpResponse, EngineProbeError> {
    let mut stream = tokio::time::timeout_at(deadline, TcpStream::connect(("127.0.0.1", port)))
        .await
        .map_err(|_| EngineProbeError::Transient)?
        .map_err(|error| probe::io_error("ninfer loopback connection failed", error))?;
    let request = format!(
        "GET {path} HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nAccept: application/json\r\nConnection: close\r\n\r\n"
    );
    tokio::time::timeout_at(deadline, stream.write_all(request.as_bytes()))
        .await
        .map_err(|_| EngineProbeError::Transient)?
        .map_err(|error| probe::io_error("ninfer readiness request write failed", error))?;

    let mut response = Vec::new();
    let mut buffer = [0_u8; 8192];
    loop {
        // A load in progress accepts the connection and never answers, so the deadline expires.
        let read = tokio::time::timeout_at(deadline, stream.read(&mut buffer))
            .await
            .map_err(|_| EngineProbeError::Transient)?;
        let read = match read {
            Ok(read) => read,
            Err(error) if response.is_empty() => {
                return Err(probe::io_error(
                    "ninfer readiness response read failed",
                    error,
                ));
            }
            Err(error) => {
                return Err(probe::terminal(anyhow::Error::new(error).context(
                    "ninfer readiness response read failed after response bytes",
                )));
            }
        };
        if read == 0 {
            if response.is_empty() {
                return Err(EngineProbeError::Transient);
            }
            return HttpResponse::parse_if_complete(&response, true)
                .map_err(probe::terminal)?
                .ok_or_else(|| {
                    probe::terminal(anyhow::anyhow!(
                        "ninfer readiness response has no complete declared HTTP body"
                    ))
                });
        }
        if response.len() + read > MAX_HTTP_RESPONSE_BYTES {
            return Err(probe::terminal(anyhow::anyhow!(
                "ninfer readiness response exceeds size limit"
            )));
        }
        response.extend_from_slice(&buffer[..read]);
        if let Some(response) =
            HttpResponse::parse_if_complete(&response, false).map_err(probe::terminal)?
        {
            return Ok(response);
        }
    }
}

fn parse_http_headers(headers: &[u8]) -> Result<(u16, Option<usize>), anyhow::Error> {
    let headers = std::str::from_utf8(headers).map_err(|error| {
        anyhow::Error::new(error).context("ninfer readiness response headers are not UTF-8")
    })?;
    let mut lines = headers.split("\r\n");
    let mut status_parts = lines.next().unwrap_or_default().split_whitespace();
    let version = status_parts.next().unwrap_or_default();
    let status_code = status_parts.next().unwrap_or_default();
    if !version.starts_with("HTTP/")
        || status_code.len() != 3
        || !status_code.bytes().all(|byte| byte.is_ascii_digit())
    {
        return Err(anyhow::anyhow!(
            "ninfer readiness response has malformed HTTP status"
        ));
    }
    let status = status_code.parse::<u16>().map_err(|error| {
        anyhow::Error::new(error).context("ninfer readiness response has invalid HTTP status")
    })?;
    let mut content_length = None;
    for line in lines {
        let (name, value) = line
            .split_once(':')
            .filter(|(name, _)| {
                !name.is_empty() && !name.bytes().any(|byte| byte.is_ascii_whitespace())
            })
            .ok_or_else(|| {
                anyhow::anyhow!("ninfer readiness response has malformed HTTP header")
            })?;
        if name.eq_ignore_ascii_case("content-length") {
            let length = value.trim().parse::<usize>().map_err(|error| {
                anyhow::Error::new(error)
                    .context("ninfer readiness response has invalid content length")
            })?;
            if content_length.is_some_and(|previous| previous != length) {
                return Err(anyhow::anyhow!(
                    "ninfer readiness response has conflicting content lengths"
                ));
            }
            content_length = Some(length);
        }
    }
    Ok((status, content_length))
}

#[cfg(test)]
mod tests {
    use super::*;

    const MODELS: &str = r#"{"object":"list","data":[{"id":"qwen3-8-27b","object":"model","owned_by":"ninfer","max_model_len":250000}]}"#;

    #[tokio::test]
    async fn ready_requires_ok_health_and_the_exact_served_model_id() {
        let port = serve([(200, r#"{"status":"ok"}"#), (200, MODELS)]).await;

        assert!(
            is_ready(&endpoint(port), "qwen3-8-27b")
                .await
                .expect("probe")
        );

        let port = serve([(200, r#"{"status":"ok"}"#), (200, MODELS)]).await;
        assert!(
            !is_ready(&endpoint(port), "qwen3-8-27")
                .await
                .expect("probe")
        );
    }

    #[tokio::test]
    async fn engine_failure_health_503_is_not_ready() {
        let port = serve([(503, r#"{"status":"unavailable"}"#)]).await;

        assert!(
            !is_ready(&endpoint(port), "qwen3-8-27b")
                .await
                .expect("probe")
        );
    }

    #[tokio::test]
    async fn accepted_connection_without_response_is_still_loading_within_the_bound() {
        let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0))
            .await
            .expect("loopback listener");
        let port = listener.local_addr().expect("listener address").port();
        let server = tokio::spawn(async move {
            let mut held = Vec::new();
            while let Ok((stream, _)) = listener.accept().await {
                held.push(stream);
            }
        });

        let started = std::time::Instant::now();
        let ready = is_ready(&endpoint(port), "qwen3-8-27b").await;
        let elapsed = started.elapsed();
        server.abort();

        assert!(!ready.expect("a hung probe is not an error"));
        assert!(
            elapsed >= READINESS_IO_TIMEOUT && elapsed < Duration::from_secs(1),
            "loading probe took {elapsed:?}"
        );
    }

    #[tokio::test]
    async fn refused_connection_is_not_ready() {
        let reserved = dotcore_domain::test_support::ports::reserve_port();
        let port = reserved.port();

        assert!(
            !is_ready(&endpoint(port), "qwen3-8-27b")
                .await
                .expect("probe")
        );
    }

    #[tokio::test]
    async fn invalid_health_json_is_terminal() {
        let port = serve([(200, "{")]).await;

        let error = is_ready(&endpoint(port), "qwen3-8-27b")
            .await
            .expect_err("invalid JSON");

        assert_eq!(
            error.to_string(),
            "engine readiness probe failed: ninfer /health returned invalid JSON"
        );
    }

    fn endpoint(port: u16) -> LoopbackEndpoint {
        LoopbackEndpoint {
            port_name: "http".to_owned(),
            host_port: port,
        }
    }

    /// Serves one response per accepted connection, in order.
    async fn serve<const N: usize>(responses: [(u16, &'static str); N]) -> u16 {
        let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0))
            .await
            .expect("loopback listener");
        let port = listener.local_addr().expect("listener address").port();
        tokio::spawn(async move {
            for (status, body) in responses {
                let (mut stream, _) = listener.accept().await.expect("probe connection");
                let mut request = Vec::new();
                let mut buffer = [0_u8; 256];
                while !request.ends_with(b"\r\n\r\n") {
                    let read = stream.read(&mut buffer).await.expect("request bytes");
                    assert_ne!(read, 0, "request ended before its headers");
                    request.extend_from_slice(&buffer[..read]);
                }
                let response = format!(
                    "HTTP/1.1 {status} X\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
                stream
                    .write_all(response.as_bytes())
                    .await
                    .expect("response");
            }
        });
        port
    }
}
