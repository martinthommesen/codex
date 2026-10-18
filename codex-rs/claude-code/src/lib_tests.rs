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
