//! Experimental Claude subscription OAuth, isolated from every other provider.
//!
//! Protocol constants and grant shapes were inspected in CLIProxyAPI (MIT), commit
//! 2422c6b0f5fe16495320fa29456493874c1977a0. See codex-api/src/anthropic/oauth/LICENSE.CLIProxyAPI.
//! Anthropic does not publish this as a third-party subscription authentication API.
//! No credential files or keychain entries belonging to Claude Code are accessed.

use std::collections::HashMap;
use std::fmt;
use std::fs::File;
use std::fs::OpenOptions;
use std::fs::TryLockError;
use std::io;
#[cfg(unix)]
use std::os::unix::fs::DirBuilderExt;
#[cfg(unix)]
use std::os::unix::fs::OpenOptionsExt;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::LazyLock;
use std::sync::Mutex;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering;
use std::time::Duration;
use std::time::SystemTime;
use std::time::UNIX_EPOCH;

use base64::Engine as _;
use codex_config::types::AuthCredentialsStoreMode;
use codex_config::types::AuthKeyringBackendKind;
use codex_keyring_store::DefaultKeyringStore;
use codex_keyring_store::KeyringStore;
use codex_secrets::LocalSecretsNamespace;
use codex_secrets::SecretName;
use codex_secrets::SecretScope;
use codex_secrets::SecretsBackendKind;
use codex_secrets::SecretsManager;
use rand::TryRngCore as _;
use rand::rngs::OsRng;
use serde::Deserialize;
use serde::Serialize;
use sha2::Digest as _;
use sha2::Sha256;
use url::Url;

use super::storage::codex_home_store_hash;

const AUTHORIZE_URL: &str = "https://claude.ai/oauth/authorize";
const TOKEN_URL: &str = "https://platform.claude.com/v1/oauth/token";
const PROFILE_URL: &str = "https://api.anthropic.com/api/oauth/profile";
const ROLES_URL: &str = "https://api.anthropic.com/api/oauth/claude_cli/roles";
const CLIENT_ID: &str = "9d1c250a-e61b-44d9-88ed-5944d1962f5e";
const REDIRECT_URI: &str = "http://localhost:54545/callback";
const SCOPE: &str =
    "user:profile user:inference user:sessions:claude_code user:mcp_servers user:file_upload";
const KEYRING_SERVICE: &str = "Codex Claude OAuth";
const REFRESH_LEAD_SECONDS: u64 = 60;
const MAX_RESPONSE_BYTES: usize = 1024 * 1024;

static EPHEMERAL: LazyLock<Mutex<HashMap<String, ClaudeOAuthCredential>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));
static REFRESH_LOCKS: LazyLock<Mutex<HashMap<String, Arc<tokio::sync::Mutex<()>>>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

/// Tokens deliberately have no derived Debug implementation.
#[derive(Clone, Serialize, Deserialize)]
pub struct ClaudeOAuthCredential {
    version: u32,
    access_token: String,
    refresh_token: String,
    expires_at: u64,
    account_uuid: String,
    organization_uuid: String,
    email: String,
    device_id: String,
    #[serde(default)]
    refresh_blocked_until: u64,
    /// A lost refresh response may have consumed the rotating refresh token. Never replay it.
    #[serde(default)]
    refresh_requires_login: bool,
}

impl ClaudeOAuthCredential {
    pub fn access_token(&self) -> &str {
        &self.access_token
    }
    pub fn account_uuid(&self) -> &str {
        &self.account_uuid
    }
    pub fn organization_uuid(&self) -> &str {
        &self.organization_uuid
    }
    pub fn email(&self) -> &str {
        &self.email
    }
    pub fn device_id(&self) -> &str {
        &self.device_id
    }
    pub fn expires_at(&self) -> u64 {
        self.expires_at
    }
}

impl fmt::Debug for ClaudeOAuthCredential {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ClaudeOAuthCredential")
            .field("tokens", &"[REDACTED]")
            .field("expires_at", &self.expires_at)
            .finish_non_exhaustive()
    }
}

/// A single-use authorization attempt. State, verifier and callback codes are never logged.
pub struct ClaudeOAuthLogin {
    state: String,
    verifier: String,
    authorization_url: Url,
}

impl ClaudeOAuthLogin {
    pub fn new() -> io::Result<Self> {
        let verifier =
            base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(random_bytes::<96>()?);
        let challenge = base64::engine::general_purpose::URL_SAFE_NO_PAD
            .encode(Sha256::digest(verifier.as_bytes()));
        let state = hex_bytes(&random_bytes::<16>()?);
        let mut authorization_url = Url::parse(AUTHORIZE_URL)
            .map_err(|_| io::Error::other("invalid built-in Claude OAuth authorization URL"))?;
        authorization_url.query_pairs_mut().extend_pairs([
            ("code", "true"),
            ("client_id", CLIENT_ID),
            ("response_type", "code"),
            ("redirect_uri", REDIRECT_URI),
            ("scope", SCOPE),
            ("code_challenge", challenge.as_str()),
            ("code_challenge_method", "S256"),
            ("state", state.as_str()),
        ]);
        Ok(Self {
            state,
            verifier,
            authorization_url,
        })
    }

    pub fn authorization_url(&self) -> &Url {
        &self.authorization_url
    }

    /// Binds IPv4 loopback only, using the exact registered redirect port/path.
    /// A caller can instead collect a manually pasted callback URL or `code#state`.
    pub fn start_callback_server(&self) -> io::Result<ClaudeOAuthCallbackServer> {
        let server = tiny_http::Server::http("127.0.0.1:54545")
            .map_err(|_| io::Error::other("could not bind Claude OAuth callback port 54545; use manual completion or close the listener"))?;
        Ok(ClaudeOAuthCallbackServer {
            server: Some(server),
            expected_state: self.state.clone(),
            cancelled: Arc::new(AtomicBool::new(false)),
        })
    }

    fn validated_code(&self, input: &str) -> io::Result<String> {
        parse_callback(input, &self.state)
    }
}

/// Optional loopback callback receiver. Debug deliberately omits login state.
pub struct ClaudeOAuthCallbackServer {
    server: Option<tiny_http::Server>,
    expected_state: String,
    cancelled: Arc<AtomicBool>,
}

impl Drop for ClaudeOAuthCallbackServer {
    fn drop(&mut self) {
        self.cancelled.store(true, Ordering::Release);
    }
}

impl ClaudeOAuthCallbackServer {
    pub async fn wait_for_callback(mut self) -> io::Result<String> {
        let server = self
            .server
            .take()
            .ok_or_else(|| io::Error::other("Claude OAuth callback listener is unavailable"))?;
        let expected_state = self.expected_state.clone();
        let cancelled = Arc::clone(&self.cancelled);
        tokio::task::spawn_blocking(move || {
            let deadline = std::time::Instant::now() + Duration::from_secs(300);
            while std::time::Instant::now() < deadline && !cancelled.load(Ordering::Acquire) {
                let Some(request) = server.recv_timeout(Duration::from_millis(100))? else {
                    continue;
                };
                let callback = format!("http://localhost:54545{}", request.url());
                let validation = parse_callback(&callback, &expected_state);
                let state_validated = validation.is_ok()
                    || validation
                        .as_ref()
                        .err()
                        .is_some_and(|error| error.kind() == io::ErrorKind::PermissionDenied);
                let valid = request.method() == &tiny_http::Method::Get
                    && request.url().split('?').next() == Some("/callback")
                    && state_validated;
                let (status, message) = if valid {
                    (
                        200,
                        "Claude OAuth callback received. You may return to Codex.",
                    )
                } else {
                    (
                        400,
                        "Invalid OAuth callback. Return to the authorization started by Codex.",
                    )
                };
                let _ = request
                    .respond(tiny_http::Response::from_string(message).with_status_code(status));
                if valid {
                    return Ok(callback);
                }
            }
            Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "Claude OAuth authorization timed out",
            ))
        })
        .await
        .map_err(|_| io::Error::other("Claude OAuth callback receiver failed"))?
    }
}

/// Dedicated storage. File/Auto select an age-encrypted file with an OS-keyring key;
/// there is intentionally no fallback to plaintext when a keyring is unavailable.
#[derive(Clone)]
pub struct ClaudeOAuthAuthConfig {
    pub codex_home: PathBuf,
    pub store_mode: AuthCredentialsStoreMode,
    pub keyring_backend_kind: AuthKeyringBackendKind,
    keyring: Arc<dyn KeyringStore>,
    endpoints: Endpoints,
}

impl fmt::Debug for ClaudeOAuthAuthConfig {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ClaudeOAuthAuthConfig")
            .field("codex_home", &self.codex_home)
            .field("store_mode", &self.store_mode)
            .field("keyring_backend_kind", &self.keyring_backend_kind)
            .finish_non_exhaustive()
    }
}

#[derive(Clone)]
struct Endpoints {
    token: String,
    profile: String,
    roles: String,
}

impl ClaudeOAuthAuthConfig {
    pub fn new(
        codex_home: PathBuf,
        store_mode: AuthCredentialsStoreMode,
        keyring_backend_kind: AuthKeyringBackendKind,
    ) -> Self {
        Self {
            codex_home,
            store_mode,
            keyring_backend_kind,
            keyring: Arc::new(DefaultKeyringStore),
            endpoints: Endpoints {
                token: TOKEN_URL.into(),
                profile: PROFILE_URL.into(),
                roles: ROLES_URL.into(),
            },
        }
    }

    fn account(&self) -> String {
        format!("claude-oauth|{}", codex_home_store_hash(&self.codex_home))
    }

    fn encrypted_store(&self) -> SecretsManager {
        SecretsManager::new_with_keyring_store_and_namespace(
            self.codex_home.clone(),
            SecretsBackendKind::Local,
            Arc::clone(&self.keyring),
            LocalSecretsNamespace::ClaudeOAuth,
        )
    }

    fn direct_keyring(&self) -> bool {
        self.store_mode == AuthCredentialsStoreMode::Keyring
            && self.keyring_backend_kind == AuthKeyringBackendKind::Direct
    }

    pub fn load_stored_credential(&self) -> io::Result<Option<ClaudeOAuthCredential>> {
        if self.store_mode == AuthCredentialsStoreMode::Ephemeral {
            return Ok(EPHEMERAL
                .lock()
                .map_err(|_| io::Error::other("Claude OAuth ephemeral storage is unavailable"))?
                .get(&self.account())
                .cloned());
        }
        let serialized = if self.direct_keyring() {
            self.keyring
                .load(KEYRING_SERVICE, &self.account())
                .map_err(|_| storage_error())?
        } else {
            self.encrypted_store()
                .get(&SecretScope::Global, &secret_name()?)
                .map_err(|_| storage_error())?
        };
        serialized
            .map(|value| decode_credential(&value))
            .transpose()
    }

    fn save(&self, credential: &ClaudeOAuthCredential) -> io::Result<()> {
        if self.store_mode == AuthCredentialsStoreMode::Ephemeral {
            EPHEMERAL
                .lock()
                .map_err(|_| storage_error())?
                .insert(self.account(), credential.clone());
            return Ok(());
        }
        let serialized = serde_json::to_string(credential).map_err(|_| storage_error())?;
        if self.direct_keyring() {
            self.keyring
                .save(KEYRING_SERVICE, &self.account(), &serialized)
                .map_err(|_| storage_error())
        } else {
            self.encrypted_store()
                .set(&SecretScope::Global, &secret_name()?, &serialized)
                .map_err(|_| storage_error())
        }
    }

    /// Removes all Claude OAuth entries, including a store used before a backend change.
    /// This is local logout; the reference defines no verified token-revocation endpoint.
    pub fn delete(&self) -> io::Result<bool> {
        let removed = EPHEMERAL
            .lock()
            .map_err(|_| storage_error())?
            .remove(&self.account())
            .is_some();
        if self.store_mode == AuthCredentialsStoreMode::Ephemeral {
            return Ok(removed);
        }
        // Serialize with refresh/login in other processes before deleting rotated credentials.
        let _guard = self.try_lock_for_logout()?;
        // This file belongs exclusively to this provider. Removing its ciphertext
        // also works when the keyring is locked or its key has been lost.
        let encrypted = match std::fs::remove_file(self.codex_home.join("secrets/claude_oauth.age"))
        {
            Ok(()) => Ok(true),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(false),
            Err(_) => Err(storage_error()),
        };
        let direct = if self.direct_keyring()
            || matches!(
                self.keyring.load(KEYRING_SERVICE, &self.account()),
                Ok(Some(_))
            ) {
            self.keyring
                .delete(KEYRING_SERVICE, &self.account())
                .map_err(|_| storage_error())
        } else {
            Ok(false)
        };
        Ok(encrypted? || direct? || removed)
    }

    pub async fn complete_login(
        &self,
        login: ClaudeOAuthLogin,
        callback: &str,
    ) -> io::Result<ClaudeOAuthCredential> {
        let code = login.validated_code(callback)?;
        let _guard = self.lock_credentials().await?;
        let client = control_client()?;
        let response = client
            .post(&self.endpoints.token)
            .json(&CodeGrant {
                grant_type: "authorization_code",
                code: &code,
                redirect_uri: REDIRECT_URI,
                client_id: CLIENT_ID,
                code_verifier: &login.verifier,
                state: &login.state,
            })
            .send()
            .await
            .map_err(|_| {
                io::Error::other("Claude OAuth token exchange connection failed; restart login")
            })?;
        let token: TokenResponse = decode_success(response, "token exchange").await?;
        validate_token(&token, true)?;
        let expires_at = now().saturating_add(token.expires_in);
        let profile = self.fetch_profile(&client, &token.access_token).await?;
        // The observed roles response has no established quota/entitlement contract.
        // Preserve the companion lookup without guessing a schema or requiring its success.
        let _ = client
            .get(&self.endpoints.roles)
            .bearer_auth(&token.access_token)
            .header("Cache-Control", "no-cache")
            .send()
            .await;
        let credential = ClaudeOAuthCredential {
            version: 1,
            access_token: token.access_token,
            refresh_token: token.refresh_token,
            expires_at,
            account_uuid: profile.account.uuid,
            organization_uuid: profile.organization.uuid,
            email: profile.account.email,
            device_id: hex_bytes(&random_bytes::<32>()?),
            refresh_blocked_until: 0,
            refresh_requires_login: false,
        };
        self.save(&credential)?;
        Ok(credential)
    }

    /// Returns only this provider's credential. No environment key, CLI, or account fallback.
    pub async fn resolve(&self) -> io::Result<Option<ClaudeOAuthCredential>> {
        let Some(credential) = self.load_stored_credential()? else {
            return Ok(None);
        };
        if !credential.refresh_requires_login
            && credential.expires_at > now().saturating_add(REFRESH_LEAD_SECONDS)
        {
            return Ok(Some(credential));
        }
        let refresh_lock = {
            let mut locks = REFRESH_LOCKS.lock().map_err(|_| storage_error())?;
            Arc::clone(
                locks
                    .entry(self.account())
                    .or_insert_with(|| Arc::new(tokio::sync::Mutex::new(()))),
            )
        };
        let _local_guard = refresh_lock.lock_owned().await;
        let _file_guard = self.lock_credentials().await?;
        // Reload after both locks: another Codex process may have rotated the refresh token.
        let Some(mut credential) = self.load_stored_credential()? else {
            return Ok(None);
        };
        if !credential.refresh_requires_login
            && credential.expires_at > now().saturating_add(REFRESH_LEAD_SECONDS)
        {
            return Ok(Some(credential));
        }
        if credential.refresh_requires_login {
            return Err(io::Error::other(
                "Claude OAuth refresh cannot safely be replayed; run codex login --provider claude-oauth",
            ));
        }
        if credential.refresh_blocked_until > now() {
            return Err(io::Error::other(format!(
                "Claude OAuth refresh is rate limited until Unix time {}; no alternate billing provider was used",
                credential.refresh_blocked_until
            )));
        }
        // Persist before sending: cancellation, a crash, or a lost successful response must
        // not cause a second process to replay a potentially consumed refresh token.
        credential.refresh_requires_login = true;
        self.save(&credential)?;
        let client = control_client()?;
        for attempt in 0..3 {
            let response = client.post(&self.endpoints.token).json(&RefreshGrant {
                client_id: CLIENT_ID,
                grant_type: "refresh_token",
                refresh_token: &credential.refresh_token,
                scope: SCOPE,
            }).send().await.map_err(|_| io::Error::other("Claude OAuth refresh response was lost; restart login to avoid replaying a rotating refresh token"))?;
            let status = response.status();
            if status == reqwest::StatusCode::TOO_MANY_REQUESTS {
                credential.refresh_requires_login = false;
                credential.refresh_blocked_until =
                    now().saturating_add(refresh_backoff(response.headers()));
                self.save(&credential)?;
                return Err(io::Error::other(
                    "Claude OAuth refresh is rate limited; respect Retry-After and retry later",
                ));
            }
            if status.is_server_error() && attempt < 2 {
                tokio::time::sleep(Duration::from_secs(attempt + 1)).await;
                continue;
            }
            if !status.is_success() {
                if status.is_server_error() {
                    credential.refresh_requires_login = false;
                    credential.refresh_blocked_until = now().saturating_add(5);
                    self.save(&credential)?;
                }
                return Err(status_error(status, "refresh"));
            }
            let token: TokenResponse = decode_success(response, "refresh").await?;
            validate_token(&token, false)?;
            credential.access_token = token.access_token;
            if !token.refresh_token.is_empty() {
                credential.refresh_token = token.refresh_token;
            }
            credential.expires_at = now().saturating_add(token.expires_in);
            credential.refresh_blocked_until = 0;
            // Save rotation BEFORE advisory account lookup, including cancellation/failure.
            self.save(&credential)?;
            if let Ok(profile) = self.fetch_profile(&client, &credential.access_token).await
                && profile.account.uuid != credential.account_uuid
            {
                credential.refresh_requires_login = true;
                credential.expires_at = 0;
                self.save(&credential)?;
                return Err(io::Error::other(
                    "Claude OAuth refresh returned a different account; login again",
                ));
            }
            credential.refresh_requires_login = false;
            self.save(&credential)?;
            return Ok(Some(credential));
        }
        Err(io::Error::other("Claude OAuth refresh failed"))
    }

    async fn fetch_profile(&self, client: &reqwest::Client, token: &str) -> io::Result<Profile> {
        let response = client
            .get(&self.endpoints.profile)
            .bearer_auth(token)
            .header("Cache-Control", "no-cache")
            .send()
            .await
            .map_err(|_| io::Error::other("Claude OAuth account validation connection failed"))?;
        let profile: Profile = decode_success(response, "account validation").await?;
        if !valid_uuid(&profile.account.uuid)
            || (!profile.organization.uuid.is_empty() && !valid_uuid(&profile.organization.uuid))
        {
            return Err(io::Error::other(
                "Claude OAuth profile did not identify an account",
            ));
        }
        Ok(profile)
    }

    fn open_lock_file(&self) -> io::Result<File> {
        let directory = self.codex_home.join("secrets");
        let mut builder = std::fs::DirBuilder::new();
        builder.recursive(true);
        #[cfg(unix)]
        builder.mode(0o700);
        builder.create(&directory)?;
        let mut options = OpenOptions::new();
        options.read(true).write(true).create(true).truncate(false);
        #[cfg(unix)]
        options.mode(0o600);
        options.open(directory.join("claude_oauth.lock"))
    }

    async fn lock_credentials(&self) -> io::Result<File> {
        let file = self.open_lock_file()?;
        tokio::time::timeout(Duration::from_secs(60), async {
            loop {
                match file.try_lock() {
                    Ok(()) => return Ok(()),
                    Err(TryLockError::WouldBlock) => {
                        tokio::time::sleep(Duration::from_millis(50)).await
                    }
                    Err(error) => return Err(io::Error::from(error)),
                }
            }
        })
        .await
        .map_err(|_| io::Error::other("timed out waiting for Claude OAuth credentials"))??;
        Ok(file)
    }

    fn try_lock_for_logout(&self) -> io::Result<File> {
        let file = self.open_lock_file()?;
        match file.try_lock() {
            Ok(()) => {}
            Err(TryLockError::WouldBlock) => {
                return Err(io::Error::new(
                    io::ErrorKind::WouldBlock,
                    "Claude OAuth credentials are being updated; retry logout shortly",
                ));
            }
            Err(error) => return Err(io::Error::from(error)),
        }
        Ok(file)
    }
}

#[derive(Serialize)]
struct CodeGrant<'a> {
    grant_type: &'a str,
    code: &'a str,
    redirect_uri: &'a str,
    client_id: &'a str,
    code_verifier: &'a str,
    state: &'a str,
}

#[derive(Serialize)]
struct RefreshGrant<'a> {
    client_id: &'a str,
    grant_type: &'a str,
    refresh_token: &'a str,
    scope: &'a str,
}

#[derive(Deserialize)]
struct TokenResponse {
    access_token: String,
    #[serde(default)]
    refresh_token: String,
    #[serde(default)]
    token_type: String,
    expires_in: u64,
}

#[derive(Default, Deserialize)]
struct Account {
    uuid: String,
    #[serde(default)]
    email: String,
}

#[derive(Default, Deserialize)]
struct Organization {
    #[serde(default)]
    uuid: String,
}

#[derive(Deserialize)]
struct Profile {
    account: Account,
    #[serde(default)]
    organization: Organization,
}

fn control_client() -> io::Result<reqwest::Client> {
    // The source emulates native Axios headers. TLS fingerprint and byte-level header
    // ordering are intentionally not emulated by this standard Rust TLS client.
    codex_http_client::build_reqwest_client_with_custom_ca(
        reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .timeout(Duration::from_secs(30))
            .connect_timeout(Duration::from_secs(10))
            .user_agent("axios/1.15.2")
            .default_headers(http::HeaderMap::from_iter([
                (
                    http::header::ACCEPT,
                    http::HeaderValue::from_static("application/json, text/plain, */*"),
                ),
                (
                    http::header::CONTENT_TYPE,
                    http::HeaderValue::from_static("application/json"),
                ),
                (
                    http::header::CONNECTION,
                    http::HeaderValue::from_static("close"),
                ),
            ])),
    )
    .map_err(|_| {
        io::Error::other(
            "could not build Claude OAuth HTTPS client; check TLS certificate settings",
        )
    })
}

async fn decode_success<T: serde::de::DeserializeOwned>(
    mut response: reqwest::Response,
    operation: &str,
) -> io::Result<T> {
    if !response.status().is_success() {
        return Err(status_error(response.status(), operation));
    }
    let mut bytes = Vec::new();
    while let Some(chunk) = response
        .chunk()
        .await
        .map_err(|_| io::Error::other("Claude OAuth response was interrupted; restart login"))?
    {
        if bytes.len().saturating_add(chunk.len()) > MAX_RESPONSE_BYTES {
            return Err(io::Error::other(
                "Claude OAuth response exceeded the size limit",
            ));
        }
        bytes.extend_from_slice(&chunk);
    }
    // Never expose a serde data error: it can embed secret-bearing response fields.
    serde_json::from_slice(&bytes)
        .map_err(|_| io::Error::other("Claude OAuth response was invalid; restart login"))
}

fn status_error(status: reqwest::StatusCode, operation: &str) -> io::Error {
    let reason = match status.as_u16() {
        400 | 401 => "credentials were rejected or revoked; login again",
        403 => "account, scope, entitlement or third-party client access was denied",
        429 => "rate limit reached; retry after the server's reset time",
        300..=399 => "redirect refused to protect credentials",
        _ => "server request failed",
    };
    io::Error::other(format!(
        "Claude OAuth {operation} failed (HTTP {status}): {reason}"
    ))
}

fn validate_token(token: &TokenResponse, require_refresh: bool) -> io::Result<()> {
    if token.access_token.is_empty()
        || token.expires_in == 0
        || (require_refresh && token.refresh_token.is_empty())
        || (!token.token_type.is_empty() && !token.token_type.eq_ignore_ascii_case("bearer"))
        || token.access_token.chars().any(char::is_whitespace)
        || token.access_token.chars().any(char::is_control)
    {
        return Err(io::Error::other(
            "Claude OAuth response contained invalid token fields",
        ));
    }
    Ok(())
}

fn decode_credential(value: &str) -> io::Result<ClaudeOAuthCredential> {
    let credential: ClaudeOAuthCredential =
        serde_json::from_str(value).map_err(|_| storage_error())?;
    if credential.version != 1
        || credential.access_token.is_empty()
        || credential.refresh_token.is_empty()
        || !valid_uuid(&credential.account_uuid)
        || (!credential.organization_uuid.is_empty() && !valid_uuid(&credential.organization_uuid))
        || credential.access_token.chars().any(char::is_whitespace)
        || credential.access_token.chars().any(char::is_control)
        || credential.device_id.len() != 64
        || !credential
            .device_id
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
    {
        return Err(storage_error());
    }
    Ok(credential)
}

fn secret_name() -> io::Result<SecretName> {
    SecretName::new("CLAUDE_OAUTH").map_err(|_| storage_error())
}

fn valid_uuid(value: &str) -> bool {
    value.len() == 36
        && value.bytes().enumerate().all(|(index, byte)| {
            if [8, 13, 18, 23].contains(&index) {
                byte == b'-'
            } else {
                byte.is_ascii_hexdigit()
            }
        })
}

fn storage_error() -> io::Error {
    io::Error::other(
        "Claude OAuth secure credential storage is unavailable or invalid; configure an OS keyring and login again (plaintext storage is disabled)",
    )
}

fn random_bytes<const N: usize>() -> io::Result<[u8; N]> {
    let mut bytes = [0; N];
    OsRng
        .try_fill_bytes(&mut bytes)
        .map_err(|_| io::Error::other("secure random generation failed"))?;
    Ok(bytes)
}

fn hex_bytes(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |duration| duration.as_secs())
}

fn refresh_backoff(headers: &http::HeaderMap) -> u64 {
    let seconds = headers
        .get("retry-after")
        .and_then(|value| value.to_str().ok())
        .and_then(|value| {
            value
                .parse::<f64>()
                .ok()
                .filter(|seconds| seconds.is_finite())
                .map(|seconds| seconds.ceil().max(0.0) as u64)
                .or_else(|| {
                    chrono::DateTime::parse_from_rfc2822(value)
                        .ok()
                        .map(|date| (date.timestamp().max(0) as u64).saturating_sub(now()))
                })
        })
        .or_else(|| {
            headers
                .get("retry-after-ms")
                .and_then(|value| value.to_str().ok())
                .and_then(|value| value.parse::<u64>().ok())
                .map(|milliseconds| milliseconds.div_ceil(1000))
        })
        .unwrap_or(5);
    seconds.clamp(5, 300)
}

fn parse_callback(input: &str, expected_state: &str) -> io::Result<String> {
    let input = input.trim();
    let (mut code, mut state, error) = if input.contains("://") {
        let url =
            Url::parse(input).map_err(|_| io::Error::other("invalid Claude OAuth callback URL"))?;
        if url.scheme() != "http"
            || url.host_str() != Some("localhost")
            || url.port_or_known_default() != Some(54545)
            || url.path() != "/callback"
            || !url.username().is_empty()
            || url.password().is_some()
        {
            return Err(io::Error::other(
                "Claude OAuth callback must match the registered localhost redirect",
            ));
        }
        let pairs = url.query_pairs().collect::<Vec<_>>();
        for name in ["code", "state", "error"] {
            if pairs.iter().filter(|(key, _)| key == name).count() > 1 {
                return Err(io::Error::other(
                    "duplicate Claude OAuth callback parameters",
                ));
            }
        }
        let field = |name| {
            pairs
                .iter()
                .find(|(key, _)| key == name)
                .map(|(_, value)| value.to_string())
                .unwrap_or_default()
        };
        (field("code"), field("state"), field("error"))
    } else {
        let (code, state) = input.split_once('#').ok_or_else(|| {
            io::Error::other("paste the complete Claude OAuth callback URL or code#state")
        })?;
        (code.to_string(), state.to_string(), String::new())
    };
    if let Some((actual_code, fragment_state)) = code.split_once('#') {
        if !state.is_empty() && state != fragment_state {
            return Err(io::Error::other("Claude OAuth state mismatch"));
        }
        state = fragment_state.to_string();
        code = actual_code.to_string();
    }
    // Check state before even processing provider errors. Comparison always touches every
    // byte of the expected fixed-length cryptographically random state.
    let mismatch = state.len() != expected_state.len()
        || state
            .as_bytes()
            .iter()
            .zip(expected_state.as_bytes())
            .fold(0, |difference, (left, right)| difference | (left ^ right))
            != 0;
    if mismatch {
        return Err(io::Error::other(
            "Claude OAuth state mismatch; restart login",
        ));
    }
    if !error.is_empty() {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "Claude OAuth authorization was denied; restart login",
        ));
    }
    if code.is_empty() || code.chars().any(char::is_control) {
        return Err(io::Error::other(
            "Claude OAuth callback did not contain a valid code",
        ));
    }
    Ok(code)
}

#[cfg(test)]
#[path = "claude_oauth_tests.rs"]
mod tests;
