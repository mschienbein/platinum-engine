# AAC encoder exactness fixtures

Golden data for `tests/portable_aac_encode.rs`. All audio is **generated** by a deterministic plain-Rust generator (below); no
recording of real music is included, and no audio file is stored: the generator source is
`tests/support/aac_encode_input.rs`, shared by the test and by the Apple recorder, so the float32 stream the recorder fed to
AudioToolbox is the one the replay feeds to the portable writer.

| file | content |
|---|---|
| `manifest.json` | per generated input: id, group, generator spec, SHA-256 of its float32 stream, and what the macOS reference `Writer` produced: SHA-256 and length of the complete `.m4a`, box order, access-unit count, first-AU and AU-stream hashes, per-AU sizes and 4-byte hashes (diagnostics), and for the smaller items the raw file's place in `refs.bin` |
| `refs.bin` | the kept Apple files, concatenated, each without its long runs of zero bytes (the `free` reservation Apple writes in front of `mdat`; the manifest lists the runs) |
| `refusals.json` | what `Writer::create` answered for 28 rates x 13 channel counts, what a failed, aborted or abandoned writer left behind, the recorder bit depths, create-time check order, destination paths and write misuse |

Counts, hashes and sizes are re-checked by `the_fixture_pack_is_intact_and_covers_what_it_claims` (and, without Apple or
Rust, by `python3 tools/portable/aac/make_encode_fixtures.py --verify`). The test also pins the SHA-256 of `manifest.json` and
`refusals.json` (`refs.bin` is pinned by the manifest) and the exact number of items per group, so that golden files regenerated
from the port itself, or a pack with items removed, fail it: a deliberate re-recording on macOS has to update those constants.

Two test targets use them. `tests/portable_aac_encode.rs` is what the engine reaches: every accepted configuration, every
content kind, every write size, the `create` classification of every rate and channel count, the bit depths, paths and misuse,
cleanup, and (`#[ignore]`d, for the release battery) the long items. `tests/portable_aac_encode_beyond_engine.rs` pins three
corners of AudioToolbox the engine cannot observe (a writer that carries a warning, fractional sample rates, subnormal input
under an ambient flush-to-zero mode), so that a deliberate difference is one decision in one place: the warned writer and the
flush-to-zero rule are reproduced; the fractional rates are **not reproduced** (the writer refuses them with
`CodecError::UnsupportedFormat`) and Apple's files for them are kept as the data that justifies the refusal. Both run through the
public portable `Writer` only and need no Apple component.

## What the reference is

macOS arm64 (the recorder prints and stores the exact OS build and CPU), through the engine's own `apple_codec::Writer`:
`ExtAudioFileCreateWithURL` (`m4af`, the `'aac '` file description with only rate and channels set), client format float32
interleaved at the same rate and channel count, `AudioConverterSetProperty('brat', 256000)`, `ExtAudioFileWrite` in 512-frame
blocks (the last one shorter) with the stack pinned (`call_with_sp_residue`, residue 0), `ExtAudioFileDispose` likewise pinned. The
engine's recorder bit depth is 0 (AAC output of a non-PCM source). `tools/portable/aac/encode_probe` drives it; nothing else
touches AudioToolbox.

MP4 outputs carry the wall-clock second of the close in `mvhd`, every `tkhd` and every `mdhd`. Every reference is recorded with
`set_mp4_header_times(path, 3,800,000,001)` and the replay normalises the portable output with the same constant through
`portable::audio::set_mp4_header_times`, so the comparison is on the whole file, byte for byte. `manifest.json` stores for each
item `m4a_sha256` and `m4a_length` of that normalised file; a replay that fails prints what differs as precisely as the stored
data allows (length, first differing byte and the box it is in, access-unit count, the first differing access unit).

The recorder was run three times over every item and the three recordings were byte-identical, the last in a process with 3 KB of
extra environment (the stack position that selects AudioToolbox's DFT kernel differs there). The write-size items were re-recorded
at every other write size and the flush-to-zero items inside the engine's flush-to-zero scope (below).

## The items

`tests/support/aac_encode_input.rs` uses only integer arithmetic and IEEE `+ - * / sqrt floor` on `f32`/`f64` (Rust never
contracts into FMA), casts, and a Taylor-polynomial sine of its own; it calls no libm function. A stream depends only on its spec
(`kind`, `rate`, `channels`, `frames`, `seed`, four integer `params`), never on how it is cut into blocks; the long items
are streamed with bounded memory. Randomness is xorshift64* seeded through a splitmix64 round, one stream per channel; a uniform
sample is `(next >> 40) * 2^-23 - 1` (exact). `params` are milli-units unless noted. 21 kinds:

| kind | params | content |
|---|---|---|
| `silence` | | digital zero |
| `square` | period, level | full-scale square, `level` high for half the period |
| `nyquist` | -, level | alternating +-level |
| `dc` | -, level (signed) | constant |
| `tone` | frequency in mHz, level | one sine, identical on every channel |
| `perchan` | base mHz, step mHz, level | a different sine on every channel (`base + c * step`), phase `c/8`, level falling with `c` |
| `sweep` | f0 Hz, f1 Hz, level | linear chirp over the whole item, phase `c/4` per channel |
| `white` | level | uniform noise, independent per channel |
| `bandnoise` | low-pass coefficient, high-pass coefficient (0 = none), level, gain | noise through two one-pole low-passes (and a high-pass) |
| `castanets` | period, decay, level, impulse flag | noise bursts with an exponential decay every `period` frames; flag 1: single-sample impulses (block switching) |
| `amnoise` | AM rate mHz, depth, level | noise with a sine amplitude envelope |
| `corr` | inter-channel correlation (signed), level | a common noise and an own one per channel; negative: odd channels inverted (M-only .. S-only) |
| `uncorr` | level | independent noise per channel with a different low-pass and level per channel (intensity / no M-S) |
| `clipped` | frequency mHz, gain | a sine times gain, hard-clipped to +-1 |
| `overrange` | amplitude (integer), tone flag | noise or a 1 kHz sine of amplitude up to 1,000,000 |
| `subnormal` | all-subnormal flag | cycles of 4096 frames: 2048 of random subnormal bit patterns, 1024 of 1e-30 noise, 1024 of a 1e-3 tone |
| `special` | bit mask, density | white noise with sparse +inf, -inf, quiet NaN, negative quiet NaN, NaN payloads and signalling NaNs (mask 1, 2, 4, 8, 16, 32, 64, 128) |
| `randbits` | | random finite float32 bit patterns (any exponent up to 254) |
| `burst` | start, end, level | silence, noise between two frames, silence |
| `lfe` | LFE channel index, level | one channel of 50 Hz tone and very low-passed noise, the others band-limited noise and a tone each |
| `mix` | level, base mHz | six partials at 1, 2, 3, 5, 8, 13 times the base with slow envelopes, a noise floor, a decaying noise transient every 5513 frames |

Groups (every group but `long` is replayed by the default tests):

| group | items | what |
|---|---:|---|
| `grid` | 34 | every accepted configuration once, 250 ms: 44.1 and 48 kHz x 1..8 channels, 32 kHz x 3..8, 24 and 22.05 kHz x 4..8, 16 kHz x 7..8 |
| `empty` | 34 | zero frames in each accepted configuration: the file is `ftyp` + `moov`, no access unit |
| `stereo44` | 33 | one item of every content kind (and the correlation variants) at 44.1 kHz stereo, 150 ms |
| `mono44` | 16 | a subset at 44.1 kHz mono |
| `stereo48`, `mono48` | 8, 3 | a subset at 48 kHz |
| `lengths` | 36 | 1, 2, 511, 512, 513, 959..961, 1023..1025, 1983..1985, 2047..2049, 2111..2113, 3007..3009 frames (access-unit boundaries are at `1024 k - 2112`; the encoder primes 2112 frames) at 44.1 kHz stereo, plus 13 short items in six other configurations |
| `mid` | 3 | 3 s: stereo `mix`, mono `castanets` at 48 kHz, 6-channel `lfe` at 48 kHz |
| `ten` | 2 | 10 s: stereo `mix` at 44.1 kHz, stereo `amnoise` at 48 kHz |
| `eightgroups` | 12 | 1,280 frames (four access units) of `castanets` in every accepted configuration with 15 short bands (22.05 and 24 kHz x 4..8 channels, 16 kHz x 7..8): the second attack, at frame 384, makes a short block of eight one-window groups, 8 x 15 = 120 bands. Added after the first recording, with `make_encode_fixtures.py --append` (see `reference.appended` in the manifest) |
| `long` | 7 | `#[ignore]`d: 330 s mono 44.1 kHz, 330 s stereo 48 kHz, 301 s 8-channel 44.1 kHz, and silence at the moov-placement edge (13,508 / 13,509 access units at 44.1 kHz, 14,540 / 14,541 at 48 kHz) |

A frame count `n > 0` needs `ceil((n + 2112) / 1024)` access units (AU 0 is the silent priming unit); the moov box moves behind
`mdat` (`ftyp free mdat moov`) once the sample tables outgrow the reservation Apple makes for 301 s. 12 items (`write_sizes`) are
additionally replayed with 1-, 7-, 513-, 4096-frame writes and one write (Apple's file is the same at every size); 7 (`fz_sha256`)
inside the engine's flush-to-zero scope: for the four items without subnormal samples Apple's file is unchanged, for the three
`subnormal` items it differs, because Apple flushes subnormal input when the caller's mode is set (an encoder fed input with its
subnormals flushed to zero reproduces Apple's flushed file exactly).

Budget: the default tests encode about 58 s of audio in total (48.3 s of items, the write-size and flush-to-zero replays on top;
`--verify` and the invariants test count it, limit 62 s), so they stay near a minute in a debug build on a hosted runner. The
`long` items are generated by the ignored test, never stored.

## Refusals (`refusals.json`)

`Writer::create` was recorded for rates `0, 1, 7350, 8000, 11025, 12000, 16000, 22050, 24000, 32000, 44100, 44100.5, 48000, 64000,
88200, 96000, 176400, 192000, 384000, NaN, +inf, -1` and six more (4,295,011,396 = 44100 + 2^32, 44099.99999, 44100.00001, 47999.999,
2^31, 1e300) against channel counts `0, 1..=10, 16, 64`: 364 cells. Each cell holds the exact classification the engine sees:

* `Err(InvalidInput)` with Apple-side message (rate not positive and finite, zero channels), checked before anything native;
* `Err(Native { operation, status })`: `ExtAudioFileCreateWithURL` or `ExtAudioFileSetProperty(client format)` with `'fmt?'`
  (1718449215);
* `Ok` with `warnings()`: `AacBitRateNotSet { status: 560226676 }` (`'!dat'`: 256 kbit/s is above the converter's maximum for that
  rate and channel count; the engine reports these as "configure Apple encoder" failures), or `Ok` with none (the 34 accepted
  configurations, and 32 fractional-rate cells: AudioToolbox accepts 44100.5, 44099.99999, 44100.00001 and 47999.999 at 1..8
  channels and encodes them differently from the integer rate: the 1100-frame probe file is 59,887 bytes at 44100.5 Hz and 59,963 at
  44100 Hz; the portable writer refuses these 32 cells with `UnsupportedFormat` and the tests assert, from these hashes, that every one
  of Apple's 1100-frame files differs from the nominal rate's file while the empty files are the same).

and what follows: a failed create, an `abort`, a writer dropped without `finish`, and a writer aborted after a write all leave their
directory empty (on Apple too); an empty finish and a 1100-frame write+finish are recorded with their file hashes. For a writer
that carries a warning Apple still finishes an empty file, fails the first write with the converter's `OSStatus`, poisons the
writer (the next write, and `finish`, fail) and reports that status from `abort` while removing everything.

`refusals.json` also records the recorder bit depth (0, 16, 24, 32 are accepted and change nothing; the other values are
`InvalidInput`), which check wins when several fail, destination paths (missing parent directory, empty path, NUL, `/`, a directory),
and write misuse (an empty write is a no-op, an odd sample count is refused as `InvalidInput` without poisoning the writer).

## Rebuilding

`tools/portable/aac/make_encode_fixtures.py --record --work DIR` (macOS arm64; DIR must not exist) builds
`tools/portable/aac/encode_probe`, records everything above as described and writes these files; `--verify` checks them anywhere.
`encode_probe crosscheck CORPUS_DIR RUNS_DIR` and `encode_probe layout DIR` compare the engine's `Writer` with an independently
recorded set of Apple encodes (raw float32 inputs plus Apple's `.m4a` and access-unit files); the maker requires every file to be
identical before it writes anything.

## Limits

The reference is valid for the machine and OS build recorded in `manifest.json` only. Generated audio exercises what the generator
reaches; it is evidence, not a proof for every possible input. Not covered: rates and channel counts Apple refuses or warns about
(only their `create` outcome), bit rates other than 256 kbit/s, and the stack-pin classes other than the engine's.
