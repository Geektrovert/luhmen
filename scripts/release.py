#!/usr/bin/env python3
"""Build a locked macOS arm64 release with deterministic archive metadata."""

from __future__ import annotations

import argparse
import gzip
import hashlib
import io
import json
import os
import shutil
import subprocess
import sys
import tarfile
import tempfile
import tomllib
from pathlib import Path

sys.dont_write_bytecode = True
from build_release import build

TARGET = "aarch64-apple-darwin"


def run(*args: str, **kwargs) -> str:
    return subprocess.check_output(args, text=True, **kwargs).strip()


def archive(destination: Path, root_name: str, files: list[tuple[str, Path]], epoch: int) -> None:
    with destination.open("wb") as raw:
        with gzip.GzipFile(filename="", mode="wb", fileobj=raw, mtime=epoch, compresslevel=9) as compressed:
            with tarfile.open(fileobj=compressed, mode="w", format=tarfile.PAX_FORMAT) as tar:
                for relative, path in sorted(files):
                    if path.is_symlink() or not path.is_file():
                        raise ValueError(f"Release input must be a regular file: {relative}")
                    content = path.read_bytes()
                    info = tarfile.TarInfo(f"{root_name}/{relative}")
                    info.size = len(content)
                    info.mode = 0o755 if path.stat().st_mode & 0o111 else 0o644
                    info.mtime = epoch
                    info.uid = info.gid = 0
                    info.uname = info.gname = "root"
                    tar.addfile(info, io.BytesIO(content))


def release(output: Path) -> None:
    repo = Path(__file__).resolve().parent.parent
    os.chdir(repo)
    if run("uname", "-s") != "Darwin" or run("uname", "-m") != "arm64":
        raise ValueError("Binary releases must be built on an Apple Silicon Mac.")
    if run("git", "status", "--porcelain", "--untracked-files=all"):
        raise ValueError("Release requires a clean, committed checkout.")
    manifest = tomllib.loads((repo / "Cargo.toml").read_text())
    version = manifest["package"]["version"]
    epoch = int(os.environ.get("SOURCE_DATE_EPOCH", run("git", "show", "-s", "--format=%ct", "HEAD")))
    if epoch < 0 or epoch > 0xFFFFFFFF:
        raise ValueError("SOURCE_DATE_EPOCH is outside the gzip timestamp range.")
    if output.exists():
        raise ValueError("Release output must not already exist.")
    tracked = subprocess.check_output(["git", "ls-files", "-z"]).decode().split("\0")
    source_files = [(name, repo / name) for name in tracked if name]
    with tempfile.TemporaryDirectory(prefix="luhmen-release-") as temporary:
        work = Path(temporary)
        os.environ["SOURCE_DATE_EPOCH"] = str(epoch)
        executable, environment = build(repo, work / "target", TARGET)
        package = work / "package"
        (package / "bin").mkdir(parents=True)
        shutil.copy2(executable, package / "bin" / "luhmen")
        notices = package / "share" / "licenses" / "luhmen"
        notices.mkdir(parents=True)
        for name in ["LICENSE", "NOTICE", "THIRD_PARTY.md"]:
            shutil.copyfile(repo / name, notices / name)
        subprocess.run([
            "python3", "scripts/collect-licenses.py", "--target", TARGET,
            "--output", str(notices / "dependencies"),
        ], env=environment, check=True)
        for name in ["README.md", "CONTRIBUTING.md", "LICENSE", "NOTICE", "THIRD_PARTY.md"]:
            shutil.copyfile(repo / name, package / name)
        shutil.copytree(repo / "docs", package / "docs")
        build_info = {
            "version": version,
            "commit": run("git", "rev-parse", "HEAD"),
            "source_date_epoch": epoch,
            "target": TARGET,
            "rustc": run("rustc", "--version"),
            "sdk_version": run("xcrun", "--sdk", "macosx", "--show-sdk-version"),
            "macos_deployment_target": environment["MACOSX_DEPLOYMENT_TARGET"],
        }
        (package / "build-info.json").write_text(json.dumps(build_info, indent=2) + "\n")
        output.mkdir(parents=True)
        binary_name = f"luhmen-{version}-{TARGET}"
        archive(output / f"{binary_name}.tar.gz", binary_name,
                [(p.relative_to(package).as_posix(), p) for p in package.rglob("*") if p.is_file()], epoch)
        source_name = f"luhmen-{version}-source"
        archive(output / f"{source_name}.tar.gz", source_name, source_files, epoch)
        sums = [f"{hashlib.sha256(p.read_bytes()).hexdigest()}  {p.name}" for p in sorted(output.glob("*.tar.gz"))]
        (output / "SHA256SUMS").write_text("\n".join(sums) + "\n", encoding="utf-8")
    print(f"Created release archives and checksums in {output}")


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--output", type=Path, required=True)
    args = parser.parse_args()
    try:
        release(args.output.resolve())
    except (ValueError, subprocess.CalledProcessError) as error:
        parser.exit(1, f"{error}\n")


if __name__ == "__main__":
    main()
