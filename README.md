# luhmen

luhmen runs Docker Engine in a dedicated Linux VM on Apple Silicon Macs. Its Rust CLI manages Lima with Apple's Virtualization.framework. Docker CLI, Compose, Buildx, and SDK clients use the Engine API through the `luhmen` Docker context.

This is a development preview for local development. Expect CLI and configuration changes before a stable release. See [supported platforms, versions, and limits](docs/support.md).

## Requirements

- An Apple Silicon Mac with macOS 14 or newer.
- Rust 1.95.0 through [rustup](https://rustup.rs/), Apple's Command Line Tools, and Python 3.11 or newer to build from source.
- Lima 2.2.0 and Docker CLI with Compose and Buildx plugins. See [installation](docs/install.md).
- At least 15 GiB of free disk space to create the VM.
- Internet access for VM creation, first-start guest packages, and container images.

## Install and start

For prebuilt macOS arm64 packages, see [GitHub Releases](https://github.com/Geektrovert/luhmen/releases) and the [archive installation instructions](docs/install.md#release-archives).

Install Rust, Python, and the [Docker client and plugins](docs/install.md#docker-client-and-plugins) first. Then clone and build:

```sh
git clone https://github.com/Geektrovert/luhmen.git
cd luhmen
./scripts/install-lima.sh --prefix "$HOME/.local/opt/luhmen-lima"
export PATH="$HOME/.local/opt/luhmen-lima/bin:$HOME/.local/bin:$PATH"
./scripts/install.sh --prefix "$HOME/.local"
luhmen doctor
mkdir -p "$HOME/projects"
luhmen create --cpus 4 --memory 4 --disk 30 --mount "$HOME/projects:rw"
luhmen start
luhmen docker run --rm hello-world
```

The last command should print `Hello from Docker!`. The first create and start download the guest image and packages and can take several minutes. If `doctor` fails, fix the reported dependency before creating the VM. The `export` above applies to the current terminal; add it to your shell configuration to use the same installation in new terminals.

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

## Before using it

- Containers run as Linux arm64. Rosetta and x86 emulation are disabled.
- File contents sync through VirtioFS, but host file-removal events do not reach guest watchers. Development servers may need polling.
- VM upgrades, data migration, backups, and disk reclamation are manual. Keep important data backed up.
- Local HTTPS is optional and does not support WebSockets or streaming.

See [troubleshooting](docs/runtime.md#recovery-and-diagnostics) for common failures. Report bugs and propose changes through [GitHub issues](https://github.com/Geektrovert/luhmen/issues). Report vulnerabilities according to the [security policy](SECURITY.md).

## Build and contribute

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
