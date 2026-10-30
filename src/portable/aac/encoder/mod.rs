//! Apple's AAC-LC encoder for the engine's configuration, reproduced bit for bit: PCM in, the access units (and the `.m4a` around them) AudioToolbox
//! produces out.
//!
//! ```text
//! let mut enc = AacEncoder::new(44100, 2)?;           // Err(ConfigError) for every format `accepts` refuses
//! enc.push_interleaved(&block)?;                      // any number of whole frames
//! enc.drain_access_units(|au| spool.write_all(au))?;  // after every push: memory stays bounded however long the stream is
//! enc.finish()?;                                      // zero-padded tail + flush blocks
//! enc.drain_access_units(|au| spool.write_all(au))?;
//! m4a::write_file(&mut out, &mut spool, &sizes, 44100, 2, frames_pushed, clock)?;
//! ```
//!
//! A failed block ends the stream: the encoder then answers every push and `finish` with [`EncodeError::Failed`].
//!
//! # Verified scope
//!
//! What is reproduced is the encoder the engine drives on macOS: an `ExtAudioFile` `m4af` file with the `'aac '` file format (sample rate and channel
//! count only), a float32 client format, `'brat'` 256000 and the defaults `'cdqu'` 0x40 and `'acbf'` 1 (ABR), written in the "write class A" call context
//! (`apple_codec`'s v0.2.0 call-stack pin, `sp % 64 == 0` at `ExtAudioFileWrite`) with the generic tail policy (the dispose-time drain uses the generic
//! 512-point DFT of [`crate::portable::dft`], never the SME kernel). In that configuration the access units depend on the sample sequence only, not on how
//! it is cut into writes, and the file is byte-identical to AudioToolbox's when the close-time clock is the same. The checked-in fixtures
//! (`tests/fixtures/aac-encode`) compare whole files with Apple's for generated inputs: 21 kinds of content (tones, noise, transients, full-scale,
//! subnormal and NaN input, ...) in every accepted configuration - 44.1/48 kHz x 1..8 channels, 32 kHz x 3..8, 24/22.05 kHz x 4..8 and 16 kHz x 7..8 -,
//! lengths around every access-unit boundary, items of up to ten seconds in the default tests and of 301 to 330 seconds in the ignored long ones (which
//! move the `moov` box behind `mdat`), and the empty file of every configuration.
//!
//! Not covered (the encoder has no knob for them, and a format outside the verified set is a [`ConfigError`]): any other `'brat'`, `'cdqu'` or `'acbf'`
//! (the converter's default bitrate and the VBR/CBR modes), the HE-AAC and ELD encoders, the write alignment "class B" (a stack position at the write
//! call that switches AudioToolbox's 512-point DFT to an SME kernel), sample rates other than the nine exact AAC rates (AudioToolbox serves rates within
//! 10 Hz of them but its output depends on the exact rate), and files whose offsets need 64 bits.
//!
//! # Floating-point mode
//!
//! The arithmetic always runs with flush-to-zero cleared (gradual underflow), whatever the mode of the caller. The one effect of the caller's mode that
//! AudioToolbox shows is on the input: with the mode set while samples are written (FPCR.FZ on arm64, MXCSR FTZ/DAZ on x86_64), its input path flushes
//! subnormal samples to zero, keeping the sign. [`AacEncoder::push_interleaved`] reproduces that: it reads the mode before clearing it and replaces the
//! subnormal samples of that push by +-0; with the mode clear (the engine writes outside its flush scopes) it copies and changes nothing. Pushes made in
//! different modes are flushed push by push; only a whole stream pushed in one mode is compared with Apple.
//!
//! # Determinism
//!
//! The numeric code uses only the Apple-exact primitives of [`crate::portable::arm`], [`crate::portable::math`], [`crate::portable::vdsp`] and
//! [`crate::portable::dft`] (explicit fused multiply-adds, correctly rounded libm replacements); it has no host-dependent branch, so its output is meant
//! to be the same on macOS arm64, Linux arm64 and Linux x86_64: the fixture replays run on each of them in CI. A release build encodes about 20 to 30
//! times faster than real time per core on an arm64 laptop (measured with the long fixture items and a larger set of generated inputs).
//!
//! The numeric constants and algorithms were recovered from Apple's AudioToolbox binaries on one reference machine by static analysis, instruction-level
//! transcription and recorded runtime states; the research material is private and not in the repository; no Apple binary or source code is included; no
//! legal clearance is claimed.

#![forbid(unsafe_code)]

mod config;
mod front;
pub mod m4a;
mod stream;

pub use config::{accepts, nominal_rate, ConfigError, MAX_CHANNELS};
#[cfg(test)]
pub use config::NOMINAL_RATES;
pub use stream::{AacEncoder, EncodeError};
