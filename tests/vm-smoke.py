#!/usr/bin/env python3
"""Opt-in integration checks for an existing, otherwise idle luhmen VM.

Requires Python 3.10+, Docker CLI, Compose, and Buildx. Every Docker command
selects the verified luhmen context. This suite restarts the VM. It refuses
existing containers and never prunes images, volumes, networks, or build caches.
JSON Lines on stdout record results; generated fixtures remain available.
"""

import argparse
from contextlib import contextmanager
import hashlib
import http.client
from http.server import BaseHTTPRequestHandler, HTTPServer
import ipaddress
import json
import os
from pathlib import Path
import re
import signal
import socket
import subprocess
import sys
import tempfile
import time
import threading
from urllib.parse import urlsplit
import uuid


BASE_IMAGE = "busybox:1.37.0@sha256:f10e809bcf667d8e9f01d2baf82869049a495cd448cdfe1f4dee94078b960ae9"
OWNER_LABEL = "io.luhmen.smoke.run"
CONTEXT_DESCRIPTION = "luhmen managed Docker Engine (schema 1)"
PROXY_VARIABLES = ("http_proxy", "https_proxy", "ftp_proxy", "all_proxy", "no_proxy",
                   "HTTP_PROXY", "HTTPS_PROXY", "FTP_PROXY", "ALL_PROXY", "NO_PROXY")


def local_ipv4_addresses():
    # Read interface addresses without changing routes, DNS, or VPN configuration.
    output = command(["/sbin/ifconfig", "-a"]).stdout
    addresses = {str(ipaddress.ip_address(value))
                 for value in re.findall(r"^\s+inet (\S+)", output, re.MULTILINE)}
    return sorted(address for address in addresses
                  if not ipaddress.ip_address(address).is_loopback)


def port_closed(address, port):
    with socket.socket(socket.AF_INET, socket.SOCK_STREAM) as stream:
        stream.settimeout(1)
        require(stream.connect_ex((address, port)) != 0,
                f"Unexpected listener at {address}:{port}")


@contextmanager
def http_fixture(token):
    """Two loopback-only HTTP fixtures, with no arbitrary proxy destinations."""
    hits = {"origin": [], "proxy": []}
    servers = []
    threads = []

    class Handler(BaseHTTPRequestHandler):
        def setup(self):
            self.request.settimeout(3)
            super().setup()

        def log_message(self, *_):
            pass

        def do_GET(self):
            self.server.requests += 1
            if self.server.requests > 100:
                self.send_error(429)
                return
            path = self.path
            if self.server.role == "proxy":
                target = urlsplit(path)
                if (target.scheme != "http" or target.netloc != authority
                        or target.query or target.fragment):
                    self.send_error(403)
                    return
                path = target.path
            if not path.startswith("/" + token + "/") or len(path) > 128:
                self.send_error(404)
                return
            hits[self.server.role].append(path)
            if self.server.role == "proxy" and path.endswith("/failure"):
                self.send_error(503)
                return
            body = token.encode()
            if self.server.role == "proxy":
                upstream = http.client.HTTPConnection("127.0.0.1", servers[0].server_port, timeout=3)
                try:
                    upstream.request("GET", path)
                    response = upstream.getresponse()
                    body = response.read(4096)
                    if response.status != 200 or body != token.encode():
                        self.send_error(502)
                        return
                except (OSError, http.client.HTTPException):
                    self.send_error(502)
                    return
                finally:
                    upstream.close()
            self.send_response(200)
            self.send_header("Content-Length", str(len(body)))
            self.end_headers()
            self.wfile.write(body)

    try:
        for role in ("origin", "proxy"):
            server = HTTPServer(("127.0.0.1", 0), Handler)
            server.role = role
            server.requests = 0
            servers.append(server)
        authority = "host.docker.internal:" + str(servers[0].server_port)
        for server in servers:
            thread = threading.Thread(target=server.serve_forever, daemon=True)
            thread.start()
            threads.append(thread)
        yield {"origin": "http://" + authority,
               "proxy": "http://host.docker.internal:" + str(servers[1].server_port),
               "host_origin": "http://127.0.0.1:" + str(servers[0].server_port),
               "hits": hits}
    finally:
        for server, thread in zip(servers, threads):
            server.shutdown()
            thread.join(timeout=5)
        for server in servers:
            server.server_close()


def emit(check, status, **details):
    print(json.dumps({"check": check, "status": status, **details}), flush=True)


def require(condition, message):
    if not condition:
        raise RuntimeError(message)


def command(arguments, *, timeout=60, env=None, cwd=None, check=True):
    arguments = [str(value) for value in arguments]
    with subprocess.Popen(arguments, stdout=subprocess.PIPE, stderr=subprocess.PIPE,
                          text=True, env=env, cwd=cwd, start_new_session=True) as process:
        try:
            stdout, stderr = process.communicate(timeout=timeout)
        except (subprocess.TimeoutExpired, KeyboardInterrupt):
            try:
                os.killpg(process.pid, signal.SIGKILL)
            except ProcessLookupError:
                pass
            process.communicate(timeout=5)
            raise
        result = subprocess.CompletedProcess(arguments, process.returncode, stdout, stderr)
    if check and result.returncode:
        raise RuntimeError(
            f"Command failed with {result.returncode}: {arguments!r}\n"
            f"{result.stderr[-6000:]}{result.stdout[-2000:]}"
        )
    return result


def eventually(action, *, timeout=60):
    deadline = time.monotonic() + timeout
    error = None
    while time.monotonic() < deadline:
        try:
            return action()
        except (OSError, RuntimeError, http.client.HTTPException) as caught:
            error = caught
            time.sleep(0.5)
    raise RuntimeError(f"Readiness deadline expired: {error}")


class Suite:
    def __init__(self, args):
        self.args = args
        self.luhmen = args.luhmen
        self.docker_binary = os.environ.get("LUHMEN_DOCKER", "docker")
        self.env = dict(os.environ)
        for name in ("DOCKER_HOST", "DOCKER_CONTEXT", "DOCKER_TLS_VERIFY",
                     "DOCKER_CERT_PATH", "BUILDX_BUILDER", "COMPOSE_PROJECT_NAME",
                     "COMPOSE_FILE", "COMPOSE_PROFILES", "COMPOSE_ENV_FILES"):
            self.env.pop(name, None)
        self.run_id = "luhmen-check-" + uuid.uuid4().hex[:12]
        self.fixture = args.fixture_root.resolve(strict=True) / self.run_id
        self.endpoint = None
        self.compose_attempted = False
        self.container_id = None
        self.images = [f"{self.run_id}-app:smoke", f"{self.run_id}-build:smoke",
                       f"{self.run_id}-proxy:smoke"]

    def inspect(self):
        return json.loads(command([self.luhmen, "inspect", "--json"], timeout=30).stdout)

    def verify_context(self):
        contexts = json.loads(command(
            [self.docker_binary, "context", "inspect", "luhmen"],
            env=self.env, timeout=20,
        ).stdout)
        require(len(contexts) == 1, "Expected exactly one luhmen context")
        context = contexts[0]
        require(context.get("Name") == "luhmen"
                and context.get("Endpoints", {}).get("docker", {}).get("Host") == self.endpoint
                and context.get("Metadata", {}).get("Description") == CONTEXT_DESCRIPTION,
                "Docker context ownership or endpoint changed; refusing operation")

    def docker(self, *arguments, timeout=60, check=True):
        self.verify_context()
        return command([self.docker_binary, "--context", "luhmen", *arguments],
                       timeout=timeout, env=self.env, cwd=self.fixture if self.fixture.exists() else None,
                       check=check)

    def compose(self, *arguments, timeout=120, check=True):
        return self.docker("compose", "--project-name", self.run_id,
                           "--file", str(self.fixture / "compose.json"),
                           *arguments, timeout=timeout, check=check)

    def guest(self, *arguments, timeout=60):
        return command([self.luhmen, "shell", *arguments], timeout=timeout)

    def preflight(self):
        require(self.args.fixture_root.is_dir(), "--fixture-root must be an existing directory")
        state = self.inspect()
        require(state.get("schema_version") == 1 and state.get("name") == "luhmen"
                and state.get("context") == "luhmen" and state.get("state") == "Running"
                and state.get("context_ready") and state.get("engine_ready"),
                "Start a healthy, owned luhmen VM before running this suite")
        vm = state.get("vm") or {}
        self.endpoint = state.get("endpoint", "")
        require(vm.get("name") == "luhmen" and vm.get("vmType") == "vz"
                and vm.get("arch") == "aarch64"
                and self.endpoint == "unix://" + str(Path(vm["dir"]) / "sock/docker.sock"),
                "VM identity and Unix socket endpoint do not agree")
        root = self.args.fixture_root.resolve(strict=True)
        mounts = (state.get("config") or {}).get("mounts", [])
        require(any(mount.get("writable") and root.is_relative_to(Path(mount["path"]).resolve())
                    for mount in mounts), "--fixture-root must be inside a configured writable mount")
        require(not self.docker("ps", "--all", "--quiet").stdout.strip(),
                "The suite restarts this VM; remove or move existing containers before running it")
        self.settings = state["config"]
        versions = {
            "luhmen": command([self.luhmen, "--version"]).stdout.strip(),
            "docker": self.docker("version", "--format", "{{json .}}").stdout.strip(),
            "compose": self.docker("compose", "version", "--short").stdout.strip(),
            "buildx": self.docker("buildx", "version").stdout.strip(),
        }
        emit("preflight", "passed", run_id=self.run_id, endpoint=self.endpoint,
             settings=self.settings, versions=versions, base_image=BASE_IMAGE)

    def assert_only_fixture_containers(self):
        identifiers = self.docker("ps", "--all", "--quiet", "--no-trunc").stdout.split()
        for identifier in identifiers:
            data = json.loads(self.docker("inspect", identifier).stdout)[0]
            require(data.get("Config", {}).get("Labels", {}).get(OWNER_LABEL) == self.run_id,
                    "Another workload appeared in luhmen; refusing a VM restart or daemon kill")

    def prepare_fixture(self):
        self.fixture.mkdir(mode=0o700)
        (self.fixture / ".luhmen-smoke.json").write_text(json.dumps({"run_id": self.run_id}) + "\n")
        shared = self.fixture / "shared"
        shared.mkdir()
        self.token = uuid.uuid4().hex
        (shared / "host.txt").write_text(self.token)
        (self.fixture / "index.html").write_text(self.token)
        (self.fixture / "Dockerfile").write_text(
            f"FROM {BASE_IMAGE}\nLABEL {OWNER_LABEL}={self.run_id}\n"
            "COPY index.html /www/index.html\nRUN test -s /www/index.html && printf built > /built\n"
            'CMD ["httpd", "-f", "-p", "8080", "-h", "/www"]\n'
        )
        (self.fixture / ".dockerignore").write_text("*\n!Dockerfile\n!index.html\n")
        with socket.socket() as reservation:
            reservation.bind(("127.0.0.1", 0))
            self.port = reservation.getsockname()[1]
        self.non_loopback_addresses = local_ipv4_addresses()
        for address in ["127.0.0.1", *self.non_loopback_addresses]:
            port_closed(address, self.port)
        compose = {
            "services": {"web": {
                "build": {"context": "."}, "image": self.images[0],
                "labels": {OWNER_LABEL: self.run_id}, "restart": "unless-stopped",
                "ports": [f"127.0.0.1:{self.port}:8080"],
                "cpus": 0.5, "mem_limit": "128m", "pids_limit": 64,
                "volumes": [{"type": "bind", "source": str(shared), "target": "/shared"},
                            {"type": "volume", "source": "persistent", "target": "/data"}],
                "healthcheck": {"test": ["CMD", "wget", "-q", "-O", "/dev/null", "http://127.0.0.1:8080"],
                                "interval": "2s", "timeout": "2s", "retries": 20},
            }},
            "volumes": {"persistent": {"labels": {OWNER_LABEL: self.run_id}}},
            "networks": {"default": {"labels": {OWNER_LABEL: self.run_id}}},
        }
        (self.fixture / "compose.json").write_text(json.dumps(compose, indent=2) + "\n")
        emit("fixture", "created", path=str(self.fixture), project=self.run_id, port=self.port)

    def ping(self):
        connection = http.client.HTTPConnection("localhost", timeout=5)
        stream = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
        stream.settimeout(5)
        try:
            stream.connect(self.endpoint.removeprefix("unix://"))
            connection.sock = stream
            connection.request("GET", "/_ping")
            response = connection.getresponse()
            body = response.read(1024)
            require(response.status == 200 and body == b"OK", "Engine API ping failed")
        finally:
            connection.close()
            stream.close()

    def http(self):
        connection = http.client.HTTPConnection("127.0.0.1", self.port, timeout=3)
        try:
            connection.request("GET", "/")
            response = connection.getresponse()
            require(response.status == 200 and response.read(4096).decode() == self.token,
                    "Published localhost port returned the wrong fixture response")
        finally:
            connection.close()

    def workloads(self):
        self.ping()
        info = json.loads(self.docker("info", "--format", "{{json .}}").stdout)
        expected_memory = self.settings["memory_gib"] * 1024 ** 3
        require(info["NCPU"] == self.settings["cpus"], "Guest CPU count differs from configuration")
        require(expected_memory * 0.7 <= info["MemTotal"] <= expected_memory,
                "Guest usable RAM is outside the configured limit and expected kernel overhead")
        emit("engine_api_and_vm_resources", "passed", cpus=info["NCPU"], memory_bytes=info["MemTotal"],
             engine_version=info.get("ServerVersion"), driver=info.get("Driver"))
        self.compose_attempted = True
        started = time.monotonic()
        self.compose("up", "--build", "--detach", "--wait", "--wait-timeout", "90", timeout=900)
        self.container_id = self.compose("ps", "--quiet", "web").stdout.strip()
        require(re.fullmatch(r"[a-f0-9]{12,64}", self.container_id), "Expected one fixture container")
        eventually(self.http)
        emit("compose_build_and_localhost_http", "passed", seconds=time.monotonic() - started)
        read = self.docker("exec", self.container_id, "cat", "/shared/host.txt").stdout
        require(read == self.token, "Container could not read a host bind mount")
        self.docker("exec", self.container_id, "sh", "-c", "printf %s \"$1\" > /shared/guest.txt", "sh", self.token)
        require((self.fixture / "shared/guest.txt").read_text() == self.token,
                "Host could not read the container's bind-mount write")
        self.docker("exec", self.container_id, "sh", "-c", "printf %s \"$1\" > /data/probe", "sh", self.token)
        details = json.loads(self.docker("inspect", self.container_id).stdout)[0]
        require(details["HostConfig"]["NanoCpus"] == 500_000_000
                and details["HostConfig"]["Memory"] == 128 * 1024 ** 2,
                "Compose container resource limits were not applied")
        emit("bind_mounts_and_container_limits", "passed")
        self.guest("getent", "ahostsv4", "host.docker.internal")
        self.docker("exec", self.container_id, "nslookup", "host.docker.internal")
        # BusyBox nslookup applies host search suffixes to an unqualified name.
        # Query the service record directly; dns_and_proxy checks HTTP via "web".
        self.docker("exec", self.container_id, "nslookup", "web.")
        emit("guest_host_alias_and_compose_dns", "passed")

    def coherence(self):
        payloads = [uuid.uuid4().hex * 128 for _ in range(4)]
        final_files = []
        for direction in ("host_to_container", "container_to_host"):
            directory = self.fixture / "shared" / direction
            directory.mkdir()
            path = directory / "state.txt"
            guest_path = "/shared/" + direction + "/state.txt"
            current = path
            remote = guest_path

            def write(destination, content):
                if direction == "host_to_container":
                    destination.write_text(content)
                else:
                    target = "/shared/" + str(destination.relative_to(self.fixture / "shared"))
                    self.docker("exec", self.container_id, "sh", "-c",
                                'printf %s "$2" > "$1"', "sh", target, content)

            def move(source, destination):
                if direction == "host_to_container":
                    source.replace(destination)
                else:
                    root = self.fixture / "shared"
                    self.docker("exec", self.container_id, "mv",
                                "/shared/" + str(source.relative_to(root)),
                                "/shared/" + str(destination.relative_to(root)))

            def visible(expected):
                if direction == "host_to_container":
                    result = self.docker("exec", self.container_id, "cat", remote, check=False)
                    require(result.returncode == 0 and result.stdout == expected,
                            "Container file contents differ from the host write")
                else:
                    require(current.read_text() == expected,
                            "Host file contents differ from the container write")

            def absent(destination):
                target = "/shared/" + str(destination.relative_to(self.fixture / "shared"))
                if direction == "host_to_container":
                    require(self.docker("exec", self.container_id, "test", "!", "-e", target,
                                        check=False).returncode == 0,
                            "Deleted or renamed host path remains visible inside the container")
                else:
                    require(not destination.exists(), "Deleted or renamed container path remains visible on the host")

            write(path, payloads[0])
            eventually(lambda: visible(payloads[0]), timeout=15)
            for operation, value in (("same_length_overwrite", payloads[1]),
                                     ("atomic_replace", payloads[2]),
                                     ("rename", payloads[2]),
                                     ("delete_recreate", payloads[3])):
                if operation == "atomic_replace":
                    temporary = current.with_suffix(".new")
                    write(temporary, value)
                    move(temporary, current)
                elif operation == "rename":
                    previous = current
                    current = current.with_name("renamed.txt")
                    move(previous, current)
                    remote = "/shared/" + str(current.relative_to(self.fixture / "shared"))
                    eventually(lambda: absent(previous), timeout=15)
                elif operation == "delete_recreate":
                    if direction == "host_to_container":
                        current.unlink()
                    else:
                        self.docker("exec", self.container_id, "rm", remote)
                    eventually(lambda: absent(current), timeout=15)
                    write(current, value)
                else:
                    write(current, value)
                eventually(lambda: visible(value), timeout=15)
                emit("file_coherence_" + operation, "passed", direction=direction)
            final_files.append(current)
        digests = {}
        for path in final_files:
            relative = str(path.relative_to(self.fixture / "shared"))
            local = hashlib.sha256(path.read_bytes()).hexdigest()
            remote = self.docker("exec", self.container_id, "sha256sum", "/shared/" + relative).stdout.split()[0]
            require(local == remote, "Final bind-mount digest differs across the VM boundary")
            digests[relative] = local
        emit("file_coherence_final_digests", "passed", sha256=digests,
             note="File visibility is checked independently of filesystem notifications")

    def port_lifecycle(self):
        details = json.loads(self.docker("inspect", self.container_id).stdout)[0]
        require(details["NetworkSettings"]["Ports"]["8080/tcp"] == [
            {"HostIp": "127.0.0.1", "HostPort": str(self.port)}],
            "Docker published the fixture outside its requested loopback address")
        for cycle in range(3):
            for address in self.non_loopback_addresses:
                port_closed(address, self.port)
            self.compose("stop", "--timeout", "10", "web", timeout=30)
            eventually(lambda: port_closed("127.0.0.1", self.port), timeout=30)
            self.compose("start", "web", timeout=30)
            eventually(self.http, timeout=60)
            emit("localhost_port_close_and_reuse", "passed", cycle=cycle + 1, port=self.port)
        for address in self.non_loopback_addresses:
            port_closed(address, self.port)
        emit("published_port_non_loopback_exposure", "passed" if self.non_loopback_addresses else "not_run",
             addresses=self.non_loopback_addresses,
             note="Local IPv4 interface probes; a separate peer and IPv6 require separate checks")

    def dns_and_proxy(self):
        addresses = {entry[4][0] for entry in socket.getaddrinfo("localhost", None)}
        require(addresses and all(ipaddress.ip_address(address).is_loopback for address in addresses),
                "Host localhost DNS resolved outside loopback")
        emit("host_localhost_dns", "passed", addresses=sorted(addresses))
        response = self.docker("exec", self.container_id, "wget", "-Y", "off", "-q", "-T", "5",
                               "-O", "-", "http://web:8080").stdout
        require(response == self.token, "Compose DNS reached the wrong application")
        emit("compose_dns_http", "passed")
        clear_proxy = [argument for name in PROXY_VARIABLES for argument in ("-u", name)]
        with http_fixture(self.token) as fixture:
            def url(case):
                return fixture["origin"] + "/" + self.token + "/" + case

            def route(case, proxied):
                path = "/" + self.token + "/" + case
                require(path in fixture["hits"]["origin"], "Request never reached the owned HTTP origin")
                require((path in fixture["hits"]["proxy"]) == proxied,
                        "Request used a different proxy route than requested")

            origin = urlsplit(fixture["host_origin"])
            connection = http.client.HTTPConnection("localhost", origin.port, timeout=3)
            try:
                connection.request("GET", "/" + self.token + "/host_direct")
                response = connection.getresponse()
                require(response.status == 200 and response.read(4096).decode() == self.token,
                        "Host localhost DNS reached the wrong HTTP fixture")
            finally:
                connection.close()
            route("host_direct", False)
            emit("host_localhost_http", "passed")
            for case, assignments, proxied in (
                    ("guest_direct", [], False),
                    ("guest_proxy", ["http_proxy=" + fixture["proxy"]], True),
                    ("guest_no_proxy", ["http_proxy=" + fixture["proxy"],
                                        "NO_PROXY=host.docker.internal"], False)):
                result = self.guest("env", *clear_proxy, *assignments,
                                    "curl", "--disable", "--fail", "--silent", "--show-error", "--max-time", "10", url(case))
                require(result.stdout == self.token, "Guest HTTP fixture returned the wrong body")
                route(case, proxied)
                emit(case, "passed")
            for case, proxied in (("container_direct", False), ("container_proxy", True)):
                variables = {name: "" for name in PROXY_VARIABLES}
                if proxied:
                    variables["http_proxy"] = fixture["proxy"]
                arguments = [argument for name, value in variables.items() for argument in ("--env", name + "=" + value)]
                result = self.docker("exec", *arguments, self.container_id, "wget", "-Y", "on" if proxied else "off",
                                     "-q", "-T", "5", "-O", "-", url(case))
                require(result.stdout == self.token, "Container HTTP fixture returned the wrong body")
                route(case, proxied)
                emit(case, "passed")
            failure = command([self.luhmen, "shell", "env", *clear_proxy,
                               "http_proxy=" + fixture["proxy"], "curl", "--disable", "--fail", "--silent",
                               "--show-error", "--max-time", "10", url("failure")], check=False, timeout=20)
            require(failure.returncode != 0 and "/" + self.token + "/failure" in fixture["hits"]["proxy"]
                    and "/" + self.token + "/failure" not in fixture["hits"]["origin"],
                    "Proxy failure was bypassed or reported as success")
            emit("guest_proxy_failure", "passed")
            build = self.fixture / "proxy-build"
            build.mkdir()
            (build / "Dockerfile").write_text(
                f"FROM {BASE_IMAGE}\nLABEL {OWNER_LABEL}={self.run_id}\n"
                f"RUN wget -Y on -q -T 5 -O /proxy-response {url('build_proxy')}\n")
            arguments = [argument for name in PROXY_VARIABLES for argument in
                         ("--build-arg", name + "=" + (fixture["proxy"] if name.lower() == "http_proxy" else ""))]
            self.docker("buildx", "build", "--builder", "luhmen", "--load", "--no-cache", *arguments,
                        "--tag", self.images[2], str(build), timeout=180)
            route("build_proxy", True)
            emit("build_http_proxy", "passed")
        emit("proxy_scope", "passed", note="Fixture HTTP requests only; registry, HTTPS CONNECT and authentication remain untested")

    def buildx(self):
        builder = self.docker("buildx", "inspect", "luhmen").stdout
        require(re.search(r"^Driver:\s+docker\s*$", builder, re.MULTILINE)
                and re.findall(r"^Endpoint:\s+(\S+)", builder, re.MULTILINE) == ["luhmen"],
                "The luhmen Buildx builder must use the docker driver and luhmen endpoint")
        timings = []
        for iteration in range(2):
            started = time.monotonic()
            result = self.docker("buildx", "build", "--builder", "luhmen", "--load", "--progress", "plain",
                                 "--tag", self.images[1], ".", timeout=600)
            timings.append(time.monotonic() - started)
            if iteration:
                require("CACHED" in result.stdout + result.stderr, "Buildx did not report a cached build step")
        emit("buildx_build_and_cache", "passed", seconds=timings)

    def persistence(self):
        self.assert_only_fixture_containers()
        started = time.monotonic()
        command([self.luhmen, "restart", "--json"], timeout=1200)
        self.verify_context()
        eventually(self.ping)
        eventually(self.http, timeout=90)
        value = self.docker("exec", self.container_id, "cat", "/data/probe").stdout
        require(value == self.token, "Named volume data changed across VM restart")
        require(self.docker("exec", self.container_id, "cat", "/shared/host.txt").stdout == self.token,
                "Host mount was not restored after restart")
        emit("restart_volume_mount_and_port_persistence", "passed", seconds=time.monotonic() - started)

    def recovery(self):
        self.assert_only_fixture_containers()
        before = self.guest("sudo", "systemctl", "show", "docker.service", "--property=MainPID", "--value").stdout.strip()
        require(before.isdecimal() and int(before) > 1, "Docker service has no main PID")
        started = time.monotonic()
        self.guest("sudo", "systemctl", "kill", "--kill-whom=main", "--signal=SIGKILL", "docker.service")
        def restarted():
            current = self.guest("sudo", "systemctl", "show", "docker.service", "--property=MainPID", "--value").stdout.strip()
            require(current.isdecimal() and int(current) > 1 and current != before, "Waiting for Docker service recovery")
            self.ping()
        eventually(restarted, timeout=90)
        eventually(self.http, timeout=90)
        require(self.docker("exec", self.container_id, "cat", "/data/probe").stdout == self.token,
                "Volume data changed after Docker daemon failure")
        emit("docker_daemon_failure_recovery", "passed", seconds=time.monotonic() - started)

    def watchers(self):
        self.guest("sudo", "apt-get", "update", "-o", "APT::Update::Error-Mode=any", timeout=180)
        self.guest("sudo", "apt-get", "install", "-y", "--no-install-recommends",
                   "inotify-tools", timeout=180)
        shared = self.fixture / "shared"
        (shared / "nested").mkdir()
        cases = [("modify", shared / "modify.txt", "write", True),
                 ("nested_modify", shared / "nested/modify.txt", "write", True),
                 ("create", shared / "create.txt", "create", False),
                 ("atomic_save", shared / "atomic.txt", "replace", False),
                 ("rename", shared / "rename.txt", "rename", False),
                 ("delete", shared / "delete.txt", "delete", False),
                 ("delete_recreate", shared / "recreate.txt", "recreate", False)]
        for name, path, operation, required in cases:
            if operation != "create":
                path.write_text("before")
            with tempfile.TemporaryFile(dir=self.fixture) as output, tempfile.TemporaryFile(dir=self.fixture) as errors:
                process = subprocess.Popen(
                    [self.luhmen, "shell", "timeout", "8", "inotifywait", "--monitor", "--recursive",
                     "--format", "%e|%w%f", "--event", "modify,create,moved_to,moved_from,delete", str(shared)],
                    stdin=subprocess.DEVNULL, stdout=output, stderr=errors,
                    start_new_session=True,
                )
                try:
                    deadline = time.monotonic() + 5
                    while time.monotonic() < deadline:
                        errors.seek(0)
                        if b"Watches established" in errors.read():
                            break
                        require(process.poll() is None, "inotifywait exited before establishing watches")
                        time.sleep(0.1)
                    else:
                        raise RuntimeError("inotifywait did not establish watches")
                    expected = [(str(path), "MODIFY")]
                    if operation in ("delete", "recreate"):
                        path.unlink()
                        expected = [(str(path), "DELETE")]
                        if operation == "recreate":
                            path.write_text("after!")
                            expected.append((str(path), "CREATE"))
                    elif operation == "replace":
                        temporary = path.with_suffix(".new")
                        temporary.write_text("after!")
                        temporary.replace(path)
                        expected = [(str(path), "MOVED_TO")]
                    elif operation == "rename":
                        destination = path.with_suffix(".renamed")
                        path.rename(destination)
                        expected = [(str(path), "MOVED_FROM"), (str(destination), "MOVED_TO")]
                    else:
                        path.write_text("after!")
                        if operation == "create":
                            expected = [(str(path), "CREATE")]
                    require(process.wait(timeout=12) == 124,
                            "inotifywait failed before its observation timeout")
                    output.seek(0)
                    events = output.read().decode().splitlines()
                    paths = {path for path, _ in expected}
                    observed = [event for event in events if event.partition("|")[2] in paths]
                    missing = [{"path": target, "event": kind} for target, kind in expected
                               if not any(event.partition("|")[2] == target
                                          and kind in event.partition("|")[0].split(",")
                                          for event in observed)]
                    require(not missing or not required, f"No host {name} event reached guest inotify")
                    emit("watcher_" + name, "passed" if not missing else "limitation",
                         events=observed, missing=missing,
                         note="Typed notification check; file contents are checked separately. Use application polling for missing events")
                finally:
                    if process.poll() is None:
                        try:
                            os.killpg(process.pid, signal.SIGKILL)
                        except ProcessLookupError:
                            pass
                        process.wait(timeout=5)

    def cleanup(self):
        if not self.compose_attempted:
            return
        self.verify_context()
        # The random project and labels prevent cleanup from expanding its scope.
        result = self.compose("down", "--volumes", "--timeout", "20", timeout=90, check=False)
        require(result.returncode == 0, "Fixture Compose cleanup failed: " + result.stderr[-2000:])
        for image in self.images:
            result = self.docker("image", "inspect", image, check=False)
            if result.returncode == 0:
                details = json.loads(result.stdout)[0]
                require(details.get("Config", {}).get("Labels", {}).get(OWNER_LABEL) == self.run_id,
                        "Fixture image ownership changed; refusing removal")
                self.docker("image", "rm", image)
        emit("cleanup", "passed", retained_fixture=str(self.fixture),
             note="Downloaded base image and shared build cache are retained")


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--run", action="store_true", help="Run checks and restart the otherwise idle luhmen VM")
    parser.add_argument("--fixture-root", type=Path, required=True, help="Existing directory already shared read-write with luhmen")
    parser.add_argument("--luhmen", default="luhmen", help="Path to the luhmen executable")
    parser.add_argument("--recovery", action="store_true", help="Also kill the VM's Docker daemon and verify recovery")
    parser.add_argument("--watchers", action="store_true", help="Install snapshot-pinned inotify-tools in the guest and check watcher events")
    args = parser.parse_args()
    if not args.run:
        parser.error("No actions taken. Pass --run to run this suite and restart the luhmen VM.")
    suite = None
    failure = False
    try:
        suite = Suite(args)
        suite.preflight()
        suite.prepare_fixture()
        suite.workloads()
        suite.coherence()
        suite.port_lifecycle()
        suite.buildx()
        suite.dns_and_proxy()
        if args.watchers:
            suite.watchers()
        else:
            emit("watchers", "not_run", reason="Pass --watchers to enable")
        suite.persistence()
        if args.recovery:
            suite.recovery()
        else:
            emit("docker_daemon_failure_recovery", "not_run", reason="Pass --recovery to enable")
        for scenario in ("vpn", "split_dns", "dns_tcp_fallback", "authenticated_proxy",
                         "https_connect_proxy", "registry_proxy", "container_no_proxy",
                         "external_peer_port_exposure", "udp_ports", "ipv6", "sleep_wake"):
            emit(scenario, "not_run", reason="Requires a separate host-specific validation window")
    except (OSError, ValueError, KeyError, RuntimeError, http.client.HTTPException,
            subprocess.SubprocessError, KeyboardInterrupt) as error:
        failure = True
        emit("suite", "failed", error=str(error))
    finally:
        if suite is not None:
            try:
                suite.cleanup()
            except (OSError, ValueError, RuntimeError, subprocess.SubprocessError) as error:
                failure = True
                emit("cleanup", "failed", error=str(error), project=suite.run_id,
                     fixture=str(suite.fixture), endpoint=suite.endpoint)
    emit("suite", "failed" if failure else "passed")
    return 1 if failure else 0


if __name__ == "__main__":
    sys.exit(main())
