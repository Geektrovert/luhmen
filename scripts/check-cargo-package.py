#!/usr/bin/env python3
"""Reject build output and Python bytecode in generated Cargo source archives."""

import argparse
from pathlib import Path, PurePosixPath
import tarfile


def check_archive(path: Path) -> int:
    with tarfile.open(path, "r:gz") as archive:
        names = [member.name for member in archive]
    forbidden = [name for name in names
                 if {"dist", "target", "__pycache__"}.intersection(PurePosixPath(name).parts)
                 or name.endswith(".pyc")]
    if forbidden:
        raise ValueError(f"Generated files in {path}:\n" + "\n".join(sorted(forbidden)))
    if not names:
        raise ValueError(f"Empty Cargo archive: {path}")
    return len(names)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("archives", nargs="+", type=Path, help="Generated .crate files to inspect")
    args = parser.parse_args()
    for path in args.archives:
        try:
            entries = check_archive(path)
        except (OSError, ValueError, tarfile.TarError) as error:
            parser.exit(1, str(error) + "\n")
        print(f"Cargo archive inventory passed: {path} ({entries} entries).")


if __name__ == "__main__":
    main()
