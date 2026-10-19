# Using Claude models in Codex

Codex can run Anthropic Claude models alongside OpenAI models. You can stay signed in to
ChatGPT/OpenAI and Claude at the same time, choose a provider and model for the main agent, and
let any agent spawn sub-agents on a different provider.

## Two ways to connect Claude

Codex ships two built-in Claude providers. They authenticate differently and are kept separate
from each other and from your ChatGPT/OpenAI login.

| Provider id   | How it authenticates                                   | How requests are sent                                                                   | Billing                          |
| ------------- | ------------------------------------------------------ | --------------------------------------------------------------------------------------- | -------------------------------- |
| `anthropic`   | An Anthropic API key (Claude Console)                  | Codex calls the Claude Messages API (`POST /v1/messages`) directly                      | API usage on your Console org    |
| `claude-code` | Your own Claude Code sign-in, e.g. a Claude subscription | Codex runs the official, unmodified `claude` CLI and lets it make the model requests | Whatever your Claude Code login uses |

Why two providers: Anthropic's
[legal and compliance page](https://code.claude.com/docs/en/legal-and-compliance#authentication-and-credential-use)
tells developers who build on Claude to use **API keys**. It also says third-party developers may
not "route requests through Free, Pro, or Max plan credentials on behalf of their users" or
"collect, store, or intermediate Claude.ai credentials or session tokens". The same page allows
"an end user ... signing in to the unmodified Claude Code binary with their own Claude
subscription". So Codex never reads, copies or forwards Claude Code credentials, and it never calls
Anthropic endpoints with them. With `claude-code`, the official Claude Code CLI uses your sign-in
to talk to Anthropic, and Codex only exchanges prompts and tool results with that process. If you
want to call the API directly, use `anthropic` with an API key. Codex never quietly turns one kind
of login into the other.

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
  `cli_auth_credentials_store` (`file`, `keyring`, `auto`, `ephemeral`). In file mode the key is
  written to `$CODEX_HOME/anthropic_auth.json` with mode `0600`. In keyring mode it is saved under
  the keyring service `Codex Anthropic Auth`, or in the encrypted secrets file
  `$CODEX_HOME/secrets/anthropic_auth.age`.
- If Codex has no stored key, it falls back to the `ANTHROPIC_API_KEY` environment variable.
- Codex sends the key as an `x-api-key` header only to the configured Anthropic base URL. It is
  marked sensitive, never logged, never written to transcripts or rollouts, and never shown to the
  model. CLI output only ever shows a masked form such as `sk-ant-api***abcd`.
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
- Billing is Anthropic's call. Its Help Center
  ([log in article](https://support.claude.com/en/articles/13189465-log-in-to-your-claude-account))
  says it may bill use from third-party tools to usage credits instead of plan limits, and that
  "advertised usage limits ... assume ordinary, individual usage". Codex does not hide that the
  requests come from Codex.

### Status, logout, and isolation

```shell
codex login status                    # OpenAI line, plus a line for each connected Claude provider
codex login status --provider all
codex logout                          # OpenAI/ChatGPT only (unchanged behavior)
codex logout --provider anthropic     # Anthropic key only
codex logout --provider all           # every provider
```

Each provider has its own storage. Logging in, refreshing tokens, or logging out of one provider
never rewrites or deletes another provider's credentials. All logins persist across restarts. The
exit code of `codex login status` without `--provider` still reflects only the OpenAI login.

## Choosing Claude for the main agent

Pick the provider and model on the command line:

```shell
codex -c model_provider=anthropic -m claude-sonnet-5-5
codex -c model_provider=claude-code -m claude-opus-5-5
```

or set them in `config.toml` (or a profile):

```toml
model_provider = "anthropic"
model = "claude-opus-5-5"
model_reasoning_effort = "high"

[profiles.claude-sub]
model_provider = "claude-code"
model = "claude-sonnet-5-5"
```

If you switch to a Claude provider without picking a model, Codex uses `claude-opus-5-5`.

Bundled Claude models: `claude-opus-5-5`, `claude-sonnet-5-5`, `claude-haiku-5-5` and
`claude-fable-5-1`. Older models are hidden from the picker but still accepted: `claude-opus-5`,
`claude-sonnet-5`, `claude-opus-4-8`, `claude-opus-4-7`, `claude-opus-4-6`, `claude-sonnet-4-6`,
`claude-opus-4-5`, `claude-sonnet-4-5` and `claude-haiku-4-5`. Other `claude-*` names are sent as
given, but Codex has no metadata for them: it warns and uses generic defaults for the context
window and reasoning levels.

### Reasoning effort

`model_reasoning_effort` is translated into Claude's thinking controls:

| Codex effort                      | Claude 4.6+ (adaptive thinking)                                                     | Claude 4.5 and older (budgeted thinking)       |
| --------------------------------- | ----------------------------------------------------------------------------------- | ---------------------------------------------- |
| `none`                            | thinking disabled; models that cannot turn it off use `low`, and Codex warns        | thinking disabled                              |
| `minimal`                         | `low` (Codex warns)                                                                 | budget 2048 (Codex warns)                      |
| `low` / `medium` / `high`         | `output_config.effort` of the same name                                             | budget 2048 / 8192 / 16384 tokens              |
| `xhigh` / `max`                   | same name where the model supports it, otherwise the nearest level (Codex warns)    | budget 24576                                   |

Whenever Codex changes a requested setting, it shows a warning in the transcript. It never drops
one silently.

With `claude-code`, only the adaptive column applies, passed to Claude Code as `--effort`. Claude
Code cannot be given a thinking budget or told to turn thinking off, so on 4.5-and-older models,
and for `none`, Codex warns and Claude Code uses its default thinking.

### Overriding the providers

The built-in providers can be customized under `[model_providers]`:

```toml
[model_providers.anthropic]
base_url = "https://api.anthropic.com/v1"   # e.g. a gateway that speaks the Messages API
env_key = "MY_ANTHROPIC_KEY"                # use only this variable (replaces the stored key)
http_headers = { "anthropic-beta" = "..." }
request_max_retries = 4
stream_idle_timeout_ms = 300000
```

Custom providers can use the new wire APIs as well: `wire_api = "anthropic_messages"` or
`wire_api = "claude_code"`. An `anthropic_messages` provider without `base_url` uses
`https://api.anthropic.com/v1`. The `claude-code` provider does not accept `base_url` or `env_key`,
because the Claude Code CLI makes every request itself.

## Delegating across providers

When more than one provider is connected, the `spawn_agent` tool gets an optional
`model_provider` argument. Its description lists the connected providers and a few models for
each. A parent agent can:

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
- A child's provider and model affect only that child. The parent and its siblings keep theirs.
  Settings tied to one provider are reset for a child on a different provider: `model_verbosity`,
  `model_context_window`, `model_auto_compact_token_limit`, and the `agents.default_subagent_model`
  and `agents.default_subagent_reasoning_effort` defaults. An agent role still applies. If the role
  pins a model that the child's provider does not serve, the spawn is rejected.
- Sandbox, approval policy, permission profile, working directory and exec policy are inherited
  exactly as they are for children on the same provider. Every provider runs its tool calls through
  the same Codex tool runtime.
- Handoff, `send_message`, `followup_task`, `wait_agent`, interrupts, completion notices and results
  work the same way whatever the provider.
- When the child is resumed, it comes back on the provider recorded in its thread.
- If no other provider is connected, the tool schema is exactly what it was before this change.

### Inter-agent message encryption

When only OpenAI is in use, Codex keeps encrypting inter-agent messages; only OpenAI's backend can
decrypt them. Claude cannot read OpenAI-encrypted messages, so once a Claude provider is current or
connected, the collaboration tools switch to plaintext. Those messages then appear in rollouts the
same way ordinary tool arguments do.

## What Claude providers support

| Capability                                   | `anthropic` | `claude-code` | Notes |
| -------------------------------------------- | :---------: | :-----------: | ----- |
| Streaming text and thinking summaries         | ✅ | ✅ | Thinking is shown as a reasoning summary |
| Shell / unified exec, `apply_patch`, MCP tools, dynamic tools | ✅ | ✅ | Codex runs every tool under its sandbox and approval policy. `claude-code` has its built-in tools turned off and reaches Codex tools over a local MCP bridge |
| Parallel tool calls                           | ✅ | ✅ | Always allowed for `anthropic`; Claude Code decides for `claude-code` |
| Images in input                               | ✅ | ✅ | |
| Cancellation (Esc / interrupt)                | ✅ | ✅ | `claude-code` interrupts the CLI and keeps its session |
| Resume and fork                               | ✅ | ✅ | History is replayed from the Codex rollout. Thinking blocks are replayed with their signatures |
| Reasoning effort                              | ✅ | ✅ | See the table above |
| Token usage                                   | ✅ | ✅ | Cache reads count as cached input |
| Local auto-compaction                         | ✅ | ✅ | Remote (server-side) compaction is not available |
| Structured output (`--output-schema`)         | ✅ | ❌ | `claude-code` warns and ignores the schema |
| Memory generation (`features.memories`)       | ✅ | ❌ | Extraction and consolidation are skipped on `claude-code`; existing memories are still read |
| Hosted web search, image generation, tool search | ❌ | ❌ | These tools are left out for Claude models |
| `model_verbosity`, service tiers, reasoning summary `concise`/`detailed` | ❌ | ❌ | Codex warns and ignores them |
| Rate-limit and credits display                | ❌ | ❌ | Anthropic rate-limit headers are not shown yet |

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
  sub-agent spawned afterwards fails on its first turn with the same hint.
- **Realtime conversations:** realtime voice sessions use OpenAI's realtime API and are refused
  while the thread runs on a Claude provider.
- **Memories on `claude-code`:** memory extraction and consolidation do not run while the thread
  uses `claude-code`, so background jobs never spend your Claude plan. Use `anthropic` or OpenAI to
  generate memories.
- **Switching providers mid-thread:** a thread stays on one provider. When a history moves between
  providers, as with forks and resumes, Codex drops reasoning that the other provider cannot verify
  (OpenAI encrypted reasoning, Claude thinking signatures) and keeps everything else.
- **Environment inheritance:** shell commands run by any agent inherit your environment as before.
  An exported `ANTHROPIC_API_KEY` is visible to them, so prefer `codex login --provider anthropic`.
  With the `file` credential store, any shell command that can read `$CODEX_HOME` can read the key
  file. Use `keyring` mode or the sandbox's read restrictions if that matters.
- **Claude Code CLI version:** the `claude-code` provider relies on the documented `claude -p`
  stream-json interface, the `--mcp-config`, `--tools`, `--allowedTools`, `--setting-sources` and
  `--resume` flags, and `claude auth status`. Use a current Claude Code release.

## Validation status

Automated tests use local mock servers and a fake `claude` executable. They cover request and
stream translation, tool round trips, credential isolation, and parent/child provider
combinations. The opt-in live test (`codex-rs/claude-code/tests/live.rs`) was run once against a
signed-in Claude Code CLI and completed a bridged tool round trip. No live Anthropic API call was
made, so validate your own setup with a short session before relying on it.
