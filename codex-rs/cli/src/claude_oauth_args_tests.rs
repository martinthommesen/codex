use super::*;

#[test]
fn claude_oauth_login_status_and_logout_select_the_isolated_provider() {
    for arguments in [
        vec!["codex", "login", "--provider", "claude-oauth"],
        vec!["codex", "login", "status", "--provider", "claude-oauth"],
        vec!["codex", "login", "--provider", "claude-oauth", "status"],
    ] {
        let cli = MultitoolCli::try_parse_from(arguments).expect("OAuth login arguments");
        let Some(Subcommand::Login(login)) = cli.subcommand else {
            panic!("expected login command");
        };
        assert_eq!(login.provider, Some(LoginProvider::ClaudeOauth));
    }
    let cli = MultitoolCli::try_parse_from(["codex", "logout", "--provider", "claude-oauth"])
        .expect("OAuth logout arguments");
    let Some(Subcommand::Logout(logout)) = cli.subcommand else {
        panic!("expected logout command");
    };
    assert_eq!(logout.provider, Some(LoginProvider::ClaudeOauth));
}

#[test]
fn login_status_help_documents_exit_codes() {
    let cli = MultitoolCli::try_parse_from(["codex", "login", "status", "--help"])
        .expect_err("help display");
    let help = cli.to_string();
    assert!(help.contains("Exit status"), "{help}");
    assert!(help.contains("at least one provider"), "{help}");
}

#[test]
fn login_help_marks_claude_oauth_experimental() {
    let cli = MultitoolCli::try_parse_from(["codex", "login", "--help"]).expect_err("help display");
    let help = cli.to_string();
    assert!(help.contains("claude-oauth"), "{help}");
    assert!(help.contains("experimental"), "{help}");
}
