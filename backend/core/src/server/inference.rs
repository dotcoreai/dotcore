//! Inference listener lifecycle and bounded connection dispatch.

use std::{future::Future, pin::Pin, sync::Arc, time::Duration};

use dotcore_domain::real_time;
use tracing::{Instrument, field, info_span, warn};

use super::{ServerError, ServerResult};
use crate::router::InferenceRouter;

const MAX_CONCURRENT_CONNECTIONS: usize = 4;
const CONNECTION_TIMEOUT: Duration = Duration::from_secs(10 * 60);

pub(super) fn prepare<F>(
    listener: tokio::net::TcpListener,
    router: InferenceRouter,
    shutdown: F,
) -> Pin<Box<dyn Future<Output = ServerResult<()>> + Send>>
where
    F: Future<Output = ()> + Send + 'static,
{
    let span = info_span!(
        "inference_listener",
        operation = "inference_listener",
        transport = "openai_http",
        outcome = field::Empty
    );
    let permits = Arc::new(tokio::sync::Semaphore::new(MAX_CONCURRENT_CONNECTIONS));
    Box::pin(async move {
        tokio::pin!(shutdown);
        let mut connections = tokio::task::JoinSet::new();
        loop {
            let permit = tokio::select! {
                _ = &mut shutdown => return stop(&mut connections, &span).await,
                permit = permits.clone().acquire_owned() => permit.expect("server-owned inference request semaphore remains open"),
            };
            tokio::select! {
                _ = &mut shutdown => return stop(&mut connections, &span).await,
                Some(result) = connections.join_next(), if !connections.is_empty() => {
                    record_connection_task_result(result);
                },
                result = listener.accept() => {
                    let (stream, _) = result.map_err(|error| ServerError::Management(anyhow::Error::new(error)))?;
                    // Without it, a small response write waits for the client's delayed
                    // acknowledgment of the previous one, which stalls streamed tokens.
                    if let Err(error) = stream.set_nodelay(true) {
                        warn!(error = %error, error_kind = "connection", "Inference connection kept its TCP delay");
                    }
                    let parent = span.clone();
                    let router = router.clone();
                    let deadline = tokio::time::Instant::now() + CONNECTION_TIMEOUT;
                    connections.spawn(async move {
                        let result = real_time::bound_until(
                            deadline,
                            crate::router::http::handle_inference_connection(
                                stream, router, permit, deadline,
                            ),
                        )
                        .await;
                        record_connection_result(result);
                    }.instrument(parent));
                }
            }
        }
    })
}

fn record_connection_result<E>(result: Result<Result<(), E>, tokio::time::error::Elapsed>)
where
    E: std::fmt::Display,
{
    match result {
        Ok(Ok(())) => {}
        Ok(Err(error)) => {
            warn!(error = %error, error_kind = "connection", "Inference connection failed");
        }
        Err(_) => {
            warn!(error_kind = "timeout", "Inference connection timed out");
        }
    }
}

fn record_connection_task_result(result: Result<(), tokio::task::JoinError>) {
    if let Err(error) = result {
        warn!(error = %error, error_kind = "connection_task", "Inference connection task failed");
    }
}

async fn stop(
    connections: &mut tokio::task::JoinSet<()>,
    span: &tracing::Span,
) -> ServerResult<()> {
    connections.abort_all();
    while connections.join_next().await.is_some() {}
    span.record("outcome", "stopped");
    Ok(())
}

#[cfg(test)]
mod tests {

    use dotcore_domain::test_support::tracing::capture_records_async;

    #[test]
    fn private_inference_limits_remain_pinned() {
        assert_eq!(super::MAX_CONCURRENT_CONNECTIONS, 4);
        assert_eq!(
            super::CONNECTION_TIMEOUT,
            std::time::Duration::from_secs(600)
        );
    }

    #[tokio::test]
    async fn connection_terminal_failures_are_recorded() {
        let (_, _, output) = capture_records_async(|| async {
            super::record_connection_result(Ok(Err(std::io::Error::other("socket unavailable"))));

            let timeout = tokio::time::timeout(
                std::time::Duration::ZERO,
                std::future::pending::<Result<(), std::io::Error>>(),
            )
            .await;
            super::record_connection_result(timeout);

            let task = tokio::spawn(async { panic!("connection task panic") });
            super::record_connection_task_result(task.await);
        })
        .await;

        assert!(output.contains("Inference connection failed"));
        assert!(output.contains("socket unavailable"));
        assert!(output.contains("Inference connection timed out"));
        assert!(output.contains("Inference connection task failed"));
        assert!(output.contains("connection task panic"));
    }
}
