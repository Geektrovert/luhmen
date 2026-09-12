#!/usr/bin/env python3
"""Exercise the guest manager protocol and lifecycle with a disposable OS model.

Absolute guest paths are rebased into a temporary directory. The helper itself
handles requests, state, admission, API calls and cleanup; a fake jailer models
asynchronous cgroup entry and a VMM that writes its supplied disk. These tests do
not establish KVM, jailer confinement, or guest-kernel behavior.
"""
import json
import os
from pathlib import Path
import signal
import subprocess
import tempfile
import textwrap
import unittest

HELPER = Path(os.environ.get("LUHMEN_TEST_HELPER", Path(__file__).with_name("microvmd.sh")))

JAILER = r'''#!/usr/bin/env python3
import json, os, pathlib, resource, signal, socket, sys, time
root = pathlib.Path(os.environ["MODEL_ROOT"])
args = sys.argv[1:]
def argument(name):
    return args[args.index(name) + 1]
for i, arg in enumerate(args):
    if arg == "--resource-limit" and args[i + 1].startswith("fsize="):
        limit = int(args[i + 1].split("=", 1)[1])
        resource.setrlimit(resource.RLIMIT_FSIZE, (limit, limit))
vm_id = argument("--id")
parent = argument("--parent-cgroup")
if parent.startswith("/"):
    sys.exit("jailer requires a relative parent cgroup")
jail = pathlib.Path(argument("--chroot-base-dir")) / "firecracker" / vm_id / "root"
pid = os.getpid()
proc = root / "proc" / str(pid)
proc.mkdir(exist_ok=True)
(proc / "stat").write_text(" ".join([str(pid), "(jailer)", "S"] + ["0"] * 18 + ["1234"]))
(proc / "cmdline").write_bytes(b"jailer\0")
def cleanup(*_):
    (group / "cgroup.procs").write_text("")
    import shutil
    shutil.rmtree(proc, ignore_errors=True)
    sys.exit(0)
time.sleep(0.15)
group = root / "sys/fs/cgroup" / parent / vm_id
group.mkdir(parents=True, exist_ok=True)
(group / "cgroup.procs").write_text(str(pid) + "\n")
for i, arg in enumerate(args):
    if arg == "--cgroup":
        key, value = args[i + 1].split("=", 1)
        (group / key).write_text(value)
(proc / "cgroup").write_text("0::/" + parent + "/" + vm_id + "\n")
for sig in (signal.SIGTERM, signal.SIGINT):
    signal.signal(sig, cleanup)
jail.mkdir(parents=True, exist_ok=True)
(proc / "cmdline").write_bytes(b"firecracker\0--api-sock\0/run/fc.sock\0")
sock = socket.socket(socket.AF_UNIX)
sock.bind(str(jail / "run/fc.sock"))
sock.listen()
while True:
    conn, _ = sock.accept()
    request = b""
    while b"\r\n\r\n" not in request:
        request += conn.recv(4096)
    header, body = request.split(b"\r\n\r\n", 1)
    length = next((int(line.split(b":", 1)[1]) for line in header.split(b"\r\n") if line.lower().startswith(b"content-length:")), 0)
    while len(body) < length:
        body += conn.recv(4096)
    data = json.loads(body)
    status = b"204 No Content"
    if data.get("action_type") == "InstanceStart":
        with (jail / "rootfs.ext4").open("r+b") as disk:
            disk.seek(int(os.environ.get("MODEL_WRITE_OFFSET", "0")), 0 if "MODEL_WRITE_OFFSET" in os.environ else 2)
            disk.write(b"guest-write\n")
    if os.environ.get("MODEL_FAIL_API") == "1":
        status = b"500 Internal Server Error"
    conn.sendall(b"HTTP/1.1 " + status + b"\r\nContent-Length: 0\r\nConnection: close\r\n\r\n")
    conn.close()
'''


class ManagerFixture:
    def __init__(self):
        self.directory = tempfile.TemporaryDirectory(prefix="mv-", dir="/tmp")
        self.root = Path(self.directory.name).resolve()
        self.tools = self.root / "tools"
        self.tools.mkdir()
        for directory in ("proc/self", "sys/fs/cgroup/system.slice/luhmen-microvmd.service", "run/luhmen", "usr/local/bin", "dev", "inputs"):
            (self.root / directory).mkdir(parents=True, exist_ok=True)
        (self.root / "proc/self/cgroup").write_text("0::/system.slice/luhmen-microvmd.service/main\n")
        (self.root / "proc/meminfo").write_text("MemTotal: 2097152 kB\n")
        (self.root / "sys/fs/cgroup/cgroup.controllers").write_text("cpu memory pids\n")
        (self.root / "sys/fs/cgroup/system.slice/luhmen-microvmd.service/cgroup.subtree_control").write_text("cpu memory pids\n")
        (self.root / "dev/kvm").touch()
        (self.root / "inputs/kernel").write_bytes(b"kernel")
        (self.root / "inputs/rootfs").write_bytes(b"original\n")
        self.executable(self.root / "usr/local/bin/jailer", JAILER)
        self.executable(self.root / "usr/local/bin/firecracker", "#!/bin/sh\nexit 1\n")
        self.executable(self.tools / "awk", """#!/usr/bin/env python3
import os, pathlib, sys
path = pathlib.Path(sys.argv[-1])
if path.name == 'stat' and path.parent.name.isdecimal() and not path.exists():
    path.parent.mkdir(exist_ok=True)
    path.write_text(' '.join([path.parent.name, '(jailer)', 'S'] + ['0'] * 18 + ['1234']))
    (path.parent / 'cmdline').write_bytes(b'jailer')
os.execv('/usr/bin/awk', ['awk'] + sys.argv[1:])
""")
        self.executable(self.tools / "rmdir", """#!/usr/bin/env python3
import pathlib, shutil, sys
path = pathlib.Path(sys.argv[-1])
if (path / 'cgroup.procs').exists():
    if (path / 'cgroup.procs').read_text().strip():
        sys.exit(1)
    shutil.rmtree(path)
else:
    path.rmdir()
""")
        self.executable(self.tools / "nproc", "#!/bin/sh\necho 4\n")
        self.executable(self.tools / "chown", "#!/bin/sh\nexit 0\n")
        self.executable(self.tools / "stdbuf", '#!/bin/sh\nshift\nexec "$@"\n')
        self.executable(self.tools / "getent", "#!/bin/sh\nexit 2\n")
        self.executable(self.tools / "cp", '#!/bin/sh\n[ "$1" != --reflink=auto ] || shift\nexec /bin/cp "$@"\n')
        # POSIX locks stay owned by the parent shell's open file description.
        self.executable(self.tools / "flock", "#!/usr/bin/env python3\nimport fcntl,sys\nfcntl.flock(int(sys.argv[-1]),fcntl.LOCK_EX)\n")
        self.env = dict(os.environ, PATH=str(self.tools) + os.pathsep + os.environ["PATH"], MODEL_ROOT=str(self.root))
        self.helper = self.root / "helper.sh"
        source = HELPER.read_text()
        for prefix in ("/var/lib/luhmen", "/usr/local/bin", "/run/luhmen", "/sys/fs/cgroup", "/proc/", "/dev/kvm", "/dev/net/tun"):
            source = source.replace(prefix, str(self.root) + prefix)
        self.helper.write_text(source)

    @staticmethod
    def executable(path, content):
        path.write_text(textwrap.dedent(content))
        path.chmod(0o755)

    def request(self, command):
        result = subprocess.run(["/bin/sh", str(self.helper)], input=command + "\n", capture_output=True, text=True, env=self.env, timeout=15)
        if result.returncode:
            raise AssertionError(f"helper exited {result.returncode}: {result.stderr}; {result.stdout}")
        lines = result.stdout.splitlines()
        if len(lines) != 1:
            raise AssertionError(f"expected one response, received {result.stdout!r}; {result.stderr}")
        return json.loads(lines[0])

    def data(self, command):
        response = self.request(command)
        if not isinstance(response, dict) or response.get("version") != 1 or response.get("ok") is not True:
            raise AssertionError(f"expected successful versioned response: {response}")
        return response["data"]

    def create(self, vm_id="demo", memory=512):
        return self.data(f"create id={vm_id} kernel={self.root}/inputs/kernel rootfs={self.root}/inputs/rootfs vcpus=1 memory_mib={memory}")

    def close(self):
        for proc in (self.root / "proc").iterdir():
            if proc.name.isdecimal():
                # A failed model process may have exited before removing its
                # fake /proc entry. Do not signal a subsequently reused PID.
                command = subprocess.run(["ps", "-p", proc.name, "-o", "command="], capture_output=True, text=True).stdout
                if str(self.root / "usr/local/bin/jailer") in command:
                    try:
                        os.kill(int(proc.name), signal.SIGKILL)
                    except ProcessLookupError:
                        pass
        self.directory.cleanup()


class ManagerTests(unittest.TestCase):
    def setUp(self):
        self.fixture = ManagerFixture()
        self.addCleanup(self.fixture.close)

    def test_empty_inspect_is_a_versioned_array(self):
        self.assertEqual(self.fixture.data("inspect"), [])

    def test_create_inspect_and_stop_share_the_client_protocol(self):
        self.assertEqual(self.fixture.create()["status"], "created")
        self.assertEqual(self.fixture.data("inspect id=demo")["status"], "created")
        self.assertEqual(self.fixture.data("stop id=demo")["status"], "stopped")

    def test_ambiguous_requests_cannot_operate_another_vm(self):
        self.fixture.create("bar")
        for request in ("stop id=foo id=bar force=0", "inspect id=bar stop",
                        "inspect id=bar kernel=/tmp/unexpected"):
            with self.subTest(request=request):
                response = self.fixture.request(request)
                self.assertFalse(response["ok"], response)
                self.assertEqual(self.fixture.data("inspect id=bar")["status"], "created")

    def test_delegated_systemd_cgroup_is_accepted(self):
        capabilities = self.fixture.data("capabilities")
        self.assertTrue(capabilities["delegated_cgroup"], capabilities)
        self.assertTrue(capabilities["ready"], capabilities)

    def test_start_survives_response_and_retains_disk_writes_across_stop(self):
        self.fixture.create()
        first = self.fixture.data("start id=demo")
        self.assertEqual(first["status"], "running")
        os.kill(first["pid"], 0)
        self.assertEqual(self.fixture.data("start id=demo")["pid"], first["pid"])
        self.assertEqual(self.fixture.data("inspect id=demo")["status"], "running")
        self.fixture.data("stop id=demo")
        disk = self.fixture.root / "var/lib/luhmen/microvms/demo/rootfs.ext4"
        self.assertEqual(disk.read_bytes(), b"original\nguest-write\n")
        self.fixture.data("start id=demo")
        self.fixture.data("stop id=demo")
        self.assertEqual(disk.read_bytes(), b"original\nguest-write\nguest-write\n")
        self.assertEqual((self.fixture.root / "inputs/rootfs").read_bytes(), b"original\n")

    def test_admission_counts_each_vmm_memory_allowance(self):
        self.fixture.create(memory=1024)
        self.fixture.data("start id=demo")
        self.fixture.create("second", memory=384)
        response = self.fixture.request("start id=second")
        self.assertFalse(response["ok"], response)
        self.assertIn("memory", response["error"])

    def test_guest_disk_writes_are_not_capped_by_the_log_limit(self):
        self.fixture.create()
        self.fixture.env["MODEL_WRITE_OFFSET"] = str(70 * 1024 * 1024)
        self.fixture.data("start id=demo")
        self.fixture.data("stop id=demo")
        with (self.fixture.root / "var/lib/luhmen/microvms/demo/rootfs.ext4").open("rb") as disk:
            disk.seek(70 * 1024 * 1024)
            self.assertEqual(disk.read(), b"guest-write\n")

    def test_failed_api_request_returns_error_and_cleans_up(self):
        self.fixture.create()
        self.fixture.env["MODEL_FAIL_API"] = "1"
        response = self.fixture.request("start id=demo")
        self.assertFalse(response["ok"], response)
        self.assertIn("configuration", response["error"])
        self.assertEqual(self.fixture.data("inspect id=demo")["status"], "created")
        self.assertFalse((self.fixture.root / "var/lib/luhmen/microvm-jails/firecracker/demo").exists())


if __name__ == "__main__":
    unittest.main()
