# Linux builds and runtime

Platinum Engine v0.4.0 targets Linux aarch64 and x86_64 as well as the macOS
arm64 reference. The default objective for admitted Linux operations is an
identical complete output file, including encoded samples, container, tags and
marker; known unsupported operations are refused or reported unavailable
instead of being approximated. The evidence is finite and there are known x86
arithmetic residuals, so this is not a guarantee that every mismatch can be
detected at runtime. Linux aarch64 is the stricter parity build. One explicit
opt-in, [described below](#opt-in-cover-art-and-wav-tags), allows two paths
that cannot be proven identical. A second, separate opt-in adds Opus, which
Platinum Notes does not accept and which therefore has [no reference to
match](#opt-in-opus-input-and-output). MP3 input and AAC input and output are
exact ports and need no setting ([MP3 input](#mp3-input), [AAC input and
output](#aac-input-and-output)). If you use v0.3.0 or earlier, read [Upgrading
from v0.3.0](#upgrading-from-v030) and [Upgrading from
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

- AAC is no longer refused as such: AAC input and AAC output are exact ports
  that need no setting (see [AAC input and output](#aac-input-and-output)).
  Refused or answered as macOS answers within AAC: HE-AAC (SBR/PS), AAC-LD/ELD,
  USAC and MPEG-1 audio object types, sample-rate conversion of an AAC source,
  AAC inside a WAVE container, and the output rates and channel counts listed
  in that section. MP3 input is decoded exactly (see [MP3 input](#mp3-input));
  only MP3 inside a WAVE container (unless the name ends in `.mp3`) and rates
  that need conversion are refused. MP3 and FLAC *encoding* use the pinned
  public LAME and libFLAC builds described below.
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
  error), and the writer refuses, where the macOS writer's frame check does, a
  packet that AudioToolbox cannot place after its retries and would lose audio
  for (see [ALAC output](#alac-output-on-linux-and-portable-builds)).
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
  refused with `unsupported_input_codec`. FLAC data under a name that does not
  select its reader and Ogg audio are typed refusals
  (`unsupported_input_codec` / `unsupported_input_format`) because AudioToolbox
  decodes them and this build does not; AAC data under such a name is read by
  the [AAC reader](#aac-input-and-output) (an AAC file named `.mp3` is read by
  the MPEG reader, as on macOS). Layer II at 22,050 Hz is refused as
  `unsupported_sample_rate_conversion` (macOS fails it mid-read with
  `decode_failed`). The remaining differences from the macOS default build that
  the admission matrix found are listed in [VERIFICATION.md](VERIFICATION.md).
- Apple never returns from opening a file with a chained ID3 header whose
  version byte is 0xff; this port refuses it with `'dta?'`.

## AAC input and output

AAC needs no setting and no library of its own. Linux and `--features portable`
builds read and write it with `src/portable/aac/`, pure-Rust ports of what
AudioToolbox's AAC-LC decoder and encoder do for the engine's own call sequence
(reference: macOS 27.2 build 26B123 on one Apple M4 machine). The macOS default
build uses AudioToolbox itself. Both ports are claimed identical within the
bounds below and either refuse or answer as macOS answers everything outside
them; neither adds a member to `report.non_identical_paths`. Their numeric
constants and algorithms were recovered from Apple's binaries on that machine;
provenance, and a note that AAC is a patented format, are in
[THIRD_PARTY_NOTICES](../THIRD_PARTY_NOTICES).

### AAC output

`--format aac`, and `match-input` of an AAC source, write the file that
AudioToolbox's `ExtAudioFile` writes for the engine's call sequence: an `m4af`
file with the `'aac '` file format, a float32 client format, `'brat'` 256000,
`'cdqu'` 0x40 and `'acbf'` 1 (average bit rate), in the call context the v0.2.0
release pinned on macOS (write class A: stack-pointer residue 0 at
`ExtAudioFileWrite`, the generic NEON tail at dispose). The port encodes
these formats, at their exact sample rates:

| Sample rate | Channels |
| --- | --- |
| 44,100 and 48,000 Hz | 1–8 |
| 32,000 Hz | 3–8 |
| 24,000 and 22,050 Hz | 4–8 |
| 16,000 Hz | 7–8 |

For these formats the `.m4a` equals the macOS default build's output byte for
byte, with the one exception both builds share: `mvhd`, every `tkhd` and every
`mdhd` carry the wall-clock second of the close, so two runs differ in those
fields. The hidden `--controlled-mp4-time-1904` diagnostic option pins that
second on every build, the Linux AAC output included, and is how two files are
compared as a whole. The container is reproduced too: the `moov` atom in front
of the media data, or behind it once the sample tables outgrow the space
reserved in front (at 13,509 access units at 44.1 kHz and 14,541 at 48 kHz, a
little over five minutes), the chunking, `iTunSMPB`, the peak bit rate in
`esds` and the layout of an empty file. Every write size gives the same file.
The tags and the processing marker are written into the `.m4a` by the tag
bridge exactly as for ALAC output.

Every other format is answered as the macOS writer answers it, or refused:

| Request | Writer answer | Item result |
| --- | --- | --- |
| 8, 11.025 and 12 kHz with 1–8 channels, 16 kHz with 1–6, 22.05 and 24 kHz with 1–3, 32 kHz with 1–2 (AudioToolbox takes no 256 kbit/s) | created with the warning `AacBitRateNotSet` (status `'!dat'`); a first write of a frame would fail with that status | fails before any write at stage `configure Apple encoder` with `encode_failed` |
| no AAC converter: a rate not within 10 Hz of one of the nine AAC rates (for example 64, 88.2 or 96 kHz), or more than 8 channels | AudioToolbox's native status: `'fmt?'` (`paramErr` above 1,024 channels and for exactly 3,000,000 Hz; `'fmt?'` from the create call above 3,000,000 Hz) | fails at stage `create Apple encoder` with `encode_failed` |
| a fractional rate, or an integral rate within 10 Hz of an AAC rate but not equal to it | typed refusal: AudioToolbox serves the request as the AAC rate but its access units follow the exact rate, which this build does not reproduce | `unsupported_output_format` |
| a stream that needs 64-bit offsets (media data above 4 GiB) or a duration above 2^32 − 1 frames | refused when the stream grows past the limit | `encode_failed` |

Other bit rates, the converter's own default bit rate, VBR and CBR modes,
HE-AAC and AAC-ELD encoders are not reproduced; the engine never asks for them.
The encoder runs with flush-to-zero cleared whatever mode the caller is in, as
AudioToolbox does, with one exception that AudioToolbox shows and the port
reproduces: when the caller's flush-to-zero mode is set while samples are
written, subnormal input samples are flushed to zero. The engine writes outside
its flush scopes, so this never applies to its own output; what the port does not
reproduce of that mode is described under [Floating-point and reference
limits](#floating-point-and-reference-limits).

### AAC input

AAC-LC in MP4/M4A, ADTS, LOAS/LATM and CAF is decoded as AudioToolbox's
`ExtAudioFileOpenURL` and `ExtAudioFileRead` decode it: the same float32
samples (interleaved, and the planar stereo read the engine's analysis uses,
with AudioToolbox's mix-down of multichannel sources; the mix-down of a 5.1
stream follows the `matrix_mixdown_idx` of the last in-band program config
element when there is one, see the refusals below), the same `file_length_frames`, `bitrate` and
`source_format`, the same padding behind a short read and the same `OSStatus`
for a file that fails to open or to read. How AudioToolbox treats damaged and
unusual files was recorded (atom and size fields, sample tables and sample
groups, movie fragments, LOAS frames that change or lose their configuration,
`ftyp` brands, `iTunSMPB` and `udta`, CAF chunks, packet tables, layered
descriptions and `desc` fields) and is reproduced rule by rule for the shapes
recorded, each rule pinned by recorded inputs; a damaged shape that was not
recorded, or that AAC_INPUT.md lists under "Not reproduced", may give a
different result or status. The rules are listed in
AAC_INPUT.md and not repeated here. Which reader opens a file follows
`AudioFile`, as for [MP3](#mp3-input). The complete contract, the container
rules and the refusals are in [AAC_INPUT.md](AAC_INPUT.md); the typed refusals,
all reported with `unsupported_input_codec` unless stated, are:

| Input | Refusal |
| --- | --- |
| HE-AAC v1/v2 whose SBR or PS is signalled in the AudioSpecificConfig or by the container (an HE-AAC stream that signals neither is decoded as its AAC-LC core, as AudioToolbox does) | `unsupported_input_codec` |
| AAC-LD, AAC-ELD, USAC, MPEG-1 audio object types in an AAC file | `unsupported_input_codec` |
| a CAF whose rate or channel count differs from its cookie's | `unsupported_input_codec` |
| AAC inside a WAVE container (format tag 0xff): AudioToolbox does not open such a file (`'fmt?'`), and the port refuses it with the typed error instead of reproducing that status | `unsupported_input_codec` |
| a stereo read (the planar read of the engine's analysis) of a 5.1 stream, once an access unit carries an in-band program config element with a `matrix_mixdown_idx` that is not well formed (its element lists are not exactly front SCE + CPE, back CPE and one LFE, so it is not the stream's own 5.1 layout; damaged streams, for which AudioToolbox's mix-down was seen to ignore the element or to follow neither matrix); the interleaved read is not affected | `unsupported_input_codec` |
| a packet table of more than 16,777,216 packets | `unsupported_input_codec` |
| a client rate other than the file's (sample-rate conversion; analysis of an AAC source at or below 22,050 Hz is refused for this reason) | `unsupported_sample_rate_conversion` |
| input above 2 GiB (the file is decoded from memory) | `unsupported_input_format` |

As for every non-MP3 source, the primary waveform of an AAC source that is not
44.1 kHz is reported unavailable (the engine asks for 44.1 kHz and the port
does not convert); the rest of the analysis can succeed. Through the CLI an
input is admitted by its extension (`.m4a` for AAC) and its tag stage must
classify the container, which is narrower than what the reader accepts: a CAF
is refused by the tag stage on every build.

### Speed, memory and determinism

In release builds the encoder runs about 20 to 30 times faster than real time per
core on an arm64 laptop (measured with the long fixture items and a larger set of
generated inputs); no figure is claimed for x86_64 or for the decoder. The
encoder spools its access units to the private staging workspace as they appear
(a write of any size is encoded and drained a slice at a time), so memory stays
bounded however long the stream is. The spool and the staged output live in the
destination's directory and exist side by side while the container is written,
so that filesystem needs room for about twice the size of the stream; finishing
copies the spool into the container and cannot be cancelled in portable builds.
The decoder reads its input whole (up to 2 GiB); its memory and work are bounded
by the input as set out in [AAC_INPUT.md](AAC_INPUT.md).

Both ports use only the Apple-exact arithmetic of the portable build (explicit
fused multiply-adds, correctly rounded libm replacements, the generic DFT), have
no host-dependent branch and compute with flush-to-zero cleared. Every finite
result of the decoder is the same on aarch64 and x86_64; for the adversarial
streams whose spectrum overflows to infinity (a resonant TNS filter) the NaNs
that follow take the sign bit of the host's arithmetic, so non-finite output bits
can differ between architectures. The encoder and decoder gave the same output on
macOS arm64, Linux aarch64 and Linux x86_64 on the corpora described in
[VERIFICATION.md](VERIFICATION.md#v040-changes-and-evidence). The x86 residual
classes of [Floating-point and reference limits](#floating-point-and-reference-limits)
are not claimed absent for the rest of the processing chain between decoding
and encoding.

### What is not reproduced

Apple's AAC encoder and decoder select a 512-point DFT kernel from the stack
position at the call: generic NEON code, or an SME kernel. The port reproduces
the generic classes only, the ones the engine reaches by construction on macOS
(write class A, tail class generic, read class G, all at stack residue 0
through the trampoline described in [BUILD.md](BUILD.md)). The SME variants
(the encoder's write class B and SME tail, the decoder's class S) are not
reproduced. Also not reproduced: HE-AAC (SBR, PS), AAC-LD/ELD, USAC, dynamic
range control, sample-rate conversion, other encoder settings, and other Apple
Silicon generations or OS builds than the recorded reference.

## ALAC output on Linux and `portable` builds

The portable ALAC writer (`src/portable/audio/alac.rs`, `mp4.rs`) writes what
AudioToolbox's `ExtAudioFile` writes, packet by packet, within the limits listed
under [What is refused](#what-is-refused). v0.4.0 corrects two differences from
AudioToolbox that v0.3.0 shipped, found while comparing AAC and ALAC outputs on
the same inputs; the macOS default build uses AudioToolbox and is unchanged.

- **Final packets of fewer than 288 frames.** AudioToolbox scores a packet from
  the first eighth of its frames and trains its predictors on the first
  thirty-second, in four work buffers it keeps for the whole stream. For a
  stereo or multichannel packet of fewer than 288 frames the span it scores
  contains positions that the last training pass of the predictor order being
  tried does not rewrite. They hold that pass's differential head and, beyond
  it, what the element's own mix search wrote, never anything from an earlier
  element or packet (pre-filling the buffers with other data changes nothing).
  v0.3.0 modelled that span by hand
  and got it wrong below 288 frames (a mono packet and a packet of 288 frames or
  more were never affected), which for some content changed a predictor-order or
  compression choice and with it the last packet: in a random sweep of 30,000
  generated files, 1,022 of 48,270 packets of the earlier code differed from
  AudioToolbox's, every one a final packet of fewer than 288 frames. Cases found
  first were a 5-channel 44.1 kHz file and a 96 kHz stereo file. The writer now
  keeps the buffers as AudioToolbox does. More than 1.67 million packets were
  compared with AudioToolbox without a byte difference (random sweeps of 1–8
  channels, 16/20/24/32-bit depths, rates from 8 to 192 kHz and 14 kinds of
  content, every final-packet size from 1 to 330 frames, and selected formats
  up to 4,095), and `tests/portable_alac_packets.rs` replays 85 recorded packet
  streams without an Apple component (the v0.3.0 writer fails it).
- **Oversized packets and the cookie.** AudioToolbox does not write a packet
  larger than its converter's output buffer (the larger of 64 KiB and
  4,096 × channels × ceil(bit depth / 8) + 8 bytes). An all-escape packet of
  incompressible audio can exceed it by 1 to 8 bytes, which near-full-scale noise
  in 4 or more channels at 32 bits, 6 or more at 24 bits or 8 at 16 bits does, and
  so can input laden with NaN or infinity. AudioToolbox keeps the audio queued and
  encodes the packet again, once per later `ExtAudioFileWrite` call and once at
  close, with the predictor coefficients the failed attempt left behind. The
  cookie fields `maxFrameBytes` and `avgBitRate` are its tally over every attempt,
  rejected ones included. A call that still cannot place the packet at its third
  or a later attempt has its audio discarded, and a packet not placed by the close
  is dropped with everything behind it; the macOS writer's frame check then
  refuses the file. The portable writer reproduces this: the packets and the
  cookie equal the macOS file's, and it refuses (`encode_failed`, at the stage
  `encode Apple output` or `finish Apple output`) where the macOS writer ends in
  that refusal. v0.3.0 made two attempts in total and tallied only the packets
  it wrote, so for such files its cookie fields differed, it refused files
  AudioToolbox writes and it wrote files the macOS check refuses (on 8,000
  generated cases 2,039 files differed and 98 were refused where AudioToolbox
  wrote; the corrected writer has neither).

Evidence for the second item: the default build's `apple_codec::Writer` against
the portable `Writer`, fed the same float blocks, whole files compared with the
six MP4 clock fields zeroed, over 1–8 channels, 16/24/32-bit depths, rates from
22.05 to 96 kHz, lengths up to 17,000 frames, content around the level at which
the first attempt overflows and a quarter of the cases laden with NaN and
infinity. With 512-frame writes (the engine's block size; the first and last
write of an engine run are shorter), 48,000 cases gave 46,607 identical files
and 1,393 refusals on both sides, and none that differed. The first write of an
engine span is 512 frames minus the limiter latency modulo 512 and its last is
cut to the file's length, so unless the latency is a multiple of 512 its packet
boundaries fall inside write calls and a real run belongs to the class of write
sizes that do not divide 4,096 frames, which is covered next.
With other write sizes (64 to 4,096 frames dividing
4,096; 4,096; 100; mixed sizes up to 9,000), 18,000 further cases gave no
differing file and no file written where AudioToolbox's writer refuses.
`tests/portable_alac_packets.rs` pins 48 oversized-packet cases (whole-file
digests, cookie fields or the kind of refusal) and
`tests/portable_alac_digests.rs` pins the whole-file digests of 18 ordinary
configurations to what the v0.3.0 writer produced, so files without these two
conditions are unchanged.

Limits. The retry schedule is exact for write calls of one size that divides
4,096 frames (512-frame calls included). The engine's own calls are not all of
that kind (above), so for engine runs, as for any caller whose write sizes do
not divide 4,096, AudioToolbox can host more attempts than modelled: the
portable writer can refuse a file AudioToolbox would write (29 of 9,000
generated cases of mixed-size and 100-frame writes; the engine's own pattern
was not swept separately, so no rate is claimed for it) but never writes a
different one. Only a packet that is still too large after two attempts is
affected at all. The
recordings and sweeps are of one macOS 27.2 arm64 encoder; another ALAC encoder
version could change them. The NaN and infinity cases matter only for robustness:
the engine's own DSP output is finite.

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
  output and that carry source data are kept. macOS and the original application
  lose the chunk TagLib saves last (the `ID3 ` chunk, or the `LIST` when both exist)
  because their marker rewrite runs while TagLib's RIFF size update is still
  buffered; with the setting on, a WAV source's tags and artwork survive
  WAV-to-WAV and `match-input`, and the output is the default output plus those
  chunks. TagLib writes tempo and key into every cross-format copy into a WAV.
  For a source without a tempo, a key or tags that chunk holds only a zero
  tempo and an empty key, which is nothing the source had: it is dropped (v0.3.0
  kept it), the output is the default one and the item lists nothing. A source with a tempo,
  a key or tags keeps the chunk (and the INFO `LIST` for tags) and the item lists
  `wav_tag_preservation`. An INFO `LIST` without a value is dropped the same way;
  a tag chunk that cannot be proven empty is kept as TagLib wrote it. The rewrite
  applies only to a WAV whose chunks before the declared RIFF end tile it exactly
  and whose bytes after it are a short run of well-formed chunks reaching the end
  of the file; any other file gets the default output. Without the
  setting the original behavior is kept byte for byte.

Each item that took one of these paths says so: `report.non_identical_paths` is
`["artwork_copy"]`, `["wav_tag_preservation"]` or both (always present, `[]`
otherwise). Without the setting Linux refuses what needs the transcoder and
drops those chunks, as in v0.1.0. The setting does not gate MP3 input (it did
in v0.2.0) or AAC input and output, which are exact and need no setting. On a
macOS default build the setting is stored but has no effect, and `config show`
says so. The setting and the item list are specified in
[CLI_CONTRACT.md](../research/CLI_CONTRACT.md#opt-in-non-identical-paths).

## Upgrading from v0.2.0

- **Libraries.** Library paths and digests live in the state directory. The
  Linux tag bridge since v0.3.0 (v09, still ABI 5) has a new SHA-256 (see
  [Linux native libraries](#linux-native-libraries)) and the v0.4.0 CLI refuses
  the v0.2.0 bridge, so `libraries.tag_bridge` must be repointed at the bridge
  of the new release. libFLAC and LAME are byte-identical to v0.2.0 and keep
  their pins; libopus is new and needed only for Opus. Unpack v0.4.0 into a new
  directory next to v0.2.0.
- **State file.** A state file that stores `parity.allow_non_identical` stays
  valid and keeps its meaning for cover art and WAV tag chunks. A file written
  by v0.3.0 or later that stores `formats` or `libraries.opus*` (only after you set
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

## Upgrading from v0.3.0

- **Libraries.** The Linux tag bridge, libFLAC, LAME and libopus pins of v0.4.0
  are unchanged since v0.3.0 (bridge `5c52c5f1…` aarch64 / `8c61f6bb…` x86_64, libFLAC `41b59a45…` aarch64 / `9420c96f…` x86_64, LAME `c780062f…` aarch64 / `cf59ae7f…` x86_64, libopus `c09146de…` aarch64 / `d09791af…` x86_64); the CLI refuses a library that is not on its
  allowlist, as before. Unpack v0.4.0 into a new directory next to v0.3.0.
- **State file.** Unchanged and valid; nothing new is stored.
- **AAC input.** v0.3.0 refused AAC input on Linux and `portable` with
  `unsupported_input_codec`. v0.4.0 decodes it exactly; an AAC file is refused
  with that code only for the variants listed under [AAC
  input](#aac-input), a damaged one fails as macOS fails it for the shapes
  recorded (`invalid_input_file` at open, `decode_failed` while reading), and a
  hand-built stub that v0.3.0 refused as an unsupported codec can now be
  `invalid_input_file`. An MP4 whose first sound entry is neither `mp4a` nor
  `alac` nor a type AudioToolbox opens as another codec is now
  `invalid_input_file` (`'wht?'`, as on macOS); v0.3.0 answered
  `unsupported_input_format`.
- **AAC output.** v0.3.0 refused `--format aac` (and `match-input` of an AAC
  source) with `unsupported_output_codec`. v0.4.0 publishes the `.m4a`, or fails
  the item as listed under [AAC output](#aac-output). `unsupported_output_codec`
  is kept in the table as reserved and no build produces it.
- **WAV tag chunks.** With `parity.allow_non_identical` on, a WAV output whose
  source has no tempo, key or tags no longer keeps TagLib's empty `ID3 ` chunk
  and no longer lists `wav_tag_preservation`; its bytes equal the default
  output's. An INFO `LIST` without a value is dropped the same way. Sources
  with a tempo, a key or tags are unchanged, and with the setting off nothing
  changed. Consumers that expected the member for every cross-format copy into
  a WAV should stop doing so.
- **ALAC output.** Portable ALAC files can differ from v0.3.0's in two cases in
  which v0.3.0 differed from AudioToolbox: the last packet of a stereo or
  multichannel stream of fewer than 288 frames, and a stream with a packet
  larger than AudioToolbox's output buffer (near-full-scale incompressible audio
  in many channels at high bit depth; the cookie fields `maxFrameBytes` and
  `avgBitRate` change as well). Of the second kind, a stream v0.3.0 wrote can now
  be refused (`encode_failed`) where the macOS writer refuses it, and one v0.3.0
  refused can now be written where AudioToolbox writes it. Files without these
  conditions are unchanged (pinned by test). See [ALAC
  output](#alac-output-on-linux-and-portable-builds).
- **Worker panics.** On every build a panic in the worker of one batch item
  fails that item (`internal_error`, stage `worker`) and the rest of the batch
  is processed and reported; in v0.3.0 it ended the run. The panicking item can
  leave its staging files behind (`.pn-workflow-*` directories and a
  `.<name>.pn-reservation` file next to the output); remove them by hand
  before reprocessing that item (v0.3.0 left the same files when the run
  ended).
- **Error codes.** Still 39; `unsupported_input_codec` is narrower and
  `internal_error` also covers a worker panic (see the
  [CLI contract](../research/CLI_CONTRACT.md#error-codes)).

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

AAC adds two notes. The AAC ports reproduce Apple's encoder and decoder on the
recorded Apple M4 reference (macOS 27.2 build 26B123) for the engine's own call
sequence; AudioToolbox picks between a generic and an SME kernel from the stack
position at the call, and only the generic classes are reproduced (the engine
pins them by construction on macOS). Another Apple Silicon generation, OS build
or caller can behave differently; do not assume it is an identical reference.
The ports run with flush-to-zero cleared, so the x86 tininess difference above
does not arise inside them, but it still applies to the DSP that runs between
the decoder and the encoder. Second, with the caller's flush-to-zero mode set
while samples are written, the encoder reproduces AudioToolbox's flushing of
subnormal input samples but not the change that mode also makes to
AudioToolbox's own arithmetic for input made of tiny normal numbers (a constant
level of about 2^-126 to 4e-37, a sine of 1e-33 to 1e-30, subnormals mixed with
one near-minimum normal sample, infinities or NaN mixed with subnormals), which
gives other access units than the mode-clear stream; the smallest recorded case
is 3,008 frames of mono DC at -2^-126, and 18 such cases turned up in 24,000
random ones written inside a flush scope. The engine never writes inside a
flush scope, where its output is exact.

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
  no JSON outcome); none is known to be reachable. **Superseded in v0.4.0:** a
  panic in the worker of one `process`, `analyze` or `inspect` item fails that
  item alone (`internal_error`, stage `worker`) and the rest of the batch is
  processed and reported; a panic elsewhere still ends the run. The failed item
  can leave its staging files behind (`.pn-workflow-*` directories and a
  `.<name>.pn-reservation` file next to the output); remove them by hand before
  reprocessing it. This is a limit, not a feature: the files were left behind
  in v0.3.0 as well when the run ended.
- **Pictures.** With the opt-in, a FLAC or MP4 destination has no aggregate
  picture cap; TagLib 1.11.1 sizes its byte vectors in 32 bits, so pictures
  totalling 4 GiB or more (and roughly 12 GiB of memory) are outside what is
  supported. ID3 destinations (MP3, AIFF, WAV) are checked exactly.
- **AAC input (new in v0.4.0).** An AAC file is read whole like an MP3 (at most
  2 GiB) and a packet table of more than 16,777,216 packets is refused; the
  decoded length a file claims is not capped, as above (an access unit of 4
  bytes already yields 1,024 frames). Memory and work are bounded by the
  input: beyond the file image the reader holds 16 bytes per packet, a file of
  N bytes costs at most about 21 N bytes live and 16 N in one allocation, and
  every walk of an atom, chunk or tag chain is linear in the file or stops at a
  fixed limit ([AAC_INPUT.md](AAC_INPUT.md)). `tests/portable_aac_hostile.rs`
  mutates the small AAC-LC inputs and a fragmented MP4 6,000 times (byte and
  bit damage, truncation, forged container headers, absurd table counts, noise
  over access units), generates 20,000 LOAS/LATM, 8,000 CAF and 8,000
  fragmented MP4 streams from the grammar of their container in a release
  build (a fifth of each in a debug build) and builds inputs of megabytes
  whose tables would cost a product of two sizes if they were scanned naively.
  It asserts no panic and no hang (a watchdog per case), allocations of at
  most 17 bytes per input byte in one piece and 40 in total beyond a fixed
  256 KiB and 1 MiB, CPU time in proportion to the input size, frames bounded
  by the input size, and typed outcomes only. The v0.3.0 reviews above did not
  cover the AAC reader; the reviews of this release are summarized in
  [VERIFICATION.md](VERIFICATION.md#v040-changes-and-evidence).
- **Atom and chunk count of the ALAC and PCM CAF readers.** The ALAC reader, the
  generic PCM CAF reader and the scan that refuses other MP4 files read at most
  65,536 top-level atoms or chunks (`MAX_SCANNED_BOXES`): a file with more is
  refused (`invalid_input_file` for ALAC, `unsupported_input_format` for the
  others) however small its atoms are. The AAC container model has its own
  bounds ([AAC_INPUT.md](AAC_INPUT.md)).
- **Hardening.** The bridge is built with `-fstack-protector-strong`,
  `-D_FORTIFY_SOURCE=2` and `-z relro,now`; the TagLib static library it links
  and libopus are not.

## Linux native libraries

The ABI-5 bridge and codec libraries are loaded from explicitly configured,
trusted paths. Linux additionally permits only exact, architecture-specific
SHA-256 identities compiled into `src/linux_libraries.rs`. The bridge introduced
in v0.3.0 (v09) adds WAV picture handling and the ID3 size check to the v0.2.0 bridge
(v08, aarch64 `8708af52d83fc7fdb6a487d72abc90ff4bd9f9b94e313436cc6fb68cc9db2aa0`,
x86_64 `ea86881cb6615d7cf8614dfb285e8670d978de621957b15611b8bd6d5b0e1ad5`) and is
built with stack protector, `_FORTIFY_SOURCE=2` and immediate binding with a
read-only relocation segment, so it has a new identity; libFLAC and LAME are the
v0.1.0 builds, and libopus is the optional fixed-point 1.6.1 build (the v0.1.0
and v0.2.0 bridge digests are no longer accepted; the pins of v0.4.0 are unchanged since v0.3.0 (bridge `5c52c5f1…` aarch64 / `8c61f6bb…` x86_64, libFLAC `41b59a45…` aarch64 / `9420c96f…` x86_64, LAME `c780062f…` aarch64 / `cf59ae7f…` x86_64, libopus `c09146de…` aarch64 / `d09791af…` x86_64)):

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
`ghcr.io/mschienbein/platinum-engine:v0.4.0`. It contains the CLI, libraries,
notices and corresponding-source/relink materials. It uses UID/GID 65532 and
runs `platinum` as its entrypoint; it does not set the library configuration
automatically. The following gives the named volume a persistent state and uses
libraries from the image. Verify the three printed hashes against the table in
"Linux native libraries" for the image's architecture before configuring them;
the v0.4.0 image carries the bridge of that table. The bridge digests of the v0.1.0
image (aarch64 `10eb77c1e12d7a12959300da3a1d853fe4907a3c7872bb4f83b12ab255549a9b`,
x86_64 `27922fdf10e6c7efbb43f8645f74138b82c4598edf0c3f2e2f55410cbaf3714d`) and
of the v0.2.0 image (listed above) are not in that table, and this release
refuses those bridges.

```sh
IMAGE=ghcr.io/mschienbein/platinum-engine:v0.4.0
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
sh tools/linux/build-image.sh platinum-native:local "$HOME/platinum-build-context-v0.4.0"
sh tools/linux/build-cli.sh platinum-native:local "$HOME/platinum-build-output-v0.4.0"
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
v0.4.0 NEW_OUTPUT_DIR`, where PLATFORM is `linux-arm64`, `linux-amd64`, or
`macos-arm64`; it does not publish. The runtime image recipe is
`linux/Dockerfile.runtime` and expects verified extracted release payloads for
both architectures. [BUILD.md](BUILD.md) and [THIRD_PARTY_NOTICES](../THIRD_PARTY_NOTICES)
cover source checksums, attribution and relinking. A successful build or
package is not a universal parity certification.
