//! `_pamMonaural(C, wseq, 1024, P, T, quality)` (0xa27e0): Apple's psychoacoustic model for one channel and frame, with its helpers
//! `_reducePeaks_Neon` (0x23a00), `_spreadEnergy_Neon` (0x24480), `_preEchoReduction` (0xa2650), `stic::pow_10_ToThe_01_X_est`
//! (0xafc4), the inline `dB()` approximation (`stic::logMantissa`) and the 3-deep float FIFOs (`_AddFloatFifoElem`).
//!
//! Numeric conventions: every float is f32; `mul_add` = AArch64 `fmadd`/`fmla`; min/max written the way the asm/model compares them
//! (`>`/`<` on floats, signed integer compares for the per-sfb quiet floor).  FZ: the code is plain IEEE f32 and does not emulate FPCR.FZ.

use crate::portable::aac::psy::param::PamParam;
use crate::portable::aac::psy::tables::{LOG_MANTISSA, POW2_TABLE};
use crate::portable::aac::psy::vdsp::sve;
use crate::portable::arm::{max_propagating, min_propagating};

const K3: f32 = f32::from_bits(0x4040a8c1); // DAT_0092261c = 3.0102999

/// Apple's inline 10*log10 approximation: `fma(float(exp-127), 3.0103, logMantissa[(bits>>16)&0x7f])` (the `fmadd` at 0xa27e0).
/// The exponent is taken with an arithmetic shift of the (signed) bit pattern, so negative inputs give large negative exponents.
#[inline]
pub fn db(x: f32) -> f32 {
    let u = x.to_bits();
    let e = ((u as i32) >> 23) - 0x7f;
    (e as f32).mul_add(K3, LOG_MANTISSA[((u >> 16) & 0x7f) as usize])
}

/// `stic::pow2table`-based `2^x`: `w8 = (int)(x + 127); frac = x - float(w8 - 127); idx = (int)(frac * 64); bits = pow2table[idx] | (w8 << 23)`
/// (`(int)` = `fcvtzs`: saturating truncation, NaN -> 0).  The table index is clamped to 0..=64 for non-finite arguments (Apple would read
/// outside the table; unreachable for the arguments of the encoder).
#[inline]
pub fn pow2_est(x: f32) -> f32 {
    let w8 = (x + 127.0) as i32;
    let fr = x - (w8.wrapping_sub(127) as f32);
    let idx = ((fr * 64.0) as i32).clamp(0, 64) as usize;
    f32::from_bits(POW2_TABLE[idx] | ((w8 as u32) << 23))
}

/// `stic::pow_10_ToThe_01_X_est` (0xafc4): table-driven `10^(x/10)` = [`pow2_est`]`(x * 0.33219281)`.
#[inline]
pub fn pow10_est(x: f32) -> f32 {
    pow2_est(x * crate::portable::aac::types::K_DB_TO_LOG2)
}

/// `FloatFifo` (fifo_buf.c): 3 vectors, newest first; `push` = `_AddFloatFifoElem`.
#[derive(Clone, Debug)]
pub struct Fifo {
    pub v: [Vec<f32>; 3],
}

impl Fifo {
    /// `_CreateFloatFifoBuffer` + 3x `_AddFloatFifoElem(init)` (aacOpen: init = quiet threshold in dB).
    pub fn filled(init: &[f32]) -> Self {
        Fifo { v: [init.to_vec(), init.to_vec(), init.to_vec()] }
    }
    #[inline]
    pub fn push(&mut self, src: &[f32]) {
        self.v.rotate_right(1);
        self.v[0][..src.len()].copy_from_slice(src);
    }
}

/// `_preEchoReduction(a, b, c, fifo, n, out)`: `out = max(cur + a, min(two + c, min(prev + b, cur)))` with cur/prev/two = fifo[0..3].
/// `vDSP_vmin`/`vDSP_vmax` are ARM `FMIN`/`FMAX` with the second operand first (NaNs propagate, `-0 < +0`).
fn pre_echo_reduction(a: f32, b: f32, c: f32, fifo: &Fifo, n: usize, out: &mut [f32]) {
    let (cur, prev, two) = (&fifo.v[0], &fifo.v[1], &fifo.v[2]);
    for i in 0..n {
        let o = min_propagating(cur[i], prev[i] + b);
        let o = min_propagating(o, two[i] + c);
        out[i] = max_propagating(o, cur[i] + a);
    }
}

#[inline]
fn at(v: &[f32], i: i32) -> f32 {
    // Apple reads memset(0xff) (NaN) scratch outside the written range; indices are always in range for the engine rates
    if i >= 0 && (i as usize) < v.len() { v[i as usize] } else { f32::NAN }
}

/// The two moving-sum passes of `_pamMonaural` (reflect at the low end, constant at the top; asm 0xa3424..0xa3560).
/// `dst[i]` = (scale *) sum of `src` over the centred window of `w` entries; only `dst[0..n)` is written.
fn smooth(src: &[f32], dst: &mut [f32], w: i32, n: i32, scale: Option<f32>) {
    let mut s = 0f32;
    if w >= 1 {
        let mut k = n;
        loop {
            k -= 1;
            s += at(src, k);
            if !(n - w < k) {
                break;
            }
        }
    }
    let put = |dst: &mut [f32], i: i32, s: f32| {
        if i >= 0 && (i as usize) < dst.len() {
            dst[i as usize] = match scale {
                Some(c) => c * s,
                None => s,
            };
        }
    };
    let top_lo = n - (w + 1) / 2;
    let mut l40 = n;
    if top_lo < n {
        let mut k = n;
        let mut l16 = n;
        loop {
            l40 = l16;
            k -= 1;
            put(dst, k, s);
            l16 = l40 - 1;
            if !(top_lo < l16) {
                break;
            }
        }
    }
    let mut u26 = if top_lo < n { l40 - 2 } else { n - 1 };
    if u26 >= 0 {
        let mut i21 = n + !w; // n - w - 1
        let mut l16 = n;
        let mut l17 = u26 + 1;
        let mut i32_ = (w - n) + 2;
        let mut i19 = i21;
        loop {
            l16 -= 1;
            s = (s - at(src, l16)) + at(src, i21);
            i21 = if i19 > 0 { i19 - 1 } else { i32_ };
            put(dst, u26, s);
            u26 -= 1;
            i32_ += 1;
            l17 -= 1;
            i19 -= 1;
            if l17 == 0 {
                break;
            }
        }
    }
}

#[derive(Clone, Debug, Default)]
struct Scratch {
    l380: Vec<f32>,
    loc748: Vec<f32>,
    loc6b0: [Vec<f32>; 8],
    ldb: Vec<f32>,
    s1: Vec<f32>,
    s2: Vec<f32>,
    pk: Vec<f32>,
    d: Vec<f32>,
    tr: Vec<f32>,
    tw: Vec<f32>,
    r: Vec<f32>,
    fwd: Vec<f32>,
    bwd: Vec<f32>,
    q: Vec<f32>,
    th0: Vec<f32>,
    sp: Vec<f32>,
    a: Vec<f32>,
    smr: Vec<f32>,
    thr_p: Vec<f32>,
    min_s: Vec<f32>,
    t9: Vec<f32>,
}

/// `T` = `ThrChannel` (calloc 0xd90, `psy+0x90+8*ch`): per-channel state (FIFOs, `+0x70`, `+0x74`, `+0x78`) and the model outputs.
#[derive(Clone, Debug)]
pub struct ThrChannel {
    /// +0x58 FIFO of the long thresholds in dB (width nS; init 3x quiet dB `P+0xa0`).
    pub fifo_long: Fifo,
    /// +0x60 FIFO of the short thresholds in dB per window (width nSh; init 3x `P+0xa8`).
    pub fifo_short: Fifo,
    /// +0x70 smoothed level of partition 0 (init -30.0).
    pub s70: f32,
    /// +0x74 window sequence of the last call (write-only in the model).
    pub last_wseq: i32,
    /// +0x78 last call ran the short path (`doShort`).
    pub last_short: bool,
    /// +0x18 partition energy + 1.0 (nP).
    pub t3: Vec<f32>,
    /// +0x38 per-long-sfb energy sum E (nS).
    pub t7: Vec<f32>,
    /// +0x30 short reference level per short sfb (nSh).
    pub t6: Vec<f32>,
    /// +0x40[w] per-short-sfb energy per window (8 x nSh).
    pub t8: [Vec<f32>; 8],
    /// +0x48 **long threshold per sfb, power domain** (nS).
    pub thr: Vec<f32>,
    /// +0x50[w] **short thresholds per window**, power domain (8 x nSh).
    pub thr_short: [Vec<f32>; 8],
    /// Mirror of `PsyChannel+0x1e0` (1024 energies E) and `+0x1e8[w]` (8 x 128 short-window energies): written by
    /// [`crate::portable::aac::psy::glue::analyse_channel`] (E5), read by `pam_monaural`.
    pub e_long: Vec<f32>,
    pub e_short: Vec<f32>,
    sc: Scratch,
}

impl ThrChannel {
    /// Fresh channel state as built by aacOpen/initPamVar.
    pub fn new(p: &PamParam) -> Self {
        let z = |n: usize| vec![0f32; n];
        let n_p = p.n_p;
        ThrChannel {
            fifo_long: Fifo::filled(&p.quiet_long_db),
            fifo_short: Fifo::filled(&p.quiet_short_db),
            s70: -30.0,
            last_wseq: 0,
            last_short: false,
            t3: z(n_p),
            t7: z(p.n_s),
            t6: z(p.n_sh),
            t8: std::array::from_fn(|_| z(p.n_sh)),
            thr: z(p.n_s),
            thr_short: std::array::from_fn(|_| z(p.n_sh)),
            e_long: z(1024),
            e_short: z(1024),
            sc: Scratch {
                l380: z(p.n_sh),
                loc748: z(p.n_sh),
                loc6b0: std::array::from_fn(|_| z(p.n_sh)),
                ldb: z(n_p),
                s1: z(n_p),
                s2: z(n_p),
                pk: z(n_p),
                d: z(n_p),
                tr: z(n_p),
                tw: z(n_p),
                r: z(n_p),
                fwd: z(n_p + 1),
                bwd: z(n_p + 1),
                q: z(n_p),
                th0: z(n_p),
                sp: z(n_p),
                a: z(n_p),
                smr: z(n_p),
                thr_p: z(n_p),
                min_s: z(p.n_sh),
                t9: z(p.n_s),
            },
        }
    }
}

/// `_pamMonaural(C, wseq, 1024, P, T, quality)`.
///
/// * `e_long`: `PsyChannel+0x1e0` energies `E` (1024 floats; `2*x^2` for quality <= 0x40, `re^2+im^2` for 0x60);
/// * `e_short`: the 8 short-window energy spectra `*(C+0x1e8+8w)` (window `w` at `w*128`); only read when the short path runs
///   (`wseq == 2` or `*flags != 0`; callers that never request it may pass an empty slice);
/// * `flags`: `PsyChannel+0x240` (u16), read and possibly cleared;
/// * `wseq`: 0 only-long, 1 start, 2 eight-short, 3 stop; `quality`: 0x20/0x40/0x60.
///
/// Outputs/state are in `t` (`thr`, `thr_short`, `t7`, `t8`, ...).
pub fn pam_monaural(p: &PamParam, t: &mut ThrChannel, e_long: &[f32], e_short: &[f32], flags: &mut u16, wseq: u32, quality: u32) {
    let (n_p, n_s, n_sh) = (p.n_p, p.n_s, p.n_sh);
    let is_long_simple = if n_sh >= 1 { wseq != 2 && *flags == 0 } else { true };
    let do_short = !is_long_simple;
    let ThrChannel { fifo_long, fifo_short, s70, last_wseq, last_short, t3, t7, t6, t8, thr: t9out, thr_short: t10, sc, .. } = t;
    let pb = &p.part_bounds;
    let sfbo = &p.sfb_off_long;

    // 1. energies (vDSP_sve + vsadd 1.0)
    for i in 0..n_p {
        t3[i] = sve(&e_long[pb[i] as usize..pb[i + 1] as usize]) + 1.0;
    }
    for s in 0..n_s {
        t7[s] = sve(&e_long[sfbo[s] as usize..sfbo[s + 1] as usize]);
    }
    // 2. short-path levels
    if do_short {
        let sho8 = &p.sfb_off_short_x8;
        for k in 0..n_sh {
            sc.l380[k] = sve(&e_long[sho8[k] as usize..sho8[k + 1] as usize]) + 1.0;
        }
        for k in 0..n_sh {
            sc.loc748[k] = db(sc.l380[k]) - p.p70[k];
        }
        for w in 0..8 {
            let ew = &e_short[w * 128..w * 128 + 128];
            let mut prev = 0usize;
            for k in 0..n_sh {
                let off = (sho8[k + 1] / 8) as usize; // (x+7)>>3 branch of the asm is only for negatives
                t8[w][k] = sve(&ew[prev..off]);
                prev = off;
            }
            for k in 0..n_sh {
                sc.loc6b0[w][k] = db(t8[w][k]) - p.p68[k];
            }
        }
    }
    // 3. weighted partition level
    for i in 0..n_p {
        sc.ldb[i] = db(t3[i] * p.p_d0[i]);
    }
    let ldb = &sc.ldb;
    // 4. effective top partition
    let mut n_eff = n_p;
    let mut b_var10 = true;
    if n_p == 1 {
        b_var10 = false;
        n_eff = 1;
    } else if n_p > 1 {
        let mut l = ldb[0];
        let mut f = l + -24.0;
        let mut last = n_p as i32;
        for j in 1..n_p {
            let lm2 = l + -2.0;
            let env = if lm2 > f { lm2 } else { f };
            let x = ldb[j];
            if env < x {
                last = j as i32;
                f = x + -24.0;
                l = x;
            } else {
                l = lm2;
            }
        }
        if last > 20 {
            if last <= p.w1 + 1 {
                last = p.w1 + 1;
            }
            if last <= p.w2 + 1 {
                last = p.w2 + 1;
            }
            let last = (last as usize).min(n_p);
            let (mut s0, mut s1) = (0f32, 0f32);
            for k in last - 20..last {
                let x = ldb[k];
                s0 += x;
                s1 = x.mul_add(x, s1);
            }
            let v = (s0 * f32::from_bits(0xbb23d70a)) * s0; // -0.0025f (DAT_009227cc)
            let v = s1.mul_add(f32::from_bits(0x3d4ccccd), v); // 0.05f (DAT_00922674)
            if v <= 60.0 {
                n_eff = last;
            }
        }
    }
    // 5. smoothed mean level S2 (W1 then W2 windows)
    smooth(ldb, &mut sc.s1, p.w1, n_eff as i32, None);
    let c = p.inv_w1 * p.inv_w2;
    smooth(&sc.s1, &mut sc.s2, p.w2, n_eff as i32, Some(c));
    let s2 = &sc.s2;
    // 6. peak runs / tonality
    let pk = &mut sc.pk;
    pk.fill(0.0);
    let d = &mut sc.d;
    {
        let mut inrun = false;
        let mut runmax = 0f32;
        let mut cnt = 0usize;
        let mut start = 0usize;
        for j in 0..n_eff {
            let dj = ldb[j] - s2[j];
            d[j] = dj;
            let mut write = false;
            if !inrun && dj > 6.0 {
                cnt = 0;
                runmax = 0.0;
                start = j;
                if runmax < dj {
                    runmax = dj;
                }
                cnt += 1;
                inrun = true;
                if j == n_eff - 1 {
                    write = true;
                }
            } else if inrun {
                if !(dj < 0.0) {
                    if runmax < dj {
                        runmax = dj;
                    }
                    cnt += 1;
                    if j == n_eff - 1 {
                        write = true;
                    }
                } else {
                    inrun = false;
                    write = true;
                }
            }
            if write {
                pk[start + cnt / 2] = runmax;
            }
        }
        for j in n_eff..n_p {
            d[j] = f32::NAN; // S2[j >= nEff] is uninitialised (NaN) scratch in Apple; overwritten below except d[nP-1]
        }
        d[n_p - 1] = pk[n_p - 1];
    }
    if b_var10 {
        let (p20, p1f) = (&p.p_100, &p.p_f8);
        let mut v = d[n_p - 1];
        for i in (0..n_p - 1).rev() {
            v = p20[i].mul_add(-0.5, v);
            if !(v > pk[i]) {
                v = pk[i];
            }
            d[i] = v;
        }
        let mut v = d[0];
        for i in 1..n_p {
            v = p1f[i - 1].mul_add(0.5, v);
            if !(v > d[i]) {
                v = d[i];
            }
            d[i] = v;
        }
    }
    let tr = &mut sc.tr;
    for i in 0..n_p {
        tr[i] = if d[i] > 0.0 { pk[i] / d[i] } else { 0.0 };
    }
    // 11. tonality weight TW
    let tw = &mut sc.tw;
    {
        let mut s = tr[0];
        let (mut w9, mut w10) = (0usize, 0usize);
        for i in 0..n_p {
            for _ in 0..p.ton_back[i].max(0) {
                s -= tr[w9];
                w9 += 1;
            }
            for _ in 0..p.ton_fwd[i].max(0) {
                if w10 < n_p - 1 {
                    w10 += 1;
                    s += tr[w10];
                }
            }
            let mut tt = s * p.ton_norm[i];
            tt = p.p160 * tt;
            tt *= tt;
            tt *= tt;
            tt += 1.0;
            tw[i] = 1.0 / tt;
        }
    }
    // 12. reducePeaks (0x23a00): vectors of 4 + scalar remainder do identical arithmetic
    let r = &mut sc.r;
    for i in 0..n_eff {
        let tt = s2[i] + p.p15c;
        let diff = ldb[i] - tt;
        let pr = min_propagating((diff * tw[i]) * p.p158, 15.0);
        r[i] = if diff > 0.0 { ldb[i] - pr } else { ldb[i] };
    }
    for i in n_eff..n_p {
        r[i] = ldb[i];
    }
    for x in r.iter_mut() {
        *x = pow10_est(*x);
    }
    let rp = &sc.r;
    // 13. asymmetric power-domain spreading
    {
        let (fwd, bwd) = (&mut sc.fwd, &mut sc.bwd);
        fwd.fill(0.0);
        bwd.fill(0.0);
        let (mut f, mut bv) = (0f32, 0f32);
        for k in 0..n_p {
            fwd[k] = f + rp[k];
            f = fwd[k] * p.p_d8[k];
            let j = n_p - 1 - k;
            bv = (bv + rp[j]) * p.p_e0[j];
            bwd[j] = bv;
        }
        for k in 0..n_p {
            let l = p.spread_lo[k] as usize;
            let mut acc = if k < n_p - 1 { fwd[l] + bwd[k + 1] } else { fwd[l] };
            for idx in l + 1..=k {
                acc += rp[idx];
            }
            sc.q[k] = acc;
        }
    }
    for i in 0..n_p {
        sc.th0[i] = db(sc.q[i]) - p.mask_offset[i];
    }
    // 14. dB-domain spreading (_spreadEnergy_Neon): forward `max(prev + slopeF, x)` then backward `max(next - slopeB, x)`
    {
        let st = p.p110;
        let sp = &mut sc.sp;
        sp[0] = sc.th0[0];
        let mut prev = sc.th0[0];
        let mut c_ = 0f32;
        for i in 0..n_p - 1 {
            let mut cand = prev + p.p_e8[i];
            if st != 0.0 {
                cand = (-c_).mul_add(c_, cand);
            }
            let x = sc.th0[i + 1];
            let val;
            if cand > x {
                val = cand;
                c_ += st;
            } else {
                val = x;
                c_ = 0.0;
            }
            sp[i + 1] = val;
            prev = val;
        }
        let mut v = sp[n_p - 1];
        for j in (0..n_p - 1).rev() {
            let tt = v - p.p_f0[j];
            v = if tt > sp[j] { tt } else { sp[j] };
            sp[j] = v;
        }
    }
    for i in 0..n_p {
        sc.a[i] = sc.sp[i] - p.p_c8[i];
        sc.thr_p[i] = sc.a[i] + p.p58[i];
        sc.smr[i] = db(t3[i]) - sc.a[i];
    }
    // 15. SMR floor ramp over the top 10 % of the partitions (P+0x130 > 0; tonal rows of the <= 32 kHz tables only)
    if p.p130 > 0.0 {
        let start = ((n_eff as i32) as f32 * f32::from_bits(0x3f666666)) as i32; // 0.9f (DAT_009225d8)
        let mut cnt = 1.0f32;
        if 1.0 <= (n_eff as i32 - start) as f32 {
            cnt = (n_eff as i32 - start) as f32;
        }
        let mut kk = 1i32;
        let ramp = |smr: &mut [f32], a: &mut [f32], thr: &mut [f32], i: usize, adj0: f32| {
            let v = smr[i];
            if v < 0.0 {
                let mut adj = adj0;
                if 0.0 - v <= adj {
                    adj = 0.0 - v;
                }
                smr[i] = v + adj;
                a[i] -= adj;
                thr[i] -= adj;
            }
        };
        let (smr, a, thr) = (&mut sc.smr, &mut sc.a, &mut sc.thr_p);
        for i in start.max(0) as usize..n_eff {
            ramp(smr, a, thr, i, (p.p130 / cnt) * (kk as f32));
            kk += 1;
        }
        for i in n_eff..n_p {
            ramp(smr, a, thr, i, p.p130);
        }
    }
    // 16. partition-0 smoothing with the carried state T+0x70
    if p.p134 > 0.0 {
        let mut st = ldb[0].mul_add(0.5, *s70 * 0.5);
        if st < -30.0 {
            st = -30.0;
        }
        *s70 = st;
        let thr0 = sc.thr_p[0];
        // asm 0xa3cb8: fcmp thr0,st ; fcsel mi -> m = thr0 < st ? thr0 : st + 0.0 ; fmul w*m ; fmadd (1-w)*thr0 + (w*m)
        let m = if thr0 < st { thr0 } else { st + 0.0 };
        sc.thr_p[0] = (1.0 - p.p134).mul_add(thr0, p.p134 * m);
    }
    // 17. per-sfb dB threshold with quiet floor: SIGNED-INTEGER min/max on the float bit patterns
    let t9 = &mut sc.t9;
    for s in 0..n_s {
        let [lo, hi] = p.sfb_range_long[s];
        let (lo, hi) = (lo as usize, hi as usize);
        let mut m = sc.thr_p[lo].to_bits() as i32;
        for idx in lo + 1..=hi {
            let b = sc.thr_p[idx].to_bits() as i32;
            if b <= m {
                m = b;
            }
        }
        let q = p.quiet_long_db[s].to_bits() as i32;
        if m <= q {
            m = q;
        }
        t9[s] = f32::from_bits(m as u32);
    }
    // 18. short-window references (doShort)
    if do_short {
        for i in 0..n_sh {
            let [lo, hi] = p.sfb_range_short[i];
            if i == 0 && p.p134 > 0.0 {
                let mut m = sc.thr_p[lo as usize];
                for idx in lo + 1..=hi {
                    let x = sc.thr_p[idx as usize];
                    if x <= m {
                        m = x;
                    }
                }
                t6[0] = sc.loc748[0] - m;
            } else {
                let mean = if hi < lo {
                    f32::NAN
                } else {
                    let mut s = 0f32;
                    for idx in lo..=hi {
                        s += sc.thr_p[idx as usize];
                    }
                    s / ((hi + 1 - lo) as u32 as f32)
                };
                t6[i] = sc.loc748[i] - mean;
            }
        }
        for w in 0..8 {
            for i in 0..n_sh {
                let v = p.p80 + (sc.loc6b0[w][i] - t6[i]);
                t10[w][i] = if p.quiet_short_db[i] < v { v } else { p.quiet_short_db[i] };
            }
        }
    }
    // 19. quality gate and flags
    if quality < 0x21 || n_sh < 1 {
        *flags = 0;
    } else {
        if wseq != 2 && (*flags & 1) != 0 {
            let i1 = sfbo[p.sfb_5000hz as usize] as usize;
            let i0 = sfbo[p.sfb_200hz as usize] as usize;
            let mut s_lo = 1.0f32;
            for k in i0..i1 {
                s_lo += e_long[k];
            }
            let i2 = sfbo[p.n_s] as usize;
            let mut s_hi = 1.0f32;
            for k in i1..i2 {
                s_hi += e_long[k];
            }
            let ratio = (s_lo / ((i1 - i0) as f32)) / (s_hi / ((i2 - i0) as f32));
            if ratio < 10000.0 {
                *flags &= 0xfffe;
            }
        }
        if wseq != 2 && *flags != 0 {
            // long block with short-window protection: bound the long thresholds by the minimum short threshold
            let min_s = &mut sc.min_s;
            min_s.fill(crate::portable::aac::types::F_1E30); // DAT_00925b70 pattern = 1.0e30f
            for w in 0..8 {
                for k in 0..n_sh {
                    if t10[w][k] < min_s[k] {
                        min_s[k] = t10[w][k];
                    }
                }
            }
            let bit1 = (*flags >> 1) & 1 != 0;
            let (add, lim) = if !bit1 { (f32::from_bits(0x41707ae1), 7.0f32) } else { (f32::from_bits(0x41507ae1), 2.0f32) };
            // DAT_009227d4 = 15.03f (bit1 clear), DAT_009227d0 = 13.03f (bit1 set)
            for x in min_s.iter_mut() {
                *x += add;
            }
            let sho8 = &p.sfb_off_short_x8;
            let mut acc = 0f32;
            let mut j = 0usize;
            for k in 0..n_sh {
                let lim_off = sho8[k];
                while sfbo[j] <= lim_off {
                    acc += t9[j] - min_s[k];
                    j += 1;
                }
            }
            if acc > 0.0 {
                let mut j = 0usize;
                for k in 0..n_sh {
                    let lim_off = sho8[k];
                    while sfbo[j] <= lim_off {
                        let tt = t9[j];
                        let dd = tt - min_s[k];
                        if lim < dd {
                            t9[j] = if 12.0 <= dd { tt + -12.0 } else { min_s[k] };
                        }
                        j += 1;
                    }
                }
            }
        }
    }
    // 20. pre-echo limiter, long (push the UNREDUCED dB thresholds)
    fifo_long.push(&t9[..n_s]);
    pre_echo_reduction(p.preecho[0], p.preecho[1], p.preecho[2], fifo_long, n_s, t9);
    // 21. short windows
    if do_short {
        for w in 0..8 {
            fifo_short.push(&t10[w][..n_sh]);
            let (a1, a2, a3) = (p.preecho[3], p.preecho[4], p.preecho[5]);
            let thou = 1000.0f32;
            let args = if !*last_short && w < 2 {
                if w == 0 { (a1, thou, thou) } else { (a1, a2, thou) }
            } else {
                (a1, a2, a3)
            };
            pre_echo_reduction(args.0, args.1, args.2, fifo_short, n_sh, &mut t10[w]);
        }
        for w in 0..8 {
            for k in 0..n_sh {
                t10[w][k] = p.short_tilt[k] + t10[w][k];
            }
        }
    }
    // 22. units: dB per line -> dB per band, then dB -> power
    for s in 0..n_s {
        t9[s] = p.p78[s] + t9[s];
    }
    if do_short {
        for w in 0..8 {
            for k in 0..n_sh {
                t10[w][k] = p.p68[k] + t10[w][k];
            }
        }
    }
    for s in 0..n_s {
        t9[s] = pow10_est(t9[s]);
    }
    if do_short {
        for w in 0..8 {
            for k in 0..n_sh {
                t10[w][k] = pow10_est(t10[w][k]);
            }
        }
    }
    if p.max_smr_enabled != 0 {
        for s in 0..n_s {
            let tt = t7[s] * p.max_smr[s];
            if t9[s] <= tt {
                t9[s] = tt;
            }
        }
    }
    t9out[..n_s].copy_from_slice(&t9[..n_s]);
    *last_wseq = wseq as i32;
    *last_short = do_short;
}
