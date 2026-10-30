//! F5: temporal noise shaping of Apple's AAC-LC encoder:
//! `_SetTnsParamChannel` (0x9134) -> [`TnsConfig`], the inline analysis of `EncodeFrame` 0x1301f0..0x1308b0 ([`analyze_window`]),
//! `_processTnsChannel` (0x4aa8c), `_TnsEncodeSingleFilter` (0x4af80), `_CalculateTnsEnergies` (0x4b460), the psy-copy pass 0x130a64..0x130b78 and
//! the psy-threshold weights (0x4aa8c tail, consumed at 0x130f60..0x131098).
//!
//! TNS is open loop and stateless between frames: one candidate filter per window (1 long window or 8 short windows), applied in place to the
//! coding spectrum after the MDCT and before the psychoacoustic model.
//!
//! Bit-exactness: `f32::mul_add` exactly at: envelope smoothing `fma(prev, 0.85f, y*0.15f)`, Schur `fma(k, g, h)` / `fma(k, h, g)`, LPC step-up,
//! FIR accumulation `acc = fma(a_t, x[i+t], acc)`, and inside `vDSP_dotpr` ([`vdsp::dotpr`]: 8 x 4-lane accumulators, tree reduction). Everything else is plain
//! f32. `asin` is replaced by 15 exact monotone thresholds (4-bit coefficients only, the only resolution the engine uses); `sin` of the 16 dequantised
//! PARCORs and `exp` of the lag windows use the Apple-exact `crate::portable::math`.

use crate::portable::aac::bits::sf_index_of_rate;
use crate::portable::aac::bits::tables::{SWB_LONG, SWB_SHORT};
use crate::portable::aac::fb::gen_tables::*;
use crate::portable::math;
use crate::portable::aac::types::{TnsChannel, TnsFilter, TnsRanges, TnsWindow, F_0_001, F_0_1};
use crate::portable::vdsp;

/// Floats per sub-block of the psy-threshold weight array (`T+0x790 + i*0x1cc`, 0x1cc bytes = 115 floats).
pub const TNS_WEIGHT_STRIDE: usize = 115;
const EPS30: f32 = f32::from_bits(0x0da24260); // 1e-30f (0x922934)
const C015: f32 = f32::from_bits(0x3e199998); // 0x922930
const C085: f32 = f32::from_bits(0x3f59999a); // 0x922768
const C0001: f32 = F_0_001; // 0.001f (0x9225e0)
const NEG0001: f32 = f32::from_bits(0xba83126f); // -0.001f (0x9228d0)
const ORDER_THR: f32 = F_0_1; // 0.1f (0x9225f8)
const W_TNS: f32 = f32::from_bits(0x3fd5c28f); // psy threshold weight 1.67f
const EPS_RATIO: f32 = f32::from_bits(0x34000000); // 2^-23
const PI_F: f32 = std::f32::consts::PI; // 0x922728
const COEF_BITS: usize = 4;

/// `_FreqToBandWithRounding` (0x9594): nearest scalefactor-band edge of `freq` Hz (ties to the lower edge).
pub fn freq_to_band(freq: f32, sr: f32, numsfb: usize, offs: &[u16]) -> usize {
    let q = freq / sr;
    let total = offs[numsfb] as f32;
    let b = (total * (q + q) + 0.5f32) as i32; // fcvtzs (truncation)
    if b >= offs[numsfb] as i32 {
        return numsfb;
    }
    let mut k = numsfb;
    for j in 0..numsfb {
        if b < offs[j + 1] as i32 {
            k = j;
            break;
        }
    }
    if ((offs[k + 1] as i32) - b) < (b - offs[k] as i32) { k + 1 } else { k }
}

/// Band configuration of one window type (`TnsConfig`, 0x48 bytes).
#[derive(Clone, Debug, PartialEq)]
pub struct TnsBandCfg {
    /// idx 0: maximum analysis/filter order M (12 long / 7 short).
    pub max_order: usize,
    /// idx 1: minimum prediction gain (1.41f).
    pub min_gain: f32,
    /// idx 7 `A`: narrow band start sfb (also analysis range start).
    pub a: usize,
    /// idx 8 `E`: band stop sfb (analysis range stop).
    pub e: usize,
    /// idx 9 `B`: wide band start sfb (first attempt).
    pub b: usize,
    /// idx 11: number of sfbs of the window type.
    pub nsfb: usize,
    /// idx 15: end of the psy-weight band range.
    pub f3c: usize,
    /// idx 16: start of the psy-weight range (`FreqToBand(2600 Hz)`).
    pub f40: usize,
    /// idx 17: the window type is not processed (short only; `A >= E`).
    pub disabled: bool,
}

/// Per-channel static TNS configuration (everything `_SetTnsParamChannel` + `_aacOpen` leave in the TnsChannel object).
#[derive(Clone, Debug)]
pub struct TnsConfig {
    /// profile `(a = filter direction, b = flags (bit 1: psy weights), c, d = lag sigma factors)` selected by the per-channel bitrate.
    pub profile: (u16, u16, f32, f32),
    /// `TNS_MAX_BANDS` caps (T+4 long, T+8 short) of the sample rate.
    pub cap_long: i32,
    pub cap_short: i32,
    pub offs_long: Vec<u16>,
    pub offs_short: Vec<u16>,
    pub long: TnsBandCfg,
    pub short: TnsBandCfg,
    /// Gaussian lag windows (13 entries): `(float) exp((k + 0.5)^2 * (-0.5 sigma^2))`.
    pub lag_long: [f32; 13],
    pub lag_short: [f32; 13],
}

fn lag_window(sr: u32, c: f32, frame_len: u32, short: bool) -> [f32; 13] {
    let s9 = (sr as f32 * PI_F) * C0001;
    let s0 = s9 * c;
    let s0 = s0 / (if short { frame_len >> 3 } else { frame_len }) as f32;
    let s1 = s0 * -0.5f32;
    let s0 = s0 * s1;
    let d10 = s0 as f64;
    let mut out = [0.0f32; 13];
    for k in 0..13 {
        let d0 = k as f64 + 0.5;
        let d1 = d0 * d10;
        let d0 = d0 * d1;
        out[k] = math::exp(d0) as f32;
    }
    out
}

impl TnsConfig {
    /// `_aacOpen` + `_SetTnsParamChannel(T, sr, bitrate/ch, profileIdx, aot 2, 1024)`. `profile_idx` = channel count - 1 as observed (0 for mono: only
    /// matters for the 28001-36000 bit/s row). Returns `None` if the sample rate has no scalefactor table or the bitrate no profile row.
    pub fn new(sample_rate: u32, bitrate_per_channel: u32, profile_idx: u32) -> Option<Self> {
        // the scalefactor-band offsets of the ISO table of the rate (the original's `_sfbInfoTab_1024` has every rate but 7350 Hz)
        let sfi = usize::from(sf_index_of_rate(sample_rate).filter(|&i| i < 12)?);
        let (offs_long, offs_short) = (SWB_LONG[sfi].to_vec(), SWB_SHORT[sfi].to_vec());
        let (cap_long, cap_short) = TNS_MAX_BANDS_1024[sfi];
        let prow = TNS_INFO_TAB.iter().find(|r| r.0 <= bitrate_per_channel && bitrate_per_channel <= r.1)?;
        let profile = if profile_idx == 0 { prow.2 } else { prow.3 };
        let sr_f = sample_rate as f32;
        let mk = |para: &[i32; 7], offs: &[u16], cap: i32, short: bool| -> TnsBandCfg {
            let nsfb = offs.len() - 1;
            let cap_idx = if cap != -1 { cap as usize } else { nsfb };
            let ftb = |f: f32| freq_to_band(f, sr_f, nsfb, offs);
            let a = ftb(para[3] as f32);
            let e = ftb(para[4] as f32);
            let b = ftb(para[5] as f32);
            let f3c = freq_to_band(((sr_f * 0.5f32) * offs[cap_idx] as f32) / offs[nsfb] as f32, sr_f, nsfb, offs);
            let f40 = ftb(2600.0);
            TnsBandCfg {
                max_order: para[0] as usize,
                min_gain: f32::from_bits(para[1] as u32),
                a,
                e,
                b,
                nsfb,
                f3c,
                f40,
                disabled: short && a >= e,
            }
        };
        let long = mk(&DEFAULT_PARA_LONG, &offs_long, cap_long, false);
        let short = mk(&DEFAULT_PARA_SHORT, &offs_short, cap_short, true);
        Some(Self {
            profile,
            cap_long,
            cap_short,
            offs_long,
            offs_short,
            long,
            short,
            lag_long: lag_window(sample_rate, profile.2, 1024, false),
            lag_short: lag_window(sample_rate, profile.3, 1024, true),
        })
    }
}

/// Outcome of the analysis of one window (stage 1): prediction gain and PARCOR coefficients.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Analysis {
    pub gain: f32,
    pub parcor: [f32; 12],
}

/// Stage 1 for one (sub)block `x` (`W` lines): 6-pass envelope, normalisation, autocorrelation over sfb `[A, E)`, lag window, Schur recursion
/// (EncodeFrame 0x1301f0..0x130898). `quality` = `aac+0xe0` (0x20 selects the simplified envelope; not an engine configuration, so the fixtures do not reach it).
pub fn analyze_window(x: &[f32], band: &TnsBandCfg, offs: &[u16], lag: &[f32; 13], quality: u32) -> Analysis {
    let w = x.len();
    let m = band.max_order;
    let mut y = [0.0f32; 1024];
    for k in 0..w {
        y[k] = x[k].abs();
    }
    let mx;
    if quality == 0x20 {
        // |x| is its own envelope; mx = vDSP_maxv(|x|)
        let mut mm = y[0];
        for k in 1..w {
            if y[k] > mm {
                mm = y[k];
            }
        }
        mx = mm;
    } else {
        let fwd = |y: &mut [f32; 1024]| {
            let mut prev = 0.0f32;
            for k in 0..w {
                prev = prev.mul_add(C085, y[k] * C015);
                y[k] = prev;
            }
        };
        let bwd = |y: &mut [f32; 1024]| -> f32 {
            let mut prev = 0.0f32;
            let mut mxv = -1.0f32;
            for k in (0..w).rev() {
                prev = prev.mul_add(C085, y[k] * C015);
                y[k] = prev;
                if prev > mxv {
                    mxv = prev;
                }
            }
            mxv
        };
        fwd(&mut y);
        bwd(&mut y);
        fwd(&mut y);
        bwd(&mut y);
        fwd(&mut y);
        mx = bwd(&mut y);
    }
    let thr = if quality == 0x20 {
        mx * C0001
    } else if w >= 4 {
        mx * C0001
    } else {
        NEG0001
    };
    let inv = 1.0f32 / (thr + EPS30);
    let mut n = [0.0f32; 1024];
    for k in 0..w {
        n[k] = if y[k] < thr { inv * x[k] } else { x[k] / (y[k] + EPS30) };
    }
    let s = offs[band.a] as usize;
    let l = offs[band.e] as usize - s;
    let mut r = [0.0f32; 13];
    for j in 0..=m {
        r[j] = vdsp::dotpr(&n[s..s + l - j], &n[s + j..s + l]);
    }
    for j in 0..=m {
        r[j] = r[j] * lag[j];
    }
    // Schur / Le Roux-Gueguen recursion
    let mut g = [0.0f32; 12];
    let mut h = [0.0f32; 12];
    g[..m].copy_from_slice(&r[..m]);
    h[..m].copy_from_slice(&r[1..m + 1]);
    let mut parcor = [0.0f32; 12];
    for i in 0..m {
        let k = if g[0].to_bits() == 0 { 0.0f32 } else { (-h[i]) / g[0] };
        parcor[i] = k;
        for j in 0..m - i {
            let gj = g[j];
            let hj = h[i + j];
            h[i + j] = k.mul_add(gj, hj);
            g[j] = k.mul_add(hj, gj);
        }
    }
    let gain = (r[0] + EPS30) / (g[0] + EPS30);
    Analysis { gain, parcor }
}

/// 4-bit coefficient index of a PARCOR value: `(int16) trunc(q_scale * (float) asin(k) +- 0.5)` expressed with the 15 exact thresholds
/// (NaN or |k| > 1 -> 0; k == 1 -> 8, which does not fit the 4-bit field and was never observed).
fn quantize_parcor(k: f32) -> i32 {
    if k.is_nan() || k > 1.0 || k < -1.0 {
        return 0;
    }
    if k == 1.0 {
        return 8;
    }
    if k.is_sign_negative() {
        if k == 0.0 {
            return 0;
        }
        -(ASIN_NEG_THRESHOLDS.iter().filter(|&&t| t >= k).count() as i32)
    } else {
        ASIN_POS_THRESHOLDS.iter().filter(|&&t| t <= k).count() as i32
    }
}

/// `float(sin(double(iq * q)))` for a 4-bit index (`q` in -8..=8): the dequantised PARCOR.
fn dequantize_parcor(q: i32) -> f32 {
    let idx = COEF_BITS - 2;
    let arg: f32 = if q < 0 { IQ_TABLE_M[idx] * (q as f32) } else { IQ_TABLE[idx] * ((q as u16) as f32) };
    math::sin(arg as f64) as f32
}

/// Result of `_TnsEncodeSingleFilter`.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct SingleFilter {
    pub order: usize,
    /// int16 indices, all `M` entries written (only the first `order` are coded).
    pub q: [i16; 12],
    /// LPC coefficients a_1..a_order (stale beyond).
    pub lpc: [f32; 13],
}

/// `_TnsEncodeSingleFilter(spec, cap, offs, parcor, M, bits=4, filt, lpc)` (0x4af80): quantises `parcor[0..m]` (replaced in place by the dequantised
/// values), trims the order, converts to LPC and filters `spec[offs[start]..offs[min(stop, cap)]]` in place (direction 1: `y[i] = x[i] + sum a_t x[i+t]`,
/// samples beyond the range read as 0; the unfiltered samples are the history).
pub fn encode_single_filter(spec: &mut [f32], cap: usize, offs: &[u16], parcor: &mut [f32; 12], m: usize, start: usize, stop: usize, direction: u16) -> SingleFilter {
    let e = stop.min(cap);
    let sb = offs[start] as usize;
    let eb = offs[e] as usize;
    let len = eb - sb;
    let mut q = [0i16; 12];
    for i in 0..m {
        q[i] = quantize_parcor(parcor[i]) as i16;
    }
    for i in 0..m {
        parcor[i] = dequantize_parcor(q[i] as i32);
    }
    let mut order = 0usize;
    for pos in (0..m).rev() {
        if parcor[pos].abs() > ORDER_THR {
            order = pos + 1;
            break;
        }
    }
    let mut lpc = [0.0f32; 13];
    lpc[0] = parcor[0];
    if order > 1 {
        for mm in 1..order {
            let mut tmp = [0.0f32; 13];
            for j in 0..mm {
                tmp[j] = lpc[mm - 1 - j];
            }
            for j in 0..mm {
                lpc[j] = parcor[mm].mul_add(tmp[j], lpc[j]);
            }
            lpc[mm] = parcor[mm];
        }
    }
    if order == 0 {
        return SingleFilter { order: 0, q, lpc };
    }
    let x: Vec<f32> = spec[sb..eb].to_vec();
    let at = |i: isize| -> f32 { if i >= 0 && (i as usize) < len { x[i as usize] } else { 0.0 } };
    for i in 0..len {
        let mut acc = x[i];
        for t in 1..=order {
            let xv = if direction != 0 { at((i + t) as isize) } else { at(i as isize - t as isize) };
            acc = lpc[t - 1].mul_add(xv, acc);
        }
        spec[sb + i] = acc;
    }
    SingleFilter { order, q, lpc }
}

/// `_CalculateTnsEnergies`: `(E_orig, E_filt)` over bins `[start, stop)`; an empty range returns `(1.0, 100.0)`.
fn tns_energies(orig: &[f32], filt: &[f32], start: usize, stop: usize) -> (f32, f32) {
    if stop <= start {
        return (1.0, 100.0);
    }
    let ef = vdsp::dotpr(&filt[start..stop], &filt[start..stop]);
    let eo = vdsp::dotpr(&orig[start..stop], &orig[start..stop]);
    (eo, ef)
}

/// Per-window record of the TnsChannel object (`R_i`, stride 0xe0) restricted to the defined fields.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct TnsWindowRec {
    /// R+0x04: number of filters (0/1).
    pub count: u8,
    /// R+0x00: prediction gain of the analysis.
    pub gain: f32,
    /// filter start/stop sfb (R+0x0c/+0x10), order, direction, final gain ratio `min(6, Eo/(Ef + 2^-23))`.
    pub start: usize,
    pub stop: usize,
    pub order: usize,
    pub direction: u16,
    pub gain_ratio: f32,
    /// R+0x74: effective max band (cap or numSfb), R+0x6c: numSfb, R+0x70: analysis order M.
    pub cap_eff: usize,
    pub nsfb: usize,
    pub m: usize,
    /// int16 coefficient indices (R+0x20, M entries) and the dequantised PARCORs (R+0x80).
    pub q: [i16; 12],
    pub parcor: [f32; 12],
    pub lpc: [f32; 13],
}

impl Default for TnsWindowRec {
    fn default() -> Self {
        Self {
            count: 0,
            gain: 0.0,
            start: 0,
            stop: 0,
            order: 0,
            direction: 0,
            gain_ratio: 0.0,
            cap_eff: 0,
            nsfb: 0,
            m: 0,
            q: [0; 12],
            parcor: [0.0; 12],
            lpc: [0.0; 13],
        }
    }
}

/// The TnsChannel object of one channel after `_processTnsChannel`.
#[derive(Clone, Debug)]
pub struct TnsResult {
    /// T[0]: at least one window accepted a filter.
    pub active: bool,
    /// T+0x0c: number of sub-blocks (1 long, 8 short); 0 if the channel was not processed (disabled window type).
    pub nsub: usize,
    pub windows: [TnsWindowRec; 8],
    /// Psy threshold weights, Apple's `T+0x790 + i*0x1cc` layout: always `8 * 115` floats, window `i` at `weights[i*115 + k]` (long frames: `[k]`),
    /// 1.0 or 1.67; the first `nsub` windows are the ones written by this frame's `_processTnsChannel` (the rest stay 1.0). The psy model multiplies the
    /// thresholds `thr[k] *= weights[i*115 + k]` of window `i` when `active`.
    pub weights: Vec<f32>,
    /// Window type processed: `true` = EIGHT_SHORT.
    pub short: bool,
}

impl TnsResult {
    pub fn inactive(short: bool) -> Self {
        Self { active: false, nsub: 0, windows: [TnsWindowRec::default(); 8], weights: vec![1.0f32; 8 * TNS_WEIGHT_STRIDE], short }
    }

    /// Syntax elements as coded (`_encodeTnsData`, 0x3dac4): `crate::portable::aac::types::TnsChannel` (`coef_res` 1 = 4-bit coefficients; `coef_compress` when all
    /// indices lie in [-4, 3]; coded values are the low `4 - compress` bits of the two's complement indices).
    pub fn to_types(&self) -> TnsChannel {
        let mut ranges = TnsRanges { present: self.active, ..Default::default() };
        let mut ch = TnsChannel { active: self.active, windows: [TnsWindow::default(); 8], weights: self.weights.clone(), ranges };
        for i in 0..self.nsub.min(8) {
            let r = &self.windows[i];
            if r.count == 0 {
                continue;
            }
            // Apple's per-filter `{start_sfb, end_sfb}` at tns+0x1c+0xe0*w+0x30*f (f = 0): `end` = R.stop = numSfb (the filter itself is limited to min(stop, cap)),
            // `start` = B (wide) or A (narrow retry); `end - start` = the coded filter length
            ranges.count[i] = 1;
            ranges.range[i][0] = (r.start as i32, r.stop as i32);
            let compress = if r.q[..r.m].iter().all(|&v| (-4..=3).contains(&v)) { 1u8 } else { 0u8 };
            let nb = COEF_BITS as u8 - compress;
            let mut f = TnsFilter { length: (r.nsfb - r.start) as u8, order: r.order as u8, direction: 0, coef_compress: 0, coefs: [0; 12] };
            if r.order > 0 {
                f.direction = r.direction as u8;
                f.coef_compress = compress;
                for k in 0..r.order {
                    f.coefs[k] = (r.q[k] as u16 & ((1u16 << nb) - 1)) as u8;
                }
            }
            ch.windows[i] = TnsWindow { n_filt: 1, coef_res: 1, filter: f };
        }
        ch.ranges = ranges;
        ch
    }
}

/// Stages 1 and 2 for one channel: analyse every window of `spec` (the coding spectrum right after the MDCT; window order for EIGHT_SHORT),
/// then `_processTnsChannel` filters it in place with accept/reject, one retry with the narrow band, and the psy weights.
/// `ws` = window sequence, `bw_line` = number of lines below the encoder bandwidth of this channel and window type (`x4+0xc`: the cutoff line, e.g.
/// 1024/128 full band, 929/116 at 48 kHz stereo), which limits the energy comparison to bins `< bw_line - 1`. `quality` = `aac+0xe0`.
pub fn process_channel(cfg: &TnsConfig, quality: u32, spec: &mut [f32; 1024], ws: u8, bw_line: usize) -> TnsResult {
    let short = ws == 2;
    let (band, offs, lag, cap) = if short { (&cfg.short, cfg.offs_short.as_slice(), &cfg.lag_short, cfg.cap_short) } else { (&cfg.long, cfg.offs_long.as_slice(), &cfg.lag_long, cfg.cap_long) };
    let mut res = TnsResult::inactive(short);
    if band.disabled {
        return res;
    }
    let nsub = if short { 8 } else { 1 };
    res.nsub = nsub;
    let n_total = 1024usize;
    let nsfb = band.nsfb;
    let w = offs[nsfb] as usize;
    // stage 1: analysis of all windows on the unfiltered spectrum
    let mut an = [Analysis { gain: 0.0, parcor: [0.0; 12] }; 8];
    for i in 0..nsub {
        let off = if short { (n_total * i) / 8 } else { 0 };
        an[i] = analyze_window(&spec[off..off + w], band, offs, lag, quality);
    }
    // stage 2: processTnsChannel
    let cap_eff = if cap == -1 { nsfb } else { cap as usize };
    let lim = bw_line as isize - 1;
    let (gfac, thr1) = if short { (1.0f32, 2.2f32) } else { (1.2f32, 1.4f32) };
    let long_offs = cfg.offs_long.as_slice();
    for i in 0..nsub {
        let off = if short { (n_total * i) / 8 } else { 0 };
        let backup: [f32; 1024] = *spec;
        let rec = &mut res.windows[i];
        rec.cap_eff = cap_eff;
        rec.nsfb = nsfb;
        rec.m = band.max_order;
        rec.gain = an[i].gain;
        rec.parcor = an[i].parcor;
        rec.count = 0;
        let gain = an[i].gain;
        if gain < band.min_gain {
            continue; // NaN is not below the threshold (fcmp mi)
        }
        rec.count = 1;
        rec.start = band.b;
        rec.stop = nsfb;
        rec.direction = cfg.profile.0;
        rec.gain_ratio = gain;
        let wlen = if short { n_total / 8 } else { n_total };
        let sub_len = (off + wlen).min(n_total);
        let accept = |eo: f32, ef: f32| -> bool { !(eo < thr1 * ef) && !(gfac * eo < gain * ef) };
        // first attempt: wide band from B
        let f = encode_single_filter(&mut spec[off..sub_len], cap_eff, offs, &mut rec.parcor, rec.m, rec.start, rec.stop, rec.direction);
        rec.order = f.order;
        rec.q = f.q;
        rec.lpc = f.lpc;
        let sb0 = offs[rec.start] as usize;
        let stop_sfb = rec.stop.min(cap_eff);
        let eb = (lim.min(offs[stop_sfb] as isize)).max(0) as usize;
        let mut sb = sb0;
        let (mut eo, mut ef) = tns_energies(&backup[off..sub_len], &spec[off..sub_len], sb, eb);
        if !accept(eo, ef) {
            if rec.start < band.a {
                spec[off..sub_len].copy_from_slice(&backup[off..sub_len]);
                rec.start = band.a;
                let f = encode_single_filter(&mut spec[off..sub_len], cap_eff, offs, &mut rec.parcor, rec.m, rec.start, rec.stop, rec.direction);
                rec.order = f.order;
                rec.q = f.q;
                rec.lpc = f.lpc;
                sb = offs[band.a] as usize;
                let r = tns_energies(&backup[off..sub_len], &spec[off..sub_len], sb, eb);
                eo = r.0;
                ef = r.1;
            }
            if !accept(eo, ef) {
                spec[off..sub_len].copy_from_slice(&backup[off..sub_len]);
                rec.count = 0;
                continue;
            }
        }
        let mut ratio = eo / (ef + EPS_RATIO);
        if ratio > 6.0 {
            ratio = 6.0;
        }
        res.active = true;
        rec.gain_ratio = ratio;
        if (cfg.profile.1 >> 1) & 1 != 0 {
            let mut j = 0usize;
            while (long_offs[j] as usize) < sb {
                j += 1;
            }
            let mut idx = band.f40.max(j);
            while idx < band.f3c {
                if ratio >= 4.0 {
                    res.weights[i * TNS_WEIGHT_STRIDE + idx] = W_TNS;
                }
                idx += 1;
            }
        }
    }
    res
}

/// Stage 3 (EncodeFrame 0x130a64..0x130b78): apply the accepted filters to a psy copy of the spectrum (`0.5 * X`: `psy` holds 1024 floats; for
/// EIGHT_SHORT the eight 128-line windows in window order, each filtered by its own window's filter). Re-runs `_TnsEncodeSingleFilter` on the
/// stored (dequantised) PARCORs exactly as the binary does; the result is bit-exactly `0.5 *` the filtered coding spectrum.
pub fn apply_to_psy_copy(cfg: &TnsConfig, res: &TnsResult, psy: &mut [f32]) {
    if !res.active {
        return;
    }
    let offs: &[u16] = if res.short { &cfg.offs_short } else { &cfg.offs_long };
    let wlen = if res.short { 128 } else { 1024 };
    for i in 0..res.nsub {
        let r = &res.windows[i];
        if r.count == 0 {
            continue;
        }
        let off = if res.short { (1024 * i) / 8 } else { 0 };
        let mut par = r.parcor;
        encode_single_filter(&mut psy[off..off + wlen], r.cap_eff, offs, &mut par, r.m, r.start, r.stop, r.direction);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn quantiser_edge_cases() {
        assert_eq!(quantize_parcor(0.0), 0);
        assert_eq!(quantize_parcor(-0.0), 0);
        assert_eq!(quantize_parcor(f32::NAN), 0);
        assert_eq!(quantize_parcor(1.5), 0);
        assert_eq!(quantize_parcor(-1.5), 0);
        assert_eq!(quantize_parcor(1.0), 8);
        assert_eq!(quantize_parcor(-1.0), -8);
        assert_eq!(quantize_parcor(0.99999994), 7);
        assert_eq!(quantize_parcor(0.5), 3); // threshold value itself selects the upper cell
        assert_eq!(quantize_parcor(-0.09226836), -1);
        assert_eq!(quantize_parcor(-0.0922683), 0);
    }

    #[test]
    fn dequantiser_table_symmetry() {
        assert_eq!(dequantize_parcor(0), 0.0);
        for q in 1..=7 {
            assert!(dequantize_parcor(q) > dequantize_parcor(q - 1));
            assert!(dequantize_parcor(-q) < dequantize_parcor(-q + 1));
        }
    }

    #[test]
    fn freq_to_band_known_values() {
        // 44.1 kHz long/short: A/E/B/f3c/f40 = 19/42/12/42/19 and 5/12/4/14/4
        let c = TnsConfig::new(44100, 128000, 1).unwrap();
        let l = &c.long;
        assert_eq!((l.a, l.e, l.b, l.f3c, l.f40), (19, 42, 12, 42, 19));
        let s = &c.short;
        assert_eq!((s.a, s.e, s.b, s.f3c, s.f40), (5, 12, 4, 14, 4));
        let c = TnsConfig::new(48000, 128000, 1).unwrap();
        assert_eq!((c.long.a, c.long.e, c.long.b, c.long.f3c, c.long.f40), (18, 40, 12, 40, 18));
        assert_eq!((c.short.a, c.short.e, c.short.b, c.short.f3c, c.short.f40), (5, 11, 4, 14, 3));
        assert_eq!((c.cap_long, c.cap_short), (40, 14));
    }
}
