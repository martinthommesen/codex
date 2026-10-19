//! Anthropic Messages API support.
//!
//! Translates Codex conversation history and tool declarations into Messages API requests and
//! translates the streamed response back into Codex [`crate::ResponseEvent`]s, so Claude models
//! run through the same turn loop, tool router, sandbox, and approval flow as OpenAI models.

mod client;
mod error;
mod model_traits;
pub mod oauth;
mod oauth_response;
mod request;
mod stream;
mod tools;

pub use client::ANTHROPIC_VERSION;
pub use client::AnthropicApiKeyAuth;
pub use client::AnthropicMessagesClient;
pub use client::InvalidAnthropicApiKey;
pub use error::anthropic_stream_error;
pub use error::is_thinking_replay_rejection;
pub use error::map_anthropic_transport_error;
pub use model_traits::AnthropicThinking;
pub use model_traits::ClaudeModelTraits;
pub use model_traits::ClaudeThinkingStyle;
pub use model_traits::ResolvedClaudeReasoning;
pub use model_traits::resolve_claude_reasoning;
pub use oauth_response::ClaudeOAuthResponseMetadata;
pub use oauth_response::map_claude_oauth_transport_error;
pub use request::ANTHROPIC_REDACTED_THINKING_PREFIX;
pub use request::ANTHROPIC_THINKING_PREFIX;
pub use request::AnthropicMessage;
pub use request::AnthropicMessagesRequest;
pub use request::AnthropicRequestOptions;
pub use request::PreparedAnthropicRequest;
pub use request::THINKING_BINDING_BETA;
pub use request::TranslatedHistory;
pub use request::build_anthropic_request;
pub use request::is_anthropic_reasoning_item;
pub use request::translate_history;
pub use stream::AnthropicErrorBody;
pub use stream::AnthropicMessageDelta;
pub use stream::AnthropicMessageStart;
pub use stream::AnthropicStreamEvent;
pub use stream::AnthropicStreamTranslator;
pub use stream::AnthropicUsage;
pub use stream::spawn_anthropic_stream;
pub use tools::AnthropicTool;
pub use tools::AnthropicToolKind;
pub use tools::AnthropicToolNameMap;
pub use tools::AnthropicToolTarget;
pub use tools::FREEFORM_INPUT_PROPERTY;
pub use tools::TranslatedTools;
pub use tools::anthropic_tool_use_id;
pub use tools::anthropic_wire_tool_name;
pub use tools::anthropic_wire_tool_name_with_limit;
pub use tools::tool_name_collision_note;
pub use tools::translate_tools;

#[cfg(test)]
mod oauth_client_tests;
