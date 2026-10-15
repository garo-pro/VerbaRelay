#!/usr/bin/env bash
# Packages the Linux client into two archives:
#   verbarelay-client-<label>-linux-x86_64.zip  executable + engine + CPU/Vulkan backends
#   verbarelay-cuda-<label>-linux-x86_64.zip    CUDA backend + CUDA runtime + cuBLAS
#
# Usage: scripts/package-client.sh <label> <client binary> <engine install dir> <cuda lib dir> <out dir>
set -euo pipefail

label="$1"
binary="$2"
engine="$3"
cuda_lib="$4"
out="$5"

core="$(mktemp -d)"
cuda="$(mktemp -d)"
trap 'rm -rf "$core" "$cuda"' EXIT

cp "$binary" "$core/verbarelay-client"
chmod 755 "$core/verbarelay-client"

# Libraries are copied under their SONAME (dereferencing symlinks), which is the
# name the dynamic loader looks for.
copy_soname() {
    local file="$1" dest="$2" soname
    soname="$(readelf -d "$file" 2>/dev/null | sed -n 's/.*SONAME.*\[\(.*\)\]/\1/p')"
    cp -L "$file" "$dest/${soname:-$(basename "$file")}"
}

copy_soname "$engine/libvr_engine.so" "$core"
copy_soname "$engine/libwhisper.so" "$core"
copy_soname "$engine/libggml.so" "$core"
copy_soname "$engine/libggml-base.so" "$core"
for module in "$engine"/libggml-cpu*.so "$engine"/libggml-vulkan.so; do
    [ -e "$module" ] && cp -L "$module" "$core/"
done

if [ -e "$engine/libggml-cuda.so" ]; then
    cp -L "$engine/libggml-cuda.so" "$cuda/"
    for lib in libcudart.so.12 libcublas.so.12 libcublasLt.so.12; do
        cp -L "$cuda_lib/$lib" "$cuda/"
    done
fi

cp LICENSE THIRD-PARTY-NOTICES.md "$core/"

mkdir -p "$out"
out="$(cd "$out" && pwd)"
(cd "$core" && zip -q -r -X "$out/verbarelay-client-$label-linux-x86_64.zip" .)
if [ -n "$(ls -A "$cuda")" ]; then
    (cd "$cuda" && zip -q -r -X "$out/verbarelay-cuda-$label-linux-x86_64.zip" .)
fi
ls -l "$out"
