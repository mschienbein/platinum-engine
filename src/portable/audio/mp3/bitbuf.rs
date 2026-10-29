//! Bit buffer + reservoir buffer of Apple's Spirit DSP MP3 decoder (front half).
//!
//! Mirrors the `BitBuf` struct at `S+0xd8` and the helpers `_mp3d_BitBufGetBits` (0x23ac4),
//! `_mp3d_BitBufEnsureBits` (0x10772c), `_BitBufAcceptNewHeader` (0x1076a0).  All field offsets below are
//! relative to the decoder state `S`; Apple's `BitBuf*` pointer is `S+0xd8`.
//!
//! The reservoir buffer `B` (`S+0x10e`, 0x242 bytes) is read through 16-bit big-endian words (the
//! refill reads `u16` at `ptr`).  A Huffman decode that overruns the 578-byte buffer reads the bytes that
//! follow `B` in Apple's state object (`S+0x350..`: IMDCT overlap memory / Layer I/II scratch); they are
//! modeled by `tail`, which the decoder keeps equal to that memory.

/// Capacity of the reservoir buffer `B` (`0x242`, EnsureBits `mov w11,#0x242`).
pub const BUF_CAP: usize = 0x242;
/// Modeled bytes of the memory following `B` (`S+0x350 .. S+0xc50`: both IMDCT overlap buffers).
pub const TAIL_LEN: usize = 0x900;

#[derive(Clone)]
pub struct BitBuf {
    /// `S+0xd8`: 32-bit cache (bit 31 = next bit).
    pub cache: u32,
    /// `S+0xdc`: signed bit position; `-16 + k` where `k` = bits of the current 16-bit word already shifted out.
    /// A refill of the next word happens when it becomes `>= 0` after adding a bit count.
    pub bitpos: i32,
    /// `S+0xe0`: byte offset (from `B`) of the next 16-bit word to refill.
    pub ptr: usize,
    /// `S+0xe8`: header word of the current frame / last accepted header (0 = none).
    pub hdr: u32,
    /// `S+0xec`: free-format size state (`0x6c1` sentinel set by `accept_new_header`), later the measured frame size.
    pub f6c1: u32,
    /// `S+0xf0`: number of valid bytes in `B`.
    pub fill: u32,
    /// `S+0xf4`: byte offset in `B` of the end of the current frame's data ("end").
    pub end: u32,
    /// `S+0xf8`: absolute bit position in `B` of the end of the current granule/channel data (set by `ensure_bits`).
    pub seg_end: u32,
    /// `S+0xfc`: unread reservoir bytes available before the current frame (`main_data_begin` upper bound).
    pub reserve: u32,
    /// `S+0x100`: read index of the next input byte in the caller's ring (Apple: absolute pointer).
    pub in_pos: usize,
    /// `S+0x108`: remaining input bytes allowed to be copied (i32 like Apple).
    pub in_size: i32,
    /// `S+0x10c`: CRC word of the current frame (when the protection bit is 0).
    pub crc: u16,
    /// `B`: `S+0x10e ..`.
    pub buf: [u8; BUF_CAP],
    /// `S+0x350 ..`: the bytes that follow `B` in the state object (Layer I/II scratch / IMDCT overlap memory).
    /// Only read when the bit position runs past the end of `B`; kept in sync by the decoder.
    pub tail: [u8; TAIL_LEN],
}

impl Default for BitBuf {
    fn default() -> Self {
        Self::new()
    }
}

impl BitBuf {
    /// State after `Reset`/`Initialize`: cache 0, bitpos 0x10 (note: `+0x10`, not `-16`; Apple stores
    /// `puVar1[0x37] = 0x10` and the first use always goes through `ensure_bits`), ptr = B.
    pub fn new() -> Self {
        BitBuf {
            cache: 0,
            bitpos: 0x10,
            ptr: 0,
            hdr: 0,
            f6c1: 0,
            fill: 0,
            end: 0,
            seg_end: 0,
            reserve: 0,
            in_pos: 0,
            in_size: 0,
            crc: 0,
            buf: [0; BUF_CAP],
            tail: [0; TAIL_LEN],
        }
    }

    /// `*(u64*)(S+0xd8) = DAT_00921288` (cache = 0, bitpos = 0x10), `ptr = B`, `fill = end = seg_end = reserve = 0`
    /// (resync paths: L205-L210, L1380-L1385).
    pub fn clear_positions(&mut self) {
        self.cache = 0;
        self.bitpos = 0x10;
        self.ptr = 0;
        self.fill = 0;
        self.end = 0;
        self.seg_end = 0;
        self.reserve = 0;
    }

    /// 16-bit big-endian word at byte offset `i` of `B`; offsets `>= BUF_CAP` read the `tail` (Apple: the bytes
    /// that follow `B` in the state object, `S+0x350..`).
    #[inline]
    pub fn rd16(&self, i: usize) -> u32 {
        if i + 1 < BUF_CAP {
            ((self.buf[i] as u32) << 8) | self.buf[i + 1] as u32
        } else if i >= BUF_CAP {
            let t = i - BUF_CAP;
            if t + 1 < TAIL_LEN { ((self.tail[t] as u32) << 8) | self.tail[t + 1] as u32 } else { 0 }
        } else {
            ((self.buf[i] as u32) << 8) | self.tail[0] as u32
        }
    }

    /// 32-bit big-endian word at byte offset `i` of `B` (`ldr w,[..]; rev w`).
    #[inline]
    pub fn rd32(&self, i: usize) -> u32 {
        (self.rd16(i) << 16) | self.rd16(i + 2)
    }

    /// Reload cache/ptr/bitpos from absolute bit position `pos` (>= 0):
    /// `ptr = B + (pos>>4)*2 + 4; cache = BE32(B + (pos>>4)*2) << (pos & 15); bitpos = (pos & 15) - 16`.
    #[inline]
    pub fn load_at(&mut self, pos: u32) {
        let o = ((pos >> 4) as usize) * 2;
        self.cache = self.rd32(o).wrapping_shl(pos & 0xf);
        self.ptr = o + 4;
        self.bitpos = (pos | 0xffff_fff0) as i32;
    }

    /// Variant used with a byte offset `byte` (`(byte>>1 & 0xfffffff)*2`, shift `(byte*8) & 8`):
    /// reload at byte `byte` (L238-L243, L1316-1322, L1334..).  `bitpos = (byte*8) | 0xfffffff0`.
    #[inline]
    pub fn load_at_byte(&mut self, byte: u32) {
        let o = ((byte >> 1) & 0x0fff_ffff) as usize * 2;
        let sh = (byte << 3) & 8;
        self.cache = self.rd32(o).wrapping_shl(sh);
        self.ptr = o + 4;
        self.bitpos = ((byte << 3) | 0xffff_fff0) as i32;
    }

    /// Absolute bit position of the next unread bit: `bitpos + ptr*8 - 0x10` (EnsureBits `uVar6`).
    #[inline]
    pub fn abs_pos(&self) -> u32 {
        (self.bitpos as u32)
            .wrapping_add((self.ptr as u32).wrapping_mul(8))
            .wrapping_sub(0x10)
    }

    /// `_mp3d_BitBufGetBits(bb, n)`: returns the top `n` bits of the cache (n = 0 returns the whole cache, as Apple).
    #[inline]
    pub fn get_bits(&mut self, n: u32) -> u32 {
        let c = self.cache;
        let mut pos = self.bitpos.wrapping_add(n as i32);
        let mut nc = c.wrapping_shl(n);
        if pos >= 0 {
            let w = self.rd16(self.ptr);
            self.ptr += 2;
            nc |= w.wrapping_shl(pos as u32);
            pos = pos.wrapping_sub(16);
        }
        self.cache = nc;
        self.bitpos = pos;
        c.wrapping_shr(0x20u32.wrapping_sub(n))
    }

    /// `_BitBufAcceptNewHeader(bb, h)` (0x1076a0).
    pub fn accept_new_header(&mut self, h: u32) -> bool {
        let not_ver = !(h >= 0xfff0_0000 || (h & 0xfffe_0000) == 0xffe2_0000);
        if (!h & 0xc00) == 0 || (h & 0x60000) == 0 {
            return false;
        }
        let br = h & 0xf000;
        if br == 0xf000 || not_ver {
            return false;
        }
        let old = self.hdr;
        if old == 0 {
            if br == 0 {
                self.f6c1 = 0x6c1;
            }
            return true;
        }
        let diff = old ^ h;
        if diff & 0xfffe_0c00 != 0 {
            return false;
        }
        (br == 0) != ((old & 0xf000) != 0)
    }

    /// `_mp3d_BitBufEnsureBits(bb, n)` (0x10772c): make `n` bits (starting at the current position) available
    /// in `B`, compacting `B` and copying input from `ring`.  Returns 0 (ok), 2 (input exhausted), 4 (request too large).
    pub fn ensure_bits(&mut self, ring: &[u8], n: i32) -> u32 {
        let mut pos = self.abs_pos(); // uVar6
        let lim = ((self.end << 3) as u64).wrapping_add(0x1210);
        if (pos.wrapping_add(n as u32) as u64) >= lim {
            return 4;
        }
        let mut fill = self.fill; // uVar8
        let mut avail = (fill << 3).wrapping_sub(pos) as i32; // iVar4
        loop {
            if n <= avail {
                self.load_at(pos);
                self.seg_end = pos.wrapping_add(n as u32);
                return 0;
            }
            let end = self.end;
            let mut dropbits: u32; // iVar10
            let mut m = pos; // uVar2
            if (end << 3) <= pos {
                m = end << 3;
            }
            m >>= 3; // bytes that may be dropped (uVar2)
            let mut d = m; // uVar11
            if fill <= m {
                d = fill;
            }
            if d == 0 {
                dropbits = 0;
            } else {
                let mut keep = d; // uVar7
                if m < fill {
                    keep = fill;
                    if (fill.wrapping_sub(d) as i32) > 0 {
                        let cnt = (fill - d) as usize;
                        let src = d as usize;
                        for k in 0..cnt {
                            let v = if src + k < self.buf.len() { self.buf[src + k] } else { 0 };
                            self.buf[k] = v;
                        }
                    }
                }
                fill = keep.wrapping_sub(d);
                self.fill = fill;
                self.end = self.end.wrapping_sub(d);
                dropbits = d << 3;
                self.seg_end = self.seg_end.wrapping_sub(d << 3);
            }
            let cap = (BUF_CAP as u32).wrapping_sub(fill) as i32; // 0x242 - fill
            let mut c = cap; // w13
            if self.in_size < c {
                c = self.in_size;
            }
            // `csel w13,w13,w15,lt`: min (signed) of cap and in_size
            if c >= 1 {
                let dst = fill as usize;
                for k in 0..c as usize {
                    let b = ring.get(self.in_pos + k).copied().unwrap_or(0);
                    if dst + k < self.buf.len() {
                        self.buf[dst + k] = b;
                    }
                }
                self.in_pos += c as usize;
                fill = self.fill;
            }
            // note: when no copy happened Apple keeps the register value of fill (== self.fill)
            pos = pos.wrapping_sub(dropbits);
            self.in_size = self.in_size.wrapping_sub(c);
            fill = fill.wrapping_add(c as u32);
            self.fill = fill;
            let cbits = (c as u32) << 3;
            avail = avail.wrapping_add(cbits as i32);
            let _ = &mut dropbits;
            if cbits == 0 {
                break;
            }
        }
        self.load_at(pos);
        2
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Bits come out big-endian across refills and `get_bits(0)` returns the whole cache (Apple quirk).
    #[test]
    fn ensure_then_get_bits_is_big_endian() {
        let ring: Vec<u8> = (0..64u32).map(|i| (i * 37 + 11) as u8).collect();
        let mut bb = BitBuf::new();
        bb.in_size = ring.len() as i32;
        bb.end = ring.len() as u32;
        assert_eq!(bb.ensure_bits(&ring, 64 * 8), 0);
        let mut acc: Vec<u8> = Vec::new();
        for _ in 0..16 {
            let v = bb.get_bits(13);
            // re-pack 13-bit groups into bytes via a bit string
            for b in (0..13).rev() {
                acc.push(((v >> b) & 1) as u8);
            }
        }
        let want: Vec<u8> = ring.iter().flat_map(|b| (0..8).rev().map(move |k| (b >> k) & 1)).take(acc.len()).collect();
        assert_eq!(acc, want);
        let mut z = BitBuf::new();
        z.cache = 0xdead_beef;
        z.bitpos = -16;
        assert_eq!(z.get_bits(0), 0xdead_beef);
    }

    /// `_BitBufAcceptNewHeader`: plain checks, free-format sentinel, consistency with the stored header.
    #[test]
    fn accept_header_rules() {
        let mut bb = BitBuf::new();
        assert!(!bb.accept_new_header(0xffe0_0000)); // layer 0 and not v1/2
        assert!(!bb.accept_new_header(0xfffb_f000)); // bitrate index 15
        assert!(!bb.accept_new_header(0xfffb_9c00)); // sample rate index 3
        assert!(bb.accept_new_header(0xfffb_9000));
        assert_eq!(bb.f6c1, 0);
        assert!(bb.accept_new_header(0xfffb_0000)); // free format, no header stored yet
        assert_eq!(bb.f6c1, 0x6c1);
        bb.hdr = 0xfffb_9000;
        assert!(bb.accept_new_header(0xfffb_b000)); // other bitrate, same stream parameters
        assert!(!bb.accept_new_header(0xfffb_0000)); // free-format-ness differs
        assert!(!bb.accept_new_header(0xfffb_9400)); // other sample rate
        assert!(!bb.accept_new_header(0xffe2_9000)); // MPEG 2.5 layer III is a valid header but differs from the stored one
        bb.hdr = 0;
        assert!(bb.accept_new_header(0xffe2_9000));
    }
}
