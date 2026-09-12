"""Inspect actual archive members, independently of Cargo's include patterns."""

import importlib.util
import io
from pathlib import Path
import tarfile
import tempfile
import unittest


spec = importlib.util.spec_from_file_location("cargo_package", Path(__file__).with_name("check-cargo-package.py"))
cargo_package = importlib.util.module_from_spec(spec)
spec.loader.exec_module(cargo_package)


class CargoPackageTests(unittest.TestCase):
    def archive(self, directory, names):
        path = Path(directory) / "luhmen-0.1.0.crate"
        with tarfile.open(path, "w:gz") as archive:
            for name in names:
                member = tarfile.TarInfo("luhmen-0.1.0/" + name)
                member.size = 7
                archive.addfile(member, io.BytesIO(b"fixture"))
        return path

    def test_accepts_source_and_cargo_metadata(self):
        with tempfile.TemporaryDirectory() as directory:
            names = ["Cargo.toml", "Cargo.toml.orig", ".cargo_vcs_info.json", "LICENSE",
                     "src/main.rs", "docs/target.md", "scripts/check-cargo-package.py"]
            self.assertEqual(cargo_package.check_archive(self.archive(directory, names)), len(names))

    def test_rejects_nested_generated_files(self):
        for name in ["dist/release/README.md", "target/package/LICENSE",
                     "tests/__pycache__/check.cpython-311.pyc", "scripts/orphan.pyc",
                     "scripts/__pycache__/metadata.json"]:
            with self.subTest(name=name), tempfile.TemporaryDirectory() as directory:
                with self.assertRaises(ValueError) as raised:
                    cargo_package.check_archive(self.archive(directory, ["Cargo.toml", name]))
                self.assertIn(name, str(raised.exception))


if __name__ == "__main__":
    unittest.main()
