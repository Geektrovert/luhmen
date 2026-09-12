# Contributing

Use [GitHub issues](https://github.com/Geektrovert/luhmen/issues) for bugs and feature requests. For a substantial change, describe the problem and proposed behavior before starting implementation. Small fixes can go straight to a pull request. Follow the [security policy](SECURITY.md) for vulnerability reports.

Build with the Rust toolchain selected by `rust-toolchain.toml` and retain `Cargo.lock` changes when dependencies change. The toolchain file also installs the Linux ARM64 target used for the static guest helper. The TLS interoperability test requires OpenSSL. Host dependencies and OS requirements are in [installation](docs/install.md).

Run these checks before submitting a change:

```sh
cargo fmt --all -- --check
cargo clippy --locked --workspace --all-targets --all-features -- -D warnings
cargo test --locked --workspace --all-features
cargo devtool check-source
```

`scripts/verify-checkout.sh` checks a clean, committed source tree in a temporary checkout. It runs the checks above, installs into a temporary prefix, checks dependency licenses, and runs CLI help. It does not start a VM.

Keep tests focused on observable behavior: generated VM settings, CLI argument rules, ownership checks, context selection, interrupted operations, and recovery. Live integration tests must use a dedicated state directory and their own Docker resources. Never rely on a contributor's current Docker context or require access to unrelated workloads.

luhmen's CLI and HTTPS daemon are Rust. The crate forbids unsafe code. Lima, Docker Engine, containerd, and BuildKit are external components and retain their upstream languages and licenses.

## VM integration checks

Changes to provisioning, mounts, networking, or lifecycle need checks on Apple Silicon. Use a dedicated VM with no existing containers and a writable host share. Choose separate `LUHMEN_HOME` and `DOCKER_CONFIG` directories before creating it so an existing `luhmen` context keeps its original endpoint. Keep both environment variables set for the whole check. A separate Docker configuration also needs access to the Compose and Buildx plugins.

Build the candidate and use that executable for creation, startup, and the suite. The example below assumes Cargo's default `target` directory and an already running test VM:

```sh
cargo build --locked
cargo devtool vm-smoke --run --luhmen "$PWD/target/debug/luhmen" \
  --fixture-root /absolute/path/to/shared-fixtures --watchers --recovery --slow-shutdown
```

Replace the fixture path with a directory inside the VM's configured writable share. If you set `CARGO_TARGET_DIR`, adjust the executable path too. Do not run this suite against a VM used for daily work. Stop the test VM when finished, using the same state and Docker configuration settings.

The suite builds and runs a Compose workload, checks Buildx caching, file visibility in both directions, Compose and host-alias DNS, named-volume persistence, localhost HTTP, and restart behavior. It restarts the VM. `--watchers` installs guest `inotify-tools` and checks a host-write event; `--recovery` kills Docker Engine to check service recovery. `--slow-shutdown` checks that a container can spend 32 seconds flushing before the VM shuts down, exceeding Lima's own 30-second shutdown window.

Cleanup removes the suite's containers, volumes, networks, and image tags. Fixture files, downloaded base images, and build cache remain. The suite checks the context endpoint before each Docker command and refuses pre-existing containers.

Output is JSON Lines. Read every `limitation` and `not_run` result even when the suite passes. Missing optional watcher events are limitations. VPN transitions, split DNS, authenticated and registry proxies, HTTPS CONNECT, IPv6, UDP, external-peer port exposure, and sleep/wake need separate checks. Local interface probes do not establish reachability from another machine.

Include the tested macOS, Lima, Engine, and client versions in change descriptions, along with any skipped checks. See [releasing](docs/releasing.md) for source archive and package checks.

For repeatable performance measurements, use the same dedicated VM with no containers or running nested microVMs:

```sh
cargo devtool vm-perf --run --luhmen "$PWD/target/debug/luhmen" \
  --fixture-root /absolute/path/to/shared-fixtures --iterations 5 --pull
```

This records warm container launch times, matching bind-mount and named-volume file operations, loopback HTTP latency, and guest memory before and after an idle interval. It retains raw samples and environment details as JSON Lines. File operations include metadata scans, reads, writes, and tar/gzip/SHA256 work; they do not represent application compilation or prove near-native performance. The suite removes its labeled containers and volume. Fixture files and the pinned base image remain. Run it separately from the smoke suite and other workloads.

Pull requests should explain the user-visible change, list the checks run and their results, and call out remaining limitations. Update the relevant documentation when changing commands, defaults, or compatibility requirements. Avoid attaching credentials, personal paths, or unrelated workload logs.

By contributing, you agree to license your contribution under Apache-2.0.
