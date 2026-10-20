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
writes explicit values to your user `config.toml`, or to the selected profile's
config file when you run with `--profile`. Running sessions keep the features
they started with, so changes apply to new sessions (`/new`).

| Switch           | Keys written                                    |
| ---------------- | ----------------------------------------------- |
| Plugins & apps   | `features.plugins`, `features.apps`             |
| Skills           | `features.skills`                               |
| Hooks            | `features.hooks`                                |
| AGENTS.md        | `features.agents_md`                            |
| Memories         | `features.memories`                             |
| Subagents        | `features.multi_agent`, `agents.enabled`        |
| Web search       | `web_search` (`"disabled"`, or an allowed mode) |
| Image generation | `features.image_generation`                     |
| Goals            | `features.goals`                                |

`features.skills = false` stops skill discovery from every source (repository,
user, system, plugin, and cloud skills), leaves skills out of model context,
and ignores `$skill` mentions. `features.agents_md = false` stops loading both
`~/.codex/AGENTS.md` and project `AGENTS.md` files; instructions supplied by a
host application are still used.

Turning Subagents off also writes `features.multi_agent_v2 = false`, which
otherwise overrides `agents.enabled`. Saving a switch also removes legacy
aliases for the same feature (such as `features.connectors`) from your user
`config.toml`. Values pinned in `requirements.toml` show as managed and are left
unchanged. Another config layer, such as a command-line flag, a trusted
project's `.codex/config.toml`, a legacy alias in your main `config.toml` while
a profile is selected, or a managed config, can still override the saved value;
`/settings` then reports the save as overridden and names that layer and the
key it sets.

A running session keeps the features it started with: saving a switch does not
add or remove that session's tools, model context, or goal support, or change
whether its hooks run. The TUI's `/skills`, `$` mentions, `/hooks`, `/init`,
`/agent`, and new `/goal` objectives follow the saved setting right away, so
they can differ from the running session until you start a new one with `/new`.
A goal that is already running keeps running after you turn Goals off, and you
can still pause, resume, edit, or clear it.
