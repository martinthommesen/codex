//! Native Claude OAuth CLI behavior; no live authorization or network access is required.

use std::time::Duration;

use anyhow::Result;
use codex_config::types::AuthCredentialsStoreMode;
use codex_login::AuthKeyringBackendKind;
use predicates::str::contains;
use tempfile::TempDir;

fn command(home: &TempDir) -> Result<assert_cmd::Command> {
    let mut command = assert_cmd::Command::new(codex_utils_cargo_bin::cargo_bin("codex")?);
    command
        .current_dir(home.path())
        .env("CODEX_HOME", home.path())
        .env_remove("CODEX_ACCESS_TOKEN")
        .env_remove("OPENAI_API_KEY")
        .env_remove("ANTHROPIC_API_KEY")
        .timeout(Duration::from_secs(15));
    Ok(command)
}

#[test]
fn oauth_rejects_other_providers_credential_import_and_custom_endpoints() -> Result<()> {
    let home = TempDir::new()?;
    for arguments in [
        vec!["--with-api-key"],
        vec!["--with-access-token"],
        vec!["--api-key", "secret-that-must-not-be-echoed"],
        vec!["--device-auth"],
        vec!["--experimental_issuer", "https://untrusted.example.test"],
        vec!["--experimental_client-id", "different-client"],
    ] {
        let assertion = command(&home)?
            .args(["login", "--provider", "claude-oauth"])
            .args(arguments)
            .assert()
            .failure()
            .stderr(contains("requires its own interactive authorization flow"));
        let error = String::from_utf8_lossy(&assertion.get_output().stderr);
        assert!(!error.contains("secret-that-must-not-be-echoed"), "{error}");
        assert!(!error.contains("oauth/authorize"), "{error}");
    }
    Ok(())
}

#[test]
fn oauth_status_and_logout_leave_saved_openai_login_untouched() -> Result<()> {
    let home = TempDir::new()?;
    std::fs::write(
        home.path().join("config.toml"),
        "cli_auth_credentials_store = 'file'\n",
    )?;
    codex_login::login_with_api_key(
        home.path(),
        "sk-openai-test-isolated-oauth-status",
        AuthCredentialsStoreMode::File,
        AuthKeyringBackendKind::Direct,
    )?;
    let auth_path = home.path().join("auth.json");
    let before = std::fs::read(&auth_path)?;
    command(&home)?
        .args(["login", "status", "--provider", "claude-oauth"])
        .assert()
        .failure()
        .stderr(contains("Claude OAuth (experimental): not configured"));
    command(&home)?
        .args(["logout", "--provider", "claude-oauth"])
        .assert()
        .success()
        .stderr(contains("No Claude OAuth credentials were stored by Codex"));
    assert_eq!(std::fs::read(&auth_path)?, before);
    command(&home)?
        .args(["login", "status", "--provider", "openai"])
        .assert()
        .success()
        .stderr(contains("Logged in using an API key"));
    Ok(())
}
