"""Python fixture env with an SSH workspace, for hud-rs ssh/2 client tests.

Serves a workspace rooted at $SLATE_WORK_DIR (or a temp dir) over ssh/2.
"""

from __future__ import annotations

import os
import tempfile

from hud.environment import Environment

env = Environment("workspace-env", version="0.1.0")
root = os.environ.get("SLATE_WORK_DIR") or tempfile.mkdtemp(prefix="hud-rs-ws-")
env.workspace(root)


@env.template(id="coding_task", description="Free-form coding task in the workspace.")
async def coding_task(task_description: str):
    yield task_description
    yield 1.0
