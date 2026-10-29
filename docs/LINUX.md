# Linux builds and runtime

Platinum Engine v0.2.0 targets Linux aarch64 and x86_64 as well as the macOS
arm64 reference. The default objective for admitted Linux operations is an
identical complete output file, including encoded samples, container, tags and
marker; known unsupported operations are refused or reported unavailable
instead of being approximated. The evidence is finite and there are known x86
arithmetic residuals, so this is not a guarantee that every mismatch can be
detected at runtime. Linux aarch64 is the stricter parity build. One explicit
opt-in, [described below](#opt-in-mp3-input-and-cover-art), allows two paths
that cannot be proven identical. See [VERIFICATION.md](VERIFICATION.md) for the
individual evidence and limits.

In the v0.1.0 frozen-source battery (the v0.2.0 differences are listed in
[VERIFICATION.md](VERIFICATION.md)), each architecture had 104 complete native
file matches, 25 documented typed refusals and three matching native
error/no-output cases, plus 16 matching workflow cases. In a separate 575-case
stress corpus, each Linux architecture matched macOS bytes in 535 cases and
refused 40; all 30 additional pitch cases matched. Refusals are not counted as
matches, and the x86 residual classes were not proved absent by those runs.

## What is refused

- AAC input decoding and AAC output encoding: the Apple decoder/encoder
  behavior is not reproduced on Linux, with or without the opt-in. MP3 input
  decoding is refused unless `parity.allow_non_identical` is enabled (see
  below). MP3 and FLAC *encoding* use the pinned public LAME and libFLAC builds
  described below.
- Sample-rate conversion is not reproduced. Processing that needs unequal
  input, analysis or output rates fails with an unsupported sample-rate
  conversion error; analysis of non-MP3 inputs at or below 22,050 Hz is
  refused. Non-44.1 kHz source waveforms are reported unavailable; the rest of
  an otherwise supported analysis can still succeed.
- Artwork operations needing Apple image transcoding fail with an artwork
  conversion error unless `parity.allow_non_identical` is enabled and the
  destination can store the picture unchanged. Do not assume existing cover art
  will be copied by default.
- Unverified codec, container and channel-layout variants, including GSM masks
  outside the tested subset, are refused. ALAC's portable writer accepts finite
  22,050–96,000 Hz rates, 1–8 channels and 16/20/24/32-bit depths within its
  implemented container subset. Audio outside those rates or channel counts is
  refused as an output problem (`unsupported_output_format`, not an input
  error), and the writer refuses packet conditions under which the native
  encoder can silently lose audio. macOS checks the completed ALAC output
  for lost frames before publication as well.
- Malformed or unknown containers with unestablished marker absence, duplicate
  AIFF ID3 chunks, candidate marker chunks in malformed matching WAV/AIFF files,
  unavailable source tags without a safe container classification, and
  unapproved native libraries fail closed. These are conservative safety
  policies, not claims that the original GUI makes the same choice.

These guards do not cover every deliberately conflicting marker in one ID3
tag, duplicate MP4 items, FLAC Xiph-versus-ID3 markers, or ID3 hidden after
sync-like junk. Such source-review scenarios may conceal a processed marker;
the exact crafted examples were not tested for decoder admission. See the
contract's marker limitation before processing untrusted modified files.

Refusals are typed in the Rust API; CLI JSON per-item errors carry a stable
machine-readable `code` (for example `unsupported_input_codec`,
`unsupported_sample_rate_conversion`, `artwork_conversion_unavailable`) beside
`stage` and `message`, and the command's exit status is 1 for failure or
partial failure. A waveform unavailability is a report field, not automatically
a failed analysis. See [CLI_CONTRACT.md](../research/CLI_CONTRACT.md) for the
complete code table, the bridge status-3 rules, marker sniffing, fields and
transaction semantics.

## Opt-in MP3 input and cover art

Hermes-style callers that must process MP3 files and keep cover art on Linux set
one boolean in the state directory once; the `process` command line does not
change:

```sh
platinum --state-dir "$STATE" config set parity.allow_non_identical true
```

With it on (Linux and `--features portable` builds only):

- **MP3 input** is decoded by an open-source, deterministic Layer III decoder
  (safe Rust, binary64, no libm) and `match-input` writes MP3 through the pinned
  LAME as before. The decoder emits AudioToolbox's 16-bit sample grid but is not
  byte-exact: in a 389-file generated corpus 95.08 % of 46.1 million samples
  equal AudioToolbox's and the largest difference is 4 LSB, so the output is not
  guaranteed byte-identical to macOS (its own output is bit-identical on macOS
  arm64, Linux aarch64 and Linux x86_64). Xing, Info and VBRI headers are read as
  AudioToolbox reads them and a truncated final frame is accepted. Layer I/II,
  free-format, mixed-block, MPEG-2 intensity-stereo, Xing/Info headers without a
  usable frame count, ambiguous data before the first or after the last frame and
  audio after a gap are refused with `unsupported_input_codec`; damaged streams
  fail with `invalid_input_file`. MPEG rates other than 22,050, 32,000, 44,100
  and 48,000 Hz need conversion and are refused with
  `unsupported_sample_rate_conversion`. Only a confirmed MP3 stream is claimed:
  any other input keeps the classification and code it has without the setting.
- **Embedded cover art** (FLAC `PICTURE`, ID3 `APIC`, MP4 `covr`) is stored in the
  output byte for byte where macOS would re-encode it as PNG (cross-format
  copies, refused by default on Linux) or drop it (FLAC to FLAC). Same-family
  ID3, MP4 and AIFF copies already preserved the picture and are unchanged. WAV
  destinations are unchanged as well: as on macOS, a picture survives in a WAV
  only when the ID3 chunk that carries it is not the chunk the marker rewrite
  discards.
  The item is refused with `artwork_conversion_unavailable`, before anything is
  written, when a picture is not a structurally complete JPEG, PNG, GIF, BMP or
  WebP, when MP4 `covr` would have to hold WebP, or when a FLAC block would
  exceed 16 MiB − 1. This applies to FLAC to FLAC too: without the setting (and
  in v0.1.0) a FLAC cover that is not a complete image is dropped and the item
  succeeds, while with the setting on the same file fails instead of losing the
  picture silently. Leave the setting off for such sources.

Each item that took one of these paths says so: `report.non_identical_paths` is
`["mp3_decode"]`, `["artwork_copy"]` or both (always present, `[]` otherwise).
Without the setting Linux behaves exactly as in v0.1.0. On a macOS default build
the setting is stored but has no effect, and `config show` says so. AAC stays
refused either way. The setting and the item list are specified in
[CLI_CONTRACT.md](../research/CLI_CONTRACT.md#opt-in-non-identical-paths).

## Floating-point and reference limits

The portability component reference used Apple M4 with SME, macOS 27.2 build
26B123, and the recorded 64-byte aligned Accelerate FFT profile. The earlier
native build recipe recorded macOS 27.2 build 26B118 and the macOS 27.0 SDK.
Accelerate dispatch and AudioToolbox AAC behavior can vary across Apple Silicon
generations and OS/compiler versions; do not assume another system is an
identical reference. The native pitch detector's literal GUI allocation was not
observed, although all captured sensitive native exports match aligned-0.

On x86_64, MXCSR flush-to-zero tests tininess after rounding, while ARM FPCR.FZ
tests it before rounding. Ordinary arithmetic can therefore retain the smallest
normal float where ARM returns signed zero. Portable helpers explicitly model
relevant boundaries but not every ordinary DSP operation is covered. Invalid
operations can also produce a default NaN of a different sign, and competing
NaNs can select different payloads or signs. Vectorized FMA can reverse operand
order relative to scalar modeling. Plain min/max signed-zero and NaN behavior
also differed in the sampled probes. These classes can affect raw bits; the
sampled probes do not prove that all such values are reachable in the engine,
and no general runtime guard for all such cases is claimed.

## Linux native libraries

The ABI-5 bridge and codec libraries are loaded from explicitly configured,
trusted paths. Linux additionally permits only exact, architecture-specific
SHA-256 identities compiled into `src/linux_libraries.rs`. The v0.2.0 bridge
adds the optional artwork-copy exports and therefore has a new identity; libFLAC
and LAME are the v0.1.0 builds (the v0.1.0 bridge digests are no longer
accepted):

| Architecture | TagLib bridge | libFLAC | LAME |
| --- | --- | --- | --- |
| aarch64 | `8708af52d83fc7fdb6a487d72abc90ff4bd9f9b94e313436cc6fb68cc9db2aa0` | `41b59a452fcd7245a0609a924b8cfcd7873e7b740945d49cd25c34321ec01e72` | `c780062f806c3b04ab00b777efd9f842f00800724f7183815374fdabd89c9d87` |
| x86_64 | `ea86881cb6615d7cf8614dfb285e8670d978de621957b15611b8bd6d5b0e1ad5` | `9420c96f562aec7734f09469ed07def4444471cf90d2c3fd70bc257e847b8755` | `cf59ae7fc6445a2b1d24465f423865e971e966b780c3f82d268cd397dbef675b` |

FLAC and LAME also require matching user-configured digests. Supplying the
hash of a different library does not override the built-in allowlist. The
loader hashes the opened regular file, retains the descriptor and loaded
handle, checks the digest after loading, and checks a mapped symbol's identity.
If post-load hashing or mapped-symbol verification fails, the descriptor and
loaded handle remain quarantined for the process lifetime and that identity is
not returned for reuse.
These checks do **not** make a writable library inode immutable: a concurrent
writer with permission to modify the same inode can change bytes during
loading, and native library initializers may execute before a post-load check.
Use trusted libraries on storage protected from such writers. SHA-256 does not
establish provenance and dynamic libraries execute native code.

Docker Desktop on macOS has been observed to report a mapped device/inode
identity mismatch for a bind-mounted bridge, while the identical in-image
library loaded; the exact mismatch mechanism was not retained. Keep libraries
inside the image, and put state on a named volume or tmpfs. A bind-mounted
filesystem may also have unstable identities for file-workflow checks; use
container storage when that happens rather than disabling the checks.

## Container image

On release publication, GitHub Actions downloads the published Linux archives
and checksum manifest, verifies their pinned identities and the packaged binary
and library hashes, and builds the image from those payloads without rebuilding
the CLI. Native runners publish each architecture and smoke-test the resulting
multi-architecture image with synthetic audio. Manual workflow dispatch accepts
a published tag whose pins are reviewed in the workflow.
The smoke test is one bounded comparison, not a universal parity guarantee.

When published, the multi-architecture image is
`ghcr.io/mschienbein/platinum-engine:v0.2.0`. It contains the CLI, libraries,
notices and corresponding-source/relink materials. It uses UID/GID 65532 and
runs `platinum` as its entrypoint; it does not set the library configuration
automatically. The following gives the named volume a persistent state and uses
libraries from the image. Verify the three printed hashes against the table in
"Linux native libraries" for the image's architecture before configuring them;
the v0.2.0 image carries the v0.2.0 bridge. The v0.1.0 image's bridge digests
(aarch64 `10eb77c1e12d7a12959300da3a1d853fe4907a3c7872bb4f83b12ab255549a9b`,
x86_64 `27922fdf10e6c7efbb43f8645f74138b82c4598edf0c3f2e2f55410cbaf3714d`) are
not in that table, and this release refuses that bridge.

```sh
IMAGE=ghcr.io/mschienbein/platinum-engine:v0.2.0
pn() {
  docker run --rm --mount type=volume,src=platinum-engine-state,dst=/state \
    "$IMAGE" --state-dir /state "$@"
}
docker run --rm --entrypoint sha256sum "$IMAGE" \
  /opt/platinum-engine/lib/libpn-tag-bridge.so \
  /opt/platinum-engine/lib/libFLAC.so /opt/platinum-engine/lib/libmp3lame.so
FLAC_SHA=$(docker run --rm --entrypoint sha256sum "$IMAGE" /opt/platinum-engine/lib/libFLAC.so | cut -d ' ' -f 1)
LAME_SHA=$(docker run --rm --entrypoint sha256sum "$IMAGE" /opt/platinum-engine/lib/libmp3lame.so | cut -d ' ' -f 1)
pn config set libraries.tag_bridge '"/opt/platinum-engine/lib/libpn-tag-bridge.so"'
pn config set libraries.flac '"/opt/platinum-engine/lib/libFLAC.so"'
pn config set libraries.flac_sha256 "\"$FLAC_SHA\""
pn config set libraries.lame '"/opt/platinum-engine/lib/libmp3lame.so"'
pn config set libraries.lame_sha256 "\"$LAME_SHA\""
pn --json templates list
```

For processing, add a read-only input mount and a separate output mount to
`docker run`, e.g. `--mount type=bind,src="$INPUT_DIR",dst=/music,readonly`
and `--mount type=bind,src="$OUTPUT_DIR",dst=/output`, then use container paths
and `--output-dir /output`. Both host directories must already exist, and the
output must be writable by UID/GID 65532. Check file identity behavior on the
actual filesystem; an unstable bind mount can be refused. Do not mount your
input writable unless deliberately using replacement with an independent backup.
For a temporary state instead, use a tmpfs at `/state` writable by UID 65532;
that state disappears with the container.

## Build from source

`linux/Dockerfile` uses the multi-architecture Rust 1.99.1 image by digest and
Debian's 2026-10-25 package snapshot. It verifies public source archives and
builds TagLib 1.11.1 as PIC static code in a separate ABI-5 bridge, plus shared
no-Ogg libFLAC 1.4.2 and LAME 3.100. Linux codec patches and the codec-local
portable math shim are checked by `linux/patches/codec-math.sha256`; there is no
process-global libm interposition. The build records configuration and input
hashes under `/opt/platinum-native/receipt`. Changing any checked input requires
review and repinning; an arbitrary rebuild need not satisfy the CLI allowlist.

From the repository root, using Docker on the target's native architecture,
choose new, absent output directories with existing parents:

```sh
sh tools/linux/build-image.sh platinum-native:local "$HOME/platinum-build-context-v0.2.0"
sh tools/linux/build-cli.sh platinum-native:local "$HOME/platinum-build-output-v0.2.0"
```

The first command creates a context from precisely the public native inputs;
the second mounts the source read-only and writes Cargo output only to its new
output directory. By default the CLI build can fetch locked dependencies. Set
`PN_LINUX_VENDOR_DIR` or `PN_LINUX_REGISTRY_DIR` to a prepared dependency cache
for a network-disabled offline build; `PN_LINUX_RUN_TESTS=1` additionally runs
the regular all-target test suite and then a library-gated lane: the
`error_codes`, `linux_opt_in` and `portable_art_copy` suites with `--ignored`,
which need the approved bridge, FLAC and LAME in the image and are ignored
(never counted as passes) in the regular suite. The dual-host `full-build-test.py` helper
is the recorded developer harness: it defaults to the `ns2` SSH host and
restricts remote scratch to a direct child of `/home/mooki/platinum-linux-build/`
or an explicitly named `/dev/shm/platinum-linux-build-NAME`. Those literal
host-specific defaults are intentional, not a generic installation recipe.
Inspect its options and prerequisites before using or adapting it; tmpfs
staging consumes memory shared with other services.

The release packaging helper is `sh tools/linux/package-release.sh PLATFORM
v0.2.0 NEW_OUTPUT_DIR`, where PLATFORM is `linux-arm64`, `linux-amd64`, or
`macos-arm64`; it does not publish. The runtime image recipe is
`linux/Dockerfile.runtime` and expects verified extracted release payloads for
both architectures. [BUILD.md](BUILD.md) and [THIRD_PARTY_NOTICES](../THIRD_PARTY_NOTICES)
cover source checksums, attribution and relinking. A successful build or
package is not a universal parity certification.
