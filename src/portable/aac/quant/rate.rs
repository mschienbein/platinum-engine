//! Rate state of the engine configuration (`'acbf'=1` = LongTermAverage, internal rate-control mode `BSE[0x364] == 2`):
//! `_initBitCountStatistics` (0000df80), the per-AU budget block of `_aacEncodeFrame` (0003c1f0..0003c8f0) and the statistics + bias
//! feedback of `AACEncoder::AACEncodeOutput` (0012d3f0).
//!
//! Ported from the rate state `LtaState` with `f32::mul_add` for every `fmadd` and plain f32 operations elsewhere. No reservoir exists in this
//! mode: `AU_bytes = ceil((side + sum(L[ch])) / 8)`.
//! `acbf` 2 (constrained VBR) is included (same statistics, different window lines and feedback); `acbf` 0 (CBR reservoir) and 3 (VBR) are not
//! implemented (no engine configuration uses them).

use crate::portable::aac::quant::fp::{fcvtzs, fma};
use crate::portable::math::expf;
use crate::portable::aac::types::ElementKind;

/// `STAT_N_LIMIT`: statistics stop accumulating at n = 155000 (~1 h): `n_old <= 154999`.
const STAT_N_LIMIT: i32 = 0x25d77;
/// bounds are first computed at the end of frame 215 (`n_old >= 215`)
const STAT_N_START: i32 = 0xd7;
/// hold counter after a bias step
const HOLD_FRAMES: i32 = 0x28;

/// Inputs of `_CreateBitstreamEncoder`/`_initBitCountStatistics` (engine values).
#[derive(Clone, Copy, Debug)]
pub struct RateConfig {
    /// sample rate in Hz
    pub sample_rate: u32,
    /// total bit rate `'brat'` (256000), the same for every channel count
    pub brat: u32,
    /// frame length (1024)
    pub frame_len: u32,
    /// coded channels (`BSE+0x35c`; includes LFE)
    pub nch: usize,
    /// `'acbf'` bit-rate control mode: 1 = LongTermAverage (engine default), 2 = constrained VBR. 0 (CBR reservoir) and 3 (VBR) are not implemented.
    pub acbf: u8,
}

/// Result of [`RateState::after_au`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct BiasStep {
    /// bias index B after this AU (`pam+0x1a0`); feed it to the psy stage (`_adaptPamParam`) when `stepped`.
    pub b: i32,
    /// B changed in this AU (`_adaptPamParam` must be re-run).
    pub stepped: bool,
}

/// `BSE` statistics + bias `B` of the LongTermAverage rate control.
#[derive(Clone, Debug)]
pub struct RateState {
    /// `BSE+0x438` AUs counted (saturates at 155000)
    pub n: i32,
    /// `BSE+0x418` running sum of AU bits
    pub sum: f64,
    /// `BSE+0x420` average bits per AU (cumulative mean, EMA tau=300 s after n > 154999)
    pub avg: f32,
    /// `BSE+0x424` EMA tau=5 s
    pub ema5: f32,
    /// `BSE+0x428` S: smoothed rate indicator (reset on every B step)
    pub s: f32,
    /// `BSE+0x440` hold counter (40 after a step, decremented per AU, floor -2)
    pub hold: i32,
    /// `BSE+0x444` kernel MAX bits (100000 default; cumulative bound from AU 216)
    pub max_bits: i32,
    /// `BSE+0x448` kernel MIN bits (0 default)
    pub min_bits: i32,
    /// `pam+0x1a0` bias index B in -4..=4
    pub b: i32,
    /// N = 1024/sr*brat (float), `BSE+0x42c/0x454`
    pub n_mean: f32,
    /// Lo = 0.95 N, Hi = 1.05 N (`BSE+0x450/0x458`)
    pub lo: f32,
    pub hi: f32,
    /// `BSE+0x430/0x434`: EMA coefficients
    pub a5: f32,
    pub a300: f32,
    /// `BSE+0x43c` hold counter of the constrained-VBR (`acbf=2`) feedback (200 after a step)
    pub hold1: i32,
    mode1: bool,
}

impl RateState {
    /// `_CreateBitstreamEncoder` + `_initBitCountStatistics` for `acbf = 1` (engine) or `acbf = 2`.
    pub fn new(cfg: &RateConfig) -> Self {
        debug_assert!(cfg.acbf == 1 || cfg.acbf == 2, "acbf 0 (CBR reservoir) and 3 (VBR) are not implemented");
        let mode1 = cfg.acbf == 2;
        let dt = (cfg.frame_len as f32) / (cfg.sample_rate as f32);
        let n = dt * (cfg.brat as f32);
        let (lo, hi, avg0) = if mode1 {
            // constrained VBR: Lo = 0.98 N, Hi = N * (1 + {.17,.17,.24}[class of the per-channel rate]), S = avg = (Lo+Hi)/2
            let per_ch = cfg.brat / cfg.nch as u32;
            let cls = if per_ch < 0x3e81 { 0 } else if per_ch > 0x1769c { 2 } else { 1 };
            let extra = [0.17f32, 0.17f32, 0.24f32][cls];
            let lo = n * 0.98f32;
            let hi = n * (extra + 1.0f32);
            (lo, hi, (lo + hi) * 0.5f32)
        } else {
            (n * 0.95f32, n * 1.05f32, n)
        };
        let a5 = expf(dt / -5.0f32);
        let a300 = expf(dt / -300.0f32);
        Self {
            n: 0,
            sum: 0.0,
            avg: avg0,
            ema5: n,
            s: avg0,
            hold: HOLD_FRAMES,
            max_bits: 100_000,
            min_bits: 0,
            b: 0,
            n_mean: n,
            lo,
            hi,
            a5,
            a300,
            hold1: 0,
            mode1,
        }
    }

    /// Budget handed to [`crate::portable::aac::quant::kernel::LoopKernel::advance_loop_kernel`] for this AU.
    pub fn kernel_budget(&self, side_bits: i32) -> crate::portable::aac::quant::kernel::KernelBudget {
        crate::portable::aac::quant::kernel::KernelBudget::engine(side_bits, self.max_bits, self.min_bits)
    }

    /// `AACEncoder::AACEncodeOutput` statistics and bias feedback for one written AU of `au_bytes` bytes. `silent` = every channel has
    /// `max_sfb == 0` (the statistics still ingest the AU, `B` is not adapted).
    pub fn after_au(&mut self, au_bytes: u32, silent: bool) -> BiasStep {
        let bits_i = (au_bytes as i64 * 8) as i32;
        let bits_f = bits_i as f32;
        self.max_bits = 100_000;
        self.min_bits = 0;
        if self.hold >= -1 {
            self.hold -= 1;
        }
        let n_old = self.n;
        if n_old <= STAT_N_LIMIT {
            self.n = n_old + 1;
            self.sum += bits_i as f64;
            self.avg = (self.sum / self.n as f64) as f32;
            if n_old >= STAT_N_START {
                let s1 = (n_old + 2) as f32;
                let lo_line = self.lo * s1;
                let d2 = lo_line as f64 - self.sum;
                let s3 = self.lo * f32::from_bits(0x3f40_0000); // 0.75f
                if d2 <= s3 as f64 {
                    if d2 > 0.0 {
                        self.min_bits = d2 as i32;
                    }
                } else {
                    self.min_bits = fcvtzs(s3);
                }
                let hi_line = self.hi * s1;
                let d0 = hi_line as f64 - self.sum;
                let s1b = self.hi * f32::from_bits(0x3fa6_6666); // 1.3f
                if d0 < s1b as f64 {
                    self.max_bits = fcvtzs(s1b);
                } else if d0 < 100_000.0 {
                    self.max_bits = d0 as i32;
                }
            }
        } else {
            let a = self.a300;
            let s2 = (1.0f32 - a) * bits_f;
            self.avg = fma(a, self.avg, s2);
        }
        let a5 = self.a5;
        let s3 = 1.0f32 - a5;
        let s4 = s3 * bits_f;
        self.ema5 = fma(a5, self.ema5, s4);
        let s3h = s3 * 0.5f32;
        let s2s = self.avg + bits_f;
        let s2s = s3h * s2s;
        self.s = fma(a5, self.s, s2s);
        if silent {
            return BiasStep { b: self.b, stepped: false };
        }
        if self.mode1 {
            return self.constrained_vbr_feedback();
        }
        // ---- bias feedback (decision tree 0012d89c..0012db98)
        let i_n = fcvtzs(self.n_mean);
        let n_f = i_n as f32;
        let (s, hi, lo, n, b) = (self.s, self.hi, self.lo, self.n, self.b);
        let m = (hi + n_f) * 0.5f32;
        let dec_n = |st: &mut Self| {
            st.b = b - 1;
            st.hold = HOLD_FRAMES;
            let v = if n < 0xd8 {
                (hi + (3 * i_n) as f32) * 0.25f32
            } else {
                let t = if s < hi { s } else { hi };
                (t + n_f) * 0.5f32
            };
            st.s = v;
        };
        let dec_b = |st: &mut Self| {
            st.b = b - 1;
            st.hold = HOLD_FRAMES;
            let t = if s < hi { s } else { hi };
            st.s = (t + n_f) * 0.5f32;
        };
        let inc = |st: &mut Self| {
            st.b = b + 1;
            st.hold = HOLD_FRAMES;
            let s3 = (lo + (3 * i_n) as f32) * 0.25f32;
            let t = if s > lo { s } else { lo };
            let s0 = (t + n_f) * 0.5f32;
            st.s = if n < 0xd8 { s3 } else { s0 };
        };
        if s > m {
            if n < 0x285 {
                if n < 0x1ae {
                    if b > -2 {
                        dec_n(self);
                        return BiasStep { b: self.b, stepped: true };
                    }
                } else if b > -3 {
                    dec_b(self);
                    return BiasStep { b: self.b, stepped: true };
                }
            } else if b > -4 {
                dec_b(self);
                return BiasStep { b: self.b, stepped: true };
            }
        }
        if s > n_f && b >= 1 && self.hold <= 0 {
            dec_n(self);
            return BiasStep { b: self.b, stepped: true };
        }
        let mlo = (lo + n_f) * 0.5f32;
        if s < mlo && b < 4 {
            inc(self);
            return BiasStep { b: self.b, stepped: true };
        }
        if s < n_f && b < 0 && self.hold <= 0 {
            inc(self);
            return BiasStep { b: self.b, stepped: true };
        }
        BiasStep { b: self.b, stepped: false }
    }
}

impl RateState {
    /// `acbf = 2` feedback (0012d770..0012da70).
    fn constrained_vbr_feedback(&mut self) -> BiasStep {
        let (s, avg, lo, hi, ema, b) = (self.s, self.avg, self.lo, self.hi, self.ema5, self.b);
        let tail = |st: &mut Self, s1: f32| {
            let s0 = st.lo + st.hi;
            let s0 = fma(s0, 0.5f32, s1);
            let s8 = s0 * 0.5f32;
            if st.hold1 > 0 {
                st.hold1 -= 1;
            }
            st.s = s8;
        };
        let dec = |st: &mut Self| {
            st.b = b - 1;
            st.hold1 = 200;
            let s1 = if s < hi { s } else { hi };
            tail(st, s1);
        };
        let inc = |st: &mut Self| {
            st.b = b + 1;
            st.hold1 = 200;
            let s1 = if s > lo { s } else { lo };
            tail(st, s1);
        };
        if !(s > hi * 0.98f32 && b > -4) {
            let b5 = avg < hi * 0.95f32;
            let w10 = avg > lo * 1.05f32 && b5;
            if w10 && b >= 1 && self.hold1 <= 0 && ema > avg {
                dec(self);
                return BiasStep { b: self.b, stepped: true };
            }
            if (s < lo * 1.02f32 && b < 4) || (w10 && b < 0 && self.hold1 <= 0 && ema < avg) {
                inc(self);
                return BiasStep { b: self.b, stepped: true };
            }
            if self.hold1 > 0 {
                self.hold1 -= 1;
            }
            self.s = s;
            return BiasStep { b: self.b, stepped: false };
        }
        dec(self);
        BiasStep { b: self.b, stepped: true }
    }
}

// ---------------------------------------------------------------------------------------------------------------------------------
// per-AU budget block of _aacEncodeFrame: side information bits

/// `ICS(w) = {11, 11, 15, 11}[w]`: ics_info bits (long 1+2+1+6+1, short 1+2+1+4+7).
#[inline]
pub fn ics_info_bits(window_sequence: u32) -> i32 {
    if window_sequence == 2 { 15 } else { 11 }
}

/// One syntactic element as the budget block sees it.
#[derive(Clone, Copy, Debug)]
pub struct ElementSide {
    pub kind: ElementKind,
    /// CPE: both channels share ics_info (always true in the engine: `commonWindowMode = 2`)
    pub common_window: bool,
    /// CPE with common window: `ms_mask_present == 1` (mixed M/S) -> the mask bits `sum_g max_sfb[g]` are counted
    pub ms_mask_mixed: bool,
    /// `sum over window groups of max_sfb` of the shared ics_info (CPE common window)
    pub ms_mask_nbands: i32,
    /// window_sequence of channel 0 / channel 1
    pub ws: [u32; 2],
    /// `_TnsBitCount` of channel 0 / 1 (data bits without the present flag)
    pub tns_bits: [i32; 2],
}

/// Side-information bits `BSE[0x3c0]` of one AU: element headers + ics_info + M/S mask + TNS data + the 3-bit ID_END
/// (`extra` = `GetAdditionalBits()` (0), `pce_bits` = `8*BSE[0x480]` for the first AU of PCE layouts, else 0).
/// `SCE: 18 + tns + ICS`, `LFE: 29`, `CPE: 30 + tns0 + tns1 + (common ? 2 + mask + ICS(ws0) : ICS(ws0)+ICS(ws1))`.
pub fn side_info_bits(extra: i32, pce_bits: i32, elements: &[ElementSide]) -> i32 {
    let mut s = extra + pce_bits;
    for e in elements {
        match e.kind {
            ElementKind::Sce => s += 18 + e.tns_bits[0] + ics_info_bits(e.ws[0]),
            ElementKind::Lfe => s += 29,
            ElementKind::Cpe => {
                s += 30 + e.tns_bits[0] + e.tns_bits[1];
                if e.common_window {
                    s += 2 + if e.ms_mask_mixed { e.ms_mask_nbands } else { 0 } + ics_info_bits(e.ws[0]);
                } else {
                    s += ics_info_bits(e.ws[0]) + ics_info_bits(e.ws[1]);
                }
            }
        }
    }
    s + 3
}

/// AU size (`WriteBitstream`, `acbf != 0`): `ceil((side + sum L[ch]) / 8)` bytes (`ID_END` + zero bits to the byte boundary).
pub fn au_bytes(side_bits: i32, loop_bits: &[i32]) -> u32 {
    let total: i64 = side_bits as i64 + loop_bits.iter().map(|&x| x as i64).sum::<i64>();
    ((total + 7) / 8) as u32
}
