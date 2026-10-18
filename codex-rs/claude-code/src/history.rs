//! Keeps Codex history and Claude Code's own transcript in sync.
//!
//! Claude Code stores the conversation itself, so each turn only sends what Codex added since the
//! last turn. A hash of the already-synced prefix detects history rewrites (compaction, rollback,
//! forks), after which a fresh Claude Code session is seeded with a rendered transcript.

use codex_api::anthropic::AnthropicMessage;
use codex_api::anthropic::translate_history;
use codex_protocol::models::ContentItem;
use codex_protocol::models::FunctionCallOutputBody;
use codex_protocol::models::FunctionCallOutputContentItem;
use codex_protocol::models::FunctionCallOutputPayload;
use codex_protocol::models::ImageReference;
use codex_protocol::models::ResponseItem;
use codex_protocol::models::plaintext_agent_message_content;
use serde_json::Value;
use serde_json::json;
use sha2::Digest;
use sha2::Sha256;

/// Rendered transcripts keep at most this many characters (the most recent ones).
pub(crate) const TRANSCRIPT_CHAR_LIMIT: usize = 400_000;

const SUPPORTED_IMAGE_TYPES: [&str; 4] = ["image/jpeg", "image/png", "image/gif", "image/webp"];

/// JSON for `item` without its Codex-assigned top-level `id`.
pub(crate) fn normalized_item_json(item: &ResponseItem) -> String {
    match serde_json::to_value(item) {
        Ok(Value::Object(mut map)) => {
            map.remove("id");
            Value::Object(map).to_string()
        }
        Ok(other) => other.to_string(),
        Err(_) => String::new(),
    }
}

/// SHA-256 (hex) over the normalized JSON of `items`.
pub(crate) fn prefix_hash(items: &[ResponseItem]) -> String {
    let mut hasher = Sha256::new();
    for item in items {
        hasher.update(normalized_item_json(item).as_bytes());
        hasher.update(b"\n");
    }
    format!("{:x}", hasher.finalize())
}

pub(crate) fn sha256_hex(text: &str) -> String {
    format!("{:x}", Sha256::digest(text.as_bytes()))
}

/// Leading developer/system messages, which are hoisted into the system prompt (the same rule as
/// [`translate_history`]'s `system` field). Returns the number of leading items and their texts.
pub(crate) fn leading_system(items: &[ResponseItem]) -> (usize, Vec<String>) {
    let mut texts = Vec::new();
    for (index, item) in items.iter().enumerate() {
        match item {
            ResponseItem::Message { role, content, .. }
                if role == "developer" || role == "system" =>
            {
                let text = message_text(content);
                if !text.trim().is_empty() {
                    texts.push(text);
                }
            }
            ResponseItem::AdditionalTools { .. }
            | ResponseItem::ConfigurationUpdate { .. }
            | ResponseItem::CompactionTrigger {}
            | ResponseItem::Other => {}
            _ => return (index, texts),
        }
    }
    (items.len(), texts)
}

/// Base instructions followed by the hoisted leading developer/system messages.
pub(crate) fn build_system_prompt(base_instructions: &str, hoisted: &[String]) -> String {
    let mut parts: Vec<&str> = Vec::with_capacity(hoisted.len() + 1);
    if !base_instructions.trim().is_empty() {
        parts.push(base_instructions);
    }
    parts.extend(hoisted.iter().map(String::as_str));
    parts.join("\n\n")
}

/// Start of the trailing run of new user input (user/developer messages and agent messages),
/// never before `floor`.
pub(crate) fn trailing_user_input_start(items: &[ResponseItem], floor: usize) -> usize {
    let mut start = items.len();
    while start > floor && is_user_input(&items[start - 1]) {
        start -= 1;
    }
    start.max(floor)
}

fn is_user_input(item: &ResponseItem) -> bool {
    match item {
        ResponseItem::Message { role, .. } => role != "assistant",
        ResponseItem::AgentMessage { .. } => true,
        _ => false,
    }
}

/// How one history item reaches Claude Code.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum Classified<'a> {
    /// Output of a Codex tool call; answers a parked MCP call when one is outstanding.
    ToolOutput {
        call_id: &'a str,
        output: &'a FunctionCallOutputPayload,
    },
    /// Content sent to Claude Code as (part of) a user message.
    UserBlocks(Vec<Value>),
    /// Produced by Claude Code itself, or a request control without content.
    Skip,
}

pub(crate) fn classify(item: &ResponseItem) -> Classified<'_> {
    match item {
        ResponseItem::Message { role, content, .. } => match role.as_str() {
            "assistant" => Classified::Skip,
            "developer" | "system" => {
                let mut blocks = Vec::new();
                let text = message_text(content);
                if !text.trim().is_empty() {
                    blocks.push(text_block(&format!("[developer instructions]\n{text}")));
                }
                for item in content {
                    if let ContentItem::InputImage { image, .. } = item {
                        blocks.push(image_block(image));
                    }
                }
                Classified::UserBlocks(blocks)
            }
            _ => Classified::UserBlocks(content.iter().filter_map(content_block).collect()),
        },
        ResponseItem::AgentMessage {
            author,
            recipient,
            content,
            ..
        } => {
            let text = match plaintext_agent_message_content(content) {
                Some(text) => format!("[message from agent {author} to {recipient}]\n{text}"),
                None => format!(
                    "[an encrypted message from agent {author} to {recipient} could not be read]"
                ),
            };
            Classified::UserBlocks(vec![text_block(&text)])
        }
        ResponseItem::Reasoning { .. }
        | ResponseItem::FunctionCall { .. }
        | ResponseItem::CustomToolCall { .. }
        | ResponseItem::AdditionalTools { .. }
        | ResponseItem::ConfigurationUpdate { .. }
        | ResponseItem::CompactionTrigger {}
        | ResponseItem::Other => Classified::Skip,
        ResponseItem::FunctionCallOutput {
            call_id: Some(call_id),
            output,
            ..
        }
        | ResponseItem::CustomToolCallOutput {
            call_id, output, ..
        } => Classified::ToolOutput { call_id, output },
        ResponseItem::FunctionCallOutput {
            call_id: None,
            output,
            ..
        } => Classified::UserBlocks(vec![text_block(&format!(
            "[tool output without a call id]\n{}",
            output.body.to_text().unwrap_or_default()
        ))]),
        ResponseItem::LocalShellCall { action, .. } => {
            let action = serde_json::to_string(action).unwrap_or_default();
            note(&format!(
                "[local shell call made with another provider: {action}]"
            ))
        }
        ResponseItem::ToolSearchCall { .. } | ResponseItem::ToolSearchOutput { .. } => {
            note("[tool search activity from another provider was omitted]")
        }
        ResponseItem::WebSearchCall { .. } => {
            note("[a hosted web search was performed with another provider]")
        }
        ResponseItem::ImageGenerationCall { revised_prompt, .. } => note(&format!(
            "[an image was generated with another provider; prompt: {}]",
            revised_prompt.as_deref().unwrap_or("(none recorded)")
        )),
        ResponseItem::Compaction { .. } | ResponseItem::ContextCompaction { .. } => note(
            "[earlier context was compacted into an encrypted summary that is not available to \
this model]",
        ),
    }
}

fn note(text: &str) -> Classified<'static> {
    Classified::UserBlocks(vec![text_block(text)])
}

/// Text describing a tool output that can no longer answer a live tool call.
pub(crate) fn tool_output_note(call_id: &str, output: &FunctionCallOutputPayload) -> String {
    let status = if output.success == Some(false) {
        " (failed)"
    } else {
        ""
    };
    format!(
        "[output of tool call {call_id}{status}]\n{}",
        output.body.to_text().unwrap_or_default()
    )
}

/// MCP `CallToolResult` for a Codex tool output.
pub(crate) fn call_tool_result(output: &FunctionCallOutputPayload) -> Value {
    let mut content = Vec::new();
    match &output.body {
        FunctionCallOutputBody::Text(text) => content.push(mcp_text(text)),
        FunctionCallOutputBody::ContentItems(items) => {
            for item in items {
                match item {
                    FunctionCallOutputContentItem::InputText { text } => {
                        if !text.is_empty() {
                            content.push(mcp_text(text));
                        }
                    }
                    FunctionCallOutputContentItem::InputImage { image, .. } => {
                        content.push(mcp_image(image));
                    }
                    FunctionCallOutputContentItem::InputAudio { .. } => {
                        content.push(mcp_text("[audio output omitted: not supported by Claude]"));
                    }
                    FunctionCallOutputContentItem::EncryptedContent { .. } => {
                        content.push(mcp_text("[encrypted output omitted]"));
                    }
                }
            }
        }
    }
    if content.is_empty() {
        content.push(mcp_text(""));
    }
    json!({
        "content": content,
        "isError": output.success == Some(false),
    })
}

/// MCP `CallToolResult` carrying an error message.
pub(crate) fn error_tool_result(message: &str) -> Value {
    json!({
        "content": [mcp_text(message)],
        "isError": true,
    })
}

/// Appends user content to a tool result so it reaches the model in the same turn.
pub(crate) fn append_user_blocks(result: &mut Value, blocks: &[Value]) {
    let Some(content) = result.get_mut("content").and_then(Value::as_array_mut) else {
        return;
    };
    content.push(mcp_text("[New input arrived while this tool was running:]"));
    for block in blocks {
        content.push(mcp_content_from_block(block));
    }
}

fn mcp_text(text: &str) -> Value {
    let text = if text.trim().is_empty() {
        "(no output)"
    } else {
        text
    };
    json!({"type": "text", "text": text})
}

fn mcp_image(image: &ImageReference) -> Value {
    match image {
        ImageReference::Inline { image_url } => match parse_base64_data_url(image_url) {
            Some((media_type, data)) if SUPPORTED_IMAGE_TYPES.contains(&media_type) => {
                json!({"type": "image", "data": data, "mimeType": media_type})
            }
            Some((media_type, _)) => {
                mcp_text(&format!("[image omitted: unsupported type {media_type}]"))
            }
            None if image_url.starts_with("https://") || image_url.starts_with("http://") => {
                mcp_text(&format!("[image available at {image_url}]"))
            }
            None => mcp_text("[image omitted: unsupported image reference]"),
        },
        ImageReference::File { .. } => {
            mcp_text("[image omitted: stored in another provider's file store]")
        }
    }
}

fn mcp_content_from_block(block: &Value) -> Value {
    match block.get("type").and_then(Value::as_str) {
        Some("text") => mcp_text(block.get("text").and_then(Value::as_str).unwrap_or("")),
        Some("image") => {
            let source = block.get("source");
            let kind = source
                .and_then(|source| source.get("type"))
                .and_then(Value::as_str);
            match kind {
                Some("base64") => json!({
                    "type": "image",
                    "data": source.and_then(|s| s.get("data")).cloned().unwrap_or(Value::Null),
                    "mimeType": source
                        .and_then(|s| s.get("media_type"))
                        .cloned()
                        .unwrap_or(Value::Null),
                }),
                _ => {
                    let url = source
                        .and_then(|source| source.get("url"))
                        .and_then(Value::as_str)
                        .unwrap_or("(unknown)");
                    mcp_text(&format!("[image available at {url}]"))
                }
            }
        }
        _ => mcp_text(&block.to_string()),
    }
}

fn parse_base64_data_url(url: &str) -> Option<(&str, &str)> {
    let rest = url.strip_prefix("data:")?;
    let (meta, data) = rest.split_once(',')?;
    let media_type = meta.strip_suffix(";base64")?;
    Some((media_type, data))
}

/// Anthropic content block for a user content item.
fn content_block(item: &ContentItem) -> Option<Value> {
    match item {
        ContentItem::InputText { text } | ContentItem::OutputText { text, .. } => {
            (!text.trim().is_empty()).then(|| text_block(text))
        }
        ContentItem::InputImage { image, .. } => Some(image_block(image)),
        ContentItem::InputAudio { .. } => {
            Some(text_block("[audio input omitted: not supported by Claude]"))
        }
    }
}

fn image_block(image: &ImageReference) -> Value {
    match image {
        ImageReference::Inline { image_url } => match parse_base64_data_url(image_url) {
            Some((media_type, data)) if SUPPORTED_IMAGE_TYPES.contains(&media_type) => json!({
                "type": "image",
                "source": {"type": "base64", "media_type": media_type, "data": data},
            }),
            Some((media_type, _)) => {
                text_block(&format!("[image omitted: unsupported type {media_type}]"))
            }
            None if image_url.starts_with("https://") || image_url.starts_with("http://") => {
                json!({"type": "image", "source": {"type": "url", "url": image_url}})
            }
            None => text_block("[image omitted: unsupported image reference]"),
        },
        ImageReference::File { .. } => {
            text_block("[image omitted: stored in another provider's file store]")
        }
    }
}

pub(crate) fn text_block(text: &str) -> Value {
    json!({"type": "text", "text": text})
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

/// A stream-json user message line.
pub(crate) fn user_message_line(blocks: Vec<Value>) -> Value {
    json!({
        "type": "user",
        "message": {"role": "user", "content": blocks},
        "parent_tool_use_id": null,
        "session_id": "",
    })
}

/// Renders earlier history as a text transcript for seeding a fresh Claude Code session.
pub(crate) fn render_transcript(items: &[ResponseItem]) -> Option<String> {
    let mut translated = translate_history(items, /*strip_thinking*/ true);
    if translated
        .notes
        .iter()
        .any(|note| note.contains("a continuation request was added"))
    {
        // The real continuation is the new input that follows the transcript.
        translated.messages.pop();
    }
    let mut body = String::new();
    for message in &translated.messages {
        let rendered = render_message(message);
        if rendered.trim().is_empty() {
            continue;
        }
        if !body.is_empty() {
            body.push_str("\n\n");
        }
        body.push_str(&format!("[{}]\n{rendered}", message.role));
    }
    if body.trim().is_empty() || is_placeholder_only(&translated.messages) {
        return None;
    }
    let body = truncate_head(&body, TRANSCRIPT_CHAR_LIMIT);
    Some(format!(
        "<prior_conversation>\nThis Claude Code session was started fresh, so the earlier \
conversation is reproduced below as a transcript. Treat it as context and continue from where it \
left off; tool calls shown here already ran and must not be repeated.\n\n{body}\n</prior_conversation>"
    ))
}

/// `translate_history` inserts placeholder turns for empty histories.
fn is_placeholder_only(messages: &[AnthropicMessage]) -> bool {
    messages.len() == 1
        && messages[0].content.len() == 1
        && messages[0].content[0].get("text").and_then(Value::as_str)
            == Some("[no prior conversation]")
}

fn render_message(message: &AnthropicMessage) -> String {
    let mut parts = Vec::new();
    for block in &message.content {
        match block.get("type").and_then(Value::as_str).unwrap_or("") {
            "text" => {
                if let Some(text) = block.get("text").and_then(Value::as_str) {
                    parts.push(text.to_string());
                }
            }
            "image" => parts.push("[image]".to_string()),
            "tool_use" => {
                let name = block.get("name").and_then(Value::as_str).unwrap_or("tool");
                let id = block.get("id").and_then(Value::as_str).unwrap_or("");
                let input = block.get("input").map(Value::to_string).unwrap_or_default();
                parts.push(format!("[called tool {name} (call {id}) with {input}]"));
            }
            "tool_result" => {
                let id = block
                    .get("tool_use_id")
                    .and_then(Value::as_str)
                    .unwrap_or("");
                let status = if block.get("is_error").and_then(Value::as_bool) == Some(true) {
                    " (error)"
                } else {
                    ""
                };
                let text = match block.get("content") {
                    Some(Value::String(text)) => text.clone(),
                    Some(Value::Array(blocks)) => blocks
                        .iter()
                        .map(|block| match block.get("text").and_then(Value::as_str) {
                            Some(text) => text.to_string(),
                            None => "[image]".to_string(),
                        })
                        .collect::<Vec<_>>()
                        .join("\n"),
                    _ => String::new(),
                };
                parts.push(format!("[result of call {id}{status}]\n{text}"));
            }
            _ => {}
        }
    }
    parts.join("\n")
}

/// Keeps the last `limit` bytes of `text`, noting how much was dropped.
pub(crate) fn truncate_head(text: &str, limit: usize) -> String {
    if text.len() <= limit {
        return text.to_string();
    }
    let mut start = text.len() - limit;
    while !text.is_char_boundary(start) {
        start += 1;
    }
    format!(
        "[... {start} earlier characters of the conversation were omitted ...]\n{}",
        &text[start..]
    )
}

#[cfg(test)]
#[path = "history_tests.rs"]
mod tests;
