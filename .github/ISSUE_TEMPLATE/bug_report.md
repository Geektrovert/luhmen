---
name: Bug report
about: Report a problem with luhmen
---

For vulnerabilities, follow the [security policy](https://github.com/Geektrovert/luhmen/blob/main/SECURITY.md) before posting details.

## What happened?

Describe what you expected and what happened instead.

## Reproduction

List the commands or steps needed to reproduce the problem. Include a minimal Compose file or gateway configuration if relevant.

## Environment

- luhmen version and source commit:
- macOS version and Mac model:
- Lima, Docker CLI, Compose, and Buildx versions:
- Custom `LUHMEN_HOME` or `DOCKER_CONFIG`, VPN, or proxy setup:

## Diagnostics

Include relevant output from `luhmen doctor --json` and `luhmen inspect --json`, plus the failing command's error. Remove credentials, private registry names, personal paths, and workload data first. Do not attach a VM disk, Docker's `config.json`, or the luhmen state directory.
