use super::*;
use crate::config::ConfigBuilder;
use pretty_assertions::assert_eq;
use tempfile::TempDir;
use toml::Value as TomlValue;

async fn config_in(codex_home: &TempDir, model_provider: &str) -> Config {
    ConfigBuilder::default()
        .codex_home(codex_home.path().to_path_buf())
        .cli_overrides(vec![(
            "model_provider".to_string(),
            TomlValue::String(model_provider.to_string()),
        )])
        .fallback_cwd(Some(codex_home.path().to_path_buf()))
        .build()
        .await
        .expect("load config")
}

fn mark_claude_code_connected(codex_home: &TempDir) {
    let marker = codex_home.path().join(CLAUDE_CODE_CONNECTED_MARKER);
    std::fs::create_dir_all(marker.parent().expect("marker parent")).expect("create dir");
    std::fs::write(marker, "{}").expect("write marker");
}

#[tokio::test]
async fn claude_code_requires_explicit_connection() {
    let codex_home = TempDir::new().expect("tempdir");
    let config = config_in(&codex_home, OPENAI_PROVIDER_ID).await;
    let provider = config
        .model_providers
        .get(CLAUDE_CODE_PROVIDER_ID)
        .expect("built-in claude-code provider")
        .clone();
    assert!(matches!(
        provider_connection(&provider, /*auth_manager*/ None, codex_home.path()),
        ProviderConnection::NotConnected { .. }
    ));

    mark_claude_code_connected(&codex_home);
    assert_eq!(
        provider_connection(&provider, /*auth_manager*/ None, codex_home.path()),
        ProviderConnection::Connected
    );
}

#[tokio::test]
async fn anthropic_connection_follows_configured_key_variable() {
    let codex_home = TempDir::new().expect("tempdir");
    let config = config_in(&codex_home, OPENAI_PROVIDER_ID).await;
    let mut provider = config
        .model_providers
        .get(ANTHROPIC_PROVIDER_ID)
        .expect("built-in anthropic provider")
        .clone();
    let auth_manager = AuthManager::from_auth_for_testing_with_home(
        codex_login::CodexAuth::from_api_key("sk-openai-test"),
        codex_home.path().to_path_buf(),
    );
    let check = |provider: &ModelProviderInfo| {
        provider_connection(provider, Some(auth_manager.as_ref()), codex_home.path())
    };
    assert!(
        matches!(check(&provider), ProviderConnection::NotConnected { ref remedy } if remedy.contains("codex login --provider anthropic --with-api-key")),
        "an empty store is not connected"
    );

    auth_manager
        .anthropic_auth_config()
        .save_api_key("sk-ant-api03-provider-models-test-key")
        .expect("store key");
    assert_eq!(check(&provider), ProviderConnection::Connected);

    // A configured key variable is the only key requests would use, so the stored key does not
    // make the provider connected while that variable is unset.
    provider.env_key = Some("CODEX_TEST_UNSET_ANTHROPIC_VAR_91c2".to_string());
    assert_eq!(
        check(&provider),
        ProviderConnection::NotConnected {
            remedy: "set the CODEX_TEST_UNSET_ANTHROPIC_VAR_91c2 environment variable".to_string(),
        }
    );
}

#[tokio::test]
async fn spawn_options_list_only_connected_providers() {
    let codex_home = TempDir::new().expect("tempdir");
    let config = config_in(&codex_home, OPENAI_PROVIDER_ID).await;

    let alone = spawn_provider_options(&config, /*auth_manager*/ None, &[]);
    assert_eq!(
        alone
            .iter()
            .map(|option| option.id.as_str())
            .collect::<Vec<_>>(),
        vec![OPENAI_PROVIDER_ID]
    );
    assert!(!collaboration_messages_require_plaintext(&alone));

    mark_claude_code_connected(&codex_home);
    let mixed = spawn_provider_options(&config, /*auth_manager*/ None, &[]);
    assert_eq!(
        mixed
            .iter()
            .map(|option| option.id.as_str())
            .collect::<Vec<_>>(),
        vec![OPENAI_PROVIDER_ID, CLAUDE_CODE_PROVIDER_ID]
    );
    assert!(mixed[1].models.contains(&"claude-opus-5-5".to_string()));
    assert!(collaboration_messages_require_plaintext(&mixed));
}

#[tokio::test]
async fn claude_parent_always_uses_plaintext_and_can_list_openai() {
    let codex_home = TempDir::new().expect("tempdir");
    let config = config_in(&codex_home, CLAUDE_CODE_PROVIDER_ID).await;
    let auth_manager = AuthManager::from_auth_for_testing_with_home(
        codex_login::CodexAuth::from_api_key("sk-openai-test"),
        codex_home.path().to_path_buf(),
    );
    let options = spawn_provider_options(&config, Some(auth_manager.as_ref()), &[]);
    assert_eq!(options[0].id, CLAUDE_CODE_PROVIDER_ID);
    assert!(options[0].is_current);
    assert!(options.iter().any(|option| option.id == OPENAI_PROVIDER_ID));
    assert!(collaboration_messages_require_plaintext(&options));
}

#[tokio::test]
async fn registry_routes_each_provider_to_its_own_catalog() {
    let codex_home = TempDir::new().expect("tempdir");
    let openai_config = config_in(&codex_home, OPENAI_PROVIDER_ID).await;
    let root = create_model_provider(openai_config.model_provider.clone(), None)
        .models_manager_without_cache(/*config_model_catalog*/ None);
    let registry = ProviderModelsRegistry::new(
        OPENAI_PROVIDER_ID.to_string(),
        Arc::clone(&root),
        /*auth_manager*/ None,
    );
    assert!(Arc::ptr_eq(&registry.for_config(&openai_config), &root));

    let claude_config = config_in(&codex_home, ANTHROPIC_PROVIDER_ID).await;
    let claude = registry.for_config(&claude_config);
    assert!(!Arc::ptr_eq(&claude, &root));
    // Repeated lookups reuse the same catalog instance.
    assert!(Arc::ptr_eq(&claude, &registry.for_config(&claude_config)));
    let presets = claude
        .list_models(
            codex_models_manager::manager::RefreshStrategy::Offline,
            claude_config.http_client_factory(),
        )
        .await;
    assert!(
        presets
            .iter()
            .any(|preset| preset.model == "claude-sonnet-5-5")
    );
    assert!(
        !presets
            .iter()
            .any(|preset| preset.model.starts_with("gpt-"))
    );
}
