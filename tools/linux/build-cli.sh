#!/bin/sh
# Run on the target Linux architecture (or native arm64 Docker Desktop).
# Usage: sh tools/linux/build-cli.sh IMAGE_TAG NEW_OUTPUT_DIR
# PN_LINUX_VENDOR_DIR or PN_LINUX_REGISTRY_DIR selects an offline, read-only
# dependency source. PN_LINUX_RUN_TESTS=1 records build and all-target test logs.
# PN_LINUX_TMP_TARGET=1 builds on container tmpfs and retains only the binary.
# The project tree is mounted read-only; all persistent writes stay in output.
set -eu
[ "$#" -eq 2 ] || { echo 'usage: build-cli.sh IMAGE_TAG NEW_OUTPUT_DIR' >&2; exit 2; }
image=$1
out=$2
root=$(CDPATH= cd -- "$(dirname -- "$0")/../.." && pwd)
mkdir "$out"
out=$(CDPATH= cd -- "$out" && pwd)
mkdir "$out/cargo" "$out/target"
vendor=
registry=
if [ -n "${PN_LINUX_VENDOR_DIR:-}" ] && [ -n "${PN_LINUX_REGISTRY_DIR:-}" ]; then
  echo 'choose either a vendor directory or a registry cache' >&2
  exit 2
fi
if [ -n "${PN_LINUX_VENDOR_DIR:-}" ]; then
  vendor=$(CDPATH= cd -- "$PN_LINUX_VENDOR_DIR" && pwd)
  cat > "$out/cargo/config.toml" <<'EOF'
[source.crates-io]
replace-with = "vendored-sources"
[source.vendored-sources]
directory = "/vendor"
EOF
  offline=1
elif [ -n "${PN_LINUX_REGISTRY_DIR:-}" ]; then
  registry=$(CDPATH= cd -- "$PN_LINUX_REGISTRY_DIR" && pwd)
  [ -d "$registry/index" ] && [ -d "$registry/cache" ] || {
    echo 'registry cache must contain index/ and cache/' >&2
    exit 2
  }
  mkdir -p "$out/cargo/registry/index" "$out/cargo/registry/cache"
  offline=1
else
  offline=0
fi
set -- docker run --rm --read-only --user "$(id -u):$(id -g)" \
  --tmpfs /tmp:exec,mode=1777 \
  -e CARGO_HOME=/out/cargo -e CARGO_TARGET_DIR=/out/target -e RUSTUP_TOOLCHAIN=1.99.1 \
  -e PN_LINUX_OFFLINE="$offline" -e PN_LINUX_RUN_TESTS="${PN_LINUX_RUN_TESTS:-0}" \
  -e PN_LINUX_TMP_TARGET="${PN_LINUX_TMP_TARGET:-0}" \
  -v "$root:/src:ro" -v "$out:/out" -w /src
if [ "$offline" -eq 1 ]; then set -- "$@" --network none; fi
if [ -n "$vendor" ]; then set -- "$@" -v "$vendor:/vendor:ro"; fi
if [ -n "$registry" ]; then
  set -- "$@" -v "$registry/index:/out/cargo/registry/index:ro" \
    -v "$registry/cache:/out/cargo/registry/cache:ro"
fi
exec "$@" "$image" sh -c '
  set -u
  if [ "$PN_LINUX_OFFLINE" = 1 ]; then set -- --offline; else set --; fi
  if [ "$PN_LINUX_TMP_TARGET" = 1 ]; then
    [ "$PN_LINUX_RUN_TESTS" = 1 ] || { echo "tmpfs target requires test mode" >&2; exit 2; }
    CARGO_TARGET_DIR=/tmp/target
    export CARGO_TARGET_DIR
  fi
  if [ "$PN_LINUX_RUN_TESTS" != 1 ]; then
    exec cargo build "$@" --locked --release --bin platinum --features portable
  fi
  cargo build "$@" --locked --release --bin platinum --features portable > /out/build.stdout 2> /out/build.stderr
  build=$?
  cargo test "$@" --locked --all-targets > /out/test.stdout 2> /out/test.stderr
  test=$?
  # Library-gated lane: scenario tests that need the native libraries of this image.
  lib=/opt/platinum-native/lib
  PLATINUM_TEST_TAG_BRIDGE=$lib/libpn-tag-bridge.so PN_TAG_BRIDGE=$lib/libpn-tag-bridge.so \
    PN_FLAC_LIB=$lib/libFLAC.so PN_FLAC_SHA256=$(sha256sum $lib/libFLAC.so | cut -d" " -f1) \
    PN_LAME_LIB=$lib/libmp3lame.so PN_LAME_SHA256=$(sha256sum $lib/libmp3lame.so | cut -d" " -f1) \
    cargo test "$@" --locked --test error_codes --test linux_opt_in --test portable_art_copy -- --ignored > /out/ignored.stdout 2> /out/ignored.stderr
  gated=$?
  # The tag bridge tests that pin the default WAV marker policy and the opt-in that keeps the chunks TagLib wrote.
  PN_TAG_BRIDGE=$lib/libpn-tag-bridge.so \
    cargo test "$@" --locked --test tags_reference -- --ignored wav_direct_copy_loses preserve_wav_chunks >> /out/ignored.stdout 2>> /out/ignored.stderr
  second=$?
  [ "$gated" -ne 0 ] || gated=$second
  printf "%s\n" "$build" > /out/build.status
  printf "%s\n" "$test" > /out/test.status
  printf "%s\n" "$gated" > /out/ignored.status
  artifact=0
  if [ "$PN_LINUX_TMP_TARGET" = 1 ] && [ "$build" -eq 0 ]; then
    mkdir -p /out/target/release && cp /tmp/target/release/platinum /out/target/release/platinum || artifact=$?
  fi
  [ "$build" -eq 0 ] && [ "$test" -eq 0 ] && [ "$gated" -eq 0 ] && [ "$artifact" -eq 0 ]
'
