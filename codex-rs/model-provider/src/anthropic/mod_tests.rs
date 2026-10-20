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

#[tokio::test]
async fn custom_anthropic_messages_provider_without_env_key_sends_the_stored_key() {
    // Documented in docs/claude.md: every `anthropic_messages` provider shares the stored key,
    // whatever its id and base URL, unless it sets its own credential source.
    let codex_home = tempfile::tempdir().expect("tempdir");
    let auth_manager = AuthManager::from_auth_for_testing_with_home(
        CodexAuth::from_api_key("sk-openai-test"),
        codex_home.path().to_path_buf(),
    );
    auth_manager
        .anthropic_auth_config()
        .save_api_key("sk-ant-stored-shared-key")
        .expect("store key");
    let info = ModelProviderInfo {
        name: "Gateway".to_string(),
        base_url: Some("https://gateway.example/v1".to_string()),
        wire_api: codex_model_provider_info::WireApi::AnthropicMessages,
        ..Default::default()
    };
    let provider = create_model_provider(info, Some(auth_manager));
    let auth = provider.api_auth().await.expect("auth resolves");
    let mut headers = HeaderMap::new();
    auth.add_auth_headers(&mut headers);
    assert_eq!(
        headers.get("x-api-key").map(|value| value.to_str().ok()),
        Some(Some("sk-ant-stored-shared-key"))
    );
}

#[tokio::test]
async fn env_key_that_cannot_be_sent_fails_without_echoing_it() {
    const VAR: &str = "CODEX_TEST_ANTHROPIC_INVALID_KEY_5c1e";
    // SAFETY: the variable name is unique to this test.
    unsafe {
        std::env::set_var(VAR, "sk-ant-bad\u{1}key");
    }
    let mut info = ModelProviderInfo::create_anthropic_provider();
    info.env_key = Some(VAR.to_string());
    let provider = AnthropicModelProvider::new(info, /*auth_manager*/ None);
    let error = match provider.api_auth().await {
        Ok(_) => panic!("an unsendable key must not resolve to auth"),
        Err(error) => error.to_string(),
    };
    assert!(
        error.contains("cannot be sent in an HTTP header"),
        "{error}"
    );
    assert!(!error.contains("sk-ant-bad"), "{error}");
    // SAFETY: the variable name is unique to this test.
    unsafe {
        std::env::remove_var(VAR);
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
    use codex_models_manager::manager::RefreshStrategy;

    let http = || {
        codex_http_client::HttpClientFactory::new(
            codex_http_client::OutboundProxyPolicy::ReqwestDefault,
        )
    };
    // A configured `model_catalog_json` with OpenAI entries must not replace the Claude catalog.
    let openai_catalog = ModelsResponse {
        models: vec![codex_models_manager::model_info::model_info_from_slug(
            "gpt-5.5",
        )],
    };
    let codex_home = tempfile::tempdir().expect("tempdir");
    let providers: Vec<Box<dyn ModelProvider>> = vec![
        Box::new(AnthropicModelProvider::new(
            ModelProviderInfo::create_anthropic_provider(),
            /*auth_manager*/ None,
        )),
        Box::new(ClaudeCodeModelProvider::new(
            ModelProviderInfo::create_claude_code_provider(),
        )),
        Box::new(ClaudeOAuthModelProvider::new(
            ModelProviderInfo::create_claude_oauth_provider(),
            None,
        )),
    ];
    for provider in &providers {
        for config_model_catalog in [None, Some(openai_catalog.clone())] {
            for manager in [
                provider.models_manager(
                    codex_home.path().to_path_buf(),
                    config_model_catalog.clone(),
                ),
                provider.models_manager_without_cache(config_model_catalog.clone()),
            ] {
                let catalog = manager
                    .raw_model_catalog(RefreshStrategy::Offline, http())
                    .await;
                assert_eq!(catalog, claude_model_catalog());
                let default_model = manager
                    .get_default_model(
                        &None,
                        /*allow_provider_model_fallback*/ true,
                        RefreshStrategy::Offline,
                        http(),
                    )
                    .await;
                assert_eq!(default_model, DEFAULT_CLAUDE_MODEL);
            }
        }
    }
}
