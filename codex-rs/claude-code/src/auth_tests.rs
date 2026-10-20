use super::ClaudeCodeAuthStatus;
use super::parse_auth_status;
use pretty_assertions::assert_eq;

#[test]
fn parses_subscription_login() {
    let status = parse_auth_status(
        r#"{"loggedIn":true,"authMethod":"claude.ai","apiProvider":"firstParty","configDirectory":"/home/me/.claude"}"#,
    )
    .expect("status");
    assert_eq!(
        status,
        ClaudeCodeAuthStatus {
            logged_in: true,
            auth_method: Some("claude.ai".to_string()),
            api_provider: Some("firstParty".to_string()),
        }
    );
    assert!(status.uses_subscription());
}

#[test]
fn oauth_token_counts_as_subscription() {
    let status =
        parse_auth_status(r#"{"loggedIn":true,"authMethod":"oauth_token"}"#).expect("status");
    assert!(status.uses_subscription());
}

#[test]
fn api_key_is_not_a_subscription() {
    let status =
        parse_auth_status(r#"{"loggedIn":true,"authMethod":"api_key","apiProvider":"firstParty"}"#)
            .expect("status");
    assert!(!status.uses_subscription());
    assert!(status.describe().contains("not a Claude subscription"));
}

#[test]
fn logged_out_drops_none_method() {
    let status =
        parse_auth_status(r#"{"loggedIn":false,"authMethod":"none","apiProvider":"firstParty"}"#)
            .expect("status");
    assert_eq!(
        status,
        ClaudeCodeAuthStatus {
            logged_in: false,
            auth_method: None,
            api_provider: Some("firstParty".to_string()),
        }
    );
    assert!(!status.uses_subscription());
    assert!(status.describe().contains("claude auth login"));
}

#[test]
fn rejects_non_json_output() {
    assert!(parse_auth_status("Not logged in").is_err());
}
