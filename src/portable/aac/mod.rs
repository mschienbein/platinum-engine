//! Pure-Rust AAC-LC encoder and decoder that reproduce Apple's AudioToolbox results bit for bit on every platform.
//!
//! # What is reproduced
//!
//! * **Encoder** ([`encoder`]): Apple's AAC-LC encoder for the engine's configuration - an `ExtAudioFile` `m4af` file with the `'aac '` file format,
//!   a float32 client format, `'brat'` 256000, `'cdqu'` 0x40 and `'acbf'` 1 (ABR) - in the "write class A" call context (the v0.2.0 call-stack pin of
//!   `apple_codec`) with the generic tail policy (the generic 512-point DFT of [`crate::portable::dft`]). It returns the same access units, and with
//!   [`encoder::m4a`] the same `.m4a` bytes, as AudioToolbox for 44.1/48 kHz x 1..8 channels, 32 kHz x 3..8, 24/22.05 kHz x 4..8 and 16 kHz x 7..8
//!   (the formats AudioToolbox takes at 256 kbit/s, at their exact sample rates); every other format is a typed refusal, and other `'brat'`/`'cdqu'`/`'acbf'`
//!   values, HE-AAC encoders and write class B are not covered (the encoder has no knob for them). See the [`encoder`] documentation for the verified scope.
//! * **Decoder** ([`decoder`]): Apple's AAC-LC decoder as `ExtAudioFileOpenURL` + `ExtAudioFileRead` deliver it for the client formats the engine uses
//!   (float32 interleaved at the source channel count, and planar stereo with the converter's mix-down) from MP4 / M4A, ADTS, LOAS / LATM and CAF
//!   input, in the generic-DFT output class "G" its read calls reach. HE-AAC, AAC-LD / ELD and the other streams it does not cover are refused with a
//!   typed error. It is wired into [`crate::portable::audio::Reader`].
//!
//! Both were reconstructed by observing and analysing the behaviour of Apple's AudioToolbox on macOS arm64 with generated audio: numeric constants and
//! algorithms were recovered from Apple's AudioToolbox binaries on one reference machine by static analysis, instruction-level transcription and
//! recorded runtime states. The research material behind this work (decompilations, runtime captures and the generators of the data tables) is the
//! maintainers' private material and is not part of this repository; the constant tables are numerically identical to data read from the Apple binaries
//! (see `THIRD_PARTY_NOTICES`). There is no affiliation with Apple; no Apple binary or source code is included; no legal clearance is claimed.
//!
//! # Stage map
//!
//! The modules follow the stages of the original, one module per stage:
//!
//! ```text
//! encoder:  fb -> psy -> ms / int -> quant -> bits -> mp4
//!           (filterbank, block switching, TNS; psychoacoustic model; M/S and intensity stereo; quantiser, scalefactors,
//!            rate control; access-unit syntax; .m4a container)
//! decoder:  decoder::bits -> decoder::synth -> decoder::file
//!           (access unit -> spectrum; IMDCT, windowing, overlap-add; containers, priming/trim accounting, client conversion)
//! ```
//!
//! [`types`] holds the plain structs the encoder stages exchange; the Apple-exact floating-point primitives come from
//! [`crate::portable::arm`], [`crate::portable::math`], [`crate::portable::vdsp`] and [`crate::portable::dft`].
//!
//! # Floating-point mode
//!
//! Both directions are computed with gradual underflow, i.e. flush-to-zero cleared. Their entry points clear it for the call with
//! [`crate::fp_mode::without_flush_to_zero`], so an enclosing [`crate::fp_mode::with_flush_to_zero`] scope does not change their arithmetic. The one
//! effect of the caller's mode that AudioToolbox itself shows, the flushing of subnormal *input* samples of an encoder write, is reproduced by the
//! encoder explicitly (see [`encoder`]).
//!
//! # Licensing note
//!
//! AAC is a patented format; patent licensing may apply to encoders and decoders in some jurisdictions.

pub mod bits;
pub mod decoder;
pub mod encoder;
pub mod fb;
pub mod int;
pub mod mp4;
pub mod ms;
pub mod psy;
pub mod quant;
pub mod types;
