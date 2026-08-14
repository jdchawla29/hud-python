"""The HUD environment for a DAS workspace and one open task.

Written to a temp file and served by `python -m hud.environment.server` via
the Rust `LocalRuntime` provider; the workspace root arrives as
$DAS_WORK_DIR. The agent loop and CLI harness stay in the Rust process; the
prompt, workspace file tracking, and grade live here.
"""

from __future__ import annotations

import os

from hud.environment import Environment

env = Environment("das-coding", version="0.1.0")
# track_files publishes a filetracking/1 capability so the Rust harness can
# stream live workspace diffs (the "changes" pane).
env.workspace(os.path.abspath(os.environ.get("DAS_WORK_DIR") or os.getcwd()), track_files=True)


@env.template(id="coding_task", description="Free-form coding task in the workspace.")
async def coding_task(task_description: str):
    yield task_description
    yield 1.0  # simple success - task completed
