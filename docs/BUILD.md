# Build and run on macOS arm64

The production CLI is independent of the Platinum Notes installation. On macOS arm64 it uses Rust DSP, AudioToolbox and Accelerate, a separately built TagLib bridge, and separately built FLAC and LAME libraries when needed. Reference-only probes are optional. Linux aarch64 and x86_64 builds use portable implementations and the pinned native-library builds described in [LINUX.md](LINUX.md).

## Prerequisites and recorded setup

- An arm64 Mac with Apple command-line developer tools, including `xcrun clang++`, `make`, and the macOS SDK.
- Rust and Cargo via rustup; `rust-toolchain.toml` pins Rust 1.99.1. `cargo build --locked` uses the checked-in `Cargo.lock`.
- CMake 3.31.6 for the native builds, plus `curl`, `tar`, `shasum`, and Python 3 for the examples below. The recorded TagLib build used CMake 3.31.6 in an isolated `uvx` environment; an equivalent executable on `PATH` is sufficient to follow the commands. If you have `uvx`, `cmake() { uvx --from 'cmake==3.31.6' cmake "$@"; }` makes the examples use that version in the current shell.
- Network access to the pinned public source archives and Rust dependencies. No reference application, app-bundled framework, login, or user audio is required.

The recorded TagLib build was Release, static and position-independent; the bridge used the system Apple frameworks and zlib. FLAC was a Release shared build with Ogg disabled. LAME used the flags and one export-list correction below. Other recorded host diagnostics report macOS 27.2 build 26B118 on arm64; the TagLib build cache reports the macOS 27.0 SDK. Compiler, SDK, OS, source, options and paths can affect outputs. Matching these directions does **not** by itself prove byte-identical libraries or output on another machine.

Run the examples from the repository root in the same shell. They create a **new** temporary dependency directory; do not point them at an existing dependency tree. Stop if a download, digest check, build, or configuration command fails.

```sh
DEPS="$(mktemp -d "${TMPDIR:-/tmp}/platinum-deps.XXXXXX")"
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

Building the CLI does not itself build the native libraries. Configuration/template commands do not require them. `research/templates/builtins.json` and `src/portable/fft_sme_coefficients.bin` are compile-time resources, not optional research reports; both must be present with the source tree.

On the recorded M4 reference Mac, the release battery runs the default-mode
native-value tests. Their exact expected values can differ on another Mac or
macOS version because the default build uses that system's libraries. CI
builds the default release and compiles those tests, then runs the portable
all-target tests against the recorded M4 implementation:

```sh
cargo build --locked --release
cargo test --locked --no-run --all-targets
cargo test --locked --all-targets --features portable --no-fail-fast
```

See [VERIFICATION.md](VERIFICATION.md#macos-reference-tests-and-ci) for the
reference and coverage boundaries.

## Required: TagLib 1.11.1 and ABI-5 bridge v07

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

The bridge ABI must be 5; the v07 source adds source-container status and read-failure handling. `process`, `analyze`, `inspect`, and marker-authorized `restore` need a trusted bridge. The earlier v07 macOS bridge SHA-256 was `7e2f65614f2ad87322e030213b3b2fb2914a3e69635b434fd96cf6b1da38d585`; the v0.1.0 package uses the same bridge with a neutral install name and an ad-hoc signature, SHA-256 `14ca6f03b65a2cb0c4bc8bd06df540ae1707796f59f2d4f117a5d33447dfd79e`. A locally rebuilt file can differ. The older `ab36bcd5…` bridge was an earlier build. The loader checks the ABI and pins the opened file, not its provenance. A dynamic library is executable code, so configure only a library you trust. A writer able to modify that same file's inode during loading can defeat the integrity check before its post-load verification; prevent concurrent modification.

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

The earlier recorded library hash was `58d3a0dba8271d00f0a88704708a6a0caa5610d8da0b1f7dcbbad64c9a165981`; the v0.1.0 package uses a neutral install name and ad-hoc signature, SHA-256 `2986f34cca6a4fbfb05934ac33b4bb919c8c47de1594e227b2f0fa4c930b377f`. Configure the digest of your own trusted build. Homebrew LAME 4.0 is not the pinned 3.100 build.

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

## Rebuilding the recorded release CLI

The following is the recorded macOS arm64 release-build environment, from the
root of the reviewed public source export. It uses cached locked dependencies,
a fresh target directory and path remaps. It does not install or
publish anything. Compiler, OS, SDK and dependency changes can change the
binary; compare the result rather than assuming identity.

```sh
EXPORT="$(pwd -P)"
RUN="$(mktemp -d "${TMPDIR:-/tmp}/platinum-release.XXXXXX")"
mkdir "$RUN/home" "$RUN/tmp"
env -i PATH=/opt/homebrew/opt/rustup/bin:/usr/bin:/bin:/usr/sbin:/sbin:/usr/local/bin \
  RUSTUP_TOOLCHAIN=1.99.1 RUSTUP_HOME="$HOME/.rustup" CARGO_HOME="$HOME/.cargo" \
  CARGO_TARGET_DIR="$RUN/target" HOME="$RUN/home" CFFIXED_USER_HOME="$RUN/home" \
  TMPDIR="$RUN/tmp/" LANG=C LC_ALL=C TZ=UTC \
  RUSTFLAGS="--remap-path-prefix=$HOME=/user --remap-path-prefix=$HOME/.rustup=/rustup --remap-path-prefix=$HOME/.cargo/registry/src=/cargo/registry/src --remap-path-prefix=$HOME/.cargo/git/checkouts=/cargo/git/checkouts --remap-path-prefix=$EXPORT=/platinum-source" \
  cargo build --offline --locked --release --bin platinum
shasum -a 256 "$RUN/target/release/platinum"
```

The v0.1.0 macOS battery executable SHA-256 was
`198e0091a212e8af05e0934b1249fd698bfa2300f643e3b430d2b188d30913b8`.
The earlier source-path-remapped battery executable was
`82d8b48904f8d6128b7399330556ea13d130f01426d86041db1d974676d49b0d`;
the release rebuild adds home, Cargo and Rustup path remaps and has its own
identity-gated battery.

For Linux, run on the target's native Docker host with a prepared offline
registry cache (`index/` and `cache/`), a new output directory and the
appropriate v07 image identity:

```sh
PN_LINUX_REGISTRY_DIR="$REGISTRY" PN_LINUX_RUN_TESTS=1 PN_LINUX_TMP_TARGET=1 \
  sh tools/linux/build-cli.sh "$IMAGE" "$NEW_OUTPUT"
# Compare NEW_OUTPUT/target/release/platinum with the target's digest below.
```

The recorded image IDs were arm64
`sha256:3ecf1b2c409ce190661f3bc31dd349a6fc2b2c1560c890048bfabb43160cde0c`
and amd64
`sha256:e3dfe3526511ab1b640178a65a1fe6a0f11dde0bb4f41c8fabd0585bed9cf8d3`.
The frozen Linux binary hashes were arm64
`6e19449bc9d492d27fa0068013b4e9d3aeaa7b4fe915a3abc6285ffbe00e289e`
and amd64
`1c34e59b586b0f6adfa394b858cbdd2d3abb3d675cd8711c3a6e443c7c7ddfff`.
The helper runs the regular all-target tests, too. Its output hashes and logs
are evidence for that build, not proof of universal audio-file parity.

## Linux and release packages

See [LINUX.md](LINUX.md) for the pinned multi-architecture native image,
architecture-specific library allowlist, source builds and runtime image. From
an appropriate native host, `sh tools/linux/package-release.sh PLATFORM
v0.1.0 NEW_OUTPUT_DIR` creates a local archive and source/relink materials
without publishing. The available platform values are `macos-arm64`,
`linux-arm64` and `linux-amd64`. The Linux allowlist means a locally rebuilt
library is refused unless its digest matches an approved entry; matching a
user-configured codec hash alone is insufficient.

## Licenses and distribution

Project-owned contributions are licensed under [LICENSE](../LICENSE), subject
to third-party terms. The separate bridge statically links TagLib (LGPL-2.1);
FLAC uses BSD-3-Clause terms and LAME LGPL-2.0-or-later. Linux codec patches
and source/relink materials are supplied in release packages; the independently
written portable ALAC implementation was informed by Apple's Apache-2.0
reference. See [THIRD_PARTY_NOTICES](../THIRD_PARTY_NOTICES) and
[NOTICE](../NOTICE), plus the package's `licenses/` and
`corresponding-source/RELINK.txt`. Review the actual distribution and its
dependencies; these notices do not constitute legal clearance or grant rights
to the reference application.
