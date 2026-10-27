# Build and run on macOS arm64

The production CLI is independent of the Platinum Notes installation. It uses Rust DSP, macOS AudioToolbox and Accelerate, a separately built TagLib bridge, and, when needed, separately built FLAC and LAME libraries. Reference-only probes are optional and are not part of these instructions. This is a macOS arm64 setup; other operating systems and architectures are not established targets.

## Prerequisites and recorded setup

- An arm64 Mac with Apple command-line developer tools, including `xcrun clang++`, `make`, and the macOS SDK.
- Rust and Cargo via rustup; `rust-toolchain.toml` pins Rust 1.99.1. `cargo build --locked` uses the checked-in `Cargo.lock`.
- CMake 3.31.6 for the native builds, plus `curl`, `tar`, `shasum`, and Python 3 for the examples below. The recorded TagLib build used CMake 3.31.6 in an isolated `uvx` environment; an equivalent executable on `PATH` is sufficient to follow the commands. If you have `uvx`, `cmake() { uvx --from 'cmake==3.31.6' cmake "$@"; }` makes the examples use that version in the current shell.
- Network access to the pinned public source archives and Rust dependencies. No reference application, app-bundled framework, login, or user audio is required.

The recorded TagLib build was Release, static and position-independent; the bridge used the system Apple frameworks and zlib. FLAC was a Release shared build with Ogg disabled. LAME used the flags and one export-list correction below. Other recorded host diagnostics report macOS 27.2 build 26B118 on arm64; the TagLib build cache reports the macOS 27.0 SDK. Compiler, SDK, OS, source, options and paths can affect outputs. Matching these directions does **not** by itself prove byte-identical libraries or output on another machine.

Run the examples from the repository root in the same shell. They create a **new** ignored dependency directory; do not point them at an existing dependency tree. Stop if a download, digest check, build, or configuration command fails.

```sh
mkdir -p local-artifacts
DEPS="$(mktemp -d "$PWD/local-artifacts/deps.XXXXXX")"
PN_STATE="$DEPS/state"
PN="$PWD/target/debug/platinum"
```

## Build the CLI

```sh
cargo build --locked --bin platinum
"$PN" --help
"$PN" --state-dir "$PN_STATE" --json templates list
"$PN" --state-dir "$PN_STATE" --json config show
```

Building the CLI does not itself build the native libraries. Configuration/template commands do not require them. `research/templates/builtins.json` is a compile-time resource, not an optional research report; it must be present with the source tree.

## Required: TagLib 1.11.1 and ABI-5 bridge

The archive is pinned to TagLib revision `e36a9cabb9882e61276161c23834d966d62073b7`. This uses a public TagLib build, never the framework supplied with the reference application. The checksum is for that exact archive, not for an arbitrary 1.11.1 archive.

```sh
(
  set -eu
  curl -fL \
    https://github.com/taglib/taglib/archive/e36a9cabb9882e61276161c23834d966d62073b7.tar.gz \
    -o "$DEPS/taglib.tar.gz"
  printf '%s  %s\n' \
    5c80b1e5273849122ef11b5a9116e4b7f182b899cfdb28aefb704087216e1360 \
    "$DEPS/taglib.tar.gz" | shasum -a 256 -c -
  tar -xzf "$DEPS/taglib.tar.gz" -C "$DEPS"
  cmake -S "$DEPS/taglib-e36a9cabb9882e61276161c23834d966d62073b7" \
    -B "$DEPS/build-taglib" \
    -DCMAKE_BUILD_TYPE=Release -DCMAKE_OSX_ARCHITECTURES=arm64 \
    -DCMAKE_POSITION_INDEPENDENT_CODE=ON \
    -DCMAKE_INSTALL_PREFIX="$DEPS/taglib-install" \
    -DBUILD_SHARED_LIBS=OFF -DBUILD_BINDINGS=OFF \
    -DBUILD_TESTS=OFF -DBUILD_EXAMPLES=OFF
  cmake --build "$DEPS/build-taglib"
  cmake --install "$DEPS/build-taglib"
  xcrun clang++ -std=c++17 -fobjc-arc -dynamiclib -DTAGLIB_STATIC \
    -I"$DEPS/taglib-install/include/taglib" \
    -x objective-c++ native/taglib_bridge.cpp -x none \
    "$DEPS/taglib-install/lib/libtag.a" \
    -framework Foundation -framework AppKit -framework CoreGraphics \
    -framework ImageIO -framework CoreServices -lz \
    -o "$DEPS/libpn-tag-bridge.dylib"
)
```

The bridge ABI must be 5. `process`, `analyze`, `inspect`, and marker-authorized `restore` need a trusted bridge; the loader checks its ABI, not its source provenance. A dynamic library is executable code, so configure only a library you trust. The recorded bridge SHA-256 was `ab36bcd51a901ccb5d95e73b960596033c0387ec450215d66bf7bc8d18fc57aa`; a locally rebuilt file can differ.

Configure its path as a JSON string. The helper handles spaces and other characters that need JSON escaping:

```sh
json_string() { python3 -c 'import json,sys; print(json.dumps(sys.argv[1]))' "$1"; }
"$PN" --state-dir "$PN_STATE" config set libraries.tag_bridge \
  "$(json_string "$DEPS/libpn-tag-bridge.dylib")"
```

## Optional: FLAC 1.4.2

FLAC **input or output** needs a trusted public libFLAC 1.4.2 and a SHA-256 pin. This build follows the recorded shared, no-Ogg configuration; Ogg support is not claimed. Skip this section if processing only non-FLAC inputs to non-FLAC outputs.

```sh
(
  set -eu
  curl -fL https://github.com/xiph/flac/releases/download/1.4.2/flac-1.4.2.tar.xz \
    -o "$DEPS/flac-1.4.2.tar.xz"
  printf '%s  %s\n' \
    e322d58a1f48d23d9dd38f432672865f6f79e73a6f9cc5a5f57fcaa83eb5a8e4 \
    "$DEPS/flac-1.4.2.tar.xz" | shasum -a 256 -c -
  tar -xf "$DEPS/flac-1.4.2.tar.xz" -C "$DEPS"
  cmake -S "$DEPS/flac-1.4.2" -B "$DEPS/build-flac" \
    -DCMAKE_BUILD_TYPE=Release -DCMAKE_OSX_ARCHITECTURES=arm64 \
    -DBUILD_SHARED_LIBS=ON -DWITH_OGG=OFF \
    -DBUILD_CXXLIBS=OFF -DBUILD_PROGRAMS=OFF -DBUILD_TESTING=OFF \
    -DBUILD_EXAMPLES=OFF -DBUILD_DOCS=OFF -DBUILD_UTILS=OFF
  cmake --build "$DEPS/build-flac"
)
FLAC_LIB="$DEPS/build-flac/src/libFLAC/libFLAC.12.0.0.dylib"
FLAC_SHA="$(shasum -a 256 "$FLAC_LIB")"; FLAC_SHA="${FLAC_SHA%% *}"
"$PN" --state-dir "$PN_STATE" config set libraries.flac "$(json_string "$FLAC_LIB")"
"$PN" --state-dir "$PN_STATE" config set libraries.flac_sha256 "$(json_string "$FLAC_SHA")"
```

The recorded FLAC library hash was `d9b3a64df9eeab7556aad9877492ad07ba51e6459e3725f90b0a9203078ac6a5`; use the digest of **your own trusted build** in configuration. A digest checks the selected file, not its provenance or equivalence to the original encoder. Homebrew FLAC 1.5 is not the pinned 1.4.2 build.

## Optional: LAME 3.100

MP3 **output**, including the default output format, needs a trusted LAME 3.100 and its SHA-256 pin. MP3 input alone uses the macOS decoder. WAV output does not require LAME unless another requested format needs it.

The recorded build removed only the obsolete `lame_init_old` line from `include/libmp3lame.sym`: the linker rejected that export because the legacy function is static in 3.100. This is an export-list change, not an audio-algorithm change. The archive and corrected export-list checks prevent accidentally applying the change to a different source tree.

```sh
(
  set -eu
  curl -fL https://ftp.debian.org/debian/pool/main/l/lame/lame_3.100.orig.tar.gz \
    -o "$DEPS/lame-3.100.tar.gz"
  printf '%s  %s\n' \
    ddfe36cab873794038ae2c1210557ad34857a4b6bdc515785d1da9e175b1da1e \
    "$DEPS/lame-3.100.tar.gz" | shasum -a 256 -c -
  tar -xzf "$DEPS/lame-3.100.tar.gz" -C "$DEPS"
  python3 - "$DEPS/lame-3.100/include/libmp3lame.sym" <<'PY'
from pathlib import Path
import sys
path = Path(sys.argv[1])
lines = path.read_bytes().splitlines(keepends=True)
needle = b"lame_init_old\n"
if lines.count(needle) != 1:
    raise SystemExit("unexpected LAME export list; refusing to modify it")
path.write_bytes(b"".join(line for line in lines if line != needle))
PY
  printf '%s  %s\n' \
    0f4570fddb684f87b04ae6e36a7919d9c6928423d2a761d53a90cd21edb76ecd \
    "$DEPS/lame-3.100/include/libmp3lame.sym" | shasum -a 256 -c -
  cd "$DEPS/lame-3.100"
  ./configure --prefix="$DEPS/lame-install" --enable-shared --disable-static \
    --disable-frontend --disable-analyzer-hooks --disable-dependency-tracking \
    CFLAGS='-O2 -ffp-contract=off'
  make -j4
)
LAME_LIB="$DEPS/lame-3.100/libmp3lame/.libs/libmp3lame.0.dylib"
LAME_SHA="$(shasum -a 256 "$LAME_LIB")"; LAME_SHA="${LAME_SHA%% *}"
"$PN" --state-dir "$PN_STATE" config set libraries.lame "$(json_string "$LAME_LIB")"
"$PN" --state-dir "$PN_STATE" config set libraries.lame_sha256 "$(json_string "$LAME_SHA")"
```

The recorded library hash was `58d3a0dba8271d00f0a88704708a6a0caa5610d8da0b1f7dcbbad64c9a165981`; configure the digest of your own trusted build. Homebrew LAME 4.0 is not the pinned 3.100 build.

## Try the pinned synthetic fixture

This example requires only the CLI and bridge above. It uses a fresh path, generated audio, the first built-in Official template, WAV output, one worker and a separate output directory; it does not replace an input. The generator refuses an existing output path. `HOME` must be set even with `--state-dir`.

```sh
(
  set -eu
  mkdir -p "$DEPS/fixture/input" "$DEPS/fixture/output"
  rustc --edition=2024 -O tools/make-reference-fixture.rs -o "$DEPS/make-reference-fixture"
  "$DEPS/make-reference-fixture" "$DEPS/fixture/input/reference.wav"
  printf '%s  %s\n' \
    2de421ff4f080693c78a24947ad856d9db8f86c5909f42252187307aa820f884 \
    "$DEPS/fixture/input/reference.wav" | shasum -a 256 -c -
  "$PN" --state-dir "$PN_STATE" --json process "$DEPS/fixture/input/reference.wav" \
    --template-index 0 --format wav --output-dir "$DEPS/fixture/output" --jobs 1
  printf '%s  %s\n' \
    06dbf2b016bf3389aa997ce7dd9816a663d226bde5d38e2b4cb62c3abb0d0c1b \
    "$DEPS/fixture/output/reference_PN.wav" | shasum -a 256 -c -
)
```

Proceed only if the input checksum matches: floating-point math runtimes can affect regenerated input. On the recorded setup, two reference exports and both Rust debug/release outputs were 3,076,786 bytes with complete-file SHA-256 `06dbf2b016bf3389aa997ce7dd9816a663d226bde5d38e2b4cb62c3abb0d0c1b`. A different OS/SDK, codec build, template, state, or other metadata-relevant context may produce a different result; do not normalize the file to manufacture a match. This is one case, not universal parity. A local run of this example does not itself compare against a reference export.

For real files, use a separate output folder first and retain an independent backup. See [the CLI contract](../research/CLI_CONTRACT.md) for explicit replacement, restore, nil markers, and failure reporting.

## Licenses and distribution

No project license granting reuse is included in this checkout. This guide does not grant redistribution rights. Review the upstream source packages and their license files before distribution: TagLib includes `COPYING.LGPL` and `COPYING.MPL`, FLAC includes `COPYING.Xiph`, and LAME includes `COPYING`; Rust dependencies have their own terms. In particular, the static TagLib linkage and the LAME export-list modification are facts to account for when evaluating distribution obligations. No reference-app binaries or audio are needed for this build.
