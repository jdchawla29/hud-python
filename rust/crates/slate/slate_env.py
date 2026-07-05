"""The environment the slate TUI serves: an SSH workspace + one open task.

Written to a temp file and served by `python -m hud.environment.server` via
the Rust `LocalRuntime` provider; the workspace root arrives as
$SLATE_WORK_DIR. The agent loop stays in the Rust process — only the prompt,
the workspace, and the grade live here.
"""

from __future__ import annotations

import os

from hud.environment import Environment

env = Environment("slate-coding", version="0.1.0")
# track_files publishes a filetracking/1 capability so the Rust harness can
# stream live workspace diffs (the "changes" pane).
env.workspace(os.path.abspath(os.environ.get("SLATE_WORK_DIR") or os.getcwd()), track_files=True)


@env.template(id="coding_task", description="Free-form coding task in the workspace.")
async def coding_task(task_description: str):
    yield task_description
    yield 1.0  # simple success - task completed
