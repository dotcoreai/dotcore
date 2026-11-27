//! Bounded readiness probing for an oMLX service on its loopback endpoint.
//!
//! oMLX is ready when `/health` answers 200 as healthy, which it does once its pinned model has
//! loaded, and `/v1/models` lists the exact served model id. Until the server listens the
//! connection is refused, and while the model loads health answers 503; both count as still
//! loading, and a failed start exits the process and surfaces as a workload exit instead. oMLX
//! reports the context it caps each request at, so a listing below the deployment's context per
//! request fails the deployment instead of being retried.

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

/// A loaded oMLX answers on loopback in milliseconds. The controller probes deployments one after
/// another in each reconcile pass, so this bounds how long a pass waits on a stalled one; a slow
/// answer only defers readiness to a later pass.
const READINESS_IO_TIMEOUT: Duration = Duration::from_secs(5);

/// Health status oMLX reports once its pinned model has loaded.
const HEALTHY_STATUS: &str = "healthy";

/// Probes one oMLX service on its loopback endpoint for readiness under its served model id, with
/// at least the deployment's context per request when it has one.
pub(super) struct OmlxReadinessProbe {
    endpoint: LoopbackEndpoint,
    served_model_id: String,
    context_per_request: Option<u64>,
}

impl OmlxReadinessProbe {
    pub(super) fn new(
        endpoint: LoopbackEndpoint,
        served_model_id: String,
        context_per_request: Option<u64>,
    ) -> Self {
        Self {
            endpoint,
            served_model_id,
            context_per_request,
        }
    }
}

impl EngineReadinessProbe for OmlxReadinessProbe {
    fn probe<'a>(
        &'a self,
        cancellation: InferenceCancellation,
    ) -> InferenceFuture<'a, Result<EngineReadiness, EngineProbeError>> {
        probe::probe_readiness(
            cancellation,
            &self.served_model_id,
            is_ready(
                &self.endpoint,
                &self.served_model_id,
                self.context_per_request,
            ),
        )
    }
}

/// The field of oMLX's `/health` answer that readiness reads.
#[derive(Deserialize)]
struct Health {
    status: String,
}

/// The fields of oMLX's `/v1/models` answer that readiness reads.
#[derive(Deserialize)]
struct Models {
    data: Vec<ListedModel>,
}

#[derive(Deserialize)]
struct ListedModel {
    id: String,
    max_model_len: Option<u64>,
}

/// Evaluates oMLX's health and served model listing from an assigned runtime endpoint.
async fn is_ready(
    endpoint: &LoopbackEndpoint,
    served_model_id: &str,
    context_per_request: Option<u64>,
) -> Result<bool, EngineProbeError> {
    let client = reqwest::Client::builder()
        .no_proxy()
        .redirect(reqwest::redirect::Policy::none())
        .retry(reqwest::retry::never())
        .build()
        .map_err(|error| {
            probe::unexpected(anyhow::Error::new(error).context("oMLX readiness client failed"))
        })?;
    let Some(health) = get_ok::<Health>(&client, endpoint, "/health").await? else {
        return Ok(false);
    };
    if health.status != HEALTHY_STATUS {
        return Ok(false);
    }
    let Some(models) = get_ok::<Models>(&client, endpoint, "/v1/models").await? else {
        return Ok(false);
    };
    let Some(model) = models
        .data
        .into_iter()
        .find(|model| model.id == served_model_id)
    else {
        return Ok(false);
    };
    let Some(context_per_request) = context_per_request else {
        return Ok(true);
    };
    match model.max_model_len {
        Some(started) if started >= context_per_request => Ok(true),
        Some(started) => Err(EngineProbeError::ContextBelowDeployment(
            StartedContextBelowDeployment {
                started,
                context_per_request,
            }
            .into(),
        )),
        // The launcher always sets the context cap, so a listing without one is not the server
        // the plan started.
        None => Err(probe::unexpected(anyhow::anyhow!(
            "oMLX /v1/models lists the served model without its context"
        ))),
    }
}

/// Gets `path` and decodes its 200 answer, or none when the server is unreachable, stalled or
/// answers another status, as it does while still loading.
async fn get_ok<T: serde::de::DeserializeOwned>(
    client: &reqwest::Client,
    endpoint: &LoopbackEndpoint,
    path: &str,
) -> Result<Option<T>, EngineProbeError> {
    let request = async {
        let response = client
            .get(format!("http://127.0.0.1:{}{path}", endpoint.host_port))
            .header("accept", "application/json")
            .send()
            .await?;
        let status = response.status();
        response.bytes().await.map(|body| (status, body))
    };
    // A refused, reset or stalled exchange is a server still starting or restarting; a start that
    // fails ends the workload, which the controller observes on its own.
    let Ok(Ok((status, body))) = real_time::bound(READINESS_IO_TIMEOUT, request).await else {
        return Ok(None);
    };
    if status != reqwest::StatusCode::OK {
        return Ok(None);
    }
    serde_json::from_slice::<T>(&body)
        .map(Some)
        .map_err(|error| {
            probe::unexpected(
                anyhow::Error::new(error)
                    .context(format!("oMLX {path} returned an unexpected body")),
            )
        })
}

/// oMLX caps each request below the deployment's context per request, so it would refuse prompts
/// the deployment admits.
#[derive(Debug, thiserror::Error)]
#[error(
    "oMLX started with a context per request of {started} tokens, below the deployment's \
     {context_per_request} tokens; recreate the deployment"
)]
struct StartedContextBelowDeployment {
    started: u64,
    context_per_request: u64,
}

#[cfg(test)]
mod tests {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    use super::*;

    /// oMLX 0.8.4's `/health` once its pinned model has loaded, and while it loads.
    const HEALTHY: &str = r#"{"status": "healthy", "default_model": "qwen3-5-4b", "engine_pool": {"model_count": 1, "loaded_count": 1, "final_ceiling": 8589934592, "current_model_memory": 2684354560}, "mcp": null}"#;
    const LOADING: &str =
        r#"{"status": "loading", "default_model": "qwen3-5-4b", "engine_pool": null, "mcp": null}"#;

    /// oMLX 0.8.4's `/v1/models` serving the model with a 32,768-token cap.
    const MODELS: &str = r#"{"object": "list", "data": [{"id": "qwen3-5-4b", "object": "model", "created": 1764115200, "owned_by": "omlx", "model_type": "llm", "max_model_len": 32768}]}"#;

    #[tokio::test]
    async fn ready_requires_health_and_the_exact_served_model_id() {
        let port = serve([(200, HEALTHY), (200, MODELS)]).await;
        assert!(
            is_ready(&endpoint(port), "qwen3-5-4b", Some(32_768))
                .await
                .expect("probe")
        );

        let port = serve([(200, HEALTHY), (200, MODELS)]).await;
        assert!(
            !is_ready(&endpoint(port), "qwen3-5", Some(32_768))
                .await
                .expect("probe")
        );
    }

    #[tokio::test]
    async fn loading_health_is_not_ready() {
        let port = serve([(503, LOADING)]).await;
        assert!(
            !is_ready(&endpoint(port), "qwen3-5-4b", Some(32_768))
                .await
                .expect("probe")
        );

        // A healthy status is required even on a 200.
        let port = serve([(200, LOADING)]).await;
        assert!(
            !is_ready(&endpoint(port), "qwen3-5-4b", Some(32_768))
                .await
                .expect("probe")
        );
    }

    #[tokio::test]
    async fn refused_connection_while_starting_is_not_ready() {
        let reserved = dotcore_domain::test_support::ports::reserve_port();

        assert!(
            !is_ready(&endpoint(reserved.port()), "qwen3-5-4b", None)
                .await
                .expect("probe")
        );
    }

    #[tokio::test]
    async fn a_listed_context_below_the_context_per_request_cannot_serve_the_deployment() {
        let port = serve([(200, HEALTHY), (200, MODELS)]).await;
        let error = is_ready(&endpoint(port), "qwen3-5-4b", Some(32_769))
            .await
            .expect_err("a context per request above the listed one");

        assert_eq!(
            error.to_string(),
            "engine cannot serve the deployment: oMLX started with a context per request of 32768 \
             tokens, below the deployment's 32769 tokens; recreate the deployment"
        );
    }

    #[tokio::test]
    async fn a_listing_without_the_context_cap_is_retried() {
        let port = serve([
            (200, HEALTHY),
            (
                200,
                &MODELS.replace(r#""max_model_len": 32768"#, r#""max_model_len": null"#),
            ),
        ])
        .await;
        let error = is_ready(&endpoint(port), "qwen3-5-4b", Some(32_768))
            .await
            .expect_err("no context cap");

        assert_eq!(
            error.to_string(),
            "engine readiness probe failed: oMLX /v1/models lists the served model without its context"
        );
    }

    #[tokio::test]
    async fn unexpected_health_body_is_retried() {
        let port = serve([(200, r#"{"ok": true}"#)]).await;

        let error = is_ready(&endpoint(port), "qwen3-5-4b", None)
            .await
            .expect_err("unexpected body");

        assert_eq!(
            error.to_string(),
            "engine readiness probe failed: oMLX /health returned an unexpected body"
        );
    }

    fn endpoint(port: u16) -> LoopbackEndpoint {
        LoopbackEndpoint {
            port_name: "http".to_owned(),
            host_port: port,
        }
    }

    /// Serves one response per connection, in order: `/health` first, then `/v1/models`.
    async fn serve<const N: usize>(responses: [(u16, &str); N]) -> u16 {
        let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0))
            .await
            .expect("loopback listener");
        let port = listener.local_addr().expect("listener address").port();
        let responses = responses
            .into_iter()
            .zip(["/health", "/v1/models"])
            .map(|((status, body), path)| {
                (
                    path,
                    format!(
                        "HTTP/1.1 {status} X\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                        body.len()
                    ),
                )
            })
            .collect::<Vec<_>>();
        tokio::spawn(async move {
            for (path, response) in responses {
                let (mut stream, _) = listener.accept().await.expect("probe connection");
                let mut request = Vec::new();
                let mut buffer = [0_u8; 256];
                while !request.ends_with(b"\r\n\r\n") {
                    let read = stream.read(&mut buffer).await.expect("request bytes");
                    assert_ne!(read, 0, "request ended before its headers");
                    request.extend_from_slice(&buffer[..read]);
                }
                assert!(request.starts_with(format!("GET {path} HTTP/1.1\r\n").as_bytes()));
                stream
                    .write_all(response.as_bytes())
                    .await
                    .expect("response");
            }
        });
        port
    }
}
