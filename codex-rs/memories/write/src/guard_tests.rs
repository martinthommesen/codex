use super::*;
use codex_login::CodexAuth;
use codex_model_provider_info::ModelProviderInfo;
use codex_protocol::protocol::RateLimitReachedType;
use core_test_support::load_default_config_for_test;
use serde_json::json;
use tempfile::TempDir;
use wiremock::Mock;
use wiremock::MockServer;
use wiremock::ResponseTemplate;
use wiremock::matchers::method;
use wiremock::matchers::path_regex;

fn snapshot(
    primary_used_percent: Option<f64>,
    secondary_used_percent: Option<f64>,
) -> RateLimitSnapshot {
    RateLimitSnapshot {
        limit_id: Some(crate::guard_limits::CODEX_LIMIT_ID.to_string()),
        limit_name: None,
        normal_model_slug: None,
        primary: primary_used_percent.map(window),
        secondary: secondary_used_percent.map(window),
        credits: None,
        individual_limit: None,
        spend_control_reached: None,
        plan_type: None,
        rate_limit_reached_type: None,
    }
}

fn window(used_percent: f64) -> RateLimitWindow {
    RateLimitWindow {
        used_percent,
        window_minutes: None,
        resets_at: None,
    }
}

#[test]
fn startup_check_uses_configured_remaining_threshold() {
    let snapshot = snapshot(
        /*primary_used_percent*/ Some(89.9),
        /*secondary_used_percent*/ Some(50.0),
    );

    assert!(snapshot_allows_startup(
        &snapshot, /*min_remaining_percent*/ 10
    ));
    assert!(!snapshot_allows_startup(
        &snapshot, /*min_remaining_percent*/ 11
    ));
}

#[test]
fn startup_check_skips_when_primary_or_secondary_is_too_low() {
    assert!(!snapshot_allows_startup(
        &snapshot(
            /*primary_used_percent*/ Some(75.1),
            /*secondary_used_percent*/ Some(10.0),
        ),
        /*min_remaining_percent*/ 25,
    ));
    assert!(!snapshot_allows_startup(
        &snapshot(
            /*primary_used_percent*/ Some(10.0),
            /*secondary_used_percent*/ Some(75.1),
        ),
        /*min_remaining_percent*/ 25,
    ));
    assert!(snapshot_allows_startup(
        &snapshot(
            /*primary_used_percent*/ Some(74.9),
            /*secondary_used_percent*/ Some(74.9),
        ),
        /*min_remaining_percent*/ 25,
    ));
}

#[test]
fn startup_check_skips_when_limit_is_reached() {
    let mut snapshot = snapshot(
        /*primary_used_percent*/ Some(10.0),
        /*secondary_used_percent*/ Some(10.0),
    );
    snapshot.rate_limit_reached_type = Some(RateLimitReachedType::RateLimitReached);

    assert!(!snapshot_allows_startup(
        &snapshot, /*min_remaining_percent*/ 25,
    ));
}

#[tokio::test]
async fn claude_providers_do_not_consult_chatgpt_rate_limits() {
    let usage = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path_regex(".*/usage$"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "plan_type": "plus",
            "rate_limit": {
                "allowed": false,
                "limit_reached": true,
                "primary_window": {"used_percent": 100, "limit_window_seconds": 300,
                    "reset_after_seconds": 60, "reset_at": 2000000000}
            }
        })))
        .mount(&usage)
        .await;
    let home = TempDir::new().expect("temp dir");
    let mut config = load_default_config_for_test(&home).await;
    config.chatgpt_base_url = usage.uri();
    let auth_manager =
        AuthManager::from_auth_for_testing(CodexAuth::create_dummy_chatgpt_auth_for_testing());

    // An OpenAI session is gated on the exhausted ChatGPT quota.
    assert!(!rate_limits_ok(&auth_manager, &config).await);
    let openai_requests = usage.received_requests().await.unwrap_or_default().len();
    assert!(openai_requests > 0);

    for provider in [
        ModelProviderInfo::create_anthropic_provider(),
        ModelProviderInfo::create_claude_oauth_provider(),
    ] {
        config.model_provider = provider;
        assert!(rate_limits_ok(&auth_manager, &config).await);
    }
    assert_eq!(
        usage.received_requests().await.unwrap_or_default().len(),
        openai_requests,
        "Claude memory jobs must not contact the ChatGPT backend"
    );
}
