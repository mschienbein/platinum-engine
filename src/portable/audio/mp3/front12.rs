//! Layer I / Layer II branch of `MP3DecoderWrapper_SpiritDSP::DecodeFrame` (0x13d768, decompile lines
//! L265-L614: everything under `if ((hdr & 0x60000) != 0x20000)`): bit allocation, scale-factor selection,
//! scale factors, sample read (linear / grouped), dequantization into the work buffer.  The polyphase
//! synthesis is the back half's `BackEnd::synth_only`.
//!
//! Apple keeps this scratch in `S+0x350..S+0x9d4`, which aliases the Layer III IMDCT overlap memory and
//! follows the reservoir buffer `B`.  It lives here in `bb.tail` (byte offsets relative to `S+0x350`), which
//! the decoder synchronizes with the back half's overlap memory (`decoder.rs`).

use super::front3::Front;
use super::granule::*;
use super::tables::{A_MPEG1BITRATE, GSTRTSUBBANDALLOC, GUCBITALLOCCODETAB, G_ADEQUANT, G_INV_3_5_9};

/// `S+0x350`: number of subbands in use (offsets below are relative to `S+0x350`).
const T_SBLIMIT: usize = 0;
/// `S+0x351`: joint-stereo bound (0 for mono).
const T_BOUND: usize = 1;
/// `S+0x352`: `sblimit - bound`.
const T_NBOUND: usize = 2;
/// `S+0x353`: allocation class per (subband, channel), 64 bytes.
const T_ALLOC: usize = 3;
/// `S+0x393`: scale-factor selection per (subband, channel) (6 = no allocation), 64 bytes.
const T_SCFSI: usize = 0x43;
/// `S+0x3d4`: (multiplier, shift) i32 pairs: `[(sb*2+ch)*6 + pair*2 + {0,1}]`, 384 words.
const T_SF: usize = 0x84;

impl Front {
    #[inline]
    fn sf_get(&self, i: usize) -> i32 {
        let o = T_SF + 4 * i;
        i32::from_le_bytes([self.bb.tail[o], self.bb.tail[o + 1], self.bb.tail[o + 2], self.bb.tail[o + 3]])
    }

    #[inline]
    fn sf_set(&mut self, i: usize, v: i32) {
        let o = T_SF + 4 * i;
        self.bb.tail[o..o + 4].copy_from_slice(&v.to_le_bytes());
    }

    /// Frame start (`counter < 0`): L267-L398.
    pub fn l12_start(&mut self, ring: &[u8], hdr: u32) {
        let l1 = (hdr & 0x60000) == 0x60000;
        self.bb.ensure_bits(ring, if l1 { 0x280 } else { 0x57c });
        let l2 = !l1;
        let mut sblimit: u32 = 0x20;
        let mut tab: usize = 27;
        if l2 {
            sblimit = 0x1e;
            tab = 18;
        }
        if l2 && (hdr & 0x80000) != 0 {
            let sr = hdr >> 10 & 3;
            let layer = hdr >> 17 & 3;
            let brb = A_MPEG1BITRATE[((layer * 15).wrapping_sub(15) as usize) + (hdr >> 12 & 0xf) as usize] as u32;
            let mut r: u32 = 0xc0;
            if brb != 0 {
                r = (brb << 3) >> ((hdr & 0xc0) != 0xc0) as u32;
            }
            sblimit = 0x1e;
            if r < 0x60 || sr == 1 {
                sblimit = 0x1b;
            }
            let lo: u32 = if sr != 2 { 8 } else { 0xc };
            if r < 0x38 {
                sblimit = lo;
            }
            tab = if r < 0x38 { 12 } else { 0 };
        }
        let mut j: u32 = 0;
        if (hdr & 0xc0) == 0x40 {
            j = ((hdr >> 2) & 0xc) + 4;
        }
        let mut bound: u32 = 0x20;
        if (hdr & 0x40) != 0 {
            bound = j;
        }
        if sblimit <= bound {
            bound = sblimit;
        }
        self.bb.tail[T_SBLIMIT] = sblimit as u8;
        self.bb.tail[T_BOUND] = bound as u8;
        let nbound = (sblimit as u8).wrapping_sub(bound as u8);
        self.bb.tail[T_NBOUND] = nbound;
        let sz = super::front3::frame_size_bytes(hdr, self.bb.f6c1);
        self.bb.end = self.bb.end.wrapping_add(sz);
        // bit allocation
        let mut idx: isize = 0;
        let mut tp = tab;
        let mut cnt: i32 = 0; // uVar25
        let mut bits: u32 = 0;
        let mut left = sblimit as i32; // uVar28
        let mut ai = 0usize;
        loop {
            let mut next = cnt.wrapping_sub(1); // uVar26
            if next == 0 || cnt < 1 {
                next = GSTRTSUBBANDALLOC.get(tp + 2).copied().unwrap_or(0) as i32;
                bits = GSTRTSUBBANDALLOC.get(tp + 1).copied().unwrap_or(0) as i8 as i32 as u32;
                idx += GSTRTSUBBANDALLOC.get(tp).copied().unwrap_or(0) as i8 as isize;
                tp += 3;
            }
            let code = self.bb.get_bits(bits) as isize;
            let mut a = GUCBITALLOCCODETAB.get((idx + code) as usize).copied().unwrap_or(0);
            self.bb.tail[T_ALLOC + ai] = a;
            if (nbound as i32) < left {
                let code2 = self.bb.get_bits(bits) as isize;
                a = GUCBITALLOCCODETAB.get((idx + code2) as usize).copied().unwrap_or(0);
            }
            self.bb.tail[T_ALLOC + ai + 1] = if bound != 0 { a } else { 0 };
            ai += 2;
            left -= 1;
            cnt = next;
            if left == 0 {
                break;
            }
        }
        // scale-factor selection info
        for i in 0..(2 * sblimit as usize) {
            let a = self.bb.tail[T_ALLOC + i];
            let v: u8 = if a == 0 {
                6
            } else if l1 {
                2
            } else {
                self.bb.get_bits(2) as u8
            };
            self.bb.tail[T_SCFSI + i] = v;
        }
        // scale factors
        let mut pi = 0usize;
        for i in 0..(2 * sblimit as usize) {
            let a = self.bb.tail[T_ALLOC + i] as u32;
            let sfi = self.bb.tail[T_SCFSI + i] as u32;
            let mut mult: u32 = 0;
            let mut shift: u32 = 0;
            let mask: u32 = if a != 0 { (0x13u32 >> (sfi & 0x1f)) | 4 } else { 0 };
            let mut m = 4u32;
            for _ in 0..3 {
                if (m & mask) != 0 {
                    let v = self.bb.get_bits(6) as i32;
                    let q = (v * 0x2b) >> 7;
                    shift = (q + 5) as u32;
                    let ix = v.wrapping_add((a as i32).wrapping_sub(q).wrapping_mul(3)).wrapping_sub(6);
                    mult = G_ADEQUANT.get(ix as usize).copied().unwrap_or(0);
                }
                self.sf_set(pi, mult as i32);
                self.sf_set(pi + 1, shift as i32);
                pi += 2;
                m >>= 1;
            }
        }
        for k in 0..nbound as usize {
            self.bb.tail[T_ALLOC + ((bound as usize) << 1 | 1) + 2 * k] = 0;
        }
        self.meta.set_u32(0, 0);
    }

    /// One pass of the group loop (L404-L580): read the samples of 4 rows (x3 for Layer II, 3 passes of x1 for Layer I),
    /// dequantize, zero-fill the unused subbands.  Returns Err(status) on the error exits.
    pub fn l12_group(&mut self, ring: &[u8], hdr: u32) -> Result<(), u32> {
        let l1 = (hdr & 0x60000) == 0x60000;
        let nsamp: usize = if l1 { 1 } else { 3 }; // uVar40
        let sblimit = self.bb.tail[T_SBLIMIT] as usize;
        let bound = self.bb.tail[T_BOUND] as usize;
        let nbound = self.bb.tail[T_NBOUND] as usize;
        let mut counter = self.meta.counter();
        let mut base: usize = if l1 { counter as usize } else { 0 };
        let mut old;
        loop {
            // remaining bits to the end of the frame
            let rem = (self.bb.end << 3)
                .wrapping_sub(self.bb.bitpos as u32)
                .wrapping_add(0u32.wrapping_sub(self.bb.ptr as u32).wrapping_mul(8))
                .wrapping_add(0x10);
            if (rem as i32) < 0 {
                return Err(4);
            }
            let mut r = rem;
            if 0x11ff < r {
                r = 0x1200;
            }
            let e = self.bb.ensure_bits(ring, r as i32);
            if e != 0 {
                return Err(e);
            }
            let mut cache = self.bb.cache;
            let mut bpos = self.bb.bitpos;
            let mut ptr = self.bb.ptr;
            let nsb2 = sblimit * 2;
            let mut rowbase = base;
            for _row in 0..4 {
                let mut p = rowbase;
                let mut stride: isize = 0x240;
                for e in 0..nsb2 {
                    let a = self.bb.tail[T_ALLOC + e] as u32;
                    if a != 0 {
                        if a < 0x11 {
                            let mut cur = cache;
                            for k in 0..nsamp {
                                let mut nc = cur.wrapping_shl(a & 0x1f);
                                bpos = bpos.wrapping_add(a as i32);
                                if bpos >= 0 {
                                    nc |= self.bb.rd16(ptr).wrapping_shl(bpos as u32);
                                    ptr += 2;
                                    bpos -= 16;
                                }
                                let v = (((-1i32) as u32).wrapping_shl(a.wrapping_sub(1) & 0x1f))
                                    .wrapping_add(1)
                                    .wrapping_add(cur.wrapping_shr(0x20u32.wrapping_sub(a) & 0x1f));
                                self.work.w[p + k] = v as i32;
                                cur = nc;
                            }
                            cache = cur;
                        } else {
                            let idx = a - 0x11;
                            let m: u32 = 2u32 << idx;
                            let iv13: u32 = m.wrapping_sub(((m as i32) >> 3) as u32);
                            let nb = iv13.wrapping_add(3);
                            let mut c2 = cache.wrapping_shl(nb & 0x1f);
                            bpos = bpos.wrapping_add(nb as i32);
                            if bpos >= 0 {
                                c2 |= self.bb.rd16(ptr).wrapping_shl(bpos as u32);
                                bpos -= 16;
                                ptr += 2;
                            }
                            let inv = G_INV_3_5_9[idx as usize] as u32;
                            let mut code = cache.wrapping_shr(0x1du32.wrapping_sub(iv13) & 0x1f);
                            for k in 0..nsamp {
                                let d = code.wrapping_add(((-1i32) as u32).wrapping_shl(idx & 0x1f));
                                code = code.wrapping_mul(inv) >> 13;
                                self.work.w[p + k] = d.wrapping_add(code.wrapping_mul(!m)) as i32;
                            }
                            cache = c2;
                        }
                    }
                    p = (p as isize + stride) as usize;
                    stride = 0x12 - stride;
                }
                rowbase += nsamp;
            }
            self.bb.cache = cache;
            self.bb.bitpos = bpos;
            self.bb.ptr = ptr;
            old = counter as i32;
            counter = counter.wrapping_add(4);
            self.meta.set_u32(0, counter);
            base = rowbase;
            if !(old < 8 && l1) {
                break;
            }
        }
        // dequantization (L582-L612)
        let cnt4 = (counter as i32) >> 2;
        let mut pi = ((cnt4 as isize) * 2 - 2) as usize;
        let mut p36: usize = 0;
        let mut p50: usize = 0x240;
        let mut np = bound;
        let mut second = false;
        loop {
            for _ in 0..np {
                let m0 = self.sf_get(pi);
                let s0 = self.sf_get(pi + 1) as u32;
                let m1 = self.sf_get(pi + 6);
                let s1 = self.sf_get(pi + 7) as u32;
                for k in 0..12 {
                    let x1 = self.work.w[p50 + k];
                    self.work.w[p36 + k + 0x240] = x1.wrapping_mul(m1) >> (s1 & 0x1f);
                    let x0 = self.work.w[p36 + k];
                    self.work.w[p36 + k] = x0.wrapping_mul(m0) >> (s0 & 0x1f);
                }
                pi += 12;
                p36 += 18;
                p50 += 18;
            }
            if second {
                break;
            }
            second = true;
            p50 = p36;
            np = nbound;
        }
        if sblimit < 0x20 {
            for _ in 0..(0x20 - sblimit) {
                for k in 0..12 {
                    self.work.w[p36 + k] = 0;
                    self.work.w[p36 + 0x240 + k] = 0;
                }
                p36 += 18;
            }
        }
        if self.meta.counter() == 0xc {
            self.meta.set_u32(0, 0xffff_ffff);
        }
        let rem = (self.bb.end << 3)
            .wrapping_sub(self.bb.bitpos as u32)
            .wrapping_add(0u32.wrapping_sub(self.bb.ptr as u32).wrapping_mul(8))
            .wrapping_add(0x10);
        if (rem as i32) >= 0 {
            Ok(())
        } else {
            Err(4)
        }
    }
}

#[allow(dead_code)]
const _: usize = META_LEN;
