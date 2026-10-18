use super::*;
use crate::create_model_provider;
use http::HeaderMap;
use pretty_assertions::assert_eq;

const TEST_KEY_VAR: &str = "CODEX_TEST_ANTHROPIC_GATEWAY_KEY_7f3a";

#[test]
fn factory_routes_claude_wire_apis_to_claude_providers() {
    let anthropic = create_model_provider(
        ModelProviderInfo::create_anthropic_provider(),
        /*auth_manager*/ None,
    );
    assert!(anthropic.info().is_anthropic());
    assert!(anthropic.auth_manager().is_none());
    assert_eq!(anthropic.capabilities(), claude_capabilities());

    let claude_code = create_model_provider(
        ModelProviderInfo::create_claude_code_provider(),
        /*auth_manager*/ None,
    );
    assert!(claude_code.info().is_claude_code());
    assert!(claude_code.auth_manager().is_none());
    assert_eq!(
        claude_code.account_state().expect("account state"),
        ProviderAccountState {
            account: None,
            requires_openai_auth: false,
        }
    );
}

#[test]
fn claude_capabilities_disable_openai_hosted_features() {
    assert_eq!(
        claude_capabilities(),
        ProviderCapabilities {
            image_generation: false,
            web_search: false,
            external_web_access: false,
            remote_compaction: RemoteCompactionSupport::Unsupported,
        }
    );
}

#[tokio::test]
async fn explicit_env_key_is_sent_as_x_api_key() {
    // SAFETY: the variable name is unique to this test.
    unsafe {
        std::env::set_var(TEST_KEY_VAR, "sk-ant-test-gateway-key");
    }
    let mut info = ModelProviderInfo::create_anthropic_provider();
    info.env_key = Some(TEST_KEY_VAR.to_string());
    let provider = AnthropicModelProvider::new(info, /*auth_manager*/ None);
    let auth = provider.api_auth().await.expect("auth resolves");
    let mut headers = HeaderMap::new();
    auth.add_auth_headers(&mut headers);
    assert_eq!(
        headers.get("x-api-key").map(|value| value.to_str().ok()),
        Some(Some("sk-ant-test-gateway-key"))
    );
    assert!(headers.get(http::header::AUTHORIZATION).is_none());
    assert!(
        headers
            .get("x-api-key")
            .is_some_and(http::HeaderValue::is_sensitive)
    );
    assert_eq!(
        provider.account_state().expect("account state"),
        ProviderAccountState {
            account: None,
            requires_openai_auth: false,
        }
    );
    // SAFETY: the variable name is unique to this test.
    unsafe {
        std::env::remove_var(TEST_KEY_VAR);
    }
}

#[test]
fn anthropic_errors_use_anthropic_mapping() {
    let provider = AnthropicModelProvider::new(
        ModelProviderInfo::create_anthropic_provider(),
        /*auth_manager*/ None,
    );
    let error = provider.map_api_error(ApiError::Transport(TransportError::Http {
        status: http::StatusCode::UNAUTHORIZED,
        url: None,
        headers: None,
        body: Some(
            r#"{"type":"error","error":{"type":"authentication_error","message":"invalid x-api-key"}}"#
                .to_string(),
        ),
        retry_after: None,
    }));
    let message = error.to_string();
    assert!(
        message.contains("codex login --provider anthropic"),
        "{message}"
    );
    assert!(error.retry_delay(/*retry_count*/ 1).is_none());
}

#[tokio::test]
async fn claude_code_provider_has_no_http_endpoint_or_credentials() {
    let provider = ClaudeCodeModelProvider::new(ModelProviderInfo::create_claude_code_provider());
    assert!(provider.api_provider().await.is_err());
    let auth = provider.api_auth().await.expect("unauthenticated provider");
    let mut headers = HeaderMap::new();
    auth.add_auth_headers(&mut headers);
    assert!(headers.is_empty());
}

#[tokio::test]
async fn claude_providers_serve_the_claude_catalog() {
    let provider = ClaudeCodeModelProvider::new(ModelProviderInfo::create_claude_code_provider());
    let manager = provider.models_manager_without_cache(/*config_model_catalog*/ None);
    let catalog = manager
        .raw_model_catalog(
            codex_models_manager::manager::RefreshStrategy::Offline,
            codex_http_client::HttpClientFactory::new(
                codex_http_client::OutboundProxyPolicy::ReqwestDefault,
            ),
        )
        .await;
    assert_eq!(catalog, claude_model_catalog());
}
