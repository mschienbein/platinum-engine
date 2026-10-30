# Platinum Engine

A Rust command-line audio processor that reproduces Platinum Notes 10.5 processing and file workflows, writing byte-identical output files in every verified reference case. It is a headless engine, not a graphical clone or a generic loudness normalizer. Platinum Engine is not affiliated with the vendor.

## What changed in v0.4.0

- **Exact AAC output on Linux and `portable` builds (replaces the refusal).** `--format aac`, and `match-input` of an AAC source, write the `.m4a` that AudioToolbox's `ExtAudioFile` writes on the macOS reference (macOS 27.2 build 26B123) for the engine's call sequence (`m4af` file, `'aac '`, float32 client format, 256 kbit/s ABR, the v0.2.0 call-stack pin), using a pure-Rust port of its AAC-LC encoder, at 44.1 and 48 kHz with 1–8 channels, 32 kHz with 3–8, 24 and 22.05 kHz with 4–8 and 16 kHz with 7–8. On the generated corpora it was verified against, the file equals the macOS default build's byte for byte when the MP4 header times are pinned (the hidden `--controlled-mp4-time-1904` diagnostic option; otherwise the close-time second in `mvhd`, `tkhd` and `mdhd` differs between two runs, as it does on macOS). Every other rate and channel count is answered as the macOS writer answers it (a format AudioToolbox takes no 256 kbit/s for fails the item at stage `configure Apple encoder` with `encode_failed`; a format with no AAC converter fails with AudioToolbox's native status, also `encode_failed`), except the few that AudioToolbox serves but this build cannot reproduce (fractional rates and rates only within ±10 Hz of an AAC rate), which are refused with `unsupported_output_format`. It needs no setting; see [docs/LINUX.md](docs/LINUX.md#aac-input-and-output).
- **Exact AAC input on Linux and `portable` builds (replaces the refusal).** AAC-LC in MP4/M4A, ADTS, LOAS/LATM and CAF is decoded with a pure-Rust port of the AudioToolbox read path: the same float32 samples, `file_length_frames`, `bitrate`, `source_format`, the padding behind short reads and the `OSStatus` values of failing opens and reads as the macOS reference (the generic-DFT decoder class the engine's read calls reach). The behaviour on damaged and unusual files (atom and size fields, sample tables and sample groups, movie fragments, LOAS frames, `ftyp` brands, `iTunSMPB` and `udta`, CAF layered descriptions, the matrix mix-down of an in-band program config element in 5.1 streams) follows recordings of AudioToolbox for the shapes the fixture pack records; [docs/AAC_INPUT.md](docs/AAC_INPUT.md) lists the rules and, under "Not reproduced", the damaged shapes that are not reproduced and may give a different result or status. HE-AAC whose SBR or PS is signalled, AAC-LD/ELD, USAC, MPEG-1 audio in MP4, sample-rate conversion (a client rate other than the file's), a CAF whose rate or channel count differs from its cookie and a stereo read of a damaged 5.1 stream whose in-band program config element with a matrix mix-down index is not the stream's own 5.1 layout are typed refusals (`unsupported_input_codec` or `unsupported_sample_rate_conversion`). AAC inside a WAVE container, which AudioToolbox does not open, is refused with `unsupported_input_codec` rather than with its status.
- **MP3 provenance trim.** The two test-only files that modelled Apple's polyphase synthesis routine register by register (`synth_machine.rs`, `synth_ref.rs`) and the unit test that compared the reader's synthesis with them are no longer part of the source or the public export; the exact-match tests against recorded AudioToolbox results are unchanged and the MP3 reader's output is unchanged.
- **WAV tag chunks under the opt-in (Linux and `portable`).** With `parity.allow_non_identical` on, only WAV tag chunks that carry source data are kept. TagLib writes tempo and key into every cross-format copy into a WAV; v0.3.0 kept the resulting `ID3 ` chunk (`TBPM` "0", empty `TKEY`) and listed `wav_tag_preservation` even for a source without tempo, key or tags. Now an `ID3 ` chunk that holds nothing but empty `TBPM` and `TKEY` frames, and an INFO `LIST` without a value, are dropped: the output is then the default policy's byte for byte and `non_identical_paths` lists nothing; a source with a tempo, a key or tags keeps its chunk and lists the path as before. With the setting off nothing changes (see [Upgrading from v0.3.0](#upgrading-from-v030)).
- **Portable ALAC output: two differences from AudioToolbox fixed (Linux and `portable`).** Comparison runs for the AAC work found two differences between portable and macOS ALAC files that v0.3.0 shipped; the macOS default build, which uses AudioToolbox, is unchanged. (1) A final packet of fewer than 288 frames of a stereo or multichannel stream was scored from a different span of the encoder's work buffers than AudioToolbox uses, which for some content changed a predictor-order or compression choice and with it the packet (first seen with a 5-channel 44.1 kHz file and a 96 kHz stereo file); portable ALAC now equals AudioToolbox on more than 1.67 million compared packets and on recorded golden packets. (2) AudioToolbox does not write a packet larger than its converter's output buffer (near-full-scale incompressible audio in 4 or more channels at 32 bits, 6 or more at 24 bits or 8 at 16 bits, and input laden with NaN or infinity) but encodes it again later, and its cookie fields `maxFrameBytes` and `avgBitRate` count every attempt; the v0.3.0 writer made two attempts and counted only the packets it wrote, so its cookie differed, it refused some files AudioToolbox writes and it wrote some that the macOS writer's own frame check refuses. It now reproduces the retries: for write calls of a constant 512 frames, 48,000 generated cases gave the same file or a refusal on both sides (`encode_failed`) and none differed. The engine's own write calls are not all of that shape (the first write of a span is 512 frames minus the limiter latency modulo 512 and the last is cut to the file's length), so its runs belong to the class of write sizes that do not divide 4,096 frames: there the writer can refuse a file AudioToolbox would write (29 of 9,000 generated cases of mixed-size and 100-frame writes; the engine's own pattern was not swept separately), never write a different one. See [docs/LINUX.md](docs/LINUX.md#alac-output-on-linux-and-portable-builds).
- **Worker panics.** A panic inside the worker of one item of a `process`, `analyze` or `inspect` batch now fails that item alone (`internal_error`, stage `worker`) and the other items are processed and reported as usual; in v0.3.0 it ended the whole run (exit 101, no JSON outcome). None is known to be reachable. A panicking item can leave its staging files behind (`.pn-workflow-*` directories and a `.<name>.pn-reservation` file next to the output); remove them by hand before reprocessing that item. This is a limit, not a feature: v0.3.0 left the same files when the run ended.
- **Version and pins.** The version string is `platinum 0.4.0`. The macOS call-context pins are unchanged: AudioToolbox's encoder write and dispose calls and both `ExtAudioFileRead` call sites enter at stack-pointer residue 0, so the encoder runs in its "write class A" context and the decoder in class G; the Linux AAC ports reproduce exactly those classes and not the SME kernel variants (write class B, read class S), which the engine never selects. The Linux bridge, libFLAC, LAME and libopus pins of this release are unchanged since v0.3.0 (bridge `5c52c5f1…` aarch64 / `8c61f6bb…` x86_64, libFLAC `41b59a45…` aarch64 / `9420c96f…` x86_64, LAME `c780062f…` aarch64 / `cf59ae7f…` x86_64, libopus `c09146de…` aarch64 / `d09791af…` x86_64). The macOS default build (`d03cc190…`) differs from the v0.3.0 build (`fa4e1f3e…`) only in the version string and in containing a worker panic to its item; the engine's numerics are unchanged, and the portable AAC and ALAC code compiled into it is not reachable on the default build. Compared case by case with the v0.3.0 battery, the setting-off corpora (132 native cases, 575 stress, 30 pitch, 500 MP3-input and 47 bind-mount cases) differ in 18 results, all intentional and ledgered (the nine native AAC-input cases on each Linux architecture), and in none unexplained.
- **Error codes.** Still 39. `unsupported_output_codec` stays in the table as reserved: Linux produced it for AAC output and no build produces it now. `unsupported_input_codec` now means an AAC stream AudioToolbox decodes that the port does not reproduce (HE-AAC, AAC-LD/ELD, USAC, a stereo read of a damaged 5.1 stream with a foreign in-band program config element), AAC inside a WAVE container, MP3 inside a WAVE container, FLAC data under a name that does not select its reader, or `match-input` of an unrecognized source codec. `internal_error` also covers a panic inside one item's worker. `non_identical_paths` never lists AAC. See the [CLI contract](research/CLI_CONTRACT.md#error-codes).
- Everything else is unchanged: the command line, exit statuses (0/1/2/130), the existing per-file JSON (additive members only), no-clobber and atomic publication; macOS default behaviour changes only in the version string and in containing a worker panic as described above.
- **Evidence.** Encoder: complete `.m4a` files compared with AudioToolbox's on generated audio, identical on every item: 55 accepted configurations (17,005 access units; two more configurations were refused by both), 160 mixtures (30,572 access units), 240 (37,999) and 200 held-out items (37,119) on macOS arm64, Linux aarch64 and Linux x86_64, plus 35 adversarial inputs (2,456), a replay of the encoder input captured from the engine (378 access units), 44 items in the remaining rate and channel configurations, long items up to one hour (164,066 access units) and 48 empty-file layouts. Decoder: a 924-item research corpus and 457 Apple-encoded files in 12 variants each with no mismatch, 518 container probes with no mismatch once three malformed-file deviations of the research decoder were resolved, and 355 held-out items (also on Linux aarch64 and x86_64) with none. The checked-in fixture packs replay on every `cargo test`: 188 encoder inputs with AudioToolbox's whole files, and 838 decoder recordings over 790 inputs (510 required to be exact, 254 to fail with AudioToolbox's status, 74 typed refusals), 404 of which pin one container rule each (brands and `iTunSMPB`, `udta`, sample groups, size fields, `trun`, `stz2`, ASC and HE signalling, LOAS, in-band program config elements, CAF counts and layered descriptions); hostile-input tests run 6,000 mutated inputs and 36,000 grammar-generated LOAS, CAF and fragmented MP4 streams against time and allocation limits. The claims, their limits and the release freeze and battery results of the v0.4.0 build are in [VERIFICATION.md](docs/VERIFICATION.md#v040-changes-and-evidence):
  - **Freeze.** Source manifest `d47422978e0236d76e73fa9c34523cb871b37595b3b0706f708574546106ab03` (820 files): macOS arm64 remapped build `d03cc190ca4cf5c1c1e2dc6105f4b8ae9aca3e684e91b4e45209b71361302f9e`, Linux aarch64 `fd5f14dae3aac333c31eeeba1093f1eb199d01af3ed232fe9e3ec1270f07f776`, Linux x86_64 `3d196d68316557f44afd2acb2b917c0f708b998f87a6dcb67e9380f82e75a733`. All-target tests: aarch64 783 passed, 0 failed, 152 ignored (56 suites) and x86_64 779 passed, 0 failed, 150 ignored (56 suites) plus library-gated lanes of 50 tests each; macOS default 807 passed, 0 failed, 203 ignored (60 suites).
  - **Public source and rebuilds.** The public source differs from the frozen tree only in test code, comments, tools and documentation; the macOS binary rebuilt from it with the recorded recipe is byte-identical to the shipped one, and the Linux rebuilds have identical code and data (see [VERIFICATION.md](docs/VERIFICATION.md#v040-release-evidence)).
  - **Randomized campaigns** (generated audio, compared with AudioToolbox). 235,360 encoder cases gave no difference, panic or crash without an ambient flush-to-zero mode (18 differences occurred only under such a mode, which the engine never uses); 412,108 decoded files, compared bit for bit, gave no difference in the clean profile of 10,000 unmodified files, and the differences found in damaged or unusual files were fixed, refused or are listed in [docs/AAC_INPUT.md](docs/AAC_INPUT.md) under "Not reproduced" ([VERIFICATION.md](docs/VERIFICATION.md#v040-changes-and-evidence)).
  - **macOS battery** (`d03cc190ca4cf5c1c1e2dc6105f4b8ae9aca3e684e91b4e45209b71361302f9e`): The macOS default build and the `--features portable` build (`0bbeedb6…`) ran the same plans as the Linux builds. On the 13 AAC suites every one of the 1,695 scenarios (6,027 steps) met its expectation on both, with 0 failures, the 18 outputs for which the original application's own file was recorded (the nine native AAC references, setting off and on) equal that file byte for byte, and the SIGINT and full-disk family passed 11 of 11 on all four platforms. The macOS default build is the reference against which the Linux outputs were compared; macOS portable equals the Linux builds on every step of the AAC suites and, in the error-code suite, differs only in two harness cases that are attributed to the platform (`ec/lib/bridge-unapproved-text` and `ec/path/non-utf8-name`).
  - **Linux battery** (aarch64 and x86_64 identical on every step):
    - AAC output: 969 scenarios in eight suites (output matrix 212, rate and channel grid 120, held-out lane corpus 200, long items 9, AAC to AAC 224, tags and artwork 156, native references 18, workflows 30) met their expectations on aarch64, x86_64, macOS portable and the macOS default build with 0 failures, and their 3,567 steps compared with the macOS default build (MP4 clock pinned) were 2,620 identical in output bytes and reports, 203 failing with the same code on both, 352 typed refusals that publish nothing, 204 steps that declare the opt-in's non-identical paths and 188 later steps of those scenarios, 0 unexplained; aarch64 equals x86_64 and macOS portable on every step.
    - AAC input: 726 scenarios in five suites (proof corpus 130, new corpus 145, containers 207, damaged files 178, documented refusals 66) met their expectations on the four platforms with 0 failures, and their 2,694 steps compared with the macOS default build were 1,497 identical, 375 typed refusals that publish nothing, 705 failing with the same code on both and 117 matching an entry of the platform-difference ledger, 0 unexplained (an explicit-PS stream fails on both builds in analysis and conversion to WAV, with `unsupported_sample_rate_conversion` on Linux and `decode_failed` on macOS); of the 2,450 output files of the 13 AAC suites 2,228 equal the macOS default build's byte for byte and 18 equal the original application's own outputs, and 612 steps per Linux platform are typed refusals of which the macOS default build served 610.
    - ALAC output: the two files that exposed the corrected differences, a 5-channel 44.1 kHz file of 106,509 frames and a 96 kHz stereo file, now give the macOS default build's file byte for byte on aarch64, x86_64 and macOS portable in the AAC suites, and the ALAC steps of the format matrices of the error-code, cover-art, exact-MP3 and AAC suites show no unexplained difference; the corrected packets are pinned by golden files recorded from AudioToolbox (85 partial-packet and 48 oversized-packet cases, replayed without an Apple component) and by sweeps of 66,000 generated cases (see "ALAC output: evidence" in [VERIFICATION.md](docs/VERIFICATION.md#alac-output-evidence)).
    - The v0.3.0 corpora (native references, stress, pitch, exact MP3 input): with the setting off, 122 of 132 native cases equal the recorded Platinum Notes output exactly on each architecture, with 7 typed refusals and 3 matching native errors (v0.3.0: 113, 16 and 3; the nine new matches are AAC-input cases), 535 of 575 stress cases equal the macOS reference and 40 are sample-rate refusals, 30 of 30 pitch cases equal it, the 500-case MP3-input corpus gives 400 byte-identical outputs and 100 sample-rate refusals on aarch64, x86_64 and macOS portable alike, the Docker Desktop bind-mount subset gives 44 + 2 + 1 and 16 of 16 workflows match their recorded behaviour; compared case by case with the v0.3.0 battery there are 18 differences, all intentional and ledgered (the nine native AAC cases on each architecture), 0 unexplained, and the 575, 30, 500 and 47 cases are unchanged on every platform; exact MP3 input 315 + 146 + 172 + 812 scenarios (the admission matrix gained 91 AAC cells) with 0 failures, and the opt-in MP3 (188 scenarios) and Opus (26 + 7) suites pass.
    - Cover art and WAV tags: 235 cover-art scenarios, 8 size-limit scenarios (v0.3.0: 207 + 8) and 188 opt-in MP3 scenarios pass on both architectures with 0 failures; the keep-or-drop boundary of the WAV chunk rule is pinned in both directions (sources with and without tempo, key, text or picture copied into a WAV, setting off and on), and the eight opt-in outputs of the cover-art suite that kept an empty `ID3 ` chunk in v0.3.0 now equal the default policy's output, as ledgered.
    - Error codes: 262 scenarios, 3 limit scenarios and 3 cancellation scenarios pass on aarch64 and x86_64 with 0 failures, and the ledger accounts for all 39 codes: 32 observed through the CLI, `cancelled` observed through the signal scenarios, and 6 not reachable from the CLI (among them `unsupported_output_codec`, which no build produces any more, and the worker-panic path of `internal_error`, which a unit test covers).
    - In every run the loaded native libraries were checked against the approved digests (74 runs, 46,134 invocations, all mapped libraries pinned files).
  - **Reviews.** Three rounds of independent review (static, code and hands-on security reviews, with probes run against AudioToolbox where behaviour mattered) covered the AAC decoder and reader, the encoder and writer, the tests and fixtures, the ALAC change, the WAV chunk rule and the panic containment. Fixed before the freeze, with regression tests: quadratic or unbounded work on hostile MP4, CAF and LOAS files (a few MiB could cost tens of seconds of CPU and a 19-byte LOAS file allocated 512 MiB); a panic of the reader on a 60-byte LOAS file, introduced by a fix of the first round; HE-AAC in a CAF decoded with the wrong tables (now a typed refusal); an encoder that returned a short stream after a failed block; a panic of the encoder on eight-group short frames at the 15-band rates, a defect older than this release; ALAC cookie fields and final packets that differed from AudioToolbox; one worker panic that ended the whole batch; two Linux tests that still expected the old AAC refusal; and AAC sources missing from the public export. The third round, on the delta of these fixes, found no release-blocking issue. Limits that remain and are documented: a caught worker panic can leave its staging files behind (`.pn-workflow-*` directories and a `.<name>.pn-reservation` file next to the output), which must be removed by hand; the frames an AAC file yields are bounded by its packet count and not by its size; finishing an AAC writer needs about twice the stream size on disk and cannot be cancelled; the damaged-file shapes listed under "Not reproduced" in AAC_INPUT.md; and, for a stream that overflows to infinity, NaN sign bits that can differ between aarch64 and x86_64.

### Previous releases

#### v0.3.0

- **Exact MP3 input, replacing the v0.2.0 opt-in decoder.** Linux (and `--features portable`) decodes MP3 with a pure-Rust port of AudioToolbox's MPEG audio path (Layer I, II and III; macOS 27.2 build 26B123): the same samples, lengths, bitrates and refusal/error `OSStatus` values as the macOS reference on the generated corpora it was verified against (no mismatch in 15,840 corpus files on macOS arm64, Linux aarch64 and Linux x86_64), and for 910 of the 922 generated inputs that the macOS default build and this one both processed the `process` output files were byte-identical (the other 12 are synthetic fuzz streams on which Apple's own decoder output depends on process history). It needs no setting; see [docs/LINUX.md](docs/LINUX.md#mp3-input) for its boundaries (sequential reads only, 2 GiB input limit, rates that need conversion stay refused). It **replaces** the open-source, not byte-exact MP3 decoder that v0.2.0 shipped behind `parity.allow_non_identical`: the `mp3_decode` member of `report.non_identical_paths` and the setting's MP3 gating no longer exist, and MP3 input is no longer refused when the setting is off. The setting now covers only cover-art copy and WAV tag chunks, and a state file that stores it stays valid. See [Upgrading from v0.2.0](#upgrading-from-v020).
- **Opt-in cover art and WAV tags (Linux tag bridge v09).** With `parity.allow_non_identical` on, a WAV now carries cover art like MP3 and AIFF in cross-format copies (v0.2.0 carried none into or out of a WAV), and a destination ID3v2 tag above its 268,435,455-byte size limit is refused with `artwork_conversion_unavailable` before anything is written (the v0.2.0 bridge let such tags through and TagLib wrote a wrapped size field). The Linux bridge is built with stack protector, `_FORTIFY_SOURCE=2` and immediate binding with a read-only relocation segment and has a new SHA-256, so the v0.2.0 bridge is refused by the v0.3.0 CLI; the libFLAC and LAME pins are unchanged. In v0.3.0 a cross-format copy into a WAV still kept the `ID3 ` chunk that TagLib writes (`TBPM` "0", empty `TKEY`) under the opt-in; v0.4.0 drops that chunk when it carries nothing the source had (see above). Without the setting nothing changed in v0.3.0, and Linux still refused AAC input and output (v0.4.0 adds both).
- **Opt-in Opus input and output.** `platinum --state-dir DIR config set formats.opus true` (default `false`) adds Ogg Opus (RFC 7845, mono or stereo, 48 kHz, no sample-rate conversion) as an input and as a `--format opus` output, using a pinned fixed-point libopus 1.6.1 (`libraries.opus`, `libraries.opus_sha256`). Platinum Notes refuses `.opus`, so **there is no Platinum Notes reference for Opus**: each Opus item says so in its report, and the output is only claimed to be identical on all four builds (macOS default, macOS portable, Linux aarch64, Linux x86_64), which the Opus corpus comparison in `local-artifacts/opus-lane-01/` checks (it was run before the exact MP3 reader existed). Without the setting every build refuses `.opus` exactly as before. Tags are carried verbatim (Xiph comments, pictures unchanged) and the processing marker is stored as in FLAC. See the [CLI contract](research/CLI_CONTRACT.md#opt-in-format-opus).
- **File-type admission and two alignments (no output byte changes).** A name ending `.mp1`, `.mp2`, `.mp3`, `.mpa` or `.mpeg` commits to the MPEG reader on Linux as on macOS, so a RIFF/WAVE or AIFF file named `.mp3` is refused as `invalid_input_file` on both (v0.2.0 processed it by content on Linux); under other names the content decides. macOS `inspect` of a symlink reports `invalid_input_path` (stage `inspect source identity`) like Linux; it reported `io_error`.
- **macOS AAC decode call context.** Both `ExtAudioFileRead` call sites (planar and waveform) now enter AudioToolbox at stack-pointer residue 0 through the trampoline that already pins the AAC encoder's write and dispose calls, so the decoder runs its generic kernel by construction and not by compiler frame layout; output is unchanged. See [VERIFICATION.md](docs/VERIFICATION.md#v030-changes-and-evidence).
- **Error codes.** 39 (the 37 of v0.2.0 plus `opus_not_enabled` and `unsupported_opus_variant`); the full table is in the [CLI contract](research/CLI_CONTRACT.md#error-codes). The version string is 0.3.0.
- Everything else is unchanged: the command line, exit statuses (0/1/2/130), the existing per-file JSON (additive members only, apart from the removed `mp3_decode`), no-clobber and atomic publication; macOS default behavior changes only in the two items above.
- **Evidence.** The already-recorded evidence (MP3 corpora, admission matrix, Opus corpus, bridge v09 matrices) is in [VERIFICATION.md](docs/VERIFICATION.md#v030-changes-and-evidence); the release freeze and battery results of the v0.3.0 build:
  - **Freeze.** Source manifest `17e875b0…` (747 files): macOS arm64 remapped build `fa4e1f3e…`, Linux aarch64 `71574242…`, Linux x86_64 `c4e5e953…`. All-target tests: aarch64 532 and x86_64 530 passed (none failed; 147 and 145 ignored) plus library-gated lanes of 48 tests each (error codes, opt-in, cover art, Opus); macOS default 569 passed (none failed, 200 ignored) plus its opt-in lanes.
  - **macOS battery** (`fa4e1f3e…`, none failed, none unclassified): 71/71 retained replay cases, 8/8 dynamics/pitch cases, 10/10 MP4 exports, 11/11 strict family cases, 13/13 synthetic AAC outputs equal to the v0.1.0 and v0.2.0 binaries and 4/4 native AAC references; all 120 output digests and 131 statuses equal the v0.1.0 and v0.2.0 baselines apart from the announced items above.
  - **Linux battery** (aarch64 and x86_64 identical on every step, none unexplained): with the opt-in off, 113 of 132 native cases match the recorded Platinum Notes output exactly (16 typed refusals and 3 matching native errors; v0.2.0: 104, 25, 3 — the new matches are MP3 inputs and one file of Ogg bytes named `.mp3`), 535 of 575 stress cases (40 sample-rate refusals) and 30 of 30 pitch cases equal the macOS reference and v0.2.0, and a new 500-case MP3-input corpus gives 400 byte-identical outputs plus 100 sample-rate refusals against the macOS default build; exact MP3 input 315 + 146 + 172 + 721 scenarios against the macOS default build (byte-identical or the same typed refusal, documented exceptions only), cover art and WAV tags 207 + 8, error codes 245 + 3 + 3 (all 39), Opus 26 + 7 scenarios with the same Opus output on macOS default, macOS portable and both Linux architectures.
  - **Reviews.** A static review, a code review and a hands-on security review (631,050 mutated MP3 and 657,996 structure-aware Opus files, the bridge under ASan/UBSan) found no critical, high or medium issue; the fixes and the bounded limits that remain are in [VERIFICATION.md](docs/VERIFICATION.md#v030-release-evidence-freeze-01) and [LINUX.md](docs/LINUX.md#hostile-input-limits).

#### v0.2.0 in brief

- **Opt-in cover art and WAV tags on Linux.** `parity.allow_non_identical` (default `false`) made Linux copy embedded cover art unchanged where macOS would transcode or drop it and keep the tag chunks of a WAV source (`ID3 `, INFO `LIST`) that the reference drops, listed in `report.non_identical_paths` (`artwork_copy`, `wav_tag_preservation`); the same setting gated an open-source MP3 decoder (`mp3_decode`) that v0.3.0 replaced by the exact port.
- **Error codes.** Every failed item, batch error and top-level error carries a stable machine-readable `code` (37 codes in v0.2.0).
- **Evidence.** The v0.2.0 macOS release build (`75c2ac87…`) passed 512 regular tests (0 failed, 190 ignored), 71/71 retained replay cases, 8/8 dynamics/pitch cases, 10/10 MP4 exports and 11/11 strict family cases, and its default behavior matches v0.1.0 on 276 comparison records. On each Linux architecture the executables passed their all-target suites (aarch64 477, x86_64 475 tests, none failed) and library-gated lanes (33 tests each), and with the opt-in off matched the macOS reference exactly in 104 of 132 native cases (25 typed refusals, 3 matching native errors), 535 of 575 stress cases (40 sample-rate refusals) and 30 of 30 pitch cases, byte-identical to the v0.1.0 receipts; with the opt-in on, aarch64 and x86_64 agreed on 175/175 MP3 (the replaced opt-in decoder), 184/184 cover-art and 174/174 error-code scenarios. Details, limits and known differences are in [VERIFICATION.md](docs/VERIFICATION.md#v020-release-evidence).

The sections below describe the v0.1.0 evidence; v0.2.0's evidence is listed separately in [VERIFICATION.md](docs/VERIFICATION.md#v020-release-evidence).

## v0.1.0 at a glance

Platinum Engine supports **macOS arm64**, the reference platform, and Linux
**aarch64** and **x86_64**. The portability target is complete-file byte identity
with the measured macOS reference on supported paths; unsupported paths are
refused or reported unavailable. The implementation is not a proof of
identity for every input: see the x86 floating-point residual and the other
boundaries below. The recorded Apple M4/SME component reference was macOS 27.2
build 26B123; other captures used the builds stated in the verification guide.

In earlier verified reference cases the engine reproduced complete original
output files, including audio, container fields, tags, artwork and processing
markers, with no normalization. The observed coverage includes:

- all three built-in templates and custom templates;
- pitch correction in both modes and directions, warmth, expansion, compression and their gates;
- WAV, AIFF, FLAC, ALAC, MP3 (CBR and VBR) and AAC output;
- mono, stereo and six-channel inputs from 22.05 to 96 kHz;
- silence, short, long and odd-length signals, and metadata;
- folder, mirror and same-folder output, cancellation, error, and replacement/restore workflows.

Nine representative format and feature families passed a strict review for the earlier `dd4b5fd0` build in the recorded original-app context. Later path-relocated replays have their own documented context limits. The first pinned Official/WAV case matched in all 3,076,786 bytes (SHA-256 `06dbf2b016bf3389aa997ce7dd9816a663d226bde5d38e2b4cb62c3abb0d0c1b`).

A finite test set cannot prove identical output for every possible input, setting or runtime. [VERIFICATION.md](docs/VERIFICATION.md) separates final-build, component and historical results. The runtime uses Rust DSP and separately built public libraries; macOS additionally uses system frameworks, while Linux uses portable implementations. It does not call the original application or its proprietary components, which are used only for optional reference comparisons.

### Measured progress

| Area | Verified result or remaining gate |
| --- | --- |
| macOS release build | `198e0091…` passed 440 top-level regular tests (179 ignored) and 38 selected public-dependency tests. It matched 71/71 retained full-file cases, eight dynamics/pitch cases, ten controlled-second MP4 exports, 15 boundary files and the W04 exact-path workflow. Nine family cases matched in a path-relocated review, not the earlier identical-path strict context. Sets overlap. |
| Earlier macOS freeze | The source-path-remapped `82d8b489…` build passed 440 top-level regular tests (181 ignored) and 38 selected public-dependency tests, with the separately documented complete-file replays. The release rebuild adds privacy path remaps; the two binaries are not byte-identical. |
| Linux battery | On each architecture, 104 native cases matched completely, 25 were documented refusals and three matched native error/no-output behavior; 16 workflows matched recorded behavior. Of 575 stress cases, 535 matched macOS bytes and 40 were documented refusals; 30 additional pitch cases matched. These bounded sets overlap and do not establish universal parity. |
| Earlier macOS candidate | The `c9f19077…` candidate passed 426 regular tests (179 ignored) and 37 selected public-dependency tests; it matched 71/71 retained full-file cases, 8/8 dynamics/pitch cases, 10/10 controlled-second MP4 exports and six extension cases. Nine family cases also matched in a **path-relocated** review, not the earlier identical-path strict context. Sets overlap; this candidate predates later fixes. |
| Clip repair | 32 generated native cases matched every sample bit and captured event property. |
| Wrapper phase correction | 40 generated native cases matched coefficients, samples, latency and state transitions in debug and release builds. |
| Shipped multiband graphs | All four graphs match captured phase-off output/intermediates and long phase-enabled output, including current-template controls and native shared-buffer warmup. |
| Limiter | All 112,384 sample words in the captured 56,192-frame stereo impulse/tail case match after replaying the observed host defaults. |
| Pitch correction | Four original AU audio captures and controlled scheduler checkpoints match in both modes, and every pitch-active native export matches the FFT alignment profile the CLI uses. |
| Analysis | Original tuning, sinc-table, partitioned-resampler and harmonic-removal leaves pass. The whitening boundary has a regular synthetic native-golden regression; restoring the old boundary makes it fail. The original-app tuning and pinned complete-file comparisons pass. |
| Earlier Rust test runs | An earlier private snapshot passed 317 regular tests, with 92 opt-in tests ignored. Separately, 20 public-library integration tests, the Xing caller-byte oracle and three native waveform-reducer capture cases passed. These are historical, overlapping counts. |
| Codecs and tags | Public FLAC/LAME paths and 38 original metadata/container cases have bounded matches. The lower-level original and Rust Ogg decoders matched 96 generated cases. An AAC fixture varied across fresh native processes, while original and direct system reads matched within each sampled process. |
| Earlier final-binary files | Release `dd4b5fd0…` matched both native references in nine full-context family captures, eight dynamics/pitch cases, 71 selected cases, all ten controlled-second MP4 exports, the Xing E05 case and 15 dynamics/pitch-gate cases. These sets overlap and are historical, not a claim about a later binary. |
| File safety | Review corrections passed: retained preflight identities, metadata/resource-fork/ACL-preserving backups, collision reservations, decoder-independent restore, native nil markers, synchronized cancellation/publication, identity- and digest-checked `--overwrite` swaps that retain rollback files, and backup consumption only after a synced, verified restore. Real CLI SIGINT exited 130 without publishing; replacement/restore preserved generated source bytes and metadata. |

**Known limits:** Pitch correction uses the observed aligned-0 FFT profile; the original GUI's literal allocation context was not observed. AudioToolbox AAC decoding can depend on stack context and original process; AAC and ALAC MP4 headers also contain the export second (a Linux AAC output carries the close-time second as macOS does; the hidden `--controlled-mp4-time-1904` diagnostic option pins it for whole-file comparison). By default Linux refuses unreproduced sample-rate conversion (including non-44.1 kHz waveforms), artwork transcoding, unverified codec/container variants and ALAC native-loss cases; the explicit opt-in `parity.allow_non_identical` replaces only the artwork refusal (and keeps the WAV tag chunks that carry source data and the reference drops) with paths that are **not** claimed identical to macOS. MP3 input is decoded by the exact port, and AAC input and output by the exact AAC ports (finite evidence on generated audio; see docs/LINUX.md): AAC is reproduced for the engine's own configuration only, that is AAC-LC input and 256 kbit/s output at the listed rates and channel counts. HE-AAC whose SBR or PS is signalled, AAC-LD/ELD, USAC and sample-rate conversion are refused, and the SME kernel variants of AudioToolbox's AAC encoder and decoder are not reproduced (the engine's macOS build never selects them). The AAC reader reproduces damaged and unusual files for the shapes its fixture pack records; other damaged shapes may give a different result or status (docs/AAC_INPUT.md, "Not reproduced"). The AAC encoder differs from AudioToolbox for input made of tiny normal numbers when the caller's flush-to-zero mode is set, which the engine never does. Linux aarch64 is the stricter cross-platform target. On x86_64, ordinary operations can retain a minimum normal result where ARM flushes before rounding; invalid-operation NaN signs and NaN payload selection, including vectorized FMA operand order, can also differ. These are known residual classes, not a promise that every affected input is detected and refused. Other Apple Silicon generations or OS versions may dispatch Accelerate differently or change AudioToolbox behavior. See [Linux](docs/LINUX.md) and [verification](docs/VERIFICATION.md) for precise scope.

**Marker limitation:** Duplicate AIFF ID3 chunks are refused, but source
review found that deliberately conflicting markers inside a single ID3 tag,
duplicate MP4 items, or FLAC Xiph-versus-ID3 markers may conceal a later
processed marker. Sync-like junk or separated ID3 tags may do so as well.
These are source-derived scenarios; the reviewer did not establish that the
exact crafted inputs decode. Use trusted inputs and retain independent backups.

Historical records retain earlier failures, the whitening correction and the crossover-ramp correction. Component results, observed file matches and strict-context acceptance are separate claims; see [VERIFICATION.md](docs/VERIFICATION.md).

## Scope

The implementation exposes volume and pitch analysis, clipping repair, pitch correction, warmth, multiband expansion/compression, output limiting, built-in/custom templates, encoding, tags and artwork. It also supports batch status and cancellation, processed-file detection, separate output folders, explicit replacement with backups, restore, cumulative Engine-owned statistics, and machine-readable analysis and waveform reports. `--allow-ambiguous-marker` is a process-only opt-in for specific ambiguous WAV markers; it can reprocess a file and does not relax restore or no-clobber safety. Implemented controls are not a claim that every combination matches the reference.

The [CLI contract](research/CLI_CONTRACT.md) specifies commands, settings,
refusals, safety behavior and the verification boundary. The reference is
Platinum Notes 10.5.0.1256 for macOS arm64; no reference installation is
required to run the CLI.

## Install and run

Release archives are named `platinum-engine-v0.4.0-macos-arm64.tar.gz`,
`platinum-engine-v0.4.0-linux-arm64.tar.gz`, and
`platinum-engine-v0.4.0-linux-amd64.tar.gz`. Obtain the appropriate archive and
the accompanying `SHA256SUMS` from the
[GitHub Releases page](https://github.com/mschienbein/platinum-engine/releases),
verify its digest using a trusted release checksum, and unpack
it in a new directory. The archive's `README.txt` gives commands to verify its
internal checksums and explicitly configure its library paths and codec
digests; it does not configure them automatically. Checksums alone do not
authenticate an untrusted download.

After downloading the selected archive and the release's `SHA256SUMS` into
the current directory, set `ARCHIVE` to its filename. This example refuses an
existing installation directory and verifies the package before using it:

```sh
(
  set -eu
  python3 - "$ARCHIVE" <<'PY'
import hashlib
from pathlib import Path
import sys
archive = Path(sys.argv[1])
suffix = "  " + archive.name
matches = [line[:-len(suffix)] for line in Path("SHA256SUMS").read_text().splitlines()
           if line.endswith(suffix)]
if len(matches) != 1:
    raise SystemExit("expected exactly one checksum for the selected archive")
digest = hashlib.sha256()
with archive.open("rb") as file:
    for chunk in iter(lambda: file.read(1024 * 1024), b""):
        digest.update(chunk)
if digest.hexdigest() != matches[0]:
    raise SystemExit("release archive checksum mismatch")
PY
  mkdir "$HOME/platinum-engine-v0.4.0"
  tar -xzf "$ARCHIVE" -C "$HOME/platinum-engine-v0.4.0"
  cd "$HOME/platinum-engine-v0.4.0"
  if [ "$(uname -s)" = Darwin ]; then shasum -a 256 -c SHA256SUMS
  else sha256sum -c SHA256SUMS; fi
  ./bin/platinum --help
  ./bin/platinum --state-dir "$HOME/platinum-state" --json templates list
)
```

### Opt in to cover art and WAV tags on Linux

A Linux caller that must keep embedded cover art and WAV tag chunks sets one
boolean in its state directory, once, after the libraries are configured (the
archive's `README.txt` shows how); the `process` command line does not change:

```sh
./bin/platinum --state-dir "$HOME/platinum-state" config set parity.allow_non_identical true
```

The default is `false`. With it on, cover art is copied byte for byte (also into
and out of a WAV) and the tag chunks of a WAV source that carry source data are
kept; items that took either path say so in `report.non_identical_paths`, and
their output is **not** guaranteed byte-identical to macOS. MP3 input and AAC
input and output need no setting: they are the exact ports of AudioToolbox's
MP3 reader and AAC encoder and decoder, and the setting does not touch them.
The setting has no effect on the macOS build. Details and limits:
[LINUX.md](docs/LINUX.md#opt-in-cover-art-and-wav-tags).

### Upgrading from v0.1.0

Unpack v0.4.0 into a new directory next to v0.1.0; do not overwrite it. Library
configuration lives in the state directory, so after unpacking, point
`libraries.tag_bridge` at the v0.4.0 bridge (and, if you want the new
directory to be self-contained, `libraries.flac` and `libraries.lame` at its
copies). On Linux the bridge has had a new SHA-256 since v0.3.0 and the v0.1.0
bridge is refused by the v0.4.0 CLI, so this step is required there; the libFLAC
and LAME files are byte-identical to v0.1.0 and keep their SHA-256 pins. Without
the opt-in, Linux behaves as in v0.1.0 except that MP3 input is decoded exactly
(since v0.3.0) and AAC input and output are exact (since v0.4.0); every JSON
change is additive (error `code` members, `report.non_identical_paths`, the Opus
members when Opus is used).

### Upgrading from v0.2.0

Unpack v0.4.0 into a new directory next to v0.2.0 and repoint
`libraries.tag_bridge` at the v0.4.0 bridge: on Linux the bridge has had a new
SHA-256 since v0.3.0 and the v0.4.0 CLI refuses the v0.2.0 bridge (libFLAC and
LAME are unchanged; libopus is needed only for Opus). A state file that stores
`parity.allow_non_identical` stays valid, but the setting no longer gates MP3
input: MP3 is decoded exactly with or without it, the `mp3_decode` member of
`report.non_identical_paths` no longer exists, and samples decoded from MP3 can
differ from those the v0.2.0 opt-in decoder produced. A state file that stores
`formats` or `libraries.opus*` cannot be read by v0.2.0. Details:
[LINUX.md](docs/LINUX.md#upgrading-from-v020).

### Upgrading from v0.3.0

Unpack v0.4.0 into a new directory next to v0.3.0. Library configuration lives
in the state directory and a state file stays valid. The Linux bridge, libFLAC,
LAME and libopus pins of v0.4.0 are unchanged since v0.3.0 (bridge `5c52c5f1…` aarch64 / `8c61f6bb…` x86_64, libFLAC `41b59a45…` aarch64 / `9420c96f…` x86_64, LAME `c780062f…` aarch64 / `cf59ae7f…` x86_64, libopus `c09146de…` aarch64 / `d09791af…` x86_64); the CLI refuses a
library that is not on its allowlist, as before. Three behaviours change on Linux
and `--features portable` builds. AAC input and output, which v0.3.0 refused
(`unsupported_input_codec` and `unsupported_output_codec`), are now exact and need
no setting; `unsupported_output_codec` is no longer produced, and a hand-built
AAC file that v0.3.0 refused as an unsupported codec can now be reported as
`invalid_input_file` or decoded, and an MP4 whose first sound entry is neither
`mp4a` nor `alac` nor a type AudioToolbox opens as another codec is now
`invalid_input_file` (`'wht?'`, as on macOS) where v0.3.0 answered
`unsupported_input_format`. ALAC output files can differ from v0.3.0's in two
cases, where v0.3.0 differed from AudioToolbox: the last packet of a stereo or
multichannel stream shorter than 288 frames, and files with packets that
AudioToolbox has to encode again (see the ALAC bullet above); an output that
v0.3.0 wrote can now be refused with `encode_failed` where the macOS writer
refuses it too. With `parity.allow_non_identical` on, a WAV
output whose source has no tempo, key or tags no longer keeps TagLib's empty
`ID3 ` chunk and no longer lists `wav_tag_preservation`: its bytes equal the
default output's. Consumers that required that member for such items should stop
doing so. On every build, a panic in one batch item's worker no longer ends the
run, but it can leave that item's staging files behind (`.pn-workflow-*`
directories and a `.<name>.pn-reservation` file next to the output): remove them
by hand before reprocessing the item. Details: [LINUX.md](docs/LINUX.md#upgrading-from-v030).

The multi-architecture container image is built by the release workflow from the
verified Linux archives and published as `ghcr.io/mschienbein/platinum-engine:v0.4.0`
(also `:latest`). The image runs as
an unprivileged user, does not auto-configure libraries, and needs durable state
and deliberately selected input/output mounts. In Docker Desktop on macOS, use
a container volume or tmpfs for state, and use the libraries already inside
the image, rather than bind-mounted libraries. See [LINUX.md](docs/LINUX.md)
for complete setup, volume ownership, example commands and platform limits.

For source builds, see [BUILD.md](docs/BUILD.md). Use `--state-dir` to isolate
settings and a separate output directory for initial processing. Existing
outputs are refused unless `--overwrite` is given. Replacement requires
explicit flags and an existing backup directory on every run; retain an
independent backup. Restore requires the exact recorded backup and consumes
that backup after a verified restore; some nil markers cannot authorize
restore. Read the [CLI contract](research/CLI_CONTRACT.md) first.

## Reference evidence and limits

The pinned result compares entire encoded files, including headers and processing markers; component-only tests are narrower diagnostics. The original pitch-correction alignment and MP3 header behavior described above can depend on runtime state, so original-to-original repeatability must be established for each new case rather than assumed. Original-component comparisons require a separately obtained local installation; they are not production dependencies.

The [verification guide](docs/VERIFICATION.md) summarizes the pinned case, the earlier mismatch and correction, test prerequisites, and the boundaries of the evidence without requiring raw reference captures.

The analyzer is identified from static evidence as K-weighted integrated LUFS-style measurement, not a demonstrated drum extractor. The controller follows analyze → heal/remeasure → final limit/encode. Neither these observations nor the presence of a command prove every reference behavior.

## Licensing

Project-owned contributions are available under the [MIT license](LICENSE), to the extent their contributors can grant those rights. Third-party terms apply: the separate bridge statically links LGPL-2.1 TagLib 1.11.1, LAME 3.100 is LGPL-2.0-or-later, libFLAC 1.4.2 uses BSD-3-Clause terms, and the optional libopus 1.6.1 uses BSD-3-Clause terms with royalty-free patent licenses from Xiph.Org, Microsoft and Broadcom. Apple's Apache-2.0 ALAC reference was studied for the independently written Rust implementation. See [THIRD_PARTY_NOTICES](THIRD_PARTY_NOTICES) and [NOTICE](NOTICE) for attribution, sources, package source/relink materials, and limitations. Neither the MIT license nor these notices grant rights to the reference application or constitute legal clearance.

The exact MP3 reader in `src/portable/audio/mp3/` is a project-written compatibility implementation whose behavior and numeric constants (33 tables) were recovered from Apple's AudioToolbox binaries on one reference machine by static analysis, instruction-level transcription and recorded runtime states; no Apple binary or source code is included, and since v0.4.0 not the test-only register-level model of one Apple routine (the polyphase synthesis) that the v0.3.0 source compiled in its unit tests. [THIRD_PARTY_NOTICES](THIRD_PARTY_NOTICES) states its provenance, and no legal clearance is claimed.

The exact AAC encoder and decoder in `src/portable/aac/` (wired in through `src/portable/audio.rs`) are project-written compatibility implementations of the same kind: their behavior and numeric constants (19 table and data files, ten of them binary, among them constant tables that are bit-exact copies of data read from the arm64e slice of Apple's AudioCodecs component) were recovered from Apple's AudioToolbox binaries on one reference machine by static analysis, instruction-level transcription and recorded runtime states; no Apple binary or source code is included and no Apple API is called. The AAC modules contain no `unsafe` code apart from one aarch64-only unit-test module; the shared arithmetic modules they use and the flush-to-zero control are not AAC-specific and contain small `unsafe` blocks. [THIRD_PARTY_NOTICES](THIRD_PARTY_NOTICES) states their provenance, and no legal clearance is claimed. AAC is a patented format: patent licensing may apply to AAC encoders and decoders in some jurisdictions, and the project grants no patent rights and gives no legal advice. The macOS default build uses AudioToolbox itself.

This reconstruction was informed by static binary analysis and measured reference behavior; it is not a clean-room provenance claim. Compatibility names, identifiers and typed parameter values retain their reference-derived provenance. The public snapshot excludes original serialized preset blobs, raw decompiler/disassembly exports, compiled resources, screenshots and the private research history.

## Verification policy

Acceptance requires **SHA-256-identical complete output files** to Platinum Notes 10.5.0.1256 for the same source bytes and effective settings. Encoded audio, container/header bytes and metadata all count. First establish original-to-original repeatability with the reference build/runtime and metadata-relevant paths/state pinned. If the original is nondeterministic, retain and explain the differing bytes rather than quietly weakening the requirement. Per-stage PCM comparisons are diagnostics only. A command, flag, generic DSP substitute, or wrapper around the original proprietary engine is not a completed Rust port.
