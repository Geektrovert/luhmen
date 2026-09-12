# Platforms and versions

luhmen requires an Apple Silicon Mac running macOS 14 or newer. macOS 14 is the minimum deployment target, but has not been verified with a live VM. Intel Macs, Linux hosts, and Windows hosts are unsupported. Rust tests can run on Linux without virtualization.

luhmen is a development preview. The CLI, saved configuration, and JSON output can change before a stable release. There is no compatibility or migration guarantee between development commits. Automated checks cover CLI behavior and packaging; they do not establish VM behavior on every supported macOS version.

## Version policy

| Component | Required or selected version |
| --- | --- |
| Rust compiler | 1.95.0 |
| Lima | 2.2.0, exact |
| Docker CLI | 29.4.0 |
| Docker Compose | 5.1.2 |
| Docker Buildx | 0.33.0 |
| Guest image | Ubuntu 24.04 minimal arm64, release 20260905 |
| Ubuntu package snapshot | 20260906T000000Z |
| Docker Engine | 29.8.0 |
| containerd | 2.3.4, bundled with Docker Engine |
| runc | 1.5.1, bundled with Docker Engine |
| BuildKit | 0.33.0, bundled with Docker Engine |
| Firecracker | 1.16.1 aarch64, nested mode only |
| macOS deployment target | 14.0 |
| Python for packaging | 3.11 or newer |

Lima must match exactly. Other Docker client versions may work through Engine API negotiation, but compatibility is not guaranteed. The guest image and Engine archive have pinned SHA-256 hashes. Guest packages use the dated Ubuntu snapshot. Ordinary start and stop commands do not upgrade an existing VM.

The Homebrew installation was also verified on macOS 26.6.2 with Docker CLI 29.8.0, Compose 5.5.1, and Buildx 0.37.0. Fresh VM creation, container execution, Compose, Buildx builds, restart persistence, and Docker daemon recovery passed. The file-watcher limitations below still apply. Homebrew can update these client packages independently; newer versions need their own compatibility check.

## Current limits

- The guest and containers use Linux arm64. Rosetta and x86 emulation are disabled; use images with an arm64 variant.
- CPU, memory, disk, and mount settings are fixed at VM creation.
- Writable mount event forwarding is experimental. Host changes produce `IN_ATTRIB` notifications; `IN_MODIFY` and exact create/rename/delete event sequences are not guaranteed. Use application polling for complete change detection.
- Local HTTPS buffers HTTP/1 uploads and streams responses, with body-size and time limits. It does not support WebSockets or CONNECT. Certificate trust is manual.
- VPN transitions, split DNS, authenticated proxies, IPv6, UDP forwarding, and sleep/wake behavior are unverified.
- Backups, data migration, and disk reclamation are manual.
- Firecracker nested virtualization requires an Apple M3 or newer running macOS 15 or later and a VM created with `--nested-virtualization`. The nested VMM path has not been validated as a hostile-workload boundary.
- Child Firecracker VMs use distinct unprivileged jailer identities, cgroup CPU/memory/swap/PID limits, parent-resource admission, and a bounded VMM log. Disk I/O throttling and durable post-crash reconciliation are not included.
- The current Firecracker slice supports no networking, guest agent, guest exec, snapshots, automatic host mounts, or Docker integration. It reports Firecracker VMM start, not guest OS readiness.

Bug reports should include the source commit or `luhmen --version`, macOS version, and redacted `luhmen doctor --json` and `luhmen inspect --json` output. Use [GitHub issues](https://github.com/Geektrovert/luhmen/issues) for bugs and feature requests and the [security policy](../SECURITY.md) for vulnerabilities.

See [runtime behavior](runtime.md), [local HTTPS](https.md), and [contributing](../CONTRIBUTING.md) for usage and checks.
