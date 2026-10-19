//! Optional live probe. Authenticate explicitly first, then run:
//! CODEX_CLAUDE_OAUTH_LIVE=1 cargo test -p codex-model-provider --test claude_oauth_live -- --ignored
//! This sends one small request to Anthropic and may consume quota or incur charges according to
//! Anthropic account settings. A successful reply does not prove subscription-quota treatment.

use codex_api::ResponseEvent;
use codex_api::anthropic::AnthropicMessagesClient;
use codex_api::anthropic::AnthropicRequestOptions;
use codex_api::anthropic::build_anthropic_request;
use codex_api::anthropic::oauth::OAuthRequestContext;
use codex_api::anthropic::resolve_claude_reasoning;
use codex_http_client::ClientRouteClass;
use codex_http_client::HttpClientBuilder;
use codex_http_client::HttpClientFactory;
use codex_http_client::OutboundProxyPolicy;
use codex_http_client::ReqwestTransport;
use codex_login::AuthCredentialsStoreMode;
use codex_login::AuthKeyringBackendKind;
use codex_login::AuthManager;
use codex_login::AuthRouteConfig;
use codex_model_provider::create_model_provider;
use codex_model_provider_info::ModelProviderInfo;
use codex_protocol::models::ContentItem;
use codex_protocol::models::ResponseItem;
use codex_protocol::openai_models::ReasoningEffort;
use futures::StreamExt;
use http::HeaderMap;

#[tokio::test]
#[ignore = "requires explicit live opt-in and separately stored Claude OAuth credentials; may consume quota or incur charges"]
async fn explicitly_enabled_claude_oauth_direct_http() {
    assert_eq!(
        std::env::var("CODEX_CLAUDE_OAUTH_LIVE").as_deref(),
        Ok("1"),
        "set CODEX_CLAUDE_OAUTH_LIVE=1 only after reviewing Anthropic account Usage and extra usage settings"
    );
    let home = std::env::var_os("CODEX_HOME")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| {
            std::path::PathBuf::from(std::env::var_os("HOME").expect("HOME or CODEX_HOME"))
                .join(".codex")
        });
    // File mode is age-encrypted with a keyring-held key. A directly stored OS-keyring login
    // can opt into its matching backend; this test never reads Claude Code credential stores.
    let store = match std::env::var("CODEX_CLAUDE_OAUTH_LIVE_STORE").as_deref() {
        Ok("keyring") => AuthCredentialsStoreMode::Keyring,
        Ok("file") | Err(_) => AuthCredentialsStoreMode::File,
        _ => panic!("CODEX_CLAUDE_OAUTH_LIVE_STORE must be file or keyring"),
    };
    let factory = HttpClientFactory::new(OutboundProxyPolicy::ReqwestDefault);
    let manager = AuthManager::shared(
        home,
        false,
        store,
        None,
        None,
        AuthKeyringBackendKind::Direct,
        AuthRouteConfig::from_http_client_factory(factory.clone()),
    )
    .await;
    let provider = create_model_provider(
        ModelProviderInfo::create_claude_oauth_provider(),
        Some(manager),
    );
    let auth = provider
        .api_auth()
        .await
        .expect("explicitly saved Claude OAuth login required");
    let identity = auth.claude_oauth_identity().expect("OAuth identity");
    let api_provider = provider
        .api_provider()
        .await
        .expect("fixed Anthropic provider");
    let http = HttpClientBuilder::new()
        .without_redirects()
        .build_respecting_outbound_proxy_policy(
            &factory,
            "https://api.anthropic.com/v1/messages",
            ClientRouteClass::Api,
        )
        .expect("HTTP client");
    let client =
        AnthropicMessagesClient::new(ReqwestTransport::from_http_client(http), api_provider, auth);
    let model = std::env::var("CODEX_CLAUDE_OAUTH_LIVE_MODEL")
        .unwrap_or_else(|_| "claude-haiku-5-5".to_string());
    // Share the production model rules: Haiku permits thinking off; an explicitly chosen
    // always-thinking model is adjusted to adaptive/low rather than sending invalid fields.
    let reasoning = resolve_claude_reasoning(&model, Some(&ReasoningEffort::None), Some(2048));
    let input = [ResponseItem::Message {
        encrypted_content: None,
        status: None,
        id: None,
        role: "user".to_string(),
        content: vec![ContentItem::InputText {
            text: "Reply with the word connected.".to_string(),
        }],
        phase: None,
        internal_chat_message_metadata_passthrough: None,
    }];
    let prepared = build_anthropic_request(
        "Reply briefly.",
        &input,
        &[],
        &AnthropicRequestOptions {
            model,
            max_tokens: reasoning.max_tokens,
            thinking: reasoning.thinking,
            summarize_thinking: false,
            effort: reasoning.effort,
            parallel_tool_calls: false,
            prompt_caching: false,
            output_schema: None,
            drop_mismatched_thinking: false,
            strip_thinking: false,
        },
    );
    let context = OAuthRequestContext {
        account_uuid: identity.account_uuid,
        device_id: identity.device_id,
        session_id: "00000000-0000-4000-8000-000000000001".to_string(),
        ..Default::default()
    };
    let (mut stream, _metadata) = client
        .stream_oauth(&prepared, HeaderMap::new(), &context)
        .await
        .expect("direct OAuth stream; quota treatment remains unverified");
    let mut completed = false;
    let mut received_text = false;
    while let Some(event) = stream.next().await {
        match event.expect("OAuth response event") {
            ResponseEvent::Completed { .. } => completed = true,
            ResponseEvent::OutputTextDelta(delta) => received_text |= !delta.is_empty(),
            _ => {}
        }
    }
    assert!(
        completed && received_text,
        "a live response is required; billing classification still requires account-side verification"
    );
}
