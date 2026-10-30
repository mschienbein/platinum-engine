//! Phase A7 of `_aacEncodeFrame` (0x3b1b4..0x3bfdc): intensity decision (ILD estimate, stationarity, correlation, penalty terms, PE of the
//! downmix, two-state trellis) and application to the post-M/S psyOut; includes `_estimateIldDistortionShort` (0x2f4914) and `_getCorrelationLong` (0x2f46c4).
use crate::portable::aac::int::config::NB;
use crate::portable::aac::int::input::ChannelLines;
use crate::portable::aac::int::state::{IntensityChannel, INT_MIN};
use crate::portable::aac::int::tables::k;
use crate::portable::aac::ms::eval_percep_entropy_by_line;
use crate::portable::aac::psy::vdsp::sve;
use crate::portable::aac::psy::{db, pow2_est};
use crate::portable::aac::types::{IntensityData, PsyOut};

impl IntensityChannel {
    /// `_estimateIldDistortionShort(ch0, ch1.en, IC)` (0x2f4914): per long sfb energies of the `en` arrays (also updates the "previous frame"
    /// energies), ILD (dB) of the long sfb and of the short bands (lines `tabShort*8` of the same arrays), and for bt in {1,2} the per-short-band
    /// ILD distortion `IC.ild[k] = sqrt(max |ild_short[k]-ild_long[s]| over non-transient s) * 1.5`; zero otherwise.
    pub fn estimate_ild_distortion_short(&mut self, bt: u32, en0: &[f32], en1: &[f32]) {
        let cfg = &self.cfg;
        let st = &mut self.st;
        let nlong = cfg.n_long;
        let nshort = cfg.n_short;
        let mut ildlong = [0.0f32; 64];
        let mut trans = [false; 64];
        let mut start = 0usize;
        for s in 0..nlong {
            let end = cfg.tab_long[s + 1] as usize;
            let el = sve(&en0[start..end]);
            let er = sve(&en1[start..end]);
            let mut t = true;
            if el <= st.prev_en_l[s] + st.prev_en_l[s] {
                t = (st.prev_en_r[s] + st.prev_en_r[s]) < er;
            }
            trans[s] = t;
            st.prev_en_l[s] = el;
            st.prev_en_r[s] = er;
            ildlong[s] = db((el + k::C_0_001) / (er + k::C_0_001));
            start = end;
        }
        let mut ildshort = [0.0f32; 32];
        start = 0;
        for kk in 0..nshort {
            let end = (cfg.tab_short[kk + 1] as usize) * 8;
            let el = sve(&en0[start..end]);
            let er = sve(&en1[start..end]);
            ildshort[kk] = db((el + k::C_0_001) / (er + k::C_0_001));
            start = end;
        }
        if bt == 1 || bt == 2 {
            let mut s = 0usize;
            while s < nlong {
                let kk = cfg.map[s] as usize;
                let mut m = 0.0f32;
                loop {
                    let d = (ildshort[kk] - ildlong[s]).abs();
                    if m < d && !trans[s] {
                        m = d;
                    }
                    s += 1;
                    if !(s < 52 && cfg.map[s] as usize == kk) {
                        break;
                    }
                }
                st.ild[kk] = m.sqrt() * 1.5;
            }
        } else {
            for kk in 0..nshort {
                st.ild[kk] = 0.0;
            }
        }
    }

    /// `_getCorrelationLong(ch0, ch1, IC)` (0x2f46c4): cosine similarity of the L/R `en` arrays over sfb pairs -> `IC.f` (smoothed, in place).
    pub fn get_correlation_long(&mut self, en0: &[f32], en1: &[f32], aliased: bool) {
        let cfg = &self.cfg;
        let st = &mut self.st;
        let nlong = cfg.n_long;
        if nlong < 2 {
            return;
        }
        let eps = k::C_1E_20;
        let mut s = 0usize;
        loop {
            let a = cfg.tab_long[s] as usize;
            let b = cfg.tab_long[s + 2] as usize;
            let mut acc = [0.0f32; 4];
            let mut s0 = [0.0f32; 4];
            let mut s1 = [0.0f32; 4];
            let mut i = a;
            while i < b {
                for l in 0..4 {
                    let v3 = en0[i + l] + eps;
                    let v4 = en1[i + l] + eps;
                    let v5 = v3.sqrt();
                    let v6 = v4.sqrt();
                    acc[l] = v6.mul_add(v5, acc[l]);
                    s0[l] += v3;
                    s1[l] += v4;
                }
                i += 4;
            }
            let num = (acc[0] + acc[3]) + (acc[2] + acc[1]);
            let d0 = (s0[0] + s0[3]) + (s0[2] + s0[1]);
            let d1 = (s1[0] + s1[3]) + (s1[2] + s1[1]);
            let den = d0.sqrt() * d1.sqrt();
            let c = num / den;
            let t = if c < 0.0 { 0.0 } else { c };
            let c1 = if aliased { t.sqrt() } else { c };
            let c2 = if c1 > 1.0 {
                1.0
            } else if c1 >= -1.0 {
                c1
            } else {
                -1.0
            };
            let p = crate::portable::math::powf(k::C_1_05 - c2, k::C_NEG_0_4);
            let mut v = (p + k::C_NEG_0_98067) * k::C_0_42849;
            if v < 0.0 {
                v = 0.0;
            }
            if !((s as i32) > st.prev_count || st.tns_cur[s] != st.tns_prev[s] || st.tns_cur[s | 1] != st.tns_prev[s | 1]) {
                v = st.f[s].mul_add(k::C_0_9, v * k::C_0_10000002);
            }
            st.f[s] = v;
            st.f[s | 1] = v;
            s += 2;
            if !(s < nlong - 1) {
                break;
            }
        }
        st.prev_count = nlong as i32;
    }

    /// Stationarity term (asm 0x3b3d4..0x3b588; long-type blocks only): std-dev over the 8 short windows of the ILD (dB) per short band.
    fn stationarity(&mut self, w0: &[f32], w1: &[f32], aliased: bool) {
        let cfg = &self.cfg;
        let st = &mut self.st;
        let mut start = 0usize;
        for kk in 0..cfg.n_short {
            let end = cfg.tab_short[kk + 1] as usize;
            let mut dbs = [0.0f32; 8];
            for w in 0..8 {
                let el = sve(&w0[w * 128 + start..w * 128 + end]);
                let er = sve(&w1[w * 128 + start..w * 128 + end]);
                dbs[w] = db((el + 1.0) / (er + 1.0));
            }
            let mut s10 = 0.0f32;
            for x in dbs {
                s10 += x;
            }
            let mean = s10 * 0.125;
            let mut var = 0.0f32;
            for x in dbs {
                let dd = x - mean;
                var += dd * dd;
            }
            let sd = (var * 0.125).sqrt();
            st.stat[kk] = if aliased { sd * k::C_0_7 } else { sd };
            start = end;
        }
    }

    /// Run the decision on the post-M/S psyOut pair and apply it (chosen bands: ch0 := downmix, ch1 := 0, thresholds/energies/PE replaced,
    /// ch0 flags `(f & ~7) | 2`, ch1 flags `|= 0x20`); publishes `ch1.intensity` (positions `IC+0x1a64`, phase `IC+0x19ec`).
    /// Returns `false` (nothing touched) when the gate fails. `l0/l1` are the same line arrays that were given to `analyze` (the M/S block
    /// never modifies them).
    pub fn decide_and_apply(&mut self, c0: &mut PsyOut, c1: &mut PsyOut, l0: &ChannelLines, l1: &ChannelLines) -> bool {
        if !self.applies(c0, c1) {
            return false;
        }
        let bt = c0.wseq;
        let short = bt == 2;
        let nsfb = c0.max_sfb as usize;
        let nspw = c0.nsfb_per_group as usize;
        let ng = c0.ngroups as usize;
        let aliased = l0.pair_im.is_none();
        let b9 = l0.psy_spectrum_flag && l1.psy_spectrum_flag;
        let use_stat = !short && b9;

        self.estimate_ild_distortion_short(bt, &l0.en, &l1.en);
        if use_stat {
            self.stationarity(&l0.winen, &l1.winen, aliased);
        }
        self.get_correlation_long(&l0.en, &l1.en, aliased);
        let n_long = self.cfg.n_long;
        let n_short = self.cfg.n_short;
        if short {
            self.map_to_short(nsfb, ng);
            // prevCount = last long sfb whose map <= nsfb (asm 0x3b2a8..0x3b2c0)
            let mut i = n_long;
            loop {
                i -= 1;
                if (self.cfg.map[i] as usize) <= nsfb {
                    break;
                }
            }
            self.st.prev_count = i as i32;
        } else {
            self.st.prev_count = nsfb as i32;
        }
        self.x1_terms(short, use_stat, nsfb, nspw, ng);
        {
            let st = &mut self.st;
            for g in 0..ng {
                for s in 0..nsfb {
                    let idx = g * nspw + s;
                    let off = c0.start[idx] as usize;
                    let w = c0.width[idx] as usize;
                    st.pe_dm[idx] = eval_percep_entropy_by_line(st.thr[idx], &st.dm[off..off + w], w as u32);
                }
            }
        }
        let prev_bt = self.st.prev_bt;
        self.remap_state(bt, prev_bt, n_long, n_short);
        self.trellis(c0, c1, nsfb, nspw, ng);

        // ---- apply (C) ----
        let st = &mut self.st;
        for g in 0..ng {
            for s in 0..nsfb {
                let idx = g * nspw + s;
                st.state[s] = st.is_flag[idx] as i32;
                if st.is_flag[idx] == 1 {
                    c0.flags[idx] = (c0.flags[idx] & !7) | 2;
                    c1.flags[idx] |= 0x20;
                    let off = c0.start[idx] as usize;
                    let w = c0.width[idx] as usize;
                    c0.spectrum[off..off + w].copy_from_slice(&st.dm[off..off + w]);
                    for x in &mut c1.spectrum[off..off + w] {
                        *x = 0.0;
                    }
                    c0.energy[idx] = st.dm_energy[idx];
                    c0.thr[idx] = st.thr[idx];
                    c0.pe[idx] = st.pe_dm[idx];
                    c1.energy[idx] = 0.0;
                    c1.thr[idx] = 1.0;
                    c1.pe[idx] = 0.0;
                } else {
                    st.pos[idx] = INT_MIN;
                    st.phase[idx] = 0;
                    st.is_flag[idx] = 0;
                }
            }
            for s in nsfb..nspw {
                let idx = g * nspw + s;
                st.pos[idx] = INT_MIN;
                st.phase[idx] = 0;
                st.is_flag[idx] = 0;
            }
        }
        for i in nsfb.min(52)..52 {
            st.state[i] = -1;
        }
        st.prev_bt = bt as i32;
        c1.intensity = Some(IntensityData { flag: st.phase, position: st.pos });
        true
    }

    /// 4.2.4 map of the long-sfb correlation penalties `f` to short bands (`IC.f2808`, replicated per group).
    fn map_to_short(&mut self, nsfb: usize, ng: usize) {
        let cfg = &self.cfg;
        let st = &mut self.st;
        let nlong = cfg.n_long;
        let mut w16 = 1usize;
        let mut kk = 0usize;
        while kk < nlong {
            let u = cfg.map[kk];
            let thr;
            if cfg.map[kk] == cfg.map[w16] {
                let mut m = 0.0f32;
                loop {
                    let e = st.dm_energy[w16];
                    if !(e <= m) {
                        m = e;
                    }
                    w16 += 1;
                    if !(w16 < 52 && cfg.map[w16] == u) {
                        break;
                    }
                }
                thr = m * 0.25;
            } else {
                thr = 0.0;
            }
            let mut acc = 0.0f32;
            let mut cnt = k::C_1E_20;
            let mut k2 = kk;
            loop {
                if st.dm_energy[k2] > thr {
                    acc += st.f[k2];
                    cnt += 1.0;
                }
                k2 += 1;
                if !(k2 < 52 && cfg.map[k2] == u) {
                    break;
                }
            }
            st.f2808[u as usize] = acc / cnt;
            kk = k2;
        }
        for g in 1..ng {
            for s in 0..nsfb {
                st.f2808[g * 15 + s] = st.f2808[s];
            }
        }
    }

    /// 4.2.5 `X1`: correlation/ILD/stationarity penalty ~ `exp(2a) - 1` via the pow2 table; clamps the used `f`/`f2808` entries to <= 1.0 in place.
    fn x1_terms(&mut self, short: bool, use_stat: bool, nsfb: usize, nspw: usize, ng: usize) {
        let cfg = &self.cfg;
        let st = &mut self.st;
        let wbr = cfg.wbr;
        let k1 = wbr * k::C_0_35;
        let inv1 = 1.0f32 / k1;
        let k3 = wbr * 3.0;
        let inv3 = 1.0f32 / k3;
        let f103 = wbr * 1.5;
        let f93 = 1.0f32 / f103;
        let one = 1.0f32;
        for g in 0..ng {
            for s in 0..nsfb {
                let v = if short { st.f2808[g * 15 + s] } else { st.f[s] };
                let v = if v > one {
                    if short {
                        st.f2808[g * 15 + s] = one;
                    } else {
                        st.f[s] = one;
                    }
                    one
                } else {
                    v
                };
                let mut a = inv1 * ((one - v) - k1);
                let u = if short { s } else { cfg.map[s] as usize };
                let b = inv3 * (st.ild[u] - k3);
                a = if a > b { a } else { b };
                if use_stat {
                    let c = f93 * (st.stat[cfg.map[s] as usize] - f103);
                    a = if a > c { a } else { c };
                }
                let mut x = a * k::C_2_88539;
                if x > 127.0 {
                    x = 127.0;
                }
                // The original does the table lookup inline; it is `pow2_est` for every x of this stage (x <= 127 after the clamp, and x >= -2.9 because the
                // penalty `a` is at least -1: the correlation term `(1 - v) / k1 - 1` has `v <= 1`, and the other terms only raise the maximum).
                let mut p2 = pow2_est(x) + (-1.0);
                if p2 < 0.0 {
                    p2 = 0.0;
                }
                if p2 > k::C_1E8 {
                    p2 = k::C_1E8;
                }
                st.x1[nspw * g + s] = p2;
            }
        }
    }

    /// 4.2.6 state remap when the block class (EIGHT_SHORT or not) changes.
    fn remap_state(&mut self, bt: u32, prev_bt: i32, n_long: usize, n_short: usize) {
        if (bt == 2) == (prev_bt == 2) {
            return;
        }
        let cfg = &self.cfg;
        let st = &mut self.st;
        let mut tmp = [-1i32; 60];
        let n;
        if prev_bt == 2 {
            for i in 0..n_long {
                tmp[i] = st.state[cfg.map[i] as usize];
            }
            n = n_long;
        } else {
            let mut kk = 0usize;
            while kk < n_long {
                let u = cfg.map[kk];
                let mut m = 0.0f32;
                let mut j = kk;
                loop {
                    if m < st.dm_energy_prev[j] {
                        m = st.dm_energy_prev[j];
                    }
                    j += 1;
                    if !(j < 52 && cfg.map[j] == u) {
                        break;
                    }
                }
                let thr = m * 0.25;
                let mut acc = -1i32;
                for k2 in kk..j {
                    let s = st.state[k2];
                    if acc != s {
                        let new = if acc == -1 {
                            s
                        } else if s == -1 {
                            acc
                        } else {
                            2
                        };
                        if !(st.dm_energy_prev[k2] <= thr) {
                            acc = new;
                        }
                    }
                }
                tmp[u as usize] = acc;
                kk = j;
            }
            n = n_short;
        }
        st.state[..n].copy_from_slice(&tmp[..n]);
    }

    /// 4.2.7 two-state Viterbi-like trellis over the sfb of each window group (state 1 = intensity, 0 = not); writes `is_flag` and `state`.
    fn trellis(&mut self, c0: &PsyOut, c1: &PsyOut, nsfb: usize, nspw: usize, ng: usize) {
        let st = &mut self.st;
        let t = k::T_0_1;
        let pe_l = &c0.pe;
        let pe_r = &c1.pe;
        let big = k::C_1E30;
        let one = 1.0f32;
        let m02 = k::C_NEG_0_2;
        let th08 = k::C_0_8;
        let asym = |tns: &[u8; NB], i: usize| -> bool { (tns[i] as u32).wrapping_sub(1) <= 1 };
        for g in 0..ng {
            let base = g * nspw;
            let pst = (st.state[0].wrapping_add(1)) as u32;
            let mut c0v = 0.0f32;
            if pst > 3 || pst == 2 {
                let mut tt = st.v2[base].mul_add(t, 0.0);
                tt = tt + 0.0;
                tt = tt * t;
                c0v = tt + 0.0;
            }
            let rat = (st.pe_dm[base] + t) / ((pe_l[base] + pe_r[base]) + t);
            let a = if th08 <= rat { rat - one } else { m02 };
            let q = st.x2[base] + st.x1[base] * 2.5;
            let mut r = if asym(&st.tns_cur, base) { q + 2.0 } else { q };
            if pst > 3 || pst == 1 {
                r = r + st.v2[base];
                r = r + 0.0;
            }
            let c1v = a + r * t;
            let mut cost = [c0v, c1v];
            if nsfb < 2 {
                let f = (cost[1] <= cost[0]) as u8;
                st.is_flag[base] = f;
                st.state[0] = f as i32;
                continue;
            }
            let mut back = [[0u8; 2]; 64];
            for s in 1..nsfb {
                let idx = base + s;
                let prev_s = st.state[s];
                let mut new = [0.0f32; 2];
                for d in 0..2usize {
                    let mut best = big;
                    let mut bsrc = 0u8;
                    for src in 0..2usize {
                        let mut a = 0.0f32;
                        let mut b = 0.0f32;
                        if d == 1 {
                            let rat = (st.pe_dm[idx] + t) / ((pe_l[idx] + pe_r[idx]) + t);
                            a = if th08 <= rat { rat - one } else { m02 };
                            let q = st.x2[idx] + st.x1[idx] * 2.5;
                            b = if asym(&st.tns_cur, idx) { q + 2.0 } else { q };
                        }
                        if src != d {
                            a = a + 4.0 / ((pe_l[idx] + pe_r[idx]) + t);
                        }
                        if prev_s != -1 && prev_s != 2 && (d as i32) != prev_s {
                            b = if d == 1 { b + st.v2[idx] } else { st.v2[idx].mul_add(t, b) };
                            b = b + 0.0;
                        }
                        let c = b.mul_add(t, a + cost[src]);
                        if best > c {
                            best = c;
                            bsrc = src as u8;
                        }
                    }
                    new[d] = best;
                    back[s][d] = bsrc;
                }
                cost = new;
            }
            let f93 = if cost[0] < big { cost[0] } else { big };
            let mut stt: usize = if f93 > cost[1] { 1 } else { 0 };
            for s in (0..nsfb).rev() {
                st.is_flag[base + s] = stt as u8;
                st.state[s] = stt as i32;
                stt = back[s][stt] as usize;
            }
            if nsfb <= 0x33 {
                for i in nsfb..52 {
                    st.state[i] = -1;
                }
            }
        }
    }
}
