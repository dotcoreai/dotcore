//! Loopback HTTP binding shared by the local engines.
//!
//! One invoker per engine service sends each request kind the engine serves to that kind's
//! transport on the engine's loopback port, and refuses any other kind before contacting the
//! engine. Transports post one JSON request and read the answer back, plain or chunked, within
//! fixed size limits. Every wait observes the request's cancellation, and a failure keeps whether
//! the engine could have started work: only a refused connection proves it did not.

use std::sync::Arc;

use anyhow::anyhow;
use dotcore_proto::dotcore::{
    catalog::v1::InferenceRequestKind,
    inference::v1::{ChatRequest, DecisionRequest},
};
use http::{HeaderName, HeaderValue};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpStream,
};

use super::{openai_compatible_transport, system_one_transport};
use crate::{
    inference::{
        DecisionResponse, EngineInvoker, InferenceAttempt, InferenceAttemptDisposition,
        InferenceCancellation, InferenceError, InferenceFuture, InferencePassthroughResponse,
        InvalidRequest,
    },
    router::{
        adapters::{
            AdapterAttemptDisposition, AdapterError, AdapterTransportError as TransportError,
            UpstreamResponse, failure_disposition,
        },
        cancellation::RequestCancellation,
    },
};

/// Endpoint-bound engine invoker for one local engine service.
pub(crate) struct LoopbackInvoker {
    port: u16,
    served_model_id: String,

    /// The request kinds the engine serves, from its engine definition.
    request_kinds: &'static [InferenceRequestKind],

    /// The engine's classifier of decision rejections, when it recognizes any.
    decision_rejections: Option<RejectionClassifier>,
}

impl LoopbackInvoker {
    /// Binds an engine listening on a loopback port that serves the model as `served_model_id`
    /// and answers the `request_kinds` its engine serves. Decision rejections the engine's
    /// `decision_rejections` classifier recognizes fail as invalid requests instead of passing
    /// through.
    pub(crate) fn bind(
        port: u16,
        served_model_id: String,
        request_kinds: &'static [InferenceRequestKind],
        decision_rejections: Option<RejectionClassifier>,
    ) -> Arc<dyn EngineInvoker> {
        Arc::new(Self {
            port,
            served_model_id,
            request_kinds,
            decision_rejections,
        })
    }

    fn serves(&self, kind: InferenceRequestKind) -> bool {
        self.request_kinds.contains(&kind)
    }
}

impl EngineInvoker for LoopbackInvoker {
    fn invoke<'a>(
        &'a self,
        request: ChatRequest,
        cancellation: InferenceCancellation,
    ) -> InferenceFuture<'a, InferenceAttempt> {
        if !self.serves(InferenceRequestKind::Chat) {
            return Box::pin(std::future::ready(
                InferenceAttempt::unsupported_request_kind(),
            ));
        }
        Box::pin(openai_compatible_transport::attempt(
            self.port,
            &self.served_model_id,
            request,
            cancellation,
        ))
    }

    fn invoke_decision<'a>(
        &'a self,
        request: DecisionRequest,
        cancellation: InferenceCancellation,
    ) -> InferenceFuture<'a, InferenceAttempt<DecisionResponse>> {
        if !self.serves(InferenceRequestKind::Decision) {
            return Box::pin(std::future::ready(
                InferenceAttempt::unsupported_request_kind(),
            ));
        }
        Box::pin(system_one_transport::attempt(
            self.port,
            &self.served_model_id,
            request,
            self.decision_rejections,
            cancellation,
        ))
    }
}

/// Recognizes an engine's unsuccessful response, from its status and body, as a platform invalid
/// request the client must fix. Responses it does not recognize keep passing through.
pub(crate) type RejectionClassifier = fn(u16, &[u8]) -> Option<InvalidRequest>;

/// Converts one transport result into the engine-neutral attempt a binding returns, keeping
/// whether its failure may fall back.
pub(super) fn inference_attempt<R>(result: Result<R, AdapterError>) -> InferenceAttempt<R> {
    match result {
        Ok(response) => InferenceAttempt::Response(response),
        Err(error) => InferenceAttempt::failed(
            match failure_disposition(&error) {
                AdapterAttemptDisposition::SafeToFallback => {
                    InferenceAttemptDisposition::SafeToFallback
                }
                AdapterAttemptDisposition::StartedOrAmbiguous => {
                    InferenceAttemptDisposition::StartedOrAmbiguous
                }
                AdapterAttemptDisposition::NonRetryable => {
                    InferenceAttemptDisposition::NonRetryable
                }
            },
            inference_error(error),
        ),
    }
}

/// Keeps an unsuccessful engine response unchanged for the router to pass through.
pub(super) fn passthrough(response: UpstreamResponse) -> InferencePassthroughResponse {
    InferencePassthroughResponse {
        status: response.status,
        headers: response.headers,
        body: response.body,
    }
}

/// The engine-neutral error of a failed transport exchange.
pub(super) fn inference_error(error: AdapterError) -> InferenceError {
    match error {
        AdapterError::Cancelled(reason) => InferenceError::Cancelled(reason),
        AdapterError::Upstream(error) | AdapterError::PreDispatchUnavailable(error) => {
            InferenceError::Provider(error)
        }
        AdapterError::UnsupportedCapability => InferenceError::UnsupportedCapability,
        AdapterError::InvalidRequest(error) => InferenceError::InvalidRequest(error),
    }
}

/// Connects to the engine on `port` and sends one JSON `body` to `path`, accepting `accept`.
///
/// A refused connection is unavailable before dispatch; any later failure may follow engine work.
pub(super) async fn post(
    port: u16,
    path: &str,
    accept: &str,
    body: &[u8],
    cancellation: &mut RequestCancellation,
) -> Result<TcpStream, AdapterError> {
    let mut stream = tokio::select! {
        biased;
        reason = cancellation.cancelled() => return Err(AdapterError::Cancelled(reason)),
        result = TcpStream::connect(("127.0.0.1", port)) => result.map_err(|error| AdapterError::PreDispatchUnavailable(anyhow::Error::new(TransportError::Connect(error))))?,
    };
    let request_head = format!(
        "POST {path} HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nContent-Type: application/json\r\nAccept: {accept}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
    );
    tokio::select! {
        biased;
        reason = cancellation.cancelled() => return Err(AdapterError::Cancelled(reason)),
        result = stream.write_all(request_head.as_bytes()) => result.map_err(|error| upstream_transport(TransportError::Io(error)))?,
    }
    tokio::select! {
        biased;
        reason = cancellation.cancelled() => return Err(AdapterError::Cancelled(reason)),
        result = stream.write_all(body) => result.map_err(|error| upstream_transport(TransportError::Io(error)))?,
    }
    Ok(stream)
}

/// Wraps a transport failure after the connection as a started upstream failure.
pub(super) fn upstream_transport(error: TransportError) -> AdapterError {
    AdapterError::Upstream(anyhow::Error::new(error))
}

/// An engine response that breaks the HTTP or wire contract.
pub(super) fn invalid_response(message: &'static str) -> AdapterError {
    upstream_transport(TransportError::InvalidResponse(anyhow!(message)))
}

pub(super) async fn read_http_response(
    stream: &mut TcpStream,
    cancellation: &mut RequestCancellation,
) -> Result<(u16, Vec<(String, String)>, Vec<u8>), AdapterError> {
    const MAX_UPSTREAM_RESPONSE_BYTES: usize = 1024 * 1024;
    let (status, headers, mut body) = read_http_head(stream, cancellation).await?;
    if transfer_encoding(&headers)?.is_some() {
        body = read_chunked_body(stream, cancellation, body).await?;
    } else {
        read_remaining_body(stream, cancellation, &mut body).await?;
        if body.len() > MAX_UPSTREAM_RESPONSE_BYTES {
            return Err(upstream_transport(TransportError::Limit(
                "engine response exceeds the size limit",
            )));
        }
    }
    Ok((status, headers, body))
}

pub(super) async fn read_remaining_body(
    stream: &mut TcpStream,
    cancellation: &mut RequestCancellation,
    body: &mut Vec<u8>,
) -> Result<(), AdapterError> {
    const MAX_UPSTREAM_RESPONSE_BYTES: usize = 1024 * 1024;
    let mut buffer = [0; 8192];
    loop {
        let read = tokio::select! {
            biased;
            reason = cancellation.cancelled() => return Err(AdapterError::Cancelled(reason)),
            value = stream.read(&mut buffer) => value.map_err(|error| AdapterError::Upstream(error.into()))?,
        };
        if read == 0 {
            return Ok(());
        }
        if body.len() + read > MAX_UPSTREAM_RESPONSE_BYTES {
            return Err(upstream_transport(TransportError::Limit(
                "engine response exceeds the size limit",
            )));
        }
        body.extend_from_slice(&buffer[..read]);
    }
}

pub(super) fn transfer_encoding(headers: &[(String, String)]) -> Result<Option<()>, AdapterError> {
    let encodings = headers
        .iter()
        .filter(|(name, _)| name.eq_ignore_ascii_case("transfer-encoding"))
        .flat_map(|(_, value)| value.split(','))
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .collect::<Vec<_>>();
    if encodings.is_empty() {
        return Ok(None);
    }
    if encodings.len() != 1 || !encodings[0].eq_ignore_ascii_case("chunked") {
        return Err(invalid_response(
            "engine response uses an unsupported transfer encoding",
        ));
    }
    Ok(Some(()))
}

pub(super) async fn read_chunked_body(
    stream: &mut TcpStream,
    cancellation: &mut RequestCancellation,
    encoded: Vec<u8>,
) -> Result<Vec<u8>, AdapterError> {
    let mut decoder = ChunkedBodyDecoder::new();
    let mut body = decoder.push(&encoded)?;
    loop {
        if decoder.is_finished() {
            return Ok(body);
        }
        let mut buffer = [0; 8192];
        let read = tokio::select! {
            biased;
            reason = cancellation.cancelled() => return Err(AdapterError::Cancelled(reason)),
            value = stream.read(&mut buffer) => value.map_err(|error| AdapterError::Upstream(error.into()))?,
        };
        if read == 0 {
            return Err(invalid_response(
                "engine chunked response body is truncated",
            ));
        }
        let decoded = decoder.push(&buffer[..read])?;
        if body.len() + decoded.len() > ChunkedBodyDecoder::MAX_CHUNK_BYTES {
            return Err(upstream_transport(TransportError::Limit(
                "engine response exceeds the size limit",
            )));
        }
        body.extend_from_slice(&decoded);
    }
}

pub(super) struct ChunkedBodyDecoder {
    encoded: Vec<u8>,
    finished: bool,
}

impl ChunkedBodyDecoder {
    const MAX_CHUNK_BYTES: usize = 1024 * 1024;
    const MAX_ENCODED_BUFFER_BYTES: usize = Self::MAX_CHUNK_BYTES + 64 * 1024;

    pub(super) fn new() -> Self {
        Self {
            encoded: Vec::new(),
            finished: false,
        }
    }

    pub(super) fn push(&mut self, bytes: &[u8]) -> Result<Vec<u8>, TransportError> {
        if self.finished && !bytes.is_empty() {
            return Err(TransportError::InvalidResponse(anyhow!(
                "engine chunked response has bytes after its body"
            )));
        }
        if self.encoded.len() + bytes.len() > Self::MAX_ENCODED_BUFFER_BYTES {
            return Err(TransportError::InvalidResponse(anyhow!(
                "engine chunk framing exceeds the size limit"
            )));
        }
        self.encoded.extend_from_slice(bytes);
        let mut decoded = Vec::new();
        while let Some(line_end) = self.encoded.windows(2).position(|value| value == b"\r\n") {
            let size = parse_chunk_size(&self.encoded[..line_end])?;
            let body_start = line_end + 2;
            if size == 0 {
                let trailer_end = chunked_trailer_end(&self.encoded, body_start);
                let Some(trailer_end) = trailer_end else {
                    break;
                };
                if trailer_end != self.encoded.len() {
                    return Err(TransportError::InvalidResponse(anyhow!(
                        "engine chunked response has bytes after its body"
                    )));
                }
                self.encoded.clear();
                self.finished = true;
                break;
            }
            if size > Self::MAX_CHUNK_BYTES {
                return Err(TransportError::InvalidResponse(anyhow!(
                    "engine chunk exceeds the size limit"
                )));
            }
            let chunk_end = body_start.checked_add(size).ok_or_else(|| {
                TransportError::InvalidResponse(anyhow!("engine chunk size is invalid"))
            })?;
            if self.encoded.len() < chunk_end + 2 {
                break;
            }
            if &self.encoded[chunk_end..chunk_end + 2] != b"\r\n" {
                return Err(TransportError::InvalidResponse(anyhow!(
                    "engine chunk is not terminated"
                )));
            }
            decoded.extend_from_slice(&self.encoded[body_start..chunk_end]);
            self.encoded.drain(..chunk_end + 2);
        }
        Ok(decoded)
    }

    pub(super) fn is_finished(&self) -> bool {
        self.finished
    }

    pub(super) fn finish(&self) -> Result<(), TransportError> {
        if self.finished {
            Ok(())
        } else {
            Err(TransportError::InvalidResponse(anyhow!(
                "engine chunked response body is truncated"
            )))
        }
    }
}

fn parse_chunk_size(bytes: &[u8]) -> Result<usize, TransportError> {
    let size_line = std::str::from_utf8(bytes)?;
    usize::from_str_radix(size_line.split(';').next().unwrap_or_default().trim(), 16)
        .map_err(TransportError::Integer)
}

fn chunked_trailer_end(encoded: &[u8], body_start: usize) -> Option<usize> {
    if encoded[body_start..].starts_with(b"\r\n") {
        Some(body_start + 2)
    } else {
        encoded[body_start..]
            .windows(4)
            .position(|value| value == b"\r\n\r\n")
            .map(|value| body_start + value + 4)
    }
}

pub(super) async fn read_http_head(
    stream: &mut TcpStream,
    cancellation: &mut RequestCancellation,
) -> Result<(u16, Vec<(String, String)>, Vec<u8>), AdapterError> {
    const MAX_UPSTREAM_HEADER_BYTES: usize = 64 * 1024;
    let mut response = Vec::new();
    loop {
        if let Some(end) = response.windows(4).position(|window| window == b"\r\n\r\n") {
            let text = std::str::from_utf8(&response[..end])
                .map_err(|error| upstream_transport(TransportError::Utf8(error)))?;
            let mut lines = text.split("\r\n");
            let status = lines
                .next()
                .and_then(|line| line.split_whitespace().nth(1))
                .ok_or_else(|| invalid_response("engine response has an invalid status"))?
                .parse::<u16>()
                .map_err(|error| upstream_transport(TransportError::Integer(error)))?;
            let headers = lines
                .map(parse_upstream_header)
                .collect::<Result<Vec<_>, _>>()?;
            return Ok((status, headers, response[end + 4..].to_vec()));
        }
        let mut buffer = [0; 4096];
        let read = tokio::select! {
            biased;
            reason = cancellation.cancelled() => return Err(AdapterError::Cancelled(reason)),
            value = stream.read(&mut buffer) => value.map_err(|error| AdapterError::Upstream(error.into()))?,
        };
        if read == 0 {
            return Err(invalid_response(
                "engine response ended before HTTP headers",
            ));
        }
        if response.len() + read > MAX_UPSTREAM_HEADER_BYTES {
            return Err(invalid_response(
                "engine response headers exceed the size limit",
            ));
        }
        response.extend_from_slice(&buffer[..read]);
    }
}

fn parse_upstream_header(line: &str) -> Result<(String, String), AdapterError> {
    let (name, value) = line
        .split_once(':')
        .ok_or_else(|| invalid_response("engine response has a malformed HTTP header"))?;
    let name = HeaderName::from_bytes(name.as_bytes())
        .map_err(|error| upstream_transport(TransportError::HeaderName(error)))?;
    let value = value.trim_matches([' ', '\t']);
    let value = HeaderValue::from_str(value)
        .map_err(|error| upstream_transport(TransportError::HeaderValue(error)))?;
    let value = value
        .to_str()
        .map_err(|error| upstream_transport(TransportError::HeaderText(error)))?;
    Ok((name.as_str().to_owned(), value.to_owned()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn upstream_header_parser_rejects_malformed_and_injectable_fields() {
        let malformed = parse_upstream_header("missing-colon").expect_err("missing colon");
        let AdapterError::Upstream(source) = &malformed else {
            panic!("missing colon must be an upstream-policy error");
        };
        let Some(TransportError::InvalidResponse(message)) = source.downcast_ref() else {
            panic!("missing colon must retain its typed transport source");
        };
        assert_eq!(
            message.to_string(),
            "engine response has a malformed HTTP header"
        );
        assert_eq!(
            malformed.to_string(),
            "inference adapter upstream failed: invalid adapter transport response: engine response has a malformed HTTP header"
        );
        assert!(source.chain().count() > 1);
        assert!(parse_upstream_header("bad name: value").is_err());
        assert!(parse_upstream_header("Retry-After: 7\r\nInjected: value").is_err());
        assert!(parse_upstream_header("Retry-After: 7\nInjected: value").is_err());
        assert!(parse_upstream_header("Retry-After: \u{000b}7").is_err());
        assert_eq!(
            parse_upstream_header("Retry-After: 7").expect("valid retry header"),
            ("retry-after".to_owned(), "7".to_owned())
        );
    }
}
