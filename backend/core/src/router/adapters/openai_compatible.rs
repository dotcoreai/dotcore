//! Owned reqwest transport for OpenAI-compatible upstream model targets.
//!
//! Each request makes exactly one provider call with no redirects or retries, under the
//! provider's private model id and credentials. It always asks the provider not to store the
//! request, since Dotcore itself stores none.

use dotcore_proto::dotcore::inference::v1::{
    ChatRequest, ChatStreamCompleted, ChatStreamEvent, chat_stream_event::Event,
};
use futures_util::StreamExt;
use tokio::sync::mpsc;

use super::{AdapterError, AdapterResponse, AdapterTransportError, UpstreamResponse};
use crate::{
    model::controller::serving_targets::UpstreamServingTarget,
    router::{cancellation::RequestCancellation, openai_compatible_codec as codec},
};

const MAX_RESPONSE_BYTES: usize = 1024 * 1024;
pub(super) const MAX_SSE_FRAME_BYTES: usize = 1024 * 1024;

/// Dispatches exactly one OpenAI-compatible Chat request for an eligible target.
pub(crate) async fn invoke(
    target: UpstreamServingTarget,
    request: ChatRequest,
    mut cancellation: RequestCancellation,
) -> Result<AdapterResponse, AdapterError> {
    validate_capabilities(&target, &request)?;
    let body = request_body(&target, &request)?;
    let url = chat_completions_url(target.dispatch().base_url.clone())?;
    let client = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .retry(reqwest::retry::never())
        .build()
        .map_err(transport_error)?;
    let mut outbound = client.post(url);
    if !target.dispatch().api_key.is_empty() {
        outbound = outbound.header(
            "authorization",
            format!("Bearer {}", target.dispatch().api_key),
        );
    }
    let outbound = outbound
        .header("content-type", "application/json")
        .header("accept", "application/json, text/event-stream")
        .body(body);
    let response = tokio::select! {
        biased;
        reason = cancellation.cancelled() => return Err(AdapterError::Cancelled(reason)),
        result = outbound.send() => result.map_err(transport_error)?,
    };
    let status = response.status().as_u16();
    let headers = response_headers(response.headers());
    if !(200..300).contains(&status) {
        return Ok(AdapterResponse::Upstream(UpstreamResponse {
            status,
            headers,
            body: read_body(response, &mut cancellation).await?,
        }));
    }
    if request.stream.unwrap_or(false) {
        return stream(response, request, cancellation).await;
    }
    let response = read_body(response, &mut cancellation).await?;
    codec::decode_completion(&response)
        .map_err(|error| AdapterError::from(AdapterTransportError::CompletionDecode(error)))
        .map(|mut completion| {
            completion.model_id = request.model_id;
            AdapterResponse::Complete(completion)
        })
}

fn validate_capabilities(
    target: &UpstreamServingTarget,
    request: &ChatRequest,
) -> Result<(), AdapterError> {
    if !target.supports_text {
        return Err(AdapterError::UnsupportedCapability);
    }
    let has_images = request.messages.iter().any(|message| {
        message.content_parts.iter().any(|part| {
            matches!(
                part.content,
                Some(
                    dotcore_proto::dotcore::inference::v1::chat_content_part::Content::ImageContent(
                        _
                    )
                )
            )
        })
    });
    if has_images && !target.supports_image {
        return Err(AdapterError::UnsupportedCapability);
    }
    Ok(())
}

fn request_body(
    target: &UpstreamServingTarget,
    request: &ChatRequest,
) -> Result<Vec<u8>, AdapterError> {
    // Providers may store requests by default; Dotcore never opts into that storage.
    let mut request = request.clone();
    request.store.get_or_insert(false);
    codec::encode_request_for_model(&request, &target.dispatch().upstream_model_id)
        .map_err(AdapterError::InvalidRequest)
}

fn chat_completions_url(mut base_url: url::Url) -> Result<url::Url, AdapterError> {
    base_url
        .path_segments_mut()
        .map_err(|_| invalid_response("OpenAI-compatible endpoint cannot accept a Chat path"))?
        .pop_if_empty()
        .extend(["chat", "completions"]);
    Ok(base_url)
}

async fn read_body(
    response: reqwest::Response,
    cancellation: &mut RequestCancellation,
) -> Result<Vec<u8>, AdapterError> {
    let mut body = Vec::new();
    let mut stream = response.bytes_stream();
    loop {
        let chunk = tokio::select! {
            biased;
            reason = cancellation.cancelled() => return Err(AdapterError::Cancelled(reason)),
            chunk = stream.next() => chunk,
        };
        let Some(chunk) = chunk else {
            return Ok(body);
        };
        let chunk = chunk.map_err(transport_error)?;
        append_bounded(&mut body, &chunk, MAX_RESPONSE_BYTES)?;
    }
}

async fn stream(
    response: reqwest::Response,
    request: ChatRequest,
    mut cancellation: RequestCancellation,
) -> Result<AdapterResponse, AdapterError> {
    let (sender, receiver) = mpsc::channel(8);
    tokio::spawn(async move {
        let result = forward_stream(response, request, &mut cancellation, &sender).await;
        if let Err(error) = result {
            let _ = sender.send(Err(error)).await;
        }
    });
    Ok(AdapterResponse::Stream(receiver))
}

async fn forward_stream(
    response: reqwest::Response,
    request: ChatRequest,
    cancellation: &mut RequestCancellation,
    sender: &mpsc::Sender<Result<ChatStreamEvent, AdapterError>>,
) -> Result<(), AdapterError> {
    let mut body = response.bytes_stream();
    let mut pending = Vec::new();
    let mut done = false;
    let include_usage = request.include_stream_usage.unwrap_or(false);
    let mut validator = codec::StreamValidator::new(include_usage);
    loop {
        while !done {
            let Some((end, separator_length)) = sse_frame_end(&pending) else {
                break;
            };
            if end > MAX_SSE_FRAME_BYTES {
                return Err(size_limit(
                    "OpenAI-compatible SSE frame exceeds the size limit",
                ));
            }
            let frame = std::str::from_utf8(&pending[..end])
                .map_err(|error| AdapterError::from(AdapterTransportError::Utf8(error)))?;
            let data = frame.lines().find_map(|line| {
                line.strip_prefix("data:")
                    .map(str::trim_start)
                    .map(str::to_owned)
            });
            pending.drain(..end + separator_length);
            let Some(data) = data else {
                continue;
            };
            if data == "[DONE]" {
                done = true;
                break;
            }
            for mut event in codec::decode_sse_frame(&data)
                .map_err(|error| AdapterError::from(AdapterTransportError::StreamDecode(error)))?
            {
                if validator.apply(&event).map_err(|error| {
                    AdapterError::from(AdapterTransportError::StreamPolicy(error))
                })? == codec::StreamAction::Filter
                {
                    continue;
                }
                if let Some(Event::Started(started)) = &mut event.event {
                    started.model_id = request.model_id.clone();
                }
                tokio::select! {
                    biased;
                    reason = cancellation.cancelled() => return Err(AdapterError::Cancelled(reason)),
                    result = sender.send(Ok(event)) => {
                        if result.is_err() {
                            return Ok(());
                        }
                    }
                }
            }
        }
        if done && pending.iter().any(|byte| !byte.is_ascii_whitespace()) {
            return Err(invalid_response(
                "OpenAI-compatible stream emitted data after [DONE]",
            ));
        }
        if !done && pending.len() > MAX_SSE_FRAME_BYTES {
            return Err(size_limit(
                "OpenAI-compatible SSE frame exceeds the size limit",
            ));
        }
        let chunk = tokio::select! {
            biased;
            reason = cancellation.cancelled() => return Err(AdapterError::Cancelled(reason)),
            chunk = body.next() => chunk,
        };
        let Some(chunk) = chunk else {
            break;
        };
        let chunk = chunk.map_err(transport_error)?;
        if done {
            if chunk.iter().any(|byte| !byte.is_ascii_whitespace()) {
                return Err(invalid_response(
                    "OpenAI-compatible stream emitted data after [DONE]",
                ));
            }
            continue;
        }
        pending.extend_from_slice(&chunk);
    }
    validator
        .finish(done, include_usage)
        .map_err(|error| AdapterError::from(AdapterTransportError::StreamPolicy(error)))?;
    tokio::select! {
        biased;
        reason = cancellation.cancelled() => Err(AdapterError::Cancelled(reason)),
        result = sender.send(Ok(ChatStreamEvent { event: Some(Event::Completed(ChatStreamCompleted {})) })) => {
            result.map_err(|_| invalid_response("OpenAI-compatible stream receiver closed"))
        }
    }
}

#[cfg(test)]
fn has_complete_sse_frame(pending: &[u8]) -> bool {
    sse_frame_end(pending).is_some()
}

fn append_bounded(
    destination: &mut Vec<u8>,
    chunk: &[u8],
    limit: usize,
) -> Result<(), AdapterError> {
    if destination.len().saturating_add(chunk.len()) > limit {
        return Err(size_limit(
            "OpenAI-compatible response exceeds the size limit",
        ));
    }
    destination.extend_from_slice(chunk);
    Ok(())
}

fn sse_frame_end(pending: &[u8]) -> Option<(usize, usize)> {
    pending
        .windows(2)
        .position(|window| window == b"\n\n")
        .map(|end| (end, 2))
        .or_else(|| {
            pending
                .windows(4)
                .position(|window| window == b"\r\n\r\n")
                .map(|end| (end, 4))
        })
}

fn response_headers(headers: &reqwest::header::HeaderMap) -> Vec<(String, String)> {
    headers
        .iter()
        .filter_map(|(name, value)| {
            value
                .to_str()
                .ok()
                .map(|value| (name.to_string(), value.to_owned()))
        })
        .collect()
}

fn transport_error(error: reqwest::Error) -> AdapterError {
    AdapterError::Upstream(
        anyhow::Error::new(error).context("OpenAI-compatible transport request failed"),
    )
}

fn invalid_response(message: &'static str) -> AdapterError {
    AdapterError::from(AdapterTransportError::InvalidResponse(anyhow::anyhow!(
        message
    )))
}

fn size_limit(message: &'static str) -> AdapterError {
    AdapterError::from(AdapterTransportError::Limit(message))
}

#[cfg(test)]
mod tests {
    use dotcore_proto::dotcore::inference::v1::{ChatMessage, ChatMessageRole, chat_message};

    use super::*;

    #[test]
    fn bounds_complete_responses_and_each_unterminated_sse_frame() {
        let mut response = vec![0; MAX_RESPONSE_BYTES];
        let error = append_bounded(&mut response, &[0], MAX_RESPONSE_BYTES)
            .expect_err("oversized upstream response");
        assert!(error.to_string().contains("exceeds the size limit"));

        let frame = vec![b'x'; MAX_SSE_FRAME_BYTES / 2];
        let pending = [frame.as_slice(), b"\n\n", frame.as_slice(), b"\n\n"].concat();
        assert!(has_complete_sse_frame(&pending));
        assert!(
            pending.len() > MAX_SSE_FRAME_BYTES,
            "the test must contain more than one frame's limit"
        );
    }

    #[test]
    fn upstream_providers_receive_instruction_roles_and_reasoning_unchanged() {
        let message = |role: ChatMessageRole, content: &str| ChatMessage {
            role: role as i32,
            content: Some(chat_message::Content::TextContent(content.to_owned())),
            ..Default::default()
        };
        let request = ChatRequest {
            model_id: "route".to_owned(),
            messages: vec![
                message(ChatMessageRole::System, "You are Codex."),
                message(ChatMessageRole::Developer, "Sandbox: read-only."),
                message(ChatMessageRole::User, "hi"),
                ChatMessage {
                    reasoning: Some("Say hello.".to_owned()),
                    ..message(ChatMessageRole::Assistant, "hello")
                },
                message(ChatMessageRole::Developer, "Be brief."),
            ],
            ..Default::default()
        };

        let body = request_body(
            &crate::model::controller::serving_targets::upstream_target_for_test("upstream"),
            &request,
        )
        .expect("upstream body");

        assert_eq!(
            serde_json::from_slice::<serde_json::Value>(&body).expect("body JSON")["messages"],
            serde_json::json!([
                {"role":"system","content":"You are Codex."},
                {"role":"developer","content":"Sandbox: read-only."},
                {"role":"user","content":"hi"},
                {"role":"assistant","content":"hello","reasoning":"Say hello."},
                {"role":"developer","content":"Be brief."}
            ])
        );
    }

    #[test]
    fn chat_completions_path_extends_the_configured_base_path() {
        for base_url in [
            "https://provider.example/api/v1",
            "https://provider.example/api/v1/",
        ] {
            assert_eq!(
                chat_completions_url(base_url.parse().expect("base URL"))
                    .expect("Chat URL")
                    .as_str(),
                "https://provider.example/api/v1/chat/completions"
            );
        }
    }
}
