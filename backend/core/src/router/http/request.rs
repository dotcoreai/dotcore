//! Bounded HTTP/1.1 inference request parsing.

use std::io;

use tokio::{io::AsyncReadExt, sync::watch};

use super::{errors::OpenAiBodyReadError, watchers::wait_for_request_cancellation};

/// Largest public OpenAI-compatible request body the inference listener accepts.
pub(crate) const MAX_OPENAI_REQUEST_BODY_BYTES: usize = 32 * 1024 * 1024;

#[derive(Debug)]
pub(super) struct OpenAiRequestHead {
    pub(super) method: String,
    pub(super) path: String,
    content_length: usize,
    buffered_body: Vec<u8>,
}

pub(super) async fn read_openai_request_head(
    reader: &mut tokio::net::tcp::OwnedReadHalf,
    cancellation: &mut watch::Receiver<
        Option<crate::router::cancellation::RequestCancellationReason>,
    >,
) -> Result<OpenAiRequestHead, OpenAiBodyReadError> {
    const MAX_REQUEST_HEADER_BYTES: usize = 64 * 1024;
    let mut bytes = Vec::new();
    let mut buffer = [0_u8; 4096];
    let header_end = loop {
        let read = tokio::select! {
            biased;
            reason = wait_for_request_cancellation(cancellation) => return Err(OpenAiBodyReadError::Cancelled(reason)),
            result = reader.read(&mut buffer) => result.map_err(OpenAiBodyReadError::io)?,
        };
        if read == 0 {
            return Err(OpenAiBodyReadError::io(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "OpenAI request ended before HTTP headers",
            )));
        }
        if bytes.len() + read > MAX_REQUEST_HEADER_BYTES {
            return Err(OpenAiBodyReadError::io(io::Error::new(
                io::ErrorKind::InvalidData,
                "OpenAI request exceeds the size limit",
            )));
        }
        bytes.extend_from_slice(&buffer[..read]);
        if let Some(end) = bytes.windows(4).position(|window| window == b"\r\n\r\n") {
            break end;
        }
    };
    parse_openai_request_head(bytes, header_end)
}

pub(super) async fn read_openai_request_body(
    reader: &mut tokio::net::tcp::OwnedReadHalf,
    head: &OpenAiRequestHead,
    cancellation: &mut watch::Receiver<
        Option<crate::router::cancellation::RequestCancellationReason>,
    >,
) -> Result<Vec<u8>, OpenAiBodyReadError> {
    let mut body = head.buffered_body.clone();
    let mut buffer = [0_u8; 4096];
    while body.len() < head.content_length {
        let read = tokio::select! {
            biased;
            reason = wait_for_request_cancellation(cancellation) => return Err(OpenAiBodyReadError::Cancelled(reason)),
            result = reader.read(&mut buffer) => result.map_err(OpenAiBodyReadError::io)?,
        };
        if read == 0 {
            return Err(OpenAiBodyReadError::io(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "OpenAI request ended before its body",
            )));
        }
        if body.len() + read > MAX_OPENAI_REQUEST_BODY_BYTES {
            return Err(OpenAiBodyReadError::TooLarge);
        }
        if body.len() + read > head.content_length {
            return Err(OpenAiBodyReadError::io(io::Error::new(
                io::ErrorKind::InvalidData,
                "OpenAI request has bytes after its declared body",
            )));
        }
        body.extend_from_slice(&buffer[..read]);
    }
    Ok(body)
}

pub(super) fn path_component(target: &str) -> &str {
    target.split_once('?').map_or(target, |(path, _)| path)
}

pub(super) fn parse_openai_request_head(
    bytes: Vec<u8>,
    header_end: usize,
) -> Result<OpenAiRequestHead, OpenAiBodyReadError> {
    let header = std::str::from_utf8(&bytes[..header_end]).map_err(|error| {
        OpenAiBodyReadError::source("OpenAI request headers are not UTF-8", error)
    })?;
    let mut lines = header.split("\r\n");
    let request = lines.next().ok_or_else(|| {
        OpenAiBodyReadError::io(io::Error::new(
            io::ErrorKind::InvalidData,
            "OpenAI request has no request line",
        ))
    })?;
    let mut parts = request.split_whitespace();
    let method = parts.next().unwrap_or_default().to_owned();
    let path = parts.next().unwrap_or_default().to_owned();
    let version = parts.next().unwrap_or_default();
    if method.is_empty() || path.is_empty() || version != "HTTP/1.1" || parts.next().is_some() {
        return Err(OpenAiBodyReadError::io(io::Error::new(
            io::ErrorKind::InvalidData,
            "OpenAI request line is invalid",
        )));
    }
    let content_length = lines
        .find_map(|line| {
            line.split_once(':')
                .filter(|(name, _)| name.eq_ignore_ascii_case("content-length"))
                .map(|(_, value)| value.trim())
        })
        .map(|value| {
            value.parse::<usize>().map_err(|error| {
                OpenAiBodyReadError::source("OpenAI content length is invalid", error)
            })
        })
        .transpose()?
        .unwrap_or(0);
    if content_length > MAX_OPENAI_REQUEST_BODY_BYTES {
        return Err(OpenAiBodyReadError::TooLarge);
    }
    let body_start = header_end + 4;
    if bytes.len() - body_start > MAX_OPENAI_REQUEST_BODY_BYTES {
        return Err(OpenAiBodyReadError::TooLarge);
    }
    if bytes.len() - body_start > content_length {
        return Err(OpenAiBodyReadError::io(io::Error::new(
            io::ErrorKind::InvalidData,
            "OpenAI request has bytes after its declared body",
        )));
    }
    Ok(OpenAiRequestHead {
        method,
        path,
        content_length,
        buffered_body: bytes[body_start..].to_vec(),
    })
}
