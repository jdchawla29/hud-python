from pathlib import Path

from hud.integrations.harbor.artifacts import copy_artifact


def test_copy_artifact_excludes_matching_files_and_directories(tmp_path: Path) -> None:
    source = tmp_path / "source"
    (source / "cache").mkdir(parents=True)
    (source / "cache" / "model.bin").write_bytes(b"cache")
    (source / "results").mkdir()
    (source / "results" / "model.pt").write_bytes(b"weights")
    (source / "results" / "summary.txt").write_text("ok", encoding="utf-8")
    target = tmp_path / "target"

    copy_artifact(source, target, ["cache", "*.pt"])

    assert [path.relative_to(target) for path in target.rglob("*")] == [
        Path("results"),
        Path("results/summary.txt"),
    ]
