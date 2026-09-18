"""MCP transport bridges for CLI agents running over SSH."""

from __future__ import annotations

import asyncio
import contextlib
import json
import logging
import os
import secrets
import shlex
import sys
from contextlib import asynccontextmanager
from pathlib import PurePosixPath
from typing import TYPE_CHECKING, Any

import asyncssh
from fastmcp.client.transports import SSETransport, StreamableHttpTransport
from fastmcp.server import create_proxy

from hud.capabilities import Capability

if TYPE_CHECKING:
    from collections.abc import AsyncIterator, Mapping

    from hud.capabilities import SSHClient

CAPABILITY_ENV = "HUD_MCP_CAPABILITY"
_PROCESS_CLOSE_TIMEOUT_S = 5.0
_BRIDGE_READY_TIMEOUT_S = 5.0
_REMOTE_TMP = PurePosixPath("/") / "tmp"

logger = logging.getLogger(__name__)


async def run_mcp_proxy(environ: Mapping[str, str] = os.environ) -> None:
    """Proxy one routed HTTP MCP capability onto this process's stdio."""
    capability = Capability.from_manifest(json.loads(environ[CAPABILITY_ENV]))
    token = capability.params.get("auth_token")
    headers = {"Authorization": f"Bearer {token}"} if token else None
    transport_name = capability.params.get("transport")
    if transport_name == "streamable-http":
        transport = StreamableHttpTransport(capability.url, headers=headers)
    elif transport_name == "sse":
        transport = SSETransport(capability.url, headers=headers)
    else:
        raise ValueError(f"unsupported bridged MCP transport {transport_name!r}")
    proxy = create_proxy(transport, name=capability.name)
    await proxy.run_async(transport="stdio", show_banner=False)


@asynccontextmanager
async def bridge_mcp(
    ssh: SSHClient,
    capability: Capability,
    *,
    shell: str,
) -> AsyncIterator[dict[str, Any]]:
    """Bridge a routed HTTP MCP capability into a remote CLI over stdio."""
    async with bridge_stdio_module(
        ssh,
        "hud.agents.cli_mcp",
        {CAPABILITY_ENV: json.dumps(capability.to_manifest(), separators=(",", ":"))},
        shell=shell,
        label=f"MCP {capability.name}",
    ) as config:
        yield config


@asynccontextmanager
async def bridge_stdio_module(
    ssh: SSHClient,
    module: str,
    environ: Mapping[str, str],
    *,
    shell: str,
    label: str,
    path_prefix: str = "hud-mcp",
) -> AsyncIterator[dict[str, Any]]:
    """Bridge one controller-side stdio module into a remote POSIX shell."""
    if shell in {"cmd", "powershell"}:
        raise RuntimeError(f"{label} requires a POSIX workspace")

    token = secrets.token_hex(16)
    request_path = str(_REMOTE_TMP / f"{path_prefix}-{token}.request")
    response_path = str(_REMOTE_TMP / f"{path_prefix}-{token}.response")
    bridge = await ssh.create_process(_bridge_command(request_path, response_path))
    local: asyncio.subprocess.Process | None = None
    tasks: list[asyncio.Task[None]] = []
    try:
        ready = await asyncio.wait_for(bridge.stderr.readline(), _BRIDGE_READY_TIMEOUT_S)
        if ready != b"ready\n":
            detail = ready.decode("utf-8", "replace").strip()
            raise RuntimeError(detail or f"{label} SSH bridge did not become ready")

        local = await asyncio.create_subprocess_exec(
            sys.executable,
            "-m",
            module,
            stdin=asyncio.subprocess.PIPE,
            stdout=asyncio.subprocess.PIPE,
            stderr=asyncio.subprocess.PIPE,
            env={**os.environ, **environ},
        )
        assert local.stdin is not None
        assert local.stdout is not None
        assert local.stderr is not None
        tasks = [
            asyncio.create_task(_copy_stream(bridge.stdout, local.stdin)),
            asyncio.create_task(_copy_stream(local.stdout, bridge.stdin)),
            asyncio.create_task(_log_stream(bridge.stderr, f"{label} SSH bridge")),
            asyncio.create_task(_log_stream(local.stderr, label)),
        ]
        yield _relay_config(request_path, response_path)
    finally:
        for task in tasks:
            task.cancel()
        await asyncio.gather(*tasks, return_exceptions=True)
        bridge.stdin.close()
        bridge.channel.close()
        with contextlib.suppress(OSError, TimeoutError, asyncssh.Error):
            await asyncio.wait_for(bridge.wait_closed(), _PROCESS_CLOSE_TIMEOUT_S)
        if local is not None:
            if local.stdin is not None:
                local.stdin.close()
            if local.returncode is None:
                with contextlib.suppress(ProcessLookupError):
                    local.terminate()
            try:
                await asyncio.wait_for(local.wait(), _PROCESS_CLOSE_TIMEOUT_S)
            except TimeoutError:
                with contextlib.suppress(ProcessLookupError):
                    local.kill()
                with contextlib.suppress(TimeoutError):
                    await asyncio.wait_for(local.wait(), _PROCESS_CLOSE_TIMEOUT_S)


def _bridge_command(request_path: str, response_path: str) -> str:
    request = shlex.quote(request_path)
    response = shlex.quote(response_path)
    cleanup = shlex.quote(f"rm -f -- {request} {response}")
    return (
        "set -eu; umask 077; "
        f"rm -f -- {request} {response}; mkfifo -- {request} {response}; "
        f"trap {cleanup} EXIT HUP INT TERM; "
        "printf 'ready\\n' >&2; "
        f"cat {request} & reader=$!; cat > {response}; wait $reader"
    )


def _relay_config(request_path: str, response_path: str) -> dict[str, Any]:
    request = shlex.quote(request_path)
    response = shlex.quote(response_path)
    script = f"cat {response} & reader=$!; cat > {request}; wait $reader"
    return {"type": "stdio", "command": "sh", "args": ["-c", script]}


async def _copy_stream(
    reader: asyncio.StreamReader | asyncssh.SSHReader[bytes],
    writer: asyncio.StreamWriter | asyncssh.SSHWriter[bytes],
) -> None:
    try:
        while chunk := await reader.read(65536):
            writer.write(chunk)
            await writer.drain()
    finally:
        writer.close()


async def _log_stream(
    reader: asyncio.StreamReader | asyncssh.SSHReader[bytes],
    source: str,
) -> None:
    while line := await reader.readline():
        logger.warning("%s: %s", source, line.decode("utf-8", "replace").rstrip())


if __name__ == "__main__":
    asyncio.run(run_mcp_proxy())


__all__ = ["CAPABILITY_ENV", "bridge_mcp", "bridge_stdio_module", "run_mcp_proxy"]
