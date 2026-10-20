use super::*;
use crate::create_model_provider;
use pretty_assertions::assert_eq;

#[test]
fn factory_routes_oauth_to_isolated_claude_provider() {
    let provider = create_model_provider(ModelProviderInfo::create_claude_oauth_provider(), None);
    assert!(provider.info().is_claude_oauth());
    assert!(provider.auth_manager().is_none());
    assert_eq!(provider.capabilities(), claude_capabilities());
    assert_eq!(
        provider.approval_review_preferred_model(),
        CLAUDE_BACKGROUND_MODEL
    );
    assert_eq!(
        provider.account_state().expect("account state"),
        ProviderAccountState {
            account: None,
            requires_openai_auth: false,
        }
    );
    assert!(!provider.is_recoverable_auth_error(&TransportError::Http {
        status: http::StatusCode::UNAUTHORIZED,
        url: None,
        headers: None,
        body: None,
        retry_after: None,
    }));
}

#[tokio::test]
async fn openai_and_anthropic_credentials_never_authenticate_oauth() {
    let home = tempfile::tempdir().expect("home");
    let manager = AuthManager::from_auth_for_testing_with_home(
        CodexAuth::from_api_key("openai-secret"),
        home.path().to_path_buf(),
    );
    manager
        .anthropic_auth_config()
        .save_api_key("anthropic-secret")
        .expect("save key");
    let provider = ClaudeOAuthModelProvider::new(
        ModelProviderInfo::create_claude_oauth_provider(),
        Some(manager),
    );
    assert!(provider.auth().await.is_none());
    let error = provider
        .api_auth()
        .await
        .err()
        .expect("isolated credential missing")
        .to_string();
    assert!(error.contains("claude-oauth"));
    assert!(!error.contains("openai-secret"));
    assert!(!error.contains("anthropic-secret"));
    let api = provider.api_provider().await.expect("direct provider");
    assert_eq!(api.base_url, "https://api.anthropic.com/v1");
    assert!(!api.headers.contains_key("x-api-key"));
    assert!(!api.headers.contains_key(http::header::AUTHORIZATION));
}

#[tokio::test]
async fn invalid_programmatic_config_cannot_send_keys_or_select_another_origin() {
    let info = ModelProviderInfo {
        base_url: Some("https://example.invalid/v1".to_string()),
        env_key: Some("ANTHROPIC_API_KEY".to_string()),
        ..ModelProviderInfo::create_claude_oauth_provider()
    };
    let provider = ClaudeOAuthModelProvider::new(info, None);
    assert!(provider.api_provider().await.is_err());
    assert!(provider.api_auth().await.is_err());
}

#[test]
fn oauth_bearer_is_sensitive_and_has_no_api_key_or_openai_identity() {
    let headers = bearer_headers("subscription-secret").expect("bearer");
    let value = headers
        .get(http::header::AUTHORIZATION)
        .expect("authorization");
    assert_eq!(
        value.to_str().expect("header"),
        "Bearer subscription-secret"
    );
    assert!(value.is_sensitive());
    assert!(!format!("{headers:?}").contains("subscription-secret"));
    assert_eq!(headers.len(), 1);
    assert!(bearer_headers("bad\r\nInjected: secret").is_err());
}

#[test]
fn outbound_auth_destination_is_pinned_before_credentials_are_resolved() {
    for url in [
        "https://api.anthropic.com/v1/messages",
        "https://api.anthropic.com/v1/messages?beta=true",
        "https://api.anthropic.com:443/v1/messages?beta=true",
    ] {
        assert!(
            validate_request_destination(&Request::new(http::Method::POST, url.to_string()))
                .is_ok(),
            "{url}"
        );
    }
    for url in [
        "http://api.anthropic.com/v1/messages",
        "https://api.anthropic.com:444/v1/messages",
        "https://api.anthropic.com.evil/v1/messages",
        "https://evil.invalid/v1/messages",
        "https://api.anthropic.com/v1/messages?redirect=https://evil.invalid",
        "https://api.anthropic.com/v1/messages#fragment",
        "https://user:pass@api.anthropic.com/v1/messages",
        "https://api.anthropic.com/api/oauth/profile",
    ] {
        let error =
            validate_request_destination(&Request::new(http::Method::POST, url.to_string()))
                .expect_err("unsafe URL")
                .to_string();
        assert!(
            !error.contains(url),
            "untrusted destination must not be reflected"
        );
    }
    assert!(
        validate_request_destination(&Request::new(
            http::Method::GET,
            "https://api.anthropic.com/v1/messages".to_string()
        ))
        .is_err()
    );
}

#[test]
fn oauth_authentication_and_entitlement_errors_are_terminal_and_redacted() {
    let provider =
        ClaudeOAuthModelProvider::new(ModelProviderInfo::create_claude_oauth_provider(), None);
    for status in [http::StatusCode::UNAUTHORIZED, http::StatusCode::FORBIDDEN] {
        let error = provider.map_api_error(ApiError::Transport(TransportError::Http {
            status,
            url: None,
            headers: None,
            body: Some("reflected-subscription-secret".to_string()),
            retry_after: None,
        }));
        let message = error.to_string();
        assert!(message.contains("Claude OAuth"));
        assert!(!message.contains("reflected-subscription-secret"));
        assert!(!message.contains("login --provider anthropic"));
        assert!(error.retry_delay(1).is_none());
    }
}

#[test]
fn already_sanitized_oauth_errors_keep_actionable_login_and_entitlement_hints() {
    let provider =
        ClaudeOAuthModelProvider::new(ModelProviderInfo::create_claude_oauth_provider(), None);
    for status in [http::StatusCode::UNAUTHORIZED, http::StatusCode::FORBIDDEN] {
        let sanitized = codex_api::anthropic::map_claude_oauth_transport_error(
            ApiError::Transport(TransportError::Http {
                status,
                url: None,
                headers: None,
                body: Some("server-secret".to_string()),
                retry_after: None,
            }),
        );
        let error = provider.map_api_error(sanitized);
        let message = error.to_string();
        assert!(!message.contains("server-secret"));
        if status == http::StatusCode::UNAUTHORIZED {
            assert!(message.contains("codex login --provider claude-oauth"));
        } else {
            assert!(message.contains("entitlement"));
        }
    }
}
