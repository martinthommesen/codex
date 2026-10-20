use super::resolve_claude_command;
use super::unsupported_tool_notes;
use pretty_assertions::assert_eq;
use std::path::Path;
use std::path::PathBuf;

#[test]
fn explicit_command_wins() {
    assert_eq!(
        resolve_claude_command(Some(Path::new("/opt/claude/bin/claude"))),
        PathBuf::from("/opt/claude/bin/claude")
    );
}

#[test]
fn hosted_tools_are_reported_as_unsupported() {
    let notes = unsupported_tool_notes(&[
        serde_json::json!({"type": "function", "name": "shell"}),
        serde_json::json!({"type": "web_search"}),
    ]);
    assert_eq!(notes.len(), 1);
    assert!(notes[0].starts_with("web_search"), "{notes:?}");
}

#[test]
fn bridged_name_collisions_are_reported_as_unsupported() {
    let long_name = "y".repeat(60);
    let truncated = codex_api::anthropic::anthropic_wire_tool_name_with_limit(
        /*namespace*/ None,
        &long_name,
        crate::bridge::MCP_TOOL_NAME_LIMIT,
    );
    let notes = unsupported_tool_notes(&[
        serde_json::json!({"type": "function", "name": long_name}),
        serde_json::json!({"type": "function", "name": truncated}),
    ]);
    assert_eq!(notes.len(), 1);
    assert!(
        notes[0].starts_with(&format!("tool `{truncated}`")),
        "{notes:?}"
    );
}
