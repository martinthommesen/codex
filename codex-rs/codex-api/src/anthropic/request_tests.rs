use super::*;
use codex_protocol::models::LocalShellAction;
use codex_protocol::models::LocalShellExecAction;
use codex_protocol::models::LocalShellStatus;
use pretty_assertions::assert_eq;
use serde_json::json;

fn user(text: &str) -> ResponseItem {
    ResponseItem::Message {
        encrypted_content: None,
        status: None,
        id: None,
        role: "user".to_string(),
        content: vec![ContentItem::InputText {
            text: text.to_string(),
        }],
        phase: None,
        internal_chat_message_metadata_passthrough: None,
    }
}

fn developer(text: &str) -> ResponseItem {
    ResponseItem::Message {
        encrypted_content: None,
        status: None,
        id: None,
        role: "developer".to_string(),
        content: vec![ContentItem::InputText {
            text: text.to_string(),
        }],
        phase: None,
        internal_chat_message_metadata_passthrough: None,
    }
}

fn assistant(text: &str) -> ResponseItem {
    ResponseItem::Message {
        encrypted_content: None,
        status: None,
        id: None,
        role: "assistant".to_string(),
        content: vec![ContentItem::OutputText {
            text: text.to_string(),
            annotations: None,
            logprobs: None,
        }],
        phase: None,
        internal_chat_message_metadata_passthrough: None,
    }
}

fn function_call(call_id: &str, name: &str, arguments: &str) -> ResponseItem {
    ResponseItem::FunctionCall {
        encrypted_content: None,
        status: None,
        id: None,
        name: name.to_string(),
        namespace: None,
        arguments: arguments.to_string(),
        encrypted_function_args: None,
        call_id: call_id.to_string(),
        internal_chat_message_metadata_passthrough: None,
    }
}

fn function_output(call_id: &str, text: &str, success: Option<bool>) -> ResponseItem {
    ResponseItem::FunctionCallOutput {
        id: None,
        call_id: Some(call_id.to_string()),
        name: None,
        namespace: None,
        output: FunctionCallOutputPayload {
            body: FunctionCallOutputBody::Text(text.to_string()),
            success,
        },
        internal_chat_message_metadata_passthrough: None,
    }
}

fn reasoning(summary: &str, encrypted: Option<&str>) -> ResponseItem {
    ResponseItem::Reasoning {
        id: None,
        summary: if summary.is_empty() {
            Vec::new()
        } else {
            vec![ReasoningItemReasoningSummary::SummaryText {
                text: summary.to_string(),
            }]
        },
        content: None,
        encrypted_content: encrypted.map(str::to_string),
        internal_chat_message_metadata_passthrough: None,
    }
}

fn options() -> AnthropicRequestOptions {
    AnthropicRequestOptions {
        model: "claude-opus-5-5".to_string(),
        max_tokens: 32_000,
        thinking: AnthropicThinking::Adaptive,
        effort: Some("high"),
        parallel_tool_calls: true,
        prompt_caching: true,
        output_schema: None,
        drop_mismatched_thinking: true,
        strip_thinking: false,
    }
}

#[test]
fn builds_tool_round_trip_with_thinking_replay() {
    let input = vec![
        developer("Follow the sandbox rules."),
        user("list files"),
        reasoning("I should run ls.", Some("anthropic.thinking.v1:sig-1")),
        assistant("Running ls."),
        function_call("toolu_1", "shell", r#"{"command":["ls"]}"#),
        function_output("toolu_1", "a.txt\nb.txt", Some(true)),
    ];
    let tools = vec![json!({
        "type": "function",
        "name": "shell",
        "description": "Run a command",
        "strict": false,
        "parameters": {"type": "object", "properties": {"command": {"type": "array"}}}
    })];

    let prepared = build_anthropic_request("You are Codex.", &input, &tools, &options());
    let body = serde_json::to_value(&prepared.request).expect("serialize request");

    assert_eq!(
        body,
        json!({
            "model": "claude-opus-5-5",
            "max_tokens": 32000,
            "system": [
                {"type": "text", "text": "You are Codex."},
                {"type": "text", "text": "Follow the sandbox rules."}
            ],
            "messages": [
                {"role": "user", "content": [{"type": "text", "text": "list files"}]},
                {"role": "assistant", "content": [
                    {"type": "thinking", "thinking": "I should run ls.", "signature": "sig-1"},
                    {"type": "text", "text": "Running ls."},
                    {"type": "tool_use", "id": "toolu_1", "name": "shell", "input": {"command": ["ls"]}}
                ]},
                {"role": "user", "content": [
                    {"type": "tool_result", "tool_use_id": "toolu_1", "content": "a.txt\nb.txt"}
                ]}
            ],
            "tools": [{
                "name": "shell",
                "description": "Run a command",
                "input_schema": {"type": "object", "properties": {"command": {"type": "array"}}}
            }],
            "tool_choice": {"type": "auto"},
            "thinking": {
                "type": "adaptive",
                "display": "summarized",
                "block_binding": {"prefix_mismatch_behavior": "drop_block"}
            },
            "output_config": {"effort": "high"},
            "cache_control": {"type": "ephemeral"},
            "stream": true
        })
    );
    assert_eq!(prepared.betas, vec![THINKING_BINDING_BETA]);
    assert!(prepared.notes.is_empty(), "{:?}", prepared.notes);
}

#[test]
fn drops_foreign_reasoning_and_strips_thinking_on_request() {
    let input = vec![
        user("hi"),
        reasoning("openai summary", Some("gAAAA-openai-encrypted")),
        reasoning("claude thought", Some("anthropic.thinking.v1:sig")),
        assistant("hello"),
        user("again"),
    ];
    let normal = translate_history(&input, /*strip_thinking*/ false);
    assert_eq!(
        normal.messages[1].content,
        vec![
            json!({"type": "thinking", "thinking": "claude thought", "signature": "sig"}),
            json!({"type": "text", "text": "hello"}),
        ]
    );
    assert_eq!(normal.notes.len(), 1);

    let stripped = translate_history(&input, /*strip_thinking*/ true);
    assert_eq!(
        stripped.messages[1].content,
        vec![json!({"type": "text", "text": "hello"})]
    );
}

#[test]
fn failed_tool_output_sets_is_error_and_missing_results_are_synthesized() {
    let input = vec![
        user("go"),
        function_call("call_a", "shell", r#"{"command":["false"]}"#),
        function_call("call_b", "shell", r#"{"command":["true"]}"#),
        function_output("call_a", "exit 1", Some(false)),
        user("next"),
    ];
    let history = translate_history(&input, /*strip_thinking*/ false);
    assert_eq!(
        history.messages[2].content,
        vec![
            json!({"type": "tool_result", "tool_use_id": "call_a", "content": "exit 1", "is_error": true}),
            json!({"type": "tool_result", "tool_use_id": "call_b", "content": "Tool call did not complete.", "is_error": true}),
            json!({"type": "text", "text": "next"}),
        ]
    );
}

#[test]
fn custom_tool_calls_round_trip_through_input_property() {
    let input = vec![
        user("patch it"),
        ResponseItem::CustomToolCall {
            encrypted_content: None,
            id: None,
            status: None,
            call_id: "call_patch".to_string(),
            name: "apply_patch".to_string(),
            namespace: None,
            input: "*** Begin Patch\n*** End Patch".to_string(),
            internal_chat_message_metadata_passthrough: None,
        },
        ResponseItem::CustomToolCallOutput {
            id: None,
            call_id: "call_patch".to_string(),
            name: None,
            output: FunctionCallOutputPayload {
                body: FunctionCallOutputBody::Text("Done".to_string()),
                success: Some(true),
            },
            internal_chat_message_metadata_passthrough: None,
        },
    ];
    let history = translate_history(&input, /*strip_thinking*/ false);
    assert_eq!(
        history.messages[1].content,
        vec![json!({
            "type": "tool_use",
            "id": "call_patch",
            "name": "apply_patch",
            "input": {"input": "*** Begin Patch\n*** End Patch"}
        })]
    );
    // The last turn is the tool result, so no continuation request is appended.
    assert_eq!(history.messages.len(), 3);
}

#[test]
fn foreign_provider_items_become_text_with_notes() {
    let input = vec![
        user("run it"),
        ResponseItem::LocalShellCall {
            encrypted_content: None,
            id: None,
            call_id: Some("ls_1".to_string()),
            status: LocalShellStatus::Completed,
            action: LocalShellAction::Exec(LocalShellExecAction {
                command: vec!["ls".to_string()],
                timeout_ms: None,
                working_directory: None,
                env: None,
                user: None,
            }),
            internal_chat_message_metadata_passthrough: None,
        },
        function_output("ls_1", "a.txt", Some(true)),
        ResponseItem::Compaction {
            id: None,
            encrypted_content: "opaque".to_string(),
            internal_chat_message_metadata_passthrough: None,
        },
    ];
    let history = translate_history(&input, /*strip_thinking*/ false);
    assert_eq!(history.messages.len(), 3);
    assert_eq!(history.messages[1].role, "assistant");
    let user_turn = serde_json::to_string(&history.messages[2]).expect("serialize");
    assert!(user_turn.contains("[output of call ls_1]"));
    assert!(user_turn.contains("compacted by another provider"));
    assert!(!history.notes.is_empty());
}

#[test]
fn images_become_base64_blocks() {
    let input = vec![ResponseItem::Message {
        encrypted_content: None,
        status: None,
        id: None,
        role: "user".to_string(),
        content: vec![
            ContentItem::InputText {
                text: "what is this".to_string(),
            },
            ContentItem::InputImage {
                image: ImageReference::Inline {
                    image_url: "data:image/png;base64,AAAA".to_string(),
                },
                detail: None,
            },
        ],
        phase: None,
        internal_chat_message_metadata_passthrough: None,
    }];
    let history = translate_history(&input, /*strip_thinking*/ false);
    assert_eq!(
        history.messages[0].content[1],
        json!({"type": "image", "source": {"type": "base64", "media_type": "image/png", "data": "AAAA"}})
    );
}

#[test]
fn trailing_assistant_turn_gets_continuation_and_parallel_flag_is_respected() {
    let input = vec![user("hi"), assistant("partial")];
    let mut opts = options();
    opts.parallel_tool_calls = false;
    opts.thinking = AnthropicThinking::Disabled;
    opts.effort = None;
    let tools = vec![
        json!({"type": "function", "name": "t", "description": "", "strict": false, "parameters": {"type": "object", "properties": {}}}),
    ];
    let prepared = build_anthropic_request("", &input, &tools, &opts);
    let body = serde_json::to_value(&prepared.request).expect("serialize");
    assert_eq!(
        body["messages"][2],
        json!({"role": "user", "content": [{"type": "text", "text": "Continue."}]})
    );
    assert_eq!(
        body["tool_choice"],
        json!({"type": "auto", "disable_parallel_tool_use": true})
    );
    assert_eq!(body["thinking"], json!({"type": "disabled"}));
    assert!(body.get("system").is_none());
    assert!(prepared.betas.is_empty());
}

#[test]
fn identifies_anthropic_reasoning_items() {
    assert!(is_anthropic_reasoning_item(&reasoning(
        "x",
        Some("anthropic.thinking.v1:abc")
    )));
    assert!(is_anthropic_reasoning_item(&reasoning(
        "",
        Some("anthropic.redacted_thinking.v1:abc")
    )));
    assert!(!is_anthropic_reasoning_item(&reasoning("x", Some("gAAAA"))));
    assert!(!is_anthropic_reasoning_item(&user("hi")));
}

#[test]
fn tool_blocks_become_text_when_no_tools_are_declared() {
    let input = vec![
        user("go"),
        reasoning("thinking", Some("anthropic.thinking.v1:sig")),
        function_call("call_1", "shell", r#"{"command":["ls"]}"#),
        function_output("call_1", "a.txt", Some(true)),
        user("summarize"),
    ];
    let prepared = build_anthropic_request("", &input, &[], &options());
    let body = serde_json::to_value(&prepared.request).expect("serialize");
    assert!(body.get("tools").is_none());
    assert!(body.get("tool_choice").is_none());
    assert_eq!(
        body["messages"],
        json!([
            {"role": "user", "content": [{"type": "text", "text": "go"}]},
            {"role": "assistant", "content": [
                {"type": "text", "text": "[called tool shell with {\"command\":[\"ls\"]}]"}
            ]},
            {"role": "user", "content": [
                {"type": "text", "text": "[tool result]\na.txt"},
                {"type": "text", "text": "summarize"}
            ]}
        ])
    );
}
