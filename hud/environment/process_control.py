"""MCP control surface for the environment process namespace."""

from __future__ import annotations

import asyncio
import secrets
import socket
from typing import TYPE_CHECKING

import fastmcp
import uvicorn

from hud.capabilities import Capability

if TYPE_CHECKING:
    from starlette.types import ASGIApp, Receive, Scope, Send

    from hud.environment.workspace import Workspace


class _BearerAuth:
    def __init__(self, app: ASGIApp, token: str) -> None:
        self.app = app
        self.authorization = f"Bearer {token}".encode()

    async def __call__(self, scope: Scope, receive: Receive, send: Send) -> None:
        if scope["type"] == "http":
            headers = dict(scope["headers"])
            if headers.get(b"authorization") != self.authorization:
                await send(
                    {
                        "type": "http.response.start",
                        "status": 401,
                        "headers": [(b"content-type", b"text/plain")],
                    }
                )
                await send({"type": "http.response.body", "body": b"unauthorized"})
                return
        await self.app(scope, receive, send)


class ProcessControl:
    """Authenticated MCP server backed by one workspace's environment processes."""

    def __init__(self, workspace: Workspace) -> None:
        self.workspace = workspace
        self.token = secrets.token_urlsafe(32)
        self._server: uvicorn.Server | None = None
        self._task: asyncio.Task[None] | None = None
        self._socket: socket.socket | None = None
        self._port: int | None = None

    async def start(self) -> Capability:
        if self._task is not None:
            return self.capability()

        mcp = fastmcp.FastMCP("workspace-processes")

        @mcp.tool()
        async def list_processes() -> list[dict[str, object]]:
            """List processes outside the agent sandbox in the environment PID namespace."""
            return await self.workspace.processes()

        @mcp.tool()
        async def signal_process(
            pid: int,
            start_time: int,
            signal: str,
        ) -> dict[str, object]:
            """Send HUP, INT, TERM, KILL, USR1, or USR2 to an environment process.

            Use the pid and start_time of a process marked signalable by list_processes. Use
            HUP when a server supports configuration reloads. Only top-level environment
            daemons are signalable; HUD infrastructure and PID 1 are not listed.
            """
            normalized = signal.removeprefix("SIG").upper()
            await self.workspace.signal_process(pid, start_time, normalized)
            return {"pid": pid, "start_time": start_time, "signal": normalized}

        listener = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
        listener.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
        listener.bind(("127.0.0.1", 0))
        listener.listen()
        listener.setblocking(False)
        self._socket = listener
        self._port = listener.getsockname()[1]
        app = _BearerAuth(
            mcp.http_app(path="/mcp", stateless_http=True),
            self.token,
        )
        self._server = uvicorn.Server(
            uvicorn.Config(app, log_level="warning", lifespan="on", access_log=False)
        )
        self._task = asyncio.create_task(self._server.serve(sockets=[listener]))
        try:
            async with asyncio.timeout(10):
                while not self._server.started:
                    if self._task.done():
                        await self._task
                    await asyncio.sleep(0)
        except BaseException:
            await self.close()
            raise
        return self.capability()

    def capability(self) -> Capability:
        if self._port is None:
            raise RuntimeError("workspace process control is not running")
        capability = Capability.mcp(
            name="workspace-processes",
            url=f"http://127.0.0.1:{self._port}/mcp",
            auth_token=self.token,
            transport="streamable-http",
        )
        capability.params["controller_bridge"] = True
        return capability

    async def close(self) -> None:
        server, self._server = self._server, None
        task, self._task = self._task, None
        listener, self._socket = self._socket, None
        self._port = None
        if server is not None:
            server.should_exit = True
        try:
            if task is not None:
                try:
                    await asyncio.wait_for(task, 10)
                except TimeoutError:
                    task.cancel()
                    await asyncio.gather(task, return_exceptions=True)
        finally:
            if listener is not None:
                listener.close()


__all__ = ["ProcessControl"]
