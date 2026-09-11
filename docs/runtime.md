# Runtime behavior

## Ownership and state

luhmen owns one Lima VM named `luhmen`. Its configuration, disk, and sockets live under `$HOME/.local/share/luhmen`. Set `LUHMEN_HOME` to an absolute path before creation to use another location, then keep that setting for later commands. Choose a short path; macOS limits Unix socket paths to 103 bytes.

The Docker context is also named `luhmen`. luhmen rejects an existing context that points elsewhere and leaves Docker's active context and registry credentials unchanged. It will not adopt another runtime's VM or a nonempty state directory without its ownership marker.

Changing `LUHMEN_HOME` alone does not create a separate Docker context namespace. For an isolated development VM, also use a separate `DOCKER_CONFIG` directory, which separates Docker contexts and credentials. Keep both settings for every command and make Compose and Buildx available in that Docker configuration.

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

Start and restart print elapsed milliseconds for `preflight`, `lima_start`, and `engine_socket_ready` to stderr. Restart also measures `lima_stop` when needed. `lima_start` includes boot, provisioning, and Lima's guest readiness probe; `engine_socket_ready` checks the host socket afterward.

`inspect --json` includes the saved attempt under `last_start`, with stages marked `running`, `complete`, `failed`, or `skipped`. An interrupted command can leave a stage marked `running`. Report write failures produce warnings; invalid reports appear in inspection errors. Use live readiness fields to check current health.

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

`luhmen docker ...` also selects this context and clears Docker endpoint overrides. It uses a separate Buildx store under the luhmen state directory at `buildx` and selects the `luhmen` builder. This keeps builds on the context's default Docker driver even when your regular Buildx configuration has a different selected builder or a builder with the same name.

The wrapper rejects explicit builder overrides. Use `docker --context luhmen ...` directly to manage custom builders; their selection and endpoints are then controlled by Docker and Buildx.

Publish container ports with `-p 127.0.0.1:8080:80` for local access. Port forwarding, guest DNS, and proxy handling use Lima's host integration. VPNs, proxies, corporate DNS, and overlapping address ranges can affect connectivity. See [platforms and versions](support.md) for limits.

Local HTTPS uses a separate proxy in front of explicitly configured localhost ports. It does not expose the Docker API over TCP. See [local HTTPS](https.md).

## Recovery and diagnostics

Start with `luhmen inspect --json` and `luhmen doctor --json`. Check free disk space and logs under the state directory after provisioning or boot failures. Correct missing dependencies or network problems, then retry `start`.

| Symptom | Next step |
| --- | --- |
| `luhmen` or `limactl` is not found | Restore the install directories to `PATH`, as shown in [installation](install.md). |
| `doctor` reports a different Lima version | Put the pinned Lima installation first on `PATH`, or set `LUHMEN_LIMACTL` to its executable. |
| `doctor` cannot find Compose or Buildx | Run `docker compose version` and `docker buildx version`, then check the plugin installation and `DOCKER_CONFIG`. |
| The Docker context belongs to another state directory | Restore the `LUHMEN_HOME` and `DOCKER_CONFIG` used to create that VM. Inspect the existing context before changing it. |
| A mount directory is missing | Restore the directory at its original path before starting. Inspection and shutdown still work. |
| A container reports `exec format error` | Check that its image supports Linux arm64. x86 emulation is disabled. |

If the guest is running but Docker Engine is unavailable, inspect its service log:

```sh
luhmen shell sudo journalctl -u docker.service --no-pager -n 100
```

Before attaching diagnostics to a public issue, remove credentials, private registry names, private project paths, and workload data. Do not attach the state directory or Docker's `config.json`.

Use `stop` before repairing Lima configuration or copying the disk for backup. `stop --force` can terminate a stuck VM, but may lose unwritten guest data and cannot repair configuration. Do not delete the VM disk to recover from a startup error.

`luhmen storage --json` reports logical and allocated bytes for VM files and Lima's shared cache. See [storage](storage.md) for accounting limits and cache placement.

`luhmen shell COMMAND...` runs a guest command, for example `luhmen shell uname -a`.

An interrupted build or Compose operation may leave normal Docker resources behind. Inspect them with `luhmen docker ps -a`, `luhmen docker volume ls`, and `luhmen docker compose ls`. Remove only resources you recognize. luhmen does not run automatic Docker pruning.
