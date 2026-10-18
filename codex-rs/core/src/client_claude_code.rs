//! Claude Code CLI transport for [`ModelClientSession`].
//!
//! The `claude-code` provider drives the user's installed, unmodified `claude` CLI, which signs
//! in on its own (for example with a Claude subscription). Claude Code's built-in tools are
//! disabled; Codex tools are exposed to it over an MCP bridge, and every call is executed by the
//! normal Codex tool router, so sandboxing and approvals are unchanged.

use std::path::PathBuf;
use std::sync::Arc;

use codex_api::anthropic::AnthropicThinking;
use codex_api::anthropic::resolve_claude_reasoning;
use codex_claude_code::ClaudeCodeSession;
use codex_claude_code::ClaudeCodeSessionConfig;
use codex_claude_code::ClaudeCodeTurnRequest;
use codex_claude_code::resolve_claude_command;
use codex_otel::SessionTelemetry;
use codex_protocol::config_types::ReasoningSummary as ReasoningSummaryConfig;
use codex_protocol::openai_models::ModelInfo;
use codex_protocol::openai_models::ReasoningEffort as ReasoningEffortConfig;
use codex_rollout_trace::InferenceTraceContext;
use codex_tools::create_tools_json_for_responses_api;

use super::ModelClient;
use super::ModelClientSession;
use super::map_response_stream;
use crate::client_common::Prompt;
use crate::client_common::ResponseStream;
use crate::responses_metadata::CodexResponsesMetadata;
use codex_protocol::error::CodexErr;
use codex_protocol::error::Result;

/// Directory under `CODEX_HOME` holding Claude Code bridge state (never credentials).
const CLAUDE_CODE_STATE_DIR: &str = "claude-code";

/// Session-level inputs needed to launch the Claude Code CLI.
#[derive(Debug, Clone)]
pub(crate) struct ClaudeCodeContext {
    pub(crate) cwd: PathBuf,
    pub(crate) codex_home: PathBuf,
    pub(crate) codex_self_exe: Option<PathBuf>,
}

impl ModelClient {
    /// Records what the Claude Code transport needs to launch the CLI for this session.
    pub(crate) fn with_claude_code_context(mut self, context: ClaudeCodeContext) -> Self {
        self.claude_code_context = Some(Arc::new(context));
        self
    }

    fn claude_code_session(&self) -> Result<Arc<ClaudeCodeSession>> {
        if let Some(session) = self.state.claude_code_session.get() {
            return Ok(Arc::clone(session));
        }
        let context = self.claude_code_context.as_ref().ok_or_else(|| {
            CodexErr::UnsupportedOperation(
                "the claude-code provider is not available in this session".to_string(),
            )
        })?;
        let bridge_command = match context.codex_self_exe.clone() {
            Some(path) => path,
            None => std::env::current_exe().map_err(CodexErr::Io)?,
        };
        let session = Arc::new(ClaudeCodeSession::new(ClaudeCodeSessionConfig {
            command: resolve_claude_command(/*explicit*/ None),
            bridge_command,
            bridge_args: vec!["stdio-to-uds".to_string()],
            cwd: context.cwd.clone(),
            thread_id: self.state.thread_id.to_string(),
            state_dir: context.codex_home.join(CLAUDE_CODE_STATE_DIR),
        }));
        Ok(Arc::clone(
            self.state.claude_code_session.get_or_init(|| session),
        ))
    }
}

impl ModelClientSession {
    /// Streams a turn through the Claude Code CLI.
    #[allow(clippy::too_many_arguments)]
    pub(super) async fn stream_claude_code(
        &mut self,
        prompt: &Prompt,
        model_info: &ModelInfo,
        session_telemetry: &SessionTelemetry,
        effort: Option<ReasoningEffortConfig>,
        summary: ReasoningSummaryConfig,
        service_tier: Option<String>,
        responses_metadata: &CodexResponsesMetadata,
        inference_trace: &InferenceTraceContext,
    ) -> Result<ResponseStream> {
        let client = &self.client;
        let session = client.claude_code_session()?;

        let effort = effort.or_else(|| model_info.default_reasoning_level.clone());
        let reasoning = resolve_claude_reasoning(
            &model_info.slug,
            effort.as_ref(),
            /*max_tokens_override*/ None,
        );
        let mut warnings = reasoning.adjustments.clone();
        warnings.extend(client.unsupported_claude_settings(summary, service_tier.as_deref()));
        if matches!(reasoning.thinking, AnthropicThinking::Disabled) {
            warnings.push(
                "Claude Code has no option to disable thinking; it uses its default thinking \
behavior instead of reasoning effort `none`"
                    .to_string(),
            );
        }
        if prompt.output_schema.is_some() {
            warnings.push(
                "output schemas are not supported by the claude-code provider and were ignored; \
use the anthropic provider for structured output"
                    .to_string(),
            );
        }
        client.emit_claude_warnings(responses_metadata, warnings);

        let request = ClaudeCodeTurnRequest {
            model: model_info.slug.clone(),
            effort: reasoning.effort.map(str::to_string),
            base_instructions: prompt.base_instructions.text.clone(),
            input: prompt.get_formatted_input_for_request(model_info),
            tools: create_tools_json_for_responses_api(&prompt.tools)?,
        };
        let inference_trace_attempt = inference_trace.start_attempt();
        let stream = session
            .stream(request)
            .await
            .map_err(|err| client.state.provider.map_api_error(err))?;
        inference_trace_attempt.record_started(&serde_json::json!({
            "transport": "claude_code",
            "model": model_info.slug,
        }));
        let (stream, _) = map_response_stream(
            stream,
            session_telemetry.clone(),
            inference_trace_attempt,
            Arc::clone(&client.state.provider),
            Vec::new(),
        );
        Ok(stream)
    }
}
