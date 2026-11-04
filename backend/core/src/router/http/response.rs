//! OpenAI-compatible HTTP response encoding and cancellable byte writing.

use std::io;

use http::{HeaderName, HeaderValue};
use tokio::io::{AsyncWrite, AsyncWriteExt};

use super::errors::OpenAiWriteError;

pub(super) async fn write_openai_response<W: AsyncWrite + Unpin>(
    writer: &mut W,
    response: crate::router::openai::OpenAiResponse,
) -> Result<(), OpenAiWriteError> {
    writer
        .write_all(&openai_response_bytes(response)?)
        .await
        .map_err(OpenAiWriteError::io)
}

/// Writes `response` unless the request is cancelled first, then answers the cancellation
/// instead. Reports how the write ended; a complete write stays complete whatever the client
/// does afterwards.
pub(super) async fn write_openai_response_cancellable<W: AsyncWrite + Unpin>(
    writer: &mut W,
    response: crate::router::openai::OpenAiResponse,
    mut cancellation: crate::router::cancellation::RequestCancellation,
) -> Result<ResponseWrite, OpenAiWriteError> {
    let response = openai_response_bytes(response)?;
    match write_bytes_cancellable(writer, &response, &mut cancellation).await? {
        CancellableWrite::Complete => Ok(ResponseWrite::Complete),
        CancellableWrite::CancelledAfterWrite => {
            Ok(ResponseWrite::Interrupted(cancellation.reason().unwrap_or(
                crate::router::cancellation::RequestCancellationReason::ClientDisconnected,
            )))
        }
        CancellableWrite::CancelledBeforeWrite(reason) => {
            let status = cancellation_answer(reason).status;
            write_cancelled_before_headers(writer, reason).await?;
            Ok(ResponseWrite::Replaced { reason, status })
        }
    }
}

/// How a cancellable response write ended.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum ResponseWrite {
    /// Every byte of the response was written.
    Complete,

    /// The request was cancelled before any byte, so the cancellation's answer of `status` was
    /// written instead.
    Replaced {
        reason: crate::router::cancellation::RequestCancellationReason,
        status: u16,
    },

    /// The request was cancelled after part of the response was written.
    Interrupted(crate::router::cancellation::RequestCancellationReason),
}

pub(super) async fn write_cancelled_before_headers<W: AsyncWrite + Unpin>(
    writer: &mut W,
    reason: crate::router::cancellation::RequestCancellationReason,
) -> Result<(), OpenAiWriteError> {
    write_openai_response(writer, cancellation_answer(reason)).await
}

/// The public answer to a request cancelled for `reason`.
pub(super) fn cancellation_answer(
    reason: crate::router::cancellation::RequestCancellationReason,
) -> crate::router::openai::OpenAiResponse {
    match reason {
        crate::router::cancellation::RequestCancellationReason::RouteRevoked => {
            crate::router::openai::route_unavailable()
        }
        crate::router::cancellation::RequestCancellationReason::ClientDisconnected => {
            crate::router::openai::cancelled()
        }
        crate::router::cancellation::RequestCancellationReason::RequestTimedOut => {
            crate::router::openai::timeout()
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum CancellableWrite {
    Complete,

    CancelledBeforeWrite(crate::router::cancellation::RequestCancellationReason),

    CancelledAfterWrite,
}

pub(super) async fn write_bytes_cancellable<W: AsyncWrite + Unpin>(
    writer: &mut W,
    bytes: &[u8],
    cancellation: &mut crate::router::cancellation::RequestCancellation,
) -> Result<CancellableWrite, OpenAiWriteError> {
    let mut written = 0;
    while written < bytes.len() {
        let count = tokio::select! { biased; reason = cancellation.cancelled() => return Ok(if written == 0 { CancellableWrite::CancelledBeforeWrite(reason) } else { CancellableWrite::CancelledAfterWrite }), result = writer.write(&bytes[written..]) => result.map_err(OpenAiWriteError::io)?, };
        if count == 0 {
            return Err(OpenAiWriteError::io(io::Error::new(
                io::ErrorKind::WriteZero,
                "OpenAI response writer stopped before all bytes were sent",
            )));
        }
        written += count;
    }
    Ok(CancellableWrite::Complete)
}

pub(super) fn openai_response_bytes(
    response: crate::router::openai::OpenAiResponse,
) -> Result<Vec<u8>, OpenAiWriteError> {
    let status = match response.status {
        200 => "200 OK".to_owned(),
        400 => "400 Bad Request".to_owned(),
        413 => "413 Payload Too Large".to_owned(),
        404 => "404 Not Found".to_owned(),
        499 => "499 Client Closed Request".to_owned(),
        502 => "502 Bad Gateway".to_owned(),
        504 => "504 Gateway Timeout".to_owned(),
        status => format!("{status} Upstream Response"),
    };
    let mut headers = format!(
        "HTTP/1.1 {status}\r\nContent-Type: {}\r\nContent-Length: {}\r\nConnection: close\r\n",
        response.content_type,
        response.body.len()
    );
    for (name, value) in response.passthrough_headers {
        let name = name.parse::<HeaderName>().map_err(|error| {
            OpenAiWriteError::encoding(
                anyhow::Error::new(error).context("invalid upstream header name"),
            )
        })?;
        let value = value.parse::<HeaderValue>().map_err(|error| {
            OpenAiWriteError::encoding(
                anyhow::Error::new(error).context("invalid upstream header value"),
            )
        })?;
        let value = value.to_str().map_err(|error| {
            OpenAiWriteError::encoding(
                anyhow::Error::new(error).context("non-ASCII upstream header value"),
            )
        })?;
        if is_safe_public_upstream_header(&name, value) {
            headers.push_str(name.as_str());
            headers.push_str(": ");
            headers.push_str(value);
            headers.push_str("\r\n");
        }
    }
    headers.push_str("\r\n");
    let mut bytes = headers.into_bytes();
    bytes.extend_from_slice(&response.body);
    Ok(bytes)
}

fn is_safe_public_upstream_header(name: &HeaderName, value: &str) -> bool {
    match name.as_str() {
        "retry-after" => value.parse::<u64>().is_ok() || is_imf_fixdate(value),
        "ratelimit-limit"
        | "ratelimit-remaining"
        | "x-ratelimit-limit"
        | "x-ratelimit-limit-requests"
        | "x-ratelimit-limit-tokens"
        | "x-ratelimit-remaining"
        | "x-ratelimit-remaining-requests"
        | "x-ratelimit-remaining-tokens" => value.parse::<u64>().is_ok(),
        "ratelimit-reset"
        | "x-ratelimit-reset"
        | "x-ratelimit-reset-requests"
        | "x-ratelimit-reset-tokens" => {
            value.parse::<u64>().is_ok() || is_rate_limit_duration(value)
        }
        _ => false,
    }
}

fn is_rate_limit_duration(value: &str) -> bool {
    let mut remaining = value;
    let mut segments = 0;
    while !remaining.is_empty() {
        let digits = remaining.bytes().take_while(u8::is_ascii_digit).count();
        if digits == 0 {
            return false;
        }
        remaining = &remaining[digits..];
        let length = if remaining.starts_with("ms") {
            2
        } else if matches!(
            remaining.as_bytes().first(),
            Some(b's' | b'm' | b'h' | b'd')
        ) {
            1
        } else {
            return false;
        };
        remaining = &remaining[length..];
        segments += 1;
    }
    segments > 0
}

fn is_imf_fixdate(value: &str) -> bool {
    let bytes = value.as_bytes();
    bytes.len() == 29
        && matches!(
            &bytes[0..3],
            b"Mon" | b"Tue" | b"Wed" | b"Thu" | b"Fri" | b"Sat" | b"Sun"
        )
        && &bytes[3..5] == b", "
        && bytes[5..7].iter().all(u8::is_ascii_digit)
        && bytes[7] == b' '
        && matches!(
            &bytes[8..11],
            b"Jan"
                | b"Feb"
                | b"Mar"
                | b"Apr"
                | b"May"
                | b"Jun"
                | b"Jul"
                | b"Aug"
                | b"Sep"
                | b"Oct"
                | b"Nov"
                | b"Dec"
        )
        && bytes[11] == b' '
        && bytes[12..16].iter().all(u8::is_ascii_digit)
        && bytes[16] == b' '
        && bytes[17..19].iter().all(u8::is_ascii_digit)
        && bytes[19] == b':'
        && bytes[20..22].iter().all(u8::is_ascii_digit)
        && bytes[22] == b':'
        && bytes[23..25].iter().all(u8::is_ascii_digit)
        && &bytes[25..29] == b" GMT"
}
