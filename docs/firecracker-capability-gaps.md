# Firecracker capability gaps

This note compares luhmen's first nested Firecracker slice with the native Firecracker surface. It is a capability inventory, not a claim that the current implementation is production-ready.

## Current baseline

luhmen provisions pinned Firecracker 1.16.1 aarch64, launches it through the matching jailer, and configures one kernel, one root filesystem, a vCPU/memory pair, and `InstanceStart`. The host-facing manager exposes only `capabilities`, `create`, `start`, `stop`, and `inspect`. The current slice intentionally has no networking, guest agent, guest exec, snapshots, automatic host mounts, or Docker integration.

Firecracker's native model is broader. Its device/API matrix includes block, network, vsock, entropy, balloon, pmem, memory hotplug, MMDS, metrics, logging, pause/resume, and snapshot operations. See the [native device/API matrix](https://github.com/firecracker-microvm/firecracker/blob/main/docs/device-api.md) and [OpenAPI specification](https://github.com/firecracker-microvm/firecracker/blob/main/src/firecracker/swagger/firecracker.yaml).

## Missing native capabilities

- Networking: no TAP device, network namespace, virtio-net interface, firewall integration, or network rate limiter.
- Guest communication: no virtio-vsock device, serial/console stream, or guest agent. luhmen can report VMM start but cannot report guest OS readiness or run a command inside the guest.
- Storage: only one file-backed root drive is configured. There are no additional drives, read-only policy, block rate limiters, async I/O, or vhost-user block devices.
- Devices: no virtio-rng/entropy, balloon, pmem, PCI transport, MMDS, or memory hotplug. The boot path uses legacy MMIO and passes `pci=off`; Firecracker documents PCI as optional but recommends it for higher throughput in supported guests. See [getting started](https://github.com/firecracker-microvm/firecracker/blob/main/docs/getting-started.md?plain=1).
- Control and observability: no native `/version`, `/vm`, logger, metrics, live configuration updates, or pause/resume. Stop terminates the VMM process; it cannot request a clean ARM64 guest shutdown.
- Snapshots: no full or diff snapshot creation, pause, restore, or dirty-page tracking. Firecracker requires a paused VM for snapshot creation and a fresh process for snapshot loading. See [snapshot support](https://github.com/firecracker-microvm/firecracker/blob/main/docs/snapshotting/snapshot-support.md).
- Portability: luhmen supports the aarch64 guest path only. Native Firecracker also supports x86_64.

## Correctness and production-operation gaps

- `capabilities` reports KVM, cgroup, binary, and parent-resource diagnostics. `start` fails before invoking the jailer when `/dev/kvm`, delegated cgroup v2 controllers, Firecracker, or jailer are unavailable. An M4 test booted a real nested ARM64 guest, kept it alive after the start reply, and verified synced disk writes across stop/start, including a write beyond 64 MiB. This does not establish hostile-workload isolation or guest readiness reporting.
- Each microVM receives a dedicated unprivileged UID/GID from a managed range. Firecracker's production guidance recommends a dedicated UID/GID per VM and trusted, non-writable jailer paths. See [jailer guidance](https://github.com/firecracker-microvm/firecracker/blob/main/docs/jailer.md) and [production host setup](https://github.com/firecracker-microvm/firecracker/blob/main/docs/prod-host-setup.md).
- The jailer creates a cgroup-v2 child with CPU, memory, swap, and PID limits. Memory admission charges 128 MiB of VMM overhead per child and retains 512 MiB for the parent. Concurrent Docker workloads can still exhaust parent resources. Disk I/O throttling is missing.
- A separate output drain retains the first 64 MiB per start and discards later output. There is no process file-size limit on guest disk writes. Log rotation and centralized metrics are missing.
- Recovery is local to the manager process. There is no durable cgroup-based reconciliation, watchdog, native Firecracker state query, or automatic child recovery after the parent Lima VM disappears. State can be marked stale and requires explicit operator action.
- The manager is a root-owned shell helper behind `socat`, with a small custom protocol rather than a typed guest daemon or native API proxy. The socket has no authentication and is intended only for trusted local development.

## Recommended order

1. Automate M3/M4 nested-KVM acceptance tests and add durable cgroup reconciliation.
2. Replace the shell manager with a typed guest daemon and expose `/version`, `/vm`, metrics, live configuration updates, pause/resume, and the remaining signal/action surface.
3. Add vsock plus a guest agent for readiness and exec.
4. Add snapshot/restore.
5. Add networking, firewall policy, TAP lifecycle, and device/storage rate limiting.

The current implementation is therefore best described as minimal nested Firecracker cold-boot support, not full native Firecracker support.
