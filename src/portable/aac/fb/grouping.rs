//! F4b: short-window grouping: the per-channel decision of EncodeFrame
//! 0x1310a0..0x1312fc and the CPE pair re-decision 0x1313e0..0x131bc8 over `_regionBoundaryTab`.
//!
//! The inputs are the per-window per-sfb PAM thresholds produced by the psychoacoustic stage (`thr_chan+0x7ec`: copy of `thr+0x50[w]` times the
//! `SfbParam` weights and the TNS threshold weights; `thr_chan+0x9cc`: copy of `thr+0x40[w]`), 8 windows x 15 floats. They are taken as function
//! arguments here (the PAM is owned by the psy module); the functions are exact given those rows (the encoder fixtures check the decisions end to end).
//!
//! libm: `log10f` (per-channel) and `logf` (pair merge) are the Apple-exact ports of `crate::portable::math`.

use crate::portable::math::{log10f, logf};

/// One window group decision: group lengths in windows (`len(lens)` = number of groups, sum 8).
pub type GroupLens = Vec<u8>;

/// Per-window "low" and "high" mean log10 threshold levels of the per-channel test (`L[w]`, `H[w]`).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct LevelsLH {
    pub l: [f32; 8],
    pub h: [f32; 8],
}

/// `_regionBoundaryTab` (0x9a0c40): entry `k` = all 7-bit masks with popcount `k`, ascending;
/// bit `i` <-> window `7 - i` starts a new group.
pub fn region_masks(k: usize) -> Vec<u8> {
    (0u8..128).filter(|m| m.count_ones() as usize == k).collect()
}

/// Group lengths of a 7-bit boundary mask (`lens_from_mask`).
pub fn lens_from_mask(mask: u8) -> GroupLens {
    let mut f = [1u8; 9];
    for i in 0..7 {
        f[7 - i] = (mask >> i) & 1;
    }
    let mut lens = Vec::new();
    let mut start = 0usize;
    loop {
        let mut n = 0usize;
        loop {
            n += 1;
            if f[n + start] != 0 {
                break;
            }
        }
        lens.push(n as u8);
        start += n;
        if start >= 8 {
            break;
        }
    }
    lens
}

/// Per-channel levels: `L[w] = (10/(u-2)) sum_{k=2}^{u-1} log10f(E[w][k]+100)`, `H[w] = (10/(nsfb-u)) sum_{k=u}^{nsfb-1} ...` with `u = nsfb >> 1`
/// (sequential f32 sums, then one multiply). `nsfb` is the number of short sfbs (14).
pub fn levels_lh(e: &[[f32; 15]; 8], nsfb: usize) -> LevelsLH {
    let u10 = nsfb >> 1;
    let s13 = 10.0f32 / (u10 as i32 - 2) as f32;
    let s14 = 10.0f32 / (nsfb - u10) as f32;
    let mut l = [0.0f32; 8];
    let mut h = [0.0f32; 8];
    for w in 0..8 {
        let mut acc = 0.0f32;
        for k in 2..u10 {
            acc = acc + log10f(e[w][k] + 100.0);
        }
        l[w] = s13 * acc;
        let mut acc = 0.0f32;
        for k in u10..nsfb {
            acc = acc + log10f(e[w][k] + 100.0);
        }
        h[w] = s14 * acc;
    }
    LevelsLH { l, h }
}

/// Group decision from the levels (asm 0x1311c8..0x131288): extend the group while the summed level stays within `[max-3, min+7]` per window of
/// the low and the high half (`(x + c) * n`, strict compares).
pub fn groups_from_lh(lh: &LevelsLH) -> GroupLens {
    let (l, h) = (&lh.l, &lh.h);
    let c7 = 7.0f32;
    let cm3 = -3.0f32;
    let mut flag = [false; 8];
    let mut j = 0usize;
    let mut n = 1u32;
    let (mut lmin, mut lmax, mut suml) = (l[0], l[0], l[0]);
    let (mut hmin, mut hmax, mut sumh) = (h[0], h[0], h[0]);
    loop {
        let fnn = n as f32;
        let keep = suml < (lmin + c7) * fnn && suml > (lmax + cm3) * fnn && sumh < (hmin + c7) * fnn && sumh > (hmax + cm3) * fnn;
        if keep {
            if j > 6 {
                break;
            }
            j += 1;
            lmin = if lmin < l[j] { lmin } else { l[j] };
            lmax = if lmax > l[j] { lmax } else { l[j] };
            hmin = if hmin < h[j] { hmin } else { h[j] };
            hmax = if hmax > h[j] { hmax } else { h[j] };
            suml = suml + l[j];
            sumh = sumh + h[j];
            n += 1;
        } else {
            flag[j] = true;
            lmin = l[j];
            lmax = l[j];
            suml = l[j];
            hmin = h[j];
            hmax = h[j];
            sumh = h[j];
            n = 1;
            if j >= 8 {
                break;
            }
        }
    }
    let mut lens = Vec::new();
    let mut cnt = 0u8;
    for k in 1..8 {
        cnt += 1;
        if flag[k] {
            lens.push(cnt);
            cnt = 0;
        }
    }
    lens.push(cnt + 1);
    lens
}

/// Per-channel short grouping (`E` = `thr_chan+0x7ec` rows). `nsfb < 6` uses the fixed grouping {2,2,3} of the binary (never at >= 8 kHz).
pub fn decide_channel_grouping(e: &[[f32; 15]; 8], nsfb: usize) -> (GroupLens, LevelsLH) {
    if nsfb < 6 {
        return (vec![2, 2, 3], LevelsLH { l: [0.0; 8], h: [0.0; 8] });
    }
    let lh = levels_lh(e, nsfb);
    (groups_from_lh(&lh), lh)
}

/// CPE pair re-decision (`pair_merge`): `e9a/e9b` = `thr_chan+0x9cc` rows of the two channels, `ng_a/ng_b` the group counts of the per-channel
/// decisions. Returns the common group lengths (both channels get the same grouping) and the number of groups of the louder-channel vote `ng_t`.
pub fn pair_merge(e9a: &[[f32; 15]; 8], e9b: &[[f32; 15]; 8], nsfb: usize, ng_a: usize, ng_b: usize) -> (GroupLens, usize) {
    let mut la = [0.0f32; 8];
    let mut lb = [0.0f32; 8];
    let mut sa = 0.0f32;
    let mut sb = 0.0f32;
    for w in 0..8 {
        let (mut a, mut b) = (0.0f32, 0.0f32);
        if nsfb >= 3 {
            for k in 2..nsfb {
                a = a + logf(e9a[w][k] + 100.0);
            }
            for k in 2..nsfb {
                b = b + logf(e9b[w][k] + 100.0);
            }
        }
        la[w] = a;
        lb[w] = b;
        sa = sa + a;
        sb = sb + b;
    }
    let ng_c = if sb > sa { ng_b } else { ng_a };
    let tot = ng_a + ng_b;
    let ng_t = if tot & 1 == 0 { tot >> 1 } else { (ng_c & 1) + tot / 2 };
    if ng_t < 2 {
        return (lens_from_mask(region_masks(0)[0]), ng_t);
    }
    let cands = region_masks(ng_t - 1);
    let mut sum = [0.0f32; 8];
    for w in 0..8 {
        sum[w] = lb[w] + la[w];
    }
    let mut best = 1e24f32;
    let mut bi = 0usize;
    for (ci, &m) in cands.iter().enumerate() {
        let mut f = [1u8; 9];
        for i in 0..7 {
            f[7 - i] = (m >> i) & 1;
        }
        let mut mean = [0.0f32; 8];
        let mut start = 0usize;
        loop {
            let mut s = 0.0f32;
            let mut n = 0usize;
            loop {
                s = s + sum[start + n];
                n += 1;
                if f[start + n] != 0 {
                    break;
                }
            }
            let mv = s / n as f32;
            for x in &mut mean[start..start + n] {
                *x = mv;
            }
            start += n;
            if start >= 8 {
                break;
            }
        }
        let mut acc = 0.0f32;
        for w in 0..8 {
            let d = mean[w] - sum[w];
            acc = acc + d * d;
        }
        if acc < best {
            best = acc;
            bi = ci;
        }
    }
    (lens_from_mask(cands[bi]), ng_t)
}
