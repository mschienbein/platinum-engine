# AAC input on Linux and `--features portable` builds

The portable `Reader` decodes AAC-LC exactly as AudioToolbox's `ExtAudioFileOpenURL` + `ExtAudioFileRead` do on the reference machine (macOS 27.2
build 26B123, arm64; the generic-DFT output class that the engine's read calls reach). Where the reference fails with an `OSStatus`, the same status is
answered; where it decodes something the port does not reproduce, the answer is a typed refusal (`CodecError::UnsupportedCodec { codec: "AAC",
operation: "decoding" }`, error code `unsupported_input_codec`; `UnsupportedSampleRateConversion`, error code `unsupported_sample_rate_conversion`; or
`UnsupportedFormat`, error code `unsupported_input_format`), never a different result - with the exceptions recorded in "Not reproduced" at the end of this document, where the
port answers differently from the reference for inputs of a few damaged shapes (a different result, not a refusal). One input the reference refuses is answered with a typed refusal
as well and not with its status: AAC inside a WAVE container (below). The macOS default build keeps AudioToolbox. The writing side is
described in [LINUX.md](LINUX.md#aac-input-and-output).

Numeric constants and algorithms were recovered from Apple's AudioToolbox binaries on one reference machine by static analysis, instruction-level
transcription and recorded runtime states; no Apple binary or source code is included, and no legal clearance is claimed. AAC is a patented format;
patent licensing may apply to decoders in some jurisdictions. See [THIRD_PARTY_NOTICES](../THIRD_PARTY_NOTICES).

## What is read

| input | how it is recognised |
| --- | --- |
| MP4 / M4A / M4B / 3GP / 3G2 / MOV, fragmented MP4 | the first sound track's first sample entry is `mp4a` (an `alac` entry stays with the ALAC reader) |
| ADTS | a header AudioFile accepts (12-bit sync, layer 00, profile LC, sampling index below 13) at the start or behind leading ID3v2 / ID3v1 blocks; the names `.aac` and `.adts` commit to it |
| LOAS / LATM | the `0x2b7` sync word at the start or behind the tag blocks; the names `.loas` and `.latm` commit to it |
| CAF | a `desc` chunk whose format is `aac `, `aach`, `aacp`, `aacl` or `aace` (other CAFs are the float32 PCM intermediates) |

Name hints follow AudioFile: an MPEG audio name (`.mp1 .mp2 .mp3 .mpa .mpeg`) commits to the MPEG reader, `.aac .adts` to ADTS (MP4 or CAF bytes under
that name fail `'sync'`, an MPEG frame header `'layr'`), `.loas .latm` to LOAS (anything else `'fmt?'`), `.mov` refuses every file that is not MP4 or
QuickTime (-12848), `.ogg .oga .opus` fail `'dta?'` and `.sd2` -39 on everything that is not an Ogg stream (resp. not empty). Any other name leaves the
choice to the content. Leading ID3v2 (size rule: `ID3`, version bytes other than 0xff, syncsafe size + 10, no footer) and ID3v1 (`TAG`, 128 bytes)
blocks are stepped over in any number and order; MP4 and CAF behind them are sniffed but do not open (`'dta?'`).

The command line admits an input by its file name extension (`.m4a` is the one it admits for AAC) and its tag stage must classify the container, so
what reaches this reader through the CLI is narrower than what the `Reader` itself reads: a CAF, for example, is refused by the tag stage
(`marker_unverifiable`) on every build. See the [CLI contract](../research/CLI_CONTRACT.md).

Client formats: float32 interleaved at the source channel count (`read`) and two planar float32 planes (`set_stereo_planar` + `read_stereo_planar`); the
planar client of a multichannel source is AudioToolbox's converter mix-down (11 layouts verified; AAC 6.0, 7.0 and octagonal have none and fail `'!dat'`
like the reference); a 5.1 source is mixed with the ISO matrix mix-down instead when an in-band program config element has set a `matrix_mixdown_idx` (below). The client rate must be the file's: another rate is `UnsupportedSampleRateConversion`. The whole input is read into memory (the
decoder works on the file image) and files above 2 GiB are refused (`UnsupportedFormat`), as for MP3; the check that asks the AAC container model about a
damaged MP4 or CAF file that the other readers refuse looks at files of up to 256 MiB only (above that the other readers' errors stand).

Memory and work are bounded by the input. Beyond the file image the container model holds one table entry of 16 bytes per packet (plus 4 bytes for a
CAF or a `trun` size while it is read, and 8 bytes for every LOAS frame behind the first that cannot be read as an access unit: the `LoasStream.failing` index vector); a packet is a byte of the file at least, so a file of N bytes costs at most about 21 N bytes live and 16 N in one
allocation, and no file more than 2^24 packets (about 270 MB of table; ADTS and LOAS grow their table by doubling, up to twice that). Every walk of a
structure is linear in the file: the box and chunk sniffers stop after 2^20 atoms or chunks (what follows is the container model's verdict), the readers
behind them (`.mov`/MP4 refusals, the ALAC and PCM CAF readers) after 2^16, a chain of leading ID3 blocks is read through a 64 KiB window and stops at 2^20
blocks, and the `iTunSMPB` search shares one allowance of half the movie atom's size plus 64 atom visits among all its `udta`, `meta` and `ilst` scans (a file
that needs more has no `iTunSMPB`; the ordinary files and every recording of the fixture pack use a fraction of it).

## What a read does (the contract the engine relies on)

The engine reads 512 frames into a zero-filled buffer and processes the whole buffer, so what AudioToolbox writes behind the returned count is part of
the result. The port reproduces the per-call behaviour measured on the reference (the complete `ExtAudioFileRead` model, any request size):

- the decoded stream is read from the first packet; the first call discards the priming in chunks of `min(request, priming left)` frames **decoded into
  the caller's buffer**, the real read then overwrites it;
- a call asks the converter for the whole request, not only for the valid frames: it returns `min(frames produced, valid frames left)` and writes the
  decoded padding behind that count (the remainder of the last packets) up to the request or the end of the data; the rest of the buffer stays untouched;
  a call made after the valid frames are used up returns 0 but still decodes up to a request of frames into the buffer;
- the converter pulls packets from the file in batches of `floor(request / 1024) + 1` packets (`ceil(chunk / 1024)` for the priming chunks), at most as
  many as fit 64 KiB by the packet size upper bound; a batch that holds an oversize packet (above 1024 bytes per channel) or one that lies (partly)
  outside the file fails the whole call with -50 (MP4); in a CAF cut inside its packet table the same request fails -50 when it ends inside the table and
  ends the stream silently when it reaches past it; in an ADTS file with a multi-block frame the scan `start + count (+1 for count >= 2)` frames far
  fails `'dta?'`;
- an access unit the decoder rejects yields no frames and resets the decoder; the fourth in a row (the run survives call boundaries) fails `'bada'`;
- zero packets fail -66567 on every call; an MP4 sample table without `stsc` entries fails `'pck?'`;
- the planar client of a source with 1, 3, 4 or 5 channels (a channel selection, not a mix) loses a failure that arrives after a 4096-frame inner request
  produced frames.

A failed read poisons the `Reader` (every later call is `CodecError::Poisoned`), as the native wrapper does. `file_length_frames` and `bitrate` do not
change later reads.

## Decoder class

AudioToolbox's AAC decoder selects its 512-point inverse-transform kernel per `ExtAudioFileRead` call from the stack-pointer residue: the generic NEON
kernel (class G) for every residue but one, an SME kernel (class S) when the residue at the read's entry is 16. The two classes round differently. The
port reproduces class G, the one the engine's read calls reach (the macOS default build enters both read call sites at residue 0 through a pinned
trampoline, see [VERIFICATION.md](VERIFICATION.md#v030-changes-and-evidence)). Class S is not reproduced; the engine never selects it.

## Properties

- `source_format()`: `'aac '`, no flags, 0 bytes per packet and frame and bits, 1024 frames per packet, the file's rate and channels.
- `file_length_frames()`: iTunSMPB / edit list (with a negative `roll` group) / CAF `pakt` / the default priming of 2112 frames; a multi-block ADTS file
  reports 0.
- `bitrate()` is `AudioFileGetProperty(kAudioFilePropertyBitRate)`: `audio data bytes * 8 / (packets * frames per packet / rate)` in `f64`, cut to `u32`
  (saturating at `u32::MAX`), 0 without packets. The rounding of the duration is part of the rule: 7,686 bytes in 14 packets of 1024 frames at 48 kHz is
  exactly 205,875 bit/s as a ratio and 205,874 as AudioToolbox answers. The audio data bytes are the sum of the sample sizes (MP4, CAF: the data chunk
  less its 4-byte edit count) or the file size less the leading tag blocks (ADTS, LOAS). Checked on every input of the fixture pack that opens against the
  recorded AudioToolbox answers (0 differences). A CAF whose `desc` has no frames per packet
  answers `'dta?'` (`AudioFileGetProperty(bit rate)`).

## Typed refusals

| trigger | error |
| --- | --- |
| HE-AAC v1 / v2 (SBR / PS signalled in the ASC, hierarchically or not, or by the CAF format, a CAF `aac ` over a cookie that signals them, or an SBR extension payload with a header in the first access unit of an ADTS stream) | `UnsupportedCodec` (AudioToolbox decodes it) |
| AAC-LD, AAC-ELD, MPEG-1 audio and USAC object types in an AAC file | `UnsupportedCodec` |
| CAF whose `desc` rate differs from the ASC's (by any amount) or is not a whole number of Hz | `UnsupportedCodec` (AudioToolbox converts the rate inside the decoder) |
| CAF whose `desc` has fewer channels than the ASC | `UnsupportedCodec` (AudioToolbox down-mixes inside the decoder) |
| a multi-block ADTS frame reached by a stereo-converted read of a 1, 3, 4 or 5 channel stream | `UnsupportedCodec` |
| a failure inside the priming skip of a stereo-converted read of a 1, 3, 4 or 5 channel stream | `UnsupportedCodec` |
| a client format change after the first read | `UnsupportedCodec` |
| a packet table of more than 16,777,216 (2^24) packets: at most 2^34 frames, and an access unit of 4 bytes already yields 1,024 frames, so the frames a file yields are bounded by its packet count, not by its size | `UnsupportedCodec` |
| a decoder configuration that cannot be built although the container was accepted | `UnsupportedCodec` |
| client rate different from the file's | `UnsupportedSampleRateConversion` |
| input above 2 GiB | `UnsupportedFormat` |
| AAC inside a WAVE container (format tag 0xff) | `UnsupportedCodec` (AudioToolbox does not open such a file, `'fmt?'`; the port refuses it with the typed error instead of reproducing that status) |
| MP4 whose first sound entry is `ac-3`, `ec-3`, `drms`, `samr` or `sawb` (AudioToolbox opens them as other codecs), or `alac` | `UnsupportedFormat` / `UnsupportedCodec` as before (the other readers') |
| MP4 whose first sound entry is any other type but `mp4a` | `CodecError::Native` `'wht?'`, as AudioToolbox |

An HE-AAC stream whose AudioSpecificConfig does not signal SBR or PS (it names the AAC-LC core only) is not refused: AudioToolbox decodes the core at the
core rate and ignores the SBR payload, and so does the port (inputs of this kind are among the exact entries of the fixture pack). ADTS is the exception
described under the container rules: there an SBR extension payload in the first access unit is what makes the stream HE-AAC. Dynamic range control is not
modelled.

AudioToolbox's own refusals are reproduced with its statuses (`'wht?' 'typ?' 'dta?' 'sync' 'layr' 'chk?' 'fmt?' 'rate' '!dat'`, -50, -66567, `'bada'`,
`'pck?'`) as `CodecError::Native` with the operation names of the native wrapper (`ExtAudioFileOpenURL`, `ExtAudioFileSetProperty(client format)`,
`ExtAudioFileRead`, `ExtAudioFileRead(planar stereo)`). In the CLI a status at open is typically reported as `invalid_input_file` and a status during a
read as `decode_failed`; the typed refusals above keep their own codes (`unsupported_input_codec`, `unsupported_sample_rate_conversion`,
`unsupported_input_format`).

A damaged MP4 or CAF container fails with AudioToolbox's status whatever the codec (`'typ?'`, `'dta?'`, `'chk?'`): when the ALAC and PCM readers cannot
open an MP4 or CAF file, the AAC container model is asked, and its structural failures are the answer; whatever else is wrong with the file (another
codec, a sample entry that is neither `mp4a` nor `alac`) keeps the typed errors of those readers.

## Container rules recorded from AudioToolbox (beyond the documented formats)

- ADTS: the header size (7, or 9 with CRC) is the first frame's for the whole stream; a later header with profile 3, a sampling index of 13..15, or
  protection_absent 0 behind an unprotected first frame ends the stream there; a first MPEG-2 header (`ID` 1) with profile 3 fails `'prof'`; an SBR
  extension payload (`fill_element`, type 13 or 14) in the first access unit makes the stream HE-AAC: without a `bs_header_flag` it fails `'fmt?'`, with
  one it is refused (a payload this port cannot tell from a corrupt one is refused too; AudioToolbox fails `'fmt?'` for a corrupt one).
- MP4: `ftyp` needs a plain 32-bit size below 2^31, and its effective size (the declared size, or what the file holds of it) of 20 to 99 bytes makes an
  MP4 file (the movie atom may hide inside a larger declared box: `'dta?'`), of 100 bytes or more it is none (`'typ?'`). Atoms are read as far as their
  data goes and a child never leaves its parent: one that runs past the end of its parent or of the file is clamped to it (a 64-bit size of 2^63 or more
  is no atom), so a track whose `mdia` runs past its `trak` has none and an `iTunSMPB` `data` atom that runs past its item is not used; the movie atom
  is held in memory as far as the file goes, which is all its contents see; a movie atom with a size of 0 is none (`'dta?'`). A cut file keeps what
  precedes the cut (`'dta?'` until the handler of a sound track is complete, then `'wht?'` for the sample table that is missing).
  The `iTunSMPB` value counts only under the brands AudioToolbox types as `m4af` / `m4bf`: a compatible brand (or the major brand) of `M4A `, `M4V ` or
  `M4B `, and no compatible brand from `3gp4` to `3gp9` or `3g2a` (byte for byte; the minor version does not matter); an edit list together with a
  negative-`roll` sample group counts for every brand, an edit list alone for none.  The frame count of the `iTunSMPB` text (and the valid-frame count of a CAF packet table) is reported as `int64(double(count) + 0.5)`: exact below 2^52, from
  there odd counts round up to the even number above, from 2^53 the double rounds to even, saturated at 2^63 - 1; an `iTunSMPB` count from 2^63 on means no tag.  The three
  `iTunSMPB` fields are read at fixed places of the text (priming from character 9, remainder from 18, valid frames from 27: 9, 9 and 17 characters, each as C `strtoull`
  in base 16 with its white space, sign and `0x` prefix, priming and remainder cut to 32 bits), whatever stands at the separators: junk there gives 0, a hex digit is a
  digit; the first 9 characters are not looked at.  Of several `udta` atoms the
  LAST one with a usable tag counts (a later one without an `iTunSMPB` item, with a text that is no tag, or an empty one leaves the earlier tag); within a
  `udta` the first `iTunSMPB` item decides.
  `mdhd` needs version 0 or 1 and a non-zero timescale inside the atom (nothing behind it is read, else `'wht?'`), `stsd` a non-zero entry count, the
  descriptor lengths of `esds` must hold (else `'wht?'`). A first sound entry that is not `mp4a` fails `'wht?'`, except for the types AudioToolbox opens
  as other codecs (`alac`, `ac-3`, `ec-3`, `drms`; `samr` and `sawb` fail the client format): those stay with the other readers.
  Sample sizes: `stsz`, or the compact `stz2` with 16-bit fields (any other field size fails `'wht?'`); a count the table cannot hold fails `'wht?'`, a
  smaller one is read as far as it goes; a table whose sizes are all 0 has no known packet size (-66567 at the first read). Chunk offsets: the entry count
  of `stco` / `co64` is ignored, the entries the atom holds are read; a table that cannot hold its header fails `'wht?'`. Sample-to-chunk: the first run
  covers the first chunk whatever its `first_chunk` says; an entry count the atom cannot hold, or no run at all, leaves the packets without a location
  (`'pck?'` on the first read). A packet that no chunk reaches is read from the start of the file, where the converter rejects it: four rejected in a row
  fail the read `'bada'`, a single one at the end of the table ends the stream without an error.
- MP4 containers that are a few bytes short: the sample tables (`stsz`, `stz2`, `stsc`, `stco`, `co64`) are bounded by the end of their TRACK, not by the `stbl` / `minf` /
  `mdia` that holds them (macOS 27.2): shortening the size field of the `stbl`, `minf` or `mdia` by up to the size of the last table (as long as the table's header
  still starts inside the `stbl`) leaves the table whole; shortening the `trak` cuts it off. (Not reproduced: a `mdia` short by 16 to 20 bytes over a 28-byte `stco`, where
  AudioToolbox fails the open `'wht?'` and the port opens.)
- MP4 sample groups: every `sgpd` of the sample table is read, in any number, or the open fails `'wht?'`. Its payload (behind the 8-byte atom header) is the
  version and flags and nothing else (4 bytes: no description), or the grouping type, `default_length` (version 1 and above), one more word (version 2 and
  above) and the entry count, all of which must be there (3 bytes, 5 to 7, or a count that is cut: `'wht?'`). Only the grouping types `roll` and `prol` have
  entries read: of 2 bytes (version 0; version 1 and above with `default_length` 2, or 0 with a length in front of every entry; any other entry length is no
  description and leaves the gate off), one by one, a `roll` description ends at its first non-negative distance (a `prol` one at its first non-positive one);
  an entry that is not there fails the open, except when the announced entries could not fit by more than 12 bytes (7 or more 2-byte entries missing, whatever the
  count): then nothing is read and the description counts as one without entries. The gate for the edit list is on when some `roll` description has only
  negative distances (a description without entries has only those); `prol` never switches it on; a cut `sbgp` is ignored.
- MP4 size fields: a child of the movie atom whose size field is 0 or 2..7 fails the open `-1` (the first child) or `-2` (a later one: the walk is one child
  after the other, whichever atom it is); inside an atom below the movie atom a child of size 0 or 2..7 ends the scan there, the child is lost with its
  siblings behind it (a `hdlr` of size 0 leaves no sound track: `'dta?'`; a `minf` of size 0 no sample table: `'wht?'`), and a data information or sample
  table atom that has one is spoilt (`'wht?'`); a handler atom of less than 12 bytes names no handler. The first child of a `udta` starts 8 bytes behind
  the `udta` and is bounded by the movie atom, not by the size the `udta` declares.
- MP4 fragments: the samples of every `trun` of the track's fragments are read (`tfhd` base data offset, the `trun` data offset, the default sizes of
  `trex` and `tfhd`); a `trun` whose sample count its atom cannot hold is skipped as if it were not there, a `trun` with samples and neither a size field
  nor a non-zero default size fails `'wht?'`, a zero-size sample is accepted; the bit rate counts the bytes of a `trun` up to its first zero-size sample.
- CAF: chunks are read in order, a chunk running past the end of the file is ignored and ends the scan; without `data` there are no packets, except that a
  complete `pakt` without `data` fails `'chk?'`; a `data` chunk cut before its edit count holds no packets; a complete one must reach the start of the
  last packet of the table (`'chk?'`). `desc` fields: frames per packet 0 opens and fails the first read (`'bada'`), another value than 1024 fails the
  client format (`'fmt?'`, from 1,000,000 frames -50); bytes per packet or bits per channel other than 0 fail it too (`'fmt?'`; bytes per packet behind
  a `pakt` chunk fail the open `'chk?'`); more than 8 channels fail it `'fmt?'`; the format flags are reported.
- LOAS: a frame that runs past the end of the file ends the stream; when it is the first one and the file holds its complete StreamMuxConfig and payload
  length, the file opens with no packets (length 0, a read returns 0 frames at once), else it fails `'fmt?'`. A complete frame behind the first that
  cannot be read as an access unit (a StreamMuxConfig whose AudioSpecificConfig differs from the first frame's, a damaged or cut configuration, a program
  count of 1, a frame length type other than 0, a payload length that is missing, runs out or exceeds what the frame holds, an empty frame) stays in the
  packet table, so the file length and the bit rate count it and so do the frames behind it, and the read that reaches it fails `'fmt?'` (the frames before it
  are delivered). A complete first frame of this kind fails the open `'fmt?'`. A payload length of 0 is an ordinary packet (the decoder rejects it, the read
  goes on); a repeated configuration is no change.
- CAF layered description: a CAF with an `ldsc` chunk of 36 bytes or more (a stream description and a layout tag per layer; the first chunk counts) is presented
  with the format of its first layer in place of `desc` (afconvert's HE-AAC files: `desc` is the core 'aac ' at half the rate, `ldsc` 'aach' / 'aacp' at the
  full rate and 2048 frames per packet): an HE-AAC first layer makes the file HE-AAC even when the cookie is gone or damaged (refused, or `'!dat'` over a plain
  AAC cookie as for a `desc` of that format); a first layer of another format that differs from `desc` in format, rate, frames per packet or channels is not reproduced
  (refused: AudioToolbox presents the layer's rate and a rescaled length); `desc` is needed whatever `ldsc` holds (`'dta?'`).
- CAF `desc` formats: `aach` over a plain AAC cookie fails `'!dat'` (without a cookie it is decoded: refused), `aacp` without a cookie opens (and is refused) at 2048 frames per packet for 1 or 2 channels, any other shape fails `'fmt?'`; `aacp` over a cookie of
  another family, and `aacl` and `aace` over a cookie of another family or none, fail `'fmt?'` (over a cookie of their own family they are decoded: refused), `alac` over an AAC cookie fails `'!dat'`; `aac `
  over a cookie that signals SBR or PS is decoded as HE-AAC at the `desc` rate whatever that rate and the frames per packet are (refused); a
  sample rate that is not a number opens (and is refused as a rate of 0); the converter's rate limits (7350 Hz and 110,852 Hz up to 2,999,999 Hz
  `'fmt?'`, 3,000,000 Hz -50) apply to the `desc` rate before a rate that differs from the cookie's is refused.
- SBR and PS signalling counts for AAC-LC (`audioObjectType` 2) only, whether it is written hierarchically (`audioObjectType` 5 / 29 in front of the core
  configuration) or as the backward-compatible extension behind it: AudioToolbox answers alike for both forms. Over another core the file is plain at the
  core rate. The extension rate must be the core rate or twice it, else `'wht?'` (reserved rate indices too). SBR: the channel configurations 3 and 5 open
  and fail the client format `'fmt?'`, 0, 8, 9, 10, 13 and 15 fail `'wht?'`, the others are HE-AAC (refused). PS: configuration 1 is HE-AAC v2 (refused)
  when the rate doubles and fails `'fmt?'` when it does not; 2, 6, 7, 11, 12 and 14 open as plain mono AAC-LC at the core rate; 3, 4, 5, 8, 9, 10, 13 and
  15 fail the client format `'!dat'`; 0 fails `'wht?'`. An ASC that ends inside `coreCoderDelay` (two bytes, dependsOnCoreCoder set) opens.
- A refused planar client format (`'!dat'` for AAC 6.0, 7.0 and octagonal) leaves the handle without a client format: every later read fails -50.
- Matrix mix-down: an in-band program config element (`id_syn_ele` 5 inside an access unit, anywhere in it) that carries `matrix_mixdown_idx_present`
  sets the matrix mix-down index (0..3; `pseudo_surround_enable`, the mono / stereo mix-down tags, the element lists and everything else in the PCE have no
  effect) as soon as the index is read: a PCE that runs out of bits behind it, a later PCE without an index, a decoder reset and an access unit the decoder
  rejects keep it; a later PCE with an index replaces it; it is in force for the access unit it was found in and every one behind it, independent of the read size.
  The stereo (planar client) mix-down of a **5.1** stream (`channelConfiguration` 6, or a PCE of the 5.1 shape) then uses, with `c = 1/sqrt(2)` and
  `a` = `1/sqrt(2)`, `1/2`, `1/(2 sqrt(2))`, `0` for the index 0, 1, 2, 3, `norm = 1 / ((1 + c) + a)` (all `f32`): `L = norm (c C + L + a Ls)`, `R = norm (c C + R + a Rs)`,
  accumulated like the other matrices (the first non-zero coefficient, then fused multiply-adds in channel order, the LFE left out; coefficients `norm * c`, `norm`,
  `norm * a`), bit-exact against recordings of all four. Every other layout (3.0, 4.0, 5.0, 6.1 and the three 7.1) keeps its matrix whatever the index;
  the interleaved read of the source channels is not changed; a PCE in the configuration (ASC / `channelConfiguration` 0 PCE of the cookie) never sets an index.
  **Not reproduced (refused):** on damaged 5.1 streams AudioToolbox's stereo mix-down was seen to ignore such a PCE, or to mix neither with the plain nor with the ISO matrix,
  where the PCE is not the stream's own. A 5.1 stream read with the stereo client therefore fails the read (`UnsupportedCodec`, "an in-band program config element that does
  not describe the 5.1 layout ...") as soon as an access unit carries an in-band PCE with a `matrix_mixdown_idx` that is not *well formed* - its element lists are exactly
  front SCE + CPE, back CPE, one LFE, no associated data and no coupling channel, as the PCEs of valid streams are.  A well-formed PCE is mixed even in an access unit
  the decoder rejects (identical to Apple).  Valid streams (every recorded PCE of the fixture pack) are not affected; the interleaved read is not (it is identical on both
  sides). In a random comparison of damaged streams (2,300 variants: single-field mutations of every PCE field, valid and invalid, at several places, a well-formed
  PCE in front of a cut access unit, bit flips) every variant was either identical to Apple (1,544) or refused (756), none differed.

## Evidence

The reader is checked against AudioToolbox's recorded behaviour by `tests/portable_aac_decode.rs` and `tests/portable_aac_hostile.rs` on the fixture
pack `tests/fixtures/aac-decode/` (generated audio and damaged variants with the results the macOS reference gave: 838 entries over 790 input files, of
which 510 are required to be exact, 254 to fail with Apple's status (or the engine wrapper's own refusal) and 74 to be typed refusals; no entry is left
without an expectation). 404 of them (`rule/`) pin the container rules above one rule at a time: the same bytes under 24 `ftyp` brand sets, `ftyp` sizes,
atoms that run past their parents, size fields that are no atoms, movie fragments, HE signalling, SBR/PS signalling, damaged sample tables, chunk offsets that no file holds, LOAS frames that change the configuration or cannot be read, in-band program config elements with a matrix mix-down, sample group descriptions, `iTunSMPB` counts above 2^52 and their separators, CAF packet-table counts above 2^52, several `udta` atoms, CAF layered descriptions and containers of the track whose size field is a few bytes short.
The pack, what it covers and how it is rebuilt are described in [its README](../tests/fixtures/aac-decode/README.md); the claims and their limits are in
[VERIFICATION.md](VERIFICATION.md#v040-changes-and-evidence).

Not reproduced (recorded, open):
- the packets of a damaged table at read sizes above 512 frames in the seven cases the fixtures README names (`rule/damage-stco-count-1`, `rule/damage-co64-count-1`,
  `rule/trun-zero-size-sample-2`, three `rule/damage-co64-offset-*-chunk-1`, and `rule/short-trak-minus-5`, which the replay therefore runs at 512 frames only): the port
  fails `'bada'` where AudioToolbox delivers frames;
- a `mdia` atom whose size field is 16 to 20 bytes short, over a sample table whose last atom is a 28-byte `stco`: AudioToolbox fails the open `'wht?'`, the port opens and
  decodes (a different result, not a refusal; it is not among the 21 recorded `short` cases, which are all reproduced);
- the other damaged shapes of the reference that the stress comparison found and that are not modelled (the status of the open and of the read of many damaged MP4, CAF and
  LOAS files, a fragment whose `data_offset` points into another atom, a CAF with a second `data` chunk, the `mvhd` and the first frames of an ADTS file when damaged): they are
  not exhaustively reproduced and may give a different result or status.
