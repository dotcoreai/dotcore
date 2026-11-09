//! Inference request timeout and client-disconnect watchers.

use dotcore_domain::real_time;
use tokio::{io::AsyncReadExt, sync::watch};
use tracing::{Instrument, Span};

use super::errors::{ConnectionError, OpenAiWriteError};
use crate::router::cancellation::RequestCancellationReason;

pub(super) async fn wait_for_request_cancellation(
    cancellation: &mut watch::Receiver<Option<RequestCancellationReason>>,
) -> RequestCancellationReason {
    loop {
        if let Some(reason) = *cancellation.borrow() {
            return reason;
        }
        let _ = cancellation.changed().await;
    }
}

pub(super) struct RequestWatchers {
    sender: watch::Sender<Option<RequestCancellationReason>>,
    client: Option<tokio::task::JoinHandle<()>>,
    timeout: Option<tokio::task::JoinHandle<()>>,
}

impl RequestWatchers {
    pub(super) fn new() -> Self {
        let (sender, _) = watch::channel(None);
        Self {
            sender,
            client: None,
            timeout: None,
        }
    }

    pub(super) fn start_client(&mut self, reader: tokio::net::tcp::OwnedReadHalf) {
        let sender = self.sender.clone();
        self.client = Some(tokio::spawn(
            async move {
                let mut reader = reader;
                let mut byte = [0_u8; 1];
                let _ = reader.read(&mut byte).await;
                signal(&sender, RequestCancellationReason::ClientDisconnected);
            }
            .instrument(Span::current()),
        ));
    }

    pub(super) fn subscribe(&self) -> watch::Receiver<Option<RequestCancellationReason>> {
        self.sender.subscribe()
    }

    pub(super) fn start_timeout(&mut self, deadline: tokio::time::Instant) {
        let sender = self.sender.clone();
        self.timeout = Some(tokio::spawn(
            async move {
                real_time::deadline(deadline).await;
                signal(&sender, RequestCancellationReason::RequestTimedOut);
            }
            .instrument(Span::current()),
        ));
    }

    pub(super) fn signal_write_failure(&self) {
        signal(&self.sender, RequestCancellationReason::ClientDisconnected);
    }

    pub(super) fn finish_write(
        &self,
        result: Result<(), OpenAiWriteError>,
    ) -> Result<(), ConnectionError> {
        if result.is_err() {
            self.signal_write_failure();
        }
        result.map_err(ConnectionError::Write)
    }
}

impl Drop for RequestWatchers {
    fn drop(&mut self) {
        if let Some(client) = &self.client {
            client.abort();
        }
        if let Some(timeout) = &self.timeout {
            timeout.abort();
        }
    }
}

fn signal(
    sender: &watch::Sender<Option<RequestCancellationReason>>,
    reason: RequestCancellationReason,
) {
    sender.send_if_modified(|current| {
        if current.is_some() {
            false
        } else {
            *current = Some(reason);
            true
        }
    });
}
