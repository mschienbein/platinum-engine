//! `LoopChannel` (Apple's `LC`, calloc 0xc800, persistent per channel), `_InitLoopChannel` (00043130), `_EstimateScaleFactors_PAM`
//! (0004ce50) and the scalefactor helpers `_getMaxquantHeadroom`, `_maxSfQuantizeZero`, `_increaseSmallScalefactors`,
//! `_ForceScalefactorsToMaxScfDelta`, `_moveFirstScalefactorToGlobalGain`, `_createScfGaps` / `_removeScfGaps` /
//! `_getSfbWithGapToDenseMap`.
//!
//! Ported literally from the decompiles of the loop kernel (checked by the encoder fixtures).

use crate::portable::aac::psy::db;
use crate::portable::aac::quant::bitcount::BitRes;
use crate::portable::aac::quant::fp::*;
use crate::portable::arm::{frecpe_f32, frsqrte_f32};
use crate::portable::math::logf;
use crate::portable::aac::types::{PsyOut, MAX_BANDS};
use crate::portable::vdsp;

/// Number of band entries of the per-band arrays (`types::MAX_BANDS`: Apple's arrays hold 0x78).
pub const NB: usize = MAX_BANDS;

/// Per-channel state of the loop kernel. Fields marked *persistent* survive between frames (and between kernel calls) exactly like
/// Apple's `LC`; everything else is rewritten by `_InitLoopChannel` every frame.
#[derive(Clone)]
pub struct LoopChannel {
    // ---- persistent across frames ------------------------------------------------------------------------------------------------
    /// `LC+0x3400`: `x34 * 2^(3 sf/16)` per band; bands whose `mask == 0` (and the whole array when `nsfbw == 0`) keep old values.
    pub x34s: [f32; 1024],
    /// `LC+0x6be8`: result struct of the last `_AACBitCount(Fast)` call on this channel (stale records, see `bitcount::BitRes`).
    pub res: BitRes,
    // ---- rewritten per frame ----------------------------------------------------------------------------------------------------
    pub nlines: usize,
    /// window_sequence (`+0x56e8`)
    pub win: u32,
    /// `psy+0x5bc` (`+0x56ec`)
    pub n56ec: i32,
    /// number of window groups (`+0x56f0`)
    pub ng: usize,
    /// sf array stride = bands per group (`+0x6060`)
    pub stride: usize,
    /// coded bands per group = psy max_sfb (`+0x6064`), 0 for silent frames
    pub nsfbw: usize,
    /// total band count `ng*stride` (`+0x605c`, argument `total` of the counters)
    pub maxsfb: usize,
    /// number of sfb records built (`+0x6058`)
    pub nsfb_tot: usize,
    pub is_pair: bool,
    pub grp: [i32; 8],
    /// `psy+0x9d4`: bit5 = gap band (intensity coded, no scalefactor)
    pub flags: [i32; NB],
    /// `+0x56e0`: any gap band present
    pub has_gap: bool,
    pub absx: [f32; 1024],
    pub sign: [i8; 1024],
    /// `|x|^0.75`
    pub x34: [f32; 1024],
    /// quantised magnitudes (`LC+0`), 0..=0x1fff
    pub q: [i32; 1024],
    /// signed quantised spectrum of the last exact pass (`LK+0x000+8c` buffer): `q * sign`
    pub iq: [i32; 1024],
    /// sfb records (`+0x4420`): start line / width / position of the maximum / threshold / 1/threshold / energy
    pub start: [i32; NB],
    pub width: [i32; NB],
    pub argmax: [i32; NB],
    pub thr: [f32; NB],
    pub invthr: [f32; NB],
    pub energy: [f32; NB],
    /// `+0x9380`: per band noise/threshold (`InvQuantizeAndEvaluateSpectrum` output)
    pub err: [f32; NB],
    /// kernel-convention scalefactors (`+0x6090`, `g*stride+b`): step = 2^((gg - sf)/4)
    pub sf: [i32; NB],
    /// `+0x6270`: band must be rescaled
    pub mask: [i32; NB],
    /// `+0x6450` global gain candidate (final: bitstream `global_gain = gg + 100`)
    pub gg: i32,
    /// `+0x6454` peak estimate of EstimateScaleFactors_PAM
    pub m6454: i32,
    /// `+0x645c` = 155
    pub gg_max: i32,
    /// `+0x6460`: scalefactors of the last exact pass (full `g*stride+b` layout)
    pub sf6460: [i32; NB],
    /// `psy+0xbc8` block present (`+0x6640`): `ipos`/`idir` valid
    pub has_ms: bool,
    pub ipos: [i32; NB],
    pub idir: [i32; NB],
    /// dense map (`_getSfbWithGapToDenseMap`): dense band -> `g*stride+b`
    pub dmap: [u8; NB],
    pub nd: usize,
    /// kernel result of the last call (gg of `+0x6458`, bits)
    pub bits: i32,
}

impl LoopChannel {
    pub fn new() -> Box<Self> {
        Box::new(Self {
            x34s: [0.0; 1024],
            res: BitRes::new(),
            nlines: 1024,
            win: 0,
            n56ec: 0,
            ng: 1,
            stride: 0,
            nsfbw: 0,
            maxsfb: 0,
            nsfb_tot: 0,
            is_pair: false,
            grp: [0; 8],
            flags: [0; NB],
            has_gap: false,
            absx: [0.0; 1024],
            sign: [0; 1024],
            x34: [0.0; 1024],
            q: [0; 1024],
            iq: [0; 1024],
            start: [0; NB],
            width: [0; NB],
            argmax: [0; NB],
            thr: [0.0; NB],
            invthr: [0.0; NB],
            energy: [0.0; NB],
            err: [0.0; NB],
            sf: [0; NB],
            mask: [0; NB],
            gg: 0,
            m6454: 0,
            gg_max: 155,
            sf6460: [0; NB],
            has_ms: false,
            ipos: [0; NB],
            idir: [0; NB],
            dmap: [0; NB],
            nd: 0,
            bits: 0,
        })
    }

    /// `x34` of the maximum line of band record `i`.
    #[inline]
    pub fn x34max(&self, i: usize) -> f32 {
        self.x34[(self.start[i] + self.argmax[i]) as usize]
    }

    /// `_InitLoopChannel(LC, psy, nlines, mode)` (00043130): `|x|`, sign, `|x|^0.75` (NEON Newton chain for quality 0x20/0x40, scalar
    /// `sqrt(x)*sqrt(sqrt(x))` otherwise), sfb records (`start` = running sum of widths), argmax, thresholds, initial global gain
    /// `gg0`, intensity blocks. Keeps `x34s` and `res` (persistent). Does **not** touch `sf`/`mask` (zeroed here like `bzero`).
    pub fn init(&mut self, psy: &PsyOut, nlines: usize, mode: u32) {
        self.win = psy.wseq;
        self.n56ec = psy.field_5bc;
        self.ng = psy.ngroups as usize;
        self.maxsfb = psy.total_bands as usize;
        self.stride = psy.nsfb_per_group as usize;
        self.nsfbw = psy.max_sfb as usize;
        self.nlines = nlines;
        self.is_pair = psy.is_pair;
        self.grp = psy.group_len;
        self.flags.copy_from_slice(&psy.flags[..NB]);
        self.has_gap = false;
        'o: for g in 0..self.ng {
            for b in 0..self.nsfbw {
                if self.flags[g * self.stride + b] & 0x20 != 0 {
                    self.has_gap = true;
                    break 'o;
                }
            }
        }
        for i in 0..nlines {
            self.absx[i] = psy.spectrum[i].abs();
        }
        if mode == 0x20 || mode == 0x40 {
            pow34_neon(&self.absx[..nlines], &mut self.x34[..nlines]);
        } else {
            for i in 0..nlines {
                let s = self.absx[i].sqrt();
                let s2 = s.sqrt();
                self.x34[i] = s * s2;
            }
        }
        for i in 0..nlines {
            let v = psy.spectrum[i];
            self.sign[i] = if v > 0.0 { 1 } else if v < 0.0 { -1 } else { 0 };
        }
        self.start = [0; NB];
        self.width = [0; NB];
        self.argmax = [0; NB];
        self.invthr = [0.0; NB];
        self.energy = [0.0; NB];
        self.thr = [0.0; NB];
        let mut n = 0usize;
        let mut pos = 0usize;
        if self.maxsfb >= 1 && nlines >= 1 {
            loop {
                let w = psy.width[n];
                self.start[n] = pos as i32;
                self.width[n] = w;
                self.energy[n] = psy.energy[n];
                self.thr[n] = psy.thr[n];
                self.invthr[n] = if psy.thr[n].to_bits() == 0 { 0.0 } else { 1.0f32 / psy.thr[n] };
                let wu = w as usize;
                self.argmax[n] = vdsp::maxvi(&self.absx[pos..(pos + wu).min(nlines)]).1 as i32;
                n += 1;
                if n >= self.maxsfb {
                    break;
                }
                pos += wu;
                if pos >= nlines {
                    break;
                }
            }
        }
        self.nsfb_tot = n;
        let mut mx = 0.0f32;
        for i in 0..n {
            let v = self.x34[(self.start[i] + self.argmax[i]) as usize];
            if v > mx || i == 0 {
                mx = v;
            }
        }
        let g0 = if mx > 0.0 { fcvtps(fma(logf(mx), LN_GAIN_SCALE, LN_GAIN_BIAS)) } else { 0 };
        self.gg = g0.max(-100).min(155);
        self.sf = [0; NB];
        self.mask = [0; NB];
        self.q = [0; 1024];
        self.has_ms = false;
        self.ipos = [0; NB];
        self.idir = [0; NB];
        if let Some(int) = &psy.intensity {
            self.has_ms = true;
            let nn = if self.win == 0 || self.win == 1 || self.win == 3 { 0x34 } else { NB };
            for k in 0..nn {
                self.idir[k] = int.flag[k] as i32;
                self.ipos[k] = int.position[k];
            }
        }
        self.m6454 = 0;
        self.err = [0.0; NB];
        self.bits = 0;
        self.build_dense_map();
    }

    /// `_getSfbWithGapToDenseMap`: dense band index -> `g*stride+b` of the non-gap bands.
    pub fn build_dense_map(&mut self) {
        let mut n = 0;
        for g in 0..self.ng {
            for b in 0..self.nsfbw {
                let i = g * self.stride + b;
                if self.has_gap && self.flags[i] & 0x20 != 0 {
                    continue;
                }
                self.dmap[n] = i as u8;
                n += 1;
            }
        }
        self.nd = n;
    }

    #[inline]
    fn is_gap(&self, i: usize) -> bool {
        self.has_gap && self.flags[i] & 0x20 != 0
    }
}

/// `|x|^0.75` of `_InitLoopChannel`'s NEON path (0004329c..000433f4): two Newton steps seeded by `frsqrte`/`frecpe`. Every `FUSED`
/// operation is a single-rounded fma; plain products are separate roundings.
pub fn pow34_neon(a: &[f32], out: &mut [f32]) {
    const FLT_MIN: f32 = f32::from_bits(0x0080_0000);
    for i in 0..a.len() {
        let x = a[i];
        let xp = x + FLT_MIN;
        let r0 = frsqrte_f32(xp);
        let e0 = frecpe_f32(xp);
        let q0 = frsqrte_f32(r0);
        let t = fma(e0, x, -1.0); // FUSED
        let q0sq = q0 * q0;
        let e1 = fma(e0, -t, e0); // FUSED (fmls)
        let q04 = q0sq * q0sq;
        let err1 = fma(q04, e1, -1.0); // FUSED
        let d1 = q0 * -0.25f32;
        let q1 = fma(err1, d1, q0); // FUSED
        let t2 = fma(e1, x, -1.0); // FUSED
        let q1sq = q1 * q1;
        let e2 = fma(e1, -t2, e1); // FUSED
        let q14 = q1sq * q1sq;
        let err2 = fma(q14, e2, -1.0); // FUSED
        let d2 = q1 * -0.25f32;
        let q2 = fma(err2, d2, q1); // FUSED
        let q2sq = q2 * q2;
        let r = q2 * q2sq;
        out[i] = if r > 0.0 { r } else { 0.0 }; // FMAX(r, +0)
    }
}

// ---------------------------------------------------------------------------------------------------------------------------------
// _EstimateScaleFactors_PAM

const EST_A: f32 = f32::from_bits(0x3f62_8f5d); // 0.885f   (vDSP_vsmsa multiplier)
const EST_B: f32 = f32::from_bits(0xc0ee_e148); // -7.465f  (vDSP_vsmsa addend)
const M10: f32 = -10.0;

/// `_EstimateScaleFactors_PAM` for one channel (the three stages are per-channel independent). `mode` = quality (0x40 engine;
/// 0x20's table-dB variant of stage 3 is not implemented (not an engine configuration); 0x60 uses the same logf stage 3).
pub fn estimate_scalefactors_pam(c: &mut LoopChannel) {
    // stage 1: per group, all non-gap bands: sf0 = vfix32(fma(fma(dB(en+1), .25, dB(invthr*width)), .885, -7.465))
    for g in 0..c.ng {
        let base = g * c.stride;
        for b in 0..c.nsfbw {
            let i = base + b;
            if c.flags[i] & 0x20 == 0 {
                let l180 = db(c.energy[i] + 1.0);
                let l250 = db(c.invthr[i] * c.width[i] as f32);
                let t = fma(l180, 0.25, l250); // FUSED vsma
                let t = fma(t, EST_A, EST_B); // FUSED vsmsa
                c.sf[i] = fcvtzs(t);
            }
        }
    }
    // stage 2: gg = 4 - first sf; delta chain clamped to +-60 against the previous band; mask = 1 for all bands
    // `first` = sf0 of the first non-gap band. Apple reads it at the PACKED index `g*nsfbw + b` (the running counter advances by `nsfbw`
    // per group), not at the strided `g*stride + b` of the flags: identical when nsfbw == stride or when the band is in group 0, otherwise
    // (3..8 channels, EIGHT_SHORT, group 0 entirely gap bands) it picks an unrelated, usually unwritten (0) entry. Decompile 0004ce50
    // 0004d1xx..0004d228; found by comparing whole streams with Apple's (a 7-channel 44.1 kHz stream, whose global_gain it changes).
    let mut first = 0i32;
    'f: for g in 0..c.ng {
        for b in 0..c.nsfbw {
            if c.flags[g * c.stride + b] & 0x20 == 0 {
                first = c.sf[g * c.nsfbw + b];
                break 'f;
            }
        }
    }
    if c.ng < 1 {
        c.gg = 4;
    } else {
        c.gg = 4 - first;
        let mut prev = 0i32;
        for g in 0..c.ng {
            for b in 0..c.nsfbw {
                let i = g * c.stride + b;
                if c.flags[i] & 0x20 == 0 {
                    let v = c.sf[i] - first;
                    let d = v - prev;
                    let mut nv = v;
                    if d < -60 {
                        nv = prev - 60;
                    }
                    if d > 60 {
                        nv = prev + 60;
                    }
                    c.sf[i] = nv;
                    prev = nv;
                }
                c.mask[i] = 1;
            }
            for b in c.nsfbw..c.stride {
                c.sf[g * c.stride + b] = 0;
            }
        }
    }
    // stage 3 (quality != 0x20)
    let mut m = -100i32;
    if c.ng >= 1 {
        let s13 = (c.gg as f32 + LN_GAIN_BIAS_POS) + M10; // two separate fadds
        for g in 0..c.ng {
            for b in 0..c.nsfbw {
                let i = g * c.stride + b;
                if c.flags[i] & 0x20 != 0 {
                    continue;
                }
                let x = c.x34[(c.start[i] + c.argmax[i]) as usize];
                let v;
                if x == 0.0 {
                    v = -100;
                } else {
                    let lg = logf(x);
                    let s0 = lg * LN_GAIN_SCALE;
                    let s1 = s13 - s0;
                    let mut w8 = fcvtps(s1);
                    let w9 = c.sf[i];
                    if w9 < w8 {
                        w8 = w9;
                    }
                    c.sf[i] = w8;
                    let s0b = s0 + LN_GAIN_BIAS; // UNFUSED
                    v = w8 + fcvtps(s0b);
                }
                if v > m {
                    m = v;
                }
            }
        }
    }
    c.m6454 = m;
    if c.gg < m + 10 {
        c.gg = m + 10;
    }
    // stage 4: clamps
    let mut gg = c.gg;
    if gg < -100 {
        gg = -100;
    } else if gg >= 0x9c {
        gg = 0x9b;
    }
    c.gg = gg;
    let lo = gg - 0x9b;
    let hi = gg + 0x5a;
    for g in 0..c.ng {
        for b in 0..c.nsfbw {
            let i = g * c.stride + b;
            if c.flags[i] & 0x20 == 0 {
                let v = c.sf[i];
                if v < lo {
                    c.sf[i] = lo;
                } else if v > hi {
                    c.sf[i] = hi;
                }
            }
        }
    }
}

// ---------------------------------------------------------------------------------------------------------------------------------
// scalefactor helpers

/// `_maxSfQuantizeZero(LC, rec i, flag, mode)`: largest sf for which the band's max line still quantises to 0 (floor, `fcvtms`).
pub fn maxsf_quantize_zero(c: &LoopChannel, i: usize, flag: u32) -> i32 {
    if !(c.energy[i] > 0.0) {
        return 1000;
    }
    let x = c.absx[(c.start[i] + c.argmax[i]) as usize];
    let f = logf(x);
    let s = if flag == 0 { f32::from_bits(0x40b8_a581) } else { f32::from_bits(0x40b8_aef5) }; // 5.770203 / 5.771357
    let t = f32::from_bits(0xc080_0061) - f * s; // -4.000046 - f*s, UNFUSED
    c.gg + fcvtms(t)
}

/// `_getMaxquantHeadroom(LC)`.
pub fn get_maxquant_headroom(c: &LoopChannel) -> i32 {
    const C1025: f32 = 1025.0;
    const Z: f32 = f32::from_bits(0x4400_0010); // 512.0009765625
    let mut res = 0x200i32;
    for g in 0..c.ng {
        for b in 0..c.nsfbw {
            let i = g * c.stride + b;
            if c.is_gap(i) {
                continue;
            }
            let xb = c.x34max(i);
            let v = if xb.to_bits() == 0 { Z } else { fma(logf(xb), LN_GAIN_SCALE, LN_GAIN_BIAS) + C1025 };
            let fx = fcvtzs(v);
            let sfv = c.sf[i];
            let t = c.gg - (fx + sfv) + 0x400;
            if t < res {
                res = t;
            }
            let t2 = c.gg - sfv + 100;
            if t2 < res {
                res = t2;
            }
        }
    }
    let t = c.gg + 100;
    if res < t { res } else { t }
}

/// `_increaseSmallScalefactors(LC, K)` (headroom exhausted inside DECREASE_GG). Returns 1 when there is more room. No fixture reaches it: ported
/// literally from the decompile.
pub fn increase_small_scalefactors(c: &mut LoopChannel, k: i32) -> i32 {
    let mut res = 0;
    let mut i16_ = 0usize;
    let p2 = c.gg - k;
    let i1 = p2 + 100;
    for g in 0..c.ng {
        for b in 0..c.nsfbw {
            let u = g * c.stride + b;
            if c.is_gap(u) {
                continue;
            }
            let xb = c.x34max(u);
            let mut i7 = if xb == 0.0 { 0x200 } else { -fcvtps(fma(logf(xb), LN_GAIN_SCALE, LN_GAIN_BIAS)) };
            i7 += p2;
            if i1 <= i7 {
                i7 = i1;
            }
            let mut i9 = c.sf[i16_];
            for j in i16_..u {
                if c.is_gap(j) {
                    continue;
                }
                let mut i13 = c.sf[j];
                let i4 = c.sf[u];
                if i13 - i4 > 0x3c {
                    i13 = i4 + 0x3c;
                    c.sf[j] = i13;
                }
                if i13 <= i9 {
                    i9 = i13;
                }
            }
            if i9 + 0x3c <= i7 {
                i7 = i9 + 0x3c;
            }
            let mut i9 = c.sf[u];
            if i9 < i7 {
                i9 += 4;
                let mut i13 = i7;
                if i9 <= i7 {
                    i13 = i9;
                }
                if i9 < i7 {
                    res = 1;
                }
                c.sf[u] = i13;
                c.mask[u] = 1;
            }
            if !(xb <= 0.0) {
                i16_ = u;
            }
        }
    }
    res
}

/// `_ForceScalefactorsToMaxScfDelta(nd, sf, mask, LC, dmap)` on the dense array (max-min <= 60 and |delta| <= 60, <= 500 rounds).
pub fn force_sf_max_delta(nd: usize, sf: &mut [i32], mask: &mut [i32], c: &LoopChannel) {
    let qz = |i: usize, flag: u32| maxsf_quantize_zero(c, c.dmap[i] as usize, flag);
    let mut it = 500i32;
    loop {
        let (mx, mn) = if nd < 1 {
            (-10000, 10000)
        } else {
            let mut mx = -10000;
            let mut mn = 10000;
            for &v in &sf[..nd] {
                if v > mx {
                    mx = v;
                }
                if v < mn {
                    mn = v;
                }
            }
            (mx, mn)
        };
        if mx - mn < 0x3d || nd < 2 {
            return;
        }
        let mut changed = false;
        let i3 = mx - 0x3c;
        let i6 = mx - ((mx - mn) - 0x3c);
        // pass 1
        for i in 0..nd - 1 {
            let mut i7 = sf[i];
            if i7 < i3 || i6 < i7 {
                let i4 = qz(i, 1);
                if i7 < i4 {
                    let mut i5 = i3;
                    if i4 <= i3 {
                        i5 = i4;
                    }
                    let mut i4b = i5;
                    if i3 <= i7 {
                        i4b = i7;
                    }
                    if i5 <= i7 {
                        i5 = i7;
                    }
                    i7 = i6;
                    if i4b <= i6 {
                        i7 = i5;
                    }
                    sf[i] = i7;
                    changed = true;
                    mask[i] = 1;
                }
            }
        }
        // pass 2
        let mut local_78 = 2usize;
        let mut local_80 = 1usize;
        for u in 0..nd - 1 {
            let i7 = sf[u];
            if i7 < i3 || i6 < i7 {
                let mut u12 = local_78;
                let mut u11 = u;
                let mut l8 = local_80;
                if u != 0 {
                    loop {
                        let u13 = u11 - 1;
                        let i4 = sf[u13];
                        let i5 = qz(u13, 0);
                        let i7 = sf[u];
                        if i4 + 0x3c < i7 {
                            sf[u] = i4 + 0x3c;
                            changed = true;
                            mask[u] = 1;
                        } else {
                            let i7 = i7 + 0x3c;
                            if i7 < i4 {
                                sf[u13] = i7;
                                changed = true;
                                mask[u13] = 1;
                            }
                        }
                        if !(1 < u11) {
                            break;
                        }
                        u11 = u13;
                        if !(i4 <= i5) {
                            break;
                        }
                    }
                }
                loop {
                    let i4 = sf[l8];
                    let i5 = qz(l8, 0);
                    let i7 = sf[u];
                    if i4 + 0x3c < i7 {
                        sf[u] = i4 + 0x3c;
                        changed = true;
                        mask[u] = 1;
                    } else {
                        let i7 = i7 + 0x3c;
                        if i7 < i4 {
                            sf[l8] = i7;
                            changed = true;
                            mask[l8] = 1;
                        }
                    }
                    if nd <= u12 {
                        break;
                    }
                    u12 += 1;
                    l8 += 1;
                    if !(i4 <= i5) {
                        break;
                    }
                }
            }
            local_80 += 1;
            local_78 += 1;
        }
        if !changed {
            return;
        }
        let done = it < 2;
        it -= 1;
        if done {
            return;
        }
    }
}

/// `_moveFirstScalefactorToGlobalGain(res, sf, &gg)`: shift so that the first coded band's scalefactor becomes 0.
/// Reads the sections of the persistent result struct (`count` at `hdr[4]`, records `cb, start, len`).
pub fn move_first_sf_to_gg(res: &BitRes, sf: &mut [i32], gg: &mut i32) -> i32 {
    let cnt = res.hdr[4].max(0) as usize;
    let mut shift = 0i32;
    for i in 0..cnt {
        let cb = res.rec[i][0];
        if (cb as u32).wrapping_sub(13) > 2 && cb != 0 {
            shift = sf[res.rec[i][1] as usize];
            break;
        }
    }
    for i in 0..cnt {
        let r = &res.rec[i];
        if (r[0] as u32).wrapping_sub(13) > 2 && r[2] > 0 {
            for j in r[1]..r[1] + r[2] {
                sf[j as usize] -= shift;
            }
        }
    }
    *gg -= shift;
    shift
}

/// `_removeScfGaps`: dense (non-gap) scalefactors into `dense`, gap-band values into `gaps` (both kept by the caller).
pub fn remove_scf_gaps(c: &LoopChannel, dense: &mut [i32], gaps: &mut [i32]) {
    let (mut di, mut gi) = (0, 0);
    for g in 0..c.ng {
        for b in 0..c.nsfbw {
            let i = g * c.stride + b;
            if c.is_gap(i) {
                gaps[gi] = c.sf[i];
                gi += 1;
            } else {
                dense[di] = c.sf[i];
                di += 1;
            }
        }
    }
}

/// `_createScfGaps`: expand `dense` (+ gap values) into the strided `g*stride+b` layout (pads between `nsfbw` and `stride` = 0).
/// Returns the number of entries written (`ng*stride`).
pub fn create_scf_gaps(c: &LoopChannel, gaps: &[i32], dense: &[i32], out: &mut [i32]) -> usize {
    let (mut gi, mut di, mut n) = (0usize, 0usize, 0usize);
    for g in 0..c.ng {
        for b in 0..c.stride {
            if b < c.nsfbw {
                let i = g * c.stride + b;
                if c.is_gap(i) {
                    out[n] = gaps[gi];
                    gi += 1;
                } else {
                    out[n] = dense[di];
                    di += 1;
                }
            } else {
                out[n] = 0;
            }
            n += 1;
        }
    }
    n
}
