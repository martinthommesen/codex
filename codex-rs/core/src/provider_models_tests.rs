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
async fn managed_provider_requirement_limits_spawn_providers() {
    let codex_home = TempDir::new().expect("tempdir");
    let mut config = config_in(&codex_home, OPENAI_PROVIDER_ID).await;
    mark_claude_code_connected(&codex_home);
    config.config_layer_stack = codex_config::ConfigLayerStack::new(
        config
            .config_layer_stack
            .all_layers_low_to_high()
            .cloned()
            .collect(),
        config.config_layer_stack.requirements().clone(),
        codex_config::ConfigRequirementsToml {
            model_provider: Some(OPENAI_PROVIDER_ID.to_string()),
            ..Default::default()
        },
    )
    .expect("config layer stack");

    assert_eq!(
        check_required_model_provider(&config, OPENAI_PROVIDER_ID),
        Ok(())
    );
    assert!(check_required_model_provider(&config, CLAUDE_CODE_PROVIDER_ID).is_err());
    // claude-code is connected, but the requirement keeps the whole agent tree on OpenAI.
    let options = spawn_provider_options(&config, /*auth_manager*/ None, &[]);
    assert_eq!(
        options
            .iter()
            .map(|option| option.id.as_str())
            .collect::<Vec<_>>(),
        vec![OPENAI_PROVIDER_ID]
    );
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

#[tokio::test]
async fn oauth_connection_never_inherits_other_provider_credentials_or_connection_markers() {
    let home = TempDir::new().expect("home");
    let config = config_in(&home, OPENAI_PROVIDER_ID).await;
    let manager = AuthManager::from_auth_for_testing_with_home(
        codex_login::CodexAuth::from_api_key("openai-test"),
        home.path().to_path_buf(),
    );
    manager
        .anthropic_auth_config()
        .save_api_key("anthropic-test")
        .expect("store key");
    mark_claude_code_connected(&home);
    let provider = &config.model_providers[CLAUDE_OAUTH_PROVIDER_ID];
    assert!(
        matches!(provider_connection(provider, Some(manager.as_ref()), home.path()), ProviderConnection::NotConnected { remedy } if remedy.contains("login --provider claude-oauth"))
    );
    let options = spawn_provider_options(&config, Some(manager.as_ref()), &[]);
    assert!(
        options
            .iter()
            .any(|option| option.id == ANTHROPIC_PROVIDER_ID)
    );
    assert!(
        options
            .iter()
            .any(|option| option.id == CLAUDE_CODE_PROVIDER_ID)
    );
    assert!(
        !options
            .iter()
            .any(|option| option.id == CLAUDE_OAUTH_PROVIDER_ID)
    );

    // Invalid generic auth cannot make OAuth appear connected through the early key path.
    let custom = ModelProviderInfo {
        experimental_bearer_token: Some("other-account".into()),
        ..provider.clone()
    };
    assert!(matches!(
        provider_connection(&custom, Some(manager.as_ref()), home.path()),
        ProviderConnection::NotConnected { .. }
    ));
}

#[tokio::test]
async fn oauth_parent_lists_connected_other_providers_and_keeps_collaboration_readable() {
    let home = TempDir::new().expect("home");
    let config = config_in(&home, CLAUDE_OAUTH_PROVIDER_ID).await;
    let manager = AuthManager::from_auth_for_testing_with_home(
        codex_login::CodexAuth::from_api_key("openai-test"),
        home.path().to_path_buf(),
    );
    manager
        .anthropic_auth_config()
        .save_api_key("anthropic-test")
        .expect("store key");
    mark_claude_code_connected(&home);
    let options = spawn_provider_options(&config, Some(manager.as_ref()), &[]);
    assert_eq!(options[0].id, CLAUDE_OAUTH_PROVIDER_ID);
    assert!(options[0].is_current);
    assert!(options[0].description.contains("experimental"));
    for id in [
        OPENAI_PROVIDER_ID,
        ANTHROPIC_PROVIDER_ID,
        CLAUDE_CODE_PROVIDER_ID,
    ] {
        assert!(options.iter().any(|option| option.id == id), "{id}");
    }
    assert!(collaboration_messages_require_plaintext(&options));
}

#[tokio::test]
async fn oauth_models_use_own_catalog_on_provider_switch_and_repeat_lookup() {
    let home = TempDir::new().expect("home");
    let openai = config_in(&home, OPENAI_PROVIDER_ID).await;
    let root = create_model_provider(openai.model_provider.clone(), None)
        .models_manager_without_cache(None);
    let registry =
        ProviderModelsRegistry::new(OPENAI_PROVIDER_ID.to_string(), Arc::clone(&root), None);
    let oauth = config_in(&home, CLAUDE_OAUTH_PROVIDER_ID).await;
    let catalog = registry.for_config(&oauth);
    assert!(!Arc::ptr_eq(&catalog, &root));
    assert!(Arc::ptr_eq(&catalog, &registry.for_config(&oauth)));
    let models = catalog
        .list_models(
            codex_models_manager::manager::RefreshStrategy::Offline,
            oauth.http_client_factory(),
        )
        .await;
    assert!(models.iter().any(|model| model.model == "claude-opus-5-5"));
    assert!(models.iter().all(|model| !model.model.starts_with("gpt-")));
    let selected = catalog
        .get_default_model(
            &Some("claude-opus-5-5".to_string()),
            true,
            codex_models_manager::manager::RefreshStrategy::Offline,
            oauth.http_client_factory(),
        )
        .await;
    assert_eq!(selected, "claude-opus-5-5");
}
