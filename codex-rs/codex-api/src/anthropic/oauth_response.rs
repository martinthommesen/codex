//! Conservative response handling for the experimental subscription OAuth transport.
//!
//! The undocumented unified rate-limit fields and reset selection follow
//! CLIProxyAPI `helps/claude_ratelimit.go` at
//! `2422c6b0f5fe16495320fa29456493874c1977a0` (MIT; see the accompanying notice).
//! A server-reported utilization is evidence of a limit, never proof of which pool
//! paid for a request. In particular, an overage reset can be a billing boundary.

use crate::error::ApiError;
use chrono::DateTime;
use chrono::Utc;
use codex_client::TransportError;
use codex_http_client::RetryAfter;
use codex_protocol::protocol::RateLimitSnapshot;
use codex_protocol::protocol::RateLimitWindow;
use http::HeaderMap;
use serde_json::Value;
use std::collections::BTreeMap;
use std::fmt;
use std::time::Duration;

const PREFIX: &str = "anthropic-ratelimit-unified";
const MAX_HEADER_BYTES: usize = 128;
const MAX_RESET_DELAY_SECONDS: f64 = (7 * 24 * 60 * 60 + 60 * 60) as f64;
const BILLING_UNCONFIRMED: &str = "Claude OAuth: Anthropic has not confirmed which billing or subscription pool paid for this request. Check Claude account Usage and extra usage settings; OAuth authentication alone does not establish subscription-quota treatment.";
const POLICY_DETAILS_WITHHELD: &str = "Anthropic declined the Claude OAuth request under a server policy. Server details are withheld to protect credentials.";

/// Allowlisted, bounded server metadata. This contains no request identity or credentials.
///
/// Debug intentionally reports only the number of fields, so unknown server values
/// in a known header can never be echoed accidentally through diagnostics.
#[derive(Clone, Default)]
pub struct ClaudeOAuthResponseMetadata {
    pub rate_limits: Vec<RateLimitSnapshot>,
    pub headers: BTreeMap<String, String>,
}

impl fmt::Debug for ClaudeOAuthResponseMetadata {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ClaudeOAuthResponseMetadata")
            .field("rate_limit_windows", &self.rate_limits.len())
            .field("allowlisted_headers", &self.headers.len())
            .finish()
    }
}

impl ClaudeOAuthResponseMetadata {
    pub fn from_headers(headers: &HeaderMap) -> Self {
        let mut preserved = BTreeMap::new();
        let mut preserve = |name: &str| {
            if let Some(value) = single_header(headers, name)
                && safe_header_value(name, value)
            {
                let value = if name.ends_with("-status")
                    || name.ends_with("-claim")
                    || name.ends_with("-reason")
                {
                    value.to_ascii_lowercase()
                } else {
                    value.to_owned()
                };
                preserved.insert(name.to_owned(), value);
            }
        };
        for window in ["", "-5h", "-7d", "-7d_oi", "-overage"] {
            for field in ["status", "reset", "utilization"] {
                // The reference has no unified-utilization field.
                if !window.is_empty() || field != "utilization" {
                    preserve(&format!("{PREFIX}{window}-{field}"));
                }
            }
        }
        preserve("anthropic-ratelimit-unified-representative-claim");
        preserve("anthropic-ratelimit-unified-overage-disabled-reason");
        preserve("retry-after");

        let mut metadata = Self {
            rate_limits: Vec::new(),
            headers: preserved,
        };
        let primary = metadata.window("5h", Some(5 * 60));
        let secondary = metadata.window("7d", Some(7 * 24 * 60));
        if primary.is_some() || secondary.is_some() {
            metadata.rate_limits.push(snapshot(
                "claude_oauth",
                "Claude OAuth (server reported; billing unconfirmed)",
                primary,
                secondary,
            ));
        }
        for (window, minutes, name) in [
            (
                "7d_oi",
                Some(7 * 24 * 60),
                "Claude OAuth 7d_oi (server reported; billing unconfirmed)",
            ),
            (
                "overage",
                None,
                "Claude OAuth overage (server reported; billing unconfirmed)",
            ),
        ] {
            if let Some(limit) = metadata.window(window, minutes) {
                metadata.rate_limits.push(snapshot(
                    &format!("claude_oauth_{window}"),
                    name,
                    Some(limit),
                    None,
                ));
            }
        }
        metadata
    }

    /// No observed response header independently proves a request's charged pool.
    pub fn warning_summary(&self) -> Option<String> {
        let overage = self
            .headers
            .keys()
            .any(|name| name.starts_with("anthropic-ratelimit-unified-overage-"))
            || self
                .get("representative-claim")
                .is_some_and(|claim| claim.contains("overage"));
        if overage {
            Some(format!(
                "Claude OAuth: Anthropic returned an overage or extra-usage signal. This may involve separately billed usage; Codex cannot confirm the charged pool. Disable extra usage in Claude account settings if separately billed usage is unacceptable. {BILLING_UNCONFIRMED}"
            ))
        } else {
            Some(BILLING_UNCONFIRMED.to_owned())
        }
    }

    /// Conservative retry advice, excluding monthly overage billing boundaries.
    ///
    /// This never rotates accounts, changes providers, or switches billing modes.
    pub fn retry_after(&self) -> Option<RetryAfter> {
        let delay = self.retry_delay_at(Utc::now().timestamp() as f64)?;
        RetryAfter::from_delay(delay)
    }

    fn get(&self, suffix: &str) -> Option<&str> {
        self.headers
            .get(&format!("{PREFIX}-{suffix}"))
            .map(String::as_str)
    }

    fn window(&self, window: &str, window_minutes: Option<i64>) -> Option<RateLimitWindow> {
        let utilization = parse_utilization(self.get(&format!("{window}-utilization"))?)?;
        Some(RateLimitWindow {
            used_percent: utilization * 100.0,
            window_minutes,
            resets_at: self
                .get(&format!("{window}-reset"))
                .and_then(parse_timestamp)
                .map(|seconds| seconds.floor() as i64),
        })
    }

    fn overage_only_rejection(&self) -> bool {
        let status5h = self.get("5h-status").unwrap_or_default();
        let status7d = self.get("7d-status").unwrap_or_default();
        if status5h == "rejected" || status7d == "rejected" {
            return false;
        }
        let overage_claim = self
            .get("representative-claim")
            .is_some_and(|claim| claim.contains("overage"));
        let overage = self.get("7d_oi-status") == Some("rejected")
            || self.get("overage-status") == Some("rejected")
            || self.get("overage-disabled-reason").is_some()
            || overage_claim;
        if !overage {
            return false;
        }
        let healthy = |window: &str| {
            self.get(&format!("{window}-utilization"))
                .and_then(parse_utilization)
                .is_some_and(|value| value < 1.0)
        };
        if (allowed(status5h) && allowed(status7d))
            || (allowed(status7d) && status5h.is_empty() && healthy("5h"))
            || (allowed(status5h) && status7d.is_empty() && healthy("7d"))
        {
            return true;
        }
        self.get("status") == Some("rejected")
            && status5h.is_empty()
            && status7d.is_empty()
            && overage_claim
            && (self.get("5h-utilization").is_none() || healthy("5h"))
            && (self.get("7d-utilization").is_none() || healthy("7d"))
    }

    fn retry_delay_at(&self, now: f64) -> Option<Duration> {
        let overage_only = self.overage_only_rejection();
        let mut deadlines = Vec::new();
        if !overage_only
            && let Some(raw) = self.headers.get("retry-after")
            && let Some(delay) = parse_retry_after(raw, now)
        {
            deadlines.push(now + delay);
        }
        for window in ["5h", "7d", "7d_oi"] {
            if self.get(&format!("{window}-status")) == Some("rejected")
                && (window != "7d_oi" || !overage_only)
                && let Some(reset) = self
                    .get(&format!("{window}-reset"))
                    .and_then(parse_timestamp)
            {
                deadlines.push(reset);
            }
        }
        let unified_rejected = !overage_only
            && (self.get("status") == Some("rejected")
                || ["5h-status", "7d-status", "7d_oi-status"]
                    .iter()
                    .any(|field| self.get(field) == Some("rejected"))
                || (self.get("status").is_none()
                    && !allowed(self.get("5h-status").unwrap_or_default())
                    && !allowed(self.get("7d-status").unwrap_or_default())));
        if unified_rejected && let Some(reset) = self.get("reset").and_then(parse_timestamp) {
            let billing_boundary = self
                .get("representative-claim")
                .is_some_and(|claim| claim.contains("overage"))
                && self.get("overage-reset").and_then(parse_timestamp) == Some(reset);
            if !billing_boundary {
                deadlines.push(reset);
            }
        }
        deadlines
            .into_iter()
            .map(|deadline| deadline - now)
            .filter(|delay| *delay > 0.0 && *delay <= MAX_RESET_DELAY_SECONDS)
            .max_by(f64::total_cmp)
            .and_then(|delay| Duration::try_from_secs_f64(delay).ok())
    }
}

fn snapshot(
    id: &str,
    name: &str,
    primary: Option<RateLimitWindow>,
    secondary: Option<RateLimitWindow>,
) -> RateLimitSnapshot {
    RateLimitSnapshot {
        limit_id: Some(id.to_owned()),
        limit_name: Some(name.to_owned()),
        normal_model_slug: None,
        primary,
        secondary,
        credits: None,
        individual_limit: None,
        spend_control_reached: None,
        plan_type: None,
        rate_limit_reached_type: None,
    }
}

fn single_header<'a>(headers: &'a HeaderMap, name: &str) -> Option<&'a str> {
    let mut values = headers.get_all(name).iter();
    let value = values.next()?.to_str().ok()?.trim();
    if values.next().is_some() || value.is_empty() || value.len() > MAX_HEADER_BYTES {
        return None;
    }
    Some(value)
}

fn safe_header_value(name: &str, value: &str) -> bool {
    if name.ends_with("-status") {
        matches!(
            value.to_ascii_lowercase().as_str(),
            "allowed" | "allowed_warning" | "rejected"
        )
    } else if name.ends_with("-utilization") {
        parse_utilization(value).is_some()
    } else if name.ends_with("-reset") {
        parse_timestamp(value).is_some()
    } else if name == "retry-after" {
        parse_retry_after(value, 0.0).is_some()
    } else {
        // Preserve bounded opaque diagnostic codes, without printing them. Their
        // spelling is deliberately not treated as a guaranteed protocol enum.
        value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-' | b'.'))
    }
}

fn allowed(value: &str) -> bool {
    matches!(value, "allowed" | "allowed_warning")
}

fn parse_utilization(raw: &str) -> Option<f64> {
    raw.parse::<f64>()
        .ok()
        .filter(|value| value.is_finite() && *value >= 0.0 && (value * 100.0).is_finite())
}

fn parse_timestamp(raw: &str) -> Option<f64> {
    if let Ok(seconds) = raw.parse::<f64>() {
        return (seconds.is_finite() && seconds > 0.0 && seconds < i64::MAX as f64)
            .then_some(seconds);
    }
    DateTime::parse_from_rfc3339(raw)
        .or_else(|_| DateTime::parse_from_rfc2822(raw))
        .ok()
        .map(|date| date.timestamp() as f64)
        .filter(|seconds| *seconds > 0.0)
}

fn parse_retry_after(raw: &str, now: f64) -> Option<f64> {
    if let Ok(seconds) = raw.parse::<f64>() {
        return (seconds.is_finite() && seconds >= 0.0).then_some(seconds);
    }
    parse_timestamp(raw).map(|deadline| (deadline - now).max(0.0))
}

/// Maps errors without echoing server bodies, URLs, or secret-bearing headers.
///
/// OAuth failures never suggest or trigger an API-key, CLI, account, or provider fallback.
pub fn map_claude_oauth_transport_error(err: ApiError) -> ApiError {
    match err {
        ApiError::Transport(TransportError::Http {
            status,
            body,
            headers,
            retry_after,
            ..
        }) => {
            let retry_after = headers
                .as_ref()
                .map(ClaudeOAuthResponseMetadata::from_headers)
                .and_then(|metadata| metadata.retry_after())
                .map(|parsed| {
                    retry_after.map_or(parsed, |original| {
                        if original.deadline() > parsed.deadline() {
                            original
                        } else {
                            parsed
                        }
                    })
                })
                .or(retry_after);
            let parsed: Option<Value> = body
                .as_deref()
                .and_then(|body| serde_json::from_str(body).ok());
            let error = parsed.as_ref().map(|body| body.get("error").unwrap_or(body));
            let kind = error
                .and_then(|error| error.get("type"))
                .and_then(Value::as_str)
                .unwrap_or_default();
            let message = error
                .and_then(|error| error.get("message"))
                .and_then(Value::as_str)
                .unwrap_or_default();
            match status.as_u16() {
                401 => ApiError::InvalidRequest {
                    message: "Claude OAuth authentication expired or was rejected. Run `codex login --provider claude-oauth` to authenticate again. No other credential or provider will be used.".to_owned(),
                },
                403 => ApiError::InvalidRequest {
                    message: "Anthropic denied Claude OAuth access. Check your account entitlement and Anthropic's third-party authentication policy; this experimental transport may not be permitted. No billing mode or provider will be substituted.".to_owned(),
                },
                402 => billing_error(),
                429 => ApiError::RateLimitExceeded {
                    message: "Anthropic rejected the Claude OAuth request because of a rate, subscription, or extra-usage limit. Honor the reported reset or Retry-After, and check Claude account Usage and extra usage settings. The charged pool is unconfirmed; Codex will not rotate accounts or switch billing modes.".to_owned(),
                    retry_after,
                },
                300..=399 => ApiError::InvalidRequest {
                    message: "Claude OAuth redirect refused to protect credentials. Only the fixed Anthropic origin is permitted.".to_owned(),
                },
                400 if prompt_too_long(message) => ApiError::ContextWindowExceeded,
                400 if thinking_rejected(message) => ApiError::InvalidRequest {
                    // The normal Anthropic recovery recognizes these words and retries
                    // the existing prompt after removing stale thinking signatures.
                    message: "Anthropic rejected a replayed thinking block signature.".to_owned(),
                },
                _ if kind == "billing_error" => billing_error(),
                500..=599 => ApiError::Retryable {
                    message: "Anthropic Claude OAuth service is unavailable or overloaded.".to_owned(),
                    retry_after,
                },
                413 => ApiError::InvalidRequest {
                    message: "Anthropic rejected the Claude OAuth request because it is too large.".to_owned(),
                },
                _ => ApiError::InvalidRequest {
                    message: format!("Anthropic rejected the Claude OAuth request (HTTP {}). Server details are withheld to protect credentials; no provider or billing fallback was attempted.", status.as_u16()),
                },
            }
        }
        ApiError::Transport(TransportError::Policy(policy)) => {
            ApiError::Transport(TransportError::Policy(policy))
        }
        ApiError::Transport(TransportError::Build(_)) => ApiError::InvalidRequest {
            message: "Failed to construct the Claude OAuth request. Request details are withheld to protect credentials.".to_owned(),
        },
        ApiError::Transport(_) => ApiError::Retryable {
            message: "Claude OAuth connection failed or was interrupted. Connection details are withheld to protect credentials.".to_owned(),
            retry_after: None,
        },
        ApiError::Retryable { retry_after, .. } => ApiError::Retryable {
            message: "Claude OAuth service or stream was interrupted.".to_owned(),
            retry_after,
        },
        ApiError::RateLimitExceeded { retry_after, .. } => ApiError::RateLimitExceeded {
            message: "Anthropic reported a Claude OAuth rate or usage limit. Check Claude account Usage; the charged pool is unconfirmed.".to_owned(),
            retry_after,
        },
        ApiError::InvalidRequest { message } if thinking_rejected(&message) => ApiError::InvalidRequest {
            message: "Anthropic rejected a replayed thinking block signature.".to_owned(),
        },
        ApiError::InvalidRequest { .. } | ApiError::InvalidPrompt { .. } => ApiError::InvalidRequest {
            message: "Anthropic rejected the Claude OAuth request. Server details are withheld to protect credentials.".to_owned(),
        },
        ApiError::QuotaExceeded | ApiError::UsageNotIncluded => billing_error(),
        ApiError::Stream(_) | ApiError::Api { .. } | ApiError::RateLimit(_) => ApiError::Stream(
            "Claude OAuth response failed. Server details are withheld to protect credentials.".to_owned(),
        ),
        ApiError::ResponseIncomplete(mut details) => {
            details.reason = "Claude OAuth response incomplete".to_owned();
            details.response_id = None;
            ApiError::ResponseIncomplete(details)
        }
        ApiError::CyberPolicy { .. } => ApiError::CyberPolicy {
            message: POLICY_DETAILS_WITHHELD.to_owned(),
        },
        ApiError::BioPolicy { .. } => ApiError::BioPolicy {
            message: POLICY_DETAILS_WITHHELD.to_owned(),
        },
        ApiError::MisalignmentPolicyViolation { .. } => ApiError::MisalignmentPolicyViolation {
            message: POLICY_DETAILS_WITHHELD.to_owned(),
            misalignment: None,
        },
        // The remaining variants contain structured local or semantic errors.
        other => other,
    }
}

fn prompt_too_long(message: &str) -> bool {
    let lower = message.to_ascii_lowercase();
    lower.contains("prompt is too long")
        || lower.contains("context window")
        || lower.contains("too many total text bytes")
}

fn thinking_rejected(message: &str) -> bool {
    let lower = message.to_ascii_lowercase();
    (lower.contains("thinking") && (lower.contains("signature") || lower.contains("block")))
        || lower.contains("block_binding")
        || lower.contains(super::request::THINKING_BINDING_BETA)
}

fn billing_error() -> ApiError {
    ApiError::InvalidRequest {
        message: "Anthropic reported a Claude OAuth billing or entitlement failure. Check Claude account Usage and extra usage settings. Subscription-quota treatment is unconfirmed; Codex will not switch to API credits or another provider.".to_owned(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use http::HeaderValue;
    use http::StatusCode;
    use pretty_assertions::assert_eq;

    fn metadata(values: &[(&str, &str)]) -> ClaudeOAuthResponseMetadata {
        let mut headers = HeaderMap::new();
        for (name, value) in values {
            headers.insert(
                http::HeaderName::from_bytes(name.as_bytes()).expect("header name"),
                HeaderValue::from_str(value).expect("header value"),
            );
        }
        ClaudeOAuthResponseMetadata::from_headers(&headers)
    }

    fn http_error(status: u16, message: &str, headers: HeaderMap) -> ApiError {
        ApiError::Transport(TransportError::Http {
            status: StatusCode::from_u16(status).expect("status"),
            url: Some("https://api.anthropic.com/?access_token=secret-token".to_owned()),
            headers: Some(headers),
            body: Some(
                serde_json::json!({
                    "error": {"type": "invalid_request_error", "message": message}
                })
                .to_string(),
            ),
            retry_after: None,
        })
    }

    #[test]
    fn quota_utilization_and_resets_are_server_reported_without_billing_claim() {
        let metadata = metadata(&[
            ("Anthropic-Ratelimit-Unified-5h-Status", "allowed_warning"),
            ("anthropic-ratelimit-unified-5h-utilization", "0.8"),
            ("anthropic-ratelimit-unified-5h-reset", "1704067200.25"),
            ("anthropic-ratelimit-unified-7d-utilization", "0.25"),
            (
                "anthropic-ratelimit-unified-7d-reset",
                "2024-01-08T00:00:00Z",
            ),
        ]);
        assert_eq!(metadata.rate_limits.len(), 1);
        let limit = &metadata.rate_limits[0];
        assert_eq!(limit.primary.as_ref().expect("5h").used_percent, 80.0);
        assert_eq!(
            limit.primary.as_ref().expect("5h").window_minutes,
            Some(300)
        );
        assert_eq!(
            limit.primary.as_ref().expect("5h").resets_at,
            Some(1704067200)
        );
        assert_eq!(limit.secondary.as_ref().expect("7d").used_percent, 25.0);
        assert_eq!(
            limit.secondary.as_ref().expect("7d").window_minutes,
            Some(10080)
        );
        assert_eq!(
            limit.secondary.as_ref().expect("7d").resets_at,
            Some(1704672000)
        );
        assert!(
            limit
                .limit_name
                .as_ref()
                .expect("name")
                .contains("billing unconfirmed")
        );
        assert!(limit.credits.is_none());
        assert!(limit.plan_type.is_none());
        assert!(
            metadata
                .warning_summary()
                .expect("warning")
                .contains("not confirmed")
        );
    }

    #[test]
    fn does_not_invent_usage_when_only_status_is_available() {
        let metadata = metadata(&[("anthropic-ratelimit-unified-5h-status", "rejected")]);
        assert!(metadata.rate_limits.is_empty());
        assert_eq!(metadata.get("5h-status"), Some("rejected"));
        assert!(metadata.warning_summary().is_some());
    }

    #[test]
    fn empty_headers_still_warn_billing_unconfirmed() {
        let metadata = metadata(&[]);
        assert!(metadata.rate_limits.is_empty());
        assert!(
            metadata
                .warning_summary()
                .expect("warning")
                .contains("subscription")
        );
    }

    #[test]
    fn preserves_only_bounded_allowlisted_unambiguous_values() {
        let mut headers = HeaderMap::new();
        headers.insert(
            "authorization",
            HeaderValue::from_static("Bearer secret-token"),
        );
        headers.insert("set-cookie", HeaderValue::from_static("secret-token"));
        headers.insert(
            "anthropic-ratelimit-unified-status",
            HeaderValue::from_static("secret-token"),
        );
        headers.insert(
            "anthropic-ratelimit-unified-5h-utilization",
            HeaderValue::from_static("NaN"),
        );
        headers.insert(
            "anthropic-ratelimit-unified-7d-utilization",
            HeaderValue::from_static("-1"),
        );
        headers.insert(
            "anthropic-ratelimit-unified-7d_oi-utilization",
            HeaderValue::from_static("inf"),
        );
        headers.insert(
            "anthropic-ratelimit-unified-overage-utilization",
            HeaderValue::from_static("1e308"),
        );
        headers.insert(
            "anthropic-ratelimit-unified-representative-claim",
            HeaderValue::from_str(&"x".repeat(129)).expect("value"),
        );
        headers.append("retry-after", HeaderValue::from_static("60"));
        headers.append("retry-after", HeaderValue::from_static("3600"));
        let metadata = ClaudeOAuthResponseMetadata::from_headers(&headers);
        assert!(metadata.headers.is_empty());
        assert!(!format!("{metadata:?}").contains("secret-token"));
    }

    #[test]
    fn overage_and_model_specific_usage_remain_distinct() {
        let metadata = metadata(&[
            ("anthropic-ratelimit-unified-7d_oi-utilization", "1.02"),
            ("anthropic-ratelimit-unified-overage-utilization", "0.4"),
            ("anthropic-ratelimit-unified-overage-status", "allowed"),
        ]);
        assert_eq!(metadata.rate_limits.len(), 2);
        assert_eq!(
            metadata.rate_limits[0].limit_id.as_deref(),
            Some("claude_oauth_7d_oi")
        );
        assert_eq!(
            metadata.rate_limits[1].limit_id.as_deref(),
            Some("claude_oauth_overage")
        );
        assert_eq!(
            metadata.rate_limits[1]
                .primary
                .as_ref()
                .expect("overage")
                .window_minutes,
            None
        );
        assert!(
            metadata
                .warning_summary()
                .expect("warning")
                .contains("separately billed")
        );
    }

    #[test]
    fn reset_parser_accepts_reference_formats() {
        for raw in [
            "1704067200",
            "1704067200.5",
            "2024-01-01T00:00:00Z",
            "Mon, 01 Jan 2024 00:00:00 GMT",
        ] {
            assert_eq!(
                parse_timestamp(raw).expect("timestamp").floor() as i64,
                1704067200
            );
        }
        for raw in ["NaN", "inf", "-1", "0", "1e100", "tomorrow"] {
            assert_eq!(parse_timestamp(raw), None);
        }
    }

    #[test]
    fn latest_rejected_reset_wins_and_allowed_windows_do_not_extend_retry() {
        let metadata = metadata(&[
            ("retry-after", "30"),
            ("anthropic-ratelimit-unified-5h-status", "rejected"),
            ("anthropic-ratelimit-unified-5h-reset", "1704070800"),
            ("anthropic-ratelimit-unified-7d-status", "allowed"),
            ("anthropic-ratelimit-unified-7d-reset", "1704153600"),
        ]);
        assert_eq!(
            metadata.retry_delay_at(1704067200.0),
            Some(Duration::from_secs(3600))
        );
    }

    #[test]
    fn overage_only_billing_reset_is_not_retry_advice() {
        let metadata = metadata(&[
            ("retry-after", "3600"),
            ("anthropic-ratelimit-unified-status", "rejected"),
            (
                "anthropic-ratelimit-unified-representative-claim",
                "overage",
            ),
            ("anthropic-ratelimit-unified-reset", "1704153600"),
            ("anthropic-ratelimit-unified-overage-reset", "1704153600"),
        ]);
        assert_eq!(metadata.retry_delay_at(1704067200.0), None);
    }

    #[test]
    fn billing_boundary_does_not_override_real_retry_after() {
        let metadata = metadata(&[
            ("retry-after", "60"),
            ("anthropic-ratelimit-unified-status", "rejected"),
            ("anthropic-ratelimit-unified-5h-status", "rejected"),
            (
                "anthropic-ratelimit-unified-representative-claim",
                "overage",
            ),
            ("anthropic-ratelimit-unified-reset", "1704153600"),
            ("anthropic-ratelimit-unified-overage-reset", "1704153600"),
        ]);
        assert_eq!(
            metadata.retry_delay_at(1704067200.0),
            Some(Duration::from_secs(60))
        );
    }

    #[test]
    fn reset_deadlines_are_bounded_and_fractional_retry_after_is_supported() {
        let metadata = metadata(&[
            ("retry-after", "0.5"),
            ("anthropic-ratelimit-unified-status", "rejected"),
            ("anthropic-ratelimit-unified-reset", "1706745600"),
        ]);
        assert_eq!(
            metadata.retry_delay_at(1704067200.0),
            Some(Duration::from_millis(500))
        );
        assert_eq!(
            parse_retry_after("Mon, 01 Jan 2024 00:01:00 GMT", 1704067200.0),
            Some(60.0)
        );
    }

    #[test]
    fn oauth_http_failures_never_echo_server_or_request_secrets() {
        for status in [
            301, 302, 307, 308, 400, 401, 402, 403, 404, 413, 422, 429, 500, 503, 529,
        ] {
            let error = map_claude_oauth_transport_error(http_error(
                status,
                "secret-token",
                HeaderMap::new(),
            ));
            assert!(!format!("{error:?}").contains("secret-token"));
            assert!(!error.to_string().contains("secret-token"));
            if status == 401 {
                assert!(error.to_string().contains("login --provider claude-oauth"));
                assert!(!error.to_string().contains("--with-api-key"));
            }
            if status == 403 {
                assert!(error.to_string().contains("entitlement"));
                assert!(
                    error
                        .to_string()
                        .contains("third-party authentication policy")
                );
            }
        }
    }

    #[test]
    fn transport_failures_redact_secret_bearing_urls() {
        for error in [
            TransportError::Network("https://bad/?token=secret-token".to_owned()),
            TransportError::Build("Authorization: Bearer secret-token".to_owned()),
        ] {
            let error = map_claude_oauth_transport_error(ApiError::Transport(error));
            assert!(!format!("{error:?}").contains("secret-token"));
        }
    }

    #[test]
    fn rate_limit_preserves_retry_after() {
        let mut headers = HeaderMap::new();
        headers.insert("retry-after", HeaderValue::from_static("60"));
        let error = map_claude_oauth_transport_error(http_error(429, "secret-token", headers));
        let ApiError::RateLimitExceeded {
            retry_after: Some(retry_after),
            ..
        } = error
        else {
            panic!("expected rate limit with retry advice");
        };
        assert!(retry_after.remaining_delay() > Duration::from_secs(59));
        assert!(retry_after.remaining_delay() <= Duration::from_secs(60));
    }

    #[test]
    fn preserves_thinking_recovery_without_echoing_signature() {
        let error = map_claude_oauth_transport_error(http_error(
            400,
            "Invalid thinking block signature: secret-token",
            HeaderMap::new(),
        ));
        assert!(super::super::error::is_thinking_replay_rejection(&error));
        assert!(!error.to_string().contains("secret-token"));
        let error = map_claude_oauth_transport_error(http_error(
            400,
            "prompt is too long: secret-token",
            HeaderMap::new(),
        ));
        assert!(matches!(error, ApiError::ContextWindowExceeded));
    }

    #[test]
    fn billing_error_is_clear_without_claiming_subscription_quota() {
        let error = ApiError::Transport(TransportError::Http {
            status: StatusCode::BAD_REQUEST,
            url: None,
            headers: None,
            body: Some(r#"{"error":{"type":"billing_error","message":"secret-token"}}"#.to_owned()),
            retry_after: None,
        });
        let error = map_claude_oauth_transport_error(error);
        assert!(error.to_string().contains("billing or entitlement"));
        assert!(error.to_string().contains("unconfirmed"));
        assert!(!error.to_string().contains("secret-token"));
    }

    #[test]
    fn policy_redaction_preserves_structured_categories_across_repeated_mapping() {
        for raw in [
            ApiError::CyberPolicy {
                message: "secret-policy-diagnostic".to_owned(),
            },
            ApiError::BioPolicy {
                message: "secret-policy-diagnostic".to_owned(),
            },
            ApiError::MisalignmentPolicyViolation {
                message: "secret-policy-diagnostic".to_owned(),
                misalignment: None,
            },
        ] {
            let category = std::mem::discriminant(&raw);
            let once = map_claude_oauth_transport_error(raw);
            assert_eq!(std::mem::discriminant(&once), category);
            assert!(!format!("{once:?}").contains("secret-policy-diagnostic"));
            let twice = map_claude_oauth_transport_error(once);
            assert_eq!(std::mem::discriminant(&twice), category);
            assert!(!format!("{twice:?}").contains("secret-policy-diagnostic"));
        }
    }
}
