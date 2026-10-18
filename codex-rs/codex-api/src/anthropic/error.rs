//! Maps Anthropic API failures onto Codex [`ApiError`]s.
//!
//! Anthropic reports errors as `{"type": "error", "error": {"type": ..., "message": ...}}` both in
//! HTTP error bodies and as in-stream `error` events. Credentials are never part of these
//! payloads, but messages are still truncated before they reach logs or the UI.

use crate::error::ApiError;
use codex_client::TransportError;
use http::StatusCode;
use serde_json::Value;

const MAX_ERROR_MESSAGE_CHARS: usize = 2_000;

/// Returns whether a 400 response says the replayed thinking blocks were rejected.
pub fn is_thinking_replay_rejection(err: &ApiError) -> bool {
    let message = match err {
        ApiError::InvalidRequest { message } => message,
        _ => return false,
    };
    let lower = message.to_ascii_lowercase();
    (lower.contains("thinking") && (lower.contains("signature") || lower.contains("block")))
        || lower.contains("block_binding")
        || lower.contains(super::request::THINKING_BINDING_BETA)
}

/// Converts an in-stream `error` event.
pub fn anthropic_stream_error(kind: &str, message: &str) -> ApiError {
    let message = truncate(message);
    match kind {
        // Codex treats ServerOverloaded as terminal without Retry-After advice, so overload is
        // reported as a retryable stream failure to get exponential backoff.
        "overloaded_error" => ApiError::Retryable {
            message: format!("Anthropic overloaded: {message}"),
            retry_after: None,
        },
        "rate_limit_error" => ApiError::RateLimitExceeded {
            message,
            retry_after: None,
        },
        "api_error" | "timeout_error" => ApiError::Retryable {
            message: format!("Anthropic {kind}: {message}"),
            retry_after: None,
        },
        "invalid_request_error" if is_prompt_too_long(&message) => ApiError::ContextWindowExceeded,
        "invalid_request_error" => ApiError::InvalidRequest { message },
        "billing_error" => ApiError::QuotaExceeded,
        other => ApiError::Stream(format!("Anthropic {other}: {message}")),
    }
}

/// Converts a transport failure from `POST /v1/messages` into an Anthropic-aware error.
pub fn map_anthropic_transport_error(err: ApiError) -> ApiError {
    let ApiError::Transport(TransportError::Http {
        status,
        body,
        retry_after,
        ..
    }) = err
    else {
        return err;
    };
    let body = body.unwrap_or_default();
    let (kind, message) = parse_error_body(&body);
    let message = truncate(&message);
    match status.as_u16() {
        400 if is_prompt_too_long(&message) => ApiError::ContextWindowExceeded,
        400 | 404 | 422 => ApiError::InvalidRequest {
            message: format!("Anthropic rejected the request ({status}): {message}"),
        },
        // Authentication and permission failures will not succeed on retry.
        401 => ApiError::InvalidRequest {
            message: format!(
                "Anthropic authentication failed: {message}. Run `codex login --provider anthropic` \
to store a valid API key, or set ANTHROPIC_API_KEY."
            ),
        },
        403 => ApiError::InvalidRequest {
            message: format!("Anthropic denied the request: {message}"),
        },
        413 => ApiError::InvalidRequest {
            message: format!("Anthropic request too large: {message}"),
        },
        429 => ApiError::RateLimitExceeded {
            message: format!("Anthropic rate limit: {message}"),
            retry_after,
        },
        529 => ApiError::Retryable {
            message: format!("Anthropic overloaded: {message}"),
            retry_after,
        },
        _ if kind == "overloaded_error" => ApiError::Retryable {
            message: format!("Anthropic overloaded: {message}"),
            retry_after,
        },
        code if (500..600).contains(&code) => ApiError::Retryable {
            message: format!("Anthropic server error ({status}): {message}"),
            retry_after,
        },
        _ => ApiError::Api {
            status: StatusCode::from_u16(status.as_u16()).unwrap_or(status),
            message,
        },
    }
}

fn parse_error_body(body: &str) -> (String, String) {
    match serde_json::from_str::<Value>(body) {
        Ok(value) => {
            let error = value.get("error").unwrap_or(&value);
            let kind = error
                .get("type")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string();
            let message = error
                .get("message")
                .and_then(Value::as_str)
                .unwrap_or(body)
                .to_string();
            (kind, message)
        }
        Err(_) => (String::new(), body.to_string()),
    }
}

fn is_prompt_too_long(message: &str) -> bool {
    let lower = message.to_ascii_lowercase();
    lower.contains("prompt is too long")
        || lower.contains("context window")
        || lower.contains("too many total text bytes")
}

fn truncate(message: &str) -> String {
    if message.chars().count() <= MAX_ERROR_MESSAGE_CHARS {
        return message.to_string();
    }
    let mut truncated: String = message.chars().take(MAX_ERROR_MESSAGE_CHARS).collect();
    truncated.push('…');
    truncated
}

#[cfg(test)]
#[path = "error_tests.rs"]
mod tests;
