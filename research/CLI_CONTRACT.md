# Platinum Engine CLI contract

Status: the Rust CLI implements this command surface on macOS arm64 and Linux aarch64/x86_64, subject to the restrictions below. Two Platinum Notes 10.5.0.1256 reference exports and Rust debug/release outputs matched complete bytes for one generated 16-second, 48 kHz, stereo 16-bit Official-template WAV case, SHA-256 `06dbf2b016bf3389aa997ce7dd9816a663d226bde5d38e2b4cb62c3abb0d0c1b`. Commands and options do not establish equivalence for other inputs, settings or execution contexts.

## Product boundary

The project targets the current application's audio and file-workflow capabilities in a headless engine, not a recreation of its windows. It does not claim complete feature parity. Pitch scratch alignment, a native-state-dependent MP3 header path, process-sensitive AAC decoding and MP4 timestamps remain bounded or unresolved. Static reference evidence excludes Ogg extensions from its normal input filter despite vendor advertising.

Production processing uses Rust DSP and explicitly configured public native libraries; macOS uses AudioToolbox/Accelerate, while Linux uses portable implementations. It does not call the original application or its proprietary components; those are reference-only and optional. The [build guide](../docs/BUILD.md) and [Linux guide](../docs/LINUX.md) cover libraries, installation and platform boundaries.

### Linux capability and error boundary

By default Linux refuses unequal-rate
sample-rate conversion, artwork conversion that requires Apple's transcoder,
unverified codec and container variants (including unverified GSM masks), and
ALAC packets that AudioToolbox cannot place after its retries and would lose
audio for (the portable writer reproduces the retries and refuses where the macOS
writer's frame check refuses the file; see [ALAC
output](../docs/LINUX.md#alac-output-on-linux-and-portable-builds)). Portable ALAC
output admits only finite 22,050–96,000 Hz rates, 1–8 channels and 16/20/24/32-bit
depths in its supported layout. Processing that requires an unavailable conversion is
refused before publication; analysis of non-MP3 inputs at or below 22,050 Hz is
refused. A non-44.1 kHz waveform is unavailable rather than approximated; an
otherwise supported analysis can still succeed.

MP3 input is not among the refusals: Linux and `portable` builds decode it
(and MPEG Layer I/II streams, which the engine treats as an unrecognized source
codec exactly as on macOS) with the bit-exact port of AudioToolbox's MPEG audio
path, which needs no setting and is an identical path (see
[MP3 input](../docs/LINUX.md#mp3-input)). Which reader opens a file follows
AudioToolbox's `AudioFile`: a name ending `.mp1`, `.mp2`, `.mp3`, `.mpa` or
`.mpeg` (any case) commits to the MPEG reader whatever the bytes are, so a
RIFF/WAVE or AIFF file named `.mp3` is refused as `invalid_input_file` on Linux
exactly as on macOS; under any other name the content decides. One opt-in,
`parity.allow_non_identical` (below), relaxes one refusal on Linux and
`portable` builds, the unchanged copy of embedded cover art, and keeps the WAV
tag chunks that carry source data and the reference marker writer drops. Without the
opt-in Linux behaves as v0.1.0 did except that MP3 input is decoded and AAC input
and output are exact.

AAC input and output are not among the refusals either (v0.4.0). Linux and
`portable` builds read AAC-LC (MP4/M4A, ADTS, LOAS/LATM, CAF) and write
`--format aac` and `match-input` of an AAC source with exact ports of
AudioToolbox's decoder and encoder that need no setting and are identical paths;
the output formats, the answers for every other rate and channel count, the
typed refusals and the diagnostic clock option are specified under
[Files and encoders](#files-and-encoders), the contracts of the ports in
[AAC_INPUT.md](../docs/AAC_INPUT.md) and
[LINUX.md](../docs/LINUX.md#aac-input-and-output).

The Rust codec layer uses `UnsupportedCodec`, `UnsupportedFormat` and
`UnsupportedSampleRateConversion` variants; artwork uses
`ArtworkTranscodeUnavailable`. Linux bridge/FLAC/LAME loads also enforce the
architecture-specific `LibraryPolicyError::Unapproved` digest in
`src/linux_libraries.rs`, plus user-configured FLAC/LAME hashes. Configuring
the digest of an unapproved library does not bypass that policy. CLI per-item
errors serialize a stable machine-readable `code` (see
[Error codes](#error-codes)) with `stage`, a human-readable `message`,
`cancelled`, recovery paths and `partial_publication`; the underlying Rust enum
is not itself exposed. See [LINUX.md](../docs/LINUX.md) for approved identities
and residual x86 floating-point differences.

The loader retains a descriptor and handle for each library identity, checks
the digest after loading and verifies a mapped entry point. Failed post-load
hash or mapped-symbol checks quarantine the descriptor and handle for the
process lifetime and prevent reuse of that identity. This does not
prevent a writer with permission on the same inode from changing it while it is
being loaded; an initializer may already have run before the post-load check.
Use libraries on trusted storage not writable by concurrent adversaries.
Docker Desktop has reported identity mismatches for a bind-mounted bridge;
use in-image libraries and container-volume/tmpfs state.

## Opt-in non-identical paths

The identical-or-refuse rule has one deliberate, explicit exception. The
boolean state-directory setting `parity.allow_non_identical` (default `false`)
is set once, for example `platinum --state-dir DIR config set
parity.allow_non_identical true`; the `process` command line is unchanged.
`config get parity`, `config get parity.allow_non_identical` and
`config set parity '{"allow_non_identical":true}'` address the same value, and
`config reset parity.allow_non_identical` restores the default.

What it enables, on Linux and on `--features portable` builds only:

- **Embedded cover art** (FLAC `PICTURE`, ID3 `APIC`, MP4 `covr`) is stored in
  the output byte for byte where the macOS bridge would re-encode it as PNG
  through Apple's image stack (generic cross-format copies, which the default
  Linux bridge refuses) or silently drop it (FLAC-to-FLAC direct copies).
  Same-family ID3, MP4 and AIFF direct copies already moved the picture
  unchanged and are untouched. With the setting on, a WAV is an ID3-capable
  container like MP3 and AIFF: the pictures of a FLAC, MP3, AIFF, M4A or WAV
  source are stored unchanged as `APIC` frames in the WAV's `ID3 ` chunk by a
  generic cross-format copy (the output lists `artwork_copy` and, because the
  chunk is kept, `wav_tag_preservation`), and the `APIC` frames of a WAV source
  are copied to FLAC, MP3, AIFF and M4A outputs. WAV to WAV and `match-input`
  are direct copies: a WAV's picture there travels with its `ID3 ` chunk through
  `wav_tag_preservation` (the next bullet; see also the WAV tag-chunk paragraph
  in the processed-marker section), and FLAC, MP3, AIFF and M4A same-format
  processing preserves pictures as before. With the setting off, no picture
  moves into or out of a WAV on a cross-format copy (it is ignored, not
  refused), as in the reference. Stored MIME is the canonical type of the
  validated bytes, and the description keeps its text encoding for ID3 sources;
  MP4 `covr` has no type or description. On this generic path,
  pictures read from MP4 become front covers in ID3 and FLAC. The direct
  M4A-to-MP3 path keeps the reference behaviour instead and is not reported as
  `artwork_copy`: its ID3 picture has type 0 and no description, and GIF, BMP
  and WebP bytes are labelled `image/png`, exactly as on macOS. The item is refused with
  `artwork_conversion_unavailable`, before anything is written, when a picture
  is not a structurally complete JPEG, PNG, GIF, BMP or WebP, when MP4 `covr`
  would have to hold WebP, when a FLAC metadata block would exceed
  16 MiB − 1, or when the destination's ID3v2 tag (MP3, AIFF or WAV output: the
  pictures, frame headers, descriptions in the encoding they are written in,
  the tag's other frames, the processed-marker frame and padding) would exceed
  the 268,435,455-byte limit of its 28-bit size field. A picture in a WAV is
  validated like every other destination's: in v0.2.0 a picture inside a WAV's
  ID3 chunk, or one converted into a WAV, was never examined (the WAV was
  excluded from the picture path), so a malformed picture did not matter there;
  since v0.3.0 such a conversion is refused with the setting on, and still
  ignored with it off.
- **WAV tag chunks** (`ID3 `/`id3 ` and INFO `LIST`) that TagLib writes into a
  WAV output and that carry source data are kept, instead of losing the one TagLib
  saved last (see the WAV tag-chunk paragraph under the processed-marker rules).
  The marker rewrite, which stays in Rust, trusts the physical chunk sequence when
  the RIFF size is stale because TagLib's size update is still buffered, but only
  when the chunks before the declared end tile it exactly and everything after the
  declared end is a run of at most 64 well-formed chunks ending exactly at the end
  of the file; anything else after the declared end is
  dropped as before. Within that run a chunk is kept unless it provably carries
  nothing: an `ID3 `/`id3 ` chunk whose ID3v2.2, 2.3 or 2.4 tag holds no frame other
  than `TBPM` frames whose text is empty or `0` and `TKEY` frames whose text is empty
  (zero padding ignored), an INFO `LIST` in which no sub-chunk has a value, and the
  chunks the reference rewrite drops anyway (`PLTN`, a `LIST` shorter than its type)
  are dropped. Any other frame, a non-zero tempo, a non-empty key, a frame or header
  flag, or an ID3 tag that cannot be parsed counts as source data, and its chunk is
  kept exactly as TagLib wrote it; every other chunk of the run is kept as before.
  Every other rule of the rewrite is unchanged, so the output differs from the
  default only by the kept chunks, an output with no kept chunk is byte-identical to
  the default policy's, and an output that kept at least one lists
  `wav_tag_preservation`. TagLib writes tempo and key into every cross-format copy
  into a WAV, so a source without tempo, key and tags yields an `ID3 ` chunk
  holding only a zero tempo and an empty key: the opt-in drops it, the output is the
  default one and nothing is listed. A source with a tempo, a key or tags keeps the
  chunk and lists the path. With the setting off the behavior is the reference's, byte
  for byte.

It never enables sample-rate conversion or any other refused path; those keep
their codes. It does not gate AAC input or output (exact since v0.4.0) or MP3
input, which have no setting: until v0.2.0 the setting also gated an open-source, non-byte-exact MP3
decoder on Linux (items using it listed `mp3_decode`), and v0.3.0 replaced it
by the exact port; a state file that stores the setting remains valid and keeps
its meaning for the two paths above.
On a macOS default build the setting is stored and shown but has **no effect**:
Apple's image stack is always used and every path is the reference path. `config show` states this
per build, next to the stored value:

```json
"parity": {"allow_non_identical": true},
"parity_effect": {"effective": false, "note": "no effect in this macOS default build: …"}
```

`parity_effect.effective` is true only when the setting is on and the build
has the paths (Linux, or `portable`). The same `parity` and `parity_effect`
objects appear in `details.effective_config` of process results. A state file
that never enabled the setting is written without a `parity` member and stays
readable by v0.1.0; v0.1.0 cannot read a file that stores the setting.

`parity.*` is reserved for opt-ins of this kind: paths that exist but cannot
be proven identical to the macOS reference. Future settings that enable
formats the original application does not accept belong to a separate
namespace and never to `parity.*`.

### `non_identical_paths`

Every `process`, `analyze` and `inspect` report carries
`non_identical_paths`, an array of strings naming each non-identical path that
actually produced something in that item's result. It is **always present** on
successful reports on every platform and is `[]` whenever every path used is an
identical one, which is always the case on macOS default builds and on Linux
without the opt-in; `analyze` and `inspect` only decode and read container
properties, which are identical everywhere, so their list is always `[]`. Failed
items have no report and therefore no list. The defined members, in this order
when several apply, are:

| Member | Meaning |
| --- | --- |
| `artwork_copy` | At least one embedded picture was stored byte for byte where the macOS bridge would have transcoded or dropped it (only `process`). It is absent when the source had no artwork or every picture moved through a path identical to macOS. |
| `wav_tag_preservation` | A WAV output kept at least one tag chunk (`ID3 `, INFO `LIST`) that TagLib wrote, that carries source data and that the reference marker writer drops (only `process`, WAV destinations). It is listed for a WAV source whose tag chunks TagLib copied, and for a cross-format copy into a WAV whose source has a tempo, a key or tags (a picture copied into a WAV travels in that chunk and counts as source data). It is absent for a WAV source without tag chunks, for a cross-format copy from a source without tempo, key and tags (TagLib's `ID3 ` chunk then holds only a zero tempo and an empty key and is dropped), and whenever TagLib wrote no chunk after the declared RIFF end. |

New members may be added in later releases; consumers must treat an unknown
member as non-identical and must not require an exact array. v0.2.0 also
defined `mp3_decode` (an open-source MP3 decoder on Linux behind the setting);
MP3 decoding is identical to macOS since v0.3.0 and the member no longer exists;
AAC input and output are identical paths since v0.4.0 and are never listed.

## Opt-in format: Opus

Platinum Notes refuses `.opus` files, so Opus has **no Platinum Notes reference**.
It is therefore a separate opt-in, never part of `parity.*`: the boolean setting
`formats.opus` (default `false`; `config get|set|reset formats.opus`, also
`config get formats`) turns on Ogg Opus (RFC 7845) input and output, and
`libraries.opus` / `libraries.opus_sha256` name the pinned libopus (see
[docs/BUILD.md](../docs/BUILD.md)). A state file that never enabled it has no
`formats` member and stays readable by earlier releases.

With the setting **off** every build behaves as before: discovery ignores `.opus`
(`process` and `analyze` of a selection holding only `.opus` files report
`no_input_files` at stage `discovery`), `inspect` of a named `.opus` reports
`unsupported_input_format`, `.ogg` stays unsupported, no libopus is handed to
any reader (a processed file whose marker names a recorded original ending in
`.opus` cannot make `analyze` decode it: the original's waveform is reported
unavailable), `process --format opus` is refused with `opus_not_enabled`
(usage error, exit 2) and `config set output.format opus` is a `settings_error`.
`config show` carries `formats` and `formats_effect` next to `parity`.

With it **on**, on macOS default, macOS `--features portable`, Linux aarch64 and
Linux x86_64, the same input and settings produce the same output bytes. This
rests on a fixed-point libopus 1.6.1 (no float, no SIMD, no libm), a
known-answer self-test at load time, 24-bit PCM at the library boundary, and an
Ogg layer with a fixed packet and page layout and a content-derived serial
number. Scope:

- Input: a single logical Ogg stream, channel mapping family 0, one or two
  channels, pre-skip, output gain and end trimming honoured exactly, every page
  checksum, sequence number, serial and granule position validated. Chained,
  multiplexed, multistream, mapping-family 1/255 and more-than-two-channel
  streams are refused with `unsupported_opus_variant`; damaged streams with
  `invalid_input_file`, including two cases that would otherwise silently shorten
  the audio: a first audio page that continues a packet from before the stream's
  first page (RFC 7845 permits it; the lost packet would be dropped), and an
  end-of-stream page that ends in a packet that never completes; a valid Ogg
  stream of another codec with
  `unsupported_input_codec`. The comment header is limited to 64 MiB, audio
  packets to 61,440 bytes.
- Processing and output are always at 48 kHz with no sample-rate conversion. A
  source that is processed at another rate (44.1 kHz, 96 kHz, ...) is refused
  with `unsupported_sample_rate_conversion`; sources with more than two channels
  are refused with `unsupported_opus_variant`. The waveform view of `analyze` is
  `waveform_unavailable` for Opus on every build.
- Output bitrate policy (as for AAC and MP3 it is fixed and independent of the
  source's own bitrate): variable bitrate, 128 kbit/s per channel (stereo 256
  kbit/s, the same target as the engine's AAC output; mono 128 kbit/s),
  complexity 10, 20 ms frames, application `audio`, signal `music`, full
  bandwidth, no DTX, no in-band FEC, 24-bit input. Pre-skip is the encoder delay
  (312 samples); the last page carries the exact end granule.
- Tags: user comments are copied verbatim and in order from an Opus or FLAC
  source (FLAC `PICTURE` blocks become `METADATA_BLOCK_PICTURE` comments with
  the same bytes, so artwork is never converted); the old `PLATINUMNOTES` field
  is replaced by the new marker, which is stored exactly as in a FLAC Xiph
  comment (`PLATINUMNOTES=` + the same wrapped Base64 text). The Opus loudness
  tags `R128_TRACK_GAIN` and `R128_ALBUM_GAIN` are the only comments not carried
  over (RFC 7845 section 5.2.1: the gain tags describe the loudness the
  processing has just replaced; the header gain is applied to the audio and the
  output header gain is 0); the names left out are in `opus_output.dropped_tags`.
  Other sources (MP3, MP4, WAV, AIFF, a FLAC with an ID3 prefix) reach Opus
  through the tag bridge's usual seven-key copy and, when they carry artwork,
  need the byte-for-byte artwork copy (`parity.allow_non_identical` on Linux and
  portable builds; macOS default refuses such sources with
  `artwork_conversion_unavailable`). From Opus to another format the same bridge
  copy applies, with the artwork behaviour that FLAC sources already have.

Every `process`, `inspect` and `analyze` report of an item that reads or writes
Opus carries `no_platinum_notes_reference` (a sentence saying so); a `process`
report also carries `opus_output` (`encoder`, `library_sha256`,
`target_bitrate_bps`, `vbr`, `complexity`, `frame_ms`, `dropped_tags`).

## Error codes

Every failure in a JSON outcome carries a stable `code` beside its `message`:

- per-item errors: `results[].error.code` for statuses `failed`,
  `cancelled` and `not_started` (together with the existing `stage`,
  `message`, `cancelled`, `retained_backup`, `recovery_path` and
  `partial_publication` fields where those apply);
- `batch_error.code` when the batch refused to start;
- `error.code` in a top-level `{"status":"error"}` or usage outcome, beside
  `kind` and `message`;
- the failure-shaped objects inside an outcome:
  - `results[].statistics.code` when the statistics accounting is `failed`
    (`statistics_error`) or `not_recorded` (`cancelled`); the audio is already
    published and the item `status` stays `ok`;
  - `code` (`io_error`) in `results[].statistics.cleanup` and in the top-level
    `error.cleanup` of a statistics failure;
  - `code` (`io_error`) in each `results[].error.cleanup_errors[]` entry;
  - `results[].error.rollback_error_code` beside `rollback_error`, which stays a
    string for compatibility; both are `null` when no rollback failed, and the
    code is never `cancelled`;
  - `code` (`io_error`) in `backup_removal_error` of a restore report.

`code` is lowercase snake case and is a public contract: codes are never
renamed or reused, and new causes add new codes, so a consumer must treat an
unknown code as a generic failure. `stage` and `message` stay diagnostic text
and can change. A code is chosen from the error's type when one variant has a
single meaning and otherwise from the stage that failed; a stage label with no
code would be a test failure and the final fallback is `internal_error`.

| Code | Meaning | Reachable |
| --- | --- | --- |
| `unsupported_input_codec` | The input's audio codec cannot be decoded here: an AAC stream that AudioToolbox decodes and the port does not reproduce (HE-AAC whose SBR or PS is signalled, AAC-LD, AAC-ELD, USAC and MPEG-1 audio object types, a CAF whose rate or channel count differs from its cookie, a packet table of more than 16,777,216 packets, a stereo read of a damaged 5.1 stream whose in-band program config element with a matrix mix-down index is not the stream's own 5.1 layout); AAC inside a WAVE container, which AudioToolbox does not open; FLAC data under a name not ending in `flac`, when the name does not select its reader; MP3 inside a WAVE container whose name does not end `.mp3`; `match-input` of an unrecognized source codec (for example MPEG Layer I/II). | Linux/portable; unrecognized codec on all |
| `unsupported_input_format` | The input's container, sample format, bit depth, channel layout or file extension is not admitted (for example OGG, an unverified WAV subtype, multichannel FLAC waveform, an MP3 or AAC file larger than 2 GiB). | all |
| `unsupported_output_codec` | Reserved: the requested output encoding is unavailable. Linux produced it for AAC output until v0.3.0; since v0.4.0 AAC output is exact and no build produces this code. It stays in the table because codes are never renamed, reused or removed. | none (reserved) |
| `unsupported_output_format` | The requested output encoder cannot write this audio's format although the input is fine: ALAC output above 96,000 Hz or with more than 8 channels on Linux, and AAC output at a fractional sample rate or at an integral rate within 10 Hz of an AAC rate but not equal to it on Linux. A format refusal raised in an encoder stage always has this side. | Linux/portable; macOS when an Apple encoder refuses the format |
| `unsupported_sample_rate_conversion` | Processing or analysis needs sample-rate conversion that Linux does not reproduce. | Linux/portable |
| `artwork_conversion_unavailable` | Embedded artwork would need Apple image transcoding on Linux, or (opt-in on) the destination cannot store the picture unchanged. | Linux/portable |
| `protected_source` | The input is a protected `m4p` or `wl.mp3` source. | all |
| `invalid_input_file` | The input is malformed, truncated, empty of audio or otherwise not decodable, or its tag container cannot be read. A name ending `.mp1 .mp2 .mp3 .mpa .mpeg` selects the MPEG reader, so a file with that name whose bytes are not MPEG audio (for example a RIFF/WAVE or AIFF file named `.mp3`) is refused here (`'dta?'`), and data that no reader recognizes under another name is `'typ?'`; this is the same on macOS and Linux. An MP4 whose first sound entry is neither `mp4a` nor `alac` nor a type AudioToolbox opens as another codec fails the open (`'wht?'`) on every build. | all |
| `invalid_input_path` | The input path is a symlink, neither file nor directory, cannot be represented in JSON, or cannot be passed to the tag bridge. | all |
| `no_input_files` | A selected input held no admitted audio files. | all |
| `opus_not_enabled` | Opus output was requested (`--format opus`) while `formats.opus` is off. | all |
| `unsupported_opus_variant` | The Ogg Opus stream or layout is outside what the engine supports: chained or multiplexed stream, channel mapping family other than 0, more than two channels (input or output), a newer OpusHead major version. | all |
| `already_processed` | A processed marker or `_PN` suffix says the file was already processed. | all |
| `marker_ambiguous` | A WAV marker is empty or incomplete and needs `--allow-ambiguous-marker`. | all |
| `marker_unverifiable` | The absence of a processed marker cannot be established (unknown container, duplicate marker chunks, hidden-marker risk, unreadable marker, a WAVE behind a leading ID3 tag: the tag stage refuses it before any reader opens it, on every build). | all |
| `marker_invalid` | A processing marker is malformed, cannot be serialized, or the written marker does not verify. | all |
| `tag_copy_failed` | Copying source tags or writing the marker failed. | all |
| `library_not_configured` | A required library path or SHA-256 pin (tag bridge, libFLAC, LAME) is not configured. | all |
| `library_unapproved` | A library's SHA-256 does not match the configured pin or the compiled Linux allowlist. | all |
| `library_load_failed` | A configured library could not be loaded, whatever the reason: the path does not exist or is a directory, the file is no loadable library, it has the wrong ABI or version, or its identity check failed. Every load stage (tag bridge, libFLAC for decoding and encoding, LAME) maps a failing path or loader the same way; a digest that does not match is `library_unapproved`. | all |
| `decode_failed` | The decoder failed while reading audio. | all |
| `encode_failed` | The encoder or its postflight check failed. On Linux and `portable` this includes AAC output for a rate and channel count that the macOS writer also fails (the 256 kbit/s warning at stage `configure Apple encoder`, AudioToolbox's native status at stage `create Apple encoder`), an AAC stream that needs 64-bit MP4 offsets or duration, and an ALAC packet that AudioToolbox cannot place after its retries (stages `encode Apple output` and `finish Apple output`). | all |
| `processing_failed` | A processing stage rejected the audio parameters (for example an unusable sample rate) or failed internally. | all |
| `io_error` | A filesystem operation failed. Also the code of a failed cleanup, rollback or backup removal, and of a cancellation that left such a failure, a recovery file or a publication behind. | all |
| `atomic_rename_unsupported` | The filesystem or kernel does not support the atomic rename the transaction requires. | all |
| `output_exists` | The destination or its reservation already exists and `--overwrite` was not given. | all |
| `output_conflict` | Two outputs collide, or the destination changed after preflight. | all |
| `source_changed` | The source changed while it was being processed. | all |
| `backup_missing` | No backup is recorded, the recorded backup does not exist, or the `--backup` path supplied to `restore` does not exist. | all |
| `backup_invalid` | The backup is not the verified copy, differs from the recorded one, or its recorded path is invalid. | all |
| `restore_not_applicable` | The file is not processed, needs no restore, or is active. | all |
| `batch_preflight_failed` | The batch was refused before work started; also the status reason of `not_started` items. | all |
| `report_serialization_failed` | The result could not be serialized; audio may already be published (`partial_publication`). | all |
| `cancelled` | The operation was cancelled before or during work and ended cleanly: no recovery file, rollback or cleanup failure, and no publication. Any other outcome of a cancellation keeps its failure code (`io_error` for cleanup and rollback failures) and `cancelled: false`. A statistics accounting that was cancelled after publication reports `cancelled` in `results[].statistics.code`. | all |
| `usage` | The command line or a request is invalid. | all |
| `settings_error` | A settings or template operation failed. | all |
| `statistics_error` | The statistics store failed (also `results[].statistics.code` when the accounting `failed`). | all |
| `signal_handler_failed` | The Ctrl-C handler could not be installed. | all |
| `internal_error` | An unclassified failure; also the fallback for an unknown stage, and a panic inside one item's worker (stage `worker`): that item fails and the other items of the batch are processed and reported as usual. The failed item can leave its staging files behind (`.pn-workflow-*` directories and a `.<name>.pn-reservation` file next to the output), which must be removed by hand before the item is reprocessed (v0.3.0 left the same files when the run ended). | all |

The table, `ErrorCode::ALL` in `src/error_code.rs` and the unit and CLI tests
are kept consistent by a test that fails when a code is added to one and not
the others.

## Commands and capability mapping

| Command family | Required behavior |
| --- | --- |
| `process INPUT...` | File and folder batches; selected built-in/custom template; original conditional processing stages; format/output/backup policy; bounded concurrent jobs; cancellation; truthful per-file results. |
| `analyze INPUT...` | First-pass level and pitch analysis, processed-marker inspection and waveform data. Clip count and dynamics may be reported from an existing marker, not measured as new repair/dynamics results; unavailable values are explicit. No encoded replacement output. |
| `templates list/show/select` | Enumerate built-in and user templates, show effective and serialized settings, select by stable identity, and preserve original fallback precedence. |
| `templates clone/edit/rename/remove` | All editable processing fields, name and description; original built-in mutability rules; persisted custom-template identity. |
| `templates import/export` | Interoperate with the observed `.pntemplate`/XML contract, including missing-field and conversion rules. Unknown fields and lossy conversions must be reported. |
| `inspect FILE...` | Format/rate/channels/bit depth, tags/artwork, processing marker, original/output/backup paths and status. |
| `restore FILE...` | Restore original audio from its recorded backup using an explicit, verified source/destination pair; report partial failure instead of pretending success. |
| `config show/get/set/reset` | Persistent non-account preferences, validation, and explicit effective values. |
| `statistics show`, `statistics reset --confirm` | Engine-owned cumulative processed-song, repaired-peak and rounded-decibel gauges; explicit reset separate from preferences and vendor state. |
| `help`, `version`, source/provenance information | Document supported operations and the reference compatibility target without impersonating the original vendor. |

## Command syntax

`platinum [--state-dir DIRECTORY] [--json] COMMAND ...` uses its own state,
not Platinum Notes' preferences. The global options can appear alongside a
subcommand. `--help`, `COMMAND --help`, `--version`, `version`, and
`provenance` describe the interface and target; no vendor activation or
account is involved. `--state-dir` is the complete state directory, useful for
isolated runs. `HOME` supplies the default output path even with `--state-dir`.
Existing settings and statistics directories must be non-symlink directories
owned by the effective user, with neither group nor world write permission.
Their state and lock files must be owned by that user, singly linked regular
files, also without group/world write permission; symlinks are refused.
On macOS, extended ACLs on these private directories or files are refused
because their effective access cannot be established from mode bits alone.
Directory and file identities are pinned and checked across operations; an
unsafe or replaced object is reported rather than followed or overwritten.

```text
platinum process INPUT... [--jobs N] [--template-index N | --template-id ID]
    [--format wav|aiff|alac|flac|mp3|aac|opus|match-input]
    [--output-dir DIR | --same-folder | --replace-originals --backup-dir DIR]
    [--mirror | --no-mirror] [--mp3-mode cbr320|vbr192]
    [--overwrite] [--allow-ambiguous-marker]
platinum analyze INPUT... [--jobs N] [--template-index N | --template-id ID]
platinum inspect FILE... [--jobs N]
platinum restore FILE... --backup ABSOLUTE_PATH [--backup ABSOLUTE_PATH ...]
platinum templates list
platinum templates show|select|clone|remove (--index N | --id ID)
platinum templates edit (--index N | --id ID) FIELD JSON_VALUE
platinum templates rename (--index N | --id ID) NAME
platinum templates import FILE
platinum templates export (--index N | --id ID) FILE
platinum config show
platinum config get FIELD
platinum config set FIELD JSON_VALUE
platinum config reset [FIELD]
platinum statistics show
platinum statistics reset --confirm
```

Template manager indices are zero-based with built-ins first; IDs are exact,
case-sensitive, and must be unambiguous. `show` reports effective values and
the exact serialized bit representation. Non-finite effective floats are
shown as `"NaN"`, `"Infinity"`, or `"-Infinity"`; their exact bits remain in
the serialized view. `edit` accepts all fields and value types listed below, e.g.
`platinum templates edit --index 3 'expansion.bands[1].threshold' -12.5`.
JSON strings need shell quoting, e.g. `platinum config set output.format '"wav"'`;
booleans, numbers, `null`, arrays and objects are also JSON values.
The output settings, the `parity.allow_non_identical` opt-in, codec library
locations and digests, and selected template are configurable using the field
paths below. Import and export
report ignored fields and losses; export never replaces an existing file.
Template exports stage in a private descriptor-pinned workspace in the destination
directory, publish without replacement, and sync that directory. A failure after
publication reports that the export exists and whether cleanup or sync failed.

Process flags override the loaded settings for that invocation only. The
dedicated default output folder is created if needed; any other selected
`--output-dir` must already exist. `--mirror` and `--no-mirror` apply to a
specific output folder. Replacement always
requires **both** `--replace-originals` and an explicit existing `--backup-dir`
on every invocation, even if replacement is persisted in the configuration.
Replacement forces match-input; a conflicting explicit format is rejected.
Each backup is copied and verified, and its directory is synced before any
source replacement. After publication the output directory is synced before
the displaced original is removed; a sync failure reports the retained
backup and recovery path rather than discarding the displaced file.
New separate outputs also sync the pinned output directory after linking; a
sync failure is reported as published with durability uncertain.
On macOS, metadata verification excludes only the kernel-managed
`com.apple.provenance` extended attribute: a newly created copy receives the
creator's provenance rather than a reproducible copy of the source value.
The attribute is not removed or forged. Other attributes, including
`com.apple.macl` and `com.apple.quarantine`, remain subject to comparison, as
do file bytes, identity, permissions, ACLs and other preserved metadata.
After a metadata copy, differing copyable extended attributes are reapplied
from the source and unexpected attributes removed before strict verification;
any failed reapplication aborts the transaction.
On Linux, copy verification covers bytes, owner/group, mode, access and
modification times, and extended attributes (including POSIX ACL attributes).
Linux birth time and filesystem flags are not copied or compared across
different inodes; when available, they remain part of same-inode identity
checks. Linux atomic swap and no-replace publication require kernel and
filesystem support for the corresponding `renameat2` flags; unsupported
operations fail rather than falling back to a non-atomic replacement.
Separate outputs refuse an existing destination by default. `--overwrite`
explicitly permits atomic replacement of a regular, non-symlink output file;
it is invocation-only and cannot be combined with `--replace-originals`. It
never authorizes overwriting a selected input, a hard link to a selected input,
a directory, a symlink, or another batch destination. Batch destination and
selected-source conflicts are checked before starting workers, including
selected files whose own preflight failed. APFS names use canonical Unicode
decomposition; HFS+/HFSX use the filesystem's Unicode 3.2 decomposition and
excluded ranges. Other and unknown filesystems use byte comparison, applying
case folding when lookup is case-insensitive or sensitivity is indeterminate.
On Linux, `FS_CASEFOLD_FL` absence is treated as case-sensitive only when its
query succeeds; casefolded or unknown directories use Rust Unicode lowercase
mappings without normalization, with ASCII folding for invalid UTF-8 bytes.
Kernel casefold equivalences this approximation misses can still fail at
no-replace or identity-checked publication rather than clobbering output.
Each destination's absent/present identity is captured at batch preflight and
must still match at staging and commit. Existing outputs
are atomically swapped with the completed stage, then the displaced entry is
identity-, metadata-, and SHA-256-checked; an unexpected entry is swapped back
instead of discarded. A rollback's displaced stage is checked against its
pinned identity, metadata, and digest and retained even when unchanged, since a
writer may still hold an open descriptor. Failed process results report its
`error.recovery_path`; the retained file is never automatically deleted.
Each worker uses the exact preflight plan and identity; a source changed after
preflight is rejected rather than inspected and planned again.
Process JSON includes `details.overwrite`,
`details.effective_config.overwrite` and per-item
`report.replaced_existing_output` (false for new output or source replacement).
MP3 staging files are private (0600 subject to umask). New MP3 outputs receive
the mode of an ordinary 0666 creation, including inherited Linux default ACLs;
explicitly overwritten outputs preserve the existing output's owner, group, mode
and effective ACL, or refuse if those permissions cannot be reproduced. Workflow
and CAF staging directories and created stage files are pinned by descriptor,
including when their original directory entry is moved or replaced. If the
platform cannot resolve a safe descriptor/inode alias for a path-only codec,
processing refuses. A partial or replaced stage whose ownership is unproven is
retained and reported. A renamed output parent is detected before publication
and rechecked afterward; errors report its current pinned location when known.
New private workspaces are validated without changing their permissions or ACLs.
On macOS, an inherited extended ACL causes refusal. Initialization failures
clean up a pinned workspace or report any directory that cannot be safely removed.
Reservation cleanup uses a private quarantine allocated before the reservation
exists; it first moves the entry into that quarantine and verifies
its pinned identity; unexpected entries are returned or retained, never deleted.
These checks detect ordinary concurrent writers; a same-user process that
deliberately modifies the private workspace during a transaction can defeat
them.
Workspace creation compares the observed directory identity with the
no-follow opened descriptor but cannot atomically prove the `mkdir` result
against a rename before the first observation. Unsuitable restrictive umasks
can leave an unpinnable directory whose path is reported rather than removing
an unverified entry.

Before reading markers or publishing any file, restore snapshots every target
and backup, freezes each backup's identity and SHA-256, and rejects the whole
request if any target aliases any backup by path or inode. Restore then pairs
each file with the correspondingly ordered explicit `--backup` argument and
verifies it against the marker read from that file. The source identity captured
before marker inspection must still match under the restore reservation.
Marker-only restore requires the tag bridge, not an audio decoder; supplied
backup paths must be absolute. After publication, the restored bytes and
metadata are verified; the restored file receives `F_FULLFSYNC` on macOS or
`fsync` on Linux, and its parent directory is synced before consuming the backup.
Backup consumption
honours the same path reservations as source-replacement jobs. The verified
backup is exclusively moved into a private sibling quarantine, rechecked, then
unlinked. Requests sharing a backup path or inode are restored as a group:
every member must succeed before any authorized backup path is consumed; if
any member fails or cannot be inspected, the backup is retained. The frozen
backup is rechecked before every member and before consumption. Original
processed inodes are retained until the group completes; if a frozen backup
changes mid-group, published members are rolled back where safe, otherwise
the recovery path and rollback failure are reported.

If a sync, identity check, reservation, quarantine, or removal fails,
restoration remains successful and the report contains `backup_removed: false`
plus a structured `backup_removal_error` with `operation`, `path`, `message`,
`retained_path`, and `unexpected_path`. `retained_path` is null if the verified
backup's location is unknown; `unexpected_path` separately identifies an
unverified entry retained after a race. On normal removal success
`backup_removed` is true and `backup_removal_error` is null; a quarantine
directory cleanup warning can accompany `backup_removed: true`.
`details.backup_policy` is `remove_after_verified_restore`.

`--allow-ambiguous-marker` is a process-only, nonpersistent override for one
WAV `PLTN` whose payload is empty or whose canonical Base64 decodes to an
incomplete JSON object. It can reprocess an already-processed file with a
damaged or empty marker; the default remains refusal. The report names each
applied override. Valid processed markers, `_PN` suffixes, duplicate `PLTN`
chunks and suspicious trailing marker bytes remain refused. An ambiguous
override also refuses any bytes beyond the declared RIFF. Analysis, inspection
and restore keep their strict marker rules. This is an explicit user-selected
safety policy, not default admission parity with the original.

The engine sniffs the source container before deciding marker state, even when
the extension-selected tag reader succeeds. Leading ID3v2 headers are validated
and skipped, including their synchsafe lengths and optional footers, before the
following container is classified. Additional nonempty ID3v2 tags are refused
because the bridge does not inspect their marker frames; an ID3-prefixed Ogg
source is also refused. An unknown signature is refused even if a permissive
extension-selected reader accepts it. MP4 detection walks bounded top-level
atoms, validates their declared sizes and a movie child, and requires both
`moov` and `mdat`; leading `free`, `wide` and `skip` atoms are permitted.
An atom name in an MPEG frame alone is not evidence of an MP4 container.
If the direct header does not identify a container, the weaker MPEG scan is
used only for an MP3-selected extension or after a valid ID3 prefix. It scans
at most 64 KiB from the post-prefix position for three complete consecutive
frames with matching version, layer and sample rate; bitrate and padding may
vary. Recognized WAV, AIFF, FLAC and Ogg headers take precedence over
incidental embedded MPEG frame bytes.

When the detected PN-writable family differs from the extension, its marker
is read through the bridge; a read error is a refusal, and a processed or
ambiguous marker follows the normal safeguards. Tag inspection and copying
still use the extension-selected family. If that reader returns ABI-5
source-unavailable status 3, no source tags are copied. A positively identified
Ogg source without an ID3 prefix may continue.

For a matching WAV or AIFF/AIFC whose tag container is unavailable, a bounded
physical chunk walk may establish marker absence. It requires format and audio
chunks, clamps a truncated final audio payload to the file, and refuses partial
headers, truncated non-audio chunks, trailing bytes and unwalkable or excessive
chunks. Any `PLTN`, `ID3 ` or `id3 ` chunk is conservatively refused even with
the ambiguous-marker override. Other matching PN-writable families are refused
on status 3. When admission succeeds without source tags, inspection reports
`tags: null` and `tags_unavailable` while reporting the marker state actually
established; processing reports `source_tags_unavailable` and writes the output
marker. Destination-container, file-open and I/O failures are not treated as
absent tags. Restore still requires an inspectable authorizing marker. Older
ABI-5 bridges report status 3 conditions as generic errors.

AIFF/AIFC marker reads also walk the physically present chunks and refuse more
than one `ID3 ` or `id3 ` chunk, even if the first has no marker. This applies
to inspection, processing and restore; the ambiguous-marker override does not
authorize duplicate AIFF tags.

This physical AIFF-chunk safeguard does not inspect every conflicting marker
within a single ID3 tag or duplicate MP4 freeform items, and FLAC's Xiph
marker takes precedence over an ID3 GEOB. A valid unprocessed first marker
could therefore hide a later processed marker. The leading-ID3 counter covers
adjacent tags; sync-like junk before a tag, or tags separated by junk, could
also hide one. These deliberately crafted scenarios are accepted limitations
from source review; the reviewer did not execute the exact end-to-end examples
or establish that their inputs decode. Do not treat this as proof of equivalent
native admission or as complete processed-marker protection for untrusted,
intentionally modified files.

The WAV marker rewrite creates its exclusive sibling with no broader mode than
the staging file, restores that file's mode after writing, and then renames it;
a restrictive umask cannot silently change the final mode. On failure, cleanup
unlinks only a dev/inode-verified sibling in a non-shared, ACL-checked parent.
Otherwise it does not unlink and reports the cleanup failure and sibling path.

A WAV source's own tag chunks mostly do not survive WAV output, as in the reference.
The marker pass rewrites the WAV while TagLib's stream is still open, so the RIFF
size TagLib wrote last is not yet on disk and the rewrite discards the chunk
TagLib saved last. A source `ID3 `/`id3 ` chunk (text and artwork) is therefore
absent from WAV-to-WAV output unless the source also has an INFO `LIST`; then the
ID3 chunk is kept and the `LIST` is lost. Platinum Notes 10.5.0.1256 produced
identical bytes for generated ID3-only WAV inputs (explicit WAV output;
`match-input` takes the same direct-copy path). Cross-format copies to WAV follow
the same rule: ID3 is kept when an INFO tag is also written, and lost for a
source whose only copied fields are tempo and key. This is parity behavior, not a
defect.

With `parity.allow_non_identical` on (Linux and `portable` builds) the marker
rewrite keeps the chunks TagLib wrote that carry source data: an `ID3 `/`id3 ` chunk
(text and artwork) and an INFO `LIST` both survive WAV-to-WAV, `match-input` and
cross-format copies to WAV, the output is the default one plus those chunks, and
`non_identical_paths` lists `wav_tag_preservation`. A chunk that carries nothing is
dropped as the reference does: TagLib writes `TBPM` and `TKEY` in every cross-format
copy, so for a source without tempo, key and tags the `ID3 ` chunk holds only a zero
tempo and an empty key (then padding), and an INFO `LIST` may hold no value; the
opt-in drops such a chunk, an output in which every trailing chunk is dropped is
byte-identical to the default policy's, and nothing is listed. This is deliberately
not parity with the reference, which is why it sits behind the opt-in.

For a first run, use a disposable copy of an input and an existing separate
output directory. After following the build guide, set `INPUT` and `OUTPUT_DIR`
to your chosen paths; `PN` and `PN_STATE` are the variables from that guide:

```sh
"$PN" --state-dir "$PN_STATE" --json process "$INPUT" \
  --template-index 0 --format wav --output-dir "$OUTPUT_DIR"
```

Replacement changes the source file. Retain an independent backup first, set
`BACKUP_DIR` to an existing backup directory, and use the explicit flags
instead of `--output-dir`:

```sh
"$PN" --state-dir "$PN_STATE" --json process "$INPUT" \
  --replace-originals --backup-dir "$BACKUP_DIR"
```

Read the per-file JSON result and retain its `output_path`, `backup_path` and
`processing_marker_status`; do not guess backup names or assume that a failed
transaction left no published output. When the marker contains the recorded
backup, set `PROCESSED_PATH` and `BACKUP_PATH` to those reported paths and
restore using the corresponding **absolute** backup path:

```sh
"$PN" --state-dir "$PN_STATE" --json restore "$PROCESSED_PATH" \
  --backup "$BACKUP_PATH"
```

Restore replaces the processed file with a verified copy of the backup and,
after verifying the published copy, consumes that backup. Check `backup_removed`
and `backup_removal_error` in the per-file report. A separate-output file or a
nil marker without a recorded backup cannot authorize restore; keep the
independent copy and reported backup for recovery instead.

JSON processing reports expose `processing_marker_status`. `present` means the
stored marker was decoded and checked against the actual processing result.
If Foundation rejects non-finite marker measurements, the native nil behavior
is instead reported as `nil_payload_absent` (ID3, MP4 and FLAC) or
`nil_payload_empty_wav_chunk` (a zero-length WAV `PLTN` chunk). The numerical
measurements remain explicit strings such as `"-Infinity"`, not invented finite
values. A nil result does not record the backup in a readable marker; retain
the reported backup path. Strict marker inspection treats a present empty WAV
payload as malformed because it cannot distinguish that chunk from malformed
input, and marker-authorized restore cannot use a nil marker.

`ProcessReport.encoded_length_frames` is the encoded container's advertised
frame length, distinct from the recorder's `input_frames`, `healed_frames`
and `output_frames`. `source_tags_unavailable` is emitted only when a source
tag container could not be inspected or copied; it is omitted otherwise.
`InspectionReport.tags_unavailable` is likewise omitted when absent, while
`tags` can be null and `processed`/`processing_state` describe the independently
established marker/suffix state. `AnalysisReport.waveform` may be null with
`waveform_unavailable` explaining why; that reason is omitted when absent.
`original_waveform` and `original_waveform_unavailable` describe the recorded
backup's waveform or error, and the latter may be null. The diagnostic-only
`controlled_mp4_time_1904` field is the supplied second or null; it does not
claim the original clock's sampling phase. It is set by the hidden `process`
option `--controlled-mp4-time-1904 SECONDS_SINCE_1904`, which replaces the
creation and modification times in `mvhd`, every `tkhd` and every `mdhd` of an
AAC or ALAC output on every build, the Linux and `portable` AAC output
included.

On the portable path, MP3 postflight inspects the encoded output without
decoding it. For an `Info` tag with a LAME or erased identifier
the packet estimate is `ceil((declared_bytes - tag_frame_size) * 8 /
bitrate_of_the_first_frame_after_the_tag * rate / samples_per_packet)` in the
observed AudioToolbox binary64 operation order: it uses the declared byte
count, not the counted bytes, so variable-bitrate output that keeps an `Info`
tag follows only that one frame's bitrate. A `Xing` tag, and an `Info` tag with
a Lavf or Lavc identifier, use the validated declared packet count. Encoder
delay and padding apply for a LAME identifier or when the tag's CRC-16/ARC
verifies. Thus encoded length is not necessarily the recorder's
submitted-frame count.

Jobs default to `processor_max`, clamped to available cores; `--jobs` is also
clamped to that maximum. Ctrl-C stops starting queued work and signals active
processing/analysis and restore. Cancellation and publication share a commit
boundary: cancellation winning before commit prevents publication, while an
already-entered commit or rollback completes before cancellation takes effect.
An already-running inspection finishes its current operation. JSON outcomes include per-file statuses and real reports or
errors, with transaction recovery details where available. A non-Unicode
command path is represented as an object containing display text and exact
`unix_bytes_hex`. Exit status is 0 for success, 1 for failure or partial
failure, 2 for command/usage errors, and 130 for cancellation. The status of
each file remains visible when outcomes are mixed.

On a render failure or cancellation, the live writer is explicitly aborted.
If writer disposal/abort also fails, its error is appended to the original
error message and the outcome is a failure rather than a clean cancellation.
The transaction reports retained backup/recovery paths and any partial
publication as applicable; callers must inspect those fields.

## Cumulative gauges

Successful CLI processing records one song, its repaired-peak count, and its
binary32 absolute input/output-volume difference rounded to the nearest integer
with ties away from zero. The counters use native signed-64 wrapping additions.
The instruction-derived nontrapping conversion maps NaN to zero and positive
overflow to `i64::MAX`; those edge cases are not observed GUI parity claims.
The original's status-4 error path can increment despite failed publication;
the Engine instead counts only returned successful processing reports.

`statistics.json` and `statistics.lock` live in the Engine state directory,
separate from `settings.json` and all Platinum Notes preferences. An absent file
starts a new baseline; no historical or vendor totals are imported. The first
recorded time is the recording host's Unix-millisecond clock, not necessarily
the first audio-processing time. Explicit reset clears gauges and that date;
configuration reset and audio restore do not change the gauges.

Cooperating processes serialize read/modify/write and reset under the permanent
lock. First publication is no-replace; existing-state updates use identity checks
and atomic rename. Malformed state is preserved and reported, not reset silently.
Settings and statistics stages keep their creation descriptors through commit
and cleanup, and are made owner-readable and writable (0600) before publication.
Cancellation prevents a reset if it wins the shared publication boundary.
After audio is published, accounting is attempted immediately; a cancelled wait
on a contended statistics lock reports that the audio exists but was not counted.
Cleanup failures remain failures rather than clean cancellations.

Audio and statistics are separate commits, not crash-atomic exactly-once
accounting. An accounting error never removes or relabels successful audio.
The per-file result retains its report and a separate statistics outcome; a
post-rename sync error reports published-but-durability-uncertain and must not
be blindly retried.


## Processing controls

Keep the command surface compact. Expose the controls visible in the screenshots, with complete four-band settings in the template model. Preserve internal settings required by the original algorithms and imported templates without inventing a separate switch for every archived control.

- Clipping: enablement, measured-volume gate and threshold percentage. The kernel's headroom remains part of the compatible template/engine state; it is not visible as a separate control in the supplied screenshot.
- Pitch: enablement, cents threshold and both modes: pitch change preserving tempo, or tempo-linked turntable-style change.
- Warmth: no warmth, gentle warmth, hot vacuum tube. Effective option/preset mapping comes from the parser and wrapper, not prose labels alone.
- Expansion and compression independently: enablement and volume gate; four bands with threshold, gain, ratio, attack and release; three crossover midpoints. Additional internal template values stay documented separately.
- Final output: the visible average-level target. Margin and character values are internal compatibility state until their current exposure is established; ten archived labels do not establish ten distinct effects.
- Built-ins: Official, Festival and The Big Boost, interpreted through the actual loading/conversion rules.

The selected analyzer has been recovered statically as K-weighted integrated LUFS-style measurement with 400 ms windows, 100 ms hops, absolute/relative gates and a bank of simulated limiter gains. That is stronger evidence than the older vendor FAQ's “drums only” wording, but it is not a standards-conformance or output-equivalence test. A generic `loudnorm` substitution is not established equivalent. XML values are not necessarily effective internal values.

## Settings and template API

On macOS, `SettingsStore::in_home` uses
`~/Library/Application Support/Platinum Engine/` (not Platinum Notes'
preferences). On Linux, it uses `$XDG_CONFIG_HOME/platinum-engine/` when
`XDG_CONFIG_HOME` is absolute, otherwise `~/.config/platinum-engine/`.
Linux statistics use `$XDG_STATE_HOME/platinum-engine/` when absolute,
otherwise `~/.local/state/platinum-engine/`. An explicit CLI `--state-dir`
places both settings and statistics in that directory on either platform.
`SettingsStore::new` accepts the complete state-directory path plus home and
available-core count for isolated use.
`load` returns a snapshot and `save` publishes that snapshot's settings. Missing
state gets defaults; malformed, incompatible, or internally inconsistent state
is an error. Saved custom templates contain the effective JSON representation,
including exact `0x`-encoded binary32 bits, not native XML. The permanent
`settings.lock` coordinates CLI writers: save checks the original bytes under
the lock and publishes an atomic rename with a fresh revision, so stale
snapshots cannot silently overwrite another cooperating writer. This does not
guarantee exclusion against an external writer that ignores the advisory lock.
`AfterPublish` or `PublishedCleanup` means publication already succeeded but a
later directory sync or temporary-file cleanup failed; the latter reports the
remaining temporary path.

Defaults are MP3, `cbr320`, the available processor count, and the first
built-in template. In contrast to destructive replacement, the default output
is a specific folder, `~/Music/Platinum Engine Output`, without mirroring. The
CLI creates that default folder if needed; the settings API itself does not.
Other selected output directories must already exist. Default MP3 output also
requires the separately configured LAME library and digest. The MP3 and
processor-count defaults are evidence-backed; the separate output path is a
deliberate Engine safety choice.

`config_show`/`config_get`/`config_set`/`config_reset` use these exact paths:

| Path | JSON value / semantics |
| --- | --- |
| `output` | `{"mode":...,"format":...}` using `file_workflow::OutputSettings`. |
| `output.mode` | `"same_folder"`, `{"specific_folder":{"directory":"...","mirror":false}}`, or `{"replace":{"backup_directory":"..."}}`. Choosing replacement forces `match_input`. |
| `output.format` | `"wav"`, `"aiff"`, `"alac"`, `"flac"`, `"mp3"`, `"aac"`, `"opus"` (only while `formats.opus` is on; otherwise a `settings_error`), or `"match_input"`. An incompatible replacement format is rejected. |
| `processor_max` | Integer clamped to `1..=max(1, available_cores)` on set, load, and save. |
| `mp3_mode` | `"cbr320"` or `"vbr192"`. |
| `libraries` | Object with `flac`, `flac_sha256`, `lame`, `lame_sha256`, `tag_bridge`, and (opt-in Opus) `opus` and `opus_sha256`; each is a string or `null`, and `opus` and `opus_sha256` are omitted from the state file while unset. Individual paths such as `libraries.flac_sha256` are also writable. A non-null digest is 64 hex digits; FLAC input/output, LAME output and Opus input/output loading require caller-pinned digests. The ABI-5 tag bridge is required for processing, analysis, inspection and marker-only restore. Paths and digests are caller-supplied; settings alone does not load the libraries or establish their provenance. |
| `selected_template` | `null`, `{"builtin":{"id":"..."}}`, or `{"custom":{"index":0,"id":"..."}}`; custom IDs can be `null` and must match the indexed object. Prefer `select_template` for validated selection. |
| `effective_selected_template` | Read-only resolved selection; even when `selected_template` is `null`, this shows the first built-in's identity. |
| `parity` | `{"allow_non_identical":false}`; the opt-in group described under "Opt-in non-identical paths". It is omitted from the state file while default. |
| `parity.allow_non_identical` | Boolean, default `false`. Writable individually; `config reset parity.allow_non_identical` restores the default. |
| `formats` | `{"opus":false}`; the opt-in format group described under "Opt-in format: Opus". It is omitted from the state file while default; `config show` adds a read-only `formats_effect`. |
| `formats.opus` | Boolean, default `false`. Writable individually; `config reset formats.opus` restores the default. Enables Ogg Opus input and `opus` output; needs `libraries.opus` and `libraries.opus_sha256`. |

On Linux, the FLAC, LAME, libopus and TagLib bridge libraries must additionally match
the exact architecture-specific SHA-256 identities in `src/linux_libraries.rs`.
An unapproved library is refused even if the caller supplies its own matching
digest; the typed policy error names the actual and expected identities. FLAC
decoding uses the same approved, inode-pinned handle as encoding. On both
platforms, codecs and bridge are hashed before and after loading through a
retained file descriptor; the FLAC and LAME configured pins are checked on
that descriptor before `dlopen`. `TagBridge::bridge_sha256()` reports the
observed bridge digest. The loader verifies the resolved entry point's mapped
file identity and caches each verified library handle and descriptor for the
process lifetime, preventing descriptor-name reuse when a dynamic loader
retains a previously closed image. The loader requires `/dev/fd` on macOS or
`/proc/self/fd` on Linux and fails if the descriptor path is unavailable.
The macOS default build retains its caller-selected codec and bridge policy.

Resetting all preferences preserves custom templates; resetting one field uses
its default subject to the same validation. The selected custom index in the
JSON is zero-based *within custom templates*. For template commands,
`TemplateRef::Index` is instead the zero-based manager order (embedded
built-ins followed by imported/duplicated customs). `TemplateRef::Id` is exact
and case-sensitive; if an ID occurs more than once, it fails and an index must
be used. Imported IDs, including duplicates, empty strings, and absent IDs,
are preserved. Selection by manager index persists the intended object even for
those cases. Removing a preceding custom adjusts the selected index; removing
the selected object returns to the first built-in fallback. This index behavior
is intentionally unambiguous where the original manager's ID lookup would
choose the first collision. Built-ins cannot be edited, renamed, or removed;
cloning one creates an editable custom with a fresh UUID. Saving never changes
an existing custom ID.

`edit_template` accepts a JSON string or `null` for `name` and `description`,
booleans for `is_default` and `.enabled`, signed integers for `pitch.threshold_cents`,
`pitch.option`, `exciter_option`, and `output.character`, and finite JSON
numbers for all float fields. Numeric values are effective internal values,
not native XML values or raw bit strings. The writable field names are:

- `name`, `description`, `is_default`, `analysis_volume`;
- `clip.enabled`, `clip.low`, `clip.high`, `clip.percentage`, `clip.headroom`;
- `pitch.enabled`, `pitch.threshold_cents`, `pitch.option`, `exciter_option`;
- `expansion.enabled`, `expansion.low`, `expansion.high`,
  `expansion.target_volume`, `expansion.headroom`;
- `compression.enabled`, `compression.low`, `compression.high`,
  `compression.target_volume`;
- `{expansion,compression}.bands[0..3].{threshold,gain,ratio,attack,release}`
  and `{expansion,compression}.crossovers_hz[0..2]` (substitute each concrete
  name/index);
- `output.limiter_threshold`, `output.margin`, `output.character`,
  `output.limiter_release`.

Identity and the built-in flag are not editable. The `is_default` flag does not
change manager fallback precedence. Clip and expansion
endpoints and both crossover arrays use the native coupled setters in the
order edits are requested; compression endpoints are independent. XML import
returns ignored element/attribute names and prospective native reserialization
losses, and selects the last imported object by index. Export uses the native
copy operation and a fresh UUID, then returns separate copy-difference and
XML-round-trip loss reports. File export creates a new destination atomically
without replacing an existing file. Neither these reports nor CLI settings
establish encoded-file parity.

## Files and encoders

The CLI accepts `wav`, `aiff`, `alac`, `flac`, `mp3`, `aac`, `match-input` and,
while `formats.opus` is on, `opus` output selections. AAC and ALAC use an M4A container. Input
discovery recognizes WAV, AIFF/AIF, MP3, M4A and FLAC (and `.opus` while `formats.opus` is on); protected M4P/WL.MP3
and `.ogg` sources are rejected. The reference's recovered normal extension
filter excludes `.ogg` and `.oga`; on macOS AudioToolbox can decode Ogg content
under a permitted extension, with source tags unavailable if TagLib cannot
interpret that named container. Actual decoding and encoding depend on the
source and host codecs, and these options do not establish byte parity.

AAC output and input. `--format aac`, and `match-input` of an AAC source, write an
`.m4a` of one fixed configuration: 256 kbit/s average bit rate, the only AAC
setting the engine uses. On macOS the engine drives AudioToolbox's
`ExtAudioFile`. On Linux and `portable` builds an exact port of its AAC-LC
encoder writes the same file, byte for byte except for the close-time second in
`mvhd`, every `tkhd` and every `mdhd`, which the hidden diagnostic option
`--controlled-mp4-time-1904` replaces on every build (the Linux AAC output
included) and which is how two files are compared as a whole. The port encodes
44.1 and 48 kHz with 1–8 channels, 32 kHz with 3–8, 24 and 22.05 kHz with 4–8 and
16 kHz with 7–8, and answers every other rate and channel count as the macOS
writer does: a format AudioToolbox takes no 256 kbit/s for fails at stage
`configure Apple encoder` and a format it has no AAC converter for fails at stage
`create Apple encoder` (both `encode_failed`, and nothing is published), and a
fractional rate or an integral rate only within 10 Hz of an AAC rate, which
AudioToolbox serves but encodes at the exact rate, is refused with
`unsupported_output_format`. AAC input is decoded by AudioToolbox on macOS and by
an exact port on Linux and `portable` builds: AAC-LC in MP4/M4A, ADTS, LOAS/LATM
and CAF with the same samples, lengths, bit rates and `OSStatus` values for the
recorded shapes of damaged and unusual files (the rules recorded from
AudioToolbox, and the damaged shapes that are not reproduced and may give a
different result or status, are listed in
[AAC_INPUT.md](../docs/AAC_INPUT.md)). On Linux and `portable`, HE-AAC whose SBR
or PS is signalled, AAC-LD, AAC-ELD, USAC and MPEG-1 audio object types, a CAF
whose rate or channel count differs from its cookie, a stereo read of a damaged
5.1 stream whose in-band program config element with a matrix mix-down index is
not the stream's own 5.1 layout, and AAC inside a WAVE
container (which AudioToolbox does not open) are refused with
`unsupported_input_codec`, a client rate other than the file's with
`unsupported_sample_rate_conversion` and a file above 2 GiB with
`unsupported_input_format`. Input discovery still admits an AAC file by its
`.m4a` extension and the tag stage must classify the container. Neither
direction appears in `non_identical_paths`.

With `PLATINUM_DIAGNOSTIC_JOB_TRACE`, macOS records loaded-image callbacks
with Mach-O UUIDs. Linux snapshots loaded ELF images at initialization and at
each job boundary, recording a GNU build ID when present. This snapshot is not
a loader callback: images loaded and unloaded between snapshots can be missed.
The x86-64 DSP flush scope enables both MXCSR flush-to-zero and
denormals-are-zero; its prior bits are restored afterward. ARM and x86 differ
in when they detect underflow, so boundary values are not claimed to be
cross-architecture byte-identical.

`analyze` obtains waveform data in a separate read pass requesting 44.1 kHz
interleaved float32. Each nonempty read of at most 512 frames produces one
pair of sequential float32 mean-absolute amplitudes, using the last two
channels or duplicating mono. A short read remains its own bin; it is not
zero-padded. JSON reports both the requested rate and the decoder's actual
rate, frame positions and actual bin lengths. The original FLAC rate setter
is a no-op, so FLAC waveform data retains its source rate. Analysis of FLAC
with more than two channels currently fails closed: the native interleaved
path can leave part of the returned buffer uninitialized, and deterministic
waveform values for that suffix have not been established. No analysis
report is returned for that unsupported path.
On Linux and in the macOS `portable` feature, a non-MP3 source at or below
22,050 Hz is refused because the reference processing-rate conversion has not
been reproduced. For other non-44.1 kHz sources, analysis can proceed but the
primary waveform is `null` with `waveform_unavailable` explaining the missing
conversion. An MP3 whose rate is not 22,050, 32,000, 44,100 or 48,000 Hz needs
the processing-rate conversion and is refused the same way (`inspect` still
reports it). Output sample-rate conversion fails before publication, and so do
the AAC refusals listed above. Portable FLAC and MP3 output can use caller-pinned
encoders; sampled patched Linux builds are documented separately and are not
a proof of whole-pipeline parity. MP3 output is inspected without decoding it;
the postflight validates MPEG frame structure and computes the
gapless length using the observed AudioToolbox reporting rules. Portable pitch
and warmth no longer have their provisional refusals; unsupported FFT
shapes still fail when the portable setup cannot be created. The verified
vector and scalar component comparisons do not by themselves establish
whole-file or whole-graph parity.

For a processed file, the primary waveform describes that file. An original
waveform is available only when a readable backup is explicitly identified
by its processing marker; otherwise its unavailability is reported. Native
component captures verify the reducer for three input sequences, not every
decoder, resampler, file or GUI cache behavior.
A separate four-case generated PCM16 WAV comparison at 44.1/48 kHz (mono/stereo)
matched every native decoder float byte and read boundary; the release CLI's
resulting bins also match.

MP3 output exposes `cbr320` and `vbr192`. The implementation derives bit depth,
sample-rate adaptation and channel handling from the source and selected
format, and transfers tags and artwork. Those paths have bounded comparisons,
not a complete original-file parity claim.
MP3 conversion accepts 1–64 source channels through the source-to-stereo PCM16
converter; LAME receives mono for a mono source and stereo otherwise. A
generated six-channel 24-bit WAV matched both pinned native MP3 captures.
After macOS ALAC finalization, the private output is decoded and its valid
frame count checked against submitted frames before publication. Valid empty
ALAC containers remain accepted; a silently truncated stream is refused.
On Linux and `portable` builds the portable ALAC writer reproduces AudioToolbox's
handling of a packet larger than its converter's output buffer: the packet is
encoded again at later write calls and every attempt is counted in the cookie
fields `maxFrameBytes` and `avgBitRate`, and where the macOS check would refuse
the file the item fails with `encode_failed`. For write sizes that do not divide
4,096 frames (the engine's own write calls are of that kind unless the limiter
latency is a multiple of 512: the first call of a span is 512 frames minus that
latency modulo 512 and the last is cut to the file's length) it can refuse a
file AudioToolbox writes; it never writes a different one.

Destination policy covers a selected directory, the input directory, mirrored input folders and replacement mode. Replacement forces match-input, retains the configured backup workflow, and exposes restoration. Ordinary outputs use the observed `_PN` naming convention. Already-processed tags and suffix checks must not disappear in the CLI.

## Safety and truthful outcomes

The reference has potentially destructive output-collision and replacement/error behavior. Those observations must be recorded rather than silently copied into a new default:

- Refuse accidental output clobber by default; destructive replacement must be explicit.
- Preserve backups before replacing originals and report rollback failures.
- Do not report success solely because a late reference status flag says processed if an earlier move/replace operation failed.
- Keep synthetic oracle files and user music separate. Verification never processes a real library by default.
- No vendor activation bypass, credential copying, automatic telemetry or hidden remote side effects.

These are explicit proposed safety differences, not claims of byte-for-byte replication of every original defect.

## Verification boundary

Acceptance requires byte-identical complete output files for the same source bytes and effective settings, checked with SHA-256 against Platinum Notes 10.5.0.1256. The encoded audio, container structure, encoder headers and metadata all count. No perceptual tolerance, PCM-only comparison, normalized-tag comparison, or generic replacement satisfies this requirement. Pin the reference build, architecture, codec/runtime versions, paths and metadata-relevant state for each case.

The first pinned generated Official/WAV case satisfied that criterion:
original A and B and corrected Rust debug/release output were identical in
all 3,076,786 bytes. This does not extend to arbitrary files or settings.

Reference comparisons must cover silence, short clips, representative unclipped/clipped synthetic signals, each mode, parameter edges, state changes, channels, supported sample rates, block partitioning, end-of-stream delay compensation and long-stream counters. Encoders and file workflows require independent metadata, path, collision, permission, backup, cancellation and error tests.

Before asserting this gate is reproducible, process the identical source repeatedly in the original under the same recorded state. If its complete-file hashes differ, preserve the outputs and identify the differing bytes and cause. Report that conflict explicitly; do not silently weaken the requested acceptance criterion. Per-stage PCM hashes and semantic tag comparisons are diagnostic tools, not alternate pass criteria.

A completed Rust implementation must not hide an unrecovered stage behind a no-op, a generic replacement, or a call into the original proprietary binary.

## Vendor-connected and presentation features

The current Personalize screenshots show only Output Location, Output File Format and Processor Load. Archived Audio Player, Privacy, License and Debug/checkpoint controls remain research evidence, not mandatory new CLI commands. File handoff, restore and other non-tab workflows still need their active code paths and relevant menus accounted for. The supplied screenshots leave the template selector/gear menus and lower multiband scroll regions unobserved; those are explicit coverage limits, not proof of additional features or permission to drop existing ones.
