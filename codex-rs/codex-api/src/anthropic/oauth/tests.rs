#![allow(clippy::unwrap_used)]

use super::*;
use crate::ResponseEvent;
use crate::anthropic::AnthropicStreamEvent;
use crate::anthropic::AnthropicStreamTranslator;
use crate::anthropic::AnthropicToolKind;
use crate::anthropic::AnthropicToolNameMap;
use pretty_assertions::assert_eq;

// Independent fixtures retained from claude_signing_test.go at the pinned reference commit.
const BASE_BODY: &str = r#"{"model":"model-a","messages":[{"role":"user","content":[{"type":"text","text":"x"}]}],"system":[{"type":"text","text":"x-anthropic-billing-header: cc_version=2.1.220.test; cc_entrypoint=sdk-cli; cch=00000;"},{"type":"text","text":"system-x"}],"tools":[],"metadata":{"user_id":"meta-x"},"max_tokens":1,"thinking":{"type":"adaptive","display":"omitted"},"context_management":{"edits":[{"type":"clear_thinking_20251015","keep":"all"}]},"output_config":{"effort":"high"},"stream":true}"#;

#[test]
fn cch_matches_reference_vectors() {
    let vectors = [
        (BASE_BODY.to_owned(), "7ee87"),
        (BASE_BODY.replace(r#""model":"model-a""#, r#""model":"model-b""#), "7ee87"),
        (BASE_BODY.replace(r#""max_tokens":1"#, r#""max_tokens":2"#), "7ee87"),
        (BASE_BODY.replace(r#""text":"x""#, r#""text":"y""#), "b9cc8"),
        (BASE_BODY.replace("system-x", "system-y"), "a30d3"),
        (BASE_BODY.replace("meta-x", "meta-y"), "7a89d"),
        (BASE_BODY.replace(r#""thinking":{"type":"adaptive","display":"omitted"}"#, r#""thinking":{"type":"disabled"}"#), "7205c"),
        (BASE_BODY.replace(r#""context_management":{"edits":[{"type":"clear_thinking_20251015","keep":"all"}]}"#, r#""context_management":{"edits":[]}"#), "05073"),
        (BASE_BODY.replace(r#""effort":"high""#, r#""effort":"low""#), "12366"),
        (BASE_BODY.replace(r#""stream":true"#, r#""stream":false"#), "60400"),
        (BASE_BODY.replace(r#""tools":[]"#, r#""tools":[{"name":"t","description":"d","input_schema":{"type":"object"}}]"#), "3d78d"),
        (BASE_BODY.replace(r#""stream":true}"#, r#""stream":true,"extra_top":"extra"}"#), "2d622"),
        (BASE_BODY.replace(r#""metadata":{"user_id":"meta-x"}"#, r#""metadata":{"user_id":"meta-x","model":"a"}"#), "0601b"),
        (BASE_BODY.replace(r#""metadata":{"user_id":"meta-x"}"#, r#""metadata":{"user_id":"meta-x","max_tokens":2}"#), "7ee87"),
        (BASE_BODY.replace(r#""stream":true}"#, r#""stream":true,"fallbacks":[{"model":"fallback-a"}]}"#), "7ee87"),
        (BASE_BODY.replace(r#""stream":true}"#, r#""stream":true,"fallback_credit_token":"a"}"#), "7ee87"),
        (BASE_BODY.replace(r#""metadata":{"user_id":"meta-x"}"#, r#""metadata":{"user_id":"meta-x","max_tokens":999,"fallbacks":[{"model":"fallback-model"}]}"#), "4589b"),
        (BASE_BODY.replace(r#""metadata":{"user_id":"meta-x"}"#, r#""metadata":{"user_id":"meta-x","model":"nested-model","max_tokens":999,"fallbacks":[{"model":"fallback-model"}],"fallback_credit_token":"not-a-real-token"}"#), "2d312"),
        (BASE_BODY.replace(r#""metadata":{"user_id":"meta-x"}"#, r#""metadata":{"user_id":"meta-x","max_tokens":999,"model":"nested-model","fallbacks":[{"model":"fallback-model"}]}"#), "0601b"),
        (r#"{"system":[{"type":"text","text":"x-anthropic-billing-header: cc_version=2.1.220.test; cc_entrypoint=sdk-cli; cch=00000;"}]}"#.to_owned(), "f2edb"),
    ];
    for (body, expected) in vectors {
        let mut signed = body.into_bytes();
        sign_body(&mut signed).unwrap();
        let text = String::from_utf8(signed).unwrap();
        assert!(text.contains(&format!("cch={expected};")), "{text}");
    }
}

#[test]
fn cch_normalization_preserves_bytes_and_native_trailing_run_quirk() {
    for (body, expected) in [
        (r#"{"model":"claude","keep":1}"#, r#"{"model":"","keep":1}"#),
        (r#"{"max_tokens":1,"keep":2}"#, r#"{"keep":2}"#),
        (
            r#"{"keep":1,"fallbacks":[{"model":"x"}],"tail":2}"#,
            r#"{"keep":1,"tail":2}"#,
        ),
        (
            r#"{"keep":1,"fallback_credit_token":"secret"}"#,
            r#"{"keep":1}"#,
        ),
        (
            r#"{"max_tokens":1,"fallbacks":[],"fallback_credit_token":"secret"}"#,
            r#"{}"#,
        ),
        (
            r#"{"keep":1,"max_tokens":1,"fallbacks":[]}"#,
            r#"{"keep":1,}"#,
        ),
        (
            r#"{"outer":{"model":"x","max_tokens":1,"keep":"y"}}"#,
            r#"{"outer":{"model":"","keep":"y"}}"#,
        ),
        (
            r#"{"text":"literal \"model\":\"x\" and \"max_tokens\":1"}"#,
            r#"{"text":"literal \"model\":\"x\" and \"max_tokens\":1"}"#,
        ),
        (
            "{ \"model\" : \"x\", \"keep\": 1 }",
            "{ \"model\" : \"\", \"keep\": 1 }",
        ),
        (r#"{"mo\u0064el":"kept"}"#, r#"{"mo\u0064el":"kept"}"#),
    ] {
        assert_eq!(
            String::from_utf8(normalize_cch_input(body.as_bytes()).unwrap()).unwrap(),
            expected
        );
    }
    assert!(normalize_cch_input(b"{broken}").is_err());
}

#[test]
fn signing_changes_only_the_billing_cch_digits() {
    let body = BASE_BODY.replace(
        r#""text":"x""#,
        r#""text":"keep literal cch=00000; in the message""#,
    );
    let mut signed = body.as_bytes().to_vec();
    sign_body(&mut signed).unwrap();
    let value: Value = serde_json::from_slice(&signed).unwrap();
    assert_eq!(
        value["messages"][0]["content"][0]["text"],
        "keep literal cch=00000; in the message"
    );
    let billing = value["system"][0]["text"].as_str().unwrap();
    let checksum = billing
        .split_once("cch=")
        .unwrap()
        .1
        .split_once(';')
        .unwrap()
        .0;
    assert_eq!(
        String::from_utf8(signed)
            .unwrap()
            .replacen(&format!("cch={checksum};"), "cch=00000;", 1),
        body
    );
    assert!(sign_body(&mut br#"{"system":[]}"#.to_vec()).is_err());
}

#[test]
fn billing_fingerprint_matches_native_utf16_vectors() {
    assert_eq!(
        billing_fingerprint("😀abcdefghijklmnopqrstuvwxyz", "2.1.280"),
        "dac"
    );
    assert_eq!(
        billing_fingerprint("abc😀abcdefghijklmnop", "2.1.280"),
        "695"
    );
}

fn context() -> OAuthRequestContext {
    OAuthRequestContext {
        account_uuid: "11111111-2222-4333-8444-555555555555".into(),
        device_id: "ab".repeat(32),
        session_id: "aaaaaaaa-bbbb-4ccc-8ddd-eeeeeeeeeeee".into(),
        ..Default::default()
    }
}

fn request() -> Value {
    json!({
        "model":"claude-opus-5-5", "max_tokens":4096,
        "system":[{"type":"text","text":"Codex rules and approvals"}],
        "messages":[{"role":"user","content":[{"type":"text","text":"read the file"},{"type":"image","source":{"type":"base64","media_type":"image/png","data":"cGl4ZWw="}}]}],
        "tools":[{"name":"exec_command","description":"Run shell with Codex approvals","input_schema":{"type":"object","properties":{"cmd":{"type":"string"}}}}],
        "thinking":{"type":"adaptive","display":"summarized","block_binding":{"prefix_mismatch_behavior":"drop_block"}},
        "output_config":{"effort":"high"},"cache_control":{"type":"ephemeral"},"stream":true
    })
}

#[test]
fn direct_oauth_request_has_exact_identity_headers_and_preserves_codex_prompt() {
    let prepared = prepare_oauth_request(request(), &context()).unwrap();
    let body: Value = serde_json::from_slice(&prepared.body).unwrap();
    assert_eq!(
        CLAUDE_OAUTH_MESSAGES_URL,
        "https://api.anthropic.com/v1/messages?beta=true"
    );
    assert_eq!(prepared.headers["accept"], "application/json");
    assert_eq!(
        prepared.headers["user-agent"],
        "claude-cli/2.1.280 (external, cli)"
    );
    assert_eq!(prepared.headers["anthropic-version"], "2023-06-01");
    assert_eq!(
        prepared.headers["x-claude-code-session-id"],
        context().session_id
    );
    assert!(!prepared.headers.contains_key("authorization"));
    assert!(!prepared.headers.contains_key("x-api-key"));
    let betas = prepared.headers["anthropic-beta"].to_str().unwrap();
    assert!(betas.starts_with("claude-code-20250219,oauth-2025-04-20,interleaved-thinking-2025-05-14,thinking-token-count-2026-05-13"));
    assert!(!betas.contains("redact-thinking"));
    assert!(betas.contains("per-turn-control-2026-07-01"));
    assert!(betas.contains("thinking-binding-controls-2026-08-01"));
    assert!(!betas.contains("fallback-credit"));
    assert_eq!(body["system"][1]["text"], CLI_IDENTITY);
    assert_eq!(body["messages"][1]["role"], "system");
    assert_eq!(
        body["messages"][1]["content"][0]["text"],
        "Codex rules and approvals"
    );
    assert_eq!(
        body["messages"][0]["content"][1]["source"]["data"],
        "cGl4ZWw="
    );
    assert_eq!(body["context_management"]["edits"][0]["keep"], "all");
    assert_eq!(body["thinking"]["display"], "summarized");
    assert_eq!(body["system"][1]["cache_control"]["ttl"], "1h");
    assert!(body.get("cache_control").is_none());
    let identity: Value =
        serde_json::from_str(body["metadata"]["user_id"].as_str().unwrap()).unwrap();
    assert_eq!(identity["account_uuid"], context().account_uuid);
    assert_eq!(identity["device_id"], context().device_id);
    assert_eq!(identity["session_id"], context().session_id);
    // Signing is idempotent and covers the exact final bytes.
    let mut signed = prepared.body.clone();
    sign_body(&mut signed).unwrap();
    assert_eq!(signed, prepared.body);
}

#[test]
fn tool_aliases_cover_declarations_history_and_tool_choice() {
    let mut body = json!({
        "tools":[{"name":"exec_command","type":"custom","input_schema":{"type":"object"}}, {"name":"mcp__server__lookup","input_schema":{"type":"object"}}, {"name":"web_search","type":"web_search_20250305"}],
        "tool_choice":{"type":"tool","name":"exec_command"},
        "messages":[{"role":"assistant","content":[{"type":"tool_use","id":"x","name":"exec_command","input":{"name":"exec_command"}}]},
            {"role":"user","content":[{"type":"tool_result","tool_use_id":"x","content":[{"type":"tool_reference","tool_name":"exec_command"}]}]}]
    });
    let map = tools::remap_tools(&mut body, "cpa-claude-mcp-default-caller").unwrap();
    // Observed fixture in reference testdata/tool_search_disabled/dev_executor_golden.json.
    let alias = "mcp__select_sight__push_exec_command";
    assert_eq!(body["tools"][0]["name"], alias);
    assert_eq!(map[alias], "exec_command");
    assert_eq!(body["tool_choice"]["name"], alias);
    assert_eq!(body["messages"][0]["content"][0]["name"], alias);
    assert_eq!(
        body["messages"][0]["content"][0]["input"]["name"],
        "exec_command"
    );
    assert_eq!(
        body["messages"][1]["content"][0]["content"][0]["tool_name"],
        alias
    );
    assert!(body["tools"][0].get("type").is_none());
    assert_eq!(body["tools"][1]["name"], "mcp__server__lookup");
    assert_eq!(body["tools"][2]["type"], "web_search_20250305");
}

#[test]
fn tool_alias_stream_round_trip_preserves_existing_tool_runtime_mapping() {
    let prepared = prepare_oauth_request(request(), &context()).unwrap();
    let (alias, original) = prepared.tool_names.iter().next().unwrap();
    let mut names = AnthropicToolNameMap::default();
    names.register(None, original, AnthropicToolKind::Function);
    names.insert(alias.clone(), names.resolve(original));
    let mut translator = AnthropicStreamTranslator::new(names);
    let events: Vec<AnthropicStreamEvent> = [
        json!({"type":"message_start","message":{"id":"msg_test","model":"claude-opus-5-5"}}),
        json!({"type":"content_block_start","index":0,"content_block":{"type":"tool_use","id":"toolu_1","name":alias,"input":{}}}),
        json!({"type":"content_block_delta","index":0,"delta":{"type":"input_json_delta","partial_json":"{\"cmd\":\"pwd\"}"}}),
        json!({"type":"content_block_stop","index":0}),
        json!({"type":"message_delta","delta":{"stop_reason":"tool_use"},"usage":{"output_tokens":12}}),
        json!({"type":"message_stop"}),
    ].into_iter().map(|value| serde_json::from_value(value).unwrap()).collect();
    let mut output = Vec::new();
    for event in events {
        output.extend(translator.handle(event).unwrap());
    }
    assert!(output.iter().any(|event| matches!(event, ResponseEvent::OutputItemDone(codex_protocol::models::ResponseItem::FunctionCall { name, arguments, .. }) if name == "exec_command" && arguments.contains("pwd"))));
}

#[test]
fn identity_isolation_rejects_missing_credentials_and_retains_identity_across_refresh() {
    assert!(matches!(
        prepare_oauth_request(request(), &OAuthRequestContext::default()),
        Err(OAuthCompatibilityError::InvalidIdentity)
    ));
    let first = prepare_oauth_request(request(), &context()).unwrap();
    let second = prepare_oauth_request(request(), &context()).unwrap();
    assert_eq!(first.body, second.body);
    assert_eq!(first.tool_names, second.tool_names);
    // A different selected account has its own device, which changes metadata and aliases.
    let mut other = context();
    other.account_uuid = "bbbbbbbb-cccc-4ddd-8eee-ffffffffffff".into();
    other.device_id = "cd".repeat(32);
    let other = prepare_oauth_request(request(), &other).unwrap();
    assert_ne!(first.body, other.body);
    assert_ne!(first.tool_names, other.tool_names);
}

#[test]
fn paid_fallbacks_and_fast_mode_are_rejected_without_switching_auth() {
    for (key, value) in [
        ("fallbacks", json!([])),
        ("fallback_credit_token", json!("credit")),
        ("speed", json!("fast")),
    ] {
        let mut request = request();
        request[key] = value;
        assert!(matches!(
            prepare_oauth_request(request, &context()),
            Err(OAuthCompatibilityError::PaidFallbackNotAllowed)
        ));
    }
}

#[test]
fn legacy_models_relocate_system_as_reminder_and_subagents_use_default_cache_ttl() {
    let mut request = request();
    request["model"] = json!("claude-sonnet-4-6");
    let mut context = context();
    context.is_subagent = true;
    let prepared = prepare_oauth_request(request, &context).unwrap();
    let body: Value = serde_json::from_slice(&prepared.body).unwrap();
    assert!(
        body["messages"][0]["content"][0]["text"]
            .as_str()
            .unwrap()
            .starts_with("<system-reminder>\nCodex rules")
    );
    assert_eq!(body["messages"].as_array().unwrap().len(), 1);
    assert!(body["system"][1]["cache_control"].get("ttl").is_none());
    assert!(
        !prepared.headers["anthropic-beta"]
            .to_str()
            .unwrap()
            .contains("extended-cache-ttl")
    );
    assert!(
        body["system"][0]["text"]
            .as_str()
            .unwrap()
            .contains("cc_is_subagent=true;")
    );
}

#[test]
fn forced_tools_disable_thinking_and_keep_sampling_valid() {
    let mut request = request();
    request["tool_choice"] = json!({"type":"any"});
    request["temperature"] = json!(0.2);
    request["top_p"] = json!(0.9);
    let prepared = prepare_oauth_request(request, &context()).unwrap();
    let body: Value = serde_json::from_slice(&prepared.body).unwrap();
    assert!(body.get("thinking").is_none());
    assert!(body.get("temperature").is_none());
    assert!(body.get("top_p").is_none());
    assert!(body.get("context_management").is_none());
    assert!(body["output_config"].get("effort").is_none());
}

#[test]
fn cache_limit_preserves_last_system_and_tools_before_older_messages() {
    let marker = json!({"type":"ephemeral"});
    let mut body = json!({
        "tools":[{"cache_control":marker},{"cache_control":marker}],
        "system":[{"cache_control":marker},{"cache_control":marker}],
        "messages":[{"content":[{"cache_control":marker},{"cache_control":marker},{"cache_control":marker}]}]
    });
    enforce_cache_limit(&mut body, 4);
    assert!(body["system"][0].get("cache_control").is_none());
    assert!(body["tools"][0].get("cache_control").is_none());
    assert!(
        body["messages"][0]["content"][0]
            .get("cache_control")
            .is_none()
    );
    assert_eq!(body["system"][1]["cache_control"], marker);
    assert_eq!(body["tools"][1]["cache_control"], marker);
}

#[test]
fn cache_ttl_normalization_never_sends_one_hour_after_five_minutes() {
    let mut request = request();
    request["tools"][0]["cache_control"] = json!({"type":"ephemeral","ttl":"5m"});
    let prepared = prepare_oauth_request(request, &context()).unwrap();
    let mut body: Value = serde_json::from_slice(&prepared.body).unwrap();
    let mut seen_five_minutes = false;
    visit_cache_blocks(&mut body, |block| {
        if let Some(cache) = block.get("cache_control") {
            if seen_five_minutes {
                assert_ne!(cache["ttl"], "1h");
            }
            if cache["ttl"] != "1h" {
                seen_five_minutes = true;
            }
        }
    });
    assert!(seen_five_minutes);
}

#[test]
fn long_and_colliding_tool_names_remain_unique_and_bounded() {
    let prefix = "a".repeat(200);
    let mut body = json!({"tools":[
        {"name":format!("{prefix}x")},{"name":format!("{prefix}y")},
        {"name":"search.网页/tool with spaces"},{"name":"搜索网页"}
    ]});
    let aliases = tools::remap_tools(&mut body, "credential-secret").unwrap();
    assert_eq!(aliases.len(), 4);
    for alias in aliases.keys() {
        assert!(tools::is_mcp_name(alias));
        assert!(alias.len() <= 64);
    }
    assert!(
        aliases
            .keys()
            .any(|alias| alias.ends_with("_search_tool_with_spaces"))
    );
    assert!(aliases.keys().any(|alias| alias.ends_with("_tool")));
}

#[test]
fn all_reference_legacy_model_families_avoid_mid_conversation_system_roles() {
    for model in [
        "claude-haiku-4-5",
        "claude-opus-4-1",
        "claude-3-7-sonnet-latest",
        "provider/CLAUDE-SONNET-4-6",
    ] {
        let mut input = request();
        input["model"] = json!(model);
        let prepared = prepare_oauth_request(input, &context()).unwrap();
        let body: Value = serde_json::from_slice(&prepared.body).unwrap();
        assert!(
            body["messages"]
                .as_array()
                .unwrap()
                .iter()
                .all(|message| message["role"] != "system")
        );
        assert!(
            !prepared.headers["anthropic-beta"]
                .to_str()
                .unwrap()
                .contains("mid-conversation-system-2026-04-07")
        );
    }
}

#[test]
fn removed_dynamic_tools_keep_the_same_alias_in_replayed_history() {
    let secret = "credential-secret";
    let mut first = json!({"tools":[{"name":"exec_command"},{"name":"lookup_file"}]});
    let first_map = tools::remap_tools(&mut first, secret).unwrap();
    let original_alias = first_map
        .iter()
        .find(|(_, name)| name.as_str() == "exec_command")
        .unwrap()
        .0;
    let mut replay = json!({
        "tools":[{"name":"lookup_file"}],
        "messages":[
            {"role":"assistant","content":[{"type":"tool_use","id":"old_call","name":"exec_command","input":{"name":"exec_command"}}]},
            {"role":"user","content":[{"type":"tool_result","tool_use_id":"old_call","content":[{"type":"tool_reference","tool_name":"exec_command"}]}]}
        ]
    });
    let replay_map = tools::remap_tools(&mut replay, secret).unwrap();
    assert_eq!(replay["tools"].as_array().unwrap().len(), 1);
    assert_eq!(
        replay["messages"][0]["content"][0]["name"],
        original_alias.as_str()
    );
    assert_eq!(
        replay["messages"][0]["content"][0]["input"]["name"],
        "exec_command"
    );
    assert_eq!(
        replay["messages"][1]["content"][0]["content"][0]["tool_name"],
        original_alias.as_str()
    );
    assert_eq!(replay_map, first_map);
}

#[test]
fn tool_changes_are_discovered_without_rewriting_server_tools_or_input_schemas() {
    let mut body = json!({"messages":[{"role":"user","content":[
        {"type":"tool_addition","tool":{"type":"tool_definition","definition":{"name":"later_tool","input_schema":{"properties":{"name":{"const":"later_tool"}}}}}},
        {"type":"tool_removal","tool":{"type":"tool_reference","name":"later_tool"}},
        {"type":"tool_addition","tool":{"type":"tool_definition","definition":{"type":"web_search_20250305","name":"web_search"}}}
    ]}]});
    let map = tools::remap_tools(&mut body, "credential-secret").unwrap();
    let alias = map
        .iter()
        .find(|(_, name)| name.as_str() == "later_tool")
        .unwrap()
        .0;
    assert_eq!(
        body.pointer("/messages/0/content/0/tool/definition/name")
            .unwrap(),
        alias
    );
    assert_eq!(
        body.pointer("/messages/0/content/1/tool/name").unwrap(),
        alias
    );
    assert_eq!(
        body.pointer("/messages/0/content/0/tool/definition/input_schema/properties/name/const")
            .unwrap(),
        "later_tool"
    );
    assert_eq!(
        body.pointer("/messages/0/content/2/tool/definition/name")
            .unwrap(),
        "web_search"
    );
}

#[test]
fn historical_union_keeps_collision_probes_stable_after_declaration_removal() {
    // Independently derived reference HMAC fixture: both names select BIP-39 word index 1555
    // with this seed, and their truncated semantic suffixes are identical.
    let first_name = format!("{}56", "a".repeat(100));
    let second_name = format!("{}88", "a".repeat(100));
    let mut original = json!({"tools":[{"name":second_name},{"name":first_name}]});
    let original_map = tools::remap_tools(&mut original, "credential-secret").unwrap();
    let mut replay = json!({
        "tools":[{"name":second_name}],
        "messages":[{"role":"assistant","content":[{"type":"tool_use","name":first_name,"input":{}}]}]
    });
    let replay_map = tools::remap_tools(&mut replay, "credential-secret").unwrap();
    assert_eq!(replay_map, original_map);
    assert_eq!(replay_map.len(), 2);
}
