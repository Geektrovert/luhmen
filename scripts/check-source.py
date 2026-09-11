#!/usr/bin/env python3
"""Reject machine-specific home paths in product source."""

from __future__ import annotations

import re
import subprocess
from pathlib import Path


def main() -> None:
    repo = Path(__file__).resolve().parent.parent
    names = subprocess.check_output(
        ["git", "ls-files", "--cached", "--others", "--exclude-standard", "-z"], cwd=repo
    ).decode().split("\0")
    home_path = re.compile(r"/(?:Users|home)/[A-Za-z0-9_.-]+(?:/|\b)")
    failures = []
    for name in names:
        if not name:
            continue
        path = repo / name
        if not path.is_file() or path.is_symlink():
            continue
        try:
            content = path.read_text(encoding="utf-8")
        except UnicodeDecodeError:
            continue
        for line_number, line in enumerate(content.splitlines(), 1):
            if home_path.search(line):
                failures.append(f"{name}:{line_number}: machine-specific home path")
    if failures:
        raise SystemExit("\n".join(failures))
    print("Source path check passed.")


if __name__ == "__main__":
    main()
