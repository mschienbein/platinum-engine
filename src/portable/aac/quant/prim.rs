//! Quantiser primitives: `_quantizeLines` (0004395c, `_quantFLOATtoUInt32_Neon` 00024520), `_calculateScaleSpectrum` (00043b2c),
//! `_InvQuantizeAndEvaluateSpectrum` (00043c70), the scale tables and the `pow43` table of `_CreateLoopKernel`.
//!
//! Operation order and fusion are taken from the machine code (each place is marked `UNFUSED`/`FUSED`).

use crate::portable::aac::quant::fp::{fma, QUANT_OFFSET};
use crate::portable::aac::quant::tables_gen::*;
use std::sync::LazyLock;

/// `LK+0x344`: `powf((float)i, 1.3333334f)`, i = 0..8191 (the table of the original kernel; Apple libm `powf`). The embedded dump is the
/// source of truth (`crate::portable::math::powf` reproduces all 8192 entries). It is the first 8192 entries of the decoder's `PULSEF` table
/// (`bits/data/powf43_8208.f32`): one copy of the data serves both.
pub static POW43: LazyLock<Vec<f32>> = LazyLock::new(|| {
    let b: &[u8] = include_bytes!("../bits/data/powf43_8208.f32");
    b[..8192 * 4].chunks_exact(4).map(|c| f32::from_le_bytes([c[0], c[1], c[2], c[3]])).collect()
});

#[inline(always)]
fn tab_hi(m: usize) -> usize {
    m.min(39)
}

/// `2^(-3*gg/16)` as the f32 product of two table entries (`_quantizeLines`): `gg >= 0` uses the inverse tables.
#[inline]
pub fn quant_scale(gg: i32) -> f32 {
    let k = 3 * gg.unsigned_abs() as usize;
    let (hi, lo) = (tab_hi(k >> 4), k & 15);
    if gg >= 0 { K_INV_POW2_TAB[hi] * K_INV_POW2_TO_00625_TAB[lo] } else { K_POW2_TAB[hi] * K_POW2_TO_00625_TAB[lo] }
}

/// `2^(3*sf/16)` (`_calculateScaleSpectrum`): `sf >= 0` uses the direct tables.
#[inline]
pub fn scale_for_sf(sf: i32) -> f32 {
    let k = 3 * sf.unsigned_abs() as usize;
    let (hi, lo) = (tab_hi(k >> 4), k & 15);
    if sf >= 0 { K_POW2_TAB[hi] * K_POW2_TO_00625_TAB[lo] } else { K_INV_POW2_TAB[hi] * K_INV_POW2_TO_00625_TAB[lo] }
}

/// Dequantiser step `2^(d/4)`, `d = gg - sf` (`_InvQuantizeAndEvaluateSpectrum`).
#[inline]
pub fn step_for(d: i32) -> f32 {
    let m = d.unsigned_abs() as usize;
    let (hi, lo) = (tab_hi(m >> 2), m & 3);
    if d >= 0 { K_POW2_TAB[hi] * K_POW2_025[lo] } else { K_INV_POW2_TAB[hi] * K_INV_POW2_TO_025_TAB[lo] }
}

/// `_quantizeLines(gg, n, x34s, out)`: `q = min(0x1fff, fcvtzu(0.4054f + scale(gg)*x34s))`.
/// n >= 20: software-pipelined loop = **UNFUSED** `fmul` then `fadd`; 4 <= n < 20: `FMLA` = **FUSED**. (n = 1024 in the engine.)
/// NaN / negative products convert to 0 (`fcvtzu`), which is what the 0xff-initialised temp buffer relies on.
pub fn quantize_lines(gg: i32, n: usize, x34s: &[f32], out: &mut [i32]) {
    let s = quant_scale(gg);
    debug_assert!(n >= 4);
    if n >= 20 {
        for i in 0..n {
            let t = s * x34s[i] + QUANT_OFFSET; // UNFUSED (separate roundings; Rust never contracts)
            out[i] = (t as u32).min(0x1fff) as i32;
        }
    } else {
        for i in 0..n {
            let t = fma(s, x34s[i], QUANT_OFFSET); // FUSED
            out[i] = (t as u32).min(0x1fff) as i32;
        }
    }
}

/// `_calculateScaleSpectrum(stride, ng, nsfb, start, width, out, in, sf, mask)`: for every band with `mask != 0`,
/// `out[start..start+width] = in[...] * 2^(3*sf/16)` (plain IEEE multiply); bands with `mask == 0` keep their old `out` values.
#[allow(clippy::too_many_arguments)]
pub fn calculate_scale_spectrum(
    stride: usize,
    ng: usize,
    nsfb: usize,
    start: &[i32],
    width: &[i32],
    out: &mut [f32],
    inp: &[f32],
    sf: &[i32],
    mask: &[i32],
) {
    for g in 0..ng {
        for b in 0..nsfb {
            let i = g * stride + b;
            if mask[i] != 0 {
                let s = scale_for_sf(sf[i]);
                let a = start[i] as usize;
                let w = width[i] as usize;
                for j in a..a + w {
                    out[j] = inp[j] * s;
                }
            }
        }
    }
}

/// `_InvQuantizeAndEvaluateSpectrum(spec, q, pow43, sfbrec, gg, sf, err, nsfb, stride, ng)`:
/// per band `err[i] = ((acc0+acc2)+(acc1+acc3)) * invthr[i]`, 4 lanes `p = step*pow43[q]` (UNFUSED fmul), `d = spec-p` (fsub),
/// `acc = fma(d, d, acc)` (FUSED FMLA). Band widths are multiples of 4. Entries not written (`b >= nsfb`) keep their previous value.
#[allow(clippy::too_many_arguments)]
pub fn inv_quantize_and_evaluate(
    spec: &[f32],
    q: &[i32],
    pow43: &[f32],
    start: &[i32],
    width: &[i32],
    invthr: &[f32],
    gg: i32,
    sf: &[i32],
    nsfb: usize,
    stride: usize,
    ng: usize,
    err: &mut [f32],
) {
    for g in 0..ng {
        for b in 0..nsfb {
            let i = g * stride + b;
            let step = step_for(gg - sf[i]);
            let a = start[i] as usize;
            let w = width[i] as usize;
            let mut acc = [0.0f32; 4];
            let mut j = a;
            while j < a + w {
                for l in 0..4 {
                    let p = step * pow43[q[j + l] as usize]; // UNFUSED
                    let d = spec[j + l] - p;
                    acc[l] = fma(d, d, acc[l]); // FUSED
                }
                j += 4;
            }
            let e = (acc[0] + acc[2]) + (acc[1] + acc[3]);
            err[i] = e * invthr[i];
        }
    }
}
