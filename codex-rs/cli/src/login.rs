//! CLI login commands and their direct-user observability surfaces.
//!
//! The TUI path already installs a broader tracing stack with feedback, OpenTelemetry, and other
//! interactive-session layers. Direct `codex login` intentionally does less: it preserves the
//! existing stderr/browser UX and adds only a small file-backed tracing layer for login-specific
//! targets. Keeping that setup local avoids pulling the TUI's session-oriented logging machinery
//! into a one-shot CLI command while still producing a durable `codex-login.log` artifact that
//! support can request from users.

use crate::claude_login::LoginProvider;
use codex_config::types::AuthCredentialsStoreMode;
use codex_core::config::Config;
use codex_core::config::edit::ConfigEdit;
use codex_core::config::edit::ConfigEditsBuilder;
use codex_login::AuthKeyringBackendKind;
use codex_login::AuthManager;
use codex_login::AuthRouteConfig;
use codex_login::CLIENT_ID;
use codex_login::ServerOptions;
use codex_login::is_workload_identity_selected;
use codex_login::login_with_access_token;
use codex_login::login_with_api_key;
use codex_login::logout_with_revoke;
use codex_login::run_device_code_login;
use codex_login::run_login_server;
use codex_mcp::ema_auth_scope;
use codex_protocol::auth::AuthMode;
use codex_protocol::config_types::ForcedLoginMethod;
use codex_rmcp_client::EnterpriseOAuthCredentialGuard;
use codex_utils_cli::CliConfigOverrides;
use std::fs::OpenOptions;
use std::io::IsTerminal;
use std::io::Read;
use std::path::Path;
use std::path::PathBuf;
use tracing_appender::non_blocking;
use tracing_appender::non_blocking::WorkerGuard;
use tracing_subscriber::EnvFilter;
use tracing_subscriber::Layer;
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::util::SubscriberInitExt;

const CHATGPT_LOGIN_DISABLED_MESSAGE: &str =
    "ChatGPT login is disabled. Use API key login instead.";
const API_KEY_LOGIN_DISABLED_MESSAGE: &str =
    "API key login is disabled. Use ChatGPT login instead.";
const ACCESS_TOKEN_LOGIN_DISABLED_MESSAGE: &str =
    "Access token login is disabled. Use API key login instead.";
const LOGIN_SUCCESS_MESSAGE: &str = "Successfully logged in";
const DEFAULT_LOGIN_LOG_FILTER: &str =
    "codex_cli=info,codex_core=info,codex_login=info,codex_otel::auth_storage=warn";

/// Installs a small file-backed tracing layer for direct `codex login` flows.
///
/// This deliberately duplicates a narrow slice of the TUI logging setup instead of reusing it
/// wholesale. The TUI stack includes session-oriented layers that are valuable for interactive
/// runs but unnecessary for a one-shot login command. Keeping the direct CLI path local lets this
/// command produce a durable `codex-login.log` artifact without coupling it to the TUI's broader
/// telemetry and feedback initialization.
fn init_login_file_logging(config: &Config) -> Option<WorkerGuard> {
    let log_dir = match codex_core::config::log_dir(config) {
        Ok(log_dir) => log_dir,
        Err(err) => {
            eprintln!("Warning: failed to resolve login log directory: {err}");
            return None;
        }
    };

    if let Err(err) = std::fs::create_dir_all(&log_dir) {
        eprintln!(
            "Warning: failed to create login log directory {}: {err}",
            log_dir.display()
        );
        return None;
    }

    let mut log_file_opts = OpenOptions::new();
    log_file_opts.create(true).append(true);

    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        log_file_opts.mode(0o600);
    }

    let log_path = log_dir.join("codex-login.log");
    let log_file = match log_file_opts.open(&log_path) {
        Ok(log_file) => log_file,
        Err(err) => {
            eprintln!(
                "Warning: failed to open login log file {}: {err}",
                log_path.display()
            );
            return None;
        }
    };

    let (non_blocking, guard) = non_blocking(log_file);
    let env_filter = EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| EnvFilter::new(DEFAULT_LOGIN_LOG_FILTER));
    let file_layer = tracing_subscriber::fmt::layer()
        .with_writer(non_blocking)
        .with_target(true)
        .with_ansi(false)
        .with_filter(env_filter);

    // Direct `codex login` otherwise relies on ephemeral stderr and browser output.
    // Persist the same login targets to a file so support can inspect auth failures
    // without reproducing them through TUI or app-server.
    if let Err(err) = tracing_subscriber::registry().with(file_layer).try_init() {
        eprintln!(
            "Warning: failed to initialize login log file {}: {err}",
            log_path.display()
        );
        return None;
    }

    Some(guard)
}

fn print_login_server_start(actual_port: u16, auth_url: &str) {
    eprintln!(
        "Starting local login server on http://localhost:{actual_port}.\nIf your browser did not open, navigate to this URL to authenticate:\n\n{auth_url}\n\nOn a remote or headless machine? Use `codex login --device-auth` instead."
    );
}

async fn clear_existing_auth_before_login(
    codex_home: &Path,
    auth_credentials_store_mode: AuthCredentialsStoreMode,
    auth_keyring_backend_kind: AuthKeyringBackendKind,
    auth_route_config: &AuthRouteConfig,
) {
    if let Err(err) = logout_with_revoke(
        codex_home,
        auth_credentials_store_mode,
        auth_keyring_backend_kind,
        auth_route_config,
    )
    .await
    {
        tracing::warn!("failed to clear existing auth before login: {err}");
    }
}

pub async fn login_with_chatgpt(
    codex_home: PathBuf,
    forced_chatgpt_workspace_id: Option<Vec<String>>,
    cli_auth_credentials_store_mode: AuthCredentialsStoreMode,
    auth_keyring_backend_kind: AuthKeyringBackendKind,
    auth_route_config: AuthRouteConfig,
) -> std::io::Result<()> {
    clear_existing_auth_before_login(
        &codex_home,
        cli_auth_credentials_store_mode,
        auth_keyring_backend_kind,
        &auth_route_config,
    )
    .await;

    let opts = ServerOptions::new(
        codex_home,
        CLIENT_ID.to_string(),
        forced_chatgpt_workspace_id,
        cli_auth_credentials_store_mode,
        auth_keyring_backend_kind,
        auth_route_config,
    );
    let server = run_login_server(opts)?;

    print_login_server_start(server.actual_port, &server.auth_url);

    server.block_until_done().await
}

pub async fn run_login_with_chatgpt(cli_config_overrides: CliConfigOverrides) -> ! {
    let config = load_config_or_exit(cli_config_overrides).await;
    let _login_log_guard = init_login_file_logging(&config);
    tracing::info!("starting browser login flow");

    if !config
        .auth_config()
        .is_login_method_allowed(ForcedLoginMethod::Chatgpt)
    {
        eprintln!("{CHATGPT_LOGIN_DISABLED_MESSAGE}");
        std::process::exit(1);
    }

    let effective_chatgpt_workspaces = config.auth_config().effective_chatgpt_workspaces();
    match login_with_chatgpt(
        config.codex_home.to_path_buf(),
        effective_chatgpt_workspaces,
        config.cli_auth_credentials_store_mode,
        config.auth_keyring_backend_kind(),
        config.auth_route_config(),
    )
    .await
    {
        Ok(_) => {
            eprintln!("{LOGIN_SUCCESS_MESSAGE}");
            std::process::exit(0);
        }
        Err(e) => {
            eprintln!("Error logging in: {e}");
            std::process::exit(1);
        }
    }
}

pub async fn run_login_with_api_key(
    cli_config_overrides: CliConfigOverrides,
    api_key: String,
) -> ! {
    if api_key.starts_with("sk-ant-") {
        eprintln!(
            "This looks like an Anthropic API key. Store it separately from your OpenAI login with `codex login --provider anthropic --with-api-key`."
        );
        std::process::exit(1);
    }
    let config = load_config_or_exit(cli_config_overrides).await;
    let _login_log_guard = init_login_file_logging(&config);
    tracing::info!("starting api key login flow");

    if !config
        .auth_config()
        .is_login_method_allowed(ForcedLoginMethod::Api)
    {
        eprintln!("{API_KEY_LOGIN_DISABLED_MESSAGE}");
        std::process::exit(1);
    }

    match login_with_api_key(
        &config.codex_home,
        &api_key,
        config.cli_auth_credentials_store_mode,
        config.auth_keyring_backend_kind(),
    ) {
        Ok(_) => {
            eprintln!("{LOGIN_SUCCESS_MESSAGE}");
            std::process::exit(0);
        }
        Err(e) => {
            eprintln!("Error logging in: {e}");
            std::process::exit(1);
        }
    }
}

pub async fn run_login_with_access_token(
    cli_config_overrides: CliConfigOverrides,
    access_token: String,
) -> ! {
    let config = load_config_or_exit(cli_config_overrides).await;
    let _login_log_guard = init_login_file_logging(&config);
    tracing::info!("starting access token login flow");

    if !config
        .auth_config()
        .is_login_method_allowed(ForcedLoginMethod::Chatgpt)
    {
        eprintln!("{ACCESS_TOKEN_LOGIN_DISABLED_MESSAGE}");
        std::process::exit(1);
    }

    let auth_route_config = config.auth_route_config();
    let effective_chatgpt_workspaces = config.auth_config().effective_chatgpt_workspaces();
    match login_with_access_token(
        &config.codex_home,
        &access_token,
        config.cli_auth_credentials_store_mode,
        effective_chatgpt_workspaces.as_deref(),
        Some(&config.chatgpt_base_url),
        config.auth_keyring_backend_kind(),
        &auth_route_config,
    )
    .await
    {
        Ok(_) => {
            eprintln!("{LOGIN_SUCCESS_MESSAGE}");
            std::process::exit(0);
        }
        Err(e) => {
            eprintln!("Error logging in with access token: {e}");
            std::process::exit(1);
        }
    }
}

pub fn read_api_key_from_stdin() -> String {
    read_stdin_secret(
        "--with-api-key expects the API key on stdin. Try piping it, e.g. `printenv OPENAI_API_KEY | codex login --with-api-key`.",
        "Reading API key from stdin...",
        "No API key provided via stdin.",
    )
}

pub fn read_access_token_from_stdin() -> String {
    read_stdin_secret(
        "--with-access-token expects the access token on stdin. Try piping it, e.g. `printenv CODEX_ACCESS_TOKEN | codex login --with-access-token`.",
        "Reading access token from stdin...",
        "No access token provided via stdin.",
    )
}

fn read_stdin_secret(terminal_message: &str, reading_message: &str, empty_message: &str) -> String {
    let mut stdin = std::io::stdin();

    if stdin.is_terminal() {
        eprintln!("{terminal_message}");
        std::process::exit(1);
    }

    eprintln!("{reading_message}");

    let mut buffer = String::new();
    if let Err(err) = stdin.read_to_string(&mut buffer) {
        eprintln!("Failed to read stdin: {err}");
        std::process::exit(1);
    }

    let secret = buffer.trim().to_string();
    if secret.is_empty() {
        eprintln!("{empty_message}");
        std::process::exit(1);
    }

    secret
}

/// Login using the OAuth device code flow.
pub async fn run_login_with_device_code(
    cli_config_overrides: CliConfigOverrides,
    issuer_base_url: Option<String>,
    client_id: Option<String>,
) -> ! {
    let config = load_config_or_exit(cli_config_overrides).await;
    let _login_log_guard = init_login_file_logging(&config);
    tracing::info!("starting device code login flow");
    if !config
        .auth_config()
        .is_login_method_allowed(ForcedLoginMethod::Chatgpt)
    {
        eprintln!("{CHATGPT_LOGIN_DISABLED_MESSAGE}");
        std::process::exit(1);
    }
    let auth_route_config = config.auth_route_config();
    clear_existing_auth_before_login(
        &config.codex_home,
        config.cli_auth_credentials_store_mode,
        config.auth_keyring_backend_kind(),
        &auth_route_config,
    )
    .await;
    let effective_chatgpt_workspaces = config.auth_config().effective_chatgpt_workspaces();
    let mut opts = ServerOptions::new(
        config.codex_home.to_path_buf(),
        client_id.unwrap_or(CLIENT_ID.to_string()),
        effective_chatgpt_workspaces,
        config.cli_auth_credentials_store_mode,
        config.auth_keyring_backend_kind(),
        auth_route_config,
    );
    if let Some(iss) = issuer_base_url {
        opts.issuer = iss;
    }
    match run_device_code_login(opts).await {
        Ok(()) => {
            eprintln!("{LOGIN_SUCCESS_MESSAGE}");
            std::process::exit(0);
        }
        Err(e) => {
            eprintln!("Error logging in with device code: {e}");
            std::process::exit(1);
        }
    }
}

/// Prefers device-code login (with `open_browser = false`) when headless environment is detected, but keeps
/// `codex login` working in environments where device-code may be disabled/feature-gated.
/// If `run_device_code_login` returns `ErrorKind::NotFound` ("device-code unsupported"), this
/// falls back to starting the local browser login server.
pub async fn run_login_with_device_code_fallback_to_browser(
    cli_config_overrides: CliConfigOverrides,
    issuer_base_url: Option<String>,
    client_id: Option<String>,
) -> ! {
    let config = load_config_or_exit(cli_config_overrides).await;
    let _login_log_guard = init_login_file_logging(&config);
    tracing::info!("starting login flow with device code fallback");
    if !config
        .auth_config()
        .is_login_method_allowed(ForcedLoginMethod::Chatgpt)
    {
        eprintln!("{CHATGPT_LOGIN_DISABLED_MESSAGE}");
        std::process::exit(1);
    }
    let auth_route_config = config.auth_route_config();
    clear_existing_auth_before_login(
        &config.codex_home,
        config.cli_auth_credentials_store_mode,
        config.auth_keyring_backend_kind(),
        &auth_route_config,
    )
    .await;

    let effective_chatgpt_workspaces = config.auth_config().effective_chatgpt_workspaces();
    let mut opts = ServerOptions::new(
        config.codex_home.to_path_buf(),
        client_id.unwrap_or(CLIENT_ID.to_string()),
        effective_chatgpt_workspaces,
        config.cli_auth_credentials_store_mode,
        config.auth_keyring_backend_kind(),
        auth_route_config,
    );
    if let Some(iss) = issuer_base_url {
        opts.issuer = iss;
    }
    opts.open_browser = false;

    match run_device_code_login(opts.clone()).await {
        Ok(()) => {
            eprintln!("{LOGIN_SUCCESS_MESSAGE}");
            std::process::exit(0);
        }
        Err(e) => {
            if e.kind() == std::io::ErrorKind::NotFound {
                eprintln!("Device code login is not enabled; falling back to browser login.");
                match run_login_server(opts) {
                    Ok(server) => {
                        print_login_server_start(server.actual_port, &server.auth_url);
                        match server.block_until_done().await {
                            Ok(()) => {
                                eprintln!("{LOGIN_SUCCESS_MESSAGE}");
                                std::process::exit(0);
                            }
                            Err(e) => {
                                eprintln!("Error logging in: {e}");
                                std::process::exit(1);
                            }
                        }
                    }
                    Err(e) => {
                        eprintln!("Error logging in: {e}");
                        std::process::exit(1);
                    }
                }
            } else {
                eprintln!("Error logging in with device code: {e}");
                std::process::exit(1);
            }
        }
    }
}

/// Stores an Anthropic API key read from stdin, leaving ChatGPT credentials untouched.
pub async fn run_login_anthropic(
    cli_config_overrides: CliConfigOverrides,
    with_api_key: bool,
) -> ! {
    if !with_api_key {
        eprintln!(
            "Anthropic login reads the API key from stdin, e.g. `printenv ANTHROPIC_API_KEY | codex login --provider anthropic --with-api-key`.\n\
To use a Claude subscription, use `codex login --provider claude-code`, or explicitly opt into \
experimental native OAuth with `codex login --provider claude-oauth`."
        );
        std::process::exit(1);
    }
    let api_key = read_stdin_secret(
        "--with-api-key expects the API key on stdin. Try piping it, e.g. `printenv ANTHROPIC_API_KEY | codex login --provider anthropic --with-api-key`.",
        "Reading Anthropic API key from stdin...",
        "No API key provided via stdin.",
    );
    let config = load_config_or_exit(cli_config_overrides).await;
    match crate::claude_login::login_anthropic_with_api_key(&config, &api_key) {
        Ok(message) => {
            eprintln!("{message}");
            std::process::exit(0);
        }
        Err(message) => {
            eprintln!("{message}");
            std::process::exit(1);
        }
    }
}

/// Connects the official Claude Code CLI (its own sign-in), leaving ChatGPT credentials untouched.
pub async fn run_login_claude_code(cli_config_overrides: CliConfigOverrides) -> ! {
    let config = load_config_or_exit(cli_config_overrides).await;
    match crate::claude_login::login_claude_code(&config).await {
        Ok(message) => {
            eprintln!("{message}");
            std::process::exit(0);
        }
        Err(message) => {
            eprintln!("{message}");
            std::process::exit(1);
        }
    }
}

/// Explicitly opts into native experimental Claude OAuth without changing other credentials.
pub async fn run_login_claude_oauth(cli_config_overrides: CliConfigOverrides) -> ! {
    let config = load_config_or_exit(cli_config_overrides).await;
    exit_with(crate::claude_oauth_login::login(&config).await);
}

pub async fn run_login_status(
    cli_config_overrides: CliConfigOverrides,
    provider: Option<LoginProvider>,
) -> ! {
    let config = load_config_or_exit(cli_config_overrides).await;
    match provider {
        Some(LoginProvider::Anthropic) => {
            let (connected, message) = crate::claude_login::anthropic_status(&config);
            eprintln!("{message}");
            std::process::exit(if connected { 0 } else { 1 });
        }
        Some(LoginProvider::ClaudeCode) => {
            let (connected, message) = crate::claude_login::claude_code_status(&config).await;
            eprintln!("{message}");
            std::process::exit(if connected { 0 } else { 1 });
        }
        Some(LoginProvider::ClaudeOauth) => {
            let (connected, message) = crate::claude_oauth_login::status(&config);
            eprintln!("{message}");
            std::process::exit(if connected { 0 } else { 1 });
        }
        Some(LoginProvider::All) => {
            let (openai_ok, openai) = openai_login_status(&config).await;
            let (anthropic_ok, anthropic) = crate::claude_login::anthropic_status(&config);
            let (claude_code_ok, claude_code) =
                crate::claude_login::claude_code_status(&config).await;
            let (claude_oauth_ok, claude_oauth) = crate::claude_oauth_login::status(&config);
            eprintln!("OpenAI: {openai}\n{anthropic}\n{claude_code}\n{claude_oauth}");
            let any = openai_ok || anthropic_ok || claude_code_ok || claude_oauth_ok;
            std::process::exit(if any { 0 } else { 1 });
        }
        Some(LoginProvider::Openai) | None => {}
    }

    let (logged_in, message) = openai_login_status(&config).await;
    eprintln!("{message}");
    // Mention connected Claude providers without changing the exit status, which keeps
    // describing the OpenAI login for scripts.
    let (anthropic_ok, anthropic) = crate::claude_login::anthropic_status(&config);
    if anthropic_ok {
        eprintln!("{anthropic}");
    }
    let (claude_oauth_ok, claude_oauth) = crate::claude_oauth_login::status(&config);
    if claude_oauth_ok {
        eprintln!("{claude_oauth}");
    }
    if config
        .codex_home
        .join(codex_core::provider_models::CLAUDE_CODE_CONNECTED_MARKER)
        .exists()
    {
        let (_, claude_code) = crate::claude_login::claude_code_status(&config).await;
        eprintln!("{claude_code}");
    }
    std::process::exit(if logged_in { 0 } else { 1 });
}

/// Describes the ChatGPT/OpenAI login; `true` when logged in.
async fn openai_login_status(config: &Config) -> (bool, String) {
    if is_workload_identity_selected() {
        return match AuthManager::shared_from_config(
            config, /*enable_codex_api_key_env*/ false,
        )
        .await
        {
            Ok(_) => (true, "Logged in using workload identity".to_string()),
            Err(err) => (false, format!("Error checking login status: {err}")),
        };
    }

    let auth_config = config.auth_config();
    match auth_config
        .load_auth(/*enable_codex_api_key_env*/ false)
        .await
    {
        Ok(Some(auth)) => match auth.auth_mode() {
            AuthMode::ApiKey => match auth.get_token() {
                Ok(api_key) => (
                    true,
                    format!("Logged in using an API key - {}", safe_format_key(&api_key)),
                ),
                Err(e) => (false, format!("Unexpected error retrieving API key: {e}")),
            },
            AuthMode::Chatgpt | AuthMode::ChatgptAuthTokens => {
                (true, "Logged in using ChatGPT".to_string())
            }
            AuthMode::Headers => {
                unreachable!("header auth cannot be loaded from auth storage")
            }
            AuthMode::AgentIdentity => (true, "Logged in using access token".to_string()),
            AuthMode::PersonalAccessToken => {
                (true, "Logged in using personal access token".to_string())
            }
            AuthMode::BedrockApiKey => (true, "Logged in using Amazon Bedrock API key".to_string()),
            AuthMode::BedrockAccessKeys => (
                true,
                "Logged in using Amazon Bedrock AWS access keys".to_string(),
            ),
        },
        Ok(None) => (false, "Not logged in".to_string()),
        Err(err) => (false, format!("Error checking login status: {err}")),
    }
}

pub async fn run_logout(
    cli_config_overrides: CliConfigOverrides,
    provider: Option<LoginProvider>,
) -> ! {
    let config = load_config_or_exit(cli_config_overrides).await;
    match provider {
        Some(LoginProvider::Anthropic) => {
            exit_with(crate::claude_login::logout_anthropic(&config));
        }
        Some(LoginProvider::ClaudeCode) => {
            exit_with(crate::claude_login::logout_claude_code(&config));
        }
        Some(LoginProvider::ClaudeOauth) => {
            exit_with(crate::claude_oauth_login::logout(&config));
        }
        Some(LoginProvider::All) => {
            for result in [
                crate::claude_login::logout_anthropic(&config),
                crate::claude_login::logout_claude_code(&config),
                crate::claude_oauth_login::logout(&config),
            ] {
                match result {
                    Ok(message) => eprintln!("{message}"),
                    Err(message) => {
                        eprintln!("{message}");
                        std::process::exit(1);
                    }
                }
            }
        }
        Some(LoginProvider::Openai) | None => {}
    }
    let auth_route_config = config.auth_route_config();

    // Cleanup is independent of the current feature gate. Retain the grant lock
    // through account removal so another process cannot commit a late login.
    let enterprise_guard = async {
        let Some(profile) = &config.mcp_enterprise_managed_auth else {
            return Ok::<_, anyhow::Error>(None);
        };
        let auth = config
            .auth_config()
            .load_auth(/*enable_codex_api_key_env*/ false)
            .await?;
        let Some(scope) = ema_auth_scope(auth.as_ref()) else {
            return Ok(None);
        };
        EnterpriseOAuthCredentialGuard::acquire(
            &profile.idp.credential_name(&scope),
            &profile.idp.issuer,
            config.auth_keyring_backend_kind(),
        )
        .await
        .map(Some)
    }
    .await;
    let cleanup_failed = match &enterprise_guard {
        Ok(Some(guard)) => guard.delete_tokens().is_err(),
        Ok(None) => false,
        Err(_) => true,
    };
    if cleanup_failed {
        eprintln!("Warning: failed to remove enterprise authorization; continuing account logout");
    }

    let logged_out = match logout_with_revoke(
        &config.codex_home,
        config.cli_auth_credentials_store_mode,
        config.auth_keyring_backend_kind(),
        &auth_route_config,
    )
    .await
    {
        Ok(logged_out) => logged_out,
        Err(err) => {
            eprintln!("Error logging out: {err}");
            std::process::exit(1);
        }
    };
    drop(enterprise_guard);

    let cleared_bedrock_config =
        if let Some(paths) = ConfigEditsBuilder::bedrock_provider_config_paths_to_clear(&config) {
            let edits = paths
                .into_iter()
                .map(|segments| ConfigEdit::ClearPath { segments });
            if let Err(err) = ConfigEditsBuilder::for_config(&config)
                .with_edits(edits)
                .apply()
                .await
            {
                eprintln!("Error clearing Amazon Bedrock configuration after logout: {err}");
                std::process::exit(1);
            }
            true
        } else {
            false
        };

    if logged_out || cleared_bedrock_config {
        eprintln!("Successfully logged out");
    } else {
        eprintln!("Not logged in");
    }
    std::process::exit(0);
}

fn exit_with(result: Result<String, String>) -> ! {
    match result {
        Ok(message) => {
            eprintln!("{message}");
            std::process::exit(0);
        }
        Err(message) => {
            eprintln!("{message}");
            std::process::exit(1);
        }
    }
}

async fn load_config_or_exit(cli_config_overrides: CliConfigOverrides) -> Config {
    let cli_overrides = match cli_config_overrides.parse_overrides() {
        Ok(v) => v,
        Err(e) => {
            eprintln!("Error parsing -c overrides: {e}");
            std::process::exit(1);
        }
    };

    match Config::load_with_cli_overrides(cli_overrides).await {
        Ok(config) => match config.auth_config().validate() {
            Ok(()) => config,
            Err(e) => {
                eprintln!("Error loading configuration: {e}");
                std::process::exit(1);
            }
        },
        Err(e) => {
            eprintln!("Error loading configuration: {e}");
            std::process::exit(1);
        }
    }
}

fn safe_format_key(key: &str) -> String {
    if key.len() <= 13 {
        return "***".to_string();
    }
    let prefix = &key[..8];
    let suffix = &key[key.len() - 5..];
    format!("{prefix}***{suffix}")
}

#[cfg(test)]
#[path = "login_storage_tests.rs"]
mod storage_tests;

#[cfg(test)]
mod tests {
    use codex_config::types::AuthCredentialsStoreMode;
    use codex_login::AuthKeyringBackendKind;
    use codex_login::load_auth_dot_json;
    use codex_login::login_with_api_key;
    use pretty_assertions::assert_eq;
    use tempfile::tempdir;

    use super::clear_existing_auth_before_login;
    use super::safe_format_key;

    #[tokio::test]
    async fn clears_existing_auth_before_login() {
        let codex_home = tempdir().expect("create temporary Codex home");
        login_with_api_key(
            codex_home.path(),
            "sk-existing",
            AuthCredentialsStoreMode::File,
            AuthKeyringBackendKind::default(),
        )
        .expect("save existing auth");

        clear_existing_auth_before_login(
            codex_home.path(),
            AuthCredentialsStoreMode::File,
            AuthKeyringBackendKind::default(),
            &codex_login::test_support::transport_default_auth_route_config(),
        )
        .await;

        let auth = load_auth_dot_json(
            codex_home.path(),
            AuthCredentialsStoreMode::File,
            AuthKeyringBackendKind::default(),
        )
        .expect("load auth after cleanup");
        assert_eq!(auth, None);
    }

    #[test]
    fn formats_long_key() {
        let key = "sk-proj-1234567890ABCDE";
        assert_eq!(safe_format_key(key), "sk-proj-***ABCDE");
    }

    #[test]
    fn short_key_returns_stars() {
        let key = "sk-proj-12345";
        assert_eq!(safe_format_key(key), "***");
    }
}
