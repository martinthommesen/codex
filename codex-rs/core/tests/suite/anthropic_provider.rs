//! End-to-end coverage for the Anthropic Messages provider and mixed-provider delegation.
//!
//! One mock server stands in for both backends: OpenAI requests arrive at `/v1/responses` and
//! Anthropic requests at `/v1/messages`. The Anthropic API key is saved in the test's Codex home
//! through the isolated Anthropic credential store, exactly as `codex login --provider anthropic`
//! does, so these tests also prove that each provider is sent only its own credential.

use std::path::Path;
use std::sync::Arc;
use std::sync::Mutex;
use std::time::Duration;

use anyhow::Result;
use codex_config::types::AuthCredentialsStoreMode;
use codex_config::types::AuthKeyringBackendKind;
use codex_core::config::Config;
use codex_features::Feature;
use codex_login::AnthropicAuthConfig;
use codex_model_provider_info::ANTHROPIC_PROVIDER_ID;
use codex_model_provider_info::CLAUDE_CODE_PROVIDER_ID;
use codex_model_provider_info::OPENAI_PROVIDER_ID;
use codex_protocol::protocol::AskForApproval;
use codex_protocol::protocol::EventMsg;
use codex_protocol::protocol::Op;
use codex_protocol::protocol::ReviewDecision;
use codex_protocol::protocol::SandboxPolicy;
use codex_protocol::protocol::ThreadSettingsOverrides;
use codex_protocol::user_input::UserInput;
use core_test_support::responses::ev_assistant_message;
use core_test_support::responses::ev_completed;
use core_test_support::responses::ev_function_call_with_namespace;
use core_test_support::responses::ev_response_created;
use core_test_support::responses::mount_sse_once_match;
use core_test_support::responses::sse;
use core_test_support::responses::start_mock_server;
use core_test_support::skip_if_no_network;
use core_test_support::test_codex::TestCodex;
use core_test_support::test_codex::local_requests;
use core_test_support::test_codex::test_codex;
use core_test_support::wait_for_event;
use core_test_support::wait_for_event_match;
use pretty_assertions::assert_eq;
use serde_json::Value;
use serde_json::json;
use tokio::time::Instant;
use tokio::time::sleep;
use wiremock::Match;
use wiremock::Mock;
use wiremock::MockServer;
use wiremock::Request;
use wiremock::ResponseTemplate;
use wiremock::matchers::method;
use wiremock::matchers::path_regex;

const ANTHROPIC_TEST_KEY: &str = "sk-ant-api03-codex-integration-test-key-0123456789";
const OPENAI_TEST_KEY: &str = "sk-openai-codex-integration-test-key";
const COLLABORATION_NAMESPACE: &str = "collaboration";
/// `collaboration.spawn_agent` as the Anthropic adapter names it on the wire.
const ANTHROPIC_SPAWN_TOOL: &str = "collaboration__spawn_agent";
const CLAUDE_PARENT_MODEL: &str = "claude-sonnet-5-5";
const CLAUDE_CHILD_MODEL: &str = "claude-haiku-5-5";
const OPENAI_PARENT_MODEL: &str = "gpt-5.5";
const OPENAI_CHILD_MODEL: &str = "gpt-5.6-luna";
/// A bundled model whose catalog entry gives sub-agents the V2 collaboration tools.
const OPENAI_V2_CHILD_MODEL: &str = "gpt-5.6-terra";
const PARENT_PROMPT: &str = "delegate the work please";
const CLAUDE_CHILD_PROMPT: &str = "claude child: summarize the repository";
const OPENAI_CHILD_PROMPT: &str = "openai child: list the test files";

// ---------------------------------------------------------------------------------------------
// Anthropic mock helpers
// ---------------------------------------------------------------------------------------------

/// Captures every `/v1/messages` request a mock matched.
#[derive(Clone, Default)]
struct MessagesLog {
    requests: Arc<Mutex<Vec<Request>>>,
}

impl MessagesLog {
    fn requests(&self) -> Vec<Request> {
        self.requests
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }

    fn single_request(&self) -> Request {
        let requests = self.requests();
        assert_eq!(requests.len(), 1, "expected exactly one Anthropic request");
        requests[0].clone()
    }

    async fn wait_for_request(&self) -> Result<Request> {
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            if let Some(request) = self.requests().into_iter().next() {
                return Ok(request);
            }
            if Instant::now() >= deadline {
                anyhow::bail!("timed out waiting for an Anthropic request");
            }
            sleep(Duration::from_millis(10)).await;
        }
    }
}

impl Match for MessagesLog {
    fn matches(&self, request: &Request) -> bool {
        self.requests
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push(request.clone());
        true
    }
}

fn request_json(request: &Request) -> Value {
    let is_zstd = request
        .headers
        .get("content-encoding")
        .and_then(|value| value.to_str().ok())
        .is_some_and(|value| value.eq_ignore_ascii_case("zstd"));
    let body = if is_zstd {
        zstd::stream::decode_all(std::io::Cursor::new(&request.body)).expect("decode zstd body")
    } else {
        request.body.clone()
    };
    serde_json::from_slice(&body).expect("request body should be JSON")
}

fn request_text(request: &Request) -> String {
    request_json(request).to_string()
}

fn header(request: &Request, name: &str) -> Option<String> {
    request
        .headers
        .get(name)
        .and_then(|value| value.to_str().ok())
        .map(str::to_string)
}

/// Mounts a one-shot `/v1/messages` response for requests accepted by `matcher`.
async fn mount_messages_once<M>(server: &MockServer, matcher: M, body: String) -> MessagesLog
where
    M: Fn(&Request) -> bool + Send + Sync + 'static,
{
    let log = MessagesLog::default();
    Mock::given(method("POST"))
        .and(path_regex(".*/v1/messages$"))
        .and(matcher)
        .and(log.clone())
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "text/event-stream")
                .insert_header("request-id", "req_codex_test")
                .set_body_string(body),
        )
        .up_to_n_times(1)
        .mount(server)
        .await;
    log
}

/// Serves `body` for any later `/v1/messages` request so follow-up turns never hit a 404.
async fn mount_messages_fallback(server: &MockServer, body: String) -> MessagesLog {
    let log = MessagesLog::default();
    Mock::given(method("POST"))
        .and(path_regex(".*/v1/messages$"))
        .and(log.clone())
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "text/event-stream")
                .set_body_string(body),
        )
        .mount(server)
        .await;
    log
}

/// Serves a plain assistant message for any later `/v1/responses` request.
async fn mount_responses_fallback(server: &MockServer) {
    Mock::given(method("POST"))
        .and(path_regex(".*/responses$"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "text/event-stream")
                .set_body_string(sse(vec![
                    ev_response_created("resp-fallback"),
                    ev_assistant_message("msg-fallback", "ok"),
                    ev_completed("resp-fallback"),
                ])),
        )
        .mount(server)
        .await;
}

/// Encodes Anthropic SSE events. Unlike `responses::sse`, every event keeps its `data:` line, as
/// the Messages API sends `data: {"type":"message_stop"}`.
fn anthropic_sse(events: Vec<Value>) -> String {
    events
        .into_iter()
        .map(|event| {
            let kind = event["type"].as_str().expect("event type").to_string();
            format!("event: {kind}\ndata: {event}\n\n")
        })
        .collect()
}

fn message_start(id: &str, model: &str) -> Value {
    json!({
        "type": "message_start",
        "message": {
            "id": id,
            "type": "message",
            "role": "assistant",
            "model": model,
            "content": [],
            "stop_reason": null,
            "usage": {"input_tokens": 10, "cache_read_input_tokens": 5, "output_tokens": 1}
        }
    })
}

fn message_end(stop_reason: &str) -> Vec<Value> {
    vec![
        json!({
            "type": "message_delta",
            "delta": {"stop_reason": stop_reason, "stop_sequence": null},
            "usage": {"output_tokens": 7}
        }),
        json!({"type": "message_stop"}),
    ]
}

fn text_block(index: u64, text: &str) -> Vec<Value> {
    vec![
        json!({"type": "content_block_start", "index": index, "content_block": {"type": "text", "text": ""}}),
        json!({"type": "content_block_delta", "index": index, "delta": {"type": "text_delta", "text": text}}),
        json!({"type": "content_block_stop", "index": index}),
    ]
}

fn tool_use_block(index: u64, id: &str, name: &str, input: &Value) -> Vec<Value> {
    vec![
        json!({
            "type": "content_block_start",
            "index": index,
            "content_block": {"type": "tool_use", "id": id, "name": name, "input": {}}
        }),
        json!({
            "type": "content_block_delta",
            "index": index,
            "delta": {"type": "input_json_delta", "partial_json": input.to_string()}
        }),
        json!({"type": "content_block_stop", "index": index}),
    ]
}

fn anthropic_text_sse(message_id: &str, model: &str, text: &str) -> String {
    let mut events = vec![message_start(message_id, model)];
    events.extend(text_block(0, text));
    events.extend(message_end("end_turn"));
    anthropic_sse(events)
}

fn anthropic_tool_use_sse(
    message_id: &str,
    model: &str,
    tool_uses: &[(&str, &str, Value)],
) -> String {
    let mut events = vec![message_start(message_id, model)];
    events.extend(text_block(0, "Working on it."));
    for (offset, (id, name, input)) in tool_uses.iter().enumerate() {
        events.extend(tool_use_block(offset as u64 + 1, id, name, input));
    }
    events.extend(message_end("tool_use"));
    anthropic_sse(events)
}

/// Returns the `tool_result` block answering `tool_use_id`, if the request carries one.
fn tool_result(request: &Request, tool_use_id: &str) -> Option<Value> {
    request_json(request)["messages"]
        .as_array()?
        .iter()
        .filter_map(|message| message["content"].as_array())
        .flatten()
        .find(|block| block["type"] == "tool_result" && block["tool_use_id"] == tool_use_id)
        .cloned()
}

fn tool_result_text(block: &Value) -> String {
    match &block["content"] {
        Value::String(text) => text.clone(),
        Value::Array(parts) => parts
            .iter()
            .filter_map(|part| part["text"].as_str())
            .collect::<Vec<_>>()
            .join("\n"),
        _ => String::new(),
    }
}

fn tool_names(request: &Request) -> Vec<String> {
    request_json(request)["tools"]
        .as_array()
        .map(|tools| {
            tools
                .iter()
                .filter_map(|tool| tool["name"].as_str().map(str::to_string))
                .collect()
        })
        .unwrap_or_default()
}

fn has_tool_result(request: &Request) -> bool {
    request_text(request).contains("\"tool_result\"")
}

// ---------------------------------------------------------------------------------------------
// Config helpers
// ---------------------------------------------------------------------------------------------

/// Saves the Anthropic key the way `codex login --provider anthropic` does (file store).
fn store_anthropic_key(home: &Path) {
    AnthropicAuthConfig::new(
        home.to_path_buf(),
        AuthCredentialsStoreMode::File,
        AuthKeyringBackendKind::default(),
    )
    .save_api_key(ANTHROPIC_TEST_KEY)
    .expect("store Anthropic key");
}

/// Points both built-in providers at the mock server; `root` picks the main agent's provider.
fn route_providers_to_mock(config: &mut Config, root: &str, model: &str) {
    let base_url = config.model_provider.base_url.clone();
    for provider_id in [OPENAI_PROVIDER_ID, ANTHROPIC_PROVIDER_ID] {
        let provider = config
            .model_providers
            .get_mut(provider_id)
            .unwrap_or_else(|| panic!("built-in provider {provider_id}"));
        provider.base_url = base_url.clone();
        provider.supports_websockets = false;
        provider.request_max_retries = Some(0);
        provider.stream_max_retries = Some(0);
    }
    if root == ANTHROPIC_PROVIDER_ID {
        config.model_provider_id = ANTHROPIC_PROVIDER_ID.to_string();
        config.model_provider = config.model_providers[ANTHROPIC_PROVIDER_ID].clone();
    } else {
        config.model_provider.request_max_retries = Some(0);
        config.model_provider.stream_max_retries = Some(0);
    }
    config.model = Some(model.to_string());
}

fn enable_multi_agent_v2(config: &mut Config) {
    config
        .features
        .enable(Feature::Collab)
        .expect("test config should allow feature update");
    config
        .features
        .enable(Feature::MultiAgentV2)
        .expect("test config should allow feature update");
}

async fn build(server: &MockServer, root: &'static str, model: &'static str) -> Result<TestCodex> {
    test_codex()
        .with_auth(codex_login::CodexAuth::from_api_key(OPENAI_TEST_KEY))
        .with_pre_build_hook(store_anthropic_key)
        .with_config(move |config| {
            route_providers_to_mock(config, root, model);
            enable_multi_agent_v2(config);
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
                approvals_reviewer: Some(codex_protocol::config_types::ApprovalsReviewer::User),
                sandbox_policy: Some(sandbox),
                ..Default::default()
            }),
        )
        .await?;
    Ok(())
}

async fn wait_for_turn_complete(test: &TestCodex) {
    wait_for_event(&test.codex, |event| {
        matches!(event, EventMsg::TurnComplete(_))
    })
    .await;
}

fn assert_anthropic_auth_only(request: &Request) {
    assert_eq!(
        header(request, "x-api-key").as_deref(),
        Some(ANTHROPIC_TEST_KEY)
    );
    assert_eq!(
        header(request, "anthropic-version").as_deref(),
        Some("2023-06-01")
    );
    assert_eq!(
        header(request, "authorization"),
        None,
        "Anthropic requests must not carry the OpenAI credential"
    );
}

fn assert_openai_auth_only(request: &core_test_support::responses::ResponsesRequest) {
    assert_eq!(
        request.header("authorization").as_deref(),
        Some(format!("Bearer {OPENAI_TEST_KEY}").as_str())
    );
    assert_eq!(
        request.header("x-api-key"),
        None,
        "OpenAI requests must not carry the Anthropic credential"
    );
}

// ---------------------------------------------------------------------------------------------
// Main-agent tests
// ---------------------------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn anthropic_turn_streams_text_with_isolated_credentials_and_usage() -> Result<()> {
    skip_if_no_network!(Ok(()));
    let server = start_mock_server().await;
    let messages = mount_messages_once(
        &server,
        |_| true,
        anthropic_text_sse("msg_1", CLAUDE_PARENT_MODEL, "Hello from Claude"),
    )
    .await;
    let test = build(&server, ANTHROPIC_PROVIDER_ID, CLAUDE_PARENT_MODEL).await?;
    assert_eq!(test.session_configured.model, CLAUDE_PARENT_MODEL);

    start_turn(
        &test,
        "say hello",
        AskForApproval::Never,
        SandboxPolicy::new_read_only_policy(),
    )
    .await?;
    let message = wait_for_event_match(&test.codex, |event| match event {
        EventMsg::AgentMessage(message) => Some(message.message.clone()),
        _ => None,
    })
    .await;
    assert_eq!(message, "Hello from Claude");
    let usage = wait_for_event_match(&test.codex, |event| match event {
        EventMsg::TokenCount(count) => count
            .info
            .as_ref()
            .map(|info| info.last_token_usage.clone()),
        _ => None,
    })
    .await;
    wait_for_turn_complete(&test).await;

    assert_eq!(
        (
            usage.input_tokens,
            usage.cached_input_tokens,
            usage.output_tokens
        ),
        (15, 5, 7)
    );
    let request = messages.single_request();
    assert_anthropic_auth_only(&request);
    let body = request_json(&request);
    assert_eq!(body["model"], CLAUDE_PARENT_MODEL);
    assert_eq!(body["stream"], true);
    assert!(body["system"].is_array() || body["system"].is_string());
    assert!(request_text(&request).contains("say hello"));
    let tools = tool_names(&request);
    for expected in ["exec_command", "apply_patch"] {
        assert!(
            tools.iter().any(|name| name == expected),
            "missing {expected}: {tools:?}"
        );
    }
    // The key never reaches the model-visible request body.
    assert!(!request_text(&request).contains(ANTHROPIC_TEST_KEY));
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn anthropic_tool_call_runs_shell_command_and_returns_output() -> Result<()> {
    skip_if_no_network!(Ok(()));
    let server = start_mock_server().await;
    let first = mount_messages_once(
        &server,
        |request| !has_tool_result(request),
        anthropic_tool_use_sse(
            "msg_tool",
            CLAUDE_PARENT_MODEL,
            &[(
                "toolu_exec_1",
                "exec_command",
                json!({"cmd": "echo claude-tool-output", "yield_time_ms": 5000}),
            )],
        ),
    )
    .await;
    let second = mount_messages_once(
        &server,
        has_tool_result,
        anthropic_text_sse("msg_done", CLAUDE_PARENT_MODEL, "Command finished."),
    )
    .await;
    let test = build(&server, ANTHROPIC_PROVIDER_ID, CLAUDE_PARENT_MODEL).await?;

    start_turn(
        &test,
        "run echo",
        AskForApproval::Never,
        SandboxPolicy::DangerFullAccess,
    )
    .await?;
    wait_for_turn_complete(&test).await;

    let _ = first.single_request();
    let follow_up = second.single_request();
    let result = tool_result(&follow_up, "toolu_exec_1").expect("tool_result for exec_command");
    assert!(
        tool_result_text(&result).contains("claude-tool-output"),
        "unexpected tool result: {result}"
    );
    // History replays the assistant tool_use before the matching result.
    let follow_up_text = request_text(&follow_up);
    assert!(follow_up_text.contains("\"tool_use\""));
    assert!(follow_up_text.contains("toolu_exec_1"));
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn anthropic_tool_call_respects_approval_policy() -> Result<()> {
    skip_if_no_network!(Ok(()));
    let server = start_mock_server().await;
    mount_messages_once(
        &server,
        |request| !has_tool_result(request),
        anthropic_tool_use_sse(
            "msg_approval",
            CLAUDE_PARENT_MODEL,
            &[(
                "toolu_touch_1",
                "exec_command",
                json!({"cmd": "touch claude-denied.txt", "yield_time_ms": 5000}),
            )],
        ),
    )
    .await;
    let second = mount_messages_once(
        &server,
        has_tool_result,
        anthropic_text_sse("msg_after_denial", CLAUDE_PARENT_MODEL, "Understood."),
    )
    .await;
    let test = build(&server, ANTHROPIC_PROVIDER_ID, CLAUDE_PARENT_MODEL).await?;

    start_turn(
        &test,
        "create a file",
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
            .any(|part| part.contains("touch claude-denied.txt")),
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
    wait_for_turn_complete(&test).await;

    assert!(!test.workspace_path("claude-denied.txt").exists());
    let follow_up = second.single_request();
    assert!(
        tool_result(&follow_up, "toolu_touch_1").is_some(),
        "the denial is reported back to Claude as the tool result"
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn anthropic_apply_patch_edits_files() -> Result<()> {
    skip_if_no_network!(Ok(()));
    let server = start_mock_server().await;
    let patch =
        "*** Begin Patch\n*** Add File: claude-note.txt\n+written by claude\n*** End Patch\n";
    mount_messages_once(
        &server,
        |request| !has_tool_result(request),
        anthropic_tool_use_sse(
            "msg_patch",
            CLAUDE_PARENT_MODEL,
            &[("toolu_patch_1", "apply_patch", json!({"input": patch}))],
        ),
    )
    .await;
    let second = mount_messages_once(
        &server,
        has_tool_result,
        anthropic_text_sse("msg_patched", CLAUDE_PARENT_MODEL, "Patched."),
    )
    .await;
    let test = build(&server, ANTHROPIC_PROVIDER_ID, CLAUDE_PARENT_MODEL).await?;

    start_turn(
        &test,
        "add a note",
        AskForApproval::Never,
        SandboxPolicy::DangerFullAccess,
    )
    .await?;
    wait_for_turn_complete(&test).await;

    let written = std::fs::read_to_string(test.workspace_path("claude-note.txt"))?;
    assert_eq!(written, "written by claude\n");
    let follow_up = second.single_request();
    assert!(tool_result(&follow_up, "toolu_patch_1").is_some());
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn anthropic_key_is_not_sent_across_a_redirect() -> Result<()> {
    skip_if_no_network!(Ok(()));
    let server = start_mock_server().await;
    let other_origin = start_mock_server().await;
    Mock::given(method("POST"))
        .and(path_regex(".*/v1/messages$"))
        .respond_with(
            ResponseTemplate::new(307)
                .insert_header("location", format!("{}/v1/messages", other_origin.uri())),
        )
        .mount(&server)
        .await;
    let test = build(&server, ANTHROPIC_PROVIDER_ID, CLAUDE_PARENT_MODEL).await?;

    start_turn(
        &test,
        "say hello",
        AskForApproval::Never,
        SandboxPolicy::new_read_only_policy(),
    )
    .await?;
    let complete = wait_for_event_match(&test.codex, |event| match event {
        EventMsg::TurnComplete(complete) => Some(complete.clone()),
        _ => None,
    })
    .await;

    assert!(complete.error.is_some(), "a redirect must fail the request");
    assert!(
        other_origin
            .received_requests()
            .await
            .unwrap_or_default()
            .is_empty(),
        "the request, and with it x-api-key, must not reach the redirect target"
    );
    Ok(())
}

// ---------------------------------------------------------------------------------------------
// Mixed-provider delegation
// ---------------------------------------------------------------------------------------------

fn spawn_args(task_name: &str, message: &str, provider: &str, model: &str) -> Value {
    json!({
        "task_name": task_name,
        "message": message,
        "model_provider": provider,
        "model": model,
    })
}

/// Like [`spawn_args`] without forking the parent's history. A same-provider child on another
/// model would otherwise first compact the forked history, which is unrelated to providers.
fn spawn_args_without_fork(task_name: &str, message: &str, provider: &str, model: &str) -> Value {
    let mut args = spawn_args(task_name, message, provider, model);
    args["fork_turns"] = json!("none");
    args
}

fn openai_spawn_turn(calls: &[(&str, Value)]) -> String {
    let mut events = vec![ev_response_created("resp-parent-1")];
    for (call_id, args) in calls {
        events.push(ev_function_call_with_namespace(
            call_id,
            COLLABORATION_NAMESPACE,
            "spawn_agent",
            &args.to_string(),
        ));
    }
    events.push(ev_completed("resp-parent-1"));
    sse(events)
}

fn body_contains(request: &Request, text: &str) -> bool {
    request_json_lossy(request).to_string().contains(text)
}

fn request_json_lossy(request: &Request) -> Value {
    let is_zstd = request
        .headers
        .get("content-encoding")
        .and_then(|value| value.to_str().ok())
        .is_some_and(|value| value.eq_ignore_ascii_case("zstd"));
    let body = if is_zstd {
        zstd::stream::decode_all(std::io::Cursor::new(&request.body)).unwrap_or_default()
    } else {
        request.body.clone()
    };
    serde_json::from_slice(&body).unwrap_or(Value::Null)
}

/// Waits for an OpenAI request accepted by `accept`. `ResponseMock` records every request it is
/// evaluated against, including ones its own matcher then rejects, so callers must filter.
async fn wait_for_responses_request(
    mock: &core_test_support::responses::ResponseMock,
    accept: impl Fn(&core_test_support::responses::ResponsesRequest) -> bool,
) -> Result<core_test_support::responses::ResponsesRequest> {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        if let Some(request) = mock.requests().into_iter().find(|request| accept(request)) {
            return Ok(request);
        }
        if Instant::now() >= deadline {
            anyhow::bail!("timed out waiting for a matching OpenAI request");
        }
        sleep(Duration::from_millis(10)).await;
    }
}

fn uses_model(
    model: &'static str,
) -> impl Fn(&core_test_support::responses::ResponsesRequest) -> bool {
    move |request| request.body_json()["model"] == model
}

fn answers_call(
    call_id: &'static str,
) -> impl Fn(&core_test_support::responses::ResponsesRequest) -> bool {
    move |request| request.function_call_output_text(call_id).is_some()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn openai_parent_spawns_claude_child() -> Result<()> {
    skip_if_no_network!(Ok(()));
    let server = start_mock_server().await;
    let parent = mount_sse_once_match(
        &server,
        |request: &Request| {
            body_contains(request, PARENT_PROMPT) && !body_contains(request, "call-spawn-claude")
        },
        openai_spawn_turn(&[(
            "call-spawn-claude",
            spawn_args(
                "claude_worker",
                CLAUDE_CHILD_PROMPT,
                ANTHROPIC_PROVIDER_ID,
                CLAUDE_CHILD_MODEL,
            ),
        )]),
    )
    .await;
    let parent_follow_up = mount_sse_once_match(
        &server,
        |request: &Request| body_contains(request, "call-spawn-claude"),
        sse(vec![
            ev_response_created("resp-parent-2"),
            ev_assistant_message("msg-parent-2", "parent done"),
            ev_completed("resp-parent-2"),
        ]),
    )
    .await;
    let child = mount_messages_once(
        &server,
        |request| body_contains(request, CLAUDE_CHILD_PROMPT),
        anthropic_text_sse("msg_child", CLAUDE_CHILD_MODEL, "claude child done"),
    )
    .await;
    mount_responses_fallback(&server).await;
    mount_messages_fallback(
        &server,
        anthropic_text_sse("msg_extra", CLAUDE_CHILD_MODEL, "ok"),
    )
    .await;
    let test = build(&server, OPENAI_PROVIDER_ID, OPENAI_PARENT_MODEL).await?;

    test.submit_turn(PARENT_PROMPT).await?;

    let parent_request = parent.single_request();
    assert_openai_auth_only(&parent_request);
    let spawn_tool = parent_request
        .tool_by_name(COLLABORATION_NAMESPACE, "spawn_agent")
        .expect("spawn_agent tool");
    let provider_param = &spawn_tool["parameters"]["properties"]["model_provider"];
    assert!(
        provider_param.to_string().contains(ANTHROPIC_PROVIDER_ID),
        "spawn_agent should list the connected Anthropic provider: {spawn_tool}"
    );
    let child_request = child.wait_for_request().await?;
    assert_anthropic_auth_only(&child_request);
    assert_eq!(request_json(&child_request)["model"], CLAUDE_CHILD_MODEL);
    let spawn_output =
        wait_for_responses_request(&parent_follow_up, answers_call("call-spawn-claude"))
            .await?
            .function_call_output_text("call-spawn-claude")
            .expect("spawn_agent output");
    assert!(!spawn_output.contains("not connected"), "{spawn_output}");
    // The parent keeps its own provider and model.
    assert_eq!(parent_request.body_json()["model"], OPENAI_PARENT_MODEL);
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn claude_parent_spawns_openai_child() -> Result<()> {
    skip_if_no_network!(Ok(()));
    let server = start_mock_server().await;
    let parent = mount_messages_once(
        &server,
        |request| body_contains(request, PARENT_PROMPT) && !has_tool_result(request),
        anthropic_tool_use_sse(
            "msg_parent",
            CLAUDE_PARENT_MODEL,
            &[(
                "toolu_spawn_openai",
                ANTHROPIC_SPAWN_TOOL,
                spawn_args(
                    "openai_worker",
                    OPENAI_CHILD_PROMPT,
                    OPENAI_PROVIDER_ID,
                    OPENAI_CHILD_MODEL,
                ),
            )],
        ),
    )
    .await;
    let parent_follow_up = mount_messages_once(
        &server,
        |request| body_contains(request, PARENT_PROMPT) && has_tool_result(request),
        anthropic_text_sse("msg_parent_done", CLAUDE_PARENT_MODEL, "parent done"),
    )
    .await;
    let child = mount_sse_once_match(
        &server,
        |request: &Request| body_contains(request, OPENAI_CHILD_PROMPT),
        sse(vec![
            ev_response_created("resp-child-1"),
            ev_assistant_message("msg-child-1", "openai child done"),
            ev_completed("resp-child-1"),
        ]),
    )
    .await;
    mount_responses_fallback(&server).await;
    mount_messages_fallback(
        &server,
        anthropic_text_sse("msg_extra", CLAUDE_PARENT_MODEL, "ok"),
    )
    .await;
    let test = build(&server, ANTHROPIC_PROVIDER_ID, CLAUDE_PARENT_MODEL).await?;

    test.submit_turn(PARENT_PROMPT).await?;

    let parent_request = parent.single_request();
    assert_anthropic_auth_only(&parent_request);
    let tools = tool_names(&parent_request);
    assert!(
        tools.iter().any(|name| name == ANTHROPIC_SPAWN_TOOL),
        "{tools:?}"
    );
    let child_request = wait_for_responses_request(&child, uses_model(OPENAI_CHILD_MODEL)).await?;
    assert_openai_auth_only(&child_request);
    assert_eq!(child_request.body_json()["model"], OPENAI_CHILD_MODEL);
    let follow_up = parent_follow_up.wait_for_request().await?;
    let result = tool_result(&follow_up, "toolu_spawn_openai").expect("spawn result");
    assert_ne!(result["is_error"], json!(true), "{result}");
    assert_eq!(request_json(&follow_up)["model"], CLAUDE_PARENT_MODEL);
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn openai_parent_spawns_openai_child_with_explicit_provider() -> Result<()> {
    skip_if_no_network!(Ok(()));
    let server = start_mock_server().await;
    let parent = mount_sse_once_match(
        &server,
        |request: &Request| {
            body_contains(request, PARENT_PROMPT) && !body_contains(request, "call-spawn-openai")
        },
        openai_spawn_turn(&[(
            "call-spawn-openai",
            spawn_args_without_fork(
                "openai_worker",
                OPENAI_CHILD_PROMPT,
                OPENAI_PROVIDER_ID,
                OPENAI_CHILD_MODEL,
            ),
        )]),
    )
    .await;
    let child = mount_sse_once_match(
        &server,
        |request: &Request| {
            body_contains(request, OPENAI_CHILD_PROMPT)
                && !body_contains(request, "call-spawn-openai")
        },
        sse(vec![
            ev_response_created("resp-child-1"),
            ev_assistant_message("msg-child-1", "openai child done"),
            ev_completed("resp-child-1"),
        ]),
    )
    .await;
    mount_responses_fallback(&server).await;
    let test = build(&server, OPENAI_PROVIDER_ID, OPENAI_PARENT_MODEL).await?;

    test.submit_turn(PARENT_PROMPT).await?;

    assert_eq!(
        parent.single_request().body_json()["model"],
        OPENAI_PARENT_MODEL
    );
    let child_request = wait_for_responses_request(&child, uses_model(OPENAI_CHILD_MODEL)).await?;
    assert_openai_auth_only(&child_request);
    assert!(child_request.body_contains_text(OPENAI_CHILD_PROMPT));
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn claude_parent_spawns_claude_child_with_different_model() -> Result<()> {
    skip_if_no_network!(Ok(()));
    let server = start_mock_server().await;
    let parent = mount_messages_once(
        &server,
        |request| body_contains(request, PARENT_PROMPT) && !has_tool_result(request),
        anthropic_tool_use_sse(
            "msg_parent",
            CLAUDE_PARENT_MODEL,
            &[(
                "toolu_spawn_claude",
                ANTHROPIC_SPAWN_TOOL,
                spawn_args(
                    "claude_worker",
                    CLAUDE_CHILD_PROMPT,
                    ANTHROPIC_PROVIDER_ID,
                    CLAUDE_CHILD_MODEL,
                ),
            )],
        ),
    )
    .await;
    let child = mount_messages_once(
        &server,
        |request| {
            request_json_lossy(request)["model"] == CLAUDE_CHILD_MODEL
                && body_contains(request, CLAUDE_CHILD_PROMPT)
        },
        anthropic_text_sse("msg_child", CLAUDE_CHILD_MODEL, "claude child done"),
    )
    .await;
    mount_responses_fallback(&server).await;
    mount_messages_fallback(
        &server,
        anthropic_text_sse("msg_extra", CLAUDE_PARENT_MODEL, "ok"),
    )
    .await;
    let test = build(&server, ANTHROPIC_PROVIDER_ID, CLAUDE_PARENT_MODEL).await?;

    test.submit_turn(PARENT_PROMPT).await?;

    assert_eq!(
        request_json(&parent.single_request())["model"],
        CLAUDE_PARENT_MODEL
    );
    let child_request = child.wait_for_request().await?;
    assert_anthropic_auth_only(&child_request);
    assert_eq!(request_json(&child_request)["model"], CLAUDE_CHILD_MODEL);
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn parent_runs_children_on_different_providers_concurrently() -> Result<()> {
    skip_if_no_network!(Ok(()));
    let server = start_mock_server().await;
    mount_sse_once_match(
        &server,
        |request: &Request| {
            body_contains(request, PARENT_PROMPT) && !body_contains(request, "call-spawn-a")
        },
        openai_spawn_turn(&[
            (
                "call-spawn-a",
                spawn_args(
                    "claude_worker",
                    CLAUDE_CHILD_PROMPT,
                    ANTHROPIC_PROVIDER_ID,
                    CLAUDE_CHILD_MODEL,
                ),
            ),
            (
                "call-spawn-b",
                spawn_args_without_fork(
                    "openai_worker",
                    OPENAI_CHILD_PROMPT,
                    OPENAI_PROVIDER_ID,
                    OPENAI_CHILD_MODEL,
                ),
            ),
            (
                "call-spawn-c",
                spawn_args(
                    "subscription_worker",
                    "never runs",
                    CLAUDE_CODE_PROVIDER_ID,
                    CLAUDE_CHILD_MODEL,
                ),
            ),
        ]),
    )
    .await;
    let parent_follow_up = mount_sse_once_match(
        &server,
        |request: &Request| body_contains(request, "call-spawn-a"),
        sse(vec![
            ev_response_created("resp-parent-2"),
            ev_assistant_message("msg-parent-2", "parent done"),
            ev_completed("resp-parent-2"),
        ]),
    )
    .await;
    let claude_child = mount_messages_once(
        &server,
        |request| body_contains(request, CLAUDE_CHILD_PROMPT),
        anthropic_text_sse("msg_child", CLAUDE_CHILD_MODEL, "claude child done"),
    )
    .await;
    let openai_child = mount_sse_once_match(
        &server,
        |request: &Request| {
            body_contains(request, OPENAI_CHILD_PROMPT) && !body_contains(request, "call-spawn-a")
        },
        sse(vec![
            ev_response_created("resp-child-1"),
            ev_assistant_message("msg-child-1", "openai child done"),
            ev_completed("resp-child-1"),
        ]),
    )
    .await;
    mount_responses_fallback(&server).await;
    mount_messages_fallback(
        &server,
        anthropic_text_sse("msg_extra", CLAUDE_CHILD_MODEL, "ok"),
    )
    .await;
    let test = build(&server, OPENAI_PROVIDER_ID, OPENAI_PARENT_MODEL).await?;

    test.submit_turn(PARENT_PROMPT).await?;

    let claude_request = claude_child.wait_for_request().await?;
    assert_anthropic_auth_only(&claude_request);
    assert_eq!(request_json(&claude_request)["model"], CLAUDE_CHILD_MODEL);
    let openai_request =
        wait_for_responses_request(&openai_child, uses_model(OPENAI_CHILD_MODEL)).await?;
    assert_openai_auth_only(&openai_request);
    assert!(openai_request.body_contains_text(OPENAI_CHILD_PROMPT));

    // Claude Code was never connected, so that spawn fails loudly instead of switching providers.
    let follow_up =
        wait_for_responses_request(&parent_follow_up, answers_call("call-spawn-c")).await?;
    let rejected = follow_up
        .function_call_output_text("call-spawn-c")
        .expect("claude-code spawn output");
    assert!(rejected.contains("not connected"), "{rejected}");
    assert!(
        rejected.contains("codex login --provider claude-code"),
        "{rejected}"
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn openai_child_of_a_claude_parent_keeps_plaintext_messages() {
    skip_if_no_network!();
    let server = start_mock_server().await;
    mount_messages_once(
        &server,
        |request| body_contains(request, PARENT_PROMPT) && !has_tool_result(request),
        anthropic_tool_use_sse(
            "msg_parent",
            CLAUDE_PARENT_MODEL,
            &[(
                "toolu_spawn_openai",
                ANTHROPIC_SPAWN_TOOL,
                spawn_args_without_fork(
                    "openai_worker",
                    OPENAI_CHILD_PROMPT,
                    OPENAI_PROVIDER_ID,
                    OPENAI_V2_CHILD_MODEL,
                ),
            )],
        ),
    )
    .await;
    let child = mount_sse_once_match(
        &server,
        |request: &Request| body_contains(request, OPENAI_CHILD_PROMPT),
        sse(vec![
            ev_response_created("resp-child-1"),
            ev_assistant_message("msg-child-1", "openai child done"),
            ev_completed("resp-child-1"),
        ]),
    )
    .await;
    mount_responses_fallback(&server).await;
    mount_messages_fallback(
        &server,
        anthropic_text_sse("msg_extra", CLAUDE_PARENT_MODEL, "ok"),
    )
    .await;
    // The parent runs on a custom Claude provider id with a gateway token, and no built-in Claude
    // provider is connected, so the child cannot infer plaintext mode from connected providers.
    let test = test_codex()
        .with_auth(codex_login::CodexAuth::from_api_key(OPENAI_TEST_KEY))
        .with_config(|config| {
            route_providers_to_mock(config, OPENAI_PROVIDER_ID, CLAUDE_PARENT_MODEL);
            let mut gateway = config.model_providers[ANTHROPIC_PROVIDER_ID].clone();
            gateway.name = "Claude gateway".to_string();
            gateway.experimental_bearer_token = Some("gateway-token".into());
            config
                .model_providers
                .insert("claude-gateway".to_string(), gateway.clone());
            config.model_provider_id = "claude-gateway".to_string();
            config.model_provider = gateway;
            // Leaf agents get no collaboration tools; allow the child to have them.
            config.agent_max_depth = 3;
            enable_multi_agent_v2(config);
        })
        .build_with_auto_env(&server)
        .await
        .expect("build");

    test.submit_turn(PARENT_PROMPT).await.expect("turn");

    let child_request = wait_for_responses_request(&child, uses_model(OPENAI_V2_CHILD_MODEL))
        .await
        .expect("child request");
    // Tools may arrive in `tools` or as additional tools in the input (code mode), so search the
    // whole request for the collaboration tool declarations.
    let body = child_request.body_json();
    let mut declarations = Vec::new();
    find_tool_declarations(&body, "send_message", &mut declarations);
    find_tool_declarations(&body, "followup_task", &mut declarations);
    assert!(!declarations.is_empty(), "collaboration tools: {body}");
    for declaration in declarations {
        assert!(
            !declaration.to_string().contains("\"encrypted\":true"),
            "the child must not ask OpenAI to encrypt messages its Claude parent has to read: \
{declaration}"
        );
    }
}

fn find_tool_declarations(value: &Value, name: &str, found: &mut Vec<Value>) {
    match value {
        Value::Object(map) => {
            if map.get("name").and_then(Value::as_str) == Some(name)
                && (map.contains_key("parameters") || map.contains_key("input_schema"))
            {
                found.push(value.clone());
            }
            for child in map.values() {
                find_tool_declarations(child, name, found);
            }
        }
        Value::Array(items) => {
            for item in items {
                find_tool_declarations(item, name, found);
            }
        }
        _ => {}
    }
}
