//! S2: M/S masking thresholds, "Precise" path (`thrmode != 0`, engine): per-band-pair correlation of the L/R per-line powers
//! (long and short flavours), the level difference `ldiff`, `_getMsThresholdsPrecise` (0x44d50), `_addLRThresholdOffsets` (0x44ca0) and
//! `_modifyLRThresholds` (0x44f68).  Asm 0x3a1e0..0x3abf8 of `_aacEncodeFrame`.
//!
//! All arithmetic is f32; `f32::mul_add` marks the fused sites (`fmla` in the correlation, `fmadd` in `corr_map` and the interpolation,
//! `dB()`).  libm only through `crate::portable::math` (Apple-exact `powf`, `expf`).

use crate::portable::aac::psy::{db, pow2_est};
use crate::portable::aac::types::{F_1E_20, K_DB_TO_LOG2};
use crate::portable::aac::ms::tables::{CORR_GRID_BITS, LDIFF_GRID_BITS, LR_OFFSET_IDX_BITS};
use crate::portable::math::{expf, powf};

/// `1e-20` added to every per-line power before the square roots (immediate 0x1e3ce508 at asm 0x3a1bc).
pub const CORR_EPS: f32 = F_1E_20;

/// `(v0 + v3) + (v2 + v1)`: the `ext #12` / `add` / `dup lane 2` / `add` horizontal reduction of the 4-lane accumulators (lane 0 of the result).
#[inline]
pub fn hsum4(v: [f32; 4]) -> f32 {
    (v[0] + v[3]) + (v[2] + v[1])
}

/// One band-pair correlation `c = sum(sqrt(a)*sqrt(b)) / (sqrt(sum a) * sqrt(sum b))` over the lines `[s0, s1)` of the per-line power
/// arrays `pl`/`pr` (long: `p1`; short: one window's `p2`), in steps of 4 lines (`do { .. } while (k < s1)`: at least one step when `s0 < s1`; a
/// width that is not a multiple of 4 reads up to 3 lines past `s1`, as the NEON loop does).  With `a = pl + 1e-20`, `b = pr + 1e-20`:
/// `cross = fma(sqrt(b), sqrt(a), cross)` (fused `fmla`), `suma += a`, `sumb += b`; reduction `hsum4`.
pub fn corr_pair(pl: &[f32], pr: &[f32], s0: usize, s1: usize) -> f32 {
    let mut cross = [0f32; 4];
    let mut sl = [0f32; 4];
    let mut sr = [0f32; 4];
    if s0 < s1 {
        let mut k = s0;
        loop {
            for j in 0..4 {
                let a = pl[k + j] + CORR_EPS;
                let b = pr[k + j] + CORR_EPS;
                cross[j] = b.sqrt().mul_add(a.sqrt(), cross[j]);
                sl[j] += a;
                sr[j] += b;
            }
            k += 4;
            if !(k < s1) {
                break;
            }
        }
    }
    hsum4(cross) / (hsum4(sl).sqrt() * hsum4(sr).sqrt())
}

/// Clamp of the averaged correlation to [-1, 1] as compiled (`if (v <= 1) { max(v, -1) } else { 1 }`, so NaN -> 1).
#[inline]
pub fn clamp_corr(v: f32) -> f32 {
    if v <= 1.0 {
        if v < -1.0 { -1.0 } else { v }
    } else {
        1.0
    }
}

/// The correlation map of the M/S threshold stage: `cc = clamp(c, -1, 1); p = powf(1.05 - cc, -0.4);
/// s = max((p + -0.98067313) * 0.42848924, 0); s = fma(s, 1.33, -0.33); clamp(s, 0, 1)` (NaN of the first step -> 0, of the second -> 1).
pub fn corr_map(c: f32) -> f32 {
    let cc = clamp_corr(c);
    let p = powf(f32::from_bits(0x3f86_6666) - cc, f32::from_bits(0xbecc_cccd));
    let s = (p + f32::from_bits(0xbf7b_0d65)) * f32::from_bits(0x3edb_62f1);
    let s = if 0.0 <= s { s } else { 0.0 };
    let s = s.mul_add(f32::from_bits(0x3faa_3d71), f32::from_bits(0xbea8_f5c3));
    if s <= 1.0 {
        if s < 0.0 { 0.0 } else { s }
    } else {
        1.0
    }
}

/// Level difference of one band: `d = |dB(thrL) - dB(thrR)|`, `e = expf(d * -0.15)`, `r = 1/(e + 1) + -0.5`, `ldiff = r + r`.
pub fn ldiff_val(thr_l: f32, thr_r: f32) -> f32 {
    let d = (db(thr_l) - db(thr_r)).abs();
    let e = expf(d * f32::from_bits(0xbe19_999a));
    let r = 1.0 / (e + 1.0) + -0.5;
    r + r
}

#[inline]
fn grid(bits: &[u32; 5]) -> [f32; 5] {
    [f32::from_bits(bits[0]), f32::from_bits(bits[1]), f32::from_bits(bits[2]), f32::from_bits(bits[3]), f32::from_bits(bits[4])]
}

/// Grid cell search of `_getMsThresholdsPrecise`: returns (cell index, clamped value).  `v > 1` or NaN -> (4, 1); `v < 0` -> (0, 0);
/// otherwise the first `i` with `grid[i] >= v` (at most 4).
#[inline]
fn grid_cell(v: f32, g: &[f32; 5]) -> (usize, f32) {
    if v <= 1.0 {
        if 0.0 <= v {
            let mut i = 0;
            while g[i] < v && i < 4 {
                i += 1;
            }
            (i, v)
        } else {
            (0, 0.0)
        }
    } else {
        (4, 1.0)
    }
}

/// `_getMsThresholdsPrecise(thrL, thrR, corr, ldiff, floor, scale, &thrM, &thrS, tabMid, tabSide)` (0x44d50) -> `(thrM, thrS)`.
/// `tab_*` are the 5x5 adjust tables (`[corr_cell * 5 + ldiff_cell]`, dB / (4 if >= 0 else 9)); `floor` the per-sfb lower bound.
pub fn get_ms_thresholds_precise(thr_l: f32, thr_r: f32, corr: f32, ldiff: f32, floor: f32, scale: f32, tab_mid: &[f32; 25], tab_side: &[f32; 25]) -> (f32, f32) {
    let cg = grid(&CORR_GRID_BITS);
    let lg = grid(&LDIFF_GRID_BITS);
    let (mut ic, corr) = grid_cell(corr, &cg);
    let (mut il, ldiff) = grid_cell(ldiff, &lg);
    let tmin = if thr_l < thr_r { thr_l } else { thr_r };
    if ic < 2 {
        ic = 1;
    }
    if il < 2 {
        il = 1;
    }
    let (c1, c0, l1, l0) = (cg[ic], cg[ic - 1], lg[il], lg[il - 1]);
    let inv = 1.0 / ((c1 - c0) * (l1 - l0));
    let a = c1 - corr;
    let b = corr - c0;
    let cc = l1 - ldiff;
    let d = ldiff - l0;
    let one = |tab: &[f32; 25]| -> f32 {
        let v00 = tab[(ic - 1) * 5 + (il - 1)];
        let v01 = tab[(ic - 1) * 5 + il];
        let v10 = tab[ic * 5 + il - 1];
        let v11 = tab[ic * 5 + il];
        let s16 = a.mul_add(v00, b * v10);
        let s17 = d * a.mul_add(v01, b * v11);
        let s16 = cc.mul_add(s16, s17);
        let r = inv * s16;
        let k = if r < 0.0 { 9.0f32 } else { 4.0f32 };
        let e = (scale * (r * k)) * K_DB_TO_LOG2;
        let t = tmin * pow2_est(e);
        if floor > t { floor } else { t }
    };
    (one(tab_mid), one(tab_side))
}

/// Exponent argument of `_addLRThresholdOffsets`: every polynomial coefficient is the literal 0.0, so it is +-0 (NaN only for a non-finite
/// `freq`), and `pow2_est(+-0) = 1.0`: the call is the identity on `thr_l`/`thr_r`.  Kept for fidelity (the M/S block calls it per band).
pub fn add_lr_threshold_offsets(freq: f32, thr_l: &mut f32, thr_r: &mut f32) {
    let idx = [f32::from_bits(LR_OFFSET_IDX_BITS[0]), f32::from_bits(LR_OFFSET_IDX_BITS[1]), f32::from_bits(LR_OFFSET_IDX_BITS[2])];
    // do { u += 1; } while (idx[u] < freq && u < 2)   (u starts at -1)
    let mut u: i32 = -1;
    let mut p = 0usize;
    loop {
        u += 1;
        let cur = idx[p];
        p += 1;
        if !(cur < freq && u < 2) {
            break;
        }
    }
    let x = if u == 0 {
        0.0f32
    } else {
        let lo = idx[(u - 1) as usize];
        let hi = idx[p - 1];
        (((freq - lo) * 0.0) / (hi - lo) + 0.0) * K_DB_TO_LOG2
    };
    let f = pow2_est(x);
    *thr_l *= f;
    *thr_r *= f;
}
