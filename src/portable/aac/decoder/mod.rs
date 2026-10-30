//! Apple's AAC-LC decoder as `ExtAudioFileRead` delivers it, reproduced bit for bit in Rust (macOS 27.2 arm64, the generic-DFT output class):
//! containers and read semantics (`file`), access unit -> spectrum (`bits`) and spectrum -> time blocks (`synth`), put together in [`AacDecoder`].
//!
//! The client formats are the two the engine uses: interleaved float32 at the source channel count, and planar float32 stereo (the processing
//! unit's read; a multichannel source is mixed down by AudioToolbox's converter, reproduced in `file::mixer`).  Streams AudioToolbox decodes that
//! are not covered (HE-AAC, LD / ELD, MPEG-1 audio object types, malformed files whose behaviour depends on its input prefetch) are refused with
//! [`Unsupported`], never decoded differently.
//!
//! A file is decoded from memory: [`AacDecoder::open`] takes the whole file image (`portable::audio` reads files of up to 2 GiB into one buffer), the packet table
//! holds 16 bytes per packet (at most 2^24 packets), and no table is sized from a field of the file alone.
//!
//! Numeric constants and algorithms were recovered from Apple's AudioToolbox binaries on one reference machine by static analysis, instruction-level
//! transcription and recorded runtime states; no Apple binary or source code is included, and no legal clearance is claimed.
//!
//! ```ignore
//! let mut dec = AacDecoder::open(std::fs::read("a.m4a")?, Some("m4a"))?;
//! dec.set_stereo_planar()?;
//! let (mut l, mut r) = (vec![0f32; 512], vec![0f32; 512]);
//! while dec.read_stereo_planar(512, &mut l, &mut r)?.frames > 0 { /* ... */ }
//! ```
#![deny(unsafe_code)]
#![allow(clippy::all)]

mod au;
mod bits;
mod decoder;
mod elements;
mod error;
mod file;
mod synth;
#[cfg(test)]
pub(crate) mod testutil;

pub(crate) use decoder::AacDecoder;
pub(crate) use error::{DecodeError, ReadFailure, Unsupported};
pub(crate) use file::mp4::first_sound_entry;
pub(crate) use file::{AacFile, ClientError, MAX_TAG_BLOCKS, OpenError, Unspecified};
