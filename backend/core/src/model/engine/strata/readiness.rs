//! Bounded readiness probing for a Strata service on its loopback endpoint.
//!
//! Strata is ready when `/health` answers 200, reports its model loaded, and names the exact
//! served model id. While the model loads, the server refuses connections; after a load, its
//! health reports the model unloaded if the engine dies. Both count as still loading, and a
//! failed start exits the process and surfaces as a workload exit instead.

use std::time::Duration;

use dotcore_domain::real_time;
use dotcore_runtime::LoopbackEndpoint;
use serde::Deserialize;

use crate::{
    inference::{
        EngineProbeError, EngineReadiness, EngineReadinessProbe, InferenceCancellation,
        InferenceFuture,
    },
    model::engine::probe,
};

/// A loaded Strata answers its health on loopback in milliseconds. The controller probes
/// deployments one after another in each reconcile pass, so this bounds how long a pass waits on
/// a stalled one; a slow answer only defers readiness to a later pass.
const READINESS_IO_TIMEOUT: Duration = Duration::from_secs(5);

/// Probes one Strata service on its loopback endpoint for readiness under its served model id.
pub(super) struct StrataReadinessProbe {
    endpoint: LoopbackEndpoint,
    served_model_id: String,
}

impl StrataReadinessProbe {
    pub(super) fn new(endpoint: LoopbackEndpoint, served_model_id: String) -> Self {
        Self {
            endpoint,
            served_model_id,
        }
    }
}

impl EngineReadinessProbe for StrataReadinessProbe {
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

/// The fields of Strata's `/health` answer that readiness reads.
#[derive(Deserialize)]
struct Health {
    loaded: bool,
    model: String,
}

/// Evaluates Strata's health from an assigned runtime endpoint.
async fn is_ready(
    endpoint: &LoopbackEndpoint,
    served_model_id: &str,
) -> Result<bool, EngineProbeError> {
    let client = reqwest::Client::builder()
        .no_proxy()
        .redirect(reqwest::redirect::Policy::none())
        .retry(reqwest::retry::never())
        .build()
        .map_err(|error| {
            probe::terminal(anyhow::Error::new(error).context("Strata readiness client failed"))
        })?;
    let request = async {
        let response = client
            .get(format!("http://127.0.0.1:{}/health", endpoint.host_port))
            .header("accept", "application/json")
            .send()
            .await?;
        let status = response.status();
        response.bytes().await.map(|body| (status, body))
    };
    // A refused, reset or stalled exchange is a server still loading or restarting; a start that
    // fails ends the workload, which the controller observes on its own.
    let Ok(Ok((status, body))) = real_time::bound(READINESS_IO_TIMEOUT, request).await else {
        return Ok(false);
    };
    if status != reqwest::StatusCode::OK {
        return Ok(false);
    }
    let health = serde_json::from_slice::<Health>(&body).map_err(|error| {
        probe::terminal(
            anyhow::Error::new(error).context("Strata /health returned an unexpected body"),
        )
    })?;
    Ok(health.loaded && health.model == served_model_id)
}

#[cfg(test)]
mod tests {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    use super::*;

    const LOADED: &str = r#"{"status": "ok", "max_context": 32768, "model": "qwen3-6-35b-a3b", "images": false, "api_key": false, "image_generation": false, "loaded": true, "service": "strata"}"#;

    #[tokio::test]
    async fn ready_requires_a_loaded_model_under_the_exact_served_model_id() {
        let port = serve(200, LOADED).await;
        assert!(
            is_ready(&endpoint(port), "qwen3-6-35b-a3b")
                .await
                .expect("probe")
        );

        let port = serve(200, LOADED).await;
        assert!(
            !is_ready(&endpoint(port), "qwen3-6-35b")
                .await
                .expect("probe")
        );
    }

    #[tokio::test]
    async fn unloaded_model_and_failing_health_are_not_ready() {
        let port = serve(
            200,
            &LOADED.replace(r#""loaded": true"#, r#""loaded": false"#),
        )
        .await;
        assert!(
            !is_ready(&endpoint(port), "qwen3-6-35b-a3b")
                .await
                .expect("probe")
        );

        let port = serve(503, "{}").await;
        assert!(
            !is_ready(&endpoint(port), "qwen3-6-35b-a3b")
                .await
                .expect("probe")
        );
    }

    #[tokio::test]
    async fn refused_connection_while_loading_is_not_ready() {
        let reserved = dotcore_domain::test_support::ports::reserve_port();

        assert!(
            !is_ready(&endpoint(reserved.port()), "qwen3-6-35b-a3b")
                .await
                .expect("probe")
        );
    }

    #[tokio::test]
    async fn unexpected_health_body_is_terminal() {
        let port = serve(200, r#"{"status": "ok"}"#).await;

        let error = is_ready(&endpoint(port), "qwen3-6-35b-a3b")
            .await
            .expect_err("unexpected body");

        assert_eq!(
            error.to_string(),
            "engine readiness probe failed: Strata /health returned an unexpected body"
        );
    }

    fn endpoint(port: u16) -> LoopbackEndpoint {
        LoopbackEndpoint {
            port_name: "http".to_owned(),
            host_port: port,
        }
    }

    /// Serves one response to the first connection.
    async fn serve(status: u16, body: &str) -> u16 {
        let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0))
            .await
            .expect("loopback listener");
        let port = listener.local_addr().expect("listener address").port();
        let response = format!(
            "HTTP/1.1 {status} X\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        );
        tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.expect("probe connection");
            let mut request = Vec::new();
            let mut buffer = [0_u8; 256];
            while !request.ends_with(b"\r\n\r\n") {
                let read = stream.read(&mut buffer).await.expect("request bytes");
                assert_ne!(read, 0, "request ended before its headers");
                request.extend_from_slice(&buffer[..read]);
            }
            assert!(request.starts_with(b"GET /health HTTP/1.1\r\n"));
            stream
                .write_all(response.as_bytes())
                .await
                .expect("response");
        });
        port
    }
}
