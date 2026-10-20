# Using Claude models in Codex

Codex can run Anthropic Claude models alongside OpenAI models. You can stay signed in to
ChatGPT/OpenAI and Claude at the same time, choose a provider and model for the main agent, and
let any agent spawn sub-agents on a different provider.

## Three ways to connect Claude

Codex ships three built-in Claude providers. Their credentials stay separate from each other and
from your ChatGPT/OpenAI login. Selecting `claude-oauth` is an explicit opt-in to an experimental,
unsupported compatibility transport.

| Provider id    | How it authenticates                               | How requests are sent                                                             | Billing                                                                                                                                                               |
| -------------- | -------------------------------------------------- | --------------------------------------------------------------------------------- | --------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| `anthropic`    | An Anthropic API key (Claude Console)              | Native HTTP to the Messages API                                                   | API usage on your Console organization (or whatever a gateway you configure charges)                                                                                  |
| `claude-code`  | The official Claude Code binary's own sign-in      | Codex runs the unmodified `claude` CLI                                            | Whatever the credential Claude Code uses charges: a Claude subscription, an Anthropic Console API key, or a cloud provider such as Amazon Bedrock or Google Vertex AI |
| `claude-oauth` | A separate interactive OAuth login stored by Codex | Native Rust HTTP directly to Anthropic; no CLI, Agent SDK, or CLIProxyAPI process | Anthropic decides; subscription allowance has **not been verified**                                                                                                   |

### Authentication policy and experimental support

Anthropic's [authentication and credential-use policy](https://code.claude.com/docs/en/legal-and-compliance#authentication-and-credential-use),
checked on **2026-10-19**, says OAuth is designed for ordinary use of Claude Code and other native
Anthropic applications. Developers building products or services should use API keys or supported
cloud providers. The policy says third-party developers may not "route requests through Free, Pro,
or Max plan credentials on behalf of their users" or "collect, store, or intermediate Claude.ai
credentials or session tokens". It separately permits an end user to sign in to the unmodified
Claude Code binary with their own subscription, and reserves enforcement without prior notice.

`claude-oauth` reproduces an undocumented client protocol and stores its own OAuth credentials.
It is potentially outside that published policy. It is **not an official or Anthropic-authorized
integration**. Using your own account, matching CLIProxyAPI, or completing OAuth does not establish
permission or guarantee continued access. Account restrictions, protocol changes, entitlement
failures, and unexpected usage-credit charges are possible. Use `anthropic` for the documented
direct API-key path or `claude-code` for the unmodified-binary integration.

The [Claude account sign-in guidance](https://support.claude.com/en/articles/13189465-log-in-to-your-claude-account),
also checked on 2026-10-19, specifically prohibits third-party tools that misrepresent their
identity to Anthropic's servers or attempt to route third-party traffic against subscription
limits. It says Anthropic may allow some third-party use with enabled usage credits and may draw
that use from credits instead of subscription limits. The identity-emulating compatibility fields
in this experimental provider create a concrete policy risk, even when used with your own account.

Codex never automatically reads or modifies Claude Code's credential files or keychain entries.
The `claude-code` provider delegates authentication to the binary; the experimental `claude-oauth`
provider obtains its own tokens through an explicit browser authorization flow. No Claude provider
silently changes to another provider or credential type.

Max and Team plans include monthly Claude Platform API credits
([details](https://support.claude.com/en/articles/17154008-monthly-api-credits-for-max-and-team-plans)).
Spend them with a Console API key through the `anthropic` provider.

### Anthropic API key (`anthropic`)

```shell
printenv ANTHROPIC_API_KEY | codex login --provider anthropic --with-api-key
codex login status --provider anthropic
codex logout --provider anthropic
```

- Codex stores the key in its own Anthropic credential store, not in `auth.json`. The store follows
  `cli_auth_credentials_store` (`file` by default, or `keyring`, `auto`, `ephemeral`). In file mode
  the key is written to `$CODEX_HOME/anthropic_auth.json` with mode `0600`. In keyring mode it is
  saved under the keyring service `Codex Anthropic Auth`, or in the encrypted secrets file
  `$CODEX_HOME/secrets/anthropic_auth.age`.
- In `auto` mode Codex tries the keyring first. If saving there fails, it writes the key to the
  plaintext `$CODEX_HOME/anthropic_auth.json` (mode `0600`) instead and only logs a warning; the
  login message does not say which store was used. Codex also reads that file whenever the keyring
  has no key or cannot be read. A later successful keyring save removes the plaintext copy. Use
  `keyring` mode if a missing keyring should be an error. Unlike `claude-oauth`, the Anthropic key
  store has this plaintext fallback.
- If Codex has no stored key, it falls back to the `ANTHROPIC_API_KEY` environment variable.
- Codex trims surrounding whitespace, such as a trailing CR/LF, from the key it stores and from
  `ANTHROPIC_API_KEY` or a provider's `env_key` variable. A key that is empty or still contains a
  control character fails the request with an error that names `ANTHROPIC_API_KEY` or `env_key`
  and never shows the key.
- Codex sends the key as an `x-api-key` header to the `base_url` of the provider in use. Every
  provider with `wire_api = "anthropic_messages"` shares this one stored key (or
  `ANTHROPIC_API_KEY`): the built-in `anthropic`, an `anthropic` override with another `base_url`,
  and any custom provider id. The exceptions are a provider that sets `env_key` (only that
  variable is sent; if it is unset the request fails, with no fallback) and a custom provider that
  sets `experimental_bearer_token` or `auth` (sent as a bearer token instead). See the warning in
  [Overriding the providers](#overriding-the-providers).
- Codex never follows redirects for these providers or for `claude-oauth`: a redirect fails the
  request, so the key is never forwarded to another origin.
- The key is marked sensitive, never logged, never written to transcripts or rollouts, and never
  shown to the model. CLI output only ever shows a masked form such as `sk-ant-api***abcd`.
- `codex login --with-api-key` (the OpenAI login) refuses keys that start with `sk-ant-` and points
  you to `--provider anthropic`.

### Claude Code sign-in (`claude-code`)

```shell
# Install Claude Code first: https://code.claude.com/docs/en/setup
codex login --provider claude-code     # runs `claude auth login` if you are not signed in yet
codex login status --provider claude-code
codex logout --provider claude-code    # disconnects Codex only
```

- `codex login --provider claude-code` checks `claude auth status`. If you are not signed in, it
  runs `claude auth login` in your terminal. When that succeeds, Codex records a non-secret marker,
  `$CODEX_HOME/claude-code/connected.json`, that says this provider is connected.
- `codex logout --provider claude-code` removes only that marker, so agents can no longer spawn
  sub-agents on `claude-code`. Selecting `claude-code` as the main provider still runs `claude` with
  its own sign-in. To sign out of Claude Code itself, run `claude auth logout`.
- An `apiKeyHelper` sign-in is not accepted, because Codex starts Claude Code without your settings
  files and the helper would never run.
- Claude Code keeps its own credentials, for example in `~/.claude/.credentials.json` or the macOS
  Keychain. Codex does not read them.
- When Codex starts `claude`, it removes `ANTHROPIC_API_KEY`, `ANTHROPIC_AUTH_TOKEN` and the other
  variables listed in `codex_claude_code::SCRUBBED_ENV_VARS` from the child environment. This
  keeps an inherited API key from silently replacing your subscription sign-in. It also passes
  `--setting-sources ""` so your Claude Code hooks, plugins and permission settings do not apply
  inside Codex.
- Set `CODEX_CLAUDE_CODE_PATH` to use a `claude` executable that is not on `PATH`.
- Codex never passes `--bare`, because bare mode ignores your subscription sign-in. Claude Code's
  docs say bare mode will become the default for `claude -p` in a future release. If that happens
  before Codex adds a way to opt out, this provider stops using your subscription.
- Billing follows the sign-in that `claude auth status` reports. With a Claude subscription,
  Anthropic decides: its Help Center
  ([log in article](https://support.claude.com/en/articles/13189465-log-in-to-your-claude-account))
  says it may bill use from third-party tools to usage credits instead of plan limits, and that
  "advertised usage limits ... assume ordinary, individual usage". Codex does not hide that the
  requests come from Codex. With an Anthropic Console API key that Claude Code manages, that
  Console organization is billed; with a cloud provider such as Amazon Bedrock or Google Vertex AI,
  that cloud account is billed. `codex login --provider claude-code` prints a note when the
  sign-in is not a subscription.
- Because Codex starts Claude Code without your settings files, a cloud provider works only when
  it is selected through environment variables (for example `CLAUDE_CODE_USE_BEDROCK=1`) in the
  shell that starts Codex.
- Claude Code reaches Codex tools through a local MCP bridge: it starts `codex stdio-to-uds`, which
  connects to a socket that Codex creates in a private directory (owner-only, mode `0600`, on
  Unix). The bridge never runs tools itself; Codex runs each call through its own approvals and
  sandbox. The bridge closes a connection that sends a message over 16 MiB, keeps at most 128 calls
  waiting per connection (further calls get an immediate error result), and, once a call carries
  Claude Code's `tool_use` id, refuses calls without one. A current Claude Code release stays
  within these limits.

### Experimental native OAuth (`claude-oauth`)

Read the policy and billing warnings above before choosing this provider:

```shell
codex login --provider claude-oauth
codex login status --provider claude-oauth
codex -c model_provider=claude-oauth -m claude-opus-5-5
codex logout --provider claude-oauth
```

The command prints the experimental warning and an authorization URL to open in your browser.
It listens on loopback port `54545` for `http://localhost:54545/callback`. If the browser cannot
reach that listener (for example, over SSH), paste the full callback URL or `code#state` into the
terminal's hidden prompt. A bare code is rejected because it cannot verify the returned state.
Manual completion remains available if the port cannot be bound. Login times out after ten
minutes and can be cancelled with Ctrl-C. Empty manual input, EOF, or an unavailable terminal does
not cancel a working browser listener. Never put the code or callback URL in shell arguments,
logs, or a shared chat.

Codex performs OAuth authorization with S256 PKCE and a random state value checked before the
code is exchanged. Authorization happens on `https://claude.ai/oauth/authorize`; the code exchange
and refresh use `https://platform.claude.com/v1/oauth/token`. These endpoints, the client identifier,
and scopes come from the pinned CLIProxyAPI reference, rather than a published third-party client
registration. API-key/token input flags, device authentication, and custom OAuth issuer/client-ID
flags cannot be used with this provider. A successful profile lookup identifies the signed-in
account; it is not proof of
model entitlement or subscription-quota treatment. `login status --provider claude-oauth` reports
local credential presence, saved identity, expiration, refresh backoff, and whether a new login is
required; it does not contact Anthropic to
validate or refresh the login. Logout deletes Codex's local OAuth credential only. It does not
revoke the authorization grant at Anthropic; manage account grants separately if available.

Credentials use Codex's isolated `ClaudeOAuth` secrets namespace. `file` and `auto` storage write
`$CODEX_HOME/secrets/claude_oauth.age`, encrypted using a separate OS-keyring-held passphrase;
`keyring` with the direct backend stores the credential under the **Codex Claude OAuth** service;
`keyring` with the secrets backend uses the encrypted `.age` file. There is no plaintext fallback:
a persistent login fails if the required keyring is unavailable. Files/directories use
secure permissions, and tokens are redacted from debug/error output. `ephemeral` stores the
credential in memory only and does not survive another CLI process. The OAuth store is separate
from `auth.json`, `anthropic_auth.json`, and Claude Code's credentials.

Access tokens refresh shortly before expiration. In-process synchronization and a per-home file
lock serialize refresh across agents/processes, then reload the latest rotated credential.
Rotation is persisted before optional profile follow-up. Cancellation during that follow-up leaves
the rotated credential recoverable: the next request repeats only the account check. Local client
or request preparation errors do not invalidate a refresh token. Ephemeral storage uses an
in-process lock and creates no credential files or lock files. Rate-limited refresh honors bounded
`Retry-After`/`Retry-After-Ms` backoff. An interrupted or ambiguous single-use refresh is not blindly
replayed; sign in again if Codex cannot safely recover it. Refresh tokens are single-use, so Codex
marks a refresh as in progress before sending it. If that request fails for any network reason
(even a DNS or connection failure where nothing reached Anthropic), the response cannot be read,
or the process stops mid-refresh, Codex cannot tell whether the token was consumed: it then
requires `codex login --provider claude-oauth` again instead of retrying, so one transient network
failure can force a new login. Only an HTTP 429 or three 5xx responses in a row leave the login
usable. `codex login status --provider claude-oauth` reports when a new login is required.
Revocation or an account-identity change also requires an explicit login. Logout removes local
credential ciphertext, the direct keyring credential entry, and in-memory credentials without
changing another provider's login. Following
the existing secrets-store convention, an unused encryption-passphrase keyring entry can remain;
it contains no access or refresh token.

Inference uses the existing Codex prompt translator and Anthropic SSE decoder. A separate
`claude_oauth` wire protocol applies experimental request compatibility transformations, then posts
directly to `https://api.anthropic.com/v1/messages?beta=true` with a sensitive bearer header.
The provider rejects arbitrary base URLs and credential/header overrides, and rejects redirects.
It does not load `ANTHROPIC_API_KEY`, `ANTHROPIC_AUTH_TOKEN`, Claude Code credentials, or an
OpenAI login as fallback credentials. An OAuth failure never launches `claude` or a proxy.

OAuth compatibility includes client-version/beta headers, session/account metadata, billing text
and CCH signing, cache controls, and reversible tool aliases. Some fields emulate the reference
client's identity. They do not turn Codex into Claude Code or establish Anthropic's permission.
These transformations are confined to `claude-oauth`; API-key requests retain their own path.
See [the reference and feature comparison](claude-oauth-compatibility.md) for the exact scope,
provenance, and omissions.

Within a running Codex conversation, successfully completed responses provide the previous server
message/request IDs for the next OAuth request. One prompt UUID stays stable across tool loops
and retries for that logical turn. Cancellation, failed/truncated streams, stale concurrent
responses, or a changed account/device cannot advance the wrong session state. This continuity
metadata is in memory: resume, restart, and fork begin fresh server diagnostics while replaying
the normal Codex history. It is separate from saved credentials and subscription quota.

### Subscription quota and billing

An OAuth bearer token proves neither that a request consumed plan allowance nor that it was free.
Anthropic controls entitlement and charging server-side. The billing text/CCH and beta headers are
compatibility fields, **not billing receipts**. Token/cache usage counts and reset headers report
only what the server supplies; a local cost estimate cannot identify the quota charged. This
implementation has **not been tested live with a subscription**, and subscription-quota consumption
has **not been independently verified**.

Before enabling the experimental provider:

1. Open [Claude Settings > Usage](https://claude.ai/settings/usage). Review plan usage and the
   **Usage credits** section (also called **Extra usage** in some versions of the interface).
2. Turn usage credits/extra usage off if you do not want separately billed overage. Review the
   monthly spend limit, credit balance, and automatic reload controls. Codex does not change these
   settings. [Anthropic's cost guidance](https://code.claude.com/docs/en/costs#add-usage-credits-to-your-subscription)
   identifies this as the place Pro/Max users can turn usage credits on or off. The
   [usage-credit instructions](https://support.claude.com/en/articles/12429409-manage-usage-credits-for-paid-claude-plans)
   confirm these are charged separately from the subscription and can be disabled here.
3. If you explicitly decide to make a live test, record usage before and after a small isolated
   request, allowing for reporting delays and other sessions. Inspect both plan usage and
   usage-credit spend, plus [Console usage](https://platform.claude.com/usage) for any API account.
   Ask Anthropic support to resolve ambiguous attribution; a successful response alone is not proof.
4. Stop using the provider if billing is unexpected. Run `codex logout --provider claude-oauth`
   and select another provider explicitly.

Codex does not enable extra usage, supply fallback-credit tokens, rotate accounts to escape limits,
or silently retry through an API key. Those safeguards cannot prevent a server from charging
already enabled account-level usage credits. A `401` requires reauthentication if refresh cannot
recover it; `403` can indicate missing scopes, entitlement, or policy restrictions; `429` means
rate or quota limits. Respect the reported wait/reset time. Access can also fail because this
experimental third-party client is not permitted.

### Status, logout, and isolation

```shell
codex login status                    # OpenAI line, plus a line for each connected Claude provider
codex login status --provider all     # one line for every provider
codex logout                          # OpenAI/ChatGPT only (unchanged behavior)
codex logout --provider anthropic     # Anthropic key only
codex logout --provider claude-oauth  # experimental OAuth credential only
codex logout --provider all           # every provider
```

`codex login status` prints to stderr, always in the order OpenAI, Anthropic API, Claude Code,
Claude OAuth (experimental):

| Command                                                                  | Lines printed                                                                                                                                                                                                           | Exit status                                                               |
| ------------------------------------------------------------------------ | ----------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- | ------------------------------------------------------------------------- |
| `codex login status` (or `--provider openai`)                            | The OpenAI status, then the Anthropic API line if a key is usable, the Claude Code line if Codex is connected to Claude Code (even when Claude Code is signed out), and the Claude OAuth line if a credential is stored | 0 only when the OpenAI login is usable; the Claude lines do not affect it |
| `codex login status --provider anthropic`, `claude-code`, `claude-oauth` | That provider's line                                                                                                                                                                                                    | 0 when that provider is usable, otherwise 1                               |
| `codex login status --provider all`                                      | All four lines; the first starts with `OpenAI: `                                                                                                                                                                        | 0 when at least one provider is usable, 1 when none is                    |

A provider is usable when: for `anthropic`, a key is available (the `env_key` variable if one is
configured, otherwise the stored key or `ANTHROPIC_API_KEY`); for `claude-code`, Codex is
connected, Claude Code is signed in, and the sign-in is not an `apiKeyHelper`; for
`claude-oauth`, a credential is stored. Status never contacts Anthropic, so a stored OAuth
credential counts even when it has expired or needs a new login.

`codex logout --provider all` signs out of Anthropic, Claude Code, Claude OAuth and then
OpenAI/ChatGPT. It attempts every provider even when one of them fails, prints each result (the
OpenAI result starts with `OpenAI: `), and exits 1 if any of them failed.

Each provider has its own storage. Logging in, refreshing tokens, or logging out of one provider
never rewrites or deletes another provider's credentials. Persistent storage survives restarts;
`ephemeral` storage does not.

## Choosing Claude for the main agent

Pick the provider and model on the command line:

```shell
codex -c model_provider=anthropic -m claude-sonnet-5-5
codex -c model_provider=claude-code -m claude-opus-5-5
codex -c model_provider=claude-oauth -m claude-opus-5-5  # experimental
```

or set them in `config.toml` (or a profile):

```toml
model_provider = "anthropic"
model = "claude-opus-5-5"
model_reasoning_effort = "high"

[profiles.claude-sub]
model_provider = "claude-code"
model = "claude-sonnet-5-5"

[profiles.claude-native-experimental]
model_provider = "claude-oauth"
model = "claude-opus-5-5"
model_reasoning_effort = "high"
```

If you switch to a Claude provider without picking a model, Codex uses `claude-opus-5-5`. Claude
providers always use the bundled Claude catalog; `model_catalog_json` applies only to OpenAI and
other Responses API providers, including when it is set through managed requirements.

Bundled Claude models: `claude-opus-5-5`, `claude-sonnet-5-5`, `claude-haiku-5-5` and
`claude-fable-5-1`. Older models are hidden from the picker but still accepted: `claude-opus-5`,
`claude-sonnet-5`, `claude-opus-4-8`, `claude-opus-4-7`, `claude-opus-4-6`, `claude-sonnet-4-6`,
`claude-opus-4-5`, `claude-sonnet-4-5` and `claude-haiku-4-5`. Other model ids, such as a newer
`claude-*` model or a gateway's own ids, are sent as given, but Codex has no metadata for them: it
warns and uses generic defaults for the context window and reasoning levels. When such an id names
a Claude model behind a provider or gateway prefix, or writes its version with dots (for example
`us.anthropic.claude-sonnet-4-5-...`, `anthropic/claude-opus-4-6` or `claude-haiku-4.5`), Codex
translates the effort with the thinking rules of the Claude model it names.

### Reasoning effort

`model_reasoning_effort` is translated into Claude's thinking controls:

| Codex effort              | Claude 4.6+ (adaptive thinking)                                                  | Claude 4.5 and older (budgeted thinking) |
| ------------------------- | -------------------------------------------------------------------------------- | ---------------------------------------- |
| `none`                    | thinking disabled; models that cannot turn it off use `low`, and Codex warns     | thinking disabled                        |
| `minimal`                 | `low` (Codex warns)                                                              | budget 2048 (Codex warns)                |
| `low` / `medium` / `high` | `output_config.effort` of the same name                                          | budget 2048 / 8192 / 16384 tokens        |
| `xhigh` / `max`           | same name where the model supports it, otherwise the nearest level (Codex warns) | budget 24576                             |

Whenever Codex changes a requested setting, it shows a warning in the transcript. It never drops
one silently.

With `claude-code`, only the adaptive column applies, passed to Claude Code as `--effort`. Claude
Code cannot be given a thinking budget or told to turn thinking off, so on 4.5-and-older models,
and for `none`, Codex warns and Claude Code uses its default thinking.

### Overriding the providers

The built-in providers can be customized under `[model_providers]`. A partial override of a
built-in id does not need a provider name:

```toml
[model_providers.anthropic]
base_url = "https://api.anthropic.com/v1"   # e.g. a gateway that speaks the Messages API
env_key = "MY_ANTHROPIC_KEY"                # use only this variable (replaces the stored key)
http_headers = { "anthropic-beta" = "..." } # added to the betas Codex sends
request_max_retries = 4
stream_idle_timeout_ms = 300000
```

`anthropic` accepts `base_url`, `env_key`, `env_key_instructions`, `http_headers`,
`request_max_retries`, `stream_max_retries` and `stream_idle_timeout_ms`. `claude-code` accepts
the same fields except `base_url` and `env_key`, because the Claude Code CLI makes every request
itself. Other fields are rejected. `anthropic-beta` values in `http_headers` are merged with the
betas Codex sends rather than replacing them.

**Warning:** an `anthropic_messages` provider without `env_key` receives your stored Anthropic API
key (or `ANTHROPIC_API_KEY`). That includes `[model_providers.anthropic]` with a changed `base_url`
and every custom `anthropic_messages` provider. Point `base_url` only at a gateway you trust with
that key, or give the gateway its own key with `env_key`. Codex refuses redirects for these
providers, and provider definitions in a repository's `.codex/config.toml` are ignored.

A `[model_providers.anthropic]` or `[model_providers.claude-code]` entry whose `name` or
`wire_api` differs from the built-in (`Anthropic`, `Claude Code`) is rejected at startup. If you
defined a custom provider with one of these ids before they became built-in, rename it (for
example to `anthropic-custom`).

Custom providers can use the new wire APIs as well: `wire_api = "anthropic_messages"` or
`wire_api = "claude_code"`. Like any custom provider, they need a `name`. An `anthropic_messages`
provider without `base_url` uses `https://api.anthropic.com/v1`. The `claude-oauth` provider uses
`wire_api = "claude_oauth"`, fixes its Anthropic origin, and rejects configurable base URLs and
credential/header overrides. Only `request_max_retries`, `stream_max_retries`, and
`stream_idle_timeout_ms` can be overridden, including in managed requirements:

```toml
[model_providers.claude-oauth]
request_max_retries = 4
stream_max_retries = 4
stream_idle_timeout_ms = 300000
```

## Delegating across providers

When more than one provider is connected, the `spawn_agent` tool gets an optional
`model_provider` argument. Its description lists the connected providers and a few models for
each, including `claude-oauth` after its separate login. For this list, Codex re-reads the Claude
OAuth store, and the Anthropic key store unless it uses the `file` mode, at most every 30 seconds,
so a login or logout can take that long to show up there; `spawn_agent` checks the connection
again when it runs. A parent agent can:

- start an OpenAI child from a Claude parent, or a Claude child from an OpenAI parent;
- start a child on the same provider with a different model;
- run children on different providers at the same time.

```jsonc
// A ChatGPT-backed parent delegating to Claude
{ "task_name": "review", "message": "Review the diff in src/", "model_provider": "anthropic", "model": "claude-sonnet-5-5" }
// A Claude parent delegating to OpenAI
{ "task_name": "tests", "message": "Write unit tests", "model_provider": "openai", "model": "gpt-5.5" }
```

Rules:

- If you set `model_provider` and leave out `model`, the child gets that provider's default model
  and default effort. A model name the provider does not serve is rejected with a hint to set
  `model_provider`, for example when a `claude-*` model is requested on `openai`. Codex never
  switches providers on its own.
- Spawning on a provider that is not connected fails with the command to connect it.
- If managed requirements pin `model_provider`, `spawn_agent` offers no other provider, and
  spawning or resuming an agent on another provider fails with "model_provider `<id>` is not
  allowed because managed requirements pin model_provider `<pinned id>`".
- A child's provider and model affect only that child. The parent and its siblings keep theirs.
  Settings tied to one provider are reset when a child starts on a different provider, and when a
  resumed agent goes back to its own provider from a parent or sender on another one:
  `model_verbosity`, `model_context_window`, `model_auto_compact_token_limit`, and the
  `agents.default_subagent_model` and `agents.default_subagent_reasoning_effort` defaults. An agent
  role still applies on top of that reset when a child starts and when a multi-agent v2 child is
  reloaded. Resuming a whole agent tree from saved rollouts does not re-apply roles. If the role
  pins a model that the child's provider does not serve, the spawn is
  rejected. On a Claude provider this also holds when the child stays on its parent's provider:
  the Claude catalog is closed, so a role model it does not list (for example a Claude CLI alias
  such as `opus`, or a gateway model id) is rejected with a hint to use another `agent_type` or
  to pass the `model_provider` that serves it.
- Sandbox, approval policy, permission profile, working directory and exec policy are inherited
  exactly as they are for children on the same provider. Every provider runs its tool calls through
  the same Codex tool runtime.
- Handoff, `send_message`, `followup_task`, `wait_agent`, interrupts, completion notices and results
  work the same way whatever the provider.
- When the child is resumed, it comes back on the provider recorded in its thread, unless managed
  requirements now pin a different provider, in which case the resume fails.
- If no other provider is connected, the tool schema is exactly what it was before this change.

### Inter-agent message encryption

When only OpenAI is in use, Codex keeps encrypting inter-agent messages; only OpenAI's backend can
decrypt them. Claude cannot read OpenAI-encrypted messages, so once a Claude provider is current or
connected, the collaboration tools switch to plaintext. Once an agent tree includes a Claude agent,
or agents on different providers, every agent in it keeps plaintext for the rest of the session and
after a resume, even if that provider is disconnected later. Those messages then appear in rollouts
the same way ordinary tool arguments do. A spawn that fails its checks (a provider that is unknown,
not allowed or not connected, an invalid task name, an unknown `agent_type`, a model the provider
does not serve) leaves the tree as it was. Otherwise the tree switches once such a child has
started, with one exception: with MultiAgentV2, a `spawn_agent` call that passes those checks but
carries an encrypted task for another provider switches the tree to plaintext right away and asks
the model to send the task again, since only a plaintext schema lets it do so. The tree then stays
plaintext even if that second call fails or never comes.

## What Claude providers support

| Capability                                                               | `anthropic` | `claude-code` | `claude-oauth` | Notes                                                                                                                                                                                                                                                                                                                   |
| ------------------------------------------------------------------------ | :---------: | :-----------: | :------------: | ----------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| Streaming text and thinking summaries                                    |     ✅      |      ✅       |       ✅       | Thinking is shown as a reasoning summary                                                                                                                                                                                                                                                                                |
| Shell / unified exec, `apply_patch`, MCP tools, dynamic tools            |     ✅      |      ✅       |       ✅       | Every tool goes through Codex sandboxing and approvals; `claude-code` reaches Codex tools over a local MCP bridge                                                                                                                                                                                                       |
| Parallel tool calls                                                      |     ✅      |      ✅       |       ✅       | Claude Code controls its own batching                                                                                                                                                                                                                                                                                   |
| Images in input                                                          |     ✅      |      ✅       |       ✅       | Shared prompt translation. `anthropic` and `claude-oauth` send the 20 most recent images per request and replace older ones with a short note, and Codex warns (the API limits requests with more than 20 images to 2000 px per side). An inline image that is not base64-encoded is replaced by a note, with a warning |
| Cancellation                                                             |     ✅      |      ✅       |       ✅       | HTTP streams are dropped; `claude-code` interrupts the CLI                                                                                                                                                                                                                                                              |
| Resume and fork                                                          |     ✅      |      ✅       |       ✅       | Codex history and signed thinking replay; experimental wire compatibility still depends on the server                                                                                                                                                                                                                   |
| Reasoning effort                                                         |     ✅      |      ✅       |       ✅       | See the effort table above                                                                                                                                                                                                                                                                                              |
| Token/cache usage                                                        |     ✅      |      ✅       |       ✅       | Counts do not establish billing category                                                                                                                                                                                                                                                                                |
| Local auto-compaction                                                    |     ✅      |      ✅       |       ✅       | Remote/server-side compaction is not wired up                                                                                                                                                                                                                                                                           |
| Structured output (`--output-schema`)                                    |     ✅      |      ❌       |       ✅       | `claude-code` warns and ignores the schema                                                                                                                                                                                                                                                                              |
| Memory generation (`features.memories`)                                  |     ✅      |      ❌       |       ❌       | On `anthropic`, background model work is billed as API usage; existing memories remain readable on all providers                                                                                                                                                                                                        |
| Hosted web search, image generation, tool search                         |     ❌      |      ❌       |       ❌       | Not exposed by this integration                                                                                                                                                                                                                                                                                         |
| `model_verbosity`, service tiers, reasoning summary `concise`/`detailed` |     ❌      |      ❌       |       ❌       | Codex warns and ignores them                                                                                                                                                                                                                                                                                            |

When Anthropic supplies valid unified utilization/reset headers, `claude-oauth` reports the server's
5-hour, 7-day, `7d_oi`, and overage windows through Codex's existing rate-limit event display,
labelled **server reported; billing unconfirmed**. It also emits a billing-uncertainty warning and
warns on available overage signals. It does not infer a plan type, credits balance, or charged
pool. Only bounded, allowlisted rate metadata is retained by the API client; the main UI/history
shows windows and warnings rather than a complete raw-header dump. `anthropic` and `claude-code`
do not gain this experimental subscription-header interpretation.

The OAuth column describes integration with Codex's existing agent pipeline and offline test
coverage. It is not a statement that Anthropic currently accepts the transport, grants access to
all catalog models, or charges subscription allowance. Rate-limit headers and usage counters must
not be presented as a confirmed remaining subscription balance.

## Known limitations

- **Resume picker:** `codex resume` lists sessions from the configured provider. Run
  `codex -c model_provider=anthropic resume` (or use a Claude profile) to list Claude sessions.
  `codex resume <SESSION_ID>` works for any session and reopens it with the provider it was
  recorded on.
- **First run:** the TUI's onboarding screen is for ChatGPT sign-in only. With `anthropic` and no
  key, the first turn fails with the `codex login --provider anthropic --with-api-key` command.
  With `claude-code`, the main agent uses whatever sign-in the `claude` CLI has. If it is signed
  out, the turn fails with Claude Code's message plus a hint to run `claude auth login` and
  `codex login --provider claude-code`. The connection marker controls only whether sub-agents may
  be spawned on `claude-code`. `claude auth logout` leaves the marker in place, so a `claude-code`
  sub-agent spawned afterwards fails on its first turn with the same hint. With `claude-oauth`, run
  `codex login --provider claude-oauth` explicitly before starting a session.
- **Realtime conversations:** realtime voice sessions use OpenAI's realtime API and are refused
  while the thread runs on a Claude provider.
- **Memories on `claude-code` and `claude-oauth`:** memory extraction and consolidation do not run
  while the thread uses `claude-code` or `claude-oauth`, so background jobs never spend your Claude
  plan. Use `anthropic` or OpenAI to generate memories. On `anthropic`, memory jobs do not check
  ChatGPT rate limits (so `memories.min_rate_limit_remaining_percent` does not apply to them) and
  never contact the ChatGPT backend.
- **Switching providers mid-thread:** a thread stays on one provider. When a history moves between
  providers, as with forks and resumes, Codex drops reasoning that the other provider cannot verify
  (OpenAI encrypted reasoning, Claude thinking signatures) and keeps everything else.
- **Thinking replay:** Codex replays earlier Claude thinking only with its signature. A thinking
  block recorded without one (for example through a gateway that drops signatures) is not
  replayed, and Codex warns once. If Anthropic rejects replayed thinking on `anthropic` or
  `claude-oauth`, Codex retries without it and warns; for the rest of that session it stops
  replaying Claude thinking blocks and stops sending `block_binding` and its beta. A compaction does
  not reset this; a new session or a resume does.
- **Tool history:** if history holds more than one output for the same tool call, only the first
  is sent as that call's result; the others are sent as text.
- **Tool names:** Claude tool names may contain only letters, digits, `_` and `-`, so Codex
  derives them from Codex tool names and namespaces. If two different tools end up with the same
  Claude name, Claude is offered only the first, and Codex warns with the names of both tools.
- **Refusals and truncated responses:** when Claude refuses, the turn fails with "Claude declined
  to respond (refusal category: ...)", no tool call from that response runs, and its partial answer
  is not recorded. If a response stops at `max_tokens` before any complete tool call, the turn
  fails with an incomplete-response error instead of ending with a truncated answer. Tool calls
  whose input was cut off by `max_tokens` or the context window are dropped; after a `max_tokens`
  stop, complete tool calls still run and the model continues after them.
- **Claude Code sessions:** Claude Code keeps its own copy of the conversation, so each turn sends
  only what Codex added since the last one. When Codex history no longer matches that copy (after a
  compaction, rollback or fork) or Claude Code failed before it started its session, Codex starts a
  fresh Claude Code session: it sends the earlier conversation as a text transcript (images in it
  appear as `[image]`) and your latest message, with its images, as the live prompt.
- **Environment inheritance:** shell commands run by any agent inherit your environment as before.
  An exported `ANTHROPIC_API_KEY` is visible to them, so prefer `codex login --provider anthropic`.
  With the `file` credential store, or after an `auto` fallback to it, any shell command that can
  read `$CODEX_HOME` can read the key file. Use `keyring` mode or the sandbox's read restrictions
  if that matters.
- **Claude Code CLI version:** the `claude-code` provider relies on the documented `claude -p`
  stream-json interface, the `--mcp-config`, `--tools`, `--allowedTools`, `--setting-sources` and
  `--resume` flags, and `claude auth status`. Use a current Claude Code release.
- **Experimental OAuth maintenance:** OAuth endpoints, scopes, native-client beta combinations,
  billing/CCH serialization, tool aliases, and identity fields are not a stable third-party API.
  Server changes may break login or inference without a Codex release. Do not substitute a guessed
  checksum or remove billing warnings to make a failing request appear successful. Re-check the
  [pinned reference comparison](claude-oauth-compatibility.md) when updating this path.

## Validation status

Automated tests use local mock servers and a fake `claude` executable. They cover request and
stream translation, tool round trips, credential isolation, and parent/child provider
combinations. The opt-in live test (`codex-rs/claude-code/tests/live.rs`) was run once against a
signed-in Claude Code CLI and completed a bridged tool round trip. No live Anthropic API call was
made for that validation. `claude-oauth` authentication, requests, signing, streaming, and provider
integration have local fixture/mock coverage. The six new mixed-provider lifecycle/resume cases
check provider/history/permission preservation without making inference requests; the existing
API-key Anthropic suite exercises the shared actual tool and approval runtime. Native OAuth
inference end to end across every provider pairing has not been tested. No live OAuth login or
inference request was made for this change, and subscription billing remains unverified. The
optional live test in
`codex-rs/model-provider/tests/claude_oauth_live.rs` is ignored by default and requires an explicit
opt-in; ordinary tests and CI do not need real credentials. See
[the compatibility report](claude-oauth-compatibility.md) for coverage and remaining gaps.
