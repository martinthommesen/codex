#![cfg(unix)]
#![allow(clippy::expect_used)]
//! End-to-end tests of `ClaudeCodeSession` against the fake Claude Code CLI.

use codex_api::ApiError;
use codex_api::ResponseEvent;
use codex_api::ResponseStream;
use codex_claude_code::ClaudeCodeSession;
use codex_claude_code::ClaudeCodeSessionConfig;
use codex_claude_code::ClaudeCodeTurnRequest;
use codex_protocol::models::ContentItem;
use codex_protocol::models::FunctionCallOutputPayload;
use codex_protocol::models::ResponseItem;
use pretty_assertions::assert_eq;
use serde_json::Value;
use serde_json::json;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;
use tempfile::TempDir;
use tokio::time::timeout;

const THREAD_ID: &str = "0199a8c4-5a2b-7c3d-8e4f-001122334455";
const MODEL: &str = "claude-fake-model";
const EVENT_TIMEOUT: Duration = Duration::from_secs(30);

struct Fixture {
    _dir: TempDir,
    cwd: PathBuf,
    state_dir: PathBuf,
}

impl Fixture {
    fn new() -> Self {
        let dir = tempfile::tempdir().expect("tempdir");
        let cwd = dir.path().join("work");
        let state_dir = dir.path().join("state");
        std::fs::create_dir_all(&cwd).expect("cwd");
        Self {
            _dir: dir,
            cwd,
            state_dir,
        }
    }

    fn config(&self) -> ClaudeCodeSessionConfig {
        ClaudeCodeSessionConfig {
            command: codex_utils_cargo_bin::cargo_bin("codex-claude-code-fake")
                .expect("fake claude binary"),
            // The fake connects to the socket directly, so the bridge command is never run.
            bridge_command: PathBuf::from("/nonexistent/codex"),
            bridge_args: vec!["stdio-to-uds".to_string()],
            cwd: self.cwd.clone(),
            thread_id: THREAD_ID.to_string(),
            state_dir: self.state_dir.clone(),
        }
    }

    fn records(&self, event: &str) -> Vec<Value> {
        let Ok(contents) = std::fs::read_to_string(self.cwd.join("fake-claude-record.jsonl"))
        else {
            return Vec::new();
        };
        contents
            .lines()
            .filter_map(|line| serde_json::from_str::<Value>(line).ok())
            .filter(|record| record.get("event").and_then(Value::as_str) == Some(event))
            .collect()
    }

    fn spawn_argvs(&self) -> Vec<Vec<String>> {
        self.records("spawn")
            .iter()
            .map(|record| {
                record["argv"]
                    .as_array()
                    .expect("argv")
                    .iter()
                    .map(|arg| arg.as_str().expect("arg").to_string())
                    .collect()
            })
            .collect()
    }

    fn user_message_texts(&self) -> Vec<Vec<String>> {
        self.records("user_message")
            .iter()
            .map(|record| {
                record["content"]
                    .as_array()
                    .expect("content")
                    .iter()
                    .filter_map(|block| block.get("text").and_then(Value::as_str))
                    .map(str::to_string)
                    .collect()
            })
            .collect()
    }
}

fn option_value(argv: &[String], name: &str) -> Option<String> {
    argv.iter()
        .position(|arg| arg == name)
        .and_then(|index| argv.get(index + 1))
        .cloned()
}

fn message(role: &str, text: &str) -> ResponseItem {
    let content = if role == "assistant" {
        ContentItem::OutputText {
            text: text.to_string(),
            annotations: None,
            logprobs: None,
        }
    } else {
        ContentItem::InputText {
            text: text.to_string(),
        }
    };
    ResponseItem::Message {
        encrypted_content: None,
        status: None,
        id: None,
        role: role.to_string(),
        content: vec![content],
        phase: None,
        internal_chat_message_metadata_passthrough: None,
    }
}

fn shell_tool() -> Value {
    json!({
        "type": "function",
        "name": "shell",
        "description": "Runs a shell command.",
        "strict": false,
        "parameters": {
            "type": "object",
            "properties": {"command": {"type": "string"}},
            "required": ["command"],
        },
    })
}

fn request(input: &[ResponseItem]) -> ClaudeCodeTurnRequest {
    ClaudeCodeTurnRequest {
        model: MODEL.to_string(),
        effort: None,
        base_instructions: "You are Codex.".to_string(),
        input: input.to_vec(),
        tools: vec![shell_tool()],
    }
}

async fn collect(mut stream: ResponseStream) -> Vec<ResponseEvent> {
    let mut events = Vec::new();
    loop {
        match timeout(EVENT_TIMEOUT, stream.rx_event.recv()).await {
            Ok(Some(Ok(event))) => events.push(event),
            Ok(Some(Err(err))) => panic!("stream failed: {err}; events so far: {events:?}"),
            Ok(None) => return events,
            Err(_) => panic!("timed out waiting for events; events so far: {events:?}"),
        }
    }
}

async fn run_turn(session: &ClaudeCodeSession, input: &[ResponseItem]) -> Vec<ResponseEvent> {
    let stream = session.stream(request(input)).await.expect("stream");
    collect(stream).await
}

/// Runs a turn that must end with an error.
async fn run_failing_turn(session: &ClaudeCodeSession, input: &[ResponseItem]) {
    let mut stream = session.stream(request(input)).await.expect("stream");
    let mut failed = false;
    while let Some(event) = timeout(EVENT_TIMEOUT, stream.rx_event.recv())
        .await
        .expect("events")
    {
        failed |= event.is_err();
    }
    assert!(failed, "the turn did not fail");
}

/// Waits for the `[hang]` scenario's partial answer, then drops the stream.
async fn drop_after_partial_answer(mut stream: ResponseStream) {
    loop {
        match timeout(EVENT_TIMEOUT, stream.rx_event.recv()).await {
            Ok(Some(Ok(ResponseEvent::OutputTextDelta(delta)))) => {
                assert_eq!(delta, "partial");
                return;
            }
            Ok(Some(Ok(_))) => {}
            other => panic!("unexpected stream state: {other:?}"),
        }
    }
}

/// Output items in the order Codex would record them.
fn output_items(events: &[ResponseEvent]) -> Vec<ResponseItem> {
    events
        .iter()
        .filter_map(|event| match event {
            ResponseEvent::OutputItemDone(item) => Some(item.clone()),
            _ => None,
        })
        .collect()
}

fn assistant_texts(events: &[ResponseEvent]) -> Vec<String> {
    output_items(events)
        .into_iter()
        .filter_map(|item| match item {
            ResponseItem::Message { role, content, .. } if role == "assistant" => Some(
                content
                    .into_iter()
                    .filter_map(|content| match content {
                        ContentItem::OutputText { text, .. } => Some(text),
                        _ => None,
                    })
                    .collect::<String>(),
            ),
            _ => None,
        })
        .collect()
}

fn end_turn(events: &[ResponseEvent]) -> Option<Option<bool>> {
    match events.last() {
        Some(ResponseEvent::Completed { end_turn, .. }) => Some(*end_turn),
        _ => None,
    }
}

#[tokio::test]
async fn tool_round_trip_across_two_segments() {
    let fixture = Fixture::new();
    let session = ClaudeCodeSession::new(fixture.config());
    let mut input = vec![
        message("developer", "Be brief."),
        message("user", "please [tool]"),
    ];

    let first = run_turn(&session, &input).await;
    assert_eq!(
        assistant_texts(&first),
        vec!["Calling the tool.".to_string()]
    );
    let calls: Vec<(String, Option<String>, String, Value)> = output_items(&first)
        .into_iter()
        .filter_map(|item| match item {
            ResponseItem::FunctionCall {
                name,
                namespace,
                call_id,
                arguments,
                ..
            } => Some((
                name,
                namespace,
                call_id,
                serde_json::from_str(&arguments).expect("arguments"),
            )),
            _ => None,
        })
        .collect();
    assert_eq!(
        calls,
        vec![(
            "shell".to_string(),
            None,
            "toolu_fake_1".to_string(),
            json!({"command": "echo hi"}),
        )]
    );
    assert_eq!(end_turn(&first), Some(None));

    input.extend(output_items(&first));
    input.push(ResponseItem::FunctionCallOutput {
        id: None,
        call_id: Some("toolu_fake_1".to_string()),
        name: None,
        namespace: None,
        output: FunctionCallOutputPayload::from_text("hi from codex".to_string()),
        internal_chat_message_metadata_passthrough: None,
    });
    let second = run_turn(&session, &input).await;
    assert_eq!(
        assistant_texts(&second),
        vec!["Tool said: hi from codex".to_string()]
    );
    assert_eq!(end_turn(&second), Some(Some(true)));

    let tool_results = fixture.records("tool_result");
    assert_eq!(tool_results.len(), 1);
    assert_eq!(
        tool_results[0]["response"]["result"],
        json!({"content": [{"type": "text", "text": "hi from codex"}], "isError": false})
    );
    assert_eq!(
        fixture.records("tools")[0]["names"],
        json!(["shell"]),
        "Codex tools are listed under their MCP names"
    );

    let argvs = fixture.spawn_argvs();
    assert_eq!(argvs.len(), 1, "one process serves both segments");
    let argv = &argvs[0];
    for (flag, value) in [
        ("--tools", ""),
        ("--allowedTools", "mcp__codex"),
        ("--permission-mode", "dontAsk"),
        ("--setting-sources", ""),
        ("--model", MODEL),
        ("--session-id", THREAD_ID),
        ("--input-format", "stream-json"),
        ("--output-format", "stream-json"),
    ] {
        assert_eq!(
            option_value(argv, flag).as_deref(),
            Some(value),
            "flag {flag}"
        );
    }
    for flag in [
        "--strict-mcp-config",
        "--include-partial-messages",
        "--disable-slash-commands",
    ] {
        assert!(argv.iter().any(|arg| arg == flag), "missing {flag}");
    }
    let prompt_file = option_value(argv, "--system-prompt-file").expect("system prompt file");
    assert_eq!(
        std::fs::read_to_string(prompt_file).expect("prompt"),
        "You are Codex.\n\nBe brief."
    );
    // The leading developer message is part of the system prompt, not the user turn.
    assert_eq!(
        fixture.user_message_texts(),
        vec![vec!["please [tool]".to_string()]]
    );
    assert_eq!(
        fixture.records("spawn")[0]["enable_tool_search"],
        json!("false")
    );
}

#[tokio::test]
async fn second_user_turn_reuses_the_live_process() {
    let fixture = Fixture::new();
    let session = ClaudeCodeSession::new(fixture.config());
    let mut input = vec![message("user", "hello")];
    let first = run_turn(&session, &input).await;
    assert_eq!(assistant_texts(&first), vec!["echo: hello".to_string()]);
    assert_eq!(end_turn(&first), Some(Some(true)));

    input.extend(output_items(&first));
    input.push(message("user", "again"));
    let second = run_turn(&session, &input).await;
    assert_eq!(assistant_texts(&second), vec!["echo: again".to_string()]);
    assert_eq!(end_turn(&second), Some(Some(true)));

    assert_eq!(fixture.spawn_argvs().len(), 1);
    assert_eq!(
        fixture.user_message_texts(),
        vec![vec!["hello".to_string()], vec!["again".to_string()]]
    );
}

#[tokio::test]
async fn persisted_session_resumes_in_a_new_session_object() {
    let fixture = Fixture::new();
    let mut input = vec![message("user", "hello")];
    {
        let session = ClaudeCodeSession::new(fixture.config());
        let first = run_turn(&session, &input).await;
        input.extend(output_items(&first));
    }

    input.push(message("user", "after restart"));
    let session = ClaudeCodeSession::new(fixture.config());
    let events = run_turn(&session, &input).await;
    assert_eq!(
        assistant_texts(&events),
        vec!["echo: after restart".to_string()]
    );

    let argvs = fixture.spawn_argvs();
    assert_eq!(argvs.len(), 2);
    assert_eq!(
        option_value(&argvs[1], "--resume").as_deref(),
        Some(THREAD_ID)
    );
    assert_eq!(option_value(&argvs[1], "--session-id"), None);
    assert_eq!(
        fixture.user_message_texts(),
        vec![vec!["hello".to_string()], vec!["after restart".to_string()]]
    );
}

#[tokio::test]
async fn dropped_stream_interrupts_and_next_turn_resumes() {
    let fixture = Fixture::new();
    let session = ClaudeCodeSession::new(fixture.config());
    let mut input = vec![message("user", "[hang]")];
    let stream = session.stream(request(&input)).await.expect("stream");
    drop_after_partial_answer(stream).await;

    input.push(message("user", "next"));
    let events = run_turn(&session, &input).await;
    assert_eq!(assistant_texts(&events), vec!["echo: next".to_string()]);
    assert_eq!(fixture.records("interrupt").len(), 1);

    let argvs = fixture.spawn_argvs();
    assert_eq!(argvs.len(), 2);
    assert_eq!(
        option_value(&argvs[1], "--resume").as_deref(),
        Some(THREAD_ID)
    );
}

#[tokio::test]
async fn an_interrupt_the_cli_survives_reuses_the_live_process() {
    let fixture = Fixture::new();
    std::fs::write(fixture.cwd.join("fake-claude-survive-interrupt"), "").expect("flag");
    let session = ClaudeCodeSession::new(fixture.config());
    let mut input = vec![message("user", "[hang]")];
    let stream = session.stream(request(&input)).await.expect("stream");
    drop_after_partial_answer(stream).await;

    input.push(message("user", "next"));
    let events = run_turn(&session, &input).await;
    assert_eq!(assistant_texts(&events), vec!["echo: next".to_string()]);
    assert_eq!(fixture.records("interrupt").len(), 1);
    assert_eq!(
        fixture.spawn_argvs().len(),
        1,
        "the interrupted process is reused"
    );
    assert_eq!(
        fixture.user_message_texts(),
        vec![vec!["[hang]".to_string()], vec!["next".to_string()]]
    );
}

#[tokio::test]
async fn a_crash_after_startup_resumes_without_repeating_the_prompt() {
    let fixture = Fixture::new();
    let session = ClaudeCodeSession::new(fixture.config());
    let input = vec![message("user", "hello")];
    let fail_flag = fixture.cwd.join("fake-claude-fail-turn");
    std::fs::write(&fail_flag, "crash-after-init").expect("flag");
    run_failing_turn(&session, &input).await;
    std::fs::remove_file(&fail_flag).expect("remove flag");

    // Claude Code recorded the prompt before it crashed, so the retry resumes that session and
    // asks it to continue instead of sending the prompt a second time.
    let events = run_turn(&session, &input).await;
    assert_eq!(
        assistant_texts(&events),
        vec!["echo: Continue.".to_string()]
    );
    let argvs = fixture.spawn_argvs();
    assert_eq!(argvs.len(), 2);
    assert_eq!(
        option_value(&argvs[1], "--resume").as_deref(),
        Some(THREAD_ID)
    );
    assert_eq!(
        fixture.user_message_texts(),
        vec![vec!["hello".to_string()], vec!["Continue.".to_string()]]
    );
}

#[tokio::test]
async fn an_error_result_does_not_resend_the_prompt() {
    let fixture = Fixture::new();
    let session = ClaudeCodeSession::new(fixture.config());
    let input = vec![message("user", "hello")];
    let fail_flag = fixture.cwd.join("fake-claude-fail-turn");
    std::fs::write(&fail_flag, "error-after-init").expect("flag");
    run_failing_turn(&session, &input).await;
    std::fs::remove_file(&fail_flag).expect("remove flag");

    // The failed query is part of the live session already.
    let events = run_turn(&session, &input).await;
    assert_eq!(
        assistant_texts(&events),
        vec!["echo: Continue.".to_string()]
    );
    assert_eq!(fixture.spawn_argvs().len(), 1);
    assert_eq!(
        fixture.user_message_texts(),
        vec![vec!["hello".to_string()], vec!["Continue.".to_string()]]
    );
}

#[tokio::test]
async fn an_error_before_startup_reseeds_the_retry() {
    let fixture = Fixture::new();
    let session = ClaudeCodeSession::new(fixture.config());
    run_turn(&session, &[message("user", "first question")]).await;

    // A rewritten history needs a fresh session, which fails before Claude Code created it.
    let rewritten = vec![
        message("user", "summary of earlier work"),
        message("assistant", "noted"),
        message("user", "follow up"),
    ];
    let fail_flag = fixture.cwd.join("fake-claude-fail-turn");
    std::fs::write(&fail_flag, "error-before-init").expect("flag");
    run_failing_turn(&session, &rewritten).await;
    std::fs::remove_file(&fail_flag).expect("remove flag");

    let events = run_turn(&session, &rewritten).await;
    assert_eq!(
        assistant_texts(&events),
        vec!["echo: follow up".to_string()]
    );
    let messages = fixture.user_message_texts();
    let reseeded = messages.last().expect("reseeded message");
    assert!(
        reseeded[0].starts_with("<prior_conversation>"),
        "{reseeded:?}"
    );
    assert_eq!(reseeded.last().map(String::as_str), Some("follow up"));
}

#[tokio::test]
async fn trailing_control_items_keep_the_prompt_live() {
    let fixture = Fixture::new();
    let session = ClaudeCodeSession::new(fixture.config());
    let input = vec![
        message("user", "hello"),
        ResponseItem::AdditionalTools {
            id: None,
            role: "developer".to_string(),
            tools: Arc::from(Vec::new()),
        },
    ];
    let events = run_turn(&session, &input).await;
    assert_eq!(assistant_texts(&events), vec!["echo: hello".to_string()]);
    assert_eq!(
        fixture.user_message_texts(),
        vec![vec!["hello".to_string()]]
    );
}

#[tokio::test]
async fn rewritten_history_starts_a_fresh_session_with_a_transcript() {
    let fixture = Fixture::new();
    let session = ClaudeCodeSession::new(fixture.config());
    let input = vec![message("user", "first question")];
    run_turn(&session, &input).await;

    // Compaction replaced the history Claude Code has already seen.
    let rewritten = vec![
        message("user", "summary of earlier work"),
        message("assistant", "noted"),
        message("user", "follow up"),
    ];
    let events = run_turn(&session, &rewritten).await;
    assert_eq!(
        assistant_texts(&events),
        vec!["echo: follow up".to_string()]
    );

    let argvs = fixture.spawn_argvs();
    assert_eq!(argvs.len(), 2);
    let fresh_id = option_value(&argvs[1], "--session-id").expect("fresh session id");
    assert_ne!(fresh_id, THREAD_ID);
    assert_eq!(option_value(&argvs[1], "--resume"), None);

    let messages = fixture.user_message_texts();
    assert_eq!(messages.len(), 2);
    let fresh = &messages[1];
    assert_eq!(fresh.len(), 2);
    assert!(fresh[0].starts_with("<prior_conversation>"), "{fresh:?}");
    assert!(
        fresh[0].contains("[user]\nsummary of earlier work"),
        "{fresh:?}"
    );
    assert!(fresh[0].contains("[assistant]\nnoted"), "{fresh:?}");
    assert!(fresh[0].ends_with("</prior_conversation>"), "{fresh:?}");
    assert_eq!(fresh[1], "follow up");
}

#[tokio::test]
async fn a_session_lost_before_startup_is_reseeded_on_retry() {
    let fixture = Fixture::new();
    let session = ClaudeCodeSession::new(fixture.config());
    run_turn(&session, &[message("user", "first question")]).await;

    // A rewritten history needs a fresh session, and that process dies during startup.
    let rewritten = vec![
        message("user", "summary of earlier work"),
        message("assistant", "noted"),
        message("user", "follow up"),
    ];
    let crash_flag = fixture.cwd.join("fake-claude-crash-before-init");
    std::fs::write(&crash_flag, "").expect("flag");
    run_failing_turn(&session, &rewritten).await;
    std::fs::remove_file(&crash_flag).expect("remove flag");

    // Codex retries the same request; the new process must get the whole conversation again.
    let events = run_turn(&session, &rewritten).await;
    assert_eq!(
        assistant_texts(&events),
        vec!["echo: follow up".to_string()]
    );
    let messages = fixture.user_message_texts();
    let reseeded = messages.last().expect("reseeded message");
    assert!(
        reseeded[0].starts_with("<prior_conversation>"),
        "{reseeded:?}"
    );
    assert_eq!(reseeded.last().map(String::as_str), Some("follow up"));
}

#[tokio::test]
async fn failed_resume_falls_back_to_a_seeded_fresh_session() {
    let fixture = Fixture::new();
    let mut input = vec![message("user", "hello")];
    {
        let session = ClaudeCodeSession::new(fixture.config());
        let first = run_turn(&session, &input).await;
        input.extend(output_items(&first));
    }
    std::fs::write(fixture.cwd.join("fake-claude-fail-resume"), "").expect("flag");

    input.push(message("user", "still there?"));
    let session = ClaudeCodeSession::new(fixture.config());
    let events = run_turn(&session, &input).await;
    assert_eq!(
        assistant_texts(&events),
        vec!["echo: still there?".to_string()]
    );

    let argvs = fixture.spawn_argvs();
    assert_eq!(argvs.len(), 3);
    assert_eq!(
        option_value(&argvs[1], "--resume").as_deref(),
        Some(THREAD_ID)
    );
    let fresh_id = option_value(&argvs[2], "--session-id").expect("fresh session id");
    assert_ne!(fresh_id, THREAD_ID);
    let messages = fixture.user_message_texts();
    let seeded = messages.last().expect("seeded message");
    assert!(seeded[0].contains("[user]\nhello"), "{seeded:?}");
    assert!(seeded[0].contains("[assistant]\necho: hello"), "{seeded:?}");
    assert_eq!(seeded[1], "still there?");
}

#[tokio::test]
async fn unsupported_effort_is_rejected_explicitly() {
    let fixture = Fixture::new();
    let session = ClaudeCodeSession::new(fixture.config());
    let mut turn = request(&[message("user", "hello")]);
    turn.effort = Some("minimal".to_string());
    match session.stream(turn).await {
        Err(ApiError::InvalidRequest { message }) => {
            assert!(message.contains("minimal"), "{message}");
        }
        Err(other) => panic!("unexpected error: {other}"),
        Ok(_) => panic!("unsupported effort was accepted"),
    }
    assert_eq!(fixture.spawn_argvs().len(), 0);
}

#[tokio::test]
async fn missing_cli_reports_install_hint() {
    let fixture = Fixture::new();
    let mut config = fixture.config();
    config.command = fixture.cwd.join("no-such-claude");
    let session = ClaudeCodeSession::new(config);
    match session.stream(request(&[message("user", "hello")])).await {
        Err(ApiError::InvalidRequest { message }) => {
            assert!(message.contains("was not found"), "{message}");
        }
        Err(other) => panic!("unexpected error: {other}"),
        Ok(_) => panic!("missing CLI was accepted"),
    }
}
