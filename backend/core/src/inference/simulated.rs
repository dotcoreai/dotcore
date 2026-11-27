//! Deterministic native inference for the simulated model realization.
//!
//! This provider implements inference contracts directly. It does not emulate an engine protocol
//! or receive workload lifecycle operations.

use std::{
    fmt,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
};

use dotcore_domain::{clock::Clock, timing::SimulatedTiming};
use dotcore_proto::{
    dotcore::inference::v1::{
        ChatCompletionChoice, ChatCompletionResult, ChatFinishReason, ChatMessage, ChatMessageRole,
        ChatRequest, ChatStreamCompleted, ChatStreamEvent, ChatStreamFinished, ChatStreamStarted,
        ChatTextDelta, ChatToolCallDelta, ChatToolChoiceMode, DecisionAnswer, DecisionChoiceAnswer,
        DecisionPredicateAnswer, DecisionRefusal, DecisionRequest, DecisionResult,
        DecisionScoreAnswer, DecisionUsage, FunctionToolCall, FunctionToolDefinition, TokenUsage,
        chat_content_part, chat_message, chat_stream_event::Event, chat_text_delta,
        chat_tool_choice, decision_answer, decision_content_part, decision_question,
    },
    google::protobuf::NullValue,
};
use tokio::sync::{Notify, mpsc};

use super::{
    DecisionResponse, EngineBinding, EngineBindingError, EngineBindingIdentity, EngineInvoker,
    EngineProbeError, EngineReadiness, EngineReadinessProbe, InferenceAttempt,
    InferenceAttemptDisposition, InferenceCancellation, InferenceError, InferenceFuture,
    InferenceResponse, NativeInferenceProvider, decision,
};

/// Text tokens of the simulated answer: its first word, the word repeated to fill its length,
/// and its closing token, one token per streamed delta.
const SIMULATED_FIRST_TOKEN: &str = "Simulated";
const SIMULATED_FILL_TOKEN: &str = " response";
const SIMULATED_LAST_TOKEN: &str = ".";

/// Length of the simulated answer when the request sets no completion limit.
const SIMULATED_DEFAULT_COMPLETION_TOKENS: u32 = 3;

/// Longest simulated answer, whatever the request's limit, so a client's large limit can't make a
/// paced stream last for minutes.
const SIMULATED_MAX_COMPLETION_TOKENS: u32 = 256;

/// Prompt bytes per simulated prompt token, a common average for English text.
const SIMULATED_BYTES_PER_TOKEN: usize = 4;

/// Test-only controls for deterministic native simulated inference.
#[derive(Clone, Default)]
pub struct SimulatedInferenceTestSupport {
    admission: Arc<SimulatedInferenceAdmission>,
}

#[derive(Default)]
struct SimulatedInferenceAdmission {
    blocked: AtomicBool,
    /// Events a blocked streaming attempt sends before it holds; zero holds before the stream.
    stream_events_before_hold: usize,
    admitted: AtomicBool,
    released: AtomicBool,
    admitted_signal: Notify,
    release_signal: Notify,
}

impl fmt::Debug for SimulatedInferenceTestSupport {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("SimulatedInferenceTestSupport")
    }
}

impl PartialEq for SimulatedInferenceTestSupport {
    fn eq(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.admission, &other.admission)
    }
}

impl Eq for SimulatedInferenceTestSupport {}

impl SimulatedInferenceTestSupport {
    pub fn blocked() -> Self {
        Self::blocked_after_stream_events(0)
    }

    /// Holds like [`Self::blocked`], except that a streaming attempt first sends `events` stream
    /// events. A test can then interrupt a stream whose output is already committed.
    pub fn blocked_after_stream_events(events: usize) -> Self {
        let support = Self {
            admission: Arc::new(SimulatedInferenceAdmission {
                stream_events_before_hold: events,
                ..SimulatedInferenceAdmission::default()
            }),
        };
        support.admission.blocked.store(true, Ordering::Release);
        support
    }

    pub async fn wait_for_blocked_inference(&self) {
        loop {
            let notified = self.admission.admitted_signal.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            if self.admission.admitted.load(Ordering::Acquire) {
                return;
            }
            notified.await;
        }
    }

    pub fn release_blocked_inference(&self) {
        self.admission.released.store(true, Ordering::Release);
        self.admission.release_signal.notify_waiters();
    }

    async fn hold_after_admission(&self) {
        if !self.admission.blocked.load(Ordering::Acquire) {
            return;
        }
        self.admission.admitted.store(true, Ordering::Release);
        self.admission.admitted_signal.notify_waiters();
        loop {
            let notified = self.admission.release_signal.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            if self.admission.released.load(Ordering::Acquire) {
                return;
            }
            notified.await;
        }
    }
}

/// Native inference provider paired with simulated runtime composition. It streams its answer one
/// token at a time, paced at the simulated generation speed of its timing policy.
pub(crate) struct SimulatedNativeProvider {
    test_support: Option<SimulatedInferenceTestSupport>,
    timing: SimulatedTiming,
    clock: Arc<dyn Clock>,
}

impl SimulatedNativeProvider {
    /// Creates a provider pacing streamed tokens at the simulated generation speed of `timing`.
    pub(crate) fn new(timing: SimulatedTiming, clock: Arc<dyn Clock>) -> Self {
        Self {
            test_support: None,
            timing,
            clock,
        }
    }

    /// This provider, holding its attempts under `test_support`.
    pub(crate) fn with_test_support(self, test_support: SimulatedInferenceTestSupport) -> Self {
        Self {
            test_support: Some(test_support),
            ..self
        }
    }
}
impl NativeInferenceProvider for SimulatedNativeProvider {
    fn bind(
        &self,
        identity: EngineBindingIdentity,
        served_model_id: String,
    ) -> Result<EngineBinding, EngineBindingError> {
        if served_model_id.is_empty() {
            return Err(EngineBindingError::Invalid(
                "simulated provider requires an expected model identity".to_owned(),
            ));
        }
        Ok(EngineBinding::new(
            identity,
            Arc::new(SimulatedReadinessProbe {
                model_id: served_model_id,
            }),
            Arc::new(SimulatedInvoker {
                test_support: self.test_support.clone(),
                timing: self.timing,
                clock: Arc::clone(&self.clock),
            }),
        ))
    }
}

struct SimulatedReadinessProbe {
    model_id: String,
}

impl EngineReadinessProbe for SimulatedReadinessProbe {
    fn probe<'a>(
        &'a self,
        cancellation: InferenceCancellation,
    ) -> InferenceFuture<'a, Result<EngineReadiness, EngineProbeError>> {
        Box::pin(async move {
            if cancellation.reason().is_some() {
                return Err(EngineProbeError::Transient);
            }
            Ok(EngineReadiness::Ready {
                served_model_id: self.model_id.clone(),
            })
        })
    }
}

struct SimulatedInvoker {
    test_support: Option<SimulatedInferenceTestSupport>,
    /// Simulated timing that paces the streamed text tokens.
    timing: SimulatedTiming,
    clock: Arc<dyn Clock>,
}
impl EngineInvoker for SimulatedInvoker {
    fn invoke<'a>(
        &'a self,
        request: ChatRequest,
        mut cancellation: InferenceCancellation,
    ) -> InferenceFuture<'a, InferenceAttempt> {
        Box::pin(async move {
            if let Some(reason) = cancellation.reason() {
                return InferenceAttempt::failed(
                    InferenceAttemptDisposition::StartedOrAmbiguous,
                    InferenceError::Cancelled(reason),
                );
            }
            let stream = request.stream.unwrap_or(false);
            // A stream that holds after some events holds inside its sender task instead.
            let stream_hold = self
                .test_support
                .clone()
                .filter(|support| stream && support.admission.stream_events_before_hold > 0);
            if let Some(test_support) = self.test_support.as_ref().filter(|_| stream_hold.is_none())
            {
                tokio::select! {
                    _ = test_support.hold_after_admission() => {}
                    reason = cancellation.cancelled() => {
                        return InferenceAttempt::failed(
                            InferenceAttemptDisposition::StartedOrAmbiguous,
                            InferenceError::Cancelled(reason),
                        );
                    }
                }
            }
            let (choice, events, usage) = response(&request.model_id, &request);
            if !stream {
                return InferenceAttempt::Response(InferenceResponse::Complete(
                    ChatCompletionResult {
                        completion_id: "chatcmpl_simulated".to_owned(),
                        model_id: request.model_id,
                        created_at_unix_seconds: 0,
                        choices: vec![choice],
                        usage: Some(usage),
                    },
                ));
            }

            let token_interval = self.timing.token_interval;
            let clock = Arc::clone(&self.clock);
            let (sender, receiver) = mpsc::channel(8);
            tokio::spawn(async move {
                let mut text_sent = false;
                for (index, event) in events.into_iter().enumerate() {
                    if let Some(support) = stream_hold
                        .as_ref()
                        .filter(|support| index == support.admission.stream_events_before_hold)
                    {
                        tokio::select! {
                            biased;
                            reason = cancellation.cancelled() => {
                                let _ = sender.send(Err(InferenceError::Cancelled(reason))).await;
                                return;
                            }
                            () = support.hold_after_admission() => {}
                        }
                    }
                    let text = matches!(event.event, Some(Event::TextDelta(_)));
                    if text && text_sent && !token_interval.is_zero() {
                        tokio::select! {
                            biased;
                            reason = cancellation.cancelled() => {
                                let _ = sender.send(Err(InferenceError::Cancelled(reason))).await;
                                return;
                            }
                            () = clock.sleep(token_interval) => {}
                        }
                    }
                    text_sent |= text;
                    tokio::select! {
                        biased;
                        reason = cancellation.cancelled() => {
                            let _ = sender.send(Err(InferenceError::Cancelled(reason))).await;
                            return;
                        }
                        result = sender.send(Ok(event)) => if result.is_err() { return; }
                    }
                }
            });
            InferenceAttempt::Response(InferenceResponse::Stream(receiver))
        })
    }

    fn invoke_decision<'a>(
        &'a self,
        request: DecisionRequest,
        mut cancellation: InferenceCancellation,
    ) -> InferenceFuture<'a, InferenceAttempt<DecisionResponse>> {
        Box::pin(async move {
            if let Some(reason) = cancellation.reason() {
                return InferenceAttempt::failed(
                    InferenceAttemptDisposition::StartedOrAmbiguous,
                    InferenceError::Cancelled(reason),
                );
            }
            if let Some(test_support) = &self.test_support {
                tokio::select! {
                    _ = test_support.hold_after_admission() => {}
                    reason = cancellation.cancelled() => {
                        return InferenceAttempt::failed(
                            InferenceAttemptDisposition::StartedOrAmbiguous,
                            InferenceError::Cancelled(reason),
                        );
                    }
                }
            }
            InferenceAttempt::Response(DecisionResponse::Complete(simulated_decision(&request)))
        })
    }
}

/// Answers every question from hash-derived probabilities, so equal requests get equal answers
/// whatever the state; input usage counts one token per four state bytes.
fn simulated_decision(request: &DecisionRequest) -> DecisionResult {
    let answers = request
        .questions
        .iter()
        .map(|question| {
            let weight = |key: &str| simulated_weight(&question.name, key);
            let answer = match &question.kind {
                Some(decision_question::Kind::Predicate(_)) => {
                    let probabilities = normalized(&[weight("true"), weight("false")]);
                    decision_answer::Answer::Predicate(DecisionPredicateAnswer {
                        true_probability: Some(probabilities[0]),
                        value: Some(decision::predicate_value(probabilities[0])),
                        confidence: Some(confidence(&probabilities)),
                    })
                }
                Some(decision_question::Kind::Choice(choice)) => {
                    let weights = choice
                        .options
                        .iter()
                        .map(|option| weight(&option.name))
                        .collect::<Vec<_>>();
                    let probabilities = normalized(&weights);
                    decision_answer::Answer::Choice(DecisionChoiceAnswer {
                        selected_option_index: selected_index(&probabilities),
                        confidence: Some(confidence(&probabilities)),
                        option_probabilities: probabilities,
                    })
                }
                Some(decision_question::Kind::Score(score)) => {
                    let weights = (0..score.levels.len())
                        .map(|index| weight(&index.to_string()))
                        .collect::<Vec<_>>();
                    let probabilities = normalized(&weights);
                    decision_answer::Answer::Score(DecisionScoreAnswer {
                        expected_level: Some(decision::expected_level(&probabilities)),
                        selected_level_index: selected_index(&probabilities),
                        confidence: Some(confidence(&probabilities)),
                        level_probabilities: probabilities,
                    })
                }
                None => decision_answer::Answer::Refusal(DecisionRefusal {}),
            };
            DecisionAnswer {
                answer: Some(answer),
            }
        })
        .collect();
    let state_bytes = request
        .state
        .iter()
        .map(|part| match &part.content {
            Some(
                decision_content_part::Content::TextContent(text)
                | decision_content_part::Content::JsonContent(text),
            ) => text.len(),
            None => 0,
        })
        .sum::<usize>();
    DecisionResult {
        answers,
        usage: Some(DecisionUsage {
            input_tokens: state_bytes.div_ceil(4) as u64,
        }),
    }
}

/// A weight from 1 to 4 derived from FNV-1a over a question name and one of its answer keys.
fn simulated_weight(question: &str, key: &str) -> u64 {
    let mut hash = 0xcbf2_9ce4_8422_2325_u64;
    for byte in question.bytes().chain([0xff]).chain(key.bytes()) {
        hash = (hash ^ u64::from(byte)).wrapping_mul(0x0100_0000_01b3);
    }
    hash % 4 + 1
}

fn normalized(weights: &[u64]) -> Vec<f64> {
    let total = weights.iter().sum::<u64>() as f64;
    weights
        .iter()
        .map(|weight| *weight as f64 / total)
        .collect()
}

/// The selected index of an answer: its first most probable entry.
fn selected_index(probabilities: &[f64]) -> Option<u32> {
    decision::most_probable(probabilities).and_then(|index| u32::try_from(index).ok())
}

/// How far the most probable answer stands above a uniform distribution, from zero to one.
fn confidence(probabilities: &[f64]) -> f64 {
    let uniform = 1.0 / probabilities.len() as f64;
    let most = probabilities.iter().copied().fold(0.0, f64::max);
    if uniform >= 1.0 {
        1.0
    } else {
        ((most - uniform) / (1.0 - uniform)).max(0.0)
    }
}

/// The answer to a request, as a complete choice and as stream events, with its token usage.
///
/// Prompt tokens follow the message text, and every message but the last counts as cached, like
/// the earlier turns of a conversation an engine keeps in its prompt cache. A text answer fills
/// the request's completion limit, up to a simulated maximum, and a tool call takes one token.
fn response(
    model_id: &str,
    request: &ChatRequest,
) -> (ChatCompletionChoice, Vec<ChatStreamEvent>, TokenUsage) {
    let tool = simulated_tool(request);
    let message_tokens = request
        .messages
        .iter()
        .map(simulated_prompt_tokens)
        .collect::<Vec<_>>();
    let prompt_tokens = message_tokens.iter().sum::<u64>();
    let cached_prompt_tokens = prompt_tokens - message_tokens.last().copied().unwrap_or(0);
    let (completion_tokens, finish_reason) = simulated_completion_length(request);
    let completion_tokens = if tool.is_some() { 1 } else { completion_tokens };

    let usage = TokenUsage {
        prompt_tokens,
        completion_tokens: u64::from(completion_tokens),
        total_tokens: prompt_tokens + u64::from(completion_tokens),
        cached_prompt_tokens: Some(cached_prompt_tokens),
        reasoning_tokens: None,
    };
    let (choice, events) = tool.map_or_else(
        || simulated_text_response(model_id, completion_tokens, finish_reason, usage),
        |tool| simulated_tool_response(model_id, tool, usage),
    );

    (choice, events, usage)
}

/// Prompt tokens of one message: one for its role, plus one per four bytes of its text, tool
/// calls, tool result and reasoning. Image parts count no tokens.
fn simulated_prompt_tokens(message: &ChatMessage) -> u64 {
    let content = match &message.content {
        Some(chat_message::Content::TextContent(text)) => text.len(),
        Some(chat_message::Content::ToolResult(result)) => result.content.len(),
        Some(chat_message::Content::NullContent(_)) | None => 0,
    };
    let parts = message
        .content_parts
        .iter()
        .map(|part| match &part.content {
            Some(chat_content_part::Content::TextContent(text)) => text.len(),
            Some(chat_content_part::Content::ImageContent(_)) | None => 0,
        })
        .sum::<usize>();
    let tool_calls = message
        .tool_calls
        .iter()
        .map(|call| call.function_name.len() + call.arguments_json.len())
        .sum::<usize>();
    let reasoning = message.reasoning.as_ref().map_or(0, String::len);

    let bytes = content + parts + tool_calls + reasoning;
    1 + bytes.div_ceil(SIMULATED_BYTES_PER_TOKEN) as u64
}

/// The text answer's length and finish reason: the request's completion limit, which the answer
/// reaches and so finishes on length, capped at the simulated maximum, or the default length.
fn simulated_completion_length(request: &ChatRequest) -> (u32, ChatFinishReason) {
    let limit = request.generation_controls.as_ref().and_then(|controls| {
        controls
            .max_completion_tokens
            .or(controls.max_tokens)
            .filter(|limit| *limit > 0)
    });
    match limit {
        Some(limit) if limit <= SIMULATED_MAX_COMPLETION_TOKENS => {
            (limit, ChatFinishReason::Length)
        }
        Some(_) => (SIMULATED_MAX_COMPLETION_TOKENS, ChatFinishReason::Stop),
        None => (SIMULATED_DEFAULT_COMPLETION_TOKENS, ChatFinishReason::Stop),
    }
}

/// The answer's text tokens: its first word, then the fill word, closing on the last token once
/// the answer has room for all three.
fn simulated_text_tokens(length: u32) -> Vec<&'static str> {
    (0..length)
        .map(|index| match index {
            0 => SIMULATED_FIRST_TOKEN,
            index if index + 1 == length && length >= 3 => SIMULATED_LAST_TOKEN,
            _ => SIMULATED_FILL_TOKEN,
        })
        .collect()
}

fn simulated_tool_response(
    model_id: &str,
    tool: &FunctionToolDefinition,
    usage: TokenUsage,
) -> (ChatCompletionChoice, Vec<ChatStreamEvent>) {
    let call = FunctionToolCall {
        tool_call_id: "call_simulated_0".to_owned(),
        function_name: tool.name.clone(),
        arguments_json: "{}".to_owned(),
    };
    (
        ChatCompletionChoice {
            index: 0,
            message: Some(ChatMessage {
                role: ChatMessageRole::Assistant as i32,
                content: Some(chat_message::Content::NullContent(
                    NullValue::NullValue as i32,
                )),
                tool_calls: vec![call.clone()],
                name: None,
                content_parts: Vec::new(),
                reasoning: None,
            }),
            finish_reason: ChatFinishReason::ToolCalls as i32,
        },
        vec![
            started(model_id),
            tool_delta(
                Some(call.tool_call_id),
                Some(call.function_name),
                Some("{".to_owned()),
            ),
            tool_delta(None, None, Some("}".to_owned())),
            finished(ChatFinishReason::ToolCalls),
            usage_event(usage),
            completed(),
        ],
    )
}

fn simulated_text_response(
    model_id: &str,
    length: u32,
    finish_reason: ChatFinishReason,
    usage: TokenUsage,
) -> (ChatCompletionChoice, Vec<ChatStreamEvent>) {
    let tokens = simulated_text_tokens(length);
    let text = tokens.concat();
    let mut events = vec![started(model_id)];
    events.extend(tokens.iter().map(|token| text_delta((*token).to_owned())));
    events.extend([finished(finish_reason), usage_event(usage), completed()]);
    (
        ChatCompletionChoice {
            index: 0,
            message: Some(ChatMessage {
                role: ChatMessageRole::Assistant as i32,
                content: Some(chat_message::Content::TextContent(text)),
                tool_calls: Vec::new(),
                name: None,
                content_parts: Vec::new(),
                reasoning: None,
            }),
            finish_reason: finish_reason as i32,
        },
        events,
    )
}

fn simulated_tool(request: &ChatRequest) -> Option<&FunctionToolDefinition> {
    let has_tool_result = request
        .messages
        .iter()
        .any(|message| matches!(message.content, Some(chat_message::Content::ToolResult(_))));
    match request
        .tool_choice
        .as_ref()
        .and_then(|choice| choice.selection.as_ref())
    {
        Some(chat_tool_choice::Selection::Mode(mode)) => {
            match ChatToolChoiceMode::try_from(*mode).unwrap_or(ChatToolChoiceMode::Unspecified) {
                ChatToolChoiceMode::None => None,
                ChatToolChoiceMode::Required => request.tools.first(),
                ChatToolChoiceMode::Auto | ChatToolChoiceMode::Unspecified => {
                    (!has_tool_result).then(|| request.tools.first()).flatten()
                }
            }
        }
        Some(chat_tool_choice::Selection::FunctionName(name)) => {
            request.tools.iter().find(|tool| tool.name == *name)
        }
        None => (!has_tool_result).then(|| request.tools.first()).flatten(),
    }
}

fn started(model_id: &str) -> ChatStreamEvent {
    ChatStreamEvent {
        event: Some(Event::Started(ChatStreamStarted {
            completion_id: "chatcmpl_simulated".to_owned(),
            model_id: model_id.to_owned(),
            created_at_unix_seconds: 0,
        })),
    }
}

fn text_delta(content: String) -> ChatStreamEvent {
    ChatStreamEvent {
        event: Some(Event::TextDelta(ChatTextDelta {
            choice_index: 0,
            role: Some(ChatMessageRole::Assistant as i32),
            content: Some(chat_text_delta::Content::TextContent(content)),
        })),
    }
}

fn tool_delta(
    tool_call_id: Option<String>,
    function_name: Option<String>,
    arguments_json_fragment: Option<String>,
) -> ChatStreamEvent {
    ChatStreamEvent {
        event: Some(Event::ToolCallDelta(ChatToolCallDelta {
            choice_index: 0,
            tool_call_index: 0,
            tool_call_id,
            function_name,
            arguments_json_fragment,
        })),
    }
}

fn finished(finish_reason: ChatFinishReason) -> ChatStreamEvent {
    ChatStreamEvent {
        event: Some(Event::Finished(ChatStreamFinished {
            choice_index: 0,
            finish_reason: finish_reason as i32,
        })),
    }
}

fn usage_event(usage: TokenUsage) -> ChatStreamEvent {
    ChatStreamEvent {
        event: Some(Event::Usage(usage)),
    }
}

fn completed() -> ChatStreamEvent {
    ChatStreamEvent {
        event: Some(Event::Completed(ChatStreamCompleted {})),
    }
}

#[cfg(test)]
mod tests {
    use dotcore_proto::dotcore::inference::v1::{
        ChatGenerationControls, ChatMessage, ChatMessageRole, ChatRequest, FunctionToolResult,
        chat_message, chat_stream_event::Event,
    };

    use dotcore_domain::{clock::SystemClock, timing::Timing};

    use super::*;
    use crate::inference::{
        InferenceCancellationReason, InferenceResponse, NativeInferenceProvider,
        inference_cancellation,
    };

    #[tokio::test]
    async fn native_binding_is_ready_and_returns_the_deterministic_completion() {
        let identity =
            crate::inference::EngineBindingIdentity::new("deployment", "realization", "http", 1)
                .expect("binding identity");
        let binding = test_provider()
            .bind(identity, "fixture-model".to_owned())
            .expect("native binding");
        let (_, cancellation) = inference_cancellation();

        assert_eq!(
            binding
                .probe()
                .probe(cancellation.clone())
                .await
                .expect("readiness"),
            crate::inference::EngineReadiness::Ready {
                served_model_id: "fixture-model".to_owned(),
            }
        );
        let request = ChatRequest {
            model_id: "public-route".to_owned(),
            messages: vec![ChatMessage {
                role: ChatMessageRole::User as i32,
                content: Some(chat_message::Content::TextContent("Hello".to_owned())),
                ..Default::default()
            }],
            ..Default::default()
        };
        let crate::inference::InferenceAttempt::Response(InferenceResponse::Complete(response)) =
            binding.invoker().invoke(request, cancellation).await
        else {
            panic!("native provider must return a complete response")
        };

        assert_eq!(response.completion_id, "chatcmpl_simulated");
        assert_eq!(response.model_id, "public-route");
        // "Hello" is one role token plus two text tokens; a lone message has nothing cached.
        assert_eq!(
            response.usage,
            Some(TokenUsage {
                prompt_tokens: 3,
                completion_tokens: 3,
                total_tokens: 6,
                cached_prompt_tokens: Some(0),
                reasoning_tokens: None,
            })
        );
        assert_eq!(
            response.choices[0]
                .message
                .as_ref()
                .and_then(|message| message.content.clone()),
            Some(chat_message::Content::TextContent(
                "Simulated response.".to_owned()
            ))
        );
    }

    #[test]
    fn prompt_usage_follows_message_content_and_caches_earlier_turns() {
        let request = ChatRequest {
            messages: vec![
                text_message(ChatMessageRole::System, &"s".repeat(4096)),
                ChatMessage {
                    role: ChatMessageRole::Assistant as i32,
                    tool_calls: vec![FunctionToolCall {
                        tool_call_id: "call_1".to_owned(),
                        function_name: "read".to_owned(),
                        arguments_json: "{\"path\":\"a\"}".to_owned(),
                    }],
                    reasoning: Some("why".to_owned()),
                    ..Default::default()
                },
                ChatMessage {
                    role: ChatMessageRole::Tool as i32,
                    content: Some(chat_message::Content::ToolResult(FunctionToolResult {
                        tool_call_id: "call_1".to_owned(),
                        content: "x".repeat(9),
                    })),
                    ..Default::default()
                },
                text_message(ChatMessageRole::User, &"u".repeat(2048)),
            ],
            ..Default::default()
        };

        let (_, _, usage) = response("model", &request);

        // Role token plus bytes over four: 1 + 1024, 1 + ceil(4 + 12 + 3 / 4), 1 + ceil(9 / 4),
        // and the last message 1 + 512, which alone is not cached.
        assert_eq!(
            usage,
            TokenUsage {
                prompt_tokens: 1025 + 6 + 4 + 513,
                completion_tokens: 3,
                total_tokens: 1548 + 3,
                cached_prompt_tokens: Some(1035),
                reasoning_tokens: None,
            }
        );
    }

    #[test]
    fn text_answer_fills_the_completion_limit_up_to_the_simulated_maximum() {
        let answer = |max_tokens: Option<u32>, max_completion_tokens: Option<u32>| {
            let request = ChatRequest {
                messages: vec![text_message(ChatMessageRole::User, "Hi")],
                generation_controls: Some(ChatGenerationControls {
                    max_tokens,
                    max_completion_tokens,
                    ..Default::default()
                }),
                ..Default::default()
            };
            let (choice, events, usage) = response("model", &request);
            let deltas = events
                .iter()
                .filter(|event| matches!(event.event, Some(Event::TextDelta(_))))
                .count();
            (
                usage.completion_tokens,
                deltas,
                ChatFinishReason::try_from(choice.finish_reason).expect("finish reason"),
            )
        };

        assert_eq!(answer(None, None), (3, 3, ChatFinishReason::Stop));
        assert_eq!(answer(Some(2), None), (2, 2, ChatFinishReason::Length));
        assert_eq!(
            answer(Some(8), Some(48)),
            (48, 48, ChatFinishReason::Length)
        );
        assert_eq!(
            answer(None, Some(256)),
            (256, 256, ChatFinishReason::Length)
        );
        assert_eq!(
            answer(None, Some(32_000)),
            (256, 256, ChatFinishReason::Stop)
        );
        assert_eq!(simulated_text_tokens(1), ["Simulated"]);
        assert_eq!(simulated_text_tokens(2), ["Simulated", " response"]);
        assert_eq!(
            simulated_text_tokens(4).concat(),
            "Simulated response response."
        );
    }

    #[tokio::test]
    async fn native_binding_answers_a_fixed_decision_deterministically() {
        let identity =
            crate::inference::EngineBindingIdentity::new("deployment", "realization", "http", 1)
                .expect("binding identity");
        let binding = test_provider()
            .bind(identity, "fixture-model".to_owned())
            .expect("native binding");

        let mut results = Vec::new();
        for _ in 0..2 {
            let crate::inference::InferenceAttempt::Response(DecisionResponse::Complete(result)) =
                binding
                    .invoker()
                    .invoke_decision(decision_request(), inference_cancellation().1)
                    .await
            else {
                panic!("native provider must answer the decision")
            };
            results.push(result);
        }

        // Probabilities are hash-derived weights over their sum: 2/3, then 1/6, 4/6, 1/6, then
        // 2/9, 3/9, 4/9; confidence measures the most probable answer against a uniform one.
        let answer = |answer| DecisionAnswer {
            answer: Some(answer),
        };
        assert_eq!(results[0], results[1]);
        assert_eq!(
            results[0],
            DecisionResult {
                answers: vec![
                    answer(decision_answer::Answer::Predicate(
                        DecisionPredicateAnswer {
                            true_probability: Some(2.0 / 3.0),
                            value: Some(true),
                            confidence: Some(0.33333333333333326),
                        }
                    )),
                    answer(decision_answer::Answer::Choice(DecisionChoiceAnswer {
                        option_probabilities: vec![1.0 / 6.0, 4.0 / 6.0, 1.0 / 6.0],
                        selected_option_index: Some(1),
                        confidence: Some(0.49999999999999994),
                    })),
                    answer(decision_answer::Answer::Score(DecisionScoreAnswer {
                        level_probabilities: vec![2.0 / 9.0, 3.0 / 9.0, 4.0 / 9.0],
                        expected_level: Some(1.222222222222222),
                        selected_level_index: Some(2),
                        confidence: Some(0.16666666666666663),
                    })),
                ],
                usage: Some(DecisionUsage { input_tokens: 4 }),
            }
        );
    }

    #[tokio::test]
    async fn native_stream_observes_route_revocation() {
        let identity =
            crate::inference::EngineBindingIdentity::new("deployment", "realization", "http", 1)
                .expect("binding identity");
        let binding = test_provider()
            .bind(identity, "fixture-model".to_owned())
            .expect("native binding");
        let (signal, cancellation) = inference_cancellation();
        let request = ChatRequest {
            model_id: "public-route".to_owned(),
            stream: Some(true),
            ..Default::default()
        };
        let crate::inference::InferenceAttempt::Response(InferenceResponse::Stream(mut stream)) =
            binding.invoker().invoke(request, cancellation).await
        else {
            panic!("native provider must return a stream")
        };
        signal.cancel(InferenceCancellationReason::RouteRevoked);

        let error = stream
            .recv()
            .await
            .expect("terminal stream item")
            .expect_err("route revocation");
        assert!(matches!(
            error,
            crate::inference::InferenceError::Cancelled(InferenceCancellationReason::RouteRevoked)
        ));
        assert!(stream.recv().await.is_none());
    }

    #[tokio::test]
    async fn blocked_native_attempt_observes_route_revocation() {
        let support = SimulatedInferenceTestSupport::blocked();
        let identity =
            crate::inference::EngineBindingIdentity::new("deployment", "realization", "http", 1)
                .expect("binding identity");
        let binding = test_provider()
            .with_test_support(support.clone())
            .bind(identity, "fixture-model".to_owned())
            .expect("native binding");
        let (signal, cancellation) = inference_cancellation();
        let invoker = Arc::clone(binding.invoker());
        let attempt =
            tokio::spawn(async move { invoker.invoke(ChatRequest::default(), cancellation).await });
        support.wait_for_blocked_inference().await;

        signal.cancel(InferenceCancellationReason::RouteRevoked);

        assert!(matches!(
            attempt.await.expect("native attempt"),
            crate::inference::InferenceAttempt::Failed {
                error: crate::inference::InferenceError::Cancelled(
                    InferenceCancellationReason::RouteRevoked
                ),
                ..
            }
        ));
    }

    #[tokio::test]
    async fn blocked_stream_holds_after_its_committed_events_until_revoked() {
        let support = SimulatedInferenceTestSupport::blocked_after_stream_events(2);
        let identity =
            crate::inference::EngineBindingIdentity::new("deployment", "realization", "http", 1)
                .expect("binding identity");
        let binding = test_provider()
            .with_test_support(support.clone())
            .bind(identity, "fixture-model".to_owned())
            .expect("native binding");
        let (signal, cancellation) = inference_cancellation();
        let request = ChatRequest {
            model_id: "public-route".to_owned(),
            stream: Some(true),
            ..Default::default()
        };
        let crate::inference::InferenceAttempt::Response(InferenceResponse::Stream(mut stream)) =
            binding.invoker().invoke(request, cancellation).await
        else {
            panic!("a blocked stream still returns its stream")
        };

        let first = stream.recv().await.expect("first event").expect("Started");
        let second = stream
            .recv()
            .await
            .expect("second event")
            .expect("TextDelta");
        assert!(matches!(first.event, Some(Event::Started(_))));
        assert!(matches!(second.event, Some(Event::TextDelta(_))));
        support.wait_for_blocked_inference().await;
        assert!(matches!(
            stream.try_recv(),
            Err(tokio::sync::mpsc::error::TryRecvError::Empty)
        ));

        signal.cancel(InferenceCancellationReason::RouteRevoked);

        let error = stream
            .recv()
            .await
            .expect("terminal stream item")
            .expect_err("route revocation");
        assert!(matches!(
            error,
            crate::inference::InferenceError::Cancelled(InferenceCancellationReason::RouteRevoked)
        ));
        assert!(stream.recv().await.is_none());
    }

    #[tokio::test]
    async fn native_stream_keeps_public_model_name_and_always_reports_usage() {
        let identity =
            crate::inference::EngineBindingIdentity::new("deployment", "realization", "http", 1)
                .expect("binding identity");
        let binding = test_provider()
            .bind(identity, "fixture-model".to_owned())
            .expect("native binding");
        let (_, cancellation) = inference_cancellation();
        let request = ChatRequest {
            model_id: "public-route".to_owned(),
            stream: Some(true),
            include_stream_usage: Some(false),
            ..Default::default()
        };
        let crate::inference::InferenceAttempt::Response(InferenceResponse::Stream(mut stream)) =
            binding.invoker().invoke(request, cancellation).await
        else {
            panic!("native provider must return a stream")
        };
        let mut events = Vec::new();
        while let Some(event) = stream.recv().await {
            events.push(event.expect("stream event"));
        }

        assert!(matches!(
            events.first().and_then(|event| event.event.as_ref()),
            Some(Event::Started(started)) if started.model_id == "public-route"
        ));
        assert_eq!(
            events
                .iter()
                .filter(|event| matches!(event.event, Some(Event::Usage(_))))
                .count(),
            1
        );
        assert!(matches!(
            events.last().and_then(|event| event.event.as_ref()),
            Some(Event::Completed(_))
        ));
    }

    #[tokio::test(start_paused = true)]
    async fn native_stream_sends_one_text_token_per_delta_at_the_generation_speed() {
        let identity =
            crate::inference::EngineBindingIdentity::new("deployment", "realization", "http", 1)
                .expect("binding identity");
        let production = Timing::production();
        let binding = SimulatedNativeProvider::new(production.simulated, Arc::new(SystemClock))
            .bind(identity, "fixture-model".to_owned())
            .expect("native binding");
        let (_, cancellation) = inference_cancellation();
        let request = ChatRequest {
            model_id: "public-route".to_owned(),
            stream: Some(true),
            ..Default::default()
        };
        let crate::inference::InferenceAttempt::Response(InferenceResponse::Stream(mut stream)) =
            binding.invoker().invoke(request, cancellation).await
        else {
            panic!("native provider must return a stream")
        };
        let mut tokens = Vec::new();
        let mut usage = None;
        while let Some(event) = stream.recv().await {
            match event.expect("stream event").event {
                Some(Event::TextDelta(delta)) => {
                    let Some(chat_text_delta::Content::TextContent(text)) = delta.content else {
                        panic!("text delta must carry text")
                    };
                    tokens.push((text, tokio::time::Instant::now()));
                }
                Some(Event::Usage(reported)) => usage = Some(reported),
                _ => {}
            }
        }

        assert_eq!(
            tokens
                .iter()
                .map(|(text, _)| text.as_str())
                .collect::<Vec<_>>(),
            ["Simulated", " response", "."]
        );
        assert_eq!(
            tokens
                .windows(2)
                .map(|pair| pair[1].1 - pair[0].1)
                .collect::<Vec<_>>(),
            [production.simulated.token_interval; 2]
        );
        assert_eq!(usage.expect("usage").completion_tokens, 3);
    }

    /// A provider on the test timing, whose streamed tokens follow each other without pause.
    fn text_message(role: ChatMessageRole, text: &str) -> ChatMessage {
        ChatMessage {
            role: role as i32,
            content: Some(chat_message::Content::TextContent(text.to_owned())),
            ..Default::default()
        }
    }

    fn test_provider() -> SimulatedNativeProvider {
        SimulatedNativeProvider::new(Timing::test().simulated, Arc::new(SystemClock))
    }

    fn decision_request() -> DecisionRequest {
        use dotcore_proto::dotcore::inference::v1::{
            DecisionChoice, DecisionContentPart, DecisionOption, DecisionPredicate,
            DecisionQuestion, DecisionScore, DecisionScoreLevel,
        };

        let level = || DecisionScoreLevel {
            label: None,
            description: None,
        };
        DecisionRequest {
            model_id: "public-route".to_owned(),
            state: vec![DecisionContentPart {
                content: Some(decision_content_part::Content::TextContent(
                    "Refund please".to_owned(),
                )),
            }],
            questions: vec![
                DecisionQuestion {
                    name: "urgent".to_owned(),
                    kind: Some(decision_question::Kind::Predicate(
                        DecisionPredicate::default(),
                    )),
                    ..Default::default()
                },
                DecisionQuestion {
                    name: "category".to_owned(),
                    kind: Some(decision_question::Kind::Choice(DecisionChoice {
                        options: ["refund", "exchange", "other"]
                            .map(|name| DecisionOption {
                                name: name.to_owned(),
                                description: None,
                            })
                            .to_vec(),
                    })),
                    ..Default::default()
                },
                DecisionQuestion {
                    name: "severity".to_owned(),
                    kind: Some(decision_question::Kind::Score(DecisionScore {
                        levels: vec![level(), level(), level()],
                    })),
                    ..Default::default()
                },
            ],
        }
    }
}
