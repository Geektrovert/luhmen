# Storage

`luhmen storage --json` reads storage metadata without starting the VM or removing data.

## Reading the report

| Field | Meaning |
| --- | --- |
| `host_available_bytes` | Space currently available on the filesystem containing luhmen's state directory. Other applications can change this number during the scan. |
| `vm_directory.file_logical_bytes` | Sum of regular file lengths below the isolated VM directory. Sparse regions count toward this value. |
| `vm_directory.file_allocated_bytes` | Sum of allocated file blocks, in bytes, reported by the host filesystem. |
| `backing_images` | Separate logical and allocated byte counts for Lima's `disk`, setup `image`, and legacy `basedisk` and `diffdisk` names. Existing regular files are already included in the VM directory totals. |
| `shared_lima_cache` | Regular file totals below `$HOME/Library/Caches/lima`, used by Lima 2.2.0 on macOS. This cache can serve other Lima installations. `root_uid` and `root_gid` identify its filesystem owner; they do not establish exclusive ownership by luhmen. |

Configured disk capacity appears in `luhmen config show` as `disk_gib`. It limits the guest disk's capacity; it does not reserve that amount of host storage or describe free guest space. A 30 GiB sparse image can occupy much less than 30 GiB on the Mac. Guest files, filesystem metadata, and container layers determine how the image grows.

Allocated bytes come from filesystem block counts. APFS clones can share blocks and snapshots can retain deleted data, so these counts do not predict how much space deletion will free.

Each scan visits at most 10,000 entries and 64 directory levels, with a two-second budget checked between filesystem calls. A stalled call can exceed that budget. Missing directories report `exists: false`. Check `complete` and `errors` before comparing totals; incomplete scans report only files reached.

The scan counts regular files once per directory tree. It skips symlinks and special files, rejects symlinked roots and ancestors, and stays on one filesystem. Stop the VM for stable backing-image measurements.

Deleting guest data or running guest trim may not reduce the image's host allocation. Check macOS free space after cleanup. luhmen does not automate disk reclamation or cache removal.

## Keep source on the Mac and build data in volumes

Docker named volumes can hold dependency caches and build output while source stays in a Mac bind mount. Those caches consume VM disk space.

For a Rust project, share its source directory when creating the VM, then add this `compose.yaml`. Choose the Rust image version required by your project.

```yaml
services:
  build:
    image: rust:1.95.0
    working_dir: /workspace
    environment:
      CARGO_HOME: /cargo-cache
      CARGO_TARGET_DIR: /build-output
    volumes:
      - type: bind
        source: .
        target: /workspace
        read_only: true
      - cargo-cache:/cargo-cache
      - build-output:/build-output
    command: cargo build --locked

volumes:
  cargo-cache:
  build-output:
```

Run the build with an explicit Docker context and project name:

```sh
docker --context luhmen compose --project-name example-build run --rm build
```

Commit `Cargo.lock` before using `--locked`. This source mount is read-only; builds that write into it need a writable share. The two volumes persist across container removal and VM restart. Export files from `/build-output` through a writable bind mount when you need them on the Mac.

The image runs as root. If using a different UID, initialize volume permissions for it before building. The example does not share host Cargo credentials.

Mounts hide files already at the target path. Review paths before adapting this layout. See [Docker volume behavior](https://docs.docker.com/engine/storage/volumes/).

To delete this example's caches and build output, run from its Compose directory:

```sh
docker --context luhmen compose --project-name example-build down --volumes
```

Do not reuse the example project name for a project with persistent application data. Normal `compose down` without `--volumes` preserves named volumes.
