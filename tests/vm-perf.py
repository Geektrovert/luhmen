#!/usr/bin/env python3
"""Opt-in measurements on an existing, otherwise idle ARM64 luhmen VM.

Requires Python 3.10+ and Docker. Pass --run and a writable shared --fixture-root.
The pinned image must be cached, or pass --pull. JSON Lines contain raw samples
and environment details. No VM restart, global Docker changes, or pruning occurs.
Only this run's labeled containers and volume are removed; host fixtures remain.
These synthetic workloads are not application-build or native-parity benchmarks.
"""

import argparse
import csv
import hashlib
import http.client
import io
import json
import os
from pathlib import Path
import platform
import shutil
import signal
import subprocess
import sys
import time
import uuid


IMAGE = "busybox:1.37.0@sha256:f10e809bcf667d8e9f01d2baf82869049a495cd448cdfe1f4dee94078b960ae9"
LABEL = "io.luhmen.perf.run"
DESCRIPTION = "luhmen managed Docker Engine (schema 1)"


def emit(check, **details):
    print(json.dumps({"check": check, **details}), flush=True)


def require(condition, message):
    if not condition:
        raise RuntimeError(message)


def command(args, env=None, timeout=60, check=True):
    args = [str(arg) for arg in args]
    with subprocess.Popen(args, stdout=subprocess.PIPE, stderr=subprocess.PIPE,
                          text=True, env=env, start_new_session=True) as process:
        try:
            stdout, stderr = process.communicate(timeout=timeout)
        except (subprocess.TimeoutExpired, KeyboardInterrupt):
            try:
                os.killpg(process.pid, signal.SIGKILL)
            except ProcessLookupError:
                pass
            process.communicate(timeout=5)
            raise
    result = subprocess.CompletedProcess(args, process.returncode, stdout, stderr)
    if check and result.returncode:
        raise RuntimeError(f"Command failed ({result.returncode}): {args!r}\n"
                           + stderr[-2000:] + stdout[-1000:])
    return result


def mount_spec(*fields):
    # Docker parses --mount as CSV. Quote paths with commas or spaces correctly.
    output = io.StringIO()
    csv.writer(output).writerow(fields)
    return output.getvalue().removesuffix("\r\n")


def cpu_seconds(value):
    days, separator, clock = value.partition("-")
    total = int(days) * 86400 if separator else 0
    fields = (clock if separator else value).split(":")
    for index, field in enumerate(reversed(fields)):
        total += float(field) * 60 ** index
    return total


class Measurements:
    def __init__(self, args):
        self.args = args
        self.env = dict(os.environ)
        for key in ("DOCKER_HOST", "DOCKER_CONTEXT", "DOCKER_TLS_VERIFY", "DOCKER_CERT_PATH",
                    "BUILDX_BUILDER", "BUILDKIT_HOST", "DOCKER_DEFAULT_PLATFORM"):
            self.env.pop(key, None)
        self.docker_binary = os.environ.get("LUHMEN_DOCKER", "docker")
        self.run_id = "luhmen-perf-" + uuid.uuid4().hex[:12]
        self.fixture = args.fixture_root.resolve(strict=True) / self.run_id
        self.endpoint = None
        self.containers = []
        self.volume_attempted = False
        self.volume = self.run_id + "-data"

    def verify_context(self):
        contexts = json.loads(command([self.docker_binary, "context", "inspect", "luhmen"],
                                      env=self.env, timeout=20).stdout)
        require(len(contexts) == 1, "Expected one luhmen Docker context")
        context = contexts[0]
        require(context.get("Name") == "luhmen"
                and context.get("Endpoints", {}).get("docker", {}).get("Host") == self.endpoint
                and context.get("Metadata", {}).get("Description") == DESCRIPTION,
                "Docker context ownership or endpoint changed; refusing operation")

    def docker(self, *args, timeout=60, check=True, measured=None, via_wrapper=False):
        self.verify_context()
        invocation = ([self.args.luhmen, "docker"] if via_wrapper else
                      [self.docker_binary, "--context", "luhmen"])
        started = time.perf_counter()
        result = command([*invocation, *args], env=self.env, timeout=timeout, check=check)
        elapsed = time.perf_counter() - started
        if measured is not None:
            emit("sample", seconds=elapsed, **measured)
        return result

    def guest_memory(self):
        output = command([self.args.luhmen, "shell", "cat", "/proc/meminfo"],
                         env=self.env, timeout=20).stdout
        fields = {}
        for line in output.splitlines():
            name, _, value = line.partition(":")
            if name in {"MemTotal", "MemFree", "MemAvailable", "Buffers", "Cached",
                        "SwapTotal", "SwapFree", "Dirty", "Writeback"}:
                fields[name + "_kib"] = int(value.split()[0])
        require("MemAvailable_kib" in fields, "Guest memory report is incomplete")
        return fields

    def preflight(self):
        state = json.loads(command([self.args.luhmen, "inspect", "--json"],
                                   env=self.env, timeout=30).stdout)
        require(state.get("schema_version") == 1 and state.get("name") == "luhmen"
                and state.get("context") == "luhmen" and state.get("state") == "Running"
                and state.get("context_ready") and state.get("engine_ready"),
                "Start a healthy, owned luhmen VM before measuring")
        self.vm = state.get("vm") or {}
        self.endpoint = state.get("endpoint")
        require(self.vm.get("name") == "luhmen" and self.vm.get("vmType") == "vz"
                and self.vm.get("arch") == "aarch64"
                and self.endpoint == "unix://" + str(Path(self.vm["dir"]) / "sock/docker.sock"),
                "VM identity and socket endpoint do not agree")
        root = self.fixture.parent
        require(root.is_dir() and any(mount.get("writable") and root.is_relative_to(
            Path(mount["path"]).resolve()) for mount in state["config"].get("mounts", [])),
            "--fixture-root must be inside a configured writable share")
        require(not self.docker("ps", "--all", "--quiet").stdout.strip(),
                "Measurements require a VM with no existing containers")
        image = self.docker("image", "inspect", IMAGE, check=False)
        if image.returncode:
            require(self.args.pull, "Pinned BusyBox image is missing; pass --pull to download it")
            self.docker("pull", "--platform=linux/arm64", IMAGE, timeout=180)
            image = self.docker("image", "inspect", IMAGE)
        require(json.loads(image.stdout)[0].get("Architecture") == "arm64",
                "The cached measurement image is not arm64")
        self.fixture.mkdir(mode=0o700)
        (self.fixture / ".luhmen-perf.json").write_text(json.dumps({"run_id": self.run_id}) + "\n")
        self.host_pids = {self.vm[key] for key in ("hostAgentPID", "driverPID")
                          if isinstance(self.vm.get(key), int) and self.vm[key] > 0}
        require(self.host_pids, "VM host process identifiers are unavailable")
        host = {"system": platform.system(), "release": platform.release(),
                "macos": platform.mac_ver()[0], "machine": platform.machine(),
                "logical_cpus": os.cpu_count(), "python": platform.python_version()}
        if platform.system() == "Darwin":
            for name, key in (("chip", "machdep.cpu.brand_string"), ("memory_bytes", "hw.memsize")):
                host[name] = command(["sysctl", "-n", key], timeout=10).stdout.strip()
        binary = shutil.which(self.args.luhmen, path=self.env.get("PATH"))
        require(binary, "Cannot resolve the measured luhmen executable")
        emit("environment", run_id=self.run_id, endpoint=self.endpoint, fixture=str(self.fixture),
             measured_at_unix=time.time(), host=host,
             config=state["config"], lima=self.vm.get("limaVersion"), image=IMAGE,
             luhmen=command([self.args.luhmen, "--version"], env=self.env).stdout.strip(),
             luhmen_entrypoint_sha256=hashlib.sha256(Path(binary).read_bytes()).hexdigest(),
             measurement_script_sha256=hashlib.sha256(Path(__file__).read_bytes()).hexdigest(),
             docker=json.loads(self.docker("version", "--format", "{{json .}}").stdout),
             iterations=self.args.iterations, files=self.args.files, blob_mib=self.args.blob_mib,
             timing="Host wall time includes Docker CLI/exec overhead. The tool's context checks are excluded; "
                    "wrapper samples include Luhmen's own ownership/context checks. No cache flush. "
                    "Startup and storage samples follow warm-ups and alternate invocation/placement order.",
             limitations="Synthetic packaging, not application compilation. No native or OrbStack baseline. "
                         "Host process RSS can count shared pages more than once; it is not physical VM RAM.")

    def assert_idle(self):
        for identifier in self.docker("ps", "--all", "--quiet").stdout.split():
            data = json.loads(self.docker("container", "inspect", identifier).stdout)[0]
            require(data.get("Config", {}).get("Labels", {}).get(LABEL) == self.run_id,
                    "An unrelated container appeared; stopping measurements")

    def host_processes(self):
        output = command(["ps", "-axo", "pid=,ppid=,time=,rss=,comm="], timeout=10).stdout
        rows = {}
        for line in output.splitlines():
            fields = line.split(None, 4)
            if len(fields) == 5:
                pid, parent, cpu, rss, name = fields
                rows[int(pid)] = {"pid": int(pid), "ppid": int(parent),
                                  "cpu_seconds": cpu_seconds(cpu), "rss_kib": int(rss), "command": name}
        require(self.host_pids.issubset(rows), "A VM host process exited during measurement")
        selected = set(self.host_pids)
        while True:
            expanded = selected | {pid for pid, row in rows.items() if row["ppid"] in selected}
            if expanded == selected:
                return [rows[pid] for pid in sorted(selected)]
            selected = expanded

    def idle_resources(self):
        memory_before = self.guest_memory()
        before = self.host_processes()
        started = time.perf_counter()
        time.sleep(self.args.idle_seconds)
        after = self.host_processes()
        elapsed = time.perf_counter() - started
        prior = {row["pid"]: row for row in before}
        common = [row for row in after if row["pid"] in prior
                  and row["command"] == prior[row["pid"]]["command"]]
        cpu_delta = sum(max(0, row["cpu_seconds"] - prior[row["pid"]]["cpu_seconds"])
                        for row in common)
        emit("idle_resources", interval_seconds=elapsed, host_processes_before=before,
             host_processes_after=after, surviving_process_cpu_seconds=cpu_delta,
             surviving_process_cpu_percent=cpu_delta * 100 / elapsed,
             guest_memory_before=memory_before, guest_memory_after=self.guest_memory(),
             note="CPU percent uses 100% for one core and excludes processes that exited. "
                  "Only the VM driver/host agent and their descendants are sampled.")

    def container(self, suffix, *args, measured=None, via_wrapper=False):
        name = self.run_id + "-" + suffix
        self.containers.append(name)  # Track before starting, including ambiguous failures.
        return self.docker("run", "--name", name, "--label", LABEL + "=" + self.run_id,
                           "--platform=linux/arm64", *args, measured=measured, via_wrapper=via_wrapper)

    def startup(self):
        for iteration in range(self.args.iterations + 1):
            order = ("direct", "wrapper") if iteration % 2 else ("wrapper", "direct")
            for invocation in order:
                self.assert_idle()
                self.container(f"start-{invocation}-{iteration}", "--rm", "--network=none", IMAGE, "true",
                               via_wrapper=invocation == "wrapper", measured=None if iteration == 0 else {
                                   "workload": "warm_container_run_to_exit", "invocation": invocation,
                                   "iteration": iteration})

    def storage(self):
        self.volume_attempted = True
        self.docker("volume", "create", "--label", LABEL + "=" + self.run_id, self.volume)
        workers = {}
        for placement, mount in (
            ("bind", mount_spec("type=bind", "source=" + str(self.fixture), "target=/work")),
            ("volume", mount_spec("type=volume", "source=" + self.volume, "target=/work")),
        ):
            name = self.run_id + "-" + placement
            self.container(placement, "--detach", "--network=none", "--mount", mount,
                           IMAGE, "sleep", "900")
            workers[placement] = name
            self.docker("exec", name, "sh", "-ec",
                        'mkdir /work/src; dd if=/dev/zero of=/work/seed bs=4096 count=1 2>/dev/null; '
                        'i=0; while [ "$i" -lt "$1" ]; do cp /work/seed /work/src/f$i; i=$((i+1)); done',
                        "seed", str(self.args.files))
        workloads = (
            ("metadata", 'find /work/src -type f -exec stat -c %s {} + | sha256sum'),
            ("read", 'find /work/src -type f -exec cat {} + | sha256sum'),
            ("write_fsync", 'dd if=/dev/zero of=/work/blob bs=1048576 count="$1" conv=fsync'),
            ("package_build", 'tar -czf /work/package.tar.gz -C /work/src .; sha256sum /work/package.tar.gz'),
        )
        expected_digests = {
            "metadata": hashlib.sha256(b"4096\n" * self.args.files).hexdigest(),
            "read": hashlib.sha256(bytes(4096 * self.args.files)).hexdigest(),
        }
        for iteration in range(self.args.iterations + 1):
            self.assert_idle()
            order = ("bind", "volume") if iteration % 2 else ("volume", "bind")
            for workload, script in workloads:
                for placement in order:
                    result = self.docker("exec", workers[placement], "sh", "-ec", script,
                                         "workload", str(self.args.blob_mib),
                                         measured=None if iteration == 0 else {
                                             "workload": workload, "placement": placement,
                                             "iteration": iteration})
                    if workload in expected_digests:
                        require(result.stdout.split()[0] == expected_digests[workload],
                                f"{placement} {workload} did not observe the complete fixture")
        emit("storage_workload", file_count=self.args.files, bytes_per_file=4096,
             write_bytes=self.args.blob_mib * 1048576,
             note="Zero-filled fixtures. package_build creates a gzip tar archive and hashes it. "
                  "write_fsync requests guest fsync; it is not a host power-loss durability test.")

    def http_latency(self):
        self.assert_idle()
        self.container("http", "--detach", "--publish", "127.0.0.1::8080", IMAGE,
                       "sh", "-ec", 'mkdir /www; printf %s "$1" > /www/index.html; '
                       'exec httpd -f -p 8080 -h /www', "http", self.run_id)
        info = json.loads(self.docker("container", "inspect", self.run_id + "-http").stdout)[0]
        bindings = info["NetworkSettings"]["Ports"]["8080/tcp"]
        require(len(bindings) == 1 and bindings[0]["HostIp"] == "127.0.0.1",
                "HTTP fixture was not bound only to loopback")
        port = int(bindings[0]["HostPort"])

        def request():
            connection = http.client.HTTPConnection("127.0.0.1", port, timeout=2)
            started = time.perf_counter()
            try:
                connection.request("GET", "/")
                response = connection.getresponse()
                require(response.status == 200 and response.read(1024).decode() == self.run_id,
                        "Unexpected HTTP fixture response")
                return time.perf_counter() - started
            finally:
                connection.close()

        deadline = time.monotonic() + 20
        while True:
            try:
                request()
                break
            except (OSError, http.client.HTTPException):
                if time.monotonic() >= deadline:
                    raise
                time.sleep(0.25)
        for iteration in range(1, self.args.iterations * 5 + 1):
            emit("sample", workload="localhost_http_new_connection", iteration=iteration,
                 seconds=request(), port=port)

    def cleanup(self):
        if not self.containers and not self.volume_attempted:
            return
        errors = []
        for kind, name in [("container", name) for name in reversed(self.containers)] + (
                [("volume", self.volume)] if self.volume_attempted else []):
            try:
                # Listing distinguishes absent resources from an unreachable Engine.
                listing = self.docker(kind, "ls", *(["--all"] if kind == "container" else []),
                                      "--format", "{{.Names}}" if kind == "container" else "{{.Name}}")
                if name not in listing.stdout.splitlines():
                    continue
                info = json.loads(self.docker(kind, "inspect", name).stdout)[0]
                labels = info.get("Config", {}).get("Labels", {}) if kind == "container" else info.get("Labels", {})
                require((labels or {}).get(LABEL) == self.run_id,
                        f"{kind} {name} no longer has this run's ownership label")
                self.docker(kind, "rm", *(["--force"] if kind == "container" else []),
                            info["Id"] if kind == "container" else name)
            except (OSError, ValueError, KeyError, RuntimeError, subprocess.SubprocessError) as error:
                errors.append(str(error))
        require(not errors, "Cleanup incomplete: " + "; ".join(errors))
        emit("cleanup", status="passed", retained_fixture=str(self.fixture),
             note="Only this run's labeled containers and volume were removed. Image cache retained.")


def bounded_integer(low, high):
    def parse(value):
        number = int(value)
        if not low <= number <= high:
            raise argparse.ArgumentTypeError(f"must be between {low} and {high}")
        return number
    return parse


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--run", action="store_true")
    parser.add_argument("--fixture-root", required=True, type=Path)
    parser.add_argument("--luhmen", default="luhmen")
    parser.add_argument("--pull", action="store_true", help="Allow downloading the pinned image if missing")
    parser.add_argument("--iterations", default=3, type=bounded_integer(1, 10))
    parser.add_argument("--files", default=256, type=bounded_integer(16, 2048))
    parser.add_argument("--blob-mib", default=16, type=bounded_integer(1, 64))
    parser.add_argument("--idle-seconds", default=3, type=bounded_integer(1, 30))
    args = parser.parse_args()
    if not args.run:
        parser.error("No actions taken. Pass --run to measure the otherwise idle VM.")
    def interrupted(*_):
        raise KeyboardInterrupt
    signal.signal(signal.SIGTERM, interrupted)
    suite = None
    failed = False
    try:
        suite = Measurements(args)
        suite.preflight()
        suite.idle_resources()
        suite.startup()
        suite.storage()
        suite.http_latency()
        emit("guest_memory_after_workloads", **suite.guest_memory())
    except (OSError, ValueError, KeyError, RuntimeError, http.client.HTTPException,
            subprocess.SubprocessError, KeyboardInterrupt) as error:
        failed = True
        emit("suite", status="failed", error=str(error))
    finally:
        if suite is not None:
            try:
                suite.cleanup()
            except (OSError, ValueError, RuntimeError, subprocess.SubprocessError) as error:
                failed = True
                emit("cleanup", status="failed", error=str(error), run_id=suite.run_id,
                     fixture=str(suite.fixture), endpoint=suite.endpoint)
    emit("suite", status="failed" if failed else "passed")
    return int(failed)


if __name__ == "__main__":
    sys.exit(main())
