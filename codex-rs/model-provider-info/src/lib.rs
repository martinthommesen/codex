//! Registry of model providers supported by Codex.
//!
//! Providers can be defined in two places:
//!   1. Built-in defaults compiled into the binary so Codex works out-of-the-box.
//!   2. User-defined entries inside `~/.codex/config.toml` under the `model_providers`
//!      key. These override or extend the defaults at runtime.
//!
//! API provider construction applies the process-wide managed residency policy also
//! used by default HTTP headers.

use codex_client::Provider as ApiProvider;
use codex_client::RetryConfig as ApiRetryConfig;
use codex_protocol::auth::AuthMode;
use codex_protocol::config_types::ModelProviderAuthInfo;
use codex_protocol::error::CodexErr;
use codex_protocol::error::EnvVarError;
use codex_protocol::error::Result as CodexResult;
use codex_utils_redacted_string::RedactedString;
use http::HeaderMap;
use http::header::HeaderName;
use http::header::HeaderValue;
use schemars::JsonSchema;
use serde::Deserialize;
use serde::Serialize;
use std::collections::HashMap;
use std::fmt;
use std::num::NonZeroU64;
use std::path::Component;
use std::path::Path;
use std::sync::PoisonError;
use std::sync::RwLock;
use std::time::Duration;

mod capabilities;
mod gateway_oauth;
pub use capabilities::ModelProviderCapabilities;
pub use capabilities::RemoteCompactionSupport;
pub use gateway_oauth::GatewayOAuthConfig;
pub use gateway_oauth::GatewayOAuthDelivery;

pub const RESIDENCY_HEADER_NAME: &str = "x-openai-internal-codex-residency";

#[derive(Deserialize, Debug, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum ResidencyRequirement {
    Us,
}

static REQUIREMENTS_RESIDENCY: RwLock<Option<ResidencyRequirement>> = RwLock::new(None);

/// Sets the process-wide residency requirement loaded from managed configuration.
pub fn set_managed_residency_requirement(enforce_residency: Option<ResidencyRequirement>) {
    // Recover the stored policy if the lock is poisoned rather than silently disabling it.
    *REQUIREMENTS_RESIDENCY
        .write()
        .unwrap_or_else(PoisonError::into_inner) = enforce_residency;
}

/// Returns the current process-wide managed residency requirement.
pub fn read_managed_residency_requirement() -> Option<ResidencyRequirement> {
    *REQUIREMENTS_RESIDENCY
        .read()
        .unwrap_or_else(PoisonError::into_inner)
}

const DEFAULT_STREAM_IDLE_TIMEOUT_MS: u64 = 300_000;
const DEFAULT_STREAM_MAX_RETRIES: u64 = 5;
const DEFAULT_REQUEST_MAX_RETRIES: u64 = 4;
const DEFAULT_AWS_CREDENTIAL_EXPORT_TIMEOUT_MS: u64 = 30_000;
const DEFAULT_AWS_AUTH_REFRESH_TIMEOUT_MS: u64 = 300_000;
pub const DEFAULT_WEBSOCKET_CONNECT_TIMEOUT_MS: u64 = 15_000;
/// Hard cap for user-configured `stream_max_retries`.
const MAX_STREAM_MAX_RETRIES: u64 = 100;
/// Hard cap for user-configured `request_max_retries`.
const MAX_REQUEST_MAX_RETRIES: u64 = 100;

const OPENAI_PROVIDER_NAME: &str = "OpenAI";
const OPENAI_ACTOR_AUTHORIZATION_HEADER: &str = "x-openai-actor-authorization";
pub const OPENAI_PROVIDER_ID: &str = "openai";
pub const CHATGPT_CODEX_BASE_URL: &str = "https://chatgpt.com/backend-api/codex";
const AMAZON_BEDROCK_PROVIDER_NAME: &str = "Amazon Bedrock";
pub const AMAZON_BEDROCK_PROVIDER_ID: &str = "amazon-bedrock";
const AMAZON_BEDROCK_RUNTIME_PROVIDER_NAME: &str = "Amazon Bedrock Runtime";
pub const AMAZON_BEDROCK_RUNTIME_PROVIDER_ID: &str = "amazon-bedrock-runtime";
pub const AMAZON_BEDROCK_GPT_5_5_MODEL_ID: &str = "openai.gpt-5.5";
pub const AMAZON_BEDROCK_GPT_5_6_SOL_MODEL_ID: &str = "openai.gpt-5.6-sol";
pub const AMAZON_BEDROCK_GPT_6_1_SOL_MODEL_ID: &str = "openai.gpt-6.1-sol";
pub const AMAZON_BEDROCK_GPT_6_SOL_MODEL_ID: &str = "openai.gpt-6-sol";
pub const AMAZON_BEDROCK_GPT_6_LUNA_MODEL_ID: &str = "openai.gpt-6-luna";
pub const AMAZON_BEDROCK_GPT_6_ASTRA_MODEL_ID: &str = "openai.gpt-6-astra";
pub const AMAZON_BEDROCK_GPT_5_6_TERRA_MODEL_ID: &str = "openai.gpt-5.6-terra";
pub const AMAZON_BEDROCK_GPT_5_6_LUNA_MODEL_ID: &str = "openai.gpt-5.6-luna";
pub const AMAZON_BEDROCK_RUNTIME_GLOBAL_GPT_5_6_TERRA_MODEL_ID: &str =
    "global.openai.gpt-5.6-terra";
pub const AMAZON_BEDROCK_RUNTIME_GLOBAL_GPT_5_6_LUNA_MODEL_ID: &str = "global.openai.gpt-5.6-luna";
pub const AMAZON_BEDROCK_DEFAULT_BASE_URL: &str =
    "https://bedrock-mantle.us-east-1.api.aws/openai/v1";
const AMAZON_BEDROCK_MANTLE_CLIENT_AGENT_HEADER: &str = "x-amzn-mantle-client-agent";
const AMAZON_BEDROCK_MANTLE_CLIENT_AGENT_VALUE: &str = "codex";
const CHAT_WIRE_API_REMOVED_ERROR: &str = "`wire_api = \"chat\"` is no longer supported.\nHow to fix: set `wire_api = \"responses\"` in your provider config.\nMore info: https://github.com/openai/codex/discussions/7782";
const ANTHROPIC_PROVIDER_NAME: &str = "Anthropic";
pub const ANTHROPIC_PROVIDER_ID: &str = "anthropic";
pub const ANTHROPIC_DEFAULT_BASE_URL: &str = "https://api.anthropic.com/v1";
/// Environment variable used as a fallback when no Anthropic API key is stored by Codex.
pub const ANTHROPIC_API_KEY_ENV_VAR: &str = "ANTHROPIC_API_KEY";
const CLAUDE_CODE_PROVIDER_NAME: &str = "Claude Code";
pub const CLAUDE_CODE_PROVIDER_ID: &str = "claude-code";
const CLAUDE_OAUTH_PROVIDER_NAME: &str = "Claude OAuth (experimental)";
pub const CLAUDE_OAUTH_PROVIDER_ID: &str = "claude-oauth";
pub const LEGACY_OLLAMA_CHAT_PROVIDER_ID: &str = "ollama-chat";
pub const OLLAMA_CHAT_PROVIDER_REMOVED_ERROR: &str = "`ollama-chat` is no longer supported.\nHow to fix: replace `ollama-chat` with `ollama` in `model_provider`, `oss_provider`, or `--local-provider`.\nMore info: https://github.com/openai/codex/discussions/7782";

/// Wire protocol that the provider speaks.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum WireApi {
    /// The Responses API exposed by OpenAI at `/v1/responses`.
    #[default]
    Responses,
    /// The Anthropic Messages API at `/v1/messages`, authenticated with an Anthropic API key.
    #[serde(rename = "anthropic_messages")]
    AnthropicMessages,
    /// The locally installed, unmodified Claude Code CLI (`claude`), which uses its own sign-in
    /// (for example a Claude subscription). Codex never reads Claude Code credentials.
    #[serde(rename = "claude_code")]
    ClaudeCode,
    /// Experimental direct Anthropic Messages transport using separately stored Claude OAuth
    /// credentials. Undocumented compatibility does not establish Anthropic authorization or
    /// subscription-quota treatment.
    #[serde(rename = "claude_oauth")]
    ClaudeOAuth,
}

const WIRE_API_VARIANTS: &[&str] = &[
    "responses",
    "anthropic_messages",
    "claude_code",
    "claude_oauth",
];

impl WireApi {
    /// Returns whether this protocol targets Claude models.
    pub fn is_claude(self) -> bool {
        matches!(
            self,
            Self::AnthropicMessages | Self::ClaudeCode | Self::ClaudeOAuth
        )
    }
}

impl fmt::Display for WireApi {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let value = match self {
            Self::Responses => "responses",
            Self::AnthropicMessages => "anthropic_messages",
            Self::ClaudeCode => "claude_code",
            Self::ClaudeOAuth => "claude_oauth",
        };
        f.write_str(value)
    }
}

impl<'de> Deserialize<'de> for WireApi {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let value = String::deserialize(deserializer)?;
        match value.as_str() {
            "responses" => Ok(Self::Responses),
            "anthropic_messages" => Ok(Self::AnthropicMessages),
            "claude_code" => Ok(Self::ClaudeCode),
            "claude_oauth" => Ok(Self::ClaudeOAuth),
            "chat" => Err(serde::de::Error::custom(CHAT_WIRE_API_REMOVED_ERROR)),
            _ => Err(serde::de::Error::unknown_variant(&value, WIRE_API_VARIANTS)),
        }
    }
}

/// Serializable representation of a provider definition.
#[derive(Debug, Clone, Default, Deserialize, Serialize, PartialEq, JsonSchema)]
#[schemars(deny_unknown_fields)]
pub struct ModelProviderInfo {
    /// Friendly display name.
    #[serde(default)]
    pub name: String,
    /// Base URL for the provider's OpenAI-compatible API.
    pub base_url: Option<String>,
    /// Optional full URL for a Codex-native model catalog. When unset, OpenAI discovery
    /// uses the Codex backend unless `base_url` overrides the inference endpoint.
    pub model_catalog_url: Option<RedactedString>,
    /// Environment variable that stores the user's API key for this provider.
    pub env_key: Option<String>,

    /// Optional instructions to help the user get a valid value for the
    /// variable and set it.
    pub env_key_instructions: Option<String>,
    /// Value to use with `Authorization: Bearer <token>` header. Use of this
    /// config is discouraged in favor of `env_key` for security reasons, but
    /// this may be necessary when using this programmatically.
    pub experimental_bearer_token: Option<RedactedString>,
    /// Command-backed bearer-token configuration for this provider.
    pub auth: Option<ModelProviderAuthInfo>,
    /// Secondary OAuth credentials required by the provider's gateway.
    pub gateway_oauth: Option<GatewayOAuthConfig>,
    /// AWS SigV4 auth configuration for this provider.
    pub aws: Option<ModelProviderAwsAuthInfo>,
    /// Which wire protocol this provider expects.
    #[serde(default)]
    pub wire_api: WireApi,
    /// Optional API capability overrides for a custom Responses-compatible provider.
    /// Unspecified capabilities retain their existing provider defaults.
    pub capabilities: Option<ModelProviderCapabilities>,
    /// Optional query parameters to append to the base URL.
    pub query_params: Option<HashMap<String, RedactedString>>,
    /// Additional HTTP headers to include in requests to this provider where
    /// the (key, value) pairs are the header name and value.
    pub http_headers: Option<HashMap<String, RedactedString>>,
    /// Optional HTTP headers to include in requests to this provider where the
    /// (key, value) pairs are the header name and _environment variable_ whose
    /// value should be used. If the environment variable is not set, or the
    /// value is empty, the header will not be included in the request.
    pub env_http_headers: Option<HashMap<String, String>>,
    /// Maximum number of times to retry a failed HTTP request to this provider.
    pub request_max_retries: Option<u64>,
    /// Number of times to retry reconnecting a dropped streaming response before failing.
    pub stream_max_retries: Option<u64>,
    /// Idle timeout (in milliseconds) to wait for activity on a streaming response before treating
    /// the connection as lost.
    pub stream_idle_timeout_ms: Option<u64>,
    /// Maximum time (in milliseconds) to wait for a websocket connection attempt before treating
    /// it as failed.
    pub websocket_connect_timeout_ms: Option<u64>,
    /// Does this provider require an OpenAI API key or ChatGPT login token? If true,
    /// the user is presented with a login screen on first run, and credentials are
    /// stored using the backend selected by `cli_auth_credentials_store`.
    /// If false (the default), the login screen is skipped, and the API key
    /// (if needed) comes from the environment variable specified by `env_key`.
    #[serde(default)]
    pub requires_openai_auth: bool,
    /// Whether this provider supports the Responses API WebSocket transport.
    #[serde(default)]
    pub supports_websockets: bool,
    /// Whether this provider supports the standalone web-search endpoint.
    #[serde(default)]
    pub supports_standalone_web_search: bool,
    /// Runtime-only opt-in for internal metadata, independent of the destination check.
    /// This cannot be loaded from or written to serialized provider configuration.
    #[serde(skip)]
    #[schemars(skip)]
    pub include_internal_metadata: bool,
}

/// AWS SigV4 auth configuration for a model provider.
#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq, JsonSchema)]
#[schemars(deny_unknown_fields)]
pub struct ModelProviderAwsAuthInfo {
    /// AWS profile name to use. When unset, the AWS SDK default chain decides.
    pub profile: Option<String>,
    /// AWS region to use for provider-specific endpoints.
    pub region: Option<String>,
    /// Optional command whose exported credentials replace the AWS SDK credential chain.
    pub credential_export: Option<AwsCredentialExportConfig>,
    /// Optional command used to reauthenticate after a refreshable AWS auth failure.
    pub auth_refresh: Option<AwsAuthRefreshConfig>,
}

/// Command used to export AWS signing credentials for a model provider.
#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq, JsonSchema)]
#[schemars(deny_unknown_fields)]
pub struct AwsCredentialExportConfig {
    /// Executable to invoke directly, without a shell.
    pub command: String,
    /// Arguments passed to the credential export command.
    #[serde(default)]
    pub args: Vec<RedactedString>,
    /// Maximum time to wait for the credential export command to complete.
    #[serde(default = "default_aws_credential_export_timeout_ms")]
    pub timeout_ms: NonZeroU64,
}

impl AwsCredentialExportConfig {
    pub fn timeout(&self) -> Duration {
        Duration::from_millis(self.timeout_ms.get())
    }
}

fn default_aws_credential_export_timeout_ms() -> NonZeroU64 {
    match NonZeroU64::new(DEFAULT_AWS_CREDENTIAL_EXPORT_TIMEOUT_MS) {
        Some(timeout_ms) => timeout_ms,
        None => panic!("AWS credential export timeout must be non-zero"),
    }
}

/// Command used to refresh AWS credentials for a model provider.
#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq, JsonSchema)]
#[schemars(deny_unknown_fields)]
pub struct AwsAuthRefreshConfig {
    /// Executable to invoke directly, without a shell.
    pub command: String,
    /// Arguments passed to the refresh command.
    #[serde(default)]
    pub args: Vec<RedactedString>,
    /// Maximum time to wait for the refresh command to complete.
    #[serde(default = "default_aws_auth_refresh_timeout_ms")]
    pub timeout_ms: NonZeroU64,
}

impl AwsAuthRefreshConfig {
    pub fn timeout(&self) -> Duration {
        Duration::from_millis(self.timeout_ms.get())
    }
}

fn default_aws_auth_refresh_timeout_ms() -> NonZeroU64 {
    match NonZeroU64::new(DEFAULT_AWS_AUTH_REFRESH_TIMEOUT_MS) {
        Some(timeout_ms) => timeout_ms,
        None => panic!("AWS auth refresh timeout must be non-zero"),
    }
}

impl ModelProviderInfo {
    /// Checks that a configured Bedrock entry only customizes supported fields.
    /// Call this on the override before merging it with the built-in provider.
    pub fn validate_bedrock_override(&self) -> Result<(), String> {
        let unsupported_fields = Self {
            base_url: None,
            auth: None,
            aws: None,
            http_headers: None,
            ..self.clone()
        };
        if unsupported_fields != Self::default() {
            return Err("only supports changing \
`base_url`, `auth`, `http_headers`, `aws.profile`, `aws.region`, `aws.credential_export`, \
and `aws.auth_refresh`; \
other non-default provider fields are not supported"
                .to_string());
        }
        Ok(())
    }

    /// Checks that a configured Anthropic or Claude Code entry only customizes supported fields.
    /// Call this on the override before merging it with the built-in provider.
    pub fn validate_claude_override(&self) -> Result<(), String> {
        let unsupported_fields = Self {
            // The built-in name and wire protocol always win; restating them is harmless.
            name: String::new(),
            wire_api: WireApi::default(),
            base_url: None,
            env_key: None,
            env_key_instructions: None,
            http_headers: None,
            request_max_retries: None,
            stream_max_retries: None,
            stream_idle_timeout_ms: None,
            ..self.clone()
        };
        if unsupported_fields != Self::default() {
            return Err(
                "only supports changing `base_url`, `env_key`, `env_key_instructions`, \
`http_headers`, `request_max_retries`, `stream_max_retries`, and `stream_idle_timeout_ms`; \
other non-default provider fields are not supported"
                    .to_string(),
            );
        }
        Ok(())
    }

    /// Validates a configured `anthropic` or `claude-code` entry, which partially overrides the
    /// built-in provider of the same id. Returns `Ok(())` for any other id.
    ///
    /// Before these ids became built-in, `[model_providers.anthropic]` was an ordinary custom
    /// provider, and custom providers always carry a `name`. A `name` (or `wire_api`) that
    /// differs from the built-in therefore identifies such a pre-existing provider; it is
    /// rejected instead of being silently converted to the built-in wire protocol.
    pub fn validate_claude_builtin_override(&self, key: &str) -> Result<(), String> {
        let (built_in_name, built_in_wire_api) = match key {
            ANTHROPIC_PROVIDER_ID => (ANTHROPIC_PROVIDER_NAME, WireApi::AnthropicMessages),
            CLAUDE_CODE_PROVIDER_ID => (CLAUDE_CODE_PROVIDER_NAME, WireApi::ClaudeCode),
            _ => return Ok(()),
        };
        let name = self.name.trim();
        let redefines_wire_api =
            self.wire_api != WireApi::default() && self.wire_api != built_in_wire_api;
        if (!name.is_empty() && name != built_in_name) || redefines_wire_api {
            return Err(format!(
                "is the built-in `{built_in_name}` provider and cannot be redefined with a \
different `name` or `wire_api`; rename your custom provider (for example `{key}-custom`), or \
remove `name` and `wire_api` to override only the supported fields of the built-in"
            ));
        }
        self.validate_claude_override()?;
        if key == CLAUDE_CODE_PROVIDER_ID && (self.base_url.is_some() || self.env_key.is_some()) {
            return Err(
                "does not support `base_url` or `env_key`; Claude Code uses its own sign-in"
                    .to_string(),
            );
        }
        Ok(())
    }

    /// OAuth identity and credentials are fixed to Anthropic's HTTPS endpoint. Only retry and
    /// timeout tuning may override the explicitly selected experimental built-in provider.
    pub fn validate_claude_oauth_override(&self) -> Result<(), String> {
        let unsupported_fields = Self {
            name: String::new(),
            wire_api: WireApi::default(),
            request_max_retries: None,
            stream_max_retries: None,
            stream_idle_timeout_ms: None,
            ..self.clone()
        };
        if unsupported_fields != Self::default() {
            return Err(
                "only supports changing `request_max_retries`, `stream_max_retries`, and \
`stream_idle_timeout_ms`; Claude OAuth credentials and endpoint cannot be overridden"
                    .to_string(),
            );
        }
        Ok(())
    }

    pub fn validate(&self) -> std::result::Result<(), String> {
        if let Some(gateway) = &self.gateway_oauth {
            gateway.validate(self)?;
        }
        if self.wire_api.is_claude() {
            let mut conflicts = Vec::new();
            if self.requires_openai_auth {
                conflicts.push("requires_openai_auth");
            }
            if self.aws.is_some() {
                conflicts.push("aws");
            }
            if self.gateway_oauth.is_some() {
                conflicts.push("gateway_oauth");
            }
            if self.supports_websockets {
                conflicts.push("supports_websockets");
            }
            if matches!(self.wire_api, WireApi::ClaudeCode | WireApi::ClaudeOAuth) {
                if self.base_url.is_some()
                    && (self.wire_api == WireApi::ClaudeCode
                        || self.base_url.as_deref() != Some(ANTHROPIC_DEFAULT_BASE_URL))
                {
                    conflicts.push("base_url");
                }
                if self.env_key.is_some() {
                    conflicts.push("env_key");
                }
                if self.experimental_bearer_token.is_some() {
                    conflicts.push("experimental_bearer_token");
                }
                if self.auth.is_some() {
                    conflicts.push("auth");
                }
            }
            if self.wire_api == WireApi::ClaudeOAuth {
                if self.query_params.is_some() {
                    conflicts.push("query_params");
                }
                if self.http_headers.is_some() {
                    conflicts.push("http_headers");
                }
                if self.env_http_headers.is_some() {
                    conflicts.push("env_http_headers");
                }
                if self.model_catalog_url.is_some() {
                    conflicts.push("model_catalog_url");
                }
            }
            if !conflicts.is_empty() {
                return Err(format!(
                    "wire_api = \"{}\" cannot be combined with {}",
                    self.wire_api,
                    conflicts.join(", ")
                ));
            }
        }
        if let Some(aws) = self.aws.as_ref() {
            if self.supports_websockets {
                // TODO(celia-oai): Support AWS SigV4 signing for WebSocket
                // upgrade requests before allowing AWS-authenticated providers
                // to enable Responses-over-WebSocket.
                return Err("provider aws cannot be combined with supports_websockets".to_string());
            }

            let mut conflicts = Vec::new();
            if self.env_key.is_some() {
                conflicts.push("env_key");
            }
            if self.experimental_bearer_token.is_some() {
                conflicts.push("experimental_bearer_token");
            }
            if self.auth.is_some() {
                conflicts.push("auth");
            }
            if self.requires_openai_auth {
                conflicts.push("requires_openai_auth");
            }

            if !conflicts.is_empty() {
                return Err(format!(
                    "provider aws cannot be combined with {}",
                    conflicts.join(", ")
                ));
            }

            if let Some(credential_export) = aws.credential_export.as_ref() {
                if aws.profile.is_some() {
                    return Err(
                        "provider aws.credential_export cannot be combined with aws.profile"
                            .to_string(),
                    );
                }
                if credential_export.command.trim().is_empty() {
                    return Err(
                        "provider aws.credential_export.command must not be empty".to_string()
                    );
                }
                let command = Path::new(&credential_export.command);
                let mut components = command.components();
                let is_bare_command = matches!(
                    (components.next(), components.next()),
                    (Some(Component::Normal(name)), None) if name == command.as_os_str()
                );
                if !command.is_absolute() && !is_bare_command {
                    return Err(
                        "provider aws.credential_export.command must be an absolute path or a bare executable name"
                            .to_string(),
                    );
                }
            }

            if let Some(auth_refresh) = aws.auth_refresh.as_ref() {
                if auth_refresh.command.trim().is_empty() {
                    return Err("provider aws.auth_refresh.command must not be empty".to_string());
                }
                if auth_refresh.command != "aws" {
                    return Err("provider aws.auth_refresh.command must be `aws`".to_string());
                }
            }
        }

        let Some(auth) = self.auth.as_ref() else {
            return Ok(());
        };

        if auth.command.trim().is_empty() {
            return Err("provider auth.command must not be empty".to_string());
        }

        let mut conflicts = Vec::new();
        if self.env_key.is_some() {
            conflicts.push("env_key");
        }
        if self.experimental_bearer_token.is_some() {
            conflicts.push("experimental_bearer_token");
        }
        if self.requires_openai_auth {
            conflicts.push("requires_openai_auth");
        }

        if conflicts.is_empty() {
            Ok(())
        } else {
            Err(format!(
                "provider auth cannot be combined with {}",
                conflicts.join(", ")
            ))
        }
    }

    fn build_header_map(&self) -> CodexResult<HeaderMap> {
        let capacity = self.http_headers.as_ref().map_or(0, HashMap::len)
            + self.env_http_headers.as_ref().map_or(0, HashMap::len);
        let mut headers = HeaderMap::with_capacity(capacity);
        if let Some(extra) = &self.http_headers {
            for (k, v) in extra {
                if let (Ok(name), Ok(value)) =
                    (HeaderName::try_from(k), HeaderValue::try_from(v.as_str()))
                {
                    headers.insert(name, value);
                }
            }
        }

        if let Some(env_headers) = &self.env_http_headers {
            for (header, env_var) in env_headers {
                if let Ok(val) = std::env::var(env_var)
                    && !val.trim().is_empty()
                    && let (Ok(name), Ok(value)) =
                        (HeaderName::try_from(header), HeaderValue::try_from(val))
                {
                    headers.insert(name, value);
                }
            }
        }

        Ok(headers)
    }

    /// Builds an API provider with managed residency taking precedence over configured headers.
    pub fn to_api_provider(&self, auth_mode: Option<AuthMode>) -> CodexResult<ApiProvider> {
        // An Anthropic credential must never fall back to an OpenAI origin.
        let default_base_url = if self.is_anthropic() || self.is_claude_oauth() {
            ANTHROPIC_DEFAULT_BASE_URL
        } else if matches!(
            auth_mode,
            Some(
                AuthMode::Chatgpt
                    | AuthMode::ChatgptAuthTokens
                    | AuthMode::Headers
                    | AuthMode::AgentIdentity
                    | AuthMode::PersonalAccessToken
            )
        ) {
            CHATGPT_CODEX_BASE_URL
        } else {
            "https://api.openai.com/v1"
        };
        let base_url = self
            .base_url
            .clone()
            .unwrap_or_else(|| default_base_url.to_string());

        let mut headers = self.build_header_map()?;
        if let Some(requirement) = read_managed_residency_requirement() {
            let value = match requirement {
                ResidencyRequirement::Us => HeaderValue::from_static("us"),
            };
            headers.insert(RESIDENCY_HEADER_NAME, value);
        }
        let retry = ApiRetryConfig {
            max_attempts: self.request_max_retries(),
            base_delay: Duration::from_millis(200),
            retry_429: false,
            retry_5xx: true,
            retry_transport: true,
        };

        Ok(ApiProvider {
            name: self.name.clone(),
            base_url,
            query_params: self.query_params.clone().map(|params| {
                params
                    .into_iter()
                    .map(|(name, value)| (name, value.into_inner()))
                    .collect()
            }),
            headers,
            retry,
            stream_idle_timeout: self.stream_idle_timeout(),
        })
    }

    /// If `env_key` is Some, returns the API key for this provider if present
    /// (and non-empty) in the environment. If `env_key` is required but
    /// cannot be found, returns an error.
    pub fn api_key(&self) -> CodexResult<Option<String>> {
        match &self.env_key {
            Some(env_key) => {
                let api_key = std::env::var(env_key)
                    .ok()
                    .filter(|v| !v.trim().is_empty())
                    .ok_or_else(|| {
                        CodexErr::EnvVar(EnvVarError {
                            var: env_key.clone(),
                            instructions: self.env_key_instructions.clone(),
                        })
                    })?;
                Ok(Some(api_key))
            }
            None => Ok(None),
        }
    }

    /// Effective maximum number of request retries for this provider.
    pub fn request_max_retries(&self) -> u64 {
        self.request_max_retries
            .unwrap_or(DEFAULT_REQUEST_MAX_RETRIES)
            .min(MAX_REQUEST_MAX_RETRIES)
    }

    /// Effective maximum number of stream reconnection attempts for this provider.
    pub fn stream_max_retries(&self) -> u64 {
        self.stream_max_retries
            .unwrap_or(DEFAULT_STREAM_MAX_RETRIES)
            .min(MAX_STREAM_MAX_RETRIES)
    }

    /// Effective idle timeout for streaming responses.
    pub fn stream_idle_timeout(&self) -> Duration {
        self.stream_idle_timeout_ms
            .map(Duration::from_millis)
            .unwrap_or(Duration::from_millis(DEFAULT_STREAM_IDLE_TIMEOUT_MS))
    }

    /// Effective timeout for websocket connect attempts.
    pub fn websocket_connect_timeout(&self) -> Duration {
        self.websocket_connect_timeout_ms
            .map(Duration::from_millis)
            .unwrap_or(Duration::from_millis(DEFAULT_WEBSOCKET_CONNECT_TIMEOUT_MS))
    }

    pub fn create_openai_provider(base_url: Option<String>) -> ModelProviderInfo {
        ModelProviderInfo {
            name: OPENAI_PROVIDER_NAME.into(),
            base_url,
            model_catalog_url: None,
            env_key: None,
            env_key_instructions: None,
            experimental_bearer_token: None,
            auth: None,
            gateway_oauth: None,
            aws: None,
            wire_api: WireApi::Responses,
            query_params: None,
            http_headers: Some(
                [("version".to_string(), env!("CARGO_PKG_VERSION").into())]
                    .into_iter()
                    .collect(),
            ),
            env_http_headers: Some(
                [
                    (
                        "OpenAI-Organization".to_string(),
                        "OPENAI_ORGANIZATION".to_string(),
                    ),
                    ("OpenAI-Project".to_string(), "OPENAI_PROJECT".to_string()),
                ]
                .into_iter()
                .collect(),
            ),
            // Use global defaults for retry/timeout unless overridden in config.toml.
            request_max_retries: None,
            stream_max_retries: None,
            stream_idle_timeout_ms: None,
            websocket_connect_timeout_ms: None,
            requires_openai_auth: true,
            supports_websockets: true,
            supports_standalone_web_search: true,
            capabilities: None,
            include_internal_metadata: true,
        }
    }

    /// Built-in provider for the Anthropic Messages API (API-key access).
    pub fn create_anthropic_provider() -> ModelProviderInfo {
        ModelProviderInfo {
            name: ANTHROPIC_PROVIDER_NAME.into(),
            base_url: Some(ANTHROPIC_DEFAULT_BASE_URL.to_string()),
            model_catalog_url: None,
            // Keys come from the isolated Codex Anthropic credential store, falling back to
            // ANTHROPIC_API_KEY. Setting env_key here would make a stored key alone insufficient.
            env_key: None,
            env_key_instructions: None,
            experimental_bearer_token: None,
            auth: None,
            gateway_oauth: None,
            aws: None,
            wire_api: WireApi::AnthropicMessages,
            capabilities: None,
            query_params: None,
            http_headers: None,
            env_http_headers: None,
            request_max_retries: None,
            stream_max_retries: None,
            stream_idle_timeout_ms: None,
            websocket_connect_timeout_ms: None,
            requires_openai_auth: false,
            supports_websockets: false,
            supports_standalone_web_search: false,
            include_internal_metadata: false,
        }
    }

    /// Built-in provider that drives the official Claude Code CLI with its own sign-in.
    pub fn create_claude_code_provider() -> ModelProviderInfo {
        ModelProviderInfo {
            name: CLAUDE_CODE_PROVIDER_NAME.into(),
            base_url: None,
            model_catalog_url: None,
            env_key: None,
            env_key_instructions: None,
            experimental_bearer_token: None,
            auth: None,
            gateway_oauth: None,
            aws: None,
            wire_api: WireApi::ClaudeCode,
            capabilities: None,
            query_params: None,
            http_headers: None,
            env_http_headers: None,
            request_max_retries: None,
            stream_max_retries: None,
            stream_idle_timeout_ms: None,
            websocket_connect_timeout_ms: None,
            requires_openai_auth: false,
            supports_websockets: false,
            supports_standalone_web_search: false,
            include_internal_metadata: false,
        }
    }

    /// Explicitly selected experimental direct Claude OAuth transport. It never uses an API key
    /// or the Claude Code subprocess and never accepts endpoint or credential overrides.
    pub fn create_claude_oauth_provider() -> ModelProviderInfo {
        ModelProviderInfo {
            name: CLAUDE_OAUTH_PROVIDER_NAME.into(),
            wire_api: WireApi::ClaudeOAuth,
            ..Self::create_anthropic_provider()
        }
    }

    pub fn is_claude_oauth(&self) -> bool {
        self.wire_api == WireApi::ClaudeOAuth
    }

    /// Whether this provider speaks the Anthropic Messages API.
    pub fn is_anthropic(&self) -> bool {
        self.wire_api == WireApi::AnthropicMessages
    }

    /// Whether this provider drives the Claude Code CLI.
    pub fn is_claude_code(&self) -> bool {
        self.wire_api == WireApi::ClaudeCode
    }

    pub fn create_amazon_bedrock_provider(
        aws: Option<ModelProviderAwsAuthInfo>,
    ) -> ModelProviderInfo {
        ModelProviderInfo {
            name: AMAZON_BEDROCK_PROVIDER_NAME.into(),
            // The runtime provider derives the regional Mantle endpoint when
            // this is unset. A configured value is therefore unambiguously an
            // endpoint override.
            base_url: None,
            model_catalog_url: None,
            env_key: None,
            env_key_instructions: None,
            experimental_bearer_token: None,
            auth: None,
            gateway_oauth: None,
            aws: Some(aws.unwrap_or(ModelProviderAwsAuthInfo {
                profile: None,
                region: None,
                credential_export: None,
                auth_refresh: None,
            })),
            wire_api: WireApi::Responses,
            query_params: None,
            http_headers: Some(HashMap::from([(
                AMAZON_BEDROCK_MANTLE_CLIENT_AGENT_HEADER.to_string(),
                AMAZON_BEDROCK_MANTLE_CLIENT_AGENT_VALUE.into(),
            )])),
            env_http_headers: None,
            request_max_retries: None,
            stream_max_retries: None,
            stream_idle_timeout_ms: None,
            websocket_connect_timeout_ms: None,
            requires_openai_auth: false,
            supports_websockets: false,
            supports_standalone_web_search: false,
            capabilities: None,
            include_internal_metadata: false,
        }
    }

    pub fn create_amazon_bedrock_runtime_provider(
        aws: Option<ModelProviderAwsAuthInfo>,
    ) -> ModelProviderInfo {
        let mut provider = Self::create_amazon_bedrock_provider(aws);
        provider.name = AMAZON_BEDROCK_RUNTIME_PROVIDER_NAME.into();
        provider
    }

    pub fn is_openai(&self) -> bool {
        self.name == OPENAI_PROVIDER_NAME
    }

    pub fn supports_codex_backend_routes(&self) -> bool {
        self.is_openai()
            && self.base_url.as_deref().is_none_or(|base_url| {
                base_url
                    .trim_end_matches('/')
                    .ends_with("/backend-api/codex")
            })
    }

    pub fn uses_openai_actor_authorization(&self) -> bool {
        !self.requires_openai_auth
            && self.http_headers.as_ref().is_some_and(|headers| {
                headers.iter().any(|(name, value)| {
                    name.eq_ignore_ascii_case(OPENAI_ACTOR_AUTHORIZATION_HEADER)
                        && !value.trim().is_empty()
                })
            })
    }

    pub fn is_amazon_bedrock(&self) -> bool {
        self.name == AMAZON_BEDROCK_PROVIDER_NAME
            || self.name == AMAZON_BEDROCK_RUNTIME_PROVIDER_NAME
    }

    pub fn is_amazon_bedrock_runtime(&self) -> bool {
        self.name == AMAZON_BEDROCK_RUNTIME_PROVIDER_NAME
    }

    pub fn has_command_auth(&self) -> bool {
        self.auth.is_some()
    }
}

pub const DEFAULT_LMSTUDIO_PORT: u16 = 1234;
pub const DEFAULT_OLLAMA_PORT: u16 = 11434;

pub const LMSTUDIO_OSS_PROVIDER_ID: &str = "lmstudio";
pub const OLLAMA_OSS_PROVIDER_ID: &str = "ollama";

/// Built-in default provider list.
pub fn built_in_model_providers(
    openai_base_url: Option<String>,
) -> HashMap<String, ModelProviderInfo> {
    use ModelProviderInfo as P;
    let openai_provider = P::create_openai_provider(openai_base_url);
    let amazon_bedrock_provider = P::create_amazon_bedrock_provider(/*aws*/ None);
    let amazon_bedrock_runtime_provider =
        P::create_amazon_bedrock_runtime_provider(/*aws*/ None);
    let anthropic_provider = P::create_anthropic_provider();
    let claude_code_provider = P::create_claude_code_provider();
    let claude_oauth_provider = P::create_claude_oauth_provider();

    // Provider availability does not activate authentication. Claude OAuth remains an
    // explicit experimental selection and never inherits another provider's credentials.
    [
        (OPENAI_PROVIDER_ID, openai_provider),
        (AMAZON_BEDROCK_PROVIDER_ID, amazon_bedrock_provider),
        (
            AMAZON_BEDROCK_RUNTIME_PROVIDER_ID,
            amazon_bedrock_runtime_provider,
        ),
        (ANTHROPIC_PROVIDER_ID, anthropic_provider),
        (CLAUDE_CODE_PROVIDER_ID, claude_code_provider),
        (CLAUDE_OAUTH_PROVIDER_ID, claude_oauth_provider),
        (
            OLLAMA_OSS_PROVIDER_ID,
            create_oss_provider(DEFAULT_OLLAMA_PORT, WireApi::Responses),
        ),
        (
            LMSTUDIO_OSS_PROVIDER_ID,
            create_oss_provider(DEFAULT_LMSTUDIO_PORT, WireApi::Responses),
        ),
    ]
    .into_iter()
    .map(|(k, v)| (k.to_string(), v))
    .collect()
}

/// Merge configured providers into the built-in provider catalog.
///
/// Configured providers extend the built-in set. Built-in providers are not
/// generally overridable. Claude OAuth permits retry/timeout tuning only; the built-in
/// Anthropic and Claude Code providers permit their existing endpoint, key-variable, header,
/// and retry tuning. Built-in Amazon
/// Bedrock providers allow the user to customize their endpoint,
/// authentication, headers, and AWS settings.
pub fn merge_configured_model_providers(
    mut model_providers: HashMap<String, ModelProviderInfo>,
    configured_model_providers: HashMap<String, ModelProviderInfo>,
) -> Result<HashMap<String, ModelProviderInfo>, String> {
    for (key, mut provider) in configured_model_providers {
        if matches!(
            key.as_str(),
            AMAZON_BEDROCK_PROVIDER_ID | AMAZON_BEDROCK_RUNTIME_PROVIDER_ID
        ) {
            provider
                .validate_bedrock_override()
                .map_err(|message| format!("model_providers.{key} {message}"))?;
            let base_url_override = provider.base_url.take();
            let auth_override = provider.auth.take();
            let aws_override = provider.aws.take();
            let http_headers_override = provider.http_headers.take();
            if let Some(built_in_provider) = model_providers.get_mut(&key) {
                built_in_provider.base_url = base_url_override;
                built_in_provider.auth = auth_override;
                if let Some(aws_override) = aws_override {
                    built_in_provider.aws = Some(aws_override);
                }
                if let Some(http_headers_override) = http_headers_override {
                    built_in_provider
                        .http_headers
                        .get_or_insert_default()
                        .extend(http_headers_override);
                }
            }
        } else if key == CLAUDE_OAUTH_PROVIDER_ID {
            provider
                .validate_claude_oauth_override()
                .map_err(|message| format!("model_providers.{key} {message}"))?;
            if let Some(built_in_provider) = model_providers.get_mut(&key) {
                built_in_provider.request_max_retries = provider.request_max_retries;
                built_in_provider.stream_max_retries = provider.stream_max_retries;
                built_in_provider.stream_idle_timeout_ms = provider.stream_idle_timeout_ms;
            }
        } else if matches!(
            key.as_str(),
            ANTHROPIC_PROVIDER_ID | CLAUDE_CODE_PROVIDER_ID
        ) {
            provider
                .validate_claude_builtin_override(&key)
                .map_err(|message| format!("model_providers.{key} {message}"))?;
            if let Some(built_in_provider) = model_providers.get_mut(&key) {
                if let Some(base_url) = provider.base_url.take() {
                    built_in_provider.base_url = Some(base_url);
                }
                if let Some(env_key) = provider.env_key.take() {
                    built_in_provider.env_key = Some(env_key);
                }
                if let Some(instructions) = provider.env_key_instructions.take() {
                    built_in_provider.env_key_instructions = Some(instructions);
                }
                if let Some(http_headers) = provider.http_headers.take() {
                    built_in_provider
                        .http_headers
                        .get_or_insert_default()
                        .extend(http_headers);
                }
                built_in_provider.request_max_retries = provider
                    .request_max_retries
                    .or(built_in_provider.request_max_retries);
                built_in_provider.stream_max_retries = provider
                    .stream_max_retries
                    .or(built_in_provider.stream_max_retries);
                built_in_provider.stream_idle_timeout_ms = provider
                    .stream_idle_timeout_ms
                    .or(built_in_provider.stream_idle_timeout_ms);
            }
        } else {
            model_providers.entry(key).or_insert(provider);
        }
    }

    Ok(model_providers)
}

pub fn create_oss_provider(default_provider_port: u16, wire_api: WireApi) -> ModelProviderInfo {
    // These CODEX_OSS_ environment variables are experimental: we may
    // switch to reading values from config.toml instead.
    let default_codex_oss_base_url = format!(
        "http://localhost:{codex_oss_port}/v1",
        codex_oss_port = std::env::var("CODEX_OSS_PORT")
            .ok()
            .filter(|value| !value.trim().is_empty())
            .and_then(|value| value.parse::<u16>().ok())
            .unwrap_or(default_provider_port)
    );

    let codex_oss_base_url = std::env::var("CODEX_OSS_BASE_URL")
        .ok()
        .filter(|v| !v.trim().is_empty())
        .unwrap_or(default_codex_oss_base_url);
    create_oss_provider_with_base_url(&codex_oss_base_url, wire_api)
}

pub fn create_oss_provider_with_base_url(base_url: &str, wire_api: WireApi) -> ModelProviderInfo {
    ModelProviderInfo {
        name: "gpt-oss".into(),
        base_url: Some(base_url.into()),
        model_catalog_url: None,
        env_key: None,
        env_key_instructions: None,
        experimental_bearer_token: None,
        auth: None,
        gateway_oauth: None,
        aws: None,
        wire_api,
        query_params: None,
        http_headers: None,
        env_http_headers: None,
        request_max_retries: None,
        stream_max_retries: None,
        stream_idle_timeout_ms: None,
        websocket_connect_timeout_ms: None,
        requires_openai_auth: false,
        supports_websockets: false,
        supports_standalone_web_search: false,
        capabilities: None,
        include_internal_metadata: false,
    }
}

#[cfg(test)]
#[path = "model_provider_info_tests.rs"]
mod tests;
