use super::*;
use pretty_assertions::assert_eq;
use serde_json::json;

#[test]
fn wire_names_are_stable_and_valid() {
    assert_eq!(anthropic_wire_tool_name(None, "shell"), "shell");
    assert_eq!(
        anthropic_wire_tool_name(Some("functions"), "apply_patch"),
        "apply_patch"
    );
    assert_eq!(
        anthropic_wire_tool_name(Some("mcp__github__"), "create_issue"),
        "mcp__github__create_issue"
    );
    assert_eq!(
        anthropic_wire_tool_name(Some("collaboration"), "spawn_agent"),
        "collaboration__spawn_agent"
    );

    let dotted = anthropic_wire_tool_name(Some("mcp__srv__"), "get.thing");
    assert!(dotted.starts_with("mcp__srv__get_thing_"));
    assert_eq!(
        dotted,
        anthropic_wire_tool_name(Some("mcp__srv__"), "get.thing")
    );

    let limited = anthropic_wire_tool_name_with_limit(None, &"y".repeat(60), 52);
    assert_eq!(limited.len(), 52);

    let long_name = "x".repeat(100);
    let long = anthropic_wire_tool_name(None, &long_name);
    assert_eq!(long.len(), 64);
    assert!(
        long.chars()
            .all(|ch| ch.is_ascii_alphanumeric() || ch == '_' || ch == '-')
    );
}

#[test]
fn tool_use_ids_are_sanitized_deterministically() {
    assert_eq!(anthropic_tool_use_id("call_abc-123"), "call_abc-123");
    let odd = anthropic_tool_use_id("call:1/2");
    assert!(odd.starts_with("call_1_2_"));
    assert_eq!(odd, anthropic_tool_use_id("call:1/2"));
}

#[test]
fn translates_function_namespace_and_freeform_tools() {
    let declarations = vec![
        json!({
            "type": "function",
            "name": "shell",
            "description": "Run a command",
            "strict": false,
            "parameters": {"type": "object", "properties": {"command": {"type": "array"}}}
        }),
        json!({
            "type": "namespace",
            "name": "mcp__docs__",
            "description": "Docs server",
            "tools": [{
                "type": "function",
                "name": "search",
                "description": "Search docs",
                "strict": false,
                "parameters": {"type": "object", "properties": {}}
            }]
        }),
        json!({
            "type": "custom",
            "name": "apply_patch",
            "description": "Apply a patch",
            "format": {"type": "grammar", "syntax": "lark", "definition": "start: patch"}
        }),
        json!({"type": "web_search"}),
        json!({"type": "tool_search", "execution": "client", "description": "", "parameters": {}}),
    ];

    let translated = translate_tools(&declarations);
    let names: Vec<&str> = translated
        .tools
        .iter()
        .map(|tool| tool.name.as_str())
        .collect();
    assert_eq!(names, vec!["shell", "mcp__docs__search", "apply_patch"]);
    assert_eq!(
        translated.tools[1].description,
        "Search docs\n\n(Docs server)"
    );
    assert_eq!(
        translated.tools[2].input_schema["required"],
        json!(["input"])
    );
    assert!(translated.tools[2].description.contains("start: patch"));
    assert_eq!(translated.unsupported.len(), 2);

    assert_eq!(
        translated.names.resolve("mcp__docs__search"),
        AnthropicToolTarget {
            name: "search".to_string(),
            namespace: Some("mcp__docs__".to_string()),
            kind: AnthropicToolKind::Function,
        }
    );
    assert_eq!(
        translated.names.resolve("apply_patch").kind,
        AnthropicToolKind::Freeform
    );
    assert_eq!(
        translated.names.resolve("unknown_tool"),
        AnthropicToolTarget {
            name: "unknown_tool".to_string(),
            namespace: None,
            kind: AnthropicToolKind::Function,
        }
    );
}

#[test]
fn input_schema_is_forced_to_object() {
    let translated = translate_tools(&[json!({
        "type": "function",
        "name": "noop",
        "description": "",
        "strict": false,
        "parameters": {"properties": {}}
    })]);
    assert_eq!(
        translated.tools[0].input_schema,
        json!({"type": "object", "properties": {}})
    );
}

#[test]
fn freeform_wording_is_removed_from_wrapped_tools() {
    let translated = translate_tools(&[json!({
        "type": "custom",
        "name": "apply_patch",
        "description": "The `apply_patch` tool can be used to edit files. This is a FREEFORM tool, so do not wrap the patch in JSON.",
        "format": {"type": "grammar", "syntax": "lark", "definition": "start: x"}
    })]);
    let description = &translated.tools[0].description;
    assert!(description.starts_with("The `apply_patch` tool can be used to edit files."));
    assert!(!description.contains("FREEFORM"));
    assert!(description.contains("start: x"));
}

#[test]
fn top_level_schema_composition_is_flattened_and_reported() {
    let translated = translate_tools(&[json!({
        "type": "function",
        "name": "fetch",
        "description": "Fetch a resource.",
        "parameters": {
            "type": "object",
            "properties": {"timeout": {"type": "number"}},
            "anyOf": [
                {"properties": {"path": {"type": "string"}}, "required": ["path"]},
                {"properties": {"url": {"type": "string"}}, "required": ["url"]}
            ]
        }
    })]);
    let tool = &translated.tools[0];
    assert_eq!(
        tool.input_schema,
        json!({
            "type": "object",
            "properties": {
                "timeout": {"type": "number"},
                "path": {"type": "string"},
                "url": {"type": "string"}
            }
        })
    );
    let description = &tool.description;
    assert!(
        description.starts_with("Fetch a resource."),
        "{description}"
    );
    assert!(description.contains("\"anyOf\""), "{description}");
    assert_eq!(translated.unsupported.len(), 1);
    assert!(translated.unsupported[0].contains("`fetch`"));
}
