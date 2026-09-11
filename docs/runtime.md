# Runtime behavior

## Ownership and state

luhmen owns one Lima VM named `luhmen`. Its configuration, disk, and sockets live under `$HOME/.local/share/luhmen`. Set `LUHMEN_HOME` to an absolute path before creation to use another location, then keep that setting for later commands. Choose a short path; macOS limits Unix socket paths to 103 bytes.

The Docker context is also named `luhmen`. luhmen rejects an existing context that points elsewhere and leaves Docker's active context and registry credentials unchanged. It will not adopt another runtime's VM or a nonempty state directory without its ownership marker.

The VM disk contains Docker images, containers, BuildKit cache, and named volumes. Stopping or restarting the VM preserves that disk. A bind mount exposes a host directory and stores its contents on the host instead.

Lima uses `$HOME/Library/Caches/lima` for downloaded images, metadata, and converted disks. Other Lima installations can use this cache. luhmen does not remove it.

## Configuration

`luhmen create` saves CPU, memory, disk, and mount settings without starting the VM. These settings are fixed after creation. `luhmen config show` prints them.

Creation needs at least 15 GiB of free host storage; start and restart need 2 GiB. The CPU count cannot exceed the host count. The memory setting must leave at least 2 GiB of physical RAM for macOS. The sparse disk grows as data is written.

Free-space checks do not reserve disk capacity. `luhmen create --dry-run` validates the configuration and prints the Lima settings without creating state or invoking Lima.

The stored configuration uses this format:

```json
{
  "schema_version": 1,
  "cpus": 4,
  "memory_gib": 4,
  "disk_gib": 30,
  "mounts": [
    { "path": "/absolute/path/to/project", "writable": false }
  ]
}
```

Use `--mount /absolute/path:rw` for writable shares or `--mount /absolute/path:ro` for read-only shares. Shares must be existing directories and cannot overlap each other or luhmen's state directory. No host directories are shared by default.

VirtioFS carries file contents between the Mac and VM. Lima's experimental `mountInotify` forwards some host changes on writable mounts, but omits host file-removal events. Enable polling in development servers that need complete change detection. See [Lima's mount documentation](https://lima-vm.io/docs/config/mount/#mount-inotify).

`LUHMEN_LIMACTL` and `LUHMEN_DOCKER` select executable paths instead of `limactl` and `docker` on `PATH`. The Lima version requirement still applies.

## Lifecycle and readiness

`start` waits for Docker Engine to answer through the host socket before reporting success. `inspect --json` reports VM state, configured resources, `engine_ready`, `context_ready`, and errors. A missing shared directory does not prevent inspection or shutdown. `doctor` checks host dependencies without starting workloads.

`stop` requests a graceful guest shutdown. `restart` stops and starts the same VM. Docker restart policies determine which containers resume. The VM disk persists in both cases.

Only one lifecycle command can run at a time. Cancellation and deadlines terminate the temporary command and its helpers, while Lima's detached VM process can remain running. Inspect the VM before retrying. After interrupted creation, rerun `create` with the same settings if the VM is missing, or `start` if it exists.

Lima owns the VM and host integration processes after the CLI exits. The optional `luhmen daemon` is a separate foreground HTTPS service; stopping it does not stop the VM.

## Docker, networking, and SDKs

The guest runs upstream Docker Engine. Compose and Buildx use the host Docker client. SDKs can use the socket URL printed by:

```sh
docker context inspect luhmen --format '{{.Endpoints.docker.Host}}'
```

Select the context explicitly:

```sh
docker --context luhmen ps
docker --context luhmen compose up -d
```

`luhmen docker ...` also selects this context and clears Docker endpoint and Buildx builder environment overrides.

Publish container ports with `-p 127.0.0.1:8080:80` for local access. Port forwarding, guest DNS, and proxy handling use Lima's host integration. VPNs, proxies, corporate DNS, and overlapping address ranges can affect connectivity. See [platforms and versions](support.md) for limits.

Local HTTPS uses a separate proxy in front of explicitly configured localhost ports. It does not expose the Docker API over TCP. See [local HTTPS](https.md).

## Recovery and diagnostics

Start with `luhmen inspect --json` and `luhmen doctor --json`. Check free disk space and logs under the state directory after provisioning or boot failures. Correct missing dependencies or network problems, then retry `start`.

Use `stop` before repairing Lima configuration or copying the disk for backup. `stop --force` can terminate a stuck VM, but may lose unwritten guest data and cannot repair configuration. Do not delete the VM disk to recover from a startup error.

`luhmen shell COMMAND...` runs a guest command, for example `luhmen shell uname -a`.

An interrupted build or Compose operation may leave normal Docker resources behind. Inspect them with `luhmen docker ps -a`, `luhmen docker volume ls`, and `luhmen docker compose ls`. Remove only resources you recognize. luhmen does not run automatic Docker pruning.
