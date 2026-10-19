//! Translates Anthropic Messages API stream events into Codex [`ResponseEvent`]s.
//!
//! The translator is transport-agnostic: the HTTP client feeds it SSE events, and the Claude Code
//! bridge feeds it the raw `stream_event` payloads emitted by `claude --output-format
//! stream-json --include-partial-messages`.

use super::error::anthropic_stream_error;
use super::oauth_response::map_claude_oauth_transport_error;
use super::request::ANTHROPIC_REDACTED_THINKING_PREFIX;
use super::request::ANTHROPIC_THINKING_PREFIX;
use super::tools::AnthropicToolKind;
use super::tools::AnthropicToolNameMap;
use super::tools::FREEFORM_INPUT_PROPERTY;
use crate::common::ResponseEvent;
use crate::common::ResponseStream;
use crate::error::ApiError;
use crate::telemetry::SseTelemetry;
use codex_client::ByteStream;
use codex_client::StreamResponse;
use codex_protocol::error::ResponseIncompleteError;
use codex_protocol::models::ContentItem;
use codex_protocol::models::MessagePhase;
use codex_protocol::models::ReasoningItemReasoningSummary;
use codex_protocol::models::ResponseItem;
use codex_protocol::protocol::RateLimitSnapshot;
use codex_protocol::protocol::TokenUsage;
use eventsource_stream::Eventsource;
use futures::StreamExt;
use serde::Deserialize;
use serde_json::Value;
use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::mpsc;
use tokio::time::Instant;
use tokio::time::timeout;
use tracing::debug;
use tracing::trace;
use tracing::warn;

const REQUEST_ID_HEADER: &str = "request-id";

/// A Messages API streaming event.
#[derive(Debug, Clone, Deserialize, PartialEq)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum AnthropicStreamEvent {
    MessageStart {
        message: AnthropicMessageStart,
    },
    ContentBlockStart {
        index: usize,
        content_block: Value,
    },
    ContentBlockDelta {
        index: usize,
        delta: Value,
    },
    ContentBlockStop {
        index: usize,
    },
    MessageDelta {
        delta: AnthropicMessageDelta,
        #[serde(default)]
        usage: Option<AnthropicUsage>,
    },
    MessageStop,
    Ping,
    Error {
        error: AnthropicErrorBody,
    },
    #[serde(other)]
    Unknown,
}

#[derive(Debug, Clone, Default, Deserialize, PartialEq)]
pub struct AnthropicMessageStart {
    #[serde(default)]
    pub id: Option<String>,
    #[serde(default)]
    pub model: Option<String>,
    #[serde(default)]
    pub usage: Option<AnthropicUsage>,
}

#[derive(Debug, Clone, Default, Deserialize, PartialEq)]
pub struct AnthropicMessageDelta {
    #[serde(default)]
    pub stop_reason: Option<String>,
    #[serde(default)]
    pub stop_details: Option<Value>,
}

/// Token usage reported by the Messages API.
#[derive(Debug, Clone, Copy, Default, Deserialize, PartialEq, Eq)]
pub struct AnthropicUsage {
    #[serde(default)]
    pub input_tokens: Option<i64>,
    #[serde(default)]
    pub output_tokens: Option<i64>,
    #[serde(default)]
    pub cache_creation_input_tokens: Option<i64>,
    #[serde(default)]
    pub cache_read_input_tokens: Option<i64>,
}

impl AnthropicUsage {
    fn merge(&mut self, other: &AnthropicUsage) {
        // message_delta usage is cumulative; later values replace earlier ones.
        if other.input_tokens.is_some() {
            self.input_tokens = other.input_tokens;
        }
        if other.output_tokens.is_some() {
            self.output_tokens = other.output_tokens;
        }
        if other.cache_creation_input_tokens.is_some() {
            self.cache_creation_input_tokens = other.cache_creation_input_tokens;
        }
        if other.cache_read_input_tokens.is_some() {
            self.cache_read_input_tokens = other.cache_read_input_tokens;
        }
    }

    /// Converts to Codex token usage, where `input_tokens` includes cached input.
    pub fn to_token_usage(self) -> TokenUsage {
        let uncached = self.input_tokens.unwrap_or(0);
        let cache_read = self.cache_read_input_tokens.unwrap_or(0);
        let cache_write = self.cache_creation_input_tokens.unwrap_or(0);
        let output = self.output_tokens.unwrap_or(0);
        let input = uncached + cache_read + cache_write;
        TokenUsage {
            input_tokens: input,
            cached_input_tokens: cache_read,
            cache_write_input_tokens: cache_write,
            output_tokens: output,
            // The Messages API does not break thinking tokens out of output tokens.
            reasoning_output_tokens: 0,
            total_tokens: input + output,
            codex_rollout_budget_units: None,
        }
    }
}

#[derive(Debug, Clone, Default, Deserialize, PartialEq)]
pub struct AnthropicErrorBody {
    #[serde(default, rename = "type")]
    pub kind: String,
    #[serde(default)]
    pub message: String,
}

#[derive(Debug)]
enum BlockState {
    Text {
        text: String,
    },
    Thinking {
        thinking: String,
        signature: String,
    },
    RedactedThinking {
        data: String,
    },
    ToolUse {
        id: String,
        name: String,
        input_json: String,
        initial_input: Value,
    },
    Ignored,
}

#[derive(Debug)]
struct HeldToolCall {
    id: String,
    name: String,
    input: Value,
    /// The input parsed as JSON, or a later content block started, so the block was not cut off
    /// by `max_tokens`.
    complete: bool,
}

/// Stateful translator for one streamed message.
#[derive(Debug)]
pub struct AnthropicStreamTranslator {
    tool_names: AnthropicToolNameMap,
    message_id: Option<String>,
    usage: AnthropicUsage,
    stop_reason: Option<String>,
    stop_details: Option<Value>,
    blocks: BTreeMap<usize, BlockState>,
    /// Finished text whose phase depends on what the model does next.
    pending_text: Option<String>,
    /// Tool calls of this message, in model order. They are released only once the stop reason
    /// is known, because a `refusal` (or a cut-off by `max_tokens`) can still end the message
    /// and its tool calls must then not run.
    held_tool_calls: Vec<HeldToolCall>,
    /// At least one tool call was emitted, so a `max_tokens` stop still has work to continue.
    emitted_tool_calls: bool,
    completed: bool,
}

impl AnthropicStreamTranslator {
    pub fn new(tool_names: AnthropicToolNameMap) -> Self {
        Self {
            tool_names,
            message_id: None,
            usage: AnthropicUsage::default(),
            stop_reason: None,
            stop_details: None,
            blocks: BTreeMap::new(),
            pending_text: None,
            held_tool_calls: Vec::new(),
            emitted_tool_calls: false,
            completed: false,
        }
    }

    /// Whether `message_stop` has been processed.
    pub fn is_completed(&self) -> bool {
        self.completed
    }

    /// The model's stop reason, once known.
    pub fn stop_reason(&self) -> Option<&str> {
        self.stop_reason.as_deref()
    }

    /// Usage accumulated so far.
    pub fn usage(&self) -> AnthropicUsage {
        self.usage
    }

    /// Message id assigned by the API, once known.
    pub fn message_id(&self) -> Option<&str> {
        self.message_id.as_deref()
    }

    /// Processes one stream event and returns the Codex events it produces.
    pub fn handle(&mut self, event: AnthropicStreamEvent) -> Result<Vec<ResponseEvent>, ApiError> {
        let mut out = Vec::new();
        match event {
            AnthropicStreamEvent::MessageStart { message } => {
                self.message_id = message.id.clone();
                if let Some(usage) = message.usage.as_ref() {
                    self.usage.merge(usage);
                }
                out.push(ResponseEvent::Created {
                    response_id: message.id,
                });
                // Reported input usage already reflects replayed thinking (earlier turns' thinking
                // is not counted by the API), so Codex must not add its own estimates.
                out.push(ResponseEvent::ServerReasoningIncluded(true));
            }
            AnthropicStreamEvent::ContentBlockStart {
                index,
                content_block,
            } => {
                // More output follows, so earlier text was mid-turn commentary and earlier tool
                // calls were not cut off.
                self.flush_pending_text(Some(MessagePhase::Commentary), &mut out);
                for call in &mut self.held_tool_calls {
                    call.complete = true;
                }
                self.start_block(index, &content_block, &mut out);
            }
            AnthropicStreamEvent::ContentBlockDelta { index, delta } => {
                self.apply_delta(index, &delta, &mut out);
            }
            AnthropicStreamEvent::ContentBlockStop { index } => self.stop_block(index, &mut out),
            AnthropicStreamEvent::MessageDelta { delta, usage } => {
                if delta.stop_reason.is_some() {
                    self.stop_reason = delta.stop_reason;
                }
                if let Some(stop_reason) = self.stop_reason.as_deref() {
                    if stop_reason == "refusal" {
                        // Partial output of a refused response is discarded, not recorded as
                        // the final answer and replayed.
                        self.pending_text = None;
                    } else {
                        let phase = if stop_reason == "tool_use" || stop_reason == "pause_turn" {
                            MessagePhase::Commentary
                        } else {
                            MessagePhase::FinalAnswer
                        };
                        self.flush_pending_text(Some(phase), &mut out);
                    }
                    // Text is finished first, so a tool call never inherits the id of a still
                    // active message item.
                    self.release_held_tool_calls(&mut out);
                }
                if delta.stop_details.is_some() {
                    self.stop_details = delta.stop_details;
                }
                if let Some(usage) = usage.as_ref() {
                    self.usage.merge(usage);
                }
            }
            AnthropicStreamEvent::MessageStop => {
                // Flush blocks that never received an explicit stop.
                let open: Vec<usize> = self.blocks.keys().copied().collect();
                for index in open {
                    self.stop_block(index, &mut out);
                }
                if self.stop_reason.as_deref() == Some("refusal") {
                    self.pending_text = None;
                } else {
                    self.flush_pending_text(Some(MessagePhase::FinalAnswer), &mut out);
                }
                self.release_held_tool_calls(&mut out);
                out.push(self.completion()?);
                self.completed = true;
            }
            AnthropicStreamEvent::Ping | AnthropicStreamEvent::Unknown => {}
            AnthropicStreamEvent::Error { error } => {
                return Err(anthropic_stream_error(&error.kind, &error.message));
            }
        }
        Ok(out)
    }

    fn start_block(&mut self, index: usize, block: &Value, out: &mut Vec<ResponseEvent>) {
        let kind = block.get("type").and_then(Value::as_str).unwrap_or("");
        let state = match kind {
            "text" => {
                let text = block
                    .get("text")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_string();
                out.push(ResponseEvent::OutputItemAdded(assistant_message(
                    String::new(),
                    /*phase*/ None,
                )));
                if !text.is_empty() {
                    out.push(ResponseEvent::OutputTextDelta(text.clone()));
                }
                BlockState::Text { text }
            }
            "thinking" => {
                let thinking = block
                    .get("thinking")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_string();
                let signature = block
                    .get("signature")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_string();
                out.push(ResponseEvent::OutputItemAdded(reasoning_item(
                    String::new(),
                    None,
                )));
                if !thinking.is_empty() {
                    out.push(ResponseEvent::ReasoningSummaryDelta {
                        delta: thinking.clone(),
                        summary_index: 0,
                    });
                }
                BlockState::Thinking {
                    thinking,
                    signature,
                }
            }
            "redacted_thinking" => BlockState::RedactedThinking {
                data: block
                    .get("data")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_string(),
            },
            "tool_use" => BlockState::ToolUse {
                id: block
                    .get("id")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_string(),
                name: block
                    .get("name")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_string(),
                input_json: String::new(),
                initial_input: block.get("input").cloned().unwrap_or(Value::Null),
            },
            other => {
                debug!("ignoring unsupported Anthropic content block type `{other}`");
                BlockState::Ignored
            }
        };
        self.blocks.insert(index, state);
    }

    fn apply_delta(&mut self, index: usize, delta: &Value, out: &mut Vec<ResponseEvent>) {
        let Some(state) = self.blocks.get_mut(&index) else {
            trace!("delta for unknown Anthropic content block {index}");
            return;
        };
        let kind = delta.get("type").and_then(Value::as_str).unwrap_or("");
        match (state, kind) {
            (BlockState::Text { text }, "text_delta") => {
                if let Some(fragment) = delta.get("text").and_then(Value::as_str)
                    && !fragment.is_empty()
                {
                    text.push_str(fragment);
                    out.push(ResponseEvent::OutputTextDelta(fragment.to_string()));
                }
            }
            (BlockState::Thinking { thinking, .. }, "thinking_delta") => {
                if let Some(fragment) = delta.get("thinking").and_then(Value::as_str)
                    && !fragment.is_empty()
                {
                    thinking.push_str(fragment);
                    out.push(ResponseEvent::ReasoningSummaryDelta {
                        delta: fragment.to_string(),
                        summary_index: 0,
                    });
                }
            }
            (BlockState::Thinking { signature, .. }, "signature_delta") => {
                if let Some(fragment) = delta.get("signature").and_then(Value::as_str) {
                    signature.push_str(fragment);
                }
            }
            (BlockState::ToolUse { input_json, .. }, "input_json_delta") => {
                if let Some(fragment) = delta.get("partial_json").and_then(Value::as_str) {
                    input_json.push_str(fragment);
                }
            }
            (_, "citations_delta") => {}
            (_, _) => trace!("ignoring unsupported Anthropic delta for block {index}"),
        }
    }

    fn stop_block(&mut self, index: usize, out: &mut Vec<ResponseEvent>) {
        let Some(state) = self.blocks.remove(&index) else {
            return;
        };
        match state {
            BlockState::Text { text } => {
                self.flush_pending_text(Some(MessagePhase::Commentary), out);
                self.pending_text = Some(text);
            }
            BlockState::Thinking {
                thinking,
                signature,
            } => {
                let encrypted = format!("{ANTHROPIC_THINKING_PREFIX}{signature}");
                out.push(ResponseEvent::OutputItemDone(reasoning_item(
                    thinking,
                    Some(encrypted),
                )));
            }
            BlockState::RedactedThinking { data } => {
                let encrypted = format!("{ANTHROPIC_REDACTED_THINKING_PREFIX}{data}");
                out.push(ResponseEvent::OutputItemDone(reasoning_item(
                    String::new(),
                    Some(encrypted),
                )));
            }
            BlockState::ToolUse {
                id,
                name,
                input_json,
                initial_input,
            } => {
                // Blocks close before `message_delta` reports the stop reason. Every call is held
                // until then: a `refusal` must not run any of them, and input that is empty or
                // unparseable may have been cut off by `max_tokens`.
                let (input, complete) = if input_json.trim().is_empty() {
                    let input = match initial_input {
                        Value::Object(map) => Value::Object(map),
                        _ => Value::Object(serde_json::Map::new()),
                    };
                    (input, false)
                } else {
                    match serde_json::from_str::<Value>(&input_json) {
                        Ok(value) => (value, true),
                        // The tool router reports malformed arguments to the model.
                        Err(_) => (Value::String(input_json), false),
                    }
                };
                self.held_tool_calls.push(HeldToolCall {
                    id,
                    name,
                    input,
                    complete,
                });
                if self.stop_reason.is_some() {
                    self.release_held_tool_calls(out);
                }
            }
            BlockState::Ignored => {}
        }
    }

    /// Emits held tool calls in model order. A refused response runs none of them; a response
    /// that was cut off (or ended without a stop reason) drops only calls that may be truncated.
    fn release_held_tool_calls(&mut self, out: &mut Vec<ResponseEvent>) {
        let held = std::mem::take(&mut self.held_tool_calls);
        let stop_reason = self.stop_reason.as_deref();
        if stop_reason == Some("refusal") {
            if !held.is_empty() {
                warn!(
                    dropped_tool_calls = held.len(),
                    "dropping Anthropic tool calls from a refused response"
                );
            }
            return;
        }
        let cut_off = matches!(
            stop_reason,
            None | Some("max_tokens" | "model_context_window_exceeded")
        );
        let (held, truncated): (Vec<_>, Vec<_>) =
            held.into_iter().partition(|call| call.complete || !cut_off);
        if !truncated.is_empty() {
            warn!(
                dropped_tool_calls = truncated.len(),
                "dropping Anthropic tool calls whose input was cut off"
            );
        }
        for call in held {
            self.emitted_tool_calls = true;
            out.push(ResponseEvent::OutputItemDone(
                self.tool_call_item(call.id, &call.name, call.input),
            ));
        }
    }

    fn flush_pending_text(&mut self, phase: Option<MessagePhase>, out: &mut Vec<ResponseEvent>) {
        if let Some(text) = self.pending_text.take() {
            out.push(ResponseEvent::OutputItemDone(assistant_message(
                text, phase,
            )));
        }
    }

    fn tool_call_item(&self, call_id: String, wire_name: &str, input: Value) -> ResponseItem {
        let target = self.tool_names.resolve(wire_name);
        match target.kind {
            AnthropicToolKind::Freeform => {
                let text = match input.get(FREEFORM_INPUT_PROPERTY) {
                    Some(Value::String(text)) => text.clone(),
                    Some(other) => other.to_string(),
                    None => match &input {
                        Value::String(text) => text.clone(),
                        other => other.to_string(),
                    },
                };
                ResponseItem::CustomToolCall {
                    encrypted_content: None,
                    id: None,
                    status: None,
                    call_id,
                    name: target.name,
                    namespace: target.namespace,
                    input: text,
                    internal_chat_message_metadata_passthrough: None,
                }
            }
            AnthropicToolKind::Function => {
                let arguments = match &input {
                    Value::String(raw) => raw.clone(),
                    other => other.to_string(),
                };
                ResponseItem::FunctionCall {
                    encrypted_content: None,
                    status: None,
                    id: None,
                    name: target.name,
                    namespace: target.namespace,
                    arguments,
                    // Claude arguments are plaintext; this marks collaboration messages as
                    // directly readable by other agents.
                    encrypted_function_args: Some(Vec::new()),
                    call_id,
                    internal_chat_message_metadata_passthrough: None,
                }
            }
        }
    }

    fn completion(&self) -> Result<ResponseEvent, ApiError> {
        let token_usage = Some(self.usage.to_token_usage());
        let response_id = self.message_id.clone().unwrap_or_default();
        let end_turn = match self.stop_reason.as_deref() {
            Some("end_turn") | Some("stop_sequence") => Some(true),
            Some("tool_use") => None,
            Some("pause_turn") => Some(false),
            Some("max_tokens") => {
                // Like an incomplete Responses API response: a truncated answer must not end the
                // turn as if it were complete. Complete tool calls still run and the model
                // continues after them.
                if !self.emitted_tool_calls {
                    return Err(ApiError::ResponseIncomplete(Box::new(
                        ResponseIncompleteError {
                            reason: "max_tokens".to_string(),
                            response_id: self.message_id.clone(),
                            token_usage,
                        },
                    )));
                }
                warn!("Anthropic response stopped at max_tokens after complete tool calls");
                None
            }
            Some("model_context_window_exceeded") => return Err(ApiError::ContextWindowExceeded),
            Some("refusal") => {
                let category = self
                    .stop_details
                    .as_ref()
                    .and_then(|details| details.get("category"))
                    .and_then(Value::as_str)
                    .unwrap_or("unspecified");
                let explanation = self
                    .stop_details
                    .as_ref()
                    .and_then(|details| details.get("explanation"))
                    .and_then(Value::as_str)
                    .unwrap_or("");
                let message = if explanation.is_empty() {
                    format!("Claude declined to respond (refusal category: {category}).")
                } else {
                    format!(
                        "Claude declined to respond (refusal category: {category}): {explanation}"
                    )
                };
                return Err(match category {
                    "cyber" => ApiError::CyberPolicy { message },
                    "bio" => ApiError::BioPolicy { message },
                    _ => ApiError::InvalidRequest { message },
                });
            }
            Some(_) => {
                debug!("unrecognized Anthropic stop_reason");
                None
            }
            None => None,
        };
        Ok(ResponseEvent::Completed {
            response_id,
            token_usage,
            usage_metadata: None,
            end_turn,
        })
    }
}

fn assistant_message(text: String, phase: Option<MessagePhase>) -> ResponseItem {
    ResponseItem::Message {
        encrypted_content: None,
        status: None,
        id: None,
        role: "assistant".to_string(),
        content: vec![ContentItem::OutputText {
            text,
            annotations: None,
            logprobs: None,
        }],
        phase,
        internal_chat_message_metadata_passthrough: None,
    }
}

/// Builds the history item for a thinking block.
///
/// The exact thinking text is kept as the (persisted) reasoning summary because the API checks
/// it against the signature when the block is replayed.
fn reasoning_item(thinking: String, encrypted_content: Option<String>) -> ResponseItem {
    let summary = if thinking.is_empty() {
        Vec::new()
    } else {
        vec![ReasoningItemReasoningSummary::SummaryText { text: thinking }]
    };
    ResponseItem::Reasoning {
        id: None,
        summary,
        content: None,
        encrypted_content,
        internal_chat_message_metadata_passthrough: None,
    }
}

/// Spawns a task that reads an Anthropic SSE response and forwards translated events.
pub fn spawn_anthropic_stream(
    stream_response: StreamResponse,
    idle_timeout: Duration,
    tool_names: AnthropicToolNameMap,
    telemetry: Option<Arc<dyn SseTelemetry>>,
) -> ResponseStream {
    spawn_messages_stream(
        stream_response,
        idle_timeout,
        tool_names,
        telemetry,
        Vec::new(),
        false,
    )
}

pub(super) fn spawn_claude_oauth_stream(
    stream_response: StreamResponse,
    idle_timeout: Duration,
    tool_names: AnthropicToolNameMap,
    telemetry: Option<Arc<dyn SseTelemetry>>,
    rate_limits: Vec<RateLimitSnapshot>,
) -> ResponseStream {
    spawn_messages_stream(
        stream_response,
        idle_timeout,
        tool_names,
        telemetry,
        rate_limits,
        true,
    )
}

fn spawn_messages_stream(
    stream_response: StreamResponse,
    idle_timeout: Duration,
    tool_names: AnthropicToolNameMap,
    telemetry: Option<Arc<dyn SseTelemetry>>,
    rate_limits: Vec<RateLimitSnapshot>,
    oauth: bool,
) -> ResponseStream {
    let upstream_request_id = stream_response
        .headers
        .get(REQUEST_ID_HEADER)
        .and_then(|value| value.to_str().ok())
        .map(str::to_string);
    let (tx_event, rx_event) = mpsc::channel::<Result<ResponseEvent, ApiError>>(1600);
    tokio::spawn(process_anthropic_sse(
        stream_response.bytes,
        tx_event,
        idle_timeout,
        AnthropicStreamTranslator::new(tool_names),
        telemetry,
        rate_limits,
        oauth,
    ));
    ResponseStream {
        rx_event,
        upstream_request_id,
        interrupt: None,
    }
}

async fn process_anthropic_sse(
    stream: ByteStream,
    tx_event: mpsc::Sender<Result<ResponseEvent, ApiError>>,
    idle_timeout: Duration,
    mut translator: AnthropicStreamTranslator,
    telemetry: Option<Arc<dyn SseTelemetry>>,
    rate_limits: Vec<RateLimitSnapshot>,
    oauth: bool,
) {
    for snapshot in rate_limits {
        if tx_event
            .send(Ok(ResponseEvent::RateLimits(snapshot)))
            .await
            .is_err()
        {
            return;
        }
    }
    let mut stream = stream.eventsource();
    loop {
        let start = Instant::now();
        let response = tokio::select! {
            biased;
            _ = tx_event.closed() => return,
            response = timeout(idle_timeout, stream.next()) => response,
        };
        if let Some(t) = telemetry.as_ref() {
            if oauth {
                record_oauth_sse_poll(t.as_ref(), &response, start.elapsed());
            } else {
                t.on_sse_poll(&response, start.elapsed());
            }
        }
        let sse = match response {
            Ok(Some(Ok(sse))) => sse,
            Ok(Some(Err(err))) => {
                let error = match err {
                    eventsource_stream::EventStreamError::Transport(
                        error @ codex_client::TransportError::Policy(_),
                    ) => ApiError::Transport(error),
                    _ if oauth => map_claude_oauth_transport_error(ApiError::Stream(
                        "Claude OAuth stream transport or framing failed".to_string(),
                    )),
                    error => ApiError::Stream(error.to_string()),
                };
                let _ = tx_event.send(Err(error)).await;
                return;
            }
            Ok(None) => {
                if !translator.is_completed() {
                    let _ = tx_event
                        .send(Err(ApiError::Stream(
                            "Anthropic stream closed before message_stop".to_string(),
                        )))
                        .await;
                }
                return;
            }
            Err(_) => {
                let _ = tx_event
                    .send(Err(ApiError::Stream(
                        "idle timeout waiting for Anthropic SSE".to_string(),
                    )))
                    .await;
                return;
            }
        };
        if sse.data.trim().is_empty() {
            continue;
        }
        let mut event: AnthropicStreamEvent = match serde_json::from_str(&sse.data) {
            Ok(event) => event,
            Err(err) => {
                debug!(
                    error_line = err.line(),
                    error_column = err.column(),
                    payload_bytes = sse.data.len(),
                    "failed to parse Anthropic SSE event"
                );
                continue;
            }
        };
        if oauth {
            // Classify the raw error before discarding its text, retaining context-limit and
            // thinking-signature semantics without echoing tokens or authorization codes.
            if let AnthropicStreamEvent::Error { error } = &event {
                let error = map_claude_oauth_transport_error(anthropic_stream_error(
                    &error.kind,
                    &error.message,
                ));
                let _ = tx_event.send(Err(error)).await;
                return;
            }
            if let AnthropicStreamEvent::MessageDelta { delta, .. } = &mut event
                && delta.stop_reason.as_deref() == Some("refusal")
            {
                // Refusal details are server-supplied diagnostic strings, rather than model
                // output. Keep the policy category while withholding opaque explanations.
                let category = delta
                    .stop_details
                    .as_ref()
                    .and_then(|details| details["category"].as_str());
                let category = match category {
                    Some("bio") => "bio",
                    Some("cyber") => "cyber",
                    _ => "unspecified",
                };
                delta.stop_details = Some(serde_json::json!({"category":category}));
            }
        }
        match translator.handle(event) {
            Ok(events) => {
                for event in events {
                    if tx_event.send(Ok(event)).await.is_err() {
                        return;
                    }
                }
                if translator.is_completed() {
                    return;
                }
            }
            Err(err) => {
                let err = if oauth {
                    map_claude_oauth_transport_error(err)
                } else {
                    err
                };
                let _ = tx_event.send(Err(err)).await;
                return;
            }
        }
    }
}

type SsePollResult = Result<
    Option<
        Result<
            eventsource_stream::Event,
            eventsource_stream::EventStreamError<codex_client::TransportError>,
        >,
    >,
    tokio::time::error::Elapsed,
>;

fn record_oauth_sse_poll(
    telemetry: &dyn SseTelemetry,
    response: &SsePollResult,
    duration: Duration,
) {
    // Telemetry can format parser/transport failures and special SSE error event payloads.
    // Report the same poll timing and success state with bounded protocol-only metadata.
    let safe_response = match response {
        Ok(Some(Ok(event))) => {
            let kind = match event.event.as_str() {
                "message_start" => "message_start",
                "content_block_start" => "content_block_start",
                "content_block_delta" => "content_block_delta",
                "content_block_stop" => "content_block_stop",
                "message_delta" => "message_delta",
                "message_stop" => "message_stop",
                "ping" => "ping",
                "error" => "error",
                _ => "unknown",
            };
            Ok(Some(Ok(eventsource_stream::Event {
                event: kind.to_string(),
                data: "{}".to_string(),
                ..Default::default()
            })))
        }
        Ok(Some(Err(_))) => Ok(Some(Err(eventsource_stream::EventStreamError::Transport(
            codex_client::TransportError::Network(
                "Claude OAuth stream transport or framing failed".to_string(),
            ),
        )))),
        _ => {
            telemetry.on_sse_poll(response, duration);
            return;
        }
    };
    telemetry.on_sse_poll(&safe_response, duration);
}

#[cfg(test)]
#[path = "stream_tests.rs"]
mod tests;
