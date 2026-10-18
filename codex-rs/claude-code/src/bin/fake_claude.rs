//! Test double for the Claude Code CLI used by this crate's integration tests.
//!
//! It speaks the subset of the stream-json protocol the session driver relies on and connects
//! directly to the Codex tool bridge socket (the last argument of the `codex` MCP server entry).
//! Scenarios are chosen by markers in the user message text:
//! - `[tool]`: stream text plus a `tool_use` for the first bridged tool, call it over MCP, then
//!   stream a final answer echoing the tool result;
//! - `[hang]`: stream a partial answer and wait (exits when interrupted);
//! - anything else: reply with `echo: <text>`.
//!
//! Session processes record what they observe in `fake-claude-record.jsonl` in their working
//! directory; `auth status --json` prints `$FAKE_CLAUDE_AUTH_JSON`.

use codex_uds::UnixStream;
use serde_json::Value;
use serde_json::json;
use std::io::Write;
use tokio::io::AsyncBufReadExt;
use tokio::io::AsyncWriteExt;
use tokio::io::BufReader;
use tokio::io::ReadHalf;
use tokio::io::WriteHalf;

/// File in the working directory that receives one JSON line per spawn, user message, tool
/// result, and interrupt.
const RECORD_FILE: &str = "fake-claude-record.jsonl";
/// When this file exists in the working directory, `--resume` fails like a missing session.
const FAIL_RESUME_FILE: &str = "fake-claude-fail-resume";
/// JSON printed by `auth status --json`.
const AUTH_JSON_ENV: &str = "FAKE_CLAUDE_AUTH_JSON";

#[tokio::main(flavor = "current_thread")]
async fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.first().map(String::as_str) == Some("auth") {
        auth_status();
    }
    let code = match run(&args).await {
        Ok(()) => 0,
        Err(err) => {
            eprintln!("fake claude error: {err}");
            1
        }
    };
    std::process::exit(code);
}

fn auth_status() -> ! {
    let output = std::env::var(AUTH_JSON_ENV).unwrap_or_else(|_| {
        r#"{"loggedIn":true,"authMethod":"claude.ai","apiProvider":"firstParty"}"#.to_string()
    });
    println!("{output}");
    let logged_in = serde_json::from_str::<Value>(&output)
        .ok()
        .and_then(|value| value.get("loggedIn").and_then(Value::as_bool))
        .unwrap_or(false);
    std::process::exit(if logged_in { 0 } else { 1 });
}

fn option_value<'a>(args: &'a [String], name: &str) -> Option<&'a str> {
    args.iter()
        .position(|arg| arg == name)
        .and_then(|index| args.get(index + 1))
        .map(String::as_str)
}

fn record(entry: Value) {
    let mut entry = entry;
    entry["pid"] = json!(std::process::id());
    if let Ok(mut file) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(RECORD_FILE)
    {
        let _ = writeln!(file, "{entry}");
    }
}

fn emit(value: Value) {
    let mut stdout = std::io::stdout().lock();
    let _ = writeln!(stdout, "{value}");
    let _ = stdout.flush();
}

fn emit_stream_event(session_id: &str, event: Value) {
    emit(json!({
        "type": "stream_event",
        "event": event,
        "parent_tool_use_id": null,
        "session_id": session_id,
    }));
}

struct Mcp {
    reader: BufReader<ReadHalf<UnixStream>>,
    writer: WriteHalf<UnixStream>,
    next_id: i64,
}

impl Mcp {
    async fn connect(socket: &str) -> std::io::Result<Self> {
        let stream = UnixStream::connect(socket).await?;
        let (read, writer) = tokio::io::split(stream);
        Ok(Self {
            reader: BufReader::new(read),
            writer,
            next_id: 0,
        })
    }

    async fn send(&mut self, message: Value) -> std::io::Result<()> {
        self.writer
            .write_all(format!("{message}\n").as_bytes())
            .await?;
        self.writer.flush().await
    }

    async fn request(&mut self, method: &str, params: Value) -> std::io::Result<Value> {
        self.next_id += 1;
        let id = self.next_id;
        self.send(json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params}))
            .await?;
        loop {
            let mut line = String::new();
            if self.reader.read_line(&mut line).await? == 0 {
                return Err(std::io::Error::other("bridge closed the connection"));
            }
            let Ok(message) = serde_json::from_str::<Value>(&line) else {
                continue;
            };
            if message.get("id") == Some(&json!(id)) {
                return Ok(message);
            }
        }
    }
}

async fn run(args: &[String]) -> std::io::Result<()> {
    let session_id = option_value(args, "--session-id")
        .or_else(|| option_value(args, "--resume"))
        .unwrap_or("")
        .to_string();
    let resumed = option_value(args, "--resume").is_some();
    record(json!({
        "event": "spawn",
        "argv": args,
        "anthropic_api_key_present": std::env::var_os("ANTHROPIC_API_KEY").is_some(),
        "anthropic_auth_token_present": std::env::var_os("ANTHROPIC_AUTH_TOKEN").is_some(),
        "openai_api_key_present": std::env::var_os("OPENAI_API_KEY").is_some(),
        "codex_api_key_present": std::env::var_os("CODEX_API_KEY").is_some(),
        "oauth_token_present": std::env::var_os("CLAUDE_CODE_OAUTH_TOKEN").is_some(),
        "enable_tool_search": std::env::var("ENABLE_TOOL_SEARCH").ok(),
        "cwd": std::env::current_dir()?.display().to_string(),
    }));

    if resumed && std::path::Path::new(FAIL_RESUME_FILE).exists() {
        emit(json!({
            "type": "result",
            "subtype": "error_during_execution",
            "is_error": true,
            "session_id": session_id,
        }));
        eprintln!("No conversation found with session ID: {session_id}");
        std::process::exit(1);
    }

    let mcp_config: Value = serde_json::from_str(option_value(args, "--mcp-config").unwrap_or(""))
        .map_err(std::io::Error::other)?;
    let socket = mcp_config["mcpServers"]["codex"]["args"]
        .as_array()
        .and_then(|args| args.last())
        .and_then(Value::as_str)
        .ok_or_else(|| std::io::Error::other("missing bridge socket"))?
        .to_string();
    let mut mcp = Mcp::connect(&socket).await?;
    mcp.request(
        "initialize",
        json!({
            "protocolVersion": "2025-06-18",
            "capabilities": {},
            "clientInfo": {"name": "fake-claude", "version": "0"},
        }),
    )
    .await?;
    mcp.send(json!({"jsonrpc": "2.0", "method": "notifications/initialized"}))
        .await?;
    let tools = mcp.request("tools/list", json!({})).await?;
    let tool_names: Vec<String> = tools["result"]["tools"]
        .as_array()
        .map(|tools| {
            tools
                .iter()
                .filter_map(|tool| tool.get("name").and_then(Value::as_str))
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default();
    record(json!({"event": "tools", "names": tool_names}));

    let mut stdin = BufReader::new(tokio::io::stdin()).lines();
    let mut initialized = false;
    let mut message_counter = 0;
    while let Some(line) = stdin.next_line().await? {
        let Ok(message) = serde_json::from_str::<Value>(&line) else {
            continue;
        };
        match message.get("type").and_then(Value::as_str) {
            Some("control_request") => {
                record(json!({"event": "interrupt"}));
                emit(json!({
                    "type": "control_response",
                    "response": {"subtype": "success", "request_id": message["request_id"]},
                }));
                std::process::exit(0);
            }
            Some("user") => {}
            _ => continue,
        }
        let content = message["message"]["content"].clone();
        record(json!({"event": "user_message", "content": content}));
        if !initialized {
            initialized = true;
            emit(json!({
                "type": "system",
                "subtype": "init",
                "session_id": session_id,
                "mcp_servers": [{"name": "codex", "status": "connected"}],
                "tools": tool_names.iter().map(|name| format!("mcp__codex__{name}")).collect::<Vec<_>>(),
            }));
        }
        let text: String = content
            .as_array()
            .map(|blocks| {
                blocks
                    .iter()
                    .filter_map(|block| block.get("text").and_then(Value::as_str))
                    .collect::<Vec<_>>()
                    .join("\n")
            })
            .unwrap_or_default();
        message_counter += 1;
        if text.contains("[tool]") {
            let Some(tool) = tool_names.first() else {
                return Err(std::io::Error::other("no tools were listed"));
            };
            tool_scenario(&session_id, &mut mcp, tool, message_counter).await?;
        } else if text.contains("[hang]") {
            stream_message_start(&session_id, &format!("msg_fake_{message_counter}"));
            emit_stream_event(
                &session_id,
                json!({"type": "content_block_start", "index": 0, "content_block": {"type": "text", "text": ""}}),
            );
            emit_stream_event(
                &session_id,
                json!({"type": "content_block_delta", "index": 0, "delta": {"type": "text_delta", "text": "partial"}}),
            );
        } else {
            let reply = format!("echo: {}", text.lines().last().unwrap_or(""));
            stream_text_message(
                &session_id,
                &format!("msg_fake_{message_counter}"),
                &reply,
                "end_turn",
            );
            emit_result(&session_id, &reply);
        }
    }
    Ok(())
}

async fn tool_scenario(
    session_id: &str,
    mcp: &mut Mcp,
    tool: &str,
    message_counter: usize,
) -> std::io::Result<()> {
    let tool_use_id = format!("toolu_fake_{message_counter}");
    let arguments = json!({"command": "echo hi"});
    stream_message_start(session_id, &format!("msg_fake_{message_counter}_a"));
    emit_stream_event(
        session_id,
        json!({"type": "content_block_start", "index": 0, "content_block": {"type": "text", "text": ""}}),
    );
    emit_stream_event(
        session_id,
        json!({"type": "content_block_delta", "index": 0, "delta": {"type": "text_delta", "text": "Calling the tool."}}),
    );
    emit_stream_event(
        session_id,
        json!({"type": "content_block_stop", "index": 0}),
    );
    emit_stream_event(
        session_id,
        json!({"type": "content_block_start", "index": 1, "content_block": {
            "type": "tool_use",
            "id": tool_use_id,
            "name": format!("mcp__codex__{tool}"),
            "input": {},
        }}),
    );
    emit_stream_event(
        session_id,
        json!({"type": "content_block_delta", "index": 1, "delta": {
            "type": "input_json_delta",
            "partial_json": arguments.to_string(),
        }}),
    );
    emit_stream_event(
        session_id,
        json!({"type": "content_block_stop", "index": 1}),
    );
    emit_stream_event(
        session_id,
        json!({"type": "message_delta", "delta": {"stop_reason": "tool_use"}, "usage": {"output_tokens": 12}}),
    );
    emit_stream_event(session_id, json!({"type": "message_stop"}));
    emit(json!({
        "type": "assistant",
        "message": {"role": "assistant", "content": [{"type": "tool_use", "id": tool_use_id}]},
        "parent_tool_use_id": null,
        "session_id": session_id,
    }));

    let response = mcp
        .request(
            "tools/call",
            json!({
                "name": tool,
                "arguments": arguments,
                "_meta": {"claudecode/toolUseId": tool_use_id},
            }),
        )
        .await?;
    record(json!({"event": "tool_result", "response": response}));
    let output: String = response["result"]["content"]
        .as_array()
        .map(|blocks| {
            blocks
                .iter()
                .filter_map(|block| block.get("text").and_then(Value::as_str))
                .collect::<Vec<_>>()
                .join("\n")
        })
        .unwrap_or_default();
    emit(json!({
        "type": "user",
        "message": {"role": "user", "content": [{"type": "tool_result", "tool_use_id": tool_use_id}]},
        "parent_tool_use_id": null,
        "session_id": session_id,
    }));
    let reply = format!("Tool said: {output}");
    stream_text_message(
        session_id,
        &format!("msg_fake_{message_counter}_b"),
        &reply,
        "end_turn",
    );
    emit_result(session_id, &reply);
    Ok(())
}

fn stream_message_start(session_id: &str, message_id: &str) {
    emit_stream_event(
        session_id,
        json!({"type": "message_start", "message": {
            "id": message_id,
            "type": "message",
            "role": "assistant",
            "model": "claude-fake",
            "content": [],
            "usage": {"input_tokens": 5, "output_tokens": 1},
        }}),
    );
}

fn stream_text_message(session_id: &str, message_id: &str, text: &str, stop_reason: &str) {
    stream_message_start(session_id, message_id);
    emit_stream_event(
        session_id,
        json!({"type": "content_block_start", "index": 0, "content_block": {"type": "text", "text": ""}}),
    );
    emit_stream_event(
        session_id,
        json!({"type": "content_block_delta", "index": 0, "delta": {"type": "text_delta", "text": text}}),
    );
    emit_stream_event(
        session_id,
        json!({"type": "content_block_stop", "index": 0}),
    );
    emit_stream_event(
        session_id,
        json!({"type": "message_delta", "delta": {"stop_reason": stop_reason}, "usage": {"output_tokens": 7}}),
    );
    emit_stream_event(session_id, json!({"type": "message_stop"}));
}

fn emit_result(session_id: &str, text: &str) {
    emit(json!({
        "type": "result",
        "subtype": "success",
        "is_error": false,
        "result": text,
        "session_id": session_id,
        "usage": {"input_tokens": 5, "output_tokens": 7},
    }));
}
