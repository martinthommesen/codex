# Configuration

For basic configuration instructions, see [this documentation](https://developers.openai.com/codex/config-basic).

For advanced configuration instructions, see [this documentation](https://developers.openai.com/codex/config-advanced).

For a full configuration reference, see [this documentation](https://developers.openai.com/codex/config-reference).

## Lifecycle hooks

Admins can set top-level `allow_managed_hooks_only = true` in
`requirements.toml` to ignore user, project, and session hook configs while
still allowing managed hooks from requirements and managed config layers. This
setting is only supported in `requirements.toml`; putting it in `config.toml`
does not enable managed-hooks-only mode.

## Turning whole features off

Run `/settings` in the TUI to switch entire features on or off. Each switch
writes explicit values to your user `config.toml`. Running sessions keep the
features they started with, so changes apply to new sessions (`/new`).

| Switch           | Keys written                                       |
| ---------------- | -------------------------------------------------- |
| Plugins & apps   | `features.plugins`, `features.apps`                |
| Skills           | `features.skills`                                  |
| Hooks            | `features.hooks`                                   |
| AGENTS.md        | `features.agents_md`                               |
| Memories         | `features.memories`                                |
| Subagents        | `features.multi_agent`, `agents.enabled`           |
| Web search       | `web_search = "disabled"` (cleared when turned on) |
| Image generation | `features.image_generation`                        |
| Goals            | `features.goals`                                   |

`features.skills = false` stops skill discovery from every source (repository,
user, system, plugin, and cloud skills), leaves skills out of model context,
and ignores `$skill` mentions. `features.agents_md = false` stops loading both
`~/.codex/AGENTS.md` and project `AGENTS.md` files; instructions supplied by a
host application are still used.

Saving a switch removes legacy aliases for the same feature (such as
`features.connectors`), and a canonical key now takes precedence over its
alias when both are set. Values pinned in `requirements.toml` show as managed
and cannot be changed from `/settings`. A profile or a trusted project's
`.codex/config.toml` can still override the user value.
