//! `SpiritDecoder`: bit-exact port of `MP3DecoderWrapper_SpiritDSP::DecodeFrame` (0x13d768, decompile
//! `ghidra/out1/0013d768.c`; `Lnnn` = line numbers of that file) with the Spirit decoder state.
//!
//! Division of labour: this file = resync / header search / return codes / granule loop (front half);
//! `front3.rs` = Layer III side info, scale factors, Huffman, requantization; `back.rs` (Mp3XBack) =
//! everything after the stereo stage (`LAB_0013f1e8`), see `granule.rs`.

use super::back::BackEnd;
use super::bitbuf::BitBuf;
use super::codec::{
    DECODE_FAIL, DECODE_NEED_MORE, DECODE_OK, DECODE_OUT_TOO_SMALL, DECODE_RESET, DECODE_SILENT, FrameDecoder, FrameResult,
    RING_SIZE,
};
use super::front3::*;
use super::granule::*;

/// `_SpiritMP3GetLastFrameInfo` (0x11a20c): PCM bytes of one frame for header `hdr` (`info[5]`).
pub fn frame_out_bytes(hdr: u32) -> u32 {
    let layer = hdr >> 17 & 3;
    let stereo = (hdr & 0xc0) != 0xc0;
    let mut samples: u32 = if layer != 3 { 0x480 } else { 0x180 };
    if layer == 1 && (hdr & 0x80000) == 0 {
        samples = 0x240;
    }
    (samples << 1) << (stereo as u32)
}

/// Spirit decoder + wrapper-object internals except the 0x2000-byte input ring (owned by `codec.rs`).
pub struct SpiritDecoder {
    pub front: Front,
    pub back: BackEnd,
    /// Set when a Layer I/II frame was met (not ported yet): the layer bits of the header.
    pub unsupported_layer: Option<u32>,
    /// Debug/test hook called at the stereo-stage entry (`LAB_0013f1e8`) of every granule, before the back half.
    pub tap: Option<Box<dyn FnMut(&Front, u32, usize)>>,
    /// Debug/test hook called before `_mp3d_SubbandSynth` of a Layer I/II group (`LAB_0013f8b4`).
    pub tap_synth: Option<Box<dyn FnMut(&Front)>>,
}

impl Default for SpiritDecoder {
    fn default() -> Self {
        Self::new()
    }
}

#[inline]
fn map_status(st: u32, flag: bool) -> u32 {
    if st == 1 {
        DECODE_SILENT
    } else if st == 3 {
        DECODE_OUT_TOO_SMALL
    } else if st == 4 {
        DECODE_RESET
    } else if flag {
        DECODE_NEED_MORE
    } else if st.wrapping_sub(5) > 1 {
        DECODE_OK
    } else {
        DECODE_FAIL
    }
}

impl SpiritDecoder {
    pub fn new() -> Self {
        SpiritDecoder { front: Front::new(), back: BackEnd::new(), unsupported_layer: None, tap: None, tap_synth: None }
    }

    /// `MP3DecoderWrapper_SpiritDSP::Reset` Spirit-state half.
    pub fn reset_state(&mut self) {
        self.front.reset_state();
        self.back.reset();
    }

    /// `bb.tail` (`S+0x350 ..`) := the back half's IMDCT overlap memory (little-endian words, channel 0 then 1).
    fn tail_in(&mut self) {
        for ch in 0..2 {
            let o = self.back.overlap_mut(ch);
            let base = ch * OVERLAP_WORDS * 4;
            for (i, w) in o.iter().enumerate() {
                self.front.bb.tail[base + 4 * i..base + 4 * i + 4].copy_from_slice(&w.to_le_bytes());
            }
        }
    }

    /// Inverse of `tail_in` (Layer I/II scratch writes land in the overlap memory).
    fn tail_out(&mut self) {
        for ch in 0..2 {
            let base = ch * OVERLAP_WORDS * 4;
            let o = self.back.overlap_mut(ch);
            for (i, w) in o.iter_mut().enumerate() {
                let t = &self.front.bb.tail[base + 4 * i..base + 4 * i + 4];
                *w = i32::from_le_bytes([t[0], t[1], t[2], t[3]]);
            }
        }
    }
    /// `LAB_0013da8c` onward: status mapping, input shift amount.
    fn da8c(&mut self, st: u32, consumed: u32, out_bytes: u32) -> FrameResult {
        let flag = st == 2;
        let status = map_status(st, flag);
        if status == DECODE_RESET {
            self.reset_state();
        }
        if st < 3 {
            FrameResult { status, consumed: consumed as usize, out_bytes }
        } else {
            FrameResult { status, consumed: 0, out_bytes: 0 }
        }
    }

    /// `LAB_0013da54`: skip `junk` input bytes (resetting the bit buffer when junk != 0), need more data.
    fn skip_wait(&mut self, junk: u32) -> FrameResult {
        let bb = &mut self.front.bb;
        bb.in_pos += junk as usize;
        if junk != 0 {
            bb.clear_positions();
        }
        let consumed = bb.in_pos as u32;
        self.da8c(2, consumed, 0)
    }

    /// `LAB_0013f9a8`: end of frame / error exit.  `normal` is Apple's `bVar18`.
    fn finish(&mut self, ring: &[u8], mut st: u32, normal: bool) -> FrameResult {
        let bb = &mut self.front.bb;
        let rem = (bb.end << 3)
            .wrapping_sub(bb.bitpos as u32)
            .wrapping_add(0u32.wrapping_sub(bb.ptr as u32).wrapping_mul(8))
            .wrapping_add(0x10);
        if (rem as i32) < 1 {
            if (rem as i32) < 0 {
                st = 4;
                let consumed = bb.in_pos as u32;
                return self.da8c(st, consumed, 0);
            }
        } else {
            let mut r = rem;
            let mut tail = false;
            if r < 0x1009 {
                if 0xff8 < r {
                    tail = true;
                }
            } else {
                loop {
                    bb.get_bits(0x10);
                    r -= 0x10;
                    if r <= 0x1008 {
                        break;
                    }
                }
                tail = true;
            }
            if tail {
                bb.get_bits(r - 0xff8);
                r = 0xff8;
            }
            bb.ensure_bits(ring, r as i32);
        }
        let consumed = bb.in_pos as u32;
        if !normal {
            return self.da8c(st, consumed, 0);
        }
        let ob = frame_out_bytes(bb.hdr);
        self.da8c(0, consumed, ob)
    }

    /// The whole of `DecodeFrame` (param_5 == 0).
    pub fn decode(&mut self, ring: &[u8], avail: usize, out: &mut [i16], out_cap: usize) -> FrameResult {
        let size = avail as u32;
        self.front.bb.in_pos = 0;
        self.front.bb.in_size = size as i32;
        if (size as i32) < 4 {
            return self.da8c(2, 0, 0);
        }
        let size_i = size as i32;
        let mut w: u32 = ((ring[0] as u32) << 16) | ((ring[1] as u32) << 8) | ring[2] as u32; // uVar28
        let mut pos: usize = 3; // lVar41
        let (hdr, junk, fs): (u32, u32, i32);
        'resync: loop {
            let mut idx = pos; // lVar76
            let mut prev = w; // uVar78
            let mut cur: u32;
            loop {
                cur = (ring[idx] as u32) | (prev << 8);
                let nxt = idx + 1;
                if self.front.bb.accept_new_header(cur) {
                    pos = nxt;
                    break;
                }
                idx = nxt;
                prev = cur;
                if ((size & 0x7fff_ffff) as usize) <= nxt {
                    return self.skip_wait((nxt - 4) as u32);
                }
            }
            let jn = (idx - 3) as u32; // local_1308
            w = cur;
            if self.front.bb.f6c1 == 0x6c1 {
                if size_i < idx as i32 + 0x6c1 {
                    return self.skip_wait(jn);
                }
                self.front.bb.hdr = cur;
                self.front.bb.f6c1 = 0;
                let base = pos;
                let mut win = ((ring[base] as u32) << 16) | ((ring[base + 1] as u32) << 8) | ring[base + 2] as u32;
                let mut p = base + 3;
                let mut lv76: usize = 4;
                let mut lv71: u64 = 0;
                let mut u72: u64;
                loop {
                    u72 = lv71 + 4;
                    win = ring[p] as u32 | (win << 8);
                    let acc = self.front.bb.accept_new_header(win);
                    if acc || u72 > 0x6bf {
                        break;
                    }
                    p = lv76 + base;
                    lv76 += 1;
                    lv71 += 1;
                }
                if u72 < 0x6c0 {
                    let mut u26 = (lv71 as u32).wrapping_add(3);
                    if (!prev & 0x600) == 0 {
                        u26 = lv71 as u32;
                    }
                    if (prev & 2) == 0 {
                        u26 = u72 as u32;
                    }
                    self.front.bb.f6c1 = u26;
                } else {
                    self.front.bb.hdr = 0;
                    continue 'resync;
                }
            }
            let f = frame_size_bytes(cur, self.front.bb.f6c1) as i32; // iVar23
            let iv24 = pos as i32;
            if f.wrapping_add(iv24) <= size_i {
                let nb = (f as usize) + jn as usize;
                let nxt = ((ring[nb] as u32) << 16) | ((ring[nb + 1] as u32) << 8) | ring[nb + 2] as u32;
                if ((nxt ^ prev) & 0xfffe0c) == 0 && (((prev & 0xf0) == 0) == (ring[nb + 2] < 0x10)) {
                    junk = jn;
                } else {
                    self.front.bb.hdr = 0;
                    continue 'resync;
                }
            } else {
                if jn != 0 || size_i < f {
                    return self.skip_wait(jn);
                }
                junk = 0;
            }
            if cur == 0 {
                return self.skip_wait(junk);
            }
            hdr = cur;
            fs = f;
            break;
        }
        let ob = frame_out_bytes(hdr) as i32;
        if ob > out_cap as i32 {
            return FrameResult { status: DECODE_OUT_TOO_SMALL, consumed: 0, out_bytes: 0 };
        }
        // L222: previous frame's data must end exactly `junk` bytes beyond what is buffered
        if self.front.bb.end.wrapping_sub(self.front.bb.fill) != junk {
            let bb = &mut self.front.bb;
            bb.clear_positions();
            bb.in_pos += junk as usize;
            bb.in_size = bb.in_size.wrapping_sub(junk as i32);
            let consumed = bb.in_pos as u32;
            return self.da8c(4, consumed, 0);
        }
        {
            let bb = &mut self.front.bb;
            bb.in_size = fs.wrapping_add(junk as i32);
            let k = 0u32.wrapping_sub(bb.bitpos as u32) & 7;
            let np = (bb.bitpos as u32).wrapping_add(k);
            bb.cache = bb.cache.wrapping_shl(k);
            bb.bitpos = np as i32;
            let rem = (bb.end.wrapping_mul(8).wrapping_sub(np)).wrapping_add(0u32.wrapping_sub(bb.ptr as u32).wrapping_mul(8)) as i32; // iVar24
            let t = rem.wrapping_add(0x10);
            let u = if t >= 0 { t } else { rem.wrapping_add(0x17) };
            bb.reserve = (u >> 3) as u32;
            bb.ensure_bits(ring, rem.wrapping_add(0x40));
            bb.load_at_byte(bb.end);
            let h1 = bb.get_bits(16);
            let h2 = bb.get_bits(16);
            bb.hdr = h2 | (h1 << 16);
            if (bb.hdr >> 16 & 1) == 0 {
                bb.crc = bb.get_bits(16) as u16;
            }
        }
        self.front.meta.set_u32(0, 0xffff_fffe);
        let mut out_pos: usize = 0;
        loop {
            let hdr = self.front.bb.hdr;
            if (hdr & 0x60000) != 0x20000 {
                // Layer I / II (L265-L614): frame start, group read + dequantization, synthesis
                let stereo = (hdr & 0xc0) != 0xc0;
                self.tail_in();
                if (self.front.meta.counter() as i32) < 0 {
                    self.front.l12_start(ring, hdr);
                }
                let r = self.front.l12_group(ring, hdr);
                self.tail_out();
                if let Err(st) = r {
                    return self.finish(ring, st, false);
                }
                if let Some(t) = self.tap_synth.as_mut() {
                    t(&self.front);
                }
                let n = self.back.synth_only(&mut self.front.work, 12, stereo, &mut out[out_pos..]);
                out_pos += n * (1 + stereo as usize);
                if self.front.meta.counter() == 0xffff_ffff {
                    return self.finish(ring, 0, true);
                }
                continue;
            }
            self.tail_in();
            let mut counter = self.front.meta.counter() as i32;
            if counter < -1 {
                if let Err(st) = self.front.side_info(hdr) {
                    return self.finish(ring, st, false);
                }
                counter = 0;
                self.front.meta.set_u32(0, 0);
            }
            let stereo = (hdr & 0xc0) != 0xc0;
            let nch: usize = 1 + stereo as usize;
            let ch0 = (counter & 1) as usize;
            let grnch = (((counter >> 1) as i32) << (stereo as u32)) as usize;
            for ch in ch0..nch {
                let roff = META_REC + (grnch + ch) * REC_LEN;
                if let Err(st) = self.front.channel(ring, hdr, roff, ch) {
                    return self.finish(ring, st, false);
                }
                let c = self.front.meta.counter().wrapping_add((nch as u32) ^ 3);
                self.front.meta.set_u32(0, c);
            }
            // LAB_0013f1e8: stereo stage onward = back half
            let rec0 = META_REC + grnch * REC_LEN;
            {
                let hdr = self.front.bb.hdr;
                if let Some(t) = self.tap.as_mut() {
                    t(&self.front, hdr, rec0);
                }
                let Front { meta, work, .. } = &mut self.front;
                let mut g = GranuleIn { hdr, meta, rec0, work };
                let produced = self.back.granule(&mut g, &mut out[out_pos..]);
                out_pos += produced * nch;
            }
            let c = self.front.meta.counter();
            if (hdr >> 19 & 1) == 0 {
                if c == 2 {
                    self.front.meta.set_u32(0, 0xffff_ffff);
                }
            } else if c == 4 {
                self.front.meta.set_u32(0, 0xffff_ffff);
            }
            if self.front.meta.counter() == 0xffff_ffff {
                return self.finish(ring, 0, true);
            }
        }
    }
}

impl FrameDecoder for SpiritDecoder {
    fn reset(&mut self) {
        self.reset_state();
    }

    fn decode_frame(&mut self, ring: &[u8; RING_SIZE], avail: usize, out: &mut [i16], out_cap_bytes: usize) -> FrameResult {
        if avail == 0 {
            return FrameResult { status: DECODE_NEED_MORE, consumed: 0, out_bytes: 0 };
        }
        self.decode(ring, avail, out, out_cap_bytes)
    }
}

#[allow(dead_code)]
fn _assert_bitbuf(_: &BitBuf) {}
