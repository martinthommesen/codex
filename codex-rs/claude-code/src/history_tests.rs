use super::Classified;
use super::append_user_blocks;
use super::build_system_prompt;
use super::call_tool_result;
use super::classify;
use super::leading_system;
use super::prefix_hash;
use super::render_transcript;
use super::trailing_user_input_start;
use super::truncate_head;
use super::user_message_line;
use codex_protocol::ResponseItemId;
use codex_protocol::models::ConfigurationReasoning;
use codex_protocol::models::ContentItem;
use codex_protocol::models::FunctionCallOutputBody;
use codex_protocol::models::FunctionCallOutputContentItem;
use codex_protocol::models::FunctionCallOutputPayload;
use codex_protocol::models::ImageReference;
use codex_protocol::models::ResponseItem;
use codex_protocol::openai_models::ReasoningEffort;
use pretty_assertions::assert_eq;
use serde_json::json;
use std::sync::Arc;

fn message(role: &str, text: &str) -> ResponseItem {
    ResponseItem::Message {
        encrypted_content: None,
        status: None,
        id: None,
        role: role.to_string(),
        content: vec![ContentItem::InputText {
            text: text.to_string(),
        }],
        phase: None,
        internal_chat_message_metadata_passthrough: None,
    }
}

fn function_call(call_id: &str) -> ResponseItem {
    ResponseItem::FunctionCall {
        encrypted_content: None,
        status: None,
        id: None,
        name: "shell".to_string(),
        namespace: None,
        arguments: r#"{"command":"ls"}"#.to_string(),
        encrypted_function_args: None,
        call_id: call_id.to_string(),
        internal_chat_message_metadata_passthrough: None,
    }
}

fn function_output(call_id: &str, text: &str) -> ResponseItem {
    ResponseItem::FunctionCallOutput {
        id: None,
        call_id: Some(call_id.to_string()),
        name: None,
        namespace: None,
        output: FunctionCallOutputPayload::from_text(text.to_string()),
        internal_chat_message_metadata_passthrough: None,
    }
}

#[test]
fn prefix_hash_ignores_codex_item_ids() {
    let without_id = message("user", "hi");
    let mut with_id = without_id.clone();
    with_id.set_id(Some(ResponseItemId::new("msg")));
    assert_eq!(
        prefix_hash(std::slice::from_ref(&without_id)),
        prefix_hash(&[with_id])
    );
    assert_ne!(
        prefix_hash(&[without_id]),
        prefix_hash(&[message("user", "hi!")])
    );
}

#[test]
fn leading_developer_messages_are_hoisted() {
    let items = vec![
        message("developer", "rules"),
        ResponseItem::AdditionalTools {
            id: None,
            role: "developer".to_string(),
            tools: Arc::from(Vec::new()),
        },
        message("system", "more rules"),
        message("user", "hi"),
        message("developer", "later"),
    ];
    let (leading_len, hoisted) = leading_system(&items);
    assert_eq!(leading_len, 3);
    assert_eq!(hoisted, vec!["rules".to_string(), "more rules".to_string()]);
    assert_eq!(
        build_system_prompt("base", &hoisted),
        "base\n\nrules\n\nmore rules"
    );
    assert_eq!(build_system_prompt("", &[]), "");
}

#[test]
fn trailing_user_input_stops_at_model_output() {
    let items = vec![
        message("user", "q1"),
        message("assistant", "a1"),
        message("user", "q2"),
        message("developer", "note"),
    ];
    assert_eq!(trailing_user_input_start(&items, 0), 2);
    assert_eq!(trailing_user_input_start(&items, 3), 3);
    assert_eq!(
        trailing_user_input_start(&[function_output("c", "x")], 0),
        1
    );
}

#[test]
fn trailing_user_input_passes_transparent_control_items() {
    let items = vec![
        message("user", "q1"),
        message("assistant", "a1"),
        message("user", "q2"),
        ResponseItem::AdditionalTools {
            id: None,
            role: "developer".to_string(),
            tools: Arc::from(Vec::new()),
        },
        ResponseItem::ConfigurationUpdate {
            reasoning: ConfigurationReasoning {
                effort: ReasoningEffort::High,
            },
        },
        ResponseItem::CompactionTrigger {},
        ResponseItem::Other,
    ];
    assert_eq!(trailing_user_input_start(&items, 0), 2);
}

#[test]
fn classifies_items_by_origin() {
    assert_eq!(classify(&message("assistant", "a")), Classified::Skip);
    assert_eq!(classify(&function_call("call_1")), Classified::Skip);
    assert_eq!(
        classify(&message("developer", "be careful")),
        Classified::UserBlocks(vec![
            json!({"type": "text", "text": "[developer instructions]\nbe careful"})
        ])
    );
    let output = function_output("call_1", "done");
    match classify(&output) {
        Classified::ToolOutput { call_id, .. } => assert_eq!(call_id, "call_1"),
        other => panic!("unexpected classification: {other:?}"),
    }
}

#[test]
fn user_images_become_anthropic_blocks() {
    let item = ResponseItem::Message {
        encrypted_content: None,
        status: None,
        id: None,
        role: "user".to_string(),
        content: vec![
            ContentItem::InputText {
                text: "look".to_string(),
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
    };
    let Classified::UserBlocks(blocks) = classify(&item) else {
        panic!("expected user blocks");
    };
    assert_eq!(
        user_message_line(blocks),
        json!({
            "type": "user",
            "message": {"role": "user", "content": [
                {"type": "text", "text": "look"},
                {"type": "image", "source": {"type": "base64", "media_type": "image/png", "data": "AAAA"}},
            ]},
            "parent_tool_use_id": null,
            "session_id": "",
        })
    );
}

#[test]
fn tool_results_keep_text_images_and_failure() {
    let output = FunctionCallOutputPayload {
        body: FunctionCallOutputBody::ContentItems(vec![
            FunctionCallOutputContentItem::InputText {
                text: "exit 1".to_string(),
            },
            FunctionCallOutputContentItem::InputImage {
                image: ImageReference::Inline {
                    image_url: "data:image/jpeg;base64,/9j/".to_string(),
                },
                detail: None,
            },
        ]),
        success: Some(false),
    };
    let mut result = call_tool_result(&output);
    assert_eq!(
        result,
        json!({
            "content": [
                {"type": "text", "text": "exit 1"},
                {"type": "image", "data": "/9j/", "mimeType": "image/jpeg"},
            ],
            "isError": true,
        })
    );

    append_user_blocks(&mut result, &[json!({"type": "text", "text": "stop"})]);
    assert_eq!(
        result["content"].as_array().expect("content")[2..].to_vec(),
        vec![
            json!({"type": "text", "text": "[New input arrived while this tool was running:]"}),
            json!({"type": "text", "text": "stop"}),
        ]
    );
    assert_eq!(
        call_tool_result(&FunctionCallOutputPayload::from_text(String::new())),
        json!({"content": [{"type": "text", "text": "(no output)"}], "isError": false})
    );
}

#[test]
fn transcript_renders_turns_and_tool_activity() {
    let transcript = render_transcript(&[
        message("user", "list files"),
        function_call("call_1"),
        function_output("call_1", "a.txt"),
        message("assistant", "There is a.txt."),
    ])
    .expect("transcript");
    assert!(transcript.starts_with("<prior_conversation>\n"));
    assert!(transcript.ends_with("\n</prior_conversation>"));
    assert!(transcript.contains("[user]\nlist files"));
    assert!(
        transcript.contains(r#"[called tool shell (call call_1) with {"command":"ls"}]"#),
        "{transcript}"
    );
    assert!(transcript.contains("[result of call call_1]\na.txt"));
    assert!(transcript.contains("[assistant]\nThere is a.txt."));
    assert_eq!(render_transcript(&[]), None);
}

#[test]
fn truncation_keeps_the_most_recent_text() {
    assert_eq!(truncate_head("abcdef", 10), "abcdef");
    assert_eq!(
        truncate_head("abcdef", 2),
        "[... 4 earlier characters of the conversation were omitted ...]\nef"
    );
}
