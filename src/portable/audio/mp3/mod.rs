//! Bit-exact portable MP3 reader: what AudioToolbox's `ExtAudioFileRead` delivers for an MP3 file.
//!
//! The engine's `apple_codec::Reader` opens an MP3 with `ExtAudioFileOpenURL`, sets a float client format and reads
//! blocks of frames (512 in the engine). Everything that call chain does for MP3 is reproduced here by project-written
//! Rust that neither calls nor ships any Apple component:
//!
//! | layer | Apple (macOS 27.2, build 26B123, arm64e) | module |
//! |---|---|---|
//! | AudioFile container | `MPEGAudioFile` (sync search, ID3v2/ID3v1, Xing/Info/LAME/VBRI, packet table, priming/remainder, bitrate) in AudioToolboxCore | [`container`] |
//! | ExtAudioFile | priming trim, `FileLengthFrames`, the converter's packet-group reads for every read size, the `'bada'` / `'dta?'` read failures | [`gapless`], [`reader`] |
//! | AudioCodec | `ACMP3Decoder` (`AppendInputData` 0x5597c, `ProduceOutputPackets` 0x55b54) in `AudioCodecs.component` | [`codec`] |
//! | Spirit DSP decoder | `MP3DecoderWrapper_SpiritDSP::DecodeFrame` 0x13d768: Layer I/II/III integer decoder | [`decoder`], [`front12`], [`front3`], [`bitbuf`], [`back`], [`granule`], [`hybrid`], [`stereo`], [`synth`] |
//! | constant tables | the decoder's data (`_mp3d_g_huff`, window/DCT constants, ...) | [`tables`], [`back_tables`] |
//!
//! Addresses in the module documents are file addresses in the arm64e slice of
//! `/System/Library/Components/AudioCodecs.component/Contents/MacOS/AudioCodecs` (thin slice sha256
//! `70e5d4a56b24d037b940328916df496a20267eb206f8e316216ae7bef69a1626`) and, for the container layer, symbols of
//! AudioToolboxCore in the dyld shared cache. Names such as `Mp3XBack` or `Mp3XTables` in those documents are the
//! research work directories `local-artifacts/portable-parity/mp3-exact-02/<name>/` (each has a `REPORT.md`); the
//! decompilation, the lldb sessions and the generators of the data modules (`@generated` headers) are kept there.
//! That directory and the helper files named in comments (`NOTES.md`, `HUFFMAN.md`, `gen_tables.py`, `verify_tables.py`,
//! `tools/*.py`, `tools/rdseq.c`, `huff_ref.py`) are the maintainers' private research material and are not part of this
//! repository. The constant tables are numerically identical to data read from the Apple binary named above (see
//! `THIRD_PARTY_NOTICES`).
//!
//! The decoder is integer arithmetic only (wrapping, explicit `smull`/`asr` semantics): its output does not depend on
//! the host, on the floating-point mode or on the architecture. The only float is the final `i16 as f32 / 32768.0`,
//! exact for every `i16`, done by the caller ([`Reader`](super::Reader)).
//!
//! Entry point: [`reader::Mp3Reader`]. Verified against Apple with 0 mismatches on generated corpora (see
//! `tests/fixtures/mp3-exact/README.md` and `docs/VERIFICATION.md`).
//!
//! Contract: the same as AudioToolbox's `ExtAudioFileRead` for the same sequence of read sizes. The research compared
//! sizes 1 to 8192, odd sizes and mixed sequences with the live engine reader (`Mp3XContainer/REPORT.md` in
//! `local-artifacts/portable-parity/mp3-exact-02/`); the checked-in golden fixtures replay 100, 512, 513 and 4096
//! frames (`tests/portable_mp3.rs`). Not modelled: seeking, a client rate other than the file's, and reads from a reader that
//! was asked for its file length or bitrate first (AudioToolbox's length query scans the whole file and changes what
//! later reads see; the engine queries on a reader it never reads from).
//!
//! No `unsafe`, `std` only.
#![forbid(unsafe_code)]
// The modules transcribe machine code step by step, and the correspondence with the original is the point: clippy's
// style rewrites (iterator loops for indexed table walks, casts kept to show a register width, late initialisation,
// slice-copy loops) would obscure it and could only be re-verified against Apple's output, not against the source.
#![allow(
    clippy::needless_range_loop,
    clippy::unnecessary_cast,
    clippy::needless_late_init,
    clippy::type_complexity,
    clippy::manual_memcpy,
    clippy::explicit_counter_loop,
    clippy::while_let_loop,
    clippy::chunks_exact_to_as_chunks
)]

pub mod back;
pub mod back_tables;
pub mod bitbuf;
pub mod codec;
pub mod container;
pub mod decoder;
pub mod front12;
pub mod front3;
pub mod gapless;
pub mod granule;
pub mod hybrid;
pub mod reader;
pub mod stereo;
pub mod synth;
pub mod tables;
