# Installation

## Build from source

Install the macOS Command Line Tools with `xcode-select --install`, Rust through [rustup](https://rustup.rs/), and Python 3.11 or newer. The repository's `rust-toolchain.toml` selects Rust 1.95.0. The build needs network access once to download the toolchain and locked Cargo dependencies.

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

Lima and Docker plugins are separate dependencies; Cargo does not install them. See [platforms and versions](support.md) for compatibility limits.

VM creation downloads a checksum-pinned Ubuntu image. The first start installs guest packages and a checksum-pinned Docker Engine archive. Missing upstream artifacts prevent new VM preparation; use a luhmen version with updated pins. Existing VMs reuse their installed image and Engine.

## Release archives

Extract the `luhmen-<version>-aarch64-apple-darwin.tar.gz` archive after verifying its entry in `SHA256SUMS`, then copy `bin/luhmen` into a directory on `PATH`. Keep the included license and notice files when redistributing the archive or binary. Lima and Docker clients are installed separately.

## Uninstall

Stop the runtime with `luhmen stop`, then remove the installed executable and the `share/licenses/luhmen` directory from the prefix you supplied. Removing the executable does not remove VM data, Docker contexts, certificates, or credentials. Keep a backup of persistent data before manually removing the luhmen state directory. The default location is `$HOME/.local/share/luhmen`.
