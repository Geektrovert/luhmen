# Platforms and versions

luhmen requires an Apple Silicon Mac running macOS 14 or newer. macOS 14 is the minimum deployment target, but has not been verified with a live VM. Intel Macs, Linux hosts, and Windows hosts are unsupported. Rust tests can run on Linux without virtualization.

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
| macOS deployment target | 14.0 |
| Python for packaging | 3.11 or newer |

Lima must match exactly. Other Docker client versions may work through Engine API negotiation, but compatibility is not guaranteed. The guest image and Engine archive have pinned SHA-256 hashes. Guest packages use the dated Ubuntu snapshot. Ordinary start and stop commands do not upgrade an existing VM.

## Current limits

- CPU, memory, disk, and mount settings are fixed at VM creation.
- Writable mount event forwarding is experimental and omits host file-removal events. Use application polling for complete change detection.
- Local HTTPS buffers HTTP/1 requests and responses. It does not support WebSockets, CONNECT, or streaming. Certificate trust is manual.
- VPN transitions, split DNS, authenticated proxies, IPv6, UDP forwarding, and sleep/wake behavior are unverified.
- Backups, data migration, and disk reclamation are manual.

See [runtime behavior](runtime.md), [local HTTPS](https.md), and [contributing](../CONTRIBUTING.md) for usage and checks.
