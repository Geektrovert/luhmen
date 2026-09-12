"""Permission boundaries for the opt-in VM measurement tool. No VM is needed."""

import argparse
import contextlib
import importlib.util
import io
import json
from pathlib import Path
import subprocess
import sys
import tempfile
import unittest
from unittest.mock import patch


SCRIPT = Path(__file__).with_name("vm-perf.py")
SPEC = importlib.util.spec_from_file_location("vm_perf", SCRIPT)
perf = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(perf)


class PermissionTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.args = argparse.Namespace(fixture_root=Path(self.temp.name), luhmen="luhmen", pull=False)
        self.suite = perf.Measurements(self.args)
        self.suite.endpoint = "unix:///owned/lima/luhmen/sock/docker.sock"
        self.context = {"Name": "luhmen", "Endpoints": {"docker": {"Host": self.suite.endpoint}},
                        "Metadata": {"Description": perf.DESCRIPTION}}

    def test_no_run_flag_never_invokes_dependencies(self):
        result = subprocess.run([sys.executable, str(SCRIPT), "--fixture-root", self.temp.name,
                                 "--luhmen", "/missing/dependency"], capture_output=True, text=True)
        self.assertEqual(result.returncode, 2)
        self.assertIn("No actions taken", result.stderr)
        self.assertEqual(list(Path(self.temp.name).iterdir()), [])

    def test_endpoint_change_blocks_docker_mutation(self):
        self.context["Endpoints"]["docker"]["Host"] = "unix:///unrelated.sock"
        response = subprocess.CompletedProcess([], 0, json.dumps([self.context]), "")
        with patch.object(perf, "command", return_value=response) as command:
            with self.assertRaisesRegex(RuntimeError, "endpoint changed"):
                self.suite.docker("run", perf.IMAGE, "true")
        self.assertFalse(any("run" in call.args[0] for call in command.call_args_list))

    def test_existing_workload_blocks_preflight_before_fixture_creation(self):
        state = {"schema_version": 1, "name": "luhmen", "context": "luhmen", "state": "Running",
                 "context_ready": True, "engine_ready": True, "endpoint": self.suite.endpoint,
                 "vm": {"name": "luhmen", "vmType": "vz", "arch": "aarch64", "dir": "/owned/lima/luhmen"},
                 "config": {"mounts": [{"writable": True, "path": self.temp.name}]}}
        def command(args, **_):
            if args[1:] == ["inspect", "--json"]:
                output = json.dumps(state)
            elif args[1:] == ["context", "inspect", "luhmen"]:
                output = json.dumps([self.context])
            elif args[1:] == ["--context", "luhmen", "ps", "--all", "--quiet"]:
                output = "existing-container\n"
            else:
                self.fail(f"Unexpected command: {args}")
            return subprocess.CompletedProcess(args, 0, output, "")

        with patch.object(perf, "command", side_effect=command):
            with self.assertRaisesRegex(RuntimeError, "no existing containers"):
                self.suite.preflight()
        self.assertFalse(self.suite.fixture.exists())

    def test_cleanup_preserves_changed_owner_and_removes_only_owned_id(self):
        self.suite.containers = ["changed", "owned"]
        removed = []

        def command(args, **_):
            if args[1:] == ["context", "inspect", "luhmen"]:
                output = json.dumps([self.context])
            else:
                self.assertEqual(args[1:4], ["--context", "luhmen", "container"])
                action = args[4]
                if action == "ls":
                    output = "changed\nowned\nunrelated\n"
                elif action == "inspect":
                    name = args[5]
                    label = self.suite.run_id if name == "owned" else "another-run"
                    output = json.dumps([{"Id": "id-" + name,
                                          "Config": {"Labels": {perf.LABEL: label}}}])
                elif action == "rm":
                    removed.append(args[-1])
                    output = ""
                else:
                    self.fail(f"Unexpected command: {args}")
            return subprocess.CompletedProcess(args, 0, output, "")

        with patch.object(perf, "command", side_effect=command), contextlib.redirect_stdout(io.StringIO()):
            with self.assertRaisesRegex(RuntimeError, "ownership label"):
                self.suite.cleanup()
        self.assertEqual(removed, ["id-owned"])


if __name__ == "__main__":
    unittest.main()
