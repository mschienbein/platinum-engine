//! decoder::bits: bit-exact pure-Rust port of the first half of Apple's AAC-LC decoder (AudioToolbox, macOS 27.2): from the bytes of an access
//! unit (`raw_data_block`) to the per-channel `float32[1024]` spectrum that `AACDecoder::DecodeFrame` hands to the inverse MDCT
//! (`FrequencyToTimeInPlace`), together with the window sequence / shape of every channel.
//!
//! ```ignore
//! let cfg = SpectralConfig::new(sfi, channel_config, pce)?;
//! let mut dec = SpectralDecoder::new(cfg);
//! dec.decode_au_into(bytes, &mut au);                  // au.status == 0: au.channels in F2T-counter (element list) order
//! ```
//!
//! Float semantics: IEEE f32 everywhere, `f32::mul_add` at exactly three sites (PNS energy, TNS step-up, TNS AR filter), no libm in the
//! hot path (finite embedded tables), gradual underflow (= FPCR.FZ 0: TNS decay tails do produce subnormals); every NaN the PNS and TNS stages
//! produce is the ARM default NaN.  All finite results are identical on aarch64 and x86_64; the NaN bits of a spectrum that overflowed to infinity
//! are ARM's on aarch64 (both signs of NaN occur downstream) and the host's elsewhere, see `decoder::synth`.
// `f32::mul_add` exactly at the three documented fused sites (the engine-level clippy.toml asks for
// `ArmFloat::arm_mul_add`; identical values).  The index loops mirror Apple's group / band structure.
#![allow(clippy::disallowed_methods, clippy::needless_range_loop, clippy::manual_div_ceil, clippy::collapsible_if)]
#![cfg_attr(not(test), forbid(unsafe_code))]
#![cfg_attr(test, deny(unsafe_code))]

mod bitreader;
mod config;
mod decoder;
mod huffman;
mod ics;
mod stages;
pub mod tables;

pub use config::{ConfigError, PceLists, SpectralConfig};
pub use decoder::SpectralDecoder;

/// Reason an access unit was dropped.  Apple's `Deserialize` status is `-1` for every one of them ([`AuError::status`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AuError {
    /// Explicit-rate ASC without an SFB table (rates above 96 kHz): every read fails.
    NoSfbTable,
    /// `ID_END` reached with a different number of decoded list elements ("Unmatched number of channel elements in payload").
    UnmatchedElementCount,
    /// Fewer than 3 bits left after an element, or a FIL / DSE payload not fully inside the access unit ("Too few bits left").
    TooFewBitsLeft,
    /// `max_sfb` above the number of scalefactor bands of the window type.
    MaxSfbExceeded,
    /// `predictor_data_present` in a long ICS (no Main-profile prediction / LTP).
    PredictionPresent,
    SectionLengthZero,
    SectionLengthExceedsMaxSfb,
    /// Codebook 14/15 in an SCE, LFE or the left channel of a CPE.
    IntensityInLeftChannel,
    /// Codebook 12 (FairPlay; no cryptor attached).
    Codebook12,
    PulseInShortWindow,
    /// `pulse start_sfb >= num_swb`.
    PulseStartSfb,
    TnsLengthZero,
    /// `gain_control_data_present == 1`.
    GainControl,
    /// `ms_mask_present == 3` (with `max_sfb != 0`).
    MsMaskPresent3,
}

impl AuError {
    /// The status Apple's `AACDecoder::Deserialize` returns.
    pub fn status(self) -> i32 {
        -1
    }
}

impl std::fmt::Display for AuError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "access unit rejected ({self:?})")
    }
}

impl std::error::Error for AuError {}

/// One channel of a decoded access unit.
#[derive(Clone, Debug, PartialEq)]
pub struct ChannelSpectrum {
    /// Spectrum handed to the IMDCT.  Long / start / stop: 1024 lines; EIGHT_SHORT: 8 consecutive 128-line windows (de-grouped,
    /// window-major).  Lines above the coded bandwidth and the zero lines of the dequantiser are `+0.0`; other zeros may be `-0.0` and
    /// must be preserved.  Scale: `gain * |q|^(4/3)` with `gain = 2^((sf-100)/4)` (no extra normalisation).
    pub coeffs: [f32; 1024],
    /// `ICSInfo.byte[10] & 3`: 0 ONLY_LONG, 1 LONG_START, 2 EIGHT_SHORT, 3 LONG_STOP (shared by both channels of a common-window CPE).
    pub window_sequence: u8,
    /// `(ICSInfo.byte[10] >> 2) & 1`: 0 sine, 1 KBD (a non-common CPE has independent infos per channel).
    pub window_shape: u8,
}

/// Result of one access unit.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct AuResult {
    /// 0 = success; otherwise the access unit is dropped (Apple: 0 output frames, no F2T call at all, no concealment).
    pub status: i32,
    pub error: Option<AuError>,
    /// One entry per output channel in F2T-counter order (= element-list order of `CreateList`, CPE: left then right); empty on error.
    pub channels: Vec<ChannelSpectrum>,
}
