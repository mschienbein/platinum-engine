//! `PamParam` (Apple's `P`, calloc 0x1b8 + heap arrays, `*(psyConf+8)`): construction (aacOpen/paminit.c: partition layout,
//! `_initThresholdInQuiet`, `_getMaskOffsetGlobal2adjustBR`, `_setBrIndexOffsetMinMax`, `_initMaxSmrParameters`) and the per-frame
//! refresh `_adaptPamParam(B)` (called by `AACEncodeOutput` after each frame in which the ABR bias index changed).
//!
//! Apple decompiles of `0000aaf0`, `0000ae4c`, `0000b0ac`, `0000b8b4`, `0000bacc`, `0000bb58`, `0000bba8`, `0000bc84`.  Only the AAC-LC
//! (1024) engine path is ported (no LD/ELD, `param_10 == param_11 == 0`).
//!
//! libm: every transcendental goes through `crate::portable::math` (Apple-exact): `log10` (double), `exp10f`, `pow`.
//! Rounding/fusion notes are given at each formula (`fma` = `f32::mul_add` = AArch64 `fmadd`; everything else is a plain f32 op).

use crate::portable::aac::psy::pam::pow10_est;
use crate::portable::aac::psy::tables::*;
use crate::portable::math::{exp10f, log10, pow};

/// Inputs of the PAM parameter derivation that come from the codec configuration (`encoder::config`).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct PamConfig {
    /// sample rate in Hz (44100 or 48000 for the engine).
    pub rate: u32,
    /// channel count as seen by `_getMaskOffsetGlobal2adjustBR`/`_adaptPamParam` (`x2`: mono = 1, stereo = 2, 5.1 = 6, ...).
    pub nch: u32,
    /// `aacFmtDescription.bitratePerFbwChannel` (+0x18): 128000 stereo, 256000 mono, 85333 3ch, 64000 4ch, 51200 5/6ch, 42666 7ch, 36571 8ch.
    pub bitrate_per_channel: u32,
    /// quality bucket (`cdqu` -> 0x20/0x40/0x60; engine 0x40).
    pub quality: u32,
    /// `psyConf+0x198` (`_getAverageBandWidthRatio`, bandwidth model of `encoder::config`), e.g. 0.4582266 stereo 44.1.
    pub bw_ratio: f32,
    /// `kAudioCodecPropertyBitRateControlMode` (`acbf`, aacHandle+0xf0): 0 CBR, 1 LongTermAverage (engine default), 2 VBR constrained,
    /// 3 VBR.  Only selects TRDE word 7 here.
    pub acbf: u16,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PsyError {
    /// no `_sfbInfoTab_1024` entry for this sample rate.
    UnsupportedSampleRate(u32),
}

impl std::fmt::Display for PsyError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::UnsupportedSampleRate(rate) => write!(f, "no psychoacoustic band table for a sample rate of {rate} Hz"),
        }
    }
}

impl std::error::Error for PsyError {}

/// Apple's `sfbinfo` (`_GetSfbParam`): `N` lines per window, `nsfb` bands, widths and offsets (`nsfb + 1`).
#[derive(Clone, Copy, Debug)]
pub struct SfbInfo {
    pub n: u16,
    pub nsfb: usize,
    pub widths: &'static [u16],
    pub offsets: &'static [u16],
}

/// `_GetSfbParam(rate, kind, granule = 1024)`: kind long (0, 1, 3) or short (2).
pub fn get_sfb_param(rate: f32, short: bool) -> Option<SfbInfo> {
    SFB_INFO_1024.iter().find(|e| e.rate == rate).map(|e| {
        if short {
            SfbInfo { n: 128, nsfb: e.short_widths.len(), widths: e.short_widths, offsets: e.short_offsets }
        } else {
            SfbInfo { n: 1024, nsfb: e.long_widths.len(), widths: e.long_widths, offsets: e.long_offsets }
        }
    })
}

/// `P`.  Field comments give the offset in Apple's struct.
#[derive(Clone, Debug)]
pub struct PamParam {
    pub cfg: PamConfig,
    pub n_p: usize,
    pub n_s: usize,
    pub n_sh: usize,
    /// +0x00 partition boundaries (nP+1).
    pub part_bounds: Vec<i32>,
    /// +0x08 first/last INCLUSIVE partition of each long sfb.
    pub sfb_range_long: Vec<[i32; 2]>,
    /// +0x10 same for the short sfbs.
    pub sfb_range_short: Vec<[i32; 2]>,
    /// +0x18 long sfb offsets (nS+1).
    pub sfb_off_long: Vec<i32>,
    /// +0x20 short sfb offsets x 8 (nSh+1) (1024-line domain).
    pub sfb_off_short_x8: Vec<i32>,
    /// +0x38 partition centre frequency (Hz).
    pub cf: Vec<f32>,
    /// +0x58 10*log10(hz/ERB).
    pub p58: Vec<f32>,
    /// +0x68 10log10(short width), +0x70 10log10(short width * 8), +0x78 10log10(long width).
    pub p68: Vec<f32>,
    pub p70: Vec<f32>,
    pub p78: Vec<f32>,
    /// +0x80 (1.0, added to the short thresholds).
    pub p80: f32,
    /// +0x88/+0x90/+0x98 tonality window: back count, forward count, 1/(window ERB sum).
    pub ton_back: Vec<i32>,
    pub ton_fwd: Vec<i32>,
    pub ton_norm: Vec<f32>,
    /// +0xa0/+0xa8 quiet threshold dB per long/short sfb; +0xb0/+0xb8 in power.
    pub quiet_long_db: Vec<f32>,
    pub quiet_short_db: Vec<f32>,
    pub quiet_long_pow: Vec<f32>,
    pub quiet_short_pow: Vec<f32>,
    /// +0xc8 `20 log10(1/(400/cf+1))` (dB), +0xd0 its power weight `exp10f(0.1 * +0xc8)`.
    pub p_c8: Vec<f32>,
    pub p_d0: Vec<f32>,
    /// +0xd8 / +0xe0 power-domain spreading decay forward / backward.
    pub p_d8: Vec<f32>,
    pub p_e0: Vec<f32>,
    /// +0xe8 / +0xf0 / +0xf8 / +0x100 dB slopes (-6, +12, -10, +14) * cl (nP-1).
    pub p_e8: Vec<f32>,
    pub p_f0: Vec<f32>,
    pub p_f8: Vec<f32>,
    pub p_100: Vec<f32>,
    /// +0x108 spreading lower partition index.
    pub spread_lo: Vec<i32>,
    /// +0x110 spreading acceleration (always 0 for LC).
    pub p110: f32,
    /// +0x118 per-partition mask offset (dB) -- re-derived by `adapt`.
    pub mask_offset: Vec<f32>,
    /// +0x128 `0.25*B (+ overshoot beyond [Bmin,Bmax])` -- `adapt`.
    pub p128: f32,
    /// +0x12c global mask offset (`_getMaskOffsetGlobal2adjustBR`).
    pub p12c: f32,
    /// +0x130 SMR-floor ramp (tonal table row[12]) -- `adapt`.
    pub p130: f32,
    /// +0x134 partition-0 smoothing weight.
    pub p134: f32,
    /// +0x138..0x14c pre-echo parameters {long base, +prev, +twoBack, short base, +prev, +twoBack} -- `adapt`.
    pub preecho: [f32; 6],
    /// +0x150 / +0x154 sfb index of 200 Hz / 5000 Hz (`_freq2SfbBound`).
    pub sfb_200hz: i32,
    pub sfb_5000hz: i32,
    /// +0x158 / +0x15c reducePeaks multiplier / offset -- `adapt`.
    pub p158: f32,
    pub p15c: f32,
    /// +0x160 tonality scale (0.5).
    pub p160: f32,
    /// +0x164 / +0x168 window lengths W1 / W2, +0x16c / +0x170 their reciprocals -- `adapt`.
    pub w1: i32,
    pub w2: i32,
    pub inv_w1: f32,
    pub inv_w2: f32,
    /// +0x178 short-block tilt (nSh) -- `adapt`.
    pub short_tilt: Vec<f32>,
    /// +0x180 max-SMR curve (all 1000.0 for LC) and +0x18c enable (0 for LC).
    pub max_smr: Vec<f32>,
    pub max_smr_enabled: i32,
    /// +0x190 / +0x194 / +0x198 (pamMiscParamSet[4], [4], [5]) -- `adapt`; +0x19c = 1.1.
    pub p190: f32,
    pub p194: f32,
    pub p198: f32,
    /// +0x1a0 bias index B (set by the ABR controller, passed to `adapt`), +0x1a4 / +0x1a8 Bmin / Bmax.
    pub b: i32,
    pub bmin: i32,
    pub bmax: i32,
    /// +0x1ac tonal table row[13] (number of top short sfbs treated with min-thr in `_AdvancePsyOut`; 0 for 44.1/48 kHz).
    pub p1ac: i32,
    /// +0x1b0 `TRDE` block-switching tuning struct (0x40 bytes as 16 f32 bit patterns; words 0-6 and 8-15 rewritten by `adapt`, word 7
    /// = integer set at construction from (acbf, quality, rate, br/ch)): consumed by the filterbank stage.
    pub trde: [f32; 16],
}

#[inline]
fn f32c(x: f64) -> f32 {
    x as f32
}

/// `_thresholdQuiet` (0xae4c): absolute threshold in quiet in dB at frequency `f`.
pub fn threshold_quiet(f: f32, rate: f32, off: f32, br: u32) -> f32 {
    if f < 0.0 {
        return 250.0;
    }
    let s9 = off + 22.0;
    let (mut s10, s11, s1);
    if rate >= 50000.0 {
        (s11, s10, s1) = (70.0f32, 50.0f32, 28000.0f32);
    } else if br < 77001 {
        (s11, s10, s1) = (70.0, 50.0, 20000.0);
    } else {
        let mut t = (br as i32 - 77000) as f32 / 102000.0;
        if t > 1.0 {
            t = 1.0;
        }
        s1 = t.mul_add(8000.0, 20000.0);
        s10 = t.mul_add(-28.0, 50.0);
        s11 = t.mul_add(-60.0, 70.0);
    }
    let s12 = 10000.0f32;
    let d0 = log10((s1 / s12) as f64);
    let arg;
    if f < 500.0 {
        s10 = s11 / 2.8864989f32;
        arg = f / 500.0;
    } else if f <= s12 {
        return if s9 < 250.0 { s9 } else { 250.0 };
    } else {
        let mut s0 = f32c(d0);
        s0 *= s0;
        s10 /= s0;
        arg = f / s12;
    }
    let l = f32c(log10(arg as f64));
    let s9 = (s10 * l).mul_add(l, s9);
    if s9 < 250.0 { s9 } else { 250.0 }
}

fn bitrate_bucket(br: i32) -> usize {
    // shared first half of `_getBitrateIndexPam` / `_setBrIndexOffsetMinMax`: nearest bucket of bitrateMapPam_CPE (midpoint rounds down).
    let mut i = 0usize;
    while BITRATE_MAP_PAM_CPE[i] < br && i < 29 {
        i += 1;
    }
    let mut base = 0usize;
    if i != 0 {
        base = i;
        if (br as f32) < ((BITRATE_MAP_PAM_CPE[i - 1] + BITRATE_MAP_PAM_CPE[i]) as f32) / 2.0 {
            base = i - 1;
        }
    }
    base
}

/// `_getBitrateIndexPam(br, Bmin, Bmax, B) -> (P+0x128, index)`.
pub fn get_bitrate_index_pam(br: i32, bmin: i32, bmax: i32, b: i32) -> (f32, i32) {
    let base = bitrate_bucket(br) as i32;
    let mut eff = b;
    let mut over = 0.0f32;
    if b - bmax != 0 && bmax <= b {
        eff = bmax;
        over = (b - bmax) as f32;
    }
    if bmin - b != 0 && b <= bmin {
        eff = bmin;
        over = -((bmin - b) as f32);
    }
    (over + (b as f32) * 0.25, eff + base)
}

/// `_getSampleRateIndexPam(rate)`.
pub fn get_sample_rate_index_pam(rate: f32) -> usize {
    let mut i = 0usize;
    while SAMPLE_RATE_MAP_PAM[i] < rate as i32 && i < 11 {
        i += 1;
    }
    if i != 0 && rate < ((SAMPLE_RATE_MAP_PAM[i - 1] + SAMPLE_RATE_MAP_PAM[i]) as f32) / 2.0 {
        i -= 1;
    }
    i
}

/// `_setBrIndexOffsetMinMax(br)` -> (Bmin, Bmax).
pub fn set_br_index_offset_min_max(br: i32, rate: f32) -> (i32, i32) {
    let bridx = bitrate_bucket(br) as i32;
    let sri = get_sample_rate_index_pam(rate);
    let setv = PAM_PARAM_SET_INDEX[bridx as usize][sri] as i32 - 8;
    let bmin = if setv >= 0 { (-setv).max(-4) } else { 0 };
    let bmax = (29 - bridx).min(4);
    (bmin, bmax)
}

/// `_getMaskOffsetGlobal2adjustBR` (0xb0ac) for AAC-LC (`isLD == 0`, tool flags 0): global mask offset in dB.
pub fn mask_offset_global(rate: f32, bw_ratio: f32, br: u32, nch: u32, quality: u32) -> f32 {
    let mut row = &MASK_OFFSET_GLOBAL[6];
    for r in MASK_OFFSET_GLOBAL.iter() {
        if rate <= r[0] {
            row = r;
            break;
        }
    }
    let mut v = 0.0f32;
    if quality > 0x20 && nch >= 2 {
        v += 0.3f32;
    }
    if quality == 0x20 {
        v += row[2];
        if nch == 1 {
            let f12 = (br as f32) / rate;
            let t = f12 * 0.5;
            if t <= 1.0 {
                let dv = t as f64;
                let dv9 = pow(dv, 0.800000011920929f64) / dv;
                v += (dv9 * 1.2999999523162842f64) as f32;
            }
            if rate >= 44100.0 {
                v += -1.0;
            }
        }
    }
    v + (((1.0 - bw_ratio) * row[3]) * (br as f32)) / rate
}

impl PamParam {
    /// aacOpen (paminit.c) + `_adaptPamParam(B)`.  Engine: `new(cfg, 0)`.
    pub fn new(cfg: PamConfig, b: i32) -> Result<Self, PsyError> {
        let rate_f = cfg.rate as f32;
        let long = get_sfb_param(rate_f, false).ok_or(PsyError::UnsupportedSampleRate(cfg.rate))?;
        let short = get_sfb_param(rate_f, true).ok_or(PsyError::UnsupportedSampleRate(cfg.rate))?;
        let n_s = long.nsfb;
        let n_sh = short.nsfb;
        let sfbo: Vec<i32> = long.offsets.iter().map(|&x| x as i32).collect();
        let sho8: Vec<i32> = short.offsets.iter().map(|&x| (x as i32) << 3).collect();

        // ---- partition boundaries (aacOpen 0x6a3c): (n4, n2, last) by rate
        let rate_i = cfg.rate as i32;
        let (n4, n2, last) = if rate_i < 8001 {
            (6usize, 0x26usize, 2usize)
        } else if rate_i < 16001 {
            (5, 0x29, 2)
        } else if rate_i <= 24000 {
            (0x2e, 0x2e, 4)
        } else if rate_i <= 32000 {
            (0x1f, 0x2b, 1)
        } else if rate_i <= 48000 {
            (0x1f, 0x27, 4)
        } else {
            (0x20, 0x29, 1)
        };
        let mut pb: Vec<i32> = Vec::with_capacity(160);
        for i in 0..n_s {
            let w = sfbo[i + 1] - sfbo[i];
            if i == n_s - 1 {
                for k in 0..last as i32 {
                    pb.push(sfbo[i] + w * k / last as i32);
                }
            } else if i < n4 {
                for k in 0..4 {
                    pb.push(sfbo[i] + w * k / 4);
                }
            } else if i < n2 {
                for k in 0..2 {
                    pb.push(sfbo[i] + w * k / 2);
                }
            } else {
                pb.push(sfbo[i]);
            }
        }
        pb.push(sfbo[n_s]);
        let n_p = pb.len() - 1;

        // ---- static arrays (pam_init.build_static)
        let hz = rate_f / 2048.0f32;
        let a = 0.004370000213384628f64 as f32;
        let b_ = 24.700000762939453f64 as f32;
        let mut cf = vec![0f32; n_p];
        let mut bw_hz = vec![0f32; n_p];
        let mut p48 = vec![0f32; n_p];
        let mut p50 = vec![0f32; n_p];
        let mut p58 = vec![0f32; n_p];
        let mut p_c8 = vec![0f32; n_p];
        let mut p_d0 = vec![0f32; n_p];
        for i in 0..n_p {
            // cf = hz * float((pb[i+1]+pb[i])/2.0): the half-integer is exact in f32
            cf[i] = hz * (((pb[i + 1] + pb[i]) as f64 / 2.0) as f32);
            bw_hz[i] = hz * ((pb[i + 1] - pb[i]) as f32);
            let inv = 1.0f32 / (cf[i].mul_add(a, 1.0) * b_);
            p48[i] = bw_hz[i] * inv;
            p50[i] = hz * inv;
            p58[i] = f32c(log10(p50[i] as f64)) * 10.0;
            // double arithmetic: 1.0 / (400.0 / (double)cf + 1.0)
            let v = 1.0f64 / (400.0f64 / (cf[i] as f64) + 1.0);
            p_c8[i] = f32c(log10(v)) * 20.0;
            p_d0[i] = exp10f(p_c8[i] * 0.10000000149011612f64 as f32);
        }
        let c21 = 21.399999618530273f64 as f32;
        let mut cl = vec![0f32; n_p - 1];
        for i in 0..n_p - 1 {
            let t1 = f32c(log10(cf[i + 1].mul_add(a, 1.0) as f64)) * c21;
            let t0 = f32c(log10(cf[i].mul_add(a, 1.0) as f64)) * c21;
            cl[i] = t1 - t0;
        }
        // tonality window arrays
        let mut ton_back = vec![0i32; n_p];
        let mut ton_fwd = vec![0i32; n_p];
        let mut ton_norm = vec![0f32; n_p];
        {
            let (mut bwd, mut fwd) = (0f32, 0f32);
            let (mut jb, mut jf) = (0usize, 0usize);
            for i in 0..n_p {
                if i != 0 {
                    let d = cl[i - 1];
                    bwd += d;
                    fwd -= d;
                }
                let mut cnt = 0;
                if bwd > 2.5 {
                    loop {
                        bwd -= cl[jb];
                        jb += 1;
                        cnt += 1;
                        if !(bwd > 2.5) {
                            break;
                        }
                    }
                }
                let mut cnt2 = 0;
                if fwd < 2.5 && jf < n_p - 1 {
                    loop {
                        fwd += cl[jf];
                        jf += 1;
                        cnt2 += 1;
                        if !(fwd < 2.5 && jf < n_p - 1) {
                            break;
                        }
                    }
                }
                ton_back[i] = cnt;
                ton_fwd[i] = cnt2;
                ton_norm[i] = 1.0f32 / (bwd + fwd);
            }
        }
        // sfb -> partition ranges (inclusive)
        let ranges = |offs: &[i32], nseg: usize| -> Vec<[i32; 2]> {
            let mut r = Vec::with_capacity(nseg);
            let mut start = 1usize;
            for s in 0..nseg {
                let mut p = start;
                while pb[p] <= offs[s] {
                    p += 1;
                }
                let lo = p as i32 - 1;
                let mut q = p;
                while pb[q] < offs[s + 1] {
                    q += 1;
                }
                let hi = q as i32 - 1;
                r.push([lo, hi]);
                start = q;
            }
            r
        };
        let sfb_range_long = ranges(&sfbo, n_s);
        let sfb_range_short = ranges(&sho8, n_sh);
        let lg10x10 = |w: i32| f32c(log10((w as f32) as f64)) * 10.0;
        let p68: Vec<f32> = (0..n_sh).map(|k| lg10x10(short.offsets[k + 1] as i32 - short.offsets[k] as i32)).collect();
        let p70: Vec<f32> = (0..n_sh).map(|k| lg10x10(sho8[k + 1] - sho8[k])).collect();
        let p78: Vec<f32> = (0..n_s).map(|k| lg10x10(sfbo[k + 1] - sfbo[k])).collect();
        let p_e8: Vec<f32> = cl.iter().map(|&c| c * -6.0).collect();
        let p_f0: Vec<f32> = cl.iter().map(|&c| c * 12.0).collect();
        let p_f8: Vec<f32> = cl.iter().map(|&c| c * -10.0).collect();
        let p_100: Vec<f32> = cl.iter().map(|&c| c * 14.0).collect();
        let mut p_d8 = vec![0f32; n_p];
        let mut p_e0 = vec![0f32; n_p];
        for i in 0..n_p - 1 {
            p_e0[i + 1] = exp10f(cl[i] * -3.0);
            p_d8[i] = exp10f(cl[i] * -1.5);
        }
        let mut spread_lo = vec![0i32; n_p];
        for i in 0..n_p {
            let mut x12 = i as i32 + 1;
            let mut x13 = i as i32;
            let mut s1 = 0f32;
            loop {
                x12 -= 1;
                s1 += p48[x13 as usize];
                x13 -= 1;
                if !(s1 < 0.5 && x12 > 0) {
                    break;
                }
            }
            spread_lo[i] = x12;
        }

        // ---- initThresholdInQuiet (0xaaf0), N = 1024; the DRC/loudness branch (param_8 flags) is inactive in the engine
        let hzr = rate_f;
        let fr = 2048.0f32;
        let br = cfg.bitrate_per_channel;
        let f14 = ((hzr / 1.0f32) * ((br as f32) / 63000.0f32)) / 48000.0f32;
        let mut off = 0.0f32;
        if 2.0 < f14 {
            let t = f14 + -2.0;
            let f20 = if !(1.0 <= t) { 1.0 } else { t };
            if f20 * -20.0 <= off {
                off = f20 * -20.0;
            }
        }
        let mut quiet_long_db = vec![0f32; n_s];
        let mut quiet_short_db = vec![0f32; n_sh];
        for s in 0..n_s {
            let mut best = 1.0e30f32;
            for k in sfbo[s]..sfbo[s + 1] {
                let fv = (hzr / fr) * ((k as f32) + -0.5);
                if fv > 0.0 {
                    let v = threshold_quiet(fv, rate_f, off, br);
                    if v <= best {
                        best = v;
                    }
                }
            }
            quiet_long_db[s] = best;
        }
        for s in 0..n_sh {
            let mut best = 1.0e30f32;
            let mut k = sho8[s];
            while k < sho8[s + 1] {
                let f = ((hzr * 8.0f32) / fr) * (((k >> 3) as f32) + 0.5);
                let v = threshold_quiet(f, rate_f, off, br);
                if v <= best {
                    best = v;
                }
                k += 8;
            }
            quiet_short_db[s] = best;
        }
        let quiet_long_pow: Vec<f32> = quiet_long_db.iter().map(|&x| pow10_est(x)).collect();
        let quiet_short_pow: Vec<f32> = quiet_short_db.iter().map(|&x| pow10_est(x)).collect();
        let mut p134 = 0.0f32;
        if n_s != 0 && br > 80000 && cfg.quality > 0x20 {
            let v = ((br as i32 - 80000) as f32) / 48000.0f32;
            p134 = if v > 1.0 { 1.0 } else { v };
        }

        // ---- freq2SfbBound (0xb028): 200 Hz and 5000 Hz
        let freq2sfb = |f: f32| -> i32 {
            // line = (int)((float)N * (f/rate + f/rate) + 0.5)
            let r = f / rate_f;
            let line = (1024.0f32 * (r + r) + 0.5f32) as i32;
            if line >= 1024 {
                return n_s as i32 - 1;
            }
            // nearest sfb boundary index: pick i or i+1 by the smaller distance
            let mut i = 0usize;
            while i + 1 < sfbo.len() && sfbo[i + 1] <= line {
                i += 1;
            }
            if i + 1 < sfbo.len() && (line - sfbo[i]) > (sfbo[i + 1] - line) {
                i += 1;
            }
            i as i32
        };
        let sfb_200hz = freq2sfb(200.0);
        let sfb_5000hz = freq2sfb(5000.0);

        let (bmin, bmax) = set_br_index_offset_min_max(br as i32, rate_f);
        let p12c = mask_offset_global(rate_f, cfg.bw_ratio, br, cfg.nch, cfg.quality);

        let mut p = PamParam {
            cfg,
            n_p,
            n_s,
            n_sh,
            part_bounds: pb,
            sfb_range_long,
            sfb_range_short,
            sfb_off_long: sfbo,
            sfb_off_short_x8: sho8,
            cf,
            p58,
            p68,
            p70,
            p78,
            p80: 1.0,
            ton_back,
            ton_fwd,
            ton_norm,
            quiet_long_db,
            quiet_short_db,
            quiet_long_pow,
            quiet_short_pow,
            p_c8,
            p_d0,
            p_d8,
            p_e0,
            p_e8,
            p_f0,
            p_f8,
            p_100,
            spread_lo,
            p110: 0.0,
            mask_offset: vec![0f32; n_p],
            p128: 0.0,
            p12c,
            p130: 0.0,
            p134,
            preecho: [0.0; 6],
            sfb_200hz,
            sfb_5000hz,
            p158: 0.0,
            p15c: 0.0,
            p160: 0.5,
            w1: 0,
            w2: 0,
            inv_w1: 0.0,
            inv_w2: 0.0,
            short_tilt: vec![0f32; n_sh],
            // `_initMaxSmrParameters` for LC: P+0x180[] = 1000.0 and P+0x18c = 0 (disabled)
            max_smr: vec![1000.0f32; n_s],
            max_smr_enabled: 0,
            p190: 0.0,
            p194: 0.0,
            p198: 0.0,
            b,
            bmin,
            bmax,
            p1ac: 0,
            trde: {
                // word 7 (+0x1c), aacOpen raw L1664-1680: 0; 1 (and `u = 3`) if acbf == 3 or (acbf == 2 && br/ch > 95999); then, for
                // quality > 0x20 (AAC-LC) and (rate > 24000 && br/ch > 32000 || 16001 <= rate <= 24000 && br/ch > 24000): `u` (2 by default)
                let mut t = [0.0f32; 16];
                let (mut w7, mut u) = (0u32, 2u32);
                if cfg.acbf == 3 || (cfg.acbf == 2 && br > 95999) {
                    w7 = 1;
                    u = 3;
                }
                if cfg.quality > 0x20
                    && ((cfg.rate > 24000 && br > 32000) || ((cfg.rate.wrapping_sub(16001)) >> 6 < 125 && br > 24000))
                {
                    w7 = u;
                }
                t[7] = f32::from_bits(w7);
                t
            },
        };
        p.adapt(b);
        Ok(p)
    }

    /// `_adaptPamParam(B)`: re-derives everything that depends on the ABR bias index (mask offsets `+0x118`, `+0x128`, `+0x130`,
    /// pre-echo/misc rows, window lengths, short tilt, `+0x1ac`, TRDE block-switching tuning).  `b` is stored at `+0x1a0`.
    pub fn adapt(&mut self, b: i32) {
        self.b = b;
        let rate_f = self.cfg.rate as f32;
        let br = self.cfg.bitrate_per_channel as i32;
        let nch = self.cfg.nch;

        // tonal table selection (maskOffsetParamSetSelectionTable: first limit >= rate, 8 entries, default = 96k table)
        let mut tonal: &[[f32; 14]; 28] = &MASK_OFFSET_TONAL_96K;
        let tabs: [&[[f32; 14]; 28]; 8] = [
            &MASK_OFFSET_TONAL_8K,
            &MASK_OFFSET_TONAL_12K,
            &MASK_OFFSET_TONAL_16K,
            &MASK_OFFSET_TONAL_24K,
            &MASK_OFFSET_TONAL_32K,
            &MASK_OFFSET_TONAL_48K,
            &MASK_OFFSET_TONAL_96K,
            &MASK_OFFSET_TONAL_96K,
        ];
        for (j, lim) in MASK_OFFSET_SET_LIMITS.iter().enumerate() {
            if rate_f <= *lim {
                tonal = tabs[j];
                break;
            }
        }
        let (p128, idx) = get_bitrate_index_pam(br, self.bmin, self.bmax, b);
        self.p128 = p128;
        let sri = get_sample_rate_index_pam(rate_f);
        let setv = PAM_PARAM_SET_INDEX[idx.clamp(0, 29) as usize][sri] as i32 - 8;
        let set = setv.max(0) as usize;
        self.p80 = 1.0;
        self.preecho = PREECHO_PARAM_SET[set];
        let misc = &PAM_MISC_PARAM_SET[set];
        self.p158 = misc[2];
        // asm 0xbe34: `str d0,[P,#0x158]` with d0 = zero-extended u32 misc[2]: +0x15c is 0.0
        self.p15c = 0.0;
        self.p190 = misc[4];
        self.p194 = misc[4];
        self.p198 = misc[5];
        // W1 = 1 + 2*floor(5 d), W2 = 1 + 2*floor(3 d), d = nP / (float(log10(rate/2*0.00437+1)) * 21.4)
        let x = ((rate_f * 0.5f32).mul_add(0.004370000213384628f64 as f32, 1.0f32)) as f64;
        let l = f32c(log10(x)) * 21.399999618530273f64 as f32;
        let d = ((self.n_p as i32) as f32) / l;
        let d = d as f64;
        let w1f = (5.0f64 * d).floor() as f32;
        let w2f = (3.0f64 * d).floor() as f32;
        self.w1 = (1.0f32 + 2.0f32 * w1f) as i32;
        self.w2 = (1.0f32 + 2.0f32 * w2f) as i32;
        self.inv_w1 = 1.0f32 / (self.w1 as f32);
        self.inv_w2 = 1.0f32 / (self.w2 as f32);

        let trow = &tonal[set];
        let t_scalar = if nch != 1 { trow[8] } else { trow[10] };
        let base = self.p12c + (p128 + t_scalar);
        let ctrl = &MASK_TONAL_CTRL_FREQS;
        let mut seg = 0usize;
        let mut ub = 500.0f32;
        for k in 0..self.n_p {
            let cfk = self.cf[k];
            if cfk >= ub {
                seg += 1;
            }
            let fl = ctrl[seg];
            let fh = ctrl[seg + 1];
            ub = fh;
            let (tl, th) = (trow[seg], trow[seg + 1]);
            let s4 = fh - cfk;
            let s2 = cfk - fl;
            let s2 = th * s2;
            let s2 = tl.mul_add(s4, s2);
            let s3 = fh - fl;
            self.mask_offset[k] = base + s2 / s3;
        }
        self.p130 = trow[12];
        self.p1ac = trow[13].to_bits() as i32;
        // short tilt: fma(m0/(nSh-1), k, fma(m0, -0.5, m1))
        let (m0, m1) = (misc[0], misc[1]);
        let sl = m0 / ((self.n_sh as i32 - 1) as f32);
        let s1 = m0.mul_add(-0.5, m1);
        for k in 0..self.n_sh {
            self.short_tilt[k] = sl.mul_add(k as f32, s1);
        }

        // ---- TRDE block-switching tuning (asm 0xc03c..0xc1b4)
        let mut sr_cls = 0usize;
        for (i, &sr) in TR_SAMPLE_RATES.iter().enumerate() {
            let srf = sr as f32;
            // fcmp s8,s0 ; csel w8,w8,w9,hi : keep the class when rate > sr (or unordered), else take i
            if !(rate_f > srf || rate_f.is_nan() || srf.is_nan()) {
                sr_cls = i;
            }
        }
        let mut best = i32::MIN;
        for j in 0..4 {
            let a = if br > TRDE_BR_C1[j] { i32::MIN } else { TRDE_BR_A[j] };
            let bb = if br > TRDE_BR_C2[j] { a } else { TRDE_BR_B[j] };
            if bb > best {
                best = bb;
            }
        }
        let br_cls = if best == i32::MIN { 0usize } else { best as usize };
        let t = &TRDE_TUNING[br_cls * 15 + sr_cls * 5..][..5];
        let h = &TRDE_TUNING_HELPER[br_cls * 12 + sr_cls * 4..][..4];
        let mut tr = [0f32; 16];
        tr[0] = t[0];
        tr[1] = t[1];
        tr[2] = t[2];
        tr[3] = t[4];
        let f3 = t[3];
        tr[8] = h[0];
        tr[9] = h[1];
        tr[10] = h[2];
        tr[11] = f32::from_bits(0x3ecccccd);
        let h3 = h[3];
        let e1 = (((128.0f32 / (rate_f * 4.0f32)) * 40.0f32)) as f64;
        let x1 = pow((1.0f32 / f3) as f64, e1) as f32;
        let sq1 = x1 * x1;
        tr[5] = sq1;
        tr[6] = x1;
        tr[4] = sq1 * sq1;
        let x2 = pow((1.0f32 / h3) as f64, e1) as f32;
        let sq2 = x2 * x2;
        tr[13] = sq2;
        tr[14] = x2;
        tr[12] = sq2 * sq2;
        let e3 = ((256.0f32 / rate_f) * 40.0f32) as f64;
        tr[15] = pow(f64::from_bits(TRDE_POW_BASE_BITS), e3) as f32;
        // word 7 (+0x1c) is not written by adapt (set at construction)
        for (i, v) in tr.iter().enumerate() {
            if i != 7 {
                self.trde[i] = *v;
            }
        }
    }
}
