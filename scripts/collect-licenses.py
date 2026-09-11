#!/usr/bin/env python3
"""Copy the actual license texts supplied with locked Cargo dependencies."""

from __future__ import annotations

import argparse
import json
import re
import shutil
import subprocess
from pathlib import Path


def license_files(package: dict) -> list[Path]:
    root = Path(package["manifest_path"]).parent.resolve()
    files: set[Path] = set()
    declared = package.get("license_file")
    if declared:
        candidate = (root / declared).resolve()
        if not candidate.is_relative_to(root):
            raise ValueError(f"License path escapes package: {package['name']}")
        if candidate.is_file() and candidate.stat().st_size:
            files.add(candidate)
    for candidate in root.rglob("*"):
        if not candidate.is_file() or candidate.is_symlink():
            continue
        if candidate.stat().st_size and re.match(r"^(licen[cs]e|copying|copyright|notice|unlicense)([.\-_]|$)", candidate.name, re.I):
            files.add(candidate)
    return sorted(files)


def collect(output: Path, target: str | None) -> None:
    if target is None:
        version = subprocess.check_output(["rustc", "-vV"], text=True)
        target = next(line.removeprefix("host: ") for line in version.splitlines() if line.startswith("host: "))
    command = ["cargo", "metadata", "--locked", "--format-version", "1"]
    if target:
        command += ["--filter-platform", target]
    metadata = json.loads(subprocess.check_output(command, text=True))
    workspace = set(metadata["workspace_members"])
    packages = sorted(
        (p for p in metadata["packages"] if p["id"] not in workspace),
        key=lambda p: (p["name"], p["version"]),
    )
    if output.exists():
        raise ValueError(f"Output must not already exist: {output}")
    selections = []
    supplements = Path(__file__).resolve().parent.parent / "licenses"
    for package in packages:
        root = Path(package["manifest_path"]).parent.resolve()
        files = license_files(package)
        if not files:
            supplement = supplements / f"{package['name']}-{package['version']}"
            if supplement.is_dir():
                root = supplement
                files = license_files({"manifest_path": str(supplement / "Cargo.toml"), "name": package["name"]})
                if files and (supplement / "SOURCE").is_file():
                    files.append(supplement / "SOURCE")
        selections.append((package, files, root))
    missing = [f"{p['name']} {p['version']}" for p, files, _ in selections if not files]
    if missing:
        raise ValueError("No license text found for: " + ", ".join(missing))
    output.mkdir(parents=True)
    index = []
    for package, files, root in selections:
        dirname = f"{package['name']}-{package['version']}"
        copied = []
        for source in files:
            relative = source.relative_to(root)
            destination = output / dirname / relative
            destination.parent.mkdir(parents=True, exist_ok=True)
            shutil.copyfile(source, destination)
            copied.append(f"{dirname}/{relative.as_posix()}")
        index.append({
            "name": package["name"],
            "version": package["version"],
            "license": package.get("license"),
            "repository": package.get("repository"),
            "files": copied,
        })
    (output / "index.json").write_text(json.dumps(index, indent=2) + "\n", encoding="utf-8")
    print(f"Collected license texts for {len(index)} locked dependencies.")


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--output", type=Path, required=True)
    parser.add_argument("--target", help="Cargo target platform filter")
    args = parser.parse_args()
    try:
        collect(args.output, args.target)
    except (ValueError, subprocess.CalledProcessError) as error:
        parser.exit(1, f"{error}\n")


if __name__ == "__main__":
    main()
