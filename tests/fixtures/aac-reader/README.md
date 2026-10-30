# AAC reader unit-test inputs

Five tiny AAC-LC inputs of **generated** audio (a short synthetic signal; no recording of real music), used by the unit tests of the AAC reader
(`src/portable/aac/decoder/testutil.rs`, `src/portable/audio/aac_read_tests.rs`), by the container-rule tests that cut variants out of them
(`src/portable/aac/decoder/file/container_rules.rs`), by `tests/portable_aac_hostile.rs` and as the base of the `rule/` group of
`tests/fixtures/aac-decode/` (`tools/portable/aac/make_decode_rule_fixtures.py`).

| file | bytes | content |
|---|---:|---|
| `tiny-44100-2.m4a` | 7,105 | stereo, 44.1 kHz, 11 access units; `ftyp` (28 bytes, brands `M4A `, `mp42`, `isom`), `moov` (862 bytes, with the iTunSMPB tag: 8,820 valid frames), a 3,198-byte `free`, `mdat`; the layout the byte surgery of the tests relies on (asserted there) |
| `tiny-48000-1.m4a` | 5,458 | mono, 48 kHz, 12 access units, the same structure with an iTunSMPB tag |
| `tiny-44100-2.adts` | 3,086 | an ADTS stream of the same stereo signal |
| `tiny-44100-2.caf` | 7,105 | the same signal as a CAF file (`desc`, `kuki`, `pakt`, `data`) |
| `tiny-44100-2.loas` | 1,889 | the same signal as a LOAS/LATM stream (ten frames; the first carries the StreamMuxConfig) |

The signals are synthetic and the files hold no more than a few thousand bytes of audio data. Every behaviour the unit tests assert about them was recorded through AudioToolbox on macOS 27.2
(see the comments of the tests) and holds on every platform the portable reader runs on.
