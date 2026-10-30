//! Shared plain-data contracts between the stages of the AAC encoder.
//!
//! This module has no dependencies and no logic beyond constructors. Layouts mirror Apple's own objects at the function boundaries the stages
//! exchange: `psyOut`, the TNS channel and the access-unit syntax elements.
#![forbid(unsafe_code)]
#![allow(clippy::all)]

pub const N_LONG: usize = 1024;
/// Maximum number of scalefactor bands in any frame (long 49; short = groups * bands per window: up to 8 * 15 = 120 at 16, 22.05 and 24 kHz, 8 * 14 = 112
/// at the other rates); Apple arrays hold 0x78 entries.
pub const MAX_BANDS: usize = 120;

/// Float literals that several stages read (the original keeps one copy per use site; the stages name the address they take theirs from).
/// `1.0e30f`: initial ring of the block switch, running minima of the PAM, "huge" of the intensity stage.
pub const F_1E30: f32 = f32::from_bits(0x7149f2ca);
/// `1.0e-20f`: added to powers before square roots.
pub const F_1E_20: f32 = f32::from_bits(0x1e3ce508);
/// `0.001f`.
pub const F_0_001: f32 = f32::from_bits(0x3a83126f);
/// `0.1f`.
pub const F_0_1: f32 = f32::from_bits(0x3dcccccd);
/// log2(10) / 10 = 0.33219281 (`DAT_009226fc`): dB -> exponent of 2.
pub const K_DB_TO_LOG2: f32 = f32::from_bits(0x3eaa152d);

/// One TNS filter exactly as coded in the bitstream (`direction` = 1 always for Apple; `coef_bits` is 3 or 4 from coef_res/compress).
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct TnsFilter {
    pub length: u8,
    pub order: u8,
    pub direction: u8,
    pub coef_compress: u8,
    /// coded (unsigned, `coef_bits` wide) coefficient values as written, first `order` valid.
    pub coefs: [u8; 12],
}

/// TNS data of one window (long frames: window 0 only; EIGHT_SHORT: up to 8 windows). The engine uses at most one filter per window.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct TnsWindow {
    pub n_filt: u8,
    pub coef_res: u8,
    pub filter: TnsFilter,
}

/// Per-filter sfb ranges Apple keeps in its TNS struct (`tns+0x1c+0xe0*w+0x30*f`: `{start_sfb, end_sfb}`; `end` = top sfb of the filter, `start = end - length`).
/// Consumed by the intensity-stereo stage (`int`), produced by the TNS stage (`fb`).
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct TnsRanges {
    /// byte 0 of the TNS struct: `== 1` (TNS analysis produced a filter this frame).
    pub present: bool,
    /// filters per window (analysis honours at most 3; the engine produces <= 1).
    pub count: [i32; 8],
    pub range: [[(i32, i32); 3]; 8],
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct TnsChannel {
    /// byte 0 of Apple's TNS channel struct: TNS analysis ran and produced at least one accepted filter in this frame.
    pub active: bool,
    pub windows: [TnsWindow; 8],
    /// Apple keeps further analysis state in this struct (weights at +0x790, copies of spectra...): the filterbank stage appends what the PAM/quantizer need.
    /// `weights` follows Apple's tns+0x790 layout (long: `[k]`; EIGHT_SHORT: window `w` at `w*115 + k`); the PAM's E7 TNS-gain scaling reads it.
    pub weights: Vec<f32>,
    /// per-filter sfb ranges (see [`TnsRanges`]), used by the `int` stage.
    pub ranges: TnsRanges,
}

/// Intensity-stereo data attached to a CPE's psyOut#1 (`psyOut+0xbc8`): per band flag + position.
#[derive(Clone, Debug, PartialEq)]
pub struct IntensityData {
    pub flag: [u8; MAX_BANDS],
    pub position: [i32; MAX_BANDS],
}
impl Default for IntensityData {
    fn default() -> Self {
        Self { flag: [0; MAX_BANDS], position: [0; MAX_BANDS] }
    }
}

/// Mirror of Apple's `psyOut` (0xbd8 bytes) restricted to the fields that cross stage boundaries. One per channel and per slot
/// (#1 = L/R slot read by the quantizer, #2 = M/S candidate slot).
#[derive(Clone, Debug)]
pub struct PsyOut {
    /// +0x000: spectrum used by the quantizer, 1024 floats (EIGHT_SHORT: after `_sortShortSpectrum`, i.e. group-sorted order).
    pub spectrum: Vec<f32>,
    /// +0x018: band energies sum x^2.
    pub energy: [f32; MAX_BANDS],
    /// +0x1f8: masking thresholds in the quantizer's power domain.
    pub thr: [f32; MAX_BANDS],
    /// +0x3d8: perceptual entropy per band.
    pub pe: [f32; MAX_BANDS],
    /// +0x5b8: channel is part of a pair that went through the pair path.
    pub is_pair: bool,
    /// +0x5bc (= PsyChannel+0x23c): carried through from the psy stage; its meaning in Apple's code is not established.
    pub field_5bc: i32,
    /// +0x5c0: window sequence.
    pub wseq: u32,
    /// +0x5c4: lines per window; 1024 for BOTH long and EIGHT_SHORT frames (the value comes from the long sfbinfo N).
    pub lines_per_window: u32,
    /// +0x5c8: total number of bands (49 long; groups * 14 or 15 short).
    pub total_bands: u32,
    /// +0x5cc: bands per window group (49 long; 14 or 15 short); sf/mask arrays are indexed `g * nsfb_per_group + b`.
    pub nsfb_per_group: u32,
    /// +0x5d0: max_sfb (`_calculateMaxSfb`; 0 for silent frames).
    pub max_sfb: u32,
    /// +0x5d4: number of window groups (1 long; 1..8 short).
    pub ngroups: u32,
    /// +0x5d8: group lengths (windows per group).
    pub group_len: [i32; 8],
    /// +0x5f8: band widths in lines (short: group_len * sfb width).
    pub width: [i32; MAX_BANDS],
    /// +0x7d8: band start lines (cumulative). One entry more than the other band arrays: `start[total_bands]` is the end of the last band, and for 8 groups
    /// of 15 bands it is entry 120. Apple's array holds 0x78 entries and stores that one in the 0x18 bytes of padding behind it (`+0x9b8`).
    pub start: [i32; MAX_BANDS + 1],
    /// +0x9d0: NOT 'is short': `_AdvancePsyOut` param_8, 1 in every long and short frame; the `int` stage reads its own `ChannelLines::psy_spectrum_flag`, do not derive block type from it.
    pub is_short: bool,
    /// +0x9d4: per band flags: bit0 M/S used; after intensity: ch0 (f & 7) == 2 intensity source band, ch1 |= 0x20 intensity-coded band.
    pub flags: [i32; MAX_BANDS],
    /// +0xbc0
    pub tns: Option<TnsChannel>,
    /// +0xbc8
    pub intensity: Option<IntensityData>,
}

impl PsyOut {
    pub fn new() -> Self {
        Self {
            spectrum: vec![0.0; N_LONG],
            energy: [0.0; MAX_BANDS],
            thr: [0.0; MAX_BANDS],
            pe: [0.0; MAX_BANDS],
            is_pair: false,
            field_5bc: 0,
            wseq: 0,
            lines_per_window: N_LONG as u32,
            total_bands: 0,
            nsfb_per_group: 0,
            max_sfb: 0,
            ngroups: 1,
            group_len: [1, 0, 0, 0, 0, 0, 0, 0],
            width: [0; MAX_BANDS],
            start: [0; MAX_BANDS + 1],
            is_short: false,
            flags: [0; MAX_BANDS],
            tns: None,
            intensity: None,
        }
    }
}
impl Default for PsyOut {
    fn default() -> Self {
        Self::new()
    }
}

// ---------------------------------------------------------------------------------------------------------------------------------
// Bitstream-side contract: what the writer (`bits`) serialises; the quantizer output converts 1:1.

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ElementKind {
    Sce,
    Cpe,
    Lfe,
}

#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct IcsInfo {
    pub window_sequence: u8,
    pub window_shape: u8,
    pub max_sfb: u8,
    pub scale_factor_grouping: u8,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Section {
    pub cb: u8,
    pub start: u16,
    pub end: u16,
}

/// individual_channel_stream payload of one channel.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct ChannelStream {
    pub global_gain: u8,
    /// sections per window group (sfb index ranges, codebook 0..=15 incl. 15 = intensity).
    pub sections: Vec<Vec<Section>>,
    /// coded scalefactor per group per sfb (absolute value as in the stream, i.e. what the access unit carries as `sf`; for codebook 15 bands
    /// the intensity position `is_position`); entries of zero-codebook bands are ignored by the writer.
    pub sf: Vec<Vec<i32>>,
    pub tns: Option<TnsChannel>,
    /// signed quantized spectrum in coded (group-sorted) order, `1024` entries: group `g` starts at line `win0*128` (`win0` = windows in the preceding groups),
    /// band `sfb` at `+ swb[sfb]*L` (`L` = windows in the group), window `w` of the band at `+ w*width + k` (long blocks: `q[line]`); i.e. each section is one contiguous slice.
    pub q: Vec<i32>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Element {
    pub kind: ElementKind,
    pub tag: u8,
    pub common_window: bool,
    /// CPE with common_window: shared ics_info; otherwise per channel infos are in `ics[0..n]`.
    pub ics: [IcsInfo; 2],
    /// 0 none, 1 per-band mask in `ms_used`, 2 all bands.
    pub ms_mask_present: u8,
    pub ms_used: Vec<Vec<bool>>,
    pub channels: Vec<ChannelStream>,
}

/// In-band program config element (AU 0 of 4-ch and 7-ch streams).
#[derive(Clone, Debug, PartialEq)]
pub struct PceInfo {
    pub tag: u8,
    pub profile: u8,
    pub sf_index: u8,
    pub front: Vec<(bool, u8)>,
    pub side: Vec<(bool, u8)>,
    pub back: Vec<(bool, u8)>,
    pub lfe: Vec<u8>,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct AccessUnit {
    pub pce: Option<PceInfo>,
    pub elements: Vec<Element>,
}
