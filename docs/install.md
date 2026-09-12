# Installation

## Homebrew

On an Apple Silicon Mac running macOS 14 or newer:

```sh
brew install Geektrovert/tap/luhmen
```

The [tap](https://github.com/Geektrovert/homebrew-tap) downloads the published luhmen binary and a checksum-pinned Lima 2.2.0 distribution. No Rust build is needed. Lima stays inside luhmen's Homebrew installation, so another Lima installation or upgrade does not change its runtime. `LUHMEN_LIMACTL` still overrides that default when explicitly set.

Homebrew also installs Docker CLI, Compose, and Buildx. The luhmen wrapper selects Homebrew's Docker CLI unless `LUHMEN_DOCKER` is explicitly set. Docker needs one configuration change to discover Homebrew's plugins. Run `brew --prefix` to find the Homebrew prefix, then add its `lib/docker/cli-plugins` directory to `cliPluginsExtraDirs` in `${DOCKER_CONFIG:-$HOME/.docker}/config.json`. Create the parent directory if it does not exist. For the standard Apple Silicon prefix, a new configuration file looks like this:

```json
{
  "cliPluginsExtraDirs": [
    "/opt/homebrew/lib/docker/cli-plugins"
  ]
}
```

If the file already exists, merge this property into the existing JSON object and retain its other settings. If `cliPluginsExtraDirs` already exists, add the Homebrew path to that array without removing its existing entries. Use the actual output of `brew --prefix` instead of `/opt/homebrew` if it differs. This is the setup recommended by the [Homebrew Compose](https://formulae.brew.sh/formula/docker-compose) and [Buildx](https://formulae.brew.sh/formula/docker-buildx) packages.

Check the installation before creating a VM:

```sh
luhmen --version
docker --version
docker compose version
docker buildx version
luhmen doctor
```

Homebrew manages the Docker client versions separately from luhmen. They may differ from the versions in the original release's VM checks; see [platforms and versions](support.md).

If an older source installation takes precedence, `command -v luhmen` will show it. Put Homebrew's `bin` directory earlier on `PATH`, or invoke `"$(brew --prefix)/bin/luhmen"` directly. Follow the [README](../README.md#install-and-start) to create and start the VM.

## Build from source

Install the macOS Command Line Tools with `xcode-select --install` and Rust through [rustup](https://rustup.rs/). The repository's `rust-toolchain.toml` selects Rust 1.95.0 and the Linux ARM64 guest target. The build needs network access once to download the toolchain, target, and locked Cargo dependencies.

Install the [Docker client and plugins](#docker-client-and-plugins), then clone and build:

```sh
git clone https://github.com/Geektrovert/luhmen.git
cd luhmen
./scripts/install-lima.sh --prefix "$HOME/.local/opt/luhmen-lima"
./scripts/install.sh --prefix "$HOME/.local"
export PATH="$HOME/.local/opt/luhmen-lima/bin:$HOME/.local/bin:$PATH"
luhmen doctor
```

The installer builds with `cargo build --release --locked` and copies the executable and license files below the supplied prefix. It does not start a VM or change shell configuration. Set `CARGO_TARGET_DIR` to reuse a build directory. The `export` applies to the current terminal; add it to your shell configuration to use the same installation in new terminals.

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

VM creation downloads a checksum-pinned Ubuntu image. The first start installs guest packages and a checksum-pinned Docker Engine archive. A VM created with `--nested-virtualization` also downloads the pinned Firecracker 1.16.1 aarch64 archive and retains its upstream notice files in the guest. Missing upstream artifacts prevent new VM preparation; use a luhmen version with updated pins. Existing VMs reuse their installed image, Engine, and Firecracker installation.

## Release archives

Find release downloads on [GitHub Releases](https://github.com/Geektrovert/luhmen/releases). Download the `luhmen-<version>-aarch64-apple-darwin.tar.gz` archive and `SHA256SUMS` from the same release. Set `luhmen_version` to the downloaded version and verify the archive before extracting it:

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

Stop an active HTTPS daemon before replacing its executable, then restart it afterward.

For a Homebrew installation:

```sh
brew update
brew upgrade Geektrovert/tap/luhmen
```

Rebuilding and rerunning the source installer replaces the CLI in the chosen prefix.

Replacing the CLI leaves an existing VM's saved provisioning unchanged. To apply the installed CLI's provisioning fixes, stop the VM and update it explicitly:

```sh
luhmen stop
luhmen update
luhmen start
```

`update` is available in the unreleased source tree. It replaces only provisioning, preserving the VM disk, base image, CPU, memory, mounts, and networking settings. The next start applies the current scripts and installs the selected Engine or helper version when needed. It does not replace the guest OS image or upgrade every guest package. Edited or unrecognized Lima templates are refused. If updating is interrupted between template writes, rerun the same CLI version before upgrading again.

## Uninstall

Stop the runtime with `luhmen stop`. For a Homebrew installation, then run:

```sh
brew uninstall Geektrovert/tap/luhmen
```

For source or archive installations, remove the installed executable and the `share/licenses/luhmen` directory from the prefix you supplied. Removing the executable does not remove VM data, Docker contexts, certificates, or credentials. Keep a backup of persistent data before manually removing the luhmen state directory. The default location is `$HOME/.local/share/luhmen`.
