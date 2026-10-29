# Linux builds and runtime

Platinum Engine v0.3.0 targets Linux aarch64 and x86_64 as well as the macOS
arm64 reference. The default objective for admitted Linux operations is an
identical complete output file, including encoded samples, container, tags and
marker; known unsupported operations are refused or reported unavailable
instead of being approximated. The evidence is finite and there are known x86
arithmetic residuals, so this is not a guarantee that every mismatch can be
detected at runtime. Linux aarch64 is the stricter parity build. One explicit
opt-in, [described below](#opt-in-cover-art-and-wav-tags), allows two paths
that cannot be proven identical. A second, separate opt-in adds Opus, which
Platinum Notes does not accept and which therefore has [no reference to
match](#opt-in-opus-input-and-output). If you use v0.2.0, read [Upgrading from
v0.2.0](#upgrading-from-v020) first. See [VERIFICATION.md](VERIFICATION.md) for
the individual evidence and limits.

In the v0.1.0 frozen-source battery (the later differences are listed in
[VERIFICATION.md](VERIFICATION.md)), each architecture had 104 complete native
file matches, 25 documented typed refusals and three matching native
error/no-output cases, plus 16 matching workflow cases. In a separate 575-case
stress corpus, each Linux architecture matched macOS bytes in 535 cases and
refused 40; all 30 additional pitch cases matched. Refusals are not counted as
matches, and the x86 residual classes were not proved absent by those runs.

## What is refused

- AAC input decoding and AAC output encoding: the Apple decoder/encoder
  behavior is not reproduced on Linux, with or without either opt-in; Rust
  ports of the AudioToolbox AAC paths are research and are not part of this
  release. MP3 input is decoded exactly (see [MP3 input](#mp3-input)); only MP3
  inside a WAVE container (unless the name ends in `.mp3`) and rates that need
  conversion are refused. MP3 and FLAC *encoding* use the pinned public LAME
  and libFLAC builds described below.
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

## MP3 input

MP3 input needs no setting. `src/portable/audio/mp3/` is a port of what
AudioToolbox does for an MP3 file: the AudioFile container parser (sync search,
ID3v2/ID3v1, Xing/Info/LAME/VBRI, packet table, priming and remainder), the
`ExtAudioFile` trimming and read rules, and Apple's integer Spirit DSP decoder
(Layer I, II and III), transcribed from the macOS 27.2 (26B123) arm64e
`AudioCodecs` component. It is integer arithmetic only, so it gives the same
16-bit samples on macOS arm64, Linux aarch64 and Linux x86_64, and the same
`OSStatus` refusals and read errors (`'dta?'`, `'wht?'`, `-39`, `'fmt?'`,
`'bada'`) that `ExtAudioFileOpenURL`/`ExtAudioFileRead` return. The engine's
reader reports it like any other codec: source description, file length, bitrate
and float samples (`int16 / 32768`) are those of the macOS reference, and
`report.non_identical_paths` never lists it. (v0.2.0 shipped an opt-in,
non-byte-exact open-source decoder in its place; it no longer exists, see
[Upgrading from v0.2.0](#upgrading-from-v020).)

Evidence: generated corpora of 7,100, 2,300 and 6,440 files (every sample rate,
channel mode, block type, tag layout and damage class the maintainers could
construct) were read through the engine's own macOS `apple_codec::Reader` and
compared with this port with no mismatch in open outcome, properties, delivered
frames, the `OSStatus` of every refusal and read failure, or any sample, at
512-frame reads and at 100-, 1153- and 4096-frame reads, plus several hundred
thousand live comparisons over other read sizes and mixed read sequences.
A 315-file subset with Apple's recorded results, and 63 of those files again at
100-, 513- and 4096-frame reads (both interleaved and planar), is checked in at
`tests/fixtures/mp3-exact/` and replayed by `tests/portable_mp3.rs` on every
`cargo test` on macOS (`portable`), Linux aarch64 and Linux x86_64; see
[VERIFICATION.md](VERIFICATION.md) and that directory's README.

The engine pulls whole blocks, and `ExtAudioFileRead` leaves the decoder's
padding frames in the buffer behind the frames it returns (it only lowers the
returned count); the port reproduces that (checked in
`tests/fixtures/mp3-exact/tails.json`, 188 recordings over 62 inputs, and on
2,379 inputs at three block sizes in two buffer shapes with no difference). With
it, `process` output for MP3 input is byte-identical to the macOS default build
at every sample rate both can process: of the 922 generated inputs that both
processed, 910 gave identical output files, and the 12 others are synthetic
fuzz-profile streams on which Apple's own decoder output depends on what the
process decoded before, which a port cannot reproduce.

Boundaries (all refusals are typed; none changes a result silently):

- The contract is AudioToolbox's behaviour for the same sequence of read sizes:
  sequential reads of any size deliver the frames, and fail with the status,
  that `ExtAudioFileRead` does for those sizes. A failed read loses the frames
  it had gathered, so the number delivered before an error depends on the read
  size, on macOS and here alike (254 of the 15,840 corpus files differ between
  512 and 4096 frames, all of them streams that end in a read failure). Not
  modelled: seeking, a client rate other than the file's (refused), and reading
  from a reader on which the file length or bitrate was queried first
  (AudioToolbox's length query scans the whole file and changes what later reads
  see; the engine queries both on a reader it never reads from).
- The file is read into memory; inputs larger than 2 GiB are refused with
  `unsupported_input_format`.
- MPEG rates other than 22,050, 32,000, 44,100 and 48,000 Hz need the
  processing-rate conversion and are refused with
  `unsupported_sample_rate_conversion` (`inspect` still reports them).
- Which reader opens a file follows `AudioFile`. A name ending in `.mp3` (also
  `.mp1 .mp2 .mpa .mpeg`, any case) is read by the MPEG reader whatever the
  bytes are: a WAVE or AIFF file named `.mp3` is refused as
  `invalid_input_file` (`ExtAudioFileOpenURL`, `'dta?'`) exactly as on macOS,
  and MP3 inside a WAVE container (format tag 0x55) named `.mp3` decodes, its
  WAVE header being junk before the first frame. Under any other name the
  content decides: leading ID3 blocks are stepped over, WAVE, AIFF, CAF, MP4,
  ADTS, FLAC and Ogg are told by their signatures, a valid MPEG frame header
  selects the MPEG reader, and anything else is `'typ?'` (also
  `invalid_input_file`). MP3 inside a WAVE container under another name is
  refused with `unsupported_input_codec`. AAC and FLAC data under a name that
  does not select their reader, and Ogg audio, are typed refusals
  (`unsupported_input_codec` / `unsupported_input_format`) because AudioToolbox
  decodes them and this build does not. Layer II at 22,050 Hz is refused as
  `unsupported_sample_rate_conversion` (macOS fails it mid-read with
  `decode_failed`). The remaining differences from the macOS default build that
  the admission matrix found are listed in [VERIFICATION.md](VERIFICATION.md).
- Apple never returns from opening a file with a chained ID3 header whose
  version byte is 0xff; this port refuses it with `'dta?'`.

## Opt-in cover art and WAV tags

Hermes-style callers that must keep cover art and WAV tags on Linux set one boolean in the
state directory once; the `process` command line does not change:

```sh
platinum --state-dir "$STATE" config set parity.allow_non_identical true
```

With it on (Linux and `--features portable` builds only):

- **Embedded cover art** (FLAC `PICTURE`, ID3 `APIC`, MP4 `covr`) is stored in the
  output byte for byte where macOS would re-encode it as PNG (cross-format
  copies, refused by default on Linux) or drop it (FLAC to FLAC). Same-family
  ID3, MP4 and AIFF copies already preserved the picture and are unchanged. A
  WAV is an ID3-capable container in these generic copies, like MP3 and AIFF:
  the picture of a FLAC, MP3, AIFF, M4A or WAV source is stored unchanged as an
  `APIC` frame of the WAV's `ID3 ` chunk (the item lists `artwork_copy` and,
  because the chunk is kept, `wav_tag_preservation`), and the `APIC` frames of
  a WAV source are the pictures copied into FLAC, MP3, AIFF and M4A outputs.
  Type, description (kept in its text encoding for ID3 sources), canonical MIME
  and the checks below apply to WAV exactly as to MP3 and AIFF. WAV to WAV and
  `match-input` are direct copies and are unchanged: a WAV's picture travels
  with its ID3 chunk through `wav_tag_preservation`, as for FLAC, MP3, AIFF and
  M4A same-format processing. With the setting off, no picture moves into or
  out of a WAV on a cross-format copy (it is ignored, not refused), as in the
  reference; v0.2.0 never carried a picture into or out of a WAV either.
  The item is refused with `artwork_conversion_unavailable`, before anything is
  written, when a picture is not a structurally complete JPEG, PNG, GIF, BMP or
  WebP, when MP4 `covr` would have to hold WebP, when a FLAC block would
  exceed 16 MiB − 1, or when the destination's ID3v2 tag (MP3, AIFF or WAV
  output: the pictures, their frame headers and descriptions in the encoding
  they are written in, the tag's other frames, the processed-marker frame and
  TagLib's padding) would exceed the 268,435,455-byte limit of the tag's
  28-bit size field. This applies to FLAC to FLAC too: without the setting (and
  in v0.1.0) a FLAC cover that is not a complete image is dropped and the item
  succeeds, while with the setting on the same file fails instead of losing the
  picture silently. Leave the setting off for such sources.
- **WAV tag chunks** (`ID3 `/`id3 ` and INFO `LIST`) that TagLib writes into a WAV
  output are all kept. macOS and the original application lose the chunk TagLib
  saves last (the `ID3 ` chunk, or the `LIST` when both exist) because their
  marker rewrite runs while TagLib's RIFF size update is still buffered; with the
  setting on, a WAV source's tags and artwork survive WAV-to-WAV and
  `match-input`, and the output is the default output plus those chunks. A
  cross-format copy into a WAV always writes tempo and key, so with the setting
  on even a source without tags gains an `ID3 ` chunk (`TBPM` "0", an empty
  `TKEY`) and the item lists `wav_tag_preservation`; this policy is unchanged
  in v0.3.0 and remains an open point. Without the setting the original
  behavior is kept byte for byte.

Each item that took one of these paths says so: `report.non_identical_paths` is
`["artwork_copy"]`, `["wav_tag_preservation"]` or both (always present, `[]`
otherwise). Without the setting Linux refuses what needs the transcoder and
drops those chunks, as in v0.1.0. The setting does not gate MP3 input (it did
in v0.2.0). On a macOS default build the setting is stored
but has no effect, and `config show` says so. AAC stays refused either way. The
setting and the item list are specified in
[CLI_CONTRACT.md](../research/CLI_CONTRACT.md#opt-in-non-identical-paths).

## Upgrading from v0.2.0

- **Libraries.** Library paths and digests live in the state directory. The
  v0.3.0 Linux tag bridge (v09, still ABI 5) has a new SHA-256 (see
  [Linux native libraries](#linux-native-libraries)) and the v0.3.0 CLI refuses
  the v0.2.0 bridge, so `libraries.tag_bridge` must be repointed at the v0.3.0
  bridge. libFLAC and LAME are byte-identical to v0.2.0 and keep their pins;
  libopus is new and needed only for Opus. Unpack v0.3.0 into a new directory
  next to v0.2.0.
- **State file.** A state file that stores `parity.allow_non_identical` stays
  valid and keeps its meaning for cover art and WAV tag chunks. A file written
  by v0.3.0 that stores `formats` or `libraries.opus*` (only after you set
  them) cannot be read by v0.2.0 or v0.1.0; a file that never set them can.
- **MP3 input.** In v0.2.0 MP3 input on Linux was refused with
  `unsupported_input_codec` unless `parity.allow_non_identical` was on, and then
  an open-source decoder that was not byte-exact decoded it and the item listed
  `mp3_decode` in `report.non_identical_paths`. In v0.3.0 MP3 input is decoded
  exactly with no setting: the `mp3_decode` member and the setting's MP3 gating
  are gone, the setting covers only cover art and WAV tag chunks, and MP3 input
  is no longer refused when it is off. Samples decoded from MP3 (and files
  encoded from them) can therefore differ from those v0.2.0 produced with the
  setting on. Consumers that tested for `mp3_decode` should stop doing so; the
  contract already told them to treat unknown members as non-identical.
- **Name-based admission.** A RIFF/WAVE or AIFF file named `.mp3` is refused as
  `invalid_input_file` exactly as on macOS (v0.2.0 processed it by content).
- **Cover art.** With the setting on, generic copies now carry pictures into and
  out of a WAV (v0.2.0 carried none), and a destination ID3v2 tag above the
  268,435,455-byte limit is refused with `artwork_conversion_unavailable` (v0.2.0
  let about sixteen maximal FLAC pictures through and TagLib wrote the size
  field modulo 2^28). With the setting off nothing changed.
- **Error codes** grew from 37 to 39 (`opus_not_enabled`,
  `unsupported_opus_variant`); the existing codes keep their meaning.

## Opt-in Opus input and output

Opus is not part of Platinum Notes (it refuses `.opus`), so there is no reference to match and no parity claim; it is a separate opt-in, `formats.opus`, independent of `parity.allow_non_identical` (see [CLI_CONTRACT.md](../research/CLI_CONTRACT.md#opt-in-format-opus)). It needs the pinned fixed-point libopus 1.6.1, which the image provides at `/opt/platinum-engine/lib/libopus.so.0`:

| Architecture | libopus (`libopus.so.0`, built by `tools/linux/build-libopus.sh` with the image's clang 14.0.6) |
| --- | --- |
| aarch64 | `c09146de56e00143e350f3f988ce2e1e01048f58b6a159c93dc176f5899fc7c6` |
| x86_64 | `d09791af2f266f370537a0312490abf16cc882f2bd5cbc89b6f288522513b1f0` |

```sh
OPUS_SHA=$(docker run --rm --entrypoint sha256sum "$IMAGE" /opt/platinum-engine/lib/libopus.so | cut -d ' ' -f 1)
pn config set libraries.opus '"/opt/platinum-engine/lib/libopus.so"'
pn config set libraries.opus_sha256 "\"$OPUS_SHA\""
pn config set formats.opus true
```

The same Opus input gives the same output bytes on macOS (default and `portable`), Linux aarch64 and Linux x86_64: libopus is built fixed-point without SIMD, float API or run-time CPU detection, so it uses no libm and no vector unit whose rounding could differ (an arm64 NEON build is *not* bit-exact against the C code, an x86 SSE4.1/AVX2 build is), and the loader runs a known-answer encode and decode before trusting any library. Another compiler's build of the same source also matched in the recorded matrix (`local-artifacts/opus-lane-01/libopus/`); it can be used by adding its digest to the allowlist. As with the bridge, a library bind-mounted into a container may be refused (the loader checks the mapped image against the opened file): copy it into the container.

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

## Hostile-input limits

The portable readers and the tag bridge parse untrusted files in the engine's
process. The v0.3.0 reviews (a static review, a code review and a hands-on
security review that fuzzed 631,050 mutated MP3 files and 657,996 structure-aware
Ogg Opus files without a panic, hang or out-of-range read, and ran the bridge
under ASan/UBSan over about 75,000 calls with no report) found no memory-safety
defect, and recorded these bounded limits, none of which is new in kind:

- **Memory.** An MP3 is read whole (at most 2 GiB) and a 16-byte entry is kept per
  packet, so 2 GiB of minimum-size frames needs about 3.2 GB while an empty
  2 GiB file `inspect`s at about 2.2 GB. An Opus comment header of up to 64 MiB
  is held several times (about 7x the file). Every figure multiplies with
  `--jobs`; the default is the core count.
- **Decoded length.** Neither engine path caps the duration a file claims: a
  117 KB Opus file can claim 115 million frames and `process` then writes an
  intermediate of about 0.9 GB to `$TMPDIR` (the MP3 reader has the same
  property). Apply a size, duration or free-space policy outside the engine for
  untrusted input.
- **Worker panics.** A panic in one worker thread ends the whole run (exit 101,
  no JSON outcome); none is known to be reachable.
- **Pictures.** With the opt-in, a FLAC or MP4 destination has no aggregate
  picture cap; TagLib 1.11.1 sizes its byte vectors in 32 bits, so pictures
  totalling 4 GiB or more (and roughly 12 GiB of memory) are outside what is
  supported. ID3 destinations (MP3, AIFF, WAV) are checked exactly.
- **Hardening.** The bridge is built with `-fstack-protector-strong`,
  `-D_FORTIFY_SOURCE=2` and `-z relro,now`; the TagLib static library it links
  and libopus are not.

## Linux native libraries

The ABI-5 bridge and codec libraries are loaded from explicitly configured,
trusted paths. Linux additionally permits only exact, architecture-specific
SHA-256 identities compiled into `src/linux_libraries.rs`. The v0.3.0 bridge
(v09) adds WAV picture handling and the ID3 size check to the v0.2.0 bridge
(v08, aarch64 `8708af52d83fc7fdb6a487d72abc90ff4bd9f9b94e313436cc6fb68cc9db2aa0`,
x86_64 `ea86881cb6615d7cf8614dfb285e8670d978de621957b15611b8bd6d5b0e1ad5`) and is
built with stack protector, `_FORTIFY_SOURCE=2` and immediate binding with a
read-only relocation segment, so it has a new identity; libFLAC and LAME are the
v0.1.0 builds, and libopus is the optional fixed-point 1.6.1 build (the v0.1.0
and v0.2.0 bridge digests are no longer accepted):

| Architecture | TagLib bridge | libFLAC | LAME | libopus |
| --- | --- | --- | --- | --- |
| aarch64 | `5c52c5f16da96177a98b3f7569257aeb33d7024b235903da7f0afaecccf73cde` | `41b59a452fcd7245a0609a924b8cfcd7873e7b740945d49cd25c34321ec01e72` | `c780062f806c3b04ab00b777efd9f842f00800724f7183815374fdabd89c9d87` | `c09146de56e00143e350f3f988ce2e1e01048f58b6a159c93dc176f5899fc7c6` |
| x86_64 | `8c61f6bba22ad2e92bb1774e695c3a1cca5a1658e9ffc86e3ef65418550cefb9` | `9420c96f562aec7734f09469ed07def4444471cf90d2c3fd70bc257e847b8755` | `cf59ae7fc6445a2b1d24465f423865e971e966b780c3f82d268cd397dbef675b` | `d09791af2f266f370537a0312490abf16cc882f2bd5cbc89b6f288522513b1f0` |

The bridge is compiled with `-O2 -fPIC -fvisibility=hidden
-fstack-protector-strong -D_FORTIFY_SOURCE=2` and linked with
`-Wl,-z,relro,-z,now`, the version script and `--exclude-libs`. `readelf` on
both architectures showed `BIND_NOW`, `GNU_RELRO`, the undefined
`__stack_chk_fail` and `__memcpy_chk`, and exactly the nine ABI exports. The
tag bridge ABI stays 5 and `pn_tag_bridge_features()` still reports 1; no
export was added.

FLAC, LAME and libopus also require matching user-configured digests. Supplying the
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
`ghcr.io/mschienbein/platinum-engine:v0.3.0`. It contains the CLI, libraries,
notices and corresponding-source/relink materials. It uses UID/GID 65532 and
runs `platinum` as its entrypoint; it does not set the library configuration
automatically. The following gives the named volume a persistent state and uses
libraries from the image. Verify the three printed hashes against the table in
"Linux native libraries" for the image's architecture before configuring them;
the v0.3.0 image carries the v0.3.0 bridge. The bridge digests of the v0.1.0
image (aarch64 `10eb77c1e12d7a12959300da3a1d853fe4907a3c7872bb4f83b12ab255549a9b`,
x86_64 `27922fdf10e6c7efbb43f8645f74138b82c4598edf0c3f2e2f55410cbaf3714d`) and
of the v0.2.0 image (listed above) are not in that table, and this release
refuses those bridges.

```sh
IMAGE=ghcr.io/mschienbein/platinum-engine:v0.3.0
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
no-Ogg libFLAC 1.4.2, LAME 3.100 and the fixed-point libopus 1.6.1
(`tools/linux/build-libopus.sh`). Linux codec patches and the codec-local
portable math shim are checked by `linux/patches/codec-math.sha256`; there is no
process-global libm interposition. The build records configuration and input
hashes under `/opt/platinum-native/receipt`. Changing any checked input requires
review and repinning; an arbitrary rebuild need not satisfy the CLI allowlist.

From the repository root, using Docker on the target's native architecture,
choose new, absent output directories with existing parents:

```sh
sh tools/linux/build-image.sh platinum-native:local "$HOME/platinum-build-context-v0.3.0"
sh tools/linux/build-cli.sh platinum-native:local "$HOME/platinum-build-output-v0.3.0"
```

The first command creates a context from precisely the public native inputs;
the second mounts the source read-only and writes Cargo output only to its new
output directory. By default the CLI build can fetch locked dependencies. Set
`PN_LINUX_VENDOR_DIR` or `PN_LINUX_REGISTRY_DIR` to a prepared dependency cache
for a network-disabled offline build; `PN_LINUX_RUN_TESTS=1` additionally runs
the regular all-target test suite and then a library-gated lane: the
`error_codes`, `linux_opt_in`, `portable_art_copy` and `opus_cli` suites with
`--ignored` (plus the WAV tag-chunk bridge tests of `tags_reference` and the
Opus layer's own `--lib opus::` tests),
which need the approved bridge, FLAC, LAME and libopus in the image and are ignored
(never counted as passes) in the regular suite. The dual-host `full-build-test.py` helper
is the recorded developer harness: it defaults to the `ns2` SSH host and
restricts remote scratch to a direct child of `/home/mooki/platinum-linux-build/`
or an explicitly named `/dev/shm/platinum-linux-build-NAME`. Those literal
host-specific defaults are intentional, not a generic installation recipe.
Inspect its options and prerequisites before using or adapting it; tmpfs
staging consumes memory shared with other services.

The release packaging helper is `sh tools/linux/package-release.sh PLATFORM
v0.3.0 NEW_OUTPUT_DIR`, where PLATFORM is `linux-arm64`, `linux-amd64`, or
`macos-arm64`; it does not publish. The runtime image recipe is
`linux/Dockerfile.runtime` and expects verified extracted release payloads for
both architectures. [BUILD.md](BUILD.md) and [THIRD_PARTY_NOTICES](../THIRD_PARTY_NOTICES)
cover source checksums, attribution and relinking. A successful build or
package is not a universal parity certification.
