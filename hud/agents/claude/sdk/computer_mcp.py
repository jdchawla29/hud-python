"""MCP server that exposes computer-use over VNC.

Single tool ``computer`` backed by ``ClaudeComputerTool`` / ``RFBTool``.
"""

from __future__ import annotations

import asyncio
import json
import os
from typing import TYPE_CHECKING, Any

import fastmcp
from fastmcp.exceptions import ToolError
from pydantic import TypeAdapter

from hud.agents import cli_mcp
from hud.agents.claude.tools.computer import ClaudeComputerTool
from hud.agents.tools.base import AgentToolSpec, result_text
from hud.capabilities import Capability
from hud.capabilities.rfb import RFBClient, ScreenshotEncoding, WebPScreenshotEncoding

if TYPE_CHECKING:
    from collections.abc import Mapping
    from contextlib import AbstractAsyncContextManager

    from hud.capabilities import SSHClient

_DEFAULT_SCREENSHOT_ENCODING = WebPScreenshotEncoding()
RFB_CAPABILITY_ENV = "HUD_RFB_CAPABILITY"
SCREENSHOT_ENCODING_ENV = "HUD_SCREENSHOT_ENCODING"


def create_computer_mcp(
    rfb: RFBClient,
    screenshot_encoding: ScreenshotEncoding = _DEFAULT_SCREENSHOT_ENCODING,
) -> fastmcp.FastMCP:
    """Build a FastMCP server with one ``computer`` tool backed by ``rfb``."""

    mcp = fastmcp.FastMCP("computer-use")
    tool = ClaudeComputerTool(
        spec=AgentToolSpec(api_type="computer", api_name="computer"),
        client=rfb,
        screenshot_encoding=screenshot_encoding,
    )

    @mcp.tool()
    async def computer(
        action: str,
        coordinate: list[int] | None = None,
        text: str | None = None,
        scroll_direction: str | None = None,
        scroll_amount: int | None = None,
        start_coordinate: list[int] | None = None,
        duration: float | None = None,
        repeat: int | None = None,
        region: list[int] | None = None,
    ) -> list[Any]:
        """Control a remote screen — screenshot, click, type, key, scroll, move, drag, wait, zoom.

        Actions: screenshot, left_click, right_click, middle_click, double_click,
        triple_click, mouse_move, move, type, key, scroll, left_click_drag, drag,
        wait, hold_key, cursor_position, zoom, left_mouse_down, left_mouse_up.

        Returns the resulting screenshot image so you can see the screen state.
        """
        arguments = {
            name: value
            for name, value in {
                "action": action,
                "coordinate": coordinate,
                "text": text,
                "scroll_direction": scroll_direction,
                "scroll_amount": scroll_amount,
                "start_coordinate": start_coordinate,
                "duration": duration,
                "repeat": repeat,
                "region": region,
            }.items()
            if value is not None
        }
        result = await tool.execute(arguments)
        if result.isError:
            raise ToolError(result_text(result) or "computer action failed")
        return result.content

    return mcp


async def run_computer_mcp(environ: Mapping[str, str] = os.environ) -> None:
    """Run computer-use over stdio in a controller-side child process."""
    capability = Capability.from_manifest(json.loads(environ[RFB_CAPABILITY_ENV]))
    screenshot_encoding = TypeAdapter(ScreenshotEncoding).validate_json(
        environ[SCREENSHOT_ENCODING_ENV]
    )

    rfb = await RFBClient.connect(capability)
    try:
        await create_computer_mcp(rfb, screenshot_encoding).run_async(
            transport="stdio",
            show_banner=False,
        )
    finally:
        await rfb.close()


def bridge_computer_mcp(
    ssh: SSHClient,
    capability: Capability,
    screenshot_encoding: ScreenshotEncoding = _DEFAULT_SCREENSHOT_ENCODING,
    *,
    shell: str,
) -> AbstractAsyncContextManager[dict[str, Any]]:
    """Bridge a controller-side computer MCP process into a remote POSIX shell."""
    return cli_mcp.bridge_stdio_module(
        ssh,
        "hud.agents.claude.sdk.computer_mcp",
        {
            RFB_CAPABILITY_ENV: json.dumps(capability.to_manifest(), separators=(",", ":")),
            SCREENSHOT_ENCODING_ENV: screenshot_encoding.model_dump_json(),
        },
        shell=shell,
        label="computer MCP",
        path_prefix="hud-computer",
    )


if __name__ == "__main__":
    asyncio.run(run_computer_mcp())


__all__ = [
    "RFB_CAPABILITY_ENV",
    "SCREENSHOT_ENCODING_ENV",
    "bridge_computer_mcp",
    "create_computer_mcp",
    "run_computer_mcp",
]
