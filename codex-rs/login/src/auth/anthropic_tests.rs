use super::*;
use crate::auth::AuthDotJson;
use crate::auth::AuthManager;
use crate::auth::login_with_api_key;
use crate::auth::logout;
use crate::auth::logout_with_revoke;
use crate::auth::storage::AuthStorageBackend;
use crate::auth::storage::create_auth_storage_with_store;
use crate::test_support::transport_default_auth_route_config;
use codex_keyring_store::tests::MockKeyringStore;
use codex_protocol::auth::AuthMode;
use codex_secrets::compute_keyring_account;
use keyring::Error as KeyringError;
use pretty_assertions::assert_eq;
use serde_json::json;
use tempfile::tempdir;

const ANTHROPIC_KEY: &str = "sk-ant-api03-test-anthropic-key-0123456789abcdefghij";
const ROTATED_ANTHROPIC_KEY: &str = "sk-ant-api03-rotated-anthropic-key-9876543210zyxwvu";
const ENV_ANTHROPIC_KEY: &str = "sk-ant-api03-environment-anthropic-key-0123456789";
const OPENAI_KEY: &str = "sk-openai-test-key-0123456789";
const ROTATED_OPENAI_KEY: &str = "sk-openai-rotated-key-9876543210";

const ALL_STORE_CONFIGS: [(AuthCredentialsStoreMode, AuthKeyringBackendKind); 6] = [
    (
        AuthCredentialsStoreMode::File,
        AuthKeyringBackendKind::Direct,
    ),
    (
        AuthCredentialsStoreMode::Keyring,
        AuthKeyringBackendKind::Direct,
    ),
    (
        AuthCredentialsStoreMode::Keyring,
        AuthKeyringBackendKind::Secrets,
    ),
    (
        AuthCredentialsStoreMode::Auto,
        AuthKeyringBackendKind::Direct,
    ),
    (
        AuthCredentialsStoreMode::Auto,
        AuthKeyringBackendKind::Secrets,
    ),
    (
        AuthCredentialsStoreMode::Ephemeral,
        AuthKeyringBackendKind::Direct,
    ),
];

const PERSISTENT_STORE_CONFIGS: [(AuthCredentialsStoreMode, AuthKeyringBackendKind); 5] = [
    (
        AuthCredentialsStoreMode::File,
        AuthKeyringBackendKind::Direct,
    ),
    (
        AuthCredentialsStoreMode::Keyring,
        AuthKeyringBackendKind::Direct,
    ),
    (
        AuthCredentialsStoreMode::Keyring,
        AuthKeyringBackendKind::Secrets,
    ),
    (
        AuthCredentialsStoreMode::Auto,
        AuthKeyringBackendKind::Direct,
    ),
    (
        AuthCredentialsStoreMode::Auto,
        AuthKeyringBackendKind::Secrets,
    ),
];

fn anthropic_config(
    codex_home: &Path,
    store_mode: AuthCredentialsStoreMode,
    keyring_backend_kind: AuthKeyringBackendKind,
    keyring: &MockKeyringStore,
) -> AnthropicAuthConfig {
    AnthropicAuthConfig::new_with_keyring_store(
        codex_home.to_path_buf(),
        store_mode,
        keyring_backend_kind,
        Arc::new(keyring.clone()),
    )
}

fn chatgpt_storage(
    codex_home: &Path,
    store_mode: AuthCredentialsStoreMode,
    keyring_backend_kind: AuthKeyringBackendKind,
    keyring: &MockKeyringStore,
) -> Arc<dyn AuthStorageBackend> {
    create_auth_storage_with_store(
        codex_home.to_path_buf(),
        store_mode,
        Arc::new(keyring.clone()),
        keyring_backend_kind,
    )
}

fn chatgpt_api_key_auth(api_key: &str) -> AuthDotJson {
    AuthDotJson {
        auth_mode: Some(AuthMode::ApiKey),
        openai_api_key: Some(api_key.to_string()),
        tokens: None,
        last_refresh: None,
        agent_identity: None,
        personal_access_token: None,
        bedrock_api_key: None,
        bedrock_access_keys: None,
    }
}

fn anthropic_keyring_account(codex_home: &Path) -> String {
    format!("anthropic|{}", codex_home_store_hash(codex_home))
}

fn chatgpt_keyring_account(codex_home: &Path) -> String {
    format!("cli|{}", codex_home_store_hash(codex_home))
}

fn anthropic_file(codex_home: &Path) -> PathBuf {
    codex_home.join("anthropic_auth.json")
}

fn read_optional(path: &Path) -> io::Result<Option<Vec<u8>>> {
    match std::fs::read(path) {
        Ok(contents) => Ok(Some(contents)),
        Err(err) if err.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(err) => Err(err),
    }
}

fn directory_entries(path: &Path) -> anyhow::Result<Vec<String>> {
    let mut entries = std::fs::read_dir(path)?
        .map(|entry| entry.map(|entry| entry.file_name().to_string_lossy().into_owned()))
        .collect::<io::Result<Vec<_>>>()?;
    entries.sort();
    Ok(entries)
}

fn platform_failure() -> KeyringError {
    KeyringError::PlatformFailure(Box::new(io::Error::other("keyring unavailable")))
}

#[derive(Debug, PartialEq)]
struct ChatgptStorageSnapshot {
    auth_json: Option<Vec<u8>>,
    direct_keyring_entry: Option<String>,
    encrypted_auth_file: Option<Vec<u8>>,
    secrets_passphrase: Option<String>,
    loaded: Option<AuthDotJson>,
}

fn chatgpt_snapshot(
    codex_home: &Path,
    keyring: &MockKeyringStore,
    storage: &dyn AuthStorageBackend,
) -> anyhow::Result<ChatgptStorageSnapshot> {
    Ok(ChatgptStorageSnapshot {
        auth_json: read_optional(&codex_home.join("auth.json"))?,
        direct_keyring_entry: keyring.saved_value(&chatgpt_keyring_account(codex_home)),
        encrypted_auth_file: read_optional(&codex_home.join("secrets").join("codex_auth.age"))?,
        secrets_passphrase: keyring.saved_value(&compute_keyring_account(
            codex_home,
            LocalSecretsNamespace::CodexAuth,
        )),
        loaded: storage.load()?,
    })
}

#[derive(Debug, PartialEq)]
struct AnthropicStorageSnapshot {
    file: Option<Vec<u8>>,
    direct_keyring_entry: Option<String>,
    encrypted_file: Option<Vec<u8>>,
    secrets_passphrase: Option<String>,
    loaded: Option<String>,
}

fn anthropic_snapshot(
    codex_home: &Path,
    keyring: &MockKeyringStore,
    config: &AnthropicAuthConfig,
) -> anyhow::Result<AnthropicStorageSnapshot> {
    Ok(AnthropicStorageSnapshot {
        file: read_optional(&anthropic_file(codex_home))?,
        direct_keyring_entry: keyring.saved_value(&anthropic_keyring_account(codex_home)),
        encrypted_file: read_optional(&codex_home.join("secrets").join("anthropic_auth.age"))?,
        secrets_passphrase: keyring.saved_value(&compute_keyring_account(
            codex_home,
            LocalSecretsNamespace::AnthropicAuth,
        )),
        loaded: config.load_stored_api_key()?,
    })
}

#[test]
fn save_api_key_trims_and_rejects_invalid_keys() -> anyhow::Result<()> {
    let codex_home = tempdir()?;
    let keyring = MockKeyringStore::default();
    let config = anthropic_config(
        codex_home.path(),
        AuthCredentialsStoreMode::File,
        AuthKeyringBackendKind::Direct,
        &keyring,
    );

    for invalid in [
        "",
        "   \n\t",
        "sk-ant-api03 with-space",
        "sk-ant-api03\u{7}bell",
    ] {
        let error = config
            .save_api_key(invalid)
            .expect_err("invalid key should be rejected");
        assert_eq!(error.kind(), io::ErrorKind::InvalidInput);
        if !invalid.trim().is_empty() {
            assert!(!error.to_string().contains(invalid.trim()));
        }
    }
    assert!(!anthropic_file(codex_home.path()).exists());

    config.save_api_key(&format!("  {ANTHROPIC_KEY}\n"))?;
    assert_eq!(
        config.load_stored_api_key()?,
        Some(ANTHROPIC_KEY.to_string())
    );
    Ok(())
}

#[test]
fn file_store_round_trips_through_private_atomic_file() -> anyhow::Result<()> {
    let codex_home = tempdir()?;
    let keyring = MockKeyringStore::default();
    let config = anthropic_config(
        codex_home.path(),
        AuthCredentialsStoreMode::File,
        AuthKeyringBackendKind::Direct,
        &keyring,
    );

    config.save_api_key(ANTHROPIC_KEY)?;
    let saved: serde_json::Value =
        serde_json::from_slice(&std::fs::read(anthropic_file(codex_home.path()))?)?;
    assert_eq!(saved, json!({"version": 1, "api_key": ANTHROPIC_KEY}));
    #[cfg(unix)]
    {
        let mode = std::fs::metadata(anthropic_file(codex_home.path()))?
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o600);
    }

    config.save_api_key(ROTATED_ANTHROPIC_KEY)?;
    assert_eq!(
        (
            config.load_stored_api_key()?,
            directory_entries(codex_home.path())?,
        ),
        (
            Some(ROTATED_ANTHROPIC_KEY.to_string()),
            vec!["anthropic_auth.json".to_string()],
        )
    );
    assert!(!keyring.contains(&anthropic_keyring_account(codex_home.path())));

    assert!(config.delete()?);
    assert_eq!(config.load_stored_api_key()?, None);
    assert!(!anthropic_file(codex_home.path()).exists());
    assert!(!config.delete()?);
    Ok(())
}

#[test]
fn direct_keyring_store_round_trips_and_removes_plaintext_fallback() -> anyhow::Result<()> {
    let codex_home = tempdir()?;
    let keyring = MockKeyringStore::default();
    let account = anthropic_keyring_account(codex_home.path());
    anthropic_config(
        codex_home.path(),
        AuthCredentialsStoreMode::File,
        AuthKeyringBackendKind::Direct,
        &keyring,
    )
    .save_api_key(ROTATED_ANTHROPIC_KEY)?;
    let config = anthropic_config(
        codex_home.path(),
        AuthCredentialsStoreMode::Keyring,
        AuthKeyringBackendKind::Direct,
        &keyring,
    );

    config.save_api_key(ANTHROPIC_KEY)?;

    let saved: serde_json::Value = serde_json::from_str(
        &keyring
            .saved_value(&account)
            .expect("Anthropic keyring entry should exist"),
    )?;
    assert_eq!(saved, json!({"version": 1, "api_key": ANTHROPIC_KEY}));
    assert!(!anthropic_file(codex_home.path()).exists());
    assert!(!keyring.contains(&chatgpt_keyring_account(codex_home.path())));
    assert!(!codex_home.path().join("secrets").exists());
    assert_eq!(
        config.load_stored_api_key()?,
        Some(ANTHROPIC_KEY.to_string())
    );

    assert!(config.delete()?);
    assert_eq!(keyring.saved_value(&account), None);
    assert_eq!(config.load_stored_api_key()?, None);
    assert!(!config.delete()?);
    Ok(())
}

#[test]
fn secrets_keyring_store_round_trips_with_dedicated_namespace() -> anyhow::Result<()> {
    let codex_home = tempdir()?;
    let keyring = MockKeyringStore::default();
    let config = anthropic_config(
        codex_home.path(),
        AuthCredentialsStoreMode::Keyring,
        AuthKeyringBackendKind::Secrets,
        &keyring,
    );

    config.save_api_key(ANTHROPIC_KEY)?;

    assert_eq!(
        directory_entries(&codex_home.path().join("secrets"))?,
        vec!["anthropic_auth.age".to_string()]
    );
    assert!(
        !String::from_utf8_lossy(&std::fs::read(
            codex_home.path().join("secrets/anthropic_auth.age")
        )?)
        .contains(ANTHROPIC_KEY)
    );
    assert!(
        keyring
            .saved_value(&compute_keyring_account(
                codex_home.path(),
                LocalSecretsNamespace::AnthropicAuth,
            ))
            .is_some()
    );
    assert!(!keyring.contains(&compute_keyring_account(
        codex_home.path(),
        LocalSecretsNamespace::CodexAuth,
    )));
    assert!(!keyring.contains(&anthropic_keyring_account(codex_home.path())));
    assert!(!anthropic_file(codex_home.path()).exists());
    assert_eq!(
        config.load_stored_api_key()?,
        Some(ANTHROPIC_KEY.to_string())
    );

    assert!(config.delete()?);
    assert_eq!(config.load_stored_api_key()?, None);
    assert!(!config.delete()?);
    Ok(())
}

#[test]
fn auto_store_prefers_keyring_and_removes_plaintext_fallback() -> anyhow::Result<()> {
    for keyring_backend_kind in [
        AuthKeyringBackendKind::Direct,
        AuthKeyringBackendKind::Secrets,
    ] {
        let codex_home = tempdir()?;
        let keyring = MockKeyringStore::default();
        anthropic_config(
            codex_home.path(),
            AuthCredentialsStoreMode::File,
            keyring_backend_kind,
            &keyring,
        )
        .save_api_key(ROTATED_ANTHROPIC_KEY)?;
        let config = anthropic_config(
            codex_home.path(),
            AuthCredentialsStoreMode::Auto,
            keyring_backend_kind,
            &keyring,
        );

        config.save_api_key(ANTHROPIC_KEY)?;

        assert!(!anthropic_file(codex_home.path()).exists());
        assert_eq!(config.load_keyring()?, Some(ANTHROPIC_KEY.to_string()));
        assert_eq!(
            config.load_stored_api_key()?,
            Some(ANTHROPIC_KEY.to_string())
        );
        assert!(config.delete()?);
        assert_eq!(config.load_stored_api_key()?, None);
    }
    Ok(())
}

#[test]
fn auto_store_falls_back_to_file_when_keyring_save_fails() -> anyhow::Result<()> {
    for keyring_backend_kind in [
        AuthKeyringBackendKind::Direct,
        AuthKeyringBackendKind::Secrets,
    ] {
        let codex_home = tempdir()?;
        let keyring = MockKeyringStore::default();
        let failing_account = match keyring_backend_kind {
            AuthKeyringBackendKind::Direct => anthropic_keyring_account(codex_home.path()),
            AuthKeyringBackendKind::Secrets => {
                compute_keyring_account(codex_home.path(), LocalSecretsNamespace::AnthropicAuth)
            }
        };
        let config = anthropic_config(
            codex_home.path(),
            AuthCredentialsStoreMode::Auto,
            keyring_backend_kind,
            &keyring,
        );
        keyring.set_error(&failing_account, platform_failure());

        config.save_api_key(ANTHROPIC_KEY)?;

        let saved: serde_json::Value =
            serde_json::from_slice(&std::fs::read(anthropic_file(codex_home.path()))?)?;
        assert_eq!(saved, json!({"version": 1, "api_key": ANTHROPIC_KEY}));
        assert_eq!(config.load_keyring()?, None);
        assert_eq!(
            config.load_stored_api_key()?,
            Some(ANTHROPIC_KEY.to_string())
        );
        let restarted = anthropic_config(
            codex_home.path(),
            AuthCredentialsStoreMode::Auto,
            keyring_backend_kind,
            &keyring,
        );
        assert_eq!(
            restarted.load_stored_api_key()?,
            Some(ANTHROPIC_KEY.to_string())
        );
        assert!(restarted.delete()?);
        assert_eq!(restarted.load_stored_api_key()?, None);
    }
    Ok(())
}

#[test]
fn auto_store_reads_and_deletes_file_when_keyring_errors() -> anyhow::Result<()> {
    let codex_home = tempdir()?;
    let keyring = MockKeyringStore::default();
    let account = anthropic_keyring_account(codex_home.path());
    let config = anthropic_config(
        codex_home.path(),
        AuthCredentialsStoreMode::Auto,
        AuthKeyringBackendKind::Direct,
        &keyring,
    );
    anthropic_config(
        codex_home.path(),
        AuthCredentialsStoreMode::File,
        AuthKeyringBackendKind::Direct,
        &keyring,
    )
    .save_api_key(ANTHROPIC_KEY)?;

    keyring.set_error(&account, platform_failure());
    assert_eq!(
        config.load_stored_api_key()?,
        Some(ANTHROPIC_KEY.to_string())
    );

    keyring.set_error(&account, platform_failure());
    assert!(config.delete()?);
    assert!(!anthropic_file(codex_home.path()).exists());
    assert_eq!(config.load_stored_api_key()?, None);
    Ok(())
}

#[test]
fn keyring_mode_reports_keyring_errors_without_using_file() -> anyhow::Result<()> {
    let codex_home = tempdir()?;
    let keyring = MockKeyringStore::default();
    let account = anthropic_keyring_account(codex_home.path());
    let config = anthropic_config(
        codex_home.path(),
        AuthCredentialsStoreMode::Keyring,
        AuthKeyringBackendKind::Direct,
        &keyring,
    );

    keyring.set_error(&account, platform_failure());
    let error = config
        .save_api_key(ANTHROPIC_KEY)
        .expect_err("keyring mode must not fall back to a file");
    assert!(error.to_string().contains("keyring unavailable"), "{error}");
    assert!(!error.to_string().contains(ANTHROPIC_KEY));
    assert!(!anthropic_file(codex_home.path()).exists());

    config.save_api_key(ANTHROPIC_KEY)?;
    keyring.set_error(&account, platform_failure());
    let error = config
        .load_stored_api_key()
        .expect_err("keyring load errors should be reported");
    assert!(!format!("{error:?}").contains(ANTHROPIC_KEY));
    assert_eq!(
        config.load_stored_api_key()?,
        Some(ANTHROPIC_KEY.to_string())
    );
    Ok(())
}

#[test]
fn ephemeral_store_keeps_key_in_process_memory_only() -> anyhow::Result<()> {
    let codex_home = tempdir()?;
    let other_home = tempdir()?;
    let keyring = MockKeyringStore::default();
    let config = anthropic_config(
        codex_home.path(),
        AuthCredentialsStoreMode::Ephemeral,
        AuthKeyringBackendKind::Direct,
        &keyring,
    );

    config.save_api_key(ANTHROPIC_KEY)?;

    assert_eq!(directory_entries(codex_home.path())?, Vec::<String>::new());
    assert!(!keyring.contains(&anthropic_keyring_account(codex_home.path())));
    let same_process = anthropic_config(
        codex_home.path(),
        AuthCredentialsStoreMode::Ephemeral,
        AuthKeyringBackendKind::Secrets,
        &MockKeyringStore::default(),
    );
    assert_eq!(
        (
            same_process.load_stored_api_key()?,
            anthropic_config(
                other_home.path(),
                AuthCredentialsStoreMode::Ephemeral,
                AuthKeyringBackendKind::Direct,
                &keyring,
            )
            .load_stored_api_key()?,
            anthropic_config(
                codex_home.path(),
                AuthCredentialsStoreMode::File,
                AuthKeyringBackendKind::Direct,
                &keyring,
            )
            .load_stored_api_key()?,
        ),
        (Some(ANTHROPIC_KEY.to_string()), None, None)
    );

    assert!(config.delete()?);
    assert_eq!(same_process.load_stored_api_key()?, None);
    assert!(!config.delete()?);
    Ok(())
}

#[test]
fn stored_key_persists_across_restarts() -> anyhow::Result<()> {
    for (store_mode, keyring_backend_kind) in PERSISTENT_STORE_CONFIGS {
        let codex_home = tempdir()?;
        let keyring = MockKeyringStore::default();
        anthropic_config(
            codex_home.path(),
            store_mode,
            keyring_backend_kind,
            &keyring,
        )
        .save_api_key(ANTHROPIC_KEY)?;

        let restarted = anthropic_config(
            codex_home.path(),
            store_mode,
            keyring_backend_kind,
            &keyring,
        );
        let credential = restarted
            .resolve_with_env(|_| None)?
            .expect("stored key should resolve after restart");

        assert_eq!(
            (credential.api_key(), credential.source()),
            (ANTHROPIC_KEY, AnthropicCredentialSource::Stored),
            "{store_mode:?}/{keyring_backend_kind:?}"
        );
    }
    Ok(())
}

#[test]
fn resolve_prefers_stored_key_over_environment() -> anyhow::Result<()> {
    let codex_home = tempdir()?;
    let keyring = MockKeyringStore::default();
    let config = anthropic_config(
        codex_home.path(),
        AuthCredentialsStoreMode::File,
        AuthKeyringBackendKind::Direct,
        &keyring,
    );
    let env_with = |value: &'static str| {
        move |name: &str| (name == ANTHROPIC_API_KEY_ENV_VAR).then(|| value.to_string())
    };

    assert!(config.resolve_with_env(|_| None)?.is_none());
    assert!(config.resolve_with_env(env_with(" \n\t"))?.is_none());
    assert!(
        config
            .resolve_with_env(
                |name| (name != ANTHROPIC_API_KEY_ENV_VAR).then(|| ENV_ANTHROPIC_KEY.to_string())
            )?
            .is_none()
    );

    let from_env = config
        .resolve_with_env(env_with(
            " sk-ant-api03-environment-anthropic-key-0123456789\n",
        ))?
        .expect("environment key should resolve");
    assert_eq!(
        (
            from_env.api_key(),
            from_env.source(),
            from_env.source().description(),
        ),
        (
            ENV_ANTHROPIC_KEY,
            AnthropicCredentialSource::Environment,
            "ANTHROPIC_API_KEY environment variable",
        )
    );

    config.save_api_key(ANTHROPIC_KEY)?;
    let stored = config
        .resolve_with_env(env_with(ENV_ANTHROPIC_KEY))?
        .expect("stored key should resolve");
    assert_eq!(
        (
            stored.api_key(),
            stored.source(),
            stored.source().description(),
        ),
        (
            ANTHROPIC_KEY,
            AnthropicCredentialSource::Stored,
            "stored by Codex",
        )
    );

    assert!(config.delete()?);
    let after_logout = config
        .resolve_with_env(env_with(ENV_ANTHROPIC_KEY))?
        .expect("environment key should resolve after deleting the stored key");
    assert_eq!(
        after_logout.source(),
        AnthropicCredentialSource::Environment
    );
    Ok(())
}

#[test]
fn debug_output_and_masking_never_expose_api_key() -> anyhow::Result<()> {
    let codex_home = tempdir()?;
    let keyring = MockKeyringStore::default();
    let config = anthropic_config(
        codex_home.path(),
        AuthCredentialsStoreMode::Keyring,
        AuthKeyringBackendKind::Direct,
        &keyring,
    );
    config.save_api_key(ANTHROPIC_KEY)?;
    let credential = config
        .resolve_with_env(|_| Some(ENV_ANTHROPIC_KEY.to_string()))?
        .expect("stored key should resolve");

    let credential_debug = format!("{credential:?}");
    let config_debug = format!("{config:?}");
    for debug in [&credential_debug, &config_debug] {
        assert!(!debug.contains(ANTHROPIC_KEY), "{debug}");
        assert!(!debug.contains("anthropic-key"), "{debug}");
    }
    assert_eq!(
        credential_debug,
        r#"AnthropicCredential { api_key: "[REDACTED]", source: Stored }"#
    );

    let short = AnthropicCredential {
        api_key: "sk-ant-short-key".to_string(),
        source: AnthropicCredentialSource::Environment,
    };
    assert_eq!(
        (credential.masked(), short.masked()),
        ("sk-ant-api***ghij".to_string(), "***".to_string())
    );
    Ok(())
}

#[test]
fn malformed_storage_errors_do_not_expose_key_material() -> anyhow::Result<()> {
    let leaked = "sk-ant-api03-private-credential-value-0123456789";
    let codex_home = tempdir()?;
    let keyring = MockKeyringStore::default();
    let file_config = anthropic_config(
        codex_home.path(),
        AuthCredentialsStoreMode::File,
        AuthKeyringBackendKind::Direct,
        &keyring,
    );
    let keyring_config = anthropic_config(
        codex_home.path(),
        AuthCredentialsStoreMode::Keyring,
        AuthKeyringBackendKind::Direct,
        &keyring,
    );

    for malformed in [
        format!(r#"{{"version":"{leaked}","api_key":"x"}}"#),
        format!(r#"{{"version":1,"api_key":["{leaked}"]}}"#),
        format!(r#"{{"version":1 "api_key":"{leaked}"}}"#),
    ] {
        std::fs::write(anthropic_file(codex_home.path()), &malformed)?;
        keyring
            .save(
                ANTHROPIC_KEYRING_SERVICE,
                &anthropic_keyring_account(codex_home.path()),
                &malformed,
            )
            .expect("seed malformed keyring entry");

        for error in [
            file_config
                .load_stored_api_key()
                .expect_err("malformed file should fail to parse"),
            keyring_config
                .load_stored_api_key()
                .expect_err("malformed keyring entry should fail to parse"),
        ] {
            assert!(error.to_string().starts_with("failed to parse"), "{error}");
            for formatted in [error.to_string(), format!("{error:?}")] {
                assert!(!formatted.contains(leaked), "{formatted}");
            }
        }
    }

    std::fs::write(
        anthropic_file(codex_home.path()),
        format!(r#"{{"version":2,"api_key":"{leaked}"}}"#),
    )?;
    let error = file_config
        .load_stored_api_key()
        .expect_err("newer format versions should be rejected");
    assert!(error.to_string().contains("version 2"), "{error}");
    assert!(!error.to_string().contains(leaked));
    Ok(())
}

#[test]
fn anthropic_changes_leave_chatgpt_auth_untouched() -> anyhow::Result<()> {
    for (store_mode, keyring_backend_kind) in ALL_STORE_CONFIGS {
        let codex_home = tempdir()?;
        let keyring = MockKeyringStore::default();
        let chatgpt = chatgpt_storage(
            codex_home.path(),
            store_mode,
            keyring_backend_kind,
            &keyring,
        );
        chatgpt.save(&chatgpt_api_key_auth(OPENAI_KEY))?;
        let before = chatgpt_snapshot(codex_home.path(), &keyring, chatgpt.as_ref())?;
        assert_eq!(before.loaded, Some(chatgpt_api_key_auth(OPENAI_KEY)));
        let anthropic = anthropic_config(
            codex_home.path(),
            store_mode,
            keyring_backend_kind,
            &keyring,
        );

        anthropic.save_api_key(ANTHROPIC_KEY)?;
        anthropic.save_api_key(ROTATED_ANTHROPIC_KEY)?;
        assert_eq!(
            anthropic.load_stored_api_key()?,
            Some(ROTATED_ANTHROPIC_KEY.to_string())
        );
        assert_eq!(
            chatgpt_snapshot(codex_home.path(), &keyring, chatgpt.as_ref())?,
            before,
            "save: {store_mode:?}/{keyring_backend_kind:?}"
        );

        assert!(anthropic.delete()?);
        assert_eq!(anthropic.load_stored_api_key()?, None);
        assert_eq!(
            chatgpt_snapshot(codex_home.path(), &keyring, chatgpt.as_ref())?,
            before,
            "delete: {store_mode:?}/{keyring_backend_kind:?}"
        );
    }
    Ok(())
}

#[test]
fn chatgpt_storage_changes_leave_anthropic_key_untouched() -> anyhow::Result<()> {
    for (store_mode, keyring_backend_kind) in ALL_STORE_CONFIGS {
        let codex_home = tempdir()?;
        let keyring = MockKeyringStore::default();
        let anthropic = anthropic_config(
            codex_home.path(),
            store_mode,
            keyring_backend_kind,
            &keyring,
        );
        anthropic.save_api_key(ANTHROPIC_KEY)?;
        let before = anthropic_snapshot(codex_home.path(), &keyring, &anthropic)?;
        assert_eq!(before.loaded, Some(ANTHROPIC_KEY.to_string()));
        let chatgpt = chatgpt_storage(
            codex_home.path(),
            store_mode,
            keyring_backend_kind,
            &keyring,
        );

        chatgpt.save(&chatgpt_api_key_auth(OPENAI_KEY))?;
        chatgpt.save(&chatgpt_api_key_auth(ROTATED_OPENAI_KEY))?;
        assert_eq!(
            anthropic_snapshot(codex_home.path(), &keyring, &anthropic)?,
            before,
            "ChatGPT save: {store_mode:?}/{keyring_backend_kind:?}"
        );

        assert!(chatgpt.delete()?);
        assert_eq!(chatgpt.load()?, None);
        assert_eq!(
            anthropic_snapshot(codex_home.path(), &keyring, &anthropic)?,
            before,
            "ChatGPT delete: {store_mode:?}/{keyring_backend_kind:?}"
        );
    }
    Ok(())
}

#[tokio::test]
async fn chatgpt_login_and_logout_leave_anthropic_key_untouched() -> anyhow::Result<()> {
    for store_mode in [
        AuthCredentialsStoreMode::File,
        AuthCredentialsStoreMode::Ephemeral,
    ] {
        let keyring_backend_kind = AuthKeyringBackendKind::Direct;
        let codex_home = tempdir()?;
        let keyring = MockKeyringStore::default();
        let anthropic = anthropic_config(
            codex_home.path(),
            store_mode,
            keyring_backend_kind,
            &keyring,
        );
        anthropic.save_api_key(ANTHROPIC_KEY)?;
        let before = anthropic_snapshot(codex_home.path(), &keyring, &anthropic)?;
        let assert_anthropic_intact = |step: &str| -> anyhow::Result<()> {
            assert_eq!(
                anthropic_snapshot(codex_home.path(), &keyring, &anthropic)?,
                before,
                "{step}: {store_mode:?}"
            );
            Ok(())
        };

        login_with_api_key(
            codex_home.path(),
            OPENAI_KEY,
            store_mode,
            keyring_backend_kind,
        )?;
        assert_anthropic_intact("login_with_api_key")?;
        assert!(logout(codex_home.path(), store_mode, keyring_backend_kind)?);
        assert_anthropic_intact("logout")?;

        login_with_api_key(
            codex_home.path(),
            ROTATED_OPENAI_KEY,
            store_mode,
            keyring_backend_kind,
        )?;
        let manager = AuthManager::new(
            codex_home.path().to_path_buf(),
            /*enable_codex_api_key_env*/ false,
            store_mode,
            /*forced_chatgpt_workspace_id*/ None,
            /*chatgpt_base_url*/ None,
            keyring_backend_kind,
            transport_default_auth_route_config(),
        )
        .await;
        assert_eq!(
            manager
                .auth_cached()
                .and_then(|auth| auth.api_key().map(str::to_string)),
            Some(ROTATED_OPENAI_KEY.to_string())
        );
        assert!(manager.logout().await?);
        assert!(manager.auth_cached().is_none());
        assert_anthropic_intact("AuthManager::logout")?;

        login_with_api_key(
            codex_home.path(),
            OPENAI_KEY,
            store_mode,
            keyring_backend_kind,
        )?;
        assert!(
            logout_with_revoke(
                codex_home.path(),
                store_mode,
                keyring_backend_kind,
                &transport_default_auth_route_config(),
            )
            .await?
        );
        assert_anthropic_intact("logout_with_revoke")?;
    }
    Ok(())
}

#[tokio::test]
async fn auth_manager_exposes_matching_anthropic_auth_config() -> anyhow::Result<()> {
    let codex_home = tempdir()?;
    let manager = AuthManager::new(
        codex_home.path().to_path_buf(),
        /*enable_codex_api_key_env*/ false,
        AuthCredentialsStoreMode::File,
        /*forced_chatgpt_workspace_id*/ None,
        /*chatgpt_base_url*/ None,
        AuthKeyringBackendKind::Secrets,
        transport_default_auth_route_config(),
    )
    .await;

    let config = manager.anthropic_auth_config();
    assert_eq!(
        (
            config.codex_home.clone(),
            config.store_mode,
            config.keyring_backend_kind,
        ),
        (
            codex_home.path().to_path_buf(),
            AuthCredentialsStoreMode::File,
            AuthKeyringBackendKind::Secrets,
        )
    );

    config.save_api_key(ANTHROPIC_KEY)?;
    assert_eq!(
        manager.anthropic_auth_config().load_stored_api_key()?,
        Some(ANTHROPIC_KEY.to_string())
    );
    assert!(manager.auth_cached().is_none());
    assert!(!codex_home.path().join("auth.json").exists());
    Ok(())
}
