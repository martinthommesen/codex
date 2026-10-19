use super::BridgeCore;
use super::MCP_TOOL_NAME_LIMIT;
use super::ToolRegistry;
use super::wire_tool_name;
use codex_api::anthropic::AnthropicToolKind;
use codex_api::anthropic::AnthropicToolTarget;
use pretty_assertions::assert_eq;
use serde_json::Value;
use serde_json::json;
use std::sync::Arc;
use std::time::Duration;
use tokio::io::AsyncBufReadExt;
use tokio::io::AsyncWriteExt;
use tokio::io::BufReader;
use tokio::io::DuplexStream;
use tokio::io::Lines;
use tokio::io::ReadHalf;
use tokio::io::WriteHalf;

struct Client {
    lines: Lines<BufReader<ReadHalf<DuplexStream>>>,
    writer: WriteHalf<DuplexStream>,
}

impl Client {
    fn connect(core: &Arc<BridgeCore>) -> Self {
        let (client, server) = tokio::io::duplex(64 * 1024);
        core.spawn_connection(server);
        let (read, writer) = tokio::io::split(client);
        Self {
            lines: BufReader::new(read).lines(),
            writer,
        }
    }

    async fn send(&mut self, message: Value) {
        self.writer
            .write_all(format!("{message}\n").as_bytes())
            .await
            .expect("write");
    }

    async fn recv(&mut self) -> Value {
        let line = tokio::time::timeout(Duration::from_secs(5), self.lines.next_line())
            .await
            .expect("timely response")
            .expect("read")
            .expect("line");
        serde_json::from_str(&line).expect("json")
    }

    async fn assert_silent(&mut self) {
        let next = tokio::time::timeout(Duration::from_millis(100), self.lines.next_line()).await;
        assert!(next.is_err(), "unexpected message: {next:?}");
    }

    async fn initialize(&mut self) {
        self.send(json!({"jsonrpc": "2.0", "id": 0, "method": "initialize", "params": {"protocolVersion": "2025-03-26"}}))
            .await;
        self.recv().await;
        self.send(json!({"jsonrpc": "2.0", "method": "notifications/initialized"}))
            .await;
    }
}

fn shell_registry() -> ToolRegistry {
    ToolRegistry::from_declarations(&[json!({
        "type": "function",
        "name": "shell",
        "description": "Runs a command.",
        "parameters": {"type": "object", "properties": {"command": {"type": "string"}}},
    })])
}

fn call(id: i64, name: &str, tool_use_id: Option<&str>) -> Value {
    let mut params = json!({"name": name, "arguments": {}});
    if let Some(tool_use_id) = tool_use_id {
        params["_meta"] = json!({"claudecode/toolUseId": tool_use_id});
    }
    json!({"jsonrpc": "2.0", "id": id, "method": "tools/call", "params": params})
}

fn text_result(text: &str) -> Value {
    json!({"content": [{"type": "text", "text": text}], "isError": false})
}

#[test]
fn registry_maps_mcp_names_back_to_codex_tools() {
    let long_name = "x".repeat(80);
    let registry = ToolRegistry::from_declarations(&[
        json!({"type": "function", "name": "shell", "parameters": {"type": "object"}}),
        json!({"type": "namespace", "name": "mcp__github__", "description": "GitHub", "tools": [
            {"type": "function", "name": long_name, "description": "Long."},
        ]}),
        json!({"type": "custom", "name": "apply_patch", "description": "Patch files."}),
        json!({"type": "web_search"}),
    ]);
    let names: Vec<&str> = registry
        .tools
        .iter()
        .filter_map(|tool| tool["name"].as_str())
        .collect();
    assert_eq!(names.len(), 3);
    for name in &names {
        assert!(name.len() <= MCP_TOOL_NAME_LIMIT, "{name}");
        assert!(wire_tool_name(name).len() <= 64, "{name}");
    }
    assert_eq!(
        registry.targets.get("apply_patch"),
        Some(&AnthropicToolTarget {
            name: "apply_patch".to_string(),
            namespace: None,
            kind: AnthropicToolKind::Freeform,
        })
    );
    let namespaced = names
        .iter()
        .find(|name| name.starts_with("mcp__github__"))
        .expect("namespaced tool");
    assert_eq!(
        registry.targets[*namespaced],
        AnthropicToolTarget {
            name: long_name,
            namespace: Some("mcp__github__".to_string()),
            kind: AnthropicToolKind::Function,
        }
    );
    assert_eq!(registry.unsupported.len(), 1);
    assert_eq!(
        registry.mcp_name_for_wire("mcp__codex__shell"),
        Some("shell")
    );
    assert_eq!(registry.mcp_name_for_wire("mcp__codex__nope"), None);
    assert_eq!(registry.mcp_name_for_wire("shell"), None);
    assert_eq!(
        registry
            .translator_names()
            .resolve("mcp__codex__apply_patch")
            .kind,
        AnthropicToolKind::Freeform
    );
}

#[tokio::test]
async fn initialize_ping_list_and_unknown_methods() {
    let core = Arc::new(BridgeCore::default());
    core.set_tools(shell_registry());
    let mut client = Client::connect(&core);

    client
        .send(json!({"jsonrpc": "2.0", "id": 1, "method": "initialize", "params": {"protocolVersion": "2025-03-26"}}))
        .await;
    let response = client.recv().await;
    assert_eq!(response["result"]["protocolVersion"], json!("2025-03-26"));
    assert_eq!(
        response["result"]["capabilities"],
        json!({"tools": {"listChanged": true}})
    );
    assert_eq!(response["result"]["serverInfo"]["name"], json!("codex"));

    client
        .send(json!({"jsonrpc": "2.0", "id": "p", "method": "ping"}))
        .await;
    assert_eq!(
        client.recv().await,
        json!({"jsonrpc": "2.0", "id": "p", "result": {}})
    );

    client
        .send(json!({"jsonrpc": "2.0", "id": 2, "method": "tools/list"}))
        .await;
    assert_eq!(
        client.recv().await["result"]["tools"],
        json!([{
            "name": "shell",
            "description": "Runs a command.",
            "inputSchema": {"type": "object", "properties": {"command": {"type": "string"}}},
        }])
    );

    client
        .send(json!({"jsonrpc": "2.0", "id": 3, "method": "resources/list"}))
        .await;
    assert_eq!(client.recv().await["error"]["code"], json!(-32601));
}

#[tokio::test]
async fn tool_calls_are_parked_until_codex_answers() {
    let core = Arc::new(BridgeCore::default());
    core.set_tools(shell_registry());
    let mut client = Client::connect(&core);
    client.initialize().await;

    client.send(call(7, "shell", Some("toolu_1"))).await;
    client.assert_silent().await;
    assert_eq!(core.parked_count(), 1);

    core.answer("toolu_1", "shell", text_result("done"));
    assert_eq!(
        client.recv().await,
        json!({"jsonrpc": "2.0", "id": 7, "result": text_result("done")})
    );
    assert_eq!(core.parked_count(), 0);
}

#[tokio::test]
async fn answers_given_before_the_call_arrives_are_delivered_on_arrival() {
    let core = Arc::new(BridgeCore::default());
    core.set_tools(shell_registry());
    let mut client = Client::connect(&core);
    client.initialize().await;

    core.answer("toolu_2", "shell", text_result("early"));
    client.send(call(8, "shell", Some("toolu_2"))).await;
    assert_eq!(client.recv().await["result"], text_result("early"));
}

#[tokio::test]
async fn calls_without_tool_use_ids_match_in_order() {
    let core = Arc::new(BridgeCore::default());
    core.set_tools(shell_registry());
    let mut client = Client::connect(&core);
    client.initialize().await;

    client.send(call(1, "shell", None)).await;
    client.send(call(2, "shell", None)).await;
    client.assert_silent().await;
    core.answer("toolu_a", "shell", text_result("first"));
    core.answer("toolu_b", "shell", text_result("second"));
    let first = client.recv().await;
    let second = client.recv().await;
    assert_eq!(
        (first["id"].clone(), first["result"].clone()),
        (json!(1), text_result("first"))
    );
    assert_eq!(
        (second["id"].clone(), second["result"].clone()),
        (json!(2), text_result("second"))
    );
}

#[tokio::test]
async fn unknown_tools_fail_immediately_and_fail_all_releases_parked_calls() {
    let core = Arc::new(BridgeCore::default());
    core.set_tools(shell_registry());
    let mut client = Client::connect(&core);
    client.initialize().await;

    client.send(call(1, "rm_rf", Some("toolu_x"))).await;
    let response = client.recv().await;
    assert_eq!(response["result"]["isError"], json!(true));

    client.send(call(2, "shell", Some("toolu_y"))).await;
    client
        .send(json!({"jsonrpc": "2.0", "id": 3, "method": "ping"}))
        .await;
    assert_eq!(client.recv().await["id"], json!(3));
    core.fail_all("interrupted");
    assert_eq!(
        client.recv().await,
        json!({"jsonrpc": "2.0", "id": 2, "result": {
            "content": [{"type": "text", "text": "interrupted"}],
            "isError": true,
        }})
    );
}

#[tokio::test]
async fn cancelled_calls_are_dropped() {
    let core = Arc::new(BridgeCore::default());
    core.set_tools(shell_registry());
    let mut client = Client::connect(&core);
    client.initialize().await;

    client.send(call(4, "shell", Some("toolu_c"))).await;
    client
        .send(json!({"jsonrpc": "2.0", "id": 5, "method": "ping"}))
        .await;
    assert_eq!(client.recv().await["id"], json!(5));
    assert_eq!(core.parked_count(), 1);
    client
        .send(json!({"jsonrpc": "2.0", "method": "notifications/cancelled", "params": {"requestId": 4}}))
        .await;
    client
        .send(json!({"jsonrpc": "2.0", "id": 6, "method": "ping"}))
        .await;
    assert_eq!(client.recv().await["id"], json!(6));
    assert_eq!(core.parked_count(), 0);
}

#[tokio::test]
async fn changing_tools_notifies_initialized_clients() {
    let core = Arc::new(BridgeCore::default());
    core.set_tools(shell_registry());
    let mut client = Client::connect(&core);
    client.initialize().await;
    client
        .send(json!({"jsonrpc": "2.0", "id": 1, "method": "ping"}))
        .await;
    client.recv().await;

    assert!(!core.set_tools(shell_registry()));
    client.assert_silent().await;
    assert!(core.set_tools(ToolRegistry::default()));
    assert_eq!(
        client.recv().await,
        json!({"jsonrpc": "2.0", "method": "notifications/tools/list_changed"})
    );
}

#[tokio::test]
async fn calls_codex_never_received_are_released_after_the_grace_period() {
    let core = Arc::new(BridgeCore::default());
    core.set_tools(shell_registry());
    let mut client = Client::connect(&core);
    client.initialize().await;

    client.send(call(1, "shell", Some("toolu_expected"))).await;
    client.send(call(2, "shell", Some("toolu_unknown"))).await;
    client.assert_silent().await;
    assert_eq!(core.parked_count(), 2);

    // Within the grace period nothing is released.
    assert_eq!(
        core.fail_unexpected_parked(&[], Duration::from_secs(3600)),
        0
    );
    // Afterwards only the call Codex does not know about is answered.
    assert_eq!(
        core.fail_unexpected_parked(&["toolu_expected".to_string()], Duration::ZERO),
        1
    );
    let response = client.recv().await;
    assert_eq!(
        (
            response["id"].clone(),
            response["result"]["isError"].clone()
        ),
        (json!(2), json!(true))
    );
    assert_eq!(core.parked_count(), 1);
}
