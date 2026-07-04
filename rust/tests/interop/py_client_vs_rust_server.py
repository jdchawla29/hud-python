"""Drive a Rust-served HUD environment with the reference Python client.

Usage: uv run python py_client_vs_rust_server.py <port>
Prints PY-INTEROP-OK and exits 0 on success.
"""

from __future__ import annotations

import asyncio
import sys
from urllib.parse import urlsplit

from hud.clients import connect
from hud.eval.runtime import Runtime


async def main() -> None:
    port = int(sys.argv[1])
    async with connect(Runtime(url=f"tcp://127.0.0.1:{port}"), ready_timeout=30) as client:
        manifest = client.manifest
        assert manifest is not None
        assert manifest.server_info.name == "echo-env", manifest
        assert manifest.session_id.startswith("sess-"), manifest

        tasks = await client.list_tasks()
        assert any(t["id"] == "echo" for t in tasks), tasks

        started = await client.start_task("echo", {"text": "ping"})
        assert started["prompt"] == "Repeat exactly: ping", started

        graded = await client.grade({"answer": "ping"})
        assert graded["score"] == 1.0, graded

        # The Rust server's loopback capability must tunnel through the
        # control port via the Python client's local forwarder.
        cap = client.binding("echo-bytes")
        parts = urlsplit(cap.url)
        assert parts.hostname is not None and parts.port is not None, cap
        reader, writer = await asyncio.open_connection(parts.hostname, parts.port)
        writer.write(b"tunnel-ok")
        await writer.drain()
        data = await reader.readexactly(9)
        assert data == b"tunnel-ok", data
        writer.close()

    print("PY-INTEROP-OK")


asyncio.run(main())
