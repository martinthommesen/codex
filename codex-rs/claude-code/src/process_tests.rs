use super::SessionArg;
use super::SpawnSpec;
use super::bounded_tail;
use super::build_args;
use super::mcp_config_json;
use pretty_assertions::assert_eq;
use serde_json::json;
use std::path::Path;
use uuid::Uuid;

#[test]
fn mcp_config_appends_socket_to_bridge_args() {
    let config = mcp_config_json(
        Path::new("/usr/bin/codex"),
        &["stdio-to-uds".to_string()],
        Path::new("/tmp/codex-cc-1/mcp.sock"),
    );
    let parsed: serde_json::Value = serde_json::from_str(&config).expect("json");
    assert_eq!(
        parsed,
        json!({"mcpServers": {"codex": {
            "type": "stdio",
            "command": "/usr/bin/codex",
            "args": ["stdio-to-uds", "/tmp/codex-cc-1/mcp.sock"],
        }}})
    );
}

#[test]
fn args_disable_builtin_tools_and_user_settings() {
    let id = Uuid::nil();
    let spec = SpawnSpec {
        command: Path::new("claude"),
        cwd: Path::new("/work"),
        model: "claude-sonnet-5",
        effort: Some("high"),
        system_prompt_file: Path::new("/state/prompt.md"),
        mcp_config: "{}",
        session: SessionArg::Resume(id),
    };
    let args: Vec<String> = build_args(&spec)
        .into_iter()
        .map(|arg| arg.to_string_lossy().into_owned())
        .collect();
    let expected: Vec<String> = [
        "-p",
        "--verbose",
        "--input-format",
        "stream-json",
        "--output-format",
        "stream-json",
        "--include-partial-messages",
        "--model",
        "claude-sonnet-5",
        "--effort",
        "high",
        "--system-prompt-file",
        "/state/prompt.md",
        "--tools",
        "",
        "--strict-mcp-config",
        "--mcp-config",
        "{}",
        "--allowedTools",
        "mcp__codex",
        "--permission-mode",
        "dontAsk",
        "--setting-sources",
        "",
        "--settings",
        r#"{"disableAllHooks":true}"#,
        "--disable-slash-commands",
        "--resume",
        "00000000-0000-0000-0000-000000000000",
    ]
    .into_iter()
    .map(str::to_string)
    .collect();
    assert_eq!(args, expected);
}

#[test]
fn bounded_tail_keeps_the_end() {
    assert_eq!(bounded_tail("short", 10), "short");
    assert_eq!(bounded_tail("0123456789abcdef", 4), "...cdef");
}
