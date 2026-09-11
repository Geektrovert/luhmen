#!/usr/bin/env python3
"""Build the release executable with local source paths remapped."""

from __future__ import annotations

import argparse
import os
import subprocess
from pathlib import Path


def build(repo: Path, target_dir: Path, target: str | None = None) -> tuple[Path, dict[str, str]]:
    repo = repo.resolve()
    target_dir = target_dir.resolve()
    cargo_home = Path(os.environ.get("CARGO_HOME", str(Path.home() / ".cargo"))).resolve()
    sysroot = Path(subprocess.check_output(["rustc", "--print", "sysroot"], text=True, cwd=repo).strip())
    mappings = [
        (Path.home(), "/usr/local/home"),
        (target_dir, "/tmp/luhmen-build"),
        (cargo_home, "/usr/local/cargo"),
        (sysroot, "/usr/local/rust"),
        (repo, "/usr/src/luhmen"),
    ]
    environment = dict(os.environ)
    environment.update({
        "MACOSX_DEPLOYMENT_TARGET": "14.0",
        "CARGO_TARGET_DIR": str(target_dir),
        "CARGO_INCREMENTAL": "0",
        "CARGO_PROFILE_RELEASE_STRIP": "symbols",
        "CARGO_PROFILE_RELEASE_DEBUG": "0",
        "CARGO_ENCODED_RUSTFLAGS": "\x1f".join(
            f"--remap-path-prefix={source}={destination}" for source, destination in mappings
        ),
    })
    environment.pop("RUSTFLAGS", None)
    command = ["cargo", "build", "--release", "--locked"]
    if target:
        command += ["--target", target]
    subprocess.run(command, cwd=repo, env=environment, check=True)
    executable = target_dir / target / "release" / "luhmen" if target else target_dir / "release" / "luhmen"
    contents = executable.read_bytes()
    if any(str(source).encode() in contents for source, _ in mappings):
        raise ValueError("Binary contains an unremapped local source path.")
    return executable, environment


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--target")
    args = parser.parse_args()
    repo = Path(__file__).resolve().parent.parent
    target_dir = Path(os.environ.get("CARGO_TARGET_DIR", str(repo / "target")))
    if not target_dir.is_absolute():
        target_dir = repo / target_dir
    try:
        executable, _ = build(repo, target_dir, args.target)
    except (ValueError, subprocess.CalledProcessError) as error:
        parser.exit(1, f"{error}\n")
    print(f"Built {executable.name} with local source paths remapped.")


if __name__ == "__main__":
    main()
