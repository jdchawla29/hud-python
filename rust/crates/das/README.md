# das

DAS manages projects, Git worktrees, and interactive thread-weaving coding
sessions in one Rust application on the HUD SDK.

The orchestrator plans through either the HUD gateway or a tool-disabled Claude
Code session and dispatches bounded actions to persistent Codex or Claude CLI
workers. HUD provides the local workspace runtime, file tracking, and rollout
trace. The interactive TUI shows the weave, worker threads, commands, workspace
changes, and token usage.

## Requirements

- `HUD_API_KEY` exported or stored in `~/.hud/.env` when using the gateway
  orchestrator.
- `uv` on `PATH` for the HUD environment server.
- `codex` and a running app-server daemon for Codex workers.
- `claude` for Claude orchestration or workers.

## Projects and worktrees

Run `das` with no arguments to open the interactive dashboard. It provides
responsive project, workspace, and session panes. Use Tab or the arrow keys to
move between panes, Enter to open the selected item, `p` to register a project,
`w` to create a worktree, `o` to configure a session, `a` to safely archive a
worktree, and `?` for the complete key map. Archiving refuses dirty or unmerged
worktrees and always retains the branch.

The command surface remains available for scripts:

```sh
das project add hud /path/to/repository
das project list

das workspace create hud feature
das workspace list hud
das workspace archive hud feature
```

Projects and worktrees are stored in `$DAS_HOME/state.sqlite3`, defaulting to
`~/.das/state.sqlite3`. Worktrees default to
`$DAS_HOME/worktrees/<project>/<workspace>`.

## Interactive sessions

```sh
# Claude Code orchestrator with Codex workers
das open hud feature --orch-harness claude --orch-model opus \
  --worker-harness codex --worker-model gpt-5.6

# HUD gateway orchestrator with an explicit model
das open hud feature --orch-harness gateway \
  --orch-model claude-opus-4-8 --worker-harness codex

# Existing managed Codex daemon and configured default worker model
das open hud feature --worker-harness codex

# Explicit Codex socket and model
das open hud feature --worker-harness codex \
  --codex-socket /path/to/codex.sock --worker-model gpt-5.4

# Persistent Claude CLI workers
das open hud feature --worker-harness claude --worker-model sonnet
```

DAS connects to the existing daemon through `codex app-server proxy`; it does
not start another server or require remote control. Each logical worker maps to
one persistent native Codex thread or Claude session.

The orchestrator and workers select their harnesses and models independently.
The default orchestrator is `gateway/claude-opus-4-8`; the default worker is
Codex with its configured model. A Claude orchestrator uses the local Claude
Code sign-in and configured model when `--orch-model` is omitted. DAS removes
Anthropic API environment overrides from Claude CLI child processes so the
local Claude Code account remains the authentication owner. Its built-in tools,
plugins, hooks, MCP servers, and project instructions are disabled; it can act
only by returning schema-validated DAS `dispatch` or `finish` decisions.

In a coding session, Enter sends a message, Esc interrupts the current turn,
and Ctrl-C quits. Tab moves focus across the thread, change, conversation, and
message panes; Enter opens the selected thread or file. `:` opens a searchable
command palette. Replies and worker episodes use Markdown-aware terminal
styling. `/diff` renders the complete tracked Git patch, `/thread <id>` shows a
worker's command and episode history, and `/activity` collapses or expands
captured command output. Other slash commands are `/help`, `/threads`, `/clear`,
and `/quit`.

Sessions are stored under `$DAS_HOME/sessions/<id>` and belong to a registered
project and worktree:

```sh
das sessions hud feature
das open hud feature --resume <id>
```

For scripts and CI, use the same managed worktree without the TUI:

```sh
das open hud feature --headless \
  --task "Create calc.py with add(a, b) and verify it" \
  --message "Now add subtract(a, b) and verify it"
```
