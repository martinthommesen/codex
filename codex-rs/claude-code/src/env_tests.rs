use super::is_scrubbed_env_var;
use pretty_assertions::assert_eq;

#[test]
fn scrubs_provider_and_codex_secrets_only() {
    let cases = [
        ("ANTHROPIC_API_KEY", true),
        ("ANTHROPIC_AUTH_TOKEN", true),
        ("OPENAI_API_KEY", true),
        ("CODEX_API_KEY", true),
        ("CODEX_ACCESS_TOKEN", true),
        ("CODEX_REMOTE_AUTH_TOKEN", true),
        ("CODEX_GITHUB_TOKEN", true),
        ("OPENAI_IDENTITY_TOKEN_FILE", true),
        ("CLAUDE_CODE_OAUTH_TOKEN", false),
        ("CODEX_CLAUDE_CODE_PATH", false),
        ("CODEX_HOME", false),
        ("HOME", false),
        ("PATH", false),
        ("ANTHROPIC_BASE_URL", false),
    ];
    let actual: Vec<(&str, bool)> = cases
        .iter()
        .map(|(name, _)| (*name, is_scrubbed_env_var(name)))
        .collect();
    assert_eq!(actual, cases.to_vec());
}
