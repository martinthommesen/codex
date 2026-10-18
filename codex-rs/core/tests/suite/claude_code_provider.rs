#![cfg(unix)]
//! Runs the `claude-code` provider end to end against the fake Claude Code CLI.
//!
//! The fake speaks Claude Code's stream-json protocol and calls Codex tools over the MCP bridge,
//! so these tests cover the real session driver, bridge, and Codex tool runtime without a Claude
//! sign-in. They need the `codex-claude-code-fake` binary
//! (`cargo build -p codex-claude-code --bin codex-claude-code-fake`).

use std::ffi::OsString;

use anyhow::Result;
use codex_model_provider_info::CLAUDE_CODE_PROVIDER_ID;
use codex_protocol::config_types::ApprovalsReviewer;
use codex_protocol::protocol::AskForApproval;
use codex_protocol::protocol::EventMsg;
use codex_protocol::protocol::Op;
use codex_protocol::protocol::ReviewDecision;
use codex_protocol::protocol::SandboxPolicy;
use codex_protocol::protocol::ThreadSettingsOverrides;
use codex_protocol::user_input::UserInput;
use core_test_support::responses::start_mock_server;
use core_test_support::test_codex::TestCodex;
use core_test_support::test_codex::local_requests;
use core_test_support::test_codex::test_codex;
use core_test_support::wait_for_event;
use core_test_support::wait_for_event_match;
use pretty_assertions::assert_eq;

const CLAUDE_CODE_MODEL: &str = "claude-sonnet-5-5";

/// Points `CODEX_CLAUDE_CODE_PATH` at the fake for the duration of a test.
struct ClaudeCodePathGuard {
    original: Option<OsString>,
}

impl ClaudeCodePathGuard {
    fn set(path: &std::path::Path) -> Self {
        let original = std::env::var_os(codex_claude_code::CLAUDE_CODE_PATH_ENV_VAR);
        // Only the Claude Code backend reads this variable.
        unsafe {
            std::env::set_var(codex_claude_code::CLAUDE_CODE_PATH_ENV_VAR, path);
        }
        Self { original }
    }
}

impl Drop for ClaudeCodePathGuard {
    fn drop(&mut self) {
        unsafe {
            match &self.original {
                Some(value) => {
                    std::env::set_var(codex_claude_code::CLAUDE_CODE_PATH_ENV_VAR, value)
                }
                None => std::env::remove_var(codex_claude_code::CLAUDE_CODE_PATH_ENV_VAR),
            }
        }
    }
}

macro_rules! fake_claude_or_skip {
    () => {
        match codex_utils_cargo_bin::cargo_bin("codex-claude-code-fake") {
            Ok(path) => path,
            Err(err) => {
                eprintln!("skipping: fake Claude Code CLI is not built ({err})");
                return Ok(());
            }
        }
    };
}

async fn build_claude_code(server: &wiremock::MockServer) -> Result<TestCodex> {
    test_codex()
        .with_config(|config| {
            config.model_provider_id = CLAUDE_CODE_PROVIDER_ID.to_string();
            config.model_provider = config.model_providers[CLAUDE_CODE_PROVIDER_ID].clone();
            config.model = Some(CLAUDE_CODE_MODEL.to_string());
        })
        .build_with_auto_env(server)
        .await
}

async fn start_turn(
    test: &TestCodex,
    prompt: &str,
    approval: AskForApproval,
    sandbox: SandboxPolicy,
) -> Result<()> {
    test.codex
        .start_or_steer_turn(
            codex_core::TurnInputRequest::user_input(vec![UserInput::Text {
                text: prompt.to_string(),
                text_elements: Vec::new(),
            }])
            .with_thread_settings(ThreadSettingsOverrides {
                environments: Some(local_requests(test.config.cwd.clone())),
                approval_policy: Some(approval),
                approvals_reviewer: Some(ApprovalsReviewer::User),
                sandbox_policy: Some(sandbox),
                ..Default::default()
            }),
        )
        .await?;
    Ok(())
}

async fn last_agent_message(test: &TestCodex) -> Option<String> {
    let mut last = None;
    loop {
        match wait_for_event(&test.codex, |event| {
            matches!(event, EventMsg::AgentMessage(_) | EventMsg::TurnComplete(_))
        })
        .await
        {
            EventMsg::AgentMessage(message) => last = Some(message.message),
            EventMsg::TurnComplete(complete) => {
                assert_eq!(complete.error, None, "turn should succeed");
                return last;
            }
            _ => unreachable!(),
        }
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn claude_code_runs_codex_tools_through_the_bridge() -> Result<()> {
    let fake = fake_claude_or_skip!();
    let _guard = ClaudeCodePathGuard::set(&fake);
    // No model request goes to this server; the harness only needs a base URL.
    let server = start_mock_server().await;
    let test = build_claude_code(&server).await?;
    assert_eq!(
        test.session_configured.model_provider_id,
        CLAUDE_CODE_PROVIDER_ID
    );

    start_turn(
        &test,
        "[exec:echo bridged-output]",
        AskForApproval::Never,
        SandboxPolicy::DangerFullAccess,
    )
    .await?;
    let reply = last_agent_message(&test).await.expect("final answer");

    assert!(reply.starts_with("Tool said:"), "{reply}");
    assert!(reply.contains("bridged-output"), "{reply}");
    assert!(
        server
            .received_requests()
            .await
            .unwrap_or_default()
            .is_empty(),
        "Claude Code makes model requests itself; Codex must not call an HTTP endpoint"
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn claude_code_tool_calls_respect_approval_policy() -> Result<()> {
    let fake = fake_claude_or_skip!();
    let _guard = ClaudeCodePathGuard::set(&fake);
    let server = start_mock_server().await;
    let test = build_claude_code(&server).await?;

    start_turn(
        &test,
        "[exec:touch claude-code-denied.txt]",
        AskForApproval::UnlessTrusted,
        SandboxPolicy::new_read_only_policy(),
    )
    .await?;
    let approval = wait_for_event_match(&test.codex, |event| match event {
        EventMsg::ExecApprovalRequest(approval) => Some(approval.clone()),
        EventMsg::TurnComplete(_) => panic!("turn completed without asking for approval"),
        _ => None,
    })
    .await;
    assert!(
        approval
            .command
            .iter()
            .any(|part| part.contains("touch claude-code-denied.txt")),
        "{:?}",
        approval.command
    );
    test.codex
        .submit(Op::ExecApproval {
            id: approval.effective_approval_id(),
            turn_id: None,
            decision: ReviewDecision::denied("not allowed"),
        })
        .await?;
    let reply = last_agent_message(&test).await.expect("final answer");

    assert!(reply.starts_with("Tool said:"), "{reply}");
    assert!(!test.workspace_path("claude-code-denied.txt").exists());
    Ok(())
}
