#!/usr/bin/env bash
# Packages the macOS (Apple Silicon) client into one archive:
#   verbarelay-client-<label>-macos-arm64.zip  executable + engine + CPU/Metal/BLAS backends
#
# Usage: scripts/package-client-macos.sh <label> <client binary> <engine install dir> <out dir>
set -euo pipefail

label="$1"
binary="$2"
engine="$3"
out="$4"

core="$(mktemp -d)"
trap 'rm -rf "$core"' EXIT

cp "$binary" "$core/verbarelay-client"
chmod 755 "$core/verbarelay-client"
# Dereference symlinks so the archive holds real files under the install names.
for lib in "$engine"/*.dylib "$engine"/*.so; do
    [ -e "$lib" ] && cp -L "$lib" "$core/"
done
cp LICENSE THIRD-PARTY-NOTICES.md "$core/"

mkdir -p "$out"
out="$(cd "$out" && pwd)"
(cd "$core" && zip -q -r -X "$out/verbarelay-client-$label-macos-arm64.zip" .)
ls -l "$out"
