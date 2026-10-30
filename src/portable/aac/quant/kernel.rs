//! `_AdvanceLoopKernel` (000f0c30) and its callees `_calcBitrateNMR` (000f2c78), `_UpdateGlobalGainNMR` (000438cc),
//! `_bitCountExceedsBufferSize` (000f2e58), `_copyLoopResult` (000f0b78).
//!
//! Literal port of `advance_loop_kernel` (variable names = Ghidra locals, so the control flow can be diffed against the listing). The engine path is
//! `BSE+0x364 == 2`, `BSE+0x408 == 0` (so every `E` branch is dead in this configuration but ported), quality 0x40, `stretch == 1.0`.
//!
//! Per call: Init -> Estimate -> candidate table (10 real quantisations + `AACBitCountFast` per channel) -> trellis -> exact pass
//! (`quantizeLines` + `AACBitCount`) -> budget/lambda steering -> element-cap restart -> `moveFirstScalefactorToGlobalGain`.

use crate::portable::aac::psy::db;
use crate::portable::aac::quant::bitcount::{aac_bit_count, aac_bit_count_fast, BitRes};
use crate::portable::aac::quant::channel::*;
use crate::portable::aac::quant::fp::*;
use crate::portable::aac::quant::prim::*;
use crate::portable::aac::quant::tables_gen::HUFF_LTAB_SCF_LOOPS;
use crate::portable::aac::types::PsyOut;

/// Columns of the candidate table (global-gain offsets 0..-9).
pub const NCOL: usize = 10;
const MAX_CH: usize = 8;

/// Static configuration of a kernel instance (`_CreateLoopKernel` inputs; engine values in [`crate::portable::aac::quant::QuantConfig`]).
#[derive(Clone, Debug)]
pub struct KernelConfig {
    /// number of coded channels (`LK+0xc0`, <= 8)
    pub nch: usize,
    /// lines per channel (`LK+0xc4`, 1024)
    pub nlines: usize,
    /// quality mode (`LK+0x338` = `H[0xe0]`): 0x40 engine. 0x20 (Fast kernel) is not part of this module.
    pub quality: u32,
    /// number of element groups (`LK+0x108`)
    pub ngroups: usize,
    /// element group of each channel (`LK+0x12c+4c`), consumed by `_bitCountExceedsBufferSize`
    pub tags: [i32; MAX_CH],
}

/// Parameters of the PAM block read by the kernel (`pam+0x190/0x194/0x198`) and `ENC+0x2c0`.
#[derive(Clone, Copy, Debug)]
pub struct PamParams {
    /// `pam+0x190` lambda = 350.0
    pub lam0: f32,
    /// `pam+0x194` (only used/updated when `BSE+0x408 > 0`)
    pub lam_state: f32,
    /// `pam+0x198` NMR scale = 6.0
    pub nmrscale: f32,
    /// `ENC+0x2c0` stretch ratio (1.0; another value is not an engine configuration and is unused)
    pub stretch: f32,
}

impl Default for PamParams {
    fn default() -> Self {
        Self { lam0: 350.0, lam_state: 350.0, nmrscale: 6.0, stretch: 1.0 }
    }
}

/// Budget fields of the bitstream-encoder state (`BSE`) that the kernel reads.
#[derive(Clone, Copy, Debug)]
pub struct KernelBudget {
    /// `BSE+0x364` internal rate-control mode: 2 for `acbf=1` (engine); 1..3 select the statistics budget branch
    pub mode: i32,
    /// `BSE+0x3c0` side-information bits of this AU
    pub side_bits: i32,
    /// `BSE+0x400` / `+0x404` (CBR budgets, acbf=0 only)
    pub cbr_max: i32,
    pub cbr_min: i32,
    /// `BSE+0x408` (0 for the engine: kills every ABR-state branch)
    pub e408: i32,
    /// `BSE+0x444` MAX (100000 default) / `+0x448` MIN (0 default) / `+0x44c` bit limit (1,000,000)
    pub max_bits: i32,
    pub min_bits: i32,
    pub limit_bits: i32,
}

impl KernelBudget {
    /// Engine defaults before AU 216 (`BSE+0x444=100000`, `0x448=0`, `0x44c=1000000`, mode 2, `0x408=0`).
    pub fn engine(side_bits: i32, max_bits: i32, min_bits: i32) -> Self {
        Self { mode: 2, side_bits, cbr_max: 0, cbr_min: 0, e408: 0, max_bits, min_bits, limit_bits: 1_000_000 }
    }
}

/// What the kernel reports besides the per-channel state left in [`LoopChannel`].
#[derive(Clone, Debug, Default)]
pub struct KernelOut {
    /// `LK+0x2d0[c]`: bits counted by the last pass of each channel.
    pub bits: [i32; MAX_CH],
    /// `BSE+0x40c`: total of the FIRST exact pass (None when the kernel never recorded one).
    pub first_pass_total: Option<i32>,
    /// `pam+0x194` after the call (only meaningful when `e408 > 0`).
    pub lam_state: f32,
}

/// `_UpdateGlobalGainNMR(LC, pow43)`: quantise at `gg` with the current `x34s`, then evaluate the per-band error.
pub fn update_global_gain_nmr(c: &mut LoopChannel, pow43: &[f32]) {
    quantize_lines(c.gg, c.nlines, &c.x34s, &mut c.q);
    inv_quantize_and_evaluate(
        &c.absx, &c.q, pow43, &c.start, &c.width, &c.invthr, c.gg, &c.sf, c.nsfbw, c.stride, c.ng, &mut c.err,
    );
}

/// `AACBitCountFast` on the channel's current `q`/`sf` into its persistent result struct.
fn bit_count_fast_ch(c: &mut LoopChannel) {
    let sfb = crate::portable::aac::quant::bitcount::Sfb { start: &c.start, width: &c.width, maxpos: &c.argmax };
    let ipos = if c.has_ms { Some(&c.ipos[..]) } else { None };
    aac_bit_count_fast(&c.q, &c.sf, ipos, &c.flags, c.win, c.maxsfb as i32, c.nsfbw as i32, c.ng as i32, &sfb, &mut c.res);
}

/// `local_340`-style array of `bits` of the Fast records `i < ng*stride` (1000 beyond).
fn fast_bits(c: &LoopChannel, out: &mut [i32; NB]) {
    let n = c.ng * c.stride;
    for i in 0..NB {
        out[i] = 1000;
    }
    for i in 0..n.min(NB) {
        out[i] = c.res.rec[i][4];
    }
}

/// `_calcBitrateNMR(LK, LC, out_nmrdB, out_bits)`.
pub fn calc_bitrate_nmr(c: &mut LoopChannel, pow43: &[f32], nmr_db: &mut [f32; NB], bits: &mut [i32; NB]) {
    update_global_gain_nmr(c, pow43);
    *nmr_db = [0.0; NB];
    for i in 0..c.nsfb_tot {
        nmr_db[i] = db(c.err[i]);
    }
    bit_count_fast_ch(c);
    fast_bits(c, bits);
}

/// `_bitCountExceedsBufferSize`: returns `Some((order, n_active, new_max))` when an element exceeds `n_ch*0x1599` bits.
fn bit_count_exceeds_buffer_size(cfg: &KernelConfig, bits: &[i32; MAX_CH]) -> Option<([usize; MAX_CH], usize, i32)> {
    for grp in 0..cfg.ngroups {
        let mut sel = [0usize; MAX_CH];
        let mut n = 0;
        let mut tot = 0i32;
        for c in 0..cfg.nch {
            if cfg.tags[c] == grp as i32 {
                tot = tot.wrapping_add(bits[c]);
                sel[n] = c;
                n += 1;
            }
        }
        if (n as i32) * 0x1599 < tot {
            let mut order = [0usize; MAX_CH];
            order[0] = sel[0];
            if n == 2 {
                order[1] = sel[1];
            }
            return Some((order, n, n as i32 * 0x1599));
        }
    }
    None
}

/// Persistent loop-kernel state: one [`LoopChannel`] per channel plus the candidate-table scratch.
pub struct LoopKernel {
    pub cfg: KernelConfig,
    pub ch: Vec<Box<LoopChannel>>,
    pow43: &'static [f32],
    nmr_tab: Vec<Box<[[f32; NCOL]; NB]>>,
    bits_tab: Vec<Box<[[i32; NCOL]; NB]>>,
    back_tab: Vec<Box<[[i8; NCOL]; NB]>>,
}

#[inline]
fn i32w(x: i64) -> i32 {
    x as i32
}

impl LoopKernel {
    pub fn new(cfg: KernelConfig) -> Self {
        debug_assert!(cfg.nch >= 1 && cfg.nch <= MAX_CH);
        let ch = (0..cfg.nch).map(|_| LoopChannel::new()).collect();
        let nmr_tab = (0..cfg.nch).map(|_| Box::new([[0.0f32; NCOL]; NB])).collect();
        let bits_tab = (0..cfg.nch).map(|_| Box::new([[0i32; NCOL]; NB])).collect();
        let back_tab = (0..cfg.nch).map(|_| Box::new([[0i8; NCOL]; NB])).collect();
        let pow43: &'static [f32] = &POW43;
        Self { cfg, ch, pow43, nmr_tab, bits_tab, back_tab }
    }

    fn calc_nmr(&mut self, c: usize, nm: &mut [f32; NB], bt: &mut [i32; NB]) {
        calc_bitrate_nmr(&mut self.ch[c], self.pow43, nm, bt);
    }

    fn fill_col(&mut self, c: usize, k: usize, nmrscale: f32, nmr_db: &[f32; NB], bt: &[i32; NB]) {
        let nd = self.ch[c].nd;
        for i in 0..nd {
            let sfb = self.ch[c].dmap[i] as usize;
            let mut v = nmrscale * nmr_db[sfb];
            if nmr_db[sfb] < 0.0 {
                v = v / (1.0f32 - v);
            }
            self.nmr_tab[c][i][k] = v;
            self.bits_tab[c][i][k] = bt[sfb];
        }
    }

    /// `_AdvanceLoopKernel(LK, psyArr, pam, BSE, ENC)`. `psy[c]` is the final psy-out of channel `c` (after M/S and intensity).
    /// On return every channel holds `q`/`iq`, `sf` (first coded sf = 0), `gg` (bitstream `global_gain = gg + 100`) and the
    /// section struct `res`; `out.bits` the per-channel bit counts.
    pub fn advance_loop_kernel(&mut self, psy: &[&PsyOut], pam: &PamParams, bse: &KernelBudget) -> KernelOut {
        let nch = self.cfg.nch;
        assert!(psy.len() >= nch);
        let mode = self.cfg.quality;
        let fvar74 = pam.lam0;
        let fvar76 = pam.nmrscale;
        let ivar9 = bse.e408;
        let nlines = self.cfg.nlines;
        let mut out = KernelOut::default();
        let fvar77: f32 = if pam.stretch == 1.0 || ivar9 < 1 { f32::from_bits(0x3f88_f5c3) } else { f32::from_bits(0x3f91_2d77) };
        // ---- per channel init + estimate
        for c in 0..nch {
            self.ch[c].init(psy[c], nlines, mode);
            self.ch[c].gg_max = 0x9b;
        }
        for c in 0..nch {
            estimate_scalefactors_pam(&mut self.ch[c]);
        }
        for c in 0..nch {
            let ch = &mut self.ch[c];
            let x34 = ch.x34;
            let (stride, ng, nsfbw) = (ch.stride, ch.ng, ch.nsfbw);
            calculate_scale_spectrum(stride, ng, nsfbw, &ch.start, &ch.width, &mut ch.x34s, &x34, &ch.sf, &ch.mask);
            update_global_gain_nmr(ch, self.pow43);
        }
        let mut nb = [0usize; MAX_CH];
        let mut bits = [0i32; MAX_CH];
        let mut can_dec = [1i32; MAX_CH];
        let mut can_inc = [1i32; MAX_CH];
        let mut hh = [0i32; MAX_CH];
        let mut kc = [0usize; MAX_CH];
        let mut nd = [0usize; MAX_CH];
        let mut bvar13 = nch > 0;
        let mut fvar70 = fvar74;
        if ivar9 > 0 {
            fvar70 = pam.lam_state;
        }
        let mut local_2118: i32;
        let mut local_21bc: i32;
        let uvar50: u32;
        if bse.mode >= 1 && bse.mode <= 3 {
            local_2118 = bse.max_bits;
            if (nch as i32) * 0x1800 <= bse.max_bits {
                local_2118 = nch as i32 * 0x1800;
            }
            local_21bc = bse.min_bits.max(0);
            uvar50 = 8;
        } else {
            local_2118 = bse.cbr_max;
            local_21bc = bse.cbr_min;
            uvar50 = 0x18;
        }
        let ivar65b = bse.limit_bits - bse.side_bits;
        if ivar65b <= local_2118 {
            local_2118 = ivar65b;
        }
        if ivar9 > 0 && local_21bc <= fcvtzs(local_2118 as f32 * 0.9f32) {
            local_21bc = fcvtzs(local_2118 as f32 * 0.9f32);
        }
        let mut local_216c: i32;
        let mut local_21d4: i32 = -1;
        // ---- candidate tables
        let mut nm = [0.0f32; NB];
        let mut bt = [0i32; NB];
        if bvar13 {
            bvar13 = false;
            for c in 0..nch {
                nb[c] = self.ch[c].ng * self.ch[c].nsfbw;
                nd[c] = self.ch[c].nd;
                self.nmr_tab[c].iter_mut().for_each(|r| *r = [0.0; NCOL]);
                self.bits_tab[c].iter_mut().for_each(|r| *r = [0; NCOL]);
                self.back_tab[c].iter_mut().for_each(|r| *r = [0; NCOL]);
                if nb[c] == 0 {
                    bits[c] = 0;
                    continue;
                }
                // local_340 = AACBitCountFast of the Phase-0 quantisation
                bit_count_fast_ch(&mut self.ch[c]);
                let mut local_340 = [0i32; NB];
                fast_bits(&self.ch[c], &mut local_340);
                let mut local_790 = [0.0f32; NB];
                for i in 0..self.ch[c].nsfb_tot {
                    local_790[i] = db(self.ch[c].err[i]);
                }
                hh[c] = get_maxquant_headroom(&self.ch[c]);
                if hh[c] < 0xb {
                    self.ch[c].gg = (self.ch[c].gg - hh[c]) + 10;
                    hh[c] = 10;
                    can_dec[c] = 0;
                }
                self.fill_col(c, 0, fvar76, &local_790, &local_340);
                bvar13 = true;
                local_21d4 = self.ch[c].gg;
                kc[c] = NCOL;
                for k in 1..NCOL {
                    self.ch[c].gg = local_21d4 - k as i32;
                    self.calc_nmr(c, &mut nm, &mut bt);
                    self.fill_col(c, k, fvar76, &nm, &bt);
                }
                self.ch[c].gg = local_21d4;
            }
            local_216c = local_21d4;
        } else {
            local_216c = -1;
        }
        let ivar65: i32 = local_216c;
        // ---- constants (f32)
        let fvar28 = f32::from_bits(0x3f82_8f5c); // 1.02
        let fvar27 = f32::from_bits(0x47c3_5040); // 100000.5
        let fvar26 = f32::from_bits(0x3f8c_cccd); // 1.1
        let fvar25 = f32::from_bits(0x3e99_999a); // 0.3
        let fvar24 = f32::from_bits(0x3ecc_cccd); // 0.4
        let fvar23 = f32::from_bits(0x3f73_3333); // 0.95
        let fvar22 = f32::from_bits(0x3f19_999a); // 0.6
        let fvar75 = f32::from_bits(0x3f33_3333); // 0.7
        let fvar21 = f32::from_bits(0x3fa6_6666); // 1.3
        let mut s: u32 = 0x1100;
        let mut bvar19 = true;
        let mut order = [0usize; MAX_CH];
        for c in 0..nch {
            order[c] = c;
        }
        let mut local_21e4: i32 = 0;
        let mut local_21e0: i32 = 0;
        let bvar3 = ivar9 > 0;
        let mut nactive = nch;
        let mut best = [0usize; MAX_CH];
        let mut local_2154: i32;
        let mut local_211c: i32;
        // stack scratch (0xaa / 0xff initialised once per call, never cleared between channels/passes)
        const AA: i32 = 0xaaaa_aaaau32 as i32;
        let mut l970 = [AA; NB];
        let mut ld30 = [AA; NB];
        let mut lb50 = [AA; NB];
        let mut tmp = [f32::from_bits(0xffff_ffff); 1024];
        let mut full_sf = [0i32; NB];
        let mut full_mask = [0i32; NB];
        let mut had_exact = [false; MAX_CH];

        // local state of the control flow
        let mut local_21b0: i32;
        let mut bvar17: bool;
        let mut bvar20: bool;
        let mut bvar15: bool;
        let mut local_21d0: i32;
        let mut uvar55: usize;
        let mut ivar52: i32;
        let mut ivar66: i32;
        let mut bvar2: bool;
        let mut ivar51: i32;
        let mut ivar42: i32;
        let mut fvar78: f32;
        let mut bvar14: bool;
        let mut local_21b4: i32;
        let mut ivar43: i32;
        let mut bvar18: bool;
        let mut bvar16: bool;

        loop {
            // ================= LAB_000f16e4 (retry) =================
            local_21b0 = 0;
            s &= 0xffef_3781;
            bvar17 = false;
            bvar20 = false;
            bvar15 = false;
            local_21d0 = 0;
            local_2154 = 0;
            'outer: loop {
                // ---------- LAB_000f1710 ----------
                uvar55 = nactive;
                ivar52 = local_2118;
                ivar66 = 0;
                s &= 0xffff_fbaf;
                bvar2 = false;
                ivar51 = fcvtzs(local_2118 as f32 * fvar26);
                ivar42 = fcvtzs(local_2118 as f32 * fvar21);
                fvar78 = fvar24;
                let mut go_2438;
                'lambda: loop {
                    // ---------- LAB_000f1750 ----------
                    bvar14 = false;
                    local_21b4 = ivar66;
                    ivar43 = local_2154;
                    go_2438 = false;
                    let mut goto_1750 = false;
                    'pass: loop {
                        bvar18 = true;
                        bvar16 = true;
                        local_2154 = 0;
                        for uvar63 in 0..uvar55 {
                            let lvar36 = order[uvar63];
                            let uvar45 = nb[lvar36];
                            if uvar45 == 0 {
                                bits[lvar36] = 0;
                                self.ch[lvar36].res = BitRes::new(); // bzero(LC+0x6be8, 0x2798)
                                had_exact[lvar36] = false;
                                continue;
                            }
                            let uvar33 = nd[lvar36];
                            // _removeScfGaps
                            remove_scf_gaps(&self.ch[lvar36], &mut l970, &mut ld30);
                            if !bvar14 {
                                // ---- trellis over the candidate table
                                let uvar10 = kc[lvar36];
                                let tabn = &self.nmr_tab[lvar36];
                                let tabb = &self.bits_tab[lvar36];
                                let tabk = &mut self.back_tab[lvar36];
                                let mut prev = [0i32; NCOL];
                                if uvar10 > 0 {
                                    let fvar71 = tabn[0][0];
                                    let ivar66_ = tabb[0][0];
                                    for k in 0..uvar10 {
                                        prev[k] = i32w(
                                            ((tabb[0][k] as i64 + 100000) - ivar66_ as i64) - fcvtzs(fma(fvar70, fvar71 - tabn[0][k], fvar27)) as i64,
                                        );
                                    }
                                }
                                let mut cur = prev;
                                if uvar33 >= 2 {
                                    let mut ivar66_ = l970[0];
                                    for uvar59 in 1..uvar33 {
                                        let ivar44 = l970[uvar59];
                                        cur = [0i32; NCOL];
                                        if uvar10 > 0 {
                                            let fvar72 = tabn[uvar59][0];
                                            let ivar71 = tabb[uvar59][0];
                                            ivar66_ = (ivar66_ - ivar44) + 0x48;
                                            for c_ in 0..uvar10 {
                                                let mut ivar39 = 0x3000_0000i32;
                                                let mut ivar38 = 0i32;
                                                for p_ in 0..uvar10 {
                                                    let idx = ivar66_ as i64 + p_ as i64;
                                                    let lt = HUFF_LTAB_SCF_LOOPS.get(idx as usize).copied().unwrap_or(0x4000_0000);
                                                    let ivar32 = i32w(lt as i64 + prev[p_] as i64);
                                                    let ivar6 = if ivar32 <= ivar39 { ivar32 } else { ivar39 };
                                                    let ivar7 = if ivar39 <= ivar32 { ivar38 } else { p_ as i32 };
                                                    ivar39 = ivar6;
                                                    ivar38 = ivar7;
                                                }
                                                cur[c_] = i32w(
                                                    ((tabb[uvar59][c_] as i64 + 100000) - ivar71 as i64)
                                                        - fcvtzs(fma(fvar70, fvar72 - tabn[uvar59][c_], fvar27)) as i64
                                                        + ivar39 as i64,
                                                );
                                                tabk[uvar59][c_] = ivar38 as i8;
                                                ivar66_ -= 1;
                                            }
                                        }
                                        prev = cur;
                                        ivar66_ = ivar44;
                                    }
                                }
                                best[lvar36] = 0;
                                let mut m = 0x7fff_ffffi32;
                                for u in 0..uvar10 {
                                    if cur[u] < m {
                                        best[lvar36] = u;
                                        m = cur[u];
                                    }
                                }
                            }
                            // -------------------------------------------------------------------------------------------
                            if (s & 0x1100) == 0x1000 {
                                // ---- estimate-only pass (no quantisation)
                                let ivar66b = best[lvar36] as i32;
                                let mut skip = false;
                                let mut ivar44: usize;
                                if ivar66b == 0 {
                                    ivar44 = 0;
                                    local_211c = 1;
                                } else {
                                    ivar44 = ivar66b as usize;
                                    if 8 < ivar66b {
                                        ivar44 = 9;
                                    }
                                    if 9 < ivar66b && 0 < ivar9 {
                                        bvar2 = true;
                                        skip = true;
                                    }
                                    local_211c = 0;
                                }
                                if skip {
                                    continue;
                                }
                                let tabb = &self.bits_tab[lvar36];
                                let tabk = &self.back_tab[lvar36];
                                let ivar66c: i32;
                                if uvar33 < 1 {
                                    ivar66c = 0;
                                } else {
                                    let mut ivar39 = 0i32;
                                    if uvar33 != 1 {
                                        ivar39 = 0;
                                        // quirk: the start value reads sf_dense[nb-1] (nb = ng*nsfbw, not nd)
                                        let mut ivar66_ = l970[uvar45 - 1] + ivar44 as i32;
                                        let mut uvar49 = uvar33 + 1;
                                        let mut uvar69 = uvar33;
                                        loop {
                                            let lvar46 = uvar69 - 1;
                                            ivar39 = tabb[lvar46][ivar44] + ivar39;
                                            let cvar12 = tabk[lvar46][ivar44] as i32;
                                            let skip2;
                                            if cvar12 == 0 {
                                                ivar44 = 0;
                                                local_211c += 1;
                                                skip2 = true;
                                            } else {
                                                ivar44 = cvar12 as usize;
                                                if 8 < cvar12 {
                                                    ivar44 = 9;
                                                }
                                                if cvar12 < 10 || ivar9 < 1 {
                                                    skip2 = true;
                                                } else {
                                                    bvar2 = true;
                                                    ivar44 = 9;
                                                    skip2 = false;
                                                }
                                            }
                                            if skip2 {
                                                let mut ivar38 = l970[uvar69 - 2] + ivar44 as i32;
                                                if ivar38 <= ivar66_ - 0x3c {
                                                    ivar38 = ivar66_ - 0x3c;
                                                }
                                                if ivar66_ + 0x3c <= ivar38 {
                                                    ivar38 = ivar66_ + 0x3c;
                                                }
                                                ivar39 = HUFF_LTAB_SCF_LOOPS[12 + ((ivar38 - ivar66_) + 0x3c) as usize] + ivar39;
                                                ivar66_ = ivar38;
                                            }
                                            uvar49 -= 1;
                                            uvar69 -= 1;
                                            if !(2 < uvar49) {
                                                break;
                                            }
                                        }
                                    }
                                    ivar66c = fcvtzs(fvar77 * ((tabb[0][ivar44] + ivar39) as f32));
                                }
                                ivar66 = ivar66c;
                                bits[lvar36] = ivar66c;
                                bvar18 = false;
                            } else {
                                // ---- exact pass
                                local_211c = 0;
                                let mut chosen = [0usize; NB];
                                if uvar33 > 0 {
                                    let mut uvar49 = uvar33 + 1;
                                    let mut ivar66b = best[lvar36] as i32;
                                    let mut uvar59 = uvar33;
                                    loop {
                                        uvar59 -= 1;
                                        let ivar44: i32;
                                        let ok;
                                        if ivar66b == 0 {
                                            ivar44 = 0;
                                            local_211c += 1;
                                            ok = true;
                                        } else {
                                            let mut i44 = ivar66b;
                                            if 8 < ivar66b {
                                                i44 = 9;
                                            }
                                            ivar44 = i44;
                                            if ivar66b < 10 || ivar9 < 1 {
                                                ok = true;
                                            } else {
                                                bvar2 = true;
                                                ivar66b = 9;
                                                ok = false;
                                            }
                                        }
                                        if ok {
                                            l970[uvar59] += ivar44;
                                            lb50[uvar59] = 1;
                                            chosen[uvar59] = ivar44 as usize;
                                            ivar66b = self.back_tab[lvar36][uvar59][ivar44 as usize] as i32;
                                        }
                                        uvar49 -= 1;
                                        if !(1 < uvar49) {
                                            break;
                                        }
                                    }
                                }
                                {
                                    let ch = &mut self.ch[lvar36];
                                    let n = ch.ng * ch.stride;
                                    for m in ch.mask[..n].iter_mut() {
                                        *m = 1;
                                    }
                                }
                                let ch = &self.ch[lvar36];
                                let i_var39 = ch.nsfbw;
                                let tabb = &self.bits_tab[lvar36];
                                if i_var39 > 0 && uvar33 > 0 {
                                    let mut first: Option<usize> = None;
                                    for u in 0..uvar33 {
                                        if tabb[u][chosen[u]] >= 1 {
                                            first = Some(u);
                                            break;
                                        }
                                    }
                                    if let Some(f) = first {
                                        if f != 0 {
                                            let mut ivar38 = l970[f];
                                            let mut uvar49_ = f as i64;
                                            let mut first_it = true;
                                            for _itn in 0..f + 1 {
                                                if !first_it {
                                                    let mut ivar32 = maxsf_quantize_zero(ch, ch.dmap[uvar49_ as usize] as usize, 1);
                                                    if ivar38 <= ivar32 {
                                                        ivar32 = ivar38;
                                                    }
                                                    ivar38 = l970[uvar49_ as usize];
                                                    if ivar38 < ivar32 {
                                                        l970[uvar49_ as usize] = ivar32;
                                                        lb50[uvar49_ as usize] = 1;
                                                        ivar38 = ivar32;
                                                    }
                                                }
                                                first_it = false;
                                                uvar49_ -= 1;
                                            }
                                        }
                                        let mut uvar59_ = l970[0];
                                        for lv in 0..uvar33 {
                                            if tabb[lv][chosen[lv]] == 0 {
                                                let mut q_ = maxsf_quantize_zero(ch, ch.dmap[lv] as usize, 1);
                                                if uvar59_ <= q_ {
                                                    q_ = uvar59_;
                                                }
                                                let uvar10_ = l970[lv];
                                                uvar59_ = uvar10_;
                                                if uvar10_ < q_ {
                                                    l970[lv] = q_;
                                                    lb50[lv] = 1;
                                                    uvar59_ = q_;
                                                }
                                            } else {
                                                uvar59_ = l970[lv];
                                            }
                                        }
                                    }
                                }
                                force_sf_max_delta(uvar33, &mut l970, &mut lb50, ch);
                                let nfull = create_scf_gaps(ch, &ld30, &l970, &mut full_sf);
                                create_scf_gaps(ch, &ld30, &lb50, &mut full_mask);
                                l970[..nfull].copy_from_slice(&full_sf[..nfull]);
                                lb50[..nfull].copy_from_slice(&full_mask[..nfull]);
                                let ch = &mut self.ch[lvar36];
                                // calculateScaleSpectrum into the 4 KB temp buffer (stale bands survive)
                                let x34 = ch.x34;
                                calculate_scale_spectrum(ch.stride, ch.ng, ch.nsfbw, &ch.start, &ch.width, &mut tmp, &x34, &full_sf, &full_mask);
                                quantize_lines(ch.gg, ch.nlines, &tmp, &mut ch.q);
                                for i in 0..ch.nlines {
                                    ch.iq[i] = ch.q[i] * ch.sign[i] as i32;
                                }
                                let ipos = if ch.has_ms { Some(&ch.ipos[..]) } else { None };
                                let idir = if ch.has_ms { Some(&ch.idir[..]) } else { None };
                                let sfb = crate::portable::aac::quant::bitcount::Sfb { start: &ch.start, width: &ch.width, maxpos: &ch.argmax };
                                let bits_ex = aac_bit_count(
                                    &ch.iq, &full_sf, ipos, idir, &ch.flags, None, ch.win, ch.maxsfb as i32, ch.nsfbw as i32, ch.ng as i32, &sfb,
                                    &mut ch.res,
                                );
                                had_exact[lvar36] = true;
                                ivar66 = bits_ex;
                                bits[lvar36] = bits_ex;
                                let n = ch.ng * ch.stride;
                                ch.sf6460[..n].copy_from_slice(&full_sf[..n]);
                            }
                            local_2154 = ivar66.wrapping_add(local_2154);
                            if (local_211c as f32) <= (uvar45 as f32) * fvar75 {
                                bvar16 = false;
                            }
                        }
                        // -------- after the per-channel loop --------
                        if !bvar13 {
                            go_2438 = true;
                            break 'pass;
                        }
                        if ((s >> 8) & 1) == 0 {
                            if bvar2 {
                                go_2438 = true;
                                break 'pass;
                            }
                        } else {
                            out.first_pass_total = Some(local_2154);
                        }
                        let ivar66n;
                        if local_2154 < fcvtzs(ivar43 as f32 * fvar28) {
                            let mut v = 0;
                            if (!bvar14) && ivar43 < fcvtzs(local_2154 as f32 * fvar28) {
                                v = local_21b0 + 1;
                            }
                            ivar66n = v;
                        } else {
                            ivar66n = 0;
                        }
                        ivar66 = ivar66n;
                        local_21b0 = ivar66;
                        bvar14 = bvar17;
                        if ivar52 < local_2154 {
                            // ---- over budget
                            let mut goto_22a8 = false;
                            if bvar16 {
                                goto_22a8 = true;
                            } else if 0 < ivar66 {
                                if 0 < ivar9 && local_2154 <= ivar51 {
                                    if local_2154 == ivar43 {
                                        goto_22a8 = true;
                                    } else if bvar20 || ivar42 < local_2154 {
                                        goto_22a8 = true;
                                    }
                                } else {
                                    goto_22a8 = true;
                                }
                            } else {
                                if 0 < ivar9 && local_2154 == ivar43 {
                                    goto_22a8 = true;
                                }
                                if !goto_22a8 && (bvar20 || ivar42 < local_2154) {
                                    goto_22a8 = true;
                                }
                            }
                            if goto_22a8 {
                                local_21b4 = 0x31;
                            }
                            ivar66 = 0;
                            if local_21e0 < 1 || bvar17 {
                                ivar66 = local_21e0;
                            }
                            let mut uvar45: u32 = 0xc004;
                            let mut ivar43b: i32 = 0x31;
                            bvar14 = true;
                            bvar15 = true;
                            if local_21e0 < 1 || bvar17 {
                                uvar45 = (s & 0xffff_fbf1) | ((s & 2) << 3) | 4;
                                ivar43b = local_21b4;
                                bvar14 = bvar17;
                                bvar15 = bvar20;
                            }
                            bvar20 = bvar15;
                            bvar15 = true;
                            local_21e0 = ivar66;
                            ivar43 = ivar43b;
                            if 0x30 < ivar43 {
                                s = uvar45 & 0xffff_feff;
                                ivar43 = local_2154;
                                bvar17 = bvar14;
                                go_2438 = true;
                                break 'pass;
                            }
                            if fvar22 <= fvar78 || ((uvar45 >> 4) & 1) == 0 {
                                local_21b4 = ivar43;
                                if (uvar45 & 0x10) != 0 && (fvar22 < fvar78 && bvar3) {
                                    local_21b4 = 0x31;
                                }
                            } else {
                                fvar78 = fvar78.sqrt();
                            }
                            fvar70 *= fvar78;
                            // LAB_000f2410
                            s = uvar45 & 0xffff_feff;
                            ivar66 = local_21b4 + 1;
                            bvar17 = bvar14;
                            ivar43 = local_2154;
                            if 0x30 < local_21b4 {
                                go_2438 = true;
                                break 'pass;
                            }
                            goto_1750 = true;
                            break 'pass;
                        }
                        // ---- within budget
                        let uvar33_ = (s & 4) << 2;
                        let cond = (local_2154 < local_21bc)
                            && ((ivar9 < 1 || (!bvar15 && (s & 0x10) == 0)) || local_2154 <= fcvtzs(local_21bc as f32 * fvar23));
                        if !cond {
                            if (s & 0x1100) != 0x1000 {
                                s = (s & 0xffff_faf5) | 2;
                                go_2438 = true;
                                break 'pass;
                            }
                            s = uvar33_ | (s & 0xffff_eaf1);
                            bvar15 = false;
                            bvar14 = true;
                            local_21b4 = 0x1f;
                            ivar43 = local_2154;
                            continue 'pass;
                        }
                        // LAB_000f2220 (under MIN)
                        let uvar33_ = uvar33_ | s;
                        let mut uvar45: u32 = uvar33_ | 2;
                        let mut bvar41 = false;
                        if uvar55 > 0 {
                            let mut bvar16_ = false;
                            for ii in 0..uvar55 {
                                let lv = order[ii];
                                let mut bv = bvar16_;
                                if can_dec[lv] == 0 {
                                    bv = true;
                                    if can_inc[lv] != 0 {
                                        bv = bvar16_;
                                    }
                                }
                                bvar16_ = bv;
                            }
                            bvar41 = bvar16_;
                        }
                        if !bvar41 {
                            if local_21e4 == 0 {
                                local_21e4 = 0;
                                uvar45 = uvar33_ | 10;
                            } else if !bvar17 {
                                local_21e4 = 0;
                                uvar45 = (s & 0xffff_23e9) | 0xc004;
                                bvar20 = true;
                                bvar15 = true;
                                local_21b4 = 0x31;
                                bvar14 = true;
                            }
                        }
                        // LAB_000f2390
                        if ivar66 < 2 {
                            if local_21bc <= fcvtzs(local_2154 as f32 * 1.5f32) && !bvar20 && local_21b4 < 0x31 {
                                if fvar78 < fvar22 && (uvar45 & 0x10) != 0 {
                                    fvar78 = fvar78.sqrt();
                                } else if (uvar45 & 0x10) != 0 && (fvar22 < fvar78 && bvar3) {
                                    local_21b4 = 0x31;
                                }
                                fvar70 /= fvar78;
                                // LAB_000f2410
                                s = uvar45 & 0xffff_feff;
                                ivar66 = local_21b4 + 1;
                                bvar17 = bvar14;
                                ivar43 = local_2154;
                                if 0x30 < local_21b4 {
                                    go_2438 = true;
                                    break 'pass;
                                }
                                goto_1750 = true;
                                break 'pass;
                            }
                        } else {
                            local_21b0 = 2;
                            if ivar66 != 2 {
                                local_21b0 = ivar66;
                                uvar45 = (uvar45 & 0xffff_efff) | 0x4000;
                                bvar14 = true;
                            }
                        }
                        // LAB_000f2b10
                        s = uvar45 & 0xffff_feff;
                        ivar43 = local_2154;
                        bvar17 = bvar14;
                        go_2438 = true;
                        break 'pass;
                    }
                    if goto_1750 && !go_2438 {
                        continue 'lambda;
                    }
                    break 'lambda;
                }
                // ============ joined_r0x000f2438 ============
                let mut done = false;
                if !bvar13 {
                    done = true;
                } else {
                    if (uvar50 as i32 <= local_21d0) || (0x18 < local_216c - ivar65) {
                        if (((s >> 1) & 1) != 0 && bvar18) || (0x3b < local_21d0) {
                            done = true;
                        } else {
                            s = (s & 0xffef_afff) | 0x4000;
                            bvar17 = true;
                        }
                    }
                    if !done {
                        if ((s >> 1) & 1) == 0 {
                            // ---- INCREASE_GG (coarser)
                            s = ((!(s >> 2)) & 0x1000) | s;
                            local_21e4 += 1;
                            fvar70 = fvar74;
                            if uvar55 > 0 {
                                fvar78 = 0.5;
                                if local_2154 <= ivar42 {
                                    fvar78 = fvar25;
                                }
                                for uvar63 in 0..uvar55 {
                                    let lvar36 = order[uvar63];
                                    if nb[lvar36] > 0 {
                                        let mut ivar66b: i32;
                                        if bvar20 {
                                            ivar66b = kc[lvar36] as i32;
                                            if 0 < ivar66b {
                                                ivar66b = 1;
                                            }
                                        } else if (local_2154 <= ivar42 && bvar3) && local_2154 < ivar51 {
                                            ivar66b = 1;
                                        } else {
                                            let ivar52_ = kc[lvar36] as i32;
                                            ivar66b = fcvtzs(fvar78 * (ivar52_ as f32));
                                            if ivar66b < 1 && 0 < ivar52_ {
                                                ivar66b = 1;
                                            }
                                        }
                                        local_216c = self.ch[lvar36].gg + ivar66b;
                                        self.ch[lvar36].gg = local_216c;
                                        let ivar52_ = hh[lvar36] + ivar66b;
                                        hh[lvar36] = ivar52_;
                                        let mut ivar44 = ivar52_;
                                        if 9 < ivar52_ {
                                            ivar44 = 10;
                                        }
                                        kc[lvar36] = ivar44 as usize;
                                        can_dec[lvar36] = if 10 < ivar52_ { 1 } else { 0 };
                                        let uvar55_: i32;
                                        if ivar66b < ivar44 {
                                            let mut u = ivar44 - 1;
                                            while u >= ivar66b {
                                                for i in 0..nd[lvar36] {
                                                    self.nmr_tab[lvar36][i][u as usize] = self.nmr_tab[lvar36][i][(u - ivar66b) as usize];
                                                    self.bits_tab[lvar36][i][u as usize] = self.bits_tab[lvar36][i][(u - ivar66b) as usize];
                                                }
                                                u -= 1;
                                            }
                                            uvar55_ = ivar66b - 1;
                                        } else {
                                            uvar55_ = ivar44 - 1;
                                        }
                                        let mut u = uvar55_;
                                        while u >= 0 {
                                            self.ch[lvar36].gg = local_216c - u;
                                            self.calc_nmr(lvar36, &mut nm, &mut bt);
                                            self.fill_col(lvar36, u as usize, fvar76, &nm, &bt);
                                            u -= 1;
                                        }
                                        self.ch[lvar36].gg = local_216c;
                                    }
                                    fvar70 = fvar74;
                                }
                            }
                        } else if ((s >> 3) & 1) != 0 {
                            // ---- DECREASE_GG (finer)
                            s = ((!(s >> 2)) & 0x1000) | s;
                            local_21e0 += 1;
                            fvar70 = fvar74;
                            if uvar55 > 0 {
                                let ivar66b: i32 = if bvar20 { 1 } else { 6 };
                                for uvar63 in 0..uvar55 {
                                    let lvar36 = order[uvar63];
                                    if nb[lvar36] > 0 {
                                        let mut ivar52_ = hh[lvar36];
                                        let uvar55_ = kc[lvar36] as i32;
                                        let do_shift;
                                        let shift: i32;
                                        if uvar55_ + ivar66b < ivar52_ {
                                            hh[lvar36] = ivar52_ - ivar66b;
                                            local_216c = self.ch[lvar36].gg - ivar66b;
                                            self.ch[lvar36].gg = local_216c;
                                            ivar52_ = ivar66b;
                                            do_shift = true;
                                            shift = ivar52_;
                                        } else {
                                            can_dec[lvar36] = 0;
                                            hh[lvar36] = uvar55_;
                                            ivar52_ = ivar52_ - uvar55_;
                                            local_216c = self.ch[lvar36].gg - ivar52_;
                                            self.ch[lvar36].gg = local_216c;
                                            if ivar52_ != 0 {
                                                do_shift = true;
                                                shift = ivar52_;
                                            } else {
                                                do_shift = false;
                                                shift = 0;
                                                can_inc[lvar36] = increase_small_scalefactors(&mut self.ch[lvar36], uvar55_);
                                                {
                                                    let chh = &mut self.ch[lvar36];
                                                    let x34 = chh.x34;
                                                    calculate_scale_spectrum(chh.stride, chh.ng, chh.nsfbw, &chh.start, &chh.width, &mut chh.x34s, &x34, &chh.sf, &chh.mask);
                                                    update_global_gain_nmr(chh, self.pow43);
                                                }
                                                for u in 0..uvar55_ {
                                                    self.ch[lvar36].gg = local_216c - u;
                                                    self.calc_nmr(lvar36, &mut nm, &mut bt);
                                                    self.fill_col(lvar36, u as usize, fvar76, &nm, &bt);
                                                }
                                            }
                                        }
                                        if do_shift {
                                            let nn = uvar55_ - shift;
                                            let mut uu = 0;
                                            if nn >= 1 {
                                                for u2 in 0..nn {
                                                    for i in 0..nd[lvar36] {
                                                        self.nmr_tab[lvar36][i][u2 as usize] = self.nmr_tab[lvar36][i][(u2 + shift) as usize];
                                                        self.bits_tab[lvar36][i][u2 as usize] = self.bits_tab[lvar36][i][(u2 + shift) as usize];
                                                    }
                                                }
                                                uu = nn;
                                            }
                                            for u in uu..uvar55_ {
                                                self.ch[lvar36].gg = local_216c - u;
                                                self.calc_nmr(lvar36, &mut nm, &mut bt);
                                                self.fill_col(lvar36, u as usize, fvar76, &nm, &bt);
                                            }
                                        }
                                        self.ch[lvar36].gg = local_216c;
                                    }
                                    fvar70 = fvar74;
                                }
                            }
                        }
                    }
                }
                if done {
                    break 'outer;
                }
                local_21d0 += 1;
                local_2154 = ivar43;
                if (s & 10) == 2 {
                    break 'outer;
                }
            }
            // ============ LAB_000f2b28 ============
            match bit_count_exceeds_buffer_size(&self.cfg, &bits) {
                None => bvar19 = false,
                Some((ord, n, mx)) => {
                    order = ord;
                    nactive = n;
                    local_2118 = mx;
                    local_21bc = fcvtzs(local_2118 as f32 * 0.75f32);
                }
            }
            if !bvar19 {
                break;
            }
        }
        // ============ exit ============
        for c in 0..nch {
            let ch = &mut self.ch[c];
            let mut gg = ch.gg;
            {
                let (res, sf6460) = (&ch.res, &mut ch.sf6460);
                move_first_sf_to_gg(res, sf6460, &mut gg);
            }
            ch.gg = gg;
            let n = ch.ng * ch.stride;
            ch.sf[..n].copy_from_slice(&ch.sf6460[..n]);
            ch.bits = bits[c];
        }
        out.bits = bits;
        out.lam_state = fvar70;
        out
    }
}
