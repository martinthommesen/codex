//! Translates Anthropic Messages API stream events into Codex [`ResponseEvent`]s.
//!
//! The translator is transport-agnostic: the HTTP client feeds it SSE events, and the Claude Code
//! bridge feeds it the raw `stream_event` payloads emitted by `claude --output-format
//! stream-json --include-partial-messages`.

use super::error::anthropic_stream_error;
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
    produced_output: bool,
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
            produced_output: false,
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
                // More output follows, so earlier text was mid-turn commentary.
                self.flush_pending_text(Some(MessagePhase::Commentary), &mut out);
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
                    let phase = if stop_reason == "tool_use" || stop_reason == "pause_turn" {
                        MessagePhase::Commentary
                    } else {
                        MessagePhase::FinalAnswer
                    };
                    self.flush_pending_text(Some(phase), &mut out);
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
                self.flush_pending_text(Some(MessagePhase::FinalAnswer), &mut out);
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
            (_, other) => trace!("ignoring Anthropic delta `{other}` for block {index}"),
        }
    }

    fn stop_block(&mut self, index: usize, out: &mut Vec<ResponseEvent>) {
        let Some(state) = self.blocks.remove(&index) else {
            return;
        };
        match state {
            BlockState::Text { text } => {
                self.produced_output = true;
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
                let input = if input_json.trim().is_empty() {
                    match initial_input {
                        Value::Object(map) => Value::Object(map),
                        _ => Value::Object(serde_json::Map::new()),
                    }
                } else {
                    match serde_json::from_str::<Value>(&input_json) {
                        Ok(value) => value,
                        Err(err) => {
                            if self.stop_reason.as_deref() == Some("max_tokens") {
                                warn!("dropping truncated Anthropic tool call {id}: {err}");
                                return;
                            }
                            // Let the tool router report malformed arguments to the model.
                            Value::String(input_json)
                        }
                    }
                };
                self.produced_output = true;
                out.push(ResponseEvent::OutputItemDone(
                    self.tool_call_item(id, &name, input),
                ));
            }
            BlockState::Ignored => {}
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
                if !self.produced_output {
                    return Err(ApiError::ResponseIncomplete(Box::new(
                        ResponseIncompleteError {
                            reason: "max_tokens".to_string(),
                            response_id: self.message_id.clone(),
                            token_usage,
                        },
                    )));
                }
                warn!("Anthropic response stopped at max_tokens; output may be truncated");
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
            Some(other) => {
                debug!("unrecognized Anthropic stop_reason `{other}`");
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
) {
    let mut stream = stream.eventsource();
    loop {
        let start = Instant::now();
        let response = tokio::select! {
            biased;
            _ = tx_event.closed() => return,
            response = timeout(idle_timeout, stream.next()) => response,
        };
        if let Some(t) = telemetry.as_ref() {
            t.on_sse_poll(&response, start.elapsed());
        }
        let sse = match response {
            Ok(Some(Ok(sse))) => sse,
            Ok(Some(Err(err))) => {
                let error = match err {
                    eventsource_stream::EventStreamError::Transport(
                        error @ codex_client::TransportError::Policy(_),
                    ) => ApiError::Transport(error),
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
        let event: AnthropicStreamEvent = match serde_json::from_str(&sse.data) {
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
                let _ = tx_event.send(Err(err)).await;
                return;
            }
        }
    }
}

#[cfg(test)]
#[path = "stream_tests.rs"]
mod tests;
