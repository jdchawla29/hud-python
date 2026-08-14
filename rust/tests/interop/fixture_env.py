"""Python fixture environment for hud-rs interop tests.

Mirrors the Rust `echo_env` example: an `echo` task plus a loopback TCP
byte-echo capability, so a Rust client can exercise the full control session
and `tunnel.open` against the reference Python server.

Serve with: uv run python -m hud.environment.server fixture_env.py
"""

from __future__ import annotations

import asyncio

from hud.capabilities import Capability
from hud.environment import Environment

env = Environment("echo-env", version="0.1.0")


@env.template(id="echo", description="Repeat the given text exactly.")
async def echo(text: str):
    answer = yield f"Repeat exactly: {text}"
    yield 1.0 if str(answer).strip() == text else 0.0


@env.initialize
async def start_echo_daemon() -> None:
    async def handle(reader: asyncio.StreamReader, writer: asyncio.StreamWriter) -> None:
        while data := await reader.read(4096):
            writer.write(data)
            await writer.drain()
        writer.close()

    server = await asyncio.start_server(handle, "127.0.0.1", 0)
    port = server.sockets[0].getsockname()[1]
    env.add_capability(
        Capability(name="echo-bytes", protocol="raw/1", url=f"tcp://127.0.0.1:{port}")
    )
