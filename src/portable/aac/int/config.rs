//! Per-element configuration of the intensity stage: what Apple's `_aacOpen` (0x5538, raw L2262-2352) stores into the `IntensityChannel`
//! (`IC`) of a CPE: sfb tables, long->short band map, threshold-in-quiet tables, bitrate weight.

/// Number of entries of the per-band arrays (`crate::portable::aac::types::MAX_BANDS`).
pub const NB: usize = crate::portable::aac::types::MAX_BANDS;

/// Tool-enable rule of `SetAACEncoderOptions` (0x54b8): intensity (tool mask bit0) is on for a pair element iff `nch >= 2` and the AAC quality
/// field (`cdqu` bucket 0x20/0x40/0x60, `aac+0xe0`) is `> 0x3f`. Engine (`cdqu` 0x40): on for every CPE.
pub fn intensity_enabled(nch: u32, quality_field: u32) -> bool {
    nch >= 2 && quality_field > 0x3f
}

/// Bitrate weight `IC+0x19e8` (aacOpen asm 0x6e98..0x6eb4): `bpc > 64000 ? 64000.0f/(float)bpc : 1.0f`, `bpc` = integer bitrate per
/// (full-bandwidth) channel. Stereo 256 kbps: 128000 -> 0.5; 3 ch (85333) -> 0.75; 4+ ch: 1.0.
pub fn bitrate_weight(bitrate_per_channel: u32) -> f32 {
    if (bitrate_per_channel as i32) > 64000 {
        64000.0f32 / (bitrate_per_channel as f32)
    } else {
        1.0
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct IntensityConfig {
    /// element[10] (tool mask bit0); when false the stage never runs (CPE without intensity).
    pub enabled: bool,
    /// `IC+0x2718`: number of long sfb (49 at 44.1/48 kHz; 51 at 32 kHz).
    pub n_long: usize,
    /// `IC+0x2720` -> psy config `+0x6e`: long sfb start offsets (`n_long + 1` valid entries, then the 1024 end).
    pub tab_long: [u16; 64],
    /// `IC+0x2728`: number of short sfb (14).
    pub n_short: usize,
    /// `IC+0x2730`: short sfb offsets per window (`n_short + 1` valid entries).
    pub tab_short: [u16; 64],
    /// `IC+0x26b0`: long sfb -> short band map (`u16[52]`, entries >= `n_long` are 0).
    pub map: [u16; 52],
    /// `IC+0x1780` (psy config `+0xb0`): threshold in quiet per long sfb.
    pub quiet_long: [f32; NB],
    /// `IC+0x1788` (psy config `+0xb8`): threshold in quiet per short sfb.
    pub quiet_short: [f32; NB],
    /// `IC+0x19e8`.
    pub wbr: f32,
}

/// aacOpen raw L2288-2300: sfb `i` belongs to the first short band `k` with `((tabLong[i]+tabLong[i+1])>>1) < tabShort[k+1]*8`.
pub fn build_map(tab_long: &[u16], n_long: usize, tab_short: &[u16], n_short: usize) -> [u16; 52] {
    let mut map = [0u16; 52];
    let mut i = 0usize;
    for kk in 0..n_short {
        let ub = (tab_short[kk + 1] as u32) << 3;
        while i < n_long && (((tab_long[i] as u32) + (tab_long[i + 1] as u32)) >> 1) < ub {
            map[i] = kk as u16;
            i += 1;
        }
    }
    map
}

impl IntensityConfig {
    /// Build from explicit tables (`tab_long`: `n_long+1` offsets, `tab_short`: `n_short+1` offsets, quiet tables `n_long`/`n_short` entries).
    pub fn new(enabled: bool, tab_long: &[u16], tab_short: &[u16], quiet_long: &[f32], quiet_short: &[f32], bitrate_per_channel: u32) -> Self {
        let n_long = tab_long.len() - 1;
        let n_short = tab_short.len() - 1;
        let mut tl = [0u16; 64];
        tl[..tab_long.len()].copy_from_slice(tab_long);
        let mut ts = [0u16; 64];
        ts[..tab_short.len()].copy_from_slice(tab_short);
        let mut ql = [0.0f32; NB];
        ql[..quiet_long.len()].copy_from_slice(quiet_long);
        let mut qs = [0.0f32; NB];
        qs[..quiet_short.len()].copy_from_slice(quiet_short);
        let map = build_map(&tl, n_long, &ts, n_short);
        Self { enabled, n_long, tab_long: tl, n_short, tab_short: ts, map, quiet_long: ql, quiet_short: qs, wbr: bitrate_weight(bitrate_per_channel) }
    }
}
