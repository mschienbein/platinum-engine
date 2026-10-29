# MP3 exactness fixtures

Golden data for `tests/portable_mp3.rs`, `tests/portable_mp3_hostile.rs` and the granule test in
`src/portable/audio/mp3/back.rs`. All audio is **generated** (LAME and ffmpeg encodes of synthetic signals, hand-built
and mutated MPEG streams); no recording of real music is included.

| file | content |
|---|---|
| `pack.bin` | 315 input files, concatenated (1,971,938 bytes, each ≤ 40,000 bytes, the smallest is empty) |
| `manifest.json` | per input: id, offset/length/SHA-256, selection features, and what the macOS reference did with it at 512-frame reads |
| `readsizes.json` | the same recordings at 100-, 513- and 4096-frame reads, planar and interleaved, for 63 of the inputs (see below) |
| `back_golden.cases`, `back_golden.dump` | 40 decoder-state snapshots recorded from Apple's `DecodeFrame` (see below) |
| `admission.json` | 500 recorded file-type admission cases (generated content x file name, 43,116 bytes): what AudioToolbox answered on macOS for each, and what the portable `Reader` must answer (`same`, or the kind of typed refusal for content AudioToolbox decodes and this port does not); the recipes rebuild the inputs; used by `file_type_admission_follows_audiofile_for_every_recorded_name_and_content` |
| `tails.json` | 188 recordings (62 pack members, several block sizes and both shapes, 19,705 bytes) of what `ExtAudioFileRead` leaves in the caller's buffer behind the frames it returns (the decoder's padding frames); used by `the_decoders_padding_behind_the_valid_frames_lands_in_the_callers_buffer_like_on_macos` |

Hashes: `pack.bin` `f3cdf49c3ac0e259673ce81f4c5d7e3f2884781adf2f9ee52e950b8104d0c02f`, `manifest.json`
`9aadb7daac64ad66f3768eea7c37527875927c81e915248bd2a368711731ff54`, `readsizes.json`
`a5ed3f8efcd74a31d18b01ebd371951395e9b39086a8856ca16acfda52f261c6` (the manifest also pins the pack's SHA-256 and
every input's, `readsizes.json` pins the pack and the inputs it covers;
`the_fixture_pack_is_intact_and_covers_what_it_claims` and `read_sizes_other_than_512_match_the_apple_recordings`
re-check them).

## What the reference is

For every input the macOS arm64 reference (macOS 27.2, build 26B123, Apple M4) is the engine's own
`apple_codec::Reader` (AudioToolbox `ExtAudioFileOpenURL` / `ExtAudioFileRead`), driven by `apple_probe` the way the
engine drives it: client format at the source rate, 512-frame reads, planar stereo (mono duplicated) for analysis, or
interleaved at the source channel count. The manifest stores, per input, only values copied from those runs:

* `open`: accepted, or refused with the failing call (`ExtAudioFileOpenURL` or `ExtAudioFileSetProperty(client format)`)
  and its signed `OSStatus` (`'dta?'` 0x6474613f, `'wht?'` 0x7768743f, `-39`, `'fmt?'` 0x666d743f);
* `info`: source rate, format id (`.mp1`/`.mp2`/`.mp3`), frames per packet, channels, the file-length property and the
  AudioFile bitrate;
* `planar512` and `inter512`: frames delivered, the read failure (`'bada'` 0x62616461 or `'dta?'`, with the operation name)
  if one ended the stream (the frames of the failed read are not delivered), and the SHA-256 of the delivered float32
  stream (little endian; planar: L,R per frame).

The test decodes each input through the public `Reader` and compares everything bit for bit. No Apple component is
involved when the tests run.

## Provenance and rebuilding

The inputs are a selection from three corpora built by the MP3 exactness research (evidence in
`local-artifacts/portable-parity/mp3-exact-02/`; each agent directory has a `REPORT.md`):

| set | files in the corpus | generator and reference | in this pack |
|---|---:|---|---:|
| `corpus` | 7,100 | `Mp3XCorpus/gen_corpus.py` (+ `gc_*.py`), reference `Mp3XCorpus/ref_corpus.py`; corpus manifest SHA-256 `c5a608fcd7a10795001bffb5fb30de7d801eb00549f446aa40256ab1db750da3`, `reference.json` SHA-256 `defb0e7ca02826c9530c3e9f87e4be346987dbf4e2ef6327519e1f63e0a92b80` | 196 |
| `fuzz` | 2,300 | `Mp3XFuzz/gen_fuzz.py`, `reference.json` SHA-256 `1a56d64b6d3c11934fe4f0181ce6822cfb488eda386fc211ca91134951d60e46` | 45 |
| `held` | 6,440 | `fz9001_*`: the `Mp3XFuzz` profiles with a fresh seed; `lame9002`: `lead/tools/gen_held_lame.py`; `m7001`/`m7002`: byte mutants of corpus streams emitted by `lead/tools/emit_mutants.rs`; `reference.json` SHA-256 `38052562578919af3ce5fc1dee6b739375ea83a025f32fa04d8efc9202a220f4` | 74 |

The `apple_probe` binary used for all three has SHA-256 `0fa83568aeb4807eb76db54c88355551d00dbfe96f39364775b5729e104e9f73`.
`manifest.json` lists every selected input as `<set>/<family>/<name>` with its selection features; the selection
(greedy cover of the rarest decoder features, then a per-family quota; `Mp3XIntegrate/fixtures/select_fixtures.py` and
`COVERAGE.md` in the evidence directory) covers every family of every corpus except `long` (all files above 40 KB), all
nine sample rates, MPEG-1/2/2.5, Layers I/II/III, every channel mode, block types including mixed blocks, intensity
stereo, free-format, tags (ID3v1/v2, APE, Lyrics3, Xing/Info/VBRI, LAME gapless fields), damage, refusals of every
status and read failures of both statuses.

Left out on purpose: corpus `foreign_format` files such as a WAVE named `.mp3` (`admission.json` records them: a name
ending `.mp3` commits to the MPEG reader). The 300 files of the first selection came from `select_fixtures.py` (`Mp3XIntegrate/fixtures/` in
the evidence directory has the selection and `COVERAGE.md`); 15 were added afterwards for the read-size recordings: the
byte mutant `held/m7001/m02287` (a Layer I stream whose first read fails with `'dta?'`; an earlier port version
returned `'bada'`, the reason it had been left out) and 14 more streams whose Apple result depends on the read size
(`held/m7001/{m00321,m00548,m02055}`, `held/m7002/{m00352,m02294}`, `corpus/stream_change/{d10_to_x8_notag,
d3_to_x22m_tag_a,x11_to_x22_notag,x8_to_x11_tag_a}`, `corpus/bad_frames/d3_all_payload_random`,
`corpus/hdr_patch/d3_all_mode_stereo`, `corpus/sideinfo_mut/ds3_bigv_511`, `corpus/layers/L2_v2_sr22050_m3_rand`,
`corpus/id3_in_stream/id3v24_fakesync_at10`). Their manifest entries were derived by `make_fixtures.py --update`
from an id list and carry the feature `blocksize_sensitive`.

## Read sizes

Apple's result depends on the read size when a read fails: the frames the failed read had gathered are lost and the
converter's packet-group reads shift, so the number of frames delivered before an error and sometimes the status itself
change (254 of the 15,840 corpus files differ between 512 and 4096 frames; all of them end in a read failure).
`readsizes.json` records, for 63 inputs, `apple_probe dump FILE OUT planar|inter BLOCK` at BLOCK = 100, 513 and 4096:
every input with a read error, every `stream_change` and `blocksize_sensitive` input, and every 9th other input for
contrast (30 of the 63 end cleanly). 116 of its 378 recordings differ from the 512-frame one; 198 end in a read
failure. `tools/portable/mp3/record_readsizes.py` makes it on macOS (it first requires `apple_probe` at 512 frames to
reproduce the manifest for every selected input, and the file is byte-identical across runs) and checks it against
the pack and manifest on any machine (`--verify`). The harness is the engine's: reads of BLOCK frames until one returns
0 or fails; a short read is not the end.

`tools/portable/mp3/make_fixtures.py` rebuilds `pack.bin` and `manifest.json` from the id list in the manifest and the
corpora (and refuses to write if any derived fact differs), or checks a pack against its manifest without them:

    python3 tools/portable/mp3/make_fixtures.py --verify-pack tests/fixtures/mp3-exact/pack.bin tests/fixtures/mp3-exact/manifest.json

## Granule snapshots

`back_golden.cases` / `back_golden.dump` hold 40 granules: the decoder state (spectrum, overlap, polyphase ring) fed into
Apple's `DecodeFrame` back half through lldb and read back, with Apple's PCM. They cover all MPEG-1 block types, M/S and
intensity stereo, MPEG-1 mono, MPEG-2 intensity, MPEG-2.5 mixed blocks and overflowing spectra, and are replayed by
`portable::audio::mp3::back::golden::matches_apple_recorded_granules`.

## Limits

The reference is valid for the machine and OS build above only. Generated audio exercises what the generators and the
mutation operators reach; it is evidence, not a proof for every possible file.
