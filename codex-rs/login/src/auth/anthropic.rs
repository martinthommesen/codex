//! Anthropic API-key storage kept separate from ChatGPT/OpenAI auth.
//!
//! ChatGPT login, refresh, and logout rewrite or delete the whole `auth.json` record and its
//! keyring entry, so the Anthropic key lives in its own file, keyring entry, encrypted secrets
//! namespace, and ephemeral map. Nothing here reads or writes ChatGPT credential storage.

use std::collections::HashMap;
use std::fmt;
use std::fs::OpenOptions;
use std::io;
use std::io::Write;
#[cfg(unix)]
use std::os::unix::fs::OpenOptionsExt;
#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::AtomicU64;
use std::sync::atomic::Ordering;
use std::time::SystemTime;
use std::time::UNIX_EPOCH;

use codex_config::types::AuthCredentialsStoreMode;
use codex_config::types::AuthKeyringBackendKind;
use codex_keyring_store::DefaultKeyringStore;
use codex_keyring_store::KeyringStore;
use codex_secrets::LocalSecretsNamespace;
use codex_secrets::SecretName;
use codex_secrets::SecretScope;
use codex_secrets::SecretsBackendKind;
use codex_secrets::SecretsManager;
use once_cell::sync::Lazy;
use serde::Deserialize;
use serde::Serialize;
use tracing::warn;

use super::storage::codex_home_store_hash;
use super::storage::storage_error;

/// Environment variable consulted when no Anthropic API key is stored by Codex.
pub const ANTHROPIC_API_KEY_ENV_VAR: &str = "ANTHROPIC_API_KEY";

const ANTHROPIC_AUTH_FILE_NAME: &str = "anthropic_auth.json";
const ANTHROPIC_AUTH_FORMAT_VERSION: u32 = 1;
const ANTHROPIC_KEYRING_SERVICE: &str = "Codex Anthropic Auth";
const MASKED_PREFIX_CHARS: usize = 10;
const MASKED_SUFFIX_CHARS: usize = 4;
/// Shorter keys are fully masked so the visible prefix and suffix never reveal most of the key.
const MASKED_MIN_CHARS: usize = 24;

static ANTHROPIC_AUTH_SECRET_NAME: Lazy<SecretName> =
    Lazy::new(|| match SecretName::new("ANTHROPIC_AUTH") {
        Ok(name) => name,
        Err(err) => unreachable!("ANTHROPIC_AUTH should be a valid secret name: {err}"),
    });

/// Process-global map of keyring account -> API key, separate from ChatGPT's ephemeral store.
static EPHEMERAL_ANTHROPIC_AUTH_STORE: Lazy<Mutex<HashMap<String, String>>> =
    Lazy::new(|| Mutex::new(HashMap::new()));

static TEMP_FILE_COUNTER: AtomicU64 = AtomicU64::new(0);

/// Where an Anthropic API key was found.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AnthropicCredentialSource {
    /// Saved by Codex in the configured Anthropic credential store.
    Stored,
    /// Read from the `ANTHROPIC_API_KEY` environment variable.
    Environment,
}

impl AnthropicCredentialSource {
    pub fn description(&self) -> &'static str {
        match self {
            Self::Stored => "stored by Codex",
            Self::Environment => "ANTHROPIC_API_KEY environment variable",
        }
    }
}

/// A resolved Anthropic API key. `Debug` never prints the key.
#[derive(Clone)]
pub struct AnthropicCredential {
    api_key: String,
    source: AnthropicCredentialSource,
}

impl AnthropicCredential {
    pub fn api_key(&self) -> &str {
        &self.api_key
    }

    pub fn source(&self) -> AnthropicCredentialSource {
        self.source
    }

    /// Returns a display-safe form such as `sk-ant-api***WXYZ`, or `***` for short keys.
    pub fn masked(&self) -> String {
        let chars: Vec<char> = self.api_key.chars().collect();
        if chars.len() < MASKED_MIN_CHARS {
            return "***".to_string();
        }
        let prefix: String = chars[..MASKED_PREFIX_CHARS].iter().collect();
        let suffix: String = chars[chars.len() - MASKED_SUFFIX_CHARS..].iter().collect();
        format!("{prefix}***{suffix}")
    }
}

impl fmt::Debug for AnthropicCredential {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("AnthropicCredential")
            .field("api_key", &"[REDACTED]")
            .field("source", &self.source)
            .finish()
    }
}

/// Anthropic API-key storage for one Codex home.
///
/// Uses the same store mode and keyring backend settings as ChatGPT auth, but every backend
/// writes to Anthropic-specific locations:
/// - file: `$CODEX_HOME/anthropic_auth.json`
/// - direct keyring: service `Codex Anthropic Auth`, account `anthropic|<codex home hash>`
/// - secrets keyring: `$CODEX_HOME/secrets/anthropic_auth.age` with its own passphrase entry
/// - ephemeral: a process-global map separate from ChatGPT's
#[derive(Clone)]
pub struct AnthropicAuthConfig {
    pub codex_home: PathBuf,
    pub store_mode: AuthCredentialsStoreMode,
    pub keyring_backend_kind: AuthKeyringBackendKind,
    keyring_store: Arc<dyn KeyringStore>,
}

impl fmt::Debug for AnthropicAuthConfig {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("AnthropicAuthConfig")
            .field("codex_home", &self.codex_home)
            .field("store_mode", &self.store_mode)
            .field("keyring_backend_kind", &self.keyring_backend_kind)
            .finish_non_exhaustive()
    }
}

impl AnthropicAuthConfig {
    pub fn new(
        codex_home: PathBuf,
        store_mode: AuthCredentialsStoreMode,
        keyring_backend_kind: AuthKeyringBackendKind,
    ) -> Self {
        Self {
            codex_home,
            store_mode,
            keyring_backend_kind,
            keyring_store: Arc::new(DefaultKeyringStore),
        }
    }

    #[cfg(test)]
    pub(crate) fn new_with_keyring_store(
        codex_home: PathBuf,
        store_mode: AuthCredentialsStoreMode,
        keyring_backend_kind: AuthKeyringBackendKind,
        keyring_store: Arc<dyn KeyringStore>,
    ) -> Self {
        Self {
            codex_home,
            store_mode,
            keyring_backend_kind,
            keyring_store,
        }
    }

    /// Saves `api_key` with surrounding whitespace removed, replacing any stored key.
    pub fn save_api_key(&self, api_key: &str) -> io::Result<()> {
        let api_key = normalize_api_key(api_key)?;
        match self.store_mode {
            AuthCredentialsStoreMode::File => self.save_file(api_key),
            AuthCredentialsStoreMode::Keyring => self.save_keyring(api_key),
            AuthCredentialsStoreMode::Auto => match self.save_keyring(api_key) {
                Ok(()) => Ok(()),
                Err(err) => {
                    warn!(
                        "failed to save Anthropic credentials to keyring, falling back to file storage: {err}"
                    );
                    self.save_file(api_key)
                }
            },
            AuthCredentialsStoreMode::Ephemeral => self.save_ephemeral(api_key),
        }
    }

    /// Loads the key saved by Codex, ignoring `ANTHROPIC_API_KEY`.
    pub fn load_stored_api_key(&self) -> io::Result<Option<String>> {
        match self.store_mode {
            AuthCredentialsStoreMode::File => self.load_file(),
            AuthCredentialsStoreMode::Keyring => self.load_keyring(),
            AuthCredentialsStoreMode::Auto => match self.load_keyring() {
                Ok(Some(api_key)) => Ok(Some(api_key)),
                Ok(None) => self.load_file(),
                Err(err) => {
                    warn!(
                        "failed to load Anthropic credentials from keyring, falling back to file storage: {err}"
                    );
                    self.load_file()
                }
            },
            AuthCredentialsStoreMode::Ephemeral => self.load_ephemeral(),
        }
    }

    /// Removes the Anthropic key from every Anthropic store the configured mode uses, plus the
    /// in-process ephemeral store. ChatGPT credentials are never touched. Returns `true` when
    /// anything was removed.
    pub fn delete(&self) -> io::Result<bool> {
        let ephemeral_removed = self.delete_ephemeral()?;
        let stored_removed = match self.store_mode {
            AuthCredentialsStoreMode::Ephemeral => false,
            AuthCredentialsStoreMode::File => self.delete_file()?,
            AuthCredentialsStoreMode::Keyring => {
                // Remove a stale plaintext copy even when the keyring reports an error.
                let keyring_result = self.delete_keyring();
                let file_removed = self.delete_file()?;
                keyring_result? || file_removed
            }
            AuthCredentialsStoreMode::Auto => {
                let file_removed = self.delete_file()?;
                let keyring_removed = match self.delete_keyring() {
                    Ok(removed) => removed,
                    // A keyring that cannot return the key cannot keep the provider connected.
                    Err(err) if !matches!(self.load_keyring(), Ok(Some(_))) => {
                        warn!("failed to delete Anthropic credentials from keyring: {err}");
                        false
                    }
                    Err(err) => return Err(err),
                };
                file_removed || keyring_removed
            }
        };
        Ok(ephemeral_removed || stored_removed)
    }

    /// Resolves the key used for direct Anthropic API access: the stored key first, then a
    /// non-empty `ANTHROPIC_API_KEY`. Storage errors are returned rather than skipped so a
    /// broken store never silently switches the credential source.
    pub fn resolve(&self) -> io::Result<Option<AnthropicCredential>> {
        self.resolve_with_env(|name| std::env::var(name).ok())
    }

    /// Same as [`Self::resolve`], reading environment variables through `env`.
    pub fn resolve_with_env(
        &self,
        env: impl Fn(&str) -> Option<String>,
    ) -> io::Result<Option<AnthropicCredential>> {
        if let Some(api_key) = self.load_stored_api_key()? {
            return Ok(Some(AnthropicCredential {
                api_key,
                source: AnthropicCredentialSource::Stored,
            }));
        }
        Ok(env(ANTHROPIC_API_KEY_ENV_VAR)
            .map(|value| value.trim().to_string())
            .filter(|value| !value.is_empty())
            .map(|api_key| AnthropicCredential {
                api_key,
                source: AnthropicCredentialSource::Environment,
            }))
    }

    fn file_path(&self) -> PathBuf {
        self.codex_home.join(ANTHROPIC_AUTH_FILE_NAME)
    }

    fn keyring_account(&self) -> String {
        format!("anthropic|{}", codex_home_store_hash(&self.codex_home))
    }

    fn load_file(&self) -> io::Result<Option<String>> {
        let contents = match std::fs::read_to_string(self.file_path()) {
            Ok(contents) => contents,
            Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(err) => return Err(err),
        };
        parse_payload(&contents, "failed to parse Anthropic credentials file")
    }

    fn save_file(&self, api_key: &str) -> io::Result<()> {
        let serialized = serde_json::to_string_pretty(&AnthropicAuthPayload::new(api_key))
            .map_err(|err| {
                storage_error::with_context("failed to serialize Anthropic credentials", err)
            })?;
        write_file_atomically(&self.file_path(), serialized.as_bytes())
    }

    fn delete_file(&self) -> io::Result<bool> {
        match std::fs::remove_file(self.file_path()) {
            Ok(()) => Ok(true),
            Err(err) if err.kind() == io::ErrorKind::NotFound => Ok(false),
            Err(err) => Err(err),
        }
    }

    fn load_keyring(&self) -> io::Result<Option<String>> {
        match self.keyring_backend_kind {
            AuthKeyringBackendKind::Direct => self.load_direct_keyring(),
            AuthKeyringBackendKind::Secrets => self.load_secrets(),
        }
    }

    fn save_keyring(&self, api_key: &str) -> io::Result<()> {
        let serialized =
            serde_json::to_string(&AnthropicAuthPayload::new(api_key)).map_err(|err| {
                storage_error::with_context("failed to serialize Anthropic credentials", err)
            })?;
        match self.keyring_backend_kind {
            AuthKeyringBackendKind::Direct => self.save_direct_keyring(&serialized)?,
            AuthKeyringBackendKind::Secrets => self.save_secrets(&serialized)?,
        }
        // The keyring now holds the key, so drop any plaintext copy left by an earlier fallback.
        if let Err(err) = self.delete_file() {
            warn!("failed to remove Anthropic credentials fallback file: {err}");
        }
        Ok(())
    }

    fn delete_keyring(&self) -> io::Result<bool> {
        match self.keyring_backend_kind {
            AuthKeyringBackendKind::Direct => self.delete_direct_keyring(),
            AuthKeyringBackendKind::Secrets => {
                // Also clear a direct entry left from before the backend kind changed.
                let secrets_result = self.delete_secrets();
                let direct_removed = self.delete_direct_keyring()?;
                Ok(secrets_result? || direct_removed)
            }
        }
    }

    fn load_direct_keyring(&self) -> io::Result<Option<String>> {
        let serialized = self
            .keyring_store
            .load(ANTHROPIC_KEYRING_SERVICE, &self.keyring_account())
            .map_err(|err| {
                storage_error::with_context(
                    "failed to load Anthropic credentials from keyring",
                    io::Error::from(err),
                )
            })?;
        match serialized {
            Some(serialized) => parse_payload(
                &serialized,
                "failed to parse Anthropic credentials from keyring",
            ),
            None => Ok(None),
        }
    }

    fn save_direct_keyring(&self, serialized: &str) -> io::Result<()> {
        self.keyring_store
            .save(
                ANTHROPIC_KEYRING_SERVICE,
                &self.keyring_account(),
                serialized,
            )
            .map_err(|err| {
                storage_error::with_context(
                    "failed to write Anthropic credentials to keyring",
                    io::Error::from(err),
                )
            })
    }

    fn delete_direct_keyring(&self) -> io::Result<bool> {
        self.keyring_store
            .delete(ANTHROPIC_KEYRING_SERVICE, &self.keyring_account())
            .map_err(|err| {
                storage_error::with_context(
                    "failed to delete Anthropic credentials from keyring",
                    io::Error::from(err),
                )
            })
    }

    fn secrets_manager(&self) -> SecretsManager {
        SecretsManager::new_with_keyring_store_and_namespace(
            self.codex_home.clone(),
            SecretsBackendKind::Local,
            Arc::clone(&self.keyring_store),
            LocalSecretsNamespace::AnthropicAuth,
        )
    }

    fn load_secrets(&self) -> io::Result<Option<String>> {
        let serialized = self
            .secrets_manager()
            .get(&SecretScope::Global, &ANTHROPIC_AUTH_SECRET_NAME)
            .map_err(|err| {
                storage_error::with_context(
                    "failed to load Anthropic credentials from encrypted storage",
                    err,
                )
            })?;
        match serialized {
            Some(serialized) => parse_payload(
                &serialized,
                "failed to parse Anthropic credentials from encrypted storage",
            ),
            None => Ok(None),
        }
    }

    fn save_secrets(&self, serialized: &str) -> io::Result<()> {
        self.secrets_manager()
            .set(
                &SecretScope::Global,
                &ANTHROPIC_AUTH_SECRET_NAME,
                serialized,
            )
            .map_err(|err| {
                storage_error::with_context(
                    "failed to write Anthropic credentials to encrypted storage",
                    err,
                )
            })
    }

    fn delete_secrets(&self) -> io::Result<bool> {
        self.secrets_manager()
            .delete(&SecretScope::Global, &ANTHROPIC_AUTH_SECRET_NAME)
            .map_err(|err| {
                storage_error::with_context(
                    "failed to delete Anthropic credentials from encrypted storage",
                    err,
                )
            })
    }

    fn with_ephemeral_store<T>(
        &self,
        action: impl FnOnce(&mut HashMap<String, String>, String) -> T,
    ) -> io::Result<T> {
        let mut store = EPHEMERAL_ANTHROPIC_AUTH_STORE.lock().map_err(|_| {
            io::Error::other("failed to lock ephemeral Anthropic credential storage")
        })?;
        Ok(action(&mut store, self.keyring_account()))
    }

    fn load_ephemeral(&self) -> io::Result<Option<String>> {
        self.with_ephemeral_store(|store, key| store.get(&key).cloned())
    }

    fn save_ephemeral(&self, api_key: &str) -> io::Result<()> {
        self.with_ephemeral_store(|store, key| {
            store.insert(key, api_key.to_string());
        })
    }

    fn delete_ephemeral(&self) -> io::Result<bool> {
        self.with_ephemeral_store(|store, key| store.remove(&key).is_some())
    }
}

/// On-disk and in-keyring format. Deliberately not `Debug` so it cannot be logged by accident.
#[derive(Serialize, Deserialize)]
struct AnthropicAuthPayload {
    version: u32,
    api_key: String,
}

impl AnthropicAuthPayload {
    fn new(api_key: &str) -> Self {
        Self {
            version: ANTHROPIC_AUTH_FORMAT_VERSION,
            api_key: api_key.to_string(),
        }
    }
}

fn parse_payload(serialized: &str, context: &'static str) -> io::Result<Option<String>> {
    let payload: AnthropicAuthPayload = serde_json::from_str(serialized)
        .map_err(|err| storage_error::with_context(context, err))?;
    if payload.version > ANTHROPIC_AUTH_FORMAT_VERSION {
        return Err(io::Error::other(format!(
            "{context}: unsupported credential format version {}",
            payload.version
        )));
    }
    let api_key = payload.api_key.trim();
    Ok((!api_key.is_empty()).then(|| api_key.to_string()))
}

fn normalize_api_key(api_key: &str) -> io::Result<&str> {
    let api_key = api_key.trim();
    if api_key.is_empty() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "Anthropic API key must not be empty",
        ));
    }
    if api_key
        .chars()
        .any(|ch| ch.is_whitespace() || ch.is_control())
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "Anthropic API key must not contain whitespace or control characters",
        ));
    }
    Ok(api_key)
}

/// Writes through a private temp file in the same directory and renames it into place, so a
/// crash never leaves a truncated credentials file and the key is never world-readable.
fn write_file_atomically(path: &Path, contents: &[u8]) -> io::Result<()> {
    let dir = path
        .parent()
        .ok_or_else(|| io::Error::other("Anthropic credentials path has no parent directory"))?;
    std::fs::create_dir_all(dir)?;
    let file_name = path
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_default();
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |duration| duration.as_nanos());
    let counter = TEMP_FILE_COUNTER.fetch_add(1, Ordering::Relaxed);
    let tmp_path = dir.join(format!(
        ".{file_name}.tmp-{}-{nonce}-{counter}",
        std::process::id()
    ));
    let result =
        write_new_private_file(&tmp_path, contents).and_then(|()| std::fs::rename(&tmp_path, path));
    if result.is_err() {
        let _ = std::fs::remove_file(&tmp_path);
    }
    result
}

fn write_new_private_file(path: &Path, contents: &[u8]) -> io::Result<()> {
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        options.mode(0o600);
    }
    let mut file = options.open(path)?;
    #[cfg(unix)]
    {
        // The creation mode is filtered by the umask; pin the exact permissions explicitly.
        file.set_permissions(std::fs::Permissions::from_mode(0o600))?;
    }
    file.write_all(contents)?;
    file.sync_all()
}

#[cfg(test)]
#[path = "anthropic_tests.rs"]
mod tests;
