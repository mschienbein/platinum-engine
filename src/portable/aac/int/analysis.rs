//! Phase A4 of `_aacEncodeFrame` (0x39244..0x3a138): intensity analysis of one CPE, run BEFORE the M/S block on the original L/R psyOut.
//! Order of float operations = Apple's (checked against the decompile of `_aacEncodeFrame` and by the encoder fixtures); fused ops are `mul_add`.
use crate::portable::aac::int::config::NB;
use crate::portable::aac::int::input::{ChannelLines, TnsRanges};
use crate::portable::aac::psy::db;
use crate::portable::aac::psy::vdsp::sve;
use crate::portable::aac::int::state::IntensityChannel;
use crate::portable::aac::int::tables::{k, INV_QUANT, IS_POSITION};
use crate::portable::aac::types::PsyOut;

/// Position quantiser (asm 0x39ab8..0x39bb4): energy ratio `(EL+1e-3)/(ER+1e-3)` -> (`is_position` in [-30,30], amplitude ratio `g = R/L`).
/// `pos = 0` for ratio in [2^(-1/4), 2^(1/4)]; `pos > 0` (L louder): `1 + #{k>=1: IS_POSITION[k] < ratio}` (saturates at 30), `g = 1/2^(pos/4)`;
/// `pos < 0`: from `1/ratio`, `g = 2^(-pos/4)`.
pub fn quant_pos(ratio: f32) -> (i32, f32) {
    if ratio <= k::POS_UP {
        if ratio >= k::POS_DOWN {
            return (0, 1.0);
        }
        let inv = 1.0f32 / ratio;
        let p: i32;
        if inv <= k::POS_MAX {
            let mut kk = 1usize;
            let mut cnt = 0i32;
            loop {
                let cur = kk;
                kk += 1;
                cnt += 1;
                if !(cur < 30 && inv > IS_POSITION[cur]) {
                    break;
                }
            }
            p = -cnt;
        } else {
            p = -30;
        }
        return (p, INV_QUANT[(-p) as usize]);
    }
    let p: i32;
    if ratio <= k::POS_MAX {
        let mut kk = 1usize;
        let mut cnt = 0i32;
        loop {
            let cur = kk;
            kk += 1;
            cnt += 1;
            if !(cur < 30 && ratio > IS_POSITION[cur]) {
                break;
            }
        }
        p = cnt;
    } else {
        p = 30;
    }
    (p, 1.0f32 / INV_QUANT[p as usize])
}

/// The TNS ranges of a channel as `analyze` wants them: `None` when the psyOut has no TNS struct (`psyOut+0xbc0 == 0`).
pub fn tns_ranges(p: &PsyOut) -> Option<&TnsRanges> {
    p.tns.as_ref().map(|t| &t.ranges)
}

/// TNS mask of the frame (asm 0x39378..0x39600): window-indexed mask (`bit0` ch0, `bit1` ch1 filter coverage per sfb), then compacted to group
/// indices (`idx = g*nspw + s`). Only built when ch0's TNS struct exists.
pub fn build_tns_mask(c0: &PsyOut, tns0: Option<&TnsRanges>, tns1: Option<&TnsRanges>, out: &mut [u8; NB]) {
    *out = [0; NB];
    let nspw = c0.nsfb_per_group as usize;
    let ng = c0.ngroups as usize;
    if let Some(t0) = tns0 {
        let mut w0 = 0usize;
        for g in 0..ng {
            let n = c0.group_len[g].max(0) as usize;
            for (t, bit) in [(Some(t0), 1u8), (tns1, 2u8)] {
                let t = match t {
                    Some(t) if t.present => t,
                    _ => continue,
                };
                for wi in 0..n {
                    let w = w0 + wi;
                    if w >= 8 {
                        continue;
                    }
                    let cnt = t.count[w].clamp(0, 3) as usize;
                    for f in 0..cnt {
                        let (a, b) = t.range[w][f];
                        let mut s = a;
                        while s < b {
                            let idx = s as isize + (w * nspw) as isize;
                            if idx >= 0 && (idx as usize) < NB {
                                out[idx as usize] |= bit;
                            }
                            s += 1;
                        }
                    }
                }
            }
            w0 += n;
        }
        let mut w0 = 0usize;
        for g in 0..ng {
            let n = c0.group_len[g].max(0) as usize;
            for s in 0..nspw {
                let mut acc = out[s + nspw * w0];
                for kk in 1..n {
                    let raw = out[s + nspw * (w0 + kk)];
                    let m = if raw == 1 || raw == 2 { raw } else { 3 };
                    if acc == 0 {
                        acc = raw;
                    } else if acc == 3 {
                        acc = m;
                    }
                }
                out[s + nspw * g] = acc;
            }
            w0 += n;
        }
    }
}

impl IntensityChannel {
    /// Gate shared by analysis and decision (asm 0x39234 / 0x3b1c8): element tool enabled and both psyOuts `+0x5b8` (pair path ran).
    pub fn applies(&self, c0: &PsyOut, c1: &PsyOut) -> bool {
        self.cfg.enabled && c0.is_pair && c1.is_pair
    }

    /// Phase A4: intensity analysis (before M/S). Reads the L/R psyOut, writes only the `IntensityState` (downmix, thresholds, positions...).
    /// Returns `false` (and leaves the state untouched) when the gate fails.
    pub fn analyze(&mut self, c0: &PsyOut, c1: &PsyOut, l0: &ChannelLines, l1: &ChannelLines, tns0: Option<&TnsRanges>, tns1: Option<&TnsRanges>) -> bool {
        if !self.applies(c0, c1) {
            return false;
        }
        let cfg = &self.cfg;
        let st = &mut self.st;
        let bt = c0.wseq;
        let short = bt == 2;
        let nsfb = c0.max_sfb as usize;
        let nspw = c0.nsfb_per_group as usize;
        let ng = c0.ngroups as usize;
        let quiet = if short { &cfg.quiet_short } else { &cfg.quiet_long };
        let aliased = l0.pair_im.is_none();

        if !short {
            for i in 0..nsfb.min(52) {
                st.dm_energy_prev[i] = st.dm_energy[i];
            }
        }
        std::mem::swap(&mut st.tns_cur, &mut st.tns_prev);
        build_tns_mask(c0, tns0, tns1, &mut st.tns_cur);

        // long -> short hand-over of the smoothing state (max over runs of equal map[])
        if short && st.prev_bt != 2 && cfg.n_long > 0 {
            let mut i = 0usize;
            while i < cfg.n_long {
                let u = cfg.map[i];
                let mut m = k::C_NEG_1E30;
                let mut j = i;
                loop {
                    if !(st.st23[j] <= m) {
                        m = st.st23[j];
                    }
                    j += 1;
                    if !(j < 52 && cfg.map[j] == u) {
                        break;
                    }
                }
                st.st24[u as usize] = m;
                i = j;
            }
        }

        st.dm = [0.0; 1024];
        let mut tmp = [0.0f32; 1024];
        let mut gstart: i32 = 0;
        let mut win0 = 0usize;
        for g in 0..ng {
            let n = c0.group_len[g].max(0) as usize;
            if n == 0 {
                continue;
            }
            for s in 0..nsfb {
                let idx = s + nspw * g;
                let off = c0.start[idx] as usize;
                let w = c0.width[idx] as usize;
                let wpw = w / n;
                let woff = (((c0.start[idx] - gstart) as i64) / (n as i64)) as usize;
                let mut s8 = 0.0f32;
                let mut s9 = 0.0f32;
                let mut s11 = 0.0f32;
                let mut s10 = 0.0f32;
                // slices of the per-window arrays
                let sl = |wix: usize| -> (usize, usize) {
                    if short { (wix * 128 + woff, wix * 128 + woff + wpw) } else { (woff, woff + wpw) }
                };
                for wi in 0..n {
                    let wix = win0 + wi;
                    let (lo, hi) = sl(wix);
                    let (e0, e1) = if short { (&l0.winen[lo..hi], &l1.winen[lo..hi]) } else { (&l0.en[lo..hi], &l1.en[lo..hi]) };
                    let el = sve(e0);
                    let er = sve(e1);
                    let a = &l0.pair_re[lo..hi];
                    let b = &l1.pair_re[lo..hi];
                    for i in 0..wpw {
                        tmp[i] = a[i] + b[i];
                    }
                    for i in 0..wpw {
                        tmp[i] = tmp[i] * tmp[i];
                    }
                    s11 += sve(&tmp[..wpw]);
                    for i in 0..wpw {
                        tmp[i] = b[i] - a[i];
                    }
                    for i in 0..wpw {
                        tmp[i] = tmp[i] * tmp[i];
                    }
                    s10 += sve(&tmp[..wpw]);
                    if let (Some(ai), Some(bi)) = (&l0.pair_im, &l1.pair_im) {
                        // never exercised by the encoder (pair arrays are always aliased); decompile L671-682: the same two accumulations on the im arrays
                        for i in 0..wpw {
                            tmp[i] = ai[lo + i] + bi[lo + i];
                        }
                        for i in 0..wpw {
                            tmp[i] = tmp[i] * tmp[i];
                        }
                        s11 += sve(&tmp[..wpw]);
                        for i in 0..wpw {
                            tmp[i] = bi[lo + i] - ai[lo + i];
                        }
                        for i in 0..wpw {
                            tmp[i] = tmp[i] * tmp[i];
                        }
                        s10 += sve(&tmp[..wpw]);
                    }
                    s8 += el;
                    s9 += er;
                }
                let use_sum;
                if s10 <= s11 {
                    use_sum = true;
                } else {
                    let s0 = s10 * k::T_0_1;
                    use_sum = s0 <= s11;
                    if !use_sum {
                        s11 = 0.0;
                        for wi in 0..n {
                            let wix = win0 + wi;
                            let (lo, hi) = sl(wix);
                            let a = &l0.pair_re[lo..hi];
                            let b = &l1.pair_re[lo..hi];
                            for i in 0..wpw {
                                tmp[i] = a[i].mul_add(2.0, b[i]);
                            }
                            for i in 0..wpw {
                                tmp[i] = tmp[i] * tmp[i];
                            }
                            s11 += sve(&tmp[..wpw]);
                            if let (Some(ai), Some(bi)) = (&l0.pair_im, &l1.pair_im) {
                                for i in 0..wpw {
                                    tmp[i] = ai[lo + i].mul_add(2.0, bi[lo + i]);
                                }
                                for i in 0..wpw {
                                    tmp[i] = tmp[i] * tmp[i];
                                }
                                s11 += sve(&tmp[..wpw]);
                            }
                        }
                    }
                }
                let ed = if aliased { s11 + s11 } else { s11 };
                let ratio = (s8 + k::C_0_001) / (s9 + k::C_0_001);
                let (pos, gm) = quant_pos(ratio);
                let t = (ed + k::FLT_MIN) * (gm * gm + 1.0);
                let scale = ((s9 + s8) / t).sqrt();
                let ls = &c0.spectrum[off..off + w];
                let rs = &c1.spectrum[off..off + w];
                for i in 0..w {
                    let raw = if use_sum { ls[i] + rs[i] } else { ls[i] * 2.0 + rs[i] };
                    st.dm[off + i] = raw * scale;
                }
                let dm = &st.dm[off..off + w];
                for i in 0..w {
                    tmp[i] = dm[i] * dm[i];
                }
                let e = sve(&tmp[..w]) / (n as f32);
                let thr_l = c0.thr[idx];
                let thr_r = c1.thr[idx];
                let s1 = thr_l / (c0.energy[idx] + 1.0);
                let s2 = thr_r / (c1.energy[idx] + 1.0);
                let m = if s1 < s2 { s1 } else { s2 };
                let mut thr = (e * m) * k::C_1_4;
                let q = quiet[s];
                if !(thr >= q) {
                    thr = q;
                }
                for i in 0..w {
                    let d = dm[i] - ls[i];
                    tmp[i] = d * d;
                }
                let e_ld = sve(&tmp[..w]);
                for i in 0..w {
                    let r = dm[i] * gm;
                    let d = r - rs[i];
                    tmp[i] = d * d;
                }
                let e_rd = sve(&tmp[..w]);
                let a_ = e_ld / thr_l;
                let b_ = e_rd / thr_r + k::C_1E_4;
                let mm = if a_ > b_ { a_ } else { b_ };
                let db = db(mm) + 3.0;
                let v2;
                if short || s > 7 {
                    let v = if db < 0.0 { 0.0 } else { db };
                    v2 = v * (v * k::C_0_009);
                } else {
                    let mut v = db * k::C_1_3;
                    if v < 0.0 {
                        v = 0.0;
                    }
                    v2 = v * (v * k::C_0_009);
                }
                let m5 = if v2 > 5.0 { 5.0 } else { v2 };
                if short {
                    let t_ = st.st24[s].mul_add(k::C_0_9, m5 * k::C_0_10000002);
                    let v3 = if t_ < v2 { v2 } else { t_ };
                    st.st24[s] = v3;
                    st.x2[idx] = v3;
                } else {
                    let t_ = st.st23[idx].mul_add(k::C_0_9, m5 * k::C_0_10000002);
                    let v3 = if t_ < v2 { v2 } else { t_ };
                    st.st23[idx] = v3;
                }
                st.pos[idx] = pos;
                st.g[idx] = gm;
                st.scale[idx] = scale;
                st.dm_energy[idx] = e;
                st.thr[idx] = thr;
                st.v2[idx] = v2;
                st.phase[idx] = 0;
            }
            if !short {
                // equalise the long smoothing state over runs of equal map[] and expose it as X2
                let mut i = 0usize;
                while i < cfg.n_long {
                    let u = cfg.map[i];
                    let mut m = k::C_NEG_1E30;
                    let mut j = i;
                    loop {
                        if !(st.st23[j] <= m) {
                            m = st.st23[j];
                        }
                        j += 1;
                        if !(j < 52 && cfg.map[j] == u) {
                            break;
                        }
                    }
                    for kk in i..j {
                        st.st23[kk] = m;
                    }
                    i = j;
                }
                for i in 0..nsfb.min(52) {
                    st.x2[i] = st.st23[i];
                }
            }
            let last = nspw * (g + 1) - 1;
            gstart = c0.start[last] + c0.width[last];
            win0 += n;
        }
        true
    }
}
