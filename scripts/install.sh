#!/bin/sh
set -eu

if [ "$#" -ne 2 ] || [ "$1" != '--prefix' ]; then
  printf '%s\n' 'Usage: scripts/install.sh --prefix /absolute/directory' >&2
  exit 2
fi
prefix=$2
case "$prefix" in /*) ;; *) printf '%s\n' 'The prefix must be an absolute path.' >&2; exit 2 ;; esac
repo=$(CDPATH='' cd -- "$(dirname -- "$0")/.." && pwd -P)
cd "$repo"
executable=$(cargo devtool build-release)
licenses=$(mktemp -d "${TMPDIR:-/tmp}/luhmen-licenses.XXXXXX")
trap 'rm -rf "$licenses"' EXIT
trap 'exit 129' HUP
trap 'exit 130' INT
trap 'exit 143' TERM
cargo devtool collect-licenses --output "$licenses/dependencies"
mkdir -p "$prefix/bin" "$prefix/share/licenses/luhmen"
install -m 755 "$executable" "$prefix/bin/luhmen"
install -m 644 LICENSE NOTICE THIRD_PARTY.md "$prefix/share/licenses/luhmen/"
cp -R "$licenses/dependencies" "$prefix/share/licenses/luhmen/"
printf 'Installed luhmen in %s/bin/luhmen\n' "$prefix"
