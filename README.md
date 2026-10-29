# Platinum Engine

A Rust command-line audio processor that reproduces Platinum Notes 10.5 processing and file workflows, writing byte-identical output files in every verified reference case. It is a headless engine, not a graphical clone or a generic loudness normalizer. Platinum Engine is not affiliated with the vendor.

## What changed in v0.3.0

- **Exact MP3 input, replacing the v0.2.0 opt-in decoder.** Linux (and `--features portable`) decodes MP3 with a pure-Rust port of AudioToolbox's MPEG audio path (Layer I, II and III; macOS 27.2 build 26B123): the same samples, lengths, bitrates and refusal/error `OSStatus` values as the macOS reference on the generated corpora it was verified against (no mismatch in 15,840 corpus files on macOS arm64, Linux aarch64 and Linux x86_64), and for 910 of the 922 generated inputs that the macOS default build and this one both processed the `process` output files were byte-identical (the other 12 are synthetic fuzz streams on which Apple's own decoder output depends on process history). It needs no setting; see [docs/LINUX.md](docs/LINUX.md#mp3-input) for its boundaries (sequential reads only, 2 GiB input limit, rates that need conversion stay refused). It **replaces** the open-source, not byte-exact MP3 decoder that v0.2.0 shipped behind `parity.allow_non_identical`: the `mp3_decode` member of `report.non_identical_paths` and the setting's MP3 gating no longer exist, and MP3 input is no longer refused when the setting is off. The setting now covers only cover-art copy and WAV tag chunks, and a state file that stores it stays valid. See [Upgrading from v0.2.0](#upgrading-from-v020).
- **Opt-in cover art and WAV tags (Linux tag bridge v09).** With `parity.allow_non_identical` on, a WAV now carries cover art like MP3 and AIFF in cross-format copies (v0.2.0 carried none into or out of a WAV), and a destination ID3v2 tag above its 268,435,455-byte size limit is refused with `artwork_conversion_unavailable` before anything is written (the v0.2.0 bridge let such tags through and TagLib wrote a wrapped size field). The Linux bridge is built with stack protector, `_FORTIFY_SOURCE=2` and immediate binding with a read-only relocation segment and has a new SHA-256, so the v0.2.0 bridge is refused by the v0.3.0 CLI; the libFLAC and LAME pins are unchanged. A cross-format copy into a WAV still always gains an `ID3 ` chunk (`TBPM` "0", empty `TKEY`) under the opt-in: that policy is unchanged in v0.3.0 and remains open. Without the setting nothing changed; AAC input and output stay refused on Linux (the AAC encoder and decoder ports are research, not shipped).
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

### v0.2.0 in brief

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

**Known limits:** Pitch correction uses the observed aligned-0 FFT profile; the original GUI's literal allocation context was not observed. AudioToolbox AAC decoding can depend on stack context and original process; AAC and ALAC MP4 headers also contain the export second. By default Linux refuses AAC input decoding, AAC encoding, unreproduced sample-rate conversion (including non-44.1 kHz waveforms), artwork transcoding, unverified codec/container variants and ALAC native-loss cases; the explicit opt-in `parity.allow_non_identical` replaces only the artwork refusal (and keeps the WAV tag chunks the reference drops) with paths that are **not** claimed identical to macOS, and AAC stays refused. MP3 input is decoded by the exact port (finite evidence on generated audio; see docs/LINUX.md). Linux aarch64 is the stricter cross-platform target. On x86_64, ordinary operations can retain a minimum normal result where ARM flushes before rounding; invalid-operation NaN signs and NaN payload selection, including vectorized FMA operand order, can also differ. These are known residual classes, not a promise that every affected input is detected and refused. Other Apple Silicon generations or OS versions may dispatch Accelerate differently or change AudioToolbox behavior. See [Linux](docs/LINUX.md) and [verification](docs/VERIFICATION.md) for precise scope.

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

Release archives are named `platinum-engine-v0.3.0-macos-arm64.tar.gz`,
`platinum-engine-v0.3.0-linux-arm64.tar.gz`, and
`platinum-engine-v0.3.0-linux-amd64.tar.gz`. Obtain the appropriate archive and
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
  mkdir "$HOME/platinum-engine-v0.3.0"
  tar -xzf "$ARCHIVE" -C "$HOME/platinum-engine-v0.3.0"
  cd "$HOME/platinum-engine-v0.3.0"
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
and out of a WAV) and the tag chunks of a WAV source are kept; items that took
either path say so in `report.non_identical_paths`, and their output is **not**
guaranteed byte-identical to macOS. MP3 input needs no setting: it is decoded
by the exact port of AudioToolbox's MP3 path. AAC input and output stay refused
on Linux. The setting has no effect on the macOS build. Details and limits:
[LINUX.md](docs/LINUX.md#opt-in-cover-art-and-wav-tags).

### Upgrading from v0.1.0

Unpack v0.3.0 into a new directory next to v0.1.0; do not overwrite it. Library
configuration lives in the state directory, so after unpacking, point
`libraries.tag_bridge` at the v0.3.0 bridge (and, if you want the new
directory to be self-contained, `libraries.flac` and `libraries.lame` at its
copies). On Linux the v0.3.0 bridge has a new SHA-256 and the v0.1.0 bridge is
refused by the v0.3.0 CLI, so this step is required there; the libFLAC and LAME
files are byte-identical to v0.1.0 and keep their SHA-256 pins. Without the
opt-in, Linux behaves as in v0.1.0 except that MP3 input is now decoded exactly;
every JSON change is additive (error `code` members,
`report.non_identical_paths`, the Opus members when Opus is used).

### Upgrading from v0.2.0

Unpack v0.3.0 into a new directory next to v0.2.0 and repoint
`libraries.tag_bridge` at the v0.3.0 bridge: on Linux the v0.3.0 bridge has a
new SHA-256 and the v0.3.0 CLI refuses the v0.2.0 bridge (libFLAC and LAME are
unchanged; libopus is needed only for Opus). A state file that stores
`parity.allow_non_identical` stays valid, but the setting no longer gates MP3
input: MP3 is decoded exactly with or without it, the `mp3_decode` member of
`report.non_identical_paths` no longer exists, and samples decoded from MP3 can
differ from those the v0.2.0 opt-in decoder produced. A state file that stores
`formats` or `libraries.opus*` cannot be read by v0.2.0. Details:
[LINUX.md](docs/LINUX.md#upgrading-from-v020).

The multi-architecture container image is built by the release workflow from the
verified Linux archives and published as `ghcr.io/mschienbein/platinum-engine:v0.3.0`
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

The exact MP3 reader in `src/portable/audio/mp3/` is a project-written compatibility implementation whose behavior and numeric constants (33 tables) were recovered from Apple's AudioToolbox binaries on one reference machine by static analysis, instruction-level transcription and recorded runtime states; no Apple binary or source code is included. [THIRD_PARTY_NOTICES](THIRD_PARTY_NOTICES) states its provenance, and no legal clearance is claimed.

This reconstruction was informed by static binary analysis and measured reference behavior; it is not a clean-room provenance claim. Compatibility names, identifiers and typed parameter values retain their reference-derived provenance. The public snapshot excludes original serialized preset blobs, raw decompiler/disassembly exports, compiled resources, screenshots and the private research history.

## Verification policy

Acceptance requires **SHA-256-identical complete output files** to Platinum Notes 10.5.0.1256 for the same source bytes and effective settings. Encoded audio, container/header bytes and metadata all count. First establish original-to-original repeatability with the reference build/runtime and metadata-relevant paths/state pinned. If the original is nondeterministic, retain and explain the differing bytes rather than quietly weakening the requirement. Per-stage PCM comparisons are diagnostics only. A command, flag, generic DSP substitute, or wrapper around the original proprietary engine is not a completed Rust port.
