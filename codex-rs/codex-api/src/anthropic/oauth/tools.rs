//! Request-local MCP symbol tables adapted from CLIProxyAPI (MIT).

use std::collections::BTreeMap;
use std::collections::BTreeSet;
use std::sync::OnceLock;

use hmac::Hmac;
use hmac::Mac;
use serde_json::Value;
use sha2::Sha256;

use super::OAuthCompatibilityError;

fn words() -> &'static [&'static str] {
    static WORDS: OnceLock<Vec<&'static str>> = OnceLock::new();
    WORDS.get_or_init(|| {
        include_str!("bip39_english.txt")
            .split_whitespace()
            .collect()
    })
}

fn digest(secret: &str, purpose: &str, original: &str) -> Result<Vec<u8>, OAuthCompatibilityError> {
    let mut mac = Hmac::<Sha256>::new_from_slice(secret.as_bytes())
        .map_err(|_| OAuthCompatibilityError::InvalidToolAlias)?;
    mac.update(b"cpa-claude-mcp-alias-v2\0");
    mac.update(purpose.as_bytes());
    mac.update(b"\0");
    mac.update(original.as_bytes());
    Ok(mac.finalize().into_bytes().to_vec())
}

fn word(digest: &[u8], offset: usize, attempt: usize) -> &'static str {
    let index = usize::from(u16::from_be_bytes([digest[offset], digest[offset + 1]]));
    words()[(index + attempt) % words().len()]
}

fn semantic_suffix(original: &str, max_length: usize) -> String {
    let mut result = String::new();
    let mut pending_separator = false;
    for ch in original.chars() {
        if !ch.is_ascii_alphanumeric() && ch != '_' && ch != '-' {
            pending_separator = !result.is_empty();
            continue;
        }
        if pending_separator && result.len() + 1 < max_length {
            result.push('_');
        }
        pending_separator = false;
        if result.len() >= max_length {
            break;
        }
        result.push(ch);
    }
    let trimmed = result.trim_matches(['_', '-']);
    if trimmed.is_empty() {
        "tool".into()
    } else {
        trimmed.into()
    }
}

pub(super) fn is_mcp_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 64
        && name.strip_prefix("mcp__").is_some_and(|rest| {
            rest.split_once("__")
                .is_some_and(|(server, tool)| !server.is_empty() && !tool.is_empty())
        })
        && name
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_' || byte == b'-')
}

fn server_tool(tool: &Value) -> bool {
    let kind = tool["type"]
        .as_str()
        .unwrap_or_default()
        .trim()
        .to_ascii_lowercase();
    [
        "advisor_",
        "agent_toolset_",
        "bash_",
        "code_execution_",
        "computer_",
        "memory_",
        "text_editor_",
        "tool_search_tool_",
        "web_fetch_",
        "web_search_",
    ]
    .iter()
    .any(|prefix| kind.starts_with(prefix))
}

/// Map declaration and history names through one symbol table. No global/account state is used.
pub(super) fn remap_tools(
    body: &mut Value,
    secret: &str,
) -> Result<BTreeMap<String, String>, OAuthCompatibilityError> {
    if words().len() != 2048 {
        return Err(OAuthCompatibilityError::InvalidToolAlias);
    }
    let mut reserved: BTreeSet<String> =
        ["web_search", "code_execution", "text_editor", "computer"]
            .into_iter()
            .map(str::to_owned)
            .collect();
    let mut protected = BTreeSet::new();
    let mut forward = BTreeMap::new();
    let mut reverse = BTreeMap::new();
    let server_digest = digest(secret, "server", "")?;
    let server = format!(
        "{}_{}",
        word(&server_digest, 0, 0),
        word(&server_digest, 2, 0)
    );
    // Canonical union order keeps collision probing stable when a declaration disappears but
    // remains in replayed history. A newly introduced colliding symbol can still change a probe.
    let mut names = BTreeSet::new();
    if let Some(tools) = body.get("tools").and_then(Value::as_array) {
        for tool in tools {
            if let Some(name) = tool["name"].as_str() {
                reserved.insert(name.to_owned());
                names.insert(name.to_owned());
                if server_tool(tool) {
                    protected.insert(name.to_owned());
                }
            }
        }
    }
    // Codex replays complete history instead of CPA's opaque server threads. Include known
    // historical symbols even when dynamic tool discovery removed their current declaration.
    // The source alias algorithm is unchanged; tool inputs/results are never searched as text.
    if let Some(messages) = body.get("messages").and_then(Value::as_array) {
        for message in messages {
            if let Some(blocks) = message.get("content").and_then(Value::as_array) {
                for block in blocks {
                    for path in symbol_paths(block) {
                        if let Some(name) = block.pointer(&path).and_then(Value::as_str) {
                            reserved.insert(name.to_owned());
                            names.insert(name.to_owned());
                        }
                    }
                }
            }
        }
    }
    for name in names {
        if name.is_empty() || protected.contains(&name) {
            continue;
        }
        if is_mcp_name(&name) {
            reverse.insert(name.clone(), name);
        } else if !forward.contains_key(&name) {
            let tool_digest = digest(secret, "tool", &name)?;
            let mut allocated = None;
            for attempt in 0..words().len() {
                let prefix = format!("mcp__{server}__{}_", word(&tool_digest, 0, attempt));
                let alias = format!("{prefix}{}", semantic_suffix(&name, 64 - prefix.len()));
                if !reserved.contains(&alias) {
                    allocated = Some(alias);
                    break;
                }
            }
            let alias = allocated.ok_or(OAuthCompatibilityError::InvalidToolAlias)?;
            reserved.insert(alias.clone());
            forward.insert(name.clone(), alias.clone());
            reverse.insert(alias, name);
        }
    }
    if let Some(tools) = body.get_mut("tools").and_then(Value::as_array_mut) {
        for tool in tools {
            if server_tool(tool) {
                continue;
            }
            let name = tool["name"].as_str().unwrap_or_default().to_owned();
            if let Some(object) = tool.as_object_mut() {
                object.remove("type");
                if let Some(alias) = forward.get(&name) {
                    object.insert("name".into(), Value::String(alias.clone()));
                }
            }
        }
    }
    if body["tool_choice"]["type"] == "tool" {
        rename_field(&mut body["tool_choice"], "name", &forward, &protected);
    }
    if let Some(messages) = body.get_mut("messages").and_then(Value::as_array_mut) {
        for message in messages {
            if let Some(blocks) = message.get_mut("content").and_then(Value::as_array_mut) {
                for block in blocks {
                    rewrite_block(block, &forward, &protected);
                }
            }
        }
    }
    Ok(reverse)
}

fn rename_field(
    block: &mut Value,
    field: &str,
    forward: &BTreeMap<String, String>,
    protected: &BTreeSet<String>,
) {
    if let Some(name) = block.get_mut(field)
        && let Some(original) = name.as_str()
        && !protected.contains(original)
        && let Some(alias) = forward.get(original)
    {
        *name = Value::String(alias.clone());
    }
}

fn rewrite_block(
    block: &mut Value,
    forward: &BTreeMap<String, String>,
    protected: &BTreeSet<String>,
) {
    for path in symbol_paths(block) {
        if let Some(name) = block.pointer_mut(&path)
            && let Some(original) = name.as_str()
            && !protected.contains(original)
            && let Some(alias) = forward.get(original)
        {
            *name = Value::String(alias.clone());
        }
    }
}

/// Paths are shared by discovery and rewriting to keep the symbol table complete. No path
/// enters a tool_use input or arbitrary tool-result object supplied by an external service.
fn symbol_paths(block: &Value) -> Vec<String> {
    match block["type"].as_str().unwrap_or_default() {
        "tool_use" => vec!["/name".into()],
        "tool_reference" => vec!["/tool_name".into()],
        "tool_result" => {
            let mut paths = Vec::new();
            if let Some(content) = block.get("content").and_then(Value::as_array) {
                for (index, nested) in content.iter().enumerate() {
                    if nested["type"] == "tool_reference" {
                        paths.push(format!("/content/{index}/tool_name"));
                    }
                }
            }
            paths
        }
        "tool_search_tool_result" => {
            let mut paths = Vec::new();
            if let Some(references) = block
                .pointer("/content/tool_references")
                .and_then(Value::as_array)
            {
                for (index, reference) in references.iter().enumerate() {
                    if reference["type"] == "tool_reference" {
                        paths.push(format!("/content/tool_references/{index}/tool_name"));
                    }
                }
            }
            paths
        }
        "tool_addition" | "tool_removal" => match block["tool"]["type"].as_str() {
            Some("tool_reference") => vec!["/tool/name".into()],
            Some("tool_definition")
                if block["type"] == "tool_addition"
                    && !server_tool(&block["tool"]["definition"]) =>
            {
                vec!["/tool/definition/name".into()]
            }
            _ => Vec::new(),
        },
        _ => Vec::new(),
    }
}
