# Installation

## Build from source

Install the macOS Command Line Tools with `xcode-select --install`, Rust through [rustup](https://rustup.rs/), and Python 3.11 or newer. The repository's `rust-toolchain.toml` selects Rust 1.95.0. The build needs network access once to download the toolchain and locked Cargo dependencies.

Run these commands from the repository root:

```sh
./scripts/install.sh --prefix "$HOME/.local"
export PATH="$HOME/.local/bin:$PATH"
```

The installer builds with `cargo build --release --locked` and copies the executable and license files below the supplied prefix. It does not start a VM or change shell configuration. Set `CARGO_TARGET_DIR` to reuse a build directory.

## Lima

luhmen requires exactly Lima 2.2.0. Its [upstream release](https://github.com/lima-vm/lima/releases/tag/v2.2.0) supplies the Apple Virtualization.framework implementation and guest integration files.

```sh
./scripts/install-lima.sh --prefix "$HOME/.local/opt/luhmen-lima"
export PATH="$HOME/.local/opt/luhmen-lima/bin:$PATH"
limactl --version
```

This script downloads the pinned Darwin arm64 archive, checks its SHA-256, and installs the complete distribution in an empty directory. Keep the `bin` and `share` directories together. luhmen uses its own Lima home and instance; it does not adopt machines from an existing Lima installation.

## Docker client and plugins

Install Docker CLI 29.4.0, Compose 5.1.2, and Buildx 0.33.0 from upstream releases or your package manager. Docker Desktop is not required. luhmen can use an existing Docker CLI without changing its active context or credentials.

- [Docker CLI static binary installation](https://docs.docker.com/engine/install/binaries/#install-client-binaries-on-macos)
- [Docker Compose 5.1.2 release](https://github.com/docker/compose/releases/tag/v5.1.2)
- [Docker Buildx 0.33.0 release](https://github.com/docker/buildx/releases/tag/v0.33.0)

Docker plugins normally live in `$HOME/.docker/cli-plugins`. Follow each upstream project's macOS installation instructions, then check discovery:

```sh
docker --version
docker compose version
docker buildx version
luhmen doctor
```

Lima and Docker plugins are separate dependencies; Cargo does not install them. `doctor` checks that the client and both plugins run, but does not enforce their selected versions. See [platforms and versions](support.md) for compatibility limits.

VM creation downloads a checksum-pinned Ubuntu image. The first start installs guest packages and a checksum-pinned Docker Engine archive. Missing upstream artifacts prevent new VM preparation; use a luhmen version with updated pins. Existing VMs reuse their installed image and Engine.

## Release archives

There are no published release archives yet. Build from source using the instructions above.

When a release is available, download its `luhmen-<version>-aarch64-apple-darwin.tar.gz` archive and `SHA256SUMS` from the same release. Set `luhmen_version` to the downloaded version and verify the archive before extracting it:

```sh
luhmen_version=0.1.0
luhmen_package="luhmen-${luhmen_version}-aarch64-apple-darwin"
shasum -a 256 "${luhmen_package}.tar.gz"
```

Compare the result with the matching line in `SHA256SUMS`. If it matches, extract and install:

```sh
tar -xzf "${luhmen_package}.tar.gz"
mkdir -p "$HOME/.local/bin" "$HOME/.local/share/licenses/luhmen"
install -m 755 "$luhmen_package/bin/luhmen" "$HOME/.local/bin/luhmen"
cp -R "$luhmen_package/share/licenses/luhmen/." "$HOME/.local/share/licenses/luhmen/"
export PATH="$HOME/.local/bin:$PATH"
luhmen --version
```

Keep the included license and notice files when redistributing the archive or binary. Lima and Docker clients are installed separately. Binary archives do not contain the source build scripts; use the source archive or a Git checkout to build from source.

## Updating

Rebuilding and rerunning the source installer replaces the CLI in the chosen prefix. Stop an active HTTPS daemon before replacing its executable, then restart it afterward.

Updating the CLI does not upgrade an existing VM's guest image, package snapshot, or Docker Engine. There is no automatic VM upgrade or migration command. Read the version's release notes before updating and back up persistent data before any manual VM replacement.

## Uninstall

Stop the runtime with `luhmen stop`, then remove the installed executable and the `share/licenses/luhmen` directory from the prefix you supplied. Removing the executable does not remove VM data, Docker contexts, certificates, or credentials. Keep a backup of persistent data before manually removing the luhmen state directory. The default location is `$HOME/.local/share/luhmen`.
