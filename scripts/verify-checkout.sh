#!/bin/sh
set -eu

repo=$(CDPATH='' cd -- "$(dirname -- "$0")/.." && pwd -P)
if [ -n "$(git -C "$repo" status --porcelain --untracked-files=all)" ]; then
  printf '%s\n' 'Commit changes before running clean-checkout verification.' >&2
  exit 1
fi
work=$(mktemp -d "${TMPDIR:-/tmp}/luhmen-checkout.XXXXXX")
trap 'rm -rf "$work"' EXIT
trap 'exit 129' HUP
trap 'exit 130' INT
trap 'exit 143' TERM
git clone --quiet --no-local "$repo" "$work/source"
cd "$work/source"
export CARGO_TARGET_DIR="$work/target"
cargo devtool check-source
cargo fmt --all -- --check
cargo clippy --locked --workspace --all-targets --all-features -- -D warnings
cargo test --locked --workspace --all-features
./scripts/install.sh --prefix "$work/install"
"$work/install/bin/luhmen" --version
"$work/install/bin/luhmen" --help
test -s "$work/install/share/licenses/luhmen/dependencies/index.json"
cargo package --locked --package luhmen
cargo devtool check-cargo-package "$work/target/package"/*.crate
printf '%s\n' 'Clean-checkout build, tests, installation, licenses, and CLI help passed.'
