# AAC decoder exactness fixtures

Golden data for `tests/portable_aac_decode.rs` and `tests/portable_aac_hostile.rs`. All audio is **generated**: the engine's Apple writer,
`afconvert`, ffmpeg's native encoder and synthetic bit-level streams (made by tools outside this repository, selected here), and containers
and streams built and mutated by the scripts in `tools/portable/aac/`. No recording of real music is included.

| file | content |
|---|---|
| `pack-0.bin` .. `pack-10.bin` | 790 distinct input files, concatenated (5,064,588 bytes; each input at most 60,000 bytes, each part at most 1.5 MB); `manifest.json` gives every input's part and offset |
| `manifest.json` | 838 entries (an input appears once per replay name): id, replay name, family, origin, SHA-256, selection features, `expect`, and what the macOS reference did with the file at 512-frame reads |
| `readsizes.json` | the same recordings at 100-, 513- and 4096-frame reads, planar and interleaved, for 182 of the inputs (see below) |

Hashes (all re-checked by `the_fixture_pack_is_intact_and_covers_what_it_claims`, which pins the two JSON files as constants and the parts through
`manifest.json`): `pack-0.bin` `d7fc06ad07507bf03513d248e8df6ace996c938533a7cacbfb8af04afb7e48bc`, `pack-1.bin` `3317f3147dd003560f94ef427500df3babe854da2518290a6dadb6784b2365d3`,
`pack-2.bin` `0cb54378235dd64221efdb35fc490a5f14bf642b1936f930cd3c5f261f19f4c7`, `pack-3.bin` `2b8775a1c839b4eed7ec0d5569cef17a27efe6a1e192a673d9fc954216b38777`,
`pack-4.bin` `5ba57fd0277cff15bf5c91a52006d519336a712ea82254b01f03762a26d0f637`, `pack-5.bin` `e1421ceddf75abddbc1330308fd58f810af7c51c546fd7023030c08adc89c31b`,
`pack-6.bin` `487333c7802e83d09cfcb10623b510e05869468cba75d9b9e7550224a4df5607`, `pack-7.bin` `24b4b0f86ca7637bc483340e9f983a28ad8cc203cd4d60e8ecb57af7dc050b0d`,
`pack-8.bin` `a3c4d6e202e33e46aba3e76a1112362115d35e9d2a684344c64c59d51d4219b7`, `pack-9.bin` `e7765e517fee2ff57c84dc417143abbffe20b6554c26d18206547226e361bbcb`,
`pack-10.bin` `514722671f909de2ed7822dd61452330cd210c12810fd12f3de659995bbc0b94`,
`manifest.json` `76edb70c1597fac78d5f10fb1fe42342f5b7e2e8da915d51b8e5487613ca64c9`, `readsizes.json` `fd14eb79de6afa61330ca9fbdfcb09d01ec97dabf93d4a890677e4c7f16c7531`.

## What the reference is

For every input the macOS arm64 reference (macOS 27.2 (26B123) arm64 Apple M4) is the engine's own `apple_codec::Reader` (AudioToolbox
`ExtAudioFileOpenURL` / `ExtAudioFileRead`, the read pinned to stack residue 0 = decoder class G, as the engine does), driven by
`tools/portable/aac/decode_probe` the way the engine drives it: client format at the file's rate, 512-frame reads, planar stereo
(mono duplicated; other layouts are mixed down by AudioToolbox) for analysis, or interleaved at the source channel count. Each input is
recorded **under the name it has in the replay** (`name`): AudioFile selects its file type from the extension (an MPEG audio
extension commits to the MPEG reader whatever the bytes are). Everything in the manifest is copied from those runs; each recording
was made twice and had to be identical.

Per input (`manifest.json`):

* `open`: accepted, or refused with the failing call (`ExtAudioFileOpenURL` or `ExtAudioFileSetProperty(client format)`) and its
  signed `OSStatus`, or `{"ok": false, "other": ...}` where the engine's own wrapper refuses before AudioToolbox sees the file (a sample
  rate of 0: `InvalidInput`);
* `info`: the complete source `AudioStreamBasicDescription`, the client format after open, `file_length_frames()` and `bitrate()`
  (a value, or the failing call and status);
* `planar512`, `inter512`: frames delivered, the failure that ended the stream (the frames of the failed read are not delivered),
  the SHA-256 of the float32 stream (little endian; planar: L,R per frame), the number of reads and of short reads, a digest of the read
  lengths, and **`tails`**: every read that returned fewer frames than asked for (the last one, and the final read that returns 0)
  with the number of frames the read wrote *behind* the frames it returned into the sentinel-filled buffer (12345.5) and the FNV-1a of
  their float bits. Apple's converter hands the decoder's padding (the file's `remainder` frames) to the caller there; the engine
  processes whole blocks, so the portable reader must write the same bytes. An empty tail means the buffer behind was untouched;
* `inter_after_planar_refusal`: for the one file whose planar client format Apple refuses (`'!dat'`), what an interleaved read of that
  same reader delivers (the refusal does not poison the reader);
* `after_failure`: for an interleaved stream that ends in a read failure, what the **raw AudioToolbox handle** does for the next six reads.
  It is not poisoned: for some inputs the next reads deliver the frames of the following good packets, for others it keeps
  failing. The engine's `Reader` is a wrapper that poisons itself at the first failed read, and **the portable reader must do the same**:
  every later `read`, `read_stereo_planar` and `set_stereo_planar` returns `CodecError::Poisoned` and writes nothing (as for MP3 and ALAC);
* `rate_variants`: Apple's answer when the file is opened with another client rate (`Reader::open(path, Some(rate))`), i.e. with its
  sample-rate conversion: 60 `apple_error`, 15 `refuse_codec`, 95 `refuse_src` (for every file at or below 22.05 kHz at 44.1 kHz, and for a sample of the others and of the refusals).

## `expect`: what the portable `Reader` must do

| `expect` | files | the replay requires |
|---|---:|---|
| `exact` | 510 | opens; source format, client format, file length and bitrate equal; planar and interleaved 512-frame streams equal in frames, failure (`operation` + `OSStatus`), SHA-256, read lengths and tails; the same at 100/513/4096 frames (`readsizes.json`); poisoning after a failed read |
| `refuse_codec` | 74 | `Err(CodecError::UnsupportedCodec { codec: "AAC" (or "AAC/..."), operation: "decoding", .. })` whatever the client rate (`UnsupportedSampleRateConversion` is also accepted when a client rate is given): what Apple opens and the port deliberately does not decode or reproduce: HE-AAC v1/v2 (`'aach'`/`'aacp'`), LD (`'aacl'`), ELD (`'aace'`), MPEG audio object types inside an MP4 (`'.mp1'`/`'.mp2'`), a CAF `desc` of `'aac '` over a cookie that signals SBR/PS, a CAF whose `desc` rate is not the cookie's |
| `refuse_src` | 0 (variants: 95) | `Err(CodecError::UnsupportedSampleRateConversion { source_rate, client_rate })` with the file's own rate and the requested one; the recordings of those variants are Apple's converted streams (not asserted, for documentation) |
| `apple_error` | 254 | `Err(CodecError::Native { operation, status })` equal to Apple's (or `InvalidInput` where the engine's wrapper refuses) |

There is no class without an expectation: every input is either reproduced, refused in a typed way, or fails exactly as Apple's call does.

Notes on what "exact" covers:

* **AAC-LC at the file's own rate** (`format id 'aac '` as Apple reports it), in every container Apple opens it from, including
  implicitly signalled HE-AAC (the ASC names the LC core only: Apple decodes the core and ignores the SBR payload, so the output is at
  the core rate) and every damaged file that Apple still decodes (up to three consecutive rejected access units are skipped, the fourth
  fails the read with `'bada'`; no concealment).
* **Misnamed content**: AAC bytes under `.mp3`, `.mp2`, `.mp1`, `.mpa`, `.mpeg` are read by AudioFile's MPEG audio reader, which finds
  frame sync words inside the AAC data (format `'.mp1'`/`'.mp2'`/`'.mp3'`, `has_mpeg_extension` routing); the expectation is the port's exact
  MP3 reader's, so these inputs also judge that reader on content it was never designed for. Names such as `.wav`, `.bin`, no extension
  or `.latm` on AAC data give the content-sniffing or per-type answers Apple recorded (`the_file_name_selects_the_reader_for_the_same_bytes`).
* The decoded planes of a source with more than two channels follow AudioToolbox's layout-dependent mix-down.
* The packet table of a damaged MP4: where the sample table does not reach a packet (`stco`/`co64` without the chunk, a flipped sample-to-chunk map) AudioToolbox reads
  it from the start of the file and the converter rejects it. How many packets a failing stream delivers depends on how the reader batches packets, i.e. on the read size:
  `rule/damage-stco-count-1`, `rule/damage-co64-count-1`, `rule/trun-zero-size-sample-2` and `rule/short-trak-minus-5` are replayed at 512 frames only (the 4096-frame recordings deliver 6,080 frames
  without a failure, the port fails `'bada'` as it does at 512, 513 and 100 frames), and so are the three `rule/damage-co64-offset-*-chunk-1` cases whose second chunk lies
  beyond the file (4096-frame reads deliver all 8,820 frames: the packets of a batch are read from the position of the first one); every other input is replayed at all four sizes where `readsizes.json` has it.

## Where the inputs come from

| set | files | where from |
|---|---:|---|
| `corpus-a` | 78 | generated decoder corpus A (ffmpeg native encoder streams in 13 wrappers, afconvert, the engine Writer sequence, synthetic bit-level streams with every coding tool, container/priming variants); at most 60 KB each, chosen by rarity of features |
| `corpus-b` | 39 | generated corpus B, a held-out set (fresh ffmpeg streams with random PNS/IS/TNS/MS/coder/PCE at 8-96 kHz and 1-8 channels, Apple-encoded runs) |
| `probe` | 72 | container and stream probes (MP4/ADTS/LOAS/CAF variants, rejected access units, wrong element lists and sampling indices, truncations, edit lists, brands, tracks) |
| `fresh` | 62 | encoded through the engine's own `apple_codec::Writer` (256 kbit/s, 512-frame writes, MP4 header times pinned, the writer's `free` padding cut to 32 bytes): every config the engine's Apple writer accepts (44.1/48 kHz 1-8 channels, 32 kHz from 3, 24/22.05 kHz from 4, 16 kHz 7-8) with generated tones, noise, clicks, gated and clipped content, and lengths of 1 to 3,000 frames |
| `afconvert` | 41 | `afconvert` from generated tones: HE-AAC v1/v2 (MP4, ADTS, CAF, LOAS; plus implicit-signalling rewrites), LD, ELD, LC at 8-48 kHz in m4a/m4b/mp4/3gp/3g2/ADTS/LOAS/CAF with CBR/VBR; USAC and MPEG layers are not accepted by this afconvert |
| `mut` | 98 | deterministic, seeded mutations of one fresh MP4 per channel layout and one afconvert ADTS, CAF and LOAS file: box insertion/removal/reordering, 64-bit and giant sizes, table counts, ASC bit flips, truncation at every box or chunk boundary, access units replaced by noise (1-5 consecutive), oversize and zero-size access units, CAF desc/pakt/data damage |
| `rename` | 32 | the same bytes under 23 other file names (the file name selects the reader) |
| `id3` | 12 | leading ID3v2/ID3v1 blocks of several shapes in front of MP4, ADTS, CAF and LOAS files |
| `rule` | 404 | the container-rule cases (below), cut from one small stereo file of `tests/fixtures/aac-reader/` and from fixtures of this pack by `tools/portable/aac/make_decode_rule_fixtures.py` |

`corpus-a`, `corpus-b` and the `probe` set were produced with tools outside this repository; the rest is generated by `tools/portable/aac/make_decode_fixtures.py` and
`make_decode_rule_fixtures.py` from this repository's files and the macOS tools above. The `exact` inputs outside `corpus-a` and `probe` (fresh, afconvert, mutated, renamed, ID3-prefixed,
`corpus-b`, `rule`) were not part of what the decoder was developed on; they are the held-out judge.

### The container-rule cases (`rule/`)

Each group is a handful of small inputs that differ from one base file in the way the group's name says; every one is recorded on macOS, and the port must answer alike.

| group | files | what it pins down |
|---|---:|---|
| `brand` | 24 | the `ftyp` brands under which the iTunSMPB tag counts: only the 28 `ftyp` bytes of one file differ. The tag (8,820 valid frames) is read when a compatible brand (or the major brand) is `M4A `, `M4B ` or `M4V ` and no compatible brand is `3gp4`..`3gp9` or `3g2a`; brands are compared byte for byte (`m4a ` is none), the minor version does not matter. 10 of the 24 sets give 8,820 frames, 14 give 9,152 (`11 x 1,024 - 2,112`, the default priming) |
| `ftyp` | 23 | an `ftyp` box of 20..99 bytes is an MP4 file, of 100 bytes or more it is not (`'typ?'`); a declared size beyond the file is what the file holds of it; sizes of 20, 60, 99 hide the movie atom inside the box (`'dta?'`) |
| `atoms` | 10 | a track whose `mdia` atom runs past its parent is not the sound track (a scan of an atom's children never leaves the atom); an iTunSMPB `data` atom that runs past its parent is not used |
| `trun` | 19 | movie fragments: a `trun` that declares more samples than its atom holds is skipped, a `trun` without sizes (and a default size of 0) fails the open, zero-size samples, `tfhd` base data offsets near 2^63 |
| `cafhe` | 10 | a CAF whose `desc` says `'aac '` over a cookie that signals SBR (or PS) at several rates and frames per packet |
| `asc` | 41 | hierarchical (`audioObjectType` 5/29 in front) and backward-compatible SBR/PS signalling for the channel configurations 0..7, extension rates equal to, twice, or unrelated to the core rate, reserved indices, escape rates, LD and Main cores; ASCs that end inside `coreCoderDelay`, one byte long, with the extension flag |
| `sizes` | 44 | size fields that are no atoms (0, 1, 2..7) at every place of the movie atom's tree: a child of the movie atom with such a size fails the open `-1` (the first child) or `-2` (a later one); inside a track a child of size 0 is lost with the siblings behind it (a `hdlr` of size 0 leaves no sound track, `'dta?'`; a `minf` of size 0 no sample table, `'wht?'`) and spoils a sample table or data information it belongs to (`'wht?'`); a `udta` of 8, 9 or 12 bytes still has its first child read from 8 bytes behind it, bounded by the movie atom |
| `damage` | 71 | the compact sample size table `stz2` (field sizes 4, 8, 12, 16, 32, 0, counts that lie), zero sample sizes, `stsd` counts (0, 2, 2^32 - 1), `mdhd` of version 0/1/2 with a zero timescale or cut short, `stco`/`co64` entry counts that lie over three chunks, `co64` offsets of 2^64 - 1, 2^64 - 2, 2^63 and 2^40 (Apple: `'bada'` for the first two, -50 for the others) for the first or the second chunk, tables without entries, flipped sample-to-chunk map, a movie atom of size 0, 8 or behind `mdat`, sound entry types that are not `mp4a` |
| `loas` | 22 | LOAS frames carrying another StreamMuxConfig than the first, a repeated one (no change), a zeroed frame, and complete frames behind the first that cannot be read as an access unit (a cut or missing configuration or payload length, program count 1, frame length type 1, an empty frame, an escaped length that runs out, a payload longer than the frame, as the last frame too): the frame stays in the table and the read that reaches it fails `'fmt?'`; a first frame like that fails the open; a zero payload length is an ordinary packet; a frame cut by the end of the file ends the stream; a 60-byte stream of two frames (the second with another configuration and a payload longer than the frame) opens and fails the first read |
| `pcemix` | 18 | in-band program config elements (`ID_PCE` inside an access unit) with and without a `matrix_mixdown_idx`, in a 5.1 stream cut from a fixture of this pack (and a 7.1 and a 5.0 one): the stereo (planar client) downmix of a 5.1 stream is the ISO matrix of the index of the last such PCE (all four indices, with pseudo surround, in the first unit, replaced by a later one, kept by one without an index, in a unit the decoder rejects, between the elements; PCEs that are not the stream's own are refused by the port and have no entry), a PCE in the configuration (`channelConfiguration` 0) or only mono / stereo mixdown tags change nothing, ADTS with `channel_configuration` 0 or 6; the 7.1 and 5.0 matrices stay |
| `sgpd` | 30 | sample group descriptions in the sample table of an afconvert file: headers and entries that are too short (the open fails `'wht?'`), the grouping types `roll` (negative distances switch the edit-list priming on) and `prol`, versions 0 to 3, default length 0 / 2 / 4, counts that lie (1 to 6 missing entries fail the open, 7 or more are not read), several descriptions in one table, a cut `sbgp` |
| `smpb` | 37 | `iTunSMPB` valid-frame counts from 2^52 on (the length is `int64(double + 0.5)`) and above 2^63 - 1 (no `iTunSMPB`); the separators in front of the fields replaced by junk, digits, signs and white space (the fields are read at fixed places), a ninth digit of the priming, no white space at all |
| `cafpakt` | 9 | CAF packet-table valid-frame counts from 2^52 on: the same `int64(double + 0.5)` length |
| `short` | 21 | the size field of the `stbl`, `minf`, `mdia`, `trak` or `moov` atom of a three-chunk file shortened by 1 to 21 bytes, so that its last child (the chunk offsets) runs past it: the tables are bounded by the track, not by the container that is short (`trak -5` is replayed at 512 frames only: at 4096-frame reads Apple delivers the 6,080 frames) |
| `udta` | 13 | several `udta` atoms with an `iTunSMPB` each: the last usable tag counts; an empty `udta`, another item, a text of 0 valid frames or a `meta` without `mdir` later leave the earlier tag; two `meta` atoms or two items in one `udta` (the first decides) |
| `ldsc` | 12 | CAF layered descriptions: an HE-AAC (`aach` / `aacp`) first layer without a cookie (afconvert's HE files without their `kuki` among them), over a plain cookie (`'!dat'`), at other rates, channels and frames per packet; a chunk of 35 bytes is ignored; the first of two counts; none without `desc` |

Why the first 388 inputs did not catch the brand gate: 157 inputs of that pack carry an iTunSMPB tag and all but two have the brand `M4A ` (the exceptions are an `afconvert` `.m4b` file and a
mutation whose `ftyp` size field was damaged); the 20 inputs with another brand (`mp42`, `isom`, `iso5`, `3gp5`, `3g2a`, `qt  `) were written without a tag. No input had a tag under a brand
set that makes AudioToolbox ignore it, and none had its brand set varied while everything else stayed equal, so whatever rule the port had for the tag was never put against a file whose
only difference was the brand. The `brand` group is exactly that experiment.

## Read sizes

Apple's result depends on the read size when a read fails (the frames the failed read had gathered are lost, the converter's packet groups
shift) and in how much padding lands behind a short read. `readsizes.json` records, for 182 inputs, `decode_probe record --block N` at
N = 100, 513 and 4096, planar and interleaved: every input with a read error, every input with a short read in mid-stream, and a sample of clean ones for contrast.
653 of its 1092 recordings differ from the 512-frame one; 582 end in a read failure. `make_decode_fixtures.py readsizes` made the recordings of the first 90 inputs (it first
requires `decode_probe` at 512 frames to reproduce the manifest for every file and, for 30 clean inputs, that reads of 7, 576, 1000, 1024, 1025, 4096 and 100,000 frames
deliver the 512-frame stream: the replay's chunk-size test relies on it); `make_decode_rule_fixtures.py extend` added the others.

## Replay

    cargo test --offline --locked --features portable --test portable_aac_decode --test portable_aac_hostile

The same files run unchanged on macOS (`--features portable`), Linux arm64 and Linux x86_64. `portable_aac_decode.rs` asserts the pack's integrity and coverage (exact class counts,
pinned hashes), the recordings above (open outcome, properties, streams, tails), the read-size recordings, the refusal classes, the client-rate variants, the poisoning behaviour, chunked reads,
misuse errors, flush-to-zero independence and the file-name routing; two long cases (a 464 s ADTS stream and a 302 s MP4 with `moov` behind `mdat` and in front of it, both generated in the
test from silent access units whose Apple decode was recorded) are `#[ignore = "long: run by the release battery"]`, and measure the allocations of the decode, not the process.
`portable_aac_hostile.rs` mutates the small AAC-LC inputs and a fragmented MP4 6,000 times (seeded; byte and bit damage, truncation, insertion/removal/swap, splicing, forged headers
of every container, damaged box and chunk sizes, absurd table counts, noise over access units, ID3 chains; some cases opened with a client rate) and asserts no panic, a watchdog per
case on a thread of its own, the allocations of every case against a counting allocator (no single allocation and no total beyond what the input justifies), frames bounded by the input
size, refusals and failures only of the allowed kinds (typed refusals or statuses Apple produced for the fixtures), poisoning after a failure, equal outcomes when a case is run twice,
and an equal stream when it is read in other block sizes with flush-to-zero set; `scaled_inputs_cost_in_proportion` builds inputs of megabytes (a table of decoy tracks, a million sample-to-chunk
runs over a million chunks, an 8 MiB `ftyp`, a million edit-list entries, a fragment of a million samples, 8 MiB of LOAS, the 19-byte LOAS forgery) and asserts they cost time and memory in proportion to their size.

## Rebuilding

    python3 tools/portable/aac/make_decode_fixtures.py build --corpus <corpus dir> --probe <decode_probe> --work <scratch> --out <new dir>
    python3 tools/portable/aac/make_decode_fixtures.py readsizes --probe <decode_probe> --work <scratch> --dir <new dir>
    python3 tools/portable/aac/make_decode_rule_fixtures.py extend --probe <decode_probe> --work <scratch> --dir <new dir> --reader tests/fixtures/aac-reader
    python3 tools/portable/aac/make_decode_fixtures.py verify-pack --dir tests/fixtures/aac-decode      # any machine, no Apple, no corpora

`build` needs the generated corpora (`corpus-a`, `corpus-b`, `probes`) that tools outside this repository produced; `extend` needs nothing but this repository's files (`list` prints its inputs without a probe).
`decode_probe` is built with `cargo build --release --offline` in `tools/portable/aac/decode_probe` (macOS arm64; the SHA-256 of the probe that recorded each part is in
`manifest.json`: `apple.probe_sha256` for the first three parts, `apple.rule_probe_sha256` for the rule cases, parts 3 to 10). The generators are deterministic: seeded, `afconvert` and the writer's MP4 header times
pinned, and a second run reproduces every byte. `extend` also migrated the first parts' manifest once (the corpora got neutral names, `known_deviation` and the class `undetermined` were retired) and
adds only the cases the manifest does not have yet (the inputs it has must still be what the generators make).

## Limits

The reference is valid for the machine and OS build above only (decoder class G: stack residue 0 at the read entry). Generated audio exercises what the generators and the
mutation operators reach; it is evidence, not a proof for every possible file. Apple's MPEG audio reader hangs on at least one mutated input (an MP4 named `.mpa`); such inputs are not in the pack.
What AudioToolbox does with an `stsd` entry type it opens as another codec (`ac-3`, `ec-3`, `drms`, `samr`, `sawb`, `alac` without its cookie) is not part of the pack.
