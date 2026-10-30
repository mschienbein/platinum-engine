//! `IntensityChannel` (`IC`, 0x2a08 bytes, allocated per CPE by `_aacOpen`): all persistent and scratch arrays, with Apple's offsets in the docs.
use crate::portable::aac::int::config::{IntensityConfig, NB};

pub const INT_MIN: i32 = i32::MIN;

#[derive(Clone, Debug, PartialEq)]
pub struct IntensityState {
    /// +0x0000 scaled downmix spectrum (band-major, group-sorted layout like `PsyOut::spectrum`).
    pub dm: [f32; 1024],
    /// +0x1000 threshold of the downmix band (init 1.0).
    pub thr: [f32; NB],
    /// +0x11e0 decision flag per band (1 = intensity).
    pub is_flag: [u8; NB],
    /// +0x1258 per-sfb decision of the previous frame (-1 unknown, 0, 1, 2 = mixed after a block-class remap); init -1.
    pub state: [i32; 52],
    /// +0x1328 block type of ch0 in the last decided frame.
    pub prev_bt: i32,
    /// +0x132c `sum dm^2 / nwin` per band.
    pub dm_energy: [f32; NB],
    /// +0x150c copy of `dm_energy[0..nsfb)` taken at analysis start when bt != EIGHT_SHORT.
    pub dm_energy_prev: [f32; 52],
    /// +0x15dc / +0x16ac previous frame's long-sfb energy of ch0 / ch1 (updated by `_estimateIldDistortionShort`).
    pub prev_en_l: [f32; 52],
    pub prev_en_r: [f32; 52],
    /// +0x1790 PE of the downmix band.
    pub pe_dm: [f32; NB],
    /// +0x1970 ILD distortion per short band.
    pub ild: [f32; 15],
    /// +0x19ac std-dev of the per-short-window ILD (dB) for long-type blocks.
    pub stat: [f32; 15],
    /// +0x19ec intensity phase flag per band (1 -> cb14, 0 -> cb15); always 0 (published as `IntensityData::flag`).
    pub phase: [u8; NB],
    /// +0x1a64 quantised intensity position per band (`INT_MIN` = none; published as `IntensityData::position`).
    pub pos: [i32; NB],
    /// +0x1c44 downmix gain `s`.
    pub scale: [f32; NB],
    /// +0x1e24 correlation/ILD/stationarity penalty.
    pub x1: [f32; NB],
    /// +0x2004 instantaneous distortion penalty.
    pub v2: [f32; NB],
    /// +0x21e4 smoothed `v2`.
    pub x2: [f32; NB],
    /// +0x23c4 long-block smoothing state of `v2` (by long sfb).
    pub st23: [f32; 52],
    /// +0x2494 short-block smoothing state (by short sfb).
    pub st24: [f32; 15],
    /// +0x24d0 quantised amplitude ratio `R/L = 2^(-pos/4)`.
    pub g: [f32; NB],
    /// +0x2738 correlation penalty `f(c)` per long sfb (smoothed; entry 48 is never written).
    pub f: [f32; 52],
    /// +0x2808 `f` mapped to short bands, replicated per group (8 x 15).
    pub f2808: [f32; NB],
    /// +0x29e8 number of long sfb valid for correlation smoothing.
    pub prev_count: i32,
    /// +0x29f0 / +0x29f8 per-(group,sfb) TNS mask (bit0 ch0, bit1 ch1) of this and the previous frame (swapped every frame).
    pub tns_cur: [u8; NB],
    pub tns_prev: [u8; NB],
}

impl IntensityState {
    /// `IC` after `_aacOpen` (raw L2316-2352): `dm` = 0, `thr` = 1.0, `pos` = INT_MIN, flags/phase/energies/PE = 0, `state` = -1, `prev_bt` = 0.
    pub fn new() -> Self {
        Self {
            dm: [0.0; 1024],
            thr: [1.0; NB],
            is_flag: [0; NB],
            state: [-1; 52],
            prev_bt: 0,
            dm_energy: [0.0; NB],
            dm_energy_prev: [0.0; 52],
            prev_en_l: [0.0; 52],
            prev_en_r: [0.0; 52],
            pe_dm: [0.0; NB],
            ild: [0.0; 15],
            stat: [0.0; 15],
            phase: [0; NB],
            pos: [INT_MIN; NB],
            scale: [0.0; NB],
            x1: [0.0; NB],
            v2: [0.0; NB],
            x2: [0.0; NB],
            st23: [0.0; 52],
            st24: [0.0; 15],
            g: [0.0; NB],
            f: [0.0; 52],
            f2808: [0.0; NB],
            prev_count: 0,
            tns_cur: [0; NB],
            tns_prev: [0; NB],
        }
    }
}

impl Default for IntensityState {
    fn default() -> Self {
        Self::new()
    }
}

/// One `IntensityChannel` per CPE (config + state).
#[derive(Clone, Debug)]
pub struct IntensityChannel {
    pub cfg: IntensityConfig,
    pub st: IntensityState,
}

impl IntensityChannel {
    pub fn new(cfg: IntensityConfig) -> Self {
        Self { cfg, st: IntensityState::new() }
    }
}
