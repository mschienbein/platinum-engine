//! bits: AAC-LC `raw_data_block` writer matching AudioToolbox's `AACEncoder::WriteBitstream` for the
//! engine configuration (AOT 2, SCE/CPE/LFE, ABR without reservoir => no fill/stuffing, no pulse, no gain control, no PNS).
//!
//! Pure integer logic.  The writer consumes the plain structs of `types` (`AccessUnit`, `Element`, `ChannelStream`); it is checked by the
//! whole-file comparisons of the encoder fixtures (`tests/portable_aac_encode.rs`), which compare every access unit with Apple's.
//!
//! Spectrum layout contract (`ChannelStream::q`): coded order, group `g` starts at line `win0*128` (`win0` = windows in the preceding groups), band
//! `sfb` of that group at `+ swb[sfb]*L` (`L` = windows in the group), window `w` of the band at `+ w*width + k`; for long blocks simply `q[line]`.
#![forbid(unsafe_code)]
#![allow(clippy::all)]

pub mod tables;
mod bitwriter;
mod pce;
mod write;

pub use bitwriter::BitWriter;
pub use pce::{make_pce, write_pce_asc_bits};
pub use write::{WriteError, grouping_bits, write_access_unit};

/// MPEG-4 `sampling_frequency_index` 0..=12 as sample rates (ISO/IEC 14496-3 table 1.16): the one copy of the table.
pub const SAMPLE_RATES: [u32; 13] = [96000, 88200, 64000, 48000, 44100, 32000, 24000, 22050, 16000, 12000, 11025, 8000, 7350];

/// MPEG-4 sampling-frequency index of `rate` (same table as the ASC).
pub fn sf_index_of_rate(rate: u32) -> Option<u8> {
    SAMPLE_RATES.iter().position(|&r| r == rate).map(|i| i as u8)
}
