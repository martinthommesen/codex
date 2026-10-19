//! Experimental Claude subscription OAuth wire compatibility.
//!
//! Adapted from router-for-me/CLIProxyAPI at commit
//! `2422c6b0f5fe16495320fa29456493874c1977a0` (MIT; see oauth/LICENSE.CLIProxyAPI).
//! The source paths are claude_executor_request.go, claude_executor_cloaking.go,
//! claude_signing.go and helps/claude_mcp_alias.go under internal/runtime/executor/.
//!
//! These are observed, undocumented behaviors, including Claude Code client-identity emulation.
//! They are deliberately isolated from the supported Anthropic API-key transport. Compatibility
//! does not establish Anthropic's permission or prove subscription-quota treatment. This module
//! never supplies credentials, performs account rotation, requests paid fallback credits, or
//! starts another agent runtime. TLS fingerprint emulation and sensitive-word obfuscation from
//! the reference proxy are deliberately excluded.

mod signing;
mod tools;

use std::collections::BTreeMap;

use http::HeaderMap;
use http::HeaderName;
use http::HeaderValue;
use serde_json::Value;
use serde_json::json;
use sha2::Digest;
use sha2::Sha256;

pub use signing::normalize_cch_input;
pub use signing::sign_body;

/// Only origin to which the experimental provider may send subscription OAuth credentials.
pub const CLAUDE_OAUTH_MESSAGES_URL: &str = "https://api.anthropic.com/v1/messages?beta=true";
pub const CLIPROXYAPI_REFERENCE_COMMIT: &str = "2422c6b0f5fe16495320fa29456493874c1977a0";
const CLI_VERSION: &str = "2.1.280";
const CLI_IDENTITY: &str = "You are Claude Code, Anthropic's official CLI for Claude.";

/// Account identity comes from the OAuth profile, device identity from Codex's own secure
/// credential store, and session identity from the Codex conversation. None is inferred from
/// another provider's credentials. No access/refresh token belongs in this structure.
#[derive(Clone, Default)]
pub struct OAuthRequestContext {
    pub account_uuid: String,
    pub device_id: String,
    pub session_id: String,
    pub is_subagent: bool,
    pub previous_message_id: Option<String>,
    pub previous_request_id: Option<String>,
    pub prompt_id: Option<String>,
}

/// Final bytes and non-secret headers. Attach the selected OAuth bearer token separately and
/// send these exact bytes; serializing a parsed copy would invalidate CCH.
pub struct PreparedOAuthRequest {
    pub body: Vec<u8>,
    pub headers: HeaderMap,
    /// Request-local upstream alias -> original Anthropic wire name. Compose with the existing
    /// AnthropicToolNameMap so every call still passes through the Codex tool/approval runtime.
    pub tool_names: BTreeMap<String, String>,
}

#[derive(Debug, thiserror::Error)]
pub enum OAuthCompatibilityError {
    #[error("Claude OAuth request must be a JSON object")]
    InvalidRequest,
    #[error(
        "Claude OAuth requires a verified account UUID, a stored device ID and a conversation UUID"
    )]
    InvalidIdentity,
    #[error("Claude OAuth compatibility cannot represent this system block")]
    UnsupportedSystemBlock,
    #[error("Claude OAuth compatibility requires a user message to preserve system instructions")]
    MissingUserMessage,
    #[error(
        "Claude OAuth provider does not enable paid fast mode or server-side billing fallbacks"
    )]
    PaidFallbackNotAllowed,
    #[error("Claude OAuth request has no valid billing checksum placeholder")]
    MissingBillingChecksum,
    #[error("invalid Claude OAuth checksum input")]
    InvalidChecksumInput,
    #[error("cannot allocate a unique Claude OAuth tool alias")]
    InvalidToolAlias,
    #[error("failed to serialize Claude OAuth compatibility request")]
    Json(#[from] serde_json::Error),
    #[error("invalid Claude OAuth compatibility header")]
    Header(#[from] http::header::InvalidHeaderValue),
}

/// Prepare an existing Codex-to-Anthropic translation for the experimental OAuth endpoint.
/// All semantic transformations precede final serialization and byte-sensitive signing.
pub fn prepare_oauth_request(
    mut request: Value,
    context: &OAuthRequestContext,
) -> Result<PreparedOAuthRequest, OAuthCompatibilityError> {
    if !request.is_object() {
        return Err(OAuthCompatibilityError::InvalidRequest);
    }
    validate_identity(context)?;
    if request.get("fallbacks").is_some()
        || request.get("fallback_credit_token").is_some()
        || request["speed"]
            .as_str()
            .is_some_and(|speed| speed.eq_ignore_ascii_case("fast"))
    {
        return Err(OAuthCompatibilityError::PaidFallbackNotAllowed);
    }
    let raw_model = request["model"]
        .as_str()
        .unwrap_or_default()
        .trim()
        .to_ascii_lowercase();
    let model = raw_model.rsplit('/').next().unwrap_or_default();
    let fingerprint_text = first_user_text(&request).to_owned();
    let billing = billing_header(&fingerprint_text, context);
    relocate_system(&mut request, model, billing)?;
    apply_identity(&mut request, context)?;
    normalize_thinking(&mut request, model);
    // The reference strips foreign sampling knobs before contacting Anthropic.
    if let Some(object) = request.as_object_mut() {
        object.remove("temperature");
        object.remove("top_p");
        if matches!(
            object.get("thinking").and_then(|v| v["type"].as_str()),
            Some("enabled" | "adaptive" | "auto")
        ) {
            object.remove("top_k");
        }
        object.insert("stream".into(), Value::Bool(true));
        object.remove("betas");
    }
    // Use one caller-stable namespace across requests, sessions and refreshes. The source uses
    // its downstream proxy caller key; Codex has no such key, so use its stable device identity.
    let tool_names = tools::remap_tools(&mut request, &context.device_id)?;
    normalize_caching(&mut request, context.is_subagent);
    let headers = headers_for_request(&request, context)?;
    let mut body = serde_json::to_vec(&request)?;
    sign_body(&mut body)?;
    Ok(PreparedOAuthRequest {
        body,
        headers,
        tool_names,
    })
}

fn validate_identity(context: &OAuthRequestContext) -> Result<(), OAuthCompatibilityError> {
    if uuid::Uuid::parse_str(&context.account_uuid).is_err()
        || uuid::Uuid::parse_str(&context.session_id).is_err()
        || context.device_id.len() != 64
        || !context
            .device_id
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
    {
        return Err(OAuthCompatibilityError::InvalidIdentity);
    }
    Ok(())
}

fn apply_identity(
    request: &mut Value,
    context: &OAuthRequestContext,
) -> Result<(), OAuthCompatibilityError> {
    // Serialize explicitly to retain the reference identity field order inside this JSON string.
    #[derive(serde::Serialize)]
    struct Identity<'a> {
        device_id: &'a str,
        account_uuid: &'a str,
        session_id: &'a str,
    }
    let identity = serde_json::to_string(&Identity {
        device_id: &context.device_id,
        account_uuid: &context.account_uuid,
        session_id: &context.session_id,
    })?;
    if !request["metadata"].is_object() {
        request["metadata"] = json!({});
    }
    request["metadata"]["user_id"] = Value::String(identity);
    if let Some(previous) = &context.previous_message_id {
        request["diagnostics"] = json!({"previous_message_id": previous});
    } else {
        request["diagnostics"] = json!({"previous_message_id": null});
    }
    Ok(())
}

fn first_user_text(request: &Value) -> &str {
    let Some(messages) = request["messages"].as_array() else {
        return "";
    };
    let Some(message) = messages.iter().find(|message| message["role"] == "user") else {
        return "";
    };
    if let Some(text) = message["content"].as_str() {
        return text;
    }
    message["content"]
        .as_array()
        .and_then(|blocks| {
            blocks.iter().find_map(|block| {
                (block["type"] == "text")
                    .then(|| block["text"].as_str())
                    .flatten()
            })
        })
        .unwrap_or_default()
}

/// The build fingerprint samples UTF-16 code units exactly as the observed JS implementation.
pub fn billing_fingerprint(message: &str, version: &str) -> String {
    let units: Vec<u16> = message.encode_utf16().collect();
    let sampled = [4, 7, 20].map(|index| units.get(index).copied().unwrap_or(u16::from(b'0')));
    let input = format!(
        "59cf53e54c78{}{version}",
        String::from_utf16_lossy(&sampled)
    );
    let hash = Sha256::digest(input.as_bytes());
    format!("{:02x}{:01x}", hash[0], hash[1] >> 4)
}

fn billing_header(message: &str, context: &OAuthRequestContext) -> String {
    let mut header = format!(
        "x-anthropic-billing-header: cc_version={CLI_VERSION}.{}; cc_entrypoint=cli; cch=00000;",
        billing_fingerprint(message, CLI_VERSION)
    );
    if context.is_subagent {
        header.push_str(" cc_is_subagent=true;");
    }
    if let Some(previous) = &context.previous_request_id {
        header.push_str(&format!(" cc_prev_req={previous};"));
    }
    if let Some(prompt) = &context.prompt_id {
        header.push_str(&format!(" cc_prompt_id={prompt};"));
    }
    if !context.is_subagent {
        header.push_str(" cc_turn_origin=human;");
    }
    header
}

fn legacy_system(model: &str) -> bool {
    matches!(
        model,
        "claude-3-5-haiku-20241022"
            | "claude-3-5-haiku-latest"
            | "claude-3-7-sonnet-20250219"
            | "claude-3-7-sonnet-latest"
            | "claude-haiku-4-5"
            | "claude-haiku-4-5-20251001"
            | "claude-opus-4"
            | "claude-opus-4-20250514"
            | "claude-opus-4-1"
            | "claude-opus-4-1-20250805"
            | "claude-opus-4-5"
            | "claude-opus-4-5-20251101"
            | "claude-opus-4-6"
            | "claude-opus-4-7"
            | "claude-sonnet-4"
            | "claude-sonnet-4-20250514"
            | "claude-sonnet-4-5"
            | "claude-sonnet-4-5-20250929"
            | "claude-sonnet-4-6"
    )
}

fn relocate_system(
    request: &mut Value,
    model: &str,
    billing: String,
) -> Result<(), OAuthCompatibilityError> {
    let existing = request
        .get_mut("system")
        .map(Value::take)
        .unwrap_or(Value::Null);
    let blocks = match existing {
        Value::String(text) if !text.trim().is_empty() => vec![json!({"type":"text","text":text})],
        Value::Array(blocks) => blocks,
        Value::Null | Value::String(_) => Vec::new(),
        _ => return Err(OAuthCompatibilityError::UnsupportedSystemBlock),
    };
    if blocks
        .iter()
        .any(|block| block["type"] != "text" || !block["text"].is_string())
    {
        return Err(OAuthCompatibilityError::UnsupportedSystemBlock);
    }
    request["system"] = json!([
        {"type":"text","text":billing},
        {"type":"text","text":CLI_IDENTITY,"cache_control":{"type":"ephemeral"}},
    ]);
    if blocks.is_empty() {
        return Ok(());
    }
    let messages = request
        .get_mut("messages")
        .and_then(Value::as_array_mut)
        .ok_or(OAuthCompatibilityError::MissingUserMessage)?;
    let first_user = messages
        .iter()
        .position(|message| message["role"] == "user")
        .ok_or(OAuthCompatibilityError::MissingUserMessage)?;
    if legacy_system(model) {
        let content = &mut messages[first_user]["content"];
        if let Some(text) = content.as_str() {
            *content = json!([{"type":"text","text":text}]);
        }
        let content = content
            .as_array_mut()
            .ok_or(OAuthCompatibilityError::MissingUserMessage)?;
        let insert_at = content
            .iter()
            .take_while(|block| block["type"] == "tool_result")
            .count();
        let reminders = blocks.into_iter().map(|mut block| {
            let text = block["text"].as_str().unwrap_or_default();
            block["text"] = Value::String(format!(
                "<system-reminder>\n{text}{}</system-reminder>",
                if text.ends_with('\n') { "" } else { "\n" }
            ));
            block
        });
        content.splice(insert_at..insert_at, reminders);
    } else {
        let mut insert_at = first_user + 1;
        while insert_at < messages.len()
            && (messages[insert_at]["role"] == "user"
                || (messages[insert_at]["role"] == "system"
                    && messages[insert_at].get("output_config").is_some()
                    && messages[insert_at]["content"]
                        .as_array()
                        .is_some_and(Vec::is_empty)))
        {
            insert_at += 1;
        }
        messages.splice(
            insert_at..insert_at,
            blocks.into_iter().map(|mut block| {
                if block.get("cache_control").is_none() {
                    block["cache_control"] = json!({"type":"ephemeral"});
                }
                json!({"role":"system","content":[block]})
            }),
        );
    }
    Ok(())
}

fn progress_model(model: &str) -> bool {
    model.starts_with("claude-opus-5-5")
        || model.starts_with("claude-fable-5-1")
        || model.starts_with("claude-sonnet-5")
}

fn normalize_thinking(request: &mut Value, model: &str) {
    if matches!(
        request["tool_choice"]["type"].as_str(),
        Some("any" | "tool")
    ) {
        if let Some(object) = request.as_object_mut() {
            object.remove("thinking");
        }
        if let Some(config) = request
            .get_mut("output_config")
            .and_then(Value::as_object_mut)
        {
            config.remove("effort");
        }
        return;
    }
    if matches!(
        request["thinking"]["type"].as_str(),
        Some("enabled" | "adaptive")
    ) {
        if progress_model(model) && request["thinking"].get("display").is_none() {
            request["thinking"]["display"] = Value::String("updates".into());
        }
        if request.get("context_management").is_none() {
            request["context_management"] =
                json!({"edits":[{"type":"clear_thinking_20251015","keep":"all"}]});
        }
    }
}

fn visit_cache_blocks(request: &mut Value, mut visit: impl FnMut(&mut Value)) {
    if let Some(tools) = request.get_mut("tools").and_then(Value::as_array_mut) {
        for tool in tools {
            visit(tool);
        }
    }
    if let Some(system) = request.get_mut("system").and_then(Value::as_array_mut) {
        for block in system {
            visit(block);
        }
    }
    if let Some(messages) = request.get_mut("messages").and_then(Value::as_array_mut) {
        for message in messages {
            if let Some(content) = message.get_mut("content").and_then(Value::as_array_mut) {
                for block in content {
                    visit(block);
                }
            }
        }
    }
}

fn normalize_caching(request: &mut Value, subagent: bool) {
    // Codex's API-key builder requests automatic caching at the top level. The observed OAuth
    // path instead places explicit section breakpoints. Do not combine both mechanisms.
    if let Some(object) = request.as_object_mut() {
        object.remove("cache_control");
    }
    if let Some(messages) = request.get_mut("messages").and_then(Value::as_array_mut) {
        for message in messages.iter_mut().rev() {
            if !matches!(message["role"].as_str(), Some("user" | "assistant")) {
                continue;
            }
            let is_assistant = message["role"] == "assistant";
            if let Some(text) = message["content"].as_str() {
                message["content"] = json!([{"type":"text","text":text}]);
            }
            if let Some(content) = message["content"].as_array_mut() {
                if content.last().is_none_or(|block| {
                    is_assistant
                        && matches!(
                            block["type"].as_str(),
                            Some("thinking" | "redacted_thinking")
                        )
                }) {
                    continue;
                }
                if !content
                    .iter()
                    .any(|block| block.get("cache_control").is_some())
                    && let Some(last) = content.last_mut()
                {
                    last["cache_control"] = json!({"type":"ephemeral"});
                }
                break;
            }
        }
    }
    // Match the reference's section-aware breakpoint removal priority.
    enforce_cache_limit(request, 4);
    let mut seen_5m = false;
    visit_cache_blocks(request, |block| {
        if matches!(
            block["type"].as_str(),
            Some("thinking" | "redacted_thinking")
        ) {
            if let Some(object) = block.as_object_mut() {
                object.remove("cache_control");
            }
            return;
        }
        if let Some(cache) = block
            .get_mut("cache_control")
            .and_then(Value::as_object_mut)
        {
            if subagent {
                cache.remove("ttl");
            } else if !cache.contains_key("ttl") {
                cache.insert("ttl".into(), Value::String("1h".into()));
            }
            if cache.get("ttl").and_then(Value::as_str) != Some("1h") {
                seen_5m = true;
            } else if seen_5m {
                cache.remove("ttl");
            }
        }
    });
}

fn enforce_cache_limit(request: &mut Value, limit: usize) {
    let cache_paths = |section: &str| {
        request[section]
            .as_array()
            .into_iter()
            .flatten()
            .enumerate()
            .filter(|(_, block)| block["cache_control"]["type"] == "ephemeral")
            .map(|(index, _)| format!("/{section}/{index}"))
            .collect::<Vec<_>>()
    };
    let system = cache_paths("system");
    let tools = cache_paths("tools");
    let mut messages = Vec::new();
    if let Some(turns) = request["messages"].as_array() {
        for (turn_index, turn) in turns.iter().enumerate() {
            if let Some(content) = turn["content"].as_array() {
                for (index, block) in content.iter().enumerate() {
                    if block["cache_control"]["type"] == "ephemeral" {
                        messages.push(format!("/messages/{turn_index}/content/{index}"));
                    }
                }
            }
        }
    }
    let total = system.len() + tools.len() + messages.len();
    let priority = system
        .iter()
        .take(system.len().saturating_sub(1))
        .chain(tools.iter().take(tools.len().saturating_sub(1)))
        .chain(messages.iter())
        .chain(system.last())
        .chain(tools.last());
    for path in priority.take(total.saturating_sub(limit)) {
        if let Some(block) = request.pointer_mut(path).and_then(Value::as_object_mut) {
            block.remove("cache_control");
        }
    }
}

fn headers_for_request(
    request: &Value,
    context: &OAuthRequestContext,
) -> Result<HeaderMap, OAuthCompatibilityError> {
    let mut headers = HeaderMap::new();
    for (name, value) in [
        ("content-type", "application/json"),
        ("accept", "application/json"),
        ("anthropic-version", "2023-06-01"),
        ("anthropic-dangerous-direct-browser-access", "true"),
        ("x-app", "cli"),
        ("x-stainless-retry-count", "0"),
        ("x-stainless-runtime", "node"),
        ("x-stainless-lang", "js"),
        ("x-stainless-timeout", "600"),
        ("x-stainless-package-version", "0.112.1"),
        ("x-stainless-runtime-version", "v26.3.0"),
        ("x-stainless-os", "MacOS"),
        ("x-stainless-arch", "arm64"),
        ("user-agent", "claude-cli/2.1.280 (external, cli)"),
    ] {
        headers.insert(
            HeaderName::from_static(name),
            HeaderValue::from_static(value),
        );
    }
    headers.insert(
        HeaderName::from_static("x-claude-code-session-id"),
        HeaderValue::from_str(&context.session_id)?,
    );
    headers.insert(
        HeaderName::from_static("x-client-request-id"),
        HeaderValue::from_str(&uuid::Uuid::new_v4().to_string())?,
    );
    headers.insert(
        HeaderName::from_static("anthropic-beta"),
        HeaderValue::from_str(&beta_header(request, context.is_subagent))?,
    );
    // Let the actual Rust HTTP transport negotiate only encodings it can decode. The reference's
    // gzip/deflate/br/zstd offer must not be copied without matching decompression support.
    Ok(headers)
}

fn beta_header(request: &Value, subagent: bool) -> String {
    let raw_model = request["model"]
        .as_str()
        .unwrap_or_default()
        .trim()
        .to_ascii_lowercase();
    let model = raw_model.rsplit('/').next().unwrap_or_default();
    let mut betas = vec!["claude-code-20250219", "oauth-2025-04-20"];
    if model.contains("[1m]") {
        betas.push("context-1m-2025-08-07");
    }
    betas.push("interleaved-thinking-2025-05-14");
    if request["thinking"]["display"]
        .as_str()
        .is_none_or(str::is_empty)
    {
        betas.push("redact-thinking-2026-02-12");
    }
    betas.extend([
        "thinking-token-count-2026-05-13",
        "context-management-2025-06-27",
        "prompt-caching-scope-2026-01-05",
    ]);
    let messages = request["messages"]
        .as_array()
        .map(Vec::as_slice)
        .unwrap_or_default();
    if !legacy_system(model) {
        betas.push("mid-conversation-system-2026-04-07");
    }
    let per_turn = model.starts_with("claude-opus-5-5") || model.starts_with("claude-fable-5-1");
    if per_turn {
        betas.push("per-turn-control-2026-07-01");
    }
    if (per_turn || model.starts_with("claude-mythos-5-1"))
        && (request.pointer("/output_config/timing").is_some()
            || messages
                .iter()
                .any(|message| message.pointer("/output_config/timing").is_some()))
    {
        betas.push("timing-2026-09-09");
    }
    if !legacy_system(model) {
        if model != "claude-sonnet-5"
            && (!model.starts_with("claude-sonnet-5-") || model.starts_with("claude-sonnet-5-5"))
        {
            betas.push("mid-conversation-tool-changes-2026-07-01");
        }
        if messages.iter().any(|message| {
            message["content"].as_array().is_some_and(|blocks| {
                blocks.iter().any(|block| {
                    block["type"] == "tool_addition" && block.pointer("/tool/definition").is_some()
                })
            })
        }) {
            betas.push("inline-tools-2026-09-15");
        }
    }
    let tools = request["tools"]
        .as_array()
        .map(Vec::as_slice)
        .unwrap_or_default();
    if tools.iter().any(|tool| {
        tool["type"]
            .as_str()
            .is_some_and(|kind| kind.starts_with("advisor_"))
    }) {
        betas.push("advisor-tool-2026-03-01");
    }
    if tools.iter().any(|tool| {
        tool["type"]
            .as_str()
            .is_some_and(|kind| kind.starts_with("tool_search_tool_"))
            || tool["defer_loading"] == true
            || tool.get("input_examples").is_some()
            || tool.get("allowed_callers").is_some()
    }) {
        betas.push("advanced-tool-use-2025-11-20");
    }
    if !legacy_system(model)
        && (progress_model(model)
            || messages
                .iter()
                .any(|message| message.get("clear_at").is_some()))
    {
        betas.push("mid-conversation-system-clear-at-2026-08-21");
    }
    if request.get("safeguards").is_some() {
        betas.push("dangerous-tool-use-2026-09-03");
    }
    if !model.contains("haiku") && request["thinking"]["type"] != "disabled" {
        betas.push("effort-2025-11-24");
    }
    if request.pointer("/output_config/format").is_some() {
        betas.push("structured-outputs-2025-12-15");
    }
    if request.pointer("/thinking/block_binding").is_some()
        || (progress_model(model) && request["thinking"]["type"] == "adaptive")
    {
        betas.push("thinking-binding-controls-2026-08-01");
    }
    if request["thinking"]["type"] != "disabled" && request["thinking"]["display"] == "updates" {
        betas.push("thinking-display-updates-2026-08-18");
    }
    if !subagent {
        betas.push("extended-cache-ttl-2025-04-11");
    }
    if request.get("diagnostics").is_some() {
        betas.push("cache-diagnosis-2026-04-07");
    }
    betas.join(",")
}

#[cfg(test)]
mod tests;
