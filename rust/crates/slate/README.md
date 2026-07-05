# slate

A Slate-style thread-weaving coding agent, as an interactive TUI on the HUD
Rust SDK. A Rust port and extension of `cookbooks/slate-coding/slate_agent.py`.

One **orchestrator** model plans and never touches the environment. It
dispatches single bounded actions to persistent **worker threads**, each acting
on the task workspace over the `ssh/2` capability and compressing what it did
into an **episode** that returns to the orchestrator. The session is
interactive: after a reply the workspace, SSH connection, and conversation stay
live, so you can send follow-ups or interrupt in flight.

The workspace environment is served by the reference **Python** SDK
(`python -m hud.environment.server`), driven entirely from Rust over the
`hud/1.0` wire protocol — a Rust TUI driving a Python-served environment.

## Requirements

- `HUD_API_KEY` — gateway inference. Export it or put it in `~/.hud/.env`.
- A `hud-python` checkout — pass `--hud-python <path>` or set `HUD_PYTHON_DIR`.
- `uv` on `PATH` (used to run the Python env).

## Run

```sh
# Interactive TUI over the current directory
slate --work-dir .

# Start with a task; add follow-ups in the input line
slate --task "Add a --json flag to cli.py and cover it with a test" --work-dir ./project

# Cheaper models
slate --orch-model claude-haiku-4-5 --worker-model claude-haiku-4-5
```

In the TUI: type a message and **Enter** to send; **Esc** interrupts the
current turn (the session stays alive); **Ctrl-C** quits. Slash commands:
`/help`, `/diff`, `/threads`, `/clear`, `/quit`.

Panes: the **weave** (orchestrator turns, dispatches, bash lines, episodes),
the **threads** sidebar (per-thread action/command/episode counts), and a live
**changes** pane fed by the workspace's `filetracking/1` capability. The title
bar shows a running token count.

## Sessions

Every run is saved under `~/.slate/sessions/<id>/` (transcript + conversation).

```sh
slate --list-sessions            # list saved sessions
slate --resume <id>              # continue a session's conversation
slate --no-save                  # don't persist this run
```

Resume reuses the session's `work_dir` and models, reloads the orchestrator
conversation so it continues with full context, and replays the transcript into
the weave. Worker threads are not restored — they are disposable tactical
context; their durable conclusions already live in the conversation as episodes.

## Headless

For scripts and CI (no TTY). `--task` and each repeated `--message` run as turns
in one persistent session; the weave prints to stdout.

```sh
slate --headless \
  --task "Create calc.py with add(a,b); verify it" \
  --message "Now add subtract(a,b) and verify it"
```

## Docker (experimental)

`--docker <image>` serves the workspace from a container instead of a local
process. The image must serve a `slate-coding` env on port 8765; no such image
is built here, so this path is unverified.
