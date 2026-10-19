//! Anthropic Messages API transport for [`ModelClientSession`].
//!
//! Requests are translated from the provider-neutral [`Prompt`] by `codex_api::anthropic`, and the
//! streamed response is mapped back into ordinary [`ResponseEvent`]s, so the turn loop, tool
//! router, sandboxing, approvals, and history handling are identical to OpenAI turns.

use std::collections::HashSet;

use codex_api::anthropic::AnthropicMessagesClient;
use codex_api::anthropic::AnthropicRequestOptions;
use codex_api::anthropic::build_anthropic_request;
use codex_api::anthropic::is_thinking_replay_rejection;
use codex_api::anthropic::resolve_claude_reasoning;
use codex_login::default_client::ClientRedirectPolicy;
use codex_otel::SessionTelemetry;
use codex_protocol::config_types::ReasoningSummary as ReasoningSummaryConfig;
use codex_protocol::openai_models::ModelInfo;
use codex_protocol::openai_models::ReasoningEffort as ReasoningEffortConfig;
use codex_protocol::protocol::Event as ProtocolEvent;
use codex_protocol::protocol::EventMsg;
use codex_protocol::protocol::WarningEvent;
use codex_rollout_trace::InferenceTraceContext;
use codex_tools::create_tools_json_for_responses_api;
use http::HeaderMap;
use tracing::warn;

use super::AuthRequestTelemetryContext;
use super::ClientRouting;
use super::ModelClient;
use super::ModelClientSession;
use super::PendingUnauthorizedRetry;
use super::RequestRouteTelemetry;
use super::map_response_stream;
use crate::client_common::Prompt;
use crate::client_common::ResponseStream;
use crate::responses_metadata::CodexResponsesMetadata;
use codex_protocol::error::CodexErr;
use codex_protocol::error::Result;

const MESSAGES_PATH: &str = "/messages";

impl ModelClientSession {
    /// Streams a turn through the Anthropic Messages API.
    #[allow(clippy::too_many_arguments)]
    pub(super) async fn stream_anthropic_messages(
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
        let setup = client
            .current_client_setup(ClientRouting::ConfiguredProvider)
            .await?;
        // The key travels in `x-api-key`, which redirect handling does not strip on a
        // cross-origin hop, and the Messages API never redirects.
        let transport = client.build_api_transport(
            &setup.api_provider,
            MESSAGES_PATH,
            ClientRedirectPolicy::Reject,
        )?;
        let request_auth_context = AuthRequestTelemetryContext::new(
            /*auth_mode*/ None,
            setup.api_auth.as_ref(),
            /*agent_identity_telemetry*/ None,
            PendingUnauthorizedRetry::default(),
        );
        let (request_telemetry, sse_telemetry) = ModelClientSession::build_streaming_telemetry(
            session_telemetry,
            request_auth_context,
            RequestRouteTelemetry::for_endpoint(MESSAGES_PATH),
            client.state.auth_env_telemetry.clone(),
        );

        let effort = effort.or_else(|| model_info.default_reasoning_level.clone());
        let reasoning = resolve_claude_reasoning(
            &model_info.slug,
            effort.as_ref(),
            /*max_tokens_override*/ None,
        );
        let tools = create_tools_json_for_responses_api(&prompt.tools)?;
        let input = prompt.get_formatted_input_for_request(model_info);
        let mut warnings = reasoning.adjustments.clone();
        warnings.extend(client.unsupported_claude_settings(summary, service_tier.as_deref()));

        let mut strip_thinking = false;
        loop {
            let options = AnthropicRequestOptions {
                model: model_info.slug.clone(),
                max_tokens: reasoning.max_tokens,
                thinking: reasoning.thinking.clone(),
                summarize_thinking: !matches!(summary, ReasoningSummaryConfig::None),
                effort: reasoning.effort,
                parallel_tool_calls: prompt.parallel_tool_calls,
                prompt_caching: true,
                output_schema: prompt.output_schema.clone(),
                drop_mismatched_thinking: !strip_thinking,
                strip_thinking,
            };
            let prepared =
                build_anthropic_request(&prompt.base_instructions.text, &input, &tools, &options);
            let mut attempt_warnings = warnings.clone();
            attempt_warnings.extend(prepared.notes.iter().cloned());
            client.emit_claude_warnings(responses_metadata, attempt_warnings);

            let mut headers = HeaderMap::new();
            let inference_trace_attempt = inference_trace.start_attempt();
            inference_trace_attempt.add_request_headers(&mut headers);
            inference_trace_attempt.record_started(&prepared.request);
            let api_client = AnthropicMessagesClient::new(
                transport.clone(),
                setup.api_provider.clone(),
                setup.api_auth.clone(),
            )
            .with_telemetry(Some(request_telemetry.clone()), Some(sse_telemetry.clone()));
            match api_client.stream(&prepared, headers).await {
                Ok(stream) => {
                    let (stream, _) = map_response_stream(
                        stream,
                        session_telemetry.clone(),
                        inference_trace_attempt,
                        std::sync::Arc::clone(&client.state.provider),
                        Vec::new(),
                    );
                    return Ok(stream);
                }
                Err(err) if !strip_thinking && is_thinking_replay_rejection(&err) => {
                    // A replayed thinking block was rejected (for example after history was
                    // edited). Retry once without replayed thinking, as Anthropic recommends.
                    warn!("Anthropic rejected replayed thinking blocks; retrying without them");
                    inference_trace_attempt.record_failed(
                        &err,
                        /*upstream_request_id*/ None,
                        /*output_items*/ &[],
                    );
                    client.emit_claude_warnings(
                        responses_metadata,
                        vec![
                            "Anthropic rejected earlier thinking blocks for this conversation; \
continuing without replaying them."
                                .to_string(),
                        ],
                    );
                    strip_thinking = true;
                }
                Err(err) => {
                    let err: CodexErr = client.state.provider.map_api_error(err);
                    inference_trace_attempt.record_failed(
                        &err,
                        /*upstream_request_id*/ None,
                        /*output_items*/ &[],
                    );
                    return Err(err);
                }
            }
        }
    }
}

impl ModelClient {
    /// Returns warnings for configured settings that Claude models cannot honor.
    pub(super) fn unsupported_claude_settings(
        &self,
        summary: ReasoningSummaryConfig,
        service_tier: Option<&str>,
    ) -> Vec<String> {
        let provider = self.state.provider.info().name.clone();
        let mut warnings = Vec::new();
        if let Some(tier) = service_tier {
            warnings.push(format!(
                "service tier `{tier}` is not supported by the {provider} provider and was ignored"
            ));
        }
        if let Some(verbosity) = self.state.model_verbosity {
            warnings.push(format!(
                "model_verbosity `{verbosity}` is not supported by Claude models and was ignored"
            ));
        }
        if matches!(
            summary,
            ReasoningSummaryConfig::Concise | ReasoningSummaryConfig::Detailed
        ) {
            warnings.push(format!(
                "model_reasoning_summary `{summary}` is not configurable for Claude models; \
Claude returns its own thinking summaries"
            ));
        }
        warnings
    }

    /// Emits each distinct warning once per session as a user-visible warning event.
    pub(super) fn emit_claude_warnings(
        &self,
        responses_metadata: &CodexResponsesMetadata,
        warnings: Vec<String>,
    ) {
        if warnings.is_empty() {
            return;
        }
        let fresh: Vec<String> = {
            let mut emitted = self
                .state
                .claude_warnings_emitted
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            warnings
                .into_iter()
                .filter(|warning| emitted.insert(warning.clone()))
                .collect()
        };
        for warning in fresh {
            warn!("{warning}");
            if let Some(sender) = self.event_sender.as_ref() {
                let _ = sender.try_send(ProtocolEvent {
                    id: responses_metadata.turn_id.clone().unwrap_or_default(),
                    msg: EventMsg::Warning(WarningEvent { message: warning }),
                });
            }
        }
    }
}

/// Session-scoped record of warnings already shown for Claude providers.
pub(super) type ClaudeWarningsEmitted = std::sync::Mutex<HashSet<String>>;
