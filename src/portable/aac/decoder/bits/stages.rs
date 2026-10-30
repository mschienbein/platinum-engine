//! Spectral post-processing stages applied to the dequantised spectrum, in Apple's order: M/S -> PNS -> intensity -> TNS
//! (`JointStereo::ApplyMidSide` 0x2d5934, `IndividualChannelStream::ApplyPNS` 0x1f940, `JointStereo::ApplyIntensity` 0x2d5fac,
//! `TNSData::Apply` 0x1fc60).  FMA sites (exactly three, all `f32::mul_add`): PNS energy, TNS step-up, TNS AR filter.

use crate::portable::arm::{frsqrte_f32 as frsqrte, frsqrts_f32 as frsqrts};
use crate::portable::aac::decoder::bits::ics::ChannelWork;
use crate::portable::aac::decoder::bits::tables::{NORM_FACTOR, PNS_ADD, PNS_MUL, SIN_COEFS, TNS_MAX_BANDS_LONG, TNS_MAX_BANDS_SHORT};

/// ARM default NaN.  Every NaN of this stage is born from an invalid operation (`inf - inf`, `0 * inf`) on finite / infinite inputs, which
/// yields `0x7fc00000` on ARM (and propagates unchanged) but `0xffc00000` on x86; mapping each NaN produced by a stage operation to the ARM
/// default NaN makes the bits identical on every host (non-finite spectra only occur for adversarial gain / TNS combinations: the largest
/// finite spectrum value is about 2^106 (noise gain) and a resonant order-12 TNS filter can amplify it past f32::MAX).
#[inline(always)]
fn arm_nan(x: f32) -> f32 {
    if x.is_nan() { f32::from_bits(0x7fc0_0000) } else { x }
}

// ------------------------------------------------------------------------------------------------------------------ M/S

/// `ApplyMidSide`: `(l, r) <- (l + r, l - r)` (no 0.5) for every band with `ms_used[g][b]` and every window of the group (also for
/// noise / intensity bands, which are still zero at this point).  `mask[g]` bit `b` = `ms_used[g][b]`.
pub(crate) fn apply_ms(info: &crate::portable::aac::decoder::bits::ics::IcsInfo, present: u8, mask: &[u64; 8], l: &mut [f32; 1024], r: &mut [f32; 1024]) {
    if present == 0 || info.max_sfb == 0 {
        return;
    }
    let off = info.offsets();
    let wl = off[off.len() - 1] as usize;
    let mut wbase = 0usize;
    for g in 0..info.num_groups as usize {
        for b in 0..info.max_sfb as usize {
            if mask[g] >> b & 1 != 0 {
                for j in 0..info.glen[g] as usize {
                    let s = (wbase + j) * wl + off[b] as usize;
                    let e = (wbase + j) * wl + off[b + 1] as usize;
                    for i in s..e {
                        let a = l[i];
                        let c = r[i];
                        l[i] = a + c;
                        r[i] = a - c;
                    }
                }
            }
        }
        wbase += info.glen[g] as usize;
    }
}

// ------------------------------------------------------------------------------------------------------------------ PNS

/// One band of noise (`ApplyPNS` inner loop, asm 0x1fab8-0x1fb98): `width` lines (multiple of 4, >= 4), `out.len() == width`.
fn pns_band(state: &mut [u32; 4], width: usize, norm: f32, gain: f32, out: &mut [f32]) {
    let nv = (width >> 2).max(1);
    let mut acc = [0f32; 4];
    for i in 0..nv {
        for l in 0..4 {
            let v = (state[l] as i32 as f32) * norm; // scvtf, fmul
            acc[l] = v.mul_add(v, acc[l]); // fmla (fused)
            out[4 * i + l] = v;
            state[l] = state[l].wrapping_mul(PNS_MUL).wrapping_add(PNS_ADD);
        }
    }
    // horizontal sum (ext / fadd / dup / fadd): (l0 + l2) + (l1 + l3)
    let e = (acc[0] + acc[2]) + (acc[1] + acc[3]);
    // frsqrte + two Newton steps (frsqrts), clamped to 2^126
    let mut r = frsqrte(e);
    r *= frsqrts(e, r * r);
    r *= frsqrts(e, r * r);
    let lim = 8.507059e37f32;
    let g = if r > lim { lim } else { r };
    let sc = g * gain;
    for o in out.iter_mut().take(nv * 4) {
        *o = arm_nan(*o * sc);
    }
}

/// `ApplyPNS`: group -> section (codebook 13) -> window of the group -> band of the section.  `rng` is the channel's persistent 4-lane
/// LCG (never reset).
pub(crate) fn apply_pns(w: &mut ChannelWork, rng: &mut [u32; 4]) {
    if !w.sec.has_noise || w.info.max_sfb == 0 {
        return;
    }
    let off = w.info.offsets();
    let wl = off[off.len() - 1] as usize;
    let mut wbase = 0usize;
    for g in 0..w.info.num_groups as usize {
        let gl = w.info.glen[g] as usize;
        let mut sfb = 0usize;
        for &(cb, ln) in w.sec.group(g) {
            if cb == 13 {
                for j in 0..gl {
                    for b in sfb..sfb + ln as usize {
                        let width = (off[b + 1] - off[b]) as usize;
                        let base = (wbase + j) * wl + off[b] as usize;
                        if width >= 4 {
                            let n = (width >> 2) * 4;
                            pns_band(rng, width, NORM_FACTOR[(width >> 2) - 1], w.gains[g][b], &mut w.spec[base..base + n]);
                        }
                    }
                }
            }
            sfb += ln as usize;
        }
        wbase += gl;
    }
}

// ------------------------------------------------------------------------------------------------------------------ intensity

/// `ApplyIntensity` (common-window CPE): for every band of the RIGHT channel with codebook 14/15: `R = L * (+-gain_R)`; the sign is
/// positive iff `(cb == 14) == ms` where `ms = (ms_mask_present == 1) && ms_used[g][b]`.
pub(crate) fn apply_intensity(info: &crate::portable::aac::decoder::bits::ics::IcsInfo, present: u8, mask: &[u64; 8], l: &[f32; 1024], r: &mut ChannelWork) {
    if !r.sec.has_intensity || info.max_sfb == 0 {
        return;
    }
    let off = info.offsets();
    let wl = off[off.len() - 1] as usize;
    let mut wbase = 0usize;
    for g in 0..info.num_groups as usize {
        let mut sfb = 0usize;
        for &(cb, ln) in r.sec.group(g) {
            for b in sfb..sfb + ln as usize {
                if cb >= 14 {
                    let gain = r.gains[g][b];
                    let ms = present == 1 && (mask[g] >> b & 1 != 0);
                    let f = if (cb == 14) == ms { gain } else { -gain };
                    for j in 0..info.glen[g] as usize {
                        let s = (wbase + j) * wl + off[b] as usize;
                        let e = (wbase + j) * wl + off[b + 1] as usize;
                        for i in s..e {
                            r.spec[i] = l[i] * f;
                        }
                    }
                }
            }
            sfb += ln as usize;
        }
        wbase += info.glen[g] as usize;
    }
}

// ------------------------------------------------------------------------------------------------------------------ TNS

/// LPC from reflection coefficients (step-up, fused): `a[0] = 1; new[i] = fma(a[m - i], k_m, a[i]); new[m] = k_m`.
fn tns_lpc(refl: &[f32], order: usize, a: &mut [f32; 13]) {
    let mut cur = [0f32; 13];
    cur[0] = 1.0;
    for m in 1..=order {
        let k = refl[m - 1];
        let mut nw = [0f32; 13];
        nw[0] = cur[0];
        for i in 1..m {
            nw[i] = cur[m - i].mul_add(k, cur[i]); // fmadd
        }
        nw[m] = k;
        cur = nw;
    }
    *a = cur;
}

/// AR filter over one region (`ARFilter` 0x20a50): `y = x - st[0]; x = y; st[j] = fma(a[j+1], y, st[j+1]); st[order-1] = y * a[order]`.
fn tns_ar(data: &mut [f32], descending: bool, a: &[f32; 13], order: usize) {
    let mut st = [0f32; 13];
    let mut step = |x: &mut f32| {
        let y = arm_nan(*x - st[0]);
        *x = y;
        for j in 0..order - 1 {
            st[j] = a[j + 1].mul_add(y, st[j + 1]); // fmadd
        }
        st[order - 1] = y * a[order];
    };
    if descending {
        for x in data.iter_mut().rev() {
            step(x);
        }
    } else {
        for x in data.iter_mut() {
            step(x);
        }
    }
}

/// `TNSData::Apply`: per window, per filter in stream order; `top` starts at `num_swb`, each filter takes `bot = max(0, top - length)`.
pub(crate) fn apply_tns(w: &mut ChannelWork) {
    if !w.tns.present {
        return;
    }
    let info = &w.info;
    let short = info.is_short();
    let off = info.offsets();
    let wl = off[off.len() - 1] as usize;
    let nswb = (off.len() - 1) as i32;
    let max_bands = if short { TNS_MAX_BANDS_SHORT[info.sfi as usize] } else { TNS_MAX_BANDS_LONG[info.sfi as usize] };
    let lim = i32::from(max_bands.min(info.max_sfb));
    let nwin = if short { 8 } else { 1 };
    for win in 0..nwin {
        let nf = if short { (w.tns.mask >> (7 - win)) & 1 } else { w.tns.mask };
        let mut top = nswb;
        for i in 0..nf as usize {
            let f = &w.tns.filters[if short { win } else { i }];
            let bot = (top - i32::from(f.length)).max(0);
            if f.order != 0 {
                let b0 = bot.min(lim) as usize;
                let b1 = top.min(lim) as usize;
                let (s, e) = (off[b0] as usize, off[b1] as usize);
                if s < e {
                    let order = f.order as usize;
                    let mut refl = [0f32; 12];
                    for j in 0..order {
                        refl[j] = SIN_COEFS[f.coefs[j] as usize];
                    }
                    let mut a = [0f32; 13];
                    tns_lpc(&refl, order, &mut a);
                    let base = win * wl;
                    tns_ar(&mut w.spec[base + s..base + e], f.dir != 0, &a, order);
                }
            }
            top = bot;
        }
    }
}

#[cfg(all(test, target_arch = "aarch64"))]
mod neon_tests {
    //! The PNS kernel against the hardware NEON instructions it models (same sequence as the model's C kernel `pns_band`).
    use super::*;
    use std::arch::aarch64::*;

    #[allow(unsafe_code)]
    fn neon_pns(state: &mut [u32; 4], width: usize, norm: f32, gain: f32, out: &mut [f32]) {
        unsafe {
            let nv = (width >> 2).max(1);
            let mut acc = vdupq_n_f32(0.0);
            let mulc = vdupq_n_u32(PNS_MUL);
            let addc = vdupq_n_u32(PNS_ADD);
            let mut st = vld1q_u32(state.as_ptr());
            for i in 0..nv {
                let mut v = vcvtq_f32_s32(vreinterpretq_s32_u32(st));
                v = vmulq_n_f32(v, norm);
                acc = vfmaq_f32(acc, v, v);
                vst1q_f32(out.as_mut_ptr().add(4 * i), v);
                st = vmlaq_u32(addc, st, mulc);
            }
            vst1q_u32(state.as_mut_ptr(), st);
            let ex = vextq_f32::<2>(acc, acc);
            let s = vadd_f32(vget_low_f32(acc), vget_low_f32(ex));
            let d = vdup_lane_f32::<1>(s);
            let e2 = vadd_f32(s, d);
            let a = vset_lane_f32::<0>(vget_lane_f32::<0>(e2), vdup_n_f32(0.0));
            let x1 = vrsqrte_f32(a);
            let t1 = vrsqrts_f32(a, vmul_f32(x1, x1));
            let x1 = vmul_f32(x1, t1);
            let t2 = vrsqrts_f32(a, vmul_f32(x1, x1));
            let r = vmul_f32(x1, t2);
            let rr = vget_lane_f32::<0>(r);
            let lim = 8.507059e37f32;
            let g = if rr > lim { lim } else { rr };
            let sc = g * gain;
            for o in out.iter_mut().take(nv * 4) {
                *o *= sc;
            }
        }
    }

    #[test]
    fn pns_band_matches_neon() {
        let mut s = 0x9e37_79b9_7f4a_7c15u64;
        let mut next = || {
            s ^= s << 13;
            s ^= s >> 7;
            s ^= s << 17;
            s
        };
        let mut checked = 0;
        for iter in 0..200_000 {
            let r = next();
            let mut width = 4 * (1 + (r % 32) as usize);
            let mut st = [next() as u32, next() as u32, next() as u32, next() as u32];
            if iter % 1000 == 0 {
                st = [0; 4]; // all-zero energy: frsqrte(0) = inf, frsqrts(0, inf) = 1.5, clamp to 2^126
                width = 4;
            }
            let gain = f32::from_bits(((100 + (next() % 60) as u32) << 23) | (next() as u32 & 0x7f_ffff));
            let norm = NORM_FACTOR[width / 4 - 1];
            let (mut st_a, mut st_b) = (st, st);
            let mut out_a = vec![0f32; width];
            let mut out_b = vec![0f32; width];
            pns_band(&mut st_a, width, norm, gain, &mut out_a);
            neon_pns(&mut st_b, width, norm, gain, &mut out_b);
            assert_eq!(st_a, st_b);
            for i in 0..width {
                assert_eq!(out_a[i].to_bits(), out_b[i].to_bits(), "iter {iter} width {width} line {i}");
            }
            checked += 1;
        }
        assert_eq!(checked, 200_000);
    }
}
