//! Typed inference HTTP request and response failures.

use std::io;

#[derive(Debug, thiserror::Error)]
pub enum ConnectionError {
    #[error("inference request failed: {0}")]
    Request(#[source] OpenAiBodyReadError),

    #[error("inference response write failed: {0}")]
    Write(#[source] OpenAiWriteError),
}

#[derive(Debug, thiserror::Error)]
pub enum OpenAiWriteError {
    #[error("OpenAI response write failed: {0}")]
    Io(#[source] anyhow::Error),

    #[error("OpenAI response encoding failed: {0}")]
    Encoding(#[source] anyhow::Error),
}

impl OpenAiWriteError {
    pub(super) fn io(error: io::Error) -> Self {
        Self::Io(anyhow::Error::new(error))
    }

    pub(super) fn encoding(error: impl Into<anyhow::Error>) -> Self {
        Self::Encoding(error.into())
    }
}

#[derive(Debug, thiserror::Error)]
pub enum OpenAiBodyReadError {
    #[error("OpenAI request body I/O failed: {0}")]
    Io(#[source] anyhow::Error),

    #[error("OpenAI request was cancelled: {0}")]
    Cancelled(crate::router::cancellation::RequestCancellationReason),

    #[error("OpenAI request body exceeds the size limit")]
    TooLarge,
}

impl OpenAiBodyReadError {
    pub(super) fn io(error: io::Error) -> Self {
        Self::Io(anyhow::Error::new(error))
    }

    pub(super) fn source(message: &'static str, source: impl Into<anyhow::Error>) -> Self {
        Self::Io(source.into().context(message))
    }
}
