"""Release artifact, source installation, and dependency-notice contract checks."""

from __future__ import annotations

import importlib.util
import os
import shutil
import subprocess
import sys
import tarfile
import tempfile
import unittest
from pathlib import Path
from unittest.mock import patch

sys.dont_write_bytecode = True
import release

spec = importlib.util.spec_from_file_location("collect_licenses", Path(__file__).with_name("collect-licenses.py"))
assert spec and spec.loader
collect_licenses = importlib.util.module_from_spec(spec)
spec.loader.exec_module(collect_licenses)


class DistributionTests(unittest.TestCase):
    def test_source_install_uses_host_target_despite_cargo_defaults(self) -> None:
        with tempfile.TemporaryDirectory(prefix="luhmen-install-test-") as directory:
            root = Path(directory)
            repo = root / "source with spaces"
            (repo / "src").mkdir(parents=True)
            (repo / ".cargo").mkdir()
            (repo / "scripts").mkdir()
            (repo / "Cargo.toml").write_text(
                '[package]\nname = "luhmen"\nversion = "0.0.0"\nedition = "2024"\n'
            )
            (repo / "Cargo.lock").write_text(
                'version = 4\n\n[[package]]\nname = "luhmen"\nversion = "0.0.0"\n'
            )
            (repo / "src" / "main.rs").write_text('fn main() { println!("installed host executable"); }\n')
            (repo / ".cargo" / "config.toml").write_text('[build]\ntarget = "invalid-config-target"\n')
            for name in ["install.sh", "build_release.py", "collect-licenses.py"]:
                shutil.copy2(Path(__file__).with_name(name), repo / "scripts" / name)
            shutil.copy2(Path(__file__).resolve().parent.parent / "rust-toolchain.toml", repo / "rust-toolchain.toml")
            for name in ["LICENSE", "NOTICE", "THIRD_PARTY.md"]:
                (repo / name).write_text(f"Fixture {name}\n")
            target = root / "build with spaces"
            (target / "release").mkdir(parents=True)
            (target / "release" / "luhmen").write_text("stale executable must not be installed\n")
            prefix = root / "install with spaces"
            environment = dict(os.environ, CARGO_BUILD_TARGET="invalid-env-target", CARGO_TARGET_DIR=str(target))
            result = subprocess.run(
                [str(repo / "scripts" / "install.sh"), "--prefix", str(prefix)],
                cwd=repo, env=environment, text=True, capture_output=True,
            )
            self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
            self.assertEqual(
                subprocess.check_output([str(prefix / "bin" / "luhmen")], text=True),
                "installed host executable\n",
            )
            self.assertTrue((prefix / "share" / "licenses" / "luhmen" / "dependencies" / "index.json").is_file())
            self.assertFalse((repo / ".git").exists())

    def test_archive_is_reproducible_and_has_normalized_metadata(self) -> None:
        with tempfile.TemporaryDirectory(prefix="luhmen-archive-test-") as directory:
            root = Path(directory)
            data = root / "data"
            data.write_text("payload\n")
            executable = root / "executable"
            executable.write_text("#!/bin/sh\nexit 0\n")
            executable.chmod(0o755)
            files = [("data", data), ("bin/luhmen", executable)]
            first, second = root / "first.tar.gz", root / "second.tar.gz"
            release.archive(first, "luhmen-test", files, 1_000_000_000)
            os.utime(data, (1_600_000_000, 1_600_000_000))
            release.archive(second, "luhmen-test", list(reversed(files)), 1_000_000_000)
            self.assertEqual(first.read_bytes(), second.read_bytes())
            with tarfile.open(first) as archive:
                entries = archive.getmembers()
                self.assertEqual([entry.name for entry in entries], ["luhmen-test/bin/luhmen", "luhmen-test/data"])
                self.assertEqual([entry.mode for entry in entries], [0o755, 0o644])
                for entry in entries:
                    self.assertEqual((entry.uid, entry.gid, entry.mtime), (0, 0, 1_000_000_000))

    def test_archive_refuses_symlinks(self) -> None:
        with tempfile.TemporaryDirectory(prefix="luhmen-archive-test-") as directory:
            root = Path(directory)
            (root / "file").write_text("payload")
            (root / "link").symlink_to(root / "file")
            with self.assertRaises(ValueError):
                release.archive(root / "archive.tar.gz", "luhmen-test", [("link", root / "link")], 1)

    def test_dependency_without_license_text_blocks_collection(self) -> None:
        import json
        with tempfile.TemporaryDirectory(prefix="luhmen-notice-test-") as directory:
            root = Path(directory)
            manifest = root / "Cargo.toml"
            manifest.write_text("")
            metadata = {"workspace_members": [], "packages": [{
                "id": "example@1.0.0", "name": "example", "version": "1.0.0",
                "manifest_path": str(manifest), "license": "MIT",
            }]}
            with patch.object(collect_licenses.subprocess, "check_output", return_value=json.dumps(metadata)):
                with self.assertRaisesRegex(ValueError, "No license text found"):
                    collect_licenses.collect(root / "notices", "aarch64-apple-darwin")
            self.assertFalse((root / "notices").exists())

    def test_declared_license_cannot_escape_dependency_directory(self) -> None:
        with tempfile.TemporaryDirectory(prefix="luhmen-notice-test-") as directory:
            root = Path(directory)
            package = {"manifest_path": str(root / "Cargo.toml"), "name": "example", "license_file": "../LICENSE"}
            with self.assertRaisesRegex(ValueError, "escapes package"):
                collect_licenses.license_files(package)


if __name__ == "__main__":
    unittest.main()
