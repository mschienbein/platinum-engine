//! F4a: block switching, `_ComputeBlockMode` (0x47ddc) with `_fir4Filter` (0xa24e0) and `_trde_peakdet` (0x4a62c), reproducing the original's
//! decisions exactly.
//!
//! Per channel and call (= one `EncodeFrame`, 1024 new input frames, already clamped/scaled): history-ring write/read (the 1088-sample delayed
//! block = bank input), stage-1 FIR + peak detector, optional stage-2 band-pass energy detector, state machine, CPE pair join.
//! Call `j` (0-based) decides the window sequence of AU `j` whose coded block is `[1024j-1088, 1024j-64)`.
//!
//! Bit-exactness: everything is IEEE f32 with the stated operation order and **no libm**. The only fused operation is the release smoothing of
//! the 32-sample peak row (`f32::mul_add`, see `peakdet`). `vDSP_svemg(64)`, `vDSP_maxmgv`, `vDSP_minv`, `vDSP_vfill` are modelled exactly
//! ([`svemg64`]). Requires the default FPCR (no FZ-specific code; the arithmetic follows the ambient mode like Apple's own instructions).

use crate::portable::aac::fb::input::{HistoryRing, FRAME_LEN};
use crate::portable::aac::psy::vdsp::sve;
use crate::portable::aac::types::F_1E30;

/// `modeT1_attack` (0x925a60): next mode if an attack is present now or was present in the previous call, indexed by the previous mode.
const T1_ATTACK: [u8; 4] = [1, 2, 2, 1];
/// `modeT2_noattack` (0x925a70).
const T2_NOATTACK: [u8; 4] = [0, 3, 3, 0];
/// `fir4_coefA` (0x9235f0): stage-1 high-pass taps.
const COEF_A: [f32; 4] = [0.4927000105381012, -4.878002643585205, 8.27776050567627, -3.8922345638275146];
/// `fir4_coefB` (0x923600): stage-2 band-pass taps.
const COEF_B: [f32; 4] = [-0.13186977803707123, 0.13313673436641693, 0.13313673436641693, -0.13186977803707123];
/// `ring_init_pattern` (memset_pattern16 0x925b70, bits 0x7149f2ca): initial stage-2 ring.
const INIT_RING: f32 = F_1E30;
/// `arr_init_pattern` (0x925b60): 4 x 10.0 at `chan+0x118`.
const INIT_ARR: f32 = 10.0;
/// `DAT_00922718`.
const SVEMG_BIAS: f32 = 10000.0;

/// The 0x40-byte TRDE parameter block (`psy+0x278`), 16 words.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct TrdeCfg {
    /// cfg[0..2]: attack thresholds of the 128/64/32-sample rows (also `cfg[0]` in the 0x4 flag test).
    pub thr: [f32; 3],
    /// cfg[3]: peak-hold factor of the 0x4 flag test.
    pub hold: f32,
    /// cfg[4..6]: release coefficients c^4, c^2, c of the 128/64/32 rows.
    pub decay: [f32; 3],
    /// cfg[7]: flags (bit 0 = second tracker L1 [never enabled at engine configs], bit 1 = stage-2 energy detector).
    pub flags: i32,
    /// cfg[8..10]: L1 thresholds (unused when `flags & 1 == 0`).
    pub thr2: [f32; 3],
    /// cfg[11]: stage-2 hold ratio (0.4).
    pub s2_ratio: f32,
    /// cfg[12..14]: L1 release coefficients (unused when `flags & 1 == 0`).
    pub decay2: [f32; 3],
    /// cfg[15]: stage-2 hold decay.
    pub s2_decay: f32,
}

impl TrdeCfg {
    pub fn from_words(w: &[u32; 16]) -> Self {
        let f = |i: usize| f32::from_bits(w[i]);
        Self {
            thr: [f(0), f(1), f(2)],
            hold: f(3),
            decay: [f(4), f(5), f(6)],
            flags: w[7] as i32,
            thr2: [f(8), f(9), f(10)],
            s2_ratio: f(11),
            decay2: [f(12), f(13), f(14)],
            s2_decay: f(15),
        }
    }
}

/// Channel/element layout seen by the block switch: `cpe_pairs` (the `psy+0x240` list) and the LFE flags (`psy+0x260`).
#[derive(Clone, Debug, PartialEq)]
pub struct BsLayout {
    pub cpe_pairs: Vec<(usize, usize)>,
    pub lfe: Vec<bool>,
}

impl BsLayout {
    /// Default AudioToolbox channel layouts for N channels (`CPEL`/`LFE` records): 1 {SCE}; 2 {CPE(0,1)}; 3 {SCE,CPE(1,2)};
    /// 4 {CPE(0,1),CPE(2,3)}; 5 {SCE,CPE(1,2),CPE(3,4)}; 6 {SCE,CPE(1,2),CPE(3,4),LFE5}; 7 {SCE,CPE(1,2),CPE(3,4),SCE5,LFE6};
    /// 8 {SCE,CPE(1,2),CPE(3,4),CPE(5,6),LFE7}.
    pub fn engine(nch: usize) -> Self {
        let cpe_pairs = match nch {
            1 => vec![],
            2 => vec![(0, 1)],
            3 => vec![(1, 2)],
            4 => vec![(0, 1), (2, 3)],
            5 | 6 | 7 => vec![(1, 2), (3, 4)],
            8 => vec![(1, 2), (3, 4), (5, 6)],
            _ => panic!("unsupported channel count {nch}"),
        };
        let mut lfe = vec![false; nch];
        match nch {
            6 => lfe[5] = true,
            7 => lfe[6] = true,
            8 => lfe[7] = true,
            _ => {}
        }
        Self { cpe_pairs, lfe }
    }
}

/// Per-channel state: the modelled part of Apple's `PsyChannel` (first 0x290 bytes), of its switch-control struct (`*(chan+0x248)`, 0x8c bytes)
/// and of the history ring (`chan+0x138`). Field comments give the byte offsets in the Apple structs.
#[derive(Clone, Debug)]
pub struct BsChan {
    /// +0x00 stage-1 high-pass taps.
    pub coef_a: [f32; 4],
    /// +0x10 stage-2 band-pass taps.
    pub coef_b: [f32; 4],
    /// +0x20 stage-1 FIR carried state.
    pub fir1: [f32; 4],
    /// +0x30 stage-2 FIR carried state.
    pub fir2: [f32; 4],
    /// +0x40 stage-2 8-float ring.
    pub s2_ring: [f32; 8],
    /// +0x60 stage-2 ring index.
    pub s2_idx: i32,
    /// +0x64 / +0xa0 / +0xdc: 3 rows of 5 floats (128/64/32-sample windows): fil peaks, second tracker, raw-input peaks.
    pub l0: [[f32; 5]; 3],
    pub l1: [[f32; 5]; 3],
    pub l2: [[f32; 5]; 3],
    /// +0x118 raw-peak history (4 floats).
    pub arr: [f32; 4],
    /// +0x128 stage-2 hold counter, +0x12c hold value.
    pub hold_cnt: i32,
    pub hold_val: f32,
    /// +0x240 previous "pre-echo" mask (copied from the switch struct at every call; zeroed on a SHORT decision).
    pub pre_echo_mask: u16,
    /// +0x250 per-32-sample flags (u16[32]).
    pub flags: [u16; 32],
    /// switch struct +0x00 A[12], +0x18 B[12] (u16), +0x30 mask (u16), +0x34 previous (post-join) mode, +0x38 previous attack, +0x41 sticky.
    pub a: [u16; 12],
    pub b: [u16; 12],
    pub mask: u16,
    pub prev_mode: i32,
    pub prev_attack: bool,
    pub sticky: bool,
    /// History ring (2112 floats, write position).
    pub ring: HistoryRing,
}

impl Default for BsChan {
    fn default() -> Self {
        Self::new()
    }
}

impl BsChan {
    /// State right after `_aacOpen`.
    pub fn new() -> Self {
        Self {
            coef_a: COEF_A,
            coef_b: COEF_B,
            fir1: [0.0; 4],
            fir2: [0.0; 4],
            s2_ring: [INIT_RING; 8],
            s2_idx: 0,
            l0: [[0.0; 5]; 3],
            l1: [[0.0; 5]; 3],
            l2: [[0.0; 5]; 3],
            arr: [INIT_ARR; 4],
            hold_cnt: 0,
            hold_val: 0.0,
            pre_echo_mask: 0,
            flags: [0; 32],
            a: [0; 12],
            b: [0; 12],
            mask: 0,
            prev_mode: 0,
            prev_attack: false,
            sticky: false,
            ring: HistoryRing::new(),
        }
    }
}

/// `vDSP_svemg(a, 1, &c, 64)`: for n = 64 the library sums the magnitudes exactly as `vDSP_sve` ([`sve`]) sums the absolute values: four 4-lane
/// accumulators over chunks of 16, then `v0 += v2; v1 += v3; v0 += v1` and `(v0[0] + v0[1]) + (v0[2] + v0[3])`.
fn svemg64(a: &[f32]) -> f32 {
    debug_assert_eq!(a.len(), 64);
    let mut magnitudes = [0.0f32; 64];
    for (m, x) in magnitudes.iter_mut().zip(a) {
        *m = x.abs();
    }
    sve(&magnitudes)
}

/// `_fir4Filter(in, out, n, state[4], coef[4])` (0xa24e0): `y[m] = ((((S + c0 x[m-3]) + c1 x[m-2]) + c2 x[m-1]) + c3 x[m])` with separate
/// multiply/add (no FMA); `S` (the carried state) enters only the first 4-lane block, samples before the chunk start read as 0. The new state is
/// `c0 x[n-3] + c1 x[n-2] + c2 x[n-1] + 0*c3` etc. (summed in that order). `n` must be a multiple of 4. `x` and `out` may not alias.
pub fn fir4(x: &[f32], out: &mut [f32], state: &mut [f32; 4], coef: &[f32; 4]) {
    let n = x.len();
    assert!(n % 4 == 0 && out.len() == n && n >= 4);
    let [c0, c1, c2, c3] = *coef;
    let nb = n / 4;
    for b in 0..nb {
        let cur = [x[4 * b], x[4 * b + 1], x[4 * b + 2], x[4 * b + 3]];
        let prev = if b > 0 { [x[4 * b - 4], x[4 * b - 3], x[4 * b - 2], x[4 * b - 1]] } else { [0.0; 4] };
        let a = [prev[1], prev[2], prev[3], cur[0]];
        let bb = [prev[2], prev[3], cur[0], cur[1]];
        let c = [prev[3], cur[0], cur[1], cur[2]];
        for l in 0..4 {
            let mut acc = if b == 0 { state[l] } else { 0.0 };
            acc = acc + a[l] * c0;
            acc = acc + bb[l] * c1;
            acc = acc + c[l] * c2;
            acc = acc + cur[l] * c3;
            out[4 * b + l] = acc;
        }
    }
    let l = [x[n - 4], x[n - 3], x[n - 2], x[n - 1]];
    let v2a = [l[1], l[2], l[3], 0.0];
    let v2b = [l[2], l[3], 0.0, 0.0];
    let v2c = [l[3], 0.0, 0.0, 0.0];
    let mut v3 = [0.0f32; 4];
    for i in 0..4 {
        v3[i] = v2a[i] * c0;
        v3[i] = v3[i] + v2b[i] * c1;
        v3[i] = v3[i] + v2c[i] * c2;
        v3[i] = v3[i] + 0.0f32 * c3;
    }
    *state = v3;
}

#[inline]
fn max_mag(x: &[f32]) -> f32 {
    let mut m = 0.0f32;
    for &v in x {
        let a = v.abs();
        if a > m {
            m = a;
        }
    }
    m
}

/// The block-switch detector: configuration + per-channel state (`_ComputeBlockMode`'s `chan` array).
pub struct BlockSwitch {
    cfg: TrdeCfg,
    quality: u32,
    layout: BsLayout,
    pub chans: Vec<BsChan>,
    /// `aac+0x130` (always 1 after `AACGetInfo`).
    pub enable: bool,
}

impl BlockSwitch {
    /// `quality` = `aac+0xe0` (cdqu 0x40 -> 64).
    pub fn new(nch: usize, cfg: TrdeCfg, quality: u32, layout: BsLayout) -> Self {
        debug_assert_eq!(layout.lfe.len(), nch);
        Self { cfg, quality, layout, chans: (0..nch).map(|_| BsChan::new()).collect(), enable: true }
    }

    /// `_ComputeBlockMode`: `input[ch]` = the 1024 new samples (clamped/scaled), `delayed[ch]` receives the block delayed by 1088 samples
    /// (`chan+0x140`). Returns the window sequence of every channel after the CPE join (the AU's modes).
    pub fn compute_block_mode(&mut self, input: &[&[f32]], delayed: &mut [[f32; 1024]]) -> Vec<u8> {
        let nch = self.chans.len();
        assert!(input.len() == nch && delayed.len() == nch);
        let mut modes = vec![0u8; nch];
        for ch in 0..nch {
            assert_eq!(input[ch].len(), FRAME_LEN);
            let wold = self.chans[ch].ring.wpos;
            self.chans[ch].ring.push(input[ch], &mut delayed[ch]);
            if self.enable && !self.layout.lfe[ch] {
                modes[ch] = self.detect_channel(ch, input[ch], wold);
            } else {
                modes[ch] = 0;
            }
        }
        // channel-pair join (0x48330)
        for &(a, b) in &self.layout.cpe_pairs {
            let (mut ma, mut mb) = (modes[a], modes[b]);
            if ma == 0 {
                modes[a] = mb;
                ma = mb;
            }
            if mb == 0 {
                modes[b] = ma;
                mb = ma;
            }
            if ma == 2 || mb == 2 {
                modes[a] = 2;
                modes[b] = 2;
                ma = 2;
                mb = 2;
            }
            if ma == 3 {
                if mb == 1 {
                    modes[a] = 2;
                    modes[b] = 2;
                }
            } else if ma == 1 && mb == 3 {
                modes[a] = 2;
                modes[b] = 2;
            }
        }
        for ch in 0..nch {
            if !self.layout.lfe[ch] {
                self.chans[ch].prev_mode = modes[ch] as i32;
            }
        }
        modes
    }

    /// FIR stages + `_trde_peakdet` + stage 2 + state machine for one channel; `wold` = ring write position before this call's write.
    fn detect_channel(&mut self, ch: usize, x: &[f32], wold: usize) -> u8 {
        let n = FRAME_LEN;
        let size = crate::portable::aac::fb::input::RING_SIZE;
        let n1 = (wold + n).min(size) - wold;
        let c = &mut self.chans[ch];
        let mut fil = [0.0f32; 1024];
        let coef_a = c.coef_a;
        let mut st1 = c.fir1;
        fir4(&x[..n1], &mut fil[..n1], &mut st1, &coef_a);
        if n1 != n {
            fir4(&x[n1..], &mut fil[n1..], &mut st1, &coef_a);
        }
        c.fir1 = st1;
        peakdet(&self.cfg, self.quality, c, &fil, x);
        if self.cfg.flags & 2 != 0 {
            let mut tmp2 = [0.0f32; 1024];
            let coef_b = c.coef_b;
            let mut st2 = c.fir2;
            fir4(&fil, &mut tmp2, &mut st2, &coef_b);
            c.fir2 = st2;
            let s9 = self.cfg.s2_ratio;
            let s10 = self.cfg.s2_decay;
            for i in 0..16 {
                let e = svemg64(&tmp2[64 * i..64 * i + 64]) + SVEMG_BIAS;
                let cur = c.hold_val;
                let s0;
                if e <= cur {
                    if c.hold_cnt > 0 {
                        s0 = s10 * cur;
                        c.hold_val = s0;
                    } else {
                        c.hold_cnt += 1;
                        s0 = cur;
                    }
                } else {
                    c.hold_val = e;
                    c.hold_cnt = 0;
                    s0 = e;
                }
                let idx = if c.s2_idx < 7 { c.s2_idx + 1 } else { 0 };
                c.s2_idx = idx;
                c.s2_ring[idx as usize] = s0;
                let mut mn = c.s2_ring[0];
                for &v in &c.s2_ring[1..] {
                    if v < mn {
                        mn = v;
                    }
                }
                let hold = c.hold_val;
                let s1 = s9 * hold;
                if hold > 0.0 && s1 > mn {
                    c.flags[2 * i] = 0x20;
                    c.s2_ring = [hold; 8];
                }
            }
        }
        state_machine(c)
    }
}

/// One step of the peak pyramid of a row of 3 x 5 floats: `row[r][4] = max(row[r + 1][4], row[r + 1][4 - dist])`.
#[inline]
fn pyramid_step(row: &mut [[f32; 5]; 3], r: usize, dist: usize) {
    let a = row[r + 1][4];
    let b = row[r + 1][4 - dist];
    row[r][4] = if a >= b { a } else { b };
}

/// `_trde_peakdet(fil, raw, flags, 32, chan, cfg, quality)`: 32 sub-blocks of 32 samples.
fn peakdet(cfg: &TrdeCfg, quality: u32, c: &mut BsChan, fil: &[f32; 1024], raw: &[f32]) {
    let ext = quality >= 0x21;
    let l1_on = cfg.flags & 1 != 0;
    let s8 = cfg.hold;
    let s9 = cfg.thr[0];
    for n in 0..32usize {
        c.l0[2][4] = max_mag(&fil[32 * n..32 * n + 32]);
        if ext {
            c.l2[2][4] = max_mag(&raw[32 * n..32 * n + 32]);
        }
        c.l1[2][4] = c.l0[2][4];
        // pyramid: two iterations (rows 2 -> 1 with distance 1, rows 1 -> 0 with distance 2)
        for (r, dist) in [(1usize, 1usize), (0, 2)] {
            pyramid_step(&mut c.l0, r, dist);
            pyramid_step(&mut c.l1, r, dist);
            if ext {
                pyramid_step(&mut c.l2, r, dist);
            }
        }
        // attack tests
        c.flags[n] = 0;
        let mut w12v: u16 = 0;
        let mut w13 = 4usize;
        for k in 0..3 {
            if ((n + 1) & (w13 - 1)) == 0 {
                let cur = c.l0[k][4];
                let s1 = cur * cfg.thr[k];
                let s2 = c.l0[k][4 - w13];
                if s1 > s2 {
                    w12v = if (cur as f64) > 327.68 { 2 } else { 1 };
                    c.flags[n] = w12v;
                }
                if l1_on {
                    let cur1 = c.l1[k][4];
                    let s1b = cur1 * cfg.thr2[k];
                    let s2b = c.l1[k][4 - w13];
                    if s1b > s2b {
                        w12v = (w12v & 0xffe7) | if (cur1 as f64) > 327.68 { 0x10 } else { 8 };
                        c.flags[n] = w12v;
                    }
                }
            }
            w13 >>= 1;
        }
        if ((n + 1) & 3) == 0 {
            if c.l0[0][4] < 300.0 {
                for f in &mut c.flags[n - 3..n + 1] {
                    *f = 0;
                }
            }
            if ext {
                let mut s0 = c.arr[0];
                for j in 1..4 {
                    s0 = if s0 < c.arr[j] { s0 } else { c.arr[j] };
                }
                let s1 = c.l2[0][4];
                let s2 = s1 * s9;
                let s3 = c.l2[0][0];
                let mut w9 = s2 > s3;
                let s2b = s8 * s1;
                if !(s2b <= s0) {
                    w9 = true;
                }
                if s1 > 1000.0 && w9 {
                    c.flags[n] |= 4;
                }
            }
        }
        // release smoothing / shifts of the L0 rows (the one fused operation: fma((1-c), s2, c*s1))
        for x9 in 0..3 {
            let cc = cfg.decay[x9];
            let row = &mut c.l0[x9];
            row[0] = row[1];
            row[1] = row[2];
            let s1 = row[3];
            let s2 = row[4];
            let s3 = cc * s1;
            let s0 = (1.0f32 - cc).mul_add(s2, s3);
            let new = if s1 > s2 && x9 == 2 { s0 } else { s2 };
            row[2] = s1;
            row[3] = new;
        }
        if l1_on {
            for x9 in 0..3 {
                let cc = cfg.decay2[x9];
                let row = &mut c.l1[x9];
                row[0] = row[1];
                row[1] = row[2];
                let s2 = row[3];
                let s1 = row[4];
                row[2] = s2;
                // `flags & 1` is never set at the engine configurations (asm 0x4a9e4): this branch is not exercised
                row[3] = if x9 == 2 && s2 > s1 { (1.0f32 - cc).mul_add(s1, cc * s2) } else { s1 };
            }
        }
        if ext {
            for row in c.l2.iter_mut() {
                for i in 0..4 {
                    row[i] = row[i + 1];
                }
            }
            if ((n + 1) & 3) == 0 {
                let a = c.arr;
                c.arr[0] = a[1];
                c.arr[1] = a[2];
                c.arr[2] = a[3];
                c.arr[3] = c.l2[2][4];
            }
        }
    }
}

/// State machine 0x48178..0x48330: returns the channel's mode before the pair join.
fn state_machine(c: &mut BsChan) -> u8 {
    c.pre_echo_mask = c.mask;
    for k in 4..12 {
        c.a[k] = 0;
        c.b[k] = 0;
    }
    c.mask = 0;
    for i in 0..8usize {
        for j in 0..4usize {
            let f = c.flags[4 * i + j];
            if f & 0x18 != 0 {
                c.b[i + 4] |= 0x20;
            }
            if f & 0x04 != 0 {
                c.b[i + 4] |= 0x10;
            }
            if f & 0x23 != 0 {
                c.a[i + 4] = 1;
            }
        }
    }
    if c.sticky {
        c.a[0] = 1;
    }
    c.sticky = c.a[7] == 1;
    let mut b13 = false;
    for k in 0..8 {
        let s = c.a[k];
        if s == 1 {
            b13 = true;
        } else if s == 8 && c.prev_attack {
            b13 = true;
        }
        let v = c.b[k];
        if v & 0x10 != 0 {
            c.mask |= 1;
        }
        if v & 0x20 != 0 {
            c.mask |= 2;
        }
    }
    let (a16, a17) = (c.flags[16], c.flags[17]);
    for k in 0..4 {
        c.a[k] = c.a[8 + k];
        c.b[k] = c.b[8 + k];
    }
    if (a16 | a17) & 0x22 != 0 {
        b13 = true;
    }
    let prev = c.prev_mode;
    let mode: u8 = if b13 || c.prev_attack {
        T1_ATTACK[prev as usize] // prev > 3 never happens (modes 0..3 only)
    } else {
        T2_NOATTACK[prev as usize]
    };
    if mode == 2 && (b13 || c.prev_attack) {
        c.pre_echo_mask = 0;
    }
    c.prev_attack = b13;
    mode
}
