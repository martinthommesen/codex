use super::*;
use codex_keyring_store::tests::MockKeyringStore;
use pretty_assertions::assert_eq;
use wiremock::Mock;
use wiremock::MockServer;
use wiremock::ResponseTemplate;
use wiremock::matchers::body_json;
use wiremock::matchers::header;
use wiremock::matchers::method;
use wiremock::matchers::path;

fn configuration(home: &std::path::Path, mode: AuthCredentialsStoreMode) -> ClaudeOAuthAuthConfig {
    let mut config =
        ClaudeOAuthAuthConfig::new(home.to_path_buf(), mode, AuthKeyringBackendKind::Direct);
    config.keyring = Arc::new(MockKeyringStore::default());
    config
}

fn credential(expiry: u64) -> ClaudeOAuthCredential {
    ClaudeOAuthCredential {
        version: 1,
        access_token: "sensitive-access-token".into(),
        refresh_token: "sensitive-refresh-token".into(),
        expires_at: expiry,
        account_uuid: "aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa".into(),
        organization_uuid: "bbbbbbbb-bbbb-4bbb-8bbb-bbbbbbbbbbbb".into(),
        email: "example@example.com".into(),
        device_id: "ab".repeat(32),
        refresh_blocked_until: 0,
        refresh_requires_login: false,
    }
}

fn mock_endpoints(config: &mut ClaudeOAuthAuthConfig, server: &MockServer) {
    config.endpoints = Endpoints {
        token: format!("{}/token", server.uri()),
        profile: format!("{}/profile", server.uri()),
        roles: format!("{}/roles", server.uri()),
    };
}

async fn profile_mock(server: &MockServer) {
    Mock::given(method("GET"))
        .and(path("/profile"))
        .and(header("Authorization", "Bearer new-access"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "account": { "uuid": "aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa", "email": "example@example.com" },
            "organization": { "uuid": "bbbbbbbb-bbbb-4bbb-8bbb-bbbbbbbbbbbb" }
        })))
        .mount(server).await;
}

#[test]
fn pkce_and_authorization_parameters_match_reference() {
    let login = ClaudeOAuthLogin::new().expect("PKCE generation");
    assert_eq!(login.verifier.len(), 128);
    assert_eq!(login.state.len(), 32);
    let parameters: HashMap<_, _> = login
        .authorization_url()
        .query_pairs()
        .into_owned()
        .collect();
    assert_eq!(
        login.authorization_url().origin().ascii_serialization(),
        "https://claude.ai"
    );
    assert_eq!(parameters["client_id"], CLIENT_ID);
    assert_eq!(parameters["redirect_uri"], REDIRECT_URI);
    assert_eq!(parameters["scope"], SCOPE);
    assert_eq!(parameters["code"], "true");
    assert_eq!(parameters["response_type"], "code");
    assert_eq!(parameters["code_challenge_method"], "S256");
    assert_eq!(
        parameters["code_challenge"],
        base64::engine::general_purpose::URL_SAFE_NO_PAD
            .encode(Sha256::digest(login.verifier.as_bytes()))
    );
    assert_ne!(
        login.verifier,
        ClaudeOAuthLogin::new().expect("PKCE").verifier
    );
    // RFC 7636 Appendix B independently checks the S256 primitive.
    assert_eq!(
        base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(Sha256::digest(
            b"dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk"
        )),
        "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM"
    );
}

#[test]
fn callback_rejects_csrf_duplicates_wrong_origin_and_fragment_override() {
    let login = ClaudeOAuthLogin::new().expect("login");
    assert_eq!(
        login
            .validated_code(&format!("code-value#{}", login.state))
            .expect("manual callback"),
        "code-value"
    );
    assert_eq!(
        login
            .validated_code(&format!(
                "{REDIRECT_URI}?code=code-value&state={}",
                login.state
            ))
            .expect("URL callback"),
        "code-value"
    );
    for callback in [
        "code-value#incorrect".to_string(),
        "code-without-state".to_string(),
        format!(
            "https://attacker.example/callback?code=secret&state={}",
            login.state
        ),
        format!(
            "{REDIRECT_URI}?code=secret&state={}&state=other",
            login.state
        ),
        format!("{REDIRECT_URI}?code=secret%23other&state={}", login.state),
        format!(
            "{REDIRECT_URI}?error=secret-bearing-server-error&state={}",
            login.state
        ),
    ] {
        let error = login
            .validated_code(&callback)
            .expect_err("invalid callback rejected");
        assert!(!format!("{error:?}").contains("secret"));
        assert!(!format!("{error:#}").contains(&login.state));
    }
}

#[test]
fn grant_serialization_preserves_reference_field_order() {
    let grant = CodeGrant {
        grant_type: "authorization_code",
        code: "code",
        redirect_uri: REDIRECT_URI,
        client_id: CLIENT_ID,
        code_verifier: "verifier",
        state: "state",
    };
    assert_eq!(
        serde_json::to_string(&grant).expect("serialize"),
        format!(
            r#"{{"grant_type":"authorization_code","code":"code","redirect_uri":"{REDIRECT_URI}","client_id":"{CLIENT_ID}","code_verifier":"verifier","state":"state"}}"#
        )
    );
    let grant = RefreshGrant {
        client_id: CLIENT_ID,
        grant_type: "refresh_token",
        refresh_token: "refresh",
        scope: SCOPE,
    };
    assert_eq!(
        serde_json::to_string(&grant).expect("serialize"),
        format!(
            r#"{{"client_id":"{CLIENT_ID}","grant_type":"refresh_token","refresh_token":"refresh","scope":"{SCOPE}"}}"#
        )
    );
}

#[test]
fn credential_debug_and_storage_parse_errors_redact_secrets() {
    let secret = credential(now() + 3600);
    let debug = format!("{secret:?}");
    assert!(!debug.contains(&secret.access_token));
    assert!(!debug.contains(&secret.refresh_token));
    assert!(!debug.contains(&secret.email));
    let error =
        decode_credential(r#"{"version":"sensitive-access-token"}"#).expect_err("bad credential");
    assert!(!format!("{error:?}").contains("sensitive-access-token"));
}

#[test]
fn encrypted_store_and_keyring_are_isolated_from_other_providers() {
    let home = tempfile::tempdir().expect("tempdir");
    let config = configuration(home.path(), AuthCredentialsStoreMode::File);
    config
        .save(&credential(now() + 3600))
        .expect("encrypted save");
    let bytes =
        std::fs::read(home.path().join("secrets/claude_oauth.age")).expect("encrypted file");
    assert!(!String::from_utf8_lossy(&bytes).contains("sensitive"));
    assert!(!home.path().join("auth.json").exists());
    assert!(!home.path().join("anthropic_auth.json").exists());
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        let metadata =
            std::fs::metadata(home.path().join("secrets/claude_oauth.age")).expect("metadata");
        assert_eq!(metadata.permissions().mode() & 0o777, 0o600);
    }
    assert_eq!(
        config
            .load_stored_credential()
            .expect("load")
            .expect("credential")
            .access_token(),
        "sensitive-access-token"
    );
    assert!(config.delete().expect("logout"));
    assert!(config.load_stored_credential().expect("load").is_none());
}

#[test]
fn unavailable_keyring_never_creates_plaintext_fallback() {
    let home = tempfile::tempdir().expect("tempdir");
    let mut config = configuration(home.path(), AuthCredentialsStoreMode::Auto);
    let keyring = MockKeyringStore::default();
    let account =
        codex_secrets::compute_keyring_account(home.path(), LocalSecretsNamespace::ClaudeOAuth);
    keyring.set_error(
        &account,
        keyring::Error::NoStorageAccess(Box::new(io::Error::other("unavailable"))),
    );
    config.keyring = Arc::new(keyring);
    let error = config
        .save(&credential(now() + 3600))
        .expect_err("keyring unavailable");
    assert!(error.to_string().contains("plaintext storage is disabled"));
    assert!(!home.path().join("claude_oauth_auth.json").exists());
    assert!(!home.path().join("auth.json").exists());
}

#[test]
fn logout_removes_unreadable_encrypted_credentials_without_needing_the_key() {
    let home = tempfile::tempdir().expect("tempdir");
    let config = configuration(home.path(), AuthCredentialsStoreMode::File);
    let directory = home.path().join("secrets");
    std::fs::create_dir_all(&directory).expect("secrets directory");
    std::fs::write(directory.join("claude_oauth.age"), "corrupt ciphertext")
        .expect("corrupt fixture");
    assert!(config.load_stored_credential().is_err());
    assert!(config.delete().expect("logout without decryption"));
    assert!(!directory.join("claude_oauth.age").exists());
    assert!(
        config
            .load_stored_credential()
            .expect("missing credentials")
            .is_none()
    );
}

#[test]
fn logout_during_refresh_reports_busy_without_blocking_async_executor() {
    let home = tempfile::tempdir().expect("tempdir");
    let config = configuration(home.path(), AuthCredentialsStoreMode::Keyring);
    config
        .save(&credential(now() + 3600))
        .expect("keyring save");
    let lock = config.open_lock_file().expect("refresh lock");
    lock.lock().expect("lock");
    assert_eq!(
        config.delete().expect_err("busy refresh").kind(),
        io::ErrorKind::WouldBlock
    );
    assert!(
        config
            .load_stored_credential()
            .expect("credential preserved")
            .is_some()
    );
    drop(lock);
    assert!(config.delete().expect("logout after refresh"));
}

#[tokio::test]
async fn absent_oauth_credentials_never_fall_back_to_api_key_storage() {
    let home = tempfile::tempdir().expect("tempdir");
    let config = configuration(home.path(), AuthCredentialsStoreMode::Ephemeral);
    let anthropic = super::super::AnthropicAuthConfig::new(
        home.path().to_path_buf(),
        AuthCredentialsStoreMode::File,
        AuthKeyringBackendKind::Direct,
    );
    anthropic
        .save_api_key("sk-ant-api-key-for-separate-billing")
        .expect("API key save");
    assert!(config.resolve().await.expect("resolve").is_none());
    config.save(&credential(now() + 3600)).expect("save");
    assert!(config.delete().expect("logout"));
    assert!(
        anthropic
            .load_stored_api_key()
            .expect("API key unchanged")
            .is_some()
    );
}

#[tokio::test]
async fn code_exchange_validates_identity_before_secure_persistence() {
    let home = tempfile::tempdir().expect("tempdir");
    let server = MockServer::start().await;
    let mut config = configuration(home.path(), AuthCredentialsStoreMode::Ephemeral);
    mock_endpoints(&mut config, &server);
    let login = ClaudeOAuthLogin::new().expect("login");
    let callback = format!("auth-code#{}", login.state);
    Mock::given(method("POST")).and(path("/token")).and(body_json(serde_json::json!({
        "grant_type": "authorization_code", "code": "auth-code", "redirect_uri": REDIRECT_URI,
        "client_id": CLIENT_ID, "code_verifier": login.verifier, "state": login.state,
    }))).respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
        "access_token": "new-access", "refresh_token": "new-refresh", "expires_in": 3600, "token_type": "Bearer"
    }))).expect(1).mount(&server).await;
    profile_mock(&server).await;
    Mock::given(method("GET"))
        .and(path("/roles"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({"roles": []})))
        .expect(1)
        .mount(&server)
        .await;
    let credential = config
        .complete_login(login, &callback)
        .await
        .expect("complete login");
    assert_eq!(credential.access_token(), "new-access");
    assert_eq!(
        credential.account_uuid(),
        "aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa"
    );
    assert_eq!(credential.device_id().len(), 64);
    assert!(config.load_stored_credential().expect("stored").is_some());
}

#[tokio::test]
async fn concurrent_refresh_rotates_once_and_retains_device_identity() {
    let home = tempfile::tempdir().expect("tempdir");
    let server = MockServer::start().await;
    let mut config = configuration(home.path(), AuthCredentialsStoreMode::Ephemeral);
    mock_endpoints(&mut config, &server);
    config.save(&credential(1)).expect("save expired");
    Mock::given(method("POST")).and(path("/token")).and(body_json(serde_json::json!({
        "client_id": CLIENT_ID, "grant_type": "refresh_token", "refresh_token": "sensitive-refresh-token", "scope": SCOPE
    }))).respond_with(ResponseTemplate::new(200).set_delay(Duration::from_millis(25)).set_body_json(serde_json::json!({
        "access_token": "new-access", "refresh_token": "rotated-refresh", "expires_in": 3600
    }))).expect(1).mount(&server).await;
    profile_mock(&server).await;
    let mut tasks = Vec::new();
    for _ in 0..8 {
        let config = config.clone();
        tasks.push(tokio::spawn(async move { config.resolve().await }));
    }
    for task in tasks {
        let credential = task
            .await
            .expect("task")
            .expect("refresh")
            .expect("credential");
        assert_eq!(credential.access_token(), "new-access");
        assert_eq!(credential.device_id(), "ab".repeat(32));
    }
    let stored = config
        .load_stored_credential()
        .expect("load")
        .expect("credential");
    assert_eq!(stored.refresh_token, "rotated-refresh");
    assert!(!stored.refresh_requires_login);
}

#[tokio::test]
async fn refresh_without_rotated_refresh_token_preserves_old_token_and_identity() {
    let home = tempfile::tempdir().expect("tempdir");
    let server = MockServer::start().await;
    let mut config = configuration(home.path(), AuthCredentialsStoreMode::Ephemeral);
    mock_endpoints(&mut config, &server);
    config.save(&credential(1)).expect("save");
    Mock::given(method("POST"))
        .and(path("/token"))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(
                serde_json::json!({ "access_token": "new-access", "expires_in": 3600 }),
            ),
        )
        .expect(1)
        .mount(&server)
        .await;
    let refreshed = config
        .resolve()
        .await
        .expect("refresh")
        .expect("credential");
    assert_eq!(refreshed.refresh_token, "sensitive-refresh-token");
    assert_eq!(refreshed.email(), "example@example.com");
}

#[tokio::test]
async fn ambiguous_refresh_response_is_never_replayed() {
    let home = tempfile::tempdir().expect("tempdir");
    let server = MockServer::start().await;
    let mut config = configuration(home.path(), AuthCredentialsStoreMode::Ephemeral);
    mock_endpoints(&mut config, &server);
    config.save(&credential(1)).expect("save");
    Mock::given(method("POST"))
        .and(path("/token"))
        .respond_with(
            ResponseTemplate::new(200).set_body_string("not-json-sensitive-refresh-token"),
        )
        .expect(1)
        .mount(&server)
        .await;
    for _ in 0..2 {
        let error = config.resolve().await.expect_err("refresh failed");
        assert!(!format!("{error:?}").contains("sensitive-refresh-token"));
    }
    assert!(
        config
            .load_stored_credential()
            .expect("load")
            .expect("credential")
            .refresh_requires_login
    );
}

#[tokio::test]
async fn refresh_429_persists_retry_after_and_blocks_immediate_replay() {
    let home = tempfile::tempdir().expect("tempdir");
    let server = MockServer::start().await;
    let mut config = configuration(home.path(), AuthCredentialsStoreMode::Ephemeral);
    mock_endpoints(&mut config, &server);
    config.save(&credential(1)).expect("save");
    Mock::given(method("POST"))
        .and(path("/token"))
        .respond_with(
            ResponseTemplate::new(429)
                .insert_header("retry-after", "60")
                .set_body_string("sensitive-refresh-token"),
        )
        .expect(1)
        .mount(&server)
        .await;
    assert!(config.resolve().await.is_err());
    assert!(config.resolve().await.is_err());
    let stored = config
        .load_stored_credential()
        .expect("load")
        .expect("credential");
    assert!(!stored.refresh_requires_login);
    assert!(stored.refresh_blocked_until >= now() + 59);
}

#[tokio::test]
async fn oauth_redirects_never_forward_credentials_or_codes() {
    let home = tempfile::tempdir().expect("tempdir");
    let server = MockServer::start().await;
    let thief = MockServer::start().await;
    let mut config = configuration(home.path(), AuthCredentialsStoreMode::Ephemeral);
    mock_endpoints(&mut config, &server);
    config.save(&credential(1)).expect("save");
    Mock::given(method("POST"))
        .and(path("/token"))
        .respond_with(
            ResponseTemplate::new(307).insert_header("Location", format!("{}/stolen", thief.uri())),
        )
        .expect(1)
        .mount(&server)
        .await;
    let error = config.resolve().await.expect_err("redirect rejected");
    assert!(error.to_string().contains("redirect refused"));
    assert!(
        thief
            .received_requests()
            .await
            .expect("requests")
            .is_empty()
    );
}

#[tokio::test]
async fn revoked_refresh_credentials_require_login_without_fallback() {
    let home = tempfile::tempdir().expect("tempdir");
    let server = MockServer::start().await;
    let mut config = configuration(home.path(), AuthCredentialsStoreMode::Ephemeral);
    mock_endpoints(&mut config, &server);
    config.save(&credential(1)).expect("save");
    Mock::given(method("POST"))
        .and(path("/token"))
        .respond_with(ResponseTemplate::new(401).set_body_string("sensitive-refresh-token"))
        .expect(1)
        .mount(&server)
        .await;
    assert!(
        config
            .resolve()
            .await
            .expect_err("revoked")
            .to_string()
            .contains("revoked")
    );
    assert!(
        config
            .resolve()
            .await
            .expect_err("no replay")
            .to_string()
            .contains("login")
    );
}

#[tokio::test]
async fn cancelled_refresh_persists_uncertainty_and_does_not_replay() {
    let home = tempfile::tempdir().expect("tempdir");
    let server = MockServer::start().await;
    let mut config = configuration(home.path(), AuthCredentialsStoreMode::Ephemeral);
    mock_endpoints(&mut config, &server);
    config.save(&credential(1)).expect("save");
    Mock::given(method("POST"))
        .and(path("/token"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_delay(Duration::from_millis(500))
                .set_body_json(serde_json::json!({
                    "access_token": "new-access", "refresh_token": "rotated", "expires_in": 3600
                })),
        )
        .expect(1)
        .mount(&server)
        .await;
    let task = tokio::spawn({
        let config = config.clone();
        async move { config.resolve().await }
    });
    tokio::time::timeout(Duration::from_secs(2), async {
        while server
            .received_requests()
            .await
            .expect("requests")
            .is_empty()
        {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("refresh request started");
    task.abort();
    assert!(task.await.expect_err("cancelled").is_cancelled());
    let error = config
        .resolve()
        .await
        .expect_err("uncertain refresh needs login");
    assert!(error.to_string().contains("cannot safely be replayed"));
}

#[tokio::test]
async fn refresh_waits_for_os_credential_lock_before_sending() {
    let home = tempfile::tempdir().expect("tempdir");
    let server = MockServer::start().await;
    let mut config = configuration(home.path(), AuthCredentialsStoreMode::Ephemeral);
    mock_endpoints(&mut config, &server);
    config.save(&credential(1)).expect("save");
    Mock::given(method("POST"))
        .and(path("/token"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "access_token": "new-access", "refresh_token": "rotated", "expires_in": 3600
        })))
        .expect(1)
        .mount(&server)
        .await;
    let lock = config.open_lock_file().expect("external lock");
    lock.lock().expect("lock");
    let mut task = tokio::spawn({
        let config = config.clone();
        async move { config.resolve().await }
    });
    assert!(
        tokio::time::timeout(Duration::from_millis(25), &mut task)
            .await
            .is_err()
    );
    assert!(
        server
            .received_requests()
            .await
            .expect("requests")
            .is_empty()
    );
    drop(lock);
    assert!(task.await.expect("task").expect("refresh").is_some());
}

#[tokio::test]
async fn account_switch_on_refresh_invalidates_rotated_credentials() {
    let home = tempfile::tempdir().expect("tempdir");
    let server = MockServer::start().await;
    let mut config = configuration(home.path(), AuthCredentialsStoreMode::Ephemeral);
    mock_endpoints(&mut config, &server);
    config.save(&credential(1)).expect("save");
    Mock::given(method("POST"))
        .and(path("/token"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "access_token": "new-access", "refresh_token": "rotated", "expires_in": 3600
        })))
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/profile"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "account": { "uuid": "cccccccc-cccc-4ccc-8ccc-cccccccccccc" }
        })))
        .expect(1)
        .mount(&server)
        .await;
    assert!(
        config
            .resolve()
            .await
            .expect_err("switched account")
            .to_string()
            .contains("different account")
    );
    assert!(
        config
            .resolve()
            .await
            .expect_err("no use of switched account")
            .to_string()
            .contains("login")
    );
}

#[test]
fn retry_after_backoff_clamps_seconds_dates_and_milliseconds() {
    for (header, value, expected) in [
        ("retry-after", "1", 5),
        ("retry-after", "999999", 300),
        ("retry-after-ms", "12001", 13),
        ("retry-after", "invalid", 5),
    ] {
        let mut headers = http::HeaderMap::new();
        headers.insert(
            http::HeaderName::from_bytes(header.as_bytes()).expect("header"),
            http::HeaderValue::from_str(value).expect("value"),
        );
        assert_eq!(refresh_backoff(&headers), expected);
    }
    let date = chrono::DateTime::from_timestamp((now() + 90) as i64, 0)
        .expect("date")
        .format("%a, %d %b %Y %H:%M:%S GMT")
        .to_string();
    let mut headers = http::HeaderMap::new();
    headers.insert(
        "retry-after",
        http::HeaderValue::from_str(&date).expect("date header"),
    );
    assert!((89..=90).contains(&refresh_backoff(&headers)));
}

#[tokio::test]
async fn cancelled_callback_receiver_releases_listener_without_waiting_for_timeout() {
    let server = tiny_http::Server::http("127.0.0.1:0").expect("loopback listener");
    let address = server.server_addr().to_ip().expect("IP listener");
    let cancelled = Arc::new(AtomicBool::new(false));
    let receiver = ClaudeOAuthCallbackServer {
        server: Some(server),
        expected_state: "expected-state".into(),
        cancelled: Arc::clone(&cancelled),
    };
    let task = tokio::spawn(receiver.wait_for_callback());
    tokio::time::timeout(Duration::from_secs(2), async {
        while Arc::strong_count(&cancelled) < 3 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("blocking receiver started");
    task.abort();
    assert!(task.await.expect_err("cancelled").is_cancelled());
    tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            if let Ok(listener) = std::net::TcpListener::bind(address) {
                drop(listener);
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("cancelled receiver promptly releases callback port");
}
