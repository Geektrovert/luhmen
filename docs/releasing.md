# Releasing

Release builds require an Apple Silicon Mac, the pinned Rust toolchain, Apple's Command Line Tools, Git, and Python 3.11 or newer. The source checkout must be committed and clean. Run the checks in [contributing](../CONTRIBUTING.md), including VM integration checks, before building a release.

```sh
./scripts/verify-checkout.sh
python3 scripts/release.py --output /absolute/path/to/new-release-directory
```

The release script builds for `aarch64-apple-darwin` with `cargo build --release --locked`, targets macOS 14.0, strips symbols, and remaps local source paths. It collects the license files of locked Cargo dependencies for the target. Missing license text fails the release.

The output contains a binary archive with documentation and licenses, a source archive of Git-tracked files, and `SHA256SUMS`. Archives use sorted entries, fixed permissions, owner and group 0, and a normalized timestamp. `SOURCE_DATE_EPOCH` defaults to the source commit's timestamp. `build-info.json` records the source commit, source timestamp, Rust version, target, SDK version, and deployment target.

Build twice using the same source commit, Rust toolchain, macOS SDK, Cargo dependencies, and `SOURCE_DATE_EPOCH`, then compare `SHA256SUMS`. Record the comparison result with the release. Different SDK or linker versions may produce different executable bytes.

Extract the source archive into a fresh directory and run `cargo test --locked` and `scripts/install.sh --prefix /absolute/temporary/prefix` to verify the shipped source. This must work without a Git directory. Git is required only for producing releases and running the clean-checkout helper.

The script does not sign, notarize, upload, or publish artifacts. If signing or notarizing, record that step and regenerate checksums for the final files. Packages do not bundle Lima, Docker client tools, or a VM disk.
