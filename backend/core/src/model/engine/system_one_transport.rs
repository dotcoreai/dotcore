//! Loopback decision transport answering through llama.cpp's System One endpoint.
//!
//! It sends one canonical decision request in the System One wire form under the engine's served
//! model id and decodes the answers back into the request's question order. An unsuccessful
//! engine response the engine's classifier recognizes fails as an invalid request; any other
//! passes through unchanged, as for chat. Only a refused connection is safe to fall back from.

use dotcore_proto::dotcore::inference::v1::DecisionRequest;

use tracing::debug;

use super::loopback_http::{
    self as http, RejectionClassifier, inference_attempt, passthrough, read_http_response,
    upstream_transport,
};
use crate::{
    inference::{DecisionResponse, InferenceAttempt, InferenceCancellation},
    router::{
        adapters::{AdapterError, AdapterTransportError as TransportError, UpstreamResponse},
        cancellation::RequestCancellation,
        systemone,
    },
};

/// Engine path serving System One decisions.
const SYSTEM_ONE_PATH: &str = "/v1/systemone";

/// Makes one decision attempt on the engine listening on `port`, which serves the model as
/// `served_model_id`, failing the rejections `classify` recognizes as invalid requests.
pub(super) async fn attempt(
    port: u16,
    served_model_id: &str,
    request: DecisionRequest,
    classify: Option<RejectionClassifier>,
    cancellation: InferenceCancellation,
) -> InferenceAttempt<DecisionResponse> {
    inference_attempt(
        invoke_system_one(port, served_model_id, &request, classify, cancellation).await,
    )
}

#[tracing::instrument(
    level = "debug",
    skip_all,
    fields(operation = "inference_upstream_decision")
)]
async fn invoke_system_one(
    port: u16,
    served_model_id: &str,
    request: &DecisionRequest,
    classify: Option<RejectionClassifier>,
    mut cancellation: RequestCancellation,
) -> Result<DecisionResponse, AdapterError> {
    let body = systemone::encode_request(request, served_model_id)
        .map_err(AdapterError::InvalidRequest)?;
    let mut stream = http::post(
        port,
        SYSTEM_ONE_PATH,
        "application/json",
        &body,
        &mut cancellation,
    )
    .await?;
    let (status, headers, body) = read_http_response(&mut stream, &mut cancellation).await?;
    if !(200..300).contains(&status) {
        if let Some(invalid) = classify.and_then(|classify| classify(status, &body)) {
            debug!(
                status,
                category = %invalid.category,
                "Engine decision rejection classified as an invalid request"
            );
            return Err(AdapterError::InvalidRequest(invalid));
        }
        return Ok(DecisionResponse::Passthrough(passthrough(
            UpstreamResponse {
                status,
                headers,
                body,
            },
        )));
    }
    systemone::decode_result(request, &body)
        .map(DecisionResponse::Complete)
        .map_err(|error| upstream_transport(TransportError::DecisionDecode(error)))
}

#[cfg(test)]
mod tests {
    use std::io;

    use dotcore_proto::dotcore::inference::v1::{
        DecisionChoice, DecisionContentPart, DecisionOption, DecisionPredicate, DecisionQuestion,
        DecisionScore, DecisionScoreLevel, DecisionUsage, decision_content_part::Content,
        decision_question::Kind,
    };
    use tokio::{
        io::{AsyncReadExt, AsyncWriteExt},
        net::TcpListener,
    };

    use super::*;
    use crate::inference::{InferenceAttemptDisposition, InferenceError, inference_cancellation};

    #[tokio::test]
    async fn a_decision_posts_system_one_and_returns_the_engine_answers() {
        let response = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{RESPONSE}",
            RESPONSE.len()
        );
        let (attempt, head, body) = invoke_fake(response).await;

        assert!(
            head.starts_with("POST /v1/systemone HTTP/1.1\r\n"),
            "{head}"
        );
        assert_eq!(
            body,
            systemone::encode_request(&request(vec![text("state")]), "served")
                .expect("encoded request")
        );
        let InferenceAttempt::Response(DecisionResponse::Complete(result)) = attempt else {
            panic!("the engine answers must be returned");
        };
        assert_eq!(result.answers.len(), 4);
        assert_eq!(result.usage, Some(DecisionUsage { input_tokens: 96 }));
    }

    #[tokio::test]
    async fn engine_rejections_pass_through_unchanged() {
        for (status, reason, error) in [
            (
                400,
                "Bad Request",
                r#"{"error":{"code":400,"message":"prompt exceeds the micro-batch","type":"invalid_request_error"}}"#,
            ),
            (
                501,
                "Not Implemented",
                r#"{"error":{"code":501,"message":"not a decision model","type":"not_supported_error"}}"#,
            ),
        ] {
            let response = format!(
                "HTTP/1.1 {status} {reason}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{error}",
                error.len()
            );

            let (attempt, _, _) = invoke_fake(response).await;

            let InferenceAttempt::Response(DecisionResponse::Passthrough(passthrough)) = attempt
            else {
                panic!("a {status} response must pass through");
            };
            assert_eq!(passthrough.status, status);
            assert_eq!(passthrough.body, error.as_bytes());
            assert!(
                passthrough
                    .headers
                    .contains(&("content-type".to_owned(), "application/json".to_owned()))
            );
        }
    }

    #[tokio::test]
    async fn a_state_of_several_parts_is_refused_before_contacting_the_engine() {
        let reserved = dotcore_domain::test_support::ports::reserve_port();
        let port = reserved.port();

        let attempt = attempt(
            port,
            "served",
            request(vec![text("ticket"), json(r#"{"id":7}"#)]),
            None,
            inference_cancellation().1,
        )
        .await;

        assert!(matches!(
            attempt,
            InferenceAttempt::Failed {
                disposition: InferenceAttemptDisposition::NonRetryable,
                error: InferenceError::InvalidRequest(error),
            } if error == crate::inference::InvalidRequest::unsupported_feature(Some("state"))
        ));
    }

    #[tokio::test]
    async fn a_refused_connection_is_safe_to_fall_back_from() {
        let reserved = dotcore_domain::test_support::ports::reserve_port();
        let port = reserved.port();

        let attempt = attempt(
            port,
            "served",
            request(vec![text("state")]),
            None,
            inference_cancellation().1,
        )
        .await;

        let InferenceAttempt::Failed {
            disposition: InferenceAttemptDisposition::SafeToFallback,
            error: InferenceError::Provider(source),
        } = attempt
        else {
            panic!("a refused connection must be safe to fall back from");
        };
        let Some(TransportError::Connect(error)) = source.downcast_ref() else {
            panic!("a refused connection keeps its transport source");
        };
        assert_eq!(error.kind(), io::ErrorKind::ConnectionRefused);
    }

    #[tokio::test]
    async fn an_undecodable_answer_after_the_engine_started_never_falls_back() {
        let response =
            "HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\n{}".to_owned();

        let (attempt, _, _) = invoke_fake(response).await;

        assert!(matches!(
            attempt,
            InferenceAttempt::Failed {
                disposition: InferenceAttemptDisposition::StartedOrAmbiguous,
                error: InferenceError::Provider(_),
            }
        ));
    }

    const RESPONSE: &str = concat!(
        r#"{"model":"served","answers":{"#,
        r#""severity":{"type":"score","score":0.625,"legend":{"0":"Minor","1":"Blocking"},"probabilities":{"0":0.375,"1":0.625},"confidence":0.25},"#,
        r#""annoyed":{"type":"noul","noul":0.25},"#,
        r#""category":{"type":"choice","choice":"refund","probabilities":{"refund":0.75,"other":0.25},"confidence":0.5},"#,
        r#""urgent":{"type":"noul","noul":0.875}"#,
        r#"},"usage":{"input_tokens":96,"output_tokens":0}}"#
    );

    /// Serves one canned `response` and returns the attempt with the request head and body the
    /// engine received.
    async fn invoke_fake(
        response: String,
    ) -> (InferenceAttempt<DecisionResponse>, String, Vec<u8>) {
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("engine listener");
        let port = listener.local_addr().expect("engine address").port();
        let engine = tokio::spawn(async move {
            let (mut connection, _) = listener.accept().await.expect("engine connection");
            let mut request = Vec::new();
            let mut buffer = [0; 4096];
            let (head, body) = loop {
                let read = connection.read(&mut buffer).await.expect("request bytes");
                request.extend_from_slice(&buffer[..read]);
                let Some(end) = request.windows(4).position(|window| window == b"\r\n\r\n") else {
                    continue;
                };
                let head = String::from_utf8(request[..end + 4].to_vec()).expect("request head");
                let length = head
                    .lines()
                    .find_map(|line| line.strip_prefix("Content-Length: "))
                    .and_then(|length| length.parse::<usize>().ok())
                    .expect("content length");
                if request.len() >= end + 4 + length {
                    break (head, request[end + 4..].to_vec());
                }
            };
            connection
                .write_all(response.as_bytes())
                .await
                .expect("engine response");
            (head, body)
        });

        let attempt = attempt(
            port,
            "served",
            request(vec![text("state")]),
            None,
            inference_cancellation().1,
        )
        .await;
        let (head, body) = engine.await.expect("engine task");
        (attempt, head, body)
    }

    fn request(state: Vec<DecisionContentPart>) -> DecisionRequest {
        DecisionRequest {
            model_id: "decisions".to_owned(),
            state,
            questions: vec![
                DecisionQuestion {
                    name: "urgent".to_owned(),
                    instructions: vec![text("Is it urgent?"), json(r#"{"sla":4}"#)],
                    kind: Some(Kind::Predicate(DecisionPredicate {
                        true_description: Some(text("Needs action today")),
                        false_description: None,
                    })),
                },
                DecisionQuestion {
                    name: "category".to_owned(),
                    instructions: vec![text("Pick the category.")],
                    kind: Some(Kind::Choice(DecisionChoice {
                        options: vec![
                            DecisionOption {
                                name: "refund".to_owned(),
                                description: Some(text("Money back")),
                            },
                            DecisionOption {
                                name: "other".to_owned(),
                                description: None,
                            },
                        ],
                    })),
                },
                DecisionQuestion {
                    name: "annoyed".to_owned(),
                    instructions: vec![text("Is the customer annoyed?")],
                    kind: Some(Kind::Predicate(DecisionPredicate::default())),
                },
                DecisionQuestion {
                    name: "severity".to_owned(),
                    instructions: vec![text("Rate the severity.")],
                    kind: Some(Kind::Score(DecisionScore {
                        levels: vec![
                            DecisionScoreLevel {
                                label: Some("low".to_owned()),
                                description: Some(text("Minor")),
                            },
                            DecisionScoreLevel {
                                label: None,
                                description: Some(json(r#"{"level":"blocking"}"#)),
                            },
                        ],
                    })),
                },
            ],
        }
    }

    fn text(value: &str) -> DecisionContentPart {
        DecisionContentPart {
            content: Some(Content::TextContent(value.to_owned())),
        }
    }

    fn json(value: &str) -> DecisionContentPart {
        DecisionContentPart {
            content: Some(Content::JsonContent(value.to_owned())),
        }
    }
}
