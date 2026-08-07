"""``hud.eval.deploy`` — the platform build exchange, without the CLI."""

from __future__ import annotations

import json
from typing import TYPE_CHECKING, Any

import pytest

from hud.eval.deploy import BuildOutcome, DeployError, await_build, trigger_build
from hud.eval.runtime import RuntimeConfig
from hud.utils.exceptions import HudRequestError
from hud.utils.platform import PlatformClient

if TYPE_CHECKING:
    from pathlib import Path


class _FakePlatform(PlatformClient):
    """Records the trigger payload and answers status from a scripted list."""

    payload: dict[str, Any] | None = None
    statuses: list[dict[str, Any] | Exception] = []  # noqa: RUF012 - per instance
    asked: int = 0

    async def apost(self, path: str, *, json: Any = None) -> dict[str, Any]:
        assert path == "/builds/trigger"
        object.__setattr__(self, "payload", json)
        return {"id": "build-1", "registry_id": "registry-1"}

    async def aget(
        self,
        path: str,
        *,
        params: dict[str, Any] | None = None,
    ) -> dict[str, Any]:
        assert path == "/builds/build-1/status"
        assert params is None
        scripted = self.statuses[min(self.asked, len(self.statuses) - 1)]
        object.__setattr__(self, "asked", self.asked + 1)
        if isinstance(scripted, Exception):
            raise scripted
        return scripted


def _platform(statuses: list[dict[str, Any] | Exception] | None = None) -> _FakePlatform:
    platform = _FakePlatform("https://api.example", "key")
    object.__setattr__(platform, "statuses", statuses or [])
    object.__setattr__(platform, "asked", 0)
    return platform


async def test_trigger_sends_the_runtime_the_caller_asked_for() -> None:
    platform = _platform()

    build_id, registry_id = await trigger_build(
        platform, build_id="build-1", name="test-env", runtime="modal"
    )

    assert (build_id, registry_id) == ("build-1", "registry-1")
    assert platform.payload is not None
    assert platform.payload["runtime_provider"] == "modal"
    assert platform.payload["name"] == "test-env"


async def test_trigger_serializes_authored_compose_project(tmp_path: Path) -> None:
    project = tmp_path / "project"
    project.mkdir()
    compose = project / "compose.json"
    compose.write_text(
        json.dumps(
            {
                "services": {
                    "main": {
                        "image": "hud-harbor:local",
                        "build": {"context": "./main"},
                    }
                }
            }
        ),
        encoding="utf-8",
    )
    platform = _platform()

    await trigger_build(
        platform,
        build_id="build-1",
        name="test-env",
        runtime="modal",
        runtime_config=RuntimeConfig(compose=compose, compose_project=project),
    )

    assert platform.payload is not None
    assert platform.payload["runtime_provider"] == "modal"
    assert platform.payload["runtime_config"] == {
        "compose": {
            "services": {
                "main": {
                    "image": "hud-harbor:local",
                    "build": {"context": "./main"},
                    "environment": {},
                    "expose": [],
                    "ports": [],
                    "volumes": [],
                }
            },
            "networks": {},
        },
        "compose_project": {"compose_path": "compose.json"},
    }


async def test_trigger_leaves_compose_runtime_selection_to_platform(tmp_path: Path) -> None:
    compose = tmp_path / "compose.json"
    compose.write_text(
        '{"services":{"main":{"build":{"context":"."}},"redis":{"image":"redis:7"}}}',
        encoding="utf-8",
    )
    platform = _platform()

    await trigger_build(
        platform,
        build_id="build-1",
        name="test-env",
        runtime_config=RuntimeConfig(compose=compose, compose_project=tmp_path),
    )

    assert platform.payload is not None
    assert "runtime_provider" not in platform.payload


async def test_trigger_rejects_internal_runtime_name() -> None:
    with pytest.raises(DeployError, match="runtime must be 'hud' or 'modal'"):
        await trigger_build(
            _platform(),
            build_id="build-1",
            name="test-env",
            runtime="ec2",
        )


async def test_trigger_sends_wire_runtime_config_unchanged() -> None:
    runtime_config = {"resources": {"gpu": {"type": "A10G", "count": 1}}}
    platform = _platform()

    await trigger_build(
        platform,
        build_id="build-1",
        name="test-env",
        runtime="modal",
        runtime_config=runtime_config,
    )

    assert platform.payload is not None
    assert platform.payload["runtime_config"] == runtime_config


async def test_trigger_omits_what_the_caller_did_not_set() -> None:
    platform = _platform()

    await trigger_build(platform, build_id="build-1", name="test-env")

    assert platform.payload is not None
    assert not {
        "registry_id",
        "runtime_provider",
        "runtime_config",
        "environment_variables",
        "build_args",
        "build_secrets",
    } & set(platform.payload)


async def test_await_returns_the_first_terminal_status() -> None:
    platform = _platform([{"status": "IN_PROGRESS"}, {"status": "SUCCEEDED", "version": "3"}])

    final = await await_build(platform, "build-1", poll_interval=0)

    assert final["status"] == "SUCCEEDED"
    assert final["version"] == "3"


async def test_await_survives_a_transient_status_failure() -> None:
    platform = _platform(
        [HudRequestError("boom", status_code=502), {"status": "SUCCEEDED"}],
    )

    final = await await_build(platform, "build-1", poll_interval=0)

    assert final["status"] == "SUCCEEDED"
    assert platform.asked == 2


async def test_await_gives_up_at_the_deadline() -> None:
    platform = _platform([{"status": "IN_PROGRESS"}])

    final = await await_build(platform, "build-1", poll_interval=0, max_wait=0)

    assert final["status"] == "TIMED_OUT"


@pytest.mark.parametrize(
    ("status", "succeeded"),
    [("SUCCEEDED", True), ("FAILED", False), ("TIMED_OUT", False)],
)
def test_outcome_reports_success_only_for_a_succeeded_build(status: str, succeeded: bool) -> None:
    outcome = BuildOutcome(build_id="b", registry_id="r", status=status)

    assert outcome.succeeded is succeeded
