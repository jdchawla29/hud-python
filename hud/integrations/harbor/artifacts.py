"""Harbor artifact declarations and filesystem collection."""

from __future__ import annotations

import fnmatch
import os
import shutil
from pathlib import Path, PurePosixPath
from typing import Any

from pydantic import BaseModel, ConfigDict, Field, field_validator, model_validator


class Artifact(BaseModel):
    model_config = ConfigDict(extra="forbid")

    source: str = Field(pattern=r"^/")
    destination: str | None = None
    exclude: list[str] = Field(default_factory=list)
    service: str = Field(default="main", min_length=1)

    @model_validator(mode="before")
    @classmethod
    def expand_path(cls, value: Any) -> Any:
        return {"source": value} if isinstance(value, str) else value

    @field_validator("source")
    @classmethod
    def normalize_source(cls, value: str) -> str:
        path = PurePosixPath(value)
        if len(path.parts) == 1 or ".." in path.parts:
            raise ValueError("artifact source must name a path beneath /")
        return str(path)

    @field_validator("destination")
    @classmethod
    def validate_destination(cls, value: str | None) -> str | None:
        if value is None:
            return None
        if not value:
            return None
        if "\\" in value:
            raise ValueError("artifact destination must use forward slashes")
        path = PurePosixPath(value)
        if path.is_absolute() or not path.parts:
            raise ValueError("artifact destination must be a relative path")
        if ".." in path.parts:
            raise ValueError("artifact destination must not contain '..'")
        if value.rstrip("/") == "manifest.json":
            raise ValueError("artifact destination 'manifest.json' is reserved")
        return value


def exclude_artifact_paths(root: Path, patterns: list[str]) -> None:
    if not patterns or not root.is_dir() or root.is_symlink():
        return
    for entry in sorted(root.rglob("*"), key=lambda path: len(path.parts), reverse=True):
        relative = entry.relative_to(root).as_posix()
        parts = PurePosixPath(relative).parts
        candidates = (
            relative,
            f"./{relative}",
            *("/".join(parts[index:]) for index in range(1, len(parts))),
        )
        if not any(
            fnmatch.fnmatchcase(candidate, pattern)
            for candidate in candidates
            for pattern in patterns
        ):
            continue
        if entry.is_dir() and not entry.is_symlink():
            shutil.rmtree(entry)
        else:
            entry.unlink()


def copy_artifact(source: Path, target: Path, exclude: list[str]) -> None:
    if source.is_symlink():
        raise RuntimeError(f"artifact {source} is a symbolic link")
    if source.resolve(strict=False) != source.absolute():
        raise RuntimeError(f"artifact {source} has a symbolic link in its path")
    target.parent.mkdir(parents=True, exist_ok=True)
    if source.is_dir():
        for root, directories, files in os.walk(source, followlinks=False):
            for name in (*directories, *files):
                entry = Path(root, name)
                if entry.is_symlink():
                    raise RuntimeError(f"artifact {source} contains symbolic link {entry}")
        shutil.copytree(source, target)
        exclude_artifact_paths(target, exclude)
    elif source.exists() or source.is_symlink():
        shutil.copy2(source, target, follow_symlinks=False)
