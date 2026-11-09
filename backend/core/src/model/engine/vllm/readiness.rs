//! Bounded readiness probing for a vLLM service on its loopback endpoint.
//!
//! It issues raw HTTP requests against the local service, checking the `/health/ready` endpoint when the
//! release declares ready-and-models mode and always requiring `/v1/models` to list the exact served model
//! id. Connection failures and timeouts are reported as transient, while malformed or invalid-JSON
//! responses are reported as terminal.

use std::time::Duration;

use dotcore_proto::dotcore::engine::vllm::v1::VllmReadinessMode;
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

/// Probes one vLLM service on its loopback endpoint for readiness under its served model id.
pub(super) struct VllmReadinessProbe {
    endpoint: LoopbackEndpoint,
    served_model_id: String,
    mode: i32,
}

impl VllmReadinessProbe {
    pub(super) fn new(endpoint: LoopbackEndpoint, served_model_id: String, mode: i32) -> Self {
        Self {
            endpoint,
            served_model_id,
            mode,
        }
    }
}

impl EngineReadinessProbe for VllmReadinessProbe {
    fn probe<'a>(
        &'a self,
        cancellation: InferenceCancellation,
    ) -> InferenceFuture<'a, Result<EngineReadiness, EngineProbeError>> {
        probe::probe_readiness(
            cancellation,
            &self.served_model_id,
            is_ready(&self.endpoint, &self.served_model_id, self.mode),
        )
    }
}

/// Evaluates release-declared vLLM readiness and exact model discovery.
async fn is_ready(
    endpoint: &LoopbackEndpoint,
    served_model_id: &str,
    mode: i32,
) -> Result<bool, EngineProbeError> {
    match VllmReadinessMode::try_from(mode) {
        Ok(VllmReadinessMode::Models) => {}
        Ok(VllmReadinessMode::ReadyAndModels) => {
            let Some(response) = probe_response(endpoint, "/health/ready").await? else {
                return Ok(false);
            };
            if response.status != 200 {
                return Ok(false);
            }
        }
        Ok(VllmReadinessMode::Unspecified) | Err(_) => {
            return Err(probe::terminal(anyhow::anyhow!(
                "vLLM readiness policy is unsupported"
            )));
        }
    }
    let Some(models) = probe_response(endpoint, "/v1/models").await? else {
        return Ok(false);
    };
    models.has_exact_model_id(served_model_id)
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
            anyhow::anyhow!("vLLM readiness response has no complete declared HTTP body")
        })
    }

    fn parse_if_complete(
        response: &[u8],
        peer_closed: bool,
    ) -> Result<Option<Self>, anyhow::Error> {
        let Some(header_end) = response.windows(4).position(|window| window == b"\r\n\r\n") else {
            return if peer_closed {
                Err(anyhow::anyhow!(
                    "vLLM readiness response has no complete HTTP headers"
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
            return Err(anyhow::anyhow!(
                "vLLM readiness response declared body exceeds size limit"
            ));
        }
        if body.len() < content_length {
            return if peer_closed {
                Err(anyhow::anyhow!("vLLM readiness response body is truncated"))
            } else {
                Ok(None)
            };
        }
        Ok(Some(Self {
            status,
            body: body[..content_length].to_vec(),
        }))
    }

    fn has_exact_model_id(&self, served_model_id: &str) -> Result<bool, EngineProbeError> {
        if self.status != 200 {
            return Ok(false);
        }
        let value: Value = serde_json::from_slice(&self.body).map_err(|error| {
            probe::terminal(
                anyhow::Error::new(error).context("vLLM /v1/models returned invalid JSON"),
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
                && data.iter().any(|model| {
                    model.get("id") == Some(&Value::String(served_model_id.to_owned()))
                }),
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
        .map_err(|error| probe::io_error("vLLM loopback connection failed", error))?;
    let request = format!(
        "GET {path} HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nAccept: application/json\r\nConnection: close\r\n\r\n"
    );
    tokio::time::timeout_at(deadline, stream.write_all(request.as_bytes()))
        .await
        .map_err(|_| EngineProbeError::Transient)?
        .map_err(|error| probe::io_error("vLLM readiness request write failed", error))?;

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
        let read = match read {
            Ok(read) => read,
            Err(error) => {
                return Err(if response.is_empty() {
                    probe::io_error("vLLM readiness response read failed", error)
                } else {
                    probe::terminal(
                        anyhow::Error::new(error)
                            .context("vLLM readiness response read failed after response bytes"),
                    )
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
            return Err(probe::terminal(anyhow::anyhow!(
                "vLLM readiness response exceeds size limit"
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
        anyhow::Error::new(error).context("vLLM readiness response headers are not UTF-8")
    })?;
    let mut lines = headers.split("\r\n");
    let status_line = lines
        .next()
        .filter(|line| !line.is_empty())
        .ok_or_else(|| anyhow::anyhow!("vLLM readiness response has no HTTP status"))?;
    let mut status_parts = status_line.split_whitespace();
    let version = status_parts.next().unwrap_or_default();
    let status_code = status_parts.next().unwrap_or_default();
    if !version.starts_with("HTTP/")
        || status_code.len() != 3
        || !status_code.bytes().all(|byte| byte.is_ascii_digit())
    {
        return Err(anyhow::anyhow!(
            "vLLM readiness response has malformed HTTP status"
        ));
    }
    let status = status_code.parse::<u16>().map_err(|error| {
        anyhow::Error::new(error).context("vLLM readiness response has invalid HTTP status")
    })?;
    let mut content_length = None;
    for line in lines {
        let (name, value) = line
            .split_once(':')
            .ok_or_else(|| anyhow::anyhow!("vLLM readiness response has malformed HTTP header"))?;
        if name.is_empty() || name.bytes().any(|byte| byte.is_ascii_whitespace()) {
            return Err(anyhow::anyhow!(
                "vLLM readiness response has malformed HTTP header"
            ));
        }
        if name.eq_ignore_ascii_case("content-length") {
            let length = value.trim().parse::<usize>().map_err(|error| {
                anyhow::Error::new(error)
                    .context("vLLM readiness response has invalid content length")
            })?;
            if content_length.is_some_and(|previous| previous != length) {
                return Err(anyhow::anyhow!(
                    "vLLM readiness response has conflicting content lengths"
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

    async fn serve_readiness(ready_status: u16, model_id: &'static str) -> u16 {
        let listener = tokio::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0))
            .await
            .expect("listener");
        let port = listener.local_addr().expect("address").port();
        tokio::spawn(async move {
            for _ in 0..2 {
                let Ok((mut stream, _)) = listener.accept().await else {
                    return;
                };
                let mut request = [0_u8; 512];
                let count = stream.read(&mut request).await.expect("request");
                let path = std::str::from_utf8(&request[..count]).expect("request text");
                let (status, body) = if path.contains("/health/ready") {
                    (ready_status, "{}".to_owned())
                } else {
                    (
                        200,
                        format!(r#"{{"object":"list","data":[{{"id":"{model_id}"}}]}}"#),
                    )
                };
                let response = format!(
                    "HTTP/1.1 {status} OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
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

    #[tokio::test]
    async fn models_readiness_accepts_exact_served_alias() {
        let port = serve_readiness(200, "fixture-serving-id").await;
        let endpoint = LoopbackEndpoint {
            port_name: "http".to_owned(),
            host_port: port,
        };
        assert!(
            is_ready(
                &endpoint,
                "fixture-serving-id",
                VllmReadinessMode::Models as i32
            )
            .await
            .expect("probe")
        );
    }

    #[tokio::test]
    async fn ready_and_models_rejects_wrong_health_status_and_alias() {
        let port = serve_readiness(503, "different-id").await;
        let endpoint = LoopbackEndpoint {
            port_name: "http".to_owned(),
            host_port: port,
        };
        assert!(
            !is_ready(
                &endpoint,
                "fixture-serving-id",
                VllmReadinessMode::ReadyAndModels as i32
            )
            .await
            .expect("probe")
        );
    }

    #[tokio::test]
    async fn unsupported_readiness_mode_is_terminal() {
        let endpoint = LoopbackEndpoint {
            port_name: "http".to_owned(),
            host_port: 1,
        };
        assert!(matches!(
            is_ready(&endpoint, "fixture", VllmReadinessMode::Unspecified as i32).await,
            Err(EngineProbeError::Terminal(_))
        ));
    }
}
