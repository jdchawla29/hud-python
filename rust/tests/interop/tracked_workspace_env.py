"""Python fixture env with a file-tracked SSH workspace, for hud-rs
filetracking/1 client tests."""

from __future__ import annotations

import os
import tempfile

from hud.environment import Environment

env = Environment("tracked-workspace-env", version="0.1.0")
root = os.environ.get("SLATE_WORK_DIR") or tempfile.mkdtemp(prefix="hud-rs-ft-")
env.workspace(root, track_files=True)


@env.template(id="coding_task", description="Free-form coding task in the workspace.")
async def coding_task(task_description: str):
    yield task_description
    yield 1.0
