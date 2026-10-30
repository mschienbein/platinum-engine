//! Polyphase synthesis filter bank: the algorithmic form of Apple's `_mp3d_SubbandSynth` (0x2201c) / `_IDCT32PLONKAS`
//! (0x220bc..0x239f4) and `_mp3d_CopyPCMSamples{Stereo,Mono}` (0x24214 / 0x242fc).
//!
//! One 32-sample slot = 32-point DCT of the subband samples ([`dct32`](dct::dct32), data-flow extracted from the machine
//! code, see `synth_dct.rs`) stored into a history ring, followed by a 512-tap window ([`synth_window`]) over that
//! history with Spirit's rounding (`+512 >> 10`) and int16 saturation.  All arithmetic is explicit 32/64-bit wrapping
//! integer arithmetic; no floats.
//!
//! ## Ring state (layout identical to Apple's, so states can be compared byte for byte)
//! Per channel 34 rows x 16 columns of `i32` (0x880 bytes); the columns are a circular history, the current column `col`
//! (even between granules; `S+8` holds `base + 4*col`) moves down by one per slot.  Row sets alternate their role from
//! slot to slot ("shape"): the *primary* rows (the ones the window reads) hold the values `P0..P15` at column `col`
//! (shape *odd*: rows 17..32; shape *even*: rows 0..15), the *secondary* rows hold `S0..S16` (`S0 == P0`) at column
//! `col+1` (odd: rows 0..16) or `col-1` (even: rows 17..33).  Window row `j` = primary row `base+j`, read at the logical
//! age `a` = column `col - a`; so an age-`a` tap reads `P` of the slot `a` steps back when that slot had the same shape and
//! `S` otherwise: ISO's `V[j]` / `V[32+j]` alternation, realised by the row-set swap.
//! Slot schedule of one call with `n` slots (`rem = n`): shape *odd* first; after a slot with `rem` even the next slot is
//! shape *even* at the same column, after a slot with `rem` odd the column moves down by two and the next slot is shape *odd*
//! (for even `n` that is one column per slot; for odd `n` one column is skipped once, which no real stream does).
//!
//! ## Total behaviour (no panics)
//! `work` shorter than 0x480 ints or `nslots == 0` -> nothing happens; `nslots > 18` is processed as 18 (Layer I/II/III use
//! 12 or 18); an odd ring column given to [`Synth::set_state`] is rounded down to even (Apple jumps into the middle of an
//! instruction there); a too short ring image is applied as far as it goes.  The copy functions stop at the end of `work`
//! / `out`.

#[path = "synth_dct.rs"]
mod dct;
#[path = "synth_window.rs"]
mod window;

use dct::dct32;
use window::{WIN_0, WIN_16, WIN_A};

/// Rows per channel ring.
const ROWS: usize = 34;
/// Columns of the circular history.
const COLS: usize = 16;
/// Ints per channel ring (0x880 bytes).
const RING_INTS: usize = ROWS * COLS;
/// Staging area per channel in `work` (ints): 32 subband rows x 18 slots.
const CH_INTS: usize = 0x240;
/// Slots per call at most (Layer III granule).
const MAX_SLOTS: usize = 18;

/// Persistent polyphase synthesis state: the two channel rings and the current column.  Both are zero after
/// `Reset`/`Initialize` (column 0).
#[derive(Clone)]
pub struct Synth {
    ring: [i32; 2 * RING_INTS],
    /// Current ring column (0..16, even between granules).
    col: usize,
}

impl Default for Synth {
    fn default() -> Self {
        Synth::new()
    }
}

#[inline(always)]
fn mul16(x: i32, c: i32) -> i32 {
    ((x as i64 * c as i64) >> 16) as i32
}

/// `x + ((y * c) >> 16)`.
#[inline(always)]
fn mac(x: i32, y: i32, c: i32) -> i32 {
    x.wrapping_add(mul16(y, c))
}

/// `h * (c << 16)` as the 64-bit product of the joint-truncation terms (wraps like the machine's `smull`/`add x`).
#[inline(always)]
fn wide(h: i32, c: i32) -> i64 {
    (h as i64).wrapping_mul((c as i64) << 16)
}

/// `(x >> 10)` saturated to int16 exactly like the PCM tails of the machine code.
#[inline(always)]
fn pcm(x: i32) -> i16 {
    let v = x >> 10;
    if ((v.wrapping_add(0x8000) as u32) & 0x7fff_ffff) < 0x1_0000 {
        v as i16
    } else if v < 0 {
        i16::MIN
    } else {
        i16::MAX
    }
}

/// Ages 0..16 of the primary row `row` as seen from column `col` (`age a` = column `col - a`).
#[inline(always)]
fn ages(ring: &[i32], row: usize, col: usize) -> [i32; 16] {
    let r = &ring[row * COLS..row * COLS + COLS];
    core::array::from_fn(|a| r[(col + COLS - a) & (COLS - 1)])
}

/// One slot of one channel: DCT of `x`, store into `ring` (shape `odd`, column `col`), window; returns the 32 PCM samples.
#[inline(always)]
fn slot(ring: &mut [i32], col: usize, odd: bool, x: &[i32; 32]) -> [i16; 32] {
    let (p, s) = dct32(x);
    let (base_p, base_s, col_s) = if odd { (17, 0, (col + 1) & 15) } else { (0, 17, (col + 15) & 15) };
    for j in 0..16 {
        ring[(base_p + j) * COLS + col] = p[j];
    }
    ring[base_s * COLS + col_s] = p[0];
    for j in 1..=16 {
        ring[(base_s + j) * COLS + col_s] = s[j - 1];
    }
    let mut out = [0i16; 32];
    // PCM[0]: sum starts with H9 + H8 - H7
    {
        let h = ages(ring, base_p, col);
        let mut acc = h[9].wrapping_add(h[8]).wrapping_sub(h[7]);
        for a in 0..16 {
            acc = acc.wrapping_add(mul16(h[a], WIN_0[a]));
        }
        out[0] = pcm(acc.wrapping_add(512));
    }
    // PCM[16]: odd ages only, subtracted from H9
    {
        let h = ages(ring, base_p + 16, col);
        let mut acc = 0i32;
        for a in (1..16).step_by(2) {
            acc = acc.wrapping_add(mul16(h[a], WIN_16[a]));
        }
        out[16] = pcm(h[9].wrapping_sub(acc).wrapping_add(512));
    }
    for j in 1..16 {
        let h = ages(ring, base_p + j, col);
        let wa = &WIN_A[j - 1];
        // PCM[j]: even group + odd group
        let mut e = mac(512, h[0], wa[0]);
        for a in [2, 4, 6, 12, 14] {
            e = e.wrapping_add(mul16(h[a], wa[a]));
        }
        let s_e = ((e as i64) << 32).wrapping_add(wide(h[10], wa[10])).wrapping_add(wide(h[8], wa[8]));
        let e_tot = (h[8] as i64).wrapping_add(s_e >> 32) as i32;
        let mut o = 0i32;
        for a in [1, 3, 5, 11, 13, 15] {
            o = o.wrapping_add(mul16(h[a], wa[a]));
        }
        let s_o = ((o as i64) << 32).wrapping_add(wide(h[7], wa[7])).wrapping_add(wide(h[9], wa[9]));
        let o_tot = (h[9] as i64).wrapping_sub(s_o >> 32) as i32;
        out[j] = pcm(e_tot.wrapping_add(o_tot));
        // PCM[32 - j]: mirrored coefficients, one accumulation
        let wb = |a: usize| wa[(17 - a) & 15];
        let mut b = mac(512, h[0], wb(0));
        for a in [1, 2, 3, 4, 5, 6, 11, 12, 13, 14, 15] {
            b = b.wrapping_add(mul16(h[a], wb(a)));
        }
        let s_b = ((b as i64) << 32)
            .wrapping_add(wide(h[10], wb(10)))
            .wrapping_add(wide(h[7], wb(7)))
            .wrapping_add(wide(h[9], wb(9)))
            .wrapping_add(wide(h[8], wb(8)));
        let inner = (h[8] as i64).wrapping_sub(s_b >> 32) as i32;
        out[32 - j] = pcm(h[9].wrapping_sub(inner));
    }
    out
}

impl Synth {
    pub fn new() -> Synth {
        Synth { ring: [0; 2 * RING_INTS], col: 0 }
    }

    /// Zero both rings and restart at column 0.
    pub fn reset(&mut self) {
        self.ring = [0; 2 * RING_INTS];
        self.col = 0;
    }

    /// Current ring column (0..16, even).
    pub fn ring_slot(&self) -> usize {
        self.col
    }

    /// Both rings as little-endian bytes (2 x 0x880), Apple's memory image.
    pub fn ring_bytes(&self) -> Vec<u8> {
        self.ring.iter().flat_map(|v| v.to_le_bytes()).collect()
    }

    /// Overwrite the rings and the column (test support); `slot` is rounded down to an even column.
    pub fn set_state(&mut self, slot: usize, ring: &[u8]) {
        for (dst, src) in self.ring.iter_mut().zip(ring.chunks_exact(4)) {
            *dst = i32::from_le_bytes([src[0], src[1], src[2], src[3]]);
        }
        self.col = slot & 14;
    }

    /// `_mp3d_SubbandSynth(work, &S+8, nslots, stereo)`.
    ///
    /// `work` holds the subband samples of channel 0 in `work[0..0x240]` (sample (sb, t) at `sb*18 + t`) and of channel 1 in
    /// `work[0x240..0x480]`; `nslots` is the number of 32-sample slots (18 for Layer III).  On return the PCM is in place:
    /// channel 0 as the low halfword of cell `sb*18+t` (slot `t`, sample `sb`), channel 1 as the high halfword of the same
    /// cell, in `work[0..0x240]`; the halfwords not written keep their previous contents (Apple's exact leftovers).
    pub fn subband_synth(&mut self, work: &mut [i32], nslots: u32, stereo: bool) {
        if work.len() < 2 * CH_INTS || nslots == 0 {
            return;
        }
        let n = (nslots as usize).min(MAX_SLOTS);
        // slot schedule (shape, column), see the module documentation
        let mut sched = [(true, 0usize); MAX_SLOTS];
        let mut col = self.col;
        let mut odd = true;
        let mut rem = n;
        for s in sched.iter_mut().take(n) {
            *s = (odd, col);
            if rem & 1 == 1 {
                col = (col + COLS - 2) & (COLS - 1);
                odd = true;
            } else {
                odd = false;
            }
            rem -= 1;
        }
        let nch = if stereo { 2 } else { 1 };
        for ch in 0..nch {
            let ring = &mut self.ring[ch * RING_INTS..(ch + 1) * RING_INTS];
            for (t, &(odd, col)) in sched.iter().enumerate().take(n) {
                let x: [i32; 32] = core::array::from_fn(|sb| work[ch * CH_INTS + sb * 18 + t]);
                let out = slot(ring, col, odd, &x);
                for (sb, &v) in out.iter().enumerate() {
                    let cell = &mut work[sb * 18 + t];
                    if ch == 0 {
                        *cell = (*cell & !0xffff) | (v as u16 as i32);
                    } else {
                        *cell = (*cell & 0xffff) | ((v as u16 as i32) << 16);
                    }
                }
            }
        }
        self.col = col;
    }
}

/// `_mp3d_CopyPCMSamplesStereo(work, out, n)`: interleave the packed (ch0 low / ch1 high halfword) cells, `n` (a multiple
/// of 32) samples per channel; `out` receives `L0 R0 L1 R1 ...`.  Stops at the end of `work` / `out`.
pub fn copy_pcm_stereo(work: &[i32], out: &mut [i16], n: usize) {
    let mut o = out.chunks_exact_mut(2);
    for g in 0..n / 32 {
        for sb in 0..32 {
            let (Some(w), Some(d)) = (work.get(sb * 18 + g), o.next()) else {
                return;
            };
            d[0] = *w as u16 as i16;
            d[1] = (*w >> 16) as u16 as i16;
        }
    }
}

/// `_mp3d_CopyPCMSamplesMono(work, out, n)`: low halfword of each cell, `n` a multiple of 32.  Stops at the end of `work` / `out`.
pub fn copy_pcm_mono(work: &[i32], out: &mut [i16], n: usize) {
    let mut o = out.iter_mut();
    for g in 0..n / 32 {
        for sb in 0..32 {
            let (Some(w), Some(d)) = (work.get(sb * 18 + g), o.next()) else {
                return;
            };
            *d = *w as u16 as i16;
        }
    }
}
