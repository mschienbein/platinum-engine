//! Platform-independent replacements for Apple system components.
//!
//! Each implementation must reproduce the macOS arm64 system result bit for bit
//! for every input the engine can pass. Opt-in harnesses compare them with the
//! system originals on macOS; Linux builds use them in place of the originals.
#![allow(clippy::disallowed_methods)]

pub mod arm;
pub mod arm_scalar;
pub mod audio;
pub mod digest;
pub mod fft;
pub mod fft_sme;
pub mod foundation;
pub mod math;
pub mod vdsp;
pub mod vforce;
