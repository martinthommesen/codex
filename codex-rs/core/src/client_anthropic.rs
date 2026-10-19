//! Anthropic Messages API transport for [`ModelClientSession`].
//!
//! Requests are translated from the provider-neutral [`Prompt`] by `codex_api::anthropic`, and the
//! streamed response is mapped back into ordinary [`ResponseEvent`]s, so the turn loop, tool
//! router, sandboxing, approvals, and history handling are identical to OpenAI turns.

use std::collections::HashSet;
use std::sync::Arc;
use std::sync::Mutex;

use codex_api::anthropic::AnthropicMessagesClient;
use codex_api::anthropic::AnthropicRequestOptions;
use codex_api::anthropic::build_anthropic_request;
use codex_api::anthropic::is_thinking_replay_rejection;
use codex_api::anthropic::oauth::OAuthRequestContext;
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
use futures::StreamExt;
use http::HeaderMap;
use tokio::sync::mpsc;
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
        let oauth_attempt = if client.state.provider.info().is_claude_oauth() {
            let identity = setup.api_auth.claude_oauth_identity().ok_or_else(|| {
                CodexErr::UnsupportedOperation(
                    "Claude OAuth credentials are unavailable; run `codex login --provider claude-oauth`".into(),
                )
            })?;
            let context = OAuthRequestContext {
                account_uuid: identity.account_uuid,
                device_id: identity.device_id,
                session_id: client.state.thread_id.to_string(),
                is_subagent: client.state.session_source.is_non_root_agent(),
                ..Default::default()
            };
            Some(
                client
                    .state
                    .claude_oauth_continuity
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .begin(context, responses_metadata.turn_id.as_deref()),
            )
        } else {
            None
        };
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
        if oauth_attempt.is_some() {
            warnings.push("Experimental Claude OAuth uses undocumented Claude Code compatibility behavior. Anthropic may restrict third-party subscription authentication. Subscription-quota billing is unverified; check Claude Settings > Usage and disable extra usage before relying on subscription limits. Codex never falls back to an API key or another provider.".into());
        }

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
            let result = if let Some((context, sequence)) = &oauth_attempt {
                api_client
                    .stream_oauth(&prepared, headers, context)
                    .await
                    .map(|(stream, metadata)| {
                        if let Some(warning) = metadata.warning_summary() {
                            client.emit_claude_warnings(responses_metadata, vec![warning]);
                        }
                        track_oauth_continuity(
                            stream,
                            Arc::clone(&client.state.claude_oauth_continuity),
                            *sequence,
                        )
                    })
            } else {
                api_client.stream(&prepared, headers).await
            };
            match result {
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

/// In-memory continuity is deliberately scoped to one ModelClient session. It is shared by tool
/// turns, but never reconstructed from another provider's history or persisted as an auth secret.
pub(super) type ClaudeOAuthContinuityState = Arc<Mutex<ClaudeOAuthContinuity>>;

#[derive(Default)]
pub(super) struct ClaudeOAuthContinuity {
    account_uuid: String,
    device_id: String,
    next_sequence: u64,
    minimum_sequence: u64,
    committed_sequence: u64,
    turn_id: Option<String>,
    prompt_id: Option<String>,
    previous_message_id: Option<String>,
    previous_request_id: Option<String>,
}

impl std::fmt::Debug for ClaudeOAuthContinuity {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ClaudeOAuthContinuity")
            .field("completed_response", &self.previous_message_id.is_some())
            .field("committed_sequence", &self.committed_sequence)
            .finish_non_exhaustive()
    }
}

impl ClaudeOAuthContinuity {
    /// Sequence guards follow CLIProxyAPI helps/claude_diagnostics.go at the pinned MIT source.
    /// A changed account/device starts a fresh generation; old concurrent streams cannot commit.
    fn begin(
        &mut self,
        mut context: OAuthRequestContext,
        turn_id: Option<&str>,
    ) -> (OAuthRequestContext, u64) {
        self.next_sequence = self.next_sequence.saturating_add(1);
        let sequence = self.next_sequence;
        if self.account_uuid != context.account_uuid || self.device_id != context.device_id {
            self.account_uuid.clone_from(&context.account_uuid);
            self.device_id.clone_from(&context.device_id);
            self.minimum_sequence = sequence;
            self.committed_sequence = 0;
            self.previous_message_id = None;
            self.previous_request_id = None;
            self.turn_id = None;
            self.prompt_id = None;
        }
        if self.prompt_id.is_none() || (turn_id.is_some() && self.turn_id.as_deref() != turn_id) {
            self.prompt_id = Some(
                turn_id
                    .and_then(|id| uuid::Uuid::parse_str(id).ok())
                    .unwrap_or_else(uuid::Uuid::new_v4)
                    .to_string(),
            );
            self.turn_id = turn_id.map(str::to_owned);
        }
        context
            .previous_message_id
            .clone_from(&self.previous_message_id);
        context
            .previous_request_id
            .clone_from(&self.previous_request_id);
        context.prompt_id.clone_from(&self.prompt_id);
        (context, sequence)
    }

    fn commit(&mut self, sequence: u64, message_id: &str, request_id: Option<&str>) {
        let message_id = message_id.trim();
        if sequence < self.minimum_sequence
            || sequence < self.committed_sequence
            || message_id.is_empty()
            || message_id.len() > 256
            || !message_id
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_' || byte == b'-')
        {
            return;
        }
        self.previous_message_id = Some(message_id.to_owned());
        self.previous_request_id = request_id
            .filter(|id| {
                id.strip_prefix("req_").is_some_and(|suffix| {
                    !suffix.is_empty()
                        && suffix.len() <= 36
                        && suffix.bytes().all(|byte| {
                            byte.is_ascii_alphanumeric() || byte == b'_' || byte == b'-'
                        })
                })
            })
            .map(str::to_owned);
        self.committed_sequence = sequence;
    }
}

/// Observe completion while forwarding the same Codex events. Dropping the consumer drops the
/// original stream immediately, so interrupted/truncated/cancelled responses never advance state.
fn track_oauth_continuity(
    mut stream: codex_api::ResponseStream,
    state: ClaudeOAuthContinuityState,
    sequence: u64,
) -> codex_api::ResponseStream {
    let upstream_request_id = stream.upstream_request_id.clone();
    let observed_request_id = upstream_request_id.clone();
    let interrupt = stream.interrupt.take();
    let (tx_event, rx_event) = mpsc::channel(32);
    tokio::spawn(async move {
        loop {
            let event = tokio::select! {
                _ = tx_event.closed() => return,
                event = stream.next() => event,
            };
            let Some(event) = event else {
                return;
            };
            let completed = match &event {
                Ok(codex_api::ResponseEvent::Completed { response_id, .. }) => {
                    Some(response_id.as_str())
                }
                _ => None,
            };
            let terminal = completed.is_some() || event.is_err();
            let Ok(permit) = tx_event.reserve().await else {
                return;
            };
            if let Some(message_id) = completed {
                state
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .commit(sequence, message_id, observed_request_id.as_deref());
            }
            permit.send(event);
            if terminal {
                return;
            }
        }
    });
    codex_api::ResponseStream {
        rx_event,
        upstream_request_id,
        interrupt,
    }
}

#[cfg(test)]
#[path = "client_claude_oauth_continuity_tests.rs"]
mod oauth_continuity_tests;
