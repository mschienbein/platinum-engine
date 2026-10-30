//! F3: bandwidth chain `_SetAudioBandwidth` (0x9608) -> `_ApplyBandwidthReduction` (0x9a50) -> `_SetBandwidthParametersXEld` (0x9acc), the cutoff
//! lines / taper of `_CreatePsyParam` (0x9ca0) and `_specWinLowpass` (0x43dcc).
//!
//! Transcribed from the arm64 disassembly of those functions including every fused
//! `fmadd`: `fma(2*bps, -15000, R)` (0x9890), `fma(f3, -0.25, bw)` and `fma(red - 1, f4, 1)` (0x9a74, 0x9aa8), `fma(bw - bw2, -0.5, bw2)` (0x9d68) and the Blackman
//! taper `fma(cos(2a(L+i)), 0.08, fma(cos(a(L+i)), -0.5, 0.42))` (0x9ea0/0x9eb4). Only the engine path (`p9 == 0`: AAC-LC, `p11 == 0`) of `_SetAudioBandwidth`
//! is ported; the others return [`BandwidthError::Unsupported`]. Checked by the encoder fixtures (44.1/48 kHz x 1..8 channels and the other accepted formats).

use crate::portable::aac::fb::gen_tables::*;
use crate::portable::arm::max_number;
use crate::portable::math;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BandwidthError {
    /// Branch of `_SetAudioBandwidth` that the AAC-LC engine configuration never takes (ELD/VBR-small p9 != 0, low-sample-rate p11 != 0 variants).
    Unsupported,
}

impl std::fmt::Display for BandwidthError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Unsupported => f.write_str("a bandwidth set-up branch the AAC-LC engine configuration never takes"),
        }
    }
}

impl std::error::Error for BandwidthError {}

/// `_ApplyBandwidthReduction(fs, bw, reduction)` (0x9a50).
pub fn apply_bandwidth_reduction(bw: f32, red: f32, fs: u32) -> f32 {
    let f3 = fs as f32 * 0.5; // ucvtf s2, w0, #1
    let half = f3 * 0.5;
    let q = f3 * 0.25;
    let s7 = f3.mul_add(-0.25f32, bw); // fmadd s7, s2, s7(-0.25), s0
    let s5 = s7 / q;
    let f2 = if bw <= q { 0.0f32 } else { s5 };
    let f4 = if !(bw < half) { 1.0f32 } else { f2 };
    let mut s0 = 1.0f32 - bw / f3;
    if s0 > 1.0 {
        s0 = 1.0;
    }
    if s0 < 0.0 {
        s0 = 0.0;
    }
    let s1 = (red + -1.0f32).mul_add(f4, 1.0f32); // fmadd s1, s1, s3, s4
    s0 = s0 * s1;
    if s0 > C_009225D8 {
        s0 = C_009225D8;
    }
    f3 * (1.0f32 - s0)
}

/// `_SetBandwidthParametersXEld(st, bw, f12, s1p, fs, p11, flag_x)` (0x9acc): fills `st = {bw, ratio, bw2, f3, f4, 60.0, fs, R}` (index 6/7 are inputs).
/// Register roles of the asm: `s0` = bandwidth in play, `s2` = "f2" (the second edge), `s4` = `f4`.
pub fn set_bandwidth_parameters(st: &mut [f32; 8], bw: f32, f12: f32, s1p: f32, fs_i: u32, p11: bool, flag_x: bool) {
    let fsf = st[6];
    let mut s0: f32;
    let mut s2: f32;
    let mut s4: f32;
    if fs_i != fsf as u32 {
        // 0x9b18: struct sample rate differs from the call's rate
        let f4 = fsf * 0.5;
        s0 = if bw < f4 { bw } else { f4 };
        if p11 || s0 * C_009226DC > C_009226E0 {
            // 0x9b70
            let k = if p11 { C_009226E8 } else { C_009226E4 };
            let t = f4 * k;
            let t = if s0 < t { s0 } else { t };
            st[0] = t;
            st[2] = t;
            st[3] = t;
            s2 = t;
            s4 = fsf - t;
        } else {
            let t = f4 / C_009226DC;
            let t = if s0 < t { s0 } else { t };
            let s5 = t * C_009226DC;
            s4 = if f4 < s5 { f4 } else { s5 };
            st[0] = s4;
            st[2] = t;
            st[3] = t * C_009226E4;
            s2 = t;
        }
    } else {
        s0 = bw;
        let f2 = (fsf / s1p) * 0.5f32;
        if p11 {
            let t = f2 * C_009226E8;
            let t = if s0 < t { s0 } else { t };
            st[0] = t;
            st[2] = t;
            st[3] = t;
            s2 = t;
            s4 = fsf - t;
        } else if s0 > f2 * C_009226B8 {
            // bandwidth above 0.96 * fs/2: full band
            st[2] = f2;
            st[0] = f2;
            st[3] = 0.0;
            s4 = 0.0;
            s0 = f2;
            s2 = f2;
        } else {
            st[2] = s0;
            let t = s0 * C_009226DC;
            if t > C_009226E0 {
                st[0] = s0;
                st[3] = 0.0;
                s4 = 0.0;
            } else {
                s4 = if f2 < t { f2 } else { t };
                st[0] = s4;
                st[3] = s0 * C_009226E4;
            }
            s2 = s0;
        }
    }
    st[4] = s4;
    if flag_x {
        s2 = max_number(s2, C_009226EC);
        st[2] = s2;
        s4 = max_number(s2, C_009226F0);
        st[0] = s4;
        if s0 * C_009226DC <= C_009226E0 {
            st[3] = s2 * (2.0f32 - s4 / s2);
        } else if !p11 {
            st[3] = 0.0;
            s4 = 0.0;
        } else {
            st[3] = s2;
            s4 = fsf - s2;
        }
        st[4] = s4;
    }
    st[5] = 60.0;
    st[1] = s2 / f12;
}

/// Arguments of `_SetAudioBandwidth(s0, s1, st, fs, idx, p6, p7, p8, p9, p10, p11)` (x0..x7 + stack byte).
#[derive(Clone, Copy, Debug)]
pub struct BandwidthArgs {
    /// s0: reduction factor (`_ApplyBandwidthReduction` argument; 1.0 or 0.0 per element in the engine table).
    pub reduction: f32,
    /// s1: element channel factor (1.0).
    pub factor: f32,
    pub fs: u32,
    /// channel-config index selecting the bits-per-sample row.
    pub idx: u32,
    pub p6: u32,
    pub p7: u32,
    /// quality (`cdqu` 0x40 -> 64).
    pub quality: u32,
    pub p9: u32,
    pub p10: u32,
    pub p11: bool,
}

/// `_SetAudioBandwidth` engine path. `st` on entry: `[6]` = fs (float), `[7]` = per-channel bitrate R as raw u32 bits; on return `st[0..6]` = `{bw, ratio, bw2, f3, f4, 60.0}`.
pub fn set_audio_bandwidth(a: &BandwidthArgs, st: &mut [f32; 8]) -> Result<(), BandwidthError> {
    let fs = st[6];
    let r = st[7].to_bits();
    if a.p9 != 0 {
        return Err(BandwidthError::Unsupported);
    }
    if !(!a.p11 || (a.p6 & 5) == 0 || fs >= C_00922688) {
        return Err(BandwidthError::Unsupported);
    }
    let tbl = if a.quality == 0x20 { &BPS_MEDIUM_BW } else { &BPS_LARGE_BW };
    let bps = tbl[a.idx as usize];
    let w28 = !a.p11;
    let rf = r as f32;
    let s3 = bps + bps;
    let f12 = rf / s3;
    let mut s0 = f12;
    if f12 > C_009226C8 {
        let k = if fs / a.factor > C_009226CC { C_009226D0 } else { 13.0f32 };
        let num = s3.mul_add(C_009226D4, rf); // fmadd s2, s3, s5, s2
        s0 = num / k + C_009226C8;
    }
    let bw0 = apply_bandwidth_reduction(s0, a.reduction, a.fs);
    // flag X (stereo-like entries with R >= 64000 at 44.1/48 kHz)
    let flag_x: bool;
    if !a.p11 && a.fs != fs as u32 {
        flag_x = false;
    } else {
        let w8 = a.idx == 1 && (r >> 9) > 0x7c;
        let w9 = fs <= C_009226CC && fs >= C_009226D8;
        flag_x = w8 && w9 && a.quality > 0x20 && a.p9 == 0;
    }
    let w9 = if a.p7 == 3 { w28 } else { false };
    let p10f = a.p10 as f32;
    let hs = a.fs as f32 * 0.5;
    let mut s1v = if hs > p10f { p10f } else { hs };
    let w9b = if !w9 { flag_x } else { false };
    if !w9 {
        s1v = bw0;
    }
    let (flag_final, mut bw) = if a.p10 == 0 { (flag_x, bw0) } else { (w9b, s1v) };
    let s2 = (a.fs as f32 / a.factor) * 0.5f32;
    let s2 = if bw < s2 { bw } else { s2 };
    if !(a.factor <= 1.0) {
        bw = s2;
    }
    set_bandwidth_parameters(st, bw, f12, a.factor, a.fs, a.p11, flag_final);
    Ok(())
}

/// Cutoff and taper of `_CreatePsyParam` for one window type (`n_lines` = 1024 / 128): the number of lines below the bandwidth, the start of the
/// taper and the falling Blackman half window (empty when `bw2 == bw`).
#[derive(Clone, Debug, PartialEq)]
pub struct LowpassSpec {
    /// `(int)(((bw + bw) * N) / fs)`: lines `>= cutoff` are zeroed; also the `bwLine` of the TNS energy limit.
    pub cutoff: usize,
    /// `(int)(((2 * fma(bw - bw2, -0.5, bw2)) * N) / fs)`.
    pub edge2: usize,
    /// taper window, `cutoff - edge2` entries (0 for every engine mono/stereo/>= 4-channel config).
    pub taper: Vec<f32>,
}

/// `_CreatePsyParam` (0x9ca0) lines/taper from the bandwidth struct `st`.
pub fn create_lowpass(st: &[f32; 8], n_lines: u32) -> LowpassSpec {
    lowpass_spec(st[0], st[2], st[6], n_lines)
}

/// `_CreatePsyParam` lines/taper of bandwidth `bw` with second edge `bw2` at sample rate `fs` (also the source of the `psy` stage's `PsyParam`).
pub fn lowpass_spec(bw: f32, bw2: f32, fs: f32, n_lines: u32) -> LowpassSpec {
    let n = n_lines as f32;
    let cutoff = (((bw + bw) * n) / fs) as i32;
    let s0 = (bw - bw2).mul_add(-0.5f32, bw2);
    let s0 = s0 + s0;
    let edge2 = ((s0 * n) / fs) as i32;
    let l = cutoff - edge2;
    let mut taper = Vec::new();
    if l >= 1 {
        let lf = l as f32;
        let two_l_m1 = lf.mul_add(2.0f32, -1.0f32);
        let a = std::f32::consts::TAU / two_l_m1; // 2*pi / (2L - 1)
        let a2 = a + a;
        for i in 0..l {
            let x = (l + i) as f32;
            let c1 = math::cos((a * x) as f64) as f32;
            let t = c1.mul_add(-0.5f32, C_00922720);
            let c2 = math::cos((a2 * x) as f64) as f32;
            taper.push(c2.mul_add(C_00922724, t));
        }
    }
    LowpassSpec { cutoff: cutoff.max(0) as usize, edge2: edge2.max(0) as usize, taper }
}

/// `_specWinLowpass(p, cutoff, n, win, len)` (0x43dcc): `p[cutoff-len+i] *= win[i]` (single f32 multiplies, `vDSP_vmul`), then lines `[cutoff, n)` are
/// zeroed (none when `cutoff >= n`).
pub fn spec_win_lowpass(p: &mut [f32], cutoff: usize, n: usize, win: &[f32]) {
    let len = win.len();
    if len > 0 {
        for i in 0..len {
            p[cutoff - len + i] *= win[i];
        }
    }
    if cutoff < n {
        for v in &mut p[cutoff..n] {
            *v = 0.0;
        }
    }
}

/// Per-channel bandwidth data: the lowpass of the long and the short windows.
#[derive(Clone, Debug, PartialEq)]
pub struct ChannelBandwidth {
    /// Element bandwidth in Hz (`st[0]`), second edge (`st[2]`).
    pub bw: f32,
    pub bw2: f32,
    pub long: LowpassSpec,
    pub short: LowpassSpec,
}

impl ChannelBandwidth {
    /// From the bandwidth struct filled by `_SetAudioBandwidth` (`_CreatePsyParam` for 1024 and 128 lines).
    pub fn from_struct(st: &[f32; 8]) -> Self {
        Self { bw: st[0], bw2: st[2], long: create_lowpass(st, 1024), short: create_lowpass(st, 128) }
    }

    /// Cutoff line (`x4+0xc` / `bwLine`) of the window type.
    pub fn bw_line(&self, wseq: u8) -> usize {
        if wseq == 2 { self.short.cutoff } else { self.long.cutoff }
    }
}

/// Engine configuration of one element: `(idx, reduction s0, per-channel bitrate)` for the `_SetAudioBandwidth` call of element `elem`.
/// Table: 1 ch idx 0; 2 ch idx 1; 3 ch SCE idx 1 / CPE idx 1 with reduction 0; 4 ch idx 3; 5 ch idx 4; 6 ch idx 5; 7 ch idx 7; 8 ch idx 9;
/// `R = brat / (number of full-band channels)` truncated (LFE excluded for 6/7/8 channels).
pub fn engine_element_args(nch: usize, is_cpe_of_3ch: bool, brat: u32) -> (u32, f32, u32) {
    let n_fbw = match nch {
        6..=8 => nch - 1,
        _ => nch,
    } as u32;
    let r = brat / n_fbw;
    let idx = match nch {
        1 => 0,
        2 => 1,
        3 => 1,
        4 => 3,
        5 => 4,
        6 => 5,
        7 => 7,
        8 => 9,
        _ => panic!("unsupported channel count {nch}"),
    };
    let red = if nch == 3 && is_cpe_of_3ch { 0.0 } else { 1.0 };
    (idx, red, r)
}

/// Bandwidth of one engine element (mono SCE/stereo CPE/...): runs `_SetAudioBandwidth` with the engine arguments (cdqu 0x40, p6 = 0, p7 = 1).
pub fn engine_bandwidth(rate: u32, nch: usize, is_cpe_of_3ch: bool, brat: u32) -> ChannelBandwidth {
    let (idx, red, r) = engine_element_args(nch, is_cpe_of_3ch, brat);
    let mut st = [0.0f32; 8];
    st[6] = rate as f32;
    st[7] = f32::from_bits(r);
    let args = BandwidthArgs { reduction: red, factor: 1.0, fs: rate, idx, p6: 0, p7: 1, quality: 0x40, p9: 0, p10: 0, p11: false };
    set_audio_bandwidth(&args, &mut st).expect("engine bandwidth path");
    ChannelBandwidth::from_struct(&st)
}
