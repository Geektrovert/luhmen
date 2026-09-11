# Third-party software

luhmen is Apache-2.0 software. Each dependency keeps its own license and copyright notices.

## External runtime components

These components are installed or downloaded separately. They are not included in the luhmen binary archive.

| Component | Role | Upstream license and source |
| --- | --- | --- |
| Lima | VM lifecycle and host integration | [Apache-2.0](https://github.com/lima-vm/lima/blob/v2.2.0/LICENSE), [source](https://github.com/lima-vm/lima/tree/v2.2.0) |
| Moby / Docker Engine | Container Engine API and daemon | [Apache-2.0](https://github.com/moby/moby/blob/master/LICENSE), [source](https://github.com/moby/moby) |
| containerd | Container runtime | [Apache-2.0](https://github.com/containerd/containerd/blob/main/LICENSE), [source](https://github.com/containerd/containerd) |
| BuildKit | Image builds | [Apache-2.0](https://github.com/moby/buildkit/blob/master/LICENSE), [source](https://github.com/moby/buildkit) |
| Docker CLI | Host client | [Apache-2.0](https://github.com/docker/cli/blob/master/LICENSE), [source](https://github.com/docker/cli) |
| Docker Compose | Compose client plugin | [Apache-2.0](https://github.com/docker/compose/blob/main/LICENSE), [source](https://github.com/docker/compose) |
| Docker Buildx | Build client plugin | [Apache-2.0](https://github.com/docker/buildx/blob/master/LICENSE), [source](https://github.com/docker/buildx) |
| Ubuntu | Guest operating system | [Package copyright information](https://www.ubuntu.com/legal/intellectual-property-policy), [source archives](https://archive.ubuntu.com/ubuntu/) |

The guest contains additional distribution packages under their respective licenses. Package copyright files are available under `/usr/share/doc` in the VM. Distribution package sources are available through the corresponding Ubuntu and Docker upstream repositories. Redistributing a guest disk is outside the binary release process and requires its own complete license and source-offer review.

## Rust dependencies

`Cargo.lock` pins Rust dependencies. `scripts/collect-licenses.py` reads Cargo's locked dependency metadata for the selected platform and copies the license, copyright, and notice files distributed in each dependency's source package. Binary releases include an index and these actual texts under `share/licenses/luhmen/dependencies`.

The published `asn1-rs-impl` 0.2.0 crate omits its upstream license texts. The `licenses/asn1-rs-impl-0.2.0` directory supplies the unchanged MIT and Apache-2.0 texts from the exact source commit recorded in that crate. Its `SOURCE` file records the upstream URLs and accompanies those texts in release packages.

Release generation fails if a dependency has no discoverable license text. Review changes to the generated index when dependencies change. The index includes build and test dependencies present in Cargo metadata, so it can contain packages that are not linked into the executable.
