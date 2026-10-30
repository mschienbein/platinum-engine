# Build and run on macOS arm64

The production CLI is independent of the Platinum Notes installation. On macOS arm64 it uses Rust DSP, AudioToolbox and Accelerate, a separately built TagLib bridge, and separately built FLAC and LAME libraries when needed (and libopus for the optional Opus format). Reference-only probes are optional. Linux aarch64 and x86_64 builds use portable implementations and the pinned native-library builds described in [LINUX.md](LINUX.md). MP3 and AAC input and AAC output need no library on any platform: macOS uses AudioToolbox, and Linux and `--features portable` builds use the in-tree exact ports.

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
all-target tests with the live-system reference modules excluded:

```sh
cargo build --locked --release
cargo test --locked --no-run --all-targets
cargo test --locked --all-targets --features portable --no-fail-fast -- --skip mac_reference:: --skip system::
```

CI first checks that these substring filters match only the intended reference
modules. See [VERIFICATION.md](VERIFICATION.md#macos-reference-tests-and-ci)
for the reference and coverage boundaries.

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

The Linux/portable bridge additionally exports three **optional** ABI-5 symbols (since v0.2.0), `pn_tag_bridge_features`, `pn_tag_copy_direct_ex` and `pn_tag_copy_generic_ex`, used only when `parity.allow_non_identical` is enabled to copy embedded artwork unchanged. The six original exports (`pn_tag_read_marker`, `pn_tag_inspect`, `pn_tag_bridge_abi`, `pn_tag_copy_direct`, `pn_tag_copy_generic`, `pn_tag_save_marker`) behave exactly as before, the ABI number stays 5, and a bridge without the extension (including the macOS bridge) still loads. With the opt-in on in a build that has the non-identical paths (Linux, or `--features portable`), a bridge without the extension makes every `process` item fail before any write with `artwork_conversion_unavailable`, because the requested behavior cannot be honored. A macOS default build never uses the extension.

The v0.3.0 Linux/portable bridge is v09 (v0.2.0 shipped v08). It adds no export and keeps the ABI number 5 and the feature word 1; with the opt-in on it treats a WAV as an ID3-capable container in generic artwork copies, and it refuses before any write a destination ID3v2 tag that would exceed the 268,435,455-byte limit of its 28-bit size field (v08 let such a tag through and TagLib wrote the size modulo 2^28). The Linux bridge is built by the `bridge` target of `linux/Dockerfile` with `-O2 -fPIC -fvisibility=hidden -fstack-protector-strong -D_FORTIFY_SOURCE=2`, linked with `-Wl,-z,relro,-z,now`, the version script and `--exclude-libs`. The macOS **comparison** dylib that `--features portable` builds load in the library-gated lanes, and that the native-versus-portable matrices compare with the production bridge, is built from the same source by `sh tools/portable/bridge/build-macos-portable.sh TAGLIB_INSTALL_DIR NEW_OUTPUT_DIR` with `-O2 -fstack-protector-strong -D_FORTIFY_SOURCE=2` against the pinned TagLib install above (the relro and now flags exist only for ELF). The production macOS bridge (`native/taglib_bridge.cpp`, built by the command above) is unchanged from v0.2.0.

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

MP3 **output**, including the default output format, needs a trusted LAME 3.100 and its SHA-256 pin. MP3 input needs no library: macOS uses AudioToolbox and Linux/`portable` use the in-tree exact MP3 reader. WAV output does not require LAME unless another requested format needs it.

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

## Optional: libopus 1.6.1 (Opus input and output)

Opus is **new behaviour beyond Platinum Notes**, which refuses `.opus` files, so it is off by default and has no reference to match. It needs the fixed-point libopus 1.6.1 below plus the setting `formats.opus`. Skip this section if you do not process Opus. Not Homebrew's libopus: only this exact configuration is accepted (the loader checks the version string `libopus 1.6.1-fixed`, a SHA-256 pin and, on Linux, the architecture allowlist, then runs a known-answer encode/decode test, so a float, SIMD-selecting or otherwise different build is refused rather than producing different bytes).

```sh
(
  set -eu
  curl -fL https://downloads.xiph.org/releases/opus/opus-1.6.1.tar.gz -o "$DEPS/opus-1.6.1.tar.gz"
  sh tools/linux/build-libopus.sh "$DEPS/opus-1.6.1.tar.gz" \
    6ffcb593207be92584df15b32466ed64bbec99109f007c82205f0194572411a1 \
    "$DEPS/opus-work" "$DEPS/opus-install"
)
OPUS_LIB="$DEPS/opus-install/lib/libopus.0.dylib"
OPUS_SHA="$(shasum -a 256 "$OPUS_LIB")"; OPUS_SHA="${OPUS_SHA%% *}"
"$PN" --state-dir "$PN_STATE" config set libraries.opus "$(json_string "$OPUS_LIB")"
"$PN" --state-dir "$PN_STATE" config set libraries.opus_sha256 "$(json_string "$OPUS_SHA")"
"$PN" --state-dir "$PN_STATE" config set formats.opus true
```

The script verifies the archive digest and builds with exactly `--enable-fixed-point --disable-float-api --disable-float-approx --disable-asm --disable-intrinsics --disable-rtcd --disable-doc --disable-extra-programs --enable-shared --disable-static` and `-O2 -ffp-contract=off`. libopus is BSD-3-Clause, but it is loaded like FLAC and LAME rather than linked in so that the SHA-256 pin identifies the exact code that produced an output and the build stays a plain `cargo build`. The same fixed-point source gave identical encoder and decoder output with Apple clang on macOS arm64 and with clang and gcc on Linux aarch64 and x86_64 (see `local-artifacts/opus-lane-01/libopus/`).

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
identity-gated battery. The v0.4.0 macOS executable is built the same way from
the frozen source; its SHA-256 is recorded in the release evidence
([VERIFICATION.md](VERIFICATION.md)), not here, because it describes
the tree this file belongs to.

The macOS default build enters AudioToolbox's `ExtAudioFileWrite`,
`ExtAudioFileDispose` and both `ExtAudioFileRead` call sites (the planar read
and the interleaved waveform read) through one small aarch64 trampoline,
`call_with_sp_residue` in `src/apple_codec.rs`, which fixes the stack pointer
modulo 64 at residue 0 (`WRITE_SP_RESIDUE`, `DISPOSE_SP_RESIDUE`,
`READ_SP_RESIDUE`). AudioToolbox's AAC encoder and decoder choose between an
SME and a NEON kernel from that residue, so the pin selects the kernel the
references used by construction and not through the compiler's frame layout (the
encoder pins date from v0.2.0, the two read pins from v0.3.0, and v0.4.0 leaves
all three unchanged; the output is meant to be unchanged). The Linux and
`portable` AAC encoder and decoder reproduce exactly the classes these pins
select (write class A with the generic tail, read class G) and not the SME
variants. It is an observation of the tested macOS arm64
AudioToolbox; see [VERIFICATION.md](VERIFICATION.md#v030-changes-and-evidence).

For Linux, run on the target's native Docker host with a prepared offline
registry cache (`index/` and `cache/`), a new output directory and the v0.4.0
native-library image (built by `tools/linux/build-image.sh`; check that the
bridge, libFLAC, LAME and libopus it contains have the digests in
[LINUX.md](LINUX.md#linux-native-libraries)):

```sh
PN_LINUX_REGISTRY_DIR="$REGISTRY" PN_LINUX_RUN_TESTS=1 PN_LINUX_TMP_TARGET=1 \
  sh tools/linux/build-cli.sh "$IMAGE" "$NEW_OUTPUT"
# Compare NEW_OUTPUT/target/release/platinum with the target's recorded digest.
```

`PN_LINUX_RUN_TESTS=1` runs the regular all-target suite and then a
library-gated lane: the `error_codes`, `linux_opt_in`, `portable_art_copy` and
`opus_cli` suites with `--ignored`, the four WAV tag-chunk bridge tests of
`tags_reference` (the default policy, the opt-in that keeps the chunks that carry source data, and the one that drops those that do not) and
the Opus layer's own tests (`--lib opus::`, with `PN_OPUS_LIB` and
`PN_OPUS_SHA256`). Those tests need the approved bridge, libFLAC, LAME and
libopus in the image, so the regular suite ignores them and never counts them
as passes.

The v0.2.0 image IDs of the recorded build were arm64
`sha256:352618c7e4e953d2883f3de95c3cb222dbe5fc55e81bc9426738e75ed988b070`
and amd64
`sha256:fbdc651c2f6843657e7a0daa5021ccf7b04782071fa4161088a2dd79bf628624`
(the v0.1.0 images were `3ecf1b2c409ce190661f3bc31dd349a6fc2b2c1560c890048bfabb43160cde0c`
and `e3dfe3526511ab1b640178a65a1fe6a0f11dde0bb4f41c8fabd0585bed9cf8d3`, with the
v0.1.0 bridge, which this release refuses). The v0.1.0 frozen Linux binary
hashes were arm64
`6e19449bc9d492d27fa0068013b4e9d3aeaa7b4fe915a3abc6285ffbe00e289e`
and amd64
`1c34e59b586b0f6adfa394b858cbdd2d3abb3d675cd8711c3a6e443c7c7ddfff`; the v0.2.0,
v0.3.0 and v0.4.0 hashes are in the release evidence.

For v0.3.0 the Linux bridge matrices ran in
`platinum-codec09-artbridge-arm64:study`
(`sha256:abd22164e6e9e7ee968c6f5cd5d39ed54e5f1cdd15c7c75c5ad98bb69e326c7e`)
and `platinum-codec09-artbridge-amd64:study`
(`sha256:5fddd8a084e6eb28d2c9aa865f803e5acc0f8e0b8c8cdb6e8a88174c6d67e1c5`):
the two v0.2.0 images above with the v09 bridge and the libopus stage of
`linux/Dockerfile` applied on top. A fresh `tools/linux/build-image.sh` build of
the real `linux/Dockerfile` gave the same bridge, libFLAC, LAME and libopus
digests on both architectures, and a `docker run` compile of the bridge
reproduced the pinned digest. Those image IDs are local build identities, not
a pin. The helper's output hashes and logs are
evidence for that build, not proof of universal audio-file parity.

## Tests and fixtures of the AAC ports

The exact AAC encoder and decoder (`src/portable/aac/`, wired in through
`src/portable/audio.rs` and `src/portable/audio/aac_read.rs`) are tested
without any Apple component against golden fixtures that were recorded on
macOS arm64 from AudioToolbox. The test targets belong to the regular suite
(`cargo test --locked --all-targets`) and run unchanged on macOS (with or
without `--features portable`), Linux aarch64 and Linux x86_64:

| Target | What it replays |
| --- | --- |
| `portable_aac_encode` | The encoder through the public portable `Writer` against `tests/fixtures/aac-encode/`: every accepted configuration, content kind and write size, the `create` answer for every rate and channel count (native status, warning or acceptance), bit depths, paths, cleanup and misuse, whole `.m4a` files byte for byte with the MP4 header times pinned |
| `portable_aac_encode_beyond_engine` | Three corners of AudioToolbox that the engine cannot observe: a writer that carries a warning, fractional sample rates (not reproduced: refused) and subnormal input under an ambient flush-to-zero mode |
| `portable_aac_decode` | The reader through the public `Reader` against `tests/fixtures/aac-decode/`: open outcomes, properties, streams, the frames written behind short reads, other read sizes, refusal classes, poisoning and file-name routing |
| `portable_aac_hostile` | Seeded mutations of the small AAC-LC inputs (6,000), grammar-generated LOAS, CAF and fragmented MP4 streams (20,000, 8,000 and 8,000 in a release build) and inputs of megabytes with tables that a naive scan would make quadratic: no panic, no hang, allocations and CPU time in proportion to the input, typed outcomes only; it does not compare with Apple |

The library tests (`cargo test --lib -- portable::aac portable::audio`) cover
the stages, the container layout and the `Writer` and `Reader` semantics, with
the five small inputs of `tests/fixtures/aac-reader/`. A debug build runs the
default encoder replay in about a minute on a hosted runner; the decoder tests
can be run alone with:

```sh
cargo test --offline --locked --features portable --test portable_aac_decode --test portable_aac_hostile
```

Tests marked `#[ignore = "long: run by the release battery"]` are never counted
as passes by the regular suite: the long encoder items of `portable_aac_encode`
(streams of 301 to 330 seconds, among them silence at the point where the `moov`
atom moves behind the media data) and the two long cases of `portable_aac_decode`
(a 464 s ADTS stream and a 302 s MP4 with `moov` in either place). They need a
release build (about twenty seconds there, many minutes in a debug build) and are
run, with one more ignored test of `portable_aac_decode` that reports timings, by
the release battery and by a step of the macOS CI job:

```sh
cargo test --locked --release --features portable --test portable_aac_encode --test portable_aac_decode -- --ignored
```

### Recorders and rebuilding the fixtures

The fixtures are generated audio with the results AudioToolbox gave for it; no
recording of real music is included. They are rebuilt on macOS arm64 only (the
recorders drive the engine's own AudioToolbox-based `Writer` and `Reader` in the
default build) and can be checked on any machine. The recorders are standalone
Cargo projects under `tools/portable/aac/`:

- `encode_probe` and `make_encode_fixtures.py`:
  `python3 tools/portable/aac/make_encode_fixtures.py --record --work DIR`
  (macOS arm64; `DIR` must not exist) builds the probe, records the generated
  inputs three times and writes `manifest.json`, `refs.bin` and `refusals.json`;
  `--verify` checks the pack against its manifest anywhere, without Apple or
  Rust, and `--append` records only the items of a new group on the machine
  the pack was recorded on. The generator of the inputs is
  `tests/support/aac_encode_input.rs`, shared by the recorder and the replay.
- `decode_probe`, `make_decode_fixtures.py` and `make_decode_rule_fixtures.py`:
  `verify-pack` checks the pack anywhere, `readsizes` records the 100-, 513- and
  4096-frame reads and `build` collects, records and classifies the inputs of
  the first parts; `build` needs generated corpora outside the repository, so
  the pack itself is checked here and not regenerated. `make_decode_rule_fixtures.py
  extend` (macOS arm64) cuts and records the container-rule cases (`rule/`)
  from files of the repository alone.
- `reader_probe/` (`apple/` and `portable/` probes, `structure_sweeps.py`,
  `compare.py`): the structural-damage sweeps (CAF chunks and fields, ADTS
  headers, LOAS and MP4 atoms, file names) from which the container rules in
  [AAC_INPUT.md](AAC_INPUT.md) were recorded, and the comparison of
  AudioToolbox's answers with the port's.

The README of each fixture directory (`tests/fixtures/aac-encode/`,
`tests/fixtures/aac-decode/`) lists its contents, hashes, what the reference
was and its limits. The reference is valid for the machine and OS build
recorded in the manifests only.

### CI

The macOS job runs the `portable` all-target suite, which includes the four
AAC targets, with the live-system reference modules skipped (see
[VERIFICATION.md](VERIFICATION.md#macos-reference-tests-and-ci)); the Linux jobs
run the all-target suites with and without `--features portable` in the native
image on both architectures. The `#[ignore]` long AAC tests run in a separate release step of the macOS job (the command above); the other `#[ignore]`
tests are not run by CI, except the library-gated suites listed in the Linux job.

## Tests and fixtures of the portable ALAC writer

The portable ALAC writer is checked the same way, without an Apple component:
`tests/portable_alac_packets.rs` replays AudioToolbox's recorded packets for
short final packets (45 generated cases and a 40-point series) and its recorded
whole files, cookie fields or refusals for oversized packets (48 cases), and
`tests/portable_alac_digests.rs` pins the whole-file digests of 18 ordinary
configurations to what the v0.3.0 writer produced, so that a change to a file
without those conditions is a decision of its own. `tools/portable/alac/partial_probe.rs`
(macOS) records the expected values and compares AudioToolbox's packets with the
portable ones: `rustc --edition 2024 -O tools/portable/alac/partial_probe.rs -o
partial-probe && partial-probe /absolute/scratch/dir` prints the tables the tests
pin and exits 1 on any difference. See [LINUX.md](LINUX.md#alac-output-on-linux-and-portable-builds)
for what the two ALAC corrections are.

## Linux and release packages

See [LINUX.md](LINUX.md) for the pinned multi-architecture native image,
architecture-specific library allowlist, source builds and runtime image. From
an appropriate native host, `sh tools/linux/package-release.sh PLATFORM
v0.4.0 NEW_OUTPUT_DIR` creates a local archive and source/relink materials
without publishing. The available platform values are `macos-arm64`,
`linux-arm64` and `linux-amd64`. The Linux allowlist means a locally rebuilt
library is refused unless its digest matches an approved entry; matching a
user-configured codec hash alone is insufficient.

## Licenses and distribution

Project-owned contributions are licensed under [LICENSE](../LICENSE), subject
to third-party terms. The separate bridge statically links TagLib (LGPL-2.1);
FLAC uses BSD-3-Clause terms and LAME LGPL-2.0-or-later; the optional libopus
uses BSD-3-Clause terms with royalty-free patent licenses. Linux codec patches
and source/relink materials are supplied in release packages; the independently
written portable ALAC implementation was informed by Apple's Apache-2.0
reference, and the in-tree exact MP3 reader and exact AAC encoder and decoder
are project-written ports whose provenance is stated in THIRD_PARTY_NOTICES. See [THIRD_PARTY_NOTICES](../THIRD_PARTY_NOTICES) and
[NOTICE](../NOTICE), plus the package's `licenses/` and
`corresponding-source/RELINK.txt`. Review the actual distribution and its
dependencies; these notices do not constitute legal clearance or grant rights
to the reference application.
