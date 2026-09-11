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
python3 scripts/check-source.py
PYTHONDONTWRITEBYTECODE=1 python3 -m unittest discover -s scripts -p 'test_*.py'
cargo fmt --all -- --check
cargo clippy --locked --all-targets -- -D warnings
cargo test --locked
./scripts/install.sh --prefix "$work/install"
"$work/install/bin/luhmen" --version
"$work/install/bin/luhmen" --help
test -s "$work/install/share/licenses/luhmen/dependencies/index.json"
printf '%s\n' 'Clean-checkout build, tests, installation, licenses, and CLI help passed.'
