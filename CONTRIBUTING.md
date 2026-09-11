# Contributing

Build with the Rust toolchain selected by `rust-toolchain.toml` and retain `Cargo.lock` changes when dependencies change. Host dependencies and OS requirements are in [installation](docs/install.md).

Run these checks before submitting a change:

```sh
cargo fmt --all -- --check
cargo clippy --locked --all-targets -- -D warnings
cargo test --locked
PYTHONDONTWRITEBYTECODE=1 python3 -m unittest discover -s scripts -p 'test_*.py'
```

`scripts/verify-checkout.sh` checks a clean, committed source tree in a temporary checkout. It runs the checks above, installs into a temporary prefix, checks dependency licenses, and runs CLI help. It does not start a VM.

Keep tests focused on observable behavior: generated VM settings, CLI argument rules, ownership checks, context selection, interrupted operations, and recovery. Live integration tests must use a dedicated state directory and their own Docker resources. Never rely on a contributor's current Docker context or require access to unrelated workloads.

luhmen's CLI and HTTPS daemon are Rust. The crate forbids unsafe code. Lima, Docker Engine, containerd, and BuildKit are external components and retain their upstream languages and licenses.

## VM integration checks

Changes to provisioning, mounts, networking, or lifecycle need checks on Apple Silicon. Use a dedicated VM with no existing containers and a writable host share:

```sh
python3 tests/vm-smoke.py --run --fixture-root /absolute/path/to/shared-fixtures --watchers --recovery
```

The suite builds and runs Compose workloads, checks Buildx, file visibility in both directions, named-volume persistence, DNS, localhost port reuse, and basic HTTP proxies. It restarts the VM. `--watchers` installs guest `inotify-tools` and checks filesystem events; `--recovery` kills Docker Engine to check service recovery.

Cleanup removes the suite's containers, volumes, networks, and image tags. Fixture files, downloaded base images, and build cache remain. The suite checks the context endpoint before each Docker command and refuses pre-existing containers.

Output is JSON Lines. Read every `limitation` and `not_run` result even when the suite passes. Missing optional watcher events are limitations. VPN transitions, split DNS, authenticated and registry proxies, HTTPS CONNECT, IPv6, UDP, external-peer port exposure, and sleep/wake need separate checks. Local interface probes do not establish reachability from another machine.

Include the tested macOS, Lima, Engine, and client versions in change descriptions, along with any skipped checks. See [releasing](docs/releasing.md) for source archive and package checks.

By contributing, you agree to license your contribution under Apache-2.0.
