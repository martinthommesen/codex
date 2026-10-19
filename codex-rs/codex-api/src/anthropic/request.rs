//! Builds Anthropic Messages API requests from Codex conversation history.
//!
//! Codex history is a flat list of Responses API items. The Messages API instead wants
//! alternating `user`/`assistant` turns made of content blocks, with every `tool_use` answered by
//! a `tool_result` at the start of the next user turn. This module performs that translation
//! deterministically (so replayed history stays byte-stable for prompt caching and thinking
//! signatures) and records every item it cannot represent.

use super::model_traits::AnthropicThinking;
use super::tools::AnthropicTool;
use super::tools::AnthropicToolNameMap;
use super::tools::FREEFORM_INPUT_PROPERTY;
use super::tools::anthropic_tool_use_id;
use super::tools::anthropic_wire_tool_name;
use super::tools::translate_tools;
use codex_protocol::models::ContentItem;
use codex_protocol::models::FunctionCallOutputBody;
use codex_protocol::models::FunctionCallOutputContentItem;
use codex_protocol::models::FunctionCallOutputPayload;
use codex_protocol::models::ImageReference;
use codex_protocol::models::ReasoningItemReasoningSummary;
use codex_protocol::models::ResponseItem;
use serde::Serialize;
use serde_json::Value;
use serde_json::json;
use std::collections::BTreeSet;

/// Prefix stored in `ResponseItem::Reasoning::encrypted_content` for Anthropic thinking blocks.
pub const ANTHROPIC_THINKING_PREFIX: &str = "anthropic.thinking.v1:";
/// Prefix stored in `ResponseItem::Reasoning::encrypted_content` for redacted thinking blocks.
pub const ANTHROPIC_REDACTED_THINKING_PREFIX: &str = "anthropic.redacted_thinking.v1:";
/// Beta that lets requests ask the API to drop (instead of reject) thinking blocks whose
/// conversation prefix changed.
pub const THINKING_BINDING_BETA: &str = "thinking-binding-controls-2026-08-01";
/// Thinking captured without a signature (for example through a gateway that drops
/// `signature_delta` events) cannot be replayed.
const UNSIGNED_THINKING_NOTE: &str = "a Claude thinking block without a signature was not replayed";

/// Returns whether a history item carries Anthropic-only reasoning state.
///
/// OpenAI requests must never replay these items: their opaque payload is an Anthropic
/// signature, not OpenAI encrypted reasoning.
pub fn is_anthropic_reasoning_item(item: &ResponseItem) -> bool {
    matches!(
        item,
        ResponseItem::Reasoning {
            encrypted_content: Some(content),
            ..
        } if content.starts_with(ANTHROPIC_THINKING_PREFIX)
            || content.starts_with(ANTHROPIC_REDACTED_THINKING_PREFIX)
    )
}

/// One message in an Anthropic request.
#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct AnthropicMessage {
    pub role: &'static str,
    pub content: Vec<Value>,
}

/// Serialized body of `POST /v1/messages`.
#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct AnthropicMessagesRequest {
    pub model: String,
    pub max_tokens: u64,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub system: Vec<Value>,
    pub messages: Vec<AnthropicMessage>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub tools: Vec<AnthropicTool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_choice: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub thinking: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub output_config: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cache_control: Option<Value>,
    pub stream: bool,
}

/// Inputs that shape a request beyond the conversation history.
#[derive(Debug, Clone, PartialEq)]
pub struct AnthropicRequestOptions {
    pub model: String,
    pub max_tokens: u64,
    pub thinking: AnthropicThinking,
    /// Return readable thinking summaries (`display: "summarized"`) instead of omitting them.
    pub summarize_thinking: bool,
    pub effort: Option<&'static str>,
    pub parallel_tool_calls: bool,
    pub prompt_caching: bool,
    pub output_schema: Option<Value>,
    /// Ask the API to drop thinking blocks invalidated by history edits instead of failing.
    pub drop_mismatched_thinking: bool,
    /// Omit all replayed thinking blocks (recovery after a signature rejection).
    pub strip_thinking: bool,
}

/// A request ready to send, with the metadata needed to interpret its response.
#[derive(Debug, Clone, PartialEq)]
pub struct PreparedAnthropicRequest {
    pub request: AnthropicMessagesRequest,
    pub tool_names: AnthropicToolNameMap,
    /// `anthropic-beta` values the request depends on.
    pub betas: Vec<&'static str>,
    /// Items, tools, or settings that could not be represented exactly.
    pub notes: Vec<String>,
}

/// Builds a streaming Messages API request.
pub fn build_anthropic_request(
    base_instructions: &str,
    input: &[ResponseItem],
    tool_declarations: &[Value],
    options: &AnthropicRequestOptions,
) -> PreparedAnthropicRequest {
    let mut declarations = tool_declarations.to_vec();
    // Tools added to history mid-conversation are declared alongside the prompt tools.
    for item in input {
        if let ResponseItem::AdditionalTools { tools, .. } = item {
            declarations.extend(tools.iter().cloned());
        }
    }
    let translated_tools = translate_tools(&declarations);
    let mut notes = translated_tools.unsupported.clone();

    let mut history = translate_history(input, options.strip_thinking);
    notes.append(&mut history.notes);
    if translated_tools.tools.is_empty() && history_has_tool_blocks(&history.messages) {
        // Requests without tool declarations (for example local compaction) cannot carry
        // tool_use/tool_result blocks, so the transcript is rendered as text instead.
        flatten_tool_blocks(&mut history.messages);
    }
    limit_request_images(&mut history.messages, &mut notes);

    let mut system = Vec::new();
    if !base_instructions.trim().is_empty() {
        system.push(text_block(base_instructions));
    }
    for text in history.system {
        system.push(text_block(&text));
    }

    let mut betas = Vec::new();
    let thinking = match &options.thinking {
        AnthropicThinking::Omit => None,
        AnthropicThinking::Disabled => Some(json!({"type": "disabled"})),
        AnthropicThinking::Adaptive => {
            let display = if options.summarize_thinking {
                "summarized"
            } else {
                "omitted"
            };
            let mut thinking = json!({"type": "adaptive", "display": display});
            if options.drop_mismatched_thinking {
                thinking["block_binding"] = json!({"prefix_mismatch_behavior": "drop_block"});
                betas.push(THINKING_BINDING_BETA);
            }
            Some(thinking)
        }
        AnthropicThinking::Budget { budget_tokens } => {
            let mut thinking = json!({"type": "enabled", "budget_tokens": budget_tokens});
            if options.drop_mismatched_thinking {
                thinking["block_binding"] = json!({"prefix_mismatch_behavior": "drop_block"});
                betas.push(THINKING_BINDING_BETA);
            }
            Some(thinking)
        }
    };

    let mut output_config = serde_json::Map::new();
    if let Some(effort) = options.effort {
        output_config.insert("effort".to_string(), Value::String(effort.to_string()));
    }
    if let Some(schema) = &options.output_schema {
        output_config.insert(
            "format".to_string(),
            json!({"type": "json_schema", "schema": schema}),
        );
    }

    let tool_choice = (!translated_tools.tools.is_empty()).then(|| {
        if options.parallel_tool_calls {
            json!({"type": "auto"})
        } else {
            json!({"type": "auto", "disable_parallel_tool_use": true})
        }
    });

    let request = AnthropicMessagesRequest {
        model: options.model.clone(),
        max_tokens: options.max_tokens,
        system,
        messages: history.messages,
        tools: translated_tools.tools,
        tool_choice,
        thinking,
        output_config: (!output_config.is_empty()).then_some(Value::Object(output_config)),
        cache_control: options.prompt_caching.then(|| json!({"type": "ephemeral"})),
        stream: true,
    };

    PreparedAnthropicRequest {
        request,
        tool_names: translated_tools.names,
        betas,
        notes,
    }
}

/// History translated into Anthropic turns.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct TranslatedHistory {
    /// Leading developer/system instructions, hoisted into the top-level `system` prompt.
    pub system: Vec<String>,
    pub messages: Vec<AnthropicMessage>,
    pub notes: Vec<String>,
}

#[derive(Default)]
struct HistoryBuilder {
    messages: Vec<AnthropicMessage>,
    notes: BTreeSet<String>,
    /// Calls rendered as plain text; their outputs must be rendered as text too.
    text_calls: BTreeSet<String>,
}

impl HistoryBuilder {
    fn push(&mut self, role: &'static str, block: Value) {
        match self.messages.last_mut() {
            Some(last) if last.role == role => last.content.push(block),
            _ => self.messages.push(AnthropicMessage {
                role,
                content: vec![block],
            }),
        }
    }

    fn push_text(&mut self, role: &'static str, text: &str) {
        if !text.trim().is_empty() {
            self.push(role, text_block(text));
        }
    }

    fn note(&mut self, note: impl Into<String>) {
        self.notes.insert(note.into());
    }
}

/// Translates Codex history items into Anthropic turns.
pub fn translate_history(items: &[ResponseItem], strip_thinking: bool) -> TranslatedHistory {
    let mut builder = HistoryBuilder::default();
    let mut system = Vec::new();
    let mut leading = true;

    for item in items {
        if leading {
            if let ResponseItem::Message { role, content, .. } = item
                && (role == "developer" || role == "system")
            {
                let text = message_text(content);
                if !text.trim().is_empty() {
                    system.push(text);
                }
                continue;
            }
            if !matches!(
                item,
                ResponseItem::AdditionalTools { .. }
                    | ResponseItem::ConfigurationUpdate { .. }
                    | ResponseItem::CompactionTrigger {}
                    | ResponseItem::Other
            ) {
                leading = false;
            }
        }
        translate_item(item, strip_thinking, &mut builder);
    }

    let mut messages = builder.messages;
    let mut notes: Vec<String> = builder.notes.into_iter().collect();
    repair_turns(&mut messages, &mut notes);
    TranslatedHistory {
        system,
        messages,
        notes,
    }
}

fn translate_item(item: &ResponseItem, strip_thinking: bool, builder: &mut HistoryBuilder) {
    match item {
        ResponseItem::Message { role, content, .. } => match role.as_str() {
            "assistant" => {
                for content_item in content {
                    match content_item {
                        ContentItem::OutputText { text, .. } | ContentItem::InputText { text } => {
                            builder.push_text("assistant", text);
                        }
                        ContentItem::InputImage { .. } | ContentItem::InputAudio { .. } => {
                            builder.note("assistant media content was omitted");
                        }
                    }
                }
            }
            "developer" | "system" => {
                let text = message_text(content);
                if !text.trim().is_empty() {
                    builder.push_text("user", &format!("[developer instructions]\n{text}"));
                }
            }
            _ => {
                for content_item in content {
                    if let Some(block) = user_content_block(content_item, builder) {
                        builder.push("user", block);
                    }
                }
            }
        },
        ResponseItem::AgentMessage {
            author,
            recipient,
            content,
            ..
        } => match codex_protocol::models::plaintext_agent_message_content(content) {
            Some(text) => builder.push_text(
                "user",
                &format!("[message from agent {author} to {recipient}]\n{text}"),
            ),
            None => builder.note(
                "an encrypted inter-agent message could not be decrypted for the Anthropic model",
            ),
        },
        ResponseItem::Reasoning {
            summary,
            encrypted_content,
            ..
        } => {
            let Some(encrypted) = encrypted_content else {
                return;
            };
            if let Some(signature) = encrypted.strip_prefix(ANTHROPIC_THINKING_PREFIX) {
                if strip_thinking {
                    return;
                }
                if signature.is_empty() {
                    // The API always rejects a missing signature, even with `drop_block`.
                    builder.note(UNSIGNED_THINKING_NOTE);
                    return;
                }
                let thinking: String = summary
                    .iter()
                    .map(|ReasoningItemReasoningSummary::SummaryText { text }| text.as_str())
                    .collect();
                builder.push(
                    "assistant",
                    json!({"type": "thinking", "thinking": thinking, "signature": signature}),
                );
            } else if let Some(data) = encrypted.strip_prefix(ANTHROPIC_REDACTED_THINKING_PREFIX) {
                if strip_thinking {
                    return;
                }
                if data.is_empty() {
                    builder.note(UNSIGNED_THINKING_NOTE);
                    return;
                }
                builder.push(
                    "assistant",
                    json!({"type": "redacted_thinking", "data": data}),
                );
            } else {
                builder.note(
                    "reasoning produced by another provider is encrypted and was not replayed",
                );
            }
        }
        ResponseItem::FunctionCall {
            name,
            namespace,
            arguments,
            call_id,
            ..
        } => {
            let input = match serde_json::from_str::<Value>(arguments) {
                Ok(Value::Object(map)) => Value::Object(map),
                Ok(other) => json!({"value": other}),
                Err(_) => json!({"raw_arguments": arguments}),
            };
            builder.push(
                "assistant",
                json!({
                    "type": "tool_use",
                    "id": anthropic_tool_use_id(call_id),
                    "name": anthropic_wire_tool_name(namespace.as_deref(), name),
                    "input": input,
                }),
            );
        }
        ResponseItem::CustomToolCall {
            name,
            namespace,
            input,
            call_id,
            ..
        } => {
            builder.push(
                "assistant",
                json!({
                    "type": "tool_use",
                    "id": anthropic_tool_use_id(call_id),
                    "name": anthropic_wire_tool_name(namespace.as_deref(), name),
                    "input": {FREEFORM_INPUT_PROPERTY: input},
                }),
            );
        }
        ResponseItem::LocalShellCall {
            call_id, action, ..
        } => {
            let action = serde_json::to_string(action).unwrap_or_default();
            builder.push_text(
                "assistant",
                &format!("[local shell call made with another provider]\n{action}"),
            );
            if let Some(call_id) = call_id {
                builder.text_calls.insert(call_id.clone());
            }
        }
        ResponseItem::FunctionCallOutput {
            call_id, output, ..
        } => {
            let Some(call_id) = call_id else {
                builder.note("a tool output without a call id was omitted");
                return;
            };
            push_tool_output(builder, call_id, output);
        }
        ResponseItem::CustomToolCallOutput {
            call_id, output, ..
        } => push_tool_output(builder, call_id, output),
        ResponseItem::ToolSearchCall { .. } | ResponseItem::ToolSearchOutput { .. } => {
            builder.note("tool search history from another provider was omitted");
        }
        ResponseItem::WebSearchCall { .. } => {
            builder.note("hosted web search calls from another provider were omitted");
        }
        ResponseItem::ImageGenerationCall { revised_prompt, .. } => {
            let prompt = revised_prompt.as_deref().unwrap_or("(no prompt recorded)");
            builder.push_text(
                "assistant",
                &format!("[image generated with another provider; prompt: {prompt}]"),
            );
        }
        ResponseItem::Compaction { .. } | ResponseItem::ContextCompaction { .. } => {
            builder.push_text(
                "user",
                "[Earlier conversation context was compacted by another provider into an encrypted \
summary that is not available to this model.]",
            );
            builder.note("an encrypted compaction summary from another provider is unreadable");
        }
        ResponseItem::AdditionalTools { .. }
        | ResponseItem::ConfigurationUpdate { .. }
        | ResponseItem::CompactionTrigger {}
        | ResponseItem::Other => {}
    }
}

fn push_tool_output(
    builder: &mut HistoryBuilder,
    call_id: &str,
    output: &FunctionCallOutputPayload,
) {
    if builder.text_calls.contains(call_id) {
        let text = output.body.to_text().unwrap_or_default();
        builder.push_text("user", &format!("[output of call {call_id}]\n{text}"));
        return;
    }
    let mut block = json!({
        "type": "tool_result",
        "tool_use_id": anthropic_tool_use_id(call_id),
        "content": tool_result_content(&output.body, builder),
    });
    if output.success == Some(false) {
        block["is_error"] = Value::Bool(true);
    }
    builder.push("user", block);
}

fn tool_result_content(body: &FunctionCallOutputBody, builder: &mut HistoryBuilder) -> Value {
    match body {
        FunctionCallOutputBody::Text(text) => {
            if text.trim().is_empty() {
                Value::String("(no output)".to_string())
            } else {
                Value::String(text.clone())
            }
        }
        FunctionCallOutputBody::ContentItems(items) => {
            let mut blocks = Vec::new();
            for item in items {
                match item {
                    FunctionCallOutputContentItem::InputText { text } => {
                        if !text.trim().is_empty() {
                            blocks.push(text_block(text));
                        }
                    }
                    FunctionCallOutputContentItem::InputImage { image, .. } => {
                        if let Some(block) = image_block(image, builder) {
                            blocks.push(block);
                        }
                    }
                    FunctionCallOutputContentItem::InputAudio { .. } => {
                        builder.note("audio tool output is not supported by Anthropic models");
                        blocks.push(text_block("[audio output omitted]"));
                    }
                    FunctionCallOutputContentItem::EncryptedContent { .. } => {
                        builder.note("encrypted tool output from another provider was omitted");
                        blocks.push(text_block("[encrypted output omitted]"));
                    }
                }
            }
            if blocks.is_empty() {
                Value::String("(no output)".to_string())
            } else {
                Value::Array(blocks)
            }
        }
    }
}

fn user_content_block(item: &ContentItem, builder: &mut HistoryBuilder) -> Option<Value> {
    match item {
        ContentItem::InputText { text } | ContentItem::OutputText { text, .. } => {
            (!text.trim().is_empty()).then(|| text_block(text))
        }
        ContentItem::InputImage { image, .. } => image_block(image, builder),
        ContentItem::InputAudio { .. } => {
            builder.note("audio input is not supported by Anthropic models and was omitted");
            Some(text_block(
                "[audio input omitted: not supported by this model]",
            ))
        }
    }
}

fn image_block(image: &ImageReference, builder: &mut HistoryBuilder) -> Option<Value> {
    match image {
        ImageReference::Inline { image_url } => {
            if let Some((media_type, is_base64, data)) = parse_data_url(image_url) {
                if !matches!(
                    media_type.as_str(),
                    "image/jpeg" | "image/png" | "image/gif" | "image/webp"
                ) {
                    builder.note(format!(
                        "image type {media_type} is not supported by Anthropic models"
                    ));
                    return Some(text_block("[image omitted: unsupported image type]"));
                }
                if !is_base64 {
                    builder.note("an inline image that is not base64-encoded was omitted");
                    return Some(text_block("[image omitted: not base64-encoded]"));
                }
                return Some(json!({
                    "type": "image",
                    "source": {"type": "base64", "media_type": media_type, "data": data},
                }));
            }
            if image_url.starts_with("https://") || image_url.starts_with("http://") {
                return Some(json!({
                    "type": "image",
                    "source": {"type": "url", "url": image_url},
                }));
            }
            builder.note("an image reference in an unknown format was omitted");
            Some(text_block("[image omitted]"))
        }
        ImageReference::File { .. } => {
            builder.note("OpenAI file references are not available to Anthropic models");
            Some(text_block(
                "[image omitted: stored in another provider's file store]",
            ))
        }
    }
}

/// Splits a `data:` URL into its lowercased media type, whether it is base64-encoded, and its data.
/// The scheme and parameters are case-insensitive, and parameters may precede `base64`.
fn parse_data_url(url: &str) -> Option<(String, bool, &str)> {
    let (scheme, rest) = url.split_at_checked("data:".len())?;
    if !scheme.eq_ignore_ascii_case("data:") {
        return None;
    }
    let (meta, data) = rest.split_once(',')?;
    let mut parameters = meta.split(';');
    let media_type = parameters.next().unwrap_or("").trim().to_ascii_lowercase();
    let is_base64 = parameters.any(|parameter| parameter.trim().eq_ignore_ascii_case("base64"));
    Some((media_type, is_base64, data))
}

fn message_text(content: &[ContentItem]) -> String {
    content
        .iter()
        .filter_map(|item| match item {
            ContentItem::InputText { text } | ContentItem::OutputText { text, .. } => {
                Some(text.as_str())
            }
            ContentItem::InputImage { .. } | ContentItem::InputAudio { .. } => None,
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn text_block(text: &str) -> Value {
    json!({"type": "text", "text": text})
}

fn block_type(block: &Value) -> &str {
    block.get("type").and_then(Value::as_str).unwrap_or("")
}

/// Enforces Messages API turn rules on translated history.
fn repair_turns(messages: &mut Vec<AnthropicMessage>, notes: &mut Vec<String>) {
    // Assistant turns made only of thinking blocks (for example after an interrupted stream)
    // cannot be replayed on their own.
    messages.retain(|message| {
        message.role != "assistant"
            || message
                .content
                .iter()
                .any(|block| !matches!(block_type(block), "thinking" | "redacted_thinking"))
    });
    merge_adjacent(messages);

    let mut index = 0;
    while index < messages.len() {
        if messages[index].role != "assistant" {
            index += 1;
            continue;
        }
        let tool_use_ids: Vec<String> = messages[index]
            .content
            .iter()
            .filter(|block| block_type(block) == "tool_use")
            .filter_map(|block| block.get("id").and_then(Value::as_str).map(str::to_string))
            .collect();
        if tool_use_ids.is_empty() {
            index += 1;
            continue;
        }
        if messages
            .get(index + 1)
            .is_none_or(|next| next.role != "user")
        {
            messages.insert(
                index + 1,
                AnthropicMessage {
                    role: "user",
                    content: Vec::new(),
                },
            );
        }
        let next = &mut messages[index + 1];
        let answered: BTreeSet<String> = next
            .content
            .iter()
            .filter(|block| block_type(block) == "tool_result")
            .filter_map(|block| {
                block
                    .get("tool_use_id")
                    .and_then(Value::as_str)
                    .map(str::to_string)
            })
            .collect();
        for id in &tool_use_ids {
            if !answered.contains(id) {
                next.content.push(json!({
                    "type": "tool_result",
                    "tool_use_id": id,
                    "content": "Tool call did not complete.",
                    "is_error": true,
                }));
                notes.push("a tool call without a recorded result was marked incomplete".into());
            }
        }
        index += 2;
    }

    // Tool results must answer a tool_use in the immediately preceding assistant turn, once; any
    // other result is kept as plain text. Results must also lead their user turn.
    let mut previous_tool_uses: BTreeSet<String> = BTreeSet::new();
    for message in messages.iter_mut() {
        if message.role == "assistant" {
            previous_tool_uses = message
                .content
                .iter()
                .filter(|block| block_type(block) == "tool_use")
                .filter_map(|block| block.get("id").and_then(Value::as_str).map(str::to_string))
                .collect();
            continue;
        }
        let mut results = Vec::new();
        let mut others = Vec::new();
        for block in message.content.drain(..) {
            if block_type(&block) == "tool_result" {
                let id = block
                    .get("tool_use_id")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string();
                if previous_tool_uses.remove(&id) {
                    results.push(block);
                } else {
                    let text = match block.get("content") {
                        Some(Value::String(text)) => text.clone(),
                        Some(other) => other.to_string(),
                        None => String::new(),
                    };
                    others.push(text_block(&format!("[output of call {id}]\n{text}")));
                }
            } else {
                others.push(block);
            }
        }
        results.extend(others);
        message.content = results;
        previous_tool_uses.clear();
    }

    messages.retain(|message| !message.content.is_empty());
    merge_adjacent(messages);

    if messages
        .first()
        .is_some_and(|first| first.role == "assistant")
    {
        messages.insert(
            0,
            AnthropicMessage {
                role: "user",
                content: vec![text_block("[conversation resumed]")],
            },
        );
    }
    if messages.is_empty() {
        messages.push(AnthropicMessage {
            role: "user",
            content: vec![text_block("[no prior conversation]")],
        });
    }
    // Assistant prefill is rejected by current models, so a trailing assistant turn gets an
    // explicit continuation request.
    if messages.last().is_some_and(|last| last.role == "assistant") {
        messages.push(AnthropicMessage {
            role: "user",
            content: vec![text_block("Continue.")],
        });
        notes.push("history ended with an assistant turn; a continuation request was added".into());
    }
}

/// With more than this many images in one request, the Messages API rejects every image larger
/// than 2000 px per side, and every earlier image is resent on each request. Codex prepares
/// images up to 2048 px, so older images beyond this count are replaced by a note.
const MAX_REQUEST_IMAGES: usize = 20;
const OMITTED_IMAGE_TEXT: &str =
    "[earlier image omitted: too many images in this conversation to send them all]";

/// Keeps the most recent [`MAX_REQUEST_IMAGES`] images (including those inside tool results).
fn limit_request_images(messages: &mut [AnthropicMessage], notes: &mut Vec<String>) {
    let mut kept = 0;
    let mut omitted = 0;
    for message in messages.iter_mut().rev() {
        for block in message.content.iter_mut().rev() {
            limit_block_images(block, &mut kept, &mut omitted);
        }
    }
    if omitted > 0 {
        notes.push(format!(
            "{omitted} earlier image(s) were omitted because a Claude request can carry at most \
{MAX_REQUEST_IMAGES} images at full size"
        ));
    }
}

fn limit_block_images(block: &mut Value, kept: &mut usize, omitted: &mut usize) {
    match block_type(block) {
        "image" if *kept < MAX_REQUEST_IMAGES => *kept += 1,
        "image" => {
            *omitted += 1;
            *block = text_block(OMITTED_IMAGE_TEXT);
        }
        "tool_result" => {
            if let Some(Value::Array(content)) = block.get_mut("content") {
                for nested in content.iter_mut().rev() {
                    limit_block_images(nested, kept, omitted);
                }
            }
        }
        _ => {}
    }
}

fn history_has_tool_blocks(messages: &[AnthropicMessage]) -> bool {
    messages.iter().any(|message| {
        message
            .content
            .iter()
            .any(|block| matches!(block_type(block), "tool_use" | "tool_result"))
    })
}

/// Rewrites tool calls and results as text and drops thinking blocks, whose signatures would no
/// longer match the rewritten turns.
fn flatten_tool_blocks(messages: &mut Vec<AnthropicMessage>) {
    for message in messages.iter_mut() {
        let mut content = Vec::with_capacity(message.content.len());
        for block in message.content.drain(..) {
            match block_type(&block) {
                "tool_use" => {
                    let name = block.get("name").and_then(Value::as_str).unwrap_or("tool");
                    let input = block.get("input").map(Value::to_string).unwrap_or_default();
                    content.push(text_block(&format!("[called tool {name} with {input}]")));
                }
                "tool_result" => {
                    let text = match block.get("content") {
                        Some(Value::String(text)) => text.clone(),
                        Some(Value::Array(blocks)) => blocks
                            .iter()
                            .filter_map(|block| block.get("text").and_then(Value::as_str))
                            .collect::<Vec<_>>()
                            .join("\n"),
                        _ => String::new(),
                    };
                    content.push(text_block(&format!("[tool result]\n{text}")));
                }
                "thinking" | "redacted_thinking" => {}
                _ => content.push(block),
            }
        }
        message.content = content;
    }
    messages.retain(|message| !message.content.is_empty());
    merge_adjacent(messages);
}

fn merge_adjacent(messages: &mut Vec<AnthropicMessage>) {
    let mut merged: Vec<AnthropicMessage> = Vec::with_capacity(messages.len());
    for message in messages.drain(..) {
        match merged.last_mut() {
            Some(last) if last.role == message.role => last.content.extend(message.content),
            _ => merged.push(message),
        }
    }
    *messages = merged;
}

#[cfg(test)]
#[path = "request_tests.rs"]
mod tests;
