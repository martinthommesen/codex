# Experimental Claude OAuth compatibility record

`claude-oauth` is a native Rust transport inside Codex. It reuses Codex's agent loop, tool
execution, sandbox/approval checks, Anthropic prompt translation, SSE decoder, history, model
catalog, and collaboration runtime. It does not run Claude Code, the Agent SDK, CLIProxyAPI, or a
local inference proxy. The provider is experimental and may violate Anthropic's published
third-party authentication/identity restrictions. See [setup, policy, and billing warnings](claude.md).

No live OAuth login, inference request, or subscription billing validation was performed for this
change. Offline compatibility is not evidence of Anthropic's permission or of subscription-quota
consumption.

## Pinned sources and attribution

- Target: [martinthommesen/codex PR #1](https://github.com/martinthommesen/codex/pull/1), branch
  `feat/anthropic-claude-support`, inspected base
  `9bb20c58c8bfe26e3df3248b3678acf02601e737`.
- Reference: [router-for-me/CLIProxyAPI](https://github.com/router-for-me/CLIProxyAPI/tree/2422c6b0f5fe16495320fa29456493874c1977a0),
  commit **`2422c6b0f5fe16495320fa29456493874c1977a0`**.
- CLIProxyAPI's pinned compatibility profile names Claude Code `2.1.280`, Anthropic JS SDK
  `0.112.1`, and Node `v26.3.0`. These are source-derived emulation constants, not the software
  actually making Codex's HTTP requests, and not a supported public protocol version.
- Adapted algorithms, request transformations, and fixtures retain the upstream MIT notice in
  [LICENSE.CLIProxyAPI](../codex-rs/codex-api/src/anthropic/oauth/LICENSE.CLIProxyAPI), with a
  repository-level [NOTICE](../NOTICE). The notice is reproduced verbatim, including the
  upstream copyright dates.

Primary reference files inspected:

- `internal/auth/claude/anthropic_auth.go`, `pkce.go`, `oauth_server.go`, `oauth_response.go`,
  `identity.go`, `token.go`, and their tests.
- `internal/runtime/executor/claude_executor.go`, `claude_executor_execute.go`,
  `claude_executor_request.go`, `claude_executor_stream.go`, `claude_executor_cloaking.go`,
  `claude_executor_auth.go`, `claude_executor_tool_state.go`, and `claude_signing.go`.
- Supporting `helps/claude_mcp_alias.go`, `claude_mcp_alias_wordlist.go`,
  `claude_bip39_words.txt`, `claude_code_session.go`, `claude_credential_identity.go`,
  `claude_device_profile.go`, `claude_diagnostics.go`, `claude_ratelimit.go`, and relevant request, wire-casing,
  rate-limit, cache-TTL, signature, and stream-truncation tests.

Only Claude-specific behavior is adapted. CLIProxyAPI's multi-provider proxy, account pool,
credential routing, other-provider translators, and operational control plane are not included.

## Standard protocol, undocumented compatibility, and identity emulation

| Class                                   | Examples                                                                                                                                                                            | Treatment                                                                                         |
| --------------------------------------- | ----------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- | ------------------------------------------------------------------------------------------------- |
| Standard OAuth/HTTP mechanisms          | S256 PKCE, random state, bearer authorization, TLS, `Retry-After`, redirect refusal                                                                                                 | Rust implementation using Codex secrets and HTTP conventions                                      |
| Public Anthropic Messages behavior      | Messages/tools/images, streaming events, signed thinking, cache usage, adaptive/budgeted thinking, structured output                                                                | Reused from the existing Anthropic adapter; server/model availability still applies               |
| Undocumented subscription compatibility | Native OAuth client ID/scopes/endpoints, OAuth beta negotiation, JSON identity string, billing text/CCH normalization, nonlegacy `role=system` placement, session continuity fields | Isolated under explicit `WireApi::ClaudeOAuth`; pinned to the source above                        |
| Client identity emulation               | Claude Code version/user-agent, JS/Node/Stainless headers, Claude Code identity system text, native MCP naming shape                                                                | Explicitly identified as emulation; a policy and maintenance risk, not an authorization mechanism |

Anthropic's [legal guidance](https://code.claude.com/docs/en/legal-and-compliance#authentication-and-credential-use)
and [Claude account guidance](https://support.claude.com/en/articles/13189465-log-in-to-your-claude-account)
were checked on **2026-10-19**. They direct third-party developers to API keys or supported cloud
providers and prohibit identity misrepresentation and routing third-party traffic against plan
limits. Reproducing the reference's identity fields does not establish an exception. Anthropic
reserves enforcement and may charge enabled usage credits rather than subscription allowance.

## Feature comparison with the pinned reference

The Rust column records implemented scope, not live acceptance. There is no claim of complete
CLIProxyAPI parity: Codex already provides an agent runtime, whereas the reference is a proxy.

| Feature                         | CLIProxyAPI Claude OAuth path                                                                                                                | Native Codex scope                                                                                                                                                                                                                                         |
| ------------------------------- | -------------------------------------------------------------------------------------------------------------------------------------------- | ---------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| PKCE and state                  | Random verifier, S256 challenge, browser authorization, callback/manual code parsing                                                         | Native interactive login, checked state before exchange, loopback listener and hidden manual `callback URL` / `code#state`; bare code refused                                                                                                              |
| OAuth endpoints                 | `claude.ai/oauth/authorize`; exchange/refresh at `platform.claude.com/v1/oauth/token`; profile and `claude_cli/roles` at `api.anthropic.com` | Pinned endpoints/client ID/scopes; successful profile must identify account; advisory roles lookup; redirects refused                                                                                                                                      |
| Token persistence               | Reference-specific auth files/metadata and optional proxy backends                                                                           | Isolated Codex `ClaudeOAuth` keyring or encrypted `.age` namespace, no plaintext fallback; optional memory-only backend                                                                                                                                    |
| Refresh                         | Shared singleflight, expiry, rotated refresh token, bounded 429 delay; explicit 5xx retry                                                    | Per-home async lock plus cross-process file lock/reload; rotation persisted; interrupted/ambiguous refresh is not replayed blindly; bounded persisted 429 backoff; explicit 5xx retries                                                                    |
| Request authentication          | OAuth bearer distinct from API-key header                                                                                                    | Only the selected OAuth bearer, marked sensitive; never API-key/OpenAI/Claude Code fallback                                                                                                                                                                |
| Direct inference origin         | Messages endpoint and configurable proxy upstreams                                                                                           | Fixed `https://api.anthropic.com/v1/messages?beta=true`; arbitrary origin/credential/header overrides and all redirects rejected                                                                                                                           |
| Native request shape            | Multiple protocol translators, native-client detection, cloaking/normalization                                                               | Existing Codex Anthropic translator followed by isolated OAuth transformations; one native Messages streaming path                                                                                                                                         |
| Beta negotiation                | Ordered per-body/per-model native-client betas; optional forwarded caller betas                                                              | Pinned body/model-dependent beta subset for fields Codex emits; no arbitrary caller-header passthrough                                                                                                                                                     |
| Client/account/session identity | Selected-credential device/account identity, session IDs, native-client identity text/headers, continuity diagnostics                        | Own persisted device identity, profile-derived account UUID, Codex conversation UUID; native identity headers/text; terminal-completion server IDs and stable logical-turn prompt UUID in memory; no other provider's identity                             |
| Billing fingerprint and CCH     | SHA-256 build fingerprint plus byte-preserving seeded XXH64 final-body signing                                                               | Adapted fingerprint and CCH algorithms; final signed bytes sent without JSON re-encoding; errors fail closed                                                                                                                                               |
| Tool names/schemas              | Native MCP aliases, HMAC/BIP-39 names, declarations/history/reference remaps, reverse maps and bounded thread state                          | Same alias derivation using stable Codex device identity as local namespace; canonical union of current declarations and known historical symbols; request-local maps composed with existing Anthropic tool-name map; exact restoration into Codex runtime |
| SSE                             | Complete native events or translated protocols; usage, terminal/error tracking, interrupted-stream checks                                    | Reuses Codex Anthropic SSE-to-`ResponseEvent` decoder for text/thinking/tools/usage; terminal requirement and cancellation remain in that pipeline                                                                                                         |
| Thinking                        | Native model/budget controls, signature sanitation/replay, display/context options                                                           | Existing Codex reasoning and signed-history translation, forced-tool thinking normalization, compatible display/context fields; no new thinking cache/runtime                                                                                              |
| Prompt caching                  | Explicit breakpoints, at most four, 1h main-agent versus default subagent/probe TTL, ordering rules                                          | Existing breakpoints normalized to at most four; 1h main-agent, default subagent TTL; no thinking-block cache marker; TTL ordering normalized                                                                                                              |
| Usage                           | Input/output/cache token reporting and response metadata                                                                                     | Existing token/cache reporting; validated unified utilization/reset headers become Codex rate-limit windows and warnings labelled billing unconfirmed; no inferred plan/credit balance                                                                     |
| Rate/entitlement failures       | Detailed unified/model-level quota classification, reset parsing, proxy credential cooling                                                   | Same-account Codex error/retry path; clear authentication/permission/429 handling, preserved available reset information; no account rotation                                                                                                              |
| Paid fallback/fast mode         | Optional server-side fallback credits and fast-mode handling                                                                                 | Explicitly rejected; no fallback-credit token, server fallback, or `speed: fast` activation                                                                                                                                                                |
| Agent/tool lifecycle            | Proxy translation and client-specific continuation state                                                                                     | Existing Codex approvals, sandbox, shell, `apply_patch`, MCP/dynamic tools, multi-agent state, resume/fork, and compaction                                                                                                                                 |

The API client retains only bounded, allowlisted unified status/reset/utilization fields for
`5h`, `7d`, `7d_oi`, and `overage`, plus representative-claim/disabled-reason codes and `Retry-After`.
Validated percentages and reset timestamps enter Codex's existing `RateLimitSnapshot` event
pipeline, labelled **server reported; billing unconfirmed**. The core displays these windows and
billing/overage warnings; it does not persist/display a complete raw-header record. Unknown,
repeated, invalid, or excessive header values are dropped. Monthly overage reset boundaries are
not treated as ordinary retry deadlines.

Successful terminal stream completion commits the actual server message/request identifiers into
per-session OAuth continuity. The logical turn's prompt UUID is reused across tools and retries.
Sequence guards prevent stale concurrent streams from overwriting newer state; changing the
account/device begins a new generation. Failed/truncated streams and cancellation preserve the
last successful state. The state is not persisted: resume/restart/fork begins with null previous
message diagnostics and replays the existing Codex rollout. This preserves conversation context
without inventing upstream thread persistence.

## Byte-sensitive signing contract

`x-anthropic-billing-header` is text in the first system block, not an HTTP billing receipt. Its
fingerprint samples UTF-16 units at the source-defined positions and hashes the pinned build
input. CCH is computed only after semantic transformations and JSON serialization:

1. Replace the first billing block's five lowercase hexadecimal CCH digits with `00000`.
2. Build the reference hash view without reserializing. Empty raw `model` string values and omit
   raw `max_tokens`, `fallbacks`, and `fallback_credit_token` members recursively. Preserve source
   whitespace, string escapes, key spellings, and the reference's trailing-comma quirk.
3. Hash using XXH64 with seed `0x4D659218E32A3268`, keep the low 20 bits, and replace exactly five
   digits in the original serialized body.
4. Send those bytes. Parsing and re-encoding them afterwards invalidates the contract.

Fixtures derived from the pinned Go implementation check the checksum and normalization,
including nested objects, excluded-member runs, escaped keys, Unicode, and unchanged unrelated
bytes. These vectors show agreement with the pinned source only. If the protocol changes, update
from inspected evidence; do not invent a new checksum or silently send an unsigned fallback.

## Deliberate omissions and differences

- No Firefox/uTLS TLS fingerprint spoofing, HTTP/1 header-casing tricks, sensitive-word
  obfuscation, or automatic date/environment reminder injection. Rust negotiates only compression
  its HTTP stack can decode. Matching the complete native client fingerprint is not claimed.
- No forged account UUID on profile failure. A verified account identity is required rather than
  CLIProxyAPI's setup-token/profile-scope fallback identity.
- No automatic Fable prompt rewriting, model switching, paid fast mode, server-side fallback
  credits, account pools, quota-escape rotation, or fallback to another provider.
- No proxy-wide tool-alias cache or fuzzy restoration when a model changes an alias. Codex uses
  request-local exact reverse mappings, per-session successful-response continuity, and its normal
  history. Historical symbol fields for removed dynamic tools participate in canonical alias
  allocation; arbitrary arguments, schemas, and result text are not recursively renamed. Aliases
  remain stable for the same symbol union, but adding a newly colliding name can shift canonical
  probing. Persisted proxy-thread identity is not claimed. Continuity metadata is not persisted
  on resume/restart/fork.
- No native tool-search/hosted tools, remote compaction endpoint, token-count endpoint, or parallel
  agent runtime. Local context compaction and ordinary Codex tool execution remain available.
- Token counts, cache hits, successful profile/roles lookup, and reset/limit headers do not confirm
  which allowance was charged. There is no independently verified quota-classification receipt.
- Local logout removes the Codex credential; no unverified upstream token-revocation endpoint is
  invented. Manage account authorization separately if Anthropic provides such controls.

## Offline and live validation

Normal tests use fixtures, mocked HTTP transports, and the existing fake Claude Code process;
production OAuth origin restrictions are not relaxed through user configuration to enable tests.
Relevant test locations include:

- `codex-rs/login/src/auth/claude_oauth_tests.rs`: PKCE/state, exchange/profile, refresh and rotation,
  concurrent refresh, revoked/ambiguous outcomes, credential isolation, and redaction.
- `codex-rs/codex-api/src/anthropic/oauth/tests.rs`: request normalization, header/beta selection,
  billing/checksum vectors, caching/thinking, and tool aliases.
- `codex-rs/codex-api/src/anthropic/oauth_client_tests.rs`: direct encoded-byte transport, exact tool
  restoration, streaming, origin restrictions, and transport errors.
- `codex-rs/codex-api/src/anthropic/oauth_response.rs`: bounded unified-rate metadata, retry/reset
  selection, billing uncertainty, and OAuth error redaction/classification.
- `codex-rs/core/src/client_claude_oauth_continuity_tests.rs`: successful tool-loop continuity,
  prompt identity, stale/account-changed writes, terminal completion, truncated/failed streams,
  cancellation, and preserved interruption.
- `codex-rs/core/tests/suite/anthropic_provider.rs`: shared Anthropic/provider tool and delegation
  behavior through the existing Codex runtime.
- `codex-rs/config/src/config_toml.rs` and `model_provider_requirements_tests.rs`, plus
  `codex-rs/app-server/src/config_manager_provider_tests.rs`: partial retry-only built-in overrides
  from local config and managed requirements, with endpoint/header/credential changes rejected.
- `codex-rs/core/src/agent/control_tests.rs`: six mixed-provider lifecycle/resume combinations,
  pairing OAuth with OpenAI, API-key Anthropic, and Claude Code in both directions. They check the
  recorded provider/model, history, authentication isolation, and inherited approval/permission
  configuration. They do not make inference requests.
- Existing Anthropic/provider/CLI suites exercise shared translation, agent permissions, history,
  provider selection and mixed-provider delegation. Those existing tests alone do not establish
  live OAuth compatibility.

The new native OAuth coverage combines auth/HTTP/SSE mocks with those six core lifecycle cases.
The existing API-key Anthropic integration suite exercises the shared actual tool/approval
runtime. This does **not** establish native OAuth inference end to end for every possible
parent/child provider combination, every tool, or every sandbox configuration. Live authentication,
model acceptance, long-running subscription inference, and quota/billing behavior remain
unverified.

### Executed checks

The following checks completed successfully from `codex-rs` on 2026-10-19. Core and final
CLI/config/app-server checks used `RUST_MIN_STACK=8388608`, matching the repository's existing
[CI setting](../.github/workflows/rust-ci.yml) and [test recipe](../justfile). Builds used
`CARGO_BUILD_JOBS=4` and `CARGO_PROFILE_DEV_DEBUG=0`. Filtered commands cover the named tests;
they are not a claim that the entire workspace test suite ran.

| Executed command                                                                                                                                                                                             | Result                                                                                                                                               |
| ------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------ | ---------------------------------------------------------------------------------------------------------------------------------------------------- |
| `cargo fmt --all -- --check`                                                                                                                                                                                 | Passed on final source                                                                                                                               |
| `cargo test -p codex-api -p codex-model-provider --lib --no-fail-fast`                                                                                                                                       | API: 269 passed; model-provider: 119 passed                                                                                                          |
| `cargo test -p codex-login --lib claude_oauth`                                                                                                                                                               | 21 passed                                                                                                                                            |
| `RUST_MIN_STACK=8388608 cargo test -p codex-core --test all suite::anthropic_provider`                                                                                                                       | 14 passed                                                                                                                                            |
| `RUST_MIN_STACK=8388608 cargo test -p codex-core --lib config::schema`                                                                                                                                       | 3 passed                                                                                                                                             |
| `RUST_MIN_STACK=8388608 target/debug/deps/codex_core-c80da957f9b8705a oauth`                                                                                                                                 | 26 passed, including continuity and all six mixed-provider lifecycle cases                                                                           |
| `RUST_MIN_STACK=8388608 cargo test -p codex-cli --test claude_oauth`                                                                                                                                         | 2 passed                                                                                                                                             |
| `RUST_MIN_STACK=8388608 cargo test -p codex-cli -p codex-config -p codex-app-server --lib --bins claude_oauth`                                                                                               | App-server library: 1 passed; CLI library: 4 passed; CLI main binary: 2 passed; config library: 4 passed. Other binary targets had no selected tests |
| `cargo clippy -p codex-api -p codex-login -p codex-secrets -p codex-model-provider-info -p codex-model-provider -p codex-core -p codex-cli -p codex-config -p codex-app-server --all-targets -- -D warnings` | Passed on final source                                                                                                                               |

The core OAuth unit harness above was freshly built by the preceding Cargo schema command, then
executed directly while another Cargo check held the build lock. Its filename is specific to
this build; the portable equivalent is
`RUST_MIN_STACK=8388608 cargo test -p codex-core --lib oauth`. The combined CLI/config/app-server
command is reported once: its three retained result records refer to the same execution.

Retained earlier full library-suite results also show **239 passed** for `codex-login`,
**37 passed** for `codex-model-provider-info`, and **13 passed** for `codex-secrets`. The original
combined invocation was not retained, so no exact flags are reconstructed here. An earlier
provider-library failure in that log is superseded by the final **119 passed** result above.

Initial strict checks exposed three pre-existing issues: formatting in
`codex-rs/claude-code/src/session.rs`, an unused test import in
`codex-rs/core/tests/suite/openai_file_mcp.rs`, and a redundant return binding in
`codex-rs/app-server/src/message_processor.rs`. These small cleanups are isolated from the provider
implementation in a separate commit; they do not change runtime behavior. Final formatting,
strict Clippy, and the completed checks above pass. Debug integration tests use the existing CI
8 MiB stack convention; a first attempt without that setting aborted before its test summary.
No unresolved offline check failure remains in the listed validation scope.

Documentation validation also passed: Prettier for both Claude documents, local-link checks,
whitespace checks, and exact changed-file manifest verification. A search across all 168 tracked
and new Markdown documents found no remaining claims that only two Claude providers exist or
that Claude subscription access always requires a subprocess outside the updated Claude guide.

Live tests require an explicit opt-in and an existing separate Codex OAuth login. Even a
successful live text/tool round trip would not verify subscription-quota treatment; that requires
independent account usage/billing evidence as described in
[the billing checklist](claude.md#subscription-quota-and-billing).

The optional live probe is
`codex-rs/model-provider/tests/claude_oauth_live.rs`. It is ignored by default, checks a second
explicit environment opt-in, reads only an existing Codex OAuth credential, and uses the normal
reasoning/request builder for one text request (default `claude-haiku-5-5`, at most 2048 output
tokens). **Do not run it until you explicitly choose to incur the account-side risks and
have reviewed the Usage settings.** From `codex-rs`:

```shell
CODEX_CLAUDE_OAUTH_LIVE=1 cargo test -p codex-model-provider --test claude_oauth_live -- --ignored
```

The default store is the encrypted `file` backend. Set `CODEX_CLAUDE_OAUTH_LIVE_STORE=keyring` if
you logged in using direct keyring storage, and optionally set `CODEX_CLAUDE_OAUTH_LIVE_MODEL` to
select a model. A successful probe verifies only authentication/inference at the time of the
request. It does not verify tool use, all models, long-session continuity, policy permission, or
subscription-quota charging. This live test was **not executed** for this implementation.

## Changed-file manifest

The authentication boundary lives in `login` and `secrets`; native request compatibility and
stream handling live in `codex-api::anthropic`; `model-provider` selects the isolated auth and
transport, and `core::client_anthropic` continues through the existing Codex event/tool loop.
CLI/config/discovery changes make the explicit provider selectable and durable across resumes.
Local config and managed requirements accept partial retry-only overrides without a custom
provider name; the fixed OAuth origin, bearer source, and header restrictions remain enforced.
Other custom providers retain their required names.
The exact files changed from the inspected PR head are:

```text
NOTICE
codex-rs/Cargo.lock
codex-rs/app-server/src/config_manager.rs
codex-rs/app-server/src/config_manager_provider_tests.rs
codex-rs/app-server/src/message_processor.rs
codex-rs/claude-code/src/session.rs
codex-rs/cli/src/claude_login.rs
codex-rs/cli/src/claude_oauth_args_tests.rs
codex-rs/cli/src/claude_oauth_login.rs
codex-rs/cli/src/claude_oauth_login_tests.rs
codex-rs/cli/src/lib.rs
codex-rs/cli/src/login.rs
codex-rs/cli/src/main.rs
codex-rs/cli/tests/claude_oauth.rs
codex-rs/codex-api/Cargo.toml
codex-rs/codex-api/src/anthropic/client.rs
codex-rs/codex-api/src/anthropic/mod.rs
codex-rs/codex-api/src/anthropic/oauth.rs
codex-rs/codex-api/src/anthropic/oauth/LICENSE.CLIProxyAPI
codex-rs/codex-api/src/anthropic/oauth/bip39_english.txt
codex-rs/codex-api/src/anthropic/oauth/signing.rs
codex-rs/codex-api/src/anthropic/oauth/tests.rs
codex-rs/codex-api/src/anthropic/oauth/tools.rs
codex-rs/codex-api/src/anthropic/oauth_client_tests.rs
codex-rs/codex-api/src/anthropic/oauth_response.rs
codex-rs/codex-api/src/anthropic/stream.rs
codex-rs/codex-api/src/anthropic/stream_tests.rs
codex-rs/codex-api/src/auth.rs
codex-rs/codex-api/src/endpoint/session.rs
codex-rs/codex-api/src/lib.rs
codex-rs/config/src/config_toml.rs
codex-rs/config/src/model_provider_requirements_tests.rs
codex-rs/config/src/thread_config/proto/codex.thread_config.v1.proto
codex-rs/config/src/thread_config/proto/codex.thread_config.v1.rs
codex-rs/config/src/thread_config/remote.rs
codex-rs/core/config.schema.json
codex-rs/core/src/agent/child_config.rs
codex-rs/core/src/agent/control/spawn.rs
codex-rs/core/src/agent/control_tests.rs
codex-rs/core/src/client.rs
codex-rs/core/src/client_anthropic.rs
codex-rs/core/src/client_claude_oauth_continuity_tests.rs
codex-rs/core/src/provider_models.rs
codex-rs/core/src/provider_models_tests.rs
codex-rs/core/tests/suite/openai_file_mcp.rs
codex-rs/login/Cargo.toml
codex-rs/login/src/auth/claude_oauth.rs
codex-rs/login/src/auth/claude_oauth_tests.rs
codex-rs/login/src/auth/manager.rs
codex-rs/login/src/auth/mod.rs
codex-rs/login/src/lib.rs
codex-rs/model-provider-info/src/lib.rs
codex-rs/model-provider-info/src/model_provider_info_tests.rs
codex-rs/model-provider/Cargo.toml
codex-rs/model-provider/src/anthropic/catalog.rs
codex-rs/model-provider/src/anthropic/mod.rs
codex-rs/model-provider/src/anthropic/mod_tests.rs
codex-rs/model-provider/src/anthropic/oauth.rs
codex-rs/model-provider/src/anthropic/oauth_tests.rs
codex-rs/model-provider/src/provider.rs
codex-rs/model-provider/tests/claude_oauth_live.rs
codex-rs/secrets/src/lib.rs
codex-rs/secrets/src/local.rs
docs/claude-oauth-compatibility.md
docs/claude.md
```
