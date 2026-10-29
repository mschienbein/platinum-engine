# Verification and evidence boundaries

The project uses complete-file identity as its reference-comparison criterion: audio, container headers, encoder output and metadata all count. Component tests and decoded PCM comparisons help locate differences; they do not substitute for a complete-file match.

## macOS reference tests and CI

The default macOS build uses that system's Accelerate and math libraries, as
the original application does. Tests asserting exact native values are tied to
the recorded Apple M4/SME and macOS reference environment; another Mac or OS
release can produce different bits without a build failure. The release battery
runs those default-mode reference tests on the reference Mac.

GitHub's macOS CI builds the default release and compiles its all-target tests
without running them. It then runs the `portable` all-target suite with
`--no-fail-fast`, excluding the `portable_math` `mac_reference::` module and
the `portable_vdsp` and `portable_vforce` `system::` modules, which compare
against the live host's math or Accelerate libraries. CI inventories test
names first and fails if those filters would exclude unrelated tests. These
reference-platform comparisons run on the M4 Mac in the release battery.
The portable math and vector implementations model the recorded M4 reference;
finite test coverage does not establish universal cross-platform parity.

## v0.3.0 changes and evidence

v0.3.0 replaces the opt-in MP3 decoder of v0.2.0 with a bit-exact port of
AudioToolbox's MP3 reader, adds one opt-in format (Opus), moves the
Linux/portable tag bridge to v09, aligns two refusals across platforms, pins one
more native call context on macOS and adds two error codes. It changes no
processing numerics. The evidence is kept separate by claim, each figure names
the platforms it was measured on, ignored tests are never counted as passes,
and the freeze and battery results of the release build are listed at the end
of this section.

- **Exact MP3 input (replaces the v0.2.0 opt-in decoder).**
  `src/portable/audio/mp3/` is a bit-exact port, in safe integer-only Rust, of
  AudioToolbox's MP3 reader path: the AudioFile container model (sync search,
  ID3v2/ID3v1, Xing/Info/LAME/VBRI, packet table, priming), the `ExtAudioFile`
  priming trim and read rules, the converter's packet-group reads with their
  `'bada'`/`'dta?'` read errors, the `AudioCodec` wrapper (529-frame decoder
  delay, end-of-file tail) and Apple's integer Spirit DSP Layer I/II/III decoder
  (macOS 27.2 build 26B123, arm64e slice
  `70e5d4a56b24d037b940328916df496a20267eb206f8e316216ae7bef69a1626` of
  `AudioCodecs`). It needs no setting and is **claimed identical** within these
  bounds: sequential reads of generated inputs, planar and interleaved, of any
  size, on one reference machine (Mac16,13, Apple M4). It replaces, and does not
  extend, the open-source decoder that v0.2.0 shipped behind
  `parity.allow_non_identical` and listed as `mp3_decode`: that decoder, the
  `mp3_decode` member and the setting's MP3 gating no longer exist, and Linux
  and `portable` builds no longer refuse MP3 input when the setting is off
  ([upgrade notes](LINUX.md#upgrading-from-v020)). Nothing is established for
  another macOS version or CPU generation, and seeking, a client rate other than
  the file's and reads from a reader on which the file length or bitrate was
  queried first are not modelled (the engine does none of these).
- **File-type admission.** The portable reader now decides which reader opens a
  file as AudioFile does: a name ending `.mp1 .mp2 .mp3 .mpa .mpeg` commits to
  the MPEG reader whatever the bytes are; under any other name the content
  decides. The rule is in [LINUX.md](LINUX.md#mp3-input). Like
  `ExtAudioFileRead`, an MP3 read that reaches the end of the valid frames fills
  the caller's buffer with every frame the decoder produced (the padding)
  and returns the lower count, and the engine pulls whole 512-frame blocks, so
  these frames reach its effects; `process` output now equals the macOS default
  build's byte for byte for the inputs below.
- **Opt-in Opus.** `formats.opus` (default off) adds Ogg Opus input and output;
  see the Opus evidence below. Platinum Notes refuses `.opus`, so there is no
  reference.
- **Tag bridge v09 (Linux and `portable`).** ABI 5 and the feature word
  (`pn_tag_bridge_features()` = 1) are unchanged and no export was added. With
  `parity.allow_non_identical` on, generic copies treat a WAV like MP3 and AIFF:
  the picture of a FLAC, MP3, AIFF, M4A or WAV source is stored byte for byte as
  an `APIC` frame in the WAV's `ID3 ` chunk and the `APIC` frames of a WAV source
  are the pictures copied into FLAC, MP3, AIFF and M4A (type, description,
  canonical MIME, validators and size limits as for MP3 and AIFF); WAV to WAV and
  `match-input` are direct copies and are unchanged, and with the setting off no
  picture moves into or out of a WAV (the output is byte-identical to v08 and
  v07). The picture reaches the finished WAV through the existing `wav_preserve`
  rewrite; no Rust change was needed. The destination ID3v2 tag is checked
  against the 268,435,455-byte limit of its 28-bit size field before anything is
  written (v08 counted `64 + picture bytes` per picture, let about sixteen
  maximal FLAC pictures through and TagLib 1.11.1 then wrote the size modulo
  2^28); this closes the v0.2.0 follow-up on the ID3 size preflight. The Linux
  bridge is built with `-fstack-protector-strong -D_FORTIFY_SOURCE=2
  -Wl,-z,relro,-z,now`; the macOS comparison dylib is built by
  `tools/portable/bridge/build-macos-portable.sh`. The macOS production bridge
  (v07) is unchanged. Digests: [LINUX.md](LINUX.md#linux-native-libraries).
- **macOS AAC decode call context.** AudioToolbox's AAC decoder has the same two
  output classes as its encoder, selected per `ExtAudioFileRead` call: the SME
  512-point inverse-transform kernel runs if and only if its stack temporaries
  are 64-byte aligned, that is when `sp % 64 == 16` at the read's entry (class
  S); every other residue runs the generic NEON kernel (class G), and the two
  round differently. The native application's reads and the engine's have been
  class G, but only through frame layout: the entry residue was whatever the
  caller's compiled frame produced (0 in the shipped v0.2.0 binary, measured
  under lldb; the AAC research recorded 32 for the interleaved read of another
  build). Both `ExtAudioFileRead`
  call sites now enter AudioToolbox through the same `call_with_sp_residue`
  trampoline that pins the encoder's write and dispose calls, with
  `READ_SP_RESIDUE` 0, so the decoder is class G by construction and no longer
  by compiler frame layout. Output is unchanged for every input the v0.2.0
  layout decoded. This is an observation of the tested macOS arm64
  AudioToolbox, not a cross-version guarantee; the verification of the pin is in
  the release evidence below.
- **Error codes.** 39 (the 37 of v0.2.0 plus `opus_not_enabled` and
  `unsupported_opus_variant`); the documented table, `ErrorCode::ALL` and the CLI
  scenario ledger are kept identical by a test. The 37/37 ledger under "v0.2.0
  release evidence" is v0.2.0's.
- **Alignment fixes** (no output byte changes). A RIFF/WAVE or AIFF file named
  `.mp3` is refused on Linux and `portable` as `invalid_input_file`
  (`ExtAudioFileOpenURL`, `'dta?'`) exactly as on macOS, because a name ending
  `.mp1 .mp2 .mp3 .mpa .mpeg` commits to the MPEG reader (v0.2.0 processed it by
  content). `inspect` of a symlink on macOS now reports `invalid_input_path`
  (stage `inspect source identity`) as on Linux; it reported `io_error`.
- **Held as-is.** A cross-format copy into a WAV under the opt-in always writes
  tempo and key, so a source without tags gains an `ID3 ` chunk holding `TBPM`
  "0" and an empty `TKEY` and lists `wav_tag_preservation`; keeping only chunks
  that carry source data was considered and is not changed (open). AAC input and
  output stay refused on Linux, and Rust ports of the AudioToolbox AAC paths are
  research and are not shipped. The unflagged direct M4A-to-MP3 copy still has
  no ID3 size check (reference parity). The macOS default build is otherwise
  unchanged.

### Exact MP3 input: evidence

All observations are on the reference machine named above, with generated audio
only; inferences are not listed as results.

1. *Three corpora against Apple.* `corpus` (7,100 files in 35 families: LAME
   CBR/VBR/ABR at every rate, bitrate and mode, ffmpeg/Lavc, every tag and
   container variant, ID3/APE/Lyrics, junk, tiny and long files, loud and
   overshooting streams, Layer I/II, foreign formats and about 3,500 damaged
   files), `fuzz` (2,300 files: 21 syntactic Layer III profiles and Layer II with
   mixed blocks, MPEG-1 and LSF intensity stereo, linbits, overflow, reservoir,
   free-format and corrupt streams) and `held` (6,440 files with fresh seeds
   never used in development: 5,000 random byte-level mutants of real streams,
   840 fresh fuzz-profile files, 600 fresh random LAME encodes). Per file the
   open or refusal status (numeric), the stream description, file length,
   bitrate, the float32 PCM of 512-frame planar reads, of interleaved 512-frame
   reads and of 4096-frame planar reads, and the numeric `OSStatus` of any read
   error were compared: 7,100, 2,300 and 6,440 files, **zero mismatches** on
   macOS arm64, Linux arm64 (Docker) and Linux x86_64 (Docker on ns2). Apple's
   results are the engine's own `apple_codec::Reader` driven by a probe, replayed
   from SHA-256-verified dumps on every platform and regenerated live for the
   held-out set.
2. *Live sweeps on the final reader.* Read sizes 1 to 8192, odd sizes, mixed
   size sequences and 26,000 regenerated mutation files: 41,360 + 113,740 +
   20,680 + 47,520 + 25,850 + 104,660 + 52,330 + 738 cases, all identical in
   frames, numeric status and sample SHA-256. 85 of the 7,100 corpus files
   differ between 512- and 4096-frame reads in Apple's own results, and the port
   reproduces that.
3. *Codec and function level.* Layer III 16-bit output equals Apple's
   `AudioCodec` output on 6,381 of 6,381 and 2,102 of 2,102 openable corpus and
   fuzz files; `DecodeFrame` was compared call by call (status, consumed bytes,
   PCM) on 174,088 real, damaged, fuzz and Layer I/II frames, on 44,570
   debugger stage dumps with the full decoder state and on 411,000 injected
   granules. These counts are the workers' reports; the lead re-ran the
   end-to-end comparisons above.
4. *The newer CPU slice.* The `arm64e.x1` slice, selected by newer cores, has 33
   of 33 tables and all read-only data byte-identical to the base slice; 61 of
   83 call-closure functions are identical and 22 differ only by CSSC, CPA or
   PAuth_LR toolchain instructions. Its machine code, with those three classes
   rewritten to their Arm-ARM equivalents, ran on the M4 against Apple's real
   image: 20.5 million `DecodeFrame` calls and 14 leaf routines at 10^7 vectors
   each, no mismatch. x1 silicon itself never ran.
5. *Hostile input and speed.* Whole-reader fuzzing (bit flips, byte rewrites,
   truncation, splices, inserts, header rewrites, duplicated regions; optimized
   with overflow checks and debug assertions on) ran 1.2 million iterations on
   the first assembly, 1.2 million after the structured-synthesis wave and,
   on the final build, 0.96 million at 512-frame reads plus 0.3 million at mixed
   read sizes: no panic, overflow-check failure or hang, slowest iteration
   207 ms (inputs up to 400 KB). 100 MB hostile files (random, zeros, 0xFF,
   repeated sync headers, 240,000 bad-payload frames) open or decode in 0.02 to
   1.1 s with a typed refusal or `'bada'`. The final build decodes at 973 times
   real time in the worst case (320 kbit/s white noise, 44.1 kHz stereo; whole
   process, one core) and 1,345 to 1,477 times real time for open and decode
   inside the engine tree on three platforms.
6. *Repository tests.* A 315-file subset with Apple's recorded results (and 63 of
   those files again at 100-, 513- and 4096-frame reads, planar and
   interleaved) and 40 recorded decoder-state granules are part of the test
   suite (`tests/portable_mp3.rs`, `tests/fixtures/mp3-exact/README.md`) and are
   replayed bit-identically on macOS arm64, Linux aarch64 and Linux x86_64 with
   no Apple component. In the integration tree (macOS, `--features portable`,
   at the time of the admission slice): `portable_mp3` 14 passed, 1 ignored;
   `portable_mp3_hostile` 2 passed (20,000 mutants, 1,500 of them through
   files); `portable_audio` 1 passed, 6 ignored; `--lib portable::audio` 43
   passed; the default build ran the same three targets with 14 and 2 passed
   and 1 passed with 39 ignored native captures. The research crate's own 37 unit
   tests passed in the research tree.

Limits: a chained ID3 header (after a valid first tag) whose version or
revision byte is 0xff makes Apple's open spin forever, and the port refuses it
with `'dta?'` (nothing to compare); MPEG-2.5 Layer I/II is opened by AudioFile
but its converter fails `'fmt?'`, which the port reproduces; states from which
Apple would corrupt memory or loop are total in the port (no panic, no loop) but
are not Apple's behaviour, and no test input reaches them; the references are
one OS build on one CPU; the full corpora, the Apple dumps and the table
generator and verifier scripts are research evidence, not part of the public
source.

### File-type admission and process output: evidence

Observed on the same machine with the engine's `apple_codec::Reader` and the
macOS default CLI against the `--features portable` CLI (the portable build used
the portable-08 macOS bridge dylib, as no v09 existed then, the default build
its production bridge). The matrix: 162 generated contents (real LAME and
ffmpeg MP3 and synthetic MPEG streams of every tag and rate variant, Layer I/II,
WAV, AIFF, AIFF-C, CAF, M4A ALAC/AAC/Opus, 3GP, ADTS, FLAC, hand-built Ogg
headers, stubs, MP3-in-WAVE, random, empty and one-byte files) under 721
(content, extension) cells; `inspect`, `analyze` and `process` on both binaries,
4,326 runs per matrix.

1. *Matrix.* Before the admission fix `portable` equalled the macOS default
   build in 624, 511 and 479 of 721 cells (`inspect`, `analyze`, `process`);
   after it, in 669, 556 and 546. Every remaining difference is classified (0
   unclassified): SRC not reproduced 0/112/122, AAC 20/20/20, FLAC by content
   8/8/8, Ogg 12/12/12, unverified variants 8/8/8, MP3-in-WAVE 4/4/4, and one
   pre-existing FLAC-reader cell for `analyze` and `process`. 28 cells whose
   portable output changed (22 MP3 cells from the buffer-tail fix, 6 ALAC and
   mono-WAV files named `.mp3` that macOS decodes as MPEG) now all equal the
   macOS default output.
2. *Admission rule.* `tests/fixtures/mp3-exact/admission.json` records 500 cases
   (421 identical to macOS, 79 typed platform refusals) and is replayed by
   `portable_mp3.rs` without Apple; the old routing fails 3 tests (194 of 500
   cases differ).
3. *Buffer tails.* `tests/fixtures/mp3-exact/tails.json` records 188 recordings
   over 62 inputs (164 with a written tail); a reader that writes only the
   delivered frames fails 164 of them. At reader level Apple default and
   `portable` are identical on 14,274 recordings (2,379 files, 512, 100 and 4096
   frames, planar and interleaved).
4. *End to end.* 2,379 MP3 inputs named `.mp3` (the 315 pack members and 2,064
   prefixes of them: 1 to 8 whole frames, some with 7 bytes cut off): `inspect`
   identical in all; `analyze` identical except the documented missing waveform
   (563 cells, non-44.1 kHz) and sample-rate refusals; `process`: both builds
   processed 922 inputs, **910 with byte-identical output SHA-256** and 12
   different; only the default build processed 946 (MP3 at 8, 11.025, 12, 16 and
   24 kHz and 24 kHz output: sample-rate conversion); both refused 511 (486 with
   the same code and stage, 25 with the sample-rate refusal against an Apple
   failure). The 12 are synthetic fuzz-profile streams (`p_intensity1`,
   `p_long`, `p_linbits`; no LAME or ffmpeg input is among them): the decoded
   samples are identical to Apple's at reader level and a WAV of them processes
   identically on both builds, but Apple's own output for these streams depends
   on what the process decoded before (the same file read planar, planar,
   interleaved at 44.1 kHz and planar again gives a different last pass, which
   is the order the engine decodes in), which a port cannot reproduce.
5. *Default build unchanged by the slice.* Default build before and after the
   admission edits: 721 of 721 cells identical in all three commands, output
   SHA-256 included. The slice was run on macOS only (no Linux run; the tests are
   platform independent by construction).

Remaining documented differences between the macOS default build and
`portable`/Linux for these inputs, all typed refusals or reference quirks:

| Class | What | Portable answer |
| --- | --- | --- |
| Sample-rate conversion | MP3 at 8, 11.025, 12, 16 and 24 kHz (analysis, process); WAV, AIFF and AIFF-C below 44.1 kHz such as 22.05 and 8 kHz; MP3 24 kHz output; Layer II 22.05 kHz (macOS: `decode_failed` mid-read) | `unsupported_sample_rate_conversion` |
| AAC | ADTS, MP4 `mp4a` under any non-`.mp3` name | `unsupported_input_codec` |
| FLAC by content | `fLaC` data under a name not ending in `flac` (Apple's FLAC reader decodes it) | `unsupported_input_codec` |
| Ogg by content | Ogg Opus under a name that is neither `.mp3` nor `.opus` (Apple opens it); Ogg FLAC and Vorbis variants (Apple: `'fmt?'`, `'dta?'`) | `unsupported_input_format` |
| Unverified variants | stereo IMA4 AIFF-C (macOS opens), Opus in MP4 (macOS: `'pck?'`, `invalid_input_file`) | `unsupported_input_format` |
| MP3 in WAVE (tag 0x55) | macOS: `'dta?'` for the usual layouts, opens some | `unsupported_input_codec` |
| ID3-prefixed WAVE | not a difference through the CLI: the tag stage refuses it before any reader opens it (`marker_unverifiable`, stage `inspect tags`) on every build, macOS included; the reader's own typed refusal (`unsupported_input_format`) is reachable only through the library API | typed |
| ADTS-like garbage | macOS answers `'typ?'`, `'fmt?'`, `'wht?'`, `'dta?'` or opens (the ADTS sniff rule was fitted on 17,000 random headers and has 4 outliers, at frame length 8 or 9) | AAC refusal or `'typ?'` |
| CAF | the engine never reaches the reader (the tag stage refuses CAF as `marker_unverifiable` on both builds); the reader handles float32 CAF only | typed |
| MP3 over 2 GiB | refused (decoded from memory) | `unsupported_input_format` |
| Apple decoder state | 12 synthetic fuzz streams: Apple's second decode in one process differs from its first | not modelled |

Not differences (identical on both builds): every `inspect` cell whose
container the engine's tag stage accepts; `m4p`, `wl.mp3`, `ogg`, `opus`, `caf`,
`mp2`, `flac`-named and extension-less inputs (refused before the reader, same
codes); the MP3 analysis waveform, which `portable` omits for non-44.1 kHz input
(`waveform_unavailable`, an existing guard). One observation that is not about
MP3 and is unchanged since v0.2.0: an Apple-encoded FLAC (`afconvert`) named
`.flac` is read by the macOS default FLAC path as 16,384 of 17,640 frames
(analysis volume `-Infinity`) and by `portable` as all 17,640; engine-encoded
FLAC is identical on both, and the v0.2.0 release binary reproduces the 16,384.
This is left as is.

### Opus evidence

v0.3.0 adds one opt-in format. **Platinum Notes refuses `.opus`, so there is no
reference to match**: nothing below is a parity claim, and each Opus item says
so in `report.no_platinum_notes_reference`. The claim is narrower and different:
*the same input and settings give the same output bytes on macOS default, macOS
`portable`, Linux aarch64 and Linux x86_64*. Everything in the processing chain
other than the Opus layer is unchanged. The Opus corpus runs below were made
on the v0.2.0 candidate tree plus the Opus patch, that is before the exact MP3
reader existed, so their MP3-input refusals on `portable` and Linux are those of
that tree; the measured numbers are reproduced unchanged.

The evidence is in `local-artifacts/opus-lane-01/` (scripts, generated inputs,
per-build results and logs; no real media):

1. *Default-off equivalence.* `tests/opus_cli.rs`: with `formats.opus` off,
   discovery ignores `.opus`, `inspect` reports the unchanged
   `unsupported_input_format`, `.ogg` stays unsupported, `--format opus` is
   refused with `opus_not_enabled`, and a default state file has no new member.
2. *libopus identity.* The fixed-point, no-SIMD, no-float libopus 1.6.1 built by
   `tools/linux/build-libopus.sh` was run through a 1,159-case, 127,032-packet
   encode/decode matrix (all frame sizes and TOC configurations, bitrates,
   VBR/CVBR, complexity, bandwidth, DTX/FEC, mid-stream control changes, PLC,
   FEC recovery, corrupted packets) on Apple clang (macOS arm64), gcc 12 and
   clang 14 (Linux aarch64 and x86_64) at `-O0/-O2/-O3` and LTO: 20 builds, one
   byte-identical result file. RFC 8251 vectors 01 to 12 pass (24/24) on three
   platforms with identical decoded streams. The same build under UBSan was
   clean. An arm64 NEON build is **not** bit-exact against the C code (74 of
   1,481 lines differ; `silk_warped_autocorrelation_FIX_neon`) and an x86
   SSE4.1/AVX2 build is, which is why the product build disables intrinsics.
   The shipped Linux libraries are rebuilt by the real `linux/Dockerfile` and
   equal the standalone results for clang 14.0.6 (receipts in
   `libopus/release-image/`).
3. *Ogg Opus conformance.* An independent verifier written from RFC 3533, 7845
   and 6716 (`corpus/verify_ogg_opus.py`) checks every file the CLI wrote:
   valid and supported, one logical stream, OpusHead version 1 / family 0 /
   48 kHz / pre-skip 312 / output gain 0, EOS present, exact sample count, the
   source comments verbatim and in order (minus the old marker and R128 gain
   tags), exactly one new marker where the report says `present`, pictures
   byte-identical. ffmpeg decodes 268 reference outputs to the same frame count.
4. *Four-build corpus.* A generated 392-file corpus (206 valid Opus files:
   11 signals mono and stereo, durations from one sample to 3 minutes, bitrates
   6 to 510 kbit/s, CBR/VBR/CVBR, 2.5 to 120 ms frames, applications, input
   rates 8 to 48 kHz, pre-skip, gain, initial-granule and end-trim variants,
   page layouts, tag and picture variants; 26 WAV/FLAC/ALAC/AAC/MP3/AIFF sources;
   160 hostile files) gives 1,156 engine cases. On all four builds: 794 cases
   identical in output SHA-256 and reported fields, 346 refused with the same
   code, 16 documented platform refusals (MP3/AAC input and MP3 artwork on the
   builds that did not have them at that tree), **0 different**.
5. *Generated PCM to Opus.* 54 48 kHz WAV sources (17 signals including
   silence, DC, sweeps, noise at -6/-30/-60 dBFS, impulses, clipped, music-like
   and speech-like material, mono and stereo, 16 and 24 bit; lengths of 1 to
   580,321 frames around the 960-sample frame and 312-sample lookahead): 54 of
   54 identical on all four builds.
6. *Opus to the other formats.* WAV, AIFF, FLAC, MP3 and AAC outputs of six
   sources (tags, pictures): 20 identical, 10 documented platform refusals
   (AAC output only exists on macOS default; artwork conversion on `portable`
   and Linux without the opt-in), none different. ALAC bytes carry the wall
   clock in the MP4 header, so ALAC is run with the hidden
   `--controlled-mp4-time-1904`: 4 identical, 2 artwork refusals.
7. *Hostile input.* 160 damaged or out-of-scope files (bit flips, truncation at
   every kind of boundary, bad checksums, sequence/serial/granule faults,
   chained and multiplexed streams, mapping families 1 and 255, more than two
   channels, zero-length and oversized packets, comment headers up to and just
   over the 64 MiB limit, non-Opus Ogg, non-Ogg bytes): none crashed and a
   refused file publishes nothing; every refusal or acceptance is in the expected class
   (160/160) and 130 carry exactly the corpus's code. The rest differ only in
   which check reports first (a non-Ogg file is reported by the marker check as
   `marker_unverifiable`; a malformed packet is `invalid_input_file` at scan
   time rather than `decode_failed`). `h_page_continued_first_audio` is
   RFC-valid (the lost first packet is dropped) but is refused rather than
   silently shortening the audio. Three fuzz tests (random damage, valid
   checksums, corrupt packets) run in the unit tests.
8. *Test counts per build* (`--include-ignored`, with the library, bridge and
   pins supplied): `cargo test --lib opus::` 32 passed, `tests/opus_cli.rs` 7
   passed, `tests/error_codes.rs` 19 passed on macOS default, macOS `portable`,
   Linux aarch64 and Linux x86_64.
9. *Library-gated Linux lane.* `tools/linux/build-cli.sh` (non-root, read-only
   tree, `PN_LINUX_RUN_TESTS=1`) on the frozen v0.2.0 tree plus the Opus patch:
   the build, the all-target tests (480 passed, 0 failed; 131 ignored on aarch64,
   129 on x86_64) and the 34 library-gated tests (`error_codes` 7, `linux_opt_in`
   14, `portable_art_copy` 5, `opus_cli` 4, Opus unit tests 4) pass on both
   architectures.

Limits: mapping family 0 (mono and stereo) only; 48 kHz processing with no
sample-rate conversion (other rates, more than two channels and chained or
multiplexed streams are refused with codes); no Opus waveform view; the ALAC
and AAC MP4 clock caveat and the Linux artwork and AAC refusals are the
existing ones (MP3 input is decoded exactly on Linux and `portable` since
v0.3.0). Float builds of libopus are not accepted (they import libm and
their output depends on FMA contraction); the loader runs a known-answer test
and refuses a library whose output differs.

### Tag bridge v09: evidence

Synthetic audio and synthetic images only; the native Platinum Notes application
was never launched. Receipts are in the private evidence directory.

1. *Flag off, retained v08/v07 matrices* on Linux aarch64 and x86_64: 604
   transfers, 86 inspections, 232 source-status cases, 36 legacy-MPEG cases, 37
   fault runs and 9 WAV-mode cases, identical to the retained v07 reports
   (ignoring only bridge digest, platform and temporary paths). The macOS
   portable v09 dylib against the native v07 production bridge: 690 rows, 0
   failures, 112 expected refusals; its 232 source-status rows equal v08's. No
   behavioural difference with the flag off.
2. *Artwork matrix.* 1,188 cases and 1,781 bridge runs (v08: 887 and 1,293), 0
   failures on aarch64, x86_64 and the macOS portable dylib, and the 1,781
   results (status, message, result flags, output SHA-256, callbacks, marker,
   TagLib read-back) identical between the two Linux architectures. The new WAV
   cells cover every pair FLAC/MP3/AIFF/MP4/WAV to WAV and WAV to
   FLAC/MP3/AIFF/MP4 for JPEG, PNG, GIF, BMP and WebP, 21 picture types, three
   description encodings, multiple, empty and mixed-empty pictures, the 41 valid
   corpus images, bad images refused with the destination unchanged, 10 MB and
   17 MB pictures and destinations that already hold artwork. Verified by TagLib
   read-back, by independent standard-library parsers and on the host (ffprobe
   and ffmpeg extracted 1,341 pictures byte for byte, `metaflac` 330 pictures,
   586 decoded-audio MD5s; 0 failures, WAV outputs included); audio payload bytes
   are unchanged. Against v08 on the 1,285 shared cells, 96 results differ, all
   of them WAV cells with the flag on.
3. *ID3 size-limit cells.* 40 cells per architecture, 0 failures, identical on
   both (68 records): sixteen near-maximal FLAC pictures, one huge ID3 picture,
   eight UTF-16-description pictures and a destination holding a 200 MB `PRIV`
   frame, into MP3, AIFF and WAV, with the rendered tag exactly on the limit
   (accepted, size field 268,435,455, re-reads, pictures byte-equal), one byte
   over only because of the marker frame (refused after the serialize callback,
   nothing written) and one byte over from the pictures alone (refused before any
   callback). v08 accepted all 28 over-limit cells it was given: the 24 MP3 and
   AIFF ones were written with a wrapped size field (0 for exactly 2^28), and a
   WAV stored no picture at all.
4. *Tests.* `portable_art_copy` has 12 tests (7 new); 12 of 12 pass with the
   macOS v09 dylib (`--features portable`, together with `linux_opt_in` 17 of 17
   and the three `tags_reference` WAV-chunk tests) and in the codec09 images on
   aarch64 and x86_64, run in the `build-cli.sh` form (read-only tree, offline
   registry). The new tests fail against the v08 dylib (7 of 12).
5. *Real engine path.* The `platinum` CLI (`--features portable` on macOS, and
   the Linux release binary built by `build-cli.sh` in each image): tag-less
   FLAC, MP3, ALAC (M4A), AIFF and WAV sources with PNG, JPEG, WebP, a
   four-picture set and none, converted to WAV, FLAC, MP3, AIFF and ALAC with the
   setting on and off: 240 runs, 0 failures on macOS, aarch64 and x86_64,
   identical records. On: every source to WAV and WAV to FLAC, MP3, AIFF and
   ALAC keep the picture bytes (type and description kept except for MP4, which
   has none), and items list `artwork_copy` (plus `wav_tag_preservation` into a
   WAV); WebP into ALAC and out of a WAV is refused like any source. Off:
   unchanged; no picture is carried into or out of a WAV (35 WAV-involved runs
   succeed without pictures), the other generic copies are still refused with
   `artwork_conversion_unavailable`, and WAV to WAV loses the ID3 chunk as before.
6. *Hardening.* `readelf` on both architectures: `BIND_NOW`, `FLAGS_1 NOW`,
   `GNU_RELRO`, undefined `__stack_chk_fail` and `__memcpy_chk`, and exactly the
   nine ABI exports. A fresh build of the real `linux/Dockerfile` and a derived
   image gave the same bridge, libFLAC, LAME and libopus digests on both
   architectures, and a container compile reproduced the pinned bridge.

`PN_COPY_MP4_TO_ID3` (the unflagged direct M4A-to-MP3 copy) has no size check:
reference parity, outside this change. The x86_64 base image is the ns2 rebuild
from v0.1.0 that v08 used; its libFLAC and LAME digests equal the pins.

All counts describe the recorded synthetic corpora and environments and are not
a universal parity proof.

### v0.3.0 release evidence (freeze-01)

The release tree is the frozen snapshot `freeze-01` (747 files; source manifest
SHA-256 `17e875b02efa5668690ab7d5ac6f97418b28c0ad0857fa9f5e45c5cb134d1396`,
snapshot manifest `d0077b33c1c79710c83ef63a84a83fa44c3f7643ee3f317df3a0faeb3c7f48f5`;
the build left the source unchanged). The text of README, this guide, LINUX.md,
the CLI contract and `release.yml` changed after the freeze (documentation and
workflow pins only; no source file).

| Binary | SHA-256 |
| --- | --- |
| macOS arm64 (source and path remapped, 0 `/Users/` paths) | `fa4e1f3e0a8c3bbac1f54ff0363006336c52fca7e00852e6d1cef1a898ffdd6d` |
| Linux aarch64 | `7157424200dd277949a90fec5d9c44e56560ca7613846f32d1e11bd152be6d8d` |
| Linux x86_64 | `c4e5e9539dda255b521ac2251ff297c7e5429b0976a42785bc8896b6b0907676` |

The macOS binary differs from v0.2.0's (`75c2ac87…`) because the exact-MP3
module, the Opus layer, the decoder read pin and the identity mappings are
compiled into it, so the whole macOS battery was rerun on it.

1. *All-target tests on the frozen tree.* Linux aarch64 532 passed, 0 failed,
   147 ignored (50 suites); x86_64 530 passed, 0 failed, 145 ignored. The
   library-gated lane (`error_codes`, `linux_opt_in`, `portable_art_copy`,
   `opus_cli`, the WAV tag bridge tests and the Opus unit tests, with the pinned
   native libraries) passed 48 of 48 on each architecture. macOS default from the
   snapshot: 569 passed, 0 failed, 200 ignored (50 suites), plus public-dependency
   38, waveform 1, `mp3-xing` 1, `error_codes` 8, `opus_cli` 4, Opus unit tests 4,
   `sp_pin_tests` 6 and 184 Node tool tests (154 `parity-matrix` tests, which now
   include the v0.3.0 `formats` members, and 30 others).
2. *macOS battery* on `fa4e1f3e…` (receipt `release-final-fa4e1f3e.json`, SHA-256
   `36bb6d5d1c7702a3609bb4bb489e8ac1d078b7533cdc537f01481170d5d786b8`; no
   failure, no unclassified difference): 71/71 retained replay cases, 8/8
   dynamics and pitch cases, 10/10 controlled-clock MP4 exports, the E05 case,
   5/5 boundary files, 11/11 strict family cases, 6/6 coverage extensions, the
   documented expected failures (u02: 5, f01: 4), the CLI smoke, the W04 exact-path
   workflow and the three workflows; 13/13 synthetic AAC outputs equal to the
   v0.1.0 and v0.2.0 binaries and 4/4 native AAC references on all three; every
   per-case output digest (120/120) and status (131/131) equals both the v0.1.0
   and the v0.2.0 baselines, and default behaviour matches both on 212/212
   comparison records. The only differences are the announced ones: the new
   `formats` and `formats_effect` members of the effective configuration, the
   reworded `parity_effect.note`, `inspect` of a symlink (including a dangling or
   directory symlink) reporting `invalid_input_path` instead of `io_error`, and
   `inspect` of a FIFO returning `invalid_input_path` instead of blocking. A macOS
   default MP3 reference of 315 pack members x inspect, analyze and process into
   WAV, AIFF, FLAC, MP3, ALAC and match-input (4,310 rows) matches the admission
   matrix (709/721 cells identical; the other 12 use seven afconvert-made
   contents that are not byte-reproducible). Three synthetic fuzz members
   (`fuzz/p_reservoir/p_reservoir_1_0022`, `fuzz/p_long/p_long_1_0024`,
   `fuzz/p_intensity1/p_intensity1_1_0062`) give different bytes on repeated runs
   of the same macOS binary, and did so identically in the v0.1.0 and v0.2.0
   binaries: any macOS reference for them is a set of outputs, and the Linux
   output equals one of them.
3. *Linux battery* (receipt `receipt-freeze-01.json`, SHA-256
   `8f9350e166b5069289563006df2a435add08348796f42787fb53e8eff9dd313b`; 83
   independent checks of the receipt pass; the loaded libraries of every run were
   verified against the approved digests; aarch64 and x86_64 are identical on every
   scenario and step):
   - Opt-in off: native references 113 exact, 16 typed refusals and 3 matching
     native errors on both architectures (v0.2.0: 104, 25, 3; 24 case-level
     differences, all in the intentional-change ledger: MP3 inputs and a file of Ogg
     bytes named `.mp3` that now equal the recorded Platinum Notes output, and a
     changed message for three misnamed Ogg files), 16/16 workflows, 535 of 575 stress
     cases byte-identical to the macOS default build (40 sample-rate refusals), 30/30
     pitch cases, the Docker Desktop bind subset 44 exact + 2 refusals + 1 native
     match + 16 workflows; a new 500-case corpus (20 generated MP3 sources x 5
     templates x 5 output formats): 400 byte-identical to the macOS default build,
     100 sample-rate refusals (16, 11.025, 8 and 24 kHz sources), none different.
   - Exact MP3 input, no setting, against the macOS default build: 315 pack
     members, 146 corpus, 172 prefix and 721 admission scenarios, all verified
     (1,167 + 904 + 967 + 515 identical items; the same typed refusals; sample-rate
     refusals; documented classes only: AAC, Ogg by content, unverified variants,
     FLAC by content, MP3 in WAVE; the 28 hits of the Apple-state class are the
     fuzz members above, each backed by repeated macOS runs).
   - Cover art and WAV tags: 207 scenarios and 8 ID3-limit cells (both sides of
     268,435,455). Error codes: 245 scenarios, 3 limit cases and 3 SIGINT cases; the
     39-code ledger is accounted for (33 observed through the CLI, 5 not reachable
     from it, 1 through SIGINT). Opus: 26 input/output scenarios and 7 valid-stream
     scenarios; the Opus output (791 identical items) equals macOS default and macOS
     portable.
4. *AAC decode read pin.* Versus the v0.2.0 binary, 43,380 engine results over 525
   generated AAC files (afconvert, the engine's own AAC output, ffmpeg streams with
   PNS, intensity and TNS, 120 held-out ffmpeg streams and 235 Apple-encoded
   streams) with environment padding of 0 to 400 KiB, `--jobs` 1 to 10, batches and
   long paths: 0 differences; a deliberately wrong pin (residue 16, the SME class)
   changed 455 of 474 class-sensitive `analyze` and 445 of 470 `process` results,
   so the corpus discriminates; a debugger breakpoint on `ExtAudioFileRead` saw
   `sp % 64 == 0` in all 336 sessions.
5. *Reviews.* A static security review, a code review and a hands-on security
   review (631,050 mutated MP3 streams and 657,996 structure-aware Ogg Opus files
   through the real libopus without a panic, hang or out-of-range read; the bridge
   with TagLib 1.11.1 under ASan and UBSan over about 75,000 calls with no report;
   all downloads hash-pinned) found no critical, high or medium issue. Fixed before
   the freeze: a missing `libraries.opus` path now reports `library_load_failed`;
   libopus is handed to readers only while `formats.opus` is on (a processed file's
   recorded original ending in `.opus` could otherwise be decoded with the format
   off); an Opus stream whose end-of-stream page ends in an unfinished packet is
   refused; a FIFO named as an input or recorded original no longer blocks the macOS
   `inspect` and `analyze`; the verification tools accept the new default `formats`
   members; a disallowed `f64::max` in an Opus test; stale comments and the
   release-tooling and notice defects the review found. The bounded limits that
   remain are listed in [LINUX.md](LINUX.md#hostile-input-limits). Open for the
   maintainer: the provenance of the MP3 reader (see `THIRD_PARTY_NOTICES`).


## v0.2.0 changes and evidence

This section and the next record what v0.2.0 shipped, as published. Where v0.3.0
supersedes a statement it says so; the superseded text is kept for the record.

v0.2.0 adds one explicit opt-in and one reporting contract and pins one native
call context; it changes no processing numerics.

- **Opt-in non-identical paths.** `parity.allow_non_identical` (default off)
  enables, on Linux and `portable` builds only, an open-source deterministic
  MP3 decoder, the byte-for-byte copy of embedded cover art and the keeping of
  the WAV tag chunks the reference marker writer drops (a source's `ID3 ` chunk
  and INFO `LIST` survive WAV output). These three paths
  are **not** claimed to match the macOS reference: the decoder equals
  AudioToolbox on 95.08 % of 46.1 million samples of a 389-file generated corpus
  with a 4-LSB maximum difference (identical on macOS arm64, Linux aarch64 and
  Linux x86_64), a copied picture is not what the macOS bridge writes, and the
  kept WAV chunks are chunks the native application and macOS drop. The WAV rule
  is Rust-only (`src/wav_preserve.rs`, no bridge change); the macOS default
  executable is byte-identical with and without it. Each
  affected item lists the path in `report.non_identical_paths`. With the setting
  off, Linux behavior is the v0.1.0 identical-or-refuse behavior; on a macOS
  default build it has no effect. **Superseded in v0.3.0:** the open-source MP3
  decoder and its `mp3_decode` member are gone, replaced by the exact reader
  that needs no setting; the setting now enables only the cover-art copy and the
  WAV tag chunks.
- **Error codes.** Every failed item, batch error and top-level error carries a
  stable `code`; the table is in the [CLI contract](../research/CLI_CONTRACT.md#error-codes).
  (37 codes in v0.2.0, 39 in v0.3.0.)
- **Unchanged.** Exit statuses, the command line, no-clobber and atomic
  publication, ALAC, and AAC refusal on Linux (now with a code).
- **macOS AAC call context.** AudioToolbox's AAC encoder runs its 512-point DFTs
  with one of two kernels (SME or NEON) that it selects from the stack-pointer
  residue (`sp` modulo 64) at the call: `ExtAudioFileWrite` takes the SME kernel
  when the residue is 48, and the final drain inside `ExtAudioFileDispose` when
  it is 32. An intermediate v0.2.0 build whose engine frames had merely changed
  size produced different final AAC packets from v0.1.0 for 6 of 13 synthetic
  inputs (a control build of the v0.1.0 source with an unrelated 48-byte local
  reproduced the same bytes). Those two calls now go through a small aarch64
  trampoline (`call_with_sp_residue`) that fixes the residue at 0, so both run
  the NEON kernel, the one used by the v0.1.0 executable and by the four native
  AAC-to-AAC references (the controlled-clock exports described under "AAC
  decoder stack-context regression" below). The interleaved read list is a
  separate, 64-byte-aligned wrapper (`#[repr(align(64))]`) and does not use the
  trampoline. In scratch builds the 13 AAC outputs and the AAC waveform stayed
  equal to v0.1.0's under six frame-size perturbations and eight
  process-environment sizes, and the four native references still match. This is
  an observation of the tested macOS arm64 AudioToolbox, not a cross-version
  guarantee; the evidence is in `local-artifacts/release-v0.2.0/aac-layout-control-01/`.
  **Superseded in v0.3.0:** both `ExtAudioFileRead` call sites, including the
  interleaved one, now use the trampoline too (see v0.3.0 above).

The evidence for this release is kept separate by claim, and ignored tests are
never counted as passes:

1. *Default-off equivalence on macOS.* A generated input set (16/24-bit WAV at
   44.1 and 48 kHz, mono, clipped, noisy and silent material, AIFF, and FLAC,
   ALAC, AAC and MP3 inputs) was processed with the v0.1.0 macOS executable and
   the v0.2.0 executable using the same libraries, every output format, the
   Official template and a fixed MP4 header second. Output files and the JSON
   outcomes were compared after removing only the members v0.2.0 adds.
2. *Default-off equivalence on Linux.* The same refusals, with the same messages
   and now with codes, for MP3 and AAC input, AAC output, unreproduced
   sample-rate conversion and artwork that needs Apple image transcoding.
   (MP3 input is no longer refused in v0.3.0.)
3. *Opt-in end to end on Linux.* Generated MP3 inputs and cover-art sources
   processed with the setting on; the reported `non_identical_paths`, the
   byte-for-byte picture comparison and the decoder's own tests. (The MP3 part
   exercised the open-source decoder that v0.3.0 replaced.)
4. *Error codes.* Unit tests for every typed mapping and stage label, CLI
   scenarios per reachable code, and a test that keeps the documented table,
   `ErrorCode::ALL` and the scenario ledger identical.
5. *Release builds and test counts* per platform, recorded at the source freeze
   and listed in the next section.

## v0.2.0 release evidence

Source freeze `freeze-03b` has source-manifest SHA-256
`18acb54e10561200d15b18865bc0131894e7fc354003b9f913bc38bfd9428b94`. Its
executables, all built from that source, are:

| Platform | Executable SHA-256 |
| --- | --- |
| macOS arm64 (privacy-remapped release build) | `75c2ac875f39bacac357544a704b821a489bc306935da85c0c5feff478d39260` |
| Linux aarch64 | `881ee9093922181040126f75f9bc8508decb633c6cc43121db32019d85800454` |
| Linux x86_64 | `25a409193ac0674438b3b6e719a3d14d79353a35794d2c5f1a4c0208f62f6033` |

The macOS executable is byte-identical to the one built from the earlier
freeze `freeze-02` (`f6ff4599…`): the WAV tag rule is cfg-gated to Linux and
`portable` builds and every edit to code compiled on macOS keeps its panic-site
line and column numbers, so the macOS battery recorded for `freeze-02` applies.
The macOS executable was rebuilt independently from the exported public tree
with the recipe in [BUILD.md](BUILD.md) and matched byte for byte; both Linux
executables were likewise rebuilt from the public export in the native-library
images and matched. These are the executables in the release archives.

**Source-freeze tests** (offline, locked, all targets; ignored tests are never
counted as passes). The frozen development tree contains two private,
fixture-dependent ignored diagnostics that the public export omits, so its macOS
ignored count is two higher than the export's:

- macOS default build: **516** passed, **0** failed, **190** ignored in the
  frozen tree (every `test result` line counted; the `freeze-02` battery receipt
  counts the same suites as **512** passed across 49 suites by its
  nested-summary rule) and **516** passed, **0** failed, **188** ignored when the
  public export itself is tested. The opt-in lanes passed 38/38
  public-dependency tests, 1/1 waveform native replay, 1/1 Xing unknown-byte
  oracle, 8/8 `error_codes` tests that need the macOS bridge, and the
  portable-mode WAV tag tests (3/3 bridge tests with the v0.1.0 macOS bridge, 8/8
  unit tests).
- Linux aarch64: **477** passed, **0** failed, **130** ignored, plus the
  library-gated lane (`error_codes`, `linux_opt_in`, `portable_art_copy` and the
  three WAV tag-chunk tests of `tags_reference`, with `--ignored`) **33** passed,
  0 failed (4 suites). The same counts were obtained building and testing the
  public export.
- Linux x86_64: **475** passed, **0** failed, **128** ignored, plus the
  library-gated lane **33** passed, 0 failed (4 suites); likewise reproduced on
  the public export.

**macOS battery for `75c2ac87…`** (receipt `release-final-75c2ac87.json`, overall
status bounded-complete-file-match-with-documented-context-limits, 0 failures):
71/71 retained replay cases; 8/8 dynamics and pitch cases; 10/10 MP4 exports
including the four native AAC-to-AAC references; the Xing E05 case; 5/5
boundary cases; 11/11 strict family cases (byte-identical in a
relocation-aware review: the native and candidate output folders differ and
only that setting is substituted); 6/6 coverage extensions; 5/5 misnamed-Ogg
cases (u02; the FLAC case is a documented native failure with no output on both
sides); 4/4 failure inputs (f01; two matching native failures with no output);
the W04 exact-path workflow, the CLI workflow smoke, gauge and waveform
checks. The 13 synthetic AAC outputs equal v0.1.0's, and 120/120 output
digests and 131/131 statuses are identical to v0.1.0. Default-off comparisons
against the v0.1.0 macOS executable (the 12 earlier inputs plus four generated
WAVs carrying `ID3 `, `id3 `, INFO and combined tag chunks): 276 records, 0
stable differences; with the setting on nothing differs and nothing is listed.

**Linux battery** (receipt `battery-linux/receipt-freeze-03b.json`, acceptance
pass with no unexplained or failed items; per architecture; native libraries
hashed before and after every run and matched by device and inode in
`/proc/<pid>/maps`: of 11,276 observed child processes, 8,992 mapped a native
library, every mapped file matched its pinned digest and none mapped an
unexpected one). With the setting **off**, byte-identical to the v0.1.0 receipts:

| Corpus | aarch64 | x86_64 |
| --- | --- | --- |
| Native references (132 cases) | 104 exact, 25 typed refusals, 3 matching native errors | 104 exact, 25 typed refusals, 3 matching native errors |
| Workflows | 16 matching | 16 matching |
| Differential stress (575 cases) | 535 exact, 40 sample-rate refusals | 535 exact, 40 sample-rate refusals |
| Pitch | 30/30 exact | 30/30 exact |
| Docker Desktop bind-mount subset (separate from the tmpfs main run) | 44 exact, 2 refusals, 1 native match, plus 16 workflows | not part of the x86_64 battery |

Every output hash is identical to the v0.1.0 receipts (0 differences in each
corpus). Default-off comparisons against the v0.1.0 Linux executables on the
same 16 inputs: 248 records on each architecture, 0 stable differences, tagged
WAVs included. With the setting **on**, aarch64 and x86_64 agree on every step:
MP3 175/175 scenarios (these exercised the open-source decoder that v0.3.0
replaced), cover art 184/184 scenarios (896/896 cells) and error
codes 174/174 scenarios covering the 37/37-code ledger (v0.2.0's; v0.3.0 has 39
codes). `wav_tag_preservation`
is declared on 53 items and never in a setting-off step; against v0.1.0 only the
cross-format WAV outputs (AIFF, ALAC and FLAC to WAV) differ and each lists it.

**Known differences and limits** (none changes a default-path result):

- A RIFF/WAVE file named `.mp3` is processed by content on Linux, with the
  setting on or off; the macOS default build refuses it with
  `invalid_input_file` because the Apple stack chooses by extension. This
  predates v0.2.0. **Superseded in v0.3.0:** Linux and `portable` refuse it as
  `invalid_input_file` too (a name ending `.mp1 .mp2 .mp3 .mpa .mpeg` commits to
  the MPEG reader).
- `inspect` on a symlink returns `invalid_input_path` as the contract says on
  Linux; the macOS builds return `io_error`. **Superseded in v0.3.0:** macOS
  reports `invalid_input_path` (stage `inspect source identity`) as well.
- W03 early cancellation depends on signal timing; when the first attempt does
  not land in the analysis stage it is reconciled with the separate one-signal
  probe, as in v0.1.0 (the original attempt is retained).
- With the setting off, an MP3 with junk bytes before its first frame and no
  ID3 tag is classified `unsupported_input_format` on Linux, while macOS decodes
  it. The open-source decoder claims only confirmed MP3 streams. **Superseded in
  v0.3.0:** the exact reader decodes it on Linux exactly as on macOS (it
  reproduces AudioFile's stream search), with the setting on or off; data that is
  not MPEG audio under an `.mp3` name is refused by the MPEG reader
  (`invalid_input_file`, `'dta?'`) as on macOS, and under other names it is
  identified by content (see [LINUX.md](LINUX.md#mp3-input)).
- The direct M4A-to-MP3 picture path is the reference path (not the opt-in
  copy); its ID3 picture has type 0, no description, and `image/png` for GIF,
  BMP and WebP bytes, as on macOS.
- AAC input and output stay refused on Linux. The x86_64 floating-point
  residual classes described in [LINUX.md](LINUX.md) are unchanged.
- By default (setting off) a WAV written from a WAV source drops the last-saved
  tag chunk, as the native application does; with the setting on the source's
  tag chunks are kept and the item lists `wav_tag_preservation`. A cross-format
  copy into a WAV always writes tempo and key through TagLib, so with the setting
  on even a source without tags gains an `ID3 ` chunk (`TBPM` "0", empty `TKEY`)
  and lists `wav_tag_preservation`; a v0.3.0 follow-up will consider keeping only
  chunks that carry source data. An MP3-to-WAV copy carries the text tags but
  never a picture. **Superseded in v0.3.0:** with the setting on a cross-format
  copy carries the picture into and out of a WAV; the tempo-and-key ID3 chunk
  policy is unchanged and open.
- Follow-ups for v0.3.0 include a stricter preflight for very large pictures
  converted to ID3 (about sixteen 16 MiB FLAC pictures can pass the current
  check and still exceed the ID3 size limit; the likely effect is a failed item,
  not a corrupt publication) and hardening flags for the bridge build.
  **Superseded in v0.3.0:** both are done, and the evidence disagrees with the
  expectation above: v0.2.0's bridge did let such tags through and TagLib wrote
  a wrapped size field (0 for exactly 2^28), so the effect was a corrupt tag, not
  a failed item, which the v09 preflight now refuses (see "Tag bridge v09:
  evidence").

All counts describe the recorded synthetic corpora and environments and are not
a universal parity proof.

## v0.1.0 source freeze

The regression-fixed source freeze has source-manifest SHA-256
`5b05c38952406b2e21e6c5e7397e29d3d9140d3d0963ad675569c5e7548f72d5`
and a private build receipt at
`portable-parity/full-build-v07-regression-fix-01/receipt.json`. In the v07
native-library images its offline locked all-target suite passed **400**, failed
**0**, ignored **97** on Linux aarch64 and passed **398**, failed **0**, ignored
**95** on Linux x86_64, each across 44 top-level suites. The respective Linux
executable SHA-256 values were
`6e19449bc9d492d27fa0068013b4e9d3aeaa7b4fe915a3abc6285ffbe00e289e`
and `1c34e59b586b0f6adfa394b858cbdd2d3abb3d675cd8711c3a6e443c7c7ddfff`.
The earlier source-path-remapped optimized macOS binary SHA-256 was
`82d8b48904f8d6128b7399330556ea13d130f01426d86041db1d974676d49b0d`.
These architecture-specific binary hashes and test results do not themselves
establish complete-file parity. The freeze's full-tree manifest records the
tree at that point, not later documentation or export-related edits.

### Shipped macOS identity-gated battery

The v0.1.0 macOS executable SHA-256 is
`198e0091a212e8af05e0934b1249fd698bfa2300f643e3b430d2b188d30913b8`
(LC_UUID `4D4554E4-7EE2-39F1-9681-CDF11CC1A5A2`). It was rebuilt from the
same compile-time sources with additional home, Cargo and Rustup path remaps;
the earlier `82d8b489…` build already remapped the source path. The binaries
are not byte-identical, so the new executable received its own battery rather
than inheriting a byte-equivalence claim. The build export differed from the
previously verified export only in release workflow and Linux documentation;
later CI and documentation changes are not represented by that build manifest.

The public-export default-mode all-target suite passed **440**, failed **0**,
and ignored **179** tests across 42 suites. A separate selected public-library
run passed 38 tests, and the waveform replay and Xing caller-byte oracle each
passed one. These runs overlap and ignored tests are not passes.

Without normalizing bytes, the new binary matched **71/71** selected stable
files, **8/8** dynamics/pitch cases, **10/10** MP4 exports at their recorded
header seconds, 15 boundary files, all six x01–x06 extension cases, the Xing
E05 case and W04 exact-path replacement/restore. Nine family cases matched
only in the documented **path-relocated context review**; the original strict
toolkit rejected the different output folders. The supplemental workflow,
gauge and waveform checks matched; timed W03 cancellation landed in healing
rather than the recorded native analysis stage, while an early cancellation
reached analysis. The native application was not launched or controlled.

The shipped bridge and LAME libraries use neutral install names and ad-hoc
signatures. Their SHA-256 values are
`14ca6f03b65a2cb0c4bc8bd06df540ae1707796f59f2d4f117a5d33447dfd79e`
and `2986f34cca6a4fbfb05934ac33b4bb919c8c47de1594e227b2f0fa4c930b377f`;
libFLAC remains
`d9b3a64df9eeab7556aad9877492ad07ba51e6459e3725f90b0a9203078ac6a5`.
The bridge and LAME section and non-metadata segment bytes were checked
against their prior libraries; changes are confined to install-name/load-command
layout and code-signature-related data. The CLI and these libraries were
checked for host-specific home-directory strings.

The private primary and supplement receipts are
`hash-parity/release-final-198e0091.json` and
`hash-parity/release-final-198e0091-supplement.json`, SHA-256
`566c93b7f848ede1f2846221218b1a587b2894b84206dc810f0a21e293f2c7a4`
and `35e1efaa9d7c3ab7911c6f07f61448bfa246625570468fbd4a27e327919029bb`.
These finite, overlapping synthetic cases do not prove universal parity.

### Earlier macOS identity-gated battery

The earlier `82d8b489…` binary (LC_UUID `5348303E-C012-3ECD-9023-D9A444C57F73`) passed
**440** top-level regular tests, with **0 failed** and **181 ignored**, in
44 suites. A separate selected public-dependency run passed 38 tests; the
native waveform replay and Xing caller-byte oracle each passed one. The
original Mac receipt aggregated four nested filtered subprocess summaries and
reported 444/48; the create-only count addendum corrects this to 440/44
without changing the original receipts, test exit or binary identity. Counts
overlap and ignored tests are not passes.

Without normalizing file bytes, the binary matched 71/71 selected stable cases,
8/8 dynamics/pitch cases, 10/10 MP4 exports using each recorded header second,
15 boundary files and all six x01–x06 extension cases. The retained Xing E05
case and W04 exact-backup-path replacement/restore comparison also matched.
Nine family cases matched in a **path-relocated context review**; both c04
ALAC exports matched separately at their recorded seconds. The unmodified
strict toolkit rejected the different output folders; the adapted review
validated both paths and substituted only that setting. It did not alter
native observations or output bytes. This is not the earlier `dd4b5fd0`
identical-path strict-context result. These sets overlap and cover only the
tested synthetic cases.

The supplemental workflow replay matched the recorded gauge/statistics and
waveform checks; timed W03 cancellation landed in healing rather than the
historical native analysis stage. The native application was not launched or
controlled during this battery. The primary, supplement and counts addendum
are retained under the private evidence root as
`hash-parity/release-final-82d8b489.json`,
`hash-parity/release-final-82d8b489-supplement.json`, and
`hash-parity/release-final-82d8b489-counts-addendum.json`, with SHA-256
`d3ea14d5064b02f9b390755f634f1dd0716706d9741ef2ff2938702939b93b1a`,
`33add854616ac356fc33759ce04fcc308540c92a83b4db3b8dde2eba22381588`, and
`7cb2eca91c0ffcdaacc58c7336d808c5d8f9619356429335180823641ab5a434`.

### Linux end-to-end battery

The regression-fixed source snapshot was tested on both Linux architectures.
Per architecture, 132 native-comparison cases comprised **104 exact complete
file matches**, **25 documented typed refusals**, and **3 matching native
error/no-output cases**. Each architecture matched all 16 recorded workflow
cases. Refusals and no-output outcomes are not counted as byte matches.

For a separate 575-case stress corpus, the macOS default build produced 575
outputs; macOS portable, Linux aarch64 and Linux x86_64 each had **535 exact
byte matches and 40 documented refusals**. All four platforms matched all
30 supplemental pitch cases. A separate Docker Desktop bind-mount subset
reported 44 byte matches, one native match, two documented refusals and 16
matching workflows; it does not replace the main Linux tmpfs battery.

There were no unexplained mismatches in these runs. Low-level and nonfinite
inputs were not instrumented, so successful files do not establish that the
x86 FTZ or NaN-sign boundary was exercised or repaired. The private index
`portable-parity/linux-battery-01/receipt-final-regression-01.json` has SHA-256
`c0d49ffb934e13074fc61c0d2eb0ad700973cb8821016a93ab7c4dd234dc498b` and
links the individual native, stress, pitch and bind receipts. These finite,
overlapping synthetic cases are not a universal parity proof.


## Historical v07-fixes source build

That pre-regression-fix freeze's original source-manifest SHA-256 is
`dc82b4a030c620c1d3b80a495078613c2736690dfbea251fbd8c5ca26fcceb55`.
In the v07 native-library images, its offline locked all-target suite passed
**398**, failed **0**, ignored **97** on Linux aarch64 and passed **396**,
failed **0**, ignored **95** on Linux x86_64; each had 44 top-level suites.
The Linux executable SHA-256 values were
`8954dd025e34395ceda24cabcfbbd3687677ab236199d6b96d097821e6775bf7`
and `e7b837ecb4ed4873ba23791a24d7786b3b45a09abeec75527bca2b364972a8dc`.
These architecture-specific binaries and passing tests are not themselves
complete-file end-to-end parity evidence.

The original build receipt counted three nested filtered test subprocess
summaries as additional x86 suites and passes (399/47); the separate
`top-level-test-counts.json` corrects that aggregation to 396/44 without
changing the recorded exit codes or executable hashes. The original receipt
is preserved. Both are under the private evidence root in
`portable-parity/full-build-v07-final-fixes-01/`. Its remapped macOS binary
SHA-256 is `3f2c19c4705ad892e6c03f202416ab25b714b4aaa1b0d6ae8765ed559431f8b6`.
Later documentation and export-related edits are not represented by the
original full-tree manifest; do not describe that manifest as the public
export's identity.


## Historical v07 frozen candidate

The pre-fix, pre-documentation candidate snapshot has source-manifest SHA-256
`855445719d235d4ca14cda8294bd1c8aef2283679e5ea0d967adbd70c8c9d186`.
The remapped optimized macOS executable SHA-256 was
`c9f19077c91fb975f9d375f4aae3c7024becce2416d3211e9ed971e7ad1830a7`
(LC_UUID `5AE44579-7B18-3355-83BD-AFBEAAEF5E9E`). Its frozen private
all-target suite passed **426**, failed **0**, and ignored **179** tests
across 44 suites. A separate selected public-dependency run passed 37 tests;
the opt-in native waveform replay and Xing caller-byte oracle each passed one.
These runs overlap and ignored tests are not counted as passes.

Without normalizing file bytes, this binary matched the retained references in
71/71 selected stable cases, 8/8 dynamics/pitch cases, 10/10 MP4 exports using
each recorded header second, and all six additional x01–x06 cases. It also
matched the retained Xing E05 case and the W04 exact-backup-path
replacement/restore comparison. Nine family cases matched in a **path-relocated
context review**; both c04 ALAC exports matched separately with their recorded
seconds. The unmodified strict toolkit rejected the relocated output folders:
the adapted review substitutes only that setting after validating both paths,
and does not normalize any audio bytes or change native observations. This is
not the earlier `dd4b5fd0` identical-path strict-context result. These case sets
overlap and establish only the tested synthetic cases.

The same source freeze built and ran the offline locked all-target suite on
Linux aarch64 (**386 passed, 0 failed, 96 ignored**) and x86_64 (**384 passed,
0 failed, 94 ignored**) in the v07 native-library images. Their executable
SHA-256 values were `6826ecbed9c463550c65d95c6179c397f7355e40fc3e7daa9487828be7c76005`
and `6ecd7c4e57b9acb1f8cda80da4d1c74447cd501e88c616574a1b6ef42c4094b4`
respectively. These are architecture-specific binaries and test results, not
cross-architecture binary identity or an end-to-end parity battery.

The macOS primary and supplement receipts are retained privately as
`local-artifacts/hash-parity/release-final-c9f19077.json` and
`local-artifacts/hash-parity/release-final-c9f19077-supplement.json`, SHA-256
`c98e64e8be4faeb9b954a198ce2e696506b59526e64f4085395daace21a8c3b4`
and `a175a21d0aaf6e9f96a94a579d7793c275e6b2b39c38bb9d082b3ab853609729`.
The Linux build receipt is `portable-parity/full-build-v07-registry-stage-01/receipt.json`
under the private evidence root. The frozen inventory predates the subsequent
documentation, notices, export list and package-notice wording changes; those
edits must not be represented as part of the original frozen manifest.


## Portable component evidence

These are component or codec comparisons from separately pinned snapshots, not
additional complete-application passes. Their sets overlap; do not add their
counts to the release battery. The Accelerate and math component reference
records Apple M4 with SME, macOS 27.2 build 26B123, round-to-nearest, the
specified FZ modes and the aligned-0 FFT profile.

| Component | Bounded observation |
| --- | --- |
| Math | 46 components × 131,072 inputs per FZ mode replayed on macOS, Linux aarch64 and Linux x86_64 without mismatches; Linux raw-bit sampling is not exhaustive. The math receipt also records separate exhaustive macOS binary32 sweeps. |
| ARM scalar helpers | The three-host oracle compared 100,000,000 random plus 32,768 special triples per precision/FZ with zero helper mismatches. A separate x86 vectorized-call-shape replay recorded 210 FMA NaN-payload mismatches, a documented residual rather than a pass. |
| vDSP | Eleven element routines, 686 lengths and both FZ modes were sampled without failures; a three-host non-FFT corpus matched 54,342,720 words per FZ mode. Profiles, lengths, strides and aliases are limited to those in the component receipt. |
| vForce | Mac `vvlogf`, `vvexpf` and `vvsincosf` sweeps covered all 2^32 binary32 inputs in specified body and tail classes, both FZ modes, without mismatches. Linux replay used finite general and boundary corpora, not exhaustive Linux sweeps. |
| FFT | The aligned-0 Mac corpus used 10,000 random and seven structured vectors per size, direction and FZ; each Linux architecture replayed 3,052 vectors/28,350,464 words with zero mismatches. SME complex transforms separately used 100,000 random plus 12 structured vectors per size/direction/FZ, with zero recorded word mismatches. |
| PCM and ALAC | Per Linux architecture, the audio snapshot recorded 9,540 reader cases/179,610,900 sample-bit comparisons, 2,424 PCM writer cases/90,494,042 file bytes, 864 ALAC reader cases and 1,589 ALAC writer cases without mismatches. An ALAC one-sample mono/16 sweep matched file lengths and hashes for all 73,951 integer rates 22,050–96,000; 2,240 fractional-rate cases comprised 2,095 matches and 145 expected native-loss refusals. Sample contents and fractional rates were not exhaustive. |
| GSM | The three-host optimized snapshot matched 117 packet cases, 42,258 blocks and 13,522,560 samples; its 12,288-mask reader corpus had zero mismatches and 179 reserved-mask cases were typed refusals. Later audio source replay coverage differs by platform as recorded in its inventory; the accepted mask subset is not universal. |
| FLAC/LAME | Per Linux architecture, codec-03 matched 540 FLAC and 320 MP3 retained cases, 7,488 FLAC and 2,880 MP3 expanded cases, plus a 2,988,408-record one-frame FLAC rate/channel/depth sweep. Codec-04 additionally matched 5,244 FLAC stress/recovered-PCM cases per architecture. These are codec-only results, not all possible streams or full pipeline outputs. |
| Bridge v07 | Each Linux architecture matched 604 transfers, 86 inspections and 232 source-status cases; 37 fault runs and nine WAV permission/umask cases had no reported failures. The macOS native-versus-portable matrix covered 690 cases, including 112 expected artwork refusals. Status/refusal evidence is not the same as a file-byte match for an accepted output. |
| MP3 postflight | The final-source gate matched ten stream-description/length fields for 17,120 unique files, including selected final-battery outputs, with zero mismatches. For v0.2.0 the `Info`-tag estimate was corrected for variable-bitrate output (`src/mp3.rs` SHA-256 `7eda250126953070837eca28fabb8fd1659078e4c5947b1f8cdd997b3fc05a7b`): the 17,120 prior rows were reconciled by SHA (17,080 files run) and 1,166 new Info-VBR, Xing and erased-identifier engine outputs were compared, all with zero mismatches (506 of those 1,166 mismatched before the fix; `local-artifacts/release-v0.2.0/mp3-info-vbr-01/REPORT.json`). This tests postflight fields, not complete-file encoding equivalence. |

The component reports retain their source pins, commands, historical failures
and exact scope in the private evidence inventory. The published source includes
only the reviewed compile-time coefficient table and two compact FFT test
fixtures, not the private corpus. A different Apple Silicon generation, OS,
Accelerate dispatch, allocator alignment or AudioToolbox stack context may
change the reference. On x86, ordinary FTZ tininess, default-NaN sign and NaN
selection including vectorized FMA order remain known residual classes; no
claim is made that the runtime detects every affected input. Linux aarch64 is
the stricter build. See [LINUX.md](LINUX.md) for refusals and loader limits.

Review rounds proceeded through ReviewMacDefault, ReviewPortable and
ReviewSecurity, fixes, ReReview, FinalReview and LastRegressionReview.
FinalReviewEngine's verdict for its reviewed snapshot was
`PASS_WITH_QUALIFICATIONS`, not an overall release or security sign-off. Its
conditional MPEG/MP4 classification finding was addressed by validating
atom sizes and structure; six decoder-admitted synthetic collision cases
matched the review baseline. LastRegressionReview's ACL and restrictive-umask
findings were addressed in FixWorkflow-regression-04 and the subsequent
frozen build. These observations remain bounded by their individual receipts.

FinalReviewEngine also recorded accepted limitations for intentionally
conflicting marker representations. The bridge selects the first relevant
GEOB or MP4 freeform item and prefers a FLAC Xiph marker over ID3; a later
processed marker may therefore be concealed. Sync-like junk or nonadjacent
ID3 tags may also hide a marker from prefix discovery. These are source-derived
scenarios, not executed proof that the exact crafted inputs are decodable or
that native behavior is identical. The CLI contract documents their scope.

The v07 review changed bridge handling so read/seek errors do not become a
source-container-unavailable status or cause a destination callback, and
verified WAV rewrite modes under nine mode/umask combinations per platform.
Other review rounds addressed transaction/overwrite and restore safety,
container sniffing, duplicate ID3 markers, descriptor pinning and postflight
validation. Their bounded checks and the earlier four-review history do not
imply that adversarial concurrent modifications are impossible: a library file
writable by another process is not made immutable by hashing or path pinning.
Native library initializers may run before a post-load hash detects a change.

Earlier unpatched Linux libFLAC builds differed on 24 retained cases; codec-03
matched those cases after the FP-order/math corrections. Codec-04 traced 12
later reported ARM battery differences to an obsolete pre-patch library and
replayed those complete files successfully with codec-03. This is a
build-provenance explanation for those cases, not proof of arbitrary FLAC
identity. ALAC's later hostile sample-table review also rejected eight compact
malformed fixtures on all three hosts before attempting an impossible index
allocation. The bridge v07 directory-as-MP3 source case now fails instead of
preserving the older bridge's accidental success; the CLI accepts regular
source files only.

An earlier `v0.0.0-dryrun` distribution exercise built packages for macOS
arm64 and both Linux architectures, verified the extracted package checksums,
and rebuilt both Linux native-library sets from the bundled sources with
matching library hashes. One 8,192-frame silent WAV produced the same bytes on
all three platforms. This was a pre-integration packaging experiment, not the
v0.1.0 release package, universal parity evidence or legal clearance; no
release or GHCR image was published by that exercise.

### Correction to f01–f03 process time

The original f01/f02/f03 observation files for native PID 99320 contain the
incorrect parenthetical UTC launch time `(2026-10-27T21:36:13Z)`. The correct
launch time is **2026-10-28T12:27:07Z**, corresponding to `lstart Wed Oct 28
07:27:07 2026 CDT`. This note corrects the annotation only; the original
observations are left unchanged and no missing context is inferred from it.

No new native GUI drag-and-drop admission was tested in this release cycle
without desktop-control approval. Picker and explicit open-route observations
were made. Earlier bounded Ogg/OGA drag observations are retained below; they
do not establish drag admission generally or imply a new test in this cycle.

## First pinned match

The recorded reference was Platinum Notes 10.5.0.1256 on macOS arm64. Its two same-session exports, A and B, matched each other, and the corrected Rust debug and release outputs matched them as complete files for this case:

| Setting | Recorded value |
| --- | --- |
| Input | Generated WAV; 16 seconds, 768,000 frames, stereo, 48 kHz, signed 16-bit PCM |
| Input SHA-256 | `2de421ff4f080693c78a24947ad856d9db8f86c5909f42252187307aa820f884` |
| Template and output | The Official Platinum Notes Template; WAV; specific output folder; no replacement |
| Output length | 3,076,786 bytes |
| Complete-file SHA-256 | `06dbf2b016bf3389aa997ce7dd9816a663d226bde5d38e2b4cb62c3abb0d0c1b` |

No tags, headers or audio were normalized for that comparison. This demonstrates a match for **one input and one configuration**. It does not establish arbitrary-input, all-template, all-format, cross-process or cross-OS parity. In particular, original A and B were same-session repeats; they are not evidence of repeatability across every fresh application process.

### Historical correction

The first Rust output for this fixture did **not** match the original complete file. The captured PCM payload matched, but the input and output pitch values in the processing marker differed: the original reported `440.63037109375` Hz and Rust reported `440.6307373046875` Hz. The original file was 3,076,786 bytes; the earlier Rust file was 3,076,790 bytes. Investigation identified an off-by-one at the right edge of pitch-analysis whitening: the original replicates the mean/deviation from bin 247 where the earlier model used bin 246. Correcting that general boundary, without editing metadata or relaxing the comparison, produced the complete-file match above. The historical failure is not a pass.

The regular synthetic whitening regression checks all 256 output words plus the mean/deviation boundary, using measured native values. A controlled copy of the source with the old `N - 10` boundary restored failed at mean bin 247; the corrected source passed. This regression does not require the reference application or private fixture files.

A user-reported Big Boost export found in the earlier reference output directory matched a Rust Big Boost output for the previously generated input: 3,076,782 bytes, SHA-256 `cbf49914aa7000a5535aff29228d5097627b99946d0050f55e85c96b9e39a09d`. The input selection was not independently observed, no second original export was captured, and the marker says pitch correction was inactive. Treat this as supporting evidence, not proof of pitch-active or general template parity.

## Integrated crossover correction

The three controlled expansion-only, compression-only and combined-dynamics
cases originally differed by 8, 3 and 99 one-unit PCM24 samples. An owned native
component investigation established a crossover-control ramp over each host
event segment. The corrected graph now schedules normalized crossover controls
and advances them before each internal chunk. A regular bit-exact regression
checks all eight recorded ID-0 frequencies and first-low-section states for one
512-frame host call versus eight separate 64-frame calls.

The earlier integrated debug checkpoint, executable SHA-256
`dcebd44e72c8a2413f66db2aff2fc7ee9c141b60b123d64dd24041b35422b00b`,
matched both retained original files for those three cases and five controls:
gentle warmth, Big Boost, active pitch, Official baseline and custom pitch.
Its before/after hash receipts and outputs survive, but its executable bytes
were not separately archived before that target path was rebuilt.

That later archived source snapshot, including statistics and waveform changes, was
built and its executables archived before comparison. Debug SHA-256
`9575c5184067d0d55a367581a57bc43e3a1849877ed146f340e349f142830db9`
and optimized release SHA-256
`1ac812ef87257c5737105339eac19ce384c05504701debb47800430c021cf429`
each matched both retained native references in all eight cases. Every byte
and length counts, without normalization. Original locale, hidden settings,
timing and actual concurrency gaps remain: these are bounded file matches,
not strict contextual or universal parity. Further source changes require
new final-build verification.

The earlier `dcebd44…` checkpoint's explicit ambiguous-WAV-marker override matched both
native references for the tested empty and incomplete-JSON marker inputs.
Default processing still refused them, and a valid processed marker was
refused with or without the override. This is a user-selected safe-default
policy, not default-admission equivalence.

The archived optimized release additionally matched both preserved original
files in 71 selected format, metadata, DSP and boundary cases. The two explicit
ambiguous-marker cases are included. Execution was serial, with one input per
process and a configured jobs limit of 10; this is not evidence of ten active
workers. The reference sets overlap with the eight-case replay, so their counts
must not be added as unique coverage. Variable original outputs, timestamp
cases, path-sensitive workflows and safety refusals were retained as exclusions,
not counted as passes. Historical strict native-context gaps remain.

## JSON report precision correction

Calibration for new pitch-threshold cases exposed a reporting defect: JSON
inspection reparsed Foundation's marker numbers with default `serde_json`
float parsing, which could differ from the stored value by one binary64 unit.
Written files were unaffected; process reports and the encoded markers were
already exact. The dependency now enables exact float round-tripping, with a
regression that failed before and passes after the change. The corrected
optimized release, SHA-256
`8b8cfa128147ded271e6f1f6837f2d082ecd0ad004e6d1956a60a2505c1ee18e`,
again matched both native references in all eight dynamics/pitch cases and all
71 selected stable cases.

## Historical release build `dd4b5fd0`

The then-final source added `process --overwrite` and consumed the backup after a
verified restore; see the [CLI contract](../research/CLI_CONTRACT.md). Its
optimized release, SHA-256
`dd4b5fd05ac23bbf967e6688b71dee142ecdc1c3b399e42fe9f906ee9c7b9b98`
(LC_UUID `F3D8E57F-6B10-3CC7-A466-6B86E39E9197`), passed 317 regular tests with
92 ignored, the 20 public-library integration tests, the native
waveform-reducer replay and the Xing caller-byte oracle. Without normalization
it matched both native references in:

| Set | Matched |
|---|---|
| Strict full-context families c01–c03, c05–c07, f01–f03 | 9 of 9, no flags |
| c04 ALAC exports, given each recorded MP4 second | 2 of 2 |
| Retained dynamics/pitch cases | 8 of 8 |
| Selected format, metadata, DSP and boundary cases | 71 of 71 |
| MP4 exports, given each recorded second | 10 of 10 |
| E05 Xing without a recognized LAME tag | 1 of 1 |
| Nonzero-width dynamics gates and rounded pitch gates (D22–D24) | 15 of 15 |

The sets overlap. The strict reviewer is a copy of the frozen family toolkit
whose only change accepts the new `effective_config.overwrite` field and
requires it, `details.overwrite` and `replaced_existing_output` to be false.
The 71-case and eight-case harnesses likewise add `overwrite: false` to
historical profiles that predate the option. A CLI smoke on generated files
confirmed collision refusal by default, atomic `--overwrite` replacement with
the input untouched, source replacement with a verified backup, rejection of an
interdependent restore without changes, and backup consumption after restore.
Receipt: `local-artifacts/hash-parity/release-final-dd4b5fd0.json`.

The `dd4b5fd0` binary also repeated the recorded candidate workflows. The W01 mixed
folder exited 1 with a partial failure for the Ogg file, and its three WAV
outputs matched native. The W02 same-folder and mirror-tree outputs matched
native; the mirror root matched native B, since native A has the known marker
variant. An existing destination was refused by default and replaced
identically with `--overwrite`. W03 output-permission failure published
nothing. SIGINT exited 130 without publication when sent during analysis and
during healing; with this faster binary, the original 5-second signal timing
lands in healing rather than analysis. The gauge fixture output matched the
native export, totals reached one song, eight dB and zero clips, and reset
behaved as before. CLI waveform bins equaled the composed native decoder and
reducer words for all four generated WAV fixtures. Receipt:
`local-artifacts/hash-parity/release-final-dd4b5fd0-supplement.json`.

## AAC decoder stack-context regression

Adding a source-tag status field to the process report changed the caller's
stack layout. On the tested macOS arm64 runtime, the same synthetic AAC input
then decoded to different float32 samples inside `ExtAudioFileRead`, before
any healing DSP. In a controlled debugger intervention, shifting only the stack
pointer by 48 bytes at that call changed 83 of the first 512 decoded samples;
the input, output buffers and FPCR were unchanged. Which AudioToolbox internals
cause this is unverified.

The planar reader now keeps its live `AudioBufferList` 64-byte aligned, which
fixes the stack alignment at the read call regardless of the callers, without
changing the list ABI or the sample buffers. The E04 and E10 AAC-to-WAV cases,
tagged AAC-to-MP3 and four controlled-clock AAC-to-AAC exports then matched
their native references as complete files, and previously matching format and
DSP controls were replayed unchanged. These are bounded observations, not a
guarantee for other compiler or macOS versions. Investigation receipts:
`local-artifacts/hash-parity/aac-regression-01/`.

Since v0.3.0 the alignment wrapper is no longer what fixes the decoder's class.
AudioToolbox's AAC decoder selects its 512-point inverse-transform kernel per
`ExtAudioFileRead` call from the stack-pointer residue (`sp % 64 == 16` runs the
SME kernel, class S; every other residue the NEON kernel, class G), and the
planar and interleaved reads entered at a residue that frame layout alone
selected (0 in the shipped v0.2.0 binary). Both call sites now go through the same
`call_with_sp_residue` trampoline as the encoder's write and dispose calls with
`READ_SP_RESIDUE` 0, class G by construction; the `#[repr(align(64))]` list
wrappers remain. The pin is meant to leave every output unchanged, and its
verification is part of the v0.3.0 release evidence. It is an observation of
the tested macOS arm64 AudioToolbox, not a cross-version guarantee.

An opt-in regression test checks exact decoded sample words for the pinned
generated `aac-repeat-01.m4a` fixture:

```sh
PN_AAC_REGRESSION_SOURCE="$PWD/local-artifacts/apple-codecs/aac-repeat-01.m4a" \
  cargo test --offline --locked --release --test apple_codec_reference \
  pinned_aac_planar_decoder_samples_match_release_context -- --ignored --exact
```

## Additional native boundary cases

Two generated nonzero-width dynamics gates were imported natively. Native
GUI readback and the persisted native template IDs matched the prepared
templates. For three inputs measuring just below, at and just above
`-3.1` LUFS, the lower-inclusive `[-3.1, -1]` gate and upper-inclusive
`[-8, -3.1]` gate produced expected stage flags F/T/T and T/T/F. All six native A,
native B and Rust complete files were identical.

Nine generated pitch cases cover rounded corrections of 9, 10 and 11 cents for
negative detuning with preserved tempo and with turntable correction, plus
positive turntable correction. Native A, native B and Rust complete files were
identical in all nine. The threshold is a rounded measured correction, not an
exact physical cent boundary. Original locale, hidden settings and scheduling
remain unobserved.

## Strict full-context family captures

Earlier native captures lacked recorded locale, hidden settings, worker
concurrency or template state, so they were bounded matches rather than strict
passes. By user decision, one generated case per format family was recaptured
with full context and stands for its family. Each capture records:

- the native process instance (PID and start time before and after each render) and its arm64 architecture;
- the on-disk bundle version, executable SHA-256 and code signature;
- effective locale and timezone, and a privacy-redacted preferences snapshot;
- the effective template, derived from the native template store and checked against GUI readback;
- the exact admitted input and a polled single-item queue log;
- a candidate job trace recording each file job and every loaded library with its in-memory Mach-O UUID.

With a processor limit of 10, `parity-matrix compare` reported
`reviewed-complete-file-match-for-this-case` with no flags for nine families.
In each, two native exports from one process and the release candidate were
identical:

| Case | Template | Family |
|---|---|---|
| c01 | Official | 48 kHz stereo16 WAV → WAV |
| c02 | Official | 44.1 kHz stereo24 AIFF → match input |
| c03 | Official | 96 kHz stereo24 FLAC → MP3 CBR 320 (sample-rate fallback) |
| c05 | Official | 44.1 kHz AAC → MP3 VBR 192 |
| c06 | Official | MP3 CBR → match input |
| c07 | Official | 48 kHz mono24 WAV → FLAC |
| f01 | The Big Boost | Preserve-tempo pitch correction (+31 cents) with expansion, warmth and clip repair |
| f02 | Festival | 44.1 kHz stereo24 WAV → AIFF |
| f03 | Custom, imported natively | Turntable pitch (−34 cents), expansion and compression both active, hot warmth, clip repair |

For c04, 48 kHz stereo24 ALAC → match input, the two native exports differ only
in their MP4 header second. Given each export's recorded second, the candidate
reproduced both complete files. Evidence, abandoned attempts and attestation
limits are in `local-artifacts/hash-parity/family-fullctx-01/summary-v2.json`.
GUI polls are sampled, so the single-item queue and process continuity are
operator attestations. Loaded native images are identified from on-disk hashes
and signatures, not read from process memory.

## Batch pitch-marker variation

When several files are analyzed at once, native repeats can record slightly
different measured input pitch while the audio stays identical with correction
off. Constructing a tuning detector resets a global DSP dispatch table without
synchronization; the magnitude slot is at `0x1000ea720`. A concurrently analyzed
file can then compute one spectral window with the scalar or intermediate
magnitude routine. That routine reads the vDSP FFT's split buffer as interleaved
pairs.

Modeling one such window reproduced all five recorded native outlier words
exactly, and 80 end-to-end diagnostic replays matched. Every single-file and
one-worker native export matches the race-free value. By user decision, the
parity target is that race-free native result, which the CLI produces
deterministically; the batch outliers are documented native nondeterminism.
The historical dispatch state was not captured, and several windows can produce
each outlier. Receipts are listed under `batch-pitch-marker-race` in the private `local-artifacts/hash-parity/external-evidence-index.json`.


## Reproducing the candidate side

Follow [BUILD.md](BUILD.md), including the generated-fixture section. It pins the source archive checksums, Rust toolchain, native dependency versions, input digest, output configuration and expected output digest. The generator uses a new output path and generated audio, not recordings from a music library. Floating-point math runtimes can affect regenerated bytes: if the input digest does not match, it is not the pinned input. If the output digest differs, retain and investigate it rather than changing the comparison criterion.

The recorded native setup included a public TagLib 1.11.1 ABI-5 bridge, public FLAC 1.4.2 without Ogg, and public LAME 3.100. FLAC and LAME are not needed for the WAV fixture, but they were used in other bounded checks. Other host diagnostics recorded macOS 27.2 build 26B118 on arm64 and the native TagLib build used the macOS 27.0 SDK and CMake 3.31.6. Different toolchains, system frameworks, library builds, paths and metadata-relevant state may affect results; following the recipe is not a guarantee of identical output on another system.

To compare against your own locally obtained reference exports, preserve both unmodified files from the same input and effective settings, using the same reference version and output path. Preserve the first export before running the second. For example, after setting `A` and `B` to those two preserved files and following the build guide in the same shell:

```sh
C="$DEPS/fixture/output/reference_PN.wav"
wc -c "$A" "$B" "$C"
shasum -a 256 "$A" "$B" "$C"
cmp -s "$A" "$B" && cmp -s "$A" "$C" && printf 'Complete files match\n'
```

This optional reference exercise requires a separately obtained local reference application; the ordinary build and candidate run do not. Use only generated audio, do not alter the application or its signing, and preserve mismatches and context. A successful comparison on another configuration supports only that configuration.

For additional generated inputs, `node tools/parity-matrix.mjs --help` describes a case workflow that preserves original and candidate files and compares complete bytes. It records operator attestations and does not automate or authenticate the reference GUI. Strict reports now require concrete effective settings, process/render identity, independently evidenced stage states, actual concurrency evidence and ordered timing. Legacy receipts remain readable but incomplete where these facts were not captured; no historical unknown is retroactively filled. A candidate-only repeat is not an original comparison.

## Tests

From the repository root, the regular suite can be run without opting into reference-dependent tests:

```sh
cargo test --locked --all-targets
```

Tests marked `#[ignore]` are excluded from that command. Some need separately
built public libraries, additional public utilities, native captures or
separate oracle programs. Do not infer that ignored cases passed. Read each
test's prerequisites before opting in; running every ignored test is not a
self-contained public test suite. For example, after building a trusted macOS
v07 bridge as in [BUILD.md](BUILD.md), one generated-input bridge test can be
selected explicitly:

```sh
PN_TAG_BRIDGE="$DEPS/libpn-tag-bridge.dylib" \
cargo test --locked --test engine_reference \
  inspection_and_analysis_use_real_source_and_distinguish_malformed_markers \
  -- --ignored --exact
```

These integration tests use generated files and exercise their individual conditions; they do not establish complete original-app parity. Some regular tests also overlap with other reported runs, so do not add test counts as if all executions were distinct.

The earlier `dd4b5fd0` private development snapshot passed 317 regular tests and left 92 opt-in tests ignored. Its curated public snapshot passed 317 regular tests with 90 ignored; it excluded the two private diagnostic targets `aac_32k_fail_closed.rs` and `aac_writer_warning_probe.rs`, each holding one ignored test. Its public-dependency engine/codec/CLI/statistics run passed 20 selected tests, including marker admission, no-clobber, replacement/restore and post-publication accounting cancellation paths. These are historical counts, not results for a later source snapshot.

A separate opt-in replay consumed three captured native waveform-component input/output sequences and matched every float word. Four generated PCM16 WAV decoder cases (mono/stereo at 44.1/48 kHz, requesting 44.1 kHz) also matched every returned float byte and read boundary. The actual release CLI's waveform bins match the composition of those captured decoder words and the independently tested reducer. This is not a direct full-file GUI waveform capture, and other formats remain unverified on that path. Four historical pitch checkpoint comparisons and 96 generated Ogg decoder comparisons passed against captured or signed original components; none establishes full application parity.

## Public source boundary

The public repository starts with independent history. It includes Rust and portable sources, public TagLib bridge sources, synthetic tests, the reviewed FFT coefficient table and compact test fixtures, Linux build recipes, build instructions, typed preset parameters and the transformed human-readable template configuration required by the CLI. It excludes original serialized preset blobs, extracted compiled resources, screenshots, decompiler/disassembly exports, native libraries, audio captures and private research history. Optional reference tests may contain authored probe code or expect separately supplied captures; their presence does not mean those captures or proprietary components are distributed or needed by the production CLI. Static reconstruction and measured values are reference-derived, not a clean-room or licensing-clearance claim.

The exact MP3 reader's checked-in fixtures (`tests/fixtures/mp3-exact/`) are
generated audio only, recorded against AudioToolbox on the reference machine.
The full corpora (about 0.9 GB of deterministic regenerations), the Apple
reference dumps, the debugger sessions and the generator and verifier scripts of
the decoder's numeric tables stay in the private evidence; the table files carry
provenance comments naming the Apple binary and addresses they were read from.

Some source comments and explicitly ignored diagnostics intentionally name
`local-artifacts` paths for private evidence. The ignore file and developer
dual-host harness also name that directory to exclude generated data and
constrain scratch outputs. These are historical locators or safety rules, not
distributed fixtures or prerequisites for the regular public suite. The
public files do not contain the private artifact directory.

## Remaining limits

- The original pitch-correction splitter's output depends on its FFT scratch alignment. Real/imaginary scratch at 64-byte offset 0 gives one output profile; offsets 16, 32 and 48 give another, identical to each other. Every retained native pitch-active export in the five alignment-sensitive cases (D05, Big Boost, D04 and preserve-negative r10/r11; ten GUI exports) matches only the aligned-0 profile that Rust pins. Turntable cases do not distinguish the profiles. The splitter allocates through a 16-byte-aligned `malloc` callback, and 240 isolated processes repeating its allocation sequence never produced offset 0 for real/imaginary. The GUI therefore did not show plain-malloc behavior in any observed run, but its literal pointers and any callback override were not observed (`splitter-fft-alignment` in the private evidence index above).
- A Xing MP3 path without a recognized LAME tag makes the native updater read a reused caller byte. Static analysis shows that byte is part of the limiter AudioUnit handle and is nonzero on the in-process AudioToolbox creation path, so the CLI applies the nonzero branch. With that change the release CLI reproduced the retained original A/B/C export for the generated E05 case (644,959 bytes, SHA-256 `12b12b9ddff607fc9f82e38a25dd212518a6cbd462dc6aa3f44f79c7cf16971d`). The byte was not observed in the running app; see `local-artifacts/hash-parity/xing-nonzero-cutover-omp-01.json`.
- In one interleaved AAC fixture, original MIKAudio and direct AudioToolbox reads matched within each process but produced different bits across fresh processes with the same recorded conditions. Rust and direct reads also matched in a same-process diagnostic. By user decision, the decoding target is the native same-process result; the cross-process cause remains unknown.
- A separate direct-system AAC experiment compared untouched FPSR against clearing it before each read: 48 readers across 12 fresh processes all produced identical float streams, despite nonzero untouched FPSR values. It did not reproduce or explain the historical variability; the original app and MIKAudio wrapper were not used.
- AAC and ALAC outputs store one wall-clock second in all six `mvhd`/`tkhd`/`mdhd` creation and modification fields, so native repeats differ from each other. By user decision, an MP4 case passes when the CLI, given a native export's recorded second through the hidden `--controlled-mp4-time-1904` diagnostic option, reproduces that export's complete bytes. Historical release `1bd3b8…` did so for all ten retained ALAC, AAC and ALAC-match-input exports (`local-artifacts/hash-parity/mp4-controlled-clock-omp-01.json`), and `dd4b5fd0…` repeated it. Normal processing keeps the AudioToolbox clock; the native clock-sampling phase is not observed.
- A generated 32 kHz AAC match-input case made the original publish an invalid 28-byte M4A after a warning. The CLI refuses to publish that output by design; the user selected this safety behavior rather than reproducing the invalid file.
- A replaced file's marker stores the absolute backup path, so replacement output depends on that path. The single native W04 replacement and an earlier candidate differed only in that string; every other decoded marker field and every non-marker chunk matched. The `dd4b5fd0` build did not compare replacement at the native backup path.
- The original's normal picker disabled generated `.ogg` and `.oga`, Add Folders omitted them, and later selected drags and explicit-app file opens admitted neither. A supported WAV control was admitted by drag and processed through explicit-app handoff. These are bounded application-admission observations, not proof of decoder invocation. Lower-level original/Rust Ogg decoders matched 96 generated cases; that does not establish whole-application Ogg processing.
- Additional inputs, templates, correction/compression modes, rates, output formats and process contexts require their own repeatable, complete-file comparisons.

These limits also apply if individual component comparisons or tests pass. Keep backups and use separate output directories when processing files; see the [CLI contract](../research/CLI_CONTRACT.md) for replacement, restore and nil-marker behavior.
