//! Back half of `MP3DecoderWrapper_SpiritDSP::DecodeFrame` (file 0x13d768): everything from the dequantized
//! spectra (`LAB_0013f1e8`, 0x13f1e8) to the PCM, for one granule:
//!
//! 1. joint-stereo processing: MS (`_mp3d_MSStereo`) and/or intensity stereo (0x13f1e8..0x13f4b0),
//! 2. per channel (0x13f4b0..0x13f878): short-block reorder, count bookkeeping, alias reduction,
//!    IMDCT (`_imdct36` / `_IMDCT12`) with overlap-add, frequency inversion, zero fill,
//! 3. polyphase synthesis `_mp3d_SubbandSynth` and `_mp3d_CopyPCMSamples{Stereo,Mono}` (0x13f87c..0x13f910).
//!
//! The granule counter (`*S`) belongs to the front half (see `granule.rs`); the end-of-frame test at
//! 0x13f87c..0x13f8b0 only rewrites that counter and is done by the front.
//!
//! The per-channel callback `*(S+0x1d90)` (0x13f64c: `blraaz x8`) is never installed on the ExtAudioFile path:
//! `ACMP3Decoder::Initialize` (0x550bc) and `Reset` (0x13d754) store zero there and no other store to
//! S+0x1d90/S+0x1d98 exists in the image (see NOTES.md), so the call is not transcribed.
//!
//! Every statement carries the address of the instruction block it transcribes.
//!
//! ## Total behaviour
//! Nothing here panics or loops forever, whatever the front half hands over.  For states a real stream can produce the
//! behaviour is Apple's exactly; the states Apple itself would turn into out-of-bounds memory accesses (spectrum lines
//! at or beyond `count`, counts above 576, record/table offsets that leave `S`, sfb widths of 0 or above 33, unterminated
//! width tables, out-of-range is_pos positions) are defined as follows:
//! * bytes of `S` / `W` beyond the mirrored 0xd8 / 0x12a0 bytes read as 0, writes there are dropped (so an unterminated
//!   width table ends at the end of the mirror);
//! * a short-block sfb entry of width 0 ends the reorder region, a width above 33 is not reordered (it still advances);
//! * the number of IMDCT subbands is capped at 32 (counts above 585 are processed like 576);
//! * scans/loops over spectrum lines stop at the end of `W`.

use super::back_tables::{ANTIALIAS, REORDER_DATA, START_POSITIONS, STEREO};
use super::granule::{rec, GranuleIn, Meta, Work, CH_STRIDE, ISPOS_BASE, META_SR_CACHE, SPEC_BASE};
use super::hybrid::{imdct12, imdct36};
use super::stereo::ms_stereo;
use super::synth::{copy_pcm_mono, copy_pcm_stereo, Synth};

/// IMDCT overlap words per channel (`S+0x350 + ch*0x480`).
pub const OVERLAP_WORDS: usize = 0x120;

/// Persistent state of the back half: IMDCT overlap-add memory and the polyphase synthesis rings.
pub struct BackEnd {
    /// `S+0x350 .. S+0x350+2*0x480`: 288 words per channel (32 subbands x 9).
    overlap: [[i32; OVERLAP_WORDS]; 2],
    synth: Synth,
}

impl Default for BackEnd {
    fn default() -> Self {
        BackEnd::new()
    }
}

/// Byte `i` of the mirrored part of `S`; 0 beyond it.
#[inline]
fn mb(meta: &Meta, i: usize) -> u8 {
    meta.b.get(i).copied().unwrap_or(0)
}

/// Byte at offset `off` of `W`; 0 beyond it.
#[inline]
fn wbyte(work: &Work, off: usize) -> u8 {
    work.w.get(off >> 2).map_or(0, |v| (*v >> ((off & 3) * 8)) as u8)
}

/// Store a byte at offset `off` of `W`; dropped beyond it.
#[inline]
fn set_wbyte(work: &mut Work, off: usize, v: u8) {
    if let Some(w) = work.w.get_mut(off >> 2) {
        let sh = (off & 3) * 8;
        *w = (*w & !(0xff << sh)) | ((v as i32) << sh);
    }
}

impl BackEnd {
    /// State after `ACMP3Decoder::Initialize`/`Reset`: everything zero.
    pub fn new() -> BackEnd {
        BackEnd { overlap: [[0; OVERLAP_WORDS]; 2], synth: Synth::new() }
    }

    /// Zero the overlap memory and the synthesis rings.
    pub fn reset(&mut self) {
        self.overlap = [[0; OVERLAP_WORDS]; 2];
        self.synth.reset();
    }

    /// Overlap memory of channel `ch` (for the Layer I/II aliasing hazard owned by the front half); `ch` is taken modulo 2.
    pub fn overlap_mut(&mut self, ch: usize) -> &mut [i32; OVERLAP_WORDS] {
        &mut self.overlap[ch & 1]
    }

    /// Polyphase state, for tests.
    pub fn synth(&self) -> &Synth {
        &self.synth
    }

    /// Mutable polyphase state (state injection in tests).
    pub fn synth_mut(&mut self) -> &mut Synth {
        &mut self.synth
    }

    /// One Layer III granule, from the dequantized spectra to PCM (0x13f1e8..0x13f910).
    /// `pcm` receives `576 * nch` interleaved int16 (as far as it is long enough); returns 576 (samples per channel).
    pub fn granule(&mut self, g: &mut GranuleIn<'_>, pcm: &mut [i16]) -> usize {
        let hdr = g.hdr;
        let nch = g.nch();
        let rec0 = g.rec0.min(super::granule::META_LEN); // a record offset outside the mirrored S reads as zeros
        // ---- 0x13f1e8..0x13f4b0 : joint stereo
        if (hdr & 0xc0) == 0x40 && (hdr & 0x30) != 0 {
            joint_stereo(hdr, g.meta, g.work, rec0);
        }
        // ---- 0x13f4b0..0x13f878 : per channel
        for ch in 0..nch {
            self.channel(g.meta, g.work, rec0 + ch * super::granule::REC_LEN, ch);
        }
        // ---- 0x13f8b4..0x13f910 : synthesis + PCM copy (n = 0x240)
        let n = 0x240usize;
        let stereo = (hdr & 0xc0) != 0xc0;
        self.synth.subband_synth(&mut g.work.w, (n >> 5) as u32, stereo);
        if stereo {
            copy_pcm_stereo(&g.work.w, pcm, n);
        } else {
            copy_pcm_mono(&g.work.w, pcm, n);
        }
        n
    }

    /// Layer I/II tail (0x13f8b4..0x13f910 entered from the Layer I/II paths): `_mp3d_SubbandSynth(W, S+8,
    /// nblocks, stereo)` followed by `CopyPCMSamples`.  Returns samples per channel (`32 * nblocks`; at most 18 blocks
    /// are synthesized).
    pub fn synth_only(&mut self, work: &mut Work, nblocks: usize, stereo: bool, pcm: &mut [i16]) -> usize {
        let n = nblocks.min(18) * 32;
        self.synth.subband_synth(&mut work.w, (n >> 5) as u32, stereo);
        if stereo {
            copy_pcm_stereo(&work.w, pcm, n);
        } else {
            copy_pcm_mono(&work.w, pcm, n);
        }
        n
    }

    /// 0x13f4b0..0x13f878, one channel.  `rec` = byte offset of the channel's granule record in `S`.
    fn channel(&mut self, meta: &mut Meta, work: &mut Work, rec: usize, ch: usize) {
        let spec = SPEC_BASE + ch * CH_STRIDE; // x27 (int index in W)
        // 0x13f4c0: number of long (IMDCT36) subbands of a mixed block
        let nl: u32 = if mb(meta, rec + rec::MIXED) == 0 {
            0
        } else if meta.b[META_SR_CACHE] != 2 {
            2
        } else {
            4
        };
        // 0x13f508.. : count after short-block reorder
        let nz: u32;
        let limit: i32; // w10
        if mb(meta, rec + rec::N_SHORT) == 0 {
            // 0x13f5f0
            nz = meta.count(ch) as u32;
            limit = 0x1f;
        } else {
            let long_lines = (18 * nl) as usize; // w8 = (2nl)|(16nl)
            let start = spec + long_lines; // x9
            let rem = meta.count(ch) as i32 - long_lines as i32; // w10
            let mut x13 = start;
            if rem >= 1 {
                // 0x13f530: reorder every short sfb block of the nonzero part
                let end = start + rem as usize; // x10
                let mut tp = rec + mb(meta, rec + rec::SFB_OFF) as usize + mb(meta, rec + rec::N_LONG) as usize; // x11
                let mut x12 = start;
                loop {
                    let w = mb(meta, tp) as usize; // w13
                    tp += 3;
                    if w == 0 {
                        break; // zero width / end of the mirrored table: ends the reorder region
                    }
                    let sh = (w as u64).wrapping_sub(1) & 63;
                    if w <= 33 && ((1u64 << sh) & 0xf7c4_0001) == 0 {
                        reorder_block(&mut work.w, x12, w);
                    }
                    x13 = x12 + 6 * w; // x13 = x12 + (w + 2w)*8 bytes
                    x12 = x13;
                    if x13 >= end {
                        break;
                    }
                }
            }
            // 0x13f604
            limit = nl as i32 - 1;
            nz = ((x13 - start) as u32).wrapping_add(long_lines as u32);
            meta.set_count(ch, nz as u16);
        }
        // 0x13f61c
        let prev = meta.prev_count(ch) as u32;
        let w19 = (nz & 0xffff).max(prev);
        meta.set_prev_count(ch, nz as u16);
        let w20 = w19.wrapping_mul(0xe39).wrapping_add(0xf1c9) >> 16;
        let mut w22 = (w20 as i32).min(limit); // csel lt (signed)
        // 0x13f64c: callback at S+0x1d90 is null (see module docs)
        // 0x13f670: alias reduction over `w22` subband boundaries
        if w22 >= 1 {
            let mut up = spec + 18;
            let mut lo = spec + 17;
            while w22 != 0 {
                alias_boundary(&mut work.w, up, lo);
                up += 18;
                lo += 18;
                w22 -= 1;
            }
        }
        // 0x13f6f0
        let w8 = if w19 < 0x22f { w20 + 1 } else { w20 };
        let w22 = (if w8 > nl { w8 } else { nl + 1 }).min(32);
        let block_type = mb(meta, rec + rec::BLOCK_TYPE);
        // 0x13f710: IMDCT
        let ovl = &mut self.overlap[ch & 1];
        let mut x28 = spec;
        let mut o = 0usize;
        if nl != 0 {
            imdct36(&mut work.w, spec, &mut ovl[..], false, nl as i32);
            x28 = spec + (18 * nl) as usize;
            o = (9 * nl) as usize;
        }
        let w26 = w22.wrapping_sub(nl) as i32;
        if block_type == 3 {
            imdct36(&mut work.w, x28, &mut ovl[o..], true, w26);
        } else if block_type != 2 {
            imdct36(&mut work.w, x28, &mut ovl[o..], false, w26);
        } else if w26 != 0 {
            // 0x13f770: short blocks, three IMDCT12 per subband
            let mut cnt = w26;
            let mut x28 = x28;
            let mut o = o;
            loop {
                let out = x28 - 18;
                for k in 0..6 {
                    work.w[out + k] = ovl[o + k];
                }
                // call 1: in = x28, out = out+6, ovl = o+6
                {
                    let mut tmp = [0i32; 6];
                    imdct12(&work.w[x28..], &mut tmp, &mut ovl[o + 6..o + 9]);
                    work.w[out + 6..out + 12].copy_from_slice(&tmp);
                }
                // call 2: in = x28+2, out = out+12
                {
                    let mut tmp = [0i32; 6];
                    imdct12(&work.w[x28 + 2..], &mut tmp, &mut ovl[o + 6..o + 9]);
                    work.w[out + 12..out + 18].copy_from_slice(&tmp);
                }
                // call 3: in = x28+4, out = the overlap array itself (ovl[o..o+6])
                {
                    let mut tmp = [0i32; 6];
                    imdct12(&work.w[x28 + 4..], &mut tmp, &mut ovl[o + 6..o + 9]);
                    ovl[o..o + 6].copy_from_slice(&tmp);
                }
                o += 9;
                x28 += 18;
                cnt -= 1;
                if cnt <= 0 {
                    break;
                }
            }
        }
        // 0x13f7ec: frequency inversion of the odd samples of every even output block
        if w22 >= 2 {
            let mut p = spec + 1;
            for _ in 0..(w22 >> 1) {
                for k in 0..9 {
                    work.w[p + 2 * k] = work.w[p + 2 * k].wrapping_neg();
                }
                p += 36;
            }
        }
        // 0x13f848: zero the 18 ints that follow the last output block
        let z = spec - 18 + (18 * w22) as usize;
        work.w[z..z + 18].fill(0);
    }
}

/// One alias-reduction boundary (0x13f680..0x13f6dc): 8 butterflies between `w[lo - i]` and `w[up + i]`.
/// (`up + 7` and `lo - 7` stay inside `W` for the at most 31 boundaries of a channel.)
fn alias_boundary(w: &mut [i32], up: usize, lo: usize) {
    for i in 0..8 {
        let cs = ANTIALIAS[2 * i] as i64; // ldrsh x16,[x11]
        let ca = ANTIALIAS[2 * i + 1] as i64; // ldrsh x0,[x11,#2]
        let mut x14 = w[up + i] as i64; // ldrsw x14,[x8]
        let w15 = w[lo - i]; // ldrsw x15,[x9]
        let x17 = (cs as i32 as i64).wrapping_mul(w15 as i64); // smull x17,w16,w15
        x14 = x14.wrapping_add(x17 >> 16); // add x14,x14,x17,asr #16
        let w17: u32 = if i == 0 { x14 as u32 } else { 0 }; // csel w17,w14,wzr,eq (w10 == 8)
        let x0 = (x14.wrapping_mul(ca) as u64) >> 16; // mul x0,x14,x0 ; lsr x0,x0,#16
        let w15n = w17.wrapping_add(w15 as u32).wrapping_add(x0 as u32); // add w15,w17,w15 ; add w15,w15,w0
        let x16 = ((cs as i32 as i64).wrapping_mul(w15n as i32 as i64) as u64) >> 16; // smull x16,w16,w15 ; lsr
        let w14 = (x14 as u32).wrapping_add(x16 as u32); // add w14,w14,w16
        w[up + i] = w14 as i32; // str w14,[x8],#4
        w[lo - i] = w15n as i32; // str w15,[x9],#-4
    }
}

/// Short-block reorder of one sfb block of `width` pairs-per-window (0x13f548..0x13f5d8): permutes the pairs
/// of 2 ints in place by following the cycle leaders listed in `_g_aReorderData` / `_g_aStartPositions`.
/// Total behaviour: `width` must be 1..=33 and the `6*width` ints at `x12` must lie in `buf`, otherwise nothing happens.
fn reorder_block(buf: &mut [i32], x12: usize, width: usize) {
    if !(1..=33).contains(&width) || x12.saturating_add(6 * width) > buf.len() {
        return;
    }
    let w14 = (2 * width) as i32;
    let mut mask = REORDER_DATA[width - 1] as u32; // w15
    let mut tpos = 0usize; // x16 - START_POSITIONS
    loop {
        let clz = (mask << 16).leading_zeros(); // w17
        let Some(&w0) = START_POSITIONS.get(tpos + clz as usize) else {
            return;
        };
        let w0 = w0 as usize;
        let (Some(&a0), Some(&a1)) = (buf.get(x12 + w0), buf.get(x12 + w0 + 1)) else {
            return;
        };
        let mut d0 = [a0, a1];
        let mut w1 = w0 as i32;
        // a cycle of the permutation has at most 3*width = 99 pairs
        for _ in 0..6 * width {
            let w2 = w1.wrapping_sub(w14);
            let nw = if w2 < 0 {
                w1.wrapping_mul(3)
            } else {
                let t = w2.wrapping_sub(w14);
                if t >= 0 {
                    t.wrapping_mul(3).wrapping_add(4)
                } else {
                    w2.wrapping_mul(3).wrapping_add(2)
                }
            };
            w1 = nw;
            let Some(idx) = usize::try_from(w1).ok().map(|v| x12 + v).filter(|i| i + 1 < buf.len()) else {
                return;
            };
            let d1 = [buf[idx], buf[idx + 1]];
            buf[idx] = d0[0];
            buf[idx + 1] = d0[1];
            d0 = d1;
            if w1 == w0 as i32 {
                break;
            }
        }
        mask = (mask << clz) << 1; // lsl w15,w15,w17 ; lsl w15,w15,#1
        tpos += clz as usize + 1;
        if (mask & 0xfffe) == 0 {
            break;
        }
    }
}

/// Is the pair of spectrum lines at `cur` non-zero (lines beyond `W` read as zero)?
#[inline]
fn pair_nonzero(work: &Work, cur: usize) -> bool {
    work.w.get(cur).is_some_and(|v| *v != 0) || work.w.get(cur + 1).is_some_and(|v| *v != 0)
}

/// 0x13f1e8..0x13f4b0 for `(hdr & 0xc0) == 0x40 && (hdr & 0x30) != 0` (joint stereo with MS and/or intensity).
fn joint_stereo(hdr: u32, meta: &mut Meta, work: &mut Work, rec: usize) {
    // 0x13f20c
    let n0 = meta.count(0) as u32;
    let n1 = meta.count(1) as u32;
    let m = n0.max(n1); // csel hi
    meta.set_count(1, m as u16);
    meta.set_count(0, m as u16);
    if m == 0 {
        return;
    }
    if hdr & 0x10 == 0 {
        // 0x13f230: MS only, all `m` lines
        ms_stereo(&mut work.w, SPEC_BASE, m as i32);
        return;
    }
    // ---- intensity stereo (0x13f23c)
    let n_long = mb(meta, rec + rec::N_LONG) as i32; // w8
    let n_short = mb(meta, rec + rec::N_SHORT) as i32; // w9
    let list = rec + mb(meta, rec + rec::SFB_OFF) as usize; // x21
    let mut a8 = [-1i32; 3]; // local_a8[0..3]
    let total = n_short + n_long; // w10 (adds)
    if total != 0 {
        // 0x13f26c: last nonzero band of channel 1 per window slot (slot cycles 2,1,0)
        let mut band = 0i32;
        let mut slot = 2i32;
        let mut x15 = SPEC_BASE + CH_STRIDE; // x14 = [sp+0x10] = channel 1 spectrum
        let mut lp = list;
        loop {
            let w = mb(meta, lp) as usize;
            lp += 1;
            let x14 = x15 + 2 * w;
            let mut cur = x15;
            loop {
                if pair_nonzero(work, cur) {
                    a8[slot as usize] = band;
                    break;
                }
                cur += 2;
                if cur >= x14 {
                    break;
                }
            }
            slot = if slot - 1 < 0 { 2 } else { slot - 1 };
            band += 1;
            x15 = x14;
            if band == total {
                break;
            }
        }
    }
    // 0x13f2c4
    if n_long != 0 {
        let mut mx = a8[0].max(a8[1]);
        mx = mx.max(a8[2]);
        a8 = [mx; 3];
    }
    // 0x13f2e8
    let mpeg1 = hdr & 0x80000 != 0;
    let default_pos: u8 = if mpeg1 { 3 } else { 0 }; // w10
    let nwin: i64 = if n_short != 0 { 3 } else { 1 }; // w11
    let mut x12: i64 = total as i64 - nwin; // sxtw w13
    let mut x8: i64 = (n_long + n_short) as i64 - 2 * nwin;
    let mut x9 = nwin;
    let mut x13 = nwin + 1;
    loop {
        x9 -= 1;
        let bound = a8[x9 as usize] as i64;
        let v = if x8 <= bound { default_pos } else { wbyte(work, (ISPOS_BASE as i64 + x12 - nwin) as usize) };
        set_wbyte(work, (ISPOS_BASE as i64 + x12) as usize, v);
        x12 += 1;
        x13 -= 1;
        x8 += 1;
        if x13 <= 1 {
            break;
        }
    }
    // 0x13f350
    let limit: u32 = if hdr & 0x80000 == 0 { 0x40 } else { 7 }; // w20
    let scale = (mb(meta, rec + 0x1a) & 1) as u32; // [sp+0x140]
    let ms = hdr & 0x20 != 0;
    // table choice (0x13f378..0x13f3b8): index of the table base in STEREO (u16 units) and the LSF base value
    let (tab, t0): (usize, u32) = if mpeg1 {
        (if ms { 6 } else { 8 }, 0)
    } else {
        let tb = if ms { 0 } else { 2 };
        (tb, STEREO[tb] as u32)
    };
    let mut ch0 = SPEC_BASE; // x26
    let mut p = list; // x21
    let mut width = mb(meta, p) as usize; // w8
    let mut slot = 2i32; // w19
    let mut band = 0i32; // w22
    loop {
        // 0x13f3c4
        let is_pos = wbyte(work, ISPOS_BASE + band as usize) as i8 as i32; // ldrsb w9,[x28],#1
        let bound = a8[slot as usize];
        let intensity = band > bound && limit > (is_pos as u32);
        if !intensity {
            // 0x13f3d8
            if hdr & 0x20 != 0 {
                ms_stereo(&mut work.w, ch0, (width * 2) as i32);
            }
        } else {
            let (m0, m1): (u32, u32);
            if !mpeg1 {
                // 0x13f3f4: LSF (is_pos < 0x40 here)
                let mut w10 = ((is_pos as u32).wrapping_add(1)) >> 1;
                w10 = w10.wrapping_shl(scale & 31);
                let v = (STEREO[tab + (w10 & 3) as usize] as u32) >> ((w10 >> 2) & 31);
                if is_pos & 1 == 0 {
                    m0 = t0;
                    m1 = v;
                } else {
                    m0 = v;
                    m1 = t0;
                }
            } else {
                // 0x13f434: MPEG-1 (is_pos < 7 here)
                let w10 = (is_pos as u32) << 2;
                m0 = STEREO[tab + w10 as usize] as u32;
                m1 = STEREO[tab + (w10 | 1) as usize] as u32;
            }
            // 0x13f448 (lines whose channel-1 partner lies beyond `W` are not processed)
            let avail = work.w.len().saturating_sub(ch0 + 0x240).min(2 * width);
            for i in 0..avail {
                let x = work.w[ch0 + i] as i64;
                work.w[ch0 + i + 0x240] = (((x.wrapping_mul(m1 as i64)) as u64) >> 15) as u32 as i32;
                work.w[ch0 + i] = (((x.wrapping_mul(m0 as i64)) as u64) >> 15) as u32 as i32;
            }
        }
        // 0x13f484
        ch0 += 2 * width;
        slot = if slot - 1 < 0 { 2 } else { slot - 1 };
        band += 1;
        p += 1;
        width = mb(meta, p) as usize;
        if width == 0 {
            break;
        }
    }
}

/// Regression against granule states recorded from Apple's own `DecodeFrame` (see Mp3XBack/NOTES.md, section 8).
///
/// `tests/fixtures/mp3-exact/back_golden.cases`: per case 11504 bytes: id u32, ring slot u32, ... (see `gran` tool), work buffer at +80
/// (0x12a0), overlap at +4848 (0x900), ring at +7152 (0x1100).  `back_golden.dump` (same directory): per case 14192 bytes: id, record index,
/// header, pcm length, ..., slot at +24, S[0..0xd8] before at +32 and after at +248, work buffer after, overlap after, ring after,
/// Apple's PCM.  40 granules: MPEG-1 stereo (all block types, MS/IS), MPEG-1 mono, LSF stereo with intensity, MPEG-2.5 8 kHz stereo
/// (4-subband mixed blocks) and LSF mono; states injected into Apple's decoder and read back (lldb), so they include overflowing spectra.
#[cfg(test)]
mod golden {
    use super::*;
    use super::super::granule::{GranuleIn, Meta, Work, META_LEN, META_REC, REC_LEN, WORK_INTS};

    /// The recorded states live with the other MP3 fixtures and are read at run time.
    fn golden(name: &str) -> Vec<u8> {
        std::fs::read(format!("{}/tests/fixtures/mp3-exact/{name}", env!("CARGO_MANIFEST_DIR"))).expect("granule golden data")
    }
    const CASE_BYTES: usize = 11504;
    const DUMP_BYTES: usize = 14192;
    const OFF_W: usize = 80;
    const W_BYTES: usize = 0x12a0;
    const OFF_OVL: usize = OFF_W + W_BYTES;
    const OVL_BYTES: usize = 0x900;
    const OFF_RING: usize = OFF_OVL + OVL_BYTES;
    const RING_BYTES: usize = 0x1100;
    const D_SPRE: usize = 32;
    const D_SPOST: usize = D_SPRE + META_LEN;
    const D_WPOST: usize = D_SPOST + META_LEN;
    const D_OVL: usize = D_WPOST + W_BYTES;
    const D_RING: usize = D_OVL + OVL_BYTES;
    const D_PCM: usize = D_RING + RING_BYTES;

    fn u32at(b: &[u8], o: usize) -> u32 {
        u32::from_le_bytes([b[o], b[o + 1], b[o + 2], b[o + 3]])
    }

    fn words(b: &[u8]) -> Vec<i32> {
        b.chunks_exact(4).map(|x| i32::from_le_bytes([x[0], x[1], x[2], x[3]])).collect()
    }

    #[test]
    fn matches_apple_recorded_granules() {
        let (cases, dump) = (golden("back_golden.cases"), golden("back_golden.dump"));
        let n = dump.len() / DUMP_BYTES;
        assert_eq!(n, cases.len() / CASE_BYTES);
        assert!(n >= 40);
        for i in 0..n {
            let c = &cases[i * CASE_BYTES..(i + 1) * CASE_BYTES];
            let d = &dump[i * DUMP_BYTES..(i + 1) * DUMP_BYTES];
            let hdr = u32at(d, 8);
            let rec0 = META_REC + u32at(d, 4) as usize * REC_LEN;
            let mut meta = Meta::default();
            meta.b.copy_from_slice(&d[D_SPRE..D_SPRE + META_LEN]);
            let mut work = Work::new();
            work.w.copy_from_slice(&words(&c[OFF_W..OFF_W + W_BYTES]));
            let mut be = BackEnd::new();
            let ov = words(&c[OFF_OVL..OFF_OVL + OVL_BYTES]);
            be.overlap_mut(0).copy_from_slice(&ov[..OVERLAP_WORDS]);
            be.overlap_mut(1).copy_from_slice(&ov[OVERLAP_WORDS..2 * OVERLAP_WORDS]);
            be.synth_mut().set_state(u32at(c, 4) as usize, &c[OFF_RING..OFF_RING + RING_BYTES]);
            let mut pcm = vec![0i16; 1152];
            let n_out = be.granule(&mut GranuleIn { hdr, meta: &mut meta, rec0, work: &mut work }, &mut pcm);
            let nch = if hdr & 0xc0 == 0xc0 { 1 } else { 2 };
            assert_eq!(u32at(d, 12) as usize, n_out * nch * 2, "case {i}: pcm length");
            for k in 0..n_out * nch {
                let ap = i16::from_le_bytes([d[D_PCM + 2 * k], d[D_PCM + 2 * k + 1]]);
                assert_eq!(ap, pcm[k], "case {i}: pcm[{k}]");
            }
            let wp = words(&d[D_WPOST..D_WPOST + W_BYTES]);
            assert_eq!(wp.len(), WORK_INTS);
            for k in 0..WORK_INTS {
                assert_eq!(wp[k], work.w[k], "case {i}: work[{k}]");
            }
            for k in (4..META_LEN).filter(|k| !(8..16).contains(k)) {
                assert_eq!(d[D_SPOST + k], meta.b[k], "case {i}: S[{k:#x}]");
            }
            let ovp = words(&d[D_OVL..D_OVL + OVL_BYTES]);
            for ch in 0..2 {
                for k in 0..OVERLAP_WORDS {
                    assert_eq!(ovp[ch * OVERLAP_WORDS + k], be.overlap_mut(ch)[k], "case {i}: overlap[{ch}][{k}]");
                }
            }
            assert!(d[D_RING..D_RING + RING_BYTES] == be.synth().ring_bytes()[..], "case {i}: ring bytes");
            assert_eq!(u32at(d, 24) as usize, be.synth().ring_slot(), "case {i}: ring slot");
        }
    }

    struct Rng(u64);
    impl Rng {
        fn next(&mut self) -> u64 {
            self.0 = self.0.wrapping_add(0x9e37_79b9_7f4a_7c15);
            let mut z = self.0;
            z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
            z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
            z ^ (z >> 31)
        }
        fn below(&mut self, n: u64) -> u64 {
            self.next() % n
        }
    }

    /// Garbage states (random header, records, tables, counts, spectra, is_pos bytes, record offsets, short `pcm`) must neither
    /// panic nor hang; the synthesis entry points must accept any slice sizes and slot counts.
    #[test]
    fn hostile_states_never_panic() {
        let iters: u64 = std::env::var("MP3X_HOSTILE_ITERS").ok().and_then(|v| v.parse().ok()).unwrap_or(4000);
        let mut rng = Rng(0x1234_5678);
        let mut be = BackEnd::new();
        for it in 0..iters {
            let mut meta = Meta::default();
            for b in meta.b.iter_mut() {
                *b = rng.next() as u8;
            }
            // make a good fraction of the cases structurally plausible: small widths, plausible record fields
            if rng.below(2) == 0 {
                for i in 0x70..META_LEN {
                    meta.b[i] = if rng.below(8) == 0 { 0 } else { 1 + rng.below(40) as u8 };
                }
                for k in 0..4 {
                    let r = META_REC + k * REC_LEN;
                    meta.b[r + 7] = rng.below(4) as u8;
                    meta.b[r + 8] = rng.below(2) as u8;
                    meta.b[r + 9] = (0x70 + rng.below(0x40) as usize).saturating_sub(r).min(255) as u8;
                    meta.b[r + 0xa] = [0u8, 6, 8, 22][rng.below(4) as usize];
                    meta.b[r + 0xb] = [0u8, 0x1e, 0x27][rng.below(3) as usize];
                }
            }
            meta.set_count(0, rng.below(65536) as u16);
            meta.set_count(1, if rng.below(3) == 0 { rng.below(65536) as u16 } else { rng.below(700) as u16 });
            meta.set_prev_count(0, rng.below(65536) as u16);
            meta.set_prev_count(1, rng.below(700) as u16);
            let mut work = Work::new();
            let mode = rng.below(4);
            for w in work.w.iter_mut() {
                *w = match mode {
                    0 => 0,
                    1 => (rng.next() >> 40) as i32,
                    _ => rng.next() as i32,
                };
            }
            let hdr = (rng.next() as u32 & !0xf0) | [0x00, 0x40, 0x80, 0xc0][rng.below(4) as usize] | (rng.below(4) as u32) << 4
                | if rng.below(2) == 0 { 0x80000 } else { 0 };
            let rec0 = if rng.below(4) == 0 { rng.next() as usize } else { META_REC + rng.below(4) as usize * REC_LEN };
            let mut pcm = vec![0i16; rng.below(1300) as usize];
            let n = be.granule(&mut GranuleIn { hdr, meta: &mut meta, rec0, work: &mut work }, &mut pcm);
            assert_eq!(n, 576, "iteration {it}");
            let mut pcm2 = vec![0i16; rng.below(2000) as usize];
            let nb = rng.below(40) as usize;
            let k = be.synth_only(&mut work, nb, rng.below(2) == 1, &mut pcm2);
            assert_eq!(k, nb.min(18) * 32);
        }
        // direct entry points with hostile arguments
        let mut small = vec![1i32; 100];
        let mut ovl = vec![0i32; 20];
        super::super::hybrid::imdct36(&mut small, 18, &mut ovl, true, i32::MAX);
        super::super::hybrid::imdct36(&mut small, usize::MAX, &mut ovl, false, 3);
        super::super::hybrid::imdct36(&mut small, 0, &mut ovl, false, 3);
        super::super::hybrid::imdct12(&small[..5], &mut ovl, &mut small[..2].to_vec());
        super::super::synth::copy_pcm_stereo(&small, &mut pcm_short(), 4000);
        super::super::synth::copy_pcm_mono(&small, &mut pcm_short(), usize::MAX);
        let mut s = Synth::new();
        s.subband_synth(&mut small, 18, true);
        s.subband_synth(&mut vec![0; 0x480], u32::MAX, true);
        s.set_state(usize::MAX, &[1, 2, 3]);
        assert_eq!(s.ring_slot() & 1, 0);
    }

    fn pcm_short() -> Vec<i16> {
        vec![7i16; 33]
    }

    /// Every width the reorder can be asked for permutes whole pairs inside its own `6*width` ints and terminates.
    #[test]
    fn reorder_block_is_a_permutation_for_every_width() {
        for width in 1..=33usize {
            let n = 6 * width;
            let orig: Vec<i32> = (0..n as i32 + 8).collect();
            let mut b = orig.clone();
            reorder_block(&mut b, 0, width);
            assert_eq!(&b[n..], &orig[n..], "width {width}: wrote beyond the block");
            let mut seen = b[..n].to_vec();
            seen.sort_unstable();
            assert_eq!(&seen[..], &orig[..n], "width {width}: not a permutation");
            for k in (0..n).step_by(2) {
                assert_eq!(b[k] + 1, b[k + 1], "width {width}: pair split");
            }
        }
        // refused (no-op) cases
        let orig: Vec<i32> = (0..300).collect();
        for (x12, width) in [(0usize, 0usize), (0, 34), (0, 255), (200, 20), (usize::MAX, 5)] {
            let mut b = orig.clone();
            reorder_block(&mut b, x12, width);
            assert_eq!(b, orig);
        }
    }
}
