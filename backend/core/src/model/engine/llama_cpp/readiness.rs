//! Private bounded llama.cpp loopback readiness probing.

use std::time::Duration;

use anyhow::anyhow;
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
const READINESS_IO_TIMEOUT: Duration = Duration::from_secs(5);

/// Probes one llama.cpp service on its loopback endpoint for readiness under its served model id.
pub(crate) struct LlamaCppReadinessProbe {
    endpoint: LoopbackEndpoint,
    served_model_id: String,
}

impl LlamaCppReadinessProbe {
    pub(crate) fn new(endpoint: LoopbackEndpoint, served_model_id: String) -> Self {
        Self {
            endpoint,
            served_model_id,
        }
    }
}

impl EngineReadinessProbe for LlamaCppReadinessProbe {
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

/// Evaluates llama.cpp health and model alias from an assigned runtime endpoint.
async fn is_ready(
    endpoint: &LoopbackEndpoint,
    expected_alias: &str,
) -> Result<bool, EngineProbeError> {
    let Some(health) = probe_response(endpoint, "/health").await? else {
        return Ok(false);
    };
    if !health.is_healthy()? {
        return Ok(false);
    }
    let Some(models) = probe_response(endpoint, "/v1/models").await? else {
        return Ok(false);
    };
    models.has_exact_model_alias(expected_alias)
}

async fn probe_response(
    endpoint: &LoopbackEndpoint,
    path: &str,
) -> Result<Option<HttpResponse>, EngineProbeError> {
    match get_loopback(endpoint.host_port, path).await {
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
    fn parse(response: &[u8]) -> Result<Self, anyhow::Error> {
        Self::parse_if_complete(response, true)?.ok_or_else(|| {
            anyhow!("llama.cpp readiness response has no complete declared HTTP body")
        })
    }

    fn parse_if_complete(
        response: &[u8],
        peer_closed: bool,
    ) -> Result<Option<Self>, anyhow::Error> {
        let Some(header_end) = response.windows(4).position(|window| window == b"\r\n\r\n") else {
            return if peer_closed {
                Err(anyhow!(
                    "llama.cpp readiness response has no complete HTTP headers"
                ))
            } else {
                Ok(None)
            };
        };
        let (status, content_length) = parse_http_headers(&response[..header_end])?;
        let body = &response[header_end + 4..];
        let Some(content_length) = content_length else {
            return if peer_closed {
                Ok(Some(Self {
                    status,
                    body: body.to_vec(),
                }))
            } else {
                Ok(None)
            };
        };
        if content_length > MAX_HTTP_RESPONSE_BYTES.saturating_sub(header_end.saturating_add(4)) {
            return Err(anyhow!(
                "llama.cpp readiness response declared body exceeds size limit"
            ));
        }
        if body.len() < content_length {
            return if peer_closed {
                Err(anyhow!("llama.cpp readiness response body is truncated"))
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
        let value: Value = serde_json::from_slice(&self.body).map_err(|error| {
            probe::terminal(
                anyhow::Error::new(error).context("llama.cpp /health returned invalid JSON"),
            )
        })?;
        Ok(value.as_object().is_some_and(|object| {
            object.len() == 1 && object.get("status") == Some(&Value::String("ok".to_owned()))
        }))
    }

    fn has_exact_model_alias(&self, expected_alias: &str) -> Result<bool, EngineProbeError> {
        if self.status != 200 {
            return Ok(false);
        }
        let value: Value = serde_json::from_slice(&self.body).map_err(|error| {
            probe::terminal(
                anyhow::Error::new(error).context("llama.cpp /v1/models returned invalid JSON"),
            )
        })?;
        let Some(object) = value.as_object() else {
            return Ok(false);
        };
        let Some(data) = object.get("data").and_then(Value::as_array) else {
            return Ok(false);
        };
        Ok(
            object.get("object") == Some(&Value::String("list".to_owned()))
                && data.len() == 1
                && data[0].get("id") == Some(&Value::String(expected_alias.to_owned())),
        )
    }
}

async fn get_loopback(port: u16, path: &str) -> Result<HttpResponse, EngineProbeError> {
    get_loopback_until(port, path, Instant::now() + READINESS_IO_TIMEOUT).await
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
        .map_err(|error| probe::io_error("llama.cpp loopback connection failed", error))?;
    let request = format!(
        "GET {path} HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nAccept: application/json\r\nConnection: close\r\n\r\n"
    );
    tokio::time::timeout_at(deadline, stream.write_all(request.as_bytes()))
        .await
        .map_err(|_| EngineProbeError::Transient)?
        .map_err(|error| probe::io_error("llama.cpp readiness request write failed", error))?;

    collect_response(&mut stream, deadline).await
}

#[expect(
    clippy::disallowed_methods,
    reason = "bounds real loopback I/O of a readiness probe"
)]
async fn collect_response(
    stream: &mut TcpStream,
    deadline: Instant,
) -> Result<HttpResponse, EngineProbeError> {
    let mut response = Vec::new();
    let mut buffer = [0_u8; 8192];
    loop {
        let read = tokio::time::timeout_at(deadline, stream.read(&mut buffer))
            .await
            .map_err(|_| EngineProbeError::Transient)?;
        let read =
            match read {
                Ok(read) => read,
                Err(error) => {
                    return Err(if response.is_empty() {
                        probe::io_error("llama.cpp readiness response read failed", error)
                    } else {
                        probe::terminal(anyhow::Error::new(error).context(
                            "llama.cpp readiness response read failed after response bytes",
                        ))
                    });
                }
            };
        if read == 0 {
            return if response.is_empty() {
                Err(EngineProbeError::Transient)
            } else {
                HttpResponse::parse(&response).map_err(probe::terminal)
            };
        }
        if response.len() + read > MAX_HTTP_RESPONSE_BYTES {
            return Err(probe::terminal(anyhow!(
                "llama.cpp readiness response exceeds size limit"
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
        anyhow::Error::new(error).context("llama.cpp readiness response headers are not UTF-8")
    })?;
    let mut lines = headers.split("\r\n");
    let status_line = lines
        .next()
        .filter(|line| !line.is_empty())
        .ok_or_else(|| anyhow!("llama.cpp readiness response has no HTTP status"))?;
    let mut status_parts = status_line.split_whitespace();
    let version = status_parts.next().unwrap_or_default();
    let status_code = status_parts.next().unwrap_or_default();
    if !version.starts_with("HTTP/")
        || status_code.len() != 3
        || !status_code.bytes().all(|byte| byte.is_ascii_digit())
    {
        return Err(anyhow!(
            "llama.cpp readiness response has malformed HTTP status"
        ));
    }
    let status = status_code.parse::<u16>().map_err(|error| {
        anyhow::Error::new(error).context("llama.cpp readiness response has invalid HTTP status")
    })?;
    let mut content_length = None;
    for line in lines {
        let (name, value) = line
            .split_once(':')
            .ok_or_else(|| anyhow!("llama.cpp readiness response has malformed HTTP header"))?;
        if name.is_empty() || name.bytes().any(|byte| byte.is_ascii_whitespace()) {
            return Err(anyhow!(
                "llama.cpp readiness response has malformed HTTP header"
            ));
        }
        if name.eq_ignore_ascii_case("content-length") {
            let length = value.trim().parse::<usize>().map_err(|error| {
                anyhow::Error::new(error)
                    .context("llama.cpp readiness response has invalid content length")
            })?;
            if content_length.is_some_and(|previous| previous != length) {
                return Err(anyhow!(
                    "llama.cpp readiness response has conflicting content lengths"
                ));
            }
            content_length = Some(length);
        }
    }
    Ok((status, content_length))
}

#[cfg(test)]
mod tests {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    use super::*;

    #[test]
    fn readiness_requires_the_exact_health_and_model_alias_shape() {
        let health_response = HttpResponse {
            status: 200,
            body: br#"{"status":"ok"}"#.to_vec(),
        };
        let models = HttpResponse {
            status: 200,
            body: br#"{"object":"list","data":[{"id":"gemma-4-e2b"}]}"#.to_vec(),
        };

        assert!(health_response.is_healthy().is_ok_and(|ready| ready));
        assert!(
            models
                .has_exact_model_alias("gemma-4-e2b")
                .is_ok_and(|ready| ready)
        );
        assert!(
            models
                .has_exact_model_alias("other")
                .is_ok_and(|ready| !ready)
        );
    }

    #[test]
    fn readiness_treats_transient_io_errors_as_not_ready() {
        for kind in [
            std::io::ErrorKind::ConnectionRefused,
            std::io::ErrorKind::ConnectionReset,
            std::io::ErrorKind::TimedOut,
        ] {
            assert!(matches!(
                probe::io_error("probe", std::io::Error::from(kind)),
                EngineProbeError::Transient
            ));
        }
    }

    #[tokio::test]
    async fn readiness_treats_bounded_probe_timeout_as_not_ready() {
        let timeout = tokio::time::timeout(Duration::ZERO, std::future::pending::<()>());
        let probe: Result<(), EngineProbeError> =
            timeout.await.map_err(|_| EngineProbeError::Transient);

        assert!(matches!(probe, Err(EngineProbeError::Transient)));
    }

    #[tokio::test]
    async fn readiness_treats_connection_refused_as_not_ready() {
        let reserved = dotcore_domain::test_support::ports::reserve_port();
        let port = reserved.port();

        assert!(
            !is_ready(
                &LoopbackEndpoint {
                    port_name: "http".to_owned(),
                    host_port: port,
                },
                "gemma-4-e2b",
            )
            .await
            .expect("connection refusal is transient")
        );
    }

    #[tokio::test]
    async fn readiness_collects_fragmented_headers_and_body_without_peer_close() {
        let (port, server) = response_server(
            vec![
                b"HTTP/1.1 200 O".to_vec(),
                b"K\r\nContent-Length: 2\r\n\r\n".to_vec(),
                b"ok".to_vec(),
            ],
            true,
        )
        .await;

        let result = tokio::time::timeout(Duration::from_secs(1), get_loopback(port, "/health"))
            .await
            .expect("declared body should not wait for peer close");
        server.abort();

        let response = result.expect("fragmented response");
        assert_eq!(response.status, 200);
        assert_eq!(response.body, b"ok");
    }

    #[tokio::test]
    async fn readiness_treats_deadline_during_incomplete_response_as_not_ready() {
        let (port, server) = response_server(
            vec![b"HTTP/1.1 200 OK\r\nContent-Length: 5\r\n\r\nhe".to_vec()],
            true,
        )
        .await;

        let result =
            get_loopback_until(port, "/health", Instant::now() + Duration::from_millis(100)).await;
        server.abort();

        assert!(matches!(result, Err(EngineProbeError::Transient)));
    }

    #[tokio::test]
    async fn readiness_treats_empty_initial_close_as_not_ready() {
        let (port, server) = response_server(Vec::new(), false).await;
        let result = get_loopback(port, "/health").await;
        server.abort();

        assert!(matches!(result, Err(EngineProbeError::Transient)));
    }

    #[tokio::test]
    async fn readiness_treats_nonempty_truncated_headers_and_body_as_terminal() {
        for response in [
            b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\n".as_slice(),
            b"HTTP/1.1 200 OK\r\nContent-Length: 5\r\n\r\nok".as_slice(),
        ] {
            let (port, server) = response_server(vec![response.to_vec()], false).await;
            let result = get_loopback(port, "/health").await;
            server.abort();

            assert!(matches!(result, Err(EngineProbeError::Terminal(_))));
        }
    }

    #[tokio::test]
    async fn readiness_treats_malformed_completed_headers_as_terminal() {
        let (port, server) = response_server(
            vec![b"HTTP/1.1 200 OK\r\nContent-Length nope\r\n\r\n".to_vec()],
            false,
        )
        .await;
        let result = get_loopback(port, "/health").await;
        server.abort();

        assert!(matches!(result, Err(EngineProbeError::Terminal(_))));
    }

    #[tokio::test]
    async fn readiness_treats_oversized_responses_as_terminal() {
        let mut response = format!(
            "HTTP/1.1 200 OK\r\nContent-Length: {}\r\n\r\n",
            MAX_HTTP_RESPONSE_BYTES
        )
        .into_bytes();
        response.extend(std::iter::repeat_n(b'x', MAX_HTTP_RESPONSE_BYTES));
        let (port, server) = response_server(vec![response], false).await;
        let result = get_loopback(port, "/health").await;
        server.abort();

        assert!(matches!(result, Err(EngineProbeError::Terminal(_))));
    }

    #[test]
    fn readiness_preserves_malformed_http_and_200_json_errors() {
        assert!(HttpResponse::parse(b"HTTP/1.1 200 OK\r\n").is_err());

        let malformed = HttpResponse {
            status: 200,
            body: br#"{"#.to_vec(),
        };
        assert!(malformed.is_healthy().is_err());
        assert!(malformed.has_exact_model_alias("gemma-4-e2b").is_err());
    }

    #[test]
    fn readiness_terminal_errors_preserve_the_parser_source() {
        let malformed = HttpResponse {
            status: 200,
            body: br#"{"#.to_vec(),
        };

        let error = malformed.is_healthy().expect_err("malformed health JSON");
        assert!(matches!(error, EngineProbeError::Terminal(_)));
        assert!(
            error.to_string().starts_with(
                "engine readiness probe failed: llama.cpp /health returned invalid JSON"
            )
        );
        assert!(std::error::Error::source(&error).is_some());
    }

    async fn response_server(
        chunks: Vec<Vec<u8>>,
        keep_open: bool,
    ) -> (u16, tokio::task::JoinHandle<()>) {
        let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0))
            .await
            .expect("loopback listener");
        let port = listener.local_addr().expect("listener address").port();
        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.expect("readiness request");
            for chunk in chunks {
                stream.write_all(&chunk).await.expect("response bytes");
            }
            if keep_open {
                std::future::pending::<()>().await;
            }
        });
        (port, server)
    }

    #[tokio::test]
    async fn readiness_queries_the_runtime_assigned_loopback_endpoint() {
        let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0))
            .await
            .expect("loopback listener");
        let port = listener.local_addr().expect("listener address").port();
        tokio::spawn(async move {
            for body in [
                br#"{"status":"ok"}"#.as_slice(),
                br#"{"object":"list","data":[{"id":"gemma-4-e2b"}]}"#.as_slice(),
            ] {
                let (mut stream, _) = listener.accept().await.expect("readiness request");
                let mut request = Vec::new();
                let mut buffer = [0_u8; 256];
                while !request.ends_with(b"\r\n\r\n") {
                    let read = stream
                        .read(&mut buffer)
                        .await
                        .expect("readiness request bytes");
                    assert_ne!(read, 0, "readiness request ended before its headers");
                    request.extend_from_slice(&buffer[..read]);
                }
                stream
                    .write_all(
                        format!(
                            "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                            body.len()
                        )
                        .as_bytes(),
                    )
                    .await
                    .expect("response headers");
                stream.write_all(body).await.expect("response body");
            }
        });

        assert!(
            is_ready(
                &LoopbackEndpoint {
                    port_name: "http".to_owned(),
                    host_port: port,
                },
                "gemma-4-e2b",
            )
            .await
            .expect("healthy readiness response")
        );
    }
}
