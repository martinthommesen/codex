//! Translation of Codex tool declarations into Anthropic Messages API tool definitions.
//!
//! Codex builds tool declarations in the Responses API JSON shape. The Messages API only
//! accepts flat client tools whose names match `^[a-zA-Z0-9_-]{1,64}$`, so namespaced tools
//! are flattened, freeform (grammar) tools become JSON tools with a single `input` string,
//! and hosted OpenAI tools are reported as unsupported instead of being silently dropped.

use serde::Serialize;
use serde_json::Map;
use serde_json::Value;
use serde_json::json;
use std::collections::BTreeMap;
use std::collections::BTreeSet;

const MAX_TOOL_NAME_LEN: usize = 64;
const HASH_SUFFIX_LEN: usize = 9; // "_" + 8 hex chars
const DEFAULT_FUNCTION_NAMESPACE: &str = "functions";
/// Property used to carry the raw text of a freeform (grammar) tool call.
pub const FREEFORM_INPUT_PROPERTY: &str = "input";

/// What kind of Codex tool an Anthropic tool name refers to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AnthropicToolKind {
    /// JSON function tool; arguments are passed through as a JSON object.
    Function,
    /// Freeform tool; the raw text lives in the `input` property.
    Freeform,
}

/// Codex identity of a tool exposed to Anthropic under a sanitized wire name.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AnthropicToolTarget {
    pub name: String,
    pub namespace: Option<String>,
    pub kind: AnthropicToolKind,
}

/// Bidirectional mapping between Codex tool identities and Anthropic wire names.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct AnthropicToolNameMap {
    by_wire: BTreeMap<String, AnthropicToolTarget>,
}

impl AnthropicToolNameMap {
    /// Registers a tool and returns the wire name it is exposed under.
    pub fn register(
        &mut self,
        namespace: Option<&str>,
        name: &str,
        kind: AnthropicToolKind,
    ) -> String {
        let wire = anthropic_wire_tool_name(namespace, name);
        self.by_wire.insert(
            wire.clone(),
            AnthropicToolTarget {
                name: name.to_string(),
                namespace: namespace.map(str::to_string),
                kind,
            },
        );
        wire
    }

    /// Resolves a wire name emitted by the model back to the Codex tool identity.
    ///
    /// Unknown names are returned as plain function calls so the Codex tool router can report
    /// the unknown tool back to the model instead of failing the turn.
    pub fn resolve(&self, wire: &str) -> AnthropicToolTarget {
        self.by_wire
            .get(wire)
            .cloned()
            .unwrap_or_else(|| AnthropicToolTarget {
                name: wire.to_string(),
                namespace: None,
                kind: AnthropicToolKind::Function,
            })
    }

    /// Registers an explicit wire name, for transports that expose tools under their own
    /// naming scheme (for example MCP tools inside Claude Code).
    pub fn insert(&mut self, wire: String, target: AnthropicToolTarget) {
        self.by_wire.insert(wire, target);
    }

    /// Iterates over `(wire name, target)` pairs in wire-name order.
    pub fn iter(&self) -> impl Iterator<Item = (&str, &AnthropicToolTarget)> {
        self.by_wire
            .iter()
            .map(|(wire, target)| (wire.as_str(), target))
    }

    pub fn is_empty(&self) -> bool {
        self.by_wire.is_empty()
    }
}

/// Deterministically derives the Anthropic tool name for a Codex tool identity.
///
/// The same identity always maps to the same wire name so history replay stays byte-stable.
pub fn anthropic_wire_tool_name(namespace: Option<&str>, name: &str) -> String {
    anthropic_wire_tool_name_with_limit(namespace, name, MAX_TOOL_NAME_LEN)
}

/// Like [`anthropic_wire_tool_name`], but caps the result at `max_len` characters (at least 16),
/// leaving room for a transport prefix such as Claude Code's `mcp__<server>__`.
pub fn anthropic_wire_tool_name_with_limit(
    namespace: Option<&str>,
    name: &str,
    max_len: usize,
) -> String {
    let max_len = max_len.clamp(16, MAX_TOOL_NAME_LEN);
    let joined = match namespace {
        Some(namespace) if !namespace.is_empty() && namespace != DEFAULT_FUNCTION_NAMESPACE => {
            // MCP namespaces already end with `__`; others get a readable separator.
            if namespace.ends_with('_') {
                format!("{namespace}{name}")
            } else {
                format!("{namespace}__{name}")
            }
        }
        _ => name.to_string(),
    };
    let mut sanitized: String = joined
        .chars()
        .map(|ch| {
            if ch.is_ascii_alphanumeric() || ch == '_' || ch == '-' {
                ch
            } else {
                '_'
            }
        })
        .collect();
    if sanitized.is_empty() {
        sanitized.push_str("tool");
    }
    if sanitized == joined && sanitized.len() <= max_len {
        return sanitized;
    }
    // Lossy rewrite: keep a readable prefix and disambiguate with a stable hash.
    let hash = format!("{:08x}", fnv1a_32(joined.as_bytes()));
    let keep = max_len - HASH_SUFFIX_LEN;
    if sanitized.len() > keep {
        sanitized.truncate(keep);
    }
    format!("{sanitized}_{hash}")
}

/// Sanitizes a tool-call id so it satisfies the Messages API `^[a-zA-Z0-9_-]+$` rule.
pub fn anthropic_tool_use_id(call_id: &str) -> String {
    let sanitized: String = call_id
        .chars()
        .map(|ch| {
            if ch.is_ascii_alphanumeric() || ch == '_' || ch == '-' {
                ch
            } else {
                '_'
            }
        })
        .collect();
    if sanitized.is_empty() {
        return format!("toolu_{:08x}", fnv1a_32(call_id.as_bytes()));
    }
    if sanitized == call_id {
        sanitized
    } else {
        format!("{sanitized}_{:08x}", fnv1a_32(call_id.as_bytes()))
    }
}

fn fnv1a_32(bytes: &[u8]) -> u32 {
    let mut hash: u32 = 0x811c_9dc5;
    for byte in bytes {
        hash ^= u32::from(*byte);
        hash = hash.wrapping_mul(0x0100_0193);
    }
    hash
}

/// One Anthropic client tool definition.
#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct AnthropicTool {
    pub name: String,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub description: String,
    pub input_schema: Value,
}

/// Result of translating Codex tool declarations.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct TranslatedTools {
    pub tools: Vec<AnthropicTool>,
    pub names: AnthropicToolNameMap,
    /// Human-readable notes about declarations that have no Anthropic equivalent.
    pub unsupported: Vec<String>,
}

/// Translates Responses-API-shaped tool declarations into Anthropic tools.
pub fn translate_tools(declarations: &[Value]) -> TranslatedTools {
    let mut translated = TranslatedTools::default();
    let mut seen = BTreeSet::new();
    for declaration in declarations {
        translate_declaration(declaration, None, &mut translated, &mut seen);
    }
    translated
}

fn translate_declaration(
    declaration: &Value,
    namespace: Option<(&str, &str)>,
    translated: &mut TranslatedTools,
    seen: &mut BTreeSet<String>,
) {
    let kind = declaration
        .get("type")
        .and_then(Value::as_str)
        .unwrap_or("function");
    match kind {
        "function" => {
            let Some(name) = declaration.get("name").and_then(Value::as_str) else {
                return;
            };
            let description = joined_description(namespace, declaration);
            let input_schema = normalize_input_schema(declaration.get("parameters"));
            push_tool(
                translated,
                seen,
                namespace.map(|(ns, _)| ns),
                name,
                AnthropicToolKind::Function,
                description,
                input_schema,
            );
        }
        "custom" => {
            let Some(name) = declaration.get("name").and_then(Value::as_str) else {
                return;
            };
            let mut description =
                without_freeform_wording(&joined_description(namespace, declaration));
            if let Some(format) = declaration.get("format") {
                let syntax = format.get("syntax").and_then(Value::as_str).unwrap_or("");
                let definition = format
                    .get("definition")
                    .and_then(Value::as_str)
                    .unwrap_or("");
                if !definition.is_empty() {
                    if !description.is_empty() {
                        description.push_str("\n\n");
                    }
                    description.push_str(&format!(
                        "Put the complete raw tool input in the `{FREEFORM_INPUT_PROPERTY}` string. \
It must conform to this {syntax} grammar:\n{definition}"
                    ));
                }
            }
            let input_schema = json!({
                "type": "object",
                "properties": {
                    FREEFORM_INPUT_PROPERTY: {
                        "type": "string",
                        "description": "Raw tool input text.",
                    }
                },
                "required": [FREEFORM_INPUT_PROPERTY],
                "additionalProperties": false,
            });
            push_tool(
                translated,
                seen,
                namespace.map(|(ns, _)| ns),
                name,
                AnthropicToolKind::Freeform,
                description,
                input_schema,
            );
        }
        "namespace" => {
            let Some(ns_name) = declaration.get("name").and_then(Value::as_str) else {
                return;
            };
            let ns_description = declaration
                .get("description")
                .and_then(Value::as_str)
                .unwrap_or("");
            if let Some(tools) = declaration.get("tools").and_then(Value::as_array) {
                for tool in tools {
                    translate_declaration(tool, Some((ns_name, ns_description)), translated, seen);
                }
            }
        }
        "tool_search" => translated.unsupported.push(
            "tool_search: deferred tool loading is not available with Anthropic models; \
all tools are declared up front instead"
                .to_string(),
        ),
        "web_search" => translated.unsupported.push(
            "web_search: OpenAI's hosted web search tool is not available with Anthropic models"
                .to_string(),
        ),
        "local_shell" => translated.unsupported.push(
            "local_shell: the OpenAI local_shell tool type is not available with Anthropic models"
                .to_string(),
        ),
        other => translated.unsupported.push(format!(
            "{other}: this hosted tool type is not available with Anthropic models"
        )),
    }
}

fn push_tool(
    translated: &mut TranslatedTools,
    seen: &mut BTreeSet<String>,
    namespace: Option<&str>,
    name: &str,
    kind: AnthropicToolKind,
    description: String,
    input_schema: Value,
) {
    let wire = translated.names.register(namespace, name, kind);
    if !seen.insert(wire.clone()) {
        // Duplicate declarations (for example from history-supplied tools) keep the first.
        return;
    }
    translated.tools.push(AnthropicTool {
        name: wire,
        description,
        input_schema,
    });
}

fn joined_description(namespace: Option<(&str, &str)>, declaration: &Value) -> String {
    let description = declaration
        .get("description")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    match namespace {
        Some((_, ns_description)) if !ns_description.trim().is_empty() => {
            if description.is_empty() {
                ns_description.to_string()
            } else {
                format!("{description}\n\n({ns_description})")
            }
        }
        _ => description,
    }
}

/// Drops sentences telling the model not to wrap freeform input in JSON, which is wrong once
/// the tool is exposed as a JSON tool with an `input` string.
fn without_freeform_wording(description: &str) -> String {
    description
        .split_inclusive(". ")
        .filter(|sentence| !sentence.contains("FREEFORM"))
        .collect::<String>()
        .trim()
        .to_string()
}

/// Anthropic requires `input_schema` to be a JSON Schema object with `type: "object"`.
fn normalize_input_schema(parameters: Option<&Value>) -> Value {
    match parameters {
        Some(Value::Object(schema)) => {
            let mut schema = schema.clone();
            if schema.get("type").and_then(Value::as_str) != Some("object") {
                schema.insert("type".to_string(), Value::String("object".to_string()));
            }
            if !schema.contains_key("properties") {
                schema.insert("properties".to_string(), Value::Object(Map::new()));
            }
            Value::Object(schema)
        }
        _ => json!({"type": "object", "properties": {}}),
    }
}

#[cfg(test)]
#[path = "tools_tests.rs"]
mod tests;
