//! S0 shared kernels of the M/S stage: the perceptual-entropy kernels `_EvalPercepEntropyByLine` (0x3d180, fast NEON variant: `enc+0xe0` in
//! {0x20, 0x40}) and `_EvalPercepEntropyByLine_HQ` (0x3d318, scalar double variant: every other value, i.e. 0x60), plus `calcChannelPE`
//! (toolmain.c:0x281, asm 0x390c0..) that evaluates them over every (group, band) of a channel.  The `dB()` approximation and the `pow2`
//! estimate are `crate::portable::aac::psy::{db, pow2_est}`.
//!
//! Numeric rules (asm of 0x3d180): `est = FRECPE(thr); inv = est * FRECPS(thr, est); K = inv * n;`
//! per 4 lines `t = K * (x * x)`, `cnt += (t > 1)`, `prod = fmax(t, 1) * fmin(prod, 1e38)` (4 lanes, `prod` starts at 1);
//! `P = (p0*p1)*(p2*p3)`, `lg = fma(float(exp(P)-127), 3.0103, logMantissa[mant(P)])`, `PE = fma(lg, 0.1660964, float(count))`.
//! Only `n & ~3` lines are read; `n < 4` leaves `P = 1`, `count = 0` (so `PE = fma(logMantissa[0], 0.1661, 0) = 0.0028`, NOT 0).
//! Plain IEEE f32 everywhere (no FZ emulation).

use crate::portable::aac::psy::db;
use crate::portable::aac::types::PsyOut;
use crate::portable::arm::{frecpe_f32, frecps_f32, max_propagating, min_propagating};

/// `DAT_0092266c` = log2(10)/20: PE scale.
pub const K_PE: f32 = f32::from_bits(0x3e2a152d);
/// the `1e38` product clamp of the PE kernel (immediate 0x7e967699).
const PROD_CLAMP: f32 = f32::from_bits(0x7e96_7699);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MsError {
    /// `_EvalPercepEntropyByLine_HQ` rejects `thr <= FLT_MIN` and `n <= 0` (core error -> `__handBack`, the frame fails).
    PeInvalidArgument,
    /// `MSTool+8` is not 0/1/2 (`"this is not a valid ms debug mode"`, ms_stereo.c:0x2df).
    InvalidMsDebugMode,
}

impl std::fmt::Display for MsError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::PeInvalidArgument => f.write_str("perceptual entropy of a band with a threshold at or below FLT_MIN or no lines"),
            Self::InvalidMsDebugMode => f.write_str("invalid M/S debug mode"),
        }
    }
}
impl std::error::Error for MsError {}

/// `_EvalPercepEntropyByLine(thr, x, n, &pe)`: perceptual entropy of one band (`n` lines of `x`), fast NEON variant.
pub fn eval_percep_entropy_by_line(thr: f32, x: &[f32], n: u32) -> f32 {
    let mut prod = [1.0f32; 4];
    let mut cnt = [0i32; 4];
    if n >= 4 {
        let est = frecpe_f32(thr);
        let stp = frecps_f32(thr, est);
        let inv = est * stp;
        let k = inv * (n as f32);
        let nv = (n >> 2) as usize;
        for it in 0..nv {
            for l in 0..4 {
                let v = x[it * 4 + l];
                let t = k * (v * v);
                cnt[l] += (t > 1.0) as i32;
                let tm = max_propagating(t, 1.0);
                let pm = min_propagating(prod[l], PROD_CLAMP);
                prod[l] = tm * pm;
            }
        }
    }
    let p = (prod[0] * prod[1]) * (prod[2] * prod[3]);
    let count = (cnt[2] + cnt[3]) + (cnt[0] + cnt[1]);
    let pe = db(p).mul_add(K_PE, count as f32);
    // The kernel's fallback to the scalar double loop (asm 0x3d26c..0x3d2dc) is taken for a NaN. OPEN POINT: whether Apple tests the entropy `pe` (as
    // here) or the product `p` is not decided. A NaN only arises for thr = 0 (FRECPE(0) = inf, so `K` is inf and a band with a zero line has a NaN
    // product), and `db()` of a NaN is finite (it works on the bit pattern), so the test of `pe` below never fires, whereas a test of `p` would send
    // such a band down the scalar loop. No band with thr = 0 occurs in any input the port was compared with, and encoding with "test `pe`", "test `p` for
    // NaN" and "never fall back" gives the same streams on all of them, so the evidence does not separate the three. (A test of `p` for infinity does
    // change streams: infinite products are common and Apple does not fall back for them.)
    if (pe.to_bits() | 0x40_0000) == 0x7fc0_0000 {
        let (cnt, p) = scalar_double_loop(thr, x, n as i32);
        return db(p as f32).mul_add(K_PE, cnt as f32);
    }
    pe
}

/// The scalar double-precision loop shared by the NaN fallback of 0x3d180 (asm 0x3d26c..0x3d2dc) and `_EvalPercepEntropyByLine_HQ`
/// (0x3d330..0x3d398): `d0 = double(float(n) / thr)`; per line `t = d0 * double(x*x)`; while `t > 1`: `t = min(t, 1e9)`,
/// `p = min(p, 1e290) * t`, `count += 1`.  Returns `(count, p)`.
fn scalar_double_loop(thr: f32, x: &[f32], n: i32) -> (i32, f64) {
    let mut p = 1.0f64;
    let mut cnt = 0i32;
    if n >= 1 {
        let d0 = ((n as f32) / thr) as f64;
        for i in 0..n as usize {
            let xx = x[i] * x[i];
            let mut t = d0 * (xx as f64);
            if t > 1.0 {
                if t > 1.0e9 {
                    t = 1.0e9;
                }
                let pp = if p > 1.0e290 { 1.0e290 } else { p };
                p = pp * t;
                cnt += 1;
            }
        }
    }
    (cnt, p)
}

/// `_EvalPercepEntropyByLine_HQ(thr, x, n, &pe)` (0x3d318): scalar double variant used when `enc+0xe0` is not 0x20/0x40 (engine: 0x60 only).
/// Returns an error (`core_error`) for `thr <= FLT_MIN` or `n <= 0`.  Unlike the NaN fallback of the fast kernel it clamps the product to FLT_MAX.
pub fn eval_percep_entropy_by_line_hq(thr: f32, x: &[f32], n: u32) -> Result<f32, MsError> {
    if thr <= f32::from_bits(0x0080_0000) || (n as i32) <= 0 {
        return Err(MsError::PeInvalidArgument);
    }
    let (cnt, mut p) = scalar_double_loop(thr, x, n as i32);
    let flt_max = f32::MAX as f64;
    if p >= flt_max {
        p = flt_max;
    }
    Ok(db(p as f32).mul_add(K_PE, cnt as f32))
}

/// PE of one band with the kernel selected by `quality` (= `enc+0xe0`: 0x20/0x40 fast, otherwise HQ).
#[inline]
pub fn eval_pe(quality: u32, thr: f32, x: &[f32], n: u32) -> Result<f32, MsError> {
    if quality == 0x40 || quality == 0x20 {
        Ok(eval_percep_entropy_by_line(thr, x, n))
    } else {
        eval_percep_entropy_by_line_hq(thr, x, n)
    }
}

/// `calcChannelPE` (toolmain.c:0x281; first loop of `AdvanceTools`): `po.pe[g * stride + b] = PE(po.thr[idx], po.spectrum[start[idx]..], width[idx])`
/// for every group `g < ngroups` and band `b < max_sfb`.  Call for psyOut#1 of both channels of a CPE before the intensity analysis.
pub fn calc_channel_pe(po: &mut PsyOut, quality: u32) -> Result<(), MsError> {
    let stride = po.nsfb_per_group as usize;
    for g in 0..po.ngroups as usize {
        for b in 0..po.max_sfb as usize {
            let idx = g * stride + b;
            let s = po.start[idx] as usize;
            po.pe[idx] = eval_pe(quality, po.thr[idx], &po.spectrum[s..], po.width[idx] as u32)?;
        }
    }
    Ok(())
}
