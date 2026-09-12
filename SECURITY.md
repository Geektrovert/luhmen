# Security policy

## Reporting a vulnerability

Report vulnerabilities through the repository's [private report form](https://github.com/Geektrovert/luhmen/security/advisories/new). Include the affected commit or version, reproduction steps, expected impact, and any relevant logs with secrets removed.

If the form is unavailable, open an issue asking for a private contact method. Keep vulnerability details, exploit code, credentials, and private logs out of that public issue.

There are no stable releases or published security support periods yet.

## Runtime boundaries

luhmen is a local development tool. It runs Docker Engine with root privileges inside a Linux VM. Access to its Docker socket allows control of that VM and access to directories shared with it. Give socket access only to software and users you trust.

No host directories are shared by default. Share only directories the workload needs. The VM is not a validated boundary for running hostile workloads or hosting mutually untrusted users.

The optional nested Firecracker mode keeps the VMM, jailer, and manager inside the Lima guest. Its manager socket is exposed through the local Lima-forwarded socket and is intended only for trusted users and software. The current implementation has no socket authentication, networking, guest agent, or workload isolation validation for hostile or mutually untrusted microVMs. Do not use it as a multi-tenant boundary.

The optional HTTPS daemon listens on IPv4 loopback and proxies explicitly configured routes. It does not authenticate users. Its local certificate authority has a private key in the luhmen state directory. Protect that key and remove the CA from trust stores when retiring the runtime. See [local HTTPS](docs/https.md) for setup and limits.

The guest image, Docker Engine archive, and Ubuntu package snapshot are pinned. Starting an existing VM does not update those components. Replacing the CLI does not patch an existing guest; see [updating](docs/install.md#updating).
