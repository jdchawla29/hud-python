"""Typed Docker Compose data used by runtime adapters."""

from __future__ import annotations

import contextlib
import json
import shlex
import tarfile
import tempfile
from dataclasses import dataclass
from pathlib import Path
from typing import TYPE_CHECKING, Any

import yaml
from pydantic import BaseModel, ConfigDict, Field

if TYPE_CHECKING:
    from collections.abc import Iterator, Mapping


class ImageConfig(BaseModel):
    """OCI image defaults that Compose applies when service fields are unset."""

    model_config = ConfigDict(extra="ignore", populate_by_name=True)

    user: str | None = Field(default=None, alias="User")
    working_dir: str | None = Field(default=None, alias="WorkingDir")
    entrypoint: list[str] | None = Field(default=None, alias="Entrypoint")
    command: list[str] | None = Field(default=None, alias="Cmd")
    environment: list[str] = Field(default_factory=list, alias="Env")
    exposed_ports: dict[str, Any] = Field(default_factory=dict, alias="ExposedPorts")

    @classmethod
    def from_dockerfile(cls, path: Path) -> ImageConfig:
        """Read final-stage image defaults needed before a remote build."""
        logical: list[str] = []
        pending = ""
        for raw_line in path.read_text(encoding="utf-8").splitlines():
            line = raw_line.strip()
            if not line or line.startswith("#"):
                continue
            pending = f"{pending} {line}".strip()
            if pending.endswith("\\"):
                pending = pending[:-1].rstrip()
                continue
            logical.append(pending)
            pending = ""
        if pending:
            logical.append(pending)

        values: dict[str, Any] = {"Env": [], "ExposedPorts": {}}
        environment: dict[str, str] = {}
        for line in logical:
            instruction, separator, value = line.partition(" ")
            if not separator:
                continue
            instruction = instruction.upper()
            value = value.strip()
            if instruction == "FROM":
                values = {"Env": [], "ExposedPorts": {}}
                environment = {}
            elif instruction == "USER":
                values["User"] = value
            elif instruction == "WORKDIR":
                values["WorkingDir"] = value
            elif instruction == "ENV":
                tokens = shlex.split(value)
                if not tokens or any("=" not in token for token in tokens):
                    raise ValueError(f"remote adaptation requires ENV key=value in {path}")
                for token in tokens:
                    key, _, item = token.partition("=")
                    environment[key] = item
                values["Env"] = [f"{key}={item}" for key, item in environment.items()]
            elif instruction in {"ENTRYPOINT", "CMD"}:
                command = json.loads(value) if value.startswith("[") else ["/bin/sh", "-c", value]
                if not isinstance(command, list) or not all(
                    isinstance(item, str) for item in command
                ):
                    raise ValueError(f"invalid {instruction} in {path}")
                values["Entrypoint" if instruction == "ENTRYPOINT" else "Cmd"] = command
            elif instruction == "EXPOSE":
                values["ExposedPorts"] = {
                    port if "/" in port else f"{port}/tcp": {} for port in shlex.split(value)
                }
        return cls.model_validate(values)


class ComposeHealthcheck(BaseModel):
    model_config = ConfigDict(extra="allow")

    disable: bool | None = None
    test: list[str] | None = Field(default=None, min_length=1)
    interval: str | None = None
    timeout: str | None = None
    start_period: str | None = None
    start_interval: str | None = None
    retries: int | None = None


class ComposePort(BaseModel):
    model_config = ConfigDict(extra="allow")

    target: int
    protocol: str = "tcp"


class ComposeService(BaseModel):
    """The normalized subset of a Compose service that HUD runtimes execute."""

    model_config = ConfigDict(extra="allow")

    image: str | None = None
    build: str | dict[str, Any] | None = None
    user: str | int | None = None
    environment: dict[str, str] = Field(default_factory=dict)
    entrypoint: list[str] | None = None
    command: list[str] | None = None
    working_dir: str | None = None
    healthcheck: ComposeHealthcheck | None = None
    expose: list[str] = Field(default_factory=list)
    ports: list[ComposePort] = Field(default_factory=list)
    volumes: list[str | dict[str, Any]] = Field(default_factory=list)

    def with_image(self, image: str, config: ImageConfig) -> ComposeService:
        entrypoint = (config.entrypoint or []) if self.entrypoint is None else self.entrypoint
        if self.command is not None:
            command = self.command
        elif self.entrypoint is not None:
            command = []
        else:
            command = config.command or []
        return self.model_copy(
            update={
                "image": image,
                "entrypoint": entrypoint,
                "command": command,
                "working_dir": self.working_dir or config.working_dir or "/",
            }
        )

    @property
    def argv(self) -> list[str]:
        if self.entrypoint is None or self.command is None:
            raise RuntimeError(f"image defaults were not resolved for {self.image!r}")
        return [*self.entrypoint, *self.command]

    def shell_command(self) -> str:
        command = shlex.join(self.argv)
        if self.working_dir:
            return f"cd {shlex.quote(self.working_dir)} && {command}"
        return command


class ComposeConfig(BaseModel):
    """Normalized Compose data with unknown fields preserved for native runtimes."""

    model_config = ConfigDict(extra="allow")

    name: str | None = None
    services: dict[str, ComposeService]
    networks: dict[str, dict[str, Any] | None] = Field(default_factory=dict)

    @classmethod
    def from_file(cls, path: Path) -> ComposeConfig:
        """Load a self-contained authored Compose document without Docker."""
        source = path.read_text(encoding="utf-8")
        if "${" in source:
            raise ValueError("remote adaptation does not support Compose interpolation")
        raw = yaml.safe_load(source)
        if not isinstance(raw, dict):
            raise ValueError(f"{path.name} is not a Compose document")
        document = raw
        if any(field in document for field in ("include", "extends")):
            raise ValueError("remote adaptation does not support Compose includes")
        services = document.get("services")
        if isinstance(services, dict):
            for raw_service in services.values():
                if isinstance(raw_service, dict) and isinstance(raw_service.get("expose"), list):
                    raw_service["expose"] = [str(port) for port in raw_service["expose"]]
        return cls.model_validate(document)


class ComposeProjectRef(BaseModel):
    """Platform reference to a Compose file within an uploaded project."""

    model_config = ConfigDict(extra="forbid")

    compose_path: str


@dataclass(frozen=True, slots=True)
class ComposeSource:
    """One authored or platform-wire Compose runtime source."""

    document: Path | ComposeConfig
    project: Path | ComposeProjectRef | None = None

    def request_payload(self) -> dict[str, Any]:
        if isinstance(self.document, Path):
            document = ComposeConfig.from_file(self.document)
        else:
            document = self.document
        payload: dict[str, Any] = {
            "compose": document.model_dump(mode="json", exclude_none=True),
        }
        if isinstance(self.project, Path):
            if not isinstance(self.document, Path):
                raise ValueError("compose_project as a path requires compose as a path")
            try:
                compose_path = (
                    self.document.resolve().relative_to(self.project.resolve()).as_posix()
                )
            except ValueError:
                raise ValueError("runtime_config.compose must be inside compose_project") from None
            payload["compose_project"] = {"compose_path": compose_path}
        elif self.project is not None:
            payload["compose_project"] = self.project.model_dump(mode="json")
        return payload

    def runnable_path(self, provider: str) -> Path:
        if not isinstance(self.document, Path):
            raise ValueError(f"{provider} requires runtime_config.compose as a local file path")
        return self.document.resolve()


@dataclass(frozen=True, slots=True)
class ComposeLaunchFiles:
    compose: Path
    override: Path
    ports: Path
    archive: Path | None


@dataclass(frozen=True, slots=True)
class ComposeProject:
    """A local Compose project staged with HUD's main-service overrides."""

    compose: Path

    @contextlib.contextmanager
    def stage(
        self,
        published_port: str,
        *,
        seccomp: str | Path,
        service_socket: str | None = None,
        env_vars: Mapping[str, str] | None = None,
        cpu: float | None = None,
        memory_mb: int | None = None,
        gpu_count: int | None = None,
        archive: bool = False,
    ) -> Iterator[ComposeLaunchFiles]:
        main: dict[str, Any] = {
            "security_opt": [f"seccomp={seccomp}", "systempaths=unconfined"],
        }
        if service_socket is not None:
            main["volumes"] = [
                {
                    "type": "bind",
                    "source": service_socket,
                    "target": "/media/hud/docker.sock",
                }
            ]
        if env_vars:
            main["environment"] = dict(env_vars)
        if cpu is not None:
            main["cpus"] = cpu
        if memory_mb is not None:
            main["mem_limit"] = f"{memory_mb}m"
        if gpu_count is not None:
            main["gpus"] = gpu_count

        with tempfile.TemporaryDirectory(prefix="hud-compose-") as directory:
            root = Path(directory)
            override = root / "override.json"
            override.write_text(
                json.dumps({"services": {"main": main}}),
                encoding="utf-8",
            )
            ports = root / "ports.yaml"
            ports.write_text(
                f'services:\n  main:\n    ports: !override ["{published_port}"]\n',
                encoding="utf-8",
            )
            archive_path = None
            if archive:
                archive_path = root / "project.tar.gz"
                with tarfile.open(archive_path, "w:gz") as tar:
                    for entry in self.compose.parent.iterdir():
                        tar.add(entry, arcname=entry.name)
            yield ComposeLaunchFiles(
                compose=self.compose,
                override=override,
                ports=ports,
                archive=archive_path,
            )


__all__ = [
    "ComposeConfig",
    "ComposeHealthcheck",
    "ComposePort",
    "ComposeProject",
    "ComposeProjectRef",
    "ComposeService",
    "ComposeSource",
    "ImageConfig",
]
