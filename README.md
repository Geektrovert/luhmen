# luhmen

luhmen runs Docker Engine and optional Firecracker microVMs on Apple Silicon Macs. Its Rust CLI manages a dedicated Linux VM through Lima and Apple's Virtualization.framework. Docker CLI, Compose, Buildx, and SDK clients use the Engine API through the `luhmen` Docker context.

On supported Macs, Firecracker runs inside that Linux VM using nested hardware virtualization. Each microVM boots its own Linux kernel and has a private writable disk. Both the parent VM and Firecracker guests run ARM64 code with Rosetta disabled.

This is a development preview for local development. Expect CLI and configuration changes before a stable release. See [supported platforms, versions, and limits](docs/support.md).

## Requirements

- An Apple Silicon Mac with macOS 14 or newer.
- [Homebrew](https://brew.sh/) for the recommended installation. It installs the prebuilt CLI, pinned Lima runtime, and Docker client tools.
- At least 15 GiB of free disk space to create the VM.
- Internet access for VM creation, first-start guest packages, and container images.

## Install and start

Install from the tap. No Rust build is needed:

```sh
brew install Geektrovert/tap/luhmen
```

Follow the [one-time Docker plugin setup](docs/install.md#homebrew), then start the VM:

```sh
luhmen doctor
mkdir -p "$HOME/projects"
luhmen create --cpus 4 --memory 4 --disk 30 --mount "$HOME/projects:rw"
luhmen start
luhmen docker run --rm hello-world
```

The last command should print `Hello from Docker!`. The first create and start download the guest image and packages and can take several minutes. If `doctor` fails, fix the reported dependency before creating the VM.

For other installation methods, see [release archives](docs/install.md#release-archives) or [building from source](docs/install.md#build-from-source).

Replace `$HOME/projects` with the directory you want to share, or omit `--mount` if you do not need host files. Mounts are read-only unless suffixed with `:rw`. Choose resources and mounts before creation; these settings cannot be changed afterward. Memory and disk arguments use GiB.

Images, containers, and named volumes persist across VM stops and restarts. luhmen leaves Docker's active context unchanged. Use `luhmen docker ...` or `docker --context luhmen ...`.

```sh
luhmen inspect --json
luhmen storage --json
luhmen docker compose up -d
luhmen docker buildx build --load -t my-app .
luhmen restart
luhmen stop
```

`luhmen create --dry-run` prints the VM configuration without creating state. See [runtime behavior](docs/runtime.md) for configuration, mounts, ports, and recovery; [storage](docs/storage.md) for disk usage; and [local HTTPS](docs/https.md) for domains and certificates.

## Firecracker microVMs

Firecracker lets you boot your own Linux kernel and root filesystem inside the luhmen VM. Docker containers share the parent VM's kernel; each Firecracker microVM has its own. luhmen manages Firecracker and its jailer, assigns CPU and memory limits, and retains each microVM's disk across starts.

This requires an Apple M3 or newer running macOS 15 or later. Firecracker support is available in the [current source build](docs/install.md#build-from-source); the v0.1.0 release archives and Homebrew package do not include it.

Enable nested virtualization when you first create the parent VM. This setting cannot be added to an existing VM. The parent must be running before using any `luhmen microvm` command:

```sh
luhmen create --nested-virtualization --cpus 4 --memory 4 --disk 30
luhmen start
luhmen microvm capabilities --json
```

Supply an uncompressed ARM64 Linux kernel and an ext4 root filesystem. Both must be regular files at absolute paths inside the parent VM, with no whitespace, quotes, or backslashes in their paths. You can use files already in the guest or expose a host directory with `--mount` when creating the parent VM. luhmen does not download or build these guest images for you.

Register a microVM with those paths, then start it:

```sh
luhmen microvm create demo \
  --kernel /path/in/the/lima-vm/vmlinux \
  --rootfs /path/in/the/lima-vm/rootfs.ext4 \
  --vcpus 1 --memory-mib 512
luhmen microvm start demo
luhmen microvm inspect demo --json
luhmen microvm stop demo
```

`create` registers the configuration. The first `start` copies the root filesystem into a private writable disk; later starts reuse that disk. Keep both original image paths available for every start. A successful start confirms that Firecracker accepted the boot request, not that the guest OS has finished booting. Omit the ID from `inspect` to see all registered microVMs.

`stop` terminates Firecracker and keeps its disk. It does not ask the guest OS to shut down, so use your own guest tooling to shut down or sync writes first when persistence matters.

`capabilities --json` reports KVM, cgroup controllers, Firecracker binaries, and available parent resources. Starts require enough parent CPU and memory capacity. These checks account for registered microVMs; leave room for concurrent Docker workloads too. Each microVM runs under its own unprivileged jailer identity with cgroup resource limits and bounded logs.

The current microVMs have no networking, guest exec, snapshots, automatic host mounts, or Docker integration. This path is for trusted local development and is not a security boundary for hostile or mutually untrusted workloads. See [runtime behavior](docs/runtime.md#nested-firecracker-microvms) for admission limits, persistence, and recovery.

## Before using it

- Containers run as Linux arm64. Rosetta and x86 emulation are disabled.
- File contents sync through VirtioFS. Lima forwards host changes as attribute notifications; development servers that require `MODIFY` events or reliable create/rename/delete detection need polling.
- VM upgrades, data migration, backups, and disk reclamation are manual. Keep important data backed up.
- Local HTTPS supports HTTP/1. WebSockets and CONNECT tunnels are unsupported. See [HTTPS behavior and limits](docs/https.md#behavior-and-limits).

See [troubleshooting](docs/runtime.md#recovery-and-diagnostics) for common failures. Report bugs and propose changes through [GitHub issues](https://github.com/Geektrovert/luhmen/issues). Report vulnerabilities according to the [security policy](SECURITY.md).

## Build and contribute

Source builds require Rust 1.95.0 through [rustup](https://rustup.rs/), Apple's Command Line Tools, and Python 3.11 or newer. See [source installation](docs/install.md#build-from-source).

Run these checks from a source checkout. Binary release archives do not contain the build scripts or Rust source.

```sh
cargo build --locked
cargo test --locked
cargo fmt --all -- --check
cargo clippy --locked --all-targets -- -D warnings
```

See [contributing](CONTRIBUTING.md) for integration checks and [releasing](docs/releasing.md) for reproducible packages.

## License

luhmen is licensed under [Apache-2.0](LICENSE). External runtime components keep their upstream licenses. See [third-party notices](THIRD_PARTY.md).
