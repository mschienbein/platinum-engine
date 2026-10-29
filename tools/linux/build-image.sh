#!/bin/sh
# Create a private, immutable-input Docker build context. Never reuses it.
# Usage: sh tools/linux/build-image.sh IMAGE_TAG NEW_CONTEXT_DIR [docker build options...]
set -eu
[ "$#" -ge 2 ] || { echo 'usage: build-image.sh IMAGE_TAG NEW_CONTEXT_DIR [docker build options...]' >&2; exit 2; }
tag=$1
context=$2
shift 2
root=$(CDPATH= cd -- "$(dirname -- "$0")/../.." && pwd)
[ -f "$root/native/taglib_bridge_portable.cpp" ] || { echo 'portable ABI-5 bridge source missing' >&2; exit 1; }
[ -f "$root/tools/portable/bridge/bridge.map" ] || { echo 'portable ABI-5 export map missing' >&2; exit 1; }
mkdir "$context"
mkdir -p "$context/native" "$context/tools/portable/bridge" "$context/tools/portable/math/shim/src" "$context/tools/linux" "$context/linux" "$context/src/portable"
cp "$root/linux/Dockerfile" "$context/Dockerfile"
cp "$root/native/taglib_bridge.h" "$root/native/taglib_bridge_portable.cpp" "$context/native/"
cp "$root/tools/portable/bridge/bridge.map" "$context/tools/portable/bridge/"
cp "$root/tools/linux/build-libopus.sh" "$context/tools/linux/"
cp -R "$root/linux/patches" "$context/linux/patches"
cp "$root/src/portable/math.rs" "$context/src/portable/math.rs"
cp "$root/tools/portable/math/shim/Cargo.toml" "$root/tools/portable/math/shim/Cargo.lock" "$root/tools/portable/math/shim/pn_codec_math.h" "$context/tools/portable/math/shim/"
cp "$root/tools/portable/math/shim/src/lib.rs" "$context/tools/portable/math/shim/src/"
if command -v sha256sum >/dev/null 2>&1; then
    (cd "$context" && sha256sum -c linux/patches/codec-math.sha256)
else
    (cd "$context" && shasum -a 256 -c linux/patches/codec-math.sha256)
fi
docker build --target bridge --tag "$tag" "$@" "$context"
