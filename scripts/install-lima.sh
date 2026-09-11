#!/bin/sh
set -eu

usage() { printf '%s\n' 'Usage: scripts/install-lima.sh --prefix /absolute/empty/directory'; }
if [ "$#" -ne 2 ] || [ "$1" != '--prefix' ]; then usage >&2; exit 2; fi
prefix=$2
case "$prefix" in /*) ;; *) printf '%s\n' 'The prefix must be an absolute path.' >&2; exit 2 ;; esac
if [ "$(uname -s)" != Darwin ] || [ "$(uname -m)" != arm64 ]; then
  printf '%s\n' 'The pinned Lima distribution requires macOS on Apple Silicon.' >&2
  exit 1
fi
if [ -e "$prefix" ] || [ -L "$prefix" ]; then
  printf '%s\n' "Refusing to replace an existing prefix: $prefix" >&2
  exit 1
fi
version=2.2.0
checksum=bbdef91774885a0d05f7b048c4eb89ae2bcf3a0c252ae7ca7934e63df76d93c3
work=$(mktemp -d "${TMPDIR:-/tmp}/luhmen-lima.XXXXXX")
trap 'rm -rf "$work"' EXIT
trap 'exit 129' HUP
trap 'exit 130' INT
trap 'exit 143' TERM
archive="$work/lima.tar.gz"
curl --fail --location --proto '=https' --tlsv1.2 \
  "https://github.com/lima-vm/lima/releases/download/v${version}/lima-${version}-Darwin-arm64.tar.gz" \
  --output "$archive"
actual=$(shasum -a 256 "$archive" | cut -d ' ' -f 1)
if [ "$actual" != "$checksum" ]; then
  printf '%s\n' 'Lima archive checksum mismatch; nothing was installed.' >&2
  exit 1
fi
mkdir "$work/extracted"
tar -xzf "$archive" -C "$work/extracted"
test -x "$work/extracted/bin/limactl"
test -d "$work/extracted/share/lima"
mkdir -p "$(dirname "$prefix")"
mv "$work/extracted" "$prefix"
printf 'Installed Lima %s in %s\n' "$version" "$prefix"
printf 'Add %s/bin to PATH before running luhmen.\n' "$prefix"
